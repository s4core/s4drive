/// S4Drive CLI — проверка совместимости S3 и работа с метадатой.
///
/// Использование:
///   cargo run -p s4drive-cli -- check --endpoint https://minio.example.com --bucket my-bucket
///   cargo run -p s4drive-cli -- init-bucket --endpoint http://127.0.0.1:9000 --bucket test
///   cargo run -p s4drive-cli -- metadata status ...
///   cargo run -p s4drive-cli -- metadata tree ...
///   cargo run -p s4drive-cli -- metadata ops ...
///   cargo run -p s4drive-cli -- restore-bucket --input .s4drive --output ./restored-files
use clap::{CommandFactory, Parser, Subcommand};
use rusqlite::OpenFlags;
use s4drive_core::config::Config;
use s4drive_core::metadata::compaction::DeviceWatermarks;
use s4drive_core::metadata::engine::MetadataEngine;
use s4drive_core::metadata::serializer::Serializer;
use s4drive_core::metadata::types::{
    ContentRef, EntryType, FileEntry, FileId, OpType, Operation, SnapshotMetadata,
};
use s4drive_core::metadata::validator::Validator;
use s4drive_core::s3::S3Adapter;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

#[derive(Parser)]
#[command(name = "s4drive", about = "S4Drive CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Проверить совместимость S3 бакета
    Check {
        /// S3 endpoint URL
        #[arg(short, long)]
        endpoint: String,

        /// S3 bucket name
        #[arg(short, long)]
        bucket: String,

        /// Access key ID
        #[arg(short, long)]
        access_key: String,

        /// Secret access key
        #[arg(short = 's', long = "secret")]
        secret_key: String,

        /// AWS region (default: us-east-1)
        #[arg(short, long, default_value = "us-east-1")]
        region: String,
    },

    /// Инициализировать .s4drive/ метадату в бакете
    InitBucket {
        /// S3 endpoint URL
        #[arg(short, long)]
        endpoint: String,

        /// S3 bucket name
        #[arg(short, long)]
        bucket: String,

        /// Access key ID
        #[arg(short, long)]
        access_key: String,

        /// Secret access key
        #[arg(short = 's', long = "secret")]
        secret_key: String,

        /// AWS region (default: us-east-1)
        #[arg(short, long, default_value = "us-east-1")]
        region: String,

        /// Device name (default: hostname)
        #[arg(short, long, default_value = "s4drive-cli")]
        device_name: String,
    },

    /// Восстановить файлы из локальной копии .s4drive без GUI/S3 доступа
    RestoreBucket {
        /// Path to .s4drive or to a directory containing content/, meta/, trash/
        #[arg(long)]
        input: PathBuf,

        /// Directory where restored files will be written
        #[arg(long)]
        output: PathBuf,

        /// Replace existing files in the output directory
        #[arg(long, default_value_t = false)]
        overwrite: bool,
    },

    /// Работа с метадатой бакета
    Metadata {
        #[command(subcommand)]
        action: MetadataAction,
    },
    /// Синхронизация локальной папки с бакетом
    Sync {
        #[command(subcommand)]
        action: SyncAction,
    },
    /// Desktop integration (autostart, extensions, .desktop file)
    Desktop {
        #[command(subcommand)]
        action: DesktopAction,
    },
    /// File manager integration commands (for Nautilus/Thunar extensions)
    Fm {
        #[command(subcommand)]
        action: FmAction,
    },
    /// s4drive:// deep links delegated by the desktop file.
    #[command(external_subcommand)]
    External(Vec<String>),
}

#[derive(Subcommand)]
enum MetadataAction {
    /// Показать статус метадаты (descriptor, counts)
    Status {
        /// S3 endpoint URL
        #[arg(short, long)]
        endpoint: String,

        /// S3 bucket name
        #[arg(short, long)]
        bucket: String,

        /// Access key ID
        #[arg(short, long)]
        access_key: String,

        /// Secret access key
        #[arg(short = 's', long = "secret")]
        secret_key: String,

        /// AWS region (default: us-east-1)
        #[arg(short, long, default_value = "us-east-1")]
        region: String,
    },

    /// Показать дерево файлов
    Tree {
        /// S3 endpoint URL
        #[arg(short, long)]
        endpoint: String,

        /// S3 bucket name
        #[arg(short, long)]
        bucket: String,

        /// Access key ID
        #[arg(short, long)]
        access_key: String,

        /// Secret access key
        #[arg(short = 's', long = "secret")]
        secret_key: String,

        /// AWS region (default: us-east-1)
        #[arg(short, long, default_value = "us-east-1")]
        region: String,

        /// Limit results
        #[arg(short, long, default_value = "50")]
        limit: usize,
    },

    /// Показать operation log
    Ops {
        /// S3 endpoint URL
        #[arg(short, long)]
        endpoint: String,

        /// S3 bucket name
        #[arg(short, long)]
        bucket: String,

        /// Access key ID
        #[arg(short, long)]
        access_key: String,

        /// Secret access key
        #[arg(short = 's', long = "secret")]
        secret_key: String,

        /// AWS region (default: us-east-1)
        #[arg(short, long, default_value = "us-east-1")]
        region: String,

        /// Limit results
        #[arg(short, long, default_value = "20")]
        limit: usize,
    },

    /// Исключить старое устройство из блокирующих GC watermarks
    RetireDevice {
        /// S3 endpoint URL
        #[arg(short, long)]
        endpoint: String,

        /// S3 bucket name
        #[arg(short, long)]
        bucket: String,

        /// Access key ID
        #[arg(short, long)]
        access_key: String,

        /// Secret access key
        #[arg(short = 's', long = "secret")]
        secret_key: String,

        /// AWS region (default: us-east-1)
        #[arg(short, long, default_value = "us-east-1")]
        region: String,

        /// Device UUID to retire
        #[arg(long)]
        device_id: uuid::Uuid,
    },
}

#[derive(Subcommand)]
enum SyncAction {
    Start {
        #[arg(short, long)]
        endpoint: String,
        #[arg(short, long)]
        bucket: String,
        #[arg(short, long)]
        access_key: String,
        #[arg(short = 's', long = "secret")]
        secret_key: String,
        #[arg(short, long, default_value = "us-east-1")]
        region: String,
        #[arg(short, long)]
        local_path: String,
        #[arg(short, long, default_value = "s4drive-cli")]
        device_name: String,
    },
    Status {
        #[arg(short, long)]
        endpoint: String,
        #[arg(short, long)]
        bucket: String,
        #[arg(short, long)]
        access_key: String,
        #[arg(short = 's', long = "secret")]
        secret_key: String,
        #[arg(short, long, default_value = "us-east-1")]
        region: String,
        #[arg(short, long)]
        local_path: String,
    },
}

#[derive(Subcommand)]
enum DesktopAction {
    /// Install platform context menu integration
    InstallContextMenu,
    /// Install the .desktop file (xdg MIME registration)
    InstallDesktop,
    /// Remove the .desktop file
    RemoveDesktop,
    /// Enable autostart (create ~/.config/autostart/s4drive.desktop)
    AutostartEnable,
    /// Disable autostart
    AutostartDisable,
    /// Show autostart status
    AutostartStatus,
    /// Install Nautilus extension
    InstallNautilus,
    /// Install Thunar extension
    InstallThunar,
    /// Uninstall all extensions
    RemoveExtensions,
    /// Show desktop integration status
    Status,
}

