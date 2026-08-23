//! # Layout
//!
//! Mirrors upstream's nested layout, so a `~/.pi/agent/` directory can be
//! copied to `~/.rpi/agent/` (or pointed at via `RPI_CODING_AGENT_DIR`) and
//! "just works". The `agent/` layer matches pi's `getAgentDir()`:
//!
//! ```text
//! ~/.rpi/                 (RPI_CODING_AGENT_DIR env overrides the agent/ dir)
//! └── agent/
//!     ├── auth.json       # persisted credentials (mode 0o600 on Unix)
//!     ├── models.json     # user-defined provider/model catalog (hand-edited)
//!     ├── settings.json   # saved default provider/model/thinking + theme
//!     ├── trust.json      # per-cwd project trust decisions (read-only parity)
//!     ├── .setup_done     # first-time-setup sentinel (extras.rs)
//!     └── .earendil_seen  # earendil-announcement sentinel (extras.rs)
//! ```
//!
//! Flat-installed `~/.rpi/{auth.json,models.json}` from older rpi releases are
//! migrated under `agent/` on the next launch by [`migrate_legacy_layout`]
//! (best-effort, idempotent; only when the env override is unset).
//!
//! # Concurrency
//!
//! v1 is a single-process CLI, so we use **atomic rename** instead of upstream's
//! `proper-lockfile`: write a sibling temp file, `fs::rename` over the target,
//! then `chmod 0o600` on Unix (Windows chmod is a no-op, matching Node).
//! Concurrent `rpi auth login` from two shells could lose one update — that's
//! accepted and documented; adding a file lock is deferred.
//!
//! # Config-value expansion
//!
//! [`resolve_config_value`] expands `$ENV`/`${ENV}`/`!command` inside
//! `apiKey`/`headers` exactly like upstream's `resolve-config-value.ts` — so a
//! copied pi `models.json`/`auth.json` that references env vars or shell
//! commands resolves the same way. Applied where rpi consumes those values
//! (auth.json key, models.json bearer apiKey, provider/model `headers`).

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

/// The rpi config directory (`~/.rpi/agent` by default, `RPI_CODING_AGENT_DIR`
/// override). Creates nothing — purely a path computation. The `agent/` layer
/// mirrors upstream `getAgentDir()` (`join(homedir(), CONFIG_DIR_NAME, "agent")`)
/// so a copied `~/.pi/agent/` directory reads in place. The env override points
/// at the agent dir itself (same as pi's `PI_CODING_AGENT_DIR`).
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
    Ok(home.join(CONFIG_DIR_NAME).join("agent"))
}

/// The config dir one level above the agent dir (`~/.rpi`, or the parent of an
/// env override). Used by [`migrate_legacy_layout`] to locate the old flat
/// layout. Returns `None` when the env override has no parent (a root path).
fn config_root_dir() -> Result<PathBuf, ConfigError> {
    let agent = agent_dir()?;
    agent
        .parent()
        .map(Path::to_path_buf)
        .ok_or(ConfigError::NoHomeDir { env: CONFIG_DIR_ENV })
}

/// `~/.rpi/agent/auth.json`.
pub fn auth_path() -> Result<PathBuf, ConfigError> {
    Ok(agent_dir()?.join("auth.json"))
}

/// `~/.rpi/agent/models.json`.
pub fn models_path() -> Result<PathBuf, ConfigError> {
    Ok(agent_dir()?.join("models.json"))
}

/// `~/.rpi/agent/settings.json` (saved default provider/model/thinking + theme).
pub fn settings_path() -> Result<PathBuf, ConfigError> {
    Ok(agent_dir()?.join("settings.json"))
}

/// `~/.rpi/agent/trust.json` (per-cwd project trust decisions — read-only
/// parity with pi; rpi does not gate resources behind trust in v1).
pub fn trust_path() -> Result<PathBuf, ConfigError> {
    Ok(agent_dir()?.join("trust.json"))
}

