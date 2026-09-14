//! Shared YAML-frontmatter parser for the skills and prompt-template loaders.
//!
//! The TS duplicates `parseFrontmatter` verbatim in both `skills.ts` and
//! `prompt-templates.ts`; consolidated here to avoid the duplication. The TS
//! parses the fenced YAML block with the `yaml` npm package. This port uses a
//! **minimal YAML-subset parser** instead of pulling a `serde_yaml` dependency
//! (consistent with the workspace's minimal-dep posture): it handles the
//! frontmatter shapes that actually occur — `key: value` lines with string,
//! boolean, integer, null, short flow-collection `[..]`/`{..}` values, and
//! indentation-based block sequences/mappings — and flags unterminated flow
//! collections / quotes as parse errors so malformed frontmatter (e.g.
//! `description: [unterminated`) produces a `parse_failed` diagnostic exactly
//! like the TS `yaml` parse would. Full YAML (anchors, aliases, multiline
//! scalars, multi-doc) is out of scope; see `docs/m5e-open-questions.md`.

use serde_json::{Map, Value};

/// Parse a `---\n...\n---` frontmatter block. Returns `(frontmatter_object,
/// body)` on success, or `Err(message)` when the fenced YAML is malformed.
///
/// Mirrors TS `parseFrontmatter`:
/// - CRLF/CR normalized to LF.
/// - No leading `---` → empty frontmatter, body = the whole normalized content
///   (NOT trimmed — matches TS).
/// - No closing `\n---` → same as no-frontmatter.
/// - Otherwise yaml = between the fences; body = after the closing fence,
///   trimmed.
pub(crate) fn parse_frontmatter(content: &str) -> Result<(Value, String), String> {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    if !normalized.starts_with("---") {
        return Ok((Value::Object(Map::new()), normalized));
    }
    // Search for the closing `\n---` after the opening fence.
    let Some(end_rel) = normalized[3..].find("\n---") else {
        return Ok((Value::Object(Map::new()), normalized));
    };
    let end_index = end_rel + 3; // index of the `\n` in `\n---`
                                 // `slice(4, endIndex)` in TS; empty when endIndex < 4.
    let yaml_string = normalized.get(4..end_index).unwrap_or("");
    // `slice(endIndex + 4)` skips `\n---`; `.trim()`.
    let body = normalized
        .get(end_index + 4..)
        .unwrap_or("")
        .trim()
        .to_string();
    let frontmatter = parse_simple_yaml(yaml_string)?;
    Ok((frontmatter, body))
}

#[derive(Debug, Clone, Copy)]
struct YamlLine<'a> {
    line_no: usize,
    indent: usize,
    text: &'a str,
}

/// Minimal YAML-subset parser.
///
/// The first implementation treated every non-empty line as a top-level
/// mapping entry. That worked for the original local skills, but rejected
/// standard skill frontmatter such as:
///
/// ```yaml
/// triggers:
///   - "flowchart"
/// metadata:
///   author: example
/// ```
///
/// We keep the parser deliberately small, but make it indentation-aware so
/// block sequences and nested mappings are represented as JSON arrays/objects.
fn parse_simple_yaml(input: &str) -> Result<Value, String> {
    let lines: Vec<YamlLine<'_>> = input
        .lines()
        .enumerate()
        .filter_map(|(idx, raw_line)| {
            let trimmed = raw_line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                return None;
            }
            // YAML indentation in the skill files is spaces. Leave tabs in the
            // text so they produce the same useful parse error as other invalid
            // mapping lines instead of silently accepting mixed indentation.
            let indent = raw_line.bytes().take_while(|b| *b == b' ').count();
            Some(YamlLine {
                line_no: idx + 1,
                indent,
                text: &raw_line[indent..],
            })
        })
        .collect();

    if lines.is_empty() {
        return Ok(Value::Object(Map::new()));
    }

    let root_indent = lines[0].indent;
    let (value, next) = parse_yaml_block(&lines, 0, root_indent)?;
    if next != lines.len() {
        // A line with less indentation can only occur after the root block. It
        // is invalid YAML, and reporting the line keeps diagnostics actionable.
        let line = lines[next];
        return Err(format!("line {}: unexpected indentation", line.line_no));
    }
    if !value.is_object() {
        // Historically frontmatter was required to be a top-level mapping. Keep
        // that contract even though nested sequence values are now supported.
        return Err(format!("line {}: missing ':'", lines[0].line_no));
    }
    Ok(value)
}

