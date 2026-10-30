//! Borrowed ABI event views with tag-checked, owned payload copies.
use crate::{EventTag, StablePluginEvent};

pub struct EventView<'a> {
    raw: &'a StablePluginEvent,
}

pub struct ToolCall {
    pub tool_call_id: String,
    pub tool_name: String,
    pub params: String,
}

pub struct ToolResult {
    pub tool_call_id: String,
    pub tool_name: String,
    pub result: String,
    pub is_error: bool,
}

impl<'a> EventView<'a> {
    /// Create a view at an ABI callback boundary.
    ///
    /// # Safety
    /// The host must initialize the payload variant specified by `tag` and
    /// keep every payload buffer valid until the callback returns. The view
    /// borrows the event; accessors copy strings and never free host memory.
    pub unsafe fn from_raw(raw: &'a StablePluginEvent) -> Self {
        Self { raw }
    }

    pub fn tag(&self) -> EventTag {
        self.raw.tag
    }

    pub fn message_text(&self) -> Option<String> {
        if !matches!(
            self.tag(),
            EventTag::MessageStart | EventTag::MessageUpdate | EventTag::MessageEnd
        ) {
            return None;
        }
        // SAFETY: from_raw's invariant and the tag check select the live arm.
        Some(unsafe { self.raw.payload.message.message.to_string_lossy() })
    }

    pub fn data_text(&self) -> Option<String> {
        if !matches!(
            self.tag(),
            EventTag::SessionBeforeSwitch
                | EventTag::SessionInfoChanged
                | EventTag::SessionBeforeFork
                | EventTag::SessionBeforeCompact
                | EventTag::SessionCompact
                | EventTag::SessionBeforeTree
                | EventTag::SessionTree
                | EventTag::Context
                | EventTag::BeforeProviderRequest
                | EventTag::BeforeProviderHeaders
                | EventTag::AfterProviderResponse
                | EventTag::BeforeAgentStart
                | EventTag::ModelSelect
                | EventTag::ThinkingLevelSelect
                | EventTag::UserBash
                | EventTag::Input
                | EventTag::UiPromptStart
                | EventTag::UiPromptEnd
                | EventTag::EditorChange
                // `turn_end` carries `{outcome, stopReason, turnIndex,
                // toolResultCount}`. Older hosts send it as an empty payload;
                // the reader below therefore treats an empty string as absent
                // rather than parsing nulls.
                | EventTag::TurnEnd
        ) {
            return None;
        }
        // SAFETY: from_raw's invariant and the tag check select the live arm.
        let text = unsafe { self.raw.payload.data.data.to_string_lossy() };
        // An older host sends `turn_end` with the empty variant, whose `data`
        // field is a null `StbString`. Surface that as `None` so a consumer's
        // `?`-chain degrades to "no payload" instead of parsing `""`.
        (!text.is_empty()).then_some(text)
    }

    pub fn tool_call(&self) -> Option<ToolCall> {
        if !matches!(
            self.tag(),
            EventTag::ToolCall | EventTag::ToolExecutionStart | EventTag::ToolExecutionUpdate
        ) {
            return None;
        }
        // SAFETY: from_raw's invariant and the tag check select the live arm.
        let payload = unsafe { self.raw.payload.tool_call };
        // SAFETY: from_raw guarantees the selected payload buffers remain live.
        Some(unsafe {
            ToolCall {
                tool_call_id: payload.tool_call_id.to_string_lossy(),
                tool_name: payload.tool_name.to_string_lossy(),
                params: payload.params.to_string_lossy(),
            }
        })
    }

    pub fn tool_result(&self) -> Option<ToolResult> {
        if !matches!(
            self.tag(),
            EventTag::ToolResult | EventTag::ToolExecutionEnd
        ) {
            return None;
        }
        // SAFETY: from_raw's invariant and the tag check select the live arm.
        let payload = unsafe { self.raw.payload.tool_result };
        // SAFETY: from_raw guarantees the selected payload buffers remain live.
        Some(unsafe {
            ToolResult {
                tool_call_id: payload.tool_call_id.to_string_lossy(),
                tool_name: payload.tool_name.to_string_lossy(),
                result: payload.result.to_string_lossy(),
                is_error: payload.is_error != 0,
            }
        })
    }
}

