//! Editor text injection.
//!
//! Lets an extension place text into the interactive editor (the prompt input
//! box) instead of sending it straight to the model. Mirrors
//! [`ExtensionStatusMailbox`](crate::ExtensionStatusMailbox) and
//! [`UiDialogMailbox`](crate::UiDialogMailbox): the plugin writes through the
//! `SetEditorText` runtime action, and the TUI — the only consumer — polls
//! [`EditorTextMailbox::take_pending`] once per tick.
//!
//! This exists for voice input: a transcription is a *draft*, not a message.
//! Pasting it into the editor lets the user fix a mis-recognized word before it
//! becomes part of the conversation, and submitting from the editor renders the
//! user bubble through the normal path.
//!
//! ## Why a queue and not a plain setter
//!
//! Writes must not be lost when the TUI is busy rendering, and a headless run
//! has no editor at all. An unbounded queue of pending edits (each an
//! `append`/`replace` operation) keeps the plugin's call non-blocking and
//! lossless; a headless host simply never drains it.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// The editor's current draft, published by the TUI each tick so a plugin can
/// read it on demand (`GetEditorText`, pi's `ui.getEditorText`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditorDraft {
    /// The draft as shown (folded pastes compressed), same source as
    /// [`crate::EventTag::EditorChange`]'s `chars`.
    pub text: String,
    /// Caret position as a **byte** offset into `text` (UTF-8 boundary
    /// aligned).
    pub cursor: usize,
}

/// How a pending edit combines with what the editor already holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorTextMode {
    /// Replace the whole draft.
    Replace,
    /// Append to the end of the existing draft (separated by a space).
    Append,
}

/// One pending edit to apply to the editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorTextEdit {
    pub text: String,
    pub mode: EditorTextMode,
    /// When true the TUI should start its auto-send countdown for this edit:
    /// the text is a draft the user may correct, but it is sent automatically
    /// once the countdown elapses untouched.
    pub auto_send_ms: Option<u64>,
}

impl EditorTextEdit {
    /// Parse the `SetEditorText` action payload:
    /// `{"text":"...", "mode":"append"|"replace", "autoSendMs": 3000}`.
    pub fn from_json(args: &serde_json::Value) -> Result<Self, String> {
        let text = args
            .get("text")
            .and_then(serde_json::Value::as_str)
            .ok_or("set_editor_text requires a string `text`")?;
        let mode = match args
            .get("mode")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("replace")
            .to_ascii_lowercase()
            .as_str()
        {
            "" | "replace" | "set" => EditorTextMode::Replace,
            "append" => EditorTextMode::Append,
            other => return Err(format!("unknown editor text mode `{other}`")),
        };
        // `0` (or an absent/negative value) disables auto-send.
        let auto_send_ms = args
            .get("autoSendMs")
            .and_then(serde_json::Value::as_u64)
            .filter(|ms| *ms > 0);
        Ok(Self {
            text: text.to_string(),
            mode,
            auto_send_ms,
        })
    }
}

/// Shared, cloneable handle to the pending-edit queue plus the current draft.
#[derive(Clone, Default)]
pub struct EditorTextMailbox {
    pending: Arc<Mutex<VecDeque<EditorTextEdit>>>,
    draft: Arc<Mutex<EditorDraft>>,
}

