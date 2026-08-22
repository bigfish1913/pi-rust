//! TuiAltScreen - Alternate screen TUI with constrained layout.
//!
//! Uses the alternate screen buffer for fullscreen mode with a scrollable
//! transcript and fixed bottom dock, as described in `tui-plan.md`.

use std::any::Any;
use std::sync::{Arc, Mutex};

use super::component::Component;
use super::container::Container;
use super::editor::Editor;
use super::layout::{extract_cursor_position, render_layout_frame, LayoutFrame};
use super::scroll_view::ScrollView;
use super::tui::{OverlayHandle, OverlayOptions, TuiMode, TuiStopOptions, TUI};
use crate::ansi::CURSOR_MARKER;
use crate::terminal::{InputEvent, Terminal, TerminalInfo};

/// Symbol for ViewportTUI capability check.
pub const VIEWPORT_TUI: &[u8] = b"@earendil-works/pi-tui/viewport";

/// Alternate screen TUI with application-owned scrolling.
pub struct TuiAltScreen {
    terminal: Mutex<Box<dyn Terminal>>,
    container: Container,
    layout_root: Mutex<Option<Arc<dyn Component>>>,
    running: Arc<Mutex<bool>>,
    show_hardware_cursor: Mutex<bool>,
    clear_on_shrink: Mutex<bool>,
    focused: Mutex<Option<Arc<dyn Component>>>,
    full_redraw_count: Mutex<usize>,
    previous_screen: Mutex<Vec<String>>,
    scroll_top: Mutex<usize>,
    stick_to_bottom: Mutex<bool>,
    current_frame: Mutex<Option<LayoutFrame>>,
    // Input handlers
    input_handler: Mutex<Option<Arc<dyn Fn(InputEvent) + Send + Sync>>>,
    resize_handler: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl TuiAltScreen {
    /// Create a new alternate screen TUI.
    pub fn new(terminal: Box<dyn Terminal>, show_hardware_cursor: bool, _log_directory: Option<&str>) -> Self {
        Self {
            terminal: Mutex::new(terminal),
            container: Container::new(),
            layout_root: Mutex::new(None),
            running: Arc::new(Mutex::new(false)),
            show_hardware_cursor: Mutex::new(show_hardware_cursor),
            clear_on_shrink: Mutex::new(false),
            focused: Mutex::new(None),
            full_redraw_count: Mutex::new(0),
            previous_screen: Mutex::new(Vec::new()),
            scroll_top: Mutex::new(0),
            stick_to_bottom: Mutex::new(true),
            current_frame: Mutex::new(None),
            input_handler: Mutex::new(None),
            resize_handler: Mutex::new(None),
        }
    }

    /// Set the layout root component.
    pub fn set_layout_root(&self, component: Option<Arc<dyn Component>>) {
        if let Ok(mut root) = self.layout_root.lock() {
            *root = component;
        }
        self.request_render(false);
    }

    /// Get the layout root.
    pub fn get_layout_root(&self) -> Option<Arc<dyn Component>> {
        self.layout_root.lock().ok()?.clone()
    }

    /// Get the current scroll position.
    pub fn viewport_top(&self) -> usize {
        self.scroll_top.lock().map(|s| *s).unwrap_or(0)
    }

    /// Check if following output.
    pub fn is_following_output(&self) -> bool {
        self.stick_to_bottom.lock().map(|s| *s).unwrap_or(true)
    }

    /// Scroll by the given number of lines.
    pub fn scroll_by(&self, lines: i32) {
        if let Ok(mut scroll_top) = self.scroll_top.lock() {
            if lines > 0 {
                *scroll_top = scroll_top.saturating_add(lines as usize);
            } else {
                *scroll_top = scroll_top.saturating_sub((-lines) as usize);
            }
        }
        if lines != 0 {
            if let Ok(mut stick) = self.stick_to_bottom.lock() {
                *stick = lines > 0; // Re-enable stick on scroll down
            }
        }
        self.request_render(false);
    }

    /// Scroll to the top.
    pub fn scroll_to_top(&self) {
        if let Ok(mut scroll_top) = self.scroll_top.lock() {
            *scroll_top = 0;
        }
        if let Ok(mut stick) = self.stick_to_bottom.lock() {
            *stick = false;
        }
        self.request_render(false);
    }

    /// Scroll to the bottom.
    pub fn scroll_to_bottom(&self) {
        if let Ok(mut stick) = self.stick_to_bottom.lock() {
            *stick = true;
        }
        self.request_render(false);
    }

    /// Get the primary scroll view if any.
    pub fn get_primary_scroll_view(&self) -> Option<Arc<ScrollView>> {
        self.current_frame.lock().ok()?.as_ref()?.primary_scroll_view.clone()
    }

    /// Check if this implements ViewportTUI.
    pub fn is_viewport_tui(&self) -> bool {
        true
    }

    /// Enter alternate screen mode.
    fn enter_alt_screen(&self) {
        if let Ok(terminal) = self.terminal.lock() {
            // Enter alternate screen buffer and disable autowrap
            terminal.write("\x1b[?1049h\x1b[?7l");
            // Clear screen and hide cursor
            terminal.write("\x1b[2J\x1b[H\x1b[?25l");
            terminal.flush();
        }
    }

    /// Exit alternate screen mode.
    fn exit_alt_screen(&self, preserve_screen: bool) {
        if let Ok(terminal) = self.terminal.lock() {
            if preserve_screen {
                terminal.write("\x1b[?1049l\x1b[?25h");
            } else {
                // Render final document to main buffer
                let width = terminal.columns();
                let lines = self.render(width);
                
                // Exit alt screen first
                terminal.write("\x1b[?1049l");
                
                // Print final document
                for (i, line) in lines.iter().enumerate() {
                    if i > 0 {
                        terminal.write("\r\n");
                    }
                    let clean_line = line.replace(CURSOR_MARKER, "");
                    terminal.write(&clean_line);
                }
                terminal.write("\r\n\x1b[?25h");
            }
            terminal.flush();
        }
    }

    /// Perform a differential render with constrained layout.
    fn do_render(&self) {
        let (width, height) = if let Ok(terminal) = self.terminal.lock() {
            (terminal.columns(), terminal.rows())
        } else {
            return;
        };

        // Get layout root or use container
        let root = self.get_layout_root();
        let root_component: Arc<dyn Component> = root.unwrap_or_else(|| Arc::new(self.container.clone()));

        // Render layout frame
        let frame = render_layout_frame(root_component.clone(), width, height);

        // Store the frame for input handling
        if let Ok(mut current) = self.current_frame.lock() {
            *current = Some(frame.clone());
        }

        // Get screen lines
        let visible = frame.lines;

        // Check for full redraw
        let previous = self.previous_screen.lock().map(|p| p.clone()).unwrap_or_default();
        let full_redraw = previous.len() != height || previous.is_empty();

        // Build output buffer
        let mut buffer = String::new();

        if let Ok(terminal) = self.terminal.lock() {
            if full_redraw {
                if let Ok(mut count) = self.full_redraw_count.lock() {
                    *count += 1;
                }
                buffer.push_str("\x1b[2J"); // Clear screen
            }

            // Begin synchronized output
            buffer.push_str("\x1b[?2026h");

            // Write changed lines
            for (row, line) in visible.iter().enumerate() {
                if !full_redraw && row < previous.len() && &previous[row] == line {
                    continue; // Skip unchanged lines
                }
                buffer.push_str(&format!("\x1b[{};1H\x1b[2K{}", row + 1, line));
            }

            // Find cursor position if present
            if let Some((row, col)) = extract_cursor_position(&visible, height) {
                buffer.push_str(&format!("\x1b[{};{}H", row + 1, col + 1));
                if self.get_show_hardware_cursor() {
                    buffer.push_str("\x1b[?25h");
                }
            } else {
                // Hide cursor if no cursor marker found
                buffer.push_str("\x1b[?25l");
            }

            // End synchronized output
            buffer.push_str("\x1b[?2026l");

            terminal.write(&buffer);
            terminal.flush();
        }

        // Store current screen
        if let Ok(mut prev) = self.previous_screen.lock() {
            *prev = visible;
        }
    }
}

impl Component for TuiAltScreen {
    fn render(&self, width: usize) -> Vec<String> {
        self.get_layout_root()
            .map(|r| r.render(width))
            .unwrap_or_else(|| self.container.render(width))
    }

