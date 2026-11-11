//! Autocomplete support for TUI.
//!
//! Provides autocomplete suggestions for file paths, commands, and custom items.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::fuzzy::fuzzy_filter;

/// Path delimiters for parsing.
const PATH_DELIMITERS: &[char] = &[' ', '\t', '"', '\'', '='];

/// Characters that separate prose from an attachment/path token in native Pi.
/// CJK punctuation is included so references can follow natural-language text.
fn is_path_delimiter(c: char) -> bool {
    PATH_DELIMITERS.contains(&c)
        || matches!(
            c,
            '，' | '．'
                | '：'
                | '；'
                | '！'
                | '？'
                | '（'
                | '）'
                | '［'
                | '］'
                | '｛'
                | '｝'
                | '“'
                | '”'
                | '‘'
                | '’'
                | '…'
                | '—'
        )
}

/// Opening wrappers which native Pi permits immediately before a path.
fn is_path_wrapper(c: char) -> bool {
    matches!(c, '(' | '[' | '{' | '<' | '`')
}

/// Remove an unmatched opening wrapper from a token, e.g. `(@src` -> `@src`.
fn strip_leading_path_wrappers(token: &str) -> (&str, usize) {
    let mut offset = 0;
    loop {
        let Some(character) = token[offset..].chars().next() else {
            break;
        };
        if !is_path_wrapper(character) {
            break;
        }
        let closing = match character {
            '(' => ')',
            '[' => ']',
            '{' => '}',
            '<' => '>',
            '`' => '`',
            _ => unreachable!(),
        };
        let character_end = offset + character.len_utf8();
        if token[character_end..].contains(closing) {
            break;
        }
        offset = character_end;
    }
    (&token[offset..], offset)
}

fn home_directory() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn expand_home_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return home_directory().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
        if let Some(home) = home_directory() {
            return home.join(rest);
        }
    }
    path.to_path_buf()
}

/// Greatest char-boundary byte index in `s` that is `<= idx` (clamped to
/// `s.len()`). Callers receive a `cursor` that may be a *character* count or a
/// byte offset handed back from the editor; either way, slicing
/// `&input[..cursor]` panics if `cursor` lands inside a multibyte char. Snap to
/// the nearest preceding boundary so the slice is always sound.
///
/// A position that IS a char boundary (including the END of the string) is
/// kept as-is — the old `take_while(b <= idx)` returned the START of the char
/// at `idx`, so `end: cursor` came out one char short and Tab-accept kept the
/// input's last char: typing "/mode" + Tab produced "/modele" instead of
/// "/model".
fn snap_cursor(s: &str, idx: usize) -> usize {
    let idx = idx.min(s.len());
    if s.is_char_boundary(idx) {
        return idx;
    }
    s.char_indices()
        .take_while(|(b, _)| *b < idx)
        .last()
        .map(|(b, _)| b)
        .unwrap_or(0)
}

/// An autocomplete item.
#[derive(Debug, Clone)]
pub struct AutocompleteItem {
    /// Display text.
    pub text: String,
    /// Display label (optional, defaults to text).
    pub label: Option<String>,
    /// Description.
    pub description: Option<String>,
    /// Whether this is a directory.
    pub is_directory: bool,
    /// Whether to insert a space after this item.
    pub insert_space: bool,
}

impl AutocompleteItem {
    /// Create a new autocomplete item.
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            label: None,
            description: None,
            is_directory: false,
            insert_space: true,
        }
    }

    /// Add a label.
    pub fn with_label(mut self, label: &str) -> Self {
        self.label = Some(label.to_string());
        self
    }

    /// Add a description.
    pub fn with_description(mut self, description: &str) -> Self {
        self.description = Some(description.to_string());
        self
    }

    /// Mark as directory.
    pub fn as_directory(mut self) -> Self {
        self.is_directory = true;
        self
    }

    /// Set whether to insert space after.
    pub fn with_insert_space(mut self, insert_space: bool) -> Self {
        self.insert_space = insert_space;
        self
    }

    /// Get the display text.
    pub fn display_text(&self) -> &str {
        self.label.as_ref().unwrap_or(&self.text)
    }
}

/// Autocomplete suggestions result.
#[derive(Debug, Clone)]
pub struct AutocompleteSuggestions {
    /// List of suggestions.
    pub items: Vec<AutocompleteItem>,
    /// Start position in the input.
    pub start: usize,
    /// End position in the input.
    pub end: usize,
    /// Whether this is a path completion.
    pub is_path: bool,
}

