//! Remote model catalog overlay.
//!
//! Port of native Pi's `packages/coding-agent/src/core/remote-catalog-provider.ts`.
//! A built-in provider ships a static model list; `pi.dev` publishes a
//! per-provider catalog (`/api/models/providers/<id>`) that can add or refresh
//! entries between releases. This module:
//!
//! * fetches that catalog with an `ETag`/`Last-Modified` validator,
//! * caches the body + validator under the agent dir,
//! * revalidates at most every [`REMOTE_CATALOG_REFRESH_INTERVAL_MS`] unless
//!   forced, and
//! * merges the overlay over the baseline (overlay entries win by id).

use std::collections::BTreeMap;
use std::path::PathBuf;

use rpi_ai::model::Model;
use serde::{Deserialize, Serialize};

/// Native `DEFAULT_CATALOG_BASE_URL`.
pub const DEFAULT_CATALOG_BASE_URL: &str = "https://pi.dev";

/// Native `REMOTE_CATALOG_REFRESH_INTERVAL_MS` (4 hours).
pub const REMOTE_CATALOG_REFRESH_INTERVAL_MS: i64 = 4 * 60 * 60 * 1000;

/// Persisted catalog entry for one provider (native `ModelsStoreEntry`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntry {
    #[serde(default)]
    pub models: Vec<Model>,
    /// Wall-clock ms of the last successful (or 304) check.
    #[serde(default)]
    pub checked_at: i64,
    /// `Last-Modified` of the fetched body (ms), or 0 when unknown.
    #[serde(default)]
    pub last_modified: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

/// Persisted store: provider id → entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogStore {
    #[serde(flatten)]
    pub providers: BTreeMap<String, CatalogEntry>,
}

impl CatalogStore {
    pub fn path() -> Result<PathBuf, String> {
        let dir = crate::config::agent_dir().map_err(|e| e.to_string())?;
        Ok(dir.join("model-catalog.json"))
    }

    /// Load the store; a missing or malformed file yields an empty store
    /// (catalog caching is best-effort and must never block startup).
    pub fn load() -> Self {
        Self::load_from(&Self::path().ok())
    }

    pub fn load_from(path: &Option<PathBuf>) -> Self {
        let Some(path) = path else {
            return Self::default();
        };
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, text).map_err(|e| e.to_string())
    }
}

/// Parse a catalog payload. Native `parseCatalog`: accepts a bare array,
/// `{models:[…]}`, or an object keyed by id. Every entry is stamped with the
/// requesting provider id.
pub fn parse_catalog(provider_id: &str, value: &serde_json::Value) -> Result<Vec<Model>, String> {
    let entries: Vec<serde_json::Value> = if let Some(array) = value.as_array() {
        array.clone()
    } else if let Some(models) = value.get("models").and_then(|m| m.as_array()) {
        models.clone()
    } else if let Some(object) = value.as_object() {
        object.values().cloned().collect()
    } else {
        return Err(format!(
            "Invalid model catalog for provider \"{provider_id}\""
        ));
    };

    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        if entry.get("id").is_none() {
            continue;
        }
        match serde_json::from_value::<Model>(entry) {
            Ok(mut model) => {
                model.provider = rpi_ai::types::ProviderId::from(provider_id);
                out.push(model);
            }
            Err(_) => continue,
        }
    }
    Ok(out)
}

/// Overlay selection (native `remoteModels`): an entry only contributes when it
/// is newer than the locally generated catalog, and a missing `last_modified`
/// on the entry means "unknown" and is treated as stale.
pub fn remote_models(entry: Option<&CatalogEntry>, local_generated_at: Option<i64>) -> Vec<Model> {
    let Some(entry) = entry else {
        return Vec::new();
    };
    if let Some(local) = local_generated_at {
        if entry.last_modified == 0 || entry.last_modified <= local {
            return Vec::new();
        }
    }
    entry.models.clone()
}

/// Merge overlay over baseline; overlay entries win by `id` (native `mergeModels`).
pub fn merge_models(baseline: &[Model], dynamic: &[Model]) -> Vec<Model> {
    let mut merged = baseline.to_vec();
    for model in dynamic {
        match merged.iter().position(|m| m.id == model.id) {
            Some(index) => merged[index] = model.clone(),
            None => merged.push(model.clone()),
        }
    }
    merged
}

