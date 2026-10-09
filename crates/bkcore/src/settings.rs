//! Application settings — the handful of preferences that are not per-job.
//!
//! One JSON file, `settings.json`, in the configuration root. Everything has a
//! default, a missing or unreadable file yields the defaults rather than an
//! error (a broken preferences file must never stop a backup tool from
//! starting), and writes are atomic.

use crate::{BkError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const SETTINGS_VERSION: u32 = 1;
pub const FILE_NAME: &str = "settings.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "current_version")]
    pub version: u32,
    /// Ask GitHub for a newer release when the GUI starts.
    #[serde(default = "yes")]
    pub check_updates_on_start: bool,
    /// Also consider releases marked as pre-releases.
    #[serde(default)]
    pub beta_updates: bool,
    /// A version the user chose to skip. Offered again once something newer
    /// than it appears.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped_version: Option<String>,
    /// When the last update check ran, for the settings display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_update_check_unix: Option<u64>,
    #[serde(default)]
    pub theme: Theme,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    #[default]
    System,
    Dark,
    Light,
}

impl std::fmt::Display for Theme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Theme::System => "system",
            Theme::Dark => "dark",
            Theme::Light => "light",
        })
    }
}

fn current_version() -> u32 {
    SETTINGS_VERSION
}

fn yes() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            version: SETTINGS_VERSION,
            check_updates_on_start: true,
            beta_updates: false,
            skipped_version: None,
            last_update_check_unix: None,
            theme: Theme::System,
        }
    }
}

impl Settings {
    pub fn path_in(dir: &Path) -> PathBuf {
        dir.join(FILE_NAME)
    }

    /// The settings file in the default configuration root.
    pub fn path() -> PathBuf {
        Self::path_in(&crate::paths::config_root())
    }

    /// Load from `dir`, falling back to defaults when the file is missing or
    /// unreadable. An unreadable file is reported through `tracing`, not as an
    /// error: nothing in here is worth refusing to start over.
    pub fn load_from(dir: &Path) -> Settings {
        let path = Self::path_in(dir);
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Settings>(&bytes) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("{} is not valid; using defaults: {e}", path.display());
                    Settings::default()
                }
            },
            Err(_) => Settings::default(),
        }
    }

    pub fn load() -> Settings {
        Self::load_from(&crate::paths::config_root())
    }

    /// Write atomically into `dir`, creating it if needed.
    pub fn save_to(&self, dir: &Path) -> Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let path = Self::path_in(dir);
        let json = serde_json::to_vec_pretty(self).map_err(|e| BkError::Other(e.into()))?;
        let tmp = path.with_extension("json.tmp");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&json)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &path)?;
        if dir == crate::paths::config_root() {
            crate::paths::restrict_to_admins(&path);
        }
        Ok(path)
    }

    pub fn save(&self) -> Result<PathBuf> {
        self.save_to(&crate::paths::config_root())
    }

    /// The keys `set` understands, with their current values. Drives both the
    /// CLI's `settings show` and its tab completion of key names.
    pub fn entries(&self) -> Vec<(&'static str, String)> {
        vec![
            (
                "check_updates_on_start",
                self.check_updates_on_start.to_string(),
            ),
            ("beta_updates", self.beta_updates.to_string()),
            ("theme", self.theme.to_string()),
            (
                "skipped_version",
                self.skipped_version
                    .clone()
                    .unwrap_or_else(|| "none".into()),
            ),
        ]
    }

    /// Change one setting from text, as the CLI does.
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let bad = |what: &str| BkError::Other(anyhow::anyhow!("{what}"));
        match key {
            "check_updates_on_start" => self.check_updates_on_start = parse_bool(value)?,
            "beta_updates" => self.beta_updates = parse_bool(value)?,
            "theme" => {
                self.theme = match value.to_ascii_lowercase().as_str() {
                    "system" => Theme::System,
                    "dark" => Theme::Dark,
                    "light" => Theme::Light,
                    other => {
                        return Err(bad(&format!(
                            "theme must be system, dark or light, not '{other}'"
                        )))
                    }
                }
            }
            "skipped_version" => {
                self.skipped_version = match value {
                    "" | "none" | "null" => None,
                    v => Some(v.to_string()),
                }
            }
            other => {
                return Err(bad(&format!(
                    "unknown setting '{other}'; known: {}",
                    self.entries()
                        .iter()
                        .map(|(k, _)| *k)
                        .collect::<Vec<_>>()
                        .join(", ")
                )))
            }
        }
        Ok(())
    }
}

fn parse_bool(s: &str) -> Result<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        other => Err(BkError::Other(anyhow::anyhow!(
            "expected true or false, not '{other}'"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(n: &str) -> Self {
            let p = std::env::temp_dir().join(format!("bku_settings_{n}"));
            let _ = std::fs::remove_dir_all(&p);
            Tmp(p)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn missing_or_broken_files_yield_defaults() {
        let t = Tmp::new("missing");
        assert_eq!(Settings::load_from(&t.0), Settings::default());

        std::fs::create_dir_all(&t.0).unwrap();
        std::fs::write(Settings::path_in(&t.0), b"{ definitely not json").unwrap();
        assert_eq!(Settings::load_from(&t.0), Settings::default());
    }

    #[test]
    fn settings_round_trip_and_unknown_fields_are_tolerated() {
        let t = Tmp::new("roundtrip");
        let s = Settings {
            beta_updates: true,
            skipped_version: Some("1.2.3".into()),
            theme: Theme::Dark,
            ..Default::default()
        };
        s.save_to(&t.0).unwrap();
        assert_eq!(Settings::load_from(&t.0), s);

        // A newer build may add keys; an older build must still read the file.
        std::fs::write(
            Settings::path_in(&t.0),
            br#"{"beta_updates": true, "some_future_key": 42}"#,
        )
        .unwrap();
        let back = Settings::load_from(&t.0);
        assert!(back.beta_updates);
        assert!(back.check_updates_on_start, "missing keys take defaults");
    }

    #[test]
    fn set_parses_text_values_and_rejects_nonsense() {
        let mut s = Settings::default();
        s.set("beta_updates", "yes").unwrap();
        assert!(s.beta_updates);
        s.set("check_updates_on_start", "0").unwrap();
        assert!(!s.check_updates_on_start);
        s.set("theme", "Light").unwrap();
        assert_eq!(s.theme, Theme::Light);
        s.set("skipped_version", "none").unwrap();
        assert!(s.skipped_version.is_none());

        assert!(s.set("theme", "neon").is_err());
        assert!(s.set("beta_updates", "maybe").is_err());
        let err = s.set("nope", "1").unwrap_err().to_string();
        assert!(err.contains("unknown setting"), "{err}");
    }
}
