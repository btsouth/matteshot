//! System tray presence: runtime-generated icon, left-click captures,
//! right-click menu. The tray window shares the main thread's message loop;
//! menu picks surface as `Action`s the main loop polls after dispatch.

use anyhow::Result;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, GetDC, ReleaseDC, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyMenu, GetCursorPos, GetWindowLongPtrW, KillTimer, PostQuitMessage, RegisterClassW,
    SetForegroundWindow, SetTimer, SetWindowLongPtrW, TrackPopupMenu, CREATESTRUCTW, GWLP_USERDATA,
    HICON, ICONINFO, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING, TPM_BOTTOMALIGN,
    TPM_NONOTIFY, TPM_RETURNCMD, WM_CLOSE, WM_LBUTTONUP, WM_NCCREATE, WM_RBUTTONUP, WM_TIMER,
    WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
};
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
use winreg::RegKey;

const WM_TRAYICON: u32 = 0x8001; // WM_APP + 1
pub(crate) const WM_UPDATE_AVAILABLE: u32 = 0x8002; // WM_APP + 2
const WM_TRAY_ACTION: u32 = 0x8003; // defer until the native popup is fully dismissed
const CMD_CAPTURE: usize = 101;
const CMD_CAPTURE_ACTIVE: usize = 102;
const CMD_OPEN_FOLDER: usize = 103;
const CMD_AUTOSTART: usize = 104;
const CMD_PRTSCN: usize = 105;
const CMD_QUIT: usize = 106;
const CMD_SETTINGS: usize = 107;
const CMD_UPDATE: usize = 108;
const CMD_BUY: usize = 109;
const CMD_ACTIVATE: usize = 110;
const CMD_DEACTIVATE: usize = 111;
const CMD_DIAGNOSTICS: usize = 112;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "Matteshot";

#[derive(Clone, Copy, PartialEq)]
pub enum Action {
    Capture,
    CaptureActive,
    OpenFolder,
    Settings,
    ToggleAutostart,
    TogglePrtscn,
    OpenUpdate,
    Buy,
    Activate,
    Deactivate,
    Diagnostics,
    Quit,
}

struct TrayState {
    pending: Option<Action>,
    update: Option<crate::update::AvailableUpdate>,
    active_window: Option<HWND>,
}

pub struct Tray {
    pub hwnd: HWND,
    state: Box<TrayState>,
    _icon: HICON,
}

pub fn autostart_enabled() -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY, KEY_READ)
        .and_then(|k| k.get_value::<String, _>(RUN_VALUE))
        .is_ok()
}

pub fn set_autostart(enabled: bool) -> Result<()> {
    let key = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)?;
    if enabled {
        let exe = std::env::current_exe()?;
        key.set_value(RUN_VALUE, &format!("\"{}\"", exe.display()))?;
    } else {
        let _ = key.delete_value(RUN_VALUE);
    }
    Ok(())
}

/// The embedded app icon (assets\matteshot.ico via build.rs), falling back to
/// the runtime-drawn one if the resource is somehow missing. Also used as the
/// window icon for taskbar-visible windows.
#[allow(clippy::manual_dangling_ptr)] // Win32 MAKEINTRESOURCE: resource ID encoded as a pointer.
pub(crate) unsafe fn app_icon() -> HICON {
    use windows::Win32::UI::WindowsAndMessaging::{LoadImageW, IMAGE_ICON, LR_DEFAULTSIZE, LR_SHARED};
    if let Ok(hinstance) = GetModuleHandleW(None) {
        if let Ok(h) = LoadImageW(
            hinstance,
            windows::core::PCWSTR(1 as *const u16),
            IMAGE_ICON,
            0,
            0,
            LR_DEFAULTSIZE | LR_SHARED,
        ) {
            if !h.is_invalid() {
                return HICON(h.0);
            }
        }
    }
    make_icon()
}

