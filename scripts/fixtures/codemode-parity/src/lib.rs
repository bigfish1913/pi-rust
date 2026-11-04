//! Disposable native ABI probes. Only parity_large reads fixed test scratch files.
#![deny(unsafe_code)]

use rpi_plugin_sdk::{
    handles::ToolHandles, StableToolSchema, StbString, StepHandle, StepResult, ToolPartialCb,
};
use serde_json::{json, Value};
use std::{io::Read, sync::atomic::Ordering};

static JOBS: ToolHandles<Option<Value>> = ToolHandles::new();

rpi_plugin_sdk::execute_handler! { fn execute_large(_id, params) {
    let mut value: Value = serde_json::from_str(&params.to_string_lossy()).unwrap_or(Value::Null);
    // Private fixture dispatch marker is inserted only after ABI argument decoding.
    value["fixtureLarge"] = json!(true);
    JOBS.insert(Some(value))
} }

rpi_plugin_sdk::execute_handler! { fn execute(_id, params) {
    let value = serde_json::from_str(&params.to_string_lossy()).unwrap_or(Value::Null);
    JOBS.insert(Some(value))
} }

extern "C" fn poll(
    handle: StepHandle,
    partial: Option<ToolPartialCb>,
    data: *mut std::ffi::c_void,
) -> StepResult {
    rpi_plugin_sdk::guard(|| {
        let Some(entry) = JOBS.get(handle) else {
            return StepResult::err(StbString::from_string("unknown fixture handle".into()));
        };
        if entry.cancelled.load(Ordering::SeqCst) {
            return StepResult::err(StbString::from_string("fixture cancelled".into()));
        }
        let Some(params) = entry.lock().take() else {
            return StepResult::err(StbString::from_string("fixture already completed".into()));
        };
        // Ownership passes to the host, which uses this plugin's deallocator.
        if let Some(callback) = partial {
            callback(StbString::from_string(json!({"content":[{"type":"text","text":"fixture partial"}],"details":{"fixturePartial":true}}).to_string()), data);
        }
        if params["fixtureLarge"] == true {
            // Never accept a path from JS/model arguments. These files are generated
            // by the smoke harness in the disposable host workspace before launch.
            let path = match params["kind"].as_str() {
                Some("text" | "error") => "large-native-text-result.json",
                Some("image") => "large-native-image-result.json",
                _ => return StepResult::err(StbString::from_string("invalid large fixture kind".into())),
            };
            let mut payload = String::new();
            let read = std::fs::File::open(path).and_then(|file| {
                file.take(8 * 1024 * 1024 + 1).read_to_string(&mut payload)
            });
            if read.is_err() || payload.len() > 8 * 1024 * 1024 {
                return StepResult::err(StbString::from_string("large fixture unavailable/oversized".into()));
            }
            let mut result: Value = match serde_json::from_str(&payload) {
                Ok(value) => value,
                Err(_) => return StepResult::err(StbString::from_string("invalid large fixture JSON".into())),
            };
            if params["kind"] == "error" {
                // Exercise real AgentError finalization: no fabricated accounting,
                // names, structured-error envelope, or terminal flag on this path.
                return StepResult::err(StbString::from_string(
                    result["content"][0]["text"].as_str().unwrap_or("").to_owned()));
            }
            if let Some(business) = params.get("business") {
                result["details"]["business"] = business.clone();
            }
            result["terminate"] = json!(params["terminate"] == true);
            return StepResult::done(StbString::from_string(result.to_string()));
        }
        let mut details = json!({"fixture":"native-parity"});
        if let Some(business) = params.get("business") {
            details["business"] = business.clone();
        }
        if params["echoBinding"] == true {
            // Echo the real child invocation. The outer CodeMode DLL has a
            // different credential, so it cannot sanitize a missed child leak.
            let binding = &params[rpi_plugin_sdk::HOST_CONTEXT_KEY];
            let key = binding["runtimeKey"].as_str().unwrap_or("missing-binding");
            details["bindingEcho"] = json!({
                "metadata":binding, "key":key, "handle":binding["runtimeHandle"],
                "embedded":format!("before:{key}:after"),
                "serialized":json!({"metadata":binding,"key":key,
                    "business":params["business"]}).to_string(),
                "business":params["business"]
            });
        }
        let text = if params["echoBinding"] == true {
            details.to_string()
        } else {
            "native parity probe".into()
        };
        if params["oversized"] == true {
            // Deliberately tests ABI envelope rejection, NOT retained accounting:
            // opted-in envelopes above 1 MiB are rejected before ToolProgram.
            details["padding"] = json!("x".repeat(1024 * 1024));
        }
        let result = json!({"$rpiToolResult":1,"isError":params["error"] == true,"result":{
            "content":[{"type":"text","text":text,"textSignature":"native-parity-signature"}],"details":details,
            "terminate":params["terminate"] == true,
            "usage":{"input":1,"output":2,"cacheRead":0,"cacheWrite":0,"totalTokens":3,
                "cost":{"input":0.1,"output":0.2,"cacheRead":0.0,"cacheWrite":0.0,"total":0.3}},
            "addedToolNames":["fixture-added","fixture-added"]}});
        StepResult::done(StbString::from_string(result.to_string()))
    }).unwrap_or_else(|_| StepResult::err(StbString::from_string("fixture panic".into())))
}

