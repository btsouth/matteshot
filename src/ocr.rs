//! Copy-text: OCR via the OS-provided Windows.Media.Ocr engine. Fully
//! offline, uses the user's installed language packs, costs us nothing.

use anyhow::{bail, Context, Result};
use image::RgbaImage;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;

/// Recognize text in the capture and put it on the clipboard as text.
pub fn copy_text(img: &RgbaImage) -> Result<()> {
    let text = recognize(img)?;
    crate::output::text_to_clipboard(&text)?;
    eprintln!("ocr: {} chars copied", text.len());
    Ok(())
}

/// One recognized word, positioned in the coordinate space of the image that
/// was handed to the engine (so: raw-capture pixels, the same space
/// annotations live in).
#[derive(Clone)]
pub struct Word {
    pub text: String,
    /// x0, y0, x1, y1.
    pub rect: (f32, f32, f32, f32),
    /// Which recognized line this word came from. Lines arrive top to bottom,
    /// words left to right, so `Vec<Word>` order is reading order.
    pub line: usize,
}

/// The engine caps input dimensions, so oversized captures get downscaled.
/// Returns the bitmap and the factor applied, for mapping results back.
fn to_bitmap(img: &RgbaImage) -> Result<(SoftwareBitmap, f32)> {
    let max_dim = OcrEngine::MaxImageDimension().unwrap_or(2600);
    let (w, h) = (img.width(), img.height());
    let scaled;
    let (src, scale): (&RgbaImage, f32) = if w.max(h) > max_dim {
        let f = max_dim as f32 / w.max(h) as f32;
        scaled = image::imageops::resize(
            img,
            (w as f32 * f) as u32,
            (h as f32 * f) as u32,
            image::imageops::FilterType::CatmullRom,
        );
        (&scaled, f)
    } else {
        (img, 1.0)
    };

    // RgbaImage -> IBuffer -> SoftwareBitmap.
    let writer = DataWriter::new()?;
    writer.WriteBytes(src.as_raw())?;
    let buffer = writer.DetachBuffer()?;
    let bitmap = SoftwareBitmap::CreateCopyFromBuffer(
        &buffer,
        BitmapPixelFormat::Rgba8,
        src.width() as i32,
        src.height() as i32,
    )?;
    Ok((bitmap, scale))
}

/// Every recognized word with its box, for selecting text straight off the
/// preview. Empty result means the engine ran and found nothing.
pub fn recognize_words(img: &RgbaImage) -> Result<Vec<Word>> {
    let engine = OcrEngine::TryCreateFromUserProfileLanguages().context("create OCR engine")?;
    let (bitmap, scale) = to_bitmap(img)?;
    let result = engine.RecognizeAsync(&bitmap)?.get().context("recognize")?;
    let inverse = 1.0 / scale;
    let lines = result.Lines()?;
    let mut words = Vec::new();
    for (index, line) in (&lines).into_iter().enumerate() {
        for word in &line.Words()? {
            let text = word.Text()?.to_string_lossy();
            if text.trim().is_empty() {
                continue;
            }
            let r = word.BoundingRect()?;
            words.push(Word {
                text,
                rect: (
                    r.X * inverse,
                    r.Y * inverse,
                    (r.X + r.Width) * inverse,
                    (r.Y + r.Height) * inverse,
                ),
                line: index,
            });
        }
    }
    Ok(words)
}

pub fn recognize(img: &RgbaImage) -> Result<String> {
    let engine = OcrEngine::TryCreateFromUserProfileLanguages().context("create OCR engine")?;
    let (bitmap, _) = to_bitmap(img)?;
    let result = engine.RecognizeAsync(&bitmap)?.get().context("recognize")?;
    let lines = result.Lines()?;
    let mut text = String::new();
    for line in &lines {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&line.Text()?.to_string_lossy());
    }

    if text.trim().is_empty() {
        bail!("no text found in the capture");
    }
    Ok(text)
}
