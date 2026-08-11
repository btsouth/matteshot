//! Time-ranged video annotations stored in coordinates normalized to the whole
//! recording. The same model drives the editor preview and the full-resolution
//! MP4 export.
//!
//! Against the recording rather than against whatever is currently framed, so
//! that reframing moves the picture under an annotation instead of
//! invalidating it. `Frame` is what converts between the two: which window of
//! the source a drawn image shows, and how many pixels it occupies. Until
//! something crops, every frame is `Crop::FULL` and the arithmetic is the
//! plain `normalized * content_size` it has always been.

use image::RgbaImage;

pub use crate::annotate::TextStyle as CaptionStyle;

#[derive(Clone, Debug)]
pub enum Shape {
    Arrow { from: (f32, f32), to: (f32, f32) },
    Line { from: (f32, f32), to: (f32, f32) },
    Freehand { points: Vec<(f32, f32)> },
    Rect { a: (f32, f32), b: (f32, f32) },
    Ellipse { a: (f32, f32), b: (f32, f32) },
    Highlight { a: (f32, f32), b: (f32, f32) },
    Counter { pos: (f32, f32), n: u32 },
    Blur { a: (f32, f32), b: (f32, f32) },
    Text { pos: (f32, f32), text: String },
}

/// Direct-manipulation handles shared by arrows and rectangular annotations.
/// `First` is the arrow tail / first box corner and `Second` is the arrow head /
/// opposite box corner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShapeHandle {
    First,
    Second,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub shape: Shape,
    pub start: i64,
    pub end: i64,
    pub color: usize,
    pub size: f32,
    pub caption_style: CaptionStyle,
    pub caption_box_opacity: f32,
}

impl Item {
    pub fn active_at(&self, time: i64) -> bool {
        self.start <= time && time < self.end
    }
}

/// The window of the recording a drawn frame shows, normalized to the whole
/// source. `Crop::FULL` is the whole thing and is what every path uses until
/// somebody crops.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Crop {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// The smallest fraction of a recording worth keeping. Below this the preview
/// has nothing to show and the encoder nothing to work with.
const MIN_CROP: f32 = 0.02;

impl Crop {
    pub const FULL: Crop = Crop { x: 0.0, y: 0.0, w: 1.0, h: 1.0 };

    /// Corners in the order `resized` counts them: top-left, top-right,
    /// bottom-right, bottom-left.
    pub fn corners(self) -> [(f32, f32); 4] {
        [
            (self.x, self.y),
            (self.x + self.w, self.y),
            (self.x + self.w, self.y + self.h),
            (self.x, self.y + self.h),
        ]
    }

    pub fn contains(self, p: (f32, f32)) -> bool {
        p.0 >= self.x && p.0 <= self.x + self.w && p.1 >= self.y && p.1 <= self.y + self.h
    }

    /// Build a crop from two dragged corners. Clamped inside the recording and
    /// widened to `MIN_CROP` rather than rejected — a slightly-too-small drag
    /// should give a small crop, not nothing.
    pub fn from_points(a: (f32, f32), b: (f32, f32)) -> Crop {
        let span = |p: f32, q: f32| {
            let (low, high) = (p.min(q).clamp(0.0, 1.0), p.max(q).clamp(0.0, 1.0));
            let size = (high - low).max(MIN_CROP);
            ((low).min(1.0 - size), size)
        };
        let (x, w) = span(a.0, b.0);
        let (y, h) = span(a.1, b.1);
        Crop { x, y, w, h }
    }

    /// The crop as a source-pixel rect: `(x, y, width, height)`.
    ///
    /// The one definition of that rounding. The editor reports this size and
    /// the export encodes it, and when they each rounded for themselves they
    /// disagreed by a pixel on odd sources — the readout promised 161 and the
    /// file held 160.
    ///
    /// Real crops are evened because H.264 rejects odd dimensions. `FULL` is
    /// returned untouched: the whole recording is the whole recording, and
    /// evening it there would shave a pixel off an odd source and report
    /// itself as a crop. The encoder evens its own input regardless.
    pub fn pixel_rect(self, width: u32, height: u32) -> (u32, u32, u32, u32) {
        if self == Crop::FULL {
            return (0, 0, width, height);
        }
        let even = |v: u32| (v.max(2)) & !1;
        let span = |origin: f32, size: f32, limit: u32| {
            // An axis with fewer pixels than the even minimum keeps all of
            // them. Asking for two out of one is not merely wrong: the clamp
            // below would be given a floor above its ceiling, and `f32::clamp`
            // panics on that rather than picking one.
            if limit < 2 {
                return (0, limit);
            }
            let limit_f = limit as f32;
            let origin = (origin * limit_f).round().clamp(0.0, limit_f) as u32;
            let size = even((size * limit_f).round().clamp(2.0, limit_f) as u32);
            (origin.min(limit.saturating_sub(size)), size)
        };
        let (x, w) = span(self.x, self.w, width);
        let (y, h) = span(self.y, self.h, height);
        (x, y, w, h)
    }