#[derive(Subcommand)]
enum FmAction {
    /// Get sync status of a file
    Status {
        /// Path to the file
        path: String,
    },
    /// Trigger sync now
    SyncNow {
        /// Optional folder path (default: current dir)
        path: Option<String>,
    },
    /// Generate a share link for a file
    ShareLink {
        /// Path to the file
        path: String,
    },
    /// Open version history for a file
    VersionHistory {
        /// Path to the file
        path: String,
    },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter("s4drive=info")
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Check {
            endpoint,
            bucket,
            access_key,
            secret_key,
            region,
        } => {
            run_check(&endpoint, &bucket, &access_key, &secret_key, &region).await;
        }
        Commands::InitBucket {
            endpoint,
            bucket,
            access_key,
            secret_key,
            region,
            device_name,
        } => {
            run_init_bucket(
                &endpoint,
                &bucket,
                &access_key,
                &secret_key,
                &region,
                &device_name,
            )
            .await;
        }
        Commands::RestoreBucket {
            input,
            output,
            overwrite,
        } => {
            if let Err(error) = run_restore_bucket(&input, &output, overwrite) {
                eprintln!("  ✗ {}", error);
                std::process::exit(2);
            }
        }
        Commands::Metadata { action } => match action {
            MetadataAction::Status {
                endpoint,
                bucket,
                access_key,
                secret_key,
                region,
            } => {
                run_metadata_status(&endpoint, &bucket, &access_key, &secret_key, &region).await;
            }
            MetadataAction::Tree {
                endpoint,
                bucket,
                access_key,
                secret_key,
                region,
                limit,
            } => {
                run_metadata_tree(&endpoint, &bucket, &access_key, &secret_key, &region, limit)
                    .await;
            }
            MetadataAction::Ops {
                endpoint,
                bucket,
                access_key,
                secret_key,
                region,
                limit,
            } => {
                run_metadata_ops(&endpoint, &bucket, &access_key, &secret_key, &region, limit)
                    .await;
            }
            MetadataAction::RetireDevice {
                endpoint,
                bucket,
                access_key,
                secret_key,
                region,
                device_id,
            } => {
                run_retire_device(
                    &endpoint,
                    &bucket,
                    &access_key,
                    &secret_key,
                    &region,
                    device_id,
                )
                .await;
            }
        },
        Commands::Sync { action } => match action {
            SyncAction::Start {
                endpoint,
                bucket,
                access_key,
                secret_key,
                region,
                local_path,
                device_name,
            } => {
                run_sync_start(
                    &endpoint,
                    &bucket,
                    &access_key,
                    &secret_key,
                    &region,
                    &local_path,
                    &device_name,
                )
                .await;
            }
            SyncAction::Status {
                endpoint,
                bucket,
                access_key,
                secret_key,
                region,
                local_path,
            } => {
                run_sync_status(
                    &endpoint,
                    &bucket,
                    &access_key,
                    &secret_key,
                    &region,
                    &local_path,
                )
                .await;
            }
        },
        Commands::Desktop { action } => match action {
            DesktopAction::InstallContextMenu => run_desktop_install_context_menu(),
            DesktopAction::InstallDesktop => run_desktop_install_desktop(),
            DesktopAction::RemoveDesktop => run_desktop_remove_desktop(),
            DesktopAction::AutostartEnable => run_desktop_autostart_enable(),
            DesktopAction::AutostartDisable => run_desktop_autostart_disable(),
            DesktopAction::AutostartStatus => run_desktop_autostart_status(),
            DesktopAction::InstallNautilus => run_desktop_install_nautilus(),
            DesktopAction::InstallThunar => run_desktop_install_thunar(),
            DesktopAction::RemoveExtensions => run_desktop_remove_extensions(),
            DesktopAction::Status => run_desktop_status(),
        },
        Commands::Fm { action } => match action {
            FmAction::Status { path } => run_fm_status(&path),
            FmAction::SyncNow { path } => run_fm_sync_now(path.as_deref()),
            FmAction::ShareLink { path } => run_fm_share_link(&path),
            FmAction::VersionHistory { path } => run_fm_version_history(&path),
        },
        Commands::External(args) => run_external_command(&args),
    }
}

// ─── Helper: create adapter ──────────────────────────────────────────

fn build_config(
    endpoint: &str,
    bucket: &str,
    access_key: &str,
    secret_key: &str,
    region: &str,
) -> Config {
    let mut config = Config::default();
    config.s3.endpoint = endpoint.to_string();
    config.s3.bucket = bucket.to_string();
    config.s3.region = region.to_string();
    config.s3.access_key_id = access_key.to_string();
    config.s3.secret_key_fallback = Some(secret_key.to_string());
    config
}

// ─── Check ───────────────────────────────────────────────────────────

async fn run_check(endpoint: &str, bucket: &str, access_key: &str, secret_key: &str, region: &str) {
    println!();
    println!("╔══════════════════════════════════════════════╗");
    println!("║   S4Drive — S3 Compatibility Checker        ║");
    println!("╚══════════════════════════════════════════════╝");
    println!();
    println!("  Endpoint:  {}", endpoint);
    println!("  Bucket:    {}", bucket);
    println!("  Region:    {}", region);
    println!();

    let config = build_config(endpoint, bucket, access_key, secret_key, region);

    println!("  Connecting...");
    let adapter = match S3Adapter::new(&config).await {
        Ok(a) => {
            println!("  ✓ S3 client created");
            a
        }
        Err(e) => {
            println!("  ✗ Failed to create S3 client: {}", e);
            return;
        }
    };

    println!("  Checking bucket access...");
    match adapter.check_bucket_access().await {
        Ok(()) => println!("  ✓ Bucket '{}' is accessible", bucket),
        Err(e) => {
            println!("  ✗ {}", e);
            return;
        }
    }

    println!();
    println!("  Running compatibility tests...");
    println!();

    let report = adapter.run_compatibility_test().await;
    println!("{}", report);

    println!("╔══════════════════════════════════════════════╗");
    if report.is_level2_supported() {
        println!(
            "║   ✅ Level {}: {}                         ",
            report.level,
            level_badge(report.level)
        );
    } else {
        println!(
            "║   ❌ Level {}: {}                         ",
            report.level,
            level_badge(report.level)
        );
    }
    println!("╚══════════════════════════════════════════════╝");
    println!();

    if !report.is_level2_supported() {
        println!("  This bucket is below S4Drive Level 2 and will be rejected for sync.");
        std::process::exit(2);
    }
}

// ─── Init Bucket ─────────────────────────────────────────────────────

async fn run_init_bucket(
    endpoint: &str,
    bucket: &str,
    access_key: &str,
    secret_key: &str,
    region: &str,
    device_name: &str,
) {
    println!();
    println!("╔══════════════════════════════════════════════╗");
    println!("║   S4Drive — Bucket Initialization            ║");
    println!("╚══════════════════════════════════════════════╝");
    println!();
    println!("  Endpoint:    {}", endpoint);
    println!("  Bucket:      {}", bucket);
    println!("  Device:      {}", device_name);
    println!();

    let config = build_config(endpoint, bucket, access_key, secret_key, region);

    let adapter = match S3Adapter::new(&config).await {
        Ok(a) => a,
        Err(e) => {
            println!("  ✗ Failed to create S3 client: {}", e);
            return;
        }
    };

    // Check bucket access
    if let Err(e) = adapter.check_bucket_access().await {
        println!("  ✗ Bucket not accessible: {}", e);
        return;
    }
    println!("  ✓ Bucket accessible");

    if let Err(e) = adapter.verify_level2_prerequisites().await {
        println!("  ✗ {}", e);
        println!("  Run 's4drive check' for the full compatibility report.");
        return;
    }
    println!("  ✓ S3 Level 2 prerequisites OK");

    let device_id = uuid::Uuid::now_v7();
    let engine = MetadataEngine::new(adapter, device_id);

    // Check if already initialized
    match engine.check_initialized().await {
        Ok(true) => {
            println!("  ⚠ Bucket already initialized, reading descriptor...");
            match engine.read_descriptor().await {
                Ok(desc) => {
                    println!("  ✓ Bucket ID:   {}", desc.bucket_id);
                    println!(
                        "  ✓ Schema v{}   created at {}",
                        desc.schema_version, desc.created_at
                    );
                }
                Err(e) => println!("  ✗ Failed to read descriptor: {}", e),
            }
            return;
        }
        Ok(false) => {} // fresh bucket
        Err(e) => {
            println!("  ✗ Error checking initialization: {}", e);
            return;
        }
    }

    // Initialize
    println!("  Initializing .s4drive/ metadata structure...");
    match engine.init_bucket(device_name).await {
        Ok(desc) => {
            println!("  ✓ Bucket initialized!");
            println!("  ✓ Bucket ID:   {}", desc.bucket_id);
            println!("  ✓ Schema v{}", desc.schema_version);
            println!("  ✓ Device ID:   {}", device_id);
            println!("  ✓ Created at:  {}", desc.created_at);
        }
        Err(e) => {
            println!("  ✗ Failed to init bucket: {}", e);
        }
    }
    println!();
}

