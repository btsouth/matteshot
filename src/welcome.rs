//! First-run welcome surface. Non-modal so the resident keeps processing
//! PrtScn and tray messages while onboarding is visible.

use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, AtomicU8, Ordering};

use anyhow::{Context, Result};
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePen,
    CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, GetMonitorInfoW,
    InvalidateRect, RoundRect, SelectObject, SetBkMode, SetTextColor, UpdateWindow,
    CLEARTYPE_QUALITY, DEFAULT_CHARSET, DRAW_TEXT_FORMAT, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT,
    DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DT_WORDBREAK, FF_DONTCARE, HDC, HFONT, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, PS_SOLID, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;

use windows::Win32::UI::Input::KeyboardAndMouse::{
    TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ESCAPE, VK_RETURN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos,
    GetWindowLongPtrW, IsWindow, LoadCursorW, RegisterClassW, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow, CREATESTRUCTW, CS_HREDRAW,
    CS_VREDRAW, GWLP_USERDATA, HWND_NOTOPMOST, HWND_TOPMOST, IDC_ARROW, SWP_NOMOVE, SWP_NOSIZE,
    SWP_SHOWWINDOW, SW_RESTORE, SW_SHOW, WINDOW_STYLE, WM_CLOSE, WM_ERASEBKGND, WM_KEYDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_SETTINGCHANGE, WNDCLASSW,
    WS_CAPTION, WS_EX_APPWINDOW, WS_OVERLAPPED, WS_SYSMENU,
};

use crate::config::Config;

static WINDOW: AtomicIsize = AtomicIsize::new(0);
static ACTION: AtomicU8 = AtomicU8::new(0);

const PRIMARY: i32 = 1;
const SETTINGS: i32 = 2;
const CONSENT: i32 = 3;
const WM_MOUSELEAVE_MSG: u32 = 0x02A3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Capture,
    Settings,
}

struct State {
    theme: crate::theme::Theme,
    scale: f32,
    width: i32,
    height: i32,
    hover: i32,
    tracking_mouse: bool,
    primary: RECT,
    settings: RECT,
    font_brand: HFONT,
    font_title: HFONT,
    font_body: HFONT,
    font_step: HFONT,
    font_small: HFONT,
    /// Hit and hover area for the whole consent row, box and label together,
    /// so the label is clickable rather than only the 18px box.
    consent: RECT,
    /// What the checkbox currently shows. Nothing is written until the user
    /// leaves this screen, so closing it without choosing means no consent.
    consent_checked: bool,
    /// Only ask when the question is unanswered. Showing an unticked box to
    /// someone who already said yes would quietly revoke their consent when
    /// they pressed a button.
    consent_needed: bool,
}

impl State {
    unsafe fn new(scale: f32, width: i32, height: i32) -> Self {
        let mut state = Self {
            theme: crate::theme::current(),
            scale,
            width,
            height,
            hover: 0,
            tracking_mouse: false,
            primary: RECT::default(),
            settings: RECT::default(),
            consent: RECT::default(),
            // Ticked where opt-out is lawful, empty where consent must be
            // asked for. See crate::telemetry::consent_default_checked.
            consent_checked: crate::telemetry::consent_default_checked(),
            consent_needed: crate::config::Config::load().telemetry_unanswered(),
            font_brand: HFONT::default(),
            font_title: HFONT::default(),
            font_body: HFONT::default(),
            font_step: HFONT::default(),
            font_small: HFONT::default(),
        };
        state.apply_scale(scale, width, height);
        state
    }

    /// Rebuild everything measured in pixels for a new monitor scale.
    ///
    /// Every geometry and font here is a multiple of the scale, so a DPI
    /// change is this and a repaint. Kept as one method rather than inlined
    /// into `new` so the two paths cannot drift: a rect added for creation but
    /// forgotten here would stay at the old monitor's size forever.
    unsafe fn apply_scale(&mut self, scale: f32, width: i32, height: i32) {
        let sc = |value: i32| (value as f32 * scale) as i32;
        self.scale = scale;
        self.width = width;
        self.height = height;
        self.primary = RECT {
            left: sc(318),
            top: sc(412),
            right: sc(606),
            bottom: sc(456),
        };
        self.settings = RECT {
            left: sc(206),
            top: sc(412),
            right: sc(306),
            bottom: sc(456),
        };
        // Above the buttons, so the choice is read before either is pressed
        // rather than discovered afterwards.
        self.consent = RECT {
            left: sc(34),
            top: sc(360),
            right: sc(606),
            bottom: sc(392),
        };
        // Create first, then swap: deleting the old handles up front leaves
        // the window with no font at all if a creation fails, and a window
        // that cannot draw text is worse than one drawn at the old size.
        for (slot, height, weight) in [
            (&mut self.font_brand as *mut HFONT, -sc(11), 600),
            (&mut self.font_title as *mut HFONT, -sc(29), 650),
            (&mut self.font_body as *mut HFONT, -sc(14), 400),
            (&mut self.font_step as *mut HFONT, -sc(15), 600),
            (&mut self.font_small as *mut HFONT, -sc(12), 400),
        ] {
            let replacement = make_font(height, weight);
            if replacement.is_invalid() {
                continue;
            }
            let previous = std::mem::replace(&mut *slot, replacement);
            if !previous.is_invalid() {
                let _ = DeleteObject(previous);
            }
        }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.font_brand);
            let _ = DeleteObject(self.font_title);
            let _ = DeleteObject(self.font_body);
            let _ = DeleteObject(self.font_step);
            let _ = DeleteObject(self.font_small);
        }
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

