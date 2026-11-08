use super::*;
use serde_json::{json, Value};

fn tool(index: u32) -> ToolCheckpoint {
    ToolCheckpoint {
        invocation_id: format!("native-start-{index}"),
        assistant_entry_id: "assistant-tools".into(),
        tool_index: index,
        call_id: format!("call-{index}"),
        name: "工具.execute".into(),
        reserved_result_entry_id: format!("result-{index}"),
        result_entry_id: None,
        status: ToolStatus::Unknown,
        replay: ReplayPolicy::Never,
    }
}

fn run(status: RunStatus) -> RunCheckpoint {
    let open = status == RunStatus::Open;
    RunCheckpoint {
        id: "运行/一".into(),
        started_seq: 1,
        finished_record_id: (!open).then(|| "native-finished".into()),
        finished_seq: (!open).then_some(2),
        status,
        final_entry_id: None,
        assistant_entry_id: None,
        tools: vec![tool(0)],
    }
}

fn request(run_id: &str) -> RecoveryRequest {
    RecoveryRequest {
        run_id: run_id.into(),
        started_seq: 1,
        observed_history_revision: 2,
        unknown_invocation_ids: vec!["native-start-0".into()],
    }
}

fn assert_invalid<T: std::fmt::Debug>(result: Result<T>) {
    let error = result.unwrap_err();
    assert!(
        matches!(error, ExecutionError::InvalidFacts(_)),
        "{error:?}"
    );
    assert!(!error.to_string().is_empty());
}

#[test]
fn recovery_transition_matrix_requires_explicit_native_interruption() {
    for status in [
        RunStatus::Open,
        RunStatus::Interrupted,
        RunStatus::Completed,
        RunStatus::Aborted,
        RunStatus::Failed,
        RunStatus::Declined,
    ] {
        let selected = run(status.clone());
        selected.validate().unwrap();
        let result = ExecutionControl::default().apply(&ControlAction::Recover, Some(&selected), 2);
        if status == RunStatus::Interrupted {
            assert_eq!(
                result.unwrap().recovery_request,
                Some(request(&selected.id))
            );
        } else {
            assert_eq!(result, Err(ExecutionError::NotInterrupted), "{status:?}");
        }
    }
}

#[test]
fn forget_transition_matrix_hides_any_valid_status_without_changing_facts() {
    for status in [
        RunStatus::Open,
        RunStatus::Interrupted,
        RunStatus::Completed,
        RunStatus::Aborted,
        RunStatus::Failed,
        RunStatus::Declined,
    ] {
        let selected = run(status);
        let facts_before = selected.clone();
        let source = ExecutionControl::default();
        let candidate = source
            .apply(&ControlAction::ForgetRun, Some(&selected), 0)
            .unwrap();
        assert_eq!(candidate.forgotten_run_ids, vec![selected.id.clone()]);
        assert_eq!(candidate.recovery_request, None);
        assert_eq!(source, ExecutionControl::default());
        assert_eq!(selected, facts_before);
        candidate.validate().unwrap();
    }
}

#[test]
fn all_nonexecution_actions_are_typed_unsupported() {
    for value in [
        json!({"action":"status"}),
        json!({"action":"queue"}),
        json!({"action":"start","name":"x","steps":[]}),
        json!({"action":"checkpoint","step":"x"}),
        json!({"action":"pause"}),
        json!({"action":"resume"}),
        json!({"action":"complete"}),
        json!({"action":"fail"}),
        json!({"action":"reset"}),
        json!({"action":"enqueue","text":"x"}),
        json!({"action":"claim","item":"x"}),
        json!({"action":"finish","item":"x","status":"completed"}),
        json!({"action":"cancel","item":"x"}),
    ] {
        let action: ControlAction = serde_json::from_value(value).unwrap();
        assert_eq!(
            ExecutionControl::default().apply(&action, None, 0),
            Err(ExecutionError::UnsupportedAction)
        );
    }
}

#[test]
fn explicit_run_is_required_even_when_a_request_exists() {
    let source = ExecutionControl {
        recovery_request: Some(request("previous")),
        ..ExecutionControl::default()
    };
    for action in [ControlAction::Recover, ControlAction::ForgetRun] {
        assert_eq!(source.apply(&action, None, 10), Err(ExecutionError::NoRun));
    }
}

