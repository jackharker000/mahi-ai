//! Built-in `run_code` tool: a lightweight code interpreter in the spirit of
//! Codex / Open Interpreter. The snippet is handed to the interpreter inline
//! (`python3 -c` / `bash -c` / `node -e` / `ruby -e`); stdout/stderr stream
//! back as chunks and are captured (capped) into the final result, mirroring
//! `shell_exec`'s machinery. Approval-gated and only offered when a Mac
//! executes the tools, because it runs real processes there.

use crate::tool::{parse_args, tool_error, Tool};
use crate::tools::process::{spawn_streaming, SpawnSpec};
use async_trait::async_trait;
use mahi_contracts::tooling::{DestructiveLevel, ToolCategory, ToolDescriptor, ToolEventStream};
use mahi_contracts::types::ComputeMode;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Like `shell_exec`, only offered when a Mac executes the tools (B/C).
const RUN_CODE_MODES: [ComputeMode; 2] = [ComputeMode::MacLan, ComputeMode::MacRemote];

const DEFAULT_TIMEOUT_MS: u64 = 120_000;

#[derive(Default)]
pub struct RunCodeTool;

impl RunCodeTool {
    pub(crate) fn new() -> Self {
        Self
    }
}

/// Supported interpreters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum CodeLanguage {
    Python,
    Bash,
    Javascript,
    Ruby,
}

impl CodeLanguage {
    /// Interpreter binary, resolved via `PATH`.
    fn interpreter(self) -> &'static str {
        match self {
            CodeLanguage::Python => "python3",
            CodeLanguage::Bash => "bash",
            CodeLanguage::Javascript => "node",
            CodeLanguage::Ruby => "ruby",
        }
    }

    /// Flag that makes the interpreter execute its next argument as source.
    fn inline_flag(self) -> &'static str {
        match self {
            CodeLanguage::Python | CodeLanguage::Bash => "-c",
            CodeLanguage::Javascript | CodeLanguage::Ruby => "-e",
        }
    }
}

