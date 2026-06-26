use std::{io::Cursor, path::Path};

use font8x8::{BASIC_FONTS, UnicodeFonts};
use image::{DynamicImage, ImageBuffer, Rgba, RgbaImage};
use pango::glib::translate::{ToGlibPtr, from_glib_full};

use crate::document::{
    Annotation, AnnotationData, Color, MarkerFiber, Point, Rect, TextStyle, TextWeight,
    marker_fiber_layout,
};

#[link(name = "pangocairo-1.0")]
unsafe extern "C" {
    fn pango_cairo_create_layout(cr: *mut cairo::ffi::cairo_t) -> *mut pango::ffi::PangoLayout;
    fn pango_cairo_show_layout(cr: *mut cairo::ffi::cairo_t, layout: *mut pango::ffi::PangoLayout);
}

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
        AnnotationData::Text { origin, text, style } => draw_text(canvas, *origin, text, style),
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
            draw_marker(canvas, points, *color, *stroke_width)
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

/// Renders only the pixels a Mosaic or Blur annotation affects, returning the
/// integer top-left origin and the rendered region. Used for live draft previews
/// so the editor can show the real effect (matching the export) instead of a
/// placeholder, without cloning the whole image. Returns `None` for other tools
/// or an empty/out-of-bounds region.
pub fn render_effect_region(
    base: &RgbaImage,
    annotation: &Annotation,
) -> Option<(u32, u32, RgbaImage)> {
    let rect = match &annotation.data {
        AnnotationData::Mosaic { rect, .. } | AnnotationData::Blur { rect, .. } => *rect,
        _ => return None,
    };
    let start_x = rect.x.max(0.0).floor() as u32;
    let start_y = rect.y.max(0.0).floor() as u32;
    let end_x = (rect.x + rect.width).min(base.width() as f32).ceil() as u32;
    let end_y = (rect.y + rect.height).min(base.height() as f32).ceil() as u32;
    if end_x <= start_x || end_y <= start_y {
        return None;
    }
    let region_w = end_x - start_x;
    let region_h = end_y - start_y;
    let mut region =
        image::imageops::crop_imm(base, start_x, start_y, region_w, region_h).to_image();
    let full = Rect { x: 0.0, y: 0.0, width: region_w as f32, height: region_h as f32 };
    match &annotation.data {
        AnnotationData::Mosaic { pixel_size, .. } => {
            pixelate_region(&mut region, full, *pixel_size)
        }
        AnnotationData::Blur { radius, .. } => blur_region(&mut region, full, *radius),
        _ => return None,
    }
    Some((start_x, start_y, region))
}

pub fn encode_png_bytes(image: &RgbaImage) -> image::ImageResult<Vec<u8>> {
    let mut cursor = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image.clone()).write_to(&mut cursor, image::ImageFormat::Png)?;
    Ok(cursor.into_inner())
}

/// Output image formats aShot can encode a finished screenshot to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    Png,
    Jpeg,
    Webp,
}

impl ExportFormat {
    /// The lowercase file extension (without a leading dot) for this format.
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Png => "png",
            ExportFormat::Jpeg => "jpg",
            ExportFormat::Webp => "webp",
        }
    }

    /// Maps a file extension (case-insensitive, without a dot) to a format.
    pub fn from_extension(extension: &str) -> Option<Self> {
        match extension.to_ascii_lowercase().as_str() {
            "png" => Some(ExportFormat::Png),
            "jpg" | "jpeg" => Some(ExportFormat::Jpeg),
            "webp" => Some(ExportFormat::Webp),
            _ => None,
        }
    }
}

pub fn default_export_format() -> ExportFormat {
    ExportFormat::Png
}

pub fn default_jpeg_quality() -> u8 {
    90
}

/// Encodes an already-rendered image into `format`. `quality` only affects JPEG
/// (clamped to 1-100); PNG and lossless WebP ignore it.
pub fn encode_image_bytes(
    image: &RgbaImage,
    format: ExportFormat,
    quality: u8,
) -> image::ImageResult<Vec<u8>> {
    match format {
        ExportFormat::Png => encode_png_bytes(image),
        ExportFormat::Jpeg => encode_jpeg_bytes(image, quality),
        ExportFormat::Webp => encode_webp_bytes(image),
    }
}

