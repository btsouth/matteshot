//! Polished native activation surface. Kept separate from Settings so an
//! expired trial can be activated before capture hotkeys are registered.

use std::ffi::c_void;

use anyhow::{Context, Result};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePen,
    CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, GetMonitorInfoW,
    InvalidateRect, MonitorFromPoint, RoundRect, SelectObject, SetBkColor, SetBkMode, SetTextColor,
    UpdateWindow, CLEARTYPE_QUALITY, DEFAULT_CHARSET, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT,
    DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DT_WORDBREAK, FF_DONTCARE, HBRUSH, HDC, HFONT,
    MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, PS_SOLID, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, SetFocus, VK_ESCAPE, VK_RETURN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetCursorPos, GetMessageW, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW, IsWindow,
    LoadCursorW, MoveWindow, PostMessageW, RegisterClassW,
    SendMessageW, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    TranslateMessage, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, ES_AUTOHSCROLL, GWLP_USERDATA,
    HMENU, HWND_NOTOPMOST, HWND_TOPMOST, IDC_ARROW, MSG, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
    SW_SHOW, WM_APP, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_CTLCOLOREDIT, WM_DESTROY,
    WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCCREATE, WM_PAINT, WM_SETFONT,
    WINDOW_STYLE, WNDCLASSW, WS_CAPTION, WS_CHILD, WS_EX_APPWINDOW, WS_OVERLAPPED, WS_SYSMENU,
    WS_TABSTOP, WS_VISIBLE,
};

const ID_KEY: isize = 101;
const ID_ACTIVATE: usize = 102;
const ID_BUY: usize = 103;
const ID_CANCEL: usize = 104;
const WM_ACTIVATION_DONE: u32 = WM_APP + 31;

struct ActivationDone {
    result: std::result::Result<(), String>,
}

static ACTIVATION_COMPLETIONS: crate::completion::CompletionMailbox<ActivationDone> =
    crate::completion::CompletionMailbox::new();

struct UiState {
    edit: HWND,
    activated: bool,
    activating: bool,
    status: String,
    hover: usize,
    scale: f32,
    width: i32,
    height: i32,
    edit_border: RECT,
    activate_button: RECT,
    buy_button: RECT,
    cancel_button: RECT,
    background: HBRUSH,
    edit_background: HBRUSH,
    font_title: HFONT,
    font: HFONT,
    font_small: HFONT,
    theme: crate::theme::Theme,
}

