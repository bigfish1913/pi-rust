//! Experimental, pure, read-only local-candidate decoder; not an authenticated journal.
//!
//! A successful decode proves only normalized field form, local state/receipt
//! consistency, newly staged node shapes/references, and the current request's
//! lookup path through those local nodes. The immutable policy selects exactly
//! one caller-bound epoch; it does not restrict receipts to a single actor.
//! Older references and their summaries are not authenticated, off-path search
//! ordering is not proved, and neither predecessor history nor state transitions
//! are replayed. Arbitrary locally valid state transitions remain possible.
//! SHA-256 does not authenticate an untrusted root, epoch, receipt, or history.
//!
//! There is no Session, storage, writer, runtime, snapshot conversion, admission,
//! or production authorization here. Limits are logical raw/serialization/node
//! budgets, not physical-memory, CPU, or allocation guarantees. Existing codecs
//! and their caps are unchanged.
//!
//! Input deliberately requires strict *normalized field form*: after recursive
//! duplicate-key rejection, typed deserialization must serialize back to the
//! same JSON value. Object order and whitespace are immaterial; unknown fields,
//! omitted serialized defaults, aliases, and explicit nulls for skipped options
//! are rejected, including inside the existing permissive control models.

use super::{validate_execution_target, validate_identity, DurableReceipt};
use rpi_durable::control::{ControlAction, ControlState};
use rpi_durable::execution::ExecutionControl;
use rpi_durable::ledger::{
    insert::MAX_CREATED_NODES, Digest, Lookup, LookupStep, Node, NodeHasher, NodeRef, ReceiptRef,
};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};
use std::fmt;
use std::io::{self, Write};

pub mod archive;
pub mod chain;
pub mod control;
pub mod native_run;
pub mod source;

