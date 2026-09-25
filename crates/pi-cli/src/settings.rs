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

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::{self, strip_line_comments, ConfigError};

/// `retry` — native pi's `RetrySettings`. `provider` is parsed but unused: it
/// configures the provider SDK's own retry loop, which rpi does not run.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct RetrySettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_delay_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_agent_delay_ms: Option<u64>,
    /// Native pi's nested `retry.provider`. Kept so a copied file round-trips;
    /// rpi has no provider-SDK retry loop to configure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<serde_json::Value>,
}

/// `compaction` — native pi's `CompactionSettings`.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSettingsJson {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_recent_tokens: Option<i64>,
    /// Per-model overrides. Kept for round-tripping; the harness has no
    /// per-model compaction override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_overrides: Option<serde_json::Value>,
}

/// `branchSummary` — native pi's `BranchSummarySettings`.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummarySettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<i64>,
    /// When true, native pi skips its "Summarize branch?" prompt and defaults to
    /// *no* summary. rpi has no such prompt, so this is what makes the key mean
    /// something: `Some(true)` suppresses the summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_prompt: Option<bool>,
}

/// `steeringMode` / `followUpMode` — native pi's queue drain modes.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum QueueModeSetting {
    All,
    OneAtATime,
}

impl From<QueueModeSetting> for rpi_agent::QueueMode {
    fn from(mode: QueueModeSetting) -> Self {
        match mode {
            QueueModeSetting::All => rpi_agent::QueueMode::All,
            QueueModeSetting::OneAtATime => rpi_agent::QueueMode::OneAtATime,
        }
    }
}

/// A package entry from Pi's `packages` setting.
///
/// The string form loads every resource exposed by the package. The object
/// form can restrict individual resource kinds through [`PackageFilter`].
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(untagged)]
pub enum PackageSetting {
    Source(String),
    Filtered(PackageFilter),
}

impl PackageSetting {
    /// Return the npm, git, or local source spec regardless of entry form.
    pub fn source(&self) -> &str {
        match self {
            Self::Source(source) => source,
            Self::Filtered(filter) => &filter.source,
        }
    }
}

impl From<String> for PackageSetting {
    fn from(source: String) -> Self {
        Self::Source(source)
    }
}

impl From<&str> for PackageSetting {
    fn from(source: &str) -> Self {
        Self::Source(source.to_string())
    }
}