impl UiState {
    /// Rebuild everything measured in pixels for a monitor scale.
    ///
    /// Shared by construction and by `WM_DPICHANGED` so the two cannot drift:
    /// a control added to one and forgotten in the other would keep the old
    /// monitor's size for the rest of the window's life.
    unsafe fn apply_scale(&mut self, scale: f32, width: i32, height: i32) {
        let sc = |value: i32| (value as f32 * scale) as i32;
        self.scale = scale;
        self.width = width;
        self.height = height;
        self.edit_border = RECT {
            left: sc(32),
            top: sc(116),
            right: width - sc(32),
            bottom: sc(164),
        };
        let button_top = height - sc(66);
        self.activate_button = RECT {
            left: width - sc(344),
            top: button_top,
            right: width - sc(224),
            bottom: button_top + sc(36),
        };
        self.buy_button = RECT {
            left: width - sc(216),
            top: button_top,
            right: width - sc(90),
            bottom: button_top + sc(36),
        };
        self.cancel_button = RECT {
            left: width - sc(82),
            top: button_top,
            right: width - sc(24),
            bottom: button_top + sc(36),
        };
        // Create first, then swap — see welcome::State::apply_scale.
        for (slot, height, weight) in [
            (&mut self.font_title as *mut HFONT, -sc(25), 600),
            (&mut self.font as *mut HFONT, -sc(14), 400),
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
        // The edit is a real child window, so it has to be moved rather than
        // just re-measured, and told about the new font.
        if !self.edit.is_invalid() {
            let _ = MoveWindow(
                self.edit,
                self.edit_border.left + sc(12),
                self.edit_border.top + sc(8),
                self.edit_border.right - self.edit_border.left - sc(24),
                self.edit_border.bottom - self.edit_border.top - sc(16),
                true,
            );
            SendMessageW(self.edit, WM_SETFONT, WPARAM(self.font.0 as usize), LPARAM(1));
        }
    }

    fn new(scale: f32, width: i32, height: i32) -> Self {
        let theme = crate::theme::current();
        let mut state = Self {
            edit: HWND::default(),
            activated: false,
            activating: false,
            status: if matches!(crate::license::status(), crate::license::Status::Expired) {
                "Your 14-day trial has ended. Enter your license key to keep capturing.".into()
            } else {
                "Enter the license key from your purchase email.".into()
            },
            hover: 0,
            scale,
            width,
            height,
            edit_border: RECT::default(),
            activate_button: RECT::default(),
            buy_button: RECT::default(),
            cancel_button: RECT::default(),
            background: unsafe { CreateSolidBrush(theme.bg) },
            edit_background: unsafe { CreateSolidBrush(theme.chip) },
            font_title: HFONT::default(),
            font: HFONT::default(),
            font_small: HFONT::default(),
            theme,
        };
        unsafe { state.apply_scale(scale, width, height) };
        state
    }
}

impl Drop for UiState {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.background);
            let _ = DeleteObject(self.edit_background);
            let _ = DeleteObject(self.font_title);
            let _ = DeleteObject(self.font);
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

unsafe fn state(hwnd: HWND) -> Option<&'static mut UiState> {
    (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut UiState).as_mut()
}

fn contains(rect: RECT, x: i32, y: i32) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

unsafe fn paint_button(
    hdc: HDC,
    state: &UiState,
    rect: RECT,
    label: &str,
    primary: bool,
    hot: bool,
) {
    let fill_color = if primary {
        state.theme.accent
    } else if hot {
        state.theme.track
    } else {
        state.theme.chip
    };
    let line_color = if primary {
        state.theme.accent
    } else {
        state.theme.chip_line
    };
    let fill = CreateSolidBrush(fill_color);
    let pen = CreatePen(PS_SOLID, 1, line_color);
    let old_brush = SelectObject(hdc, fill);
    let old_pen = SelectObject(hdc, pen);
    let radius = (9.0 * state.scale) as i32;
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
    let _ = DeleteObject(fill);
    let _ = DeleteObject(pen);

    SelectObject(hdc, state.font);
    SetTextColor(
        hdc,
        if primary {
            state.theme.accent_text
        } else {
            state.theme.text
        },
    );
    let mut text: Vec<u16> = label.encode_utf16().collect();
    let mut text_rect = rect;
    DrawTextW(
        hdc,
        &mut text,
        &mut text_rect,
        DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
    );
}