// ─── Metadata Status ─────────────────────────────────────────────────

async fn run_metadata_status(
    endpoint: &str,
    bucket: &str,
    access_key: &str,
    secret_key: &str,
    region: &str,
) {
    let engine = match create_engine(endpoint, bucket, access_key, secret_key, region).await {
        Some(e) => e,
        None => return,
    };

    println!();
    println!("╔══════════════════════════════════════════════╗");
    println!("║   S4Drive — Metadata Status                  ║");
    println!("╚══════════════════════════════════════════════╝");
    println!();

    // Check initialization
    match engine.check_initialized().await {
        Ok(true) => println!("  ✓ Bucket has .s4drive/ metadata"),
        Ok(false) => {
            println!("  ✗ Bucket is NOT initialized. Run 'init-bucket' first.");
            return;
        }
        Err(e) => {
            println!("  ✗ Error: {}", e);
            return;
        }
    }

    // Descriptor
    match engine.read_descriptor().await {
        Ok(desc) => {
            println!();
            println!("  Bucket Descriptor:");
            println!("    ID:              {}", desc.bucket_id);
            println!("    Schema version:  {}", desc.schema_version);
            println!("    Created at:      {}", desc.created_at);
            println!("    Owner:           {}", desc.owner);
            println!("    Min client ver:  {}", desc.min_client_version);
        }
        Err(e) => println!("  ✗ Failed to read descriptor: {}", e),
    }

    // File tree entries
    use s4drive_core::metadata::tree::FileTree;
    let tree = FileTree::new(engine.s3());
    match tree.entry_count().await {
        Ok(count) => println!("  Files in tree:   {}", count),
        Err(e) => println!("  ✗ Tree count:    {}", e),
    }

    println!();
}

// ─── Metadata Tree ───────────────────────────────────────────────────

async fn run_metadata_tree(
    endpoint: &str,
    bucket: &str,
    access_key: &str,
    secret_key: &str,
    region: &str,
    limit: usize,
) {
    let engine = match create_engine(endpoint, bucket, access_key, secret_key, region).await {
        Some(e) => e,
        None => return,
    };

    println!();
    println!("╔══════════════════════════════════════════════╗");
    println!("║   S4Drive — File Tree                        ║");
    println!("╚══════════════════════════════════════════════╝");
    println!();

    use s4drive_core::metadata::tree::FileTree;
    let tree = FileTree::new(engine.s3());

    match tree.list_entries().await {
        Ok(ids) => {
            let total = ids.len();
            let shown = ids.into_iter().take(limit);

            for file_id in shown {
                match tree.get_entry(&file_id).await {
                    Ok(entry) => {
                        let kind = match entry.entry_type {
                            s4drive_core::metadata::types::EntryType::Folder => "📁",
                            _ => "📄",
                        };
                        println!(
                            "  {} {}  ({} bytes, {})",
                            kind,
                            entry.name,
                            entry.size,
                            &entry.file_id.to_string()[..8]
                        );
                    }
                    Err(_) => {
                        println!("  ? {}  (unreadable)", file_id);
                    }
                }
            }

            if total > limit {
                println!("  ... and {} more", total - limit);
            }
            println!("  Total: {} entries", total);
        }
        Err(e) => println!("  ✗ Failed to list tree: {}", e),
    }
    println!();
}

// ─── Metadata Ops ────────────────────────────────────────────────────

async fn run_metadata_ops(
    endpoint: &str,
    bucket: &str,
    access_key: &str,
    secret_key: &str,
    region: &str,
    limit: usize,
) {
    let engine = match create_engine(endpoint, bucket, access_key, secret_key, region).await {
        Some(e) => e,
        None => return,
    };

    println!();
    println!("╔══════════════════════════════════════════════╗");
    println!("║   S4Drive — Operation Log                    ║");
    println!("╚══════════════════════════════════════════════╝");
    println!();

    use s4drive_core::metadata::ops::OperationLog;
    let s3 = engine.s3();
    let device_id = engine.device_id();
    let mut clock = 0u64;
    let mut ops = OperationLog::new(s3, device_id, &mut clock);

    // Load head
    match ops.load_head().await {
        Ok(Some(head)) => println!("  Head pointer: {}", head),
        Ok(None) => println!("  No operations yet"),
        Err(e) => {
            println!("  ✗ Failed to load head: {}", e);
            return;
        }
    }

    match ops.list_operations().await {
        Ok(op_ids) => {
            let total = op_ids.len();
            let shown = op_ids.iter().rev().take(limit);

            for op_id in shown {
                match ops.read_operation(op_id).await {
                    Ok(op) => {
                        println!(
                            "  {} [{:?}] file={:?} clock={}",
                            op_id,
                            op.op_type,
                            op.target_file_id.map(|id| id.to_string()),
                            op.logical_clock,
                        );
                    }
                    Err(_) => {
                        println!("  {} (unreadable)", op_id);
                    }
                }
            }

            if total > limit {
                println!("  ... and {} more", total - limit);
            }
            println!("  Total ops: {}", total);
        }
        Err(e) => println!("  ✗ Failed to list ops: {}", e),
    }
    println!();
}

async fn run_retire_device(
    endpoint: &str,
    bucket: &str,
    access_key: &str,
    secret_key: &str,
    region: &str,
    device_id: uuid::Uuid,
) {
    let engine = match create_engine(endpoint, bucket, access_key, secret_key, region).await {
        Some(engine) => engine,
        None => return,
    };

    match DeviceWatermarks::new(engine.s3()).retire(device_id).await {
        Ok(_) => println!("Retired device {}", device_id),
        Err(error) => println!("Failed to retire device {}: {}", device_id, error),
    }
}

// ─── Restore Bucket ──────────────────────────────────────────────────

#[derive(Debug)]
struct RestoreSnapshot {
    seq_num: Option<u64>,
    covered_head: Option<String>,
    entries: Vec<FileEntry>,
}

#[derive(Debug, Default)]
struct RestoreReport {
    snapshot_seq: Option<u64>,
    entries_seen: usize,
    ops_applied: usize,
    files_restored: usize,
    folders_created: usize,
    deleted_skipped: usize,
    metadata_skipped: usize,
    missing_blobs: usize,
    path_conflicts: usize,
}

impl RestoreReport {
    fn has_data_loss_warnings(&self) -> bool {
        self.missing_blobs > 0 || self.metadata_skipped > 0
    }
}

