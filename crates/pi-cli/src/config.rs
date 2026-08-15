//! `~/.rpi/` persistent configuration — auth + model catalog. Mirrors (a
//! Rust-flattened slice of) the TS `packages/coding-agent/src/config.ts`
//! (`getAgentDir`/`getAuthPath`/`getModelsPath`) + `core/auth-storage.ts`
//! (`FileAuthStorageBackend`) + `core/model-config.ts` (`ModelConfig`).
//!
//! # Layout (divergence from upstream — documented in `docs/m6-cli-open-questions.md`)
//!
//! Upstream uses `~/.pi/agent/{auth.json, models.json, …}` because the same dir
//! also hosts themes/bin/prompts/sessions. rpi v1 has only two files, so it
//! drops the `agent/` layer and goes flat:
//!
//! ```text
//! ~/.rpi/                 (RPI_CODING_AGENT_DIR env overrides this)
//! ├── auth.json          # persisted credentials (mode 0o600 on Unix)
//! └── models.json        # user-defined provider/model catalog (hand-edited)
//! ```
//!
//! # Concurrency
//!
//! v1 is a single-process CLI, so we use **atomic rename** instead of upstream's
//! `proper-lockfile`: write a sibling temp file, `fs::rename` over the target,
//! then `chmod 0o600` on Unix (Windows chmod is a no-op, matching Node).
//! Concurrent `rpi auth login` from two shells could lose one update — that's
//! accepted and documented; adding a file lock is deferred.
//!
//! # models.json credential expansion
//!
//! Upstream `resolveConfigValue` expands `$ENV`/`!command`/`${ENV}` inside
//! `apiKey`/`headers`. **v1 does not** — only literal strings are accepted
//! (use the `ANTHROPIC_*` env vars for dynamic secrets). Documented divergence.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rpi_ai::{Api, InputModality, Model};

/// The config directory name under the home dir. Upstream is `.pi`; rpi uses
/// `.rpi` to avoid colliding with a native `pi` install on the same machine.
pub const CONFIG_DIR_NAME: &str = ".rpi";

/// Env var that overrides the whole config dir (mirrors upstream
/// `PI_CODING_AGENT_DIR`). Absolute path; relative values are rejected.
pub const CONFIG_DIR_ENV: &str = "RPI_CODING_AGENT_DIR";

