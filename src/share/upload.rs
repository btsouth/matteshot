//! The network half of Share, built only with `--features share`.
//!
//! One multipart POST to `<share_server>/v1/share` carrying the file and an
//! `Authorization: Bearer <share_token>` header, answered with
//! `{"url": "https://<host>/s/<id>"}`. The server is the Worker in
//! `share-server/`.

use std::ffi::c_void;
use std::io::Read;
use std::path::Path;
use std::ptr;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetOption,
    WinHttpSetTimeouts, WinHttpWriteData, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE,
    WINHTTP_OPTION_REDIRECT_POLICY, WINHTTP_OPTION_REDIRECT_POLICY_NEVER,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_LOCATION, WINHTTP_QUERY_STATUS_CODE,
};

use super::ShareTarget;

const SHARE_PATH: &str = "/v1/share";
// Matches share-server's default limit, which is Cloudflare's Free-plan
// request-body cap. Caught here too so a failure reads as "too large" before
// any upload starts.
const MAX_UPLOAD_BYTES: u64 = 100 * 1024 * 1024;

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
    let stem = filtered
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(&filtered);
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

/// Anything past this is not a share response: the server answers with a
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
/// clipboard, so it must be exactly the shape the share server mints:
/// `https://<configured host>[:<configured port>]/s/<id>`, with no userinfo,
/// query or fragment. A scheme check alone would let a faulty or hijacked
/// endpoint hand back any HTTPS site as though it were the user's own.
fn validate_share_url(target: &ShareTarget, url: &str) -> Result<()> {
    if !share_link_is_canonical(target, url) {
        bail!("the share response returned an unexpected link");
    }
    Ok(())
}

fn share_link_is_canonical(target: &ShareTarget, url: &str) -> bool {
    if url
        .bytes()
        .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return false;
    }
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(authority_end);
    // Userinfo is something the server never emits, and
    // "share.example.com@evil.example" reads as ours to a naive prefix check.
    if authority.contains('@') {
        return false;
    }
    let expected = if target.port == 443 {
        target.host.clone()
    } else {
        format!("{}:{}", target.host, target.port)
    };
    if !authority.eq_ignore_ascii_case(&expected) {
        return false;
    }
    let Some(id) = path.strip_prefix("/s/") else {
        return false;
    };
    (6..=64).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
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

