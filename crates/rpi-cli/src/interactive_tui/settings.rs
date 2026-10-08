//! Settings selectors, model catalog filtering, and setting changes.

use super::*;

pub(super) fn scoped_catalog(catalog: &[rpi_ai::Model], current_id: &str) -> Vec<rpi_ai::Model> {
    let scoped = crate::settings::load_settings()
        .ok()
        .and_then(|s| s.scoped_models)
        .unwrap_or_default();
    if scoped.is_empty() {
        return catalog.to_vec();
    }
    let mut out: Vec<rpi_ai::Model> = catalog
        .iter()
        .filter(|m| scoped.iter().any(|s| s.eq_ignore_ascii_case(&m.id)))
        .cloned()
        .collect();
    // Never strand the user: if the current model isn't in scope, keep it.
    if !out.iter().any(|m| m.id.eq_ignore_ascii_case(current_id)) {
        if let Some(cur) = catalog
            .iter()
            .find(|m| m.id.eq_ignore_ascii_case(current_id))
        {
            out.push(cur.clone());
        }
    }
    out
}

/// Interactive `/settings` menu.
///
/// A [`SettingsList`] of editable settings (upstream's `SettingsSelectorComponent`):
/// Enter/Space cycles a row's value in place, and the theme/model/thinking/scope
/// rows open a sub-selector that replaces the active selector (the
/// `active_selector` slot is single). Value changes apply immediately where the
/// running TUI can honor them and are always persisted to `settings.json`, so
/// rows marked "applies on restart" take effect next launch.
pub(super) fn open_settings_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    lane: &Arc<dyn AgentLane>,
    catalog: &[rpi_ai::Model],
    lane_model_id: &str,
    chat: &Arc<Container>,
) {
    let settings = crate::settings::load_settings().unwrap_or_default();
    let items = settings_menu_items(&settings, state, lane_model_id);
    let list = Arc::new(SettingsList::new(items));

    // ---- submenu rows ----
    // These reuse the existing sub-selectors, which already persist their own
    // choices. The settings menu is replaced by the sub-selector (single
    // `active_selector` slot); cancelling the sub-selector returns to the editor.
    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let lane_sel = lane.clone();
    let catalog_sel = catalog.to_vec();
    let lane_model_sel = lane_model_id.to_string();
    let chat_sel = chat.clone();
    list.on_select(Arc::new(move |item| match item.key.as_str() {
        "theme" => {
            open_settings_theme_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel, &chat_sel)
        }
        "model" => open_settings_model_selector(
            &state_sel,
            &ec_sel,
            &editor_sel,
            &tui_sel,
            &lane_sel,
            &catalog_sel,
            &lane_model_sel,
            &chat_sel,
        ),
        "thinking" => open_settings_thinking_selector(
            &state_sel,
            &ec_sel,
            &editor_sel,
            &tui_sel,
            &lane_sel,
            &catalog_sel,
            &lane_model_sel,
            &chat_sel,
        ),
        "scoped-models" => open_scoped_models_selector(
            &state_sel,
            &ec_sel,
            &editor_sel,
            &tui_sel,
            &catalog_sel,
            &chat_sel,
        ),
        _ => close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel),
    }));

    // ---- value rows ----
    // Apply live where possible, then persist so the choice survives a restart.
    let state_change = state.clone();
    let chat_change = chat.clone();
    let tui_change = tui.clone();
    list.on_change(Arc::new(move |key, value| {
        let mut settings = crate::settings::load_settings().unwrap_or_default();
        let applied = apply_setting_change(&state_change, key, value, &mut settings);
        let saved = crate::settings::save_settings(&settings);
        if let Some(note) = applied {
            let suffix = if saved.is_ok() { "" } else { " (not saved)" };
            add_note_message(&chat_change, &format!("{note}{suffix}"));
        }
        tui_change.request_render(false);
    }));

    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));

    open_selector_with_view(
        state,
        editor_container,
        editor,
        tui,
        SelectorView::Settings(list.clone()),
        list,
        SelectorKind::Settings,
    );
}

