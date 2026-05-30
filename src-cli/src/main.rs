/// S4Drive CLI — проверка совместимости S3 бакета.
///
/// Использование:
///   cargo run -p s4drive-cli -- check --endpoint https://minio.example.com --bucket my-bucket
use clap::{Parser, Subcommand};
use s4drive_core::config::Config;
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
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter("s4drive=debug")
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
    }
}

async fn run_check(
    endpoint: &str,
    bucket: &str,
    access_key: &str,
    secret_key: &str,
    region: &str,
) {
    println!();
    println!("╔══════════════════════════════════════════════╗");
    println!("║   S4Drive — S3 Compatibility Checker        ║");
    println!("╚══════════════════════════════════════════════╝");
    println!();
    println!("  Endpoint:  {}", endpoint);
    println!("  Bucket:    {}", bucket);
    println!("  Region:    {}", region);
    println!();

    // Build config
    let mut config = Config::default();
    config.s3.endpoint = endpoint.to_string();
    config.s3.bucket = bucket.to_string();
    config.s3.region = region.to_string();
    config.s3.access_key_id = access_key.to_string();
    config.s3.encrypted_secret_key = Some(secret_key.to_string());

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

    // Check bucket access
    println!("  Checking bucket access...");
    match adapter.check_bucket_access().await {
        Ok(true) => println!("  ✓ Bucket '{}' is accessible", bucket),
        Ok(false) => println!("  ✗ Bucket '{}' exists but returned unexpected status", bucket),
        Err(e) => {
            println!("  ✗ {}", e);
            return;
        }
    }

    println!();
    println!("  Running compatibility tests...");
    println!();

    // Run compatibility test suite
    let report = adapter.run_compatibility_test().await;

    println!("{}", report);

    // Summary
    println!("╔══════════════════════════════════════════════╗");
    if report.level >= 2 {
        println!("║   ✅ Level {}: {}                         ", report.level, level_badge(report.level));
    } else {
        println!("║   ❌ Level {}: {}                         ", report.level, level_badge(report.level));
    }
    println!("╚══════════════════════════════════════════════╝");
    println!();
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
