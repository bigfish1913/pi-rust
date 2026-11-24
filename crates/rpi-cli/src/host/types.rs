use super::{HostError, HostResult, TimeoutOverrides};
use serde::{Deserialize, Serialize};
use std::{fmt, path::PathBuf};
use uuid::Uuid;

macro_rules! identity {
    ($name:ident) => {
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }
        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
        impl TryFrom<String> for $name {
            type Error = HostError;
            fn try_from(value: String) -> HostResult<Self> {
                let id = Uuid::parse_str(&value)
                    .map_err(|_| HostError::InvalidInput("invalid identity"))?;
                if id.is_nil() {
                    return Err(HostError::InvalidInput("nil identity"));
                }
                Ok(Self(id))
            }
        }
        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0.to_string()
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}
identity!(AgentId);
identity!(HostEpoch);
identity!(RunId);
identity!(RequestId);
// Stable namespace for trusted administrative requests, distinct from epochs.
identity!(ControlScopeId);
// Opaque nonce for one frozen trusted profile, never a digest of private inputs.
identity!(ProfileVersion);

/// Public identity of immutable trusted inputs. Possessing/constructing this
/// value grants nothing: the Host must resolve the exact pair in its allowlist.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileRef {
    pub agent_id: AgentId,
    pub version: ProfileVersion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    Persistent,
    Mini,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    InProcess,
    ChildProcess,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Registered,
    Starting,
    Idle,
    Running,
    WaitingApproval,
    Closing,
    Unloaded,
    Closed,
    Interrupted,
    Lost,
}

impl AgentState {
    /// Starting reserves a slot; closing retains it until cleanup finishes.
    pub fn is_resident(self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Idle | Self::Running | Self::WaitingApproval | Self::Closing
        )
    }

    pub fn is_executing(self) -> bool {
        matches!(self, Self::Running | Self::WaitingApproval)
    }

    /// Waiting for a model/tool/retry/approval must not look idle to a reaper.
    pub fn is_busy(self) -> bool {
        self.is_resident() && self != Self::Idle
    }

    pub fn validate_for(self, mode: AgentMode) -> HostResult<()> {
        if (mode == AgentMode::Persistent && self == Self::Lost)
            || (mode == AgentMode::Mini
                && matches!(self, Self::Closed | Self::Unloaded | Self::Interrupted))
        {
            return Err(HostError::InvalidInput(
                "state does not match persistence mode",
            ));
        }
        Ok(())
    }

    /// Backend adapters must apply these only after the actual boundary: e.g.
    /// Running -> Idle requires settlement, Closing -> Closed requires cleanup.
    /// This helper does not perform execution, cancellation or persistence.
    pub fn transition(self, next: Self, mode: AgentMode) -> HostResult<Self> {
        self.validate_for(mode)?;
        if mode == AgentMode::Mini && self == Self::Lost {
            return Err(HostError::MiniStateLost);
        }
        next.validate_for(mode)?;
        let allowed = match self {
            Self::Registered => matches!(next, Self::Starting | Self::Closed | Self::Lost),
            // A restored provider-deferred conversation is already waiting for
            // external model work; publishing it as Idle would mislead reapers.
            Self::Starting => {
                matches!(next, Self::Idle | Self::Running | Self::Closed | Self::Lost)
            }
            Self::Idle => matches!(next, Self::Running | Self::Closing),
            Self::Running => matches!(next, Self::Idle | Self::WaitingApproval | Self::Closing),
            Self::WaitingApproval => matches!(next, Self::Running | Self::Idle | Self::Closing),
            Self::Closing => matches!(next, Self::Unloaded | Self::Closed | Self::Lost),
            Self::Unloaded | Self::Interrupted => matches!(next, Self::Starting | Self::Closed),
            Self::Closed => next == Self::Starting,
            Self::Lost => false,
        };
        if !allowed {
            return Err(HostError::InvalidTransition {
                from: self,
                to: next,
            });
        }
        Ok(next)
    }

    /// Classify stale observations after host restart. This neither restores a
    /// Session nor authorizes replay of a tool whose external effects are unknown.
    pub fn after_restart(self, mode: AgentMode) -> HostResult<Self> {
        self.validate_for(mode)?;
        if mode == AgentMode::Mini {
            return Ok(Self::Lost);
        }
        Ok(match self {
            Self::Idle => Self::Unloaded,
            Self::Starting | Self::Running | Self::WaitingApproval | Self::Closing => {
                Self::Interrupted
            }
            other => other,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRef {
    pub id: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRef {
    pub provider: String,
    pub model: String,
}

/// Allowlisted startup metadata, not a raw argv/env bag. No prompts, credentials
/// or tool arguments belong here. Full restore bindings are a backend concern.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchSpec {
    pub cwd: PathBuf,
    pub model: Option<ModelRef>,
    pub tools: Vec<String>,
    pub resource_profile: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSpec {
    pub id: AgentId,
    pub name: String,
    /// Relationship metadata, never an authorization grant.
    pub parent_id: Option<AgentId>,
    pub execution: ExecutionMode,
    pub mode: AgentMode,
    pub session: Option<SessionRef>,
    pub launch: LaunchSpec,
    #[serde(default)]
    pub timeouts: TimeoutOverrides,
}

impl AgentSpec {
    /// Structural validation only; no filesystem access or Session recovery.
    pub fn validate(&self) -> HostResult<()> {
        fn text(value: &str, limit: usize) -> HostResult<()> {
            if value.trim().is_empty() || value.len() > limit || value.contains('\0') {
                return Err(HostError::InvalidInput(
                    "empty, oversized or NUL-containing metadata",
                ));
            }
            Ok(())
        }
        fn path(value: &std::path::Path) -> HostResult<()> {
            let Some(value) = value.to_str().filter(|_| value.is_absolute()) else {
                return Err(HostError::InvalidInput(
                    "paths must be absolute UTF-8 paths",
                ));
            };
            text(value, 4096)
        }
        text(&self.name, 256)?;
        path(&self.launch.cwd)?;
        if self.parent_id == Some(self.id) {
            return Err(HostError::InvalidInput("Agent cannot be its own parent"));
        }
        match (self.mode, &self.session) {
            (AgentMode::Persistent, Some(session)) => {
                text(&session.id, 256)?;
                path(&session.path)?;
            }
            (AgentMode::Mini, None) => {}
            _ => {
                return Err(HostError::InvalidInput(
                    "Session reference does not match persistence mode",
                ))
            }
        }
        if let Some(model) = &self.launch.model {
            text(&model.provider, 256)?;
            text(&model.model, 256)?;
        }
        if self.launch.tools.len() > 256 {
            return Err(HostError::InvalidInput("too many tool names"));
        }
        for tool in &self.launch.tools {
            text(tool, 128)?;
        }
        if let Some(profile) = &self.launch.resource_profile {
            text(profile, 256)?;
        }
        self.timeouts.validate()?;
        Ok(())
    }
}