/// Build the `/settings` rows from the loaded settings plus the live TUI state.
///
/// Descriptions carry "(applies on restart)" for values resolved once at
/// startup, so the menu never implies an effect the running session cannot
/// apply.
pub(super) fn settings_menu_items(
    settings: &crate::settings::Settings,
    state: &Arc<TuiState>,
    lane_model_id: &str,
) -> Vec<SettingItem> {
    let current_theme = settings.theme.clone().unwrap_or_else(|| "dark".to_string());
    let current_model = settings
        .default_model
        .clone()
        .unwrap_or_else(|| lane_model_id.to_string());
    let current_thinking = settings
        .default_thinking_level
        .clone()
        .unwrap_or_else(|| "(default)".to_string());
    let scoped_desc = match &settings.scoped_models {
        Some(list) if !list.is_empty() => format!("{} model(s)", list.len()),
        _ => "all models".to_string(),
    };
    let hide_thinking = state.hide_thinking();
    let show_images = state.show_images.lock().map(|guard| *guard).unwrap_or(true);
    let show_cache_miss = settings.show_cache_miss_notices.unwrap_or(false);
    let http_idle = settings
        .http_idle_timeout_ms()
        .map(|ms| {
            if ms == 0 {
                "disabled".to_string()
            } else {
                format!("{}s", ms / 1000)
            }
        })
        .unwrap_or_else(|| "5m".to_string());

    vec![
        SettingItem::new("theme", "Theme", &current_theme)
            .with_description("Color theme for the interface")
            .with_submenu(),
        SettingItem::new("model", "Default model", &current_model)
            .with_description("Saved default model for new sessions")
            .with_submenu(),
        SettingItem::new("thinking", "Default thinking", &current_thinking)
            .with_description("Saved default thinking level")
            .with_submenu(),
        SettingItem::new("scoped-models", "Cycle scope", &scoped_desc)
            .with_description("Models enabled for Ctrl+P cycling")
            .with_submenu(),
        SettingItem::new(
            "hide-thinking",
            "Hide thinking",
            bool_setting_value(hide_thinking),
        )
        .with_description("Hide thinking blocks in assistant responses")
        .with_values(&["true", "false"]),
        SettingItem::new(
            "show-images",
            "Show images",
            bool_setting_value(show_images),
        )
        .with_description("Render images inline in the terminal")
        .with_values(&["true", "false"]),
        SettingItem::new(
            "cache-miss-notices",
            "Cache miss notices",
            bool_setting_value(show_cache_miss),
        )
        .with_description("Transcript notices for prompt-cache costs")
        .with_values(&["true", "false"]),
        SettingItem::new(
            "quiet-startup",
            "Quiet startup",
            bool_setting_value(settings.quiet_startup.unwrap_or(false)),
        )
        .with_description("Disable the verbose startup listing (applies on restart)")
        .with_values(&["true", "false"]),
        SettingItem::new(
            "collapse-changelog",
            "Collapse changelog",
            bool_setting_value(settings.collapse_changelog.unwrap_or(false)),
        )
        .with_description("Show a compact upgrade notice (applies on restart)")
        .with_values(&["true", "false"]),
        SettingItem::new(
            "terminal-progress",
            "Terminal progress",
            bool_setting_value(settings.show_terminal_progress().unwrap_or(true)),
        )
        .with_description("OSC 9;4 progress in the terminal tab bar (applies on restart)")
        .with_values(&["true", "false"]),
        SettingItem::new(
            "fullscreen-copy-on-select",
            "Fullscreen copy on select",
            bool_setting_value(settings.fullscreen_copy_on_select.unwrap_or(true)),
        )
        .with_description("Copy selected text automatically in fullscreen mode")
        .with_values(&["true", "false"]),
        SettingItem::new(
            "double-escape-action",
            "Double-escape action",
            settings
                .double_escape_action
                .clone()
                .unwrap_or_else(|| "tree".to_string())
                .as_str(),
        )
        .with_description("Esc Esc with an empty editor (applies on restart)")
        .with_values(&["tree", "fork", "none"]),
        SettingItem::new(
            "editor-padding",
            "Editor padding",
            settings.editor_padding_x.unwrap_or(1).to_string().as_str(),
        )
        .with_description("Horizontal editor padding, 0-3 (applies on restart)")
        .with_values(&["0", "1", "2", "3"]),
        SettingItem::new(
            "autocomplete-max-items",
            "Autocomplete max items",
            settings
                .autocomplete_max_visible
                .unwrap_or(5)
                .to_string()
                .as_str(),
        )
        .with_description("Max autocomplete rows, 3-20 (applies on restart)")
        .with_values(&["3", "5", "7", "10", "15", "20"]),
        SettingItem::new("http-idle-timeout", "HTTP idle timeout", &http_idle)
            .with_description("Idle gap allowed while awaiting HTTP data (applies on restart)")
            .with_values(&["30s", "1m", "5m", "disabled"]),
    ]
}