/// Resource filters for Pi's object-form package setting.
///
/// Additional properties are retained so loading and saving settings with a
/// newer Pi package schema never discards fields rpi does not yet understand.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PackageFilter {
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autoload: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub themes: Option<Vec<String>>,
    #[serde(flatten)]
    pub unknown: serde_json::Map<String, serde_json::Value>,
}

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
    /// empty ⇒ every catalog model cycles (the default). Native pi spells this
    /// `enabledModels`, which is accepted as an alias so a copied `settings.json`
    /// behaves the same.
    #[serde(default, alias = "enabledModels")]
    pub scoped_models: Option<Vec<String>>,

    /// Custom session storage directory. Native pi's `sessionDir`, same format as
    /// the `--session-dir` flag (which wins over it).
    #[serde(default)]
    pub session_dir: Option<PathBuf>,

    /// Custom shell binary. Native pi's `shellPath`. Parsed for fidelity, but
    /// **not consumed yet**: nothing in pi-tools carries a shell binary, so
    /// honouring it means plumbing one through `BashToolOptions` /
    /// `ShellCaptureOptions` first.
    #[serde(default)]
    pub shell_path: Option<String>,

    /// Prefix prepended to every bash command (native pi's `shellCommandPrefix`,
    /// e.g. `shopt -s expand_aliases`).
    #[serde(default)]
    pub shell_command_prefix: Option<String>,

    /// Proxy URL applied as `HTTP_PROXY` / `HTTPS_PROXY` for rpi's own HTTP
    /// clients. Native pi's `httpProxy`.
    #[serde(default)]
    pub http_proxy: Option<String>,

    /// Retry policy for assistant turns. Mirrors native pi `retry`; the fields
    /// map 1:1 onto the harness's `RetryPolicy`.
    #[serde(default)]
    pub retry: Option<RetrySettings>,

    /// Compaction settings. Mirrors native pi `compaction` (minus its
    /// `modelOverrides`, which the harness has no equivalent for).
    #[serde(default)]
    pub compaction: Option<CompactionSettingsJson>,

    /// Branch-summary settings. Mirrors native pi `branchSummary`.
    ///
    /// Parsed for shape fidelity, but **nothing consumes it yet**: rpi's
    /// `navigate_tree` ignores its `summarize` argument outright (`_summarize`),
    /// and `generate_branch_summary` has no production caller. Until
    /// navigation-with-summary is wired, these keys cannot change behaviour — see
    /// the doc comment on the accessors.
    #[serde(default)]
    pub branch_summary: Option<BranchSummarySettings>,

    /// How queued steering messages are drained. Native pi default:
    /// `"one-at-a-time"` (matches the harness's `QueueMode` default).
    #[serde(default)]
    pub steering_mode: Option<QueueModeSetting>,

    /// How queued follow-up messages are drained. Native pi default:
    /// `"one-at-a-time"`.
    #[serde(default)]
    pub follow_up_mode: Option<QueueModeSetting>,
    /// Pi-compatible static package specs. Entries may be local package
    /// directories, `package.json` files, installed package names, or filtered
    /// package objects.
    #[serde(default)]
    pub packages: Option<Vec<PackageSetting>>,
    /// Command used by native Pi for npm package lookup/install operations.
    /// Stored argv-style so launchers such as `mise exec -- npm` need no shell.
    #[serde(default)]
    pub npm_command: Option<Vec<String>>,
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
    /// Native Pi keybinding overrides. Values may be a key string, an array
    /// of key strings, or an empty array to unbind an action.
    #[serde(default)]
    pub keybindings: Option<HashMap<String, serde_json::Value>>,
    /// Action performed by two quick Escape presses while the editor is empty.
    /// Native Pi defaults this to `tree`; `none` disables the gesture.
    #[serde(default)]
    pub double_escape_action: Option<String>,
    /// Default tools to enable at startup. When absent, all built-in tools
    /// are enabled. When present, only the named tools are enabled.
    #[serde(default)]
    pub default_tools: Option<Vec<String>>,
    /// Hide the body of thinking blocks while retaining a compact label.
    #[serde(default)]
    pub hide_thinking_block: Option<bool>,
    /// Suppress the interactive startup header and resource summary.
    /// Update checks remain enabled, matching native Pi.
    #[serde(default)]
    pub quiet_startup: Option<bool>,
    /// Show the global terminal progress indicator while a run is active.
    #[serde(default)]
    pub show_terminal_progress: Option<bool>,
    /// Horizontal editor padding in terminal columns.
    #[serde(default)]
    pub editor_padding_x: Option<usize>,
    /// Maximum number of autocomplete rows shown above the editor.
    #[serde(default)]
    pub autocomplete_max_visible: Option<usize>,
    /// Automatically copy text to clipboard when selected in fullscreen mode.
    /// When false, text selection requires explicit copy action.
    #[serde(default)]
    pub fullscreen_copy_on_select: Option<bool>,
    /// Horizontal padding in terminal columns added to each side of the chat
    /// transcript (user/assistant messages and thinking). Native pi `outputPad`
    /// (its only legal values are 0 and 1).
    #[serde(default)]
    pub output_pad: Option<usize>,
    /// Show transcript notices for prompt-cache costs and provider recovery
    /// diagnostics. Native pi `showCacheMissNotices` (default `false`).
    #[serde(default)]
    pub show_cache_miss_notices: Option<bool>,
    /// Render images inline in the terminal. rpi stores this at the top level
    /// under `showImages`; native pi nests it as `terminal.showImages`, which
    /// [`TerminalSettings`] also accepts (see [`Settings::show_images`]).
    #[serde(default)]
    pub show_images: Option<bool>,
    /// Native pi's nested `terminal` settings block. rpi keeps its own flat
    /// keys for the settings it has always written, but reads this block so a
    /// native `settings.json` is honored.
    #[serde(default)]
    pub terminal: Option<TerminalSettings>,
    /// How long idle pooled HTTP connections are kept before being closed.
    /// Accepts a millisecond number or the string `"disabled"`. Native pi
    /// `httpIdleTimeout`. rpi surface: `httpIdleTimeout`.
    #[serde(default, alias = "httpIdleTimeoutMs")]
    pub http_idle_timeout: Option<serde_json::Value>,
}

/// Native pi's `terminal` settings block (`TerminalSettings`). Only the fields
/// rpi can honor are modeled; unknown keys are ignored.
#[derive(serde::Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSettings {
    /// `terminal.showImages` — render images inline.
    #[serde(default)]
    pub show_images: Option<bool>,
    /// `terminal.showTerminalProgress` — OSC 9;4 terminal progress indicator.
    #[serde(default)]
    pub show_terminal_progress: Option<bool>,
    /// `terminal.clearOnShrink` — clear empty rows when content shrinks.
    #[serde(default)]
    pub clear_on_shrink: Option<bool>,
}

impl Settings {
    /// Resolved HTTP idle timeout in milliseconds (`None` ⇒ library default,
    /// `Some(0)` ⇒ disabled). See [`rpi_ai::http::parse_http_idle_timeout_ms`].
    pub fn http_idle_timeout_ms(&self) -> Option<i64> {
        self.http_idle_timeout
            .as_ref()
            .and_then(rpi_ai::http::parse_http_idle_timeout_ms)
    }

    /// Retry policy, resolved onto the harness type. Absent fields keep the
    /// harness defaults, so a partial `retry` object changes only what it names.
    pub fn retry_policy(&self) -> rpi_harness::types::RetryPolicy {
        let mut policy = rpi_harness::types::RetryPolicy::default();
        let Some(retry) = &self.retry else {
            return policy;
        };
        if let Some(enabled) = retry.enabled {
            policy.enabled = enabled;
        }
        if let Some(max_retries) = retry.max_retries {
            policy.max_retries = max_retries;
        }
        if let Some(base_delay_ms) = retry.base_delay_ms {
            policy.base_delay_ms = base_delay_ms;
        }
        if let Some(max_agent_delay_ms) = retry.max_agent_delay_ms {
            policy.max_agent_delay_ms = max_agent_delay_ms;
        }
        policy
    }