/// Define an ABI event handler whose body uses an `EventView`.
///
/// The generated trampoline contains the host's ABI validity assumption and
/// catches panics before they can unwind across `extern "C"`. Payload strings
/// copied by the view may safely be retained after the callback returns.
#[macro_export]
macro_rules! event_handler {
    ($(#[$attr:meta])* fn $name:ident($event:ident, $data:ident) $body:block) => {
        $(#[$attr])*
        extern "C" fn $name(raw: $crate::StablePluginEvent, $data: *mut std::ffi::c_void) -> i32 {
            $crate::guard_or($crate::EVENT_HANDLER_ERROR, || {
                // SAFETY: this is the host-owned ABI callback; its event and
                // buffers remain valid for the duration of this invocation.
                let $event = unsafe { $crate::events::EventView::from_raw(&raw) };
                $body
            })
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StbStringRef;

    // Borrowing a Rust string gives this test a live buffer without allocating
    // or leaking an ABI string that would need an unsafe deallocator.
    fn borrowed_string(text: &str) -> crate::StbString {
        let borrowed = StbStringRef::from_str(text);
        crate::StbString {
            ptr: borrowed.ptr as *mut _,
            len: borrowed.len,
        }
    }

    #[test]
    fn wrong_accessors_do_not_read_inactive_union_arms() {
        let raw = StablePluginEvent::empty(EventTag::AgentStart);
        let view = unsafe { EventView::from_raw(&raw) };
        assert!(view.message_text().is_none());
        assert!(view.data_text().is_none());
        assert!(view.tool_call().is_none());
        assert!(view.tool_result().is_none());
    }

    #[test]
    fn copied_message_outlives_the_borrowed_event_buffer() {
        let copy = {
            let text = String::from("消息");
            let raw = StablePluginEvent::message(EventTag::MessageEnd, borrowed_string(&text));
            let view = unsafe { EventView::from_raw(&raw) };
            assert!(view.tool_result().is_none());
            view.message_text().unwrap()
        };
        assert_eq!(copy, "消息");
    }

    #[test]
    fn tool_payloads_are_copied_with_the_error_flag() {
        let raw = StablePluginEvent::tool_result(
            EventTag::ToolExecutionEnd,
            borrowed_string("call"),
            borrowed_string("tool"),
            borrowed_string("{}"),
            true,
        );
        let view = unsafe { EventView::from_raw(&raw) };
        let result = view.tool_result().unwrap();
        assert_eq!(result.tool_call_id, "call");
        assert_eq!(result.tool_name, "tool");
        assert_eq!(result.result, "{}");
        assert!(result.is_error);
        assert!(view.tool_call().is_none());
    }

    event_handler! { fn panicking(_event, _data) { panic!("handler panic") } }

    #[test]
    fn data_and_call_accessors_select_only_their_own_payloads() {
        let raw = StablePluginEvent::data(EventTag::BeforeProviderRequest, borrowed_string("{}"));
        let view = unsafe { EventView::from_raw(&raw) };
        assert_eq!(view.data_text().as_deref(), Some("{}"));
        assert!(view.message_text().is_none());
        let raw = StablePluginEvent::tool_call(
            EventTag::ToolCall,
            borrowed_string("call"),
            borrowed_string("tool"),
            borrowed_string("{}"),
        );
        let view = unsafe { EventView::from_raw(&raw) };
        let call = view.tool_call().unwrap();
        assert_eq!(call.tool_call_id, "call");
        assert_eq!(call.tool_name, "tool");
        assert_eq!(call.params, "{}");
        assert!(view.data_text().is_none());
    }

    #[test]
    fn handler_panic_returns_error_without_crossing_abi() {
        assert_eq!(
            panicking(
                StablePluginEvent::empty(EventTag::AgentStart),
                std::ptr::null_mut()
            ),
            crate::EVENT_HANDLER_ERROR
        );
    }
}
