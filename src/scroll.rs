//! Scrolling capture: repeatedly grab the target, send a wheel scroll, and
//! stitch the frames into one tall image by measuring how far the content
//! actually moved (never trusting the scroll amount we asked for).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use image::{Rgba, RgbaImage};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect,
    GetMonitorInfoW, InvalidateRect, MonitorFromWindow, ScreenToClient, SetBkMode, SelectObject,
    SetTextColor, CLEARTYPE_QUALITY, DEFAULT_CHARSET, DT_CENTER, DT_SINGLELINE, DT_VCENTER,
    FF_DONTCARE, HFONT, HMONITOR, MONITORINFO, MONITOR_DEFAULTTOPRIMARY, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, RegisterHotKey, SendInput, UnregisterHotKey, INPUT, INPUT_MOUSE, MOD_CONTROL,
    MOD_NOREPEAT, MOD_SHIFT, MOUSEEVENTF_WHEEL, MOUSEINPUT, VK_ESCAPE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClassNameW, GetCursorPos,
    GetWindowLongPtrW, PeekMessageW, RegisterClassW, SetCursorPos, SetWindowLongPtrW,
    CREATESTRUCTW, GWLP_USERDATA, MSG, PM_REMOVE, WM_ERASEBKGND, WM_HOTKEY, WM_LBUTTONDOWN,
    WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_NCCREATE, WM_PAINT, WNDCLASSW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
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

/// Why the stitching loop ended.
///
/// Ending early used to mean one thing — an error — so the only way out that
/// kept an image was reaching the real bottom of the page. A stop the user
/// asked for is a legitimate ending with a usable result, and telling the two
/// apart is what this exists for.
enum Outcome {
    /// The content stopped moving: a complete capture.
    Bottom,
    /// The user asked to stop. What has been stitched so far *is* the capture.
    Stopped,
    /// Ran into a runaway guard before the page ended.
    Limit(SafetyLimit),
    Failed(anyhow::Error),
}

impl Outcome {
    /// The error this ending reports, or `None` when it hands back the canvas.
    ///
    /// Only a real bottom-of-page and a stop the user asked for keep the
    /// image. Everything else has to fail rather than pass off a partial
    /// capture as the whole page.
    fn into_error(self) -> Option<anyhow::Error> {
        match self {
            Outcome::Bottom | Outcome::Stopped => None,
            Outcome::Limit(SafetyLimit::Steps) => Some(anyhow::anyhow!(
                "scrolling capture reached its {MAX_STEPS}-step safety limit before the page ended"
            )),
            Outcome::Limit(SafetyLimit::Height) => Some(anyhow::anyhow!(
                "scrolling capture reached its {MAX_HEIGHT}-pixel safety limit before the page ended"
            )),
            Outcome::Failed(error) => Some(error),
        }
    }
}

/// Raised by the pill's Stop button and by its hotkey, read by the loop.
///
/// Shared rather than reached through the window pointer so that the loop and
/// the window procedure never hold overlapping borrows of the same state.
type StopSignal = Arc<AtomicBool>;

/// The pill's own hotkey id. `RegisterHotKey` ids are per-window, so this
/// cannot collide with the recorder's.
const STOP_HOTKEY: i32 = 1;

/// Pill metrics at 96 DPI, scaled to whichever monitor it opens on.
const PILL_W: i32 = 300;
const PILL_H: i32 = 58;
const PILL_PAD: i32 = 16;
/// Stop button box at 96 DPI, in the pill's client coordinates.
const STOP_L: i32 = 202;
const STOP_T: i32 = 8;
const STOP_R: i32 = 284;
const STOP_B: i32 = 34;

