//! File, trust and native-extension adapters for the shared model runtime.
use crate::{config, settings};
use rpi_ai::{Model, ThinkingLevel};
use rpi_runtime::provider::RuntimeFactories;
pub use rpi_runtime::provider::{
    authenticated_catalog, available_catalog, ResolveError, ResolvedModel, ANTHROPIC_API_KEY_ENV,
    ANTHROPIC_AUTH_TOKEN_ENV, ANTHROPIC_BASE_URL_ENV, DEFAULT_MODEL_ID, DEFAULT_THINKING_LEVEL,
    NO_API_KEY_HINT, OPENAI_API_KEY_ENV,
};
use std::sync::Arc;

pub(crate) struct CliValues;
impl rpi_runtime::ValueSource for CliValues {
    fn env(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
    fn command(&self, command: &str, cached: bool) -> Option<String> {
        if cached {
            config::resolve_command(command)
        } else {
            config::resolve_command_uncached(command)
        }
    }
}
pub(crate) fn declaration(
    manifest: &rpi_extensions::ProviderManifest,
) -> rpi_runtime::ProviderDeclaration {
    rpi_runtime::ProviderDeclaration {
        id: manifest.id.clone(),
        models: manifest.models.clone(),
        api_key_env: manifest.api_key_env.clone(),
        requires_auth: manifest.requires_auth,
    }
}
fn runtime_settings(settings: settings::Settings) -> rpi_runtime::RuntimeSettings {
    let timeout = settings.http_idle_timeout_ms();
    rpi_runtime::RuntimeSettings {
        default_provider: settings.default_provider,
        default_model: settings.default_model,
        default_thinking_level: settings.default_thinking_level,
        theme: settings.theme,
        http_idle_timeout_ms: timeout,
    }
}
fn resolve_with_factories(
    provider: Option<&str>,
    model: Option<&str>,
    thinking: Option<ThinkingLevel>,
    api_key: Option<&str>,
    base_url: Option<&str>,
    settings: settings::Settings,
    factories: &RuntimeFactories,
) -> Result<ResolvedModel, ResolveError> {
    rpi_runtime::RuntimeConfig {
        models: config::load_models_config()?,
        credentials: config::read_auth()?,
        settings: runtime_settings(settings),
    }
    .resolve(
        &rpi_runtime::ModelRequest {
            provider,
            model,
            thinking,
            api_key,
            base_url,
            ..Default::default()
        },
        factories,
        &CliValues,
    )
}
pub fn resolve(
    cli_provider: Option<&str>,
    cli_model: Option<&str>,
    cli_thinking: Option<ThinkingLevel>,
    cli_api_key: Option<&str>,
    cli_base_url: Option<&str>,
) -> Result<ResolvedModel, ResolveError> {
    let settings = settings::load_settings().unwrap_or_default();
    resolve_with_settings(
        cli_provider,
        cli_model,
        cli_thinking,
        cli_api_key,
        cli_base_url,
        settings,
    )
}

/// Resolve using upstream's global -> trusted-project settings precedence.
pub fn resolve_for_cwd(
    cli_provider: Option<&str>,
    cli_model: Option<&str>,
    cli_thinking: Option<ThinkingLevel>,
    cli_api_key: Option<&str>,
    cli_base_url: Option<&str>,
    cwd: &std::path::Path,
    project_trusted: bool,
) -> Result<ResolvedModel, ResolveError> {
    let settings = settings::load_effective_model_settings(cwd, project_trusted)?;
    resolve_with_settings(
        cli_provider,
        cli_model,
        cli_thinking,
        cli_api_key,
        cli_base_url,
        settings,
    )
}

fn resolve_with_settings(
    cli_provider: Option<&str>,
    cli_model: Option<&str>,
    cli_thinking: Option<ThinkingLevel>,
    cli_api_key: Option<&str>,
    cli_base_url: Option<&str>,
    settings: settings::Settings,
) -> Result<ResolvedModel, ResolveError> {
    resolve_with_factories(
        cli_provider,
        cli_model,
        cli_thinking,
        cli_api_key,
        cli_base_url,
        settings,
        &RuntimeFactories::default(),
    )
}

pub fn resolve_with_extensions(
    args: &crate::args::Args,
    cwd: &std::path::Path,
    trusted: bool,
    session: &rpi_extensions::ExtensionSession,
) -> Result<ResolvedModel, ResolveError> {
    let settings = settings::load_effective_model_settings(cwd, trusted)?;
    resolve_with_factories(
        args.provider.as_deref(),
        args.model.as_deref(),
        args.thinking,
        args.api_key.as_deref(),
        args.base_url.as_deref(),
        settings,
        &factories_from_session(session),
    )
}

fn factories_from_session(session: &rpi_extensions::ExtensionSession) -> RuntimeFactories {
    let mut factories = RuntimeFactories::default();
    if let Some(refresher) = crate::auth::PackageOAuthRefresher::new(session) {
        factories.set_oauth_refresher(Arc::new(refresher));
    }
    if let Some(snapshot) = session.snapshot() {
        for manifest in manifests_from_session(session) {
            factories.declare(declaration(&manifest));
        }
        for provider in rpi_extensions::PluggableProvider::from_session(
            session,
            tokio::runtime::Handle::current(),
        ) {
            if let Some(record) = snapshot
                .providers()
                .iter()
                .find(|record| record.provider_id == provider.id())
            {
                let api =
                    serde_json::from_value(serde_json::Value::String(record.api_style.clone()))
                        .expect("API strings include custom protocols");
                factories.register(provider, api);
            }
        }
    }
    factories
}
pub fn catalog_all() -> Result<Vec<Model>, config::ConfigError> {
    catalog_with_manifests(&[])
}
pub fn manifests_from_session(
    session: &rpi_extensions::ExtensionSession,
) -> Vec<rpi_extensions::ProviderManifest> {
    session.snapshot().map_or_else(Vec::new, |snapshot| {
        snapshot
            .providers()
            .iter()
            .filter_map(|provider| snapshot.provider_manifest(&provider.provider_id).cloned())
            .collect()
    })
}

pub fn catalog_with_manifests(
    manifests: &[rpi_extensions::ProviderManifest],
) -> Result<Vec<Model>, config::ConfigError> {
    catalog_with_diagnostics(manifests).map(|(models, _)| models)
}
pub(crate) fn catalog_with_diagnostics(
    manifests: &[rpi_extensions::ProviderManifest],
) -> Result<(Vec<Model>, Vec<String>), config::ConfigError> {
    let declarations: Vec<_> = manifests.iter().map(declaration).collect();
    Ok(rpi_runtime::provider::catalog_with_diagnostics(
        config::load_models_config()?,
        &declarations,
        &CliValues,
    ))
}

#[cfg(test)]
mod tests;
