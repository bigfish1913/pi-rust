//! Reusable native-durable-v1 wire payload codec (prototype).
//!
//! A host must explicitly opt in, bind a `DurableService` with trusted identity,
//! and own routing, bounded in-flight admission, framing and response delivery.
//! This module has no CLI, actor inference, workers, second writer/reducer,
//! extension fallback or migration. It does not install model context or notices.
//! Raw input must go through `parse_rpc` (or `parse_json` before `decode_rpc`).
//! `execute_rpc` returns the versioned durable payload, not a transport envelope.
//! Do not replace unknown/uncertain outcomes with a fresh mutation identity.
//!
//! This is a shared protocol foundation, not an ACP/Web implementation or a
//! production cross-process/store/lifecycle guarantee.
//!
//! ```
//! use rpi_harness::durable::{DurableService, protocol::{self, Failure}};
//! # async fn host_request(service: &DurableService, host_enabled: bool) -> Result<(), Failure> {
//! let request = protocol::parse_rpc(
//!     r#"{"type":"get_durable_state","version":1}"#, host_enabled,
//! )?;
//! let payload = protocol::execute_rpc(service, request).await?;
//! assert_eq!(payload["version"], 1);
//! # Ok(())
//! # }
//! ```

use super::{DurableError, DurableExecutionCursor, DurableMutation, DurableService};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::fmt;
use std::io::{self, Write};
use tokio_util::sync::CancellationToken;

/// Raw-frame and serialized request limit, including whitespace on raw input.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
/// Durable payload limit; transports must also budget their framing/duplication.
pub const MAX_RESPONSE_BYTES: usize = 480 * 1024;
/// Serialized correlation ID limit (not the mutation `requestId` limit).
pub const MAX_ID_BYTES: usize = 4096;
/// Complete v1 optimistic-concurrency guard fields; no optional partial scope.
pub const SCOPE_FIELDS: &[&str] = &[
    "sessionId",
    "lane",
    "bindingToken",
    "leafId",
    "historyRevision",
    "historyGeneration",
    "entryId",
    "controlRevision",
];

/// Sanitized wire failure; core diagnostic extras are never reflected.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Failure {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}
impl Failure {
    /// Construct a host-defined failure from trusted, bounded literals.
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            request_id: None,
        }
    }
    pub fn busy() -> Self {
        Self::new(
            "busy",
            "Native durable request capacity exhausted; no mutation admitted",
        )
    }
    fn invalid() -> Self {
        Self::new("invalid_request", "Invalid native durable request")
    }
    fn with_request(mut self, request_id: &str) -> Self {
        // Never reflect unbounded/untrusted error extras.
        if valid_identity(request_id) {
            self.request_id = Some(request_id.into());
        }
        self
    }
}
impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Failure {}

impl From<DurableError> for Failure {
    fn from(error: DurableError) -> Self {
        let (code, message) = match error {
            DurableError::Closed => ("closed", "Durable owner is closed"),
            DurableError::Sealed => ("sealed", "Durable lifecycle is sealed"),
            DurableError::Recovering => ("recovering", "Native recovery blocks durable access"),
            DurableError::Conflict => ("conflict", "Durable scope or request conflicts"),
            DurableError::PermissionDenied => ("permission_denied", "Durable actor is not authorized"),
            DurableError::InvalidRequest(_) => ("invalid_request", "Invalid native durable request"),
            DurableError::InvalidJournal(_) => ("invalid_journal", "Invalid native durable journal"),
            DurableError::UnsupportedJournalFormat => ("unsupported_journal", "Unsupported durable journal format"),
            DurableError::LimitExceeded => ("limit_exceeded", "Durable bounded budget exceeded"),
            DurableError::ReadFailure(_) => ("read_failure", "Durable observation failed"),
            DurableError::CommitUncertain(_) => ("commit_uncertain", "Commit may have completed; inspect the receipt or retry the identical guarded request"),
            DurableError::ExecutionUnavailable(_) => ("execution_unavailable", "Native execution is unavailable on this branch"),
            DurableError::Execution(_) => ("execution_error", "Durable execution control rejected the request"),
            DurableError::Control(_) => ("control_error", "Durable workflow or queue rejected the request"),
        };
        Self::new(code, message)
    }
}

struct BudgetWriter {
    size: usize,
    limit: usize,
}
impl Write for BudgetWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.size) {
            return Err(io::Error::other("durable serialization budget exceeded"));
        }
        self.size += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
