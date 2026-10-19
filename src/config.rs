//! Optional user config file (spec §10 "Config and secrets").

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OpenMode {
    App,
    Web,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub channels: Vec<String>,
    pub watch_emoji: Vec<String>,
    pub extra_mention_queries: Vec<String>,
    pub lookback_days: u32,
    pub channel_lookback_hours: u32,
    pub refresh_secs: u64,
    pub retention_days: u32,
    pub open_mode: OpenMode,
    pub open_clears_watch: bool,
    pub ignore_users: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            channels: Vec::new(),
            watch_emoji: vec!["eyes".into()],
            extra_mention_queries: Vec::new(),
            lookback_days: 7,
            channel_lookback_hours: 48,
            refresh_secs: 180,
            retention_days: 14,
            open_mode: OpenMode::App,
            open_clears_watch: false,
            ignore_users: Vec::new(),
        }
    }
}

/// Missing file -> defaults. Unreadable or malformed file -> error.
pub fn load(path: &Path) -> Result<Config, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(format!("config.json: {e}")),
    };
    serde_json::from_slice(&bytes).map_err(|e| format!("config.json: {e}"))
}

/// `$XDG_CONFIG_HOME` (absolute only), else `$HOME/.config`, then `herdr-slack/config.json`.
fn config_home(xdg: Option<OsString>, home: Option<OsString>) -> PathBuf {
    xdg.map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(home.unwrap_or_default()).join(".config"))
}

pub fn default_path() -> PathBuf {
    config_home(env::var_os("XDG_CONFIG_HOME"), env::var_os("HOME")).join("herdr-slack/config.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(json: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        fs::write(&path, json).unwrap();
        (tmp, path)
    }

    #[test]
    fn config_partial_file_keeps_defaults() {
        let (_tmp, path) = write(r##"{"channels":["#incidents"]}"##);
        let c = load(&path).unwrap();
        assert_eq!(c.channels, vec!["#incidents".to_string()]);
        assert_eq!(c.refresh_secs, 180);
        assert_eq!(c.watch_emoji, vec!["eyes".to_string()]);
    }

    #[test]
    fn config_missing_is_default() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            load(&tmp.path().join("nope.json")).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn config_bad_json_errors() {
        let (_tmp, path) = write("{not json");
        assert!(load(&path).unwrap_err().starts_with("config.json: "));
    }

    #[test]
    fn default_path_is_under_herdr_slack() {
        assert!(default_path().ends_with("herdr-slack/config.json"));
    }

    #[test]
    fn open_mode_is_lowercase() {
        let (_tmp, path) = write(r#"{"open_mode":"web"}"#);
        assert_eq!(load(&path).unwrap().open_mode, OpenMode::Web);
    }

    #[test]
    fn config_home_ignores_relative_or_empty_xdg() {
        let some = |s: &str| Some(OsString::from(s));
        assert_eq!(
            config_home(some("/x/cfg"), some("/home/j")),
            PathBuf::from("/x/cfg")
        );
        let fallback = PathBuf::from("/home/j/.config");
        assert_eq!(config_home(None, some("/home/j")), fallback);
        assert_eq!(config_home(some(""), some("/home/j")), fallback);
        assert_eq!(config_home(some("cfg"), some("/home/j")), fallback);
    }
}
