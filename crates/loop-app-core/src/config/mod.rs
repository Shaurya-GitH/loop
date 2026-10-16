//! Configuration paths, settings, auth, and trust.

pub mod auth;
pub mod paths;
pub mod providers;
pub mod settings;
pub mod tracing;
pub mod trust;

pub use auth::{provider_has_key, FileCredentialStore};
pub use paths::*;
pub use providers::{
    is_web_search_provider, web_search_display_name, CustomProviderEntry, ProviderLoginRequest,
};
pub use settings::{load_settings, Settings};
#[cfg(feature = "telemetry")]
pub use tracing::{
    describe_tracing_status, resolve_tracing_destination, TracingControl, TracingSetupRequest,
};
pub use tracing::{validate_http_url, TracingBackend, TracingSettings};
pub use trust::TrustStore;
