use super::*;
use std::{
    num::{NonZeroU64, NonZeroUsize},
    sync::Arc,
};

fn ms(value: u64) -> Option<NonZeroU64> {
    NonZeroU64::new(value)
}
fn limit(value: usize) -> Option<NonZeroUsize> {
    NonZeroUsize::new(value)
}
fn spec(mode: AgentMode) -> AgentSpec {
    let cwd = std::env::temp_dir();
    AgentSpec {
        id: AgentId::new(),
        name: "test-agent".into(),
        parent_id: None,
        execution: ExecutionMode::InProcess,
        mode,
        session: (mode == AgentMode::Persistent).then(|| SessionRef {
            id: "a-session-id-not-necessarily-a-uuid".into(),
            path: cwd.join("test-session.jsonl"),
        }),
        launch: LaunchSpec {
            cwd,
            model: Some(ModelRef {
                provider: "test-provider".into(),
                model: "test-model".into(),
            }),
            tools: vec!["read".into(), "bash".into()],
            resource_profile: Some("test-profile".into()),
        },
        timeouts: TimeoutOverrides::default(),
    }
}

#[test]
fn identities_round_trip_without_enabling_uuid_serde_in_the_workspace() {
    macro_rules! check {
        ($ty:ty) => {{
            let id = <$ty>::new();
            let encoded = serde_json::to_string(&id).unwrap();
            assert_eq!(serde_json::from_str::<$ty>(&encoded).unwrap(), id);
            assert_ne!(id, <$ty>::new());
            assert!(
                serde_json::from_str::<$ty>("\"00000000-0000-0000-0000-000000000000\"").is_err()
            );
        }};
    }
    check!(AgentId);
    check!(HostEpoch);
    check!(RunId);
    check!(RequestId);
}

#[test]
fn bad_identity_errors_do_not_echo_untrusted_input() {
    let error = AgentId::try_from(String::from("private-prompt-or-credential")).unwrap_err();
    assert!(!error.to_string().contains("private"));
}

#[test]
fn ordinary_and_mini_metadata_round_trip() {
    for mode in [AgentMode::Persistent, AgentMode::Mini] {
        let input = spec(mode);
        input.validate().unwrap();
        let encoded = serde_json::to_string(&input).unwrap();
        let restored: AgentSpec = serde_json::from_str(&encoded).unwrap();
        assert_eq!(restored, input);
        restored.validate().unwrap();
    }
}

#[test]
fn persistence_mode_requires_the_right_session_reference() {
    let mut persistent = spec(AgentMode::Persistent);
    persistent.session = None;
    assert!(persistent.validate().is_err());
    let mut mini = spec(AgentMode::Mini);
    mini.session = spec(AgentMode::Persistent).session;
    assert!(mini.validate().is_err());
}

#[test]
fn metadata_has_no_freeform_argv_environment_or_prompt_fields() {
    for key in ["argv", "env", "prompt", "credential"] {
        let mut value = serde_json::to_value(spec(AgentMode::Mini)).unwrap();
        value["launch"][key] = serde_json::json!("not-allowed");
        assert!(serde_json::from_value::<AgentSpec>(value).is_err(), "{key}");
    }
}

#[test]
fn metadata_rejects_relative_paths_self_parent_and_unbounded_fields() {
    let mut input = spec(AgentMode::Persistent);
    input.launch.cwd = "relative".into();
    assert!(input.validate().is_err());
    input = spec(AgentMode::Persistent);
    input.session.as_mut().unwrap().path = "relative.jsonl".into();
    assert!(input.validate().is_err());
    input = spec(AgentMode::Mini);
    input.parent_id = Some(input.id);
    assert!(input.validate().is_err());
    input = spec(AgentMode::Mini);
    input.name = "x".repeat(257);
    assert!(input.validate().is_err());
    input = spec(AgentMode::Mini);
    input.launch.tools = vec!["tool".into(); 257];
    assert!(input.validate().is_err());
    input = spec(AgentMode::Mini);
    input.launch.tools.push("bad\0tool".into());
    assert!(input.validate().is_err());
}

#[test]
fn ordinary_lifecycle_keeps_start_and_cleanup_slots() {
    let mut state = AgentState::Registered;
    for next in [
        AgentState::Starting,
        AgentState::Idle,
        AgentState::Running,
        AgentState::WaitingApproval,
        AgentState::Running,
        AgentState::Idle,
        AgentState::Closing,
        AgentState::Unloaded,
        AgentState::Starting,
        AgentState::Idle,
        AgentState::Closing,
        AgentState::Closed,
    ] {
        state = state.transition(next, AgentMode::Persistent).unwrap();
        assert_eq!(
            state.is_resident(),
            !matches!(state, AgentState::Unloaded | AgentState::Closed)
        );
    }
    assert_eq!(
        state
            .transition(AgentState::Starting, AgentMode::Persistent)
            .unwrap(),
        AgentState::Starting
    );
}

