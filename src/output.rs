//! Delivering the result: clipboard, PNG on disk, optional editor handoff.

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use image::RgbaImage;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const CF_DIB: u32 = 8;
const CF_HDROP: u32 = 15;

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

fn partial_video_owner(name: &str) -> Option<u32> {
    let without_extension = name.strip_suffix(".mp4")?;
    let (_, owner_and_id) = without_extension.rsplit_once(".partial-")?;
    let (owner, id) = owner_and_id.split_once('-')?;
    if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    owner.parse().ok()
}

/// Remove only Matteshot's unmistakable incomplete-video names. A partial
/// from this process may belong to another open editor, so it is retained
/// unless it is old enough to be from a reused process ID.
pub fn cleanup_stale_video_partials(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(owner) = partial_video_owner(name) else {
            continue;
        };
        let old = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= std::time::Duration::from_secs(24 * 60 * 60));
        if (owner != std::process::id() || old) && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
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

pub fn save_png(img: &RgbaImage, style_name: &str, dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let name = format!(
        "matteshot-{}-{}.png",
        chrono::Local::now().format("%Y%m%d-%H%M%S%3f"),
        style_name.to_lowercase()
    );
    let path = dir.join(name);
    img.save(&path).context("write png")?;
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

/// Hand the finished PNG to the system default editor/viewer.
pub fn open_in_editor(path: &Path) {
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(HSTRING::from(path.as_os_str()).as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// Reveal a file in Explorer, selected.
pub fn reveal_in_explorer(path: &Path) {
    let args = format!("/select,\"{}\"", path.display());
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            w!("explorer.exe"),
            PCWSTR(HSTRING::from(args).as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
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

    #[test]
    fn partial_video_names_are_narrow_and_owner_aware() {
        assert_eq!(partial_video_owner("clip.partial-123-9.mp4"), Some(123));
        assert_eq!(partial_video_owner("clip.partial-nope-9.mp4"), None);
        assert_eq!(partial_video_owner("clip.partial-123-x.mp4"), None);
        assert_eq!(partial_video_owner("clip.mp4"), None);
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
        let live = partial_video_path(&dir.join("live.mp4"), 2);
        let normal = dir.join("normal.mp4");
        std::fs::write(&stale, b"stale").unwrap();
        std::fs::write(&live, b"live").unwrap();
        std::fs::write(&normal, b"normal").unwrap();

        assert_eq!(cleanup_stale_video_partials(&dir), 1);
        assert!(!stale.exists());
        assert!(live.exists());
        assert!(normal.exists());

        std::fs::remove_file(live).unwrap();
        std::fs::remove_file(normal).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
