//! Persistent settings at %APPDATA%\matteshot\config.json.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Config {
    /// Directory captures are saved to. Default: Pictures\Matteshot.
    pub save_dir: Option<PathBuf>,
    /// Index of the last style the user chose; the picker preselects it.
    pub last_style: usize,
    /// Export supersampling factor (1-4). Content is Lanczos-upscaled; the
    /// frame (gradient, corners, shadow) renders natively at this scale.
    pub export_scale: u32,
    /// Maximum pixel length of the finished screenshot's longest edge.
    /// Zero preserves the original output dimensions.
    pub output_max_edge: u32,
    /// First-run notification shown.
    pub onboarded: bool,
    /// Whether the resident should own PrtScn while capture is licensed.
    /// This is the user's preference, not the current Windows routing state.
    pub capture_prtscn: bool,
    /// Also write an animated GIF alongside recordings.
    pub record_gif: bool,
    /// Recording audio source: "off", "system", or "mic".
    pub record_audio: String,
    /// Recording frame rate. The UI intentionally offers only 30 or 60 FPS.
    pub record_fps: u32,
    /// Where recordings go. Default: Videos\Matteshot.
    pub video_dir: Option<PathBuf>,
    /// Install updates in the background instead of only announcing them.
    pub auto_update: bool,
    /// Whether anonymous usage telemetry may be sent.
    ///
    /// `None` means the question has not been answered, and nothing is sent
    /// while that is true. Consent has to be an actual choice: defaulting to
    /// on would have the first launch reporting before anyone could decline,
    /// which is the one thing that cannot be undone afterwards.
    ///
    /// Installs that already carry an explicit true or false keep it and are
    /// not asked again.
    pub telemetry: Option<bool>,
    /// The pseudonymous PostHog identity: a random UUID minted on the first
    /// event after consent. Deliberately not derived from `MachineGuid` or any
    /// licensing identifier — usage history must not be joinable to a
    /// customer, and it must not survive a reinstall. Cleared whenever
    /// telemetry is turned off, so turning it back on starts a fresh history.
    pub telemetry_id: Option<String>,
    /// Keep the tweak editor's tab open after Copy so the capture can keep
    /// being refined. Off restores the old close-after-copy behavior.
    pub keep_editor_open: bool,
    /// Shortcut for capturing the active window, as text ("Ctrl+Alt+S").
    /// "None" unbinds it. Text rather than a keycode so the file stays
    /// readable and a combo the Settings window does not offer can still be
    /// set by hand. See `crate::hotkey`.
    #[serde(default = "default_capture_hotkey")]
    pub capture_hotkey: String,
    /// Seconds the delayed capture waits before freezing the screen, so a
    /// menu or tooltip can be opened first. See `crate::delay`.
    #[serde(default = "default_capture_delay")]
    pub capture_delay_secs: u32,
}

fn default_capture_delay() -> u32 {
    crate::delay::DEFAULT_SECONDS
}

fn default_capture_hotkey() -> String {
    crate::hotkey::DEFAULT.to_owned()
}

impl Default for Config {
    fn default() -> Self {
        Config {
            save_dir: None,
            last_style: 0,
            export_scale: 1,
            output_max_edge: 0,
            onboarded: false,
            capture_prtscn: true,
            record_gif: false,
            record_audio: "off".into(),
            record_fps: crate::record::DEFAULT_FPS,
            video_dir: None,
            auto_update: true,
            telemetry: None,
            telemetry_id: None,
            keep_editor_open: true,
            capture_hotkey: default_capture_hotkey(),
            capture_delay_secs: default_capture_delay(),
        }
    }
}

impl Config {
    /// Telemetry only runs once someone has said yes. Unanswered is off.
    pub fn telemetry_enabled(&self) -> bool {
        self.telemetry == Some(true)
    }

    /// Whether the consent question still needs asking.
    pub fn telemetry_unanswered(&self) -> bool {
        self.telemetry.is_none()
    }

    /// Keep malformed or future config values from reaching capture timing.
    pub fn record_fps(&self) -> u32 {
        crate::record::sanitize_fps(self.record_fps)
    }

