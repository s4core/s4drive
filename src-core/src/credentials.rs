use crate::error::{CoreError, CoreResult};
use keyring::Entry;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ─── Credential Backend Trait ─────────────────────────────────────────

/// Pluggable backend for credential storage.
///
/// Two implementations:
/// - `KeychainBackend` — OS keychain (production)
/// - `InMemoryBackend` — HashMap (tests, CI, headless)
#[doc(hidden)]
pub trait CredentialBackend: std::fmt::Debug {
    fn store(&self, service: &str, username: &str, payload: &str) -> CoreResult<()>;
    fn get(&self, service: &str, username: &str) -> CoreResult<String>;
    fn delete(&self, service: &str, username: &str) -> CoreResult<()>;
}

// ─── OS Keychain Backend (production) ────────────────────────────────

/// Backed by OS keychain via `keyring` crate.
#[derive(Debug)]
pub struct KeychainBackend;

impl CredentialBackend for KeychainBackend {
    fn store(&self, service: &str, username: &str, payload: &str) -> CoreResult<()> {
        let entry = Entry::new(service, username)
            .map_err(|e| CoreError::Auth(format!("keychain entry creation failed: {}", e)))?;
        entry
            .set_password(payload)
            .map_err(|e| CoreError::Auth(format!("keychain set failed: {}", e)))?;
        Ok(())
    }

    fn get(&self, service: &str, username: &str) -> CoreResult<String> {
        let entry = Entry::new(service, username)
            .map_err(|e| CoreError::Auth(format!("keychain entry creation failed: {}", e)))?;
        entry.get_password().map_err(|e| {
            if matches!(e, keyring::Error::NoEntry) {
                CoreError::NotFound(format!(
                    "no credential found for {} @ {}",
                    username, service
                ))
            } else {
                CoreError::Auth(format!("keychain get failed: {}", e))
            }
        })
    }

    fn delete(&self, service: &str, username: &str) -> CoreResult<()> {
        let entry = Entry::new(service, username)
            .map_err(|e| CoreError::Auth(format!("keychain entry creation failed: {}", e)))?;
        entry
            .delete_credential()
            .map_err(|e| CoreError::Auth(format!("keychain delete failed: {}", e)))?;
        Ok(())
    }
}

// ─── In-Memory Backend (testing) ──────────────────────────────────────

/// Thread-local in-memory credential store for testing.
#[derive(Debug, Clone, Default)]
pub struct InMemoryBackend {
    storage: std::sync::Arc<std::sync::Mutex<HashMap<(String, String), String>>>,
}

impl CredentialBackend for InMemoryBackend {
    fn store(&self, service: &str, username: &str, payload: &str) -> CoreResult<()> {
        let mut storage = self
            .storage
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        storage.insert(
            (service.to_string(), username.to_string()),
            payload.to_string(),
        );
        Ok(())
    }

    fn get(&self, service: &str, username: &str) -> CoreResult<String> {
        let storage = self
            .storage
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        storage
            .get(&(service.to_string(), username.to_string()))
            .cloned()
            .ok_or_else(|| {
                CoreError::NotFound(format!(
                    "no credential found for {} @ {}",
                    username, service
                ))
            })
    }

    fn delete(&self, service: &str, username: &str) -> CoreResult<()> {
        let mut storage = self
            .storage
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        storage.remove(&(service.to_string(), username.to_string()));
        Ok(())
    }
}

// ─── An encrypted credential payload stored in the keychain. ──────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CredentialPayload {
    version: u32,
    secret_key_encrypted: String,
    region: String,
    bucket: String,
    metadata: HashMap<String, String>,
}

// ─── Credential Store ─────────────────────────────────────────────────

/// Credential store backed by a pluggable backend.
///
/// Production: `CredentialStore::new()` uses OS keychain.
/// Testing: `CredentialStore::new_test()` uses in-memory HashMap.
pub struct CredentialStore {
    service_prefix: String,
    backend: Box<dyn CredentialBackend>,
}

impl std::fmt::Debug for CredentialStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialStore")
            .field("service_prefix", &self.service_prefix)
            .finish()
    }
}

impl CredentialStore {
    /// Create a credential store backed by the OS keychain.
    pub fn new(service_prefix: &str) -> Self {
        Self {
            service_prefix: format!("s4drive/{}", service_prefix),
            backend: Box::new(KeychainBackend),
        }
    }

    /// Create a credential store backed by in-memory HashMap (for testing).
    /// Credentials are NOT persisted — lost when the store is dropped.
    pub fn new_test(service_prefix: &str) -> Self {
        Self {
            service_prefix: format!("s4drive-test/{}", service_prefix),
            backend: Box::new(InMemoryBackend::default()),
        }
    }

    /// Store a credential.
    pub fn store(
        &self,
        endpoint: &str,
        access_key_id: &str,
        secret_key: &str,
        region: &str,
        bucket: &str,
    ) -> CoreResult<()> {
        let username = self.keychain_username(endpoint, access_key_id);

        let payload = CredentialPayload {
            version: 1,
            secret_key_encrypted: secret_key.to_string(),
            region: region.to_string(),
            bucket: bucket.to_string(),
            metadata: HashMap::new(),
        };

        let json = serde_json::to_string(&payload)
            .map_err(|e| CoreError::Auth(format!("credential serialization failed: {}", e)))?;

        self.backend.store(&self.service_prefix, &username, &json)
    }