fn run_restore_bucket(input: &Path, output: &Path, overwrite: bool) -> Result<(), String> {
    println!();
    println!("╔══════════════════════════════════════════════╗");
    println!("║   S4Drive — Bucket Disaster Recovery        ║");
    println!("╚══════════════════════════════════════════════╝");
    println!();

    let metadata_root = locate_s4drive_metadata_root(input)?;
    let metadata_root_abs = absolutize_path(&metadata_root)
        .map_err(|e| format!("resolve input path {}: {}", metadata_root.display(), e))?;
    let output_abs = absolutize_path(output)
        .map_err(|e| format!("resolve output path {}: {}", output.display(), e))?;

    if output_abs == metadata_root_abs || output_abs.starts_with(&metadata_root_abs) {
        return Err(format!(
            "output must not be inside metadata input: {}",
            output.display()
        ));
    }
    if output.exists() && !output.is_dir() {
        return Err(format!(
            "output exists but is not a directory: {}",
            output.display()
        ));
    }

    println!("  Input:      {}", metadata_root.display());
    println!("  Output:     {}", output.display());
    println!("  Overwrite:  {}", if overwrite { "yes" } else { "no" });
    println!();

    let snapshot = load_restore_snapshot(&metadata_root)?;
    let mut entries: HashMap<FileId, FileEntry> = snapshot
        .entries
        .iter()
        .cloned()
        .map(|entry| (entry.file_id, entry))
        .collect();
    let entries_seen = entries.len();
    let current_head = read_current_head(&metadata_root)?;
    let ops_applied = apply_ops_since_snapshot(
        &metadata_root,
        &mut entries,
        snapshot.covered_head.as_deref(),
        current_head.as_deref(),
    )?;

    fs::create_dir_all(output)
        .map_err(|e| format!("create output directory {}: {}", output.display(), e))?;

    let mut report = restore_entries_to_output(&metadata_root, output, &entries, overwrite)?;
    report.snapshot_seq = snapshot.seq_num;
    report.entries_seen = entries_seen;
    report.ops_applied = ops_applied;

    println!();
    println!("  Restore summary:");
    println!(
        "    Snapshot:          {}",
        report
            .snapshot_seq
            .map(|seq| format!("#{}", seq))
            .unwrap_or_else(|| "none".to_string())
    );
    println!("    Entries in tree:   {}", report.entries_seen);
    println!("    Ops applied:       {}", report.ops_applied);
    println!("    Files restored:    {}", report.files_restored);
    println!("    Folders created:   {}", report.folders_created);
    println!("    Deleted skipped:   {}", report.deleted_skipped);
    println!("    Metadata skipped:  {}", report.metadata_skipped);
    println!("    Missing blobs:     {}", report.missing_blobs);
    println!("    Path conflicts:    {}", report.path_conflicts);
    println!();

    if report.has_data_loss_warnings() {
        return Err(
            "restore completed with missing blobs or incomplete metadata; inspect the summary above"
                .to_string(),
        );
    }

    println!("  ✓ Restore completed");
    println!();
    Ok(())
}

fn locate_s4drive_metadata_root(input: &Path) -> Result<PathBuf, String> {
    if input.join("meta").is_dir() && input.join("content").is_dir() {
        return Ok(input.to_path_buf());
    }

    let nested = input.join(".s4drive");
    if nested.join("meta").is_dir() && nested.join("content").is_dir() {
        return Ok(nested);
    }

    Err(format!(
        "input must be .s4drive or contain .s4drive/content and .s4drive/meta: {}",
        input.display()
    ))
}

fn load_restore_snapshot(root: &Path) -> Result<RestoreSnapshot, String> {
    let Some(seq_num) = find_latest_snapshot_seq(root)? else {
        return Ok(RestoreSnapshot {
            seq_num: None,
            covered_head: None,
            entries: Vec::new(),
        });
    };

    let tree_path = root
        .join("meta")
        .join("snapshots")
        .join(format!("{:020}", seq_num))
        .join("tree.json");
    let entries: Vec<FileEntry> = read_json_file(&tree_path)?;
    let metadata_path = root
        .join("meta")
        .join("snapshots")
        .join(format!("{:020}", seq_num))
        .join("metadata.json");
    let covered_head = if metadata_path.exists() {
        let metadata: SnapshotMetadata = read_json_file(&metadata_path)?;
        metadata.covered_head
    } else {
        None
    };

    Ok(RestoreSnapshot {
        seq_num: Some(seq_num),
        covered_head,
        entries,
    })
}