unsafe fn make_icon() -> HICON {
    const S: i32 = 32;
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: S,
            biHeight: -S,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let screen = GetDC(None);
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let color = CreateDIBSection(screen, &info, DIB_RGB_COLORS, &mut bits, None, 0)
        .expect("icon dib");
    ReleaseDC(None, screen);
    let px = std::slice::from_raw_parts_mut(bits as *mut u8, (S * S * 4) as usize);

    for y in 0..S {
        for x in 0..S {
            // Rounded-square coverage.
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let r = 8.0f32;
            let (cx, cy) = (16.0, 16.0);
            let (qx, qy) = ((fx - cx).abs() - (14.0 - r), (fy - cy).abs() - (14.0 - r));
            let dist = if qx > 0.0 && qy > 0.0 {
                (qx * qx + qy * qy).sqrt() - r
            } else {
                qx.max(qy) - r
            };
            let cov = (0.5 - dist).clamp(0.0, 1.0);

            // Diagonal indigo -> teal gradient.
            let t = (fx + fy) / 64.0;
            let (mut rr, mut gg, mut bb) = (
                0.36 + (0.13 - 0.36) * t,
                0.32 + (0.72 - 0.32) * t,
                0.92 + (0.78 - 0.92) * t,
            );

            // Lens dot.
            let d = ((fx - 16.0).powi(2) + (fy - 16.0).powi(2)).sqrt();
            let ring = ((0.5 - (d - 6.0).abs() + 1.4).clamp(0.0, 1.0)) * 0.9;
            rr += (1.0 - rr) * ring;
            gg += (1.0 - gg) * ring;
            bb += (1.0 - bb) * ring;

            let a = cov;
            let i = ((y * S + x) * 4) as usize;
            // Premultiplied BGRA.
            px[i] = (bb * a * 255.0) as u8;
            px[i + 1] = (gg * a * 255.0) as u8;
            px[i + 2] = (rr * a * 255.0) as u8;
            px[i + 3] = (a * 255.0) as u8;
        }
    }

    let mask = CreateBitmap(S, S, 1, 1, None);
    let icon_info = ICONINFO {
        fIcon: true.into(),
        hbmColor: color,
        hbmMask: mask,
        ..Default::default()
    };
    let icon = CreateIconIndirect(&icon_info).expect("icon");
    let _ = DeleteObject(color);
    let _ = DeleteObject(mask);
    icon
}