/// Autocomplete provider trait.
pub trait AutocompleteProvider: Send + Sync {
    /// Get suggestions for the given input.
    fn get_suggestions(&self, input: &str, cursor: usize) -> Option<AutocompleteSuggestions>;

    /// Get the name of this provider.
    fn name(&self) -> &str;
}

/// Slash command for command completion.
#[derive(Debug, Clone)]
pub struct SlashCommand {
    /// Command name (including the slash).
    pub name: String,
    /// Description.
    pub description: String,
}

const FUZZY_MAX_RESULTS: usize = 20;
const INDEX_MAX_ENTRIES: usize = 50_000;
const INDEX_TTL: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Clone)]
struct IndexedPath {
    path: String,
    is_directory: bool,
}

#[derive(Debug, Clone)]
struct CachedPathIndex {
    root: PathBuf,
    built_at: std::time::Instant,
    entries: Vec<IndexedPath>,
}

/// File path autocomplete provider.
pub struct FilePathAutocompleteProvider {
    root_path: Option<PathBuf>,
    max_results: usize,
    index: Arc<Mutex<Option<CachedPathIndex>>>,
}

fn to_display_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn completion_needs_quotes(path: &str) -> bool {
    path.chars().any(|character| {
        character.is_whitespace()
            || matches!(
                character,
                '，' | '．'
                    | '：'
                    | '；'
                    | '！'
                    | '？'
                    | '（'
                    | '）'
                    | '［'
                    | '］'
                    | '｛'
                    | '｝'
                    | '“'
                    | '”'
                    | '‘'
                    | '’'
                    | '…'
                    | '—'
            )
    })
}

/// Build one native-Pi-style path item. `quoted_prefix` means the opening
/// quote is already in the editor and must remain outside the replacement
/// span; the returned text therefore contains only the closing quote.
fn build_path_item(
    display_path: &str,
    label: &str,
    is_directory: bool,
    quoted_prefix: bool,
    with_description: bool,
) -> AutocompleteItem {
    let path_value = if is_directory {
        format!("{display_path}/")
    } else {
        display_path.to_string()
    };
    let needs_quotes = quoted_prefix || completion_needs_quotes(&path_value);
    let quoted_value = if needs_quotes {
        format!("\"{path_value}\"")
    } else {
        path_value
    };
    let text = if quoted_prefix {
        quoted_value
            .strip_prefix('"')
            .unwrap_or(&quoted_value)
            .to_string()
    } else {
        quoted_value
    };
    let display_label = if is_directory {
        format!("{label}/")
    } else {
        label.to_string()
    };
    let mut item = AutocompleteItem::new(&text)
        .with_label(&display_label)
        .with_insert_space(!is_directory);
    if is_directory {
        item = item.as_directory();
    }
    if with_description {
        item = item.with_description(display_path);
    }
    item
}

fn path_basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn path_depth(path: &str) -> usize {
    path.split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .count()
}

fn score_indexed_path(path: &str, query: &str, is_directory: bool) -> i32 {
    if query.is_empty() {
        return 1;
    }
    let lower_path = path.replace('\\', "/").to_lowercase();
    let lower_name = path_basename(path).to_lowercase();
    let lower_query = query.replace('\\', "/").to_lowercase();
    let mut score = if lower_name == lower_query {
        100
    } else if lower_name.starts_with(&lower_query) {
        80
    } else if lower_name.contains(&lower_query) {
        50
    } else if lower_path.contains(&lower_query) {
        30
    } else {
        0
    };
    if is_directory && score > 0 {
        score += 10;
    }
    score
}

impl FilePathAutocompleteProvider {
    /// Create a new file path provider.
    pub fn new() -> Self {
        Self {
            root_path: None,
            max_results: 50,
            index: Arc::new(Mutex::new(None)),
        }
    }

    /// Create a provider with a root path.
    pub fn with_root(root: PathBuf) -> Self {
        Self {
            root_path: Some(root),
            max_results: 50,
            index: Arc::new(Mutex::new(None)),
        }
    }

    /// Set max results.
    pub fn with_max_results(mut self, max: usize) -> Self {
        self.max_results = max;
        self
    }

    fn root_directory(&self) -> PathBuf {
        self.root_path
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }

