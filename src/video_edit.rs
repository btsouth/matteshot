//! Time-ranged video annotations stored in normalized output coordinates.
//! The same model drives the editor preview and full-resolution MP4 export.

use image::RgbaImage;

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

pub fn render(image: &mut RgbaImage, items: &[Item], time: i64, skip: Option<usize>) {
    let visible: Vec<_> = items
        .iter()
        .enumerate()
        .filter(|(index, item)| Some(*index) != skip && item.active_at(time))
        .map(|(_, item)| annotation(item, image.width(), image.height()))
        .collect();
    if visible.is_empty() {
        return;
    }
    let scale = (image.width().min(image.height()) as f32 / 720.0).clamp(0.45, 4.0);
    crate::annotate::render(image, &visible, scale, (0.0, 0.0), None);
}

pub fn render_one(image: &mut RgbaImage, item: &Item) {
    let scale = (image.width().min(image.height()) as f32 / 720.0).clamp(0.45, 4.0);
    crate::annotate::render(
        image,
        &[annotation(item, image.width(), image.height())],
        scale,
        (0.0, 0.0),
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
            let width = (text.chars().count() as f32 * 0.018 * item.size).clamp(0.06, 0.72);
            let height = 0.055 * item.size;
            (
                pos.0,
                pos.1,
                (pos.0 + width).min(1.0),
                (pos.1 + height).min(1.0),
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
    use super::{bounds, hit, translate, Item, Shape};

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
}
