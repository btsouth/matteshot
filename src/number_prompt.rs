//! Small native numeric prompt used by screenshot output-size controls.

use anyhow::{Context, Result};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect,
    InvalidateRect, SelectObject, SetBkColor, SetBkMode, SetTextColor, CLEARTYPE_QUALITY,
    DEFAULT_CHARSET, DT_CENTER, DT_LEFT, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, HBRUSH, HFONT,
    PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetClientRect, GetMessageW, GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, IsWindow, LoadCursorW, RegisterClassW, SendMessageW,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage,
    CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, ES_AUTOHSCROLL, ES_NUMBER, GWLP_USERDATA,
    HWND_NOTOPMOST, HWND_TOPMOST, IDC_ARROW, MSG, SWP_NOMOVE, SWP_NOSIZE, SW_SHOW,
    WINDOW_STYLE, WM_CLOSE, WM_CTLCOLOREDIT, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONUP,
    WM_NCCREATE, WM_PAINT, WNDCLASSW, WS_CAPTION, WS_CHILD, WS_EX_CLIENTEDGE,
    WS_EX_DLGMODALFRAME, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};

use crate::output::{OUTPUT_CUSTOM_MAX, OUTPUT_CUSTOM_MIN, OUTPUT_EMAIL};

const CLASS: PCWSTR = w!("MatteshotOutputSizePrompt");
const EM_SETSEL: u32 = 177;

