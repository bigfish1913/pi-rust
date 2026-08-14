//! Mirrors `packages/agent/src/harness` (tools + env + session + compaction).
//!
//! M4 scope: `ExecutionEnv` trait + `OsExecutionEnv` + `InMemoryExecutionEnv` +
//! file-mutation-queue + read/write/edit/bash tools + edit-diff + utils
//! (truncate, shell-output, image, path-utils). Session/compaction arrive in M5
//! (these live in `pi-harness`, not here).
//!
//! Workspace dep chain: `pi-telemetry → pi-ai → pi-agent → pi-tools →
//! pi-harness → pi-cli`. `pi-tools` depends on `pi-agent` (for the `AgentTool`
//! trait) and `pi-ai` (for `Tool`/content types).

pub mod env;
pub mod error;
pub mod file_mutation_queue;
pub mod image;
pub mod in_memory;
pub mod os_env;
pub mod path_utils;
pub mod shell_output;
pub mod truncate;

pub mod tools;
pub use tools::{
    create_bash_tool, create_edit_tool, create_find_tool, create_grep_tool, create_ls_tool,
    create_read_tool, create_write_tool, BashToolOptions, ExecutionToolContext, ReadToolOptions,
};

pub use env::{
    ExecutionEnv, FileContent, FileKind, FileInfo, FileSystem, Shell, ShellExecOptions,
    ShellOutput,
};
pub use error::{
    ExecutionError, ExecutionErrorCode, FileError, FileErrorCode, io_to_file_code,
    io_to_file_error,
};
pub use file_mutation_queue::{
    get_mutation_queue_key, with_file_mutation_queue, MutationQueueRegistry, MutatingEnv,
};
pub use image::{detect_supported_image_mime_type, encode_base64};
pub use in_memory::{InMemoryExecutionEnv, ShellScript};
pub use os_env::OsExecutionEnv;
pub use path_utils::{normalize_tool_path, resolve_read_tool_path, resolve_tool_path};
pub use truncate::{
    format_size, split_lines_for_counting, truncate_head, truncate_line, truncate_tail,
    TruncationLimit, TruncateLineOutput, TruncationOptions, TruncationResult,
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, GREP_MAX_LINE_LENGTH,
};
