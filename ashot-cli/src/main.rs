use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use anyhow::{Context, Result};
use ashot_ipc::{
    AshotProxy, CaptureOutcome, CommandOutcome, DBUS_NAME, OutcomeKind, SERVICE_IDENTITY,
};
use clap::{Args, Parser, Subcommand};
use tokio::{process::Command, time::sleep};
use tracing_subscriber::{EnvFilter, fmt};
use url::Url;
use zbus::{Connection, fdo::DBusProxy};

#[derive(Debug, Parser)]
#[command(name = "ashot", about = "GNOME / Wayland native screenshot helper")]
struct Cli {
    #[command(subcommand)]
    command: Option<CommandKind>,
}

#[derive(Debug, Subcommand)]
enum CommandKind {
    Gui(GuiOptions),
    Full(CaptureOptions),
    Screen(ScreenOptions),
    Launcher,
    Config,
}

#[derive(Debug, Clone, Args, Default)]
struct GuiOptions {
    #[arg(short = 'p', long)]
    path: Option<PathBuf>,
    #[arg(short = 'c', long)]
    clipboard: bool,
    #[arg(short = 'd', long = "delay", default_value_t = 0)]
    delay_ms: u64,
    #[arg(short = 'r', long)]
    raw: bool,
    #[arg(long)]
    pin: bool,
    #[arg(long)]
    region: Option<String>,
    #[arg(long)]
    last_region: bool,
    #[arg(short = 'g', long)]
    print_geometry: bool,
    #[arg(short = 's', long)]
    accept_on_select: bool,
}

impl From<&GuiOptions> for CaptureOptions {
    fn from(value: &GuiOptions) -> Self {
        Self {
            path: value.path.clone(),
            clipboard: value.clipboard,
            delay_ms: value.delay_ms,
            raw: value.raw,
            pin: value.pin,
        }
    }
}

#[derive(Debug, Clone, Args)]
struct ScreenOptions {
    #[command(flatten)]
    actions: CaptureOptions,
    #[arg(short = 'n', long)]
    number: Option<u32>,
}

#[derive(Debug, Clone, Args)]
struct CaptureOptions {
    #[arg(short = 'p', long)]
    path: Option<PathBuf>,
    #[arg(short = 'c', long)]
    clipboard: bool,
    #[arg(short = 'd', long = "delay", default_value_t = 0)]
    delay_ms: u64,
    #[arg(short = 'r', long)]
    raw: bool,
    #[arg(long)]
    pin: bool,
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

    match resolved_command(cli) {
        CommandKind::Gui(options) => {
            let connection = ensure_service().await?;
            let proxy = AshotProxy::new(&connection).await?;
            if options.last_region || options.region.is_some() || options.print_geometry {
                eprintln!(
                    "ashot: --region, --last-region, and --print-geometry are not supported by the current portal backend yet"
                );
                return Ok(outcome_to_exit_code(OutcomeKind::Unsupported));
            }
            delay_if_needed(options.delay_ms).await;
            let outcome = proxy.capture_area().await?;
            let (code, file_uri) =
                handle_capture_outcome(&proxy, outcome, &CaptureOptions::from(&options)).await?;
            let _ = file_uri;
            Ok(code)
        }
        CommandKind::Full(options) => {
            let connection = ensure_service().await?;
            let proxy = AshotProxy::new(&connection).await?;
            delay_if_needed(options.delay_ms).await;
            let outcome = proxy.capture_screen().await?;
            let (code, _) = handle_capture_outcome(&proxy, outcome, &options).await?;
            Ok(code)
        }
        CommandKind::Screen(options) => {
            let connection = ensure_service().await?;
            let proxy = AshotProxy::new(&connection).await?;
            if options.number.is_some() {
                eprintln!(
                    "ashot: selecting a specific screen is not supported by the current portal backend yet"
                );
                return Ok(outcome_to_exit_code(OutcomeKind::Unsupported));
            }
            delay_if_needed(options.actions.delay_ms).await;
            let outcome = proxy.capture_screen().await?;
            let (code, _) = handle_capture_outcome(&proxy, outcome, &options.actions).await?;
            Ok(code)
        }
        CommandKind::Launcher => {
            let connection = ensure_service().await?;
            let proxy = AshotProxy::new(&connection).await?;
            let outcome = proxy.open_settings().await?;
            print_command_message(&outcome);
            Ok(outcome_to_exit_code(outcome.kind))
        }
        CommandKind::Config => {
            let connection = ensure_service().await?;
            let proxy = AshotProxy::new(&connection).await?;
            let outcome = proxy.open_settings().await?;
            print_command_message(&outcome);
            Ok(outcome_to_exit_code(outcome.kind))
        }
    }
}

async fn delay_if_needed(delay_ms: u64) {
    if delay_ms > 0 {
        sleep(Duration::from_millis(delay_ms)).await;
    }
}

