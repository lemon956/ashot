use std::{io::Cursor, path::Path};

use font8x8::{BASIC_FONTS, UnicodeFonts};
use image::{DynamicImage, ImageBuffer, Rgba, RgbaImage};

use crate::document::{Annotation, AnnotationData, Color, Point, Rect, TextWeight};

pub fn render_document(base: &DynamicImage, annotations: &[Annotation]) -> RgbaImage {
    render_document_from_rgba(&base.to_rgba8(), annotations)
}

pub fn render_document_from_rgba(base: &RgbaImage, annotations: &[Annotation]) -> RgbaImage {
    let mut canvas = base.clone();
    for annotation in annotations {
        render_annotation_into(&mut canvas, annotation);
    }
    canvas
}

pub fn render_annotation_into(canvas: &mut RgbaImage, annotation: &Annotation) {
    match &annotation.data {
        AnnotationData::Text { origin, text, style } => {
            draw_text(canvas, *origin, text, style.size, style.weight, style.color)
        }
        AnnotationData::Line { start, end, color, stroke_width } => {
            draw_thick_line(canvas, *start, *end, *color, *stroke_width)
        }
        AnnotationData::Arrow { start, end, color, stroke_width } => {
            draw_arrow(canvas, *start, *end, *color, *stroke_width)
        }
        AnnotationData::Brush { points, color, stroke_width } => {
            draw_brush(canvas, points, *color, *stroke_width)
        }
        AnnotationData::Rectangle { rect, color, stroke_width } => {
            draw_rectangle(canvas, *rect, *color, *stroke_width)
        }
        AnnotationData::Ellipse { rect, color, stroke_width } => {
            draw_ellipse(canvas, *rect, *color, *stroke_width)
        }
        AnnotationData::Marker { points, color, stroke_width } => {
            draw_brush(canvas, points, *color, *stroke_width)
        }
        AnnotationData::Mosaic { rect, pixel_size } => {
            pixelate_region(canvas, *rect, *pixel_size);
        }
        AnnotationData::Blur { rect, radius } => {
            blur_region(canvas, *rect, *radius);
        }
        AnnotationData::Counter { center, number, color, radius } => {
            draw_counter(canvas, *center, *number, *color, *radius);
        }
        AnnotationData::FilledBox { rect, color } => {
            fill_rect(canvas, *rect, *color);
        }
    }
}

pub fn encode_png_bytes(image: &RgbaImage) -> image::ImageResult<Vec<u8>> {
    let mut cursor = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image.clone()).write_to(&mut cursor, image::ImageFormat::Png)?;
    Ok(cursor.into_inner())
}

