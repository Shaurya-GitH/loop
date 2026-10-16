//! Auth resolution order for providers.

use thiserror::Error;

use super::credential_store::CredentialStore;
use super::types::{AuthCheck, Credential, ModelAuth, ProviderAuth};

/// Models / auth errors.
#[derive(Debug, Error)]
pub enum ModelsError {
    /// Auth could not be resolved.
    #[error("auth error ({code:?}): {message}")]
    Auth {
        /// Error code.
        code: ModelsErrorCode,
        /// Message.
        message: String,
    },
    /// Provider or model missing.
    #[error("{0}")]
    NotFound(String),
    /// Stream / API misconfiguration.
    #[error("{0}")]
    Stream(String),
}

/// Auth-related error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelsErrorCode {
    /// No credential and ambient resolve failed.
    NotConfigured,
    /// Stored credential present but unusable.
    InvalidCredential,
    /// Generic auth failure.
    Failed,
}

/// Resolve auth for a provider.
///
/// Order:
/// 1. Explicit `api_key` override (if provider has apiKey auth)
/// 2. Stored credential for the provider
/// 3. Ambient `api_key.resolve()`
pub async fn resolve_provider_auth(
    provider_id: &str,
    auth: &ProviderAuth,
    store: &dyn CredentialStore,
    explicit_api_key: Option<&str>,
) -> Result<ModelAuth, ModelsError> {
    if store.is_logged_out(provider_id) {
        return Err(ModelsError::Auth {
            code: ModelsErrorCode::NotConfigured,
            message: format!("{provider_id} is logged out; run /login {provider_id}"),
        });
    }
    if let Some(key) = explicit_api_key {
        if auth.api_key.is_some() {
            return Ok(ModelAuth {
                api_key: Some(key.to_string()),
                headers: Default::default(),
                base_url: None,
            });
        }
    }

    if let Some(cred) = store.get(provider_id) {
        return credential_to_auth(&cred);
    }

    if let Some(api_key) = &auth.api_key {
        match (api_key.resolve)().await {
            Ok(result) => Ok(result.auth),
            Err(message) => Err(ModelsError::Auth {
                code: ModelsErrorCode::NotConfigured,
                message,
            }),
        }
    } else {
        Err(ModelsError::Auth {
            code: ModelsErrorCode::NotConfigured,
            message: format!("provider {provider_id} has no auth methods"),
        })
    }
}

fn credential_to_auth(cred: &Credential) -> Result<ModelAuth, ModelsError> {
    match cred {
        Credential::ApiKey { key } => Ok(ModelAuth {
            api_key: Some(key.clone()),
            headers: Default::default(),
            base_url: None,
        }),
        Credential::OAuth { access, .. } => Ok(ModelAuth {
            api_key: Some(access.clone()),
            headers: Default::default(),
            base_url: None,
        }),
    }
}

/// Check whether a provider appears configured without refreshing OAuth.
pub async fn check_provider_auth(
    provider_id: &str,
    auth: &ProviderAuth,
    store: &dyn CredentialStore,
) -> AuthCheck {
    if store.is_logged_out(provider_id) {
        return AuthCheck {
            provider_id: provider_id.to_string(),
            configured: false,
            message: Some(format!("logged out; run /login {provider_id}")),
        };
    }
    if store.get(provider_id).is_some() {
        return AuthCheck {
            provider_id: provider_id.to_string(),
            configured: true,
            message: None,
        };
    }
    if let Some(api_key) = &auth.api_key {
        match (api_key.resolve)().await {
            Ok(_) => AuthCheck {
                provider_id: provider_id.to_string(),
                configured: true,
                message: None,
            },
            Err(message) => AuthCheck {
                provider_id: provider_id.to_string(),
                configured: false,
                message: Some(message),
            },
        }
    } else {
        AuthCheck {
            provider_id: provider_id.to_string(),
            configured: false,
            message: Some("no auth methods".into()),
        }
    }
}
