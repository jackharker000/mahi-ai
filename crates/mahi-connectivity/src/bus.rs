//! The multiplexed bus abstraction and its in-process implementation.
//!
//! Everything that crosses a device boundary travels as a
//! [`MultiplexedEnvelope`] on a logical [`ChannelId`]. A bus lets you `send`
//! envelopes and `subscribe` to a channel as a stream. The `Control` channel
//! is scheduled with strict priority so heartbeat / kill-switch traffic is
//! never starved by saturated data channels.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use mahi_contracts::connectivity::{ChannelId, MultiplexedEnvelope};
use mahi_contracts::error::ConnectivityError;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::BroadcastStream;

/// A boxed stream of envelopes for one logical channel.
pub type EnvelopeStream = Pin<Box<dyn Stream<Item = MultiplexedEnvelope> + Send + 'static>>;

/// Every logical channel on the bus, in priority/declaration order.
pub(crate) const ALL_CHANNELS: [ChannelId; 8] = [
    ChannelId::Control,
    ChannelId::Inference,
    ChannelId::Sync,
    ChannelId::Approval,
    ChannelId::StreamVideo,
    ChannelId::Input,
    ChannelId::FileXfr,
    ChannelId::Audio,
];

/// Default per-channel fan-out buffer (envelopes retained for slow subscribers).
pub(crate) const DEFAULT_CHANNEL_CAPACITY: usize = 256;

/// Default depth of the send queues feeding the dispatcher / writer.
pub(crate) const DEFAULT_QUEUE_DEPTH: usize = 256;

/// The bus seam shared by every transport (in-process, TCP, later QUIC).
#[async_trait]
pub trait MultiplexedBus: Send + Sync {
    /// Enqueue an envelope for delivery. `Control` envelopes are scheduled
    /// ahead of all other channels.
    async fn send(&self, envelope: MultiplexedEnvelope) -> Result<(), ConnectivityError>;

    /// Subscribe to one logical channel. Only envelopes whose
    /// `envelope.channel` matches are yielded. Subscriptions are independent;
    /// each subscriber sees every envelope published after it subscribed.
    fn subscribe(&self, channel: ChannelId) -> EnvelopeStream;
}

/// Per-channel fan-out: routes an envelope to subscribers of its channel only.
///
/// Shared by [`LocalBus`] (dispatcher side) and the TCP transport (reader side).
pub(crate) struct ChannelRouter {
    senders: HashMap<ChannelId, broadcast::Sender<MultiplexedEnvelope>>,
}

impl ChannelRouter {
    pub(crate) fn new(capacity: usize) -> Self {
        let senders = ALL_CHANNELS
            .iter()
            .map(|&channel| (channel, broadcast::channel(capacity).0))
            .collect();
        Self { senders }
    }

    /// Fan the envelope out to subscribers of its channel. Envelopes on a
    /// channel with no subscribers are dropped (fire-and-forget semantics).
    pub(crate) fn publish(&self, envelope: MultiplexedEnvelope) {
        if let Some(tx) = self.senders.get(&envelope.channel) {
            // SendError just means "no active subscribers" — not a bus fault.
            let _ = tx.send(envelope);
        }
    }

    pub(crate) fn subscribe(&self, channel: ChannelId) -> EnvelopeStream {
        let rx = self
            .senders
            .get(&channel)
            .expect("ALL_CHANNELS covers every ChannelId variant")
            .subscribe();
        // Lagged subscribers skip dropped envelopes rather than erroring the
        // stream; Phase 0 has no resume/replay (that is seq_no-based, v1).
        Box::pin(BroadcastStream::new(rx).filter_map(|res| futures::future::ready(res.ok())))
    }
}

/// The pair of priority queues feeding a dispatcher/writer task.
pub(crate) struct SendQueues {
    control_tx: mpsc::Sender<MultiplexedEnvelope>,
    normal_tx: mpsc::Sender<MultiplexedEnvelope>,
}

impl SendQueues {
    pub(crate) fn new(
        control_tx: mpsc::Sender<MultiplexedEnvelope>,
        normal_tx: mpsc::Sender<MultiplexedEnvelope>,
    ) -> Self {
        Self {
            control_tx,
            normal_tx,
        }
    }

