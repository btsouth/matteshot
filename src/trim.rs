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
    MFMediaType_Video, MFStartup, MFVideoFormat_RGB32, MFSTARTUP_FULL,
    MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT,
    MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_PD_DURATION,
    MF_SOURCE_READER_FIRST_AUDIO_STREAM,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_SOURCE_READER_MEDIASOURCE, MF_VERSION,
};

pub struct Probe {
    pub duration_100ns: i64,
    /// BGRA thumbnails: (pixels, w, h).
    pub thumbs: Vec<(Vec<u8>, u32, u32)>,
    /// Higher-quality BGRA frames used while scrubbing in the editor.
    pub previews: Vec<(Vec<u8>, u32, u32)>,
}

/// The cheap half of a probe: everything the editor needs to appear.
pub struct OpeningProbe {
    pub duration_100ns: i64,
    pub source_size: (u32, u32),
    /// One BGRA frame at preview size, from the start of the recording.
    pub first: (Vec<u8>, u32, u32),
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

/// H.264 tops out near 9.4M luma samples in a frame. A matte with a forced
/// aspect can compose a large recording well past that, and Media Foundation
/// then refuses the media type with nothing useful to say.
///
/// Shrink the *content* until the framed result fits, rather than failing the
/// export. Returns the content size to feed the compositor; equal to the input
/// whenever nothing needs to give, which is the overwhelmingly common case.
fn content_size_for_encoder(
    w: u32,
    h: u32,
    opts: &crate::compose::ComposeOpts,
    framed: bool,
) -> (u32, u32) {
    const MAX_PIXELS: u64 = 9_400_000;
    let even = |v: u32| (v.max(2)) & !1;
    let composed = |cw: u32, ch: u32| -> u64 {
        if !framed {
            return cw as u64 * ch as u64;
        }
        let layout = crate::compose::layout(cw as usize, ch as usize, opts);
        (cw as u64 + layout.pad_x as u64 * 2) * (ch as u64 + layout.pad_y as u64 * 2)
    };
    let (mut cw, mut ch) = (w.max(2), h.max(2));
    // Padding is proportional to the content, so shrinking shrinks the frame
    // too and this converges quickly. The 0.98 keeps it from stalling right on
    // the boundary.
    for _ in 0..16 {
        let pixels = composed(cw, ch);
        if pixels <= MAX_PIXELS {
            break;
        }
        let factor = (MAX_PIXELS as f64 / pixels as f64).sqrt() * 0.98;
        cw = ((cw as f64 * factor) as u32).max(2);
        ch = ((ch as f64 * factor) as u32).max(2);
    }
    (even(cw), even(ch))
}

/// A normalized crop as a source-pixel rect: `(x, y, width, height)`.
///
/// Clamped inside the frame and rounded to even dimensions, because H.264
/// rejects odd ones — doing it here means the untouched case stays a true
/// no-op rather than paying for a one-pixel resize on the way through.
fn crop_rect(crop: crate::video_edit::Crop, w: u32, h: u32) -> (u32, u32, u32, u32) {
    let even = |v: u32| (v.max(2)) & !1;
    let span = |origin: f32, size: f32, limit: u32| {
        let limit_f = limit as f32;
        let origin = (origin * limit_f).round().clamp(0.0, limit_f) as u32;
        let size = even(((size * limit_f).round().clamp(2.0, limit_f)) as u32);
        (origin.min(limit.saturating_sub(size)), size)
    };
    let (x, width) = span(crop.x, crop.w, w);
    let (y, height) = span(crop.y, crop.h, h);
    (x, y, width, height)
}

/// Copy a sub-rect out of a tightly packed BGRA frame.
fn crop_bgra(source: &[u8], source_w: u32, rect: (u32, u32, u32, u32), out: &mut [u8]) {
    let (x, y, width, height) = rect;
    let (src_stride, dst_stride) = (source_w as usize * 4, width as usize * 4);
    out.par_chunks_mut(dst_stride)
        .take(height as usize)
        .enumerate()
        .for_each(|(row, dst)| {
            let at = (y as usize + row) * src_stride + x as usize * 4;
            dst.copy_from_slice(&source[at..at + dst_stride]);
        });
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

fn open_reader(path: &Path, with_audio: bool) -> Result<(IMFSourceReader, u32, u32, i32, u32)> {
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
            // Missing audio is fine. When it exists, fully request the 16-bit
            // layout that retime_pcm and the sink both consume.
            if let Ok(native) =
                reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32)
            {
                let channels = native.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)?;
                let rate = native.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)?;
                let block = channels * 2;
                let at = MFCreateMediaType()?;
                at.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                at.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
                at.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
                at.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)?;
                at.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                at.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, block)?;
                at.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, rate * block)?;
                reader
                    .SetCurrentMediaType(
                        MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32,
                        None,
                        &at,
                    )
                    .context("set 16-bit PCM audio decode type")?;
                let resolved =
                    reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32)?;
                anyhow::ensure!(
                    resolved.GetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE)? == 16
                        && resolved.GetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT)? == block,
                    "audio decoder did not provide 16-bit interleaved PCM"
                );
            }
        }

        let cur = reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)?;
        let size = cur.GetUINT64(&MF_MT_FRAME_SIZE)?;
        let (w, h) = ((size >> 32) as u32, (size & 0xFFFF_FFFF) as u32);
        let stride = cur
            .GetUINT32(&MF_MT_DEFAULT_STRIDE)
            .map(|v| v as i32)
            .unwrap_or((w * 4) as i32);
        let packed_rate = cur.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(0);
        let numerator = (packed_rate >> 32) as u32;
        let denominator = (packed_rate & 0xFFFF_FFFF) as u32;
        let fps = if denominator == 0 {
            crate::record::DEFAULT_FPS
        } else {
            ((numerator as f64 / denominator as f64).round() as u32).clamp(1, 120)
        };
        Ok((reader, w, h, stride, fps))
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
    let (reader, w, h, stride, _) = open_reader(path, false)?;
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

