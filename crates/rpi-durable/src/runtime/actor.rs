//! Trusted host actor identity and authorization metadata.

use super::admission::{validate_identity, DurableError};

/// Trusted host identity, never inferred from model-supplied tool metadata.
#[derive(Clone, Debug)]
pub struct DurableActor {
    pub(crate) id: String,
    pub(crate) may_replace_workflow: bool,
}

impl DurableActor {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn may_replace_workflow(&self) -> bool {
        self.may_replace_workflow
    }

    pub fn owner(id: impl Into<String>) -> Result<Self, DurableError> {
        Self::new(id.into(), true)
    }

    pub fn child(id: impl Into<String>) -> Result<Self, DurableError> {
        Self::new(id.into(), false)
    }

    fn new(id: String, may_replace_workflow: bool) -> Result<Self, DurableError> {
        validate_identity(&id)?;
        Ok(Self {
            id,
            may_replace_workflow,
        })
    }
}
