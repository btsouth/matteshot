//! Small native activation window. Kept separate from Settings so an expired
//! trial can be activated without registering or invoking capture hotkeys.

use std::ffi::c_void;

use anyhow::{Context, Result};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, GetStockObject, SetBkColor, SetTextColor, UpdateWindow,
    DEFAULT_GUI_FONT, HBRUSH,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetClientRect, GetMessageW, GetSystemMetrics, GetWindowLongPtrW, GetWindowTextLengthW,
    GetWindowTextW, IsWindow, MessageBoxW, RegisterClassW, SendMessageW, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow, TranslateMessage, CREATESTRUCTW,
    CW_USEDEFAULT, ES_AUTOHSCROLL, GWLP_USERDATA, HMENU, HWND_NOTOPMOST, HWND_TOPMOST,
    MB_ICONINFORMATION, MB_OK, MSG, SM_CXSCREEN, SM_CYSCREEN, SWP_NOMOVE, SWP_NOSIZE,
    SWP_SHOWWINDOW, SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_CREATE,
    WM_CTLCOLORBTN, WM_CTLCOLOREDIT, WM_CTLCOLORSTATIC, WM_DESTROY, WM_NCCREATE, WM_SETFONT,
    WNDCLASSW, WS_CAPTION, WS_CHILD, WS_EX_CLIENTEDGE, WS_EX_TOOLWINDOW, WS_OVERLAPPED, WS_SYSMENU,
    WS_TABSTOP, WS_VISIBLE,
};

const ID_KEY: isize = 101;
const ID_ACTIVATE: usize = 102;
const ID_BUY: usize = 103;
const ID_CANCEL: usize = 104;

struct UiState {
    edit: HWND,
    status: HWND,
    activated: bool,
    background: HBRUSH,
    edit_background: HBRUSH,
    theme: crate::theme::Theme,
}

impl UiState {
    fn new() -> Self {
        let theme = crate::theme::current();
        let background = unsafe { CreateSolidBrush(theme.bg) };
        let edit_background = unsafe { CreateSolidBrush(theme.chip) };
        Self {
            edit: HWND::default(),
            status: HWND::default(),
            activated: false,
            background,
            edit_background,
            theme,
        }
    }
}

impl Drop for UiState {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.background);
            let _ = DeleteObject(self.edit_background);
        }
    }
}

unsafe fn state(hwnd: HWND) -> Option<&'static mut UiState> {
    (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut UiState).as_mut()
}

