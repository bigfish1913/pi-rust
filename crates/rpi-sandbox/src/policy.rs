//! Filesystem sandbox policy: path rules, configuration, and precedence.
//!
//! The policy only describes intent. Enforcement backends (Landlock, Seatbelt,
//! Windows restricted tokens) are applied elsewhere; keeping the model free of
//! platform code makes it unit-testable on every host.
//!
//! Precedence, from strongest to weakest:
//!
//! 1. [`PathRule`] entries in [`SandboxPolicy::protected`] (always read-only);
//! 2. [`PathRule`] entries in [`SandboxPolicy::writable`];
//! 3. everything else is readable but not writable.

use std::path::{Component, Path, PathBuf};

use crate::approval::ApprovalScope;

/// Whether the sandbox is disabled, opportunistic, or mandatory.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SandboxMode {
    /// Never apply the sandbox.
    Off,
    /// Apply when the platform supports it; otherwise warn and continue.
    #[default]
    Auto,
    /// Refuse to start when the platform cannot enforce the sandbox.
    Required,
}

/// `sandbox.approval` settings block.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct ApprovalSettings {
    /// Whether out-of-policy writes may prompt for approval. Absent means
    /// "enabled"; a project settings file may only turn this off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Scope preselected in the approval prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_scope: Option<ApprovalScope>,
}

/// The `sandbox` block of `settings.json`.
///
/// Unknown fields are ignored so a newer host can add keys without breaking an
/// older parser.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct SandboxSettings {
    /// Master switch. Absent means disabled (upstream default: opt-in).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Whether a missing backend is fatal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<SandboxMode>,
    /// Extra writable paths. Project settings may not add entries here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writable: Vec<String>,
    /// Paths that stay read-only even when a writable rule covers them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub protected: Vec<String>,
    /// Individual files that may always be written (for example `/dev/null`).
    #[serde(
        default,
        alias = "allow_file_writes",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub allow_file_writes: Vec<String>,
    /// Approval prompt behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalSettings>,
    /// Where the broker appends denied/approved operations.
    #[serde(default, alias = "audit_log", skip_serializing_if = "Option::is_none")]
    pub audit_log: Option<String>,
}

/// One compiled allow/deny rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathRule {
    /// A directory and everything below it.
    Dir(PathBuf),
    /// Exactly one file.
    File(PathBuf),
}

impl PathRule {
    /// Whether `path` (already normalized) matches this rule.
    pub fn matches(&self, path: &Path) -> bool {
        match self {
            Self::Dir(root) => path == root || path.starts_with(root),
            Self::File(file) => path == file,
        }
    }

    /// The rule's root path.
    pub fn path(&self) -> &Path {
        match self {
            Self::Dir(path) | Self::File(path) => path,
        }
    }
}

/// Result of classifying a path against a policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Covered by a writable rule and not protected.
    Writable,
    /// Covered by a protected rule.
    Protected,
    /// Not covered by any rule: readable, not writable.
    ReadOnly,
}

impl Access {
    /// Whether writes are allowed.
    pub fn is_writable(self) -> bool {
        matches!(self, Self::Writable)
    }
}

/// Inputs used to expand configured paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyContext {
    /// Project working directory; relative entries resolve against it.
    pub cwd: PathBuf,
    /// User home directory, when known.
    pub home: Option<PathBuf>,
    /// System temporary directory.
    pub temp: PathBuf,
}

impl PolicyContext {
    /// Build a context from the current process environment.
    pub fn from_env(cwd: PathBuf) -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let temp = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        Self { cwd, home, temp }
    }
}

/// Explicit command-line overrides. These are user intent and may both widen
/// and disable the policy.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PolicyOverrides {
    /// `--sandbox` / `--no-sandbox`.
    pub enabled: Option<bool>,
    /// `--sandbox-mode`.
    pub mode: Option<SandboxMode>,
    /// `--allow-write <path>`.
    pub writable: Vec<String>,
    /// Reserved for a future `--protect <path>` flag.
    pub protected: Vec<String>,
    /// Reserved for a future `--allow-write-file <path>` flag.
    pub allow_file_writes: Vec<String>,
}

/// Compiled, platform-independent sandbox policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxPolicy {
    /// Whether enforcement should be attempted.
    pub enabled: bool,
    /// Failure behavior when the platform cannot enforce the policy.
    pub mode: SandboxMode,
    /// Rules that allow writes.
    pub writable: Vec<PathRule>,
    /// Rules that force read-only access.
    pub protected: Vec<PathRule>,
    /// Individual files exempt from the default read-only behavior.
    pub allow_file_writes: Vec<PathRule>,
    /// Approval prompt behavior.
    pub approval: ApprovalSettings,
    /// Audit log destination, when configured.
    pub audit_log: Option<PathBuf>,
}

