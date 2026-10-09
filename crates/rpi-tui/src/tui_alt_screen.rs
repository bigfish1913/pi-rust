//! TuiAltScreen - Alternate screen TUI with constrained layout.
//!
//! Uses the alternate screen buffer for fullscreen mode with a scrollable
//! transcript and fixed bottom dock, as described in `tui-plan.md`.

use std::any::Any;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::component::Component;
use super::container::Container;
use super::layout::{
    composite_tui_line, extract_cursor_position, render_layout_frame,
    render_layout_frame_reusing_scroll_content, LayoutFrame,
};
use super::overlay::OverlayManager;
use super::scroll_view::ScrollView;
use super::tui::{OverlayHandle, OverlayOptions, TuiMode, TuiStopOptions, TUI};
use crate::ansi::{visible_width, CURSOR_MARKER};
use crate::terminal::{InputEvent, Terminal, TerminalInfo};

/// Symbol for ViewportTUI capability check.
pub const VIEWPORT_TUI: &[u8] = b"rpi-tui/viewport";

/// Upper bound on the coalesced repaint rate, matching upstream's
/// `TuiBase.MIN_RENDER_INTERVAL_MS` (16 ms ≈ 60 fps).
///
/// A provider streams one `MessageUpdate` per delta, and every delta used to
/// repaint the *whole* transcript (full markdown rebuild + full layout pass +
/// a full-viewport terminal write). Throttling that to one frame per interval
/// is what keeps long, fast output from saturating the terminal — and the
/// GPU-backed compositor behind it.
pub const MIN_RENDER_INTERVAL_MS: u64 = 16;

/// Merge an incoming frame request into one that is already parked.
///
/// `reuse_scroll_content = false` means "the transcript changed, re-render it";
/// `true` means "only the viewport/dock moved, the cached transcript is fine".
/// A parked full render is never downgraded to a cached one, so a streamed
/// delta that lands inside a pure-scroll frame window is still painted.
fn merge_pending_render(parked: Option<bool>, reuse_scroll_content: bool) -> Option<bool> {
    match parked {
        Some(parked) => Some(parked && reuse_scroll_content),
        None => Some(reuse_scroll_content),
    }
}

/// Coalescing render-scheduler state — the Rust port of upstream's
/// `TuiBase.{renderRequested, renderTimer, lastRenderAt}` trio.
struct RenderSchedulerState {
    /// `None` = no frame is owed. `Some(reuse_scroll_content)` records which
    /// render variant the parked request needs.
    pending: Option<bool>,
    /// Set by [`TuiAltScreen::stop`] to end the scheduler thread.
    shutdown: bool,
    /// When the last frame was painted (upstream `lastRenderAt`).
    last_render_at: Instant,
}

/// One scheduler per TUI (never global), so a parked frame can only ever wake
/// the thread that belongs to its own screen.
struct RenderScheduler {
    state: Mutex<RenderSchedulerState>,
    cv: Condvar,
}

impl RenderScheduler {
    fn new() -> Self {
        Self {
            state: Mutex::new(RenderSchedulerState {
                pending: None,
                shutdown: false,
                last_render_at: Instant::now(),
            }),
            cv: Condvar::new(),
        }
    }

    /// Record that a frame is owed and wake the scheduler thread.
    fn park(&self, reuse_scroll_content: bool) {
        if let Ok(mut state) = self.state.lock() {
            state.pending = merge_pending_render(state.pending, reuse_scroll_content);
        }
        // The predicate is checked under the same mutex the waiter holds, so the
        // wakeup cannot be lost between the store above and the wait below.
        self.cv.notify_all();
    }

