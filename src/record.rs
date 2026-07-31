//! Screen recording: WGC frames → Media Foundation H.264/MP4, with an
//! optional downsampled GIF track. Capture runs on a worker thread; the
//! main thread shows a floating stop pill that excludes itself from the
//! recording (WDA_EXCLUDEFROMCAPTURE).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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
    MFCreateSinkWriterFromURL, MFStartup, MFVideoFormat_H264, MFVideoFormat_RGB32,
    MFVideoInterlace_Progressive, MFMediaType_Video, MFSTARTUP_FULL, MF_MT_AVG_BITRATE,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_VERSION,
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

const FPS: u32 = 30;
/// GIF sampling: every Nth frame, capped so memory stays bounded.
const GIF_EVERY: u32 = 3;
const GIF_MAX_FRAMES: usize = 240;
/// GIFs balloon fast (no interframe compression here) — keep them share-sized.
const GIF_MAX_WIDTH: u32 = 480;

fn even(v: i32) -> u32 {
    (v.max(2) as u32) & !1
}

pub unsafe fn make_sink(
    path: &std::path::Path,
    w: u32,
    h: u32,
    audio: Option<&crate::audio::Format>,
) -> Result<(IMFSinkWriter, u32, Option<u32>)> {
    make_sink_for_content(path, w, h, w, h, audio)
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
    audio: Option<&crate::audio::Format>,
) -> Result<(IMFSinkWriter, u32, Option<u32>)> {
    let writer: IMFSinkWriter =
        MFCreateSinkWriterFromURL(&HSTRING::from(path.as_os_str()), None, None)
            .context("create sink writer")?;

    // Output: H.264. 0.12 bits per moving-content pixel per frame preserves
    // crisp UI and fast motion without treating static matte padding as if it
    // were another full frame of changing content.
    let bitrate = ((content_w * content_h) as f32 * FPS as f32 * 0.12) as u32;
    let out = MFCreateMediaType()?;
    out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
    out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate.clamp(1_500_000, 40_000_000))?;
    out.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64)?;
    out.SetUINT64(&MF_MT_FRAME_RATE, ((FPS as u64) << 32) | 1)?;
    out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
    let stream = writer.AddStream(&out).context("add stream")?;

    // Input: BGRA32, top-down (positive stride).
    let inp = MFCreateMediaType()?;
    inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
    inp.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64)?;
    inp.SetUINT64(&MF_MT_FRAME_RATE, ((FPS as u64) << 32) | 1)?;
    inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
    inp.SetUINT32(&MF_MT_DEFAULT_STRIDE, w * 4)?;
    writer
        .SetInputMediaType(stream, &inp, None)
        .context("set input type (no H.264 encoder?)")?;

    // Optional AAC audio track fed with float PCM.
    let audio_stream = if let Some(fmt) = audio {
        use windows::Win32::Media::MediaFoundation::{
            MFAudioFormat_AAC, MFMediaType_Audio,
            MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE,
            MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS,
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
        SizeInt32 { Width: item_size.Width.max(2), Height: item_size.Height.max(2) },
    )?;
    let session = pool.CreateCaptureSession(&item)?;
    let _ = session.SetIsCursorCaptureEnabled(true);
    let _ = session.SetIsBorderRequired(false);
    session.StartCapture()?;

    let (writer, stream, audio_stream) =
        unsafe { make_sink(&path, out_w, out_h, audio_enc.as_ref())? };

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
        audio_fmt = Some(crate::audio::Format { rate: enc.rate, channels: enc.channels });
    }
    let mut audio_cursor: i64 = 0;

    let mut gif_frames: Vec<(Vec<u8>, u32, u32)> = Vec::new();
    let mut first_ts: Option<i64> = None;
    let mut last_ts: i64 = 0;
    let frame_interval = 10_000_000i64 / FPS as i64;
    let mut staging: Option<ID3D11Texture2D> = None;
    let mut staging_desc = D3D11_TEXTURE2D_DESC::default();

    while !progress.stop.load(Ordering::Relaxed) {
        let frame = match pool.TryGetNextFrame() {
            Ok(f) => f,
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(4));
                continue;
            }
        };

        let ts = frame.SystemRelativeTime()?.Duration;
        let base = *first_ts.get_or_insert(ts);
        let rel = (ts - base).max(0);
        // Drop frames that would land on the same encoder slot.
        if progress.frames.load(Ordering::Relaxed) > 0 && rel - last_ts < frame_interval / 2 {
            continue;
        }

        let surface = frame.Surface()?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
        let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };

        if staging.is_none() || staging_desc.Width != desc.Width || staging_desc.Height != desc.Height
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

        // Copy out the (possibly cropped) frame as tightly packed BGRA.
        let (src_x, src_y) = match crop {
            Some(c) => (c.left.max(0) as u32, c.top.max(0) as u32),
            None => (0, 0),
        };
        let row_bytes = (out_w * 4) as usize;
        let mut buf = vec![0u8; row_bytes * out_h as usize];
        unsafe {
            let src = mapped.pData as *const u8;
            for y in 0..out_h {
                let sy = (src_y + y).min(desc.Height.saturating_sub(1));
                let src_row = src.add(sy as usize * mapped.RowPitch as usize + src_x as usize * 4);
                std::ptr::copy_nonoverlapping(
                    src_row,
                    buf.as_mut_ptr().add(y as usize * row_bytes),
                    row_bytes.min((desc.Width.saturating_sub(src_x) * 4) as usize),
                );
            }
            context.Unmap(stage, 0);
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
            sample.SetSampleTime(rel)?;
            sample.SetSampleDuration(frame_interval)?;
            writer.WriteSample(stream, &sample)?;
        }

        let n = progress.frames.fetch_add(1, Ordering::Relaxed);
        last_ts = rel;

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
                while audio_cursor < rel - 600_000 {
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

        if want_gif && n.is_multiple_of(GIF_EVERY) && gif_frames.len() < GIF_MAX_FRAMES {
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
                    let i = (sy.min(out_h - 1) as usize * row_bytes)
                        + (sx.min(out_w - 1) as usize * 4);
                    rgba.extend_from_slice(&[buf[i + 2], buf[i + 1], buf[i], 255]);
                }
            }
            gif_frames.push((rgba, gw, gh));
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
fn write_gif(frames: &[(Vec<u8>, u32, u32)], path: &std::path::Path) -> Result<()> {
    use image::codecs::gif::{GifEncoder, Repeat};
    use image::{Delay, Frame, RgbaImage};

    let file = std::fs::File::create(path)?;
    let mut enc = GifEncoder::new_with_speed(file, 12);
    enc.set_repeat(Repeat::Infinite)?;
    let delay = Delay::from_numer_denom_ms(1000 * GIF_EVERY, FPS);
    for (buf, w, h) in frames {
        let Some(img) = RgbaImage::from_raw(*w, *h, buf.clone()) else { continue };
        enc.encode_frame(Frame::from_parts(img, 0, 0, delay))?;
    }
    Ok(())
}

