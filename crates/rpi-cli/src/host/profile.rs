//! Explicit native Agent profiles for the foreground daemon.
//!
//! A profile is a private, absolute JSON file. It contains the Agent identity,
//! workspace/session reference, a closed provider configuration (including any
//! inline credential values), the exact tool environment, shell path and the
//! trusted tool allowlist. Nothing falls back to CLI settings, process
//! environment, ambient credentials or plugin discovery.
//!
//! This module only parses and validates. Persistent profiles selected for
//! startup create/reopen a Session; explicit Mini profiles cannot reference one.
//! Dynamically available profiles are frozen configuration, not implicit startup
//! or execution. Parsing Mini does not enable an unwired daemon engine.

use super::{
    bindings::{NativeBindingProfile, NativeToolPolicy},
    ordinary::{BackendError, BackendResult, OrdinaryLimits},
    AgentId, AgentMode, AgentSpec, ExecutionMode, LaunchSpec, ModelRef, SessionRef,
    TimeoutOverrides,
};
use rpi_harness::types::AgentHarnessStreamOptions;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::Duration,
};

/// Absolute profile byte budget. The document can contain provider credentials,
/// so callers must read it through the private-file loader.
pub const MAX_PROFILE_BYTES: u64 = 256 * 1024;
const DEFAULT_MAX_INPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;
const MAX_STREAM_TIMEOUT_MS: u64 = 3_600_000;

/// A validated native Agent input; its name does not imply automatic startup.
pub struct HostStartupAgent {
    pub spec: AgentSpec,
    pub profile: NativeBindingProfile,
    pub limits: OrdinaryLimits,
}

