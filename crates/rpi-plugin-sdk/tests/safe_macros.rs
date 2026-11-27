//! A plugin can do everything the ABI requires without a single `unsafe` block.
//!
//! `call_action!`, `stb_text!` and `stb_take!` join `string_handler!`,
//! `execute_handler!` and `event_handler!` so raw ABI string handling stays
//! inside the SDK. This test is the proof: every **plugin-side** line compiles
//! under `#![deny(unsafe_code)]`, which is what a real plugin crate enables at
//! its root.
//!
//! The one exception is [`mod host`] below, which deliberately plays the other
//! side of the ABI — it owns and writes the raw pointers, exactly like
//! `rpi-cli` does, and therefore has to use `unsafe`. Nothing in that module is
//! code a plugin would write.

#![deny(unsafe_code)]

use std::ffi::c_void;

use rpi_plugin_sdk as sdk;
use sdk::{StbString, StbStringRef};

/// The host half of the ABI: allocates the out slot, hands over the request and
/// reclaims what the plugin produces.
mod host {
    // This module plays `rpi-cli`, so it does the raw-pointer work a host does.
    #![allow(unsafe_code)]

    use super::*;
    use std::cell::Cell;

    thread_local! {
        static FREES: Cell<usize> = const { Cell::new(0) };
    }

    /// How many plugin-produced allocations this host reclaimed.
    pub fn frees() -> usize {
        FREES.with(Cell::get)
    }

    /// Reset the counter; call at the start of a test that owns the host.
    pub fn reset() {
        FREES.with(|count| count.set(0));
    }

    unsafe extern "C" fn counting_free(value: StbString) {
        if !value.is_empty() {
            FREES.with(|count| count.set(count.get() + 1));
            // SAFETY: the plugin transferred this allocation to the host.
            unsafe { sdk::callbacks::free_string(value) };
        }
    }

    /// The counting deallocator, for plugin code that must reclaim an
    /// allocation the host handed it (a tool `execute` parameter, for example).
    pub fn counting_free_for_tests() -> sdk::FreeStringFn {
        counting_free
    }

