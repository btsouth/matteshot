//! Time-ranged video annotations stored in normalized content coordinates.
//! The same model drives the editor preview and full-resolution MP4 export.

use image::RgbaImage;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptionStyle {
    Shadow,
    Box,
}

#[derive(Clone, Debug)]
pub enum Shape {
    Arrow { from: (f32, f32), to: (f32, f32) },
    Rect { a: (f32, f32), b: (f32, f32) },
    Blur { a: (f32, f32), b: (f32, f32) },
    Text { pos: (f32, f32), text: String },
}

#[derive(Clone, Debug)]
pub struct Item {
    pub shape: Shape,
    pub start: i64,
    pub end: i64,
    pub color: usize,
    pub size: f32,
    pub caption_style: CaptionStyle,
}

impl Item {
    pub fn active_at(&self, time: i64) -> bool {
        self.start <= time && time < self.end
    }
}

fn point(point: (f32, f32), w: f32, h: f32) -> (f32, f32) {
    (point.0 * w, point.1 * h)
}

fn annotation(item: &Item, w: u32, h: u32) -> crate::annotate::Annotation {
    let (wf, hf) = (w as f32, h as f32);
    let shape = match &item.shape {
        Shape::Arrow { from, to } => crate::annotate::Shape::Arrow {
            from: point(*from, wf, hf),
            to: point(*to, wf, hf),
        },
        Shape::Rect { a, b } => crate::annotate::Shape::Rect {
            a: point(*a, wf, hf),
            b: point(*b, wf, hf),
        },
        Shape::Blur { a, b } => crate::annotate::Shape::Blur {
            a: point(*a, wf, hf),
            b: point(*b, wf, hf),
        },
        Shape::Text { pos, text } => crate::annotate::Shape::Text {
            pos: point(*pos, wf, hf),
            text: text.clone(),
        },
    };
    crate::annotate::Annotation {
        shape,
        color: item.color.min(crate::annotate::COLORS.len() - 1),
        size: item.size,
    }
}

/// Render annotations relative to the recorded content, even when the output
/// canvas has extra matte padding or a forced aspect ratio.
pub fn render_at(
    image: &mut RgbaImage,
    items: &[Item],
    time: i64,
    skip: Option<usize>,
    content_size: (u32, u32),
    offset: (f32, f32),
) {
    for (index, item) in items.iter().enumerate() {
        if Some(index) != skip && item.active_at(time) {
            render_one_at(image, item, content_size, offset);
        }
    }
}

pub fn render_one_at(
    image: &mut RgbaImage,
    item: &Item,
    content_size: (u32, u32),
    offset: (f32, f32),
) {
    let metric_scale = (content_size.0.min(content_size.1) as f32 / 720.0).clamp(0.45, 4.0);
    if let Shape::Text { pos, text } = &item.shape {
        crate::annotate::render_caption(
            image,
            point(*pos, content_size.0 as f32, content_size.1 as f32),
            text,
            crate::annotate::CaptionOptions {
                color_index: item.color,
                size: item.size,
                metric_scale,
                offset,
                boxed: item.caption_style == CaptionStyle::Box,
            },
        );
        return;
    }
    crate::annotate::render_with_metric(
        image,
        &[annotation(item, content_size.0, content_size.1)],
        1.0,
        metric_scale,
        offset,
        None,
    );
}

pub fn bounds(item: &Item) -> (f32, f32, f32, f32) {
    match &item.shape {
        Shape::Arrow { from, to } => (
            from.0.min(to.0),
            from.1.min(to.1),
            from.0.max(to.0),
            from.1.max(to.1),
        ),
        Shape::Rect { a, b } | Shape::Blur { a, b } => {
            (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1))
        }
        Shape::Text { pos, text } => {
            // Caption glyphs are sized from video height. The earlier generic
            // estimate treated every character as a percentage of video
            // width, producing selection boxes several times wider than the
            // rendered caption on landscape footage.
            let width = (text.chars().count() as f32 * 0.0055 * item.size).clamp(0.04, 0.62);
            let height = 0.035 * item.size;
            let pad = if item.caption_style == CaptionStyle::Box {
                0.006 * item.size
            } else {
                0.003 * item.size
            };
            (
                (pos.0 - pad).max(0.0),
                (pos.1 - pad).max(0.0),
                (pos.0 + width + pad).min(1.0),
                (pos.1 + height + pad).min(1.0),
            )
        }
    }
}