    /// Slide the crop without letting it leave the recording. The size never
    /// changes, so dragging into an edge stops rather than shrinking.
    pub fn moved(self, dx: f32, dy: f32) -> Crop {
        Crop {
            x: (self.x + dx).clamp(0.0, 1.0 - self.w),
            y: (self.y + dy).clamp(0.0, 1.0 - self.h),
            ..self
        }
    }

    /// Drag one corner to `point`, keeping the opposite one pinned. Returns the
    /// new crop and which corner is now held: dragging past the opposite corner
    /// flips it to the one it crossed to, without which the next move would pin
    /// the wrong point and the rectangle would stick.
    pub fn resized(self, corner: u8, point: (f32, f32)) -> (Crop, u8) {
        let opposite = self.corners()[((corner as usize) + 2) % 4];
        let held = match (point.0 > opposite.0, point.1 > opposite.1) {
            (false, false) => 0,
            (true, false) => 1,
            (true, true) => 2,
            (false, true) => 3,
        };
        (Crop::from_points(opposite, point), held)
    }
}

/// How a drawn image relates to the recording: which window of the source it
/// shows, and how many pixels that window occupies.
///
/// Annotations are stored normalized to the whole recording rather than to
/// whatever is currently framed, so changing the crop moves the picture under
/// them instead of invalidating them — the same model the photo editor uses.
/// This is what converts between the two.
#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub crop: Crop,
    /// Pixel size of the drawn content.
    pub content: (u32, u32),
}

impl Frame {
    fn size(&self) -> (f32, f32) {
        (self.content.0.max(1) as f32, self.content.1.max(1) as f32)
    }

    /// Source-normalized point to a pixel on the drawn content.
    fn to_pixels(self, p: (f32, f32)) -> (f32, f32) {
        let (w, h) = self.size();
        (
            (p.0 - self.crop.x) / self.crop.w.max(f32::EPSILON) * w,
            (p.1 - self.crop.y) / self.crop.h.max(f32::EPSILON) * h,
        )
    }

    /// A pixel distance on the drawn content, back in source-normalized units.
    fn to_normalized(self, dx: f32, dy: f32) -> (f32, f32) {
        let (w, h) = self.size();
        (dx / w * self.crop.w, dy / h * self.crop.h)
    }
}

fn point(point: (f32, f32), frame: Frame) -> (f32, f32) {
    frame.to_pixels(point)
}

fn annotation(item: &Item, frame: Frame) -> crate::annotate::Annotation {
    let shape = match &item.shape {
        Shape::Arrow { from, to } => crate::annotate::Shape::Arrow {
            from: point(*from, frame),
            to: point(*to, frame),
        },
        Shape::Line { from, to } => crate::annotate::Shape::Line {
            from: point(*from, frame),
            to: point(*to, frame),
        },
        Shape::Freehand { points } => crate::annotate::Shape::Freehand {
            points: points.iter().map(|point_| point(*point_, frame)).collect(),
        },
        Shape::Rect { a, b } => crate::annotate::Shape::Rect {
            a: point(*a, frame),
            b: point(*b, frame),
        },
        Shape::Ellipse { a, b } => crate::annotate::Shape::Ellipse {
            a: point(*a, frame),
            b: point(*b, frame),
        },
        Shape::Highlight { a, b } => crate::annotate::Shape::Highlight {
            a: point(*a, frame),
            b: point(*b, frame),
        },
        Shape::Counter { pos, n } => crate::annotate::Shape::Counter {
            pos: point(*pos, frame),
            n: *n,
        },
        Shape::Blur { a, b } => crate::annotate::Shape::Blur {
            a: point(*a, frame),
            b: point(*b, frame),
        },
        Shape::Text { pos, text } => crate::annotate::Shape::Text {
            pos: point(*pos, frame),
            text: text.clone(),
        },
    };
    crate::annotate::Annotation {
        shape,
        color: item.color.min(crate::annotate::COLORS.len() - 1),
        size: item.size,
        text_style: item.caption_style,
        text_box_opacity: item.caption_box_opacity,
    }
}

/// Render annotations relative to the recorded content, even when the output
/// canvas has extra matte padding or a forced aspect ratio.
pub fn render_at(
    image: &mut RgbaImage,
    items: &[Item],
    time: i64,
    skip: Option<usize>,
    frame: Frame,
    offset: (f32, f32),
) {
    for (index, item) in items.iter().enumerate() {
        if Some(index) != skip && item.active_at(time) {
            render_one_at(image, item, frame, offset);
        }
    }
}

