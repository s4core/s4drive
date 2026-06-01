use serde::{Deserialize, Serialize};

/// S4Drive configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// S3 connection settings
    pub s3: S3Config,
    /// Local sync folder
    pub sync_folder: SyncFolderConfig,
    /// Core behavior settings
    pub core: CoreConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3Config {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key_id: String,
    /// Encrypted secret key (keychain-managed)
    /// Fallback secret key (plaintext, only used when keychain unavailable).
    pub secret_key_fallback: Option<String>,
    pub use_tls: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncFolderConfig {
    pub local_path: String,
    pub bucket_prefix: String,
    pub polling_interval_sec: u64,
    pub bandwidth_limit_kbps: Option<u64>,
    pub max_concurrent_uploads: u32,
    pub max_concurrent_downloads: u32,
    #[serde(default = "default_exclude_patterns")]
    pub exclude_patterns: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreConfig {
    pub db_path: String,
    pub log_level: String,
    pub max_retries: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            s3: S3Config {
                endpoint: String::new(),
                region: "us-east-1".into(),
                bucket: String::new(),
                access_key_id: String::new(),
                secret_key_fallback: None,
                use_tls: true,
            },
            sync_folder: SyncFolderConfig {
                local_path: "~/S4Drive".into(),
                bucket_prefix: "/".into(),
                polling_interval_sec: 30,
                bandwidth_limit_kbps: None,
                max_concurrent_uploads: 4,
                max_concurrent_downloads: 4,
                exclude_patterns: default_exclude_patterns(),
            },
            core: CoreConfig {
                db_path: "~/.s4drive/db.sqlite".into(),
                log_level: "info".into(),
                max_retries: 3,
            },
        }
    }
}

pub fn default_exclude_patterns() -> Vec<String> {
    vec!["node_modules".to_string(), ".DS_Store".to_string()]
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let path = shellexpand::tilde(path).to_string();
        let content = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&content)?)
    }

    pub fn save(&self, path: &str) -> anyhow::Result<()> {
        let path = shellexpand::tilde(path).to_string();
        if let Some(parent) = std::path::Path::new(&path)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }
}

// Re-export toml (used by config module)
pub(crate) use toml;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_creates_parent_directories_and_loads_roundtrip() {
        let dir = std::env::temp_dir().join(format!("s4drive-config-{}", uuid::Uuid::now_v7()));
        let path = dir.join("nested").join("config.toml");
        let mut config = Config::default();
        config.s3.endpoint = "http://127.0.0.1:9000".to_string();
        config.s3.bucket = "s4drive-test".to_string();
        config.sync_folder.local_path = "/tmp/s4drive-sync".to_string();

        config.save(&path.to_string_lossy()).unwrap();
        let loaded = Config::load(&path.to_string_lossy()).unwrap();

        assert_eq!(loaded.s3.endpoint, config.s3.endpoint);
        assert_eq!(loaded.s3.bucket, config.s3.bucket);
        assert_eq!(loaded.sync_folder.local_path, config.sync_folder.local_path);

        let _ = std::fs::remove_dir_all(dir);
    }
}