pub fn save_document_png(
    base: &DynamicImage,
    annotations: &[Annotation],
    path: impl AsRef<Path>,
) -> image::ImageResult<()> {
    let image = render_document(base, annotations);
    image.save(path)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl PixelRect {
    fn from_edges(
        image_width: u32,
        image_height: u32,
        left: f32,
        top: f32,
        right: f32,
        bottom: f32,
    ) -> Option<Self> {
        if image_width == 0 || image_height == 0 {
            return None;
        }
        let left = left.floor().max(0.0).min(image_width as f32) as u32;
        let top = top.floor().max(0.0).min(image_height as f32) as u32;
        let right = right.ceil().max(0.0).min(image_width as f32) as u32;
        let bottom = bottom.ceil().max(0.0).min(image_height as f32) as u32;
        if right <= left || bottom <= top {
            return None;
        }
        Some(Self { x: left, y: top, width: right - left, height: bottom - top })
    }

    fn union(self, other: Self) -> Self {
        let left = self.x.min(other.x);
        let top = self.y.min(other.y);
        let right = (self.x + self.width).max(other.x + other.width);
        let bottom = (self.y + self.height).max(other.y + other.height);
        Self { x: left, y: top, width: right - left, height: bottom - top }
    }

    fn intersects(self, other: Self) -> bool {
        self.x < other.x + other.width
            && self.x + self.width > other.x
            && self.y < other.y + other.height
            && self.y + self.height > other.y
    }

    fn area(self) -> u64 {
        self.width as u64 * self.height as u64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderUpdateKind {
    Noop,
    Append,
    Dirty(PixelRect),
    Full,
}

pub fn incremental_render_plan(
    image_width: u32,
    image_height: u32,
    old_annotations: &[Annotation],
    new_annotations: &[Annotation],
) -> RenderUpdateKind {
    if old_annotations == new_annotations {
        return RenderUpdateKind::Noop;
    }

    if new_annotations.len() == old_annotations.len() + 1
        && new_annotations.starts_with(old_annotations)
    {
        return RenderUpdateKind::Append;
    }

    if old_annotations.iter().chain(new_annotations).any(annotation_is_complex_effect) {
        return RenderUpdateKind::Full;
    }

    if old_annotations.len() == new_annotations.len()
        && old_annotations.iter().zip(new_annotations).any(|(old, new)| old.id != new.id)
    {
        return RenderUpdateKind::Full;
    }

    let mut dirty = None::<PixelRect>;
    for old in old_annotations {
        let changed =
            new_annotations.iter().find(|new| new.id == old.id).is_none_or(|new| new != old);
        if changed && let Some(bounds) = annotation_render_bounds(old, image_width, image_height) {
            dirty = Some(dirty.map_or(bounds, |current| current.union(bounds)));
        }
    }
    for new in new_annotations {
        let changed =
            old_annotations.iter().find(|old| old.id == new.id).is_none_or(|old| old != new);
        if changed && let Some(bounds) = annotation_render_bounds(new, image_width, image_height) {
            dirty = Some(dirty.map_or(bounds, |current| current.union(bounds)));
        }
    }

    let Some(dirty) = dirty else {
        return RenderUpdateKind::Full;
    };
    let image_area = image_width as u64 * image_height as u64;
    if image_area == 0 || dirty.area() * 100 > image_area * 45 {
        RenderUpdateKind::Full
    } else {
        RenderUpdateKind::Dirty(dirty)
    }
}

pub fn update_rendered_image(
    base: &RgbaImage,
    cached: &mut RgbaImage,
    old_annotations: &[Annotation],
    new_annotations: &[Annotation],
) -> RenderUpdateKind {
    let plan =
        incremental_render_plan(base.width(), base.height(), old_annotations, new_annotations);
    match plan {
        RenderUpdateKind::Noop => {}
        RenderUpdateKind::Append => {
            if let Some(annotation) = new_annotations.last() {
                render_annotation_into(cached, annotation);
            }
        }
        RenderUpdateKind::Dirty(rect) => rebuild_dirty_region(base, cached, new_annotations, rect),
        RenderUpdateKind::Full => {
            *cached = render_document_from_rgba(base, new_annotations);
        }
    }
    plan
}

fn rebuild_dirty_region(
    base: &RgbaImage,
    cached: &mut RgbaImage,
    annotations: &[Annotation],
    dirty: PixelRect,
) {
    let mut tile = ImageBuffer::new(dirty.width, dirty.height);
    for y in 0..dirty.height {
        for x in 0..dirty.width {
            tile.put_pixel(x, y, *base.get_pixel(dirty.x + x, dirty.y + y));
        }
    }

    for annotation in annotations {
        let Some(bounds) = annotation_render_bounds(annotation, base.width(), base.height()) else {
            continue;
        };
        if !bounds.intersects(dirty) {
            continue;
        }
        let mut shifted = annotation.clone();
        shifted.translate(-(dirty.x as f32), -(dirty.y as f32));
        render_annotation_into(&mut tile, &shifted);
    }

    for y in 0..dirty.height {
        for x in 0..dirty.width {
            cached.put_pixel(dirty.x + x, dirty.y + y, *tile.get_pixel(x, y));
        }
    }
}

fn annotation_render_bounds(
    annotation: &Annotation,
    image_width: u32,
    image_height: u32,
) -> Option<PixelRect> {
    let bounds = annotation.bounds();
    let padding = annotation_render_padding(annotation);
    PixelRect::from_edges(
        image_width,
        image_height,
        bounds.x - padding,
        bounds.y - padding,
        bounds.x + bounds.width + padding,
        bounds.y + bounds.height + padding,
    )
}

fn annotation_render_padding(annotation: &Annotation) -> f32 {
    match &annotation.data {
        AnnotationData::Line { stroke_width, .. }
        | AnnotationData::Brush { stroke_width, .. }
        | AnnotationData::Rectangle { stroke_width, .. }
        | AnnotationData::Ellipse { stroke_width, .. }
        | AnnotationData::Marker { stroke_width, .. } => *stroke_width as f32 + 3.0,
        AnnotationData::Arrow { stroke_width, .. } => {
            let visual = arrow_visual_stroke_width(*stroke_width);
            let (_, head_width) = arrow_head_dimensions(visual);
            head_width + 4.0
        }
        AnnotationData::Text { style, .. } => style.size as f32 * 0.25 + 3.0,
        AnnotationData::Counter { radius, .. } => *radius as f32 * 0.2 + 3.0,
        AnnotationData::Mosaic { .. }
        | AnnotationData::Blur { .. }
        | AnnotationData::FilledBox { .. } => 1.0,
    }
}

fn annotation_is_complex_effect(annotation: &Annotation) -> bool {
    matches!(annotation.data, AnnotationData::Mosaic { .. } | AnnotationData::Blur { .. })
}

fn color_to_rgba(color: Color) -> Rgba<u8> {
    Rgba([color.r, color.g, color.b, color.a])
}

fn blend_pixel(target: &mut Rgba<u8>, source: Rgba<u8>) {
    let alpha = source[3] as f32 / 255.0;
    let inv = 1.0 - alpha;
    for idx in 0..3 {
        target[idx] = ((target[idx] as f32 * inv) + (source[idx] as f32 * alpha)) as u8;
    }
    target[3] = 255;
}

fn paint_circle(image: &mut RgbaImage, center_x: i32, center_y: i32, radius: i32, color: Rgba<u8>) {
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            if dx * dx + dy * dy > radius * radius {
                continue;
            }
            let x = center_x + dx;
            let y = center_y + dy;
            if x < 0 || y < 0 {
                continue;
            }
            let Some(pixel) = image.get_pixel_mut_checked(x as u32, y as u32) else {
                continue;
            };
            blend_pixel(pixel, color);
        }
    }
}

fn draw_thick_line(
    image: &mut RgbaImage,
    start: Point,
    end: Point,
    color: Color,
    stroke_width: u32,
) {
    let rgba = color_to_rgba(color);
    let radius = (stroke_width.max(1) as i32) / 2;
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    let steps = dx.abs().max(dy.abs()).ceil() as i32;
    if steps == 0 {
        paint_circle(image, start.x.round() as i32, start.y.round() as i32, radius, rgba);
        return;
    }

    for step in 0..=steps {
        let t = step as f32 / steps as f32;
        let x = start.x + dx * t;
        let y = start.y + dy * t;
        paint_circle(image, x.round() as i32, y.round() as i32, radius, rgba);
    }
}

fn draw_arrow(image: &mut RgbaImage, start: Point, end: Point, color: Color, stroke_width: u32) {
    let visual_stroke_width = arrow_visual_stroke_width(stroke_width);
    let shape = arrow_shape_geometry(start, end, visual_stroke_width);
    let rgba = color_to_rgba(color);
    fill_polygon(
        image,
        &[
            shape.tail_left,
            shape.body_left,
            shape.head_left,
            shape.tip,
            shape.head_right,
            shape.body_right,
            shape.tail_right,
        ],
        rgba,
    );

    let tip_radius = ((visual_stroke_width as f32) * 0.22).round().clamp(1.0, 4.0) as i32;
    let corner_radius = ((visual_stroke_width as f32) * 0.32).round().clamp(2.0, 6.0) as i32;
    paint_circle(image, shape.tip.x.round() as i32, shape.tip.y.round() as i32, tip_radius, rgba);
    paint_circle(
        image,
        shape.head_left.x.round() as i32,
        shape.head_left.y.round() as i32,
        corner_radius,
        rgba,
    );
    paint_circle(
        image,
        shape.head_right.x.round() as i32,
        shape.head_right.y.round() as i32,
        corner_radius,
        rgba,
    );
    paint_circle(
        image,
        start.x.round() as i32,
        start.y.round() as i32,
        ((visual_stroke_width as f32) * 0.24).round().clamp(2.0, 5.0) as i32,
        rgba,
    );
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

fn fill_polygon(image: &mut RgbaImage, points: &[Point], color: Rgba<u8>) {
    if points.len() < 3 {
        return;
    }

    let min_x =
        points.iter().map(|point| point.x).fold(f32::INFINITY, f32::min).floor().max(0.0) as u32;
    let min_y =
        points.iter().map(|point| point.y).fold(f32::INFINITY, f32::min).floor().max(0.0) as u32;
    let max_x = points
        .iter()
        .map(|point| point.x)
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil()
        .min(image.width().saturating_sub(1) as f32) as u32;
    let max_y = points
        .iter()
        .map(|point| point.y)
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil()
        .min(image.height().saturating_sub(1) as f32) as u32;

    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let point = Point::new(x as f32 + 0.5, y as f32 + 0.5);
            if !point_in_polygon(point, points) {
                continue;
            }
            if let Some(pixel) = image.get_pixel_mut_checked(x, y) {
                blend_pixel(pixel, color);
            }
        }
    }
}