fn find_latest_snapshot_seq(root: &Path) -> Result<Option<u64>, String> {
    let snapshots_dir = root.join("meta").join("snapshots");
    let latest_path = snapshots_dir.join("LATEST");
    if latest_path.exists() {
        let latest = read_utf8_file(&latest_path)?;
        let seq = latest
            .trim()
            .parse::<u64>()
            .map_err(|e| format!("parse {}: {}", latest_path.display(), e))?;
        let tree_path = snapshots_dir.join(format!("{:020}", seq)).join("tree.json");
        if tree_path.exists() {
            return Ok(Some(seq));
        }
    }

    if !snapshots_dir.is_dir() {
        return Ok(None);
    }

    let mut latest_seq = None;
    for entry in fs::read_dir(&snapshots_dir)
        .map_err(|e| format!("read snapshots dir {}: {}", snapshots_dir.display(), e))?
    {
        let entry = entry.map_err(|e| format!("read snapshots entry: {}", e))?;
        if !entry
            .file_type()
            .map_err(|e| format!("read snapshots entry type: {}", e))?
            .is_dir()
        {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(seq) = name.parse::<u64>() else {
            continue;
        };
        if entry.path().join("tree.json").exists() {
            latest_seq = Some(latest_seq.map_or(seq, |current: u64| current.max(seq)));
        }
    }

    Ok(latest_seq)
}

fn read_current_head(root: &Path) -> Result<Option<String>, String> {
    let path = root.join("meta").join("heads").join("current");
    if !path.exists() {
        return Ok(None);
    }
    let head = read_utf8_file(&path)?.trim().to_string();
    if head.is_empty() {
        return Ok(None);
    }
    Ok(Some(head))
}

fn apply_ops_since_snapshot(
    root: &Path,
    entries: &mut HashMap<FileId, FileEntry>,
    covered_head: Option<&str>,
    current_head: Option<&str>,
) -> Result<usize, String> {
    let Some(mut cursor) = current_head.map(ToString::to_string) else {
        return Ok(0);
    };
    if covered_head == Some(cursor.as_str()) {
        return Ok(0);
    }

    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    loop {
        if covered_head == Some(cursor.as_str()) {
            break;
        }
        if !seen.insert(cursor.clone()) {
            return Err(format!("operation chain cycle detected at {}", cursor));
        }
        let op = read_local_operation(root, &cursor)?;
        let base_head = op.base_head.clone();
        chain.push(op);
        if base_head.is_empty() {
            if covered_head.is_some() {
                return Err("operation chain does not reach snapshot covered_head".to_string());
            }
            break;
        }
        cursor = base_head;
    }

    let applied = chain.len();
    for op in chain.iter().rev() {
        apply_restore_operation(entries, op);
    }
    Ok(applied)
}

fn read_local_operation(root: &Path, op_id: &str) -> Result<Operation, String> {
    let path = root
        .join("meta")
        .join("ops")
        .join(format!("{}.json", op_id));
    let data = read_utf8_file(&path)?;
    Serializer::deserialize_operation(&data)
        .map_err(|e| format!("deserialize operation {}: {}", path.display(), e))
}

fn apply_restore_operation(entries: &mut HashMap<FileId, FileEntry>, op: &Operation) {
    let Some(file_id) = op.target_file_id else {
        return;
    };

    match op.op_type {
        OpType::CreateFile | OpType::UploadNewRevision | OpType::Restore => {
            let content_ref = op.effects.new_content_ref.clone();
            let default_name = op
                .effects
                .new_name
                .clone()
                .unwrap_or_else(|| file_id.to_string());
            let entry = entries.entry(file_id).or_insert_with(|| {
                new_restore_entry(
                    file_id,
                    EntryType::File,
                    &default_name,
                    op.effects.new_parent_id,
                    &op.timestamp,
                )
            });
            if let Some(name) = op.effects.new_name.as_deref() {
                entry.name = name.to_string();
                entry.normalized_name = Validator::normalize_name(name).to_lowercase();
            }
            if let Some(parent_id) = op.effects.new_parent_id {
                entry.parent_id = Some(parent_id);
            }
            if let Some(content_ref) = content_ref {
                entry.content_ref = Some(content_ref.clone());
                entry.size = content_ref.size;
                entry.content_hash = Some(format!("blake3:{}", content_ref.hash));
                entry.mime = Some(content_ref.mime);
            }
            if let Some(revision_id) = op.effects.new_revision_id {
                entry.current_revision_id = Some(revision_id);
                if !entry.version_history.contains(&revision_id) {
                    entry.version_history.push(revision_id);
                }
            }
            entry.deleted_at = None;
            entry.updated_at = op.timestamp.clone();
        }
        OpType::CreateFolder => {
            let name = op
                .effects
                .new_name
                .clone()
                .unwrap_or_else(|| file_id.to_string());
            let entry = entries.entry(file_id).or_insert_with(|| {
                new_restore_entry(
                    file_id,
                    EntryType::Folder,
                    &name,
                    op.effects.new_parent_id,
                    &op.timestamp,
                )
            });
            entry.entry_type = EntryType::Folder;
            entry.content_ref = None;
            entry.deleted_at = None;
            entry.updated_at = op.timestamp.clone();
        }
        OpType::Rename | OpType::Move | OpType::UpdateMetadata => {
            if let Some(entry) = entries.get_mut(&file_id) {
                if let Some(name) = op.effects.new_name.as_deref() {
                    entry.name = name.to_string();
                    entry.normalized_name = Validator::normalize_name(name).to_lowercase();
                }
                if let Some(parent_id) = op.effects.new_parent_id {
                    entry.parent_id = Some(parent_id);
                }
                entry.updated_at = op.timestamp.clone();
            }
        }
        OpType::Delete => {
            if let Some(entry) = entries.get_mut(&file_id) {
                entry.deleted_at = Some(op.timestamp.clone());
                entry.updated_at = op.timestamp.clone();
            }
        }
    }
}

fn new_restore_entry(
    file_id: FileId,
    entry_type: EntryType,
    name: &str,
    parent_id: Option<FileId>,
    timestamp: &str,
) -> FileEntry {
    FileEntry {
        file_id,
        parent_id,
        name: name.to_string(),
        normalized_name: Validator::normalize_name(name).to_lowercase(),
        entry_type,
        current_revision_id: None,
        content_ref: None,
        size: 0,
        content_hash: None,
        mime: None,
        created_at: timestamp.to_string(),
        updated_at: timestamp.to_string(),
        deleted_at: None,
        version_history: Vec::new(),
        attributes: Default::default(),
        lock_state: Default::default(),
    }
}

fn restore_entries_to_output(
    root: &Path,
    output: &Path,
    entries: &HashMap<FileId, FileEntry>,
    overwrite: bool,
) -> Result<RestoreReport, String> {
    let mut report = RestoreReport::default();
    let mut planned_paths = HashSet::new();
    let mut restore_items: Vec<(&FileId, &FileEntry, PathBuf)> = Vec::new();

    for (file_id, entry) in entries {
        if entry.deleted_at.is_some() {
            report.deleted_skipped += 1;
            continue;
        }
        let relative_path = match entry_relative_path(file_id, entries) {
            Ok(path) => path,
            Err(error) => {
                report.metadata_skipped += 1;
                eprintln!("  ⚠ Skipping {}: {}", file_id, error);
                continue;
            }
        };
        restore_items.push((file_id, entry, relative_path));
    }

    restore_items.sort_by(|a, b| a.2.cmp(&b.2));

    for (file_id, entry, relative_path) in restore_items {
        match entry.entry_type {
            EntryType::Folder => {
                let target = output.join(&relative_path);
                fs::create_dir_all(&target)
                    .map_err(|e| format!("create folder {}: {}", target.display(), e))?;
                report.folders_created += 1;
            }
            EntryType::File | EntryType::Symlink => {
                let Some(content_ref) = entry.content_ref.as_ref() else {
                    report.metadata_skipped += 1;
                    eprintln!("  ⚠ Skipping {}: file has no content_ref", file_id);
                    continue;
                };
                let source = match blob_source_path(root, content_ref) {
                    Ok(path) => path,
                    Err(error) => {
                        report.metadata_skipped += 1;
                        eprintln!("  ⚠ Skipping {}: {}", file_id, error);
                        continue;
                    }
                };
                if !source.is_file() {
                    report.missing_blobs += 1;
                    eprintln!(
                        "  ⚠ Missing blob for {}: {}",
                        relative_path.display(),
                        source.display()
                    );
                    continue;
                }

                let requested_target = output.join(&relative_path);
                let target = choose_restore_target(
                    &requested_target,
                    *file_id,
                    overwrite,
                    &mut planned_paths,
                );
                if target != requested_target {
                    report.path_conflicts += 1;
                }
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| format!("create parent {}: {}", parent.display(), e))?;
                }
                let copied = fs::copy(&source, &target).map_err(|e| {
                    format!("copy {} -> {}: {}", source.display(), target.display(), e)
                })?;
                if copied != content_ref.size {
                    return Err(format!(
                        "copied size mismatch for {}: expected {}, got {}",
                        target.display(),
                        content_ref.size,
                        copied
                    ));
                }
                report.files_restored += 1;
            }
        }
    }

    Ok(report)
}

fn entry_relative_path(
    file_id: &FileId,
    entries: &HashMap<FileId, FileEntry>,
) -> Result<PathBuf, String> {
    let mut visiting = HashSet::new();
    entry_relative_path_inner(file_id, entries, &mut visiting)
}

fn entry_relative_path_inner(
    file_id: &FileId,
    entries: &HashMap<FileId, FileEntry>,
    visiting: &mut HashSet<FileId>,
) -> Result<PathBuf, String> {
    if !visiting.insert(*file_id) {
        return Err("parent cycle detected".to_string());
    }
    let entry = entries
        .get(file_id)
        .ok_or_else(|| format!("missing entry {}", file_id))?;
    let own_path = safe_relative_path(&entry.name)?;
    let result = if let Some(parent_id) = entry.parent_id {
        if entries.contains_key(&parent_id) {
            let mut parent_path = entry_relative_path_inner(&parent_id, entries, visiting)?;
            parent_path.push(own_path);
            parent_path
        } else {
            own_path
        }
    } else {
        own_path
    };
    visiting.remove(file_id);
    Ok(result)
}

fn safe_relative_path(path: &str) -> Result<PathBuf, String> {
    if path.trim().is_empty() {
        return Err("empty path".to_string());
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Err(format!("absolute path is not allowed: {}", path));
    }
    let normalized = path.replace('\\', "/");
    if looks_like_windows_absolute_path(&normalized) {
        return Err(format!("absolute path is not allowed: {}", path));
    }

    let mut result = PathBuf::new();
    for component in normalized.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            return Err(format!(
                "parent directory component is not allowed: {}",
                path
            ));
        }
        if component.as_bytes().contains(&0) {
            return Err("path contains NUL byte".to_string());
        }
        result.push(component);
    }

    if result.as_os_str().is_empty() {
        return Err(format!("path has no usable components: {}", path));
    }
    Ok(result)
}

fn looks_like_windows_absolute_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3 && bytes[1] == b':' && bytes[2] == b'/'
}

fn blob_source_path(root: &Path, content_ref: &ContentRef) -> Result<PathBuf, String> {
    let key = content_ref
        .storage_key
        .strip_prefix(".s4drive/")
        .unwrap_or(&content_ref.storage_key);
    let candidate = root.join(safe_relative_path(key)?);
    if candidate.exists() {
        return Ok(candidate);
    }

    if content_ref.hash.len() >= 2 {
        let prefix = content_ref
            .hash
            .get(..2)
            .ok_or_else(|| "invalid content hash prefix".to_string())?;
        return Ok(root
            .join("content")
            .join("blobs")
            .join(prefix)
            .join(&content_ref.hash));
    }

    Err(format!(
        "content_ref has invalid storage key and hash: {}",
        content_ref.storage_key
    ))
}