fn distance_to_segment(point: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let ab = (b.0 - a.0, b.1 - a.1);
    let ap = (point.0 - a.0, point.1 - a.1);
    let len2 = ab.0 * ab.0 + ab.1 * ab.1;
    if len2 <= f32::EPSILON {
        return (ap.0 * ap.0 + ap.1 * ap.1).sqrt();
    }
    let t = ((ap.0 * ab.0 + ap.1 * ab.1) / len2).clamp(0.0, 1.0);
    let nearest = (a.0 + ab.0 * t, a.1 + ab.1 * t);
    ((point.0 - nearest.0).powi(2) + (point.1 - nearest.1).powi(2)).sqrt()
}

pub fn hit(item: &Item, point: (f32, f32), tolerance: f32) -> bool {
    match &item.shape {
        Shape::Arrow { from, to } => distance_to_segment(point, *from, *to) <= tolerance,
        Shape::Blur { .. } | Shape::Text { .. } => {
            let (x0, y0, x1, y1) = bounds(item);
            point.0 >= x0 - tolerance
                && point.0 <= x1 + tolerance
                && point.1 >= y0 - tolerance
                && point.1 <= y1 + tolerance
        }
        Shape::Rect { .. } => {
            let (x0, y0, x1, y1) = bounds(item);
            let inside = point.0 >= x0 - tolerance
                && point.0 <= x1 + tolerance
                && point.1 >= y0 - tolerance
                && point.1 <= y1 + tolerance;
            let edge = (point.0 - x0)
                .abs()
                .min((point.0 - x1).abs())
                .min((point.1 - y0).abs())
                .min((point.1 - y1).abs());
            inside && edge <= tolerance * 1.5
        }
    }
}

pub fn translate(item: &mut Item, dx: f32, dy: f32) {
    let (x0, y0, x1, y1) = bounds(item);
    let dx = dx.clamp(-x0, 1.0 - x1);
    let dy = dy.clamp(-y0, 1.0 - y1);
    let move_point = |point: &mut (f32, f32)| {
        point.0 += dx;
        point.1 += dy;
    };
    match &mut item.shape {
        Shape::Arrow { from, to } => {
            move_point(from);
            move_point(to);
        }
        Shape::Rect { a, b } | Shape::Blur { a, b } => {
            move_point(a);
            move_point(b);
        }
        Shape::Text { pos, .. } => move_point(pos),
    }
}

#[cfg(test)]
mod tests {
    use super::{bounds, hit, render_at, translate, CaptionStyle, Item, Shape};
    use image::{Rgba, RgbaImage};

    fn arrow() -> Item {
        Item {
            shape: Shape::Arrow {
                from: (0.1, 0.2),
                to: (0.5, 0.6),
            },
            start: 10,
            end: 20,
            color: 0,
            size: 1.0,
            caption_style: CaptionStyle::Shadow,
        }
    }

    #[test]
    fn time_ranges_are_end_exclusive() {
        let item = arrow();
        assert!(!item.active_at(9));
        assert!(item.active_at(10));
        assert!(item.active_at(19));
        assert!(!item.active_at(20));
    }

    #[test]
    fn arrow_hit_testing_and_translation_use_normalized_space() {
        let mut item = arrow();
        assert!(hit(&item, (0.3, 0.4), 0.01));
        assert!(!hit(&item, (0.8, 0.2), 0.01));
        translate(&mut item, 0.7, 0.7);
        let (x0, y0, x1, y1) = bounds(&item);
        for (actual, expected) in [x0, y0, x1, y1].into_iter().zip([0.6, 0.6, 1.0, 1.0]) {
            assert!((actual - expected).abs() < 0.00001);
        }
    }

