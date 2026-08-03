//! Native contact-strip picker: a borderless topmost window showing the
//! styled variants. Click / 1-7 / arrows+Enter chooses, T opens the tweak
//! editor, Esc cancels. Plain Win32 + GDI, double-buffered.

use anyhow::{Context, Result};
use image::RgbaImage;
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW,
    CreateRoundRectRgn, CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect,
    GetMonitorInfoW, InvalidateRect, SelectObject, SetBkMode, SetTextColor, SetWindowRgn,
    StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CLEARTYPE_QUALITY, DEFAULT_CHARSET,
    DIB_RGB_COLORS, DT_CENTER, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, HDC, HFONT, HMONITOR,
    MONITORINFO, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    VK_ESCAPE, VK_LEFT, VK_RETURN, VK_RIGHT, VK_SNAPSHOT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetWindowLongPtrW, LoadCursorW, PostQuitMessage, RegisterClassW, SetForegroundWindow,
    SetWindowLongPtrW, TranslateMessage, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW,
    GWLP_USERDATA, IDC_ARROW, MSG, WA_INACTIVE, WM_ACTIVATE, WM_DESTROY, WM_ERASEBKGND,
    WM_KEYDOWN, WM_KEYUP, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCCREATE, WM_PAINT, WNDCLASSW,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_HOTKEY, WM_USER};

const MARGIN: i32 = 16;
const GAP: i32 = 12;
const THUMB_H: i32 = 168;
const LABEL_H: i32 = 22;
const HINT_H: i32 = 20;
/// Gap between the strip and the bottom of the work area.
const BOTTOM_INSET: i32 = 28;
/// Above this mean aspect a side-by-side strip cannot stay legible (a taskbar
/// grab is ~20:1), so the variants stack instead.
const STACK_ASPECT: f32 = 3.0;
/// Posted to the strip when the resident PrtScn hotkey fires mid-pick.
const WM_RETAKE: u32 = WM_USER + 41;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyAction {
    Choose(usize),
    Tweak(usize),
    Pin,
    CopyText,
    Cancel,
    Hover(i32),
}

fn key_action(vk: u16, hover: i32, count: usize) -> Option<KeyAction> {
    let selected = (count > 0).then(|| (hover.max(0) as usize).min(count - 1));
    match vk {
        v if v == VK_ESCAPE.0 => Some(KeyAction::Cancel),
        v if v == VK_RETURN.0 => selected.map(KeyAction::Choose),
        0x54 => selected.map(KeyAction::Tweak), // T
        0x50 => Some(KeyAction::Pin),           // P
        0x43 => Some(KeyAction::CopyText),      // C
        v if v == VK_LEFT.0 && count > 0 => Some(KeyAction::Hover(
            (hover.max(0) - 1).rem_euclid(count as i32),
        )),
        v if v == VK_RIGHT.0 && count > 0 => Some(KeyAction::Hover(
            (hover + 1).rem_euclid(count as i32),
        )),
        v if (0x31..=0x39).contains(&v) => {
            let index = (v - 0x31) as usize;
            (index < count).then_some(KeyAction::Choose(index))
        }
        _ => None,
    }
}

fn should_cancel_on_deactivate(
    action_chosen: bool,
    suspended: bool,
    visible_for_ms: u128,
) -> bool {
    !action_chosen && !suspended && visible_for_ms > 500
}



pub enum PickAction {
    /// Copy + save this variant.
    Choose(usize),
    Cancel,
    /// Open the tweak panel on this variant.
    Tweak(usize),
    /// Float the raw capture as a topmost reference pin.
    Pin,
    /// OCR the raw capture and copy the text.
    CopyText,
    /// PrtScn pressed while the strip was open: the user re-entered the
    /// overlay and made a new selection, replacing the pending shot.
    Reshoot(crate::overlay::Selection, HMONITOR),
}

struct Thumb {
    bgra: Vec<u8>,
    label: Vec<u16>,
    w: i32,
    h: i32,
    x: i32,
    y: i32,
}

/// Where one variant sits inside the strip. Paint and hit-testing work off
/// these directly, so the layout is free to arrange them any way it likes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cell {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

#[derive(Debug)]
struct Layout {
    cells: Vec<Cell>,
    width: i32,
    height: i32,
}

