use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type AnnotationId = Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum DefaultTool {
    Text,
    Arrow,
    Brush,
    Rectangle,
    Mosaic,
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TextStyle {
    pub size: u32,
    pub weight: TextWeight,
    pub color: Color,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AnnotationData {
    Text { origin: Point, text: String, style: TextStyle },
    Arrow { start: Point, end: Point, color: Color, stroke_width: u32 },
    Brush { points: Vec<Point>, color: Color, stroke_width: u32 },
    Rectangle { rect: Rect, color: Color, stroke_width: u32 },
    Mosaic { rect: Rect, pixel_size: u32 },
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
            AnnotationData::Arrow { start, end, .. } => Rect::from_points(*start, *end),
            AnnotationData::Brush { points, .. } => {
                let mut min_x = f32::MAX;
                let mut min_y = f32::MAX;
                let mut max_x = f32::MIN;
                let mut max_y = f32::MIN;
                for point in points {
                    min_x = min_x.min(point.x);
                    min_y = min_y.min(point.y);
                    max_x = max_x.max(point.x);
                    max_y = max_y.max(point.y);
                }
                Rect { x: min_x, y: min_y, width: max_x - min_x, height: max_y - min_y }
            }
            AnnotationData::Rectangle { rect, .. } | AnnotationData::Mosaic { rect, .. } => *rect,
        }
    }

    pub fn translate(&mut self, dx: f32, dy: f32) {
        match &mut self.data {
            AnnotationData::Text { origin, .. } => *origin = origin.offset(dx, dy),
            AnnotationData::Arrow { start, end, .. } => {
                *start = start.offset(dx, dy);
                *end = end.offset(dx, dy);
            }
            AnnotationData::Brush { points, .. } => {
                for point in points {
                    *point = point.offset(dx, dy);
                }
            }
            AnnotationData::Rectangle { rect, .. } | AnnotationData::Mosaic { rect, .. } => {
                rect.translate(dx, dy);
            }
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

    pub fn set_zoom(&mut self, zoom: f32) {
        self.zoom = zoom.clamp(0.25, 4.0);
    }
}