#[test]
fn hidden_run_cannot_be_recovered_or_forgotten_again() {
    let selected = run(RunStatus::Interrupted);
    let source = ExecutionControl {
        forgotten_run_ids: vec![selected.id.clone()],
        recovery_request: Some(request("another-run")),
    };
    source.validate().unwrap();
    let before = source.clone();
    for action in [ControlAction::Recover, ControlAction::ForgetRun] {
        assert_eq!(
            source.apply(&action, Some(&selected), 2),
            Err(ExecutionError::NoRun)
        );
        assert_eq!(source, before);
    }
}

#[test]
fn recovery_keeps_unknowns_in_native_order_regardless_of_replay_or_synthetic_results() {
    let mut selected = run(RunStatus::Interrupted);
    selected.tools = (0..5).map(tool).collect();
    selected.tools[0].replay = ReplayPolicy::Safe;
    selected.tools[0].result_entry_id = Some(selected.tools[0].reserved_result_entry_id.clone());
    selected.tools[1].status = ToolStatus::Completed;
    selected.tools[1].result_entry_id = Some(selected.tools[1].reserved_result_entry_id.clone());
    selected.tools[2].status = ToolStatus::Failed;
    selected.tools[2].result_entry_id = Some(selected.tools[2].reserved_result_entry_id.clone());
    selected.tools[4].replay = ReplayPolicy::Safe;
    let before = selected.clone();
    let source = ExecutionControl {
        forgotten_run_ids: vec!["older-hidden-run".into()],
        recovery_request: None,
    };
    let candidate = source
        .apply(&ControlAction::Recover, Some(&selected), 10)
        .unwrap();
    let recovery = candidate.recovery_request.unwrap();
    assert_eq!(
        recovery.unknown_invocation_ids,
        ["native-start-0", "native-start-3", "native-start-4"]
    );
    assert_eq!(recovery.observed_history_revision, 10);
    assert_eq!(recovery.started_seq, 1);
    assert_eq!(candidate.forgotten_run_ids, source.forgotten_run_ids);
    assert_eq!(source.recovery_request, None);
    assert_eq!(selected, before);
}

#[test]
fn repeated_recover_never_creates_a_fake_run() {
    let selected = run(RunStatus::Interrupted);
    let source = ExecutionControl::default()
        .apply(&ControlAction::Recover, Some(&selected), 2)
        .unwrap();
    let before = source.clone();
    assert_eq!(
        source.apply(&ControlAction::Recover, Some(&selected), 50),
        Err(ExecutionError::AlreadyRequested)
    );
    assert_eq!(source, before);
}

#[test]
fn explicit_new_interrupted_run_can_replace_previous_recovery_request() {
    let selected = run(RunStatus::Interrupted);
    let source = ExecutionControl {
        forgotten_run_ids: vec!["old-hidden".into()],
        recovery_request: Some(request("old-request")),
    };
    let before = source.clone();
    let candidate = source
        .apply(&ControlAction::Recover, Some(&selected), 9)
        .unwrap();
    assert_eq!(
        candidate.recovery_request.as_ref().unwrap().run_id,
        selected.id
    );
    assert_eq!(candidate.forgotten_run_ids, before.forgotten_run_ids);
    assert_eq!(source, before);
}

#[test]
fn forget_clears_only_recovery_for_the_selected_run() {
    let selected = run(RunStatus::Interrupted);
    for target in [selected.id.as_str(), "another-run"] {
        let source = ExecutionControl {
            forgotten_run_ids: vec!["previously-hidden".into()],
            recovery_request: Some(request(target)),
        };
        let before = source.clone();
        let candidate = source
            .apply(&ControlAction::ForgetRun, Some(&selected), 0)
            .unwrap();
        assert_eq!(
            candidate.forgotten_run_ids,
            vec!["previously-hidden".to_string(), selected.id.clone()]
        );
        if target == selected.id {
            assert_eq!(candidate.recovery_request, None);
        } else {
            assert_eq!(candidate.recovery_request, source.recovery_request);
        }
        assert_eq!(source, before);
    }
}

