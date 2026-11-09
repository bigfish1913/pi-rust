//! `rpi session gc` — offline compaction of retired stream frames.
//!
//! Fork issue DavidHinton-xcpc/pi-rust#14 §4 PR-D (write-side half). A long
//! session's JSONL is dominated by `assistant_frame` appends that a later
//! `clear_run` of the same run already retired; the reader no longer indexes
//! them (#252), but the bytes stay on disk.
//!
//! This command strips **only** those frames' payloads; their identity rows
//! stay on disk so ID reservations and later appends stay consistent. Runs
//! without a `clear_run` keep
//! every frame, because `frame_progress::salvage_run_frames` replays them after
//! a crash. Sequences are never renumbered and the header is kept, so later
//! appends and older binaries both stay consistent.
//!
//! `--dry-run` reports without writing.

use std::path::{Path, PathBuf};

use crate::app::{EXIT_RUNTIME, EXIT_USAGE};

/// Dispatch `rpi session <subcommand>`.
pub async fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("gc") => {
            let rest = &args[1..];
            if rest.iter().any(|arg| arg == "--help" || arg == "-h") {
                print_gc_help();
                return 0;
            }
            gc(rest).await
        }
        Some("--help" | "-h") | None => {
            print_help();
            0
        }
        Some(other) => {
            eprintln!("error: unknown `session` subcommand `{other}`");
            print_help();
            EXIT_USAGE
        }
    }
}

fn print_help() {
    println!("Usage: rpi session <command>");
    println!();
    println!("Commands:");
    println!("  gc [path...] [--dry-run]  Drop payloads of frames a clear_run retired");
    println!();
    println!("`gc` without a path uses the session directory for the current project.");
}

fn print_gc_help() {
    println!("Usage: rpi session gc [path...] [--dry-run]");
    println!();
    println!("Arguments:");
    println!("  <path>      A session .jsonl file, or a directory to scan");
    println!("  --dry-run   Report what would be dropped without writing");
    println!();
    println!("Only the payloads of `assistant_frame` appends that precede a later clear_run of");
    println!("the same run are removed; their identity rows stay reserved. Unclear runs keep");
    println!("every frame (crash salvage).");
}

async fn gc(args: &[String]) -> i32 {
    let dry_run = args.iter().any(|arg| arg == "--dry-run");
    let targets: Vec<&String> = args.iter().filter(|arg| !arg.starts_with("--")).collect();
    let files = match collect_targets(&targets) {
        Ok(files) => files,
        Err(error) => {
            eprintln!("error: {error}");
            return EXIT_RUNTIME;
        }
    };
    if files.is_empty() {
        eprintln!("error: no session files found");
        return EXIT_RUNTIME;
    }

    let fs: std::sync::Arc<dyn rpi_tools::FileSystem> =
        std::sync::Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        ));
    let mut total_retired = 0usize;
    let mut total_before = 0usize;
    let mut total_after = 0usize;
    let mut failures = 0usize;
    for file in &files {
        // A rewrite takes the session's OS file lock first (the same
        // `<session>.lock` the running CLI claims): a live writer makes gc
        // refuse instead of racing it. A dry run reads without the lock.
        let _lock = if dry_run {
            None
        } else {
            match GcSessionLock::acquire(file) {
                Ok(lock) => Some(lock),
                Err(error) => {
                    failures += 1;
                    eprintln!("error: {}: {error}", file.display());
                    continue;
                }
            }
        };
        match rpi_harness::session::jsonl::compact_retired_frames(
            fs.clone(),
            &file.to_string_lossy(),
            dry_run,
        )
        .await
        {
            Ok(report) => {
                total_retired += report.retired_frames;
                total_before += report.bytes_before;
                total_after += report.bytes_after;
                let verb = if dry_run { "would drop" } else { "dropped" };
                if report.would_rewrite() {
                    println!(
                        "{}: {verb} {} retired frame row(s); {} -> {} bytes",
                        file.display(),
                        report.retired_frames,
                        report.bytes_before,
                        report.bytes_after
                    );
                } else {
                    println!("{}: nothing to drop", file.display());
                }
            }
            Err(error) => {
                failures += 1;
                eprintln!("error: {}: {error}", file.display());
            }
        }
    }
    // Summary in a stable, greppable shape.
    println!(
        "summary: files={} retired={} bytes={} -> {}{}",
        files.len(),
        total_retired,
        total_before,
        total_after,
        if dry_run { " (dry run)" } else { "" }
    );
    if failures > 0 {
        EXIT_RUNTIME
    } else {
        0
    }
}

