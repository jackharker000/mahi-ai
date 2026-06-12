//! End-to-end MCP client test against a real child-process server.
//!
//! Spawns a tiny Python MCP server speaking newline-delimited JSON-RPC over
//! stdio and drives the full path: handshake, `tools/list`, `tools/call`, and
//! registry dispatch through an [`McpToolAdapter`]. Skips cleanly when
//! `python3` is unavailable.

use std::sync::Arc;

use mahi_contracts::tooling::{ToolEvent, ToolInvocation, ToolInvokeContract};
use mahi_contracts::types::ComputeMode;
use mahi_tooling::mcp::McpClient;
use mahi_tooling::McpServerConfig;
use mahi_tooling::ToolRegistry;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// A minimal MCP server: one `echo` tool that returns `echo: <text>`.
const FAKE_SERVER: &str = r#"
import sys, json
def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fake","version":"0"}}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"tools":[
            {"name":"echo","description":"Echo text back","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}
        ]}})
    elif method == "tools/call":
        args = (msg.get("params") or {}).get("arguments") or {}
        if args.get("fail"):
            send({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"text","text":"boom"}],"isError":True}})
        else:
            send({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"text","text":"echo: "+str(args.get("text",""))}],"isError":False}})
    elif mid is not None:
        send({"jsonrpc":"2.0","id":mid,"error":{"code":-32601,"message":"method not found"}})
"#;

fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn fake_config() -> McpServerConfig {
    McpServerConfig::new("fake", "python3").with_args(["-c", FAKE_SERVER])
}

#[tokio::test]
async fn connect_lists_and_calls_a_real_mcp_server() {
    if !python3_available() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let client = McpClient::connect(&fake_config())
        .await
        .expect("connect to fake MCP server");

    let tools = client.list_tools();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    assert_eq!(tools[0].input_schema["required"][0], "text");

    let out = client
        .call_tool("echo", serde_json::json!({ "text": "hello" }))
        .await
        .expect("echo call");
    assert_eq!(out, "echo: hello");

    // isError:true surfaces as a ToolFailed error carrying the text content.
    let err = client
        .call_tool("echo", serde_json::json!({ "fail": true }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("boom"), "got: {err}");

    client.shutdown().await;
}

#[tokio::test]
async fn registry_dispatches_namespaced_mcp_tool() {
    if !python3_available() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let client = Arc::new(
        McpClient::connect(&fake_config())
            .await
            .expect("connect to fake MCP server"),
    );

    let registry = ToolRegistry::empty();
    let ids = registry
        .attach_mcp_server(Arc::clone(&client))
        .expect("attach mcp server");
    assert_eq!(ids, vec!["mcp__fake__echo".to_string()]);

    // The tool is offered in MacLan (mirrors shell_exec) and is approval-gated.
    let described = registry.describe(ComputeMode::MacLan).await;
    let echo = described
        .iter()
        .find(|d| d.id == "mcp__fake__echo")
        .expect("namespaced tool described");
    assert!(echo.requires_approval);
    // ...and absent on the phone's OnDevice mode.
    assert!(registry
        .describe(ComputeMode::OnDevice)
        .await
        .iter()
        .all(|d| d.id != "mcp__fake__echo"));

    let call = ToolInvocation {
        invocation_id: Uuid::new_v4(),
        tool_id: "mcp__fake__echo".to_string(),
        args: serde_json::json!({ "text": "world" }),
        session_id: Uuid::new_v4(),
        conversation_id: Uuid::new_v4(),
        message_id: Uuid::new_v4(),
        compute_mode: ComputeMode::MacLan,
        trace_id: Uuid::new_v4(),
        stream: false,
    };
    let mut stream = registry
        .invoke(call, CancellationToken::new())
        .await
        .expect("invoke namespaced tool");

    use futures::StreamExt;
    let mut got_result = false;
    while let Some(event) = stream.next().await {
        if let Ok(ToolEvent::Result { output, .. }) = event {
            assert_eq!(output["content"], "echo: world");
            got_result = true;
        }
    }
    assert!(got_result, "expected a Result event from the MCP tool");
}
