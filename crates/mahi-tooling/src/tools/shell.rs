//! Built-in `shell_exec` tool: runs a command via `tokio::process`, streaming
//! stdout/stderr lines as [`ToolEvent::Chunk`]s, ending in a
//! [`ToolEvent::Result`] with the exit code.
//!
//! Approval-gated and `Critical`: the agent core must surface an approval
//! before invoking (the descriptor carries `requires_approval = true`).
//
// TODO(contracts): the frozen contract has no sandbox-profile field on
// ToolInvocation; full sandboxing (seccomp / App Sandbox, no-network) lands
// with the SandboxedExecutor design in domain doc §2.

use crate::tool::{channel_stream, contract_error, parse_args, tool_error, Tool};
use crate::tools::file::FileScope;
use async_trait::async_trait;
use mahi_contracts::error::ContractError;
use mahi_contracts::tooling::{DestructiveLevel, ToolCategory, ToolDescriptor, ToolEvent, ToolEventStream};
use mahi_contracts::types::ComputeMode;
use serde::Deserialize;
use serde_json::json;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Shell is only offered when a Mac executes the tools (mode matrix B/C).
const SHELL_MODES: [ComputeMode; 2] = [ComputeMode::MacLan, ComputeMode::MacRemote];

const DEFAULT_TIMEOUT_MS: u64 = 120_000;

pub struct ShellExecTool {
    scope: FileScope,
}

impl ShellExecTool {
    pub(crate) fn new(scope: FileScope) -> Self {
        Self { scope }
    }
}

#[derive(Deserialize)]
struct ShellExecArgs {
    /// Command line, run via `sh -c`.
    command: String,
    /// Working directory (must stay inside the allowed root). Defaults to it.
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[async_trait]
impl Tool for ShellExecTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            id: "shell_exec".to_string(),
            display_name: "Run Shell Command".to_string(),
            category: ToolCategory::BuiltIn,
            available_in_modes: SHELL_MODES.to_vec(),
            required_permissions: vec!["shell.exec".to_string()],
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Command line, executed with sh -c" },
                    "cwd": { "type": "string", "description": "Working directory inside the allowed root" },
                    "timeout_ms": { "type": "integer", "default": DEFAULT_TIMEOUT_MS }
                },
                "required": ["command"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "exit_code": { "type": ["integer", "null"] },
                    "success": { "type": "boolean" }
                }
            }),
            requires_approval: true,
            destructive_level: DestructiveLevel::Critical,
        }
    }

    async fn run(&self, args: serde_json::Value, cancel: CancellationToken) -> ToolEventStream {
        let args: ShellExecArgs = match parse_args(args) {
            Ok(a) => a,
            Err(msg) => return tool_error(msg, false),
        };
        if args.command.trim().is_empty() {
            return tool_error("command must not be empty", false);
        }
        let cwd = match args.cwd.as_deref() {
            Some(p) => match self.scope.resolve(p) {
                Ok(p) => p,
                Err(e) => return contract_error(e),
            },
            None => self.scope.root().to_path_buf(),
        };
        let timeout = std::time::Duration::from_millis(args.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));

        let mut child = match Command::new("sh")
            .arg("-c")
            .arg(&args.command)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return tool_error(format!("failed to spawn `sh -c`: {e}"), true),
        };

        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        let (tx, rx) = mpsc::channel::<Result<ToolEvent, ContractError>>(64);
        tokio::spawn(async move {
            let mut stdout_lines = BufReader::new(stdout).lines();
            let mut stderr_lines = BufReader::new(stderr).lines();
            let mut stdout_done = false;
            let mut stderr_done = false;
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
                                message: format!("command timed out after {} ms", timeout.as_millis()),
                                retryable: true,
                            }))
                            .await;
                        return;
                    }
                    line = stdout_lines.next_line(), if !stdout_done => match line {
                        Ok(Some(l)) => {
                            let _ = tx.send(Ok(ToolEvent::Chunk { data: format!("{l}\n") })).await;
                        }
                        _ => stdout_done = true,
                    },
                    line = stderr_lines.next_line(), if !stderr_done => match line {
                        Ok(Some(l)) => {
                            let _ = tx.send(Ok(ToolEvent::Chunk { data: format!("[stderr] {l}\n") })).await;
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
                                "exit_code": status.code(),
                                "success": status.success()
                            }),
                            truncated: false,
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
        });

        channel_stream(rx)
    }
}
