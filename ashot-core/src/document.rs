use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type AnnotationId = Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum DefaultTool {
    Select,
    Text,
    Line,
    Arrow,
    Brush,
    Rectangle,
    Ellipse,
    Marker,
    Mosaic,
    Blur,
    Counter,
    FilledBox,
    ColorPicker,
    Ocr,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TextWeight {
    Regular,
    Semibold,
    Bold,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
}

pub const MARKER_HIGHLIGHT_ALPHA: u8 = 72;

pub fn marker_highlight_color(color: Color) -> Color {
    Color::rgba(color.r, color.g, color.b, color.a.min(MARKER_HIGHLIGHT_ALPHA))
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    pub fn offset(self, dx: f32, dy: f32) -> Self {
        Self { x: self.x + dx, y: self.y + dy }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn from_points(start: Point, end: Point) -> Self {
        let x = start.x.min(end.x);
        let y = start.y.min(end.y);
        let width = (start.x - end.x).abs();
        let height = (start.y - end.y).abs();
        Self { x, y, width, height }
    }

    pub fn contains(&self, point: Point) -> bool {
        point.x >= self.x
            && point.y >= self.y
            && point.x <= self.x + self.width
            && point.y <= self.y + self.height
    }

    pub fn translate(&mut self, dx: f32, dy: f32) {
        self.x += dx;
        self.y += dy;
    }

    pub fn resized(self, handle: ResizeHandle, point: Point) -> Self {
        let left = self.x;
        let top = self.y;
        let right = self.x + self.width;
        let bottom = self.y + self.height;

        let (new_left, new_top, new_right, new_bottom) = match handle {
            ResizeHandle::TopLeft => (point.x, point.y, right, bottom),
            ResizeHandle::Top => (left, point.y, right, bottom),
            ResizeHandle::TopRight => (left, point.y, point.x, bottom),
            ResizeHandle::Right => (left, top, point.x, bottom),
            ResizeHandle::BottomRight => (left, top, point.x, point.y),
            ResizeHandle::Bottom => (left, top, right, point.y),
            ResizeHandle::BottomLeft => (point.x, top, right, point.y),
            ResizeHandle::Left => (point.x, top, right, bottom),
        };

        Rect::from_points(Point::new(new_left, new_top), Point::new(new_right, new_bottom))
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ResizeHandle {
    TopLeft,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TextStyle {
    pub size: u32,
    pub weight: TextWeight,
    pub color: Color,
    #[serde(default)]
    pub family: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AnnotationData {
    Text { origin: Point, text: String, style: TextStyle },
    Line { start: Point, end: Point, color: Color, stroke_width: u32 },
    Arrow { start: Point, end: Point, color: Color, stroke_width: u32 },
    Brush { points: Vec<Point>, color: Color, stroke_width: u32 },
    Rectangle { rect: Rect, color: Color, stroke_width: u32 },
    Ellipse { rect: Rect, color: Color, stroke_width: u32 },
    Marker { points: Vec<Point>, color: Color, stroke_width: u32 },
    Mosaic { rect: Rect, pixel_size: u32 },
    Blur { rect: Rect, radius: u32 },
    Counter { center: Point, number: u32, color: Color, radius: u32 },
    FilledBox { rect: Rect, color: Color },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Annotation {
    pub id: AnnotationId,
    pub data: AnnotationData,
}

impl Annotation {
    pub fn new(data: AnnotationData) -> Self {
        Self { id: Uuid::new_v4(), data }
    }

    pub fn bounds(&self) -> Rect {
        match &self.data {
            AnnotationData::Text { origin, text, style } => {
                let width = text.chars().count() as f32 * style.size as f32 * 0.65;
                let height = style.size as f32 * 1.4;
                Rect { x: origin.x, y: origin.y, width, height }
            }
            AnnotationData::Line { start, end, .. } | AnnotationData::Arrow { start, end, .. } => {
                Rect::from_points(*start, *end)
            }
            AnnotationData::Brush { points, .. } | AnnotationData::Marker { points, .. } => {
                let mut min_x = f32::MAX;
                let mut min_y = f32::MAX;
                let mut max_x = f32::MIN;
                let mut max_y = f32::MIN;
                if points.is_empty() {
                    return Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 };
                }
                for point in points {
                    min_x = min_x.min(point.x);
                    min_y = min_y.min(point.y);
                    max_x = max_x.max(point.x);
                    max_y = max_y.max(point.y);
                }
                Rect { x: min_x, y: min_y, width: max_x - min_x, height: max_y - min_y }
            }
            AnnotationData::Rectangle { rect, .. }
            | AnnotationData::Ellipse { rect, .. }
            | AnnotationData::Mosaic { rect, .. }
            | AnnotationData::Blur { rect, .. }
            | AnnotationData::FilledBox { rect, .. } => *rect,
            AnnotationData::Counter { center, radius, .. } => {
                let diameter = *radius as f32 * 2.0;
                Rect {
                    x: center.x - *radius as f32,
                    y: center.y - *radius as f32,
                    width: diameter,
                    height: diameter,
                }
            }
        }
    }

    pub fn translate(&mut self, dx: f32, dy: f32) {
        match &mut self.data {
            AnnotationData::Text { origin, .. } => *origin = origin.offset(dx, dy),
            AnnotationData::Line { start, end, .. } | AnnotationData::Arrow { start, end, .. } => {
                *start = start.offset(dx, dy);
                *end = end.offset(dx, dy);
            }
            AnnotationData::Brush { points, .. } | AnnotationData::Marker { points, .. } => {
                for point in points {
                    *point = point.offset(dx, dy);
                }
            }
            AnnotationData::Rectangle { rect, .. }
            | AnnotationData::Ellipse { rect, .. }
            | AnnotationData::Mosaic { rect, .. }
            | AnnotationData::Blur { rect, .. }
            | AnnotationData::FilledBox { rect, .. } => {
                rect.translate(dx, dy);
            }
            AnnotationData::Counter { center, .. } => *center = center.offset(dx, dy),
        }
    }

    pub fn resize(&mut self, handle: ResizeHandle, point: Point) -> bool {
        match &mut self.data {
            AnnotationData::Line { start, end, .. } | AnnotationData::Arrow { start, end, .. } => {
                match handle {
                    ResizeHandle::TopLeft | ResizeHandle::Left | ResizeHandle::BottomLeft => {
                        *start = point
                    }
                    _ => *end = point,
                }
                true
            }
            AnnotationData::Rectangle { rect, .. }
            | AnnotationData::Ellipse { rect, .. }
            | AnnotationData::Mosaic { rect, .. }
            | AnnotationData::Blur { rect, .. }
            | AnnotationData::FilledBox { rect, .. } => {
                *rect = rect.resized(handle, point);
                true
            }
            AnnotationData::Counter { center, radius, .. } => {
                let dx = point.x - center.x;
                let dy = point.y - center.y;
                *radius = dx.abs().max(dy.abs()).round().max(1.0) as u32;
                true
            }
            AnnotationData::Text { .. }
            | AnnotationData::Brush { .. }
            | AnnotationData::Marker { .. } => false,
        }
    }

    pub fn apply_color(&mut self, color: Color) -> bool {
        match &mut self.data {
            AnnotationData::Text { style, .. } => style.color = color,
            AnnotationData::Line { color: current, .. }
            | AnnotationData::Arrow { color: current, .. }
            | AnnotationData::Brush { color: current, .. }
            | AnnotationData::Rectangle { color: current, .. }
            | AnnotationData::Ellipse { color: current, .. }
            | AnnotationData::Marker { color: current, .. }
            | AnnotationData::Counter { color: current, .. }
            | AnnotationData::FilledBox { color: current, .. } => *current = color,
            AnnotationData::Mosaic { .. } | AnnotationData::Blur { .. } => return false,
        }
        true
    }

    pub fn apply_stroke_width(&mut self, width: u32) -> bool {
        let width = width.max(1);
        match &mut self.data {
            AnnotationData::Line { stroke_width, .. }
            | AnnotationData::Arrow { stroke_width, .. }
            | AnnotationData::Brush { stroke_width, .. }
            | AnnotationData::Rectangle { stroke_width, .. }
            | AnnotationData::Ellipse { stroke_width, .. }
            | AnnotationData::Marker { stroke_width, .. } => *stroke_width = width,
            AnnotationData::Mosaic { pixel_size, .. } => *pixel_size = width,
            AnnotationData::Blur { radius, .. } => *radius = width,
            AnnotationData::Counter { radius, .. } => *radius = width,
            AnnotationData::Text { style, .. } => style.size = width,
            AnnotationData::FilledBox { .. } => return false,
        }
        true
    }

    pub fn apply_font_family(&mut self, family: Option<String>) -> bool {
        match &mut self.data {
            AnnotationData::Text { style, .. } => {
                style.family = family;
                true
            }
            _ => false,
        }
    }

    pub fn apply_text_size(&mut self, size: u32) -> bool {
        match &mut self.data {
            AnnotationData::Text { style, .. } => {
                style.size = size.max(1);
                true
            }
            _ => false,
        }
    }

    pub fn hit_test(&self, point: Point) -> bool {
        self.bounds().contains(point)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Document {
    pub image_width: u32,
    pub image_height: u32,
    pub annotations: Vec<Annotation>,
    pub selected: Option<AnnotationId>,
    pub active_tool: DefaultTool,
    pub zoom: f32,
}

impl Document {
    pub fn new(image_width: u32, image_height: u32, active_tool: DefaultTool) -> Self {
        Self {
            image_width,
            image_height,
            annotations: Vec::new(),
            selected: None,
            active_tool,
            zoom: 1.0,
        }
    }

    pub fn add_annotation(&mut self, annotation: Annotation) {
        self.selected = Some(annotation.id);
        self.annotations.push(annotation);
    }

    pub fn select_at(&mut self, point: Point) -> Option<AnnotationId> {
        let selected = self
            .annotations
            .iter()
            .rev()
            .find(|annotation| annotation.hit_test(point))
            .map(|annotation| annotation.id);
        self.selected = selected;
        selected
    }

    pub fn remove_selected(&mut self) -> Option<Annotation> {
        let id = self.selected?;
        let index = self.annotations.iter().position(|annotation| annotation.id == id)?;
        self.selected = None;
        Some(self.annotations.remove(index))
    }

    pub fn move_selected(&mut self, dx: f32, dy: f32) -> bool {
        let Some(id) = self.selected else {
            return false;
        };
        if let Some(annotation) = self.annotations.iter_mut().find(|annotation| annotation.id == id)
        {
            annotation.translate(dx, dy);
            return true;
        }
        false
    }

    pub fn duplicate_selected(&mut self, offset: Point) -> Option<AnnotationId> {
        let id = self.selected?;
        let mut duplicate = self.annotations.iter().find(|annotation| annotation.id == id)?.clone();
        duplicate.id = Uuid::new_v4();
        duplicate.translate(offset.x, offset.y);
        let duplicate_id = duplicate.id;
        self.annotations.push(duplicate);
        self.selected = Some(duplicate_id);
        Some(duplicate_id)
    }

    pub fn apply_color_to_selected(&mut self, color: Color) -> bool {
        let Some(id) = self.selected else {
            return false;
        };
        self.annotations
            .iter_mut()
            .find(|annotation| annotation.id == id)
            .is_some_and(|annotation| annotation.apply_color(color))
    }

    pub fn apply_stroke_to_selected(&mut self, width: u32) -> bool {
        let Some(id) = self.selected else {
            return false;
        };
        self.annotations
            .iter_mut()
            .find(|annotation| annotation.id == id)
            .is_some_and(|annotation| annotation.apply_stroke_width(width))
    }

    pub fn apply_font_family_to_selected(&mut self, family: Option<String>) -> bool {
        let Some(id) = self.selected else {
            return false;
        };
        self.annotations
            .iter_mut()
            .find(|annotation| annotation.id == id)
            .is_some_and(|annotation| annotation.apply_font_family(family))
    }

    pub fn apply_text_size_to_selected(&mut self, size: u32) -> bool {
        let Some(id) = self.selected else {
            return false;
        };
        self.annotations
            .iter_mut()
            .find(|annotation| annotation.id == id)
            .is_some_and(|annotation| annotation.apply_text_size(size))
    }

    pub fn text_annotation_at(&self, point: Point) -> Option<AnnotationId> {
        self.annotations.iter().rev().find_map(|annotation| match annotation.data {
            AnnotationData::Text { .. } if annotation.hit_test(point) => Some(annotation.id),
            _ => None,
        })
    }

    pub fn update_text_annotation(&mut self, id: AnnotationId, text: String) -> bool {
        if let Some(annotation) = self.annotations.iter_mut().find(|annotation| annotation.id == id)
            && let AnnotationData::Text { text: current, .. } = &mut annotation.data
        {
            *current = text;
            self.selected = Some(id);
            return true;
        }
        false
    }

    pub fn resize_selected(&mut self, handle: ResizeHandle, point: Point) -> bool {
        let Some(id) = self.selected else {
            return false;
        };
        if let Some(annotation) = self.annotations.iter_mut().find(|annotation| annotation.id == id)
        {
            return annotation.resize(handle, point);
        }
        false
    }

    pub fn next_counter(&self) -> u32 {
        self.annotations
            .iter()
            .filter_map(|annotation| match annotation.data {
                AnnotationData::Counter { number, .. } => Some(number),
                _ => None,
            })
            .max()
            .unwrap_or(0)
            + 1
    }

    pub fn set_zoom(&mut self, zoom: f32) {
        self.zoom = zoom.clamp(0.25, 4.0);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Annotation, AnnotationData, Color, DefaultTool, Document, Point, Rect, ResizeHandle,
        TextStyle, TextWeight,
    };

    #[test]
    fn flameshot_tools_have_bounds_and_translate() {
        let color = Color::rgba(255, 0, 0, 255);
        let mut annotations = vec![
            Annotation::new(AnnotationData::Line {
                start: Point::new(5.0, 6.0),
                end: Point::new(25.0, 26.0),
                color,
                stroke_width: 3,
            }),
            Annotation::new(AnnotationData::Ellipse {
                rect: Rect { x: 10.0, y: 12.0, width: 30.0, height: 20.0 },
                color,
                stroke_width: 4,
            }),
            Annotation::new(AnnotationData::Marker {
                points: vec![Point::new(3.0, 4.0), Point::new(18.0, 19.0)],
                color: Color::rgba(255, 255, 0, 96),
                stroke_width: 8,
            }),
            Annotation::new(AnnotationData::Counter {
                center: Point::new(40.0, 42.0),
                number: 7,
                color,
                radius: 12,
            }),
            Annotation::new(AnnotationData::FilledBox {
                rect: Rect { x: 50.0, y: 52.0, width: 14.0, height: 16.0 },
                color,
            }),
            Annotation::new(AnnotationData::Blur {
                rect: Rect { x: 70.0, y: 72.0, width: 18.0, height: 20.0 },
                radius: 5,
            }),
        ];

        for annotation in &mut annotations {
            let before = annotation.bounds();
            annotation.translate(10.0, 20.0);
            let after = annotation.bounds();
            assert_eq!(after.x, before.x + 10.0);
            assert_eq!(after.y, before.y + 20.0);
            assert!(
                annotation.hit_test(Point::new(
                    after.x + after.width / 2.0,
                    after.y + after.height / 2.0
                ))
            );
        }
    }

    #[test]
    fn selected_rect_like_annotation_can_resize() {
        let mut document = Document::new(200, 120, DefaultTool::Rectangle);
        let annotation = Annotation::new(AnnotationData::FilledBox {
            rect: Rect { x: 10.0, y: 20.0, width: 30.0, height: 40.0 },
            color: Color::rgba(0, 0, 0, 255),
        });
        let id = annotation.id;
        document.add_annotation(annotation);
        document.selected = Some(id);

        assert!(document.resize_selected(ResizeHandle::BottomRight, Point::new(80.0, 90.0)));

        let bounds = document.annotations[0].bounds();
        assert_eq!(bounds, Rect { x: 10.0, y: 20.0, width: 70.0, height: 70.0 });
    }

    #[test]
    fn counter_tool_tracks_next_number() {
        let mut document = Document::new(120, 80, DefaultTool::Counter);
        assert_eq!(document.next_counter(), 1);
        document.add_annotation(Annotation::new(AnnotationData::Counter {
            center: Point::new(20.0, 20.0),
            number: 1,
            color: Color::rgba(255, 0, 0, 255),
            radius: 10,
        }));
        document.add_annotation(Annotation::new(AnnotationData::Text {
            origin: Point::new(30.0, 30.0),
            text: "not a counter".into(),
            style: TextStyle {
                size: 16,
                weight: TextWeight::Regular,
                color: Color::rgba(255, 255, 255, 255),
                family: None,
            },
        }));

        assert_eq!(document.next_counter(), 2);
    }

    #[test]
    fn ocr_tool_is_available_without_annotation_data() {
        assert_eq!(format!("{:?}", DefaultTool::Ocr), "Ocr");
    }

    #[test]
    fn text_annotations_can_be_found_and_updated_in_place() {
        let mut document = Document::new(120, 80, DefaultTool::Text);
        let annotation = Annotation::new(AnnotationData::Text {
            origin: Point::new(20.0, 30.0),
            text: "old".into(),
            style: TextStyle {
                size: 16,
                weight: TextWeight::Regular,
                color: Color::rgba(255, 255, 255, 255),
                family: None,
            },
        });
        let id = annotation.id;
        document.add_annotation(annotation);

        assert_eq!(document.text_annotation_at(Point::new(22.0, 32.0)), Some(id));
        assert!(document.update_text_annotation(id, "new".into()));

        assert!(matches!(
            &document.annotations[0].data,
            AnnotationData::Text { text, .. } if text == "new"
        ));
    }

    #[test]
    fn selected_annotation_can_be_duplicated_with_offset() {
        let mut document = Document::new(120, 80, DefaultTool::Select);
        let annotation = Annotation::new(AnnotationData::Rectangle {
            rect: Rect { x: 10.0, y: 12.0, width: 20.0, height: 24.0 },
            color: Color::rgba(255, 0, 0, 255),
            stroke_width: 4,
        });
        let original_id = annotation.id;
        document.add_annotation(annotation);
        document.selected = Some(original_id);

        let duplicate_id =
            document.duplicate_selected(Point::new(8.0, 10.0)).expect("duplicate selected");

        assert_ne!(duplicate_id, original_id);
        assert_eq!(document.selected, Some(duplicate_id));
        assert_eq!(document.annotations.len(), 2);
        assert_eq!(
            document.annotations[1].bounds(),
            Rect { x: 18.0, y: 22.0, width: 20.0, height: 24.0 }
        );
    }

    #[test]
    fn selected_annotation_accepts_color_and_stroke_changes() {
        let mut document = Document::new(120, 80, DefaultTool::Select);
        let annotation = Annotation::new(AnnotationData::Arrow {
            start: Point::new(10.0, 10.0),
            end: Point::new(40.0, 40.0),
            color: Color::rgba(255, 0, 0, 255),
            stroke_width: 4,
        });
        let id = annotation.id;
        document.add_annotation(annotation);
        document.selected = Some(id);

        assert!(document.apply_color_to_selected(Color::rgba(0, 0, 255, 255)));
        assert!(document.apply_stroke_to_selected(8));

        assert!(matches!(
            document.annotations[0].data,
            AnnotationData::Arrow {
                color: Color { r: 0, g: 0, b: 255, a: 255 },
                stroke_width: 8,
                ..
            }
        ));
    }

    #[test]
    fn legacy_text_style_defaults_to_system_font() {
        let style: TextStyle = toml::from_str(
            r#"
size = 18
weight = "Regular"
color = { r = 255, g = 255, b = 255, a = 255 }
"#,
        )
        .expect("legacy text style");

        assert_eq!(style.family, None);
    }

    #[test]
    fn selected_text_annotation_accepts_font_family_changes() {
        let mut document = Document::new(120, 80, DefaultTool::Select);
        let annotation = Annotation::new(AnnotationData::Text {
            origin: Point::new(12.0, 14.0),
            text: "hello".into(),
            style: TextStyle {
                size: 18,
                weight: TextWeight::Regular,
                color: Color::rgba(255, 255, 255, 255),
                family: None,
            },
        });
        let id = annotation.id;
        document.add_annotation(annotation);
        document.selected = Some(id);

        assert!(document.apply_font_family_to_selected(Some("Noto Sans CJK SC".to_string())));
        assert!(matches!(
            &document.annotations[0].data,
            AnnotationData::Text { style, .. }
                if style.family.as_deref() == Some("Noto Sans CJK SC")
        ));

        assert!(document.apply_font_family_to_selected(None));
        assert!(matches!(
            &document.annotations[0].data,
            AnnotationData::Text { style, .. } if style.family.is_none()
        ));
    }

    #[test]
    fn selected_text_annotation_accepts_text_size_changes() {
        let mut document = Document::new(120, 80, DefaultTool::Select);
        let annotation = Annotation::new(AnnotationData::Text {
            origin: Point::new(12.0, 14.0),
            text: "hello".into(),
            style: TextStyle {
                size: 18,
                weight: TextWeight::Regular,
                color: Color::rgba(255, 255, 255, 255),
                family: None,
            },
        });
        let id = annotation.id;
        document.add_annotation(annotation);
        document.selected = Some(id);

        assert!(document.apply_text_size_to_selected(32));
        assert!(matches!(
            &document.annotations[0].data,
            AnnotationData::Text { style, .. } if style.size == 32
        ));
    }
}