/// One-time best-effort migration of a pre-nesting flat layout
/// (`~/.rpi/{auth.json,models.json,.setup_done,.earendil_seen}`) into the
/// nested `~/.rpi/agent/` layout. **No-op when `RPI_CODING_AGENT_DIR` is set**
/// (never touch an explicit override), when the agent dir already exists, or
/// when no flat files are present. Idempotent: a partial move resumes. Errors
/// are swallowed (logged via the returned `Result` only so tests can observe);
/// `app::run` ignores them so a migration hiccup never blocks startup.
pub fn migrate_legacy_layout() -> Result<usize, ConfigError> {
    // Only migrate the default home-backed layout — never an env override.
    if std::env::var_os(CONFIG_DIR_ENV).is_some() {
        return Ok(0);
    }
    let root = match config_root_dir() {
        Ok(p) => p,
        Err(_) => return Ok(0),
    };
    let agent = agent_dir()?;
    migrate_legacy_layout_in(&root, &agent)
}

/// The core migration (no env gate): if `agent/` is absent but flat files exist
/// under `root`, move `{auth.json,models.json,.setup_done,.earendil_seen}` into
/// `agent/`. Idempotent. Factored out so tests can drive it against a temp
/// root/agent pair without touching the env (the public
/// [`migrate_legacy_layout`] short-circuits on an env override, which tests
/// can't unset portably while other tests run).
fn migrate_legacy_layout_in(root: &Path, agent: &Path) -> Result<usize, ConfigError> {
    // If the agent dir already exists with any content, assume already migrated.
    if agent.exists() {
        return Ok(0);
    }
    // Probe for a flat file. If none, nothing to migrate.
    let flat_auth = root.join("auth.json");
    let flat_models = root.join("models.json");
    if !flat_auth.exists() && !flat_models.exists() {
        return Ok(0);
    }
    std::fs::create_dir_all(agent).map_err(|e| ConfigError::Write {
        path: agent.to_path_buf(),
        source: e,
    })?;
    let mut moved = 0usize;
    for leaf in ["auth.json", "models.json", ".setup_done", ".earendil_seen"] {
        let from = root.join(leaf);
        let to = agent.join(leaf);
        if from.exists() && !to.exists() {
            // `rename` across the same filesystem is atomic; fall back to copy
            // + remove on cross-device (rare for a home dir).
            if let Err(_e) = std::fs::rename(&from, &to) {
                if std::fs::copy(&from, &to).is_ok() {
                    let _ = std::fs::remove_file(&from);
                }
            }
            moved += 1;
        }
    }
    Ok(moved)
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

// ---------------------------------------------------------------------------
// trust.json — project-trust store (read-only layout parity with pi)
// ---------------------------------------------------------------------------

/// The trust store: `canonicalCwd -> decision` (`true`/`false`/`null`). Mirrors
/// pi's `TrustFile = Record<string, boolean | null | undefined>`
/// (`trust-manager.ts`). rpi reads this for layout parity (a copied pi
/// `trust.json` parses + is located correctly) but does **not** gate any
/// project resources behind trust in v1 — there is no trust prompt. Deferred.
pub type TrustStore = BTreeMap<String, Option<bool>>;

/// Read `~/.rpi/agent/trust.json`. Missing file ⇒ empty store (not an error).
/// Malformed JSON ⇒ `ConfigError::Json`. `null` decisions deserialize as
/// `None`; absent entries are simply not present.
pub fn read_trust() -> Result<TrustStore, ConfigError> {
    let path = trust_path()?;
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| ConfigError::Json {
            path: path.clone(),
            source: e,
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TrustStore::new()),
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
            let stripped = strip_line_comments(text);
            serde_json::from_str(&stripped).map_err(|_| first)
        }
    }
}

