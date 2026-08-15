//! "Share" action: uploads an already-saved screenshot or recording to
//! share.matteshot.app and returns a short preview link.
//!
//! Reuses the signed license certificate the app already holds from
//! activation/refresh as proof of license (see `license::signed_certificate`)
//! — no separate auth, and no round trip to license.matteshot.app beyond
//! whatever already keeps that certificate fresh. An unlicensed device fails
//! locally before any bytes are sent.

use std::ffi::c_void;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::ptr;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
    WinHttpWriteData, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

const SHARE_HOST: &str = "share.matteshot.app";
const SHARE_PATH: &str = "/v1/share";
// Matches the worker's own MAX_UPLOAD_BYTES; caught here too so a failure
// reads as "too large" immediately instead of after minutes of upload.
const MAX_UPLOAD_BYTES: u64 = 300 * 1024 * 1024;

/// Posted to whichever window started a share once `share_in_background`'s
/// worker thread finishes. `lparam` is a boxed `ShareCompletion` —
/// `Box::from_raw` it back exactly once in the receiving wndproc, even when
/// its request ID is stale.
pub const WM_SHARE_COMPLETE: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 10;

pub type ShareOutcome = Result<String, String>;

pub struct ShareCompletion {
    pub request_id: u64,
    pub outcome: ShareOutcome,
}

static SHARE_REQUEST_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn accept_completion(pending: &mut Option<u64>, request_id: u64) -> bool {
    if *pending != Some(request_id) {
        return false;
    }
    *pending = None;
    true
}

fn share_content_type(path: &Path) -> Result<&'static str> {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("png") => Ok("image/png"),
        Some("mp4") => Ok("video/mp4"),
        _ => bail!("This file type cannot be shared."),
    }
}

fn validate_upload_size(len: u64) -> Result<()> {
    if len > MAX_UPLOAD_BYTES {
        bail!("This file is too large to share.");
    }
    Ok(())
}

/// Anything past this is not a share response: the worker answers with a
/// short JSON object, and an endpoint that keeps streaming is faulty or not
/// the endpoint. Read no further; the buffer stays small either way.
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

fn append_bounded(response: &mut Vec<u8>, chunk: &[u8]) -> Result<()> {
    if response.len() + chunk.len() > MAX_RESPONSE_BYTES {
        bail!("the share response was unexpectedly large");
    }
    response.extend_from_slice(chunk);
    Ok(())
}

/// Every caller opens the returned link in a browser or puts it on the
/// clipboard, so it must be exactly the shape the Share worker mints:
/// `https://share.matteshot.app/s/<id>` — canonical host, default port, no
/// userinfo, no query or fragment. A scheme check alone would let a faulty
/// or hijacked endpoint hand back any HTTPS site as though it were ours.
fn validate_share_url(url: &str) -> Result<()> {
    if !share_link_is_canonical(url) {
        bail!("the share response returned an unexpected link");
    }
    Ok(())
}

fn share_link_is_canonical(url: &str) -> bool {
    if url.bytes().any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace()) {
        return false;
    }
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let authority_end = rest
        .find(['/', '?', '#'])
        .unwrap_or(rest.len());
    let (authority, path) = rest.split_at(authority_end);
    // Userinfo and an explicit port are both things the worker never emits;
    // "share.matteshot.app@evil.example" and "share.matteshot.app:8443" each
    // read as ours to a naive prefix check.
    if authority.contains('@') || authority.contains(':') {
        return false;
    }
    if !authority.eq_ignore_ascii_case(SHARE_HOST) {
        return false;
    }
    let Some(id) = path.strip_prefix("/s/") else {
        return false;
    };
    (6..=64).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

