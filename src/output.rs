//! Delivering the result: clipboard, PNG on disk, optional editor handoff.

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use image::{ImageFormat, RgbaImage};
use windows::core::{w, HRESULT, HSTRING, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GlobalFree, ERROR_INVALID_PARAMETER, HANDLE, HWND, STILL_ACTIVE,
};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::SystemInformation::GetWindowsDirectoryW;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

pub const OUTPUT_ORIGINAL: u32 = 0;
pub const OUTPUT_EMAIL: u32 = 1600;
pub const OUTPUT_COMPACT: u32 = 1200;
pub const OUTPUT_CUSTOM_MIN: u32 = 320;
pub const OUTPUT_CUSTOM_MAX: u32 = 10_000;

const CF_DIB: u32 = 8;
const CF_HDROP: u32 = 15;

/// Cap the finished screenshot by its longest edge. The matte, annotations,
/// and shadows are resized together, and small captures are never enlarged.
pub fn resize_to_max_edge(img: &RgbaImage, max_edge: u32) -> RgbaImage {
    let (width, height) = resized_dimensions(img.width(), img.height(), max_edge);
    if (width, height) == img.dimensions() {
        return img.clone();
    }
    image::imageops::resize(img, width, height, image::imageops::FilterType::Lanczos3)
}

/// Dimensions produced by [`resize_to_max_edge`], without doing the work.
/// Interactive editors use this to show the exact final pixel size live.
pub fn resized_dimensions(width: u32, height: u32, max_edge: u32) -> (u32, u32) {
    let longest = width.max(height);
    if max_edge == OUTPUT_ORIGINAL || longest <= max_edge {
        return (width, height);
    }
    let scale = max_edge as f64 / longest as f64;
    (
        (width as f64 * scale).round().max(1.0) as u32,
        (height as f64 * scale).round().max(1.0) as u32,
    )
}

pub fn output_size_label(max_edge: u32) -> String {
    match max_edge {
        OUTPUT_ORIGINAL => "Original".into(),
        OUTPUT_EMAIL => "Email".into(),
        OUTPUT_COMPACT => "Compact".into(),
        value => format!("{value}px"),
    }
}

pub fn partial_video_path(destination: &Path, id: u64) -> PathBuf {
    let parent = destination.parent().unwrap_or_else(|| Path::new(""));
    let stem = destination
        .file_stem()
        .map(|value| value.to_string_lossy())
        .unwrap_or_default();
    parent.join(format!(
        "{stem}.partial-{}-{id}.mp4",
        std::process::id()
    ))
}

fn partial_recording_owner(name: &str) -> Option<u32> {
    let without_extension = name
        .strip_suffix(".mp4")
        .or_else(|| name.strip_suffix(".gif"))?;
    let (_, owner_and_id) = without_extension.rsplit_once(".partial-")?;
    let (owner, id) = owner_and_id.split_once('-')?;
    if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    owner.parse().ok()
}

/// What to do with one stale partial. Deletion must be earned, not the
/// default: only bytes proven undecodable may be removed.
enum PartialDisposition {
    /// Playable and moved to its recovery name.
    Recovered(PathBuf),
    /// Proven-undecodable bytes; safe to delete.
    Delete,
    /// Playable but not movable right now (name exhaustion, transient lock).
    /// Leave it exactly where it is; a later cleanup retries.
    Keep,
    /// The validator could not reach a verdict (a lock on the file, COM or
    /// Media Foundation unavailable). Not proven bad, so not deletable;
    /// a later cleanup retries.
    Unverified,
}

