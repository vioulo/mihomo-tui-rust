//! Filesystem layout for everything mihomo-tui persists.
//!
//! Every artifact the subscription feature owns — the profile index, the
//! downloaded snapshots, the editable `base.yaml`, the generated
//! `runtime.yaml`, and the TUI's own settings — lives under one config
//! directory, so `--config-dir` can relocate the whole state tree at once.
//! Where that directory is follows one precedence chain, resolved here and
//! nowhere else.

use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Where mihomo-tui keeps its state, and the files inside it.
#[derive(Clone, Debug)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    /// Explicit `--config-dir` beats `MIHOMO_TUI_CONFIG_DIR`, which beats the
    /// XDG default. Only the last leg can fail (no usable home directory).
    pub fn resolve(config_dir: Option<&str>) -> Result<Self> {
        let root = match config_dir {
            Some(dir) => PathBuf::from(dir),
            None => default_root()?,
        };
        Ok(Self { root })
    }

    /// The user's own config, the thing they edit.
    pub fn base_yaml(&self) -> PathBuf {
        self.root.join("base.yaml")
    }

    /// What we actually hand to the controller.
    pub fn runtime_yaml(&self) -> PathBuf {
        self.root.join("runtime.yaml")
    }

    /// Index of imported subscriptions.
    pub fn profiles_json(&self) -> PathBuf {
        self.root.join("profiles.json")
    }

    /// Per-profile snapshots of the downloaded raw text.
    pub fn subscriptions_dir(&self) -> PathBuf {
        self.root.join("subscriptions")
    }

    /// The TUI's own settings (see [`TuiConfig`]).
    // Read by the TUI at startup once settings are wired (task 14).
    #[allow(dead_code)]
    pub fn config_json(&self) -> PathBuf {
        self.root.join("config.json")
    }

    /// Create the root and the one subdirectory with a fixed name.
    pub fn ensure(&self) -> Result<()> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("creating {}", self.root.display()))?;
        let subscriptions = self.subscriptions_dir();
        fs::create_dir_all(&subscriptions)
            .with_context(|| format!("creating {}", subscriptions.display()))?;
        Ok(())
    }
}

/// The precedence chain for "no explicit directory given":
/// `MIHOMO_TUI_CONFIG_DIR` > `XDG_CONFIG_HOME` > `$HOME/.config`.
fn default_root() -> Result<PathBuf> {
    // An empty variable means "unset", matching how the XDG spec treats it.
    if let Some(dir) = env::var_os("MIHOMO_TUI_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }

    // XDG requires absolute paths; ignore anything else rather than resolving
    // against wherever the TUI happened to be started from.
    if let Some(dir) = env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return Ok(dir.join("mihomo-tui"));
        }
    }

    let home = env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .context("$HOME is not set; pass --config-dir to choose a location")?;
    Ok(PathBuf::from(home).join(".config").join("mihomo-tui"))
}

/// TUI-only settings, stored in `config.json`. A file written by another
/// version may lack fields; `serde(default)` fills those in rather than
/// discarding what is there.
// Settings are persisted and read back by the TUI once they are wired
// (task 14: concurrency; mouse follows with the UI work).
#[allow(dead_code)]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiConfig {
    pub concurrency: usize,
    pub mouse: bool,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            concurrency: 8,
            mouse: true,
        }
    }
}

