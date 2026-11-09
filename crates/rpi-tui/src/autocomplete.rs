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

/// File path autocomplete provider.
pub struct FilePathAutocompleteProvider {
    root_path: Option<PathBuf>,
    max_results: usize,
}

impl FilePathAutocompleteProvider {
    /// Create a new file path provider.
    pub fn new() -> Self {
        Self {
            root_path: None,
            max_results: 50,
        }
    }

    /// Create a provider with a root path.
    pub fn with_root(root: PathBuf) -> Self {
        Self {
            root_path: Some(root),
            max_results: 50,
        }
    }

    /// Set max results.
    pub fn with_max_results(mut self, max: usize) -> Self {
        self.max_results = max;
        self
    }

    /// Extract the path prefix from input.
    fn extract_path_prefix(&self, input: &str, cursor: usize) -> Option<(String, usize)> {
        // Snap to a char boundary so the slices below can never land inside a
        // multibyte char (callers may pass a char count, not a byte offset).
        let cursor = snap_cursor(input, cursor);
        let before_cursor = &input[..cursor];

        // Native Pi treats CJK punctuation as a token boundary. Opening
        // wrappers are stripped separately, so
        // `(@src` and `` `@src `` retain their wrapper in the editor text.
        let mut start = 0;
        for (index, character) in before_cursor.char_indices().rev() {
            // The quote in an @-reference (`@"src/ma`) belongs to the token;
            // ordinary quoted paths still use the quote as their delimiter.
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

        // Check for an @file reference. Return the position after `@` so the
        // prefix itself is replaced while `@`, parentheses, and backticks are
        // preserved in the editor text.
        if token.starts_with('@') {
            let path = &token[1..];
            if let Some(path) = path.strip_prefix('"') {
                // Keep the opening quote out of the replacement span, while
                // retaining enough information for the caller to close it.
                return Some((path.to_string(), token_start + 2));
            }
            return Some((path.to_string(), token_start + 1));
        }
        if token.starts_with('"') {
            // Quoted path (the opening quote is kept in the editor text).
            return Some((token[1..].to_string(), token_start + 1));
        }

        // Native pi only opens a regular path panel when the token looks like
        // a path. A bare word in prose must not cause a directory scan on
        // every cursor movement.
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

    /// Get completions for a path prefix.
    fn get_path_completions(&self, prefix: &str) -> Vec<AutocompleteItem> {
        let quoted = prefix.starts_with('"');
        let prefix = prefix.strip_prefix('"').unwrap_or(prefix);
        let prefix = prefix.strip_suffix('"').unwrap_or(prefix);
        let has_separator = prefix.contains('/') || prefix.contains('\\');

        // Determine the directory to search. Relative paths are scoped to the
        // provider root, just like native Pi's basePath; `~` is expanded only
        // for lookup and is kept in the text inserted by the caller.
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
        let raw_dir = expand_home_path(&raw_dir);
        let dir = if raw_dir.is_absolute() {
            raw_dir
        } else {
            self.root_path
                .clone()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(raw_dir)
        };

        let partial_lower = partial.to_lowercase();
        let mut entries = Vec::new();
        if let Ok(read_dir) = std::fs::read_dir(&dir) {
            for entry in read_dir.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !partial_lower.is_empty() && !name.to_lowercase().starts_with(&partial_lower) {
                    continue;
                }
                // Follow directory symlinks, matching native Pi's Dirent /
                // stat fallback instead of treating every symlink as a file.
                let is_dir =
                    entry.file_type().map(|t| t.is_dir()).unwrap_or(false) || entry.path().is_dir();
                entries.push((name, is_dir));
            }
        }

        // Native Pi presents directories first and then sorts alphabetically.
        entries.sort_by(|(left_name, left_dir), (right_name, right_dir)| {
            right_dir
                .cmp(left_dir)
                .then_with(|| left_name.to_lowercase().cmp(&right_name.to_lowercase()))
                .then_with(|| left_name.cmp(right_name))
        });
        entries.truncate(self.max_results);

        let display_base = if prefix.ends_with(['/', '\\']) {
            prefix.to_string()
        } else {
            prefix
                .rfind(['/', '\\'])
                .map(|index| prefix[..index + 1].to_string())
                .unwrap_or_default()
        };

        entries
            .into_iter()
            .map(|(name, is_dir)| {
                let display_path = format!("{display_base}{name}");
                let mut text = if is_dir {
                    format!("{name}/")
                } else {
                    name.clone()
                };
                if quoted && !is_dir {
                    text.push('"');
                }
                let mut item = AutocompleteItem::new(&text)
                    .with_description(&display_path)
                    .with_insert_space(!is_dir);
                if is_dir {
                    item = item.as_directory();
                }
                item
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
        let items = self.get_path_completions(&lookup_prefix);
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
        let texts: Vec<&str> = suggestions
            .items
            .iter()
            .map(|item| item.text.as_str())
            .collect();
        assert_eq!(
            texts,
            vec!["report.txt"],
            "a trailing separator must list the directory, not offer its own name"
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
        let texts: Vec<&str> = suggestions
            .items
            .iter()
            .map(|item| item.text.as_str())
            .collect();
        assert_eq!(texts, vec!["nested/"]);
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
        assert_eq!(suggestions.items[0].text, "report.txt");

        let relative = "@nested/rep";
        let suggestions = provider
            .get_suggestions(relative, relative.len())
            .expect("a relative @ path is scoped to the provider root");
        assert_eq!(suggestions.items[0].text, "report.txt");
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
