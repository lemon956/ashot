pub mod config;
pub mod document;
pub mod export;
pub mod filename;
pub mod history;
pub mod workflow;

pub use config::{AppConfig, ConfigError};
pub use document::{
    Annotation, AnnotationData, AnnotationId, Color, DefaultTool, Document, Point, Rect, TextStyle,
    TextWeight,
};
pub use export::{render_document, save_document_png};
pub use filename::render_filename;
pub use history::EditorHistory;
pub use workflow::{SaveWorkflowError, finalize_capture_with_config, save_with_config};
