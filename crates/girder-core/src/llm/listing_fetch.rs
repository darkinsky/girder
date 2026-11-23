//! The driver [`crate::llm::listing_cache`] was waiting for.
//!
//! The cache decides when a `/models` load is due and whether its answer may be published. This
//! module does the load: one GET, the bearer credential, and [`parse_model_listing`].

use std::time::Duration;

use girder_core::llm::deepseek_files::{ModelListing, parse_model_listing};
use girder_core::llm::deepseek_options::ListingSettings;
use girder_core::llm::listing_cache::{Access, ListingCacheState};
use girder_core::llm::provider_catalog::listing_url;
use girder_core::now_millis;

/// How long one listing request may take when the route did not say.
const DEFAULT_TIMEOUT_MS: f64 = 10_000.0;

/// How old a listing may be before the next access fetches again.
const DEFAULT_MAX_AGE_MS: u64 = 300_000;

/// Fetches `/models` once through the listing cache and returns the published ids.
///
/// # Errors
///
/// Returns the transport or decode message. A missing credential is an error here: the caller that
/// has no key should not call this.
pub async fn fetch_models(
    cache: &mut ListingCacheState<ModelListing>,
    settings: &ListingSettings,
    credential: &str,
) -> Result<Vec<String>, String> {
    let now = now_millis();
    let load = match cache.access(settings, Some(credential), now) {
        Access::Closed => return Err("the model listing cache is closed".to_owned()),
        Access::Unauthenticated => return Err("the model listing has no credential".to_owned()),
        Access::Wait { load } | Access::Immediate { refresh: load } => load,
    };
    let Some(load) = load else {
        return Ok(ids_of(cache.current().listing));
    };

    let timeout = Duration::from_millis(load.timeout_ms.max(1.0) as u64);
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|error| error.to_string())?;
    let response = client
        .get(&load.url)
        .bearer_auth(credential)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    let body = response.text().await.map_err(|error| error.to_string())?;
    let outcome = if !status.is_success() {
        Err(format!("GET {} returned {status}: {body}", load.url))
    } else {
        match serde_json::from_str(&body) {
            Ok(value) => parse_model_listing(&value).map_err(|error| error.to_string()),
            Err(error) => Err(error.to_string()),
        }
    };
    let failed = outcome.as_ref().err().cloned();
    let url = load.url.clone();
    let settlement = cache.settle(load.generation, outcome, &url, settings, now_millis());
    if let Some(message) = failed {
        return Err(settlement.warning.unwrap_or(message));
    }
    Ok(ids_of(cache.current().listing))
}

/// Reads the deployment's key and base URL, fetches `/models`, and prints how many came back.
///
/// No key is success and prints nothing: an offline prompt is the normal case.
///
/// # Errors
///
/// Returns the fetch failure. The caller prints it and continues; a listing outage does not stop a turn.
pub async fn refresh_from_env() -> Result<(), String> {
    let Some((credential, base)) = credential_and_base() else {
        return Ok(());
    };
    let settings = ListingSettings {
        url: base,
        timeout_ms: DEFAULT_TIMEOUT_MS,
        max_age_ms: DEFAULT_MAX_AGE_MS,
    };
    let mut cache = ListingCacheState::new();
    let ids = fetch_models(&mut cache, &settings, &credential).await?;
    let shown: Vec<&str> = ids.iter().take(8).map(String::as_str).collect();
    eprintln!(
        "[models] {} from {} ({})",
        ids.len(),
        settings.url,
        shown.join(", ")
    );
    Ok(())
}

fn credential_and_base() -> Option<(String, String)> {
    // The same choice `provider_from_env` makes. A listing that used a different key or a different
    // origin would report models the turn cannot call.
    let protocol = std::env::var("GIRDER_PROTOCOL").unwrap_or_else(|_| "anthropic".to_owned());
    if protocol == "openai" {
        let key = std::env::var("OPENAI_API_KEY")
            .or_else(|_| std::env::var("GIRDER_OPENAI_API_KEY"))
            .ok()?;
        if key.trim().is_empty() {
            return None;
        }
        let base = std::env::var("OPENAI_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_owned());
        return Some((key, listing_url(&base, "openai-completions")));
    }
    let config = girder_core::llm::deepseek::DeepSeekConfig::from_env()?;
    Some((config.api_key, deepseek_models_url(&config.base_url)))
}

/// The OpenAI-compatible model list for a DeepSeek messages root.
///
/// Chat lives at `{origin}/anthropic/messages`. The list does not: it is `{origin}/models`. Asking
/// `{origin}/anthropic/models` is a 404, which is what a base of
/// `https://api.deepseek.com/anthropic` produced.
fn deepseek_models_url(base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    let root = trimmed
        .strip_suffix("/anthropic")
        .or_else(|| trimmed.strip_suffix("/v1"))
        .unwrap_or(trimmed);
    format!("{}/models", root.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::deepseek_models_url;

    #[test]
    fn an_anthropic_messages_root_lists_at_the_origin() {
        assert_eq!(
            deepseek_models_url("https://api.deepseek.com/anthropic"),
            "https://api.deepseek.com/models"
        );
        assert_eq!(
            deepseek_models_url("https://api.deepseek.com/v1/"),
            "https://api.deepseek.com/models"
        );
    }
}

fn ids_of(listing: Option<&ModelListing>) -> Vec<String> {
    listing
        .map(|listing| {
            listing
                .entries
                .iter()
                .map(|entry| entry.id.clone())
                .collect()
        })
        .unwrap_or_default()
}
