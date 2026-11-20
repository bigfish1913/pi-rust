//! Editor-swapping selectors for models, sessions, themes, context, thinking, tools, and images.

use super::*;

// ===========================================================================
// Selectors — editor-container swap (TS showSelector pattern)
// ===========================================================================

/// Browse the current resource snapshot without appending notes to the chat.
/// Both confirm and cancel only dismiss the panel; no resources are unloaded.
pub(super) fn open_context_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    resources: &rpi_harness::types::AgentHarnessResources,
) {
    let items = context_panel_lines(resources)
        .into_iter()
        .map(|line| SelectItem::new(&line, &line))
        .collect();
    let list = Arc::new(SelectList::new(items, 10));
    let close = {
        let state = state.clone();
        let container = editor_container.clone();
        let editor = editor.clone();
        let tui = tui.clone();
        Arc::new(move || close_selector(&state, &container, &editor, &tui))
    };
    list.on_cancel(close.clone());
    list.on_select(Arc::new(move |_| close()));
    open_searchable_selector(
        state,
        editor_container,
        editor,
        tui,
        Some("Context — loaded resources"),
        Some("Esc/Enter close · Type to filter · ↑/↓ browse · /usage for token totals"),
        list,
        SelectorKind::Context,
    );
}

/// Swap the `editor_container`'s child (the editor) for a `SelectList`,
/// hiding the editor while the selector is open. Records the selector in
/// `state.active_selector` so the key loop routes to it.
pub(super) fn open_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    list: Arc<SelectList>,
    kind: SelectorKind,
) {
    open_selector_with_view(
        state,
        editor_container,
        editor,
        tui,
        SelectorView::List(list.clone()),
        list,
        kind,
    );
}

/// Open a searchable selector: the list plus a fuzzy-filter search box.
///
/// Returns the wrapper so callers can prefill the query (upstream's
/// `initialSearchInput`). Mirrors `SelectSubmenu` with `searchable: true`.
pub(super) fn open_searchable_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    title: Option<&str>,
    description: Option<&str>,
    list: Arc<SelectList>,
    kind: SelectorKind,
) -> Arc<SearchableSelectList> {
    let searchable = Arc::new(SearchableSelectList::new(title, description, list));
    open_selector_with_view(
        state,
        editor_container,
        editor,
        tui,
        SelectorView::Searchable(searchable.clone()),
        searchable.clone(),
        kind,
    );
    // `TuiAltScreen::set_focus` only records the focused component; components
    // manage their own focus flag. Without this the search caret never renders
    // (and IME candidate positioning stays on the editor's old caret).
    searchable.set_focused(true);
    searchable
}

/// Open a selector with an optional framed view. Native extension selectors
/// wrap the list with a title and hint while built-in selectors keep the list
/// as the complete view.
///
/// `view_router` receives keys (a bare list, or a searchable wrapper) while
/// `view` is what renders — the two are the same object for every built-in
/// selector, but extension selectors frame a plain list.
pub(super) fn open_selector_with_view<V: Component + 'static>(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    view_router: SelectorView,
    view: Arc<V>,
    kind: SelectorKind,
) {
    // Dispatch ui_prompt_start event
    dispatch_ui_prompt_event(state);

    // Unfocus the editor so its cursor marker doesn't render behind the list.
    editor.set_focused(false);
    // A selector replaces the editor slot. Drop stale slash/@file
    // suggestions so they cannot reappear after the selector closes.
    state.autocomplete_container.clear();
    // Swap: clear the container and add the selector view.
    editor_container.clear();
    editor_container.add_child(view.clone());
    *state.active_selector.lock().unwrap() = Some((view_router, kind));
    let focused: Arc<dyn Component> = view;
    tui.set_focus(Some(focused));
    tui.request_render(false);
}

/// Restore the editor into the `editor_container` and clear the active
/// selector. Called by selector `on_cancel` and the Esc handler.
pub(super) fn close_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
) {
    // Dispatch ui_prompt_end event
    dispatch_ui_prompt_event_end(state);

    editor_container.clear();
    editor_container.add_child(editor.clone());
    state.autocomplete_container.clear();
    editor.set_focused(true);
    *state.active_selector.lock().unwrap() = None;
    *state.active_extension_cancel.lock().unwrap() = None;
    tui.set_focus(Some(editor.clone()));
    tui.request_render(false);
}

/// Build + open the `/model` selector. Items are the resolved catalog (display
/// label = model name; description = id), with the current model marked.
/// Selecting applies the model **live** via `lane.set_model` (takes effect on
/// the next user message — the in-flight run's config is already snapshotted),
/// updates the footer, and notes the next-prompt effect.
pub(super) fn open_model_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    catalog: &[rpi_ai::Model],
    lane: &Arc<dyn AgentLane>,
    lane_model_id: &str,
    initial_search: Option<&str>,
) -> Option<Arc<SearchableSelectList>> {
    let items = model_selector_items(catalog, lane_model_id);
    if items.is_empty() {
        add_note_message(
            &state.notice_container(),
            "No models in the catalog. Use --model at startup to select one.",
        );
        tui.request_render(false);
        return None;
    }
    let list = Arc::new(SelectList::new(items, 10));

    // Capture the catalog + lane so the on_select closure can resolve the
    // chosen Model and apply it. `on_select` fires on the blocking key thread,
    // so the async `set_model` runs on a spawned task (matches Ctrl+P).
    let catalog_arc = catalog.to_vec();
    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let chat_sel = state.notice_container();
    let lane_sel = lane.clone();
    list.on_select(Arc::new(move |item| {
        let Some(model) = find_model_selector_match(&catalog_arc, &item.value) else {
            add_note_message(
                &chat_sel,
                &format!("Model {} not found in catalog.", item.label),
            );
            close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
            return;
        };

        let lane = lane_sel.clone();
        let state = state_sel.clone();
        let tui = tui_sel.clone();
        state.session_write_tasks.clone().spawn(async move {
            report_model_selection(&lane, &state, &tui, model, false).await;
        });
        close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
    }));
    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));

    let searchable = open_searchable_selector(
        state,
        editor_container,
        editor,
        tui,
        Some("Select model"),
        Some("Type to filter by name, provider, or id"),
        list,
        SelectorKind::Model,
    );
    if let Some(term) = initial_search {
        searchable.set_search_text(term);
    }
    Some(searchable)
}

