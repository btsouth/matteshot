//! Delivering the result: clipboard, PNG on disk, optional editor handoff.

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use image::RgbaImage;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const CF_DIB: u32 = 8;
const CF_HDROP: u32 = 15;

unsafe fn put_bytes(format: u32, bytes: &[u8]) -> Result<()> {
    let hmem = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
    let ptr = GlobalLock(hmem) as *mut u8;
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
    let _ = GlobalUnlock(hmem);
    SetClipboardData(format, HANDLE(hmem.0)).context("SetClipboardData")?;
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

    unsafe {
        OpenClipboard(HWND::default()).context("open clipboard")?;
        let result = (|| -> Result<()> {
            let _ = EmptyClipboard();
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
        })();
        let _ = CloseClipboard();
        result
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
    unsafe {
        OpenClipboard(HWND::default()).context("open clipboard")?;
        let result = (|| -> Result<()> {
            let _ = EmptyClipboard();
            put_bytes(CF_HDROP, &drop)
        })();
        let _ = CloseClipboard();
        result
    }
}

/// Plain text to the clipboard (OCR results).
pub fn text_to_clipboard(text: &str) -> Result<()> {
    const CF_UNICODETEXT: u32 = 13;
    let mut wide: Vec<u8> = Vec::new();
    for u in text.encode_utf16() {
        wide.extend_from_slice(&u.to_le_bytes());
    }
    wide.extend_from_slice(&[0, 0]);
    unsafe {
        OpenClipboard(HWND::default()).context("open clipboard")?;
        let result = (|| -> Result<()> {
            let _ = EmptyClipboard();
            put_bytes(CF_UNICODETEXT, &wide)
        })();
        let _ = CloseClipboard();
        result
    }
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
