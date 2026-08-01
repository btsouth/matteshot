//! Trim support: probe an MP4 for duration + filmstrip thumbnails, and cut
//! a frame-accurate range into a new file (decode → re-encode through the
//! same sink pipeline the recorder uses).

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use image::RgbaImage;
use rayon::prelude::*;
use windows::core::{HSTRING, PROPVARIANT};
use windows::Win32::Media::MediaFoundation::{
    IMFSourceReader, MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Audio,
    MFMediaType_Video, MFStartup, MFVideoFormat_RGB32, MFSTARTUP_FULL, MF_MT_AUDIO_NUM_CHANNELS,
    MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE,
    MF_MT_SUBTYPE, MF_PD_DURATION, MF_SOURCE_READER_FIRST_AUDIO_STREAM,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_SOURCE_READER_MEDIASOURCE, MF_VERSION,
};

pub struct Probe {
    pub duration_100ns: i64,
    /// BGRA thumbnails: (pixels, w, h).
    pub thumbs: Vec<(Vec<u8>, u32, u32)>,
    /// Higher-quality BGRA frames used while scrubbing in the editor.
    pub previews: Vec<(Vec<u8>, u32, u32)>,
    pub source_size: (u32, u32),
}

pub struct PlaybackFrame {
    /// Tightly packed BGRA pixels sized for the editor preview.
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub timestamp: i64,
}

fn fit_inside(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let (w, h) = (w.max(1), h.max(1));
    let scale = (max_w.max(1) as f64 / w as f64)
        .min(max_h.max(1) as f64 / h as f64)
        .min(1.0);
    (
        (w as f64 * scale).round().max(1.0) as u32,
        (h as f64 * scale).round().max(1.0) as u32,
    )
}

fn scrub_cache_plan(
    duration: i64,
    source_size: (u32, u32),
    preview_bounds: (u32, u32),
) -> (usize, u32, u32) {
    const SCRUB_CACHE_BUDGET: u64 = 96 * 1024 * 1024;
    let seconds = (duration.max(1) as f64 / 10_000_000.0).max(1.0);
    let count = ((seconds * 2.0).ceil() as usize).clamp(24, 96);
    let (mut width, mut height) = fit_inside(
        source_size.0,
        source_size.1,
        preview_bounds.0.max(2),
        preview_bounds.1.max(2),
    );
    let allowed_pixels = (SCRUB_CACHE_BUDGET / count as u64 / 4).max(1) as f64;
    let requested_pixels = width as f64 * height as f64;
    if requested_pixels > allowed_pixels {
        let scale = (allowed_pixels / requested_pixels).sqrt();
        width = (width as f64 * scale).floor().max(2.0) as u32;
        height = (height as f64 * scale).floor().max(2.0) as u32;
    }
    (count, width, height)
}

fn open_reader(path: &Path, with_audio: bool) -> Result<(IMFSourceReader, u32, u32, i32)> {
    unsafe {
        // Without video processing the reader only emits the native format
        // (NV12); this lets it convert to RGB32 for us.
        use windows::Win32::Media::MediaFoundation::{
            MFCreateAttributes, MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING,
        };
        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 1)?;
        let attrs = attrs.context("reader attributes")?;
        attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
        let reader: IMFSourceReader =
            MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attrs)
                .context("open reader")?;

        let vt = MFCreateMediaType()?;
        vt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        vt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        reader
            .SetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, None, &vt)
            .context("set video decode type")?;

        if with_audio {
            use windows::Win32::Media::MediaFoundation::MFAudioFormat_PCM;
            let at = MFCreateMediaType()?;
            at.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            at.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            // Missing audio stream is fine.
            let _ =
                reader.SetCurrentMediaType(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32, None, &at);
        }

        let cur = reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)?;
        let size = cur.GetUINT64(&MF_MT_FRAME_SIZE)?;
        let (w, h) = ((size >> 32) as u32, (size & 0xFFFF_FFFF) as u32);
        let stride = cur
            .GetUINT32(&MF_MT_DEFAULT_STRIDE)
            .map(|v| v as i32)
            .unwrap_or((w * 4) as i32);
        Ok((reader, w, h, stride))
    }
}

fn duration_of(reader: &IMFSourceReader) -> Result<i64> {
    unsafe {
        let pv: PROPVARIANT = reader
            .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)?;
        Ok(i64::try_from(&pv).unwrap_or(0))
    }
}

