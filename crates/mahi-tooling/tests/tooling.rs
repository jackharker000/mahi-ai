//! Integration tests for the mahi-tooling registry, built-in tools,
//! computer-use safety, and the connector broker. Nothing here touches the
//! network.

use futures::StreamExt;
use mahi_contracts::error::{ContractError, ToolError};
use mahi_contracts::tooling::{
    DestructiveLevel, ToolCategory, ToolEvent, ToolEventStream, ToolInvocation, ToolInvokeContract,
};
use mahi_contracts::types::ComputeMode;
use mahi_tooling::{
    ControllerAction, EchoConnector, MockComputerController, MouseButton, ToolRegistry, UiBounds,
    UiElement,
};
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn invocation(tool_id: &str, args: serde_json::Value, mode: ComputeMode) -> ToolInvocation {
    ToolInvocation {
        invocation_id: uuid::Uuid::new_v4(),
        tool_id: tool_id.to_string(),
        args,
        session_id: uuid::Uuid::new_v4(),
        conversation_id: uuid::Uuid::new_v4(),
        message_id: uuid::Uuid::new_v4(),
        compute_mode: mode,
        trace_id: uuid::Uuid::new_v4(),
        stream: true,
    }
}

async fn collect(stream: ToolEventStream) -> Vec<Result<ToolEvent, ContractError>> {
    stream.collect::<Vec<_>>().await
}

async fn run_tool(
    registry: &ToolRegistry,
    tool_id: &str,
    args: serde_json::Value,
    mode: ComputeMode,
) -> Vec<Result<ToolEvent, ContractError>> {
    let stream = registry
        .invoke(invocation(tool_id, args, mode), CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("invoke of {tool_id} failed: {e}"));
    collect(stream).await
}

/// The final event must be a successful Result; return its output.
fn final_result(events: &[Result<ToolEvent, ContractError>]) -> serde_json::Value {
    match events.last() {
        Some(Ok(ToolEvent::Result { output, .. })) => output.clone(),
        other => panic!("expected final Result event, got {other:?} (all: {events:?})"),
    }
}

fn registry_in(dir: &std::path::Path) -> (ToolRegistry, Arc<MockComputerController>) {
    let controller = Arc::new(MockComputerController::new());
    let registry = ToolRegistry::with_builtins_scoped(controller.clone(), dir);
    (registry, controller)
}

// ---------------------------------------------------------------------------
// File tools
// ---------------------------------------------------------------------------

#[tokio::test]
async fn file_tools_round_trip_in_tempdir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (registry, _) = registry_in(dir.path());
    let mode = ComputeMode::MacLan;

    // Write.
    let events = run_tool(
        &registry,
        "file_write",
        json!({ "path": "notes/hello.txt", "content": "hello mahi\nsecond line\n" }),
        mode,
    )
    .await;
    let output = final_result(&events);
    assert_eq!(output["bytes_written"], 23);

    // Read it back.
    let events = run_tool(
        &registry,
        "file_read",
        json!({ "path": "notes/hello.txt" }),
        mode,
    )
    .await;
    let output = final_result(&events);
    assert_eq!(output["content"], "hello mahi\nsecond line\n");

    // Edit.
    let events = run_tool(
        &registry,
        "file_edit",
        json!({ "path": "notes/hello.txt", "old_string": "hello mahi", "new_string": "kia ora mahi" }),
        mode,
    )
    .await;
    assert_eq!(final_result(&events)["replacements"], 1);

    // Read again — edit visible.
    let events = run_tool(
        &registry,
        "file_read",
        json!({ "path": "notes/hello.txt" }),
        mode,
    )
    .await;
    assert_eq!(
        final_result(&events)["content"],
        "kia ora mahi\nsecond line\n"
    );

    // Search finds the content and the file name.
    let events = run_tool(
        &registry,
        "file_search",
        json!({ "query": "kia ora" }),
        mode,
    )
    .await;
    let matches = final_result(&events)["matches"]
        .as_array()
        .expect("matches array")
        .clone();
    assert!(
        matches.iter().any(|m| m["line"] == 1
            && m["path"]
                .as_str()
                .unwrap_or_default()
                .ends_with("hello.txt")),
        "content match expected, got {matches:?}"
    );
    let events = run_tool(
        &registry,
        "file_search",
        json!({ "query": "hello.txt" }),
        mode,
    )
    .await;
    let matches = final_result(&events)["matches"]
        .as_array()
        .expect("matches array")
        .clone();
    assert!(!matches.is_empty(), "file-name match expected");
}

