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

use anyhow::{Context, Result};
use ashot_capture::{CaptureClient, CaptureError};
use ashot_core::{
    Annotation, AnnotationData, AppConfig, Color, DefaultTool, Document, EditorHistory,
    LinuxDistroFamily, OcrBackend, Point, Rect, TextStyle, TextWeight, default_ocr_languages,
    detect_linux_distro_family, finalize_capture_with_config, language_install_command,
    language_package_for_distro, render_filename, save_document_png, search_ocr_languages,
};
use ashot_ipc::{
    APP_ID, CaptureMode, CaptureOutcome, CommandOutcome, DBUS_NAME, DBUS_PATH, OutcomeKind,
};
use glib::translate::ToGlibPtr;
use gtk::gdk::prelude::ToplevelExt;
use gtk::prelude::{Cast, EventControllerExt, GestureSingleExt, NativeExt};
use gtk::{
    Align, Box as GtkBox, Button, DrawingArea, Entry, Grid, HeaderBar, Label, MenuButton,
    Orientation, Overlay, Picture, PolicyType, Popover, ScrolledWindow,
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

const EDITOR_HISTORY_LIMIT: usize = 64;

type ApplicationWindow = adw::ApplicationWindow;

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
            present_pin_window(app, path);
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
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("aShot Settings")
        .default_width(680)
        .default_height(720)
        .build();

    let config = state.config_snapshot();

    let root = GtkBox::new(Orientation::Vertical, 12);
    root.set_margin_top(16);
    root.set_margin_bottom(16);
    root.set_margin_start(16);
    root.set_margin_end(16);

    let save_dir_entry = Entry::builder()
        .hexpand(true)
        .text(config.default_save_dir.to_string_lossy().as_ref())
        .build();
    root.append(&Label::new(Some("Default save directory")));
    root.append(&save_dir_entry);

    let template_entry = Entry::builder().hexpand(true).text(&config.filename_template).build();
    root.append(&Label::new(Some("Filename template")));
    root.append(&template_entry);

    let auto_copy = gtk::CheckButton::with_label("Auto-copy saved screenshots to clipboard");
    auto_copy.set_active(config.auto_copy);
    root.append(&auto_copy);

    let open_editor = gtk::CheckButton::with_label("Open the editor after capture");
    open_editor.set_active(config.post_capture_open_editor);
    root.append(&open_editor);

    let pin_after_save = gtk::CheckButton::with_label("Pin screenshots after save");
    pin_after_save.set_active(config.pin_after_save);
    root.append(&pin_after_save);

    let ocr_title = Label::new(Some("OCR"));
    ocr_title.add_css_class("heading");
    ocr_title.set_xalign(0.0);
    root.append(&ocr_title);

    let ocr_space_backend = gtk::CheckButton::with_label("Use OCR.space online backend");
    ocr_space_backend
        .set_tooltip_text(Some("When enabled, selected OCR regions are uploaded to OCR.space."));
    ocr_space_backend.set_active(config.ocr_backend == OcrBackend::OcrSpace);
    root.append(&ocr_space_backend);

    let ocr_api_key_entry = Entry::builder()
        .hexpand(true)
        .text(&config.ocr_space_api_key)
        .placeholder_text("OCR.space API key")
        .build();
    root.append(&ocr_api_key_entry);

    let ocr_filter_symbols = gtk::CheckButton::with_label("Filter emoji and symbol noise");
    ocr_filter_symbols.set_active(config.ocr_filter_symbols);
    root.append(&ocr_filter_symbols);

    let language_search = Entry::builder()
        .hexpand(true)
        .placeholder_text("Search OCR languages, e.g. 中文, English, jpn")
        .build();
    root.append(&language_search);

    let distro = detect_linux_distro_family();
    let selected_ocr_languages = Rc::new(RefCell::new(config.ocr_languages.clone()));
    let selected_ocr_label = Label::new(None);
    selected_ocr_label.set_xalign(0.0);
    selected_ocr_label.set_wrap(true);
    root.append(&selected_ocr_label);

    let language_results = GtkBox::new(Orientation::Vertical, 6);
    root.append(&language_results);

    let install_command_label = Label::new(None);
    install_command_label.set_xalign(0.0);
    install_command_label.set_wrap(true);
    install_command_label.add_css_class("dim-label");
    root.append(&install_command_label);

    let install_actions = GtkBox::new(Orientation::Horizontal, 8);
    let copy_install_command = Button::with_label("Copy Install Command");
    install_actions.append(&copy_install_command);
    root.append(&install_actions);

    refresh_ocr_language_settings(
        &language_results,
        &selected_ocr_label,
        &install_command_label,
        "",
        selected_ocr_languages.clone(),
        distro,
    );

    let language_results_for_search = language_results.clone();
    let selected_ocr_label_for_search = selected_ocr_label.clone();
    let install_command_label_for_search = install_command_label.clone();
    let selected_ocr_languages_for_search = selected_ocr_languages.clone();
    language_search.connect_changed(move |entry| {
        refresh_ocr_language_settings(
            &language_results_for_search,
            &selected_ocr_label_for_search,
            &install_command_label_for_search,
            entry.text().as_str(),
            selected_ocr_languages_for_search.clone(),
            distro,
        );
    });

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
    actions.append(&save);
    actions.append(&reset);
    root.append(&actions);

    let state_for_save = state.clone();
    let save_dir_entry_for_save = save_dir_entry.clone();
    let template_entry_for_save = template_entry.clone();
    let auto_copy_for_save = auto_copy.clone();
    let open_editor_for_save = open_editor.clone();
    let pin_after_save_for_save = pin_after_save.clone();
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
        state_for_save.update_config(updated);
    });

    let state_for_reset = state.clone();
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
        ocr_space_backend.set_active(updated.ocr_backend == OcrBackend::OcrSpace);
        ocr_api_key_entry.set_text(&updated.ocr_space_api_key);
        ocr_filter_symbols.set_active(updated.ocr_filter_symbols);
        if let Ok(mut languages) = selected_ocr_languages.try_borrow_mut() {
            *languages = updated.ocr_languages;
        }
        refresh_ocr_language_settings(
            &language_results,
            &selected_ocr_label,
            &install_command_label,
            language_search.text().as_str(),
            selected_ocr_languages.clone(),
            distro,
        );
    });

    let scrolled = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vscrollbar_policy(PolicyType::Automatic)
        .child(&root)
        .build();
    window.set_content(Some(&scrolled));
    window.present();
}