fn cleanup_stale_partials(
    dir: &Path,
    owner_of: fn(&str) -> Option<u32>,
    owner_may_be_running: impl Fn(u32) -> Option<bool>,
    disposition: impl Fn(&Path) -> PartialDisposition,
) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut handled = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(owner) = owner_of(name) else {
            continue;
        };
        let old = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= std::time::Duration::from_secs(24 * 60 * 60));
        if owner == std::process::id() && !old {
            continue;
        }
        // SBS-893: another instance may still be inside Finalize. Live owner
        // → skip (do not validate, do not delete). Unknown liveness → keep.
        // Unknown is not dead. Only a proven-dead owner may be classified.
        //
        // The 24h window applies to every PID, not just ours. Finalize is
        // bounded at 15 minutes, so a day-old partial whose owner still
        // looks live is a recycled PID — some other long-lived process now
        // holds that number. Without the window one such reuse strands a
        // playable recording for as long as that process runs.
        if owner != std::process::id() && !old {
            match owner_may_be_running(owner) {
                Some(true) => continue,
                None => {
                    crate::diagnostics::log(
                        "a stale partial's owner process could not be checked; keeping it",
                    );
                    continue;
                }
                Some(false) => {}
            }
        }
        // A stale partial is not automatically garbage: a finalize that
        // outlived its process can leave a complete, playable recording
        // behind. Offer it back before anything is deleted.
        match disposition(&path) {
            PartialDisposition::Recovered(recovered) => {
                // File name only: lifecycle events replay verbatim inside
                // the privacy-safe support report.
                crate::diagnostics::log(&format!(
                    "recovered a finished recording from a stale partial as {}",
                    recovered
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default()
                ));
                handled += 1;
            }
            PartialDisposition::Delete => {
                if std::fs::remove_file(&path).is_ok() {
                    handled += 1;
                }
            }
            PartialDisposition::Keep => {
                crate::diagnostics::log(
                    "a playable stale partial could not be recovered yet; keeping it",
                );
            }
            PartialDisposition::Unverified => {
                crate::diagnostics::log(
                    "a stale partial could not be checked for playability yet; keeping it",
                );
            }
        }
    }
    handled
}

/// The destination a recovered partial should surface under: the original
/// recording name with a `-recovered` marker, dodging collisions.
fn recovered_video_path(partial: &Path) -> Option<PathBuf> {
    let name = partial.file_name()?.to_str()?;
    // The trailing marker only: the original recording name may itself
    // contain ".partial-".
    let stem = name.rsplit_once(".partial-")?.0;
    let parent = partial.parent()?;
    (0..100)
        .map(|n| {
            parent.join(if n == 0 {
                format!("{stem}-recovered.mp4")
            } else {
                format!("{stem}-recovered-{n}.mp4")
            })
        })
        .find(|candidate| !candidate.exists())
}

fn video_partial_disposition(path: &Path) -> PartialDisposition {
    // Deletion only on proven-undecodable bytes; the validator decodes real
    // frames, so a truncated mid-write partial never passes. A check that
    // could not run — the file locked by an indexer, COM or Media Foundation
    // not up — proves nothing, so the file stays. Everything that fails
    // *after* validation — recovery-name exhaustion, a transient rename/lock
    // error — keeps the file too: it is user data that merely could not be
    // moved this time.
    if let Err(error) = crate::trim::validate_video(path) {
        return match crate::trim::validation_fault(&error) {
            crate::trim::ValidationFault::Undecodable => PartialDisposition::Delete,
            crate::trim::ValidationFault::Unavailable => PartialDisposition::Unverified,
        };
    }
    let Some(destination) = recovered_video_path(path) else {
        return PartialDisposition::Keep;
    };
    match std::fs::rename(path, &destination) {
        Ok(()) => PartialDisposition::Recovered(destination),
        Err(_) => PartialDisposition::Keep,
    }
}

/// Three states, not two (SBS-893). A query that cannot run is not "dead":
/// startup must not delete a mid-Finalize file on ACCESS_DENIED or any
/// OpenProcess failure that is not a clear "no such process".
fn process_may_be_running(pid: u32) -> Option<bool> {
    let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(handle) => handle,
        Err(error) => {
            return if error.code() == HRESULT::from_win32(ERROR_INVALID_PARAMETER.0) {
                Some(false)
            } else {
                None
            };
        }
    };
    let mut exit_code = 0u32;
    let queried = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
    let _ = unsafe { CloseHandle(handle) };
    match queried {
        Ok(()) => Some(exit_code == STILL_ACTIVE.0 as u32),
        Err(_) => None,
    }
}

/// Handle Matteshot's unmistakable incomplete-recording names: recover the
/// playable ones under a `-recovered` name, remove the rest. A partial from
/// this process may belong to another open editor, so it is retained unless
/// it is old enough to be from a reused process ID.
pub fn cleanup_stale_video_partials(dir: &Path) -> usize {
    cleanup_stale_partials(
        dir,
        partial_recording_owner,
        process_may_be_running,
        video_partial_disposition,
    )
}

struct ClipboardGuard;

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

/// Clipboard ownership is commonly held for a few milliseconds by chat apps,
/// Office, clipboard history, and remote-desktop helpers. Treat that as
/// contention, not as a failed capture.
fn open_clipboard() -> Result<ClipboardGuard> {
    let mut last_error = None;
    for attempt in 0..8u64 {
        match unsafe { OpenClipboard(HWND::default()) } {
            Ok(()) => return Ok(ClipboardGuard),
            Err(error) => last_error = Some(error),
        }
        if attempt < 7 {
            std::thread::sleep(std::time::Duration::from_millis(8 * (attempt + 1)));
        }
    }
    Err(last_error.unwrap_or_else(windows::core::Error::from_win32))
        .context("open clipboard after retries")
}