fn read_video_frame(
    reader: &IMFSourceReader,
    w: u32,
    h: u32,
    stride: i32,
) -> Result<Option<(Vec<u8>, i64)>> {
    unsafe {
        loop {
            let mut stream = 0u32;
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut sample = None;
            reader.ReadSample(
                MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                0,
                Some(&mut stream),
                Some(&mut flags),
                Some(&mut ts),
                Some(&mut sample),
            )?;
            if flags & 0x2 != 0 {
                // MF_SOURCE_READERF_ENDOFSTREAM
                return Ok(None);
            }
            let Some(sample) = sample else { continue };
            let buf = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buf.Lock(&mut ptr, None, Some(&mut len))?;
            // Normalize to top-down tightly packed BGRA.
            let abs_stride = stride.unsigned_abs() as usize;
            let mut out = vec![0u8; (w * h * 4) as usize];
            for y in 0..h as usize {
                let src_row = if stride < 0 { h as usize - 1 - y } else { y };
                let src = std::slice::from_raw_parts(
                    (ptr as *const u8).add(src_row * abs_stride),
                    (w * 4) as usize,
                );
                out[y * (w * 4) as usize..(y + 1) * (w * 4) as usize].copy_from_slice(src);
            }
            buf.Unlock()?;
            return Ok(Some((out, ts)));
        }
    }
}

/// Decode one representative frame at `position` and downscale it for the
/// editor preview. The export path always uses the original decoded pixels.
pub fn preview_frame(
    path: &Path,
    position: i64,
    max_w: u32,
    max_h: u32,
) -> Result<(Vec<u8>, u32, u32)> {
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL).ok() };
    let (reader, w, h, stride) = open_reader(path, false)?;
    unsafe {
        let pv = PROPVARIANT::from(position.max(0));
        reader
            .SetCurrentPosition(&windows::core::GUID::zeroed(), &pv)
            .context("seek preview frame")?;
    }
    let (bgra, _) = read_video_frame(&reader, w, h, stride)?
        .context("video has no frame at the requested position")?;
    let rgba = bgra_to_rgba(&bgra, w, h);
    let (target_w, target_h) = fit_inside(w, h, max_w.max(2), max_h.max(2));
    let scaled = image::imageops::resize(
        &rgba,
        target_w,
        target_h,
        image::imageops::FilterType::Triangle,
    );
    let (sw, sh) = scaled.dimensions();
    Ok((rgba_to_bgra(&scaled, sw, sh), sw, sh))
}

/// Sequential, wall-clock-paced preview decode for the in-app video editor.
/// Frames are decoded on a worker thread; late frames are dropped so the UI
/// follows media time instead of accumulating an ever-growing message queue.
pub fn playback_frames(
    path: &Path,
    start: i64,
    end: i64,
    max_w: u32,
    max_h: u32,
    cancel: &AtomicBool,
    mut deliver: impl FnMut(PlaybackFrame) -> bool,
) -> Result<()> {
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL).ok() };
    let (reader, w, h, stride) = open_reader(path, false)?;
    unsafe {
        let position = PROPVARIANT::from(start.max(0));
        reader
            .SetCurrentPosition(&windows::core::GUID::zeroed(), &position)
            .context("seek playback preview")?;
    }

    let wall_start = std::time::Instant::now();
    while !cancel.load(Ordering::Relaxed) {
        let Some((bgra, timestamp)) = read_video_frame(&reader, w, h, stride)? else {
            break;
        };
        if timestamp < start {
            continue;
        }
        if timestamp >= end {
            break;
        }

        let media_elapsed = std::time::Duration::from_nanos(
            timestamp.saturating_sub(start).max(0) as u64 * 100,
        );
        let due = wall_start + media_elapsed;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            let now = std::time::Instant::now();
            if now >= due {
                break;
            }
            std::thread::sleep((due - now).min(std::time::Duration::from_millis(5)));
        }

        // When decode falls behind, keep consuming until media time catches
        // up. Posting every late frame would make Pause feel delayed.
        if std::time::Instant::now().saturating_duration_since(due)
            > std::time::Duration::from_millis(90)
        {
            continue;
        }

        let rgba = bgra_to_rgba(&bgra, w, h);
        let (target_w, target_h) = fit_inside(w, h, max_w.max(2), max_h.max(2));
        let scaled = image::imageops::resize(
            &rgba,
            target_w,
            target_h,
            image::imageops::FilterType::Triangle,
        );
        let (width, height) = scaled.dimensions();
        if !deliver(PlaybackFrame {
            bytes: rgba_to_bgra(&scaled, width, height),
            width,
            height,
            timestamp,
        }) {
            break;
        }
    }
    Ok(())
}

