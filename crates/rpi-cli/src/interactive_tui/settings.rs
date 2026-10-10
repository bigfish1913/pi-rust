//! Settings selectors, model catalog filtering, and setting changes.

use super::*;

/// Restrict a catalog to models whose provider matches the one owning
/// `current_id`. The lane is bound to a single provider runtime, so hot-cycle
/// shortcuts (`Ctrl+P` / `Shift+Ctrl+P`) must not jump across providers.
pub(super) fn same_provider_catalog(
    catalog: &[rpi_ai::Model],
    current_id: &str,
) -> Vec<rpi_ai::Model> {
    let Some(provider) = catalog
        .iter()
        .find(|m| m.id.eq_ignore_ascii_case(current_id))
        .map(|m| m.provider.clone())
    else {
        return catalog.to_vec();
    };
    catalog
        .iter()
        .filter(|m| m.provider == provider)
        .cloned()
        .collect()
}

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
        let Some(model) = find_model_selector_match(&catalog_arc, &item.value) else {
            add_note_message(&chat_sel, &format!("Model {} not found.", item.label));
            close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
            return;
        };
        state_sel.set_current_model(&model);
        let provider = model.provider.clone();
        let model_id = model.id.clone();
        let lane = lane_sel.clone();
        tokio::spawn(async move {
            let _ = lane.set_model(model).await;
        });
        // Persist provider + model together so a cross-provider default
        // survives the next launch (bare `defaultModel` alone would fall back).
        let note = set_default_model(&provider, &model_id);
        add_note_message(
            &chat_sel,
            &format!(
                "Default model set to {} ({})",
                short_model_name(&model_id),
                if note.is_ok() { "saved" } else { "not saved" },
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
            let identity = format!("{}/{}", m.provider, m.id);
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
            // Value carries the provider-qualified identity so a bare model id
            // that exists at multiple endpoints can never resolve to the wrong
            // provider (`find_model_selector_match` understands both forms).
            SelectItem::new(&identity, &label)
                .with_description(&format!("{identity}{marker}"))
                .with_search_text(&search_text)
        })
        .collect()
}

/// Resolve a selector input by either bare model id or the qualified
/// `provider/model` identity shown for gateway models. This keeps manual
/// `/model ...` input consistent with the rows rendered by the selector.
/// A bare id that matches more than one provider is ambiguous and resolves to
/// `None` (never silently the first match).
pub(super) fn find_model_selector_match(
    catalog: &[rpi_ai::Model],
    input: &str,
) -> Option<rpi_ai::Model> {
    let (provider, id) = input
        .split_once('/')
        .filter(|(provider, id)| !provider.is_empty() && !id.is_empty())
        .map_or((None, input), |(provider, id)| (Some(provider), id));
    let mut matches = catalog.iter().filter(|model| {
        model.id.eq_ignore_ascii_case(id)
            && provider.map_or(true, |provider| {
                model.provider.eq_ignore_ascii_case(provider)
                    || (provider.eq_ignore_ascii_case("anthropic")
                        && matches!(model.api, rpi_ai::Api::AnthropicMessages))
            })
    });
    match (matches.next(), matches.next()) {
        (Some(model), None) => Some(model.clone()),
        _ => None,
    }
}

/// Build `/provider` selector items: every provider present in the merged
/// catalog (built-in + models.json), labelled with its model count, origin
/// (built-in vs configured), wire protocol, and whether a key is configured.
/// Catalog order (provider id) keeps this deterministic.
pub(super) fn provider_selector_items() -> Vec<SelectItem> {
    let catalog = crate::provider::catalog_all().unwrap_or_default();
    let models_cfg = crate::config::load_models_config().unwrap_or_default();
    let auth = crate::config::read_auth().unwrap_or_default();
    let current = crate::settings::load_settings()
        .ok()
        .and_then(|s| s.default_provider)
        .unwrap_or_else(|| crate::config::DEFAULT_PROVIDER_ID.to_string());

    let mut providers: Vec<(String, usize)> = Vec::new();
    for model in &catalog {
        match providers.iter_mut().find(|(id, _)| *id == model.provider) {
            Some(entry) => entry.1 += 1,
            None => providers.push((model.provider.clone(), 1)),
        }
    }

    providers
        .into_iter()
        .map(|(id, count)| {
            let label = provider_display_name(&id);
            let marker = if id.eq_ignore_ascii_case(&current) {
                " (current)"
            } else {
                ""
            };
            let (origin, protocol) = provider_origin_protocol(&id, &models_cfg);
            let key_status = if provider_has_key(&id, &models_cfg, &auth) {
                "已配置 key"
            } else {
                "未配置 key"
            };
            let description =
                format!("{count} model(s) · {origin} · {protocol} · {key_status}{marker}");
            SelectItem::new(&id, &label)
                .with_description(&description)
                .with_search_text(&format!("{label} {id} {origin} {protocol} {key_status}"))
        })
        .collect()
}

