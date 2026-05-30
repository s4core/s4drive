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
    if report.level >= 2 {
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
