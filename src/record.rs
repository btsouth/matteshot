//! Screen recording: WGC frames → Media Foundation H.264/MP4, with an
//! optional downsampled GIF track. Capture runs on a worker thread; the
//! main thread shows a floating stop pill that excludes itself from the
//! recording (WDA_EXCLUDEFROMCAPTURE).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use windows::core::{Interface, HSTRING};
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::Media::MediaFoundation::{
    IMFSinkWriter, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFCreateSinkWriterFromURL, MFMediaType_Video, MFStartup, MFVideoFormat_H264,
    MFVideoFormat_RGB32, MFVideoInterlace_Progressive, MFSTARTUP_FULL, MF_MT_AVG_BITRATE,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_MAX_KEYFRAME_SPACING, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_VERSION,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

/// What to record. Handles are stored as raw values so the target can move
/// to the capture thread (HWND/HMONITOR are !Send by default).
#[derive(Clone, Copy)]
pub enum Target {
    Window(isize),
    /// Virtual-screen rect plus the monitor that contains it.
    Region(RECT, isize),
}

impl Target {
    pub fn window(h: HWND) -> Self {
        Target::Window(h.0 as isize)
    }
    pub fn region(r: RECT, m: HMONITOR) -> Self {
        Target::Region(r, m.0 as isize)
    }
}

/// Live counters the stop pill reads.
pub struct Progress {
    pub stop: AtomicBool,
    pub frames: AtomicU32,
    pub started: std::time::Instant,
    pub error: Mutex<Option<String>>,
}

pub(crate) const DEFAULT_FPS: u32 = 30;
pub(crate) const SMOOTH_FPS: u32 = 60;
const GIF_FPS: u32 = 10;
const GIF_MAX_FRAMES: usize = 240;
/// GIFs balloon fast (no interframe compression here) — keep them share-sized.
const GIF_MAX_WIDTH: u32 = 480;
static NEXT_RECORD_ID: AtomicU64 = AtomicU64::new(1);

/// Flipped by the capture loop the moment its encoding clock starts. Probe
/// observability only: the headless recording probes anchor their auto-stop
/// countdown here so the requested duration measures actual recording, not
/// WGC/encoder startup. Never reset — probe processes run one session.
pub static PROBE_CAPTURE_RUNNING: AtomicBool = AtomicBool::new(false);

pub(crate) fn sanitize_fps(fps: u32) -> u32 {
    match fps {
        SMOOTH_FPS => SMOOTH_FPS,
        _ => DEFAULT_FPS,
    }
}

fn gif_every(fps: u32) -> u32 {
    (sanitize_fps(fps) / GIF_FPS).max(1)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VideoEncoding {
    fps: u32,
    bitrate: u32,
    keyframe_spacing: u32,
}

fn video_encoding(content_w: u32, content_h: u32, fps: u32) -> VideoEncoding {
    let fps = sanitize_fps(fps);
    let bitrate = ((content_w * content_h) as f32 * fps as f32 * 0.12) as u32;
    VideoEncoding {
        fps,
        bitrate: bitrate.clamp(1_500_000, 40_000_000),
        keyframe_spacing: fps,
    }
}

fn even(v: i32) -> u32 {
    (v.max(2) as u32) & !1
}

fn encoder_slot(timestamp: i64, frame_interval: i64) -> i64 {
    timestamp.max(0) / frame_interval.max(1)
}

fn nearly_blank_bgra(bytes: &[u8]) -> bool {
    let mut sampled = 0usize;
    let mut dark = 0usize;
    for pixel in bytes.chunks_exact(4).step_by(16) {
        sampled += 1;
        if pixel[0] <= 4 && pixel[1] <= 4 && pixel[2] <= 4 {
            dark += 1;
        }
    }
    sampled > 0 && dark * 1000 >= sampled * 998
}

/// Keep the recording canvas stable when a window changes size. The current
/// frame is aspect-fitted and centered instead of reading past rows or leaving
/// unpredictable uninitialized bands in the encoded sample.
fn letterbox_bgra(source: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let rgba = crate::trim::bgra_to_rgba(source, sw, sh);
    let scale = (dw as f32 / sw.max(1) as f32).min(dh as f32 / sh.max(1) as f32);
    let rw = ((sw as f32 * scale).round() as u32).clamp(1, dw);
    let rh = ((sh as f32 * scale).round() as u32).clamp(1, dh);
    let resized = image::imageops::resize(&rgba, rw, rh, image::imageops::FilterType::Triangle);
    let ox = (dw - rw) / 2;
    let oy = (dh - rh) / 2;
    let mut out = vec![0u8; (dw * dh * 4) as usize];
    for pixel in out.chunks_exact_mut(4) {
        pixel[3] = 255;
    }
    for y in 0..rh {
        for x in 0..rw {
            let pixel = resized.get_pixel(x, y);
            let at = (((oy + y) * dw + ox + x) * 4) as usize;
            out[at..at + 4].copy_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
        }
    }
    out
}

pub unsafe fn make_sink(
    path: &std::path::Path,
    w: u32,
    h: u32,
    fps: u32,
    audio: Option<&crate::audio::Format>,
) -> Result<(IMFSinkWriter, u32, Option<u32>)> {
    make_sink_for_content(path, w, h, w, h, fps, audio)
}

/// Create an H.264 sink whose bitrate follows the moving content rather than
/// static matte padding. A framed export has more output pixels, but the added
/// background is cheap to encode and should not inflate file size linearly.
pub unsafe fn make_sink_for_content(
    path: &std::path::Path,
    w: u32,
    h: u32,
    content_w: u32,
    content_h: u32,
    fps: u32,
    audio: Option<&crate::audio::Format>,
) -> Result<(IMFSinkWriter, u32, Option<u32>)> {
    let encoding = video_encoding(content_w, content_h, fps);
    let writer: IMFSinkWriter =
        MFCreateSinkWriterFromURL(&HSTRING::from(path.as_os_str()), None, None)
            .context("create sink writer")?;

    // Output: H.264. 0.12 bits per moving-content pixel per frame preserves
    // crisp UI and fast motion without treating static matte padding as if it
    // were another full frame of changing content.
    let out = MFCreateMediaType()?;
    out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
    out.SetUINT32(&MF_MT_AVG_BITRATE, encoding.bitrate)?;
    out.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64)?;
    out.SetUINT64(&MF_MT_FRAME_RATE, ((encoding.fps as u64) << 32) | 1)?;
    out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
    // A keyframe every second. Seeking decodes forward from the preceding
    // keyframe, so the encoder's default spacing is what makes scrubbing,
    // filmstrip probing and export seeking slow. Best-effort: some encoders
    // ignore the hint, and it is not worth failing a recording over.
    let _ = out.SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, encoding.keyframe_spacing);
    let stream = writer.AddStream(&out).context("add stream")?;

    // Input: BGRA32, top-down (positive stride).
    let inp = MFCreateMediaType()?;
    inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
    inp.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64)?;
    inp.SetUINT64(&MF_MT_FRAME_RATE, ((encoding.fps as u64) << 32) | 1)?;
    inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
    inp.SetUINT32(&MF_MT_DEFAULT_STRIDE, w * 4)?;
    writer
        .SetInputMediaType(stream, &inp, None)
        .with_context(|| {
            // H.264 caps a frame at roughly 9.4M luma samples. A matte with a
            // forced aspect can push a large recording past that, and Media
            // Foundation only reports an unhelpful invalid-media-type error.
            let pixels = w as u64 * h as u64;
            if pixels > 9_400_000 {
                format!(
                    "{w}x{h} is {:.1} megapixels, past what H.264 can encode; \
                     reduce the padding or choose a different aspect",
                    pixels as f64 / 1_000_000.0
                )
            } else {
                format!("set input type for {w}x{h} (no H.264 encoder?)")
            }
        })?;

    // Optional AAC audio track fed with float PCM.
    let audio_stream = if let Some(fmt) = audio {
        use windows::Win32::Media::MediaFoundation::{
            MFAudioFormat_AAC, MFMediaType_Audio, MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
            MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS,
            MF_MT_AUDIO_SAMPLES_PER_SECOND,
        };
        let aout = MFCreateMediaType()?;
        aout.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        aout.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
        aout.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, fmt.rate)?;
        aout.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, fmt.channels as u32)?;
        aout.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        aout.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 20000)?; // ~160 kbps
        let astream = writer.AddStream(&aout).context("add audio stream")?;

        // The AAC encoder takes 16-bit PCM only — we convert on the way in.
        use windows::Win32::Media::MediaFoundation::MFAudioFormat_PCM;
        let ain = MFCreateMediaType()?;
        ain.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        ain.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
        ain.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, fmt.rate)?;
        ain.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, fmt.channels as u32)?;
        ain.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        ain.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, fmt.channels as u32 * 2)?;
        ain.SetUINT32(
            &MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
            fmt.rate * fmt.channels as u32 * 2,
        )?;
        writer
            .SetInputMediaType(astream, &ain, None)
            .context("set audio input type")?;
        Some(astream)
    } else {
        None
    };

    writer.BeginWriting().context("begin writing")?;
    Ok((writer, stream, audio_stream))
}

