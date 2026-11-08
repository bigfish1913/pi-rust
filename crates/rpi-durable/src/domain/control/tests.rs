use super::*;
use serde_json::{json, Value};

#[test]
fn hand_authored_pinned_compatibility_fixtures() {
    let cases: Value =
        serde_json::from_str(include_str!("../../../tests/fixtures/control-v1.json")).unwrap();
    for case in cases.as_array().unwrap() {
        let mut state = ControlState::default();
        for input in case["actions"].as_array().unwrap() {
            commit(&mut state, input.clone());
        }
        assert_eq!(
            serde_json::to_value(&state).unwrap(),
            case["expected"],
            "{}",
            case["name"]
        );
    }
}

fn action(value: Value) -> ControlAction {
    serde_json::from_value(value).unwrap()
}

/// Simulate host acknowledgement/publication, never hide reducer revision changes.
fn commit(state: &mut ControlState, value: Value) {
    let mut candidate = state.apply(&action(value)).unwrap();
    assert_eq!(candidate.revision, state.revision);
    if candidate != *state {
        candidate.revision = state.revision.checked_add(1).unwrap();
    }
    candidate.validate().unwrap();
    *state = candidate;
}

fn started() -> ControlState {
    let mut state = ControlState::default();
    commit(
        &mut state,
        json!({"action":"start", "name":"Release", "steps":[
            {"id":"build", "title":"Build"}, {"id":"test", "title":"Test"}
        ]}),
    );
    state
}

fn queued() -> ControlState {
    let mut state = ControlState::default();
    commit(&mut state, json!({"action":"enqueue", "text":"work"}));
    state
}

fn unchanged_error(state: &ControlState, value: Value) -> ControlError {
    let before = state.clone();
    let error = state.apply(&action(value)).unwrap_err();
    assert_eq!(*state, before);
    assert!(!error.to_string().is_empty());
    error
}

#[test]
fn all_public_actions_deserialize_with_host_metadata() {
    let actions = [
        json!({"action":"status"}),
        json!({"action":"queue"}),
        json!({"action":"start", "name":"x", "steps":[{"id":"a", "title":"A"}]}),
        json!({"action":"checkpoint", "step":"a"}),
        json!({"action":"pause"}),
        json!({"action":"resume"}),
        json!({"action":"complete"}),
        json!({"action":"fail"}),
        json!({"action":"reset"}),
        json!({"action":"enqueue", "text":"x"}),
        json!({"action":"claim", "item":"item-1"}),
        json!({"action":"finish", "item":"item-1", "status":"completed"}),
        json!({"action":"cancel", "item":"item-1"}),
        json!({"action":"recover"}),
        json!({"action":"forget_run"}),
    ];
    for mut value in actions {
        value["__rpi"] = json!({"trace":[1, {"id":"host"}]});
        value["unknownFutureField"] = json!(true);
        let parsed = action(value);
        assert_eq!(action(serde_json::to_value(&parsed).unwrap()), parsed);
    }
}

#[test]
fn parser_rejects_unknown_actions_missing_required_fields_and_wrong_types() {
    for value in [
        json!({"action":"start_run"}),
        json!({"action":"settle_run"}),
        json!({"action":"recover_run"}),
        json!({"action":"bogus"}),
        json!({"action":"start", "name":"x"}),
        json!({"action":"start", "name":"x", "steps":[{"id":"a"}]}),
        json!({"action":"checkpoint"}),
        json!({"action":"enqueue"}),
        json!({"action":"claim"}),
        json!({"action":"cancel"}),
        json!({"action":"finish", "item":"item-1"}),
        json!({"action":"finish", "item":"item-1", "status":null}),
        json!({"action":"checkpoint", "step":"a", "status":"failed"}),
        json!({"action":"pause", "reason":42}),
        json!({"action":"cancel", "item":"item-1", "note":{}}),
    ] {
        assert!(
            serde_json::from_value::<ControlAction>(value.clone()).is_err(),
            "{value}"
        );
    }
}