/// Resolve arguments to session files. A directory is scanned one level deep
/// (matching the on-disk `<root>/--<cwd>--/<file>.jsonl` layout). No argument
/// means the current project's directory under the default session root.
fn collect_targets(targets: &[&String]) -> Result<Vec<PathBuf>, String> {
    if targets.is_empty() {
        let cwd = std::env::current_dir().map_err(|e| format!("could not read cwd: {e}"))?;
        let root = crate::session::default_session_dir(&cwd);
        let project = root.join(project_session_dir_name(&cwd));
        if !project.is_dir() {
            return Err(format!(
                "no session directory for this project at {}",
                project.display()
            ));
        }
        return Ok(jsonl_files_in(&project));
    }

    let mut files = Vec::new();
    let mut missing = Vec::new();
    for target in targets {
        let path = Path::new(target.as_str());
        if path.is_dir() {
            files.extend(jsonl_files_in(path));
        } else if path.is_file() {
            files.push(path.to_path_buf());
        } else {
            missing.push(target.to_string());
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "no such session file or directory: {}",
            missing.join(", ")
        ));
    }
    Ok(files)
}

/// Exclusive advisory lock over one session file for the duration of a gc
/// rewrite. Same `<session>.lock` layout as `session.rs`'s
/// `claim_session_lock`, but scoped: dropping the handle releases the lock, so
/// each compacted file owns it only while it is being rewritten.
struct GcSessionLock {
    /// Held for the lock's lifetime; `fs2` releases on close.
    _file: std::fs::File,
}

impl GcSessionLock {
    fn acquire(session_path: &Path) -> Result<GcSessionLock, String> {
        let canonical =
            std::fs::canonicalize(session_path).unwrap_or_else(|_| session_path.to_path_buf());
        let lock_path = PathBuf::from(format!("{}.lock", canonical.display()));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(&lock_path)
            .map_err(|e| format!("open session lock {}: {e}", lock_path.display()))?;
        if fs2::FileExt::try_lock_exclusive(&file).is_err() {
            return Err(format!(
                "session {} is already open in another rpi process",
                session_path.display()
            ));
        }
        Ok(GcSessionLock { _file: file })
    }
}

/// `--<cwd-with-leading-/or-\\-stripped-and-/\\-:-replaced-by-->--`, mirroring the
/// repo's `jsonlSessionDirectoryName`. Kept local because the harness export
/// lands in a separate PR; the shape is asserted by the repo's own unit test.
fn project_session_dir_name(cwd: &Path) -> String {
    let text = cwd.to_string_lossy();
    let stripped = text
        .strip_prefix('/')
        .or_else(|| text.strip_prefix('\\'))
        .unwrap_or(&text);
    let replaced: String = stripped
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' => '-',
            other => other,
        })
        .collect();
    format!("--{replaced}--")
}

fn jsonl_files_in(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Ok(inner) = std::fs::read_dir(&path) {
                for child in inner.flatten() {
                    push_if_session(&mut files, child.path());
                }
            }
        } else {
            push_if_session(&mut files, path);
        }
    }
    files.sort();
    files
}