fn point_in_polygon(point: Point, points: &[Point]) -> bool {
    let mut inside = false;
    let mut previous = points[points.len() - 1];
    for current in points {
        let crosses_y = (current.y > point.y) != (previous.y > point.y);
        if crosses_y {
            let intersection_x = (previous.x - current.x) * (point.y - current.y)
                / (previous.y - current.y)
                + current.x;
            if point.x < intersection_x {
                inside = !inside;
            }
        }
        previous = *current;
    }
    inside
}

fn draw_brush(image: &mut RgbaImage, points: &[Point], color: Color, stroke_width: u32) {
    if points.len() < 2 {
        return;
    }
    for window in points.windows(2) {
        draw_thick_line(image, window[0], window[1], color, stroke_width);
    }
}

fn draw_rectangle(image: &mut RgbaImage, rect: Rect, color: Color, stroke_width: u32) {
    let x1 = rect.x;
    let y1 = rect.y;
    let x2 = rect.x + rect.width;
    let y2 = rect.y + rect.height;
    draw_thick_line(image, Point::new(x1, y1), Point::new(x2, y1), color, stroke_width);
    draw_thick_line(image, Point::new(x2, y1), Point::new(x2, y2), color, stroke_width);
    draw_thick_line(image, Point::new(x2, y2), Point::new(x1, y2), color, stroke_width);
    draw_thick_line(image, Point::new(x1, y2), Point::new(x1, y1), color, stroke_width);
}