pub fn encode_jpeg_bytes(image: &RgbaImage, quality: u8) -> image::ImageResult<Vec<u8>> {
    // JPEG has no alpha channel, so flatten onto an opaque RGB buffer first.
    let rgb = DynamicImage::ImageRgba8(image.clone()).to_rgb8();
    let mut cursor = Cursor::new(Vec::new());
    let mut encoder =
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, quality.clamp(1, 100));
    encoder.encode_image(&rgb)?;
    Ok(cursor.into_inner())
}

pub fn encode_webp_bytes(image: &RgbaImage) -> image::ImageResult<Vec<u8>> {
    let mut cursor = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image.clone()).write_to(&mut cursor, image::ImageFormat::WebP)?;
    Ok(cursor.into_inner())
}

/// Re-encodes already-encoded PNG bytes (the editor's render cache) into
/// `format`, so a save can target JPEG/WebP without re-rendering the document.
/// PNG targets short-circuit and reuse the input bytes unchanged.
pub fn transcode_png_bytes(
    png_bytes: &[u8],
    format: ExportFormat,
    quality: u8,
) -> image::ImageResult<Vec<u8>> {
    if format == ExportFormat::Png {
        return Ok(png_bytes.to_vec());
    }
    let image = image::load_from_memory(png_bytes)?.to_rgba8();
    encode_image_bytes(&image, format, quality)
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

fn draw_marker(image: &mut RgbaImage, points: &[Point], color: Color, stroke_width: u32) {
    if points.len() < 2 || image.width() == 0 || image.height() == 0 {
        return;
    }

    let radius = (stroke_width.max(1) as i32) / 2;
    let padding = radius as f32 + 1.0;
    let min_x = points.iter().map(|point| point.x).fold(f32::INFINITY, f32::min);
    let min_y = points.iter().map(|point| point.y).fold(f32::INFINITY, f32::min);
    let max_x = points.iter().map(|point| point.x).fold(f32::NEG_INFINITY, f32::max);
    let max_y = points.iter().map(|point| point.y).fold(f32::NEG_INFINITY, f32::max);
    let Some(bounds) = PixelRect::from_edges(
        image.width(),
        image.height(),
        min_x - padding,
        min_y - padding,
        max_x + padding,
        max_y + padding,
    ) else {
        return;
    };

    let mut mask = vec![false; bounds.width as usize * bounds.height as usize];
    for window in points.windows(2) {
        mark_thick_line(
            &mut mask,
            bounds.width,
            bounds.height,
            bounds.x as i32,
            bounds.y as i32,
            window[0],
            window[1],
            stroke_width,
        );
    }

    paint_marker_trace(image, &mask, bounds, color);
    for fiber in marker_fiber_layout(stroke_width) {
        paint_marker_fiber(image, &mask, bounds, points, color, fiber);
    }
}

fn paint_marker_trace(image: &mut RgbaImage, mask: &[bool], bounds: PixelRect, color: Color) {
    let base_alpha = color.a.min(72) as f32;
    let fill = Rgba([color.r, color.g, color.b, (base_alpha * 0.34).round() as u8]);
    let edge = Rgba([color.r, color.g, color.b, (base_alpha * 0.68).round() as u8]);
    for y in 0..bounds.height {
        for x in 0..bounds.width {
            let idx = y as usize * bounds.width as usize + x as usize;
            if !mask.get(idx).copied().unwrap_or(false) {
                continue;
            }
            let source = if marker_mask_pixel_is_edge(mask, bounds.width, bounds.height, x, y) {
                edge
            } else {
                fill
            };
            if let Some(pixel) = image.get_pixel_mut_checked(bounds.x + x, bounds.y + y) {
                blend_pixel(pixel, source);
            }
        }
    }
}

fn marker_mask_pixel_is_edge(mask: &[bool], width: u32, height: u32, x: u32, y: u32) -> bool {
    for dy in -1..=1 {
        for dx in -1..=1 {
            if dx == 0 && dy == 0 {
                continue;
            }
            let nx = x as i32 + dx;
            let ny = y as i32 + dy;
            if nx < 0 || ny < 0 || nx as u32 >= width || ny as u32 >= height {
                return true;
            }
            let idx = ny as usize * width as usize + nx as usize;
            if !mask.get(idx).copied().unwrap_or(false) {
                return true;
            }
        }
    }
    false
}

fn paint_marker_fiber(
    image: &mut RgbaImage,
    mask: &[bool],
    bounds: PixelRect,
    points: &[Point],
    color: Color,
    fiber: MarkerFiber,
) {
    let alpha = color.a.min(72).saturating_add((fiber.opacity as f32 * 0.22).round() as u8);
    let rgba = Rgba([color.r, color.g, color.b, alpha.min(112)]);
    let radius = (fiber.stroke_width * 0.5).ceil().max(1.0) as i32;
    let cycle = (fiber.dash + fiber.gap).max(1.0);
    let dash_offset = fiber.phase * cycle;
    let mut total_distance = 0.0;
    let mut fiber_mask = vec![false; bounds.width as usize * bounds.height as usize];

    for window in points.windows(2) {
        let start = window[0];
        let end = window[1];
        let dx = end.x - start.x;
        let dy = end.y - start.y;
        let length = (dx * dx + dy * dy).sqrt();
        if length <= f32::EPSILON {
            continue;
        }
        let normal_x = -dy / length;
        let normal_y = dx / length;
        let offset_start =
            Point::new(start.x + normal_x * fiber.offset, start.y + normal_y * fiber.offset);
        let offset_end =
            Point::new(end.x + normal_x * fiber.offset, end.y + normal_y * fiber.offset);
        let steps = length.ceil().max(1.0) as i32;

        for step in 0..=steps {
            let t = step as f32 / steps as f32;
            let distance = total_distance + length * t;
            if (distance + dash_offset).rem_euclid(cycle) > fiber.dash {
                continue;
            }
            let x = offset_start.x + (offset_end.x - offset_start.x) * t;
            let y = offset_start.y + (offset_end.y - offset_start.y) * t;
            mark_marker_fiber_circle(
                &mut fiber_mask,
                bounds,
                x.round() as i32,
                y.round() as i32,
                radius,
            );
        }

        total_distance += length;
    }

    for y in 0..bounds.height {
        for x in 0..bounds.width {
            let idx = y as usize * bounds.width as usize + x as usize;
            if !fiber_mask.get(idx).copied().unwrap_or(false)
                || !mask.get(idx).copied().unwrap_or(false)
            {
                continue;
            }
            if let Some(pixel) = image.get_pixel_mut_checked(bounds.x + x, bounds.y + y) {
                blend_pixel(pixel, rgba);
            }
        }
    }
}

fn mark_marker_fiber_circle(
    fiber_mask: &mut [bool],
    bounds: PixelRect,
    center_x: i32,
    center_y: i32,
    radius: i32,
) {
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            if dx * dx + dy * dy > radius * radius {
                continue;
            }
            let x = center_x + dx;
            let y = center_y + dy;
            if x < bounds.x as i32
                || y < bounds.y as i32
                || x >= (bounds.x + bounds.width) as i32
                || y >= (bounds.y + bounds.height) as i32
            {
                continue;
            }
            let local_x = x - bounds.x as i32;
            let local_y = y - bounds.y as i32;
            let idx = local_y as usize * bounds.width as usize + local_x as usize;
            if let Some(cell) = fiber_mask.get_mut(idx) {
                *cell = true;
            }
        }
    }
}