    /// Retrieve a credential.
    pub fn get(&self, endpoint: &str, access_key_id: &str) -> CoreResult<StoredCredential> {
        let username = self.keychain_username(endpoint, access_key_id);
        let json = self.backend.get(&self.service_prefix, &username)?;

        let payload: CredentialPayload = serde_json::from_str(&json)
            .map_err(|e| CoreError::Auth(format!("credential deserialization failed: {}", e)))?;

        Ok(StoredCredential {
            endpoint: endpoint.to_string(),
            access_key_id: access_key_id.to_string(),
            secret_key: payload.secret_key_encrypted,
            region: payload.region,
            bucket: payload.bucket,
        })
    }

    /// Delete a credential.
    pub fn delete(&self, endpoint: &str, access_key_id: &str) -> CoreResult<()> {
        let username = self.keychain_username(endpoint, access_key_id);
        self.backend.delete(&self.service_prefix, &username)
    }

    /// Check if a credential exists.
    pub fn exists(&self, endpoint: &str, access_key_id: &str) -> bool {
        self.get(endpoint, access_key_id).is_ok()
    }

    fn keychain_username(&self, endpoint: &str, access_key_id: &str) -> String {
        format!("{}:{}", endpoint, access_key_id)
    }
}

// ─── Stored Credential ────────────────────────────────────────────────

/// A stored credential for an S3 endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredCredential {
    pub endpoint: String,
    pub access_key_id: String,
    pub secret_key: String,
    pub region: String,
    pub bucket: String,
}

// ─── Resolution ──────────────────────────────────────────────────────

/// Utility: resolve credentials from config, trying keychain first.
///
/// Resolution order:
/// 1. Keychain (if `secret_key_fallback` is absent or empty)
/// 2. Config's `secret_key_fallback` field (plaintext fallback)
pub fn resolve_secret(
    store: &CredentialStore,
    config_endpoint: &str,
    config_access_key: &str,
    config_fallback: Option<&str>,
) -> CoreResult<String> {
    // Try keychain first
    if let Ok(cred) = store.get(config_endpoint, config_access_key) {
        return Ok(cred.secret_key);
    }

    // Fallback to config field
    if let Some(secret) = config_fallback {
        if !secret.is_empty() {
            return Ok(secret.to_string());
        }
    }

    Err(CoreError::NotFound(
        "no secret key found in keychain or config".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> CredentialStore {
        CredentialStore::new_test("unit")
    }

    #[test]
    fn test_keychain_username() {
        let store = test_store();
        let username = store.keychain_username("http://localhost:9000", "minioadmin");
        assert_eq!(username, "http://localhost:9000:minioadmin");
    }

    #[test]
    fn test_resolve_secret_no_keychain() {
        let store = test_store();
        let result = resolve_secret(&store, "endpoint", "key", Some("config-secret"));
        assert_eq!(result.unwrap(), "config-secret");
    }

    #[test]
    fn test_resolve_secret_empty_fallback() {
        let store = test_store();
        let result = resolve_secret(&store, "endpoint", "key", Some(""));
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_secret_no_fallback() {
        let store = test_store();
        let result = resolve_secret(&store, "endpoint", "key", None);
        assert!(result.is_err());
    }

    #[test]
    fn test_store_and_get_credential() {
        let store = test_store();
        let endpoint = "http://localhost:9000";
        let ak = "test-access-key";
        let sk = "test-secret-key";
        let region = "us-east-1";
        let bucket = "test-bucket";

        store.store(endpoint, ak, sk, region, bucket).unwrap();

        let cred = store.get(endpoint, ak).unwrap();
        assert_eq!(cred.endpoint, endpoint);
        assert_eq!(cred.access_key_id, ak);
        assert_eq!(cred.secret_key, sk);
        assert_eq!(cred.region, region);
        assert_eq!(cred.bucket, bucket);
    }

    #[test]
    fn test_delete_credential() {
        let store = test_store();
        store
            .store("http://ep", "ak1", "sk1", "us-east-1", "b1")
            .unwrap();
        assert!(store.exists("http://ep", "ak1"));
        store.delete("http://ep", "ak1").unwrap();
        assert!(!store.exists("http://ep", "ak1"));
    }

    #[test]
    fn test_exists_missing() {
        let store = test_store();
        assert!(!store.exists("http://missing", "no-key"));
    }

    #[test]
    fn test_overwrite_credential() {
        let store = test_store();
        store.store("http://ep", "u1", "v1", "r1", "b1").unwrap();
        store.store("http://ep", "u1", "v2", "r2", "b2").unwrap();

        let cred = store.get("http://ep", "u1").unwrap();
        assert_eq!(cred.secret_key, "v2");
        assert_eq!(cred.region, "r2");
    }

    #[test]
    fn test_multiple_credentials() {
        let store = test_store();
        store.store("http://a", "ak1", "sk1", "r1", "b1").unwrap();
        store.store("http://b", "ak2", "sk2", "r2", "b2").unwrap();

        let c1 = store.get("http://a", "ak1").unwrap();
        let c2 = store.get("http://b", "ak2").unwrap();
        assert_eq!(c1.secret_key, "sk1");
        assert_eq!(c2.secret_key, "sk2");
    }
}