fn fill_rect(image: &mut RgbaImage, rect: Rect, color: Color) {
    let rgba = color_to_rgba(color);
    let start_x = rect.x.max(0.0).floor() as u32;
    let start_y = rect.y.max(0.0).floor() as u32;
    let end_x = (rect.x + rect.width).min(image.width() as f32).ceil() as u32;
    let end_y = (rect.y + rect.height).min(image.height() as f32).ceil() as u32;
    for y in start_y..end_y {
        for x in start_x..end_x {
            let Some(pixel) = image.get_pixel_mut_checked(x, y) else {
                continue;
            };
            blend_pixel(pixel, rgba);
        }
    }
}

fn draw_ellipse(image: &mut RgbaImage, rect: Rect, color: Color, stroke_width: u32) {
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return;
    }
    let center_x = rect.x + rect.width / 2.0;
    let center_y = rect.y + rect.height / 2.0;
    let radius_x = (rect.width / 2.0).max(1.0);
    let radius_y = (rect.height / 2.0).max(1.0);
    let samples = ((rect.width + rect.height) * 2.0).ceil().max(24.0) as i32;
    let mut previous = Point::new(center_x + radius_x, center_y);
    for index in 1..=samples {
        let angle = index as f32 / samples as f32 * std::f32::consts::TAU;
        let point =
            Point::new(center_x + radius_x * angle.cos(), center_y + radius_y * angle.sin());
        draw_thick_line(image, previous, point, color, stroke_width);
        previous = point;
    }
}

fn draw_counter(image: &mut RgbaImage, center: Point, number: u32, color: Color, radius: u32) {
    let radius = radius.max(4) as i32;
    paint_circle(
        image,
        center.x.round() as i32,
        center.y.round() as i32,
        radius,
        color_to_rgba(color),
    );
    let text = number.to_string();
    let text_color = Color::rgba(255, 255, 255, 255);
    let origin = Point::new(center.x - text.chars().count() as f32 * 4.0, center.y - 5.0);
    draw_text(image, origin, &text, 8, TextWeight::Bold, text_color);
}