struct State {
    edit: HWND,
    result: Option<u32>,
    invalid: bool,
    font: HFONT,
    font_small: HFONT,
    edit_brush: HBRUSH,
    theme: crate::theme::Theme,
    scale: f32,
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

fn sc(state: &State, value: i32) -> i32 {
    (value as f32 * state.scale) as i32
}

unsafe fn font(height: i32, weight: i32) -> HFONT {
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

unsafe fn draw_text(hdc: windows::Win32::Graphics::Gdi::HDC, state: &State, rect: RECT, text: &str, small: bool, color: COLORREF, flags: u32) {
    SelectObject(hdc, if small { state.font_small } else { state.font });
    SetTextColor(hdc, color);
    let mut value = wide(text);
    let mut rect = rect;
    DrawTextW(
        hdc,
        &mut value,
        &mut rect,
        windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT(flags) | DT_SINGLELINE | DT_VCENTER,
    );
}

unsafe fn commit(hwnd: HWND, state: &mut State) {
    let len = GetWindowTextLengthW(state.edit);
    let mut text = vec![0u16; len as usize + 1];
    GetWindowTextW(state.edit, &mut text);
    let parsed = String::from_utf16_lossy(&text[..len as usize]).parse::<u32>().ok();
    if let Some(value) = parsed.filter(|value| (OUTPUT_CUSTOM_MIN..=OUTPUT_CUSTOM_MAX).contains(value)) {
        state.result = Some(value);
        let _ = DestroyWindow(hwnd);
    } else {
        state.invalid = true;
        let _ = InvalidateRect(hwnd, None, false);
        let _ = SetFocus(state.edit);
        SendMessageW(state.edit, EM_SETSEL, WPARAM(0), LPARAM(-1));
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let create = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_CTLCOLOREDIT => {
            if let Some(state) = state(hwnd) {
                let hdc = windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut _);
                SetTextColor(hdc, state.theme.text);
                SetBkColor(hdc, state.theme.chip);
                return LRESULT(state.edit_brush.0 as isize);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_PAINT => {
            if let Some(state) = state(hwnd) {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                let mut client = RECT::default();
                let _ = GetClientRect(hwnd, &mut client);
                let bg = CreateSolidBrush(state.theme.bg);
                FillRect(hdc, &client, bg);
                let _ = DeleteObject(bg);
                SetBkMode(hdc, TRANSPARENT);
                let margin = sc(state, 22);
                draw_text(
                    hdc,
                    state,
                    RECT { left: margin, top: sc(state, 14), right: client.right - margin, bottom: sc(state, 42) },
                    "Custom output size",
                    false,
                    state.theme.text,
                    DT_LEFT.0,
                );
                draw_text(
                    hdc,
                    state,
                    RECT { left: margin, top: sc(state, 46), right: client.right - margin, bottom: sc(state, 68) },
                    "Maximum edge in pixels",
                    true,
                    state.theme.muted,
                    DT_LEFT.0,
                );
                let helper = if state.invalid {
                    "Enter a value from 320 to 10,000."
                } else {
                    "Images are never enlarged."
                };
                draw_text(
                    hdc,
                    state,
                    RECT { left: margin, top: sc(state, 108), right: client.right - margin, bottom: sc(state, 132) },
                    helper,
                    true,
                    if state.invalid { COLORREF(0x005858e8) } else { state.theme.muted },
                    DT_LEFT.0,
                );
                let apply = RECT { left: client.right - sc(state, 190), top: sc(state, 145), right: client.right - sc(state, 104), bottom: sc(state, 179) };
                let cancel = RECT { left: client.right - sc(state, 96), top: sc(state, 145), right: client.right - margin, bottom: sc(state, 179) };
                for (rect, label, accent) in [(apply, "Apply", true), (cancel, "Cancel", false)] {
                    let brush = CreateSolidBrush(if accent { state.theme.accent } else { state.theme.chip });
                    FillRect(hdc, &rect, brush);
                    let _ = DeleteObject(brush);
                    draw_text(hdc, state, rect, label, true, if accent { state.theme.accent_text } else { state.theme.text }, DT_CENTER.0);
                }
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if let Some(state) = state(hwnd) {
                let x = (lparam.0 & 0xffff) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xffff) as i16 as i32;
                let mut client = RECT::default();
                let _ = GetClientRect(hwnd, &mut client);
                if y >= sc(state, 145) && y <= sc(state, 179) {
                    if x >= client.right - sc(state, 190) && x <= client.right - sc(state, 104) {
                        commit(hwnd, state);
                    } else if x >= client.right - sc(state, 96) && x <= client.right - sc(state, 22) {
                        let _ = DestroyWindow(hwnd);
                    }
                }
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Ask for a longest-edge pixel cap. Returns `None` when cancelled.
pub fn ask(owner: HWND, current: u32) -> Result<Option<u32>> {
    unsafe {
        let instance = GetModuleHandleW(None).context("get app module for output-size prompt")?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: CLASS,
            style: CS_HREDRAW | CS_VREDRAW,
            ..Default::default()
        };
        let _ = RegisterClassW(&class);

        let scale = GetDpiForSystem() as f32 / 96.0;
        let px = |value: i32| (value as f32 * scale) as i32;
        let theme = crate::theme::current();
        let mut state = Box::new(State {
            edit: HWND::default(),
            result: None,
            invalid: false,
            font: font(-px(16), 600),
            font_small: font(-px(13), 400),
            edit_brush: CreateSolidBrush(theme.chip),
            theme,
            scale,
        });

        let style = WS_CAPTION | WS_SYSMENU;
        let ex_style = WS_EX_DLGMODALFRAME;
        let mut bounds = RECT { left: 0, top: 0, right: px(360), bottom: px(196) };
        AdjustWindowRectEx(&mut bounds, style, false, ex_style).context("size output prompt")?;
        let (mut x, mut y) = (100, 100);
        let mut owner_rect = RECT::default();
        if !owner.0.is_null() && GetWindowRect(owner, &mut owner_rect).is_ok() {
            x = owner_rect.left + ((owner_rect.right - owner_rect.left) - (bounds.right - bounds.left)) / 2;
            y = owner_rect.top + ((owner_rect.bottom - owner_rect.top) - (bounds.bottom - bounds.top)) / 2;
        }
        let hwnd = CreateWindowExW(
            ex_style,
            CLASS,
            w!("Output size"),
            style,
            x,
            y,
            bounds.right - bounds.left,
            bounds.bottom - bounds.top,
            owner,
            None,
            instance,
            Some((&mut *state as *mut State).cast()),
        )?;
        let initial = if (OUTPUT_CUSTOM_MIN..=OUTPUT_CUSTOM_MAX).contains(&current) { current } else { OUTPUT_EMAIL };
        let text = HSTRING::from(initial.to_string());
        state.edit = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            w!("EDIT"),
            PCWSTR(text.as_ptr()),
            WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | WINDOW_STYLE((ES_NUMBER | ES_AUTOHSCROLL) as u32),
            px(22),
            px(72),
            px(316),
            px(34),
            hwnd,
            None,
            instance,
            None,
        )?;
        if !owner.0.is_null() {
            let _ = EnableWindow(owner, false);
        }
        SendMessageW(state.edit, windows::Win32::UI::WindowsAndMessaging::WM_SETFONT, WPARAM(state.font.0 as usize), LPARAM(1));
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
        let _ = SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(state.edit);
        SendMessageW(state.edit, EM_SETSEL, WPARAM(0), LPARAM(-1));

        let mut message = MSG::default();
        while IsWindow(hwnd).as_bool() && GetMessageW(&mut message, None, 0, 0).as_bool() {
            if message.message == WM_KEYDOWN {
                match message.wParam.0 as u16 {
                    key if key == VK_RETURN.0 => {
                        commit(hwnd, &mut state);
                        continue;
                    }
                    key if key == VK_ESCAPE.0 => {
                        let _ = DestroyWindow(hwnd);
                        continue;
                    }
                    _ => {}
                }
            }
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        if !owner.0.is_null() {
            let _ = EnableWindow(owner, true);
            let _ = SetForegroundWindow(owner);
        }
        let result = state.result;
        let _ = DeleteObject(state.font);
        let _ = DeleteObject(state.font_small);
        let _ = DeleteObject(state.edit_brush);
        Ok(result)
    }
}
