use std::{
    cell::{Cell, RefCell},
    env,
    ffi::CStr,
    fs,
    os::raw::c_char,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex},
    time::Duration,
};

use crate::render_cache::{RenderCache, RenderCacheCallback, save_png_bytes_to_dir_with_filename};
use anyhow::{Context, Result};
use ashot_capture::{CaptureClient, CaptureError};
use ashot_core::{
    Annotation, AnnotationData, AppConfig, AppearanceMode, Color, DefaultTool, Document,
    EditorHistory, LinuxDistroFamily, OcrBackend, Point, Rect, ResizeHandle, TextStyle, TextWeight,
    default_ocr_languages, detect_linux_distro_family, finalize_capture_with_config,
    language_install_command, language_package_for_distro, render_filename, search_ocr_languages,
};
use ashot_ipc::{
    APP_ID, CaptureMode, CaptureOutcome, CommandOutcome, DBUS_NAME, DBUS_PATH, OutcomeKind,
};
use glib::prelude::IsA;
use glib::translate::ToGlibPtr;
use glib::types::StaticType;
use gtk::gdk::prelude::ToplevelExt;
use gtk::prelude::{
    Cast, EventControllerExt, FileChooserExt, FileChooserExtManual, GestureSingleExt,
    NativeDialogExt, NativeDialogExtManual, NativeExt, StyleContextExt, WidgetExt,
};
use gtk::{
    Adjustment, Align, Box as GtkBox, Button, DrawingArea, Entry, Grid, HeaderBar, Label,
    MenuButton, Orientation, Overlay, Picture, PolicyType, Popover, Scale, ScrolledWindow,
    SpinButton,
};
use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;
use tokio::{
    process::Command,
    runtime::{Builder, Handle},
    sync::{Mutex as AsyncMutex, mpsc, oneshot},
};
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, fmt};
use url::Url;
use zbus::connection::Builder as ConnectionBuilder;

#[cfg(test)]
use ashot_core::{render_document, save_document_png};
#[cfg(test)]
use std::io::Cursor;

const EDITOR_HISTORY_LIMIT: usize = 64;
const COLOR_ROW_LIMIT: usize = 6;
const SELECTION_HANDLE_RADIUS: f64 = 6.4;
const SELECTION_HANDLE_HIT_TOLERANCE: f32 = 13.0;
const SIDEBAR_TOOL_BUTTON_WIDTH: i32 = 42;
const SIDEBAR_TOOL_BUTTON_HEIGHT: i32 = 34;
const SIDEBAR_ACTION_BUTTON_WIDTH: i32 = 40;
const SIDEBAR_ACTION_BUTTON_HEIGHT: i32 = 34;
const TOOL_ICON_CANVAS_SIZE: i32 = 22;
const COLOR_MEMORY_BUTTON_SIZE: i32 = 24;
const COLOR_MEMORY_SWATCH_SIZE: i32 = 18;
const COLOR_VALUE_BUTTON_WIDTH: i32 = 82;
const COLOR_VALUE_BUTTON_HEIGHT: i32 = 30;
const STROKE_PREVIEW_WIDTH: i32 = 104;
const STROKE_PREVIEW_HEIGHT: i32 = 20;
const STROKE_MENU_BUTTON_WIDTH: i32 = 58;
const STROKE_MENU_BUTTON_HEIGHT: i32 = 32;
const EYEDROPPER_MAGNIFIER_MIN_ZOOM: f64 = 4.0;
const EYEDROPPER_MAGNIFIER_MAX_ZOOM: f64 = 16.0;
const EYEDROPPER_MAGNIFIER_DEFAULT_ZOOM: f64 = 8.0;
const EYEDROPPER_MAGNIFIER_SAMPLE_RADIUS: i32 = 5;

type ApplicationWindow = adw::ApplicationWindow;
type ColorCallback = Rc<dyn Fn(Color)>;
type ColorMemoryButtons = Rc<RefCell<Vec<(Rc<Cell<Color>>, DrawingArea, Button)>>>;
type StatusCallback = Rc<dyn Fn(Option<String>)>;
type ToastCallback = Rc<dyn Fn(&str)>;

pub fn run() -> Result<()> {
    let _ = fmt().with_env_filter(EnvFilter::from_env("ASHOT_LOG")).with_target(false).try_init();

    let mut service_only = false;
    let mut filtered_args = Vec::new();
    let mut args = env::args();
    while let Some(arg) = args.next() {
        if arg == "--service" {
            service_only = true;
            continue;
        }
        filtered_args.push(arg);
    }
    let _ = adw::init();
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to create tokio runtime")?;
    let (ui_tx, ui_rx) = mpsc::unbounded_channel();
    let config = AppConfig::load_or_create().unwrap_or_default();
    apply_appearance_mode(config.appearance_mode);
    let state = Arc::new(ServiceState::new(config, ui_tx));

    let service_state = state.clone();
    runtime.spawn(async move {
        if let Err(error) = register_service(service_state).await {
            error!("failed to register DBus service: {error:#}");
        }
    });

    let app_flags = if service_only {
        gio::ApplicationFlags::NON_UNIQUE
    } else {
        gio::ApplicationFlags::empty()
    };
    let app = adw::Application::builder().application_id(APP_ID).flags(app_flags).build();
    let _hold_guard = if service_only { Some(app.hold()) } else { None };

    let ui_runtime = UiRuntime::new(app.clone(), state, runtime.handle().clone(), ui_rx);
    ui_runtime.attach(service_only);

    app.run_with_args(&filtered_args);
    Ok(())
}

#[derive(Debug)]
enum UiCommand {
    OpenEditor(PathBuf),
    OpenSettings,
    Pin(PathBuf),
    Capture { mode: CaptureMode, respond_to: oneshot::Sender<CaptureOutcome> },
}

#[derive(Debug)]
enum WindowCommand {
    OpenEditor(PathBuf),
    OpenSettings,
    Pin(PathBuf),
}

struct ServiceState {
    config: Mutex<AppConfig>,
    capture_lock: AsyncMutex<()>,
    ui_tx: mpsc::UnboundedSender<UiCommand>,
}

impl ServiceState {
    fn new(config: AppConfig, ui_tx: mpsc::UnboundedSender<UiCommand>) -> Self {
        Self { config: Mutex::new(config), capture_lock: AsyncMutex::new(()), ui_tx }
    }

    fn config_snapshot(&self) -> AppConfig {
        self.config.lock().expect("config lock poisoned").clone()
    }

    fn update_config(&self, updated: AppConfig) {
        if let Ok(mut guard) = self.config.lock() {
            *guard = updated;
        }
    }

    async fn capture_mode(&self, mode: CaptureMode) -> CaptureOutcome {
        let Ok(_guard) = self.capture_lock.try_lock() else {
            return CaptureOutcome::status(OutcomeKind::Busy, "a capture is already in progress");
        };

        let (respond_to, receive_from_ui) = oneshot::channel();
        if self.ui_tx.send(UiCommand::Capture { mode, respond_to }).is_err() {
            return CaptureOutcome::status(
                OutcomeKind::Failed,
                "capture UI is unavailable; the application may be shutting down",
            );
        }

        match receive_from_ui.await {
            Ok(outcome) => outcome,
            Err(_) => CaptureOutcome::status(
                OutcomeKind::Failed,
                "capture UI disappeared before the screenshot request finished",
            ),
        }
    }

    fn finish_capture(
        &self,
        mode: CaptureMode,
        result: Result<Url, CaptureError>,
    ) -> CaptureOutcome {
        match result {
            Ok(uri) => {
                let config = self.config_snapshot();
                if mode == CaptureMode::Area || config.post_capture_open_editor {
                    if let Ok(path) = uri.to_file_path() {
                        let _ = self.ui_tx.send(UiCommand::OpenEditor(path));
                    }
                }
                CaptureOutcome::ok(uri, capture_message(mode))
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

    fn open_settings(&self) -> CommandOutcome {
        let _ = self.ui_tx.send(UiCommand::OpenSettings);
        CommandOutcome::ok("settings opened")
    }

    fn open_editor(&self, file_uri: &str) -> CommandOutcome {
        match Url::parse(file_uri).ok().and_then(|uri| uri.to_file_path().ok()) {
            Some(path) => {
                let _ = self.ui_tx.send(UiCommand::OpenEditor(path));
                CommandOutcome::ok("editor opened")
            }
            None => CommandOutcome::status(OutcomeKind::Failed, "invalid file URI"),
        }
    }

    fn pin_image(&self, file_uri: &str) -> CommandOutcome {
        match Url::parse(file_uri).ok().and_then(|uri| uri.to_file_path().ok()) {
            Some(path) => {
                let _ = self.ui_tx.send(UiCommand::Pin(path));
                CommandOutcome::ok("pin window opened")
            }
            None => CommandOutcome::status(OutcomeKind::Failed, "invalid file URI"),
        }
    }

    fn finalize_capture(&self, source_file_uri: &str, annotations_json: &str) -> CaptureOutcome {
        let Some(source_path) =
            Url::parse(source_file_uri).ok().and_then(|uri| uri.to_file_path().ok())
        else {
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
        let output_path = match finalize_capture_with_config(
            &config,
            &source_path,
            &annotations,
            chrono::Local::now(),
        ) {
            Ok(path) => path,
            Err(error) => {
                return CaptureOutcome::status(
                    OutcomeKind::Failed,
                    format!("failed to finalize annotated capture: {error}"),
                );
            }
        };

        if source_path != output_path {
            let _ = fs::remove_file(&source_path);
        }

        if config.pin_after_save {
            let _ = self.ui_tx.send(UiCommand::Pin(output_path.clone()));
        }

        match Url::from_file_path(&output_path) {
            Ok(file_uri) => CaptureOutcome::ok(
                file_uri,
                format!("saved annotated screenshot to {}", output_path.display()),
            ),
            Err(_) => CaptureOutcome::status(
                OutcomeKind::Failed,
                format!(
                    "saved annotated screenshot, but failed to convert path to file URI: {}",
                    output_path.display()
                ),
            ),
        }
    }
}

fn persist_config_change<F>(state: &Arc<ServiceState>, update: F)
where
    F: FnOnce(&mut AppConfig),
{
    let mut config = state.config_snapshot();
    update(&mut config);
    if let Err(error) = config.save() {
        error!("failed to save config: {error}");
    }
    state.update_config(config);
}

fn capture_message(mode: CaptureMode) -> &'static str {
    match mode {
        CaptureMode::Area => "region capture completed",
        CaptureMode::Screen => "screen capture completed",
        CaptureMode::Window => "window capture completed",
    }
}

async fn register_service(state: Arc<ServiceState>) -> Result<()> {
    let service = DbusService { state };
    let _connection = ConnectionBuilder::session()?
        .name(DBUS_NAME)?
        .serve_at(DBUS_PATH, service)?
        .build()
        .await?;
    std::future::pending::<()>().await;
    Ok(())
}

struct DbusService {
    state: Arc<ServiceState>,
}

#[zbus::interface(name = "io.github.ashot.App")]
impl DbusService {
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
        self.state.open_settings()
    }

    #[zbus(name = "OpenEditor")]
    async fn open_editor(&self, file_uri: &str) -> CommandOutcome {
        self.state.open_editor(file_uri)
    }

    #[zbus(name = "PinImage")]
    async fn pin_image(&self, file_uri: &str) -> CommandOutcome {
        self.state.pin_image(file_uri)
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

struct UiRuntime {
    app: adw::Application,
    state: Arc<ServiceState>,
    runtime: Handle,
    ui_rx: Rc<RefCell<mpsc::UnboundedReceiver<UiCommand>>>,
}

impl UiRuntime {
    fn new(
        app: adw::Application,
        state: Arc<ServiceState>,
        runtime: Handle,
        ui_rx: mpsc::UnboundedReceiver<UiCommand>,
    ) -> Self {
        Self { app, state, runtime, ui_rx: Rc::new(RefCell::new(ui_rx)) }
    }

    fn attach(self, service_only: bool) {
        let state = self.state.clone();
        let runtime = self.runtime.clone();
        let pending_window_commands = Rc::new(RefCell::new(Vec::<WindowCommand>::new()));
        let pending_for_activate = pending_window_commands.clone();
        self.app.connect_activate(move |app| {
            loop {
                let command = pending_for_activate.borrow_mut().pop();
                let Some(command) = command else {
                    break;
                };
                present_window_command(app, state.clone(), runtime.clone(), command);
            }
            if !service_only {
                present_launcher(app, state.clone(), runtime.clone());
            }
        });

        let app = self.app.clone();
        let state = self.state.clone();
        let runtime = self.runtime.clone();
        let receiver = self.ui_rx.clone();
        let pending_for_poll = pending_window_commands.clone();
        glib::timeout_add_local(Duration::from_millis(60), move || {
            loop {
                let command = {
                    let mut receiver = receiver.borrow_mut();
                    receiver.try_recv()
                };
                let Ok(command) = command else {
                    break;
                };
                match command {
                    UiCommand::OpenEditor(path) => {
                        info!("ui command: open editor for {}", path.display());
                        pending_for_poll.borrow_mut().push(WindowCommand::OpenEditor(path));
                        app.activate();
                    }
                    UiCommand::OpenSettings => {
                        pending_for_poll.borrow_mut().push(WindowCommand::OpenSettings);
                        app.activate();
                    }
                    UiCommand::Pin(path) => {
                        pending_for_poll.borrow_mut().push(WindowCommand::Pin(path));
                        app.activate();
                    }
                    UiCommand::Capture { mode, respond_to } => {
                        begin_capture_request(
                            &app,
                            state.clone(),
                            runtime.clone(),
                            mode,
                            respond_to,
                        );
                    }
                }
            }
            glib::ControlFlow::Continue
        });
    }
}

fn present_window_command(
    app: &adw::Application,
    state: Arc<ServiceState>,
    runtime: Handle,
    command: WindowCommand,
) {
    match command {
        WindowCommand::OpenEditor(path) => {
            if let Err(error) = present_editor(app, state, runtime, path) {
                error!("failed to open editor: {error:#}");
            }
        }
        WindowCommand::OpenSettings => {
            present_settings(app, state);
        }
        WindowCommand::Pin(path) => {
            present_pin_window(app, state, path);
        }
    }
}

fn begin_capture_request(
    app: &adw::Application,
    state: Arc<ServiceState>,
    runtime: Handle,
    mode: CaptureMode,
    respond_to: oneshot::Sender<CaptureOutcome>,
) {
    if running_in_flatpak() {
        runtime.spawn(async move {
            let result = async {
                let client = CaptureClient::new().await?;
                client.capture(mode, None).await
            }
            .await;
            let outcome = state.finish_capture(mode, result);
            let _ = respond_to.send(outcome);
        });
        return;
    }

    let (host_window, temporary_window) = capture_host_window(app);
    let started = Rc::new(Cell::new(false));
    let respond_to = Rc::new(RefCell::new(Some(respond_to)));

    let try_start = {
        let host_window = host_window.clone();
        let state = state.clone();
        let runtime = runtime.clone();
        let started = started.clone();
        let respond_to = respond_to.clone();
        move || {
            if started.get() || host_window.surface().is_none() {
                return;
            }
            started.set(true);
            let host_window = host_window.clone();
            let state = state.clone();
            let runtime = runtime.clone();
            let respond_to = respond_to.clone();
            glib::MainContext::default().spawn_local(async move {
                let parent_window = export_parent_window_identifier(&host_window).await;
                let result = match parent_window {
                    Ok(parent_window) => match runtime
                        .spawn(async move {
                            let client = CaptureClient::new().await?;
                            client.capture(mode, Some(&parent_window)).await
                        })
                        .await
                    {
                        Ok(result) => result,
                        Err(error) => Err(CaptureError::InvalidResponse(format!(
                            "capture task crashed: {error}"
                        ))),
                    },
                    Err(error) => Err(CaptureError::InvalidResponse(error)),
                };

                if temporary_window {
                    host_window.close();
                }

                let outcome = state.finish_capture(mode, result);
                if let Some(respond_to) = respond_to.borrow_mut().take() {
                    let _ = respond_to.send(outcome);
                }
            });
        }
    };

    if host_window.surface().is_none() {
        let try_start_on_focus = try_start.clone();
        host_window.connect_map(move |_| try_start_on_focus());
        host_window.present();
    }

    try_start();

    let host_window_for_timeout = host_window.clone();
    glib::timeout_add_local_once(Duration::from_secs(3), move || {
        if started.get() {
            return;
        }
        started.set(true);
        if temporary_window {
            host_window_for_timeout.close();
        }
        if let Some(respond_to) = respond_to.borrow_mut().take() {
            let _ = respond_to.send(CaptureOutcome::status(
                OutcomeKind::Failed,
                "capture window could not be realized in time",
            ));
        }
    });
}

fn capture_host_window(app: &adw::Application) -> (gtk::Window, bool) {
    let has_active_window = app.active_window().is_some();
    debug_assert!(capture_should_use_fresh_anchor(has_active_window));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("aShot Capture")
        .default_width(340)
        .default_height(96)
        .resizable(false)
        .hide_on_close(true)
        .build();
    let root = GtkBox::new(Orientation::Vertical, 8);
    root.set_margin_top(18);
    root.set_margin_bottom(18);
    root.set_margin_start(18);
    root.set_margin_end(18);
    let title = Label::new(Some("Preparing GNOME screenshot"));
    title.add_css_class("title-4");
    title.set_xalign(0.0);
    let body =
        Label::new(Some("This window only exists to anchor the portal dialog under Wayland."));
    body.set_wrap(true);
    body.set_xalign(0.0);
    root.append(&title);
    root.append(&body);
    window.set_content(Some(&root));
    window.present();
    (window.upcast::<gtk::Window>(), true)
}

fn capture_should_use_fresh_anchor(_has_active_window: bool) -> bool {
    true
}

fn editor_initial_size(image_width: u32, image_height: u32) -> (i32, i32) {
    let width = (image_width as i32 + 320).clamp(980, 1440);
    let height = (image_height as i32 + 180).clamp(620, 980);
    (width, height)
}

fn fit_scale(
    image_width: u32,
    image_height: u32,
    viewport_width: i32,
    viewport_height: i32,
) -> f64 {
    let image_width = image_width.max(1) as f64;
    let image_height = image_height.max(1) as f64;
    let viewport_width = viewport_width.max(1) as f64;
    let viewport_height = viewport_height.max(1) as f64;
    (viewport_width / image_width).min(viewport_height / image_height).min(1.0).max(0.05)
}

fn scaled_image_size(image_width: u32, image_height: u32, scale: f64) -> (i32, i32) {
    (
        (image_width as f64 * scale).round().max(1.0) as i32,
        (image_height as f64 * scale).round().max(1.0) as i32,
    )
}

fn scaled_canvas_point(x: f64, y: f64, scale: f64) -> Point {
    let scale = scale.max(0.05);
    Point::new((x / scale) as f32, (y / scale) as f32)
}

fn annotation_snapshot_for_draw(document: &Rc<RefCell<Document>>) -> Option<Vec<Annotation>> {
    document.try_borrow().ok().map(|document| document.annotations.clone())
}

fn draft_preview_for_draw(draft: &Rc<RefCell<Option<DraftAnnotation>>>) -> Option<Annotation> {
    draft
        .try_borrow()
        .ok()
        .and_then(|draft| draft.as_ref().and_then(DraftAnnotation::preview_annotation))
}

async fn export_parent_window_identifier(
    window: &gtk::Window,
) -> std::result::Result<String, String> {
    if !gtk::prelude::WidgetExt::display(window).backend().is_wayland() {
        return Err("aShot currently requires a Wayland-backed GTK window".into());
    }

    let surface =
        window.surface().ok_or_else(|| "capture window has no GDK surface yet".to_string())?;
    let toplevel = surface
        .dynamic_cast::<gtk::gdk::Toplevel>()
        .map_err(|_| "capture surface is not a toplevel surface".to_string())?;
    let (send_handle, receive_handle) = oneshot::channel::<std::result::Result<String, String>>();
    let callback_state = Box::new(RefCell::new(Some(send_handle)));

    let exported = unsafe {
        gdk_wayland_toplevel_export_handle(
            toplevel.to_glib_none().0,
            Some(exported_toplevel_handle),
            Box::into_raw(callback_state) as glib::ffi::gpointer,
            Some(destroy_export_callback),
        )
    };
    if exported == glib::ffi::GFALSE {
        return Err("GTK could not export a Wayland parent handle for this window".into());
    }

    let exported_handle = receive_handle.await.map_err(|_| {
        "GTK dropped the Wayland parent handle export before completion".to_string()
    })?;
    let handle = exported_handle?;
    Ok(format!("wayland:{handle}"))
}

unsafe extern "C" fn exported_toplevel_handle(
    _toplevel: *mut gtk::gdk::ffi::GdkToplevel,
    handle: *const c_char,
    user_data: glib::ffi::gpointer,
) {
    let callback_state = unsafe {
        &*(user_data
            as *const RefCell<Option<oneshot::Sender<std::result::Result<String, String>>>>)
    };
    if let Some(send_handle) = callback_state.borrow_mut().take() {
        let result = if handle.is_null() {
            Err("GTK exported an empty Wayland parent handle".into())
        } else {
            Ok(unsafe { CStr::from_ptr(handle) }.to_string_lossy().into_owned())
        };
        let _ = send_handle.send(result);
    }
}

unsafe extern "C" fn destroy_export_callback(user_data: glib::ffi::gpointer) {
    drop(unsafe {
        Box::from_raw(
            user_data as *mut RefCell<Option<oneshot::Sender<std::result::Result<String, String>>>>,
        )
    });
}

unsafe extern "C" {
    fn gdk_wayland_toplevel_export_handle(
        toplevel: *mut gtk::gdk::ffi::GdkToplevel,
        callback: Option<
            unsafe extern "C" fn(
                *mut gtk::gdk::ffi::GdkToplevel,
                *const c_char,
                glib::ffi::gpointer,
            ),
        >,
        user_data: glib::ffi::gpointer,
        destroy_notify: Option<unsafe extern "C" fn(glib::ffi::gpointer)>,
    ) -> glib::ffi::gboolean;
}

fn present_launcher(app: &adw::Application, state: Arc<ServiceState>, runtime: Handle) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("aShot")
        .default_width(420)
        .default_height(220)
        .build();

    let root = GtkBox::new(Orientation::Vertical, 12);
    root.set_margin_top(24);
    root.set_margin_bottom(24);
    root.set_margin_start(24);
    root.set_margin_end(24);

    let title = Label::new(Some("GNOME / Wayland native screenshot workflow"));
    title.add_css_class("title-2");
    title.set_wrap(true);
    root.append(&title);

    let subtitle = Label::new(Some(
        "Use the buttons below for testing. In daily use, bind `ashot gui` to a Linux desktop shortcut.",
    ));
    subtitle.set_wrap(true);
    subtitle.set_xalign(0.0);
    root.append(&subtitle);

    let buttons = GtkBox::new(Orientation::Horizontal, 8);
    for (label, mode) in [
        ("Region", CaptureMode::Area),
        ("Screen", CaptureMode::Screen),
        ("Window", CaptureMode::Window),
    ] {
        let button = Button::with_label(label);
        let state = state.clone();
        let runtime = runtime.clone();
        button.connect_clicked(move |_| {
            let state = state.clone();
            runtime.spawn(async move {
                let outcome = state.capture_mode(mode).await;
                if outcome.kind != OutcomeKind::Ok {
                    info!("capture result: {}", outcome.message);
                }
            });
        });
        buttons.append(&button);
    }
    root.append(&buttons);

    let settings = Button::with_label("Settings");
    let app = app.clone();
    settings.connect_clicked(move |_| {
        present_settings(&app, state.clone());
    });
    root.append(&settings);

    window.set_content(Some(&root));
    window.present();
}

fn present_settings(app: &adw::Application, state: Arc<ServiceState>) {
    let window = adw::PreferencesWindow::builder()
        .application(app)
        .title("aShot Settings")
        .default_width(680)
        .default_height(720)
        .search_enabled(true)
        .build();

    let config = state.config_snapshot();
    let page = adw::PreferencesPage::new();
    let capture_group = adw::PreferencesGroup::builder().title("Capture and Save").build();
    let appearance_group = adw::PreferencesGroup::builder()
        .title("Appearance")
        .description("Choose whether aShot follows the desktop theme or uses a fixed theme.")
        .build();
    let ocr_group = adw::PreferencesGroup::builder()
        .title("OCR")
        .description("Configure text recognition and local Tesseract language package hints.")
        .build();
    let actions_group = adw::PreferencesGroup::builder().title("Actions").build();

    let save_dir_entry = Entry::builder()
        .hexpand(true)
        .text(config.default_save_dir.to_string_lossy().as_ref())
        .build();
    let save_dir_row = settings_labeled_control("Default save directory", &save_dir_entry);
    capture_group.add(&save_dir_row);

    let template_entry = Entry::builder().hexpand(true).text(&config.filename_template).build();
    let template_row = settings_labeled_control("Filename template", &template_entry);
    capture_group.add(&template_row);

    let auto_copy = gtk::CheckButton::with_label("Auto-copy saved screenshots to clipboard");
    auto_copy.set_active(config.auto_copy);
    capture_group.add(&auto_copy);

    let open_editor = gtk::CheckButton::with_label("Open the editor after capture");
    open_editor.set_active(config.post_capture_open_editor);
    capture_group.add(&open_editor);

    let pin_after_save = gtk::CheckButton::with_label("Pin screenshots after save");
    pin_after_save.set_active(config.pin_after_save);
    capture_group.add(&pin_after_save);

    let appearance_model = gtk::StringList::new(&appearance_mode_labels());
    let appearance_expression = gtk::PropertyExpression::new(
        gtk::StringObject::static_type(),
        None::<&gtk::Expression>,
        "string",
    );
    let appearance_row = adw::ComboRow::builder()
        .title("Theme")
        .subtitle("Follow the desktop appearance or force a light/dark editor.")
        .model(&appearance_model)
        .expression(&appearance_expression)
        .selected(appearance_mode_index(config.appearance_mode))
        .build();
    appearance_group.add(&appearance_row);

    let ocr_space_backend = gtk::CheckButton::with_label("Use OCR.space online backend");
    ocr_space_backend
        .set_tooltip_text(Some("When enabled, selected OCR regions are uploaded to OCR.space."));
    ocr_space_backend.set_active(config.ocr_backend == OcrBackend::OcrSpace);
    ocr_group.add(&ocr_space_backend);

    let ocr_api_key_entry = Entry::builder()
        .hexpand(true)
        .text(&config.ocr_space_api_key)
        .placeholder_text("OCR.space API key")
        .build();
    let api_key_row = settings_labeled_control("OCR.space API key", &ocr_api_key_entry);
    ocr_group.add(&api_key_row);

    let ocr_filter_symbols = gtk::CheckButton::with_label("Filter emoji and symbol noise");
    ocr_filter_symbols.set_active(config.ocr_filter_symbols);
    ocr_group.add(&ocr_filter_symbols);

    let distro = detect_linux_distro_family();
    let selected_ocr_languages = Rc::new(RefCell::new(config.ocr_languages.clone()));
    let selected_ocr_label = Label::new(None);
    selected_ocr_label.set_xalign(0.0);
    selected_ocr_label.set_wrap(true);
    ocr_group.add(&selected_ocr_label);

    let install_command_label = Label::new(None);
    install_command_label.set_xalign(0.0);
    install_command_label.set_wrap(true);
    install_command_label.add_css_class("dim-label");
    ocr_group.add(&install_command_label);

    let language_menu = ocr_language_install_menu(
        selected_ocr_languages.clone(),
        selected_ocr_label.clone(),
        install_command_label.clone(),
        distro,
    );
    ocr_group.add(&language_menu);

    let install_actions = GtkBox::new(Orientation::Horizontal, 8);
    let copy_install_command = Button::with_label("Copy Install Command");
    install_actions.append(&copy_install_command);
    ocr_group.add(&install_actions);

    let selected_ocr_languages_for_copy = selected_ocr_languages.clone();
    copy_install_command.connect_clicked(move |_| {
        let Ok(languages) = selected_ocr_languages_for_copy.try_borrow() else {
            return;
        };
        copy_text_to_clipboard(&language_install_command(&languages, distro));
    });

    let actions = GtkBox::new(Orientation::Horizontal, 8);
    let save = Button::with_label("Save");
    let reset = Button::with_label("Restore defaults");
    let close = Button::with_label("Close");
    save.add_css_class("suggested-action");
    actions.append(&save);
    actions.append(&reset);
    actions.append(&close);
    actions_group.add(&actions);

    let state_for_save = state.clone();
    let save_dir_entry_for_save = save_dir_entry.clone();
    let template_entry_for_save = template_entry.clone();
    let auto_copy_for_save = auto_copy.clone();
    let open_editor_for_save = open_editor.clone();
    let pin_after_save_for_save = pin_after_save.clone();
    let appearance_row_for_save = appearance_row.clone();
    let ocr_space_backend_for_save = ocr_space_backend.clone();
    let ocr_api_key_entry_for_save = ocr_api_key_entry.clone();
    let ocr_filter_symbols_for_save = ocr_filter_symbols.clone();
    let selected_ocr_languages_for_save = selected_ocr_languages.clone();
    save.connect_clicked(move |_| {
        let mut updated = state_for_save.config_snapshot();
        updated.default_save_dir = PathBuf::from(save_dir_entry_for_save.text().as_str());
        updated.filename_template = template_entry_for_save.text().to_string();
        updated.auto_copy = auto_copy_for_save.is_active();
        updated.post_capture_open_editor = open_editor_for_save.is_active();
        updated.pin_after_save = pin_after_save_for_save.is_active();
        updated.appearance_mode = appearance_mode_from_index(appearance_row_for_save.selected());
        updated.ocr_backend = if ocr_space_backend_for_save.is_active() {
            OcrBackend::OcrSpace
        } else {
            OcrBackend::Tesseract
        };
        updated.ocr_space_api_key = ocr_api_key_entry_for_save.text().to_string();
        updated.ocr_filter_symbols = ocr_filter_symbols_for_save.is_active();
        if let Ok(languages) = selected_ocr_languages_for_save.try_borrow() {
            updated.ocr_languages = languages.clone();
        }
        let _ = updated.save();
        apply_appearance_mode(updated.appearance_mode);
        state_for_save.update_config(updated);
    });

    let state_for_reset = state.clone();
    let language_menu_for_reset = language_menu.clone();
    reset.connect_clicked(move |_| {
        let mut updated = state_for_reset.config_snapshot();
        updated.restore_defaults();
        let _ = updated.save();
        state_for_reset.update_config(updated.clone());
        save_dir_entry.set_text(updated.default_save_dir.to_string_lossy().as_ref());
        template_entry.set_text(&updated.filename_template);
        auto_copy.set_active(updated.auto_copy);
        open_editor.set_active(updated.post_capture_open_editor);
        pin_after_save.set_active(updated.pin_after_save);
        appearance_row.set_selected(appearance_mode_index(updated.appearance_mode));
        ocr_space_backend.set_active(updated.ocr_backend == OcrBackend::OcrSpace);
        ocr_api_key_entry.set_text(&updated.ocr_space_api_key);
        ocr_filter_symbols.set_active(updated.ocr_filter_symbols);
        if let Ok(mut languages) = selected_ocr_languages.try_borrow_mut() {
            *languages = updated.ocr_languages;
        }
        refresh_ocr_language_summary(
            &selected_ocr_label,
            &install_command_label,
            Some(&language_menu_for_reset),
            selected_ocr_languages.clone(),
            distro,
        );
        apply_appearance_mode(updated.appearance_mode);
    });

    let window_for_close = window.clone();
    close.connect_clicked(move |_| {
        window_for_close.close();
    });

    page.add(&capture_group);
    page.add(&appearance_group);
    page.add(&ocr_group);
    page.add(&actions_group);
    window.add(&page);
    window.present();
}

fn appearance_mode_labels() -> [&'static str; 3] {
    ["Follow System", "Light", "Dark"]
}

fn appearance_mode_index(mode: AppearanceMode) -> u32 {
    match mode {
        AppearanceMode::System => 0,
        AppearanceMode::Light => 1,
        AppearanceMode::Dark => 2,
    }
}

fn appearance_mode_from_index(index: u32) -> AppearanceMode {
    match index {
        1 => AppearanceMode::Light,
        2 => AppearanceMode::Dark,
        _ => AppearanceMode::System,
    }
}

fn appearance_color_scheme(mode: AppearanceMode) -> adw::ColorScheme {
    match mode {
        AppearanceMode::System => adw::ColorScheme::Default,
        AppearanceMode::Light => adw::ColorScheme::ForceLight,
        AppearanceMode::Dark => adw::ColorScheme::ForceDark,
    }
}

fn apply_appearance_mode(mode: AppearanceMode) {
    adw::StyleManager::default().set_color_scheme(appearance_color_scheme(mode));
}

fn settings_labeled_control(label: &str, control: &impl IsA<gtk::Widget>) -> GtkBox {
    let row = GtkBox::new(Orientation::Vertical, 6);
    row.set_margin_top(6);
    row.set_margin_bottom(6);
    row.set_margin_start(6);
    row.set_margin_end(6);
    let title = Label::new(Some(label));
    title.set_xalign(0.0);
    title.add_css_class("dim-label");
    row.append(&title);
    row.append(control);
    row
}

fn refresh_ocr_language_summary(
    selected_label: &Label,
    install_command_label: &Label,
    language_menu: Option<&MenuButton>,
    selected_languages: Rc<RefCell<Vec<String>>>,
    distro: LinuxDistroFamily,
) {
    let selected_snapshot =
        selected_languages.try_borrow().map(|languages| languages.clone()).unwrap_or_default();
    selected_label
        .set_text(&format!("Selected OCR languages: {}", ocr_language_summary(&selected_snapshot)));
    install_command_label.set_text(&language_install_command(&selected_snapshot, distro));
    if let Some(menu) = language_menu {
        menu.set_label(&ocr_language_label(&selected_snapshot));
    }
}

fn ocr_language_install_menu(
    selected_languages: Rc<RefCell<Vec<String>>>,
    selected_label: Label,
    install_command_label: Label,
    distro: LinuxDistroFamily,
) -> MenuButton {
    let menu = MenuButton::new();
    menu.set_tooltip_text(Some("Select OCR language packages"));
    menu.set_hexpand(true);

    let popover = Popover::new();
    let root = GtkBox::new(Orientation::Vertical, 8);
    root.set_margin_top(8);
    root.set_margin_bottom(8);
    root.set_margin_start(8);
    root.set_margin_end(8);

    let search = Entry::builder()
        .hexpand(true)
        .placeholder_text("Search languages, e.g. 中文, English, jpn")
        .build();
    root.append(&search);

    let language_results = GtkBox::new(Orientation::Vertical, 6);
    let scrolled = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vscrollbar_policy(PolicyType::Automatic)
        .min_content_height(260)
        .child(&language_results)
        .build();
    root.append(&scrolled);

    refresh_ocr_language_settings(
        &language_results,
        &selected_label,
        &install_command_label,
        Some(menu.clone()),
        "",
        selected_languages.clone(),
        distro,
    );

    let language_results_for_search = language_results.clone();
    let selected_label_for_search = selected_label.clone();
    let install_command_label_for_search = install_command_label.clone();
    let selected_languages_for_search = selected_languages.clone();
    let menu_for_search = menu.clone();
    search.connect_changed(move |entry| {
        refresh_ocr_language_settings(
            &language_results_for_search,
            &selected_label_for_search,
            &install_command_label_for_search,
            Some(menu_for_search.clone()),
            entry.text().as_str(),
            selected_languages_for_search.clone(),
            distro,
        );
    });

    popover.set_child(Some(&root));
    menu.set_popover(Some(&popover));
    menu
}

fn refresh_ocr_language_settings(
    language_results: &GtkBox,
    selected_label: &Label,
    install_command_label: &Label,
    language_menu: Option<MenuButton>,
    query: &str,
    selected_languages: Rc<RefCell<Vec<String>>>,
    distro: LinuxDistroFamily,
) {
    while let Some(child) = language_results.first_child() {
        language_results.remove(&child);
    }

    refresh_ocr_language_summary(
        selected_label,
        install_command_label,
        language_menu.as_ref(),
        selected_languages.clone(),
        distro,
    );
    let selected_snapshot =
        selected_languages.try_borrow().map(|languages| languages.clone()).unwrap_or_default();
    let checks = Rc::new(RefCell::new(Vec::<(String, gtk::CheckButton)>::new()));

    for language in search_ocr_languages(query).into_iter().take(8) {
        let row = GtkBox::new(Orientation::Vertical, 2);
        let header = GtkBox::new(Orientation::Horizontal, 8);
        let check = gtk::CheckButton::with_label(&format!(
            "{} ({})",
            language.display_name, language.tesseract_code
        ));
        check.set_active(ocr_language_is_selected(&selected_snapshot, language.tesseract_code));
        header.append(&check);
        checks.borrow_mut().push((language.tesseract_code.to_string(), check.clone()));

        let copy_package = Button::with_label("Copy Package");
        if language_package_for_distro(language, distro).is_none() {
            copy_package.set_sensitive(false);
        }
        header.append(&copy_package);
        row.append(&header);

        let package_text = if language.tesseract_code == "auto" {
            "Auto uses OCR.space language detection; local Tesseract falls back to default language packs"
                .to_string()
        } else {
            language_package_for_distro(language, distro)
                .map(|package| format!("Tesseract package: {package}"))
                .unwrap_or_else(|| {
                    format!(
                        "Tesseract language code: {}; install the matching traineddata package",
                        language.tesseract_code
                    )
                })
        };
        let package_label = Label::new(Some(&package_text));
        package_label.set_xalign(0.0);
        package_label.add_css_class("dim-label");
        row.append(&package_label);

        let language_code = language.tesseract_code.to_string();
        let selected_for_toggle = selected_languages.clone();
        let selected_label_for_toggle = selected_label.clone();
        let install_command_label_for_toggle = install_command_label.clone();
        let language_menu_for_toggle = language_menu.clone();
        let checks_for_toggle = checks.clone();
        check.connect_toggled(move |button| {
            let Ok(mut languages) = selected_for_toggle.try_borrow_mut() else {
                return;
            };
            update_ocr_language_selection(&mut languages, &language_code, button.is_active());
            drop(languages);
            refresh_ocr_language_summary(
                &selected_label_for_toggle,
                &install_command_label_for_toggle,
                language_menu_for_toggle.as_ref(),
                selected_for_toggle.clone(),
                distro,
            );
            if let (Ok(languages), Ok(checks)) =
                (selected_for_toggle.try_borrow(), checks_for_toggle.try_borrow())
            {
                for (code, check) in checks.iter() {
                    let should_be_active = ocr_language_is_selected(&languages, code);
                    if check.is_active() != should_be_active {
                        check.set_active(should_be_active);
                    }
                }
            }
        });

        if let Some(package) = language_package_for_distro(language, distro) {
            let package = package.to_string();
            copy_package.connect_clicked(move |_| {
                copy_text_to_clipboard(&package);
            });
        }

        language_results.append(&row);
    }
}

fn present_editor(
    app: &adw::Application,
    state: Arc<ServiceState>,
    runtime: Handle,
    image_path: PathBuf,
) -> Result<()> {
    install_editor_css();
    info!("present_editor start: {}", image_path.display());
    let image = image::open(&image_path)
        .with_context(|| format!("failed to load screenshot at {}", image_path.display()))?;
    info!("present_editor image loaded: {}x{}", image.width(), image.height());
    let config = state.config_snapshot();
    info!("present_editor config snapshot loaded");
    let document =
        Rc::new(RefCell::new(Document::new(image.width(), image.height(), config.default_tool)));
    info!("present_editor document created");
    let history = Rc::new(RefCell::new(EditorHistory::new(EDITOR_HISTORY_LIMIT)));
    info!("present_editor history created");
    let draft = Rc::new(RefCell::new(None::<DraftAnnotation>));
    let moving = Rc::new(RefCell::new(None::<Point>));
    let resizing = Rc::new(RefCell::new(None::<ResizeHandle>));
    let brush_cursor_preview = Rc::new(Cell::new(None::<Point>));
    let active_text_edit = Rc::new(RefCell::new(None::<ActiveTextEdit>));
    let active_color = Rc::new(Cell::new(config.default_color));
    let active_stroke = Rc::new(Cell::new(config.default_stroke_width));
    let active_ocr_languages = Rc::new(RefCell::new(config.ocr_languages.clone()));
    let active_ocr_filter_symbols = Rc::new(Cell::new(config.ocr_filter_symbols));
    let active_magnifier_enabled = Rc::new(Cell::new(config.eyedropper_magnifier_enabled));
    let active_magnifier_zoom =
        Rc::new(Cell::new(clamp_magnifier_zoom(config.eyedropper_magnifier_zoom)));
    let base_rgba = Arc::new(image.to_rgba8());
    let base_image = Rc::new(image);
    let render_cache = Rc::new(RefCell::new(RenderCache::new(runtime.clone(), base_rgba.clone())));
    let queue_render_cache: Rc<dyn Fn()> = {
        let render_cache = render_cache.clone();
        let document = document.clone();
        Rc::new(move || {
            let Some(annotations) = annotation_snapshot_for_draw(&document) else {
                return;
            };
            if let Ok(mut render_cache) = render_cache.try_borrow_mut() {
                render_cache.request_update(annotations);
            }
        })
    };
    queue_render_cache();

    let (initial_width, initial_height) =
        editor_initial_size(base_image.width(), base_image.height());
    info!("present_editor initial size calculated: {initial_width}x{initial_height}");
    let viewport_width = initial_width - 292;
    let viewport_height = initial_height - 70;
    let image_scale = Rc::new(Cell::new(fit_scale(
        base_image.width(),
        base_image.height(),
        viewport_width,
        viewport_height,
    )));
    info!("present_editor image scale calculated");
    let (scaled_width, scaled_height) =
        scaled_image_size(base_image.width(), base_image.height(), image_scale.get());
    info!("present_editor scaled size calculated: {scaled_width}x{scaled_height}");
    let window = ApplicationWindow::builder()
        .application(app)
        .title("aShot Editor")
        .default_width(initial_width)
        .default_height(initial_height)
        .build();
    info!("present_editor window built");
    let render_cache_for_poll = render_cache.clone();
    let window_for_render_cache_poll = window.downgrade();
    glib::timeout_add_local(Duration::from_millis(40), move || {
        if window_for_render_cache_poll.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }
        if let Ok(mut render_cache) = render_cache_for_poll.try_borrow_mut() {
            render_cache.poll();
        }
        glib::ControlFlow::Continue
    });

    let root = GtkBox::new(Orientation::Vertical, 8);
    root.set_vexpand(true);
    root.set_hexpand(true);
    root.set_margin_top(6);
    root.set_margin_bottom(6);
    root.set_margin_start(6);
    root.set_margin_end(6);
    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&root));
    let show_toast: ToastCallback = {
        let toast_overlay = toast_overlay.clone();
        Rc::new(move |message: &str| {
            toast_overlay.add_toast(adw::Toast::new(message));
        })
    };