unsafe fn put_bytes(format: u32, bytes: &[u8]) -> Result<()> {
    let hmem = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
    let ptr = GlobalLock(hmem) as *mut u8;
    if ptr.is_null() {
        let _ = GlobalFree(hmem);
        bail!("lock clipboard memory");
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
    let _ = GlobalUnlock(hmem);
    if let Err(error) = SetClipboardData(format, HANDLE(hmem.0)) {
        // Ownership transfers to Windows only after SetClipboardData succeeds.
        let _ = GlobalFree(hmem);
        return Err(error).context("SetClipboardData");
    }
    Ok(())
}

/// Put the image on the clipboard as CF_DIB (universal), "PNG" (quality
/// paste in chat apps), and CF_HDROP for the saved file (paste as a file in
/// Explorer, Outlook, ticket systems).
pub fn to_clipboard(img: &RgbaImage, file: Option<&Path>) -> Result<()> {
    let (w, h) = (img.width() as usize, img.height() as usize);

    // 24bpp bottom-up DIB — the most compatible clipboard bitmap there is.
    let pitch = (w * 3 + 3) & !3;
    let mut dib = Vec::with_capacity(40 + pitch * h);
    let hdr: [u32; 3] = [40, w as u32, h as u32];
    dib.extend_from_slice(&hdr[0].to_le_bytes());
    dib.extend_from_slice(&hdr[1].to_le_bytes());
    dib.extend_from_slice(&hdr[2].to_le_bytes());
    dib.extend_from_slice(&1u16.to_le_bytes()); // planes
    dib.extend_from_slice(&24u16.to_le_bytes()); // bpp
    dib.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    dib.extend_from_slice(&((pitch * h) as u32).to_le_bytes());
    dib.extend_from_slice(&[0u8; 16]); // ppm x/y, clr used/important
    let raw = img.as_raw();
    for y in (0..h).rev() {
        let row_start = y * w * 4;
        let mut written = 0;
        for x in 0..w {
            let i = row_start + x * 4;
            dib.extend_from_slice(&[raw[i + 2], raw[i + 1], raw[i]]);
            written += 3;
        }
        while written < pitch {
            dib.push(0);
            written += 1;
        }
    }

    let mut png_bytes: Vec<u8> = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageFormat::Png)
        .context("encode png")?;

    let _clipboard = open_clipboard()?;
    unsafe {
        (|| -> Result<()> {
            EmptyClipboard().context("empty clipboard")?;
            put_bytes(CF_DIB, &dib)?;
            let png_fmt = RegisterClipboardFormatW(w!("PNG"));
            if png_fmt != 0 {
                let _ = put_bytes(png_fmt, &png_bytes);
            }
            if let Some(path) = file {
                // DROPFILES header (20 bytes, wide paths) + path + double null.
                let mut drop: Vec<u8> = Vec::new();
                drop.extend_from_slice(&20u32.to_le_bytes());
                drop.extend_from_slice(&[0u8; 8]); // pt
                drop.extend_from_slice(&0u32.to_le_bytes()); // fNC
                drop.extend_from_slice(&1u32.to_le_bytes()); // fWide
                for u in path.as_os_str().encode_wide() {
                    drop.extend_from_slice(&u.to_le_bytes());
                }
                drop.extend_from_slice(&[0, 0, 0, 0]); // terminator + list end
                let _ = put_bytes(CF_HDROP, &drop);
            }
            Ok(())
        })()
    }
}

