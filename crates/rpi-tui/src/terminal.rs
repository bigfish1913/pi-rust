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

/// Mouse tracking prevents native text selection in macOS terminal emulators.
/// RPI only consumes wheel events, so keep the platform default selectable.
const fn mouse_tracking_enabled_for(is_macos: bool) -> bool {
    !is_macos
}

const fn mouse_tracking_enabled_by_default() -> bool {
    mouse_tracking_enabled_for(cfg!(target_os = "macos"))
}

/// The kitty keyboard protocol (`\x1b[>u`) makes macOS IME composition stop
/// working (no candidate window → CJK input breaks). Disable it on macOS by
/// default; `RPI_KITTY_KEYBOARD=1` re-enables it for push-to-talk-style
/// key-release handling, `RPI_KITTY_KEYBOARD=0` forces it off elsewhere.
fn keyboard_protocol_enabled() -> bool {
    if let Some(value) = std::env::var_os("RPI_KITTY_KEYBOARD") {
        return value != "0";
    }
    !cfg!(target_os = "macos")
}

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

    /// Leave raw mode *without* stopping the terminal, so the shell gets a
    /// working line discipline back. Pairs with [`Terminal::enter_raw_mode`];
    /// a `Ctrl+Z` suspend uses it, then re-enters raw mode on resume.
    fn exit_raw_mode(&self);

    /// Re-read the current terminal size into the cached [`TerminalInfo`].
    /// Called on `Event::Resize` so subsequent renders use the new dimensions.
    fn refresh_size(&self);

    /// Start terminal raw mode.
    fn start(
        &self,
        on_input: Box<dyn Fn(InputEvent) + Send + Sync>,
        on_resize: Box<dyn Fn() + Send + Sync>,
    );

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
    #[cfg(any(unix, test))]
    keyboard: Mutex<KeyboardProtocolState>,
}

#[cfg(any(unix, test))]
#[derive(Clone, Copy)]
enum KeyboardProtocol {
    Kitty,
    Legacy,
}

#[cfg(any(unix, test))]
#[derive(Default)]
struct KeyboardProtocolState {
    detected: Option<bool>,
    active: Option<KeyboardProtocol>,
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
            #[cfg(any(unix, test))]
            keyboard: Mutex::new(KeyboardProtocolState::default()),
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

    fn configure_mouse_tracking(&self) {
        if mouse_tracking_enabled_by_default() {
            self.enable_mouse();
        } else {
            self.disable_mouse();
        }
    }

    /// Negotiate before starting the input reader. Cache support so resuming
    /// never queries stdin while a reader is already running.
    #[cfg(any(unix, test))]
    fn enable_keyboard_protocol(&self, detect: impl FnOnce() -> bool) {
        let mut keyboard = self.keyboard.lock().unwrap_or_else(|p| p.into_inner());
        if keyboard.active.is_some() {
            return;
        }
        let supported = match keyboard.detected {
            Some(supported) => supported,
            None => {
                // Match Pi: 1: disambiguate, 2: repeat/release, 4: alternate keys.
                // Leave text as UTF-8 for IME commits. Flag 8 replaces text
                // with key reports; crossterm cannot consume associated text.
                self.write("\x1b[>7u");
                self.flush();
                let supported = detect();
                keyboard.detected = Some(supported);
                if !supported {
                    self.write("\x1b[<u");
                }
                if let Ok(mut info) = self.info.lock() {
                    info.supports_kitty_keyboard = supported;
                }
                if supported {
                    keyboard.active = Some(KeyboardProtocol::Kitty);
                    return;
                }
                false
            }
        };
        let protocol = if supported {
            self.write("\x1b[>7u");
            KeyboardProtocol::Kitty
        } else {
            // Crossterm cannot parse xterm's CSI 27;modifier;key~ encoding.
            // Keep legacy input rather than enabling modifyOtherKeys and
            // losing keys. Apple Terminal's Shift+Enter is normalized below.
            KeyboardProtocol::Legacy
        };
        keyboard.active = Some(protocol);
    }