fn mark_thick_line(
    mask: &mut [bool],
    mask_width: u32,
    mask_height: u32,
    offset_x: i32,
    offset_y: i32,
    start: Point,
    end: Point,
    stroke_width: u32,
) {
    let radius = (stroke_width.max(1) as i32) / 2;
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    let steps = dx.abs().max(dy.abs()).ceil() as i32;
    if steps == 0 {
        mark_circle(
            mask,
            mask_width,
            mask_height,
            offset_x,
            offset_y,
            start.x.round() as i32,
            start.y.round() as i32,
            radius,
        );
        return;
    }

    for step in 0..=steps {
        let t = step as f32 / steps as f32;
        let x = start.x + dx * t;
        let y = start.y + dy * t;
        mark_circle(
            mask,
            mask_width,
            mask_height,
            offset_x,
            offset_y,
            x.round() as i32,
            y.round() as i32,
            radius,
        );
    }
}

fn mark_circle(
    mask: &mut [bool],
    mask_width: u32,
    mask_height: u32,
    offset_x: i32,
    offset_y: i32,
    center_x: i32,
    center_y: i32,
    radius: i32,
) {
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            if dx * dx + dy * dy > radius * radius {
                continue;
            }
            let x = center_x + dx - offset_x;
            let y = center_y + dy - offset_y;
            if x < 0 || y < 0 || x as u32 >= mask_width || y as u32 >= mask_height {
                continue;
            }
            let idx = y as usize * mask_width as usize + x as usize;
            if let Some(cell) = mask.get_mut(idx) {
                *cell = true;
            }
        }
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
    draw_bitmap_text(image, origin, &text, 8, TextWeight::Bold, text_color);
}