/// Write one PCM float chunk at `ts` (100ns) and return its duration.
unsafe fn write_pcm(
    writer: &IMFSinkWriter,
    stream: u32,
    samples: &[f32],
    rate: u32,
    channels: u16,
    ts: i64,
) -> Result<i64> {
    // f32 [-1,1] → little-endian i16 for the AAC encoder.
    let mut bytes: Vec<u8> = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    let media_buf = MFCreateMemoryBuffer(bytes.len() as u32)?;
    let mut ptr = std::ptr::null_mut();
    media_buf.Lock(&mut ptr, None, None)?;
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
    media_buf.Unlock()?;
    media_buf.SetCurrentLength(bytes.len() as u32)?;
    let frames = samples.len() as i64 / channels as i64;
    let dur = frames * 10_000_000 / rate as i64;
    let sample = MFCreateSample()?;
    sample.AddBuffer(&media_buf)?;
    sample.SetSampleTime(ts)?;
    sample.SetSampleDuration(dur)?;
    writer.WriteSample(stream, &sample)?;
    Ok(dur)
}

/// The capture + encode loop. Runs on a worker thread until `stop` is set.
type GifFrames = Vec<(Vec<u8>, u32, u32)>;

fn capture_loop(
    target: Target,
    path: std::path::PathBuf,
    want_gif: bool,
    fps: u32,
    audio_source: Option<crate::audio::Source>,
    progress: Arc<Progress>,
) -> Result<Option<GifFrames>> {
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL).context("MFStartup")? };

    // Audio: probe the device format first (the sink needs it up front),
    // degrade to silent-video-only if the device is unavailable.
    let audio = audio_source.and_then(|src| crate::audio::probe_format(src).ok().map(|f| (src, f)));
    // Encoder rate/channels may differ from the device's (192 kHz interfaces,
    // surround layouts); a resampler bridges the two.
    let audio_enc = audio.as_ref().map(|(_, f)| crate::audio::encode_format(f));
    if let (Some((_, dev)), Some(enc)) = (&audio, &audio_enc) {
        eprintln!(
            "audio: device {} Hz/{} ch -> encode {} Hz/{} ch",
            dev.rate, dev.channels, enc.rate, enc.channels
        );
    }

    let (device, context) = crate::capture::device_pair()?;
    let dxgi: IDXGIDevice = device.cast()?;
    let winrt_device: IDirect3DDevice =
        unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;

    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    let window_target = matches!(target, Target::Window(_));
    let (item, crop): (GraphicsCaptureItem, Option<RECT>) = match target {
        Target::Window(h) => (unsafe { interop.CreateForWindow(HWND(h as *mut _))? }, None),
        Target::Region(r, m) => {
            let mon = HMONITOR(m as *mut _);
            let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(mon)? };
            // Region rect is virtual-screen; convert to monitor-local.
            let mut mi = windows::Win32::Graphics::Gdi::MONITORINFO {
                cbSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::MONITORINFO>() as u32,
                ..Default::default()
            };
            unsafe {
                let _ = windows::Win32::Graphics::Gdi::GetMonitorInfoW(mon, &mut mi);
            }
            (
                item,
                Some(RECT {
                    left: r.left - mi.rcMonitor.left,
                    top: r.top - mi.rcMonitor.top,
                    right: r.right - mi.rcMonitor.left,
                    bottom: r.bottom - mi.rcMonitor.top,
                }),
            )
        }
    };

    let item_size = item.Size()?;
    let (out_w, out_h) = match crop {
        Some(c) => (even(c.right - c.left), even(c.bottom - c.top)),
        None => (even(item_size.Width), even(item_size.Height)),
    };

    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &winrt_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        SizeInt32 {
            Width: item_size.Width.max(2),
            Height: item_size.Height.max(2),
        },
    )?;
    let session = pool.CreateCaptureSession(&item)?;
    let _ = session.SetIsCursorCaptureEnabled(true);
    let _ = session.SetIsBorderRequired(false);
    session.StartCapture()?;

    let (writer, stream, audio_stream) =
        unsafe { make_sink(&path, out_w, out_h, fps, audio_enc.as_ref())? };

    // Audio worker feeds PCM over a channel; we mux on this thread at a
    // contiguous cursor, filling gaps with silence keyed to the video clock.
    let mut audio_rx = None;
    let mut audio_fmt = None;
    let mut resampler = None;
    if let (Some((src, fmt)), Some(_)) = (audio.as_ref(), audio_stream) {
        let enc = audio_enc.as_ref().unwrap();
        resampler = Some(crate::audio::Resampler::new(
            fmt.rate,
            enc.rate,
            fmt.channels,
            enc.channels,
        ));
        let (tx, rx) = std::sync::mpsc::channel::<crate::audio::Chunk>();
        let stop = progress.clone();
        let src = *src;
        std::thread::spawn(move || {
            let flag = Arc::new(AtomicBool::new(false));
            // Bridge: poll the shared stop into a local flag the audio loop reads.
            let bridge = flag.clone();
            let watcher = std::thread::spawn(move || {
                while !stop.stop.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(30));
                }
                bridge.store(true, Ordering::Relaxed);
            });
            let _ = crate::audio::capture_thread(src, tx, flag);
            let _ = watcher.join();
        });
        audio_rx = Some(rx);
        audio_fmt = Some(crate::audio::Format {
            rate: enc.rate,
            channels: enc.channels,
        });
    }
    let mut audio_cursor: i64 = 0;

    let mut gif_frames: Vec<(Vec<u8>, u32, u32)> = Vec::new();
    let mut last_slot: Option<i64> = None;
    let mut latest_buf: Option<Vec<u8>> = None;
    let mut blank_frames = 0u32;
    let mut awaiting_first_content = window_target;
    let frame_interval = 10_000_000i64 / fps as i64;
    let gif_every = gif_every(fps);
    let recording_clock = std::time::Instant::now();
    // Everything the encoded duration is measured against starts here — WGC
    // item, frame pool, and sink setup are all behind us. The headless probes
    // key their auto-stop countdown off this moment: anything earlier (process
    // start, even the pill's creation) still overlaps startup, which on a
    // starved CI VM once ate a 3-second budget down to a 0.17s fixture.
    PROBE_CAPTURE_RUNNING.store(true, Ordering::Release);
    let mut staging: Option<ID3D11Texture2D> = None;
    let mut staging_desc = D3D11_TEXTURE2D_DESC::default();
    let row_bytes = (out_w * 4) as usize;

    while !progress.stop.load(Ordering::Relaxed) {
        // The encoder owns the media clock. WGC is change-driven for some
        // windows and may supply only one frame while their content is static;
        // duplicate the latest surface into each configured frame slot so
        // real elapsed time and playback duration still match. High-refresh
        // sources are naturally reduced to the same slot cadence.
        let rel = (recording_clock.elapsed().as_nanos() / 100).min(i64::MAX as u128) as i64;
        let slot = encoder_slot(rel, frame_interval);
        if last_slot == Some(slot) {
            std::thread::sleep(std::time::Duration::from_millis(1));
            continue;
        }
        let sample_ts = slot * frame_interval;
        let buf = if let Ok(frame) = pool.TryGetNextFrame() {
            let surface = frame.Surface()?;
            let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
            let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { texture.GetDesc(&mut desc) };

            if staging.is_none()
                || staging_desc.Width != desc.Width
                || staging_desc.Height != desc.Height
            {
                staging_desc = D3D11_TEXTURE2D_DESC {
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                    ..desc
                };
                let mut tex = None;
                unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut tex))? };
                staging = tex;
            }
            let stage = staging.as_ref().unwrap();
            unsafe { context.CopyResource(stage, &texture) };

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            unsafe { context.Map(stage, 0, D3D11_MAP_READ, 0, Some(&mut mapped))? };
            let fresh = unsafe {
                let src = mapped.pData as *const u8;
                let result = if let Some(c) = crop {
                    let src_x = c.left.max(0) as u32;
                    let src_y = c.top.max(0) as u32;
                    let mut cropped = vec![0u8; row_bytes * out_h as usize];
                    for y in 0..out_h {
                        let sy = (src_y + y).min(desc.Height.saturating_sub(1));
                        let src_row =
                            src.add(sy as usize * mapped.RowPitch as usize + src_x as usize * 4);
                        std::ptr::copy_nonoverlapping(
                            src_row,
                            cropped.as_mut_ptr().add(y as usize * row_bytes),
                            row_bytes.min((desc.Width.saturating_sub(src_x) * 4) as usize),
                        );
                    }
                    cropped
                } else if desc.Width < out_w
                    || desc.Height < out_h
                    || desc.Width.abs_diff(out_w) > 1
                    || desc.Height.abs_diff(out_h) > 1
                {
                    let source_row = (desc.Width * 4) as usize;
                    let mut source = vec![0u8; source_row * desc.Height as usize];
                    for y in 0..desc.Height {
                        std::ptr::copy_nonoverlapping(
                            src.add(y as usize * mapped.RowPitch as usize),
                            source.as_mut_ptr().add(y as usize * source_row),
                            source_row,
                        );
                    }
                    letterbox_bgra(&source, desc.Width, desc.Height, out_w, out_h)
                } else {
                    let mut stable = vec![0u8; row_bytes * out_h as usize];
                    for y in 0..out_h {
                        std::ptr::copy_nonoverlapping(
                            src.add(y as usize * mapped.RowPitch as usize),
                            stable.as_mut_ptr().add(y as usize * row_bytes),
                            row_bytes,
                        );
                    }
                    stable
                };
                context.Unmap(stage, 0);
                result
            };
            latest_buf = Some(fresh.clone());
            fresh
        } else if let Some(latest) = &latest_buf {
            latest.clone()
        } else {
            std::thread::sleep(std::time::Duration::from_millis(2));
            continue;
        };

        if awaiting_first_content && nearly_blank_bgra(&buf) {
            blank_frames += 1;
            if blank_frames >= fps * 3 {
                bail!(
                    "The selected window returned only blank frames for three seconds. Try recording a region of the monitor, or disable protected/hardware-overlay video in the target app."
                );
            }
        } else {
            blank_frames = 0;
            awaiting_first_content = false;
        }

        // Encode.
        unsafe {
            let media_buf = MFCreateMemoryBuffer(buf.len() as u32)?;
            let mut ptr = std::ptr::null_mut();
            media_buf.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(buf.as_ptr(), ptr, buf.len());
            media_buf.Unlock()?;
            media_buf.SetCurrentLength(buf.len() as u32)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&media_buf)?;
            sample.SetSampleTime(sample_ts)?;
            sample.SetSampleDuration(frame_interval)?;
            writer.WriteSample(stream, &sample)?;
        }

        let n = progress.frames.fetch_add(1, Ordering::Relaxed);
        last_slot = Some(slot);

        // Mux pending audio, then top up with silence to the video clock.
        if let (Some(rx), Some(fmt), Some(astream)) =
            (audio_rx.as_ref(), audio_fmt.as_ref(), audio_stream)
        {
            unsafe {
                while let Ok(chunk) = rx.try_recv() {
                    if chunk.samples.is_empty() {
                        continue;
                    }
                    let converted = match resampler.as_mut() {
                        Some(r) => r.process(&chunk.samples),
                        None => chunk.samples,
                    };
                    if converted.is_empty() {
                        continue;
                    }
                    audio_cursor += write_pcm(
                        &writer,
                        astream,
                        &converted,
                        fmt.rate,
                        fmt.channels,
                        audio_cursor,
                    )?;
                }
                while audio_cursor < sample_ts - 600_000 {
                    // 20ms of silence.
                    let frames = fmt.rate as usize / 50;
                    let silence = vec![0f32; frames * fmt.channels as usize];
                    audio_cursor += write_pcm(
                        &writer,
                        astream,
                        &silence,
                        fmt.rate,
                        fmt.channels,
                        audio_cursor,
                    )?;
                }
            }
        }

        if want_gif && n.is_multiple_of(gif_every) && gif_frames.len() < GIF_MAX_FRAMES {
            let scale = (GIF_MAX_WIDTH as f32 / out_w as f32).min(1.0);
            let (gw, gh) = (
                ((out_w as f32 * scale) as u32).max(2) & !1,
                ((out_h as f32 * scale) as u32).max(2) & !1,
            );
            let mut rgba = Vec::with_capacity((gw * gh * 4) as usize);
            for y in 0..gh {
                let sy = (y as f32 / scale) as u32;
                for x in 0..gw {
                    let sx = (x as f32 / scale) as u32;
                    let i =
                        (sy.min(out_h - 1) as usize * row_bytes) + (sx.min(out_w - 1) as usize * 4);
                    rgba.extend_from_slice(&[buf[i + 2], buf[i + 1], buf[i], 255]);
                }
            }
            gif_frames.push((rgba, gw, gh));
        }
    }

    // The resampler always holds the last frame back as interpolation
    // context for a chunk that will never arrive now (SBS-595); flush it so
    // the recording's last bit of audio isn't silently dropped.
    if let (Some(fmt), Some(astream)) = (audio_fmt.as_ref(), audio_stream) {
        if let Some(r) = resampler.as_mut() {
            let tail = r.flush();
            if !tail.is_empty() {
                // The returned duration has nothing left to advance:
                // Finalize() runs right after this, so audio_cursor's last
                // real use was the loop above.
                unsafe {
                    write_pcm(
                        &writer,
                        astream,
                        &tail,
                        fmt.rate,
                        fmt.channels,
                        audio_cursor,
                    )?;
                }
            }
        }
    }

    unsafe { writer.Finalize().context("finalize mp4")? };
    let _ = session.Close();
    let _ = pool.Close();

    if progress.frames.load(Ordering::Relaxed) == 0 {
        bail!("no frames captured");
    }
    Ok(if want_gif { Some(gif_frames) } else { None })
}

