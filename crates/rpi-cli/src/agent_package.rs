//! Published agent bundle identity and session-bound prompt commands.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::resource_management::Scope;
use crate::resource_selection::{contained, file, read_limited, relative, validate_tool_names};

mod distribution;
pub(crate) use distribution::{install, update};

const MANIFEST: &str = "rpi-package.json";
const MAX_COMMANDS: usize = 32;
const MAX_PARAMETERS: usize = 16;
const MAX_ARGUMENT_BYTES: usize = 4096;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    name: String,
    version: String,
    agent: Agent,
    #[serde(default)]
    commands: BTreeMap<String, Command>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Agent {
    instructions: Vec<PathBuf>,
    tools: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    description: String,
    kind: Kind,
    template: PathBuf,
    #[serde(default)]
    args: BTreeMap<String, Parameter>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Prompt,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameter {
    #[serde(rename = "type")]
    kind: ParameterType,
    default: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ParameterType {
    String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Commands {
    entries: BTreeMap<String, PromptCommand>,
    digest: Option<String>,
}

#[derive(Clone, Debug)]
struct PromptCommand {
    metadata: Value,
    defaults: BTreeMap<String, Option<String>>,
    parts: Vec<TemplatePart>,
}

#[derive(Clone, Debug)]
enum TemplatePart {
    Literal(String),
    Parameter(String),
}

fn identifier(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text.as_bytes()[0].is_ascii_lowercase()
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_".contains(&b))
}

fn package_file(root: &Path, path: &Path) -> Result<PathBuf, String> {
    relative(path)?;
    contained(&file(&root.join(path))?, root)
}

impl Manifest {
    fn read(root: &Path, name: &str, version: Option<&str>) -> Result<(Self, Vec<u8>), String> {
        let bytes = read_limited(&package_file(root, Path::new(MANIFEST))?)?;
        let manifest: Self =
            serde_json::from_slice(&bytes).map_err(|e| format!("invalid {MANIFEST}: {e}"))?;
        if manifest.schema_version != 1
            || !identifier(name)
            || manifest.name != name
            || semver::Version::parse(&manifest.version).is_err()
            || version.is_some_and(|version| manifest.version != version)
            || manifest.commands.len() > MAX_COMMANDS
        {
            return Err(format!(
                "invalid {MANIFEST} schema, package identity, or command count"
            ));
        }
        validate_tool_names(&manifest.agent.tools)
            .map_err(|error| format!("invalid {MANIFEST}: {error}"))?;
        for path in &manifest.agent.instructions {
            package_file(root, path)?;
        }
        Ok((manifest, bytes))
    }
}

impl Commands {
    pub(crate) fn load(root: &Path, name: &str, version: Option<&str>) -> Result<Self, String> {
        match std::fs::symlink_metadata(root.join(MANIFEST)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(error) => return Err(error.to_string()),
            Ok(_) => {}
        }
        let (manifest, bytes) = Manifest::read(root, name, version)?;
        let mut hash = Sha256::new();
        hash.update(Sha256::digest(bytes));
        let mut entries = BTreeMap::new();
        for (name, command) in manifest.commands {
            let Kind::Prompt = command.kind;
            if !identifier(&name)
                || command.description.trim().is_empty()
                || command.description.len() > 512
                || command.args.len() > MAX_PARAMETERS
            {
                return Err(format!("invalid command metadata: {name}"));
            }
            let mut defaults = BTreeMap::new();
            let mut schema = BTreeMap::new();
            for (name, parameter) in command.args {
                let ParameterType::String = parameter.kind;
                if !identifier(&name)
                    || parameter
                        .default
                        .as_ref()
                        .is_some_and(|text| text.len() > MAX_ARGUMENT_BYTES)
                {
                    return Err(format!("invalid command parameter: {name}"));
                }
                schema.insert(
                    name.clone(),
                    if let Some(default) = &parameter.default {
                        json!({"type":"string", "default":default})
                    } else {
                        json!({"type":"string", "required":true})
                    },
                );
                defaults.insert(name, parameter.default);
            }
            let template = read_limited(&package_file(root, &command.template)?)?;
            hash.update(Sha256::digest(&template));
            let template = String::from_utf8(template).map_err(|e| e.to_string())?;
            let parts = template_parts(&template, &defaults)?;
            let metadata = json!({"name":name, "description":command.description, "kind":"prompt", "args":schema});
            entries.insert(
                name,
                PromptCommand {
                    metadata,
                    defaults,
                    parts,
                },
            );
        }
        let digest = (!entries.is_empty()).then(|| format!("{:x}", hash.finalize()));
        let commands = Self { entries, digest };
        if serde_json::to_vec(&commands.list())
            .map_err(|error| error.to_string())?
            .len()
            > crate::resource_selection::MAX_SELECTION_BYTES
        {
            return Err("package command metadata exceeds the size limit".into());
        }
        Ok(commands)
    }

    pub(crate) fn digest(&self) -> Option<&str> {
        self.digest.as_deref()
    }

    pub(crate) fn list(&self) -> Value {
        json!({"commands": self.entries.values().map(|command| &command.metadata).collect::<Vec<_>>()})
    }

    pub(crate) fn render(&self, request: &Value) -> Result<String, String> {
        let name = request
            .get("command")
            .and_then(Value::as_str)
            .ok_or("missing command name")?;
        let command = self.entries.get(name).ok_or("unknown package command")?;
        let empty = serde_json::Map::new();
        let args = match request.get("args") {
            Some(value) => value
                .as_object()
                .ok_or("package command args must be an object")?,
            None => &empty,
        };
        if args.keys().any(|name| !command.defaults.contains_key(name)) {
            return Err("unknown package command argument".into());
        }
        let values = command
            .defaults
            .iter()
            .map(|(name, default)| {
                let text = match args.get(name) {
                    Some(value) => value
                        .as_str()
                        .ok_or_else(|| format!("argument {name} must be a string"))?,
                    None => default
                        .as_deref()
                        .ok_or_else(|| format!("missing required argument {name}"))?,
                };
                if text.len() > MAX_ARGUMENT_BYTES {
                    return Err(format!("argument {name} exceeds the size limit"));
                }
                Ok((name.as_str(), json!(text).to_string()))
            })
            .collect::<Result<BTreeMap<_, _>, String>>()?;
        let mut rendered = String::new();
        for part in &command.parts {
            let text = match part {
                TemplatePart::Literal(text) => text,
                TemplatePart::Parameter(name) => &values[name.as_str()],
            };
            if rendered.len() + text.len() > crate::resource_selection::MAX_SELECTION_BYTES {
                return Err("rendered command exceeds the size limit".into());
            }
            rendered.push_str(text);
        }
        Ok(rendered)
    }
}

fn template_parts(
    mut text: &str,
    args: &BTreeMap<String, Option<String>>,
) -> Result<Vec<TemplatePart>, String> {
    if text.trim().is_empty() {
        return Err("empty package command template".into());
    }
    let mut parts = Vec::new();
    while let Some((before, rest)) = text.split_once("{{") {
        let (name, after) = rest.split_once("}}").ok_or("unclosed template parameter")?;
        if !args.contains_key(name) {
            return Err(format!("unknown template parameter: {name}"));
        }
        parts.push(TemplatePart::Literal(before.into()));
        parts.push(TemplatePart::Parameter(name.into()));
        text = after;
    }
    parts.push(TemplatePart::Literal(text.into()));
    Ok(parts)
}

pub(crate) struct Launch {
    pub config: tempfile::NamedTempFile,
    pub session_dir: PathBuf,
    _lease: crate::resource_management::Lease,
}

pub(crate) fn prepare_launch(name: &str, cwd: &Path) -> Result<Launch, String> {
    if !identifier(name) {
        return Err("invalid agent package name".into());
    }
    let home = Scope::Global.root(&crate::config::AgentHome::process(), cwd)?;
    let lease = crate::resource_management::shared_lease(&home)?;
    let record = crate::packages::selected_package_at(&home, name)?;
    let packages = home
        .join("packages")
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let root = contained(&packages.join(name), &packages)?;
    let (manifest, _) = Manifest::read(&root, name, Some(&record.version))?;
    let mut config = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
    serde_json::to_writer(config.as_file_mut(), &json!({"version":1,
        "main_package":{"name":name,"scope":"global","instructions":manifest.agent.instructions},
        "tools":{"mode":"only","names":manifest.agent.tools}
    })).map_err(|e| e.to_string())?;
    let cwd = cwd.canonicalize().map_err(|e| e.to_string())?;
    let project = format!("{:x}", Sha256::digest(cwd.as_os_str().as_encoded_bytes()));
    let session_dir = home
        .join("package-sessions")
        .join(name)
        .join(manifest.version)
        .join(project);
    Ok(Launch {
        config,
        session_dir,
        _lease: lease,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_rejects_empty_and_duplicate_tool_names() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        for (tools, valid) in [
            (vec![], true),
            (vec!["read", "bash"], true),
            (vec![""], false),
            (vec!["read", "read"], false),
        ] {
            let manifest = json!({
                "schema_version": 1,
                "name": "fixture",
                "version": "0.1.0",
                "agent": {"instructions": [], "tools": tools},
            });
            std::fs::write(root.join(MANIFEST), manifest.to_string()).unwrap();
            let result = Manifest::read(&root, "fixture", Some("0.1.0"));
            if valid {
                assert!(result.is_ok());
            } else {
                assert_eq!(
                    result.err().as_deref(),
                    Some("invalid rpi-package.json: empty or duplicate tool name")
                );
            }
        }
    }
}