    /// The capture shortcut to register, or `None` when it is unbound.
    ///
    /// Unreadable text falls back to the default rather than leaving the app
    /// with no shortcut, because a typo in a config file should not silently
    /// remove a feature. "None" is honoured as written: choosing to unbind is
    /// not a mistake to correct.
    pub fn capture_hotkey(&self) -> Option<crate::hotkey::Hotkey> {
        let text = self.capture_hotkey.trim();
        if text.eq_ignore_ascii_case(crate::hotkey::NONE) {
            return None;
        }
        crate::hotkey::parse(text).or_else(|| crate::hotkey::parse(crate::hotkey::DEFAULT))
    }
}

fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("matteshot").join("config.json"))
}

const CONFIG_MUTEX: &str = "Local\\Matteshot.Config.State";

/// What was actually on disk this read. Missing is a fresh install; a present
/// file that will not parse is neither that nor last-known-good.
enum DiskConfig {
    Missing,
    Present(Config),
}

fn read_from(path: &Path) -> anyhow::Result<DiskConfig> {
    let body = match std::fs::read_to_string(path) {
        Ok(body) => body,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(DiskConfig::Missing),
        Err(error) => return Err(error.into()),
    };
    Ok(DiskConfig::Present(serde_json::from_str(&body)?))
}

fn load_from(path: &Path) -> anyhow::Result<Config> {
    match read_from(path) {
        Ok(DiskConfig::Missing) => Ok(Config::default()),
        Ok(DiskConfig::Present(config)) => Ok(config),
        Err(error) => Err(error),
    }
}

fn read_unlocked() -> anyhow::Result<DiskConfig> {
    let Some(path) = config_path() else {
        return Ok(DiskConfig::Missing);
    };
    read_from(&path)
}

/// Factory defaults turn auto-update and PrtScn on. A present file we cannot
/// read is not a fresh install, so those stay off until a later read succeeds.
/// `onboarded` is true so first-run welcome does not rewrite the file.
fn fail_closed() -> Config {
    Config {
        auto_update: false,
        capture_prtscn: false,
        telemetry: None,
        capture_hotkey: crate::hotkey::NONE.to_owned(),
        onboarded: true,
        ..Config::default()
    }
}

static LAST_GOOD: OnceLock<Mutex<Option<Config>>> = OnceLock::new();

fn last_good_slot() -> MutexGuard<'static, Option<Config>> {
    LAST_GOOD
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn remember(config: &Config) {
    *last_good_slot() = Some(config.clone());
}

/// Missing stays a fresh install and is not cached. A successful parse becomes
/// last-known-good. An unreadable present file must not look like defaults.
fn resolve_load(last_good: &mut Option<Config>, from_disk: anyhow::Result<DiskConfig>) -> Config {
    match from_disk {
        Ok(DiskConfig::Missing) => {
            *last_good = None;
            Config::default()
        }
        Ok(DiskConfig::Present(config)) => {
            *last_good = Some(config.clone());
            config
        }
        Err(_) => last_good.clone().unwrap_or_else(fail_closed),
    }
}

/// True only when this disk read succeeded and the user left auto-update on.
/// Last-known-good is for the session, not for authorizing a silent install.
pub fn auto_update_from_load<E>(result: Result<Config, E>) -> bool {
    matches!(result, Ok(config) if config.auto_update)
}

fn corrupt_backup_path(path: &Path) -> PathBuf {
    // One sidecar, not a unique sibling per retry. A failed rewrite leaves the
    // live path unreadable, so the next Config::update would otherwise copy
    // again and fill %APPDATA%\matteshot.
    path.with_extension("json.corrupt")
}

fn load_for_update_from(path: &Path, last_good: Option<&Config>) -> anyhow::Result<Config> {
    match load_from(path) {
        Ok(config) => Ok(config),
        Err(error) if error.downcast_ref::<serde_json::Error>().is_some() => {
            let Some(last) = last_good.cloned() else {
                return Err(error);
            };
            // Copy, do not rename. The live path must stay present until the
            // last-good rewrite is durable. A rename plus a failed save leaves
            // the path missing; the next load/try_load then treats that as a
            // fresh install and re-authorizes silent install (SBS-910).
            let backup = corrupt_backup_path(path);
            // Unlink first so a planted symlink is dropped, not followed.
            let _ = std::fs::remove_file(&backup);
            std::fs::copy(path, &backup)?;
            crate::diagnostics::log(
                "corrupt config was quarantined; restoring last-known-good settings",
            );
            Ok(last)
        }
        Err(error) => Err(error),
    }
}

