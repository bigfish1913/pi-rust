//! Private process-launch policy, independent of capacity, IPC and Agent loops.
//! Parsing/validation is pure; accepting a policy is not proof of support.

use serde::Deserialize;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum HostRunMode {
    #[default]
    Foreground,
    Daemon,
}

impl<'de> Deserialize<'de> for HostRunMode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            "foreground" => Ok(Self::Foreground),
            "daemon" => Ok(Self::Daemon),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &["foreground", "daemon"],
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct RecoveryPolicy {
    pub restore_on_start: bool,
    pub restart_on_crash: bool,
}

impl<'de> Deserialize<'de> for RecoveryPolicy {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Default, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        struct Fields {
            restore_on_start: bool,
            restart_on_crash: bool,
        }
        struct ObjectOnly;
        impl<'de> serde::de::Visitor<'de> for ObjectOnly {
            type Value = RecoveryPolicy;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a Host recovery policy object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<Self::Value, A::Error> {
                // Preserve the derive's duplicate/unknown field checks. Going
                // through Value would silently erase duplicate JSON keys.
                let fields =
                    Fields::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                Ok(RecoveryPolicy {
                    restore_on_start: fields.restore_on_start,
                    restart_on_crash: fields.restart_on_crash,
                })
            }
        }
        // Serde's derived default struct also accepts [] as all defaults.
        // The private JSON configuration requires an object, not a sequence.
        deserializer.deserialize_map(ObjectOnly)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(super) struct LaunchPolicy {
    pub mode: HostRunMode,
    pub recovery: RecoveryPolicy,
}

impl LaunchPolicy {
    pub fn validate(self) -> Result<(), &'static str> {
        if self.recovery.restart_on_crash && self.mode != HostRunMode::Daemon {
            return Err("shared Host restart_on_crash requires daemon mode");
        }
        Ok(())
    }

    /// Serve-only gate, before profiles, catalog or executable assembly. Queries
    /// may read a valid future policy without trying to launch a background Host.
    pub fn require_implemented(self) -> Result<(), &'static str> {
        self.validate()?;
        if self.mode == HostRunMode::Daemon {
            return Err("shared Host daemon mode is not implemented");
        }
        if self.recovery.restore_on_start {
            return Err("shared Host whole-host startup recovery is not implemented");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn missing_fields_keep_foreground_and_recovery_disabled() {
        for input in [json!({}), json!({"recovery": {}})] {
            let policy: LaunchPolicy = serde_json::from_value(input).unwrap();
            assert_eq!(policy, LaunchPolicy::default());
            assert_eq!(policy.require_implemented(), Ok(()));
        }
    }

    #[test]
    fn all_eight_policy_combinations_have_explicit_validation_and_availability() {
        for mode in [HostRunMode::Foreground, HostRunMode::Daemon] {
            for restore_on_start in [false, true] {
                for restart_on_crash in [false, true] {
                    let policy = LaunchPolicy {
                        mode,
                        recovery: RecoveryPolicy {
                            restore_on_start,
                            restart_on_crash,
                        },
                    };
                    assert_eq!(
                        policy.validate().is_ok(),
                        !restart_on_crash || mode == HostRunMode::Daemon
                    );
                    // A valid policy can still name an unimplemented capability.
                    assert_eq!(
                        policy.require_implemented().is_ok(),
                        policy == LaunchPolicy::default()
                    );
                }
            }
        }
    }

    #[test]
    fn policy_json_is_strict_and_missing_is_not_null_or_string_boolean() {
        for input in [
            json!({"mode": "background"}),
            json!({"mode": "Daemon"}),
            json!({"mode": null}),
            json!({"mode": false}),
            json!({"mode": {"foreground": null}}),
            json!({"mode": {"daemon": null}}),
            json!({"mode": []}),
            json!({"recovery": null}),
            json!({"recovery": []}),
            json!({"recovery": {"restore_on_start": "true"}}),
            json!({"recovery": {"restart_on_crash": null}}),
            json!({"recovery": {"restoreOnStart": true}}),
            json!({"recovery": {"restart_on_crash": false, "retry": true}}),
            json!({"daemon": true}),
            json!({"restore_on_start": true}),
        ] {
            assert!(
                serde_json::from_value::<LaunchPolicy>(input.clone()).is_err(),
                "{input}"
            );
        }
    }

    #[test]
    fn duplicate_policy_fields_are_rejected() {
        for input in [
            r#"{"mode":"daemon","mode":"foreground"}"#,
            r#"{"recovery":{},"recovery":{}}"#,
            r#"{"recovery":{"restore_on_start":false,"restore_on_start":true}}"#,
        ] {
            assert!(serde_json::from_str::<LaunchPolicy>(input).is_err());
        }
    }
}
