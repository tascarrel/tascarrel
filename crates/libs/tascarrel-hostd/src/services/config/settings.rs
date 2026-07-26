//! Semantic validation for portable workspace settings.
//!
//! [`validate`] enforces cross-reference and transport invariants that cannot
//! be expressed by the generated settings shape.

use hyper::Uri;
use hyper::header::HeaderName;
use hyper::header::HeaderValue;
use reportify::ErrorExt as _;
use reportify::Report;
use tascarrel_api::types::config as api;
use thiserror::Error;

/// Validates one complete portable settings document.
///
/// # Errors
///
/// Returns a value-safe diagnostic when a Tasci endpoint, model, or
/// cross-reference is invalid.
pub(crate) fn validate(
    settings: &api::WorkspaceSettings,
) -> Result<(), Report<SettingsValidationError>> {
    let Some(tasci) = settings.chat.as_ref().and_then(|chat| chat.tasci.as_ref()) else {
        return Ok(());
    };
    let endpoints = tasci.endpoints.as_ref();
    if let Some(endpoints) = endpoints {
        for (alias, endpoint) in endpoints {
            validate_alias(alias, SettingsValidationError::InvalidEndpointAlias)?;
            validate_optional_name(
                endpoint.display_name.as_deref(),
                SettingsValidationError::InvalidEndpointDisplayName,
            )?;
            validate_base_url(endpoint.base_url.as_ref())?;
            if let Some(authorization) = &endpoint.authorization {
                validate_authorization(authorization)?;
            }
        }
    }
    if let Some(models) = &tasci.models {
        for (alias, model) in models {
            validate_alias(alias, SettingsValidationError::InvalidModelAlias)?;
            validate_optional_name(
                model.display_name.as_deref(),
                SettingsValidationError::InvalidModelDisplayName,
            )?;
            validate_required_text(
                model.model.as_ref(),
                SettingsValidationError::InvalidModelIdentifier,
            )?;
            validate_required_text(
                model.endpoint.as_ref(),
                SettingsValidationError::InvalidModelEndpoint,
            )?;
            if !endpoints.is_some_and(|endpoints| endpoints.contains_key(&model.endpoint)) {
                return Err(SettingsValidationError::MissingModelEndpoint.report());
            }
            if model.context_window == Some(0) {
                return Err(SettingsValidationError::InvalidContextWindow.report());
            }
            if model.max_output_tokens == Some(0) {
                return Err(SettingsValidationError::InvalidMaxOutputTokens.report());
            }
            if model.parallel_tool_calls == Some(true) && model.tool_calls == Some(false) {
                return Err(SettingsValidationError::InvalidToolCapabilities.report());
            }
            if model
                .pricing
                .as_ref()
                .is_some_and(|pricing| pricing.token_count == 0)
            {
                return Err(SettingsValidationError::InvalidPricingTokenCount.report());
            }
        }
    }
    if let Some(default_model) = &tasci.default_model {
        validate_required_text(
            default_model.as_ref(),
            SettingsValidationError::InvalidDefaultModel,
        )?;
        if !tasci
            .models
            .as_ref()
            .is_some_and(|models| models.contains_key(default_model))
        {
            return Err(SettingsValidationError::MissingDefaultModel.report());
        }
    }
    Ok(())
}

