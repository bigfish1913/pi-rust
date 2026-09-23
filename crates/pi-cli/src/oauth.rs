//! OAuth 2.0 authentication flow for various providers.
//!
//! Implements device code flow and authorization code flow for:
//! - Anthropic (Claude Pro/Max)
//! - OpenAI
//! - GitHub Copilot
//! - OpenRouter
//! - And more...

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum OAuthError {
    #[error("HTTP request failed: {0}")]
    HttpError(String),
    
    #[error("Invalid response: {0}")]
    InvalidResponse(String),
    
    #[error("Authentication timeout")]
    Timeout,
    
    #[error("User denied authorization")]
    Denied,
    
    #[error("Token expired")]
    TokenExpired,
    
    #[error("Unsupported provider: {0}")]
    UnsupportedProvider(String),
}

/// OAuth provider types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OAuthProvider {
    Anthropic,
    OpenAI,
    GitHubCopilot,
    OpenRouter,
    Google,
    Azure,
}

impl std::fmt::Display for OAuthProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OAuthProvider::Anthropic => write!(f, "anthropic"),
            OAuthProvider::OpenAI => write!(f, "openai"),
            OAuthProvider::GitHubCopilot => write!(f, "github-copilot"),
            OAuthProvider::OpenRouter => write!(f, "openrouter"),
            OAuthProvider::Google => write!(f, "google"),
            OAuthProvider::Azure => write!(f, "azure"),
        }
    }
}

/// OAuth token information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_type: String,
    pub expires_at: Option<u64>,
    pub scope: Option<String>,
}

impl OAuthToken {
    /// Check if the token is expired
    pub fn is_expired(&self) -> bool {
        if let Some(expires_at) = self.expires_at {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            now >= expires_at
        } else {
            false
        }
    }
    
    /// Check if the token will expire soon (within the given duration)
    pub fn expires_soon(&self, within: Duration) -> bool {
        if let Some(expires_at) = self.expires_at {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let threshold = within.as_secs();
            expires_at.saturating_sub(now) <= threshold
        } else {
            false
        }
    }
}

/// Device code flow response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(alias = "verification_uri_complete")]
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    pub interval: u64,
}

/// OAuth client configuration
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub client_id: String,
    pub client_secret: Option<String>,
    pub auth_url: String,
    pub token_url: String,
    pub redirect_uri: Option<String>,
    pub scopes: Vec<String>,
}

/// OAuth client for handling authentication flows
pub struct OAuthClient {
    config: OAuthConfig,
    provider: OAuthProvider,
    http_client: reqwest::Client,
}

impl OAuthClient {
    /// Create a new OAuth client for the specified provider
    pub fn new(provider: OAuthProvider, config: OAuthConfig) -> Self {
        Self {
            config,
            provider,
            http_client: reqwest::Client::new(),
        }
    }
    
    /// Create an OAuth client with default configuration for a provider
    pub fn with_defaults(provider: OAuthProvider) -> Result<Self, OAuthError> {
        let config = match provider {
            OAuthProvider::Anthropic => OAuthConfig {
                client_id: "pi-cli".to_string(),
                client_secret: None,
                auth_url: "https://claude.ai/oauth/authorize".to_string(),
                token_url: "https://claude.ai/oauth/token".to_string(),
                redirect_uri: Some("http://localhost:8080/callback".to_string()),
                scopes: vec!["read".to_string(), "write".to_string()],
            },
            OAuthProvider::OpenAI => OAuthConfig {
                client_id: "pi-cli".to_string(),
                client_secret: None,
                auth_url: "https://auth.openai.com/authorize".to_string(),
                token_url: "https://auth.openai.com/oauth/token".to_string(),
                redirect_uri: Some("http://localhost:8080/callback".to_string()),
                scopes: vec!["openai.public".to_string()],
            },
            OAuthProvider::GitHubCopilot => OAuthConfig {
                client_id: "Iv1.b507a08c30306e6a".to_string(), // GitHub App client ID
                client_secret: None,
                auth_url: "https://github.com/login/oauth/authorize".to_string(),
                token_url: "https://github.com/login/oauth/access_token".to_string(),
                redirect_uri: Some("http://localhost:8080/callback".to_string()),
                scopes: vec![],
            },
            OAuthProvider::OpenRouter => OAuthConfig {
                client_id: "pi-cli".to_string(),
                client_secret: None,
                auth_url: "https://openrouter.ai/auth/authorize".to_string(),
                token_url: "https://openrouter.ai/auth/token".to_string(),
                redirect_uri: Some("http://localhost:8080/callback".to_string()),
                scopes: vec![],
            },
            OAuthProvider::Google => OAuthConfig {
                client_id: "pi-cli".to_string(),
                client_secret: None,
                auth_url: "https://accounts.google.com/o/oauth2/v2/auth".to_string(),
                token_url: "https://oauth2.googleapis.com/token".to_string(),
                redirect_uri: Some("http://localhost:8080/callback".to_string()),
                scopes: vec![
                    "https://www.googleapis.com/auth/generative-language".to_string(),
                ],
            },
            OAuthProvider::Azure => OAuthConfig {
                client_id: "pi-cli".to_string(),
                client_secret: None,
                auth_url: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize".to_string(),
                token_url: "https://login.microsoftonline.com/common/oauth2/v2.0/token".to_string(),
                redirect_uri: Some("http://localhost:8080/callback".to_string()),
                scopes: vec!["https://cognitiveservices.azure.com/.default".to_string()],
            },
        };
        
        Ok(Self::new(provider, config))
    }
    
