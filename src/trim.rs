//! Trim support: probe an MP4 for duration + filmstrip thumbnails, and cut
//! a frame-accurate range into a new file (decode → re-encode through the
//! same sink pipeline the recorder uses).

use std::path::Path;

use anyhow::{Context, Result};
use windows::core::{HSTRING, PROPVARIANT};
use windows::Win32::Media::MediaFoundation::{
    IMFSourceReader, MFCreateMediaType, MFCreateSourceReaderFromURL, MFStartup, MFMediaType_Audio, MFMediaType_Video, MFVideoFormat_RGB32,
    MFSTARTUP_FULL, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_PD_DURATION,
    MF_SOURCE_READER_FIRST_AUDIO_STREAM, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READER_MEDIASOURCE, MF_VERSION,
};

pub struct Probe {
    pub duration_100ns: i64,
    /// BGRA thumbnails: (pixels, w, h).
    pub thumbs: Vec<(Vec<u8>, u32, u32)>,
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
            let _ = reader.SetCurrentMediaType(
                MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32,
                None,
                &at,
            );
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

/// Duration + filmstrip thumbnails sized to tile `strip_w` x `strip_h` at
/// the video's own aspect ratio (stretching frames to fixed cells makes the
/// strip look wrong).
pub fn probe(path: &Path, strip_w: u32, thumb_h: u32) -> Result<Probe> {
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL).ok() };
    let (reader, w, h, stride) = open_reader(path, false)?;
    let duration = duration_of(&reader)?;
    let mut thumbs = Vec::new();
    let tw = ((w as f32 * thumb_h as f32 / h as f32) as u32).max(2);
    // Enough frames to fill the strip, capped so probing stays quick.
    let n_thumbs = ((strip_w as f32 / tw as f32).ceil() as usize).clamp(3, 24);

    for i in 0..n_thumbs {
        let pos = duration * i as i64 / n_thumbs as i64;
        unsafe {
            let pv = PROPVARIANT::from(pos);
            if reader.SetCurrentPosition(&windows::core::GUID::zeroed(), &pv).is_err() {
                break;
            }
        }
        let Some((bgra, _)) = read_video_frame(&reader, w, h, stride)? else { break };
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
    Ok(Probe { duration_100ns: duration, thumbs })
}

/// Resolve real stream indices — assuming video is 0 silently corrupts the
/// output (audio bytes encoded as frames).
fn stream_indices(reader: &IMFSourceReader) -> (u32, Option<u32>) {
    let (mut video, mut audio) = (0u32, None);
    unsafe {
        for i in 0..8u32 {
            let Ok(t) = reader.GetNativeMediaType(i, 0) else { continue };
            let Ok(major) = t.GetGUID(&MF_MT_MAJOR_TYPE) else { continue };
            if major == MFMediaType_Video {
                video = i;
            } else if major == MFMediaType_Audio && audio.is_none() {
                audio = Some(i);
            }
        }
    }
    (video, audio)
}

/// Cut [start, end) into `dst`, re-encoding video (frame-accurate) and audio.
pub fn cut(src: &Path, dst: &Path, start: i64, end: i64) -> Result<()> {
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
    let (ow, oh) = (even(w), even(h));
    let (writer, vstream, astream) =
        unsafe { crate::record::make_sink(dst, ow, oh, audio_fmt.as_ref())? };

    unsafe {
        let pv = PROPVARIANT::from(start.max(0));
        let _ = reader.SetCurrentPosition(&windows::core::GUID::zeroed(), &pv);
    }

    let row = (ow * 4) as usize;
    loop {
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
                let mut out = vec![0u8; row * oh as usize];
                for y in 0..oh.min(h) as usize {
                    let src_row = if stride < 0 { h as usize - 1 - y } else { y };
                    std::ptr::copy_nonoverlapping(
                        (ptr as *const u8).add(src_row * abs_stride),
                        out.as_mut_ptr().add(y * row),
                        row.min((w * 4) as usize),
                    );
                }
                buf.Unlock()?;

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
    unsafe { writer.Finalize().context("finalize trim")? };
    Ok(())
}