/// Encode collected frames as an animated GIF next to the MP4.
fn write_gif(frames: &[(Vec<u8>, u32, u32)], path: &std::path::Path, fps: u32) -> Result<()> {
    use image::codecs::gif::{GifEncoder, Repeat};
    use image::{Delay, Frame, RgbaImage};

    let mut file = std::fs::File::create(path)?;
    {
        let mut enc = GifEncoder::new_with_speed(&mut file, 12);
        enc.set_repeat(Repeat::Infinite)?;
        let delay = Delay::from_numer_denom_ms(1000 * gif_every(fps), sanitize_fps(fps));
        for (buf, w, h) in frames {
            let Some(img) = RgbaImage::from_raw(*w, *h, buf.clone()) else {
                continue;
            };
            enc.encode_frame(Frame::from_parts(img, 0, 0, delay))?;
        }
    }
    file.sync_all().context("flush gif")?;
    Ok(())
}

/// Decode every frame of a staged GIF.
///
/// `image::open` stops after the first one, so a file cut short by a full disk
/// still opens and would be renamed over the destination as a corrupt
/// recording. Walking the whole animation is what actually proves the write
/// finished. The frames are stepped through rather than collected: the encoder
/// still holds the source frames in memory, and gathering a second copy of a
/// long recording to immediately drop it would double that for nothing.
fn validate_gif(path: &std::path::Path) -> Result<()> {
    use image::AnimationDecoder;

    let file = std::fs::File::open(path).context("reopen gif")?;
    let decoder = image::codecs::gif::GifDecoder::new(std::io::BufReader::new(file))
        .context("read gif header")?;
    let mut frames = 0usize;
    for frame in decoder.into_frames() {
        frame.context("decode gif frame")?;
        frames += 1;
    }
    if frames == 0 {
        bail!("gif has no frames");
    }
    Ok(())
}