    #[test]
    fn matte_layout_keeps_annotations_in_content_coordinates() {
        let item = Item {
            shape: Shape::Arrow {
                from: (0.1, 0.5),
                to: (0.9, 0.5),
            },
            start: 0,
            end: 20,
            color: 0,
            size: 1.0,
            caption_style: CaptionStyle::Shadow,
        };
        let mut image = RgbaImage::from_pixel(200, 200, Rgba([0, 0, 0, 255]));
        render_at(&mut image, &[item], 10, None, (100, 100), (50.0, 50.0));

        assert!(image
            .enumerate_pixels()
            .any(|(x, y, pixel)| {
                (50..150).contains(&x) && (50..150).contains(&y) && pixel[0] > 0
            }));
        assert!(image
            .enumerate_pixels()
            .filter(|(x, y, _)| *x < 45 || *x >= 155 || *y < 45 || *y >= 155)
            .all(|(_, _, pixel)| pixel[0] == 0));
    }

    #[test]
    fn preview_metric_scaling_does_not_move_annotations() {
        let item = Item {
            shape: Shape::Rect {
                a: (0.75, 0.4),
                b: (0.85, 0.6),
            },
            start: 0,
            end: 20,
            color: 0,
            size: 1.0,
            caption_style: CaptionStyle::Shadow,
        };
        let mut image = RgbaImage::from_pixel(1200, 600, Rgba([0, 0, 0, 255]));
        render_at(
            &mut image,
            &[item],
            10,
            None,
            (1000, 400),
            (100.0, 100.0),
        );

        // The rectangle starts at x=100 + 75% of 1000 = 850. Stroke-size
        // scaling must not scale that already-expanded pixel coordinate.
        assert!(image.enumerate_pixels().any(|(x, y, pixel)| {
            (840..960).contains(&x) && (250..275).contains(&y) && pixel[0] > 100
        }));
        assert!(image
            .enumerate_pixels()
            .filter(|(x, _, _)| (490..545).contains(x))
            .all(|(_, _, pixel)| pixel[0] == 0));
    }

    #[test]
    fn caption_box_adds_legible_background_outside_the_glyphs() {
        let mut boxed = RgbaImage::from_pixel(400, 200, Rgba([230, 230, 230, 255]));
        let mut shadow = boxed.clone();
        let item = Item {
            shape: Shape::Text {
                pos: (0.25, 0.25),
                text: "Caption".into(),
            },
            start: 0,
            end: 20,
            color: 3,
            size: 2.0,
            caption_style: CaptionStyle::Box,
        };
        render_at(
            &mut boxed,
            std::slice::from_ref(&item),
            10,
            None,
            (400, 200),
            (0.0, 0.0),
        );
        let mut shadow_item = item;
        shadow_item.caption_style = CaptionStyle::Shadow;
        render_at(&mut shadow, &[shadow_item], 10, None, (400, 200), (0.0, 0.0));

        // The box extends left of the text origin; shadow-only text does not.
        assert!(boxed.get_pixel(94, 52)[0] < shadow.get_pixel(94, 52)[0]);
    }

    #[test]
    fn caption_can_be_moved_repeatedly_without_losing_its_position() {
        let mut item = Item {
            shape: Shape::Text {
                pos: (0.2, 0.2),
                text: "Move me".into(),
            },
            start: 0,
            end: 20,
            color: 3,
            size: 1.75,
            caption_style: CaptionStyle::Box,
        };
        translate(&mut item, 0.15, 0.1);
        translate(&mut item, 0.1, 0.2);
        let Shape::Text { pos, .. } = item.shape else {
            panic!("caption changed shape");
        };
        assert!((pos.0 - 0.45).abs() < 0.0001);
        assert!((pos.1 - 0.5).abs() < 0.0001);
    }
}