#[test]
fn optional_nulls_and_checkpoint_defaults() {
    for value in [
        json!({"action":"checkpoint", "step":"build"}),
        json!({"action":"checkpoint", "step":"build", "status":null, "note":null}),
    ] {
        assert_eq!(
            action(value),
            ControlAction::Checkpoint {
                step: "build".into(),
                status: StepStatus::InProgress,
                note: None,
            }
        );
    }
    for value in [
        json!({"action":"pause", "reason":null}),
        json!({"action":"fail", "reason":null}),
        json!({"action":"finish", "item":"item-1", "status":"failed", "note":null}),
        json!({"action":"cancel", "item":"item-1", "note":null}),
    ] {
        action(value);
    }
}

#[test]
fn start_trims_replaces_workflow_and_resets_step_metadata() {
    let mut state = started();
    commit(
        &mut state,
        json!({"action":"enqueue", "text":"queue survives"}),
    );
    let queue = state.queue.clone();
    commit(
        &mut state,
        json!({"action":"start", "name":" 新任务 ", "steps":[
            {"id":" a ", "title":" 标题 ", "status":"completed", "note":"ignored"}
        ]}),
    );
    let workflow = state.workflow.unwrap();
    assert_eq!(workflow.name, "新任务");
    assert_eq!(workflow.status, WorkflowStatus::Running);
    assert_eq!(workflow.reason, None);
    assert_eq!(
        workflow.steps,
        vec![Step {
            id: "a".into(),
            title: "标题".into(),
            status: StepStatus::Pending,
            note: None
        }]
    );
    assert_eq!(state.queue, queue);
}

#[test]
fn workflow_lifecycle_complete_and_checkpoint_regression() {
    let mut state = started();
    assert_eq!(
        unchanged_error(&state, json!({"action":"complete"})),
        ControlError::IncompleteSteps
    );
    for step in ["build", "test"] {
        commit(
            &mut state,
            json!({"action":"checkpoint", "step":step, "status":"completed", "note":" ok "}),
        );
    }
    // Completion is not monotonic at the step level while the workflow runs.
    commit(
        &mut state,
        json!({"action":"checkpoint", "step":"build", "status":"pending", "note":null}),
    );
    assert_eq!(
        state.workflow.as_ref().unwrap().steps[0].note.as_deref(),
        Some("ok")
    );
    unchanged_error(&state, json!({"action":"complete"}));
    commit(
        &mut state,
        json!({"action":"checkpoint", "step":"build", "status":"completed"}),
    );
    commit(&mut state, json!({"action":"complete"}));
    assert_eq!(
        state.workflow.as_ref().unwrap().status,
        WorkflowStatus::Completed
    );
    unchanged_error(&state, json!({"action":"complete"}));
}

#[test]
fn pause_resume_fail_defaults_and_custom_reasons() {
    let mut state = started();
    commit(&mut state, json!({"action":"pause", "reason":" waiting "}));
    assert_eq!(
        state.workflow.as_ref().unwrap().reason.as_deref(),
        Some("waiting")
    );
    commit(&mut state, json!({"action":"resume"}));
    assert_eq!(state.workflow.as_ref().unwrap().reason, None);
    commit(&mut state, json!({"action":"pause", "reason":null}));
    commit(&mut state, json!({"action":"fail", "reason":null}));
    assert_eq!(
        state.workflow.as_ref().unwrap().reason.as_deref(),
        Some("workflow marked as failed")
    );
    let mut state = started();
    commit(&mut state, json!({"action":"fail", "reason":" blocked "}));
    assert_eq!(state.workflow.unwrap().reason.as_deref(), Some("blocked"));
}

