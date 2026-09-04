//! Startup session picker used by `-r` / `--resume`.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use rpi_tui::{
    Container, ProcessTerminal, SelectItem, SelectList, Spacer, Text, TuiAltScreen, TUI,
};

/// Select a project session before the main harness is built.
///
/// Returns `Ok(None)` when the user cancels with Esc or Ctrl+C.
pub async fn select(cwd: &Path) -> Result<Option<String>, String> {
    let cwd_text = cwd.to_string_lossy().into_owned();
    let metadata = crate::session::list_session_metadata(&cwd_text)
        .await
        .map_err(|e| e.to_string())?;
    if metadata.is_empty() {
        return Err(format!(
            "no sessions found in {}",
            crate::session::default_session_dir(cwd).display()
        ));
    }

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64;
    let items = metadata
        .iter()
        .map(|meta| {
            let file_name = Path::new(&meta.path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(&meta.id);
            SelectItem::new(&meta.id, file_name)
                .with_description(&format_modified_age(now_ms, meta.modified_at))
        })
        .collect();

    let list = Arc::new(SelectList::new(items, 12));
    let search = Arc::new(Text::new("  Filter: ", 0, 0));
    let root = Arc::new(Container::new());
    root.add_child(Arc::new(Text::new("Resume session", 1, 1)));
    root.add_child(Arc::new(Text::new(
        "  Type to filter, Enter to resume, Esc to cancel",
        0,
        0,
    )));
    root.add_child(Arc::new(Spacer::new(1)));
    root.add_child(search.clone());
    root.add_child(Arc::new(Spacer::new(1)));
    root.add_child(list.clone());

    let tui = Arc::new(TuiAltScreen::new(
        Box::new(ProcessTerminal::new()),
        false,
        None,
    ));
    tui.set_layout_root(Some(root));
    tui.start_readerless();

    let input_tui = tui.clone();
    let input_list = list.clone();
    let input_search = search.clone();
    let task = tokio::task::spawn_blocking(move || -> Result<Option<String>, String> {
        let mut query = String::new();
        loop {
            match crossterm::event::poll(Duration::from_millis(100)) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => return Err(format!("could not read terminal input: {e}")),
            }
            let event = crossterm::event::read()
                .map_err(|e| format!("could not read terminal input: {e}"))?;
            match event {
                Event::Resize(_, _) => {
                    input_tui.refresh_size();
                }
                Event::Mouse(mouse) => {
                    let code = match mouse.kind {
                        MouseEventKind::ScrollUp => Some(KeyCode::Up),
                        MouseEventKind::ScrollDown => Some(KeyCode::Down),
                        _ => None,
                    };
                    if let Some(code) = code {
                        input_list.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
                        input_tui.request_render(false);
                    }
                }
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c')
                    {
                        return Ok(None);
                    }
                    match key.code {
                        KeyCode::Enter => {
                            return Ok(input_list.get_selected_item().map(|item| item.value));
                        }
                        KeyCode::Esc if query.is_empty() => return Ok(None),
                        KeyCode::Esc => {
                            query.clear();
                            input_list.set_filter("");
                        }
                        KeyCode::Backspace => {
                            query.pop();
                            input_list.set_filter(&query);
                        }
                        KeyCode::Char(ch)
                            if !key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            query.push(ch);
                            input_list.set_filter(&query);
                        }
                        _ => input_list.handle_key(key),
                    }
                    input_search.set_text(format!("  Filter: {query}"));
                    input_tui.request_render(false);
                }
                _ => {}
            }
        }
    });

    let result = task
        .await
        .map_err(|e| format!("session picker task failed: {e}"));
    tui.stop(Default::default());
    result?
}

fn format_modified_age(now_ms: i64, modified_ms: i64) -> String {
    let seconds = now_ms.saturating_sub(modified_ms).max(0) / 1000;
    match seconds {
        0..=59 => "just now".to_string(),
        60..=3_599 => format!("{}m ago", seconds / 60),
        3_600..=86_399 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modified_age_is_compact_and_stable() {
        let now = 10 * 86_400_000;
        assert_eq!(format_modified_age(now, now - 20_000), "just now");
        assert_eq!(format_modified_age(now, now - 5 * 60_000), "5m ago");
        assert_eq!(format_modified_age(now, now - 3 * 3_600_000), "3h ago");
        assert_eq!(format_modified_age(now, now - 2 * 86_400_000), "2d ago");
    }
}
