//! In-app capture history: an index of saved screenshots, browsable and
//! reusable without a trip to Explorer.
//!
//! The log only ever indexes files that already live permanently in the
//! save folder — nothing here deletes a capture on its own. The retention
//! cap bounds how much the *browser* has to show and decode, not how long a
//! file survives on disk; only an explicit Delete removes one.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicIsize, Ordering};

use anyhow::{Context, Result};
use chrono::{Local, TimeZone};
use image::RgbaImage;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePen,
    CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, GetMonitorInfoW,
    InvalidateRect, MonitorFromPoint, MonitorFromWindow, RoundRect, SelectObject, SetBkMode,
    SetTextColor, StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CLEARTYPE_QUALITY,
    DEFAULT_CHARSET, DIB_RGB_COLORS, DT_CENTER, DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE,
    DT_VCENTER, DT_WORDBREAK,
    FF_DONTCARE, HDC, HFONT, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT,
    SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetDoubleClickTime, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    GetCursorPos, GetWindowLongPtrW, IsWindow, KillTimer, LoadCursorW,
    MessageBoxW, RegisterClassW, SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, TrackPopupMenu, CREATESTRUCTW, CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA,
    HWND_NOTOPMOST, HWND_TOPMOST, IDC_ARROW, IDYES, MB_ICONWARNING, MB_OK, MB_YESNO, MF_STRING,
    SWP_NOMOVE, SWP_NOSIZE, SW_RESTORE, SW_SHOWNORMAL, TPM_NONOTIFY, TPM_RETURNCMD,
    WM_CLOSE, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_RBUTTONUP, WM_TIMER, WNDCLASSW,
    WS_CAPTION, WS_EX_APPWINDOW, WS_SYSMENU, WS_VISIBLE,
};

// ---------------------------------------------------------------------------
// Persistence — pure logic + file IO, no Win32.
// ---------------------------------------------------------------------------

/// Recent-history cap: bounds the log file and how much a window open has to
/// decode. Entries past the cap are just no longer browsable; their files are
/// untouched, exactly like a save that predates this feature.
const MAX_ENTRIES: usize = 150;

#[derive(Serialize, Deserialize, Clone)]
pub struct Entry {
    pub path: PathBuf,
    /// Unix seconds.
    pub saved_at: i64,
    pub width: u32,
    pub height: u32,
    pub style: String,
    /// The captured window's title, or a region's size — whatever the
    /// editor tab was already labeled at save time. Absent for entries
    /// recorded before this field existed.
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct Log {
    /// Oldest first, so trimming the cap is a drop from the front.
    #[serde(default)]
    entries: Vec<Entry>,
}

fn history_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("matteshot").join("history.json"))
}

const HISTORY_MUTEX: &str = "Local\\Matteshot.History.State";