#[allow(dead_code)]
impl TuiConfig {
    /// Missing or corrupt file means defaults — settings are never worth
    /// blocking the TUI over.
    pub fn load(p: &Paths) -> Self {
        fs::read_to_string(p.config_json())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Persist settings as plain JSON with ordinary permissions; nothing
    /// secret lives here. [`Paths::ensure`] must have run first.
    pub fn save(&self, p: &Paths) -> Result<()> {
        let text = serde_json::to_string_pretty(self).context("serializing tui settings")?;
        fs::write(p.config_json(), text)
            .with_context(|| format!("writing {}", p.config_json().display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};
    use tempfile::TempDir;

    /// Env vars are process-global while tests run in parallel, so every
    /// test that resolves without an explicit directory must hold this lock
    /// across the whole call — not just while setting the variables.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn env_guard() -> MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Set or clear an env var. `set_var`/`remove_var` are `unsafe` under
    /// edition 2024; safe here because callers hold `ENV_LOCK`.
    fn set_env(key: &str, value: Option<&str>) {
        unsafe {
            match value {
                Some(value) => env::set_var(key, value),
                None => env::remove_var(key),
            }
        }
    }

    #[test]
    fn config_dir_flag_wins_over_env() {
        let _env = env_guard();
        set_env("MIHOMO_TUI_CONFIG_DIR", Some("/also-ignored"));
        set_env("XDG_CONFIG_HOME", Some("/should-be-ignored"));
        set_env("HOME", Some("/still-ignored"));

        let paths = Paths::resolve(Some("/tmp/x")).unwrap();
        assert_eq!(paths.root, PathBuf::from("/tmp/x"));
    }

    #[test]
    fn env_var_overrides_the_default_locations() {
        let _env = env_guard();
        let tmp = TempDir::new().unwrap();
        set_env("MIHOMO_TUI_CONFIG_DIR", Some(tmp.path().to_str().unwrap()));
        set_env("XDG_CONFIG_HOME", Some("/should-be-ignored"));
        set_env("HOME", Some("/still-ignored"));

        let paths = Paths::resolve(None).unwrap();
        assert_eq!(paths.root, tmp.path());
    }

    #[test]
    fn default_location_is_xdg_or_home() {
        let _env = env_guard();
        let tmp = TempDir::new().unwrap();
        let xdg = tmp.path().join("t1");
        let home = tmp.path().join("t2");

        set_env("MIHOMO_TUI_CONFIG_DIR", None);
        set_env("XDG_CONFIG_HOME", Some(xdg.to_str().unwrap()));
        set_env("HOME", Some(home.to_str().unwrap()));
        let paths = Paths::resolve(None).unwrap();
        assert_eq!(paths.root, xdg.join("mihomo-tui"));

        set_env("XDG_CONFIG_HOME", None);
        let paths = Paths::resolve(None).unwrap();
        assert_eq!(paths.root, home.join(".config").join("mihomo-tui"));
    }

    #[test]
    fn ensure_creates_directories() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("any").join("depth");
        let paths = Paths::resolve(Some(root.to_str().unwrap())).unwrap();

        paths.ensure().unwrap();
        assert!(paths.root.is_dir());
        assert!(paths.subscriptions_dir().is_dir());
    }

    #[test]
    fn tui_config_defaults_and_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let paths = Paths::resolve(Some(tmp.path().to_str().unwrap())).unwrap();

        // Missing file: the documented defaults.
        let defaults = TuiConfig::load(&paths);
        assert_eq!(defaults.concurrency, 8);
        assert!(defaults.mouse);

        // Save then load: an exact roundtrip.
        let config = TuiConfig {
            concurrency: 16,
            mouse: false,
        };
        config.save(&paths).unwrap();
        let loaded = TuiConfig::load(&paths);
        assert_eq!(loaded.concurrency, 16);
        assert!(!loaded.mouse);

        // Corrupt file: back to defaults instead of an error.
        fs::write(paths.config_json(), "not json").unwrap();
        let recovered = TuiConfig::load(&paths);
        assert_eq!(recovered.concurrency, 8);
        assert!(recovered.mouse);

        // A file missing one field keeps the other and fills the default.
        fs::write(paths.config_json(), r#"{"concurrency":3}"#).unwrap();
        let partial = TuiConfig::load(&paths);
        assert_eq!(partial.concurrency, 3);
        assert!(partial.mouse);
    }

    #[test]
    fn known_files_live_inside_the_config_root() {
        // These names are the contract the other modules build on.
        let paths = Paths::resolve(Some("/cfg")).unwrap();
        assert_eq!(paths.base_yaml(), PathBuf::from("/cfg/base.yaml"));
        assert_eq!(paths.runtime_yaml(), PathBuf::from("/cfg/runtime.yaml"));
        assert_eq!(paths.profiles_json(), PathBuf::from("/cfg/profiles.json"));
        assert_eq!(
            paths.subscriptions_dir(),
            PathBuf::from("/cfg/subscriptions")
        );
        assert_eq!(paths.config_json(), PathBuf::from("/cfg/config.json"));
    }
}