    /// Compaction settings, resolved onto the harness type.
    pub fn compaction_settings(&self) -> rpi_harness::types::CompactionSettings {
        let mut settings = rpi_harness::types::CompactionSettings::default();
        let Some(compaction) = &self.compaction else {
            return settings;
        };
        if let Some(enabled) = compaction.enabled {
            settings.enabled = enabled;
        }
        if let Some(reserve_tokens) = compaction.reserve_tokens {
            settings.reserve_tokens = reserve_tokens;
        }
        if let Some(keep_recent_tokens) = compaction.keep_recent_tokens {
            settings.keep_recent_tokens = keep_recent_tokens;
        }
        settings
    }

    /// Tokens reserved for a branch summary's prompt + output.
    ///
    /// Not consumed yet: `AgentLane::navigate_tree` ignores `summarize`, so no
    /// branch summary is ever generated from this path. Wiring it means
    /// implementing navigation-with-summary in the harness, not reading a key.
    pub fn branch_summary_reserve_tokens(&self) -> Option<i64> {
        self.branch_summary.as_ref().and_then(|s| s.reserve_tokens)
    }

    /// Whether branch summaries are suppressed (native pi's `skipPrompt`).
    ///
    /// Not consumed yet, for the same reason as
    /// [`Self::branch_summary_reserve_tokens`].
    pub fn branch_summary_skipped(&self) -> bool {
        self.branch_summary
            .as_ref()
            .and_then(|s| s.skip_prompt)
            .unwrap_or(false)
    }

    /// Default steering-queue drain mode (native pi default: one-at-a-time).
    pub fn steering_mode(&self) -> rpi_agent::QueueMode {
        self.steering_mode
            .map(Into::into)
            .unwrap_or(rpi_agent::QueueMode::OneAtATime)
    }

    /// Default follow-up-queue drain mode (native pi default: one-at-a-time).
    pub fn follow_up_mode(&self) -> rpi_agent::QueueMode {
        self.follow_up_mode
            .map(Into::into)
            .unwrap_or(rpi_agent::QueueMode::OneAtATime)
    }

    /// Render images inline. rpi's flat `showImages` wins; native pi's nested
    /// `terminal.showImages` is the fallback.
    pub fn show_images(&self) -> Option<bool> {
        self.show_images
            .or_else(|| self.terminal.as_ref().and_then(|t| t.show_images))
    }