#[test]
fn recovery_requires_observation_at_or_after_native_finish() {
    let mut selected = run(RunStatus::Interrupted);
    selected.started_seq = 5;
    selected.finished_seq = Some(8);
    for revision in [0, 4, 5, 7] {
        assert_invalid(ExecutionControl::default().apply(
            &ControlAction::Recover,
            Some(&selected),
            revision,
        ));
    }
    for revision in [8, 9, u64::MAX] {
        ExecutionControl::default()
            .apply(&ControlAction::Recover, Some(&selected), revision)
            .unwrap()
            .validate()
            .unwrap();
    }
}

#[test]
fn sequence_minimums_and_maximums_do_not_overflow() {
    let mut selected = run(RunStatus::Interrupted);
    selected.started_seq = 0;
    assert_invalid(selected.validate());
    selected.started_seq = u64::MAX - 1;
    selected.finished_seq = Some(u64::MAX);
    selected.validate().unwrap();
    ExecutionControl::default()
        .apply(&ControlAction::Recover, Some(&selected), u64::MAX)
        .unwrap();
    selected.started_seq = u64::MAX;
    assert_invalid(selected.validate());
    let mut open = run(RunStatus::Open);
    open.started_seq = u64::MAX;
    open.validate().unwrap();
    let mut recovery = request("run");
    recovery.started_seq = u64::MAX;
    recovery.observed_history_revision = u64::MAX;
    recovery.validate().unwrap();
    recovery.observed_history_revision = u64::MAX - 1;
    assert_invalid(recovery.validate());
    recovery.started_seq = 0;
    assert_invalid(recovery.validate());
}

#[test]
fn finished_record_sequence_and_status_must_agree() {
    for status in [
        RunStatus::Open,
        RunStatus::Interrupted,
        RunStatus::Completed,
        RunStatus::Aborted,
        RunStatus::Failed,
        RunStatus::Declined,
    ] {
        for record in [None, Some("finish".to_string())] {
            for seq in [None, Some(0), Some(1), Some(2)] {
                let mut selected = run(status.clone());
                selected.finished_record_id = record.clone();
                selected.finished_seq = seq;
                let valid = if status == RunStatus::Open {
                    record.is_none() && seq.is_none()
                } else {
                    record.is_some() && seq == Some(2)
                };
                assert_eq!(selected.validate().is_ok(), valid, "{selected:?}");
            }
        }
    }
}

#[test]
fn open_runs_cannot_have_terminal_references_but_legacy_terminals_may_omit_them() {
    for final_ref in [None, Some("final".to_string())] {
        for assistant_ref in [None, Some("assistant".to_string())] {
            let mut open = run(RunStatus::Open);
            open.final_entry_id = final_ref.clone();
            open.assistant_entry_id = assistant_ref.clone();
            assert_eq!(
                open.validate().is_ok(),
                final_ref.is_none() && assistant_ref.is_none()
            );
            for status in [
                RunStatus::Interrupted,
                RunStatus::Completed,
                RunStatus::Aborted,
                RunStatus::Failed,
                RunStatus::Declined,
            ] {
                let mut terminal = run(status);
                terminal.final_entry_id = final_ref.clone();
                terminal.assistant_entry_id = assistant_ref.clone();
                terminal.validate().unwrap();
            }
        }
    }
}

#[test]
fn tool_outcome_requires_matching_reserved_result_proof() {
    for status in [
        ToolStatus::Unknown,
        ToolStatus::Completed,
        ToolStatus::Failed,
    ] {
        for result in [
            None,
            Some("result-0".to_string()),
            Some("forged-result".to_string()),
        ] {
            let mut selected = run(RunStatus::Interrupted);
            selected.tools[0].status = status.clone();
            selected.tools[0].result_entry_id = result.clone();
            let valid = match result.as_deref() {
                None => status == ToolStatus::Unknown,
                Some("result-0") => true,
                Some(_) => false,
            };
            assert_eq!(selected.validate().is_ok(), valid, "{selected:?}");
        }
    }
}

#[test]
fn each_native_tool_proof_key_is_unique() {
    for duplicate in 0..3 {
        let mut selected = run(RunStatus::Interrupted);
        selected.tools.push(tool(1));
        match duplicate {
            0 => selected.tools[1].invocation_id = selected.tools[0].invocation_id.clone(),
            1 => selected.tools[1].tool_index = selected.tools[0].tool_index,
            2 => {
                selected.tools[1].reserved_result_entry_id =
                    selected.tools[0].reserved_result_entry_id.clone()
            }
            _ => unreachable!(),
        }
        assert_invalid(selected.validate());
    }
}

