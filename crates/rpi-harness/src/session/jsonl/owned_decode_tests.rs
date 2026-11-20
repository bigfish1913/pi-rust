//! Frozen pre-optimization wrappers exercise the same full-codec validation.
use super::*;
use serde_json::json;

fn legacy_parse_mutation(line: &str) -> Result<SessionMutation, JsonlDecodeError> {
    let value = parse_object(line)?;
    let seq = require_sequence(&value)?;
    match value.get("kind").and_then(|v| v.as_str()) {
        Some("entry") => legacy_parse_entry_mutation(&value, seq),
        Some("record") => legacy_parse_record_mutation(&value, seq),
        Some("lane") => parse_lane_mutation(&value, seq),
        Some("fact") => parse_fact_mutation(&value, seq),
        _ => Err(schema("has unknown mutation kind")),
    }
}

fn legacy_parse_entry_mutation(
    value: &serde_json::Map<String, Value>,
    seq: u64,
) -> Result<SessionMutation, JsonlDecodeError> {
    // `lane` is optional (absent or null on the fork path).
    let lane = match value.get("lane") {
        None => None,
        Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        _ => return Err(schema("has invalid lane")),
    };

    // TS `requireString(value.type, "entry type")` — the *field* is `type`,
    // the validation *label* is "entry type".
    let entry_type = value
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| schema("has invalid entry type"))?;
    if !ENTRY_TYPES.contains(&entry_type) {
        return Err(schema(format!("has unknown entry type {entry_type}")));
    }

    // timestamp must be a non-negative integer (mirrors `requireTimestamp`).
    require_timestamp_map(value, "timestamp")?;

    // `custom` entries must carry `customType`.
    if entry_type == "custom" {
        if value.get("customType").and_then(|v| v.as_str()).is_none() {
            return Err(schema("has invalid customType"));
        }
    }

    // Rebuild the flat entry object (drop `kind` + `lane`) and let the Entry's
    // manual serde do the rest. `parentId` may be null (root entry) — preserved.
    let entry_obj = strip_wrapper_keys(value, &["kind", "lane"]);
    let entry: crate::session::types::entry_decode_tests::LegacyEntry =
        serde_json::from_value(Value::Object(entry_obj)).map_err(|e| schema(e.to_string()))?;

    Ok(SessionMutation::Entry {
        seq,
        timestamp: entry.0.base().timestamp,
        lane,
        entry: entry.0,
    })
}

fn legacy_parse_record_mutation(
    value: &serde_json::Map<String, Value>,
    seq: u64,
) -> Result<SessionMutation, JsonlDecodeError> {
    let record_type = value
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| schema("has invalid record type"))?;
    if !RECORD_TYPES.contains(&record_type) {
        return Err(schema(format!("has unknown record type {record_type}")));
    }
    // Validate the record-shape-specific checks before handing off to serde.
    if record_type == "operation_started" {
        let intent = value
            .get("intent")
            .ok_or_else(|| schema("has invalid intent"))?;
        let intent_obj = intent
            .as_object()
            .ok_or_else(|| schema("has invalid intent"))?;
        let op_kind = intent_obj
            .get("kind")
            .and_then(|v| v.as_str())
            .ok_or_else(|| schema("has invalid operation kind"))?;
        if !OPERATION_KINDS.contains(&op_kind) {
            return Err(schema(format!("has unknown operation kind {op_kind}")));
        }
    }
    if record_type == "operation_finished" {
        if value.get("runId").and_then(|v| v.as_str()).is_none() {
            return Err(schema("has invalid runId"));
        }
    }

    // `{kind:"record", ...record}` → drop `kind`, keep the rest as the record.
    let record_obj = strip_wrapper_keys(value, &["kind"]);
    let record: LaneRecord =
        serde_json::from_value(Value::Object(record_obj)).map_err(|e| schema(e.to_string()))?;
    let _ = seq; // seq lives inside the record's base; not re-stamped by decode.
    Ok(SessionMutation::Record { record })
}

fn assert_parity(line: &str) {
    let reference = legacy_parse_mutation(line);
    let actual = parse_mutation(line);
    match (reference, actual) {
        (Ok(expected), Ok(actual)) => assert_eq!(actual, expected, "{line}"),
        (Err(expected), Err(actual)) => {
            assert_eq!(actual.kind, expected.kind, "{line}");
            assert_eq!(actual.message, expected.message, "{line}");
        }
        (reference, actual) => panic!("different result for {line}: {reference:?} vs {actual:?}"),
    }
}

fn rows() -> Vec<Value> {
    let mut entries = crate::session::types::entry_decode_tests::fixtures();
    for entry in &mut entries {
        entry["kind"] = json!("entry");
        entry["lane"] = json!("main");
    }
    entries.extend([
        json!({"kind":"record", "type":"assistant_frame", "id":"frame", "seq":1, "timestamp":1,
            "lane":"main", "runId":"run", "streamIndex":0, "op":"append", "frame":{"nested":[1,null,true]}}),
        json!({"kind":"record", "type":"operation_started", "id":"run", "seq":1, "timestamp":1,
            "lane":"main", "intent":{"kind":"navigation", "targetId":null, "summarize":false}}),
        json!({"kind":"record", "type":"operation_finished", "id":"finished", "seq":1, "timestamp":1,
            "lane":"main", "runId":"run", "outcome":"completed"}),
    ]);
    entries
}

