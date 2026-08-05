//! Settings window — custom-drawn dark UI matching the picker/overlay,
//! non-modal, single instance, running on the main thread's message loop.

use std::sync::atomic::{AtomicIsize, Ordering};

use anyhow::{Context, Result};
use windows::core::{w, HSTRING, PCWSTR};
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
    MessageBoxW, RegisterClassW, SetForegroundWindow, SetWindowLongPtrW, ShowWindow, CREATESTRUCTW,
    CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, IDC_ARROW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK,
    SW_RESTORE, SW_SHOWNORMAL, WM_CLOSE, WM_ERASEBKGND, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCCREATE,
    WM_KEYDOWN, WM_KILLFOCUS, WM_NCDESTROY, WM_PAINT, WM_SYSKEYDOWN, WNDCLASSW,
    WS_CAPTION, WS_SYSMENU, WS_VISIBLE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN, VIRTUAL_KEY,
    VK_CONTROL, VK_ESCAPE, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
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
    OutputSize(u32),
    CustomSize,
    Autostart,
    Prtscn,
    RecordGif,
    AutoUpdate,
    Telemetry,
    KeepEditorOpen,
    Audio(&'static str),
    CaptureHotkey,
    CaptureDelay(u32),
    Diagnostics,
    Deactivate,
}

/// Painted furniture: section headers, the folder paths, and the one inline
/// label. Positioned by the same pass as the controls so the two can no longer
/// disagree.
///
/// The text is resolved at paint time rather than stored here, because
/// `refresh` reloads the config and repaints without rebuilding the layout, so
/// a baked string would go stale the moment a setting changed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Chrome {
    SaveFolderHeader,
    SavePath,
    VideoFolderHeader,
    VideoPath,
    RenderQualityHeader,
    ScreenshotSizeHeader,
    AudioLabel,
    HotkeyLabel,
    DelayLabel,
}

/// Walks down the window handing out rects.
///
/// Every position used to be a literal, in two lists that had to agree by
/// hand: the builder placed controls, and the paint routine separately placed
/// headers at its own hardcoded coordinates. Inserting a row meant editing
/// both and shifting everything below in each. Here a row is one call, what
/// follows moves on its own, and the window height is wherever the cursor
/// stopped.
struct Layout {
    scale: f32,
    margin: i32,
    width: i32,
    y: i32,
    controls: Vec<(RECT, Ctrl)>,
    chrome: Vec<(RECT, Chrome)>,
}

impl Layout {
    fn new(scale: f32, width: i32) -> Layout {
        let margin = (24.0 * scale) as i32;
        Layout {
            scale,
            margin,
            width,
            y: 0,
            controls: Vec::new(),
            chrome: Vec::new(),
        }
    }

    fn sc(&self, v: i32) -> i32 {
        (v as f32 * self.scale) as i32
    }

    fn gap(&mut self, v: i32) {
        self.y += self.sc(v);
    }

    /// Full-width band at the cursor, which the cursor then moves past.
    fn band(&mut self, height: i32) -> RECT {
        let rect = RECT {
            left: self.margin,
            top: self.y,
            right: self.width - self.margin,
            bottom: self.y + self.sc(height),
        };
        self.y = rect.bottom;
        rect
    }

    fn header(&mut self, chrome: Chrome, height: i32) {
        let rect = self.band(height);
        self.chrome.push((rect, chrome));
    }

    /// A folder path with its Change / Open buttons on the same line.
    fn path_row(&mut self, chrome: Chrome, change: Ctrl, open: Ctrl) {
        let rect = self.band(30);
        self.chrome.push((rect, chrome));
        let right = self.width - self.margin;
        self.controls.push((
            RECT { left: right - self.sc(140), top: rect.top, right: right - self.sc(64), bottom: rect.bottom },
            change,
        ));
        self.controls.push((
            RECT { left: right - self.sc(56), top: rect.top, right, bottom: rect.bottom },
            open,
        ));
    }

    /// A run of chips starting at the left margin, or at `indent` when it
    /// shares its line with a label.
    fn chips(&mut self, items: &[Ctrl], width: i32, stride: i32, indent: i32, height: i32) {
        let rect = self.band(height);
        for (i, ctrl) in items.iter().enumerate() {
            let x = self.margin + self.sc(indent) + i as i32 * self.sc(stride);
            self.controls.push((
                RECT { left: x, top: rect.top, right: x + self.sc(width), bottom: rect.bottom },
                *ctrl,
            ));
        }
    }

