//! Scrolling capture: repeatedly grab the target, send a wheel scroll, and
//! stitch the frames into one tall image by measuring how far the content
//! actually moved (never trusting the scroll amount we asked for).

use anyhow::{bail, Context, Result};
use image::{Rgba, RgbaImage};
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
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClassNameW, GetCursorPos,
    GetWindowLongPtrW, PeekMessageW, RegisterClassW, SetCursorPos, SetWindowLongPtrW,
    CREATESTRUCTW, GWLP_USERDATA, MSG, PM_REMOVE, WM_ERASEBKGND, WM_NCCREATE, WM_PAINT,
    WNDCLASSW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SafetyLimit {
    Steps,
    Height,
}

fn safety_limit(steps: usize, height: u32) -> Option<SafetyLimit> {
    if steps >= MAX_STEPS {
        Some(SafetyLimit::Steps)
    } else if height >= MAX_HEIGHT {
        Some(SafetyLimit::Height)
    } else {
        None
    }
}
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
    let hinstance = GetModuleHandleW(None)?;
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
    let hwnd = match CreateWindowExW(
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
    ) {
        Ok(hwnd) => hwnd,
        Err(error) => {
            let _ = DeleteObject(pill.font);
            return Err(error.into());
        }
    };
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

fn edge_anchor(rect: RECT, current: POINT) -> POINT {
    let width = (rect.right - rect.left).max(1);
    let inset = (width / 32).clamp(24, 64);
    let right = POINT { x: rect.right - inset, y: current.y };
    let left = POINT { x: rect.left + inset, y: current.y };
    let candidate = if (current.x - right.x).abs() > (current.x - left.x).abs() {
        right
    } else {
        left
    };
    clamp_anchor(candidate, rect)
}

fn recovery_anchor(target: Target, current: POINT) -> Option<POINT> {
    let Target::Window(hwnd, _) = target else {
        return None;
    };
    let mut rect = RECT::default();
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut rect);
    }
    Some(edge_anchor(rect, current))
}

fn scrollbar_width_for_class(class: &str) -> u32 {
    match class {
        "Chrome_WidgetWin_1" | "MozillaWindowClass" => 16,
        _ => 0,
    }
}

fn transient_scrollbar_width(target: Target) -> u32 {
    let Target::Window(hwnd, _) = target else {
        return 0;
    };
    let mut class = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, &mut class) } as usize;
    // Browser scrollbars describe the temporary viewport, not the final
    // stitched document. Keeping them repeats the moving thumb at every seam,
    // so remove the narrow strip from full-page output.
    scrollbar_width_for_class(&String::from_utf16_lossy(&class[..len]))
}

fn scrollbar_track(
    frame: &RgbaImage,
    width: u32,
    view_top: u32,
    view_bottom: u32,
) -> Vec<Rgba<u8>> {
    let width = width.min(frame.width());
    let top = view_top.min(frame.height().saturating_sub(1));
    let bottom = view_bottom.clamp(top + 1, frame.height());
    (frame.width() - width..frame.width())
        .map(|x| {
            let mut colors = std::collections::HashMap::<u32, u32>::new();
            for y in top..bottom {
                let p = frame.get_pixel(x, y);
                let key = u32::from_be_bytes([p[0], p[1], p[2], p[3]]);
                *colors.entry(key).or_default() += 1;
            }
            let key = colors
                .into_iter()
                .max_by_key(|(_, count)| *count)
                .map(|(color, _)| color)
                .unwrap_or_default();
            Rgba(key.to_be_bytes())
        })
        .collect()
}

fn paint_scrollbar_track(image: &mut RgbaImage, track: &[Rgba<u8>]) {
    let width = track.len().min(image.width() as usize);
    let start = image.width() - width as u32;
    for y in 0..image.height() {
        for (i, color) in track.iter().take(width).enumerate() {
            image.put_pixel(start + i as u32, y, *color);
        }
    }
}

fn paint_final_scrollbar(
    canvas: &mut RgbaImage,
    frame: &RgbaImage,
    width: u32,
    view_top: u32,
) {
    let width = width.min(canvas.width()).min(frame.width());
    let available = frame.height().saturating_sub(view_top);
    let height = available.min(canvas.height());
    if width == 0 || height == 0 {
        return;
    }
    let canvas_x = canvas.width() - width;
    let frame_x = frame.width() - width;
    let canvas_y = canvas.height() - height;
    let frame_y = frame.height() - height;
    for y in 0..height {
        for x in 0..width {
            canvas.put_pixel(canvas_x + x, canvas_y + y, *frame.get_pixel(frame_x + x, frame_y + y));
        }
    }
}

