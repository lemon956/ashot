use std::{
    cell::{Cell, RefCell},
    env,
    ffi::CStr,
    fs,
    os::raw::c_char,
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use ashot_capture::{CaptureClient, CaptureError};
use ashot_core::{
    Annotation, AnnotationData, AppConfig, Color, DefaultTool, Document, EditorHistory, Point,
    Rect, TextStyle, TextWeight, finalize_capture_with_config, save_document_png, save_with_config,
};
use ashot_ipc::{
    APP_ID, CaptureMode, CaptureOutcome, CommandOutcome, DBUS_NAME, DBUS_PATH, OutcomeKind,
};
use gtk::{
    Align, ApplicationWindow, Box as GtkBox, Button, ComboBoxText, DrawingArea, Entry, HeaderBar,
    Label, Orientation, Overlay, Picture, PolicyType, ScrolledWindow,
};
use gtk4 as gtk;
use gtk::prelude::Cast;
use glib::translate::ToGlibPtr;
use libadwaita as adw;
use libadwaita::prelude::*;
use tokio::{
    runtime::{Builder, Handle},
    sync::{Mutex as AsyncMutex, mpsc, oneshot},
};
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, fmt};
use url::Url;
use zbus::connection::Builder as ConnectionBuilder;

pub fn run() -> Result<()> {
    let _ = fmt().with_env_filter(EnvFilter::from_env("ASHOT_LOG")).with_target(false).try_init();

    let args = env::args().collect::<Vec<_>>();
    let service_only = args.iter().any(|arg| arg == "--service");
    let filtered_args =
        args.iter().filter(|arg| arg.as_str() != "--service").cloned().collect::<Vec<_>>();
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

    let app = adw::Application::builder().application_id(APP_ID).build();
    let _ = adw::init();
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
                if config.post_capture_open_editor {
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
    async fn finalize_capture(&self, source_file_uri: &str, annotations_json: &str) -> CaptureOutcome {
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
        self.app.connect_activate(move |app| {
            if !service_only {
                present_launcher(app, state.clone(), runtime.clone());
            }
        });

        let app = self.app.clone();
        let state = self.state.clone();
        let runtime = self.runtime.clone();
        let receiver = self.ui_rx.clone();
        glib::timeout_add_local(Duration::from_millis(60), move || {
            while let Ok(command) = receiver.borrow_mut().try_recv() {
                match command {
                    UiCommand::OpenEditor(path) => {
                        if let Err(error) =
                            present_editor(&app, state.clone(), runtime.clone(), path)
                        {
                            error!("failed to open editor: {error:#}");
                        }
                    }
                    UiCommand::OpenSettings => {
                        present_settings(&app, state.clone());
                    }
                    UiCommand::Pin(path) => {
                        present_pin_window(&app, path);
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

fn begin_capture_request(
    app: &adw::Application,
    state: Arc<ServiceState>,
    runtime: Handle,
    mode: CaptureMode,
    respond_to: oneshot::Sender<CaptureOutcome>,
) {
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
    if let Some(window) = app.active_window() {
        return (window, false);
    }

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
    let body = Label::new(Some("This window only exists to anchor the portal dialog under Wayland."));
    body.set_wrap(true);
    body.set_xalign(0.0);
    root.append(&title);
    root.append(&body);
    window.set_content(Some(&root));
    window.present();
    (window.upcast::<gtk::Window>(), true)
}

async fn export_parent_window_identifier(window: &gtk::Window) -> std::result::Result<String, String> {
    if !gtk::prelude::WidgetExt::display(window).backend().is_wayland() {
        return Err("aShot currently requires a Wayland-backed GTK window".into());
    }

    let surface = window
        .surface()
        .ok_or_else(|| "capture window has no GDK surface yet".to_string())?;
    let toplevel = surface
        .dynamic_cast::<gtk::gdk::Toplevel>()
        .map_err(|_| "capture surface is not a toplevel surface".to_string())?;
    let (send_handle, receive_handle) =
        oneshot::channel::<std::result::Result<String, String>>();
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

    let exported_handle = receive_handle
        .await
        .map_err(|_| "GTK dropped the Wayland parent handle export before completion".to_string())?;
    let handle = exported_handle?;
    Ok(format!("wayland:{handle}"))
}

unsafe extern "C" fn exported_toplevel_handle(
    _toplevel: *mut gtk::gdk::ffi::GdkToplevel,
    handle: *const c_char,
    user_data: glib::ffi::gpointer,
) {
    let callback_state = unsafe {
        &*(user_data as *const RefCell<Option<oneshot::Sender<std::result::Result<String, String>>>>)
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
        "Use the buttons below for testing. In daily use, bind `ashot capture area` to a GNOME custom shortcut.",
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
        .default_width(560)
        .default_height(360)
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
    save.connect_clicked(move |_| {
        let mut updated = state_for_save.config_snapshot();
        updated.default_save_dir = PathBuf::from(save_dir_entry_for_save.text().as_str());
        updated.filename_template = template_entry_for_save.text().to_string();
        updated.auto_copy = auto_copy_for_save.is_active();
        updated.post_capture_open_editor = open_editor_for_save.is_active();
        updated.pin_after_save = pin_after_save_for_save.is_active();
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
    });

    window.set_content(Some(&root));
    window.present();
}

fn present_editor(
    app: &adw::Application,
    state: Arc<ServiceState>,
    _runtime: Handle,
    image_path: PathBuf,
) -> Result<()> {
    let image = image::open(&image_path)
        .with_context(|| format!("failed to load screenshot at {}", image_path.display()))?;
    let config = state.config_snapshot();
    let document =
        Rc::new(RefCell::new(Document::new(image.width(), image.height(), config.default_tool)));
    let history = Rc::new(RefCell::new(EditorHistory::new(64)));
    let draft = Rc::new(RefCell::new(None::<DraftAnnotation>));
    let moving = Rc::new(RefCell::new(None::<Point>));
    let base_image = Rc::new(image);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("aShot Editor")
        .default_width((base_image.width() as i32).min(1440))
        .default_height((base_image.height() as i32 + 160).min(980))
        .build();

    let root = GtkBox::new(Orientation::Vertical, 8);
    root.set_margin_top(8);
    root.set_margin_bottom(8);
    root.set_margin_start(8);
    root.set_margin_end(8);

    let header = HeaderBar::new();
    let tool_combo = ComboBoxText::new();
    for tool in ["Text", "Arrow", "Brush", "Rectangle", "Mosaic"] {
        tool_combo.append_text(tool);
    }
    tool_combo.set_active(Some(tool_index(config.default_tool)));

    let color_combo = ComboBoxText::new();
    for label in ["Red", "Blue", "Green", "Yellow", "Black"] {
        color_combo.append_text(label);
    }
    color_combo.set_active(Some(0));

    let stroke_combo = ComboBoxText::new();
    for width in ["2", "4", "8", "12"] {
        stroke_combo.append_text(width);
    }
    stroke_combo.set_active(Some(1));

    header.pack_start(&tool_combo);
    header.pack_start(&color_combo);
    header.pack_start(&stroke_combo);
    window.set_titlebar(Some(&header));

    let overlay = Overlay::new();
    let picture = Picture::for_filename(&image_path);
    picture.set_can_shrink(false);
    picture.set_halign(Align::Start);
    picture.set_valign(Align::Start);
    picture.set_size_request(base_image.width() as i32, base_image.height() as i32);
    overlay.set_child(Some(&picture));

    let canvas = DrawingArea::new();
    canvas.set_content_width(base_image.width() as i32);
    canvas.set_content_height(base_image.height() as i32);
    canvas.set_halign(Align::Start);
    canvas.set_valign(Align::Start);
    overlay.add_overlay(&canvas);

    let scrolled = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Automatic)
        .vscrollbar_policy(PolicyType::Automatic)
        .child(&overlay)
        .build();
    root.append(&scrolled);

    let actions = GtkBox::new(Orientation::Horizontal, 8);
    let undo = Button::with_label("Undo");
    let redo = Button::with_label("Redo");
    let save = Button::with_label("Save");
    let save_close = Button::with_label("Save & Close");
    let copy = Button::with_label("Copy");
    let pin = Button::with_label("Pin");
    actions.append(&undo);
    actions.append(&redo);
    actions.append(&save);
    actions.append(&save_close);
    actions.append(&copy);
    actions.append(&pin);
    root.append(&actions);

    let doc_for_draw = document.clone();
    canvas.set_draw_func(move |_, cr, _, _| {
        let document = doc_for_draw.borrow();
        cr.set_line_join(gtk::cairo::LineJoin::Round);
        cr.set_line_cap(gtk::cairo::LineCap::Round);
        for annotation in &document.annotations {
            draw_annotation(cr, annotation);
        }
    });

    let doc_for_tool = document.clone();
    tool_combo.connect_changed(move |combo| {
        if let Some(text) = combo.active_text() {
            doc_for_tool.borrow_mut().active_tool = match text.as_str() {
                "Text" => DefaultTool::Text,
                "Arrow" => DefaultTool::Arrow,
                "Brush" => DefaultTool::Brush,
                "Rectangle" => DefaultTool::Rectangle,
                _ => DefaultTool::Mosaic,
            };
        }
    });

    let click = gtk::GestureClick::new();
    let doc_for_click = document.clone();
    let draft_for_click = draft.clone();
    let history_for_click = history.clone();
    let canvas_for_click = canvas.clone();
    let parent_for_click = window.clone();
    let color_combo_for_click = color_combo.clone();
    click.connect_pressed(move |_, _, x, y| {
        let point = Point::new(x as f32, y as f32);
        let mut document = doc_for_click.borrow_mut();
        if document.active_tool == DefaultTool::Text {
            history_for_click.borrow_mut().snapshot(&document.annotations);
            let text = prompt_text(&parent_for_click);
            if !text.is_empty() {
                document.add_annotation(Annotation::new(AnnotationData::Text {
                    origin: point,
                    text,
                    style: TextStyle {
                        size: 20,
                        weight: TextWeight::Bold,
                        color: active_color(&color_combo_for_click),
                    },
                }));
            }
            canvas_for_click.queue_draw();
            return;
        }

        if document.select_at(point).is_some() {
            *draft_for_click.borrow_mut() = None;
        }
    });
    canvas.add_controller(click);

    let drag = gtk::GestureDrag::new();
    let doc_for_drag = document.clone();
    let draft_for_drag = draft.clone();
    let moving_for_drag = moving.clone();
    let history_for_drag = history.clone();
    let color_combo_for_drag = color_combo.clone();
    let stroke_combo_for_drag = stroke_combo.clone();
    drag.connect_drag_begin(move |_, x, y| {
        let point = Point::new(x as f32, y as f32);
        let mut document = doc_for_drag.borrow_mut();
        if document.select_at(point).is_some() {
            *moving_for_drag.borrow_mut() = Some(point);
            history_for_drag.borrow_mut().snapshot(&document.annotations);
            return;
        }

        history_for_drag.borrow_mut().snapshot(&document.annotations);
        let tool = document.active_tool;
        *draft_for_drag.borrow_mut() = Some(DraftAnnotation::new(
            tool,
            point,
            active_color(&color_combo_for_drag),
            active_stroke(&stroke_combo_for_drag),
        ));
    });

    let doc_for_update = document.clone();
    let draft_for_update = draft.clone();
    let moving_for_update = moving.clone();
    let canvas_for_update = canvas.clone();
    drag.connect_drag_update(move |gesture, dx, dy| {
        let (start_x, start_y) = gesture.start_point().unwrap_or((0.0, 0.0));
        let current = Point::new((start_x + dx) as f32, (start_y + dy) as f32);
        if let Some(previous) = *moving_for_update.borrow() {
            let delta_x = current.x - previous.x;
            let delta_y = current.y - previous.y;
            doc_for_update.borrow_mut().move_selected(delta_x, delta_y);
            *moving_for_update.borrow_mut() = Some(current);
            canvas_for_update.queue_draw();
            return;
        }

        if let Some(draft) = draft_for_update.borrow_mut().as_mut() {
            draft.extend(current);
            canvas_for_update.queue_draw();
        }
    });

    let doc_for_end = document.clone();
    let draft_for_end = draft.clone();
    let moving_for_end = moving.clone();
    let canvas_for_end = canvas.clone();
    drag.connect_drag_end(move |_, _, _| {
        *moving_for_end.borrow_mut() = None;
        if let Some(draft) = draft_for_end.borrow_mut().take() {
            if let Some(annotation) = draft.finish() {
                doc_for_end.borrow_mut().add_annotation(annotation);
            }
        }
        canvas_for_end.queue_draw();
    });
    canvas.add_controller(drag);

    let doc_for_undo = document.clone();
    let canvas_for_undo = canvas.clone();
    let history_for_undo = history.clone();
    undo.connect_clicked(move |_| {
        let current = doc_for_undo.borrow().annotations.clone();
        if let Some(previous) = history_for_undo.borrow_mut().undo(&current) {
            doc_for_undo.borrow_mut().annotations = previous;
            canvas_for_undo.queue_draw();
        }
    });

    let doc_for_redo = document.clone();
    let canvas_for_redo = canvas.clone();
    let history_for_redo = history.clone();
    redo.connect_clicked(move |_| {
        let current = doc_for_redo.borrow().annotations.clone();
        if let Some(next) = history_for_redo.borrow_mut().redo(&current) {
            doc_for_redo.borrow_mut().annotations = next;
            canvas_for_redo.queue_draw();
        }
    });

    let doc_for_save = document.clone();
    let state_for_save = state.clone();
    let image_for_save = base_image.clone();
    let path_for_save = image_path.clone();
    save.connect_clicked(move |_| {
        let config = state_for_save.config_snapshot();
        let annotations = doc_for_save.borrow().annotations.clone();
        if let Ok(output) =
            save_with_config(&config, &image_for_save, &annotations, chrono::Local::now())
        {
            if config.auto_copy {
                copy_image_to_clipboard(&output);
            }
        }
        info!("saved screenshot based on {}", path_for_save.display());
    });

    let doc_for_save_close = document.clone();
    let state_for_save_close = state.clone();
    let image_for_save_close = base_image.clone();
    let window_for_save_close = window.clone();
    let app_for_save_close = app.clone();
    save_close.connect_clicked(move |_| {
        let config = state_for_save_close.config_snapshot();
        let annotations = doc_for_save_close.borrow().annotations.clone();
        if let Ok(output) =
            save_with_config(&config, &image_for_save_close, &annotations, chrono::Local::now())
        {
            if config.auto_copy {
                copy_image_to_clipboard(&output);
            }
            if config.pin_after_save {
                present_pin_window(&app_for_save_close, output);
            }
            window_for_save_close.close();
        }
    });

    let doc_for_copy = document.clone();
    let image_for_copy = base_image.clone();
    copy.connect_clicked(move |_| {
        let temp = std::env::temp_dir().join("ashot_clipboard.png");
        let annotations = doc_for_copy.borrow().annotations.clone();
        if save_document_png(&image_for_copy, &annotations, &temp).is_ok() {
            copy_image_to_clipboard(&temp);
        }
    });

    let doc_for_pin = document.clone();
    let image_for_pin = base_image.clone();
    let app_for_pin = app.clone();
    pin.connect_clicked(move |_| {
        let temp = std::env::temp_dir().join("ashot_pin.png");
        let annotations = doc_for_pin.borrow().annotations.clone();
        if save_document_png(&image_for_pin, &annotations, &temp).is_ok() {
            present_pin_window(&app_for_pin, temp);
        }
    });

    root.append(&Label::new(Some(
        "Drag to draw. Click an existing annotation to move it. Text opens a small prompt dialog.",
    )));
    window.set_child(Some(&root));
    window.present();
    Ok(())
}

fn present_pin_window(app: &adw::Application, image_path: PathBuf) {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Pinned Screenshot")
        .default_width(480)
        .default_height(320)
        .build();
    let root = GtkBox::new(Orientation::Vertical, 8);
    root.set_margin_top(8);
    root.set_margin_bottom(8);
    root.set_margin_start(8);
    root.set_margin_end(8);

    let picture = Picture::for_filename(&image_path);
    picture.set_can_shrink(true);
    picture.set_hexpand(true);
    picture.set_vexpand(true);
    root.append(&picture);

    let zoom = gtk::Scale::with_range(Orientation::Horizontal, 0.25, 4.0, 0.05);
    zoom.set_value(1.0);
    let picture_for_zoom = picture.clone();
    zoom.connect_value_changed(move |scale| {
        let value = scale.value();
        picture_for_zoom.set_size_request((480.0 * value) as i32, (320.0 * value) as i32);
    });
    root.append(&zoom);

    window.set_child(Some(&root));
    window.present();
}

fn prompt_text(parent: &ApplicationWindow) -> String {
    let dialog =
        gtk::Dialog::builder().transient_for(parent).modal(true).title("Insert Text").build();
    dialog.add_button("Cancel", gtk::ResponseType::Cancel);
    dialog.add_button("Insert", gtk::ResponseType::Accept);
    let entry = Entry::new();
    entry.set_hexpand(true);
    dialog.content_area().append(&entry);
    let result = Rc::new(RefCell::new(String::new()));
    let result_for_response = result.clone();
    let entry_for_response = entry.clone();
    let loop_ = glib::MainLoop::new(None, false);
    let loop_for_response = loop_.clone();
    dialog.connect_response(move |dialog, response| {
        if response == gtk::ResponseType::Accept {
            *result_for_response.borrow_mut() = entry_for_response.text().to_string();
        }
        dialog.close();
        loop_for_response.quit();
    });
    dialog.present();
    loop_.run();
    result.borrow().clone()
}

fn active_color(combo: &ComboBoxText) -> Color {
    match combo.active_text().as_deref() {
        Some("Blue") => Color::rgba(37, 99, 235, 255),
        Some("Green") => Color::rgba(34, 197, 94, 255),
        Some("Yellow") => Color::rgba(234, 179, 8, 255),
        Some("Black") => Color::rgba(17, 24, 39, 255),
        _ => Color::rgba(232, 62, 38, 255),
    }
}

fn active_stroke(combo: &ComboBoxText) -> u32 {
    combo.active_text().and_then(|text| text.parse::<u32>().ok()).unwrap_or(4)
}

fn tool_index(tool: DefaultTool) -> u32 {
    match tool {
        DefaultTool::Text => 0,
        DefaultTool::Arrow => 1,
        DefaultTool::Brush => 2,
        DefaultTool::Rectangle => 3,
        DefaultTool::Mosaic => 4,
    }
}

#[derive(Debug, Clone)]
struct DraftAnnotation {
    tool: DefaultTool,
    start: Point,
    points: Vec<Point>,
    color: Color,
    stroke_width: u32,
}

impl DraftAnnotation {
    fn new(tool: DefaultTool, start: Point, color: Color, stroke_width: u32) -> Self {
        Self { tool, start, points: vec![start], color, stroke_width }
    }

    fn extend(&mut self, point: Point) {
        if self.tool == DefaultTool::Brush {
            self.points.push(point);
        } else {
            self.points.truncate(1);
            self.points.push(point);
        }
    }

    fn finish(self) -> Option<Annotation> {
        let end = *self.points.last().unwrap_or(&self.start);
        match self.tool {
            DefaultTool::Arrow => Some(Annotation::new(AnnotationData::Arrow {
                start: self.start,
                end,
                color: self.color,
                stroke_width: self.stroke_width,
            })),
            DefaultTool::Brush => Some(Annotation::new(AnnotationData::Brush {
                points: self.points,
                color: self.color,
                stroke_width: self.stroke_width,
            })),
            DefaultTool::Rectangle => Some(Annotation::new(AnnotationData::Rectangle {
                rect: Rect::from_points(self.start, end),
                color: self.color,
                stroke_width: self.stroke_width,
            })),
            DefaultTool::Mosaic => Some(Annotation::new(AnnotationData::Mosaic {
                rect: Rect::from_points(self.start, end),
                pixel_size: self.stroke_width.max(8),
            })),
            DefaultTool::Text => None,
        }
    }
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
        AnnotationData::Arrow { start, end, color, stroke_width } => {
            set_cairo_color(cr, *color);
            cr.set_line_width(*stroke_width as f64);
            cr.move_to(start.x as f64, start.y as f64);
            cr.line_to(end.x as f64, end.y as f64);
            let _ = cr.stroke();
        }
        AnnotationData::Brush { points, color, stroke_width } => {
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
        AnnotationData::Mosaic { rect, .. } => {
            cr.set_source_rgba(0.5, 0.5, 0.5, 0.35);
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
