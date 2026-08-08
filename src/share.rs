//! "Share" action: uploads an already-saved screenshot or recording to
//! share.matteshot.app and returns a short preview link.
//!
//! Reuses the signed license certificate the app already holds from
//! activation/refresh as proof of license (see `license::signed_certificate`)
//! — no separate auth, and no round trip to license.matteshot.app beyond
//! whatever already keeps that certificate fresh. An unlicensed device fails
//! locally before any bytes are sent.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER,
    WINHTTP_QUERY_STATUS_CODE,
};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

const SHARE_HOST: &str = "share.matteshot.app";
const SHARE_PATH: &str = "/v1/share";
// Matches the worker's own MAX_UPLOAD_BYTES; caught here too so a failure
// reads as "too large" immediately instead of after minutes of upload.
const MAX_UPLOAD_BYTES: u64 = 300 * 1024 * 1024;

/// Posted to whichever window started a share once `share_in_background`'s
/// worker thread finishes. `lparam` is a boxed `ShareOutcome` — `Box::from_raw`
/// it back, exactly once, in the receiving wndproc.
pub const WM_SHARE_COMPLETE: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 10;

pub type ShareOutcome = Result<String, String>;

/// Upload on a worker thread and post `WM_SHARE_COMPLETE` to `hwnd` with the
/// result. Every caller (picker, tweak editor, history browser) shares this
/// instead of each spawning and posting for itself, matching the pattern
/// update.rs already uses to notify the tray window from its own background
/// download thread.
pub fn share_in_background(hwnd: HWND, path: PathBuf) {
    let hwnd_value = hwnd.0 as isize;
    std::thread::spawn(move || {
        let outcome: ShareOutcome = share_file(&path).map_err(|error| format!("{error:#}"));
        let raw = Box::into_raw(Box::new(outcome));
        let posted = unsafe {
            PostMessageW(
                HWND(hwnd_value as *mut c_void),
                WM_SHARE_COMPLETE,
                WPARAM(0),
                LPARAM(raw as isize),
            )
        };
        if posted.is_err() {
            unsafe {
                drop(Box::from_raw(raw));
            }
        }
    });
}

struct InternetHandle(*mut c_void);

impl InternetHandle {
    fn new(raw: *mut c_void, what: &str) -> Result<Self> {
        if raw.is_null() {
            Err(windows::core::Error::from_win32()).with_context(|| what.to_owned())
        } else {
            Ok(Self(raw))
        }
    }
}

impl Drop for InternetHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = WinHttpCloseHandle(self.0);
        }
    }
}

#[derive(Deserialize, Default)]
struct ShareResponse {
    url: Option<String>,
    error: Option<String>,
}

/// Upload an already-saved PNG or MP4 and return its share link. Blocking:
/// callers on a UI thread must run this on a worker thread, the same way the
/// app already backgrounds auto-update downloads.
pub fn share_file(path: &Path) -> Result<String> {
    let (certificate, signature) = crate::license::signed_certificate()
        .context("Sharing needs an active Matteshot license.")?;
    let device_id = crate::license::device_id();

    let content_type = match path.extension().and_then(|e| e.to_str()) {
        Some("png") => "image/png",
        Some("mp4") => "video/mp4",
        _ => bail!("This file type cannot be shared."),
    };
    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("capture");
    let bytes = std::fs::read(path).context("read the file to share")?;
    if bytes.len() as u64 > MAX_UPLOAD_BYTES {
        bail!("This file is too large to share.");
    }

    let boundary = format!("matteshot-{}", boundary_suffix());
    let body = build_multipart(
        &boundary,
        &certificate,
        &signature,
        &device_id,
        filename,
        content_type,
        &bytes,
    );

    let (status, response_body) = post_multipart(SHARE_PATH, &boundary, &body)?;
    let parsed: ShareResponse = serde_json::from_slice(&response_body).unwrap_or_default();
    if status != 200 {
        bail!(parsed.error.unwrap_or_else(|| format!("share request failed ({status})")));
    }
    let url = parsed.url.context("the share response had no link")?;
    // Every caller either opens this in a browser or hands it to the
    // clipboard as a link; the same guard update.rs already applies to its
    // manifest URLs before treating a network response as actionable.
    if !url.starts_with("https://") {
        bail!("the share response returned an unexpected link");
    }
    Ok(url)
}

