//! Frozen pre-optimization decoder for semantic and diagnostic parity checks.
use super::*;
use serde_json::json;

#[derive(Debug)]
pub(crate) struct LegacyEntry(pub(crate) Entry);

impl<'de> Deserialize<'de> for LegacyEntry {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = serde_json::Value::deserialize(d)?;
        let obj = raw
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("entry is not a JSON object"))?;
        let ty = obj
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| serde::de::Error::custom("entry missing \"type\""))?;
        let base = deserialize_base(obj).map_err(serde::de::Error::custom)?;
        let entry = match ty {
            "message" => Entry::Message(MessageEntry {
                base,
                message: serde_json::from_value(obj.get("message").cloned().ok_or_else(|| {
                    serde::de::Error::custom("message entry missing \"message\"")
                })?)
                .map_err(serde::de::Error::custom)?,
                terminate: obj.get("terminate").and_then(|v| v.as_bool()),
            }),
            "model_change" => {
                Entry::ModelChange(ModelChangeEntry {
                    base,
                    provider: serde_json::from_value(obj.get("provider").cloned().ok_or_else(
                        || serde::de::Error::custom("model_change missing \"provider\""),
                    )?)
                    .map_err(serde::de::Error::custom)?,
                    model_id: serde_json::from_value(obj.get("modelId").cloned().ok_or_else(
                        || serde::de::Error::custom("model_change missing \"modelId\""),
                    )?)
                    .map_err(serde::de::Error::custom)?,
                })
            }
            "thinking_level_change" => Entry::ThinkingLevel(ThinkingLevelEntry {
                base,
                thinking_level: serde_json::from_value(
                    obj.get("thinkingLevel").cloned().ok_or_else(|| {
                        serde::de::Error::custom("thinking_level_change missing \"thinkingLevel\"")
                    })?,
                )
                .map_err(serde::de::Error::custom)?,
            }),
            "active_tools_change" => Entry::ActiveTools(ActiveToolsEntry {
                base,
                active_tool_names: serde_json::from_value(
                    obj.get("activeToolNames").cloned().ok_or_else(|| {
                        serde::de::Error::custom("active_tools_change missing \"activeToolNames\"")
                    })?,
                )
                .map_err(serde::de::Error::custom)?,
            }),
            "compaction" => {
                let retained_tail =
                    serde_json::from_value(obj.get("retainedTail").cloned().ok_or_else(|| {
                        serde::de::Error::custom("compaction missing \"retainedTail\"")
                    })?)
                    .map_err(serde::de::Error::custom)?;
                let summary =
                    serde_json::from_value(obj.get("summary").cloned().ok_or_else(|| {
                        serde::de::Error::custom("compaction missing \"summary\"")
                    })?)
                    .map_err(serde::de::Error::custom)?;
                let tokens_before =
                    serde_json::from_value(obj.get("tokensBefore").cloned().ok_or_else(|| {
                        serde::de::Error::custom("compaction missing \"tokensBefore\"")
                    })?)
                    .map_err(serde::de::Error::custom)?;
                Entry::Compaction(CompactionEntry {
                    base,
                    summary,
                    retained_tail,
                    tokens_before,
                    details: obj.get("details").cloned(),
                    usage: obj
                        .get("usage")
                        .and_then(|v| serde_json::from_value(v.clone()).ok()),
                })
            }
            "branch_summary" => Entry::BranchSummary(BranchSummaryEntry {
                base,
                from_id: serde_json::from_value(obj.get("fromId").cloned().ok_or_else(|| {
                    serde::de::Error::custom("branch_summary missing \"fromId\"")
                })?)
                .map_err(serde::de::Error::custom)?,
                summary: serde_json::from_value(obj.get("summary").cloned().ok_or_else(|| {
                    serde::de::Error::custom("branch_summary missing \"summary\"")
                })?)
                .map_err(serde::de::Error::custom)?,
                details: obj.get("details").cloned(),
                usage: obj
                    .get("usage")
                    .and_then(|v| serde_json::from_value(v.clone()).ok()),
            }),
            "custom" => Entry::Custom(CustomEntry {
                base,
                custom_type: serde_json::from_value(
                    obj.get("customType")
                        .cloned()
                        .ok_or_else(|| serde::de::Error::custom("custom missing \"customType\""))?,
                )
                .map_err(serde::de::Error::custom)?,
                data: obj.get("data").cloned(),
            }),
            other => {
                return Err(serde::de::Error::custom(format!(
                    "unknown entry type {other}"
                )))
            }
        };
        Ok(Self(entry))
    }
}