#[test]
fn workflow_transition_matrix() {
    for status in [
        WorkflowStatus::Running,
        WorkflowStatus::Paused,
        WorkflowStatus::Completed,
        WorkflowStatus::Failed,
    ] {
        let mut state = started();
        let workflow = state.workflow.as_mut().unwrap();
        workflow.status = status;
        for step in &mut workflow.steps {
            step.status = StepStatus::Completed;
        }
        for (name, allowed) in [
            ("checkpoint", status == WorkflowStatus::Running),
            ("pause", status == WorkflowStatus::Running),
            ("resume", status == WorkflowStatus::Paused),
            ("complete", status == WorkflowStatus::Running),
            (
                "fail",
                matches!(status, WorkflowStatus::Running | WorkflowStatus::Paused),
            ),
        ] {
            let value = if name == "checkpoint" {
                json!({"action":name, "step":"build"})
            } else {
                json!({"action":name})
            };
            let result = state.apply(&action(value));
            assert_eq!(result.is_ok(), allowed, "{status:?}: {name}");
            if !allowed {
                assert!(matches!(
                    result,
                    Err(ControlError::WorkflowTransition { .. })
                ));
            }
        }
    }
}

#[test]
fn missing_workflow_and_steps_are_typed_errors() {
    for value in [
        json!({"action":"checkpoint", "step":"x"}),
        json!({"action":"pause"}),
        json!({"action":"resume"}),
        json!({"action":"complete"}),
        json!({"action":"fail"}),
    ] {
        assert_eq!(
            unchanged_error(&ControlState::default(), value),
            ControlError::NoWorkflow
        );
    }
    assert_eq!(
        unchanged_error(&started(), json!({"action":"checkpoint", "step":"absent"})),
        ControlError::StepNotFound {
            id: "absent".into()
        }
    );
}

#[test]
fn duplicate_trimmed_steps_and_step_count_limits() {
    assert_eq!(
        unchanged_error(
            &ControlState::default(),
            json!({"action":"start", "name":"x", "steps":[
                {"id":"a", "title":"A"}, {"id":" a ", "title":"B"}
            ]})
        ),
        ControlError::DuplicateStepId { id: "a".into() }
    );
    for count in [0, MAX_STEPS, MAX_STEPS + 1] {
        let steps: Vec<_> = (0..count)
            .map(|i| json!({"id":format!("s-{i}"), "title":"step"}))
            .collect();
        let result = ControlState::default().apply(&action(
            json!({"action":"start", "name":"x", "steps":steps}),
        ));
        assert_eq!(result.is_ok(), count == MAX_STEPS);
    }
}

#[test]
fn reads_and_identical_workflow_changes_are_no_ops() {
    for mut state in [ControlState::default(), started(), queued()] {
        state.revision = u64::MAX;
        assert_eq!(state.apply(&ControlAction::Status).unwrap(), state);
        assert_eq!(state.apply(&ControlAction::Queue).unwrap(), state);
    }
    let mut state = started();
    commit(
        &mut state,
        json!({"action":"checkpoint", "step":"build", "note":"keep"}),
    );
    state.revision = u64::MAX;
    assert_eq!(
        state
            .apply(&action(
                json!({"action":"checkpoint", "step":"build", "note":null})
            ))
            .unwrap(),
        state
    );
    let empty = ControlState {
        revision: u64::MAX,
        ..Default::default()
    };
    assert_eq!(empty.apply(&ControlAction::Reset).unwrap(), empty);
}

#[test]
fn reset_keeps_queue_and_counter() {
    let mut state = started();
    commit(&mut state, json!({"action":"enqueue", "text":"work"}));
    let queue = state.queue.clone();
    let counter = state.next_queue_id;
    commit(&mut state, json!({"action":"reset"}));
    assert!(state.workflow.is_none());
    assert_eq!(state.queue, queue);
    assert_eq!(state.next_queue_id, counter);
    commit(&mut state, json!({"action":"enqueue", "text":"next"}));
    assert_eq!(state.queue[1].id, "item-2");
}

#[test]
fn execution_actions_never_fabricate_facts() {
    for state in [ControlState::default(), started(), queued()] {
        for action in [ControlAction::Recover, ControlAction::ForgetRun] {
            assert_eq!(
                state.apply(&action),
                Err(ControlError::ExecutionFactsRequired)
            );
        }
    }
}