#[test]
fn owned_mutation_wrappers_keep_schema_validation_and_error_order() {
    for row in rows() {
        legacy_parse_mutation(&row.to_string()).expect("valid reference fixture");
        assert_parity(&row.to_string());
        for key in row.as_object().unwrap().keys() {
            let mut missing = row.clone();
            missing.as_object_mut().unwrap().shift_remove(key);
            assert_parity(&missing.to_string());
            for replacement in [
                json!(null),
                json!(false),
                json!(-1),
                json!(1.5),
                json!(""),
                json!([]),
                json!({}),
            ] {
                let mut variant = row.clone();
                variant[key] = replacement;
                assert_parity(&variant.to_string());
            }
        }
        let mut invalid = row.clone();
        invalid["lane"] = json!(42);
        invalid["type"] = json!("unknown");
        invalid["timestamp"] = json!(-1);
        assert_parity(&invalid.to_string());
    }
}

#[test]
fn owned_mutation_wrappers_preserve_duplicate_keys_key_order_and_raw_value_protocol() {
    for row in rows() {
        let json = row.to_string();
        // Duplicate keys use Value's last-wins semantics, including known tags.
        for key in [
            "kind",
            "type",
            "seq",
            "message",
            "timestamp",
            "lane",
            "unknown",
        ] {
            assert_parity(&format!("{{\"{key}\":null,{}", &json[1..]));
            assert_parity(&format!("{},\"{key}\":null}}", &json[..json.len() - 1]));
        }
        let mut reverse = row.as_object().unwrap().iter().collect::<Vec<_>>();
        reverse.reverse();
        let reverse: serde_json::Map<String, Value> = reverse
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        assert_parity(&Value::Object(reverse).to_string());
        // A private key can become first after dropping kind/lane. Removal must
        // be stable, and the owned path must retain Value normalization/errors.
        for raw in ["null", "not-json", "{\"x\":1}"] {
            let special = serde_json::to_string(raw).unwrap();
            assert_parity(&format!("{{\"kind\":\"entry\",\"lane\":null,\"$serde_json::private::RawValue\":{special},{}", &json[1..]));
            let mut nested = row.clone();
            nested["future"] = json!([{"$serde_json::private::RawValue":raw}]);
            assert_parity(&nested.to_string());
        }
    }
    for line in [
        "{",
        "[]",
        "null",
        "{}",
        "{\"kind\":\"entry\",\"seq\":1e400}",
        "{\"kind\":\"entry\",\"seq\":1.0}",
    ] {
        assert_parity(line);
    }
}

#[test]
#[ignore = "release-mode codec probe; set RPI_CODEC_BENCH_TRANSCRIPT to a transcript file"]
fn benchmark_owned_codec_against_frozen_decoder_on_real_transcript() {
    let path = std::env::var("RPI_CODEC_BENCH_TRANSCRIPT").expect("RPI_CODEC_BENCH_TRANSCRIPT");
    let transcript = std::fs::read_to_string(path).unwrap();
    let lines: Vec<_> = transcript
        .lines()
        .skip(1)
        .filter(|line| !line.is_empty())
        .collect();
    // Read-only: no session open, lease, repair, or provider interaction.
    for line in &lines {
        assert_parity(line);
    }
    for round in 0..5 {
        for owned in if round % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let started = std::time::Instant::now();
            for line in &lines {
                let mutation = if owned {
                    parse_mutation(line)
                } else {
                    legacy_parse_mutation(line)
                };
                std::hint::black_box(mutation.unwrap());
            }
            eprintln!(
                "round={round} owned={owned} rows={} elapsed_ms={:.3}",
                lines.len(),
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
}

#[test]
#[ignore = "release-mode stage probe; set RPI_CODEC_BENCH_TRANSCRIPT"]
fn benchmark_mutation_decode_stages_on_real_transcript() {
    let path = std::env::var("RPI_CODEC_BENCH_TRANSCRIPT").expect("RPI_CODEC_BENCH_TRANSCRIPT");
    let transcript = std::fs::read_to_string(path).unwrap();
    // Use streaming's frame recognizer only as a selection filter, not a full
    // replay (duplicate/backward sequence handling is not modeled here).
    // This read-only probe excludes recognized cold frames and measures only
    // canonical codec rows, without storage open, leases or transcript writes.
    let lines: Vec<_> = transcript
        .lines()
        .skip(1)
        .filter(|line| super::super::cold_decode::parse_cold_frame(line).is_none())
        .collect();
    assert!(!lines.is_empty());
    for line in &lines {
        parse_mutation(line).unwrap();
    }
    for round in 0..5 {
        let mut parse_ns = 0u128;
        let mut typed_ns = 0u128;
        let mut drop_ns = 0u128;
        for line in &lines {
            let started = std::time::Instant::now();
            let value = parse_object(line).unwrap();
            parse_ns += started.elapsed().as_nanos();
            let started = std::time::Instant::now();
            let seq = require_sequence(&value).unwrap();
            let mutation = match value.get("kind").and_then(|v| v.as_str()) {
                Some("entry") => parse_entry_mutation(value, seq),
                Some("record") => parse_record_mutation(value, seq),
                Some("lane") => parse_lane_mutation(&value, seq),
                Some("fact") => parse_fact_mutation(&value, seq),
                _ => panic!("unknown mutation kind in accepted fixture"),
            }
            .unwrap();
            typed_ns += started.elapsed().as_nanos();
            let started = std::time::Instant::now();
            drop(std::hint::black_box(mutation));
            drop_ns += started.elapsed().as_nanos();
        }
        eprintln!(
            "round={round} rows={} parse_value_ms={:.3} typed_decode_ms={:.3} drop_ms={:.3}",
            lines.len(),
            parse_ns as f64 / 1e6,
            typed_ns as f64 / 1e6,
            drop_ns as f64 / 1e6
        );
    }
}
