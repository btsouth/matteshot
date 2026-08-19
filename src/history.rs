//! In-app capture history: an index of saved screenshots, browsable and
//! reusable without a trip to Explorer.
//!
//! The log only ever indexes files that already live permanently in the
//! save folder — nothing here deletes a capture on its own. The retention
//! cap bounds how much the *browser* has to show and decode, not how long a
//! file survives on disk; only an explicit Delete removes one.
//!
//! `source` is the captured window title (or a region-size label), stored
//! in plaintext on this PC. Opening History drops entries whose files are
//! gone and rewrites that pruned list (SBS-765). Clear titles strips
//! `source` without deleting captures. Uninstall may offer to delete the
//! index; it never deletes the files it pointed at.

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

fn quarantine_path(path: &Path) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.with_extension(format!("json.corrupt-{unique}"))
}

/// Read the index ahead of a rewrite. `Ok` is a log that is safe to mutate
/// and save back: the real contents, a fresh log for a file that has never
/// existed, or a fresh log after corrupt bytes were quarantined aside. `Err`
/// is a transient read failure — the bytes on disk may be perfectly good, so
/// the caller must not write anything over them. Collapsing that case into
/// "empty" is how one locked read plus one save used to erase the entire
/// history.
fn load_for_mutation(path: &Path) -> Result<Log> {
    // Raw bytes, so a file that is not UTF-8 (a UTF-16 save from an editor,
    // mixed bytes) is a parse failure — corrupt, and quarantined below —
    // rather than an `InvalidData` read error that looks transient and would
    // block every future save while the bad file stays.
    let body = match std::fs::read(path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Log::default()),
        Err(error) => return Err(error).context("read history index"),
    };
    match serde_json::from_slice(&body) {
        Ok(log) => Ok(log),
        Err(error) => {
            // Corrupt is permanent, unlike a sharing violation: keep the
            // bytes for inspection, then deliberately start fresh rather
            // than failing every future save.
            let backup = quarantine_path(path);
            std::fs::rename(path, &backup).context("quarantine corrupt history index")?;
            crate::diagnostics::log(&format!("corrupt history index quarantined: {error}"));
            Ok(Log::default())
        }
    }
}

fn save_unlocked_at(path: &Path, log: &Log) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("create history directory")?;
    }
    let json = serde_json::to_vec_pretty(log).context("serialize history index")?;
    crate::state_lock::atomic_write(path, &json).context("write history index")
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
    let entry = Entry {
        path: path.to_path_buf(),
        saved_at: Local::now().timestamp(),
        width,
        height,
        style: style.to_string(),
        source: normalize_source(source),
    };
    let _guard = crate::state_lock::lock(HISTORY_MUTEX).ok();
    let Some(index) = history_path() else { return };
    append_at(&index, entry);
}

fn append_at(index: &Path, entry: Entry) {
    let mut log = match load_for_mutation(index) {
        Ok(log) => log,
        Err(error) => {
            // Transient: the capture PNG is saved either way; skipping this
            // one index entry is recoverable, overwriting the whole history
            // with it never was.
            crate::diagnostics::log(&format!(
                "capture not indexed, history unreadable this save: {error:#}"
            ));
            return;
        }
    };
    log.entries.push(entry);
    log.entries = trim_entries(log.entries, MAX_ENTRIES, |p| p.is_file());
    if let Err(error) = save_unlocked_at(index, &log) {
        crate::diagnostics::log(&format!("capture not indexed, history write failed: {error:#}"));
    }
}

/// Every entry whose file still exists, most recent first.
///
/// Missing files are dropped from the on-disk index as well (SBS-765).
/// A transient or corrupt read still shows an empty window this open and
/// leaves the bytes exactly as they were: quarantine-and-start-fresh
/// belongs to explicit mutating paths, which pair it with a replacement
/// write. Browsing only rewrites after a successful parse that actually
/// dropped something.
pub fn list() -> Vec<Entry> {
    let _guard = crate::state_lock::lock(HISTORY_MUTEX).ok();
    let Some(path) = history_path() else {
        return Vec::new();
    };
    list_at(&path)
}

fn list_at(index: &Path) -> Vec<Entry> {
    let body = match std::fs::read(index) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            crate::diagnostics::log(&format!(
                "history unreadable this open, index left unchanged: {error}"
            ));
            return Vec::new();
        }
    };
    let Ok(log) = serde_json::from_slice::<Log>(&body) else {
        return Vec::new();
    };
    let original_len = log.entries.len();
    let pruned = trim_entries(log.entries, MAX_ENTRIES, |p| p.is_file());
    if pruned.len() != original_len {
        if let Err(error) = save_unlocked_at(index, &Log { entries: pruned.clone() }) {
            crate::diagnostics::log(&format!("history prune not persisted: {error:#}"));
        }
    }
    let mut entries = pruned;
    entries.reverse();
    entries
}

/// True when `path` is inside `root` after both are canonicalized.
/// A path that cannot be resolved, or a root that cannot be resolved, is not owned.
fn path_is_under_root(path: &Path, root: &Path) -> bool {
    let Ok(path) = std::fs::canonicalize(path) else {
        return false;
    };
    let Ok(root) = std::fs::canonicalize(root) else {
        return false;
    };
    path.starts_with(root)
}