#[test]
fn queue_candidate_stamps_next_revision_without_bumping_it() {
    let state = ControlState {
        revision: 42,
        ..Default::default()
    };
    let mut candidate = state
        .apply(&ControlAction::Enqueue {
            text: " work ".into(),
        })
        .unwrap();
    assert_eq!(candidate.revision, 42);
    assert!(state.queue.is_empty());
    assert_eq!(candidate.queue[0].id, "item-1");
    assert_eq!(candidate.queue[0].created_revision, 43);
    assert_eq!(candidate.queue[0].updated_revision, 43);
    assert_eq!(candidate.queue[0].text, "work");
    assert!(matches!(
        candidate.validate(),
        Err(ControlError::InvalidQueueRevisions { .. })
    ));
    candidate.revision = 43;
    candidate.validate().unwrap();
    let claimed = candidate
        .apply(&ControlAction::Claim {
            item: "item-1".into(),
        })
        .unwrap();
    assert_eq!(claimed.queue[0].created_revision, 43);
    assert_eq!(claimed.queue[0].updated_revision, 44);
    assert_eq!(claimed.queue[0].claimed_by, None);
}

#[test]
fn queue_claim_finish_notes_and_host_owner_preserved() {
    for status in ["completed", "failed"] {
        let mut state = queued();
        commit(&mut state, json!({"action":"claim", "item":" item-1 "}));
        state.queue[0].claimed_by = Some("actor-1".into());
        state.queue[0].note = "prior note".into();
        commit(
            &mut state,
            json!({"action":"finish", "item":"item-1", "status":status, "note":null}),
        );
        assert_eq!(state.queue[0].status.as_str(), status);
        assert_eq!(state.queue[0].note, "prior note");
        assert_eq!(state.queue[0].claimed_by.as_deref(), Some("actor-1"));
    }
    let mut state = queued();
    commit(&mut state, json!({"action":"claim", "item":"item-1"}));
    commit(
        &mut state,
        json!({"action":"finish", "item":"item-1", "status":"completed", "note":" done "}),
    );
    assert_eq!(state.queue[0].note, "done");
}

#[test]
fn cancel_default_custom_and_null_note_semantics() {
    for claimed in [false, true] {
        for note in [None, Some("already noted")] {
            let mut state = queued();
            if claimed {
                commit(&mut state, json!({"action":"claim", "item":"item-1"}));
            }
            state.queue[0].note = note.unwrap_or_default().into();
            commit(
                &mut state,
                json!({"action":"cancel", "item":"item-1", "note":null}),
            );
            assert_eq!(state.queue[0].status, QueueStatus::Cancelled);
            assert_eq!(state.queue[0].note, "cancelled");
        }
    }
    let mut state = queued();
    commit(
        &mut state,
        json!({"action":"cancel", "item":"item-1", "note":" not needed "}),
    );
    assert_eq!(state.queue[0].note, "not needed");
    let mut state = queued();
    state.queue[0].note = "replaced by default".into();
    commit(&mut state, json!({"action":"cancel", "item":"item-1"}));
    assert_eq!(state.queue[0].note, "cancelled");
}

#[test]
fn queue_transition_matrix_and_invalid_finish_statuses() {
    for status in [
        QueueStatus::Queued,
        QueueStatus::Claimed,
        QueueStatus::Completed,
        QueueStatus::Failed,
        QueueStatus::Cancelled,
    ] {
        let mut state = queued();
        state.queue[0].status = status;
        for (name, allowed) in [
            ("claim", status == QueueStatus::Queued),
            ("finish", status == QueueStatus::Claimed),
            (
                "cancel",
                matches!(status, QueueStatus::Queued | QueueStatus::Claimed),
            ),
        ] {
            let value = if name == "finish" {
                json!({"action":name,"item":"item-1","status":"completed"})
            } else {
                json!({"action":name,"item":"item-1"})
            };
            let result = state.apply(&action(value));
            assert_eq!(result.is_ok(), allowed, "{status:?}: {name}");
            if !allowed {
                assert!(matches!(result, Err(ControlError::QueueTransition { .. })));
            }
        }
    }
    for status in [
        QueueStatus::Queued,
        QueueStatus::Claimed,
        QueueStatus::Cancelled,
    ] {
        assert_eq!(
            queued().apply(&ControlAction::Finish {
                item: "item-1".into(),
                status,
                note: None
            }),
            Err(ControlError::InvalidFinishStatus)
        );
    }
}