/// Count serialized bytes without allocating the complete body. This is a
/// cooperative serialization budget, not a hard CPU or wall-clock bound.
pub fn fits(value: &impl Serialize, limit: usize) -> bool {
    serde_json::to_writer(&mut BudgetWriter { size: 0, limit }, value).is_ok()
}
/// Budget a normalized request. This cannot recover raw whitespace/duplicates.
pub fn request_budget(value: &Value) -> Result<(), Failure> {
    if fits(value, MAX_REQUEST_BYTES) {
        Ok(())
    } else {
        Err(Failure::new(
            "limit_exceeded",
            "Durable request exceeds 64 KiB",
        ))
    }
}
/// Budget a payload or an adapter's wrapped result without truncation. Supply
/// the committed mutation identity for a late bound so failure remains uncertain.
pub fn response_budget(value: Value, committed_request: Option<&str>) -> Result<Value, Failure> {
    if fits(&value, MAX_RESPONSE_BYTES) {
        return Ok(value);
    }
    Err(match committed_request {
        Some(id) => Failure::new(
            "commit_uncertain",
            "Mutation completed but its response exceeds 480 KiB; inspect the receipt",
        )
        .with_request(id),
        None => Failure::new("limit_exceeded", "Durable response exceeds 480 KiB"),
    })
}

// Value's ordinary Deserialize collapses duplicate keys. This visitor rejects
// them at every depth, including host metadata and otherwise unused fields.
struct StrictValue(Value);
impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct StrictVisitor;
        impl<'de> Visitor<'de> for StrictVisitor {
            type Value = StrictValue;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("JSON without duplicate object keys")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(StrictValue(v.into()))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(StrictValue(v.into()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(StrictValue(v.into()))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(v)
                    .map(|n| StrictValue(Value::Number(n)))
                    .ok_or_else(|| E::custom("invalid number"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(StrictValue(v.into()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(StrictValue(v.into()))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::Null))
            }
            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(StrictValue(value)) = seq.next_element()? {
                    values.push(value);
                }
                Ok(StrictValue(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut values = Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(de::Error::custom("duplicate object key"));
                    }
                    let StrictValue(value) = map.next_value()?;
                    values.insert(key, value);
                }
                Ok(StrictValue(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(StrictVisitor)
    }
}

/// Bounded JSON parser rejecting duplicate keys recursively, including escaped
/// aliases and metadata. For an RPC frame prefer the combined `parse_rpc`.
pub fn parse_json(raw: &str) -> Result<Value, Failure> {
    if raw.len() > MAX_REQUEST_BYTES {
        return Err(Failure::new(
            "limit_exceeded",
            "Durable request exceeds 64 KiB",
        ));
    }
    serde_json::from_str::<StrictValue>(raw)
        .map(|v| v.0)
        .map_err(|_| Failure::invalid())
}
fn object(value: &Value) -> Result<&Map<String, Value>, Failure> {
    value.as_object().ok_or_else(Failure::invalid)
}
fn keys(map: &Map<String, Value>, allowed: &[&str]) -> Result<(), Failure> {
    if map.keys().all(|key| allowed.contains(&key.as_str())) {
        Ok(())
    } else {
        Err(Failure::invalid())
    }
}
fn string<'a>(map: &'a Map<String, Value>, key: &str) -> Result<&'a str, Failure> {
    map.get(key)
        .and_then(Value::as_str)
        .ok_or_else(Failure::invalid)
}
/// V1 wire request identity grammar: 1–128 ASCII letters, digits or `._-`.
pub fn valid_identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}
fn request_id(map: &Map<String, Value>) -> Result<&str, Failure> {
    let id = string(map, "requestId")?;
    if valid_identity(id) {
        Ok(id)
    } else {
        Err(Failure::invalid())
    }
}
fn action_fields(action: &str) -> Result<&'static [&'static str], Failure> {
    Ok(match action {
        "status" | "queue" | "resume" | "complete" | "reset" | "recover" | "forget_run" => &[],
        "start" => &["name", "steps"],
        "checkpoint" => &["step", "status", "note"],
        "pause" | "fail" => &["reason"],
        "enqueue" => &["text"],
        "claim" => &["item"],
        "finish" => &["item", "status", "note"],
        "cancel" => &["item", "note"],
        _ => return Err(Failure::invalid()),
    })
}
fn validate_action(value: &Value) -> Result<&str, Failure> {
    let map = object(value)?;
    let action = string(map, "action")?;
    let mut allowed = vec!["action"];
    allowed.extend_from_slice(action_fields(action)?);
    keys(map, &allowed)?;
    if action == "start" {
        if let Some(steps) = map.get("steps").and_then(Value::as_array) {
            for step in steps {
                keys(object(step)?, &["id", "title"])?;
            }
        }
    }
    Ok(action)
}
fn validate_expected(value: &Value) -> Result<(), Failure> {
    let map = object(value)?;
    keys(map, SCOPE_FIELDS)?;
    if SCOPE_FIELDS.iter().all(|field| map.contains_key(*field)) {
        Ok(())
    } else {
        Err(Failure::invalid())
    }
}
/// Check cancellation before admission. This never cancels a committed mutation.
pub fn cancellation(signal: Option<&CancellationToken>) -> Result<(), Failure> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        Err(Failure::new(
            "cancelled",
            "Durable invocation cancelled before admission",
        ))
    } else {
        Ok(())
    }
}
/// Race only a read or guard observation against cancellation, without spawning.
/// Do not use this helper to imply rollback or non-admission of a mutation.
pub async fn cancellable_read<T>(
    signal: Option<&CancellationToken>,
    read: impl std::future::Future<Output = Result<T, Failure>>,
) -> Result<T, Failure> {
    match signal {
        Some(signal) => tokio::select! {
            biased;
            _ = signal.cancelled() => { cancellation(Some(signal))?; unreachable!() },
            result = read => result,
        },
        None => read.await,
    }
}

