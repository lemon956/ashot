use chrono::{DateTime, Local};

pub fn render_filename(template: &str, now: DateTime<Local>) -> String {
    let rendered = now.format(template).to_string();
    if rendered.ends_with(".png") { rendered } else { format!("{rendered}.png") }
}

#[cfg(test)]
mod tests {
    use chrono::{Local, TimeZone};

    use super::render_filename;

    #[test]
    fn appends_png_when_missing() {
        let now = Local.with_ymd_and_hms(2026, 4, 3, 10, 11, 12).unwrap();
        let file = render_filename("Screenshot_%Y%m%d", now);
        assert_eq!(file, "Screenshot_20260403.png");
    }
}