fn send_wheel_at(point: POINT) {
    unsafe {
        let _ = SetCursorPos(point.x, point.y);
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
}

fn frame_after_wheel(
    target: Target,
    point: POINT,
    view_top: u32,
    view_bottom: u32,
) -> Result<RgbaImage> {
    send_wheel_at(point);
    std::thread::sleep(std::time::Duration::from_millis(110));
    pump();
    let mut next = grab(target).context("capture scrolled frame")?;
    let settle_by = std::time::Instant::now() + std::time::Duration::from_millis(700);
    loop {
        std::thread::sleep(std::time::Duration::from_millis(70));
        pump();
        let again = grab(target).context("capture settling frame")?;
        let moving = viewport_diff(&next, &again, view_top, view_bottom);
        next = again;
        if moving < 1.0 || std::time::Instant::now() > settle_by {
            break;
        }
    }
    Ok(next)
}

struct CaptureCleanup {
    saved: POINT,
    pill: Option<(HWND, HFONT)>,
}

impl Drop for CaptureCleanup {
    fn drop(&mut self) {
        unsafe {
            if let Some((pill, font)) = self.pill.take() {
                let _ = DestroyWindow(pill);
                let _ = DeleteObject(font);
            }
            let _ = SetCursorPos(self.saved.x, self.saved.y);
        }
    }
}

#[derive(Clone, Copy)]
struct Motion {
    idle: f32,
    shift: u32,
    score: f32,
    ceiling: u32,
}

fn motion(
    prev: &RgbaImage,
    next: &RgbaImage,
    frame_height: u32,
    view_top: u32,
    view_bottom: u32,
) -> Motion {
    let idle = viewport_diff(prev, next, view_top, view_bottom);
    let (shift, score, ceiling) =
        measure_shift(prev, next, frame_height / 2, view_top, view_bottom);
    Motion { idle, shift, score, ceiling }
}

fn stitchable(m: Motion, last_shift: Option<u32>) -> bool {
    let convincing = m.score < m.idle * 0.5;
    let consistent = last_shift
        .map(|p| m.score < 12.0 && m.shift.abs_diff(p) * 8 <= p)
        .unwrap_or(false);
    m.idle >= 2.5
        && m.shift >= 4
        && m.score <= 26.0
        && m.shift < m.ceiling
        && (convincing || consistent)
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
    if let Target::Region(rect, monitor, _) = target {
        if !crate::window::monitor_contains_rect(monitor, rect) {
            bail!(
                "Scrolling capture regions must stay on one monitor. Select the scrollable area on a single display."
            );
        }
    }
    let anchor = match target {
        Target::Window(h, _) => unsafe { MonitorFromWindow(h, MONITOR_DEFAULTTOPRIMARY) },
        Target::Region(_, m, _) => m,
    };

    // Put the cursor exactly where the user chose so wheel routing follows
    // their intent: page body scrolls the page; a nested pane scrolls itself.
    let mut hover = match target {
        Target::Window(h, point) => {
            let mut r = RECT::default();
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(h, &mut r);
            }
            clamp_anchor(point, r)
        }
        Target::Region(r, _, point) => clamp_anchor(point, r),
    };
    let chosen_anchor = hover;
    let mut saved = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut saved);
        let _ = SetCursorPos(hover.x, hover.y);
        // Let the target process the move before the first wheel event.
        std::thread::sleep(std::time::Duration::from_millis(120));
    }
    let mut cleanup = CaptureCleanup { saved, pill: None };
    eprintln!("scroll: wheel anchor {},{}", hover.x, hover.y);

    let (pill, mut pill_state) = unsafe { show_pill(anchor)? };
    cleanup.pill = Some((pill, pill_state.font));
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
    let mut last_observed = first.clone();
    let mut steps = 0usize;
    let mut failure: Option<anyhow::Error> = None;
    // Learned on the first successful step, then held steady.
    let mut chrome: Option<(u32, u32)> = None;
    let scrollbar_width = transient_scrollbar_width(target);
    let mut scrollbar_track_colors: Option<Vec<Rgba<u8>>> = None;
    // Apps scroll a consistent amount per notch; that's a strong prior.
    let mut last_shift: Option<u32> = None;

    while steps < MAX_STEPS && canvas.height() < MAX_HEIGHT {
        if esc_pressed() {
            failure = Some(anyhow::anyhow!("scrolling capture cancelled"));
            break;
        }
        // The viewport we know so far (whole frame until chrome is learned).
        let (vt, vb) = chrome.map(|(t, b)| (t, fh - b)).unwrap_or((0, fh));

        let mut next = match frame_after_wheel(target, hover, vt, vb) {
            Ok(frame) => frame,
            Err(error) => {
                failure = Some(error);
                break;
            }
        };
        if next.dimensions() != (fw, fh) {
            failure = Some(anyhow::anyhow!(
                "the scrolling target changed size during capture"
            ));
            break;
        }

        // Did anything move at all? This is the honest bottom-of-page test —
        // and it also catches apps that ignore the wheel entirely.
        let mut movement = motion(&prev, &next, fh, vt, vb);
        if movement.idle < 2.5 {
            std::thread::sleep(std::time::Duration::from_millis(280));
            pump();
            match grab(target) {
                Ok(retry) => {
                    next = retry;
                    movement = motion(&prev, &next, fh, vt, vb);
                }
                Err(error) => {
                    failure = Some(error.context("recapture scrolling target"));
                    break;
                }
            }
        }
        if failure.is_some() {
            break;
        }
        last_observed = next.clone();

        if std::env::var("MATTESHOT_SCROLL_DEBUG").is_ok() {
            eprintln!(
                "step {steps}: shift={} score={:.2} idle={:.2} view={vt}..{vb}",
                movement.shift, movement.score, movement.idle
            );
        }

        // A fixed screen point can eventually be covered by a nested scroller
        // as the main page moves. Once global scrolling is established, a
        // localized/idle step is retried near the far window edge, where the
        // browser routes the wheel back to the main document.
        if !stitchable(movement, last_shift) {
            let Some(fallback) = last_shift
                .and_then(|_| {
                    if hover != chosen_anchor {
                        Some(chosen_anchor)
                    } else {
                        recovery_anchor(target, hover)
                    }
                })
                .filter(|p| *p != hover)
            else {
                if movement.idle >= 2.5 {
                    failure = Some(anyhow::anyhow!(
                        "content moved but Matteshot could not stitch it reliably"
                    ));
                }
                break;
            };
            let recovery_base = next;
            eprintln!(
                "scroll: anchor blocked; retrying at {},{}",
                fallback.x, fallback.y
            );
            let retry = match frame_after_wheel(target, fallback, vt, vb) {
                Ok(frame) => frame,
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            };
            last_observed = retry.clone();
            let recovered = motion(&recovery_base, &retry, fh, vt, vb);
            if !stitchable(recovered, last_shift) {
                if recovered.idle >= 2.5 {
                    failure = Some(anyhow::anyhow!(
                        "content moved but Matteshot could not stitch it reliably"
                    ));
                }
                break;
            }
            hover = fallback;
            next = retry;
            movement = recovered;
            eprintln!("scroll: main page recovered");
        }
        let shift = movement.shift;
        let established_main_page = last_shift.is_none();
        last_shift = Some(shift);
        if established_main_page {
            // Once the first full-frame shift proves that the user intended
            // the main document, move to a stable edge before embedded panes
            // can slide under the original point. Region captures keep the
            // exact chosen point because their crop defines the scroll pane.
            if let Some(edge) = recovery_anchor(target, hover).filter(|p| *p != hover) {
                hover = edge;
                eprintln!("scroll: main page anchored at {},{}", hover.x, hover.y);
            }
        }

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
            if scrollbar_width > 0 {
                scrollbar_track_colors =
                    Some(scrollbar_track(&first, scrollbar_width, t, fh - b));
            }
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
            failure = Some(anyhow::anyhow!("scrolling capture found no new rows to append"));
            break;
        }
        let mut grown = RgbaImage::new(fw, canvas.height() + tail_h);
        image::imageops::replace(&mut grown, &canvas, 0, 0);
        let mut tail = image::imageops::crop_imm(&next, 0, tail_top, fw, tail_h).to_image();
        if let Some(track) = &scrollbar_track_colors {
            paint_scrollbar_track(&mut tail, track);
        }
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
            let mut footer =
                image::imageops::crop_imm(&prev, 0, fh - bottom_chrome, fw, bottom_chrome)
                    .to_image();
            if let Some(track) = &scrollbar_track_colors {
                paint_scrollbar_track(&mut footer, track);
            }
            let mut grown = RgbaImage::new(fw, canvas.height() + bottom_chrome);
            image::imageops::replace(&mut grown, &canvas, 0, 0);
            let at = canvas.height() as i64;
            image::imageops::replace(&mut grown, &footer, 0, at);
            canvas = grown;
        }
    }

    if failure.is_none() {
        failure = match safety_limit(steps, canvas.height()) {
            Some(SafetyLimit::Steps) => Some(anyhow::anyhow!(
                "scrolling capture reached its {MAX_STEPS}-step safety limit before the page ended"
            )),
            Some(SafetyLimit::Height) => Some(anyhow::anyhow!(
                "scrolling capture reached its {MAX_HEIGHT}-pixel safety limit before the page ended"
            )),
            None => None,
        };
    }

    if scrollbar_track_colors.is_some() {
        let view_top = chrome.map(|(top, _)| top).unwrap_or(0);
        paint_final_scrollbar(&mut canvas, &last_observed, scrollbar_width, view_top);
        eprintln!("scroll: kept browser scrollbar at start and finish");
    }

    drop(cleanup);
    pump();

    if let Some(error) = failure {
        return Err(error);
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
    fn incomplete_safety_limited_captures_are_detected() {
        assert_eq!(safety_limit(MAX_STEPS, 1000), Some(SafetyLimit::Steps));
        assert_eq!(safety_limit(2, MAX_HEIGHT), Some(SafetyLimit::Height));
        assert_eq!(safety_limit(2, 1000), None);
    }

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

    #[test]
    fn recovery_moves_to_the_far_window_edge() {
        let rect = RECT { left: 0, top: 0, right: 1920, bottom: 1080 };
        assert_eq!(edge_anchor(rect, POINT { x: 600, y: 700 }), POINT { x: 1860, y: 700 });
        assert_eq!(edge_anchor(rect, POINT { x: 1500, y: 700 }), POINT { x: 60, y: 700 });
    }

    #[test]
    fn browser_scrollbars_are_recognized_as_side_chrome() {
        assert_eq!(scrollbar_width_for_class("Chrome_WidgetWin_1"), 16);
        assert_eq!(scrollbar_width_for_class("MozillaWindowClass"), 16);
        assert_eq!(scrollbar_width_for_class("Notepad"), 0);
    }

    #[test]
    fn repeated_scrollbar_thumbs_are_replaced_without_cropping() {
        let mut first = RgbaImage::from_pixel(8, 8, Rgba([10, 10, 10, 255]));
        for y in 0..8 {
            for x in 6..8 {
                first.put_pixel(x, y, Rgba([30, 30, 30, 255]));
            }
        }
        for y in 2..4 {
            for x in 6..8 {
                first.put_pixel(x, y, Rgba([220, 220, 220, 255]));
            }
        }
        let track = scrollbar_track(&first, 2, 0, 8);
        assert_eq!(track, vec![Rgba([30, 30, 30, 255]); 2]);

        let mut tail = RgbaImage::from_pixel(8, 4, Rgba([80, 80, 80, 255]));
        paint_scrollbar_track(&mut tail, &track);
        assert_eq!(tail.width(), 8);
        assert_eq!(*tail.get_pixel(5, 2), Rgba([80, 80, 80, 255]));
        assert_eq!(*tail.get_pixel(6, 2), Rgba([30, 30, 30, 255]));
        assert_eq!(*tail.get_pixel(7, 2), Rgba([30, 30, 30, 255]));
    }

    #[test]
    fn final_scrollbar_is_restored_at_the_bottom() {
        let mut canvas = RgbaImage::from_pixel(8, 16, Rgba([10, 10, 10, 255]));
        let mut final_frame = RgbaImage::from_pixel(8, 8, Rgba([20, 20, 20, 255]));
        for y in 0..8 {
            final_frame.put_pixel(7, y, Rgba([100 + y as u8, 0, 0, 255]));
        }
        paint_final_scrollbar(&mut canvas, &final_frame, 1, 2);

        assert_eq!(canvas.width(), 8);
        assert_eq!(*canvas.get_pixel(7, 9), Rgba([10, 10, 10, 255]));
        for y in 0..6 {
            assert_eq!(*canvas.get_pixel(7, 10 + y), Rgba([102 + y as u8, 0, 0, 255]));
        }
    }
}