/// Resolve a hot-cycle against the live lane, including authenticated models
/// from other providers. Keep the actual current model when it is off-scope.
pub(super) async fn cycle_model(
    lane: &Arc<dyn AgentLane>,
    catalog: &[rpi_ai::Model],
    backwards: bool,
) -> Result<Option<rpi_ai::Model>, String> {
    if catalog.is_empty() {
        return Ok(None);
    }
    let current = lane.get_model().await.map_err(|error| error.to_string())?;
    let scope = scoped_catalog(catalog, &current);
    Ok(if backwards {
        cycle_prev_model(&scope, &current)
    } else {
        cycle_next_model(&scope, &current)
    })
}

/// Commit UI/default changes only after the live lane accepts the model.
///
/// `save_default` is the *only* way this function touches `settings.json`:
/// session-scoped switches (`/model`, the Ctrl+P cycle) pass `false` and stay
/// in memory, so they can never silently overwrite the persisted default.
/// Callers that mean to persist (`/settings` → Default model) pass `true`.
pub(super) async fn apply_model_selection(
    lane: &Arc<dyn AgentLane>,
    state: &Arc<TuiState>,
    model: rpi_ai::Model,
    save_default: bool,
) -> Result<String, String> {
    lane.set_model(model.clone())
        .await
        .map_err(|error| format!("Could not switch model: {error}"))?;
    state.set_current_model(&model);
    dispatch_model_select(state, &model);
    let mut note = format!(
        "Model set to {}/{} — applies to the next message.",
        model.provider, model.id
    );
    if save_default {
        match set_default_model(&model.provider, &model.id) {
            Ok(_) => note.push_str(" Default saved."),
            Err(error) => note.push_str(&format!(" Could not save the default: {error}")),
        }
    } else {
        // Session-scoped switch: point at the explicit way to make it stick
        // instead of writing the default behind the user's back.
        note.push_str(" Not saved as the default — use /settings to change it.");
    }
    Ok(note)
}

/// Notify extensions immediately, including while the agent is idle. The
/// payload mirrors the JS extension API's `model_select` event.
pub(super) fn dispatch_model_select(state: &Arc<TuiState>, model: &rpi_ai::Model) {
    let snapshot = state
        .extension_session
        .lock()
        .ok()
        .and_then(|session| session.snapshot_arc());
    let Some(snapshot) = snapshot else {
        return;
    };
    let payload = serde_json::json!({ "model": model }).to_string();
    rpi_extensions::dispatch_data_event(&snapshot, rpi_plugin_sdk::EventTag::ModelSelect, &payload);
}

pub(super) async fn report_model_selection(
    lane: &Arc<dyn AgentLane>,
    state: &Arc<TuiState>,
    tui: &Arc<TuiAltScreen>,
    model: rpi_ai::Model,
    save_default: bool,
) {
    match apply_model_selection(lane, state, model, save_default).await {
        Ok(note) => add_note_message(&state.notice_container(), &note),
        Err(error) => add_error_message(&state.notice_container(), &error),
    }
    tui.request_render(false);
}

/// Cycle after the current provider/API/model identity, wrapping to the first.
/// An empty catalog returns `None`; a missing anchor returns the first entry.
pub(super) fn cycle_next_model(
    catalog: &[rpi_ai::Model],
    current: &rpi_ai::Model,
) -> Option<rpi_ai::Model> {
    if catalog.is_empty() {
        return None;
    }
    let idx = catalog.iter().position(|m| {
        m.provider == current.provider
            && m.api == current.api
            && m.id.eq_ignore_ascii_case(&current.id)
    });
    match idx {
        Some(i) => {
            let next = (i + 1) % catalog.len();
            Some(catalog[next].clone())
        }
        None => Some(catalog[0].clone()),
    }
}

/// Cycle before the current provider/API/model identity, wrapping to the last —
/// the mirror of [`cycle_next_model`] for the previous-model hotkey.
pub(super) fn cycle_prev_model(
    catalog: &[rpi_ai::Model],
    current: &rpi_ai::Model,
) -> Option<rpi_ai::Model> {
    if catalog.is_empty() {
        return None;
    }
    let idx = catalog.iter().position(|m| {
        m.provider == current.provider
            && m.api == current.api
            && m.id.eq_ignore_ascii_case(&current.id)
    });
    match idx {
        Some(i) => {
            let prev = (i + catalog.len() - 1) % catalog.len();
            Some(catalog[prev].clone())
        }
        None => Some(catalog[0].clone()),
    }
}

