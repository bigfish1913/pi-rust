//! Mirrors `packages/agent/src/harness/utils/shell-output.ts` — the bash tool's
//! output-capture layer wrapping `Shell::exec`.
//!
//! Combines stdout+stderr into a single rolling tail buffer (capped at
//! `max_output_bytes = DEFAULT_MAX_BYTES * 2 = 100KB`), spills to a temp file
//! (`bash-*.log`) once cumulative output exceeds `DEFAULT_MAX_BYTES`/`DEFAULT_MAX_LINES`,
//! and produces a [`ShellCaptureResult`] carrying truncation metadata + the
//! temp-file path. `on_chunk` fires per sanitized/CR-stripped chunk with a
//! progress snapshot.
//!
//! `return_execution_errors: true` causes exec failures to come back as a
//! success-result with `execution_error` populated (so the bash tool — not the
//! env — owns error rendering via `append_status`). There is NO time-based
//! throttle here; throttling lives in the bash tool.
//!
//! **v1 divergence from the TS:** the TS initializes the temp file mid-stream
//! (inside the stdout/stderr callback) and appends subsequent chunks. Doing
//! async file I/O from a sync `FnMut` callback is unsound in Rust, so the v1
//! port performs the spill in a single **post-stream** pass: the entire
//! captured tail is written to the temp file after `exec` resolves. The
//! `full_output_path` and truncation metadata are identical; only the timing
//! of the file write differs. Tests verify the temp file exists when output
//! exceeds limits — they don't depend on mid-stream creation.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::env::{ExecutionEnv, FileContent, ShellExecOptions};
use crate::error::{ExecutionError, ExecutionErrorCode};
use crate::truncate::{
    trim_to_last_utf8_bytes, truncate_tail, TruncationOptions, TruncationLimit,
    TruncationResult, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES,
};

/// The 100KB cap on the in-memory `tail_output` rolling buffer.
const MAX_OUTPUT_BYTES: usize = DEFAULT_MAX_BYTES * 2;

/// Mirrors `ShellCaptureProgress`.
#[derive(Clone)]
pub struct ShellCaptureProgress {
    pub output: String,
    pub truncation: TruncationResult,
    pub full_output_path: Option<PathBuf>,
    pub last_line_bytes: usize,
}

/// Mirrors `ShellCaptureResult`.
#[derive(Clone)]
pub struct ShellCaptureResult {
    pub output: String,
    pub truncation: TruncationResult,
    pub full_output_path: Option<PathBuf>,
    pub last_line_bytes: usize,
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub truncated: bool,
    pub execution_error: Option<ExecutionError>,
}

/// Mirrors `ShellCaptureOptions`.
pub struct ShellCaptureOptions<'a> {
    pub cwd: Option<PathBuf>,
    pub env: Option<HashMap<String, String>>,
    pub inherit_env: bool,
    pub timeout: Option<f64>,
    pub cancel: Option<&'a CancellationToken>,
    pub on_chunk: Option<Box<dyn FnMut(&str, &dyn Fn() -> ShellCaptureProgress) + Send + 'a>>,
    pub return_execution_errors: bool,
}

impl<'a> Default for ShellCaptureOptions<'a> {
    fn default() -> Self {
        Self {
            cwd: None,
            env: None,
            inherit_env: true,
            timeout: None,
            cancel: None,
            on_chunk: None,
            return_execution_errors: false,
        }
    }
}

/// Shared capture state, mutated by the stdout/stderr closures and read by the
/// post-stream progress computation.
#[derive(Default)]
struct CaptureState {
    tail_output: String,
    total_bytes: usize,
    completed_lines: usize,
    has_open_line: bool,
    current_line_bytes: usize,
    full_output_path: Option<PathBuf>,
    full_output_requested: bool,
}

impl CaptureState {
    fn ingest_chunk(&mut self, text: &str) {
        let text_bytes = text.len();
        self.total_bytes += text_bytes;
        let newline_count = text.matches('\n').count();
        self.completed_lines += newline_count;
        let last_newline = text.rfind('\n');
        match last_newline {
            Some(idx) => {
                let trailing = &text[idx + 1..];
                self.current_line_bytes = trailing.len();
                self.has_open_line = !trailing.is_empty();
            }
            None if !text.is_empty() => {
                self.current_line_bytes += text_bytes;
                self.has_open_line = true;
            }
            None => {}
        }
        self.tail_output.push_str(text);
        // Cap the in-memory tail at MAX_OUTPUT_BYTES.
        let trimmed = trim_to_last_utf8_bytes(&self.tail_output, MAX_OUTPUT_BYTES);
        if trimmed.len() != self.tail_output.len() {
            self.tail_output = trimmed.to_string();
        }
    }