/// One frame the scrubber wants. `generation` lets the editor ignore frames
/// for a drag position it has already moved past.
pub struct ScrubRequest {
    pub position: i64,
    pub generation: u64,
}

/// Serve scrub requests from a long-lived reader.
///
/// `preview_frame` opens a fresh source reader per call, which costs more than
/// the decode does and is why scrubbing only ever showed cached frames until
/// the mouse came up. This keeps one reader open for the life of the editor.
///
/// Requests coalesce: while decoding, everything the user scrubbed past is
/// dropped and only the newest position is served, so the decoder never falls
/// behind the cursor.
pub fn scrub_worker(
    path: &Path,
    max_w: u32,
    max_h: u32,
    requests: std::sync::mpsc::Receiver<ScrubRequest>,
    mut deliver: impl FnMut(u64, Vec<u8>, u32, u32) -> bool,
) -> Result<()> {
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL).ok() };
    let (reader, w, h, stride, _) = open_reader(path, false)?;
    let (target_w, target_h) = fit_inside(w, h, max_w.max(2), max_h.max(2));
    while let Ok(mut request) = requests.recv() {
        while let Ok(newer) = requests.try_recv() {
            request = newer;
        }
        unsafe {
            let pv = PROPVARIANT::from(request.position.max(0));
            if reader
                .SetCurrentPosition(&windows::core::GUID::zeroed(), &pv)
                .is_err()
            {
                continue;
            }
        }
        let Ok(Some((bgra, _))) = read_video_frame(&reader, w, h, stride) else {
            continue;
        };
        let rgba = bgra_to_rgba(&bgra, w, h);
        let scaled = image::imageops::resize(
            &rgba,
            target_w,
            target_h,
            image::imageops::FilterType::Triangle,
        );
        if !deliver(
            request.generation,
            rgba_to_bgra(&scaled, target_w, target_h),
            target_w,
            target_h,
        ) {
            break;
        }
    }
    Ok(())
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
    deliver: impl FnMut(PlaybackFrame) -> bool,
) -> Result<()> {
    playback_frames_with_speed(path, start, end, max_w, max_h, &[], cancel, deliver)
}

