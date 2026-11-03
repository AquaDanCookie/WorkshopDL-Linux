use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub output_dir: String,
    pub username: String,
    pub anonymous: bool,
    pub auto_open: bool,
    pub window_width: u32,
    pub window_height: u32,
    pub provider: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            output_dir: String::new(),
            username: String::new(),
            anonymous: true,
            auto_open: true,
            window_width: 1073,
            window_height: 760,
            provider: "steamcmd".to_string(),
        }
    }
}

impl Settings {
    pub fn path(project: &Path) -> PathBuf {
        project.join("config").join("settings.json")
    }

    pub fn load(project: &Path) -> Self {
        let path = Self::path(project);
        let mut settings = match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<Settings>(&text).unwrap_or_default(),
            Err(_) => Settings::default(),
        };
        if settings.output_dir.trim().is_empty() {
            settings.output_dir = crate::util::default_output_dir(project)
                .to_string_lossy()
                .into_owned();
        }
        if settings.window_width < 800 {
            settings.window_width = 1000;
        }
        if settings.window_height < 560 {
            settings.window_height = 720;
        }
        if settings.provider != "ggntw" {
            settings.provider = "steamcmd".to_string();
        }
        settings
    }

    pub fn save(&self, project: &Path) {
        let path = Self::path(project);
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let Ok(text) = serde_json::to_string_pretty(self) else {
            return;
        };
        let tmp = path.with_extension("json.tmp");
        if fs::write(&tmp, text + "\n").is_ok() {
            let _ = fs::rename(tmp, path);
        }
    }
}