fn path_is_owned_capture(path: &Path, roots: &[impl AsRef<Path>]) -> bool {
    roots.iter().any(|root| path_is_under_root(path, root.as_ref()))
}

enum HistoryPathDisposition {
    Owned,
    UnownedRefused,
}

/// File name only: diagnostics::report() embeds this log and promises no save path.
fn history_event_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unnamed".into())
}

fn take_owned_history_path_at(
    path: &Path,
    roots: &[PathBuf],
) -> HistoryPathDisposition {
    if path_is_owned_capture(path, roots) {
        return HistoryPathDisposition::Owned;
    }
    // History is an index; a stale or hand-edited entry must not be
    // uploaded, copied, opened, or handed to Explorer. The row stays: a
    // previous save folder is still a real capture, and a canonicalize
    // failure is not proof the file is foreign.
    crate::diagnostics::log(&format!(
        "history path outside save/video folders: {}",
        history_event_name(path)
    ));
    HistoryPathDisposition::UnownedRefused
}

fn take_owned_history_path(path: &Path) -> Result<HistoryPathDisposition> {
    let config = crate::config::Config::try_load().inspect_err(|_| {
        crate::diagnostics::log("history ownership check skipped because config could not be loaded");
    })?;
    let roots = [config.save_dir(), config.video_dir()];
    Ok(take_owned_history_path_at(path, &roots))
}

fn stable_canonical(path: &Path) -> Option<PathBuf> {
    let first = std::fs::canonicalize(path).ok()?;
    let second = std::fs::canonicalize(path).ok()?;
    (first == second).then_some(first)
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
}

fn parent_is_under_any_root(path: &Path, roots: &[impl AsRef<Path>]) -> bool {
    path.parent().is_some_and(|parent| {
        roots
            .iter()
            .any(|root| path_is_under_root(parent, root.as_ref()))
    })
}

fn remove_at(index: &Path, path: &Path, roots: &[PathBuf]) -> Result<()> {
    // Load first: a Delete that cannot update the index must not unlink the file.
    let mut log = load_for_mutation(index)?;
    let unlink = if is_symlink(path) && parent_is_under_any_root(path, roots) {
        Some(path.to_path_buf())
    } else {
        match stable_canonical(path) {
            Some(canonical) if path_is_owned_capture(&canonical, roots) => Some(canonical),
            _ => None,
        }
    };
    if let Some(target) = unlink {
        if !is_symlink(path) && !path_still_matches_canonical(path, &target) {
            crate::diagnostics::log(&format!(
                "history delete skipped because the path changed: {}",
                history_event_name(path)
            ));
        } else {
            match std::fs::remove_file(&target) {
                Ok(()) => {}
                // Already gone (deleted outside the app, or a repeat click racing
                // its own first Delete): the index is just stale, so finish
                // dropping the entry instead of reporting a failure the user has no
                // way to act on.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("delete capture"),
            }
        }
    } else {
        // History is an index, not a file manager: a stale or hand-edited
        // entry must not unlink a file outside the save/video folders.
        crate::diagnostics::log(&format!(
            "history delete skipped file outside save/video folders: {}",
            history_event_name(path)
        ));
    }
    log.entries.retain(|e| e.path != path);
    save_unlocked_at(index, &log)
}

fn path_still_matches_canonical(path: &Path, expected: &Path) -> bool {
    std::fs::canonicalize(path).ok().as_deref() == Some(expected)
}

/// Delete the file and drop it from history. Unlike `record`, errors surface:
/// a Delete click that silently failed would look like it had worked.
pub fn remove(path: &Path) -> Result<()> {
    let _guard = crate::state_lock::lock(HISTORY_MUTEX).ok();
    let config = crate::config::Config::try_load().context("config could not be loaded")?;
    let roots = [config.save_dir(), config.video_dir()];
    let index = history_path().context("Windows has no application data directory")?;
    remove_at(&index, path, &roots)
}

/// Strip stored window titles from the index. Capture files stay on disk.
/// Missing files are dropped in the same rewrite so a title cannot linger
/// on an already-gone path.
pub fn clear_source_metadata() -> Result<usize> {
    let _guard = crate::state_lock::lock(HISTORY_MUTEX).ok();
    let index = history_path().context("Windows has no application data directory")?;
    clear_source_metadata_at(&index)
}

fn clear_source_metadata_at(index: &Path) -> Result<usize> {
    let mut log = load_for_mutation(index)?;
    let cleared = log
        .entries
        .iter()
        .filter(|entry| entry.source.as_ref().is_some_and(|source| !source.is_empty()))
        .count();
    for entry in &mut log.entries {
        entry.source = None;
    }
    let before = log.entries.len();
    log.entries = trim_entries(std::mem::take(&mut log.entries), MAX_ENTRIES, |p| p.is_file());
    if cleared > 0 || log.entries.len() != before {
        save_unlocked_at(index, &log)?;
    }
    Ok(cleared)
}