/// Arrange `aspects` (width / height per variant) inside the work area.
///
/// The result never exceeds the work area — a wide capture like the taskbar
/// used to compute a strip several monitors long and spill off screen. Wide
/// content stacks into rows instead, where each variant gets the full width.
fn layout(aspects: &[f32], work_w: i32, work_h: i32) -> Layout {
    let n = aspects.len() as i32;
    if n == 0 {
        return Layout { cells: Vec::new(), width: MARGIN * 2, height: MARGIN * 2 };
    }
    // Sanitize: a degenerate preview must not poison the whole layout.
    let aspects: Vec<f32> = aspects
        .iter()
        .map(|a| if a.is_finite() && *a > 0.0 { a.clamp(0.01, 100.0) } else { 1.0 })
        .collect();
    let sum_aspect: f32 = aspects.iter().sum();
    let max_aspect = aspects.iter().cloned().fold(0.01f32, f32::max);

    let chrome_h = MARGIN * 2 + HINT_H + 8 + BOTTOM_INSET;
    let stacked = n > 1 && sum_aspect / n as f32 > STACK_ASPECT;

    // Fit against both axes. Whatever height falls out is the height: a floor
    // here would be a constraint violation, not a nicer picture.
    let cells: Vec<Cell> = if stacked {
        let avail_w = (work_w - MARGIN * 2 - 24).max(80) as f32;
        let avail_h = (work_h - chrome_h - GAP * (n - 1)).max(n * 8);
        let row_h = (avail_h / n - LABEL_H).max(4);
        let thumb_h = THUMB_H.min((avail_w / max_aspect) as i32).min(row_h).max(1);
        let widest = aspects
            .iter()
            .map(|a| (a * thumb_h as f32) as i32)
            .fold(1, i32::max);
        let cells = aspects
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let w = ((a * thumb_h as f32) as i32).max(1);
                Cell {
                    x: MARGIN + (widest - w) / 2,
                    y: MARGIN + i as i32 * (thumb_h + LABEL_H + GAP),
                    w,
                    h: thumb_h,
                }
            })
            .collect();
        cells
    } else {
        let avail_w = (work_w - MARGIN * 2 - GAP * (n - 1) - 24).max(80) as f32;
        let avail_h = (work_h - chrome_h - LABEL_H).max(4);
        let thumb_h = THUMB_H.min((avail_w / sum_aspect) as i32).min(avail_h).max(1);
        let mut x = MARGIN;
        let cells = aspects
            .iter()
            .map(|a| {
                let w = ((a * thumb_h as f32) as i32).max(1);
                let cell = Cell { x, y: MARGIN, w, h: thumb_h };
                x += w + GAP;
                cell
            })
            .collect();
        cells
    };

    let right = cells.iter().map(|c| c.x + c.w).max().unwrap_or(MARGIN);
    let bottom = cells.iter().map(|c| c.y + c.h).max().unwrap_or(MARGIN);

    // Belt and braces: whatever the arithmetic above produced, the window is
    // never allowed off the monitor it was asked to appear on.
    Layout {
        cells,
        width: (right + MARGIN).min(work_w - 16).max(120),
        height: (bottom + LABEL_H + HINT_H + 8).min(work_h - 16).max(80),
    }
}

struct State {
    thumbs: Vec<Thumb>,
    hover: i32,
    action: Option<PickAction>,
    shown_at: std::time::Instant,
    font: HFONT,
    font_small: HFONT,
    width: i32,
    height: i32,
    /// True while a nested reshoot overlay is running — suppresses the
    /// cancel-on-focus-loss behavior.
    suspended: bool,
    theme: crate::theme::Theme,
}

