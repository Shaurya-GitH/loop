//! Credential storage.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use super::types::Credential;

/// Persistent (or in-memory) credential store keyed by provider id.
pub trait CredentialStore: Send + Sync {
    /// Explicit logout blocks credentials and ambient authentication until login.
    fn is_logged_out(&self, _provider_id: &str) -> bool {
        false
    }
    /// Get credential for a provider.
    fn get(&self, provider_id: &str) -> Option<Credential>;
    /// Set credential for a provider.
    fn set(&self, provider_id: &str, credential: Credential);
    /// Remove credential.
    fn remove(&self, provider_id: &str);
    /// List provider ids with credentials.
    fn list(&self) -> Vec<String>;
}

/// Process-local credential store.
#[derive(Debug, Default, Clone)]
pub struct InMemoryCredentialStore {
    inner: Arc<Mutex<HashMap<String, Credential>>>,
}

impl InMemoryCredentialStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl CredentialStore for InMemoryCredentialStore {
    fn get(&self, provider_id: &str) -> Option<Credential> {
        self.inner.lock().get(provider_id).cloned()
    }

    fn set(&self, provider_id: &str, credential: Credential) {
        self.inner
            .lock()
            .insert(provider_id.to_string(), credential);
    }

    fn remove(&self, provider_id: &str) {
        self.inner.lock().remove(provider_id);
    }

    fn list(&self) -> Vec<String> {
        self.inner.lock().keys().cloned().collect()
    }
}
