use serde::{Deserialize, Serialize};
use url::Url;
use zvariant::Type;

pub const APP_ID: &str = "io.github.ashot.App";
pub const DBUS_NAME: &str = "io.github.ashot.Service";
pub const DBUS_PATH: &str = "/io/github/ashot/App";
pub const DBUS_INTERFACE: &str = "io.github.ashot.App";
pub const SHELL_DBUS_NAME: &str = "io.github.ashot.Shell";
pub const SHELL_DBUS_PATH: &str = "/io/github/ashot/Shell";
pub const SHELL_DBUS_INTERFACE: &str = "io.github.ashot.Shell";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Type)]
pub enum CaptureMode {
    Area,
    Screen,
    Window,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Type)]
pub enum OutcomeKind {
    Ok,
    Cancelled,
    Busy,
    Unsupported,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Type)]
pub struct CaptureOutcome {
    pub kind: OutcomeKind,
    pub message: String,
    pub file_uri: String,
}

impl CaptureOutcome {
    pub fn ok(file_uri: Url, message: impl Into<String>) -> Self {
        Self { kind: OutcomeKind::Ok, message: message.into(), file_uri: file_uri.to_string() }
    }

    pub fn status(kind: OutcomeKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), file_uri: String::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Type)]
pub struct CommandOutcome {
    pub kind: OutcomeKind,
    pub message: String,
}

impl CommandOutcome {
    pub fn ok(message: impl Into<String>) -> Self {
        Self { kind: OutcomeKind::Ok, message: message.into() }
    }

    pub fn status(kind: OutcomeKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
}

#[zbus::proxy(
    interface = "io.github.ashot.App",
    default_service = "io.github.ashot.Service",
    default_path = "/io/github/ashot/App"
)]
pub trait Ashot {
    #[zbus(name = "CaptureArea")]
    async fn capture_area(&self) -> zbus::Result<CaptureOutcome>;

    #[zbus(name = "CaptureScreen")]
    async fn capture_screen(&self) -> zbus::Result<CaptureOutcome>;

    #[zbus(name = "CaptureWindow")]
    async fn capture_window(&self) -> zbus::Result<CaptureOutcome>;

    #[zbus(name = "OpenSettings")]
    async fn open_settings(&self) -> zbus::Result<CommandOutcome>;

    #[zbus(name = "OpenEditor")]
    async fn open_editor(&self, file_uri: &str) -> zbus::Result<CommandOutcome>;

    #[zbus(name = "PinImage")]
    async fn pin_image(&self, file_uri: &str) -> zbus::Result<CommandOutcome>;

    #[zbus(name = "FinalizeCapture")]
    async fn finalize_capture(
        &self,
        source_file_uri: &str,
        annotations_json: &str,
    ) -> zbus::Result<CaptureOutcome>;
}

#[zbus::proxy(
    interface = "io.github.ashot.Shell",
    default_service = "io.github.ashot.Shell",
    default_path = "/io/github/ashot/Shell"
)]
pub trait AshotShell {
    #[zbus(name = "StartCapture")]
    async fn start_capture(&self) -> zbus::Result<()>;
}