pub fn draw_text_cairo(cr: &cairo::Context, origin: Point, text: &str, style: &TextStyle) {
    if text.is_empty() {
        return;
    }
    let _ = cr.save();
    cr.set_source_rgba(
        style.color.r as f64 / 255.0,
        style.color.g as f64 / 255.0,
        style.color.b as f64 / 255.0,
        style.color.a as f64 / 255.0,
    );
    cr.move_to(origin.x as f64, origin.y as f64);

    let layout = unsafe { create_pango_cairo_layout(cr) };
    layout.set_text(text);
    let desc = pango_font_description(style);
    layout.set_font_description(Some(&desc));

    unsafe {
        pango_cairo_show_layout(cr.to_glib_none().0, layout.to_glib_none().0);
    }
    let _ = cr.restore();
}

unsafe fn create_pango_cairo_layout(cr: &cairo::Context) -> pango::Layout {
    unsafe { from_glib_full(pango_cairo_create_layout(cr.to_glib_none().0)) }
}

fn pango_font_description(style: &TextStyle) -> pango::FontDescription {
    let mut desc = pango::FontDescription::new();
    if let Some(family) = style.family.as_deref().filter(|family| !family.trim().is_empty()) {
        desc.set_family(family);
    }
    desc.set_size((style.size.max(1) as i32) * pango::SCALE);
    desc.set_weight(match style.weight {
        TextWeight::Regular => pango::Weight::Normal,
        TextWeight::Semibold => pango::Weight::Semibold,
        TextWeight::Bold => pango::Weight::Bold,
    });
    desc
}

fn draw_text(image: &mut RgbaImage, origin: Point, text: &str, style: &TextStyle) {
    let width = image.width() as i32;
    let height = image.height() as i32;
    if width <= 0 || height <= 0 || text.is_empty() {
        return;
    }

    let Ok(mut surface) = cairo::ImageSurface::create(cairo::Format::ARgb32, width, height) else {
        return;
    };
    {
        let Ok(cr) = cairo::Context::new(&surface) else {
            return;
        };
        draw_text_cairo(&cr, origin, text, style);
    }
    surface.flush();
    let stride = surface.stride() as usize;
    let Ok(data) = surface.data() else {
        return;
    };

    for y in 0..image.height() {
        for x in 0..image.width() {
            let idx = y as usize * stride + x as usize * 4;
            let alpha = data[idx + 3];
            if alpha == 0 {
                continue;
            }
            let unpremultiply = |value: u8| -> u8 {
                ((value as u32 * 255 + alpha as u32 / 2) / alpha as u32).min(255) as u8
            };
            let source = Rgba([
                unpremultiply(data[idx + 2]),
                unpremultiply(data[idx + 1]),
                unpremultiply(data[idx]),
                alpha,
            ]);
            let Some(pixel) = image.get_pixel_mut_checked(x, y) else {
                continue;
            };
            blend_pixel(pixel, source);
        }
    }
}

