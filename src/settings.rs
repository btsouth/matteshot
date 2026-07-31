//! Settings window — custom-drawn dark UI matching the picker/overlay,
//! non-modal, single instance, running on the main thread's message loop.

use std::sync::atomic::{AtomicIsize, Ordering};

use anyhow::{Context, Result};
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePen,
    CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, InvalidateRect,
    RoundRect, SelectObject, SetBkMode, SetTextColor, CLEARTYPE_QUALITY, DEFAULT_CHARSET,
    DT_END_ELLIPSIS, DT_LEFT, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, HDC, HFONT,
    PS_SOLID, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileOpenDialog, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindowLongPtrW, IsWindow, LoadCursorW,
    RegisterClassW, SetForegroundWindow, SetWindowLongPtrW, ShowWindow, CREATESTRUCTW, CS_HREDRAW,
    CS_VREDRAW, GWLP_USERDATA, IDC_ARROW, SW_RESTORE, SW_SHOWNORMAL, WM_CLOSE, WM_ERASEBKGND,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WNDCLASSW, WS_CAPTION,
    WS_SYSMENU, WS_VISIBLE,
};

use crate::config::Config;
use crate::{prtscn, tray, HOTKEY_ID_PRTSCN};



static WINDOW: AtomicIsize = AtomicIsize::new(0);

