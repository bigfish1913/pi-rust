//! TuiAltScreen - Alternate screen TUI with constrained layout.
//!
//! Uses the alternate screen buffer for fullscreen mode with a scrollable
//! transcript and fixed bottom dock, as described in `tui-plan.md`.

use std::any::Any;
use std::sync::{Arc, Mutex};

use super::component::Component;
use super::container::Container;
use super::layout::{
    composite_tui_line, extract_cursor_position, render_layout_frame,
    render_layout_frame_reusing_scroll_content, LayoutFrame,
};
use super::overlay::OverlayManager;
use super::scroll_view::ScrollView;
use super::tui::{
    OverlayAnchor, OverlayHandle, OverlayOptions, SizeValue, TuiMode, TuiStopOptions, TUI,
};
use crate::ansi::{visible_width, CURSOR_MARKER};
use crate::terminal::{InputEvent, Terminal, TerminalInfo};

/// Symbol for ViewportTUI capability check.
pub const VIEWPORT_TUI: &[u8] = b"@earendil-works/pi-tui/viewport";

/// Alternate screen TUI with application-owned scrolling.
pub struct TuiAltScreen {
    /// Serializes the complete diff/render/write transaction. Input, streaming
    /// events, and loader ticks can request frames from different threads; if
    /// they race, an older frame can otherwise overwrite `previous_screen`
    /// after a newer frame has already reached the terminal.
    render_lock: Mutex<()>,
    terminal: Arc<Mutex<Box<dyn Terminal>>>,
    terminal_proxy: TerminalProxy,
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
    /// When true, render into the terminal's main buffer and let the terminal
    /// own scrollback (native pi's default `regular` mode).
    main_screen_mode: Mutex<bool>,
    main_previous_width: Mutex<usize>,
    main_previous_height: Mutex<usize>,
    main_hardware_row: Mutex<usize>,
    main_viewport_top: Mutex<usize>,
    // Input handlers
    #[allow(dead_code)]
    input_handler: Mutex<Option<Arc<dyn Fn(InputEvent) + Send + Sync>>>,
    #[allow(dead_code)]
    resize_handler: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    overlays: Arc<OverlayManager>,
    render_suspended: Mutex<bool>,
}

impl TuiAltScreen {
    /// Create a new alternate screen TUI.
    pub fn new(
        terminal: Box<dyn Terminal>,
        show_hardware_cursor: bool,
        _log_directory: Option<&str>,
    ) -> Self {
        let terminal = Arc::new(Mutex::new(terminal));
        let terminal_proxy = TerminalProxy {
            terminal: terminal.clone(),
        };
        Self {
            render_lock: Mutex::new(()),
            terminal,
            terminal_proxy,
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
            main_screen_mode: Mutex::new(false),
            main_previous_width: Mutex::new(0),
            main_previous_height: Mutex::new(0),
            main_hardware_row: Mutex::new(0),
            main_viewport_top: Mutex::new(0),
            input_handler: Mutex::new(None),
            resize_handler: Mutex::new(None),
            overlays: Arc::new(OverlayManager::new()),
            render_suspended: Mutex::new(false),
        }
    }

    /// Use the terminal's main screen and native scrollback instead of the
    /// constrained alternate-screen viewport. Must be set before start.
    pub fn set_main_screen_mode(&self, enabled: bool) {
        if !self.is_running() {
            *self.main_screen_mode.lock().unwrap() = enabled;
        }
    }

