//! Shared Host control wire v1. Mirrored in rpi-extcore, never linked into a DLL
//! from rpi-cli. Wire declarations do NOT advertise implemented capabilities.
//! This is distinct from both native session RPC and the rpi-server protocol.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

pub const PROTOCOL: &str = "rpi-shared-host";
pub const MAJOR: u16 = 1;
pub const MINOR: u16 = 5;
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
/// Text-result page budget in UTF-8 bytes, independent of catalog page counts.
pub const MAX_RESULT_TEXT_BYTES: usize = 16 * 1024;
pub const MAX_PAGE_SIZE: usize = 128;
/// Bounded text prompt for one submit. Larger inputs need an explicit contract
/// (images/structured content) rather than an unbounded JSON string.
pub const MAX_SUBMIT_PROMPT_BYTES: usize = 256 * 1024;

macro_rules! identity {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(uuid::Uuid);
        impl $name {
            pub fn new() -> Self {
                Self(uuid::Uuid::new_v4())
            }
        }
        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
        impl TryFrom<String> for $name {
            type Error = &'static str;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                let id = uuid::Uuid::parse_str(&value).map_err(|_| "invalid identity")?;
                if id.is_nil() {
                    return Err("nil identity");
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
identity!(HostId);
identity!(Epoch);
identity!(AgentId);
identity!(RequestId);
identity!(CallId);
identity!(TicketId);
identity!(ControlScopeId);
identity!(ProfileVersion);

/// Opaque immutable input identity, never configuration or authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileRef {
    pub agent_id: AgentId,
    pub version: ProfileVersion,
}
/// Complete intent for receipt reconciliation and explicitly matched execution.
/// Its epoch can be historical; the enclosing transport epoch must be current.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdministrativeRequest {
    pub scope: ControlScopeId,
    pub request_id: RequestId,
    pub epoch: Epoch,
    pub action: AdministrativeAction,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdministrativeAction {
    /// Legacy history only, not permission to execute unbound input.
    Create {
        agent_id: AgentId,
    },
    Restore {
        agent_id: AgentId,
        revision: u64,
    },
    CreateProfile {
        profile: ProfileRef,
    },
    RestoreProfile {
        profile: ProfileRef,
        revision: u64,
    },
    Cancel {
        agent_id: AgentId,
        revision: u64,
        ticket_id: TicketId,
    },
    /// Stable submitted input, never an observer's volatile ticket.
    CancelSubmitted {
        agent_id: AgentId,
        revision: u64,
        submitted_request_id: RequestId,
    },
    Close {
        agent_id: AgentId,
        revision: u64,
    },
    Stop {},
}
impl AdministrativeRequest {
    pub fn valid(&self) -> bool {
        match self.action {
            AdministrativeAction::Restore { revision, .. }
            | AdministrativeAction::RestoreProfile { revision, .. }
            | AdministrativeAction::Cancel { revision, .. }
            | AdministrativeAction::CancelSubmitted { revision, .. }
            | AdministrativeAction::Close { revision, .. } => {
                (1..=i64::MAX as u64).contains(&revision)
            }
            _ => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Version {
    pub major: u16,
    pub minor: u16,
}
impl Default for Version {
    fn default() -> Self {
        Self {
            major: MAJOR,
            minor: MINOR,
        }
    }
}

/// Unknown future capabilities are ignored, never treated as permission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Status,
    List,
    Inspect,
    Receipts,
    Results,
    Profiles,
    AdministrativeReceipts,
    AdministrativeCreate,
    AdministrativeRestore,
    AdministrativeClose,
    AdministrativeCancel,
    AdministrativeStop,
    Create,
    Submit,
    Cancel,
    Close,
    Restore,
    Events,
    Approvals,
    Configure,
    Stop,
    Mini,
    #[serde(other)]
    Unknown,
}

/// Serialized only in the authenticated handshake. Neither Debug nor client
/// errors disclose it. Callers must keep serialized frames out of logs.
#[derive(Clone, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AuthToken(String);
impl AuthToken {
    pub fn new(value: String) -> Result<Self, &'static str> {
        if !(32..=128).contains(&value.len())
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err("invalid authentication token");
        }
        Ok(Self(value))
    }
    pub fn matches(&self, other: &Self) -> bool {
        // Bound-independent work for valid tokens; no ordinary string equality
        // early exit. This is not a same-UID process isolation boundary.
        let a = self.0.as_bytes();
        let b = other.0.as_bytes();
        let mut diff = a.len() ^ b.len();
        for i in 0..128 {
            diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
        }
        diff == 0
    }
}
impl TryFrom<String> for AuthToken {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl From<AuthToken> for String {
    fn from(value: AuthToken) -> Self {
        value.0
    }
}
impl fmt::Debug for AuthToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AuthToken([redacted])")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    pub protocol: String,
    pub version: Version,
    pub host_id: HostId,
    pub token: AuthToken,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub max_frame_bytes: usize,
    pub max_in_flight: usize,
    pub max_page_size: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_frame_bytes: MAX_FRAME_BYTES,
            max_in_flight: 64,
            max_page_size: MAX_PAGE_SIZE,
        }
    }
}
impl Limits {
    pub fn valid(&self) -> bool {
        (512..=MAX_FRAME_BYTES).contains(&self.max_frame_bytes)
            && (1..=64).contains(&self.max_in_flight)
            && (1..=MAX_PAGE_SIZE).contains(&self.max_page_size)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Welcome {
    pub protocol: String,
    pub version: Version,
    pub host_id: HostId,
    pub epoch: Epoch,
    pub capabilities: Vec<Capability>,
    pub limits: Limits,
}
impl Welcome {
    pub fn compatible(&self, host: HostId) -> bool {
        self.protocol == PROTOCOL
            && self.version.major == MAJOR
            && self.host_id == host
            && self.limits.valid()
            && self.capabilities.len() <= 64
    }
    pub fn supports(&self, capability: Capability) -> bool {
        capability != Capability::Unknown && self.capabilities.contains(&capability)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Correlation only, never a durable mutation receipt.
    pub call_id: CallId,
    pub epoch: Epoch,
    pub command: Command,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    // An empty struct variant is intentional: serde's internally tagged unit
    // variant otherwise ignores unknown fields even with deny_unknown_fields.
    Status {},
    List {
        after: Option<AgentId>,
        limit: usize,
    },
    Inspect {
        agent_id: AgentId,
    },
    /// Immutable allowed refs only, paginated independently from catalog rows.
    Profiles {
        after: Option<AgentId>,
        limit: usize,
    },
    /// Query only: an embedded Cancel/Stop intent grants no execution capability.
    AdministrativeReceipt {
        request: AdministrativeRequest,
    },
    /// Complete intent. Only CreateProfile is valid, never legacy Create.
    AdministrativeCreate {
        request: AdministrativeRequest,
    },
    /// Complete intent. Only RestoreProfile is valid.
    AdministrativeRestore {
        request: AdministrativeRequest,
    },
    /// Complete intent. Only revision-guarded Close is valid.
    AdministrativeClose {
        request: AdministrativeRequest,
    },
    /// Complete intent. Only guarded CancelSubmitted is valid, never legacy
    /// ticket Cancel. A successful signal is not execution settlement.
    AdministrativeCancel {
        request: AdministrativeRequest,
    },
    /// Complete Stop intent. NativeSettled is not peer receipt or process exit.
    /// Only this owner's fresh, fully closed Stop can retire its listener.
    AdministrativeStop {
        request: AdministrativeRequest,
    },
    /// Read one request's native admission record and execution observation.
    /// Agent-scoped: the native journal belongs to one Agent's Session.
    Receipt {
        agent_id: AgentId,
        request_id: RequestId,
    },
    /// Read a request-linked terminal text result, never an arbitrary run or
    /// the current lane tip. Nonzero offsets require the original run identity.
    Result {
        agent_id: AgentId,
        request_id: RequestId,
        run_id: Option<String>,
        offset: usize,
        max_bytes: usize,
    },
    /// Submit one exact text prompt to an already loaded Agent. The reply is a
    /// durable native admission receipt, not execution completion. Reusing the
    /// request id with identical text is idempotent and never executes twice.
    Submit {
        agent_id: AgentId,
        request_id: RequestId,
        prompt: String,
    },
    /// Guarded administrative mutations. Creation/input delivery are added only
    /// with their binding/admission contract, not as arbitrary JSON dispatch.
    Cancel {
        mutation: AgentMutation,
        ticket_id: TicketId,
    },
    Close {
        mutation: AgentMutation,
    },
    Restore {
        mutation: AgentMutation,
    },
    Stop {
        request_id: RequestId,
    },
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMutation {
    pub request_id: RequestId,
    pub agent_id: AgentId,
    pub revision: u64,
}
impl Command {
    pub fn capability(&self) -> Capability {
        match self {
            Self::Status {} => Capability::Status,
            Self::List { .. } => Capability::List,
            Self::Inspect { .. } => Capability::Inspect,
            Self::Profiles { .. } => Capability::Profiles,
            Self::AdministrativeReceipt { .. } => Capability::AdministrativeReceipts,
            Self::AdministrativeCreate { .. } => Capability::AdministrativeCreate,
            Self::AdministrativeRestore { .. } => Capability::AdministrativeRestore,
            Self::AdministrativeClose { .. } => Capability::AdministrativeClose,
            Self::AdministrativeCancel { .. } => Capability::AdministrativeCancel,
            Self::AdministrativeStop { .. } => Capability::AdministrativeStop,
            Self::Receipt { .. } => Capability::Receipts,
            Self::Result { .. } => Capability::Results,
            Self::Submit { .. } => Capability::Submit,
            Self::Cancel { .. } => Capability::Cancel,
            Self::Close { .. } => Capability::Close,
            Self::Restore { .. } => Capability::Restore,
            Self::Stop { .. } => Capability::Stop,
        }
    }
    pub fn request_id(&self) -> Option<RequestId> {
        match self {
            Self::AdministrativeCreate { request }
            | Self::AdministrativeRestore { request }
            | Self::AdministrativeClose { request }
            | Self::AdministrativeCancel { request }
            | Self::AdministrativeStop { request } => Some(request.request_id),
            Self::Cancel { mutation, .. }
            | Self::Close { mutation }
            | Self::Restore { mutation } => Some(mutation.request_id),
            Self::Submit { request_id, .. } => Some(*request_id),
            Self::Stop { request_id } => Some(*request_id),
            _ => None,
        }
    }
    pub fn valid(&self, limits: Limits) -> bool {
        match self {
            Self::List { limit, .. } | Self::Profiles { limit, .. } => {
                (1..=limits.max_page_size).contains(limit)
            }
            Self::AdministrativeReceipt { request } => request.valid(),
            Self::AdministrativeCreate { request } => {
                request.valid()
                    && matches!(request.action, AdministrativeAction::CreateProfile { .. })
            }
            Self::AdministrativeRestore { request } => {
                request.valid()
                    && matches!(request.action, AdministrativeAction::RestoreProfile { .. })
            }
            Self::AdministrativeClose { request } => {
                request.valid() && matches!(request.action, AdministrativeAction::Close { .. })
            }
            Self::AdministrativeCancel { request } => {
                request.valid()
                    && matches!(request.action, AdministrativeAction::CancelSubmitted { .. })
            }
            Self::AdministrativeStop { request } => {
                request.valid() && matches!(request.action, AdministrativeAction::Stop {})
            }
            Self::Cancel { mutation, .. }
            | Self::Close { mutation }
            | Self::Restore { mutation } => (1..=i64::MAX as u64).contains(&mutation.revision),
            Self::Result {
                run_id,
                offset,
                max_bytes,
                ..
            } => {
                (4..=MAX_RESULT_TEXT_BYTES).contains(max_bytes)
                    && (*offset == 0 || run_id.is_some())
                    && run_id
                        .as_ref()
                        .is_none_or(|id| !id.is_empty() && id.len() <= 128)
            }
            Self::Submit { prompt, .. } => {
                !prompt.trim().is_empty() && prompt.len() <= MAX_SUBMIT_PROMPT_BYTES
            }
            _ => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Unsupported,
    Unauthorized,
    InvalidRequest,
    NotFound,
    Busy,
    Capacity,
    StaleEpoch,
    StaleRevision,
    Conflict,
    ResyncRequired,
    PersistenceUncertain,
    OutcomeUnknown,
    Expired,
    Internal,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    /// Bounded operation-specific projection, not a Harness Debug report or a
    /// replacement transcript. Each service method validates its own schema.
    Data {
        data: Value,
    },
    Error {
        code: ErrorCode,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub call_id: CallId,
    pub epoch: Epoch,
    pub reply: Reply,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn golden_frames_roundtrip_and_reject_ambiguous_mutations() {
        let frames: Value = serde_json::from_str(include_str!("fixtures/control-v1.json")).unwrap();
        let hello: Hello = serde_json::from_value(frames["hello"].clone()).unwrap();
        let welcome: Welcome = serde_json::from_value(frames["welcome"].clone()).unwrap();
        assert!(welcome.compatible(hello.host_id));
        assert!(welcome.supports(Capability::Status));
        assert!(!welcome.supports(Capability::Unknown));
        for value in frames["requests"].as_array().unwrap() {
            let request: Request = serde_json::from_value(value.clone()).unwrap();
            assert!(request.command.valid(welcome.limits));
            assert_eq!(serde_json::to_value(request).unwrap(), *value);
        }
        let response: Response = serde_json::from_value(frames["response"].clone()).unwrap();
        assert_eq!(serde_json::to_value(response).unwrap(), frames["response"]);
        let cancel = frames["requests"]
            .as_array()
            .unwrap()
            .iter()
            .position(|value| value["command"]["op"] == "cancel")
            .expect("golden fixture carries a cancel mutation");
        let mut request = frames["requests"][cancel].clone();
        request["command"]["mutation"]["revision"] = 0.into();
        assert!(!serde_json::from_value::<Request>(request.clone())
            .unwrap()
            .command
            .valid(welcome.limits));
        request["command"]["mutation"]
            .as_object_mut()
            .unwrap()
            .remove("request_id");
        assert!(serde_json::from_value::<Request>(request).is_err());
        assert!(
            serde_json::from_str::<CallId>("\"00000000-0000-0000-0000-000000000000\"").is_err()
        );
        assert!(
            serde_json::from_str::<Command>(r#"{"op":"status","argv":["--approve"]}"#).is_err()
        );
        let agent: AgentId = "66666666-6666-4666-8666-666666666666"
            .to_string()
            .try_into()
            .unwrap();
        let request: RequestId = "99999999-9999-4999-8999-999999999999"
            .to_string()
            .try_into()
            .unwrap();
        for prompt in [String::new(), "  \n ".into()] {
            assert!(!Command::Submit {
                agent_id: agent,
                request_id: request,
                prompt
            }
            .valid(welcome.limits));
        }
        assert!(!Command::Submit {
            agent_id: agent,
            request_id: request,
            prompt: "x".repeat(MAX_SUBMIT_PROMPT_BYTES + 1),
        }
        .valid(welcome.limits));
        assert!(Command::Submit {
            agent_id: agent,
            request_id: request,
            prompt: "hello".into(),
        }
        .valid(welcome.limits));
    }
    #[test]
    fn result_pages_are_read_only_bounded_and_require_original_run_after_offset_zero() {
        let agent_id = AgentId::new();
        let request_id = RequestId::new();
        let make = |run_id: Option<String>, offset, max_bytes| Command::Result {
            agent_id,
            request_id,
            run_id,
            offset,
            max_bytes,
        };
        let command = make(None, 0, MAX_RESULT_TEXT_BYTES);
        assert_eq!(command.capability(), Capability::Results);
        assert!(command.request_id().is_none());
        assert!(command.valid(Limits::default()));
        assert!(!make(None, 1, 4096).valid(Limits::default()));
        assert!(!make(Some(String::new()), 0, 4096).valid(Limits::default()));
        assert!(!make(Some("x".repeat(129)), 0, 4096).valid(Limits::default()));
        assert!(!make(None, 0, 3).valid(Limits::default()));
        assert!(!make(None, 0, MAX_RESULT_TEXT_BYTES + 1).valid(Limits::default()));
        assert!(make(Some("native-run".into()), usize::MAX, 4).valid(Limits::default()));
        assert!(serde_json::from_str::<Command>(r#"{"op":"result","agent_id":"66666666-6666-4666-8666-666666666666","request_id":"99999999-9999-4999-8999-999999999999","offset":0,"max_bytes":4096,"path":"/must/not/read"}"#).is_err());
    }
    #[test]
    fn token_debug_and_errors_never_reveal_material() {
        let token = AuthToken::new("fixture-only-auth-token-0123456789".into()).unwrap();
        assert_eq!(format!("{token:?}"), "AuthToken([redacted])");
        assert!(token.matches(&token.clone()));
        assert!(
            !token.matches(&AuthToken::new("fixture-only-auth-token-9876543210".into()).unwrap())
        );
        assert!(AuthToken::new("private/invalid\nvalue".into()).is_err());
    }
    #[test]
    fn future_minor_and_unknown_capability_do_not_grant_authority() {
        let mut welcome: Welcome = serde_json::from_value(
            serde_json::from_str::<Value>(include_str!("fixtures/control-v1.json")).unwrap()
                ["welcome"]
                .clone(),
        )
        .unwrap();
        welcome.version.minor = u16::MAX;
        welcome
            .capabilities
            .push(serde_json::from_str("\"future_capability\"").unwrap());
        assert!(welcome.compatible(welcome.host_id));
        assert!(!welcome.supports(Capability::Unknown));
        welcome.version.major += 1;
        assert!(!welcome.compatible(welcome.host_id));
        assert!(!Limits {
            max_frame_bytes: usize::MAX,
            ..Limits::default()
        }
        .valid());
    }
    #[test]
    fn administrative_queries_are_closed_bounded_and_never_claim_mutation_identity() {
        let agent = AgentId::new();
        let profile = ProfileRef {
            agent_id: agent,
            version: ProfileVersion::new(),
        };
        for action in [
            AdministrativeAction::Create { agent_id: agent },
            AdministrativeAction::Restore {
                agent_id: agent,
                revision: 1,
            },
            AdministrativeAction::CreateProfile { profile },
            AdministrativeAction::RestoreProfile {
                profile,
                revision: 1,
            },
            AdministrativeAction::Cancel {
                agent_id: agent,
                revision: 1,
                ticket_id: TicketId::new(),
            },
            AdministrativeAction::Close {
                agent_id: agent,
                revision: 1,
            },
            AdministrativeAction::Stop {},
        ] {
            let request = AdministrativeRequest {
                scope: ControlScopeId::new(),
                request_id: RequestId::new(),
                epoch: Epoch::new(),
                action,
            };
            let encoded = serde_json::to_value(&request).unwrap();
            assert!(serde_json::to_vec(&request).unwrap().len() < 4096);
            assert_eq!(
                serde_json::from_value::<AdministrativeRequest>(encoded.clone()).unwrap(),
                request
            );
            let command = Command::AdministrativeReceipt { request };
            assert!(command.valid(Limits::default()));
            assert_eq!(command.request_id(), None);
            assert_eq!(command.capability(), Capability::AdministrativeReceipts);
            let mut injected = encoded;
            injected["action"]["grant"] = serde_json::json!({"stop": true});
            assert!(serde_json::from_value::<AdministrativeRequest>(injected).is_err());
        }
        for limit in [0, MAX_PAGE_SIZE + 1, usize::MAX] {
            assert!(!Command::Profiles { after: None, limit }.valid(Limits::default()));
        }
        assert_eq!(
            Command::Profiles {
                after: None,
                limit: 1
            }
            .request_id(),
            None
        );
        assert!(
            serde_json::from_str::<ProfileVersion>("\"00000000-0000-0000-0000-000000000000\"")
                .is_err()
        );
    }
    #[test]
    fn administrative_receipt_revision_limits_and_old_epoch_are_independent_from_transport() {
        for revision in [0, i64::MAX as u64 + 1, u64::MAX] {
            let request = AdministrativeRequest {
                scope: ControlScopeId::new(),
                request_id: RequestId::new(),
                epoch: Epoch::new(),
                action: AdministrativeAction::Close {
                    agent_id: AgentId::new(),
                    revision,
                },
            };
            assert!(!request.valid());
        }
        let frames: Value = serde_json::from_str(include_str!("fixtures/control-v1.json")).unwrap();
        let frame: Request = serde_json::from_value(
            frames["requests"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()
                .clone(),
        )
        .unwrap();
        let Command::AdministrativeReceipt { request } = frame.command else {
            panic!("expected administrative query");
        };
        assert_ne!(frame.epoch, request.epoch);
        assert!(request.valid());
    }
    #[test]
    fn administrative_cancel_binds_stable_input_and_rejects_hybrid_or_unbounded_intents() {
        let request = AdministrativeRequest {
            scope: ControlScopeId::new(),
            request_id: RequestId::new(),
            epoch: Epoch::new(),
            action: AdministrativeAction::CancelSubmitted {
                agent_id: AgentId::new(),
                revision: 1,
                submitted_request_id: RequestId::new(),
            },
        };
        let command = Command::AdministrativeCancel {
            request: request.clone(),
        };
        assert!(command.valid(Limits::default()));
        assert_eq!(command.request_id(), Some(request.request_id));
        assert_eq!(command.capability(), Capability::AdministrativeCancel);
        for revision in [0, i64::MAX as u64 + 1] {
            let mut value = serde_json::to_value(&command).unwrap();
            value["request"]["action"]["revision"] = serde_json::json!(revision);
            assert!(!serde_json::from_value::<Command>(value)
                .unwrap()
                .valid(Limits::default()));
        }
        for case in 0..4 {
            let mut value = serde_json::to_value(&command).unwrap();
            match case {
                0 => {
                    value["request"]["action"]["submitted_request_id"] =
                        serde_json::json!("00000000-0000-0000-0000-000000000000")
                }
                1 => value["request"]["action"]["ticket_id"] = serde_json::json!(TicketId::new()),
                2 => value["request"]["grant"] = serde_json::json!({"cancel":true}),
                _ => {
                    value["request"]["action"]
                        .as_object_mut()
                        .unwrap()
                        .remove("submitted_request_id");
                }
            }
            assert!(serde_json::from_value::<Command>(value).is_err());
        }
    }
    #[test]
    fn administrative_stop_is_closed_complete_and_distinct_from_legacy_placeholder() {
        let request = AdministrativeRequest {
            scope: ControlScopeId::new(),
            request_id: RequestId::new(),
            epoch: Epoch::new(),
            action: AdministrativeAction::Stop {},
        };
        let command = Command::AdministrativeStop {
            request: request.clone(),
        };
        assert!(command.valid(Limits::default()));
        assert_eq!(command.request_id(), Some(request.request_id));
        assert_eq!(command.capability(), Capability::AdministrativeStop);
        let original = serde_json::to_value(&command).unwrap();
        for field in ["scope", "request_id", "epoch", "action"] {
            let mut value = original.clone();
            value["request"].as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<Command>(value).is_err());
        }
        for field in ["scope", "request_id", "epoch"] {
            let mut value = original.clone();
            value["request"][field] = serde_json::json!("00000000-0000-0000-0000-000000000000");
            assert!(serde_json::from_value::<Command>(value).is_err());
        }
        for field in ["agent_id", "revision", "grant", "ticket_id"] {
            let mut value = original.clone();
            value["request"]["action"][field] = serde_json::json!(1);
            assert!(serde_json::from_value::<Command>(value).is_err());
        }
        let legacy = Command::Stop {
            request_id: request.request_id,
        };
        assert_eq!(legacy.capability(), Capability::Stop);
        assert_ne!(legacy.capability(), command.capability());
    }
    #[test]
    fn administrative_execution_commands_match_exact_intent_and_keep_mutation_identity() {
        let agent_id = AgentId::new();
        let profile = ProfileRef {
            agent_id,
            version: ProfileVersion::new(),
        };
        let actions = [
            AdministrativeAction::Create { agent_id },
            AdministrativeAction::Restore {
                agent_id,
                revision: 1,
            },
            AdministrativeAction::CreateProfile { profile },
            AdministrativeAction::RestoreProfile {
                profile,
                revision: 1,
            },
            AdministrativeAction::Cancel {
                agent_id,
                revision: 1,
                ticket_id: TicketId::new(),
            },
            AdministrativeAction::Close {
                agent_id,
                revision: 1,
            },
            AdministrativeAction::Stop {},
            AdministrativeAction::CancelSubmitted {
                agent_id,
                revision: 1,
                submitted_request_id: RequestId::new(),
            },
        ];
        type Constructor = fn(AdministrativeRequest) -> Command;
        let constructors: [(Constructor, Capability, usize); 5] = [
            (
                |request| Command::AdministrativeCreate { request },
                Capability::AdministrativeCreate,
                2,
            ),
            (
                |request| Command::AdministrativeRestore { request },
                Capability::AdministrativeRestore,
                3,
            ),
            (
                |request| Command::AdministrativeClose { request },
                Capability::AdministrativeClose,
                5,
            ),
            (
                |request| Command::AdministrativeCancel { request },
                Capability::AdministrativeCancel,
                7,
            ),
            (
                |request| Command::AdministrativeStop { request },
                Capability::AdministrativeStop,
                6,
            ),
        ];
        for (make, capability, allowed) in constructors {
            for (index, action) in actions.iter().enumerate() {
                let request = AdministrativeRequest {
                    scope: ControlScopeId::new(),
                    request_id: RequestId::new(),
                    epoch: Epoch::new(),
                    action: action.clone(),
                };
                let id = request.request_id;
                let command = make(request);
                assert_eq!(command.capability(), capability);
                assert_eq!(command.request_id(), Some(id));
                assert_eq!(command.valid(Limits::default()), index == allowed);
                let mut value = serde_json::to_value(command).unwrap();
                value["request"]["grant"] = serde_json::json!({"create": true});
                assert!(serde_json::from_value::<Command>(value).is_err());
            }
        }
    }
}