    let status_label = Label::new(None);
    status_label.set_xalign(0.0);
    status_label.add_css_class("dim-label");
    let refresh_status: StatusCallback = {
        let status_label = status_label.clone();
        let document = document.clone();
        let active_color = active_color.clone();
        let active_stroke = active_stroke.clone();
        let state = state.clone();
        let image_width = base_image.width();
        let image_height = base_image.height();
        Rc::new(move |message: Option<String>| {
            let tool = document
                .try_borrow()
                .map(|document| document.active_tool)
                .unwrap_or(DefaultTool::Select);
            let config = state.config_snapshot();
            status_label.set_text(&editor_status_text(
                tool,
                active_color.get(),
                active_stroke.get(),
                image_width,
                image_height,
                &config.default_save_dir,
                message.as_deref(),
            ));
        })
    };

    let header = HeaderBar::new();
    root.append(&header);
    info!("present_editor header installed");

    let undo = action_icon_button("edit-undo-symbolic", "Undo");
    let redo = action_icon_button("edit-redo-symbolic", "Redo");
    let copy = action_icon_button("edit-copy-symbolic", "Copy to Clipboard");
    let pin = action_icon_button("view-pin-symbolic", "Pin Screenshot");
    let [save_label, save_to_label, copy_close_label] = output_action_menu_items();
    let save = action_text_button("document-save-symbolic", save_label, "Save");
    let save_close = action_text_button(
        "document-save-as-symbolic",
        output_action_primary_label(),
        "Save and Close",
    );
    let save_to = action_text_button("folder-save-symbolic", save_to_label, "Save to a folder");
    let copy_close = action_text_button("edit-copy-symbolic", copy_close_label, "Copy and Close");

    let overlay = Overlay::new();
    overlay.set_hexpand(true);
    overlay.set_vexpand(true);
    overlay.add_css_class("ashot-canvas-surface");
    let picture = Picture::for_filename(&image_path);
    picture.set_can_shrink(true);
    picture.set_halign(Align::Start);
    picture.set_valign(Align::Start);
    picture.set_size_request(scaled_width, scaled_height);
    overlay.set_child(Some(&picture));
    info!("present_editor picture added");

    let canvas = DrawingArea::new();
    canvas.set_content_width(scaled_width);
    canvas.set_content_height(scaled_height);
    canvas.set_halign(Align::Start);
    canvas.set_valign(Align::Start);
    overlay.add_overlay(&canvas);
    info!("present_editor canvas added");

    let text_entry = Entry::new();
    text_entry.set_width_chars(24);
    text_entry.set_halign(Align::Start);
    text_entry.set_valign(Align::Start);
    text_entry.set_visible(false);
    text_entry.add_css_class("osd");
    overlay.add_overlay(&text_entry);
    info!("present_editor text entry added");

    let canvas_frame = GtkBox::new(Orientation::Vertical, 0);
    canvas_frame.add_css_class("ashot-canvas-frame");
    canvas_frame.set_halign(Align::Start);
    canvas_frame.set_valign(Align::Start);
    canvas_frame.append(&overlay);

    let scrolled = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Automatic)
        .vscrollbar_policy(PolicyType::Automatic)
        .min_content_width(720)
        .min_content_height(420)
        .hexpand(true)
        .vexpand(true)
        .child(&canvas_frame)
        .build();
    scrolled.add_css_class("view");

    let editor_body = GtkBox::new(Orientation::Horizontal, 8);
    editor_body.set_hexpand(true);
    editor_body.set_vexpand(true);

    let left_panel = GtkBox::new(Orientation::Vertical, 10);
    left_panel.add_css_class("ashot-sidebar");
    left_panel.set_hexpand(false);
    left_panel.set_vexpand(true);
    left_panel.set_margin_top(4);
    left_panel.set_margin_bottom(4);
    left_panel.set_margin_start(4);
    left_panel.set_margin_end(4);

    left_panel.append(&section_title("Actions"));
    info!("present_editor left panel actions added");
    let quick_actions = GtkBox::new(Orientation::Horizontal, 6);
    quick_actions.append(&undo);
    quick_actions.append(&redo);
    quick_actions.append(&copy);
    quick_actions.append(&pin);
    left_panel.append(&quick_actions);

    let output_actions = GtkBox::new(Orientation::Horizontal, 6);
    save_close.set_hexpand(true);
    output_actions.append(&save_close);
    save_close.add_css_class("ashot-action-primary");
    let output_more = output_actions_menu(&save, &save_to, &copy_close);
    output_actions.append(&output_more);
    left_panel.append(&output_actions);
    update_history_action_buttons(&history, &undo, &redo);

    left_panel.append(&section_title("Tools"));
    let tool_grid = Grid::new();
    tool_grid.set_row_spacing(5);
    tool_grid.set_column_spacing(5);
    let tool_buttons = Rc::new(RefCell::new(Vec::<(DefaultTool, Button)>::new()));
    for (index, (label, tool)) in editor_tool_layout().iter().enumerate() {
        let button = Button::new();
        let icon = tool_icon_area(*tool);
        button.set_child(Some(&icon));
        button.set_tooltip_text(Some(label));
        button.set_hexpand(true);
        button.set_size_request(SIDEBAR_TOOL_BUTTON_WIDTH, SIDEBAR_TOOL_BUTTON_HEIGHT);
        button.add_css_class("ashot-tool-button");
        let document = document.clone();
        let tool_buttons_for_click = tool_buttons.clone();
        let canvas_for_tool = canvas.clone();
        let state_for_tool = state.clone();
        let refresh_status_for_tool = refresh_status.clone();
        let tool = *tool;
        button.connect_clicked(move |_| {
            if let Ok(mut document) = document.try_borrow_mut() {
                document.active_tool = tool;
            }
            update_tool_button_selection(&tool_buttons_for_click, tool);
            apply_editor_cursor(&canvas_for_tool, tool);
            persist_config_change(&state_for_tool, |config| {
                config.default_tool = tool;
            });
            refresh_status_for_tool(None);
        });
        tool_grid.attach(&button, (index % 3) as i32, (index / 3) as i32, 1, 1);
        tool_buttons.borrow_mut().push((tool, button));
    }
    left_panel.append(&tool_grid);
    info!("present_editor tool grid added");

    left_panel.append(&section_title("OCR"));
    let ocr_tool_button = Button::new();
    let ocr_tool_content = GtkBox::new(Orientation::Horizontal, 6);
    ocr_tool_content.set_halign(Align::Center);
    ocr_tool_content.append(&tool_icon_area(DefaultTool::Ocr));
    ocr_tool_content.append(&Label::new(Some("OCR")));
    ocr_tool_button.set_child(Some(&ocr_tool_content));
    ocr_tool_button.set_tooltip_text(Some("Recognize text from a selected region"));
    ocr_tool_button.set_hexpand(true);
    ocr_tool_button.set_size_request(0, SIDEBAR_TOOL_BUTTON_HEIGHT);
    ocr_tool_button.add_css_class("ashot-tool-button");
    let document_for_ocr_button = document.clone();
    let tool_buttons_for_ocr = tool_buttons.clone();
    let canvas_for_ocr_button = canvas.clone();
    let state_for_ocr_button = state.clone();
    let refresh_status_for_ocr = refresh_status.clone();
    ocr_tool_button.connect_clicked(move |_| {
        if let Ok(mut document) = document_for_ocr_button.try_borrow_mut() {
            document.active_tool = DefaultTool::Ocr;
        }
        update_tool_button_selection(&tool_buttons_for_ocr, DefaultTool::Ocr);
        apply_editor_cursor(&canvas_for_ocr_button, DefaultTool::Ocr);
        persist_config_change(&state_for_ocr_button, |config| {
            config.default_tool = DefaultTool::Ocr;
        });
        refresh_status_for_ocr(None);
    });
    tool_buttons.borrow_mut().push((DefaultTool::Ocr, ocr_tool_button.clone()));
    left_panel.append(&ocr_tool_button);
    let ocr_language_selector = ocr_language_menu(active_ocr_languages.clone(), state.clone());
    ocr_language_selector.add_css_class("ashot-compact-menu");
    left_panel.append(&ocr_language_selector);
    let ocr_filter_symbols_toggle = gtk::CheckButton::with_label("Filter emoji/symbols");
    ocr_filter_symbols_toggle.set_active(config.ocr_filter_symbols);
    ocr_filter_symbols_toggle
        .set_tooltip_text(Some("Remove emoji and decorative symbol noise from OCR text"));
    ocr_filter_symbols_toggle.add_css_class("ashot-compact-check");
    let active_ocr_filter_symbols_for_toggle = active_ocr_filter_symbols.clone();
    let state_for_ocr_filter = state.clone();
    ocr_filter_symbols_toggle.connect_toggled(move |button| {
        active_ocr_filter_symbols_for_toggle.set(button.is_active());
        persist_config_change(&state_for_ocr_filter, |config| {
            config.ocr_filter_symbols = button.is_active();
        });
    });
    left_panel.append(&ocr_filter_symbols_toggle);
    info!("present_editor ocr section added");
    update_tool_button_selection(&tool_buttons, config.default_tool);

    let stroke_previews = Rc::new(RefCell::new(Vec::<DrawingArea>::new()));
    let color_ui_refresh = Rc::new(RefCell::new(None::<ColorCallback>));
    let recent_color_recorder = Rc::new(RefCell::new(None::<ColorCallback>));
    let color_picker = color_picker_section(
        active_color.clone(),
        stroke_previews.clone(),
        document.clone(),
        tool_buttons.clone(),
        color_ui_refresh.clone(),
        recent_color_recorder.clone(),
        canvas.clone(),
        state.clone(),
        history.clone(),
        undo.clone(),
        redo.clone(),
        refresh_status.clone(),
        show_toast.clone(),
        queue_render_cache.clone(),
        active_magnifier_enabled.clone(),
        active_magnifier_zoom.clone(),
    );
    left_panel.append(&color_picker);
    info!("present_editor color section added");

    left_panel.append(&section_title("Stroke / Effect"));
    let stroke_menu = stroke_width_dropdown(
        active_stroke.clone(),
        active_color.clone(),
        stroke_previews.clone(),
        document.clone(),
        history.clone(),
        canvas.clone(),
        undo.clone(),
        redo.clone(),
        state.clone(),
        refresh_status.clone(),
        queue_render_cache.clone(),
    );
    left_panel.append(&stroke_menu);
    info!("present_editor stroke section added");

    let left_scrolled = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vscrollbar_policy(PolicyType::Automatic)
        .min_content_width(260)
        .max_content_width(260)
        .hexpand(false)
        .vexpand(true)
        .child(&left_panel)
        .build();

    editor_body.append(&left_scrolled);
    editor_body.append(&scrolled);
    root.append(&editor_body);
    info!("present_editor layout assembled");

    let doc_for_draw = document.clone();
    let draft_for_draw = draft.clone();
    let scale_for_draw = image_scale.clone();
    let brush_cursor_for_draw = brush_cursor_preview.clone();
    let active_color_for_draw = active_color.clone();
    let active_stroke_for_draw = active_stroke.clone();
    let active_magnifier_enabled_for_draw = active_magnifier_enabled.clone();
    let active_magnifier_zoom_for_draw = active_magnifier_zoom.clone();
    let base_rgba_for_magnifier = base_rgba.clone();
    canvas.set_draw_func(move |_, cr, draw_width, draw_height| {
        let _ = cr.save();
        let scale = scale_for_draw.get();
        cr.scale(scale, scale);
        cr.set_line_join(gtk::cairo::LineJoin::Round);
        cr.set_line_cap(gtk::cairo::LineCap::Round);
        if let Some(annotations) = annotation_snapshot_for_draw(&doc_for_draw) {
            for annotation in &annotations {
                draw_annotation(cr, annotation);
            }
        }
        if let Ok(document) = doc_for_draw.try_borrow()
            && let Some(bounds) = selected_annotation_bounds(&document)
        {
            draw_selection_overlay(cr, bounds);
        }
        if let Some(annotation) = draft_preview_for_draw(&draft_for_draw) {
            draw_annotation(cr, &annotation);
        }
        if let Ok(document) = doc_for_draw.try_borrow()
            && matches!(document.active_tool, DefaultTool::Brush | DefaultTool::Marker)
            && draft_for_draw.try_borrow().is_ok_and(|draft| draft.is_none())
            && let Some(point) = brush_cursor_for_draw.get()
        {
            draw_brush_cursor_preview(
                cr,
                point,
                active_color_for_draw.get(),
                active_stroke_for_draw.get(),
            );
        }
        let _ = cr.restore();
        if let Ok(document) = doc_for_draw.try_borrow()
            && let Some(point) = eyedropper_magnifier_point(
                document.active_tool,
                active_magnifier_enabled_for_draw.get(),
                brush_cursor_for_draw.get(),
                base_rgba_for_magnifier.width(),
                base_rgba_for_magnifier.height(),
            )
        {
            draw_eyedropper_magnifier(
                cr,
                &base_rgba_for_magnifier,
                point,
                Point::new(point.x * scale as f32, point.y * scale as f32),
                draw_width,
                draw_height,
                active_magnifier_zoom_for_draw.get(),
            );
        }
    });

    let click = gtk::GestureClick::new();
    click.set_button(1);
    let doc_for_click = document.clone();
    let draft_for_click = draft.clone();
    let history_for_click = history.clone();
    let image_for_click = base_image.clone();
    let canvas_for_click = canvas.clone();
    let color_for_click = active_color.clone();
    let color_ui_refresh_for_click = color_ui_refresh.clone();
    let recent_for_click = recent_color_recorder.clone();
    let undo_for_click = undo.clone();
    let redo_for_click = redo.clone();
    let scale_for_click = image_scale.clone();
    let text_entry_for_click = text_entry.clone();
    let active_text_edit_for_click = active_text_edit.clone();
    let queue_render_cache_for_click = queue_render_cache.clone();
    click.connect_pressed(move |_, _, x, y| {
        let point = scaled_canvas_point(x, y, scale_for_click.get());
        let Ok(document) = doc_for_click.try_borrow() else {
            return;
        };
        let active_tool = document.active_tool;
        drop(document);

        if tool_picks_canvas_color(active_tool) {
            let color = image_color_at(&image_for_click, point);
            color_for_click.set(color);
            if let Some(refresh) = color_ui_refresh_for_click.borrow().as_ref().cloned() {
                refresh(color);
            }
            canvas_for_click.queue_draw();
            return;
        }

        if active_tool == DefaultTool::Text {
            let Ok(document) = doc_for_click.try_borrow() else {
                return;
            };
            let existing = document.text_annotation_at(point);
            let initial_text =
                existing.and_then(|id| text_for_annotation(&document, id)).unwrap_or_default();
            drop(document);

            if !set_active_text_edit(
                &active_text_edit_for_click,
                ActiveTextEdit { id: existing, origin: point, color: color_for_click.get() },
            ) {
                return;
            }
            position_text_entry(&text_entry_for_click, x, y);
            text_entry_for_click.set_text(&initial_text);
            text_entry_for_click.set_visible(true);
            text_entry_for_click.grab_focus();
            text_entry_for_click.set_position(-1);
            return;
        }
        if active_tool == DefaultTool::Counter {
            let added = if let Ok(mut document) = doc_for_click.try_borrow_mut() {
                if let Ok(mut history) = history_for_click.try_borrow_mut() {
                    history.snapshot(&document.annotations);
                }
                let number = document.next_counter();
                document.add_annotation(Annotation::new(AnnotationData::Counter {
                    center: point,
                    number,
                    color: color_for_click.get(),
                    radius: 12,
                }));
                true
            } else {
                false
            };
            if added {
                if let Some(record_recent) = recent_for_click.borrow().as_ref().cloned() {
                    record_recent(color_for_click.get());
                }
                update_history_action_buttons(&history_for_click, &undo_for_click, &redo_for_click);
                queue_render_cache_for_click();
                canvas_for_click.queue_draw();
            }
            return;
        }

        if draft_tool_can_draw(active_tool) {
            return;
        }

        if tool_can_select_existing(active_tool) {
            let selected = doc_for_click
                .try_borrow_mut()
                .ok()
                .and_then(|mut document| document.select_at(point))
                .is_some();
            if selected {
                if let Ok(mut draft) = draft_for_click.try_borrow_mut() {
                    *draft = None;
                }
            }
        }
    });
    canvas.add_controller(click);
    info!("present_editor click controller added");

    let motion = gtk::EventControllerMotion::new();
    let doc_for_motion = document.clone();
    let canvas_for_motion = canvas.clone();
    let scale_for_motion = image_scale.clone();
    let brush_cursor_for_motion = brush_cursor_preview.clone();
    motion.connect_motion(move |_, x, y| {
        let point = scaled_canvas_point(x, y, scale_for_motion.get());
        brush_cursor_for_motion.set(Some(point));
        if let Ok(document) = doc_for_motion.try_borrow() {
            apply_canvas_hover_cursor(
                &canvas_for_motion,
                &document,
                point,
                SELECTION_HANDLE_HIT_TOLERANCE / (scale_for_motion.get().max(0.1) as f32),
            );
        }
        canvas_for_motion.queue_draw();
    });
    let doc_for_motion_leave = document.clone();
    let canvas_for_motion_leave = canvas.clone();
    let brush_cursor_for_leave = brush_cursor_preview.clone();
    motion.connect_leave(move |_| {
        brush_cursor_for_leave.set(None);
        if let Ok(document) = doc_for_motion_leave.try_borrow() {
            apply_editor_cursor(&canvas_for_motion_leave, document.active_tool);
        }
        canvas_for_motion_leave.queue_draw();
    });
    canvas.add_controller(motion);
    info!("present_editor motion controller added");

    let doc_for_text_commit = document.clone();
    let history_for_text_commit = history.clone();
    let canvas_for_text_commit = canvas.clone();
    let active_text_edit_for_commit = active_text_edit.clone();
    let undo_for_text_commit = undo.clone();
    let redo_for_text_commit = redo.clone();
    let recent_for_text_commit = recent_color_recorder.clone();
    let queue_render_cache_for_text_commit = queue_render_cache.clone();
    text_entry.connect_activate(move |entry| {
        commit_text_entry(
            entry,
            &doc_for_text_commit,
            &history_for_text_commit,
            &canvas_for_text_commit,
            &active_text_edit_for_commit,
            &undo_for_text_commit,
            &redo_for_text_commit,
            &recent_for_text_commit,
            &queue_render_cache_for_text_commit,
        );
    });

    let focus = gtk::EventControllerFocus::new();
    let doc_for_text_focus = document.clone();
    let history_for_text_focus = history.clone();
    let canvas_for_text_focus = canvas.clone();
    let active_text_edit_for_focus = active_text_edit.clone();
    let undo_for_text_focus = undo.clone();
    let redo_for_text_focus = redo.clone();
    let recent_for_text_focus = recent_color_recorder.clone();
    let queue_render_cache_for_text_focus = queue_render_cache.clone();
    focus.connect_leave(move |controller| {
        if let Some(entry) = controller.widget().and_then(|widget| widget.downcast::<Entry>().ok())
        {
            commit_text_entry(
                &entry,
                &doc_for_text_focus,
                &history_for_text_focus,
                &canvas_for_text_focus,
                &active_text_edit_for_focus,
                &undo_for_text_focus,
                &redo_for_text_focus,
                &recent_for_text_focus,
                &queue_render_cache_for_text_focus,
            );
        }
    });
    text_entry.add_controller(focus);
    info!("present_editor text focus controller added");

    let drag = gtk::GestureDrag::new();
    drag.set_button(1);
    let doc_for_drag = document.clone();
    let draft_for_drag = draft.clone();
    let moving_for_drag = moving.clone();
    let resizing_for_drag = resizing.clone();
    let history_for_drag = history.clone();
    let color_for_drag = active_color.clone();
    let color_ui_refresh_for_drag = color_ui_refresh.clone();
    let stroke_for_drag = active_stroke.clone();
    let image_for_drag = base_image.clone();
    let canvas_for_drag = canvas.clone();
    let scale_for_drag = image_scale.clone();
    let undo_for_drag = undo.clone();
    let redo_for_drag = redo.clone();
    drag.connect_drag_begin(move |_, x, y| {
        let point = scaled_canvas_point(x, y, scale_for_drag.get());
        let Some((annotations, tool, resize_handle)) = ({
            let Ok(document) = doc_for_drag.try_borrow() else {
                return;
            };
            let handle_tolerance =
                SELECTION_HANDLE_HIT_TOLERANCE / (scale_for_drag.get().max(0.1) as f32);
            Some((
                document.annotations.clone(),
                document.active_tool,
                resize_handle_at(&document, point, handle_tolerance),
            ))
        }) else {
            return;
        };

        if tool_picks_canvas_color(tool) {
            let color = image_color_at(&image_for_drag, point);
            color_for_drag.set(color);
            if let Some(refresh) = color_ui_refresh_for_drag.borrow().as_ref().cloned() {
                refresh(color);
            }
            canvas_for_drag.queue_draw();
            return;
        }

        if !tool_can_select_existing(tool) && !draft_tool_can_draw(tool) {
            return;
        }

        if tool_can_select_existing(tool) {
            if let Some(handle) = resize_handle {
                if let Ok(mut history) = history_for_drag.try_borrow_mut() {
                    history.snapshot(&annotations);
                }
                update_history_action_buttons(&history_for_drag, &undo_for_drag, &redo_for_drag);
                if let Ok(mut resizing) = resizing_for_drag.try_borrow_mut() {
                    *resizing = Some(handle);
                }
                return;
            }

            let selected = doc_for_drag
                .try_borrow_mut()
                .ok()
                .and_then(|mut document| document.select_at(point))
                .is_some();

            if selected {
                if let Ok(mut history) = history_for_drag.try_borrow_mut() {
                    history.snapshot(&annotations);
                }
                update_history_action_buttons(&history_for_drag, &undo_for_drag, &redo_for_drag);
                if let Ok(mut moving) = moving_for_drag.try_borrow_mut() {
                    *moving = Some(point);
                }
            }
            canvas_for_drag.queue_draw();
            return;
        }

        if let Ok(mut history) = history_for_drag.try_borrow_mut() {
            history.snapshot(&annotations);
        }
        update_history_action_buttons(&history_for_drag, &undo_for_drag, &redo_for_drag);

        if let Ok(mut draft) = draft_for_drag.try_borrow_mut() {
            *draft = Some(DraftAnnotation::new(
                tool,
                point,
                color_for_drag.get(),
                stroke_for_drag.get(),
            ));
        }
    });

    let doc_for_update = document.clone();
    let draft_for_update = draft.clone();
    let moving_for_update = moving.clone();
    let resizing_for_update = resizing.clone();
    let canvas_for_update = canvas.clone();
    let scale_for_update = image_scale.clone();
    drag.connect_drag_update(move |gesture, dx, dy| {
        let (start_x, start_y) = gesture.start_point().unwrap_or((0.0, 0.0));
        let current = scaled_canvas_point(start_x + dx, start_y + dy, scale_for_update.get());
        let resize_handle = resizing_for_update.try_borrow().ok().and_then(|handle| *handle);
        if let Some(handle) = resize_handle {
            if let Ok(mut document) = doc_for_update.try_borrow_mut() {
                document.resize_selected(handle, current);
            }
            canvas_for_update.queue_draw();
            return;
        }

        if let Some((delta_x, delta_y)) = moving_delta_and_update(&moving_for_update, current) {
            if let Ok(mut document) = doc_for_update.try_borrow_mut() {
                document.move_selected(delta_x, delta_y);
            }
            canvas_for_update.queue_draw();
            return;
        }

        let updated = if let Ok(mut draft_state) = draft_for_update.try_borrow_mut() {
            if let Some(draft) = draft_state.as_mut() {
                draft.extend(current);
                true
            } else {
                false
            }
        } else {
            false
        };
        if updated {
            canvas_for_update.queue_draw();
        }
    });

    let doc_for_end = document.clone();
    let draft_for_end = draft.clone();
    let moving_for_end = moving.clone();
    let resizing_for_end = resizing.clone();
    let canvas_for_end = canvas.clone();
    let state_for_ocr = state.clone();
    let runtime_for_ocr = runtime.clone();
    let image_for_ocr = base_image.clone();
    let window_for_ocr = window.clone();
    let active_ocr_languages_for_end = active_ocr_languages.clone();
    let active_ocr_filter_symbols_for_end = active_ocr_filter_symbols.clone();
    let refresh_status_for_ocr_end = refresh_status.clone();
    let show_toast_for_ocr_end = show_toast.clone();
    let recent_for_end = recent_color_recorder.clone();
    let queue_render_cache_for_end = queue_render_cache.clone();
    drag.connect_drag_end(move |_, _, _| {
        let was_moving = moving_for_end.try_borrow().ok().and_then(|moving| *moving).is_some();
        let was_resizing =
            resizing_for_end.try_borrow().ok().and_then(|resizing| *resizing).is_some();
        if let Ok(mut moving) = moving_for_end.try_borrow_mut() {
            *moving = None;
        }
        if let Ok(mut resizing) = resizing_for_end.try_borrow_mut() {
            *resizing = None;
        }
        let draft = draft_for_end.try_borrow_mut().ok().and_then(|mut draft| draft.take());
        let Some(draft) = draft else {
            if was_moving || was_resizing {
                queue_render_cache_for_end();
            }
            canvas_for_end.queue_draw();
            return;
        };

        if draft.tool == DefaultTool::Ocr {
            let ocr_languages = active_ocr_languages_for_end
                .try_borrow()
                .map(|languages| languages.clone())
                .unwrap_or_else(|_| default_ocr_languages());
            begin_ocr_request(
                &window_for_ocr,
                state_for_ocr.clone(),
                runtime_for_ocr.clone(),
                &image_for_ocr,
                draft.bounds(),
                ocr_languages,
                active_ocr_filter_symbols_for_end.get(),
                Some(refresh_status_for_ocr_end.clone()),
                Some(show_toast_for_ocr_end.clone()),
            );
            canvas_for_end.queue_draw();
            return;
        }

        let draft_color = draft.color;
        let annotation = draft.finish();
        if let Some(annotation) = annotation {
            let keep_selection = annotation_keeps_selection_after_creation(&annotation);
            if let Ok(mut document) = doc_for_end.try_borrow_mut() {
                document.add_annotation(annotation);
                if !keep_selection {
                    document.selected = None;
                }
            }
            if let Some(record_recent) = recent_for_end.borrow().as_ref().cloned() {
                record_recent(draft_color);
            }
            queue_render_cache_for_end();
        }
        canvas_for_end.queue_draw();
    });
    canvas.add_controller(drag);
    info!("present_editor drag controller added");

    apply_editor_cursor(&canvas, config.default_tool);

    let doc_for_undo = document.clone();
    let canvas_for_undo = canvas.clone();
    let history_for_undo = history.clone();
    let undo_for_undo = undo.clone();
    let redo_for_undo = redo.clone();
    let queue_render_cache_for_undo = queue_render_cache.clone();
    undo.connect_clicked(move |_| {
        let Some(current) = annotation_snapshot_for_draw(&doc_for_undo) else {
            return;
        };
        let previous =
            history_for_undo.try_borrow_mut().ok().and_then(|mut history| history.undo(&current));
        if let Some(previous) = previous {
            if let Ok(mut document) = doc_for_undo.try_borrow_mut() {
                document.annotations = previous;
                canvas_for_undo.queue_draw();
            }
            queue_render_cache_for_undo();
            update_history_action_buttons(&history_for_undo, &undo_for_undo, &redo_for_undo);
        }
    });

    let doc_for_redo = document.clone();
    let canvas_for_redo = canvas.clone();
    let history_for_redo = history.clone();
    let undo_for_redo = undo.clone();
    let redo_for_redo = redo.clone();
    let queue_render_cache_for_redo = queue_render_cache.clone();
    redo.connect_clicked(move |_| {
        let Some(current) = annotation_snapshot_for_draw(&doc_for_redo) else {
            return;
        };
        let next =
            history_for_redo.try_borrow_mut().ok().and_then(|mut history| history.redo(&current));
        if let Some(next) = next {
            if let Ok(mut document) = doc_for_redo.try_borrow_mut() {
                document.annotations = next;
                canvas_for_redo.queue_draw();
            }
            queue_render_cache_for_redo();
            update_history_action_buttons(&history_for_redo, &undo_for_redo, &redo_for_redo);
        }
    });

    let key_controller = gtk::EventControllerKey::new();
    let doc_for_keys = document.clone();
    let history_for_keys = history.clone();
    let canvas_for_keys = canvas.clone();
    let undo_for_keys = undo.clone();
    let redo_for_keys = redo.clone();
    let active_text_edit_for_keys = active_text_edit.clone();
    let queue_render_cache_for_keys = queue_render_cache.clone();
    key_controller.connect_key_pressed(move |_, key, _, modifiers| {
        if active_text_edit_for_keys.try_borrow().is_ok_and(|edit| edit.is_some()) {
            return glib::Propagation::Proceed;
        }

        let ctrl = modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK);
        let shift = modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK);
        let nudge = if shift { 10.0 } else { 1.0 };

        let changed = if let Ok(mut document) = doc_for_keys.try_borrow_mut() {
            let before = document.annotations.clone();
            let changed = match key {
                gtk::gdk::Key::Delete | gtk::gdk::Key::BackSpace => {
                    document.remove_selected().is_some()
                }
                gtk::gdk::Key::d | gtk::gdk::Key::D if ctrl => {
                    document.duplicate_selected(Point::new(12.0, 12.0)).is_some()
                }
                gtk::gdk::Key::Left => document.move_selected(-nudge, 0.0),
                gtk::gdk::Key::Right => document.move_selected(nudge, 0.0),
                gtk::gdk::Key::Up => document.move_selected(0.0, -nudge),
                gtk::gdk::Key::Down => document.move_selected(0.0, nudge),
                _ => false,
            };
            if changed && let Ok(mut history) = history_for_keys.try_borrow_mut() {
                history.snapshot(&before);
            }
            changed
        } else {
            false
        };

        if changed {
            update_history_action_buttons(&history_for_keys, &undo_for_keys, &redo_for_keys);
            queue_render_cache_for_keys();
            canvas_for_keys.queue_draw();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    window.add_controller(key_controller);

    let doc_for_save = document.clone();
    let state_for_save = state.clone();
    let path_for_save = image_path.clone();
    let render_cache_for_save = render_cache.clone();
    let runtime_for_save = runtime.clone();
    let refresh_status_for_save = refresh_status.clone();
    let show_toast_for_save = show_toast.clone();
    save.connect_clicked(move |button| {
        let config = state_for_save.config_snapshot();
        let initial_filename = suggested_save_filename_at(&config, chrono::Local::now());
        let doc_for_confirm = doc_for_save.clone();
        let state_for_confirm = state_for_save.clone();
        let path_for_confirm = path_for_save.clone();
        let render_cache_for_confirm = render_cache_for_save.clone();
        let runtime_for_confirm = runtime_for_save.clone();
        let refresh_status_for_confirm = refresh_status_for_save.clone();
        let show_toast_for_confirm = show_toast_for_save.clone();
        show_save_filename_popover(
            button,
            initial_filename,
            "Save",
            move |requested_filename, popover| {
                let config = state_for_confirm.config_snapshot();
                let Some(annotations) = annotation_snapshot_for_draw(&doc_for_confirm) else {
                    popover.finish_error("Save failed: editor is busy");
                    return;
                };
                refresh_status_for_confirm(Some("Saving...".to_string()));
                let save_dir = config.default_save_dir.clone();
                let auto_copy = config.auto_copy;
                let runtime_for_write = runtime_for_confirm.clone();
                let path_for_result = path_for_confirm.clone();
                let refresh_status_for_result = refresh_status_for_confirm.clone();
                let show_toast_for_result = show_toast_for_confirm.clone();
                let popover_for_result = popover.clone();
                let callback: RenderCacheCallback = Box::new(move |result| match result {
                    Ok(png_bytes) => {
                        let png_bytes_for_copy = png_bytes.clone();
                        save_png_bytes_to_dir_async(
                            runtime_for_write,
                            save_dir,
                            png_bytes,
                            requested_filename,
                            move |save_result| match save_result {
                                Ok(output) => {
                                    if auto_copy
                                        && let Err(error) =
                                            copy_png_bytes_to_clipboard(png_bytes_for_copy)
                                    {
                                        error!("{error}");
                                    }
                                    info!(
                                        "saved screenshot based on {}",
                                        path_for_result.display()
                                    );
                                    show_toast_for_result(&format!("Saved {}", output.display()));
                                    refresh_status_for_result(None);
                                    popover_for_result.finish_success();
                                }
                                Err(error) => {
                                    error!("{error}");
                                    show_toast_for_result(&format!("Save failed: {error}"));
                                    refresh_status_for_result(None);
                                    popover_for_result
                                        .finish_error(&format!("Save failed: {error}"));
                                }
                            },
                        );
                    }
                    Err(error) => {
                        error!("{error}");
                        show_toast_for_result(&format!("Save failed: {error}"));
                        refresh_status_for_result(None);
                        popover_for_result.finish_error(&format!("Save failed: {error}"));
                    }
                });
                if let Ok(mut render_cache) = render_cache_for_confirm.try_borrow_mut() {
                    render_cache.request_latest(annotations, callback);
                } else {
                    refresh_status_for_confirm(None);
                    popover.finish_error("Save failed: renderer is busy");
                }
            },
        );
    });

    let doc_for_save_close = document.clone();
    let state_for_save_close = state.clone();
    let render_cache_for_save_close = render_cache.clone();
    let runtime_for_save_close = runtime.clone();
    let window_for_save_close = window.clone();
    let app_for_save_close = app.clone();
    let refresh_status_for_save_close = refresh_status.clone();
    let show_toast_for_save_close = show_toast.clone();
    save_close.connect_clicked(move |button| {
        let config = state_for_save_close.config_snapshot();
        let initial_filename = suggested_save_filename_at(&config, chrono::Local::now());
        let doc_for_confirm = doc_for_save_close.clone();
        let state_for_confirm = state_for_save_close.clone();
        let render_cache_for_confirm = render_cache_for_save_close.clone();
        let runtime_for_confirm = runtime_for_save_close.clone();
        let window_for_confirm = window_for_save_close.clone();
        let app_for_confirm = app_for_save_close.clone();
        let refresh_status_for_confirm = refresh_status_for_save_close.clone();
        let show_toast_for_confirm = show_toast_for_save_close.clone();
        show_save_filename_popover(
            button,
            initial_filename,
            "Save",
            move |requested_filename, popover| {
                let config = state_for_confirm.config_snapshot();
                let Some(annotations) = annotation_snapshot_for_draw(&doc_for_confirm) else {
                    popover.finish_error("Save failed: editor is busy");
                    return;
                };
                refresh_status_for_confirm(Some("Saving...".to_string()));
                let save_dir = config.default_save_dir.clone();
                let auto_copy = config.auto_copy;
                let pin_after_save = config.pin_after_save;
                let runtime_for_write = runtime_for_confirm.clone();
                let state_for_result = state_for_confirm.clone();
                let app_for_result = app_for_confirm.clone();
                let window_for_result = window_for_confirm.clone();
                let refresh_status_for_result = refresh_status_for_confirm.clone();
                let show_toast_for_result = show_toast_for_confirm.clone();
                let popover_for_result = popover.clone();
                let callback: RenderCacheCallback = Box::new(move |result| match result {
                    Ok(png_bytes) => {
                        let png_bytes_for_copy = png_bytes.clone();
                        save_png_bytes_to_dir_async(
                            runtime_for_write,
                            save_dir,
                            png_bytes,
                            requested_filename,
                            move |save_result| match save_result {
                                Ok(output) => {
                                    if auto_copy
                                        && let Err(error) =
                                            copy_png_bytes_to_clipboard(png_bytes_for_copy)
                                    {
                                        error!("{error}");
                                    }
                                    if pin_after_save {
                                        present_pin_window(
                                            &app_for_result,
                                            state_for_result,
                                            output,
                                        );
                                    }
                                    show_toast_for_result("Saved");
                                    refresh_status_for_result(None);
                                    popover_for_result.finish_success();
                                    window_for_result.close();
                                }
                                Err(error) => {
                                    error!("{error}");
                                    show_toast_for_result(&format!("Save failed: {error}"));
                                    refresh_status_for_result(None);
                                    popover_for_result
                                        .finish_error(&format!("Save failed: {error}"));
                                }
                            },
                        );
                    }
                    Err(error) => {
                        error!("{error}");
                        show_toast_for_result(&format!("Save failed: {error}"));
                        refresh_status_for_result(None);
                        popover_for_result.finish_error(&format!("Save failed: {error}"));
                    }
                });
                if let Ok(mut render_cache) = render_cache_for_confirm.try_borrow_mut() {
                    render_cache.request_latest(annotations, callback);
                } else {
                    refresh_status_for_confirm(None);
                    popover.finish_error("Save failed: renderer is busy");
                }
            },
        );
    });

    let doc_for_save_to = document.clone();
    let state_for_save_to = state.clone();
    let path_for_save_to = image_path.clone();
    let render_cache_for_save_to = render_cache.clone();
    let runtime_for_save_to = runtime.clone();
    let window_for_save_to = window.clone();
    let refresh_status_for_save_to = refresh_status.clone();
    let show_toast_for_save_to = show_toast.clone();
    save_to.connect_clicked(move |button| {
        let config = state_for_save_to.config_snapshot();
        let initial_filename = suggested_save_filename_at(&config, chrono::Local::now());
        let dialog = gtk::FileChooserNative::new(
            Some("Choose Save Folder"),
            Some(&window_for_save_to),
            gtk::FileChooserAction::SelectFolder,
            Some("Select"),
            Some("Cancel"),
        );
        let initial_folder = gio::File::for_path(&config.default_save_dir);
        let _ = dialog.set_current_folder(Some(&initial_folder));

        let button_for_folder = button.clone();
        let doc_for_folder = doc_for_save_to.clone();
        let state_for_folder = state_for_save_to.clone();
        let path_for_folder = path_for_save_to.clone();
        let render_cache_for_folder = render_cache_for_save_to.clone();
        let runtime_for_folder = runtime_for_save_to.clone();
        let refresh_status_for_folder = refresh_status_for_save_to.clone();
        let show_toast_for_folder = show_toast_for_save_to.clone();
        dialog.run_async(move |dialog, response| {
            if response != gtk::ResponseType::Accept {
                refresh_status_for_folder(None);
                dialog.destroy();
                return;
            }
            let Some(folder) = dialog.file() else {
                dialog.destroy();
                error!("no save folder was selected");
                show_toast_for_folder("Save failed: no folder selected");
                return;
            };
            dialog.destroy();
            let Some(folder_path) = folder.path() else {
                error!("selected save folder is not a local path");
                show_toast_for_folder("Save failed: selected folder is not local");
                return;
            };

            let doc_for_confirm = doc_for_folder.clone();
            let state_for_confirm = state_for_folder.clone();
            let path_for_confirm = path_for_folder.clone();
            let render_cache_for_confirm = render_cache_for_folder.clone();
            let runtime_for_confirm = runtime_for_folder.clone();
            let refresh_status_for_confirm = refresh_status_for_folder.clone();
            let show_toast_for_confirm = show_toast_for_folder.clone();
            show_save_filename_popover(
                &button_for_folder,
                initial_filename,
                "Save",
                move |requested_filename, popover| {
                    let mut config = state_for_confirm.config_snapshot();
                    let Some(annotations) = annotation_snapshot_for_draw(&doc_for_confirm) else {
                        popover.finish_error("Save failed: editor is busy");
                        return;
                    };
                    refresh_status_for_confirm(Some("Saving...".to_string()));
                    let auto_copy = config.auto_copy;
                    let save_dir = folder_path.clone();
                    let runtime_for_write = runtime_for_confirm.clone();
                    let state_for_result = state_for_confirm.clone();
                    let path_for_result = path_for_confirm.clone();
                    let refresh_status_for_result = refresh_status_for_confirm.clone();
                    let show_toast_for_result = show_toast_for_confirm.clone();
                    let popover_for_result = popover.clone();
                    let callback: RenderCacheCallback = Box::new(move |result| match result {
                        Ok(png_bytes) => {
                            let png_bytes_for_copy = png_bytes.clone();
                            save_png_bytes_to_dir_async(
                                runtime_for_write,
                                save_dir.clone(),
                                png_bytes,
                                requested_filename,
                                move |save_result| match save_result {
                                    Ok(output) => {
                                        config.default_save_dir = save_dir.clone();
                                        if auto_copy
                                            && let Err(error) =
                                                copy_png_bytes_to_clipboard(png_bytes_for_copy)
                                        {
                                            error!("{error}");
                                        }
                                        let _ = config.save();
                                        state_for_result.update_config(config);
                                        info!(
                                            "saved screenshot based on {} to {}",
                                            path_for_result.display(),
                                            output.display()
                                        );
                                        show_toast_for_result(&format!(
                                            "Saved {}",
                                            output.display()
                                        ));
                                        refresh_status_for_result(None);
                                        popover_for_result.finish_success();
                                    }
                                    Err(error) => {
                                        error!("{error}");
                                        show_toast_for_result(&format!("Save failed: {error}"));
                                        refresh_status_for_result(None);
                                        popover_for_result
                                            .finish_error(&format!("Save failed: {error}"));
                                    }
                                },
                            );
                        }
                        Err(error) => {
                            error!("{error}");
                            show_toast_for_result(&format!("Save failed: {error}"));
                            refresh_status_for_result(None);
                            popover_for_result.finish_error(&format!("Save failed: {error}"));
                        }
                    });
                    if let Ok(mut render_cache) = render_cache_for_confirm.try_borrow_mut() {
                        render_cache.request_latest(annotations, callback);
                    } else {
                        refresh_status_for_confirm(None);
                        popover.finish_error("Save failed: renderer is busy");
                    }
                },
            );
        });
    });

    let doc_for_copy = document.clone();
    let render_cache_for_copy = render_cache.clone();
    let refresh_status_for_copy = refresh_status.clone();
    let show_toast_for_copy = show_toast.clone();
    copy.connect_clicked(move |button| {
        set_button_loading(button, true);
        let Some(annotations) = annotation_snapshot_for_draw(&doc_for_copy) else {
            set_button_loading(button, false);
            return;
        };
        refresh_status_for_copy(Some("Copying...".to_string()));
        let button_for_result = button.clone();
        let refresh_status_for_result = refresh_status_for_copy.clone();
        let show_toast_for_result = show_toast_for_copy.clone();
        let callback: RenderCacheCallback = Box::new(move |result| {
            match result.and_then(copy_png_bytes_to_clipboard) {
                Ok(()) => show_toast_for_result("Copied"),
                Err(error) => show_toast_for_result(&format!("Copy failed: {error}")),
            }
            refresh_status_for_result(None);
            set_button_loading(&button_for_result, false);
        });
        if let Ok(mut render_cache) = render_cache_for_copy.try_borrow_mut() {
            render_cache.request_latest(annotations, callback);
        } else {
            show_toast_for_copy("Copy failed: renderer is busy");
            refresh_status_for_copy(None);
            set_button_loading(button, false);
        }
    });

    let doc_for_copy_close = document.clone();
    let render_cache_for_copy_close = render_cache.clone();
    let window_for_copy_close = window.clone();
    let refresh_status_for_copy_close = refresh_status.clone();
    let show_toast_for_copy_close = show_toast.clone();
    copy_close.connect_clicked(move |button| {
        set_button_loading(button, true);
        let Some(annotations) = annotation_snapshot_for_draw(&doc_for_copy_close) else {
            set_button_loading(button, false);
            return;
        };
        refresh_status_for_copy_close(Some("Copying...".to_string()));
        let button_for_result = button.clone();
        let window_for_result = window_for_copy_close.clone();
        let refresh_status_for_result = refresh_status_for_copy_close.clone();
        let show_toast_for_result = show_toast_for_copy_close.clone();
        let callback: RenderCacheCallback =
            Box::new(move |result| match result.and_then(copy_png_bytes_to_clipboard) {
                Ok(()) => window_for_result.close(),
                Err(error) => {
                    error!("{error}");
                    show_toast_for_result(&format!("Copy failed: {error}"));
                    refresh_status_for_result(None);
                    set_button_loading(&button_for_result, false);
                }
            });
        if let Ok(mut render_cache) = render_cache_for_copy_close.try_borrow_mut() {
            render_cache.request_latest(annotations, callback);
        } else {
            show_toast_for_copy_close("Copy failed: renderer is busy");
            refresh_status_for_copy_close(None);
            set_button_loading(button, false);
        }
    });

    let doc_for_pin = document.clone();
    let render_cache_for_pin = render_cache.clone();
    let runtime_for_pin = runtime.clone();
    let app_for_pin = app.clone();
    let state_for_pin = state.clone();
    let refresh_status_for_pin = refresh_status.clone();
    let show_toast_for_pin = show_toast.clone();
    pin.connect_clicked(move |button| {
        set_button_loading(button, true);
        let Some(annotations) = annotation_snapshot_for_draw(&doc_for_pin) else {
            set_button_loading(button, false);
            return;
        };
        let temp_filename = unique_temp_png_filename("ashot_pin");
        let runtime_for_write = runtime_for_pin.clone();
        let app_for_result = app_for_pin.clone();
        let state_for_result = state_for_pin.clone();
        let button_for_result = button.clone();
        let refresh_status_for_result = refresh_status_for_pin.clone();
        let show_toast_for_result = show_toast_for_pin.clone();
        refresh_status_for_pin(Some("Pinning...".to_string()));
        let callback: RenderCacheCallback = Box::new(move |result| match result {
            Ok(png_bytes) => {
                save_png_bytes_to_dir_async(
                    runtime_for_write,
                    std::env::temp_dir(),
                    png_bytes,
                    temp_filename,
                    move |save_result| {
                        match save_result {
                            Ok(output) => {
                                present_pin_window(&app_for_result, state_for_result, output);
                                show_toast_for_result("Pinned");
                            }
                            Err(error) => {
                                error!("{error}");
                                show_toast_for_result(&format!("Pin failed: {error}"));
                            }
                        }
                        refresh_status_for_result(None);
                        set_button_loading(&button_for_result, false);
                    },
                );
            }
            Err(error) => {
                error!("{error}");
                show_toast_for_result(&format!("Pin failed: {error}"));
                refresh_status_for_result(None);
                set_button_loading(&button_for_result, false);
            }
        });
        if let Ok(mut render_cache) = render_cache_for_pin.try_borrow_mut() {
            render_cache.request_latest(annotations, callback);
        } else {
            show_toast_for_pin("Pin failed: renderer is busy");
            refresh_status_for_pin(None);
            set_button_loading(button, false);
        }
    });

    refresh_status(None);
    root.append(&status_label);
    window.set_content(Some(&toast_overlay));
    window.present();
    info!("present_editor done: {}", image_path.display());
    Ok(())
}

