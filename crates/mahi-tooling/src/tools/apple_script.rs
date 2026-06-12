//! Built-in `apple_script` tool: precise, script-driven macOS app control by
//! running AppleScript or JXA through `/usr/bin/osascript`.
//!
//! This is the "smart" computer-use path: when an app is scriptable, a few
//! lines of AppleScript (open/quit apps, move windows, read or set values,
//! click menu items, query Finder/Safari/Mail/Notes/Calendar) are
//! deterministic and far more reliable than screenshot-and-click. Like
//! `shell_exec` it is approval-gated and only offered when a Mac executes
//! the tools; on other platforms it reports an honest error.

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
const APPLE_SCRIPT_MODES: [ComputeMode; 2] = [ComputeMode::MacLan, ComputeMode::MacRemote];

const OSASCRIPT: &str = "/usr/bin/osascript";
const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const NOT_MACOS_MSG: &str = "apple_script requires macOS";

#[derive(Default)]
pub struct AppleScriptTool;

impl AppleScriptTool {
    pub(crate) fn new() -> Self {
        Self
    }
}

/// Which osascript language to run the script as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ScriptLanguage {
    #[default]
    Applescript,
    Javascript,
}

impl ScriptLanguage {
    fn extension(self) -> &'static str {
        match self {
            ScriptLanguage::Applescript => "applescript",
            ScriptLanguage::Javascript => "js",
        }
    }
}

#[derive(Deserialize)]
struct AppleScriptArgs {
    /// AppleScript (or JXA) source to execute.
    script: String,
    /// Defaults to AppleScript; `javascript` runs JXA via `-l JavaScript`.
    #[serde(default)]
    language: ScriptLanguage,
}

#[async_trait]
impl Tool for AppleScriptTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            id: "apple_script".to_string(),
            display_name: "Automate Mac Apps (AppleScript)".to_string(),
            category: ToolCategory::BuiltIn,
            available_in_modes: APPLE_SCRIPT_MODES.to_vec(),
            required_permissions: vec!["automation.applescript".to_string()],
            input_schema: json!({
                "type": "object",
                "description": "Control macOS apps precisely with AppleScript or JXA, run via \
                    osascript: open/quit apps, manipulate windows, read and set values, click \
                    menu items, send keystrokes to a named app, and query Finder, Safari, Mail, \
                    Notes, or Calendar. Deterministic and far more reliable than \
                    screenshot-and-click — prefer this over ui_click/ui_type whenever the target \
                    app is scriptable. The script's return value and output are captured as \
                    stdout, script errors as stderr.",
                "properties": {
                    "script": {
                        "type": "string",
                        "description": "The AppleScript (or JXA) source to run, e.g. `tell \
                            application \"Safari\" to get URL of front document`. Multi-line \
                            scripts are supported."
                    },
                    "language": {
                        "type": "string",
                        "enum": ["applescript", "javascript"],
                        "default": "applescript",
                        "description": "Scripting language: \"applescript\" (default) or \
                            \"javascript\" for JXA (JavaScript for Automation)."
                    }
                },
                "required": ["script"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "stdout": { "type": "string", "description": "Script result/output (capped at 100 KB)" },
                    "stderr": { "type": "string", "description": "osascript errors and warnings (capped at 100 KB)" },
                    "exit_code": { "type": ["integer", "null"] },
                    "success": { "type": "boolean" }
                }
            }),
            requires_approval: true,
            destructive_level: DestructiveLevel::High,
        }
    }

    async fn run(&self, args: serde_json::Value, cancel: CancellationToken) -> ToolEventStream {
        let args: AppleScriptArgs = match parse_args(args) {
            Ok(a) => a,
            Err(msg) => return tool_error(msg, false),
        };
        if args.script.trim().is_empty() {
            return tool_error("script must not be empty", false);
        }
        if !cfg!(target_os = "macos") {
            return tool_error(NOT_MACOS_MSG, false);
        }

        // Multi-line scripts are the norm; staging the source in a temp file
        // is cleaner than chaining `-e` lines and avoids quoting pitfalls.
        // The runner deletes it once the child is done.
        let path = std::env::temp_dir().join(format!(
            "mahi-apple-script-{}.{}",
            uuid::Uuid::new_v4(),
            args.language.extension(),
        ));
        if let Err(e) = tokio::fs::write(&path, &args.script).await {
            return tool_error(
                format!("failed to stage script at {}: {e}", path.display()),
                true,
            );
        }

        let mut command = Command::new(OSASCRIPT);
        if args.language == ScriptLanguage::Javascript {
            command.args(["-l", "JavaScript"]);
        }
        command.arg(&path);

        spawn_streaming(
            SpawnSpec {
                command,
                timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
                display: "osascript".to_string(),
                not_found_message: format!("{NOT_MACOS_MSG} (`{OSASCRIPT}` not found)"),
                cleanup_file: Some(path),
            },
            cancel,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use mahi_contracts::error::ContractError;
    use mahi_contracts::tooling::ToolEvent;

    async fn run(args: serde_json::Value) -> Vec<Result<ToolEvent, ContractError>> {
        AppleScriptTool::new()
            .run(args, CancellationToken::new())
            .await
            .collect::<Vec<_>>()
            .await
    }

    #[test]
    fn descriptor_mirrors_shell_exec_gating() {
        let d = AppleScriptTool::new().descriptor();
        assert_eq!(d.id, "apple_script");
        assert_eq!(d.category, ToolCategory::BuiltIn);
        assert_eq!(
            d.available_in_modes,
            vec![ComputeMode::MacLan, ComputeMode::MacRemote]
        );
        assert!(d.requires_approval);
        assert_eq!(d.destructive_level, DestructiveLevel::High);
        assert_eq!(d.input_schema["required"], json!(["script"]));
        assert_eq!(
            d.input_schema["properties"]["language"]["enum"],
            json!(["applescript", "javascript"])
        );
    }

    #[tokio::test]
    async fn rejects_empty_script() {
        let events = run(json!({ "script": "   " })).await;
        assert!(
            matches!(
                events.last(),
                Some(Ok(ToolEvent::Error { message, retryable: false }))
                    if message == "script must not be empty"
            ),
            "expected an empty-script error, got {events:?}"
        );
    }

    #[tokio::test]
    async fn rejects_unknown_language() {
        let events = run(json!({ "script": "return 1", "language": "perl" })).await;
        assert!(
            matches!(
                events.last(),
                Some(Ok(ToolEvent::Error { message, .. })) if message.contains("unknown variant")
            ),
            "expected a schema error, got {events:?}"
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn reports_macos_requirement_off_mac() {
        let events = run(json!({ "script": "return 1 + 1" })).await;
        assert!(
            matches!(
                events.last(),
                Some(Ok(ToolEvent::Error { message, retryable: false }))
                    if message == "apple_script requires macOS"
            ),
            "expected the macOS-requirement error, got {events:?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn runs_applescript_and_jxa_via_osascript() {
        let events = run(json!({ "script": "return 1 + 1" })).await;
        match events.last() {
            Some(Ok(ToolEvent::Result { output, .. })) => {
                assert_eq!(output["stdout"], "2\n");
                assert_eq!(output["exit_code"], 0);
                assert_eq!(output["success"], true);
            }
            other => panic!("expected a final Result, got {other:?}"),
        }

        let events = run(json!({ "script": "21 * 2", "language": "javascript" })).await;
        match events.last() {
            Some(Ok(ToolEvent::Result { output, .. })) => {
                assert_eq!(output["stdout"], "42\n");
                assert_eq!(output["success"], true);
            }
            other => panic!("expected a final Result, got {other:?}"),
        }
    }
}