    #[cfg(any(unix, test))]
    fn disable_keyboard_protocol(&self) {
        let mut keyboard = self.keyboard.lock().unwrap_or_else(|p| p.into_inner());
        match keyboard.active.take() {
            Some(KeyboardProtocol::Kitty) => self.write("\x1b[<u"),
            Some(KeyboardProtocol::Legacy) | None => {}
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
        self.configure_mouse_tracking();
        self.hide_cursor();
        // Enable bracketed paste mode so pasted text arrives as a single
        // Event::Paste instead of individual key events (which would trigger
        // submit per line for multi-line pastes).
        self.write("\x1b[?2004h");
        #[cfg(unix)]
        if keyboard_protocol_enabled() && self.is_tty() && io::stdin().is_terminal() {
            self.enable_keyboard_protocol(|| {
                cterm::supports_keyboard_enhancement().unwrap_or(false)
            });
        }
        self.update_size();
        self.flush();
    }

    fn exit_raw_mode(&self) {
        // Mirror `enter_raw_mode` without tearing the terminal down: drop the
        // bracketed-paste mode it turned on and restore the line discipline.
        self.write("\x1b[?2004l");
        #[cfg(unix)]
        self.disable_keyboard_protocol();
        self.flush();
        let _ = cterm::disable_raw_mode();
    }

    fn refresh_size(&self) {
        self.update_size();
    }

    fn start(
        &self,
        on_input: Box<dyn Fn(InputEvent) + Send + Sync>,
        on_resize: Box<dyn Fn() + Send + Sync>,
    ) {
        self.enter_raw_mode();

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
                                Event::Key(key) => {
                                    on_input(InputEvent::Key(normalize_key_event(key)))
                                }
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
                    Ok(false) => {}  // Timeout, continue polling
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
        // Disable bracketed paste mode before exiting raw mode.
        self.write("\x1b[?2004l");
        #[cfg(unix)]
        self.disable_keyboard_protocol();
        self.flush();

        // Exit raw mode
        let _ = cterm::disable_raw_mode();
    }

    fn is_tty(&self) -> bool {
        std::io::stdout().is_terminal()
    }

    fn set_progress(&self, active: bool) {
        // OSC 9;4 is supported by modern terminal emulators (including
        // Windows Terminal, iTerm2-compatible terminals, and many Linux
        // terminals). State 1 starts an indeterminate progress indicator and
        // state 0 clears it.
        let state = if active { 1 } else { 0 };
        self.write(&format!("\x1b]9;4;{state};0\x07"));
        self.flush();
    }

    fn flush(&self) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.flush();
        }
    }
}

/// Apple Terminal sends the same CR for Enter and Shift+Enter. Like Pi's
/// native helper, consult the local modifier state only for that legacy key.
/// SSH input belongs to the remote user, not the Mac's physical keyboard.
pub fn normalize_key_event(key: KeyEvent) -> KeyEvent {
    #[cfg(target_os = "macos")]
    if key.code == event::KeyCode::Enter
        && key.modifiers.is_empty()
        && local_apple_terminal(
            std::env::var("TERM_PROGRAM").ok().as_deref(),
            ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
                .iter()
                .any(|name| std::env::var_os(name).is_some()),
        )
    {
        #[link(name = "CoreGraphics", kind = "framework")]
        extern "C" {
            fn CGEventSourceFlagsState(state_id: i32) -> u64;
        }
        // kCGEventSourceStateCombinedSessionState = 0; maskShift = 1 << 17.
        // This accessor reads modifier flags without creating an event tap.
        let shift_pressed = unsafe { CGEventSourceFlagsState(0) } & (1 << 17) != 0;
        return normalize_native_shift_enter(key, shift_pressed);
    }
    key
}

#[cfg(any(target_os = "macos", test))]
fn local_apple_terminal(term_program: Option<&str>, ssh: bool) -> bool {
    term_program == Some("Apple_Terminal") && !ssh
}