#[tokio::test]
async fn file_edit_rejects_missing_and_ambiguous_old_string() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (registry, _) = registry_in(dir.path());
    let mode = ComputeMode::OnDevice;

    run_tool(
        &registry,
        "file_write",
        json!({ "path": "a.txt", "content": "dup dup" }),
        mode,
    )
    .await;

    let events = run_tool(
        &registry,
        "file_edit",
        json!({ "path": "a.txt", "old_string": "absent", "new_string": "x" }),
        mode,
    )
    .await;
    assert!(matches!(
        events.last(),
        Some(Ok(ToolEvent::Error { message, .. })) if message.contains("not found")
    ));

    let events = run_tool(
        &registry,
        "file_edit",
        json!({ "path": "a.txt", "old_string": "dup", "new_string": "x" }),
        mode,
    )
    .await;
    assert!(matches!(
        events.last(),
        Some(Ok(ToolEvent::Error { message, .. })) if message.contains("replace_all")
    ));

    // replace_all succeeds.
    let events = run_tool(
        &registry,
        "file_edit",
        json!({ "path": "a.txt", "old_string": "dup", "new_string": "x", "replace_all": true }),
        mode,
    )
    .await;
    assert_eq!(final_result(&events)["replacements"], 2);
}

#[tokio::test]
async fn file_tools_refuse_to_escape_allowed_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (registry, _) = registry_in(dir.path());

    for (tool, args) in [
        ("file_read", json!({ "path": "../outside.txt" })),
        (
            "file_write",
            json!({ "path": "/etc/mahi-evil.txt", "content": "x" }),
        ),
        (
            "file_edit",
            json!({ "path": "../../x", "old_string": "a", "new_string": "b" }),
        ),
    ] {
        let events = run_tool(&registry, tool, args, ComputeMode::MacLan).await;
        assert!(
            matches!(
                events.last(),
                Some(Err(ContractError::Tool(ToolError::SandboxViolation)))
            ),
            "{tool} must report a sandbox violation, got {events:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Registry: describe filtering, dispatch errors, approval metadata
// ---------------------------------------------------------------------------

#[tokio::test]
async fn describe_filters_by_compute_mode() {
    let controller = Arc::new(MockComputerController::new());
    let registry = ToolRegistry::with_builtins(controller);

    let ids = |tools: &[mahi_contracts::tooling::ToolDescriptor]| {
        tools.iter().map(|t| t.id.clone()).collect::<Vec<_>>()
    };

    // MacLan (B): everything.
    let mac = ids(&registry.describe(ComputeMode::MacLan).await);
    for expected in [
        "file_read",
        "file_write",
        "file_edit",
        "file_search",
        "shell_exec",
        "web_fetch",
        "web_search",
        "screen_capture",
        "ui_describe",
        "ui_click",
        "ui_type",
        "ui_scroll",
        "ui_key",
    ] {
        assert!(
            mac.contains(&expected.to_string()),
            "MacLan should offer {expected}"
        );
    }

    // Hosted (D): web only — no local fs, shell, or computer use.
    let hosted = ids(&registry.describe(ComputeMode::Hosted).await);
    assert!(hosted.contains(&"web_fetch".to_string()));
    assert!(hosted.contains(&"web_search".to_string()));
    for absent in [
        "file_read",
        "file_write",
        "shell_exec",
        "ui_click",
        "screen_capture",
    ] {
        assert!(
            !hosted.contains(&absent.to_string()),
            "Hosted must not offer {absent}"
        );
    }

    // OnDevice (A): file tools but no shell/web/computer-use.
    let on_device = ids(&registry.describe(ComputeMode::OnDevice).await);
    assert!(on_device.contains(&"file_read".to_string()));
    for absent in ["shell_exec", "web_fetch", "ui_type"] {
        assert!(
            !on_device.contains(&absent.to_string()),
            "OnDevice must not offer {absent}"
        );
    }
}

#[tokio::test]
async fn invoke_rejects_unknown_tool_and_wrong_mode() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (registry, _) = registry_in(dir.path());

    let err = registry
        .invoke(
            invocation("nope", json!({}), ComputeMode::MacLan),
            CancellationToken::new(),
        )
        .await
        .err()
        .expect("unknown tool must fail");
    assert!(matches!(
        err,
        ContractError::Tool(ToolError::NotFound { .. })
    ));

    let err = registry
        .invoke(
            invocation(
                "shell_exec",
                json!({ "command": "true" }),
                ComputeMode::OnDevice,
            ),
            CancellationToken::new(),
        )
        .await
        .err()
        .expect("shell_exec is unavailable on-device");
    assert!(matches!(
        err,
        ContractError::Tool(ToolError::UnavailableInMode { .. })
    ));
}

#[tokio::test]
async fn approval_and_destructive_metadata_is_reported() {
    let controller = Arc::new(MockComputerController::new());
    let registry = ToolRegistry::with_builtins(controller);
    let tools = registry.describe(ComputeMode::MacLan).await;
    let get = |id: &str| {
        tools
            .iter()
            .find(|t| t.id == id)
            .unwrap_or_else(|| panic!("{id}?"))
    };

    for approval_required in [
        "file_write",
        "file_edit",
        "shell_exec",
        "ui_click",
        "ui_type",
        "ui_key",
    ] {
        assert!(
            get(approval_required).requires_approval,
            "{approval_required} needs approval"
        );
    }
    for read_only in [
        "file_read",
        "file_search",
        "screen_capture",
        "ui_describe",
        "web_fetch",
    ] {
        assert!(
            !get(read_only).requires_approval,
            "{read_only} must not need approval"
        );
    }
    assert_eq!(get("file_write").destructive_level, DestructiveLevel::High);
    assert_eq!(get("file_edit").destructive_level, DestructiveLevel::High);
    assert_eq!(
        get("shell_exec").destructive_level,
        DestructiveLevel::Critical
    );
    assert_eq!(get("file_read").destructive_level, DestructiveLevel::Low);
    assert_eq!(get("screen_capture").category, ToolCategory::ComputerUse);
}

#[tokio::test]
async fn pre_cancelled_invocation_yields_cancelled_event() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (registry, _) = registry_in(dir.path());
    let cancel = CancellationToken::new();
    cancel.cancel();
    let stream = registry
        .invoke(
            invocation(
                "file_read",
                json!({ "path": "whatever.txt" }),
                ComputeMode::MacLan,
            ),
            cancel,
        )
        .await
        .expect("invoke ok");
    let events = collect(stream).await;
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], Ok(ToolEvent::Cancelled)));
}

