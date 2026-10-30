//! Neutral, bounded headless authentication. Only explicitly public display fields
//! cross stdout; callback payloads, credentials and diagnostic errors never do.
//! Callbacks run inline, owned by the registered snapshot: no detached host threads.
//! The parent must kill this child if a synchronous plugin callback blocks.
use std::sync::Arc;
use std::time::Duration;

use rpi_extensions::{OAuthActionResponse, OAuthResponseKind, RegistrySnapshot};
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::time::{timeout_at, Instant};

use super::{config, parse_provider, Credential, EXIT_ERROR};

const LIMIT: usize = 64 * 1024;
const MAX_INTERACTIONS: usize = 32;
const DEADLINE: Duration = Duration::from_secs(300);
type AuthResult<T> = Result<T, &'static str>;

fn kind(provider: &str, snapshot: Option<&Arc<RegistrySnapshot>>) -> &'static str {
    if snapshot.is_some_and(|s| s.oauth_provider(provider).is_some()) {
        "oauth"
    } else {
        "api_key"
    }
}

pub(super) fn describe(args: &[String], snapshot: Option<&Arc<RegistrySnapshot>>) -> i32 {
    let Some(provider) = parse_provider(args).filter(|s| !s.is_empty() && s.len() < 1024) else {
        return EXIT_ERROR;
    };
    if !args.iter().any(|s| s == "--json") {
        return EXIT_ERROR;
    }
    println!("{}", json!({"rpc":true,"provider":provider,"kind":kind(provider, snapshot)}));
    0
}

pub(super) async fn login(args: &[String], snapshot: Option<&Arc<RegistrySnapshot>>) -> i32 {
    let mut output = tokio::io::stdout();
    let deadline = Instant::now() + DEADLINE;
    let Some(provider) = parse_provider(args).filter(|s| !s.is_empty() && s.len() < 1024) else {
        let _ = emit(&mut output, &json!({"type":"auth_result","authenticated":false,"error":"invalid provider"}), deadline).await;
        return EXIT_ERROR;
    };
    let oauth = kind(provider, snapshot) == "oauth";
    drive(
        provider, oauth, BufReader::new(tokio::io::stdin()), &mut output,
        |action, input| {
            let snapshot = snapshot.ok_or("OAuth unavailable")?;
            let raw = rpi_extensions::request_oauth(snapshot, provider, action, input)
                .map_err(|_| "OAuth request failed")?;
            OAuthActionResponse::parse(raw).map_err(|_| "Invalid OAuth response")
        },
        |credential| config::upsert_credential(provider, credential).map_err(|_| "Credential storage failed"),
        deadline,
    ).await
}

async fn emit<W: AsyncWrite + Unpin>(out: &mut W, value: &Value, deadline: Instant) -> AuthResult<()> {
    if Instant::now() >= deadline { return Err("Authentication timed out"); }
    let mut bytes = serde_json::to_vec(value).map_err(|_| "Invalid output")?;
    if bytes.len() + 1 > LIMIT { return Err("Output too large"); }
    bytes.push(b'\n');
    timeout_at(deadline, async {
        out.write_all(&bytes).await.map_err(|_| "Output closed")?;
        out.flush().await.map_err(|_| "Output closed")
    }).await.map_err(|_| "Authentication timed out")?
}

// take() bounds allocation even if a peer never sends a newline. An unterminated
// line at EOF is not an accepted response.
async fn response<R: AsyncBufRead + Unpin>(input: &mut R, id: &str, deadline: Instant) -> AuthResult<Value> {
    use tokio::io::AsyncReadExt;
    if Instant::now() >= deadline { return Err("Authentication timed out"); }
    let mut bytes = Vec::new();
    let n = timeout_at(deadline, input.take((LIMIT + 1) as u64).read_until(b'\n', &mut bytes))
        .await.map_err(|_| "Authentication timed out")?
        .map_err(|_| "Input closed")?;
    if n == 0 || bytes.last() != Some(&b'\n') { return Err("Input closed"); }
    if n > LIMIT { return Err("Response too large"); }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| "Invalid response")?;
    if value.get("type").and_then(Value::as_str) != Some("auth_response")
        || value.get("requestId").and_then(Value::as_str) != Some(id) {
        return Err("Invalid response");
    }
    match value.get("action").and_then(Value::as_str) {
        Some("accept") => {},
        Some("cancel" | "decline") => return Err("Authentication cancelled"),
        _ => return Err("Invalid response"),
    }
    match value.get("content") {
        None => Ok(json!({})),
        Some(v) if v.is_object() => Ok(v.clone()),
        _ => Err("Invalid form content"),
    }
}

