//! Explicit complete-intent CLI inputs and conservative administrative results.
//! No transport, identity generation, retry or native execution lives here.
use super::unix::read_private_bytes;
use crate::host::{
    client::ClientError,
    lifecycle::{LifecycleReceipt, LifecycleState},
    protocol::{AdministrativeRequest, Command, ControlScopeId, ErrorCode, Limits, RequestId},
};
use serde::Deserialize;
use std::path::Path;

const MAX_REQUEST_BYTES: u64 = 4096;

#[derive(Clone, Copy)]
pub(super) enum Operation {
    Receipt,
    Create,
    Restore,
    Close,
    Cancel,
    Stop,
}
impl Operation {
    pub(super) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "administrative-receipt" => Self::Receipt,
            "administrative-create" => Self::Create,
            "administrative-restore" => Self::Restore,
            "administrative-close" => Self::Close,
            "administrative-cancel" => Self::Cancel,
            "administrative-stop" => Self::Stop,
            _ => return None,
        })
    }
    fn command(self, request: AdministrativeRequest) -> Result<Command, &'static str> {
        let command = match self {
            Self::Receipt => Command::AdministrativeReceipt { request },
            Self::Create => Command::AdministrativeCreate { request },
            Self::Restore => Command::AdministrativeRestore { request },
            Self::Close => Command::AdministrativeClose { request },
            Self::Cancel => Command::AdministrativeCancel { request },
            Self::Stop => Command::AdministrativeStop { request },
        };
        if !command.valid(Limits::default()) {
            return Err("invalid Host administrative intent");
        }
        Ok(command)
    }
}
pub(super) fn read_request(operation: Operation, path: &Path) -> Result<Command, &'static str> {
    let bytes = read_private_bytes(path, MAX_REQUEST_BYTES)?;
    let request =
        serde_json::from_slice(&bytes).map_err(|_| "invalid Host administrative intent")?;
    operation.command(request)
}

pub(super) struct Expectation {
    request: AdministrativeRequest,
    query: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    scope: ControlScopeId,
    request_id: RequestId,
    receipt: Option<LifecycleReceipt>,
    found: Option<bool>,
}
impl Expectation {
    pub(super) fn for_command(command: &Command) -> Option<Self> {
        let (request, query) = match command {
            Command::AdministrativeReceipt { request } => (request, true),
            Command::AdministrativeCreate { request }
            | Command::AdministrativeRestore { request }
            | Command::AdministrativeClose { request }
            | Command::AdministrativeCancel { request }
            | Command::AdministrativeStop { request } => (request, false),
            _ => return None,
        };
        Some(Self {
            request: request.clone(),
            query,
        })
    }
    pub(super) fn is_mutation(&self) -> bool {
        !self.query
    }