fn parse_yaml_block(
    lines: &[YamlLine<'_>],
    start: usize,
    indent: usize,
) -> Result<(Value, usize), String> {
    let Some(line) = lines.get(start) else {
        return Ok((Value::Null, start));
    };
    if line.indent != indent {
        return Err(format!("line {}: unexpected indentation", line.line_no));
    }
    if is_sequence_item(line.text) {
        parse_yaml_sequence(lines, start, indent)
    } else {
        parse_yaml_mapping(lines, start, indent)
    }
}

fn parse_yaml_mapping(
    lines: &[YamlLine<'_>],
    mut index: usize,
    indent: usize,
) -> Result<(Value, usize), String> {
    let mut map = Map::new();

    while let Some(line) = lines.get(index) {
        if line.indent < indent {
            break;
        }
        if line.indent > indent {
            return Err(format!("line {}: unexpected indentation", line.line_no));
        }

        let Some(colon) = find_mapping_colon(line.text) else {
            return Err(format!("line {}: missing ':'", line.line_no));
        };
        let key = line.text[..colon].trim().to_string();
        if key.is_empty() {
            return Err(format!("line {}: empty key", line.line_no));
        }
        let value_str = line.text[colon + 1..].trim();
        index += 1;

        let value = if value_str.is_empty() {
            if let Some(child) = lines.get(index).filter(|child| child.indent > indent) {
                let (value, next) = parse_yaml_block(lines, index, child.indent)?;
                index = next;
                value
            } else {
                Value::Null
            }
        } else {
            parse_yaml_value(value_str, line.line_no)?
        };
        map.insert(key, value);
    }

    Ok((Value::Object(map), index))
}

fn parse_yaml_sequence(
    lines: &[YamlLine<'_>],
    mut index: usize,
    indent: usize,
) -> Result<(Value, usize), String> {
    let mut values = Vec::new();

    while let Some(line) = lines.get(index) {
        if line.indent < indent {
            break;
        }
        if line.indent > indent {
            return Err(format!("line {}: unexpected indentation", line.line_no));
        }
        if !is_sequence_item(line.text) {
            break;
        }

        let item_text = sequence_item_text(line.text);
        index += 1;
        let value = if item_text.is_empty() {
            if let Some(child) = lines.get(index).filter(|child| child.indent > indent) {
                let (value, next) = parse_yaml_block(lines, index, child.indent)?;
                index = next;
                value
            } else {
                Value::Null
            }
        } else if let Some(colon) = find_sequence_mapping_colon(item_text) {
            // Support the common YAML form `- key: value`, including mapping
            // continuation lines indented beneath the sequence item.
            let mut item_map = Map::new();
            let key = item_text[..colon].trim().to_string();
            if key.is_empty() {
                return Err(format!("line {}: empty key", line.line_no));
            }
            let item_value_str = item_text[colon + 1..].trim();
            let item_value = if item_value_str.is_empty() {
                if let Some(child) = lines.get(index).filter(|child| child.indent > indent) {
                    let (value, next) = parse_yaml_block(lines, index, child.indent)?;
                    index = next;
                    value
                } else {
                    Value::Null
                }
            } else {
                parse_yaml_value(item_value_str, line.line_no)?
            };
            item_map.insert(key, item_value);

            // `- key: value` may be followed by `  other: value` fields. Parse
            // and merge that continuation mapping when present.
            if let Some(continuation) = lines.get(index).filter(|next| next.indent > indent) {
                let (continuation_value, next) =
                    parse_yaml_mapping(lines, index, continuation.indent)?;
                index = next;
                if let Value::Object(fields) = continuation_value {
                    item_map.extend(fields);
                }
            }
            Value::Object(item_map)
        } else {
            let value = parse_yaml_value(item_text, line.line_no)?;
            if let Some(next) = lines.get(index).filter(|next| next.indent > indent) {
                return Err(format!("line {}: unexpected indentation", next.line_no));
            }
            value
        };
        values.push(value);
    }

    Ok((Value::Array(values), index))
}

fn is_sequence_item(text: &str) -> bool {
    text == "-" || text.starts_with("- ")
}

fn sequence_item_text(text: &str) -> &str {
    text.strip_prefix('-').unwrap_or(text).trim()
}

/// Find the key/value separator for a mapping entry. Colons inside a quoted or
/// flow scalar do not introduce a mapping, which matters for sequence items
/// such as `- "https://example.test/path"`.
fn find_mapping_colon(text: &str) -> Option<usize> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('"')
        || trimmed.starts_with('\'')
        || trimmed.starts_with('[')
        || trimmed.starts_with('{')
    {
        return None;
    }
    text.find(':')
}

/// Sequence items use the stricter YAML separator rule (`key: value`). This
/// keeps an unquoted URL such as `- https://example.test` as a scalar instead
/// of interpreting `https` as a mapping key. Top-level mappings continue to
/// use `find_mapping_colon` for backwards compatibility with the old parser.
fn find_sequence_mapping_colon(text: &str) -> Option<usize> {
    let colon = find_mapping_colon(text)?;
    let after = text[colon + 1..].chars().next();
    if after.is_none() || after.is_some_and(char::is_whitespace) {
        Some(colon)
    } else {
        None
    }
}

fn parse_yaml_value(s: &str, line_no: usize) -> Result<Value, String> {
    if s.is_empty() {
        return Ok(Value::Null);
    }
    if let Some(rest) = s.strip_prefix('[') {
        return if let Some(inner) = rest.strip_suffix(']') {
            let items: Vec<Value> = if inner.trim().is_empty() {
                Vec::new()
            } else {
                inner.split(',').map(|p| parse_scalar(p.trim())).collect()
            };
            Ok(Value::Array(items))
        } else {
            Err(format!("line {line_no}: unterminated flow sequence"))
        };
    }
    if let Some(rest) = s.strip_prefix('{') {
        return if let Some(inner) = rest.strip_suffix('}') {
            let mut m = Map::new();
            if !inner.trim().is_empty() {
                for part in inner.split(',') {
                    let Some(c) = part.find(':') else {
                        return Err(format!("line {line_no}: malformed flow mapping entry"));
                    };
                    let k = part[..c].trim().to_string();
                    let v = parse_scalar(part[c + 1..].trim());
                    m.insert(k, v);
                }
            }
            Ok(Value::Object(m))
        } else {
            Err(format!("line {line_no}: unterminated flow mapping"))
        };
    }
    if let Some(rest) = s.strip_prefix('"') {
        return match find_quote_end(rest, '"') {
            Some(end) => Ok(Value::String(unescape_double(&rest[..end]))),
            None => Err(format!("line {line_no}: unterminated double-quoted string")),
        };
    }
    if let Some(rest) = s.strip_prefix('\'') {
        return match find_quote_end(rest, '\'') {
            Some(end) => Ok(Value::String(rest[..end].to_string())),
            None => Err(format!("line {line_no}: unterminated single-quoted string")),
        };
    }
    Ok(parse_scalar(s))
}

/// Parse a bare scalar (no flow collection, no quote): bool / null / number /
/// string. Mirrors YAML 1.1 core schema for the common atoms.
fn parse_scalar(s: &str) -> Value {
    match s {
        "true" | "True" | "TRUE" => Value::Bool(true),
        "false" | "False" | "FALSE" => Value::Bool(false),
        "null" | "Null" | "NULL" | "~" => Value::Null,
        _ => {
            if let Ok(i) = s.parse::<i64>() {
                return Value::from(i);
            }
            if let Ok(f) = s.parse::<f64>() {
                if f.is_finite() && s.contains('.') {
                    return Value::from(f);
                }
            }
            Value::String(s.to_string())
        }
    }
}

fn find_quote_end(rest: &str, quote: char) -> Option<usize> {
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if quote == '"' && c == '\\' {
            i += 2; // skip escaped char
            continue;
        }
        if c == quote {
            // single-quote: `''` is an escaped quote
            if quote == '\'' && i + 1 < bytes.len() && bytes[i + 1] as char == '\'' {
                i += 2;
                continue;
            }
            return Some(i);
        }
        i += 1;
    }
    None
}

fn unescape_double(s: &str) -> String {
    s.replace("\\\"", "\"")
        .replace("\\\\", "\\")
        .replace("\\n", "\n")
        .replace("\\t", "\t")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_frontmatter_returns_whole_body_untrimmed() {
        let (fm, body) = parse_frontmatter("First line\nBody\n").unwrap();
        assert!(fm.as_object().unwrap().is_empty());
        assert_eq!(body, "First line\nBody\n");
    }

    #[test]
    fn parses_string_and_bool_fields() {
        let content = "---\nname: example\ndescription: Example skill\ndisable-model-invocation: true\n---\nUse this skill.\n";
        let (fm, body) = parse_frontmatter(content).unwrap();
        let o = fm.as_object().unwrap();
        assert_eq!(o.get("name").and_then(|v| v.as_str()), Some("example"));
        assert_eq!(
            o.get("description").and_then(|v| v.as_str()),
            Some("Example skill")
        );
        assert_eq!(
            o.get("disable-model-invocation").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(body, "Use this skill.");
    }

    #[test]
    fn empty_value_is_null() {
        let (fm, _) = parse_frontmatter("---\ndescription:\n---\nbody").unwrap();
        assert!(fm.get("description").unwrap().is_null());
    }

    #[test]
    fn unterminated_flow_sequence_is_parse_error() {
        let content = "---\ndescription: [unterminated\n---\nBody";
        assert!(parse_frontmatter(content).is_err());
    }

    #[test]
    fn flow_array_parses() {
        let (fm, _) = parse_frontmatter("---\ntags: [a, b, c]\n---\nx").unwrap();
        let arr = fm.get("tags").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0].as_str(), Some("a"));
    }

    #[test]
    fn parses_ai_animation_block_sequence_frontmatter() {
        let content = r#"---
name: "flowchart"
description: "生成教育/科普类流程图、概念图、原理演示的动画 HTML 页面。"
version: "0.2.0"
triggers:
  - "流程图"
  - "概念图"
  - "原理演示"
  - "flowchart"
  - "flow diagram"
---
# Flowchart Skill
"#;
        let (fm, body) = parse_frontmatter(content).unwrap();
        assert_eq!(fm.get("name").and_then(Value::as_str), Some("flowchart"));
        assert_eq!(fm.get("version").and_then(Value::as_str), Some("0.2.0"));
        let triggers = fm.get("triggers").and_then(Value::as_array).unwrap();
        assert_eq!(triggers.len(), 5);
        assert_eq!(triggers[0].as_str(), Some("流程图"));
        assert_eq!(triggers[4].as_str(), Some("flow diagram"));
        assert_eq!(body, "# Flowchart Skill");
    }

    #[test]
    fn parses_nested_metadata_mapping() {
        let content = r#"---
name: "dynamic-archify"
description: "Create professional architecture diagrams"
version: "2.6"
license: MIT
triggers:
  - "架构图"
  - "dynamic-archify"
metadata:
  author: tt-a1i
  based_on: Cocoon-AI/architecture-diagram-generator (MIT, v1.0)
---
Body
"#;
        let (fm, _) = parse_frontmatter(content).unwrap();
        let metadata = fm.get("metadata").and_then(Value::as_object).unwrap();
        assert_eq!(
            metadata.get("author").and_then(Value::as_str),
            Some("tt-a1i")
        );
        assert_eq!(
            metadata.get("based_on").and_then(Value::as_str),
            Some("Cocoon-AI/architecture-diagram-generator (MIT, v1.0)")
        );
    }

    #[test]
    fn parses_sequence_of_nested_mappings() {
        let content = "---\nitems:\n  - name: first\n    enabled: true\n  - name: second\n    enabled: false\n---\nbody";
        let (fm, _) = parse_frontmatter(content).unwrap();
        let items = fm.get("items").and_then(Value::as_array).unwrap();
        assert_eq!(items[0].get("name").and_then(Value::as_str), Some("first"));
        assert_eq!(items[0].get("enabled").and_then(Value::as_bool), Some(true));
        assert_eq!(items[1].get("name").and_then(Value::as_str), Some("second"));
        assert_eq!(
            items[1].get("enabled").and_then(Value::as_bool),
            Some(false)
        );
    }

    #[test]
    fn keeps_unquoted_urls_in_block_sequences_as_scalars() {
        let content = "---\nlinks:\n  - https://example.test/docs\n  - http://localhost:5231/preview\n---\nbody";
        let (fm, _) = parse_frontmatter(content).unwrap();
        let links = fm.get("links").and_then(Value::as_array).unwrap();
        assert_eq!(links[0].as_str(), Some("https://example.test/docs"));
        assert_eq!(links[1].as_str(), Some("http://localhost:5231/preview"));
    }

    #[test]
    fn missing_closing_fence_treated_as_no_frontmatter() {
        let (fm, body) = parse_frontmatter("---\nname: x\nbody still here").unwrap();
        assert!(fm.as_object().unwrap().is_empty());
        assert!(body.contains("name: x"));
    }
}