fn key_schema(field: &str) -> Value {
    json!({"type":"object","properties":{field:{"type":"string","password":true}},"required":[field],"additionalProperties":false})
}

// A bounded JSON-schema subset: preserve provider types and constraints exactly,
// reject unsupported validation rather than silently dropping it.
fn valid_schema(s: &Value, depth: usize) -> bool {
    if depth > 8 { return false; }
    let Some(obj) = s.as_object() else { return false; };
    let Some(kind) = s.get("type").and_then(Value::as_str) else { return false; };
    if obj.iter().any(|(k, v)| match k.as_str() {
        "type" => false,
        "title" | "description" => !v.is_string(),
        "password" => kind != "string" || !v.is_boolean(),
        "enum" => !v.as_array().is_some_and(|a| !a.is_empty() && a.iter().all(|v| valid_type(kind, v))),
        "properties" | "required" | "additionalProperties" => kind != "object",
        "minimum" | "maximum" => !matches!(kind, "number" | "integer") || v.as_f64().is_none(),
        "minLength" | "maxLength" => kind != "string" || v.as_u64().is_none(),
        _ => true,
    }) { return false; }
    for (min, max) in [("minimum", "maximum"), ("minLength", "maxLength")] {
        if let (Some(a), Some(b)) = (s.get(min).and_then(Value::as_f64), s.get(max).and_then(Value::as_f64)) {
            if a > b { return false; }
        }
    }
    match kind {
        "object" => {
            s.get("properties").and_then(Value::as_object).is_some_and(|p| p.len() <= 64 && p.values().all(|s| valid_schema(s, depth + 1)))
                && s.get("required").is_none_or(|r| r.as_array().is_some_and(|a| a.iter().all(|v| v.as_str().is_some_and(|k| s["properties"].get(k).is_some()))))
                && s.get("additionalProperties").is_none_or(|v| v == false)
        },
        "string" | "number" | "integer" | "boolean" => true,
        _ => false,
    }
}

fn valid_type(kind: &str, v: &Value) -> bool {
    match kind {
        "object" => v.is_object(),
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "number" => v.is_number(),
        "integer" => v.is_i64() || v.is_u64(),
        _ => false,
    }
}

fn valid_content(s: &Value, v: &Value) -> bool {
    if !s["type"].as_str().is_some_and(|kind| valid_type(kind, v)) { return false; }
    if let Some(options) = s.get("enum") { if !options.as_array().is_some_and(|a| a.contains(v)) { return false; } }
    if let Some(n) = v.as_f64() {
        if s.get("minimum").and_then(Value::as_f64).is_some_and(|min| n < min)
            || s.get("maximum").and_then(Value::as_f64).is_some_and(|max| n > max) { return false; }
    }
    if let Some(text) = v.as_str() {
        let len = text.chars().count() as u64;
        if s.get("minLength").and_then(Value::as_u64).is_some_and(|min| len < min)
            || s.get("maxLength").and_then(Value::as_u64).is_some_and(|max| len > max) { return false; }
    }
    v.as_object().is_none_or(|obj| {
        s.get("required").and_then(Value::as_array).is_none_or(|a| a.iter().all(|k| obj.contains_key(k.as_str().unwrap_or(""))))
            && obj.iter().all(|(k,v)| s["properties"].get(k).is_some_and(|s| valid_content(s,v)))
    })
}

fn valid_url(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|parsed| {
        matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some()
            && parsed.username().is_empty() && parsed.password().is_none()
            // Also reject an explicitly empty userinfo component.
            && !url.split_once("://").is_some_and(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or("").contains('@'))
    })
}

// Terminal login supplies {value: line}. Keep arbitrary structured content,
// adding that compatibility alias only for conventional single-string inputs.
fn exchange_input(mut content: Value) -> Value {
    if let Some(obj) = content.as_object_mut() {
        if obj.len() == 1 && !obj.contains_key("value") {
            if let Some(value) = obj.get("code").or_else(|| obj.get("apiKey")).filter(|v| v.is_string()).cloned() {
                obj.insert("value".into(), value);
            }
        }
    }
    content
}