fn refresh_ocr_language_settings(
    language_results: &GtkBox,
    selected_label: &Label,
    install_command_label: &Label,
    query: &str,
    selected_languages: Rc<RefCell<Vec<String>>>,
    distro: LinuxDistroFamily,
) {
    while let Some(child) = language_results.first_child() {
        language_results.remove(&child);
    }

    let selected_snapshot =
        selected_languages.try_borrow().map(|languages| languages.clone()).unwrap_or_default();
    selected_label
        .set_text(&format!("Selected OCR languages: {}", ocr_language_summary(&selected_snapshot)));
    install_command_label.set_text(&language_install_command(&selected_snapshot, distro));

    for language in search_ocr_languages(query).into_iter().take(8) {
        let row = GtkBox::new(Orientation::Vertical, 2);
        let header = GtkBox::new(Orientation::Horizontal, 8);
        let check = gtk::CheckButton::with_label(&format!(
            "{} ({})",
            language.display_name, language.tesseract_code
        ));
        check.set_active(ocr_language_is_selected(&selected_snapshot, language.tesseract_code));
        header.append(&check);

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
        check.connect_toggled(move |button| {
            let Ok(mut languages) = selected_for_toggle.try_borrow_mut() else {
                return;
            };
            update_ocr_language_selection(&mut languages, &language_code, button.is_active());
            selected_label_for_toggle
                .set_text(&format!("Selected OCR languages: {}", ocr_language_summary(&languages)));
            install_command_label_for_toggle
                .set_text(&language_install_command(&languages, distro));
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
    let active_text_edit = Rc::new(RefCell::new(None::<ActiveTextEdit>));
    let active_color = Rc::new(Cell::new(config.default_color));
    let active_stroke = Rc::new(Cell::new(config.default_stroke_width));
    let active_ocr_languages = Rc::new(RefCell::new(config.ocr_languages.clone()));
    let active_ocr_filter_symbols = Rc::new(Cell::new(config.ocr_filter_symbols));
    let base_image = Rc::new(image);

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

    let root = GtkBox::new(Orientation::Vertical, 8);
    root.set_vexpand(true);
    root.set_hexpand(true);
    root.set_margin_top(6);
    root.set_margin_bottom(6);
    root.set_margin_start(6);
    root.set_margin_end(6);

    let header = HeaderBar::new();
    root.append(&header);
    info!("present_editor header installed");

    let undo = action_icon_button("edit-undo-symbolic", "Undo");
    let redo = action_icon_button("edit-redo-symbolic", "Redo");
    let copy = action_icon_button("edit-copy-symbolic", "Copy to Clipboard");
    let pin = action_icon_button("view-pin-symbolic", "Pin Screenshot");
    let save = action_text_button("document-save-symbolic", "Save", "Save");
    let save_close = action_text_button("document-save-as-symbolic", "Done", "Save and Close");

    let overlay = Overlay::new();
    overlay.set_hexpand(true);
    overlay.set_vexpand(true);
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

    let scrolled = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Automatic)
        .vscrollbar_policy(PolicyType::Automatic)
        .min_content_width(720)
        .min_content_height(420)
        .hexpand(true)
        .vexpand(true)
        .child(&overlay)
        .build();
    scrolled.add_css_class("view");

    let editor_body = GtkBox::new(Orientation::Horizontal, 8);
    editor_body.set_hexpand(true);
    editor_body.set_vexpand(true);

    let left_panel = GtkBox::new(Orientation::Vertical, 10);
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
    save.set_hexpand(true);
    save_close.set_hexpand(true);
    output_actions.append(&save);
    output_actions.append(&save_close);
    left_panel.append(&output_actions);
    update_history_action_buttons(&history, &undo, &redo);

    left_panel.append(&section_title("Tools"));
    let tool_grid = Grid::new();
    tool_grid.set_row_spacing(6);
    tool_grid.set_column_spacing(6);
    let tool_buttons = Rc::new(RefCell::new(Vec::<(DefaultTool, Button)>::new()));
    for (index, (label, tool)) in editor_tool_layout().iter().enumerate() {
        let button = Button::with_label(label);
        button.set_tooltip_text(Some(label));
        button.set_hexpand(true);
        button.set_size_request(74, 34);
        let document = document.clone();
        let tool_buttons_for_click = tool_buttons.clone();
        let tool = *tool;
        button.connect_clicked(move |_| {
            if let Ok(mut document) = document.try_borrow_mut() {
                document.active_tool = tool;
            }
            update_tool_button_selection(&tool_buttons_for_click, tool);
        });
        tool_grid.attach(&button, (index % 3) as i32, (index / 3) as i32, 1, 1);
        tool_buttons.borrow_mut().push((tool, button));
    }
    left_panel.append(&tool_grid);
    info!("present_editor tool grid added");

    left_panel.append(&section_title("OCR"));
    let ocr_tool_button = Button::with_label("OCR");
    ocr_tool_button.set_tooltip_text(Some("Recognize text from a selected region"));
    ocr_tool_button.set_hexpand(true);
    ocr_tool_button.set_size_request(0, 38);
    let document_for_ocr_button = document.clone();
    let tool_buttons_for_ocr = tool_buttons.clone();
    ocr_tool_button.connect_clicked(move |_| {
        if let Ok(mut document) = document_for_ocr_button.try_borrow_mut() {
            document.active_tool = DefaultTool::Ocr;
        }
        update_tool_button_selection(&tool_buttons_for_ocr, DefaultTool::Ocr);
    });
    tool_buttons.borrow_mut().push((DefaultTool::Ocr, ocr_tool_button.clone()));
    left_panel.append(&ocr_tool_button);
    let ocr_language_selector = ocr_language_menu(active_ocr_languages.clone());
    left_panel.append(&ocr_language_selector);
    let ocr_filter_symbols_toggle = gtk::CheckButton::with_label("Filter emoji/symbols");
    ocr_filter_symbols_toggle.set_active(config.ocr_filter_symbols);
    ocr_filter_symbols_toggle
        .set_tooltip_text(Some("Remove emoji and decorative symbol noise from OCR text"));
    let active_ocr_filter_symbols_for_toggle = active_ocr_filter_symbols.clone();
    ocr_filter_symbols_toggle.connect_toggled(move |button| {
        active_ocr_filter_symbols_for_toggle.set(button.is_active());
    });
    left_panel.append(&ocr_filter_symbols_toggle);
    info!("present_editor ocr section added");
    update_tool_button_selection(&tool_buttons, config.default_tool);

    let color_header = GtkBox::new(Orientation::Horizontal, 8);
    color_header.append(&section_title("Color"));
    let selected_color_preview = selected_color_preview(active_color.clone());
    selected_color_preview.set_hexpand(true);
    selected_color_preview.set_halign(Align::End);
    color_header.append(&selected_color_preview);
    left_panel.append(&color_header);
    let color_grid = Grid::new();
    color_grid.set_row_spacing(4);
    color_grid.set_column_spacing(4);
    let color_buttons = Rc::new(RefCell::new(Vec::<(Color, Button)>::new()));
    let stroke_previews = Rc::new(RefCell::new(Vec::<DrawingArea>::new()));
    for (index, (name, color)) in editor_color_palette().iter().enumerate() {
        let button = color_swatch_button(name, *color);
        let active_color_for_click = active_color.clone();
        let color_buttons_for_click = color_buttons.clone();
        let stroke_previews_for_click = stroke_previews.clone();
        let selected_color_preview_for_click = selected_color_preview.clone();
        let color = *color;
        button.connect_clicked(move |_| {
            active_color_for_click.set(color);
            update_color_button_selection(&color_buttons_for_click, color);
            selected_color_preview_for_click.queue_draw();
            if let Ok(stroke_previews) = stroke_previews_for_click.try_borrow() {
                for preview in stroke_previews.iter() {
                    preview.queue_draw();
                }
            }
        });
        color_grid.attach(&button, (index % 8) as i32, (index / 8) as i32, 1, 1);
        color_buttons.borrow_mut().push((color, button));
    }
    update_color_button_selection(&color_buttons, active_color.get());
    left_panel.append(&color_grid);
    info!("present_editor color section added");

    left_panel.append(&section_title("Stroke / Effect"));
    let stroke_menu =
        stroke_width_dropdown(active_stroke.clone(), active_color.clone(), stroke_previews.clone());
    left_panel.append(&stroke_menu);
    info!("present_editor stroke section added");

    let info = Label::new(Some(
        "The full screenshot is fit to the canvas. Drag tools preview live before release.",
    ));
    info.set_wrap(true);
    info.set_xalign(0.0);
    info.add_css_class("dim-label");
    left_panel.append(&info);

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
    canvas.set_draw_func(move |_, cr, _, _| {
        let _ = cr.save();
        cr.scale(scale_for_draw.get(), scale_for_draw.get());
        cr.set_line_join(gtk::cairo::LineJoin::Round);
        cr.set_line_cap(gtk::cairo::LineCap::Round);
        if let Some(annotations) = annotation_snapshot_for_draw(&doc_for_draw) {
            for annotation in &annotations {
                draw_annotation(cr, annotation);
            }
        }
        if let Some(annotation) = draft_preview_for_draw(&draft_for_draw) {
            draw_annotation(cr, &annotation);
        }
        let _ = cr.restore();
    });

    let click = gtk::GestureClick::new();
    let doc_for_click = document.clone();
    let draft_for_click = draft.clone();
    let history_for_click = history.clone();
    let canvas_for_click = canvas.clone();
    let color_for_click = active_color.clone();
    let undo_for_click = undo.clone();
    let redo_for_click = redo.clone();
    let scale_for_click = image_scale.clone();
    let text_entry_for_click = text_entry.clone();
    let active_text_edit_for_click = active_text_edit.clone();
    click.connect_pressed(move |_, _, x, y| {
        let point = scaled_canvas_point(x, y, scale_for_click.get());
        let Ok(document) = doc_for_click.try_borrow() else {
            return;
        };
        let active_tool = document.active_tool;
        drop(document);

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
                update_history_action_buttons(&history_for_click, &undo_for_click, &redo_for_click);
                canvas_for_click.queue_draw();
            }
            return;
        }

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
    });
    canvas.add_controller(click);
    info!("present_editor click controller added");

    let doc_for_text_commit = document.clone();
    let history_for_text_commit = history.clone();
    let canvas_for_text_commit = canvas.clone();
    let active_text_edit_for_commit = active_text_edit.clone();
    let undo_for_text_commit = undo.clone();
    let redo_for_text_commit = redo.clone();
    text_entry.connect_activate(move |entry| {
        commit_text_entry(
            entry,
            &doc_for_text_commit,
            &history_for_text_commit,
            &canvas_for_text_commit,
            &active_text_edit_for_commit,
            &undo_for_text_commit,
            &redo_for_text_commit,
        );
    });

    let focus = gtk::EventControllerFocus::new();
    let doc_for_text_focus = document.clone();
    let history_for_text_focus = history.clone();
    let canvas_for_text_focus = canvas.clone();
    let active_text_edit_for_focus = active_text_edit.clone();
    let undo_for_text_focus = undo.clone();
    let redo_for_text_focus = redo.clone();
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
            );
        }
    });
    text_entry.add_controller(focus);
    info!("present_editor text focus controller added");

    let drag = gtk::GestureDrag::new();
    let doc_for_drag = document.clone();
    let draft_for_drag = draft.clone();
    let moving_for_drag = moving.clone();
    let history_for_drag = history.clone();
    let color_for_drag = active_color.clone();
    let stroke_for_drag = active_stroke.clone();
    let scale_for_drag = image_scale.clone();
    let undo_for_drag = undo.clone();
    let redo_for_drag = redo.clone();
    drag.connect_drag_begin(move |_, x, y| {
        let point = scaled_canvas_point(x, y, scale_for_drag.get());
        let Some((selected, annotations, tool)) = ({
            let Ok(mut document) = doc_for_drag.try_borrow_mut() else {
                return;
            };
            let selected = document.select_at(point).is_some();
            Some((selected, document.annotations.clone(), document.active_tool))
        }) else {
            return;
        };

        if !selected && !draft_tool_can_draw(tool) {
            return;
        }

        if let Ok(mut history) = history_for_drag.try_borrow_mut() {
            history.snapshot(&annotations);
        }
        update_history_action_buttons(&history_for_drag, &undo_for_drag, &redo_for_drag);

        if selected {
            if let Ok(mut moving) = moving_for_drag.try_borrow_mut() {
                *moving = Some(point);
            }
            return;
        }

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
    let canvas_for_update = canvas.clone();
    let scale_for_update = image_scale.clone();
    drag.connect_drag_update(move |gesture, dx, dy| {
        let (start_x, start_y) = gesture.start_point().unwrap_or((0.0, 0.0));
        let current = scaled_canvas_point(start_x + dx, start_y + dy, scale_for_update.get());
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
    let canvas_for_end = canvas.clone();
    let state_for_ocr = state.clone();
    let runtime_for_ocr = runtime.clone();
    let image_for_ocr = base_image.clone();
    let window_for_ocr = window.clone();
    let active_ocr_languages_for_end = active_ocr_languages.clone();
    let active_ocr_filter_symbols_for_end = active_ocr_filter_symbols.clone();
    drag.connect_drag_end(move |_, _, _| {
        if let Ok(mut moving) = moving_for_end.try_borrow_mut() {
            *moving = None;
        }
        let draft = draft_for_end.try_borrow_mut().ok().and_then(|mut draft| draft.take());
        let Some(draft) = draft else {
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
            );
            canvas_for_end.queue_draw();
            return;
        }

        let annotation = draft.finish();
        if let Some(annotation) = annotation {
            if let Ok(mut document) = doc_for_end.try_borrow_mut() {
                document.add_annotation(annotation);
            }
        }
        canvas_for_end.queue_draw();
    });
    canvas.add_controller(drag);
    info!("present_editor drag controller added");

    let doc_for_undo = document.clone();
    let canvas_for_undo = canvas.clone();
    let history_for_undo = history.clone();
    let undo_for_undo = undo.clone();
    let redo_for_undo = redo.clone();
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
            update_history_action_buttons(&history_for_undo, &undo_for_undo, &redo_for_undo);
        }
    });

    let doc_for_redo = document.clone();
    let canvas_for_redo = canvas.clone();
    let history_for_redo = history.clone();
    let undo_for_redo = undo.clone();
    let redo_for_redo = redo.clone();
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
            update_history_action_buttons(&history_for_redo, &undo_for_redo, &redo_for_redo);
        }
    });

    let doc_for_save = document.clone();
    let state_for_save = state.clone();
    let image_for_save = base_image.clone();
    let path_for_save = image_path.clone();
    save.connect_clicked(move |button| {
        let config = state_for_save.config_snapshot();
        let initial_filename = suggested_save_filename_at(&config, chrono::Local::now());
        let doc_for_confirm = doc_for_save.clone();
        let state_for_confirm = state_for_save.clone();
        let image_for_confirm = image_for_save.clone();
        let path_for_confirm = path_for_save.clone();
        show_save_filename_popover(button, initial_filename, "Save", move |requested_filename| {
            let config = state_for_confirm.config_snapshot();
            let Some(annotations) = annotation_snapshot_for_draw(&doc_for_confirm) else {
                return false;
            };
            match save_editor_document_with_filename(
                &config,
                &image_for_confirm,
                &annotations,
                &requested_filename,
            ) {
                Ok(output) => {
                    if config.auto_copy {
                        copy_image_to_clipboard(&output);
                    }
                    info!("saved screenshot based on {}", path_for_confirm.display());
                    true
                }
                Err(error) => {
                    error!("{error}");
                    false
                }
            }
        });
    });

    let doc_for_save_close = document.clone();
    let state_for_save_close = state.clone();
    let image_for_save_close = base_image.clone();
    let window_for_save_close = window.clone();
    let app_for_save_close = app.clone();
    save_close.connect_clicked(move |button| {
        let config = state_for_save_close.config_snapshot();
        let initial_filename = suggested_save_filename_at(&config, chrono::Local::now());
        let doc_for_confirm = doc_for_save_close.clone();
        let state_for_confirm = state_for_save_close.clone();
        let image_for_confirm = image_for_save_close.clone();
        let window_for_confirm = window_for_save_close.clone();
        let app_for_confirm = app_for_save_close.clone();
        show_save_filename_popover(button, initial_filename, "Save", move |requested_filename| {
            let config = state_for_confirm.config_snapshot();
            let Some(annotations) = annotation_snapshot_for_draw(&doc_for_confirm) else {
                return false;
            };
            match save_editor_document_with_filename(
                &config,
                &image_for_confirm,
                &annotations,
                &requested_filename,
            ) {
                Ok(output) => {
                    if config.auto_copy {
                        copy_image_to_clipboard(&output);
                    }
                    if config.pin_after_save {
                        present_pin_window(&app_for_confirm, output);
                    }
                    window_for_confirm.close();
                    true
                }
                Err(error) => {
                    error!("{error}");
                    false
                }
            }
        });
    });

    let doc_for_copy = document.clone();
    let image_for_copy = base_image.clone();
    copy.connect_clicked(move |_| {
        let temp = std::env::temp_dir().join("ashot_clipboard.png");
        let Some(annotations) = annotation_snapshot_for_draw(&doc_for_copy) else {
            return;
        };
        if save_document_png(&image_for_copy, &annotations, &temp).is_ok() {
            copy_image_to_clipboard(&temp);
        }
    });

    let doc_for_pin = document.clone();
    let image_for_pin = base_image.clone();
    let app_for_pin = app.clone();
    pin.connect_clicked(move |_| {
        let temp = std::env::temp_dir().join("ashot_pin.png");
        let Some(annotations) = annotation_snapshot_for_draw(&doc_for_pin) else {
            return;
        };
        if save_document_png(&image_for_pin, &annotations, &temp).is_ok() {
            present_pin_window(&app_for_pin, temp);
        }
    });

    root.append(&Label::new(Some(
        "Drag to draw. Click text on the screenshot to edit it in place.",
    )));
    window.set_content(Some(&root));
    window.present();
    info!("present_editor done: {}", image_path.display());
    Ok(())
}