    /// 0 means only a validated journal Succeeded, never current residency,
    /// cancellation settlement or process exit. Missing is not a no-effect proof.
    pub(super) fn exit_code(&self, value: &serde_json::Value) -> Result<i32, ()> {
        let response: Response = serde_json::from_value(value.clone()).map_err(|_| ())?;
        if response.scope != self.request.scope || response.request_id != self.request.request_id {
            return Err(());
        }
        match (&response.receipt, response.found, self.query) {
            (None, Some(false), true) => return Ok(3),
            (Some(_), Some(true), true) | (Some(_), None, false) => {}
            _ => return Err(()),
        }
        let receipt = response.receipt.ok_or(())?;
        receipt.validate().map_err(|_| ())?;
        let original = serde_json::to_value(&self.request).map_err(|_| ())?;
        if serde_json::to_value(&receipt.request).map_err(|_| ())? != original {
            return Err(());
        }
        Ok(match receipt.status {
            LifecycleState::Succeeded { .. } => 0,
            LifecycleState::Rejected { .. } => 1,
            LifecycleState::Prepared {}
            | LifecycleState::Interrupted {}
            | LifecycleState::Uncertain { .. } => 3,
        })
    }
}
pub(super) fn error_exit(error: ClientError) -> i32 {
    match error {
        ClientError::OutcomeUnknown { .. }
        | ClientError::QueryFailed(_)
        | ClientError::Rejected(ErrorCode::OutcomeUnknown | ErrorCode::PersistenceUncertain) => 3,
        ClientError::NotSent(_) | ClientError::Rejected(_) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{
        client::Failure,
        protocol::{AdministrativeAction as A, AgentId, Epoch, ProfileRef, ProfileVersion},
    };
    use serde_json::{json, Value};

    fn request() -> AdministrativeRequest {
        AdministrativeRequest {
            scope: ControlScopeId::new(),
            request_id: RequestId::new(),
            epoch: Epoch::new(),
            action: A::CreateProfile {
                profile: ProfileRef {
                    agent_id: AgentId::new(),
                    version: ProfileVersion::new(),
                },
            },
        }
    }
    fn response(request: &AdministrativeRequest, status: Value) -> Value {
        json!({"scope": request.scope, "request_id": request.request_id, "receipt": {"request": request, "status": status}})
    }
    #[test]
    fn complete_intent_cli_matches_actions_without_replacing_identity() {
        let original = request();
        let A::CreateProfile { profile } = original.action else {
            unreachable!()
        };
        let actions = [
            original.action.clone(),
            A::RestoreProfile {
                profile,
                revision: 1,
            },
            A::Close {
                agent_id: profile.agent_id,
                revision: 1,
            },
            A::Create {
                agent_id: profile.agent_id,
            },
            A::Restore {
                agent_id: profile.agent_id,
                revision: 1,
            },
            A::Cancel {
                agent_id: profile.agent_id,
                revision: 1,
                ticket_id: crate::host::protocol::TicketId::new(),
            },
            A::Stop {},
            A::CancelSubmitted {
                agent_id: profile.agent_id,
                revision: 1,
                submitted_request_id: RequestId::new(),
            },
        ];
        for (operation, index) in [
            (Operation::Create, 0),
            (Operation::Restore, 1),
            (Operation::Close, 2),
            (Operation::Cancel, 7),
            (Operation::Stop, 6),
        ] {
            for (position, action) in actions.iter().enumerate() {
                let mut request = original.clone();
                request.action = action.clone();
                let command = operation.command(request.clone());
                assert_eq!(command.is_ok(), position == index);
                if let Ok(command) = command {
                    let expected = Expectation::for_command(&command).unwrap();
                    assert_eq!(expected.request, request);
                    assert_eq!(command.request_id(), Some(original.request_id));
                }
                assert!(Operation::Receipt.command(request).is_ok());
            }
        }
    }
    #[test]
    fn stop_cli_validates_complete_history_without_claiming_current_process_exit() {
        let mut request = request();
        request.action = A::Stop {};
        let command = Operation::parse("administrative-stop")
            .unwrap()
            .command(request.clone())
            .unwrap();
        let expected = Expectation::for_command(&command).unwrap();
        assert!(expected.is_mutation());
        assert_eq!(command.request_id(), Some(request.request_id));
        let value = response(
            &request,
            json!({"state":"succeeded","result":{"kind":"native_settled"}}),
        );
        assert_eq!(expected.exit_code(&value), Ok(0));
        let mut query = value.clone();
        query["found"] = json!(true);
        assert_eq!(
            Expectation {
                request: request.clone(),
                query: true
            }
            .exit_code(&query),
            Ok(0)
        );
        for case in 0..8 {
            let mut invalid = value.clone();
            match case {
                0 => invalid["receipt"]["request"]["scope"] = json!(ControlScopeId::new()),
                1 => invalid["receipt"]["request"]["request_id"] = json!(RequestId::new()),
                2 => invalid["receipt"]["request"]["epoch"] = json!(Epoch::new()),
                3 => {
                    invalid["receipt"]["request"]["action"] =
                        json!({"op":"close","agent_id":AgentId::new(),"revision":1})
                }
                4 => invalid["receipt"]["status"]["result"] = json!({"kind":"closed","revision":1}),
                5 => {
                    invalid["receipt"]["status"]["result"] =
                        json!({"kind":"cancel_signalled","signalled":true})
                }
                6 => invalid["receipt"]["request"]["action"]["revision"] = json!(1),
                _ => invalid["scope"] = json!(ControlScopeId::new()),
            }
            assert!(expected.exit_code(&invalid).is_err(), "case {case}");
        }
        for (status, code) in [
            (json!({"state":"rejected","code":"unauthorized"}), 1),
            (json!({"state":"prepared"}), 3),
            (json!({"state":"interrupted"}), 3),
            (json!({"state":"uncertain","code":"internal"}), 3),
        ] {
            assert_eq!(expected.exit_code(&response(&request, status)), Ok(code));
        }
        assert_eq!(
            error_exit(ClientError::Rejected(ErrorCode::OutcomeUnknown)),
            3
        );
        assert_eq!(
            error_exit(ClientError::OutcomeUnknown {
                request_id: request.request_id,
                reason: crate::host::client::Failure::Connection
            }),
            3
        );
    }
    #[test]
    fn cancellation_cli_binds_original_target_and_signal_is_only_recorded_history() {
        let mut request = request();
        request.action = A::CancelSubmitted {
            agent_id: AgentId::new(),
            revision: 1,
            submitted_request_id: RequestId::new(),
        };
        let command = Operation::parse("administrative-cancel")
            .unwrap()
            .command(request.clone())
            .unwrap();
        let expected = Expectation::for_command(&command).unwrap();
        assert!(expected.is_mutation());
        assert_eq!(command.request_id(), Some(request.request_id));
        for signalled in [true, false] {
            let value = response(
                &request,
                json!({"state":"succeeded","result":{"kind":"cancel_signalled","signalled":signalled}}),
            );
            assert_eq!(expected.exit_code(&value), Ok(0));
            let mut query = value.clone();
            query["found"] = json!(true);
            assert_eq!(
                Expectation {
                    request: request.clone(),
                    query: true
                }
                .exit_code(&query),
                Ok(0)
            );
            for case in 0..8 {
                let mut invalid = value.clone();
                match case {
                    0 => {
                        invalid["receipt"]["request"]["action"]["submitted_request_id"] =
                            json!(RequestId::new())
                    }
                    1 => invalid["receipt"]["request"]["action"]["revision"] = json!(2),
                    2 => invalid["receipt"]["request"]["epoch"] = json!(Epoch::new()),
                    3 => invalid["request_id"] = json!(RequestId::new()),
                    4 => {
                        invalid["receipt"]["status"]["result"] =
                            json!({"kind":"loaded","revision":1})
                    }
                    5 => invalid["receipt"]["status"]["result"]["signalled"] = json!("true"),
                    6 => {
                        invalid["receipt"]["request"]["action"]["ticket_id"] =
                            json!(crate::host::protocol::TicketId::new())
                    }
                    _ => {
                        invalid["receipt"]["status"]["result"]
                            .as_object_mut()
                            .unwrap()
                            .remove("signalled");
                    }
                }
                assert!(expected.exit_code(&invalid).is_err(), "case {case}");
            }
        }
        for (status, code) in [
            (json!({"state":"rejected","code":"stale_revision"}), 1),
            (json!({"state":"prepared"}), 3),
            (json!({"state":"interrupted"}), 3),
            (json!({"state":"uncertain","code":"internal"}), 3),
        ] {
            assert_eq!(expected.exit_code(&response(&request, status)), Ok(code));
        }
    }
    #[test]
    fn administrative_exit_codes_require_exact_validated_receipts() {
        let request = request();
        let expected =
            Expectation::for_command(&Operation::Create.command(request.clone()).unwrap()).unwrap();
        for (status, code) in [
            (
                json!({"state":"succeeded","result":{"kind":"loaded","revision":1}}),
                0,
            ),
            (json!({"state":"rejected","code":"busy"}), 1),
            (json!({"state":"uncertain","code":"internal"}), 3),
            (json!({"state":"prepared"}), 3),
            (json!({"state":"interrupted"}), 3),
        ] {
            let value = response(&request, status);
            assert_eq!(expected.exit_code(&value), Ok(code));
            let mut query = value.clone();
            query["found"] = json!(true);
            assert_eq!(
                Expectation {
                    request: request.clone(),
                    query: true
                }
                .exit_code(&query),
                Ok(code)
            );
        }
        let success = response(
            &request,
            json!({"state":"succeeded","result":{"kind":"loaded","revision":1}}),
        );
        for case in 0..8 {
            let mut value = success.clone();
            match case {
                0 => value["scope"] = json!(ControlScopeId::new()),
                1 => value["request_id"] = json!(RequestId::new()),
                2 => value["receipt"]["request"]["epoch"] = json!(Epoch::new()),
                3 => {
                    value["receipt"]["request"]["action"]["profile"]["version"] =
                        json!(ProfileVersion::new())
                }
                4 => value["receipt"]["status"]["result"]["kind"] = json!("closed"),
                5 => value["receipt"]["status"]["result"]["revision"] = json!(0),
                6 => value["receipt"]["status"]["state"] = json!("unknown"),
                _ => value["found"] = json!(false),
            }
            assert!(expected.exit_code(&value).is_err(), "case {case}");
        }
        let missing = json!({"scope":request.scope,"request_id":request.request_id,"found":false,"receipt":null});
        assert_eq!(
            Expectation {
                request,
                query: true
            }
            .exit_code(&missing),
            Ok(3)
        );
        assert!(expected.exit_code(&missing).is_err());
    }
    #[test]
    fn administrative_transport_uncertainty_is_not_a_success_or_retry_instruction() {
        assert_eq!(
            error_exit(ClientError::OutcomeUnknown {
                request_id: RequestId::new(),
                reason: Failure::Closed
            }),
            3
        );
        assert_eq!(
            error_exit(ClientError::Rejected(ErrorCode::OutcomeUnknown)),
            3
        );
        assert_eq!(
            error_exit(ClientError::Rejected(ErrorCode::PersistenceUncertain)),
            3
        );
        assert_eq!(error_exit(ClientError::QueryFailed(Failure::Timeout)), 3);
        assert_eq!(error_exit(ClientError::NotSent(Failure::Timeout)), 1);
        assert_eq!(
            error_exit(ClientError::Rejected(ErrorCode::Unauthorized)),
            1
        );
    }
    #[test]
    fn request_files_are_private_bounded_closed_and_never_rewritten() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().join("intent.json");
        let original = request();
        let bytes = serde_json::to_vec(&original).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_request(Operation::Create, &path).is_ok());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(read_request(Operation::Close, &path).is_err());
        let alias = root.path().join("alias.json");
        symlink(&path, &alias).unwrap();
        assert!(read_request(Operation::Create, &alias).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_request(Operation::Create, &path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut value = serde_json::to_value(original).unwrap();
        value["grant"] = json!({"create":true});
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(read_request(Operation::Create, &path).is_err());
        std::fs::write(&path, vec![b' '; 4097]).unwrap();
        assert!(read_request(Operation::Create, &path).is_err());
    }
}