    fn invalidate(&self) {
        if let Some(root) = self.get_layout_root() {
            root.invalidate();
        } else {
            self.container.invalidate();
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl TUI for TuiAltScreen {
    fn mode(&self) -> TuiMode {
        TuiMode::Fullscreen
    }

    fn terminal(&self) -> &dyn Terminal {
        // This trait method is problematic because we store Terminal in Mutex<Box<dyn Terminal>>
        // We cannot return a reference through a Mutex guard. 
        // For now, we provide a stub that should not be called.
        // Alternative: change the trait to not require this method, or use different storage.
        // This is a known design issue - users should use the terminal through other methods.
        static DUMMY: DummyTerminal = DummyTerminal;
        &DUMMY
    }

    fn children(&self) -> Vec<Arc<dyn Component>> {
        Vec::new()
    }

    fn add_child(&self, component: Arc<dyn Component>) {
        self.container.add_child(component);
    }

    fn remove_child(&self, _component: &Arc<dyn Component>) {}

    fn clear(&self) {
        self.container.clear();
    }

    fn get_show_hardware_cursor(&self) -> bool {
        self.show_hardware_cursor.lock().map(|s| *s).unwrap_or(false)
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

    fn show_overlay(&self, _component: Arc<dyn Component>, _options: Option<OverlayOptions>) -> Arc<dyn OverlayHandle> {
        Arc::new(DummyOverlayHandle)
    }

    fn hide_overlay(&self) {}

    fn has_overlay(&self) -> bool {
        false
    }

    fn start(&self) {
        if let Ok(mut running) = self.running.lock() {
            *running = true;
        }
        
        // Clone the necessary Arc references for the closures
        let running = self.running.clone();
        
        // Get terminal and start it
        if let Ok(terminal) = self.terminal.lock() {
            terminal.start(
                Box::new(move |event: InputEvent| {
                    // Check if still running
                    if !*running.lock().unwrap() {
                        return;
                    }
                    
                    // Handle input events
                    match event {
                        InputEvent::Key(_key) => {
                            // Key handling would be done by focused component
                            // For now, this is a placeholder
                        }
                        InputEvent::Mouse(_mouse) => {
                            // Mouse handling - could be implemented later
                        }
                        InputEvent::Resize(_cols, _rows) => {
                            // Resize triggers re-render
                            // For now, this is a placeholder
                        }
                        _ => {}
                    }
                }),
                Box::new(move || {
                    // Handle resize
                    // For now, this is a placeholder
                }),
            );
        }
        
        self.enter_alt_screen();
        
        // Trigger initial render after entering alt screen
        self.do_render();
    }

    fn stop(&self, options: TuiStopOptions) {
        if let Ok(mut running) = self.running.lock() {
            *running = false;
        }
        
        // Stop terminal first
        if let Ok(terminal) = self.terminal.lock() {
            terminal.stop();
        }
        
        // Exit alt screen
        self.exit_alt_screen(options.preserve_screen);
    }

    fn render_now(&self, force: bool) {
        if force {
            if let Ok(mut prev) = self.previous_screen.lock() {
                prev.clear();
            }
        }
        self.do_render();
    }

    fn request_render(&self, _force: bool) {
        self.render_now(false);
    }

    fn full_redraws(&self) -> usize {
        self.full_redraw_count.lock().map(|c| *c).unwrap_or(0)
    }
}

/// Check if a TUI implements ViewportTUI.
pub fn is_viewport_tui(_tui: &dyn TUI) -> bool {
    // This would need proper implementation using Any
    false
}

/// Dummy overlay handle for alternate screen.
struct DummyOverlayHandle;

impl OverlayHandle for DummyOverlayHandle {
    fn hide(&self) {}
    fn set_hidden(&self, _hidden: bool) {}
    fn is_hidden(&self) -> bool { false }
    fn focus(&self) {}
    fn is_focused(&self) -> bool { false }
}

/// Dummy terminal for the terminal() stub.
struct DummyTerminal;

impl Terminal for DummyTerminal {
    fn info(&self) -> TerminalInfo { TerminalInfo::default() }
    fn write(&self, _data: &str) {}
    fn hide_cursor(&self) {}
    fn show_cursor(&self) {}
    fn move_cursor(&self, _row: usize, _col: usize) {}
    fn clear_screen(&self) {}
    fn set_title(&self, _title: &str) {}
    fn enable_mouse(&self) {}
    fn disable_mouse(&self) {}
    fn start(&self, _on_input: Box<dyn Fn(InputEvent) + Send + Sync>, _on_resize: Box<dyn Fn() + Send + Sync>) {}
    fn stop(&self) {}
    fn is_tty(&self) -> bool { false }
    fn set_progress(&self, _active: bool) {}
    fn flush(&self) {}
}