    /// Terminal progress indicator. rpi's flat `showTerminalProgress` wins;
    /// native pi's nested `terminal.showTerminalProgress` is the fallback.
    pub fn show_terminal_progress(&self) -> Option<bool> {
        self.show_terminal_progress.or_else(|| {
            self.terminal
                .as_ref()
                .and_then(|t| t.show_terminal_progress)
        })
    }
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

/// Load the single active project settings document. `.rpi/settings.json`
/// takes precedence; native Pi's `.pi/settings.json` is consulted only when
/// the rpi-owned file does not exist. A malformed preferred file masks the
/// fallback and yields no project settings, keeping project configuration
/// fail-closed instead of executing entries from a stale lower-priority file.
pub fn load_project_settings(cwd: &Path) -> Vec<Settings> {
    load_project_settings_with_paths(cwd)
        .into_iter()
        .map(|(_, settings)| settings)
        .collect()
}

/// Load the active project settings together with its source path.
pub fn load_project_settings_with_paths(cwd: &Path) -> Vec<(PathBuf, Settings)> {
    load_active_project_settings(cwd)
        .ok()
        .flatten()
        .into_iter()
        .collect()
}

/// Load the active project settings while preserving parse/read failures for
/// callers that must fail closed, such as package install/update preflight.
pub fn load_active_project_settings(
    cwd: &Path,
) -> Result<Option<(PathBuf, Settings)>, ConfigError> {
    let preferred = cwd.join(".rpi/settings.json");
    match load_settings_file(&preferred) {
        Ok(settings) => Ok(Some((preferred, settings))),
        Err(ConfigError::Read { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            let fallback = cwd.join(".pi/settings.json");
            match load_settings_file(&fallback) {
                Ok(settings) => Ok(Some((fallback, settings))),
                Err(ConfigError::Read { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    Ok(None)
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

/// Load model-selection settings with native Pi's global -> trusted-project
/// precedence. Only fields consumed during provider resolution are overlaid;
/// package and resource loading keeps its own scope-aware merge semantics.
pub fn load_effective_model_settings(
    cwd: &Path,
    project_trusted: bool,
) -> Result<Settings, ConfigError> {
    let mut effective = load_settings()?;
    if project_trusted {
        if let Some((_, project)) = load_active_project_settings(cwd)? {
            if project.default_provider.is_some() {
                effective.default_provider = project.default_provider;
            }
            if project.default_model.is_some() {
                effective.default_model = project.default_model;
            }
            if project.default_thinking_level.is_some() {
                effective.default_thinking_level = project.default_thinking_level;
            }
            if project.theme.is_some() {
                effective.theme = project.theme;
            }
        }
    }
    Ok(effective)
}

/// Load the project settings document that rpi may safely update. The
/// preferred `.rpi` file wins; when it does not exist, native Pi's `.pi` file
/// seeds the first rpi-owned save so package changes do not discard fields rpi
/// does not model.
pub fn load_project_settings_for_write(cwd: &Path) -> Result<Settings, ConfigError> {
    Ok(load_active_project_settings(cwd)?
        .map(|(_, settings)| settings)
        .unwrap_or_default())
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

/// Parse an existing settings document while retaining fields that rpi does
/// not model. Native Pi permits `//` line comments in `settings.json`, so use
/// the same strict-then-comment-stripped strategy as [`parse_settings`].
/// Keeping this separate from `parse_settings` is important: deserializing
/// into [`Settings`] would discard unknown fields before a save/merge.
fn parse_settings_value(text: &str) -> Result<serde_json::Value, serde_json::Error> {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(value) => Ok(value),
        Err(first) => {
            let stripped = strip_line_comments(text);
            serde_json::from_str::<serde_json::Value>(&stripped).map_err(|_| first)
        }
    }
}

/// Persist the honored settings back to `~/.rpi/agent/settings.json`.
/// Unknown pi fields (which `Settings` doesn't model) are **preserved**: the
/// current file is read as raw JSON, the known fields are overlaid, and the
/// merged object is written — so a copied pi `settings.json` survives edits
/// without losing pi-only keys. `//` comments are accepted using the same
/// parser as [`load_settings`]. Missing file ⇒ a fresh object. Existing files
/// that cannot be parsed safely are left untouched and reported as errors.
pub fn save_settings(settings: &Settings) -> Result<(), String> {
    let path = config::settings_path().map_err(|e| e.to_string())?;
    let fallback = if std::env::var_os(config::CONFIG_DIR_ENV).is_some() {
        None
    } else {
        dirs::home_dir().map(|home| home.join(".pi/agent/settings.json"))
    };
    save_settings_to_path(settings, &path, fallback.as_deref())
}

/// Persist project-scoped settings under `.rpi/settings.json`, preserving an
/// existing native `.pi/settings.json` as the raw fallback on the first save.
pub fn save_project_settings(cwd: &Path, settings: &Settings) -> Result<(), String> {
    save_settings_to_path(
        settings,
        &cwd.join(".rpi/settings.json"),
        Some(&cwd.join(".pi/settings.json")),
    )
}

fn save_settings_to_path(
    settings: &Settings,
    path: &Path,
    fallback: Option<&Path>,
) -> Result<(), String> {
    // Read the existing file as a raw object to preserve unknown fields.
    let mut merged = match std::fs::read_to_string(path) {
        Ok(text) => parse_settings_value(&text).map_err(|error| {
            format!(
                "cannot parse existing settings file {}: {error}",
                path.display()
            )
        })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            read_fallback_settings_value(path, fallback)?
                .unwrap_or_else(|| serde_json::Value::Object(Default::default()))
        }
        Err(error) => {
            return Err(format!(
                "cannot read existing settings file {}: {error}",
                path.display()
            ));
        }
    };
    let obj = merged
        .as_object_mut()
        .ok_or_else(|| format!("settings file {} is not an object", path.display()))?;
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
                serde_json::to_value(list).map_err(|e| e.to_string())?,
            );
        }
        _ => {
            obj.remove("packages");
        }
    }
    match &settings.npm_command {
        Some(command) => {
            obj.insert(
                "npmCommand".to_string(),
                serde_json::to_value(command).map_err(|e| e.to_string())?,
            );
        }
        None => {
            obj.remove("npmCommand");
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
    match &settings.keybindings {
        Some(bindings) => {
            obj.insert(
                "keybindings".to_string(),
                serde_json::to_value(bindings).map_err(|e| e.to_string())?,
            );
        }
        None => {
            obj.remove("keybindings");
        }
    }
    match settings.double_escape_action.as_deref() {
        Some(action) if !action.trim().is_empty() => {
            obj.insert(
                "doubleEscapeAction".to_string(),
                serde_json::Value::String(action.to_string()),
            );
        }
        _ => {
            obj.remove("doubleEscapeAction");
        }
    }
    for (key, value) in [
        (
            "hideThinkingBlock",
            settings.hide_thinking_block.map(serde_json::Value::Bool),
        ),
        (
            "quietStartup",
            settings.quiet_startup.map(serde_json::Value::Bool),
        ),
        (
            "showTerminalProgress",
            settings.show_terminal_progress.map(serde_json::Value::Bool),
        ),
        (
            "showImages",
            settings.show_images.map(serde_json::Value::Bool),
        ),
        (
            "showCacheMissNotices",
            settings
                .show_cache_miss_notices
                .map(serde_json::Value::Bool),
        ),
        (
            "fullscreenCopyOnSelect",
            settings
                .fullscreen_copy_on_select
                .map(serde_json::Value::Bool),
        ),
        (
            "editorPaddingX",
            settings
                .editor_padding_x
                .map(|v| serde_json::Value::Number(v.into())),
        ),
        (
            "outputPad",
            settings
                .output_pad
                .map(|v| serde_json::Value::Number(v.into())),
        ),
        (
            "autocompleteMaxVisible",
            settings
                .autocomplete_max_visible
                .map(|v| serde_json::Value::Number(v.into())),
        ),
        ("httpIdleTimeout", settings.http_idle_timeout.clone()),
    ] {
        match value {
            Some(value) => {
                obj.insert(key.to_string(), value);
            }
            None => {
                obj.remove(key);
            }
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(&merged).map_err(|e| e.to_string())?;
    config::atomic_write(path, text.as_bytes()).map_err(|e| e.to_string())
}

/// When rpi is still reading native Pi's settings fallback, seed the first
/// rpi-owned save from that complete raw object. Otherwise a modeled-field
/// save could shadow the native file and silently drop fields added by Pi.
fn read_fallback_settings_value(
    target: &Path,
    fallback: Option<&Path>,
) -> Result<Option<serde_json::Value>, String> {
    let Some(path) = fallback else {
        return Ok(None);
    };
    if path == target {
        return Ok(None);
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_settings_value(&text).map(Some).map_err(|error| {
            format!(
                "cannot parse fallback settings file {}: {error}",
                path.display()
            )
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "cannot read fallback settings file {}: {error}",
            path.display()
        )),
    }
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
        assert!(s.packages.is_none());
        assert!(s.npm_command.is_none());
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
                "quietStartup": true,
                "showTerminalProgress": false,
                "editorPaddingX": 3,
                "autocompleteMaxVisible": 7,
                "compaction": { "threshold": 100 },
                "npmCommand": ["mise", "exec", "node@20", "--", "npm"],
                "packages": [
                    "some-pkg",
                    {
                        "source": "npm:filtered-pkg",
                        "autoload": false,
                        "extensions": ["dist/index.js"],
                        "skills": ["skills/review"],
                        "prompts": ["prompts/review.md"],
                        "themes": ["themes/dark.json"],
                        "futureFilter": { "enabled": true }
                    }
                ]
            }"#,
        )
        .unwrap();
        let s = load_settings().unwrap();
        assert_eq!(s.default_provider.as_deref(), Some("anthropic"));
        assert_eq!(s.default_model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(s.default_thinking_level.as_deref(), Some("high"));
        assert_eq!(s.theme.as_deref(), Some("dark"));
        assert_eq!(
            s.npm_command.as_deref(),
            Some(
                ["mise", "exec", "node@20", "--", "npm"]
                    .map(String::from)
                    .as_slice()
            )
        );
        let packages = s.packages.as_deref().unwrap();
        assert_eq!(packages[0], PackageSetting::from("some-pkg"));
        assert_eq!(packages[1].source(), "npm:filtered-pkg");
        let PackageSetting::Filtered(filter) = &packages[1] else {
            panic!("expected an object-form package setting");
        };
        assert_eq!(filter.autoload, Some(false));
        assert_eq!(
            filter.extensions.as_deref(),
            Some(["dist/index.js".into()].as_slice())
        );
        assert_eq!(
            filter.skills.as_deref(),
            Some(["skills/review".into()].as_slice())
        );
        assert_eq!(
            filter.prompts.as_deref(),
            Some(["prompts/review.md".into()].as_slice())
        );
        assert_eq!(
            filter.themes.as_deref(),
            Some(["themes/dark.json".into()].as_slice())
        );
        assert_eq!(filter.unknown["futureFilter"]["enabled"], true);
        assert_eq!(s.hide_thinking_block, Some(true));
        assert_eq!(s.quiet_startup, Some(true));
        assert_eq!(s.show_terminal_progress, Some(false));
        assert_eq!(s.editor_padding_x, Some(3));
        assert_eq!(s.autocomplete_max_visible, Some(7));
    }

    #[test]
    fn rpi_project_settings_mask_native_pi_settings() {
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
        assert_eq!(settings.len(), 1);
        assert_eq!(
            settings[0].skill_dirs.as_deref(),
            Some(["rpi-skills".to_string()].as_slice())
        );
        assert_eq!(
            settings[0].extension_dirs.as_deref(),
            Some(["rpi-ext".to_string()].as_slice())
        );
    }

    #[test]
    fn native_pi_project_settings_are_a_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".pi")).unwrap();
        std::fs::write(
            tmp.path().join(".pi/settings.json"),
            r#"{"defaultProvider":"native-provider","defaultModel":"native-model"}"#,
        )
        .unwrap();

        let settings = load_project_settings_with_paths(tmp.path());
        assert_eq!(settings.len(), 1);
        assert!(settings[0].0.ends_with(".pi/settings.json"));
        assert_eq!(
            settings[0].1.default_provider.as_deref(),
            Some("native-provider")
        );
    }

    #[test]
    fn malformed_rpi_project_settings_mask_native_pi_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".rpi")).unwrap();
        std::fs::create_dir_all(tmp.path().join(".pi")).unwrap();
        std::fs::write(tmp.path().join(".rpi/settings.json"), "{ malformed").unwrap();
        std::fs::write(
            tmp.path().join(".pi/settings.json"),
            r#"{"packages":["npm:must-not-load"]}"#,
        )
        .unwrap();

        assert!(load_project_settings_with_paths(tmp.path()).is_empty());
        assert!(load_active_project_settings(tmp.path()).is_err());
    }