#[test]
fn item_ids_are_stable_exact_not_indices() {
    for id in ["0", "1", "item-0", "item-01", "ITEM-1", "item-2", "item-+1"] {
        assert_eq!(
            unchanged_error(&queued(), json!({"action":"claim", "item":id})),
            ControlError::QueueItemNotFound { id: id.into() }
        );
    }
}

#[test]
fn terminal_queue_items_still_count_toward_cap() {
    let mut state = ControlState::default();
    for i in 1..=MAX_QUEUE_ITEMS {
        commit(
            &mut state,
            json!({"action":"enqueue", "text":format!("work-{i}")}),
        );
        commit(
            &mut state,
            json!({"action":"cancel", "item":format!("item-{i}")}),
        );
    }
    assert_eq!(
        unchanged_error(&state, json!({"action":"enqueue", "text":"overflow"})),
        ControlError::QueueFull
    );
    commit(&mut state, json!({"action":"reset"}));
    assert_eq!(state.queue.len(), MAX_QUEUE_ITEMS);
}

#[test]
fn unicode_scalar_limits_are_after_trim_not_bytes_or_graphemes() {
    // Multi-byte scalars count once; combining marks count separately.
    let mut state = ControlState::default();
    commit(
        &mut state,
        json!({"action":"start", "name":format!("　{}　", "🦀".repeat(MAX_NAME_CHARS)), "steps":[
            {"id":"é".repeat(MAX_ID_CHARS), "title":"界".repeat(MAX_TITLE_CHARS)}
        ]}),
    );
    commit(
        &mut state,
        json!({"action":"checkpoint", "step":"é".repeat(MAX_ID_CHARS), "note":"🦀".repeat(MAX_TEXT_CHARS)}),
    );
    commit(
        &mut state,
        json!({"action":"pause", "reason":"界".repeat(MAX_TEXT_CHARS)}),
    );
    commit(
        &mut state,
        json!({"action":"enqueue", "text":format!(" {} ", "🦀".repeat(MAX_TEXT_CHARS))}),
    );
    assert!(matches!(
        unchanged_error(
            &state,
            json!({"action":"enqueue", "text":"e\u{301}".repeat(MAX_TEXT_CHARS / 2 + 1)})
        ),
        ControlError::TextTooLong { field: "text", .. }
    ));
    let cases = [
        json!({"action":"start", "name":"x".repeat(MAX_NAME_CHARS + 1), "steps":[{"id":"a","title":"A"}]}),
        json!({"action":"start", "name":"x", "steps":[{"id":"x".repeat(MAX_ID_CHARS + 1),"title":"A"}]}),
        json!({"action":"start", "name":"x", "steps":[{"id":"a","title":"x".repeat(MAX_TITLE_CHARS + 1)}]}),
        json!({"action":"fail", "reason":"x".repeat(MAX_TEXT_CHARS + 1)}),
        json!({"action":"cancel", "item":"item-1", "note":"x".repeat(MAX_TEXT_CHARS + 1)}),
        json!({"action":"claim", "item":"x".repeat(MAX_ID_CHARS + 1)}),
    ];
    for case in cases {
        assert!(matches!(
            unchanged_error(&state, case),
            ControlError::TextTooLong { .. }
        ));
    }
}