/// Render a downscaled editor preview using the source video's annotation
/// metrics. This avoids the minimum-size clamp making captions and strokes
/// jump larger when the playhead switches to a cached scrub frame.
pub fn render_preview_at(
    image: &mut RgbaImage,
    items: &[Item],
    time: i64,
    skip: Option<usize>,
    preview: Frame,
    source_content_size: (u32, u32),
    offset: (f32, f32),
) {
    let effective_metric = preview_metric_scale(preview.content, source_content_size);
    for (index, item) in items.iter().enumerate() {
        if Some(index) != skip && item.active_at(time) {
            render_one_with_metric(image, item, preview, offset, effective_metric);
        }
    }
}

fn preview_metric_scale(
    preview_content_size: (u32, u32),
    source_content_size: (u32, u32),
) -> f32 {
    let source_scale = metric_scale(source_content_size);
    let preview_scale = (preview_content_size.0.max(1) as f32
        / source_content_size.0.max(1) as f32)
        .min(
            preview_content_size.1.max(1) as f32
                / source_content_size.1.max(1) as f32,
        );
    source_scale * preview_scale
}

fn metric_scale(content_size: (u32, u32)) -> f32 {
    (content_size.0.min(content_size.1) as f32 / 720.0).clamp(0.45, 4.0)
}

pub fn render_one_at(image: &mut RgbaImage, item: &Item, frame: Frame, offset: (f32, f32)) {
    render_one_with_metric(image, item, frame, offset, metric_scale(frame.content));
}

fn render_one_with_metric(
    image: &mut RgbaImage,
    item: &Item,
    frame: Frame,
    offset: (f32, f32),
    metric_scale: f32,
) {
    if let Shape::Text { pos, text } = &item.shape {
        crate::annotate::render_caption(
            image,
            point(*pos, frame),
            text,
            crate::annotate::CaptionOptions {
                color_index: item.color,
                size: item.size,
                metric_scale,
                offset,
                boxed: item.caption_style == CaptionStyle::Box,
                box_opacity: item.caption_box_opacity,
            },
        );
        return;
    }
    crate::annotate::render_with_metric(
        image,
        &[annotation(item, frame)],
        1.0,
        metric_scale,
        offset,
        None,
    );
}