    #[test]
    fn trusted_project_model_defaults_override_global_defaults() {
        let _cfg = TempConfig::new();
        let global = config::settings_path().unwrap();
        std::fs::create_dir_all(global.parent().unwrap()).unwrap();
        std::fs::write(
            global,
            r#"{"defaultProvider":"global","defaultModel":"global-model","theme":"dark"}"#,
        )
        .unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join(".rpi")).unwrap();
        std::fs::write(
            project.path().join(".rpi/settings.json"),
            r#"{"defaultProvider":"project","defaultModel":"project-model"}"#,
        )
        .unwrap();

        let trusted = load_effective_model_settings(project.path(), true).unwrap();
        assert_eq!(trusted.default_provider.as_deref(), Some("project"));
        assert_eq!(trusted.default_model.as_deref(), Some("project-model"));
        assert_eq!(trusted.theme.as_deref(), Some("dark"));

        let untrusted = load_effective_model_settings(project.path(), false).unwrap();
        assert_eq!(untrusted.default_provider.as_deref(), Some("global"));
        assert_eq!(untrusted.default_model.as_deref(), Some("global-model"));
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

    /// The settings native pi lets users tune must reach the harness types.
    ///
    /// These were previously hardcoded defaults, so a `settings.json` copied from
    /// native pi parsed cleanly but changed nothing — the exact failure mode a
    /// "parses but is not consumed" field creates.