impl EditorTextMailbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue an edit. Never blocks and never fails (a poisoned lock, which would
    /// mean a panicking reader, just drops the write).
    pub fn push(&self, edit: EditorTextEdit) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.push_back(edit);
        }
    }

    /// Handle the `SetEditorText` runtime action payload.
    pub fn handle(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let edit = EditorTextEdit::from_json(&args)?;
        let len = edit.text.chars().count();
        self.push(edit);
        Ok(serde_json::json!({ "ok": true, "chars": len }))
    }

    /// Remove and return the next queued edit, or `None` when idle.
    pub fn take_pending(&self) -> Option<EditorTextEdit> {
        self.pending.lock().ok()?.pop_front()
    }

    /// Publish the editor's current draft. Called by the TUI whenever the draft
    /// may have changed; cheap enough to call every tick (one mutex + one
    /// compare on the common unchanged path).
    pub fn set_draft(&self, text: &str, cursor: usize) {
        let Ok(mut draft) = self.draft.lock() else {
            return;
        };
        if draft.text == text && draft.cursor == cursor {
            return;
        }
        draft.text.clear();
        draft.text.push_str(text);
        draft.cursor = cursor.min(text.len());
    }

    /// The last published draft (empty on a host with no editor).
    pub fn draft(&self) -> EditorDraft {
        self.draft.lock().map(|d| d.clone()).unwrap_or_default()
    }

    /// Handle the `GetEditorText` runtime action payload (`{}`).
    ///
    /// Never an error: a host without an editor (RPC, print, headless) has an
    /// empty draft, which is a real answer — a plugin degrades instead of
    /// branching on a failure.
    pub fn handle_get(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        let draft = self.draft();
        Ok(serde_json::json!({ "text": draft.text, "cursor": draft.cursor }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_take_round_trip_fifo() {
        let mailbox = EditorTextMailbox::new();
        assert!(mailbox.take_pending().is_none());
        mailbox.push(EditorTextEdit {
            text: "first".into(),
            mode: EditorTextMode::Replace,
            auto_send_ms: None,
        });
        mailbox.push(EditorTextEdit {
            text: "second".into(),
            mode: EditorTextMode::Append,
            auto_send_ms: Some(3000),
        });
        let a = mailbox.take_pending().expect("first");
        assert_eq!(a.text, "first");
        assert_eq!(a.mode, EditorTextMode::Replace);
        let b = mailbox.take_pending().expect("second");
        assert_eq!(b.text, "second");
        assert_eq!(b.mode, EditorTextMode::Append);
        assert_eq!(b.auto_send_ms, Some(3000));
        assert!(mailbox.take_pending().is_none());
    }

    #[test]
    fn handle_defaults_to_replace_without_auto_send() {
        let mailbox = EditorTextMailbox::new();
        let out = mailbox
            .handle(serde_json::json!({"text": "hello"}))
            .expect("ok");
        assert_eq!(out["ok"], serde_json::json!(true));
        assert_eq!(out["chars"], serde_json::json!(5));
        let edit = mailbox.take_pending().unwrap();
        assert_eq!(edit.mode, EditorTextMode::Replace);
        assert_eq!(edit.auto_send_ms, None);
    }

    #[test]
    fn handle_parses_mode_and_auto_send() {
        let mailbox = EditorTextMailbox::new();
        mailbox
            .handle(serde_json::json!({
                "text": "draft", "mode": "append", "autoSendMs": 2500
            }))
            .expect("ok");
        let edit = mailbox.take_pending().unwrap();
        assert_eq!(edit.mode, EditorTextMode::Append);
        assert_eq!(edit.auto_send_ms, Some(2500));
    }

    #[test]
    fn handle_rejects_missing_text_and_unknown_mode() {
        let mailbox = EditorTextMailbox::new();
        assert!(mailbox.handle(serde_json::json!({})).is_err());
        assert!(mailbox
            .handle(serde_json::json!({"text": "x", "mode": "sideways"}))
            .is_err());
        // A rejected payload must not enqueue anything.
        assert!(mailbox.take_pending().is_none());
    }

    #[test]
    fn zero_auto_send_means_manual_only() {
        let mailbox = EditorTextMailbox::new();
        mailbox
            .handle(serde_json::json!({"text": "x", "autoSendMs": 0}))
            .expect("ok");
        assert_eq!(mailbox.take_pending().unwrap().auto_send_ms, None);
    }

    #[test]
    fn get_editor_text_returns_the_published_draft() {
        let mailbox = EditorTextMailbox::new();
        // No editor yet: an empty draft is a real answer, not an error.
        let out = mailbox.handle_get(serde_json::json!({})).expect("ok");
        assert_eq!(out["text"], serde_json::json!(""));
        assert_eq!(out["cursor"], serde_json::json!(0));

        mailbox.set_draft("hello #Bud", 10);
        let out = mailbox.handle_get(serde_json::json!({})).expect("ok");
        assert_eq!(out["text"], serde_json::json!("hello #Bud"));
        assert_eq!(out["cursor"], serde_json::json!(10));
        // Reading does not consume or clear the draft.
        assert_eq!(mailbox.draft().text, "hello #Bud");
    }

    #[test]
    fn set_draft_clamps_the_cursor_to_the_text() {
        let mailbox = EditorTextMailbox::new();
        mailbox.set_draft("abc", 99);
        assert_eq!(mailbox.draft().cursor, 3);
        mailbox.set_draft("你", 1);
        assert_eq!(mailbox.draft().cursor, 1);
    }
}
