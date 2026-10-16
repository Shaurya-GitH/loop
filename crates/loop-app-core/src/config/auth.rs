//! File-backed auth.json credential store.

use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use loop_ai::{Credential, CredentialStore};

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthFile {
    #[serde(default)]
    providers: HashMap<String, StoredCred>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    logged_out_providers: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum StoredCred {
    #[serde(rename = "apiKey")]
    ApiKey { key: String },
}

impl From<StoredCred> for Credential {
    fn from(value: StoredCred) -> Self {
        match value {
            StoredCred::ApiKey { key } => Credential::api_key(key),
        }
    }
}

fn stored_from_credential(value: &Credential) -> Option<StoredCred> {
    match value {
        Credential::ApiKey { key } => Some(StoredCred::ApiKey { key: key.clone() }),
        Credential::OAuth { .. } => None,
    }
}

/// Credential store persisted to `auth.json` (mode 0600).
#[derive(Debug, Clone)]
pub struct FileCredentialStore {
    path: PathBuf,
    state: Arc<Mutex<AuthState>>,
}

#[derive(Debug, Clone, Default)]
struct AuthState {
    providers: HashMap<String, Credential>,
    logged_out_providers: BTreeSet<String>,
}

impl FileCredentialStore {
    /// Load (or create empty) store at path.
    pub fn open(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = path.into();
        let file = if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            if raw.trim().is_empty() {
                AuthFile::default()
            } else {
                serde_json::from_str(&raw)?
            }
        } else {
            AuthFile::default()
        };
        Ok(Self {
            path,
            state: Arc::new(Mutex::new(AuthState {
                providers: file
                    .providers
                    .into_iter()
                    .map(|(k, v)| (k, v.into()))
                    .collect(),
                logged_out_providers: file.logged_out_providers,
            })),
        })
    }

    fn persist(&self, state: &AuthState) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = AuthFile {
            providers: state
                .providers
                .iter()
                .filter_map(|(k, v)| {
                    stored_from_credential(v).map(|credential| (k.clone(), credential))
                })
                .collect(),
            logged_out_providers: state.logged_out_providers.clone(),
        };
        let json = serde_json::to_vec_pretty(&file)?;
        let tmp = self.path.with_extension("json.tmp");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options.open(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            output.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        output.write_all(&json)?;
        output.sync_all()?;
        drop(output);
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    fn update(&self, change: impl FnOnce(&mut AuthState)) -> anyhow::Result<()> {
        let mut state = self.state.lock();
        let mut next = state.clone();
        change(&mut next);
        self.persist(&next)?;
        *state = next;
        Ok(())
    }

    /// Save a verified login and clear explicit logout in one write.
    pub fn connect(&self, provider_id: &str, credential: Option<Credential>) -> anyhow::Result<()> {
        self.update(|state| {
            match credential {
                Some(credential) => {
                    state.providers.insert(provider_id.to_owned(), credential);
                }
                None => {
                    state.providers.remove(provider_id);
                }
            }
            state.logged_out_providers.remove(provider_id);
        })
    }

    /// Forget the saved key and block ambient keys until a successful login.
    pub fn logout(&self, provider_id: &str) -> anyhow::Result<()> {
        self.update(|state| {
            state.providers.remove(provider_id);
            state.logged_out_providers.insert(provider_id.to_owned());
        })
    }

    /// Path to auth.json.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl CredentialStore for FileCredentialStore {
    fn is_logged_out(&self, provider_id: &str) -> bool {
        self.state.lock().logged_out_providers.contains(provider_id)
    }

    fn get(&self, provider_id: &str) -> Option<Credential> {
        self.state.lock().providers.get(provider_id).cloned()
    }

    fn set(&self, provider_id: &str, credential: Credential) {
        if let Err(error) = self.update(|state| {
            state.providers.insert(provider_id.to_owned(), credential);
        }) {
            tracing::warn!("could not save credential for {provider_id}: {error}");
        }
    }

    fn remove(&self, provider_id: &str) {
        if let Err(error) = self.update(|state| {
            state.providers.remove(provider_id);
        }) {
            tracing::warn!("could not remove credential for {provider_id}: {error}");
        }
    }

    fn list(&self) -> Vec<String> {
        self.state.lock().providers.keys().cloned().collect()
    }
}

/// Whether Soket (or named provider) has a usable key from env or store.
pub fn provider_has_key(store: &dyn CredentialStore, provider_id: &str, envs: &[&str]) -> bool {
    if store.is_logged_out(provider_id) {
        return false;
    }
    if store.get(provider_id).is_some() {
        return true;
    }
    for env in envs {
        if let Ok(v) = std::env::var(env) {
            if !v.trim().is_empty() {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logout_survives_restart_and_unrelated_credential_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            r#"{"providers":{"soket":{"type":"apiKey","key":"old-key"}}}"#,
        )
        .unwrap();
        let store = FileCredentialStore::open(&path).unwrap();
        assert!(!store.is_logged_out("soket"));
        store.logout("soket").unwrap();
        store.set("other", Credential::api_key("other-key"));
        store.remove("other");
        let store = FileCredentialStore::open(&path).unwrap();
        assert!(store.is_logged_out("soket"));
        assert!(store.get("soket").is_none());
        assert!(!std::fs::read_to_string(&path).unwrap().contains("old-key"));
        // Ordinary writes cannot undo an explicit logout.
        store.set("soket", Credential::api_key("candidate"));
        assert!(store.is_logged_out("soket"));
        store
            .connect("soket", Some(Credential::api_key("verified")))
            .unwrap();
        let reopened = FileCredentialStore::open(path).unwrap();
        assert!(!reopened.is_logged_out("soket"));
        assert_eq!(reopened.get("soket"), Some(Credential::api_key("verified")));
    }

    #[test]
    fn failed_writes_do_not_change_live_auth_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let store = FileCredentialStore::open(&path).unwrap();
        store
            .connect("soket", Some(Credential::api_key("saved")))
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(store.logout("soket").is_err());
        assert!(!store.is_logged_out("soket"));
        assert_eq!(store.get("soket"), Some(Credential::api_key("saved")));
        std::fs::remove_dir(&path).unwrap();
        store.logout("soket").unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(store
            .connect("soket", Some(Credential::api_key("new")))
            .is_err());
        assert!(store.is_logged_out("soket"));
        assert!(store.get("soket").is_none());
    }
}
