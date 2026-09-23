//! Changelog fetching and parsing.
//!
//! Port of native Pi's `packages/coding-agent/src/utils/changelog.ts`. Native pi
//! reads a local `CHANGELOG.md` when present and can otherwise fetch it from
//! GitHub. rpi keeps the same split: parse a local/remote markdown changelog into
//! versioned entries.
//!
//! The remote URL is overridable with `RPI_CHANGELOG_URL` so a fork can point at
//! its own changelog without a rebuild.

use serde::{Deserialize, Serialize};

/// One released version's changelog section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangelogEntry {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// The section body (everything under the heading, trimmed).
    pub content: String,
}

/// Default remote changelog URL (native `GITHUB_REPO`).
pub const DEFAULT_CHANGELOG_URL: &str =
    "https://raw.githubusercontent.com/earendil-works/pi/main/CHANGELOG.md";

/// The remote changelog URL to use, honoring `RPI_CHANGELOG_URL`.
pub fn changelog_url() -> String {
    std::env::var("RPI_CHANGELOG_URL").unwrap_or_else(|_| DEFAULT_CHANGELOG_URL.to_string())
}

/// Parse a semver-ish heading like `## 1.2.3` / `# 1.2.3` / `## v1.2.3`.
fn parse_heading(line: &str) -> Option<(u64, u64, u64)> {
    let trimmed = line.trim_start_matches('#').trim();
    let trimmed = trimmed.strip_prefix('v').unwrap_or(trimmed);
    let version = trimmed.split_whitespace().next()?;
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let patch = parts
        .next()
        .map(|p| p.split(['-', '+']).next().unwrap_or(p))
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    Some((major, minor, patch))
}

/// Split a changelog into versioned entries (native `parseChangelog`).
pub fn parse_changelog(text: &str) -> Vec<ChangelogEntry> {
    let mut entries: Vec<ChangelogEntry> = Vec::new();
    let mut current: Option<ChangelogEntry> = None;

    for line in text.lines() {
        let is_heading = line.trim_start().starts_with('#');
        if is_heading {
            if let Some((major, minor, patch)) = parse_heading(line) {
                if let Some(entry) = current.take() {
                    entries.push(finalize(entry));
                }
                current = Some(ChangelogEntry {
                    major,
                    minor,
                    patch,
                    content: String::new(),
                });
                continue;
            }
        }
        if let Some(entry) = current.as_mut() {
            entry.content.push_str(line);
            entry.content.push('\n');
        }
    }
    if let Some(entry) = current.take() {
        entries.push(finalize(entry));
    }
    entries
}

fn finalize(mut entry: ChangelogEntry) -> ChangelogEntry {
    entry.content = entry.content.trim().to_string();
    entry
}

/// Fetch the changelog body. `allow_network == false` returns `Ok(None)`.
pub async fn fetch_changelog(allow_network: bool) -> Result<Option<String>, String> {
    if !allow_network {
        return Ok(None);
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent(format!("rpi/{}", crate::VERSION))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .get(changelog_url())
        .header("accept", "text/plain")
        .send()
        .await
        .map_err(|e| format!("changelog request failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("changelog request failed: {}", response.status().as_u16()));
    }
    let text = response.text().await.map_err(|e| e.to_string())?;
    Ok(Some(text))
}

/// Fetch and parse the changelog, newest first. Best-effort: a network failure
/// yields an empty list rather than an error.
pub async fn load_entries(allow_network: bool) -> Vec<ChangelogEntry> {
    match fetch_changelog(allow_network).await {
        Ok(Some(text)) => parse_changelog(&text),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions() {
        let text = "\
# Changelog

## 1.2.3
- fixed a thing

## 0.9.0
- initial

## v2.0.0-beta.1
- future
";
        let entries = parse_changelog(text);
        assert_eq!(entries.len(), 3);
        assert_eq!((entries[0].major, entries[0].minor, entries[0].patch), (1, 2, 3));
        assert!(entries[0].content.contains("fixed a thing"));
        assert_eq!((entries[2].major, entries[2].minor, entries[2].patch), (2, 0, 0));
    }

    #[test]
    fn ignores_non_version_headings() {
        let text = "# Changelog\n\n## Unreleased\n- wip\n\n## 1.0.0\n- out\n";
        let entries = parse_changelog(text);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "- out");
    }

    #[test]
    fn offline_is_empty() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let entries = rt.block_on(load_entries(false));
        assert!(entries.is_empty());
    }
}