/// `"true"` / `"false"` for a settings row value.
pub(super) fn bool_setting_value(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

/// Apply one `/settings` value change to the running TUI and to `settings`.
///
/// Returns a note to show in the transcript when the change is user-visible;
/// `None` for changes that only take effect on the next launch.
pub(super) fn apply_setting_change(
    state: &Arc<TuiState>,
    key: &str,
    value: &str,
    settings: &mut crate::settings::Settings,
) -> Option<String> {
    let as_bool = || value == "true";
    match key {
        // Live: hide/show thinking blocks in the transcript immediately.
        "hide-thinking" => {
            state.set_hide_thinking(as_bool());
            settings.hide_thinking_block = Some(as_bool());
            Some(format!(
                "Thinking blocks {}.",
                if as_bool() { "hidden" } else { "shown" }
            ))
        }
        // Live: the image flag is consulted when images would be rendered.
        "show-images" => {
            state.set_show_images(as_bool());
            settings.show_images = Some(as_bool());
            Some(format!(
                "Inline images {}.",
                if as_bool() { "enabled" } else { "disabled" }
            ))
        }
        // Live: read from `state` once per cache-miss notice.
        "cache-miss-notices" => {
            if let Ok(mut guard) = state.cache_miss_notices.lock() {
                *guard = as_bool();
            }
            settings.show_cache_miss_notices = Some(as_bool());
            Some(format!(
                "Cache miss notices {}.",
                if as_bool() { "enabled" } else { "disabled" }
            ))
        }
        // Live: read from settings on each selection mouse-up.
        "fullscreen-copy-on-select" => {
            settings.fullscreen_copy_on_select = Some(as_bool());
            Some(format!(
                "Copy on select {}.",
                if as_bool() { "enabled" } else { "disabled" }
            ))
        }
        // Persisted only: resolved once at startup.
        "quiet-startup" => {
            settings.quiet_startup = Some(as_bool());
            None
        }
        "collapse-changelog" => {
            settings.collapse_changelog = Some(as_bool());
            None
        }
        "terminal-progress" => {
            settings.show_terminal_progress = Some(as_bool());
            None
        }
        "double-escape-action" => {
            settings.double_escape_action = Some(value.to_string());
            None
        }
        "editor-padding" => {
            settings.editor_padding_x = value.parse().ok();
            None
        }
        "autocomplete-max-items" => {
            settings.autocomplete_max_visible = value.parse().ok();
            None
        }
        "http-idle-timeout" => {
            settings.http_idle_timeout = Some(match value {
                "disabled" => serde_json::Value::String("disabled".to_string()),
                "30s" => serde_json::json!(30_000),
                "1m" => serde_json::json!(60_000),
                _ => serde_json::json!(300_000),
            });
            None
        }
        _ => None,
    }
}

/// Apply a theme choice AND persist it to settings.json (`/settings` → Theme).
pub(super) fn open_settings_theme_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    chat: &Arc<Container>,
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
    let chat_sel = chat.clone();
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
        let mut settings = crate::settings::load_settings().unwrap_or_default();
        settings.theme = Some(item.value.clone());
        let saved = crate::settings::save_settings(&settings);
        add_note_message(
            &chat_sel,
            &format!(
                "Theme set to {} (saved{})",
                item.label,
                if saved.is_ok() { "" } else { ", not saved" },
            ),
        );
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
        SelectorKind::Settings,
    );
}

