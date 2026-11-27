//! Fixed-authority administrative dispatch. Each execution command only accepts
//! its matching intent; a queried Cancel/CancelSubmitted/Stop is observation.
use super::{
    AdministrativeRequest, Command, DaemonAdministration, ErrorCode, Limits, Reply, MAX_PAGE_SIZE,
};
use crate::host::{administration::AdministrativeError, lifecycle::LifecycleRequest};

pub(super) async fn dispatch(
    administration: Option<&DaemonAdministration>,
    command: Command,
) -> Reply {
    let Some(administration) = administration else {
        return error(ErrorCode::Unsupported);
    };
    if !command.valid(Limits::default()) {
        return error(ErrorCode::InvalidRequest);
    }
    match command {
        Command::AdministrativeCreate { request }
        | Command::AdministrativeRestore { request }
        | Command::AdministrativeClose { request }
        | Command::AdministrativeCancel { request }
        | Command::AdministrativeStop { request } => {
            let native = match native_request(&request) {
                Ok(native) => native,
                Err(code) => return error(code),
            };
            // Only the fixed native facade can authorize/admit. It owns the
            // job independently of this bounded connection waiter and gives
            // fresh custody only after the intent's durability barrier.
            match administration.execute(native).await {
                Ok(receipt) => Reply::Data {
                    data: serde_json::json!({"scope": request.scope, "request_id": request.request_id, "receipt": receipt}),
                },
                Err(reason) => administrative_error(reason, false),
            }
        }
        Command::Profiles { after, limit } => {
            if !(1..=MAX_PAGE_SIZE).contains(&limit) {
                return error(ErrorCode::InvalidRequest);
            }
            let after = match after.map(|id| String::from(id).try_into()).transpose() {
                Ok(after) => after,
                Err(_) => return error(ErrorCode::InvalidRequest),
            };
            let mut profiles: Vec<_> = administration
                .profile_references()
                .filter(|profile| after.is_none_or(|after| profile.agent_id > after))
                .take(limit + 1)
                .collect();
            let more = profiles.len() > limit;
            if more {
                profiles.pop();
            }
            let next_cursor = if more {
                profiles.last().map(|profile| profile.agent_id)
            } else {
                None
            };
            Reply::Data {
                data: serde_json::json!({"scope": administration.scope(), "profiles": profiles, "next_cursor": next_cursor}),
            }
        }
        Command::AdministrativeReceipt { request } => {
            if !request.valid() {
                return error(ErrorCode::InvalidRequest);
            }
            // Both sides are closed typed schemas bounded entirely by UUIDs,
            // revisions and an enum. No configuration/secret/arbitrary JSON is
            // accepted. Native authorization precedes any catalog lookup.
            let native = match native_request(&request) {
                Ok(native) => native,
                Err(code) => return error(code),
            };
            match administration.receipt(&native).await {
                Ok(receipt) => Reply::Data {
                    data: serde_json::json!({"found": receipt.is_some(), "scope": request.scope, "request_id": request.request_id, "receipt": receipt}),
                },
                Err(reason) => administrative_error(reason, true),
            }
        }
        _ => error(ErrorCode::Unsupported),
    }
}
fn native_request(request: &AdministrativeRequest) -> Result<LifecycleRequest, ErrorCode> {
    serde_json::to_value(request)
        .and_then(serde_json::from_value)
        .map_err(|_| ErrorCode::InvalidRequest)
}
fn administrative_error(reason: AdministrativeError, query: bool) -> Reply {
    error(match reason {
        AdministrativeError::Unauthorized => ErrorCode::Unauthorized,
        AdministrativeError::InvalidRequest => ErrorCode::InvalidRequest,
        AdministrativeError::Conflict => ErrorCode::Conflict,
        AdministrativeError::NotFound => ErrorCode::NotFound,
        AdministrativeError::StaleEpoch => ErrorCode::StaleEpoch,
        AdministrativeError::StaleRevision => ErrorCode::StaleRevision,
        AdministrativeError::Busy | AdministrativeError::Stopping => ErrorCode::Busy,
        AdministrativeError::Capacity => ErrorCode::Capacity,
        AdministrativeError::Unsupported => ErrorCode::Unsupported,
        AdministrativeError::OutcomeUnknown if query => ErrorCode::PersistenceUncertain,
        AdministrativeError::OutcomeUnknown => ErrorCode::OutcomeUnknown,
    })
}
fn error(code: ErrorCode) -> Reply {
    Reply::Error { code }
}