async fn observe(service: &DurableService, run_id: Option<&Value>) -> Result<Value, Failure> {
    let value = match run_id {
        Some(value) => {
            let id = value.as_str().ok_or_else(Failure::invalid)?;
            json!({"version":1, "state":service.execution(id).await.map_err(Failure::from)?})
        }
        None => json!({"version":1, "state":service.snapshot().await.map_err(Failure::from)?}),
    };
    response_budget(value, None)
}
async fn execution_page(
    service: &DurableService,
    cursor: Option<&Value>,
) -> Result<Value, Failure> {
    // Null and omission explicitly start a new traversal. A supplied position is
    // unsigned and does not authorize another lane, binding or branch.
    let cursor: Option<DurableExecutionCursor> = match cursor {
        None | Some(Value::Null) => None,
        Some(value) => {
            let map = object(value)?;
            keys(map, &["scope", "beforeSeq"])?;
            validate_expected(map.get("scope").ok_or_else(Failure::invalid)?)?;
            Some(serde_json::from_value(value.clone()).map_err(|_| Failure::invalid())?)
        }
    };
    let page = service
        .execution_page(cursor.as_ref())
        .await
        .map_err(Failure::from)?;
    // One service call, no implicit continuation or response truncation. This is
    // read-only, so an oversized payload is never a commit-uncertain result.
    response_budget(json!({"version":1, "page":page}), None)
}
async fn lookup(service: &DurableService, id: &str) -> Result<Value, Failure> {
    let receipt = service
        .request_outcome(id)
        .await
        .map_err(|e| Failure::from(e).with_request(id))?;
    response_budget(json!({"version":1, "receipt":receipt}), None)
}

async fn mutate(
    service: &DurableService,
    value: Value,
    signal: Option<&CancellationToken>,
) -> Result<Value, Failure> {
    let id = request_id(object(&value)?)?.to_owned();
    let result = async {
        let map = object(&value)?;
        validate_expected(map.get("expected").ok_or_else(Failure::invalid)?)?;
        let action = validate_action(map.get("action").ok_or_else(Failure::invalid)?)?;
        if matches!(action, "status" | "queue") {
            return Err(Failure::invalid());
        }
        let mutation: DurableMutation =
            serde_json::from_value(value).map_err(|_| Failure::invalid())?;
        cancellation(signal)?;
        let outcome = service.mutate(mutation).await.map_err(Failure::from)?;
        // A successful service call has already committed; a late wire bound
        // must never suggest the caller can safely issue a fresh request ID.
        response_budget(json!({"version":1, "outcome":outcome}), Some(&id))
    }
    .await;
    result.map_err(|failure| failure.with_request(&id))
}

/// An envelope validated by `decode_rpc` or `parse_rpc`, not an authorization grant.
/// Payload/action validation and journal admission still happen at execution.
#[derive(Clone, Debug)]
pub struct RpcRequest {
    kind: String,
    value: Value,
}

/// Strict raw-frame entry point. The host supplies `enabled`; JSON cannot opt in.
pub fn parse_rpc(raw: &str, enabled: bool) -> Result<RpcRequest, Failure> {
    decode_rpc(&parse_json(raw)?, enabled)
}

