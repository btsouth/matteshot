//! Scrolling capture: repeatedly grab the target, send a wheel scroll, and
//! stitch the frames into one tall image by measuring how far the content
//! actually moved (never trusting the scroll amount we asked for).

use anyhow::{bail, Context, Result};
use image::RgbaImage;
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect,
    GetMonitorInfoW, InvalidateRect, MonitorFromWindow, SetBkMode, SelectObject, SetTextColor,
    CLEARTYPE_QUALITY, DEFAULT_CHARSET, DT_CENTER, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE,
    HFONT, HMONITOR, MONITORINFO, MONITOR_DEFAULTTOPRIMARY, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_MOUSE, MOUSEEVENTF_WHEEL, MOUSEINPUT, VK_ESCAPE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos,
    GetWindowLongPtrW, PeekMessageW, RegisterClassW, SetCursorPos, SetWindowLongPtrW, CREATESTRUCTW, GWLP_USERDATA, MSG, PM_REMOVE, WM_ERASEBKGND, WM_NCCREATE,
    WM_PAINT, WNDCLASSW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};

/// What to scroll-capture.
#[derive(Clone, Copy)]
pub enum Target {
    Window(HWND, POINT),
    /// Virtual-screen rect on a monitor.
    Region(RECT, HMONITOR, POINT),
}

impl Target {
    /// Test-only/title-driven captures have no overlay click, so preserve the
    /// old centered behavior for that path.
    pub fn centered_window(hwnd: HWND) -> Self {
        let mut r = RECT::default();
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut r);
        }
        Self::Window(
            hwnd,
            POINT { x: (r.left + r.right) / 2, y: (r.top + r.bottom) / 2 },
        )
    }
}

// Generous ceilings — these are runaway guards, not working limits. Long
// articles routinely need hundreds of steps (a 60-step cap once truncated a
// Wikipedia article at 3/4).
const MAX_STEPS: usize = 400;
const MAX_HEIGHT: u32 = 40_000;
/// Wheel notches per step — small enough that frames always overlap.
const NOTCHES: i32 = 3;

struct Pill {
    text: Vec<u16>,
    theme: crate::theme::Theme,
    font: HFONT,
    w: i32,
    h: i32,
}

unsafe extern "system" fn pill_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Pill;
            if let Some(p) = ptr.as_mut() {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                let bg = CreateSolidBrush(p.theme.panel);
                FillRect(hdc, &RECT { left: 0, top: 0, right: p.w, bottom: p.h }, bg);
                let _ = DeleteObject(bg);
                SetBkMode(hdc, TRANSPARENT);
                SelectObject(hdc, p.font);
                SetTextColor(hdc, p.theme.text);
                let mut t = p.text.clone();
                let mut r = RECT { left: 0, top: 0, right: p.w, bottom: p.h };
                DrawTextW(hdc, &mut t, &mut r, DT_CENTER | DT_SINGLELINE | DT_VCENTER);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn show_pill(anchor: HMONITOR) -> Result<(HWND, Box<Pill>)> {
    let theme = crate::theme::current();
    let (w, h) = (240, 40);
    let mut pill = Box::new(Pill {
        text: "Scrolling capture\u{2026}".encode_utf16().collect(),
        theme,
        font: CreateFontW(
            -15,
            0,
            0,
            0,
            500,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            0,
            0,
            CLEARTYPE_QUALITY.0 as u32,
            FF_DONTCARE.0 as u32,
            w!("Segoe UI"),
        ),
        w,
        h,
    });
    let hinstance = GetModuleHandleW(None)?;
    let class = WNDCLASSW {
        lpfnWndProc: Some(pill_proc),
        hInstance: hinstance.into(),
        lpszClassName: w!("matteshot_scrollpill"),
        ..Default::default()
    };
    RegisterClassW(&class);
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let _ = GetMonitorInfoW(anchor, &mut mi);
    let x = mi.rcWork.left + (mi.rcWork.right - mi.rcWork.left - w) / 2;
    let y = mi.rcWork.top + 40;
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        w!("matteshot_scrollpill"),
        w!("Matteshot"),
        WS_POPUP | WS_VISIBLE,
        x,
        y,
        w,
        h,
        None,
        None,
        hinstance,
        Some(&mut *pill as *mut Pill as *const _),
    )?;
    // Keep our own progress out of the captured frames.
    let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowDisplayAffinity(
        hwnd,
        windows::Win32::UI::WindowsAndMessaging::WDA_EXCLUDEFROMCAPTURE,
    );
    Ok((hwnd, pill))
}

fn pump() {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            DispatchMessageW(&msg);
        }
    }
}

