//! Host-side bridge for provider-owned OAuth flows.
//!
//! The host owns discovery and callback invocation, while platform-specific
//! authorization remains in the plugin. Token-bearing JSON is returned to the
//! caller only; this module never logs it or emits it as an agent event.

use std::sync::Arc;

use rpi_plugin_sdk::{StbString, StbStringRef};
use serde_json::Value;

use crate::registry::{RegisteredOAuthProvider, RegistrySnapshot};

#[derive(Debug, thiserror::Error)]
pub enum OAuthExtensionError {
    #[error("OAuth provider `{0}` is not registered")]
    NotFound(String),
    #[error("OAuth response has unsupported version")]
    UnsupportedVersion,
    #[error("OAuth response has invalid kind")]
    InvalidResponseKind,
    #[error("OAuth action must be one of begin, exchange, refresh, revoke")]
    InvalidAction,
    #[error("OAuth plugin returned status {0}: {1}")]
    PluginStatus(i32, String),
    #[error("OAuth plugin returned invalid JSON: {0}")]
    InvalidResponse(String),
}

/// Standard response kinds understood by the host. Payload contents remain
/// provider-defined except for `credential`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthResponseKind {
    Interaction,
    Credential,
    Success,
    Error,
}

#[derive(Debug, Clone)]
pub struct OAuthActionResponse {
    pub kind: OAuthResponseKind,
    pub payload: Value,
}

impl OAuthActionResponse {
    pub fn parse(value: Value) -> Result<Self, OAuthExtensionError> {
        let version = value.get("version").and_then(Value::as_u64).unwrap_or(1);
        if version != 1 {
            return Err(OAuthExtensionError::UnsupportedVersion);
        }
        let kind = match value.get("kind").and_then(Value::as_str) {
            Some("interaction") => OAuthResponseKind::Interaction,
            Some("credential") => OAuthResponseKind::Credential,
            Some("success") => OAuthResponseKind::Success,
            Some("error") => OAuthResponseKind::Error,
            _ => return Err(OAuthExtensionError::InvalidResponseKind),
        };
        Ok(Self {
            kind,
            payload: value.get("payload").cloned().unwrap_or(Value::Null),
        })
    }