    /// Start device code flow
    pub async fn start_device_code_flow(&self) -> Result<DeviceCodeResponse, OAuthError> {
        let mut params = HashMap::new();
        params.insert("client_id", self.config.client_id.as_str());
        
        let scope_str = self.config.scopes.join(" ");
        if !scope_str.is_empty() {
            params.insert("scope", scope_str.as_str());
        }
        
        let response = self.http_client
            .post(&self.config.auth_url)
            .form(&params)
            .send()
            .await
            .map_err(|e| OAuthError::HttpError(e.to_string()))?;
        
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(OAuthError::HttpError(format!("{}: {}", status, text)));
        }
        
        let device_code: DeviceCodeResponse = response
            .json()
            .await
            .map_err(|e| OAuthError::InvalidResponse(e.to_string()))?;
        
        Ok(device_code)
    }
    
    /// Poll for token completion (device code flow)
    pub async fn poll_for_token(
        &self,
        device_code: &str,
        interval: Duration,
        timeout: Duration,
    ) -> Result<OAuthToken, OAuthError> {
        let start = std::time::Instant::now();
        
        loop {
            if start.elapsed() > timeout {
                return Err(OAuthError::Timeout);
            }
            
            tokio::time::sleep(interval).await;
            
            let mut params = HashMap::new();
            params.insert("client_id", self.config.client_id.as_str());
            params.insert("device_code", device_code);
            params.insert("grant_type", "urn:ietf:params:oauth:grant-type:device_code");
            
            let response = self.http_client
                .post(&self.config.token_url)
                .form(&params)
                .send()
                .await
                .map_err(|e| OAuthError::HttpError(e.to_string()))?;
            
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            
            if status.is_success() {
                // Parse successful token response
                let token_response: serde_json::Value = serde_json::from_str(&text)
                    .map_err(|e| OAuthError::InvalidResponse(e.to_string()))?;
                
                let access_token = token_response["access_token"]
                    .as_str()
                    .ok_or_else(|| OAuthError::InvalidResponse("missing access_token".to_string()))?
                    .to_string();
                
                let refresh_token = token_response["refresh_token"]
                    .as_str()
                    .map(|s| s.to_string());
                
                let token_type = token_response["token_type"]
                    .as_str()
                    .unwrap_or("Bearer")
                    .to_string();
                
                let expires_at = token_response["expires_in"]
                    .as_u64()
                    .map(|expires_in| {
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs()
                            + expires_in
                    });
                
                let scope = token_response["scope"]
                    .as_str()
                    .map(|s| s.to_string());
                
                return Ok(OAuthToken {
                    access_token,
                    refresh_token,
                    token_type,
                    expires_at,
                    scope,
                });
            }
            
            // Check for specific error codes
            if let Ok(error_response) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(error) = error_response["error"].as_str() {
                    match error {
                        "authorization_pending" => continue,
                        "slow_down" => {
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            continue;
                        }
                        "access_denied" => return Err(OAuthError::Denied),
                        "expired_token" => return Err(OAuthError::Timeout),
                        _ => return Err(OAuthError::InvalidResponse(format!("unknown error: {}", error))),
                    }
                }
            }
        }
    }
    
    /// Refresh an expired token
    pub async fn refresh_token(&self, refresh_token: &str) -> Result<OAuthToken, OAuthError> {
        let mut params = HashMap::new();
        params.insert("client_id", self.config.client_id.as_str());
        params.insert("refresh_token", refresh_token);
        params.insert("grant_type", "refresh_token");
        
        if let Some(secret) = &self.config.client_secret {
            params.insert("client_secret", secret.as_str());
        }
        
        let response = self.http_client
            .post(&self.config.token_url)
            .form(&params)
            .send()
            .await
            .map_err(|e| OAuthError::HttpError(e.to_string()))?;
        
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(OAuthError::HttpError(format!("{}: {}", status, text)));
        }
        
        let token_response: serde_json::Value = response
            .json()
            .await
            .map_err(|e| OAuthError::InvalidResponse(e.to_string()))?;
        
        let access_token = token_response["access_token"]
            .as_str()
            .ok_or_else(|| OAuthError::InvalidResponse("missing access_token".to_string()))?
            .to_string();
        
        let new_refresh_token = token_response["refresh_token"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| refresh_token.to_string());
        
        let token_type = token_response["token_type"]
            .as_str()
            .unwrap_or("Bearer")
            .to_string();
        
        let expires_at = token_response["expires_in"]
            .as_u64()
            .map(|expires_in| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    + expires_in
            });
        
        let scope = token_response["scope"]
            .as_str()
            .map(|s| s.to_string());
        
        Ok(OAuthToken {
            access_token,
            refresh_token: Some(new_refresh_token),
            token_type,
            expires_at,
            scope,
        })
    }
    
    /// Get the provider type
    pub fn provider(&self) -> OAuthProvider {
        self.provider
    }
}

