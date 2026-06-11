//! Claude-style computer-use tools: `screen_capture`, `ui_describe`,
//! `ui_click`, `ui_type`, `ui_scroll`, `ui_key`.
//!
//! Every tool delegates to the injected [`ComputerController`]; nothing here
//! touches the platform directly. Input tools (`ui_click`, `ui_type`,
//! `ui_key`) are approval-gated AND refuse outright when the controller
//! reports a sensitive (login/payment) focused context — the approval wall
//! must not be bypassable for credential surfaces.

use crate::computer::{ComputerController, MouseButton};
use crate::tool::{events, ok_result, parse_args, tool_error, Tool};
use async_trait::async_trait;
use mahi_contracts::error::ContractError;
use mahi_contracts::tooling::{DestructiveLevel, ToolCategory, ToolDescriptor, ToolEvent, ToolEventStream};
use mahi_contracts::types::ComputeMode;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Computer use requires a Mac executing the tools (mode matrix B/C).
const COMPUTER_MODES: [ComputeMode; 2] = [ComputeMode::MacLan, ComputeMode::MacRemote];

const SENSITIVE_REFUSAL: &str = "refused: the focused context appears to be a login or payment \
     screen; automated input is blocked and the user must act directly";

fn observe_permissions() -> Vec<String> {
    vec!["computer.observe".to_string()]
}

fn input_permissions() -> Vec<String> {
    vec!["computer.input".to_string()]
}

/// Hex-encode bytes for JSON transport.
// TODO(contracts): ToolEvent has no binary/artifact payload variant; image
// bytes are hex-encoded into the Result JSON until an artifact store seam
// exists in the contracts.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

async fn refuse_if_sensitive(controller: &Arc<dyn ComputerController>) -> Option<ToolEventStream> {
    if controller.is_sensitive_context().await {
        Some(tool_error(SENSITIVE_REFUSAL, false))
    } else {
        None
    }
}

fn controller_failure(action: &str, err: ContractError) -> ToolEventStream {
    events(vec![Ok(ToolEvent::Error {
        message: format!("computer controller failed to {action}: {err}"),
        retryable: true,
    })])
}

// ---------------------------------------------------------------------------
// screen_capture (read-only)
// ---------------------------------------------------------------------------

pub struct ScreenCaptureTool {
    controller: Arc<dyn ComputerController>,
}

impl ScreenCaptureTool {
    pub(crate) fn new(controller: Arc<dyn ComputerController>) -> Self {
        Self { controller }
    }
}

#[async_trait]
impl Tool for ScreenCaptureTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            id: "screen_capture".to_string(),
            display_name: "Capture Screen".to_string(),
            category: ToolCategory::ComputerUse,
            available_in_modes: COMPUTER_MODES.to_vec(),
            required_permissions: observe_permissions(),
            input_schema: json!({ "type": "object", "properties": {} }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "width": { "type": "integer" },
                    "height": { "type": "integer" },
                    "description": { "type": "string" },
                    "image_hex": { "type": "string" }
                }
            }),
            requires_approval: false,
            destructive_level: DestructiveLevel::Low,
        }
    }

    async fn run(&self, _args: serde_json::Value, _cancel: CancellationToken) -> ToolEventStream {
        match self.controller.screenshot().await {
            Ok(shot) => ok_result(json!({
                "width": shot.width,
                "height": shot.height,
                "description": shot.description,
                "image_hex": hex(&shot.bytes),
            })),
            Err(e) => controller_failure("capture the screen", e),
        }
    }
}

// ---------------------------------------------------------------------------
// ui_describe (read-only)
// ---------------------------------------------------------------------------

pub struct UiDescribeTool {
    controller: Arc<dyn ComputerController>,
}

impl UiDescribeTool {
    pub(crate) fn new(controller: Arc<dyn ComputerController>) -> Self {
        Self { controller }
    }
}