    /// Furniture on a line the cursor has already passed, for a label that
    /// shares its row with controls.
    ///
    /// `width` stops short of whatever else is on the line. Without it the
    /// label gets the full band, and at a DPI or font where the text runs
    /// wider it would slide under the controls and be overdrawn by them.
    fn chrome_at(&mut self, top: i32, height: i32, width: i32, chrome: Chrome) {
        self.chrome.push((
            RECT {
                left: self.margin,
                top,
                right: self.margin + self.sc(width),
                bottom: top + self.sc(height),
            },
            chrome,
        ));
    }

    fn checkbox(&mut self, ctrl: Ctrl) {
        let rect = self.band(28);
        self.controls.push((rect, ctrl));
    }

    /// Two side-by-side actions at the foot of the window.
    fn button_pair(&mut self, left: Ctrl, right: Ctrl, width: i32, stride: i32) {
        let rect = self.band(30);
        for (i, ctrl) in [left, right].into_iter().enumerate() {
            let x = self.margin + i as i32 * self.sc(stride);
            self.controls.push((
                RECT { left: x, top: rect.top, right: x + self.sc(width), bottom: rect.bottom },
                ctrl,
            ));
        }
    }

    /// Client height: where the cursor stopped, plus room for the footer line.
    fn finish(&self) -> i32 {
        self.y + self.sc(38)
    }
}

struct State {
    cfg: Config,
    license: crate::license::Status,
    font: HFONT,
    font_small: HFONT,
    controls: Vec<(RECT, Ctrl)>,
    chrome: Vec<(RECT, Chrome)>,
    /// True while the shortcut chip is waiting for a key. Purely a window
    /// mode: nothing is written until a usable combo arrives.
    capturing: bool,
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

