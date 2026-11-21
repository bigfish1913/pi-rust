use super::{HostError, HostResult};
use serde::{Deserialize, Serialize};
use std::num::{NonZeroU64, NonZeroUsize};

// Explicitly disabled is available for longer-lived Agents. Bound finite values
// so future monotonic deadline arithmetic cannot overflow platform Instants.
const MAX_TIMEOUT_MS: u64 = 365 * 24 * 60 * 60 * 1000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "milliseconds",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum TimeoutOverride {
    #[default]
    Inherit,
    Disabled,
    After(NonZeroU64),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TimeoutOverrides {
    pub idle: TimeoutOverride,
    pub run: TimeoutOverride,
    pub disconnected: TimeoutOverride,
    pub shutdown_grace: TimeoutOverride,
}

impl TimeoutOverrides {
    pub fn validate(self) -> HostResult<()> {
        for value in [self.idle, self.run, self.disconnected, self.shutdown_grace] {
            if let TimeoutOverride::After(value) = value {
                validate_duration(Some(value))?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Timeouts {
    pub idle_ms: Option<NonZeroU64>,
    pub run_ms: Option<NonZeroU64>,
    pub disconnected_ms: Option<NonZeroU64>,
    pub shutdown_grace_ms: Option<NonZeroU64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeoutSource {
    HostDefault,
    AgentOverride,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveTimeout {
    pub milliseconds: Option<NonZeroU64>,
    pub source: TimeoutSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveTimeouts {
    pub idle: EffectiveTimeout,
    pub run: EffectiveTimeout,
    pub disconnected: EffectiveTimeout,
    pub shutdown_grace: EffectiveTimeout,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HostConfig {
    /// Resident backend instances, including starting/closing reservations.
    /// None disables the count cap, not future byte/queue budgets.
    pub max_agents: Option<NonZeroUsize>,
    pub host_idle_timeout_ms: Option<NonZeroU64>,
    pub default_timeouts: Timeouts,
    /// A finite hard limit also prohibits disabling that timeout.
    pub hard_timeout_limits: Timeouts,
}

fn validate_duration(value: Option<NonZeroU64>) -> HostResult<()> {
    if value.is_some_and(|value| value.get() > MAX_TIMEOUT_MS) {
        return Err(HostError::InvalidInput("finite timeout exceeds 365 days"));
    }
    Ok(())
}

fn resolve(
    setting: TimeoutOverride,
    default: Option<NonZeroU64>,
    hard: Option<NonZeroU64>,
) -> HostResult<EffectiveTimeout> {
    validate_duration(default)?;
    validate_duration(hard)?;
    let (milliseconds, source) = match setting {
        TimeoutOverride::Inherit => (default, TimeoutSource::HostDefault),
        TimeoutOverride::Disabled => (None, TimeoutSource::AgentOverride),
        TimeoutOverride::After(value) => (Some(value), TimeoutSource::AgentOverride),
    };
    validate_duration(milliseconds)?;
    if let Some(hard) = hard {
        if milliseconds.is_none_or(|value| value > hard) {
            return Err(HostError::InvalidInput("timeout violates Host hard limit"));
        }
    }
    Ok(EffectiveTimeout {
        milliseconds,
        source,
    })
}

impl HostConfig {
    pub fn validate(&self) -> HostResult<()> {
        validate_duration(self.host_idle_timeout_ms)?;
        self.resolve_unchecked(TimeoutOverrides::default())
            .map(|_| ())
    }

    pub fn resolve_timeouts(&self, overrides: TimeoutOverrides) -> HostResult<EffectiveTimeouts> {
        self.validate()?;
        overrides.validate()?;
        self.resolve_unchecked(overrides)
    }

    fn resolve_unchecked(&self, overrides: TimeoutOverrides) -> HostResult<EffectiveTimeouts> {
        let d = self.default_timeouts;
        let h = self.hard_timeout_limits;
        Ok(EffectiveTimeouts {
            idle: resolve(overrides.idle, d.idle_ms, h.idle_ms)?,
            run: resolve(overrides.run, d.run_ms, h.run_ms)?,
            disconnected: resolve(overrides.disconnected, d.disconnected_ms, h.disconnected_ms)?,
            shutdown_grace: resolve(
                overrides.shutdown_grace,
                d.shutdown_grace_ms,
                h.shutdown_grace_ms,
            )?,
        })
    }
}