// ---------------------------------------------------------------------------
// shell_exec
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shell_exec_streams_chunks_and_exit_code() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (registry, _) = registry_in(dir.path());

    let events = run_tool(
        &registry,
        "shell_exec",
        json!({ "command": "echo out-line; echo err-line 1>&2; exit 3" }),
        ComputeMode::MacLan,
    )
    .await;

    let chunks: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            Ok(ToolEvent::Chunk { data }) => Some(data.clone()),
            _ => None,
        })
        .collect();
    assert!(
        chunks.iter().any(|c| c.contains("out-line")),
        "stdout chunk missing: {chunks:?}"
    );
    assert!(
        chunks.iter().any(|c| c.contains("[stderr] err-line")),
        "stderr chunk missing: {chunks:?}"
    );

    let output = final_result(&events);
    assert_eq!(output["exit_code"], 3);
    assert_eq!(output["success"], false);
}

#[tokio::test]
async fn shell_exec_runs_in_scoped_cwd() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (registry, _) = registry_in(dir.path());

    let events = run_tool(
        &registry,
        "shell_exec",
        json!({ "command": "pwd" }),
        ComputeMode::MacRemote,
    )
    .await;
    let chunks: String = events
        .iter()
        .filter_map(|e| match e {
            Ok(ToolEvent::Chunk { data }) => Some(data.clone()),
            _ => None,
        })
        .collect();
    let canonical = dir.path().canonicalize().expect("canonicalize tempdir");
    assert!(
        chunks.contains(&canonical.display().to_string()),
        "pwd output {chunks:?} should be the scoped root {canonical:?}"
    );

    // cwd outside the root is a sandbox violation.
    let events = run_tool(
        &registry,
        "shell_exec",
        json!({ "command": "pwd", "cwd": "/" }),
        ComputeMode::MacLan,
    )
    .await;
    assert!(matches!(
        events.last(),
        Some(Err(ContractError::Tool(ToolError::SandboxViolation)))
    ));
}