/// Build + open the `/session` selector. Lists JSONL session files under the
/// selected session directory, newest-first, labelled with the session name or
/// first prompt so two sessions are distinguishable. Header-only sessions are
/// skipped: there is nothing to switch to.
///
/// The default scope is the current project (fork issue #13). Tab toggles to All,
/// which tags rows with their project directory. The scope resets to Project on
/// every open, so it cannot leak between sessions.
pub(super) fn open_session_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    tx: &mpsc::UnboundedSender<TuiMessage>,
) {
    *state.session_scope.lock().unwrap() = crate::session::SessionScope::Project;
    let scope = *state.session_scope.lock().unwrap();
    let items = session_selector_items(state, scope);
    // Keep the picker installed even with zero rows: it advertises Tab for All,
    // and without an active selector the key loop cannot own Tab. The empty
    // list renders "No matching items"; Tab then rebuilds the rows in place.
    let description = if items.is_empty() {
        "No sessions for this project · Tab: all projects"
    } else {
        "Type to filter · Tab: all projects"
    };
    let description = description.to_string();
    let mut list = SelectList::new(items, 10);
    list.set_layout(rpi_tui::SelectListLayoutOptions {
        min_primary_column_width: Some(56),
        max_primary_column_width: Some(72),
        truncate_primary: None,
    });
    let list = Arc::new(list);

    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let tx_sel = tx.clone();
    list.on_select(Arc::new(move |item| {
        // Close the selector first, then ask the async loop to hot-switch:
        // opening the session file + swapping the harness backing is async
        // (repo list/open) and must not run on the blocking key thread.
        close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
        let _ = tx_sel.send(TuiMessage::SwitchSession(item.value.clone()));
    }));
    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));

    open_searchable_selector(
        state,
        editor_container,
        editor,
        tui,
        Some("Resume session"),
        Some(&description),
        list,
        SelectorKind::Session,
    );
}

/// Key-loop hook: a bare Tab in the open `/session` selector toggles its scope.
/// Split out of the key loop so the routing itself is probe-able (the loop's
/// `selector_open()` branch is not reachable from a unit test).
pub(super) fn selector_owns_key(state: &Arc<TuiState>, key: &KeyEvent) -> bool {
    key.code == KeyCode::Tab && key.modifiers == KeyModifiers::NONE && toggle_session_scope(state)
}

/// Tab in the open `/session` selector swaps the scope and rebuilds the rows in
/// place. The typed filter is re-applied from the search input so switching
/// scope cannot silently widen an active query. Returns false when the active
/// selector is not the session picker, letting the key loop fall through.
pub(super) fn toggle_session_scope(state: &Arc<TuiState>) -> bool {
    let guard = state.active_selector.lock().unwrap();
    let Some((SelectorView::Searchable(searchable), SelectorKind::Session)) = guard.as_ref() else {
        return false;
    };
    let searchable = searchable.clone();
    drop(guard);
    let mut scope = state.session_scope.lock().unwrap();
    let next = scope.toggled();
    *scope = next;
    drop(scope);
    let filter = searchable.input().get_value();
    let list = searchable.list();
    list.set_items(session_selector_items(state, next));
    list.set_filter(&filter);
    true
}

/// Build rows for one scope. The All view prefixes each row with its project so
/// a flat list stays attributable; the project view uses the cwd-encoded
/// directory the repo itself writes into.
fn session_selector_items(
    state: &Arc<TuiState>,
    scope: crate::session::SessionScope,
) -> Vec<SelectItem> {
    let mut items: Vec<SelectItem> = Vec::new();
    for entry in crate::session::list_session_files_in_scope_sync(
        &state.session_cwd,
        &state.session_dir,
        scope,
    ) {
        let summary = crate::session::summarize_session_file(&entry.path);
        if summary.empty {
            continue;
        }
        let stem = entry
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("(unnamed)")
            .to_string();
        let label = crate::session::session_display_label(&summary);
        let id_tail = stem.rsplit('_').next().unwrap_or(&stem);
        let short_id = crate::session::short_session_id(id_tail);
        let mut description = format!(
            "{} · {}",
            crate::session::format_session_bytes(summary.bytes),
            short_id,
        );
        if let Some(project) = &entry.project {
            description = format!("{project} · {description}");
        }
        items.push(
            SelectItem::new(&stem, &label)
                .with_description(&description)
                .with_search_text(&format!("{label} {short_id} {stem}")),
        );
    }
    items
}

