//! Model-bound isolation: deliberately NOT a child/side-question snapshot.
use super::*;

impl AgentHarness {
    /// Create an independent, memory-only text runner. The only inherited values
    /// are the selected model and its already credential-bound provider object.
    /// In particular no extension request hooks or context callbacks cross here.
    pub async fn create_isolated_text(
        &self,
        system_prompt: String,
        max_tokens: u64,
        temperature: Option<f64>,
        max_response_bytes: usize,
    ) -> HarnessResult<AgentHarness> {
        let (model, provider) = {
            let inner = self.inner.lock().unwrap();
            if inner.closed { return Err(HarnessError::closed()); }
            (inner.model.clone(), Self::resolve_provider(&inner.models, &inner.model)?)
        };
        if !provider.supports_response_budget() {
            return Err(HarnessError::agent("sampling unsupported: provider has no response budget"));
        }
        let mut options = AgentHarnessOptions::default();
        options.model = model;
        options.models = vec![provider];
        options.system_prompt = Some(system_prompt);
        options.thinking_level = rpi_ai::types::ThinkingLevel::Off;
        options.stream_options.max_response_bytes = Some(max_response_bytes);
        options.stream_options.max_tokens = Some(max_tokens);
        options.stream_options.temperature = temperature;
        options.stream_options.max_retries = Some(0);
        options.retry.enabled = false;
        options.compaction.enabled = false;
        // Default options own fresh InMemorySessionStorage, empty tools and
        // resources, and no emitters/transforms/hooks. Never clone parent options.
        options.session = Session::new(
            Arc::new(crate::session::memory::InMemorySessionStorage::new(
                crate::session::types::SessionMetadata {
                    id: uuid::Uuid::new_v4().to_string(), created_at: 0, parent_session_id: None,
                },
                Arc::new(crate::session::memory::SystemClock),
                Arc::new(crate::session::DefaultIdGenerator::new()),
            )), None,
        );
        let mut isolated = Self::create(options).await?;
        isolated.max_turns = Some(1);
        Ok(isolated)
    }
}
