//! Persistent settings at %APPDATA%\matteshot\config.json.

use std::path::PathBuf;

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
    /// First-run notification shown.
    pub onboarded: bool,
    /// Whether the resident should own PrtScn while capture is licensed.
    /// This is the user's preference, not the current Windows routing state.
    pub capture_prtscn: bool,
    /// Also write an animated GIF alongside recordings.
    pub record_gif: bool,
    /// Recording audio source: "off", "system", or "mic".
    pub record_audio: String,
    /// Where recordings go. Default: Videos\Matteshot.
    pub video_dir: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            save_dir: None,
            last_style: 0,
            export_scale: 1,
            onboarded: false,
            capture_prtscn: true,
            record_gif: false,
            record_audio: "off".into(),
            video_dir: None,
        }
    }
}

fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("matteshot").join("config.json"))
}

const CONFIG_MUTEX: &str = "Local\\Matteshot.Config.State";

fn load_unlocked() -> Config {
    config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_unlocked(config: &Config) {
    let Some(path) = config_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_vec_pretty(config) {
        let _ = crate::state_lock::atomic_write(&path, &json);
    }
}

impl Config {
    pub fn load() -> Config {
        let _guard = crate::state_lock::lock(CONFIG_MUTEX).ok();
        load_unlocked()
    }

    /// Atomically update only the fields owned by one action. This prevents a
    /// long-lived Settings window from overwriting a newer last-used matte.
    pub fn update(change: impl FnOnce(&mut Config)) -> Config {
        let _guard = crate::state_lock::lock(CONFIG_MUTEX).ok();
        let mut config = load_unlocked();
        change(&mut config);
        save_unlocked(&config);
        config
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