/// Value-safe semantic settings failures.
#[derive(Clone, Copy, Debug, Error)]
pub(crate) enum SettingsValidationError {
    #[error("Tasci endpoint aliases must not be empty or contain surrounding whitespace")]
    InvalidEndpointAlias,
    #[error("Tasci endpoint display names must not be empty when specified")]
    InvalidEndpointDisplayName,
    #[error(
        "Tasci endpoint base URLs must be absolute HTTP or HTTPS URLs without credentials, queries, or fragments"
    )]
    InvalidBaseUrl,
    #[error("Tasci authorization header names are invalid")]
    InvalidAuthorizationHeader,
    #[error("Tasci authorization prefixes must be valid HTTP header text")]
    InvalidAuthorizationPrefix,
    #[error("Tasci authorization secret references must specify a provider and secret")]
    InvalidAuthorizationCredential,
    #[error("Tasci model aliases must not be empty or contain surrounding whitespace")]
    InvalidModelAlias,
    #[error("Tasci model display names must not be empty when specified")]
    InvalidModelDisplayName,
    #[error("Tasci model identifiers must not be empty or contain surrounding whitespace")]
    InvalidModelIdentifier,
    #[error("Tasci model endpoint references must not be empty or contain surrounding whitespace")]
    InvalidModelEndpoint,
    #[error("a Tasci model references an endpoint that is not configured")]
    MissingModelEndpoint,
    #[error("Tasci model context windows must be greater than zero")]
    InvalidContextWindow,
    #[error("Tasci model output limits must be greater than zero")]
    InvalidMaxOutputTokens,
    #[error("Tasci models cannot enable parallel tool calls while disabling tool calls")]
    InvalidToolCapabilities,
    #[error("Tasci model pricing token counts must be greater than zero")]
    InvalidPricingTokenCount,
    #[error("the default Tasci model must not be empty or contain surrounding whitespace")]
    InvalidDefaultModel,
    #[error("the default Tasci model is not configured")]
    MissingDefaultModel,
}

/// Validates a stable map key without constraining its portable character set.
fn validate_alias(
    value: &str,
    error: SettingsValidationError,
) -> Result<(), Report<SettingsValidationError>> {
    validate_required_text(value, error)
}

/// Validates optional user-facing text.
fn validate_optional_name(
    value: Option<&str>,
    error: SettingsValidationError,
) -> Result<(), Report<SettingsValidationError>> {
    if value.is_some_and(|value| value.trim().is_empty()) {
        return Err(error.report());
    }
    Ok(())
}

/// Validates required identifiers and references.
fn validate_required_text(
    value: &str,
    error: SettingsValidationError,
) -> Result<(), Report<SettingsValidationError>> {
    if value.is_empty() || value.trim() != value {
        return Err(error.report());
    }
    Ok(())
}

/// Validates an API base URL according to the model transport contract.
fn validate_base_url(value: &str) -> Result<(), Report<SettingsValidationError>> {
    let Ok(uri) = value.parse::<Uri>() else {
        return Err(SettingsValidationError::InvalidBaseUrl.report());
    };
    if !matches!(uri.scheme_str(), Some("http" | "https"))
        || uri.authority().is_none()
        || uri
            .authority()
            .is_some_and(|authority| authority.as_str().contains('@'))
        || uri
            .path_and_query()
            .and_then(hyper::http::uri::PathAndQuery::query)
            .is_some()
    {
        return Err(SettingsValidationError::InvalidBaseUrl.report());
    }
    Ok(())
}

/// Validates header metadata without resolving the referenced secret.
fn validate_authorization(
    authorization: &api::WorkspaceTasciAuthorization,
) -> Result<(), Report<SettingsValidationError>> {
    HeaderName::from_bytes(authorization.header.as_bytes())
        .map_err(|_| SettingsValidationError::InvalidAuthorizationHeader.report())?;
    if authorization
        .prefix
        .as_ref()
        .is_some_and(|prefix| HeaderValue::from_str(prefix).is_err())
    {
        return Err(SettingsValidationError::InvalidAuthorizationPrefix.report());
    }
    if !valid_secret_provider_name(authorization.credential.provider.as_ref())
        || !valid_secret_name(authorization.credential.secret.as_ref())
    {
        return Err(SettingsValidationError::InvalidAuthorizationCredential.report());
    }
    Ok(())
}

/// Returns whether a provider name belongs to the portable secret namespace.
fn valid_secret_provider_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic())
        && characters.all(|character| {
            character == '_' || character == '-' || character.is_ascii_alphanumeric()
        })
}

/// Returns whether a secret name belongs to the portable provider namespace.
fn valid_secret_name(name: &str) -> bool {
    let mut characters = name.chars();
    name != "sops"
        && characters
            .next()
            .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}