fn present_pin_window(app: &adw::Application, state: Arc<ServiceState>, image_path: PathBuf) {
    let (image_width, image_height) = image::image_dimensions(&image_path).unwrap_or((480, 320));
    let (window_width, window_height) = pin_window_size(image_width, image_height);
    let config = state.config_snapshot();
    let initial_scale = pin_initial_scale_with_saved(
        image_width,
        image_height,
        window_width,
        window_height,
        config.last_pin_scale,
    );

    let window = ApplicationWindow::builder()
        .application(app)
        .title("Pinned Screenshot")
        .default_width(window_width)
        .default_height(window_height)
        .build();
    window.set_resizable(true);
    window.set_opacity(config.last_pin_opacity.clamp(0.35, 1.0));

    let image_overlay = Overlay::new();
    image_overlay.set_halign(Align::Start);
    image_overlay.set_valign(Align::Start);
    let picture = Picture::for_filename(&image_path);
    picture.set_can_shrink(true);
    picture.set_halign(Align::Start);
    picture.set_valign(Align::Start);
    image_overlay.set_child(Some(&picture));

    let dimension_label = Label::new(None);
    dimension_label.add_css_class("osd");
    dimension_label.add_css_class("ashot-osd");
    dimension_label.set_halign(Align::Start);
    dimension_label.set_valign(Align::Start);
    dimension_label.set_margin_top(8);
    dimension_label.set_margin_start(8);
    dimension_label.set_can_target(false);
    image_overlay.add_overlay(&dimension_label);

    let scale = Rc::new(Cell::new(initial_scale));
    apply_pin_zoom(
        &window,
        &image_overlay,
        &picture,
        &dimension_label,
        image_width,
        image_height,
        scale.get(),
    );

    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
    let window_for_scroll = window.clone();
    let image_overlay_for_scroll = image_overlay.clone();
    let picture_for_scroll = picture.clone();
    let dimension_label_for_scroll = dimension_label.clone();
    let scale_for_scroll = scale.clone();
    let state_for_scroll = state.clone();
    scroll.connect_scroll(move |_, _, dy| {
        let next = pin_zoom_from_scroll(scale_for_scroll.get(), dy);
        scale_for_scroll.set(next);
        persist_config_change(&state_for_scroll, |config| {
            config.last_pin_scale = next;
        });
        apply_pin_zoom(
            &window_for_scroll,
            &image_overlay_for_scroll,
            &picture_for_scroll,
            &dimension_label_for_scroll,
            image_width,
            image_height,
            next,
        );
        glib::Propagation::Stop
    });
    image_overlay.add_controller(scroll);

    let click = gtk::GestureClick::new();
    click.set_button(0);
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    let window_for_click = window.clone();
    let image_overlay_for_click = image_overlay.clone();
    let state_for_click = state.clone();
    let image_path_for_click = image_path.clone();
    click.connect_pressed(move |gesture, n_press, x, y| {
        let button = gesture.current_button();
        if pin_click_action(n_press, button) == Some(PinClickAction::Close) {
            window_for_click.close();
            return;
        }
        if n_press == 1 && button == 1 {
            if let Some(edge) = pin_resize_edge_at(
                x,
                y,
                image_overlay_for_click.allocated_width(),
                image_overlay_for_click.allocated_height(),
                8.0,
            ) {
                begin_pin_window_resize(&window_for_click, gesture, edge, x, y);
                return;
            }
            begin_pin_window_move(&window_for_click, gesture, x, y);
            return;
        }
        if pin_click_action(n_press, button) == Some(PinClickAction::Menu) {
            show_pin_context_popover(
                &image_overlay_for_click,
                &window_for_click,
                state_for_click.clone(),
                image_path_for_click.clone(),
                x,
                y,
            );
        }
    });
    image_overlay.add_controller(click);

    let pin_motion = gtk::EventControllerMotion::new();
    let image_overlay_for_motion = image_overlay.clone();
    pin_motion.connect_motion(move |_, x, y| {
        if let Some(edge) = pin_resize_edge_at(
            x,
            y,
            image_overlay_for_motion.allocated_width(),
            image_overlay_for_motion.allocated_height(),
            8.0,
        ) {
            image_overlay_for_motion.set_cursor_from_name(Some(cursor_name_for_surface_edge(edge)));
        } else {
            image_overlay_for_motion.set_cursor_from_name(Some("move"));
        }
    });
    let image_overlay_for_leave = image_overlay.clone();
    pin_motion.connect_leave(move |_| {
        image_overlay_for_leave.set_cursor(None);
    });
    image_overlay.add_controller(pin_motion);

    window.set_content(Some(&image_overlay));
    window.present();
}

fn action_icon_button(icon_name: &str, tooltip: &str) -> Button {
    let button = Button::from_icon_name(icon_name);
    button.set_tooltip_text(Some(tooltip));
    button.set_size_request(SIDEBAR_ACTION_BUTTON_WIDTH, SIDEBAR_ACTION_BUTTON_HEIGHT);
    button.add_css_class("flat");
    button.add_css_class("ashot-action-button");
    button
}

fn action_text_button(icon_name: &str, label: &str, tooltip: &str) -> Button {
    let button = Button::new();
    let content = GtkBox::new(Orientation::Horizontal, 6);
    content.set_halign(Align::Center);
    content.append(&gtk::Image::from_icon_name(icon_name));
    content.append(&Label::new(Some(label)));
    button.set_child(Some(&content));
    button.set_tooltip_text(Some(tooltip));
    button.set_size_request(0, SIDEBAR_ACTION_BUTTON_HEIGHT);
    button.add_css_class("ashot-action-button");
    button
}

fn output_action_primary_label() -> &'static str {
    "Done"
}

fn output_action_menu_items() -> [&'static str; 3] {
    ["Save", "Save To", "Copy & Close"]
}

fn output_actions_menu(save: &Button, save_to: &Button, copy_close: &Button) -> MenuButton {
    let menu = MenuButton::new();
    menu.set_tooltip_text(Some("More save actions"));
    menu.set_size_request(SIDEBAR_ACTION_BUTTON_WIDTH, SIDEBAR_ACTION_BUTTON_HEIGHT);
    menu.add_css_class("flat");
    menu.add_css_class("ashot-action-button");
    menu.add_css_class("ashot-output-more");
    menu.set_icon_name("open-menu-symbolic");

    let popover = Popover::new();
    let list = GtkBox::new(Orientation::Vertical, 4);
    list.add_css_class("ashot-output-menu");
    list.set_margin_top(6);
    list.set_margin_bottom(6);
    list.set_margin_start(6);
    list.set_margin_end(6);

    for button in [save, save_to, copy_close] {
        button.set_hexpand(true);
        button.add_css_class("flat");
        list.append(button);
    }

    popover.set_child(Some(&list));
    menu.set_popover(Some(&popover));
    menu
}

fn set_button_loading(button: &Button, loading: bool) {
    button.set_sensitive(!loading);
    if loading {
        button.add_css_class("ashot-loading");
    } else {
        button.remove_css_class("ashot-loading");
    }
}

fn pin_window_size(image_width: u32, image_height: u32) -> (i32, i32) {
    let scale = fit_scale(image_width, image_height, 900, 640);
    let (display_width, display_height) = pin_display_size(image_width, image_height, scale);
    (display_width.clamp(280, 900), display_height.clamp(180, 640))
}

fn pin_window_size_for_scale(image_width: u32, image_height: u32, scale: f64) -> (i32, i32) {
    pin_display_size(image_width, image_height, scale)
}

fn pin_initial_scale(
    image_width: u32,
    image_height: u32,
    window_width: i32,
    window_height: i32,
) -> f64 {
    fit_scale(image_width, image_height, window_width, window_height)
}

fn pin_initial_scale_with_saved(
    image_width: u32,
    image_height: u32,
    window_width: i32,
    window_height: i32,
    saved_scale: f64,
) -> f64 {
    if (saved_scale - 1.0).abs() > f64::EPSILON {
        saved_scale.clamp(0.1, 8.0)
    } else {
        pin_initial_scale(image_width, image_height, window_width, window_height)
    }
}

fn pin_zoom_from_scroll(current: f64, dy: f64) -> f64 {
    let factor = if dy < 0.0 {
        1.1
    } else if dy > 0.0 {
        1.0 / 1.1
    } else {
        1.0
    };
    (current * factor).clamp(0.1, 8.0)
}

fn pin_display_size(image_width: u32, image_height: u32, scale: f64) -> (i32, i32) {
    let scale = scale.clamp(0.1, 8.0);
    (
        (image_width.max(1) as f64 * scale).round().max(1.0) as i32,
        (image_height.max(1) as f64 * scale).round().max(1.0) as i32,
    )
}

fn pin_dimension_label(
    _image_width: u32,
    _image_height: u32,
    display_width: i32,
    display_height: i32,
    scale: f64,
) -> String {
    format!("{} x {} px · {:.0}%", display_width, display_height, (scale * 100.0).round())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PinClickAction {
    Move,
    Close,
    Menu,
}

fn pin_click_action(n_press: i32, button: u32) -> Option<PinClickAction> {
    if button == 3 && n_press == 1 {
        return Some(PinClickAction::Menu);
    }
    if button == 1 {
        return match n_press {
            1 => Some(PinClickAction::Move),
            2 => Some(PinClickAction::Close),
            _ => None,
        };
    }
    None
}

fn pin_context_popover_rect(x: f64, y: f64) -> (i32, i32, i32, i32) {
    fn coordinate(value: f64) -> i32 {
        if value.is_finite() { value.round().clamp(0.0, i32::MAX as f64) as i32 } else { 0 }
    }

    (coordinate(x), coordinate(y), 1, 1)
}

fn pin_resize_edge_at(
    x: f64,
    y: f64,
    width: i32,
    height: i32,
    tolerance: f64,
) -> Option<gtk::gdk::SurfaceEdge> {
    let width = width.max(1) as f64;
    let height = height.max(1) as f64;
    let west = x <= tolerance;
    let east = x >= width - tolerance;
    let north = y <= tolerance;
    let south = y >= height - tolerance;
    match (north, east, south, west) {
        (true, false, false, true) => Some(gtk::gdk::SurfaceEdge::NorthWest),
        (true, true, false, false) => Some(gtk::gdk::SurfaceEdge::NorthEast),
        (false, true, true, false) => Some(gtk::gdk::SurfaceEdge::SouthEast),
        (false, false, true, true) => Some(gtk::gdk::SurfaceEdge::SouthWest),
        (true, false, false, false) => Some(gtk::gdk::SurfaceEdge::North),
        (false, true, false, false) => Some(gtk::gdk::SurfaceEdge::East),
        (false, false, true, false) => Some(gtk::gdk::SurfaceEdge::South),
        (false, false, false, true) => Some(gtk::gdk::SurfaceEdge::West),
        _ => None,
    }
}

fn begin_pin_window_move(window: &ApplicationWindow, gesture: &gtk::GestureClick, x: f64, y: f64) {
    let Some(device) = gesture.current_event_device() else {
        return;
    };
    let Some(surface) = window.surface() else {
        return;
    };
    let Ok(toplevel) = surface.downcast::<gtk::gdk::Toplevel>() else {
        return;
    };

    toplevel.begin_move(
        &device,
        gesture.current_button() as i32,
        x,
        y,
        gesture.current_event_time(),
    );
}

fn begin_pin_window_resize(
    window: &ApplicationWindow,
    gesture: &gtk::GestureClick,
    edge: gtk::gdk::SurfaceEdge,
    x: f64,
    y: f64,
) {
    let device = gesture.current_event_device();
    let Some(surface) = window.surface() else {
        return;
    };
    let Ok(toplevel) = surface.downcast::<gtk::gdk::Toplevel>() else {
        return;
    };

    toplevel.begin_resize(
        edge,
        device.as_ref(),
        gesture.current_button() as i32,
        x,
        y,
        gesture.current_event_time(),
    );
}

fn cursor_name_for_surface_edge(edge: gtk::gdk::SurfaceEdge) -> &'static str {
    match edge {
        gtk::gdk::SurfaceEdge::NorthWest | gtk::gdk::SurfaceEdge::SouthEast => "nwse-resize",
        gtk::gdk::SurfaceEdge::NorthEast | gtk::gdk::SurfaceEdge::SouthWest => "nesw-resize",
        gtk::gdk::SurfaceEdge::North | gtk::gdk::SurfaceEdge::South => "ns-resize",
        gtk::gdk::SurfaceEdge::East | gtk::gdk::SurfaceEdge::West => "ew-resize",
        _ => "move",
    }
}