unsafe fn show_menu(hwnd: HWND, state: &mut TrayState) {
    // Keep a real app foreground if one is still available. If clicking the
    // tray already foregrounded the taskbar, retain the last timer snapshot.
    if let Some(active) = crate::window::external_foreground() {
        state.active_window = Some(active);
    }

    let menu = CreatePopupMenu().expect("menu");
    let check = |on: bool| if on { MF_CHECKED } else { Default::default() };
    let license = crate::license::status();
    let capture_flags = if license.can_capture() {
        MF_STRING
    } else {
        MF_STRING | MF_GRAYED
    };
    let _ = AppendMenuW(menu, capture_flags, CMD_CAPTURE, w!("Capture\tPrtScn"));
    let _ = AppendMenuW(
        menu,
        capture_flags,
        CMD_CAPTURE_ACTIVE,
        w!("Capture active window\tCtrl+Alt+S"),
    );
    let _ = AppendMenuW(menu, MF_STRING, CMD_OPEN_FOLDER, w!("Open captures folder"));
    if let Some(update) = &state.update {
        let label: Vec<u16> = format!("Update available: v{}\u{2026}", update.version)
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let _ = AppendMenuW(menu, MF_STRING, CMD_UPDATE, PCWSTR(label.as_ptr()));
    }
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    let license_label: Vec<u16> = license
        .tray_label()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let _ = AppendMenuW(
        menu,
        MF_STRING | MF_GRAYED,
        0,
        PCWSTR(license_label.as_ptr()),
    );
    match license {
        crate::license::Status::Licensed { .. } => {
            let _ = AppendMenuW(
                menu,
                MF_STRING,
                CMD_DEACTIVATE,
                w!("Deactivate this PC\u{2026}"),
            );
        }
        _ => {
            let _ = AppendMenuW(menu, MF_STRING, CMD_BUY, w!("Buy Matteshot\u{2026}"));
            let _ = AppendMenuW(
                menu,
                MF_STRING,
                CMD_ACTIVATE,
                w!("Enter license key\u{2026}"),
            );
        }
    }
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    let _ = AppendMenuW(menu, MF_STRING, CMD_SETTINGS, w!("Settings\u{2026}"));
    let _ = AppendMenuW(
        menu,
        MF_STRING | check(autostart_enabled()),
        CMD_AUTOSTART,
        w!("Start with Windows"),
    );
    let _ = AppendMenuW(
        menu,
        capture_flags,
        CMD_PRTSCN,
        if crate::prtscn::preferred() {
            w!("Give PrtScn back to Snipping Tool")
        } else {
            w!("Take over PrtScn")
        },
    );
    let _ = AppendMenuW(menu, MF_STRING, CMD_DIAGNOSTICS, w!("Copy diagnostics"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    let _ = AppendMenuW(menu, MF_STRING, CMD_QUIT, w!("Quit Matteshot"));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // Required so the menu dismisses when clicking elsewhere.
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_NONOTIFY | TPM_BOTTOMALIGN,
        pt.x,
        pt.y,
        0,
        hwnd,
        None,
    );
    eprintln!("tray: command {}", cmd.0);
    let _ = DestroyMenu(menu);
    // Documented TrackPopupMenu quirk: without this, the next click on the
    // tray icon can be swallowed by leftover menu state.
    let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
        hwnd,
        windows::Win32::UI::WindowsAndMessaging::WM_NULL,
        WPARAM(0),
        LPARAM(0),
    );

    let action = match cmd.0 as usize {
        CMD_CAPTURE => Some(Action::Capture),
        CMD_CAPTURE_ACTIVE => Some(Action::CaptureActive),
        CMD_OPEN_FOLDER => Some(Action::OpenFolder),
        CMD_SETTINGS => Some(Action::Settings),
        CMD_AUTOSTART => Some(Action::ToggleAutostart),
        CMD_PRTSCN => Some(Action::TogglePrtscn),
        CMD_UPDATE => Some(Action::OpenUpdate),
        CMD_BUY => Some(Action::Buy),
        CMD_ACTIVATE => Some(Action::Activate),
        CMD_DEACTIVATE => Some(Action::Deactivate),
        CMD_DIAGNOSTICS => Some(Action::Diagnostics),
        CMD_QUIT => Some(Action::Quit),
        _ => None,
    };
    if let Some(action) = action {
        let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            hwnd,
            WM_TRAY_ACTION,
            WPARAM(action as usize),
            LPARAM(0),
        );
    }
}

pub fn request_existing_settings() -> bool {
    let Some(hwnd) = crate::window::find_by_class("matteshot_tray") else {
        return false;
    };
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            hwnd,
            WM_TRAY_ACTION,
            WPARAM(Action::Settings as usize),
            LPARAM(0),
        )
        .is_ok()
    }
}