extern "C" fn cancel(handle: StepHandle) {
    let _ = rpi_plugin_sdk::guard(|| JOBS.cancel(handle));
}
extern "C" fn destroy(handle: StepHandle) {
    let _ = rpi_plugin_sdk::guard(|| {
        JOBS.remove(handle);
    });
}

// The SDK macro owns the raw entrypoint boundary; fixture code remains safe.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
mod registration {
    use super::*;
    rpi_plugin_sdk::export_plugin!(|api| {
        let schema = StableToolSchema {
        name: StbString::from_string("parity_probe".into()),
        description: StbString::from_string("Test-only native ABI parity probe; no side effects".into()),
        parameters: StbString::from_string(json!({"type":"object","x-rpi-result-envelope":1,
            "properties":{"error":{"type":"boolean"},"terminate":{"type":"boolean"},"oversized":{"type":"boolean"},
                "business":{"type":"object","additionalProperties":true,
                    "properties":{"runtimeKey":{"type":"string"},"runtimeHandle":{},
                        "__rpi":{"type":"object","additionalProperties":true,
                            "properties":{"runtimeKey":{"type":"string"}}},
                        "__rpiRuntimeKey":{"type":"string"}}},
                "echoBinding":{"type":"boolean"}},
            "additionalProperties":false}).to_string()),
    };
        match api.register_tool {
            Some(register) => {
                // Reserved guest helpers remain reachable as native aliases
                // through tools.call(name, args).
                for name in ["parity_probe", "call", "list", "emit", "result", "release"] {
                    let alias = StableToolSchema {
                        name: StbString::from_string(name.into()),
                        description: StbString::from_string(schema.description.to_string_lossy()),
                        parameters: StbString::from_string(schema.parameters.to_string_lossy()),
                    };
                    let status = register(
                        &alias,
                        execute,
                        poll,
                        cancel,
                        destroy,
                        rpi_plugin_sdk::callbacks::free_string,
                    );
                    if status != 0 {
                        return status;
                    }
                }
                // Deliberately legacy: no x-rpi-result-envelope opt-in (1 MiB cap).
                let large = StableToolSchema {
                    name: StbString::from_string("parity_large".into()),
                    description: StbString::from_string(
                        "Test-only fixed scratch large native result".into(),
                    ),
                    parameters: StbString::from_string(
                        json!({"type":"object",
                        "properties":{"kind":{"type":"string","enum":["text","image","error"]},
                            "business":{"type":"object","additionalProperties":true},
                            "terminate":{"type":"boolean"}},
                        "required":["kind"],"additionalProperties":false})
                        .to_string(),
                    ),
                };
                register(
                    &large,
                    execute_large,
                    poll,
                    cancel,
                    destroy,
                    rpi_plugin_sdk::callbacks::free_string,
                )
            }
            None => 1,
        }
    });
}