#[allow(clippy::too_many_arguments)]
pub fn playback_frames_with_speed(
    path: &Path,
    start: i64,
    end: i64,
    max_w: u32,
    max_h: u32,
    speed_ranges: &[crate::video_speed::SpeedRange],
    cancel: &AtomicBool,
    mut deliver: impl FnMut(PlaybackFrame) -> bool,
) -> Result<()> {
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL).ok() };
    let (reader, w, h, stride, _) = open_reader(path, false)?;
    let time_map = crate::video_speed::TimeMap::new(start, end, speed_ranges)
        .map_err(anyhow::Error::msg)?;
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

        let media_elapsed =
            std::time::Duration::from_nanos(time_map.output_time(timestamp) as u64 * 100);
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
    let (reader, w, h, stride, _) = open_reader(path, false)?;
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
    })
}

pub fn probe(path: &Path, strip_w: u32, thumb_h: u32) -> Result<Probe> {
    probe_impl(path, strip_w, thumb_h, None)
}

/// Just enough to put the editor on screen: how long the recording is, how big
/// its frames are, and one frame to show.
///
/// The filmstrip and the scrub cache together cost dozens of seeks, and a seek
/// runs about 100ms because the decoder restarts from the preceding keyframe.
/// Paying for all of them before the window exists is what made opening the
/// editor take seconds, and get worse the longer the recording was.
pub fn probe_opening(path: &Path, preview_w: u32, preview_h: u32) -> Result<OpeningProbe> {
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL).ok() };
    let (reader, w, h, stride, _) = open_reader(path, false)?;
    let duration = duration_of(&reader)?;
    let (bgra, _) = read_video_frame(&reader, w, h, stride)?
        .context("the finished recording has no decodable video frames")?;
    let rgba = bgra_to_rgba(&bgra, w, h);
    let (pw, ph) = fit_inside(w, h, preview_w.max(2), preview_h.max(2));
    let scaled = image::imageops::resize(&rgba, pw, ph, image::imageops::FilterType::Triangle);
    Ok(OpeningProbe {
        duration_100ns: duration,
        source_size: (w, h),
        first: (rgba_to_bgra(&scaled, pw, ph), pw, ph),
    })
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

/// These two run per frame during playback and per frame while recording, so
/// they get the same parallel treatment as the export path rather than walking
/// a megapixel with bounds-checked pixel accessors.
pub(crate) fn bgra_to_rgba(bytes: &[u8], w: u32, h: u32) -> RgbaImage {
    let mut rgba = vec![0u8; w as usize * h as usize * 4];
    rgba.par_chunks_exact_mut(4)
        .zip(bytes.par_chunks_exact(4))
        .for_each(|(dst, src)| dst.copy_from_slice(&[src[2], src[1], src[0], src[3]]));
    RgbaImage::from_raw(w, h, rgba).expect("BGRA frame has exact dimensions")
}