    /// Extract the path prefix from input.
    fn extract_path_prefix(&self, input: &str, cursor: usize) -> Option<(String, usize)> {
        let cursor = snap_cursor(input, cursor);
        let before_cursor = &input[..cursor];
        let mut start = 0;
        for (index, character) in before_cursor.char_indices().rev() {
            let is_at_quote =
                character == '"' && index > 0 && before_cursor[..index].ends_with('@');
            if is_path_delimiter(character) && !is_at_quote {
                start = index + character.len_utf8();
                break;
            }
        }

        let raw_token = &input[start..cursor];
        let (token, wrapper_len) = strip_leading_path_wrappers(raw_token);
        let token_start = start + wrapper_len;
        if token.starts_with('@') {
            let path = &token[1..];
            if let Some(path) = path.strip_prefix('"') {
                return Some((path.to_string(), token_start + 2));
            }
            return Some((path.to_string(), token_start + 1));
        }
        if token.starts_with('"') {
            return Some((token[1..].to_string(), token_start + 1));
        }

        let looks_like_path = token.contains('/')
            || token.contains('\\')
            || token.starts_with('.')
            || token.starts_with("~/");
        let quoted = token_start > 0 && input[..token_start].ends_with('"');
        if !looks_like_path && !quoted {
            return None;
        }
        Some((token.to_string(), token_start))
    }

    fn resolve_directory(&self, raw_dir: &Path) -> PathBuf {
        let expanded = expand_home_path(raw_dir);
        if expanded.is_absolute() {
            expanded
        } else {
            self.root_directory().join(expanded)
        }
    }

    /// Get direct-directory completions for ordinary paths.
    fn get_path_completions(&self, prefix: &str) -> Vec<AutocompleteItem> {
        let quoted = prefix.starts_with('"');
        let prefix = prefix.strip_prefix('"').unwrap_or(prefix);
        let prefix = prefix.strip_suffix('"').unwrap_or(prefix);
        let has_separator = prefix.contains('/') || prefix.contains('\\');
        let (raw_dir, partial) = if prefix == "~" {
            (PathBuf::from(prefix), String::new())
        } else if prefix.ends_with(['/', '\\']) {
            (PathBuf::from(prefix), String::new())
        } else if has_separator {
            let path = Path::new(prefix);
            let parent = path.parent().unwrap_or(Path::new("."));
            let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            (parent.to_path_buf(), file_name.to_string())
        } else {
            (PathBuf::from("."), prefix.to_string())
        };
        let dir = self.resolve_directory(&raw_dir);
        let partial_lower = partial.to_lowercase();
        let mut entries = Vec::new();
        if let Ok(read_dir) = std::fs::read_dir(&dir) {
            for entry in read_dir.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !partial_lower.is_empty() && !name.to_lowercase().starts_with(&partial_lower) {
                    continue;
                }
                let is_dir =
                    entry.file_type().map(|t| t.is_dir()).unwrap_or(false) || entry.path().is_dir();
                entries.push((name, is_dir));
            }
        }
        entries.sort_by(|(left_name, left_dir), (right_name, right_dir)| {
            right_dir
                .cmp(left_dir)
                .then_with(|| left_name.to_lowercase().cmp(&right_name.to_lowercase()))
                .then_with(|| left_name.cmp(right_name))
        });
        entries.truncate(self.max_results);

