//! Extension argument suggestions for the TUI autocomplete pipeline.
//!
//! A plugin registers a suggester per slash command (`/msg <instance> …`).
//! While the editor draft starts with that command and the caret sits in the
//! argument region, this provider feeds plugin candidates into the regular
//! completion popup — so `/msg ap` narrows to `apmprod@…` as the user types.
//!
//! This layer is **pure UI**: argument-region detection, prefix narrowing and
//! popup semantics. Every FFI concern — worker scheduling, keepalive
//! lifetime, string reclamation, timeouts, per-command concurrency, caching
//! and refresh cadence — lives in the session-level
//! [`rpi_extensions::SuggestService`], one per [`ExtensionSession`], so this
//! provider holds no threads and no cache of its own (a `/reload` swaps the
//! session and the stale frames become unreachable).
//!
//! ## Responsiveness (never blocks typing)
//!
//! `AutocompleteProvider::get_suggestions` is a synchronous host interface.
//! This provider only reads the service's cached frame (a mutex-guarded map)
//! and submits refreshes via `try_send` — it never waits on the plugin. The
//! popup appears once the first background result is available.

use std::sync::Arc;

use rpi_extensions::SuggestItem;
use rpi_tui::autocomplete::{AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions};

/// Provider backed by the live extension session. Holds no state of its own
/// beyond the session cell: worker, cache and refresh cadence live on the
/// session's [`rpi_extensions::SuggestService`].
pub struct ExtensionArgumentProvider {
    session: crate::session::ExtensionSessionCell,
}

impl ExtensionArgumentProvider {
    pub fn new(session: crate::session::ExtensionSessionCell) -> Self {
        Self { session }
    }
}

/// What one locked session yields for the current command: the registered
/// suggester if any, the session's keepalive (rides with the FFI call) and
/// the session-level suggestion service.
struct Resolved {
    suggester: Option<rpi_extensions::ArgumentSuggester>,
    keepalive: Arc<rpi_extensions::PluginKeepalive>,
    service: Arc<rpi_extensions::SuggestService>,
}

/// Resolve everything the popup needs from the CURRENT session (`/reload`
/// swaps the cell's contents; every call re-resolves, so the provider follows
/// the swap without rebuild). One lock acquisition per keystroke.
fn resolve(session: &crate::session::ExtensionSessionCell, command: &str) -> Option<Resolved> {
    let session = session.lock().ok()?;
    let command = command.trim_start_matches('/').to_ascii_lowercase();
    let snapshot = session.snapshot_arc();
    let suggester = snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.argument_suggester(&command).cloned());
    let keepalive = session.keepalive();
    let service = session.suggest_service();
    Some(Resolved {
        suggester,
        keepalive,
        service,
    })
}

fn snap_cursor(input: &str, cursor: usize) -> usize {
    let cursor = cursor.min(input.len());
    if input.is_char_boundary(cursor) {
        return cursor;
    }
    input
        .char_indices()
        .take_while(|(byte, _)| *byte < cursor)
        .last()
        .map(|(byte, _)| byte)
        .unwrap_or(0)
}

impl AutocompleteProvider for ExtensionArgumentProvider {
    fn get_suggestions(&self, input: &str, cursor: usize) -> Option<AutocompleteSuggestions> {
        // Only the command token at input start. A draft that does not begin
        // with `/` belongs to `@`-path or bare-word providers.
        if !input.starts_with('/') || cursor < 1 {
            return None;
        }
        let cursor = snap_cursor(input, cursor);
        let command_end = input.find(char::is_whitespace).unwrap_or(input.len());
        let token = &input[..command_end];
        if token.len() < 2 {
            return None;
        }
        // Inside the command token (`/me|sg`): the slash-command provider
        // owns the popup. Also guards the `args_text` slice below.
        if cursor < command_end {
            return None;
        }
        let command = token[1..].to_ascii_lowercase();
        let args_text = input[command_end..cursor].trim_start().to_string();

        // One lock acquisition per keystroke: suggester presence decides the
        // `/msg|` boundary case AND the refresh; keepalive + service ride
        // along. `/reload` swaps the cell's contents under the same lock, so
        // every keystroke sees the current session.
        let resolved = resolve(&self.session, &command)?;
        let Some(suggester) = resolved.suggester else {
            return None; // no suggester for this command: never own the popup
        };
        // The caret must be past the command token, OR right after a complete
        // command token with no space yet (`/msg|`): there the user means
        // "arguments next", and Tab completes the FIRST argument (an
        // instance), not the command's own name again.
        let caret_in_args = cursor > command_end;
        let right_after_complete_command =
            cursor == input.len() && !input.contains(char::is_whitespace);
        if !caret_in_args && !right_after_complete_command {
            return None;
        }

        // Refresh the cached FULL list in the background when missing or
        // stale. `refresh_if_needed` never blocks (frame-age check +
        // per-command slot + bounded channel) and carries the session
        // keepalive, so a `/reload` cannot unload the plugin under an
        // in-flight call.
        resolved
            .service
            .refresh_if_needed(&suggester, Arc::clone(&resolved.keepalive));
        // Serve the cached frame, whatever its age — a slightly stale list
        // renders rather than a popup that blinks off mid-word. Never blocks:
        // typing shows nothing rather than waiting on the plugin.
        let all = resolved.service.cached(&command)?;
        Self::narrow(all, &args_text, command_end, cursor)
    }