fn choose_restore_target(
    requested: &Path,
    file_id: FileId,
    overwrite: bool,
    planned_paths: &mut HashSet<PathBuf>,
) -> PathBuf {
    if overwrite && !planned_paths.contains(requested) {
        planned_paths.insert(requested.to_path_buf());
        return requested.to_path_buf();
    }
    if !requested.exists() && !planned_paths.contains(requested) {
        planned_paths.insert(requested.to_path_buf());
        return requested.to_path_buf();
    }

    let mut counter = 1usize;
    loop {
        let candidate = restore_conflict_path(requested, file_id, counter);
        if !candidate.exists() && !planned_paths.contains(&candidate) {
            planned_paths.insert(candidate.clone());
            return candidate;
        }
        counter += 1;
    }
}

fn restore_conflict_path(requested: &Path, file_id: FileId, counter: usize) -> PathBuf {
    let suffix = format!("restored {} {}", &file_id.to_string()[..8], counter);
    let file_name = requested
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("restored-file");
    let candidate_name = match requested.extension().and_then(|ext| ext.to_str()) {
        Some(ext) if file_name.len() > ext.len() + 1 => {
            let stem_len = file_name.len() - ext.len() - 1;
            format!("{} ({suffix}).{}", &file_name[..stem_len], ext)
        }
        _ => format!("{file_name} ({suffix})"),
    };
    requested.with_file_name(candidate_name)
}

fn read_json_file<T>(path: &Path) -> Result<T, String>
where
    T: serde::de::DeserializeOwned,
{
    let text = read_utf8_file(path)?;
    serde_json::from_str(&text).map_err(|e| format!("parse {}: {}", path.display(), e))
}

fn read_utf8_file(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("read {}: {}", path.display(), e))
}

// ─── Helpers ─────────────────────────────────────────────────────────

async fn create_engine(
    endpoint: &str,
    bucket: &str,
    access_key: &str,
    secret_key: &str,
    region: &str,
) -> Option<MetadataEngine> {
    let config = build_config(endpoint, bucket, access_key, secret_key, region);
    let adapter = match S3Adapter::new(&config).await {
        Ok(a) => a,
        Err(e) => {
            println!("  ✗ Failed to create S3 client: {}", e);
            return None;
        }
    };
    let device_id = uuid::Uuid::now_v7();
    Some(MetadataEngine::new(adapter, device_id))
}

fn level_badge(level: u32) -> &'static str {
    match level {
        0 => "NOT SUPPORTED",
        1 => "BASIC STORAGE ONLY",
        2 => "SAFE SYNC ✓",
        3 => "VERSIONED SYNC ✓",
        4 => "FULL ENHANCED ✓",
        _ => "UNKNOWN",
    }
}

// ─── Sync Commands ────────────────────────────────────────────────────

async fn run_sync_start(
    endpoint: &str,
    bucket: &str,
    access_key: &str,
    secret_key: &str,
    region: &str,
    local_path: &str,
    device_name: &str,
) {
    println!();
    println!("╔══════════════════════════════════════════════╗");
    println!("║   S4Drive — Sync Start                      ║");
    println!("╚══════════════════════════════════════════════╝");
    println!();
    println!("  Endpoint:    {}", endpoint);
    println!("  Bucket:      {}", bucket);
    println!("  Local path:  {}", local_path);
    println!("  Device:      {}", device_name);
    println!();

    let mut config = build_config(endpoint, bucket, access_key, secret_key, region);
    config.sync_folder.local_path = local_path.to_string();
    config.sync_folder.polling_interval_sec = 10;

    let mut core = s4drive_core::core::S4DriveCore::new(config);

    // Init core
    if let Err(e) = core.init().await {
        println!("  ✗ Init failed: {}", e);
        return;
    }
    println!("  ✓ Core initialized");

    // Init bucket if needed
    match core.check_initialized().await {
        Ok(true) => println!("  ✓ Bucket already has .s4drive/ metadata"),
        Ok(false) => {
            println!("  Initializing bucket...");
            if let Err(e) = core.init_bucket(device_name).await {
                println!("  ✗ Bucket init failed: {}", e);
                return;
            }
            println!("  ✓ Bucket initialized");
        }
        Err(e) => {
            println!("  ✗ Check failed: {}", e);
            return;
        }
    }

    // Start sync
    if let Err(e) = core.start().await {
        println!("  ✗ Sync start failed: {}", e);
        return;
    }
    println!("  ✓ Sync engine started");
    println!("  ✓ Monitoring: {}", local_path);
    println!();
    println!("  Press Ctrl+C to stop");
    println!();

    // Keep running until Ctrl+C
    tokio::signal::ctrl_c().await.ok();
    println!("  Shutting down...");
    let _ = core.stop().await;
    println!("  ✓ Sync stopped");
    println!();
}

async fn run_sync_status(
    endpoint: &str,
    bucket: &str,
    access_key: &str,
    secret_key: &str,
    region: &str,
    local_path: &str,
) {
    println!();
    println!("╔══════════════════════════════════════════════╗");
    println!("║   S4Drive — Sync Status                     ║");
    println!("╚══════════════════════════════════════════════╝");
    println!();

    let mut config = build_config(endpoint, bucket, access_key, secret_key, region);
    config.sync_folder.local_path = local_path.to_string();

    let mut core = s4drive_core::core::S4DriveCore::new(config);

    if let Err(e) = core.init().await {
        println!("  ✗ Init failed: {}", e);
        return;
    }

    let initialized = core.check_initialized().await.unwrap_or(false);
    println!(
        "  Bucket initialized:  {}",
        if initialized { "✓" } else { "✗" }
    );

    if core.health_check() {
        println!("  Core health:         ✓");
    } else {
        println!("  Core health:         ⚠");
    }

    if let Some(sync) = &core.sync {
        println!("  Sync state:          {:?}", sync.current_state());
        println!(
            "  Sync running:        {}",
            if sync.is_running() { "✓" } else { "✗" }
        );
        println!(
            "  Sync paused:         {}",
            if sync.is_paused() { "yes" } else { "no" }
        );
    }

    if let Some(db) = &core.db {
        println!(
            "  Database healthy:    {}",
            if db.is_healthy() { "✓" } else { "✗" }
        );
    }

    if let Some(transfer) = &core.transfer {
        if let Ok((up, down)) = transfer.pending_count() {
            println!("  Pending:             {} uploads, {} downloads", up, down);
        }
    }

    println!();
}

// ─── Desktop Integration Commands ─────────────────────────────────────

fn get_cli_path() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "s4drive".to_string())
}

fn get_app_path() -> String {
    std::env::var("S4DRIVE_APP_BIN")
        .or_else(|_| std::env::var("S4DRIVE_TAURI_BIN"))
        .unwrap_or_else(|_| get_cli_path())
}

fn run_desktop_install_context_menu() {
    let cli = get_cli_path();
    match s4drive_core::desktop::install_context_menu(&cli) {
        Ok(paths) if paths.is_empty() => println!("No desktop context menu files were installed"),
        Ok(paths) => {
            for path in paths {
                println!("✓ Context integration file: {}", path.display());
            }
        }
        Err(e) => eprintln!("✗ Failed to install context menu integration: {}", e),
    }
}

fn run_desktop_install_desktop() {
    let app = get_app_path();
    match s4drive_core::desktop::install_desktop_file(&app) {
        Ok(path) => println!("✓ Desktop file installed: {}", path.display()),
        Err(e) => eprintln!("✗ Failed to install desktop file: {}", e),
    }
    // Also update MIME database
    if let Err(e) = std::process::Command::new("update-desktop-database")
        .arg(s4drive_core::desktop::XdgPaths::applications())
        .output()
    {
        eprintln!("  ⚠ update-desktop-database failed: {}", e);
        eprintln!("  (MIME types may not register until next login)");
    }
}

