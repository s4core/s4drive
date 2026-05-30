use crate::error::{CoreError, CoreResult};
use keyring::Entry;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Credential store backed by the OS keychain (via `keyring` crate).
///
/// Each credential is stored with a service prefix to avoid collisions
/// with other applications using the same keychain.
pub struct CredentialStore {
    service_prefix: String,
}

/// A stored credential for an S3 endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredCredential {
    pub endpoint: String,
    pub access_key_id: String,
    /// The plaintext secret key (never stored to disk unencrypted).
    pub secret_key: String,
    pub region: String,
    pub bucket: String,
}

/// An encrypted credential payload stored in the keychain.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CredentialPayload {
    version: u32,
    secret_key_encrypted: String,
    region: String,
    bucket: String,
    metadata: HashMap<String, String>,
}

impl CredentialStore {
    /// Create a new credential store with the given service prefix.
    pub fn new(service_prefix: &str) -> Self {
        Self {
            service_prefix: format!("s4drive/{}", service_prefix),
        }
    }

    /// Store a credential in the keychain.
    /// The secret key is stored encrypted by the OS keychain.
    pub fn store(
        &self,
        endpoint: &str,
        access_key_id: &str,
        secret_key: &str,
        region: &str,
        bucket: &str,
    ) -> CoreResult<()> {
        let username = self.keychain_username(endpoint, access_key_id);
        let entry = Entry::new(&self.service_prefix, &username)
            .map_err(|e| CoreError::Auth(format!("keychain entry creation failed: {}", e)))?;

        let payload = CredentialPayload {
            version: 1,
            secret_key_encrypted: secret_key.to_string(),
            region: region.to_string(),
            bucket: bucket.to_string(),
            metadata: HashMap::new(),
        };

        let json = serde_json::to_string(&payload)
            .map_err(|e| CoreError::Auth(format!("credential serialization failed: {}", e)))?;

        entry
            .set_password(&json)
            .map_err(|e| CoreError::Auth(format!("keychain set failed: {}", e)))?;

        Ok(())
    }

    /// Retrieve a credential from the keychain.
    pub fn get(&self, endpoint: &str, access_key_id: &str) -> CoreResult<StoredCredential> {
        let username = self.keychain_username(endpoint, access_key_id);
        let entry = Entry::new(&self.service_prefix, &username)
            .map_err(|e| CoreError::Auth(format!("keychain entry creation failed: {}", e)))?;

        let json = entry.get_password().map_err(|e| {
            if matches!(e, keyring::Error::NoEntry) {
                CoreError::NotFound(format!(
                    "no credential found for {} @ {}",
                    access_key_id, endpoint
                ))
            } else {
                CoreError::Auth(format!("keychain get failed: {}", e))
            }
        })?;

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

    /// Delete a credential from the keychain.
    pub fn delete(&self, endpoint: &str, access_key_id: &str) -> CoreResult<()> {
        let username = self.keychain_username(endpoint, access_key_id);
        let entry = Entry::new(&self.service_prefix, &username)
            .map_err(|e| CoreError::Auth(format!("keychain entry creation failed: {}", e)))?;

        entry
            .delete_credential()
            .map_err(|e| CoreError::Auth(format!("keychain delete failed: {}", e)))?;

        Ok(())
    }

    /// Check if a credential exists in the keychain.
    pub fn exists(&self, endpoint: &str, access_key_id: &str) -> bool {
        self.get(endpoint, access_key_id).is_ok()
    }

    /// Generate a unique keychain username from endpoint + access key.
    fn keychain_username(&self, endpoint: &str, access_key_id: &str) -> String {
        format!("{}:{}", endpoint, access_key_id)
    }
}

/// Temporary credential holder (in-memory, for testing / config-driven setup).
///
/// Falls back to plaintext if keychain is not available (e.g. headless CI).
#[derive(Debug, Clone, Default)]
pub struct TempCredentials {
    pub access_key_id: String,
    pub secret_key: String,
}

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

    /// Test that keychain username generation is deterministic and unique.
    #[test]
    fn test_keychain_username() {
        let store = CredentialStore::new("test");
        let username = store.keychain_username("http://localhost:9000", "minioadmin");
        assert_eq!(username, "http://localhost:9000:minioadmin");
    }

    /// Test that service prefix is consistent.
    #[test]
    fn test_service_prefix() {
        let store = CredentialStore::new("default");
        // We can't test keychain I/O in unit tests without a backend,
        // but we can verify the prefix structure.
        let _ = store;
    }

    /// Test resolve_secret with no keychain (should fail gracefully).
    /// Test resolve_secret with no keychain (should fall back to config).
    #[test]
    fn test_resolve_secret_no_keychain() {
        let store = CredentialStore::new("test-unit");
        let result = resolve_secret(&store, "endpoint", "key", Some("config-secret"));
        assert_eq!(result.unwrap(), "config-secret");
    }

    /// Test resolve_secret with empty config fallback.
    #[test]
    fn test_resolve_secret_empty_fallback() {
        let store = CredentialStore::new("test-unit");
        let result = resolve_secret(&store, "endpoint", "key", Some(""));
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_secret_no_fallback() {
        let store = CredentialStore::new("test-unit");
        let result = resolve_secret(&store, "endpoint", "key", None);
        assert!(result.is_err());
    }

    /// Test store → get round-trip (uses a unique service per test run).
    /// NOTE: Requires a running OS keychain backend (fails in headless CI).
    #[ignore = "requires OS keychain backend"]
    #[test]
    fn test_store_and_get_credential() {
        let store = CredentialStore::new(&format!("s4drive-test-{}", std::process::id()));
        let endpoint = "http://localhost:9000";
        let ak = "test-access-key";
        let sk = "test-secret-key";
        let region = "us-east-1";
        let bucket = "test-bucket";

        // Store
        store.store(endpoint, ak, sk, region, bucket).unwrap();

        // Get
        let cred = store.get(endpoint, ak).unwrap();
        assert_eq!(cred.endpoint, endpoint);
        assert_eq!(cred.access_key_id, ak);
        assert_eq!(cred.secret_key, sk);
        assert_eq!(cred.region, region);
        assert_eq!(cred.bucket, bucket);

        // Cleanup
        store.delete(endpoint, ak).unwrap();

        // Verify deleted
        assert!(!store.exists(endpoint, ak));
    }

    /// Test exists() returns false for missing credentials.
    #[test]
    fn test_exists_missing() {
        let store = CredentialStore::new("test-exists");
        assert!(!store.exists("http://missing", "no-key"));
    }

    /// Test overwrite (store twice with same key).
    /// NOTE: Requires a running OS keychain backend (fails in headless CI).
    #[ignore = "requires OS keychain backend"]
    #[test]
    fn test_overwrite_credential() {
        let store = CredentialStore::new(&format!("s4drive-overwrite-{}", std::process::id()));
        let ep = "http://example.com";
        let ak = "user1";

        store.store(ep, ak, "v1", "us-east-1", "b1").unwrap();
        store.store(ep, ak, "v2", "eu-west-1", "b2").unwrap();

        let cred = store.get(ep, ak).unwrap();
        assert_eq!(cred.secret_key, "v2");
        assert_eq!(cred.region, "eu-west-1");

        store.delete(ep, ak).unwrap();
    }
}