#[async_trait]
impl Tool for UiDescribeTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            id: "ui_describe".to_string(),
            display_name: "Describe UI".to_string(),
            category: ToolCategory::ComputerUse,
            available_in_modes: COMPUTER_MODES.to_vec(),
            required_permissions: observe_permissions(),
            input_schema: json!({ "type": "object", "properties": {} }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "elements": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "role": { "type": "string" },
                                "label": { "type": ["string", "null"] },
                                "bounds": { "type": "object" },
                                "focused": { "type": "boolean" }
                            }
                        }
                    }
                }
            }),
            requires_approval: false,
            destructive_level: DestructiveLevel::Low,
        }
    }

    async fn run(&self, _args: serde_json::Value, _cancel: CancellationToken) -> ToolEventStream {
        match self.controller.describe_ui().await {
            Ok(elements) => match serde_json::to_value(&elements) {
                Ok(value) => ok_result(json!({ "elements": value })),
                Err(e) => tool_error(format!("failed to serialize UI elements: {e}"), false),
            },
            Err(e) => controller_failure("describe the UI", e),
        }
    }
}

// ---------------------------------------------------------------------------
// ui_click (input; approval + sensitive-context refusal)
// ---------------------------------------------------------------------------

pub struct UiClickTool {
    controller: Arc<dyn ComputerController>,
}

impl UiClickTool {
    pub(crate) fn new(controller: Arc<dyn ComputerController>) -> Self {
        Self { controller }
    }
}

#[derive(Deserialize)]
struct UiClickArgs {
    x: i32,
    y: i32,
    #[serde(default)]
    button: MouseButton,
}

#[async_trait]
impl Tool for UiClickTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            id: "ui_click".to_string(),
            display_name: "Click".to_string(),
            category: ToolCategory::ComputerUse,
            available_in_modes: COMPUTER_MODES.to_vec(),
            required_permissions: input_permissions(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "x": { "type": "integer" },
                    "y": { "type": "integer" },
                    "button": { "type": "string", "enum": ["left", "right", "middle"], "default": "left" }
                },
                "required": ["x", "y"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": { "clicked": { "type": "boolean" } }
            }),
            requires_approval: true,
            destructive_level: DestructiveLevel::High,
        }
    }

    async fn run(&self, args: serde_json::Value, _cancel: CancellationToken) -> ToolEventStream {
        let args: UiClickArgs = match parse_args(args) {
            Ok(a) => a,
            Err(msg) => return tool_error(msg, false),
        };
        if let Some(refusal) = refuse_if_sensitive(&self.controller).await {
            return refusal;
        }
        match self.controller.click(args.x, args.y, args.button).await {
            Ok(()) => ok_result(json!({
                "clicked": true,
                "x": args.x,
                "y": args.y,
                "button": serde_json::to_value(args.button).unwrap_or(json!("left")),
            })),
            Err(e) => controller_failure("click", e),
        }
    }
}

// ---------------------------------------------------------------------------
// ui_type (input; approval + sensitive-context refusal)
// ---------------------------------------------------------------------------

pub struct UiTypeTool {
    controller: Arc<dyn ComputerController>,
}

impl UiTypeTool {
    pub(crate) fn new(controller: Arc<dyn ComputerController>) -> Self {
        Self { controller }
    }
}

#[derive(Deserialize)]
struct UiTypeArgs {
    text: String,
}

#[async_trait]
impl Tool for UiTypeTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            id: "ui_type".to_string(),
            display_name: "Type Text".to_string(),
            category: ToolCategory::ComputerUse,
            available_in_modes: COMPUTER_MODES.to_vec(),
            required_permissions: input_permissions(),
            input_schema: json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": { "typed_chars": { "type": "integer" } }
            }),
            requires_approval: true,
            destructive_level: DestructiveLevel::High,
        }
    }

    async fn run(&self, args: serde_json::Value, _cancel: CancellationToken) -> ToolEventStream {
        let args: UiTypeArgs = match parse_args(args) {
            Ok(a) => a,
            Err(msg) => return tool_error(msg, false),
        };
        if let Some(refusal) = refuse_if_sensitive(&self.controller).await {
            return refusal;
        }
        let typed_chars = args.text.chars().count();
        match self.controller.type_text(&args.text).await {
            Ok(()) => ok_result(json!({ "typed_chars": typed_chars })),
            Err(e) => controller_failure("type text", e),
        }
    }
}