/// Choose the default model AND persist it (`/settings` → Default model):
/// applies live via `lane.set_model` and saves `defaultModel` to settings.json
/// (which `provider::resolve` honors as pi's `findInitialModel` step 3).
pub(super) fn open_settings_model_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    lane: &Arc<dyn AgentLane>,
    catalog: &[rpi_ai::Model],
    lane_model_id: &str,
    chat: &Arc<Container>,
) {
    let items = model_selector_items(catalog, lane_model_id);
    if items.is_empty() {
        add_note_message(chat, "No models in the catalog.");
        tui.request_render(false);
        return;
    }
    let list = Arc::new(SelectList::new(items, 10));

    let catalog_arc = catalog.to_vec();
    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let chat_sel = chat.clone();
    let lane_sel = lane.clone();
    list.on_select(Arc::new(move |item| {
        let Some(model) = catalog_arc.iter().find(|m| m.id == item.value).cloned() else {
            add_note_message(&chat_sel, &format!("Model {} not found.", item.label));
            close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
            return;
        };
        state_sel.set_current_model(&model);
        let lane = lane_sel.clone();
        tokio::spawn(async move {
            let _ = lane.set_model(model).await;
        });
        let mut settings = crate::settings::load_settings().unwrap_or_default();
        settings.default_model = Some(item.value.clone());
        let saved = crate::settings::save_settings(&settings);
        add_note_message(
            &chat_sel,
            &format!(
                "Default model set to {} (saved{}",
                short_model_name(&item.value),
                if saved.is_ok() { ")" } else { ", not saved)" },
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

    open_searchable_selector(
        state,
        editor_container,
        editor,
        tui,
        Some("Default model"),
        Some("Type to filter by name, provider, or id"),
        list,
        SelectorKind::Settings,
    );
}

/// Convert the authenticated runtime catalog into selector rows. Keep the
/// model id as the value so `/model <id>` and the selection callback share one
/// lookup path, while making the provider visible for OpenAI-compatible
/// gateways where the same model id may exist at multiple endpoints.
pub(super) fn model_selector_items(
    catalog: &[rpi_ai::Model],
    lane_model_id: &str,
) -> Vec<SelectItem> {
    let mut seen = std::collections::HashSet::new();
    catalog
        .iter()
        .filter(|m| {
            seen.insert((
                m.api.clone(),
                m.provider.to_ascii_lowercase(),
                m.id.to_ascii_lowercase(),
            ))
        })
        .map(|m| {
            let label = if m.name.is_empty() {
                short_model_name(&m.id)
            } else {
                m.name.clone()
            };
            let identity = if matches!(m.api, rpi_ai::Api::AnthropicMessages)
                && m.provider.eq_ignore_ascii_case("anthropic")
            {
                m.id.clone()
            } else {
                format!("{}/{}", m.provider, m.id)
            };
            let marker = if m.id.eq_ignore_ascii_case(lane_model_id) {
                " (current)"
            } else {
                ""
            };
            // upstream ranks provider-prefixed queries first, so the bare id
            // is not the leading token (`getModelSelectorSearchText`).
            let name = if m.name.is_empty() {
                String::new()
            } else {
                format!(" {}", m.name)
            };
            let search_text = format!(
                "{} {}/{} {} {}{}",
                m.provider, m.provider, m.id, m.provider, m.id, name
            );
            SelectItem::new(&m.id, &label)
                .with_description(&format!("{identity}{marker}"))
                .with_search_text(&search_text)
        })
        .collect()
}

/// Resolve a selector input by either bare model id or the qualified
/// `provider/model` identity shown for gateway models. This keeps manual
/// `/model ...` input consistent with the rows rendered by the selector.
pub(super) fn find_model_selector_match(
    catalog: &[rpi_ai::Model],
    input: &str,
) -> Option<rpi_ai::Model> {
    let (provider, id) = input
        .split_once('/')
        .filter(|(provider, id)| !provider.is_empty() && !id.is_empty())
        .map_or((None, input), |(provider, id)| (Some(provider), id));
    catalog
        .iter()
        .find(|model| {
            model.id.eq_ignore_ascii_case(id)
                && provider.map_or(true, |provider| {
                    model.provider.eq_ignore_ascii_case(provider)
                        || (provider.eq_ignore_ascii_case("anthropic")
                            && matches!(model.api, rpi_ai::Api::AnthropicMessages))
                })
        })
        .cloned()
}

/// Choose the default thinking level AND persist it (`/settings` → Default
/// thinking): applies live via `lane.set_thinking_level` and saves
/// `defaultThinkingLevel` to settings.json.
pub(super) fn open_settings_thinking_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    lane: &Arc<dyn AgentLane>,
    catalog: &[rpi_ai::Model],
    lane_model_id: &str,
    chat: &Arc<Container>,
) {
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
        footer_sel.set_thinking_level(Some(thinking_level_name(level)));
        let mut settings = crate::settings::load_settings().unwrap_or_default();
        settings.default_thinking_level = Some(item.value.clone());
        let saved = crate::settings::save_settings(&settings);
        add_note_message(
            &chat_sel,
            &format!(
                "Default thinking set to {} (saved{}",
                item.label,
                if saved.is_ok() { ")" } else { ", not saved)" },
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
        SelectorKind::Settings,
    );
}

/// `/scoped-models`: a multi-toggle selector over the catalog. Selecting an
/// item toggles it in the in-progress set (the selector stays open); Esc saves
/// the set to settings.json and closes. The active scoped set is echoed after
/// each toggle so the user sees the current selection.
pub(super) fn open_scoped_models_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    catalog: &[rpi_ai::Model],
    chat: &Arc<Container>,
) {
    if catalog.is_empty() {
        add_note_message(chat, "No models in the catalog.");
        tui.request_render(false);
        return;
    }
    // Seed the edit set from the saved scoped models.
    let seed: Vec<String> = crate::settings::load_settings()
        .ok()
        .and_then(|s| s.scoped_models)
        .unwrap_or_default();
    *state.scoped_edit.lock().unwrap() = Some(seed);

    let mut items: Vec<SelectItem> = Vec::new();
    for m in catalog {
        items.push(SelectItem::new(&m.id, &m.id));
    }
    let list = Arc::new(SelectList::new(items, 10));

    let state_sel = state.clone();
    let chat_sel = chat.clone();
    let tui_sel = tui.clone();
    list.on_select(Arc::new(move |item| {
        // Toggle the model in the in-progress set; the selector stays open.
        let mut set = state_sel.scoped_edit.lock().unwrap();
        let set = set.get_or_insert_with(Vec::new);
        if let Some(pos) = set.iter().position(|m| m.eq_ignore_ascii_case(&item.value)) {
            set.remove(pos);
            add_note_message(&chat_sel, &format!("{} removed — Esc to save", item.label));
        } else {
            set.push(item.value.clone());
            add_note_message(&chat_sel, &format!("{} added — Esc to save", item.label));
        }
        tui_sel.request_render(false);
    }));
    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    let chat_cancel = chat.clone();
    list.on_cancel(Arc::new(move || {
        // Save the edited set to settings.json and close.
        let set = state_cancel
            .scoped_edit
            .lock()
            .unwrap()
            .take()
            .unwrap_or_default();
        let mut settings = crate::settings::load_settings().unwrap_or_default();
        settings.scoped_models = if set.is_empty() {
            None
        } else {
            Some(set.clone())
        };
        match crate::settings::save_settings(&settings) {
            Ok(()) => {
                if set.is_empty() {
                    add_note_message(&chat_cancel, "Ctrl+P cycles all models (scope cleared).");
                } else {
                    add_note_message(
                        &chat_cancel,
                        &format!("Ctrl+P cycle scope: {}", set.join(", ")),
                    );
                }
            }
            Err(e) => add_error_message(&chat_cancel, &format!("Could not save settings: {e}")),
        }
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));

    open_selector(
        state,
        editor_container,
        editor,
        tui,
        list,
        SelectorKind::ScopedModels,
    );
}
