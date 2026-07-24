//! Minimal models.dev catalog support for chat model pricing.
//!
//! The wire types intentionally contain only the fields required to attach the
//! default token rates to models discovered by a harness. Provider-specific
//! metadata, pricing tiers, and experimental modes remain opaque because the
//! current chat API can only represent one flat rate per model.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

use futures_util::StreamExt as _;
use reportify::Report;
use serde::Deserialize;
use serde_json::Number;
use sha2::Digest as _;
use sha2::Sha256;
use tascarrel_api::types::chats::ChatHarnessKind;
use tascarrel_api::types::chats::ChatModel;
use tascarrel_api::types::chats::ChatModelPricing;
use tascarrel_api::types::common::Money;
use thiserror::Error;

const MODELS_DEV_URL: &str = "https://models.dev/api.json";
const MAX_CATALOG_BYTES: usize = 8 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const TOKEN_COUNT: u64 = 1_000_000_000_000;
const RATE_DECIMAL_PLACES: i32 = 8;

/// A fetched catalog together with the bytes to persist as the durable cache.
pub(crate) struct FetchedModelPricingCatalog {
    /// Parsed catalog ready to publish to harness state.
    pub(crate) catalog: ModelPricingCatalog,
    /// Original models.dev response used to construct the catalog version.
    pub(crate) bytes: Vec<u8>,
}

/// Immutable default pricing for the providers exposed by Tascarrel.
#[derive(Clone)]
pub(crate) struct ModelPricingCatalog {
    version: String,
    openai: HashMap<String, ChatModelPricing>,
    anthropic: HashMap<String, ChatModelPricing>,
}

impl ModelPricingCatalog {
    /// Loads a previously fetched catalog, if one exists.
    pub(crate) fn load(path: &Path) -> Result<Option<Self>, Report<ModelPricingCatalogError>> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(Report::new(ModelPricingCatalogError::ReadCache(error)));
            }
        };
        if bytes.len() > MAX_CATALOG_BYTES {
            return Err(Report::new(ModelPricingCatalogError::CatalogTooLarge));
        }
        Self::parse(&bytes).map(Some)
    }

    /// Fetches and validates the current models.dev catalog.
    pub(crate) async fn fetch()
    -> Result<FetchedModelPricingCatalog, Report<ModelPricingCatalogError>> {
        let response = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| Report::new(ModelPricingCatalogError::Request(error)))?
            .get(MODELS_DEV_URL)
            .send()
            .await
            .map_err(|error| Report::new(ModelPricingCatalogError::Request(error)))?
            .error_for_status()
            .map_err(|error| Report::new(ModelPricingCatalogError::Request(error)))?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_CATALOG_BYTES as u64)
        {
            return Err(Report::new(ModelPricingCatalogError::CatalogTooLarge));
        }

        let mut bytes = Vec::with_capacity(
            response
                .content_length()
                .and_then(|length| usize::try_from(length).ok())
                .unwrap_or_default(),
        );
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|error| Report::new(ModelPricingCatalogError::Request(error)))?;
            if bytes.len().saturating_add(chunk.len()) > MAX_CATALOG_BYTES {
                return Err(Report::new(ModelPricingCatalogError::CatalogTooLarge));
            }
            bytes.extend_from_slice(&chunk);
        }

        let catalog = Self::parse(&bytes)?;
        Ok(FetchedModelPricingCatalog { catalog, bytes })
    }

    /// Atomically replaces the durable cached response.
    pub(crate) async fn persist(
        path: &Path,
        bytes: &[u8],
    ) -> Result<(), Report<ModelPricingCatalogError>> {
        let next = path.with_extension("json.next");
        tokio::fs::write(&next, bytes)
            .await
            .map_err(|error| Report::new(ModelPricingCatalogError::WriteCache(error)))?;
        tokio::fs::rename(&next, path)
            .await
            .map_err(|error| Report::new(ModelPricingCatalogError::WriteCache(error)))
    }

    /// Applies the catalog's default rate to every known model in place.
    pub(crate) fn apply(&self, kind: &ChatHarnessKind, models: &mut [ChatModel]) {
        let provider = match kind {
            ChatHarnessKind::Codex => &self.openai,
            ChatHarnessKind::ClaudeCode => &self.anthropic,
        };
        for model in models {
            let id = model.id.as_ref();
            model.pricing = provider
                .get(id)
                .or_else(|| id.split_once('[').and_then(|(id, _)| provider.get(id)))
                .cloned();
        }
    }

    /// Returns the immutable identifier derived from the source response.
    pub(crate) fn version(&self) -> &str {
        &self.version
    }

    fn parse(bytes: &[u8]) -> Result<Self, Report<ModelPricingCatalogError>> {
        let source: ModelsDevCatalog = serde_json::from_slice(bytes)
            .map_err(|error| Report::new(ModelPricingCatalogError::Decode(error)))?;
        let version = format!("models.dev:sha256:{:x}", Sha256::digest(bytes));
        Ok(Self {
            openai: convert_provider(&source.openai, &version)?,
            anthropic: convert_provider(&source.anthropic, &version)?,
            version,
        })
    }
}

#[derive(Deserialize)]
struct ModelsDevCatalog {
    openai: ModelsDevProvider,
    anthropic: ModelsDevProvider,
}

#[derive(Deserialize)]
struct ModelsDevProvider {
    models: HashMap<String, ModelsDevModel>,
}

#[derive(Deserialize)]
struct ModelsDevModel {
    cost: Option<ModelsDevCost>,
}

#[derive(Deserialize)]
struct ModelsDevCost {
    input: Number,
    output: Number,
    cache_read: Option<Number>,
    cache_write: Option<Number>,
}