// ---------------------------------------------------------------------------
// ui_scroll (no approval; cannot alter state destructively)
// ---------------------------------------------------------------------------

pub struct UiScrollTool {
    controller: Arc<dyn ComputerController>,
}

impl UiScrollTool {
    pub(crate) fn new(controller: Arc<dyn ComputerController>) -> Self {
        Self { controller }
    }
}

#[derive(Deserialize)]
struct UiScrollArgs {
    dx: i32,
    dy: i32,
}

#[async_trait]
impl Tool for UiScrollTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            id: "ui_scroll".to_string(),
            display_name: "Scroll".to_string(),
            category: ToolCategory::ComputerUse,
            available_in_modes: COMPUTER_MODES.to_vec(),
            required_permissions: input_permissions(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "dx": { "type": "integer", "description": "Horizontal delta (positive = right)" },
                    "dy": { "type": "integer", "description": "Vertical delta (positive = down)" }
                },
                "required": ["dx", "dy"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": { "scrolled": { "type": "boolean" } }
            }),
            requires_approval: false,
            destructive_level: DestructiveLevel::Low,
        }
    }

    async fn run(&self, args: serde_json::Value, _cancel: CancellationToken) -> ToolEventStream {
        let args: UiScrollArgs = match parse_args(args) {
            Ok(a) => a,
            Err(msg) => return tool_error(msg, false),
        };
        match self.controller.scroll(args.dx, args.dy).await {
            Ok(()) => ok_result(json!({ "scrolled": true, "dx": args.dx, "dy": args.dy })),
            Err(e) => controller_failure("scroll", e),
        }
    }
}

// ---------------------------------------------------------------------------
// ui_key (input; approval + sensitive-context refusal)
// ---------------------------------------------------------------------------

pub struct UiKeyTool {
    controller: Arc<dyn ComputerController>,
}

impl UiKeyTool {
    pub(crate) fn new(controller: Arc<dyn ComputerController>) -> Self {
        Self { controller }
    }
}

#[derive(Deserialize)]
struct UiKeyArgs {
    /// Key combo, e.g. "cmd+s", "enter", "ctrl+shift+t".
    combo: String,
}

#[async_trait]
impl Tool for UiKeyTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            id: "ui_key".to_string(),
            display_name: "Press Keys".to_string(),
            category: ToolCategory::ComputerUse,
            available_in_modes: COMPUTER_MODES.to_vec(),
            required_permissions: input_permissions(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "combo": { "type": "string", "description": "Key combo like cmd+s, enter, ctrl+shift+t" }
                },
                "required": ["combo"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": { "pressed": { "type": "boolean" } }
            }),
            requires_approval: true,
            destructive_level: DestructiveLevel::High,
        }
    }

    async fn run(&self, args: serde_json::Value, _cancel: CancellationToken) -> ToolEventStream {
        let args: UiKeyArgs = match parse_args(args) {
            Ok(a) => a,
            Err(msg) => return tool_error(msg, false),
        };
        if args.combo.trim().is_empty() {
            return tool_error("combo must not be empty", false);
        }
        if let Some(refusal) = refuse_if_sensitive(&self.controller).await {
            return refusal;
        }
        match self.controller.key(&args.combo).await {
            Ok(()) => ok_result(json!({ "pressed": true, "combo": args.combo })),
            Err(e) => controller_failure("press keys", e),
        }
    }
}