// ---------------------------------------------------------------------------
// Computer use
// ---------------------------------------------------------------------------

#[tokio::test]
async fn computer_use_tools_drive_the_controller_and_log_actions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (registry, controller) = registry_in(dir.path());
    let mode = ComputeMode::MacLan;

    controller.set_elements(vec![UiElement {
        role: "button".to_string(),
        label: Some("Save".to_string()),
        bounds: UiBounds {
            x: 10,
            y: 20,
            width: 80,
            height: 24,
        },
        focused: true,
    }]);

    let events = run_tool(&registry, "screen_capture", json!({}), mode).await;
    let shot = final_result(&events);
    assert_eq!(shot["width"], 1280);
    assert_eq!(shot["height"], 800);
    assert_eq!(shot["description"], "mock screen: empty desktop");
    assert!(shot["image_hex"]
        .as_str()
        .expect("hex image")
        .starts_with("89504e47"));

    let events = run_tool(&registry, "ui_describe", json!({}), mode).await;
    let ui = final_result(&events);
    assert_eq!(ui["elements"][0]["label"], "Save");

    let events = run_tool(&registry, "ui_click", json!({ "x": 50, "y": 32 }), mode).await;
    assert_eq!(final_result(&events)["clicked"], true);

    let events = run_tool(&registry, "ui_type", json!({ "text": "hello world" }), mode).await;
    assert_eq!(final_result(&events)["typed_chars"], 11);

    let events = run_tool(&registry, "ui_scroll", json!({ "dx": 0, "dy": -120 }), mode).await;
    assert_eq!(final_result(&events)["scrolled"], true);

    let events = run_tool(&registry, "ui_key", json!({ "combo": "cmd+s" }), mode).await;
    assert_eq!(final_result(&events)["pressed"], true);

    assert_eq!(
        controller.actions(),
        vec![
            ControllerAction::Screenshot,
            ControllerAction::DescribeUi,
            ControllerAction::Click {
                x: 50,
                y: 32,
                button: MouseButton::Left
            },
            ControllerAction::TypeText {
                text: "hello world".to_string()
            },
            ControllerAction::Scroll { dx: 0, dy: -120 },
            ControllerAction::Key {
                combo: "cmd+s".to_string()
            },
        ]
    );
}

#[tokio::test]
async fn ui_click_supports_other_buttons() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (registry, controller) = registry_in(dir.path());

    run_tool(
        &registry,
        "ui_click",
        json!({ "x": 1, "y": 2, "button": "right" }),
        ComputeMode::MacRemote,
    )
    .await;
    assert_eq!(
        controller.actions(),
        vec![ControllerAction::Click {
            x: 1,
            y: 2,
            button: MouseButton::Right
        }]
    );
}

#[tokio::test]
async fn input_tools_refuse_in_sensitive_context() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (registry, controller) = registry_in(dir.path());
    controller.set_sensitive(true);
    let mode = ComputeMode::MacLan;

    for (tool, args) in [
        ("ui_click", json!({ "x": 5, "y": 5 })),
        ("ui_type", json!({ "text": "hunter2" })),
        ("ui_key", json!({ "combo": "enter" })),
    ] {
        let events = run_tool(&registry, tool, args, mode).await;
        match events.last() {
            Some(Ok(ToolEvent::Error { message, retryable })) => {
                assert!(
                    message.contains("login or payment"),
                    "{tool} refusal should explain itself: {message}"
                );
                assert!(!retryable, "{tool} refusal is not retryable");
            }
            other => panic!("{tool} must refuse in sensitive context, got {other:?}"),
        }
    }
    // No input action reached the controller.
    assert_eq!(controller.actions(), vec![]);

    // Read-only observation is still allowed.
    let events = run_tool(&registry, "screen_capture", json!({}), mode).await;
    assert_eq!(final_result(&events)["width"], 1280);
    assert_eq!(controller.actions(), vec![ControllerAction::Screenshot]);
}