fn present_pin_window(app: &adw::Application, image_path: PathBuf) {
    let (image_width, image_height) = image::image_dimensions(&image_path).unwrap_or((480, 320));
    let (window_width, window_height) = pin_window_size(image_width, image_height);
    let initial_scale = pin_initial_scale(image_width, image_height, window_width, window_height);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("Pinned Screenshot")
        .default_width(window_width)
        .default_height(window_height)
        .build();
    window.set_resizable(false);

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
    scroll.connect_scroll(move |_, _, dy| {
        let next = pin_zoom_from_scroll(scale_for_scroll.get(), dy);
        scale_for_scroll.set(next);
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
    click.connect_pressed(move |gesture, n_press, x, y| {
        match pin_click_action(n_press, gesture.current_button()) {
            Some(PinClickAction::Close) => window_for_click.close(),
            Some(PinClickAction::Move) => begin_pin_window_move(&window_for_click, gesture, x, y),
            None => {}
        }
    });
    image_overlay.add_controller(click);

    window.set_content(Some(&image_overlay));
    window.present();
}

fn action_icon_button(icon_name: &str, tooltip: &str) -> Button {
    let button = Button::from_icon_name(icon_name);
    button.set_tooltip_text(Some(tooltip));
    button.set_size_request(54, 36);
    button.add_css_class("flat");
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
    button.set_size_request(0, 36);
    button
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
}

fn pin_click_action(n_press: i32, button: u32) -> Option<PinClickAction> {
    if button != 1 {
        return None;
    }

    match n_press {
        1 => Some(PinClickAction::Move),
        2 => Some(PinClickAction::Close),
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
    title.set_xalign(0.0);
    title
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

fn ocr_language_menu(active_languages: Rc<RefCell<Vec<String>>>) -> MenuButton {
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
        check.connect_toggled(move |button| {
            if let Ok(mut languages) = active_languages_for_toggle.try_borrow_mut() {
                update_ocr_language_selection(&mut languages, &language_code, button.is_active());
                menu_for_toggle.set_label(&ocr_language_label(&languages));
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

fn editor_stroke_widths() -> [u32; 5] {
    [2, 4, 6, 8, 12]
}

fn color_swatch_button(name: &str, color: Color) -> Button {
    let area = DrawingArea::new();
    area.set_content_width(22);
    area.set_content_height(22);
    area.set_draw_func(move |_, cr, width, height| {
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.12);
        cr.rectangle(1.0, 1.0, width as f64 - 2.0, height as f64 - 2.0);
        let _ = cr.fill();
        set_cairo_color(cr, color);
        cr.rectangle(2.0, 2.0, width as f64 - 4.0, height as f64 - 4.0);
        let _ = cr.fill_preserve();
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.35);
        cr.set_line_width(1.0);
        let _ = cr.stroke();
    });

    let button = Button::new();
    button.set_child(Some(&area));
    button.set_tooltip_text(Some(name));
    button.set_size_request(28, 28);
    button.add_css_class("flat");
    button
}

fn selected_color_preview(active_color: Rc<Cell<Color>>) -> DrawingArea {
    let area = DrawingArea::new();
    area.set_content_width(42);
    area.set_content_height(20);
    area.set_draw_func(move |_, cr, width, height| {
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.12);
        cr.rectangle(0.5, 0.5, width as f64 - 1.0, height as f64 - 1.0);
        let _ = cr.stroke();
        set_cairo_color(cr, active_color.get());
        cr.rectangle(3.0, 3.0, width as f64 - 6.0, height as f64 - 6.0);
        let _ = cr.fill();
    });
    area
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
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.14);
        cr.set_line_width(stroke_width as f64 + 2.0);
        cr.set_line_cap(gtk::cairo::LineCap::Round);
        let y = area_height as f64 / 2.0;
        cr.move_to(10.0, y);
        cr.line_to(area_width as f64 - 10.0, y);
        let _ = cr.stroke();
        set_cairo_color(cr, active_color.get());
        cr.set_line_width(stroke_width as f64);
        cr.set_line_cap(gtk::cairo::LineCap::Round);
        cr.move_to(10.0, y);
        cr.line_to(area_width as f64 - 10.0, y);
        let _ = cr.stroke();
    });
    area
}

fn fixed_stroke_preview(width: u32, active_color: Rc<Cell<Color>>) -> DrawingArea {
    let area = DrawingArea::new();
    area.set_content_width(86);
    area.set_content_height(24);
    area.set_draw_func(move |_, cr, area_width, area_height| {
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.14);
        cr.set_line_width(width as f64 + 2.0);
        cr.set_line_cap(gtk::cairo::LineCap::Round);
        let y = area_height as f64 / 2.0;
        cr.move_to(10.0, y);
        cr.line_to(area_width as f64 - 10.0, y);
        let _ = cr.stroke();
        set_cairo_color(cr, active_color.get());
        cr.set_line_width(width as f64);
        cr.set_line_cap(gtk::cairo::LineCap::Round);
        cr.move_to(10.0, y);
        cr.line_to(area_width as f64 - 10.0, y);
        let _ = cr.stroke();
    });
    area
}

fn stroke_width_dropdown(
    active_stroke: Rc<Cell<u32>>,
    active_color: Rc<Cell<Color>>,
    stroke_previews: Rc<RefCell<Vec<DrawingArea>>>,
) -> GtkBox {
    let container = GtkBox::new(Orientation::Horizontal, 8);
    let current = current_stroke_preview(active_color.clone(), active_stroke.clone(), 118, 24);
    current.set_hexpand(true);
    stroke_previews.borrow_mut().push(current.clone());

    let menu_button = MenuButton::new();
    menu_button.set_label(&format!("{}px", active_stroke.get()));
    menu_button.set_tooltip_text(Some("Stroke width / effect strength"));
    menu_button.set_size_request(76, 36);

    let popover = Popover::new();
    let list = GtkBox::new(Orientation::Vertical, 4);
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
        button.set_hexpand(true);

        let active_stroke_for_click = active_stroke.clone();
        let stroke_previews_for_click = stroke_previews.clone();
        let menu_button_for_click = menu_button.clone();
        button.connect_clicked(move |_| {
            active_stroke_for_click.set(width);
            menu_button_for_click.set_label(&format!("{width}px"));
            if let Ok(stroke_previews) = stroke_previews_for_click.try_borrow() {
                for preview in stroke_previews.iter() {
                    preview.queue_draw();
                }
            }
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
                button.add_css_class("suggested-action");
            } else {
                button.remove_css_class("suggested-action");
            }
        }
    }
}