#[test]
fn invalid_lifecycle_jumps_do_not_fake_completion() {
    for (from, to) in [
        (AgentState::Registered, AgentState::Running),
        (AgentState::Running, AgentState::Closed),
        (AgentState::Idle, AgentState::Unloaded),
        (AgentState::Closing, AgentState::Running),
    ] {
        assert!(from.transition(to, AgentMode::Persistent).is_err());
    }
}

#[test]
fn approval_and_cleanup_are_busy_not_idle() {
    assert!(!AgentState::Idle.is_busy());
    for state in [
        AgentState::Starting,
        AgentState::Running,
        AgentState::WaitingApproval,
        AgentState::Closing,
    ] {
        assert!(state.is_busy());
        assert!(state.is_resident());
    }
    assert!(AgentState::WaitingApproval.is_executing());
    assert!(!AgentState::Starting.is_executing());
    assert!(!AgentState::Closing.is_executing());
}

#[test]
fn restart_classifies_observations_without_replaying_work() {
    for state in [
        AgentState::Starting,
        AgentState::Running,
        AgentState::WaitingApproval,
        AgentState::Closing,
    ] {
        assert_eq!(
            state.after_restart(AgentMode::Persistent).unwrap(),
            AgentState::Interrupted
        );
    }
    assert_eq!(
        AgentState::Idle
            .after_restart(AgentMode::Persistent)
            .unwrap(),
        AgentState::Unloaded
    );
    for state in [
        AgentState::Registered,
        AgentState::Unloaded,
        AgentState::Closed,
        AgentState::Interrupted,
    ] {
        assert_eq!(state.after_restart(AgentMode::Persistent).unwrap(), state);
    }
    assert!(AgentState::Lost
        .after_restart(AgentMode::Persistent)
        .is_err());
}

#[test]
fn mini_loss_is_terminal_and_never_session_recovery() {
    for state in [
        AgentState::Registered,
        AgentState::Starting,
        AgentState::Idle,
        AgentState::Running,
        AgentState::WaitingApproval,
        AgentState::Closing,
        AgentState::Lost,
    ] {
        assert_eq!(
            state.after_restart(AgentMode::Mini).unwrap(),
            AgentState::Lost
        );
    }
    assert_eq!(
        AgentState::Lost
            .transition(AgentState::Starting, AgentMode::Mini)
            .unwrap_err(),
        HostError::MiniStateLost
    );
    assert_eq!(
        AgentState::Closing
            .transition(AgentState::Lost, AgentMode::Mini)
            .unwrap(),
        AgentState::Lost
    );
    for state in [
        AgentState::Closed,
        AgentState::Unloaded,
        AgentState::Interrupted,
    ] {
        assert!(state.validate_for(AgentMode::Mini).is_err());
    }
}

#[test]
fn default_configuration_introduces_no_timeouts_or_agent_count_cap() {
    let config = HostConfig::default();
    config.validate().unwrap();
    assert_eq!(config.max_agents, None);
    assert_eq!(config.host_idle_timeout_ms, None);
    let effective = config
        .resolve_timeouts(TimeoutOverrides::default())
        .unwrap();
    for timeout in [
        effective.idle,
        effective.run,
        effective.disconnected,
        effective.shutdown_grace,
    ] {
        assert_eq!(timeout.milliseconds, None);
        assert_eq!(timeout.source, TimeoutSource::HostDefault);
    }
}

#[test]
fn agent_timeout_inheritance_disable_and_override_are_distinct() {
    let config = HostConfig {
        default_timeouts: Timeouts {
            idle_ms: ms(100),
            run_ms: ms(200),
            disconnected_ms: ms(300),
            shutdown_grace_ms: ms(400),
        },
        ..Default::default()
    };
    let effective = config
        .resolve_timeouts(TimeoutOverrides {
            idle: TimeoutOverride::Disabled,
            run: TimeoutOverride::After(ms(150).unwrap()),
            disconnected: TimeoutOverride::Inherit,
            shutdown_grace: TimeoutOverride::After(ms(250).unwrap()),
        })
        .unwrap();
    assert_eq!(effective.idle.milliseconds, None);
    assert_eq!(effective.idle.source, TimeoutSource::AgentOverride);
    assert_eq!(effective.run.milliseconds, ms(150));
    assert_eq!(effective.disconnected.milliseconds, ms(300));
    assert_eq!(effective.disconnected.source, TimeoutSource::HostDefault);
    assert_eq!(effective.shutdown_grace.milliseconds, ms(250));
    let wire = serde_json::to_string(&TimeoutOverrides::default()).unwrap();
    assert!(wire.contains("inherit"));
    let disabled = serde_json::to_string(&TimeoutOverride::Disabled).unwrap();
    assert!(disabled.contains("disabled"));
}

