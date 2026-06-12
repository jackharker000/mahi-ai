//! Shared subprocess plumbing for the script-running built-ins
//! (`apple_script`, `run_code`).
//!
//! Mirrors `shell_exec`'s machinery — a `tokio::process` child, stdout/stderr
//! lines streamed as [`ToolEvent::Chunk`]s, a hard timeout, kill-on-cancel —
//! and additionally captures capped stdout/stderr transcripts so the final
//! [`ToolEvent::Result`] carries `{ stdout, stderr, exit_code, success }`.

use crate::tool::{channel_stream, tool_error};
use mahi_contracts::error::ContractError;
use mahi_contracts::tooling::{ToolEvent, ToolEventStream};
use serde_json::json;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Cap applied to each captured transcript (stdout and stderr) in the final
/// [`ToolEvent::Result`]; chunks stream uncapped, like `shell_exec`.
pub(crate) const MAX_CAPTURE_BYTES: usize = 100 * 1024;

/// How a script-running tool wants its child process executed.
pub(crate) struct SpawnSpec {
    /// Fully-argued command; stdio configuration is applied here.
    pub(crate) command: Command,
    /// The child is killed and an error reported once this elapses.
    pub(crate) timeout: Duration,
    /// Short human label used in error messages ("osascript", "python3", ...).
    pub(crate) display: String,
    /// Message reported when the binary is missing (`ErrorKind::NotFound`).
    pub(crate) not_found_message: String,
    /// Best-effort removed once the child is done (temp script files).
    pub(crate) cleanup_file: Option<PathBuf>,
}

/// Spawn the child and adapt it into a [`ToolEventStream`]: stdout/stderr
/// lines as [`ToolEvent::Chunk`]s, then one [`ToolEvent::Result`] with the
/// captured (capped) transcripts and exit code. Cancellation and timeout
/// kill the child, mirroring `shell_exec`.
pub(crate) fn spawn_streaming(spec: SpawnSpec, cancel: CancellationToken) -> ToolEventStream {
    let SpawnSpec {
        mut command,
        timeout,
        display,
        not_found_message,
        cleanup_file,
    } = spec;

    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            remove_cleanup_file(cleanup_file.as_deref());
            return if e.kind() == std::io::ErrorKind::NotFound {
                tool_error(not_found_message, false)
            } else {
                tool_error(format!("failed to spawn `{display}`: {e}"), true)
            };
        }
    };

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let (tx, rx) = mpsc::channel::<Result<ToolEvent, ContractError>>(64);
    tokio::spawn(async move {
        pump(child, stdout, stderr, timeout, cancel, display, &tx).await;
        // Runs on every exit path (result, error, timeout, cancel) before the
        // sender drops, so consumers never observe a stale temp script.
        remove_cleanup_file(cleanup_file.as_deref());
    });

    channel_stream(rx)
}

/// The streaming loop shared by all spawned children: forward lines as
/// chunks, accumulate capped transcripts, observe cancel/timeout, then
/// report the exit code.
async fn pump(
    mut child: tokio::process::Child,
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    timeout: Duration,
    cancel: CancellationToken,
    display: String,
    tx: &mpsc::Sender<Result<ToolEvent, ContractError>>,
) {
    let mut stdout_lines = BufReader::new(stdout).lines();
    let mut stderr_lines = BufReader::new(stderr).lines();
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();
    let mut truncated = false;
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                let _ = tx.send(Ok(ToolEvent::Cancelled)).await;
                return;
            }
            _ = tokio::time::sleep_until(deadline) => {
                let _ = child.kill().await;
                let _ = tx
                    .send(Ok(ToolEvent::Error {
                        message: format!("`{display}` timed out after {} ms", timeout.as_millis()),
                        retryable: true,
                    }))
                    .await;
                return;
            }
            line = stdout_lines.next_line(), if !stdout_done => match line {
                Ok(Some(l)) => {
                    let _ = tx.send(Ok(ToolEvent::Chunk { data: format!("{l}\n") })).await;
                    append_capped(&mut stdout_buf, &l, &mut truncated);
                }
                _ => stdout_done = true,
            },
            line = stderr_lines.next_line(), if !stderr_done => match line {
                Ok(Some(l)) => {
                    let _ = tx.send(Ok(ToolEvent::Chunk { data: format!("[stderr] {l}\n") })).await;
                    append_capped(&mut stderr_buf, &l, &mut truncated);
                }
                _ => stderr_done = true,
            },
        }
        if stdout_done && stderr_done {
            break;
        }
    }

    match child.wait().await {
        Ok(status) => {
            let _ = tx
                .send(Ok(ToolEvent::Result {
                    output: json!({
                        "stdout": stdout_buf,
                        "stderr": stderr_buf,
                        "exit_code": status.code(),
                        "success": status.success(),
                    }),
                    truncated,
                }))
                .await;
        }
        Err(e) => {
            let _ = tx
                .send(Ok(ToolEvent::Error {
                    message: format!("failed waiting for child process: {e}"),
                    retryable: true,
                }))
                .await;
        }
    }
}