/// A timestamp alone is not a uniqueness guarantee — clock resolution varies
/// by system, and two calls in the same process can land in the same tick.
/// The counter is what actually makes every call distinct.
static BOUNDARY_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn boundary_suffix() -> String {
    use std::sync::atomic::Ordering;
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let count = BOUNDARY_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}-{}-{count:x}", std::process::id())
}

/// Hand-built rather than pulled from a multipart crate: one field-heavy but
/// fixed-shape request, and the rest of this app already hand-rolls its own
/// HTTP calls over WinHTTP with no client library at all.
#[allow(clippy::too_many_arguments)]
fn build_multipart(
    boundary: &str,
    certificate: &str,
    signature: &str,
    device_id: &str,
    filename: &str,
    content_type: &str,
    file_bytes: &[u8],
) -> Vec<u8> {
    let mut body = Vec::with_capacity(file_bytes.len() + 512);
    for (name, value) in [
        ("certificate", certificate),
        ("signature", signature),
        ("device_id", device_id),
    ] {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(file_bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

fn post_multipart(path: &str, boundary: &str, body: &[u8]) -> Result<(u32, Vec<u8>)> {
    unsafe {
        let agent = HSTRING::from(concat!("Matteshot/", env!("CARGO_PKG_VERSION")));
        let session = InternetHandle::new(
            WinHttpOpen(&agent, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0),
            "open share connection",
        )?;
        // A large recording needs real headroom on the send side; the
        // response is a small JSON body, so receive stays close to what
        // license.rs's post_json budgets for one.
        WinHttpSetTimeouts(session.0, 10_000, 10_000, 120_000, 30_000)
            .context("set share connection timeouts")?;
        let host = HSTRING::from(SHARE_HOST);
        let connection = InternetHandle::new(
            WinHttpConnect(session.0, &host, 443, 0),
            "connect to Matteshot share service",
        )?;
        let method = HSTRING::from("POST");
        let wide_path = HSTRING::from(path);
        let request = InternetHandle::new(
            WinHttpOpenRequest(
                connection.0,
                &method,
                &wide_path,
                PCWSTR::null(),
                PCWSTR::null(),
                ptr::null(),
                WINHTTP_FLAG_SECURE,
            ),
            "open share request",
        )?;
        let headers: Vec<u16> =
            format!("Content-Type: multipart/form-data; boundary={boundary}\r\n")
                .encode_utf16()
                .collect();
        WinHttpSendRequest(
            request.0,
            Some(&headers),
            Some(body.as_ptr() as *const c_void),
            body.len() as u32,
            body.len() as u32,
            0,
        )
        .context("send share request")?;
        WinHttpReceiveResponse(request.0, ptr::null_mut()).context("receive share response")?;

        let mut status = 0u32;
        let mut status_size = std::mem::size_of::<u32>() as u32;
        let mut index = 0u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut c_void),
            &mut status_size,
            &mut index,
        )
        .context("read share response status")?;

        let mut response = Vec::new();
        loop {
            let mut chunk = [0u8; 4096];
            let mut read = 0u32;
            WinHttpReadData(
                request.0,
                chunk.as_mut_ptr() as *mut c_void,
                chunk.len() as u32,
                &mut read,
            )
            .context("read share response body")?;
            if read == 0 {
                break;
            }
            response.extend_from_slice(&chunk[..read as usize]);
        }
        Ok((status, response))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multipart_body_carries_every_field_and_the_file_between_boundaries() {
        let body = build_multipart(
            "BOUND",
            "cert-value",
            "sig-value",
            "device-value",
            "shot.png",
            "image/png",
            b"\x89PNGraw",
        );
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("--BOUND\r\n"));
        assert!(text.contains("name=\"certificate\"\r\n\r\ncert-value\r\n"));
        assert!(text.contains("name=\"signature\"\r\n\r\nsig-value\r\n"));
        assert!(text.contains("name=\"device_id\"\r\n\r\ndevice-value\r\n"));
        assert!(text.contains(
            "name=\"file\"; filename=\"shot.png\"\r\nContent-Type: image/png\r\n\r\n"
        ));
        assert!(text.ends_with("--BOUND--\r\n"));
        // The raw bytes must survive untouched inside the body, not just the
        // lossily-decoded text used above to check the surrounding structure.
        assert!(body.windows(7).any(|w| w == b"\x89PNGraw"));
    }

    #[test]
    fn boundary_suffix_never_collides_with_itself_in_quick_succession() {
        let a = boundary_suffix();
        let b = boundary_suffix();
        assert_ne!(a, b, "two boundaries generated back to back must still differ");
    }
}