pub const CUSTOM_TYPE: &str = "rpi.durable.control.experimental.v2";
pub const FORMAT: &str = "rpi-native-durable-control-ledger-experimental";
pub const MAX_RECORD_BYTES: usize = 1024 * 1024;
const MAX_RECEIPT_BYTES: usize = 64 * 1024;
const RECEIPT_DOMAIN: &[u8] = b"rpi-durable-ledger-receipt-experimental-2\0";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Epoch {
    pub session_id: String,
    pub lane: String,
    pub epoch_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Previous {
    pub entry_id: String,
    pub ordinal: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Record {
    pub format: String,
    pub version: u32,
    pub epoch: Epoch,
    pub ordinal: u64,
    pub previous: Option<Previous>,
    pub state: ControlState,
    pub execution_control: ExecutionControl,
    pub receipt: DurableReceipt,
    pub nodes: Vec<Node>,
    pub root: NodeRef,
}

/// Fixed caller-selected epoch, not actor authorization or trusted history.
#[derive(Clone, Debug)]
pub struct ReadPolicy {
    epoch: Epoch,
}

impl ReadPolicy {
    pub fn new_epoch(
        session: impl Into<String>,
        lane: impl Into<String>,
        epoch: impl Into<String>,
    ) -> Result<Self, CodecError> {
        let epoch = Epoch {
            session_id: session.into(),
            lane: lane.into(),
            epoch_id: epoch.into(),
        };
        validate_epoch(&epoch)?;
        Ok(Self { epoch })
    }
}

/// Locally checked candidate only. Construction requires [`decode`]; this is
/// deliberately not deserializable and cannot be converted into a snapshot.
#[derive(Clone, Debug)]
pub struct DecodedCandidate {
    record: Record,
}

impl DecodedCandidate {
    pub fn record(&self) -> &Record {
        &self.record
    }
}

/// Closed, sanitized error categories; no untrusted input or model diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CodecError {
    #[error("experimental ledger budget exceeded")]
    Limit,
    #[error("invalid experimental ledger JSON")]
    Json,
    #[error("invalid experimental ledger schema or local state")]
    Schema,
    #[error("invalid experimental ledger identity")]
    Identity,
    #[error("experimental ledger epoch mismatch")]
    Epoch,
    #[error("unsupported experimental ledger format")]
    Format,
    #[error("invalid experimental ledger receipt")]
    Receipt,
    #[error("invalid experimental ledger node batch")]
    Ledger,
    #[error("invalid experimental ledger local proof")]
    Proof,
}

/// SHA-256 over the existing ledger's canonical node bytes. Selecting a real
/// digest does not confer trust on caller-supplied roots or older references.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sha256NodeHasher;

impl NodeHasher for Sha256NodeHasher {
    fn digest(&self, canonical: &[u8]) -> Digest {
        Sha256::digest(canonical).into()
    }
}

fn identity(value: &str) -> Result<(), CodecError> {
    validate_identity(value).map_err(|_| CodecError::Identity)
}

fn validate_epoch(epoch: &Epoch) -> Result<(), CodecError> {
    identity(&epoch.session_id)?;
    identity(&epoch.lane)?;
    identity(&epoch.epoch_id)
}

fn validate_receipt(receipt: &DurableReceipt) -> Result<(), CodecError> {
    identity(&receipt.request_id)?;
    identity(&receipt.actor)?;
    identity(&receipt.entry_id)?;
    let expected = &receipt.expected;
    identity(&expected.session_id)?;
    identity(&expected.lane)?;
    identity(&expected.binding_token)?;
    for id in [&expected.leaf_id, &expected.entry_id]
        .into_iter()
        .flatten()
    {
        identity(id)?;
    }
    validate_execution_target(&receipt.action, receipt.run_id.as_deref())
        .map_err(|_| CodecError::Receipt)?;
    if matches!(receipt.action, ControlAction::Status | ControlAction::Queue) {
        return Err(CodecError::Receipt);
    }
    Ok(())
}

// Count before allocating the complete receipt serialization. This is a
// cooperative serialization bound, not a claim about physical peak memory.
struct ReceiptBudget(usize);

impl Write for ReceiptBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_RECEIPT_BYTES.saturating_sub(self.0) {
            return Err(io::Error::other("receipt budget exceeded"));
        }
        self.0 += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Domain || u32 big-endian length || typed receipt JSON in fixed struct field
/// order (never input object order). The JSON body is limited to 64 KiB before
/// allocating output. Validates receipt identities and action/run target only;
/// it does not authenticate contents or validate a historical transition.
pub fn receipt_canonical_bytes(receipt: &DurableReceipt) -> Result<Vec<u8>, CodecError> {
    validate_receipt(receipt)?;
    serde_json::to_writer(&mut ReceiptBudget(0), receipt).map_err(|_| CodecError::Limit)?;
    let body = serde_json::to_vec(receipt).map_err(|_| CodecError::Schema)?;
    if body.len() > MAX_RECEIPT_BYTES {
        return Err(CodecError::Limit);
    }
    let length = u32::try_from(body.len()).map_err(|_| CodecError::Limit)?;
    let mut bytes = Vec::with_capacity(RECEIPT_DOMAIN.len() + 4 + body.len());
    bytes.extend_from_slice(RECEIPT_DOMAIN);
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(&body);
    Ok(bytes)
}

/// Reference to typed receipt contents; positive ordinal, not history proof.
pub fn receipt_reference(receipt: &DurableReceipt, ordinal: u64) -> Result<ReceiptRef, CodecError> {
    if ordinal == 0 {
        return Err(CodecError::Receipt);
    }
    let bytes = receipt_canonical_bytes(receipt)?;
    Ok(ReceiptRef {
        entry_id: receipt.entry_id.clone(),
        ordinal,
        digest: Sha256::digest(&bytes).into(),
    })
}

/// Decode raw JSON into a local candidate, never a trusted snapshot. Raw length
/// is checked before parsing; the default serde_json recursion limit remains on.
/// The current request must be found using newly staged nodes only. Requiring
/// an older node on that path fails closed rather than invoking any resolver.
pub fn decode(
    raw: &[u8],
    entry_id: &str,
    custom_type: &str,
    policy: &ReadPolicy,
) -> Result<DecodedCandidate, CodecError> {
    if raw.len() > MAX_RECORD_BYTES {
        return Err(CodecError::Limit);
    }
    let mut deserializer = serde_json::Deserializer::from_slice(raw);
    let StrictValue(original) =
        StrictValue::deserialize(&mut deserializer).map_err(|_| CodecError::Json)?;
    deserializer.end().map_err(|_| CodecError::Json)?;
    let record: Record =
        serde_json::from_value(original.clone()).map_err(|_| CodecError::Schema)?;
    if serde_json::to_value(&record).map_err(|_| CodecError::Schema)? != original {
        return Err(CodecError::Schema);
    }
    identity(entry_id)?;
    validate_epoch(&policy.epoch)?;
    validate_epoch(&record.epoch)?;
    if custom_type != CUSTOM_TYPE || record.format != FORMAT || record.version != 2 {
        return Err(CodecError::Format);
    }
    if record.epoch != policy.epoch {
        return Err(CodecError::Epoch);
    }
    record.state.validate().map_err(|_| CodecError::Schema)?;
    record
        .execution_control
        .validate()
        .map_err(|_| CodecError::Schema)?;
    validate_receipt(&record.receipt)?;
    let receipt = &record.receipt;
    let expected = &receipt.expected;
    if receipt.entry_id != entry_id
        || expected.session_id != record.epoch.session_id
        || expected.lane != record.epoch.lane
        || receipt.control_revision != record.state.revision
        || expected
            .control_revision
            .checked_add(u64::from(receipt.changed))
            != Some(record.state.revision)
    {
        return Err(CodecError::Receipt);
    }
    match (record.ordinal, &record.previous) {
        (1, None) if expected.entry_id.is_none() && expected.control_revision == 0 => {}
        (ordinal, Some(previous)) if ordinal > 1 => {
            identity(&previous.entry_id)?;
            if previous.entry_id == entry_id
                || previous.ordinal != ordinal - 1
                || expected.entry_id.as_deref() != Some(previous.entry_id.as_str())
            {
                return Err(CodecError::Receipt);
            }
        }
        _ => return Err(CodecError::Receipt),
    }
    validate_nodes(&record, entry_id)?;
    Ok(DecodedCandidate { record })
}

fn validate_nodes(record: &Record, entry_id: &str) -> Result<(), CodecError> {
    if record.nodes.is_empty() || record.nodes.len() > MAX_CREATED_NODES {
        return Err(CodecError::Ledger);
    }
    let current_receipt = receipt_reference(&record.receipt, record.ordinal)?;
    let hasher = Sha256NodeHasher;
    let mut references = Vec::with_capacity(record.nodes.len());
    for (slot, node) in record.nodes.iter().enumerate() {
        if node.address.entry_id != entry_id
            || node.address.ordinal != record.ordinal
            || usize::from(node.address.slot) != slot
        {
            return Err(CodecError::Ledger);
        }
        // reference() validates the complete local shape before hashing.
        let reference = node.reference(&hasher).map_err(|_| CodecError::Ledger)?;
        if node.value.entry_id == entry_id {
            if node.value != current_receipt {
                return Err(CodecError::Ledger);
            }
        } else if node.value.ordinal >= record.ordinal {
            return Err(CodecError::Ledger);
        }
        for child in [&node.left, &node.right].into_iter().flatten() {
            if child.address.entry_id == entry_id {
                if references.get(usize::from(child.address.slot)) != Some(child) {
                    return Err(CodecError::Ledger);
                }
            } else if child.address.ordinal >= record.ordinal || child.count > record.ordinal - 1 {
                return Err(CodecError::Ledger);
            }
        }
        references.push(reference);
    }
    if record.root.address.entry_id != entry_id
        || record.root.address.ordinal != record.ordinal
        || record.root.count != record.ordinal
        || references.get(usize::from(record.root.address.slot)) != Some(&record.root)
    {
        return Err(CodecError::Ledger);
    }
    let mut lookup = Lookup::new(&record.receipt.request_id, Some(record.root.clone()))
        .map_err(|_| CodecError::Proof)?;
    loop {
        match lookup.step() {
            LookupStep::NeedNode(reference) => {
                if reference.address.entry_id != entry_id
                    || reference.address.ordinal != record.ordinal
                {
                    return Err(CodecError::Proof);
                }
                let node = record
                    .nodes
                    .get(usize::from(reference.address.slot))
                    .ok_or(CodecError::Proof)?;
                lookup
                    .provide(node, &hasher)
                    .map_err(|_| CodecError::Proof)?;
            }
            LookupStep::Found(value) if value == &current_receipt => return Ok(()),
            _ => return Err(CodecError::Proof),
        }
    }
}

// Unlike Value's ordinary Deserialize, this rejects duplicate decoded keys at
// every depth, including escaped spellings of the same key and unused fields.
struct StrictValue(Value);

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct StrictVisitor;
        impl<'de> Visitor<'de> for StrictVisitor {
            type Value = StrictValue;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("JSON without duplicate object keys")
            }
            fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(StrictValue(value.into()))
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(StrictValue(value.into()))
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(StrictValue(value.into()))
            }
            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(value)
                    .map(|number| StrictValue(Value::Number(number)))
                    .ok_or_else(|| E::custom("invalid number"))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(StrictValue(value.into()))
            }
            fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(StrictValue(value.into()))
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
