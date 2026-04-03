use std::{
    fs,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Local};
use image::DynamicImage;
use thiserror::Error;

use crate::{Annotation, AppConfig, export::save_document_png, filename::render_filename};

#[derive(Debug, Error)]
pub enum SaveWorkflowError {
    #[error("failed to open captured image at {path}: {source}")]
    OpenImage {
        path: PathBuf,
        #[source]
        source: image::ImageError,
    },
    #[error("failed to create screenshot directory at {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to save screenshot at {path}: {source}")]
    Save {
        path: PathBuf,
        #[source]
        source: image::ImageError,
    },
}

pub fn finalize_capture_with_config(
    config: &AppConfig,
    source_image: impl AsRef<Path>,
    annotations: &[Annotation],
    now: DateTime<Local>,
) -> Result<PathBuf, SaveWorkflowError> {
    let source_image = source_image.as_ref();
    let base = image::open(source_image).map_err(|source| SaveWorkflowError::OpenImage {
        path: source_image.to_path_buf(),
        source,
    })?;
    save_with_config(config, &base, annotations, now)
}

pub fn save_with_config(
    config: &AppConfig,
    base: &DynamicImage,
    annotations: &[Annotation],
    now: DateTime<Local>,
) -> Result<PathBuf, SaveWorkflowError> {
    fs::create_dir_all(&config.default_save_dir).map_err(|source| {
        SaveWorkflowError::CreateDir { path: config.default_save_dir.clone(), source }
    })?;
    let filename = render_filename(&config.filename_template, now);
    let output = config.default_save_dir.join(filename);
    save_document_png(base, annotations, &output)
        .map_err(|source| SaveWorkflowError::Save { path: output.clone(), source })?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use chrono::{Local, TimeZone};
    use image::DynamicImage;
    use tempfile::tempdir;

    use crate::{Annotation, AnnotationData, AppConfig, Color, Point};

    use super::{finalize_capture_with_config, save_with_config};

    #[test]
    fn save_workflow_uses_filename_template() {
        let dir = tempdir().expect("tempdir");
        let config = AppConfig {
            default_save_dir: dir.path().join("shots"),
            filename_template: "Shot_%Y%m%d_%H%M%S".into(),
            ..AppConfig::default()
        };
        let base = DynamicImage::new_rgba8(16, 16);
        let annotations = vec![Annotation::new(AnnotationData::Arrow {
            start: Point::new(1.0, 1.0),
            end: Point::new(10.0, 10.0),
            color: Color::rgba(255, 0, 0, 255),
            stroke_width: 2,
        })];

        let now = Local.with_ymd_and_hms(2026, 4, 3, 11, 22, 33).unwrap();
        let output = save_with_config(&config, &base, &annotations, now).expect("save output");

        assert!(output.exists());
        assert_eq!(
            output.file_name().and_then(|name| name.to_str()),
            Some("Shot_20260403_112233.png")
        );
    }

    #[test]
    fn finalize_capture_loads_source_image_before_saving() {
        let dir = tempdir().expect("tempdir");
        let source_path = dir.path().join("capture.png");
        DynamicImage::new_rgba8(8, 8).save(&source_path).expect("save source");

        let config = AppConfig {
            default_save_dir: dir.path().join("shots"),
            filename_template: "Final_%Y%m%d_%H%M%S".into(),
            ..AppConfig::default()
        };

        let now = Local.with_ymd_and_hms(2026, 4, 3, 12, 34, 56).unwrap();
        let output =
            finalize_capture_with_config(&config, &source_path, &[], now).expect("finalized");

        assert!(output.exists());
        assert_eq!(
            output.file_name().and_then(|name| name.to_str()),
            Some("Final_20260403_123456.png")
        );
    }
}
