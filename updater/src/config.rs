//! Runtime configuration loading and XDG path discovery for the updater.

use anyhow::{Context, Result};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

const SERVICE_NAME: &str = "codex-update-manager";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
/// Runtime configuration values that control how the updater behaves on Linux.
pub struct RuntimeConfig {
    pub dmg_url: String,
    pub initial_check_delay_seconds: u64,
    pub check_interval_hours: u64,
    pub auto_install_on_app_exit: bool,
    pub notifications: bool,
    pub workspace_root: PathBuf,
    pub builder_bundle_root: PathBuf,
    pub app_executable_path: PathBuf,
}

#[derive(Debug, Clone)]
/// Resolved XDG filesystem locations used by the updater at runtime.
pub struct RuntimePaths {
    pub config_file: PathBuf,
    pub state_file: PathBuf,
    pub log_file: PathBuf,
    pub cache_dir: PathBuf,
    pub state_dir: PathBuf,
    pub config_dir: PathBuf,
}

impl RuntimePaths {
    /// Resolves updater paths from the current user's XDG base directories.
    pub fn from_base_dirs(base_dirs: &BaseDirs) -> Self {
        let config_dir = base_dirs.config_dir().join(SERVICE_NAME);
        let state_root = base_dirs
            .state_dir()
            .unwrap_or_else(|| base_dirs.data_local_dir());
        let state_dir = state_root.join(SERVICE_NAME);
        let cache_dir = base_dirs.cache_dir().join(SERVICE_NAME);

        Self {
            config_file: config_dir.join("config.toml"),
            state_file: state_dir.join("state.json"),
            log_file: state_dir.join("service.log"),
            cache_dir,
            state_dir,
            config_dir,
        }
    }

    /// Detects updater paths for the current machine.
    pub fn detect() -> Result<Self> {
        let base_dirs = BaseDirs::new().context("Could not resolve XDG base directories")?;
        Ok(Self::from_base_dirs(&base_dirs))
    }

    /// Creates the runtime directories needed by the updater.
    pub fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.config_dir)
            .with_context(|| format!("Failed to create {}", self.config_dir.display()))?;
        fs::create_dir_all(&self.state_dir)
            .with_context(|| format!("Failed to create {}", self.state_dir.display()))?;
        fs::create_dir_all(&self.cache_dir)
            .with_context(|| format!("Failed to create {}", self.cache_dir.display()))?;
        Ok(())
    }
}

impl RuntimeConfig {
    /// Builds the default runtime configuration for the resolved paths.
    pub fn default_with_paths(paths: &RuntimePaths) -> Self {
        let packaged_bundle_root = PathBuf::from("/opt/codex-desktop/update-builder");
        let builder_bundle_root = if packaged_bundle_root.exists() {
            packaged_bundle_root
        } else {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("updater crate should live inside the repository root")
                .to_path_buf()
        };

        Self {
            dmg_url: "https://persistent.oaistatic.com/codex-app-prod/Codex.dmg".to_string(),
            initial_check_delay_seconds: 30,
            check_interval_hours: 6,
            auto_install_on_app_exit: true,
            notifications: true,
            workspace_root: paths.cache_dir.clone(),
            builder_bundle_root,
            app_executable_path: PathBuf::from("/opt/codex-desktop/electron"),
        }
    }

    /// Loads the runtime configuration from disk, or returns defaults if missing.
    pub fn load_or_default(paths: &RuntimePaths) -> Result<Self> {
        if !paths.config_file.exists() {
            return Ok(Self::default_with_paths(paths));
        }

        let content = fs::read_to_string(&paths.config_file)
            .with_context(|| format!("Failed to read {}", paths.config_file.display()))?;
        let config = toml::from_str::<Self>(&content)
            .with_context(|| format!("Failed to parse {}", paths.config_file.display()))?;
        Ok(config)
    }
}

const APP_SETTINGS_FILE: &str = "settings.json";
const DEFAULT_APP_ID: &str = "codex-desktop";
const AUTO_INSTALL_SETTING_KEY: &str = "codex-linux-auto-update-on-exit";

/// Resolves the Codex Desktop app id the same way the Linux launcher and main
/// bundle do: `CODEX_LINUX_APP_ID`, then `CODEX_APP_ID`, then `codex-desktop`.
/// Invalid ids fall back to the default so a malformed env value can never point
/// the lookup at an attacker-controlled path.
fn resolve_app_id() -> String {
    fn valid(id: &str) -> bool {
        !id.is_empty()
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    }

    for var in ["CODEX_LINUX_APP_ID", "CODEX_APP_ID"] {
        if let Ok(value) = std::env::var(var) {
            if valid(&value) {
                return value;
            }
        }
    }
    DEFAULT_APP_ID.to_string()
}

/// Resolves the app `settings.json` path mirroring the launcher
/// (`launcher/start.sh.template`) and the main-bundle persistence helper
/// (`scripts/patches/launch-actions.js`): honor `CODEX_LINUX_SETTINGS_FILE`
/// first, then `XDG_CONFIG_HOME`, then `$HOME/.config`, joined with the app id.
fn app_settings_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("CODEX_LINUX_SETTINGS_FILE") {
        if !explicit.is_empty() {
            return Some(PathBuf::from(explicit));
        }
    }

    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;

    Some(config_home.join(resolve_app_id()).join(APP_SETTINGS_FILE))
}

/// Coerces a settings.json value into a boolean the same way the launcher's
/// `linux_setting_enabled` helper does: real booleans pass through, numbers are
/// truthy when non-zero, and strings are falsey only for `0/false/no/off`.
fn coerce_setting_bool(value: &serde_json::Value) -> Option<bool> {
    match value {
        serde_json::Value::Bool(flag) => Some(*flag),
        serde_json::Value::Number(number) => number.as_f64().map(|n| n != 0.0),
        serde_json::Value::String(text) => {
            let normalized = text.trim().to_ascii_lowercase();
            Some(!matches!(normalized.as_str(), "0" | "false" | "no" | "off"))
        }
        _ => None,
    }
}