fn show_pin_context_popover(
    anchor: &Overlay,
    window: &ApplicationWindow,
    state: Arc<ServiceState>,
    image_path: PathBuf,
    x: f64,
    y: f64,
) {
    let popover = Popover::new();
    popover.set_parent(anchor);
    let (x, y, width, height) = pin_context_popover_rect(x, y);
    let pointing_to = gtk::gdk::Rectangle::new(x, y, width, height);
    popover.set_pointing_to(Some(&pointing_to));

    let root = GtkBox::new(Orientation::Vertical, 6);
    root.set_margin_top(8);
    root.set_margin_bottom(8);
    root.set_margin_start(8);
    root.set_margin_end(8);

    let copy = pin_menu_button("edit-copy-symbolic", "Copy");
    let save = pin_menu_button("document-save-symbolic", "Save");
    let opacity_100 = pin_menu_button("view-visible-symbolic", "Opacity 100%");
    let opacity_85 = pin_menu_button("view-visible-symbolic", "Opacity 85%");
    let opacity_70 = pin_menu_button("view-visible-symbolic", "Opacity 70%");
    let always_on_top = pin_menu_button("view-pin-symbolic", "Always on Top");
    always_on_top.set_sensitive(false);
    always_on_top.set_tooltip_text(Some("Not reliably supported by GTK4 on Wayland"));
    let close = pin_menu_button("window-close-symbolic", "Close");
    close.add_css_class("destructive-action");

    for button in [&copy, &save, &opacity_100, &opacity_85, &opacity_70, &always_on_top, &close] {
        button.set_hexpand(true);
        root.append(button);
    }

    let image_for_copy = image_path.clone();
    copy.connect_clicked(move |_| {
        copy_image_to_clipboard(&image_for_copy);
    });

    let image_for_save = image_path.clone();
    let state_for_save = state.clone();
    save.connect_clicked(move |button| {
        let config = state_for_save.config_snapshot();
        let initial_filename = suggested_save_filename_at(&config, chrono::Local::now());
        let image_for_confirm = image_for_save.clone();
        let state_for_confirm = state_for_save.clone();
        show_save_filename_popover(
            button,
            initial_filename,
            "Save",
            move |requested_filename, popover| {
                let config = state_for_confirm.config_snapshot();
                match save_pinned_image_with_filename(
                    &config,
                    &image_for_confirm,
                    &requested_filename,
                ) {
                    Ok(output) => {
                        info!("saved pinned screenshot to {}", output.display());
                        popover.finish_success();
                    }
                    Err(error) => {
                        error!("{error}");
                        popover.finish_error(&format!("Save failed: {error}"));
                    }
                }
            },
        );
    });

    for (button, opacity) in [(&opacity_100, 1.0), (&opacity_85, 0.85), (&opacity_70, 0.70)] {
        let window = window.clone();
        let state = state.clone();
        button.connect_clicked(move |_| {
            window.set_opacity(opacity);
            persist_config_change(&state, |config| {
                config.last_pin_opacity = opacity;
            });
        });
    }

    let window_for_close = window.clone();
    close.connect_clicked(move |_| {
        window_for_close.close();
    });

    popover.set_child(Some(&root));
    popover.popup();
}

fn pin_menu_button(icon_name: &str, label: &str) -> Button {
    let button = Button::new();
    let content = GtkBox::new(Orientation::Horizontal, 8);
    content.set_halign(Align::Start);
    content.append(&gtk::Image::from_icon_name(icon_name));
    content.append(&Label::new(Some(label)));
    button.set_child(Some(&content));
    button.add_css_class("flat");
    button
}

fn save_pinned_image_with_filename(
    config: &AppConfig,
    image_path: &Path,
    requested_filename: &str,
) -> std::result::Result<PathBuf, String> {
    let filename = normalized_save_filename(requested_filename)
        .ok_or_else(|| "Enter a file name".to_string())?;
    fs::create_dir_all(&config.default_save_dir).map_err(|source| {
        format!(
            "failed to create screenshot directory {}: {source}",
            config.default_save_dir.display()
        )
    })?;
    let output = config.default_save_dir.join(filename);
    fs::copy(image_path, &output).map_err(|source| {
        format!("failed to save pinned screenshot at {}: {source}", output.display())
    })?;
    Ok(output)
}

fn apply_pin_zoom(
    window: &ApplicationWindow,
    image_overlay: &Overlay,
    picture: &Picture,
    dimension_label: &Label,
    image_width: u32,
    image_height: u32,
    scale: f64,
) {
    let (display_width, display_height) = pin_display_size(image_width, image_height, scale);
    let (window_width, window_height) = pin_window_size_for_scale(image_width, image_height, scale);
    window.set_default_size(window_width, window_height);
    image_overlay.set_size_request(display_width, display_height);
    picture.set_size_request(display_width, display_height);
    dimension_label.set_text(&pin_dimension_label(
        image_width,
        image_height,
        display_width,
        display_height,
        scale,
    ));
    window.queue_resize();
}

fn section_title(text: &str) -> Label {
    let title = Label::new(Some(text));
    title.add_css_class("heading");
    title.add_css_class("ashot-section-title");
    title.set_xalign(0.0);
    title
}

fn tool_status_name(tool: DefaultTool) -> &'static str {
    match tool {
        DefaultTool::Select => "Select",
        DefaultTool::Text => "Text",
        DefaultTool::Line => "Line",
        DefaultTool::Arrow => "Arrow",
        DefaultTool::Brush => "Pencil",
        DefaultTool::Rectangle => "Rect",
        DefaultTool::Ellipse => "Circle",
        DefaultTool::Marker => "Marker",
        DefaultTool::Mosaic => "Pixel",
        DefaultTool::Blur => "Blur",
        DefaultTool::Counter => "Count",
        DefaultTool::FilledBox => "Fill",
        DefaultTool::ColorPicker => "Color Picker",
        DefaultTool::Ocr => "OCR",
    }
}

fn editor_status_text(
    tool: DefaultTool,
    color: Color,
    stroke_width: u32,
    image_width: u32,
    image_height: u32,
    save_dir: &Path,
    message: Option<&str>,
) -> String {
    let mut text = format!(
        "Tool: {} · Color: {} · Width: {}px · Image: {} x {} · Path: {}",
        tool_status_name(tool),
        color_to_hex(color),
        stroke_width,
        image_width,
        image_height,
        save_dir.display()
    );
    if let Some(message) = message.filter(|message| !message.trim().is_empty()) {
        text.push_str(" · ");
        text.push_str(message.trim());
    }
    text
}

fn install_editor_css() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_data(
        r#"
        .ashot-sidebar {
            padding: 6px;
            background: alpha(@view_bg_color, 0.42);
            border: 1px solid alpha(currentColor, 0.08);
            border-radius: 10px;
        }
        .ashot-section-title {
            margin-top: 2px;
            margin-bottom: 0;
            color: alpha(currentColor, 0.62);
            font-size: 0.78em;
            font-weight: 700;
            letter-spacing: 0.03em;
        }
        .ashot-action-button,
        .ashot-tool-button,
        .ashot-compact-menu {
            background: transparent;
            border: 1px solid alpha(currentColor, 0.10);
            box-shadow: none;
            border-radius: 8px;
            min-height: 32px;
            padding: 0 6px;
        }
        .ashot-tool-button {
            min-width: 42px;
            min-height: 34px;
            padding: 0;
        }
        .ashot-action-button:hover,
        .ashot-tool-button:hover {
            background: alpha(@accent_bg_color, 0.08);
            border-color: alpha(@accent_bg_color, 0.32);
        }
        .ashot-tool-icon {
            color: alpha(currentColor, 0.86);
        }
        button.active-tool {
            background: alpha(@accent_bg_color, 0.14);
            color: @accent_bg_color;
            border-color: alpha(@accent_bg_color, 0.54);
            box-shadow: inset 3px 0 0 @accent_bg_color;
        }
        button.active-tool label,
        button.active-tool .ashot-tool-icon {
            color: @accent_bg_color;
        }
        .ashot-action-primary {
            background: alpha(@accent_bg_color, 0.16);
            color: @accent_bg_color;
            border-color: alpha(@accent_bg_color, 0.55);
        }
        .ashot-action-primary image,
        .ashot-action-primary label {
            color: @accent_bg_color;
        }
        .ashot-output-more {
            min-width: 40px;
            min-height: 34px;
            padding: 0;
            border-radius: 8px;
        }
        .ashot-output-menu button {
            min-width: 172px;
            min-height: 36px;
            border-radius: 7px;
        }
        .ashot-color-header {
            margin-top: -1px;
        }
        .ashot-color-value-button {
            min-width: 82px;
            min-height: 30px;
            padding: 0 7px;
            border-radius: 8px;
            background: alpha(@view_bg_color, 0.56);
            border: 1px solid alpha(currentColor, 0.10);
        }
        .ashot-color-value-button:hover {
            background: alpha(@accent_bg_color, 0.08);
            border-color: alpha(@accent_bg_color, 0.28);
        }
        .ashot-eyedropper-button {
            min-width: 30px;
            min-height: 30px;
        }
        .ashot-magnifier-row {
            margin-top: 2px;
            padding: 4px 5px;
            border-radius: 9px;
            background: alpha(@view_bg_color, 0.28);
            border: 1px solid alpha(currentColor, 0.06);
        }
        .ashot-magnifier-settings {
            min-width: 30px;
            min-height: 28px;
            padding: 0;
            border-radius: 8px;
        }
        .ashot-magnifier-zoom-value {
            font-weight: 700;
            color: @accent_bg_color;
        }
        .ashot-magnifier-spin {
            min-width: 52px;
        }
        .ashot-color-memory-row {
            margin-top: 2px;
            min-height: 31px;
        }
        .ashot-memory-label {
            color: alpha(currentColor, 0.58);
            font-size: 0.78em;
            font-weight: 600;
            padding-right: 1px;
        }
        .ashot-color-memory-strip {
            padding: 3px;
            border-radius: 999px;
            background: alpha(@view_bg_color, 0.36);
            border: 1px solid alpha(currentColor, 0.07);
        }
        .ashot-color-memory-button {
            min-width: 24px;
            min-height: 24px;
            padding: 0;
            border: 0;
            border-radius: 999px;
            background: transparent;
            box-shadow: none;
        }
        .ashot-color-memory-button:hover {
            background: alpha(@accent_bg_color, 0.12);
        }
        .ashot-color-memory-button:disabled {
            opacity: 0.52;
        }
        .ashot-color-hex-label {
            font-weight: 600;
        }
        .ashot-favorite-button {
            min-width: 34px;
            min-height: 30px;
            padding: 0;
            border-radius: 8px;
        }
        .ashot-stroke-control {
            background: alpha(@view_bg_color, 0.38);
            border: 1px solid alpha(currentColor, 0.08);
            border-radius: 9px;
            padding: 5px;
        }
        .ashot-stroke-menu button {
            min-height: 30px;
            border-radius: 7px;
        }
        .ashot-compact-check {
            margin-top: -2px;
            font-size: 0.86em;
        }
        .ashot-canvas-frame {
            background: mix(@window_bg_color, @view_bg_color, 0.45);
            border-radius: 10px;
            padding: 12px;
        }
        .ashot-canvas-surface {
            background: @view_bg_color;
            box-shadow: 0 8px 28px alpha(black, 0.18), 0 0 0 1px alpha(black, 0.16);
        }
        .ashot-popover-panel {
            padding: 12px;
        }
        .ashot-osd {
            background: alpha(black, 0.68);
            color: white;
            border-radius: 7px;
            padding: 4px 8px;
            font-size: 0.86em;
        }
        .ashot-status {
            padding: 5px 8px;
            font-size: 0.86em;
        }
        .ashot-loading {
            opacity: 0.72;
        }
        .ashot-error {
            color: @error_color;
        }
        .ashot-ocr-dialog {
            background: @window_bg_color;
        }
        .ashot-ocr-header {
            padding: 4px 2px 8px 2px;
        }
        .ashot-ocr-status-icon {
            min-width: 34px;
            min-height: 34px;
            border-radius: 999px;
            background: alpha(@accent_bg_color, 0.13);
            color: @accent_bg_color;
        }
        .ashot-ocr-status-icon.error {
            background: alpha(@error_color, 0.13);
            color: @error_color;
        }
        .ashot-ocr-card {
            background: alpha(@view_bg_color, 0.72);
            border: 1px solid alpha(currentColor, 0.08);
            border-radius: 10px;
            padding: 1px;
        }
        .ashot-ocr-card.error {
            border-color: alpha(@error_color, 0.34);
            background: alpha(@error_color, 0.07);
        }
        .ashot-ocr-text-view {
            background: transparent;
            font-size: 0.98em;
        }
        .ashot-ocr-command {
            background: alpha(@view_bg_color, 0.82);
            border: 1px solid alpha(currentColor, 0.10);
            border-radius: 9px;
            padding: 8px;
        }
        .ashot-ocr-command label {
            font-family: monospace;
            font-size: 0.88em;
        }
        .ashot-ocr-actions {
            padding-top: 2px;
        }
        "#,
    );
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn editor_tool_layout() -> [(&'static str, DefaultTool); 12] {
    [
        ("Select", DefaultTool::Select),
        ("Pencil", DefaultTool::Brush),
        ("Line", DefaultTool::Line),
        ("Arrow", DefaultTool::Arrow),
        ("Rect", DefaultTool::Rectangle),
        ("Circle", DefaultTool::Ellipse),
        ("Marker", DefaultTool::Marker),
        ("Text", DefaultTool::Text),
        ("Count", DefaultTool::Counter),
        ("Pixel", DefaultTool::Mosaic),
        ("Blur", DefaultTool::Blur),
        ("Fill", DefaultTool::FilledBox),
    ]
}

#[cfg(test)]
fn tool_icon_label(tool: DefaultTool) -> &'static str {
    match tool {
        DefaultTool::Select => "↖",
        DefaultTool::Brush => "✎",
        DefaultTool::Line => "╱",
        DefaultTool::Arrow => "➜",
        DefaultTool::Rectangle => "□",
        DefaultTool::Ellipse => "○",
        DefaultTool::Marker => "▰",
        DefaultTool::Text => "T",
        DefaultTool::Counter => "#",
        DefaultTool::Mosaic => "▦",
        DefaultTool::Blur => "◌",
        DefaultTool::FilledBox => "■",
        DefaultTool::Ocr => "OCR",
        DefaultTool::ColorPicker => "◉",
    }
}

fn tool_icon_area(tool: DefaultTool) -> DrawingArea {
    let icon = DrawingArea::new();
    icon.set_content_width(TOOL_ICON_CANVAS_SIZE);
    icon.set_content_height(TOOL_ICON_CANVAS_SIZE);
    icon.add_css_class("ashot-tool-icon");
    icon.set_draw_func(move |area, cr, width, height| {
        draw_tool_icon(area, cr, tool, width, height);
    });
    icon
}

fn draw_tool_icon(
    area: &DrawingArea,
    cr: &gtk::cairo::Context,
    tool: DefaultTool,
    width: i32,
    height: i32,
) {
    let size = width.min(height).max(1) as f64;
    let scale = size / 24.0;
    let offset_x = (width as f64 - size) * 0.5;
    let offset_y = (height as f64 - size) * 0.5;
    let color = area.style_context().color();
    let _ = cr.save();
    cr.translate(offset_x, offset_y);
    cr.scale(scale, scale);
    cr.set_source_rgba(
        color.red() as f64,
        color.green() as f64,
        color.blue() as f64,
        color.alpha() as f64,
    );
    cr.set_line_width(tool_icon_stroke_width());
    cr.set_line_cap(gtk::cairo::LineCap::Round);
    cr.set_line_join(gtk::cairo::LineJoin::Round);

    match tool {
        DefaultTool::Select => {
            cr.move_to(7.0, 4.0);
            cr.line_to(18.0, 12.0);
            cr.line_to(13.2, 13.1);
            cr.line_to(16.0, 20.0);
            cr.line_to(13.2, 21.0);
            cr.line_to(10.5, 14.2);
            cr.line_to(6.5, 17.1);
            cr.close_path();
            let _ = cr.stroke();
        }
        DefaultTool::Brush => {
            cr.move_to(6.0, 18.0);
            cr.curve_to(9.0, 17.0, 10.0, 15.0, 11.2, 12.2);
            cr.line_to(17.2, 6.2);
            cr.move_to(13.8, 8.2);
            cr.line_to(16.2, 10.6);
            cr.move_to(5.4, 19.0);
            cr.curve_to(7.6, 21.0, 11.0, 20.7, 12.7, 18.6);
            let _ = cr.stroke();
        }
        DefaultTool::Line => {
            cr.move_to(6.0, 18.0);
            cr.line_to(18.0, 6.0);
            let _ = cr.stroke();
        }
        DefaultTool::Arrow => {
            cr.move_to(5.5, 18.0);
            cr.line_to(18.0, 5.5);
            cr.move_to(18.0, 5.5);
            cr.line_to(17.0, 11.0);
            cr.move_to(18.0, 5.5);
            cr.line_to(12.5, 6.5);
            let _ = cr.stroke();
        }
        DefaultTool::Rectangle => {
            draw_round_rect(cr, 5.0, 6.0, 14.0, 12.0, 2.0);
            let _ = cr.stroke();
        }
        DefaultTool::Ellipse => {
            let _ = cr.save();
            cr.translate(12.0, 12.0);
            cr.scale(1.25, 0.86);
            cr.arc(0.0, 0.0, 6.0, 0.0, std::f64::consts::TAU);
            let _ = cr.restore();
            let _ = cr.stroke();
        }
        DefaultTool::Marker => {
            draw_round_rect(cr, 6.0, 7.0, 12.0, 8.0, 2.0);
            let _ = cr.stroke();
            cr.move_to(8.0, 18.0);
            cr.line_to(16.0, 18.0);
            cr.move_to(9.2, 10.8);
            cr.line_to(14.8, 10.8);
            let _ = cr.stroke();
        }
        DefaultTool::Text => {
            cr.move_to(6.0, 6.0);
            cr.line_to(18.0, 6.0);
            cr.move_to(12.0, 6.0);
            cr.line_to(12.0, 19.0);
            cr.move_to(9.0, 19.0);
            cr.line_to(15.0, 19.0);
            let _ = cr.stroke();
        }
        DefaultTool::Counter => {
            cr.arc(12.0, 12.0, 7.2, 0.0, std::f64::consts::TAU);
            let _ = cr.stroke();
            cr.move_to(9.0, 12.0);
            cr.line_to(15.0, 12.0);
            cr.move_to(10.0, 9.5);
            cr.line_to(10.0, 14.5);
            cr.move_to(14.0, 9.5);
            cr.line_to(14.0, 14.5);
            let _ = cr.stroke();
        }
        DefaultTool::Mosaic => {
            for y in 0..3 {
                for x in 0..3 {
                    draw_round_rect(
                        cr,
                        6.0 + x as f64 * 4.25,
                        6.0 + y as f64 * 4.25,
                        2.8,
                        2.8,
                        0.8,
                    );
                }
            }
            let _ = cr.fill();
        }
        DefaultTool::Blur => {
            cr.arc(9.0, 12.0, 3.2, 0.0, std::f64::consts::TAU);
            cr.arc(14.0, 10.0, 3.0, 0.0, std::f64::consts::TAU);
            cr.arc(14.5, 15.0, 2.5, 0.0, std::f64::consts::TAU);
            let _ = cr.stroke();
        }
        DefaultTool::FilledBox => {
            draw_round_rect(cr, 6.0, 7.0, 12.0, 10.0, 2.0);
            let _ = cr.fill();
        }
        DefaultTool::Ocr => {
            cr.move_to(6.0, 9.0);
            cr.line_to(6.0, 6.0);
            cr.line_to(9.0, 6.0);
            cr.move_to(18.0, 9.0);
            cr.line_to(18.0, 6.0);
            cr.line_to(15.0, 6.0);
            cr.move_to(6.0, 15.0);
            cr.line_to(6.0, 18.0);
            cr.line_to(9.0, 18.0);
            cr.move_to(18.0, 15.0);
            cr.line_to(18.0, 18.0);
            cr.line_to(15.0, 18.0);
            cr.move_to(9.0, 11.0);
            cr.line_to(15.0, 11.0);
            cr.move_to(9.0, 14.0);
            cr.line_to(13.5, 14.0);
            let _ = cr.stroke();
        }
        DefaultTool::ColorPicker => {
            let _ = cr.save();
            cr.translate(12.0, 12.0);
            cr.rotate(-std::f64::consts::FRAC_PI_4);
            cr.arc(0.0, -6.8, 3.4, 0.0, std::f64::consts::TAU);
            let _ = cr.stroke();
            draw_round_rect(cr, -4.6, -2.4, 9.2, 3.6, 1.8);
            let _ = cr.stroke();
            cr.move_to(-2.8, 1.2);
            cr.line_to(-2.8, 8.0);
            cr.line_to(0.0, 10.5);
            cr.line_to(2.8, 8.0);
            cr.line_to(2.8, 1.2);
            let _ = cr.stroke();
            let _ = cr.restore();
        }
    }
    let _ = cr.restore();
}

fn tool_icon_stroke_width() -> f64 {
    1.55
}

fn ocr_language_menu(
    active_languages: Rc<RefCell<Vec<String>>>,
    state: Arc<ServiceState>,
) -> MenuButton {
    let menu = MenuButton::new();
    if let Ok(languages) = active_languages.try_borrow() {
        menu.set_label(&ocr_language_label(&languages));
    }
    menu.set_tooltip_text(Some("Select OCR language"));
    menu.set_hexpand(true);

    let popover = Popover::new();
    let root = GtkBox::new(Orientation::Vertical, 4);
    root.set_margin_top(8);
    root.set_margin_bottom(8);
    root.set_margin_start(8);
    root.set_margin_end(8);

    let checks = Rc::new(RefCell::new(Vec::<(String, gtk::CheckButton)>::new()));
    let languages = active_languages.try_borrow().map(|items| items.clone()).unwrap_or_default();
    for language in search_ocr_languages("") {
        let check = gtk::CheckButton::with_label(language.display_name);
        check.set_active(ocr_language_is_selected(&languages, language.tesseract_code));
        check.set_tooltip_text(Some(language.tesseract_code));

        let language_code = language.tesseract_code.to_string();
        let active_languages_for_toggle = active_languages.clone();
        let menu_for_toggle = menu.clone();
        let checks_for_toggle = checks.clone();
        let state_for_toggle = state.clone();
        check.connect_toggled(move |button| {
            if let Ok(mut languages) = active_languages_for_toggle.try_borrow_mut() {
                update_ocr_language_selection(&mut languages, &language_code, button.is_active());
                menu_for_toggle.set_label(&ocr_language_label(&languages));
                let persisted = languages.clone();
                persist_config_change(&state_for_toggle, |config| {
                    config.ocr_languages = persisted;
                });
            }
            if let (Ok(languages), Ok(checks)) =
                (active_languages_for_toggle.try_borrow(), checks_for_toggle.try_borrow())
            {
                for (code, check) in checks.iter() {
                    let should_be_active = ocr_language_is_selected(&languages, code);
                    if check.is_active() != should_be_active {
                        check.set_active(should_be_active);
                    }
                }
            }
        });

        checks.borrow_mut().push((language.tesseract_code.to_string(), check.clone()));
        root.append(&check);
    }

    popover.set_child(Some(&root));
    menu.set_popover(Some(&popover));
    menu
}

fn ocr_language_label(languages: &[String]) -> String {
    format!("Language: {}", ocr_language_summary(languages))
}

fn ocr_language_summary(languages: &[String]) -> String {
    let languages = normalize_ocr_languages(languages.to_vec());
    if languages.iter().any(|language| language == "auto") {
        return "Auto".to_string();
    }
    languages.join(" + ")
}

fn ocr_language_is_selected(languages: &[String], code: &str) -> bool {
    normalize_ocr_languages(languages.to_vec()).iter().any(|language| language == code)
}

