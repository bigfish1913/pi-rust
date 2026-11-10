use crate::storage::{BackendError, DurableBackend, JournalFrame, JournalMutation};

/// Storage-backed durable append service.
///
/// This service is intentionally independent of `AgentMessage`, Session tree
/// types and tool execution. The harness owns those abstractions and submits
/// converted [`JournalMutation`] values. A successful append advances the
/// service's expected revision; a failed append does not.
pub struct DurableService<B> {
    backend: B,
    expected_sequence: Option<u64>,
    /// A backend contract violation is terminal for this service instance.
    /// Continuing would make the locally cached revision untrustworthy.
    poisoned: Option<String>,
}

impl<B: DurableBackend> DurableService<B> {
    pub fn open(mut backend: B) -> Result<Self, BackendError> {
        let expected_sequence = backend.head()?;
        Ok(Self {
            backend,
            expected_sequence,
            poisoned: None,
        })
    }

    pub fn append(&mut self, mutation: JournalMutation) -> Result<JournalFrame, BackendError> {
        self.ensure_healthy()?;
        let submitted = mutation.clone();
        let expected = self.expected_sequence;
        let frame = self.backend.append(mutation, expected)?;
        if let Err(error) = validate_returned_frame(&frame, &submitted, expected) {
            return Err(self.poison(error));
        }
        self.expected_sequence = Some(frame.sequence);
        Ok(frame)
    }

    /// Append a logical batch under one expected-sequence check. The service
    /// updates its local head only after the backend reports the whole batch
    /// committed.
    pub fn append_batch(
        &mut self,
        mutations: Vec<JournalMutation>,
    ) -> Result<Vec<JournalFrame>, BackendError> {
        self.ensure_healthy()?;
        let submitted = mutations.clone();
        let expected = self.expected_sequence;
        let frames = self.backend.append_batch(mutations, expected)?;
        if frames.len() != submitted.len() {
            return Err(self.poison(BackendError::Invalid(format!(
                "durable backend returned {} frames for {} mutations",
                frames.len(),
                submitted.len()
            ))));
        }
        let mut previous = expected;
        for (frame, mutation) in frames.iter().zip(&submitted) {
            if let Err(error) = validate_returned_frame(frame, mutation, previous) {
                return Err(self.poison(error));
            }
            previous = Some(frame.sequence);
        }
        if let Some(last) = frames.last() {
            self.expected_sequence = Some(last.sequence);
        }
        Ok(frames)
    }

    pub fn load(&mut self) -> Result<Vec<JournalFrame>, BackendError> {
        self.ensure_healthy()?;
        self.backend.load()
    }

    pub fn refresh(&mut self) -> Result<Option<u64>, BackendError> {
        self.ensure_healthy()?;
        let sequence = self.backend.head()?;
        self.expected_sequence = sequence;
        Ok(sequence)
    }

    fn ensure_healthy(&self) -> Result<(), BackendError> {
        match &self.poisoned {
            Some(reason) => Err(BackendError::Invalid(format!(
                "durable service is poisoned after backend contract violation: {reason}"
            ))),
            None => Ok(()),
        }
    }

    fn poison(&mut self, error: BackendError) -> BackendError {
        self.poisoned = Some(error.to_string());
        error
    }

    pub fn expected_sequence(&self) -> Option<u64> {
        self.expected_sequence
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }
}

fn validate_returned_frame(
    frame: &JournalFrame,
    mutation: &JournalMutation,
    expected: Option<u64>,
) -> Result<(), BackendError> {
    frame.validate()?;
    let expected_sequence = expected
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| BackendError::Invalid("journal sequence overflow".into()))?;
    if frame.sequence != expected_sequence
        || frame.timestamp != mutation.timestamp
        || frame.kind != mutation.kind
        || frame.payload != mutation.payload
    {
        return Err(BackendError::Invalid(
            "durable backend returned a frame that does not match its mutation or revision".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::jsonl::JsonlBackend;
    use tempfile::tempdir;

    struct MaliciousBackend;

    impl DurableBackend for MaliciousBackend {
        fn load(&mut self) -> Result<Vec<JournalFrame>, BackendError> {
            Ok(Vec::new())
        }

        fn append(
            &mut self,
            mutation: JournalMutation,
            _expected_sequence: Option<u64>,
        ) -> Result<JournalFrame, BackendError> {
            Ok(JournalFrame {
                sequence: 99,
                timestamp: mutation.timestamp,
                kind: mutation.kind,
                payload: mutation.payload,
            })
        }

        fn head(&mut self) -> Result<Option<u64>, BackendError> {
            Ok(None)
        }
    }

    #[test]
    fn batch_failure_does_not_advance_service_head() {
        let dir = tempdir().unwrap();
        let backend = JsonlBackend::open(dir.path().join("journal.jsonl")).unwrap();
        let mut service = DurableService::open(backend).unwrap();
        let valid = JournalMutation {
            timestamp: 1,
            kind: "session.entry.custom".into(),
            payload: serde_json::json!({"id": "entry"}),
        };
        let invalid = JournalMutation {
            timestamp: 2,
            kind: "bad\nkind".into(),
            payload: serde_json::json!({}),
        };
        assert!(service.append_batch(vec![valid, invalid]).is_err());
        assert_eq!(service.expected_sequence(), None);
        assert!(service.load().unwrap().is_empty());
    }

    #[test]
    fn invalid_backend_return_poison_services_fail_closed() {
        let mut service = DurableService::open(MaliciousBackend).unwrap();
        let mutation = JournalMutation {
            timestamp: 1,
            kind: "session.entry.custom".into(),
            payload: serde_json::json!({"id": "entry"}),
        };
        assert!(service.append(mutation.clone()).is_err());
        assert!(service.load().is_err());
        assert!(service.refresh().is_err());
        assert!(service.append(mutation).is_err());
    }

    #[test]
    fn service_owns_sequence_assignment_and_refreshes_after_external_append() {
        let dir = tempdir().unwrap();
        let backend = JsonlBackend::open(dir.path().join("journal.jsonl")).unwrap();
        let mut service = DurableService::open(backend).unwrap();
        let frame = service
            .append(JournalMutation {
                timestamp: 1,
                kind: "session.entry.message".into(),
                payload: serde_json::json!({"text": "hello"}),
            })
            .unwrap();
        assert_eq!(frame.sequence, 1);
        assert_eq!(service.expected_sequence(), Some(1));
    }
}