fn run_desktop_remove_desktop() {
    match s4drive_core::desktop::remove_desktop_file() {
        Ok(()) => println!("✓ Desktop file removed"),
        Err(e) => eprintln!("✗ Failed to remove desktop file: {}", e),
    }
}

fn run_desktop_autostart_enable() {
    let app = get_app_path();
    match s4drive_core::desktop::install_platform_autostart(&app) {
        Ok(path) => println!("✓ Autostart enabled: {}", path.display()),
        Err(e) => eprintln!("✗ Failed to enable autostart: {}", e),
    }
}

fn run_desktop_autostart_disable() {
    match s4drive_core::desktop::disable_autostart() {
        Ok(()) => println!("✓ Autostart disabled"),
        Err(e) => eprintln!("✗ Failed to disable autostart: {}", e),
    }
}

fn run_desktop_autostart_status() {
    let enabled = s4drive_core::desktop::autostart_enabled();
    println!("{}", if enabled { "enabled" } else { "disabled" });
}

fn run_desktop_install_nautilus() {
    let cli = get_cli_path();
    match s4drive_core::desktop::install_nautilus_extension(&cli) {
        Ok(path) => {
            println!("✓ Nautilus extension installed: {}", path.display());
            println!("  Restart Nautilus: nautilus -q && nautilus &");
        }
        Err(e) => eprintln!("✗ Failed to install Nautilus extension: {}", e),
    }
}

fn run_desktop_install_thunar() {
    let cli = get_cli_path();
    match s4drive_core::desktop::install_thunar_extension(&cli) {
        Ok(path) => {
            println!("✓ Thunar extension installed: {}", path.display());
            println!("  Restart Thunar: thunar -q && thunar &");
        }
        Err(e) => eprintln!("✗ Failed to install Thunar extension: {}", e),
    }
}

fn run_desktop_remove_extensions() {
    match s4drive_core::desktop::uninstall_extensions() {
        Ok(()) => println!("✓ Extensions removed"),
        Err(e) => eprintln!("✗ Failed to remove extensions: {}", e),
    }
}

fn run_desktop_status() {
    use s4drive_core::desktop;

    println!();
    println!("╔══════════════════════════════════════════════╗");
    println!("║   S4Drive — Desktop Integration Status      ║");
    println!("╚══════════════════════════════════════════════╝");
    println!();

    let desktop_path =
        desktop::XdgPaths::applications().join(format!("{}.desktop", desktop::APP_ID));
    println!(
        "  Desktop file:    {}",
        if desktop_path.exists() {
            "✓ installed"
        } else {
            "✗ not installed"
        }
    );
    println!(
        "  Autostart:       {}",
        if desktop::autostart_enabled() {
            "✓ enabled"
        } else {
            "✗ disabled"
        }
    );

    let nautilus_path = desktop::XdgPaths::nautilus_extensions().join("s4drive-nautilus.py");
    println!(
        "  Nautilus ext:    {}",
        if nautilus_path.exists() {
            "✓ installed"
        } else {
            "✗ not installed"
        }
    );

    let thunar_path = desktop::XdgPaths::thunar_extensions().join("s4drive-thunar.py");
    println!(
        "  Thunar ext:      {}",
        if thunar_path.exists() {
            "✓ installed"
        } else {
            "✗ not installed"
        }
    );

    println!();
    println!("  CLI path:  {}", get_cli_path());
    println!("  App path:  {}", get_app_path());
    println!("  Data dir:  {}", desktop::XdgPaths::data_dir().display());
    println!();
}

// ─── File Manager (Fm) Commands ──────────────────────────────────────

#[derive(Debug, Clone)]
struct FmFileInfo {
    file_id: String,
    state: String,
}

fn run_fm_status(path: &str) {
    match lookup_fm_file(Path::new(path)) {
        Ok(Some(info)) => println!("{} {}", normalize_fm_state(&info.state), info.file_id),
        Ok(None) => println!("none"),
        Err(error) => {
            tracing::debug!("file-manager status lookup failed: {}", error);
            println!("unknown");
        }
    }
}

fn run_fm_sync_now(path: Option<&str>) {
    if let Some(path) = path {
        let url = format!("s4drive://sync-now?path={}", percent_encode(path));
        let _ = open_url_best_effort(&url);
    }
    println!("ok");
}

fn run_fm_share_link(path: &str) {
    let link = match lookup_fm_file(Path::new(path)) {
        Ok(Some(info)) => format!("s4drive://files/{}", info.file_id),
        Ok(None) | Err(_) => format!("s4drive://paths/{}", percent_encode(path)),
    };
    let _ = copy_to_clipboard(&link);
    println!("{}", link);
}

fn run_fm_version_history(path: &str) {
    let url = match lookup_fm_file(Path::new(path)) {
        Ok(Some(info)) => format!("s4drive://versions/{}", info.file_id),
        Ok(None) | Err(_) => format!("s4drive://versions?path={}", percent_encode(path)),
    };
    let _ = open_url_best_effort(&url);
    println!("{}", url);
}

fn run_external_command(args: &[String]) {
    let Some(first) = args.first() else {
        let _ = Cli::command().print_help();
        println!();
        return;
    };

    if first.starts_with("s4drive://") {
        run_open_deep_link(first);
    } else {
        eprintln!("Unknown command: {}", first);
        std::process::exit(2);
    }
}

fn run_open_deep_link(url: &str) {
    if let Ok(app_bin) = std::env::var("S4DRIVE_APP_BIN") {
        if let Err(error) = std::process::Command::new(app_bin).arg(url).spawn() {
            tracing::debug!("failed to delegate deep link to app: {}", error);
        }
    }
    println!("{}", url);
}

fn lookup_fm_file(path: &Path) -> Result<Option<FmFileInfo>, String> {
    let config = load_fm_config();
    let sync_folder = absolutize_path(&expand_home_path(&config.sync_folder.local_path))
        .map_err(|e| e.to_string())?;
    let requested = absolutize_path(path).map_err(|e| e.to_string())?;

    if !requested.starts_with(&sync_folder) {
        return Ok(None);
    }

    let db_path = expand_home_path(&config.core.db_path);
    if !db_path.exists() {
        return Ok(None);
    }

    let conn = rusqlite::Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("open {}: {}", db_path.display(), e))?;

    let requested_text = requested.to_string_lossy().to_string();
    if let Some(info) = query_fm_file_by_local_path(&conn, &requested_text)? {
        return Ok(Some(info));
    }

    let relative_text = requested
        .strip_prefix(&sync_folder)
        .ok()
        .map(|p| p.to_string_lossy().trim_start_matches('/').to_string())
        .filter(|p| !p.is_empty());

    match relative_text {
        Some(relative) => query_fm_file_by_local_path(&conn, &relative),
        None => Ok(None),
    }
}

fn query_fm_file_by_local_path(
    conn: &rusqlite::Connection,
    local_path: &str,
) -> Result<Option<FmFileInfo>, String> {
    let result = conn.query_row(
        "SELECT file_id, state FROM objects WHERE local_path = ?1 LIMIT 1",
        [local_path],
        |row| {
            Ok(FmFileInfo {
                file_id: row.get(0)?,
                state: row.get(1)?,
            })
        },
    );

    match result {
        Ok(info) => Ok(Some(info)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn load_fm_config() -> Config {
    for candidate in fm_config_candidates() {
        if candidate.exists() {
            if let Ok(config) = Config::load(&candidate.to_string_lossy()) {
                return config;
            }
        }
    }
    Config::default()
}

fn fm_config_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(path) = std::env::var("S4DRIVE_CONFIG") {
        candidates.push(expand_home_path(&path));
    }
    candidates.push(expand_home_path("~/.s4drive/config.toml"));
    if let Some(config_dir) = dirs::config_dir() {
        candidates.push(config_dir.join("s4drive/config.toml"));
    }
    candidates
}

fn expand_home_path(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    PathBuf::from(path)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn absolutize_path(path: &Path) -> std::io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn normalize_fm_state(state: &str) -> &'static str {
    match state {
        "synced" => "synced",
        "pending_upload" | "pending_download" | "in_progress" | "queued" => "syncing",
        "conflicted" | "conflict" => "conflict",
        "failed" | "error" => "error",
        "paused" => "paused",
        "deleted" | "deleted_locally" => "deleted",
        "ignored" => "ignored",
        _ => "unknown",
    }
}

fn percent_encode(input: &str) -> String {
    let mut encoded = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{:02X}", byte)),
        }
    }
    encoded
}

