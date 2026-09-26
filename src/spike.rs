//! DPI supersampling spike (experimental, `--spike-dpi`).
//!
//! Theory: PMv2 apps re-render on WM_DPICHANGED, and the canonical handler
//! reads the new DPI from wParam instead of re-querying the monitor. So a
//! synthetic WM_DPICHANGED with a doubled DPI + doubled suggested rect may
//! coax a window into rendering at 2x on a 1x display. Capture while it's
//! doubled, then restore. This probe measures what actually happens per app.

use anyhow::{Context, Result};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowRect, SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_DPICHANGED,
};

use crate::capture;

fn send_dpichanged(hwnd: HWND, dpi: u32, rect: &RECT) {
    let wparam = WPARAM(((dpi << 16) | dpi) as usize);
    let lparam = LPARAM(rect as *const RECT as isize);
    unsafe {
        // SendMessageTimeout so a stuck app can't hang the probe.
        let _ = SendMessageTimeoutW(
            hwnd,
            WM_DPICHANGED,
            wparam,
            lparam,
            SMTO_ABORTIFHUNG,
            2000,
            None,
        );
    }
}

pub fn run(hwnd: HWND, title: &str) -> Result<()> {
    let dpi_before = unsafe { GetDpiForWindow(hwnd) };
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect)? };
    let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
    eprintln!(
        "spike[{title}]: dpi={dpi_before}, window {w}x{h} at ({},{})",
        rect.left, rect.top
    );

    let before = capture::capture_window(hwnd).context("before capture")?;
    eprintln!(
        "spike[{title}]: before capture {}x{}",
        before.width(),
        before.height()
    );

    // Ask for double DPI with a doubled suggested rect anchored in place.
    let target_dpi = dpi_before * 2;
    let doubled = RECT {
        left: rect.left,
        top: rect.top,
        right: rect.left + w * 2,
        bottom: rect.top + h * 2,
    };
    send_dpichanged(hwnd, target_dpi, &doubled);
    std::thread::sleep(std::time::Duration::from_millis(600));

    let mut mid_rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut mid_rect)? };
    let (mw, mh) = (
        mid_rect.right - mid_rect.left,
        mid_rect.bottom - mid_rect.top,
    );
    let after = capture::capture_window(hwnd).context("after capture")?;
    eprintln!(
        "spike[{title}]: after DPICHANGED({target_dpi}) window {mw}x{mh}, capture {}x{}",
        after.width(),
        after.height()
    );

    // Restore.
    send_dpichanged(hwnd, dpi_before, &rect);
    std::thread::sleep(std::time::Duration::from_millis(400));
    let mut back_rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut back_rect)? };
    eprintln!(
        "spike[{title}]: restored to {}x{}",
        back_rect.right - back_rect.left,
        back_rect.bottom - back_rect.top
    );

    let dir = std::env::temp_dir().join("matteshot-spike");
    std::fs::create_dir_all(&dir)?;
    let safe: String = title
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(20)
        .collect();
    let p1 = dir.join(format!("{safe}-before.png"));
    let p2 = dir.join(format!("{safe}-after.png"));
    before.save(&p1)?;
    after.save(&p2)?;
    eprintln!(
        "spike[{title}]: saved {} and {}",
        p1.display(),
        p2.display()
    );

    let verdict = if after.width() >= before.width() * 2 - 8 {
        "window doubled — check whether content is genuinely 2x-rendered or just relaid-out"
    } else if after.width() > before.width() + 8 {
        "window grew but not 2x — partial honor"
    } else {
        "no size change — app ignored the synthetic DPICHANGED"
    };
    eprintln!("spike[{title}]: verdict: {verdict}");
    Ok(())
}
