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
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetOption,
    WinHttpSetTimeouts, WinHttpWriteData, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
    WINHTTP_FLAG_SECURE, WINHTTP_OPTION_REDIRECT_POLICY, WINHTTP_OPTION_REDIRECT_POLICY_NEVER,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_LOCATION, WINHTTP_QUERY_STATUS_CODE,
};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

/// Shown when Share is refused because this device has no paid license.
/// Same sentence `share_file` returns so every surface tells the same
/// truth (SBS-906).
pub const LICENSE_REQUIRED: &str = "Sharing needs an active Matteshot license.";

/// Outcome of a Share click before any upload starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareStart {
    Begin,
    Unavailable(&'static str),
}

/// SBS-906: do not start a share unless a paid license can attach a
/// certificate. Callers pass the live `license::can_share()` result.
pub fn share_start(can_share: bool) -> ShareStart {
    if can_share {
        ShareStart::Begin
    } else {
        ShareStart::Unavailable(LICENSE_REQUIRED)
    }
}

/// Recdone already refuses a second Share with `if !state.sharing`.
/// History and tweak pass `pending_share.is_some()` for the same signal
/// (SBS-1075). A status string is not this check: History clears it on a
/// timer, and recdone's other handlers overwrite it while an upload runs.
pub fn share_idle(in_flight: bool) -> bool {
    !in_flight
}

/// Claim this window's share slot if idle. `start` — typically
/// `share_in_background` — runs only when nothing is already pending, so a
/// second click cannot spawn another ≤300 MB upload or overwrite the id
/// `accept_completion` will match. Recdone already does this with
/// `if !state.sharing` (SBS-1075).
pub fn begin_if_idle(pending: &mut Option<u64>, start: impl FnOnce() -> u64) -> bool {
    if !share_idle(pending.is_some()) {
        return false;
    }
    *pending = Some(start());
    true
}

const SHARE_HOST: &str = "share.matteshot.app";
const SHARE_PATH: &str = "/v1/share";
// Matches the worker's own MAX_UPLOAD_BYTES; caught here too so a failure
// reads as "too large" immediately instead of after minutes of upload.
const MAX_UPLOAD_BYTES: u64 = 300 * 1024 * 1024;

/// Posted to whichever window started a share once `share_in_background`'s
/// worker thread finishes. `lparam` is an opaque token from
/// `SHARE_COMPLETIONS` (SBS-743) — never a pointer. Take it with
/// `take_completion`; a forged or stale token is ignored.
pub const WM_SHARE_COMPLETE: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 10;

pub type ShareOutcome = Result<String, String>;

pub struct ShareCompletion {
    pub request_id: u64,
    pub outcome: ShareOutcome,
}

static SHARE_COMPLETIONS: crate::completion::CompletionMailbox<ShareCompletion> =
    crate::completion::CompletionMailbox::new();

static SHARE_REQUEST_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn accept_completion(pending: &mut Option<u64>, request_id: u64) -> bool {
    if *pending != Some(request_id) {
        return false;
    }
    *pending = None;
    true
}

/// Redeem a `WM_SHARE_COMPLETE` token for this window. Forged `LPARAM`
/// values (0, 1, mapped addresses) return `None` and do not dereference.
pub fn take_completion(token: u64, hwnd: isize) -> Option<ShareCompletion> {
    SHARE_COMPLETIONS.take(token, hwnd)
}

pub fn discard_window(hwnd: isize) {
    SHARE_COMPLETIONS.unbind(hwnd);
}

fn share_content_type(path: &Path) -> Result<&'static str> {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("png") => Ok("image/png"),
        Some("mp4") => Ok("video/mp4"),
        _ => bail!("This file type cannot be shared."),
    }
}

