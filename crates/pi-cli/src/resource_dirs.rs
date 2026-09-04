//! Resource-directory resolution + **project-wins dedupe** for skills and
//! prompt templates. Mirrors the precedence contract from pi's
//! `DefaultResourceLoader`/`package-manager` (`resource-loader.ts:676-681`,
//! `package-manager.ts:178-181`): resources are ranked project=0/1 < user=2/3
//! < package=4, and `addSkills`/`dedupePrompts` are **first-registration-wins**
//! on name → loading project *before* global means **project wins** on
//! collision (skills.ts:399-428), with a collision diagnostic naming the winner
//! (kept) and loser (dropped) paths.
//!
//! rpi's library loaders (`rpi_harness::skills::load_skills`,
//! `rpi_harness::prompt_templates::load_prompt_templates`) are **append-only**
//! (no dedup-by-name) — they correctly mirror pi's *per-directory* discovery
//! (SKILL.md-first / root-.md / subdir recursion for skills; non-recursive `.md`
//! children for prompts) but leave the cross-directory merge to the caller. This
//! module is that caller-side merge: load project dir then global dir, then
//! dedupe first-wins-by-name so project wins.
//!
//! **Trust gate (v1 divergence):** pi gates project `.pi/SYSTEM.md` /
//! `.pi/APPEND_SYSTEM.md` (and some project resources) behind
//! `settingsManager.isProjectTrusted()`. rpi v1 has **no trust prompt**
//! (`config.rs:349`: "does not gate any project resources behind trust in v1"),
//! so project resources are read unconditionally here. A copied `.pi/` directory
//! drops in and works (the documented intent). Full trust gating is deferred.
//!
//! **Deferred (documented):** pi's `.agents/skills` + `~/.agents/skills` +
//! package-installed skills/prompts (4 discovery roots in pi; rpi v1 mirrors the
//! two primary: project `.pi/<sub>` + user `agent_dir()<sub>`); worktree
//! shadowed-context-file dedup (`findShadowedContextFile`); full structured
//! winner/loser collision diagnostics (rpi v1 encodes collisions as a
//! `SkillDiagnostic`/`PromptTemplateDiagnostic` with a descriptive message).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rpi_harness::prompt_templates::{
    load_prompt_templates, LoadPromptTemplatesResult, PromptTemplateDiagnostic,
    PromptTemplateDiagnosticCode,
};
use rpi_harness::skills::{load_skills, LoadSkillsResult, SkillDiagnostic, SkillDiagnosticCode};
use rpi_harness::types::{PromptTemplate, Skill};
use rpi_tools::env::ExecutionEnv;

/// The project-local config dir name. Mirrors pi's `.pi/` (NOT `.rpi/`) so a
/// copied pi project directory drops in and works: skills under `<cwd>/.pi/skills`,
/// prompts under `<cwd>/.pi/prompts`, `SYSTEM.md`/`APPEND_SYSTEM.md` under
/// `<cwd>/.pi/`. The **global** config lives under `agent_dir()` (`~/.rpi/agent`),
/// which IS `.rpi` — see `config.rs`.
pub const PROJECT_CONFIG_DIR_NAME: &str = ".pi";

/// Resolve the project-local resource subdir `<cwd>/.pi/<sub>`.
pub fn project_dir(cwd: &Path, sub: &str) -> PathBuf {
    cwd.join(PROJECT_CONFIG_DIR_NAME).join(sub)
}

/// Resolve the global resource subdir `<agent_dir>/<sub>` (e.g.
/// `~/.rpi/agent/skills`). Returns `None` if the agent dir can't be resolved
/// (no home dir + no `RPI_CODING_AGENT_DIR`) — callers then proceed project-only.
pub fn global_dir(sub: &str) -> Option<PathBuf> {
    crate::config::agent_dir().ok().map(|d| d.join(sub))
}

/// The candidate context/SYSTEM/APPEND filenames live directly under
/// `<cwd>/.pi/` and `<agent_dir>/` (no `skills`/`prompts` subdir). Re-exports the
/// harness context-file candidates for the system/append discovery path so
/// callers share one source of truth.
pub fn project_config_file(cwd: &Path, name: &str) -> PathBuf {
    cwd.join(PROJECT_CONFIG_DIR_NAME).join(name)
}

/// Global config file under `<agent_dir>/<name>` (`~/.rpi/agent/SYSTEM.md`).
pub fn global_config_file(name: &str) -> Option<PathBuf> {
    crate::config::agent_dir().ok().map(|d| d.join(name))
}

// ---------------------------------------------------------------------------
// SYSTEM.md / APPEND_SYSTEM.md discovery (project-wins, mirroring pi)
// ---------------------------------------------------------------------------