/// One private daemon Agent profile document (startup or dynamic-only).
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentProfileFile {
    agent: ProfileAgent,
    runtime: rpi_runtime::config::ModelsConfig,
    #[serde(default)]
    provider_values: BTreeMap<String, String>,
    tool_environment: BTreeMap<String, String>,
    shell: PathBuf,
    #[serde(default)]
    approved_tools: Vec<String>,
    #[serde(default)]
    thinking: rpi_ai::ThinkingLevel,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    max_input_bytes: Option<usize>,
    /// Exact v1 declaration; instructions/tools alone need no native grant.
    /// Explicit extensions additionally require exact nativeHooks approval.
    #[serde(default)]
    resources: Option<crate::resource_selection::ResourceSelection>,
    #[serde(default)]
    native_hooks: Option<super::NativeHookConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ProfileAgent {
    id: AgentId,
    name: String,
    cwd: PathBuf,
    #[serde(default = "persistent_mode")]
    mode: AgentMode,
    #[serde(default)]
    session: Option<ProfileSession>,
    model: ProfileModel,
    #[serde(default)]
    tools: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ProfileSession {
    id: String,
    path: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ProfileModel {
    provider: String,
    model: String,
}

fn persistent_mode() -> AgentMode {
    AgentMode::Persistent
}

fn bounded_text(value: &str, limit: usize) -> bool {
    !value.is_empty()
        && value.chars().count() <= limit
        && !value.chars().any(char::is_control)
        && value.trim() == value
}

impl AgentProfileFile {
    /// Validate the document and produce the exact AgentSpec/native binding the
    /// daemon will use. Rejections are fixed diagnostics without secret values.
    pub fn into_startup(self) -> BackendResult<HostStartupAgent> {
        if !bounded_text(&self.agent.name, 128) {
            return Err(BackendError::Binding("profile Agent name is invalid"));
        }
        if !self.agent.cwd.is_absolute() {
            return Err(BackendError::Binding("profile workspace must be absolute"));
        }
        let session = match (self.agent.mode, self.agent.session) {
            (AgentMode::Persistent, Some(session)) => {
                if !bounded_text(&session.id, 512) {
                    return Err(BackendError::Binding("profile Session id is invalid"));
                }
                if !session.path.is_absolute() {
                    return Err(BackendError::Binding(
                        "profile Session path must be absolute",
                    ));
                }
                Some(SessionRef {
                    id: session.id,
                    path: session.path,
                })
            }
            (AgentMode::Persistent, None) => {
                return Err(BackendError::Binding(
                    "persistent profile requires a Session",
                ))
            }
            (AgentMode::Mini, Some(_)) => {
                return Err(BackendError::Binding(
                    "mini profile must not reference a Session",
                ))
            }
            (AgentMode::Mini, None) => None,
        };
        if !bounded_text(&self.agent.model.provider, 128)
            || !bounded_text(&self.agent.model.model, 256)
        {
            return Err(BackendError::Binding("profile model reference is invalid"));
        }
        if self.runtime.providers.is_empty() {
            return Err(BackendError::Binding(
                "profile has no runtime provider configuration",
            ));
        }
        let approved: BTreeSet<String> = self.approved_tools.into_iter().collect();
        if self.agent.tools.iter().any(|tool| !approved.contains(tool)) {
            return Err(BackendError::Binding(
                "profile requests a tool that is not preapproved",
            ));
        }
        let max_input_bytes = self.max_input_bytes.unwrap_or(DEFAULT_MAX_INPUT_BYTES);
        if max_input_bytes == 0 || max_input_bytes > MAX_INPUT_BYTES {
            return Err(BackendError::Binding("profile input budget is invalid"));
        }
        let timeout = match self.timeout_ms {
            Some(ms) if ms == 0 || ms > MAX_STREAM_TIMEOUT_MS => {
                return Err(BackendError::Binding("profile stream timeout is invalid"))
            }
            Some(ms) => Some(Duration::from_millis(ms)),
            None => None,
        };
        if self.shell.as_os_str().is_empty() || !self.shell.is_absolute() {
            return Err(BackendError::Binding("profile shell path must be absolute"));
        }
        let instruction_resources = match (self.resources, self.native_hooks) {
            (None, None) => None,
            (Some(selection), hooks) => Some(std::sync::Arc::new(
                super::NativeBindingResources::freeze(selection, hooks)?,
            )),
            (None, Some(_)) => {
                return Err(BackendError::Binding(
                    "native hooks require exact resources",
                ))
            }
        };
        if let Some(resources) = &instruction_resources {
            resources.validate_tools(&self.agent.tools)?;
        }
        let spec = AgentSpec {
            id: self.agent.id,
            name: self.agent.name,
            parent_id: None,
            mode: self.agent.mode,
            execution: ExecutionMode::InProcess,
            session,
            launch: LaunchSpec {
                cwd: self.agent.cwd,
                model: Some(ModelRef {
                    provider: self.agent.model.provider,
                    model: self.agent.model.model,
                }),
                tools: self.agent.tools,
                resource_profile: instruction_resources
                    .as_ref()
                    .map(|r| r.identity().to_owned()),
            },
            timeouts: TimeoutOverrides::default(),
        };
        let profile = NativeBindingProfile {
            runtime: rpi_runtime::RuntimeConfig {
                models: self.runtime,
                credentials: Default::default(),
                settings: Default::default(),
            },
            provider_values: self.provider_values,
            tool_environment: self.tool_environment,
            shell_path: self.shell,
            tools: NativeToolPolicy::Preapproved(approved),
            thinking: self.thinking,
            stream_options: AgentHarnessStreamOptions {
                timeout,
                ..Default::default()
            },
            instruction_resources,
        };
        Ok(HostStartupAgent {
            spec,
            profile,
            limits: OrdinaryLimits { max_input_bytes },
        })
    }
}

/// Canonical workspace and Session paths used when creating a new Session.
/// Existing Session files are left for the ordinary strict restore to verify.
pub fn canonical_workspace(spec: &AgentSpec) -> BackendResult<PathBuf> {
    std::fs::canonicalize(&spec.launch.cwd)
        .map_err(|_| BackendError::Binding("profile workspace is unavailable"))
}

pub fn session_parent(path: &Path) -> BackendResult<PathBuf> {
    let parent = path
        .parent()
        .ok_or(BackendError::Binding("profile Session path has no parent"))?;
    if !parent.is_dir() {
        return Err(BackendError::Binding(
            "profile Session parent directory is unavailable",
        ));
    }
    Ok(parent.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base(workspace: &Path) -> serde_json::Value {
        json!({
            "agent": {
                "id": "11111111-1111-4111-8111-111111111111",
                "name": "profile-agent",
                "cwd": workspace.to_string_lossy(),
                "session": {"id": "profile-session", "path": workspace.join("s.jsonl").to_string_lossy()},
                "model": {"provider": "fixture", "model": "host-fixture"},
                "tools": ["read"]
            },
            "runtime": {"providers": {"fixture": {
                "api": "openai-completions",
                "baseUrl": "http://127.0.0.1:9/v1",
                "apiKey": "$PROFILE_KEY",
                "models": [{"id": "host-fixture", "contextWindow": 32768, "maxTokens": 1024}]
            }}},
            "providerValues": {"PROFILE_KEY": "private-key"},
            "toolEnvironment": {"HOME": workspace.to_string_lossy(), "PATH": "/usr/bin:/bin"},
            "shell": "/bin/sh",
            "approvedTools": ["read"],
            "thinking": "off",
            "timeoutMs": 1000
        })
    }

    fn parse(value: serde_json::Value) -> Result<HostStartupAgent, BackendError> {
        serde_json::from_value::<AgentProfileFile>(value)
            .map_err(|_| BackendError::Binding("invalid Agent profile"))
            .and_then(AgentProfileFile::into_startup)
    }

    #[test]
    fn explicit_profile_builds_a_closed_ordinary_spec() {
        let workspace = tempfile::tempdir().unwrap();
        let startup = parse(base(workspace.path())).unwrap();
        assert_eq!(startup.spec.name, "profile-agent");
        assert_eq!(startup.spec.mode, AgentMode::Persistent);
        assert_eq!(startup.spec.execution, ExecutionMode::InProcess);
        assert_eq!(startup.spec.launch.tools, vec!["read".to_string()]);
        assert_eq!(
            startup.spec.launch.model.as_ref().unwrap().provider,
            "fixture"
        );
        assert_eq!(startup.limits.max_input_bytes, DEFAULT_MAX_INPUT_BYTES);
        assert_eq!(
            startup.profile.stream_options.timeout,
            Some(Duration::from_millis(1000))
        );
        assert!(matches!(
            startup.profile.tools,
            NativeToolPolicy::Preapproved(ref tools) if tools.contains("read")
        ));
    }

    #[test]
    fn explicit_modes_enforce_session_identity_without_creating_files() {
        let workspace = tempfile::tempdir().unwrap();
        let mut value = base(workspace.path());
        value["agent"]["mode"] = json!("persistent");
        assert!(parse(value.clone()).unwrap().spec.session.is_some());
        value["agent"].as_object_mut().unwrap().remove("session");
        assert!(parse(value.clone()).is_err());
        value["agent"].as_object_mut().unwrap().remove("mode");
        assert!(
            parse(value.clone()).is_err(),
            "legacy profiles still require a Session"
        );
        value["agent"]["mode"] = json!("mini");
        let mini = parse(value.clone()).unwrap();
        assert_eq!(mini.spec.mode, AgentMode::Mini);
        assert!(mini.spec.session.is_none());
        mini.spec.validate().unwrap();
        value["agent"]["session"] = serde_json::Value::Null;
        assert!(parse(value.clone()).unwrap().spec.session.is_none());
        value["agent"]["session"] = base(workspace.path())["agent"]["session"].clone();
        assert!(
            parse(value.clone()).is_err(),
            "mini must not ignore a Session reference"
        );
        for mode in [json!("unknown"), json!("Mini"), json!(null), json!(true)] {
            value["agent"]["mode"] = mode;
            assert!(parse(value.clone()).is_err());
        }
        assert_eq!(std::fs::read_dir(workspace.path()).unwrap().count(), 0);
    }

    #[test]
    fn profile_rejects_relative_paths_unapproved_tools_and_unknown_fields() {
        let workspace = tempfile::tempdir().unwrap();
        for (mutate, expected) in [
            (
                Box::new(|value: &mut serde_json::Value| {
                    value["agent"]["cwd"] = json!("relative/workspace");
                }) as Box<dyn Fn(&mut serde_json::Value)>,
                "workspace must be absolute",
            ),
            (
                Box::new(|value: &mut serde_json::Value| {
                    value["agent"]["session"]["path"] = json!("relative.jsonl");
                }),
                "Session path must be absolute",
            ),
            (
                Box::new(|value: &mut serde_json::Value| {
                    value["agent"]["tools"] = json!(["bash"]);
                }),
                "not preapproved",
            ),
            (
                Box::new(|value: &mut serde_json::Value| {
                    value["approvedTools"] = json!([]);
                }),
                "not preapproved",
            ),
            (
                Box::new(|value: &mut serde_json::Value| {
                    value["shell"] = json!("sh");
                }),
                "shell path must be absolute",
            ),
            (
                Box::new(|value: &mut serde_json::Value| {
                    value["timeoutMs"] = json!(0);
                }),
                "stream timeout is invalid",
            ),
            (
                Box::new(|value: &mut serde_json::Value| {
                    value["maxInputBytes"] = json!(0);
                }),
                "input budget is invalid",
            ),
            (
                Box::new(|value: &mut serde_json::Value| {
                    value["runtime"] = json!({"providers": {}});
                }),
                "no runtime provider configuration",
            ),
        ] {
            let mut value = base(workspace.path());
            mutate(&mut value);
            let error = parse(value).err().expect("profile must be rejected");
            assert!(error.to_string().contains(expected), "{error}");
        }
        let mut unknown = base(workspace.path());
        unknown["ambientSettings"] = json!(true);
        assert!(serde_json::from_value::<AgentProfileFile>(unknown).is_err());
    }

    #[test]
    fn instruction_profiles_freeze_content_and_reject_unsupported_authority() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("instruction.md");
        std::fs::write(&path, "frozen {{placeholder}}").unwrap();
        let mut value = base(workspace.path());
        value["resources"] =
            json!({"version":1,"tools":{"mode":"only","names":["read"]},"instructions":[path]});
        let startup = parse(value.clone()).unwrap();
        let frozen = startup.spec.launch.resource_profile.clone().unwrap();
        assert_eq!(
            frozen,
            startup
                .profile
                .instruction_resources
                .as_ref()
                .unwrap()
                .identity()
        );
        std::fs::write(&path, "replacement").unwrap();
        assert_ne!(
            frozen,
            parse(value.clone())
                .unwrap()
                .spec
                .launch
                .resource_profile
                .unwrap()
        );
        // Existing owned snapshots never re-read a changed source file.
        assert_eq!(
            frozen,
            startup
                .profile
                .instruction_resources
                .as_ref()
                .unwrap()
                .identity()
        );
        for resources in [
            json!({"version":2,"tools":{"mode":"only","names":["read"]}}),
            json!({"version":1,"tools":{"mode":"default"}}),
            json!({"version":1,"tools":{"mode":"none"}}),
            json!({"version":1,"tools":{"mode":"only","names":["read","read"]}}),
            json!({"version":1,"tools":{"mode":"only","names":["bash"]}}),
            json!({"version":1,"tools":{"mode":"only","names":["read"]},"instructions":["relative.md"]}),
            json!({"version":1,"tools":{"mode":"only","names":["read"]},"instructions":[path,path]}),
            json!({"version":1,"tools":{"mode":"only","names":["read"]},"extensions":[path]}),
            json!({"version":1,"tools":{"mode":"only","names":["read"]},"skills":[workspace.path()]}),
            json!({"version":1,"tools":{"mode":"only","names":["read"]},"main_package":{"name":"ambient"}}),
        ] {
            value["resources"] = resources;
            assert!(
                parse(value.clone()).is_err(),
                "unsupported resource accepted"
            );
        }
        value["resources"] =
            json!({"version":1,"tools":{"mode":"only","names":["read"]},"instructions":[path]});
        std::fs::write(&path, vec![0xff]).unwrap();
        assert!(parse(value.clone()).is_err());
        std::fs::write(&path, "x".repeat(64 * 1024 + 1)).unwrap();
        assert!(parse(value).is_err());
        assert!(!workspace.path().join("s.jsonl").exists());
    }

    #[test]
    fn profile_requires_an_absolute_home_in_the_closed_tool_environment() {
        let workspace = tempfile::tempdir().unwrap();
        let mut value = base(workspace.path());
        value["toolEnvironment"] = json!({"PATH": "/usr/bin:/bin"});
        // Parsing succeeds; bind is what enforces the explicit HOME baseline.
        let startup = parse(value).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let error = runtime
            .block_on(startup.profile.bind(startup.spec))
            .err()
            .expect("missing HOME must be rejected");
        assert!(error.to_string().contains("tool environment"), "{error}");
    }

    #[test]
    fn session_parent_and_workspace_checks_are_explicit() {
        let workspace = tempfile::tempdir().unwrap();
        let startup = parse(base(workspace.path())).unwrap();
        assert_eq!(
            canonical_workspace(&startup.spec).unwrap(),
            workspace.path().canonicalize().unwrap()
        );
        assert_eq!(
            session_parent(&startup.spec.session.as_ref().unwrap().path).unwrap(),
            workspace.path()
        );
        let mut missing = startup.spec.clone();
        missing.launch.cwd = workspace.path().join("absent");
        assert!(canonical_workspace(&missing).is_err());
    }
}
