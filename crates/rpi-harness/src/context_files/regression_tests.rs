//! Context discovery regressions. Git layouts are synthetic and offline;
//! real filesystem fixtures are only needed for symlink semantics.

use super::*;
use rpi_tools::env::FileSystem;
use rpi_tools::in_memory::InMemoryExecutionEnv;

const CWD: &str = "/scope/repo/worktrees/feature/src";
const AGENT: &str = "/scope/agent";

async fn seed(env: &InMemoryExecutionEnv, path: &str, content: &str) {
    let parent = Path::new(path).parent().unwrap();
    env.create_dir(&parent.to_string_lossy(), true, None)
        .await
        .unwrap();
    env.seed_file(path, content.as_bytes().to_vec()).await;
}

async fn nested_worktree() -> Arc<dyn ExecutionEnv> {
    let env = InMemoryExecutionEnv::with_cwd(PathBuf::from(CWD));
    for (path, content) in [
        ("/scope/agent/AGENTS.md", "GLOBAL"),
        ("/scope/AGENTS.md", "OUTER"),
        ("/scope/repo/AGENTS.md", "MAIN"),
        ("/scope/repo/.git/config", "[core]"),
        (
            "/scope/repo/.git/worktrees/feature/HEAD",
            "ref: refs/heads/feature\n",
        ),
        ("/scope/repo/.git/worktrees/feature/commondir", "../..\n"),
        (
            "/scope/repo/worktrees/feature/.git",
            "gitdir: ../../.git/worktrees/feature\n",
        ),
        ("/scope/repo/worktrees/AGENTS.md", "INTERMEDIATE"),
        ("/scope/repo/worktrees/feature/AGENTS.md", "FEATURE"),
        ("/scope/repo/worktrees/feature/src/AGENTS.md", "SUB"),
    ] {
        seed(&env, path, content).await;
    }
    Arc::new(env)
}

async fn contents(env: &Arc<dyn ExecutionEnv>, cwd: &str) -> Vec<String> {
    load_project_context_files(env, Path::new(cwd), Path::new(AGENT))
        .await
        .into_iter()
        .map(|file| file.content)
        .collect()
}

#[tokio::test]
async fn nested_worktree_shadows_only_main_same_named_file() {
    let env = nested_worktree().await;
    assert_eq!(
        contents(&env, CWD).await,
        ["GLOBAL", "OUTER", "INTERMEDIATE", "FEATURE", "SUB"]
    );
}

#[tokio::test]
async fn nested_worktree_without_own_context_keeps_main_instructions() {
    let env = nested_worktree().await;
    env.remove(
        "/scope/repo/worktrees/feature/AGENTS.md",
        false,
        false,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        contents(&env, CWD).await,
        ["GLOBAL", "OUTER", "MAIN", "INTERMEDIATE", "SUB"]
    );
}

#[tokio::test]
async fn different_candidate_names_do_not_shadow_each_other() {
    let env = nested_worktree().await;
    env.rename_file(
        "/scope/repo/worktrees/feature/AGENTS.md",
        "/scope/repo/worktrees/feature/CLAUDE.md",
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        contents(&env, CWD).await,
        ["GLOBAL", "OUTER", "MAIN", "INTERMEDIATE", "FEATURE", "SUB"]
    );
}

#[tokio::test]
async fn override_priority_is_preserved_at_worktree_root() {
    let env = nested_worktree().await;
    env.write_file(
        "/scope/repo/worktrees/feature/AGENTS.override.md",
        rpi_tools::env::FileContent::Text("OVERRIDE".into()),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        contents(&env, CWD).await,
        ["GLOBAL", "OUTER", "MAIN", "INTERMEDIATE", "OVERRIDE", "SUB"]
    );
}

#[tokio::test]
async fn normal_repository_keeps_ancestor_inheritance() {
    let env = nested_worktree().await;
    env.remove("/scope/repo/worktrees/feature/.git", false, false, None)
        .await
        .unwrap();
    assert_eq!(
        contents(&env, CWD).await,
        ["GLOBAL", "OUTER", "MAIN", "INTERMEDIATE", "FEATURE", "SUB"]
    );
}