        let display_base = if prefix == "~" {
            "~/".to_string()
        } else if prefix.ends_with(['/', '\\']) {
            to_display_path(prefix)
        } else {
            prefix
                .rfind(['/', '\\'])
                .map(|index| to_display_path(&prefix[..index + 1]))
                .unwrap_or_default()
        };
        entries
            .into_iter()
            .map(|(name, is_dir)| {
                let display_path = format!("{display_base}{name}");
                build_path_item(&display_path, &name, is_dir, quoted, true)
            })
            .collect()
    }

    fn resolve_fuzzy_scope(&self, raw_query: &str) -> Option<(PathBuf, String, String)> {
        let normalized = to_display_path(raw_query);
        if normalized == "~" {
            return home_directory().map(|home| (home, String::new(), "~/".to_string()));
        }
        let Some(slash) = normalized.rfind(['/', '\\']) else {
            return Some((self.root_directory(), normalized, String::new()));
        };
        let display_base = normalized[..slash + 1].to_string();
        let query = normalized[slash + 1..].to_string();
        let base = Path::new(&display_base);
        let base_dir = if display_base.starts_with("~/") {
            expand_home_path(base)
        } else if base.is_absolute() {
            base.to_path_buf()
        } else {
            self.root_directory().join(base)
        };
        if !base_dir.is_dir() {
            return None;
        }
        Some((base_dir, query, display_base))
    }

    fn build_index(root: &Path) -> Vec<IndexedPath> {
        let mut entries = Vec::new();
        let walker = ignore::WalkBuilder::new(root)
            .hidden(false)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .follow_links(true)
            .filter_entry(|entry| entry.file_name() != ".git")
            .build();
        for result in walker {
            let Ok(entry) = result else { continue };
            let path = entry.path();
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            if relative.as_os_str().is_empty() {
                continue;
            }
            let Some(relative) = relative.to_str() else {
                continue;
            };
            let is_directory = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
            entries.push(IndexedPath {
                path: to_display_path(relative),
                is_directory,
            });
            if entries.len() >= INDEX_MAX_ENTRIES {
                break;
            }
        }
        entries
    }

    fn indexed_entries(&self, root: &Path) -> Vec<IndexedPath> {
        let now = std::time::Instant::now();
        if let Ok(cache) = self.index.lock() {
            if let Some(cache) = cache.as_ref() {
                if cache.root == root && now.duration_since(cache.built_at) < INDEX_TTL {
                    return cache.entries.clone();
                }
            }
        }
        let entries = Self::build_index(root);
        if let Ok(mut cache) = self.index.lock() {
            *cache = Some(CachedPathIndex {
                root: root.to_path_buf(),
                built_at: now,
                entries: entries.clone(),
            });
        }
        entries
    }

    /// Get the recursive fuzzy candidates used by native Pi for `@` paths.
    fn get_at_completions(&self, raw_query: &str, quoted: bool) -> Vec<AutocompleteItem> {
        let raw_query = raw_query.strip_prefix('"').unwrap_or(raw_query);
        let raw_query = raw_query.strip_suffix('"').unwrap_or(raw_query);
        let Some((base_dir, query, display_base)) = self.resolve_fuzzy_scope(raw_query) else {
            return Vec::new();
        };
        let entries = self.indexed_entries(&base_dir);
        let mut scored = entries
            .into_iter()
            .filter_map(|entry| {
                let score = score_indexed_path(&entry.path, &query, entry.is_directory);
                (score > 0).then_some((score, entry))
            })
            .collect::<Vec<_>>();
        scored.sort_by(|(left_score, left), (right_score, right)| {
            right_score
                .cmp(left_score)
                .then_with(|| path_depth(&left.path).cmp(&path_depth(&right.path)))
                .then_with(|| left.path.len().cmp(&right.path.len()))
                .then_with(|| left.path.cmp(&right.path))
        });
        scored
            .into_iter()
            .take(FUZZY_MAX_RESULTS)
            .map(|(_, entry)| {
                let display_path = format!("{}{}", display_base, entry.path);
                build_path_item(
                    &display_path,
                    path_basename(&entry.path),
                    entry.is_directory,
                    quoted,
                    true,
                )
            })
            .collect()
    }
}

impl Default for FilePathAutocompleteProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl AutocompleteProvider for FilePathAutocompleteProvider {
    fn get_suggestions(&self, input: &str, cursor: usize) -> Option<AutocompleteSuggestions> {
        let cursor = snap_cursor(input, cursor);
        let (prefix, start) = self.extract_path_prefix(input, cursor)?;
        let quoted_at = start >= 2 && input[..start].ends_with("@\"");
        let is_at_reference = quoted_at || start > 0 && input[..start].ends_with('@');

        if prefix.is_empty() && !is_at_reference {
            return None;
        }

        // The provider keeps the opening quote outside the replacement span,
        // but the path lookup still needs to know that this is a quoted @
        // reference so a closing quote can be inserted for files.
        let lookup_prefix = if quoted_at {
            format!("\"{prefix}")
        } else {
            prefix.clone()
        };
        let items = if is_at_reference {
            self.get_at_completions(&lookup_prefix, quoted_at)
        } else {
            self.get_path_completions(&lookup_prefix)
        };
        if items.is_empty() {
            return None;
        }

        Some(AutocompleteSuggestions {
            items,
            start,
            end: cursor,
            is_path: true,
        })
    }

    fn name(&self) -> &str {
        "filepath"
    }
}

/// Slash command autocomplete provider.
pub struct SlashCommandAutocompleteProvider {
    commands: Vec<SlashCommand>,
}

impl SlashCommandAutocompleteProvider {
    /// Create a new slash command provider.
    pub fn new(commands: Vec<SlashCommand>) -> Self {
        Self { commands }
    }