fn convert_provider(
    provider: &ModelsDevProvider,
    version: &str,
) -> Result<HashMap<String, ChatModelPricing>, Report<ModelPricingCatalogError>> {
    provider
        .models
        .iter()
        .filter_map(|(id, model)| model.cost.as_ref().map(|cost| (id, cost)))
        .map(|(id, cost)| Ok((id.clone(), convert_cost(cost, version)?)))
        .collect()
}

fn convert_cost(
    cost: &ModelsDevCost,
    version: &str,
) -> Result<ChatModelPricing, Report<ModelPricingCatalogError>> {
    Ok(ChatModelPricing {
        catalog_version: version.into(),
        token_count: TOKEN_COUNT,
        input: rate(&cost.input)?,
        cache_read_input: cost.cache_read.as_ref().map(rate).transpose()?,
        cache_write_input: cost.cache_write.as_ref().map(rate).transpose()?,
        output: rate(&cost.output)?,
    })
}

fn rate(value: &Number) -> Result<Money, Report<ModelPricingCatalogError>> {
    let amount = scaled_decimal(value)
        .ok_or_else(|| Report::new(ModelPricingCatalogError::InvalidRate(value.to_string())))?;
    Ok(Money {
        currency: "USD".into(),
        amount,
    })
}

fn scaled_decimal(value: &Number) -> Option<u64> {
    let value = value.to_string();
    let (mantissa, exponent) = match value.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, exponent.parse::<i32>().ok()?),
        None => (value.as_str(), 0_i32),
    };
    if mantissa.starts_with('-') {
        return None;
    }
    let mantissa = mantissa.strip_prefix('+').unwrap_or(mantissa);
    let (integer, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if integer.is_empty() && fraction.is_empty() {
        return None;
    }
    let digits = format!("{integer}{fraction}").parse::<u128>().ok()?;
    let decimal_places = i32::try_from(fraction.len()).ok()?;
    let power = exponent
        .checked_sub(decimal_places)?
        .checked_add(RATE_DECIMAL_PLACES)?;
    let scaled = if power >= 0 {
        digits.checked_mul(10_u128.checked_pow(u32::try_from(power).ok()?)?)?
    } else {
        let divisor = 10_u128.checked_pow(power.unsigned_abs())?;
        if digits % divisor != 0 {
            return None;
        }
        digits / divisor
    };
    u64::try_from(scaled).ok()
}

/// Failure while loading, fetching, or converting the models.dev catalog.
#[derive(Debug, Error)]
pub(crate) enum ModelPricingCatalogError {
    /// The durable cache could not be read.
    #[error("failed to read cached models.dev catalog")]
    ReadCache(#[source] io::Error),
    /// The durable cache could not be replaced.
    #[error("failed to persist models.dev catalog")]
    WriteCache(#[source] io::Error),
    /// The network request failed or returned an unsuccessful status.
    #[error("failed to fetch models.dev catalog")]
    Request(#[source] reqwest::Error),
    /// The response exceeded the defensive size limit.
    #[error("models.dev catalog exceeds the size limit")]
    CatalogTooLarge,
    /// The response did not conform to the minimal catalog schema.
    #[error("failed to decode models.dev catalog")]
    Decode(#[source] serde_json::Error),
    /// A rate could not be represented exactly in the chat API's minor units.
    #[error("models.dev returned an unsupported token rate: {0}")]
    InvalidRate(String),
}

#[cfg(test)]
mod tests {
    use tascarrel_api::ArcVec;

    use super::*;

    /// Confirms exact decimal conversion, provider routing, and unknown-model
    /// behavior using the minimal shape consumed from models.dev.
    #[test]
    fn enriches_known_models_from_default_provider_rates() {
        let bytes = br#"{
            "openai":{"models":{"gpt-test":{"cost":{
                "input":2.5,"output":15,"cache_read":0.25,"cache_write":3.125,
                "tiers":[{"input":5}],"context_over_200k":{"input":5}
            }}}},
            "anthropic":{"models":{"claude-test":{"cost":{
                "input":5,"output":25,"cache_read":0.5,"cache_write":6.25
            }}}},
            "ignored-provider":{"models":{}}
        }"#;
        let catalog = ModelPricingCatalog::parse(bytes).unwrap();
        let mut models = vec![model("gpt-test"), model("unknown")];

        catalog.apply(&ChatHarnessKind::Codex, &mut models);

        let pricing = models[0].pricing.as_ref().unwrap();
        assert!(
            pricing
                .catalog_version
                .as_ref()
                .starts_with("models.dev:sha256:")
        );
        assert_eq!(pricing.token_count, TOKEN_COUNT);
        assert_eq!(pricing.input.amount, 250_000_000);
        assert_eq!(
            pricing.cache_read_input.as_ref().unwrap().amount,
            25_000_000
        );
        assert_eq!(
            pricing.cache_write_input.as_ref().unwrap().amount,
            312_500_000
        );
        assert_eq!(pricing.output.amount, 1_500_000_000);
        assert!(models[1].pricing.is_none());

        let mut claude_models = vec![model("claude-test[1m]")];
        catalog.apply(&ChatHarnessKind::ClaudeCode, &mut claude_models);
        let pricing = claude_models[0].pricing.as_ref().unwrap();
        assert_eq!(pricing.input.amount, 500_000_000);
        assert_eq!(pricing.output.amount, 2_500_000_000);
    }

    fn model(id: &str) -> ChatModel {
        ChatModel {
            id: id.into(),
            display_name: id.into(),
            short_name: None,
            is_custom: false,
            options: ArcVec::new(),
            pricing: None,
        }
    }
}