    /// `sessionDir` selects the session directory, with the CLI flag winning.
    ///
    /// Native pi's comment: "same format as --session-dir CLI flag". A settings
    /// key that parses but is ignored is exactly the failure mode this closes.
    #[test]
    fn session_dir_setting_selects_the_session_directory() {
        let settings: Settings =
            serde_json::from_str(r#"{"sessionDir": "/tmp/from-settings"}"#).unwrap();
        assert_eq!(
            settings.session_dir,
            Some(std::path::PathBuf::from("/tmp/from-settings"))
        );
        // Absent ⇒ the caller falls back to its built-in default.
        assert_eq!(Settings::default().session_dir, None);
    }

    /// The bash-facing keys must reach the tool options.
    #[test]
    fn shell_command_prefix_reaches_the_bash_tool() {
        let settings: Settings =
            serde_json::from_str(r#"{"shellCommandPrefix": "shopt -s expand_aliases"}"#).unwrap();
        assert_eq!(
            settings.shell_command_prefix.as_deref(),
            Some("shopt -s expand_aliases")
        );
        // `shellPath` is parsed (native's shape round-trips) but nothing consumes
        // it yet — see its doc comment for why.
        let with_shell: Settings =
            serde_json::from_str(r#"{"shellPath": "/usr/bin/zsh"}"#).unwrap();
        assert_eq!(with_shell.shell_path.as_deref(), Some("/usr/bin/zsh"));
    }

    /// `httpProxy` is stored in native's shape for the client setup to apply.
    #[test]
    fn http_proxy_setting_is_read() {
        let settings: Settings =
            serde_json::from_str(r#"{"httpProxy": "http://127.0.0.1:8888"}"#).unwrap();
        assert_eq!(
            settings.http_proxy.as_deref(),
            Some("http://127.0.0.1:8888")
        );
        assert_eq!(Settings::default().http_proxy, None);
    }
    #[test]
    fn harness_options_come_from_settings() {
        let settings: Settings = serde_json::from_str(
            r#"{
                "retry": { "enabled": false, "maxRetries": 7, "baseDelayMs": 250, "maxAgentDelayMs": 9000 },
                "compaction": { "enabled": false, "reserveTokens": 1234, "keepRecentTokens": 4321 },
                "steeringMode": "all",
                "followUpMode": "all"
            }"#,
        )
        .expect("settings parse");

        let retry = settings.retry_policy();
        assert!(!retry.enabled);
        assert_eq!(retry.max_retries, 7);
        assert_eq!(retry.base_delay_ms, 250);
        assert_eq!(retry.max_agent_delay_ms, 9000);

        let compaction = settings.compaction_settings();
        assert!(!compaction.enabled);
        assert_eq!(compaction.reserve_tokens, 1234);
        assert_eq!(compaction.keep_recent_tokens, 4321);

        assert_eq!(settings.steering_mode(), rpi_agent::QueueMode::All);
        assert_eq!(settings.follow_up_mode(), rpi_agent::QueueMode::All);
    }

    /// An absent key keeps the harness default rather than a zero value: a partial
    /// `retry` object must change only what it names.
    #[test]
    fn absent_harness_settings_keep_the_harness_defaults() {
        let empty = Settings::default();
        let default_retry = rpi_harness::types::RetryPolicy::default();
        let retry = empty.retry_policy();
        assert_eq!(retry.enabled, default_retry.enabled);
        assert_eq!(retry.max_retries, default_retry.max_retries);
        assert_eq!(retry.base_delay_ms, default_retry.base_delay_ms);
        assert_eq!(retry.max_agent_delay_ms, default_retry.max_agent_delay_ms);
        let default_compaction = rpi_harness::types::CompactionSettings::default();
        let compaction = empty.compaction_settings();
        assert_eq!(compaction.enabled, default_compaction.enabled);
        assert_eq!(compaction.reserve_tokens, default_compaction.reserve_tokens);
        assert_eq!(
            compaction.keep_recent_tokens,
            default_compaction.keep_recent_tokens
        );

        // Native pi's documented defaults for the queue modes.
        assert_eq!(empty.steering_mode(), rpi_agent::QueueMode::OneAtATime);
        assert_eq!(empty.follow_up_mode(), rpi_agent::QueueMode::OneAtATime);

        // A partial object keeps the defaults it does not name.
        let partial: Settings = serde_json::from_str(r#"{"retry":{"maxRetries":9}}"#).unwrap();
        let retry = partial.retry_policy();
        assert_eq!(retry.max_retries, 9);
        assert_eq!(retry.base_delay_ms, default_retry.base_delay_ms);
    }

    /// Native pi's key names must be accepted, not only rpi's.
    ///
    /// A user copying native pi's `settings.json` writes `enabledModels` and
    /// `httpIdleTimeoutMs`; before this they were silently ignored because rpi
    /// spelled them differently.
    #[test]
    fn native_settings_key_names_are_accepted() {
        let settings: Settings = serde_json::from_str(
            r#"{
                "enabledModels": ["a", "b"],
                "httpIdleTimeoutMs": 1500,
                "branchSummary": { "reserveTokens": 4242, "skipPrompt": true }
            }"#,
        )
        .expect("native key names parse");

        assert_eq!(
            settings.scoped_models,
            Some(vec!["a".to_string(), "b".to_string()]),
            "native's `enabledModels` must map onto rpi's scoped models"
        );
        assert_eq!(
            settings.http_idle_timeout_ms(),
            Some(1500),
            "native's `httpIdleTimeoutMs` must be read"
        );
        assert_eq!(settings.branch_summary_reserve_tokens(), Some(4242));
        assert!(settings.branch_summary_skipped());
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
    fn save_uses_atomic_sibling_replacement() {
        let (_tmp, _guard) = with_temp_env();
        let path = config::settings_path().unwrap();
        std::fs::write(&path, r#"{"theme":"dark","piOnlyField":true}"#).unwrap();

        let mut settings = load_settings().unwrap();
        settings.theme = Some("light".into());
        save_settings(&settings).unwrap();

        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["theme"], "light");
        assert_eq!(saved["piOnlyField"], true);
        // `config::atomic_write` cleans up its sibling by replacing the
        // target in one rename; a successful save leaves no staging file.
        let temp = path.with_file_name(format!(
            ".{}.tmp",
            path.file_name().and_then(|name| name.to_str()).unwrap()
        ));
        assert!(!temp.exists(), "atomic staging file should not remain");
    }

    #[test]
    fn save_preserves_unknown_fields_and_writes_the_tui_settings() {
        // Regression: `fullscreenCopyOnSelect` and `httpIdleTimeout` used to be
        // read but never written back, so `/settings` changes silently
        // vanished. The newer `/settings` rows (showImages,
        // showCacheMissNotices, outputPad) must round-trip too.
        let (_tmp, _guard) = with_temp_env();
        let path = config::settings_path().unwrap();
        let mut settings = Settings::default();
        settings.show_images = Some(false);
        settings.show_cache_miss_notices = Some(true);
        settings.output_pad = Some(0);
        settings.fullscreen_copy_on_select = Some(false);
        settings.http_idle_timeout = Some(serde_json::json!(30_000));
        settings.show_terminal_progress = Some(false);
        save_settings(&settings).unwrap();

        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["showImages"], false);
        assert_eq!(raw["showCacheMissNotices"], true);
        assert_eq!(raw["outputPad"], 0);
        assert_eq!(raw["fullscreenCopyOnSelect"], false);
        assert_eq!(raw["httpIdleTimeout"], 30_000);
        assert_eq!(raw["showTerminalProgress"], false);

        // And they load back.
        let reloaded = load_settings().unwrap();
        assert_eq!(reloaded.show_images(), Some(false));
        assert_eq!(reloaded.show_cache_miss_notices, Some(true));
        assert_eq!(reloaded.output_pad, Some(0));
        assert_eq!(reloaded.fullscreen_copy_on_select, Some(false));
        assert_eq!(reloaded.http_idle_timeout_ms(), Some(30_000));
        assert_eq!(reloaded.show_terminal_progress(), Some(false));
    }

    #[test]
    fn nested_terminal_block_is_accepted_as_a_fallback() {
        // Native pi nests `showImages` / `showTerminalProgress` under
        // `terminal`; rpi's flat keys win when both are present.
        let (_tmp, _guard) = with_temp_env();
        let path = config::settings_path().unwrap();
        std::fs::write(
            &path,
            r#"{ "terminal": { "showImages": false, "showTerminalProgress": false } }"#,
        )
        .unwrap();
        let settings = load_settings().unwrap();
        assert_eq!(settings.show_images(), Some(false));
        assert_eq!(settings.show_terminal_progress(), Some(false));

        std::fs::write(
            &path,
            r#"{
                "showImages": true,
                "terminal": { "showImages": false }
            }"#,
        )
        .unwrap();
        let settings = load_settings().unwrap();
        assert_eq!(settings.show_images(), Some(true));
    }

    #[test]
    fn save_preserves_unknown_fields() {
        let (_tmp, _guard) = with_temp_env();
        let path = config::settings_path().unwrap();
        std::fs::write(
            &path,
            r#"{
                "piOnlyField": "keep-me",
                "theme": "dark",
                "npmCommand": ["pnpm"],
                "packages": [{
                    "source": "npm:future-package",
                    "autoload": false,
                    "futureFilter": { "enabled": true }
                }]
            }"#,
        )
        .unwrap();
        let mut s = load_settings().unwrap();
        assert_eq!(s.npm_command, Some(vec!["pnpm".to_string()]));
        s.scoped_models = Some(vec!["m1".into()]);
        save_settings(&s).unwrap();
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["piOnlyField"], "keep-me");
        assert_eq!(raw["scopedModels"][0], "m1");
        assert_eq!(raw["theme"], "dark");
        assert_eq!(raw["npmCommand"][0], "pnpm");
        assert_eq!(raw["packages"][0]["source"], "npm:future-package");
        assert_eq!(raw["packages"][0]["autoload"], false);
        assert_eq!(raw["packages"][0]["futureFilter"]["enabled"], true);
    }