fn load_for_update_unlocked() -> anyhow::Result<Config> {
    let Some(path) = config_path() else {
        return Ok(Config::default());
    };
    let last = last_good_slot().clone();
    load_for_update_from(&path, last.as_ref())
}

fn save_unlocked(config: &Config) -> anyhow::Result<()> {
    let Some(path) = config_path() else { return Ok(()) };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_vec_pretty(config)?;
    crate::state_lock::atomic_write(&path, &json)
}

impl Config {
    pub fn load() -> Config {
        let _guard = crate::state_lock::lock(CONFIG_MUTEX).ok();
        let from_disk = read_unlocked();
        if let Err(error) = &from_disk {
            crate::diagnostics::log(&format!("config could not be loaded: {error:#}"));
        }
        resolve_load(&mut last_good_slot(), from_disk)
    }

    /// This read only. Used by the updater so a stale last-known-good cannot
    /// authorize a silent install after the file has become unreadable.
    /// A parse or I/O error is not "use Pictures\Matteshot".
    pub fn try_load() -> anyhow::Result<Config> {
        let _guard = crate::state_lock::lock(CONFIG_MUTEX).ok();
        match read_unlocked() {
            Ok(DiskConfig::Missing) => {
                *last_good_slot() = None;
                Ok(Config::default())
            }
            Ok(DiskConfig::Present(config)) => {
                remember(&config);
                Ok(config)
            }
            Err(error) => Err(error),
        }
    }

    /// Atomically update only the fields owned by one action. This prevents a
    /// long-lived Settings window from overwriting a newer last-used matte.
    pub fn update(change: impl FnOnce(&mut Config)) -> anyhow::Result<Config> {
        let _guard = crate::state_lock::lock(CONFIG_MUTEX).ok();
        let mut config = load_for_update_unlocked().map_err(|error| {
            crate::diagnostics::log(&format!(
                "config update failed because existing state could not be loaded: {error:#}"
            ));
            error
        })?;
        change(&mut config);
        save_unlocked(&config)?;
        remember(&config);
        Ok(config)
    }

    pub fn save_dir(&self) -> PathBuf {
        self.save_dir.clone().unwrap_or_else(|| {
            dirs::picture_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("Matteshot")
        })
    }

