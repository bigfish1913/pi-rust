//! Plugin-contributed completion, adapted onto the TUI's provider trait.
//!
//! Two pi capabilities land here, both registered through the plugin ABI:
//!
//! - `PluginApi::register_autocomplete_provider` (pi's
//!   `ui.addAutocompleteProvider`): candidates for the draft itself, which is
//!   how an extension offers `#document`-style selection.
//! - `PluginApi::register_command_completions` (pi's
//!   `RegisteredCommand.getArgumentCompletions`): candidates for the arguments
//!   of a slash command it registered, e.g. `/office con<Tab>`.
//!
//! Both providers read the **current** extension session on every query rather
//! than capturing a snapshot at startup, so a `/reload` that registers new
//! plugins or commands takes effect without rebuilding the provider chain.
//!
//! Plugin handlers run **outside** the session lock: the provider clones the
//! registrations out, drops the guard, and only then calls into the plugin. A
//! handler that calls back into `runtime_action` therefore cannot deadlock
//! against the TUI thread that is asking for suggestions.

use std::ffi::c_void;
use std::sync::Arc;

use rpi_plugin_sdk::{FreeStringFn, StbString, StbStringRef};
use rpi_tui::{AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions};

use rpi_extensions::{RegisteredAutocompleteProvider, RegisteredCommand};

use crate::session::ExtensionSessionCell;

/// Hard ceiling on candidates from one plugin, so a runaway provider cannot
/// stall the render tick. The built-in providers cap themselves similarly.
const MAX_ITEMS: usize = 50;

/// The shared ABI shape of both completion callbacks: a borrowed query in, an
/// owning JSON out.
type CompletionFn = extern "C" fn(StbStringRef, *mut StbString, *mut c_void) -> i32;

/// Provider that offers plugin candidates for the whole draft.
pub(super) struct PluginDraftAutocomplete {
    session: ExtensionSessionCell,
}

impl PluginDraftAutocomplete {
    pub(super) fn new(session: ExtensionSessionCell) -> Self {
        Self { session }
    }
}

impl AutocompleteProvider for PluginDraftAutocomplete {
    fn get_suggestions(&self, input: &str, cursor: usize) -> Option<AutocompleteSuggestions> {
        let providers = {
            let guard = self.session.lock().ok()?;
            let snapshot = guard.snapshot_arc()?;
            if !snapshot.is_active() {
                return None;
            }
            snapshot.autocomplete_providers().to_vec()
        };
        draft_suggestions(&providers, input, cursor)
    }

    fn name(&self) -> &str {
        "plugin"
    }
}

/// Provider that offers argument completions for extension slash commands.
pub(super) struct PluginCommandArguments {
    session: ExtensionSessionCell,
}

impl PluginCommandArguments {
    pub(super) fn new(session: ExtensionSessionCell) -> Self {
        Self { session }
    }
}

impl AutocompleteProvider for PluginCommandArguments {
    fn get_suggestions(&self, input: &str, cursor: usize) -> Option<AutocompleteSuggestions> {
        // Cheap shape check first: only `/name <args>` can have command
        // arguments, so ordinary typing never takes the session lock.
        let before = input.get(..cursor)?;
        let command = before.strip_prefix('/')?;
        if !command.contains(char::is_whitespace) {
            return None;
        }
        let commands = {
            let guard = self.session.lock().ok()?;
            let snapshot = guard.snapshot_arc()?;
            if !snapshot.is_active() {
                return None;
            }
            if !snapshot
                .commands()
                .iter()
                .any(|registered| registered.completions.is_some())
            {
                return None;
            }
            snapshot.commands().to_vec()
        };
        command_argument_suggestions(&commands, input, cursor)
    }

    fn name(&self) -> &str {
        "plugin_command_args"
    }
}

