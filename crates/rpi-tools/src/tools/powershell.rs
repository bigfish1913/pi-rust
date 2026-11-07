//! Optional Windows PowerShell tool, matching upstream's `powershell` tool.
//! The command is passed through `-EncodedCommand` so quotes and newlines in
//! user input cannot be reinterpreted by the outer Bash transport.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, TextContentOrImage, ToolResultPartial};
use rpi_ai::types::Tool;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::env::{FileContent, ShellOverride};
use crate::shell_output::{execute_shell_with_capture, ShellCaptureOptions};
use crate::tools::tool_context::ExecutionToolContext;

#[derive(Debug, Clone, JsonSchema, Deserialize)]
pub struct PowerShellInput {
    pub command: String,
    #[serde(default)]
    pub timeout: Option<f64>,
}

pub struct PowerShellTool {
    schema: Tool,
    env: Arc<dyn crate::env::ExecutionEnv>,
}

impl PowerShellTool {
    fn schema() -> Tool {
        Tool {
            name: "powershell".into(),
            description: "Execute a PowerShell command on Windows. Returns stdout and stderr. Prefer bash for file/text work; use this only for PowerShell object operations (registry, COM/WMI, scheduled tasks, .NET)."
                .into(),
            parameters: rpi_ai::types::Schema::new(
                serde_json::to_value(schemars::schema_for!(PowerShellInput)).unwrap_or_default(),
            ),
            constrained_sampling: None,
        }
    }
}

pub fn create_powershell_tool(context: &ExecutionToolContext) -> Arc<dyn AgentTool> {
    Arc::new(PowerShellTool {
        schema: PowerShellTool::schema(),
        env: context.env().clone(),
    })
}