pub(super) fn custom_entry_display_text(
    custom_type: &str,
    data: Option<&serde_json::Value>,
) -> Option<String> {
    let data = data?;
    let text = data
        .get("summary")
        .or_else(|| data.get("text"))
        .or_else(|| data.get("output"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())?;
    let label = match custom_type {
        "compactionSummary" => "Compaction summary",
        "branchSummary" => "Branch summary",
        "bashExecution" => "Command output",
        other => other,
    };
    Some(format!("{label}: {text}"))
}

/// Max characters (not bytes) kept in a user-message preview. Char-boundary
/// safe on multi-byte text — `&text[..n]` would panic there.
const TREE_PREVIEW_MAX_CHARS: usize = 60;

/// One-line preview of a user message's text content, or `None` when there is
/// nothing meaningful to show. Only user messages get previews: assistant,
/// toolResult, and custom entries keep the plain `type #seq` row (tool output
/// is long and noisy), per the #387 review. Newlines and whitespace runs are
/// collapsed to single spaces so a multi-line message cannot break the row.
/// A message with no text but with image blocks yields an `[image]` placeholder
/// instead of a blank line.
fn user_message_preview(entry: &Entry) -> Option<String> {
    let AgentMessage::User(user) = entry.as_message()? else {
        return None;
    };
    let collapsed = user_message_text(user)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.chars().count() > TREE_PREVIEW_MAX_CHARS {
        let truncated: String = collapsed.chars().take(TREE_PREVIEW_MAX_CHARS).collect();
        return Some(format!("{truncated}…"));
    }
    if collapsed.is_empty() {
        let has_image = match &user.content {
            rpi_ai::types::UserContent::Blocks(blocks) => blocks
                .iter()
                .any(|block| matches!(block, Content::Image(_))),
            rpi_ai::types::UserContent::Text(_) => false,
        };
        if has_image {
            return Some("[image]".to_string());
        }
        return None;
    }
    Some(collapsed)
}

/// Build the `/tree` row for one entry. Keeps the existing `type #seq` label
/// (`(current)` marker on the leaf), the short id, and the type/seq/id search
/// text; a user-message preview rides along in the description and the search
/// text so the filter box can match "what I said" (issue #387).
fn tree_entry_item(entry: &Entry, is_current: bool) -> SelectItem {
    let label = if is_current {
        format!("{} #{} (current)", entry.entry_type(), entry.seq())
    } else {
        format!("{} #{}", entry.entry_type(), entry.seq())
    };
    let short_id = &entry.id()[..entry.id().len().min(12)];
    let preview = user_message_preview(entry);
    let description = match &preview {
        Some(text) => format!("{short_id} — {text}"),
        None => short_id.to_string(),
    };
    // Search on the entry type, sequence, and id so `/tree` stays usable in
    // long sessions, plus the user-message preview so a remembered phrase is
    // enough to find the right fork point.
    let mut search_text = format!("{} {} {}", entry.entry_type(), entry.seq(), entry.id());
    if let Some(text) = &preview {
        search_text.push(' ');
        search_text.push_str(text);
    }
    SelectItem::new(entry.id(), &label)
        .with_description(&description)
        .with_search_text(&search_text)
}

/// Index of the current leaf in the `/tree` entry list, so the selector opens
/// centered on where the user already is instead of the oldest entry.
fn tree_current_index(entries: &[Entry], current: Option<&str>) -> Option<usize> {
    let current_id = current?;
    entries.iter().position(|entry| entry.id() == current_id)
}

/// Open a selector for the current session's persisted entry tree. Selecting a
/// message moves the main lane leaf to that entry, then the caller reloads the
/// visible branch from durable storage.
pub(super) async fn open_tree_selector(
    harness: &AgentHarness,
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    tx: &mpsc::UnboundedSender<TuiMessage>,
) {
    let entries = match harness
        .session()
        .view("main")
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
    {
        Ok(entries) => entries,
        Err(error) => {
            add_error_message(
                &state.notice_container(),
                &format!("Could not read session tree: {error}"),
            );
            tui.request_render(false);
            return;
        }
    };
    let current = harness.session().get_leaf_id().await.ok().flatten();
    let items: Vec<SelectItem> = entries
        .iter()
        .map(|entry| tree_entry_item(entry, current.as_deref() == Some(entry.id())))
        .collect();
    if items.is_empty() {
        add_note_message(
            &state.notice_container(),
            "The current session has no entries to navigate.",
        );
        tui.request_render(false);
        return;
    }
    let list = Arc::new(SelectList::new(items, 12));
    // Open centered on the current leaf instead of the first (oldest) entry.
    if let Some(index) = tree_current_index(&entries, current.as_deref()) {
        list.set_selected_index(index);
    }
    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let tx_sel = tx.clone();
    list.on_select(Arc::new(move |item| {
        let _ = tx_sel.send(TuiMessage::NavigateTree(item.value.clone()));
        close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
    }));
    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));
    open_searchable_selector(
        state,
        editor_container,
        editor,
        tui,
        Some("Session tree"),
        Some("Type to filter by entry type, sequence, or id"),
        list,
        SelectorKind::Tree,
    );
}

/// Build + open the `/theme` selector. Built-in presets come first, then any
/// discovered custom themes (project + global theme dirs). Selecting applies
/// the theme live and persists it; custom themes are parsed only on select.
pub(super) fn open_theme_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
) {
    let mut items = vec![
        SelectItem::new("dark", "Dark").with_description("Default dark theme"),
        SelectItem::new("light", "Light").with_description("Light background"),
        SelectItem::new("monochrome", "Monochrome").with_description("No color accents"),
    ];
    let registry = state
        .theme_registry
        .read()
        .ok()
        .map(|registry| registry.clone());
    if let Some(registry) = registry {
        for name in registry.names() {
            items.push(SelectItem::new(name, name));
        }
    }
    let list = Arc::new(SelectList::new(items, 10));

    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let chat_sel = state.notice_container();
    list.on_select(Arc::new(move |item| {
        let registry = state_sel.theme_registry.read().ok().map(|r| r.clone());
        match crate::themes::apply_and_persist(
            &item.value,
            &state_sel.theme_manager,
            registry.as_ref(),
        ) {
            Ok(message) => {
                state_sel.set_active_theme_spec(Some(item.value.clone()));
                if let Some(registry_now) = registry.as_ref() {
                    crate::themes::restart_theme_watcher(
                        &state_sel.theme_watcher,
                        Some(&item.value),
                        registry_now,
                        &state_sel.theme_manager,
                        theme_watch_notify(&state_sel, tui_sel.clone()),
                    );
                }
                add_note_message(&chat_sel, &message);
            }
            Err(message) => add_error_message(&chat_sel, &message),
        }
        close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
        tui_sel.render_now(true);
    }));
    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));

    open_selector(
        state,
        editor_container,
        editor,
        tui,
        list,
        SelectorKind::Theme,
    );
}

