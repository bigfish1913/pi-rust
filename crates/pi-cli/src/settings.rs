//! `~/.rpi/agent/settings.json` — saved user defaults. Mirrors the slice of
//! pi's `Settings` interface (`packages/coding-agent/src/core/settings-manager.ts`)
//! that rpi honors: `defaultProvider` / `defaultModel` / `defaultThinkingLevel`
//! (consumed by `provider::resolve` as pi's `findInitialModel` step 3 — the
//! saved default, when authed, wins over the built-in fallback), `theme`,
//! packages, and configurable resource directories.
//!
//! pi's `Settings` carries ~40 fields; rpi reads the fields it uses and drops the
//! rest (serde `default` ignores unknown fields), so a copied pi `settings.json`
//! parses clean.

use std::path::{Path, PathBuf};

use crate::config::{self, strip_line_comments, ConfigError};

/// The honored subset of pi's `Settings`. Unknown fields are ignored.
#[derive(serde::Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// Saved default provider id (v1 honors only `"anthropic"`; an absent or
    /// anthropic value allows the saved default-model lookup).
    #[serde(default)]
    pub default_provider: Option<String>,
    /// Saved default model id. When present and the model is authed,
    /// `provider::resolve` selects it (pi `findInitialModel` step 3).
    #[serde(default)]
    pub default_model: Option<String>,
    /// Saved default thinking level (a level-name string: off/minimal/low/medium/
    /// high/xhigh/max). Parsed by the caller via `args::parse_thinking_level`.
    #[serde(default)]
    pub default_thinking_level: Option<String>,
    /// Saved theme name. Surfaced for best-effort TUI theme application.
    #[serde(default)]
    pub theme: Option<String>,
    /// `/scoped-models`: the model ids allowed in the Ctrl+M cycle. Absent /
    /// empty ⇒ every catalog model cycles (the default).
    #[serde(default)]
    pub scoped_models: Option<Vec<String>>,
    /// Pi-compatible static package specs. Entries may be local package
    /// directories, `package.json` files, or installed package names.
    #[serde(default)]
    pub packages: Option<Vec<String>>,
    /// Additional skill directories. Relative paths are resolved against the
    /// settings file's owner (project root for project settings, agent dir for
    /// global settings). `skills` is accepted as a compatibility shorthand.
    #[serde(default, alias = "skills")]
    pub skill_dirs: Option<Vec<String>>,
    /// Additional prompt-template directories/files. `prompts` is accepted as
    /// a compatibility shorthand.
    #[serde(default, alias = "prompts")]
    pub prompt_dirs: Option<Vec<String>>,
    /// Additional Rust cdylib extension directories. `extensions` is accepted
    /// as a compatibility shorthand.
    #[serde(default, alias = "extensions")]
    pub extension_dirs: Option<Vec<String>>,
}

/// Load `~/.rpi/agent/settings.json`. Missing file ⇒ `Settings::default()`
/// (not an error). Malformed JSON ⇒ `ConfigError::Json`. Tolerates `//` line
/// comments (a copied pi settings.json may contain them).
pub fn load_settings() -> Result<Settings, ConfigError> {
    let path = config::settings_path()?;
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_settings(&text).map_err(|e| ConfigError::Json {
            path: path.clone(),
            source: e,
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // A native Pi installation keeps its settings under ~/.pi/agent.
            // Read that file only as a fallback; all writes still target the
            // rpi-owned ~/.rpi/agent/settings.json path.
            let legacy = if std::env::var_os(config::CONFIG_DIR_ENV).is_some() {
                None
            } else {
                dirs::home_dir().map(|home| home.join(".pi/agent/settings.json"))
            };
            match legacy.filter(|candidate| candidate != &path) {
                Some(legacy_path) => match std::fs::read_to_string(&legacy_path) {
                    Ok(text) => parse_settings(&text).map_err(|e| ConfigError::Json {
                        path: legacy_path,
                        source: e,
                    }),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        Ok(Settings::default())
                    }
                    Err(error) => Err(ConfigError::Read {
                        path: legacy_path,
                        source: error,
                    }),
                },
                None => Ok(Settings::default()),
            }
        }
        Err(e) => Err(ConfigError::Read { path, source: e }),
    }
}

/// Load project-local settings in precedence order: `.rpi/settings.json`,
/// then legacy `.pi/settings.json`. Both files may contribute additional
/// resource paths; the preferred `.rpi` file is returned first so its paths
/// register before legacy Pi paths. Malformed or unreadable optional project
/// settings are ignored, matching the best-effort behavior of resource-dir
/// discovery; global settings remain available through [`load_settings`].
pub fn load_project_settings(cwd: &Path) -> Vec<Settings> {
    load_project_settings_with_paths(cwd)
        .into_iter()
        .map(|(_, settings)| settings)
        .collect()
}