/// Bounds in source-normalized coordinates.
///
/// Pixel-sized shapes — the step badge's radius, a caption's text metrics —
/// are measured against the drawn content and converted back through the crop,
/// so a tighter crop makes them cover proportionally more of the recording,
/// which is exactly what is on screen.
pub fn bounds(item: &Item, frame: Frame) -> (f32, f32, f32, f32) {
    let content_size = frame.content;
    match &item.shape {
        Shape::Arrow { from, to } | Shape::Line { from, to } => (
            from.0.min(to.0),
            from.1.min(to.1),
            from.0.max(to.0),
            from.1.max(to.1),
        ),
        Shape::Freehand { points } => points.iter().fold(
            (1.0, 1.0, 0.0, 0.0),
            |(x0, y0, x1, y1), point| {
                (x0.min(point.0), y0.min(point.1), x1.max(point.0), y1.max(point.1))
            },
        ),
        Shape::Rect { a, b }
        | Shape::Ellipse { a, b }
        | Shape::Highlight { a, b }
        | Shape::Blur { a, b } => {
            (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1))
        }
        Shape::Counter { pos, .. } => {
            let radius = (14.0 * metric_scale(content_size) * item.size).max(9.0);
            let (rx, ry) = frame.to_normalized(radius, radius);
            (
                (pos.0 - rx).max(0.0),
                (pos.1 - ry).max(0.0),
                (pos.0 + rx).min(1.0),
                (pos.1 + ry).min(1.0),
            )
        }
        Shape::Text { pos, text } => {
            let (width, height) = crate::annotate::caption_text_size(
                text,
                item.size,
                metric_scale(content_size),
            )
            .map(|(width, height)| frame.to_normalized(width as f32, height as f32))
            .unwrap_or((0.04, 0.035 * item.size));
            (
                pos.0.max(0.0),
                pos.1.max(0.0),
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

pub fn hit(item: &Item, point: (f32, f32), tolerance: f32, frame: Frame) -> bool {
    let content_size = frame.content;
    match &item.shape {
        Shape::Arrow { from, to } | Shape::Line { from, to } => {
            distance_to_segment(point, *from, *to) <= tolerance
        }
        Shape::Freehand { points } => points
            .windows(2)
            .any(|segment| distance_to_segment(point, segment[0], segment[1]) <= tolerance),
        Shape::Blur { .. } | Shape::Highlight { .. } | Shape::Text { .. } => {
            let (x0, y0, x1, y1) = bounds(item, frame);
            point.0 >= x0 - tolerance
                && point.0 <= x1 + tolerance
                && point.1 >= y0 - tolerance
                && point.1 <= y1 + tolerance
        }
        Shape::Rect { .. } => {
            let (x0, y0, x1, y1) = bounds(item, frame);
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
        Shape::Ellipse { a, b } => {
            let (cx, cy) = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
            let (rx, ry) = (
                ((a.0 - b.0) / 2.0).abs().max(f32::EPSILON),
                ((a.1 - b.1) / 2.0).abs().max(f32::EPSILON),
            );
            let value = (((point.0 - cx) / rx).powi(2) + ((point.1 - cy) / ry).powi(2)).sqrt();
            (value - 1.0).abs() * rx.min(ry) <= tolerance * 1.5
        }
        Shape::Counter { pos, .. } => {
            // Measured in drawn pixels so the grab target stays circular on a
            // wide recording, and through the crop so it keeps matching what
            // is on screen once the frame narrows.
            let (width, height) = (content_size.0.max(1) as f32, content_size.1.max(1) as f32);
            let radius = (14.0 * metric_scale(content_size) * item.size).max(9.0);
            let from_centre = frame.to_pixels(point);
            let centre = frame.to_pixels(*pos);
            let (dx, dy) = (from_centre.0 - centre.0, from_centre.1 - centre.1);
            dx.hypot(dy) <= radius + tolerance * width.min(height)
        }
    }
}

pub fn translate(item: &mut Item, dx: f32, dy: f32, frame: Frame) {
    let (x0, y0, x1, y1) = bounds(item, frame);
    let dx = dx.clamp(-x0, 1.0 - x1);
    let dy = dy.clamp(-y0, 1.0 - y1);
    let move_point = |point: &mut (f32, f32)| {
        point.0 += dx;
        point.1 += dy;
    };
    match &mut item.shape {
        Shape::Arrow { from, to } | Shape::Line { from, to } => {
            move_point(from);
            move_point(to);
        }
        Shape::Freehand { points } => points.iter_mut().for_each(move_point),
        Shape::Rect { a, b }
        | Shape::Ellipse { a, b }
        | Shape::Highlight { a, b }
        | Shape::Blur { a, b } => {
            move_point(a);
            move_point(b);
        }
        Shape::Counter { pos, .. } | Shape::Text { pos, .. } => move_point(pos),
    }
}

pub fn handles(item: &Item) -> Option<[(ShapeHandle, (f32, f32)); 2]> {
    match &item.shape {
        Shape::Arrow { from, to } | Shape::Line { from, to } => Some([
            (ShapeHandle::First, *from),
            (ShapeHandle::Second, *to),
        ]),
        Shape::Rect { a, b }
        | Shape::Ellipse { a, b }
        | Shape::Highlight { a, b }
        | Shape::Blur { a, b } => Some([
            (ShapeHandle::First, *a),
            (ShapeHandle::Second, *b),
        ]),
        Shape::Freehand { .. } | Shape::Counter { .. } | Shape::Text { .. } => None,
    }
}

/// Hit-test a direct manipulation handle in content-aware pixel space. This
/// keeps the grab target circular on both wide videos and portrait captures.
pub fn hit_handle(
    item: &Item,
    point: (f32, f32),
    radius: f32,
    frame: Frame,
) -> Option<ShapeHandle> {
    let (width, height) = (frame.content.0.max(1) as f32, frame.content.1.max(1) as f32);
    let radius_px = radius * width.min(height);
    let at = frame.to_pixels(point);
    handles(item)?.into_iter().find_map(|(handle, candidate)| {
        let candidate = frame.to_pixels(candidate);
        let (dx, dy) = (at.0 - candidate.0, at.1 - candidate.1);
        (dx.hypot(dy) <= radius_px).then_some(handle)
    })
}

pub fn set_handle(item: &mut Item, handle: ShapeHandle, point: (f32, f32)) {
    let point = (point.0.clamp(0.0, 1.0), point.1.clamp(0.0, 1.0));
    match (&mut item.shape, handle) {
        (Shape::Arrow { from, .. }, ShapeHandle::First)
        | (Shape::Line { from, .. }, ShapeHandle::First)
        | (Shape::Rect { a: from, .. }, ShapeHandle::First)
        | (Shape::Ellipse { a: from, .. }, ShapeHandle::First)
        | (Shape::Highlight { a: from, .. }, ShapeHandle::First)
        | (Shape::Blur { a: from, .. }, ShapeHandle::First) => *from = point,
        (Shape::Arrow { to, .. }, ShapeHandle::Second)
        | (Shape::Line { to, .. }, ShapeHandle::Second)
        | (Shape::Rect { b: to, .. }, ShapeHandle::Second)
        | (Shape::Ellipse { b: to, .. }, ShapeHandle::Second)
        | (Shape::Highlight { b: to, .. }, ShapeHandle::Second)
        | (Shape::Blur { b: to, .. }, ShapeHandle::Second) => *to = point,
        (Shape::Freehand { .. } | Shape::Counter { .. } | Shape::Text { .. }, _) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{
        bounds, handles, hit, hit_handle, metric_scale, preview_metric_scale, render_at,
        set_handle, translate, CaptionStyle, Crop, Frame, Item, Shape, ShapeHandle,
    };
    use image::{Rgba, RgbaImage};

    /// The whole recording at `content` pixels — what every caller passed
    /// before cropping existed, and the baseline these tests compare against.
    fn whole(content: (u32, u32)) -> Frame {
        Frame { crop: Crop::FULL, content }
    }

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
            caption_box_opacity: 0.68,
        }
    }

    #[test]
    fn the_pixel_rect_is_what_both_the_editor_and_the_encoder_get() {
        // The whole recording is handed back untouched, odd dimensions and
        // all: evening it here would shave a pixel off an odd source and
        // report itself as a crop.
        assert_eq!(Crop::FULL.pixel_rect(1920, 1080), (0, 0, 1920, 1080));
        assert_eq!(Crop::FULL.pixel_rect(321, 241), (0, 0, 321, 241));

        // A real crop is evened, because H.264 rejects odd dimensions — and
        // this is the only place that decides it, so the size the editor shows
        // is the size the export encodes. Half of 321 rounds to 161, which
        // must come back as 160 rather than the editor promising a pixel the
        // encoder was never going to keep.
        let (x, y, w, h) = Crop { x: 0.0, y: 0.0, w: 0.5, h: 0.5 }.pixel_rect(321, 241);
        assert_eq!((x, y), (0, 0));
        assert_eq!((w, h), (160, 120));
        assert_eq!((w % 2, h % 2), (0, 0));

        // A source with fewer pixels than the even minimum keeps what it has.
        // Two out of one is not just wrong: it asks `f32::clamp` for a floor
        // above its ceiling, which panics.
        for limit in [0u32, 1, 2, 3] {
            let (x, y, w, h) =
                Crop { x: 0.0, y: 0.0, w: 0.5, h: 0.5 }.pixel_rect(limit, limit);
            assert!(
                x + w <= limit && y + h <= limit,
                "a {limit}px source gave {w}x{h} at {x},{y}"
            );
        }
        // Two pixels is the first size that can satisfy the even minimum.
        assert_eq!(Crop { x: 0.0, y: 0.0, w: 0.5, h: 0.5 }.pixel_rect(1, 1), (0, 0, 1, 1));
        assert_eq!(Crop { x: 0.0, y: 0.0, w: 0.5, h: 0.5 }.pixel_rect(2, 2), (0, 0, 2, 2));

        // Never hanging off an edge, however the floats round.
        let (x, y, w, h) = Crop { x: 0.9, y: 0.9, w: 0.2, h: 0.2 }.pixel_rect(641, 481);
        assert!(x + w <= 641 && y + h <= 481, "crop {x},{y} {w}x{h} left the frame");
        // And always something encodable.
        let (_, _, w, h) = Crop { x: 0.0, y: 0.0, w: 0.0, h: 0.0 }.pixel_rect(320, 240);
        assert!(w >= 2 && h >= 2);
    }

    #[test]
    fn a_swept_crop_normalizes_and_stays_inside_the_recording() {
        // Dragged up and to the left, and past the edges.
        assert_eq!(
            Crop::from_points((0.9, 0.7), (-0.5, -0.3)),
            Crop { x: 0.0, y: 0.0, w: 0.9, h: 0.7 }
        );
        let corner = Crop::from_points((0.6, 0.4), (9.0, 9.0));
        assert!((corner.x - 0.6).abs() < 1e-6 && (corner.w - 0.4).abs() < 1e-6);
        assert!((corner.y - 0.4).abs() < 1e-6 && (corner.h - 0.6).abs() < 1e-6);

        // A flick gives a small crop, nudged back inside rather than hanging
        // off the far edge.
        let flick = Crop::from_points((1.0, 1.0), (1.0, 1.0));
        assert!(flick.w >= super::MIN_CROP && flick.h >= super::MIN_CROP);
        assert!(flick.x + flick.w <= 1.0 + 1e-6 && flick.y + flick.h <= 1.0 + 1e-6);
    }

    #[test]
    fn dragging_a_crop_into_an_edge_stops_it_instead_of_resizing_it() {
        let crop = Crop { x: 0.1, y: 0.1, w: 0.4, h: 0.3 };
        let moved = crop.moved(0.05, -0.04);
        assert!((moved.x - 0.15).abs() < 1e-6 && (moved.y - 0.06).abs() < 1e-6);
        assert_eq!((moved.w, moved.h), (crop.w, crop.h));

        let pinned = crop.moved(9.0, 9.0);
        assert!((pinned.x - 0.6).abs() < 1e-6 && (pinned.y - 0.7).abs() < 1e-6);
        assert_eq!((pinned.w, pinned.h), (crop.w, crop.h));
        let pinned = crop.moved(-9.0, -9.0);
        assert!(pinned.x.abs() < 1e-6 && pinned.y.abs() < 1e-6);
    }

    #[test]
    fn resizing_a_crop_pins_the_opposite_corner_and_survives_crossing_it() {
        let crop = Crop { x: 0.1, y: 0.1, w: 0.4, h: 0.3 };
        // Corner 0 is the top-left; (0.5, 0.4) stays put while it is dragged.
        let (resized, held) = crop.resized(0, (0.2, 0.25));
        assert!((resized.x - 0.2).abs() < 1e-6 && (resized.y - 0.25).abs() < 1e-6);
        assert!((resized.w - 0.3).abs() < 1e-6 && (resized.h - 0.15).abs() < 1e-6);
        assert_eq!(held, 0);

        // Dragged past the pinned corner it becomes the bottom-right one, so
        // the next move still pins (0.5, 0.4).
        let (crossed, held) = crop.resized(0, (0.7, 0.6));
        assert_eq!(held, 2);
        assert!((crossed.x - 0.5).abs() < 1e-6 && (crossed.y - 0.4).abs() < 1e-6);
        let (again, _) = crossed.resized(held, (0.8, 0.7));
        assert!((again.x - 0.5).abs() < 1e-6 && (again.w - 0.3).abs() < 1e-6);
    }

    #[test]
    fn an_uncropped_frame_maps_exactly_as_a_bare_content_size_did() {
        // The whole point of `Crop::FULL`: every existing path keeps its
        // arithmetic to the pixel, so nothing moves until somebody crops.
        let frame = whole((1920, 1080));
        assert_eq!(frame.crop, Crop::FULL);
        for p in [(0.0, 0.0), (0.25, 0.75), (1.0, 1.0), (0.5, 0.5)] {
            let mapped = frame.to_pixels(p);
            assert!((mapped.0 - p.0 * 1920.0).abs() < 0.001);
            assert!((mapped.1 - p.1 * 1080.0).abs() < 0.001);
        }
        let (nx, ny) = frame.to_normalized(192.0, 108.0);
        assert!((nx - 0.1).abs() < 0.0001 && (ny - 0.1).abs() < 0.0001);
    }

    #[test]
    fn a_crop_reframes_source_coordinates_onto_the_drawn_content() {
        // The middle quarter of the recording, drawn at its own pixel size.
        let frame = Frame {
            crop: Crop { x: 0.25, y: 0.25, w: 0.5, h: 0.5 },
            content: (960, 540),
        };
        assert_ne!(frame.crop, Crop::FULL);

        // The crop's top-left is the content's origin, its centre the middle.
        let origin = frame.to_pixels((0.25, 0.25));
        assert!(origin.0.abs() < 0.001 && origin.1.abs() < 0.001);
        let centre = frame.to_pixels((0.5, 0.5));
        assert!((centre.0 - 480.0).abs() < 0.001 && (centre.1 - 270.0).abs() < 0.001);

        // A point outside the crop maps outside the content rather than being
        // clamped — annotations keep their place in the recording and the
        // renderer clips whatever now falls off.
        let above = frame.to_pixels((0.25, 0.0));
        assert!(above.1 < 0.0, "a point above the crop should map above it");

        // Pixel-sized things shrink into a proportionally larger share of the
        // recording, which is what a tighter crop looks like on screen.
        let (nx, ny) = frame.to_normalized(96.0, 54.0);
        assert!((nx - 0.05).abs() < 0.0001 && (ny - 0.05).abs() < 0.0001);
    }

    #[test]
    fn a_cropped_render_places_an_annotation_where_the_crop_puts_it() {
        // An arrow across the middle of the recording. Cropped to the middle
        // quarter it must still cross the middle of the drawn frame, not sit
        // where the uncropped coordinates would have put it.
        let item = Item {
            shape: Shape::Rect { a: (0.45, 0.45), b: (0.55, 0.55) },
            start: 0,
            end: 20,
            color: 0,
            size: 1.0,
            caption_style: CaptionStyle::Shadow,
            caption_box_opacity: 0.68,
        };
        let mut image = RgbaImage::from_pixel(200, 200, Rgba([0, 0, 0, 255]));
        render_at(
            &mut image,
            std::slice::from_ref(&item),
            10,
            None,
            Frame { crop: Crop { x: 0.25, y: 0.25, w: 0.5, h: 0.5 }, content: (200, 200) },
            (0.0, 0.0),
        );

        // 0.45..0.55 of the source is 0.4..0.6 of this crop — pixels 80..120.
        let painted: Vec<(u32, u32)> = image
            .enumerate_pixels()
            .filter(|(_, _, pixel)| pixel[0] > 0 || pixel[1] > 0 || pixel[2] > 0)
            .map(|(x, y, _)| (x, y))
            .collect();
        assert!(!painted.is_empty(), "nothing was drawn");
        let (min_x, max_x) = (
            painted.iter().map(|(x, _)| *x).min().unwrap(),
            painted.iter().map(|(x, _)| *x).max().unwrap(),
        );
        assert!(min_x >= 70 && max_x <= 130, "drawn at {min_x}..{max_x}, expected ~80..120");
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
    fn added_video_shapes_render_and_hit_test_through_the_shared_renderer() {
        let shapes = [
            (
                Shape::Line {
                    from: (0.15, 0.2),
                    to: (0.85, 0.8),
                },
                (0.5, 0.5),
            ),
            (
                Shape::Freehand {
                    points: vec![(0.15, 0.2), (0.5, 0.75), (0.85, 0.2)],
                },
                (0.5, 0.75),
            ),
            (
                Shape::Ellipse {
                    a: (0.2, 0.2),
                    b: (0.8, 0.8),
                },
                (0.5, 0.2),
            ),
            (
                Shape::Highlight {
                    a: (0.2, 0.2),
                    b: (0.8, 0.8),
                },
                (0.5, 0.5),
            ),
            (
                Shape::Counter {
                    pos: (0.5, 0.5),
                    n: 3,
                },
                (0.5, 0.5),
            ),
        ];

        for (shape, hit_point) in shapes {
            let item = Item {
                shape,
                start: 0,
                end: 20,
                color: 0,
                size: 1.0,
                caption_style: CaptionStyle::Shadow,
                caption_box_opacity: 0.68,
            };
            let mut image = RgbaImage::from_pixel(320, 180, Rgba([0, 0, 0, 255]));
            render_at(
                &mut image,
                std::slice::from_ref(&item),
                10,
                None,
                whole((320, 180)),
                (0.0, 0.0),
            );
            assert!(image.pixels().any(|pixel| pixel[0] > 0));
            assert!(hit(&item, hit_point, 0.03, whole((320, 180))));
        }
    }

    #[test]
    fn freehand_paths_move_as_one_annotation() {
        let mut item = Item {
            shape: Shape::Freehand {
                points: vec![(0.1, 0.2), (0.3, 0.4), (0.5, 0.2)],
            },
            start: 0,
            end: 20,
            color: 0,
            size: 1.0,
            caption_style: CaptionStyle::Shadow,
            caption_box_opacity: 0.68,
        };
        translate(&mut item, 0.2, 0.3, whole((1920, 1080)));
        let Shape::Freehand { points } = item.shape else {
            panic!("expected freehand path");
        };
        for (actual, expected) in points
            .iter()
            .zip([(0.3, 0.5), (0.5, 0.7), (0.7, 0.5)])
        {
            assert!((actual.0 - expected.0).abs() < 1e-6);
            assert!((actual.1 - expected.1).abs() < 1e-6);
        }
    }

    #[test]
    fn arrow_hit_testing_and_translation_use_normalized_space() {
        let mut item = arrow();
        assert!(hit(&item, (0.3, 0.4), 0.01, whole((1920, 1080))));
        assert!(!hit(&item, (0.8, 0.2), 0.01, whole((1920, 1080))));
        translate(&mut item, 0.7, 0.7, whole((1920, 1080)));
        let (x0, y0, x1, y1) = bounds(&item, whole((1920, 1080)));
        for (actual, expected) in [x0, y0, x1, y1].into_iter().zip([0.6, 0.6, 1.0, 1.0]) {
            assert!((actual - expected).abs() < 0.00001);
        }
    }

    #[test]
    fn arrow_endpoints_can_be_redirected_without_moving_the_other_end() {
        let mut item = arrow();
        assert_eq!(
            hit_handle(&item, (0.102, 0.202), 0.025, whole((1920, 1080))),
            Some(ShapeHandle::First)
        );
        assert_eq!(
            hit_handle(&item, (0.498, 0.598), 0.025, whole((1920, 1080))),
            Some(ShapeHandle::Second)
        );
        assert_eq!(handles(&item).unwrap()[1].1, (0.5, 0.6));

        set_handle(&mut item, ShapeHandle::First, (0.8, -0.2));
        let Shape::Arrow { from, to } = item.shape else {
            panic!("expected arrow");
        };
        assert_eq!(from, (0.8, 0.0));
        assert_eq!(to, (0.5, 0.6));
    }

    #[test]
    fn box_and_blur_corner_handles_resize_in_place() {
        for shape in [
            Shape::Rect {
                a: (0.2, 0.3),
                b: (0.6, 0.7),
            },
            Shape::Blur {
                a: (0.2, 0.3),
                b: (0.6, 0.7),
            },
        ] {
            let mut item = Item {
                shape,
                start: 0,
                end: 10,
                color: 0,
                size: 1.0,
                caption_style: CaptionStyle::Shadow,
                caption_box_opacity: 0.68,
            };
            set_handle(&mut item, ShapeHandle::Second, (0.9, 0.1));
            assert_eq!(handles(&item).unwrap()[0].1, (0.2, 0.3));
            assert_eq!(handles(&item).unwrap()[1].1, (0.9, 0.1));
        }
    }

    #[test]
    fn scrub_preview_metrics_scale_from_the_source_without_a_thumbnail_floor() {
        let source = (1920, 1080);
        let half = preview_metric_scale((960, 540), source);
        let thumbnail = preview_metric_scale((160, 90), source);
        assert!((half - 0.75).abs() < 0.0001);
        assert!((thumbnail - 0.125).abs() < 0.0001);
        assert!((half / 960.0 - thumbnail / 160.0).abs() < 0.0001);
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
            caption_box_opacity: 0.68,
        };
        let mut image = RgbaImage::from_pixel(200, 200, Rgba([0, 0, 0, 255]));
        render_at(&mut image, &[item], 10, None, whole((100, 100)), (50.0, 50.0));

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
            caption_box_opacity: 0.68,
        };
        let mut image = RgbaImage::from_pixel(1200, 600, Rgba([0, 0, 0, 255]));
        render_at(
            &mut image,
            &[item],
            10,
            None,
            whole((1000, 400)),
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
            caption_box_opacity: 0.68,
        };
        render_at(
            &mut boxed,
            std::slice::from_ref(&item),
            10,
            None,
            whole((400, 200)),
            (0.0, 0.0),
        );
        let mut shadow_item = item;
        shadow_item.caption_style = CaptionStyle::Shadow;
        render_at(&mut shadow, &[shadow_item], 10, None, whole((400, 200)), (0.0, 0.0));

        // The box extends left of the text origin; shadow-only text does not.
        assert!(boxed.get_pixel(94, 52)[0] < shadow.get_pixel(94, 52)[0]);
    }

    #[test]
    fn caption_selection_bounds_match_the_rendered_text_metrics() {
        let content_size = (1920, 1036);
        let item = Item {
            shape: Shape::Text {
                pos: (0.15, 0.2),
                text: "BATTLE".into(),
            },
            start: 0,
            end: 20,
            color: 3,
            size: 3.4,
            caption_style: CaptionStyle::Box,
            caption_box_opacity: 0.68,
        };
        let (x0, y0, x1, y1) = bounds(&item, whole(content_size));
        let (text_w, text_h) = crate::annotate::caption_text_size(
            "BATTLE",
            item.size,
            metric_scale(content_size),
        )
        .expect("caption metrics");
        assert!(((x1 - x0) * content_size.0 as f32 - text_w as f32).abs() < 1.0);
        assert!(((y1 - y0) * content_size.1 as f32 - text_h as f32).abs() < 1.0);
    }

    #[test]
    fn caption_box_opacity_changes_only_the_plate_strength() {
        let background = RgbaImage::from_pixel(400, 200, Rgba([230, 230, 230, 255]));
        let mut light = background.clone();
        let mut solid = background;
        let mut item = Item {
            shape: Shape::Text {
                pos: (0.25, 0.25),
                text: "Caption".into(),
            },
            start: 0,
            end: 20,
            color: 3,
            size: 2.0,
            caption_style: CaptionStyle::Box,
            caption_box_opacity: 0.25,
        };
        render_at(
            &mut light,
            std::slice::from_ref(&item),
            10,
            None,
            whole((400, 200)),
            (0.0, 0.0),
        );
        item.caption_box_opacity = 0.90;
        render_at(&mut solid, &[item], 10, None, whole((400, 200)), (0.0, 0.0));

        // This pixel sits on the plate padding, outside the caption glyphs.
        assert!(solid.get_pixel(95, 52)[0] < light.get_pixel(95, 52)[0]);
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
            caption_box_opacity: 0.68,
        };
        translate(&mut item, 0.15, 0.1, whole((1920, 1080)));
        translate(&mut item, 0.1, 0.2, whole((1920, 1080)));
        let Shape::Text { pos, .. } = item.shape else {
            panic!("caption changed shape");
        };
        assert!((pos.0 - 0.45).abs() < 0.0001);
        assert!((pos.1 - 0.5).abs() < 0.0001);
    }
}