    /// Parse the host-owned credential envelope. Provider-specific token
    /// aliases must be converted by the extension before this point.
    pub fn credential(&self) -> Result<Option<OAuthCredential>, OAuthExtensionError> {
        if self.kind != OAuthResponseKind::Credential {
            return Ok(None);
        }
        let object = self.payload.as_object().ok_or_else(|| {
            OAuthExtensionError::InvalidResponse("credential payload must be an object".into())
        })?;
        let access = object
            .get("access")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                OAuthExtensionError::InvalidResponse("missing credential access".into())
            })?;
        let refresh = object
            .get("refresh")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let expires_at = object
            .get("expires_at")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        Ok(Some(OAuthCredential {
            access: access.to_string(),
            refresh: refresh.to_string(),
            expires_at,
        }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthCredential {
    pub access: String,
    pub refresh: String,
    pub expires_at: i64,
}

/// A non-secret view of a registered OAuth provider.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OAuthProviderInfo {
    pub plugin: String,
    pub id: String,
    #[serde(flatten)]
    pub manifest: serde_json::Map<String, Value>,
}

/// List OAuth manifests without exposing callbacks or user data.
pub fn list(snapshot: &RegistrySnapshot) -> Vec<OAuthProviderInfo> {
    snapshot.oauth_providers().iter().filter_map(info).collect()
}

/// Invoke one provider-owned OAuth action.
///
/// `params` is action-specific and may contain transient authorization data.
/// The returned value is intentionally not logged by this layer.
pub fn request(
    snapshot: &Arc<RegistrySnapshot>,
    provider_id: &str,
    action: &str,
    params: Value,
) -> Result<Value, OAuthExtensionError> {
    if !matches!(action, "begin" | "exchange" | "refresh" | "revoke") {
        return Err(OAuthExtensionError::InvalidAction);
    }
    let provider = snapshot
        .oauth_provider(provider_id)
        .ok_or_else(|| OAuthExtensionError::NotFound(provider_id.to_string()))?;
    invoke(provider, action, params)
}

fn info(provider: &RegisteredOAuthProvider) -> Option<OAuthProviderInfo> {
    let mut manifest = serde_json::from_str::<Value>(&provider.manifest_json)
        .ok()?
        .as_object()?
        .clone();
    let id = manifest.get("id").and_then(Value::as_str)?.to_string();
    manifest.remove("id");
    Some(OAuthProviderInfo {
        plugin: provider.plugin.clone(),
        id,
        manifest,
    })
}

fn invoke(
    provider: &RegisteredOAuthProvider,
    action: &str,
    params: Value,
) -> Result<Value, OAuthExtensionError> {
    let request = serde_json::json!({
        "action": action,
        "providerId": provider_id(provider),
        "input": params,
    })
    .to_string();
    let mut out = StbString::empty();
    let status = (provider.request_fn)(
        StbStringRef::from_str(&request),
        &mut out,
        provider.user_data,
    );
    let response = out.to_string_lossy();
    if !out.is_empty() {
        (provider.plugin_free_string)(out);
    }
    if status != 0 {
        return Err(OAuthExtensionError::PluginStatus(status, response));
    }
    serde_json::from_str(&response)
        .map_err(|error| OAuthExtensionError::InvalidResponse(error.to_string()))
}

fn provider_id(provider: &RegisteredOAuthProvider) -> String {
    // Registration validates this field, so a malformed value can only be
    // present if a future host mutates the record. Return an empty id rather
    // than panic across an extension boundary.
    serde_json::from_str::<Value>(&provider.manifest_json)
        .ok()
        .and_then(|value| value.get("id").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{ExtensionRegistry, RegisteredOAuthProvider};
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static FREED: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn free_output(value: StbString) {
        FREED.fetch_add(1, Ordering::SeqCst);
        if !value.is_empty() {
            unsafe {
                let bytes = std::slice::from_raw_parts(value.ptr as *const u8, value.len);
                drop(Box::from_raw(bytes as *const [u8] as *mut [u8]));
            }
        }
    }

    extern "C" fn callback(
        request: StbStringRef,
        out: *mut StbString,
        _user_data: *mut c_void,
    ) -> i32 {
        let request = unsafe { request.as_str() };
        let value: Value = serde_json::from_str(request).unwrap();
        if value.get("action").and_then(Value::as_str) == Some("refresh") {
            unsafe {
                *out = StbString::from_string(r#"{"access":"secret"}"#.to_string());
            }
            0
        } else {
            unsafe {
                *out = StbString::from_string(r#"{"ok":true}"#.to_string());
            }
            0
        }
    }

    fn snapshot() -> Arc<RegistrySnapshot> {
        let mut registry = ExtensionRegistry::new();
        registry.register_oauth_provider(RegisteredOAuthProvider {
            plugin: "oauth-test".to_string(),
            manifest_json: r#"{"id":"acme","displayName":"Acme"}"#.to_string(),
            request_fn: callback,
            plugin_free_string: free_output,
            user_data: std::ptr::null_mut(),
        });
        Arc::new(registry.snapshot())
    }

    #[test]
    fn lists_non_secret_manifest_info() {
        let providers = list(&snapshot());
        assert_eq!(providers[0].id, "acme");
        assert_eq!(providers[0].plugin, "oauth-test");
        assert!(!providers[0].manifest.contains_key("id"));
    }

    #[test]
    fn invokes_action_and_reclaims_plugin_output() {
        FREED.store(0, Ordering::SeqCst);
        let result = request(&snapshot(), "acme", "refresh", serde_json::json!({"x": 1})).unwrap();
        assert_eq!(result["access"], "secret");
        assert_eq!(FREED.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn parses_standard_interaction_and_credential_responses() {
        let interaction = OAuthActionResponse::parse(serde_json::json!({
            "version": 1,
            "kind": "interaction",
            "payload": {"display": {"url": "opaque"}}
        }))
        .unwrap();
        assert_eq!(interaction.kind, OAuthResponseKind::Interaction);

        let credential = OAuthActionResponse::parse(serde_json::json!({
            "version": 1,
            "kind": "credential",
            "payload": {"access": "a", "refresh": "r", "expires_at": 42}
        }))
        .unwrap();
        assert_eq!(credential.credential().unwrap().unwrap().expires_at, 42);
    }

    #[test]
    fn rejects_unknown_response_version_and_kind() {
        assert!(matches!(
            OAuthActionResponse::parse(serde_json::json!({"version": 2, "kind": "success"})),
            Err(OAuthExtensionError::UnsupportedVersion)
        ));
        assert!(matches!(
            OAuthActionResponse::parse(
                serde_json::json!({"version": 1, "kind": "provider-specific"})
            ),
            Err(OAuthExtensionError::InvalidResponseKind)
        ));
    }

    #[test]
    fn rejects_unknown_action_and_provider() {
        assert!(matches!(
            request(&snapshot(), "acme", "unknown", Value::Null),
            Err(OAuthExtensionError::InvalidAction)
        ));
        assert!(matches!(
            request(&snapshot(), "missing", "begin", Value::Null),
            Err(OAuthExtensionError::NotFound(_))
        ));
    }
}
