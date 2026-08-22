//! Loader component - loading indicator.
//!
//! Provides a visual loading indicator for TUI.

use std::any::Any;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::component::Component;

/// Loader indicator options.
#[derive(Debug, Clone)]
pub struct LoaderIndicatorOptions {
    /// Characters to cycle through for the spinner.
    pub spinner_chars: Vec<char>,
    /// Animation frame duration.
    pub frame_duration_ms: u64,
    /// Text to display next to the spinner.
    pub text: Option<String>,
}

impl Default for LoaderIndicatorOptions {
    fn default() -> Self {
        Self {
            spinner_chars: "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".chars().collect(),
            frame_duration_ms: 80,
            text: None,
        }
    }
}

/// Loader component.
pub struct Loader {
    options: LoaderIndicatorOptions,
    frame: Mutex<usize>,
    start_time: Mutex<Option<Instant>>,
    running: Mutex<bool>,
}

impl Loader {
    /// Create a new loader.
    pub fn new() -> Self {
        Self {
            options: LoaderIndicatorOptions::default(),
            frame: Mutex::new(0),
            start_time: Mutex::new(None),
            running: Mutex::new(false),
        }
    }

    /// Create a loader with options.
    pub fn with_options(options: LoaderIndicatorOptions) -> Self {
        Self {
            options,
            frame: Mutex::new(0),
            start_time: Mutex::new(None),
            running: Mutex::new(false),
        }
    }

    /// Create a loader with text.
    pub fn with_text(text: &str) -> Self {
        let mut options = LoaderIndicatorOptions::default();
        options.text = Some(text.to_string());
        Self::with_options(options)
    }

    /// Start the loader animation.
    pub fn start(&self) {
        if let Ok(mut running) = self.running.lock() {
            *running = true;
        }
        if let Ok(mut start_time) = self.start_time.lock() {
            *start_time = Some(Instant::now());
        }
    }

    /// Stop the loader animation.
    pub fn stop(&self) {
        if let Ok(mut running) = self.running.lock() {
            *running = false;
        }
    }

    /// Check if the loader is running.
    pub fn is_running(&self) -> bool {
        *self.running.lock().unwrap()
    }

    /// Get the elapsed time since the loader started.
    pub fn elapsed(&self) -> Duration {
        self.start_time.lock()
            .ok()
            .and_then(|t| t.map(|s| s.elapsed()))
            .unwrap_or_default()
    }

    /// Advance to the next frame.
    pub fn advance(&self) {
        if let Ok(mut frame) = self.frame.lock() {
            *frame = (*frame + 1) % self.options.spinner_chars.len();
        }
    }

    /// Get the current spinner character.
    fn current_char(&self) -> char {
        let frame = *self.frame.lock().unwrap();
        self.options.spinner_chars.get(frame).copied().unwrap_or('⠋')
    }

    /// Set the text.
    pub fn set_text(&mut self, text: &str) {
        self.options.text = Some(text.to_string());
    }

    /// Clear the text.
    pub fn clear_text(&mut self) {
        self.options.text = None;
    }
}

impl Default for Loader {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Loader {
    fn render(&self, width: usize) -> Vec<String> {
        let spinner = self.current_char();
        let text = self.options.text.clone().unwrap_or_default();

        let elapsed = self.elapsed();
        let seconds = elapsed.as_secs();
        let minutes = seconds / 60;
        let secs = seconds % 60;

        let time_str = if minutes > 0 {
            format!("{}m {}s", minutes, secs)
        } else {
            format!("{}s", secs)
        };

        let line = if text.is_empty() {
            format!("\x1b[36m{}\x1b[0m \x1b[90m{}\x1b[0m", spinner, time_str)
        } else {
            let text_width = crate::utils::visible_width(&text);
            let max_text_width = width.saturating_sub(15); // Reserve space for spinner and time
            let display_text = if text_width > max_text_width {
                crate::utils::truncate_to_width(&text, max_text_width, "...")
            } else {
                text
            };
            format!("\x1b[36m{}\x1b[0m \x1b[1m{}\x1b[0m \x1b[90m{}\x1b[0m", spinner, display_text, time_str)
        };

        // Advance frame for next render
        self.advance();

        vec![line]
    }

