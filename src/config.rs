//! Persistent settings at %APPDATA%\matteshot\config.json.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

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

fn load_from(path: &Path) -> anyhow::Result<Config> {
    let body = match std::fs::read_to_string(path) {
        Ok(body) => body,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) => return Err(error.into()),
    };
    Ok(serde_json::from_str(&body)?)
}

fn load_unlocked() -> anyhow::Result<Config> {
    let Some(path) = config_path() else {
        return Ok(Config::default());
    };
    load_from(&path)
}

fn corrupt_backup_path(path: &Path) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.with_extension(format!("json.corrupt-{unique}"))
}

fn load_for_update_from(path: &Path) -> anyhow::Result<Config> {
    match load_from(path) {
        Ok(config) => Ok(config),
        Err(error) if error.downcast_ref::<serde_json::Error>().is_some() => {
            let backup = corrupt_backup_path(path);
            std::fs::rename(path, &backup)?;
            crate::diagnostics::log("corrupt config was quarantined before resetting settings");
            Ok(Config::default())
        }
        Err(error) => Err(error),
    }
}

fn load_for_update_unlocked() -> anyhow::Result<Config> {
    let Some(path) = config_path() else {
        return Ok(Config::default());
    };
    load_for_update_from(&path)
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
        load_unlocked().unwrap_or_else(|error| {
            crate::diagnostics::log(&format!("config could not be loaded: {error:#}"));
            Config::default()
        })
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
    fn updating_a_corrupt_config_quarantines_it_before_resetting() {
        let path = temporary_path("recover");
        let corrupt = b"{ definitely not json";
        std::fs::write(&path, corrupt).unwrap();

        let mut config = load_for_update_from(&path).expect("quarantine corrupt config");
        config.capture_hotkey = "Ctrl+Shift+F9".into();
        let json = serde_json::to_vec_pretty(&config).unwrap();
        crate::state_lock::atomic_write(&path, &json).unwrap();

        let recovered = load_from(&path).expect("read recovered config");
        assert_eq!(recovered.capture_hotkey, "Ctrl+Shift+F9");
        let backup = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!(
                        "{}.corrupt-",
                        path.file_name().unwrap().to_string_lossy()
                    ))
            })
            .expect("corrupt backup");
        assert_eq!(std::fs::read(backup.path()).unwrap(), corrupt);
        let _ = std::fs::remove_file(backup.path());
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
}
