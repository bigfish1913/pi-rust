//! Editor-swapping selectors for models, sessions, themes, thinking, tools, and images.

use super::*;

// ===========================================================================
// Selectors — editor-container swap (TS showSelector pattern)
// ===========================================================================

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
    chat: &Arc<Container>,
    initial_search: Option<&str>,
) -> Option<Arc<SearchableSelectList>> {
    let items = model_selector_items(catalog, lane_model_id);
    if items.is_empty() {
        add_note_message(
            chat,
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
    let chat_sel = chat.clone();
    let lane_sel = lane.clone();
    list.on_select(Arc::new(move |item| {
        let Some(model) = catalog_arc.iter().find(|m| m.id == item.value).cloned() else {
            add_note_message(
                &chat_sel,
                &format!("Model {} not found in catalog.", item.label),
            );
            close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
            return;
        };
        state_sel.set_current_model(&model);
        let lane = lane_sel.clone();
        tokio::spawn(async move {
            let _ = lane.set_model(model).await;
        });
        add_note_message(
            &chat_sel,
            &format!(
                "Model set to {} — applies to the next message.",
                short_model_name(&item.value)
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

/// Cycle to the next catalog entry after `current_id`, wrapping to the first.
/// Returns `None` only when the catalog is empty or the current id isn't
/// found (in which case the first entry is returned — a no-op if it IS the
/// current). Used by the Ctrl+P model-cycle hotkey.
pub(super) fn cycle_next_model(
    catalog: &[rpi_ai::Model],
    current_id: &str,
) -> Option<rpi_ai::Model> {
    if catalog.is_empty() {
        return None;
    }
    let idx = catalog
        .iter()
        .position(|m| m.id.eq_ignore_ascii_case(current_id));
    match idx {
        Some(i) => {
            let next = (i + 1) % catalog.len();
            Some(catalog[next].clone())
        }
        None => Some(catalog[0].clone()),
    }
}

/// Cycle to the catalog entry *before* `current_id`, wrapping to the last —
/// the mirror of [`cycle_next_model`] for the previous-model hotkey.
pub(super) fn cycle_prev_model(
    catalog: &[rpi_ai::Model],
    current_id: &str,
) -> Option<rpi_ai::Model> {
    if catalog.is_empty() {
        return None;
    }
    let idx = catalog
        .iter()
        .position(|m| m.id.eq_ignore_ascii_case(current_id));
    match idx {
        Some(i) => {
            let prev = (i + catalog.len() - 1) % catalog.len();
            Some(catalog[prev].clone())
        }
        None => Some(catalog[0].clone()),
    }
}

/// Build + open the `/session` selector. Lists JSONL session files under the
/// default session dir (`<cwd>/.rpi/sessions`
/// fallback), newest-first, labelled with the session name or first prompt so
/// two sessions are distinguishable. Header-only sessions are skipped: there is
/// nothing to switch to.
pub(super) fn open_session_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    cwd: &std::path::Path,
    tx: &mpsc::UnboundedSender<TuiMessage>,
) {
    let dir = crate::session::default_session_dir(cwd);
    let mut items: Vec<SelectItem> = Vec::new();
    for path in crate::session::list_session_files_sync(&dir) {
        let summary = crate::session::summarize_session_file(&path);
        if summary.empty {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("(unnamed)")
            .to_string();
        let label = crate::session::session_display_label(&summary);
        // The file stem is `<timestamp>_<uuid>`; the id tail disambiguates rows
        // that share a timestamp.
        let id_tail = stem.rsplit('_').next().unwrap_or(&stem);
        let short_id = crate::session::short_session_id(id_tail);
        let description = format!(
            "{} · {}",
            crate::session::format_session_bytes(summary.bytes),
            short_id,
        );
        items.push(
            SelectItem::new(&stem, &label)
                .with_description(&description)
                .with_search_text(&format!("{label} {short_id} {stem}")),
        );
    }
    if items.is_empty() {
        add_note_message(
            &state.chat_container,
            "No saved sessions found. Sessions are created automatically in interactive mode.",
        );
        tui.request_render(false);
        return;
    }
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
        Some("Type to filter sessions"),
        list,
        SelectorKind::Session,
    );
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

/// Open a selector for the current session's persisted entry tree. Selecting a
/// message moves the main lane leaf to that entry, then the caller reloads the
/// visible branch from durable storage.
pub(super) async fn open_tree_selector(
    harness: &AgentHarness,
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    chat: &Arc<Container>,
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
            add_error_message(chat, &format!("Could not read session tree: {error}"));
            tui.request_render(false);
            return;
        }
    };
    let current = harness.session().get_leaf_id().await.ok().flatten();
    let items: Vec<SelectItem> = entries
        .iter()
        .map(|entry| {
            let label = if current.as_deref() == Some(entry.id()) {
                format!("{} #{} (current)", entry.entry_type(), entry.seq())
            } else {
                format!("{} #{}", entry.entry_type(), entry.seq())
            };
            let short_id = &entry.id()[..entry.id().len().min(12)];
            // Search on the entry type, sequence, and id so `/tree` stays
            // usable in long sessions.
            let search_text = format!("{} {} {}", entry.entry_type(), entry.seq(), entry.id());
            SelectItem::new(entry.id(), &label)
                .with_description(short_id)
                .with_search_text(&search_text)
        })
        .collect();
    if items.is_empty() {
        add_note_message(chat, "The current session has no entries to navigate.");
        tui.request_render(false);
        return;
    }
    let list = Arc::new(SelectList::new(items, 12));
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

/// Build + open the `/theme` selector. Built-in presets and enabled package
/// themes are shown; selecting applies the theme live and re-renders.
pub(super) fn open_theme_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
) {
    let items = vec![
        SelectItem::new("dark", "Dark").with_description("Default dark theme"),
        SelectItem::new("light", "Light").with_description("Light background"),
        SelectItem::new("monochrome", "Monochrome").with_description("No color accents"),
    ];
    let list = Arc::new(SelectList::new(items, 10));

    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let chat_sel = state.chat_container.clone();
    list.on_select(Arc::new(move |item| {
        let preset = match item.value.as_str() {
            "light" => Some(ThemePreset::Light),
            "monochrome" => Some(ThemePreset::Monochrome),
            "dark" => Some(ThemePreset::Dark),
            name => {
                add_error_message(&chat_sel, &format!("Unknown theme `{name}`."));
                close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
                tui_sel.render_now(true);
                return;
            }
        };
        let Some(preset) = preset else { return };
        apply_theme_preset(preset);
        state_sel.theme_manager.apply_preset(preset);
        // A quick accent note so the user sees the change registered even if
        // the terminal's own colors mask the preset difference.
        add_note_message(&chat_sel, &format!("Theme set to {}.", item.label));
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
    chat: &Arc<Container>,
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
        add_note_message(chat, "This model has no supported thinking levels.");
        tui.request_render(false);
        return;
    }
    let list = Arc::new(SelectList::new(items, 10));

    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let chat_sel = chat.clone();
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
        tokio::spawn(async move {
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
    chat: &Arc<Container>,
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
    let chat_sel = chat.clone();
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
        tokio::spawn(async move {
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
    chat: &Arc<Container>,
) {
    let current = *state.show_images.lock().unwrap();
    let items = vec![
        SelectItem::new("yes", "Yes").with_description(if current {
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
    let chat_sel = chat.clone();
    list.on_select(Arc::new(move |item| {
        let on = item.value == "yes";
        *state_sel.show_images.lock().unwrap() = on;
        add_note_message(
            &chat_sel,
            &format!("Inline images {}.", if on { "enabled" } else { "disabled" }),
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