    /// A host that echoes the request envelope back inside the response.
    extern "C" fn echo_action(
        _id: u32,
        args: StbStringRef,
        out: *mut StbString,
        _user_data: *mut c_void,
    ) -> i32 {
        let echoed = sdk::stb_text!(args);
        let body = format!(r#"{{"echo":{}}}"#, serde_json::json!(echoed));
        // SAFETY: the host gives every action trampoline an exclusive out slot.
        unsafe { *out = StbString::from_string(body) };
        0
    }

    /// Minimal stand-in for the host side of `runtime_action` + `free_string`.
    #[derive(Clone, Copy)]
    pub struct FakeHost {
        pub runtime_action: sdk::RuntimeActionFn,
        pub free_string: sdk::FreeStringFn,
        pub user_data: *mut c_void,
    }

    pub fn fake_host() -> FakeHost {
        FakeHost {
            runtime_action: echo_action,
            free_string: counting_free,
            user_data: std::ptr::null_mut(),
        }
    }
}

#[test]
fn outbound_macros_need_no_unsafe_in_plugin_code() {
    host::reset();
    let host = host::fake_host();
    let (rc, body) = sdk::call_action!(host, 27u32, r#"{"reason":"stop"}"#);
    assert_eq!(rc, 0);
    assert_eq!(body, r#"{"echo":"{\"reason\":\"stop\"}"}"#);
    // The response allocation went back to the host exactly once.
    assert_eq!(host::frees(), 1);

    // Read-only copy of a string the plugin owns: no free, no unsafe.
    let owned = StbString::from_string("still mine".into());
    assert_eq!(sdk::stb_text!(owned), "still mine");
    assert!(!owned.is_empty());
    // Reclaim it the safe way.
    assert_eq!(
        sdk::stb_take!(owned, Some(host::counting_free_for_tests())),
        "still mine"
    );
    assert_eq!(host::frees(), 2);

    let discarded = StbString::from_string("discarded".into());
    sdk::stb_drop!(discarded, Some(host::counting_free_for_tests()));
    sdk::stb_drop!(StbString::empty(), Some(host::counting_free_for_tests()));
    assert_eq!(host::frees(), 3);
}

#[test]
fn fake_host_tool_registration_needs_no_unsafe() {
    sdk::tool_schema_handler!(capture, |schema, execute, (poll, cancel, destroy), free| {
        assert_eq!(sdk::stb_take!(schema.name, Some(free)), "test-tool");
        sdk::stb_drop!(schema.description, Some(free));
        sdk::stb_drop!(schema.parameters, Some(free));
        let handle = execute(StbStringRef::from_str("call-1"), StbString::empty(), None);
        cancel(handle);
        assert_eq!(
            sdk::take_step!(poll(handle, None, std::ptr::null_mut())),
            sdk::callbacks::Step::Done("output".into())
        );
        destroy(handle);
        17
    });
    sdk::tool_schema_handler!(panics, |_schema, _execute, _data| {
        panic!("fake host failed");
    });
    sdk::execute_handler! {
        fn execute(id, _params) {
            assert_eq!(id, "call-1");
            std::ptr::null_mut()
        }
    }
    extern "C" fn poll(
        _: sdk::StepHandle,
        _: Option<sdk::ToolPartialCb>,
        _: *mut c_void,
    ) -> sdk::StepResult {
        sdk::StepResult::done(StbString::from_string("output".into()))
    }
    extern "C" fn cleanup(_: sdk::StepHandle) {}

    host::reset();
    let free = host::counting_free_for_tests();
    let schema = sdk::StableToolSchema {
        name: StbString::from_string("test-tool".into()),
        description: StbString::from_string("test description".into()),
        parameters: StbString::from_string("{}".into()),
    };
    assert_eq!(capture(&schema, execute, poll, cleanup, cleanup, free), 17);
    assert_eq!(host::frees(), 3);
    assert_eq!(
        capture(std::ptr::null(), execute, poll, cleanup, cleanup, free),
        1
    );
    let empty = sdk::StableToolSchema {
        name: StbString::empty(),
        description: StbString::empty(),
        parameters: StbString::empty(),
    };
    assert_eq!(panics(&empty, execute, poll, cleanup, cleanup, free), 1);
}

#[test]
fn taking_a_step_result_needs_no_unsafe_in_caller_code() {
    host::reset();
    // A plugin produced this in `poll`; the host has to copy and reclaim it.
    let done = sdk::take_step!(sdk::StepResult::done(StbString::from_string(
        r#"{"content":[]}"#.into(),
    )));
    match done {
        sdk::callbacks::Step::Done(body) => assert_eq!(body, r#"{"content":[]}"#),
        other => panic!("expected Done, got {other:?}"),
    }
    let failed = sdk::take_step!(sdk::StepResult::err(StbString::from_string("boom".into(),)));
    assert_eq!(failed, sdk::callbacks::Step::Err("boom".into()));
    let pending = sdk::take_step!(sdk::StepResult::pending(StbString::from_string(
        r#"{"progress":0.5}"#.into(),
    )));
    assert_eq!(
        pending,
        sdk::callbacks::Step::Pending(r#"{"progress":0.5}"#.into())
    );

    // A foreign plugin's result must be reclaimed with *its* deallocator.
    let foreign = sdk::take_step_with!(
        sdk::StepResult::done(StbString::from_string("x".into())),
        host::counting_free_for_tests()
    );
    assert_eq!(foreign, sdk::callbacks::Step::Done("x".into()));
    // The three local results were reclaimed through this SDK's own deallocator
    // (`take_step!`), so only the foreign one shows up on the counted host free.
    assert_eq!(host::frees(), 1);
}

#[test]
fn a_command_handler_needs_no_unsafe_in_plugin_code() {
    sdk::string_handler! {
        fn probe_command(input, out, _user_data) {
            out.write(format!(r#"{{"kind":"message","text":"{input}"}}"#));
            0
        }
    }

    let mut slot = StbString::empty();
    let rc = probe_command(
        StbStringRef::from_str("hello"),
        &mut slot,
        std::ptr::null_mut(),
    );
    assert_eq!(rc, 0);
    assert_eq!(sdk::stb_text!(slot), r#"{"kind":"message","text":"hello"}"#);
}

#[test]
fn a_tool_execute_handler_needs_no_unsafe_in_plugin_code() {
    host::reset();
    sdk::execute_handler! {
        fn probe_execute(id, params) {
            assert_eq!(id, "call-1");
            assert_eq!(params.to_string_lossy(), r#"{"a":1}"#);
            std::ptr::null_mut()
        }
    }

    let params = StbString::from_string(r#"{"a":1}"#.into());
    let handle = probe_execute(
        StbStringRef::from_str("call-1"),
        params,
        Some(host::counting_free_for_tests()),
    );
    assert!(handle.is_null());
    // `execute_handler!` reclaims the parameters it was handed.
    assert_eq!(host::frees(), 1);
}