async fn handle_capture_outcome(
    proxy: &AshotProxy<'_>,
    outcome: CaptureOutcome,
    options: &CaptureOptions,
) -> Result<(ExitCode, String)> {
    if outcome.kind != OutcomeKind::Ok {
        if !outcome.message.is_empty() {
            eprintln!("{}", outcome.message);
        }
        return Ok((outcome_to_exit_code(outcome.kind), String::new()));
    }

    let mut file_uri = outcome.file_uri.clone();
    if let Some(destination) = &options.path {
        file_uri = copy_capture_to_destination(&file_uri, destination)?;
    }

    if options.pin && !file_uri.is_empty() {
        let pin_outcome = proxy.pin_image(&file_uri).await?;
        print_command_message(&pin_outcome);
    }

    if options.clipboard {
        eprintln!("clipboard final action will run in the GTK editor path when available");
    }

    if options.raw {
        write_file_uri_to_stdout(&file_uri)?;
    } else if !file_uri.is_empty() {
        println!("{file_uri}");
    }
    if !outcome.message.is_empty() {
        eprintln!("{}", outcome.message);
    }
    Ok((ExitCode::SUCCESS, file_uri))
}

fn resolved_command(cli: Cli) -> CommandKind {
    cli.command.unwrap_or_else(|| CommandKind::Gui(GuiOptions::default()))
}

fn copy_capture_to_destination(file_uri: &str, destination: &Path) -> Result<String> {
    let source = Url::parse(file_uri)
        .ok()
        .and_then(|uri| uri.to_file_path().ok())
        .ok_or_else(|| anyhow::anyhow!("capture did not return a valid file URI"))?;
    let output = if destination.is_dir() {
        let filename = source.file_name().unwrap_or_default();
        destination.join(filename)
    } else {
        destination.to_path_buf()
    };
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create output directory {}", parent.display()))?;
    }
    fs::copy(&source, &output).with_context(|| {
        format!("failed to copy screenshot from {} to {}", source.display(), output.display())
    })?;
    Url::from_file_path(&output).map(|uri| uri.to_string()).map_err(|_| {
        anyhow::anyhow!("failed to convert output path to file URI: {}", output.display())
    })
}

fn write_file_uri_to_stdout(file_uri: &str) -> Result<()> {
    let source = Url::parse(file_uri)
        .ok()
        .and_then(|uri| uri.to_file_path().ok())
        .ok_or_else(|| anyhow::anyhow!("capture did not return a valid file URI"))?;
    let data = fs::read(&source).with_context(|| {
        format!("failed to read screenshot for raw output: {}", source.display())
    })?;
    io::stdout().write_all(&data).context("failed to write raw screenshot to stdout")
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
        if current_service_is_running(&connection).await? {
            return Ok(connection);
        }
    }

    Command::new(resolve_app_binary()?)
        .arg("--service")
        .spawn()
        .context("failed to launch ashot-app background service")?;

    for _ in 0..40 {
        sleep(Duration::from_millis(250)).await;
        if let Ok(connection) = Connection::session().await {
            if current_service_is_running(&connection).await? {
                return Ok(connection);
            }
        }
    }

    Err(anyhow::anyhow!("background service did not appear on DBus after launch"))
}

async fn current_service_is_running(connection: &Connection) -> Result<bool> {
    if !service_is_running(connection).await? {
        return Ok(false);
    }

    let proxy = AshotProxy::new(connection).await?;
    let reported_version = proxy.version().await.ok();
    if service_version_status(reported_version.as_deref()) {
        return Ok(true);
    }

    let _ = proxy.quit().await;
    Ok(false)
}

fn service_version_status(reported_version: Option<&str>) -> bool {
    reported_version == Some(SERVICE_IDENTITY)
}

fn resolve_app_binary() -> Result<PathBuf> {
    if let Some(path) = env::var_os("ASHOT_APP_BIN") {
        return Ok(PathBuf::from(path));
    }

    if let Ok(current) = env::current_exe()
        && let Some(parent) = current.parent()
    {
        let sibling = parent.join("ashot-app");
        if sibling.exists() {
            return Ok(sibling);
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

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, CommandKind, resolved_command, service_version_status};

    #[test]
    fn parses_gui_with_flameshot_final_actions() {
        let cli = Cli::try_parse_from([
            "ashot",
            "gui",
            "--path",
            "/tmp/out.png",
            "--clipboard",
            "--pin",
            "--accept-on-select",
        ])
        .expect("parse gui");

        match cli.command {
            Some(CommandKind::Gui(options)) => {
                assert_eq!(options.path.as_deref(), Some(std::path::Path::new("/tmp/out.png")));
                assert!(options.clipboard);
                assert!(options.pin);
                assert!(options.accept_on_select);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_full_with_delay_and_raw() {
        let cli =
            Cli::try_parse_from(["ashot", "full", "--delay", "350", "--raw"]).expect("parse full");

        match cli.command {
            Some(CommandKind::Full(options)) => {
                assert_eq!(options.delay_ms, 350);
                assert!(options.raw);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn rejects_removed_capture_command() {
        let error = Cli::try_parse_from(["ashot", "capture", "area"]).expect_err("old command");
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn defaults_to_gui_when_no_subcommand_is_provided() {
        let cli = Cli::try_parse_from(["ashot"]).expect("parse default");

        match resolved_command(cli) {
            CommandKind::Gui(options) => {
                assert_eq!(options.delay_ms, 0);
                assert!(!options.clipboard);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn service_version_status_requires_current_version() {
        assert!(service_version_status(Some(ashot_ipc::SERVICE_IDENTITY)));
        assert!(!service_version_status(Some(env!("CARGO_PKG_VERSION"))));
        assert!(!service_version_status(Some("0.0.0")));
        assert!(!service_version_status(None));
    }
}
