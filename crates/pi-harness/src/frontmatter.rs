//! Shared YAML-frontmatter parser for the skills and prompt-template loaders.
//!
//! The TS duplicates `parseFrontmatter` verbatim in both `skills.ts` and
//! `prompt-templates.ts`; consolidated here to avoid the duplication. The TS
//! parses the fenced YAML block with the `yaml` npm package. This port uses a
//! **minimal YAML-subset parser** instead of pulling a `serde_yaml` dependency
//! (consistent with the workspace's minimal-dep posture): it handles the
//! frontmatter shapes that actually occur — `key: value` lines with string,
//! boolean, integer, null, and short flow-collection `[..]`/`{..}` values — and
//! flags unterminated flow collections / quotes as parse errors so malformed
//! frontmatter (e.g. `description: [unterminated`) produces a `parse_failed`
//! diagnostic exactly like the TS `yaml` parse would. Full YAML (anchors, block
//! sequences, multi-doc) is out of scope; see `docs/m5e-open-questions.md`.

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

/// Minimal YAML-subset parser: a top-level mapping of `key: value` lines.
fn parse_simple_yaml(input: &str) -> Result<Value, String> {
    let mut map = Map::new();
    for (idx, raw_line) in input.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Full-line comment (YAML `#` at line start or after indentation). Inline
        // trailing comments are intentionally NOT stripped — they would break URL
        // values like `http://x/#frag` (see open-questions).
        if trimmed.starts_with('#') {
            continue;
        }
        let Some(colon) = raw_line.find(':') else {
            return Err(format!("line {line_no}: missing ':'"));
        };
        let key = raw_line[..colon].trim().to_string();
        if key.is_empty() {
            return Err(format!("line {line_no}: empty key"));
        }
        let value_str = raw_line[colon + 1..].trim();
        let value = parse_yaml_value(value_str, line_no)?;
        map.insert(key, value);
    }
    Ok(Value::Object(map))
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
    fn missing_closing_fence_treated_as_no_frontmatter() {
        let (fm, body) = parse_frontmatter("---\nname: x\nbody still here").unwrap();
        assert!(fm.as_object().unwrap().is_empty());
        assert!(body.contains("name: x"));
    }
}