/// Upload on a worker thread and post `WM_SHARE_COMPLETE` to `hwnd` with the
/// result. Every caller (picker, tweak editor, history browser) shares this
/// instead of each spawning and posting for itself, matching the pattern
/// update.rs already uses to notify the tray window from its own background
/// download thread.
pub fn share_in_background(hwnd: HWND, path: PathBuf) -> u64 {
    let request_id = SHARE_REQUEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let hwnd_value = hwnd.0 as isize;
    std::thread::spawn(move || {
        let outcome: ShareOutcome = share_file(&path).map_err(|error| format!("{error:#}"));
        let raw = Box::into_raw(Box::new(ShareCompletion {
            request_id,
            outcome,
        }));
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
    request_id
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
    let device_id = crate::license::device_id()
        .context("Sharing needs a stable device identity.")?;

    let content_type = share_content_type(path)?;
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("capture");
    let file_len = std::fs::metadata(path)
        .context("read the file to share")?
        .len();
    validate_upload_size(file_len)?;

    let boundary = format!("matteshot-{}", boundary_suffix());
    let parts = multipart_parts(
        &boundary,
        &certificate,
        &signature,
        &device_id,
        filename,
        content_type,
    );

    let (status, response_body) = post_multipart(SHARE_PATH, &boundary, path, file_len, &parts)?;
    let parsed: ShareResponse = serde_json::from_slice(&response_body).unwrap_or_default();
    if status != 200 {
        bail!(parsed
            .error
            .unwrap_or_else(|| format!("share request failed ({status})")));
    }
    let url = parsed.url.context("the share response had no link")?;
    // Every caller either opens this in a browser or hands it to the
    // clipboard as a link; the same guard update.rs already applies to its
    // manifest URLs before treating a network response as actionable.
    validate_share_url(&url)?;
    Ok(url)
}

/// A timestamp alone is not a uniqueness guarantee — clock resolution varies
/// by system, and two calls in the same process can land in the same tick.
/// The counter is what actually makes every call distinct.
static BOUNDARY_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn boundary_suffix() -> String {
    use std::sync::atomic::Ordering;
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let count = BOUNDARY_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}-{}-{count:x}", std::process::id())
}

/// Hand-built rather than pulled from a multipart crate: one field-heavy but
/// fixed-shape request, and the rest of this app already hand-rolls its own
/// HTTP calls over WinHTTP with no client library at all.
#[allow(clippy::too_many_arguments)]
struct MultipartParts {
    before_file: Vec<u8>,
    after_file: Vec<u8>,
}

fn multipart_parts(
    boundary: &str,
    certificate: &str,
    signature: &str,
    device_id: &str,
    filename: &str,
    content_type: &str,
) -> MultipartParts {
    let mut before_file = Vec::with_capacity(1024);
    for (name, value) in [
        ("certificate", certificate),
        ("signature", signature),
        ("device_id", device_id),
    ] {
        before_file.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    before_file.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
        )
        .as_bytes(),
    );
    MultipartParts {
        before_file,
        after_file: format!("\r\n--{boundary}--\r\n").into_bytes(),
    }
}

unsafe fn write_request_bytes(request: *mut c_void, bytes: &[u8]) -> Result<()> {
    let mut offset = 0;
    while offset < bytes.len() {
        let amount = (bytes.len() - offset).min(u32::MAX as usize) as u32;
        let mut written = 0u32;
        WinHttpWriteData(
            request,
            Some(bytes[offset..].as_ptr() as *const c_void),
            amount,
            &mut written,
        )
        .context("write share request")?;
        if written == 0 {
            bail!("write share request made no progress");
        }
        offset += written as usize;
    }
    Ok(())
}