/// Load project settings together with their source paths. The path is kept
/// so callers can preserve `.rpi` before `.pi` ordering even when only one of
/// the two files exists.
pub fn load_project_settings_with_paths(cwd: &Path) -> Vec<(PathBuf, Settings)> {
    [
        cwd.join(".rpi").join("settings.json"),
        cwd.join(".pi").join("settings.json"),
    ]
    .into_iter()
    .filter_map(|path| {
        load_settings_file(&path)
            .ok()
            .map(|settings| (path, settings))
    })
    .collect()
}

fn load_settings_file(path: &Path) -> Result<Settings, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    parse_settings(&text).map_err(|source| ConfigError::Json {
        path: path.to_path_buf(),
        source,
    })
}

/// Resolve a configured path relative to its settings owner. Absolute paths
/// are preserved; empty entries are ignored.
pub fn resolve_configured_paths(base: &Path, values: &[String]) -> Vec<PathBuf> {
    values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(|value| {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                path
            } else {
                base.join(path)
            }
        })
        .collect()
}

fn parse_settings(text: &str) -> Result<Settings, serde_json::Error> {
    match serde_json::from_str(text) {
        Ok(s) => Ok(s),
        Err(first) => {
            let stripped = strip_line_comments(text);
            serde_json::from_str(&stripped).map_err(|_| first)
        }
    }
}

/// Persist the honored settings back to `~/.rpi/agent/settings.json`.
/// Unknown pi fields (which `Settings` doesn't model) are **preserved**: the
/// current file is read as raw JSON, the known fields are overlaid, and the
/// merged object is written — so a copied pi `settings.json` survives edits
/// without losing pi-only keys. Missing file ⇒ a fresh object. Best-effort
/// errors are returned as strings for the caller to surface.
pub fn save_settings(settings: &Settings) -> Result<(), String> {
    let path = config::settings_path().map_err(|e| e.to_string())?;
    // Read the existing file as a raw object to preserve unknown fields.
    let mut merged = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
            .unwrap_or(serde_json::Value::Object(Default::default())),
        Err(_) => serde_json::Value::Object(Default::default()),
    };
    let obj = merged
        .as_object_mut()
        .ok_or("settings file is not an object")?;
    for (key, val) in [
        ("defaultProvider", settings.default_provider.as_ref()),
        ("defaultModel", settings.default_model.as_ref()),
        (
            "defaultThinkingLevel",
            settings.default_thinking_level.as_ref(),
        ),
        ("theme", settings.theme.as_ref()),
    ] {
        match val {
            Some(v) => {
                obj.insert(key.to_string(), serde_json::Value::String(v.clone()));
            }
            None => {
                obj.remove(key);
            }
        }
    }
    match &settings.scoped_models {
        Some(list) if !list.is_empty() => {
            obj.insert(
                "scopedModels".to_string(),
                serde_json::Value::Array(
                    list.iter()
                        .map(|m| serde_json::Value::String(m.clone()))
                        .collect(),
                ),
            );
        }
        _ => {
            obj.remove("scopedModels");
        }
    }
    match &settings.packages {
        Some(list) if !list.is_empty() => {
            obj.insert(
                "packages".to_string(),
                serde_json::Value::Array(
                    list.iter()
                        .map(|p| serde_json::Value::String(p.clone()))
                        .collect(),
                ),
            );
        }
        _ => {
            obj.remove("packages");
        }
    }
    for (key, values) in [
        ("skillDirs", settings.skill_dirs.as_ref()),
        ("promptDirs", settings.prompt_dirs.as_ref()),
        ("extensionDirs", settings.extension_dirs.as_ref()),
    ] {
        match values {
            Some(list) if !list.is_empty() => {
                obj.insert(
                    key.to_string(),
                    serde_json::Value::Array(
                        list.iter()
                            .map(|path| serde_json::Value::String(path.clone()))
                            .collect(),
                    ),
                );
            }
            _ => {
                obj.remove(key);
            }
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(&merged).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::env_lock;

    /// Point `RPI_CODING_AGENT_DIR` at a fresh temp dir for this test.
    struct TempConfig {
        _guard: std::sync::MutexGuard<'static, ()>,
        _tmp: tempfile::TempDir,
        prev: Option<std::ffi::OsString>,
    }
    impl TempConfig {
        fn new() -> Self {
            let guard = env_lock().lock().unwrap();
            let prev = std::env::var_os(config::CONFIG_DIR_ENV);
            let tmp = tempfile::TempDir::new().unwrap();
            std::env::set_var(config::CONFIG_DIR_ENV, tmp.path());
            Self {
                _guard: guard,
                _tmp: tmp,
                prev,
            }
        }
    }
    impl Drop for TempConfig {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var(config::CONFIG_DIR_ENV, v),
                None => std::env::remove_var(config::CONFIG_DIR_ENV),
            }
        }
    }

    #[test]
    fn missing_settings_is_default() {
        let _cfg = TempConfig::new();
        let s = load_settings().unwrap();
        assert!(s.default_provider.is_none());
        assert!(s.default_model.is_none());
        assert!(s.default_thinking_level.is_none());
        assert!(s.theme.is_none());
        assert!(s.skill_dirs.is_none());
        assert!(s.prompt_dirs.is_none());
        assert!(s.extension_dirs.is_none());
    }

    #[test]
    fn reads_honored_fields_and_ignores_unknown() {
        let _cfg = TempConfig::new();
        let path = config::settings_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A pi-style settings.json with many unknown fields + the 4 we honor.
        std::fs::write(
            &path,
            r#"{
                "lastChangelogVersion": "1.0.0",
                "defaultProvider": "anthropic",
                "defaultModel": "claude-sonnet-5",
                "defaultThinkingLevel": "high",
                "theme": "dark",
                "hideThinkingBlock": true,
                "compaction": { "threshold": 100 },
                "packages": ["some-pkg"]
            }"#,
        )
        .unwrap();
        let s = load_settings().unwrap();
        assert_eq!(s.default_provider.as_deref(), Some("anthropic"));
        assert_eq!(s.default_model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(s.default_thinking_level.as_deref(), Some("high"));
        assert_eq!(s.theme.as_deref(), Some("dark"));
        assert_eq!(s.packages, Some(vec!["some-pkg".to_string()]));
    }

    #[test]
    fn loads_project_settings_in_rpi_then_pi_order() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".rpi")).unwrap();
        std::fs::create_dir_all(tmp.path().join(".pi")).unwrap();
        std::fs::write(
            tmp.path().join(".rpi/settings.json"),
            r#"{"skillDirs":["rpi-skills"],"extensions":["rpi-ext"]}"#,
        )
        .unwrap();
        std::fs::write(
            tmp.path().join(".pi/settings.json"),
            r#"{"skills":["pi-skills"],"extensionDirs":["pi-ext"]}"#,
        )
        .unwrap();

        let settings = load_project_settings(tmp.path());
        assert_eq!(settings.len(), 2);
        assert_eq!(
            settings[0].skill_dirs.as_deref(),
            Some(["rpi-skills".to_string()].as_slice())
        );
        assert_eq!(
            settings[0].extension_dirs.as_deref(),
            Some(["rpi-ext".to_string()].as_slice())
        );
        assert_eq!(
            settings[1].skill_dirs.as_deref(),
            Some(["pi-skills".to_string()].as_slice())
        );
        assert_eq!(
            settings[1].extension_dirs.as_deref(),
            Some(["pi-ext".to_string()].as_slice())
        );
    }

    #[test]
    fn tolerates_line_comments() {
        let _cfg = TempConfig::new();
        let path = config::settings_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "{\n  // my default\n  \"defaultModel\": \"glm-5\",\n  \"theme\": \"light\"\n}\n",
        )
        .unwrap();
        let s = load_settings().unwrap();
        assert_eq!(s.default_model.as_deref(), Some("glm-5"));
        assert_eq!(s.theme.as_deref(), Some("light"));
    }

    #[test]
    fn malformed_is_error() {
        let _cfg = TempConfig::new();
        let path = config::settings_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        assert!(matches!(load_settings(), Err(ConfigError::Json { .. })));
    }
}