#[test]
fn tool_positions_are_scoped_to_assistant_and_call_ids_are_not_invocation_ids() {
    let mut selected = run(RunStatus::Interrupted);
    selected.tools.push(tool(1));
    selected.tools[1].tool_index = selected.tools[0].tool_index;
    selected.tools[1].assistant_entry_id = "different-assistant".into();
    selected.tools[1].call_id = selected.tools[0].call_id.clone();
    selected.validate().unwrap();
    selected.tools[1].tool_index = u32::MAX;
    selected.validate().unwrap();
}

#[test]
fn tool_count_and_recovery_count_are_bounded_but_may_be_empty() {
    for count in [0, MAX_TOOLS, MAX_TOOLS + 1] {
        let mut selected = run(RunStatus::Interrupted);
        selected.tools = (0..count as u32).map(tool).collect();
        let recovery = RecoveryRequest {
            unknown_invocation_ids: selected
                .tools
                .iter()
                .map(|tool| tool.invocation_id.clone())
                .collect(),
            ..request("run")
        };
        assert_eq!(selected.validate().is_ok(), count <= MAX_TOOLS);
        assert_eq!(recovery.validate().is_ok(), count <= MAX_TOOLS);
        if count <= MAX_TOOLS {
            let candidate = ExecutionControl::default()
                .apply(&ControlAction::Recover, Some(&selected), 2)
                .unwrap();
            assert_eq!(
                candidate
                    .recovery_request
                    .unwrap()
                    .unknown_invocation_ids
                    .len(),
                count
            );
        }
    }
}

#[test]
fn identifiers_accept_native_unicode_at_scalar_limit_not_just_ascii_or_bytes() {
    let mut selected = run(RunStatus::Interrupted);
    selected.id = "🦀".repeat(512);
    assert_eq!(selected.id.len(), 2048);
    selected.validate().unwrap();
    selected.id.push('界');
    assert_invalid(selected.validate());
    for id in [
        "native:id/路径#1",
        "é",
        "e\u{301}",
        "contains internal space",
    ] {
        selected.id = id.into();
        selected.validate().unwrap();
    }
}

#[test]
fn every_identifier_field_rejects_blank_untrimmed_control_and_oversized_values() {
    let mut values = vec![
        "".to_string(),
        " ".into(),
        "\u{2003}".into(),
        " leading".into(),
        "trailing ".into(),
        "a\nb".into(),
        "a\0b".into(),
        "a\u{7f}b".into(),
        "a\u{85}b".into(),
    ];
    values.push("界".repeat(513));
    for value in values {
        for field in ["id", "finishedRecordId", "finalEntryId", "assistantEntryId"] {
            let mut encoded = serde_json::to_value(run(RunStatus::Interrupted)).unwrap();
            encoded[field] = json!(value);
            let selected: RunCheckpoint = serde_json::from_value(encoded).unwrap();
            assert_invalid(selected.validate());
        }
        for field in [
            "invocationId",
            "assistantEntryId",
            "callId",
            "name",
            "reservedResultEntryId",
            "resultEntryId",
        ] {
            let mut encoded = serde_json::to_value(run(RunStatus::Interrupted)).unwrap();
            encoded["tools"][0][field] = json!(value);
            let selected: RunCheckpoint = serde_json::from_value(encoded).unwrap();
            assert_invalid(selected.validate());
        }
        let control = ExecutionControl {
            forgotten_run_ids: vec![value.clone()],
            recovery_request: None,
        };
        assert_invalid(control.validate());
        let mut recovery = request("run");
        recovery.run_id = value.clone();
        assert_invalid(recovery.validate());
        recovery.run_id = "run".into();
        recovery.unknown_invocation_ids = vec![value];
        assert_invalid(recovery.validate());
    }
}

#[test]
fn recovery_request_rejects_duplicates_and_invalid_revisions() {
    let mut recovery = request("run");
    recovery
        .unknown_invocation_ids
        .push(recovery.unknown_invocation_ids[0].clone());
    assert_invalid(recovery.validate());
    recovery.unknown_invocation_ids.clear();
    recovery.validate().unwrap();
    recovery.started_seq = 3;
    recovery.observed_history_revision = 2;
    assert_invalid(recovery.validate());
    recovery.started_seq = 0;
    assert_invalid(recovery.validate());
}

