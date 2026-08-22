//! Terminal abstraction for TUI.
//!
//! Provides a trait for terminal operations and a `ProcessTerminal` implementation
//! using crossterm.

use std::io::{self, IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyEvent, MouseEvent},
    terminal as cterm,
};
use thiserror::Error;

/// Errors that can occur during terminal operations.
#[derive(Debug, Error)]
pub enum TerminalError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("Terminal error: {0}")]
    Terminal(String),
}

/// Terminal capabilities and state.
#[derive(Debug, Clone, Default)]
pub struct TerminalInfo {
    pub columns: usize,
    pub rows: usize,
    pub supports_true_color: bool,
    pub supports_mouse: bool,
    pub supports_kitty_keyboard: bool,
}

/// Input event types.
#[derive(Debug, Clone)]
pub enum InputEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize(usize, usize),
    FocusGained,
    FocusLost,
}

/// Terminal trait for abstracting terminal operations.
///
/// This allows for different terminal implementations (real terminal, virtual terminal for testing).
pub trait Terminal: Send + Sync {
    /// Get terminal information (columns, rows, capabilities).
    fn info(&self) -> TerminalInfo;

    /// Get current terminal width in columns.
    fn columns(&self) -> usize {
        self.info().columns
    }

    /// Get current terminal height in rows.
    fn rows(&self) -> usize {
        self.info().rows
    }

    /// Write raw bytes to the terminal.
    fn write(&self, data: &str);

    /// Hide the cursor.
    fn hide_cursor(&self);

    /// Show the cursor.
    fn show_cursor(&self);

    /// Move cursor to position (0-indexed).
    fn move_cursor(&self, row: usize, col: usize);

    /// Clear the screen.
    fn clear_screen(&self);

    /// Set terminal title.
    fn set_title(&self, title: &str);

    /// Enable mouse events.
    fn enable_mouse(&self);

    /// Disable mouse events.
    fn disable_mouse(&self);

    /// Enter terminal raw mode (required for `event::read()` to deliver
    /// individual key events without line buffering). Pairs with
    /// [`Terminal::stop`], which exits raw mode.
    ///
    /// Callers that drive their own input loop (instead of the reader thread
    /// spawned by [`Terminal::start`]) use this to set up the tty without
    /// spawning a competing reader — see `TuiAltScreen::start_readerless`.
    fn enter_raw_mode(&self);

    /// Re-read the current terminal size into the cached [`TerminalInfo`].
    /// Called on `Event::Resize` so subsequent renders use the new dimensions.
    fn refresh_size(&self);

    /// Start terminal raw mode.
    fn start(&self, on_input: Box<dyn Fn(InputEvent) + Send + Sync>, on_resize: Box<dyn Fn() + Send + Sync>);

    /// Stop terminal and restore original state.
    fn stop(&self);

    /// Check if terminal is a TTY.
    fn is_tty(&self) -> bool;

    /// Set progress indicator (terminal progress bar support).
    fn set_progress(&self, active: bool);

    /// Flush output buffer.
    fn flush(&self);
}

/// Process-based terminal using crossterm.
pub struct ProcessTerminal {
    info: Mutex<TerminalInfo>,
    running: Mutex<bool>,
    writer: Mutex<Box<dyn Write + Send + Sync>>,
}

impl ProcessTerminal {
    /// Create a new process terminal.
    pub fn new() -> Self {
        let (cols, rows) = cterm::size().unwrap_or((80, 24));
        let info = TerminalInfo {
            columns: cols as usize,
            rows: rows as usize,
            supports_true_color: true, // Assume true color support
            supports_mouse: true,
            supports_kitty_keyboard: false, // Will be detected
        };

        Self {
            info: Mutex::new(info),
            running: Mutex::new(false),
            writer: Mutex::new(Box::new(io::stdout())),
        }
    }

    /// Update terminal size.
    fn update_size(&self) {
        if let Ok(size) = cterm::size() {
            if let Ok(mut info) = self.info.lock() {
                info.columns = size.0 as usize;
                info.rows = size.1 as usize;
            }
        }
    }
}