#[cfg(test)]
mod scoped_tests {
    use super::*;
    use crate::config::test_support::env_lock;

    fn with_temp_env() -> (tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
        let guard = env_lock().lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var(config::CONFIG_DIR_ENV, tmp.path());
        (tmp, guard)
    }

    #[test]
    fn save_load_scoped_models_roundtrip() {
        let (_tmp, _guard) = with_temp_env();
        let mut s = Settings::default();
        s.scoped_models = Some(vec!["a".into(), "b".into()]);
        save_settings(&s).unwrap();
        let loaded = load_settings().unwrap();
        assert_eq!(
            loaded.scoped_models,
            Some(vec!["a".to_string(), "b".to_string()])
        );
        // Clearing removes the key.
        let mut s2 = load_settings().unwrap();
        s2.scoped_models = None;
        save_settings(&s2).unwrap();
        assert_eq!(load_settings().unwrap().scoped_models, None);
    }

    #[test]
    fn save_preserves_unknown_fields() {
        let (_tmp, _guard) = with_temp_env();
        let path = config::settings_path().unwrap();
        std::fs::write(&path, r#"{"piOnlyField":"keep-me","theme":"dark"}"#).unwrap();
        let mut s = load_settings().unwrap();
        s.scoped_models = Some(vec!["m1".into()]);
        save_settings(&s).unwrap();
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["piOnlyField"], "keep-me");
        assert_eq!(raw["scopedModels"][0], "m1");
        assert_eq!(raw["theme"], "dark");
    }
}