/// The plugin-draft completion rule, split out so it is testable without a live
/// extension session (the providers above only add the snapshot lookup).
pub(super) fn draft_suggestions(
    providers: &[RegisteredAutocompleteProvider],
    input: &str,
    cursor: usize,
) -> Option<AutocompleteSuggestions> {
    if providers.is_empty() {
        return None;
    }
    let cursor = cursor.min(input.len());
    if !input.is_char_boundary(cursor) {
        return None;
    }
    let query = serde_json::json!({
        "text": input,
        "cursor": cursor,
        "line": line_at(input, cursor),
        "linePrefix": line_prefix(input, cursor),
    })
    .to_string();
    for provider in providers {
        // rc != 0 or an empty list means "no opinion": fall through to the next
        // plugin and, after them all, to the built-in providers.
        let Some(items) = call_completion(
            provider.handler,
            provider.plugin_free_string,
            provider.user_data,
            &query,
        ) else {
            continue;
        };
        if items.is_empty() {
            continue;
        }
        let start = token_start(input, cursor);
        return Some(AutocompleteSuggestions {
            items,
            start,
            end: cursor,
            is_path: false,
        });
    }
    None
}

/// The slash-command argument rule, split out like [`draft_suggestions`].
pub(super) fn command_argument_suggestions(
    commands: &[RegisteredCommand],
    input: &str,
    cursor: usize,
) -> Option<AutocompleteSuggestions> {
    let cursor = cursor.min(input.len());
    if !input.is_char_boundary(cursor) {
        return None;
    }
    let before = input.get(..cursor)?;
    let after_slash = before.strip_prefix('/')?;
    // The separator itself may be more than one byte (U+3000, U+00A0), so the
    // argument span starts after the whole whitespace character — an `index + 1`
    // shortcut would slice mid-character and panic.
    let (name, arguments_start) = after_slash.find(char::is_whitespace).map(|index| {
        let separator = after_slash[index..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(1);
        (&after_slash[..index], 1 + index + separator)
    })?;
    let argument_prefix = &before[arguments_start..];
    let command = commands
        .iter()
        .find(|registered| registered.name == name && registered.completions.is_some())?;
    let completions = command.completions?;
    let query = serde_json::json!({ "argumentPrefix": argument_prefix }).to_string();
    let items = call_completion(
        completions.handler,
        completions.plugin_free_string,
        completions.user_data,
        &query,
    )?;
    if items.is_empty() {
        return None;
    }
    Some(AutocompleteSuggestions {
        items,
        start: arguments_start,
        end: cursor,
        is_path: false,
    })
}

/// Invoke one plugin callback and decode its candidates.
///
/// Returns `None` for "no opinion" (nonzero rc, malformed JSON, or no `items`
/// key). The plugin's own `free_string` reclaims the output buffer: the
/// allocation belongs to the plugin's SDK instance, not to this process's
/// allocator view.
fn call_completion(
    handler: CompletionFn,
    plugin_free_string: FreeStringFn,
    user_data: *mut c_void,
    query: &str,
) -> Option<Vec<AutocompleteItem>> {
    let mut out = StbString::empty();
    let rc = handler(StbStringRef::from_str(query), &mut out, user_data);
    // SAFETY: `out` was filled by the callback above with a live allocation (or
    // left empty), and `to_string_lossy` only reads it.
    let text = unsafe { out.to_string_lossy() };
    // SAFETY: the callback produced `out`, so its producer frees it exactly
    // once. `to_string_lossy` already copied what we need.
    unsafe { plugin_free_string(out) };
    if rc != 0 {
        return None;
    }
    parse_items(&text)
}

/// Decode `{"items":[{"text","label?","description?","insertSpace?",
/// "isDirectory?"}]}`. `insertText` is accepted as an alias of `text` so a
/// port of a pi extension that used pi's field name still works.
fn parse_items(text: &str) -> Option<Vec<AutocompleteItem>> {
    if text.trim().is_empty() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let items = value.get("items")?.as_array()?;
    let mut out = Vec::new();
    for item in items.iter().take(MAX_ITEMS) {
        let Some(insert) = item
            .get("text")
            .or_else(|| item.get("insertText"))
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        if insert.is_empty() {
            continue;
        }
        let mut entry = AutocompleteItem::new(insert);
        if let Some(label) = item.get("label").and_then(serde_json::Value::as_str) {
            entry = entry.with_label(label);
        }
        if let Some(description) = item.get("description").and_then(serde_json::Value::as_str) {
            entry = entry.with_description(description);
        }
        if item.get("insertSpace").and_then(serde_json::Value::as_bool) == Some(false) {
            entry = entry.with_insert_space(false);
        }
        if item.get("isDirectory").and_then(serde_json::Value::as_bool) == Some(true) {
            entry = entry.as_directory();
        }
        out.push(entry);
    }
    Some(out)
}

/// Start of the whitespace-delimited token the caret sits in — the span the TUI
/// replaces when a suggestion is accepted.
fn token_start(input: &str, cursor: usize) -> usize {
    input
        .get(..cursor)
        .map(|before| {
            before
                .rfind(char::is_whitespace)
                .map(|index| {
                    index
                        + before[index..]
                            .chars()
                            .next()
                            .map(char::len_utf8)
                            .unwrap_or(0)
                })
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

/// The line containing `cursor`.
fn line_at(input: &str, cursor: usize) -> &str {
    let start = input[..cursor].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let end = input[cursor..]
        .find('\n')
        .map(|i| cursor + i)
        .unwrap_or(input.len());
    &input[start..end]
}

/// The text before the caret on its own line (pi's `linePrefix`).
fn line_prefix(input: &str, cursor: usize) -> &str {
    let start = input[..cursor].rfind('\n').map(|i| i + 1).unwrap_or(0);
    &input[start..cursor]
}

/// Providers to install, in the order the TUI should consult them: command
/// arguments first (`/office con`), then draft candidates (`#Bud`), then the
/// built-in slash/file providers.
pub(super) fn providers(session: ExtensionSessionCell) -> Vec<Arc<dyn AutocompleteProvider>> {
    vec![
        Arc::new(PluginCommandArguments::new(session.clone())),
        Arc::new(PluginDraftAutocomplete::new(session)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    // Records the last query a fake plugin handler saw. Thread-local because the
    // test harness runs each test on its own thread; a shared static would let
    // two tests overwrite each other's observation.
    thread_local! {
        static LAST_QUERY: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
    }

    fn record(query: &str) {
        LAST_QUERY.with(|slot| *slot.borrow_mut() = Some(query.to_string()));
    }

    fn last_query() -> Option<String> {
        LAST_QUERY.with(|slot| slot.borrow().clone())
    }

    fn write_out(out: *mut StbString, json: &str) {
        unsafe { *out = StbString::from_string(json.to_string()) };
    }

    /// Test-only no-op producer free: the fake handlers' `StbString`s are a few
    /// bytes and the test process exits, so leaking them keeps the helper free
    /// of a second allocator contract.
    unsafe extern "C" fn free_out(_s: StbString) {}

    extern "C" fn documents_provider(
        query: StbStringRef,
        out: *mut StbString,
        _user_data: *mut c_void,
    ) -> i32 {
        let query = unsafe { query.as_str() }.to_string();
        record(&query);
        if !query.contains('#') {
            return 1;
        }
        write_out(
            out,
            r##"{"items":[{"text":"#Budget.xlsx","label":"Budget.xlsx","description":"Excel","insertSpace":false}]}"##,
        );
        0
    }

    extern "C" fn silent_provider(
        _query: StbStringRef,
        _out: *mut StbString,
        _user_data: *mut c_void,
    ) -> i32 {
        1
    }

    extern "C" fn office_arguments(
        args: StbStringRef,
        out: *mut StbString,
        _user_data: *mut c_void,
    ) -> i32 {
        let args = unsafe { args.as_str() }.to_string();
        record(&args);
        write_out(
            out,
            r#"{"items":[{"text":"connect","description":"Connect the bridge"}]}"#,
        );
        0
    }

    fn provider(handler: CompletionFn) -> RegisteredAutocompleteProvider {
        RegisteredAutocompleteProvider {
            handler,
            plugin_free_string: free_out,
            user_data: std::ptr::null_mut(),
        }
    }

    #[test]
    fn draft_provider_wins_only_when_it_has_candidates() {
        let providers = vec![provider(silent_provider), provider(documents_provider)];
        // No `#` in the draft → the first plugin has no opinion, the second one
        // does not either, so the built-ins keep the job.
        assert!(draft_suggestions(&providers, "hello", 5).is_none());

        let suggestions = draft_suggestions(&providers, "hello #Bud", 10).expect("suggestions");
        assert_eq!(suggestions.items.len(), 1);
        assert_eq!(suggestions.items[0].text, "#Budget.xlsx");
        assert!(!suggestions.items[0].insert_space);
        // The completed span is the token under the caret.
        assert_eq!((suggestions.start, suggestions.end), (6, 10));

        let query: serde_json::Value =
            serde_json::from_str(&last_query().expect("query")).expect("json");
        assert_eq!(query["text"], serde_json::json!("hello #Bud"));
        assert_eq!(query["cursor"], serde_json::json!(10));
        assert_eq!(query["line"], serde_json::json!("hello #Bud"));
        assert_eq!(query["linePrefix"], serde_json::json!("hello #Bud"));
    }

    #[test]
    fn draft_provider_falls_through_on_empty_items() {
        extern "C" fn empty_items(
            _query: StbStringRef,
            out: *mut StbString,
            _ud: *mut c_void,
        ) -> i32 {
            write_out(out, r#"{"items":[]}"#);
            0
        }
        let providers = vec![provider(empty_items), provider(documents_provider)];
        let suggestions = draft_suggestions(&providers, "#Bud", 4).expect("second provider wins");
        assert_eq!(suggestions.items[0].text, "#Budget.xlsx");
    }

    #[test]
    fn malformed_plugin_output_is_ignored() {
        extern "C" fn garbage(_query: StbStringRef, out: *mut StbString, _ud: *mut c_void) -> i32 {
            write_out(out, "not json");
            0
        }
        let providers = vec![provider(garbage), provider(silent_provider)];
        assert!(draft_suggestions(&providers, "#Bud", 4).is_none());
    }

    #[test]
    fn item_fields_are_parsed_defensively() {
        let items = parse_items(
            r##"{"items":[{"insertText":"#A.docx"},{"text":""},{"label":"no text"},
                          {"text":"#B.xlsx","isDirectory":true}]}"##,
        )
        .expect("items");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text, "#A.docx");
        assert_eq!(items[1].text, "#B.xlsx");
        assert!(items[1].is_directory);
        assert!(parse_items("{}").is_none());
        assert!(parse_items("  ").is_none());
    }

    #[test]
    fn line_context_helpers_split_on_newlines() {
        let text = "first line\nsecond #Bud\nthird";
        let cursor = text.find("#Bud").unwrap() + 4;
        assert_eq!(line_at(text, cursor), "second #Bud");
        assert_eq!(line_prefix(text, cursor), "second #Bud");
        assert_eq!(token_start(text, cursor), text.find("#Bud").unwrap());
    }

    #[test]
    fn command_arguments_use_the_plugin_handler() {
        let commands = vec![RegisteredCommand {
            name: "office".into(),
            description: "Control the bridge".into(),
            handler: office_arguments,
            user_data: std::ptr::null_mut(),
            completions: Some(rpi_extensions::RegisteredCommandCompletions {
                handler: office_arguments,
                plugin_free_string: free_out,
                user_data: std::ptr::null_mut(),
            }),
        }];
        let suggestions =
            command_argument_suggestions(&commands, "/office con", 11).expect("suggestions");
        assert_eq!(suggestions.items.len(), 1);
        assert_eq!(suggestions.items[0].text, "connect");
        assert_eq!((suggestions.start, suggestions.end), (8, 11));
        let query: serde_json::Value =
            serde_json::from_str(&last_query().expect("query")).expect("json");
        assert_eq!(query["argumentPrefix"], serde_json::json!("con"));

        // Before the space it is the command name being typed, not an argument.
        assert!(command_argument_suggestions(&commands, "/offi", 5).is_none());
        // A command without completions leaves the editor alone.
        let plain = vec![RegisteredCommand {
            name: "office".into(),
            description: String::new(),
            handler: office_arguments,
            user_data: std::ptr::null_mut(),
            completions: None,
        }];
        assert!(command_argument_suggestions(&plain, "/office con", 11).is_none());
        // An unknown command never reaches a plugin.
        assert!(command_argument_suggestions(&commands, "/other con", 10).is_none());
        // A multi-byte separator (ideographic space) must not slice mid-char.
        let wide = "/office\u{3000}con";
        let suggestions =
            command_argument_suggestions(&commands, wide, wide.len()).expect("suggestions");
        assert_eq!((suggestions.start, suggestions.end), (10, wide.len()));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&last_query().expect("query")).unwrap()
                ["argumentPrefix"],
            serde_json::json!("con")
        );
    }
}
