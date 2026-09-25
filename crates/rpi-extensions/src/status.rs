//! Extension status line.
//!
//! A tiny host-side registry that lets an extension publish a short status
//! string (e.g. `langfuse ✓ (trace sent)`) for the TUI to render in its footer.
//! Mirrors [`UiDialogMailbox`](crate::UiDialogMailbox): the plugin writes
//! through the `SetStatus` runtime action, and the TUI — the only consumer —
//! polls [`ExtensionStatusMailbox::revision`] once per render tick and repaints
//! only when something actually changed.
//!
//! Keyed by extension name so several extensions can report at once; keys are
//! rendered in sorted order, which keeps the line stable frame to frame. An
//! empty value clears that key. Headless runs simply never read the registry —
//! writing to it is always cheap and never fails.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Shared, cloneable handle to the extension status registry.
///
/// Every clone refers to the same map: the host hands one clone to the action
/// bridge (plugin writes) and one to the TUI (rendering).
#[derive(Clone, Default)]
pub struct ExtensionStatusMailbox {
    entries: Arc<Mutex<BTreeMap<String, String>>>,
    /// Bumped on every accepted write so readers can skip unchanged frames.
    revision: Arc<AtomicU64>,
}

impl ExtensionStatusMailbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set (or, with an empty `value`, clear) one extension's status text.
    ///
    /// Returns `true` when the map actually changed — the caller can use that
    /// to avoid a redundant repaint.
    pub fn set(&self, key: &str, value: &str) -> bool {
        let key = key.trim();
        if key.is_empty() {
            return false;
        }
        let Ok(mut entries) = self.entries.lock() else {
            return false;
        };
        let changed = if value.trim().is_empty() {
            entries.remove(key).is_some()
        } else {
            entries.insert(key.to_string(), value.to_string()) != Some(value.to_string())
        };
        if changed {
            self.revision.fetch_add(1, Ordering::SeqCst);
        }
        changed
    }

    /// Handle the `SetStatus` runtime action: `{"key": "...", "value": "..."}`
    /// (an empty/absent `value` clears the key).
    pub fn handle(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let key = args
            .get("key")
            .and_then(serde_json::Value::as_str)
            .ok_or("set_status requires a string `key`")?;
        let value = args
            .get("value")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let changed = self.set(key, value);
        Ok(serde_json::json!({ "ok": true, "changed": changed }))
    }

    /// Monotonic write counter; changes whenever any entry changed.
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::SeqCst)
    }

    /// The status line: non-empty values joined by two spaces, in key order.
    pub fn text(&self) -> String {
        self.entries
            .lock()
            .map(|entries| {
                entries
                    .values()
                    .filter(|value| !value.trim().is_empty())
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("  ")
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_clear_round_trip() {
        let mailbox = ExtensionStatusMailbox::new();
        assert_eq!(mailbox.text(), "");
        assert!(mailbox.set("langfuse", "langfuse ✓"));
        assert_eq!(mailbox.text(), "langfuse ✓");
        // Re-setting the same value is not a change (no repaint).
        assert!(!mailbox.set("langfuse", "langfuse ✓"));
        assert!(mailbox.set("langfuse", "langfuse ✓ (trace sent)"));
        assert_eq!(mailbox.text(), "langfuse ✓ (trace sent)");
        let before = mailbox.revision();
        // Clearing removes the entry and bumps the revision.
        assert!(mailbox.set("langfuse", ""));
        assert_eq!(mailbox.text(), "");
        assert!(mailbox.revision() > before);
        // Clearing an absent key is a no-op.
        assert!(!mailbox.set("langfuse", ""));
        assert!(!mailbox.set("  ", "ignored"));
    }

    #[test]
    fn multiple_extensions_render_in_key_order() {
        let mailbox = ExtensionStatusMailbox::new();
        mailbox.set("zeta", "z");
        mailbox.set("alpha", "a");
        assert_eq!(mailbox.text(), "a  z");
    }

    #[test]
    fn handle_parses_the_action_payload() {
        let mailbox = ExtensionStatusMailbox::new();
        let out = mailbox
            .handle(serde_json::json!({"key": "langfuse", "value": "✓"}))
            .expect("ok");
        assert_eq!(out["ok"], serde_json::json!(true));
        assert_eq!(mailbox.text(), "✓");
        assert!(mailbox.handle(serde_json::json!({"value": "x"})).is_err());
    }
}
