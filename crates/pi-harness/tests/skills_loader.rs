//! M5e integration — skills loader over `InMemoryExecutionEnv`. Mirrors the
//! `loadSkills`/`loadSourcedSkills` cases in
//! `packages/agent/test/harness/skills.test.ts`:
//! - loads `SKILL.md` through recursive directory traversal;
//! - parses frontmatter (`name`/`description`/`disable-model-invocation`);
//! - drops a skill whose description is missing/empty with an
//!   `invalid_metadata` diagnostic;
//! - loads direct root `.md` children only from the root directory (nested
//!   `.md` files at depth > 0 are NOT loaded as skills);
//! - honors the two format paths (invariant §9): the system-prompt listing is
//!   XML-escaped, the invocation block is unescaped.
//!
//! The TS suite runs against `NodeExecutionEnv` + real tempdirs (incl. a
//! symlink case). The in-memory env does not model symlinks faithfully
//! (`canonical_path` is identity), so the symlink case is deferred to an
//! OS-env conformance test — see `docs/m5e-open-questions.md`.

use std::sync::Arc;

use rpi_harness::skills::{
    format_skill_invocation, format_skills_for_system_prompt, load_skills, load_sourced_skills,
    SkillDiagnosticCode, SourcedSkillInput,
};
use rpi_tools::in_memory::InMemoryExecutionEnv;
use rpi_tools::FileSystem;

/// Build an env, returning the typed handle (for seeding) and the dyn view (for
/// `load_skills`). The env is `Arc`-shared, so both point at the same state.
fn fresh_env() -> (Arc<InMemoryExecutionEnv>, Arc<dyn rpi_tools::ExecutionEnv>) {
    let typed = Arc::new(InMemoryExecutionEnv::new());
    let env: Arc<dyn rpi_tools::ExecutionEnv> = typed.clone();
    (typed, env)
}

#[tokio::test]
async fn loads_skill_md_with_frontmatter() {
    let (typed, env) = fresh_env();
    typed.create_dir(".agents/skills/example", true, None).await.unwrap();
    typed
        .write_file(
            ".agents/skills/example/SKILL.md",
            "---\nname: example\ndescription: Example skill\ndisable-model-invocation: true\n---\nUse this skill.\n".into(),
            None,
        )
        .await
        .unwrap();

    let result = load_skills(&env, &[".agents/skills".to_string()]).await;
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    assert_eq!(result.skills.len(), 1);
    let s = &result.skills[0];
    assert_eq!(s.name, "example");
    assert_eq!(s.description, "Example skill");
    assert_eq!(s.content, "Use this skill.");
    assert_eq!(s.disable_model_invocation, Some(true));
    // filePath is the resolved absolute path (cwd "/").
    assert!(s.file_path.ends_with(".agents/skills/example/SKILL.md"));
}

#[tokio::test]
async fn drops_skill_with_missing_description_and_emits_diagnostic() {
    let (typed, env) = fresh_env();
    typed.create_dir("user/broken", true, None).await.unwrap();
    typed
        .write_file("user/broken/SKILL.md", "---\nname: broken\n---\nMissing description.".into(), None)
        .await
        .unwrap();

    let result = load_skills(&env, &["user".to_string()]).await;
    assert!(result.skills.is_empty());
    assert_eq!(result.diagnostics.len(), 1, "{:?}", result.diagnostics);
    let d = &result.diagnostics[0];
    assert_eq!(d.code, SkillDiagnosticCode::InvalidMetadata);
    assert_eq!(d.message, "description is required");
    assert!(d.path.ends_with("user/broken/SKILL.md"));
}