#[test]
fn hard_limits_cannot_be_disabled_or_silently_clamped() {
    let config = HostConfig {
        default_timeouts: Timeouts {
            run_ms: ms(100),
            ..Default::default()
        },
        hard_timeout_limits: Timeouts {
            run_ms: ms(200),
            ..Default::default()
        },
        ..Default::default()
    };
    for value in [
        TimeoutOverride::Disabled,
        TimeoutOverride::After(ms(201).unwrap()),
    ] {
        assert!(config
            .resolve_timeouts(TimeoutOverrides {
                run: value,
                ..Default::default()
            })
            .is_err());
    }
    let resolved = config
        .resolve_timeouts(TimeoutOverrides {
            run: TimeoutOverride::After(ms(200).unwrap()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(resolved.run.milliseconds, ms(200));
    let mut invalid = config;
    invalid.default_timeouts.run_ms = None;
    assert!(invalid.validate().is_err());
}

#[test]
fn unknown_config_zero_and_oversized_durations_are_rejected() {
    for wire in [
        r#"{"max_agents":0}"#,
        r#"{"host_idle_timeout_ms":0}"#,
        r#"{"unknown":1}"#,
    ] {
        assert!(serde_json::from_str::<HostConfig>(wire).is_err());
    }
    assert!(
        serde_json::from_str::<TimeoutOverride>(r#"{"kind":"after","milliseconds":0}"#).is_err()
    );
    let config = HostConfig {
        host_idle_timeout_ms: ms(u64::MAX),
        ..Default::default()
    };
    assert!(config.validate().is_err());
    let mut input = spec(AgentMode::Mini);
    input.timeouts.idle = TimeoutOverride::After(ms(u64::MAX).unwrap());
    assert!(input.validate().is_err());
}

#[test]
fn reservation_is_unique_and_drop_releases_exactly_one_slot() {
    let capacity = Capacity::new(limit(1));
    let id = AgentId::new();
    let permit = capacity.try_reserve(id).unwrap();
    assert_eq!(permit.agent_id(), id);
    assert_eq!(
        capacity.try_reserve(id).unwrap_err(),
        HostError::AlreadyResident
    );
    assert_eq!(
        capacity.try_reserve(AgentId::new()).unwrap_err(),
        HostError::AtCapacity { limit: 1 }
    );
    assert_eq!(capacity.snapshot().unwrap().resident, 1);
    drop(permit);
    assert_eq!(capacity.snapshot().unwrap().resident, 0);
    assert!(capacity.try_reserve(id).is_ok());
}

#[test]
fn lowering_capacity_never_evicts_and_available_saturates() {
    let capacity = Capacity::new(None);
    let first = capacity.try_reserve(AgentId::new()).unwrap();
    let second = capacity.try_reserve(AgentId::new()).unwrap();
    capacity.set_limit(limit(1)).unwrap();
    assert_eq!(
        capacity.snapshot().unwrap(),
        CapacitySnapshot {
            limit: Some(1),
            resident: 2,
            available: Some(0)
        }
    );
    assert!(capacity.try_reserve(AgentId::new()).is_err());
    drop(first);
    assert!(capacity.try_reserve(AgentId::new()).is_err());
    drop(second);
    assert!(capacity.try_reserve(AgentId::new()).is_ok());
    capacity.set_limit(None).unwrap();
    assert_eq!(capacity.snapshot().unwrap().available, None);
}

#[test]
fn unwinding_startup_releases_a_reservation() {
    let capacity = Capacity::new(limit(1));
    let result = std::panic::catch_unwind(|| {
        let _permit = capacity.try_reserve(AgentId::new()).unwrap();
        panic!("simulated backend construction failure");
    });
    assert!(result.is_err());
    assert_eq!(capacity.snapshot().unwrap().resident, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_reservations_never_exceed_the_limit() {
    let capacity = Capacity::new(limit(7));
    let barrier = Arc::new(tokio::sync::Barrier::new(65));
    let mut tasks = Vec::new();
    for _ in 0..64 {
        let capacity = capacity.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            let result = capacity.try_reserve(AgentId::new());
            barrier.wait().await;
            result
        }));
    }
    barrier.wait().await;
    // Completed JoinHandles retain returned permits until we collect them.
    assert_eq!(capacity.snapshot().unwrap().resident, 7);
    let mut permits = Vec::new();
    let mut rejected = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(permit) => permits.push(permit),
            Err(HostError::AtCapacity { limit: 7 }) => rejected += 1,
            other => panic!("unexpected reservation result: {other:?}"),
        }
    }
    assert_eq!(permits.len(), 7);
    assert_eq!(rejected, 57);
    drop(permits);
    assert_eq!(capacity.snapshot().unwrap().resident, 0);
}

#[tokio::test]
async fn cancellation_of_an_unstarted_backend_releases_its_reservation() {
    let capacity = Capacity::new(limit(1));
    let permit = capacity.try_reserve(AgentId::new()).unwrap();
    let task = tokio::spawn(async move {
        let _permit = permit;
        std::future::pending::<()>().await;
    });
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(capacity.snapshot().unwrap().resident, 0);
}