    /// Recordings live with videos, not screenshots.
    pub fn video_dir(&self) -> PathBuf {
        self.video_dir.clone().unwrap_or_else(|| {
            dirs::video_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("Matteshot")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_path(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "matteshot-config-{name}-{}-{unique}.json",
            std::process::id()
        ))
    }

    #[test]
    fn a_missing_config_is_a_fresh_install() {
        let path = temporary_path("missing");
        let config = load_from(&path).expect("missing config uses defaults");
        assert_eq!(config.capture_hotkey, crate::hotkey::DEFAULT);
        assert_eq!(config.record_fps(), crate::record::DEFAULT_FPS);
        assert!(!path.exists());
    }

    #[test]
    fn recording_frame_rate_accepts_only_supported_values() {
        let mut config = Config::default();
        assert_eq!(config.record_fps(), 30);
        config.record_fps = 60;
        assert_eq!(config.record_fps(), 60);
        config.record_fps = 144;
        assert_eq!(config.record_fps(), 30);
    }

    #[test]
    fn a_corrupt_config_is_not_mistaken_for_defaults() {
        let path = temporary_path("corrupt");
        std::fs::write(&path, b"{ definitely not json").unwrap();
        assert!(load_from(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{ definitely not json");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn try_load_from_corrupt_json_is_err_not_default_pictures_path() {
        let path = temporary_path("try-load-corrupt");
        std::fs::write(&path, b"{ definitely not json").unwrap();
        assert!(
            read_from(&path).is_err(),
            "try_load reads via read_from; corrupt json must not become Config::default()"
        );
        let default_save = Config::default().save_dir();
        assert!(
            default_save
                .file_name()
                .is_some_and(|name| name == "Matteshot"),
            "the default path try_load must not invent is {default_save:?}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn updating_a_corrupt_config_without_last_good_leaves_the_file_alone() {
        let path = temporary_path("recover");
        let corrupt = b"{ definitely not json";
        std::fs::write(&path, corrupt).unwrap();
        assert!(
            load_for_update_from(&path, None).is_err(),
            "without last-known-good, a parse error must not write factory defaults"
        );
        assert_eq!(std::fs::read(&path).unwrap(), corrupt);
        assert!(
            corrupt_siblings(&path).is_empty(),
            "a failed update must not quarantine the only copy"
        );

        let _ = std::fs::remove_file(path);
    }

    /// The whole point: a fresh install must not report anything before the
    /// question has been answered.
    #[test]
    fn telemetry_is_off_until_the_question_is_answered() {
        let fresh: Config = serde_json::from_str("{}").expect("empty config");
        assert!(fresh.telemetry_unanswered(), "a fresh install must be unanswered");
        assert!(!fresh.telemetry_enabled(), "nothing may be sent before consent");
    }

    #[test]
    fn an_existing_answer_is_kept_and_not_re_asked() {
        for (json, expected) in [
            (r#"{"telemetry":true}"#, true),
            (r#"{"telemetry":false}"#, false),
        ] {
            let cfg: Config = serde_json::from_str(json).expect(json);
            assert!(!cfg.telemetry_unanswered(), "{json} was already answered");
            assert_eq!(cfg.telemetry_enabled(), expected, "for {json}");
        }
    }

    #[test]
    fn declining_is_distinct_from_never_asked() {
        // Otherwise a decline would put the question back the next time round.
        let declined = Config { telemetry: Some(false), ..Default::default() };
        assert!(!declined.telemetry_enabled());
        assert!(!declined.telemetry_unanswered());
    }

    #[test]
    fn a_missing_shortcut_setting_uses_the_default() {
        // Configs written before the setting existed have no such key, and
        // serde's default has to supply one or every upgrade loses its
        // shortcut.
        let cfg: Config = serde_json::from_str("{}").expect("empty config");
        assert_eq!(cfg.capture_hotkey, crate::hotkey::DEFAULT);
        assert!(cfg.capture_hotkey().is_some());
    }

    #[test]
    fn unreadable_text_falls_back_rather_than_unbinding() {
        let cfg = Config {
            capture_hotkey: "Ctrl+Nonsense".into(),
            ..Default::default()
        };
        // A typo should not silently remove the feature.
        assert_eq!(cfg.capture_hotkey(), crate::hotkey::parse(crate::hotkey::DEFAULT));
    }

    #[test]
    fn choosing_none_is_honoured_not_corrected() {
        for text in ["None", "none", " NONE "] {
            let cfg = Config {
                capture_hotkey: text.into(),
                ..Default::default()
            };
            assert_eq!(cfg.capture_hotkey(), None, "for {text:?}");
        }
    }

    #[test]
    fn a_custom_shortcut_is_used_as_written() {
        let cfg = Config {
            capture_hotkey: "Ctrl+Shift+F9".into(),
            ..Default::default()
        };
        let parsed = cfg.capture_hotkey().expect("custom shortcut");
        assert_eq!(parsed.vk, 0x70 + 8);
    }

    fn write_fixture(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
    }

    fn load_session(path: &Path, last_good: &mut Option<Config>) -> Config {
        resolve_load(last_good, read_from(path))
    }

    fn corrupt_siblings(path: &Path) -> Vec<PathBuf> {
        let name = path.file_name().unwrap().to_string_lossy();
        let backup = path.parent().unwrap().join(format!("{name}.corrupt"));
        if backup.exists() {
            vec![backup]
        } else {
            Vec::new()
        }
    }

    fn opted_out_fixture() -> &'static str {
        r#"{
            "auto_update": false,
            "capture_prtscn": false,
            "capture_hotkey": "Ctrl+Shift+F9",
            "telemetry": false,
            "capture_delay_secs": 7
        }"#
    }

    #[test]
    fn a_missing_config_through_the_session_resolver_is_still_a_fresh_install() {
        let path = temporary_path("session-missing");
        let mut last_good = None;
        let config = load_session(&path, &mut last_good);
        assert!(config.auto_update);
        assert!(config.capture_prtscn);
        assert_eq!(config.capture_hotkey, crate::hotkey::DEFAULT);
        assert!(
            last_good.is_none(),
            "absence is not a successful read of user settings"
        );
        assert!(!path.exists());
    }

    #[test]
    fn an_unreadable_present_file_is_not_a_fresh_install() {
        let path = temporary_path("unreadable-first");
        write_fixture(&path, "{");
        let mut last_good = None;
        let config = load_session(&path, &mut last_good);
        assert!(
            last_good.is_none(),
            "a failed first read must not invent last-known-good"
        );
        assert!(!config.auto_update);
        assert!(!config.capture_prtscn);
        assert_eq!(config.capture_hotkey, crate::hotkey::NONE);
        assert!(
            config.onboarded,
            "a present unreadable file is not first-run and must not open welcome"
        );
        assert!(config.telemetry_unanswered());
        assert!(!config.telemetry_enabled());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_directory_at_the_config_path_is_not_a_fresh_install() {
        let path = temporary_path("unreadable-dir");
        std::fs::create_dir_all(&path).unwrap();
        let mut last_good = None;
        let config = load_session(&path, &mut last_good);
        assert!(!config.auto_update);
        assert!(!config.capture_prtscn);
        assert_eq!(config.capture_hotkey, crate::hotkey::NONE);
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn fail_closed_is_not_a_first_run() {
        let config = fail_closed();
        assert!(config.onboarded);
        assert!(!config.auto_update);
        assert!(!config.capture_prtscn);
        assert_eq!(config.capture_hotkey, crate::hotkey::NONE);
    }

    #[test]
    fn missing_clears_stale_last_good_so_a_later_corrupt_read_fails_closed() {
        let path = temporary_path("missing-then-corrupt");
        write_fixture(&path, opted_out_fixture());
        let mut last_good = None;
        let first = load_session(&path, &mut last_good);
        assert!(!first.auto_update);
        assert!(last_good.is_some());

        let _ = std::fs::remove_file(&path);
        let missing = load_session(&path, &mut last_good);
        assert!(missing.auto_update, "absence is a fresh install");
        assert!(
            last_good.is_none(),
            "a missing file must not keep a previous session's settings"
        );

        write_fixture(&path, "{");
        let corrupt = load_session(&path, &mut last_good);
        assert!(!corrupt.auto_update);
        assert_eq!(corrupt.capture_hotkey, crate::hotkey::NONE);
        assert!(corrupt.onboarded);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn updating_after_a_good_read_restores_last_good_instead_of_factory_defaults() {
        let path = temporary_path("update-from-last-good");
        write_fixture(&path, opted_out_fixture());
        let mut last_good = None;
        let loaded = load_session(&path, &mut last_good);

        write_fixture(&path, "{");
        let mut config =
            load_for_update_from(&path, last_good.as_ref()).expect("seed from last-known-good");
        assert!(!config.auto_update);
        assert_eq!(config.capture_hotkey, "Ctrl+Shift+F9");
        assert_eq!(config.telemetry, Some(false));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"{",
            "quarantine must leave the live path in place until the rewrite is durable"
        );
        config.last_style = 3;
        let json = serde_json::to_vec_pretty(&config).unwrap();
        crate::state_lock::atomic_write(&path, &json).unwrap();

        let saved = load_from(&path).expect("updated last-known-good");
        assert!(!saved.auto_update);
        assert_eq!(saved.capture_hotkey, "Ctrl+Shift+F9");
        assert_eq!(saved.telemetry, Some(false));
        assert_eq!(saved.last_style, 3);
        assert_eq!(loaded.capture_delay_secs, 7);
        assert_eq!(saved.capture_delay_secs, 7);
        for backup in corrupt_siblings(&path) {
            let _ = std::fs::remove_file(backup);
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_later_unreadable_read_keeps_the_last_good_session() {
        let path = temporary_path("keep-last-good");
        write_fixture(&path, opted_out_fixture());
        let mut last_good = None;
        let first = load_session(&path, &mut last_good);
        assert!(!first.auto_update);
        assert!(!first.capture_prtscn);
        assert_eq!(first.capture_hotkey, "Ctrl+Shift+F9");
        assert_eq!(first.telemetry, Some(false));
        assert_eq!(first.capture_delay_secs, 7);

        write_fixture(&path, "{");
        let second = load_session(&path, &mut last_good);
        assert!(!second.auto_update);
        assert!(!second.capture_prtscn);
        assert_eq!(second.capture_hotkey, "Ctrl+Shift+F9");
        assert_eq!(second.telemetry, Some(false));
        assert_eq!(second.capture_delay_secs, 7);

        let _ = std::fs::remove_file(&path);
        std::fs::create_dir_all(&path).unwrap();
        let third = load_session(&path, &mut last_good);
        assert!(!third.auto_update);
        assert!(!third.capture_prtscn);
        assert_eq!(third.capture_hotkey, "Ctrl+Shift+F9");
        assert_eq!(third.telemetry, Some(false));
        assert_eq!(third.capture_delay_secs, 7);
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn loading_a_corrupt_config_does_not_quarantine_it() {
        let path = temporary_path("no-quarantine-on-read");
        write_fixture(&path, "{ definitely not json");
        let mut last_good = None;
        let _ = load_session(&path, &mut last_good);
        assert!(load_from(&path).is_err());
        assert!(
            corrupt_siblings(&path).is_empty(),
            "a background read must leave the bytes in place"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn auto_update_is_authorized_only_on_a_successful_opt_in_read() {
        let missing = temporary_path("auto-update-missing");
        assert!(
            auto_update_from_load(load_from(&missing)),
            "a missing file is a fresh install, which ships with auto-update on"
        );

        let on = temporary_path("auto-update-on");
        write_fixture(&on, r#"{"auto_update":true}"#);
        assert!(auto_update_from_load(load_from(&on)));
        let _ = std::fs::remove_file(&on);

        let off = temporary_path("auto-update-off");
        write_fixture(&off, opted_out_fixture());
        assert!(!auto_update_from_load(load_from(&off)));

        write_fixture(&off, "{");
        assert!(
            !auto_update_from_load(load_from(&off)),
            "truncated bytes must not authorize a silent install"
        );
        let _ = std::fs::remove_file(&off);

        let locked = temporary_path("auto-update-locked");
        std::fs::create_dir_all(&locked).unwrap();
        assert!(
            !auto_update_from_load(load_from(&locked)),
            "a locked or unreadable path must not authorize a silent install"
        );
        let _ = std::fs::remove_dir_all(locked);
    }

    /// SBS-910: renaming the live file aside before the last-good rewrite is
    /// durable turns a failed save into a missing file. The next load/try_load
    /// then clears last-good and returns factory defaults, which re-authorizes
    /// silent install — a bypass of SBS-856.
    #[test]
    fn a_failed_save_after_quarantine_must_not_look_like_a_fresh_install() {
        let path = temporary_path("failed-save-after-quarantine");
        write_fixture(&path, opted_out_fixture());
        let mut last_good = None;
        let loaded = load_session(&path, &mut last_good);
        assert!(!loaded.auto_update);
        assert!(last_good.is_some());

        write_fixture(&path, "{");
        let recovered =
            load_for_update_from(&path, last_good.as_ref()).expect("seed from last-known-good");
        assert!(!recovered.auto_update);
        assert_eq!(recovered.capture_hotkey, "Ctrl+Shift+F9");

        // Failed save: the rewrite never becomes durable. The live path must
        // still be a present unreadable file. Missing is the fresh-install
        // arm in both resolve_load and try_load.
        assert!(
            path.exists(),
            "quarantine must not remove the live path before the rewrite is durable"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"{");
        assert!(
            !corrupt_siblings(&path).is_empty(),
            "the corrupt bytes should still be preserved aside"
        );

        let from_disk = read_from(&path);
        assert!(
            from_disk.is_err(),
            "the live path must still be a present unreadable file, not Missing"
        );
        let after = resolve_load(&mut last_good, from_disk);
        assert!(
            last_good.is_some(),
            "a failed rewrite must not clear last-known-good"
        );
        assert!(!after.auto_update);
        assert_eq!(after.capture_hotkey, "Ctrl+Shift+F9");
        assert!(
            !auto_update_from_load(load_from(&path)),
            "a failed rewrite must not authorize a silent install"
        );

        for backup in corrupt_siblings(&path) {
            let _ = std::fs::remove_file(backup);
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_failed_rewrite_does_not_accumulate_corrupt_siblings() {
        let path = temporary_path("one-corrupt-sidecar");
        write_fixture(&path, opted_out_fixture());
        let mut last_good = None;
        load_session(&path, &mut last_good);
        write_fixture(&path, "{");

        load_for_update_from(&path, last_good.as_ref()).expect("first restore");
        load_for_update_from(&path, last_good.as_ref()).expect("retry restore");
        assert_eq!(
            corrupt_siblings(&path).len(),
            1,
            "retries must overwrite one sidecar, not mint a new sibling"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"{");

        for backup in corrupt_siblings(&path) {
            let _ = std::fs::remove_file(backup);
        }
        let _ = std::fs::remove_file(path);
    }
}