fn draw_text(
    image: &mut RgbaImage,
    origin: Point,
    text: &str,
    size: u32,
    weight: TextWeight,
    color: Color,
) {
    let scale = (size.max(8) / 8) as i32;
    let mut x = origin.x.round() as i32;
    let mut y = origin.y.round() as i32;
    let rgba = color_to_rgba(color);
    let passes = match weight {
        TextWeight::Regular => 1,
        TextWeight::Semibold => 2,
        TextWeight::Bold => 3,
    };

    for ch in text.chars() {
        if ch == '\n' {
            y += scale * 12;
            x = origin.x.round() as i32;
            continue;
        }
        if let Some(glyph) = BASIC_FONTS.get(ch) {
            for pass in 0..passes {
                for (row_idx, row) in glyph.iter().enumerate() {
                    for col_idx in 0..8 {
                        if (row >> col_idx) & 1 != 1 {
                            continue;
                        }
                        let base_x = x + (7 - col_idx) * scale + pass;
                        let base_y = y + row_idx as i32 * scale;
                        for sy in 0..scale {
                            for sx in 0..scale {
                                let px = base_x + sx;
                                let py = base_y + sy;
                                if px < 0 || py < 0 {
                                    continue;
                                }
                                let Some(pixel) = image.get_pixel_mut_checked(px as u32, py as u32)
                                else {
                                    continue;
                                };
                                blend_pixel(pixel, rgba);
                            }
                        }
                    }
                }
            }
        }
        x += scale * 9;
    }
}

fn pixelate_region(image: &mut RgbaImage, rect: Rect, pixel_size: u32) {
    let block = pixel_size.max(1);
    let start_x = rect.x.max(0.0).floor() as u32;
    let start_y = rect.y.max(0.0).floor() as u32;
    let end_x = (rect.x + rect.width).min(image.width() as f32).ceil() as u32;
    let end_y = (rect.y + rect.height).min(image.height() as f32).ceil() as u32;

    let mut output: ImageBuffer<Rgba<u8>, Vec<u8>> = image.clone();
    let mut x = start_x;
    while x < end_x {
        let mut y = start_y;
        while y < end_y {
            let x_limit = (x + block).min(end_x);
            let y_limit = (y + block).min(end_y);
            let mut total = [0u32; 4];
            let mut count = 0u32;
            for yy in y..y_limit {
                for xx in x..x_limit {
                    let pixel = image.get_pixel(xx, yy);
                    total[0] += pixel[0] as u32;
                    total[1] += pixel[1] as u32;
                    total[2] += pixel[2] as u32;
                    total[3] += pixel[3] as u32;
                    count += 1;
                }
            }
            if count > 0 {
                let average = Rgba([
                    (total[0] / count).min(u8::MAX as u32) as u8,
                    (total[1] / count).min(u8::MAX as u32) as u8,
                    (total[2] / count).min(u8::MAX as u32) as u8,
                    (total[3] / count).min(u8::MAX as u32) as u8,
                ]);
                for yy in y..y_limit {
                    for xx in x..x_limit {
                        output.put_pixel(xx, yy, average);
                    }
                }
            }
            y += block;
        }
        x += block;
    }

    *image = output;
}

fn blur_region(image: &mut RgbaImage, rect: Rect, radius: u32) {
    let radius = radius.max(1) as i32;
    let start_x = rect.x.max(0.0).floor() as u32;
    let start_y = rect.y.max(0.0).floor() as u32;
    let end_x = (rect.x + rect.width).min(image.width() as f32).ceil() as u32;
    let end_y = (rect.y + rect.height).min(image.height() as f32).ceil() as u32;
    let mut output = image.clone();

    for y in start_y..end_y {
        for x in start_x..end_x {
            let mut total = [0u32; 4];
            let mut count = 0u32;
            for dy in -radius..=radius {
                for dx in -radius..=radius {
                    let xx = x as i32 + dx;
                    let yy = y as i32 + dy;
                    if xx < start_x as i32
                        || yy < start_y as i32
                        || xx >= end_x as i32
                        || yy >= end_y as i32
                    {
                        continue;
                    }
                    let pixel = image.get_pixel(xx as u32, yy as u32);
                    total[0] += pixel[0] as u32;
                    total[1] += pixel[1] as u32;
                    total[2] += pixel[2] as u32;
                    total[3] += pixel[3] as u32;
                    count += 1;
                }
            }
            if count > 0 {
                output.put_pixel(
                    x,
                    y,
                    Rgba([
                        (total[0] / count).min(u8::MAX as u32) as u8,
                        (total[1] / count).min(u8::MAX as u32) as u8,
                        (total[2] / count).min(u8::MAX as u32) as u8,
                        (total[3] / count).min(u8::MAX as u32) as u8,
                    ]),
                );
            }
        }
    }

    *image = output;
}