    /// Note that a frame was just painted, restarting the throttle window.
    fn mark_painted(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.last_render_at = Instant::now();
        }
    }

    /// Take the parked frame, if any.
    fn take_pending(&self) -> Option<bool> {
        self.state.lock().ok()?.pending.take()
    }

    /// Discard any parked frame (an immediate render supersedes it).
    fn clear_pending(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.pending = None;
        }
    }

    /// Signal the scheduler thread to exit and reclaim the parked frame.
    fn shutdown(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.shutdown = true;
            state.pending = None;
        }
        self.cv.notify_all();
    }
}

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
    /// own scrollback (upstream's default `regular` mode).
    main_screen_mode: Mutex<bool>,
    main_previous_width: Mutex<usize>,
    main_previous_height: Mutex<usize>,
    main_hardware_row: Mutex<usize>,
    main_viewport_top: Mutex<usize>,
    main_previous_cursor: Mutex<Option<(usize, usize)>>,
    // Input handlers
    #[allow(dead_code)]
    input_handler: Mutex<Option<Arc<dyn Fn(InputEvent) + Send + Sync>>>,
    #[allow(dead_code)]
    resize_handler: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    overlays: Arc<OverlayManager>,
    render_suspended: Mutex<bool>,
    /// Coalescing render scheduler (upstream `TuiBase` frame throttle). While
    /// it is active, `request_render(false)` and
    /// `request_render_reusing_scroll_content()` park a frame instead of
    /// repainting, and the scheduler thread paints at most one frame per
    /// [`MIN_RENDER_INTERVAL_MS`].
    scheduler: Arc<RenderScheduler>,
    /// Whether [`TuiAltScreen::start_render_scheduler`] succeeded. When false,
    /// render requests paint inline (standalone/unit-test behaviour).
    scheduler_started: AtomicBool,
    /// The scheduler thread, joined by [`TuiAltScreen::stop`] so a parked frame
    /// cannot repaint the terminal after shutdown.
    scheduler_thread: Mutex<Option<JoinHandle<()>>>,
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
            main_previous_cursor: Mutex::new(None),
            input_handler: Mutex::new(None),
            resize_handler: Mutex::new(None),
            overlays: Arc::new(OverlayManager::new()),
            render_suspended: Mutex::new(false),
            scheduler: Arc::new(RenderScheduler::new()),
            scheduler_started: AtomicBool::new(false),
            scheduler_thread: Mutex::new(None),
        }
    }

    /// Start the coalescing render scheduler — the Rust port of upstream's
    /// `TuiBase.scheduleRender()` timer. Idempotent.
    ///
    /// Every `request_render(false)` / `request_render_reusing_scroll_content()`
    /// call after this parks a frame instead of repainting immediately, and the
    /// scheduler thread paints at most one frame per [`MIN_RENDER_INTERVAL_MS`].
    /// A burst of streamed deltas therefore collapses into a single repaint of
    /// the transcript instead of one per token.
    ///
    /// Hosts that drive their own event loop may instead leave this off and call
    /// [`TuiAltScreen::flush_pending_render`] from their own tick; without either
    /// the requests paint inline (the pre-throttle behaviour).
    ///
    /// Takes `&Arc<Self>` because the scheduler thread keeps the TUI alive until
    /// [`TuiAltScreen::stop`] shuts it down; call it after the TUI has been
    /// wrapped in the `Arc` the host renders through.
    pub fn start_render_scheduler(self: &Arc<Self>) {
        if self.scheduler_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let tui = self.clone();
        let spawned = std::thread::Builder::new()
            .name("rpi-tui-render".to_string())
            .spawn(move || tui.run_render_scheduler());
        match spawned {
            Ok(handle) => {
                if let Ok(mut slot) = self.scheduler_thread.lock() {
                    *slot = Some(handle);
                }
            }
            Err(_) => {
                // A failed spawn must not silently disable repaints: fall back
                // to the synchronous path rather than parking frames forever.
                self.scheduler_started.store(false, Ordering::Release);
            }
        }
    }

    /// Scheduler thread body: sleep until a frame is owed, then paint at most one
    /// frame per [`MIN_RENDER_INTERVAL_MS`]. Idle screens pay no wakeups at all —
    /// the thread is parked on the condvar.
    fn run_render_scheduler(self: Arc<Self>) {
        let interval = Duration::from_millis(MIN_RENDER_INTERVAL_MS);
        loop {
            // ---- Wait for a frame to be owed (or shutdown) ----
            {
                let Ok(mut state) = self.scheduler.state.lock() else {
                    return;
                };
                while state.pending.is_none() && !state.shutdown {
                    let Ok(next) = self.scheduler.cv.wait(state) else {
                        return;
                    };
                    state = next;
                }
                if state.shutdown {
                    return;
                }
            }
            // ---- Hold the frame back until the throttle window elapses ----
            loop {
                let remaining = {
                    let Ok(state) = self.scheduler.state.lock() else {
                        return;
                    };
                    match interval.checked_sub(state.last_render_at.elapsed()) {
                        Some(remaining) => remaining,
                        None => break,
                    }
                };
                let Ok(state) = self.scheduler.state.lock() else {
                    return;
                };
                if state.shutdown {
                    return;
                }
                let Ok((state, _)) = self.scheduler.cv.wait_timeout(state, remaining) else {
                    return;
                };
                if state.shutdown {
                    return;
                }
            }
            self.flush_pending_render();
        }
    }

    /// Paint a frame parked by [`TUI::request_render`] /
    /// [`TuiAltScreen::request_render_reusing_scroll_content`].
    ///
    /// Returns `true` when a frame was painted, `false` when nothing was owed
    /// (or the owed frame was dropped because the TUI is stopped or a foreign
    /// runtime owns the terminal — both of those paths force a fresh full frame
    /// when they end, so nothing is lost on screen).
    ///
    /// The scheduler thread calls this; it is also the deterministic hook a host
    /// tick or a unit test can use to flush a request synchronously.
    pub fn flush_pending_render(&self) -> bool {
        let Some(reuse_scroll_content) = self.scheduler.take_pending() else {
            return false;
        };
        if !self.is_running() || self.is_render_suspended() {
            return false;
        }
        self.do_render(reuse_scroll_content);
        true
    }

    /// Park a frame request, or paint it inline when no scheduler is running.
    fn park_render(&self, reuse_scroll_content: bool) {
        if !self.scheduler_started.load(Ordering::Acquire) {
            self.do_render(reuse_scroll_content);
            return;
        }
        self.scheduler.park(reuse_scroll_content);
    }

    /// Test seam: enable frame parking *without* spawning the scheduler thread,
    /// so a test can drive [`TuiAltScreen::flush_pending_render`] itself and
    /// observe the coalescing deterministically.
    #[cfg(test)]
    fn enable_render_coalescing(&self) {
        self.scheduler_started.store(true, Ordering::Release);
    }

    /// Record the frame timestamp that gates the next coalesced repaint.
    fn mark_frame_painted(&self) {
        self.scheduler.mark_painted();
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
            // Autowrap is a terminal mode, not restored by leaving the buffer.
            terminal.write("\x1b[?7h");
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

    /// Hand the terminal back to the shell so the process can be stopped
    /// (upstream's `handleCtrlZ`): pause the renderer, leave the alternate
    /// buffer, and restore cooked mode. [`TuiAltScreen::resume`] reverses it
    /// once `SIGTSTP` returns.
    ///
    /// Unlike the private `stop`, this keeps the render scheduler alive, so the
    /// same TUI can be resumed in place.
    pub fn suspend(&self) {
        self.set_render_suspended(true);
        if let Ok(terminal) = self.terminal.lock() {
            terminal.disable_mouse();
            terminal.exit_raw_mode();
            terminal.flush();
        }
        if !self.uses_main_screen() {
            // `preserve_screen`: leave the alt buffer without clearing the
            // user's main-buffer scrollback (unlike a normal shutdown).
            self.exit_alt_screen(true);
        }
    }

    /// Re-enter raw mode and the alternate buffer after the process resumes
    /// from `SIGTSTP`, then force a full repaint (the alt buffer came back
    /// empty — `set_render_suspended` is what schedules it).
    pub fn resume(&self) {
        if !self.uses_main_screen() {
            self.enter_alt_screen();
        }
        if let Ok(terminal) = self.terminal.lock() {
            terminal.enter_raw_mode();
            if self.uses_main_screen() {
                terminal.disable_mouse();
            } else {
                terminal.enable_mouse();
            }
            terminal.flush();
        }
        self.set_render_suspended(false);
    }

    /// Render the complete component tree into the main terminal buffer. This
    /// follows upstream's regular-mode strategy: append growth with CRLF so
    /// the terminal creates real scrollback, and rewrite only the changed tail.
    fn do_render_main_screen(&self) {
        let Ok(_render_guard) = self.render_lock.lock() else {
            return;
        };
        // A render request may have passed the public suspension check while
        // a custom UI was opening. Re-check after taking the render lock so an
        // already queued outer frame cannot overwrite the foreign screen.
        if self.is_render_suspended() {
            return;
        }
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
        let previous_cursor = *self.main_previous_cursor.lock().unwrap();
        terminal.write(&crate::terminal_image::removed_kitty_placements(
            &previous, &lines,
        ));

        // Size changes alter wrapping or viewport coordinates everywhere;
        // mirror upstream and rebuild once. Normal streaming updates stay
        // incremental and preserve terminal scrollback.
        if (previous_width != 0 && previous_width != width)
            || (previous_height != 0 && previous_height != height)
        {
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
            let mut last_changed = None;
            let max = previous.len().max(lines.len());
            for index in 0..max {
                if previous.get(index) != lines.get(index) {
                    first_changed.get_or_insert(index);
                    last_changed = Some(index);
                }
            }
            if first_changed.is_none() && cursor == previous_cursor {
                return;
            }
            if let (Some(first), Some(last)) = (first_changed, last_changed) {
                let mut hardware_row = *self.main_hardware_row.lock().unwrap();
                let mut viewport_top = *self.main_viewport_top.lock().unwrap();
                let viewport_bottom = viewport_top + height.saturating_sub(1);
                let deleted_tail_only = first >= lines.len();
                let append_start = first == previous.len() && first > 0;
                let move_target = if deleted_tail_only {
                    lines.len().saturating_sub(1)
                } else if append_start {
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
                output.push_str(if append_start { "\r\n" } else { "\r" });
                let render_end = last.min(lines.len().saturating_sub(1));
                if !deleted_tail_only {
                    for index in first..=render_end {
                        if index > first {
                            output.push_str("\r\n");
                        }
                        output.push_str("\x1b[2K");
                        output.push_str(&lines[index]);
                    }
                }
                let mut final_row = render_end;
                if previous.len() > lines.len() {
                    let extra_lines = previous.len() - lines.len();
                    let clear_start_offset = usize::from(!lines.is_empty());
                    if clear_start_offset > 0 {
                        output.push_str("\x1b[1B");
                    }
                    for index in 0..extra_lines {
                        output.push_str("\r\x1b[2K");
                        if index + 1 < extra_lines {
                            output.push_str("\x1b[1B");
                        }
                    }
                    let move_back = extra_lines.saturating_sub(1) + clear_start_offset;
                    if move_back > 0 {
                        output.push_str(&format!("\x1b[{move_back}A"));
                    }
                    if extra_lines > 0 {
                        final_row = lines.len().saturating_sub(1);
                    }
                }
                output.push_str("\x1b[?2026l");
                terminal.write(&output);
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
        *self.main_previous_height.lock().unwrap() = height;
        *self.main_previous_cursor.lock().unwrap() = cursor;
    }

    /// Perform a differential render with constrained layout.
    fn do_render(&self, reuse_scroll_content: bool) {
        if self.is_render_suspended() {
            return;
        }
        // upstream stamps the frame time immediately before `doRender()`; the
        // scheduler reads it to hold the next request back for the remainder of
        // the throttle window.
        self.mark_frame_painted();
        if self.uses_main_screen() {
            self.do_render_main_screen();
            return;
        }
        let Ok(_render_guard) = self.render_lock.lock() else {
            return;
        };
        if self.is_render_suspended() {
            return;
        }

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
        buffer.push_str(&crate::terminal_image::removed_kitty_placements(
            &previous, &visible,
        ));

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
        for overlay in self.overlays.render_visible(width, height) {
            let (x, y, overlay_width) = (overlay.x, overlay.y, overlay.width);
            let overlay_lines = overlay.lines;
            let overlay_height = overlay_lines.len();
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
        if self.uses_main_screen() {
            TuiMode::Regular
        } else {
            TuiMode::Fullscreen
        }
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

        // Keyboard protocol stacks belong to each screen buffer. Switch first
        // so the terminal enables its protocol on the screen that receives input.
        if !self.uses_main_screen() {
            self.enter_alt_screen();
        }

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
            if self.uses_main_screen() {
                terminal.disable_mouse();
            } else {
                terminal.enable_mouse();
            }
            terminal.flush();
        }

        self.do_render(false);
    }

    fn stop(&self, options: TuiStopOptions) {
        if let Ok(mut running) = self.running.lock() {
            *running = false;
        }

        // Stop the coalescing scheduler *before* the terminal goes away, and
        // join it so a frame already in flight cannot repaint the screen after
        // shutdown (the class of bug the 0.1.18 regular-mode fix targets).
        self.scheduler.shutdown();
        if let Some(handle) = self.scheduler_thread.lock().ok().and_then(|mut s| s.take()) {
            let _ = handle.join();
        }

        if self.uses_main_screen() {
            if let Ok(terminal) = self.terminal.lock() {
                // Leave the shell prompt below the rendered footer while
                // preserving all conversation rows in terminal scrollback.
                terminal.write("\x1b[?25h\r\n");
                terminal.stop();
            }
        } else {
            // Pop the keyboard protocol on its owning screen before leaving it.
            if let Ok(terminal) = self.terminal.lock() {
                terminal.stop();
            }
            self.exit_alt_screen(options.preserve_screen);
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
        // An immediate frame supersedes a parked coalesced one: upstream's
        // `renderNow` clears `renderRequested` and cancels the render timer.
        self.scheduler.clear_pending();
        self.do_render(false);
    }

    fn request_render(&self, force: bool) {
        if self.is_render_suspended() {
            return;
        }
        if force {
            // upstream `requestRender(true)`: reset the diff state and paint an
            // immediate frame instead of waiting out the throttle window.
            self.render_now(true);
            return;
        }
        if !self.is_running() {
            return;
        }
        // Coalesced (upstream `requestRender(false)`): park the frame and let
        // the scheduler paint it at most once per `MIN_RENDER_INTERVAL_MS`.
        self.park_render(false);
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
    /// The first frame is painted inline. Hosts that want the native-pi frame
    /// throttle should also call [`TuiAltScreen::start_render_scheduler`].
    ///
    /// This avoids two `event::read()` consumers racing the same stdin queue
    /// (the stub thread in [`TUI::start`] dropped a fraction of keystrokes).
    /// The caller is responsible for `enable_raw_mode`-dependent event reads
    /// and for calling `refresh_size` on `Event::Resize`.
    pub fn start_readerless(&self) {
        if let Ok(mut running) = self.running.lock() {
            *running = true;
        }
        if !self.uses_main_screen() {
            self.enter_alt_screen();
        }
        if let Ok(terminal) = self.terminal.lock() {
            terminal.enter_raw_mode();
            if self.uses_main_screen() {
                // Give wheel/touchpad gestures back to the terminal emulator;
                // it can now scroll its native history smoothly.
                terminal.disable_mouse();
            } else {
                // On macOS enter_raw_mode intentionally leaves mouse tracking
                // disabled so the main screen can use native scrollback. The
                // alternate-screen transcript owns scrolling, however, so it
                // must explicitly re-enable wheel/touchpad events here.
                terminal.enable_mouse();
            }
            terminal.flush();
        }
        self.do_render(false);
    }

    /// Render a frame while reusing scroll-view content. This is appropriate
    /// when only the viewport or bottom dock changed. A missing cache or width
    /// change falls back to rendering fresh content automatically.
    ///
    /// Coalesced like [`TUI::request_render`]; a request that upgrades the parked
    /// frame to a full transcript render wins.
    pub fn request_render_reusing_scroll_content(&self) {
        if !self.is_running() || self.is_render_suspended() {
            return;
        }
        self.park_render(true);
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

    fn exit_raw_mode(&self) {
        let _ = self.with(|terminal| terminal.exit_raw_mode());
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
        fn enter_raw_mode(&self) {
            self.write("<raw-on>");
        }
        fn exit_raw_mode(&self) {
            self.write("<raw-off>");
        }
        fn refresh_size(&self) {}
        fn start(
            &self,
            _on_input: Box<dyn Fn(InputEvent) + Send + Sync>,
            _on_resize: Box<dyn Fn() + Send + Sync>,
        ) {
            self.enter_raw_mode();
        }
        fn stop(&self) {
            self.write("<stop>");
        }
        fn is_tty(&self) -> bool {
            true
        }
        fn set_progress(&self, _active: bool) {}
        fn flush(&self) {}
    }

    #[test]
    fn keyboard_setup_and_teardown_stay_on_the_owning_screen_buffer() {
        for readerless in [true, false] {
            let output = Arc::new(Mutex::new(String::new()));
            let tui = TuiAltScreen::new(
                Box::new(RecordingTerminal {
                    output: output.clone(),
                }),
                true,
                None,
            );
            if readerless {
                tui.start_readerless();
            } else {
                tui.start();
            }
            let rendered = output.lock().unwrap().clone();
            assert!(rendered.find("\x1b[?1049h").unwrap() < rendered.find("<raw-on>").unwrap());
            output.lock().unwrap().clear();
            tui.suspend();
            let rendered = output.lock().unwrap().clone();
            assert!(rendered.find("<raw-off>").unwrap() < rendered.find("\x1b[?1049l").unwrap());
            assert!(rendered.contains("\x1b[?7h"));
            output.lock().unwrap().clear();
            tui.resume();
            let rendered = output.lock().unwrap().clone();
            assert!(rendered.find("\x1b[?1049h").unwrap() < rendered.find("<raw-on>").unwrap());
            output.lock().unwrap().clear();
            tui.stop(TuiStopOptions::default());
            let rendered = output.lock().unwrap().clone();
            assert!(rendered.find("<stop>").unwrap() < rendered.find("\x1b[?1049l").unwrap());
        }
    }

    #[test]
    fn regular_start_never_switches_to_the_alternate_screen() {
        let output = Arc::new(Mutex::new(String::new()));
        let tui = TuiAltScreen::new(
            Box::new(RecordingTerminal {
                output: output.clone(),
            }),
            true,
            None,
        );
        tui.set_main_screen_mode(true);
        tui.start();
        tui.suspend();
        tui.resume();
        tui.stop(TuiStopOptions::default());
        let rendered = output.lock().unwrap().clone();
        assert!(rendered.contains("<raw-on>"));
        assert!(!rendered.contains("\x1b[?1049"));
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

    #[test]
    fn cached_redraw_is_suppressed_while_rendering_is_suspended() {
        let output = Arc::new(Mutex::new(String::new()));
        let terminal = RecordingTerminal {
            output: output.clone(),
        };
        let tui = TuiAltScreen::new(Box::new(terminal), true, None);
        tui.set_layout_root(Some(Arc::new(Text::new("outer frame", 0, 0))));
        tui.start_readerless();

        tui.set_render_suspended(true);
        let before = output.lock().unwrap().len();
        tui.request_render_reusing_scroll_content();
        assert_eq!(output.lock().unwrap().len(), before);

        tui.set_render_suspended(false);
        tui.set_layout_root(Some(Arc::new(Text::new("new frame", 0, 0))));
        assert!(output.lock().unwrap().len() > before);
    }

    #[test]
    fn regular_single_line_update_does_not_rewrite_unchanged_tail() {
        let output = Arc::new(Mutex::new(String::new()));
        let terminal = RecordingTerminal {
            output: output.clone(),
        };
        let tui = TuiAltScreen::new(Box::new(terminal), true, None);
        let content = Arc::new(Text::new("header\nstatus\neditor\nfooter", 0, 0));
        tui.set_main_screen_mode(true);
        tui.set_layout_root(Some(content.clone()));
        tui.start_readerless();
        output.lock().unwrap().clear();

        content.set_text("header\nstatus2\neditor\nfooter");
        tui.request_render(false);

        let rendered = output.lock().unwrap().clone();
        assert!(rendered.contains("status2"));
        assert!(!rendered.contains("editor"));
        assert!(!rendered.contains("footer"));
    }

    #[test]
    fn regular_unchanged_frame_writes_nothing() {
        let output = Arc::new(Mutex::new(String::new()));
        let terminal = RecordingTerminal {
            output: output.clone(),
        };
        let tui = TuiAltScreen::new(Box::new(terminal), true, None);
        tui.set_main_screen_mode(true);
        tui.set_layout_root(Some(Arc::new(Text::new(
            "header\nstatus\neditor\nfooter",
            0,
            0,
        ))));
        tui.start_readerless();
        output.lock().unwrap().clear();

        tui.request_render(false);

        assert!(output.lock().unwrap().is_empty());
    }

    #[test]
    fn merge_pending_render_never_downgrades_a_full_render() {
        // Nothing parked: the request's own variant wins.
        assert_eq!(merge_pending_render(None, false), Some(false));
        assert_eq!(merge_pending_render(None, true), Some(true));
        // A parked cached-content frame is upgraded by a full render...
        assert_eq!(merge_pending_render(Some(true), false), Some(false));
        // ...and stays cached when another cached request arrives.
        assert_eq!(merge_pending_render(Some(true), true), Some(true));
        // A parked full render is never downgraded by a cached request.
        assert_eq!(merge_pending_render(Some(false), true), Some(false));
        assert_eq!(merge_pending_render(Some(false), false), Some(false));
    }

    #[test]
    fn rapid_requests_coalesce_into_a_single_frame() {
        let output = Arc::new(Mutex::new(String::new()));
        let terminal = RecordingTerminal {
            output: output.clone(),
        };
        let tui = TuiAltScreen::new(Box::new(terminal), true, None);
        let content = Arc::new(Text::new("first", 0, 0));
        tui.set_layout_root(Some(content.clone()));
        tui.enable_render_coalescing();
        tui.start_readerless();
        output.lock().unwrap().clear();

        // A streaming burst: many requests, no painting until the host flush.
        content.set_text("second");
        for _ in 0..50 {
            tui.request_render(false);
        }
        assert!(
            output.lock().unwrap().is_empty(),
            "parked requests must not repaint the transcript"
        );

        // The host flush paints exactly one frame and clears what was owed.
        assert!(tui.flush_pending_render());
        let rendered = output.lock().unwrap().clone();
        assert!(rendered.contains("second"));
        assert!(!tui.flush_pending_render(), "nothing owed after the flush");
    }

    #[test]
    fn immediate_render_discards_a_parked_frame() {
        let output = Arc::new(Mutex::new(String::new()));
        let terminal = RecordingTerminal {
            output: output.clone(),
        };
        let tui = TuiAltScreen::new(Box::new(terminal), true, None);
        let content = Arc::new(Text::new("before", 0, 0));
        tui.set_layout_root(Some(content.clone()));
        tui.enable_render_coalescing();
        tui.start_readerless();
        output.lock().unwrap().clear();

        content.set_text("after");
        tui.request_render(false);
        // An immediate frame (keyboard input, selector open) wins over the
        // parked one instead of rendering the same content twice.
        tui.render_now(true);

        assert!(output.lock().unwrap().contains("after"));
        assert!(!tui.flush_pending_render());
    }

    /// End-to-end throttle check on the real scheduler thread. Each frame emits
    /// exactly one synchronized-output opener (`\x1b[?2026h`), so counting those
    /// counts painted frames. Before the throttle, every one of the 200
    /// "streamed deltas" repainted the whole transcript.
    #[test]
    fn streaming_burst_paints_at_most_one_frame_per_interval() {
        let output = Arc::new(Mutex::new(String::new()));
        let terminal = RecordingTerminal {
            output: output.clone(),
        };
        let tui = Arc::new(TuiAltScreen::new(Box::new(terminal), true, None));
        let content = Arc::new(Text::new("0", 0, 0));
        tui.set_layout_root(Some(content.clone()));
        tui.start_render_scheduler();
        tui.start_readerless();
        output.lock().unwrap().clear();

        for i in 0..200 {
            content.set_text(&format!("delta {i}"));
            tui.request_render(false);
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(Duration::from_millis(50));

        let frames = output.lock().unwrap().matches("\x1b[?2026h").count();
        assert!(frames >= 1, "the scheduler must still paint frames");
        assert!(
            frames <= 40,
            "200 requests in ~250 ms must coalesce into ~16 ms frames, painted {frames}"
        );
        tui.stop(TuiStopOptions::default());
    }
}