#[test]
fn blank_inputs_and_optional_blank_notes_fail_without_changes() {
    for value in [
        json!({"action":"start", "name":"　 ", "steps":[{"id":"a","title":"A"}]}),
        json!({"action":"start", "name":"x", "steps":[{"id":"　 ","title":"A"}]}),
        json!({"action":"start", "name":"x", "steps":[{"id":"a","title":"　 "}]}),
        json!({"action":"checkpoint", "step":" "}),
        json!({"action":"checkpoint", "step":"build", "note":" "}),
        json!({"action":"pause", "reason":" "}),
        json!({"action":"fail", "reason":" "}),
        json!({"action":"enqueue", "text":" "}),
        json!({"action":"claim", "item":" "}),
        json!({"action":"finish", "item":"item-1", "status":"completed", "note":" "}),
        json!({"action":"cancel", "item":"item-1", "note":" "}),
    ] {
        assert!(matches!(
            unchanged_error(&started(), value),
            ControlError::EmptyText { .. }
        ));
    }
}

#[test]
fn checked_revision_and_queue_counter_overflow() {
    let mut state = queued();
    state.revision = u64::MAX;
    for value in [
        json!({"action":"enqueue","text":"x"}),
        json!({"action":"claim","item":"item-1"}),
        json!({"action":"cancel","item":"item-1"}),
        json!({"action":"start","name":"x","steps":[{"id":"a","title":"A"}]}),
    ] {
        assert_eq!(
            unchanged_error(&state, value),
            ControlError::RevisionOverflow
        );
    }
    state.queue[0].status = QueueStatus::Claimed;
    assert_eq!(
        unchanged_error(
            &state,
            json!({"action":"finish","item":"item-1","status":"failed"})
        ),
        ControlError::RevisionOverflow
    );
    let state = ControlState {
        next_queue_id: u64::MAX,
        ..Default::default()
    };
    assert_eq!(
        unchanged_error(&state, json!({"action":"enqueue","text":"x"})),
        ControlError::QueueIdOverflow
    );
    let mut state = ControlState {
        next_queue_id: u64::MAX - 1,
        revision: u64::MAX - 1,
        ..Default::default()
    };
    commit(&mut state, json!({"action":"enqueue","text":"last"}));
    assert_eq!(state.queue[0].id, format!("item-{}", u64::MAX));
    assert_eq!(state.revision, u64::MAX);
}

#[test]
fn serialized_shape_and_roundtrip_exclude_execution_state() {
    let mut state = started();
    commit(&mut state, json!({"action":"enqueue", "text":"work"}));
    let value = serde_json::to_value(&state).unwrap();
    assert_eq!(value.as_object().unwrap().len(), 4);
    assert_eq!(value["nextQueueId"], 1);
    assert_eq!(value["queue"][0]["createdRevision"], 2);
    assert!(value["queue"][0].get("claimedBy").is_none());
    assert!(value["queue"][0].get("note").is_none());
    assert_eq!(
        serde_json::from_value::<ControlState>(value).unwrap(),
        state
    );
    assert_eq!(
        serde_json::from_value::<ControlState>(json!({})).unwrap(),
        ControlState::default()
    );
    commit(&mut state, json!({"action":"claim", "item":"item-1"}));
    state.queue[0].claimed_by = Some("host-actor".into());
    let value = serde_json::to_value(&state).unwrap();
    assert_eq!(value["queue"][0]["claimedBy"], "host-actor");
    assert_eq!(
        serde_json::from_value::<ControlState>(value).unwrap(),
        state
    );
}

#[test]
fn restored_invalid_workflow_is_rejected_by_validate_and_apply() {
    let base = started();
    for index in 0..10 {
        let mut state = base.clone();
        let workflow = state.workflow.as_mut().unwrap();
        match index {
            0 => workflow.name = " ".into(),
            1 => workflow.name = "x".repeat(MAX_NAME_CHARS + 1),
            2 => workflow.steps.clear(),
            3 => workflow.steps.push(workflow.steps[0].clone()),
            4 => workflow.steps[0].id = " ".into(),
            5 => workflow.steps[0].title = " ".into(),
            6 => workflow.steps[0].note = Some("x".repeat(MAX_TEXT_CHARS + 1)),
            7 => workflow.reason = Some("unexpected".into()),
            8 => workflow.status = WorkflowStatus::Completed,
            9 => workflow.steps[0].id = " build ".into(),
            _ => unreachable!(),
        }
        assert!(state.validate().is_err(), "case {index}");
        assert!(state.apply(&ControlAction::Status).is_err(), "case {index}");
    }
}

