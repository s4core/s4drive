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
    /// Bounded lifecycle cleanup and reconciliation settings
    #[serde(default)]
    pub maintenance: MaintenanceConfig,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MaintenanceConfig {
    pub enabled: bool,
    pub local_delete_batch_size: usize,
    pub transfer_retention_hours: u64,
    pub activity_retention_days: u64,
    pub activity_keep_min: usize,
    pub resolved_conflict_retention_days: u64,
    pub deleted_object_retention_days: u64,
    pub revision_retention_days: u64,
    pub min_revisions_per_file: u32,
    pub remote_tombstone_retention_days: u32,
    pub remote_tombstone_batch_size: usize,
    pub remote_snapshot_keep: u32,
    pub remote_blob_gc_enabled: bool,
    pub remote_blob_quarantine_days: u64,
    pub remote_blob_discovery_batch_size: usize,
    pub remote_blob_delete_batch_size: usize,
    pub remote_blob_reference_page_size: i32,
    pub remote_op_compaction_enabled: bool,
    pub remote_op_compaction_batch_size: usize,
    pub remote_op_retention_days: u64,
    pub remote_op_snapshot_interval_ops: usize,
    pub remote_op_snapshot_max_entries: usize,
    pub remote_op_watermark_stale_days: u64,
    pub blob_lease_ttl_minutes: u64,
    pub sqlite_maintenance: bool,
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            local_delete_batch_size: 250,
            transfer_retention_hours: 24,
            activity_retention_days: 30,
            activity_keep_min: 1_000,
            resolved_conflict_retention_days: 30,
            deleted_object_retention_days: 90,
            revision_retention_days: 90,
            min_revisions_per_file: 10,
            remote_tombstone_retention_days: 90,
            remote_tombstone_batch_size: 250,
            remote_snapshot_keep: 5,
            remote_blob_gc_enabled: true,
            remote_blob_quarantine_days: 90,
            remote_blob_discovery_batch_size: 250,
            remote_blob_delete_batch_size: 2,
            remote_blob_reference_page_size: 250,
            remote_op_compaction_enabled: true,
            remote_op_compaction_batch_size: 250,
            remote_op_retention_days: 90,
            remote_op_snapshot_interval_ops: 1_000,
            remote_op_snapshot_max_entries: 50_000,
            remote_op_watermark_stale_days: 90,
            blob_lease_ttl_minutes: 30,
            sqlite_maintenance: true,
        }
    }
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
            maintenance: MaintenanceConfig::default(),
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

    #[test]
    fn load_old_config_without_maintenance_uses_defaults() {
        let text = r#"
[s3]
endpoint = "http://127.0.0.1:9000"
region = "us-east-1"
bucket = "s4drive-test"
access_key_id = "minioadmin"
use_tls = false

[sync_folder]
local_path = "/tmp/s4drive"
bucket_prefix = "/"
polling_interval_sec = 30
max_concurrent_uploads = 4
max_concurrent_downloads = 4
exclude_patterns = []

[core]
db_path = ":memory:"
log_level = "info"
max_retries = 3
"#;

        let config: Config = toml::from_str(text).unwrap();

        assert!(config.maintenance.enabled);
        assert_eq!(config.maintenance.local_delete_batch_size, 250);
        assert_eq!(config.maintenance.remote_tombstone_retention_days, 90);
        assert!(config.maintenance.remote_blob_gc_enabled);
        assert_eq!(config.maintenance.remote_blob_quarantine_days, 90);
        assert!(config.maintenance.remote_op_compaction_enabled);
        assert_eq!(config.maintenance.remote_op_compaction_batch_size, 250);
        assert_eq!(config.maintenance.remote_op_retention_days, 90);
        assert_eq!(config.maintenance.remote_op_snapshot_interval_ops, 1_000);
        assert_eq!(config.maintenance.remote_op_snapshot_max_entries, 50_000);
    }
}