#[tokio::test]
async fn malformed_git_metadata_does_not_suppress_context() {
    for metadata in ["", "not a gitdir", "gitdir: ", "gitdir: missing"] {
        let env = nested_worktree().await;
        env.write_file(
            "/scope/repo/worktrees/feature/.git",
            rpi_tools::env::FileContent::Text(metadata.into()),
            None,
        )
        .await
        .unwrap();
        assert!(contents(&env, CWD)
            .await
            .iter()
            .any(|content| content == "MAIN"));
    }
}

#[tokio::test]
async fn missing_worktree_head_keeps_main_instructions() {
    let env = nested_worktree().await;
    env.remove(
        "/scope/repo/.git/worktrees/feature/HEAD",
        false,
        false,
        None,
    )
    .await
    .unwrap();
    assert!(find_shadowed_context_file(&env, Path::new(CWD))
        .await
        .is_none());
    assert_eq!(
        contents(&env, CWD).await,
        ["GLOBAL", "OUTER", "MAIN", "INTERMEDIATE", "FEATURE", "SUB"]
    );
}

#[tokio::test]
async fn submodule_gitdir_without_commondir_keeps_context() {
    let env = nested_worktree().await;
    env.remove(
        "/scope/repo/.git/worktrees/feature/commondir",
        false,
        false,
        None,
    )
    .await
    .unwrap();
    assert!(find_shadowed_context_file(&env, Path::new(CWD))
        .await
        .is_none());
    assert!(contents(&env, CWD)
        .await
        .iter()
        .any(|content| content == "MAIN"));
}

#[tokio::test]
async fn sibling_worktree_does_not_shadow_ancestors() {
    let concrete = InMemoryExecutionEnv::with_cwd(PathBuf::from("/scope/sibling"));
    for (path, content) in [
        ("/scope/repo/.git/config", "[core]"),
        (
            "/scope/repo/.git/worktrees/feature/HEAD",
            "ref: refs/heads/feature",
        ),
        ("/scope/repo/.git/worktrees/feature/commondir", "../.."),
        (
            "/scope/sibling/.git",
            "gitdir: ../repo/.git/worktrees/feature",
        ),
        ("/scope/sibling/AGENTS.md", "SIBLING"),
        ("/scope/AGENTS.md", "OUTER"),
    ] {
        seed(&concrete, path, content).await;
    }
    let env: Arc<dyn ExecutionEnv> = Arc::new(concrete);
    assert!(
        find_shadowed_context_file(&env, Path::new("/scope/sibling"))
            .await
            .is_none()
    );
    assert_eq!(contents(&env, "/scope/sibling").await, ["OUTER", "SIBLING"]);
}

#[tokio::test]
async fn bare_repository_layout_does_not_hide_outer_instructions() {
    let concrete = InMemoryExecutionEnv::with_cwd(PathBuf::from("/scope/main"));
    for (path, content) in [
        ("/scope/.bare/worktrees/main/HEAD", "ref: refs/heads/main"),
        ("/scope/.bare/worktrees/main/commondir", "../.."),
        ("/scope/main/.git", "gitdir: ../.bare/worktrees/main"),
        ("/scope/AGENTS.md", "OUTER"),
        ("/scope/main/AGENTS.md", "MAIN"),
    ] {
        seed(&concrete, path, content).await;
    }
    let env: Arc<dyn ExecutionEnv> = Arc::new(concrete);
    assert!(find_shadowed_context_file(&env, Path::new("/scope/main"))
        .await
        .is_none());
    assert_eq!(contents(&env, "/scope/main").await, ["OUTER", "MAIN"]);
}

#[tokio::test]
async fn absolute_git_metadata_and_crlf_are_supported() {
    let env = nested_worktree().await;
    for (path, content) in [
        (
            "/scope/repo/worktrees/feature/.git",
            "gitdir: /scope/repo/.git/worktrees/feature\r\n",
        ),
        (
            "/scope/repo/.git/worktrees/feature/commondir",
            "/scope/repo/.git\r\n",
        ),
    ] {
        env.write_file(
            path,
            rpi_tools::env::FileContent::Text(content.into()),
            None,
        )
        .await
        .unwrap();
    }
    assert_eq!(
        contents(&env, CWD).await,
        ["GLOBAL", "OUTER", "INTERMEDIATE", "FEATURE", "SUB"]
    );
}