/// Sentinel value for the "Add new provider" row in the `/provider` picker.
const ADD_PROVIDER_VALUE: &str = "__add_provider__";
/// Sentinel value for the "Remove provider" row in the `/provider` picker.
const REMOVE_PROVIDER_VALUE: &str = "__remove_provider__";

/// Pick a provider for the current session; the async loop applies and saves it.
pub(super) fn open_provider_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    extension_session: &crate::session::ExtensionSessionCell,
    tx: &mpsc::UnboundedSender<TuiMessage>,
    current_provider: Option<&str>,
) {
    let mut items = provider_selector_items(
        &current_provider_manifests(extension_session),
        current_provider,
    );
    if items.is_empty() {
        add_note_message(
            &state.notice_container(),
            "No providers available. Use \"Add new provider\" to configure one in ~/.rpi/agent/models.json.",
        );
        tui.request_render(false);
        return;
    }
    // The trailing actions turn the picker into a small management surface.
    items.push(
        SelectItem::new(ADD_PROVIDER_VALUE, "＋ Add new provider")
            .with_description("Interactive form → writes ~/.rpi/agent/models.json")
            .with_search_text("add new provider"),
    );
    items.push(
        SelectItem::new(REMOVE_PROVIDER_VALUE, "− Remove provider configuration")
            .with_description("Delete a models.json provider")
            .with_search_text("remove delete provider"),
    );
    let list = Arc::new(SelectList::new(items, 10));

    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let tx_sel = tx.clone();
    list.on_select(Arc::new(move |item| {
        if item.value == ADD_PROVIDER_VALUE {
            close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
            begin_provider_form(&state_sel, &ec_sel, &editor_sel, &tui_sel);
            return;
        }
        if item.value == REMOVE_PROVIDER_VALUE {
            close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
            open_provider_remove_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
            return;
        }
        let _ = tx_sel.send(TuiMessage::SwitchProvider(item.value.clone()));
        close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
    }));
    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));

    // Searchable like `/model` so the selector carries a title, a filter box,
    // and the Enter/Esc affordances instead of a bare list.
    open_searchable_selector(
        state,
        editor_container,
        editor,
        tui,
        Some("Select provider"),
        Some("Type to filter providers · Enter selects · Esc cancels"),
        list,
        SelectorKind::Provider,
    );
}

/// Pick a provider for `/login`, then run its OAuth or API-key flow.
pub(super) fn open_login_provider_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    extension_session: &crate::session::ExtensionSessionCell,
    tx: &mpsc::UnboundedSender<TuiMessage>,
) {
    let manifests = current_provider_manifests(extension_session);
    let oauth_providers = current_oauth_providers(extension_session);
    let items = login_provider_items_with_oauth(&manifests, &oauth_providers);
    if items.is_empty() {
        add_note_message(
            &state.notice_container(),
            "No login-capable providers are loaded. Configure a provider or install an extension first.",
        );
        tui.request_render(false);
        return;
    }
    let list = Arc::new(SelectList::new(items, 10));
    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let tx_sel = tx.clone();
    let extension_session_sel = extension_session.clone();
    list.on_select(Arc::new(move |item| {
        close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
        begin_provider_login(
            &state_sel,
            &ec_sel,
            &editor_sel,
            &tui_sel,
            &tx_sel,
            &extension_session_sel,
            &item.value,
        );
    }));
    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));
    open_searchable_selector(
        state,
        editor_container,
        editor,
        tui,
        Some("Log in to provider"),
        Some("Type to filter providers · Enter selects · Esc cancels"),
        list,
        SelectorKind::Provider,
    );
}
pub(super) fn open_logout_provider_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    tx: &mpsc::UnboundedSender<TuiMessage>,
) {
    let items = stored_credential_items();
    if items.is_empty() {
        add_note_message(
            &state.notice_container(),
            "No saved provider credentials found.",
        );
        tui.request_render(false);
        return;
    }
    let list = Arc::new(SelectList::new(items, 10));
    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let tx_sel = tx.clone();
    list.on_select(Arc::new(move |item| {
        let _ = tx_sel.send(TuiMessage::LogoutProvider(item.value.clone()));
        close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
    }));
    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));
    open_searchable_selector(
        state,
        editor_container,
        editor,
        tui,
        Some("Log out provider"),
        Some("Type to filter providers · Enter removes · Esc cancels"),
        list,
        SelectorKind::Provider,
    );
}
/// Open the "remove provider" picker: lists only the providers defined in
/// `models.json` (built-ins cannot be removed), and deletes the chosen one.
pub(super) fn open_provider_remove_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
) {
    let items = removable_provider_items();
    if items.is_empty() {
        add_note_message(
            &state.notice_container(),
            "No removable providers. Only providers defined in ~/.rpi/agent/models.json can be removed.",
        );
        tui.request_render(false);
        return;
    }
    let list = Arc::new(SelectList::new(items, 10));

    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let chat_sel = state.notice_container();
    list.on_select(Arc::new(move |item| {
        match remove_provider(&item.value) {
            Ok(note) => add_note_message(&chat_sel, &note),
            Err(error) => add_error_message(&chat_sel, &error),
        }
        close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
    }));
    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));

    open_searchable_selector(
        state,
        editor_container,
        editor,
        tui,
        Some("Remove provider"),
        Some("Type to filter · Enter deletes · Esc cancels"),
        list,
        SelectorKind::Provider,
    );
}