fn draw_bitmap_text(
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

    // Each block reads its own pixels then overwrites them with their average.
    // Blocks are disjoint, so we can write back into `image` directly instead of
    // cloning the whole image for a region-local effect.
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
            // The block always covers at least one pixel; `max(1)` keeps the
            // division total without a manual zero-check.
            let count = count.max(1);
            let average = Rgba([
                (total[0] / count).min(u8::MAX as u32) as u8,
                (total[1] / count).min(u8::MAX as u32) as u8,
                (total[2] / count).min(u8::MAX as u32) as u8,
                (total[3] / count).min(u8::MAX as u32) as u8,
            ]);
            for yy in y..y_limit {
                for xx in x..x_limit {
                    image.put_pixel(xx, yy, average);
                }
            }
            y += block;
        }
        x += block;
    }
}

fn blur_region(image: &mut RgbaImage, rect: Rect, radius: u32) {
    let radius = radius.max(1) as usize;
    let start_x = rect.x.max(0.0).floor() as u32;
    let start_y = rect.y.max(0.0).floor() as u32;
    let end_x = (rect.x + rect.width).min(image.width() as f32).ceil() as u32;
    let end_y = (rect.y + rect.height).min(image.height() as f32).ceil() as u32;
    if end_x <= start_x || end_y <= start_y {
        return;
    }
    let region_w = (end_x - start_x) as usize;
    let region_h = (end_y - start_y) as usize;

    // Separable box blur evaluated with per-line prefix sums. Each pass costs
    // O(region pixels) regardless of `radius`, replacing the previous
    // O(radius^2 * region) box convolution. Only a region-sized scratch buffer is
    // allocated (the horizontal pass result); the previous code cloned the whole
    // image even for a small selection. The horizontal averages feed the vertical
    // pass, which writes the final blur back into `image`.
    let mut horizontal = vec![[0u8; 4]; region_w * region_h];

    let mut prefix = vec![[0u32; 4]; region_w + 1];
    for j in 0..region_h {
        let y = start_y + j as u32;
        for i in 0..region_w {
            let pixel = image.get_pixel(start_x + i as u32, y);
            for c in 0..4 {
                prefix[i + 1][c] = prefix[i][c] + pixel[c] as u32;
            }
        }
        for i in 0..region_w {
            let lo = i.saturating_sub(radius);
            let hi = (i + radius + 1).min(region_w);
            let count = (hi - lo) as u32;
            let mut out = [0u8; 4];
            for c in 0..4 {
                out[c] = ((prefix[hi][c] - prefix[lo][c]) / count) as u8;
            }
            horizontal[j * region_w + i] = out;
        }
    }

    let mut prefix = vec![[0u32; 4]; region_h + 1];
    for i in 0..region_w {
        for j in 0..region_h {
            let src = horizontal[j * region_w + i];
            for c in 0..4 {
                prefix[j + 1][c] = prefix[j][c] + src[c] as u32;
            }
        }
        for j in 0..region_h {
            let lo = j.saturating_sub(radius);
            let hi = (j + radius + 1).min(region_h);
            let count = (hi - lo) as u32;
            let mut out = [0u8; 4];
            for c in 0..4 {
                out[c] = ((prefix[hi][c] - prefix[lo][c]) / count) as u8;
            }
            image.put_pixel(start_x + i as u32, start_y + j as u32, Rgba(out));
        }
    }
}

#[cfg(test)]
mod tests {
    use image::{DynamicImage, Rgba, RgbaImage};

    use crate::document::{Annotation, AnnotationData, Color, Point, Rect, TextStyle, TextWeight};

