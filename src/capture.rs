//! Single-frame window capture via Windows.Graphics.Capture.

use anyhow::{Context, Result};
use image::RgbaImage;
use windows::core::Interface;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

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

pub fn capture_window(hwnd: HWND) -> Result<RgbaImage> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd)? };
    capture_item(item)
}

pub fn capture_monitor(hmonitor: windows::Win32::Graphics::Gdi::HMONITOR) -> Result<RgbaImage> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(hmonitor)? };
    capture_item(item)
}

fn capture_item(item: GraphicsCaptureItem) -> Result<RgbaImage> {
    let t0 = std::time::Instant::now();
    let (device, context) = cached_device()?;
    let winrt_device = WINRT_DEVICE.with(|cell| -> Result<IDirect3DDevice> {
        if cell.get().is_none() {
            let dxgi: IDXGIDevice = device.cast()?;
            let wrapped: IDirect3DDevice =
                unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
            let _ = cell.set(wrapped);
        }
        Ok(cell.get().unwrap().clone())
    })?;

    let size = item.Size()?;

    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &winrt_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )?;
    let session = pool.CreateCaptureSession(&item)?;
    let _ = session.SetIsCursorCaptureEnabled(false);
    // Removing the capture border needs a capability grant on some builds; best-effort.
    let _ = session.SetIsBorderRequired(false);
    session.StartCapture()?;

    let wait_frame = |timeout_ms: u32| {
        for _ in 0..timeout_ms / 4 {
            if let Ok(f) = pool.TryGetNextFrame() {
                return Some(f);
            }
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
        None
    };
    // The very first WGC frame can be stale or partially composited (seen
    // as missing taskbar in overlay freezes) — prefer the second frame.
    // Phase timings are opt-in: they are only meaningful on an idle machine,
    // and under load they mislead badly enough to send you optimising the
    // wrong thing. MATTESHOT_CAPTURE_TIMING=1 turns them on.
    let timing = std::env::var_os("MATTESHOT_CAPTURE_TIMING").is_some();
    let t_setup = t0.elapsed();
    let first = wait_frame(3000).context("no capture frame arrived within 3s")?;
    let t_first = t0.elapsed();
    let frame = wait_frame(80).unwrap_or(first);
    let t_second = t0.elapsed();
    if timing {
        eprintln!(
            "  phase: setup {:?} first-frame {:?} second-frame {:?}",
            t_setup,
            t_first - t_setup,
            t_second - t_first
        );
    }

    let surface = frame.Surface()?;
    let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
    let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };

    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };

    let staging = staging_for(&device, &desc)?;
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
    if timing {
        eprintln!(
            "  phase: stage+map+copy {:?} bgra-swap {:?}",
            t_copy - t_second,
            t0.elapsed() - t_copy
        );
    }
    let img = RgbaImage::from_raw(width, height, buf).context("assemble image")?;

    let _ = session.Close();
    let _ = pool.Close();
    eprintln!("timing: capture {}x{} in {:?}", width, height, t0.elapsed());
    Ok(img)
}
