pub mod config;
pub mod document;
pub mod export;
pub mod filename;
pub mod history;
pub mod ocr;
pub mod workflow;

pub use config::{AppConfig, AppearanceMode, ConfigError};
pub use document::{
    Annotation, AnnotationData, AnnotationId, Color, DefaultTool, Document, MARKER_HIGHLIGHT_ALPHA,
    MARKER_MULTI_FIBER_MIN_WIDTH, MARKER_STROKE_MULTIPLIER, MarkerFiber, Point, Rect, ResizeHandle,
    TextStyle, TextWeight, marker_fiber_layout, marker_highlight_color, marker_visual_stroke_width,
};
pub use export::{draw_text_cairo, render_document, save_document_png};
pub use filename::render_filename;
pub use history::EditorHistory;
pub use ocr::{
    LinuxDistroFamily, OcrBackend, OcrLanguage, OcrLanguagePackages, default_ocr_filter_symbols,
    default_ocr_languages, default_ocr_space_engine, detect_linux_distro_family,
    language_install_command, language_package_for_distro, linux_distro_family_from_os_release,
    ocr_language_by_tesseract_code, search_ocr_languages,
};
pub use workflow::{SaveWorkflowError, finalize_capture_with_config, save_with_config};