impl SandboxPolicy {
    /// Classify an absolute or cwd-relative path.
    ///
    /// The path is canonicalized when it exists (or when its parent exists),
    /// so a symlinked target is compared by its real location.
    pub fn classify(&self, path: &Path) -> Access {
        let path = normalize_and_resolve(path);
        if self.protected.iter().any(|rule| rule.matches(&path)) {
            return Access::Protected;
        }
        if self.writable.iter().any(|rule| rule.matches(&path))
            || self
                .allow_file_writes
                .iter()
                .any(|rule| rule.matches(&path))
        {
            return Access::Writable;
        }
        Access::ReadOnly
    }

    /// Convenience wrapper around [`Self::classify`].
    pub fn is_writable(&self, path: &Path) -> bool {
        self.classify(path).is_writable()
    }

    /// Protected rules that fall inside a writable directory rule.
    ///
    /// Landlock and Seatbelt only add rights; they cannot subtract a subtree
    /// from an allowed parent. Callers must surface these pairs (warn in
    /// `auto`, fail closed in `required`) until nested protection is enforced
    /// by the in-process layer.
    pub fn nested_protected(&self) -> Vec<(PathBuf, PathBuf)> {
        let mut nested: Vec<(PathBuf, PathBuf)> = Vec::new();
        for protected in &self.protected {
            if nested
                .iter()
                .any(|(existing, _)| existing == protected.path())
            {
                continue;
            }
            for writable in &self.writable {
                if matches!(writable, PathRule::Dir(_)) && writable.matches(protected.path()) {
                    nested.push((
                        protected.path().to_path_buf(),
                        writable.path().to_path_buf(),
                    ));
                    break;
                }
            }
        }
        nested
    }
}

/// Compile global, project, and CLI settings into one policy.
///
/// Merge rules (documented in `docs/architecture/fs-sandbox.md`):
///
/// - `enabled`, `mode`, `writable`, `allowFileWrites`, `auditLog` come from the
///   global settings file and the CLI only; **project settings are ignored**
///   for these fields because a repository is untrusted input.
/// - `protected` is additive across global, project, and CLI sources.
/// - Project settings may turn approvals off (tightening) but may not turn
///   them on.
/// - `cwd` and the system temp directory are always writable; callers pass any
///   additional runtime roots (session directory, export target) in
///   `auto_writable`.
pub fn compile_policy(
    global: Option<&SandboxSettings>,
    project: Option<&SandboxSettings>,
    overrides: &PolicyOverrides,
    context: &PolicyContext,
    auto_writable: &[PathBuf],
) -> SandboxPolicy {
    let enabled = overrides
        .enabled
        .or_else(|| global.and_then(|settings| settings.enabled))
        .unwrap_or(false);
    let mode = overrides
        .mode
        .or_else(|| global.and_then(|settings| settings.mode))
        .unwrap_or_default();

    let mut writable = Vec::new();
    if let Some(settings) = global {
        push_rules(&mut writable, &settings.writable, context);
    }
    push_rules(&mut writable, &overrides.writable, context);
    writable.push(PathRule::Dir(normalize_and_resolve(&context.cwd)));
    writable.push(PathRule::Dir(normalize_and_resolve(&context.temp)));
    for root in auto_writable {
        writable.push(auto_rule(root));
    }

    let mut protected = Vec::new();
    if let Some(settings) = global {
        push_rules(&mut protected, &settings.protected, context);
    }
    if let Some(settings) = project {
        push_rules(&mut protected, &settings.protected, context);
    }
    push_rules(&mut protected, &overrides.protected, context);

    let mut allow_file_writes = Vec::new();
    if let Some(settings) = global {
        push_file_rules(&mut allow_file_writes, &settings.allow_file_writes, context);
    }
    push_file_rules(
        &mut allow_file_writes,
        &overrides.allow_file_writes,
        context,
    );

    let mut approval = global
        .and_then(|settings| settings.approval.clone())
        .unwrap_or_default();
    if project
        .and_then(|settings| settings.approval.as_ref())
        .and_then(|approval| approval.enabled)
        == Some(false)
    {
        // A project may only tighten: disable prompts, never enable them.
        approval.enabled = Some(false);
    }

    let audit_log = global
        .and_then(|settings| settings.audit_log.as_deref())
        .and_then(|value| expand_configured_path(value, context));

    SandboxPolicy {
        enabled,
        mode,
        writable,
        protected,
        allow_file_writes,
        approval,
        audit_log,
    }
}

