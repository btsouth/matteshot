//! Single-frame window capture via Windows.Graphics.Capture.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use image::RgbaImage;
use windows::core::Interface;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

thread_local! {
    /// D3D device creation costs tens of ms; cache per thread. All captures
    /// run on the main thread in practice.
    static DEVICE: std::cell::OnceCell<(ID3D11Device, ID3D11DeviceContext)> =
        const { std::cell::OnceCell::new() };
    /// The WinRT device wrapper around the cached D3D one. Rebuilding it per
    /// capture cost several milliseconds for no reason.
    static WINRT_DEVICE: std::cell::OnceCell<IDirect3DDevice> =
        const { std::cell::OnceCell::new() };
}

/// A staging texture for reading `desc` back on the CPU.
///
/// Deliberately allocated per capture. Caching one keyed by size was measured
/// and made no difference (fastest observed 25.7ms fresh vs 26.5ms cached), so
/// the cache was not worth the device-affinity assumption it needed.
fn staging_for(device: &ID3D11Device, desc: &D3D11_TEXTURE2D_DESC) -> Result<ID3D11Texture2D> {
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
        ..*desc
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut texture))? };
    texture.context("create staging texture")
}

fn cached_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    DEVICE.with(|cell| {
        if cell.get().is_none() {
            let pair = create_d3d_device()?;
            let _ = cell.set(pair);
        }
        Ok(cell.get().unwrap().clone())
    })
}

fn create_d3d_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    let mut result = unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    };
    if result.is_err() {
        // Fall back to software rendering (VMs, remote sessions).
        result = unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_WARP,
                None,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        };
    }
    result.context("D3D11CreateDevice failed")?;
    Ok((device.unwrap(), context.unwrap()))
}

/// Create the D3D device ahead of time so the first real capture is warm.
pub fn warmup() {
    let _ = cached_device();
}

/// A fresh D3D11 device/context pair (the recorder runs on its own thread
/// and must not share the thread-local one).
pub fn device_pair() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    create_d3d_device()
}

/// The window's on-screen bounds in screen coordinates.
///
/// DWM frame bounds are preferred for normal windows, but layered/hosted
/// surfaces (Ceiling's taskbar widget is a child of Explorer's `Shell_TrayWnd`)
/// return empty or fail outright — fall back to `GetWindowRect`, which still
/// reports where the window actually is.
fn window_frame_bounds(hwnd: HWND) -> Result<RECT> {
    let mut rect = RECT::default();
    let dwm_ok = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut RECT as *mut _,
            std::mem::size_of::<RECT>() as u32,
        )
        .is_ok()
    } && rect.right > rect.left
        && rect.bottom > rect.top;
    if !dwm_ok {
        let mut wr = RECT::default();
        unsafe { GetWindowRect(hwnd, &mut wr) }
            .ok()
            .context("window rect")?;
        rect = wr;
    }
    if rect.right <= rect.left || rect.bottom <= rect.top {
        bail!(
            "window has empty bounds ({}x{})",
            rect.right - rect.left,
            rect.bottom - rect.top
        );
    }
    Ok(rect)
}

/// Capture a window by screenshotting its monitor and cropping to the DWM
/// frame. Used when WGC rejects the HWND (shell-hosted widgets, protected
/// surfaces, empty capture items).
fn capture_window_monitor_crop(hwnd: HWND) -> Result<RgbaImage> {
    let bounds = window_frame_bounds(hwnd)?;
    let mon = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        GetMonitorInfoW(mon, &mut mi)
            .ok()
            .context("monitor info for window crop")?;
    }
    let full = capture_monitor(mon)?;
    let x = (bounds.left - mi.rcMonitor.left).max(0) as u32;
    let y = (bounds.top - mi.rcMonitor.top).max(0) as u32;
    let w = ((bounds.right - bounds.left) as u32)
        .min(full.width().saturating_sub(x))
        .max(1);
    let h = ((bounds.bottom - bounds.top) as u32)
        .min(full.height().saturating_sub(y))
        .max(1);
    if x >= full.width() || y >= full.height() {
        bail!("window bounds are outside the monitor capture");
    }
    Ok(image::imageops::crop_imm(&full, x, y, w, h).to_image())
}