/// Duration + filmstrip thumbnails sized to tile `strip_w` x `strip_h` at
/// the video's own aspect ratio (stretching frames to fixed cells makes the
/// strip look wrong).
fn probe_impl(
    path: &Path,
    strip_w: u32,
    thumb_h: u32,
    preview_bounds: Option<(u32, u32)>,
) -> Result<Probe> {
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL).ok() };
    let (reader, w, h, stride) = open_reader(path, false)?;
    let duration = duration_of(&reader)?;
    let mut thumbs = Vec::new();
    let mut previews = Vec::new();
    let tw = ((w as f32 * thumb_h as f32 / h as f32) as u32).max(2);
    // Enough frames to fill the strip, capped so probing stays quick.
    let n_thumbs = ((strip_w as f32 / tw as f32).ceil() as usize).clamp(3, 24);

    for i in 0..n_thumbs {
        let pos = duration * i as i64 / n_thumbs as i64;
        unsafe {
            let pv = PROPVARIANT::from(pos);
            if reader
                .SetCurrentPosition(&windows::core::GUID::zeroed(), &pv)
                .is_err()
            {
                break;
            }
        }
        let Some((bgra, _)) = read_video_frame(&reader, w, h, stride)? else {
            break;
        };
        // Nearest-neighbor downscale to the thumb size.
        let mut t = Vec::with_capacity((tw * thumb_h * 4) as usize);
        for y in 0..thumb_h {
            let sy = (y as u64 * h as u64 / thumb_h as u64) as usize;
            for x in 0..tw {
                let sx = (x as u64 * w as u64 / tw as u64) as usize;
                let idx = (sy * w as usize + sx) * 4;
                t.extend_from_slice(&bgra[idx..idx + 3]);
                t.push(255);
            }
        }
        thumbs.push((t, tw, thumb_h));
    }

    if let Some((max_w, max_h)) = preview_bounds {
        // Filmstrip cells are intentionally tiny and must never be stretched
        // into the main preview. Keep a separate scrub cache sampled at about
        // 2 fps, with a bounded memory footprint. Mouse-up still decodes the
        // exact requested frame.
        let (preview_count, preview_w, preview_h) =
            scrub_cache_plan(duration, (w, h), (max_w, max_h));
        for index in 0..preview_count {
            let position = if preview_count > 1 {
                duration * index as i64 / (preview_count - 1) as i64
            } else {
                0
            };
            unsafe {
                let pv = PROPVARIANT::from(position.max(0));
                if reader
                    .SetCurrentPosition(&windows::core::GUID::zeroed(), &pv)
                    .is_err()
                {
                    break;
                }
            }
            let Some((bgra, _)) = read_video_frame(&reader, w, h, stride)? else {
                break;
            };
            let rgba = bgra_to_rgba(&bgra, w, h);
            let scaled = image::imageops::resize(
                &rgba,
                preview_w,
                preview_h,
                image::imageops::FilterType::Triangle,
            );
            previews.push((
                rgba_to_bgra(&scaled, preview_w, preview_h),
                preview_w,
                preview_h,
            ));
        }
    }
    Ok(Probe {
        duration_100ns: duration,
        thumbs,
        previews,
        source_size: (w, h),
    })
}

pub fn probe(path: &Path, strip_w: u32, thumb_h: u32) -> Result<Probe> {
    probe_impl(path, strip_w, thumb_h, None)
}

/// Probe a recording for the editor. Filmstrip cells stay compact, while a
/// separate high-quality cache keeps the large preview sharp during scrubs.
pub fn probe_editor(
    path: &Path,
    strip_w: u32,
    thumb_h: u32,
    preview_w: u32,
    preview_h: u32,
) -> Result<Probe> {
    probe_impl(
        path,
        strip_w,
        thumb_h,
        Some((preview_w, preview_h)),
    )
}