fn to_bgra(img: &RgbaImage) -> Vec<u8> {
    let mut out = Vec::with_capacity((img.width() * img.height() * 4) as usize);
    for p in img.pixels() {
        out.extend_from_slice(&[p[2], p[1], p[0], 255]);
    }
    out
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

fn hit_test(state: &State, mx: i32, my: i32) -> i32 {
    for (i, t) in state.thumbs.iter().enumerate() {
        if mx >= t.x && mx < t.x + t.w && my >= t.y && my < t.y + t.h + LABEL_H {
            return i as i32;
        }
    }
    -1
}

unsafe fn paint(hdc: HDC, state: &State) {
    let bg = CreateSolidBrush(state.theme.panel);
    FillRect(
        hdc,
        &RECT { left: 0, top: 0, right: state.width, bottom: state.height },
        bg,
    );
    let _ = DeleteObject(bg);
    SetBkMode(hdc, TRANSPARENT);

    for (i, t) in state.thumbs.iter().enumerate() {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: t.w,
                biHeight: -t.h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        StretchDIBits(
            hdc,
            t.x,
            t.y,
            t.w,
            t.h,
            0,
            0,
            t.w,
            t.h,
            Some(t.bgra.as_ptr() as *const _),
            &info,
            DIB_RGB_COLORS,
            SRCCOPY,
        );

        let hovered = i as i32 == state.hover;
        if hovered {
            let accent = CreateSolidBrush(state.theme.accent);
            let (x0, y0, x1, y1) = (t.x - 3, t.y - 3, t.x + t.w + 3, t.y + t.h + 3);
            for r in [
                RECT { left: x0, top: y0, right: x1, bottom: y0 + 2 },
                RECT { left: x0, top: y1 - 2, right: x1, bottom: y1 },
                RECT { left: x0, top: y0, right: x0 + 2, bottom: y1 },
                RECT { left: x1 - 2, top: y0, right: x1, bottom: y1 },
            ] {
                FillRect(hdc, &r, accent);
            }
            let _ = DeleteObject(accent);
        }

        SelectObject(hdc, state.font);
        SetTextColor(hdc, if hovered { state.theme.accent } else { state.theme.text });
        let mut label_rect = RECT {
            left: t.x,
            top: t.y + t.h + 2,
            right: t.x + t.w,
            bottom: t.y + t.h + LABEL_H,
        };
        let mut label = t.label.clone();
        DrawTextW(
            hdc,
            &mut label,
            &mut label_rect,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
        );
    }

    // Hint line along the bottom.
    SelectObject(hdc, state.font_small);
    SetTextColor(hdc, state.theme.faint);
    let mut hint = wide(
        "\u{2713} copied \u{2014} 1\u{2013}7 or click to switch   \u{00b7}   T tweak   \u{00b7}   C copy text   \u{00b7}   P pin   \u{00b7}   PrtScn snip again   \u{00b7}   Esc",
    );
    let mut hint_rect = RECT {
        left: 0,
        top: state.height - HINT_H - 4,
        right: state.width,
        bottom: state.height - 4,
    };
    DrawTextW(
        hdc,
        &mut hint,
        &mut hint_rect,
        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );
}

unsafe fn finish(hwnd: HWND, state: &mut State, action: PickAction) {
    state.action = Some(action);
    let _ = DestroyWindow(hwnd);
}

/// PrtScn mid-pick: reopen the freeze-frame overlay so the user can snip
/// again like normal. The freeze happens while this strip is still on
/// screen, so the strip itself is snippable as a region. Esc in the overlay
/// returns to this strip untouched.
unsafe fn trigger_reshoot(hwnd: HWND, state: &mut State) {
    if state.suspended {
        return;
    }
    state.suspended = true;
    let result = crate::overlay::select();
    state.suspended = false;
    match result {
        Ok(Some((sel, mon))) => finish(hwnd, state, PickAction::Reshoot(sel, mon)),
        _ => {
            // Cancelled — keep picking; reclaim focus from the dead overlay.
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_ERASEBKGND => LRESULT(1), // double-buffered; skip background erase
        WM_PAINT => {
            if let Some(state) = state_of(hwnd) {
                let mut ps = PAINTSTRUCT::default();
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
                let (mx, my) = (
                    (lparam.0 & 0xFFFF) as i16 as i32,
                    ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                );
                let hover = hit_test(state, mx, my);
                if hover != state.hover && hover >= 0 {
                    state.hover = hover;
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if let Some(state) = state_of(hwnd) {
                let (mx, my) = (
                    (lparam.0 & 0xFFFF) as i16 as i32,
                    ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                );
                let hit = hit_test(state, mx, my);
                if hit >= 0 {
                    finish(hwnd, state, PickAction::Choose(hit as usize));
                }
            }
            LRESULT(0)
        }
        // PrtScn historically arrives as key-up only.
        WM_KEYUP => {
            if let Some(state) = state_of(hwnd) {
                if wparam.0 as u16 == VK_SNAPSHOT.0 {
                    trigger_reshoot(hwnd, state);
                }
            }
            LRESULT(0)
        }
        WM_RETAKE => {
            if let Some(state) = state_of(hwnd) {
                trigger_reshoot(hwnd, state);
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if let Some(state) = state_of(hwnd) {
                let vk = wparam.0 as u16;
                match key_action(vk, state.hover, state.thumbs.len()) {
                    Some(KeyAction::Cancel) => finish(hwnd, state, PickAction::Cancel),
                    Some(KeyAction::Choose(index)) => {
                        finish(hwnd, state, PickAction::Choose(index))
                    }
                    Some(KeyAction::Tweak(index)) => finish(hwnd, state, PickAction::Tweak(index)),
                    Some(KeyAction::Pin) => finish(hwnd, state, PickAction::Pin),
                    Some(KeyAction::CopyText) => finish(hwnd, state, PickAction::CopyText),
                    Some(KeyAction::Hover(index)) => {
                        state.hover = index;
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    None => {}
                }
            }
            LRESULT(0)
        }
        WM_ACTIVATE => {
            // Clicking away cancels — but ignore the initial activation churn.
            if let Some(state) = state_of(hwnd) {
                if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE
                    && should_cancel_on_deactivate(
                        state.action.is_some(),
                        state.suspended,
                        state.shown_at.elapsed().as_millis(),
                    )
                {
                    finish(hwnd, state, PickAction::Cancel);
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Show the picker on `monitor`; `initial` is the preselected variant.
pub fn pick(
    previews: &[RgbaImage],
    names: &[&'static str],
    monitor: HMONITOR,
    initial: usize,
) -> Result<PickAction> {
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetMonitorInfoW(monitor, &mut mi).ok().context("monitor info")? };
    let work_w = mi.rcWork.right - mi.rcWork.left;
    let work_h = mi.rcWork.bottom - mi.rcWork.top;

    // Fit the strip inside the work area no matter what shape the content is —
    // a taskbar capture must not spill onto other monitors.
    let aspects: Vec<f32> = previews
        .iter()
        .map(|p| p.width() as f32 / p.height().max(1) as f32)
        .collect();
    let plan = layout(&aspects, work_w, work_h);

    let mut thumbs = Vec::new();
    for (i, p) in previews.iter().enumerate() {
        let cell = plan.cells[i];
        let resized = image::imageops::resize(
            p,
            cell.w as u32,
            cell.h as u32,
            image::imageops::FilterType::Triangle,
        );
        thumbs.push(Thumb {
            bgra: to_bgra(&resized),
            label: wide(&format!("{}  {}", i + 1, names.get(i).unwrap_or(&""))),
            w: cell.w,
            h: cell.h,
            x: cell.x,
            y: cell.y,
        });
    }
    let (total_w, total_h) = (plan.width, plan.height);

    let win_x = (mi.rcWork.left + (work_w - total_w) / 2)
        .clamp(mi.rcWork.left + 8, (mi.rcWork.right - total_w - 8).max(mi.rcWork.left + 8));
    let win_y = (mi.rcWork.top + work_h - total_h - BOTTOM_INSET).max(mi.rcWork.top + 8);

    let (font, font_small) = unsafe { (make_font(-14), make_font(-12)) };
    let mut state = Box::new(State {
        thumbs,
        hover: (initial.min(previews.len().saturating_sub(1))) as i32,
        action: None,
        shown_at: std::time::Instant::now(),
        font,
        font_small,
        width: total_w,
        height: total_h,
        suspended: false,
        theme: crate::theme::current(),
    });

    unsafe {
        let hinstance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: w!("matteshot_picker"),
            ..Default::default()
        };
        RegisterClassW(&class);

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            w!("matteshot_picker"),
            w!("Matteshot — pick a matte"),
            WS_POPUP | WS_VISIBLE,
            win_x,
            win_y,
            total_w,
            total_h,
            None,
            None,
            hinstance,
            Some(&mut *state as *mut State as *const _),
        )?;

        let region = CreateRoundRectRgn(0, 0, total_w, total_h, 16, 16);
        SetWindowRgn(hwnd, region, true);
        let _ = SetForegroundWindow(hwnd);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            // The resident hotkeys arrive as thread messages that would
            // otherwise be swallowed by this modal loop; PrtScn mid-pick
            // means "shoot the strip itself".
            if msg.hwnd.0.is_null() && msg.message == WM_HOTKEY {
                let _ = PostMessageW(hwnd, WM_RETAKE, WPARAM(0), LPARAM(0));
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = DeleteObject(state.font);
        let _ = DeleteObject(state.font_small);
    }

    Ok(state.action.unwrap_or(PickAction::Cancel))
}

#[cfg(test)]
mod tests {
    use super::{key_action, layout, should_cancel_on_deactivate, KeyAction, THUMB_H};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        VK_ESCAPE, VK_LEFT, VK_RETURN, VK_RIGHT,
    };

    #[test]
    fn picker_shortcuts_choose_the_visible_variant() {
        assert_eq!(key_action(VK_RETURN.0, 3, 7), Some(KeyAction::Choose(3)));
        assert_eq!(key_action(0x54, 3, 7), Some(KeyAction::Tweak(3)));
        assert_eq!(key_action(0x50, 3, 7), Some(KeyAction::Pin));
        assert_eq!(key_action(0x43, 3, 7), Some(KeyAction::CopyText));
        assert_eq!(key_action(VK_ESCAPE.0, 3, 7), Some(KeyAction::Cancel));
    }

    #[test]
    fn picker_arrows_wrap_and_recover_an_unset_hover() {
        assert_eq!(key_action(VK_LEFT.0, 0, 7), Some(KeyAction::Hover(6)));
        assert_eq!(key_action(VK_RIGHT.0, 6, 7), Some(KeyAction::Hover(0)));
        assert_eq!(key_action(VK_RIGHT.0, -1, 7), Some(KeyAction::Hover(0)));
        assert_eq!(key_action(VK_LEFT.0, -1, 7), Some(KeyAction::Hover(6)));
    }

    #[test]
    fn numeric_shortcuts_cannot_select_a_missing_variant() {
        assert_eq!(key_action(0x31, 0, 7), Some(KeyAction::Choose(0)));
        assert_eq!(key_action(0x37, 0, 7), Some(KeyAction::Choose(6)));
        assert_eq!(key_action(0x38, 0, 7), None);
        assert_eq!(key_action(VK_RETURN.0, 0, 0), None);
        assert_eq!(key_action(VK_RIGHT.0, 0, 0), None);
    }

    /// Every cell inside the window, and the window inside the work area.
    fn assert_contained(aspects: &[f32], work_w: i32, work_h: i32) {
        let plan = layout(aspects, work_w, work_h);
        assert_eq!(plan.cells.len(), aspects.len());
        assert!(
            plan.width <= work_w && plan.height <= work_h,
            "window {}x{} exceeds work area {}x{}",
            plan.width,
            plan.height,
            work_w,
            work_h
        );
        for (i, c) in plan.cells.iter().enumerate() {
            assert!(c.w >= 1 && c.h >= 1, "cell {i} is degenerate: {c:?}");
            assert!(
                c.x >= 0 && c.y >= 0 && c.x + c.w <= plan.width && c.y + c.h <= plan.height,
                "cell {i} {c:?} escapes the {}x{} window",
                plan.width,
                plan.height
            );
        }
    }

    #[test]
    fn a_taskbar_capture_stays_on_one_monitor() {
        // 2560x48 plus matte padding: the shape that used to run off screen.
        assert_contained(&[20.0; 7], 2560, 1392);
        assert_contained(&[20.0; 7], 1280, 680);
        // Stacked rows, so each variant is wider than the old strip allowed.
        let plan = layout(&[20.0; 7], 2560, 1392);
        assert!(plan.cells[0].w > 1000, "stacked rows should use the width");
        assert!(plan.cells[1].y > plan.cells[0].y, "variants should stack");
    }

    #[test]
    fn extreme_aspects_never_overflow_the_work_area() {
        for aspects in [
            vec![100.0; 7],  // a one-pixel-tall sliver
            vec![0.01; 7],   // a full-page scroll capture
            vec![1.0; 7],
            vec![0.5; 1],
            vec![f32::NAN, f32::INFINITY, 0.0, -3.0],
        ] {
            assert_contained(&aspects, 2560, 1392);
            assert_contained(&aspects, 1366, 728);
        }
        assert_contained(&[], 2560, 1392);
    }

    #[test]
    fn ordinary_captures_keep_the_horizontal_strip() {
        let plan = layout(&[1.6; 7], 2560, 1392);
        assert!(plan.cells.iter().all(|c| c.y == plan.cells[0].y), "one row");
        assert_eq!(plan.cells[0].h, THUMB_H, "and full-size thumbs");
        assert!(plan.cells[1].x > plan.cells[0].x);
    }

    #[test]
    fn focus_loss_only_cancels_a_stable_unsuspended_picker() {
        assert!(!should_cancel_on_deactivate(false, false, 500));
        assert!(should_cancel_on_deactivate(false, false, 501));
        assert!(!should_cancel_on_deactivate(true, false, 1_000));
        assert!(!should_cancel_on_deactivate(false, true, 1_000));
    }
}