fn partial_gif_path(destination: &std::path::Path, id: u64) -> std::path::PathBuf {
    let parent = destination
        .parent()
        .unwrap_or_else(|| std::path::Path::new(""));
    let stem = destination
        .file_stem()
        .map(|value| value.to_string_lossy())
        .unwrap_or_default();
    parent.join(format!("{stem}.partial-{}-{id}.gif", std::process::id()))
}

fn publish_recording_with(
    partial: &std::path::Path,
    destination: &std::path::Path,
    rename: impl FnOnce(&std::path::Path, &std::path::Path) -> std::io::Result<()>,
) -> Result<()> {
    // Validation has already succeeded, so the partial is user data now.
    // Never remove it on a publication error: its explicit path is the
    // recovery mechanism for a destination collision or transient lock.
    rename(partial, destination).with_context(|| {
        format!(
            "publish finalized recording; recovery file kept at {}",
            partial.display()
        )
    })
}

fn publish_gif_with(
    frames: &[(Vec<u8>, u32, u32)],
    destination: &std::path::Path,
    id: u64,
    fps: u32,
    rename: impl FnOnce(&std::path::Path, &std::path::Path) -> std::io::Result<()>,
) -> Result<()> {
    let partial = partial_gif_path(destination, id);
    let _ = std::fs::remove_file(&partial);
    let staged = (|| {
        write_gif(frames, &partial, fps).context("encode gif")?;
        validate_gif(&partial).context("validate gif")?;
        Ok(())
    })();
    if let Err(error) = staged {
        let _ = std::fs::remove_file(&partial);
        return Err(error);
    }
    rename(&partial, destination)
        .with_context(|| format!("publish gif; recovery file kept at {}", partial.display()))
}