/// The providers that can be removed: everything defined in `models.json`
/// (built-ins are compiled in and cannot be deleted).
pub(super) fn removable_provider_items() -> Vec<SelectItem> {
    let cfg = crate::config::load_models_config().unwrap_or_default();
    cfg.providers
        .iter()
        .map(|(id, provider)| {
            let label = provider_display_name(id);
            SelectItem::new(id, &label)
                .with_description(&format!(
                    "{} model(s) · will be removed from models.json",
                    provider.models.len()
                ))
                .with_search_text(&format!("{label} {id} remove delete"))
        })
        .collect()
}

/// Where a provider comes from and what wire protocol it speaks, for the
/// `/provider` description column.
fn provider_origin_protocol(
    id: &str,
    cfg: &crate::config::ModelsConfig,
) -> (&'static str, &'static str) {
    match id.to_ascii_lowercase().as_str() {
        "anthropic" => ("内置", "Anthropic"),
        "openai" => ("内置", "OpenAI"),
        _ => {
            let protocol = cfg
                .providers
                .get(id)
                .and_then(|p| p.api.as_deref())
                .map(|api| match api {
                    "anthropic-messages" => "Anthropic 兼容",
                    "openai-completions" | "openai-responses" => "OpenAI 兼容",
                    _ => "自定义协议",
                })
                .unwrap_or("自定义协议");
            ("自定义", protocol)
        }
    }
}