#[derive(Clone, Copy, PartialEq)]
enum Ctrl {
    ChangeDir,
    OpenDir,
    ChangeVideoDir,
    OpenVideoDir,
    Scale(u32),
    Autostart,
    Prtscn,
    RecordGif,
    Audio(&'static str),
}

struct State {
    cfg: Config,
    license: crate::license::Status,
    font: HFONT,
    font_small: HFONT,
    controls: Vec<(RECT, Ctrl)>,
    hover: i32,
    scale: f32,
    width: i32,
    height: i32,
    theme: crate::theme::Theme,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

unsafe fn make_font(height: i32) -> HFONT {
    CreateFontW(
        height,
        0,
        0,
        0,
        400,
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

unsafe fn state_of(hwnd: HWND) -> Option<&'static mut State> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
    ptr.as_mut()
}

fn s(state: &State, v: i32) -> i32 {
    (v as f32 * state.scale) as i32
}

unsafe fn draw_text_in(hdc: HDC, font: HFONT, color: COLORREF, r: RECT, text: &str, flags: u32) {
    SelectObject(hdc, font);
    SetTextColor(hdc, color);
    let mut t = wide(text);
    let mut rc = r;
    DrawTextW(
        hdc,
        &mut t,
        &mut rc,
        windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT(flags) | DT_SINGLELINE | DT_VCENTER,
    );
}

unsafe fn draw_chip_button(hdc: HDC, r: RECT, label: &str, state: &State, active: bool, hot: bool) {
    let fill = CreateSolidBrush(if active { state.theme.accent } else { state.theme.chip });
    let pen = CreatePen(PS_SOLID, 1, if active { state.theme.accent } else { state.theme.chip_line });
    let ob = SelectObject(hdc, fill);
    let op = SelectObject(hdc, pen);
    let _ = RoundRect(hdc, r.left, r.top, r.right, r.bottom, s(state, 10), s(state, 10));
    SelectObject(hdc, ob);
    SelectObject(hdc, op);
    let _ = DeleteObject(fill);
    let _ = DeleteObject(pen);
    let color = if active {
        state.theme.accent_text
    } else if hot {
        state.theme.text
    } else {
        state.theme.muted
    };
    SelectObject(hdc, state.font);
    SetTextColor(hdc, color);
    let mut t = wide(label);
    let mut rc = r;
    DrawTextW(
        hdc,
        &mut t,
        &mut rc,
        windows::Win32::Graphics::Gdi::DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );
}

unsafe fn draw_checkbox(hdc: HDC, r: RECT, label: &str, state: &State, checked: bool, hot: bool) {
    let box_r = RECT {
        left: r.left,
        top: r.top + (r.bottom - r.top - s(state, 18)) / 2,
        right: r.left + s(state, 18),
        bottom: r.top + (r.bottom - r.top - s(state, 18)) / 2 + s(state, 18),
    };
    let fill = CreateSolidBrush(if checked { state.theme.accent } else { state.theme.chip });
    let pen = CreatePen(PS_SOLID, 1, if checked { state.theme.accent } else { state.theme.chip_line });
    let ob = SelectObject(hdc, fill);
    let op = SelectObject(hdc, pen);
    let _ = RoundRect(hdc, box_r.left, box_r.top, box_r.right, box_r.bottom, s(state, 6), s(state, 6));
    SelectObject(hdc, ob);
    SelectObject(hdc, op);
    let _ = DeleteObject(fill);
    let _ = DeleteObject(pen);
    if checked {
        draw_text_in(hdc, state.font_small, state.theme.accent_text, box_r, "\u{2713}", 1 /*DT_CENTER*/);
    }
    let label_r = RECT { left: box_r.right + s(state, 10), ..r };
    draw_text_in(
        hdc,
        state.font,
        if hot { state.theme.accent } else { state.theme.text },
        label_r,
        label,
        0,
    );
}

unsafe fn paint(hdc: HDC, state: &State) {
    let bg = CreateSolidBrush(state.theme.bg);
    FillRect(hdc, &RECT { left: 0, top: 0, right: state.width, bottom: state.height }, bg);
    let _ = DeleteObject(bg);
    SetBkMode(hdc, TRANSPARENT);

    let m = s(state, 24);
    let mut y = s(state, 20);

    // Save folder
    draw_text_in(
        hdc,
        state.font_small,
        state.theme.muted,
        RECT { left: m, top: y, right: state.width - m, bottom: y + s(state, 20) },
        "SAVE FOLDER",
        0,
    );
    y += s(state, 24);
    let path = state.cfg.save_dir().display().to_string();
    let path_r = RECT {
        left: m,
        top: y,
        right: state.width - m - s(state, 150),
        bottom: y + s(state, 30),
    };
    SelectObject(hdc, state.font);
    SetTextColor(hdc, state.theme.text);
    let mut t = wide(&path);
    let mut rc = path_r;
    DrawTextW(hdc, &mut t, &mut rc, DT_LEFT | DT_END_ELLIPSIS | DT_SINGLELINE | DT_VCENTER);

    // Video folder — recordings live apart from screenshots.
    draw_text_in(
        hdc,
        state.font_small,
        state.theme.muted,
        RECT { left: m, top: s(state, 82), right: state.width - m, bottom: s(state, 102) },
        "VIDEO FOLDER  (recordings)",
        0,
    );
    let vpath = state.cfg.video_dir().display().to_string();
    SelectObject(hdc, state.font);
    SetTextColor(hdc, state.theme.text);
    let mut vt = wide(&vpath);
    let mut vrc = RECT {
        left: m,
        top: s(state, 106),
        right: state.width - m - s(state, 150),
        bottom: s(state, 136),
    };
    DrawTextW(hdc, &mut vt, &mut vrc, DT_LEFT | DT_END_ELLIPSIS | DT_SINGLELINE | DT_VCENTER);

    for (r, c) in &state.controls {
        let hot = state
            .controls
            .iter()
            .position(|(rr, _)| rr == r)
            .map(|i| i as i32 == state.hover)
            .unwrap_or(false);
        match c {
            Ctrl::ChangeDir | Ctrl::ChangeVideoDir => {
                draw_chip_button(hdc, *r, "Change\u{2026}", state, false, hot)
            }
            Ctrl::OpenDir | Ctrl::OpenVideoDir => {
                draw_chip_button(hdc, *r, "Open", state, false, hot)
            }
            Ctrl::Scale(n) => draw_chip_button(
                hdc,
                *r,
                &format!("{n}x"),
                state,
                state.cfg.export_scale == *n,
                hot,
            ),
            Ctrl::Autostart => {
                draw_checkbox(hdc, *r, "Start with Windows", state, tray::autostart_enabled(), hot)
            }
            Ctrl::Prtscn => {
                draw_checkbox(
                    hdc,
                    *r,
                    "Capture the PrtScn key",
                    state,
                    state.cfg.capture_prtscn,
                    hot,
                );
                let (label, color) = if !state.cfg.capture_prtscn {
                    ("Off", state.theme.muted)
                } else if !state.license.can_capture() {
                    ("Activate to use", state.theme.muted)
                } else if prtscn::owns_key() {
                    ("Active", state.theme.accent)
                } else {
                    ("Reconnecting\u{2026}", state.theme.muted)
                };
                draw_text_in(
                    hdc,
                    state.font_small,
                    color,
                    RECT {
                        left: r.right - s(state, 130),
                        top: r.top,
                        right: r.right,
                        bottom: r.bottom,
                    },
                    label,
                    DT_RIGHT.0,
                );
            }
            Ctrl::RecordGif => draw_checkbox(
                hdc,
                *r,
                "Also save a GIF when recording",
                state,
                state.cfg.record_gif,
                hot,
            ),
            Ctrl::Audio(mode) => draw_chip_button(
                hdc,
                *r,
                match *mode {
                    "system" => "System",
                    "mic" => "Mic",
                    _ => "Off",
                },
                state,
                state.cfg.record_audio == *mode,
                hot,
            ),
        }
    }

    // Section label for quality
    y = s(state, 152);
    draw_text_in(
        hdc,
        state.font_small,
        state.theme.muted,
        RECT { left: m, top: y, right: state.width - m, bottom: y + s(state, 20) },
        "EXPORT QUALITY  (2x recommended for sharing)",
        0,
    );

    // Recording audio label.
    draw_text_in(
        hdc,
        state.font,
        state.theme.text,
        RECT {
            left: m,
            top: s(state, 324),
            right: m + s(state, 145),
            bottom: s(state, 352),
        },
        "Recording audio",
        0,
    );

    // Footer
    let footer = RECT {
        left: m,
        top: state.height - s(state, 34),
        right: state.width - m,
        bottom: state.height - s(state, 10),
    };
    let footer_text = format!(
        "Matteshot {}   \u{00b7}   {}",
        env!("CARGO_PKG_VERSION"),
        state.license.tray_label()
    );
    draw_text_in(
        hdc,
        state.font_small,
        state.theme.muted,
        footer,
        &footer_text,
        0,
    );
}

unsafe fn pick_folder(hwnd: HWND) -> Option<String> {
    let dialog: IFileOpenDialog =
        CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
    let opts = dialog.GetOptions().ok()?;
    dialog.SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
    dialog.Show(hwnd).ok()?;
    let item = dialog.GetResult().ok()?;
    let pw = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
    let path = pw.to_string().ok();
    CoTaskMemFree(Some(pw.0 as *const _));
    path
}

unsafe fn activate(hwnd: HWND, state: &mut State, ctrl: Ctrl) {
    match ctrl {
        Ctrl::ChangeDir => {
            if let Some(path) = pick_folder(hwnd) {
                state.cfg = Config::update(|cfg| cfg.save_dir = Some(path.into()));
            }
        }
        Ctrl::OpenDir => crate::output::open_folder(&state.cfg.save_dir()),
        Ctrl::ChangeVideoDir => {
            if let Some(path) = pick_folder(hwnd) {
                state.cfg = Config::update(|cfg| cfg.video_dir = Some(path.into()));
            }
        }
        Ctrl::OpenVideoDir => crate::output::open_folder(&state.cfg.video_dir()),
        Ctrl::Scale(n) => {
            state.cfg = Config::update(|cfg| cfg.export_scale = n);
        }
        Ctrl::Autostart => {
            let _ = tray::set_autostart(!tray::autostart_enabled());
        }
        Ctrl::Prtscn => {
            let enabled = !state.cfg.capture_prtscn;
            state.cfg = Config::update(|cfg| cfg.capture_prtscn = enabled);
            prtscn::set_preferred(state.cfg.capture_prtscn);
            if state.cfg.capture_prtscn {
                let _ = prtscn::take(HOTKEY_ID_PRTSCN);
            } else {
                prtscn::release(HOTKEY_ID_PRTSCN);
            }
        }
        Ctrl::RecordGif => {
            let enabled = !state.cfg.record_gif;
            state.cfg = Config::update(|cfg| cfg.record_gif = enabled);
        }
        Ctrl::Audio(mode) => {
            state.cfg = Config::update(|cfg| cfg.record_audio = mode.to_string());
        }
    }
    let _ = InvalidateRect(hwnd, None, false);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            if let Some(state) = state_of(hwnd) {
                let mut ps = windows::Win32::Graphics::Gdi::PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                let mem = CreateCompatibleDC(hdc);
                let bmp = CreateCompatibleBitmap(hdc, state.width, state.height);
                let old = SelectObject(mem, bmp);
                paint(mem, state);
                let _ = BitBlt(hdc, 0, 0, state.width, state.height, mem, 0, 0, SRCCOPY);
                SelectObject(mem, old);
                let _ = DeleteObject(bmp);
                let _ = DeleteDC(mem);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if let Some(state) = state_of(hwnd) {
                let (x, y) = (
                    (lparam.0 & 0xFFFF) as i16 as i32,
                    ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                );
                let hover = state
                    .controls
                    .iter()
                    .position(|(r, _)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
                    .map(|i| i as i32)
                    .unwrap_or(-1);
                if hover != state.hover {
                    state.hover = hover;
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if let Some(state) = state_of(hwnd) {
                let (x, y) = (
                    (lparam.0 & 0xFFFF) as i16 as i32,
                    ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                );
                if let Some(i) = state
                    .controls
                    .iter()
                    .position(|(r, _)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
                {
                    let ctrl = state.controls[i].1;
                    activate(hwnd, state, ctrl);
                }
            }
            LRESULT(0)
        }
        // The settings window is persistent: follow live theme flips.
        windows::Win32::UI::WindowsAndMessaging::WM_SETTINGCHANGE => {
            if let Some(state) = state_of(hwnd) {
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
                let state = Box::from_raw(ptr);
                let _ = DeleteObject(state.font);
                let _ = DeleteObject(state.font_small);
            }
            WINDOW.store(0, Ordering::SeqCst);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Open (or focus) the settings window. Non-modal; shares the main loop.
pub fn open() -> Result<()> {
    unsafe {
        let existing = HWND(WINDOW.load(Ordering::SeqCst) as *mut _);
        if !existing.0.is_null() && IsWindow(existing).as_bool() {
            use windows::Win32::UI::WindowsAndMessaging::{
                SetWindowPos, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOMOVE, SWP_NOSIZE,
            };
            let _ = ShowWindow(existing, SW_RESTORE);
            let _ = SetWindowPos(existing, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
            let _ = SetWindowPos(existing, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
            let _ = SetForegroundWindow(existing);
            eprintln!("settings: focused existing window");
            return Ok(());
        }

        let scale = GetDpiForSystem() as f32 / 96.0;
        let sc = |v: i32| (v as f32 * scale) as i32;
        let (cw, ch) = (sc(500), sc(424));

        let font = make_font(-sc(15));
        let font_small = make_font(-sc(12));

        // Static layout.
        let m = sc(24);
        // Save folder row buttons (right-aligned).
        let mut controls = vec![
            (
                RECT { left: cw - m - sc(140), top: sc(42), right: cw - m - sc(64), bottom: sc(72) },
                Ctrl::ChangeDir,
            ),
            (
                RECT { left: cw - m - sc(56), top: sc(42), right: cw - m, bottom: sc(72) },
                Ctrl::OpenDir,
            ),
            (
                RECT { left: cw - m - sc(140), top: sc(106), right: cw - m - sc(64), bottom: sc(136) },
                Ctrl::ChangeVideoDir,
            ),
            (
                RECT { left: cw - m - sc(56), top: sc(106), right: cw - m, bottom: sc(136) },
                Ctrl::OpenVideoDir,
            ),
        ];
        // Export scale segmented.
        for (i, n) in [1u32, 2, 3].iter().enumerate() {
            let x = m + i as i32 * sc(62);
            controls.push((
                RECT { left: x, top: sc(176), right: x + sc(54), bottom: sc(206) },
                Ctrl::Scale(*n),
            ));
        }
        // Checkboxes.
        controls.push((
            RECT { left: m, top: sc(226), right: cw - m, bottom: sc(254) },
            Ctrl::Autostart,
        ));
        controls.push((
            RECT { left: m, top: sc(258), right: cw - m, bottom: sc(286) },
            Ctrl::Prtscn,
        ));
        controls.push((
            RECT { left: m, top: sc(290), right: cw - m, bottom: sc(318) },
            Ctrl::RecordGif,
        ));
        // Recording audio segmented control.
        for (i, mode) in ["off", "system", "mic"].iter().enumerate() {
            let x = m + sc(150) + i as i32 * sc(78);
            controls.push((
                RECT { left: x, top: sc(324), right: x + sc(70), bottom: sc(352) },
                Ctrl::Audio(mode),
            ));
        }

        let state = Box::new(State {
            cfg: Config::load(),
            license: crate::license::status(),
            font,
            font_small,
            controls,
            hover: -1,
            scale,
            width: cw,
            height: ch,
            theme: crate::theme::current(),
        });

        let hinstance = GetModuleHandleW(None).context("get app module for settings")?;
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: crate::tray::app_icon(),
            lpszClassName: w!("matteshot_settings"),
            hbrBackground: Default::default(),
            ..Default::default()
        };
        RegisterClassW(&class);

        let leaked = Box::into_raw(state);
        let mut outer = RECT { left: 0, top: 0, right: cw, bottom: ch };
        let _ = windows::Win32::UI::WindowsAndMessaging::AdjustWindowRectEx(
            &mut outer,
            WS_CAPTION | WS_SYSMENU,
            false,
            windows::Win32::UI::WindowsAndMessaging::WS_EX_APPWINDOW,
        );
        // Center on the monitor the cursor is on (the user just clicked the
        // tray there); a fixed 120,120 lands behind whatever is maximized.
        let (ww, wh) = (outer.right - outer.left, outer.bottom - outer.top);
        let mut pt = windows::Win32::Foundation::POINT::default();
        let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt);
        let mon = windows::Win32::Graphics::Gdi::MonitorFromPoint(
            pt,
            windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTOPRIMARY,
        );
        let mut mi = windows::Win32::Graphics::Gdi::MONITORINFO {
            cbSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = windows::Win32::Graphics::Gdi::GetMonitorInfoW(mon, &mut mi);
        let x = mi.rcWork.left + (mi.rcWork.right - mi.rcWork.left - ww) / 2;
        let y = mi.rcWork.top + (mi.rcWork.bottom - mi.rcWork.top - wh) / 2;

        let hwnd = match CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WS_EX_APPWINDOW,
            w!("matteshot_settings"),
            w!("Matteshot settings"),
            WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            x,
            y,
            ww,
            wh,
            None,
            None,
            hinstance,
            Some(leaked as *const _),
        ) {
            Ok(h) => h,
            Err(error) => {
                drop(Box::from_raw(leaked));
                return Err(error).context("create settings window");
            }
        };

        crate::theme::apply_titlebar(hwnd, &crate::theme::current());
        WINDOW.store(hwnd.0 as isize, Ordering::SeqCst);
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        // After a tray menu closes our process has lost its foreground
        // permission, so SetForegroundWindow alone silently fails and the new
        // window is born BEHIND the active app. The topmost toggle forces
        // z-order without needing activation rights.
        use windows::Win32::UI::WindowsAndMessaging::{
            SetWindowPos, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOMOVE, SWP_NOSIZE,
        };
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
        let _ = SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
        let _ = SetForegroundWindow(hwnd);
        eprintln!("settings: opened");
        Ok(())
    }
}

pub fn refresh() {
    unsafe {
        let hwnd = HWND(WINDOW.load(Ordering::SeqCst) as *mut _);
        if !hwnd.0.is_null() && IsWindow(hwnd).as_bool() {
            if let Some(state) = state_of(hwnd) {
                state.license = crate::license::status();
                state.cfg = Config::load();
            }
            let _ = InvalidateRect(hwnd, None, false);
        }
    }
}