/// Outcome of a refresh attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum RefreshOutcome {
    /// Network not allowed, or a fresh cache made the request unnecessary.
    Skipped,
    /// A `304 Not Modified` confirmed the cached overlay.
    NotModified,
    /// The catalog was refreshed (or explicitly absent: 404/501).
    Updated(usize),
}

/// Fetch and cache the remote catalog for `provider_id`.
///
/// `allow_network == false` returns [`RefreshOutcome::Skipped`] without any I/O.
/// `force` bypasses the freshness window. Returns the overlay models to merge.
pub async fn refresh_provider_catalog(
    provider_id: &str,
    base_url: &str,
    allow_network: bool,
    force: bool,
    local_generated_at: Option<i64>,
    store: &mut CatalogStore,
) -> Result<Vec<Model>, String> {
    let stored = store.providers.get(provider_id).cloned();

    // Serve the cached overlay first when the network is off or the cache is
    // still fresh.
    if !allow_network {
        return Ok(remote_models(stored.as_ref(), local_generated_at));
    }
    if !force {
        if let Some(entry) = &stored {
            let age = now_ms() - entry.checked_at;
            if entry.checked_at > 0 && age >= 0 && age < REMOTE_CATALOG_REFRESH_INTERVAL_MS {
                return Ok(remote_models(Some(entry), local_generated_at));
            }
        }
    }

    let url = format!(
        "{}/api/models/providers/{}",
        base_url.trim_end_matches('/'),
        urlencode(provider_id)
    );
    let validator = stored
        .as_ref()
        .filter(|e| !e.models.is_empty())
        .and_then(|e| e.etag.clone());

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("catalog HTTP client: {e}"))?;
    let mut request = client
        .get(&url)
        .header("accept", "application/json")
        .header("User-Agent", format!("rpi/{}", crate::VERSION));
    if let Some(etag) = &validator {
        request = request.header("if-none-match", etag);
    }

    let response = match request.send().await {
        Ok(r) => r,
        Err(error) => {
            // Transient failure: keep the cached body + validator.
            if let Some(entry) = stored.clone() {
                store.providers.insert(
                    provider_id.to_string(),
                    CatalogEntry {
                        checked_at: now_ms(),
                        ..entry
                    },
                );
                let _ = store.save();
            }
            return Err(format!(
                "Model catalog request failed for {provider_id}: {error}"
            ));
        }
    };

    let status = response.status();
    let checked_at = now_ms();

    if status.as_u16() == 304 {
        if let Some(mut entry) = stored.clone() {
            entry.checked_at = checked_at;
            store
                .providers
                .insert(provider_id.to_string(), entry.clone());
            let _ = store.save();
            return Ok(remote_models(Some(&entry), local_generated_at));
        }
    }

    if status.as_u16() == 404 || status.as_u16() == 501 {
        let mut entry = stored.unwrap_or_default();
        entry.checked_at = checked_at;
        entry.last_modified = 0;
        entry.etag = None;
        entry.models.clear();
        store.providers.insert(provider_id.to_string(), entry);
        let _ = store.save();
        return Ok(Vec::new());
    }

    if !status.is_success() {
        if let Some(entry) = stored {
            store.providers.insert(
                provider_id.to_string(),
                CatalogEntry {
                    checked_at,
                    ..entry
                },
            );
            let _ = store.save();
        }
        return Err(format!(
            "Model catalog request failed for {provider_id}: {}",
            status.as_u16()
        ));
    }

    let last_modified = response
        .headers()
        .get("last-modified")
        .and_then(|v| v.to_str().ok())
        .map(parse_http_date_ms)
        .unwrap_or(0);
    let etag = response
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("invalid catalog JSON for {provider_id}: {e}"))?;
    let models = parse_catalog(provider_id, &body)?;

    let entry = CatalogEntry {
        models: models.clone(),
        checked_at,
        last_modified,
        etag,
    };
    let overlay = remote_models(Some(&entry), local_generated_at);
    let count = overlay.len();
    store.providers.insert(provider_id.to_string(), entry);
    let _ = store.save();
    let _ = count;
    Ok(overlay)
}

