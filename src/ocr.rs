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

pub fn recognize(img: &RgbaImage) -> Result<String> {
    let engine = OcrEngine::TryCreateFromUserProfileLanguages()
        .context("create OCR engine")?;

    // The engine caps input dimensions; downscale oversized captures.
    let max_dim = OcrEngine::MaxImageDimension().unwrap_or(2600);
    let (w, h) = (img.width(), img.height());
    let scaled;
    let src: &RgbaImage = if w.max(h) > max_dim {
        let f = max_dim as f32 / w.max(h) as f32;
        scaled = image::imageops::resize(
            img,
            (w as f32 * f) as u32,
            (h as f32 * f) as u32,
            image::imageops::FilterType::CatmullRom,
        );
        &scaled
    } else {
        img
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