    fn uses_main_screen(&self) -> bool {
        self.main_screen_mode
            .lock()
            .map(|mode| *mode)
            .unwrap_or(false)
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

    fn is_running(&self) -> bool {
        self.running.lock().map(|running| *running).unwrap_or(false)
    }

    /// Temporarily suspend the outer application renderer while a foreign
    /// runtime owns the terminal (for example a Node extension's fullscreen
    /// component). The terminal remains in raw/alternate-screen mode; only
    /// background repaint requests are suppressed.
    pub fn set_render_suspended(&self, suspended: bool) {
        if let Ok(mut value) = self.render_suspended.lock() {
            *value = suspended;
        }
        if !suspended {
            self.request_render(true);
        }
    }

    pub fn is_render_suspended(&self) -> bool {
        self.render_suspended
            .lock()
            .map(|value| *value)
            .unwrap_or(false)
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
        self.current_frame
            .lock()
            .ok()?
            .as_ref()?
            .primary_scroll_view
            .clone()
    }

    /// Get the current terminal column count (cached, refreshed on resize).
    /// Used by callers that need a width for off-layout rendering (e.g.
    /// diff-line width sizing) without re-reading the terminal themselves.
    pub fn width(&self) -> usize {
        self.terminal.lock().map(|t| t.columns()).unwrap_or(80)
    }

    /// Set the terminal window/tab title.
    ///
    /// The [`Terminal`] trait's `set_title` on the real `ProcessTerminal` emits
    /// `\x1b]2;{title}\x07` (OSC 2); the `DummyTerminal` stub is a no-op. This
    /// accessor locks the real underlying terminal (bypassing the no-op trait
    /// impl returned by [`TUI::terminal`]) so hosts can reflect run state in
    /// the window title — e.g. "rpi — working" while a turn is in flight.
    pub fn set_title(&self, title: &str) {
        if let Ok(terminal) = self.terminal.lock() {
            terminal.set_title(title);
            terminal.flush();
        }
    }

    /// Check if this implements ViewportTUI.
    pub fn is_viewport_tui(&self) -> bool {
        true
    }

    /// Enter alternate screen mode.
    /// Refresh the cached terminal size (call on `Event::Resize`) and force a
    /// full redraw so the constrained layout re-fits the new dimensions.
    pub fn refresh_size(&self) {
        if let Ok(terminal) = self.terminal.lock() {
            terminal.refresh_size();
        }
        // Alt-screen frames need every viewport row repainted. Main-screen mode
        // keeps the previous document so its renderer can detect width changes
        // and deliberately rebuild terminal scrollback once.
        if !self.uses_main_screen() {
            if let Ok(mut prev) = self.previous_screen.lock() {
                prev.clear();
            }
        }
        self.request_render(false);
    }

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
                // Exit the alt buffer CLEANLY: back to the main buffer, clear
                // it, and show the cursor. The old path re-rendered the full
                // TUI frame (borders, backgrounds, spinner state, cursor
                // markers) into the main buffer, which is exactly the garbage
                // that made quitting look scrambled. The caller prints its own
                // farewell after this.
                terminal.write("\x1b[?1049l\x1b[2J\x1b[H\x1b[?25h");
            }
            terminal.flush();
        }
    }

    /// Render the complete component tree into the main terminal buffer. This
    /// follows native pi's regular-mode strategy: append growth with CRLF so
    /// the terminal creates real scrollback, and rewrite only the changed tail.
    fn do_render_main_screen(&self) {
        let Ok(_render_guard) = self.render_lock.lock() else {
            return;
        };
        let Ok(terminal) = self.terminal.lock() else {
            return;
        };
        let width = terminal.columns();
        let height = terminal.rows();
        let root: Arc<dyn Component> = self
            .get_layout_root()
            .unwrap_or_else(|| Arc::new(self.container.clone()));
        let raw_lines = root.render(width);
        let cursor = raw_lines.iter().enumerate().rev().find_map(|(row, line)| {
            line.find(CURSOR_MARKER)
                .map(|idx| (row, visible_width(&line[..idx])))
        });
        let lines: Vec<String> = raw_lines
            .into_iter()
            .map(|line| line.replace(CURSOR_MARKER, ""))
            .collect();
        let previous = self
            .previous_screen
            .lock()
            .map(|p| p.clone())
            .unwrap_or_default();
        let previous_width = *self.main_previous_width.lock().unwrap();
        let previous_height = *self.main_previous_height.lock().unwrap();

        // Width changes alter wrapping everywhere; redraw the document. Normal
        // streaming updates stay incremental and preserve terminal scrollback.
        if previous_width != 0 && previous_width != width {
            terminal.write("\x1b[2J\x1b[H\x1b[3J");
            terminal.write("\x1b[?2026h");
            terminal.write(&lines.join("\r\n"));
            terminal.write("\x1b[?2026l");
            *self.main_hardware_row.lock().unwrap() = lines.len().saturating_sub(1);
            *self.main_viewport_top.lock().unwrap() = lines.len().saturating_sub(height);
        } else if previous.is_empty() {
            terminal.write("\x1b[?2026h");
            terminal.write(&lines.join("\r\n"));
            terminal.write("\x1b[?2026l");
            *self.main_hardware_row.lock().unwrap() = lines.len().saturating_sub(1);
            *self.main_viewport_top.lock().unwrap() = lines.len().saturating_sub(height);
        } else {
            let mut first_changed = None;
            let max = previous.len().max(lines.len());
            for index in 0..max {
                if previous.get(index) != lines.get(index) {
                    first_changed = Some(index);
                    break;
                }
            }
            if let Some(first) = first_changed {
                let mut hardware_row = *self.main_hardware_row.lock().unwrap();
                let mut viewport_top = *self.main_viewport_top.lock().unwrap();
                let viewport_bottom = viewport_top + height.saturating_sub(1);
                let move_target = if first == previous.len() && first > 0 {
                    first - 1
                } else {
                    first
                };
                let mut output = String::from("\x1b[?2026h");
                if move_target > viewport_bottom {
                    let current_screen = hardware_row
                        .saturating_sub(viewport_top)
                        .min(height.saturating_sub(1));
                    let down = height.saturating_sub(1).saturating_sub(current_screen);
                    if down > 0 {
                        output.push_str(&format!("\x1b[{down}B"));
                    }
                    let scroll = move_target - viewport_bottom;
                    output.push_str(&"\r\n".repeat(scroll));
                    viewport_top += scroll;
                    hardware_row = move_target;
                }
                let current_screen = hardware_row.saturating_sub(viewport_top);
                let target_screen = move_target.saturating_sub(viewport_top);
                if target_screen > current_screen {
                    output.push_str(&format!("\x1b[{}B", target_screen - current_screen));
                } else if current_screen > target_screen {
                    output.push_str(&format!("\x1b[{}A", current_screen - target_screen));
                }
                output.push_str(if first == previous.len() && first > 0 {
                    "\r\n"
                } else {
                    "\r"
                });
                for index in first..lines.len() {
                    if index > first {
                        output.push_str("\r\n");
                    }
                    output.push_str("\x1b[2K");
                    output.push_str(&lines[index]);
                }
                if previous.len() > lines.len() {
                    for _ in lines.len()..previous.len() {
                        output.push_str("\r\n\x1b[2K");
                    }
                }
                output.push_str("\x1b[?2026l");
                terminal.write(&output);
                let final_row = lines.len().saturating_sub(1);
                let advanced = final_row.saturating_sub(viewport_top + height.saturating_sub(1));
                viewport_top += advanced;
                *self.main_hardware_row.lock().unwrap() = final_row;
                *self.main_viewport_top.lock().unwrap() = viewport_top;
            }
        }

        if let Some((row, col)) = cursor {
            let hardware_row = *self.main_hardware_row.lock().unwrap();
            if hardware_row > row {
                terminal.write(&format!("\x1b[{}A", hardware_row - row));
            } else if row > hardware_row {
                terminal.write(&format!("\x1b[{}B", row - hardware_row));
            }
            terminal.write(&format!("\r\x1b[{}C", col));
            terminal.write("\x1b[?25h");
            *self.main_hardware_row.lock().unwrap() = row;
        } else {
            terminal.write("\x1b[?25l");
        }
        terminal.flush();
        *self.previous_screen.lock().unwrap() = lines;
        *self.main_previous_width.lock().unwrap() = width;
        *self.main_previous_height.lock().unwrap() = previous_height.max(height);
    }

    /// Perform a differential render with constrained layout.
    fn do_render(&self, reuse_scroll_content: bool) {
        if self.uses_main_screen() {
            self.do_render_main_screen();
            return;
        }
        let Ok(_render_guard) = self.render_lock.lock() else {
            return;
        };

        let (width, height) = if let Ok(terminal) = self.terminal.lock() {
            (terminal.columns(), terminal.rows())
        } else {
            return;
        };

        // Get layout root or use container
        let root = self.get_layout_root();
        let root_component: Arc<dyn Component> =
            root.unwrap_or_else(|| Arc::new(self.container.clone()));

        // Render layout frame
        let frame = if reuse_scroll_content {
            render_layout_frame_reusing_scroll_content(root_component.clone(), width, height)
        } else {
            render_layout_frame(root_component.clone(), width, height)
        };

        // Store the frame for input handling
        if let Ok(mut current) = self.current_frame.lock() {
            *current = Some(frame.clone());
        }

        // Get screen lines and composite modal overlays above the layout.
        let mut visible = frame.lines;
        self.paint_overlays(&mut visible, width, height);

        // Cursor markers are an internal layout protocol, not terminal output.
        // Extract the location first, then strip every marker before diffing or
        // writing. Sending the APC marker to some Windows terminals caused the
        // character under the cursor to be erased when moving left/right.
        let cursor = extract_cursor_position(&visible, height);
        for line in &mut visible {
            *line = line.replace(CURSOR_MARKER, "");
        }

        // Check for full redraw
        let previous = self
            .previous_screen
            .lock()
            .map(|p| p.clone())
            .unwrap_or_default();
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

            // Position the hardware cursor using the marker location captured
            // before internal markers were stripped from `visible`.
            if let Some((row, col)) = cursor {
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

    fn paint_overlays(&self, lines: &mut [String], width: usize, height: usize) {
        for (component, options) in self.overlays.get_visible() {
            if let Some(visible) = options.visible {
                if !visible(width, height) {
                    continue;
                }
            }
            let mut overlay_lines = component.render(width);
            if overlay_lines.is_empty() {
                continue;
            }
            let natural_width = overlay_lines
                .iter()
                .map(|line| visible_width(line))
                .max()
                .unwrap_or(1);
            let overlay_width = match options.width {
                Some(SizeValue::Absolute(value)) => value,
                Some(SizeValue::Percent(value)) => ((width as f64 * value).round() as usize).max(1),
                None => natural_width,
            }
            .max(options.min_width.unwrap_or(0))
            .min(width.max(1));
            if let Some(max_height) = options.max_height.map(|value| match value {
                SizeValue::Absolute(value) => value,
                SizeValue::Percent(value) => ((height as f64 * value).round() as usize).max(1),
            }) {
                overlay_lines.truncate(max_height.max(1));
            }
            let overlay_height = overlay_lines.len().min(height.max(1));
            let margin = options.margin.unwrap_or_default();
            let x = match options.anchor {
                OverlayAnchor::TopLeft | OverlayAnchor::LeftCenter | OverlayAnchor::BottomLeft => {
                    margin.left
                }
                OverlayAnchor::TopRight
                | OverlayAnchor::RightCenter
                | OverlayAnchor::BottomRight => width.saturating_sub(overlay_width + margin.right),
                _ => width.saturating_sub(overlay_width) / 2,
            };
            let y = match options.anchor {
                OverlayAnchor::TopLeft | OverlayAnchor::TopCenter | OverlayAnchor::TopRight => {
                    margin.top
                }
                OverlayAnchor::BottomLeft
                | OverlayAnchor::BottomCenter
                | OverlayAnchor::BottomRight => {
                    height.saturating_sub(overlay_height + margin.bottom)
                }
                _ => height.saturating_sub(overlay_height) / 2,
            };
            let x = (x as i32 + options.offset_x).max(0) as usize;
            let y = (y as i32 + options.offset_y).max(0) as usize;
            for (index, line) in overlay_lines.iter().take(overlay_height).enumerate() {
                let row = y + index;
                if row >= lines.len() {
                    break;
                }
                lines[row] = composite_tui_line(&lines[row], line, x, overlay_width, width);
            }
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
        &self.terminal_proxy
    }

    fn children(&self) -> Vec<Arc<dyn Component>> {
        self.container.get_children()
    }

    fn add_child(&self, component: Arc<dyn Component>) {
        self.container.add_child(component);
    }

    fn remove_child(&self, component: &Arc<dyn Component>) {
        self.container.remove_child(component);
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
        component: Arc<dyn Component>,
        options: Option<OverlayOptions>,
    ) -> Arc<dyn OverlayHandle> {
        let handle = self.overlays.add(component, options.unwrap_or_default());
        self.request_render(false);
        Arc::new(ManagedOverlayHandle {
            handle,
            hidden: Mutex::new(false),
            focused: Mutex::new(true),
        })
    }

    fn hide_overlay(&self) {
        self.overlays.remove_topmost();
        self.request_render(false);
    }

    fn has_overlay(&self) -> bool {
        !self.overlays.get_visible().is_empty()
    }

    fn start(&self) {
        if let Ok(mut running) = self.running.lock() {
            *running = true;
        }

        // Clone the necessary Arc references for the closures
        let running = self.running.clone();

        // Get terminal and start it (spawns its own input-reader thread whose
        // callbacks are stubs below; kept for the non-pi-tui callers that still
        // rely on `start()`). The asynchronous interactive path uses
        // `start_readerless` instead to avoid a competing stdin reader.
        if let Ok(terminal) = self.terminal.lock() {
            terminal.start(
                Box::new(move |event: InputEvent| {
                    if !*running.lock().unwrap() {
                        return;
                    }
                    let _ = event; // stub: input handled by the caller's own loop
                }),
                Box::new(move || {}),
            );
        }

        self.enter_alt_screen();
        self.do_render(false);
    }

    fn stop(&self, options: TuiStopOptions) {
        if let Ok(mut running) = self.running.lock() {
            *running = false;
        }

        if self.uses_main_screen() {
            if let Ok(terminal) = self.terminal.lock() {
                // Leave the shell prompt below the rendered footer while
                // preserving all conversation rows in terminal scrollback.
                terminal.write("\x1b[?25h\r\n");
                terminal.stop();
            }
        } else {
            // Leave the alternate buffer and clear the restored main screen
            // while the terminal is still in raw mode.
            self.exit_alt_screen(options.preserve_screen);
            if let Ok(terminal) = self.terminal.lock() {
                terminal.stop();
            }
        }
    }

    fn render_now(&self, force: bool) {
        // Layout is assembled before `start()`. Rendering during that phase
        // writes the future TUI frame into the main screen, which is then
        // restored on exit and appears as uncleared output.
        if !self.is_running() {
            return;
        }
        if self.is_render_suspended() {
            return;
        }
        if force {
            if let Ok(mut prev) = self.previous_screen.lock() {
                prev.clear();
            }
        }
        self.do_render(false);
    }

    fn request_render(&self, _force: bool) {
        if self.is_render_suspended() {
            return;
        }
        self.render_now(false);
    }

    fn full_redraws(&self) -> usize {
        self.full_redraw_count.lock().map(|c| *c).unwrap_or(0)
    }
}

impl TuiAltScreen {
    /// Set up the tty and render the first frame **without** spawning the
    /// `ProcessTerminal` input-reader thread. The caller owns the input loop
    /// (e.g. a `spawn_blocking` `event::read()` loop) and handles resize/key
    /// events directly via [`TuiAltScreen::refresh_size`] / its key dispatch.
    ///
    /// This avoids two `event::read()` consumers racing the same stdin queue
    /// (the stub thread in [`TUI::start`] dropped a fraction of keystrokes).
    /// The caller is responsible for `enable_raw_mode`-dependent event reads
    /// and for calling `refresh_size` on `Event::Resize`.
    pub fn start_readerless(&self) {
        if let Ok(mut running) = self.running.lock() {
            *running = true;
        }
        if let Ok(terminal) = self.terminal.lock() {
            terminal.enter_raw_mode();
            if self.uses_main_screen() {
                // Give wheel/touchpad gestures back to the terminal emulator;
                // it can now scroll its native history smoothly.
                terminal.disable_mouse();
            }
        }
        if !self.uses_main_screen() {
            self.enter_alt_screen();
        }
        self.do_render(false);
    }

    /// Render a frame while reusing scroll-view content. This is appropriate
    /// when only the viewport or bottom dock changed. A missing cache or width
    /// change falls back to rendering fresh content automatically.
    pub fn request_render_reusing_scroll_content(&self) {
        if self.is_running() {
            self.do_render(true);
        }
    }
}

/// Check if a TUI implements ViewportTUI.
pub fn is_viewport_tui(_tui: &dyn TUI) -> bool {
    _tui.as_any().is::<TuiAltScreen>()
}

struct ManagedOverlayHandle {
    handle: super::overlay::OverlayHandle,
    hidden: Mutex<bool>,
    focused: Mutex<bool>,
}

impl OverlayHandle for ManagedOverlayHandle {
    fn hide(&self) {
        self.handle.hide();
        if let Ok(mut hidden) = self.hidden.lock() {
            *hidden = true;
        }
    }

    fn set_hidden(&self, hidden: bool) {
        self.handle.set_visible(!hidden);
        if let Ok(mut current) = self.hidden.lock() {
            *current = hidden;
        }
    }

    fn is_hidden(&self) -> bool {
        self.hidden.lock().map(|hidden| *hidden).unwrap_or(true)
    }

    fn focus(&self) {
        if let Ok(mut focused) = self.focused.lock() {
            *focused = true;
        }
    }

    fn is_focused(&self) -> bool {
        self.focused.lock().map(|focused| *focused).unwrap_or(false)
    }
}

/// Shared terminal proxy exposed through [`TUI::terminal`]. The TUI owns the
/// terminal behind a mutex because render and input paths can run concurrently,
/// while callers of the trait need a stable `&dyn Terminal` reference.
struct TerminalProxy {
    terminal: Arc<Mutex<Box<dyn Terminal>>>,
}

impl TerminalProxy {
    fn with<R>(&self, f: impl FnOnce(&dyn Terminal) -> R) -> Option<R> {
        self.terminal
            .lock()
            .ok()
            .map(|terminal| f(terminal.as_ref()))
    }
}

impl Terminal for TerminalProxy {
    fn info(&self) -> TerminalInfo {
        self.with(|terminal| terminal.info()).unwrap_or_default()
    }

    fn write(&self, data: &str) {
        let _ = self.with(|terminal| terminal.write(data));
    }

    fn hide_cursor(&self) {
        let _ = self.with(|terminal| terminal.hide_cursor());
    }

    fn show_cursor(&self) {
        let _ = self.with(|terminal| terminal.show_cursor());
    }

    fn move_cursor(&self, row: usize, col: usize) {
        let _ = self.with(|terminal| terminal.move_cursor(row, col));
    }

    fn clear_screen(&self) {
        let _ = self.with(|terminal| terminal.clear_screen());
    }

    fn set_title(&self, title: &str) {
        let _ = self.with(|terminal| terminal.set_title(title));
    }

    fn enable_mouse(&self) {
        let _ = self.with(|terminal| terminal.enable_mouse());
    }

    fn disable_mouse(&self) {
        let _ = self.with(|terminal| terminal.disable_mouse());
    }

    fn enter_raw_mode(&self) {
        let _ = self.with(|terminal| terminal.enter_raw_mode());
    }

    fn refresh_size(&self) {
        let _ = self.with(|terminal| terminal.refresh_size());
    }

    fn start(
        &self,
        on_input: Box<dyn Fn(InputEvent) + Send + Sync>,
        on_resize: Box<dyn Fn() + Send + Sync>,
    ) {
        let _ = self.with(|terminal| terminal.start(on_input, on_resize));
    }

    fn stop(&self) {
        let _ = self.with(|terminal| terminal.stop());
    }

    fn is_tty(&self) -> bool {
        self.with(|terminal| terminal.is_tty()).unwrap_or(false)
    }

    fn set_progress(&self, active: bool) {
        let _ = self.with(|terminal| terminal.set_progress(active));
    }

    fn flush(&self) {
        let _ = self.with(|terminal| terminal.flush());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Editor, Focusable, Text};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    struct RecordingTerminal {
        output: Arc<Mutex<String>>,
    }

    impl Terminal for RecordingTerminal {
        fn info(&self) -> TerminalInfo {
            TerminalInfo {
                columns: 40,
                rows: 8,
                ..Default::default()
            }
        }

        fn write(&self, data: &str) {
            self.output.lock().unwrap().push_str(data);
        }

        fn hide_cursor(&self) {}
        fn show_cursor(&self) {}
        fn move_cursor(&self, _row: usize, _col: usize) {}
        fn clear_screen(&self) {}
        fn set_title(&self, _title: &str) {}
        fn enable_mouse(&self) {}
        fn disable_mouse(&self) {}
        fn enter_raw_mode(&self) {}
        fn refresh_size(&self) {}
        fn start(
            &self,
            _on_input: Box<dyn Fn(InputEvent) + Send + Sync>,
            _on_resize: Box<dyn Fn() + Send + Sync>,
        ) {
        }
        fn stop(&self) {}
        fn is_tty(&self) -> bool {
            true
        }
        fn set_progress(&self, _active: bool) {}
        fn flush(&self) {}
    }

    #[test]
    fn cursor_marker_is_never_written_to_the_terminal() {
        let output = Arc::new(Mutex::new(String::new()));
        let terminal = RecordingTerminal {
            output: output.clone(),
        };
        let tui = TuiAltScreen::new(Box::new(terminal), true, None);
        let editor = Arc::new(Editor::simple());
        editor.set_focused(true);
        editor.insert("hello");
        tui.set_layout_root(Some(editor.clone()));
        tui.start_readerless();

        editor.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        tui.request_render(false);

        let rendered = output.lock().unwrap().clone();
        assert!(!rendered.contains(CURSOR_MARKER));
        assert!(rendered.contains("hello"));
        assert_eq!(editor.get_text(), "hello");
    }

    #[test]
    fn layout_does_not_touch_main_screen_and_stop_clears_it() {
        let output = Arc::new(Mutex::new(String::new()));
        let terminal = RecordingTerminal {
            output: output.clone(),
        };
        let tui = TuiAltScreen::new(Box::new(terminal), true, None);

        tui.set_layout_root(Some(Arc::new(Text::new("screen content", 0, 0))));
        assert!(output.lock().unwrap().is_empty());

        tui.start_readerless();
        assert!(output.lock().unwrap().contains("screen content"));

        tui.stop(TuiStopOptions::default());
        let rendered = output.lock().unwrap().clone();
        let content_position = rendered.find("screen content").unwrap();
        let clear_position = rendered
            .rfind("\x1b[?1049l\x1b[2J\x1b[H\x1b[?25h")
            .expect("stop should leave alt screen and clear the restored main screen");
        assert!(clear_position > content_position);
    }
}
