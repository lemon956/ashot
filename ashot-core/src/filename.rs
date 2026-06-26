use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};

pub fn render_filename(template: &str, now: DateTime<Local>) -> String {
    let rendered = now.format(template).to_string();
    if rendered.ends_with(".png") { rendered } else { format!("{rendered}.png") }
}

/// Returns `path` unchanged if nothing exists there, otherwise the first free
/// variant with a ` (n)` suffix inserted before the extension
/// (e.g. `Shot.png` -> `Shot (1).png`). This keeps a save from silently
/// overwriting an existing screenshot.
pub fn deduplicated_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let parent = path.parent();
    let stem = path.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
    let extension = path.extension().map(|extension| extension.to_string_lossy().into_owned());
    for counter in 1u32.. {
        let candidate_name = match &extension {
            Some(extension) => format!("{stem} ({counter}).{extension}"),
            None => format!("{stem} ({counter})"),
        };
        let candidate = match parent {
            Some(parent) => parent.join(candidate_name),
            None => PathBuf::from(candidate_name),
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    // Unreachable for any real filesystem; keeps the function total.
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use chrono::{Local, TimeZone};
    use tempfile::tempdir;

    use super::{deduplicated_path, render_filename};

    #[test]
    fn appends_png_when_missing() {
        let now = Local.with_ymd_and_hms(2026, 4, 3, 10, 11, 12).unwrap();
        let file = render_filename("Screenshot_%Y%m%d", now);
        assert_eq!(file, "Screenshot_20260403.png");
    }

    #[test]
    fn deduplicated_path_returns_input_when_free() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("Shot.png");
        assert_eq!(deduplicated_path(&path), path);
    }

    #[test]
    fn deduplicated_path_inserts_counter_before_extension() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("Shot.png");
        std::fs::write(&path, b"x").expect("write");
        assert_eq!(deduplicated_path(&path), dir.path().join("Shot (1).png"));

        std::fs::write(dir.path().join("Shot (1).png"), b"x").expect("write");
        assert_eq!(deduplicated_path(&path), dir.path().join("Shot (2).png"));
    }
}