unsafe fn state(hwnd: HWND) -> Option<&'static mut State> {
    (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State).as_mut()
}

fn sc(state: &State, value: i32) -> i32 {
    (value as f32 * state.scale) as i32
}

fn contains(rect: RECT, x: i32, y: i32) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

unsafe fn text(
    hdc: HDC,
    font: HFONT,
    color: COLORREF,
    rect: RECT,
    value: &str,
    flags: DRAW_TEXT_FORMAT,
) {
    SelectObject(hdc, font);
    SetTextColor(hdc, color);
    let mut value: Vec<u16> = value.encode_utf16().collect();
    let mut rect = rect;
    DrawTextW(hdc, &mut value, &mut rect, flags);
}

unsafe fn rounded_panel(hdc: HDC, rect: RECT, fill: COLORREF, line: COLORREF, radius: i32) {
    let brush = CreateSolidBrush(fill);
    let pen = CreatePen(PS_SOLID, 1, line);
    let old_brush = SelectObject(hdc, brush);
    let old_pen = SelectObject(hdc, pen);
    let _ = RoundRect(
        hdc,
        rect.left,
        rect.top,
        rect.right,
        rect.bottom,
        radius,
        radius,
    );
    SelectObject(hdc, old_brush);
    SelectObject(hdc, old_pen);
    let _ = DeleteObject(brush);
    let _ = DeleteObject(pen);
}

unsafe fn button(hdc: HDC, state: &State, rect: RECT, label: &str, primary: bool, hot: bool) {
    let fill = if primary {
        state.theme.accent
    } else if hot {
        state.theme.track
    } else {
        state.theme.chip
    };
    let line = if primary {
        state.theme.accent
    } else {
        state.theme.chip_line
    };
    rounded_panel(hdc, rect, fill, line, sc(state, 10));
    text(
        hdc,
        state.font_step,
        if primary {
            state.theme.accent_text
        } else {
            state.theme.text
        },
        rect,
        label,
        DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
    );
}

unsafe fn step_card(hdc: HDC, state: &State, rect: RECT, number: &str, title: &str, detail: &str) {
    rounded_panel(
        hdc,
        rect,
        state.theme.panel,
        state.theme.chip_line,
        sc(state, 14),
    );
    let number_rect = RECT {
        left: rect.left + sc(state, 15),
        top: rect.top + sc(state, 14),
        right: rect.left + sc(state, 43),
        bottom: rect.top + sc(state, 42),
    };
    rounded_panel(
        hdc,
        number_rect,
        state.theme.accent,
        state.theme.accent,
        sc(state, 14),
    );
    text(
        hdc,
        state.font_step,
        state.theme.accent_text,
        number_rect,
        number,
        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );
    text(
        hdc,
        state.font_step,
        state.theme.text,
        RECT {
            left: rect.left + sc(state, 51),
            top: rect.top + sc(state, 11),
            right: rect.right - sc(state, 12),
            bottom: rect.top + sc(state, 45),
        },
        title,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
    );
    text(
        hdc,
        state.font_small,
        state.theme.muted,
        RECT {
            left: rect.left + sc(state, 15),
            top: rect.top + sc(state, 52),
            right: rect.right - sc(state, 14),
            bottom: rect.bottom - sc(state, 10),
        },
        detail,
        DT_LEFT | DT_WORDBREAK | DT_NOPREFIX,
    );
}