fn esc_pressed() -> bool {
    unsafe { GetAsyncKeyState(VK_ESCAPE.0 as i32) as u16 & 0x8000 != 0 }
}

fn grab(target: Target) -> Result<RgbaImage> {
    match target {
        Target::Window(h, _) => crate::capture::capture_window(h),
        Target::Region(r, mon, _) => {
            let full = crate::capture::capture_monitor(mon)?;
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            unsafe {
                let _ = GetMonitorInfoW(mon, &mut mi);
            }
            let (x, y) = (
                (r.left - mi.rcMonitor.left).max(0) as u32,
                (r.top - mi.rcMonitor.top).max(0) as u32,
            );
            let (w, h) = ((r.right - r.left).max(1) as u32, (r.bottom - r.top).max(1) as u32);
            Ok(image::imageops::crop_imm(
                &full,
                x.min(full.width().saturating_sub(1)),
                y.min(full.height().saturating_sub(1)),
                w.min(full.width() - x.min(full.width() - 1)),
                h.min(full.height() - y.min(full.height() - 1)),
            )
            .to_image())
        }
    }
}

fn clamp_anchor(anchor: POINT, rect: RECT) -> POINT {
    // Keep the point just inside the target. A drag starts on the selected
    // region's edge, and exact border coordinates can route the wheel to an
    // adjacent control in some apps.
    let left = rect.left.min(rect.right - 1);
    let right = (rect.right - 1).max(left);
    let top = rect.top.min(rect.bottom - 1);
    let bottom = (rect.bottom - 1).max(top);
    POINT {
        x: anchor.x.clamp(left, right),
        y: anchor.y.clamp(top, bottom),
    }
}

/// Mean per-channel difference of one row between two frames.
fn row_diff(a: &RgbaImage, b: &RgbaImage, y: u32) -> f32 {
    let w = a.width();
    let step = (w / 96).max(1);
    let (mut acc, mut n) = (0f64, 0u32);
    let mut x = 0;
    while x < w {
        let (p, q) = (a.get_pixel(x, y), b.get_pixel(x, y));
        acc += (p[0] as i32 - q[0] as i32).unsigned_abs() as f64
            + (p[1] as i32 - q[1] as i32).unsigned_abs() as f64
            + (p[2] as i32 - q[2] as i32).unsigned_abs() as f64;
        n += 1;
        x += step;
    }
    (acc / (n.max(1) as f64 * 3.0)) as f32
}

/// Mean difference across the viewport with no shift applied: "did anything
/// move at all?". Without this, periodic content (log lines, tables) at the
/// bottom of a page matches itself at one line-height and the stitcher
/// appends duplicates forever instead of stopping.
fn viewport_diff(prev: &RgbaImage, next: &RgbaImage, view_top: u32, view_bottom: u32) -> f32 {
    if view_bottom <= view_top {
        return 0.0;
    }
    let (mut acc, mut n) = (0f32, 0u32);
    let mut y = view_top;
    let step = ((view_bottom - view_top) / 60).max(1);
    while y < view_bottom {
        acc += row_diff(prev, next, y);
        n += 1;
        y += step;
    }
    acc / n.max(1) as f32
}

/// Rows of unchanging chrome at the top and bottom (toolbars, status bars);
/// without this every seam repeats the footer.
///
/// Just "unchanged between frames", contiguous from each edge. Erring
/// toward over-detection is safe: the tail we append is taken from the
/// bottom of the viewport, so a viewport that is smaller than reality still
/// captures every row — while under-detecting the footer makes it repeat at
/// every seam, which is the visible failure.
fn static_edges(prev: &RgbaImage, next: &RgbaImage) -> (u32, u32) {
    let h = prev.height();
    let limit = h / 4;
    const SAME: f32 = 2.5;

    let mut top = 0;
    while top < limit && row_diff(prev, next, top) < SAME {
        top += 1;
    }
    let mut bottom = 0;
    while bottom < limit && row_diff(prev, next, h - 1 - bottom) < SAME {
        bottom += 1;
    }
    // Degenerate (nothing moved anywhere, or uniform content): trust none.
    if top + bottom > h / 2 {
        return (0, 0);
    }
    (top, bottom)
}

