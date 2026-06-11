//! The platform hook for Claude-style computer use.
//!
//! The macOS app implements [`ComputerController`] with ScreenCaptureKit +
//! CGEvent and injects it through the FFI builder (see
//! `docs/backend/phase-0/03-engine-facade.md`). [`MockComputerController`]
//! ships here so tests and Linux builds work without a display server.

use async_trait::async_trait;
use mahi_contracts::error::ContractError;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// One captured frame of the screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Screenshot {
    /// Encoded image bytes (PNG for the real macOS implementation).
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Text description placeholder. Later filled by OCR / a vision model so
    /// non-vision compute modes can still reason about the screen.
    pub description: String,
}

/// Pixel-space bounding box of a UI element.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiBounds {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// One UI element discovered via accessibility / vision inspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiElement {
    /// Accessibility role, e.g. "button", "text_field", "window".
    pub role: String,
    /// Human-readable label/title, when one exists.
    pub label: Option<String>,
    pub bounds: UiBounds,
    /// Whether this element currently has keyboard focus.
    pub focused: bool,
}

/// Mouse button for click actions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

/// Platform hook the host app implements (screenshot, click, type, ...).
///
/// All methods are async because the real implementation crosses an FFI/IPC
/// boundary into the Swift app. Implementations must be cheap to call from
/// multiple tools concurrently.
#[async_trait]
pub trait ComputerController: Send + Sync {
    /// Capture the current screen.
    async fn screenshot(&self) -> Result<Screenshot, ContractError>;

    /// Enumerate the visible UI elements (accessibility tree / vision).
    async fn describe_ui(&self) -> Result<Vec<UiElement>, ContractError>;

    /// Click at absolute pixel coordinates with the given button.
    async fn click(&self, x: i32, y: i32, button: MouseButton) -> Result<(), ContractError>;

    /// Move the pointer to absolute pixel coordinates without clicking.
    async fn move_mouse(&self, x: i32, y: i32) -> Result<(), ContractError>;

    /// Type literal text into the focused element.
    async fn type_text(&self, text: &str) -> Result<(), ContractError>;

    /// Scroll by the given deltas (positive = right/down).
    async fn scroll(&self, dx: i32, dy: i32) -> Result<(), ContractError>;

    /// Press a key combo, e.g. "cmd+s", "enter", "ctrl+shift+t".
    async fn key(&self, combo: &str) -> Result<(), ContractError>;

    /// Whether the focused context looks like a login/payment surface
    /// (URL patterns, AX roles like secure text fields, keyword list).
    /// Input tools (`ui_click`, `ui_type`, `ui_key`) refuse to act when this
    /// returns true — the approval wall must not be bypassable here.
    async fn is_sensitive_context(&self) -> bool;
}

/// One recorded action on the [`MockComputerController`], for test assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControllerAction {
    Screenshot,
    DescribeUi,
    Click { x: i32, y: i32, button: MouseButton },
    MoveMouse { x: i32, y: i32 },
    TypeText { text: String },
    Scroll { dx: i32, dy: i32 },
    Key { combo: String },
}

/// Test/Linux implementation: returns a canned screenshot and logs every
/// action into an inspectable list.
#[derive(Debug, Default)]
pub struct MockComputerController {
    actions: Mutex<Vec<ControllerAction>>,
    sensitive: AtomicBool,
    elements: Mutex<Vec<UiElement>>,
}

impl MockComputerController {
    pub fn new() -> Self {
        Self::default()
    }

    /// Make `is_sensitive_context()` report a login/payment-like screen.
    pub fn set_sensitive(&self, sensitive: bool) {
        self.sensitive.store(sensitive, Ordering::SeqCst);
    }

    /// Replace the canned UI tree returned by `describe_ui()`.
    pub fn set_elements(&self, elements: Vec<UiElement>) {
        *self.elements.lock().expect("mock elements lock poisoned") = elements;
    }

    /// Snapshot of every action performed so far, in order.
    pub fn actions(&self) -> Vec<ControllerAction> {
        self.actions.lock().expect("mock action lock poisoned").clone()
    }

    fn log(&self, action: ControllerAction) {
        self.actions.lock().expect("mock action lock poisoned").push(action);
    }
}

#[async_trait]
impl ComputerController for MockComputerController {
    async fn screenshot(&self) -> Result<Screenshot, ContractError> {
        self.log(ControllerAction::Screenshot);
        Ok(Screenshot {
            // Minimal valid PNG signature so consumers that sniff magic
            // bytes behave sensibly; not a decodable image.
            bytes: vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            width: 1280,
            height: 800,
            description: "mock screen: empty desktop".to_string(),
        })
    }

    async fn describe_ui(&self) -> Result<Vec<UiElement>, ContractError> {
        self.log(ControllerAction::DescribeUi);
        Ok(self.elements.lock().expect("mock elements lock poisoned").clone())
    }

    async fn click(&self, x: i32, y: i32, button: MouseButton) -> Result<(), ContractError> {
        self.log(ControllerAction::Click { x, y, button });
        Ok(())
    }

    async fn move_mouse(&self, x: i32, y: i32) -> Result<(), ContractError> {
        self.log(ControllerAction::MoveMouse { x, y });
        Ok(())
    }

    async fn type_text(&self, text: &str) -> Result<(), ContractError> {
        self.log(ControllerAction::TypeText { text: text.to_string() });
        Ok(())
    }

    async fn scroll(&self, dx: i32, dy: i32) -> Result<(), ContractError> {
        self.log(ControllerAction::Scroll { dx, dy });
        Ok(())
    }

    async fn key(&self, combo: &str) -> Result<(), ContractError> {
        self.log(ControllerAction::Key { combo: combo.to_string() });
        Ok(())
    }

    async fn is_sensitive_context(&self) -> bool {
        self.sensitive.load(Ordering::SeqCst)
    }
}