/// Reads the user's auto-install-on-exit preference from the app
/// `settings.json`. Returns `Some(true|false)` only when the toggle key is
/// present and coercible; any missing file, parse error, or absent key yields
/// `None` so the caller falls back to the config/default value. Never panics.
pub fn settings_auto_install_override() -> Option<bool> {
    let path = app_settings_path()?;
    let content = fs::read_to_string(&path).ok()?;
    let parsed = serde_json::from_str::<serde_json::Value>(&content).ok()?;
    let object = parsed.as_object()?;
    coerce_setting_bool(object.get(AUTO_INSTALL_SETTING_KEY)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use std::sync::{Mutex, MutexGuard, OnceLock};
    use tempfile::tempdir;

    // Env vars are process-global, so settings-override tests must not run in
    // parallel with one another.
    fn settings_env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Writes `settings.json` content to a tempfile, points
    /// `CODEX_LINUX_SETTINGS_FILE` at it, and returns the override result.
    /// `None` content means "do not create the file" (missing-file case).
    fn override_with_settings(content: Option<&str>) -> Option<bool> {
        let _guard = settings_env_lock();
        let temp = tempdir().expect("tempdir");
        let settings_path = temp.path().join("settings.json");
        if let Some(body) = content {
            std::fs::write(&settings_path, body).expect("write settings");
        }
        std::env::set_var("CODEX_LINUX_SETTINGS_FILE", &settings_path);
        let result = settings_auto_install_override();
        std::env::remove_var("CODEX_LINUX_SETTINGS_FILE");
        result
    }

    #[test]
    fn settings_override_reads_explicit_bool() {
        assert_eq!(
            override_with_settings(Some(r#"{"codex-linux-auto-update-on-exit": false}"#)),
            Some(false)
        );
        assert_eq!(
            override_with_settings(Some(r#"{"codex-linux-auto-update-on-exit": true}"#)),
            Some(true)
        );
    }

    #[test]
    fn settings_override_coerces_string_and_number() {
        assert_eq!(
            override_with_settings(Some(r#"{"codex-linux-auto-update-on-exit": "off"}"#)),
            Some(false)
        );
        assert_eq!(
            override_with_settings(Some(r#"{"codex-linux-auto-update-on-exit": "on"}"#)),
            Some(true)
        );
        assert_eq!(
            override_with_settings(Some(r#"{"codex-linux-auto-update-on-exit": 0}"#)),
            Some(false)
        );
        assert_eq!(
            override_with_settings(Some(r#"{"codex-linux-auto-update-on-exit": 1}"#)),
            Some(true)
        );
    }

    #[test]
    fn settings_override_absent_yields_none() {
        // Missing file, malformed JSON, non-object, and absent key all fall back.
        assert_eq!(override_with_settings(None), None);
        assert_eq!(override_with_settings(Some("not json{")), None);
        assert_eq!(override_with_settings(Some("[1,2,3]")), None);
        assert_eq!(override_with_settings(Some(r#"{"other-key": true}"#)), None);
    }

    #[test]
    fn loads_default_when_config_is_missing() -> Result<()> {
        let temp = tempdir()?;
        let paths = RuntimePaths {
            config_file: temp.path().join("config/config.toml"),
            state_file: temp.path().join("state/state.json"),
            log_file: temp.path().join("state/service.log"),
            cache_dir: temp.path().join("cache"),
            state_dir: temp.path().join("state"),
            config_dir: temp.path().join("config"),
        };

        let config = RuntimeConfig::load_or_default(&paths)?;
        assert_eq!(config.initial_check_delay_seconds, 30);
        assert!(config.auto_install_on_app_exit);
        assert_eq!(config.workspace_root, paths.cache_dir);
        assert!(config.builder_bundle_root.is_absolute());
        Ok(())
    }

    #[test]
    fn parses_runtime_config_from_disk() -> Result<()> {
        let temp = tempdir()?;
        let paths = RuntimePaths {
            config_file: temp.path().join("config/config.toml"),
            state_file: temp.path().join("state/state.json"),
            log_file: temp.path().join("state/service.log"),
            cache_dir: temp.path().join("cache"),
            state_dir: temp.path().join("state"),
            config_dir: temp.path().join("config"),
        };
        fs::create_dir_all(&paths.config_dir)?;
        fs::write(
            &paths.config_file,
            r#"
dmg_url = "https://example.com/Codex.dmg"
initial_check_delay_seconds = 5
check_interval_hours = 12
auto_install_on_app_exit = false
notifications = false
workspace_root = "/tmp/codex-workspaces"
builder_bundle_root = "/tmp/codex-builder"
app_executable_path = "/opt/codex-desktop/electron"
"#,
        )?;

        let config = RuntimeConfig::load_or_default(&paths)?;
        assert_eq!(config.dmg_url, "https://example.com/Codex.dmg");
        assert_eq!(config.initial_check_delay_seconds, 5);
        assert_eq!(config.check_interval_hours, 12);
        assert!(!config.auto_install_on_app_exit);
        assert!(!config.notifications);
        assert_eq!(
            config.workspace_root,
            PathBuf::from("/tmp/codex-workspaces")
        );
        assert_eq!(
            config.builder_bundle_root,
            PathBuf::from("/tmp/codex-builder")
        );
        assert_eq!(
            config.app_executable_path,
            PathBuf::from("/opt/codex-desktop/electron")
        );
        Ok(())
    }
}