/// Where to put the pill so that it cannot swallow the wheel.
///
/// The pill is a real window sitting in the cursor's path: `send_wheel_at`
/// drives the pointer to the scroll anchor and synthesizes a wheel event
/// there, and whatever window is under that point receives it. If that window
/// is the pill, the target never scrolls — and because nothing moved, the
/// capture reads it as a clean bottom-of-page and returns a single frame.
///
/// Window captures walk their anchor out to a window edge after the first
/// step, but `recovery_anchor` declines to move a region's, so a region chosen
/// near the top centre of the screen would sit under the pill for the whole
/// run.
///
/// Clearing the anchor's *row* rather than the anchor point is deliberate, and
/// it is what makes this hold for the whole capture rather than only the first
/// step. The anchor moves horizontally — `edge_anchor` rebuilds it as
/// `{ x: <window edge>, y: current.y }` — so its y never changes once chosen.
/// Avoiding that row therefore avoids every position the anchor can ever take,
/// without having to predict which edge it will walk to. Testing the initial
/// point alone would leave a narrow window centred on screen able to walk its
/// anchor under the pill on a later step.
fn pill_origin(work: RECT, w: i32, h: i32, gap: i32, anchor: POINT) -> POINT {
    let cx = work.left + (work.right - work.left - w) / 2;
    let candidates = [
        POINT { x: cx, y: work.top + gap },
        POINT { x: cx, y: work.bottom - gap - h },
        POINT { x: work.left + gap, y: work.top + gap },
        POINT { x: work.right - gap - w, y: work.top + gap },
    ];
    let home = candidates[0];
    candidates
        .into_iter()
        .find(|p| anchor.y < p.y || anchor.y >= p.y + h)
        .unwrap_or(home)
}

/// The pill's geometry at one scale.
struct PillLayout {
    w: i32,
    h: i32,
    stop_rect: RECT,
    pad: i32,
}

/// Lay the pill out for a monitor's scale.
///
/// Separate from the window so it can be checked: the Stop button is the first
/// interactive control in this window, and a hit rect that scales differently
/// from what is painted is a button that misses.
fn pill_layout(scale: f32) -> PillLayout {
    let sc = |v: i32| (v as f32 * scale) as i32;
    PillLayout {
        w: sc(PILL_W),
        h: sc(PILL_H),
        stop_rect: RECT {
            left: sc(STOP_L),
            top: sc(STOP_T),
            right: sc(STOP_R),
            bottom: sc(STOP_B),
        },
        pad: sc(PILL_PAD),
    }
}

