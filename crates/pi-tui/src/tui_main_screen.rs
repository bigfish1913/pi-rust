//! TuiMainScreen - Main screen TUI with terminal scrollback.
//!
//! This mode renders content vertically and allows the terminal to handle scrolling.
//! It's suitable for simple output that should be preserved in terminal history.

use std::any::Any;
use std::sync::{Arc, Mutex};

use super::component::Component;
use super::container::Container;
use super::tui::{OverlayHandle, OverlayOptions, TuiMode, TuiStopOptions, TUI};
use crate::terminal::Terminal;

/// Main screen TUI that uses terminal scrollback for scrolling.
pub struct TuiMainScreen {
    terminal: Box<dyn Terminal>,
    container: Container,
    running: Mutex<bool>,
    show_hardware_cursor: Mutex<bool>,
    clear_on_shrink: Mutex<bool>,
    focused: Mutex<Option<Arc<dyn Component>>>,
    full_redraw_count: Mutex<usize>,
}

impl TuiMainScreen {
    /// Create a new main screen TUI.
    pub fn new(
        terminal: Box<dyn Terminal>,
        show_hardware_cursor: bool,
        _log_directory: Option<&str>,
    ) -> Self {
        Self {
            terminal,
            container: Container::new(),
            running: Mutex::new(false),
            show_hardware_cursor: Mutex::new(show_hardware_cursor),
            clear_on_shrink: Mutex::new(false),
            focused: Mutex::new(None),
            full_redraw_count: Mutex::new(0),
        }
    }
}

impl Component for TuiMainScreen {
    fn render(&self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    fn invalidate(&self) {
        self.container.invalidate();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl TUI for TuiMainScreen {
    fn mode(&self) -> TuiMode {
        TuiMode::Regular
    }

    fn terminal(&self) -> &dyn Terminal {
        self.terminal.as_ref()
    }

    fn children(&self) -> Vec<Arc<dyn Component>> {
        // Container doesn't expose children directly, return empty for now
        Vec::new()
    }

    fn add_child(&self, component: Arc<dyn Component>) {
        self.container.add_child(component);
    }

    fn remove_child(&self, _component: &Arc<dyn Component>) {
        // Container doesn't support this yet
    }

    fn clear(&self) {
        self.container.clear();
    }

    fn get_show_hardware_cursor(&self) -> bool {
        self.show_hardware_cursor
            .lock()
            .map(|s| *s)
            .unwrap_or(false)
    }

    fn set_show_hardware_cursor(&self, enabled: bool) {
        if let Ok(mut show) = self.show_hardware_cursor.lock() {
            *show = enabled;
        }
    }

    fn get_clear_on_shrink(&self) -> bool {
        self.clear_on_shrink.lock().map(|s| *s).unwrap_or(false)
    }

    fn set_clear_on_shrink(&self, enabled: bool) {
        if let Ok(mut clear) = self.clear_on_shrink.lock() {
            *clear = enabled;
        }
    }

    fn set_focus(&self, component: Option<Arc<dyn Component>>) {
        if let Ok(mut focused) = self.focused.lock() {
            *focused = component;
        }
    }

    fn get_focus(&self) -> Option<Arc<dyn Component>> {
        self.focused.lock().ok()?.clone()
    }

    fn show_overlay(
        &self,
        _component: Arc<dyn Component>,
        _options: Option<OverlayOptions>,
    ) -> Arc<dyn OverlayHandle> {
        // Main screen doesn't support overlays in the same way
        Arc::new(DummyOverlayHandle)
    }

    fn hide_overlay(&self) {
        // No-op for main screen
    }

    fn has_overlay(&self) -> bool {
        false
    }

    fn start(&self) {
        if let Ok(mut running) = self.running.lock() {
            *running = true;
        }
        self.terminal.show_cursor();
        self.request_render(false);
    }

    fn stop(&self, _options: TuiStopOptions) {
        if let Ok(mut running) = self.running.lock() {
            *running = false;
        }
        self.terminal.show_cursor();
        self.terminal.stop();
    }

    fn render_now(&self, force: bool) {
        if force {
            if let Ok(mut count) = self.full_redraw_count.lock() {
                *count += 1;
            }
        }

        let width = self.terminal.columns();
        let lines = self.render(width);

        // Write each line to terminal
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                self.terminal.write("\r\n");
            }
            self.terminal.write(line);
        }

        self.terminal.flush();
    }

    fn request_render(&self, _force: bool) {
        // Simple immediate render for main screen
        self.render_now(false);
    }

    fn full_redraws(&self) -> usize {
        self.full_redraw_count.lock().map(|c| *c).unwrap_or(0)
    }
}

/// Dummy overlay handle for main screen.
struct DummyOverlayHandle;

impl OverlayHandle for DummyOverlayHandle {
    fn hide(&self) {}
    fn set_hidden(&self, _hidden: bool) {}
    fn is_hidden(&self) -> bool {
        false
    }
    fn focus(&self) {}
    fn is_focused(&self) -> bool {
        false
    }
}
