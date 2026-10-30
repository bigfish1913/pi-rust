//! Opt-in local-network OAuth fixture, never a production OAuth provider.
//! No dependency on the host: only the SDK registration/callback ABI is used.
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Mutex;
use std::time::Duration;

use rpi_plugin_sdk::{PluginApi, StbStringRef};
use serde_json::{json, Value};

// A login owns one native process; URL acceptance advances to the form, not a token.
static FLOW: Mutex<Option<String>> = Mutex::new(None);

// Deliberately small HTTP client: numeric IPv4 loopback only, no proxy, bounded
// response, connect/read/write deadlines, no diagnostics containing token bytes.
fn http(path: &str, body: Option<Value>) -> Result<Value, ()> {
    let address = std::env::var("RPI_STUB_OAUTH_ADDRESS").map_err(|_| ())?;
    let address: SocketAddr = address.parse().map_err(|_| ())?;
    if !address.ip().is_loopback() || !address.is_ipv4() { return Err(()); }
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5)).map_err(|_| ())?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).map_err(|_| ())?;
    stream.set_write_timeout(Some(Duration::from_secs(5))).map_err(|_| ())?;
    let text = body.as_ref().map(Value::to_string).unwrap_or_default();
    let method = if body.is_some() { "POST" } else { "GET" };
    write!(stream, "{method} {path} HTTP/1.0\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len()).map_err(|_| ())?;
    let mut bytes = Vec::new();
    stream.take(65537).read_to_end(&mut bytes).map_err(|_| ())?;
    if bytes.len() > 65536 { return Err(()); }
    let text = std::str::from_utf8(&bytes).map_err(|_| ())?;
    let (header, body) = text.split_once("\r\n\r\n").ok_or(())?;
    if !header.lines().next().is_some_and(|s| s.contains(" 200 ")) { return Err(()); }
    serde_json::from_str(body).map_err(|_| ())
}

fn handle(req: Value) -> Result<Value, ()> {
    if req["providerId"] != "stub-oauth-test" { return Err(()); }
    let mut flow = FLOW.lock().map_err(|_| ())?;
    match req["action"].as_str() {
        Some("begin") => {
            let begin = http("/begin", None)?;
            *flow = Some(begin["state"].as_str().ok_or(())?.to_owned());
            Ok(json!({"version":1,"kind":"interaction","payload":{
                "display":{"url":begin["url"],"message":"Authorize with the local OAuth test server"},
                "private":begin["private"]
            }}))
        }
        Some("exchange") => {
            let expected = flow.as_ref().ok_or(())?;
            let input = &req["input"];
            if input.as_object().is_some_and(|obj| obj.is_empty()) {
                return Ok(json!({"version":1,"kind":"interaction","payload":{"display":{
                    "message":"Enter the local authorization code and state",
                    "requestedSchema":{"type":"object","properties":{
                        "code":{"type":"string","password":true,"minLength":1},
                        "state":{"type":"string","minLength":1}
                    },"required":["code","state"],"additionalProperties":false}
                }}}));
            }
            if input["state"].as_str() != Some(expected.as_str()) { return Err(()); }
            let token = http("/token", Some(json!({
                "grant_type":"authorization_code", "code":input["code"], "state":input["state"]
            })))?;
            let access = token["access_token"].as_str().filter(|s| !s.is_empty()).ok_or(())?;
            let refresh = token["refresh_token"].as_str().ok_or(())?;
            *flow = None;
            Ok(json!({"version":1,"kind":"credential","payload":{
                "access":access,"refresh":refresh,"expires_at":token["expires_at"]
            }}))
        }
        Some("revoke") => http("/revoke", Some(req["input"].clone())),
        _ => Err(()),
    }
}

rpi_plugin_sdk::string_handler! {
    fn request(req, out, _user_data) {
        let value = serde_json::from_str(req).map_err(|_| ())
            .and_then(handle)
            .unwrap_or_else(|_| json!({"version":1,"kind":"error","payload":{"message":"Local OAuth test failed"}}));
        i32::from(!out.write(value.to_string()))
    }
}

pub(super) fn register(api: &PluginApi) -> i32 {
    if std::env::var("RPI_STUB_OAUTH_TEST").as_deref() != Ok("1") { return 0; }
    let Some(register) = api.register_oauth_provider else { return 1; };
    register(
        StbStringRef::from_str(r#"{"id":"stub-oauth-test","displayName":"Local OAuth test only","scopes":[],"supportsPkce":false}"#),
        request, rpi_plugin_sdk::callbacks::free_string, std::ptr::null_mut(),
    )
}
