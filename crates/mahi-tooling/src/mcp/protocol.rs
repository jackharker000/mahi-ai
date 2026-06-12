//! Minimal JSON-RPC 2.0 + MCP wire types, hand-rolled with serde_json.
//!
//! Only what the stdio client needs: `initialize`,
//! `notifications/initialized`, `tools/list`, and `tools/call` against
//! protocol version "2024-11-05". One JSON-RPC message per line
//! (newline-delimited JSON), as specified by the MCP stdio transport.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};

/// JSON-RPC version string carried by every message.
pub const JSONRPC_VERSION: &str = "2.0";

/// The MCP protocol revision this client speaks.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 envelope
// ---------------------------------------------------------------------------

/// A JSON-RPC request (carries an id; expects a [`Response`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Request {
    pub fn new(id: u64, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            method: method.into(),
            params,
        }
    }
}

/// A JSON-RPC notification (no id; no response expected).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Notification {
    pub fn new(method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.into(),
            params,
        }
    }
}

/// A JSON-RPC response: exactly one of `result` / `error` is set.
#[derive(Debug, Clone, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

/// The JSON-RPC error object.
#[derive(Debug, Clone, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

// ---------------------------------------------------------------------------
// MCP payloads
// ---------------------------------------------------------------------------

/// Params for `initialize`.
pub fn initialize_params() -> Value {
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {
            "name": "mahi",
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

/// One tool advertised by a server in a `tools/list` result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "inputSchema", default = "default_input_schema")]
    pub input_schema: Value,
}

fn default_input_schema() -> Value {
    json!({ "type": "object" })
}

/// Result shape of `tools/list`.
#[derive(Debug, Clone, Deserialize)]
pub struct ListToolsResult {
    #[serde(default)]
    pub tools: Vec<ToolInfo>,
}

/// Result shape of `tools/call`.
#[derive(Debug, Clone, Deserialize)]
pub struct CallToolResult {
    #[serde(default)]
    pub content: Vec<ContentItem>,
    #[serde(rename = "isError", default)]
    pub is_error: bool,
}

/// One content item in a `tools/call` result. Text is kept verbatim; any
/// other content type (image, audio, resource, ...) is preserved only as its
/// type tag so [`render_content`] can note it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentItem {
    Text { text: String },
    Other { kind: String },
}

impl<'de> Deserialize<'de> for ContentItem {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if kind == "text" {
            if let Some(text) = value.get("text").and_then(Value::as_str) {
                return Ok(ContentItem::Text {
                    text: text.to_string(),
                });
            }
        }
        Ok(ContentItem::Other {
            kind: kind.to_string(),
        })
    }
}

/// Flatten content items into one result string: text items verbatim,
/// non-text items noted as e.g. `[image content]`, joined by newlines.
pub fn render_content(items: &[ContentItem]) -> String {
    items
        .iter()
        .map(|item| match item {
            ContentItem::Text { text } => text.clone(),
            ContentItem::Other { kind } => format!("[{kind} content]"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_serializes_without_null_params() {
        let req = Request::new(7, "tools/list", None);
        let wire = serde_json::to_value(&req).unwrap();
        assert_eq!(
            wire,
            json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/list" })
        );

        let req = Request::new(8, "tools/call", Some(json!({ "name": "echo" })));
        let wire = serde_json::to_value(&req).unwrap();
        assert_eq!(wire["params"]["name"], "echo");
        assert_eq!(wire["jsonrpc"], "2.0");
    }

    #[test]
    fn notification_serializes_method_only() {
        let note = Notification::new("notifications/initialized", None);
        let wire = serde_json::to_value(&note).unwrap();
        assert_eq!(
            wire,
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
        );
        assert!(wire.get("id").is_none());
    }

    #[test]
    fn response_parses_result_and_error_forms() {
        let ok: Response =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#).unwrap();
        assert_eq!(ok.id, 1);
        assert!(ok.result.is_some() && ok.error.is_none());

        let err: Response = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"method not found"}}"#,
        )
        .unwrap();
        assert_eq!(err.id, 2);
        let rpc = err.error.expect("error object");
        assert_eq!(rpc.code, -32601);
        assert_eq!(rpc.message, "method not found");
        assert!(rpc.data.is_none());
    }

    #[test]
    fn initialize_params_have_required_shape() {
        let params = initialize_params();
        assert_eq!(params["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert_eq!(params["clientInfo"]["name"], "mahi");
        assert!(params["capabilities"].is_object());
    }

    #[test]
    fn tool_info_parses_input_schema_rename_and_default() {
        let tool: ToolInfo = serde_json::from_value(json!({
            "name": "echo",
            "description": "Echo a message",
            "inputSchema": { "type": "object", "required": ["message"] }
        }))
        .unwrap();
        assert_eq!(tool.name, "echo");
        assert_eq!(tool.input_schema["required"][0], "message");

        // Schema and description are optional on the wire.
        let bare: ToolInfo = serde_json::from_value(json!({ "name": "bare" })).unwrap();
        assert!(bare.description.is_none());
        assert_eq!(bare.input_schema, json!({ "type": "object" }));
    }

    #[test]
    fn call_result_parses_and_renders_mixed_content() {
        let result: CallToolResult = serde_json::from_value(json!({
            "content": [
                { "type": "text", "text": "hello" },
                { "type": "image", "data": "AAAA", "mimeType": "image/png" },
                { "type": "text", "text": "world" }
            ]
        }))
        .unwrap();
        assert!(!result.is_error);
        assert_eq!(
            render_content(&result.content),
            "hello\n[image content]\nworld"
        );

        let failed: CallToolResult = serde_json::from_value(json!({
            "content": [{ "type": "text", "text": "boom" }],
            "isError": true
        }))
        .unwrap();
        assert!(failed.is_error);

        // Empty/missing content renders as an empty string.
        let empty: CallToolResult = serde_json::from_value(json!({})).unwrap();
        assert_eq!(render_content(&empty.content), "");
    }

    #[test]
    fn unknown_content_items_keep_their_type_tag() {
        let items: Vec<ContentItem> = serde_json::from_value(json!([
            { "type": "resource", "resource": { "uri": "file:///x" } },
            { "no_type_at_all": true }
        ]))
        .unwrap();
        assert_eq!(
            render_content(&items),
            "[resource content]\n[unknown content]"
        );
    }
}