// ===========================================================================
// Feasible selectors — /thinking, /tools, /images
// ===========================================================================

/// One-line descriptions for each thinking level, ported from
/// thinking-selector.ts (the TS `getThinkingLevelDescription` table).
pub(super) fn thinking_level_description(level: rpi_ai::types::ThinkingLevel) -> &'static str {
    use rpi_ai::types::ThinkingLevel::*;
    match level {
        Off => "Off — No reasoning",
        Minimal => "Minimal — Brief reasoning (~1k tokens)",
        Low => "Low — Light reasoning (~1k tokens)",
        Medium => "Medium — Moderate reasoning (~80% of max)",
        High => "High — Extensive reasoning (~95% of max)",
        Xhigh => "Xhigh — Near-maximal reasoning",
        Max => "Max — Maximum reasoning",
    }
}

/// The lowercase serialized name of a [`ThinkingLevel`] (matches its
/// `#[serde(rename_all = "lowercase")]` form): "off", "minimal", … "max".
pub(super) fn thinking_level_name(level: rpi_ai::types::ThinkingLevel) -> &'static str {
    use rpi_ai::types::ThinkingLevel::*;
    match level {
        Off => "off",
        Minimal => "minimal",
        Low => "low",
        Medium => "medium",
        High => "high",
        Xhigh => "xhigh",
        Max => "max",
    }
}

/// Parse a thinking-level name back to the enum (case-insensitive). Returns
/// `None` for an unknown name; used by the `/thinking` selector callback.
pub(super) fn thinking_level_from_name(name: &str) -> Option<rpi_ai::types::ThinkingLevel> {
    use rpi_ai::types::ThinkingLevel::*;
    match name.to_ascii_lowercase().as_str() {
        "off" => Some(Off),
        "minimal" => Some(Minimal),
        "low" => Some(Low),
        "medium" => Some(Medium),
        "high" => Some(High),
        "xhigh" => Some(Xhigh),
        "max" => Some(Max),
        _ => None,
    }
}

/// Build + open the `/thinking` selector. Items are the levels the current
/// model supports (`Model::supported_thinking_levels`), each with a
/// description; the current level (read beforehand via `lane.get_thinking_level`)
/// is preselected. Selecting applies it live via `lane.set_thinking_level`.
///
/// `on_select` fires on the blocking key thread, so it can't await
/// `lane.get_thinking_level()` to know the current level — the opener resolves
/// it first (best-effort) and preselects; the toggle on_select just applies
/// whatever was picked.
pub(super) fn open_thinking_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    lane: &Arc<dyn AgentLane>,
    catalog: &[rpi_ai::Model],
    lane_model_id: &str,
) {
    // Find the current model in the catalog to read its supported levels. If
    // absent, fall back to all levels so the selector still opens.
    let model = catalog
        .iter()
        .find(|m| m.id.eq_ignore_ascii_case(lane_model_id));
    let levels: Vec<rpi_ai::types::ThinkingLevel> = model
        .map(|m| m.supported_thinking_levels())
        .unwrap_or_else(|| {
            use rpi_ai::types::ThinkingLevel::*;
            vec![Off, Minimal, Low, Medium, High]
        });
    let mut items: Vec<SelectItem> = Vec::new();
    for lvl in &levels {
        let name = thinking_level_name(*lvl);
        items.push(SelectItem::new(name, name).with_description(thinking_level_description(*lvl)));
    }
    if items.is_empty() {
        add_note_message(
            &state.notice_container(),
            "This model has no supported thinking levels.",
        );
        tui.request_render(false);
        return;
    }
    let list = Arc::new(SelectList::new(items, 10));

    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let chat_sel = state.notice_container();
    let lane_sel = lane.clone();
    list.on_select(Arc::new(move |item| {
        let Some(level) = thinking_level_from_name(&item.value) else {
            add_note_message(
                &chat_sel,
                &format!("Unknown thinking level: {}.", item.label),
            );
            close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
            return;
        };
        let lane = lane_sel.clone();
        let footer_sel = state_sel.footer.clone();
        state_sel.session_write_tasks.spawn(async move {
            let _ = lane.set_thinking_level(level).await;
        });
        // Reflect the chosen level in the footer's model suffix (pi parity:
        // `model • thinking off` / `model • medium`). The shown text for the
        // Off level is "off", matching the TS `thinkingLevel === "off"` branch.
        footer_sel.set_thinking_level(Some(thinking_level_name(level)));
        add_note_message(&chat_sel, &format!("Thinking set to {}.", item.label));
        close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
    }));
    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));

    open_selector(
        state,
        editor_container,
        editor,
        tui,
        list,
        SelectorKind::Thinking,
    );
}