unsafe fn paint(hdc: HDC, state: &UiState) {
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
    let sc = |value: i32| (value as f32 * state.scale) as i32;

    SelectObject(hdc, state.font_title);
    SetTextColor(hdc, state.theme.text);
    let mut title: Vec<u16> = "Activate Matteshot".encode_utf16().collect();
    let mut title_rect = RECT {
        left: sc(32),
        top: sc(24),
        right: state.width - sc(32),
        bottom: sc(58),
    };
    DrawTextW(hdc, &mut title, &mut title_rect, DT_LEFT | DT_SINGLELINE | DT_VCENTER);

    SelectObject(hdc, state.font);
    SetTextColor(hdc, state.theme.muted);
    let mut subtitle: Vec<u16> = state.status.encode_utf16().collect();
    let mut subtitle_rect = RECT {
        left: sc(32),
        top: sc(65),
        right: state.width - sc(32),
        bottom: sc(104),
    };
    DrawTextW(
        hdc,
        &mut subtitle,
        &mut subtitle_rect,
        DT_LEFT | DT_WORDBREAK | DT_NOPREFIX,
    );

    let edit_fill = CreateSolidBrush(state.theme.chip);
    let edit_pen = CreatePen(PS_SOLID, 1, state.theme.chip_line);
    let old_brush = SelectObject(hdc, edit_fill);
    let old_pen = SelectObject(hdc, edit_pen);
    let radius = sc(10);
    let _ = RoundRect(
        hdc,
        state.edit_border.left,
        state.edit_border.top,
        state.edit_border.right,
        state.edit_border.bottom,
        radius,
        radius,
    );
    SelectObject(hdc, old_brush);
    SelectObject(hdc, old_pen);
    let _ = DeleteObject(edit_fill);
    let _ = DeleteObject(edit_pen);

    let info = RECT {
        left: sc(32),
        top: sc(180),
        right: state.width - sc(32),
        bottom: sc(232),
    };
    let info_fill = CreateSolidBrush(state.theme.panel);
    let info_pen = CreatePen(PS_SOLID, 1, state.theme.chip_line);
    let old_brush = SelectObject(hdc, info_fill);
    let old_pen = SelectObject(hdc, info_pen);
    let _ = RoundRect(hdc, info.left, info.top, info.right, info.bottom, radius, radius);
    SelectObject(hdc, old_brush);
    SelectObject(hdc, old_pen);
    let _ = DeleteObject(info_fill);
    let _ = DeleteObject(info_pen);

    SelectObject(hdc, state.font_small);
    SetTextColor(hdc, state.theme.text);
    let mut info_title: Vec<u16> = "One purchase. Three Windows PCs.".encode_utf16().collect();
    let mut info_title_rect = RECT {
        left: info.left + sc(16),
        top: info.top + sc(7),
        right: info.right - sc(16),
        bottom: info.top + sc(27),
    };
    DrawTextW(
        hdc,
        &mut info_title,
        &mut info_title_rect,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );
    SetTextColor(hdc, state.theme.muted);
    let mut info_subtitle: Vec<u16> = "No subscription. Your license keeps working offline."
        .encode_utf16()
        .collect();
    let mut info_subtitle_rect = RECT {
        left: info.left + sc(16),
        top: info.top + sc(25),
        right: info.right - sc(16),
        bottom: info.bottom - sc(5),
    };
    DrawTextW(
        hdc,
        &mut info_subtitle,
        &mut info_subtitle_rect,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
    );

    let activate_label = if state.activated {
        "Done"
    } else if state.activating {
        "Activating..."
    } else {
        "Activate"
    };
    paint_button(
        hdc,
        state,
        state.activate_button,
        activate_label,
        true,
        state.hover == ID_ACTIVATE,
    );
    paint_button(
        hdc,
        state,
        state.buy_button,
        "Buy Matteshot",
        false,
        state.hover == ID_BUY,
    );
    paint_button(
        hdc,
        state,
        state.cancel_button,
        "Cancel",
        false,
        state.hover == ID_CANCEL,
    );
}

