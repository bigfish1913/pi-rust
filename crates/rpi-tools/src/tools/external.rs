//! Protocol-neutral, model-facing tools supplied by an external backend.

use std::{collections::HashSet, sync::Arc};

use async_trait::async_trait;
use jsonschema::JSONSchema;
use rpi_agent::{
    agent_tool::AgentTool,
    error::AgentError,
    types::{AgentToolResult, ToolResultPartial},
};
use rpi_ai::types::{Schema, Tool};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// A caller-supplied tool definition. Limits are measured in UTF-8 bytes.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExternalToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// The backend owns any resources it creates and must honor cancellation.
#[async_trait]
pub trait ExternalToolBackend: Send + Sync {
    async fn execute(
        &self,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<AgentToolResult, AgentError>;
}

fn invalid(message: impl Into<String>) -> AgentError {
    AgentError::Validation(message.into())
}

// Scan every value, including unused definitions, before compiling any schema.
fn check_refs(value: &Value) -> Result<(), AgentError> {
    match value {
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref") {
                if !reference.as_str().is_some_and(|s| s.starts_with('#')) {
                    return Err(invalid(
                        "external tool schemas permit only fragment $ref values",
                    ));
                }
            }
            // A base URI can turn a fragment into an external reference.
            for key in ["$id", "id"] {
                if let Some(id) = object.get(key) {
                    if !id
                        .as_str()
                        .is_some_and(|s| s.is_empty() || s.starts_with('#'))
                    {
                        return Err(invalid(
                            "external tool schemas cannot set a nonlocal base URI",
                        ));
                    }
                }
            }
            for value in object.values() {
                check_refs(value)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                check_refs(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn compile(definition: &ExternalToolDefinition) -> Result<JSONSchema, AgentError> {
    let name = &definition.name;
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(invalid(
            "external tool name must contain 1..64 ASCII letters, digits, '_' or '-'",
        ));
    }
    if definition.description.len() > 8192 {
        return Err(invalid("external tool description exceeds 8192 bytes"));
    }
    if !definition.input_schema.is_object()
        || definition.input_schema.get("type").and_then(Value::as_str) != Some("object")
    {
        return Err(invalid(
            "external tool inputSchema must be an object with type 'object'",
        ));
    }
    check_refs(&definition.input_schema)?;
    JSONSchema::compile(&definition.input_schema)
        .map_err(|e| invalid(format!("invalid external tool schema: {e}")))
}

/// Validate a complete registration batch before installing any tools.
/// Reserved names are compared exactly, as are duplicate names.
pub fn validate_external_tool_definitions(
    definitions: &[ExternalToolDefinition],
    reserved: &[String],
) -> Result<(), AgentError> {
    if definitions.len() > 128 {
        return Err(invalid("at most 128 external tools are allowed"));
    }
    let bytes = serde_json::to_vec(definitions).map_err(|e| invalid(e.to_string()))?;
    if bytes.len() > 256 * 1024 {
        return Err(invalid("external tool definitions exceed 256 KiB"));
    }
    let mut names = HashSet::new();
    for definition in definitions {
        if reserved.contains(&definition.name) {
            return Err(invalid(format!(
                "reserved external tool name: {}",
                definition.name
            )));
        }
        if !names.insert(&definition.name) {
            return Err(invalid(format!(
                "duplicate external tool name: {}",
                definition.name
            )));
        }
        compile(definition)?;
    }
    Ok(())
}

struct ExternalTool {
    schema: Tool,
    validator: Result<JSONSchema, AgentError>,
    backend: Arc<dyn ExternalToolBackend>,
}

/// Create a tool with the supplied schema, without coercion or fallback.
/// Validate the entire batch first with [`validate_external_tool_definitions`].
/// An invalid individual definition fails closed on execution, never calling the backend.
pub fn create_external_tool(
    definition: ExternalToolDefinition,
    backend: Arc<dyn ExternalToolBackend>,
) -> Arc<dyn AgentTool> {
    let validator = validate_external_tool_definitions(std::slice::from_ref(&definition), &[])
        .and_then(|()| compile(&definition));
    Arc::new(ExternalTool {
        schema: Tool {
            name: definition.name,
            description: definition.description,
            parameters: Schema::new(definition.input_schema),
            constrained_sampling: None,
        },
        validator,
        backend,
    })
}

#[async_trait]
impl AgentTool for ExternalTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        &self.schema.name
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        if signal.is_cancelled() {
            return Err(AgentError::Abort);
        }
        let validator = self.validator.as_ref().map_err(Clone::clone)?;
        if let Err(mut errors) = validator.validate(&params) {
            return Err(invalid(
                errors
                    .next()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "invalid external tool arguments".into()),
            ));
        }
        tokio::select! {
            biased;
            _ = signal.cancelled() => Err(AgentError::Abort),
            result = self.backend.execute(&self.schema.name, params, signal.clone()) => {
                if signal.is_cancelled() { Err(AgentError::Abort) } else { result }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_agent::types::TextContentOrImage;
    use rpi_ai::types::{ImageContent, ImageContentType};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn definition() -> ExternalToolDefinition {
        ExternalToolDefinition {
            name: "external-test_1".into(),
            description: "Real external tool".into(),
            input_schema: json!({"type":"object", "properties":{"value":{"type":"integer"}}, "required":["value"], "additionalProperties":false}),
        }
    }

    #[derive(Default)]
    struct Backend {
        calls: AtomicUsize,
        wait: bool,
        started: tokio::sync::Notify,
    }

    fn response() -> AgentToolResult {
        AgentToolResult {
            content: vec![
                TextContentOrImage::text("first"),
                TextContentOrImage::Image(ImageContent {
                    kind: ImageContentType,
                    data: "aW1hZ2U=".into(),
                    mime_type: "image/png".into(),
                }),
                TextContentOrImage::text("last"),
            ],
            details: json!({"nested":{"unchanged":true}}),
            terminate: true,
            ..Default::default()
        }
    }

    #[async_trait]
    impl ExternalToolBackend for Backend {
        async fn execute(
            &self,
            name: &str,
            arguments: Value,
            cancel: CancellationToken,
        ) -> Result<AgentToolResult, AgentError> {
            assert_eq!(name, "external-test_1");
            assert_eq!(arguments, json!({"value":7}));
            assert!(!cancel.is_cancelled());
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            if self.wait {
                cancel.cancelled().await;
                return Err(AgentError::Abort);
            }
            Ok(response())
        }
    }

    #[tokio::test]
    async fn factory_preserves_schema_and_complete_result() {
        let backend = Arc::new(Backend::default());
        let def = definition();
        let expected_schema = Schema::new(def.input_schema.clone());
        let tool = create_external_tool(def, backend.clone());
        assert_eq!(tool.schema().parameters, expected_schema);
        assert_eq!(tool.label(), "external-test_1");
        let result = tool
            .execute(
                "id",
                json!({"value":7}),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
            .unwrap();
        assert_eq!(result.content, response().content);
        assert_eq!(result.details, response().details);
        assert!(result.terminate);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn definitions_are_strict_and_bounded() {
        let def = definition();
        let encoded = serde_json::to_value(&def).unwrap();
        assert!(encoded.get("inputSchema").is_some());
        let mut extra = encoded;
        extra["unexpected"] = json!(true);
        assert!(serde_json::from_value::<ExternalToolDefinition>(extra).is_err());
        assert!(validate_external_tool_definitions(&[def.clone()], &[]).is_ok());
        assert!(validate_external_tool_definitions(&[def.clone(), def.clone()], &[]).is_err());
        assert!(validate_external_tool_definitions(&[def.clone()], &[def.name.clone()]).is_err());
        for name in ["", "has space", "unicodeé", &"a".repeat(65)] {
            let mut bad = def.clone();
            bad.name = name.into();
            assert!(validate_external_tool_definitions(&[bad], &[]).is_err());
        }
        let mut boundary = def.clone();
        boundary.name = "a".repeat(64);
        boundary.description = "x".repeat(8192);
        assert!(validate_external_tool_definitions(&[boundary.clone()], &[]).is_ok());
        boundary.description.push('x');
        assert!(validate_external_tool_definitions(&[boundary], &[]).is_err());
        let mut batch: Vec<_> = (0..128)
            .map(|i| {
                let mut d = def.clone();
                d.name = format!("tool{i}");
                d
            })
            .collect();
        assert!(validate_external_tool_definitions(&batch, &[]).is_ok());
        batch.push(def.clone());
        assert!(validate_external_tool_definitions(&batch, &[]).is_err());
        batch.pop();
        for d in &mut batch {
            d.description = "x".repeat(8192);
        }
        assert!(validate_external_tool_definitions(&batch, &[]).is_err());
        for schema in [
            json!(true),
            json!({}),
            json!({"type":"array"}),
            json!({"type":"object","properties":{"x":{"type":42}}}),
            json!({"type":"object","$ref":"https://example.com/schema"}),
            json!({"type":"object","definitions":{"unused":{"$ref":"file:///secret"}}}),
            json!({"type":"object","$ref":42}),
        ] {
            let mut bad = def.clone();
            bad.input_schema = schema;
            assert!(validate_external_tool_definitions(&[bad], &[]).is_err());
        }
        let mut local = def;
        local.input_schema = json!({"type":"object","properties":{"value":{"$ref":"#/definitions/n"}},"definitions":{"n":{"type":"integer"}}});
        assert!(validate_external_tool_definitions(&[local], &[]).is_ok());
    }

    #[tokio::test]
    async fn invalid_arguments_and_cancel_never_call_backend() {
        let backend = Arc::new(Backend::default());
        let tool = create_external_tool(definition(), backend.clone());
        for args in [
            json!({}),
            json!({"value":"7"}),
            json!({"value":7,"extra":true}),
            json!([]),
        ] {
            assert!(matches!(
                tool.execute("id", args, CancellationToken::new(), Arc::new(|_| {}))
                    .await,
                Err(AgentError::Validation(_))
            ));
        }
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            tool.execute("id", json!({"value":7}), cancel, Arc::new(|_| {}))
                .await,
            Err(AgentError::Abort)
        ));
        let mut bad = definition();
        bad.input_schema = json!({});
        assert!(matches!(
            create_external_tool(bad, backend.clone())
                .execute("id", json!({}), CancellationToken::new(), Arc::new(|_| {}))
                .await,
            Err(AgentError::Validation(_))
        ));
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn in_flight_cancellation_returns_abort() {
        let backend = Arc::new(Backend {
            wait: true,
            ..Default::default()
        });
        let tool = create_external_tool(definition(), backend.clone());
        let cancel = CancellationToken::new();
        let run = tool.execute("id", json!({"value":7}), cancel.clone(), Arc::new(|_| {}));
        let cancellation = async {
            backend.started.notified().await;
            cancel.cancel();
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(run, cancellation)
        })
        .await
        .unwrap();
        assert!(matches!(result, Err(AgentError::Abort)));
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    }
}