/// Automatic roots come from runtime bookkeeping (session dir, agent-dir
/// files) and are not user configuration. Existing regular files become
/// single-file rules so Landlock gets a write/truncate mask it accepts.
fn auto_rule(path: &Path) -> PathRule {
    let normalized = normalize_and_resolve(path);
    match std::fs::metadata(&normalized) {
        Ok(metadata) if metadata.is_file() => PathRule::File(normalized),
        _ => PathRule::Dir(normalized),
    }
}

fn push_rules(target: &mut Vec<PathRule>, values: &[String], context: &PolicyContext) {
    for value in values {
        if let Some(path) = expand_configured_path(value, context) {
            target.push(PathRule::Dir(path));
        }
    }
}

fn push_file_rules(target: &mut Vec<PathRule>, values: &[String], context: &PolicyContext) {
    for value in values {
        if let Some(path) = expand_configured_path(value, context) {
            target.push(PathRule::File(path));
        }
    }
}

/// Expand `${cwd}`, `${home}`, `${tmp}`, and a leading `~`, then resolve
/// relative paths against the project directory.
///
/// Returns `None` for empty values and for `${home}` when the home directory is
/// unknown.
pub fn expand_configured_path(value: &str, context: &PolicyContext) -> Option<PathBuf> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let expanded: PathBuf = if let Some(rest) = value.strip_prefix("${cwd}") {
        join_rest(&context.cwd, rest)
    } else if let Some(rest) = value.strip_prefix("${tmp}") {
        join_rest(&context.temp, rest)
    } else if let Some(rest) = value.strip_prefix("${home}") {
        join_rest(context.home.as_deref()?, rest)
    } else if value == "~" {
        context.home.clone()?
    } else if let Some(rest) = value.strip_prefix("~/") {
        join_rest(context.home.as_deref()?, rest)
    } else {
        PathBuf::from(value)
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        context.cwd.join(expanded)
    };
    Some(normalize_and_resolve(&absolute))
}

fn join_rest(base: &Path, rest: &str) -> PathBuf {
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    if rest.is_empty() {
        base.to_path_buf()
    } else {
        base.join(rest)
    }
}