/// How far did content move between two frames? Compares a band from the
/// middle of the scrollable viewport against the previous frame at each
/// candidate shift. Returns (shift, score) — lower score is better.
fn measure_shift(
    prev: &RgbaImage,
    next: &RgbaImage,
    max_shift: u32,
    view_top: u32,
    view_bottom: u32,
) -> (u32, f32, u32) {
    let (w, h) = (prev.width(), prev.height());
    if next.width() != w || next.height() != h || view_bottom <= view_top + 60 {
        return (0, f32::MAX, 0);
    }
    let view_h = view_bottom - view_top;
    let band_h = (view_h / 4).clamp(24, 160);
    let band_top = view_top + view_h / 6;
    let col_step = (w / 64).max(1);
    let row_step = 2u32;

    let mut best = (0u32, f32::MAX);
    let ceiling = max_shift.min(view_bottom.saturating_sub(band_top + band_h + 1));
    for s in 1..=ceiling {
        let mut acc = 0f64;
        let mut n = 0u32;
        let mut y = 0;
        while y < band_h {
            let ny = band_top + y;
            let py = band_top + y + s;
            let mut x = 0;
            while x < w {
                let a = next.get_pixel(x, ny);
                let b = prev.get_pixel(x, py);
                acc += (a[0] as i32 - b[0] as i32).unsigned_abs() as f64
                    + (a[1] as i32 - b[1] as i32).unsigned_abs() as f64
                    + (a[2] as i32 - b[2] as i32).unsigned_abs() as f64;
                n += 1;
                x += col_step;
            }
            y += row_step;
        }
        let score = (acc / (n.max(1) as f64 * 3.0)) as f32;
        if score < best.1 {
            best = (s, score);
        }
    }
    (best.0, best.1, ceiling)
}