async fn drive<R, W, F, S>(provider: &str, oauth: bool, mut input: R, out: &mut W, mut callback: F, mut store: S, deadline: Instant) -> i32
where R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin,
    F: FnMut(&str, Value) -> AuthResult<OAuthActionResponse>,
    S: FnMut(Credential) -> AuthResult<()>,
{
    let result = async {
        emit(out, &json!({"type":"auth_ready","provider":provider,"kind":if oauth {"oauth"} else {"api_key"}}), deadline).await?;
        let mut action = "begin";
        let mut content = json!({});
        let mut count = 0;
        loop {
            if Instant::now() >= deadline { return Err("Authentication timed out"); }
            let display = if oauth {
                let response = callback(action, content)?;
                if Instant::now() >= deadline { return Err("Authentication timed out"); }
                match response.kind {
                    OAuthResponseKind::Credential => {
                        let token = response.credential().map_err(|_| "Invalid OAuth credential")?.ok_or("Missing OAuth credential")?;
                        store(Credential::Oauth { access: token.access, refresh: token.refresh, expires: token.expires_at })?;
                        return Ok(());
                    },
                    OAuthResponseKind::Interaction => response.payload.get("display").cloned().ok_or("Missing public OAuth display")?,
                    _ => return Err("OAuth did not return credentials"),
                }
            } else { json!({"requestedSchema":key_schema("apiKey")}) };
            if count >= MAX_INTERACTIONS { return Err("Too many interactions"); }
            count += 1;
            let id = format!("auth-{count}");
            let display = display.as_object().ok_or("Invalid public OAuth display")?;
            if display.get("url").is_some_and(|v| !v.is_string()) { return Err("Invalid authorization URL"); }
            let url = display.get("url").and_then(Value::as_str);
            let schema = display.get("requestedSchema").or_else(|| display.get("schema")).cloned()
                // No provider schema is invented: this is precisely the existing
                // terminal OAuth input contract, a single opaque `value` string.
                .or_else(|| if url.is_none() { Some(key_schema(if oauth {"value"} else {"apiKey"})) } else { None });
            if schema.as_ref().is_some_and(|s| s["type"] != "object" || serde_json::to_vec(s).map_or(true, |b| b.len() > LIMIT) || !valid_schema(s, 0)) { return Err("Unsupported form schema"); }
            let mut request = json!({"type":"auth_interaction","requestId":id,"mode":if url.is_some() {"url"} else {"form"},"message":"Complete provider authentication"});
            // Only explicit public display data; never forward the opaque payload.
            if let Some(message) = display.get("message").or_else(|| display.get("title")).and_then(Value::as_str) { request["message"] = json!(message); }
            if let Some(instructions) = display.get("instructions") {
                let instructions = instructions.as_array().ok_or("Invalid instructions")?;
                let mut message = request["message"].as_str().unwrap_or_default().to_owned();
                for line in instructions {
                    let line = line.as_str().ok_or("Invalid instructions")?;
                    if message.len().saturating_add(line.len()) + 1 > LIMIT { return Err("Instructions too large"); }
                    message.push('\n'); message.push_str(line);
                }
                request["message"] = json!(message);
            }
            if let Some(url) = url {
                if !valid_url(url) { return Err("Invalid authorization URL"); }
                request["url"] = json!(url);
            }
            if let Some(schema) = &schema { request["requestedSchema"] = schema.clone(); }
            if let Some(id) = display.get("elicitationId").and_then(Value::as_str) { request["elicitationId"] = json!(id); }
            emit(out, &request, deadline).await?;
            content = response(&mut input, &id, deadline).await?;
            if schema.as_ref().is_some_and(|s| !valid_content(s, &content)) { return Err("Invalid form content"); }
            if Instant::now() >= deadline { return Err("Authentication timed out"); }
            if !oauth {
                let key = content["apiKey"].as_str().filter(|k| !k.trim().is_empty()).ok_or("Empty API key")?;
                store(Credential::ApiKey { key: Some(key.to_owned()), env: None })?;
                return Ok(());
            }
            // URL acceptance is only permission to continue. Only an actual
            // credential response from exchange can authenticate the provider.
            action = "exchange";
            content = exchange_input(content);
        }
    }.await;
    let final_value = match result {
        Ok(()) => json!({"type":"auth_result","authenticated":true}),
        Err(error) => json!({"type":"auth_result","authenticated":false,"error":error}),
    };
    if emit(out, &final_value, deadline).await.is_err() || result.is_err() { EXIT_ERROR } else { 0 }
}

#[cfg(test)]
mod tests;
