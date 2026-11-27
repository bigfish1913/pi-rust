//! Complete data-only registrations BEFORE a batch merge can discard metadata.
//! This describes registrations, not schemas, approval, or safe native code.
use super::{ExtensionRegistry, DEFAULT_PRIORITY};
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventRegistration {
    pub tag: u32,
    pub count: usize,
}

/// No function pointers, plugin user_data, active registry or code keepalives.
/// Inspect each plugin independently before legacy first-wins merging. Includes
/// declarations with no associated provider, which `absorb` otherwise discards.
/// This is NOT an artifact grant, tool-schema identity or callback settlement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistrationInventory {
    pub tools: Vec<String>,
    pub commands: Vec<String>,
    pub flags: Vec<String>,
    pub shortcuts: Vec<String>,
    pub event_handlers: Vec<EventRegistration>,
    pub resources_discover: usize,
    pub before_agent_start: usize,
    pub tool_result: usize,
    pub autocomplete: usize,
    pub argument_suggesters: Vec<String>,
    pub providers: Vec<String>,
    pub provider_manifests: Vec<String>,
    pub oauth_providers: usize,
    pub renderers: usize,
    pub priority: i32,
    pub platforms: Vec<String>,
    pub owns_stdio: bool,
}
impl Default for RegistrationInventory {
    fn default() -> Self {
        Self {
            tools: Vec::new(),
            commands: Vec::new(),
            flags: Vec::new(),
            shortcuts: Vec::new(),
            event_handlers: Vec::new(),
            resources_discover: 0,
            before_agent_start: 0,
            tool_result: 0,
            autocomplete: 0,
            argument_suggesters: Vec::new(),
            providers: Vec::new(),
            provider_manifests: Vec::new(),
            oauth_providers: 0,
            renderers: 0,
            priority: DEFAULT_PRIORITY,
            platforms: Vec::new(),
            owns_stdio: false,
        }
    }
}
impl ExtensionRegistry {
    pub fn registration_inventory(&self) -> RegistrationInventory {
        RegistrationInventory {
            tools: self
                .tools
                .iter()
                .map(|item| item.tool.name.clone())
                .collect(),
            commands: self.commands.iter().map(|item| item.name.clone()).collect(),
            flags: self.flags.iter().map(|item| item.name.clone()).collect(),
            shortcuts: self.shortcuts.iter().map(|item| item.key.clone()).collect(),
            event_handlers: self
                .handlers
                .iter()
                .enumerate()
                .filter(|(_, items)| !items.is_empty())
                .map(|(tag, items)| EventRegistration {
                    tag: tag as u32,
                    count: items.len(),
                })
                .collect(),
            resources_discover: self.resources_discover.len(),
            before_agent_start: self.before_agent_start.len(),
            tool_result: self.tool_result.len(),
            autocomplete: self.autocomplete.len(),
            argument_suggesters: self
                .argument_suggesters
                .iter()
                .map(|item| item.command.clone())
                .collect(),
            providers: self
                .providers
                .iter()
                .map(|item| item.provider_id.clone())
                .collect(),
            provider_manifests: self.provider_manifests.keys().cloned().collect(),
            oauth_providers: self.oauth_providers.len(),
            renderers: self.renderers.len(),
            priority: self.priority,
            platforms: self.platforms.clone(),
            owns_stdio: self.owns_stdio,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_is_complete_data_and_keeps_unbound_declarations_before_merge() {
        let mut registry = ExtensionRegistry::new();
        assert_eq!(
            registry.registration_inventory(),
            RegistrationInventory::default()
        );
        assert!(registry.apply_declaration_json(r#"{"priority":7,"platforms":["linux"],"ownsStdio":true,"providers":[{"id":"unbound","models":[]}]}"#));
        assert!(!registry.register_flag("flag".into(), "description".into()));
        let inventory = registry.registration_inventory();
        assert_eq!(inventory.priority, 7);
        assert_eq!(inventory.platforms, ["linux"]);
        assert!(inventory.owns_stdio);
        assert_eq!(inventory.flags, ["flag"]);
        assert_eq!(inventory.provider_manifests, ["unbound"]);
        assert!(inventory.providers.is_empty());
        let serialized = serde_json::to_value(&inventory).unwrap();
        assert_eq!(serialized.as_object().unwrap().len(), 17);
        assert_eq!(
            serialized["providerManifests"],
            serde_json::json!(["unbound"])
        );
        registry.invalidate();
        // A data copy neither becomes stale nor keeps registry/code alive.
        assert_eq!(inventory.priority, 7);
        let mut merged = ExtensionRegistry::new();
        merged.absorb(registry);
        assert!(merged
            .registration_inventory()
            .provider_manifests
            .is_empty());
        assert_eq!(inventory.provider_manifests, ["unbound"]);
    }
}
