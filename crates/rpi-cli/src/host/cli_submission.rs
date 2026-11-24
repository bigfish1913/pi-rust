//! Plain-text submission CLI. A zero exit proves input custody only, never
//! successful execution. The exact original private file is also the query key.
use super::unix::read_private_bytes;
use crate::host::protocol::{AgentId, Command, HostId, Limits, RequestId, MAX_FRAME_BYTES};
use rpi_harness::agent_harness::{
    HostAdmissionExecution, HostAdmissionReceipt, MAX_HOST_ADMISSION_BYTES,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::Path;

#[derive(Clone, Copy)]
pub(super) enum Operation {
    Submit,
    Receipt,
}
impl Operation {
    pub(super) fn parse(value: &str) -> Option<Self> {
        match value {
            "submit" => Some(Self::Submit),
            "receipt" => Some(Self::Receipt),
            _ => None,
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Submission {
    agent_id: AgentId,
    request_id: RequestId,
    prompt: String,
}
pub(super) struct Expectation {
    host_id: HostId,
    agent_id: AgentId,
    request_id: RequestId,
    digest: String,
    legacy_digest: String,
    message_bytes: usize,
    query: bool,
}
pub(super) fn read_request(
    operation: Operation,
    path: &Path,
    host_id: HostId,
) -> Result<(Command, Expectation), &'static str> {
    let bytes = read_private_bytes(path, MAX_FRAME_BYTES as u64)?;
    let request = serde_json::from_slice(&bytes).map_err(|_| "invalid Host submit request")?;
    build(operation, request, host_id)
}
fn build(
    operation: Operation,
    request: Submission,
    host_id: HostId,
) -> Result<(Command, Expectation), &'static str> {
    let command = Command::Submit {
        agent_id: request.agent_id,
        request_id: request.request_id,
        prompt: request.prompt,
    };
    if !command.valid(Limits::default()) {
        return Err("invalid Host submit request");
    }
    let Command::Submit { prompt, .. } = &command else {
        unreachable!()
    };
    // Byte-for-byte the server's plain text message, with its stable zero stamp.
    let messages: Vec<rpi_agent::AgentMessage> =
        vec![rpi_ai::UserMessage::new(prompt.clone(), 0).into()];
    let bytes = serde_json::to_vec(&messages).map_err(|_| "invalid Host submit request")?;
    if bytes.len() > MAX_HOST_ADMISSION_BYTES {
        return Err("Host native input exceeds byte budget");
    }
    // The native 512KiB exact-message cap also leaves ample room in the 1MiB
    // frame for the plain-text command plus transport envelope.
    let hash = Sha256::digest(&bytes);
    let expectation = Expectation {
        host_id,
        agent_id: request.agent_id,
        request_id: request.request_id,
        digest: format!("{hash:x}"),
        legacy_digest: format!("{:x}", Sha256::digest(hash)),
        message_bytes: bytes.len(),
        query: matches!(operation, Operation::Receipt),
    };
    let command = if expectation.query {
        Command::Receipt {
            agent_id: request.agent_id,
            request_id: request.request_id,
        }
    } else {
        command
    };
    Ok((command, expectation))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    agent_id: AgentId,
    request_id: RequestId,
    durable: Option<bool>,
    found: Option<bool>,
    receipt: Option<HostAdmissionReceipt>,
    execution: Option<HostAdmissionExecution>,
}
impl Expectation {
    pub(super) fn is_mutation(&self) -> bool {
        !self.query
    }
    pub(super) fn exit_code(&self, value: &serde_json::Value) -> Result<i32, ()> {
        let response: Response = serde_json::from_value(value.clone()).map_err(|_| ())?;
        if response.agent_id != self.agent_id || response.request_id != self.request_id {
            return Err(());
        }
        match (
            self.query,
            response.durable,
            response.found,
            &response.receipt,
            &response.execution,
        ) {
            (true, None, Some(false), None, None) => return Ok(3),
            (true, None, Some(true), Some(_), Some(_))
            | (false, Some(true), None, Some(_), None) => {}
            _ => return Err(()),
        }
        let receipt = response.receipt.ok_or(())?;
        if receipt.host_id != String::from(self.host_id)
            || receipt.agent_id != String::from(self.agent_id)
            || receipt.request_id != String::from(self.request_id)
            || receipt.entry_id != String::from(self.request_id)
            || (receipt.digest != self.digest && receipt.digest != self.legacy_digest)
            || receipt.message_count != 1
            || receipt.message_bytes != self.message_bytes
            || receipt.entry_seq == 0
            || receipt.history.revision < receipt.entry_seq
        {
            return Err(());
        }
        if let Some(HostAdmissionExecution::Started { run_id, .. }) = response.execution {
            if uuid::Uuid::parse_str(&run_id).map_err(|_| ())?.is_nil() {
                return Err(());
            }
        }
        // Even Started{finished:true} is not a successful tool/run result.
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    fn request(prompt: &str) -> Submission {
        Submission {
            agent_id: AgentId::new(),
            request_id: RequestId::new(),
            prompt: prompt.into(),
        }
    }
    fn response(expected: &Expectation) -> Value {
        json!({"agent_id":expected.agent_id,"request_id":expected.request_id,"durable":true,"receipt":{"hostId":expected.host_id,"agentId":expected.agent_id,"requestId":expected.request_id,"kind":{"type":"prompt"},"digest":expected.digest,"messageCount":1,"messageBytes":expected.message_bytes,"entryId":expected.request_id,"entrySeq":1,"history":{"revision":2,"generation":0}}})
    }
    #[test]
    fn submit_cli_keeps_exact_input_and_uses_existing_native_limits() {
        for prompt in [
            "",
            " \n",
            &"x".repeat(crate::host::protocol::MAX_SUBMIT_PROMPT_BYTES + 1),
            &"\u{1}x".repeat(110_000),
        ] {
            assert!(build(Operation::Submit, request(prompt), HostId::new()).is_err());
        }
        let original = request("precise\n你好");
        let id = original.request_id;
        let (command, expected) = build(Operation::Submit, original, HostId::new()).unwrap();
        assert_eq!(command.request_id(), Some(id));
        assert!(matches!(command,Command::Submit {prompt, ..} if prompt == "precise\n你好"));
        assert_eq!(expected.digest.len(), 64);
        let (command, expected) =
            build(Operation::Receipt, request("original"), HostId::new()).unwrap();
        assert_eq!(command.request_id(), None);
        assert!(!expected.is_mutation());
        assert!(serde_json::from_value::<Submission>(json!({"agent_id":AgentId::new(),"request_id":RequestId::new(),"prompt":"original","grant":{}})).is_err());
    }
    #[test]
    fn submission_receipts_bind_exact_original_content_and_all_identities() {
        let (_, expected) = build(Operation::Submit, request("original"), HostId::new()).unwrap();
        let valid = response(&expected);
        assert_eq!(expected.exit_code(&valid), Ok(0));
        let mut legacy = valid.clone();
        legacy["receipt"]["digest"] = json!(expected.legacy_digest);
        assert_eq!(expected.exit_code(&legacy), Ok(0));
        for case in 0..13 {
            let mut invalid = valid.clone();
            match case {
                0 => invalid["agent_id"] = json!(AgentId::new()),
                1 => invalid["request_id"] = json!(RequestId::new()),
                2 => invalid["receipt"]["hostId"] = json!(HostId::new()),
                3 => invalid["receipt"]["agentId"] = json!(AgentId::new()),
                4 => invalid["receipt"]["requestId"] = json!(RequestId::new()),
                5 => invalid["receipt"]["entryId"] = json!(RequestId::new()),
                6 => invalid["receipt"]["digest"] = json!("0".repeat(64)),
                7 => invalid["receipt"]["messageBytes"] = json!(1),
                8 => invalid["receipt"]["messageCount"] = json!(2),
                9 => invalid["receipt"]["entrySeq"] = json!(0),
                10 => invalid["durable"] = json!(false),
                11 => invalid["receipt"]["history"]["revision"] = json!(0),
                _ => invalid["receipt"]["extra"] = json!("must-not-echo"),
            }
            assert!(expected.exit_code(&invalid).is_err(), "case {case}");
        }
    }
    #[test]
    fn receipt_observation_never_claims_successful_execution_or_replay() {
        let (_, mut expected) =
            build(Operation::Submit, request("original"), HostId::new()).unwrap();
        let mut value = response(&expected);
        value.as_object_mut().unwrap().remove("durable");
        value["found"] = json!(true);
        expected.query = true;
        for execution in [
            json!({"state":"admitted"}),
            json!({"state":"started","runId":uuid::Uuid::new_v4().to_string(),"finished":false}),
            json!({"state":"started","runId":uuid::Uuid::new_v4().to_string(),"finished":true}),
        ] {
            value["execution"] = execution;
            assert_eq!(expected.exit_code(&value), Ok(0));
        }
        value["execution"]["runId"] = json!(uuid::Uuid::nil().to_string());
        assert!(expected.exit_code(&value).is_err());
        assert_eq!(expected.exit_code(&json!({"agent_id":expected.agent_id,"request_id":expected.request_id,"found":false})),Ok(3));
    }
    #[test]
    fn submission_private_file_is_bounded_and_preserved_for_reconciliation() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().join("submission.json");
        let bytes = serde_json::to_vec(
            &json!({"agent_id":AgentId::new(),"request_id":RequestId::new(),"prompt":"original"}),
        )
        .unwrap();
        std::fs::write(&path, &bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_request(Operation::Submit, &path, HostId::new()).is_ok());
        assert!(read_request(Operation::Receipt, &path, HostId::new()).is_ok());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        std::fs::write(&path, vec![b' '; MAX_FRAME_BYTES + 1]).unwrap();
        assert!(read_request(Operation::Submit, &path, HostId::new()).is_err());
    }
}