/// Scroll-capture `target` into one tall image.
pub fn capture(target: Target) -> Result<RgbaImage> {
    let anchor = match target {
        Target::Window(h, _) => unsafe { MonitorFromWindow(h, MONITOR_DEFAULTTOPRIMARY) },
        Target::Region(_, m, _) => m,
    };

    // Put the cursor exactly where the user chose so wheel routing follows
    // their intent: page body scrolls the page; a nested pane scrolls itself.
    let hover = match target {
        Target::Window(h, point) => {
            let mut r = RECT::default();
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(h, &mut r);
            }
            clamp_anchor(point, r)
        }
        Target::Region(r, _, point) => clamp_anchor(point, r),
    };
    let mut saved = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut saved);
        let _ = SetCursorPos(hover.x, hover.y);
        // Let the target process the move before the first wheel event.
        std::thread::sleep(std::time::Duration::from_millis(120));
    }
    eprintln!("scroll: wheel anchor {},{}", hover.x, hover.y);

    let (pill, mut pill_state) = unsafe { show_pill(anchor)? };
    pump();

    let first = grab(target).context("first frame")?;
    let (fw, fh) = (first.width(), first.height());
    if fh < 80 {
        unsafe {
            let _ = DestroyWindow(pill);
        }
        bail!("target too short to scroll-capture");
    }
    let mut canvas = first.clone();
    let mut prev = first.clone();
    let mut steps = 0usize;
    let mut aborted = false;
    // Learned on the first successful step, then held steady.
    let mut chrome: Option<(u32, u32)> = None;
    // Apps scroll a consistent amount per notch; that's a strong prior.
    let mut last_shift: Option<u32> = None;

    while steps < MAX_STEPS && canvas.height() < MAX_HEIGHT {
        if esc_pressed() {
            aborted = true;
            break;
        }
        // Scroll down.
        unsafe {
            // Some apps move the pointer as controls disappear during a
            // scroll. Reassert the chosen target before every wheel event.
            let _ = SetCursorPos(hover.x, hover.y);
            let input = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: 0,
                        dy: 0,
                        mouseData: (-120 * NOTCHES) as u32,
                        dwFlags: MOUSEEVENTF_WHEEL,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        }
        // The viewport we know so far (whole frame until chrome is learned).
        let (vt, vb) = chrome.map(|(t, b)| (t, fh - b)).unwrap_or((0, fh));

        // Wait for the scroll to settle rather than guessing a delay: apps
        // animate at wildly different speeds, so grab until two consecutive
        // frames agree (or we run out of patience).
        std::thread::sleep(std::time::Duration::from_millis(110));
        pump();
        let mut next = match grab(target) {
            Ok(f) => f,
            Err(_) => break,
        };
        let settle_by = std::time::Instant::now() + std::time::Duration::from_millis(700);
        loop {
            std::thread::sleep(std::time::Duration::from_millis(70));
            pump();
            let Ok(again) = grab(target) else { break };
            let moving = viewport_diff(&next, &again, vt, vb);
            next = again;
            if moving < 1.0 || std::time::Instant::now() > settle_by {
                break;
            }
        }

        // Did anything move at all? This is the honest bottom-of-page test —
        // and it also catches apps that ignore the wheel entirely.
        let mut idle = viewport_diff(&prev, &next, vt, vb);
        if idle < 2.5 {
            std::thread::sleep(std::time::Duration::from_millis(280));
            pump();
            match grab(target) {
                Ok(retry) => {
                    idle = viewport_diff(&prev, &retry, vt, vb);
                    next = retry;
                }
                Err(_) => break,
            }
            if idle < 2.5 {
                break; // genuinely at the bottom
            }
        }

        let (shift, score, ceiling) = measure_shift(&prev, &next, fh / 2, vt, vb);
        if std::env::var("MATTESHOT_SCROLL_DEBUG").is_ok() {
            eprintln!(
                "step {steps}: shift={shift} score={score:.2} idle={idle:.2} view={vt}..{vb}"
            );
        }
        // Trust the match if it explains the frame far better than "no
        // movement" does — or, when the page has animated content that keeps
        // scores high (video, GIFs, spinners), if the shift agrees with the
        // steady scroll rate we've already established.
        let convincing = score < idle * 0.5;
        let consistent = last_shift
            .map(|p| score < 12.0 && shift.abs_diff(p) * 8 <= p)
            .unwrap_or(false);
        if shift < 4 || score > 26.0 || shift >= ceiling || !(convincing || consistent) {
            break;
        }
        last_shift = Some(shift);

        // Only once we know the content really moved can static rows be
        // read as fixed chrome rather than "nothing happened yet".
        if chrome.is_none() {
            let (mut t, mut b) = static_edges(&prev, &next);
            // A quarter of the frame "static" means detection is confused.
            if t >= fh / 4 {
                t = 0;
            }
            if b >= fh / 4 {
                b = 0;
            }
            if t > 0 || b > 0 {
                eprintln!("scroll: chrome — {t}px top, {b}px bottom");
            }
            chrome = Some((t, b));
            // Drop the footer from the frame already on the canvas.
            if b > 0 {
                canvas = image::imageops::crop_imm(&canvas, 0, 0, fw, fh - b).to_image();
            }
        }
        let (view_top, view_bottom) = chrome.map(|(t, b)| (t, fh - b)).unwrap();

        // Append only the newly revealed rows from inside the viewport.
        let tail_bottom = view_bottom;
        let tail_top = tail_bottom.saturating_sub(shift).max(view_top);
        let tail_h = tail_bottom - tail_top;
        if tail_h == 0 {
            break;
        }
        let mut grown = RgbaImage::new(fw, canvas.height() + tail_h);
        image::imageops::replace(&mut grown, &canvas, 0, 0);
        let tail = image::imageops::crop_imm(&next, 0, tail_top, fw, tail_h).to_image();
        let at = canvas.height() as i64;
        image::imageops::replace(&mut grown, &tail, 0, at);
        canvas = grown;
        prev = next;
        steps += 1;

        pill_state.text = format!("Scrolling capture\u{2026}  {} px", canvas.height())
            .encode_utf16()
            .collect();
        unsafe {
            let _ = InvalidateRect(pill, None, false);
        }
        pump();
    }

    // Put the footer back once, at the very bottom.
    if let Some((_, bottom_chrome)) = chrome {
        if bottom_chrome > 0 && steps > 0 {
            let footer =
                image::imageops::crop_imm(&prev, 0, fh - bottom_chrome, fw, bottom_chrome)
                    .to_image();
            let mut grown = RgbaImage::new(fw, canvas.height() + bottom_chrome);
            image::imageops::replace(&mut grown, &canvas, 0, 0);
            let at = canvas.height() as i64;
            image::imageops::replace(&mut grown, &footer, 0, at);
            canvas = grown;
        }
    }

    unsafe {
        let _ = DestroyWindow(pill);
        let _ = SetCursorPos(saved.x, saved.y);
        let _ = DeleteObject(pill_state.font);
    }
    pump();

    if aborted && canvas.height() <= fh {
        bail!("cancelled");
    }
    eprintln!(
        "scroll capture: {} steps, {}x{}",
        steps,
        canvas.width(),
        canvas.height()
    );
    Ok(canvas)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_anchor_stays_at_the_users_point() {
        let rect = RECT { left: 100, top: 200, right: 500, bottom: 700 };
        assert_eq!(clamp_anchor(POINT { x: 240, y: 360 }, rect), POINT { x: 240, y: 360 });
    }

    #[test]
    fn scroll_anchor_is_kept_inside_the_capture_target() {
        let rect = RECT { left: 100, top: 200, right: 500, bottom: 700 };
        assert_eq!(clamp_anchor(POINT { x: 900, y: 900 }, rect), POINT { x: 499, y: 699 });
        assert_eq!(clamp_anchor(POINT { x: 20, y: 40 }, rect), POINT { x: 100, y: 200 });
    }
}