/// Files uninstall / data-removal may delete after an explicit yes.
/// Capture files are never in this list — only the index, a leftover
/// atomic-write temp, and quarantined copies, all under the app-data folder.
fn is_history_metadata_name(name: &str) -> bool {
    name == "history.json"
        || name == "history.json.tmp"
        || name.starts_with("history.json.corrupt-")
}

fn history_metadata_files(config_dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(config_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("read history directory"),
    };
    Ok(entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .is_some_and(|name| is_history_metadata_name(&name.to_string_lossy()))
        })
        .collect())
}

fn remove_history_metadata_in(dir: &Path) -> Result<()> {
    for path in history_metadata_files(dir)? {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("remove history metadata"),
        }
    }
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


    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("matteshot-history-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry_at(path: &Path) -> Entry {
        Entry {
            path: path.to_path_buf(),
            saved_at: 1,
            width: 10,
            height: 10,
            style: "plain".into(),
            source: None,
        }
    }

    /// A capture file the exists-filter in `trim_entries` will keep.
    fn capture(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"png").unwrap();
        path
    }

    #[test]
    fn a_missing_index_starts_fresh_and_appends_keep_prior_entries() {
        let dir = temp_dir("append");
        let index = dir.join("history.json");
        append_at(&index, entry_at(&capture(&dir, "a.png")));
        append_at(&index, entry_at(&capture(&dir, "b.png")));
        let log = load_for_mutation(&index).unwrap();
        assert_eq!(log.entries.len(), 2, "the second append lost the first entry");
    }

    #[test]
    fn corrupt_bytes_are_quarantined_before_the_recovery_write() {
        let dir = temp_dir("corrupt");
        let index = dir.join("history.json");
        std::fs::write(&index, b"{ not json").unwrap();
        append_at(&index, entry_at(&capture(&dir, "a.png")));
        // The deliberate fresh index holds the new capture...
        assert_eq!(load_for_mutation(&index).unwrap().entries.len(), 1);
        // ...and the original bytes were set aside, not destroyed.
        let quarantined = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.file_name().to_string_lossy().starts_with("history.json.corrupt-"))
            .expect("the corrupt index was quarantined");
        assert_eq!(std::fs::read(quarantined.path()).unwrap(), b"{ not json");
    }

    #[test]
    fn an_unreadable_index_is_never_overwritten() {
        let dir = temp_dir("unreadable");
        // A directory at the index path fails reads with something other
        // than NotFound — the same shape as a sharing/permission error, the
        // exact case that used to collapse into "empty history" and then be
        // saved over.
        let index = dir.join("history.json");
        std::fs::create_dir_all(&index).unwrap();
        append_at(&index, entry_at(&capture(&dir, "a.png")));
        assert!(index.is_dir(), "the unreadable index was replaced by a write");
    }

    #[test]
    fn a_non_utf8_index_is_corrupt_not_transient_so_saves_resume() {
        let dir = temp_dir("utf16");
        let index = dir.join("history.json");
        // Notepad's "Unicode" save: UTF-16 with a BOM. Not a lock, not a
        // permission problem — the bytes will never parse, so treating them
        // as transient would block every future save while they sat there.
        let mut utf16 = vec![0xFF, 0xFE];
        for unit in "{\"entries\":[]}".encode_utf16() {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(&index, &utf16).unwrap();
        append_at(&index, entry_at(&capture(&dir, "a.png")));
        append_at(&index, entry_at(&capture(&dir, "b.png")));
        assert_eq!(
            load_for_mutation(&index).unwrap().entries.len(),
            2,
            "saves never resumed after the undecodable index"
        );
        let quarantined = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.file_name().to_string_lossy().starts_with("history.json.corrupt-"))
            .expect("the undecodable index was quarantined");
        assert_eq!(std::fs::read(quarantined.path()).unwrap(), utf16);
    }

    #[test]
    fn a_failed_index_write_surfaces_instead_of_being_swallowed() {
        let dir = temp_dir("write-failure");
        let index = dir.join("history.json");
        // A directory squatting on the atomic write's temporary path is the
        // simplest way to make the replace fail; `remove` propagates this so
        // a Delete whose index write failed does not report success while
        // the stale entry stays.
        std::fs::create_dir_all(dir.join("history.json.tmp")).unwrap();
        assert!(save_unlocked_at(&index, &Log::default()).is_err());
        assert!(!index.exists());
    }

    /// A prefix lookalike (`root-evil`) or a `..` escape must not count as inside `root`.
    #[test]
    fn path_is_under_root_accepts_a_real_file_inside_and_rejects_lookalikes() {
        let base = temp_dir("under-root");
        let root = base.join("root");
        let root_evil = base.join("root-evil");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&root_evil).unwrap();
        let inside = capture(&root, "shot.png");
        let sibling = capture(&root_evil, "shot.png");
        let via_dotdot = root.join("..").join("root-evil").join("shot.png");

        assert!(path_is_under_root(&inside, &root));
        assert!(!path_is_under_root(&sibling, &root), "root-evil must not match root");
        assert!(!path_is_under_root(&via_dotdot, &root), "`..` that lands outside is not owned");
        assert!(!path_is_under_root(&inside, &base.join("no-such-root")));
        assert!(!path_is_under_root(&root.join("missing.png"), &root));
    }

    /// Ownership is the current save_dir or video_dir, not "any existing file".
    #[test]
    fn a_capture_is_owned_if_it_lives_under_save_dir_or_video_dir() {
        let base = temp_dir("owned");
        let save = base.join("save");
        let video = base.join("video");
        let other = base.join("other");
        std::fs::create_dir_all(&save).unwrap();
        std::fs::create_dir_all(&video).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let in_save = capture(&save, "a.png");
        let in_video = capture(&video, "b.mp4");
        let in_other = capture(&other, "c.png");
        let roots = [save, video];

        assert!(path_is_owned_capture(&in_save, &roots));
        assert!(path_is_owned_capture(&in_video, &roots));
        assert!(!path_is_owned_capture(&in_other, &roots));
    }

    #[test]
    fn history_event_name_is_a_file_name_not_a_drive_or_unc_path() {
        let drive = history_event_name(Path::new(r"C:\Users\alex\Pictures\Matteshot\shot.png"));
        assert_eq!(drive, "shot.png");
        assert!(!drive.contains(':'));
        assert!(!drive.contains('\\'));
        let unc = history_event_name(Path::new(r"\\nas\share\secret.txt"));
        assert_eq!(unc, "secret.txt");
        assert!(!unc.contains('\\'));
    }

    /// Share/copy/open/reveal refuse an outside path without rewriting history:
    /// a previous save folder is still a real capture.
    #[test]
    fn an_unowned_history_path_is_refused_and_the_file_and_index_are_left_alone() {
        let dir = temp_dir("take-unowned");
        let save = dir.join("save");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&save).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let foreign = capture(&outside, "win.ini");
        let index = dir.join("history.json");
        append_at(&index, entry_at(&foreign));

        let disposition = take_owned_history_path_at(&foreign, &[save]);
        assert!(
            matches!(disposition, HistoryPathDisposition::UnownedRefused),
            "an outside path must not surface as owned"
        );
        assert!(foreign.is_file(), "a file outside the save folder must stay on disk");
        assert_eq!(
            load_for_mutation(&index).unwrap().entries.len(),
            1,
            "changing folders must not wipe a previous-folder capture from the index"
        );
    }

    /// An owned capture stays on disk and in the index so the action can proceed.
    #[test]
    fn an_owned_save_dir_path_is_left_in_the_index() {
        let dir = temp_dir("take-owned-save");
        let save = dir.join("save");
        std::fs::create_dir_all(&save).unwrap();
        let shot = capture(&save, "a.png");
        let index = dir.join("history.json");
        append_at(&index, entry_at(&shot));

        let disposition = take_owned_history_path_at(&shot, &[save]);
        assert!(matches!(disposition, HistoryPathDisposition::Owned));
        assert!(shot.is_file(), "an owned capture must not be unlinked");
        assert_eq!(
            load_for_mutation(&index).unwrap().entries.len(),
            1,
            "an owned path must stay in the index"
        );
    }

    /// Recordings live under video_dir; that folder is an allowed root, not a second-class one.
    #[test]
    fn a_file_inside_video_dir_is_owned_even_when_outside_save_dir() {
        let dir = temp_dir("take-owned-video");
        let save = dir.join("save");
        let video = dir.join("video");
        std::fs::create_dir_all(&save).unwrap();
        std::fs::create_dir_all(&video).unwrap();
        let rec = capture(&video, "clip.mp4");
        let index = dir.join("history.json");
        append_at(&index, entry_at(&rec));

        let disposition = take_owned_history_path_at(&rec, &[save, video]);
        assert!(matches!(disposition, HistoryPathDisposition::Owned));
        assert!(rec.is_file());
        assert_eq!(load_for_mutation(&index).unwrap().entries.len(), 1);
    }

    /// A `..` path that resolves outside the save folder must not be treated as owned.
    #[test]
    fn a_path_that_escapes_the_save_folder_with_dotdot_is_unowned() {
        let dir = temp_dir("take-escape");
        let save = dir.join("save");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&save).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let foreign = capture(&outside, "secret.txt");
        let escaped = save.join("..").join("outside").join("secret.txt");
        let index = dir.join("history.json");
        append_at(&index, entry_at(&escaped));

        let disposition = take_owned_history_path_at(&escaped, &[save]);
        assert!(matches!(disposition, HistoryPathDisposition::UnownedRefused));
        assert!(foreign.is_file(), "a `..` escape must not unlink the foreign file");
        assert_eq!(
            load_for_mutation(&index).unwrap().entries.len(),
            1,
            "refusing the action must not wipe the index row"
        );
    }

    #[test]
    fn a_missing_file_is_unowned_without_being_treated_as_owned() {
        let dir = temp_dir("take-missing");
        let save = dir.join("save");
        std::fs::create_dir_all(&save).unwrap();
        let missing = save.join("gone.png");

        let disposition = take_owned_history_path_at(&missing, &[save]);
        assert!(matches!(disposition, HistoryPathDisposition::UnownedRefused));
    }

    #[test]
    fn an_unowned_history_entry_is_dropped_without_deleting_the_file() {
        let dir = temp_dir("remove-unowned");
        let save = dir.join("save");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&save).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let foreign = capture(&outside, "win.ini");
        let index = dir.join("history.json");
        append_at(&index, entry_at(&foreign));

        assert!(remove_at(&index, &foreign, &[save]).is_ok());
        assert!(foreign.is_file(), "a file outside the save folder must stay on disk");
        assert!(
            load_for_mutation(&index).unwrap().entries.is_empty(),
            "the stale index row is dropped even when the file is left alone"
        );
    }

    #[test]
    fn a_file_inside_the_save_folder_is_unlinked_and_dropped() {
        let dir = temp_dir("remove-owned");
        let save = dir.join("save");
        std::fs::create_dir_all(&save).unwrap();
        let shot = capture(&save, "a.png");
        let index = dir.join("history.json");
        append_at(&index, entry_at(&shot));

        assert!(remove_at(&index, &shot, &[save]).is_ok());
        assert!(!shot.exists());
        assert!(load_for_mutation(&index).unwrap().entries.is_empty());
    }

    #[test]
    fn a_file_inside_the_video_folder_is_unlinked_even_when_outside_save_dir() {
        let dir = temp_dir("remove-video");
        let save = dir.join("save");
        let video = dir.join("video");
        std::fs::create_dir_all(&save).unwrap();
        std::fs::create_dir_all(&video).unwrap();
        let rec = capture(&video, "clip.mp4");
        let index = dir.join("history.json");
        append_at(&index, entry_at(&rec));

        assert!(remove_at(&index, &rec, &[save, video]).is_ok());
        assert!(!rec.exists(), "recordings under video_dir are an allowed root");
        assert!(load_for_mutation(&index).unwrap().entries.is_empty());
    }

    #[test]
    fn a_path_that_escapes_the_save_folder_with_dotdot_is_not_unlinked() {
        let dir = temp_dir("remove-escape");
        let save = dir.join("save");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&save).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let foreign = capture(&outside, "secret.txt");
        let escaped = save.join("..").join("outside").join("secret.txt");
        let index = dir.join("history.json");
        append_at(&index, entry_at(&escaped));

        assert!(remove_at(&index, &escaped, &[save]).is_ok());
        assert!(foreign.is_file(), "a `..` escape must not unlink the foreign file");
        assert!(load_for_mutation(&index).unwrap().entries.is_empty());
    }

    #[test]
    fn a_symlink_inside_the_save_folder_is_unlinked_without_touching_its_target() {
        let dir = temp_dir("remove-symlink");
        let save = dir.join("save");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&save).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let target = capture(&outside, "secret.txt");
        let link = save.join("link.png");
        if std::os::windows::fs::symlink_file(&target, &link).is_err() {
            eprintln!("skipping: creating a file symlink requires privilege");
            return;
        }
        let index = dir.join("history.json");
        append_at(&index, entry_at(&link));

        assert!(remove_at(&index, &link, &[save]).is_ok());
        assert!(
            !link.exists(),
            "the symlink inside save_dir must be unlinked"
        );
        assert!(
            target.is_file(),
            "the outside target must not be deleted"
        );
        assert!(load_for_mutation(&index).unwrap().entries.is_empty());
    }

    #[test]
    fn a_symlink_to_another_file_in_the_save_folder_does_not_delete_the_target() {
        let dir = temp_dir("remove-symlink-inside");
        let save = dir.join("save");
        std::fs::create_dir_all(&save).unwrap();
        let target = capture(&save, "real.png");
        let link = save.join("link.png");
        if std::os::windows::fs::symlink_file(&target, &link).is_err() {
            eprintln!("skipping: creating a file symlink requires privilege");
            return;
        }
        let index = dir.join("history.json");
        append_at(&index, entry_at(&link));

        assert!(remove_at(&index, &link, &[save]).is_ok());
        assert!(!link.exists(), "the symlink must be unlinked");
        assert!(
            target.is_file(),
            "an in-folder symlink must not delete its target"
        );
        assert!(load_for_mutation(&index).unwrap().entries.is_empty());
    }

    #[test]
    fn an_unreadable_index_does_not_unlink_the_file() {
        let dir = temp_dir("remove-unreadable-index");
        let save = dir.join("save");
        std::fs::create_dir_all(&save).unwrap();
        let shot = capture(&save, "a.png");
        let index = dir.join("history.json");
        std::fs::create_dir_all(&index).unwrap();

        assert!(remove_at(&index, &shot, &[save]).is_err());
        assert!(shot.is_file(), "a failed index load must leave the capture");
    }

    #[test]
    fn path_still_matches_canonical_is_false_after_the_file_moves() {
        let dir = temp_dir("canonical-moved");
        std::fs::create_dir_all(&dir).unwrap();
        let original = capture(&dir, "a.png");
        let canonical = std::fs::canonicalize(&original).unwrap();
        assert!(path_still_matches_canonical(&original, &canonical));
        let moved = dir.join("b.png");
        std::fs::rename(&original, &moved).unwrap();
        assert!(!path_still_matches_canonical(&original, &canonical));
        assert!(moved.is_file());
    }

    #[test]
    fn a_directory_swap_of_save_dir_does_not_delete_an_outside_file() {
        let dir = temp_dir("remove-swap");
        let save = dir.join("save");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&save).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let shot = capture(&save, "a.png");
        let foreign = capture(&outside, "secret.txt");
        let index = dir.join("history.json");
        append_at(&index, entry_at(&shot));

        let real_save = dir.join("save.real");
        std::fs::rename(&save, &real_save).unwrap();
        if std::os::windows::fs::symlink_dir(&outside, &save).is_err() {
            let _ = std::fs::rename(&real_save, &save);
            eprintln!("skipping: creating a directory symlink requires privilege");
            return;
        }

        let swapped = save.join("a.png");
        assert!(remove_at(&index, &swapped, std::slice::from_ref(&real_save)).is_ok());
        assert!(
            foreign.is_file(),
            "a swapped save_dir must not unlink an outside file"
        );
        let _ = std::fs::remove_dir(&save);
        let _ = std::fs::rename(&real_save, &save);
    }

    fn entry_with_source(path: &Path, source: &str) -> Entry {
        let mut entry = entry_at(path);
        entry.source = Some(source.to_string());
        entry
    }

    fn write_log(index: &Path, entries: Vec<Entry>) {
        save_unlocked_at(index, &Log { entries }).unwrap();
    }

    #[test]
    fn history_metadata_names_are_the_index_temp_and_quarantine_only() {
        assert!(is_history_metadata_name("history.json"));
        assert!(is_history_metadata_name("history.json.tmp"));
        assert!(is_history_metadata_name("history.json.corrupt-1"));
        assert!(!is_history_metadata_name("config.json"));
        assert!(!is_history_metadata_name("license.json"));
        assert!(!is_history_metadata_name("shot.png"));
        assert!(!is_history_metadata_name("history.json.bak"));
    }

    /// SBS-765: browsing used to hide a missing file only in memory, so
    /// its window title stayed in history.json until the next save.
    #[test]
    fn opening_history_persists_a_missing_file_drop() {
        let dir = temp_dir("list-persist-prune");
        let kept = capture(&dir, "kept.png");
        let gone = capture(&dir, "gone.png");
        let index = dir.join("history.json");
        write_log(
            &index,
            vec![
                entry_with_source(&kept, "Confidential.docx"),
                entry_with_source(&gone, "Secret subject"),
            ],
        );
        std::fs::remove_file(&gone).unwrap();

        let listed = list_at(&index);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, kept);
        assert_eq!(listed[0].source.as_deref(), Some("Confidential.docx"));

        let on_disk = load_for_mutation(&index).unwrap();
        assert_eq!(on_disk.entries.len(), 1, "the missing file's title stayed on disk");
        assert_eq!(on_disk.entries[0].path, kept);
        assert!(kept.is_file(), "persist-prune must not delete the remaining capture");
    }

    #[test]
    fn opening_history_does_not_create_an_index_when_none_exists() {
        let dir = temp_dir("list-missing-index");
        let index = dir.join("history.json");
        assert!(list_at(&index).is_empty());
        assert!(!index.exists(), "a first History open must not mint an empty index");
    }

    #[test]
    fn opening_history_does_not_rewrite_when_every_file_still_exists() {
        let dir = temp_dir("list-no-rewrite");
        let shot = capture(&dir, "a.png");
        let index = dir.join("history.json");
        write_log(&index, vec![entry_with_source(&shot, "Notepad")]);
        let before = std::fs::read(&index).unwrap();
        let listed = list_at(&index);
        assert_eq!(listed.len(), 1);
        assert_eq!(std::fs::read(&index).unwrap(), before);
    }

    #[test]
    fn an_unreadable_index_is_not_pruned_over_on_open() {
        let dir = temp_dir("list-unreadable");
        let index = dir.join("history.json");
        std::fs::create_dir_all(&index).unwrap();
        assert!(list_at(&index).is_empty());
        assert!(index.is_dir(), "a transient/unreadable index must not be replaced");
    }

    #[test]
    fn a_corrupt_index_is_left_in_place_when_history_opens() {
        let dir = temp_dir("list-corrupt");
        let index = dir.join("history.json");
        std::fs::write(&index, b"{ not json").unwrap();
        assert!(list_at(&index).is_empty());
        assert_eq!(std::fs::read(&index).unwrap(), b"{ not json");
    }

    /// SBS-765: Clear History strips titles and leaves the files.
    #[test]
    fn clear_source_metadata_strips_titles_and_leaves_files() {
        let dir = temp_dir("clear-titles");
        let a = capture(&dir, "a.png");
        let b = capture(&dir, "b.png");
        let index = dir.join("history.json");
        write_log(
            &index,
            vec![
                entry_with_source(&a, "Inbox — Q3 plan"),
                entry_with_source(&b, "https://internal.example/doc"),
            ],
        );

        assert_eq!(clear_source_metadata_at(&index).unwrap(), 2);
        assert!(a.is_file());
        assert!(b.is_file());
        let log = load_for_mutation(&index).unwrap();
        assert_eq!(log.entries.len(), 2);
        assert!(log.entries.iter().all(|entry| entry.source.is_none()));
        assert_eq!(log.entries[0].path, a);
        assert_eq!(log.entries[1].path, b);
    }

    #[test]
    fn clear_source_metadata_also_drops_missing_files() {
        let dir = temp_dir("clear-and-prune");
        let kept = capture(&dir, "kept.png");
        let gone = capture(&dir, "gone.png");
        let index = dir.join("history.json");
        write_log(
            &index,
            vec![
                entry_with_source(&kept, "Kept title"),
                entry_with_source(&gone, "Gone title"),
            ],
        );
        std::fs::remove_file(&gone).unwrap();

        assert_eq!(clear_source_metadata_at(&index).unwrap(), 2);
        let log = load_for_mutation(&index).unwrap();
        assert_eq!(log.entries.len(), 1);
        assert_eq!(log.entries[0].path, kept);
        assert!(log.entries[0].source.is_none());
        assert!(kept.is_file());
    }

    #[test]
    fn clear_source_metadata_does_not_write_when_nothing_changed() {
        let dir = temp_dir("clear-noop");
        let shot = capture(&dir, "a.png");
        let index = dir.join("history.json");
        write_log(&index, vec![entry_at(&shot)]);
        let before = std::fs::read(&index).unwrap();
        assert_eq!(clear_source_metadata_at(&index).unwrap(), 0);
        assert_eq!(std::fs::read(&index).unwrap(), before);
        assert!(shot.is_file());
    }

    #[test]
    fn an_unreadable_index_is_not_cleared_over() {
        let dir = temp_dir("clear-unreadable");
        let index = dir.join("history.json");
        std::fs::create_dir_all(&index).unwrap();
        assert!(clear_source_metadata_at(&index).is_err());
        assert!(index.is_dir());
    }

    /// SBS-765: data-removal deletes only the index (and quarantines), never captures.
    #[test]
    fn remove_history_metadata_deletes_index_and_quarantine_not_captures_or_config() {
        let dir = temp_dir("remove-metadata");
        let captures = dir.join("captures");
        std::fs::create_dir_all(&captures).unwrap();
        let shot = capture(&captures, "shot.png");
        let index = dir.join("history.json");
        write_log(&index, vec![entry_with_source(&shot, "Payroll.xlsx")]);
        let quarantined = dir.join("history.json.corrupt-1");
        std::fs::write(&quarantined, b"{ not json").unwrap();
        let config = dir.join("config.json");
        std::fs::write(&config, b"{}").unwrap();
        let license = dir.join("license.json");
        std::fs::write(&license, b"{}").unwrap();
        let tmp = dir.join("history.json.tmp");
        std::fs::write(&tmp, b"partial").unwrap();

        let targets = history_metadata_files(&dir).unwrap();
        assert!(targets.iter().any(|p| p == &index));
        assert!(targets.iter().any(|p| p == &quarantined));
        assert!(targets.iter().any(|p| p == &tmp), "a leftover atomic-write temp still holds titles");
        assert!(!targets.iter().any(|p| p == &shot), "a capture must not be an uninstall target");
        assert!(!targets.iter().any(|p| p == &config));
        assert!(!targets.iter().any(|p| p == &license));

        remove_history_metadata_in(&dir).unwrap();
        assert!(!index.exists());
        assert!(!quarantined.exists());
        assert!(!tmp.exists());
        assert!(shot.is_file(), "data-removal must not delete the capture");
        assert!(config.is_file(), "data-removal must not delete config.json");
        assert!(license.is_file(), "data-removal must not delete license.json");
    }

    #[test]
    fn remove_history_metadata_is_ok_when_nothing_is_there() {
        let dir = temp_dir("remove-missing");
        remove_history_metadata_in(&dir).unwrap();
        assert!(dir.is_dir());
    }

    #[test]
    fn an_unreadable_history_dir_does_not_look_empty() {
        let dir = temp_dir("metadata-unreadable");
        let not_a_dir = dir.join("not-a-dir");
        std::fs::write(&not_a_dir, b"x").unwrap();
        assert!(
            history_metadata_files(&not_a_dir).is_err(),
            "a failed directory read is not 'no metadata'"
        );
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

    /// SBS-906: History must not offer Share to a trial (or any unpaid) user.
    #[test]
    fn history_omits_share_without_a_paid_license() {
        let licensed: Vec<_> = history_menu_items(true)
            .into_iter()
            .map(|(_, label)| label)
            .collect();
        let trial: Vec<_> = history_menu_items(false)
            .into_iter()
            .map(|(_, label)| label)
            .collect();
        assert!(licensed.contains(&"Share link"));
        assert!(!trial.contains(&"Share link"));
        assert_eq!(
            trial,
            ["Copy", "Open in editor", "Show in folder", "Delete\u{2026}"]
        );
        assert_eq!(
            history_menu_items(false)
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 5]
        );
    }

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