/// Canonicalize when possible, otherwise normalize lexically.
///
/// Write targets usually do not exist yet; resolving the parent directory still
/// resolves symlinked roots, which is what enforcement compares against.
pub fn normalize_and_resolve(path: &Path) -> PathBuf {
    let normalized = lexical_normalize(path);
    if !normalized.is_absolute() {
        return normalized;
    }
    if let Ok(resolved) = std::fs::canonicalize(&normalized) {
        return resolved;
    }
    // The target may not exist yet (a write target, a captured session dir).
    // Resolve the deepest existing ancestor and append the missing components
    // verbatim, so rules and lookups agree even when a parent is a symlink
    // (`/srv` -> `/var/srv` on Fedora, `/tmp` -> `/private/tmp` on macOS).
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut current = normalized.as_path();
    while let Some(parent) = current.parent() {
        if let Some(name) = current.file_name() {
            suffix.push(name.to_os_string());
        }
        if let Ok(base) = std::fs::canonicalize(parent) {
            let mut out = base;
            for component in suffix.iter().rev() {
                out.push(component);
            }
            return lexical_normalize(&out);
        }
        current = parent;
    }
    normalized
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> PolicyContext {
        PolicyContext {
            cwd: PathBuf::from("/work/project"),
            home: Some(PathBuf::from("/home/user")),
            temp: PathBuf::from("/tmp"),
        }
    }

    fn settings(json: &str) -> SandboxSettings {
        serde_json::from_str(json).expect("valid settings")
    }

    #[test]
    fn disabled_by_default() {
        let policy = compile_policy(None, None, &PolicyOverrides::default(), &context(), &[]);
        assert!(!policy.enabled);
        assert_eq!(policy.mode, SandboxMode::Auto);
    }

    #[test]
    fn settings_can_enable() {
        let policy = compile_policy(
            Some(&settings(r#"{"enabled": true}"#)),
            None,
            &PolicyOverrides::default(),
            &context(),
            &[],
        );
        assert!(policy.enabled);
    }

    #[test]
    fn settings_can_disable() {
        let policy = compile_policy(
            Some(&settings(r#"{"enabled": false}"#)),
            None,
            &PolicyOverrides::default(),
            &context(),
            &[],
        );
        assert!(!policy.enabled);
    }

    #[test]
    fn cwd_and_temp_are_always_writable() {
        let policy = compile_policy(
            Some(&settings(r#"{"enabled": true}"#)),
            None,
            &PolicyOverrides::default(),
            &context(),
            &[],
        );
        assert!(policy.is_writable(&PathBuf::from("/work/project/src/main.rs")));
        assert!(policy.is_writable(&PathBuf::from("/tmp/x")));
        assert!(!policy.is_writable(&PathBuf::from("/etc/hosts")));
        assert_eq!(
            policy.classify(&PathBuf::from("/etc/hosts")),
            Access::ReadOnly
        );
    }

    #[test]
    fn file_auto_roots_become_file_rules() {
        let dir = tempfile::tempdir().unwrap();
        let lease = dir.path().join("lease");
        std::fs::write(&lease, b"").unwrap();
        let policy = compile_policy(
            Some(&settings(r#"{"enabled": true}"#)),
            None,
            &PolicyOverrides::default(),
            &context(),
            std::slice::from_ref(&lease),
        );
        let normalized = normalize_and_resolve(&lease);
        assert!(
            policy
                .writable
                .iter()
                .any(|rule| matches!(rule, PathRule::File(path) if path == &normalized)),
            "existing auto roots that are files must become file rules"
        );
    }

    #[test]
    fn dir_rules_are_recursive_and_file_rules_are_exact() {
        let dir = PathRule::Dir(PathBuf::from("/data/cache"));
        assert!(dir.matches(Path::new("/data/cache")));
        assert!(dir.matches(Path::new("/data/cache/a/b")));
        assert!(!dir.matches(Path::new("/data/cache-other")));

        let file = PathRule::File(PathBuf::from("/dev/null"));
        assert!(file.matches(Path::new("/dev/null")));
        assert!(!file.matches(Path::new("/dev/null/child")));
    }

    #[test]
    fn protected_wins_over_writable() {
        let policy = compile_policy(
            Some(&settings(
                r#"{
                    "enabled": true,
                    "writable": ["${cwd}"],
                    "protected": ["${cwd}/.rpi/agent"]
                }"#,
            )),
            None,
            &PolicyOverrides::default(),
            &context(),
            &[],
        );
        assert_eq!(
            policy.classify(&PathBuf::from("/work/project/src/lib.rs")),
            Access::Writable
        );
        assert_eq!(
            policy.classify(&PathBuf::from("/work/project/.rpi/agent/models.json")),
            Access::Protected
        );
    }

    #[test]
    fn project_settings_cannot_add_writable() {
        let policy = compile_policy(
            Some(&settings(r#"{"enabled": true}"#)),
            Some(&settings(r#"{"writable": ["/etc"]}"#)),
            &PolicyOverrides::default(),
            &context(),
            &[],
        );
        assert!(!policy.is_writable(&PathBuf::from("/etc/hosts")));
    }

    #[test]
    fn project_settings_cannot_disable() {
        let policy = compile_policy(
            Some(&settings(r#"{"enabled": true}"#)),
            Some(&settings(r#"{"enabled": false}"#)),
            &PolicyOverrides::default(),
            &context(),
            &[],
        );
        assert!(policy.enabled);
    }

    #[test]
    fn project_settings_can_add_protected() {
        let policy = compile_policy(
            Some(&settings(r#"{"enabled": true}"#)),
            Some(&settings(r#"{"protected": ["${cwd}/secrets"]}"#)),
            &PolicyOverrides::default(),
            &context(),
            &[],
        );
        assert_eq!(
            policy.classify(&PathBuf::from("/work/project/secrets/key")),
            Access::Protected
        );
    }

    #[test]
    fn nested_protection_is_reported() {
        let policy = compile_policy(
            Some(&settings(
                r#"{
                    "enabled": true,
                    "writable": ["${cwd}"],
                    "protected": ["${cwd}/.rpi/agent", "${home}/.ssh"]
                }"#,
            )),
            None,
            &PolicyOverrides::default(),
            &context(),
            &[],
        );
        let nested = policy.nested_protected();
        assert_eq!(nested.len(), 1);
        assert_eq!(nested[0].0, PathBuf::from("/work/project/.rpi/agent"));
    }

    #[test]
    fn command_line_can_disable() {
        let policy = compile_policy(
            Some(&settings(r#"{"enabled": true}"#)),
            None,
            &PolicyOverrides {
                enabled: Some(false),
                ..PolicyOverrides::default()
            },
            &context(),
            &[],
        );
        assert!(!policy.enabled);
    }

    #[test]
    fn command_line_writable_is_additive() {
        let policy = compile_policy(
            Some(&settings(r#"{"enabled": true}"#)),
            None,
            &PolicyOverrides {
                writable: vec!["/srv/data".to_string()],
                ..PolicyOverrides::default()
            },
            &context(),
            &[],
        );
        assert!(policy.is_writable(&PathBuf::from("/srv/data/file")));
    }

    #[test]
    fn expands_placeholders() {
        let ctx = context();
        assert_eq!(
            expand_configured_path("${cwd}/target", &ctx),
            Some(normalize_and_resolve(Path::new("/work/project/target")))
        );
        assert_eq!(
            expand_configured_path("${home}/.ssh", &ctx),
            Some(normalize_and_resolve(Path::new("/home/user/.ssh")))
        );
        assert_eq!(
            expand_configured_path("~/notes", &ctx),
            Some(normalize_and_resolve(Path::new("/home/user/notes")))
        );
        assert_eq!(
            expand_configured_path("${tmp}/rpi", &ctx),
            Some(normalize_and_resolve(Path::new("/tmp/rpi")))
        );
        assert_eq!(expand_configured_path("", &ctx), None);
        assert_eq!(expand_configured_path("   ", &ctx), None);
    }

    #[test]
    fn relative_entries_resolve_against_cwd() {
        let ctx = context();
        assert_eq!(
            expand_configured_path("cache", &ctx),
            Some(normalize_and_resolve(Path::new("/work/project/cache")))
        );
    }

    #[test]
    fn parent_components_are_normalized() {
        let ctx = context();
        assert_eq!(
            expand_configured_path("${cwd}/a/../b", &ctx),
            Some(normalize_and_resolve(Path::new("/work/project/b")))
        );
    }

    #[test]
    fn unknown_home_skips_home_rules() {
        let ctx = PolicyContext {
            cwd: PathBuf::from("/work"),
            home: None,
            temp: PathBuf::from("/tmp"),
        };
        assert_eq!(expand_configured_path("${home}/.ssh", &ctx), None);
        assert_eq!(expand_configured_path("~/x", &ctx), None);
    }

    #[test]
    fn parses_documented_configuration() {
        let parsed = settings(
            r#"{
                "enabled": true,
                "mode": "required",
                "writable": ["${cwd}", "${tmp}"],
                "protected": ["${home}/.ssh"],
                "allowFileWrites": ["/dev/null"],
                "approval": { "enabled": true, "defaultScope": "once" },
                "auditLog": "${home}/.rpi/audit/sandbox.jsonl"
            }"#,
        );
        assert_eq!(parsed.enabled, Some(true));
        assert_eq!(parsed.mode, Some(SandboxMode::Required));
        assert_eq!(parsed.allow_file_writes, vec!["/dev/null".to_string()]);
        assert_eq!(
            parsed.approval.as_ref().unwrap().default_scope,
            Some(ApprovalScope::Once)
        );
        assert_eq!(
            parsed.audit_log.as_deref(),
            Some("${home}/.rpi/audit/sandbox.jsonl")
        );
    }

    #[test]
    fn project_can_only_disable_approvals() {
        let base = settings(r#"{"enabled": true, "approval": {"enabled": true}}"#);
        let tightened = settings(r#"{"approval": {"enabled": false}}"#);
        let policy = compile_policy(
            Some(&base),
            Some(&tightened),
            &PolicyOverrides::default(),
            &context(),
            &[],
        );
        assert_eq!(policy.approval.enabled, Some(false));

        let loosened = compile_policy(
            Some(&settings(r#"{"enabled": true}"#)),
            Some(&settings(r#"{"approval": {"enabled": true}}"#)),
            &PolicyOverrides::default(),
            &context(),
            &[],
        );
        assert_eq!(loosened.approval.enabled, None);
    }

    #[test]
    fn audit_log_is_expanded() {
        let policy = compile_policy(
            Some(&settings(
                r#"{"enabled": true, "auditLog": "${home}/.rpi/audit/sandbox.jsonl"}"#,
            )),
            None,
            &PolicyOverrides::default(),
            &context(),
            &[],
        );
        assert_eq!(
            policy.audit_log,
            Some(normalize_and_resolve(Path::new(
                "/home/user/.rpi/audit/sandbox.jsonl"
            )))
        );
    }
}