/// OAuth token storage
pub struct OAuthTokenStore {
    tokens: HashMap<OAuthProvider, OAuthToken>,
}

impl OAuthTokenStore {
    /// Create a new token store
    pub fn new() -> Self {
        Self {
            tokens: HashMap::new(),
        }
    }
    
    /// Store a token
    pub fn store(&mut self, provider: OAuthProvider, token: OAuthToken) {
        self.tokens.insert(provider, token);
    }
    
    /// Retrieve a token
    pub fn get(&self, provider: OAuthProvider) -> Option<&OAuthToken> {
        self.tokens.get(&provider)
    }
    
    /// Remove a token
    pub fn remove(&mut self, provider: OAuthProvider) -> Option<OAuthToken> {
        self.tokens.remove(&provider)
    }
    
    /// Check if a token exists and is valid
    pub fn is_valid(&self, provider: OAuthProvider) -> bool {
        self.tokens
            .get(&provider)
            .map(|token| !token.is_expired())
            .unwrap_or(false)
    }
}

impl Default for OAuthTokenStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_oauth_token_expiry() {
        let token = OAuthToken {
            access_token: "test".to_string(),
            refresh_token: None,
            token_type: "Bearer".to_string(),
            expires_at: Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    + 3600,
            ),
            scope: None,
        };
        
        assert!(!token.is_expired());
        assert!(!token.expires_soon(Duration::from_secs(1800)));
        assert!(token.expires_soon(Duration::from_secs(7200)));
    }
    
    #[test]
    fn test_oauth_token_store() {
        let mut store = OAuthTokenStore::new();
        let token = OAuthToken {
            access_token: "test".to_string(),
            refresh_token: None,
            token_type: "Bearer".to_string(),
            expires_at: None,
            scope: None,
        };
        
        store.store(OAuthProvider::Anthropic, token.clone());
        assert!(store.is_valid(OAuthProvider::Anthropic));
        assert!(!store.is_valid(OAuthProvider::OpenAI));
        
        let retrieved = store.get(OAuthProvider::Anthropic).unwrap();
        assert_eq!(retrieved.access_token, "test");
        
        store.remove(OAuthProvider::Anthropic);
        assert!(!store.is_valid(OAuthProvider::Anthropic));
    }
}