/// `source` is whatever the capture is already labeled by — the captured
/// window's title, or a region's size — purely for the history browser to
/// show later; it never affects the file itself.
fn partial_png_path(destination: &Path) -> PathBuf {
    let name = destination.file_name().and_then(|name| name.to_str()).unwrap_or("capture.png");
    destination.with_file_name(format!(
        "{name}.matteshot-partial-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ))
}

fn partial_png_owner(name: &str) -> Option<u32> {
    let (finished_name, owner_and_id) = name.rsplit_once(".matteshot-partial-")?;
    if !finished_name.ends_with(".png") {
        return None;
    }
    let (owner, id) = owner_and_id.split_once('-')?;
    if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    owner.parse().ok()
}

/// A killed encoder can leave only the unmistakable staging name, never a
/// finished PNG. Remove staging files whose owning process is gone (or whose
/// PID has been reused after a day) when the resident starts again.
pub fn cleanup_stale_png_partials(dir: &Path) -> usize {
    // PNG partials are staged bytes mid-save with nothing to recover: the
    // finished capture either published or the save failed loudly.
    cleanup_stale_partials(
        dir,
        partial_png_owner,
        |_| Some(false),
        |_| PartialDisposition::Delete,
    )
}

fn publish_png_with(
    img: &RgbaImage,
    destination: &Path,
    rename: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> Result<()> {
    let partial = partial_png_path(destination);
    let staged = (|| {
        img.save_with_format(&partial, ImageFormat::Png).context("write partial png")?;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&partial)
            .context("open partial png for flush")?
            .sync_all()
            .context("flush partial png")?;
        Ok(())
    })();
    if let Err(error) = staged {
        let _ = std::fs::remove_file(&partial);
        return Err(error);
    }
    // The encoded, flushed capture is user data now. If publication fails,
    // preserve the unmistakable partial so recovery remains possible.
    rename(&partial, destination).context("publish png")
}

fn publish_png(img: &RgbaImage, destination: &Path) -> Result<()> {
    publish_png_with(img, destination, |from, to| std::fs::rename(from, to))
}

pub fn save_png(img: &RgbaImage, style_name: &str, dir: &Path, source: Option<&str>) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let name = format!(
        "matteshot-{}-{}.png",
        chrono::Local::now().format("%Y%m%d-%H%M%S%3f"),
        style_name.to_lowercase()
    );
    let path = dir.join(name);
    publish_png(img, &path)?;
    crate::history::record(&path, img.width(), img.height(), style_name, source);
    Ok(path)
}

/// A file on the clipboard (recordings): paste into chat, mail, Explorer.
pub fn file_to_clipboard(path: &Path) -> Result<()> {
    let mut drop: Vec<u8> = Vec::new();
    drop.extend_from_slice(&20u32.to_le_bytes());
    drop.extend_from_slice(&[0u8; 8]);
    drop.extend_from_slice(&0u32.to_le_bytes());
    drop.extend_from_slice(&1u32.to_le_bytes());
    for u in path.as_os_str().encode_wide() {
        drop.extend_from_slice(&u.to_le_bytes());
    }
    drop.extend_from_slice(&[0, 0, 0, 0]);
    let _clipboard = open_clipboard()?;
    unsafe { EmptyClipboard().context("empty clipboard")? };
    unsafe { put_bytes(CF_HDROP, &drop) }
}

/// Plain text to the clipboard (OCR results).
pub fn text_to_clipboard(text: &str) -> Result<()> {
    const CF_UNICODETEXT: u32 = 13;
    let mut wide: Vec<u8> = Vec::new();
    for u in text.encode_utf16() {
        wide.extend_from_slice(&u.to_le_bytes());
    }
    wide.extend_from_slice(&[0, 0]);
    let _clipboard = open_clipboard()?;
    unsafe { EmptyClipboard().context("empty clipboard")? };
    unsafe { put_bytes(CF_UNICODETEXT, &wide) }
}

/// Reveal a file in Explorer, selected.
///
/// Explorer is launched from the Windows directory, never by unqualified
/// name. A decoy `explorer.exe` in the process current directory must not
/// run (SBS-764). If the Windows directory cannot be read, reveal is
/// skipped rather than falling back to PATH search.
pub fn reveal_in_explorer(path: &Path) {
    let Some(exe) = reveal_explorer_exe(windows_directory().as_deref()) else {
        return;
    };
    let args = format!("/select,\"{}\"", path.display());
    let exe = HSTRING::from(exe.as_os_str());
    let args = HSTRING::from(args);
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(exe.as_ptr()),
            PCWSTR(args.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// Join a Windows-owned directory with a helper file name.
///
/// `file_name` must be a single name (`explorer.exe`). Callers pass a
/// directory they already trust — `GetWindowsDirectoryW`, not cwd.
fn trusted_helper_path(trusted_dir: &Path, file_name: &str) -> Option<PathBuf> {
    if !is_plain_helper_name(file_name) {
        return None;
    }
    Some(trusted_dir.join(file_name))
}

fn is_plain_helper_name(file_name: &str) -> bool {
    if file_name.is_empty() || file_name == "." || file_name == ".." {
        return false;
    }
    if file_name.bytes().any(|b| {
        matches!(
            b,
            b'/' | b'\\' | b':' | b'*' | b'?' | b'"' | b'<' | b'>' | b'|' | b'\0'
        )
    }) {
        return false;
    }
    Path::new(file_name).file_name().and_then(|n| n.to_str()) == Some(file_name)
}

/// Absolute `explorer.exe` under the Windows directory, or `None` when
/// that directory is unknown. `None` is not "search PATH".
fn reveal_explorer_exe(windows_dir: Option<&Path>) -> Option<PathBuf> {
    trusted_helper_path(windows_dir?, "explorer.exe")
}

fn windows_directory() -> Option<PathBuf> {
    // MAX_PATH. If the real path is longer, the first call returns the
    // required size and we retry. Empty/zero is unknown, not "search PATH".
    let mut buf = vec![0u16; 260];
    let mut n = unsafe { GetWindowsDirectoryW(Some(&mut buf)) };
    if n == 0 {
        return None;
    }
    if (n as usize) >= buf.len() {
        buf.resize(n as usize, 0);
        n = unsafe { GetWindowsDirectoryW(Some(&mut buf)) };
        if n == 0 || (n as usize) >= buf.len() {
            return None;
        }
    }
    buf.truncate(n as usize);
    if buf.is_empty() {
        return None;
    }
    Some(PathBuf::from(OsString::from_wide(&buf)))
}

/// Open the captures folder in Explorer.
pub fn open_folder(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(HSTRING::from(dir.as_os_str()).as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// Open a trusted HTTPS URL with the user's default browser.
pub fn open_url(url: &str) {
    let url = HSTRING::from(url);
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(url.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GenericImageView, Rgba};

    fn image(width: u32, height: u32) -> RgbaImage {
        RgbaImage::from_pixel(width, height, Rgba([30, 60, 90, 255]))
    }

    #[test]
    fn output_size_caps_landscape_longest_edge() {
        let resized = resize_to_max_edge(&image(2400, 1200), 1600);
        assert_eq!(resized.dimensions(), (1600, 800));
    }

    #[test]
    fn output_size_caps_portrait_longest_edge() {
        let resized = resize_to_max_edge(&image(1200, 2400), 1200);
        assert_eq!(resized.dimensions(), (600, 1200));
    }

    #[test]
    fn output_size_never_upscales_or_changes_original() {
        assert_eq!(resize_to_max_edge(&image(800, 600), 1600).dimensions(), (800, 600));
        assert_eq!(resize_to_max_edge(&image(2400, 1200), 0).dimensions(), (2400, 1200));
    }

    #[test]
    fn output_size_preview_matches_resize_rounding() {
        let source = image(2345, 1333);
        let expected = resized_dimensions(source.width(), source.height(), 1600);
        assert_eq!(resize_to_max_edge(&source, 1600).dimensions(), expected);
        assert_eq!(expected, (1600, 910));
    }

    #[test]
    fn partial_video_names_are_narrow_and_owner_aware() {
        assert_eq!(partial_recording_owner("clip.partial-123-9.mp4"), Some(123));
        assert_eq!(partial_recording_owner("clip.partial-123-9.gif"), Some(123));
        assert_eq!(partial_recording_owner("clip.partial-nope-9.mp4"), None);
        assert_eq!(partial_recording_owner("clip.partial-123-x.gif"), None);
        assert_eq!(partial_recording_owner("clip.mp4"), None);
        assert_eq!(partial_recording_owner("clip.gif"), None);
    }

    #[test]
    fn partial_png_names_are_narrow_and_owner_aware() {
        assert_eq!(partial_png_owner("capture.png.matteshot-partial-123-9"), Some(123));
        assert_eq!(partial_png_owner("capture.jpg.matteshot-partial-123-9"), None);
        assert_eq!(partial_png_owner("capture.png.matteshot-partial-nope-9"), None);
        assert_eq!(partial_png_owner("capture.png.partial-123-9"), None);
        assert_eq!(partial_png_owner("capture.png"), None);
    }

    #[test]
    fn png_is_complete_before_the_finished_name_appears() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "matteshot-png-publish-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let destination = dir.join("capture.png");

        publish_png(&image(17, 11), &destination).unwrap();

        assert_eq!(image::open(&destination).unwrap().dimensions(), (17, 11));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_file(destination).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn a_publish_failure_preserves_the_complete_partial() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "matteshot-png-publish-failure-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let destination = dir.join("capture.png");

        let result = publish_png_with(&image(17, 11), &destination, |_, _| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "destination is locked",
            ))
        });

        assert!(result.is_err());
        assert!(!destination.exists());
        let partial = std::fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
        assert_eq!(
            image::load_from_memory(&std::fs::read(&partial).unwrap()).unwrap().dimensions(),
            (17, 11)
        );
        std::fs::remove_file(partial).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn stale_partial_cleanup_does_not_touch_normal_or_live_files() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "matteshot-partial-cleanup-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let other_pid = std::process::id().wrapping_add(1).max(1);
        let stale = dir.join(format!("clip.partial-{other_pid}-1.mp4"));
        let stale_gif = dir.join(format!("clip.partial-{other_pid}-2.gif"));
        let live = partial_video_path(&dir.join("live.mp4"), 2);
        let live_gif = dir.join(format!(
            "live.partial-{}-3.gif",
            std::process::id()
        ));
        let normal = dir.join("normal.mp4");
        std::fs::write(&stale, b"stale").unwrap();
        std::fs::write(&stale_gif, b"stale gif").unwrap();
        std::fs::write(&live, b"live").unwrap();
        std::fs::write(&live_gif, b"live gif").unwrap();
        std::fs::write(&normal, b"normal").unwrap();

        // wrapping_add(1) is not guaranteed to be a dead PID (reuse). This
        // test pins disposition plumbing — other-PID undecodable bytes are
        // deleted, live/normal names are left — not process liveness.
        // Inject "known dead" so the old assertion still holds (SBS-893).
        assert_eq!(
            cleanup_stale_partials(
                &dir,
                partial_recording_owner,
                |_| Some(false),
                video_partial_disposition,
            ),
            2
        );
        assert!(!stale.exists());
        assert!(!stale_gif.exists());
        assert!(live.exists());
        assert!(live_gif.exists());
        assert!(normal.exists());

        std::fs::remove_file(live).unwrap();
        std::fs::remove_file(live_gif).unwrap();
        std::fs::remove_file(normal).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn stale_png_partial_cleanup_does_not_touch_finished_captures() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "matteshot-png-partial-cleanup-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let other_pid = std::process::id().wrapping_add(1).max(1);
        let stale = dir.join(format!("capture.png.matteshot-partial-{other_pid}-1"));
        let finished = dir.join("capture.png");
        std::fs::write(&stale, b"partial").unwrap();
        std::fs::write(&finished, b"finished").unwrap();

        assert_eq!(cleanup_stale_png_partials(&dir), 1);
        assert!(!stale.exists());
        assert!(finished.exists());

        std::fs::remove_file(finished).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn playable_stale_partials_are_recovered_under_the_original_name() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "matteshot-partial-recovery-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let other_pid = std::process::id().wrapping_add(1).max(1);
        let playable = dir.join(format!("clip.partial-{other_pid}-1.mp4"));
        std::fs::write(&playable, b"finished recording").unwrap();

        // The real disposition fn validates with Media Foundation; the
        // plumbing under test is that a recoverable partial is moved,
        // counted, and never deleted.
        let handled = cleanup_stale_partials(
            &dir,
            partial_recording_owner,
            |_| Some(false),
            |path| {
                let Some(destination) = recovered_video_path(path) else {
                    return PartialDisposition::Keep;
                };
                match std::fs::rename(path, &destination) {
                    Ok(()) => PartialDisposition::Recovered(destination),
                    Err(_) => PartialDisposition::Keep,
                }
            },
        );
        assert_eq!(handled, 1);
        assert!(!playable.exists());
        let recovered = dir.join("clip-recovered.mp4");
        assert!(recovered.exists(), "the playable partial was not recovered");
        assert_eq!(std::fs::read(&recovered).unwrap(), b"finished recording");

        std::fs::remove_file(recovered).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn a_playable_partial_that_cannot_move_is_kept_not_deleted() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "matteshot-partial-keep-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let other_pid = std::process::id().wrapping_add(1).max(1);
        let playable = dir.join(format!("clip.partial-{other_pid}-1.mp4"));
        std::fs::write(&playable, b"finished recording").unwrap();

        // Validation passed but the move could not happen (transient lock,
        // name exhaustion): the recording must survive in place.
        let handled = cleanup_stale_partials(
            &dir,
            partial_recording_owner,
            |_| Some(false),
            |_| PartialDisposition::Keep,
        );
        assert_eq!(handled, 0);
        assert!(playable.exists(), "a kept partial was deleted");

        // Validation could not run at all (the file locked, the decoder
        // unavailable): not proven bad, so not deletable either.
        let handled = cleanup_stale_partials(
            &dir,
            partial_recording_owner,
            |_| Some(false),
            |_| PartialDisposition::Unverified,
        );
        assert_eq!(handled, 0);
        assert!(playable.exists(), "an unverified partial was deleted");

        std::fs::remove_file(playable).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn recovery_names_dodge_existing_files() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "matteshot-recovery-names-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let partial = dir.join("clip.partial-42-1.mp4");
        std::fs::write(&partial, b"x").unwrap();
        let taken = dir.join("clip-recovered.mp4");
        std::fs::write(&taken, b"earlier recovery").unwrap();

        assert_eq!(
            recovered_video_path(&partial).unwrap(),
            dir.join("clip-recovered-1.mp4")
        );

        // Only the trailing marker is stripped: a recording whose own name
        // contains ".partial-" keeps that name.
        let odd = dir.join("clip.partial-backup.partial-42-1.mp4");
        assert_eq!(
            recovered_video_path(&odd).unwrap(),
            dir.join("clip.partial-backup-recovered.mp4")
        );

        std::fs::remove_file(partial).unwrap();
        std::fs::remove_file(taken).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn stale_partial_disposition_deletes_only_proven_undecodable_bytes() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "matteshot-partial-disposition-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // Too small to be a recording: the validator's own verdict, deletable.
        let garbage = dir.join("clip.partial-42-1.mp4");
        std::fs::write(&garbage, b"stale").unwrap();
        assert!(matches!(
            video_partial_disposition(&garbage),
            PartialDisposition::Delete
        ));

        // Gone before the check ran (or unreadable): no verdict on the bytes,
        // so nothing may be deleted on the strength of it.
        let missing = dir.join("clip.partial-42-2.mp4");
        assert!(matches!(
            video_partial_disposition(&missing),
            PartialDisposition::Unverified
        ));

        std::fs::remove_file(garbage).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }


    /// Pins SBS-764: an unqualified helper name is never the launch path.
    #[test]
    fn trusted_helper_path_never_returns_an_unqualified_name() {
        let trusted = Path::new("trusted-windows-dir");
        let exe = trusted_helper_path(trusted, "explorer.exe").unwrap();
        assert_eq!(exe, trusted.join("explorer.exe"));
        assert_ne!(exe.as_os_str(), "explorer.exe");
    }

    /// Pins SBS-764: a decoy beside cwd is not selected when the trusted
    /// directory is the Windows directory (or any other injected dir).
    #[test]
    fn trusted_helper_path_ignores_a_decoy_in_another_directory() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let cwd = std::env::temp_dir().join(format!(
            "matteshot-sbs-764-decoy-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&cwd).unwrap();
        let decoy = cwd.join("explorer.exe");
        std::fs::write(&decoy, b"not explorer").unwrap();

        let trusted = Path::new("trusted-windows-dir");
        let exe = trusted_helper_path(trusted, "explorer.exe").unwrap();
        assert_eq!(exe, trusted.join("explorer.exe"));
        assert_ne!(exe, decoy);

        std::fs::remove_file(decoy).unwrap();
        std::fs::remove_dir(cwd).unwrap();
    }

    /// Pins SBS-764: helper names cannot steer the join with separators.
    #[test]
    fn trusted_helper_path_rejects_traversal_and_absolute_names() {
        let trusted = Path::new("trusted-windows-dir");
        for name in [
            "",
            ".",
            "..",
            "../explorer.exe",
            r"..\explorer.exe",
            r"C:\evil\explorer.exe",
            "explorer.exe/../evil.exe",
            r"explorer.exe\..\evil.exe",
            "C:explorer.exe",
        ] {
            assert_eq!(
                trusted_helper_path(trusted, name),
                None,
                "accepted {name:?}"
            );
        }
    }

    /// Pins SBS-764: failing to read the Windows directory is its own
    /// state. It must not collapse into "launch explorer.exe by name".
    #[test]
    fn an_unreadable_windows_directory_does_not_fall_back_to_an_unqualified_helper() {
        assert_eq!(reveal_explorer_exe(None), None);
    }

    /// Pins SBS-764: a known Windows directory always yields that
    /// directory's explorer.exe, never an unqualified name.
    #[test]
    fn reveal_explorer_exe_is_under_the_supplied_windows_directory() {
        let trusted = Path::new("other-windows-dir");
        let exe = reveal_explorer_exe(Some(trusted)).unwrap();
        assert_eq!(exe, trusted.join("explorer.exe"));
    }

    /// Pins SBS-764 at the source: the production launch must not go
    /// back to an unqualified explorer helper.
    #[test]
    fn reveal_source_does_not_launch_unqualified_explorer() {
        let source = include_str!("output.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source");
        // Built in pieces so this test body cannot satisfy the needle.
        let forbidden = ["w!(", "\"explorer.exe\")"].concat();
        assert!(
            !production.contains(&forbidden),
            "reveal_in_explorer must not launch explorer.exe by unqualified name"
        );
        assert!(
            production.contains("reveal_explorer_exe(windows_directory().as_deref())"),
            "reveal_in_explorer must resolve explorer from the Windows directory"
        );
    }

    fn leftover_dir(label: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "matteshot-partial-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Move a file's last-write time back so the 24h PID-reuse window
    /// applies. No test can wait a day for it.
    fn backdate(path: &Path, by: std::time::Duration) {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::{FILETIME, HANDLE};
        use windows::Win32::Storage::FileSystem::SetFileTime;

        // FILETIME counts 100ns ticks from 1601-01-01.
        const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
        let since_epoch = (std::time::SystemTime::now() - by)
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let ticks = UNIX_EPOCH_TICKS
            + since_epoch.as_secs() * 10_000_000
            + u64::from(since_epoch.subsec_nanos()) / 100;
        let stamp = FILETIME {
            dwLowDateTime: ticks as u32,
            dwHighDateTime: (ticks >> 32) as u32,
        };
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        unsafe {
            SetFileTime(HANDLE(file.as_raw_handle()), None, None, Some(&stamp)).unwrap();
        }
    }

    /// SBS-893: PIDs get recycled. Finalize is bounded at 15 minutes, so a
    /// day-old partial whose owner still answers "live" is some unrelated
    /// long-running process holding that number. Without the 24h window one
    /// such reuse strands the file for as long as that process runs.
    #[test]
    fn cleanup_classifies_a_day_old_partial_whose_owner_pid_was_recycled() {
        let dir = leftover_dir("recycled-pid");
        let other_pid = std::process::id().wrapping_add(1).max(1);
        let partial = dir.join(format!("clip.partial-{other_pid}-1.mp4"));
        std::fs::write(&partial, b"not an mp4").unwrap();
        backdate(&partial, std::time::Duration::from_secs(25 * 60 * 60));

        let handled = cleanup_stale_partials(
            &dir,
            partial_recording_owner,
            |_| Some(true),
            |_| PartialDisposition::Delete,
        );
        assert_eq!(handled, 1, "a recycled PID stranded a day-old partial");
        assert!(!partial.exists());

        std::fs::remove_dir(dir).unwrap();
    }

    /// A live owner may still be inside Media Foundation Finalize.
    /// Deleting those bytes is the SBS-893 hole: the next start classified
    /// a mid-write MP4 as Undecodable and removed it.
    #[test]
    fn cleanup_skips_a_partial_whose_owner_process_may_still_be_running() {
        let dir = leftover_dir("live-owner");
        let other_pid = std::process::id().wrapping_add(1).max(1);
        let partial = dir.join(format!("clip.partial-{other_pid}-1.mp4"));
        std::fs::write(&partial, b"still being written").unwrap();

        let handled = cleanup_stale_partials(
            &dir,
            partial_recording_owner,
            |_| Some(true),
            |_| PartialDisposition::Delete,
        );
        assert_eq!(handled, 0);
        assert!(
            partial.exists(),
            "a partial whose owner may still be running was deleted"
        );

        std::fs::remove_file(partial).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    /// Failed liveness query is not "the owner is dead". Prefer keep over
    /// delete when in doubt (SBS-893).
    #[test]
    fn cleanup_keeps_an_undecodable_partial_when_owner_liveness_is_unknown() {
        let dir = leftover_dir("unknown-owner");
        let other_pid = std::process::id().wrapping_add(1).max(1);
        let partial = dir.join(format!("clip.partial-{other_pid}-1.mp4"));
        std::fs::write(&partial, b"still being written").unwrap();

        let handled = cleanup_stale_partials(
            &dir,
            partial_recording_owner,
            |_| None,
            |_| PartialDisposition::Delete,
        );
        assert_eq!(handled, 0);
        assert!(
            partial.exists(),
            "an undecodable partial was deleted when owner liveness was unknown"
        );

        std::fs::remove_file(partial).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    /// Proven-dead owner + proven-undecodable bytes are still garbage.
    /// The live/unknown skips must not stop us deleting that case (SBS-893).
    #[test]
    fn cleanup_still_deletes_undecodable_bytes_when_the_owner_is_known_dead() {
        let dir = leftover_dir("dead-owner");
        let other_pid = std::process::id().wrapping_add(1).max(1);
        let partial = dir.join(format!("clip.partial-{other_pid}-1.mp4"));
        std::fs::write(&partial, b"not an mp4").unwrap();

        let handled = cleanup_stale_partials(
            &dir,
            partial_recording_owner,
            |_| Some(false),
            |_| PartialDisposition::Delete,
        );
        assert_eq!(handled, 1);
        assert!(
            !partial.exists(),
            "proven-undecodable bytes from a dead owner were kept"
        );

        std::fs::remove_dir(dir).unwrap();
    }
}