#[derive(Deserialize)]
struct RunCodeArgs {
    language: CodeLanguage,
    code: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[async_trait]
impl Tool for RunCodeTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            id: "run_code".to_string(),
            display_name: "Run Code Snippet".to_string(),
            category: ToolCategory::BuiltIn,
            available_in_modes: RUN_CODE_MODES.to_vec(),
            required_permissions: vec!["shell.exec".to_string()],
            input_schema: json!({
                "type": "object",
                "description": "Run a Python, Bash, JavaScript (Node.js), or Ruby snippet and \
                    get its stdout, stderr, and exit code — use for computation, data wrangling, \
                    file/system tasks, and quick scripts where a real interpreter beats chaining \
                    shell utilities. Print whatever you want returned.",
                "properties": {
                    "language": {
                        "type": "string",
                        "enum": ["python", "bash", "javascript", "ruby"],
                        "description": "Interpreter: python → python3, bash → bash, \
                            javascript → node, ruby → ruby."
                    },
                    "code": {
                        "type": "string",
                        "description": "Source code to execute. Emit results on stdout \
                            (print / echo / console.log / puts)."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "default": DEFAULT_TIMEOUT_MS,
                        "description": "Kill the interpreter after this many milliseconds."
                    }
                },
                "required": ["language", "code"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "stdout": { "type": "string", "description": "Captured stdout (capped at 100 KB)" },
                    "stderr": { "type": "string", "description": "Captured stderr (capped at 100 KB)" },
                    "exit_code": { "type": ["integer", "null"] },
                    "success": { "type": "boolean" }
                }
            }),
            requires_approval: true,
            destructive_level: DestructiveLevel::High,
        }
    }

    async fn run(&self, args: serde_json::Value, cancel: CancellationToken) -> ToolEventStream {
        let args: RunCodeArgs = match parse_args(args) {
            Ok(a) => a,
            Err(msg) => return tool_error(msg, false),
        };
        if args.code.trim().is_empty() {
            return tool_error("code must not be empty", false);
        }
        let interpreter = args.language.interpreter();
        let mut command = Command::new(interpreter);
        command.arg(args.language.inline_flag()).arg(&args.code);

        spawn_streaming(
            SpawnSpec {
                command,
                timeout: Duration::from_millis(args.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)),
                display: interpreter.to_string(),
                not_found_message: format!("interpreter `{interpreter}` not found"),
                cleanup_file: None,
            },
            cancel,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::MockComputerController;
    use crate::registry::ToolRegistry;
    use crate::tools::process::MAX_CAPTURE_BYTES;
    use futures::StreamExt;
    use mahi_contracts::error::ContractError;
    use mahi_contracts::tooling::{ToolEvent, ToolInvokeContract};
    use std::sync::Arc;

    fn interpreter_available(binary: &str) -> bool {
        std::process::Command::new(binary)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    async fn run(args: serde_json::Value) -> Vec<Result<ToolEvent, ContractError>> {
        RunCodeTool::new()
            .run(args, CancellationToken::new())
            .await
            .collect::<Vec<_>>()
            .await
    }

    fn final_result(events: &[Result<ToolEvent, ContractError>]) -> serde_json::Value {
        match events.last() {
            Some(Ok(ToolEvent::Result { output, .. })) => output.clone(),
            other => panic!("expected final Result event, got {other:?} (all: {events:?})"),
        }
    }

    #[test]
    fn descriptor_mirrors_shell_exec_gating() {
        let d = RunCodeTool::new().descriptor();
        assert_eq!(d.id, "run_code");
        assert_eq!(d.category, ToolCategory::BuiltIn);
        assert_eq!(
            d.available_in_modes,
            vec![ComputeMode::MacLan, ComputeMode::MacRemote]
        );
        assert!(d.requires_approval);
        assert_eq!(d.destructive_level, DestructiveLevel::High);
        assert_eq!(d.input_schema["required"], json!(["language", "code"]));
        assert_eq!(
            d.input_schema["properties"]["language"]["enum"],
            json!(["python", "bash", "javascript", "ruby"])
        );
    }

    #[tokio::test]
    async fn both_script_tools_are_registered_for_mac_modes_only() {
        let registry = ToolRegistry::with_builtins(Arc::new(MockComputerController::new()));
        let ids = |tools: Vec<mahi_contracts::tooling::ToolDescriptor>| {
            tools.into_iter().map(|t| t.id).collect::<Vec<_>>()
        };

        let mac = ids(registry.describe(ComputeMode::MacLan).await);
        let remote = ids(registry.describe(ComputeMode::MacRemote).await);
        for id in ["apple_script", "run_code"] {
            assert!(mac.contains(&id.to_string()), "MacLan should offer {id}");
            assert!(
                remote.contains(&id.to_string()),
                "MacRemote should offer {id}"
            );
        }

        let hosted = ids(registry.describe(ComputeMode::Hosted).await);
        let on_device = ids(registry.describe(ComputeMode::OnDevice).await);
        for id in ["apple_script", "run_code"] {
            assert!(
                !hosted.contains(&id.to_string()),
                "Hosted must not offer {id}"
            );
            assert!(
                !on_device.contains(&id.to_string()),
                "OnDevice must not offer {id}"
            );
        }
    }

    #[tokio::test]
    async fn python_snippet_prints_two() {
        if !interpreter_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let events = run(json!({ "language": "python", "code": "print(1+1)" })).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Ok(ToolEvent::Chunk { data }) if data == "2\n")),
            "stdout should stream as a chunk: {events:?}"
        );
        let output = final_result(&events);
        assert_eq!(output["stdout"], "2\n");
        assert_eq!(output["stderr"], "");
        assert_eq!(output["exit_code"], 0);
        assert_eq!(output["success"], true);
    }

    #[tokio::test]
    async fn bash_snippet_echoes() {
        if !interpreter_available("bash") {
            eprintln!("skipping: bash not available");
            return;
        }
        let events = run(json!({ "language": "bash", "code": "echo hi" })).await;
        let output = final_result(&events);
        assert_eq!(output["stdout"], "hi\n");
        assert_eq!(output["exit_code"], 0);
    }

    #[tokio::test]
    async fn javascript_snippet_logs() {
        if !interpreter_available("node") {
            eprintln!("skipping: node not available");
            return;
        }
        let events = run(json!({ "language": "javascript", "code": "console.log(6*7)" })).await;
        assert_eq!(final_result(&events)["stdout"], "42\n");
    }

    #[tokio::test]
    async fn ruby_snippet_puts() {
        if !interpreter_available("ruby") {
            eprintln!("skipping: ruby not available");
            return;
        }
        let events = run(json!({ "language": "ruby", "code": "puts 1+1" })).await;
        assert_eq!(final_result(&events)["stdout"], "2\n");
    }

    #[tokio::test]
    async fn unknown_language_is_rejected() {
        let events = run(json!({ "language": "perl", "code": "print 1" })).await;
        assert!(
            matches!(
                events.last(),
                Some(Ok(ToolEvent::Error { message, retryable: false }))
                    if message.contains("unknown variant `perl`")
            ),
            "expected a schema error naming the bad language, got {events:?}"
        );
    }

    #[tokio::test]
    async fn empty_code_is_rejected() {
        let events = run(json!({ "language": "python", "code": "  \n" })).await;
        assert!(
            matches!(
                events.last(),
                Some(Ok(ToolEvent::Error { message, retryable: false }))
                    if message == "code must not be empty"
            ),
            "expected an empty-code error, got {events:?}"
        );
    }

    #[tokio::test]
    async fn stderr_and_exit_code_are_reported() {
        if !interpreter_available("bash") {
            eprintln!("skipping: bash not available");
            return;
        }
        let events = run(json!({ "language": "bash", "code": "echo oops 1>&2; exit 3" })).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Ok(ToolEvent::Chunk { data }) if data == "[stderr] oops\n")),
            "stderr should stream as a tagged chunk: {events:?}"
        );
        let output = final_result(&events);
        assert_eq!(output["stderr"], "oops\n");
        assert_eq!(output["exit_code"], 3);
        assert_eq!(output["success"], false);
    }

    #[tokio::test]
    async fn long_output_is_truncated_in_the_result() {
        if !interpreter_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let events = run(json!({
            "language": "python",
            "code": "print('x' * 300000)"
        }))
        .await;
        match events.last() {
            Some(Ok(ToolEvent::Result { output, truncated })) => {
                assert!(truncated, "a 300 KB line must set truncated");
                let stdout = output["stdout"].as_str().expect("stdout string");
                assert!(
                    stdout.len() <= MAX_CAPTURE_BYTES,
                    "captured stdout must be capped, got {} bytes",
                    stdout.len()
                );
                assert_eq!(output["exit_code"], 0);
            }
            other => panic!("expected a final Result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancel_kills_the_interpreter() {
        if !interpreter_available("bash") {
            eprintln!("skipping: bash not available");
            return;
        }
        let started = std::time::Instant::now();
        let cancel = CancellationToken::new();
        let stream = RunCodeTool::new()
            .run(
                json!({ "language": "bash", "code": "sleep 30" }),
                cancel.clone(),
            )
            .await;
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.cancel();
        });
        let events = stream.collect::<Vec<_>>().await;
        canceller.await.expect("canceller task");
        assert!(
            matches!(events.last(), Some(Ok(ToolEvent::Cancelled))),
            "expected Cancelled, got {events:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "cancel must kill the interpreter promptly"
        );
    }

    #[tokio::test]
    async fn timeout_ms_is_honoured() {
        if !interpreter_available("bash") {
            eprintln!("skipping: bash not available");
            return;
        }
        let events = run(json!({
            "language": "bash",
            "code": "sleep 30",
            "timeout_ms": 200
        }))
        .await;
        assert!(
            matches!(
                events.last(),
                Some(Ok(ToolEvent::Error { message, retryable: true }))
                    if message.contains("timed out after 200 ms")
            ),
            "expected a timeout error, got {events:?}"
        );
    }
}