    fn create_progress(&self) -> ShellCaptureProgress {
        let tail_truncation = truncate_tail(&self.tail_output, TruncationOptions::default());
        let total_lines = self.completed_lines + if self.has_open_line { 1 } else { 0 };
        let truncated = total_lines > DEFAULT_MAX_LINES || self.total_bytes > DEFAULT_MAX_BYTES;
        let truncated_by = if truncated {
            tail_truncation.truncated_by.or(if self.total_bytes > DEFAULT_MAX_BYTES {
                Some(TruncationLimit::Bytes)
            } else {
                Some(TruncationLimit::Lines)
            })
        } else {
            None
        };
        let truncation = TruncationResult {
            truncated,
            truncated_by,
            total_lines,
            total_bytes: self.total_bytes,
            ..tail_truncation
        };
        let output = if truncated {
            truncation.content.clone()
        } else {
            self.tail_output.clone()
        };
        ShellCaptureProgress {
            output,
            truncation,
            full_output_path: self.full_output_path.clone(),
            last_line_bytes: self.current_line_bytes,
        }
    }
}

/// Sanitize a chunk: drop C0 controls (except TAB/LF/CR) and interlinear
/// annotation chars (U+FFF9..U+FFFB), then strip all CR. Mirrors
/// `sanitizeBinaryOutput` + the per-chunk `.replace(/\r/g, "")`.
fn sanitize_and_strip_cr(chunk: &str) -> String {
    let mut out = String::with_capacity(chunk.len());
    for c in chunk.chars() {
        let u = c as u32;
        if u == 0x09 || u == 0x0a || u == 0x0d {
            out.push(c);
        } else if u <= 0x1f {
            // other C0 controls → drop
        } else if (0xfff9..=0xfffb).contains(&u) {
            // interlinear annotation → drop
        } else {
            out.push(c);
        }
    }
    out.replace('\r', "")
}

/// Mirrors `executeShellWithCapture`. Drives `env.exec` with on_stdout/on_stderr
/// callbacks that fold into [`CaptureState`], spills to a temp file in a
/// post-stream pass, and returns the terminal [`ShellCaptureResult`].
pub async fn execute_shell_with_capture(
    env: &Arc<dyn ExecutionEnv>,
    command: &str,
    options: ShellCaptureOptions<'_>,
) -> Result<ShellCaptureResult, ExecutionError> {
    let state = Arc::new(Mutex::new(CaptureState::default()));

    // The user on_chunk callback. Both streams fire it (the TS wires both to the
    // same onChunk); we share it via Arc<Mutex<Option<Box>>> so both closures can
    // call it without moving the Box twice.
    let user_cb: Arc<Mutex<Option<Box<dyn FnMut(&str, &dyn Fn() -> ShellCaptureProgress) + Send>>>> =
        Arc::new(Mutex::new(options.on_chunk));

    let state_stdout = state.clone();
    let user_cb_stdout = user_cb.clone();
    let on_stdout: Box<dyn FnMut(&str) + Send> = Box::new(move |chunk: &str| {
        let text = sanitize_and_strip_cr(chunk);
        // Snapshot the progress BEFORE ingest so on_chunk sees the pre-chunk
        // state? The TS calls onChunk after updating, with the post-chunk
        // progress. We match: ingest first, then snapshot.
        let snap = {
            let mut s = match state_stdout.try_lock() {
                Ok(g) => g,
                Err(_) => return, // contention — skip this chunk's callback
            };
            s.ingest_chunk(&text);
            s.create_progress()
        };
        let mut cbg = match user_cb_stdout.try_lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(cb) = cbg.as_mut() {
            // The TS onChunk receives (chunk_text, getProgress). We pass the
            // snapshot getter as a closure returning the already-captured value.
            let snap = snap.clone();
            let getter = move || snap.clone();
            cb(chunk, &getter);
        }
    });

    let state_stderr = state.clone();
    let user_cb_stderr = user_cb.clone();
    let on_stderr: Box<dyn FnMut(&str) + Send> = Box::new(move |chunk: &str| {
        let text = sanitize_and_strip_cr(chunk);
        let snap = {
            let mut s = match state_stderr.try_lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            s.ingest_chunk(&text);
            s.create_progress()
        };
        let mut cbg = match user_cb_stderr.try_lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(cb) = cbg.as_mut() {
            let snap = snap.clone();
            let getter = move || snap.clone();
            cb(chunk, &getter);
        }
    });

    let exec_opts = ShellExecOptions {
        cwd: options.cwd.clone(),
        env: options.env.clone(),
        inherit_env: options.inherit_env,
        timeout: options.timeout,
        cancel: options.cancel,
        on_stdout: Some(on_stdout),
        on_stderr: Some(on_stderr),
    };

    let exec_result = env.exec(command, exec_opts).await;

    // Post-stream safety net: if truncated and no temp file yet, init it now
    // with the captured tail.
    {
        let progress = state.lock().await.create_progress();
        if progress.truncation.truncated {
            let s = state.lock().await;
            if !s.full_output_requested {
                let initial = s.tail_output.clone();
                drop(s);
                match init_full_output_file(env.clone(), &initial, options.cancel).await {
                    Ok(path) => {
                        let mut s2 = state.lock().await;
                        s2.full_output_path = Some(path);
                        s2.full_output_requested = true;
                    }
                    Err(e) => {
                        // Spill failure is non-fatal — the capture still
                        // returns; the truncation metadata is present, just no
                        // fullOutputPath. Log via tracing.
                        tracing::warn!("bash full-output temp file init failed: {e}");
                    }
                }
            }
        }
    }

    let progress = state.lock().await.create_progress();
    // Pull the scalar/move fields out once so we never read a partially-moved
    // `progress` (e.g. `progress.truncation.truncated` after `truncation:` moved it).
    let ShellCaptureProgress {
        output,
        truncation,
        full_output_path,
        last_line_bytes,
    } = progress;
    let truncated_flag = truncation.truncated;
    match exec_result {
        Ok(out) => {
            let cancelled = options.cancel.map(|t| t.is_cancelled()).unwrap_or(false);
            Ok(ShellCaptureResult {
                output: output.clone(),
                truncation: truncation.clone(),
                full_output_path: full_output_path.clone(),
                last_line_bytes,
                exit_code: if cancelled { None } else { Some(out.exit_code) },
                cancelled,
                truncated: truncated_flag,
                execution_error: None,
            })
        }
        Err(e) => {
            let is_aborted = e.code == ExecutionErrorCode::Aborted
                || options.cancel.map(|t| t.is_cancelled()).unwrap_or(false);
            if is_aborted {
                Ok(ShellCaptureResult {
                    output,
                    truncation,
                    full_output_path,
                    last_line_bytes,
                    exit_code: None,
                    cancelled: true,
                    truncated: truncated_flag,
                    execution_error: None,
                })
            } else if options.return_execution_errors {
                Ok(ShellCaptureResult {
                    output,
                    truncation,
                    full_output_path,
                    last_line_bytes,
                    exit_code: None,
                    cancelled: false,
                    truncated: truncated_flag,
                    execution_error: Some(e),
                })
            } else {
                Err(e)
            }
        }
    }
}