#[cfg(any(target_os = "macos", test))]
fn normalize_native_shift_enter(mut key: KeyEvent, shift_pressed: bool) -> KeyEvent {
    if key.code == event::KeyCode::Enter && key.modifiers.is_empty() && shift_pressed {
        key.modifiers.insert(event::KeyModifiers::SHIFT);
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct RecordedOutput(Arc<Mutex<Vec<u8>>>);
    impl Write for RecordedOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn kitty_protocol_preserves_ime_text_and_restores_once_per_activation() {
        let terminal = ProcessTerminal::new();
        let output = RecordedOutput(Arc::new(Mutex::new(Vec::new())));
        *terminal.writer.lock().unwrap() = Box::new(output.clone());
        terminal.enable_keyboard_protocol(|| true);
        assert!(terminal.info().supports_kitty_keyboard);
        terminal.enable_keyboard_protocol(|| panic!("active protocol must not be pushed twice"));
        terminal.disable_keyboard_protocol();
        terminal.disable_keyboard_protocol();
        terminal.enable_keyboard_protocol(|| panic!("resume must not query stdin"));
        terminal.disable_keyboard_protocol();
        let bytes = output.0.lock().unwrap().clone();
        assert_eq!(bytes, b"\x1b[>7u\x1b[<u\x1b[>7u\x1b[<u");
        // Check the actual emitted flags to prevent reintroducing all-key mode.
        let sequence = std::str::from_utf8(&bytes)
            .unwrap()
            .split('u')
            .next()
            .unwrap();
        let mask: u8 = sequence.strip_prefix("\x1b[>").unwrap().parse().unwrap();
        let flags = event::KeyboardEnhancementFlags::from_bits(mask).unwrap();
        assert!(flags.contains(event::KeyboardEnhancementFlags::REPORT_EVENT_TYPES));
        assert!(flags.contains(event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES));
        assert!(flags.contains(event::KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS));
        assert!(!flags.contains(event::KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES));
        // Bit 16 is associated-text reporting, unsupported by crossterm 0.27.
        assert_eq!(mask & 16, 0);
    }

    #[test]
    fn legacy_terminal_does_not_enable_an_encoding_the_reader_cannot_parse() {
        let terminal = ProcessTerminal::new();
        let output = RecordedOutput(Arc::new(Mutex::new(Vec::new())));
        *terminal.writer.lock().unwrap() = Box::new(output.clone());
        terminal.enable_keyboard_protocol(|| false);
        assert!(!terminal.info().supports_kitty_keyboard);
        terminal.disable_keyboard_protocol();
        terminal.enable_keyboard_protocol(|| panic!("resume must reuse detection"));
        terminal.disable_keyboard_protocol();
        assert_eq!(output.0.lock().unwrap().as_slice(), b"\x1b[>7u\x1b[<u");
    }

    #[test]
    fn native_shift_enter_only_changes_unmodified_enter_in_local_apple_terminal() {
        use event::{KeyCode, KeyModifiers};
        assert!(local_apple_terminal(Some("Apple_Terminal"), false));
        assert!(!local_apple_terminal(Some("Apple_Terminal"), true));
        assert!(!local_apple_terminal(Some("iTerm.app"), false));
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            normalize_native_shift_enter(enter, true).modifiers,
            KeyModifiers::SHIFT
        );
        assert_eq!(normalize_native_shift_enter(enter, false), enter);
        for key in [
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
        ] {
            assert_eq!(normalize_native_shift_enter(key, true), key);
        }
    }

    #[test]
    fn native_shift_enter_inserts_a_line_before_plain_enter_submits() {
        use event::{KeyCode, KeyModifiers};
        let editor = crate::Editor::simple();
        let submitted = Arc::new(Mutex::new(Vec::new()));
        let capture = submitted.clone();
        editor.on_submit(Arc::new(move |text| {
            capture.lock().unwrap().push(text.to_string())
        }));
        editor.insert("first");
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        editor.handle_key(normalize_native_shift_enter(enter, true));
        editor.insert("second");
        assert_eq!(editor.get_text(), "first\nsecond");
        assert!(submitted.lock().unwrap().is_empty());
        editor.handle_key(normalize_native_shift_enter(enter, false));
        assert_eq!(submitted.lock().unwrap().as_slice(), &["first\nsecond"]);
    }

    #[test]
    fn mouse_tracking_default_preserves_native_selection_on_macos() {
        assert!(!mouse_tracking_enabled_for(true));
        assert!(mouse_tracking_enabled_for(false));
    }

    #[test]
    fn ime_committed_utf8_text_reaches_editor_without_submitting() {
        use event::{KeyCode, KeyModifiers};
        let editor = crate::Editor::simple();
        let submitted = Arc::new(Mutex::new(Vec::new()));
        let capture = submitted.clone();
        editor.on_submit(Arc::new(move |text| {
            capture.lock().unwrap().push(text.to_string())
        }));
        let committed = "你好，世界𠮷 abc";
        for ch in committed.chars() {
            editor.handle_key(normalize_key_event(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )));
        }
        assert_eq!(editor.get_text(), committed);
        assert!(submitted.lock().unwrap().is_empty());
        editor.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(submitted.lock().unwrap().as_slice(), &[committed]);
    }
}