#[test]
fn execution_control_rejects_recovery_targeting_hidden_run() {
    let source = ExecutionControl {
        forgotten_run_ids: vec!["run".into()],
        recovery_request: Some(request("run")),
    };
    assert_invalid(source.validate());
    let selected = run(RunStatus::Interrupted);
    for action in [ControlAction::Recover, ControlAction::ForgetRun] {
        assert_invalid(source.apply(&action, Some(&selected), 2));
    }
}

#[test]
fn apply_validates_source_and_selected_facts_without_mutation() {
    let selected = run(RunStatus::Interrupted);
    let invalid_source = ExecutionControl {
        forgotten_run_ids: vec![" bad ".into()],
        recovery_request: None,
    };
    let before = invalid_source.clone();
    for action in [ControlAction::Recover, ControlAction::ForgetRun] {
        assert_invalid(invalid_source.apply(&action, Some(&selected), 2));
    }
    assert_eq!(invalid_source, before);
    let mut invalid_run = selected;
    invalid_run.tools[0].status = ToolStatus::Completed;
    let before = invalid_run.clone();
    for action in [ControlAction::Recover, ControlAction::ForgetRun] {
        assert_invalid(ExecutionControl::default().apply(&action, Some(&invalid_run), 2));
    }
    assert_eq!(invalid_run, before);
}

#[test]
fn serde_roundtrips_and_pins_camel_case_reference_only_contract() {
    let selected = run(RunStatus::Interrupted);
    let encoded = serde_json::to_value(&selected).unwrap();
    assert_eq!(
        encoded,
        json!({
            "id":"运行/一", "startedSeq":1, "finishedRecordId":"native-finished", "finishedSeq":2,
            "status":"interrupted", "finalEntryId":null, "assistantEntryId":null,
            "tools":[{
                "invocationId":"native-start-0", "assistantEntryId":"assistant-tools", "toolIndex":0,
                "callId":"call-0", "name":"工具.execute", "reservedResultEntryId":"result-0",
                "resultEntryId":null, "status":"unknown", "replay":"never"
            }]
        })
    );
    assert_eq!(
        serde_json::from_value::<RunCheckpoint>(encoded).unwrap(),
        selected
    );
    let source = ExecutionControl {
        forgotten_run_ids: vec!["older".into()],
        recovery_request: Some(request(&selected.id)),
    };
    let encoded = serde_json::to_value(&source).unwrap();
    assert_eq!(
        encoded,
        json!({
            "forgottenRunIds":["older"],
            "recoveryRequest":{
                "runId":"运行/一", "startedSeq":1, "observedHistoryRevision":2,
                "unknownInvocationIds":["native-start-0"]
            }
        })
    );
    assert_eq!(
        serde_json::from_value::<ExecutionControl>(encoded).unwrap(),
        source
    );
    assert_eq!(
        serde_json::from_str::<ExecutionControl>("{}").unwrap(),
        ExecutionControl::default()
    );
    assert_eq!(
        serde_json::from_value::<ExecutionControl>(
            json!({"forgottenRunIds":[],"recoveryRequest":null})
        )
        .unwrap(),
        ExecutionControl::default()
    );
}

#[test]
fn serde_enum_spelling_is_snake_case_and_all_variants_roundtrip() {
    for status in [
        RunStatus::Open,
        RunStatus::Interrupted,
        RunStatus::Completed,
        RunStatus::Aborted,
        RunStatus::Failed,
        RunStatus::Declined,
    ] {
        let value = serde_json::to_value(&status).unwrap();
        assert_eq!(
            value.as_str().unwrap(),
            format!("{status:?}").to_lowercase()
        );
        assert_eq!(serde_json::from_value::<RunStatus>(value).unwrap(), status);
    }
    for status in [
        ToolStatus::Unknown,
        ToolStatus::Completed,
        ToolStatus::Failed,
    ] {
        let value = serde_json::to_value(&status).unwrap();
        assert_eq!(
            value.as_str().unwrap(),
            format!("{status:?}").to_lowercase()
        );
        assert_eq!(serde_json::from_value::<ToolStatus>(value).unwrap(), status);
    }
    for policy in [ReplayPolicy::Never, ReplayPolicy::Safe] {
        let value = serde_json::to_value(&policy).unwrap();
        assert_eq!(
            value.as_str().unwrap(),
            format!("{policy:?}").to_lowercase()
        );
        assert_eq!(
            serde_json::from_value::<ReplayPolicy>(value).unwrap(),
            policy
        );
    }
    assert!(serde_json::from_value::<RunStatus>(json!("Interrupted")).is_err());
    assert!(serde_json::from_value::<ToolStatus>(json!("interrupted")).is_err());
    assert!(serde_json::from_value::<ReplayPolicy>(json!("automatic")).is_err());
}