fn update_ocr_language_selection(languages: &mut Vec<String>, code: &str, active: bool) {
    if code == "auto" {
        languages.clear();
        if active {
            languages.push("auto".to_string());
        } else {
            languages.extend(default_ocr_languages());
        }
        return;
    }

    languages.retain(|language| language != "auto");
    if active {
        if !languages.iter().any(|language| language == code) {
            languages.push(code.to_string());
        }
    } else {
        languages.retain(|language| language != code);
    }

    if languages.is_empty() {
        languages.extend(default_ocr_languages());
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct HsvColor {
    hue: f64,
    saturation: f64,
    value: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct HslColor {
    hue: f64,
    saturation: f64,
    lightness: f64,
}

fn color_to_hex(color: Color) -> String {
    if color.a == 255 {
        format!("#{:02X}{:02X}{:02X}", color.r, color.g, color.b)
    } else {
        format!("#{:02X}{:02X}{:02X}{:02X}", color.r, color.g, color.b, color.a)
    }
}

fn parse_hex_color(input: &str, fallback_alpha: u8) -> Option<Color> {
    let value = input.trim().trim_start_matches('#');
    let expanded = match value.len() {
        3 => value.chars().flat_map(|ch| [ch, ch]).collect::<String>(),
        4 => value.chars().flat_map(|ch| [ch, ch]).collect::<String>(),
        6 | 8 => value.to_string(),
        _ => return None,
    };

    let r = u8::from_str_radix(&expanded[0..2], 16).ok()?;
    let g = u8::from_str_radix(&expanded[2..4], 16).ok()?;
    let b = u8::from_str_radix(&expanded[4..6], 16).ok()?;
    let a = if expanded.len() == 8 {
        u8::from_str_radix(&expanded[6..8], 16).ok()?
    } else {
        fallback_alpha
    };
    Some(Color::rgba(r, g, b, a))
}

fn rgb_to_hsv(color: Color) -> HsvColor {
    let r = color.r as f64 / 255.0;
    let g = color.g as f64 / 255.0;
    let b = color.b as f64 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;

    let hue = if delta <= f64::EPSILON {
        0.0
    } else if (max - r).abs() <= f64::EPSILON {
        60.0 * ((g - b) / delta).rem_euclid(6.0)
    } else if (max - g).abs() <= f64::EPSILON {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    let saturation = if max <= f64::EPSILON { 0.0 } else { delta / max };

    HsvColor { hue, saturation, value: max }
}

fn hsv_to_color(hsv: HsvColor, alpha: u8) -> Color {
    let hue = hsv.hue.rem_euclid(360.0);
    let saturation = hsv.saturation.clamp(0.0, 1.0);
    let value = hsv.value.clamp(0.0, 1.0);
    let chroma = value * saturation;
    let x = chroma * (1.0 - ((hue / 60.0).rem_euclid(2.0) - 1.0).abs());
    let m = value - chroma;
    let (r1, g1, b1) = match hue {
        h if h < 60.0 => (chroma, x, 0.0),
        h if h < 120.0 => (x, chroma, 0.0),
        h if h < 180.0 => (0.0, chroma, x),
        h if h < 240.0 => (0.0, x, chroma),
        h if h < 300.0 => (x, 0.0, chroma),
        _ => (chroma, 0.0, x),
    };

    Color::rgba(
        ((r1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((g1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((b1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        alpha,
    )
}

fn rgb_to_hsl(color: Color) -> HslColor {
    let r = color.r as f64 / 255.0;
    let g = color.g as f64 / 255.0;
    let b = color.b as f64 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let lightness = (max + min) * 0.5;
    let saturation =
        if delta <= f64::EPSILON { 0.0 } else { delta / (1.0 - (2.0 * lightness - 1.0).abs()) };
    let hue = rgb_to_hsv(color).hue;

    HslColor { hue, saturation, lightness }
}

fn hsl_to_color(hsl: HslColor, alpha: u8) -> Color {
    let hue = hsl.hue.rem_euclid(360.0);
    let saturation = hsl.saturation.clamp(0.0, 1.0);
    let lightness = hsl.lightness.clamp(0.0, 1.0);
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let x = chroma * (1.0 - ((hue / 60.0).rem_euclid(2.0) - 1.0).abs());
    let m = lightness - chroma * 0.5;
    let (r1, g1, b1) = match hue {
        h if h < 60.0 => (chroma, x, 0.0),
        h if h < 120.0 => (x, chroma, 0.0),
        h if h < 180.0 => (0.0, chroma, x),
        h if h < 240.0 => (0.0, x, chroma),
        h if h < 300.0 => (x, 0.0, chroma),
        _ => (chroma, 0.0, x),
    };

    Color::rgba(
        ((r1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((g1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((b1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        alpha,
    )
}

fn push_recent_color(recent: &mut Vec<Color>, color: Color, limit: usize) {
    recent.retain(|item| *item != color);
    recent.insert(0, color);
    recent.truncate(limit);
}

fn add_favorite_color(favorites: &mut Vec<Color>, color: Color, limit: usize) -> Result<(), usize> {
    if let Some(index) = favorites.iter().position(|item| *item == color) {
        let color = favorites.remove(index);
        favorites.insert(0, color);
        return Ok(());
    }
    if favorites.len() >= limit {
        return Err(limit);
    }
    favorites.insert(0, color);
    Ok(())
}

fn remove_favorite_color(favorites: &mut Vec<Color>, color: Color) {
    favorites.retain(|item| *item != color);
}

#[cfg(test)]
fn editor_color_palette() -> [(&'static str, Color); 32] {
    [
        ("White", Color { r: 255, g: 255, b: 255, a: 255 }),
        ("Mist", Color { r: 226, g: 232, b: 240, a: 255 }),
        ("Silver", Color { r: 148, g: 163, b: 184, a: 255 }),
        ("Gray", Color { r: 100, g: 116, b: 139, a: 255 }),
        ("Slate", Color { r: 51, g: 65, b: 85, a: 255 }),
        ("Charcoal", Color { r: 30, g: 41, b: 59, a: 255 }),
        ("Black", Color { r: 15, g: 23, b: 42, a: 255 }),
        ("Brown", Color { r: 120, g: 80, b: 48, a: 255 }),
        ("Red", Color { r: 239, g: 68, b: 68, a: 255 }),
        ("Flame", Color { r: 232, g: 62, b: 38, a: 255 }),
        ("Orange", Color { r: 249, g: 115, b: 22, a: 255 }),
        ("Amber", Color { r: 245, g: 158, b: 11, a: 255 }),
        ("Yellow", Color { r: 234, g: 179, b: 8, a: 255 }),
        ("Lime", Color { r: 132, g: 204, b: 22, a: 255 }),
        ("Green", Color { r: 34, g: 197, b: 94, a: 255 }),
        ("Emerald", Color { r: 16, g: 185, b: 129, a: 255 }),
        ("Teal", Color { r: 20, g: 184, b: 166, a: 255 }),
        ("Cyan", Color { r: 6, g: 182, b: 212, a: 255 }),
        ("Sky", Color { r: 14, g: 165, b: 233, a: 255 }),
        ("Blue", Color { r: 37, g: 99, b: 235, a: 255 }),
        ("Indigo", Color { r: 79, g: 70, b: 229, a: 255 }),
        ("Violet", Color { r: 124, g: 58, b: 237, a: 255 }),
        ("Purple", Color { r: 147, g: 51, b: 234, a: 255 }),
        ("Fuchsia", Color { r: 217, g: 70, b: 239, a: 255 }),
        ("Rose", Color { r: 244, g: 63, b: 94, a: 255 }),
        ("Pink", Color { r: 236, g: 72, b: 153, a: 255 }),
        ("Coral", Color { r: 251, g: 113, b: 133, a: 255 }),
        ("Sand", Color { r: 180, g: 134, b: 90, a: 255 }),
        ("Olive", Color { r: 101, g: 128, b: 42, a: 255 }),
        ("Mint", Color { r: 45, g: 212, b: 191, a: 255 }),
        ("Navy", Color { r: 30, g: 64, b: 175, a: 255 }),
        ("Lavender", Color { r: 196, g: 181, b: 253, a: 255 }),
    ]
}

fn editor_favorite_palette() -> [(&'static str, Color); 4] {
    [
        ("Red", Color { r: 239, g: 68, b: 68, a: 255 }),
        ("Yellow", Color { r: 234, g: 179, b: 8, a: 255 }),
        ("Blue", Color { r: 37, g: 99, b: 235, a: 255 }),
        ("Black", Color { r: 15, g: 23, b: 42, a: 255 }),
    ]
}

fn editor_stroke_widths() -> [u32; 5] {
    [2, 4, 6, 8, 12]
}

fn selected_color_preview(active_color: Rc<Cell<Color>>) -> DrawingArea {
    let area = DrawingArea::new();
    area.set_content_width(34);
    area.set_content_height(18);
    area.add_css_class("ashot-color-preview");
    area.set_draw_func(move |_, cr, width, height| {
        draw_color_swatch(cr, active_color.get(), width, height, 6.0, 2.0);
    });
    area
}

fn dynamic_color_swatch(color: Rc<Cell<Color>>, width: i32, height: i32) -> DrawingArea {
    let area = DrawingArea::new();
    area.set_content_width(width);
    area.set_content_height(height);
    area.set_draw_func(move |_, cr, draw_width, draw_height| {
        draw_color_swatch(cr, color.get(), draw_width, draw_height, 999.0, 2.0);
    });
    area
}

fn draw_color_swatch(
    cr: &gtk::cairo::Context,
    color: Color,
    width: i32,
    height: i32,
    radius: f64,
    inset: f64,
) {
    let width = width.max(1) as f64;
    let height = height.max(1) as f64;
    let outer_radius = radius.min(width * 0.5).min(height * 0.5);
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.16);
    draw_round_rect(cr, 0.5, 0.5, width - 1.0, height - 1.0, outer_radius);
    let _ = cr.stroke();
    if color.a == 0 {
        cr.set_source_rgba(0.45, 0.48, 0.53, 0.35);
        cr.set_line_width(1.1);
        cr.move_to(width * 0.28, height * 0.72);
        cr.line_to(width * 0.72, height * 0.28);
        let _ = cr.stroke();
        return;
    }

    let inner_width = (width - inset * 2.0).max(1.0);
    let inner_height = (height - inset * 2.0).max(1.0);
    set_cairo_color(cr, color);
    draw_round_rect(cr, inset, inset, inner_width, inner_height, (outer_radius - inset).max(1.0));
    let _ = cr.fill();
}

fn eyedropper_icon_button() -> Button {
    let button = Button::new();
    button.set_child(Some(&tool_icon_area(DefaultTool::ColorPicker)));
    button.set_tooltip_text(Some("Pick color from image"));
    button.set_size_request(COLOR_VALUE_BUTTON_HEIGHT, COLOR_VALUE_BUTTON_HEIGHT);
    button.add_css_class("flat");
    button.add_css_class("ashot-tool-button");
    button.add_css_class("ashot-eyedropper-button");
    button
}

fn eyedropper_magnifier_control_row(
    active_enabled: Rc<Cell<bool>>,
    active_zoom: Rc<Cell<f64>>,
    canvas: DrawingArea,
    state: Arc<ServiceState>,
) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 6);
    row.add_css_class("ashot-magnifier-row");

    let toggle = gtk::CheckButton::with_label("Magnifier");
    toggle.set_active(active_enabled.get());
    toggle.set_hexpand(true);
    toggle.add_css_class("ashot-compact-check");
    row.append(&toggle);

    let settings = MenuButton::new();
    settings.set_icon_name("preferences-system-symbolic");
    settings.set_tooltip_text(Some("Magnifier settings"));
    settings.set_size_request(30, 28);
    settings.add_css_class("flat");
    settings.add_css_class("ashot-magnifier-settings");
    row.append(&settings);

    let popover = Popover::new();
    let root = GtkBox::new(Orientation::Vertical, 8);
    root.add_css_class("ashot-popover-panel");
    root.set_margin_top(8);
    root.set_margin_bottom(8);
    root.set_margin_start(8);
    root.set_margin_end(8);
    root.set_size_request(260, -1);

    let title_row = GtkBox::new(Orientation::Horizontal, 8);
    let title = Label::new(Some("Magnifier"));
    title.set_xalign(0.0);
    title.set_hexpand(true);
    let zoom_value = Label::new(Some(&format_magnifier_zoom(active_zoom.get())));
    zoom_value.add_css_class("ashot-magnifier-zoom-value");
    title_row.append(&title);
    title_row.append(&zoom_value);
    root.append(&title_row);

    let control_row = GtkBox::new(Orientation::Horizontal, 8);
    let zoom_label = Label::new(Some("Zoom"));
    zoom_label.set_xalign(0.0);
    zoom_label.set_width_chars(5);
    let adjustment = Adjustment::new(
        clamp_magnifier_zoom(active_zoom.get()),
        EYEDROPPER_MAGNIFIER_MIN_ZOOM,
        EYEDROPPER_MAGNIFIER_MAX_ZOOM,
        1.0,
        2.0,
        0.0,
    );
    let zoom_scale = Scale::new(Orientation::Horizontal, Some(&adjustment));
    zoom_scale.set_draw_value(false);
    zoom_scale.set_hexpand(true);
    zoom_scale.set_size_request(150, -1);
    let zoom_spin = SpinButton::new(Some(&adjustment), 1.0, 0);
    zoom_spin.set_numeric(true);
    zoom_spin.set_snap_to_ticks(true);
    zoom_spin.set_width_chars(3);
    zoom_spin.add_css_class("ashot-magnifier-spin");
    control_row.append(&zoom_label);
    control_row.append(&zoom_scale);
    control_row.append(&zoom_spin);
    root.append(&control_row);

    let active_enabled_for_toggle = active_enabled.clone();
    let canvas_for_toggle = canvas.clone();
    let state_for_toggle = state.clone();
    toggle.connect_toggled(move |button| {
        let enabled = button.is_active();
        active_enabled_for_toggle.set(enabled);
        persist_config_change(&state_for_toggle, |config| {
            config.eyedropper_magnifier_enabled = enabled;
        });
        canvas_for_toggle.queue_draw();
    });

    let active_zoom_for_adjustment = active_zoom.clone();
    let canvas_for_adjustment = canvas.clone();
    let state_for_adjustment = state.clone();
    adjustment.connect_value_changed(move |adjustment| {
        let zoom = clamp_magnifier_zoom(adjustment.value());
        if (adjustment.value() - zoom).abs() > f64::EPSILON {
            adjustment.set_value(zoom);
            return;
        }
        active_zoom_for_adjustment.set(zoom);
        zoom_value.set_text(&format_magnifier_zoom(zoom));
        persist_config_change(&state_for_adjustment, |config| {
            config.eyedropper_magnifier_zoom = zoom;
        });
        canvas_for_adjustment.queue_draw();
    });

    popover.set_child(Some(&root));
    settings.set_popover(Some(&popover));
    row
}

fn color_memory_row(title: &str, buttons: &ColorMemoryButtons) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 6);
    row.add_css_class("ashot-color-memory-row");
    let label = Label::new(Some(title));
    label.set_xalign(1.0);
    label.set_width_chars(8);
    label.add_css_class("ashot-memory-label");
    row.append(&label);

    let strip = GtkBox::new(Orientation::Horizontal, 3);
    strip.set_halign(Align::Start);
    strip.set_hexpand(false);
    strip.add_css_class("ashot-color-memory-strip");
    for _ in 0..COLOR_ROW_LIMIT {
        let color_cell = Rc::new(Cell::new(Color::rgba(0, 0, 0, 0)));
        let swatch = dynamic_color_swatch(
            color_cell.clone(),
            COLOR_MEMORY_SWATCH_SIZE,
            COLOR_MEMORY_SWATCH_SIZE,
        );
        let button = Button::new();
        button.set_child(Some(&swatch));
        button.set_sensitive(false);
        button.set_size_request(COLOR_MEMORY_BUTTON_SIZE, COLOR_MEMORY_BUTTON_SIZE);
        button.add_css_class("flat");
        button.add_css_class("ashot-color-memory-button");
        strip.append(&button);
        buttons.borrow_mut().push((color_cell, swatch, button));
    }
    row.append(&strip);
    row
}

fn update_color_memory_buttons(colors: &[Color], buttons: &ColorMemoryButtons) {
    if let Ok(buttons) = buttons.try_borrow() {
        for (index, (color_cell, swatch, button)) in buttons.iter().enumerate() {
            if let Some(color) = colors.get(index) {
                color_cell.set(*color);
                button.set_sensitive(true);
                button.set_tooltip_text(Some(&color_to_hex(*color)));
            } else {
                color_cell.set(Color::rgba(0, 0, 0, 0));
                button.set_sensitive(false);
                button.set_tooltip_text(None);
            }
            swatch.queue_draw();
        }
    }
}

fn show_favorite_remove_popover(
    anchor: &Button,
    color: Color,
    active_color: Rc<Cell<Color>>,
    favorites: Rc<RefCell<Vec<Color>>>,
    favorite_buttons: ColorMemoryButtons,
    refresh: ColorCallback,
    state: Arc<ServiceState>,
) {
    let popover = Popover::new();
    popover.set_parent(anchor);

    let root = GtkBox::new(Orientation::Vertical, 6);
    root.set_margin_top(8);
    root.set_margin_bottom(8);
    root.set_margin_start(8);
    root.set_margin_end(8);

    let delete = Button::with_label("Remove");
    delete.add_css_class("destructive-action");
    root.append(&delete);

    let popover_for_delete = popover.clone();
    delete.connect_clicked(move |_| {
        let mut removed = false;
        if let Ok(mut favorites) = favorites.try_borrow_mut() {
            removed = favorites.iter().any(|favorite| *favorite == color);
            remove_favorite_color(&mut favorites, color);
            update_color_memory_buttons(&favorites, &favorite_buttons);
            let persisted = favorites.clone();
            persist_config_change(&state, |config| {
                config.favorite_colors = persisted;
            });
        }
        if removed {
            refresh(active_color.get());
        }
        popover_for_delete.popdown();
        popover_for_delete.unparent();
    });

    popover.set_child(Some(&root));
    popover.popup();
}

fn draw_round_rect(cr: &gtk::cairo::Context, x: f64, y: f64, width: f64, height: f64, radius: f64) {
    let r = radius.min(width * 0.5).min(height * 0.5);
    cr.new_sub_path();
    cr.arc(x + width - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
    cr.arc(x + width - r, y + height - r, r, 0.0, std::f64::consts::FRAC_PI_2);
    cr.arc(x + r, y + height - r, r, std::f64::consts::FRAC_PI_2, std::f64::consts::PI);
    cr.arc(x + r, y + r, r, std::f64::consts::PI, std::f64::consts::PI * 1.5);
    cr.close_path();
}

fn slider_value_from_x(area: &DrawingArea, x: f64) -> f64 {
    let width = area.allocated_width().max(1) as f64;
    (x / width).clamp(0.0, 1.0)
}

fn clamp_magnifier_zoom(zoom: f64) -> f64 {
    if !zoom.is_finite() {
        return EYEDROPPER_MAGNIFIER_DEFAULT_ZOOM;
    }
    zoom.round().clamp(EYEDROPPER_MAGNIFIER_MIN_ZOOM, EYEDROPPER_MAGNIFIER_MAX_ZOOM)
}

fn format_magnifier_zoom(zoom: f64) -> String {
    format!("{}x", clamp_magnifier_zoom(zoom) as i32)
}

fn draw_hue_slider_bar(cr: &gtk::cairo::Context, width: i32, height: i32, hue: f64) {
    let width_f = width.max(1) as f64;
    let height_f = height.max(1) as f64;
    let radius = height_f * 0.34;
    let _ = cr.save();
    draw_round_rect(cr, 1.0, 2.0, width_f - 2.0, height_f - 4.0, radius);
    cr.clip();
    for x in 0..width {
        let color = hsv_to_color(
            HsvColor {
                hue: x as f64 * 360.0 / (width - 1).max(1) as f64,
                saturation: 1.0,
                value: 1.0,
            },
            255,
        );
        cr.set_source_rgb(color.r as f64 / 255.0, color.g as f64 / 255.0, color.b as f64 / 255.0);
        cr.rectangle(x as f64, 0.0, 1.0, height_f);
        let _ = cr.fill();
    }
    let _ = cr.restore();
    draw_slider_knob(cr, hue.rem_euclid(360.0) / 360.0, width_f, height_f);
}

fn draw_opacity_slider_bar(cr: &gtk::cairo::Context, width: i32, height: i32, color: Color) {
    let width_f = width.max(1) as f64;
    let height_f = height.max(1) as f64;
    let radius = height_f * 0.34;
    let _ = cr.save();
    draw_round_rect(cr, 1.0, 2.0, width_f - 2.0, height_f - 4.0, radius);
    cr.clip();
    for x in 0..width {
        let alpha = x as f64 / (width - 1).max(1) as f64;
        cr.set_source_rgba(
            color.r as f64 / 255.0,
            color.g as f64 / 255.0,
            color.b as f64 / 255.0,
            alpha,
        );
        cr.rectangle(x as f64, 0.0, 1.0, height_f);
        let _ = cr.fill();
    }
    let _ = cr.restore();
    draw_slider_knob(cr, color.a as f64 / 255.0, width_f, height_f);
}

fn draw_slider_knob(cr: &gtk::cairo::Context, value: f64, width: f64, height: f64) {
    let knob_width = 16.0;
    let knob_height = (height - 2.0).max(18.0);
    let x = (value.clamp(0.0, 1.0) * width).clamp(knob_width * 0.5, width - knob_width * 0.5);
    let y = (height - knob_height) * 0.5;
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.22);
    draw_round_rect(cr, x - knob_width * 0.5 + 1.0, y + 1.0, knob_width, knob_height, 7.0);
    let _ = cr.fill();
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.98);
    draw_round_rect(cr, x - knob_width * 0.5, y, knob_width, knob_height, 7.0);
    let _ = cr.fill_preserve();
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.18);
    cr.set_line_width(1.0);
    let _ = cr.stroke();
}

fn format_percent(value: f64) -> String {
    format!("{}%", (value * 100.0).round() as i32)
}

fn set_entry_text_if_needed(entry: &Entry, value: &str) {
    if entry.text().as_str() != value {
        entry.set_text(value);
    }
}

fn image_color_at(base: &image::DynamicImage, point: Point) -> Color {
    let rgba_image = base.to_rgba8();
    rgba_color_at(&rgba_image, point)
}

fn rgba_color_at(base: &image::RgbaImage, point: Point) -> Color {
    let image_x = point.x.floor().clamp(0.0, base.width().saturating_sub(1) as f32) as u32;
    let image_y = point.y.floor().clamp(0.0, base.height().saturating_sub(1) as f32) as u32;
    let pixel = base.get_pixel(image_x, image_y).0;
    Color::rgba(pixel[0], pixel[1], pixel[2], pixel[3])
}

fn image_point_inside(point: Point, image_width: u32, image_height: u32) -> bool {
    point.x >= 0.0
        && point.y >= 0.0
        && point.x < image_width as f32
        && point.y < image_height as f32
}

fn eyedropper_magnifier_point(
    tool: DefaultTool,
    enabled: bool,
    point: Option<Point>,
    image_width: u32,
    image_height: u32,
) -> Option<Point> {
    let point = point?;
    if tool_picks_canvas_color(tool)
        && enabled
        && image_point_inside(point, image_width, image_height)
    {
        Some(point)
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct MagnifierGeometry {
    x: f64,
    y: f64,
    size: f64,
}

fn magnifier_size_for_zoom(zoom: f64) -> f64 {
    let sample_width = (EYEDROPPER_MAGNIFIER_SAMPLE_RADIUS * 2 + 1) as f64;
    (sample_width * clamp_magnifier_zoom(zoom) + 32.0).clamp(88.0, 210.0)
}

fn magnifier_geometry(
    canvas_point: Point,
    canvas_width: i32,
    canvas_height: i32,
    zoom: f64,
) -> MagnifierGeometry {
    let size = magnifier_size_for_zoom(zoom);
    let edge = 6.0;
    let offset = 18.0;
    let point_x = canvas_point.x as f64;
    let point_y = canvas_point.y as f64;
    let mut x = point_x + offset;
    let mut y = point_y - size - offset;

    if y < edge {
        y = point_y + offset;
    }
    if x + size > canvas_width as f64 - edge {
        x = point_x - size - offset;
    }

    let max_x = (canvas_width as f64 - size - edge).max(edge);
    let max_y = (canvas_height as f64 - size - edge).max(edge);
    MagnifierGeometry { x: x.clamp(edge, max_x), y: y.clamp(edge, max_y), size }
}

fn draw_eyedropper_magnifier(
    cr: &gtk::cairo::Context,
    base: &image::RgbaImage,
    image_point: Point,
    canvas_point: Point,
    canvas_width: i32,
    canvas_height: i32,
    zoom: f64,
) {
    let zoom = clamp_magnifier_zoom(zoom);
    let geometry = magnifier_geometry(canvas_point, canvas_width, canvas_height, zoom);
    let size = geometry.size;
    let radius = size * 0.5;
    let center_x = geometry.x + radius;
    let center_y = geometry.y + radius;
    let sample_width = EYEDROPPER_MAGNIFIER_SAMPLE_RADIUS * 2 + 1;
    let cell_size = zoom;
    let grid_size = sample_width as f64 * cell_size;
    let grid_x = center_x - grid_size * 0.5;
    let grid_y = center_y - grid_size * 0.5;
    let center_color = rgba_color_at(base, image_point);

    let _ = cr.save();
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.24);
    cr.arc(center_x + 1.5, center_y + 2.0, radius, 0.0, std::f64::consts::TAU);
    let _ = cr.fill();

    cr.arc(center_x, center_y, radius, 0.0, std::f64::consts::TAU);
    cr.clip();
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.96);
    cr.paint().ok();

    let origin_x = image_point.x.floor() as i32;
    let origin_y = image_point.y.floor() as i32;
    for sample_y in -EYEDROPPER_MAGNIFIER_SAMPLE_RADIUS..=EYEDROPPER_MAGNIFIER_SAMPLE_RADIUS {
        for sample_x in -EYEDROPPER_MAGNIFIER_SAMPLE_RADIUS..=EYEDROPPER_MAGNIFIER_SAMPLE_RADIUS {
            let x = (origin_x + sample_x).clamp(0, base.width().saturating_sub(1) as i32) as u32;
            let y = (origin_y + sample_y).clamp(0, base.height().saturating_sub(1) as i32) as u32;
            let pixel = base.get_pixel(x, y).0;
            cr.set_source_rgba(
                pixel[0] as f64 / 255.0,
                pixel[1] as f64 / 255.0,
                pixel[2] as f64 / 255.0,
                1.0,
            );
            cr.rectangle(
                grid_x + (sample_x + EYEDROPPER_MAGNIFIER_SAMPLE_RADIUS) as f64 * cell_size,
                grid_y + (sample_y + EYEDROPPER_MAGNIFIER_SAMPLE_RADIUS) as f64 * cell_size,
                cell_size.ceil(),
                cell_size.ceil(),
            );
            let _ = cr.fill();
        }
    }

    cr.set_source_rgba(1.0, 1.0, 1.0, 0.72);
    cr.set_line_width(1.0);
    cr.move_to(center_x - cell_size * 0.5, center_y);
    cr.line_to(center_x + cell_size * 0.5, center_y);
    cr.move_to(center_x, center_y - cell_size * 0.5);
    cr.line_to(center_x, center_y + cell_size * 0.5);
    let _ = cr.stroke();
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.62);
    cr.rectangle(center_x - cell_size * 0.5, center_y - cell_size * 0.5, cell_size, cell_size);
    cr.set_line_width(1.2);
    let _ = cr.stroke();

    cr.set_source_rgba(0.0, 0.0, 0.0, 0.58);
    cr.rectangle(geometry.x, geometry.y + size - 28.0, size, 28.0);
    let _ = cr.fill();
    cr.set_source_rgba(
        center_color.r as f64 / 255.0,
        center_color.g as f64 / 255.0,
        center_color.b as f64 / 255.0,
        1.0,
    );
    cr.arc(geometry.x + 22.0, geometry.y + size - 14.0, 6.0, 0.0, std::f64::consts::TAU);
    let _ = cr.fill();
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
    cr.select_font_face("Sans", gtk::cairo::FontSlant::Normal, gtk::cairo::FontWeight::Bold);
    cr.set_font_size(11.0);
    cr.move_to(geometry.x + 34.0, geometry.y + size - 10.0);
    let _ = cr.show_text(&color_to_hex(center_color));
    let _ = cr.restore();

    let _ = cr.save();
    cr.arc(center_x, center_y, radius - 0.5, 0.0, std::f64::consts::TAU);
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.92);
    cr.set_line_width(2.0);
    let _ = cr.stroke();
    cr.arc(center_x, center_y, radius - 1.5, 0.0, std::f64::consts::TAU);
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.28);
    cr.set_line_width(1.0);
    let _ = cr.stroke();
    let _ = cr.restore();
}

fn color_picker_section(
    active_color: Rc<Cell<Color>>,
    stroke_previews: Rc<RefCell<Vec<DrawingArea>>>,
    document: Rc<RefCell<Document>>,
    tool_buttons: Rc<RefCell<Vec<(DefaultTool, Button)>>>,
    color_ui_refresh: Rc<RefCell<Option<ColorCallback>>>,
    recent_color_recorder: Rc<RefCell<Option<ColorCallback>>>,
    canvas: DrawingArea,
    state: Arc<ServiceState>,
    history: Rc<RefCell<EditorHistory>>,
    undo: Button,
    redo: Button,
    refresh_status: StatusCallback,
    show_toast: ToastCallback,
    queue_render_cache: Rc<dyn Fn()>,
    active_magnifier_enabled: Rc<Cell<bool>>,
    active_magnifier_zoom: Rc<Cell<f64>>,
) -> GtkBox {
    let container = GtkBox::new(Orientation::Vertical, 5);
    container.add_css_class("ashot-color-section");
    let header = GtkBox::new(Orientation::Horizontal, 5);
    header.add_css_class("ashot-color-header");
    header.append(&section_title("Color"));
    let header_preview = selected_color_preview(active_color.clone());
    header.append(&header_preview);

    let menu_button = MenuButton::new();
    menu_button.set_label(&color_to_hex(active_color.get()));
    menu_button.set_tooltip_text(Some("Open color picker"));
    menu_button.set_size_request(COLOR_VALUE_BUTTON_WIDTH, COLOR_VALUE_BUTTON_HEIGHT);
    menu_button.add_css_class("ashot-color-value-button");
    header.append(&menu_button);
    let eyedropper_button = eyedropper_icon_button();
    let document_for_pick = document.clone();
    let tool_buttons_for_pick = tool_buttons.clone();
    let canvas_for_pick = canvas.clone();
    let state_for_pick = state.clone();
    let refresh_status_for_pick = refresh_status.clone();
    eyedropper_button.connect_clicked(move |_| {
        if let Ok(mut document) = document_for_pick.try_borrow_mut() {
            document.active_tool = DefaultTool::ColorPicker;
        }
        update_tool_button_selection(&tool_buttons_for_pick, DefaultTool::ColorPicker);
        apply_editor_cursor(&canvas_for_pick, DefaultTool::ColorPicker);
        persist_config_change(&state_for_pick, |config| {
            config.default_tool = DefaultTool::ColorPicker;
        });
        refresh_status_for_pick(None);
    });
    tool_buttons.borrow_mut().push((DefaultTool::ColorPicker, eyedropper_button.clone()));
    if let Ok(document) = document.try_borrow() {
        update_tool_button_selection(&tool_buttons, document.active_tool);
    }
    header.append(&eyedropper_button);
    container.append(&header);

    let popover = Popover::new();
    let root = GtkBox::new(Orientation::Vertical, 7);
    root.add_css_class("ashot-popover-panel");
    root.set_margin_top(8);
    root.set_margin_bottom(8);
    root.set_margin_start(8);
    root.set_margin_end(8);
    root.set_halign(Align::Start);
    root.set_hexpand(false);
    root.set_size_request(340, -1);

    let hsv_state = Rc::new(Cell::new(rgb_to_hsv(active_color.get())));
    let alpha_state = Rc::new(Cell::new(active_color.get().a));
    let refreshing = Rc::new(Cell::new(false));
    let config_snapshot = state.config_snapshot();
    let recent_colors = Rc::new(RefCell::new(
        config_snapshot.recent_colors.into_iter().take(COLOR_ROW_LIMIT).collect::<Vec<_>>(),
    ));
    let favorite_colors = Rc::new(RefCell::new(if config_snapshot.favorite_colors.is_empty() {
        editor_favorite_palette()
            .iter()
            .take(COLOR_ROW_LIMIT)
            .map(|(_, color)| *color)
            .collect::<Vec<_>>()
    } else {
        config_snapshot.favorite_colors.into_iter().take(COLOR_ROW_LIMIT).collect::<Vec<_>>()
    }));
    let recent_buttons =
        Rc::new(RefCell::new(Vec::<(Rc<Cell<Color>>, DrawingArea, Button)>::new()));
    let favorite_buttons =
        Rc::new(RefCell::new(Vec::<(Rc<Cell<Color>>, DrawingArea, Button)>::new()));

    let recent_row = color_memory_row("Recent", &recent_buttons);
    let favorite_row = color_memory_row("Favorite", &favorite_buttons);
    container.append(&recent_row);
    container.append(&favorite_row);
    container.append(&eyedropper_magnifier_control_row(
        active_magnifier_enabled.clone(),
        active_magnifier_zoom.clone(),
        canvas.clone(),
        state.clone(),
    ));

    let top_row = GtkBox::new(Orientation::Horizontal, 8);
    top_row.set_halign(Align::Start);
    let current_color = Rc::new(Cell::new(active_color.get()));
    let current_preview = dynamic_color_swatch(current_color.clone(), 34, 28);
    let hex_label = Label::new(Some(&color_to_hex(active_color.get())));
    hex_label.set_xalign(0.0);
    hex_label.set_width_chars(10);
    hex_label.add_css_class("ashot-color-hex-label");
    let favorite_error = Label::new(None);
    favorite_error.add_css_class("error");
    favorite_error.set_xalign(0.0);
    let add_favorite = Button::with_label("☆");
    add_favorite.set_tooltip_text(Some("Add or remove favorite"));
    add_favorite.set_size_request(34, 30);
    add_favorite.add_css_class("ashot-favorite-button");
    top_row.append(&current_preview);
    top_row.append(&hex_label);
    top_row.append(&add_favorite);
    root.append(&top_row);

    let hsv_area = DrawingArea::new();
    hsv_area.set_content_width(316);
    hsv_area.set_content_height(150);
    hsv_area.set_halign(Align::Start);
    let hsv_for_draw = hsv_state.clone();
    hsv_area.set_draw_func(move |_, cr, width, height| {
        let hsv = hsv_for_draw.get();
        for y in (0..height).step_by(2) {
            let value = 1.0 - (y as f64 / (height - 1).max(1) as f64);
            for x in (0..width).step_by(2) {
                let saturation = x as f64 / (width - 1).max(1) as f64;
                let color = hsv_to_color(HsvColor { hue: hsv.hue, saturation, value }, 255);
                cr.set_source_rgb(
                    color.r as f64 / 255.0,
                    color.g as f64 / 255.0,
                    color.b as f64 / 255.0,
                );
                cr.rectangle(x as f64, y as f64, 2.0, 2.0);
                let _ = cr.fill();
            }
        }
        let knob_x = hsv.saturation * width as f64;
        let knob_y = (1.0 - hsv.value) * height as f64;
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
        cr.arc(knob_x, knob_y, 5.5, 0.0, std::f64::consts::TAU);
        cr.set_line_width(2.0);
        let _ = cr.stroke();
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.65);
        cr.arc(knob_x, knob_y, 7.0, 0.0, std::f64::consts::TAU);
        cr.set_line_width(1.0);
        let _ = cr.stroke();
    });
    root.append(&hsv_area);

    let hue_row = GtkBox::new(Orientation::Horizontal, 8);
    hue_row.set_halign(Align::Start);
    let hue_label = Label::new(Some("Hue"));
    hue_label.set_xalign(0.0);
    hue_label.set_width_chars(6);
    let hue_bar = DrawingArea::new();
    hue_bar.set_content_width(248);
    hue_bar.set_content_height(28);
    hue_bar.set_halign(Align::Start);
    let hsv_for_hue_bar = hsv_state.clone();
    hue_bar.set_draw_func(move |_, cr, width, height| {
        draw_hue_slider_bar(cr, width, height, hsv_for_hue_bar.get().hue);
    });
    hue_row.append(&hue_label);
    hue_row.append(&hue_bar);
    root.append(&hue_row);

    let opacity_row = GtkBox::new(Orientation::Horizontal, 8);
    opacity_row.set_halign(Align::Start);
    let opacity_label = Label::new(Some("Opacity"));
    opacity_label.set_xalign(0.0);
    opacity_label.set_width_chars(6);
    let opacity_value =
        Label::new(Some(&format!("{}%", (active_color.get().a as f64 * 100.0 / 255.0).round())));
    let opacity_bar = DrawingArea::new();
    opacity_bar.set_content_width(220);
    opacity_bar.set_content_height(28);
    opacity_bar.set_halign(Align::Start);
    let opacity_color = current_color.clone();
    opacity_bar.set_draw_func(move |_, cr, width, height| {
        draw_opacity_slider_bar(cr, width, height, opacity_color.get());
    });
    opacity_row.append(&opacity_label);
    opacity_row.append(&opacity_bar);
    opacity_row.append(&opacity_value);
    root.append(&opacity_row);

    let tab_row = GtkBox::new(Orientation::Horizontal, 4);
    tab_row.set_halign(Align::Start);
    let hex_tab = Button::with_label("HEX");
    let rgb_tab = Button::with_label("RGB");
    let hsl_tab = Button::with_label("HSL");
    tab_row.append(&hex_tab);
    tab_row.append(&rgb_tab);
    tab_row.append(&hsl_tab);
    root.append(&tab_row);

    let stack = gtk::Stack::new();
    stack.set_halign(Align::Start);
    stack.set_hexpand(false);
    let hex_entry = Entry::new();
    hex_entry.set_width_chars(9);
    hex_entry.set_max_width_chars(9);
    hex_entry.set_size_request(148, 34);
    hex_entry.set_hexpand(false);
    stack.add_titled(&hex_entry, Some("hex"), "HEX");

    let rgb_box = GtkBox::new(Orientation::Horizontal, 8);
    rgb_box.set_halign(Align::Start);
    rgb_box.set_size_request(316, -1);
    let r_entry = Entry::new();
    let g_entry = Entry::new();
    let b_entry = Entry::new();
    for (label, entry) in [("R", &r_entry), ("G", &g_entry), ("B", &b_entry)] {
        rgb_box.append(&Label::new(Some(label)));
        entry.set_width_chars(4);
        entry.set_max_width_chars(4);
        entry.set_hexpand(false);
        rgb_box.append(entry);
    }
    stack.add_titled(&rgb_box, Some("rgb"), "RGB");

    let hsl_box = GtkBox::new(Orientation::Horizontal, 8);
    hsl_box.set_halign(Align::Start);
    hsl_box.set_size_request(316, -1);
    let h_entry = Entry::new();
    let s_entry = Entry::new();
    let l_entry = Entry::new();
    for (label, entry, width) in [("H", &h_entry, 4), ("S", &s_entry, 5), ("L", &l_entry, 5)] {
        hsl_box.append(&Label::new(Some(label)));
        entry.set_width_chars(width);
        entry.set_max_width_chars(width);
        entry.set_hexpand(false);
        hsl_box.append(entry);
    }
    stack.add_titled(&hsl_box, Some("hsl"), "HSL");
    root.append(&stack);

    root.append(&favorite_error);

    let refresh_ui: Rc<dyn Fn(Color)> = {
        let active_color = active_color.clone();
        let hsv_state = hsv_state.clone();
        let alpha_state = alpha_state.clone();
        let refreshing = refreshing.clone();
        let header_preview = header_preview.clone();
        let current_color = current_color.clone();
        let current_preview = current_preview.clone();
        let hex_label = hex_label.clone();
        let menu_button = menu_button.clone();
        let add_favorite = add_favorite.clone();
        let hsv_area = hsv_area.clone();
        let hue_bar = hue_bar.clone();
        let opacity_bar = opacity_bar.clone();
        let opacity_value = opacity_value.clone();
        let hex_entry = hex_entry.clone();
        let r_entry = r_entry.clone();
        let g_entry = g_entry.clone();
        let b_entry = b_entry.clone();
        let h_entry = h_entry.clone();
        let s_entry = s_entry.clone();
        let l_entry = l_entry.clone();
        let stroke_previews = stroke_previews.clone();
        let favorite_colors = favorite_colors.clone();
        let favorite_buttons = favorite_buttons.clone();
        let document = document.clone();
        let history = history.clone();
        let undo = undo.clone();
        let redo = redo.clone();
        let canvas = canvas.clone();
        let state = state.clone();
        let refresh_status = refresh_status.clone();
        let queue_render_cache = queue_render_cache.clone();
        Rc::new(move |color: Color| {
            refreshing.set(true);
            active_color.set(color);
            persist_config_change(&state, |config| {
                config.default_color = color;
            });
            let style_changed = if let Ok(mut document) = document.try_borrow_mut() {
                if document.active_tool == DefaultTool::Select && document.selected.is_some() {
                    let before = document.annotations.clone();
                    if document.apply_color_to_selected(color) {
                        if let Ok(mut history) = history.try_borrow_mut() {
                            history.snapshot(&before);
                        }
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            };
            let hsv = rgb_to_hsv(color);
            hsv_state.set(hsv);
            alpha_state.set(color.a);
            current_color.set(color);
            hex_label.set_text(&color_to_hex(color));
            menu_button.set_label(&color_to_hex(color));
            set_entry_text_if_needed(&hex_entry, &color_to_hex(color));
            set_entry_text_if_needed(&r_entry, &color.r.to_string());
            set_entry_text_if_needed(&g_entry, &color.g.to_string());
            set_entry_text_if_needed(&b_entry, &color.b.to_string());
            let hsl = rgb_to_hsl(color);
            set_entry_text_if_needed(&h_entry, &format!("{}", hsl.hue.round() as i32));
            set_entry_text_if_needed(&s_entry, &format_percent(hsl.saturation));
            set_entry_text_if_needed(&l_entry, &format_percent(hsl.lightness));
            opacity_value.set_text(&format!("{}%", (color.a as f64 * 100.0 / 255.0).round()));
            header_preview.queue_draw();
            current_preview.queue_draw();
            hsv_area.queue_draw();
            hue_bar.queue_draw();
            opacity_bar.queue_draw();
            if let Ok(stroke_previews) = stroke_previews.try_borrow() {
                for preview in stroke_previews.iter() {
                    preview.queue_draw();
                }
            }
            if let Ok(favorites) = favorite_colors.try_borrow() {
                update_color_memory_buttons(&favorites, &favorite_buttons);
                if favorites.iter().any(|favorite| *favorite == color) {
                    add_favorite.set_label("★");
                    add_favorite.add_css_class("suggested-action");
                } else {
                    add_favorite.set_label("☆");
                    add_favorite.remove_css_class("suggested-action");
                }
            }
            if style_changed {
                update_history_action_buttons(&history, &undo, &redo);
                queue_render_cache();
                canvas.queue_draw();
            }
            refresh_status(None);
            refreshing.set(false);
        })
    };
    *color_ui_refresh.borrow_mut() = Some(refresh_ui.clone());
    *recent_color_recorder.borrow_mut() = Some({
        let recent_colors = recent_colors.clone();
        let recent_buttons = recent_buttons.clone();
        let state = state.clone();
        Rc::new(move |color: Color| {
            if let Ok(mut recent) = recent_colors.try_borrow_mut() {
                push_recent_color(&mut recent, color, COLOR_ROW_LIMIT);
                update_color_memory_buttons(&recent, &recent_buttons);
                let persisted = recent.clone();
                persist_config_change(&state, |config| {
                    config.recent_colors = persisted;
                });
            }
        })
    });

    let apply_hsv_from_position: Rc<dyn Fn(&DrawingArea, f64, f64)> = {
        let hsv_state = hsv_state.clone();
        let alpha_state = alpha_state.clone();
        let refresh_ui = refresh_ui.clone();
        Rc::new(move |area: &DrawingArea, x: f64, y: f64| {
            let width = area.allocated_width().max(1) as f64;
            let height = area.allocated_height().max(1) as f64;
            let mut hsv = hsv_state.get();
            hsv.saturation = (x / width).clamp(0.0, 1.0);
            hsv.value = (1.0 - y / height).clamp(0.0, 1.0);
            refresh_ui(hsv_to_color(hsv, alpha_state.get()));
        })
    };

    let hsv_click = gtk::GestureClick::new();
    let hsv_area_for_click = hsv_area.clone();
    let apply_for_click = apply_hsv_from_position.clone();
    hsv_click.connect_pressed(move |_, _, x, y| {
        apply_for_click(&hsv_area_for_click, x, y);
    });
    hsv_area.add_controller(hsv_click);

    let hsv_drag = gtk::GestureDrag::new();
    let hsv_area_for_drag = hsv_area.clone();
    hsv_drag.connect_drag_update(move |gesture, dx, dy| {
        let (start_x, start_y) = gesture.start_point().unwrap_or((0.0, 0.0));
        apply_hsv_from_position(&hsv_area_for_drag, start_x + dx, start_y + dy);
    });
    hsv_area.add_controller(hsv_drag);

    let apply_hue_from_position: Rc<dyn Fn(&DrawingArea, f64)> = {
        let refresh = refresh_ui.clone();
        let hsv_state = hsv_state.clone();
        let alpha_state = alpha_state.clone();
        Rc::new(move |area: &DrawingArea, x: f64| {
            let mut hsv = hsv_state.get();
            hsv.hue = slider_value_from_x(area, x) * 360.0;
            refresh(hsv_to_color(hsv, alpha_state.get()));
        })
    };
    let hue_click = gtk::GestureClick::new();
    let hue_bar_for_click = hue_bar.clone();
    let apply_hue_for_click = apply_hue_from_position.clone();
    hue_click.connect_pressed(move |_, _, x, _| {
        apply_hue_for_click(&hue_bar_for_click, x);
    });
    hue_bar.add_controller(hue_click);
    let hue_drag = gtk::GestureDrag::new();
    let hue_bar_for_drag = hue_bar.clone();
    hue_drag.connect_drag_update(move |gesture, dx, _| {
        let (start_x, _) = gesture.start_point().unwrap_or((0.0, 0.0));
        apply_hue_from_position(&hue_bar_for_drag, start_x + dx);
    });
    hue_bar.add_controller(hue_drag);

    let apply_opacity_from_position: Rc<dyn Fn(&DrawingArea, f64)> = {
        let refresh = refresh_ui.clone();
        let hsv_state = hsv_state.clone();
        Rc::new(move |area: &DrawingArea, x: f64| {
            let alpha = (slider_value_from_x(area, x) * 255.0).round().clamp(0.0, 255.0) as u8;
            refresh(hsv_to_color(hsv_state.get(), alpha));
        })
    };
    let opacity_click = gtk::GestureClick::new();
    let opacity_bar_for_click = opacity_bar.clone();
    let apply_opacity_for_click = apply_opacity_from_position.clone();
    opacity_click.connect_pressed(move |_, _, x, _| {
        apply_opacity_for_click(&opacity_bar_for_click, x);
    });
    opacity_bar.add_controller(opacity_click);
    let opacity_drag = gtk::GestureDrag::new();
    let opacity_bar_for_drag = opacity_bar.clone();
    opacity_drag.connect_drag_update(move |gesture, dx, _| {
        let (start_x, _) = gesture.start_point().unwrap_or((0.0, 0.0));
        apply_opacity_from_position(&opacity_bar_for_drag, start_x + dx);
    });
    opacity_bar.add_controller(opacity_drag);

    for (button, name) in [(&hex_tab, "hex"), (&rgb_tab, "rgb"), (&hsl_tab, "hsl")] {
        let stack = stack.clone();
        button.connect_clicked(move |_| {
            stack.set_visible_child_name(name);
        });
    }

    let refresh_for_hex = refresh_ui.clone();
    let alpha_for_hex = alpha_state.clone();
    hex_entry.connect_activate(move |entry| {
        if let Some(color) = parse_hex_color(entry.text().as_str(), alpha_for_hex.get()) {
            refresh_for_hex(color);
        }
    });

    for entry in [&r_entry, &g_entry, &b_entry] {
        let refresh = refresh_ui.clone();
        let r_entry = r_entry.clone();
        let g_entry = g_entry.clone();
        let b_entry = b_entry.clone();
        let alpha = alpha_state.clone();
        entry.connect_activate(move |_| {
            let parse = |entry: &Entry| entry.text().parse::<u8>().ok();
            if let (Some(r), Some(g), Some(b)) = (parse(&r_entry), parse(&g_entry), parse(&b_entry))
            {
                refresh(Color::rgba(r, g, b, alpha.get()));
            }
        });
    }

    for entry in [&h_entry, &s_entry, &l_entry] {
        let refresh = refresh_ui.clone();
        let h_entry = h_entry.clone();
        let s_entry = s_entry.clone();
        let l_entry = l_entry.clone();
        let alpha = alpha_state.clone();
        entry.connect_activate(move |_| {
            let parse_percent = |entry: &Entry| {
                entry
                    .text()
                    .trim()
                    .trim_end_matches('%')
                    .parse::<f64>()
                    .ok()
                    .map(|value| (value / 100.0).clamp(0.0, 1.0))
            };
            if let (Ok(hue), Some(saturation), Some(lightness)) = (
                h_entry.text().trim().parse::<f64>(),
                parse_percent(&s_entry),
                parse_percent(&l_entry),
            ) {
                refresh(hsl_to_color(HslColor { hue, saturation, lightness }, alpha.get()));
            }
        });
    }

    if let Ok(buttons) = recent_buttons.try_borrow() {
        for (color_cell, _, button) in buttons.iter() {
            let refresh = refresh_ui.clone();
            let color_cell = color_cell.clone();
            button.connect_clicked(move |_| {
                let color = color_cell.get();
                if color.a > 0 {
                    refresh(color);
                }
            });
        }
    }

    if let Ok(buttons) = favorite_buttons.try_borrow() {
        for (color_cell, _, button) in buttons.iter() {
            let refresh = refresh_ui.clone();
            let color_cell_for_click = color_cell.clone();
            let left_click = gtk::GestureClick::new();
            left_click.set_button(1);
            left_click.connect_pressed(move |_, _, _, _| {
                let color = color_cell_for_click.get();
                if color.a > 0 {
                    refresh(color);
                }
            });
            button.add_controller(left_click);

            let favorites_for_remove = favorite_colors.clone();
            let favorite_buttons_for_remove = favorite_buttons.clone();
            let color_cell_for_remove = color_cell.clone();
            let button_for_remove = button.clone();
            let active_color_for_remove = active_color.clone();
            let state_for_remove = state.clone();
            let right_click = gtk::GestureClick::new();
            right_click.set_button(3);
            let refresh_after_remove = refresh_ui.clone();
            right_click.connect_pressed(move |_, _, _, _| {
                let color = color_cell_for_remove.get();
                if color.a == 0 {
                    return;
                }
                show_favorite_remove_popover(
                    &button_for_remove,
                    color,
                    active_color_for_remove.clone(),
                    favorites_for_remove.clone(),
                    favorite_buttons_for_remove.clone(),
                    refresh_after_remove.clone(),
                    state_for_remove.clone(),
                );
            });
            button.add_controller(right_click);
        }
    }

    let refresh_for_favorite = refresh_ui.clone();
    let active_for_favorite = active_color.clone();
    let favorites_for_add = favorite_colors.clone();
    let favorite_buttons_for_add = favorite_buttons.clone();
    let state_for_favorite = state.clone();
    let show_toast_for_favorite = show_toast.clone();
    add_favorite.connect_clicked(move |_| {
        favorite_error.set_text("");
        let color = active_for_favorite.get();
        let mut should_refresh = false;
        let mut favorite_limit = None;
        if let Ok(mut favorites) = favorites_for_add.try_borrow_mut() {
            if favorites.iter().any(|favorite| *favorite == color) {
                remove_favorite_color(&mut favorites, color);
                update_color_memory_buttons(&favorites, &favorite_buttons_for_add);
                let persisted = favorites.clone();
                persist_config_change(&state_for_favorite, |config| {
                    config.favorite_colors = persisted;
                });
                should_refresh = true;
            } else {
                match add_favorite_color(&mut favorites, color, COLOR_ROW_LIMIT) {
                    Ok(()) => {
                        update_color_memory_buttons(&favorites, &favorite_buttons_for_add);
                        let persisted = favorites.clone();
                        persist_config_change(&state_for_favorite, |config| {
                            config.favorite_colors = persisted;
                        });
                        should_refresh = true;
                    }
                    Err(limit) => {
                        favorite_limit = Some(limit);
                    }
                }
            }
        }
        if let Some(limit) = favorite_limit {
            show_toast_for_favorite(&format!("Favorite can save up to {limit} colors"));
        }
        if should_refresh {
            refresh_for_favorite(color);
        }
    });

    refresh_ui(active_color.get());
    popover.set_child(Some(&root));
    menu_button.set_popover(Some(&popover));
    container
}

fn current_stroke_preview(
    active_color: Rc<Cell<Color>>,
    active_stroke: Rc<Cell<u32>>,
    width: i32,
    height: i32,
) -> DrawingArea {
    let area = DrawingArea::new();
    area.set_content_width(width);
    area.set_content_height(height);
    area.set_draw_func(move |_, cr, area_width, area_height| {
        let stroke_width = active_stroke.get();
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.10);
        cr.set_line_width(stroke_width as f64 + 1.4);
        cr.set_line_cap(gtk::cairo::LineCap::Round);
        let y = area_height as f64 / 2.0;
        cr.move_to(8.0, y);
        cr.line_to(area_width as f64 - 8.0, y);
        let _ = cr.stroke();
        set_cairo_color(cr, active_color.get());
        cr.set_line_width(stroke_width as f64);
        cr.set_line_cap(gtk::cairo::LineCap::Round);
        cr.move_to(8.0, y);
        cr.line_to(area_width as f64 - 8.0, y);
        let _ = cr.stroke();
    });
    area
}

fn fixed_stroke_preview(width: u32, active_color: Rc<Cell<Color>>) -> DrawingArea {
    let area = DrawingArea::new();
    area.set_content_width(74);
    area.set_content_height(STROKE_PREVIEW_HEIGHT);
    area.set_draw_func(move |_, cr, area_width, area_height| {
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.10);
        cr.set_line_width(width as f64 + 1.4);
        cr.set_line_cap(gtk::cairo::LineCap::Round);
        let y = area_height as f64 / 2.0;
        cr.move_to(8.0, y);
        cr.line_to(area_width as f64 - 8.0, y);
        let _ = cr.stroke();
        set_cairo_color(cr, active_color.get());
        cr.set_line_width(width as f64);
        cr.set_line_cap(gtk::cairo::LineCap::Round);
        cr.move_to(8.0, y);
        cr.line_to(area_width as f64 - 8.0, y);
        let _ = cr.stroke();
    });
    area
}

fn stroke_width_dropdown(
    active_stroke: Rc<Cell<u32>>,
    active_color: Rc<Cell<Color>>,
    stroke_previews: Rc<RefCell<Vec<DrawingArea>>>,
    document: Rc<RefCell<Document>>,
    history: Rc<RefCell<EditorHistory>>,
    canvas: DrawingArea,
    undo: Button,
    redo: Button,
    state: Arc<ServiceState>,
    refresh_status: StatusCallback,
    queue_render_cache: Rc<dyn Fn()>,
) -> GtkBox {
    let container = GtkBox::new(Orientation::Horizontal, 8);
    container.add_css_class("ashot-stroke-control");
    let current = current_stroke_preview(
        active_color.clone(),
        active_stroke.clone(),
        STROKE_PREVIEW_WIDTH,
        STROKE_PREVIEW_HEIGHT,
    );
    current.set_hexpand(true);
    stroke_previews.borrow_mut().push(current.clone());

    let menu_button = MenuButton::new();
    menu_button.set_label(&format!("{}px", active_stroke.get()));
    menu_button.set_tooltip_text(Some("Stroke width / effect strength"));
    menu_button.set_size_request(STROKE_MENU_BUTTON_WIDTH, STROKE_MENU_BUTTON_HEIGHT);
    menu_button.add_css_class("ashot-compact-menu");

    let popover = Popover::new();
    let list = GtkBox::new(Orientation::Vertical, 4);
    list.add_css_class("ashot-stroke-menu");
    list.set_margin_top(6);
    list.set_margin_bottom(6);
    list.set_margin_start(6);
    list.set_margin_end(6);

    for width in editor_stroke_widths() {
        let preview = fixed_stroke_preview(width, active_color.clone());
        stroke_previews.borrow_mut().push(preview.clone());
        let row = GtkBox::new(Orientation::Horizontal, 8);
        row.append(&preview);
        row.append(&Label::new(Some(&format!("{width}px"))));

        let button = Button::new();
        button.set_child(Some(&row));
        button.set_tooltip_text(Some(&format!("Use {width}px stroke or effect strength")));
        button.add_css_class("flat");
        button.add_css_class("ashot-stroke-option");
        button.set_hexpand(true);

        let active_stroke_for_click = active_stroke.clone();
        let stroke_previews_for_click = stroke_previews.clone();
        let menu_button_for_click = menu_button.clone();
        let document_for_click = document.clone();
        let history_for_click = history.clone();
        let canvas_for_click = canvas.clone();
        let undo_for_click = undo.clone();
        let redo_for_click = redo.clone();
        let state_for_click = state.clone();
        let refresh_status_for_click = refresh_status.clone();
        let queue_render_cache_for_click = queue_render_cache.clone();
        button.connect_clicked(move |_| {
            active_stroke_for_click.set(width);
            persist_config_change(&state_for_click, |config| {
                config.default_stroke_width = width;
            });
            let style_changed = if let Ok(mut document) = document_for_click.try_borrow_mut() {
                if document.active_tool == DefaultTool::Select && document.selected.is_some() {
                    let before = document.annotations.clone();
                    if document.apply_stroke_to_selected(width) {
                        if let Ok(mut history) = history_for_click.try_borrow_mut() {
                            history.snapshot(&before);
                        }
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            };
            menu_button_for_click.set_label(&format!("{width}px"));
            if let Ok(stroke_previews) = stroke_previews_for_click.try_borrow() {
                for preview in stroke_previews.iter() {
                    preview.queue_draw();
                }
            }
            if style_changed {
                update_history_action_buttons(&history_for_click, &undo_for_click, &redo_for_click);
                queue_render_cache_for_click();
                canvas_for_click.queue_draw();
            }
            refresh_status_for_click(None);
            menu_button_for_click.popdown();
        });
        list.append(&button);
    }

    popover.set_child(Some(&list));
    menu_button.set_popover(Some(&popover));
    container.append(&current);
    container.append(&menu_button);
    container
}

fn update_tool_button_selection(
    buttons: &Rc<RefCell<Vec<(DefaultTool, Button)>>>,
    selected: DefaultTool,
) {
    if let Ok(buttons) = buttons.try_borrow() {
        for (tool, button) in buttons.iter() {
            if *tool == selected {
                button.add_css_class("active-tool");
                button.remove_css_class("suggested-action");
            } else {
                button.remove_css_class("active-tool");
                button.remove_css_class("suggested-action");
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorCursorKind {
    Default,
    Text,
    Crosshair,
    Eyedropper,
}

fn editor_cursor_kind_for_tool(tool: DefaultTool) -> EditorCursorKind {
    match tool {
        DefaultTool::Select => EditorCursorKind::Default,
        DefaultTool::Text => EditorCursorKind::Text,
        DefaultTool::ColorPicker => EditorCursorKind::Eyedropper,
        DefaultTool::Line
        | DefaultTool::Arrow
        | DefaultTool::Brush
        | DefaultTool::Rectangle
        | DefaultTool::Ellipse
        | DefaultTool::Marker
        | DefaultTool::Mosaic
        | DefaultTool::Blur
        | DefaultTool::Counter
        | DefaultTool::FilledBox
        | DefaultTool::Ocr => EditorCursorKind::Crosshair,
    }
}

#[cfg(test)]
fn editor_cursor_for_tool(tool: DefaultTool) -> Option<&'static str> {
    match editor_cursor_kind_for_tool(tool) {
        EditorCursorKind::Default => Some("default"),
        EditorCursorKind::Text => Some("text"),
        EditorCursorKind::Crosshair => Some("crosshair"),
        EditorCursorKind::Eyedropper => None,
    }
}

fn apply_editor_cursor(canvas: &DrawingArea, tool: DefaultTool) {
    match editor_cursor_kind_for_tool(tool) {
        EditorCursorKind::Default => canvas.set_cursor(None),
        EditorCursorKind::Text => canvas.set_cursor_from_name(Some("text")),
        EditorCursorKind::Crosshair => canvas.set_cursor_from_name(Some("crosshair")),
        EditorCursorKind::Eyedropper => {
            let cursor = editor_eyedropper_cursor();
            canvas.set_cursor(Some(&cursor));
        }
    }
}

fn apply_canvas_hover_cursor(
    canvas: &DrawingArea,
    document: &Document,
    point: Point,
    tolerance: f32,
) {
    if document.active_tool != DefaultTool::Select {
        apply_editor_cursor(canvas, document.active_tool);
        return;
    }
    if let Some(handle) = resize_handle_at(document, point, tolerance) {
        canvas.set_cursor_from_name(Some(cursor_name_for_resize_handle(handle)));
        return;
    }
    if selected_annotation_bounds(document).is_some_and(|bounds| bounds.contains(point)) {
        canvas.set_cursor_from_name(Some("move"));
    } else {
        canvas.set_cursor(None);
    }
}

fn cursor_name_for_resize_handle(handle: ResizeHandle) -> &'static str {
    match handle {
        ResizeHandle::TopLeft | ResizeHandle::BottomRight => "nwse-resize",
        ResizeHandle::TopRight | ResizeHandle::BottomLeft => "nesw-resize",
        ResizeHandle::Top | ResizeHandle::Bottom => "ns-resize",
        ResizeHandle::Left | ResizeHandle::Right => "ew-resize",
    }
}

fn editor_eyedropper_cursor() -> gtk::gdk::Cursor {
    let pixels = eyedropper_cursor_pixels();
    let bytes = glib::Bytes::from_owned(pixels);
    let texture =
        gtk::gdk::MemoryTexture::new(32, 32, gtk::gdk::MemoryFormat::R8g8b8a8, &bytes, 32 * 4);
    let fallback = gtk::gdk::Cursor::from_name("crosshair", None);
    gtk::gdk::Cursor::from_texture(&texture, 6, 26, fallback.as_ref())
}

fn eyedropper_cursor_pixels() -> Vec<u8> {
    let mut pixels = vec![0; 32 * 32 * 4];
    let white = [255, 255, 255, 240];
    let black = [18, 18, 18, 255];

    draw_cursor_line(&mut pixels, Point::new(7.0, 25.0), Point::new(20.0, 12.0), 3.4, white);
    draw_cursor_line(&mut pixels, Point::new(7.0, 25.0), Point::new(20.0, 12.0), 1.35, black);
    draw_cursor_line(&mut pixels, Point::new(11.0, 21.0), Point::new(24.0, 8.0), 3.4, white);
    draw_cursor_line(&mut pixels, Point::new(11.0, 21.0), Point::new(24.0, 8.0), 1.35, black);
    draw_cursor_line(&mut pixels, Point::new(18.0, 8.0), Point::new(26.0, 16.0), 4.6, white);
    draw_cursor_line(&mut pixels, Point::new(18.0, 8.0), Point::new(26.0, 16.0), 2.2, black);
    draw_cursor_line(&mut pixels, Point::new(19.0, 9.0), Point::new(25.0, 15.0), 1.2, white);
    draw_cursor_line(&mut pixels, Point::new(5.0, 27.0), Point::new(8.0, 24.0), 2.9, white);
    draw_cursor_line(&mut pixels, Point::new(5.0, 27.0), Point::new(8.0, 24.0), 1.25, black);

    pixels
}

fn draw_cursor_line(pixels: &mut [u8], start: Point, end: Point, radius: f32, color: [u8; 4]) {
    for y in 0..32 {
        for x in 0..32 {
            let point = Point::new(x as f32 + 0.5, y as f32 + 0.5);
            if distance_to_segment(point, start, end) <= radius {
                let index = ((y * 32 + x) * 4) as usize;
                pixels[index..index + 4].copy_from_slice(&color);
            }
        }
    }
}

fn distance_to_segment(point: Point, start: Point, end: Point) -> f32 {
    let vx = end.x - start.x;
    let vy = end.y - start.y;
    let wx = point.x - start.x;
    let wy = point.y - start.y;
    let length_squared = vx * vx + vy * vy;
    if length_squared <= f32::EPSILON {
        let dx = point.x - start.x;
        let dy = point.y - start.y;
        return (dx * dx + dy * dy).sqrt();
    }
    let t = ((wx * vx + wy * vy) / length_squared).clamp(0.0, 1.0);
    let projection = Point::new(start.x + t * vx, start.y + t * vy);
    let dx = point.x - projection.x;
    let dy = point.y - projection.y;
    (dx * dx + dy * dy).sqrt()
}

fn update_history_action_buttons(
    history: &Rc<RefCell<EditorHistory>>,
    undo: &Button,
    redo: &Button,
) {
    let Ok(history) = history.try_borrow() else {
        return;
    };
    let undo_count = history.undo_count();
    let redo_count = history.redo_count();
    undo.set_sensitive(undo_count > 0);
    redo.set_sensitive(redo_count > 0);
    undo.set_tooltip_text(Some(&format!("Undo ({undo_count}/{})", history.limit())));
    redo.set_tooltip_text(Some(&format!("Redo ({redo_count})")));
}

fn draft_tool_can_draw(tool: DefaultTool) -> bool {
    matches!(
        tool,
        DefaultTool::Line
            | DefaultTool::Arrow
            | DefaultTool::Brush
            | DefaultTool::Rectangle
            | DefaultTool::Ellipse
            | DefaultTool::Marker
            | DefaultTool::Mosaic
            | DefaultTool::Blur
            | DefaultTool::FilledBox
            | DefaultTool::Ocr
    )
}

fn tool_can_select_existing(tool: DefaultTool) -> bool {
    tool == DefaultTool::Select
}

fn tool_picks_canvas_color(tool: DefaultTool) -> bool {
    tool == DefaultTool::ColorPicker
}

fn annotation_shows_selection_overlay(annotation: &Annotation) -> bool {
    matches!(
        annotation.data,
        AnnotationData::Line { .. }
            | AnnotationData::Arrow { .. }
            | AnnotationData::Rectangle { .. }
            | AnnotationData::Ellipse { .. }
            | AnnotationData::Mosaic { .. }
            | AnnotationData::Blur { .. }
            | AnnotationData::Counter { .. }
            | AnnotationData::FilledBox { .. }
    )
}

fn annotation_keeps_selection_after_creation(annotation: &Annotation) -> bool {
    annotation_shows_selection_overlay(annotation)
}

fn selected_annotation_bounds(document: &Document) -> Option<Rect> {
    let id = document.selected?;
    document
        .annotations
        .iter()
        .find(|annotation| annotation.id == id && annotation_shows_selection_overlay(annotation))
        .map(Annotation::bounds)
}

fn resize_handle_points(rect: Rect) -> [(ResizeHandle, Point); 8] {
    let left = rect.x;
    let center_x = rect.x + rect.width * 0.5;
    let right = rect.x + rect.width;
    let top = rect.y;
    let center_y = rect.y + rect.height * 0.5;
    let bottom = rect.y + rect.height;
    [
        (ResizeHandle::TopLeft, Point::new(left, top)),
        (ResizeHandle::Top, Point::new(center_x, top)),
        (ResizeHandle::TopRight, Point::new(right, top)),
        (ResizeHandle::Right, Point::new(right, center_y)),
        (ResizeHandle::BottomRight, Point::new(right, bottom)),
        (ResizeHandle::Bottom, Point::new(center_x, bottom)),
        (ResizeHandle::BottomLeft, Point::new(left, bottom)),
        (ResizeHandle::Left, Point::new(left, center_y)),
    ]
}

fn resize_handle_at(document: &Document, point: Point, tolerance: f32) -> Option<ResizeHandle> {
    let bounds = selected_annotation_bounds(document)?;
    resize_handle_points(bounds).iter().find_map(|(handle, center)| {
        let dx = point.x - center.x;
        let dy = point.y - center.y;
        ((dx * dx + dy * dy).sqrt() <= tolerance).then_some(*handle)
    })
}

fn draw_selection_overlay(cr: &gtk::cairo::Context, rect: Rect) {
    let _ = cr.save();
    cr.set_source_rgba(0.13, 0.48, 0.95, 0.88);
    cr.set_line_width(1.4);
    cr.rectangle(rect.x as f64, rect.y as f64, rect.width as f64, rect.height as f64);
    let _ = cr.stroke();

    for (_, center) in resize_handle_points(rect) {
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.96);
        cr.arc(
            center.x as f64,
            center.y as f64,
            SELECTION_HANDLE_RADIUS,
            0.0,
            std::f64::consts::TAU,
        );
        let _ = cr.fill_preserve();
        cr.set_source_rgba(0.13, 0.48, 0.95, 0.95);
        cr.set_line_width(1.4);
        let _ = cr.stroke();
    }
    let _ = cr.restore();
}

#[derive(Debug, Clone)]
struct DraftAnnotation {
    tool: DefaultTool,
    start: Point,
    points: Vec<Point>,
    color: Color,
    stroke_width: u32,
}

#[derive(Debug, Clone)]
struct ActiveTextEdit {
    id: Option<ashot_core::AnnotationId>,
    origin: Point,
    color: Color,
}

fn position_text_entry(entry: &Entry, x: f64, y: f64) {
    entry.set_margin_start(x.round().max(0.0) as i32);
    entry.set_margin_top(y.round().max(0.0) as i32);
}

fn text_for_annotation(document: &Document, id: ashot_core::AnnotationId) -> Option<String> {
    document.annotations.iter().find_map(|annotation| {
        if annotation.id != id {
            return None;
        }
        match &annotation.data {
            AnnotationData::Text { text, .. } => Some(text.clone()),
            _ => None,
        }
    })
}

fn moving_delta_and_update(
    moving: &Rc<RefCell<Option<Point>>>,
    current: Point,
) -> Option<(f32, f32)> {
    let previous = {
        let moving = moving.try_borrow().ok()?;
        (*moving)?
    };

    let delta = (current.x - previous.x, current.y - previous.y);
    *moving.try_borrow_mut().ok()? = Some(current);
    Some(delta)
}

fn set_active_text_edit(
    active_text_edit: &Rc<RefCell<Option<ActiveTextEdit>>>,
    edit: ActiveTextEdit,
) -> bool {
    let Ok(mut active_text_edit) = active_text_edit.try_borrow_mut() else {
        return false;
    };
    *active_text_edit = Some(edit);
    true
}

fn take_active_text_edit(
    active_text_edit: &Rc<RefCell<Option<ActiveTextEdit>>>,
) -> Option<ActiveTextEdit> {
    active_text_edit.try_borrow_mut().ok().and_then(|mut edit| edit.take())
}

fn commit_text_entry(
    entry: &Entry,
    document: &Rc<RefCell<Document>>,
    history: &Rc<RefCell<EditorHistory>>,
    canvas: &DrawingArea,
    active_text_edit: &Rc<RefCell<Option<ActiveTextEdit>>>,
    undo: &Button,
    redo: &Button,
    recent_recorder: &Rc<RefCell<Option<ColorCallback>>>,
    queue_render_cache: &Rc<dyn Fn()>,
) {
    let Some(edit) = take_active_text_edit(active_text_edit) else {
        return;
    };

    let text = entry.text().trim().to_string();
    if text.is_empty() {
        entry.set_visible(false);
        entry.set_text("");
        return;
    }
    let edit_color = edit.color;

    let committed = if let Ok(mut document) = document.try_borrow_mut() {
        if let Ok(mut history) = history.try_borrow_mut() {
            history.snapshot(&document.annotations);
        }
        if let Some(id) = edit.id {
            let _ = document.update_text_annotation(id, text);
        } else {
            document.add_annotation(Annotation::new(AnnotationData::Text {
                origin: edit.origin,
                text,
                style: TextStyle { size: 20, weight: TextWeight::Bold, color: edit.color },
            }));
        }
        true
    } else {
        set_active_text_edit(active_text_edit, edit)
    };

    if committed {
        if let Some(record_recent) = recent_recorder.borrow().as_ref().cloned() {
            record_recent(edit_color);
        }
        update_history_action_buttons(history, undo, redo);
        queue_render_cache();
        entry.set_visible(false);
        entry.set_text("");
        canvas.queue_draw();
    }
}

impl DraftAnnotation {
    fn new(tool: DefaultTool, start: Point, color: Color, stroke_width: u32) -> Self {
        Self { tool, start, points: vec![start], color, stroke_width }
    }

    fn extend(&mut self, point: Point) {
        if matches!(self.tool, DefaultTool::Brush | DefaultTool::Marker) {
            self.points.push(point);
        } else {
            self.points.truncate(1);
            self.points.push(point);
        }
    }

    fn finish(self) -> Option<Annotation> {
        if self.tool == DefaultTool::Ocr {
            return None;
        }
        self.preview_annotation()
    }

    fn bounds(&self) -> Rect {
        Rect::from_points(self.start, *self.points.last().unwrap_or(&self.start))
    }

    fn preview_annotation(&self) -> Option<Annotation> {
        let end = *self.points.last().unwrap_or(&self.start);
        match self.tool {
            DefaultTool::Line => Some(Annotation::new(AnnotationData::Line {
                start: self.start,
                end,
                color: self.color,
                stroke_width: self.stroke_width,
            })),
            DefaultTool::Arrow => Some(Annotation::new(AnnotationData::Arrow {
                start: self.start,
                end,
                color: self.color,
                stroke_width: self.stroke_width,
            })),
            DefaultTool::Brush => Some(Annotation::new(AnnotationData::Brush {
                points: self.points.clone(),
                color: self.color,
                stroke_width: self.stroke_width,
            })),
            DefaultTool::Rectangle => Some(Annotation::new(AnnotationData::Rectangle {
                rect: Rect::from_points(self.start, end),
                color: self.color,
                stroke_width: self.stroke_width,
            })),
            DefaultTool::Ellipse => Some(Annotation::new(AnnotationData::Ellipse {
                rect: Rect::from_points(self.start, end),
                color: self.color,
                stroke_width: self.stroke_width,
            })),
            DefaultTool::Marker => Some(Annotation::new(AnnotationData::Marker {
                points: self.points.clone(),
                color: Color::rgba(self.color.r, self.color.g, self.color.b, 96),
                stroke_width: self.stroke_width.max(8),
            })),
            DefaultTool::Mosaic => Some(Annotation::new(AnnotationData::Mosaic {
                rect: Rect::from_points(self.start, end),
                pixel_size: mosaic_pixel_size_for_stroke(self.stroke_width),
            })),
            DefaultTool::Blur => Some(Annotation::new(AnnotationData::Blur {
                rect: Rect::from_points(self.start, end),
                radius: blur_radius_for_stroke(self.stroke_width),
            })),
            DefaultTool::FilledBox => Some(Annotation::new(AnnotationData::FilledBox {
                rect: Rect::from_points(self.start, end),
                color: self.color,
            })),
            DefaultTool::Ocr => Some(Annotation::new(AnnotationData::Rectangle {
                rect: Rect::from_points(self.start, end),
                color: Color::rgba(14, 165, 233, 220),
                stroke_width: 2,
            })),
            DefaultTool::Select
            | DefaultTool::Text
            | DefaultTool::Counter
            | DefaultTool::ColorPicker => None,
        }
    }
}

#[cfg(test)]
fn arrow_head_points(start: Point, end: Point, stroke_width: u32) -> (Point, Point) {
    let (_, left, right) = arrow_head_geometry(start, end, stroke_width);
    (left, right)
}

#[cfg(test)]
fn arrow_head_geometry(start: Point, end: Point, stroke_width: u32) -> (Point, Point, Point) {
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    let length = (dx * dx + dy * dy).sqrt();
    if length <= f32::EPSILON {
        return (end, end, end);
    }

    let unit_x = dx / length;
    let unit_y = dy / length;
    let normal_x = -unit_y;
    let normal_y = unit_x;
    let (head_len, head_width) = arrow_head_dimensions(stroke_width);
    let head_len = head_len.min(length * 0.72);
    let half_width = head_width * 0.5;

    let base = Point::new(end.x - unit_x * head_len, end.y - unit_y * head_len);
    let left = Point::new(base.x + normal_x * half_width, base.y + normal_y * half_width);
    let right = Point::new(base.x - normal_x * half_width, base.y - normal_y * half_width);
    (base, left, right)
}

fn arrow_head_dimensions(stroke_width: u32) -> (f32, f32) {
    let stroke = stroke_width.max(1) as f32;
    ((stroke * 4.8).clamp(18.0, 54.0), (stroke * 5.2).clamp(20.0, 58.0))
}

fn arrow_visual_stroke_width(stroke_width: u32) -> u32 {
    ((stroke_width.max(1) as f32) * 1.7).round().clamp(6.0, 24.0) as u32
}

#[derive(Clone, Copy, Debug)]
struct ArrowShape {
    tail_left: Point,
    body_left: Point,
    head_left: Point,
    tip: Point,
    head_right: Point,
    body_right: Point,
    tail_right: Point,
}

fn arrow_shape_geometry(start: Point, end: Point, stroke_width: u32) -> ArrowShape {
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    let length = (dx * dx + dy * dy).sqrt();
    if length <= f32::EPSILON {
        return ArrowShape {
            tail_left: start,
            body_left: start,
            head_left: start,
            tip: end,
            head_right: start,
            body_right: start,
            tail_right: start,
        };
    }

    let unit_x = dx / length;
    let unit_y = dy / length;
    let normal_x = -unit_y;
    let normal_y = unit_x;
    let (head_len, head_width) = arrow_head_dimensions(stroke_width);
    let head_len = head_len.min(length * 0.72);
    let head_half = head_width * 0.5;
    let body_half = (stroke_width as f32 * 0.7).clamp(4.0, head_half * 0.55);
    let tail_half = (stroke_width as f32 * 0.24).clamp(1.8, body_half * 0.48);
    let base = Point::new(end.x - unit_x * head_len, end.y - unit_y * head_len);
    let body_join_offset = (stroke_width as f32 * 0.75).min(head_len * 0.28).max(0.0);
    let body_join =
        Point::new(base.x - unit_x * body_join_offset, base.y - unit_y * body_join_offset);

    ArrowShape {
        tail_left: Point::new(start.x + normal_x * tail_half, start.y + normal_y * tail_half),
        body_left: Point::new(
            body_join.x + normal_x * body_half,
            body_join.y + normal_y * body_half,
        ),
        head_left: Point::new(base.x + normal_x * head_half, base.y + normal_y * head_half),
        tip: end,
        head_right: Point::new(base.x - normal_x * head_half, base.y - normal_y * head_half),
        body_right: Point::new(
            body_join.x - normal_x * body_half,
            body_join.y - normal_y * body_half,
        ),
        tail_right: Point::new(start.x - normal_x * tail_half, start.y - normal_y * tail_half),
    }
}

fn mosaic_pixel_size_for_stroke(stroke_width: u32) -> u32 {
    match stroke_width {
        0..=2 => 6,
        3..=4 => 10,
        5..=6 => 14,
        7..=8 => 20,
        _ => 28,
    }
}

fn blur_radius_for_stroke(stroke_width: u32) -> u32 {
    stroke_width.clamp(2, 24)
}

fn draw_cairo_segment(
    cr: &gtk::cairo::Context,
    start: Point,
    end: Point,
    color: Color,
    stroke_width: u32,
) {
    let _ = cr.save();
    set_cairo_color(cr, color);
    cr.set_line_width(stroke_width.max(1) as f64);
    cr.set_line_cap(gtk::cairo::LineCap::Round);
    cr.move_to(start.x as f64, start.y as f64);
    cr.line_to(end.x as f64, end.y as f64);
    let _ = cr.stroke();
    let _ = cr.restore();
}

fn draw_cairo_arrow(
    cr: &gtk::cairo::Context,
    start: Point,
    end: Point,
    color: Color,
    stroke_width: u32,
) {
    let visual_stroke_width = arrow_visual_stroke_width(stroke_width);
    let shape = arrow_shape_geometry(start, end, visual_stroke_width);
    let _ = cr.save();
    set_cairo_color(cr, color);
    cr.set_line_width((visual_stroke_width as f64 * 0.42).clamp(2.0, 8.0));
    cr.set_line_cap(gtk::cairo::LineCap::Round);
    cr.set_line_join(gtk::cairo::LineJoin::Round);
    cr.move_to(shape.tail_left.x as f64, shape.tail_left.y as f64);
    cr.line_to(shape.body_left.x as f64, shape.body_left.y as f64);
    cr.line_to(shape.head_left.x as f64, shape.head_left.y as f64);
    cr.line_to(shape.tip.x as f64, shape.tip.y as f64);
    cr.line_to(shape.head_right.x as f64, shape.head_right.y as f64);
    cr.line_to(shape.body_right.x as f64, shape.body_right.y as f64);
    cr.line_to(shape.tail_right.x as f64, shape.tail_right.y as f64);
    cr.close_path();
    let _ = cr.fill_preserve();
    let _ = cr.stroke();
    let _ = cr.restore();
}

fn draw_mosaic_preview(cr: &gtk::cairo::Context, rect: Rect, pixel_size: u32) {
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return;
    }

    let block = pixel_size.max(4) as f64;
    let strength = ((pixel_size as f64 - 6.0) / 22.0).clamp(0.0, 1.0);
    let light_alpha = 0.24 + strength * 0.14;
    let dark_alpha = 0.34 + strength * 0.22;
    let x0 = rect.x as f64;
    let y0 = rect.y as f64;
    let x1 = x0 + rect.width as f64;
    let y1 = y0 + rect.height as f64;

    let _ = cr.save();
    cr.rectangle(x0, y0, rect.width as f64, rect.height as f64);
    cr.clip();

    let mut y = y0;
    let mut row = 0;
    while y < y1 {
        let mut x = x0;
        let mut column = 0;
        while x < x1 {
            if (row + column) % 2 == 0 {
                cr.set_source_rgba(0.04, 0.05, 0.06, dark_alpha);
            } else {
                cr.set_source_rgba(0.88, 0.90, 0.94, light_alpha);
            }
            cr.rectangle(x, y, block.min(x1 - x), block.min(y1 - y));
            let _ = cr.fill();
            x += block;
            column += 1;
        }
        y += block;
        row += 1;
    }

    let _ = cr.restore();
    cr.set_source_rgba(0.04, 0.05, 0.06, 0.56);
    cr.set_line_width(1.0);
    cr.rectangle(x0 + 0.5, y0 + 0.5, rect.width as f64 - 1.0, rect.height as f64 - 1.0);
    let _ = cr.stroke();
}

fn draw_blur_preview(cr: &gtk::cairo::Context, rect: Rect, radius: u32) {
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return;
    }

    let radius = radius.max(1) as f64;
    let x0 = rect.x as f64;
    let y0 = rect.y as f64;
    let width = rect.width as f64;
    let height = rect.height as f64;
    let x1 = x0 + width;
    let y1 = y0 + height;
    let spacing = (radius * 1.6).clamp(8.0, 22.0);
    let line_width = (radius * 0.45).clamp(2.0, 7.0);

    let _ = cr.save();
    cr.rectangle(x0, y0, width, height);
    cr.clip();

    cr.set_source_rgba(0.12, 0.42, 0.90, 0.14);
    cr.rectangle(x0, y0, width, height);
    let _ = cr.fill();

    let mut y = y0 + spacing * 0.7;
    let mut row = 0.0;
    while y < y1 {
        let inset = 6.0 + (row % 2.0) * spacing * 0.5;
        cr.set_source_rgba(0.45, 0.76, 1.0, 0.28);
        cr.set_line_width(line_width);
        cr.set_line_cap(gtk::cairo::LineCap::Round);
        cr.move_to((x0 + inset).min(x1), y);
        cr.curve_to(
            x0 + width * 0.32,
            y - spacing * 0.35,
            x0 + width * 0.66,
            y + spacing * 0.35,
            (x1 - inset).max(x0),
            y,
        );
        let _ = cr.stroke();
        y += spacing;
        row += 1.0;
    }

    let _ = cr.restore();
    cr.set_source_rgba(0.12, 0.42, 0.90, 0.54);
    cr.set_line_width(1.2);
    cr.rectangle(x0 + 0.5, y0 + 0.5, width - 1.0, height - 1.0);
    let _ = cr.stroke();
}

fn draw_annotation(cr: &gtk::cairo::Context, annotation: &Annotation) {
    match &annotation.data {
        AnnotationData::Text { origin, text, style } => {
            set_cairo_color(cr, style.color);
            cr.select_font_face(
                "Sans",
                gtk::cairo::FontSlant::Normal,
                match style.weight {
                    TextWeight::Regular => gtk::cairo::FontWeight::Normal,
                    TextWeight::Semibold | TextWeight::Bold => gtk::cairo::FontWeight::Bold,
                },
            );
            cr.set_font_size(style.size as f64);
            cr.move_to(origin.x as f64, origin.y as f64 + style.size as f64);
            let _ = cr.show_text(text);
        }
        AnnotationData::Line { start, end, color, stroke_width } => {
            draw_cairo_segment(cr, *start, *end, *color, *stroke_width);
        }
        AnnotationData::Arrow { start, end, color, stroke_width } => {
            draw_cairo_arrow(cr, *start, *end, *color, *stroke_width);
        }
        AnnotationData::Brush { points, color, stroke_width }
        | AnnotationData::Marker { points, color, stroke_width } => {
            if points.is_empty() {
                return;
            }
            set_cairo_color(cr, *color);
            cr.set_line_width(*stroke_width as f64);
            cr.move_to(points[0].x as f64, points[0].y as f64);
            for point in points.iter().skip(1) {
                cr.line_to(point.x as f64, point.y as f64);
            }
            let _ = cr.stroke();
        }
        AnnotationData::Rectangle { rect, color, stroke_width } => {
            set_cairo_color(cr, *color);
            cr.set_line_width(*stroke_width as f64);
            cr.rectangle(rect.x as f64, rect.y as f64, rect.width as f64, rect.height as f64);
            let _ = cr.stroke();
        }
        AnnotationData::Ellipse { rect, color, stroke_width } => {
            set_cairo_color(cr, *color);
            cr.set_line_width(*stroke_width as f64);
            let _ = cr.save();
            cr.translate((rect.x + rect.width / 2.0) as f64, (rect.y + rect.height / 2.0) as f64);
            cr.scale((rect.width / 2.0).max(1.0) as f64, (rect.height / 2.0).max(1.0) as f64);
            cr.arc(0.0, 0.0, 1.0, 0.0, std::f64::consts::TAU);
            let _ = cr.restore();
            let _ = cr.stroke();
        }
        AnnotationData::Mosaic { rect, pixel_size } => {
            draw_mosaic_preview(cr, *rect, *pixel_size);
        }
        AnnotationData::Blur { rect, radius } => {
            draw_blur_preview(cr, *rect, *radius);
        }
        AnnotationData::Counter { center, number, color, radius } => {
            set_cairo_color(cr, *color);
            cr.arc(center.x as f64, center.y as f64, *radius as f64, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
            cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
            cr.select_font_face(
                "Sans",
                gtk::cairo::FontSlant::Normal,
                gtk::cairo::FontWeight::Bold,
            );
            cr.set_font_size((*radius as f64).max(10.0));
            let text = number.to_string();
            cr.move_to(
                center.x as f64 - (*radius as f64 * 0.35),
                center.y as f64 + (*radius as f64 * 0.35),
            );
            let _ = cr.show_text(&text);
        }
        AnnotationData::FilledBox { rect, color } => {
            set_cairo_color(cr, *color);
            cr.rectangle(rect.x as f64, rect.y as f64, rect.width as f64, rect.height as f64);
            let _ = cr.fill();
        }
    }
}

fn draw_brush_cursor_preview(
    cr: &gtk::cairo::Context,
    point: Point,
    color: Color,
    stroke_width: u32,
) {
    let radius = (stroke_width.max(2) as f64 * 0.5).max(2.5);
    let _ = cr.save();
    cr.set_source_rgba(
        color.r as f64 / 255.0,
        color.g as f64 / 255.0,
        color.b as f64 / 255.0,
        0.20,
    );
    cr.arc(point.x as f64, point.y as f64, radius, 0.0, std::f64::consts::TAU);
    let _ = cr.fill_preserve();
    cr.set_source_rgba(
        color.r as f64 / 255.0,
        color.g as f64 / 255.0,
        color.b as f64 / 255.0,
        0.68,
    );
    cr.set_line_width(1.2);
    let _ = cr.stroke();
    let _ = cr.restore();
}

fn set_cairo_color(cr: &gtk::cairo::Context, color: Color) {
    cr.set_source_rgba(
        color.r as f64 / 255.0,
        color.g as f64 / 255.0,
        color.b as f64 / 255.0,
        color.a as f64 / 255.0,
    );
}

fn copy_image_to_clipboard(path: &std::path::Path) {
    if let Some(display) = gtk::gdk::Display::default() {
        if let Ok(bytes) = fs::read(path) {
            let bytes = glib::Bytes::from_owned(bytes);
            let provider = gtk::gdk::ContentProvider::for_bytes("image/png", &bytes);
            if display.clipboard().set_content(Some(&provider)).is_ok() {
                return;
            }
        }
        let file = gio::File::for_path(path);
        if let Ok(texture) = gtk::gdk::Texture::from_file(&file) {
            display.clipboard().set_texture(&texture);
        }
    }
}

#[cfg(test)]
fn render_document_png_bytes(
    base: &image::DynamicImage,
    annotations: &[Annotation],
) -> image::ImageResult<Vec<u8>> {
    let rendered = render_document(base, annotations);
    let mut cursor = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(rendered).write_to(&mut cursor, image::ImageFormat::Png)?;
    Ok(cursor.into_inner())
}

fn copy_png_bytes_to_clipboard(png_bytes: Arc<Vec<u8>>) -> std::result::Result<(), String> {
    let Some(display) = gtk::gdk::Display::default() else {
        return Err("clipboard is unavailable".to_string());
    };
    let bytes = glib::Bytes::from_owned(png_bytes.as_ref().clone());
    let provider = gtk::gdk::ContentProvider::for_bytes("image/png", &bytes);
    display
        .clipboard()
        .set_content(Some(&provider))
        .map_err(|error| format!("failed to copy image to clipboard: {error}"))
}

fn copy_text_to_clipboard(text: &str) {
    if let Some(display) = gtk::gdk::Display::default() {
        display.clipboard().set_text(text);
    }
}

fn normalized_save_filename(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    let file_name = Path::new(trimmed).file_name()?.to_string_lossy().trim().to_string();
    if file_name.is_empty() || file_name == "." || file_name == ".." {
        return None;
    }

    let mut path = PathBuf::from(file_name);
    if !path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("png")) {
        path.set_extension("png");
    }
    path.file_name().map(|name| name.to_string_lossy().to_string())
}

fn suggested_save_filename_at(config: &AppConfig, now: chrono::DateTime<chrono::Local>) -> String {
    normalized_save_filename(&render_filename(&config.filename_template, now))
        .unwrap_or_else(|| "Screenshot.png".to_string())
}

fn unique_temp_png_filename(prefix: &str) -> String {
    format!(
        "{prefix}_{}_{}.png",
        std::process::id(),
        chrono::Local::now().timestamp_nanos_opt().unwrap_or_default()
    )
}

#[cfg(test)]
fn save_editor_document_to_dir_with_filename(
    save_dir: &Path,
    base: &image::DynamicImage,
    annotations: &[Annotation],
    requested_filename: &str,
) -> std::result::Result<PathBuf, String> {
    let filename = normalized_save_filename(requested_filename)
        .ok_or_else(|| "Enter a file name".to_string())?;
    fs::create_dir_all(save_dir).map_err(|source| {
        format!("failed to create screenshot directory {}: {source}", save_dir.display())
    })?;
    let output = save_dir.join(filename);
    save_document_png(base, annotations, &output)
        .map_err(|source| format!("failed to save screenshot at {}: {source}", output.display()))?;
    Ok(output)
}

fn save_png_bytes_to_dir_async<F>(
    runtime: Handle,
    save_dir: PathBuf,
    png_bytes: Arc<Vec<u8>>,
    requested_filename: String,
    on_done: F,
) where
    F: FnOnce(std::result::Result<PathBuf, String>) + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    runtime.spawn_blocking(move || {
        let result =
            save_png_bytes_to_dir_with_filename(&save_dir, png_bytes.as_ref(), &requested_filename);
        let _ = tx.send(result);
    });

    let on_done = Rc::new(RefCell::new(Some(on_done)));
    glib::timeout_add_local(Duration::from_millis(40), move || match rx.try_recv() {
        Ok(result) => {
            if let Some(on_done) = on_done.borrow_mut().take() {
                on_done(result);
            }
            glib::ControlFlow::Break
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            if let Some(on_done) = on_done.borrow_mut().take() {
                on_done(Err("save task stopped unexpectedly".to_string()));
            }
            glib::ControlFlow::Break
        }
    });
}

fn crop_image_region(base: &image::DynamicImage, rect: Rect) -> Option<image::DynamicImage> {
    let x = rect.x.floor().max(0.0) as u32;
    let y = rect.y.floor().max(0.0) as u32;
    if x >= base.width() || y >= base.height() {
        return None;
    }

    let right = (rect.x + rect.width).ceil().max(0.0) as u32;
    let bottom = (rect.y + rect.height).ceil().max(0.0) as u32;
    let right = right.min(base.width());
    let bottom = bottom.min(base.height());
    if right <= x || bottom <= y {
        return None;
    }

    Some(base.crop_imm(x, y, right - x, bottom - y))
}

fn begin_ocr_request(
    parent: &ApplicationWindow,
    state: Arc<ServiceState>,
    runtime: Handle,
    base_image: &image::DynamicImage,
    rect: Rect,
    languages: Vec<String>,
    filter_symbols: bool,
    status: Option<StatusCallback>,
    toast: Option<ToastCallback>,
) {
    let Some(crop) = crop_image_region(base_image, rect) else {
        if let Some(status) = &status {
            status(None);
        }
        if let Some(toast) = &toast {
            toast("OCR failed: empty region");
        }
        show_ocr_result_dialog(parent, Err("Select a non-empty OCR region".to_string()));
        return;
    };
    let crop_path = std::env::temp_dir().join(format!(
        "ashot_ocr_{}_{}.png",
        std::process::id(),
        chrono::Local::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    if let Err(error) = crop.save(&crop_path) {
        if let Some(status) = &status {
            status(None);
        }
        if let Some(toast) = &toast {
            toast(&format!("OCR failed: {error}"));
        }
        show_ocr_result_dialog(parent, Err(format!("failed to prepare OCR image: {error}")));
        return;
    }

    if let Some(status) = &status {
        status(Some("Recognizing...".to_string()));
    }

    let mut config = state.config_snapshot();
    config.ocr_languages = normalize_ocr_languages(languages);
    config.ocr_filter_symbols = filter_symbols;
    let (tx, rx) = std::sync::mpsc::channel();
    runtime.spawn(async move {
        let result = run_ocr_backend(config, crop_path.clone()).await;
        let _ = fs::remove_file(&crop_path);
        let _ = tx.send(result);
    });

    let parent_for_poll = parent.clone();
    let status_for_poll = status.clone();
    let toast_for_poll = toast.clone();
    glib::timeout_add_local(Duration::from_millis(80), move || match rx.try_recv() {
        Ok(result) => {
            if let Some(status) = &status_for_poll {
                status(None);
            }
            if let Some(toast) = &toast_for_poll {
                match &result {
                    Ok(_) => toast("OCR finished"),
                    Err(error) => toast(&format!("OCR failed: {error}")),
                }
            }
            show_ocr_result_dialog(&parent_for_poll, result);
            glib::ControlFlow::Break
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            if let Some(status) = &status_for_poll {
                status(None);
            }
            if let Some(toast) = &toast_for_poll {
                toast("OCR failed: task stopped unexpectedly");
            }
            show_ocr_result_dialog(&parent_for_poll, Err("OCR task stopped unexpectedly".into()));
            glib::ControlFlow::Break
        }
    });
}

async fn run_ocr_backend(config: AppConfig, crop_path: PathBuf) -> Result<String, String> {
    let text = match config.ocr_backend {
        OcrBackend::Tesseract => run_tesseract_ocr(&config, &crop_path).await,
        OcrBackend::OcrSpace => run_ocr_space_ocr(&config, &crop_path).await,
    }?;

    if config.ocr_filter_symbols { Ok(filter_ocr_symbols(&text)) } else { Ok(text) }
}

async fn run_tesseract_ocr(config: &AppConfig, crop_path: &Path) -> Result<String, String> {
    let (program, args) =
        tesseract_command_invocation(crop_path, &config.ocr_languages, running_in_flatpak());
    let output = Command::new(&program).args(&args).output().await.map_err(|error| {
        format!(
            "failed to run {program}: {error}\n{}",
            language_install_command(&config.ocr_languages, LinuxDistroFamily::Unknown)
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!(
            "{}\n{}",
            if stderr.is_empty() { "tesseract OCR failed".to_string() } else { stderr },
            language_install_command(&config.ocr_languages, LinuxDistroFamily::Unknown)
        ));
    }

    let text = String::from_utf8_lossy(&output.stdout).to_string();
    if text.trim().is_empty() {
        return Err("tesseract returned no text".to_string());
    }
    Ok(text)
}

async fn run_ocr_space_ocr(config: &AppConfig, crop_path: &Path) -> Result<String, String> {
    let api_key = config.ocr_space_api_key.trim();
    if api_key.is_empty() {
        return Err("OCR.space API key is not configured".to_string());
    }
    let file_size = fs::metadata(crop_path).map_err(|error| error.to_string())?.len();
    if file_size > 1_000_000 {
        return Err("OCR.space free API only accepts images up to 1 MB; select a smaller region or use local Tesseract".to_string());
    }

    let language = ocr_space_language_arg(&config.ocr_languages);
    let args = ocr_space_curl_args(crop_path, api_key, &language, config.ocr_space_engine);
    let output = Command::new("curl")
        .args(&args)
        .output()
        .await
        .map_err(|error| format!("failed to run curl for OCR.space: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() { "OCR.space request failed".into() } else { stderr });
    }
    parse_ocr_space_response(&String::from_utf8_lossy(&output.stdout))
}

fn show_ocr_result_dialog(parent: &ApplicationWindow, result: Result<String, String>) {
    let is_success = result.is_ok();
    let window = gtk::Window::builder()
        .title("OCR Result")
        .default_width(560)
        .default_height(420)
        .transient_for(parent)
        .build();
    window.add_css_class("ashot-ocr-dialog");

    let root = GtkBox::new(Orientation::Vertical, 10);
    root.set_margin_top(16);
    root.set_margin_bottom(14);
    root.set_margin_start(16);
    root.set_margin_end(16);

    let header = GtkBox::new(Orientation::Horizontal, 10);
    header.add_css_class("ashot-ocr-header");
    let status_icon = Label::new(Some(if is_success { "OK" } else { "!" }));
    status_icon.add_css_class("ashot-ocr-status-icon");
    if !is_success {
        status_icon.add_css_class("error");
    }
    header.append(&status_icon);

    let title_box = GtkBox::new(Orientation::Vertical, 2);
    title_box.set_hexpand(true);
    let title = Label::new(Some(ocr_result_title(&result)));
    title.add_css_class("heading");
    title.set_xalign(0.0);
    let subtitle = Label::new(Some(ocr_result_subtitle(&result)));
    subtitle.add_css_class("dim-label");
    subtitle.set_xalign(0.0);
    subtitle.set_wrap(true);
    title_box.append(&title);
    title_box.append(&subtitle);
    header.append(&title_box);
    root.append(&header);

    let text = ocr_result_body_text(&result);
    let install_command = extract_ocr_install_command(&text);
    let text_view = gtk::TextView::new();
    text_view.set_wrap_mode(gtk::WrapMode::WordChar);
    text_view.set_monospace(false);
    text_view.set_editable(is_success);
    text_view.set_cursor_visible(is_success);
    text_view.set_left_margin(10);
    text_view.set_right_margin(10);
    text_view.set_top_margin(10);
    text_view.set_bottom_margin(10);
    text_view.add_css_class("ashot-ocr-text-view");
    text_view.buffer().set_text(&text);
    let scrolled = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Automatic)
        .vscrollbar_policy(PolicyType::Automatic)
        .hexpand(true)
        .vexpand(true)
        .child(&text_view)
        .build();
    scrolled.add_css_class("ashot-ocr-card");
    if !is_success {
        scrolled.add_css_class("error");
    }
    root.append(&scrolled);

    if let Some(command) = install_command.clone() {
        let command_row = GtkBox::new(Orientation::Horizontal, 8);
        command_row.add_css_class("ashot-ocr-command");
        let command_label = Label::new(Some(&command));
        command_label.set_selectable(true);
        command_label.set_xalign(0.0);
        command_label.set_wrap(true);
        command_label.set_hexpand(true);
        let copy_command = Button::with_label("Copy Command");
        command_row.append(&command_label);
        command_row.append(&copy_command);
        let command_for_copy = command.clone();
        copy_command.connect_clicked(move |_| {
            copy_text_to_clipboard(&command_for_copy);
        });
        root.append(&command_row);
    }

    let actions = GtkBox::new(Orientation::Horizontal, 8);
    actions.add_css_class("ashot-ocr-actions");
    actions.set_halign(Align::End);
    let feedback = Label::new(None);
    feedback.add_css_class("dim-label");
    feedback.set_xalign(0.0);
    feedback.set_hexpand(true);
    let copy_selected = Button::with_label("Copy Selected");
    let copy_all = Button::with_label("Copy All");
    let primary = Button::with_label(ocr_result_primary_action(&result));
    primary.add_css_class("suggested-action");
    let close = Button::with_label("Close");
    actions.append(&feedback);
    actions.append(&copy_selected);
    actions.append(&copy_all);
    actions.append(&close);
    actions.append(&primary);
    root.append(&actions);

    let text_view_for_selected = text_view.clone();
    let feedback_for_selected = feedback.clone();
    copy_selected.connect_clicked(move |_| {
        let buffer = text_view_for_selected.buffer();
        if let Some((start, end)) = buffer.selection_bounds() {
            let selected = buffer.text(&start, &end, true);
            copy_text_to_clipboard(selected.as_str());
            feedback_for_selected.set_text("Copied selected text");
        } else {
            feedback_for_selected.set_text("Select text first");
        }
    });

    let text_for_all = text.clone();
    let feedback_for_all = feedback.clone();
    copy_all.connect_clicked(move |_| {
        copy_text_to_clipboard(&text_for_all);
        feedback_for_all.set_text("Copied all text");
    });

    let window_for_close = window.clone();
    close.connect_clicked(move |_| {
        window_for_close.close();
    });

    let text_for_primary = text.clone();
    let window_for_primary = window.clone();
    let feedback_for_primary = feedback.clone();
    primary.connect_clicked(move |_| {
        copy_text_to_clipboard(&text_for_primary);
        if is_success {
            window_for_primary.close();
        } else {
            feedback_for_primary.set_text("Copied error");
        }
    });

    window.set_child(Some(&root));
    window.present();
}

fn ocr_result_title(result: &Result<String, String>) -> &'static str {
    if result.is_ok() { "Recognized Text" } else { "OCR Failed" }
}

fn ocr_result_subtitle(result: &Result<String, String>) -> &'static str {
    if result.is_ok() {
        "Edit the recognized text if needed, then copy it back to your workflow."
    } else {
        "Review the message below. Copy the install command if a language pack is missing."
    }
}

fn ocr_result_primary_action(result: &Result<String, String>) -> &'static str {
    if result.is_ok() { "Copy & Close" } else { "Copy Error" }
}

fn ocr_result_body_text(result: &Result<String, String>) -> String {
    match result {
        Ok(text) if text.trim().is_empty() => "No text recognized.".to_string(),
        Ok(text) => text.clone(),
        Err(error) if error.trim().is_empty() => "OCR failed without an error message.".to_string(),
        Err(error) => error.clone(),
    }
}

fn extract_ocr_install_command(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| {
            line.starts_with("sudo apt ")
                || line.starts_with("sudo dnf ")
                || line.starts_with("sudo pacman ")
                || line.starts_with("sudo zypper ")
        })
        .map(ToOwned::to_owned)
}

fn tesseract_command_args(image_path: &Path, languages: &[String]) -> Vec<String> {
    let language_arg = effective_tesseract_languages(languages).join("+");
    vec![
        image_path.to_string_lossy().to_string(),
        "stdout".to_string(),
        "-l".to_string(),
        language_arg,
        "--psm".to_string(),
        "6".to_string(),
    ]
}

fn tesseract_command_invocation(
    image_path: &Path,
    languages: &[String],
    in_flatpak: bool,
) -> (String, Vec<String>) {
    let args = tesseract_command_args(image_path, languages);
    if in_flatpak {
        let mut host_args = vec!["--host".to_string(), "tesseract".to_string()];
        host_args.extend(args);
        ("flatpak-spawn".to_string(), host_args)
    } else {
        ("tesseract".to_string(), args)
    }
}

fn running_in_flatpak() -> bool {
    Path::new("/.flatpak-info").exists()
}

fn effective_tesseract_languages(languages: &[String]) -> Vec<String> {
    if languages.is_empty() || languages.iter().any(|language| language == "auto") {
        default_ocr_languages()
    } else {
        languages.to_vec()
    }
}

fn normalize_ocr_languages(languages: Vec<String>) -> Vec<String> {
    if languages.is_empty() { default_ocr_languages() } else { languages }
}

fn ocr_space_language_arg(languages: &[String]) -> String {
    if languages.len() == 1 {
        if let Some(language) = ashot_core::ocr_language_by_tesseract_code(&languages[0]) {
            return language.ocr_space_code.to_string();
        }
    }
    "auto".to_string()
}

fn ocr_space_curl_args(
    image_path: &Path,
    api_key: &str,
    language: &str,
    engine: u8,
) -> Vec<String> {
    vec![
        "-sS".to_string(),
        "-X".to_string(),
        "POST".to_string(),
        "-H".to_string(),
        format!("apikey:{api_key}"),
        "-F".to_string(),
        format!("file=@{}", image_path.to_string_lossy()),
        "-F".to_string(),
        format!("language={language}"),
        "-F".to_string(),
        format!("OCREngine={engine}"),
        "https://api.ocr.space/parse/image".to_string(),
    ]
}

fn parse_ocr_space_response(body: &str) -> Result<String, String> {
    let value =
        serde_json::from_str::<serde_json::Value>(body).map_err(|error| error.to_string())?;
    if value.get("IsErroredOnProcessing").and_then(|value| value.as_bool()).unwrap_or(false) {
        return Err(ocr_space_error_message(&value));
    }

    let exit_code = value.get("OCRExitCode").and_then(|value| value.as_i64()).unwrap_or(0);
    if exit_code != 1 && exit_code != 2 {
        return Err(ocr_space_error_message(&value));
    }

    let text = value
        .get("ParsedResults")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|result| result.get("ParsedText").and_then(|text| text.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        return Err("OCR.space returned no text".to_string());
    }
    Ok(text)
}

fn ocr_space_error_message(value: &serde_json::Value) -> String {
    if let Some(errors) = value.get("ErrorMessage").and_then(|value| value.as_array()) {
        let message = errors.iter().filter_map(|item| item.as_str()).collect::<Vec<_>>().join("; ");
        if !message.is_empty() {
            return message;
        }
    }
    value
        .get("ErrorMessage")
        .and_then(|value| value.as_str())
        .unwrap_or("OCR.space request failed")
        .to_string()
}

fn filter_ocr_symbols(text: &str) -> String {
    text.chars().filter(|character| !is_ocr_symbol_noise(*character)).collect()
}

fn is_ocr_symbol_noise(character: char) -> bool {
    let code = character as u32;
    matches!(
        code,
        0x1F000..=0x1FAFF
            | 0x2600..=0x27BF
            | 0xFE00..=0xFE0F
            | 0xE0020..=0xE007F
    ) || character == '\u{200d}'
}

#[derive(Clone)]
struct SaveFilenamePopoverHandle {
    popover: Popover,
    entry: Entry,
    error: Label,
    confirm: Button,
    cancel: Button,
    confirm_label: String,
}

impl SaveFilenamePopoverHandle {
    fn set_busy(&self, busy: bool) {
        self.entry.set_sensitive(!busy);
        self.cancel.set_sensitive(!busy);
        self.confirm.set_sensitive(!busy);
        if busy {
            self.confirm.set_label("Saving...");
        } else {
            self.confirm.set_label(&self.confirm_label);
        }
    }

    fn finish_success(&self) {
        self.popover.popdown();
        self.popover.unparent();
    }

    fn finish_error(&self, message: &str) {
        self.set_busy(false);
        self.error.set_text(message);
        self.entry.grab_focus();
    }
}

fn show_save_filename_popover<F>(
    anchor: &Button,
    initial_filename: String,
    confirm_label: &str,
    on_confirm: F,
) where
    F: Fn(String, SaveFilenamePopoverHandle) + 'static,
{
    let popover = Popover::new();
    popover.set_parent(anchor);

    let root = GtkBox::new(Orientation::Vertical, 8);
    root.set_margin_top(10);
    root.set_margin_bottom(10);
    root.set_margin_start(10);
    root.set_margin_end(10);

    let title = Label::new(Some("File name"));
    title.add_css_class("heading");
    title.set_xalign(0.0);
    root.append(&title);

    let entry = Entry::new();
    entry.set_width_chars(32);
    entry.set_text(&initial_filename);
    entry.set_activates_default(true);
    root.append(&entry);

    let error = Label::new(None);
    error.add_css_class("error");
    error.set_xalign(0.0);
    root.append(&error);

    let actions = GtkBox::new(Orientation::Horizontal, 6);
    actions.set_halign(Align::End);
    let cancel = Button::with_label("Cancel");
    let confirm = Button::with_label(confirm_label);
    confirm.add_css_class("suggested-action");
    actions.append(&cancel);
    actions.append(&confirm);
    root.append(&actions);

    popover.set_child(Some(&root));

    let handle = SaveFilenamePopoverHandle {
        popover: popover.clone(),
        entry: entry.clone(),
        error: error.clone(),
        confirm: confirm.clone(),
        cancel: cancel.clone(),
        confirm_label: confirm_label.to_string(),
    };
    let on_confirm: Rc<dyn Fn(String, SaveFilenamePopoverHandle)> = Rc::new(on_confirm);
    let confirm_action = {
        let entry = entry.clone();
        let error = error.clone();
        let handle = handle.clone();
        let on_confirm = on_confirm.clone();
        move || {
            let filename = entry.text().to_string();
            if normalized_save_filename(&filename).is_none() {
                error.set_text("Enter a valid PNG file name");
                entry.grab_focus();
                return;
            }
            handle.set_busy(true);
            on_confirm(filename, handle.clone());
        }
    };

    let confirm_action = Rc::new(confirm_action);
    let confirm_action_for_button = confirm_action.clone();
    confirm.connect_clicked(move |_| {
        confirm_action_for_button();
    });
    let confirm_action_for_entry = confirm_action.clone();
    entry.connect_activate(move |_| {
        confirm_action_for_entry();
    });

    let popover_for_cancel = popover.clone();
    cancel.connect_clicked(move |_| {
        popover_for_cancel.popdown();
        popover_for_cancel.unparent();
    });

    popover.popup();
    entry.grab_focus();
    entry.set_position(-1);
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use ashot_core::{
        Annotation, AnnotationData, AppConfig, AppearanceMode, Color, DefaultTool, Document, Point,
        Rect, ResizeHandle,
    };

    use super::{
        ActiveTextEdit, COLOR_MEMORY_BUTTON_SIZE, COLOR_MEMORY_SWATCH_SIZE, COLOR_ROW_LIMIT,
        COLOR_VALUE_BUTTON_HEIGHT, COLOR_VALUE_BUTTON_WIDTH, DraftAnnotation, EditorCursorKind,
        HslColor, PinClickAction, SELECTION_HANDLE_HIT_TOLERANCE, SIDEBAR_ACTION_BUTTON_HEIGHT,
        SIDEBAR_ACTION_BUTTON_WIDTH, SIDEBAR_TOOL_BUTTON_HEIGHT, SIDEBAR_TOOL_BUTTON_WIDTH,
        STROKE_MENU_BUTTON_HEIGHT, STROKE_MENU_BUTTON_WIDTH, STROKE_PREVIEW_HEIGHT,
        STROKE_PREVIEW_WIDTH, TOOL_ICON_CANVAS_SIZE, add_favorite_color,
        annotation_keeps_selection_after_creation, annotation_snapshot_for_draw,
        appearance_color_scheme, appearance_mode_from_index, appearance_mode_index,
        appearance_mode_labels, arrow_head_geometry, arrow_head_points, arrow_shape_geometry,
        arrow_visual_stroke_width, blur_radius_for_stroke, capture_should_use_fresh_anchor,
        clamp_magnifier_zoom, crop_image_region, cursor_name_for_resize_handle,
        cursor_name_for_surface_edge, draft_preview_for_draw, draft_tool_can_draw,
        editor_color_palette, editor_cursor_for_tool, editor_cursor_kind_for_tool,
        editor_favorite_palette, editor_initial_size, editor_status_text, editor_stroke_widths,
        editor_tool_layout, extract_ocr_install_command, eyedropper_magnifier_point,
        filter_ocr_symbols, fit_scale, format_magnifier_zoom, hsl_to_color, hsv_to_color,
        image_color_at, magnifier_geometry, magnifier_size_for_zoom, mosaic_pixel_size_for_stroke,
        moving_delta_and_update, normalized_save_filename, ocr_language_label,
        ocr_result_body_text, ocr_result_primary_action, ocr_result_title, ocr_space_curl_args,
        ocr_space_language_arg, output_action_menu_items, output_action_primary_label,
        parse_hex_color, parse_ocr_space_response, pin_click_action, pin_context_popover_rect,
        pin_dimension_label, pin_display_size, pin_initial_scale, pin_initial_scale_with_saved,
        pin_window_size, pin_window_size_for_scale, pin_zoom_from_scroll, push_recent_color,
        remove_favorite_color, render_document_png_bytes, resize_handle_at, rgb_to_hsl, rgb_to_hsv,
        rgba_color_at, save_editor_document_to_dir_with_filename, scaled_canvas_point,
        selected_annotation_bounds, set_active_text_edit, suggested_save_filename_at,
        take_active_text_edit, tesseract_command_args, tesseract_command_invocation,
        text_for_annotation, tool_can_select_existing, tool_icon_label, tool_icon_stroke_width,
        tool_picks_canvas_color, update_ocr_language_selection,
    };

    use chrono::{Local, TimeZone};
    use libadwaita::ColorScheme;

    #[test]
    fn capture_never_reuses_existing_editor_window_as_portal_parent() {
        assert!(capture_should_use_fresh_anchor(true));
        assert!(capture_should_use_fresh_anchor(false));
    }

    #[test]
    fn editor_initial_size_reserves_space_for_image_and_chrome() {
        let (width, height) = editor_initial_size(3200, 1800);
        assert_eq!(width, 1440);
        assert_eq!(height, 980);

        let (small_width, small_height) = editor_initial_size(640, 360);
        assert!(small_width >= 980);
        assert!(small_height >= 620);
    }

    #[test]
    fn editor_status_text_reports_tool_style_image_and_save_path() {
        let text = editor_status_text(
            DefaultTool::Arrow,
            Color::rgba(232, 62, 38, 255),
            4,
            1920,
            1080,
            std::path::Path::new("/tmp/screens"),
            Some("Saved"),
        );

        assert!(text.contains("Tool: Arrow"));
        assert!(text.contains("Color: #E83E26"));
        assert!(text.contains("Width: 4px"));
        assert!(text.contains("Image: 1920 x 1080"));
        assert!(text.contains("Path: /tmp/screens"));
        assert!(text.contains("Saved"));
    }

    #[test]
    fn appearance_mode_maps_to_libadwaita_color_scheme() {
        assert_eq!(appearance_color_scheme(AppearanceMode::System), ColorScheme::Default);
        assert_eq!(appearance_color_scheme(AppearanceMode::Light), ColorScheme::ForceLight);
        assert_eq!(appearance_color_scheme(AppearanceMode::Dark), ColorScheme::ForceDark);
    }

    #[test]
    fn appearance_mode_settings_options_round_trip() {
        assert_eq!(appearance_mode_labels(), ["Follow System", "Light", "Dark"]);
        assert_eq!(appearance_mode_index(AppearanceMode::System), 0);
        assert_eq!(appearance_mode_index(AppearanceMode::Light), 1);
        assert_eq!(appearance_mode_index(AppearanceMode::Dark), 2);
        assert_eq!(appearance_mode_from_index(0), AppearanceMode::System);
        assert_eq!(appearance_mode_from_index(1), AppearanceMode::Light);
        assert_eq!(appearance_mode_from_index(2), AppearanceMode::Dark);
        assert_eq!(appearance_mode_from_index(99), AppearanceMode::System);
    }

    #[test]
    fn fit_scale_keeps_full_image_visible_when_it_is_larger_than_viewport() {
        let scale = fit_scale(2560, 1440, 1120, 760);
        assert!(scale < 1.0);
        assert!((2560.0 * scale) <= 1120.0);
        assert!((1440.0 * scale) <= 760.0);
    }

    #[test]
    fn fit_scale_does_not_upscale_small_captures() {
        assert_eq!(fit_scale(640, 360, 1120, 760), 1.0);
    }

    #[test]
    fn scaled_canvas_points_map_back_to_image_coordinates() {
        let point = scaled_canvas_point(320.0, 180.0, 0.5);
        assert_eq!(point, Point::new(640.0, 360.0));
    }

    #[test]
    fn draft_annotation_produces_live_preview_before_release() {
        let mut draft = DraftAnnotation::new(
            DefaultTool::Rectangle,
            Point::new(10.0, 20.0),
            Color::rgba(232, 62, 38, 255),
            4,
        );
        draft.extend(Point::new(60.0, 80.0));

        let preview = draft.preview_annotation().expect("preview annotation");

        assert!(matches!(preview.data, AnnotationData::Rectangle { .. }));
    }

    #[test]
    fn color_picker_converts_between_hex_hsv_and_hsl() {
        let red = Color::rgba(244, 67, 54, 255);

        assert_eq!(parse_hex_color("#F44336", 255), Some(red));
        assert_eq!(hsv_to_color(rgb_to_hsv(red), 255), red);
        assert_eq!(
            hsl_to_color(
                HslColor {
                    hue: rgb_to_hsl(red).hue,
                    saturation: rgb_to_hsl(red).saturation,
                    lightness: rgb_to_hsl(red).lightness,
                },
                255,
            ),
            red
        );
    }

    #[test]
    fn color_picker_supports_alpha_hex_and_recent_deduplication() {
        let translucent = Color::rgba(244, 67, 54, 128);
        assert_eq!(parse_hex_color("#F4433680", 255), Some(translucent));

        let mut recent = vec![Color::rgba(1, 2, 3, 255), translucent];
        push_recent_color(&mut recent, translucent, 2);

        assert_eq!(recent, vec![translucent, Color::rgba(1, 2, 3, 255)]);
    }

    #[test]
    fn clipboard_render_bytes_include_annotations() {
        let base = image::DynamicImage::new_rgba8(24, 24);
        let annotations = vec![Annotation::new(AnnotationData::FilledBox {
            rect: Rect::from_points(Point::new(4.0, 4.0), Point::new(12.0, 12.0)),
            color: Color::rgba(255, 0, 0, 255),
        })];

        let bytes = render_document_png_bytes(&base, &annotations).expect("render png bytes");
        let rendered = image::load_from_memory(&bytes).expect("decode copied png").to_rgba8();

        assert_eq!(rendered.get_pixel(6, 6).0, [255, 0, 0, 255]);
    }

    #[test]
    fn color_picker_has_focused_favorite_palette() {
        let palette = editor_favorite_palette();

        assert_eq!(palette.len(), 4);
        assert!(palette.iter().any(|(name, _)| *name == "Red"));
        assert!(palette.iter().any(|(name, _)| *name == "Black"));
    }

    #[test]
    fn color_picker_limits_and_removes_favorites() {
        let mut favorites = vec![Color::rgba(1, 1, 1, 255), Color::rgba(2, 2, 2, 255)];

        assert_eq!(add_favorite_color(&mut favorites, Color::rgba(3, 3, 3, 255), 3), Ok(()));
        assert_eq!(add_favorite_color(&mut favorites, Color::rgba(4, 4, 4, 255), 3), Err(3));

        remove_favorite_color(&mut favorites, Color::rgba(2, 2, 2, 255));
        assert!(!favorites.contains(&Color::rgba(2, 2, 2, 255)));
    }

    #[test]
    fn tool_buttons_use_icons_with_tooltips_for_names() {
        for (name, tool) in editor_tool_layout() {
            assert_ne!(tool_icon_label(tool), name);
        }
        assert_eq!(tool_icon_label(DefaultTool::Text), "T");
    }

    #[test]
    fn sidebar_visual_metrics_stay_compact() {
        assert_eq!((SIDEBAR_TOOL_BUTTON_WIDTH, SIDEBAR_TOOL_BUTTON_HEIGHT), (42, 34));
        assert_eq!((SIDEBAR_ACTION_BUTTON_WIDTH, SIDEBAR_ACTION_BUTTON_HEIGHT), (40, 34));
        assert_eq!(TOOL_ICON_CANVAS_SIZE, 22);
        assert!(tool_icon_stroke_width() < 1.7);
    }

    #[test]
    fn color_and_stroke_controls_keep_single_line_density() {
        assert_eq!(COLOR_ROW_LIMIT, 6);
        assert_eq!(COLOR_MEMORY_BUTTON_SIZE, 24);
        assert_eq!(COLOR_MEMORY_SWATCH_SIZE, 18);
        assert_eq!((COLOR_VALUE_BUTTON_WIDTH, COLOR_VALUE_BUTTON_HEIGHT), (82, 30));
        assert_eq!((STROKE_PREVIEW_WIDTH, STROKE_PREVIEW_HEIGHT), (104, 20));
        assert_eq!((STROKE_MENU_BUTTON_WIDTH, STROKE_MENU_BUTTON_HEIGHT), (58, 32));
    }

    #[test]
    fn magnifier_zoom_clamps_and_formats_as_integer_multiplier() {
        assert_eq!(clamp_magnifier_zoom(1.0), 4.0);
        assert_eq!(clamp_magnifier_zoom(8.4), 8.0);
        assert_eq!(clamp_magnifier_zoom(18.0), 16.0);
        assert_eq!(clamp_magnifier_zoom(f64::NAN), 8.0);
        assert_eq!(format_magnifier_zoom(12.0), "12x");
    }

    #[test]
    fn magnifier_only_draws_for_enabled_color_picker_inside_image() {
        let point = Some(Point::new(12.0, 8.0));

        assert_eq!(
            eyedropper_magnifier_point(DefaultTool::ColorPicker, true, point, 24, 16),
            point
        );
        assert_eq!(
            eyedropper_magnifier_point(DefaultTool::ColorPicker, false, point, 24, 16),
            None
        );
        assert_eq!(eyedropper_magnifier_point(DefaultTool::Arrow, true, point, 24, 16), None);
        assert_eq!(
            eyedropper_magnifier_point(
                DefaultTool::ColorPicker,
                true,
                Some(Point::new(30.0, 8.0)),
                24,
                16
            ),
            None
        );
    }

    #[test]
    fn magnifier_geometry_stays_inside_canvas_and_scales_with_zoom() {
        let small = magnifier_size_for_zoom(4.0);
        let large = magnifier_size_for_zoom(16.0);
        assert!(large > small);

        let geometry = magnifier_geometry(Point::new(395.0, 6.0), 400, 260, 16.0);
        assert!(geometry.x >= 6.0);
        assert!(geometry.y >= 6.0);
        assert!(geometry.x + geometry.size <= 400.0);
        assert!(geometry.y + geometry.size <= 260.0);
    }

    #[test]
    fn output_actions_keep_done_primary_and_group_secondary_items() {
        assert_eq!(output_action_primary_label(), "Done");
        assert_eq!(output_action_menu_items(), ["Save", "Save To", "Copy & Close"]);
    }

    #[test]
    fn arrow_preview_has_head_points_distinct_from_the_line_tip() {
        let end = Point::new(90.0, 10.0);
        let (left, right) = arrow_head_points(Point::new(10.0, 10.0), end, 4);

        assert_ne!(left, end);
        assert_ne!(right, end);
        assert!(left.x < end.x);
        assert!(right.x < end.x);
        assert_ne!(left.y, right.y);
    }

    #[test]
    fn arrow_preview_uses_filled_head_geometry() {
        let start = Point::new(10.0, 10.0);
        let end = Point::new(90.0, 10.0);
        let (base, left, right) = arrow_head_geometry(start, end, 6);

        assert!(base.x < end.x);
        assert!(base.x > start.x);
        assert!(left.x < end.x);
        assert!(right.x < end.x);
        assert!((left.y - right.y).abs() >= 31.0);
        assert!(left.y < end.y || right.y < end.y);
        assert!(left.y > end.y || right.y > end.y);
    }

    #[test]
    fn arrow_preview_uses_tapered_body_instead_of_straight_line() {
        let start = Point::new(10.0, 10.0);
        let end = Point::new(90.0, 10.0);
        let shape = arrow_shape_geometry(start, end, 6);
        let tail_width = (shape.tail_left.y - shape.tail_right.y).abs();
        let body_width = (shape.body_left.y - shape.body_right.y).abs();
        let head_width = (shape.head_left.y - shape.head_right.y).abs();

        assert!(tail_width < body_width);
        assert!(body_width < head_width);
        assert!(shape.body_left.x < shape.head_left.x);
        assert!(shape.body_right.x < shape.head_right.x);
    }

    #[test]
    fn arrow_preview_handles_very_short_drag_distance() {
        let shape = arrow_shape_geometry(Point::new(10.0, 10.0), Point::new(12.0, 10.0), 6);

        assert_eq!(shape.tip, Point::new(12.0, 10.0));
    }

    #[test]
    fn arrow_preview_uses_bolder_visual_width_than_plain_line() {
        assert_eq!(arrow_visual_stroke_width(2), 6);
        assert_eq!(arrow_visual_stroke_width(4), 7);
        assert!(arrow_visual_stroke_width(12) > 12);
    }

    #[test]
    fn mosaic_uses_stroke_dropdown_as_effect_strength() {
        assert_eq!(mosaic_pixel_size_for_stroke(2), 6);
        assert_eq!(mosaic_pixel_size_for_stroke(4), 10);
        assert_eq!(mosaic_pixel_size_for_stroke(8), 20);
        assert_eq!(mosaic_pixel_size_for_stroke(12), 28);

        let mut draft = DraftAnnotation::new(
            DefaultTool::Mosaic,
            Point::new(10.0, 20.0),
            Color::rgba(232, 62, 38, 255),
            12,
        );
        draft.extend(Point::new(60.0, 80.0));

        let preview = draft.preview_annotation().expect("mosaic preview");

        assert!(matches!(preview.data, AnnotationData::Mosaic { pixel_size: 28, .. }));
    }

    #[test]
    fn blur_uses_stroke_dropdown_as_effect_strength() {
        assert_eq!(blur_radius_for_stroke(2), 2);
        assert_eq!(blur_radius_for_stroke(8), 8);
        assert_eq!(blur_radius_for_stroke(12), 12);

        let mut draft = DraftAnnotation::new(
            DefaultTool::Blur,
            Point::new(10.0, 20.0),
            Color::rgba(232, 62, 38, 255),
            8,
        );
        draft.extend(Point::new(60.0, 80.0));

        let preview = draft.preview_annotation().expect("blur preview");

        assert!(matches!(preview.data, AnnotationData::Blur { radius: 8, .. }));
    }

    #[test]
    fn annotation_snapshot_for_draw_skips_when_document_is_mutably_borrowed() {
        let document = Rc::new(RefCell::new(Document::new(100, 80, DefaultTool::Text)));
        let _borrow = document.borrow_mut();

        assert!(annotation_snapshot_for_draw(&document).is_none());
    }

    #[test]
    fn text_for_annotation_returns_existing_text() {
        let mut document = Document::new(100, 80, DefaultTool::Text);
        let annotation = ashot_core::Annotation::new(AnnotationData::Text {
            origin: Point::new(12.0, 18.0),
            text: "hello".into(),
            style: ashot_core::TextStyle {
                size: 20,
                weight: ashot_core::TextWeight::Bold,
                color: Color::rgba(255, 255, 255, 255),
            },
        });
        let id = annotation.id;
        document.add_annotation(annotation);

        assert_eq!(text_for_annotation(&document, id), Some("hello".into()));
    }

    #[test]
    fn moving_delta_and_update_releases_read_borrow_before_writing() {
        let moving = Rc::new(RefCell::new(Some(Point::new(10.0, 20.0))));

        let delta = moving_delta_and_update(&moving, Point::new(18.0, 35.0));

        assert_eq!(delta, Some((8.0, 15.0)));
        assert_eq!(*moving.borrow(), Some(Point::new(18.0, 35.0)));
    }

    #[test]
    fn resize_handle_hit_testing_uses_selected_bounds() {
        let mut document = Document::new(120, 80, DefaultTool::Select);
        let annotation = Annotation::new(AnnotationData::Rectangle {
            rect: Rect { x: 10.0, y: 20.0, width: 40.0, height: 30.0 },
            color: Color::rgba(255, 0, 0, 255),
            stroke_width: 4,
        });
        let id = annotation.id;
        document.add_annotation(annotation);
        document.selected = Some(id);

        assert_eq!(
            resize_handle_at(&document, Point::new(50.0, 50.0), 6.0),
            Some(ResizeHandle::BottomRight)
        );
        assert_eq!(resize_handle_at(&document, Point::new(30.0, 35.0), 6.0), None);
    }

    #[test]
    fn selection_overlay_is_only_shown_for_resizable_annotations() {
        let mut document = Document::new(120, 80, DefaultTool::Select);
        let brush = Annotation::new(AnnotationData::Brush {
            points: vec![Point::new(10.0, 20.0), Point::new(50.0, 50.0)],
            color: Color::rgba(255, 0, 0, 255),
            stroke_width: 4,
        });
        let brush_id = brush.id;
        document.add_annotation(brush);
        document.selected = Some(brush_id);
        assert_eq!(selected_annotation_bounds(&document), None);

        let marker = Annotation::new(AnnotationData::Marker {
            points: vec![Point::new(15.0, 25.0), Point::new(55.0, 65.0)],
            color: Color::rgba(255, 220, 0, 96),
            stroke_width: 12,
        });
        let marker_id = marker.id;
        document.add_annotation(marker);
        document.selected = Some(marker_id);
        assert_eq!(selected_annotation_bounds(&document), None);

        let text = Annotation::new(AnnotationData::Text {
            origin: Point::new(12.0, 18.0),
            text: "note".into(),
            style: ashot_core::TextStyle {
                size: 20,
                weight: ashot_core::TextWeight::Bold,
                color: Color::rgba(255, 255, 255, 255),
            },
        });
        let text_id = text.id;
        document.add_annotation(text);
        document.selected = Some(text_id);
        assert_eq!(selected_annotation_bounds(&document), None);

        let rect = Annotation::new(AnnotationData::Rectangle {
            rect: Rect { x: 10.0, y: 20.0, width: 40.0, height: 30.0 },
            color: Color::rgba(255, 0, 0, 255),
            stroke_width: 4,
        });
        let rect_id = rect.id;
        document.add_annotation(rect);
        document.selected = Some(rect_id);
        assert!(selected_annotation_bounds(&document).is_some());
    }

    #[test]
    fn non_resizable_annotations_do_not_keep_selection_after_creation() {
        let brush = Annotation::new(AnnotationData::Brush {
            points: vec![Point::new(10.0, 20.0), Point::new(50.0, 50.0)],
            color: Color::rgba(255, 0, 0, 255),
            stroke_width: 4,
        });
        let marker = Annotation::new(AnnotationData::Marker {
            points: vec![Point::new(15.0, 25.0), Point::new(55.0, 65.0)],
            color: Color::rgba(255, 220, 0, 96),
            stroke_width: 12,
        });
        let rect = Annotation::new(AnnotationData::Rectangle {
            rect: Rect { x: 10.0, y: 20.0, width: 40.0, height: 30.0 },
            color: Color::rgba(255, 0, 0, 255),
            stroke_width: 4,
        });

        assert!(!annotation_keeps_selection_after_creation(&brush));
        assert!(!annotation_keeps_selection_after_creation(&marker));
        assert!(annotation_keeps_selection_after_creation(&rect));
    }

    #[test]
    fn resize_handles_use_a_larger_default_hit_target() {
        let mut document = Document::new(120, 80, DefaultTool::Select);
        let annotation = Annotation::new(AnnotationData::Rectangle {
            rect: Rect { x: 10.0, y: 20.0, width: 40.0, height: 30.0 },
            color: Color::rgba(255, 0, 0, 255),
            stroke_width: 4,
        });
        let id = annotation.id;
        document.add_annotation(annotation);
        document.selected = Some(id);

        assert_eq!(
            resize_handle_at(&document, Point::new(58.5, 58.5), SELECTION_HANDLE_HIT_TOLERANCE),
            Some(ResizeHandle::BottomRight)
        );
    }

    #[test]
    fn active_text_edit_helpers_do_not_panic_when_state_is_borrowed() {
        let active_text_edit = Rc::new(RefCell::new(Some(ActiveTextEdit {
            id: None,
            origin: Point::new(4.0, 8.0),
            color: Color::rgba(255, 255, 255, 255),
        })));
        let _borrow = active_text_edit.borrow_mut();

        assert!(take_active_text_edit(&active_text_edit).is_none());
        assert!(!set_active_text_edit(
            &active_text_edit,
            ActiveTextEdit {
                id: None,
                origin: Point::new(12.0, 16.0),
                color: Color::rgba(232, 62, 38, 255),
            },
        ));
    }

    #[test]
    fn draft_preview_for_draw_skips_when_draft_is_mutably_borrowed() {
        let draft = Rc::new(RefCell::new(Some(DraftAnnotation::new(
            DefaultTool::Rectangle,
            Point::new(10.0, 20.0),
            Color::rgba(232, 62, 38, 255),
            4,
        ))));
        let _borrow = draft.borrow_mut();

        assert!(draft_preview_for_draw(&draft).is_none());
    }

    #[test]
    fn only_drawing_tools_create_drag_snapshots() {
        assert!(draft_tool_can_draw(DefaultTool::Arrow));
        assert!(draft_tool_can_draw(DefaultTool::Rectangle));
        assert!(draft_tool_can_draw(DefaultTool::Brush));
        assert!(draft_tool_can_draw(DefaultTool::Mosaic));
        assert!(draft_tool_can_draw(DefaultTool::Blur));
        assert!(draft_tool_can_draw(DefaultTool::Ocr));

        assert!(!draft_tool_can_draw(DefaultTool::Select));
        assert!(!draft_tool_can_draw(DefaultTool::Text));
        assert!(!draft_tool_can_draw(DefaultTool::Counter));
    }

    #[test]
    fn only_select_tool_uses_existing_annotation_selection() {
        assert!(tool_can_select_existing(DefaultTool::Select));
        assert!(!tool_can_select_existing(DefaultTool::Arrow));
        assert!(!tool_can_select_existing(DefaultTool::Ocr));
        assert!(!tool_can_select_existing(DefaultTool::ColorPicker));
    }

    #[test]
    fn only_color_picker_samples_canvas_color() {
        assert!(tool_picks_canvas_color(DefaultTool::ColorPicker));
        assert!(!tool_picks_canvas_color(DefaultTool::Select));
        assert!(!tool_picks_canvas_color(DefaultTool::Brush));
    }

    #[test]
    fn cursor_changes_to_match_active_tool_family() {
        assert_eq!(editor_cursor_kind_for_tool(DefaultTool::Select), EditorCursorKind::Default);
        assert_eq!(editor_cursor_kind_for_tool(DefaultTool::Text), EditorCursorKind::Text);
        assert_eq!(
            editor_cursor_kind_for_tool(DefaultTool::ColorPicker),
            EditorCursorKind::Eyedropper
        );
        assert_eq!(editor_cursor_kind_for_tool(DefaultTool::Arrow), EditorCursorKind::Crosshair);

        assert_eq!(editor_cursor_for_tool(DefaultTool::Select), Some("default"));
        assert_eq!(editor_cursor_for_tool(DefaultTool::Text), Some("text"));
        assert_eq!(editor_cursor_for_tool(DefaultTool::ColorPicker), None);
        assert_eq!(editor_cursor_for_tool(DefaultTool::Arrow), Some("crosshair"));
        assert_eq!(editor_cursor_for_tool(DefaultTool::Rectangle), Some("crosshair"));
        assert_eq!(editor_cursor_for_tool(DefaultTool::Ocr), Some("crosshair"));
    }

    #[test]
    fn resize_handles_map_to_directional_cursors() {
        assert_eq!(cursor_name_for_resize_handle(ResizeHandle::TopLeft), "nwse-resize");
        assert_eq!(cursor_name_for_resize_handle(ResizeHandle::BottomRight), "nwse-resize");
        assert_eq!(cursor_name_for_resize_handle(ResizeHandle::TopRight), "nesw-resize");
        assert_eq!(cursor_name_for_resize_handle(ResizeHandle::Left), "ew-resize");
        assert_eq!(cursor_name_for_resize_handle(ResizeHandle::Bottom), "ns-resize");
    }

    #[test]
    fn color_picker_samples_and_clamps_base_image_pixels() {
        let mut image = image::DynamicImage::new_rgba8(2, 2).to_rgba8();
        image.put_pixel(1, 1, image::Rgba([3, 5, 8, 255]));
        let image = image::DynamicImage::ImageRgba8(image);

        assert_eq!(image_color_at(&image, Point::new(1.0, 1.0)), Color::rgba(3, 5, 8, 255));
        assert_eq!(image_color_at(&image, Point::new(100.0, 100.0)), Color::rgba(3, 5, 8, 255));
    }

    #[test]
    fn magnifier_center_sample_matches_color_picker_sample() {
        let mut rgba = image::DynamicImage::new_rgba8(3, 3).to_rgba8();
        rgba.put_pixel(2, 1, image::Rgba([11, 22, 33, 255]));
        let image = image::DynamicImage::ImageRgba8(rgba.clone());
        let point = Point::new(2.0, 1.0);

        assert_eq!(rgba_color_at(&rgba, point), image_color_at(&image, point));
    }

    #[test]
    fn ocr_draft_previews_region_but_finishes_without_annotation() {
        let mut draft = DraftAnnotation::new(
            DefaultTool::Ocr,
            Point::new(10.0, 20.0),
            Color::rgba(232, 62, 38, 255),
            4,
        );
        draft.extend(Point::new(60.0, 80.0));

        assert!(matches!(
            draft.preview_annotation().expect("ocr region preview").data,
            AnnotationData::Rectangle { .. }
        ));
        assert!(draft.finish().is_none());
    }

    #[test]
    fn ocr_crop_clamps_region_to_original_image() {
        let image = image::DynamicImage::new_rgba8(100, 80);

        let crop = crop_image_region(&image, Rect { x: 90.0, y: 70.0, width: 30.0, height: 20.0 })
            .expect("crop");

        assert_eq!(crop.width(), 10);
        assert_eq!(crop.height(), 10);
    }

    #[test]
    fn tesseract_command_uses_selected_languages() {
        let args = tesseract_command_args(
            std::path::Path::new("/tmp/ocr.png"),
            &["chi_sim".to_string(), "eng".to_string()],
        );

        assert_eq!(args, vec!["/tmp/ocr.png", "stdout", "-l", "chi_sim+eng", "--psm", "6"]);
    }

    #[test]
    fn flatpak_tesseract_invocation_runs_host_tesseract() {
        let (program, args) = tesseract_command_invocation(
            std::path::Path::new("/tmp/ocr.png"),
            &["eng".to_string()],
            true,
        );

        assert_eq!(program, "flatpak-spawn");
        assert_eq!(
            args,
            vec!["--host", "tesseract", "/tmp/ocr.png", "stdout", "-l", "eng", "--psm", "6"]
        );
    }

    #[test]
    fn ocr_space_uses_single_language_or_auto_for_multiple() {
        assert_eq!(ocr_space_language_arg(&["chi_sim".to_string()]), "chs");
        assert_eq!(ocr_space_language_arg(&["chi_sim".to_string(), "eng".to_string()]), "auto");
    }

    #[test]
    fn ocr_auto_language_uses_auto_for_online_and_default_for_tesseract() {
        assert_eq!(ocr_space_language_arg(&["auto".to_string()]), "auto");

        let args = tesseract_command_args(std::path::Path::new("/tmp/ocr.png"), &["auto".into()]);

        assert!(args.contains(&"chi_sim+eng".to_string()));
    }

    #[test]
    fn ocr_symbol_filter_removes_emoji_noise_but_keeps_text() {
        assert_eq!(filter_ocr_symbols("hello ✅ world 😀"), "hello  world ");
    }

    #[test]
    fn ocr_result_dialog_labels_success_and_failure_states() {
        let success = Ok("hello".to_string());
        let failure = Err("missing language pack".to_string());

        assert_eq!(ocr_result_title(&success), "Recognized Text");
        assert_eq!(ocr_result_title(&failure), "OCR Failed");
        assert_eq!(ocr_result_primary_action(&success), "Copy & Close");
        assert_eq!(ocr_result_primary_action(&failure), "Copy Error");
    }

    #[test]
    fn ocr_result_dialog_uses_placeholders_for_empty_messages() {
        assert_eq!(ocr_result_body_text(&Ok(String::new())), "No text recognized.");
        assert_eq!(
            ocr_result_body_text(&Err(String::new())),
            "OCR failed without an error message."
        );
    }

    #[test]
    fn ocr_result_dialog_extracts_install_commands_from_errors() {
        let error = "tesseract failed\nsudo apt install tesseract-ocr tesseract-ocr-eng";

        assert_eq!(
            extract_ocr_install_command(error),
            Some("sudo apt install tesseract-ocr tesseract-ocr-eng".to_string())
        );
        assert_eq!(extract_ocr_install_command("Install tesseract traineddata"), None);
    }

    #[test]
    fn editor_tool_layout_keeps_ocr_separate_from_tool_grid() {
        assert!(!editor_tool_layout().iter().any(|(_, tool)| *tool == DefaultTool::Ocr));
    }

    #[test]
    fn ocr_space_curl_args_include_api_key_file_language_and_engine() {
        let args = ocr_space_curl_args(std::path::Path::new("/tmp/ocr.png"), "secret", "chs", 2);

        assert!(args.contains(&"-H".to_string()));
        assert!(args.contains(&"apikey:secret".to_string()));
        assert!(args.contains(&"file=@/tmp/ocr.png".to_string()));
        assert!(args.contains(&"language=chs".to_string()));
        assert!(args.contains(&"OCREngine=2".to_string()));
    }

    #[test]
    fn ocr_space_response_extracts_text_or_error() {
        let success = r#"{"OCRExitCode":1,"ParsedResults":[{"ParsedText":"hello\n"}]}"#;
        assert_eq!(parse_ocr_space_response(success).expect("text"), "hello\n");

        let failure = r#"{"OCRExitCode":3,"ErrorMessage":["bad image"]}"#;
        assert!(parse_ocr_space_response(failure).expect_err("error").contains("bad image"));
    }

    #[test]
    fn editor_palette_has_multiple_color_families() {
        let palette = editor_color_palette();
        assert!(palette.len() >= 32);
        assert!(palette.iter().any(|(_, color)| color.r > 220 && color.g < 100));
        assert!(palette.iter().any(|(_, color)| color.g > 180 && color.r < 100));
        assert!(palette.iter().any(|(_, color)| color.b > 200 && color.r < 100));
    }

    #[test]
    fn editor_stroke_dropdown_has_practical_widths() {
        assert_eq!(editor_stroke_widths(), [2, 4, 6, 8, 12]);
    }

    #[test]
    fn ocr_language_selector_supports_auto_and_exclusive_language_choices() {
        let mut languages = vec!["chi_sim".to_string(), "eng".to_string()];

        update_ocr_language_selection(&mut languages, "auto", true);
        assert_eq!(languages, vec!["auto"]);
        assert_eq!(ocr_language_label(&languages), "Language: Auto");

        update_ocr_language_selection(&mut languages, "jpn", true);
        assert_eq!(languages, vec!["jpn"]);
    }

    #[test]
    fn pin_initial_scale_fits_full_image_inside_window() {
        let (window_width, window_height) = pin_window_size(2560, 1440);
        let scale = pin_initial_scale(2560, 1440, window_width, window_height);
        let (display_width, display_height) = pin_display_size(2560, 1440, scale);

        assert!(display_width <= window_width);
        assert!(display_height <= window_height);
        assert!(scale < 1.0);
    }

    #[test]
    fn pin_initial_scale_uses_remembered_non_default_zoom() {
        assert_eq!(pin_initial_scale_with_saved(1200, 800, 600, 400, 1.5), 1.5);
    }

    #[test]
    fn pin_wheel_zoom_can_expand_and_shrink() {
        let current = 1.0;

        assert!(pin_zoom_from_scroll(current, -1.0) > current);
        assert!(pin_zoom_from_scroll(current, 1.0) < current);
    }

    #[test]
    fn pin_window_frame_tracks_zoomed_image_size() {
        assert_eq!(pin_window_size_for_scale(1200, 800, 0.5), (600, 400));
        assert_eq!(pin_window_size_for_scale(1200, 800, 1.25), (1500, 1000));
    }

    #[test]
    fn pin_clicks_move_or_close_from_image_area() {
        assert_eq!(pin_click_action(1, 1), Some(PinClickAction::Move));
        assert_eq!(pin_click_action(2, 1), Some(PinClickAction::Close));
        assert_eq!(pin_click_action(1, 3), Some(PinClickAction::Menu));
    }

    #[test]
    fn pin_context_menu_anchors_to_click_position() {
        assert_eq!(pin_context_popover_rect(24.4, 35.6), (24, 36, 1, 1));
        assert_eq!(pin_context_popover_rect(-8.0, f64::NAN), (0, 0, 1, 1));
    }

    #[test]
    fn pin_resize_edges_map_to_directional_cursors() {
        assert_eq!(cursor_name_for_surface_edge(gtk4::gdk::SurfaceEdge::NorthWest), "nwse-resize");
        assert_eq!(cursor_name_for_surface_edge(gtk4::gdk::SurfaceEdge::NorthEast), "nesw-resize");
        assert_eq!(cursor_name_for_surface_edge(gtk4::gdk::SurfaceEdge::East), "ew-resize");
        assert_eq!(cursor_name_for_surface_edge(gtk4::gdk::SurfaceEdge::South), "ns-resize");
    }

    #[test]
    fn pin_dimension_label_reports_display_size_and_scale() {
        assert_eq!(pin_dimension_label(1920, 1080, 960, 540, 0.5), "960 x 540 px · 50%");
    }

    #[test]
    fn normalized_save_filename_always_targets_png() {
        assert_eq!(normalized_save_filename(" shot "), Some("shot.png".into()));
        assert_eq!(normalized_save_filename("shot.PNG"), Some("shot.PNG".into()));
        assert_eq!(normalized_save_filename("shot.jpg"), Some("shot.png".into()));
        assert_eq!(normalized_save_filename("/tmp/final-name"), Some("final-name.png".into()));
        assert_eq!(normalized_save_filename("   "), None);
    }

    #[test]
    fn suggested_save_filename_uses_config_template() {
        let config =
            AppConfig { filename_template: "Capture_%Y%m%d_%H%M%S".into(), ..AppConfig::default() };
        let now = Local.with_ymd_and_hms(2026, 4, 27, 12, 34, 56).unwrap();

        assert_eq!(suggested_save_filename_at(&config, now), "Capture_20260427_123456.png");
    }

    #[test]
    fn save_to_specific_directory_uses_requested_directory() {
        let requested_dir = std::env::temp_dir().join(format!(
            "ashot-save-to-test-{}",
            chrono::Local::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let base = image::DynamicImage::new_rgba8(16, 16);
        let annotations = vec![Annotation::new(AnnotationData::FilledBox {
            rect: Rect::from_points(Point::new(2.0, 2.0), Point::new(8.0, 8.0)),
            color: Color::rgba(255, 0, 0, 255),
        })];

        let output = save_editor_document_to_dir_with_filename(
            &requested_dir,
            &base,
            &annotations,
            "named-shot",
        )
        .expect("save to requested dir");

        assert_eq!(output, requested_dir.join("named-shot.png"));
        assert!(output.exists());
        let _ = std::fs::remove_dir_all(requested_dir);
    }
}