/// Record `target` until the user stops. Blocks on the caller's (main)
/// thread running the stop-pill message loop; returns the saved paths.
pub fn session(target: Target, want_gif: bool) -> Result<()> {
    let cfg = crate::config::Config::load();
    let audio_source = match cfg.record_audio.as_str() {
        "system" => Some(crate::audio::Source::System),
        "mic" => Some(crate::audio::Source::Mic),
        _ => None,
    };
    let dir = cfg.video_dir();
    std::fs::create_dir_all(&dir)?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let mp4 = dir.join(format!("matteshot-{stamp}.mp4"));
    let gif = dir.join(format!("matteshot-{stamp}.gif"));

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
        let mp4 = mp4.clone();
        std::thread::spawn(move || match capture_loop(
            target,
            mp4,
            want_gif,
            audio_source,
            progress.clone(),
        ) {
            Ok(frames) => frames,
            Err(e) => {
                *progress.error.lock().unwrap() = Some(format!("{e:#}"));
                progress.stop.store(true, Ordering::Relaxed);
                None
            }
        })
    };

    // Stop pill owns the main thread until the user stops.
    crate::recui::run(progress.clone(), target)?;
    progress.stop.store(true, Ordering::Relaxed);
    let gif_frames = worker.join().map_err(|_| anyhow::anyhow!("recorder panicked"))?;

    if let Some(err) = progress.error.lock().unwrap().clone() {
        bail!(err);
    }

    let mut gif_saved = None;
    if let Some(frames) = gif_frames {
        if !frames.is_empty() && write_gif(&frames, &gif).is_ok() {
            gif_saved = Some(gif);
        }
    }
    let frames = progress.frames.load(Ordering::Relaxed);
    let secs = progress.started.elapsed().as_secs();
    eprintln!("recorded {frames} frames -> {}", mp4.display());
    // The file itself on the clipboard: paste straight into chat or a ticket.
    let _ = crate::output::file_to_clipboard(&mp4);
    // And a review window so stopping never feels like the recording vanished.
    let _ = crate::recdone::show(mp4, gif_saved, frames, secs);
    Ok(())
}