fn copy_to_clipboard(text: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        return write_to_command("pbcopy", &[], text);
    }
    #[cfg(target_os = "windows")]
    {
        return write_to_command(
            "powershell",
            &["-NoProfile", "-Command", "Set-Clipboard"],
            text,
        );
    }
    #[cfg(target_os = "linux")]
    {
        write_to_command("wl-copy", &[], text)
            || write_to_command("xclip", &["-selection", "clipboard"], text)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = text;
        false
    }
}

fn write_to_command(program: &str, args: &[&str], input: &str) -> bool {
    let mut child = match std::process::Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };

    if let Some(stdin) = child.stdin.as_mut() {
        if stdin.write_all(input.as_bytes()).is_err() {
            return false;
        }
    }

    child.wait().map(|status| status.success()).unwrap_or(false)
}

fn open_url_best_effort(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(url).spawn();

    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();

    #[cfg(target_os = "linux")]
    let result = std::process::Command::new("xdg-open").arg(url).spawn();

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = url;
        return false;
    }

    result.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fm_state_is_normalized_for_file_manager_badges() {
        assert_eq!(normalize_fm_state("synced"), "synced");
        assert_eq!(normalize_fm_state("pending_upload"), "syncing");
        assert_eq!(normalize_fm_state("pending_download"), "syncing");
        assert_eq!(normalize_fm_state("conflicted"), "conflict");
        assert_eq!(normalize_fm_state("deleted_locally"), "deleted");
        assert_eq!(normalize_fm_state("something-new"), "unknown");
    }

    #[test]
    fn percent_encode_makes_deep_link_segments_safe() {
        assert_eq!(percent_encode("docs/report 1.txt"), "docs%2Freport%201.txt");
        assert_eq!(percent_encode("abc-_.~"), "abc-_.~");
    }

    #[test]
    fn safe_relative_path_rejects_escape_paths() {
        assert!(safe_relative_path("../secret.txt").is_err());
        assert!(safe_relative_path("/tmp/secret.txt").is_err());
        assert!(safe_relative_path("C:\\tmp\\secret.txt").is_err());
        assert_eq!(
            safe_relative_path("docs/report.txt").unwrap(),
            PathBuf::from("docs").join("report.txt")
        );
    }

    #[test]
    fn entry_relative_path_uses_parent_chain() {
        let folder_id = uuid::Uuid::now_v7();
        let file_id = uuid::Uuid::now_v7();
        let mut entries = HashMap::new();
        entries.insert(
            folder_id,
            test_entry(folder_id, None, "docs", EntryType::Folder, None),
        );
        entries.insert(
            file_id,
            test_entry(
                file_id,
                Some(folder_id),
                "report.txt",
                EntryType::File,
                Some(test_content_ref("ab")),
            ),
        );

        assert_eq!(
            entry_relative_path(&file_id, &entries).unwrap(),
            PathBuf::from("docs").join("report.txt")
        );
    }

    #[test]
    fn restore_bucket_applies_ops_after_snapshot_and_copies_blob() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".s4drive");
        let output = temp.path().join("restored");
        let hash = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let blob = root.join("content").join("blobs").join("ab").join(hash);
        fs::create_dir_all(blob.parent().unwrap()).unwrap();
        fs::write(&blob, b"new content").unwrap();

        let file_id = uuid::Uuid::now_v7();
        let snapshot_dir = root
            .join("meta")
            .join("snapshots")
            .join("00000000000000000001");
        fs::create_dir_all(&snapshot_dir).unwrap();
        fs::write(root.join("meta").join("snapshots").join("LATEST"), b"1").unwrap();
        let snapshot_entry = test_entry(
            file_id,
            None,
            "docs/report.txt",
            EntryType::File,
            Some(test_content_ref("oldhash")),
        );
        fs::write(
            snapshot_dir.join("tree.json"),
            serde_json::to_string_pretty(&vec![snapshot_entry]).unwrap(),
        )
        .unwrap();
        let metadata = SnapshotMetadata {
            schema_version: 1,
            seq_num: 1,
            created_at: "2026-06-08T00:00:00Z".to_string(),
            entry_count: 1,
            tree_key: ".s4drive/meta/snapshots/00000000000000000001/tree.json".to_string(),
            covered_head: Some("op1".to_string()),
        };
        fs::write(
            snapshot_dir.join("metadata.json"),
            serde_json::to_string_pretty(&metadata).unwrap(),
        )
        .unwrap();

        let op = Operation {
            op_id: "op2".to_string(),
            device_id: uuid::Uuid::now_v7(),
            actor_id: "test".to_string(),
            logical_clock: 2,
            base_head: "op1".to_string(),
            target_file_id: Some(file_id),
            op_type: OpType::UploadNewRevision,
            preconditions: s4drive_core::metadata::types::Preconditions {
                expected_etag: None,
                expected_version_id: None,
                file_exists: true,
                parent_exists: true,
            },
            effects: s4drive_core::metadata::types::Effects {
                new_revision_id: Some(uuid::Uuid::now_v7()),
                new_content_ref: Some(ContentRef {
                    blob_id: uuid::Uuid::now_v7(),
                    hash: hash.to_string(),
                    size: 11,
                    mime: "text/plain".to_string(),
                    storage_key: format!(".s4drive/content/blobs/ab/{hash}"),
                }),
                new_name: None,
                new_parent_id: None,
                deleted: false,
            },
            timestamp: "2026-06-08T00:01:00Z".to_string(),
            signature: None,
        };
        let ops_dir = root.join("meta").join("ops");
        fs::create_dir_all(&ops_dir).unwrap();
        fs::write(
            ops_dir.join("op2.json"),
            Serializer::serialize_operation(&op).unwrap(),
        )
        .unwrap();
        fs::create_dir_all(root.join("meta").join("heads")).unwrap();
        fs::write(root.join("meta").join("heads").join("current"), b"op2").unwrap();

        run_restore_bucket(&root, &output, false).unwrap();

        assert_eq!(
            fs::read(output.join("docs").join("report.txt")).unwrap(),
            b"new content"
        );
    }

    fn test_content_ref(hash: &str) -> ContentRef {
        ContentRef {
            blob_id: uuid::Uuid::now_v7(),
            hash: hash.to_string(),
            size: 0,
            mime: "application/octet-stream".to_string(),
            storage_key: format!(
                ".s4drive/content/blobs/{}/{}",
                &hash[..2.min(hash.len())],
                hash
            ),
        }
    }

    fn test_entry(
        file_id: FileId,
        parent_id: Option<FileId>,
        name: &str,
        entry_type: EntryType,
        content_ref: Option<ContentRef>,
    ) -> FileEntry {
        FileEntry {
            file_id,
            parent_id,
            name: name.to_string(),
            normalized_name: name.to_lowercase(),
            entry_type,
            current_revision_id: None,
            size: content_ref
                .as_ref()
                .map(|content| content.size)
                .unwrap_or(0),
            content_hash: content_ref
                .as_ref()
                .map(|content| format!("blake3:{}", content.hash)),
            mime: content_ref.as_ref().map(|content| content.mime.clone()),
            content_ref,
            created_at: "2026-06-08T00:00:00Z".to_string(),
            updated_at: "2026-06-08T00:00:00Z".to_string(),
            deleted_at: None,
            version_history: Vec::new(),
            attributes: Default::default(),
            lock_state: Default::default(),
        }
    }
}
