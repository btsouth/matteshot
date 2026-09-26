//! "Share" action: uploads an already-saved screenshot or recording to a
//! self-hosted share server and returns a short preview link.
//!
//! Share is not part of the default build. Matteshot does not run a public
//! upload service, so the upload code only exists when the app is built with
//! `--features share`, and even then nothing is offered until config.json
//! names a server (`share_server`) and the upload token it expects
//! (`share_token`). The server is the Worker in `share-server/`; see
//! `docs/self-hosting-share.md`.
//!
//! Everything in this file is the editor-facing plumbing (whether Share is
//! offered, one upload per window, routing the result back to the window that
//! asked). The network half lives in `share/upload.rs`.

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use anyhow::Result;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

#[cfg(feature = "share")]
mod upload;

/// Shown when a Share action is reached without a usable share server. The
/// editors hide Share in that case, so this is the defensive path.
pub const NOT_CONFIGURED: &str = "Sharing is not set up. See docs/self-hosting-share.md.";

/// Outcome of a Share click before any upload starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareStart {
    Begin,
    Unavailable(&'static str),
}

/// Where uploads go, read from config.json. Only built when both halves are
/// present and the server is a plain `https://host[:port]` origin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareTarget {
    pub host: String,
    pub port: u16,
    // Read only by the upload half, which the default build leaves out.
    #[cfg_attr(not(feature = "share"), allow(dead_code))]
    pub token: String,
}

/// Parse `share_server`. Accepts `https://host` or `https://host:port`, with
/// an optional trailing slash, and nothing else: no path, query, userinfo, or
/// plain http. The upload carries the token, so it must never go anywhere a
/// typo or a pasted link could redirect it.
pub fn parse_server(server: &str) -> Option<(String, u16)> {
    let rest = server.trim().strip_prefix("https://")?;
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    if authority.is_empty() || authority.contains(['/', '?', '#', '@', ' ']) {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port.parse::<u16>().ok().filter(|port| *port != 0)?),
        None => (authority, 443),
    };
    let valid_host = !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
        && !host.starts_with(['.', '-'])
        && !host.ends_with(['.', '-']);
    valid_host.then(|| (host.to_ascii_lowercase(), port))
}

pub fn target_from(config: &crate::config::Config) -> Option<ShareTarget> {
    let (host, port) = parse_server(config.share_server.as_deref()?)?;
    let token = config.share_token.as_deref()?.trim();
    if token.is_empty() || token.bytes().any(|byte| byte.is_ascii_control()) {
        return None;
    }
    Some(ShareTarget {
        host,
        port,
        token: token.to_owned(),
    })
}

/// Whether Share should be offered at all: this build includes it, and
/// config.json points at a usable server.
pub fn available() -> bool {
    cfg!(feature = "share") && target_from(&crate::config::Config::load()).is_some()
}

pub fn share_start() -> ShareStart {
    if available() {
        ShareStart::Begin
    } else {
        ShareStart::Unavailable(NOT_CONFIGURED)
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
/// second click cannot spawn another upload or overwrite the id
/// `accept_completion` will match. Recdone already does this with
/// `if !state.sharing` (SBS-1075).
pub fn begin_if_idle(pending: &mut Option<u64>, start: impl FnOnce() -> u64) -> bool {
    if !share_idle(pending.is_some()) {
        return false;
    }
    *pending = Some(start());
    true
}

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

/// Upload an already-saved PNG or MP4 and return its share link. Blocking:
/// callers on a UI thread must use `share_in_background` instead.
pub fn share_file(path: &Path) -> Result<String> {
    #[cfg(feature = "share")]
    {
        let target = target_from(&crate::config::Config::load())
            .ok_or_else(|| anyhow::anyhow!(NOT_CONFIGURED))?;
        upload::share_file(&target, path)
    }
    #[cfg(not(feature = "share"))]
    {
        let _ = path;
        anyhow::bail!("This build of Matteshot does not include Share.")
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn config(server: Option<&str>, token: Option<&str>) -> Config {
        Config {
            share_server: server.map(str::to_owned),
            share_token: token.map(str::to_owned),
            ..Config::default()
        }
    }

    #[test]
    fn a_plain_https_origin_is_a_share_server() {
        assert_eq!(
            parse_server("https://share.example.com"),
            Some(("share.example.com".into(), 443))
        );
        assert_eq!(
            parse_server(" https://Share.Example.com/ "),
            Some(("share.example.com".into(), 443))
        );
        assert_eq!(
            parse_server("https://share.example.com:8443"),
            Some(("share.example.com".into(), 8443))
        );
    }

    #[test]
    fn anything_but_a_plain_https_origin_is_refused() {
        for server in [
            "",
            "share.example.com",
            "http://share.example.com",
            "https://",
            "https:///",
            "https://share.example.com/v1/share",
            "https://share.example.com?x=1",
            "https://share.example.com#x",
            "https://user@share.example.com",
            "https://share.example.com:0",
            "https://share.example.com:99999",
            "https://share.example.com:",
            "https://share example.com",
            "https://-share.example.com",
            "https://share.example.com.",
            "https://sh_are.example.com",
        ] {
            assert_eq!(parse_server(server), None, "accepted {server:?}");
        }
    }

    #[test]
    fn share_needs_both_a_server_and_a_token() {
        assert_eq!(target_from(&config(None, None)), None);
        assert_eq!(target_from(&config(Some("https://s.example"), None)), None);
        assert_eq!(target_from(&config(None, Some("secret"))), None);
        assert_eq!(
            target_from(&config(Some("https://s.example"), Some("  "))),
            None
        );
        assert_eq!(
            target_from(&config(Some("https://s.example"), Some("line\nbreak"))),
            None
        );
        assert_eq!(
            target_from(&config(Some("http://s.example"), Some("secret"))),
            None
        );
        assert_eq!(
            target_from(&config(Some("https://s.example"), Some(" secret "))),
            Some(ShareTarget {
                host: "s.example".into(),
                port: 443,
                token: "secret".into(),
            })
        );
    }

    #[test]
    fn a_default_config_offers_no_share() {
        assert_eq!(target_from(&Config::default()), None);
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
        assert_eq!(pending, Some(1), "a second click overwrote pending_share");
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

    /// Pins SBS-743: the Share wndproc helper must ignore a forged LPARAM
    /// instead of `Box::from_raw`ing it, and must not consume a real result.
    #[test]
    fn forged_share_lparams_do_not_take_a_real_completion() {
        let hwnd = 0x51A2E;
        let token = SHARE_COMPLETIONS.insert(
            hwnd,
            ShareCompletion {
                request_id: 9,
                outcome: Ok("https://share.example.com/s/ABCDEFGHJKMN".into()),
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

    #[cfg(not(feature = "share"))]
    #[test]
    fn the_default_build_never_offers_share() {
        assert!(!available());
        assert_eq!(share_start(), ShareStart::Unavailable(NOT_CONFIGURED));
        assert!(share_file(Path::new("shot.png")).is_err());
    }
}
