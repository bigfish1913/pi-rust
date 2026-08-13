//! Mirrors `packages/agent/src/harness/system-prompt.ts` — composition of the
//! harness system prompt from a base prompt + the skill listing.
//!
//! The TS `system-prompt.ts` file contains only `formatSkillsForSystemPrompt`;
//! that function is implemented in [`crate::skills`] (co-located with the
//! loader for cohesion, per the module doc). This module owns the *composition*
//! helper that assembles the final system string the harness sends to the
//! provider: the base prompt, followed by the skill listing (when non-empty),
//! joined by a blank line — mirroring how `agent-harness.ts` stitches the two.
//!
//! The skill *invocation* block (unescaped, full content) is a separate path
//! (invariant §9) handled by [`crate::skills::format_skill_invocation`] and is
//! injected as a user message at invocation time, NOT into the system prompt.

use crate::skills::format_skills_for_system_prompt;
use crate::types::Skill;

/// Compose the harness system prompt: base + skill listing. Mirrors the
/// assembly in `agent-harness.ts` (`basePrompt + "\n\n" + skillListing` when
/// the listing is non-empty; just `basePrompt` otherwise).
///
/// `base_prompt` may be `None`/empty — in that case the result is the skill
/// listing alone (or empty if there are no model-visible skills). This matches
/// the TS behavior where a missing base prompt simply omits the prefix.
pub fn compose_system_prompt(base_prompt: Option<&str>, skills: &[Skill]) -> String {
    let listing = format_skills_for_system_prompt(skills);
    match base_prompt.map(str::trim).filter(|s| !s.is_empty()) {
        Some(base) if listing.is_empty() => base.to_string(),
        Some(base) => format!("{base}\n\n{listing}"),
        None => listing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Skill;

    fn skill(name: &str, desc: &str, file: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: desc.to_string(),
            content: "c".to_string(),
            file_path: file.to_string(),
            disable_model_invocation: None,
        }
    }

    #[test]
    fn base_only_when_no_skills() {
        assert_eq!(compose_system_prompt(Some("You are helpful."), &[]), "You are helpful.");
    }

    #[test]
    fn listing_only_when_no_base() {
        let out = compose_system_prompt(None, &[skill("x", "d", "/x/SKILL.md")]);
        assert!(out.starts_with("The following skills"));
        assert!(out.contains("<name>x</name>"));
    }

    #[test]
    fn base_and_listing_joined_by_blank_line() {
        let out = compose_system_prompt(Some("You are helpful."), &[skill("x", "d", "/x/SKILL.md")]);
        assert!(out.starts_with("You are helpful.\n\nThe following skills"));
    }

    #[test]
    fn empty_when_no_base_and_no_visible_skills() {
        let mk = || {
            let mut s = skill("hidden", "h", "/x/SKILL.md");
            s.disable_model_invocation = Some(true);
            s
        };
        assert_eq!(compose_system_prompt(None, &[mk()]), "");
        assert_eq!(compose_system_prompt(Some("   "), &[mk()]), "");
    }
}