pub fn capture_window_wgc(hwnd: HWND) -> Result<RgbaImage> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd)? };
    // Empty items pass CreateForWindow on some hosted/layered surfaces and
    // only blow up later in CreateFreeThreaded with E_INVALIDARG.
    let size = item.Size()?;
    if size.Width <= 0 || size.Height <= 0 {
        bail!(
            "Could not capture the given window (empty WGC item {}x{})",
            size.Width,
            size.Height
        );
    }
    capture_item(item)
}

pub fn capture_window(hwnd: HWND) -> Result<RgbaImage> {
    match capture_window_wgc(hwnd) {
        Ok(img) => Ok(img),
        Err(wgc_err) => {
            eprintln!("WGC window capture failed ({wgc_err:#}); falling back to monitor crop");
            capture_window_monitor_crop(hwnd).with_context(|| format!("WGC failed: {wgc_err:#}"))
        }
    }
}

pub fn capture_monitor(hmonitor: windows::Win32::Graphics::Gdi::HMONITOR) -> Result<RgbaImage> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(hmonitor)? };
    capture_item(item)
}

/// Capture several monitors as one batch. All WGC sessions are started before
/// any frame is awaited, so the compositor can prepare their first/second
/// frames together instead of charging the overlay one wait per monitor.
pub fn capture_monitors(
    monitors: &[windows::Win32::Graphics::Gdi::HMONITOR],
) -> Result<Vec<RgbaImage>> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    let mut items = Vec::with_capacity(monitors.len());
    for &monitor in monitors {
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(monitor)? };
        items.push(item);
    }
    capture_items(items)
}

struct PendingCapture {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
}

impl Drop for PendingCapture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

fn winrt_device(device: &ID3D11Device) -> Result<IDirect3DDevice> {
    WINRT_DEVICE.with(|cell| -> Result<IDirect3DDevice> {
        if cell.get().is_none() {
            let dxgi: IDXGIDevice = device.cast()?;
            let wrapped: IDirect3DDevice =
                unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
            let _ = cell.set(wrapped);
        }
        Ok(cell.get().unwrap().clone())
    })
}

fn start_capture(item: &GraphicsCaptureItem, device: &IDirect3DDevice) -> Result<PendingCapture> {
    let size = item.Size()?;
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )?;
    let session = pool.CreateCaptureSession(item)?;
    let _ = session.SetIsCursorCaptureEnabled(false);
    // Removing the capture border needs a capability grant on some builds; best-effort.
    let _ = session.SetIsBorderRequired(false);
    session.StartCapture()?;
    Ok(PendingCapture { pool, session })
}

fn wait_for_frames(captures: &[PendingCapture]) -> Result<Vec<Direct3D11CaptureFrame>> {
    struct Slot {
        first: Option<Direct3D11CaptureFrame>,
        second: Option<Direct3D11CaptureFrame>,
    }

    let mut slots: Vec<Slot> = (0..captures.len())
        .map(|_| Slot {
            first: None,
            second: None,
        })
        .collect();
    let first_deadline = Instant::now() + Duration::from_secs(3);
    while slots.iter().any(|slot| slot.first.is_none()) {
        let mut progressed = false;
        for (capture, slot) in captures.iter().zip(&mut slots) {
            if slot.second.is_some() {
                continue;
            }
            if let Ok(frame) = capture.pool.TryGetNextFrame() {
                progressed = true;
                if slot.first.is_none() {
                    slot.first = Some(frame);
                } else {
                    slot.second = Some(frame);
                }
            }
        }
        if Instant::now() >= first_deadline {
            bail!("no capture frame arrived within 3s");
        }
        if !progressed {
            std::thread::sleep(Duration::from_millis(4));
        }
    }

    // The very first WGC frame can be stale or partially composited (seen
    // as missing taskbar in overlay freezes) — prefer the second frame.
    // Poll every active session within one shared window. This preserves the
    // correctness guard while preventing N monitors from paying N x 80ms.
    let second_deadline = Instant::now() + Duration::from_millis(80);
    while slots.iter().any(|slot| slot.second.is_none()) && Instant::now() < second_deadline {
        let mut progressed = false;
        for (capture, slot) in captures.iter().zip(&mut slots) {
            if slot.second.is_none() {
                if let Ok(frame) = capture.pool.TryGetNextFrame() {
                    slot.second = Some(frame);
                    progressed = true;
                }
            }
        }
        if !progressed {
            std::thread::sleep(Duration::from_millis(4));
        }
    }

    slots
        .into_iter()
        .map(|slot| {
            slot.second
                .or(slot.first)
                .context("capture frame disappeared")
        })
        .collect()
}