fn post_multipart(
    path: &str,
    boundary: &str,
    file_path: &Path,
    file_len: u64,
    parts: &MultipartParts,
) -> Result<(u32, Vec<u8>)> {
    unsafe {
        let agent = HSTRING::from(concat!("Matteshot/", env!("CARGO_PKG_VERSION")));
        let session = InternetHandle::new(
            WinHttpOpen(
                &agent,
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            ),
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
        let total_len = (parts.before_file.len() as u64)
            .checked_add(file_len)
            .and_then(|length| length.checked_add(parts.after_file.len() as u64))
            .filter(|length| *length <= u32::MAX as u64)
            .context("share request is too large")? as u32;
        WinHttpSendRequest(request.0, Some(&headers), None, 0, total_len, 0)
            .context("send share request")?;
        write_request_bytes(request.0, &parts.before_file)?;
        let mut file = std::fs::File::open(file_path).context("open the file to share")?;
        let mut chunk = [0u8; 64 * 1024];
        let mut remaining = file_len;
        while remaining > 0 {
            let wanted = chunk.len().min(remaining as usize);
            let read = file
                .read(&mut chunk[..wanted])
                .context("read the file to share")?;
            if read == 0 {
                bail!("the file changed while it was being shared");
            }
            write_request_bytes(request.0, &chunk[..read])?;
            remaining -= read as u64;
        }
        if file.read(&mut chunk[..1]).context("check the file size")? != 0 {
            bail!("the file changed while it was being shared");
        }
        write_request_bytes(request.0, &parts.after_file)?;
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
            append_bounded(&mut response, &chunk[..read as usize])?;
        }
        Ok((status, response))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multipart_body_carries_every_field_and_the_file_between_boundaries() {
        let parts = multipart_parts(
            "BOUND",
            "cert-value",
            "sig-value",
            "device-value",
            "shot.png",
            "image/png",
        );
        let mut body = parts.before_file;
        body.extend_from_slice(b"\x89PNGraw");
        body.extend_from_slice(&parts.after_file);
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("--BOUND\r\n"));
        assert!(text.contains("name=\"certificate\"\r\n\r\ncert-value\r\n"));
        assert!(text.contains("name=\"signature\"\r\n\r\nsig-value\r\n"));
        assert!(text.contains("name=\"device_id\"\r\n\r\ndevice-value\r\n"));
        assert!(text
            .contains("name=\"file\"; filename=\"shot.png\"\r\nContent-Type: image/png\r\n\r\n"));
        assert!(text.ends_with("--BOUND--\r\n"));
        // The raw bytes must survive untouched inside the body, not just the
        // lossily-decoded text used above to check the surrounding structure.
        assert!(body.windows(7).any(|w| w == b"\x89PNGraw"));
    }

    #[test]
    fn multipart_metadata_stays_bounded_independent_of_file_size() {
        let parts = multipart_parts(
            "BOUND",
            "cert-value",
            "sig-value",
            "device-value",
            "recording.mp4",
            "video/mp4",
        );
        assert!(parts.before_file.len() + parts.after_file.len() < 2048);
    }

    #[test]
    fn only_the_latest_share_completion_is_accepted() {
        let mut pending = Some(2);
        assert!(!accept_completion(&mut pending, 1));
        assert_eq!(
            pending,
            Some(2),
            "a stale result cleared the current request"
        );
        assert!(accept_completion(&mut pending, 2));
        assert_eq!(pending, None);
    }

    #[test]
    fn boundary_suffix_never_collides_with_itself_in_quick_succession() {
        let a = boundary_suffix();
        let b = boundary_suffix();
        assert_ne!(
            a, b,
            "two boundaries generated back to back must still differ"
        );
    }

    #[test]
    fn share_preflight_accepts_only_supported_file_types() {
        assert_eq!(
            share_content_type(Path::new("shot.png")).unwrap(),
            "image/png"
        );
        assert_eq!(
            share_content_type(Path::new("clip.mp4")).unwrap(),
            "video/mp4"
        );
        for path in ["capture.gif", "payload.exe", "capture", "capture.PNG"] {
            assert!(
                share_content_type(Path::new(path)).is_err(),
                "accepted {path}"
            );
        }
    }

    #[test]
    fn share_preflight_rejects_only_files_over_the_limit() {
        assert!(validate_upload_size(MAX_UPLOAD_BYTES).is_ok());
        assert!(validate_upload_size(MAX_UPLOAD_BYTES + 1).is_err());
    }

    #[test]
    fn only_a_canonical_share_link_is_accepted() {
        assert!(validate_share_url("https://share.matteshot.app/s/ABCDEFGHJKMN").is_ok());
        // Host names are case-insensitive; the id is whatever the worker minted.
        assert!(validate_share_url("https://SHARE.matteshot.app/s/abc123XYZ").is_ok());
        for url in [
            "",
            // Wrong or missing scheme.
            "http://share.matteshot.app/s/ABCDEFGHJKMN",
            "file:///capture.png",
            "javascript:alert(1)",
            "share.matteshot.app/s/ABCDEFGHJKMN",
            // Not our host, including lookalikes a prefix check would pass.
            "https://share.example/s/ABCDEFGHJKMN",
            "https://share.matteshot.app.evil.example/s/ABCDEFGHJKMN",
            "https://evil.example/share.matteshot.app/s/ABCDEFGHJKMN",
            "https://matteshot.app/s/ABCDEFGHJKMN",
            // Userinfo and alternate ports.
            "https://share.matteshot.app@evil.example/s/ABCDEFGHJKMN",
            "https://user:pw@share.matteshot.app/s/ABCDEFGHJKMN",
            "https://share.matteshot.app:8443/s/ABCDEFGHJKMN",
            "https://share.matteshot.app:443/s/ABCDEFGHJKMN",
            // Wrong path shape, query, fragment, traversal, odd characters.
            "https://share.matteshot.app/",
            "https://share.matteshot.app/s/",
            "https://share.matteshot.app/v1/share",
            "https://share.matteshot.app/s/ABCDEFGHJKMN/raw",
            "https://share.matteshot.app/s/ABCDEFGHJKMN?x=1",
            "https://share.matteshot.app/s/ABCDEFGHJKMN#f",
            "https://share.matteshot.app/s/../s/ABCDEFGHJKMN",
            "https://share.matteshot.app/s/ABC",
            "https://share.matteshot.app/s/ABCDEF GHJKMN",
            "https://share.matteshot.app/s/ABCDEFGHJKMN\r\nSet-Cookie:x",
            "https://share.matteshot.app/s/ABCDEFGHJKMN%2F..",
        ] {
            assert!(validate_share_url(url).is_err(), "accepted {url:?}");
        }
        let too_long = format!("https://share.matteshot.app/s/{}", "A".repeat(65));
        assert!(validate_share_url(&too_long).is_err());
    }

    #[test]
    fn the_response_body_is_capped_before_it_grows() {
        let mut body = Vec::new();
        let chunk = vec![b'x'; 4096];
        for _ in 0..(MAX_RESPONSE_BYTES / chunk.len()) {
            append_bounded(&mut body, &chunk).unwrap();
        }
        assert_eq!(body.len(), MAX_RESPONSE_BYTES);
        // The first byte past the cap fails, and nothing more is retained.
        assert!(append_bounded(&mut body, b"y").is_err());
        assert_eq!(body.len(), MAX_RESPONSE_BYTES);
    }
}