/// Whether a provider has a usable credential: a stored `auth.json` entry, a
/// resolved `models.json` apiKey, or (for built-ins) its canonical env var.
fn provider_has_key(
    id: &str,
    cfg: &crate::config::ModelsConfig,
    auth: &crate::config::AuthStore,
) -> bool {
    if auth.contains_key(id) {
        return true;
    }
    if let Some(provider_cfg) = cfg.providers.get(id) {
        if let Some(raw) = provider_cfg.api_key.as_deref().filter(|k| !k.is_empty()) {
            if crate::config::resolve_config_value(raw, None)
                .filter(|v| !v.is_empty())
                .is_some()
            {
                return true;
            }
        }
    }
    match id.to_ascii_lowercase().as_str() {
        "anthropic" => std::env::var("ANTHROPIC_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false),
        "openai" => std::env::var("OPENAI_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false),
        _ => false,
    }
}

/// Persist the default provider (and, when the current saved model does not
/// belong to it, a matching default model) so the next launch resolves to that
/// provider. Returns a note describing what was saved.
pub(super) fn set_default_provider(input: &str) -> Result<String, String> {
    let catalog = crate::provider::catalog_all().map_err(|error| error.to_string())?;
    let provider_id = canonicalize_provider_input(input, &catalog)?;

    let mut settings = crate::settings::load_settings().unwrap_or_default();
    let keeps_existing_model = settings.default_model.as_deref().is_some_and(|existing| {
        catalog.iter().any(|m| {
            m.provider.eq_ignore_ascii_case(&provider_id) && m.id.eq_ignore_ascii_case(existing)
        })
    });
    if !keeps_existing_model {
        settings.default_model = catalog
            .iter()
            .find(|m| m.provider.eq_ignore_ascii_case(&provider_id))
            .map(|m| m.id.clone());
    }
    settings.default_provider = Some(provider_id.clone());
    crate::settings::save_settings(&settings).map_err(|error| error.to_string())?;

    Ok(match &settings.default_model {
        Some(model_id) if !model_id.is_empty() => format!(
            "Default provider set to {provider_id} (model {model_id}) — takes effect after restart. The current session keeps its startup provider."
        ),
        _ => format!(
            "Default provider set to {provider_id} — takes effect after restart. The current session keeps its startup provider."
        ),
    })
}

/// Persist a specific default provider + model pair (used when `/model` picks a
/// model from a different provider than the one currently bound to the lane).
pub(super) fn set_default_model(provider_id: &str, model_id: &str) -> Result<String, String> {
    let mut settings = crate::settings::load_settings().unwrap_or_default();
    settings.default_provider = Some(provider_id.to_string());
    settings.default_model = Some(model_id.to_string());
    crate::settings::save_settings(&settings).map_err(|error| error.to_string())?;
    Ok(format!(
        "Default provider set to {provider_id} (model {model_id}) — takes effect after restart. The current session keeps its startup provider."
    ))
}

// ===========================================================================
// Interactive "Add provider" form (`/provider` → Add new provider)
// ===========================================================================

/// Start the guided provider form. Each step renders its OWN single-line input
/// in the editor slot (isolated from the chat editor); Enter advances, Esc (or
/// an empty submission) cancels, and the final step writes `models.json`.
pub(super) fn begin_provider_form(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
) {
    *state.provider_form.lock().unwrap() = Some(ProviderFormState::default());
    show_provider_form_step(state, editor_container, editor, tui);
}

/// Render the current form step in the editor slot. Choice-style steps (the
/// protocol step) render a select list; every other step renders a titled,
/// bordered single-line input.
fn show_provider_form_step(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
) {
    let Some(form) = state.provider_form.lock().unwrap().clone() else {
        return;
    };
    *state.active_provider_input.lock().unwrap() = None;
    *state.active_provider_select.lock().unwrap() = None;

    if form.step == ProviderFormStep::Api {
        show_provider_api_selector(state, editor_container, editor, tui);
    } else {
        show_provider_text_input(state, editor_container, editor, tui, &form);
    }
}

/// Render a text-style form step as a titled single-line input.
fn show_provider_text_input(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    form: &ProviderFormState,
) {
    let (step_label, question, placeholder) = provider_step_prompt(&form.step);

    let input = Arc::new(Input::with_placeholder(placeholder));
    input.set_focused(true);

    let frame = provider_form_frame(
        step_label,
        question,
        input.clone(),
        "Enter next · Esc cancels (empty Enter also cancels)",
    );
    editor_container.clear();
    editor_container.add_child(frame);
    *state.active_provider_input.lock().unwrap() = Some(input.clone());
    tui.set_focus(Some(input.clone()));

    let state_submit = state.clone();
    let ec_submit = editor_container.clone();
    let editor_submit = editor.clone();
    let tui_submit = tui.clone();
    input.on_submit(Arc::new(move |text: &str| {
        handle_provider_form_input(&state_submit, &ec_submit, &editor_submit, &tui_submit, text);
    }));

    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    input.on_escape(Arc::new(move || {
        cancel_provider_form(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));
    tui.request_render(false);
}

/// Render the protocol step as a select list instead of a free-text input.
fn show_provider_api_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
) {
    let items = vec![
        SelectItem::new("openai-completions", "OpenAI Completions")
            .with_description("OpenAI Chat Completions 兼容接口"),
        SelectItem::new("anthropic-messages", "Anthropic Messages")
            .with_description("Anthropic Messages 兼容接口"),
        SelectItem::new("openai-responses", "OpenAI Responses")
            .with_description("OpenAI Responses 接口"),
    ];
    let list = Arc::new(SelectList::new(items, 3));

    let frame = provider_form_frame(
        "Add provider · step 2/6",
        "Protocol — choose one with ↑/↓ and Enter",
        list.clone(),
        "Enter selects · Esc cancels",
    );
    editor_container.clear();
    editor_container.add_child(frame);
    *state.active_provider_select.lock().unwrap() = Some(list.clone());
    tui.set_focus(Some(list.clone()));

    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    list.on_select(Arc::new(move |item| {
        let Some(mut form) = state_sel.provider_form.lock().unwrap().take() else {
            return;
        };
        form.api = item.value.clone();
        form.step = ProviderFormStep::BaseUrl;
        *state_sel.provider_form.lock().unwrap() = Some(form);
        show_provider_form_step(&state_sel, &ec_sel, &editor_sel, &tui_sel);
    }));

    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        cancel_provider_form(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));
    tui.request_render(false);
}

/// Build the bordered frame shared by every provider-form step.
fn provider_form_frame(
    title: &str,
    question: &str,
    body: Arc<dyn Component>,
    hint: &str,
) -> Arc<Container> {
    let frame = Arc::new(Container::new());
    frame.add_child(Arc::new(DynamicBorder::new()));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(Arc::new(Text::new(title, 0, 0)));
    frame.add_child(Arc::new(Text::new(question, 0, 0)));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(body);
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(Arc::new(Text::new(hint, 0, 0)));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(Arc::new(DynamicBorder::new()));
    frame
}

/// Prompt metadata for one form step: a heading, a question, and the input
/// placeholder.
fn provider_step_prompt(step: &ProviderFormStep) -> (&'static str, &'static str, &'static str) {
    match step {
        ProviderFormStep::Id => (
            "Add provider · step 1/6",
            "Provider id — a short unique name, e.g. openrouter.",
            "openrouter",
        ),
        ProviderFormStep::Api => (
            "Add provider · step 2/6",
            "Protocol — choose one below.",
            "",
        ),
        ProviderFormStep::BaseUrl => (
            "Add provider · step 3/6",
            "Base URL — OpenAI-compatible endpoints usually end in /v1.",
            "https://api.example.com/v1",
        ),
        ProviderFormStep::ApiKey => (
            "Add provider · step 4/6",
            "API key — a literal sk-… or an env reference like $SOME_API_KEY.",
            "$SOME_API_KEY",
        ),
        ProviderFormStep::ModelId => (
            "Add provider · step 5/6",
            "Model id — at least one.",
            "my-model",
        ),
        ProviderFormStep::ModelName => (
            "Add provider · step 6/6",
            "Model name — optional, empty keeps the id.",
            "My Model",
        ),
    }
}

/// Consume one input submission for the active form step, advancing to the
/// next step or writing `models.json` on the last one.
fn handle_provider_form_input(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    text: &str,
) {
    let Some(mut form) = state.provider_form.lock().unwrap().take() else {
        return;
    };
    let text = text.trim();

    // Empty submission cancels the form at any step.
    if text.is_empty() {
        cancel_provider_form(state, editor_container, editor, tui);
        return;
    }

    match form.step {
        ProviderFormStep::Id => {
            if provider_id_exists(text) {
                add_error_message(
                    &state.chat_container,
                    &format!("Provider \"{text}\" already exists. Choose another id."),
                );
                *state.provider_form.lock().unwrap() = Some(form);
                show_provider_form_step(state, editor_container, editor, tui);
                return;
            }
            form.id = text.to_string();
            form.step = ProviderFormStep::Api;
        }
        ProviderFormStep::Api => {
            // The protocol step is rendered as a select list, so a text
            // submission can only arrive here via a bug; re-show the selector
            // rather than panicking.
            *state.provider_form.lock().unwrap() = Some(form);
            show_provider_form_step(state, editor_container, editor, tui);
            return;
        }
        ProviderFormStep::BaseUrl => {
            if !is_valid_provider_url(text) {
                add_error_message(
                    &state.chat_container,
                    &format!("Invalid URL \"{text}\". It must start with http:// or https://."),
                );
                *state.provider_form.lock().unwrap() = Some(form);
                show_provider_form_step(state, editor_container, editor, tui);
                return;
            }
            form.base_url = text.to_string();
            form.step = ProviderFormStep::ApiKey;
        }
        ProviderFormStep::ApiKey => {
            form.api_key = text.to_string();
            form.step = ProviderFormStep::ModelId;
        }
        ProviderFormStep::ModelId => {
            form.model_id = text.to_string();
            form.step = ProviderFormStep::ModelName;
        }
        ProviderFormStep::ModelName => {
            form.model_name = text.to_string();
            match save_provider_from_form(&form) {
                Ok(note) => add_note_message(&state.chat_container, &note),
                Err(error) => add_error_message(&state.chat_container, &error),
            }
            finish_provider_form(state, editor_container, editor, tui);
            return;
        }
    }
    *state.provider_form.lock().unwrap() = Some(form);
    show_provider_form_step(state, editor_container, editor, tui);
}

/// Restore the chat editor and clear the form state (done or cancelled).
fn finish_provider_form(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
) {
    *state.provider_form.lock().unwrap() = None;
    *state.active_provider_input.lock().unwrap() = None;
    *state.active_provider_select.lock().unwrap() = None;
    // The form never shares the chat editor, so clear it on exit: any draft or
    // stray paste must not resurface as if the user typed it.
    editor.clear();
    editor_container.clear();
    editor_container.add_child(editor.clone());
    editor.set_focused(true);
    tui.set_focus(Some(editor.clone()));
    tui.request_render(false);
}

fn cancel_provider_form(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
) {
    add_note_message(&state.chat_container, "Provider add cancelled.");
    finish_provider_form(state, editor_container, editor, tui);
}

/// Whether a provider id is already taken in `models.json`.
pub(super) fn provider_id_exists(id: &str) -> bool {
    crate::config::load_models_config()
        .map(|cfg| cfg.providers.contains_key(id))
        .unwrap_or(false)
}

/// A provider base URL must carry an http(s) scheme; anything else is almost
/// certainly a paste/copy mistake.
pub(super) fn is_valid_provider_url(input: &str) -> bool {
    input.starts_with("http://") || input.starts_with("https://")
}

/// Remove a configured provider from `models.json` (built-ins cannot be
/// removed). Clears a saved default that pointed at the removed provider.
pub(super) fn remove_provider(input: &str) -> Result<String, String> {
    let mut cfg = crate::config::load_models_config().map_err(|error| error.to_string())?;
    let id = input.trim();
    if cfg.providers.shift_remove(id).is_none() {
        return Err(format!(
            "Unknown or built-in provider \"{id}\". Only providers defined in models.json can be removed."
        ));
    }
    crate::config::save_models_config(&cfg).map_err(|error| error.to_string())?;

    // Drop a saved default that referenced the removed provider so the next
    // launch falls back to auth-based selection instead of a stale id.
    let mut settings = crate::settings::load_settings().unwrap_or_default();
    if settings.default_provider.as_deref() == Some(id) {
        settings.default_provider = None;
        settings.default_model = None;
        let _ = crate::settings::save_settings(&settings);
    }
    Ok(format!("Removed provider \"{id}\" — restart rpi to apply."))
}

/// Write the completed form into `models.json` as a new provider with one
/// model.
pub(super) fn save_provider_from_form(form: &ProviderFormState) -> Result<String, String> {
    let mut cfg = crate::config::load_models_config().map_err(|error| error.to_string())?;
    let provider = crate::config::ProviderConfig {
        name: None,
        base_url: Some(form.base_url.clone()),
        api_key: Some(form.api_key.clone()),
        api: Some(form.api.clone()),
        headers: None,
        auth_header: None,
        models: vec![crate::config::ModelDefinition {
            id: form.model_id.clone(),
            name: if form.model_name.is_empty() {
                None
            } else {
                Some(form.model_name.clone())
            },
            base_url: None,
            reasoning: None,
            context_window: None,
            max_tokens: None,
            input: None,
            headers: None,
            compat: None,
        }],
    };
    cfg.providers.insert(form.id.clone(), provider);
    crate::config::save_models_config(&cfg).map_err(|error| error.to_string())?;
    Ok(format!(
        "Added provider \"{}\" ({} · {} model) — restart rpi to use it.",
        form.id, form.api, form.model_id
    ))
}

/// Human-friendly label for a provider id.
fn provider_display_name(id: &str) -> String {
    match id.to_ascii_lowercase().as_str() {
        "anthropic" => "Anthropic".to_string(),
        "openai" => "OpenAI".to_string(),
        "openai-completions" => "OpenAI Completions".to_string(),
        "openai-responses" => "OpenAI Responses".to_string(),
        _ => id.to_string(),
    }
}

/// Resolve a `/provider <id>` input against the catalog's provider ids: exact
/// match wins, then a unique case-insensitive match, mirroring the CLI
/// `--provider` canonicalization without weakening provider identity.
pub(super) fn canonicalize_provider_input(
    input: &str,
    catalog: &[rpi_ai::Model],
) -> Result<String, String> {
    let mut ids: Vec<&str> = Vec::new();
    for model in catalog {
        if !ids.iter().any(|id| *id == model.provider) {
            ids.push(&model.provider);
        }
    }
    if let Some(id) = ids.iter().find(|id| **id == input) {
        return Ok(id.to_string());
    }
    let matches: Vec<&str> = ids
        .iter()
        .copied()
        .filter(|id| id.eq_ignore_ascii_case(input))
        .collect();
    match matches.as_slice() {
        [id] => Ok(id.to_string()),
        [] => Err(format!(
            "Unknown provider \"{input}\". Run /provider to browse available providers."
        )),
        _ => Err(format!(
            "Ambiguous provider \"{input}\": {}.",
            matches.join(", ")
        )),
    }
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
