//! Completion suggestions and editor cursor operations.

use super::*;

// ===========================================================================
// Autocomplete
// ===========================================================================

/// Refresh the autocomplete suggestion list from the current editor text +
/// cursor. Renders the suggestions into `autocomplete_container` (above the
/// editor) or clears it when there are none.
pub(super) fn refresh_autocomplete(state: &Arc<TuiState>, editor: &Arc<Editor>) {
    let text = editor.get_text();
    let cursor = editor_cursor_offset(editor, &text);
    let suggestions = state.autocomplete.get_suggestions(&text, cursor);
    render_autocomplete(state, suggestions);
}

/// Convert the editor's logical `(row, byte-column)` caret into the absolute
/// byte offset expected by autocomplete providers.
pub(super) fn editor_cursor_offset(editor: &Editor, text: &str) -> usize {
    let (row, col) = editor.cursor_position();
    let mut offset = 0;
    for (index, line) in text.split('\n').enumerate() {
        if index == row {
            return (offset + col.min(line.len())).min(text.len());
        }
        offset = offset.saturating_add(line.len() + 1);
    }
    text.len()
}

/// Restore an editor caret from an absolute byte offset after autocomplete
/// replaces a span in a multi-line draft.
pub(super) fn set_editor_cursor_offset(editor: &Editor, text: &str, offset: usize) {
    let offset = offset.min(text.len());
    let mut consumed = 0;
    for (row, line) in text.split('\n').enumerate() {
        let end = consumed + line.len();
        if offset <= end {
            editor.set_cursor(row, offset - consumed);
            return;
        }
        consumed = end + 1;
    }
    let last_row = text.bytes().filter(|byte| *byte == b'\n').count();
    editor.set_cursor(
        last_row,
        text.rsplit('\n').next().map(str::len).unwrap_or(0),
    );
}

/// Render (or clear) the autocomplete suggestion list into the container.
pub(super) fn render_autocomplete(
    state: &Arc<TuiState>,
    suggestions: Option<AutocompleteSuggestions>,
) {
    state.autocomplete_container.clear();
    let Some(sugg) = suggestions else {
        return;
    };
    if sugg.items.is_empty() {
        return;
    }
    // Build a compact list: top item marked with `→`, rest with `  `.
    // Cap the list so the dock doesn't swallow the transcript.
    let accent = state.theme_manager.get().colors.accent;
    let muted = state.theme_manager.get().colors.muted;
    for (i, item) in sugg
        .items
        .iter()
        .take(state.autocomplete_max_visible)
        .enumerate()
    {
        let prefix = if i == 0 { "→ " } else { "  " };
        let label = item.display_text();
        let line = if i == 0 {
            format!(
                "{prefix}{} {}",
                accent.fg(label),
                muted.fg(item.description.as_deref().unwrap_or(""))
            )
        } else {
            format!(
                "{prefix}{} {}",
                muted.fg(label),
                muted.fg(item.description.as_deref().unwrap_or(""))
            )
        };
        state
            .autocomplete_container
            .add_child(Arc::new(Text::new(line, 1, 0)));
    }
}

/// Accept the top autocomplete suggestion: replace `text[start..end]` with the
/// suggestion text, reposition the caret, and clear the suggestion list.
/// Returns `true` if a suggestion was accepted.
pub(super) fn accept_top_suggestion(state: &Arc<TuiState>, editor: &Arc<Editor>) -> bool {
    let text = editor.get_text();
    let cursor = editor_cursor_offset(editor, &text);
    let Some(sugg) = state.autocomplete.get_suggestions(&text, cursor) else {
        return false;
    };
    let Some(top) = sugg.items.first() else {
        return false;
    };
    // Replace the [start, end) span with the suggestion text. `start`/`end`
    // are byte offsets emitted by the providers on char boundaries, so the
    // `text[..start]` / `text[end..]` slices are sound for multibyte input.
    let start = sugg.start.min(text.len());
    let end = sugg.end.min(text.len());
    let mut replaced = String::with_capacity(text.len() + top.text.len());
    replaced.push_str(&text[..start]);
    replaced.push_str(&top.text);
    // Keep the text AFTER the replaced span (mid-line completion: replacing
    // `[start, end)` must not drop the rest of the line).
    replaced.push_str(&text[end..]);
    if top.insert_space && !replaced.ends_with('/') {
        replaced.push(' ');
    }
    // New caret position: after the inserted text (byte offset; the editor
    // snaps `set_cursor` to a char boundary as a safety net).
    let new_cursor = replaced.len().min(
        start
            + top.text.len()
            + if top.insert_space && !top.text.ends_with('/') {
                1
            } else {
                0
            },
    );
    editor.set_text(&replaced);
    set_editor_cursor_offset(editor, &replaced, new_cursor);
    state.autocomplete_container.clear();
    true
}