/// The provider id under which `rpi auth login` stores the Anthropic key.
/// Mirrors upstream's fixed `anthropic` provider id.
pub const DEFAULT_PROVIDER_ID: &str = "anthropic";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A config-layer error (path resolution, IO, JSON). Surfaced to the user by
/// the `auth` subcommand / `provider::resolve`.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not resolve home directory (set {env} to override)")]
    NoHomeDir { env: &'static str },
    #[error("config dir override {env}={val:?} is not an absolute path")]
    RelativeOverride { env: &'static str, val: String },
    #[error("could not read {path}: {source}")]
    Read { path: PathBuf, #[source] source: std::io::Error },
    #[error("could not write {path}: {source}")]
    Write { path: PathBuf, #[source] source: std::io::Error },
    #[error("invalid JSON in {path}: {source}")]
    Json { path: PathBuf, #[source] source: serde_json::Error },
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// The rpi config directory (`~/.rpi` by default, `RPI_CODING_AGENT_DIR`
/// override). Creates nothing — purely a path computation.
pub fn agent_dir() -> Result<PathBuf, ConfigError> {
    if let Some(val) = std::env::var_os(CONFIG_DIR_ENV) {
        let p = PathBuf::from(&val);
        if !p.is_absolute() {
            return Err(ConfigError::RelativeOverride {
                env: CONFIG_DIR_ENV,
                val: val.to_string_lossy().into_owned(),
            });
        }
        return Ok(p);
    }
    let home = dirs::home_dir()
        .ok_or(ConfigError::NoHomeDir { env: CONFIG_DIR_ENV })?;
    Ok(home.join(CONFIG_DIR_NAME))
}

/// `~/.rpi/auth.json`.
pub fn auth_path() -> Result<PathBuf, ConfigError> {
    Ok(agent_dir()?.join("auth.json"))
}

/// `~/.rpi/models.json`.
pub fn models_path() -> Result<PathBuf, ConfigError> {
    Ok(agent_dir()?.join("models.json"))
}

// ---------------------------------------------------------------------------
// auth.json — Credential store
// ---------------------------------------------------------------------------

/// A stored credential. Mirrors the TS `Credential` union
/// (`packages/ai/src/auth/types.ts`). The `Oauth` variant exists for forward
/// compatibility but v1 never writes it (no OAuth device-code flow); `resolve`
/// does not consume it.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Credential {
    /// An API key, optionally sourced from an env var map. v1 stores only the
    /// literal `key` (the `env` field is kept for upstream-shape compatibility).
    ApiKey {
        key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<BTreeMap<String, String>>,
    },
    /// OAuth tokens (access + refresh + expiry). v1 does not write this.
    Oauth {
        access: String,
        refresh: String,
        /// Unix epoch seconds.
        expires: i64,
    },
}

/// The auth store: `providerId -> Credential`. Mirrors upstream
/// `Record<providerId, Credential>`.
pub type AuthStore = BTreeMap<String, Credential>;

/// Read the auth store. Missing file ⇒ empty store (not an error). Malformed
/// JSON ⇒ `ConfigError::Json` (we do not silently swallow a corrupt auth file).
pub fn read_auth() -> Result<AuthStore, ConfigError> {
    let path = auth_path()?;
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(serde_json::from_str(&text).map_err(|e| ConfigError::Json {
            path: path.clone(),
            source: e,
        })?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AuthStore::new()),
        Err(e) => Err(ConfigError::Read { path, source: e }),
    }
}

/// Atomically write the whole auth store (ensures the dir exists, writes a
/// temp sibling, `rename`s over the target, then `chmod 0o600` on Unix).
pub fn write_auth(store: &AuthStore) -> Result<(), ConfigError> {
    let path = auth_path()?;
    let dir = agent_dir()?;
    ensure_dir(&dir)?;
    let json = serde_json::to_string_pretty(store).unwrap();
    atomic_write(&path, json.as_bytes())?;
    set_owner_only(&path);
    Ok(())
}

/// Read-modify-write: upsert a credential for `provider_id`.
pub fn upsert_credential(provider_id: &str, cred: Credential) -> Result<(), ConfigError> {
    let mut store = read_auth()?;
    store.insert(provider_id.to_string(), cred);
    write_auth(&store)
}

/// Remove `provider_id` from the store. Returns `true` if a credential was
/// present (and is now gone), `false` if it was already absent. Always rewrites
/// the file when the provider existed (so `auth logout` reflects the new state
/// on disk even if the map isn't empty).
pub fn delete_credential(provider_id: &str) -> Result<bool, ConfigError> {
    let mut store = read_auth()?;
    if store.remove(provider_id).is_some() {
        write_auth(&store)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// models.json — provider/model catalog
// ---------------------------------------------------------------------------

/// The `models.json` document. Mirrors TS `{ providers: Record<id, ProviderConfig> }`
/// (`core/model-config.ts` `ModelsConfigSchema`).
#[derive(serde::Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ModelsConfig {
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
}

/// A provider entry in `models.json`. The fields mirror the TS `ProviderConfig`
/// one-for-one; v1 honors `base_url`/`api_key`/`headers`/`auth_header`/`models`,
/// and **ignores** `api` values other than `anthropic-messages` (documented).
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api: Option<String>,
    #[serde(default)]
    pub headers: Option<BTreeMap<String, String>>,
    /// `true` ⇒ wrap `api_key` as `Authorization: Bearer <key>` (mirrors
    /// upstream `provider-composer.ts` `authHeader`).
    #[serde(default)]
    pub auth_header: Option<bool>,
    #[serde(default)]
    pub models: Vec<ModelDefinition>,
}

/// One model under a provider. `id` is required (mirrors TS `ModelDefinition`).
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ModelDefinition {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub reasoning: Option<bool>,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    /// Free-form modality strings ("text"/"image"); unknown values fall back
    /// to text-only.
    #[serde(default)]
    pub input: Option<Vec<String>>,
    #[serde(default)]
    pub headers: Option<BTreeMap<String, String>>,
}

/// Load `~/.rpi/models.json`. Missing file ⇒ empty config (no error).
pub fn load_models_config() -> Result<ModelsConfig, ConfigError> {
    let path = models_path()?;
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_models_json(&text).map_err(|e| ConfigError::Json {
            path: path.clone(),
            source: e,
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ModelsConfig::default()),
        Err(e) => Err(ConfigError::Read { path, source: e }),
    }
}

/// Parse the models JSON, tolerating `//` line comments (a minimal subset of
/// upstream's `stripJsonComments`). Tries strict JSON first; on failure, strips
/// `//…` to end-of-line and retries.
fn parse_models_json(text: &str) -> Result<ModelsConfig, serde_json::Error> {
    match serde_json::from_str(text) {
        Ok(c) => Ok(c),
        Err(first) => {
            // Best-effort comment strip — only `//` to EOL, never inside strings
            // (a `//` inside a JSON string would already have made the strict
            // parse fail for a *different* reason; stripping naively is an
            // acceptable v1 trade-off, documented as a limitation).
            let stripped: String = text
                .lines()
                .map(|line| {
                    if let Some(idx) = find_line_comment(line) {
                        line[..idx].to_string()
                    } else {
                        line.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            serde_json::from_str(&stripped).map_err(|_| first)
        }
    }
}

/// Index of a `//` line comment that is *not* inside a double-quoted string.
fn find_line_comment(line: &str) -> Option<usize> {
    let mut in_str = false;
    let mut esc = false;
    for (i, ch) in line.char_indices() {
        if esc {
            esc = false;
            continue;
        }
        match ch {
            '\\' if in_str => esc = true,
            '"' => in_str = !in_str,
            '/' if !in_str => {
                if line.as_bytes().get(i + 1) == Some(&b'/') {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Whether the entry under `provider_id` speaks the v1-honored protocol
/// (`anthropic-messages`, or omitted/unknown). Unknown `api` is allowed through
/// for forward-compat but flagged ignored-in-v1 in the docs. Public so
/// [`crate::provider`] can scan models.json providers for an `authHeader:true`
/// gateway bearer source.
pub fn provider_is_anthropic_compatible(cfg: &ProviderConfig) -> bool {
    match cfg.api.as_deref() {
        None | Some("") | Some("anthropic-messages") => true,
        _ => false,
    }
}

/// Convert a `(provider_id, ProviderConfig)` pair into a list of library
/// [`Model`]s. Provider-level `base_url`/`headers`/`auth_header` fold into each
/// model. Returns `None` for non-anthropic providers (v1 ignores them).
pub fn provider_to_models(
    provider_id: &str,
    cfg: &ProviderConfig,
) -> Option<Vec<Model>> {
    let _ = provider_id; // config-namespacing only; v1 routes via the single AnthropicProvider.
    if !provider_is_anthropic_compatible(cfg) {
        return None;
    }
    let provider_base = cfg.base_url.clone().unwrap_or_else(default_anthropic_base_url);
    let mut merged: Vec<Model> = Vec::with_capacity(cfg.models.len());
    for def in &cfg.models {
        let base_url = def
            .base_url
            .clone()
            .unwrap_or_else(|| provider_base.clone());
        let name = def.name.clone().unwrap_or_else(|| def.id.clone());
        // v1 routes EVERY anthropic-messages model through the single
        // `AnthropicProvider` (whose `id()` is "anthropic"). Upstream
        // `registerProvider(providerName, …)` registers a distinct provider per
        // models.json key and routes by that key; v1 has no multi-provider
        // registry, so the models.json provider id is config-namespacing only
        // — the per-model `base_url` + `headers` carry the actual endpoint/auth
        // differentiation. Stamping `provider = "anthropic"` here lets the
        // harness's `resolve_provider` (`provider.id() == model.provider`)
        // match. Without this, a `gateway/custom-claude` model would carry
        // `provider = "gateway"` and the run would fail with "No provider
        // registered for 'gateway'". Divergence documented in
        // `docs/m6-cli-open-questions.md`.
        let mut m = Model::new(
            def.id.clone(),
            name,
            Api::AnthropicMessages,
            DEFAULT_PROVIDER_ID.to_string(),
            base_url,
        );
        m.reasoning = def.reasoning.unwrap_or(false);
        m.context_window = def.context_window.unwrap_or(0);
        m.max_tokens = def.max_tokens.unwrap_or(0);
        m.input = parse_input_modalities(def.input.as_deref());
        // Merge: model-level headers, then provider-level headers (provider wins
        // on conflict — it's the more specific-to-this-endpoint declaration).
        // NOTE: the `authHeader:true` Bearer synthesis is NOT done here —
        // [`crate::provider::resolve`] applies it centrally so it can skip it
        // when a higher-priority x-api-key source (`--api-key` / auth.json /
        // `ANTHROPIC_API_KEY`) wins. Folding it here unconditionally would put a
        // Bearer on the model even on the x-api-key path. See
        // `models_json_bearer_token` + the fold loop in `resolve`.
        let mut headers: BTreeMap<String, String> = BTreeMap::new();
        if let Some(h) = def.headers.clone() {
            headers.extend(h);
        }
        if let Some(h) = cfg.headers.clone() {
            headers.extend(h);
        }
        if !headers.is_empty() {
            m.headers = Some(headers);
        }
        merged.push(m);
    }
    Some(merged)
}

/// Parse `["text","image"]`-style modality strings into [`InputModality`]s;
/// unknown values drop to text-only. `None` ⇒ text (the [`Model::new`] default).
fn parse_input_modalities(input: Option<&[String]>) -> Vec<InputModality> {
    match input {
        None => vec![InputModality::Text],
        Some(list) if list.is_empty() => vec![InputModality::Text],
        Some(list) => list
            .iter()
            .filter_map(|s| match s.to_ascii_lowercase().as_str() {
                "text" => Some(InputModality::Text),
                "image" => Some(InputModality::Image),
                _ => None,
            })
            .collect::<Vec<_>>()
            .pipe(|v| if v.is_empty() { vec![InputModality::Text] } else { v }),
    }
}

/// The first-party Anthropic endpoint — used as the fallback `base_url` when a
/// models.json provider omits it. Kept here (not imported from `rpi_ai`) so the
/// config layer never depends on the provider's private `models` module.
/// Public so [`crate::provider::resolve`] can tell a gateway model (whose
/// `base_url` differs from this) from a built-in Anthropic model.
pub const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Same value as [`ANTHROPIC_DEFAULT_BASE_URL`], as an owned `String` for the
/// `unwrap_or_else` ergonomic used by [`provider_to_models`].
fn default_anthropic_base_url() -> String {
    ANTHROPIC_DEFAULT_BASE_URL.to_string()
}

// ---------------------------------------------------------------------------
// Internals: dir ensure, atomic write, chmod
// ---------------------------------------------------------------------------

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// Create the config dir if missing. Mode 0o700 on Unix (mkdir default on
/// Windows, where the sticky-permission concept doesn't apply).
fn ensure_dir(dir: &Path) -> Result<(), ConfigError> {
    if dir.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dir).map_err(|e| ConfigError::Write {
        path: dir.to_path_buf(),
        source: e,
    })?;
    #[cfg(unix)]
    {
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// Write `bytes` to `path` atomically: a temp sibling → `rename`. The temp
/// file lives next to the target so the rename stays on one filesystem.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ConfigError> {
    let dir = path
        .parent()
        .ok_or_else(|| ConfigError::Write {
            path: path.to_path_buf(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent"),
        })?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("rpi")
    ));
    std::fs::write(&tmp, bytes).map_err(|e| ConfigError::Write { path: tmp.clone(), source: e })?;
    std::fs::rename(&tmp, path).map_err(|e| ConfigError::Write {
        path: path.to_path_buf(),
        source: e,
    })?;
    Ok(())
}

/// Best-effort tighten to owner-only (0o600). No-op on Windows (the Node
/// upstream applies no ACL either).
fn set_owner_only(_path: &Path) {
    #[cfg(unix)]
    {
        let _ = std::fs::set_permissions(
            _path,
            std::fs::Permissions::from_mode(0o600),
        );
    }
}

// A tiny `.pipe`-shim so the `parse_input_modalities` chain reads top-to-bottom
// without pulling itertools. Kept private to this module.
trait Pipe: Sized {
    fn pipe<R>(self, f: impl FnOnce(Self) -> R) -> R {
        f(self)
    }
}
impl<T> Pipe for T {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod test_support {
    /// A shared workspace lock for tests that touch process-global env vars
    /// (`RPI_CODING_AGENT_DIR`, `ANTHROPIC_*`). All env-mutating tests across
    /// the crate (config / provider / auth) share this ONE mutex so they can't
    /// race on the shared environment. Hold the returned guard for the whole
    /// test (store it in a RAII struct).
    use std::sync::{Mutex, OnceLock};
    pub(crate) fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::env_lock;

    /// Point `RPI_CODING_AGENT_DIR` at a fresh temp dir for the duration of the
    /// test (cleaned up on drop). Holds the prior value of the env var to
    /// restore it. Hold the env lock for its whole lifetime.
    struct TempConfig {
        _guard: std::sync::MutexGuard<'static, ()>,
        _tmp: tempfile::TempDir,
        prev: Option<std::ffi::OsString>,
    }
    impl TempConfig {
        fn new() -> Self {
            let guard = env_lock().lock().unwrap();
            let prev = std::env::var_os(CONFIG_DIR_ENV);
            let tmp = tempfile::TempDir::new().unwrap();
            std::env::set_var(CONFIG_DIR_ENV, tmp.path());
            Self { _guard: guard, _tmp: tmp, prev }
        }
    }
    impl Drop for TempConfig {
        fn drop(&mut self) {
            restore_env(CONFIG_DIR_ENV, self.prev.take());
        }
    }

    #[test]
    fn read_auth_missing_file_is_empty() {
        let _cfg = TempConfig::new();
        let store = read_auth().unwrap();
        assert!(store.is_empty());
    }

    #[test]
    fn upsert_then_read_roundtrip() {
        let _cfg = TempConfig::new();
        upsert_credential(
            "anthropic",
            Credential::ApiKey { key: Some("sk-test-123".into()), env: None },
        )
        .unwrap();
        let store = read_auth().unwrap();
        match store.get("anthropic") {
            Some(Credential::ApiKey { key, .. }) => assert_eq!(key.as_deref(), Some("sk-test-123")),
            other => panic!("unexpected cred: {other:?}"),
        }
        // The file should exist and be JSON.
        let path = auth_path().unwrap();
        assert!(path.exists(), "auth.json should exist after upsert");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"anthropic\""));
        assert!(raw.contains("api_key"));
    }

    #[test]
    fn delete_credential_removes_entry() {
        let _cfg = TempConfig::new();
        upsert_credential("anthropic", Credential::ApiKey { key: Some("k".into()), env: None })
            .unwrap();
        assert!(delete_credential("anthropic").unwrap());
        // Second delete is a no-op.
        assert!(!delete_credential("anthropic").unwrap());
        assert!(read_auth().unwrap().is_empty());
    }

    #[test]
    fn load_models_config_missing_is_empty() {
        let _cfg = TempConfig::new();
        let c = load_models_config().unwrap();
        assert!(c.providers.is_empty());
    }

    #[test]
    fn load_models_config_parses_with_comments() {
        let _cfg = TempConfig::new();
        let json = r#"{
  // a one-api style gateway
  "providers": {
    "gateway": {
      "baseUrl": "https://gw.example.com",
      "authHeader": true,
      "apiKey": "gw-secret",
      "models": [
        { "id": "claude-sonnet-5", "name": "Sonnet via gateway" }
      ]
    }
  }
}"#;
        std::fs::write(models_path().unwrap(), json).unwrap();
        let c = load_models_config().unwrap();
        let gw = c.providers.get("gateway").expect("gateway provider present");
        assert_eq!(gw.base_url.as_deref(), Some("https://gw.example.com"));
        assert!(gw.auth_header.unwrap_or(false));
        assert_eq!(gw.models.len(), 1);
        assert_eq!(gw.models[0].id, "claude-sonnet-5");
    }

    #[test]
    fn provider_to_models_merges_headers_without_synth_bearer() {
        // `provider_to_models` merges model-level then provider-level headers,
        // but does NOT synthesize the `authHeader:true` Bearer itself — that
        // happens centrally in `crate::provider::resolve` (via
        // `models_json_bearer_token`) so it can be skipped on the x-api-key
        // path. Here the model carries only what the file declared.
        let cfg = ProviderConfig {
            name: None,
            base_url: Some("https://gw.example.com".into()),
            api_key: Some("gw-secret".into()),
            api: None,
            headers: Some({
                let mut h = BTreeMap::new();
                h.insert("x-portkey-key".into(), "portkey-secret".into());
                h
            }),
            auth_header: Some(true),
            models: vec![ModelDefinition {
                id: "claude-sonnet-5".into(),
                name: None,
                base_url: None,
                reasoning: None,
                context_window: None,
                max_tokens: None,
                input: None,
                headers: None,
            }],
        };
        let models = provider_to_models("gateway", &cfg).expect("anthropic-compatible");
        assert_eq!(models.len(), 1);
        let m = &models[0];
        assert_eq!(m.id, "claude-sonnet-5");
        assert_eq!(m.base_url, "https://gw.example.com");
        // v1 stamps `provider = "anthropic"` on every models.json model so the
        // single AnthropicProvider routes it (the models.json provider id is
        // config-namespacing only).
        assert_eq!(m.provider, DEFAULT_PROVIDER_ID);
        let headers = m.headers.as_ref().expect("provider headers merged");
        // Declared provider header folds in…
        assert_eq!(
            headers.get("x-portkey-key").map(|s| s.as_str()),
            Some("portkey-secret")
        );
        // …but no Bearer is synthesized here. The bearer-from-authHeader path
        // is exercised end-to-end by the provider.rs `resolve` tests
        // (`models_json_auth_header_satisfies_auth_without_env`,
        // `api_key_flag_beats_models_json_bearer`).
        assert!(
            headers.get("authorization").is_none(),
            "provider_to_models must not synthesize the Bearer; resolve does"
        );
    }

    #[test]
    fn provider_to_models_ignores_non_anthropic_api() {
        let cfg = ProviderConfig {
            name: None,
            base_url: None,
            api_key: None,
            api: Some("openai-completions".into()),
            headers: None,
            auth_header: None,
            models: vec![],
        };
        assert!(provider_to_models("oai", &cfg).is_none());
    }

    #[test]
    fn malformed_auth_json_is_an_error_not_silent_empty() {
        let _cfg = TempConfig::new();
        std::fs::write(auth_path().unwrap(), "{ not json").unwrap();
        assert!(matches!(read_auth(), Err(ConfigError::Json { .. })));
    }

    #[test]
    fn agent_dir_respects_env_override() {
        let _guard = env_lock().lock().unwrap();
        let prev = std::env::var_os(CONFIG_DIR_ENV);
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var(CONFIG_DIR_ENV, tmp.path());
        let dir = agent_dir().unwrap();
        restore_env(CONFIG_DIR_ENV, prev);
        assert_eq!(dir, tmp.path());
    }

    #[test]
    fn relative_override_is_rejected() {
        let _guard = env_lock().lock().unwrap();
        let prev = std::env::var_os(CONFIG_DIR_ENV);
        std::env::set_var(CONFIG_DIR_ENV, "relative/path");
        let err = agent_dir().unwrap_err();
        restore_env(CONFIG_DIR_ENV, prev);
        assert!(matches!(err, ConfigError::RelativeOverride { .. }));
    }

    /// Restore/remove an env var based on its prior `OsString` value.
    fn restore_env(name: &str, prev: Option<std::ffi::OsString>) {
        match prev {
            Some(v) => std::env::set_var(name, v),
            None => std::env::remove_var(name),
        }
    }
}