    // Furniture first, from the same layout pass that placed the controls.
    // These used to be drawn at their own hardcoded coordinates, which is what
    // made inserting a row a two-list edit.
    for (r, chrome) in &state.chrome {
        match chrome {
            Chrome::SaveFolderHeader => {
                draw_text_in(hdc, state.font_small, state.theme.muted, *r, "SAVE FOLDER", 0)
            }
            Chrome::VideoFolderHeader => draw_text_in(
                hdc,
                state.font_small,
                state.theme.muted,
                *r,
                "VIDEO FOLDER  (recordings)",
                0,
            ),
            Chrome::RenderQualityHeader => draw_text_in(
                hdc,
                state.font_small,
                state.theme.muted,
                *r,
                "RENDER QUALITY  (2x recommended for sharing)",
                0,
            ),
            Chrome::ScreenshotSizeHeader => {
                // Resolved here, not at layout time: refresh reloads the
                // config and repaints without rebuilding the layout.
                let summary = match state.cfg.output_max_edge {
                    crate::output::OUTPUT_ORIGINAL => "Original pixels".to_string(),
                    value => format!("{value} px maximum edge"),
                };
                draw_text_in(
                    hdc,
                    state.font_small,
                    state.theme.muted,
                    *r,
                    &format!("SCREENSHOT SIZE  ({summary})"),
                    0,
                );
            }
            Chrome::AudioLabel => {
                draw_text_in(hdc, state.font, state.theme.text, *r, "Recording audio", 0)
            }
            Chrome::HotkeyLabel => {
                draw_text_in(hdc, state.font, state.theme.text, *r, "Capture shortcut", 0)
            }
            Chrome::DelayLabel => {
                draw_text_in(hdc, state.font, state.theme.text, *r, "Capture delay", 0)
            }
            // The paths stop short of the buttons sharing their line.
            Chrome::SavePath | Chrome::VideoPath => {
                let text = if *chrome == Chrome::SavePath {
                    state.cfg.save_dir().display().to_string()
                } else {
                    state.cfg.video_dir().display().to_string()
                };
                SelectObject(hdc, state.font);
                SetTextColor(hdc, state.theme.text);
                let mut wide_text = wide(&text);
                let mut rc = RECT { right: r.right - s(state, 150), ..*r };
                DrawTextW(
                    hdc,
                    &mut wide_text,
                    &mut rc,
                    DT_LEFT | DT_END_ELLIPSIS | DT_SINGLELINE | DT_VCENTER,
                );
            }
        }
    }

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
            Ctrl::OutputSize(max_edge) => draw_chip_button(
                hdc,
                *r,
                &crate::output::output_size_label(*max_edge),
                state,
                state.cfg.output_max_edge == *max_edge,
                hot,
            ),
            Ctrl::CustomSize => {
                let custom = ![
                    crate::output::OUTPUT_ORIGINAL,
                    crate::output::OUTPUT_EMAIL,
                    crate::output::OUTPUT_COMPACT,
                ]
                .contains(&state.cfg.output_max_edge);
                let custom_label = format!("{}px", state.cfg.output_max_edge);
                draw_chip_button(
                    hdc,
                    *r,
                    if custom { &custom_label } else { "Custom" },
                    state,
                    custom,
                    hot,
                )
            }
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
            Ctrl::AutoUpdate => draw_checkbox(
                hdc,
                *r,
                "Install updates automatically",
                state,
                state.cfg.auto_update,
                hot,
            ),
            Ctrl::Telemetry => draw_checkbox(
                hdc,
                *r,
                "Send anonymous usage stats",
                state,
                state.cfg.telemetry,
                hot,
            ),
            Ctrl::KeepEditorOpen => draw_checkbox(
                hdc,
                *r,
                "Keep the editor open after Copy",
                state,
                state.cfg.keep_editor_open,
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
            Ctrl::CaptureHotkey => {
                // While capturing, the chip is the prompt; there is nowhere
                // else on this row to put one.
                let label = if state.capturing {
                    "Press a key\u{2026}".to_string()
                } else {
                    crate::hotkey::label(state.cfg.capture_hotkey())
                };
                draw_chip_button(hdc, *r, &label, state, state.capturing, hot);
                if !state.capturing && crate::capture_hotkey_taken() {
                    draw_text_in(
                        hdc,
                        state.font_small,
                        state.theme.muted,
                        RECT {
                            left: r.right + s(state, 8),
                            top: r.top,
                            right: state.width - s(state, 24),
                            bottom: r.bottom,
                        },
                        "In use",
                        0,
                    );
                }
            }
            Ctrl::CaptureDelay(seconds) => draw_chip_button(
                hdc,
                *r,
                &format!("{seconds}s"),
                state,
                crate::delay::sanitize(state.cfg.capture_delay_secs) == *seconds,
                hot,
            ),
            Ctrl::Diagnostics => draw_chip_button(hdc, *r, "Copy diagnostics", state, false, hot),
            Ctrl::Deactivate => {
                let licensed = matches!(state.license, crate::license::Status::Licensed { .. });
                if licensed {
                    draw_chip_button(hdc, *r, "Deactivate this PC\u{2026}", state, false, hot);
                }
            }
        }
    }

    let m = s(state, 24);

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
        Ctrl::OutputSize(max_edge) => {
            state.cfg = Config::update(|cfg| cfg.output_max_edge = max_edge);
        }
        Ctrl::CustomSize => {
            if let Ok(Some(max_edge)) = crate::number_prompt::ask(hwnd, state.cfg.output_max_edge) {
                state.cfg = Config::update(|cfg| cfg.output_max_edge = max_edge);
            }
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
        Ctrl::CaptureHotkey => {
            state.capturing = true;
        }
        Ctrl::CaptureDelay(seconds) => {
            state.cfg = Config::update(|cfg| cfg.capture_delay_secs = seconds);
        }
        Ctrl::RecordGif => {
            let enabled = !state.cfg.record_gif;
            state.cfg = Config::update(|cfg| cfg.record_gif = enabled);
        }
        Ctrl::AutoUpdate => {
            let enabled = !state.cfg.auto_update;
            state.cfg = Config::update(|cfg| cfg.auto_update = enabled);
        }
        Ctrl::Telemetry => {
            let enabled = !state.cfg.telemetry;
            state.cfg = Config::update(|cfg| cfg.telemetry = enabled);
            crate::telemetry::set_enabled(enabled);
        }
        Ctrl::KeepEditorOpen => {
            let enabled = !state.cfg.keep_editor_open;
            state.cfg = Config::update(|cfg| cfg.keep_editor_open = enabled);
        }
        Ctrl::Audio(mode) => {
            state.cfg = Config::update(|cfg| cfg.record_audio = mode.to_string());
        }
        Ctrl::Diagnostics => match crate::diagnostics::copy_report() {
            Ok(()) => {
                let _ = MessageBoxW(
                    None,
                    w!("Copied a privacy-safe support report."),
                    w!("Matteshot"),
                    MB_OK | MB_ICONINFORMATION,
                );
            }
            Err(error) => {
                eprintln!("diagnostics copy failed: {error:#}");
                let text = format!("Could not copy diagnostics:\n\n{error:#}");
                let _ = MessageBoxW(
                    None,
                    PCWSTR(HSTRING::from(text).as_ptr()),
                    w!("Matteshot"),
                    MB_OK | MB_ICONERROR,
                );
            }
        },
        Ctrl::Deactivate => {
            // A running resident owns the capture hotkeys, so route through its
            // loop: it hands PrtScn back to Windows and keeps its own hotkey
            // state in sync. Standalone --settings mode has no resident, so a
            // direct deactivation is all there is to do there.
            let result = if crate::tray::request_deactivate() {
                Ok(())
            } else {
                crate::deactivate_license()
            };
            if let Err(error) = result {
                eprintln!("deactivate failed: {error:#}");
                let text = format!("Deactivation failed:\n\n{error:#}");
                let _ = MessageBoxW(
                    None,
                    PCWSTR(HSTRING::from(text).as_ptr()),
                    w!("Matteshot"),
                    MB_OK | MB_ICONERROR,
                );
            }
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
        // Alt combinations arrive as WM_SYSKEYDOWN, so both are needed or
        // every shortcut containing Alt would be uncapturable.
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            // state_of hands out a &'static mut from a raw pointer, so it is
            // taken exactly once. Testing `capturing` in a match guard and
            // again in the body would alias two &mut to the same State.
            let Some(state) = state_of(hwnd) else {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            };
            if !state.capturing {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            {
                let vk = wparam.0 as u32;
                let down = |key: VIRTUAL_KEY| (GetKeyState(key.0 as i32) as u16 & 0x8000) != 0;
                if vk == VK_ESCAPE.0 as u32 {
                    state.capturing = false;
                    let _ = InvalidateRect(hwnd, None, false);
                    return LRESULT(0);
                }
                // Holding a modifier is not yet a choice; keep waiting for the
                // key it modifies rather than binding Ctrl on its own.
                let modifier_only = [VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN]
                    .iter()
                    .any(|m| vk == m.0 as u32);
                if modifier_only {
                    return LRESULT(0);
                }
                let mut modifiers = HOT_KEY_MODIFIERS(0);
                if down(VK_CONTROL) {
                    modifiers |= MOD_CONTROL;
                }
                if down(VK_MENU) {
                    modifiers |= MOD_ALT;
                }
                if down(VK_SHIFT) {
                    modifiers |= MOD_SHIFT;
                }
                if down(VK_LWIN) || down(VK_RWIN) {
                    modifiers |= MOD_WIN;
                }
                // A bare key would register system-wide and swallow that key
                // everywhere. Stay in capture mode rather than accept it.
                if modifiers.0 == 0 {
                    return LRESULT(0);
                }
                let chosen = crate::hotkey::Hotkey { modifiers, vk };
                let text = crate::hotkey::label(Some(chosen));
                // Round-trip through the parser so what is stored is something
                // the app can read back. label() has a "0x.." fallback for keys
                // it has no name for, and parse() rejects those, so this
                // refuses them at capture rather than writing a combo that is
                // silently ignored at startup.
                if crate::hotkey::parse(&text).is_none() {
                    return LRESULT(0);
                }
                state.cfg = Config::update(|cfg| cfg.capture_hotkey = text);
                state.capturing = false;
                crate::rebind_capture_hotkey();
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        // Clicking away or alt-tabbing abandons the capture, so the window is
        // never left silently swallowing keys.
        WM_KILLFOCUS => {
            if let Some(state) = state_of(hwnd) {
                if state.capturing {
                    state.capturing = false;
                    let _ = InvalidateRect(hwnd, None, false);
                }
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
        let cw = sc(500);

        let font = make_font(-sc(15));
        let font_small = make_font(-sc(12));

        // One pass, top to bottom. The gaps below are the spacing that used
        // to be baked into ~37 literal coordinates in this function and 17
        // more in the paint routine; they are derived from that geometry, so
        // the window is pixel-identical to before the cursor existed.
        let mut l = Layout::new(scale, cw);

        l.gap(20);
        l.header(Chrome::SaveFolderHeader, 20);
        l.gap(2);
        l.path_row(Chrome::SavePath, Ctrl::ChangeDir, Ctrl::OpenDir);

        l.gap(10);
        l.header(Chrome::VideoFolderHeader, 20);
        l.gap(4);
        l.path_row(Chrome::VideoPath, Ctrl::ChangeVideoDir, Ctrl::OpenVideoDir);

        l.gap(16);
        l.header(Chrome::RenderQualityHeader, 20);
        l.gap(4);
        l.chips(&[Ctrl::Scale(1), Ctrl::Scale(2), Ctrl::Scale(3)], 54, 62, 0, 30);

        // Finished screenshot size. These cap the completed matte and never
        // upscale a smaller image.
        l.gap(10);
        l.header(Chrome::ScreenshotSizeHeader, 22);
        l.gap(2);
        l.chips(
            &[
                Ctrl::OutputSize(crate::output::OUTPUT_ORIGINAL),
                Ctrl::OutputSize(crate::output::OUTPUT_EMAIL),
                Ctrl::OutputSize(crate::output::OUTPUT_COMPACT),
                Ctrl::CustomSize,
            ],
            102,
            110,
            0,
            30,
        );

        // Checkboxes, grouped: app and editor behavior, then recording,
        // then updates, with privacy last.
        l.gap(16);
        l.checkbox(Ctrl::Autostart);
        l.gap(4);
        l.checkbox(Ctrl::Prtscn);
        // Sits with the PrtScn toggle: both decide how capture is reached.
        l.gap(4);
        let hotkey_top = l.y;
        l.chips(&[Ctrl::CaptureHotkey], 190, 0, 150, 28);
        l.chrome_at(hotkey_top, 28, 145, Chrome::HotkeyLabel);

        // Delay sits under the shortcut: both are about arming a capture.
        l.gap(4);
        let delay_top = l.y;
        let delays: Vec<Ctrl> = crate::delay::CHOICES
            .iter()
            .map(|seconds| Ctrl::CaptureDelay(*seconds))
            .collect();
        l.chips(&delays, 54, 62, 150, 28);
        l.chrome_at(delay_top, 28, 145, Chrome::DelayLabel);

        l.gap(4);
        l.checkbox(Ctrl::KeepEditorOpen);
        l.gap(4);
        l.checkbox(Ctrl::RecordGif);

        // Recording audio: the label shares its line with the chips.
        l.gap(4);
        let audio_top = l.y;
        l.chips(&[Ctrl::Audio("off"), Ctrl::Audio("system"), Ctrl::Audio("mic")], 70, 78, 150, 28);
        l.chrome_at(audio_top, 28, 145, Chrome::AudioLabel);

        l.gap(4);
        l.checkbox(Ctrl::AutoUpdate);
        l.gap(4);
        l.checkbox(Ctrl::Telemetry);

        // Support and license actions, above the footer. Deactivate is hidden
        // for anyone without a license to give up.
        l.gap(6);
        l.button_pair(Ctrl::Diagnostics, Ctrl::Deactivate, 190, 198);

        let ch = l.finish();
        let controls = l.controls;
        let chrome = l.chrome;

        let state = Box::new(State {
            cfg: Config::load(),
            license: crate::license::status(),
            font,
            font_small,
            controls,
            chrome,
            capturing: false,
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

/// Whether the settings surface still exists.
///
/// The resident shares its normal message loop with this non-modal window, but
/// the standalone `--settings` diagnostic needs to know when the window was
/// closed so it can end its own message loop and exit cleanly.
pub fn is_open() -> bool {
    unsafe {
        let hwnd = HWND(WINDOW.load(Ordering::SeqCst) as *mut _);
        !hwnd.0.is_null() && IsWindow(hwnd).as_bool()
    }
}

