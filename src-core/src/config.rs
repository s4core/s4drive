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
    pub encrypted_secret_key: Option<String>,
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
                encrypted_secret_key: None,
                use_tls: true,
            },
            sync_folder: SyncFolderConfig {
                local_path: "~/S4Drive".into(),
                bucket_prefix: "/".into(),
                polling_interval_sec: 30,
                bandwidth_limit_kbps: None,
                max_concurrent_uploads: 4,
                max_concurrent_downloads: 4,
            },
            core: CoreConfig {
                db_path: "~/.s4drive/db.sqlite".into(),
                log_level: "info".into(),
                max_retries: 3,
            },
        }
    }
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&content)?)
    }

    pub fn save(&self, path: &str) -> anyhow::Result<()> {
        let content = toml::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }
}

// Re-export toml (used by config module)
pub(crate) use toml;