/// Build + open the `/tools` selector. Lists the 7 builtin tool names; each
/// visit reads the live active set via `lane.get_active_tools()` (best-effort,
/// resolved synchronously by the opener using `tokio::runtime::Handle` block_on
/// — the blocking key thread can't await) and selecting a tool **toggles** it
/// on/off via `lane.set_active_tools`. Active tools are marked `(on)`.
pub(super) fn open_tools_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    lane: &Arc<dyn AgentLane>,
) {
    // Best-effort read of the current active set. The opener runs on the async
    // runtime (it's called from the main loop's channel dispatch or the submit
    // closure that lives on the blocking thread — but `handle.block_on` is safe
    // because `get_active_tools` is std-Mutex-backed and finishes quickly).
    let mut active = match tokio::runtime::Handle::try_current() {
        Ok(h) => h
            .block_on(async { lane.get_active_tools().await })
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    // An empty active set is the harness sentinel for "all registered tools"
    // (the selector only exposes built-ins). Expand it before rendering and
    // toggling so the first `/tools` visit does not show every tool as off or
    // accidentally reduce the active set to the one item selected.
    if active.is_empty() {
        active = crate::session::BUILTIN_TOOL_NAMES
            .iter()
            .map(|name| (*name).to_string())
            .collect();
    }
    let mut items: Vec<SelectItem> = Vec::new();
    for name in crate::session::BUILTIN_TOOL_NAMES {
        let on = active.iter().any(|a| a == name);
        let label = if on {
            format!("{name} (on)")
        } else {
            (*name).to_string()
        };
        items.push(SelectItem::new(name, &label).with_description("Toggle tool on/off"));
    }
    let list = Arc::new(SelectList::new(items, 10));

    // Capture the active set so on_select can toggle without re-reading.
    let active_captured = active.clone();
    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let chat_sel = state.notice_container();
    let lane_sel = lane.clone();
    list.on_select(Arc::new(move |item| {
        let mut next = active_captured.clone();
        if let Some(pos) = next.iter().position(|a| a == &item.value) {
            next.remove(pos);
        } else {
            next.push(item.value.clone());
        }
        let on = next.iter().any(|a| a == &item.value);
        let lane = lane_sel.clone();
        let next_clone = next.clone();
        state_sel.session_write_tasks.spawn(async move {
            let _ = lane.set_active_tools(next_clone).await;
        });
        let list_str = if next.is_empty() {
            "(none)".to_string()
        } else {
            next.join(", ")
        };
        add_note_message(
            &chat_sel,
            &format!(
                "{} {} — active tools: {}",
                item.value,
                if on { "enabled" } else { "disabled" },
                list_str
            ),
        );
        close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
    }));
    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));

    open_selector(
        state,
        editor_container,
        editor,
        tui,
        list,
        SelectorKind::Tools,
    );
}

/// Build + open the `/images` selector (Yes/No). Stores the choice in
/// `state.show_images` and notes it. Image wiring is minimal this pass — the
/// flag is consulted where images would be shown and echoed back here.
pub(super) fn open_images_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
) {
    let current = *state.show_images.lock().unwrap();
    // Say up front that this terminal cannot paint the images, instead of
    // letting "Inline images enabled" imply they will appear.
    let can_display = rpi_tui::get_capabilities().protocol != rpi_tui::ImageProtocol::None;
    let items = vec![
        SelectItem::new("yes", "Yes").with_description(if !can_display {
            "Inline images (this terminal shows placeholders)"
        } else if current {
            "Inline images (current)"
        } else {
            "Inline images"
        }),
        SelectItem::new("no", "No").with_description(if current {
            "Placeholder only"
        } else {
            "Placeholder only (current)"
        }),
    ];
    let list = Arc::new(SelectList::new(items, 5));

    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let chat_sel = state.notice_container();
    list.on_select(Arc::new(move |item| {
        let on = item.value == "yes";
        state_sel.set_show_images(on);
        add_note_message(
            &chat_sel,
            &images_setting_note(on, rpi_tui::get_capabilities().protocol),
        );
        close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
    }));
    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));

    open_selector(
        state,
        editor_container,
        editor,
        tui,
        list,
        SelectorKind::Images,
    );
}

#[cfg(test)]
mod tree_preview_tests {
    use super::*;
    use rpi_harness::session::types::{EntryBase, MessageEntry};

    fn message_entry(seq: u64, id: &str, message: AgentMessage) -> Entry {
        Entry::Message(MessageEntry {
            base: EntryBase {
                entry_type: "message".to_string(),
                id: id.to_string(),
                seq,
                parent_id: None,
                timestamp: 0,
            },
            message,
            terminate: None,
        })
    }

    fn user_entry(seq: u64, id: &str, content: rpi_ai::types::UserContent) -> Entry {
        message_entry(
            seq,
            id,
            AgentMessage::User(rpi_ai::types::UserMessage::new(content, 0)),
        )
    }

    fn text_blocks(texts: &[&str]) -> Vec<Content> {
        texts
            .iter()
            .map(|text| {
                Content::Text(rpi_ai::types::TextContent {
                    kind: rpi_ai::types::TextContentType,
                    text: (*text).to_string(),
                    text_signature: None,
                })
            })
            .collect()
    }

    fn image_block() -> Content {
        Content::Image(rpi_ai::types::ImageContent {
            kind: rpi_ai::types::ImageContentType,
            data: std::sync::Arc::from("aGk="),
            mime_type: "image/png".to_string(),
        })
    }

    #[test]
    fn user_message_preview_joins_and_collapses_whitespace() {
        let entry = user_entry(
            1,
            "e1",
            rpi_ai::types::UserContent::Blocks(text_blocks(&[
                "帮我看看\n这个 panic",
                "  第二段  \t文本",
            ])),
        );
        let preview = user_message_preview(&entry).expect("text blocks yield a preview");
        assert_eq!(preview, "帮我看看 这个 panic 第二段 文本");

        let item = tree_entry_item(&entry, false);
        assert_eq!(item.label, "message #1");
        assert_eq!(
            item.description.as_deref(),
            Some("e1 — 帮我看看 这个 panic 第二段 文本")
        );
        let search = item.search_text.expect("search text is always set");
        assert!(search.starts_with("message 1 e1 "));
        assert!(
            search.contains("帮我看看 这个 panic"),
            "search text must include the preview: {search}"
        );
    }