    #[test]
    fn save_preserves_unknown_fields_and_packages_with_line_comments() {
        let (_tmp, _guard) = with_temp_env();
        let path = config::settings_path().unwrap();
        let original = r#"{
            // Native Pi permits comments in settings files.
            "piOnlyField": { "keep": true },
            "theme": "dark",
            "packages": [
                // Keep the package object and fields that rpi does not use.
                {
                    "source": "npm:future-package",
                    "autoload": false,
                    "futureFilter": { "enabled": true }
                }
            ]
        }
        "#;
        std::fs::write(&path, original).unwrap();

        let mut settings = load_settings().unwrap();
        settings.scoped_models = Some(vec!["m1".into()]);
        save_settings(&settings).unwrap();

        // The comments may be normalized away by pretty-printing, but every
        // unknown field and package property must survive the merge.
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["piOnlyField"]["keep"], true);
        assert_eq!(raw["theme"], "dark");
        assert_eq!(raw["packages"][0]["source"], "npm:future-package");
        assert_eq!(raw["packages"][0]["autoload"], false);
        assert_eq!(raw["packages"][0]["futureFilter"]["enabled"], true);
        assert_eq!(raw["scopedModels"][0], "m1");
    }

    #[test]
    fn save_fails_closed_for_unparseable_existing_settings() {
        let (_tmp, _guard) = with_temp_env();
        let path = config::settings_path().unwrap();
        // This is not recoverable by stripping comments (the closing brace is
        // missing). Saving must leave the user's file byte-for-byte intact.
        let original = b"{\n  // keep this file intact\n  \"piOnlyField\": \"keep-me\"\n";
        std::fs::write(&path, original).unwrap();

        let mut settings = Settings::default();
        settings.theme = Some("light".into());
        let error =
            save_settings(&settings).expect_err("malformed settings must not be overwritten");
        assert!(error.contains("cannot parse existing settings file"));
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn project_save_seeds_from_native_pi_without_losing_unknown_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join(".pi/settings.json");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(
            &legacy,
            r#"{
                // Preserve fields from native Pi on the first rpi save.
                "piOnlyField": { "keep": true },
                "packages": ["npm:existing"]
            }"#,
        )
        .unwrap();

        let mut settings = load_project_settings_for_write(tmp.path()).unwrap();
        settings.theme = Some("dark".into());
        save_project_settings(tmp.path(), &settings).unwrap();

        let preferred = tmp.path().join(".rpi/settings.json");
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(preferred).unwrap()).unwrap();
        assert_eq!(saved["piOnlyField"]["keep"], true);
        assert_eq!(saved["packages"][0], "npm:existing");
        assert_eq!(saved["theme"], "dark");
    }

    #[test]
    fn project_save_fails_closed_for_malformed_native_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join(".pi/settings.json");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "{ malformed").unwrap();

        assert!(load_project_settings_for_write(tmp.path()).is_err());
        assert!(!tmp.path().join(".rpi/settings.json").exists());
    }
}