/// A finalized MP4 is trusted only after Media Foundation can read its
/// duration and decode video from it. File existence or a successful
/// SinkWriter::Finalize alone does not prove the container is usable.
pub fn validate_video(path: &Path) -> Result<()> {
    let bytes = std::fs::metadata(path)
        .with_context(|| format!("read video metadata for {}", path.display()))?
        .len();
    if bytes < 512 {
        anyhow::bail!("video is unexpectedly small ({bytes} bytes)");
    }
    let probe = probe(path, 96, 48).context("decode finalized video")?;
    if probe.duration_100ns <= 0 {
        anyhow::bail!("video has no playable duration");
    }
    if probe.thumbs.is_empty() {
        anyhow::bail!("video has no decodable frames");
    }
    Ok(())
}

/// Resolve real stream indices — assuming video is 0 silently corrupts the
/// output (audio bytes encoded as frames).
fn stream_indices(reader: &IMFSourceReader) -> (u32, Option<u32>) {
    let (mut video, mut audio) = (0u32, None);
    unsafe {
        for i in 0..8u32 {
            let Ok(t) = reader.GetNativeMediaType(i, 0) else {
                continue;
            };
            let Ok(major) = t.GetGUID(&MF_MT_MAJOR_TYPE) else {
                continue;
            };
            if major == MFMediaType_Video {
                video = i;
            } else if major == MFMediaType_Audio && audio.is_none() {
                audio = Some(i);
            }
        }
    }
    (video, audio)
}

pub(crate) fn bgra_to_rgba(bytes: &[u8], w: u32, h: u32) -> RgbaImage {
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for pixel in bytes.chunks_exact(4) {
        rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
    }
    RgbaImage::from_raw(w, h, rgba).expect("BGRA frame has exact dimensions")
}