#[test]
fn restored_invalid_queue_invariants_are_rejected() {
    let base = queued();
    for index in 0..13 {
        let mut state = base.clone();
        match index {
            0 => state.queue[0].id = "item-01".into(),
            1 => state.queue[0].id = "item-0".into(),
            2 => state.queue.push(state.queue[0].clone()),
            3 => state.next_queue_id = 0,
            4 => state.queue[0].text = " ".into(),
            5 => state.queue[0].text = "x".repeat(MAX_TEXT_CHARS + 1),
            6 => state.queue[0].note = "x".repeat(MAX_TEXT_CHARS + 1),
            7 => state.queue[0].created_revision = 0,
            8 => state.queue[0].created_revision = state.queue[0].updated_revision + 1,
            9 => state.queue[0].updated_revision = state.revision + 1,
            10 => state.queue[0].claimed_by = Some("host".into()),
            11 => {
                state.queue[0].status = QueueStatus::Claimed;
                state.queue[0].claimed_by = Some(" ".into());
            }
            12 => state.queue = vec![state.queue[0].clone(); MAX_QUEUE_ITEMS + 1],
            _ => unreachable!(),
        }
        assert!(state.validate().is_err(), "case {index}");
        assert!(state.apply(&ControlAction::Reset).is_err(), "case {index}");
    }
}

#[test]
fn deterministic_property_like_sequences_preserve_invariants_and_source() {
    for seed in 0..32_u64 {
        let mut random = seed + 1;
        let mut state = ControlState::default();
        for _ in 0..512 {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            let item = format!("item-{}", (random >> 16) % 35 + 1);
            let command = match (random >> 32) % 15 {
                0 => ControlAction::Status,
                1 => ControlAction::Queue,
                2 => ControlAction::Start {
                    name: "sequence".into(),
                    steps: vec![StepInput {
                        id: "a".into(),
                        title: "A".into(),
                    }],
                },
                3 => ControlAction::Checkpoint {
                    step: "a".into(),
                    status: StepStatus::Completed,
                    note: None,
                },
                4 => ControlAction::Pause { reason: None },
                5 => ControlAction::Resume,
                6 => ControlAction::Complete,
                7 => ControlAction::Fail { reason: None },
                8 => ControlAction::Reset,
                9 => ControlAction::Enqueue {
                    text: "task".into(),
                },
                10 => ControlAction::Claim { item },
                11 => ControlAction::Finish {
                    item,
                    status: QueueStatus::Failed,
                    note: None,
                },
                12 => ControlAction::Cancel { item, note: None },
                13 => ControlAction::Recover,
                _ => ControlAction::ForgetRun,
            };
            let before = state.clone();
            match state.apply(&command) {
                Ok(mut candidate) => {
                    assert_eq!(state, before);
                    assert_eq!(candidate.revision, state.revision);
                    if candidate != state {
                        candidate.revision += 1;
                    }
                    candidate.validate().unwrap();
                    let restored: ControlState =
                        serde_json::from_value(serde_json::to_value(&candidate).unwrap()).unwrap();
                    assert_eq!(restored, candidate);
                    assert!(candidate.next_queue_id >= state.next_queue_id);
                    for old in &state.queue {
                        let new = candidate
                            .queue
                            .iter()
                            .find(|item| item.id == old.id)
                            .unwrap();
                        assert_eq!(new.created_revision, old.created_revision);
                        assert!(new.updated_revision >= old.updated_revision);
                        if matches!(
                            old.status,
                            QueueStatus::Completed | QueueStatus::Failed | QueueStatus::Cancelled
                        ) {
                            assert_eq!(new, old);
                        }
                    }
                    state = candidate;
                }
                Err(_) => assert_eq!(state, before),
            }
        }
    }
}