/// Crops to `w` x `h` when the image is larger, which callers rely on for
/// odd-sized preview frames.
fn rgba_to_bgra(image: &RgbaImage, w: u32, h: u32) -> Vec<u8> {
    let cols = w.min(image.width()) as usize;
    let rows = h.min(image.height()) as usize;
    if cols == 0 || rows == 0 {
        return Vec::new();
    }
    let src_stride = image.width() as usize * 4;
    let src = image.as_raw();
    let mut bgra = vec![0u8; cols * rows * 4];
    bgra.par_chunks_mut(cols * 4)
        .enumerate()
        .for_each(|(y, dst_row)| {
            let row = &src[y * src_stride..y * src_stride + cols * 4];
            for x in 0..cols {
                let s = &row[x * 4..x * 4 + 4];
                dst_row[x * 4..x * 4 + 4].copy_from_slice(&[s[2], s[1], s[0], s[3]]);
            }
        });
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

/// Convert one decoded PCM sample onto the shortened output timeline. Normal
/// pieces retain their bytes; sped pieces become shorter silence so speech is
/// never turned into unintelligible chipmunk audio.
fn retime_pcm(
    bytes: &[u8],
    timestamp: i64,
    format: &crate::audio::Format,
    time_map: &crate::video_speed::TimeMap,
) -> Option<(Vec<u8>, i64, i64)> {
    const TICKS_PER_SECOND: i64 = 10_000_000;
    let block = format.channels as usize * 2;
    if block == 0 || format.rate == 0 {
        return None;
    }
    let source_frames = bytes.len() / block;
    if source_frames == 0 {
        return None;
    }
    let source_duration = source_frames as i64 * TICKS_PER_SECOND / format.rate as i64;
    let source_end = timestamp.saturating_add(source_duration);
    let segments = time_map.segments(timestamp, source_end);
    let first = segments.first()?.0;
    let mut output = Vec::new();
    let frame_at = |time: i64| {
        (((time - timestamp).max(0) as i128 * format.rate as i128
            + (TICKS_PER_SECOND / 2) as i128)
            / TICKS_PER_SECOND as i128)
            .clamp(0, source_frames as i128) as usize
    };

    for (from, to, rate) in segments {
        let from_frame = frame_at(from);
        let to_frame = frame_at(to).max(from_frame).min(source_frames);
        let frames = to_frame.saturating_sub(from_frame);
        if frames == 0 {
            continue;
        }
        if rate == 1 {
            output.extend_from_slice(&bytes[from_frame * block..to_frame * block]);
        } else {
            let output_frames = frames.div_ceil(rate as usize);
            output.resize(output.len() + output_frames * block, 0);
        }
    }
    if output.is_empty() {
        return None;
    }
    let output_frames = output.len() / block;
    let output_duration = output_frames as i64 * TICKS_PER_SECOND / format.rate as i64;
    Some((output, time_map.output_time(first), output_duration.max(1)))
}

fn retime_video_sample(
    timestamp: i64,
    duration: i64,
    frame_interval: i64,
    time_map: &crate::video_speed::TimeMap,
) -> (i64, i64, Option<i64>) {
    let output_time = time_map.output_time(timestamp);
    if time_map.rate_at(timestamp) == 1 {
        let output_end = time_map.output_time(timestamp.saturating_add(duration.max(1)));
        return (output_time, (output_end - output_time).max(1), None);
    }
    let slot = output_time / frame_interval.max(1);
    (slot * frame_interval, frame_interval.max(1), Some(slot))
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
    progress: impl FnMut(u32),
) -> Result<()> {
    cut_with_speed_edit_progress_cancel(
        src,
        dst,
        start,
        end,
        matte,
        annotations,
        &[],
        crate::video_edit::Crop::FULL,
        cancel,
        progress,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn cut_with_speed_edit_progress_cancel(
    src: &Path,
    dst: &Path,
    start: i64,
    end: i64,
    matte: Option<(&crate::style::Style, &crate::compose::ComposeOpts)>,
    annotations: &[crate::video_edit::Item],
    speed_ranges: &[crate::video_speed::SpeedRange],
    crop: crate::video_edit::Crop,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u32),
) -> Result<()> {
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL).ok() };
    let (reader, w, h, stride, source_fps) = open_reader(src, true)?;
    let fps = crate::record::sanitize_fps(source_fps);
    let (video_idx, audio_idx) = stream_indices(&reader);
    let time_map = crate::video_speed::TimeMap::new(start, end, speed_ranges)
        .map_err(anyhow::Error::msg)?;

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
    // The crop decides what "content" means before anything else measures it:
    // the encoder is sized from the kept region, not the recording.
    let crop_rect = crop_rect(crop, w, h);
    let cropped = crop_rect != (0, 0, w, h);
    let (crop_w, crop_h) = (crop_rect.2, crop_rect.3);
    if cropped {
        eprintln!(
            "export: cropping {w}x{h} to {crop_w}x{crop_h} at {},{}",
            crop_rect.0, crop_rect.1
        );
    }
    // Content may have to shrink so the framed result stays encodable.
    let (cw, ch) = content_size_for_encoder(crop_w, crop_h, &matte_opts, matte.is_some());
    let downscaled = (cw, ch) != (crop_w, crop_h);
    if downscaled {
        crate::diagnostics::log("export content scaled down to stay encodable");
        eprintln!("export: content {w}x{h} -> {cw}x{ch} so the framed result fits H.264");
    }
    let matte_base = matte
        .map(|style| crate::compose::compose_base(cw as usize, ch as usize, style, &matte_opts));
    let annotation_offset = if matte_base.is_some() {
        let layout = crate::compose::layout(cw as usize, ch as usize, &matte_opts);
        (layout.pad_x as f32, layout.pad_y as f32)
    } else {
        (0.0, 0.0)
    };
    let (ow, oh) = matte_base
        .as_ref()
        .map(|base| (even(base.width()), even(base.height())))
        .unwrap_or_else(|| (even(cw), even(ch)));
    let (writer, vstream, astream) = unsafe {
        crate::record::make_sink_for_content(
            dst,
            ow,
            oh,
            even(cw),
            even(ch),
            fps,
            audio_fmt.as_ref(),
        )?
    };

    unsafe {
        let pv = PROPVARIANT::from(start.max(0));
        let _ = reader.SetCurrentPosition(&windows::core::GUID::zeroed(), &pv);
    }

    let row = (ow * 4) as usize;
    let source_row = (w * 4) as usize;
    let content_row = (cw * 4) as usize;
    let mut raw = vec![0u8; source_row * h as usize];
    // Only allocated when a crop actually removes something.
    let mut cropped_frame = if cropped {
        vec![0u8; crop_w as usize * 4 * crop_h as usize]
    } else {
        Vec::new()
    };
    // Only allocated when the content actually has to shrink.
    let mut scaled_content = if downscaled {
        vec![0u8; content_row * ch as usize]
    } else {
        Vec::new()
    };
    let mut plain_out = vec![0u8; row * oh as usize];
    let mut composed_frame = if let Some(base) = &matte_base {
        Some(base.clone())
    } else if !annotations.is_empty() {
        Some(RgbaImage::new(ow, oh))
    } else {
        None
    };
    let frame_interval = 10_000_000i64 / fps as i64;
    let mut last_video_slot = None;
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
                let source_duration = sample.GetSampleDuration().unwrap_or(frame_interval);
                let (output_time, output_duration, output_slot) =
                    retime_video_sample(ts, source_duration, frame_interval, &time_map);
                if output_slot.is_some() && last_video_slot == output_slot {
                    continue;
                }
                last_video_slot = output_slot;
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

                // Everything downstream works in content coordinates: the
                // kept region of the source, shrunk further only if the framed
                // result would otherwise be too big to encode.
                let kept: &[u8] = if cropped {
                    crop_bgra(&raw, w, crop_rect, &mut cropped_frame);
                    &cropped_frame
                } else {
                    &raw
                };
                let content: &[u8] = if downscaled {
                    let rgba = bgra_to_rgba(kept, crop_w, crop_h);
                    let small = image::imageops::resize(
                        &rgba,
                        cw,
                        ch,
                        image::imageops::FilterType::Triangle,
                    );
                    scaled_content
                        .par_chunks_exact_mut(4)
                        .zip(small.as_raw().par_chunks_exact(4))
                        .for_each(|(dst, src)| {
                            dst.copy_from_slice(&[src[2], src[1], src[0], src[3]])
                        });
                    &scaled_content
                } else {
                    kept
                };

                let out = if let Some(composed) = composed_frame.as_mut() {
                    if let Some(base) = &matte_base {
                        composed.as_mut().copy_from_slice(base.as_raw());
                        crate::compose::blend_bgra_content(
                            composed,
                            content,
                            cw as usize,
                            ch as usize,
                            &matte_opts,
                        );
                    } else {
                        bgra_to_rgba_into(content, composed);
                    }
                    // Annotations are normalized to the recording, so the
                    // frame has to say which window of it these `cw` x `ch`
                    // pixels are — otherwise a cropped export would place them
                    // against a picture that no longer matches.
                    crate::video_edit::render_at(
                        composed,
                        annotations,
                        ts,
                        None,
                        crate::video_edit::Frame { crop, content: (cw, ch) },
                        annotation_offset,
                    );
                    rgba_to_bgra_in_place(composed);
                    composed.as_raw()
                } else {
                    for y in 0..oh.min(ch) as usize {
                        plain_out[y * row..(y + 1) * row]
                            .copy_from_slice(&content[y * content_row..y * content_row + row]);
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
                s.SetSampleTime(output_time)?;
                s.SetSampleDuration(output_duration)?;
                writer.WriteSample(vstream, &s)?;
                let span = (end - start).max(1);
                progress(((rel * 100 / span).clamp(0, 99)) as u32);
            } else if let Some(astream) = astream {
                let buf = sample.ConvertToContiguousBuffer()?;
                let mut ptr = std::ptr::null_mut();
                let mut len = 0u32;
                buf.Lock(&mut ptr, None, Some(&mut len))?;
                let source_bytes =
                    std::slice::from_raw_parts(ptr as *const u8, len as usize).to_vec();
                buf.Unlock()?;
                let retimed = if time_map.is_empty() {
                    Some((
                        source_bytes,
                        rel,
                        sample.GetSampleDuration().unwrap_or(100_000),
                    ))
                } else {
                    let audio_fmt =
                        audio_fmt.as_ref().context("audio stream has no PCM format")?;
                    retime_pcm(&source_bytes, ts, audio_fmt, &time_map)
                };
                let Some((bytes, output_time, output_duration)) = retimed else {
                    continue;
                };

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
                s.SetSampleTime(output_time)?;
                s.SetSampleDuration(output_duration)?;
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
    use super::{
        bgra_to_rgba, crop_rect, cut_with_speed_edit_progress_cancel, fit_inside, open_reader,
        read_video_frame, retime_pcm, retime_video_sample, rgba_to_bgra, scrub_cache_plan,
        validate_video,
    };
    use std::sync::atomic::AtomicBool;

    #[test]
    fn sped_audio_becomes_shorter_silence_while_surrounding_pcm_is_preserved() {
        let format = crate::audio::Format { rate: 100, channels: 1 };
        let source: Vec<u8> = (1i16..=100).flat_map(i16::to_le_bytes).collect();
        let map = crate::video_speed::TimeMap::new(
            0,
            10_000_000,
            &[crate::video_speed::SpeedRange::new(2_000_000, 6_000_000, 4)],
        )
        .unwrap();

        let (output, timestamp, duration) = retime_pcm(&source, 0, &format, &map).unwrap();
        assert_eq!(timestamp, 0);
        assert_eq!(duration, 7_000_000);
        assert_eq!(output.len(), 70 * 2);
        assert_eq!(&output[..20 * 2], &source[..20 * 2]);
        assert!(output[20 * 2..30 * 2].iter().all(|byte| *byte == 0));
        assert_eq!(&output[30 * 2..], &source[60 * 2..]);
    }

    #[test]
    fn mixed_speed_video_preserves_normal_sample_timing() {
        const SECOND: i64 = 10_000_000;
        let interval = SECOND / 30;
        let duration = 500_000;
        let map = crate::video_speed::TimeMap::new(
            0,
            6 * SECOND,
            &[crate::video_speed::SpeedRange::new(2 * SECOND, 4 * SECOND, 2)],
        )
        .unwrap();

        assert_eq!(
            retime_video_sample(SECOND, duration, interval, &map),
            (SECOND, duration, None)
        );
        let sped = retime_video_sample(2 * SECOND + 2_000_000, duration, interval, &map);
        assert_eq!(sped.1, interval);
        assert_eq!(sped.2, Some(sped.0 / interval));
        assert_eq!(
            retime_video_sample(5 * SECOND, duration, interval, &map),
            (4 * SECOND, duration, None)
        );
    }

    #[test]
    fn trim_preserves_sixty_fps_output() {
        use windows::Win32::Media::MediaFoundation::{
            MFCreateMemoryBuffer, MFCreateSample, MFStartup, MFSTARTUP_FULL, MF_VERSION,
        };

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "matteshot-60fps-trim-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.mp4");
        let output = dir.join("output.mp4");
        const FPS: u32 = 60;
        const WIDTH: u32 = 64;
        const HEIGHT: u32 = 64;
        let interval = 10_000_000i64 / FPS as i64;

        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_FULL).unwrap();
            let (writer, stream, _) =
                crate::record::make_sink(&source, WIDTH, HEIGHT, FPS, None).unwrap();
            let frame = vec![64u8; (WIDTH * HEIGHT * 4) as usize];
            for index in 0..FPS {
                let buffer = MFCreateMemoryBuffer(frame.len() as u32).unwrap();
                let mut destination = std::ptr::null_mut();
                buffer.Lock(&mut destination, None, None).unwrap();
                std::ptr::copy_nonoverlapping(frame.as_ptr(), destination, frame.len());
                buffer.Unlock().unwrap();
                buffer.SetCurrentLength(frame.len() as u32).unwrap();
                let sample = MFCreateSample().unwrap();
                sample.AddBuffer(&buffer).unwrap();
                sample.SetSampleTime(index as i64 * interval).unwrap();
                sample.SetSampleDuration(interval).unwrap();
                writer.WriteSample(stream, &sample).unwrap();
            }
            writer.Finalize().unwrap();
        }

        cut_with_speed_edit_progress_cancel(
            &source,
            &output,
            0,
            10_000_000,
            None,
            &[],
            &[],
            crate::video_edit::Crop::FULL,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        let (_, _, _, _, output_fps) = open_reader(&output, false).unwrap();
        assert_eq!(output_fps, FPS);

        std::fs::remove_file(output).unwrap();
        std::fs::remove_file(source).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn a_cropped_export_keeps_only_the_cropped_pixels() {
        use windows::Win32::Media::MediaFoundation::{
            MFCreateMemoryBuffer, MFCreateSample, MFStartup, MFSTARTUP_FULL, MF_VERSION,
        };

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir()
            .join(format!("matteshot-crop-export-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.mp4");
        let output = dir.join("output.mp4");
        const FPS: u32 = 30;
        const WIDTH: u32 = 320;
        const HEIGHT: u32 = 240;
        let interval = 10_000_000i64 / FPS as i64;

        // Left half red, right half blue. Cropping to the right half must
        // leave blue only — a crop that silently did nothing would keep both.
        let mut frame = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
        for y in 0..HEIGHT as usize {
            for x in 0..WIDTH as usize {
                let at = (y * WIDTH as usize + x) * 4;
                let bgra: [u8; 4] = if (x as u32) < WIDTH / 2 {
                    [0, 0, 255, 255]
                } else {
                    [255, 0, 0, 255]
                };
                frame[at..at + 4].copy_from_slice(&bgra);
            }
        }

        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_FULL).unwrap();
            let (writer, stream, _) =
                crate::record::make_sink(&source, WIDTH, HEIGHT, FPS, None).unwrap();
            for index in 0..FPS {
                let buffer = MFCreateMemoryBuffer(frame.len() as u32).unwrap();
                let mut destination = std::ptr::null_mut();
                buffer.Lock(&mut destination, None, None).unwrap();
                std::ptr::copy_nonoverlapping(frame.as_ptr(), destination, frame.len());
                buffer.Unlock().unwrap();
                buffer.SetCurrentLength(frame.len() as u32).unwrap();
                let sample = MFCreateSample().unwrap();
                sample.AddBuffer(&buffer).unwrap();
                sample.SetSampleTime(index as i64 * interval).unwrap();
                sample.SetSampleDuration(interval).unwrap();
                writer.WriteSample(stream, &sample).unwrap();
            }
            writer.Finalize().unwrap();
        }

        cut_with_speed_edit_progress_cancel(
            &source,
            &output,
            0,
            10_000_000,
            None,
            &[],
            &[],
            crate::video_edit::Crop { x: 0.5, y: 0.0, w: 0.5, h: 1.0 },
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();

        let (reader, out_w, out_h, stride, _) = open_reader(&output, false).unwrap();
        assert_eq!((out_w, out_h), (WIDTH / 2, HEIGHT), "cropped frame size");
        let (bgra, _) = read_video_frame(&reader, out_w, out_h, stride)
            .unwrap()
            .expect("cropped export has a frame");

        // Every pixel should be the blue half. Sampled with a wide tolerance
        // because H.264 is lossy; the point is that no red survived.
        let centre = ((out_h / 2 * out_w + out_w / 2) * 4) as usize;
        let (blue, red) = (bgra[centre] as i32, bgra[centre + 2] as i32);
        assert!(
            blue > 128 && red < 96,
            "centre of the cropped export is not the kept half: b={blue} r={red}"
        );

        std::fs::remove_file(output).unwrap();
        std::fs::remove_file(source).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn a_normalized_crop_becomes_an_even_source_rect_inside_the_frame() {
        // Even dimensions because H.264 rejects odd ones, and never hanging
        // off an edge however the floats round.
        assert_eq!(crop_rect(crate::video_edit::Crop::FULL, 1920, 1080), (0, 0, 1920, 1080));
        assert_eq!(
            crop_rect(crate::video_edit::Crop { x: 0.5, y: 0.0, w: 0.5, h: 1.0 }, 320, 240),
            (160, 0, 160, 240)
        );
        // An odd span rounds down to even and is nudged back inside.
        let (x, y, w, h) =
            crop_rect(crate::video_edit::Crop { x: 0.9, y: 0.9, w: 0.1, h: 0.1 }, 641, 481);
        assert_eq!((w % 2, h % 2), (0, 0), "odd dimensions would be rejected");
        assert!(x + w <= 641 && y + h <= 481, "crop {x},{y} {w}x{h} hangs off the frame");
        // A degenerate crop still yields something encodable.
        let (_, _, w, h) =
            crop_rect(crate::video_edit::Crop { x: 0.0, y: 0.0, w: 0.0, h: 0.0 }, 320, 240);
        assert!(w >= 2 && h >= 2);
    }

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

        // Callers pass a smaller target than the image and expect a crop from
        // the top-left, not a rescale or a panic.
        let wide = image::RgbaImage::from_raw(
            2,
            2,
            vec![
                1, 2, 3, 255, 9, 9, 9, 9, // row 0: keep px0, drop px1
                4, 5, 6, 200, 8, 8, 8, 8, // row 1: keep px0, drop px1
            ],
        )
        .unwrap();
        assert_eq!(rgba_to_bgra(&wide, 1, 2), vec![3, 2, 1, 255, 6, 5, 4, 200]);
        assert!(rgba_to_bgra(&wide, 0, 2).is_empty());
    }

    #[test]
    fn oversized_framed_exports_shrink_until_h264_accepts_them() {
        use super::content_size_for_encoder;
        let opts = crate::compose::ComposeOpts {
            metric_scale: 1.0,
            pad_factor: 0.14,
            aspect: Some(1.0),
        };
        let composed_pixels = |cw: u32, ch: u32| {
            let l = crate::compose::layout(cw as usize, ch as usize, &opts);
            (cw as u64 + l.pad_x as u64 * 2) * (ch as u64 + l.pad_y as u64 * 2)
        };

        // The real failure: 1:1 aspect on a large recording composed to
        // 3088x3088, which is 9.5 megapixels.
        let (cw, ch) = content_size_for_encoder(2560, 1440, &opts, true);
        assert!(
            composed_pixels(cw, ch) <= 9_400_000,
            "still {} pixels at {cw}x{ch}",
            composed_pixels(cw, ch)
        );
        assert!(cw < 2560 && ch < 1440, "should have shrunk");
        assert_eq!((cw % 2, ch % 2), (0, 0), "encoder needs even dimensions");

        // Ordinary sizes must pass through untouched.
        assert_eq!(content_size_for_encoder(1920, 1080, &opts, true), (1920, 1080));
        assert_eq!(content_size_for_encoder(960, 522, &opts, true), (960, 522));
        // Unframed output is its own size, so only absurd sources shrink.
        assert_eq!(content_size_for_encoder(3840, 2160, &opts, false), (3840, 2160));
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