/// Validate the envelope without binding or reading a service.
///
/// Use `parse_rpc` for untrusted raw JSON. This lower-level form is for transports
/// that already used `parse_json`; an ordinary `Value` has lost duplicate keys.
pub fn decode_rpc(request: &Value, enabled: bool) -> Result<RpcRequest, Failure> {
    request_budget(request)?;
    let map = object(request)?;
    if let Some(id) = map.get("id") {
        if !fits(id, MAX_ID_BYTES) {
            return Err(Failure::new(
                "limit_exceeded",
                "Durable RPC id exceeds 4096 bytes",
            ));
        }
    }
    if map.get("version").and_then(Value::as_u64) != Some(1) {
        return Err(Failure::invalid());
    }
    let kind = string(map, "type")?;
    let allowed: &[&str] = match kind {
        "get_durable_state" => &["id", "type", "version", "runId"],
        "get_durable_runs" => &["id", "type", "version", "cursor"],
        "durable_mutate" => &[
            "id",
            "type",
            "version",
            "requestId",
            "expected",
            "action",
            "runId",
        ],
        "get_durable_request" => &["id", "type", "version", "requestId"],
        _ => return Err(Failure::invalid()),
    };
    keys(map, allowed)?;
    if !enabled {
        return Err(Failure::new(
            "disabled",
            "Native durable capability is disabled",
        ));
    }
    Ok(RpcRequest {
        kind: kind.into(),
        value: request.clone(),
    })
}

/// Execute against a service bound by the trusted host, never a wire-supplied
/// actor/lane. No retries, routing, capacity pool or alternate state are created.
/// Dropping this future does not undo an admitted core-owned mutation.
pub async fn execute_rpc(service: &DurableService, request: RpcRequest) -> Result<Value, Failure> {
    let map = request.value.as_object().expect("validated envelope");
    match request.kind.as_str() {
        "get_durable_state" => observe(service, map.get("runId")).await,
        "get_durable_runs" => execution_page(service, map.get("cursor")).await,
        "get_durable_request" => {
            let id = request_id(map)?;
            lookup(service, id).await
        }
        "durable_mutate" => {
            let mut payload = map.clone();
            for field in ["id", "type", "version"] {
                payload.remove(field);
            }
            mutate(service, Value::Object(payload), None).await
        }
        _ => unreachable!("validated command"),
    }
}

/// Legacy flat tool/slash shape. Only __rpi host metadata is ignored; it never
/// supplies identity. Expected may be omitted here (sampled once), but RPC always
/// requires the full guard. Callers must bind the service from trusted host state.
/// Cancellation is raced only against observations/guard sampling, never against
/// an admitted mutation. No receipt means no promise that a retry is safe.
pub async fn flat_request(
    service: &DurableService,
    params: Value,
    signal: Option<&CancellationToken>,
) -> Result<Value, Failure> {
    cancellation(signal)?;
    request_budget(&params)?;
    let mut map = object(&params)?.clone();
    map.remove("__rpi");
    let action = match map.get("action") {
        Some(v) => v.as_str().ok_or_else(Failure::invalid)?,
        None => "status",
    }
    .to_owned();
    let mut allowed = vec!["action"];
    allowed.extend_from_slice(action_fields(&action)?);
    if action == "status" {
        allowed.extend_from_slice(&["runId", "requestId"]);
    }
    if !matches!(action.as_str(), "status" | "queue") {
        allowed.extend_from_slice(&["requestId", "expected"]);
        if matches!(action.as_str(), "recover" | "forget_run") {
            allowed.push("runId");
        }
    }
    keys(&map, &allowed)?;
    if action == "status" && map.contains_key("requestId") {
        if map.contains_key("runId") {
            return Err(Failure::invalid());
        }
        let id = request_id(&map)?;
        return cancellable_read(signal, lookup(service, id)).await;
    }
    if matches!(action.as_str(), "status" | "queue") {
        return cancellable_read(signal, observe(service, map.get("runId"))).await;
    }
    let id = request_id(&map)?.to_owned();
    let result = async {
        let expected = match map.remove("expected") {
            Some(expected) => {
                validate_expected(&expected)?;
                expected
            }
            None => serde_json::to_value(
                cancellable_read(signal, async {
                    service.snapshot().await.map_err(Failure::from)
                })
                .await?
                .scope,
            )
            .map_err(|_| Failure::invalid())?,
        };
        let mut payload = Map::new();
        payload.insert(
            "requestId".into(),
            map.remove("requestId").expect("validated ID"),
        );
        payload.insert("expected".into(), expected);
        if let Some(run_id) = map.remove("runId") {
            payload.insert("runId".into(), run_id);
        }
        map.insert("action".into(), Value::String(action));
        payload.insert("action".into(), Value::Object(map));
        mutate(service, Value::Object(payload), signal).await
    }
    .await;
    result.map_err(|e| e.with_request(&id))
}