#[tokio::test]
async fn loads_direct_markdown_children_only_from_root() {
    // Mirrors TS "loads direct markdown children only from the root directory":
    // a root `.md` file loads as a skill whose `name` = the parent (loaded) dir
    // basename when frontmatter omits `name`; nested `.md` files at depth > 0
    // are NOT loaded (includeRootFiles is false in recursion).
    let (typed, env) = fresh_env();
    typed.create_dir("skills/nested", true, None).await.unwrap();
    typed
        .write_file("skills/root.md", "---\ndescription: Root skill\n---\nRoot content".into(), None)
        .await
        .unwrap();
    typed
        .write_file(
            "skills/nested/ignored.md",
            "---\ndescription: Ignored\n---\nIgnored content".into(),
            None,
        )
        .await
        .unwrap();

    let result = load_skills(&env, &["skills".to_string()]).await;
    assert_eq!(result.skills.len(), 1, "{:?}", result.skills);
    // name = parent dir basename ("skills"); frontmatter has no `name`.
    assert_eq!(result.skills[0].name, "skills");
    assert_eq!(result.skills[0].content, "Root content");
}

#[tokio::test]
async fn missing_input_directory_is_silently_skipped() {
    let (_typed, env) = fresh_env();
    let result = load_skills(&env, &["does/not/exist".to_string()]).await;
    assert!(result.skills.is_empty());
    assert!(result.diagnostics.is_empty(), "not_found should be silent: {:?}", result.diagnostics);
}

#[tokio::test]
async fn listing_is_xml_escaped_and_invocation_is_not() {
    // Invariant §9: the two skill-format paths are distinct.
    use rpi_harness::types::Skill;
    let s = Skill {
        name: "a&b".to_string(),
        description: "Use <this> & that".to_string(),
        content: "Do the <thing> & stuff".to_string(),
        file_path: "/skills/a&b/SKILL.md".to_string(),
        disable_model_invocation: None,
    };

    // Path (1) — listing escapes all fields.
    let listing = format_skills_for_system_prompt(&[s.clone()]);
    assert!(listing.contains("<name>a&amp;b</name>"));
    assert!(listing.contains("<description>Use &lt;this&gt; &amp; that</description>"));
    assert!(listing.contains("<location>/skills/a&amp;b/SKILL.md</location>"));

    // Path (2) — invocation embeds content verbatim (unescaped).
    let inv = format_skill_invocation(&s, None);
    assert!(inv.contains("Do the <thing> & stuff"));
    assert!(inv.contains("<skill name=\"a&b\" location=\"/skills/a&b/SKILL.md\">"));
    assert!(inv.contains("References are relative to /skills/a&b."));
}

#[tokio::test]
async fn sourced_skills_preserve_source_and_attach_to_diagnostics() {
    // Mirrors TS "preserves source info for sourced skills" + "attaches source
    // info to diagnostics". One good skill + one broken skill under separate
    // sourced inputs.
    let (typed, env) = fresh_env();
    typed.create_dir("user/example", true, None).await.unwrap();
    typed
        .write_file(
            "user/example/SKILL.md",
            "---\nname: example\ndescription: Example skill\n---\nUse this skill.".into(),
            None,
        )
        .await
        .unwrap();
    typed.create_dir("user/broken", true, None).await.unwrap();
    typed
        .write_file("user/broken/SKILL.md", "---\nname: broken\n---\nMissing description.".into(), None)
        .await
        .unwrap();

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Source {
        User,
    }

    let result = load_sourced_skills::<Source>(
        &env,
        &[
            SourcedSkillInput { path: "user/example".to_string(), source: Source::User },
            SourcedSkillInput { path: "user/broken".to_string(), source: Source::User },
        ],
    )
    .await;

    assert_eq!(result.skills.len(), 1);
    let s = &result.skills[0];
    assert_eq!(s.skill.name, "example");
    assert_eq!(s.source, Source::User);

    assert_eq!(result.diagnostics.len(), 1);
    let d = &result.diagnostics[0];
    assert_eq!(d.diagnostic.code, SkillDiagnosticCode::InvalidMetadata);
    assert_eq!(d.diagnostic.message, "description is required");
    assert_eq!(d.source, Source::User);
    assert!(d.diagnostic.path.ends_with("user/broken/SKILL.md"));
}