/// Discover `SYSTEM.md`: project `<cwd>/.pi/SYSTEM.md` overrides global
/// `<agent_dir>/SYSTEM.md` (mirrors pi `discoverSystemPromptFile`
/// `resource-loader.ts:1022-1034`). Returns the first existing file in that
/// order, or `None`.
///
/// **Trust gate (v1 divergence):** pi gates the **project** `SYSTEM.md` behind
/// `settingsManager.isProjectTrusted()` (global is always honored). rpi v1 has
/// no trust prompt (`config.rs:349`), so the project file is read unconditionally
/// — a copied `.pi/` drops in and works. Full trust gating is deferred.
pub fn discover_system_prompt_file(cwd: &Path) -> Option<PathBuf> {
    let project = project_config_file(cwd, "SYSTEM.md");
    if project.is_file() {
        return Some(project);
    }
    global_config_file("SYSTEM.md").filter(|p| p.is_file())
}

/// Discover `APPEND_SYSTEM.md`: same precedence as `SYSTEM.md` — project
/// `<cwd>/.pi/APPEND_SYSTEM.md` overrides global `<agent_dir>/APPEND_SYSTEM.md`
/// (mirrors pi `discoverAppendSystemPromptFile` `resource-loader.ts:1036-1048`).
/// Returns the first existing file in that order, or `None`. The discovered
/// content is appended to the system prompt (pi `appendSystemPrompt`
/// `:525-542`).
///
/// **Trust gate (v1 divergence):** same as [`discover_system_prompt_file`] —
/// pi gates the project file on trust, rpi v1 reads it unconditionally.
pub fn discover_append_system_prompt_file(cwd: &Path) -> Option<PathBuf> {
    let project = project_config_file(cwd, "APPEND_SYSTEM.md");
    if project.is_file() {
        return Some(project);
    }
    global_config_file("APPEND_SYSTEM.md").filter(|p| p.is_file())
}

// ---------------------------------------------------------------------------
// Dedupe: first-wins-by-name (project wins when loaded project→global)
// ---------------------------------------------------------------------------

/// Dedupe skills by name, **first-wins**. Mirrors pi `addSkills`
/// (`skills.ts:399-428`): the first skill with a given name is kept; later
/// duplicates emit a collision diagnostic naming the winner (kept) and loser
/// (dropped) paths. Load dirs in **project→global** order so project wins.
///
/// **v1 divergence:** rpi's `SkillDiagnostic` has no structured
/// `winnerPath`/`loserPath` fields (pi's `SkillCollisionDiagnostic`); the
/// collision is encoded as an `InvalidMetadata` diagnostic with a descriptive
/// message naming both paths and `path` set to the loser.
pub fn dedupe_skills(skills: Vec<Skill>, diagnostics: &mut Vec<SkillDiagnostic>) -> Vec<Skill> {
    let mut winner_path: HashMap<String, String> = HashMap::new();
    let mut out: Vec<Skill> = Vec::with_capacity(skills.len());
    for skill in skills {
        if let Some(winner) = winner_path.get(&skill.name) {
            diagnostics.push(SkillDiagnostic {
                code: SkillDiagnosticCode::InvalidMetadata,
                message: format!(
                    "Skill name \"{}\" from {} is shadowed by {} \
                     (first-registration wins; load project before global so project wins)",
                    skill.name, skill.file_path, winner
                ),
                path: skill.file_path.clone(),
            });
        } else {
            winner_path.insert(skill.name.clone(), skill.file_path.clone());
            out.push(skill);
        }
    }
    out
}

