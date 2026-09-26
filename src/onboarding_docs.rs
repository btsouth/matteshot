//! SBS-907: README and INTERACTIVE-REGRESSION must match shipped first-run
//! and tray-menu behavior. Kept free of `windows` so `rustc --test` can
//! prove the claims on a Linux box.

#[cfg(test)]
mod tests {
    const README: &str = include_str!("../README.md");
    const REGRESSION: &str = include_str!("../INTERACTIVE-REGRESSION.md");

    /// First run is the welcome window. A balloon is only the failure path
    /// in `src/main.rs` after `welcome::open_first_run()` errors.
    #[test]
    fn readme_does_not_treat_a_balloon_as_first_run() {
        assert!(
            !README.contains("First run shows a tray balloon"),
            "README still describes a first-run balloon"
        );
        assert!(
            README.contains("compact native welcome surface"),
            "README must still describe the welcome surface"
        );
        assert!(
            README.contains("If that window fails to open, a tray balloon"),
            "README must describe the balloon as the welcome-failure fallback"
        );
    }

    /// `src/tray.rs` always appends delayed capture and History.
    ///
    /// Anchored to the tray-menu sentence itself. A bare `contains("History")`
    /// passes on any other mention in the file, so it would not notice the
    /// tray list losing the entry.
    #[test]
    fn readme_tray_menu_includes_delayed_capture_and_history() {
        assert!(
            README.contains("the active-window and delayed-capture variants"),
            "README tray-menu sentence must mention delayed capture"
        );
        assert!(
            README.contains("open captures/videos folders, History, and Settings"),
            "README tray-menu sentence must list History"
        );
    }

    /// Anchored to the tray-menu check line for the same reason as the
    /// README test: a loose `contains` passes on an unrelated mention.
    #[test]
    fn interactive_regression_tray_menu_includes_delayed_capture_and_history() {
        assert!(
            REGRESSION.contains(
                "Tray menu lists capture, active window, delayed capture, open captures/videos folders, History, and Settings"
            ),
            "interactive regression tray check must list delayed capture and History in order"
        );
        assert!(
            !REGRESSION.contains(
                "Tray menu is slim: capture, active window, open captures/videos folders, license, Settings"
            ),
            "interactive regression still lists the pre-History slim tray"
        );
    }

    /// The regression must not invent a first-run balloon.
    #[test]
    fn interactive_regression_does_not_invent_a_first_run_balloon() {
        assert!(
            !REGRESSION
                .to_ascii_lowercase()
                .contains("first-run balloon")
                && !REGRESSION
                    .to_ascii_lowercase()
                    .contains("first run balloon")
                && !REGRESSION.contains("First run shows a tray balloon"),
            "interactive regression invented a first-run balloon"
        );
        assert!(
            REGRESSION.contains("native welcome surface"),
            "interactive regression must check the shipped welcome surface"
        );
        assert!(
            REGRESSION.contains("`--welcome` opens the same surface"),
            "interactive regression must not treat --welcome as having a balloon fallback"
        );
    }
}