fn rgba_to_bgra(image: &RgbaImage, w: u32, h: u32) -> Vec<u8> {
    let mut bgra = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h.min(image.height()) {
        for x in 0..w.min(image.width()) {
            let pixel = image.get_pixel(x, y);
            bgra.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
    }
    bgra
}

fn bgra_to_rgba_into(bytes: &[u8], image: &mut RgbaImage) {
    image
        .as_mut()
        .par_chunks_mut(4)
        .zip(bytes.par_chunks(4))
        .for_each(|(dst, src)| {
            dst.copy_from_slice(&[src[2], src[1], src[0], src[3]]);
        });
}

fn rgba_to_bgra_in_place(image: &mut RgbaImage) {
    image.as_mut().par_chunks_mut(4).for_each(|pixel| pixel.swap(0, 2));
}

/// Re-encode [start, end), optionally framing every video frame with one
/// deterministic matte. Audio timing and bytes follow the trim path unchanged.
pub fn cut_with_matte(
    src: &Path,
    dst: &Path,
    start: i64,
    end: i64,
    matte: Option<&crate::style::Style>,
) -> Result<()> {
    cut_with_edit(src, dst, start, end, matte, &[])
}

pub fn cut_with_edit(
    src: &Path,
    dst: &Path,
    start: i64,
    end: i64,
    matte: Option<&crate::style::Style>,
    annotations: &[crate::video_edit::Item],
) -> Result<()> {
    let compose_opts = crate::compose::ComposeOpts::default();
    cut_with_edit_progress(
        src,
        dst,
        start,
        end,
        matte.map(|style| (style, &compose_opts)),
        annotations,
        |_| {},
    )
}

pub fn cut_with_edit_progress(
    src: &Path,
    dst: &Path,
    start: i64,
    end: i64,
    matte: Option<(&crate::style::Style, &crate::compose::ComposeOpts)>,
    annotations: &[crate::video_edit::Item],
    progress: impl FnMut(u32),
) -> Result<()> {
    let cancel = AtomicBool::new(false);
    cut_with_edit_progress_cancel(
        src,
        dst,
        start,
        end,
        matte,
        annotations,
        &cancel,
        progress,
    )
}

// The cancel-aware form deliberately mirrors the established export API and
// adds one synchronization primitive; grouping these strongly typed inputs
// into a bag would make call sites less explicit.
#[allow(clippy::too_many_arguments)]
pub fn cut_with_edit_progress_cancel(
    src: &Path,
    dst: &Path,
    start: i64,
    end: i64,
    matte: Option<(&crate::style::Style, &crate::compose::ComposeOpts)>,
    annotations: &[crate::video_edit::Item],
    cancel: &AtomicBool,
    mut progress: impl FnMut(u32),
) -> Result<()> {
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL).ok() };
    let (reader, w, h, stride) = open_reader(src, true)?;
    let (video_idx, audio_idx) = stream_indices(&reader);

    // Audio format, if the source has a track.
    let audio_fmt = unsafe {
        reader
            .GetCurrentMediaType(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32)
            .ok()
            .and_then(|t| {
                Some(crate::audio::Format {
                    rate: t.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).ok()?,
                    channels: t.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).ok()? as u16,
                })
            })
    };

    let even = |v: u32| (v.max(2)) & !1;
    let (matte, matte_opts) = match matte {
        Some((style, opts)) if !crate::compose::is_plain(style) => (Some(style), *opts),
        _ => (None, crate::compose::ComposeOpts::default()),
    };
    let matte_base =
        matte.map(|style| crate::compose::compose_base(w as usize, h as usize, style, &matte_opts));
    let annotation_offset = if matte_base.is_some() {
        let layout = crate::compose::layout(w as usize, h as usize, &matte_opts);
        (layout.pad_x as f32, layout.pad_y as f32)
    } else {
        (0.0, 0.0)
    };
    let (ow, oh) = matte_base
        .as_ref()
        .map(|base| (even(base.width()), even(base.height())))
        .unwrap_or_else(|| (even(w), even(h)));
    let (writer, vstream, astream) = unsafe {
        crate::record::make_sink_for_content(dst, ow, oh, even(w), even(h), audio_fmt.as_ref())?
    };

    unsafe {
        let pv = PROPVARIANT::from(start.max(0));
        let _ = reader.SetCurrentPosition(&windows::core::GUID::zeroed(), &pv);
    }

    let row = (ow * 4) as usize;
    let source_row = (w * 4) as usize;
    let mut raw = vec![0u8; source_row * h as usize];
    let mut plain_out = vec![0u8; row * oh as usize];
    let mut composed_frame = if let Some(base) = &matte_base {
        Some(base.clone())
    } else if !annotations.is_empty() {
        Some(RgbaImage::new(ow, oh))
    } else {
        None
    };
    loop {
        if cancel.load(Ordering::Relaxed) {
            anyhow::bail!("export cancelled");
        }
        unsafe {
            let mut stream = 0u32;
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut sample = None;
            reader.ReadSample(
                0xFFFFFFFE, // MF_SOURCE_READER_ANY_STREAM
                0,
                Some(&mut stream),
                Some(&mut flags),
                Some(&mut ts),
                Some(&mut sample),
            )?;
            if flags & 0x2 != 0 {
                // A stream ended; stop once video is done.
                if stream == video_idx {
                    break;
                }
                continue;
            }
            if flags & 0x1000 != 0 {
                continue;
            }
            let Some(sample) = sample else { continue };
            if ts >= end {
                if stream == video_idx {
                    break;
                }
                continue;
            }
            if ts < start {
                continue;
            }
            let rel = ts - start;

            let is_video = stream == video_idx;
            if !is_video && Some(stream) != audio_idx {
                continue;
            }
            if is_video {
                // Repack to tight top-down rows for the sink.
                let buf = sample.ConvertToContiguousBuffer()?;
                let mut ptr = std::ptr::null_mut();
                buf.Lock(&mut ptr, None, None)?;
                let abs_stride = stride.unsigned_abs() as usize;
                for y in 0..h as usize {
                    let src_row = if stride < 0 { h as usize - 1 - y } else { y };
                    std::ptr::copy_nonoverlapping(
                        (ptr as *const u8).add(src_row * abs_stride),
                        raw.as_mut_ptr().add(y * source_row),
                        source_row,
                    );
                }
                buf.Unlock()?;

                let out = if let Some(composed) = composed_frame.as_mut() {
                    if let Some(base) = &matte_base {
                        composed.as_mut().copy_from_slice(base.as_raw());
                        crate::compose::blend_bgra_content(
                            composed,
                            &raw,
                            w as usize,
                            h as usize,
                            &matte_opts,
                        );
                    } else {
                        bgra_to_rgba_into(&raw, composed);
                    }
                    crate::video_edit::render_at(
                        composed,
                        annotations,
                        ts,
                        None,
                        (w, h),
                        annotation_offset,
                    );
                    rgba_to_bgra_in_place(composed);
                    composed.as_raw()
                } else {
                    for y in 0..oh.min(h) as usize {
                        plain_out[y * row..(y + 1) * row]
                            .copy_from_slice(&raw[y * source_row..y * source_row + row]);
                    }
                    plain_out.as_slice()
                };

                use windows::Win32::Media::MediaFoundation::{
                    MFCreateMemoryBuffer, MFCreateSample,
                };
                let mb = MFCreateMemoryBuffer(out.len() as u32)?;
                let mut mp = std::ptr::null_mut();
                mb.Lock(&mut mp, None, None)?;
                std::ptr::copy_nonoverlapping(out.as_ptr(), mp, out.len());
                mb.Unlock()?;
                mb.SetCurrentLength(out.len() as u32)?;
                let s = MFCreateSample()?;
                s.AddBuffer(&mb)?;
                s.SetSampleTime(rel)?;
                s.SetSampleDuration(sample.GetSampleDuration().unwrap_or(333_333))?;
                writer.WriteSample(vstream, &s)?;
                let span = (end - start).max(1);
                progress(((rel * 100 / span).clamp(0, 99)) as u32);
            } else if let Some(astream) = astream {
                let buf = sample.ConvertToContiguousBuffer()?;
                let mut ptr = std::ptr::null_mut();
                let mut len = 0u32;
                buf.Lock(&mut ptr, None, Some(&mut len))?;
                let bytes = std::slice::from_raw_parts(ptr as *const u8, len as usize).to_vec();
                buf.Unlock()?;

                use windows::Win32::Media::MediaFoundation::{
                    MFCreateMemoryBuffer, MFCreateSample,
                };
                let mb = MFCreateMemoryBuffer(bytes.len() as u32)?;
                let mut mp = std::ptr::null_mut();
                mb.Lock(&mut mp, None, None)?;
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), mp, bytes.len());
                mb.Unlock()?;
                mb.SetCurrentLength(bytes.len() as u32)?;
                let s = MFCreateSample()?;
                s.AddBuffer(&mb)?;
                s.SetSampleTime(rel)?;
                s.SetSampleDuration(sample.GetSampleDuration().unwrap_or(100_000))?;
                writer.WriteSample(astream, &s)?;
            }
        }
    }
    if cancel.load(Ordering::Relaxed) {
        anyhow::bail!("export cancelled");
    }
    unsafe { writer.Finalize().context("finalize trim")? };
    progress(100);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{bgra_to_rgba, fit_inside, rgba_to_bgra, scrub_cache_plan, validate_video};

    #[test]
    fn preview_fit_preserves_the_recorded_aspect_ratio() {
        assert_eq!(fit_inside(1920, 1036, 960, 540), (960, 518));
        assert_eq!(fit_inside(1920, 1036, 1600, 420), (778, 420));
        assert_eq!(fit_inside(640, 480, 1920, 1080), (640, 480));
    }

    #[test]
    fn scrub_cache_is_smooth_sharp_and_memory_bounded() {
        let (count, width, height) =
            scrub_cache_plan(30 * 10_000_000, (1920, 1036), (1600, 700));
        assert_eq!(count, 60);
        assert!(width >= 700 && height >= 375, "cache frame was {width}x{height}");
        assert!((width as f32 / height as f32 - 1920.0 / 1036.0).abs() < 0.01);
        assert!(count as u64 * width as u64 * height as u64 * 4 <= 96 * 1024 * 1024);

        assert_eq!(scrub_cache_plan(2 * 10_000_000, (1280, 720), (900, 600)).0, 24);
        assert_eq!(scrub_cache_plan(90 * 10_000_000, (1280, 720), (900, 600)).0, 96);
    }

    #[test]
    fn video_pixel_channel_conversion_round_trips() {
        let bgra = vec![3, 2, 1, 255, 30, 20, 10, 128];
        let rgba = bgra_to_rgba(&bgra, 2, 1);
        assert_eq!(rgba.as_raw(), &[1, 2, 3, 255, 10, 20, 30, 128]);
        assert_eq!(rgba_to_bgra(&rgba, 2, 1), bgra);
    }

    #[test]
    fn video_validation_rejects_truncated_output() {
        let path = std::env::temp_dir().join(format!(
            "matteshot-invalid-video-{}.mp4",
            std::process::id()
        ));
        std::fs::write(&path, b"not an mp4").unwrap();
        assert!(validate_video(&path).is_err());
        std::fs::remove_file(path).unwrap();
    }
}