#[test]
fn serde_rejects_unknown_fields_in_all_execution_dtos_including_content() {
    let mut encoded = serde_json::to_value(run(RunStatus::Interrupted)).unwrap();
    encoded["transcript"] = json!("must not persist here");
    assert!(serde_json::from_value::<RunCheckpoint>(encoded).is_err());
    for field in ["arguments", "result", "unknownFutureField"] {
        let mut encoded = serde_json::to_value(tool(0)).unwrap();
        encoded[field] = json!("not part of contract");
        assert!(serde_json::from_value::<ToolCheckpoint>(encoded).is_err());
    }
    let mut encoded = serde_json::to_value(request("run")).unwrap();
    encoded["execute"] = json!(true);
    assert!(serde_json::from_value::<RecoveryRequest>(encoded).is_err());
    assert!(serde_json::from_value::<ExecutionControl>(json!({"unknown":true})).is_err());
    assert!(serde_json::from_value::<ExecutionControl>(json!({"forgottenRunId":"old"})).is_err());
}

#[test]
fn serde_rejects_wrong_types_negative_sequences_and_tool_index_overflow() {
    for value in [Value::Null, json!("run"), json!([null]), json!([42])] {
        assert!(
            serde_json::from_value::<ExecutionControl>(json!({"forgottenRunIds":value})).is_err()
        );
    }
    for (field, value) in [
        ("startedSeq", json!(-1)),
        ("startedSeq", json!("1")),
        ("finishedSeq", json!(-1)),
        ("tools", Value::Null),
        ("status", json!("running")),
        ("id", json!(42)),
    ] {
        let mut encoded = serde_json::to_value(run(RunStatus::Interrupted)).unwrap();
        encoded[field] = value;
        assert!(serde_json::from_value::<RunCheckpoint>(encoded).is_err());
    }
    for value in [json!(-1), json!(u64::from(u32::MAX) + 1)] {
        let mut encoded = serde_json::to_value(tool(0)).unwrap();
        encoded["toolIndex"] = value;
        assert!(serde_json::from_value::<ToolCheckpoint>(encoded).is_err());
    }
}

#[test]
fn structural_validation_is_host_agnostic_not_storage_or_branch_proof() {
    let mut selected = run(RunStatus::Interrupted);
    selected.id = "host-must-prove-this-run".into();
    selected.finished_record_id = Some("host-must-prove-this-finish".into());
    selected.assistant_entry_id = Some("possibly-another-branch".into());
    selected.final_entry_id = Some("host-must-prove-terminal-reference".into());
    selected.tools[0].result_entry_id = Some(selected.tools[0].reserved_result_entry_id.clone());
    selected.tools[0].status = ToolStatus::Completed;
    selected.validate().unwrap();
    let candidate = ExecutionControl::default()
        .apply(&ControlAction::Recover, Some(&selected), 2)
        .unwrap();
    assert!(candidate
        .recovery_request
        .unwrap()
        .unknown_invocation_ids
        .is_empty());
}

#[test]
fn forgetting_two_runs_keeps_both_hidden_and_preserves_audit_facts() {
    let first = run(RunStatus::Interrupted);
    let mut second = first.clone();
    second.id = "second-run".into();
    let facts_before = (first.clone(), second.clone());
    let source = ExecutionControl::default();
    let hidden_first = source
        .apply(&ControlAction::ForgetRun, Some(&first), 2)
        .unwrap();
    let hidden_both = hidden_first
        .apply(&ControlAction::ForgetRun, Some(&second), 2)
        .unwrap();
    assert_eq!(
        hidden_both.forgotten_run_ids,
        vec![first.id.clone(), second.id.clone()]
    );
    for selected in [&first, &second] {
        for action in [ControlAction::Recover, ControlAction::ForgetRun] {
            assert_eq!(
                hidden_both.apply(&action, Some(selected), 2),
                Err(ExecutionError::NoRun)
            );
        }
    }
    assert_eq!(source, ExecutionControl::default());
    assert_eq!(hidden_first.forgotten_run_ids, vec![first.id.clone()]);
    assert_eq!((first, second), facts_before);
}