fn update_color_button_selection(buttons: &Rc<RefCell<Vec<(Color, Button)>>>, selected: Color) {
    if let Ok(buttons) = buttons.try_borrow() {
        for (color, button) in buttons.iter() {
            if *color == selected {
                button.add_css_class("suggested-action");
            } else {
                button.remove_css_class("suggested-action");
            }
        }
    }
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
        update_history_action_buttons(history, undo, redo);
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

fn arrow_head_points(start: Point, end: Point, stroke_width: u32) -> (Point, Point) {
    let angle = (end.y - start.y).atan2(end.x - start.x);
    let head_len = (stroke_width as f32 * 2.6).max(10.0);
    let left = Point::new(
        end.x - head_len * (angle - std::f32::consts::FRAC_PI_6).cos(),
        end.y - head_len * (angle - std::f32::consts::FRAC_PI_6).sin(),
    );
    let right = Point::new(
        end.x - head_len * (angle + std::f32::consts::FRAC_PI_6).cos(),
        end.y - head_len * (angle + std::f32::consts::FRAC_PI_6).sin(),
    );
    (left, right)
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
            draw_cairo_segment(cr, *start, *end, *color, *stroke_width);
            let (left, right) = arrow_head_points(*start, *end, *stroke_width);
            draw_cairo_segment(cr, *end, left, *color, *stroke_width);
            draw_cairo_segment(cr, *end, right, *color, *stroke_width);
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
        let file = gio::File::for_path(path);
        if let Ok(texture) = gtk::gdk::Texture::from_file(&file) {
            display.clipboard().set_texture(&texture);
        }
    }
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

fn save_editor_document_with_filename(
    config: &AppConfig,
    base: &image::DynamicImage,
    annotations: &[Annotation],
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
    save_document_png(base, annotations, &output)
        .map_err(|source| format!("failed to save screenshot at {}: {source}", output.display()))?;
    Ok(output)
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
) {
    let Some(crop) = crop_image_region(base_image, rect) else {
        show_ocr_result_dialog(parent, Err("Select a non-empty OCR region".to_string()));
        return;
    };
    let crop_path = std::env::temp_dir().join(format!(
        "ashot_ocr_{}_{}.png",
        std::process::id(),
        chrono::Local::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    if let Err(error) = crop.save(&crop_path) {
        show_ocr_result_dialog(parent, Err(format!("failed to prepare OCR image: {error}")));
        return;
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
    glib::timeout_add_local(Duration::from_millis(80), move || match rx.try_recv() {
        Ok(result) => {
            show_ocr_result_dialog(&parent_for_poll, result);
            glib::ControlFlow::Break
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
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
    let window = gtk::Window::builder()
        .title("OCR Result")
        .default_width(520)
        .default_height(360)
        .transient_for(parent)
        .build();

    let root = GtkBox::new(Orientation::Vertical, 10);
    root.set_margin_top(14);
    root.set_margin_bottom(14);
    root.set_margin_start(14);
    root.set_margin_end(14);

    let title = Label::new(Some(match result {
        Ok(_) => "Recognized Text",
        Err(_) => "OCR Failed",
    }));
    title.add_css_class("heading");
    title.set_xalign(0.0);
    root.append(&title);

    let text = match result {
        Ok(text) => text,
        Err(error) => error,
    };
    let text_view = gtk::TextView::new();
    text_view.set_wrap_mode(gtk::WrapMode::WordChar);
    text_view.set_monospace(false);
    text_view.buffer().set_text(&text);
    let scrolled = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Automatic)
        .vscrollbar_policy(PolicyType::Automatic)
        .hexpand(true)
        .vexpand(true)
        .child(&text_view)
        .build();
    root.append(&scrolled);

    let actions = GtkBox::new(Orientation::Horizontal, 8);
    actions.set_halign(Align::End);
    let copy_selected = Button::with_label("Copy Selected");
    let copy_all = Button::with_label("Copy All");
    let close = Button::with_label("Close");
    actions.append(&copy_selected);
    actions.append(&copy_all);
    actions.append(&close);
    root.append(&actions);

    let text_view_for_selected = text_view.clone();
    copy_selected.connect_clicked(move |_| {
        let buffer = text_view_for_selected.buffer();
        if let Some((start, end)) = buffer.selection_bounds() {
            let selected = buffer.text(&start, &end, true);
            copy_text_to_clipboard(selected.as_str());
        }
    });

    let text_for_all = text.clone();
    copy_all.connect_clicked(move |_| {
        copy_text_to_clipboard(&text_for_all);
    });

    let window_for_close = window.clone();
    close.connect_clicked(move |_| {
        window_for_close.close();
    });

    window.set_child(Some(&root));
    window.present();
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

fn show_save_filename_popover<F>(
    anchor: &Button,
    initial_filename: String,
    confirm_label: &str,
    on_confirm: F,
) where
    F: Fn(String) -> bool + 'static,
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

    let on_confirm = Rc::new(on_confirm);
    let confirm_action = {
        let entry = entry.clone();
        let error = error.clone();
        let popover = popover.clone();
        let on_confirm = on_confirm.clone();
        move || {
            let filename = entry.text().to_string();
            if on_confirm(filename) {
                popover.popdown();
                popover.unparent();
            } else {
                error.set_text("Enter a valid PNG file name");
                entry.grab_focus();
            }
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

    use ashot_core::{AnnotationData, AppConfig, Color, DefaultTool, Document, Point, Rect};

    use super::{
        ActiveTextEdit, DraftAnnotation, PinClickAction, annotation_snapshot_for_draw,
        arrow_head_points, blur_radius_for_stroke, capture_should_use_fresh_anchor,
        crop_image_region, draft_preview_for_draw, draft_tool_can_draw, editor_color_palette,
        editor_initial_size, editor_stroke_widths, editor_tool_layout, filter_ocr_symbols,
        fit_scale, mosaic_pixel_size_for_stroke, moving_delta_and_update, normalized_save_filename,
        ocr_language_label, ocr_space_curl_args, ocr_space_language_arg, parse_ocr_space_response,
        pin_click_action, pin_dimension_label, pin_display_size, pin_initial_scale,
        pin_window_size, pin_window_size_for_scale, pin_zoom_from_scroll, scaled_canvas_point,
        set_active_text_edit, suggested_save_filename_at, take_active_text_edit,
        tesseract_command_args, tesseract_command_invocation, text_for_annotation,
        update_ocr_language_selection,
    };

    use chrono::{Local, TimeZone};

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
        assert!(draft_tool_can_draw(DefaultTool::Rectangle));
        assert!(draft_tool_can_draw(DefaultTool::Brush));
        assert!(draft_tool_can_draw(DefaultTool::Blur));
        assert!(draft_tool_can_draw(DefaultTool::Ocr));

        assert!(!draft_tool_can_draw(DefaultTool::Select));
        assert!(!draft_tool_can_draw(DefaultTool::Text));
        assert!(!draft_tool_can_draw(DefaultTool::Counter));
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
        assert_eq!(pin_click_action(1, 3), None);
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
}