/// Append one output line (plus newline) to a capped transcript, flagging
/// truncation once [`MAX_CAPTURE_BYTES`] is reached (UTF-8 boundary safe).
fn append_capped(buf: &mut String, line: &str, truncated: &mut bool) {
    let remaining = MAX_CAPTURE_BYTES.saturating_sub(buf.len());
    if remaining == 0 {
        *truncated = true;
        return;
    }
    if line.len() < remaining {
        buf.push_str(line);
        buf.push('\n');
        return;
    }
    let mut end = remaining.min(line.len());
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    buf.push_str(&line[..end]);
    *truncated = true;
}

fn remove_cleanup_file(path: Option<&std::path::Path>) {
    if let Some(path) = path {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn binary_available(binary: &str) -> bool {
        std::process::Command::new(binary)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    async fn collect(stream: ToolEventStream) -> Vec<Result<ToolEvent, ContractError>> {
        stream.collect::<Vec<_>>().await
    }

    fn spec(command: Command) -> SpawnSpec {
        SpawnSpec {
            command,
            timeout: Duration::from_secs(30),
            display: "test-child".to_string(),
            not_found_message: "test-child not found".to_string(),
            cleanup_file: None,
        }
    }

    #[test]
    fn append_capped_appends_lines_until_the_cap() {
        let mut buf = String::new();
        let mut truncated = false;
        append_capped(&mut buf, "hello", &mut truncated);
        append_capped(&mut buf, "world", &mut truncated);
        assert_eq!(buf, "hello\nworld\n");
        assert!(!truncated);

        let mut buf = String::new();
        let mut truncated = false;
        let long = "x".repeat(MAX_CAPTURE_BYTES + 5);
        append_capped(&mut buf, &long, &mut truncated);
        assert_eq!(buf.len(), MAX_CAPTURE_BYTES);
        assert!(truncated);

        // Once full, later lines are dropped but still flagged.
        let mut still_truncated = false;
        append_capped(&mut buf, "more", &mut still_truncated);
        assert_eq!(buf.len(), MAX_CAPTURE_BYTES);
        assert!(still_truncated);
    }

    #[test]
    fn append_capped_respects_utf8_boundaries() {
        // One byte of head-room, then a 2-byte char: nothing fits, but the
        // buffer must stay valid UTF-8.
        let mut buf = "a".repeat(MAX_CAPTURE_BYTES - 1);
        let mut truncated = false;
        append_capped(&mut buf, "é", &mut truncated);
        assert_eq!(buf.len(), MAX_CAPTURE_BYTES - 1);
        assert!(truncated);
        assert!(buf.is_char_boundary(buf.len()));
    }

    #[tokio::test]
    async fn missing_binary_reports_the_not_found_message() {
        let events = collect(spawn_streaming(
            spec(Command::new("mahi-definitely-not-a-real-binary")),
            CancellationToken::new(),
        ))
        .await;
        assert!(
            matches!(
                events.last(),
                Some(Ok(ToolEvent::Error { message, retryable: false }))
                    if message == "test-child not found"
            ),
            "expected the not-found message, got {events:?}"
        );
    }

    #[tokio::test]
    async fn timeout_kills_the_child_and_reports() {
        if !binary_available("bash") {
            eprintln!("skipping: bash not available");
            return;
        }
        let started = std::time::Instant::now();
        let mut command = Command::new("bash");
        command.args(["-c", "sleep 30"]);
        let mut spec = spec(command);
        spec.timeout = Duration::from_millis(200);
        let events = collect(spawn_streaming(spec, CancellationToken::new())).await;
        assert!(
            matches!(
                events.last(),
                Some(Ok(ToolEvent::Error { message, retryable: true }))
                    if message.contains("timed out after 200 ms")
            ),
            "expected a timeout error, got {events:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "timeout must kill the child promptly"
        );
    }

    #[tokio::test]
    async fn cancel_kills_the_child_and_yields_cancelled() {
        if !binary_available("bash") {
            eprintln!("skipping: bash not available");
            return;
        }
        let started = std::time::Instant::now();
        let cancel = CancellationToken::new();
        let mut command = Command::new("bash");
        command.args(["-c", "sleep 30"]);
        let stream = spawn_streaming(spec(command), cancel.clone());
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.cancel();
        });
        let events = collect(stream).await;
        canceller.await.expect("canceller task");
        assert!(
            matches!(events.last(), Some(Ok(ToolEvent::Cancelled))),
            "expected Cancelled, got {events:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "cancel must kill the child promptly"
        );
    }

    #[tokio::test]
    async fn cleanup_file_is_removed_once_the_stream_ends() {
        if !binary_available("bash") {
            eprintln!("skipping: bash not available");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("script.sh");
        std::fs::write(&path, "echo hi").expect("write script");
        let mut command = Command::new("bash");
        command.arg(&path);
        let mut spec = spec(command);
        spec.cleanup_file = Some(path.clone());
        let events = collect(spawn_streaming(spec, CancellationToken::new())).await;
        match events.last() {
            Some(Ok(ToolEvent::Result { output, truncated })) => {
                assert_eq!(output["stdout"], "hi\n");
                assert_eq!(output["exit_code"], 0);
                assert_eq!(output["success"], true);
                assert!(!truncated);
            }
            other => panic!("expected a final Result, got {other:?}"),
        }
        assert!(!path.exists(), "temp script must be cleaned up");
    }
}