impl Default for ProcessTerminal {
    fn default() -> Self {
        Self::new()
    }
}

impl Terminal for ProcessTerminal {
    fn info(&self) -> TerminalInfo {
        self.info.lock().map(|i| i.clone()).unwrap_or_default()
    }

    fn write(&self, data: &str) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.write_all(data.as_bytes());
        }
    }

    fn hide_cursor(&self) {
        self.write("\x1b[?25l");
    }

    fn show_cursor(&self) {
        self.write("\x1b[?25h");
    }

    fn move_cursor(&self, row: usize, col: usize) {
        // ANSI escape: ESC [ row ; col H (1-indexed)
        self.write(&format!("\x1b[{};{}H", row + 1, col + 1));
    }

    fn clear_screen(&self) {
        self.write("\x1b[2J\x1b[H");
    }

    fn set_title(&self, title: &str) {
        self.write(&format!("\x1b]2;{}\x07", title));
    }

    fn enable_mouse(&self) {
        // Enable mouse tracking (SGR mode)
        self.write("\x1b[?1000h\x1b[?1002h\x1b[?1006h");
    }

    fn disable_mouse(&self) {
        self.write("\x1b[?1006l\x1b[?1002l\x1b[?1000l");
    }

    fn enter_raw_mode(&self) {
        let _ = cterm::enable_raw_mode();
        if let Ok(mut running) = self.running.lock() {
            *running = true;
        }
        self.enable_mouse();
        self.hide_cursor();
        self.update_size();
        self.flush();
    }

    fn refresh_size(&self) {
        self.update_size();
    }

    fn start(&self, on_input: Box<dyn Fn(InputEvent) + Send + Sync>, on_resize: Box<dyn Fn() + Send + Sync>) {
        // Enter raw mode
        let _ = cterm::enable_raw_mode();

        if let Ok(mut running) = self.running.lock() {
            *running = true;
        }

        // Enable mouse
        self.enable_mouse();
        self.hide_cursor();
        self.flush();

        // Update size initially
        self.update_size();

        // Spawn input handling thread
        let running = Arc::new(Mutex::new(true));
        let running_clone = running.clone();
        let info_clone = Arc::new(Mutex::new(self.info()));

        std::thread::spawn(move || {
            loop {
                // Check if we're still running
                if !*running_clone.lock().unwrap() {
                    break;
                }

                // Poll for events with timeout
                match event::poll(Duration::from_millis(100)) {
                    Ok(true) => {
                        if let Ok(event) = event::read() {
                            match event {
                                Event::Key(key) => on_input(InputEvent::Key(key)),
                                Event::Mouse(mouse) => on_input(InputEvent::Mouse(mouse)),
                                Event::Resize(cols, rows) => {
                                    if let Ok(mut info) = info_clone.lock() {
                                        info.columns = cols as usize;
                                        info.rows = rows as usize;
                                    }
                                    on_resize();
                                }
                                Event::FocusGained => on_input(InputEvent::FocusGained),
                                Event::FocusLost => on_input(InputEvent::FocusLost),
                                Event::Paste(_) => { /* Ignore paste events for now */ }
                            }
                        }
                    }
                    Ok(false) => {} // Timeout, continue polling
                    Err(_) => break, // Error, stop the thread
                }
            }
        });

        // Store running flag
        if let Ok(mut r) = self.running.lock() {
            *r = true;
        }
    }

    fn stop(&self) {
        if let Ok(mut running) = self.running.lock() {
            *running = false;
        }

        self.disable_mouse();
        self.show_cursor();
        self.flush();

        // Exit raw mode
        let _ = cterm::disable_raw_mode();
    }

    fn is_tty(&self) -> bool {
        std::io::stdout().is_terminal()
    }

    fn set_progress(&self, _active: bool) {
        // Progress indicator support not implemented yet
    }

    fn flush(&self) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.flush();
        }
    }
}