struct Pill {
    text: Vec<u16>,
    theme: crate::theme::Theme,
    font: HFONT,
    font_small: HFONT,
    stop: StopSignal,
    stop_rect: RECT,
    /// Named separately from the button because it has to tell the truth about
    /// whether the chord actually registered.
    hint: Vec<u16>,
    pad: i32,
    w: i32,
    h: i32,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn in_rect(r: &RECT, p: POINT) -> bool {
    p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
}

/// Whether the pointer is over Stop, read live rather than tracked.
///
/// The capture drives the cursor itself, teleporting it back to the scroll
/// anchor several times a second, so a hover flag maintained from mouse
/// messages would get stuck on: the pointer leaves without the pill ever
/// seeing it go.
unsafe fn hovering_stop(hwnd: HWND, pill: &Pill) -> bool {
    let mut pt = POINT::default();
    if GetCursorPos(&mut pt).is_err() || !ScreenToClient(hwnd, &mut pt).as_bool() {
        return false;
    }
    in_rect(&pill.stop_rect, pt)
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
        // The pill must never take focus from the window being scrolled:
        // activation can change what that window draws (focus rings, sticky
        // headers) halfway through a capture.
        WM_MOUSEACTIVATE => LRESULT(3), // MA_NOACTIVATE
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
                let mut r = RECT {
                    left: p.pad,
                    top: p.stop_rect.top,
                    right: p.stop_rect.left - p.pad,
                    bottom: p.stop_rect.bottom,
                };
                DrawTextW(hdc, &mut t, &mut r, DT_SINGLELINE | DT_VCENTER);

                let hot = hovering_stop(hwnd, p);
                let fill = CreateSolidBrush(if hot { p.theme.accent } else { p.theme.chip });
                FillRect(hdc, &p.stop_rect, fill);
                let _ = DeleteObject(fill);
                SetTextColor(hdc, if hot { p.theme.accent_text } else { p.theme.text });
                let mut stop = wide("Stop");
                let mut sr = p.stop_rect;
                DrawTextW(hdc, &mut stop, &mut sr, DT_CENTER | DT_SINGLELINE | DT_VCENTER);

                SelectObject(hdc, p.font_small);
                SetTextColor(hdc, p.theme.faint);
                let mut hint = p.hint.clone();
                let mut hr =
                    RECT { left: 0, top: p.stop_rect.bottom, right: p.w, bottom: p.h };
                DrawTextW(hdc, &mut hint, &mut hr, DT_CENTER | DT_SINGLELINE | DT_VCENTER);

                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        // Repaint so the Stop button lights up under the pointer; the hover
        // test itself happens in WM_PAINT.
        WM_MOUSEMOVE => {
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        // Down, not up: the capture teleports the cursor back to the scroll
        // anchor between wheel events, so a press and its release frequently
        // do not land on the same window and the up never arrives.
        WM_LBUTTONDOWN => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Pill;
            if let Some(p) = ptr.as_mut() {
                let pt = POINT {
                    x: (lparam.0 & 0xFFFF) as i16 as i32,
                    y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                };
                if in_rect(&p.stop_rect, pt) {
                    p.stop.store(true, Ordering::Relaxed);
                }
            }
            LRESULT(0)
        }
        WM_HOTKEY => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Pill;
            if let Some(p) = ptr.as_mut() {
                p.stop.store(true, Ordering::Relaxed);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn make_font(height: i32, weight: i32) -> HFONT {
    CreateFontW(
        height,
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        DEFAULT_CHARSET.0 as u32,
        0,
        0,
        CLEARTYPE_QUALITY.0 as u32,
        FF_DONTCARE.0 as u32,
        w!("Segoe UI"),
    )
}

unsafe fn show_pill(
    monitor: HMONITOR,
    wheel_anchor: POINT,
    stop: StopSignal,
) -> Result<(HWND, Box<Pill>)> {
    let theme = crate::theme::current();
    // The pill opens on the monitor it is anchored to, so that monitor's scale
    // is the one its metrics are measured in — including the Stop button's hit
    // rect, which is no use if it is drawn at a third of its intended size.
    let scale = crate::dpi::scale_for_monitor(monitor);
    let sc = |v: i32| (v as f32 * scale) as i32;
    let layout = pill_layout(scale);
    let (w, h) = (layout.w, layout.h);
    let hinstance = GetModuleHandleW(None)?;
    let mut pill = Box::new(Pill {
        text: wide("Scrolling\u{2026}"),
        theme,
        font: make_font(-sc(15), 600),
        font_small: make_font(-sc(12), 400),
        stop,
        stop_rect: layout.stop_rect,
        hint: wide("Esc or Ctrl+Shift+S to stop"),
        pad: layout.pad,
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
    let _ = GetMonitorInfoW(monitor, &mut mi);
    let POINT { x, y } = pill_origin(mi.rcWork, w, h, sc(40), wheel_anchor);
    let hwnd = match CreateWindowExW(
        // NOACTIVATE so that clicking Stop does not pull focus off the window
        // being captured.
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
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
            let _ = DeleteObject(pill.font_small);
            return Err(error.into());
        }
    };
    // Keep our own progress out of the captured frames.
    let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowDisplayAffinity(
        hwnd,
        windows::Win32::UI::WindowsAndMessaging::WDA_EXCLUDEFROMCAPTURE,
    );
    // A chord, and registered rather than polled, for one reason: RegisterHotKey
    // swallows the keystroke. A bare key would reach the page being captured,
    // and the obvious candidates are the worst offenders — Space is page-down
    // in every browser, which would scroll the target out from under the very
    // step that is measuring it.
    //
    // A chord can already be owned by another process, in which case the hint
    // must stop naming it: a control that is advertised and does nothing is
    // worse than one that was never offered. Esc and the button are unaffected,
    // so this degrades rather than fails.
    if RegisterHotKey(
        hwnd,
        STOP_HOTKEY,
        MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT,
        0x53, // S
    )
    .is_err()
    {
        eprintln!("scroll: Ctrl+Shift+S is already taken; Esc and Stop still work");
        pill.hint = wide("Esc or the Stop button to stop");
    }
    Ok((hwnd, pill))
}

/// Drain the queue so the pill repaints and its Stop button and hotkey land.
///
/// The resident's capture hotkeys are dropped here on purpose. They are posted
/// to the *thread* — the PrtScn hook via `PostThreadMessageW`, and both
/// `RegisterHotKey` calls with a null window — so they arrive with a null
/// `hwnd`, while the pill's own hotkey is posted to its window and must still
/// be dispatched. Starting a second capture inside a running one would open a
/// nested overlay and fight this loop for the cursor, and nothing else guards
/// against it: `state_lock` enforces a single resident *process*, not a single
/// capture.
///
/// `DispatchMessageW` ignores null-`hwnd` messages anyway, so this check
/// changes no behaviour today. It is written out because the obvious way to
/// make a new hotkey work during a capture is to handle `WM_HOTKEY` inline
/// here, exactly as the main loop does — and doing that without noticing this
/// distinction would quietly turn PrtScn into "open an overlay mid-scroll".
fn pump() {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_HOTKEY && msg.hwnd.0.is_null() {
                continue;
            }
            DispatchMessageW(&msg);
        }
    }
}

/// Whether Esc has been pressed, including a tap that started and finished
/// between two polls.
///
/// The high bit alone samples a level — "is it down at this instant" — and the
/// gaps between polls here are 70ms to 280ms, which is the length of an
/// ordinary key tap. Sampling would drop those, and a stop key that needs to
/// be held to work teaches people to hold it, which is exactly what
/// `wait_for_esc_release` then has to clean up after. The low bit latches
/// "was pressed since the previous call", turning the poll into an edge test.
///
/// Relying on that latch is safe here because it is per-process and nothing
/// else polls `VK_ESCAPE` while a capture runs: `delay.rs` is the only other
/// caller, and a countdown and a scrolling capture never overlap.
fn esc_pressed() -> bool {
    unsafe { GetAsyncKeyState(VK_ESCAPE.0 as i32) as u16 & 0x8001 != 0 }
}

/// Clear a stale "pressed since last call" latch before the loop reads it.
///
/// The latch accumulates from whenever it was last read, so an Esc pressed
/// before the capture began would otherwise stop it on the first poll.
fn clear_esc_latch() {
    let _ = esc_pressed();
}

/// Wait for Esc to come back up before handing control to the picker.
///
/// The stop is detected while the key is still down, and the picker cancels on
/// `WM_KEYDOWN`, which auto-repeat keeps delivering for as long as the key is
/// held. Returning with Esc still down would let it close the picker the
/// instant it opens and destroy the very capture the stop was meant to save.
///
/// Bounded: a key reported as stuck must not hang the capture behind it.
fn wait_for_esc_release() {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    // The level bit only — the edge latch would re-trigger on the press that
    // stopped us and never clear.
    while unsafe { GetAsyncKeyState(VK_ESCAPE.0 as i32) } as u16 & 0x8000 != 0 {
        if std::time::Instant::now() > deadline {
            eprintln!("scroll: Esc still down after 3s; continuing anyway");
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        pump();
    }
}

/// Whether the user has asked the capture to end.
///
/// Esc counts, and it keeps the image rather than discarding it. Getting into
/// a scrolling capture takes three deliberate steps, after which the user is a
/// passenger watching the page go by, so the overwhelmingly common thing they
/// want mid-capture is "that is far enough" — and Esc is the key they will
/// reach for to say it. Pointing the most reachable key at "throw away the
/// last minute of scrolling" got that exactly backwards. Abandoning a capture
/// outright is now the picker's job: Esc there closes it, so the way out is
/// the same key twice.
fn stop_requested(stop: &StopSignal) -> bool {
    esc_pressed() || stop.load(Ordering::Relaxed)
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

/// What one wheel step produced.
enum Frame {
    /// A settled frame, ready to stitch.
    Ready(RgbaImage),
    /// The user stopped the capture partway through this step. The frame in
    /// flight is mid-scroll and is thrown away: the canvas ends on the last
    /// step that completed, rather than on a torn one.
    Stopped,
}

fn frame_after_wheel(
    target: Target,
    point: POINT,
    view_top: u32,
    view_bottom: u32,
    stop: &StopSignal,
) -> Result<Frame> {
    send_wheel_at(point);
    std::thread::sleep(std::time::Duration::from_millis(110));
    pump();
    if stop_requested(stop) {
        return Ok(Frame::Stopped);
    }
    let mut next = grab(target).context("capture scrolled frame")?;
    let settle_by = std::time::Instant::now() + std::time::Duration::from_millis(700);
    loop {
        std::thread::sleep(std::time::Duration::from_millis(70));
        pump();
        // Checked here and not only once per step: a step can run the better
        // part of a second, and a Stop that takes that long to register reads
        // as a button that did nothing.
        if stop_requested(stop) {
            return Ok(Frame::Stopped);
        }
        let again = grab(target).context("capture settling frame")?;
        let moving = viewport_diff(&next, &again, view_top, view_bottom);
        next = again;
        if moving < 1.0 || std::time::Instant::now() > settle_by {
            break;
        }
    }
    Ok(Frame::Ready(next))
}

/// Owns everything the capture has to hand back: the cursor position, and the
/// pill window together with the state its window procedure reads.
///
/// The `Box<Pill>` lives here rather than beside it in `capture` so the
/// ordering is structural. `GWLP_USERDATA` points into that box, so the window
/// must be destroyed before the box is freed; keeping them in separate
/// bindings makes that a fact about declaration order, which an unwind would
/// invert.
struct CaptureCleanup {
    saved: POINT,
    pill: Option<(HWND, Box<Pill>)>,
}

impl Drop for CaptureCleanup {
    fn drop(&mut self) {
        unsafe {
            if let Some((hwnd, pill)) = self.pill.take() {
                let _ = UnregisterHotKey(hwnd, STOP_HOTKEY);
                // Before the fonts, and before `pill` falls out of scope: the
                // window procedure can still run during destruction.
                let _ = DestroyWindow(hwnd);
                let _ = DeleteObject(pill.font);
                let _ = DeleteObject(pill.font_small);
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
    if top + bottom >= h / 2 {
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

    // Checked before the pill exists, so that the window can never outlive the
    // state its window procedure reads through `GWLP_USERDATA`: the pill's
    // teardown is `cleanup`'s, and `cleanup` is declared first, so it is
    // dropped last.
    let first = grab(target).context("first frame")?;
    let (fw, fh) = (first.width(), first.height());
    if fh < 80 {
        bail!("target too short to scroll-capture");
    }

    let stop: StopSignal = Arc::new(AtomicBool::new(false));
    let (pill, pill_state) = unsafe { show_pill(anchor, hover, Arc::clone(&stop))? };
    cleanup.pill = Some((pill, pill_state));
    // An Esc already held as the capture begins would read as "stop" on the
    // very first poll and hand back a single frame, because clearing the latch
    // cannot clear the key actually being down. Let it come up first.
    wait_for_esc_release();
    // Whatever the latch accumulated before now belongs to the overlay, not to
    // this capture.
    clear_esc_latch();
    pump();

    let mut canvas = first.clone();
    let mut prev = first.clone();
    let mut last_observed = first.clone();
    let mut steps = 0usize;
    let mut outcome = Outcome::Bottom;
    // Learned on the first successful step, then held steady.
    let mut chrome: Option<(u32, u32)> = None;
    let scrollbar_width = transient_scrollbar_width(target);
    let mut scrollbar_track_colors: Option<Vec<Rgba<u8>>> = None;
    // Apps scroll a consistent amount per notch; that's a strong prior.
    let mut last_shift: Option<u32> = None;

    while steps < MAX_STEPS && canvas.height() < MAX_HEIGHT {
        if stop_requested(&stop) {
            outcome = Outcome::Stopped;
            break;
        }
        // The viewport we know so far (whole frame until chrome is learned).
        let (vt, vb) = chrome.map(|(t, b)| (t, fh - b)).unwrap_or((0, fh));

        let mut next = match frame_after_wheel(target, hover, vt, vb, &stop) {
            Ok(Frame::Ready(frame)) => frame,
            Ok(Frame::Stopped) => {
                outcome = Outcome::Stopped;
                break;
            }
            Err(error) => {
                outcome = Outcome::Failed(error);
                break;
            }
        };
        if next.dimensions() != (fw, fh) {
            outcome = Outcome::Failed(anyhow::anyhow!(
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
            if stop_requested(&stop) {
                outcome = Outcome::Stopped;
                break;
            }
            match grab(target) {
                Ok(retry) => {
                    next = retry;
                    movement = motion(&prev, &next, fh, vt, vb);
                }
                Err(error) => {
                    outcome = Outcome::Failed(error.context("recapture scrolling target"));
                    break;
                }
            }
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
                    outcome = Outcome::Failed(anyhow::anyhow!(
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
            let retry = match frame_after_wheel(target, fallback, vt, vb, &stop) {
                Ok(Frame::Ready(frame)) => frame,
                Ok(Frame::Stopped) => {
                    outcome = Outcome::Stopped;
                    break;
                }
                Err(error) => {
                    outcome = Outcome::Failed(error);
                    break;
                }
            };
            last_observed = retry.clone();
            let recovered = motion(&recovery_base, &retry, fh, vt, vb);
            if !stitchable(recovered, last_shift) {
                if recovered.idle >= 2.5 {
                    outcome = Outcome::Failed(anyhow::anyhow!(
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
            outcome = Outcome::Failed(anyhow::anyhow!(
                "scrolling capture found no new rows to append"
            ));
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

        if let Some((_, pill_state)) = cleanup.pill.as_mut() {
            pill_state.text = wide(&format!("Scrolling\u{2026}  {} px", canvas.height()));
        }
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

    // Only a loop that ran to its own end can have run into a guard; any other
    // ending got there first and keeps its own reason.
    if matches!(outcome, Outcome::Bottom) {
        if let Some(limit) = safety_limit(steps, canvas.height()) {
            outcome = Outcome::Limit(limit);
        }
    }

    if scrollbar_track_colors.is_some() {
        let view_top = chrome.map(|(top, _)| top).unwrap_or(0);
        paint_final_scrollbar(&mut canvas, &last_observed, scrollbar_width, view_top);
        eprintln!("scroll: kept browser scrollbar at start and finish");
    }

    // With the pill still up, so the wait is visible rather than a freeze.
    wait_for_esc_release();

    drop(cleanup);
    pump();

    let stopped = matches!(outcome, Outcome::Stopped);
    if let Some(error) = outcome.into_error() {
        return Err(error);
    }
    eprintln!(
        "scroll capture: {} steps, {}x{}{}",
        steps,
        canvas.width(),
        canvas.height(),
        if stopped { " (stopped by the user)" } else { "" }
    );
    Ok(canvas)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_markers(width: u32, height: u32) -> RgbaImage {
        RgbaImage::from_fn(width, height, |x, y| {
            Rgba([
                (y * 17 + x * 3) as u8,
                (y * 29 + x * 5) as u8,
                (y * 43 + x * 7) as u8,
                255,
            ])
        })
    }

    fn shifted_up(previous: &RgbaImage, shift: u32) -> RgbaImage {
        RgbaImage::from_fn(previous.width(), previous.height(), |x, y| {
            if y + shift < previous.height() {
                *previous.get_pixel(x, y + shift)
            } else {
                Rgba([241, (x * 13 + y) as u8, 17, 255])
            }
        })
    }

    #[test]
    fn shift_measurement_finds_known_vertical_motion() {
        let previous = row_markers(96, 160);
        let next = shifted_up(&previous, 18);
        let measured = motion(&previous, &next, 160, 0, 160);

        assert_eq!(measured.shift, 18);
        assert!(measured.score < 0.01, "unexpected match score: {}", measured.score);
        assert!(measured.score < measured.idle * 0.5);
        assert!(stitchable(measured, None));
    }

    #[test]
    fn stitchability_rejects_idle_and_weak_periodic_matches() {
        let frame = row_markers(96, 160);
        assert!(!stitchable(motion(&frame, &frame, 160, 0, 160), None));

        let weak = Motion { idle: 20.0, shift: 16, score: 12.0, ceiling: 80 };
        assert!(!stitchable(weak, None));

        let consistent = Motion { score: 10.0, ..weak };
        assert!(stitchable(consistent, Some(16)));
    }

    #[test]
    fn static_edges_report_only_contiguous_unchanged_chrome() {
        let previous = row_markers(96, 120);
        let mut next = previous.clone();
        for y in 8..114 {
            for x in 0..next.width() {
                next.put_pixel(x, y, Rgba([250, (x + y) as u8, 3, 255]));
            }
        }

        assert_eq!(static_edges(&previous, &next), (8, 6));
        assert_eq!(static_edges(&previous, &previous), (0, 0));
    }

    #[test]
    fn only_a_deliberate_stop_or_a_real_bottom_hands_back_the_canvas() {
        // Stopping is the whole point of the pill: it ends the capture *with*
        // an image. Every ending the user did not ask for stays an error, so a
        // partial page is never returned as if it were a whole one.
        assert!(Outcome::Bottom.into_error().is_none());
        assert!(Outcome::Stopped.into_error().is_none());
        assert!(Outcome::Limit(SafetyLimit::Steps).into_error().is_some());
        assert!(Outcome::Limit(SafetyLimit::Height).into_error().is_some());
        assert!(Outcome::Failed(anyhow::anyhow!("boom")).into_error().is_some());
    }

    #[test]
    fn the_pill_never_parks_on_the_point_the_wheel_is_sent_to() {
        let work = RECT { left: 0, top: 0, right: 1920, bottom: 1040 };
        let (w, h, gap) = (300, 58, 40);
        let covers = |o: POINT, p: POINT| {
            in_rect(
                &RECT { left: o.x, top: o.y, right: o.x + w, bottom: o.y + h },
                p,
            )
        };
        // A region chosen near the top centre is exactly where the pill wants
        // to sit, and a region capture never moves its anchor off it.
        let under = POINT { x: 960, y: 60 };
        assert!(!covers(pill_origin(work, w, h, gap, under), under));
        // Everywhere else it stays where it has always been.
        assert_eq!(
            pill_origin(work, w, h, gap, POINT { x: 400, y: 700 }),
            POINT { x: (1920 - w) / 2, y: gap }
        );
        // The whole row is cleared, not just the point. The anchor only ever
        // moves horizontally, so a placement that dodges the initial x but
        // keeps the row would still be walked under on a later step.
        for x in [0, 400, 960, 1500, 1919] {
            let anchor = POINT { x, y: 60 };
            let origin = pill_origin(work, w, h, gap, anchor);
            assert!(
                !covers(origin, POINT { x: 960, y: 60 }),
                "row not cleared for an anchor at x={x}"
            );
        }
    }

    #[test]
    fn the_stop_button_stays_inside_the_pill_at_every_scale() {
        for scale in [1.0f32, 1.25, 1.5, 1.75, 2.0] {
            let l = pill_layout(scale);
            let r = l.stop_rect;
            assert!(r.left > l.pad && r.right < l.w, "at {scale}x");
            assert!(r.top > 0 && r.bottom < l.h, "at {scale}x");
            // The label sits between the padding and the button; if the button
            // ever slid left far enough to close that gap the progress text
            // would be drawn into a backwards rect and vanish.
            assert!(r.left - l.pad > l.pad, "no room for the label at {scale}x");
            // Small targets are the whole reason this is scaled: a button that
            // stays 82px while the screen doubles is half the size it looks.
            assert!(
                r.right - r.left >= ((STOP_R - STOP_L) as f32 * scale) as i32 - 1,
                "button did not scale at {scale}x"
            );
        }
    }

    // Esc's half of `stop_requested` reads real key state and so is not
    // reachable from a test; the signal the pill raises is.
    #[test]
    fn a_raised_stop_signal_ends_the_loop() {
        let stop: StopSignal = Arc::new(AtomicBool::new(false));
        assert!(!stop_requested(&stop));
        stop.store(true, Ordering::Relaxed);
        assert!(stop_requested(&stop));
    }

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