pub(crate) fn fixtures() -> Vec<JsonValue> {
    let tool = json!({
        "kind":"tool_result", "role":"toolResult", "toolCallId":"call-🌊", "toolName":"read",
        "content":[{"type":"text", "text":"large output\r\n"}],
        "details":{"nested":[null,true,1.25,{"result":"ok"}]}, "isError":false, "timestamp":2
    });
    let user = json!({"kind":"user", "role":"user", "content":"hello", "timestamp":1});
    let assistant = serde_json::to_value(rpi_agent::message::AgentMessage::Assistant(Box::new(
        rpi_ai::types::AssistantMessage::empty(rpi_ai::types::Api::Faux, "faux", "faux", 1),
    )))
    .unwrap();
    let custom =
        json!({"kind":"custom", "role":"notice", "content":[], "data":{"x":1}, "timestamp":1});
    [
        json!({"type":"message", "message":user}),
        json!({"type":"message", "message":tool}),
        json!({"type":"message", "message":assistant}),
        json!({"type":"message", "message":custom}),
        json!({"type":"model_change", "provider":"fixture", "modelId":"model"}),
        json!({"type":"thinking_level_change", "thinkingLevel":"high"}),
        json!({"type":"active_tools_change", "activeToolNames":["read", "write"]}),
        json!({"type":"compaction", "retainedTail":[tool], "summary":"summary", "tokensBefore":123,
            "details":null, "usage":{"invalid":true}}),
        json!({"type":"branch_summary", "fromId":"parent", "summary":"summary", "details":{"x":1}}),
        json!({"type":"custom", "customType":"rpi.value", "data":{"key":"x", "value":[true,null,1]}}),
    ].into_iter().map(|arm| {
        let mut base = json!({"id":"entry-1", "seq":1, "parentId":null, "timestamp":1});
        base.as_object_mut().unwrap().extend(arm.as_object().unwrap().clone());
        base
    }).collect()
}

fn outcome(result: Result<Entry, serde_json::Error>) -> Result<Entry, String> {
    result.map_err(|e| e.to_string())
}

fn assert_parity(value: JsonValue) {
    let expected = outcome(serde_json::from_value::<LegacyEntry>(value.clone()).map(|e| e.0));
    assert_eq!(
        outcome(serde_json::from_value::<Entry>(value.clone())),
        expected,
        "serde: {value}"
    );
    if let JsonValue::Object(obj) = value {
        assert_eq!(
            outcome(Entry::from_json_object(obj.clone())),
            expected,
            "owned: {obj:?}"
        );
    }
}

#[test]
fn all_entry_variants_and_required_field_errors_match_the_frozen_decoder() {
    let replacements = [
        json!(null),
        json!(false),
        json!(42),
        json!(1.5),
        json!(""),
        json!([]),
        json!({}),
    ];
    for value in fixtures() {
        assert!(serde_json::from_value::<LegacyEntry>(value.clone()).is_ok());
        assert_parity(value.clone());
        for key in value.as_object().unwrap().keys() {
            let mut missing = value.clone();
            missing.as_object_mut().unwrap().shift_remove(key);
            assert_parity(missing);
            for replacement in &replacements {
                let mut invalid = value.clone();
                invalid[key] = replacement.clone();
                assert_parity(invalid);
            }
        }
    }
    for value in [
        json!(null),
        json!(true),
        json!([]),
        json!(42),
        json!("entry"),
    ] {
        assert_parity(value);
    }
}

#[test]
fn nullable_permissive_and_unknown_entry_fields_keep_their_existing_meaning() {
    for value in fixtures() {
        for key in [
            "parentId",
            "terminate",
            "details",
            "usage",
            "data",
            "future",
        ] {
            for replacement in [
                json!(null),
                json!(false),
                json!(42),
                json!([]),
                json!({}),
                json!("value"),
            ] {
                let mut variant = value.clone();
                variant[key] = replacement;
                assert_parity(variant);
            }
        }
    }
}

#[test]
fn private_raw_value_keys_preserve_normalization_and_error_diagnostics() {
    const RAW: &str = "$serde_json::private::RawValue";
    let values = fixtures();
    for value in &values {
        for raw in [
            json!("null"),
            json!("{\"replacement\":true}"),
            json!("invalid"),
            json!(42),
            json!(value.to_string()),
        ] {
            let special = JsonValue::Object(serde_json::Map::from_iter([(RAW.into(), raw)]));
            assert_parity(special.clone());
            for key in [
                "message",
                "retainedTail",
                "data",
                "details",
                "usage",
                "future",
            ] {
                let mut variant = value.clone();
                variant[key] = special.clone();
                assert_parity(variant.clone());
                variant[key] = json!([special]);
                assert_parity(variant);
            }
            let mut last = value.clone();
            last[RAW] = special[RAW].clone();
            assert_parity(last);
        }
    }
}

#[test]
fn owned_entry_moves_large_payload_buffers_instead_of_cloning_them() {
    for ty in ["custom", "compaction", "branch_summary"] {
        let mut value = fixtures().into_iter().find(|v| v["type"] == ty).unwrap();
        let key = if ty == "custom" { "data" } else { "details" };
        value[key] = json!({"body":"🌊".repeat(64 * 1024)});
        let before = value[key]["body"].as_str().unwrap().as_ptr();
        let JsonValue::Object(obj) = value else {
            unreachable!()
        };
        let entry = Entry::from_json_object(obj).unwrap();
        let data = match &entry {
            Entry::Custom(e) => e.data.as_ref().unwrap(),
            Entry::Compaction(e) => e.details.as_ref().unwrap(),
            Entry::BranchSummary(e) => e.details.as_ref().unwrap(),
            _ => unreachable!(),
        };
        assert_eq!(data["body"].as_str().unwrap().as_ptr(), before);
    }
}
