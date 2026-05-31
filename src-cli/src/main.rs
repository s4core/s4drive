/// S4Drive CLI — проверка совместимости S3 и работа с метадатой.
///
/// Использование:
///   cargo run -p s4drive-cli -- check --endpoint https://minio.example.com --bucket my-bucket
///   cargo run -p s4drive-cli -- init-bucket --endpoint http://127.0.0.1:9000 --bucket test
///   cargo run -p s4drive-cli -- metadata status ...
///   cargo run -p s4drive-cli -- metadata tree ...
///   cargo run -p s4drive-cli -- metadata ops ...
use clap::{Parser, Subcommand};
use s4drive_core::config::Config;
use s4drive_core::metadata::engine::MetadataEngine;
use s4drive_core::s3::S3Adapter;

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

fn run_desktop_install_desktop() {
    let cli = get_cli_path();
    match s4drive_core::desktop::install_desktop_file(&cli) {
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
    let cli = get_cli_path();
    // Auto-detect Tauri binary or CLI
    let tauri_path = std::env::var("S4DRIVE_TAURI_BIN")
        .ok()
        .unwrap_or_else(|| cli.clone());
    let autostart_exec = if tauri_path.contains("s4drive") && tauri_path != cli {
        tauri_path
    } else {
        // Use CLI with tray flag as fallback
        format!("{} tray", cli)
    };
    match s4drive_core::desktop::enable_autostart(&autostart_exec) {
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
    println!("  Data dir:  {}", desktop::XdgPaths::data_dir().display());
    println!();
}

// ─── File Manager (Fm) Commands ──────────────────────────────────────

fn run_fm_status(path: &str) {
    let path = std::path::Path::new(path);
    if !path.exists() {
        eprintln!("error");
        return;
    }

    // Try to find an S4Drive config to determine the sync folder
    let config_path = dirs::config_dir().map(|p| p.join("s4drive/config.toml"));

    match config_path.filter(|p| p.exists()) {
        Some(cfg_path) => {
            let config_content = std::fs::read_to_string(&cfg_path).unwrap_or_default();
            let sync_folder = config_content
                .lines()
                .find(|l| l.contains("local_path"))
                .and_then(|l| l.split('=').nth(1))
                .map(|s| s.trim().trim_matches('"').to_string());

            match sync_folder {
                Some(folder) if path.starts_with(&folder) => {
                    // Path is within sync folder — query DB
                    let db_path = dirs::data_dir()
                        .map(|d| d.join("s4drive/s4drive.db"))
                        .filter(|p| p.exists());

                    match db_path {
                        Some(db) => match rusqlite::Connection::open(&db) {
                            Ok(conn) => {
                                let rel = path
                                    .strip_prefix(&folder)
                                    .unwrap_or(path)
                                    .to_string_lossy()
                                    .to_string();
                                let stmt = conn
                                    .prepare(
                                        "SELECT state FROM objects WHERE local_path = ?1 LIMIT 1",
                                    )
                                    .ok();
                                match stmt {
                                    Some(mut s) => {
                                        let state: Result<String, _> =
                                            s.query_row([&rel], |row| row.get(0));
                                        match state {
                                            Ok(s) => println!("{}", s),
                                            Err(_) => println!("unknown"),
                                        }
                                    }
                                    None => println!("unknown"),
                                }
                            }
                            Err(_) => println!("unknown"),
                        },
                        None => println!("unknown"),
                    }
                }
                _ => println!("none"),
            }
        }
        None => println!("none"),
    }
}

fn run_fm_sync_now(path: Option<&str>) {
    println!("ok");
    if let Some(p) = path {
        eprintln!("Sync triggered for: {}", p);
    }
}

fn run_fm_share_link(path: &str) {
    let abs = std::path::Path::new(path);
    let name = abs
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_else(|| path.into());
    println!("s4drive://share/{}", name);
}

fn run_fm_version_history(path: &str) {
    let abs = std::path::Path::new(path);
    let name = abs
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_else(|| path.into());
    let url = format!("s4drive://versions/{}", name);
    let _ = std::process::Command::new("xdg-open").arg(&url).output();
    println!("{}", url);
}