#[test]
fn forgotten_run_capacity_fails_closed_without_eviction_or_clearing_intent() {
    let selected = run(RunStatus::Interrupted);
    let source = ExecutionControl {
        forgotten_run_ids: (0..MAX_FORGOTTEN_RUNS - 1)
            .map(|i| format!("hidden-{i}"))
            .collect(),
        recovery_request: Some(request(&selected.id)),
    };
    let before = source.clone();
    let full = source
        .apply(&ControlAction::ForgetRun, Some(&selected), 2)
        .unwrap();
    assert_eq!(full.forgotten_run_ids.len(), MAX_FORGOTTEN_RUNS);
    assert_eq!(
        &full.forgotten_run_ids[..MAX_FORGOTTEN_RUNS - 1],
        before.forgotten_run_ids.as_slice()
    );
    assert_eq!(full.recovery_request, None);
    assert_eq!(source, before);
    let mut another = selected.clone();
    another.id = "not-yet-hidden".into();
    for target in [another.id.as_str(), "unrelated-run"] {
        let mut full = full.clone();
        full.recovery_request = Some(request(target));
        full.validate().unwrap();
        let before = full.clone();
        assert_eq!(
            full.apply(&ControlAction::ForgetRun, Some(&another), 2),
            Err(ExecutionError::ForgetLimit)
        );
        assert_eq!(full, before);
        for action in [ControlAction::Recover, ControlAction::ForgetRun] {
            assert_eq!(
                full.apply(&action, Some(&selected), 2),
                Err(ExecutionError::NoRun)
            );
        }
        let recovered = full.apply(&ControlAction::Recover, Some(&another), 2);
        if target == another.id {
            assert_eq!(recovered, Err(ExecutionError::AlreadyRequested));
        } else {
            let recovered = recovered.unwrap();
            assert_eq!(recovered.forgotten_run_ids, full.forgotten_run_ids);
            assert_eq!(recovered.recovery_request, Some(request(&another.id)));
        }
    }
}

#[test]
fn forgotten_run_ids_are_bounded_unique_and_conflicts_check_every_id() {
    for count in [0, MAX_FORGOTTEN_RUNS, MAX_FORGOTTEN_RUNS + 1] {
        let source = ExecutionControl {
            forgotten_run_ids: (0..count).map(|i| format!("hidden-{i}")).collect(),
            recovery_request: None,
        };
        assert_eq!(source.validate().is_ok(), count <= MAX_FORGOTTEN_RUNS);
        if count > MAX_FORGOTTEN_RUNS {
            for action in [ControlAction::Recover, ControlAction::ForgetRun] {
                assert_invalid(source.apply(&action, Some(&run(RunStatus::Interrupted)), 2));
            }
        }
    }
    let mut source = ExecutionControl {
        forgotten_run_ids: vec!["first".into(), "second".into(), "first".into()],
        recovery_request: None,
    };
    let before = source.clone();
    assert_invalid(source.validate());
    for action in [ControlAction::Recover, ControlAction::ForgetRun] {
        assert_invalid(source.apply(&action, Some(&run(RunStatus::Interrupted)), 2));
    }
    assert_eq!(source, before);
    source.forgotten_run_ids.pop();
    source.validate().unwrap();
    for target in ["first", "second"] {
        source.recovery_request = Some(request(target));
        assert_invalid(source.validate());
    }
}

#[test]
fn hidden_run_facts_are_validated_before_visibility_is_checked() {
    let mut selected = run(RunStatus::Interrupted);
    let source = ExecutionControl {
        forgotten_run_ids: vec![selected.id.clone()],
        recovery_request: None,
    };
    selected.started_seq = 0;
    for action in [ControlAction::Recover, ControlAction::ForgetRun] {
        assert_invalid(source.apply(&action, Some(&selected), 2));
    }
}