    /// Create with default commands plus a set of dynamically-discovered slash
    /// commands. The discovered commands (e.g. prompt-template names exposed as
    /// `/expand`-style `/`-prefixed entries) are appended to the built-in set so
    /// the built-in names win on a fuzzy tie and the discovered names appear as
    /// additional suggestions. Mirrors pi's behavior where prompt-template
    /// invocations (`/<name>`) surface alongside built-in slash commands in
    /// `/`-autocomplete (`agent-session.ts:1124` + `expandPromptTemplate`).
    pub fn with_commands(discovered: Vec<SlashCommand>) -> Self {
        let mut commands = Self::with_default_commands().commands;
        commands.extend(discovered);
        Self { commands }
    }

    /// Create with default commands.
    pub fn with_default_commands() -> Self {
        Self::new(vec![
            SlashCommand {
                name: "/help".to_string(),
                description: "Show help information".to_string(),
            },
            SlashCommand {
                name: "/clear".to_string(),
                description: "Clear the conversation".to_string(),
            },
            SlashCommand {
                name: "/exit".to_string(),
                description: "Exit the application".to_string(),
            },
            SlashCommand {
                name: "/model".to_string(),
                description: "Change the model".to_string(),
            },
            SlashCommand {
                name: "/context".to_string(),
                description: "List discovered context files, skills, and prompt templates"
                    .to_string(),
            },
        ])
    }

    /// Add a command.
    pub fn add_command(&mut self, command: SlashCommand) {
        self.commands.push(command);
    }
}

impl AutocompleteProvider for SlashCommandAutocompleteProvider {
    fn get_suggestions(&self, input: &str, cursor: usize) -> Option<AutocompleteSuggestions> {
        let cursor = snap_cursor(input, cursor);
        let before_cursor = &input[..cursor];

        // Check if we're at the start of a slash command
        if !before_cursor.starts_with('/') {
            return None;
        }

        let query = &before_cursor[1..];
        let items: Vec<AutocompleteItem> = fuzzy_filter(&self.commands, query, |cmd| &cmd.name)
            .into_iter()
            .map(|cmd| {
                AutocompleteItem::new(&cmd.name)
                    .with_description(&cmd.description)
                    .with_insert_space(false)
            })
            .collect();

        if items.is_empty() {
            return None;
        }

        Some(AutocompleteSuggestions {
            items,
            start: 0,
            end: cursor,
            is_path: false,
        })
    }

    fn name(&self) -> &str {
        "slash_command"
    }
}

/// Combined autocomplete provider that chains multiple providers.
pub struct CombinedAutocompleteProvider {
    providers: Vec<Arc<dyn AutocompleteProvider>>,
}

impl CombinedAutocompleteProvider {
    /// Create a new combined provider.
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    /// Add a provider.
    pub fn add_provider(&mut self, provider: Arc<dyn AutocompleteProvider>) {
        self.providers.push(provider);
    }

    /// Create with default providers.
    pub fn with_defaults() -> Self {
        let mut combined = Self::new();
        combined.add_provider(Arc::new(
            SlashCommandAutocompleteProvider::with_default_commands(),
        ));
        combined.add_provider(Arc::new(FilePathAutocompleteProvider::new()));
        combined
    }
}

impl Default for CombinedAutocompleteProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl AutocompleteProvider for CombinedAutocompleteProvider {
    fn get_suggestions(&self, input: &str, cursor: usize) -> Option<AutocompleteSuggestions> {
        for provider in &self.providers {
            if let Some(suggestions) = provider.get_suggestions(input, cursor) {
                return Some(suggestions);
            }
        }
        None
    }

    fn name(&self) -> &str {
        "combined"
    }
}

/// Global autocomplete manager.
pub struct AutocompleteManager {
    provider: Mutex<Option<Arc<dyn AutocompleteProvider>>>,
}

impl AutocompleteManager {
    /// Create a new manager.
    pub fn new() -> Self {
        Self {
            provider: Mutex::new(None),
        }
    }

    /// Set the provider.
    pub fn set_provider(&self, provider: Arc<dyn AutocompleteProvider>) {
        if let Ok(mut p) = self.provider.lock() {
            *p = Some(provider);
        }
    }

    /// Get suggestions.
    pub fn get_suggestions(&self, input: &str, cursor: usize) -> Option<AutocompleteSuggestions> {
        self.provider
            .lock()
            .ok()?
            .as_ref()?
            .get_suggestions(input, cursor)
    }
}