#[tokio::test]
async fn shadowing_does_not_remove_explicit_global_context() {
    let env = nested_worktree().await;
    let files = load_project_context_files(&env, Path::new(CWD), Path::new("/scope/repo")).await;
    assert_eq!(
        files
            .iter()
            .map(|file| file.content.as_str())
            .collect::<Vec<_>>(),
        ["MAIN", "OUTER", "INTERMEDIATE", "FEATURE", "SUB"]
    );
}

#[tokio::test]
async fn relative_paths_and_parent_components_are_resolved_before_walking() {
    let env = nested_worktree().await;
    let files = load_project_context_files(
        &env,
        Path::new("../src/../src"),
        Path::new("../../../../agent"),
    )
    .await;
    assert_eq!(
        files
            .iter()
            .map(|file| file.content.as_str())
            .collect::<Vec<_>>(),
        ["GLOBAL", "OUTER", "INTERMEDIATE", "FEATURE", "SUB"]
    );
    assert!(files.iter().all(|file| !file
        .path
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))));
}

#[tokio::test]
async fn directory_candidate_falls_back_to_regular_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("AGENTS.md")).unwrap();
    std::fs::write(dir.path().join("CLAUDE.md"), "CLAUDE").unwrap();
    let env: Arc<dyn ExecutionEnv> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
        dir.path().to_path_buf(),
    ));
    let file = load_context_file_from_dir(&env, dir.path()).await.unwrap();
    assert_eq!(file.content, "CLAUDE");
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_candidate_preserves_priority_and_addressed_path() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("instructions.md"), "LINKED").unwrap();
    std::fs::write(dir.path().join("CLAUDE.md"), "CLAUDE").unwrap();
    let link = dir.path().join("AGENTS.md");
    std::os::unix::fs::symlink("instructions.md", &link).unwrap();
    let env: Arc<dyn ExecutionEnv> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
        dir.path().to_path_buf(),
    ));
    let file = load_context_file_from_dir(&env, dir.path()).await.unwrap();
    assert_eq!(file.path, link);
    assert_eq!(file.content, "LINKED");
}

#[cfg(unix)]
#[tokio::test]
async fn dangling_directory_and_cyclic_symlinks_fall_back() {
    for target in ["missing", "directory", "AGENTS.md"] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("directory")).unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "CLAUDE").unwrap();
        std::os::unix::fs::symlink(target, dir.path().join("AGENTS.md")).unwrap();
        let env: Arc<dyn ExecutionEnv> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
            dir.path().to_path_buf(),
        ));
        let file = load_context_file_from_dir(&env, dir.path()).await.unwrap();
        assert_eq!(file.content, "CLAUDE");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_worktree_context_survives_main_shadowing_and_symlinked_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("repo");
    let worktree = main.join("feature");
    std::fs::create_dir_all(main.join(".git/worktrees/feature")).unwrap();
    std::fs::create_dir_all(worktree.join("src")).unwrap();
    std::fs::write(main.join("AGENTS.md"), "SHARED").unwrap();
    std::fs::write(
        main.join(".git/worktrees/feature/HEAD"),
        "ref: refs/heads/feature\n",
    )
    .unwrap();
    std::fs::write(main.join(".git/worktrees/feature/commondir"), "../..\n").unwrap();
    std::fs::write(worktree.join(".git"), "gitdir: ../.git/worktrees/feature\n").unwrap();
    std::os::unix::fs::symlink("../AGENTS.md", worktree.join("AGENTS.md")).unwrap();
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink(&main, &alias).unwrap();
    let cwd = alias.join("feature/src");
    let env: Arc<dyn ExecutionEnv> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(cwd.clone()));
    let files = load_project_context_files(&env, &cwd, &dir.path().join("agent")).await;
    let shared = files
        .iter()
        .filter(|file| file.content == "SHARED")
        .collect::<Vec<_>>();
    assert_eq!(shared.len(), 1);
    assert_eq!(shared[0].path, alias.join("feature/AGENTS.md"));
}
