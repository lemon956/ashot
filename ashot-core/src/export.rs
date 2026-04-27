use std::path::Path;

use font8x8::{BASIC_FONTS, UnicodeFonts};
use image::{DynamicImage, ImageBuffer, Rgba, RgbaImage};

use crate::document::{Annotation, AnnotationData, Color, Point, Rect, TextWeight};

pub fn render_document(base: &DynamicImage, annotations: &[Annotation]) -> RgbaImage {
    let mut canvas = base.to_rgba8();
    for annotation in annotations {
        match &annotation.data {
            AnnotationData::Text { origin, text, style } => {
                draw_text(&mut canvas, *origin, text, style.size, style.weight, style.color)
            }
            AnnotationData::Line { start, end, color, stroke_width } => {
                draw_thick_line(&mut canvas, *start, *end, *color, *stroke_width)
            }
            AnnotationData::Arrow { start, end, color, stroke_width } => {
                draw_arrow(&mut canvas, *start, *end, *color, *stroke_width)
            }
            AnnotationData::Brush { points, color, stroke_width } => {
                draw_brush(&mut canvas, points, *color, *stroke_width)
            }
            AnnotationData::Rectangle { rect, color, stroke_width } => {
                draw_rectangle(&mut canvas, *rect, *color, *stroke_width)
            }
            AnnotationData::Ellipse { rect, color, stroke_width } => {
                draw_ellipse(&mut canvas, *rect, *color, *stroke_width)
            }
            AnnotationData::Marker { points, color, stroke_width } => {
                draw_brush(&mut canvas, points, *color, *stroke_width)
            }
            AnnotationData::Mosaic { rect, pixel_size } => {
                pixelate_region(&mut canvas, *rect, *pixel_size);
            }
            AnnotationData::Blur { rect, radius } => {
                blur_region(&mut canvas, *rect, *radius);
            }
            AnnotationData::Counter { center, number, color, radius } => {
                draw_counter(&mut canvas, *center, *number, *color, *radius);
            }
            AnnotationData::FilledBox { rect, color } => {
                fill_rect(&mut canvas, *rect, *color);
            }
        }
    }
    canvas
}

pub fn save_document_png(
    base: &DynamicImage,
    annotations: &[Annotation],
    path: impl AsRef<Path>,
) -> image::ImageResult<()> {
    let image = render_document(base, annotations);
    image.save(path)
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
    draw_thick_line(image, start, end, color, stroke_width);
    let angle = (end.y - start.y).atan2(end.x - start.x);
    let head_len = (stroke_width as f32 * 2.6).max(10.0);
    let left = Point::new(
        end.x - head_len * (angle - std::f32::consts::FRAC_PI_6).cos(),
        end.y - head_len * (angle - std::f32::consts::FRAC_PI_6).sin(),
    );
    let right = Point::new(
        end.x - head_len * (angle + std::f32::consts::FRAC_PI_6).cos(),
        end.y - head_len * (angle + std::f32::consts::FRAC_PI_6).sin(),
    );
    draw_thick_line(image, end, left, color, stroke_width);
    draw_thick_line(image, end, right, color, stroke_width);
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
                    (total[0] / count) as u8,
                    (total[1] / count) as u8,
                    (total[2] / count) as u8,
                    (total[3] / count) as u8,
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
                        (total[0] / count) as u8,
                        (total[1] / count) as u8,
                        (total[2] / count) as u8,
                        (total[3] / count) as u8,
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

    use super::render_document;

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
}