    fn invalidate(&self) {
        // No cached state
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Cancellable loader with a cancel callback.
pub struct CancellableLoader {
    loader: Loader,
    on_cancel: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl CancellableLoader {
    /// Create a new cancellable loader.
    pub fn new() -> Self {
        Self {
            loader: Loader::new(),
            on_cancel: Mutex::new(None),
        }
    }

    /// Create with text.
    pub fn with_text(text: &str) -> Self {
        Self {
            loader: Loader::with_text(text),
            on_cancel: Mutex::new(None),
        }
    }

    /// Set cancel callback.
    pub fn on_cancel(&self, callback: Arc<dyn Fn() + Send + Sync>) {
        if let Ok(mut cb) = self.on_cancel.lock() {
            *cb = Some(callback);
        }
    }

    /// Cancel the loader.
    pub fn cancel(&self) {
        self.loader.stop();

        if let Ok(cb) = self.on_cancel.lock() {
            if let Some(callback) = cb.as_ref() {
                callback();
            }
        }
    }

    /// Start the loader.
    pub fn start(&self) {
        self.loader.start();
    }

    /// Stop the loader.
    pub fn stop(&self) {
        self.loader.stop();
    }

    /// Check if running.
    pub fn is_running(&self) -> bool {
        self.loader.is_running()
    }

    /// Set text.
    pub fn set_text(&mut self, text: &str) {
        self.loader.set_text(text);
    }
}

impl Default for CancellableLoader {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for CancellableLoader {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = self.loader.render(width);

        // Add cancel hint
        if self.is_running() {
            let hint = "\x1b[90mPress Ctrl+C to cancel\x1b[0m";
            lines.push(hint.to_string());
        }

        lines
    }

    fn invalidate(&self) {
        self.loader.invalidate();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Progress loader with percentage.
pub struct ProgressLoader {
    loader: Loader,
    progress: Mutex<f32>, // 0.0 to 1.0
}

impl ProgressLoader {
    /// Create a new progress loader.
    pub fn new() -> Self {
        Self {
            loader: Loader::new(),
            progress: Mutex::new(0.0),
        }
    }

    /// Create with text.
    pub fn with_text(text: &str) -> Self {
        Self {
            loader: Loader::with_text(text),
            progress: Mutex::new(0.0),
        }
    }

    /// Set progress (0.0 to 1.0).
    pub fn set_progress(&self, progress: f32) {
        if let Ok(mut p) = self.progress.lock() {
            *p = progress.clamp(0.0, 1.0);
        }
    }

    /// Get progress.
    pub fn get_progress(&self) -> f32 {
        *self.progress.lock().unwrap()
    }

    /// Start the loader.
    pub fn start(&self) {
        self.loader.start();
    }

    /// Stop the loader.
    pub fn stop(&self) {
        self.loader.stop();
    }
}

impl Default for ProgressLoader {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for ProgressLoader {
    fn render(&self, width: usize) -> Vec<String> {
        let progress = self.get_progress();
        let percent = (progress * 100.0) as usize;

        let spinner = self.loader.current_char();
        let text = self.loader.options.text.clone().unwrap_or_default();

        // Create progress bar
        let bar_width = width.saturating_sub(20).min(30);
        let filled = (bar_width as f32 * progress) as usize;
        let empty = bar_width - filled;

        let bar = format!(
            "\x1b[36m{}\x1b[44m{}\x1b[40m{}\x1b[0m {}%",
            spinner,
            "█".repeat(filled),
            "░".repeat(empty),
            percent
        );

        let line = if text.is_empty() {
            bar
        } else {
            let text_display = crate::utils::truncate_to_width(&text, width.saturating_sub(bar_width + 5), "...");
            format!("{} {}", text_display, bar)
        };

        // Advance frame
        self.loader.advance();

        vec![line]
    }

    fn invalidate(&self) {
        self.loader.invalidate();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_loader_new() {
        let loader = Loader::new();
        assert!(!loader.is_running());
    }

    #[test]
    fn test_loader_start_stop() {
        let loader = Loader::new();
        loader.start();
        assert!(loader.is_running());
        loader.stop();
        assert!(!loader.is_running());
    }

    #[test]
    fn test_loader_render() {
        let loader = Loader::with_text("Loading...");
        loader.start();
        let lines = loader.render(40);
        assert!(!lines.is_empty());
        assert!(lines[0].contains("Loading"));
    }

    #[test]
    fn test_cancellable_loader() {
        let loader = CancellableLoader::new();
        let cancelled = Arc::new(Mutex::new(false));
        let cancelled_clone = cancelled.clone();

        loader.on_cancel(Arc::new(move || {
            *cancelled_clone.lock().unwrap() = true;
        }));

        loader.cancel();
        assert!(*cancelled.lock().unwrap());
    }

    #[test]
    fn test_progress_loader() {
        let loader = ProgressLoader::new();
        loader.set_progress(0.5);
        assert_eq!(loader.get_progress(), 0.5);
    }
}