#[cfg(test)]
mod tests {
    use image::{DynamicImage, Rgba};

    use crate::document::{Annotation, AnnotationData, Color, Point, Rect, TextStyle, TextWeight};

    use super::{
        RenderUpdateKind, arrow_head_geometry, arrow_shape_geometry, arrow_visual_stroke_width,
        encode_png_bytes, incremental_render_plan, render_document, render_document_from_rgba,
        update_rendered_image,
    };

    #[test]
    fn export_renders_shapes() {
        let base = DynamicImage::new_rgba8(64, 64);
        let annotations = vec![
            Annotation::new(AnnotationData::Rectangle {
                rect: Rect { x: 5.0, y: 5.0, width: 20.0, height: 20.0 },
                color: Color::rgba(255, 0, 0, 255),
                stroke_width: 4,
            }),
            Annotation::new(AnnotationData::Arrow {
                start: Point::new(0.0, 0.0),
                end: Point::new(30.0, 30.0),
                color: Color::rgba(0, 255, 0, 255),
                stroke_width: 3,
            }),
            Annotation::new(AnnotationData::Text {
                origin: Point::new(10.0, 40.0),
                text: "Hi".into(),
                style: TextStyle {
                    size: 16,
                    weight: TextWeight::Bold,
                    color: Color::rgba(0, 0, 255, 255),
                },
            }),
        ];

        let rendered = render_document(&base, &annotations);
        assert!(rendered.pixels().any(|pixel| *pixel != Rgba([0, 0, 0, 0])));
    }

    #[test]
    fn export_arrow_uses_filled_head_geometry() {
        let start = Point::new(8.0, 20.0);
        let end = Point::new(56.0, 20.0);
        let (base, left, right) = arrow_head_geometry(start, end, 6);

        assert!(base.x < end.x);
        assert!(base.x > start.x);
        assert!(left.x < end.x);
        assert!(right.x < end.x);
        assert!((left.y - right.y).abs() >= 31.0);
        assert_ne!(left.y, right.y);
    }