unsafe fn paint(hdc: HDC, state: &State) {
    let background = CreateSolidBrush(state.theme.bg);
    FillRect(
        hdc,
        &RECT {
            left: 0,
            top: 0,
            right: state.width,
            bottom: state.height,
        },
        background,
    );
    let _ = DeleteObject(background);
    SetBkMode(hdc, TRANSPARENT);

    // Small brand mark: the white card on Matteshot's accent field.
    let mark = RECT {
        left: sc(state, 32),
        top: sc(state, 25),
        right: sc(state, 60),
        bottom: sc(state, 53),
    };
    rounded_panel(
        hdc,
        mark,
        state.theme.accent,
        state.theme.accent,
        sc(state, 9),
    );
    rounded_panel(
        hdc,
        RECT {
            left: mark.left + sc(state, 7),
            top: mark.top + sc(state, 8),
            right: mark.right - sc(state, 7),
            bottom: mark.bottom - sc(state, 7),
        },
        state.theme.accent_text,
        state.theme.accent_text,
        sc(state, 3),
    );
    text(
        hdc,
        state.font_brand,
        state.theme.muted,
        RECT {
            left: sc(state, 70),
            top: sc(state, 25),
            right: sc(state, 220),
            bottom: sc(state, 53),
        },
        "MATTESHOT",
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );

    text(
        hdc,
        state.font_title,
        state.theme.text,
        RECT {
            left: sc(state, 32),
            top: sc(state, 68),
            right: state.width - sc(state, 32),
            bottom: sc(state, 108),
        },
        "Screenshots, ready to paste.",
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
    );
    text(
        hdc,
        state.font_body,
        state.theme.muted,
        RECT {
            left: sc(state, 32),
            top: sc(state, 112),
            right: state.width - sc(state, 32),
            bottom: sc(state, 147),
        },
        "Press PrtScn, choose what you want, and Matteshot puts a polished version on your clipboard.",
        DT_LEFT | DT_WORDBREAK | DT_NOPREFIX,
    );

    step_card(
        hdc,
        state,
        RECT {
            left: sc(state, 32),
            top: sc(state, 158),
            right: sc(state, 216),
            bottom: sc(state, 270),
        },
        "1",
        "Press PrtScn",
        "Your screen freezes so you can choose the exact shot.",
    );
    step_card(
        hdc,
        state,
        RECT {
            left: sc(state, 228),
            top: sc(state, 158),
            right: sc(state, 412),
            bottom: sc(state, 270),
        },
        "2",
        "Pick your shot",
        "Click a window or drag a region. The matte picker appears.",
    );
    step_card(
        hdc,
        state,
        RECT {
            left: sc(state, 424),
            top: sc(state, 158),
            right: sc(state, 608),
            bottom: sc(state, 270),
        },
        "3",
        "Paste anywhere",
        "Your selected matte is already copied and ready to paste.",
    );

    let trial = RECT {
        left: sc(state, 32),
        top: sc(state, 282),
        right: sc(state, 608),
        bottom: sc(state, 340),
    };
    rounded_panel(
        hdc,
        trial,
        state.theme.panel,
        state.theme.chip_line,
        sc(state, 12),
    );
    let dot = RECT {
        left: trial.left + sc(state, 16),
        top: trial.top + sc(state, 21),
        right: trial.left + sc(state, 28),
        bottom: trial.top + sc(state, 33),
    };
    rounded_panel(
        hdc,
        dot,
        state.theme.accent,
        state.theme.accent,
        sc(state, 6),
    );
    text(
        hdc,
        state.font_step,
        state.theme.text,
        RECT {
            left: trial.left + sc(state, 40),
            top: trial.top + sc(state, 5),
            right: trial.right - sc(state, 14),
            bottom: trial.top + sc(state, 30),
        },
        "14 days free. No card required.",
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );
    text(
        hdc,
        state.font_small,
        state.theme.muted,
        RECT {
            left: trial.left + sc(state, 40),
            top: trial.top + sc(state, 26),
            right: trial.right - sc(state, 14),
            bottom: trial.bottom - sc(state, 4),
        },
        "Your trial starts with your first completed capture.",
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );

    text(
        hdc,
        state.font_small,
        state.theme.faint,
        RECT {
            left: sc(state, 32),
            top: sc(state, 412),
            right: sc(state, 194),
            bottom: sc(state, 456),
        },
        "Ready in your tray.",
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );
    // Consent row. Unticked, and deliberately not styled as a call to
    // action: a pre-ticked box is not consent, and a box people tick by
    // reflex is not much better.
    if state.consent_needed {
        let box_side = sc(state, 18);
        let box_rect = RECT {
            left: state.consent.left,
            top: state.consent.top + sc(state, 6),
            right: state.consent.left + box_side,
            bottom: state.consent.top + sc(state, 6) + box_side,
        };
        let checked = state.consent_checked;
        rounded_panel(
            hdc,
            box_rect,
            if checked { state.theme.accent } else { state.theme.panel },
            if state.hover == CONSENT { state.theme.accent } else { state.theme.chip_line },
            sc(state, 4),
        );
        if checked {
            text(
                hdc,
                state.font_small,
                state.theme.accent_text,
                box_rect,
                "\u{2713}",
                DT_CENTER | DT_SINGLELINE | DT_VCENTER,
            );
        }
        text(
            hdc,
            state.font_small,
            state.theme.muted,
            RECT {
                left: state.consent.left + box_side + sc(state, 10),
                top: state.consent.top,
                right: state.consent.right,
                bottom: state.consent.bottom,
            },
            "Share anonymous usage stats. No screenshots, text, file names or paths.",
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );
    }
    button(
        hdc,
        state,
        state.settings,
        "Settings",
        false,
        state.hover == SETTINGS,
    );
    button(
        hdc,
        state,
        state.primary,
        "Take your first screenshot",
        true,
        state.hover == PRIMARY,
    );
}

