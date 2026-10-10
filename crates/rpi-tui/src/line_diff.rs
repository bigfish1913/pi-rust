//! Column patches for stable terminal rows, preserving graphemes and styles.
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(PartialEq)]
struct Atom<'a> {
    text: &'a str,
    width: usize,
}
fn atoms(line: &str) -> Option<Vec<Atom<'_>>> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset < line.len() {
        let tail = &line[offset..];
        if tail.starts_with('\x1b') {
            let bytes = tail.as_bytes();
            let end = if tail.starts_with("\x1b[") {
                let end = bytes
                    .iter()
                    .enumerate()
                    .skip(2)
                    .find(|(_, b)| (0x40..=0x7e).contains(*b))?
                    .0
                    + 1;
                if bytes[end - 1] != b'm' {
                    return None;
                }
                end
            } else if tail.starts_with("\x1b]8;") {
                let mut end = None;
                for i in 4..bytes.len() {
                    if bytes[i] == 7 {
                        end = Some(i + 1);
                        break;
                    }
                    if bytes[i] == 27 && bytes.get(i + 1) == Some(&b'\\') {
                        end = Some(i + 2);
                        break;
                    }
                }
                end?
            } else {
                return None;
            };
            result.push(Atom {
                text: &tail[..end],
                width: 0,
            });
            offset += end;
        } else {
            let end = tail.find('\x1b').unwrap_or(tail.len());
            for text in tail[..end].graphemes(true) {
                if text.chars().any(char::is_control) {
                    return None;
                }
                result.push(Atom {
                    text,
                    width: UnicodeWidthStr::width(text),
                });
            }
            offset += end;
        }
    }
    Some(result)
}
fn context(atoms: &[Atom<'_>]) -> String {
    let mut sgr = String::new();
    let mut link = String::new();
    for atom in atoms.iter().filter(|atom| atom.width == 0) {
        if atom.text.starts_with("\x1b[") {
            if matches!(atom.text, "\x1b[0m" | "\x1b[m") {
                sgr.clear();
            }
            sgr.push_str(atom.text);
        } else if atom.text.starts_with("\x1b]8;") {
            link = atom.text.into();
        }
    }
    format!("{sgr}{link}")
}
/// Return the first changed column and its replacement. Unsupported terminal
/// protocols fall back to the normal row writer rather than slicing controls.
pub(crate) fn patch(old: &str, new: &str) -> Option<(usize, String)> {
    let old = atoms(old)?;
    let new = atoms(new)?;
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let column = old[..prefix].iter().map(|a| a.width).sum();
    let old_width: usize = old.iter().map(|a| a.width).sum();
    let new_width: usize = new.iter().map(|a| a.width).sum();
    let mut suffix = 0;
    if old_width == new_width {
        suffix = old[prefix..]
            .iter()
            .rev()
            .zip(new[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        while suffix > 0
            && context(&old[..old.len() - suffix]) != context(&new[..new.len() - suffix])
        {
            if new[new.len() - suffix].width == 0 {
                suffix -= 1;
            } else {
                suffix = 0;
            }
        }
    }
    let mut text = context(&new[..prefix]);
    for atom in &new[prefix..new.len() - suffix] {
        text.push_str(atom.text);
    }
    text.push_str(&" ".repeat(old_width.saturating_sub(new_width)));
    text.push_str("\x1b[0m\x1b]8;;\x07");
    Some((column, text))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scrolling_chat_does_not_write_unchanged_sidebar() {
        let (column, text) = patch(
            "chat old │ \x1b[2mRUN STATS\x1b[22m",
            "chat new │ \x1b[2mRUN STATS\x1b[22m",
        )
        .unwrap();
        assert_eq!(column, 5);
        assert!(text.starts_with("new"));
        assert!(!text.contains("RUN STATS"));
        let (_, text) = patch(
            "\x1b[31mold\x1b[0m │ \x1b[2mRUN STATS\x1b[22m",
            "\x1b[32mnew\x1b[0m │ \x1b[2mRUN STATS\x1b[22m",
        )
        .unwrap();
        assert!(!text.contains("RUN STATS"));
        assert!(text.contains("\x1b[32mnew"));
        let (column, text) = patch("chat same │ TPS 123", "chat same │ TPS 456").unwrap();
        assert_eq!(column, 16);
        assert!(!text.contains("chat same"));
    }
    #[test]
    fn graphemes_styles_and_shrinking_lines_are_preserved() {
        let (column, text) = patch("中文 👩‍💻 old", "中文 👩‍💻 new").unwrap();
        assert_eq!(column, 8);
        assert!(text.starts_with("new"));
        let (_, text) = patch("\x1b[31mred\x1b[0m", "\x1b[32mred\x1b[0m").unwrap();
        assert!(text.contains("\x1b[32mred"));
        let (column, text) = patch("long text", "long").unwrap();
        assert_eq!(column, 4);
        assert!(text.starts_with("     "));
        assert!(patch("plain", "\x1b[2Junsafe").is_none());
    }
}
