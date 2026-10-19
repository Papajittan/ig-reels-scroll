use crate::layout::LogicalRect;

pub mod kde_wayland;

/// An opaque identifier owned by a platform backend.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct WindowHandle(String);

impl WindowHandle {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlatformWindow {
    pub handle: WindowHandle,
    pub app_id: Option<String>,
    pub title: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayoutPass {
    Restore,
    Place,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerButton {
    Left,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ButtonState {
    Press,
    Release,
}

/// Capabilities used by the engine and optional audio layer.
/// Geometry and pointer positions are compositor logical pixels; scroll
/// distances are logical pixels as a floating-point amount.
#[allow(async_fn_in_trait)]
pub trait Platform: Send + Sync {
    async fn list_windows(&self) -> Result<Vec<PlatformWindow>, String>;
    async fn window_geometry(&self, window: &WindowHandle) -> Result<Option<LogicalRect>, String>;
    async fn activate_window(&self, window: &WindowHandle) -> Result<(), String>;
    async fn arrange_windows(
        &self,
        windows: &[WindowHandle],
        layout: crate::layout::GridSpec,
        pass: LayoutPass,
    ) -> Result<(), String>;
    async fn move_pointer(&self, x: i32, y: i32) -> Result<(), String>;
    async fn scroll(&self, horizontal: f64, vertical: f64) -> Result<(), String>;
    async fn pointer_button(&self, button: PointerButton, state: ButtonState)
        -> Result<(), String>;
}