/// Write the answer the user is leaving with.
///
/// Only called when they act on this screen. Closing it with Escape or the X
/// leaves the setting unanswered, so nothing is sent and the question can be
/// asked again rather than silently defaulting to yes.
fn record_consent(state: &State) {
    if !state.consent_needed {
        return;
    }
    let allowed = state.consent_checked;
    // Re-checked inside the update, which holds the config lock. This window
    // is non-modal, so Settings can answer the question while it sits open,
    // and pressing a button here must not overwrite that real answer with the
    // default this screen happened to open with.
    let settled = crate::config::Config::update(|cfg| {
        if cfg.telemetry.is_none() {
            cfg.telemetry = Some(allowed);
        }
    });
    match settled {
        Ok(settled) => crate::telemetry::set_enabled(settled.telemetry_enabled()),
        Err(error) => crate::diagnostics::log(&format!(
            "telemetry consent could not be saved: {error:#}"
        )),
    }
}
unsafe fn activate(hwnd: HWND, action: u8) {
    ACTION.store(action, Ordering::SeqCst);
    let _ = DestroyWindow(hwnd);
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_NCCREATE => {
            let create = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            LRESULT(1)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            if let Some(state) = state(hwnd) {
                let memory = CreateCompatibleDC(hdc);
                let bitmap = CreateCompatibleBitmap(hdc, state.width, state.height);
                let old_bitmap = SelectObject(memory, bitmap);
                paint(memory, state);
                let _ = BitBlt(hdc, 0, 0, state.width, state.height, memory, 0, 0, SRCCOPY);
                SelectObject(memory, old_bitmap);
                let _ = DeleteObject(bitmap);
                let _ = DeleteDC(memory);
            }
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        // Dragged to a monitor at a different scale. The window is fixed-size,
        // so this is the new scale, a rebuild of everything measured from it,
        // and the bounds Windows suggests.
        windows::Win32::UI::WindowsAndMessaging::WM_DPICHANGED => {
            if let Some(state) = state(hwnd) {
                let scale = crate::dpi::scale_from_message(wparam);
                let sc = |value: i32| (value as f32 * scale) as i32;
                state.apply_scale(scale, sc(640), sc(476));
                crate::dpi::apply_suggested_bounds(hwnd, lparam);
                let _ = InvalidateRect(hwnd, None, true);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_MOUSEMOVE => {
            if let Some(state) = state(hwnd) {
                if !state.tracking_mouse {
                    let mut track = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        ..Default::default()
                    };
                    let _ = TrackMouseEvent(&mut track);
                    state.tracking_mouse = true;
                }
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                let hover = if contains(state.primary, x, y) {
                    PRIMARY
                } else if contains(state.settings, x, y) {
                    SETTINGS
                } else if state.consent_needed && contains(state.consent, x, y) {
                    CONSENT
                } else {
                    0
                };
                if state.hover != hover {
                    state.hover = hover;
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE_MSG => {
            if let Some(state) = state(hwnd) {
                state.tracking_mouse = false;
                state.hover = 0;
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if let Some(state) = state(hwnd) {
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                if state.consent_needed && contains(state.consent, x, y) {
                    state.consent_checked = !state.consent_checked;
                    let _ = InvalidateRect(hwnd, None, false);
                } else if contains(state.primary, x, y) {
                    record_consent(state);
                    activate(hwnd, PRIMARY as u8);
                } else if contains(state.settings, x, y) {
                    record_consent(state);
                    activate(hwnd, SETTINGS as u8);
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if wparam.0 as u16 == VK_RETURN.0 {
                if let Some(state) = state(hwnd) {
                    record_consent(state);
                }
                activate(hwnd, PRIMARY as u8);
            } else if wparam.0 as u16 == VK_ESCAPE.0 {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            if let Some(state) = state(hwnd) {
                state.theme = crate::theme::current();
                crate::theme::apply_titlebar(hwnd, &state.theme);
                let _ = InvalidateRect(hwnd, None, true);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            let ptr = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *mut State;
            if !ptr.is_null() {
                drop(Box::from_raw(ptr));
            }
            WINDOW.store(0, Ordering::SeqCst);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

fn open(mark_seen: bool) -> Result<()> {
    unsafe {
        let existing = HWND(WINDOW.load(Ordering::SeqCst) as *mut _);
        if !existing.0.is_null() && IsWindow(existing).as_bool() {
            let _ = ShowWindow(existing, SW_RESTORE);
            let _ = SetForegroundWindow(existing);
            return Ok(());
        }

        ACTION.store(0, Ordering::SeqCst);
        let instance = GetModuleHandleW(None).context("get app module for welcome")?;
        // This opens centred on the cursor's monitor, so it is that monitor's
        // scale that decides its size, not the primary one's.
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        let scale = crate::dpi::scale_for_point(cursor);
        let sc = |value: i32| (value as f32 * scale) as i32;
        let (client_width, client_height) = (sc(640), sc(476));
        let state = Box::new(State::new(scale, client_width, client_height));
        let leaked = Box::into_raw(state);

        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hIcon: crate::tray::app_icon(),
            lpszClassName: w!("matteshot_welcome"),
            ..Default::default()
        };
        RegisterClassW(&class);

        let style: WINDOW_STYLE = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU;
        let ex_style = WS_EX_APPWINDOW;
        let bounds = crate::dpi::outer_bounds(
            RECT { left: 0, top: 0, right: client_width, bottom: client_height },
            style,
            ex_style,
            scale,
        );
        let width = bounds.right - bounds.left;
        let height = bounds.bottom - bounds.top;

        let mut monitor_info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let monitor =
            windows::Win32::Graphics::Gdi::MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
        let _ = GetMonitorInfoW(monitor, &mut monitor_info);
        let x = monitor_info.rcWork.left
            + (monitor_info.rcWork.right - monitor_info.rcWork.left - width) / 2;
        let y = monitor_info.rcWork.top
            + (monitor_info.rcWork.bottom - monitor_info.rcWork.top - height) / 2;

        let hwnd = match CreateWindowExW(
            ex_style,
            w!("matteshot_welcome"),
            w!("Welcome to Matteshot"),
            style,
            x,
            y,
            width,
            height,
            None,
            None,
            instance,
            Some(leaked as *const c_void),
        ) {
            Ok(hwnd) => hwnd,
            Err(error) => {
                drop(Box::from_raw(leaked));
                return Err(error).context("create welcome window");
            }
        };

        WINDOW.store(hwnd.0 as isize, Ordering::SeqCst);
        let _ = SetWindowTextW(hwnd, w!("Welcome to Matteshot"));
        let theme = (*leaked).theme;
        crate::theme::apply_titlebar(hwnd, &theme);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
        let _ = SetWindowPos(
            hwnd,
            HWND_NOTOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
        let _ = SetForegroundWindow(hwnd);
        let _ = UpdateWindow(hwnd);

        if mark_seen {
            let _ = Config::update(|config| config.onboarded = true);
        }
        Ok(())
    }
}

pub fn open_first_run() -> Result<()> {
    open(true)
}

pub fn open_preview() -> Result<()> {
    open(false)
}

pub fn is_open() -> bool {
    unsafe {
        let hwnd = HWND(WINDOW.load(Ordering::SeqCst) as *mut _);
        !hwnd.0.is_null() && IsWindow(hwnd).as_bool()
    }
}

pub fn take_action() -> Option<Action> {
    match ACTION.swap(0, Ordering::SeqCst) {
        1 => Some(Action::Capture),
        2 => Some(Action::Settings),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_are_single_use() {
        ACTION.store(PRIMARY as u8, Ordering::SeqCst);
        assert_eq!(take_action(), Some(Action::Capture));
        assert_eq!(take_action(), None);
        ACTION.store(SETTINGS as u8, Ordering::SeqCst);
        assert_eq!(take_action(), Some(Action::Settings));
        assert_eq!(take_action(), None);
    }
}
