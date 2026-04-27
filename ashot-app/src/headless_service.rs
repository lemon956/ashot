use std::{
    fs,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use ashot_capture::{CaptureClient, CaptureError};
use ashot_core::{Annotation, AppConfig, finalize_capture_with_config};
use ashot_ipc::{CaptureMode, CaptureOutcome, CommandOutcome, DBUS_NAME, DBUS_PATH, OutcomeKind};
use tokio::{runtime::Builder, sync::Mutex as AsyncMutex};
use tracing::error;
use tracing_subscriber::{EnvFilter, fmt};
use url::Url;
use zbus::connection::Builder as ConnectionBuilder;

pub fn run() -> Result<()> {
    let _ = fmt().with_env_filter(EnvFilter::from_env("ASHOT_LOG")).with_target(false).try_init();

    let service_only = std::env::args().any(|arg| arg == "--service");
    if !service_only {
        eprintln!(
            "ashot-app was built without GTK support. Rebuild with `--features gtk-ui` for the full editor, or run `ashot-app --service` for background capture service mode."
        );
        return Ok(());
    }

    let runtime = Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to create tokio runtime")?;
    runtime.block_on(async {
        let config = AppConfig::load_or_create().unwrap_or_default();
        let state = Arc::new(HeadlessState::new(config));
        if let Err(error) = register_service(state).await {
            error!("failed to register headless DBus service: {error:#}");
            return Err(error);
        }
        Ok(())
    })
}

struct HeadlessState {
    config: Mutex<AppConfig>,
    capture_lock: AsyncMutex<()>,
}

impl HeadlessState {
    fn new(config: AppConfig) -> Self {
        Self { config: Mutex::new(config), capture_lock: AsyncMutex::new(()) }
    }

    fn config_snapshot(&self) -> AppConfig {
        self.config.lock().expect("config lock poisoned").clone()
    }

    async fn capture_mode(&self, mode: CaptureMode) -> CaptureOutcome {
        let Ok(_guard) = self.capture_lock.try_lock() else {
            return CaptureOutcome::status(OutcomeKind::Busy, "a capture is already in progress");
        };

        let client = match CaptureClient::new().await {
            Ok(client) => client,
            Err(error) => {
                return CaptureOutcome::status(
                    OutcomeKind::Unsupported,
                    format!("failed to connect to screenshot portal: {error}"),
                );
            }
        };

        match client.capture(mode, None).await {
            Ok(uri) => {
                let config = self.config_snapshot();
                let mut message = capture_message(mode).to_string();
                if mode == CaptureMode::Area || config.post_capture_open_editor {
                    message.push_str(
                        "; editor window requires building ashot-app with the `gtk-ui` feature",
                    );
                }
                CaptureOutcome::ok(uri, message)
            }
            Err(CaptureError::Cancelled) => {
                CaptureOutcome::status(OutcomeKind::Cancelled, "capture cancelled")
            }
            Err(error @ CaptureError::Portal(_)) => CaptureOutcome::status(
                OutcomeKind::Unsupported,
                format!("failed to connect to screenshot portal: {error}"),
            ),
            Err(error) => {
                CaptureOutcome::status(OutcomeKind::Failed, format!("capture failed: {error}"))
            }
        }
    }

    fn unsupported(&self, action: &str) -> CommandOutcome {
        CommandOutcome::status(
            OutcomeKind::Unsupported,
            format!("{action} requires building ashot-app with the `gtk-ui` feature"),
        )
    }

    fn finalize_capture(&self, source_file_uri: &str, annotations_json: &str) -> CaptureOutcome {
        let Some(source_path) = parse_file_uri(source_file_uri) else {
            return CaptureOutcome::status(OutcomeKind::Failed, "invalid source file URI");
        };

        let annotations = match serde_json::from_str::<Vec<Annotation>>(annotations_json) {
            Ok(annotations) => annotations,
            Err(error) => {
                return CaptureOutcome::status(
                    OutcomeKind::Failed,
                    format!("invalid annotation payload: {error}"),
                );
            }
        };

        let config = self.config_snapshot();
        match finalize_capture_with_config(
            &config,
            &source_path,
            &annotations,
            chrono::Local::now(),
        ) {
            Ok(output_path) => match Url::from_file_path(&output_path) {
                Ok(file_uri) => {
                    if source_path != output_path {
                        let _ = fs::remove_file(&source_path);
                    }
                    let mut message =
                        format!("saved annotated screenshot to {}", output_path.display());
                    if config.auto_copy {
                        message.push_str("; clipboard copy is unavailable in headless mode");
                    }
                    if config.pin_after_save {
                        message.push_str("; pin window is unavailable in headless mode");
                    }
                    CaptureOutcome::ok(file_uri, message)
                }
                Err(_) => CaptureOutcome::status(
                    OutcomeKind::Failed,
                    format!(
                        "saved annotated screenshot, but failed to convert path to file URI: {}",
                        output_path.display()
                    ),
                ),
            },
            Err(error) => CaptureOutcome::status(
                OutcomeKind::Failed,
                format!("failed to finalize annotated capture: {error}"),
            ),
        }
    }
}

fn capture_message(mode: CaptureMode) -> &'static str {
    match mode {
        CaptureMode::Area => "region capture completed",
        CaptureMode::Screen => "screen capture completed",
        CaptureMode::Window => "window capture completed",
    }
}

async fn register_service(state: Arc<HeadlessState>) -> Result<()> {
    let service = HeadlessDbusService { state };
    let _connection = ConnectionBuilder::session()?
        .name(DBUS_NAME)?
        .serve_at(DBUS_PATH, service)?
        .build()
        .await?;
    std::future::pending::<()>().await;
    Ok(())
}

fn parse_file_uri(file_uri: &str) -> Option<std::path::PathBuf> {
    Url::parse(file_uri).ok().and_then(|uri| uri.to_file_path().ok())
}

struct HeadlessDbusService {
    state: Arc<HeadlessState>,
}

#[zbus::interface(name = "io.github.ashot.App")]
impl HeadlessDbusService {
    #[zbus(name = "CaptureArea")]
    async fn capture_area(&self) -> CaptureOutcome {
        self.state.capture_mode(CaptureMode::Area).await
    }

    #[zbus(name = "CaptureScreen")]
    async fn capture_screen(&self) -> CaptureOutcome {
        self.state.capture_mode(CaptureMode::Screen).await
    }

    #[zbus(name = "CaptureWindow")]
    async fn capture_window(&self) -> CaptureOutcome {
        self.state.capture_mode(CaptureMode::Window).await
    }

    #[zbus(name = "OpenSettings")]
    async fn open_settings(&self) -> CommandOutcome {
        self.state.unsupported("open-settings")
    }

    #[zbus(name = "OpenEditor")]
    async fn open_editor(&self, _file_uri: &str) -> CommandOutcome {
        self.state.unsupported("open-editor")
    }

    #[zbus(name = "PinImage")]
    async fn pin_image(&self, _file_uri: &str) -> CommandOutcome {
        self.state.unsupported("pin-image")
    }

    #[zbus(name = "FinalizeCapture")]
    async fn finalize_capture(
        &self,
        source_file_uri: &str,
        annotations_json: &str,
    ) -> CaptureOutcome {
        self.state.finalize_capture(source_file_uri, annotations_json)
    }
}