// ---------------------------------------------------------------------------
// Web tools (non-net path; never touches the network)
// ---------------------------------------------------------------------------

#[cfg(not(feature = "net"))]
#[tokio::test]
async fn web_tools_report_honest_error_without_net_feature() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (registry, _) = registry_in(dir.path());

    for (tool, args) in [
        ("web_fetch", json!({ "url": "https://example.com" })),
        ("web_search", json!({ "query": "mahi ai" })),
    ] {
        let events = run_tool(&registry, tool, args, ComputeMode::Hosted).await;
        assert!(
            matches!(
                events.last(),
                Some(Ok(ToolEvent::Error { message, retryable: false })) if message.contains("net")
            ),
            "{tool} should report the missing net feature, got {events:?}"
        );
    }
}

#[tokio::test]
async fn web_fetch_validates_url_before_any_network() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (registry, _) = registry_in(dir.path());
    let events = run_tool(
        &registry,
        "web_fetch",
        json!({ "url": "file:///etc/passwd" }),
        ComputeMode::Hosted,
    )
    .await;
    assert!(matches!(
        events.last(),
        Some(Ok(ToolEvent::Error { message, .. })) if message.contains("unsupported URL")
    ));
}

// ---------------------------------------------------------------------------
// Connector broker
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mounted_connector_tools_describe_and_invoke() {
    let registry = ToolRegistry::empty();
    registry
        .mount_connector(Arc::new(EchoConnector::new()))
        .expect("mount echo");
    assert_eq!(registry.connector_ids(), vec!["echo".to_string()]);

    let tools = registry.describe(ComputeMode::Hosted).await;
    let echo = tools
        .iter()
        .find(|t| t.id == "echo.say")
        .expect("echo.say listed");
    assert_eq!(echo.category, ToolCategory::Connector);
    assert!(!echo.requires_approval);

    let events = run_tool(
        &registry,
        "echo.say",
        json!({ "message": "ping" }),
        ComputeMode::OnDevice,
    )
    .await;
    assert_eq!(final_result(&events)["echo"], "ping");
}

#[tokio::test]
async fn mounting_a_conflicting_connector_fails_atomically() {
    let registry = ToolRegistry::empty();
    registry
        .mount_connector(Arc::new(EchoConnector::new()))
        .expect("first mount ok");
    let err = registry
        .mount_connector(Arc::new(EchoConnector::new()))
        .expect_err("duplicate tool ids must fail");
    assert!(err.to_string().contains("already registered"));
    // Only the first instance is recorded.
    assert_eq!(registry.connector_ids().len(), 1);
}

#[tokio::test]
async fn custom_tools_can_be_registered_on_an_empty_registry() {
    use async_trait::async_trait;
    use mahi_contracts::tooling::ToolDescriptor;
    use mahi_tooling::Tool;

    struct PingTool;

    #[async_trait]
    impl Tool for PingTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                id: "ping".to_string(),
                display_name: "Ping".to_string(),
                category: ToolCategory::BuiltIn,
                available_in_modes: vec![ComputeMode::OnDevice],
                required_permissions: vec![],
                input_schema: json!({ "type": "object" }),
                output_schema: json!({ "type": "object" }),
                requires_approval: false,
                destructive_level: DestructiveLevel::Low,
            }
        }

        async fn run(
            &self,
            _args: serde_json::Value,
            _cancel: CancellationToken,
        ) -> ToolEventStream {
            mahi_tooling::tool::ok_result(json!({ "pong": true }))
        }
    }

    let registry = ToolRegistry::empty();
    registry
        .register(Arc::new(PingTool))
        .expect("register custom tool");
    assert!(
        registry.register(Arc::new(PingTool)).is_err(),
        "duplicate ids rejected"
    );

    let events = run_tool(&registry, "ping", json!({}), ComputeMode::OnDevice).await;
    assert_eq!(final_result(&events)["pong"], true);
}