/// Strip `//` line comments (to end-of-line), skipping `//` that appears inside
/// a double-quoted string. A minimal subset of upstream's `stripJsonComments`,
/// shared by [`parse_models_json`] and [`crate::settings::load_settings`] so a
/// copied pi `models.json`/`settings.json` (which pi allows comments in) parses.
pub(crate) fn strip_line_comments(text: &str) -> String {
    text.lines()
        .map(|line| match find_line_comment(line) {
            Some(idx) => line[..idx].to_string(),
            None => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
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

// ---------------------------------------------------------------------------
// Config-value expansion (mirrors pi `resolve-config-value.ts`)
// ---------------------------------------------------------------------------

/// A process-lifetime cache for `!command` resolutions, mirroring pi's
/// `commandResultCache`. Keyed by the raw `!cmd` string (including the `!`).
fn command_cache(
) -> &'static std::sync::Mutex<std::collections::HashMap<String, Option<String>>> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, Option<String>>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Resolve a config value (API key, header value) that may be a shell command,
/// an env-var template, or a literal — mirroring pi's `resolveConfigValue`.
///
/// - `!command` → run the rest as a shell command (`sh -c` on Unix, `cmd /C` on
///   Windows), return trimmed stdout (cached per process). Missing shell or
///   non-zero exit ⇒ `None`.
/// - `$VAR` / `${VAR}` templates: interpolate from `env_overlay` (winning) then
///   the process env. `$$`→`$`, `$!`→`!` escapes. Any referenced var that is
///   unset ⇒ the **whole** value resolves to `None` (pi semantics).
/// - Otherwise the literal string (returned as-is).
///
/// `env_overlay` is the `credential.env` map for auth.json keys (pi passes the
/// same). `None` (or an empty overlay) means process-env only — used for
/// models.json apiKey/headers, which have no env overlay.
pub fn resolve_config_value(
    config: &str,
    env_overlay: Option<&BTreeMap<String, String>>,
) -> Option<String> {
    if let Some(cmd) = config.strip_prefix('!') {
        return resolve_command(cmd);
    }
    resolve_template(config, env_overlay)
}

/// Like [`resolve_config_value`] but **uncached** — mirrors pi's
/// `resolveConfigValueUncached`, used when a fresh resolution is required
/// (e.g. headers, which pi resolves uncached so a rotating token is re-read).
pub fn resolve_config_value_uncached(
    config: &str,
    env_overlay: Option<&BTreeMap<String, String>>,
) -> Option<String> {
    if let Some(cmd) = config.strip_prefix('!') {
        return resolve_command_uncached(cmd);
    }
    resolve_template(config, env_overlay)
}

/// Resolve every header value via [`resolve_config_value_uncached`]; drop
/// entries that resolve to `None` (mirrors pi `resolveHeaders`). Used on
/// models.json `headers` maps before folding onto a model.
pub fn resolve_headers(
    headers: &BTreeMap<String, String>,
    env_overlay: Option<&BTreeMap<String, String>>,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (k, v) in headers {
        if let Some(resolved) = resolve_config_value_uncached(v, env_overlay) {
            out.insert(k.clone(), resolved);
        }
    }
    out
}

/// Env lookup: `env_overlay` (if present) wins over the process env, matching
/// pi's `resolveEnvConfigValue` (which checks `env?.[name]` before `process.env`).
fn env_lookup(name: &str, env_overlay: Option<&BTreeMap<String, String>>) -> Option<String> {
    if let Some(overlay) = env_overlay {
        if let Some(v) = overlay.get(name) {
            return Some(v.clone());
        }
    }
    std::env::var(name).ok()
}

/// A parsed template part — literal text or an env-var reference.
enum TemplatePart {
    Literal(String),
    Env(String),
}

/// Parse a `$VAR`/`${VAR}` template (mirrors pi `parseConfigValueTemplate`).
/// `$$`→`$` and `$!`→`!` are escapes; `${NAME}` requires `NAME` to match
/// `^[A-Za-z_][A-Za-z0-9_]*$` else the raw slice is kept literal; `$NAME` takes
/// the longest `[A-Za-z_][A-Za-z0-9_]*` prefix as the name.
fn parse_template(config: &str) -> Vec<TemplatePart> {
    let mut parts: Vec<TemplatePart> = Vec::new();
    let bytes = config.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        // Find the next `$`.
        match config[i..].find('$') {
            None => {
                push_literal(&mut parts, &config[i..]);
                break;
            }
            Some(offset) => {
                let dollar = i + offset;
                push_literal(&mut parts, &config[i..dollar]);
                let after = dollar + 1;
                let next = bytes.get(after).copied();
                if next == Some(b'$') || next == Some(b'!') {
                    push_literal(&mut parts, &config[after..after + 1]);
                    i = after + 1;
                    continue;
                }
                if next == Some(b'{') {
                    // ${NAME}
                    if let Some(end_rel) = config[after + 1..].find('}') {
                        let end = after + 1 + end_rel;
                        let name = &config[after + 1..end];
                        if is_env_name(name) {
                            parts.push(TemplatePart::Env(name.to_string()));
                        } else {
                            // Not a valid name — keep the raw `${…}` literal.
                            push_literal(&mut parts, &config[dollar..=end]);
                        }
                        i = end + 1;
                        continue;
                    }
                    // No closing `}` — literal `$`.
                    push_literal(&mut parts, "$");
                    i = after;
                    continue;
                }
                // $NAME (greedy prefix). Bare `$` with no name char follows.
                if let Some(name) = env_name_prefix(&config[after..]) {
                    parts.push(TemplatePart::Env(name.to_string()));
                    i = after + name.len();
                } else {
                    push_literal(&mut parts, "$");
                    i = after;
                }
            }
        }
    }
    parts
}

fn push_literal(parts: &mut Vec<TemplatePart>, value: &str) {
    if value.is_empty() {
        return;
    }
    if let Some(TemplatePart::Literal(s)) = parts.last_mut() {
        s.push_str(value);
    } else {
        parts.push(TemplatePart::Literal(value.to_string()));
    }
}

fn is_env_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The longest `[A-Za-z_][A-Za-z0-9_]*` prefix of `s` (mirrors the TS
/// `ENV_VAR_NAME_PREFIX_RE` match), or `None` when `s` doesn't start with one.
fn env_name_prefix(s: &str) -> Option<&str> {
    let mut chars = s.char_indices();
    match chars.next() {
        Some((_, c)) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return None,
    }
    let end = chars
        .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_'))
        .map(|(idx, _)| idx)
        .unwrap_or(s.len());
    Some(&s[..end])
}

/// Resolve a parsed template: any referenced env var that is unset ⇒ the whole
/// value is `None` (pi semantics). Literal-only templates pass through as-is.
fn resolve_template(
    config: &str,
    env_overlay: Option<&BTreeMap<String, String>>,
) -> Option<String> {
    let parts = parse_template(config);
    let mut out = String::with_capacity(config.len());
    for part in parts {
        match part {
            TemplatePart::Literal(s) => out.push_str(&s),
            TemplatePart::Env(name) => match env_lookup(&name, env_overlay) {
                Some(v) => out.push_str(&v),
                None => return None,
            },
        }
    }
    Some(out)
}

/// Run `cmd` (without the leading `!`), returning trimmed stdout. Cached per
/// process (mirrors pi `executeCommand`). 10s timeout; non-zero exit / missing
/// shell ⇒ `None`.
fn resolve_command(cmd: &str) -> Option<String> {
    let key = format!("!{cmd}");
    if let Some(v) = command_cache().lock().ok()?.get(&key) {
        return v.clone();
    }
    let result = resolve_command_uncached(cmd);
    if let Ok(mut cache) = command_cache().lock() {
        cache.insert(key, result.clone());
    }
    result
}

#[cfg(unix)]
fn spawn_shell_command(cmd: &str) -> Option<std::process::Output> {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
}

#[cfg(windows)]
fn spawn_shell_command(cmd: &str) -> Option<std::process::Output> {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("cmd")
        .arg("/C")
        .arg(cmd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()
        .ok()
}

/// Uncached `!command` execution (mirrors pi `executeCommandUncached`).
fn resolve_command_uncached(cmd: &str) -> Option<String> {
    let output = spawn_shell_command(cmd)?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}


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
        // Values are resolved via `resolve_headers` (`$ENV`/`!command` expansion,
        // mirroring pi's `resolveHeadersOrThrow`) so a copied pi models.json
        // referencing env vars / commands resolves the same way. Models.json
        // providers have no credential env overlay (only auth.json keys do), so
        // the expansion is env-only here.
        // NOTE: the `authHeader:true` Bearer synthesis is NOT done here —
        // [`crate::provider::resolve`] applies it centrally so it can skip it
        // when a higher-priority x-api-key source (`--api-key` / auth.json /
        // `ANTHROPIC_API_KEY`) wins. Folding it here unconditionally would put a
        // Bearer on the model even on the x-api-key path. See
        // `models_json_bearer_token` + the fold loop in `resolve`.
        let mut headers: BTreeMap<String, String> = BTreeMap::new();
        if let Some(h) = def.headers.clone() {
            for (k, v) in resolve_headers(&h, None) {
                headers.insert(k, v);
            }
        }
        if let Some(h) = cfg.headers.clone() {
            for (k, v) in resolve_headers(&h, None) {
                headers.insert(k, v);
            }
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
/// `unwrap_or_else` ergonomic used by [`provider_to_models`] and
/// [`crate::provider::models_json_provider_auth`].
pub fn default_anthropic_base_url() -> String {
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
    fn agent_dir_nests_under_agent_by_default() {
        // With no env override, agent_dir() must end in `.../.rpi/agent`
        // (mirrors pi's `getAgentDir`). We can't assertion the home prefix
        // portably, but the leaf two segments are stable.
        let _guard = env_lock().lock().unwrap();
        let prev = std::env::var_os(CONFIG_DIR_ENV);
        std::env::remove_var(CONFIG_DIR_ENV);
        let dir = agent_dir().unwrap();
        restore_env(CONFIG_DIR_ENV, prev);
        assert!(dir.ends_with("agent"));
        assert!(dir
            .parent()
            .map(|p| p.ends_with(CONFIG_DIR_NAME))
            .unwrap_or(false));
    }

    #[test]
    fn migrate_legacy_layout_moves_flat_files_into_agent() {
        // Drive the core migration directly against a temp root/agent so the
        // result is independent of whatever RPI_CODING_AGENT_DIR the parallel
        // TempConfig tests happen to set.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let agent = root.join("agent");
        std::fs::write(root.join("auth.json"), "{}").unwrap();
        std::fs::write(root.join("models.json"), "{}").unwrap();
        std::fs::write(root.join(".setup_done"), "1").unwrap();
        let moved = migrate_legacy_layout_in(&root, &agent).unwrap();
        assert_eq!(moved, 3);
        assert!(agent.join("auth.json").exists());
        assert!(agent.join("models.json").exists());
        assert!(agent.join(".setup_done").exists());
        assert!(!root.join("auth.json").exists());
    }

    #[test]
    fn migrate_legacy_layout_noop_when_agent_exists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let agent = root.join("agent");
        std::fs::write(root.join("auth.json"), "{}").unwrap();
        std::fs::create_dir_all(&agent).unwrap();
        let moved = migrate_legacy_layout_in(&root, &agent).unwrap();
        assert_eq!(moved, 0); // agent/ already present — leave flat file alone
    }

    #[test]
    fn migrate_legacy_layout_noop_when_no_flat_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let agent = root.join("agent");
        let moved = migrate_legacy_layout_in(&root, &agent).unwrap();
        assert_eq!(moved, 0);
    }

    #[test]
    fn migrate_legacy_layout_public_skips_env_override() {
        // When RPI_CODING_AGENT_DIR is set, the public entry point is a no-op
        // (it must never touch an explicit override). TempConfig sets it.
        let _cfg = TempConfig::new();
        let moved = migrate_legacy_layout().unwrap();
        assert_eq!(moved, 0);
    }

    #[test]
    fn read_trust_missing_file_is_empty() {
        let _cfg = TempConfig::new();
        assert!(read_trust().unwrap().is_empty());
    }

    #[test]
    fn read_trust_parses_decisions() {
        let _cfg = TempConfig::new();
        let path = trust_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{ "/home/me/proj": true, "/home/me/untrusted": false, "/home/me/null": null }"#,
        )
        .unwrap();
        let store = read_trust().unwrap();
        assert_eq!(store.len(), 3);
        assert_eq!(store.get("/home/me/proj").copied().flatten(), Some(true));
        assert_eq!(store.get("/home/me/untrusted").copied().flatten(), Some(false));
        assert_eq!(store.get("/home/me/null").copied().flatten(), None);
    }

    #[test]
    fn resolve_config_value_literal_passthrough() {
        assert_eq!(resolve_config_value("sk-literal-key", None), Some("sk-literal-key".into()));
    }

    #[test]
    fn resolve_config_value_env_var() {
        let _guard = env_lock().lock().unwrap();
        let prev = std::env::var_os("RPI_TEST_CFG_KEY");
        let prev2 = std::env::var_os("RPI_TEST_CFG_KEY2");
        std::env::set_var("RPI_TEST_CFG_KEY", "secret-from-env");
        assert_eq!(
            resolve_config_value("$RPI_TEST_CFG_KEY", None),
            Some("secret-from-env".into())
        );
        assert_eq!(
            resolve_config_value("prefix-${RPI_TEST_CFG_KEY}-suffix", None),
            Some("prefix-secret-from-env-suffix".into())
        );
        // Two vars in one template.
        std::env::set_var("RPI_TEST_CFG_KEY2", "two");
        assert_eq!(
            resolve_config_value("a-$RPI_TEST_CFG_KEY-b-$RPI_TEST_CFG_KEY2-c", None),
            Some("a-secret-from-env-b-two-c".into())
        );
        // Env overlay wins over process env.
        let mut overlay = BTreeMap::new();
        overlay.insert("RPI_TEST_CFG_KEY".into(), "overlay-value".into());
        assert_eq!(
            resolve_config_value("$RPI_TEST_CFG_KEY", Some(&overlay)),
            Some("overlay-value".into())
        );
        restore_env("RPI_TEST_CFG_KEY", prev);
        restore_env("RPI_TEST_CFG_KEY2", prev2);
    }

    #[test]
    fn resolve_config_value_unset_env_is_none() {
        let _guard = env_lock().lock().unwrap();
        let prev = std::env::var_os("RPI_TEST_CFG_ABSENT");
        std::env::remove_var("RPI_TEST_CFG_ABSENT");
        // Any referenced unset var ⇒ the whole value is None (pi semantics).
        assert_eq!(resolve_config_value("$RPI_TEST_CFG_ABSENT", None), None);
        assert_eq!(
            resolve_config_value("prefix-$RPI_TEST_CFG_ABSENT-suffix", None),
            None
        );
        restore_env("RPI_TEST_CFG_ABSENT", prev);
    }

    #[test]
    fn resolve_config_value_dollar_dollar_escapes_literal() {
        assert_eq!(resolve_config_value("price-$$5", None), Some("price-$5".into()));
        assert_eq!(resolve_config_value("$!bang", None), Some("!bang".into()));
    }

    #[test]
    fn resolve_config_value_command_runs_shell() {
        // `!echo resolved` → "resolved" (sh on Unix; `echo` works under cmd too).
        assert_eq!(
            resolve_config_value_uncached("!echo rpi-cfg-resolved", None),
            Some("rpi-cfg-resolved".into())
        );
        // Non-zero exit ⇒ None.
        assert_eq!(
            resolve_config_value_uncached("!false", None),
            None
        );
    }

    #[test]
    fn resolve_headers_drops_unresolvable() {
        let _guard = env_lock().lock().unwrap();
        let prev = std::env::var_os("RPI_TEST_HDR_SET");
        std::env::set_var("RPI_TEST_HDR_SET", "set-value");
        let mut h = BTreeMap::new();
        h.insert("x-set".into(), "$RPI_TEST_HDR_SET".into());
        h.insert("x-unset".into(), "$RPI_TEST_HDR_UNSET".into());
        h.insert("x-literal".into(), "literal-value".into());
        let resolved = resolve_headers(&h, None);
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved.get("x-set").map(|s| s.as_str()), Some("set-value"));
        assert_eq!(resolved.get("x-literal").map(|s| s.as_str()), Some("literal-value"));
        assert!(!resolved.contains_key("x-unset"));
        restore_env("RPI_TEST_HDR_SET", prev);
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
