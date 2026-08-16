//! Settings policy for flipping the Startup shortcut (SBS-759).
//!
//! `tray::set_autostart` already returns a `Result`. Settings used to discard
//! it, so a denied, missing, or read-only Startup folder left the checkbox
//! looking unchanged (or briefly wrong) with no explanation. This module is
//! the interpretation layer: after every attempt, the displayed state is the
//! shortcut that is actually on disk, and a failed or unverified write is its
//! own outcome, not success.

/// Outcome of one Settings click on "Start with Windows".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AutostartToggle {
    /// What the checkbox must show after this attempt. Always the actual
    /// shortcut state, never the requested one.
    pub enabled: bool,
    /// Concise actionable error for a Settings dialog. `None` only when the
    /// write reported success *and* the shortcut state matches the request.
    pub error: Option<String>,
    /// Privacy-safe diagnostic line. No path, no user folder. `None` on the
    /// same success condition as `error`.
    pub diagnostic: Option<&'static str>,
}

pub(crate) const CREATE_FAILED: &str = "autostart shortcut create failed";
pub(crate) const REMOVE_FAILED: &str = "autostart shortcut remove failed";

/// Apply a requested autostart change and report what Settings should show.
///
/// `set` is the writer (`tray::set_autostart` in production). `is_enabled` is
/// a fresh read of the shortcut (`tray::autostart_enabled`). The read happens
/// after the write so "the writer returned Ok" is not treated as "the file is
/// now in the requested state".
pub(crate) fn apply_autostart_toggle<E: std::fmt::Display>(
    want_enabled: bool,
    set: impl FnOnce(bool) -> Result<(), E>,
    is_enabled: impl FnOnce() -> bool,
) -> AutostartToggle {
    // `{:#}` and not `to_string`: production passes `tray::set_autostart`,
    // whose errors are anyhow contexts. Plain Display prints the outermost
    // context alone, so a real Access Denied would reach the dialog as the
    // tautology "write the autostart shortcut" with the OS cause dropped.
    let set_error = match set(want_enabled) {
        Ok(()) => None,
        Err(error) => Some(format!("{error:#}")),
    };
    let enabled = is_enabled();
    if set_error.is_none() && enabled == want_enabled {
        return AutostartToggle {
            enabled,
            error: None,
            diagnostic: None,
        };
    }

    let action = if want_enabled { "create" } else { "remove" };
    let hint = if want_enabled {
        "Check that your Startup folder exists and is writable."
    } else {
        "Check that the Startup shortcut is not locked or read-only."
    };
    let reason = match set_error {
        Some(error) => error,
        None => format!(
            "the Startup shortcut is still {} after a reported-successful {action}",
            if enabled { "present" } else { "missing" }
        ),
    };
    AutostartToggle {
        enabled,
        error: Some(format!(
            "Matteshot could not {action} the Start with Windows shortcut. The checkbox still matches the shortcut on disk.\n\n{hint}\n\n{reason}"
        )),
        diagnostic: Some(if want_enabled {
            CREATE_FAILED
        } else {
            REMOVE_FAILED
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fail(msg: &'static str) -> impl FnOnce(bool) -> Result<(), &'static str> {
        move |_| Err(msg)
    }

    fn succeed() -> impl FnOnce(bool) -> Result<(), &'static str> {
        |_| Ok(())
    }

    /// A denied or missing Startup folder used to look like the toggle did
    /// nothing: the write failed, the file stayed absent, and Settings said
    /// nothing. The checkbox must stay off and the user must get an error.
    #[test]
    fn create_failure_keeps_the_toggle_off_and_returns_an_error() {
        let out = apply_autostart_toggle(true, fail("access denied"), || false);
        assert!(!out.enabled, "failed create must not show the toggle on");
        let error = out.error.expect("create failure must surface an error");
        assert!(
            error.contains("could not create"),
            "error must name the failed action: {error}"
        );
        assert!(
            error.contains("Startup folder exists and is writable"),
            "error must be actionable: {error}"
        );
        assert!(
            error.contains("access denied"),
            "error must keep the writer reason: {error}"
        );
        assert_eq!(out.diagnostic, Some(CREATE_FAILED));
    }

    /// A locked or read-only shortcut used to look like Disable did nothing.
    #[test]
    fn remove_failure_keeps_the_toggle_on_and_returns_an_error() {
        let out = apply_autostart_toggle(false, fail("sharing violation"), || true);
        assert!(out.enabled, "failed remove must not show the toggle off");
        let error = out.error.expect("remove failure must surface an error");
        assert!(
            error.contains("could not remove"),
            "error must name the failed action: {error}"
        );
        assert!(
            error.contains("not locked or read-only"),
            "error must be actionable: {error}"
        );
        assert!(
            error.contains("sharing violation"),
            "error must keep the writer reason: {error}"
        );
        assert_eq!(out.diagnostic, Some(REMOVE_FAILED));
    }

    /// The unit tests above pass a `&'static str`, which cannot catch a
    /// dropped anyhow cause chain. Production passes `tray::set_autostart`,
    /// which wraps the OS error in `.context("write the autostart
    /// shortcut")`; that context alone tells the user nothing.
    #[test]
    fn the_writer_reason_keeps_the_anyhow_cause_chain() {
        let out = apply_autostart_toggle(
            true,
            |_| {
                Err(anyhow::anyhow!("Access is denied. (os error 5)")
                    .context("write the autostart shortcut"))
            },
            || false,
        );
        let error = out.error.expect("create failure must surface an error");
        assert!(
            error.contains("Access is denied"),
            "the OS cause was dropped from the dialog: {error}"
        );
        assert!(
            error.contains("write the autostart shortcut"),
            "the outer context is still worth showing: {error}"
        );
    }

    /// Successful enable/disable must stay silent. The old path already
    /// worked when the shortcut write succeeded; do not add a dialog there.
    #[test]
    fn successful_enable_has_no_error() {
        let out = apply_autostart_toggle(true, succeed(), || true);
        assert!(out.enabled);
        assert_eq!(out.error, None);
        assert_eq!(out.diagnostic, None);
    }

    #[test]
    fn successful_disable_has_no_error() {
        let out = apply_autostart_toggle(false, succeed(), || false);
        assert!(!out.enabled);
        assert_eq!(out.error, None);
        assert_eq!(out.diagnostic, None);
    }

    /// "The writer returned Ok" is not "the shortcut is now in the requested
    /// state". A create that reports success while the file is still missing
    /// is unknown, not enabled.
    #[test]
    fn create_ok_but_shortcut_still_missing_is_an_error() {
        let out = apply_autostart_toggle(true, succeed(), || false);
        assert!(
            !out.enabled,
            "display the actual missing shortcut, not the request"
        );
        let error = out.error.expect("unverified create is not success");
        assert!(
            error.contains("still missing"),
            "must distinguish unverified success from a writer error: {error}"
        );
        assert_eq!(out.diagnostic, Some(CREATE_FAILED));
    }

    #[test]
    fn remove_ok_but_shortcut_still_present_is_an_error() {
        let out = apply_autostart_toggle(false, succeed(), || true);
        assert!(
            out.enabled,
            "display the actual remaining shortcut, not the request"
        );
        let error = out.error.expect("unverified remove is not success");
        assert!(
            error.contains("still present"),
            "must distinguish unverified success from a writer error: {error}"
        );
        assert_eq!(out.diagnostic, Some(REMOVE_FAILED));
    }

    /// The diagnostic line is what lands in matteshot.log. It must stay
    /// path-free so a support report cannot leak the user's profile folder.
    #[test]
    fn diagnostic_is_path_free_even_when_the_writer_error_has_a_path() {
        let out = apply_autostart_toggle(
            true,
            fail(
                r"failed to write C:\Users\someone\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup\Matteshot.lnk",
            ),
            || false,
        );
        let diagnostic = out.diagnostic.expect("create failure logs a diagnostic");
        assert!(
            !diagnostic.to_lowercase().contains("users"),
            "diagnostic leaked a profile path: {diagnostic}"
        );
        assert!(
            !diagnostic.to_lowercase().contains(".lnk"),
            "diagnostic leaked a shortcut path: {diagnostic}"
        );
        assert_eq!(diagnostic, CREATE_FAILED);
    }

    /// Create and remove must not collapse into one event. Support needs to
    /// tell a denied Startup folder from a locked existing shortcut.
    #[test]
    fn create_and_remove_diagnostics_are_distinct() {
        assert_ne!(CREATE_FAILED, REMOVE_FAILED);
        let create = apply_autostart_toggle(true, fail("x"), || false);
        let remove = apply_autostart_toggle(false, fail("x"), || true);
        assert_eq!(create.diagnostic, Some(CREATE_FAILED));
        assert_eq!(remove.diagnostic, Some(REMOVE_FAILED));
    }

    /// The writer must be asked for the requested state, not the current one.
    #[test]
    fn set_is_called_with_the_requested_state() {
        let mut seen = None;
        let out = apply_autostart_toggle(
            true,
            |want| {
                seen = Some(want);
                Ok::<(), &'static str>(())
            },
            || true,
        );
        assert_eq!(seen, Some(true));
        assert!(out.enabled);
        assert_eq!(out.error, None);
    }
}