    #[test]
    fn export_arrow_uses_tapered_body_instead_of_straight_line() {
        let start = Point::new(8.0, 20.0);
        let end = Point::new(56.0, 20.0);
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
    fn export_arrow_handles_very_short_drag_distance() {
        let shape = arrow_shape_geometry(Point::new(10.0, 10.0), Point::new(12.0, 10.0), 6);

        assert_eq!(shape.tip, Point::new(12.0, 10.0));
    }

    #[test]
    fn export_arrow_uses_bolder_visual_width_than_plain_line() {
        assert_eq!(arrow_visual_stroke_width(2), 6);
        assert_eq!(arrow_visual_stroke_width(4), 7);
        assert!(arrow_visual_stroke_width(12) > 12);
    }

    #[test]
    fn export_pixelates_region() {
        let mut base = DynamicImage::new_rgba8(8, 8).to_rgba8();
        for y in 0..8 {
            for x in 0..8 {
                base.put_pixel(x, y, Rgba([x as u8 * 30, y as u8 * 30, 0, 255]));
            }
        }
        let base = DynamicImage::ImageRgba8(base);
        let annotations = vec![Annotation::new(AnnotationData::Mosaic {
            rect: Rect { x: 0.0, y: 0.0, width: 8.0, height: 8.0 },
            pixel_size: 8,
        })];

        let rendered = render_document(&base, &annotations);
        let reference = *rendered.get_pixel(0, 0);
        assert_eq!(reference, *rendered.get_pixel(7, 7));
    }

    #[test]
    fn export_renders_flameshot_style_tools() {
        let base = DynamicImage::new_rgba8(96, 96);
        let annotations = vec![
            Annotation::new(AnnotationData::Line {
                start: Point::new(2.0, 2.0),
                end: Point::new(32.0, 2.0),
                color: Color::rgba(255, 0, 0, 255),
                stroke_width: 3,
            }),
            Annotation::new(AnnotationData::Ellipse {
                rect: Rect { x: 8.0, y: 12.0, width: 22.0, height: 18.0 },
                color: Color::rgba(0, 255, 0, 255),
                stroke_width: 3,
            }),
            Annotation::new(AnnotationData::Marker {
                points: vec![Point::new(4.0, 44.0), Point::new(42.0, 44.0)],
                color: Color::rgba(255, 255, 0, 96),
                stroke_width: 9,
            }),
            Annotation::new(AnnotationData::Counter {
                center: Point::new(60.0, 20.0),
                number: 3,
                color: Color::rgba(255, 0, 0, 255),
                radius: 10,
            }),
            Annotation::new(AnnotationData::FilledBox {
                rect: Rect { x: 58.0, y: 44.0, width: 18.0, height: 12.0 },
                color: Color::rgba(0, 0, 255, 255),
            }),
        ];

        let rendered = render_document(&base, &annotations);

        assert!(rendered.pixels().filter(|pixel| **pixel != Rgba([0, 0, 0, 0])).count() > 40);
    }

    #[test]
    fn export_blurs_region() {
        let mut base = DynamicImage::new_rgba8(9, 3).to_rgba8();
        for x in 0..9 {
            base.put_pixel(x, 1, Rgba([x as u8 * 25, 0, 0, 255]));
        }
        let base = DynamicImage::ImageRgba8(base);
        let annotations = vec![Annotation::new(AnnotationData::Blur {
            rect: Rect { x: 0.0, y: 0.0, width: 9.0, height: 3.0 },
            radius: 1,
        })];

        let rendered = render_document(&base, &annotations);

        assert_ne!(*rendered.get_pixel(4, 1), Rgba([100, 0, 0, 255]));
    }

    #[test]
    fn incremental_append_matches_full_render_and_encodes_png() {
        let base = DynamicImage::new_rgba8(64, 64).to_rgba8();
        let first = vec![Annotation::new(AnnotationData::Rectangle {
            rect: Rect { x: 8.0, y: 8.0, width: 16.0, height: 14.0 },
            color: Color::rgba(255, 0, 0, 255),
            stroke_width: 3,
        })];
        let mut second = first.clone();
        second.push(Annotation::new(AnnotationData::Arrow {
            start: Point::new(4.0, 52.0),
            end: Point::new(48.0, 18.0),
            color: Color::rgba(0, 200, 255, 255),
            stroke_width: 4,
        }));

        let mut cached = render_document_from_rgba(&base, &first);
        let update = update_rendered_image(&base, &mut cached, &first, &second);
        let full = render_document_from_rgba(&base, &second);
        let bytes = encode_png_bytes(&cached).expect("png bytes");

        assert_eq!(update, RenderUpdateKind::Append);
        assert_eq!(cached, full);
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn dirty_region_delete_matches_full_render() {
        let base = DynamicImage::new_rgba8(64, 64).to_rgba8();
        let keep = Annotation::new(AnnotationData::FilledBox {
            rect: Rect { x: 2.0, y: 2.0, width: 18.0, height: 16.0 },
            color: Color::rgba(255, 0, 0, 255),
        });
        let remove = Annotation::new(AnnotationData::Line {
            start: Point::new(5.0, 40.0),
            end: Point::new(50.0, 40.0),
            color: Color::rgba(0, 255, 0, 255),
            stroke_width: 5,
        });
        let old = vec![keep.clone(), remove];
        let new = vec![keep];

        let mut cached = render_document_from_rgba(&base, &old);
        let update = update_rendered_image(&base, &mut cached, &old, &new);
        let full = render_document_from_rgba(&base, &new);

        assert!(matches!(update, RenderUpdateKind::Dirty(_)));
        assert_eq!(cached, full);
    }

    #[test]
    fn dirty_region_with_complex_effect_falls_back_to_full_rebuild() {
        let old = vec![Annotation::new(AnnotationData::Blur {
            rect: Rect { x: 4.0, y: 4.0, width: 24.0, height: 24.0 },
            radius: 3,
        })];
        let new = Vec::new();

        assert_eq!(incremental_render_plan(64, 64, &old, &new), RenderUpdateKind::Full);
    }
}
