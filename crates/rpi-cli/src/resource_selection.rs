//! Opt-in native startup resource selection. This is a host contract, not a package format.
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock, Weak};

use rpi_harness::agent_harness::AgentHarness;
use rpi_harness::session::types::SessionStorage;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::args::{Args, Mode};

pub const MAX_SELECTION_BYTES: usize = 64 * 1024;
pub const MAX_STATE_BYTES: usize = 256 * 1024;
const SESSION_KEY: &str = "rpi.resource-selection.v1";

/// Only public generic append APIs are restricted. Native SessionValueWriter
/// remains available for the host's atomic declaration/inventory persistence.
pub(crate) fn validate_public_entry(custom_type: &str, data: Option<&Value>) -> Result<(), String> {
    if custom_type == rpi_harness::session::values::VALUE_CUSTOM_TYPE
        && data
            .and_then(|d| d.get("key"))
            .and_then(Value::as_str)
            .is_some_and(|key| {
                key.starts_with("rpi.resource-selection.")
                    || key.starts_with("rpi.resource-inventory.")
            })
    {
        return Err(
            "host-owned resource session values cannot be appended through public APIs".into(),
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceSelection {
    pub version: u32,
    pub main_package: Option<MainPackage>,
    #[serde(default)]
    pub skills: Vec<PathBuf>,
    #[serde(default)]
    pub extensions: Vec<PathBuf>,
    #[serde(default)]
    pub instructions: Vec<PathBuf>,
    pub tools: ToolSelection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MainPackage {
    pub name: String,
    /// Explicit package-relative persistent instruction files. Prompts are never auto-executed.
    #[serde(default)]
    pub instructions: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolSelection {
    Default {},
    Only { names: Vec<String> },
    None {},
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedPackage {
    pub name: String,
    pub version: String,
    pub source: String,
    pub cargo_package: String,
    pub root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ResolvedSelection {
    pub package: Option<ResolvedPackage>,
    pub selection: ResourceSelection,
    pub skills: Vec<PathBuf>,
    pub extensions: Vec<PathBuf>,
    pub instructions: Vec<(PathBuf, String, String)>,
    validated_skills: Option<ValidatedSkills>,
}

#[derive(Debug, Clone)]
pub(crate) struct ValidatedSkills {
    pub skills: Vec<rpi_harness::types::Skill>,
    inventory: Value,
}

fn contained(path: &Path, root: &Path) -> Result<PathBuf, String> {
    let path = std::fs::canonicalize(path).map_err(|e| e.to_string())?;
    if !path.starts_with(root) || path == root {
        return Err(format!(
            "package resource escapes canonical package root: {}",
            path.display()
        ));
    }
    Ok(path)
}

// Validate nested symlinks/junctions too, before feeding package directories to
// the existing recursive loader. Explicit standalone sources are caller-authorized.
fn validate_package_tree(
    path: &Path,
    root: &Path,
    visited: &mut HashSet<PathBuf>,
) -> Result<(), String> {
    let path = contained(path, root)?;
    if path.is_dir() {
        if !visited.insert(path.clone()) {
            return Err("package resource directory cycle/alias".into());
        }
        for entry in std::fs::read_dir(path).map_err(|e| e.to_string())? {
            validate_package_tree(&entry.map_err(|e| e.to_string())?.path(), root, visited)?;
        }
    }
    Ok(())
}

fn read_limited(path: &Path) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .take((MAX_SELECTION_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > MAX_SELECTION_BYTES {
        return Err(format!("resource input exceeds 64 KiB: {}", path.display()));
    }
    Ok(bytes)
}
fn fingerprint(path: &Path) -> Result<String, String> {
    let mut reader = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hash = Sha256::new();
    let mut buf = [0; 8192];
    loop {
        let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn file(path: &Path) -> Result<PathBuf, String> {
    if !path.is_file() {
        return Err(format!("missing resource file: {}", path.display()));
    }
    std::fs::canonicalize(path).map_err(|e| e.to_string())
}
fn unique_paths(paths: &[PathBuf]) -> Result<(), String> {
    let mut seen = HashSet::new();
    for path in paths {
        if !seen.insert(path) {
            return Err(format!("duplicate resource: {}", path.display()));
        }
    }
    Ok(())
}
fn relative(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(format!(
            "expected a package-relative path: {}",
            path.display()
        ));
    }
    Ok(())
}

impl ResolvedSelection {
    pub fn load(args: &Args, cwd: &Path, trusted: bool) -> Result<Option<Self>, String> {
        let Some(config) = &args.resource_config else {
            return Ok(None);
        };
        if args.mode != Mode::Rpc
            || args.dev_local_only
            || args.project_only
            || args.trust_override == Some(false)
            || !trusted
            || args.system_prompt.is_some()
            || !args.append_system_prompt.is_empty()
            || args.tools.is_some()
            || args.exclude_tools.is_some()
            || args.no_tools
            || args.no_builtin_tools
            || args.no_skills
            || args.no_prompt_templates
            || args.no_extensions
            || args.no_context_files
            || !args.skill.is_empty()
            || !args.prompt_template.is_empty()
            || !args.extension.is_empty()
            || !args.extensions_dir.is_empty()
            || !args.unknown_flags.is_empty()
        {
            return Err("--resource-config requires trusted --mode rpc and conflicts with resource/tool/prompt/discovery/dev flags and unknown extension flags".into());
        }
        let bytes = read_limited(&cwd.join(config))?;
        let selection: ResourceSelection =
            serde_json::from_slice(&bytes).map_err(|e| format!("invalid resource config: {e}"))?;
        if selection.version != 1 {
            return Err("unsupported resource config version".into());
        }
        if let ToolSelection::Only { names } = &selection.tools {
            let mut seen = HashSet::new();
            for name in names {
                if name.is_empty() || !seen.insert(name) {
                    return Err("empty or duplicate tool name".into());
                }
            }
        }
        let mut skills = Vec::new();
        let mut extensions = Vec::new();
        let mut instructions = Vec::new();
        let mut resolved_package = None;
        if let Some(main) = &selection.main_package {
            let package = crate::packages::selected_project_package(cwd, &main.name)?;
            let packages_root =
                std::fs::canonicalize(cwd.join(".rpi/packages")).map_err(|e| e.to_string())?;
            let root = std::fs::canonicalize(packages_root.join(&main.name))
                .map_err(|e| format!("missing package directory: {e}"))?;
            if !root.is_dir() || !root.starts_with(&packages_root) || root == packages_root {
                return Err("package root escapes .rpi/packages".into());
            }
            let skill_dir = root.join("skills");
            if skill_dir.exists() {
                let skill_dir = contained(&skill_dir, &root)?;
                validate_package_tree(&skill_dir, &root, &mut HashSet::new())?;
                skills.push(skill_dir);
            }
            for artifact in &package.artifacts {
                relative(Path::new(artifact))?;
                extensions.push(contained(
                    &file(&root.join("extensions").join(artifact))?,
                    &root,
                )?);
            }
            for instruction in &main.instructions {
                relative(instruction)?;
                let path = contained(&file(&root.join(instruction))?, &root)?;
                instructions.push((path, format!("package:{}", main.name), String::new()));
            }
            resolved_package = Some(ResolvedPackage {
                name: package.name,
                version: package.version,
                source: package.source,
                cargo_package: package.cargo_package,
                root,
            });
        }
        // Explicit additions first: use the existing first-registration-wins loader.
        let mut additions = Vec::new();
        for skill in &selection.skills {
            let path = cwd.join(skill);
            if !path.exists() {
                return Err(format!("missing skill: {}", path.display()));
            }
            additions.push(std::fs::canonicalize(path).map_err(|e| e.to_string())?);
        }
        additions.extend(skills);
        skills = additions;
        for extension in &selection.extensions {
            extensions.push(file(&cwd.join(extension))?);
        }
        for instruction in &selection.instructions {
            instructions.push((
                file(&cwd.join(instruction))?,
                "explicit".into(),
                String::new(),
            ));
        }
        unique_paths(&skills)?;
        unique_paths(&extensions)?;
        unique_paths(&instructions.iter().map(|i| i.0.clone()).collect::<Vec<_>>())?;
        for extension in &extensions {
            if extension.extension().and_then(|e| e.to_str())
                != Some(std::env::consts::DLL_EXTENSION)
            {
                return Err(format!("not a native extension: {}", extension.display()));
            }
        }
        for (path, _, text) in &mut instructions {
            *text = String::from_utf8(read_limited(path)?)
                .map_err(|e| format!("instruction {}: {e}", path.display()))?;
        }
        Ok(Some(Self {
            package: resolved_package,
            selection,
            skills,
            extensions,
            instructions,
            validated_skills: None,
        }))
    }

    /// Resolve once, before code or native writers. Hash the very bytes parsed
    /// by the existing loader, preserving ignore/recursion and first-wins rules.
    pub(crate) async fn validate_skills(
        &mut self,
        env: &std::sync::Arc<dyn rpi_tools::ExecutionEnv>,
    ) -> Result<(), String> {
        let hashes = Mutex::new(std::collections::HashMap::new());
        let observe = |path: &str, raw: &str| {
            hashes.lock().unwrap().insert(
                path.to_owned(),
                format!("{:x}", Sha256::digest(raw.as_bytes())),
            );
        };
        let mut skills = Vec::new();
        let mut diagnostics = Vec::new();
        for path in &self.skills {
            let loaded = rpi_harness::skills::load_skills_with_source_observer(
                env,
                &[path.to_string_lossy().into_owned()],
                &observe,
            )
            .await;
            if loaded.skills.is_empty() {
                return Err(format!(
                    "selected skill source loaded no Skills: {}",
                    path.display()
                ));
            }
            skills.extend(loaded.skills);
            diagnostics.extend(loaded.diagnostics);
        }
        let skills = crate::resource_dirs::dedupe_skills(skills, &mut diagnostics);
        if !diagnostics.is_empty() {
            return Err(format!("selected skill diagnostics: {diagnostics:?}"));
        }
        let hashes = hashes.into_inner().unwrap();
        let inventory = json!(skills
            .iter()
            .map(|skill| json!({
                "name": skill.name, "path": skill.file_path, "sha256": hashes[&skill.file_path]
            }))
            .collect::<Vec<_>>());
        self.validated_skills = Some(ValidatedSkills { skills, inventory });
        Ok(())
    }

    pub(crate) fn validated_skills(&self) -> Result<&ValidatedSkills, String> {
        self.validated_skills
            .as_ref()
            .ok_or_else(|| "selected Skills were not preflight validated".into())
    }

    pub fn active_tools(&self, registered: &[String]) -> Result<Vec<String>, String> {
        match &self.selection.tools {
            ToolSelection::Default {} => Ok(registered.to_vec()),
            ToolSelection::None {} => Ok(Vec::new()),
            ToolSelection::Only { names } => {
                for name in names {
                    if !registered.contains(name) {
                        return Err(format!("unknown selected tool: {name}"));
                    }
                }
                Ok(names.clone())
            }
        }
    }

    pub fn provenance(&self) -> Value {
        json!(self.instructions.iter().map(|(path, source, text)| json!({"path": path, "source": source, "sha256": format!("{:x}",Sha256::digest(text.as_bytes()))})).collect::<Vec<_>>())
    }
}

// CLI attachment keyed by allocation, not durable session ID: concurrent hosts reopening
// the same transcript must not inherit each other's startup observations.
type Attachment = (Weak<dyn SessionStorage>, Value);
fn states() -> &'static Mutex<Vec<Attachment>> {
    static STATES: OnceLock<Mutex<Vec<Attachment>>> = OnceLock::new();
    STATES.get_or_init(|| Mutex::new(Vec::new()))
}
pub(crate) fn attach(harness: &AgentHarness, state: Value) {
    let mut states = states().lock().unwrap();
    states.retain(|(storage, _)| storage.strong_count() > 0);
    states.push((
        std::sync::Arc::downgrade(&harness.session().storage()),
        state,
    ));
}

pub async fn get_resource_state(harness: &AgentHarness) -> Result<Value, String> {
    let storage = harness.session().storage();
    let state = states().lock().unwrap().iter().find_map(|(s, state)| {
        s.upgrade()
            .filter(|s| std::sync::Arc::ptr_eq(s, &storage))
            .map(|_| state.clone())
    });
    let mut state = state.ok_or("get_resource_state requires --resource-config v1")?;
    let metadata = harness
        .session()
        .get_metadata()
        .await
        .map_err(|e| e.to_string())?;
    let resources = harness.get_resources().await.map_err(|e| e.to_string())?;
    let registered: Vec<_> = harness
        .get_tools()
        .await
        .map_err(|e| e.to_string())?
        .iter()
        .map(|t| t.tool.schema().name.clone())
        .collect();
    let execution = harness.execution_context().map_err(|e| e.to_string())?;
    let active = execution["tools"].clone();
    let read = active
        .as_array()
        .is_some_and(|names| names.iter().any(|n| n == "read"));
    state["sessionId"] = json!(metadata.id);
    state["registeredTools"] = json!(registered);
    state["modelEffectiveTools"] = active;
    state["loadedSkills"] = json!(resources.skills.unwrap_or_default().iter().map(|s| json!({
        "name": s.name, "path": s.file_path, "modelVisible": read && !s.disable_model_invocation.unwrap_or(false)
    })).collect::<Vec<_>>());
    state["promptTemplates"] = json!(resources
        .prompt_templates
        .unwrap_or_default()
        .iter()
        .map(|t| &t.name)
        .collect::<Vec<_>>());
    if serde_json::to_vec(&state).map_err(|e| e.to_string())?.len() > MAX_STATE_BYTES {
        return Err("resource snapshot exceeds 256 KiB".into());
    }
    Ok(state)
}

fn declaration(s: &ResolvedSelection) -> Value {
    json!({"selection": s.selection, "package": s.package, "skills": s.skills, "extensions": s.extensions, "instructions": s.provenance()})
}

pub(crate) async fn bind_session(
    session: &rpi_harness::session::Session,
    selected: Option<&ResolvedSelection>,
) -> Result<(), String> {
    use rpi_harness::session::SessionValues;
    let values = SessionValues::load(session)
        .await
        .map_err(|e| e.to_string())?;
    validate_session_binding(&values, selected)
}

/// Shared read-only check for preflight facts and the accepted writer's session.
pub(crate) fn validate_session_binding(
    values: &rpi_harness::session::SessionValues,
    selected: Option<&ResolvedSelection>,
) -> Result<(), String> {
    match (values.get(SESSION_KEY), selected) {
        (Some(old), Some(selected)) if old.get("declaration") == Some(&declaration(selected)) => {
            // Code identity is checked before dlopen/registration, not just after
            // the schema registry has been constructed. No provider callback runs here.
            let extensions = selected.extensions.iter().map(|path| Ok(json!({"path":path,"sha256":fingerprint(path)?})))
                .collect::<Result<Vec<_>, String>>()?;
            if old.pointer("/inventory/extensions") != Some(&json!(extensions)) {
                return Err("session resource artifact inventory changed; restore original resources or use a new session".into());
            }
            if old.pointer("/inventory/skills") != Some(&selected.validated_skills()?.inventory) {
                return Err("session resource Skill inventory changed; restore original resources or use a new session".into());
            }
            Ok(())
        },
        (Some(_), _) => Err("session resource selection differs or is missing; restart with the same declared selection or use a new session".into()),
        (None, _) => Ok(()),
    }
}

/// Reopening observes the same files/registrations, not merely the same JSON.
/// Native values remain in the native transcript, outside model-visible history.
pub(crate) async fn bind_inventory(
    harness: &AgentHarness,
    selected: &ResolvedSelection,
) -> Result<(), String> {
    use rpi_harness::session::{SessionValueWriter, SessionValues};
    let resources = harness.get_resources().await.map_err(|e| e.to_string())?;
    let mut skills = Vec::new();
    for skill in resources.skills.unwrap_or_default() {
        skills.push(json!({"name": skill.name, "path": skill.file_path, "sha256": fingerprint(Path::new(&skill.file_path))?}));
    }
    // Defense in depth: neither a registration-time file edit nor a different
    // constructed resource set may bind source bytes other than those parsed.
    if json!(skills) != selected.validated_skills()?.inventory {
        return Err("session resource Skill inventory changed after preflight".into());
    }
    let mut extensions = Vec::new();
    for path in &selected.extensions {
        extensions.push(json!({"path": path, "sha256": fingerprint(path)?}));
    }
    let tools: Vec<_> = harness
        .get_tools()
        .await
        .map_err(|e| e.to_string())?
        .iter()
        .map(|t| json!(t.tool.schema()))
        .collect();
    let inventory = json!({"skills":skills, "extensions":extensions, "toolSchemasSha256":format!("{:x}",Sha256::digest(serde_json::to_vec(&tools).map_err(|e| e.to_string())?))});
    let session = harness.session();
    let values = SessionValues::load(session)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(old) = values.get(SESSION_KEY) {
        if old.get("inventory") != Some(&inventory)
            || old.get("declaration") != Some(&declaration(selected))
        {
            return Err(
                "session resource inventory changed; use original resources or a new session"
                    .into(),
            );
        }
    } else {
        // One native append binds declaration and observed inventory together.
        SessionValueWriter::new(session.clone())
            .set(
                SESSION_KEY,
                json!({"declaration": declaration(selected), "inventory": inventory}),
            )
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn typed_schema_rejects_unknown_and_duplicate_fields_at_each_level() {
        for input in [
            r#"{"version":1,"tools":{"mode":"none","unexpected":1}}"#,
            r#"{"version":1,"tools":{"mode":"default","names":[]}}"#,
            r#"{"version":1,"main_package":{"name":"x","unexpected":1},"tools":{"mode":"none"}}"#,
            r#"{"version":1,"version":1,"tools":{"mode":"none"}}"#,
            r#"{"version":1,"tools":{"mode":"only","names":[],"unexpected":1}}"#,
        ] {
            assert!(
                serde_json::from_str::<ResourceSelection>(input).is_err(),
                "{input}"
            );
        }
        for path in ["../outside.md", "./instruction.md", ""] {
            assert!(relative(Path::new(path)).is_err());
        }
    }

    #[test]
    fn configuration_instruction_and_registry_reads_are_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let input = root.join("input.md");
        std::fs::write(&input, vec![b'x'; MAX_SELECTION_BYTES]).unwrap();
        assert_eq!(read_limited(&input).unwrap().len(), MAX_SELECTION_BYTES);
        std::fs::write(&input, vec![b'x'; MAX_SELECTION_BYTES + 1]).unwrap();
        assert!(read_limited(&input).unwrap_err().contains("exceeds 64 KiB"));
        std::fs::create_dir(root.join(".rpi")).unwrap();
        std::fs::write(
            root.join(".rpi/packages.json"),
            vec![b' '; MAX_STATE_BYTES + 1],
        )
        .unwrap();
        assert!(crate::packages::selected_project_package(root, "valid")
            .unwrap_err()
            .contains("exceeds 256 KiB"));
    }

    #[tokio::test]
    async fn validated_skills_keep_parsed_content_and_exact_source_hash() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("review")).unwrap();
        let path = root.join("review/SKILL.md");
        let raw = "---\r\nname: review\r\ndescription: Review fixture\r\n---\r\nOriginal body\r\n";
        std::fs::write(&path, raw).unwrap();
        std::fs::write(
            root.join("resources.json"),
            r#"{"version":1,"skills":["review"],"tools":{"mode":"none"}}"#,
        )
        .unwrap();
        let args = Args {
            mode: Mode::Rpc,
            resource_config: Some("resources.json".into()),
            ..Default::default()
        };
        let mut selected = ResolvedSelection::load(&args, root, true).unwrap().unwrap();
        let env: std::sync::Arc<dyn rpi_tools::ExecutionEnv> =
            std::sync::Arc::new(rpi_tools::OsExecutionEnv::with_cwd(root.to_path_buf()));
        selected.validate_skills(&env).await.unwrap();
        let validated = selected.validated_skills().unwrap();
        assert_eq!(
            validated.inventory[0]["sha256"],
            format!("{:x}", Sha256::digest(raw.as_bytes()))
        );
        assert_eq!(validated.skills[0].name, "review");
        let content = validated.skills[0].content.clone();
        // Construction must retain these parsed Skills, not do a weaker second
        // read/discovery after native code has had a chance to alter the file.
        std::fs::write(&path, "---\nname: [\n---\nMalformed replacement").unwrap();
        assert_eq!(
            selected.validated_skills().unwrap().skills[0].content,
            content
        );
        assert_ne!(
            selected.validated_skills().unwrap().inventory[0]["sha256"],
            fingerprint(&path).unwrap()
        );
    }

    #[tokio::test]
    async fn overlapping_skill_roots_fail_closed_without_validated_inventory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir(root.join("review")).unwrap();
        std::fs::write(
            root.join("review/SKILL.md"),
            "---\nname: review\ndescription: Review fixture\n---\nValid body\n",
        )
        .unwrap();
        std::fs::write(
            root.join("resources.json"),
            r#"{"version":1,"skills":["review","review/SKILL.md"],"tools":{"mode":"none"}}"#,
        )
        .unwrap();
        let args = Args {
            mode: Mode::Rpc,
            resource_config: Some("resources.json".into()),
            ..Default::default()
        };
        // Distinct overlapping roots pass declaration-level path uniqueness,
        // but repeat a Skill name even though both reads have the same path.
        let mut selected = ResolvedSelection::load(&args, root, true).unwrap().unwrap();
        let env: std::sync::Arc<dyn rpi_tools::ExecutionEnv> =
            std::sync::Arc::new(rpi_tools::OsExecutionEnv::with_cwd(root.to_path_buf()));
        let error = selected.validate_skills(&env).await.unwrap_err();
        assert!(error.contains("selected skill diagnostics"), "{error}");
        assert!(error.contains("shadowed"), "{error}");
        assert!(error.contains("first-registration wins"), "{error}");
        assert!(selected.validated_skills().is_err());
    }

    #[tokio::test]
    async fn snapshot_errors_without_exact_mode_and_when_oversized() {
        let harness = AgentHarness::create(Default::default()).await.unwrap();
        assert!(get_resource_state(&harness)
            .await
            .unwrap_err()
            .contains("requires --resource-config"));
        harness.seal_resources(vec![]).unwrap();
        attach(&harness, json!({"oversized": "x".repeat(MAX_STATE_BYTES)}));
        assert!(get_resource_state(&harness)
            .await
            .unwrap_err()
            .contains("exceeds 256 KiB"));
    }
}