pub fn share_file(target: &ShareTarget, path: &Path) -> Result<String> {
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
    let parts = multipart_parts(&boundary, &filename, content_type);

    let (status, response_body) = post_multipart(target, &boundary, path, file_len, &parts)?;
    let parsed: ShareResponse = serde_json::from_slice(&response_body).unwrap_or_default();
    if status != 200 {
        bail!(parsed
            .error
            .unwrap_or_else(|| format!("share request failed ({status})")));
    }
    let url = parsed.url.context("the share response had no link")?;
    validate_share_url(target, &url)?;
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

/// Hand-built rather than pulled from a multipart crate: one fixed-shape
/// request, and the rest of this app already hand-rolls its HTTP calls over
/// WinHTTP with no client library at all.
struct MultipartParts {
    before_file: Vec<u8>,
    after_file: Vec<u8>,
}

fn multipart_parts(boundary: &str, filename: &str, content_type: &str) -> MultipartParts {
    MultipartParts {
        before_file: format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
        )
        .into_bytes(),
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

fn request_headers(boundary: &str, token: &str) -> Vec<u16> {
    format!(
        "Content-Type: multipart/form-data; boundary={boundary}\r\nAuthorization: Bearer {token}\r\n"
    )
    .encode_utf16()
    .collect()
}

fn post_multipart(
    target: &ShareTarget,
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
        // WinHTTP follows 307/308 by default and would resubmit the capture
        // bytes, and the token, to Location.
        let policy = winhttp_redirect_policy().to_ne_bytes();
        WinHttpSetOption(
            Some(session.0 as *const c_void),
            WINHTTP_OPTION_REDIRECT_POLICY,
            Some(policy.as_slice()),
        )
        .context("disable share redirects")?;
        // A large recording needs real headroom on the send side; the
        // response is a small JSON body.
        WinHttpSetTimeouts(session.0, 10_000, 10_000, 120_000, 30_000)
            .context("set share connection timeouts")?;
        let host = HSTRING::from(target.host.as_str());
        let connection = InternetHandle::new(
            WinHttpConnect(session.0, &host, target.port, 0),
            "connect to the share server",
        )?;
        let method = HSTRING::from("POST");
        let wide_path = HSTRING::from(SHARE_PATH);
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
        let headers = request_headers(boundary, &target.token);
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

    fn target() -> ShareTarget {
        ShareTarget {
            host: "share.example.com".into(),
            port: 443,
            token: "token-value".into(),
        }
    }

    #[test]
    fn multipart_body_carries_only_the_file_between_boundaries() {
        let parts = multipart_parts("BOUND", "shot.png", "image/png");
        let mut body = parts.before_file;
        body.extend_from_slice(b"\x89PNGraw");
        body.extend_from_slice(&parts.after_file);
        let text = String::from_utf8_lossy(&body);
        assert!(text.starts_with("--BOUND\r\n"));
        assert!(text
            .contains("name=\"file\"; filename=\"shot.png\"\r\nContent-Type: image/png\r\n\r\n"));
        assert!(text.ends_with("--BOUND--\r\n"));
        assert_eq!(text.matches("Content-Disposition").count(), 1);
        // The raw bytes must survive untouched inside the body, not just the
        // lossily-decoded text used above to check the surrounding structure.
        assert!(body.windows(7).any(|w| w == b"\x89PNGraw"));
    }

    #[test]
    fn the_token_travels_in_a_header_and_never_in_the_body() {
        let headers = String::from_utf16(&request_headers("BOUND", "token-value")).unwrap();
        assert!(headers.contains("Authorization: Bearer token-value\r\n"));
        assert!(headers.contains("boundary=BOUND\r\n"));
        let parts = multipart_parts("BOUND", "shot.png", "image/png");
        let body = String::from_utf8_lossy(&parts.before_file).into_owned()
            + &String::from_utf8_lossy(&parts.after_file);
        assert!(!body.contains("token-value"));
    }

    #[test]
    fn multipart_metadata_stays_bounded_independent_of_file_size() {
        let parts = multipart_parts("BOUND", "recording.mp4", "video/mp4");
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
                "shot.png\"\r\nContent-Disposition: form-data; name=\"file\"",
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
            "shot.png\"\r\nContent-Disposition: form-data; name=\"extra\"\r\n\r\nforged\r\n--BOUND\r\nContent-Disposition: form-data; name=\"file\"; filename=\"pwned.png";
        let filename = sanitize_share_filename(hostile, "image/png");
        assert_inert_share_filename(&filename);
        let parts = multipart_parts("BOUND", &filename, "image/png");
        let before = String::from_utf8(parts.before_file).unwrap();
        assert!(!before.contains(hostile), "raw hostile payload leaked");
        assert!(!before.contains("shot.png\""));
        assert!(!before.contains("name=\"extra\""));
        assert_eq!(before.matches("name=\"file\"").count(), 1);
        assert_eq!(before.matches("filename=").count(), 1);
        assert!(before.contains(&format!("filename=\"{filename}\"")));
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
        assert_eq!(
            sanitize_share_filename("---.png", "image/png"),
            "capture.png"
        );
        assert_eq!(sanitize_share_filename(".png", "image/png"), "capture.png");
        assert_eq!(
            sanitize_share_filename("matteshot.png", "image/png"),
            "matteshot.png"
        );
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
    fn only_a_link_on_the_configured_server_is_accepted() {
        let target = target();
        assert!(validate_share_url(&target, "https://share.example.com/s/ABCDEFGHJKMN").is_ok());
        // Host names are case-insensitive; the id is whatever the server minted.
        assert!(validate_share_url(&target, "https://SHARE.example.com/s/abc123XYZ").is_ok());
        for url in [
            "",
            // Wrong or missing scheme.
            "http://share.example.com/s/ABCDEFGHJKMN",
            "file:///capture.png",
            "javascript:alert(1)",
            "share.example.com/s/ABCDEFGHJKMN",
            // Not the configured host, including lookalikes a prefix check would pass.
            "https://share.matteshot.app/s/ABCDEFGHJKMN",
            "https://share.example.com.evil.example/s/ABCDEFGHJKMN",
            "https://evil.example/share.example.com/s/ABCDEFGHJKMN",
            "https://example.com/s/ABCDEFGHJKMN",
            // Userinfo and alternate ports.
            "https://share.example.com@evil.example/s/ABCDEFGHJKMN",
            "https://user:pw@share.example.com/s/ABCDEFGHJKMN",
            "https://share.example.com:8443/s/ABCDEFGHJKMN",
            "https://share.example.com:443/s/ABCDEFGHJKMN",
            // Wrong path shape, query, fragment, traversal, odd characters.
            "https://share.example.com/",
            "https://share.example.com/s/",
            "https://share.example.com/v1/share",
            "https://share.example.com/s/ABCDEFGHJKMN/raw",
            "https://share.example.com/s/ABCDEFGHJKMN?x=1",
            "https://share.example.com/s/ABCDEFGHJKMN#f",
            "https://share.example.com/s/../s/ABCDEFGHJKMN",
            "https://share.example.com/s/ABC",
            "https://share.example.com/s/ABCDEF GHJKMN",
            "https://share.example.com/s/ABCDEFGHJKMN\r\nSet-Cookie:x",
            "https://share.example.com/s/ABCDEFGHJKMN%2F..",
        ] {
            assert!(
                validate_share_url(&target, url).is_err(),
                "accepted {url:?}"
            );
        }
        let too_long = format!("https://share.example.com/s/{}", "A".repeat(65));
        assert!(validate_share_url(&target, &too_long).is_err());
    }

    #[test]
    fn a_server_on_another_port_must_answer_with_that_port() {
        let target = ShareTarget {
            port: 8443,
            ..target()
        };
        assert!(validate_share_url(&target, "https://share.example.com:8443/s/ABCDEFGH").is_ok());
        assert!(validate_share_url(&target, "https://share.example.com/s/ABCDEFGH").is_err());
        assert!(validate_share_url(&target, "https://share.example.com:9443/s/ABCDEFGH").is_err());
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

    /// A 307 to a second host must not transmit the body or the token.
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
    fn location_header_growth_is_bounded() {
        assert_eq!(next_location_header_chars(1024, 4096), Some(2048));
        assert_eq!(next_location_header_chars(1024, 1024), None);
        assert_eq!(next_location_header_chars(1024, 64 * 1024), None);
        assert!(redirect_refusal("share", 307, None).contains("location=none"));
    }
}
