//! Semantic validation for portable workspace settings.
//!
//! [`validate`] enforces cross-reference and transport invariants that cannot
//! be expressed by the generated settings shape.

use std::collections::HashMap;
use std::collections::HashSet;

use hyper::Uri;
use hyper::header::HeaderName;
use hyper::header::HeaderValue;
use reportify::ErrorExt as _;
use reportify::Report;
use tascarrel_api::is_valid_chat_cost_center_id;
use tascarrel_api::types::config as api;
use thiserror::Error;

/// Validates one complete portable settings document.
///
/// # Errors
///
/// Returns a value-safe diagnostic when a Tasci endpoint, model, MCP server, or
/// cross-reference is invalid.
pub(crate) fn validate(
    settings: &api::WorkspaceSettings,
) -> Result<(), Report<SettingsValidationError>> {
    validate_usage_settings(settings.usage.as_ref())?;
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
    if let Some(mcp_servers) = &tasci.mcp_servers {
        for (name, server) in mcp_servers {
            validate_mcp_server_name(name)?;
            validate_optional_name(
                server.display_name.as_deref(),
                SettingsValidationError::InvalidMcpServerDisplayName,
            )?;
            validate_mcp_endpoint(server.endpoint.as_ref())?;
            validate_mcp_headers(server.headers.as_ref())?;
        }
    }
    Ok(())
}

/// Value-safe semantic settings failures.
#[derive(Clone, Copy, Debug, Error)]
pub(crate) enum SettingsValidationError {
    #[error(
        "cost-center identifiers must contain 1-64 ASCII letters, digits, hyphens, or underscores"
    )]
    InvalidCostCenterId,
    #[error("cost-center names must contain non-whitespace text")]
    InvalidCostCenterName,
    #[error("the default cost center must reference an active configured cost center")]
    InvalidDefaultCostCenter,
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
    #[error("Tasci authorization values must be valid non-empty HTTP header text")]
    InvalidAuthorizationValue,
    #[error("Tasci authorization must specify either a value or one legacy credential reference")]
    InvalidAuthorizationShape,
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
    #[error(
        "Tasci MCP server names must contain only ASCII letters, digits, hyphens, and underscores"
    )]
    InvalidMcpServerName,
    #[error("Tasci MCP server display names must not be empty when specified")]
    InvalidMcpServerDisplayName,
    #[error(
        "Tasci MCP endpoints must be absolute HTTP or HTTPS URLs without credentials, queries, or fragments"
    )]
    InvalidMcpEndpoint,
    #[error("Tasci MCP header names are invalid or repeated without regard to case")]
    InvalidMcpHeaderName,
    #[error("Tasci MCP header values are not valid HTTP header text")]
    InvalidMcpHeaderValue,
}

/// Validates cost-center identifiers, names, and the configured default.
fn validate_usage_settings(
    usage: Option<&api::WorkspaceUsageSettings>,
) -> Result<(), Report<SettingsValidationError>> {
    let Some(usage) = usage else {
        return Ok(());
    };
    if let Some(cost_centers) = &usage.cost_centers {
        for (id, cost_center) in cost_centers {
            if !is_valid_chat_cost_center_id(id) {
                return Err(SettingsValidationError::InvalidCostCenterId.report());
            }
            if cost_center.name.trim().is_empty() {
                return Err(SettingsValidationError::InvalidCostCenterName.report());
            }
        }
    }
    if let Some(default) = &usage.default_cost_center
        && !usage.cost_centers.as_ref().is_some_and(|cost_centers| {
            cost_centers
                .get(default.as_str())
                .is_some_and(|cost_center| cost_center.archived != Some(true))
        })
    {
        return Err(SettingsValidationError::InvalidDefaultCostCenter.report());
    }
    Ok(())
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

/// Validates one settings key for use in model-visible MCP tool names.
fn validate_mcp_server_name(value: &str) -> Result<(), Report<SettingsValidationError>> {
    if value.is_empty()
        || value.chars().any(|character| {
            !character.is_ascii_alphanumeric() && character != '-' && character != '_'
        })
    {
        return Err(SettingsValidationError::InvalidMcpServerName.report());
    }
    Ok(())
}

/// Validates a Streamable HTTP endpoint.
fn validate_mcp_endpoint(value: &str) -> Result<(), Report<SettingsValidationError>> {
    let Ok(uri) = value.parse::<Uri>() else {
        return Err(SettingsValidationError::InvalidMcpEndpoint.report());
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
        return Err(SettingsValidationError::InvalidMcpEndpoint.report());
    }
    Ok(())
}

/// Validates arbitrary MCP request header templates.
fn validate_mcp_headers(
    headers: Option<&HashMap<tascarrel_api::ArcStr, tascarrel_api::ArcStr>>,
) -> Result<(), Report<SettingsValidationError>> {
    let mut names = HashSet::new();
    for (name, value) in headers.into_iter().flatten() {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| SettingsValidationError::InvalidMcpHeaderName.report())?;
        if !names.insert(name) {
            return Err(SettingsValidationError::InvalidMcpHeaderName.report());
        }
        HeaderValue::from_str(value)
            .map_err(|_| SettingsValidationError::InvalidMcpHeaderValue.report())?;
    }
    Ok(())
}

/// Validates header metadata without resolving the referenced secret.
fn validate_authorization(
    authorization: &api::WorkspaceTasciAuthorization,
) -> Result<(), Report<SettingsValidationError>> {
    HeaderName::from_bytes(authorization.header.as_bytes())
        .map_err(|_| SettingsValidationError::InvalidAuthorizationHeader.report())?;
    match (&authorization.value, &authorization.credential) {
        (Some(value), None) => {
            if value.is_empty() || HeaderValue::from_str(value).is_err() {
                return Err(SettingsValidationError::InvalidAuthorizationValue.report());
            }
            if authorization.prefix.is_some() {
                return Err(SettingsValidationError::InvalidAuthorizationShape.report());
            }
        }
        (None, Some(credential)) => {
            if authorization
                .prefix
                .as_ref()
                .is_some_and(|prefix| HeaderValue::from_str(prefix).is_err())
            {
                return Err(SettingsValidationError::InvalidAuthorizationPrefix.report());
            }
            if !valid_secret_provider_name(credential.provider.as_ref())
                || !valid_secret_name(credential.secret.as_ref())
            {
                return Err(SettingsValidationError::InvalidAuthorizationCredential.report());
            }
        }
        (Some(_), Some(_)) | (None, None) => {
            return Err(SettingsValidationError::InvalidAuthorizationShape.report());
        }
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

#[cfg(test)]
mod tests {
    use tascarrel_api::types::chats::ChatCostCenterId;

    use super::*;

    /// Confirms that archived cost centers remain historical-only and cannot
    /// become the default for new chats.
    #[test]
    fn rejects_archived_usage_default() {
        let settings = api::WorkspaceSettings {
            chat: None,
            usage: Some(api::WorkspaceUsageSettings {
                default_cost_center: Some(ChatCostCenterId::new("internal")),
                cost_centers: Some(HashMap::from([(
                    "internal".into(),
                    api::WorkspaceCostCenter {
                        name: "Internal".into(),
                        archived: Some(true),
                    },
                )])),
            }),
        };

        assert!(validate(&settings).is_err());
    }
}