/// Resolve the PowerShell executable: prefer `pwsh` on PATH, then the fixed
/// `%ProgramFiles%\PowerShell\7\pwsh.exe` install location, then the Windows
/// PowerShell 5.1 fallback.
///
/// The fixed-path-only lookup (before this change) missed scoop/winget/user
/// installs that put pwsh 7 on PATH but not in `%ProgramFiles%`, so every call
/// silently ran 5.1: CLIXML/progress-stream noise, ANSI-GBK output and
/// `NativeCommandError` wrapping. Pure function so the resolution order is
/// testable on every host OS.
fn resolve_powershell_program(program_files: Option<&str>, path: Option<&str>) -> PathBuf {
    if let Some(path) = path {
        for dir in std::env::split_paths(path) {
            let candidate = dir.join("pwsh.exe");
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    if let Some(program_files) = program_files {
        let candidate = PathBuf::from(program_files).join("PowerShell/7/pwsh.exe");
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from("powershell.exe")
}

/// Wrapper sent via `-EncodedCommand`: set the progress preference, then run
/// the staged script file in the caller's scope. The user's script is never
/// concatenated into this text, so `using`/`param` and every other
/// first-statement construct keep their original positions.
fn build_wrapper_command(script_path: &str) -> String {
    let escaped = script_path.replace('\'', "\u{27}\u{27}");
    format!("$ProgressPreference='SilentlyContinue'; . '{}'", escaped)
}

/// Stage the caller's script verbatim. The UTF-8 BOM makes Windows PowerShell
/// 5.1 decode the file as UTF-8 instead of the legacy ANSI codepage.
fn build_staged_script(command: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(command.len() + 3);
    bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    bytes.extend_from_slice(command.as_bytes());
    bytes
}

#[async_trait]
impl AgentTool for PowerShellTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "powershell"
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        if !cfg!(windows) {
            return Err(AgentError::Tool(
                "PowerShell tool is only available on Windows".into(),
            ));
        }
        let input: PowerShellInput = serde_json::from_value(params)
            .map_err(|e| AgentError::Validation(format!("powershell input invalid: {e}")))?;
        // Stage the caller's script verbatim in its own file and run it from a
        // tiny wrapper. Prepending the preference directly would put an
        // executable statement before constructs that must be first in their
        // script (`using`, `param`), a ParserError regression the reviewer
        // demonstrated with real `-EncodedCommand` invocations. With the file
        // indirection the wrapped command is always the same shape, and the
        // caller's bytes reach PowerShell untouched (UTF-8 BOM for 5.1).
        let staged_path = self
            .env
            .create_temp_file("rpi-ps-", ".ps1", Some(&signal))
            .await
            .map_err(|e| AgentError::Tool(format!("create staged PowerShell script: {e}")))?;
        self.env
            .write_file(
                &staged_path.to_string_lossy(),
                FileContent::Bytes(build_staged_script(&input.command)),
                Some(&signal),
            )
            .await
            .map_err(|e| AgentError::Tool(format!("stage PowerShell script: {e}")))?;
        let script = build_wrapper_command(&staged_path.to_string_lossy());
        let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let encoded = STANDARD.encode(utf16);
        let command = encoded;
        let program = if cfg!(windows) {
            resolve_powershell_program(
                std::env::var("ProgramFiles").ok().as_deref(),
                std::env::var("PATH").ok().as_deref(),
            )
        } else {
            PathBuf::from("powershell.exe")
        };
        let staged_path_text = staged_path.to_string_lossy().into_owned();
        let capture = execute_shell_with_capture(
            &self.env,
            &command,
            ShellCaptureOptions {
                shell: Some(ShellOverride {
                    program,
                    args: vec![
                        "-NoLogo".into(),
                        "-NoProfile".into(),
                        "-NonInteractive".into(),
                        "-ExecutionPolicy".into(),
                        "Bypass".into(),
                        "-EncodedCommand".into(),
                    ],
                }),
                cwd: Some(self.env.cwd().to_path_buf()),
                env: None,
                inherit_env: true,
                timeout: input.timeout,
                cancel: Some(&signal),
                on_chunk: None,
                return_execution_errors: true,
            },
        )
        .await
        .map_err(|e| AgentError::Tool(e.to_string()))?;
        // The wrapper dot-sources this file synchronously; remove the staged
        // script (and its isolated temp dir) after the child exits so repeated
        // tool calls do not leave caller scripts behind in the OS temp dir.
        let _ = self
            .env
            .remove(&staged_path_text, false, true, Some(&signal))
            .await;
        if let Some(dir) = staged_path.parent() {
            let _ = self
                .env
                .remove(&dir.to_string_lossy(), false, true, Some(&signal))
                .await;
        }
        let mut output = capture.output;
        if capture.cancelled {
            return Err(AgentError::Tool("PowerShell command aborted".into()));
        }
        if let Some(error) = capture.execution_error {
            return Err(AgentError::Tool(error.to_string()));
        }
        if let Some(code) = capture.exit_code.filter(|code| *code != 0) {
            output.push_str(&format!("\n\nPowerShell exited with code {code}"));
            return Err(AgentError::Tool(output));
        }
        if output.trim().is_empty() {
            output = "(no output)".into();
        }
        Ok(AgentToolResult {
            content: vec![TextContentOrImage::text(output)],
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &std::path::Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let file = dir.join(name);
        std::fs::write(&file, b"").unwrap();
        file
    }

    /// Regression for fork issue #14 PR-A: a PATH-installed pwsh 7 (scoop,
    /// winget, user install) wins over the fixed ProgramFiles location and the
    /// 5.1 fallback. Before this change only ProgramFiles was consulted.
    #[test]
    fn review_issue14_path_pwsh_wins_over_fixed_location_and_51_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let on_path = tmp.path().join("scoop").join("shims");
        let pwsh = touch(&on_path, "pwsh.exe");
        let program_files = tmp.path().join("pf");
        let fixed = touch(&program_files.join("PowerShell").join("7"), "pwsh.exe");
        let path = on_path.to_string_lossy().into_owned();
        let pf = program_files.to_string_lossy().into_owned();
        assert_eq!(resolve_powershell_program(Some(&pf), Some(&path)), pwsh);
        // Without a PATH hit the fixed install still beats 5.1.
        assert_eq!(resolve_powershell_program(Some(&pf), None), fixed);
        // An empty PATH still falls through to the fixed install when present.
        assert_eq!(resolve_powershell_program(Some(&pf), Some("")), fixed);
        // With neither a PATH hit nor a fixed install, 5.1 remains
        // (resolved by CreateProcess). A stale ProgramFiles path must not be
        // treated as a hit.
        let missing_pf = tmp.path().join("no-such-program-files");
        assert_eq!(
            resolve_powershell_program(Some(&missing_pf.to_string_lossy()), None),
            PathBuf::from("powershell.exe")
        );
        assert_eq!(
            resolve_powershell_program(None, None),
            PathBuf::from("powershell.exe")
        );
    }

    /// A directory that merely exists but has no pwsh.exe must not be chosen.
    #[test]
    fn review_issue14_empty_path_entries_do_not_shadow_powershell() {
        let tmp = tempfile::tempdir().unwrap();
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let path = empty.to_string_lossy().into_owned();
        assert_eq!(
            resolve_powershell_program(None, Some(&path)),
            PathBuf::from("powershell.exe")
        );
    }
}

#[cfg(test)]
mod review_probes {
    use super::*;

    /// The reviewer's P2 cases: both scripts are valid standalone PowerShell,
    /// and both MUST reach pwsh byte-identical (no prefix glued in front).
    /// `using` must be the first statement; `param` must be the first
    /// executable token in a script block. A `& script-file` wrapper keeps the
    /// original script in its own file, so its top-level syntax is preserved.
    const REVIEW_USING_SCRIPT: &str =
        "using namespace System.Text\n[StringBuilder]::new().GetType().Name\n";
    const REVIEW_PARAM_SCRIPT: &str = "param([string]$Name='ok')\nWrite-Output $Name\n";

    fn wrapper_for(script_path: &str) -> String {
        build_wrapper_command(script_path)
    }

    #[test]
    fn review270_wrapper_never_touches_the_user_script_text() {
        for script in [REVIEW_USING_SCRIPT, REVIEW_PARAM_SCRIPT] {
            let wrapper = wrapper_for("C:\\temp\\x.ps1");
            assert!(
                !wrapper.contains(script.trim_end()),
                "the user script must not be inlined into the wrapper: {wrapper}"
            );
        }
    }

    #[test]
    fn review270_wrapper_sets_progress_preference_then_dot_sources_the_file() {
        let wrapper = wrapper_for("C:\\temp\\x.ps1");
        assert!(
            wrapper.starts_with("$ProgressPreference='SilentlyContinue';"),
            "the preference must still be set first: {wrapper}"
        );
        assert!(
            wrapper.contains(". 'C:\\temp\\x.ps1'"),
            "the wrapper must dot-source the staged script: {wrapper}"
        );
    }

    #[test]
    fn review270_wrapper_quotes_the_path_for_the_single_quote_edge() {
        // A temp path containing a single quote must still parse: PS escapes
        // it by doubling inside a single-quoted string.
        let wrapper = wrapper_for("C:\\temp\\it's\\x.ps1");
        assert!(
            wrapper.contains(". 'C:\\temp\\it''s\\x.ps1'"),
            "single quotes in the staged path must be doubled: {wrapper}"
        );
    }

    #[test]
    fn review270_user_script_is_written_verbatim_with_utf8_bom() {
        // The staged file carries the caller's bytes 1:1 plus a UTF-8 BOM so
        // Windows PowerShell 5.1 reads non-ASCII correctly without BOM sniffing.
        let mut staged = build_staged_script(REVIEW_USING_SCRIPT);
        assert!(staged.starts_with(b"\xEF\xBB\xBF"), "BOM required for 5.1");
        assert_eq!(&staged[3..], REVIEW_USING_SCRIPT.as_bytes());
        staged = build_staged_script(REVIEW_PARAM_SCRIPT);
        assert_eq!(&staged[3..], REVIEW_PARAM_SCRIPT.as_bytes());
    }

    #[test]
    fn review270_encoded_wrapper_decodes_to_the_expected_wrapper() {
        let wrapper = wrapper_for("C:\\temp\\x.ps1");
        let utf16: Vec<u8> = wrapper.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let encoded = STANDARD.encode(utf16);
        let bytes = STANDARD.decode(&encoded).unwrap();
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(String::from_utf16(&units).unwrap(), wrapper);
    }

    /// The exact two scripts from the review, plus the exact old prefix, are
    /// kept as a permanent offline regression input. This probe models the
    /// whole command chain: the encoded command contains only the wrapper, and
    /// the staged file contains only the caller's bytes. A real PowerShell
    /// parser check is not possible on this Linux host (declared in the PR
    /// reply); this establishes the syntactic precondition instead.
    #[test]
    fn review270_encoded_command_carries_no_statement_before_using_or_param() {
        let old_prefix = "$ProgressPreference='SilentlyContinue'; ";
        for script in [REVIEW_USING_SCRIPT, REVIEW_PARAM_SCRIPT] {
            let staged = build_staged_script(script);
            let wrapper = wrapper_for("C:\\temp\\rpi-ps-x.ps1");
            let encoded = STANDARD.encode(
                wrapper
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<u8>>(),
            );
            let decoded = STANDARD.decode(&encoded).unwrap();
            let units: Vec<u16> = decoded
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            let wire = String::from_utf16(&units).unwrap();
            assert!(
                !wire.contains(script.trim_end()),
                "the caller's script must not be inlined into -EncodedCommand: {wire}"
            );
            assert!(
                !wire.contains(&format!("{old_prefix}{}", script.trim_end())),
                "the old prefix+script concatenation must be gone: {wire}"
            );
            assert_eq!(&staged[3..], script.as_bytes());
        }
    }
}