unsafe fn child(
    class: PCWSTR,
    text: &str,
    style: windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE,
    ex_style: WINDOW_EX_STYLE,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    parent: HWND,
    id: isize,
) -> Result<HWND> {
    let text = HSTRING::from(text);
    let handle = CreateWindowExW(
        ex_style,
        class,
        &text,
        style | WS_CHILD | WS_VISIBLE,
        x,
        y,
        width,
        height,
        parent,
        HMENU(id as *mut c_void),
        GetModuleHandleW(None)?,
        None,
    )?;
    let font = GetStockObject(DEFAULT_GUI_FONT);
    let _ = SendMessageW(handle, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    Ok(handle)
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
            let mut client = RECT::default();
            let _ = GetClientRect(hwnd, &mut client);
            let width = client.right - client.left;
            let _ = child(
                w!("STATIC"),
                "Enter the license key from your Lemon Squeezy receipt.",
                Default::default(),
                Default::default(),
                28,
                24,
                width - 56,
                24,
                hwnd,
                0,
            );
            state.edit = child(
                w!("EDIT"),
                "",
                WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                WS_EX_CLIENTEDGE,
                28,
                58,
                width - 56,
                30,
                hwnd,
                ID_KEY,
            )
            .unwrap_or_default();
            state.status = child(
                w!("STATIC"),
                "One license covers you on up to three Windows PCs.",
                Default::default(),
                Default::default(),
                28,
                98,
                width - 56,
                42,
                hwnd,
                0,
            )
            .unwrap_or_default();
            let _ = child(
                w!("BUTTON"),
                "Activate",
                WS_TABSTOP,
                Default::default(),
                width - 334,
                154,
                92,
                34,
                hwnd,
                ID_ACTIVATE as isize,
            );
            let _ = child(
                w!("BUTTON"),
                "Buy Matteshot",
                WS_TABSTOP,
                Default::default(),
                width - 234,
                154,
                112,
                34,
                hwnd,
                ID_BUY as isize,
            );
            let _ = child(
                w!("BUTTON"),
                "Cancel",
                WS_TABSTOP,
                Default::default(),
                width - 114,
                154,
                86,
                34,
                hwnd,
                ID_CANCEL as isize,
            );
            let _ = SetFocus(state.edit);
            LRESULT(0)
        }
        WM_COMMAND => {
            let command = wparam.0 & 0xffff;
            match command {
                ID_ACTIVATE => {
                    let Some(state) = state(hwnd) else {
                        return LRESULT(0);
                    };
                    let length = GetWindowTextLengthW(state.edit).max(0) as usize;
                    let mut buffer = vec![0u16; length + 1];
                    let copied = GetWindowTextW(state.edit, &mut buffer);
                    let key = String::from_utf16_lossy(&buffer[..copied.max(0) as usize]);
                    let _ = SetWindowTextW(state.status, w!("Activating\u{2026}"));
                    let _ = UpdateWindow(hwnd);
                    match crate::license::activate(&key) {
                        Ok(_) => {
                            state.activated = true;
                            let _ = MessageBoxW(
                                hwnd,
                                w!("Matteshot is activated on this PC."),
                                w!("Matteshot"),
                                MB_OK | MB_ICONINFORMATION,
                            );
                            let _ = DestroyWindow(hwnd);
                        }
                        Err(error) => {
                            let message = HSTRING::from(format!("{error:#}"));
                            let _ = SetWindowTextW(state.status, &message);
                        }
                    }
                    LRESULT(0)
                }
                ID_BUY => {
                    crate::output::open_url(crate::license::BUY_URL);
                    LRESULT(0)
                }
                ID_CANCEL => {
                    let _ = DestroyWindow(hwnd);
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, message, wparam, lparam),
            }
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
            let Some(state) = state(hwnd) else {
                return DefWindowProcW(hwnd, message, wparam, lparam);
            };
            let hdc = windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void);
            let _ = SetTextColor(hdc, state.theme.text);
            let _ = SetBkColor(hdc, state.theme.bg);
            LRESULT(state.background.0 as isize)
        }
        WM_CTLCOLOREDIT => {
            let Some(state) = state(hwnd) else {
                return DefWindowProcW(hwnd, message, wparam, lparam);
            };
            let hdc = windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void);
            let _ = SetTextColor(hdc, state.theme.text);
            let _ = SetBkColor(hdc, state.theme.chip);
            LRESULT(state.edit_background.0 as isize)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => LRESULT(0),
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

/// Open a modal activation surface on the current UI thread. Returns true
/// when activation succeeded.
pub fn open() -> Result<bool> {
    unsafe {
        let instance = GetModuleHandleW(None).context("get app module for activation")?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hIcon: crate::tray::app_icon(),
            hbrBackground: HBRUSH::default(),
            lpszClassName: w!("matteshot_activation"),
            ..Default::default()
        };
        RegisterClassW(&class);

        let mut ui_state = Box::new(UiState::new());
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU;
        let ex_style = WS_EX_TOOLWINDOW;
        let mut bounds = RECT {
            left: 0,
            top: 0,
            right: 520,
            bottom: 220,
        };
        AdjustWindowRectEx(&mut bounds, style, false, ex_style)
            .context("size activation window")?;
        let width = bounds.right - bounds.left;
        let height = bounds.bottom - bounds.top;
        let x = ((GetSystemMetrics(SM_CXSCREEN) - width) / 2).max(0);
        let y = ((GetSystemMetrics(SM_CYSCREEN) - height) / 2).max(0);
        let hwnd = CreateWindowExW(
            ex_style,
            w!("matteshot_activation"),
            w!("Activate Matteshot"),
            style,
            if x == 0 { CW_USEDEFAULT } else { x },
            if y == 0 { CW_USEDEFAULT } else { y },
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
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        Ok(ui_state.activated)
    }
}