    #[test]
    fn user_message_preview_truncates_on_char_boundaries() {
        let long = "好".repeat(TREE_PREVIEW_MAX_CHARS + 20);
        let entry = user_entry(2, "e2", rpi_ai::types::UserContent::Text(long));
        let preview = user_message_preview(&entry).expect("long text yields a preview");
        let chars: Vec<char> = preview.chars().collect();
        assert_eq!(chars.len(), TREE_PREVIEW_MAX_CHARS + 1);
        assert_eq!(chars.last().copied(), Some('…'));
        // All 60 kept characters are intact CJK chars, no split multi-byte panics.
        assert!(chars[..TREE_PREVIEW_MAX_CHARS].iter().all(|&c| c == '好'));
    }

    #[test]
    fn image_only_user_message_gets_placeholder() {
        let entry = user_entry(
            3,
            "e3",
            rpi_ai::types::UserContent::Blocks(vec![image_block()]),
        );
        let item = tree_entry_item(&entry, false);
        assert_eq!(user_message_preview(&entry).as_deref(), Some("[image]"));
        assert_eq!(item.description.as_deref(), Some("e3 — [image]"));
    }

    #[test]
    fn mixed_text_and_image_blocks_preview_text_only() {
        let entry = user_entry(
            4,
            "e4",
            rpi_ai::types::UserContent::Blocks(vec![
                image_block(),
                text_blocks(&["截图里的报错"])[0].clone(),
            ]),
        );
        assert_eq!(
            user_message_preview(&entry).as_deref(),
            Some("截图里的报错"),
            "image blocks next to text must not blank the preview"
        );
    }

    #[test]
    fn whitespace_only_user_message_has_no_preview() {
        let entry = user_entry(
            5,
            "e5",
            rpi_ai::types::UserContent::Text("  \n\t ".to_string()),
        );
        assert_eq!(user_message_preview(&entry), None);
        let item = tree_entry_item(&entry, false);
        assert_eq!(item.description.as_deref(), Some("e5"));
    }

    #[test]
    fn non_user_entries_keep_plain_rows() {
        let assistant = {
            let mut a = rpi_ai::types::AssistantMessage::empty(
                rpi_ai::Api::AnthropicMessages,
                "anthropic",
                "m",
                0,
            );
            a.content = text_blocks(&["very long assistant reply that must not appear"]);
            a
        };
        let tool_result = rpi_agent::message::AgentMessage::ToolResult(Box::new(
            rpi_ai::types::ToolResultMessage {
                role: rpi_ai::types::ToolResultRole,
                tool_call_id: "tc1".to_string(),
                tool_name: "bash".to_string(),
                content: text_blocks(&["tool output noise"]),
                details: None,
                usage: None,
                added_tool_names: Vec::new(),
                is_error: false,
                timestamp: 0,
            },
        ));
        let entries = vec![
            message_entry(
                6,
                "e6",
                rpi_agent::message::AgentMessage::Assistant(Box::new(assistant)),
            ),
            message_entry(7, "e7", tool_result),
            Entry::ModelChange(rpi_harness::session::types::ModelChangeEntry {
                base: EntryBase {
                    entry_type: "model_change".to_string(),
                    id: "e8".to_string(),
                    seq: 8,
                    parent_id: None,
                    timestamp: 0,
                },
                provider: "anthropic".to_string(),
                model_id: "m".to_string(),
            }),
        ];
        for entry in &entries {
            assert_eq!(user_message_preview(entry), None, "{entry:?}");
            let item = tree_entry_item(entry, false);
            assert!(
                !item.description.as_deref().unwrap().contains("—"),
                "{item:?}"
            );
            let search = item.search_text.unwrap();
            assert!(!search.contains("noise"));
            assert!(!search.contains("reply"));
        }
    }

    #[test]
    fn current_entry_keeps_marker_and_short_id() {
        let entry = user_entry(
            9,
            "abcdefghijklmn",
            rpi_ai::types::UserContent::Text("hi".into()),
        );
        let current = tree_entry_item(&entry, true);
        assert_eq!(current.label, "message #9 (current)");
        assert!(current
            .description
            .as_deref()
            .unwrap()
            .starts_with("abcdefghijkl — hi"));

        let not_current = tree_entry_item(&entry, false);
        assert_eq!(not_current.label, "message #9");
    }

    #[test]
    fn tree_current_index_finds_leaf_position() {
        let entries: Vec<Entry> = ["e1", "e2", "e3"]
            .iter()
            .enumerate()
            .map(|(i, id)| {
                user_entry(
                    i as u64 + 1,
                    id,
                    rpi_ai::types::UserContent::Text(format!("msg {id}")),
                )
            })
            .collect();
        assert_eq!(tree_current_index(&entries, Some("e2")), Some(1));
        assert_eq!(tree_current_index(&entries, Some("e3")), Some(2));
    }

    #[test]
    fn tree_current_index_without_current_or_missing_id_is_none() {
        let entries: Vec<Entry> = ["e1", "e2"]
            .iter()
            .enumerate()
            .map(|(i, id)| {
                user_entry(
                    i as u64 + 1,
                    id,
                    rpi_ai::types::UserContent::Text(format!("msg {id}")),
                )
            })
            .collect();
        assert_eq!(tree_current_index(&entries, None), None);
        assert_eq!(tree_current_index(&entries, Some("gone")), None);
        assert_eq!(tree_current_index(&[], Some("e1")), None);
    }
}
