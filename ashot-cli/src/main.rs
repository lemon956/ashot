use std::{env, path::PathBuf, process::ExitCode, time::Duration};

use anyhow::{Context, Result};
use ashot_ipc::{
    AshotProxy, AshotShellProxy, CaptureMode, CommandOutcome, DBUS_NAME, OutcomeKind,
    SHELL_DBUS_NAME,
};
use clap::{Parser, Subcommand, ValueEnum};
use tokio::{process::Command, time::sleep};
use tracing_subscriber::{EnvFilter, fmt};
use url::Url;
use zbus::{Connection, fdo::DBusProxy};

#[derive(Debug, Parser)]
#[command(name = "ashot", about = "GNOME / Wayland native screenshot helper")]
struct Cli {
    #[command(subcommand)]
    command: CommandKind,
}

#[derive(Debug, Subcommand)]
enum CommandKind {
    Capture {
        #[arg(value_enum)]
        mode: CaptureModeArg,
    },
    OpenSettings,
    Pin {
        image_path: PathBuf,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CaptureModeArg {
    Area,
    Screen,
    Window,
}

impl From<CaptureModeArg> for CaptureMode {
    fn from(value: CaptureModeArg) -> Self {
        match value {
            CaptureModeArg::Area => CaptureMode::Area,
            CaptureModeArg::Screen => CaptureMode::Screen,
            CaptureModeArg::Window => CaptureMode::Window,
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let _ = fmt().with_env_filter(EnvFilter::from_env("ASHOT_LOG")).with_target(false).try_init();

    match run().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("ashot: {error:#}");
            ExitCode::from(1)
        }
    }
}

async fn run() -> Result<ExitCode> {
    let cli = Cli::parse();

    match cli.command {
        CommandKind::Capture { mode } => {
            let mode = CaptureMode::from(mode);
            if mode == CaptureMode::Area {
                let connection = Connection::session().await?;
                if !name_has_owner(&connection, SHELL_DBUS_NAME).await? {
                    eprintln!(
                        "interactive area capture requires the GNOME Shell extension `ashot-shell@io.github.ashot` to be enabled"
                    );
                    return Ok(outcome_to_exit_code(OutcomeKind::Unsupported));
                }

                let shell = AshotShellProxy::new(&connection).await?;
                shell.start_capture().await?;
                eprintln!("interactive area capture started");
                return Ok(ExitCode::SUCCESS);
            }

            let connection = ensure_service().await?;
            let proxy = AshotProxy::new(&connection).await?;
            let outcome = match mode {
                CaptureMode::Area => proxy.capture_area().await?,
                CaptureMode::Screen => proxy.capture_screen().await?,
                CaptureMode::Window => proxy.capture_window().await?,
            };

            if !outcome.file_uri.is_empty() {
                println!("{}", outcome.file_uri);
            }
            if !outcome.message.is_empty() {
                eprintln!("{}", outcome.message);
            }
            Ok(outcome_to_exit_code(outcome.kind))
        }
        CommandKind::OpenSettings => {
            let connection = ensure_service().await?;
            let proxy = AshotProxy::new(&connection).await?;
            let outcome = proxy.open_settings().await?;
            print_command_message(&outcome);
            Ok(outcome_to_exit_code(outcome.kind))
        }
        CommandKind::Pin { image_path } => {
            let connection = ensure_service().await?;
            let proxy = AshotProxy::new(&connection).await?;
            let url = Url::from_file_path(&image_path).map_err(|_| {
                anyhow::anyhow!("failed to convert path to file URI: {image_path:?}")
            })?;
            let outcome = proxy.pin_image(url.as_str()).await?;
            print_command_message(&outcome);
            Ok(outcome_to_exit_code(outcome.kind))
        }
    }
}

fn print_command_message(outcome: &CommandOutcome) {
    if !outcome.message.is_empty() {
        eprintln!("{}", outcome.message);
    }
}

fn outcome_to_exit_code(kind: OutcomeKind) -> ExitCode {
    match kind {
        OutcomeKind::Ok => ExitCode::SUCCESS,
        OutcomeKind::Cancelled => ExitCode::from(2),
        OutcomeKind::Busy => ExitCode::from(3),
        OutcomeKind::Unsupported => ExitCode::from(4),
        OutcomeKind::Failed => ExitCode::from(1),
    }
}

async fn ensure_service() -> Result<Connection> {
    if let Ok(connection) = Connection::session().await {
        if service_is_running(&connection).await? {
            return Ok(connection);
        }
    }

    Command::new(resolve_app_binary()?)
        .arg("--service")
        .spawn()
        .context("failed to launch ashot-app background service")?;

    for _ in 0..10 {
        sleep(Duration::from_millis(250)).await;
        if let Ok(connection) = Connection::session().await {
            if service_is_running(&connection).await? {
                return Ok(connection);
            }
        }
    }

    Err(anyhow::anyhow!("background service did not appear on DBus after launch"))
}

fn resolve_app_binary() -> Result<PathBuf> {
    if let Some(path) = env::var_os("ASHOT_APP_BIN") {
        return Ok(PathBuf::from(path));
    }

    if let Ok(current) = env::current_exe() {
        if let Some(parent) = current.parent() {
            let sibling = parent.join("ashot-app");
            if sibling.exists() {
                return Ok(sibling);
            }
        }
    }

    Ok(PathBuf::from("ashot-app"))
}

async fn service_is_running(connection: &Connection) -> Result<bool> {
    name_has_owner(connection, DBUS_NAME).await
}

async fn name_has_owner(connection: &Connection, name: &str) -> Result<bool> {
    let dbus = DBusProxy::new(connection).await?;
    Ok(dbus.name_has_owner(name.try_into()?).await?)
}