/// Keep only ASCII alphanumeric plus `._-`. Empty after filtering becomes
/// `capture.png` / `capture.mp4` from the already-validated content type,
/// or `capture` if the type is unknown.
///
/// A renamed capture can put quotes/CR/LF in the on-disk name; the
/// disposition must stay one inert token.
fn sanitize_share_filename(name: &str, content_type: &str) -> String {
    let filtered: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(*c, '.' | '_' | '-'))
        .collect();
    let stem = filtered.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(&filtered);
    if stem.chars().any(|c| c.is_ascii_alphanumeric()) {
        return filtered;
    }
    match content_type {
        "image/png" => "capture.png".to_string(),
        "video/mp4" => "capture.mp4".to_string(),
        _ => "capture".to_string(),
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
    let mailbox_generation = SHARE_COMPLETIONS.generation_of(hwnd_value);
    std::thread::spawn(move || {
        let outcome: ShareOutcome = share_file(&path).map_err(|error| format!("{error:#}"));
        SHARE_COMPLETIONS.post_with_at(
            hwnd_value,
            mailbox_generation,
            ShareCompletion {
                request_id,
                outcome,
            },
            |token| unsafe {
                PostMessageW(
                    HWND(hwnd_value as *mut c_void),
                    WM_SHARE_COMPLETE,
                    WPARAM(0),
                    LPARAM(token as isize),
                )
                .is_ok()
            },
        );
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
    let (certificate, signature) = crate::license::signed_certificate().context(LICENSE_REQUIRED)?;
    let device_id = crate::license::device_id()
        .context("Sharing needs a stable device identity.")?;

    let content_type = share_content_type(path)?;
    let filename = sanitize_share_filename(
        path.file_name().and_then(|n| n.to_str()).unwrap_or(""),
        content_type,
    );
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
        &filename,
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

fn winhttp_redirect_policy() -> u32 {
    WINHTTP_OPTION_REDIRECT_POLICY_NEVER
}

fn redirect_status_is_error(status: u32) -> bool {
    (300..400).contains(&status)
}

fn redirect_refusal(service: &str, status: u32, location: Option<&str>) -> String {
    format!(
        "{service} service redirected the request: status={status} location={}",
        location.filter(|value| !value.is_empty()).unwrap_or("none")
    )
}

fn next_location_header_chars(current: usize, required_bytes: u32) -> Option<usize> {
    const MAX_CHARS: usize = 8192;
    let needed = (required_bytes as usize).div_ceil(2);
    if needed <= current || needed > MAX_CHARS {
        None
    } else {
        Some(needed)
    }
}

fn query_location_header(request: *mut c_void) -> Option<String> {
    let mut chars = 1024usize;
    loop {
        let mut buf = vec![0u16; chars];
        let mut size = (buf.len() * 2) as u32;
        let mut index = 0u32;
        match unsafe {
            WinHttpQueryHeaders(
                request,
                WINHTTP_QUERY_LOCATION,
                PCWSTR::null(),
                Some(buf.as_mut_ptr() as *mut c_void),
                &mut size,
                &mut index,
            )
        } {
            Ok(()) => {
                let wide = (size as usize) / 2;
                let text = String::from_utf16_lossy(&buf[..wide.min(buf.len())]);
                let text = text.trim_end_matches('\0').trim();
                return (!text.is_empty()).then(|| text.to_owned());
            }
            Err(_) => {
                let next = next_location_header_chars(chars, size)?;
                chars = next;
            }
        }
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
        // WinHTTP follows 307/308 by default and would resubmit the
        // capture bytes to Location.
        let policy = winhttp_redirect_policy().to_ne_bytes();
        WinHttpSetOption(
            Some(session.0 as *const c_void),
            WINHTTP_OPTION_REDIRECT_POLICY,
            Some(policy.as_slice()),
        )
        .context("disable share redirects")?;
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
        if redirect_status_is_error(status) {
            let location = query_location_header(request.0);
            bail!("{}", redirect_refusal("share", status, location.as_deref()));
        }

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

    fn assert_inert_share_filename(name: &str) {
        assert!(
            name.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')),
            "unsafe chars survived: {name:?}"
        );
        for needle in ['"', '\r', '\n', '/', '\\'] {
            assert!(!name.contains(needle), "{needle:?} survived in {name:?}");
        }
    }

    #[test]
    fn app_generated_share_filenames_are_unchanged() {
        assert_eq!(
            sanitize_share_filename("matteshot-20260815-133700123-adaptive.png", "image/png"),
            "matteshot-20260815-133700123-adaptive.png"
        );
        assert_eq!(
            sanitize_share_filename("matteshot-20260815-133700123.mp4", "video/mp4"),
            "matteshot-20260815-133700123.mp4"
        );
    }

    #[test]
    fn quotes_cr_lf_and_path_separators_never_survive_in_share_filename() {
        for (name, content_type) in [
            ("evil\".png", "image/png"),
            (
                "shot.png\"\r\nContent-Disposition: form-data; name=\"certificate\"",
                "image/png",
            ),
            ("shot.png\r\n\r\n--BOUND\r\n", "image/png"),
            ("..\\..\\Windows\\win.ini", "image/png"),
            ("foo/bar.png", "image/png"),
            ("foo\\bar.png", "image/png"),
        ] {
            let sanitized = sanitize_share_filename(name, content_type);
            assert_inert_share_filename(&sanitized);
            assert_ne!(sanitized, name, "hostile name used as-is: {name:?}");
        }
    }

    #[test]
    fn sanitized_hostile_filename_cannot_inject_multipart_fields() {
        let hostile =
            "shot.png\"\r\nContent-Disposition: form-data; name=\"certificate\"\r\n\r\nforged\r\n--BOUND\r\nContent-Disposition: form-data; name=\"file\"; filename=\"pwned.png";
        let filename = sanitize_share_filename(hostile, "image/png");
        assert_inert_share_filename(&filename);
        let parts = multipart_parts(
            "BOUND",
            "cert-value",
            "sig-value",
            "device-value",
            &filename,
            "image/png",
        );
        let before = String::from_utf8(parts.before_file).unwrap();
        assert!(
            !before.contains(hostile),
            "raw hostile payload leaked into before_file"
        );
        assert!(!before.contains("shot.png\""));
        assert!(!before.contains("name=\"certificate\"\r\n\r\nforged"));
        assert_eq!(before.matches("name=\"certificate\"").count(), 1);
        assert_eq!(before.matches("name=\"signature\"").count(), 1);
        assert_eq!(before.matches("name=\"device_id\"").count(), 1);
        assert_eq!(before.matches("name=\"file\"").count(), 1);
        assert_eq!(before.matches("filename=").count(), 1);
        assert!(before.contains(&format!("filename=\"{filename}\"")));
        assert!(before.contains("name=\"certificate\"\r\n\r\ncert-value\r\n"));
    }

    #[test]
    fn empty_or_unusable_share_filenames_fall_back_from_content_type() {
        for empty in ["", "...", "---", "._-", "\"\r\n", "/", "\\", "   "] {
            assert_eq!(
                sanitize_share_filename(empty, "image/png"),
                "capture.png",
                "{empty:?}"
            );
            assert_eq!(
                sanitize_share_filename(empty, "video/mp4"),
                "capture.mp4",
                "{empty:?}"
            );
            assert_eq!(
                sanitize_share_filename(empty, "application/octet-stream"),
                "capture",
                "{empty:?}"
            );
        }
    }

    #[test]
    fn shareable_png_names_without_an_ascii_stem_use_the_capture_png_fallback() {
        assert_eq!(
            sanitize_share_filename("スクリーンショット.png", "image/png"),
            "capture.png"
        );
        assert_eq!(sanitize_share_filename("---.png", "image/png"), "capture.png");
        assert_eq!(sanitize_share_filename(".png", "image/png"), "capture.png");
        assert_eq!(
            sanitize_share_filename("matteshot.png", "image/png"),
            "matteshot.png"
        );
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

    /// The old History/tweak Share path: always spawn, overwrite
    /// `pending_share`. That is SBS-1075 — `accept_completion` then drops
    /// the first result while a second uncancellable upload keeps running.
    fn overwrite_pending_share(pending: &mut Option<u64>, start: impl FnOnce() -> u64) -> bool {
        *pending = Some(start());
        true
    }

    #[test]
    fn a_second_share_does_not_spawn_or_overwrite_pending() {
        let mut pending = None;
        let mut started = Vec::new();
        assert!(share_idle(pending.is_some()), "a fresh window must be idle");
        assert!(begin_if_idle(&mut pending, || {
            started.push(1);
            1
        }));
        assert_eq!(pending, Some(1));
        assert!(!share_idle(pending.is_some()), "recdone's sharing flag");
        assert!(
            !begin_if_idle(&mut pending, || {
                started.push(2);
                2
            }),
            "History/tweak used to spawn here"
        );
        assert_eq!(
            pending,
            Some(1),
            "a second click overwrote pending_share"
        );
        assert_eq!(started, [1], "a second click started another upload");
        // accept_completion still only drops a stale UI result — that is
        // not the cancel path, and must not be treated as one.
        assert!(!accept_completion(&mut pending, 2));
        assert_eq!(pending, Some(1));
        assert!(accept_completion(&mut pending, 1));
        assert_eq!(pending, None);
        assert!(share_idle(pending.is_some()));
        assert!(begin_if_idle(&mut pending, || {
            started.push(3);
            3
        }));
        assert_eq!(pending, Some(3));
        assert_eq!(started, [1, 3]);
    }

    /// Pins the bug this helper replaces: the old always-overwrite path
    /// would fail `a_second_share_does_not_spawn_or_overwrite_pending`.
    #[test]
    fn overwriting_pending_share_is_the_sbs_1075_bug() {
        let mut pending = None;
        let mut started = Vec::new();
        assert!(overwrite_pending_share(&mut pending, || {
            started.push(1);
            1
        }));
        assert!(overwrite_pending_share(&mut pending, || {
            started.push(2);
            2
        }));
        assert_eq!(pending, Some(2), "the second click overwrote the first id");
        assert_eq!(started, [1, 2], "two uploads were in flight");
        assert!(
            !accept_completion(&mut pending, 1),
            "the first completion is now a stale UI result"
        );
        assert_eq!(pending, Some(2));
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
    fn share_is_refused_without_a_paid_license() {
        assert_eq!(
            share_start(false),
            ShareStart::Unavailable(LICENSE_REQUIRED)
        );
        assert_eq!(share_start(true), ShareStart::Begin);
        assert_eq!(
            LICENSE_REQUIRED,
            "Sharing needs an active Matteshot license."
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

    #[test]
    fn winhttp_redirect_policy_is_never() {
        assert_eq!(
            winhttp_redirect_policy(),
            WINHTTP_OPTION_REDIRECT_POLICY_NEVER
        );
        assert_eq!(winhttp_redirect_policy(), 0);
    }

    /// A 307 to a second host must not transmit the body — NEVER plus
    /// treating 3xx as error is the probe.
    #[test]
    fn redirect_status_is_error_for_3xx_including_307_and_308() {
        for status in [301, 302, 303, 307, 308] {
            assert!(redirect_status_is_error(status), "HTTP {status}");
        }
        for status in [200, 403, 404, 500] {
            assert!(!redirect_status_is_error(status), "HTTP {status}");
        }
    }

    #[test]
    fn a_307_is_an_error_and_not_a_successful_share() {
        assert!(redirect_status_is_error(307));
        // share_file only treats 200 as a share URL; a 307 cannot become one.
        assert_ne!(307u32, 200);
    }

    /// Pins SBS-743: the Share wndproc helper must ignore a forged LPARAM
    /// instead of `Box::from_raw`ing it, and must not consume a real result.
    #[test]
    fn forged_share_lparams_do_not_take_a_real_completion() {
        let hwnd = 0x51A2E;
        let token = SHARE_COMPLETIONS.insert(
            hwnd,
            ShareCompletion {
                request_id: 9,
                outcome: Ok("https://share.matteshot.app/s/ABCDEFGHJKMN".into()),
            },
        );
        for forged in [0_u64, 1, 0x7fff_ffff, 0xDEAD_BEEF] {
            assert_ne!(token, forged);
            assert!(
                take_completion(forged, hwnd).is_none(),
                "forged share token {:#x} was accepted",
                forged
            );
        }
        let got = take_completion(token, hwnd).expect("real token");
        assert_eq!(got.request_id, 9);
        assert!(take_completion(token, hwnd).is_none());
    }
}