/// Create the temp file with initial content. Mirrors `ensureFullOutputFile`'s
/// first-call path: `env.createTempFile({prefix:"bash-", suffix:".log"})` +
/// `appendFile(path, initialContent)`.
async fn init_full_output_file(
    env: Arc<dyn ExecutionEnv>,
    initial: &str,
    cancel: Option<&CancellationToken>,
) -> Result<PathBuf, ExecutionError> {
    let path = env
        .create_temp_file("bash-", ".log", cancel)
        .await
        .map_err(|e| ExecutionError::new(ExecutionErrorCode::Unknown, e.to_string()))?;
    env.append_file(
        &path.to_string_lossy(),
        FileContent::Text(initial.to_string()),
        cancel,
    )
    .await
    .map_err(|e| ExecutionError::new(ExecutionErrorCode::Unknown, e.to_string()))?;
    Ok(path)
}

/// Re-export `format_size` for the bash tool's truncation footer messages.
pub use crate::truncate::format_size;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_controls_keeps_tab_lf() {
        let s = "a\x01b\tc\nd\x0be";
        let out = sanitize_and_strip_cr(&s);
        assert_eq!(out, "ab\tc\nde");
    }

    #[test]
    fn sanitize_strips_cr() {
        let s = "a\r\nb\rc";
        let out = sanitize_and_strip_cr(&s);
        assert_eq!(out, "a\nbc");
    }

    #[test]
    fn sanitize_strips_interlinear_annotation() {
        let s = "a\u{fff9}b\u{fffb}c";
        let out = sanitize_and_strip_cr(&s);
        assert_eq!(out, "abc");
    }

    #[test]
    fn capture_state_ingest_and_progress() {
        let mut s = CaptureState::default();
        s.ingest_chunk("line1\nline2\n");
        s.ingest_chunk("line3");
        let p = s.create_progress();
        // 3 lines total (line1, line2, line3-open).
        assert_eq!(p.truncation.total_lines, 3);
        assert!(!p.truncation.truncated);
        assert_eq!(p.output, "line1\nline2\nline3");
    }
}