#[cfg(test)]
mod thumb_tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "matteshot-history-thumb-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn make_thumb_letterboxes_a_tall_scroll_into_the_cell() {
        let dir = temp_dir("cell");
        let path = dir.join("scroll.png");
        let mut img = RgbaImage::new(80, 4000);
        for pixel in img.pixels_mut() {
            *pixel = Rgba([8, 16, 24, 255]);
        }
        img.save(&path).unwrap();
        let thumb = make_thumb(
            &Entry {
                path,
                saved_at: 1,
                width: 80,
                height: 4000,
                style: "Deep".into(),
                source: Some("Notepad".into()),
            },
            CELL_W,
            CELL_IMG_H,
        )
        .expect("tall scroll should still produce a cell thumb");
        assert!(thumb.img_w <= CELL_W, "thumb width {} exceeds cell", thumb.img_w);
        assert!(thumb.img_h <= CELL_IMG_H, "thumb height {} exceeds cell", thumb.img_h);
        assert_eq!(
            thumb.bgra.len(),
            (thumb.img_w * thumb.img_h * 4) as usize
        );
        assert_eq!(thumb.source_label, "Notepad");
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
///
/// Decode is capped (SBS-913): a 40k-px scroll capture is downsampled while
/// the decoder still only holds one scanline, then letterboxed into the cell.
/// Copy / reopen-in-editor still load the file at native size; those are
/// one image on demand, not every indexed PNG on window open.
fn make_thumb(entry: &Entry, max_w: i32, max_h: i32) -> Option<Thumb> {
    let cap = crate::thumb_decode::THUMB_DECODE_MAX
        .max(max_w.max(1) as u32)
        .max(max_h.max(1) as u32);
    let img = crate::thumb_decode::decode_for_thumb(&entry.path, cap)?;
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

/// Returns true only when the path is an owned capture. An outside path is
/// refused without rewriting the index; a failed check leaves the action
/// unperformed.
unsafe fn require_owned_history_path(hwnd: HWND, path: &Path) -> bool {
    match take_owned_history_path(path) {
        Ok(HistoryPathDisposition::Owned) => true,
        Ok(HistoryPathDisposition::UnownedRefused) => {
            let message = HSTRING::from("This file is not in the Matteshot save folder.");
            let _ = MessageBoxW(hwnd, PCWSTR(message.as_ptr()), w!("Matteshot"), MB_OK | MB_ICONWARNING);
            false
        }
        Err(error) => {
            warn(hwnd, "This action could not be completed.", &error);
            false
        }
    }
}

unsafe fn copy_entry(hwnd: HWND, entry: &Entry) {
    if !require_owned_history_path(hwnd, &entry.path) {
        return;
    }
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
    if !require_owned_history_path(hwnd, &entry.path) {
        return;
    }
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

unsafe fn reveal_entry(hwnd: HWND, entry: &Entry) {
    if !require_owned_history_path(hwnd, &entry.path) {
        return;
    }
    crate::output::reveal_in_explorer(&entry.path);
}

unsafe fn share_entry(hwnd: HWND, entry: &Entry) {
    if let crate::share::ShareStart::Unavailable(reason) =
        crate::share::share_start(crate::license::can_share())
    {
        if let Some(state) = state_of(hwnd) {
            state.status = Some((reason.to_string(), std::time::Instant::now()));
            let _ = InvalidateRect(hwnd, None, false);
        }
        return;
    }
    if !require_owned_history_path(hwnd, &entry.path) {
        return;
    }
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

/// History context-menu rows. Share is omitted unless a paid license can
/// actually upload (SBS-906). Command ids stay stable so Delete is always 5.
fn history_menu_items(can_share: bool) -> Vec<(usize, &'static str)> {
    let mut items = vec![
        (1, "Copy"),
        (2, "Open in editor"),
        (3, "Show in folder"),
    ];
    if can_share {
        items.push((4, "Share link"));
    }
    items.push((5, "Delete\u{2026}"));
    items
}

/// No `State` reference is held across `TrackPopupMenu` below, for the same
/// reason spelled out above the action functions: it pumps WM_TIMER for this
/// window while blocked.
unsafe fn context_menu(hwnd: HWND, entry: Entry) {
    crate::theme::enable_dark_menus();
    let Ok(menu) = CreatePopupMenu() else { return };
    for (id, label) in history_menu_items(crate::license::can_share()) {
        let text = HSTRING::from(label);
        let _ = AppendMenuW(menu, MF_STRING, id, PCWSTR(text.as_ptr()));
    }
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY, pt.x, pt.y, 0, hwnd, None);
    let _ = DestroyMenu(menu);
    match cmd.0 {
        1 => copy_entry(hwnd, &entry),
        2 => open_in_editor(hwnd, MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &entry),
        3 => reveal_entry(hwnd, &entry),
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
            let Some(completion) = crate::share::take_completion(lparam.0 as u64, hwnd.0 as isize)
            else {
                return LRESULT(0);
            };
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
            crate::share::discard_window(hwnd.0 as isize);
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

/// Rebuild an already-open browser after an out-of-window metadata change
/// (Settings → Clear History titles). No-op when the window is closed.
pub fn reload_if_open() {
    unsafe {
        let hwnd = HWND(WINDOW.load(Ordering::SeqCst) as *mut _);
        if !hwnd.0.is_null() && IsWindow(hwnd).as_bool() {
            reload(hwnd);
        }
    }
}