fn push_if_session(files: &mut Vec<PathBuf>, path: PathBuf) {
    let is_jsonl = path.extension().and_then(|e| e.to_str()) == Some("jsonl");
    let is_companion = path.to_string_lossy().ends_with(".progress.jsonl");
    if is_jsonl && !is_companion {
        files.push(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_session(dir: &Path, name: &str, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        let header = rpi_harness::session::jsonl::JsonlV4Header::new(
            "cli-gc".into(),
            1,
            "/cwd".into(),
            None,
            None,
            None,
        );
        std::fs::write(
            &path,
            format!(
                "{}{body}",
                rpi_harness::session::jsonl::encode_header(&header)
            ),
        )
        .unwrap();
        path
    }

    #[test]
    fn review_issue14_cli_scan_skips_progress_companions_and_finds_nested_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sessions");
        let project = root.join("--proj--");
        let nested = write_session(&project, "2026-01-01_a.jsonl", "");
        let companion = project.join("2026-01-01_a.jsonl.progress.jsonl");
        std::fs::write(&companion, "{}\n").unwrap();
        // A file two levels down is outside the documented layout.
        let deeper = write_session(&project.join("deep"), "b.jsonl", "");

        let files = jsonl_files_in(&root);
        assert!(files.contains(&nested), "{files:?}");
        assert!(!files.contains(&companion), "{files:?}");
        assert!(!files.contains(&deeper), "{files:?}");
        // Deterministic order so the CLI summary is stable.
        let mut sorted = files.clone();
        sorted.sort();
        assert_eq!(files, sorted);
    }

    #[test]
    fn review_issue14_cli_project_dir_name_matches_the_repo_encoding() {
        // Same cases as the harness's own `directory_name_encoding` test.
        assert_eq!(
            project_session_dir_name(Path::new("/home/user/proj")),
            "--home-user-proj--"
        );
        assert_eq!(project_session_dir_name(Path::new("/a/b:c")), "--a-b-c--");
    }

    #[tokio::test]
    async fn review_issue14_cli_dry_run_reports_without_writing() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("sessions");
        let mut body = String::new();
        for (seq, op) in [
            (1u64, rpi_harness::session::types::AssistantFrameOp::Append),
            (2, rpi_harness::session::types::AssistantFrameOp::ClearRun),
        ] {
            body.push_str(&rpi_harness::session::jsonl::encode_mutation(
                &rpi_harness::session::types::SessionMutation::Record {
                    record: rpi_harness::session::types::LaneRecord::AssistantFrame(
                        rpi_harness::session::types::AssistantFrameRecord {
                            base: rpi_harness::session::types::RecordBase {
                                id: format!("f{seq}"),
                                seq,
                                lane: "main".into(),
                                timestamp: seq as i64,
                            },
                            run_id: "r".into(),
                            stream_index: 0,
                            op,
                            frame: match op {
                                rpi_harness::session::types::AssistantFrameOp::Append => {
                                    Some(serde_json::json!({"text": format!("frame-{seq}")}))
                                }
                                _ => None,
                            },
                        },
                    ),
                },
            ));
        }
        let path = write_session(&dir, "one.jsonl", &body);
        let original = std::fs::read_to_string(&path).unwrap();
        let args = vec![path.to_string_lossy().into_owned(), "--dry-run".into()];
        assert_eq!(gc(&args).await, 0);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "dry run must not modify the file"
        );

        // A real run drops exactly the retired append's payload (identity row
        // stays, per the review #274 P2 contract).
        let args = vec![path.to_string_lossy().into_owned()];
        assert_eq!(gc(&args).await, 0);
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains("frame-1"), "{after}");
        assert!(after.contains("\"id\":\"f1\""), "{after}");
        assert!(after.contains("\"f2\""), "{after}");
    }

    #[tokio::test]
    async fn review_issue14_cli_reports_missing_paths() {
        let args = vec!["/definitely/not/a/session".to_string()];
        assert_eq!(gc(&args).await, EXIT_RUNTIME);
    }

    #[tokio::test]
    async fn review_issue14_cli_gc_refuses_a_file_another_writer_locks() {
        let temp = tempfile::tempdir().unwrap();
        let body = format!(
            "{}{}{}",
            rpi_harness::session::jsonl::encode_mutation(
                &rpi_harness::session::types::SessionMutation::Record {
                    record: rpi_harness::session::types::LaneRecord::AssistantFrame(
                        rpi_harness::session::types::AssistantFrameRecord {
                            base: rpi_harness::session::types::RecordBase {
                                id: "f1".into(),
                                seq: 1,
                                lane: "main".into(),
                                timestamp: 1,
                            },
                            run_id: "r".into(),
                            stream_index: 0,
                            op: rpi_harness::session::types::AssistantFrameOp::Append,
                            frame: Some(serde_json::json!({"text": "x"})),
                        },
                    ),
                }
            ),
            rpi_harness::session::jsonl::encode_mutation(
                &rpi_harness::session::types::SessionMutation::Record {
                    record: rpi_harness::session::types::LaneRecord::AssistantFrame(
                        rpi_harness::session::types::AssistantFrameRecord {
                            base: rpi_harness::session::types::RecordBase {
                                id: "f2".into(),
                                seq: 2,
                                lane: "main".into(),
                                timestamp: 2,
                            },
                            run_id: "r".into(),
                            stream_index: 0,
                            op: rpi_harness::session::types::AssistantFrameOp::ClearRun,
                            frame: None,
                        },
                    ),
                }
            ),
            rpi_harness::session::jsonl::encode_mutation(
                &rpi_harness::session::types::SessionMutation::Entry {
                    seq: 3,
                    timestamp: 3,
                    lane: Some("main".into()),
                    entry: rpi_harness::session::types::provisioned_into_entry(
                        rpi_harness::session::types::ProvisionedEntry {
                            id: "e3".into(),
                            kind: rpi_harness::session::types::ProvisionedKind::Message {
                                message: rpi_ai::UserMessage::new("kept", 3).into(),
                                terminate: None,
                            },
                        },
                        3,
                        None,
                        3,
                    ),
                }
            ),
        );
        let path = write_session(&temp.path().join("s"), "locked.jsonl", &body);
        let lock_path = path.with_file_name("locked.jsonl.lock");

        // A live writer holds the session lock: gc must refuse and leave the
        // file untouched.
        let writer = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        fs2::FileExt::try_lock_exclusive(&writer).unwrap();
        let args = vec![path.to_string_lossy().into_owned()];
        let original = std::fs::read_to_string(&path).unwrap();
        assert_eq!(gc(&args).await, EXIT_RUNTIME);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "a locked session must not be rewritten"
        );
        drop(writer);

        // Once the writer is gone the same gc run succeeds, and it releases
        // the lock afterwards (a following gc can acquire it again).
        assert_eq!(gc(&args).await, 0);
        let again = GcSessionLock::acquire(&path);
        assert!(again.is_ok(), "gc must release its lock when done");
    }
}