unsafe fn begin_activation(hwnd: HWND, state: &mut UiState) {
    if state.activated {
        let _ = DestroyWindow(hwnd);
        return;
    }
    if state.activating {
        return;
    }
    let length = GetWindowTextLengthW(state.edit).max(0) as usize;
    let mut buffer = vec![0u16; length + 1];
    let copied = GetWindowTextW(state.edit, &mut buffer);
    let key = String::from_utf16_lossy(&buffer[..copied.max(0) as usize]);
    if key.trim().is_empty() {
        state.status = "Enter the license key from your purchase email.".into();
        let _ = InvalidateRect(hwnd, None, false);
        return;
    }

    state.activating = true;
    state.status = "Checking your license...".into();
    let _ = EnableWindow(state.edit, false);
    let _ = InvalidateRect(hwnd, None, false);
    let _ = UpdateWindow(hwnd);
    let hwnd_raw = hwnd.0 as isize;
    let mailbox_generation = ACTIVATION_COMPLETIONS.generation_of(hwnd_raw);
    std::thread::spawn(move || {
        let result = crate::license::activate(&key)
            .map(|_| ())
            .map_err(|error| format!("{error:#}"));
        ACTIVATION_COMPLETIONS.post_with_at(
            hwnd_raw,
            mailbox_generation,
            ActivationDone { result },
            |token| unsafe {
                PostMessageW(
                    HWND(hwnd_raw as *mut _),
                    WM_ACTIVATION_DONE,
                    WPARAM(0),
                    LPARAM(token as isize),
                )
                .is_ok()
            },
        );
    });
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
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        WM_CREATE => {
            let Some(state) = state(hwnd) else {
                return LRESULT(-1);
            };
            let Ok(module) = GetModuleHandleW(None) else {
                return LRESULT(-1);
            };
            let sc = |value: i32| (value as f32 * state.scale) as i32;
            state.edit = CreateWindowExW(
                Default::default(),
                w!("EDIT"),
                w!(""),
                WS_CHILD
                    | WS_VISIBLE
                    | WS_TABSTOP
                    | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                state.edit_border.left + sc(12),
                state.edit_border.top + sc(8),
                state.edit_border.right - state.edit_border.left - sc(24),
                state.edit_border.bottom - state.edit_border.top - sc(16),
                hwnd,
                HMENU(ID_KEY as *mut c_void),
                module,
                None,
            )
            .unwrap_or_default();
            let _ = SendMessageW(
                state.edit,
                WM_SETFONT,
                WPARAM(state.font.0 as usize),
                LPARAM(1),
            );
            let _ = SetFocus(state.edit);
            LRESULT(0)
        }
        WM_COMMAND => {
            let command = wparam.0 & 0xffff;
            if let Some(state) = state(hwnd) {
                match command {
                    ID_ACTIVATE => begin_activation(hwnd, state),
                    ID_BUY => crate::output::open_url(crate::license::BUY_URL),
                    ID_CANCEL => {
                        let _ = DestroyWindow(hwnd);
                    }
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_ACTIVATION_DONE => {
            let Some(done) = ACTIVATION_COMPLETIONS.take(lparam.0 as u64, hwnd.0 as isize) else {
                return LRESULT(0);
            };
            if let Some(state) = state(hwnd) {
                state.activating = false;
                let _ = EnableWindow(state.edit, true);
                match done.result {
                    Ok(()) => {
                        state.activated = true;
                        state.status = "Activated. Matteshot is ready to capture.".into();
                    }
                    Err(error) => {
                        state.status = error;
                        let _ = SetFocus(state.edit);
                    }
                }
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_PAINT => {
            if let Some(state) = state(hwnd) {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                let memory = CreateCompatibleDC(hdc);
                let bitmap = CreateCompatibleBitmap(hdc, state.width, state.height);
                let old = SelectObject(memory, bitmap);
                paint(memory, state);
                let _ = BitBlt(
                    hdc,
                    0,
                    0,
                    state.width,
                    state.height,
                    memory,
                    0,
                    0,
                    SRCCOPY,
                );
                SelectObject(memory, old);
                let _ = DeleteObject(bitmap);
                let _ = DeleteDC(memory);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if let Some(state) = state(hwnd) {
                let x = (lparam.0 & 0xffff) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xffff) as i16 as i32;
                let hover = if contains(state.activate_button, x, y) {
                    ID_ACTIVATE
                } else if contains(state.buy_button, x, y) {
                    ID_BUY
                } else if contains(state.cancel_button, x, y) {
                    ID_CANCEL
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
        WM_LBUTTONUP => {
            if let Some(state) = state(hwnd) {
                let x = (lparam.0 & 0xffff) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xffff) as i16 as i32;
                if contains(state.activate_button, x, y) {
                    begin_activation(hwnd, state);
                } else if contains(state.buy_button, x, y) {
                    crate::output::open_url(crate::license::BUY_URL);
                } else if contains(state.cancel_button, x, y) {
                    let _ = DestroyWindow(hwnd);
                }
            }
            LRESULT(0)
        }
        WM_CTLCOLOREDIT => {
            let Some(state) = state(hwnd) else {
                return DefWindowProcW(hwnd, message, wparam, lparam);
            };
            let hdc = HDC(wparam.0 as *mut c_void);
            let _ = SetTextColor(hdc, state.theme.text);
            let _ = SetBkColor(hdc, state.theme.chip);
            LRESULT(state.edit_background.0 as isize)
        }
        // Fixed-size window, so a DPI change is the new scale, a rebuild of
        // everything measured from it, and the bounds Windows suggests.
        windows::Win32::UI::WindowsAndMessaging::WM_DPICHANGED => {
            if let Some(state) = state(hwnd) {
                let scale = crate::dpi::scale_from_message(wparam);
                let sc = |value: i32| (value as f32 * scale) as i32;
                state.apply_scale(scale, sc(600), sc(320));
                crate::dpi::apply_suggested_bounds(hwnd, lparam);
                let _ = InvalidateRect(hwnd, None, true);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            ACTIVATION_COMPLETIONS.unbind(hwnd.0 as isize);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

/// Open a modal activation surface on the current UI thread. Returns true
/// when activation succeeded.
pub fn open() -> Result<bool> {
    unsafe {
        let instance = GetModuleHandleW(None).context("get app module for activation")?;
        // Centres on the cursor's monitor below, so that is the monitor whose
        // scale decides its size.
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        let scale = crate::dpi::scale_for_point(cursor);
        let sc = |value: i32| (value as f32 * scale) as i32;
        let (client_width, client_height) = (sc(600), sc(320));
        let mut ui_state = Box::new(UiState::new(scale, client_width, client_height));
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hIcon: crate::tray::app_icon(),
            hbrBackground: ui_state.background,
            lpszClassName: w!("matteshot_activation"),
            ..Default::default()
        };
        RegisterClassW(&class);

        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU;
        let ex_style = WS_EX_APPWINDOW;
        let bounds = crate::dpi::outer_bounds(
            RECT { left: 0, top: 0, right: client_width, bottom: client_height },
            style,
            ex_style,
            scale,
        );
        let width = bounds.right - bounds.left;
        let height = bounds.bottom - bounds.top;

        // The same cursor reading that chose the scale above. Sampling it
        // twice lets a moving pointer hand this window one monitor's scale and
        // another monitor's work area.
        let mut monitor_info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
        let _ = GetMonitorInfoW(monitor, &mut monitor_info);
        let x = monitor_info.rcWork.left
            + (monitor_info.rcWork.right - monitor_info.rcWork.left - width) / 2;
        let y = monitor_info.rcWork.top
            + (monitor_info.rcWork.bottom - monitor_info.rcWork.top - height) / 2;

        let hwnd = CreateWindowExW(
            ex_style,
            w!("matteshot_activation"),
            w!("Matteshot"),
            style,
            x,
            y,
            width,
            height,
            None,
            None,
            instance,
            Some(&mut *ui_state as *mut UiState as *const c_void),
        )
        .context("create activation window")?;
        crate::theme::apply_titlebar(hwnd, &ui_state.theme);
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

        let mut message = MSG::default();
        while IsWindow(hwnd).as_bool() && GetMessageW(&mut message, None, 0, 0).as_bool() {
            if message.message == WM_KEYDOWN {
                if message.wParam.0 as u16 == VK_RETURN.0 {
                    let _ = SendMessageW(hwnd, WM_COMMAND, WPARAM(ID_ACTIVATE), LPARAM(0));
                    continue;
                }
                if message.wParam.0 as u16 == VK_ESCAPE.0 {
                    let _ = SendMessageW(hwnd, WM_COMMAND, WPARAM(ID_CANCEL), LPARAM(0));
                    continue;
                }
            }
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        Ok(ui_state.activated)
    }
}