unsafe fn notify(hwnd: HWND, title: &str, text: &str) {
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_INFO,
        ..Default::default()
    };
    let t: Vec<u16> = title.encode_utf16().collect();
    let x: Vec<u16> = text.encode_utf16().collect();
    data.szInfoTitle[..t.len().min(63)].copy_from_slice(&t[..t.len().min(63)]);
    data.szInfo[..x.len().min(255)].copy_from_slice(&x[..x.len().min(255)]);
    let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_TRAYICON => {
            let state = (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayState).as_mut();
            if let Some(state) = state {
                match lparam.0 as u32 {
                    WM_LBUTTONUP => state.pending = Some(Action::Capture),
                    WM_RBUTTONUP => show_menu(hwnd, state),
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_UPDATE_AVAILABLE => {
            let update = Box::from_raw(lparam.0 as *mut crate::update::AvailableUpdate);
            let state = (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayState).as_mut();
            if let Some(state) = state {
                let changed = state.update.as_ref().map(|u| &u.version) != Some(&update.version);
                state.update = Some(*update);
                if changed {
                    let version = &state.update.as_ref().unwrap().version;
                    notify(
                        hwnd,
                        "Matteshot update available",
                        &format!(
                            "Version {version} is ready. Right-click Matteshot to download."
                        ),
                    );
                }
            }
            LRESULT(0)
        }
        WM_TRAY_ACTION => {
            let state = (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayState).as_mut();
            if let Some(state) = state {
                state.pending = match wparam.0 {
                    x if x == Action::Capture as usize => Some(Action::Capture),
                    x if x == Action::CaptureActive as usize => Some(Action::CaptureActive),
                    x if x == Action::OpenFolder as usize => Some(Action::OpenFolder),
                    x if x == Action::Settings as usize => Some(Action::Settings),
                    x if x == Action::ToggleAutostart as usize => Some(Action::ToggleAutostart),
                    x if x == Action::TogglePrtscn as usize => Some(Action::TogglePrtscn),
                    x if x == Action::OpenUpdate as usize => Some(Action::OpenUpdate),
                    x if x == Action::Buy as usize => Some(Action::Buy),
                    x if x == Action::Activate as usize => Some(Action::Activate),
                    x if x == Action::Deactivate as usize => Some(Action::Deactivate),
                    x if x == Action::Diagnostics as usize => Some(Action::Diagnostics),
                    x if x == Action::Quit as usize => Some(Action::Quit),
                    _ => None,
                };
            }
            LRESULT(0)
        }
        WM_TIMER => {
            let state = (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayState).as_mut();
            if let (Some(state), Some(active)) = (state, crate::window::external_foreground()) {
                state.active_window = Some(active);
            }
            LRESULT(0)
        }
        // Used by the installer and uninstaller. This runs on the resident's
        // own UI thread, allowing the message loop to unwind normally instead
        // of terminating the process during an active file operation.
        WM_CLOSE => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

impl Tray {
    pub fn create() -> Result<Tray> {
        unsafe {
            let hinstance = GetModuleHandleW(None)?;
            let class = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinstance.into(),
                lpszClassName: w!("matteshot_tray"),
                ..Default::default()
            };
            RegisterClassW(&class);

            let mut state = Box::new(TrayState {
                pending: None,
                update: None,
                active_window: crate::window::external_foreground(),
            });
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("matteshot_tray"),
                w!("Matteshot"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                hinstance,
                Some(&mut *state as *mut TrayState as *const _),
            )?;

            let icon = make_icon();
            let mut data = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
                uCallbackMessage: WM_TRAYICON,
                hIcon: icon,
                ..Default::default()
            };
            let tip: Vec<u16> = "Matteshot \u{2014} PrtScn to capture".encode_utf16().collect();
            data.szTip[..tip.len()].copy_from_slice(&tip);
            let _ = Shell_NotifyIconW(NIM_ADD, &data);
            let _ = SetTimer(hwnd, 1, 250, None);

            Ok(Tray { hwnd, state, _icon: icon })
        }
    }

    /// One-shot balloon notification (first run).
    pub fn notify(&self, title: &str, text: &str) {
        unsafe {
            notify(self.hwnd, title, text);
        }
    }

    pub fn update_url(&self) -> Option<String> {
        self.state.update.as_ref().map(|u| u.download_url.clone())
    }

    pub fn active_window(&self) -> Option<HWND> {
        self.state
            .active_window
            .filter(|hwnd| crate::window::is_external(*hwnd))
    }

    /// Poll and clear the pending menu action.
    pub fn take_action(&mut self) -> Option<Action> {
        self.state.pending.take()
    }

    pub fn remove(&self) {
        unsafe {
            let _ = KillTimer(self.hwnd, 1);
            let data = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: self.hwnd,
                uID: 1,
                ..Default::default()
            };
            let _ = Shell_NotifyIconW(NIM_DELETE, &data);
        }
    }

    pub fn quit() {
        unsafe { PostQuitMessage(0) };
    }
}