fn load_unlocked() -> Log {
    history_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_unlocked(log: &Log) {
    let Some(path) = history_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_vec_pretty(log) {
        let _ = crate::state_lock::atomic_write(&path, &json);
    }
}

/// Drop entries the predicate rejects, then keep only the most recent `cap`.
/// A free function so the retention rule is testable without touching disk.
fn trim_entries(mut entries: Vec<Entry>, cap: usize, exists: impl Fn(&Path) -> bool) -> Vec<Entry> {
    entries.retain(|e| exists(&e.path));
    if entries.len() > cap {
        entries.drain(0..entries.len() - cap);
    }
    entries
}

/// A window title is not bounded by Windows the way a control's own text
/// often is; capping it here (same reasoning as diagnostics.rs's own event
/// log) keeps one pathological title from bloating every future read of the
/// whole history file, not just its own entry.
const MAX_SOURCE_CHARS: usize = 200;

/// A window's title is set by whatever app owns it, not by this app, so it
/// gets the same treatment diagnostics.rs's event log gives untrusted text:
/// control characters (plus the Unicode line/paragraph separators, which
/// `char::is_control` does not cover) flattened to spaces rather than left
/// to disturb the label's layout. Bidi override/isolate and zero-width
/// characters are dropped outright — spacing the former out would still
/// leave them able to reorder the surrounding text, and the latter are
/// invisible either way, including making an otherwise-blank title dodge
/// the empty-string check below by being technically non-empty.
fn sanitize_source(text: &str) -> String {
    text.chars()
        .filter(|c| {
            !matches!(*c,
                '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
                | '\u{FEFF}')
        })
        .map(|c| if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') { ' ' } else { c })
        .collect()
}

/// A blank or whitespace-only title is not useful to show later, so it's
/// dropped to `None` here rather than carried through as an empty label.
fn normalize_source(source: Option<&str>) -> Option<String> {
    let sanitized = sanitize_source(source?);
    let trimmed = sanitized.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(MAX_SOURCE_CHARS).collect())
}

/// Record a successful save. Never fatal: history is a convenience index, not
/// something a capture should fail over, so every error is swallowed here.
pub fn record(path: &Path, width: u32, height: u32, style: &str, source: Option<&str>) {
    let _guard = crate::state_lock::lock(HISTORY_MUTEX).ok();
    let mut log = load_unlocked();
    log.entries.push(Entry {
        path: path.to_path_buf(),
        saved_at: Local::now().timestamp(),
        width,
        height,
        style: style.to_string(),
        source: normalize_source(source),
    });
    log.entries = trim_entries(log.entries, MAX_ENTRIES, |p| p.is_file());
    save_unlocked(&log);
}

/// Every entry whose file still exists, most recent first.
pub fn list() -> Vec<Entry> {
    let _guard = crate::state_lock::lock(HISTORY_MUTEX).ok();
    let mut entries = trim_entries(load_unlocked().entries, MAX_ENTRIES, |p| p.is_file());
    entries.reverse();
    entries
}

/// Delete the file and drop it from history. Unlike `record`, errors surface:
/// a Delete click that silently failed would look like it had worked.
pub fn remove(path: &Path) -> Result<()> {
    let _guard = crate::state_lock::lock(HISTORY_MUTEX).ok();
    match std::fs::remove_file(path) {
        Ok(()) => {}
        // Already gone (deleted outside the app, or a repeat click racing
        // its own first Delete): the index is just stale, so finish
        // dropping the entry instead of reporting a failure the user has no
        // way to act on.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("delete capture"),
    }
    let mut log = load_unlocked();
    log.entries.retain(|e| e.path != path);
    save_unlocked(&log);
    Ok(())
}

fn format_when(saved_at: i64) -> String {
    match Local.timestamp_opt(saved_at, 0).single() {
        Some(dt) => dt.format("%b %-d, %-I:%M %p").to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;

    fn entry(path: &str) -> Entry {
        Entry {
            path: PathBuf::from(path),
            saved_at: 0,
            width: 10,
            height: 10,
            style: "Deep".into(),
            source: None,
        }
    }

    #[test]
    fn missing_files_are_dropped_regardless_of_the_cap() {
        let entries = vec![entry("a"), entry("b"), entry("c")];
        let kept = trim_entries(entries, 10, |p| p.to_str() != Some("b"));
        let paths: Vec<_> = kept.iter().map(|e| e.path.to_str().unwrap()).collect();
        assert_eq!(paths, vec!["a", "c"]);
    }

    #[test]
    fn the_cap_drops_the_oldest_entries_first() {
        let entries = vec![entry("a"), entry("b"), entry("c"), entry("d")];
        let kept = trim_entries(entries, 2, |_| true);
        let paths: Vec<_> = kept.iter().map(|e| e.path.to_str().unwrap()).collect();
        assert_eq!(paths, vec!["c", "d"], "oldest-first order means the cap trims the front");
    }

    #[test]
    fn a_cap_at_or_above_the_count_changes_nothing() {
        let entries = vec![entry("a"), entry("b")];
        assert_eq!(trim_entries(entries.clone(), 2, |_| true).len(), 2);
        assert_eq!(trim_entries(entries, 99, |_| true).len(), 2);
    }

    #[test]
    fn a_readable_timestamp_formats_and_an_unreadable_one_does_not_panic() {
        assert!(!format_when(1_700_000_000).is_empty());
        assert_eq!(format_when(i64::MAX), "");
    }

    #[test]
    fn the_displayed_time_is_12_hour_with_a_meridiem_not_24_hour() {
        // Every local time in 12-hour format carries AM or PM; 24-hour format
        // never does, regardless of which timezone this test happens to run
        // in — so this holds without pinning a specific hour.
        let formatted = format_when(1_700_000_000);
        assert!(
            formatted.contains("AM") || formatted.contains("PM"),
            "expected a 12-hour time with AM/PM, got {formatted:?}"
        );
    }

    #[test]
    fn a_blank_source_normalizes_to_none_but_a_real_title_is_kept_trimmed() {
        assert_eq!(normalize_source(None), None);
        assert_eq!(normalize_source(Some("")), None);
        assert_eq!(normalize_source(Some("   ")), None);
        assert_eq!(normalize_source(Some("  Notepad  ")), Some("Notepad".to_string()));
    }

    #[test]
    fn a_pathological_window_title_is_capped_before_it_reaches_the_log() {
        let long = "x".repeat(MAX_SOURCE_CHARS + 50);
        let normalized = normalize_source(Some(&long)).unwrap();
        assert_eq!(normalized.chars().count(), MAX_SOURCE_CHARS);
    }

    #[test]
    fn control_and_bidi_override_characters_do_not_survive_normalization() {
        // A tab/newline flattens to a space rather than disturbing the
        // label's single-line layout; a bidi override is dropped outright,
        // since spacing it out would still leave it able to reorder the
        // surrounding text.
        let normalized = normalize_source(Some("Left\t\u{202E}txet.exe\u{202C}")).unwrap();
        assert!(!normalized.contains('\t'));
        assert!(!normalized.contains('\u{202E}'));
        assert!(!normalized.contains('\u{202C}'));
        assert_eq!(normalized, "Left txet.exe");
    }

    #[test]
    fn a_title_made_entirely_of_zero_width_characters_normalizes_to_none() {
        // Invisible either way, but without stripping them a title like this
        // would pass the empty-string check and print as a blank line
        // instead of the placeholder.
        assert_eq!(normalize_source(Some("\u{200B}\u{200C}\u{FEFF}")), None);
    }

    #[test]
    fn line_and_paragraph_separators_flatten_like_a_newline_does() {
        // Not `char::is_control` (they're categories Zl/Zp, not Cc), so this
        // needs its own case rather than relying on the same check as \t/\n.
        let normalized = normalize_source(Some("Left\u{2028}Right\u{2029}End")).unwrap();
        assert_eq!(normalized, "Left Right End");
    }
}

// ---------------------------------------------------------------------------
// Browser window — pure layout helpers first (testable), Win32 UI below.
// ---------------------------------------------------------------------------

const MARGIN: i32 = 16;
const GAP: i32 = 14;
const CELL_W: i32 = 176;
const CELL_IMG_H: i32 = 118;
/// Source app / window title — the more identifying of the two lines, so it
/// sits above the timestamp.
const SOURCE_LABEL_H: i32 = 18;
const LABEL_H: i32 = 20;
const CELL_H: i32 = CELL_IMG_H + SOURCE_LABEL_H + LABEL_H;
const WIN_W: i32 = 860;
const WIN_H: i32 = 620;
const WHEEL_STEP: i32 = 90;
const STATUS_MS: u128 = 1500;
/// Periodic timer that ages out `status`.
const STATUS_TIMER_ID: usize = 1;
/// One-shot, armed on WM_LBUTTONUP and cancelled by WM_LBUTTONDBLCLK: what
/// tells a single click from the first half of a double-click.
const CLICK_TIMER_ID: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cell {
    x: i32,
    y: i32,
}

/// Row-major grid: as many columns as fit `viewport_w`, at least one. Returns
/// each cell's top-left in unscrolled content coordinates, plus the total
/// content height. `margin`/`gap` are parameters rather than the module
/// constants directly so a caller can pass DPI-scaled values — `viewport_w`,
/// `cell_w`, and `cell_h` already have to be, and gutters that stayed at
/// logical size while the cards around them grew would look inconsistent.
fn grid_layout(
    count: usize,
    viewport_w: i32,
    cell_w: i32,
    cell_h: i32,
    margin: i32,
    gap: i32,
) -> (Vec<Cell>, i32) {
    let cols = (((viewport_w - margin * 2 + gap) / (cell_w + gap)).max(1)) as usize;
    let cells: Vec<Cell> = (0..count)
        .map(|i| {
            let (col, row) = (i % cols, i / cols);
            Cell {
                x: margin + col as i32 * (cell_w + gap),
                y: margin + row as i32 * (cell_h + gap),
            }
        })
        .collect();
    let rows = if count == 0 { 0 } else { (count - 1) / cols + 1 };
    let content_h = if rows == 0 {
        margin * 2
    } else {
        margin * 2 + rows as i32 * cell_h + (rows as i32 - 1) * gap
    };
    (cells, content_h)
}

fn hit_test(cells: &[Cell], cell_w: i32, cell_h: i32, scroll_y: i32, mx: i32, my: i32) -> i32 {
    cells
        .iter()
        .position(|c| {
            let (cx, cy) = (c.x, c.y - scroll_y);
            mx >= cx && mx < cx + cell_w && my >= cy && my < cy + cell_h
        })
        .map(|i| i as i32)
        .unwrap_or(-1)
}

/// How far the content can scroll before the bottom row clears the viewport.
fn max_scroll(content_h: i32, viewport_h: i32) -> i32 {
    (content_h - viewport_h).max(0)
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    #[test]
    fn a_wide_viewport_fits_more_columns_than_a_narrow_one() {
        let (wide_cells, _) = grid_layout(12, 900, CELL_W, CELL_H, MARGIN, GAP);
        let (narrow_cells, _) = grid_layout(12, 300, CELL_W, CELL_H, MARGIN, GAP);
        let wide_cols = wide_cells.iter().filter(|c| c.y == wide_cells[0].y).count();
        let narrow_cols = narrow_cells.iter().filter(|c| c.y == narrow_cells[0].y).count();
        assert!(wide_cols > narrow_cols);
    }

    #[test]
    fn cells_never_overlap() {
        let (cells, _) = grid_layout(23, 860, CELL_W, CELL_H, MARGIN, GAP);
        for (i, a) in cells.iter().enumerate() {
            for b in &cells[i + 1..] {
                let overlap_x = a.x < b.x + CELL_W && b.x < a.x + CELL_W;
                let overlap_y = a.y < b.y + CELL_H && b.y < a.y + CELL_H;
                assert!(!(overlap_x && overlap_y), "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn an_empty_history_has_no_cells_and_minimal_height() {
        let (cells, h) = grid_layout(0, 860, CELL_W, CELL_H, MARGIN, GAP);
        assert!(cells.is_empty());
        assert_eq!(h, MARGIN * 2);
    }

    #[test]
    fn even_a_single_pixel_viewport_still_lays_out_one_column() {
        let (cells, _) = grid_layout(3, 1, CELL_W, CELL_H, MARGIN, GAP);
        assert_eq!(cells.iter().filter(|c| c.x == MARGIN).count(), 3, "must collapse to one column, not zero");
    }

    #[test]
    fn hit_test_accounts_for_scroll_offset() {
        let (cells, _) = grid_layout(4, 860, CELL_W, CELL_H, MARGIN, GAP);
        // Unscrolled, the mouse over the first cell hits index 0.
        assert_eq!(hit_test(&cells, CELL_W, CELL_H, 0, MARGIN + 5, MARGIN + 5), 0);
        // Scroll the content up by one row's worth; the same screen point now
        // misses everything in a one-row grid.
        assert_eq!(hit_test(&cells, CELL_W, CELL_H, CELL_H + GAP, MARGIN + 5, MARGIN + 5), -1);
        assert_eq!(hit_test(&cells, CELL_W, CELL_H, 0, 0, 0), -1);
    }

    #[test]
    fn scroll_is_clamped_to_the_overflow_past_the_viewport() {
        assert_eq!(max_scroll(2000, 600), 1400);
        assert_eq!(max_scroll(400, 600), 0, "content shorter than the viewport never scrolls");
    }
}

struct Thumb {
    entry: Entry,
    bgra: Vec<u8>,
    img_w: i32,
    img_h: i32,
    /// Normalized once here rather than on every WM_PAINT — hover alone
    /// repaints the whole visible grid, so redoing this per thumbnail per
    /// frame would be pure repeated allocation for a value that never
    /// changes after the entry is loaded.
    source_label: String,
}

fn to_bgra(img: &RgbaImage) -> Vec<u8> {
    let mut out = Vec::with_capacity((img.width() * img.height() * 4) as usize);
    for p in img.pixels() {
        out.extend_from_slice(&[p[2], p[1], p[0], 255]);
    }
    out
}

/// Decode and letterbox one entry's file into a thumbnail no larger than the
/// cell's image area. `None` when the file cannot be decoded (corrupt or
/// swapped for something else since it was indexed) — the entry is simply
/// left out of the grid rather than shown broken.
fn make_thumb(entry: &Entry, max_w: i32, max_h: i32) -> Option<Thumb> {
    let img = image::open(&entry.path).ok()?.to_rgba8();
    let (w, h) = (img.width().max(1) as f32, img.height().max(1) as f32);
    let scale = (max_w as f32 / w).min(max_h as f32 / h);
    let (tw, th) = ((w * scale).round().max(1.0) as u32, (h * scale).round().max(1.0) as u32);
    let resized = image::imageops::resize(&img, tw, th, image::imageops::FilterType::Triangle);
    let source_label =
        normalize_source(entry.source.as_deref()).unwrap_or_else(|| "\u{2014}".to_string());
    Some(Thumb {
        entry: entry.clone(),
        bgra: to_bgra(&resized),
        img_w: tw as i32,
        img_h: th as i32,
        source_label,
    })
}

fn build_thumbs(entries: &[Entry], max_w: i32, max_h: i32) -> Vec<Thumb> {
    entries.par_iter().filter_map(|e| make_thumb(e, max_w, max_h)).collect()
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

struct State {
    thumbs: Vec<Thumb>,
    cells: Vec<Cell>,
    content_h: i32,
    viewport_w: i32,
    viewport_h: i32,
    /// DPI-scaled cell geometry, fixed for the window's lifetime (it does not
    /// handle WM_DPICHANGED). Every layout/hit-test/repaint after creation
    /// must read these rather than the logical CELL_*/MARGIN/GAP constants,
    /// or a click stops lining up with what is drawn (cell_w/cell_h) or the
    /// gutters stay logical-sized while the cards around them scale
    /// (margin/gap) on any non-100% monitor.
    cell_w: i32,
    cell_h: i32,
    cell_img_h: i32,
    margin: i32,
    gap: i32,
    scroll_y: i32,
    hover: i32,
    font: HFONT,
    font_small: HFONT,
    theme: crate::theme::Theme,
    /// Feedback text (e.g. "Copied") shown at the bottom, cleared by the
    /// window's periodic timer once it has been up for `STATUS_MS`.
    status: Option<(String, std::time::Instant)>,
    /// A click on this cell copies it once `CLICK_TIMER_ID` fires unless a
    /// double-click (or anything else that can invalidate the index) cancels
    /// it first — otherwise every double-click would also copy, once for
    /// each of a double-click's two WM_LBUTTONUP events.
    pending_click: Option<usize>,
    /// Only the latest Share click may update the browser or clipboard.
    pending_share: Option<u64>,
}

/// Singleton like Settings: reopening focuses the existing window instead of
/// stacking a second one.
static WINDOW: AtomicIsize = AtomicIsize::new(0);

unsafe fn state_of(hwnd: HWND) -> Option<&'static mut State> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
    ptr.as_mut()
}

fn relayout(state: &mut State) {
    let (cells, content_h) = grid_layout(
        state.thumbs.len(),
        state.viewport_w,
        state.cell_w,
        state.cell_h,
        state.margin,
        state.gap,
    );
    state.cells = cells;
    state.content_h = content_h;
    state.scroll_y = state.scroll_y.min(max_scroll(content_h, state.viewport_h)).max(0);
}

unsafe fn paint(hdc: HDC, state: &State) {
    let bg = CreateSolidBrush(state.theme.bg);
    FillRect(hdc, &RECT { left: 0, top: 0, right: state.viewport_w, bottom: state.viewport_h }, bg);
    let _ = DeleteObject(bg);
    SetBkMode(hdc, TRANSPARENT);

    if state.thumbs.is_empty() {
        SelectObject(hdc, state.font);
        SetTextColor(hdc, state.theme.muted);
        let mut msg = wide("No captures yet \u{2014} press PrtScn to make your first one.");
        let mut rc = RECT {
            left: state.margin,
            top: 0,
            right: state.viewport_w - state.margin,
            bottom: state.viewport_h,
        };
        DrawTextW(hdc, &mut msg, &mut rc, DT_CENTER | DT_VCENTER | DT_WORDBREAK);
        return;
    }

    // Created once and reused for every cell: the colors never change within
    // a paint, so recreating either per cell was pure repeated GDI overhead
    // on a repaint every hover-driven mouse move.
    let card = CreateSolidBrush(state.theme.chip);
    let hover_pen = CreatePen(windows::Win32::Graphics::Gdi::PS_SOLID, 2, state.theme.accent);
    let null_brush =
        windows::Win32::Graphics::Gdi::GetStockObject(windows::Win32::Graphics::Gdi::NULL_BRUSH);

    for (i, (thumb, cell)) in state.thumbs.iter().zip(&state.cells).enumerate() {
        let (cx, cy) = (cell.x, cell.y - state.scroll_y);
        if cy + state.cell_h < 0 || cy > state.viewport_h {
            continue; // off-screen: skip the decode-free but still real GDI cost
        }

        let hovered = i as i32 == state.hover;
        let card_rect =
            RECT { left: cx, top: cy, right: cx + state.cell_w, bottom: cy + state.cell_h };
        FillRect(hdc, &card_rect, card);
        if hovered {
            let old_pen = SelectObject(hdc, hover_pen);
            let old_brush = SelectObject(hdc, null_brush);
            let _ = RoundRect(hdc, cx, cy, cx + state.cell_w, cy + state.cell_h, 8, 8);
            SelectObject(hdc, old_pen);
            SelectObject(hdc, old_brush);
        }

        // Letterboxed, centered within the image area.
        let ix = cx + (state.cell_w - thumb.img_w) / 2;
        let iy = cy + (state.cell_img_h - thumb.img_h) / 2;
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: thumb.img_w,
                biHeight: -thumb.img_h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        StretchDIBits(
            hdc,
            ix,
            iy,
            thumb.img_w,
            thumb.img_h,
            0,
            0,
            thumb.img_w,
            thumb.img_h,
            Some(thumb.bgra.as_ptr() as *const _),
            &info,
            DIB_RGB_COLORS,
            SRCCOPY,
        );

        // Split the already DPI-scaled label band proportionally rather than
        // storing a second scaled field: SOURCE_LABEL_H : LABEL_H at 1x scale
        // is the ratio at every scale.
        let label_band = state.cell_h - state.cell_img_h;
        let source_h = label_band * SOURCE_LABEL_H / (SOURCE_LABEL_H + LABEL_H);

        SelectObject(hdc, state.font_small);
        SetTextColor(hdc, if hovered { state.theme.text } else { state.theme.muted });
        let mut source_label = wide(&thumb.source_label);
        let mut source_rect = RECT {
            left: cx + 6,
            top: cy + state.cell_img_h,
            right: cx + state.cell_w - 6,
            bottom: cy + state.cell_img_h + source_h,
        };
        DrawTextW(
            hdc,
            &mut source_label,
            &mut source_rect,
            // DT_NOPREFIX: a window title is arbitrary text, not a menu
            // label — a real title containing "&" (e.g. "Search & Rescue")
            // would otherwise have it eaten as an accelerator-prefix marker
            // by DrawTextW's default menu-string behavior.
            DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
        );

        SetTextColor(hdc, if hovered { state.theme.accent } else { state.theme.muted });
        let mut label = wide(&format_when(thumb.entry.saved_at));
        let mut label_rect = RECT {
            left: cx + 6,
            top: cy + state.cell_img_h + source_h,
            right: cx + state.cell_w - 6,
            bottom: cy + state.cell_h,
        };
        DrawTextW(hdc, &mut label, &mut label_rect, DT_CENTER | DT_SINGLELINE | DT_VCENTER);
    }

    if let Some((text, _)) = &state.status {
        SelectObject(hdc, state.font_small);
        SetTextColor(hdc, state.theme.accent);
        let mut t = wide(text);
        let mut rc = RECT {
            left: state.margin,
            top: state.viewport_h - 22,
            right: state.viewport_w - state.margin,
            bottom: state.viewport_h,
        };
        DrawTextW(hdc, &mut t, &mut rc, DT_CENTER | DT_SINGLELINE | DT_VCENTER);
    }

    // null_brush is a stock object owned by GDI and must not be deleted.
    let _ = DeleteObject(card);
    let _ = DeleteObject(hover_pen);
}

// The four actions below deliberately take plain data (an already-cloned
// `Entry`) rather than `&State`/`&mut State`. `warn` (and, for delete, the
// confirmation prompt) calls `MessageBoxW`, which pumps this window's own
// message queue while blocking — including WM_TIMER, which fires every
// `STATUS_TIMER_ID` tick. A `&mut State` (or even a `&State`) still in scope
// across that call would race a second, freshly fetched `state_of(hwnd)` from
// the timer handler: two live references to the same allocation, one of them
// mutable, which is undefined behavior regardless of whether it happens to
// work today. Every touch of `State` here happens either before or after the
// blocking call, never spanning it, via a fresh `state_of(hwnd)` each time.

unsafe fn copy_entry(hwnd: HWND, entry: &Entry) {
    let image = match image::open(&entry.path) {
        Ok(img) => img.to_rgba8(),
        Err(error) => return warn(hwnd, "This capture could not be opened.", &error),
    };
    if let Err(error) = crate::output::to_clipboard(&image, Some(&entry.path)) {
        return warn(hwnd, "This capture could not be copied.", &error);
    }
    if let Some(state) = state_of(hwnd) {
        state.status = Some(("Copied to clipboard".to_string(), std::time::Instant::now()));
        let _ = InvalidateRect(hwnd, None, false);
    }
}

unsafe fn open_in_editor(hwnd: HWND, monitor: HMONITOR, entry: &Entry) {
    let img = match image::open(&entry.path) {
        Ok(img) => img,
        Err(error) => return warn(hwnd, "This capture could not be opened.", &error),
    };
    let raw = img.to_rgba8();
    let styles = crate::style::variants(&raw);
    // Reopens un-matted: the file is already a finished composite, and
    // re-applying a matte on top of one would double-frame it. The editor
    // still offers every matte chip if the user wants one.
    let none = styles.iter().position(|s| s.name == "None").unwrap_or(0);
    let title = entry
        .path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Capture".to_string());
    if let Err(error) = crate::tweak::open(raw, styles, none, monitor, title) {
        warn(hwnd, "This capture could not be reopened.", &error);
    }
}

unsafe fn reveal_entry(entry: &Entry) {
    crate::output::reveal_in_explorer(&entry.path);
}

unsafe fn share_entry(hwnd: HWND, entry: &Entry) {
    if let Some(state) = state_of(hwnd) {
        state.status = Some(("Sharing\u{2026}".to_string(), std::time::Instant::now()));
        let _ = InvalidateRect(hwnd, None, false);
    }
    let request_id = crate::share::share_in_background(hwnd, entry.path.clone());
    if let Some(state) = state_of(hwnd) {
        state.pending_share = Some(request_id);
    }
}

unsafe fn delete_entry(hwnd: HWND, entry: &Entry) {
    let name = entry.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let prompt = HSTRING::from(format!("Delete {name}? This cannot be undone."));
    let confirmed = MessageBoxW(hwnd, PCWSTR(prompt.as_ptr()), w!("Matteshot"), MB_YESNO | MB_ICONWARNING) == IDYES;
    if !confirmed {
        return;
    }
    let result = crate::history::remove(&entry.path);
    let Some(state) = state_of(hwnd) else { return };
    match result {
        Ok(()) => {
            // Looked up by path rather than trusting a pre-dialog index: nothing
            // can actually resize `thumbs` while a modal MessageBoxW owns
            // input, but this makes that assumption unnecessary rather than
            // load-bearing.
            if let Some(i) = state.thumbs.iter().position(|t| t.entry.path == entry.path) {
                state.thumbs.remove(i);
            }
            // The whole index mapping can shift on a removal, so any stale
            // hover is cleared outright rather than only when it pointed at
            // the deleted cell.
            state.hover = -1;
            relayout(state);
            state.status = Some(("Deleted".to_string(), std::time::Instant::now()));
        }
        Err(error) => warn(hwnd, "This capture could not be deleted.", &error),
    }
    let _ = InvalidateRect(hwnd, None, false);
}

unsafe fn warn(hwnd: HWND, prefix: &str, error: &dyn std::fmt::Display) {
    crate::diagnostics::log("history: action failed");
    let message = HSTRING::from(format!("{prefix}\n\n{error}"));
    let _ = MessageBoxW(hwnd, PCWSTR(message.as_ptr()), w!("Matteshot"), MB_OK | MB_ICONWARNING);
}

/// No `State` reference is held across `TrackPopupMenu` below, for the same
/// reason spelled out above the action functions: it pumps WM_TIMER for this
/// window while blocked.
unsafe fn context_menu(hwnd: HWND, entry: Entry) {
    let Ok(menu) = CreatePopupMenu() else { return };
    let _ = AppendMenuW(menu, MF_STRING, 1, w!("Copy"));
    let _ = AppendMenuW(menu, MF_STRING, 2, w!("Open in editor"));
    let _ = AppendMenuW(menu, MF_STRING, 3, w!("Show in folder"));
    let _ = AppendMenuW(menu, MF_STRING, 4, w!("Share link"));
    let _ = AppendMenuW(menu, MF_STRING, 5, w!("Delete\u{2026}"));
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY, pt.x, pt.y, 0, hwnd, None);
    let _ = DestroyMenu(menu);
    match cmd.0 {
        1 => copy_entry(hwnd, &entry),
        2 => open_in_editor(hwnd, MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &entry),
        3 => reveal_entry(&entry),
        4 => share_entry(hwnd, &entry),
        5 => delete_entry(hwnd, &entry),
        _ => {}
    }
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
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                let mem = CreateCompatibleDC(hdc);
                let bmp = CreateCompatibleBitmap(hdc, state.viewport_w, state.viewport_h);
                let old = SelectObject(mem, bmp);
                paint(mem, state);
                let _ = BitBlt(hdc, 0, 0, state.viewport_w, state.viewport_h, mem, 0, 0, SRCCOPY);
                SelectObject(mem, old);
                let _ = DeleteObject(bmp);
                let _ = DeleteDC(mem);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if let Some(state) = state_of(hwnd) {
                let (mx, my) = ((lparam.0 & 0xFFFF) as i16 as i32, ((lparam.0 >> 16) & 0xFFFF) as i16 as i32);
                let hover = hit_test(&state.cells, state.cell_w, state.cell_h, state.scroll_y, mx, my);
                if hover != state.hover {
                    state.hover = hover;
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            if let Some(state) = state_of(hwnd) {
                let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16;
                let step = if delta > 0 { -WHEEL_STEP } else { WHEEL_STEP };
                state.scroll_y =
                    (state.scroll_y + step).clamp(0, max_scroll(state.content_h, state.viewport_h));
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if let Some(state) = state_of(hwnd) {
                let (mx, my) = ((lparam.0 & 0xFFFF) as i16 as i32, ((lparam.0 >> 16) & 0xFFFF) as i16 as i32);
                let hit = hit_test(&state.cells, state.cell_w, state.cell_h, state.scroll_y, mx, my);
                if hit >= 0 {
                    // Deferred rather than immediate: WM_LBUTTONDBLCLK
                    // cancels this before it fires, so a double-click opens
                    // the editor instead of also copying to the clipboard.
                    state.pending_click = Some(hit as usize);
                    let _ = SetTimer(hwnd, CLICK_TIMER_ID, GetDoubleClickTime(), None);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            let entry = state_of(hwnd).and_then(|state| {
                let _ = KillTimer(hwnd, CLICK_TIMER_ID);
                state.pending_click = None;
                let (mx, my) = ((lparam.0 & 0xFFFF) as i16 as i32, ((lparam.0 >> 16) & 0xFFFF) as i16 as i32);
                let hit = hit_test(&state.cells, state.cell_w, state.cell_h, state.scroll_y, mx, my);
                (hit >= 0).then(|| state.thumbs[hit as usize].entry.clone())
            });
            if let Some(entry) = entry {
                open_in_editor(hwnd, MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &entry);
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            let entry = state_of(hwnd).and_then(|state| {
                // A right-click cancels any pending single-click too: the
                // menu below can delete the very entry it would have copied.
                let _ = KillTimer(hwnd, CLICK_TIMER_ID);
                state.pending_click = None;
                let (mx, my) = ((lparam.0 & 0xFFFF) as i16 as i32, ((lparam.0 >> 16) & 0xFFFF) as i16 as i32);
                let hit = hit_test(&state.cells, state.cell_w, state.cell_h, state.scroll_y, mx, my);
                (hit >= 0).then(|| state.thumbs[hit as usize].entry.clone())
            });
            if let Some(entry) = entry {
                context_menu(hwnd, entry);
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if wparam.0 as u16 == VK_ESCAPE.0 {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == CLICK_TIMER_ID {
                let _ = KillTimer(hwnd, CLICK_TIMER_ID);
                let pending = state_of(hwnd).and_then(|state| {
                    let index = state.pending_click.take()?;
                    Some(state.thumbs[index].entry.clone())
                });
                if let Some(entry) = pending {
                    copy_entry(hwnd, &entry);
                }
            } else if let Some(state) = state_of(hwnd) {
                if let Some((_, shown_at)) = &state.status {
                    if shown_at.elapsed().as_millis() > STATUS_MS {
                        state.status = None;
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
            }
            LRESULT(0)
        }
        crate::share::WM_SHARE_COMPLETE => {
            if lparam.0 == 0 {
                return LRESULT(0);
            }
            let completion = *Box::from_raw(lparam.0 as *mut crate::share::ShareCompletion);
            let is_current = state_of(hwnd).is_some_and(|state| {
                crate::share::accept_completion(&mut state.pending_share, completion.request_id)
            });
            if !is_current {
                return LRESULT(0);
            }
            match completion.outcome {
                Ok(url) => {
                    // Opening the page is the visible confirmation that
                    // something happened; the clipboard copy alone was easy
                    // to miss entirely.
                    crate::output::open_url(&url);
                    match crate::output::text_to_clipboard(&url) {
                        Ok(()) => {
                            if let Some(state) = state_of(hwnd) {
                                state.status = Some((
                                    "Link copied to clipboard".to_string(),
                                    std::time::Instant::now(),
                                ));
                                let _ = InvalidateRect(hwnd, None, false);
                            }
                        }
                        Err(error) => warn(hwnd, "The share link could not be copied.", &error),
                    }
                }
                Err(message) => warn(hwnd, "This capture could not be shared.", &message),
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            let _ = KillTimer(hwnd, STATUS_TIMER_ID);
            let _ = KillTimer(hwnd, CLICK_TIMER_ID);
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

/// Reload the entry list and thumbnails into an already-open window.
unsafe fn reload(hwnd: HWND) {
    let Some(state) = state_of(hwnd) else { return };
    let _ = KillTimer(hwnd, CLICK_TIMER_ID);
    state.pending_click = None;
    let entries = list();
    state.thumbs = build_thumbs(&entries, state.cell_w, state.cell_img_h);
    state.scroll_y = 0;
    state.hover = -1;
    relayout(state);
    let _ = InvalidateRect(hwnd, None, false);
}

/// Open (or focus and refresh) the history browser. Non-modal; shares the
/// main thread's message loop, same as Settings.
pub fn open() -> Result<()> {
    unsafe {
        let existing = HWND(WINDOW.load(Ordering::SeqCst) as *mut _);
        if !existing.0.is_null() && IsWindow(existing).as_bool() {
            let _ = ShowWindow(existing, SW_RESTORE);
            let _ = SetWindowPos(existing, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
            let _ = SetWindowPos(existing, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
            let _ = SetForegroundWindow(existing);
            reload(existing);
            return Ok(());
        }

        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        let scale = crate::dpi::scale_for_point(cursor);
        let sc = |v: i32| (v as f32 * scale) as i32;
        let (cw, ch) = (sc(WIN_W), sc(WIN_H));

        let entries = list();
        let cell_w = sc(CELL_W);
        let cell_img_h = sc(CELL_IMG_H);
        let cell_h = sc(CELL_H);
        let margin = sc(MARGIN);
        let gap = sc(GAP);
        let thumbs = build_thumbs(&entries, cell_w, cell_img_h);
        let (cells, content_h) = grid_layout(thumbs.len(), cw, cell_w, cell_h, margin, gap);

        let monitor = MonitorFromPoint(cursor, windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST);
        let font = make_font(-sc(14));
        let font_small = make_font(-sc(12));

        let state = Box::new(State {
            thumbs,
            cells,
            content_h,
            viewport_w: cw,
            viewport_h: ch,
            cell_w,
            cell_h,
            cell_img_h,
            margin,
            gap,
            scroll_y: 0,
            hover: -1,
            font,
            font_small,
            theme: crate::theme::current(),
            status: None,
            pending_click: None,
            pending_share: None,
        });

        let hinstance = GetModuleHandleW(None).context("get app module for history")?;
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: crate::tray::app_icon(),
            lpszClassName: w!("matteshot_history"),
            ..Default::default()
        };
        RegisterClassW(&class);

        let leaked = Box::into_raw(state);
        let outer = crate::dpi::outer_bounds(
            RECT { left: 0, top: 0, right: cw, bottom: ch },
            WS_CAPTION | WS_SYSMENU,
            WS_EX_APPWINDOW,
            scale,
        );
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(monitor, &mut mi);
        let (ww, wh) = (outer.right - outer.left, outer.bottom - outer.top);
        let x = mi.rcWork.left + (mi.rcWork.right - mi.rcWork.left - ww) / 2;
        let y = mi.rcWork.top + (mi.rcWork.bottom - mi.rcWork.top - wh) / 2;

        let hwnd = match CreateWindowExW(
            WS_EX_APPWINDOW,
            w!("matteshot_history"),
            w!("Matteshot \u{2014} History"),
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
                return Err(error).context("create history window");
            }
        };

        crate::theme::apply_titlebar(hwnd, &crate::theme::current());
        WINDOW.store(hwnd.0 as isize, Ordering::SeqCst);
        let _ = SetTimer(hwnd, STATUS_TIMER_ID, 500, None);
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        // Mirrors settings::open: a tray menu just closed, so this process may
        // have lost foreground permission; the topmost toggle forces z-order
        // without needing activation rights.
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
        let _ = SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
        let _ = SetForegroundWindow(hwnd);
        eprintln!("history: opened with {} entries", entries.len());
        Ok(())
    }
}

/// Whether the browser is currently open, for diagnostic/CLI use.
pub fn is_open() -> bool {
    unsafe {
        let hwnd = HWND(WINDOW.load(Ordering::SeqCst) as *mut _);
        !hwnd.0.is_null() && IsWindow(hwnd).as_bool()
    }
}
