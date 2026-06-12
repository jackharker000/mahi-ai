//! Shared SSE → [`InferenceChunk`] plumbing for hosted providers.
//!
//! Providers implement [`SseChunkParser`] (a pure, unit-testable state
//! machine over `(event_type, data)` pairs); [`chunks_from_events`] lifts a
//! parser over an `eventsource-stream` event stream into the contract's
//! [`InferenceStream`], wiring in cancellation.

use eventsource_stream::{Event, EventStreamError};
use futures::{future, stream, Stream, StreamExt};
use mahi_contracts::compute::{FinishReason, InferenceChunk, InferenceStream};
use mahi_contracts::error::{ContractError, InferenceError};
use mahi_contracts::types::ComputeMode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// A pure, stateful parser turning SSE events into inference chunks.
pub(crate) trait SseChunkParser: Send {
    /// The compute mode stamped on produced chunks.
    fn mode(&self) -> ComputeMode;
    /// Whether the logical stream has finished (`[DONE]` / `message_stop`).
    fn done(&self) -> bool;
    /// Consume one SSE event; return zero or more chunks.
    fn handle_event(
        &mut self,
        event_type: &str,
        data: &str,
    ) -> Vec<Result<InferenceChunk, ContractError>>;
}

/// Lift `parser` over a stream of SSE events into an [`InferenceStream`].
///
/// - Transport/parse errors surface as `InferenceError::Provider` items.
/// - The stream ends once the parser reports done.
/// - Cancellation promptly ends the stream; if the parser had not finished,
///   a terminal `FinishReason::Cancelled` chunk is appended.
pub(crate) fn chunks_from_events<S, E, P>(
    events: S,
    parser: P,
    cancel: CancellationToken,
) -> InferenceStream
where
    S: Stream<Item = Result<Event, EventStreamError<E>>> + Send + 'static,
    E: std::fmt::Display + Send + 'static,
    P: SseChunkParser + 'static,
{
    let mode = parser.mode();
    let finished = Arc::new(AtomicBool::new(false));
    let finished_in_scan = Arc::clone(&finished);
    let cancel_for_tail = cancel.clone();

    let body = events
        .take_until(cancel.cancelled_owned())
        .scan(parser, move |parser, item| {
            if parser.done() {
                return future::ready(None);
            }
            let out = match item {
                Ok(event) => parser.handle_event(&event.event, &event.data),
                Err(err) => vec![Err(ContractError::Inference(InferenceError::Provider {
                    message: format!("SSE stream error: {err}"),
                    retryable: true,
                }))],
            };
            if parser.done() {
                finished_in_scan.store(true, Ordering::SeqCst);
            }
            future::ready(Some(stream::iter(out)))
        })
        .flatten();

    // Terminal Cancelled chunk, emitted only when the stream was actually cut
    // short by cancellation (not after a natural [DONE]/message_stop).
    let tail = stream::once(future::ready(Ok(InferenceChunk::finish(
        FinishReason::Cancelled,
        mode,
    ))))
    .filter(move |_| {
        future::ready(cancel_for_tail.is_cancelled() && !finished.load(Ordering::SeqCst))
    });

    Box::pin(body.chain(tail))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;

    struct EchoParser {
        done: bool,
    }

    impl SseChunkParser for EchoParser {
        fn mode(&self) -> ComputeMode {
            ComputeMode::Hosted
        }
        fn done(&self) -> bool {
            self.done
        }
        fn handle_event(
            &mut self,
            _event_type: &str,
            data: &str,
        ) -> Vec<Result<InferenceChunk, ContractError>> {
            if data == "[DONE]" {
                self.done = true;
                return vec![Ok(InferenceChunk::finish(FinishReason::Stop, self.mode()))];
            }
            vec![Ok(InferenceChunk::text(data.to_string(), self.mode()))]
        }
    }

    fn sse_stream(
        raw: &'static str,
    ) -> impl Stream<Item = Result<Event, EventStreamError<Infallible>>> {
        use eventsource_stream::Eventsource;
        stream::iter(vec![Ok::<_, Infallible>(raw.as_bytes())]).eventsource()
    }

    #[tokio::test]
    async fn parses_events_until_done() {
        let raw = "data: hello\n\ndata: world\n\ndata: [DONE]\n\ndata: ignored-after-done\n\n";
        let chunks: Vec<_> = chunks_from_events(
            sse_stream(raw),
            EchoParser { done: false },
            CancellationToken::new(),
        )
        .collect()
        .await;

        let chunks: Vec<_> = chunks.into_iter().map(Result::unwrap).collect();
        assert_eq!(chunks.len(), 3, "events after [DONE] must be dropped");
        assert_eq!(chunks[0].delta.as_deref(), Some("hello"));
        assert_eq!(chunks[1].delta.as_deref(), Some("world"));
        assert_eq!(chunks[2].finish_reason, Some(FinishReason::Stop));
    }

    #[tokio::test]
    async fn cancellation_appends_cancelled_finish() {
        let cancel = CancellationToken::new();
        cancel.cancel(); // pre-cancelled: stream ends immediately
        let raw = "data: hello\n\n";
        let chunks: Vec<_> =
            chunks_from_events(sse_stream(raw), EchoParser { done: false }, cancel)
                .collect()
                .await;
        let last = chunks.last().unwrap().as_ref().unwrap();
        assert_eq!(last.finish_reason, Some(FinishReason::Cancelled));
    }

    #[tokio::test]
    async fn natural_completion_has_no_trailing_cancelled_chunk() {
        let raw = "data: hello\n\ndata: [DONE]\n\n";
        let cancel = CancellationToken::new();
        let chunks: Vec<_> =
            chunks_from_events(sse_stream(raw), EchoParser { done: false }, cancel.clone())
                .collect()
                .await;
        let finishes: Vec<_> = chunks
            .iter()
            .filter_map(|c| c.as_ref().ok().and_then(|c| c.finish_reason))
            .collect();
        assert_eq!(finishes, vec![FinishReason::Stop]);
    }
}