/// Dedupe prompt templates by name, **first-wins**. Mirrors pi `dedupePrompts`
/// (`resource-loader.ts:969-993`): the first template with a given name is kept;
/// later duplicates emit a collision diagnostic. Load paths in **project→global**
/// order so project wins.
///
/// **v1 divergence:** `PromptTemplate` carries no `file_path` (only
/// name/description/content), so the collision diagnostic's `path` is set to the
/// colliding template **name** rather than a file path; and the code is
/// `ParseFailed` (rpi has no dedicated collision code) with a descriptive
/// message. Structured winner/loser diagnostics are deferred.
pub fn dedupe_prompt_templates(
    templates: Vec<PromptTemplate>,
    diagnostics: &mut Vec<PromptTemplateDiagnostic>,
) -> Vec<PromptTemplate> {
    let mut seen: HashMap<String, ()> = HashMap::new();
    let mut out: Vec<PromptTemplate> = Vec::with_capacity(templates.len());
    for t in templates {
        if seen.contains_key(&t.name) {
            diagnostics.push(PromptTemplateDiagnostic {
                code: PromptTemplateDiagnosticCode::ParseFailed,
                message: format!(
                    "Prompt template name \"{}\" is shadowed by an earlier registration \
                     (first-registration wins; load project before global so project wins)",
                    t.name
                ),
                // PromptTemplate carries no file path; the name is the collision
                // key, so use it as the diagnostic path.
                path: t.name.clone(),
            });
        } else {
            seen.insert(t.name.clone(), ());
            out.push(t);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Precedence-aware loaders: project dir → global dir → dedupe (project wins)
// ---------------------------------------------------------------------------

/// Load skills from `dirs` in order, then dedupe first-wins-by-name. Missing
/// directories are skipped silently by the underlying loader (NotFound →
/// `continue`, mirroring pi). Pass dirs in **project→global** order so project
/// wins on name collisions.
pub async fn load_skills_with_precedence(
    env: &Arc<dyn ExecutionEnv>,
    dirs: &[PathBuf],
) -> LoadSkillsResult {
    let dir_strs: Vec<String> = dirs
        .iter()
        .map(|d| d.to_string_lossy().into_owned())
        .collect();
    let mut result = load_skills(env, &dir_strs).await;
    result.skills = dedupe_skills(result.skills, &mut result.diagnostics);
    result
}

/// Load prompt templates from `paths` (dirs or `.md` files) in order, then
/// dedupe first-wins-by-name. Missing paths are skipped silently. Pass paths in
/// **project→global** order so project wins on name collisions.
pub async fn load_prompt_templates_with_precedence(
    env: &Arc<dyn ExecutionEnv>,
    paths: &[PathBuf],
) -> LoadPromptTemplatesResult {
    let path_strs: Vec<String> = paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let mut result = load_prompt_templates(env, &path_strs).await;
    result.prompt_templates =
        dedupe_prompt_templates(result.prompt_templates, &mut result.diagnostics);
    result
}

/// The ordered skill dirs for a project: `[<cwd>/.pi/skills, <agent_dir>/skills]`.
/// The global dir is omitted when `agent_dir()` can't be resolved (no home dir).
pub fn skill_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![project_dir(cwd, "skills")];
    if let Some(g) = global_dir("skills") {
        dirs.push(g);
    }
    dirs
}

/// The ordered prompt-template paths for a project:
/// `[<cwd>/.pi/prompts, <agent_dir>/prompts]`.
pub fn prompt_template_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![project_dir(cwd, "prompts")];
    if let Some(g) = global_dir("prompts") {
        dirs.push(g);
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, path: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: "d".to_string(),
            content: "c".to_string(),
            file_path: path.to_string(),
            disable_model_invocation: None,
        }
    }

    fn tmpl(name: &str) -> PromptTemplate {
        PromptTemplate {
            name: name.to_string(),
            description: None,
            content: "c".to_string(),
        }
    }

    #[test]
    fn dedupe_skills_first_wins_keeps_project() {
        // Project loaded first, global second; same name → project (first) wins.
        let skills = vec![
            skill("echo", "/proj/.pi/skills/echo/SKILL.md"),
            skill("echo", "/home/.rpi/agent/skills/echo/SKILL.md"),
        ];
        let mut diags = Vec::new();
        let out = dedupe_skills(skills, &mut diags);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].file_path, "/proj/.pi/skills/echo/SKILL.md");
        assert_eq!(diags.len(), 1);
        assert!(diags[0]
            .message
            .contains("/home/.rpi/agent/skills/echo/SKILL.md"));
        assert!(diags[0].message.contains("/proj/.pi/skills/echo/SKILL.md"));
        assert_eq!(diags[0].path, "/home/.rpi/agent/skills/echo/SKILL.md");
    }

    #[test]
    fn dedupe_skills_distinct_names_all_kept() {
        let skills = vec![skill("a", "/p/a"), skill("b", "/p/b"), skill("c", "/g/c")];
        let mut diags = Vec::new();
        let out = dedupe_skills(skills, &mut diags);
        assert_eq!(out.len(), 3);
        assert!(diags.is_empty());
    }

    #[test]
    fn dedupe_skills_third_duplicate_drops_against_first() {
        let skills = vec![
            skill("x", "/proj/x"),
            skill("x", "/global/x"),
            skill("x", "/pkg/x"),
        ];
        let mut diags = Vec::new();
        let out = dedupe_skills(skills, &mut diags);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].file_path, "/proj/x");
        // Both later duplicates emit a collision diagnostic vs the same winner.
        assert_eq!(diags.len(), 2);
    }

    #[test]
    fn dedupe_skills_empty_input() {
        let mut diags = Vec::new();
        let out = dedupe_skills(Vec::new(), &mut diags);
        assert!(out.is_empty());
        assert!(diags.is_empty());
    }

    #[test]
    fn dedupe_prompts_first_wins() {
        let templates = vec![tmpl("greet"), tmpl("greet")];
        let mut diags = Vec::new();
        let out = dedupe_prompt_templates(templates, &mut diags);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "greet");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].path, "greet");
    }

    #[test]
    fn dedupe_prompts_distinct_all_kept() {
        let templates = vec![tmpl("a"), tmpl("b"), tmpl("c")];
        let mut diags = Vec::new();
        let out = dedupe_prompt_templates(templates, &mut diags);
        assert_eq!(out.len(), 3);
        assert!(diags.is_empty());
    }

    #[test]
    fn project_dir_uses_pi_name() {
        let d = project_dir(Path::new("/proj"), "skills");
        assert_eq!(d, PathBuf::from("/proj/.pi/skills"));
    }

    #[test]
    fn project_config_file_under_pi() {
        let p = project_config_file(Path::new("/proj"), "SYSTEM.md");
        assert_eq!(p, PathBuf::from("/proj/.pi/SYSTEM.md"));
    }
}