impl Default for AutocompleteManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique scratch directory for a filesystem probe; removed on drop.
    /// The provider tests live in this crate, which has no `tempfile`
    /// dev-dependency — a per-test unique name under the OS temp dir is
    /// enough and keeps `Cargo.lock` untouched.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0);
            let path =
                std::env::temp_dir().join(format!("rpi-tui-{tag}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn snap_cursor_keeps_boundaries_including_end() {
        // Regression: the caret at the END of the input (what Tab-accept sees)
        // snapped one char short, so `end` excluded the last char and accept
        // kept it: "/mode" + Tab became "/modele" instead of "/model".
        assert_eq!(snap_cursor("/mode", 5), 5);
        assert_eq!(snap_cursor("/mo", 3), 3);
        assert_eq!(snap_cursor("/model", 6), 6);
        assert_eq!(snap_cursor("abc", 3), 3);
        // Interior boundary + mid-multibyte snap down.
        assert_eq!(snap_cursor("/mode", 2), 2);
        assert_eq!(snap_cursor("你", 1), 0);
        assert_eq!(snap_cursor("你", 3), 3);
        // Out-of-range clamps to the end.
        assert_eq!(snap_cursor("/mode", 99), 5);
    }
    #[test]
    fn test_slash_command_provider() {
        let provider = SlashCommandAutocompleteProvider::with_default_commands();
        let suggestions = provider.get_suggestions("/he", 3);

        assert!(suggestions.is_some());
        let s = suggestions.unwrap();
        assert!(!s.items.is_empty());
        assert!(s.items[0].text.starts_with("/help"));
    }

    #[test]
    fn test_autocomplete_item() {
        let item = AutocompleteItem::new("test.txt")
            .with_description("A test file")
            .as_directory();

        assert_eq!(item.text, "test.txt");
        assert!(item.is_directory);
    }

    /// A prefix that ends at a separator asks for the directory's CONTENTS.
    /// `Path::parent`/`file_name` ignore the trailing separator, so splitting
    /// there used to search the parent for an entry named after the directory
    /// itself and offer `nested/` for the text `…/nested/` — inserting it
    /// again produced `…/nested/nested/`, a path that does not exist.
    #[test]
    fn file_path_provider_browses_into_a_trailing_separator() {
        let scratch = Scratch::new("trailing-separator");
        let nested = scratch.0.join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("report.txt"), "fixture").unwrap();
        assert!(!nested.join("nested").exists());
        let provider = FilePathAutocompleteProvider::with_root(scratch.0.clone());
        let prefix = format!("@{}{}", nested.display(), std::path::MAIN_SEPARATOR);
        let suggestions = provider
            .get_suggestions(&prefix, prefix.len())
            .expect("the directory's contents are a completion");
        let texts: Vec<String> = suggestions
            .items
            .iter()
            .map(|item| item.text.clone())
            .collect();
        let display_nested = to_display_path(&format!(
            "{}{separator}",
            nested.display(),
            separator = std::path::MAIN_SEPARATOR
        ));
        assert_eq!(
            texts,
            vec![format!("{display_nested}report.txt")],
            "a trailing separator must list the directory's contents, not offer its own name"
        );
        assert_eq!(suggestions.start, 1, "the @ stays outside the span");
        assert_eq!(suggestions.end, prefix.len());
    }

    /// The partial-name path is unchanged: `…/nes` still completes the entry
    /// `nested/` inside its parent.
    #[test]
    fn file_path_provider_still_completes_a_partial_name() {
        let scratch = Scratch::new("partial-name");
        let nested = scratch.0.join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("report.txt"), "fixture").unwrap();
        let provider = FilePathAutocompleteProvider::with_root(scratch.0.clone());
        let prefix = format!("@{}{}nes", scratch.0.display(), std::path::MAIN_SEPARATOR);
        let suggestions = provider
            .get_suggestions(&prefix, prefix.len())
            .expect("a partial directory name");
        let texts: Vec<String> = suggestions
            .items
            .iter()
            .map(|item| item.text.clone())
            .collect();
        let display_root = to_display_path(&format!(
            "{}{separator}",
            scratch.0.display(),
            separator = std::path::MAIN_SEPARATOR
        ));
        assert_eq!(
            texts,
            vec![
                format!("{display_root}nested/"),
                format!("{display_root}nested/report.txt")
            ]
        );
        assert_eq!(suggestions.start, 1);
        assert_eq!(suggestions.end, prefix.len());
    }

    /// Fork issue #11: a bare word in prose is not a path. Holding an arrow
    /// key moves the caret through `n n` / `cc cc` / `hhhh hhhh`, and every
    /// keystroke called `refresh_autocomplete`; the old provider answered
    /// each token (`c`, `hhhh`, ...) with a `read_dir` of cwd, so the file
    /// panel flickered open and shut. Native pi keeps an explicit path
    /// appearance gate here — this probe fails on the pre-fix provider.
    #[test]
    fn bare_word_tokens_never_open_the_file_panel() {
        let scratch = Scratch::new("bare-token");
        std::fs::write(scratch.0.join("chromium-proxy-policy.reg"), "fixture").unwrap();
        let provider = FilePathAutocompleteProvider::with_root(scratch.0.clone());
        let text = "n n\ncc cc\nhhhh hhhh";
        // Every caret position inside a bare word, including the `c` in
        // `cc c|c` from the report: the popup must stay closed for the whole
        // arrow-key sweep. `cursor` is a byte offset, as the editor supplies.
        for (cursor, _) in text
            .char_indices()
            .chain(std::iter::once((text.len(), ' ')))
        {
            if text[..cursor]
                .chars()
                .last()
                .is_some_and(char::is_whitespace)
            {
                continue;
            }
            assert!(
                provider.get_suggestions(text, cursor).is_none(),
                "bare word at byte {cursor} in {text:?} opened a file panel"
            );
        }
        assert!(provider.get_suggestions("n n\ncc cc", 5).is_none());
    }

    #[test]
    fn a_leading_dot_still_looks_like_a_path() {
        let scratch = Scratch::new("leading-dot");
        std::fs::write(scratch.0.join(".env"), "fixture").unwrap();
        let provider = FilePathAutocompleteProvider::with_root(scratch.0.clone());
        // Native pi treats any leading `.` as a path trigger, including a
        // lone dot in prose. Keep that exemption rather than inventing a
        // stricter gate than upstream.
        let suggestions = provider
            .get_suggestions("hello .", 7)
            .expect("a leading dot still looks like a path");
        assert_eq!(suggestions.start, 6);
        assert_eq!(suggestions.end, 7);
    }

    /// The gate must not take away any of the explicit ways to ask for a
    /// file: `@` references, quoted paths, and path-shaped tokens. Assert on
    /// the extraction step, not `get_suggestions`: a relative path containing
    /// a separator is resolved against the process cwd (pre-existing
    /// `get_path_completions` behavior), so whether it finds an entry is not
    /// a property of this gate.
    #[test]
    fn path_appearance_and_explicit_references_still_complete() {
        let provider = FilePathAutocompleteProvider::new();
        let cases: &[(&str, &str, usize)] = &[
            ("@st", "st", 1),
            ("@nested/", "nested/", 1),
            ("@\"no", "no", 2),
            ("\"no", "no", 1),
            ("./ed", "./ed", 0),
            ("./", "./", 0),
            ("~/", "~/", 0),
            ("a/b", "a/b", 0),
            ("a\\b", "a\\b", 0),
            ("/ab", "/ab", 0),
        ];
        for (input, prefix, start) in cases {
            let extracted = provider.extract_path_prefix(input, input.len());
            assert_eq!(
                extracted.as_ref().map(|(p, s)| (p.as_str(), *s)),
                Some((*prefix, *start)),
                "{input:?} must still be extracted as a path"
            );
        }
    }

    /// End-to-end: an `@` reference and an absolute path still produce
    /// candidates through `get_suggestions` after the gate.
    #[test]
    fn explicit_file_references_still_resolve() {
        let scratch = Scratch::new("path-gate");
        std::fs::write(scratch.0.join("notes.txt"), "fixture").unwrap();
        let nested = scratch.0.join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("report.txt"), "fixture").unwrap();
        let provider = FilePathAutocompleteProvider::with_root(scratch.0.clone());

        let at = "@notes";
        let suggestions = provider
            .get_suggestions(at, at.len())
            .expect("an @ reference completes a root entry");
        assert_eq!(suggestions.items[0].text, "notes.txt");
        assert_eq!(suggestions.start, 1);

        let absolute = format!("@{}{}repo", nested.display(), std::path::MAIN_SEPARATOR);
        let suggestions = provider
            .get_suggestions(&absolute, absolute.len())
            .expect("an absolute @ path completes its basename");
        let display_nested = to_display_path(&format!(
            "{}{separator}",
            nested.display(),
            separator = std::path::MAIN_SEPARATOR
        ));
        assert_eq!(
            suggestions.items[0].text,
            format!("{display_nested}report.txt")
        );

        let relative = "@nested/rep";
        let suggestions = provider
            .get_suggestions(relative, relative.len())
            .expect("a relative @ path is scoped to the provider root");
        assert_eq!(suggestions.items[0].text, "nested/report.txt");
        assert_eq!(
            suggestions.items[0].description.as_deref(),
            Some("nested/report.txt")
        );
    }

    #[test]
    fn at_references_follow_native_token_boundaries_and_wrappers() {
        let scratch = Scratch::new("at-boundaries");
        std::fs::write(scratch.0.join("notes.txt"), "fixture").unwrap();
        let provider = FilePathAutocompleteProvider::with_root(scratch.0.clone());

        for input in ["查看，@not", "(@not", "`@not"] {
            let suggestions = provider
                .get_suggestions(input, input.len())
                .unwrap_or_else(|| panic!("{input:?} should open @ completion"));
            assert_eq!(suggestions.items[0].text, "notes.txt", "input: {input:?}");
            assert!(
                input[..suggestions.start].ends_with('@'),
                "the replacement span must begin after @: {input:?}"
            );
        }
    }

    #[test]
    fn quoted_at_references_close_the_quote_for_files() {
        let scratch = Scratch::new("at-quoted");
        std::fs::write(scratch.0.join("notes.txt"), "fixture").unwrap();
        let provider = FilePathAutocompleteProvider::with_root(scratch.0.clone());
        let input = "@\"not";
        let suggestions = provider
            .get_suggestions(input, input.len())
            .expect("a quoted @ reference should complete");

        assert_eq!(suggestions.start, 2, "replace only the text after @\"");
        assert_eq!(suggestions.items[0].text, "notes.txt\"");
    }

    #[test]
    fn at_search_is_recursive_ignored_and_quotes_special_paths() {
        let scratch = Scratch::new("recursive-at");
        std::fs::write(scratch.0.join(".gitignore"), "ignored/\nignored.txt\n").unwrap();
        std::fs::create_dir_all(scratch.0.join("src")).unwrap();
        std::fs::write(scratch.0.join("src").join("main.rs"), "fixture").unwrap();
        std::fs::create_dir_all(scratch.0.join("ignored")).unwrap();
        std::fs::write(scratch.0.join("ignored").join("main.rs"), "fixture").unwrap();
        std::fs::write(scratch.0.join("ignored.txt"), "fixture").unwrap();
        std::fs::create_dir(scratch.0.join(".git")).unwrap();
        std::fs::write(scratch.0.join(".git").join("main.rs"), "fixture").unwrap();
        std::fs::write(scratch.0.join("my notes.txt"), "fixture").unwrap();

        let provider = FilePathAutocompleteProvider::with_root(scratch.0.clone());
        let recursive = provider
            .get_suggestions("@main", "@main".len())
            .expect("recursive @ search should find nested files");
        let main_path = "src/main.rs";
        assert_eq!(recursive.items[0].text, main_path);
        assert!(recursive
            .items
            .iter()
            .all(|item| !item.text.contains("ignored") && !item.text.contains(".git")));

        assert!(provider
            .get_suggestions("@ignored", "@ignored".len())
            .is_none());
        let spaced = provider
            .get_suggestions("@my", "@my".len())
            .expect("a spaced file should be searchable");
        assert_eq!(spaced.items[0].text, "\"my notes.txt\"");
    }

    #[test]
    fn file_items_are_not_marked_as_directories() {
        let scratch = Scratch::new("file-kind");
        std::fs::write(scratch.0.join("notes.txt"), "fixture").unwrap();
        std::fs::create_dir(scratch.0.join("src")).unwrap();
        let provider = FilePathAutocompleteProvider::with_root(scratch.0.clone());
        let suggestions = provider
            .get_suggestions("@", 1)
            .expect("bare @ should list root entries");

        let file = suggestions
            .items
            .iter()
            .find(|item| item.text == "notes.txt")
            .expect("file suggestion");
        let directory = suggestions
            .items
            .iter()
            .find(|item| item.text == "src/")
            .expect("directory suggestion");
        assert!(!file.is_directory);
        assert!(directory.is_directory);
    }
}