    use super::{
        ExportFormat, RenderUpdateKind, arrow_head_geometry, arrow_shape_geometry,
        arrow_visual_stroke_width, encode_image_bytes, encode_png_bytes, incremental_render_plan,
        render_document, render_document_from_rgba, render_effect_region, transcode_png_bytes,
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
                    family: None,
                },
            }),
        ];

        let rendered = render_document(&base, &annotations);
        assert!(rendered.pixels().any(|pixel| *pixel != Rgba([0, 0, 0, 0])));
    }

    #[test]
    fn export_renders_cjk_text_with_system_font_fallback() {
        let base = DynamicImage::new_rgba8(160, 64);
        let annotations = vec![Annotation::new(AnnotationData::Text {
            origin: Point::new(8.0, 8.0),
            text: "中文".into(),
            style: TextStyle {
                size: 24,
                weight: TextWeight::Bold,
                color: Color::rgba(255, 0, 0, 255),
                family: None,
            },
        })];

        let rendered = render_document(&base, &annotations);

        assert!(
            rendered
                .pixels()
                .any(|pixel| pixel[0] > 0 && pixel[1] == 0 && pixel[2] == 0 && pixel[3] > 0)
        );
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
    fn marker_highlight_does_not_compound_alpha_over_dense_paths() {
        let base = RgbaImage::from_pixel(40, 16, Rgba([180, 180, 180, 255]));
        let points = (2..38).map(|x| Point::new(x as f32, 8.0)).collect::<Vec<_>>();
        let annotations = vec![Annotation::new(AnnotationData::Marker {
            points,
            color: Color::rgba(255, 0, 0, 96),
            stroke_width: 9,
        })];

        let rendered = render_document_from_rgba(&base, &annotations);
        let pixel = rendered.get_pixel(20, 8);

        assert!(pixel[0] < 245, "marker red channel washed out to {}", pixel[0]);
        assert!(pixel[1] > 95, "marker green channel was crushed to {}", pixel[1]);
        assert!(pixel[2] > 95, "marker blue channel was crushed to {}", pixel[2]);
    }

    #[test]
    fn marker_export_draws_same_color_fibers_inside_marker_trace() {
        let base = RgbaImage::from_pixel(96, 32, Rgba([255, 255, 255, 255]));
        let annotations = vec![Annotation::new(AnnotationData::Marker {
            points: vec![Point::new(8.0, 16.0), Point::new(88.0, 16.0)],
            color: Color::rgba(255, 0, 0, 255),
            stroke_width: 18,
        })];

        let rendered = render_document_from_rgba(&base, &annotations);
        let changed = rendered
            .pixels()
            .filter(|pixel| **pixel != Rgba([255, 255, 255, 255]))
            .collect::<Vec<_>>();

        assert!(!changed.is_empty(), "marker should render visible fiber strokes on white");
        assert!(
            changed.iter().all(|pixel| pixel[0] >= pixel[1] && pixel[0] >= pixel[2]),
            "all marker fiber lines should use the selected marker hue"
        );
        let center = rendered.get_pixel(48, 16);
        assert_ne!(
            center,
            &Rgba([255, 255, 255, 255]),
            "marker center should show the original marker stroke trace"
        );
        let edge = rendered.get_pixel(48, 7);
        assert_ne!(edge, &Rgba([255, 255, 255, 255]), "marker edge should show a visible boundary");
        assert_eq!(
            rendered.get_pixel(48, 2),
            &Rgba([255, 255, 255, 255]),
            "fibers should not render outside the marker stroke area"
        );
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
    fn blur_keeps_uniform_region_unchanged() {
        // A box blur of a constant field is the same constant, and pixels outside
        // the blur rect must not be touched.
        let mut canvas = DynamicImage::new_rgba8(8, 8).to_rgba8();
        for pixel in canvas.pixels_mut() {
            *pixel = Rgba([40, 80, 120, 255]);
        }
        let base = DynamicImage::ImageRgba8(canvas);
        let annotations = vec![Annotation::new(AnnotationData::Blur {
            rect: Rect { x: 1.0, y: 1.0, width: 6.0, height: 6.0 },
            radius: 2,
        })];

        let rendered = render_document(&base, &annotations);

        for y in 0..8 {
            for x in 0..8 {
                assert_eq!(*rendered.get_pixel(x, y), Rgba([40, 80, 120, 255]));
            }
        }
    }

    #[test]
    fn blur_interior_matches_box_average() {
        // For an interior pixel whose full (2r+1)^2 window lies inside the blur
        // rect, the separable box blur equals the true 2D box average. The image
        // is a separable gradient (red = x*10, green = y*10) so the expected
        // average is exact and easy to derive.
        let size = 11u32;
        let mut canvas = DynamicImage::new_rgba8(size, size).to_rgba8();
        for y in 0..size {
            for x in 0..size {
                canvas.put_pixel(x, y, Rgba([(x * 10) as u8, (y * 10) as u8, 0, 255]));
            }
        }
        let base = DynamicImage::ImageRgba8(canvas);
        let annotations = vec![Annotation::new(AnnotationData::Blur {
            rect: Rect { x: 0.0, y: 0.0, width: size as f32, height: size as f32 },
            radius: 2,
        })];

        let rendered = render_document(&base, &annotations);

        // At (5,5): red = mean(30,40,50,60,70) = 50, green = 50 by symmetry.
        assert_eq!(*rendered.get_pixel(5, 5), Rgba([50, 50, 0, 255]));
    }

    fn solid_image(r: u8, g: u8, b: u8) -> RgbaImage {
        let mut canvas = DynamicImage::new_rgba8(4, 4).to_rgba8();
        for pixel in canvas.pixels_mut() {
            *pixel = Rgba([r, g, b, 255]);
        }
        canvas
    }

    #[test]
    fn encodes_jpeg_and_webp_with_expected_magic() {
        let canvas = solid_image(10, 120, 200);

        let jpeg = encode_image_bytes(&canvas, ExportFormat::Jpeg, 80).expect("jpeg");
        assert_eq!(&jpeg[0..3], &[0xFF, 0xD8, 0xFF]);

        let webp = encode_image_bytes(&canvas, ExportFormat::Webp, 0).expect("webp");
        assert_eq!(&webp[0..4], b"RIFF");
        assert_eq!(&webp[8..12], b"WEBP");
    }

    #[test]
    fn transcode_png_reuses_png_and_converts_jpeg() {
        let png = encode_png_bytes(&solid_image(200, 30, 40)).expect("png");

        // PNG target reuses the input bytes unchanged (no decode/re-encode).
        assert_eq!(transcode_png_bytes(&png, ExportFormat::Png, 90).expect("png"), png);

        // JPEG target decodes then re-encodes into a valid JPEG.
        let jpeg = transcode_png_bytes(&png, ExportFormat::Jpeg, 85).expect("jpeg");
        assert_eq!(&jpeg[0..3], &[0xFF, 0xD8, 0xFF]);
    }

    #[test]
    fn render_effect_region_covers_only_the_region() {
        let mut canvas = DynamicImage::new_rgba8(10, 10).to_rgba8();
        for (i, pixel) in canvas.pixels_mut().enumerate() {
            *pixel = Rgba([(i * 7 % 256) as u8, (i * 3 % 256) as u8, 0, 255]);
        }
        let mosaic = Annotation::new(AnnotationData::Mosaic {
            rect: Rect { x: 2.0, y: 2.0, width: 4.0, height: 4.0 },
            pixel_size: 4,
        });

        let (origin_x, origin_y, region) =
            render_effect_region(&canvas, &mosaic).expect("mosaic region");
        assert_eq!((origin_x, origin_y), (2, 2));
        assert_eq!((region.width(), region.height()), (4, 4));
        // A single block covering the whole region collapses to one color.
        let first = *region.get_pixel(0, 0);
        assert!(region.pixels().all(|pixel| *pixel == first));

        // Non-region tools have no effect region.
        let line = Annotation::new(AnnotationData::Line {
            start: Point::new(0.0, 0.0),
            end: Point::new(5.0, 5.0),
            color: Color::rgba(255, 0, 0, 255),
            stroke_width: 2,
        });
        assert!(render_effect_region(&canvas, &line).is_none());
    }

    #[test]
    fn export_format_extension_mapping() {
        assert_eq!(ExportFormat::from_extension("PNG"), Some(ExportFormat::Png));
        assert_eq!(ExportFormat::from_extension("jpeg"), Some(ExportFormat::Jpeg));
        assert_eq!(ExportFormat::from_extension("JPG"), Some(ExportFormat::Jpeg));
        assert_eq!(ExportFormat::from_extension("webp"), Some(ExportFormat::Webp));
        assert_eq!(ExportFormat::from_extension("gif"), None);
        assert_eq!(ExportFormat::Jpeg.extension(), "jpg");
        assert_eq!(ExportFormat::Webp.extension(), "webp");
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