/// Merge the remote overlay into a full catalog, provider by provider.
///
/// Best-effort: any provider whose refresh fails keeps its baseline entries.
/// `allow_network == false` serves the persisted cache only (no request).
pub async fn merge_into_catalog(
    baseline: Vec<Model>,
    allow_network: bool,
    local_generated_at: Option<i64>,
) -> Vec<Model> {
    use std::collections::BTreeSet;
    let providers: BTreeSet<String> = baseline.iter().map(|m| m.provider.to_string()).collect();
    if providers.is_empty() {
        return baseline;
    }
    let mut store = CatalogStore::load();
    let mut result = baseline;
    for provider_id in providers {
        let overlay = match refresh_provider_catalog(
            &provider_id,
            DEFAULT_CATALOG_BASE_URL,
            allow_network,
            false,
            local_generated_at,
            &mut store,
        )
        .await
        {
            Ok(models) => models,
            Err(_) => Vec::new(),
        };
        if !overlay.is_empty() {
            result = merge_models(&result, &overlay);
        }
    }
    result
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Percent-encode a path segment (provider ids are simple, but be safe).
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Parse an HTTP date (`Last-Modified`) into ms since epoch. Best-effort: any
/// format we don't understand yields 0 (treated as "unknown").
fn parse_http_date_ms(value: &str) -> i64 {
    // IMF-fixdate is the common case: "Sun, 06 Nov 1994 08:49:37 GMT".
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let parts: Vec<&str> = value.split_whitespace().collect();
    if parts.len() < 5 {
        return 0;
    }
    let day: i64 = match parts[1].parse() {
        Ok(d) => d,
        Err(_) => return 0,
    };
    let month = match MONTHS.iter().position(|m| *m == parts[2]) {
        Some(m) => m as i64,
        None => return 0,
    };
    let year: i64 = match parts[3].parse() {
        Ok(y) => y,
        Err(_) => return 0,
    };
    let time: Vec<&str> = parts[4].split(':').collect();
    if time.len() != 3 {
        return 0;
    }
    let (h, m, s): (i64, i64, i64) = match (time[0].parse(), time[1].parse(), time[2].parse()) {
        (Ok(h), Ok(m), Ok(s)) => (h, m, s),
        _ => return 0,
    };
    // Days since epoch (Howard Hinnant's algorithm); `month` is 0-based, the
    // algorithm expects 1-based March-anchored months.
    let month1 = month + 1;
    let y = if month1 <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month1 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    (days * 86_400 + h * 3600 + m * 60 + s) * 1000
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_ai::types::{Api, ModelCost, ProviderId};

    fn model(id: &str, provider: &str) -> Model {
        Model {
            id: id.to_string(),
            name: id.to_string(),
            api: Api::AnthropicMessages,
            provider: ProviderId::from(provider),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![],
            cost: ModelCost::default(),
            context_window: 0,
            max_tokens: 0,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    #[test]
    fn parse_array_and_object() {
        let array = serde_json::json!([{ "id": "m1", "name": "M1", "api": "anthropic-messages",
            "provider": "x", "baseUrl": "", "reasoning": false, "input": [],
            "cost": { "input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0 },
            "contextWindow": 0, "maxTokens": 0 }]);
        let parsed = parse_catalog("anthropic", &array).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].provider.to_string(), "anthropic");

        let wrapped = serde_json::json!({ "models": array });
        assert_eq!(parse_catalog("anthropic", &wrapped).unwrap().len(), 1);
    }

    #[test]
    fn merge_overlay_wins_by_id() {
        let base = vec![model("a", "p"), model("b", "p")];
        let overlay = vec![model("b", "p"), model("c", "p")];
        let merged = merge_models(&base, &overlay);
        assert_eq!(merged.len(), 3);
        assert_eq!(
            merged.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
    }

    #[test]
    fn remote_models_respects_local_generation() {
        let entry = CatalogEntry {
            models: vec![model("a", "p")],
            checked_at: 0,
            last_modified: 1_000,
            etag: None,
        };
        assert!(remote_models(Some(&entry), Some(2_000)).is_empty());
        assert_eq!(remote_models(Some(&entry), Some(500)).len(), 1);
        assert_eq!(remote_models(Some(&entry), None).len(), 1);
        assert!(remote_models(None, None).is_empty());
    }

    #[test]
    fn http_date_parses_imf_fixdate() {
        // 1994-11-06T08:49:37Z == 784111777 s
        assert_eq!(
            parse_http_date_ms("Sun, 06 Nov 1994 08:49:37 GMT"),
            784_111_777_000
        );
        assert_eq!(parse_http_date_ms("not a date"), 0);
    }
}