    fn name(&self) -> &str {
        "extension-arguments"
    }
}

impl ExtensionArgumentProvider {
    /// Narrow the cached full list by the typed prefix — instant, per
    /// keystroke. An entry whose value EXACTLY equals the typed text is
    /// excluded: it is the just-completed selection, and keeping it would
    /// re-open the popup after Tab/Enter-apply, making the next Enter apply
    /// the same entry again instead of submitting the command.
    fn narrow(
        all: Arc<Vec<SuggestItem>>,
        args_text: &str,
        command_end: usize,
        cursor: usize,
    ) -> Option<AutocompleteSuggestions> {
        let to_item = |item: &SuggestItem| AutocompleteItem {
            text: item.value.clone(),
            label: item.label.clone(),
            description: item.description.clone(),
            is_directory: false,
            // One space between the picked target and the message body.
            insert_space: true,
        };
        let query = args_text.to_lowercase();
        // Matching rule (documented in the SDK contract): the candidate's
        // value must START WITH the typed prefix — the popup shows what the
        // remaining keystrokes can still complete, which also keeps Up/Down
        // order stable (value-prefix rows first, then label matches, then
        // substring matches). Contains-matching is deliberate: instance
        // labels carry host names and marks (当前/微信), so typing a host or
        // mark substring would otherwise find nothing.
        let items: Vec<AutocompleteItem> = if query.is_empty() {
            all.iter().map(&to_item).collect()
        } else {
            let mut matched: Vec<(usize, AutocompleteItem)> = Vec::new();
            for item in all.iter() {
                let text = item.value.to_lowercase();
                if text == query {
                    continue; // just-completed selection: see below
                }
                let label = item.label.as_deref().unwrap_or("").to_lowercase();
                let rank = if text.starts_with(&query) {
                    0
                } else if label.contains(&query) {
                    1
                } else if text.contains(&query) {
                    2
                } else {
                    continue;
                };
                matched.push((rank, to_item(item)));
            }
            matched.sort_by_key(|(rank, _)| *rank);
            matched.into_iter().map(|(_, item)| item).collect()
        };
        if items.is_empty() {
            return None;
        }
        Some(AutocompleteSuggestions {
            items,
            // The replacement region is the argument text only (everything
            // after the command token that the caret covers).
            start: command_end,
            end: cursor,
            is_path: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::ExtensionSessionCell;
    use rpi_extensions::{ExtensionRegistry, ExtensionSession};
    use rpi_plugin_sdk::{StbString, StbStringRef};
    use std::ffi::c_void;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// A fast plugin: returns the fixed FULL instance list (full-candidate-
    /// list contract — the host always asks with empty args and narrows
    /// locally).
    extern "C" fn list_handler(
        _request_json: StbStringRef,
        out: *mut StbString,
        _ud: *mut c_void,
    ) -> i32 {
        let payload = serde_json::json!({
            "items": [
                {"value": "apmprod@h", "label": "1. apmprod（微信）", "description": "/export/home"},
                {"value": "apserver@h", "label": "2. apserver", "description": "/data"},
                {"value": "office@h", "label": "3. office（当前）", "description": "/Users/me"},
            ]
        });
        unsafe {
            *out = StbString::from_string(payload.to_string());
        }
        0
    }

    fn suggester_session() -> ExtensionSession {
        let mut registry = ExtensionRegistry::new();
        registry.register_argument_suggester(
            "msg",
            list_handler,
            rpi_plugin_sdk::callbacks::free_string,
            std::ptr::null_mut(),
        );
        ExtensionSession::from_parts(
            Arc::new(registry.snapshot()),
            rpi_extensions::PluginKeepalive::empty(),
            Vec::new(),
            None,
        )
    }

    fn session_with_suggester() -> ExtensionSessionCell {
        Arc::new(Mutex::new(suggester_session()))
    }

    /// Drive the real pipeline until the first background frame lands (at
    /// most ~3 s), so every test exercises provider + service + FFI together.
    fn prime_cache(provider: &ExtensionArgumentProvider, input: &str, cursor: usize) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if provider.get_suggestions(input, cursor).is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("background frame never landed for {input:?}");
    }

    #[test]
    fn popup_narrows_live_from_the_cached_full_list() {
        let provider = ExtensionArgumentProvider::new(session_with_suggester());
        assert!(provider.get_suggestions("/msg ", 5).is_none());
        prime_cache(&provider, "/msg ", 5);

        // Typing `ap` narrows to the two ap* instances, in full-list order.
        let suggestions = provider
            .get_suggestions("/msg ap", 7)
            .expect("narrowed frame");
        assert_eq!(suggestions.start, 4);
        assert_eq!(suggestions.end, 7);
        let values: Vec<&str> = suggestions
            .items
            .iter()
            .map(|item| item.text.as_str())
            .collect();
        assert_eq!(values, ["apmprod@h", "apserver@h"]);
        assert!(suggestions.items[0].insert_space);

        // `off` matches office@h by value prefix (it is NOT the full value —
        // that case is covered by the completed-entry test below).
        let suggestions = provider.get_suggestions("/msg off", 8).expect("frame");
        assert_eq!(suggestions.items.len(), 1);
        assert_eq!(suggestions.items[0].text, "office@h");
        assert_eq!(
            suggestions.items[0].label.as_deref(),
            Some("3. office（当前）")
        );

        // Narrowing further to nothing closes the popup.
        assert!(provider.get_suggestions("/msg zz", 7).is_none());
    }

    #[test]
    fn first_call_never_blocks_and_the_popup_appears_afterwards() {
        let provider = ExtensionArgumentProvider::new(session_with_suggester());
        let start = Instant::now();
        let first = provider.get_suggestions("/msg ", 5);
        let elapsed = start.elapsed();
        // No cache yet: returns immediately (None), never waits on the
        // plugin's FFI round-trip.
        assert!(first.is_none());
        assert!(
            elapsed < Duration::from_millis(50),
            "first call blocked {elapsed:?}"
        );
        // The background frame lands on a later keystroke.
        prime_cache(&provider, "/msg ", 5);
    }

    #[test]
    fn popup_owns_the_token_boundary_but_yields_inside_the_command_word() {
        let provider = ExtensionArgumentProvider::new(session_with_suggester());
        // Inside the command token (`/m|sg`): the slash-command provider owns
        // the popup; this provider declines.
        assert!(provider.get_suggestions("/msg", 2).is_none());
        // Right after the complete token (`/msg|`): arguments next — Tab
        // completes the FIRST ARGUMENT, not the command name again.
        prime_cache(&provider, "/msg", 4);
        let suggestions = provider.get_suggestions("/msg", 4).expect("frame");
        assert_eq!(suggestions.start, 4);
        assert_eq!(suggestions.end, 4);
        assert_eq!(suggestions.items.len(), 3);
    }

    #[test]
    fn completed_entry_is_excluded_so_enter_submits_instead_of_reapplying() {
        let provider = ExtensionArgumentProvider::new(session_with_suggester());
        prime_cache(&provider, "/msg ", 5);
        // The user just applied `apmprod@h`: the draft equals the candidate's
        // value, the popup must stay closed so the next Enter submits.
        assert!(provider.get_suggestions("/msg apmprod@h", 14).is_none());
    }

    #[test]
    fn non_slash_drafts_and_unknown_commands_are_declined() {
        let provider = ExtensionArgumentProvider::new(session_with_suggester());
        assert!(provider.get_suggestions("hello", 5).is_none());
        // A command WITHOUT a registered suggester never owns the popup.
        let provider2 =
            ExtensionArgumentProvider::new(Arc::new(Mutex::new(ExtensionSession::none())));
        assert!(provider2.get_suggestions("/msg", 4).is_none());
    }

    #[test]
    fn reload_hides_the_old_frames_until_the_fresh_session_answers() {
        let cell = session_with_suggester();
        let provider = ExtensionArgumentProvider::new(Arc::clone(&cell));
        prime_cache(&provider, "/msg ", 5);

        // `/reload` swaps the session INSIDE the same cell (mem::replace in
        // session.rs): the provider survives, the old frames must not.
        let _old = std::mem::replace(&mut *cell.lock().unwrap(), suggester_session());
        // Immediately after the swap: no stale frame renders.
        assert!(
            provider.get_suggestions("/msg ", 5).is_none(),
            "pre-reload frame leaked across the session swap"
        );
        // The fresh session's worker repopulates the cache.
        prime_cache(&provider, "/msg ", 5);
    }
}