/// Record `target` until the user stops. Blocks on the caller's (main)
/// thread running the stop-pill message loop; returns the saved paths.
pub fn session(target: Target, want_gif: bool) -> Result<()> {
    if let Target::Region(rect, monitor) = target {
        let monitor = HMONITOR(monitor as *mut _);
        if !crate::window::monitor_contains_rect(monitor, rect) {
            bail!(
                "Recording regions must stay on one monitor. Select a smaller region or record the full window."
            );
        }
    }
    let cfg = crate::config::Config::load();
    let fps = cfg.record_fps();
    let audio_source = match cfg.record_audio.as_str() {
        "system" => Some(crate::audio::Source::System),
        "mic" => Some(crate::audio::Source::Mic),
        _ => None,
    };
    crate::diagnostics::log(&format!(
        "recording start target={} audio={} gif={want_gif} fps={fps}",
        if matches!(target, Target::Window(_)) {
            "window"
        } else {
            "region"
        },
        cfg.record_audio
    ));
    let dir = cfg.video_dir();
    std::fs::create_dir_all(&dir)?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S%3f");
    let mp4 = dir.join(format!("matteshot-{stamp}.mp4"));
    let record_id = NEXT_RECORD_ID.fetch_add(1, Ordering::Relaxed);
    let partial_mp4 = crate::output::partial_video_path(&mp4, record_id);
    let gif = dir.join(format!("matteshot-{stamp}.gif"));
    let _ = std::fs::remove_file(&partial_mp4);

    let progress = Arc::new(Progress {
        stop: AtomicBool::new(false),
        frames: AtomicU32::new(0),
        started: std::time::Instant::now(),
        error: Mutex::new(None),
    });

    // The freeze-frame selector owned foreground while the target was
    // chosen. Restore the selected window before the capture worker starts;
    // hardware-rendered apps may pause or tear down their visible swap chain
    // as soon as they lose activation.
    if let Target::Window(h) = target {
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(HWND(h as *mut _));
        }
    }

    let worker = {
        let progress = progress.clone();
        let partial_mp4 = partial_mp4.clone();
        std::thread::spawn(move || {
            match capture_loop(
                target,
                partial_mp4,
                want_gif,
                fps,
                audio_source,
                progress.clone(),
            ) {
                Ok(frames) => frames,
                Err(e) => {
                    *progress.error.lock().unwrap() = Some(format!("{e:#}"));
                    progress.stop.store(true, Ordering::Relaxed);
                    None
                }
            }
        })
    };

    // Stop pill owns the main thread until the user stops. Always signal and
    // join the capture worker even if creating or running the controls fails.
    let ui_result = crate::recui::run(progress.clone(), target);
    progress.stop.store(true, Ordering::Relaxed);
    let worker_deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !worker.is_finished() {
        if std::time::Instant::now() >= worker_deadline {
            crate::diagnostics::log("recording finalize timeout");
            bail!(
                "The recorder did not finish within 60 seconds. The incomplete file was quarantined and will be removed after restart."
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let worker_result = worker.join();

    if let Err(error) = ui_result {
        crate::diagnostics::log("recording controls failed");
        let _ = std::fs::remove_file(&partial_mp4);
        let _ = std::fs::remove_file(&gif);
        return Err(error).context("recording controls failed");
    }
    let gif_frames = match worker_result {
        Ok(frames) => frames,
        Err(_) => {
            crate::diagnostics::log("recording worker panic");
            let _ = std::fs::remove_file(&partial_mp4);
            let _ = std::fs::remove_file(&gif);
            bail!("recorder worker panicked");
        }
    };

    if let Some(err) = progress.error.lock().unwrap().clone() {
        crate::diagnostics::log("recording capture failed");
        let _ = std::fs::remove_file(&partial_mp4);
        let _ = std::fs::remove_file(&gif);
        bail!(err);
    }

    if let Err(error) = crate::trim::validate_video(&partial_mp4) {
        crate::diagnostics::log("recording validation failed");
        let _ = std::fs::remove_file(&partial_mp4);
        let _ = std::fs::remove_file(&gif);
        return Err(error).context("recording failed its final integrity check");
    }
    if let Err(error) =
        publish_recording_with(&partial_mp4, &mp4, |from, to| std::fs::rename(from, to))
    {
        crate::diagnostics::log("recording publish failed");
        let _ = std::fs::remove_file(&gif);
        return Err(error);
    }

    let mut gif_saved = None;
    let mut gif_failed = false;
    if let Some(frames) = gif_frames {
        if !frames.is_empty() {
            match publish_gif_with(&frames, &gif, record_id, fps, |from, to| {
                std::fs::rename(from, to)
            }) {
                Ok(()) => gif_saved = Some(gif),
                Err(error) => {
                    gif_failed = true;
                    crate::diagnostics::log("recording gif failed");
                    eprintln!("recording gif failed: {error:#}");
                }
            }
        }
    }
    let frames = progress.frames.load(Ordering::Relaxed);
    let secs = progress.started.elapsed().as_secs();
    crate::diagnostics::log(&format!(
        "recording complete frames={frames} seconds={secs}"
    ));
    eprintln!("recorded {frames} frames -> {}", mp4.display());
    // The file itself on the clipboard: paste straight into chat or a ticket.
    let mut notices = Vec::new();
    if gif_failed {
        notices.push("GIF unavailable");
    }
    match crate::output::file_to_clipboard(&mp4) {
        Ok(()) => {}
        Err(error) => {
            crate::diagnostics::log("recording clipboard copy failed");
            eprintln!("recording clipboard copy failed: {error:#}");
            notices.push("clipboard unavailable");
        }
    }
    let initial_status =
        (!notices.is_empty()).then(|| format!("recording saved · {}", notices.join(" · ")));
    // And a review window so stopping never feels like the recording vanished.
    let _ = crate::recdone::show(mp4, gif_saved, frames, secs, initial_status);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_rates_are_constrained_and_gif_cadence_stays_constant() {
        assert_eq!(sanitize_fps(30), 30);
        assert_eq!(sanitize_fps(60), 60);
        assert_eq!(sanitize_fps(144), 30);
        assert_eq!(gif_every(30), 3);
        assert_eq!(gif_every(60), 6);
    }

    #[test]
    fn sixty_fps_configures_encoder_rate_bitrate_and_keyframes() {
        assert_eq!(
            video_encoding(1920, 1080, 60),
            VideoEncoding { fps: 60, bitrate: 14_929_920, keyframe_spacing: 60 }
        );
    }

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "matteshot-record-{label}-{}-{unique}",
            std::process::id()
        ))
    }

    #[test]
    fn validated_recording_survives_a_publish_failure() {
        let dir = temp_dir("publish-failure");
        std::fs::create_dir_all(&dir).unwrap();
        let partial = dir.join("capture.partial.mp4");
        let destination = dir.join("capture.mp4");
        std::fs::write(&partial, b"validated recording").unwrap();

        let result = publish_recording_with(&partial, &destination, |_, _| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "locked",
            ))
        });

        assert!(result.is_err());
        assert_eq!(std::fs::read(&partial).unwrap(), b"validated recording");
        assert!(!destination.exists());
        std::fs::remove_file(partial).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn gif_publish_failure_keeps_a_valid_recovery_file() {
        let dir = temp_dir("gif-publish-failure");
        std::fs::create_dir_all(&dir).unwrap();
        let destination = dir.join("capture.gif");
        let frames = vec![(vec![255, 0, 0, 255], 1, 1)];

        let result = publish_gif_with(&frames, &destination, 7, DEFAULT_FPS, |_, _| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "locked",
            ))
        });
        let partial = partial_gif_path(&destination, 7);

        assert!(result.is_err());
        assert!(image::open(&partial).is_ok());
        assert!(!destination.exists());
        std::fs::remove_file(partial).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn a_gif_cut_short_by_a_full_disk_fails_validation() {
        let dir = temp_dir("gif-truncated");
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("whole.gif");
        let frames: Vec<(Vec<u8>, u32, u32)> = (0..6)
            .map(|n| (vec![n * 40, 0, 255 - n * 40, 255], 1, 1))
            .collect();
        write_gif(&frames, &whole, DEFAULT_FPS).unwrap();
        assert!(validate_gif(&whole).is_ok(), "a complete gif must validate");

        // Same file with the tail lost, as a write that ran out of disk would
        // leave it. The header and first frame survive, which is exactly why
        // `image::open` was not enough to catch this.
        let bytes = std::fs::read(&whole).unwrap();
        let cut = dir.join("cut.gif");
        std::fs::write(&cut, &bytes[..bytes.len() * 2 / 3]).unwrap();
        assert!(image::open(&cut).is_ok(), "the weakness this guards is gone");
        assert!(validate_gif(&cut).is_err(), "a truncated gif must not validate");

        std::fs::remove_file(whole).unwrap();
        std::fs::remove_file(cut).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn high_refresh_frames_share_one_thirty_fps_slot() {
        let interval = 10_000_000 / 30;
        assert_eq!(encoder_slot(0, interval), 0);
        assert_eq!(encoder_slot(interval / 2, interval), 0);
        assert_eq!(encoder_slot(interval + 1, interval), 1);
    }

    #[test]
    fn resized_windows_are_letterboxed_without_distortion() {
        let mut source = vec![0u8; 4 * 2 * 4];
        for pixel in source.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[0, 0, 255, 255]);
        }
        let output = letterbox_bgra(&source, 4, 2, 4, 4);
        assert_eq!(&output[0..4], &[0, 0, 0, 255]);
        assert_eq!(&output[(4 * 4)..(4 * 4 + 4)], &[0, 0, 255, 255]);
        assert_eq!(&output[(3 * 4 * 4)..(3 * 4 * 4 + 4)], &[0, 0, 0, 255]);
    }

    #[test]
    fn blank_window_detection_ignores_normal_dark_content() {
        let mut blank = vec![0u8; 32 * 4];
        for pixel in blank.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
        assert!(nearly_blank_bgra(&blank));
        blank[0..4].copy_from_slice(&[80, 80, 80, 255]);
        assert!(!nearly_blank_bgra(&blank));
    }
}
