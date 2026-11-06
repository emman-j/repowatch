//! Portable storage: everything lives in `.repowatch/` next to the executable.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A named group of repositories (its own tab in the UI).
#[derive(Serialize, Deserialize, Clone)]
pub struct Profile {
    pub name: String,
    pub repos: Vec<PathBuf>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Config {
    pub profiles: Vec<Profile>,
    pub active: usize,
    pub auto_refresh: bool,
    pub interval_secs: u64,
    pub font_size: f32,
    /// "<profile>\u{1}<group>" keys of folder groups the user collapsed.
    pub collapsed: Vec<String>,
    /// Pre-profiles config format; migrated into a "Default" profile on load.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        let mut c = Self {
            profiles: Vec::new(),
            active: 0,
            auto_refresh: true,
            interval_secs: 60,
            font_size: 14.0,
            collapsed: Vec::new(),
            repos: Vec::new(),
        };
        c.normalize();
        c
    }
}

impl Config {
    /// Migrate old configs and keep `active` valid. There is always >= 1 profile.
    pub fn normalize(&mut self) {
        if self.profiles.is_empty() {
            self.profiles.push(Profile {
                name: "Default".into(),
                repos: std::mem::take(&mut self.repos),
            });
        } else if !self.repos.is_empty() {
            // old repos alongside profiles: keep them, don't lose anything
            let old = std::mem::take(&mut self.repos);
            for r in old {
                if !self.profiles[0].repos.contains(&r) {
                    self.profiles[0].repos.push(r);
                }
            }
        }
        if self.active >= self.profiles.len() {
            self.active = 0;
        }
        if !self.font_size.is_finite() {
            self.font_size = 14.0;
        }
        self.font_size = self.font_size.clamp(9.0, 32.0);
    }

    pub fn repos(&self) -> &Vec<PathBuf> {
        &self.profiles[self.active].repos
    }

    pub fn repos_mut(&mut self) -> &mut Vec<PathBuf> {
        &mut self.profiles[self.active].repos
    }
}

pub struct Store {
    pub dir: PathBuf,
    pub error: Option<String>,
}

impl Store {
    pub fn open() -> (Self, Config) {
        let base = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));
        let dir = base.join(".repowatch");
        let mut store = Store { dir, error: None };
        if let Err(e) = std::fs::create_dir_all(&store.dir) {
            store.error = Some(format!(
                "Cannot create {} ({e}). Changes won't be saved - move the app to a writable folder.",
                store.dir.display()
            ));
        }
        let mut cfg: Config = std::fs::read_to_string(store.file())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        cfg.normalize();
        (store, cfg)
    }

    fn file(&self) -> PathBuf {
        self.dir.join("config.json")
    }

    pub fn save(&mut self, cfg: &Config) {
        let res = serde_json::to_string_pretty(cfg)
            .map_err(|e| e.to_string())
            .and_then(|s| {
                // write to temp then rename so a crash can't corrupt the config
                let tmp = self.dir.join("config.json.tmp");
                std::fs::write(&tmp, s).map_err(|e| e.to_string())?;
                std::fs::rename(&tmp, self.file()).map_err(|e| e.to_string())
            });
        self.error = res.err().map(|e| format!("Could not save settings: {e}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_old_config_into_default_profile() {
        let mut c: Config =
            serde_json::from_str(r#"{"repos":["/a","/b"],"auto_refresh":false,"interval_secs":30}"#).unwrap();
        c.normalize();
        assert_eq!(c.profiles.len(), 1);
        assert_eq!(c.profiles[0].name, "Default");
        assert_eq!(c.repos().len(), 2);
        assert!(!c.auto_refresh);
        assert!(c.repos.is_empty());
        // legacy field is no longer written
        let v: serde_json::Value = serde_json::to_value(&c).unwrap();
        assert!(v.get("repos").is_none(), "legacy top-level repos must not be saved");
        assert_eq!(v["profiles"][0]["repos"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn profiles_roundtrip_and_active_is_clamped() {
        let mut c: Config = serde_json::from_str(
            r#"{"profiles":[{"name":"P1","repos":["/x"]},{"name":"P2","repos":[]}],"active":9}"#,
        )
        .unwrap();
        c.normalize();
        assert_eq!(c.active, 0);
        assert_eq!(c.profiles.len(), 2);
        assert_eq!(c.repos().len(), 1);
    }
}