    fn for_channel(&self, channel: ChannelId) -> &mpsc::Sender<MultiplexedEnvelope> {
        if channel == ChannelId::Control {
            &self.control_tx
        } else {
            &self.normal_tx
        }
    }
}

/// Picks the sender for an envelope or fails if the bus has been closed.
pub(crate) fn pick_sender(
    queues: &Mutex<Option<SendQueues>>,
    channel: ChannelId,
) -> Result<mpsc::Sender<MultiplexedEnvelope>, ConnectivityError> {
    let guard = queues.lock().expect("send-queue lock poisoned");
    match guard.as_ref() {
        Some(q) => Ok(q.for_channel(channel).clone()),
        None => Err(ConnectivityError::ChannelClosed { channel }),
    }
}

/// In-process implementation of [`MultiplexedBus`].
///
/// Envelopes are queued into one of two `mpsc` queues (`Control` vs everything
/// else); a dispatcher task drains them with a biased `select!` so control
/// traffic always wins, then fans out to per-channel `broadcast` subscribers.
pub struct LocalBus {
    router: Arc<ChannelRouter>,
    /// `None` once the bus is closed; dropping the senders stops the dispatcher.
    queues: Mutex<Option<SendQueues>>,
}

impl LocalBus {
    /// Create a bus with default buffer sizes and start its dispatcher task.
    /// Must be called from within a tokio runtime.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CHANNEL_CAPACITY, DEFAULT_QUEUE_DEPTH)
    }

    /// Create a bus with explicit per-channel fan-out capacity and send-queue depth.
    pub fn with_capacity(channel_capacity: usize, queue_depth: usize) -> Self {
        let router = Arc::new(ChannelRouter::new(channel_capacity));
        let (control_tx, control_rx) = mpsc::channel(queue_depth);
        let (normal_tx, normal_rx) = mpsc::channel(queue_depth);

        tokio::spawn(dispatch(Arc::clone(&router), control_rx, normal_rx));

        Self {
            router,
            queues: Mutex::new(Some(SendQueues {
                control_tx,
                normal_tx,
            })),
        }
    }

    /// Close the bus: subsequent `send`s fail with
    /// [`ConnectivityError::ChannelClosed`] and the dispatcher task drains and
    /// exits. Existing subscriber streams end once in-flight envelopes flush.
    pub fn close(&self) {
        self.queues.lock().expect("send-queue lock poisoned").take();
    }

    /// Whether `close()` has been called.
    pub fn is_closed(&self) -> bool {
        self.queues
            .lock()
            .expect("send-queue lock poisoned")
            .is_none()
    }
}

impl Default for LocalBus {
    fn default() -> Self {
        Self::new()
    }
}

/// Drains the priority queues and fans envelopes out to channel subscribers.
/// `biased` ordering means a ready `Control` envelope is always delivered
/// before any pending envelope from the normal queue.
async fn dispatch(
    router: Arc<ChannelRouter>,
    mut control_rx: mpsc::Receiver<MultiplexedEnvelope>,
    mut normal_rx: mpsc::Receiver<MultiplexedEnvelope>,
) {
    let mut control_open = true;
    let mut normal_open = true;
    loop {
        tokio::select! {
            biased;
            maybe = control_rx.recv(), if control_open => match maybe {
                Some(envelope) => router.publish(envelope),
                None => control_open = false,
            },
            maybe = normal_rx.recv(), if normal_open => match maybe {
                Some(envelope) => router.publish(envelope),
                None => normal_open = false,
            },
            else => break,
        }
    }
    tracing::debug!("LocalBus dispatcher stopped");
}

#[async_trait]
impl MultiplexedBus for LocalBus {
    async fn send(&self, envelope: MultiplexedEnvelope) -> Result<(), ConnectivityError> {
        let tx = pick_sender(&self.queues, envelope.channel)?;
        tx.send(envelope)
            .await
            .map_err(|e| ConnectivityError::ChannelClosed { channel: e.0.channel })
    }

    fn subscribe(&self, channel: ChannelId) -> EnvelopeStream {
        self.router.subscribe(channel)
    }
}