fn frame_to_image(
    frame: &Direct3D11CaptureFrame,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    timing: bool,
) -> Result<RgbaImage> {
    let t0 = Instant::now();
    let surface = frame.Surface()?;
    let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
    let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };

    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };

    let staging = staging_for(device, &desc)?;
    unsafe { context.CopyResource(&staging, &texture) };

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))? };

    // The texture can be larger than the actual window contents.
    let content = frame.ContentSize()?;
    let width = (content.Width.max(1) as u32).min(desc.Width);
    let height = (content.Height.max(1) as u32).min(desc.Height);

    // Row-wise copy then in-place BGRA->RGBA swap — vastly faster than
    // per-pixel put_pixel over a multi-megapixel monitor.
    let row_bytes = (width * 4) as usize;
    let mut buf = vec![0u8; row_bytes * height as usize];
    unsafe {
        let src = mapped.pData as *const u8;
        for y in 0..height as usize {
            std::ptr::copy_nonoverlapping(
                src.add(y * mapped.RowPitch as usize),
                buf.as_mut_ptr().add(y * row_bytes),
                row_bytes,
            );
        }
        context.Unmap(&staging, 0);
    }
    let t_copy = t0.elapsed();
    for px in buf.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    // Phase timings are opt-in: they are only meaningful on an idle machine,
    // and under load they mislead badly enough to send you optimising the
    // wrong thing. MATTESHOT_CAPTURE_TIMING=1 turns them on.
    if timing {
        eprintln!(
            "  phase: stage+map+copy {:?} bgra-swap {:?}",
            t_copy,
            t0.elapsed() - t_copy
        );
    }
    RgbaImage::from_raw(width, height, buf).context("assemble image")
}

fn capture_items(items: Vec<GraphicsCaptureItem>) -> Result<Vec<RgbaImage>> {
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let t0 = Instant::now();
    let timing = std::env::var_os("MATTESHOT_CAPTURE_TIMING").is_some();
    let (device, context) = cached_device()?;
    let direct3d = winrt_device(&device)?;
    let mut captures = Vec::with_capacity(items.len());
    for item in &items {
        captures.push(start_capture(item, &direct3d)?);
    }
    let t_setup = t0.elapsed();
    let frames = wait_for_frames(&captures)?;
    let t_frames = t0.elapsed();
    if timing {
        eprintln!(
            "  phase: setup {:?} shared-frame-wait {:?} sessions {}",
            t_setup,
            t_frames - t_setup,
            captures.len()
        );
    }

    let mut images = Vec::with_capacity(frames.len());
    for frame in &frames {
        images.push(frame_to_image(frame, &device, &context, timing)?);
    }
    eprintln!(
        "timing: capture batch {} image(s) in {:?}",
        images.len(),
        t0.elapsed()
    );
    Ok(images)
}

fn capture_item(item: GraphicsCaptureItem) -> Result<RgbaImage> {
    capture_items(vec![item])?
        .pop()
        .context("capture returned no image")
}
