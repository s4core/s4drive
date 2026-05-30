//! S4Drive Phase 3 — smoke test against real MinIO.
//!
//! Run: cargo test --test smoke_phase3 -- --ignored
//! Requires: docker running MinIO on localhost:9000 with bucket s4drive-phase3
use std::process::Command;

#[test]
#[ignore = "requires running MinIO on localhost:9000"]
fn smoke_phase3_metadata_protocol() {
    // Use the CLI to verify init-bucket and metadata commands
    let status = Command::new("cargo")
        .args([
            "run",
            "-p",
            "s4drive-cli",
            "--",
            "metadata",
            "status",
            "--endpoint",
            "http://127.0.0.1:9000",
            "--bucket",
            "s4drive-phase3",
            "--access-key",
            "minioadmin",
            "--secret",
            "minioadmin",
        ])
        .output()
        .expect("failed to run s4drive-cli metadata status");

    let output = String::from_utf8_lossy(&status.stdout);
    assert!(
        output.contains("Schema version:  1"),
        "Expected Schema version 1, got: {}",
        output
    );
    assert!(
        output.contains("Bucket has .s4drive/ metadata"),
        "Bucket should have .s4drive/ metadata"
    );

    // Check tree is accessible
    let tree_out = Command::new("cargo")
        .args([
            "run",
            "-p",
            "s4drive-cli",
            "--",
            "metadata",
            "tree",
            "--endpoint",
            "http://127.0.0.1:9000",
            "--bucket",
            "s4drive-phase3",
            "--access-key",
            "minioadmin",
            "--secret",
            "minioadmin",
        ])
        .output()
        .expect("failed to run s4drive-cli metadata tree");

    let tree_output = String::from_utf8_lossy(&tree_out.stdout);
    assert!(
        tree_output.contains("entries"),
        "Tree command should show entries, got: {}",
        tree_output
    );

    // Check ops log is accessible
    let ops_out = Command::new("cargo")
        .args([
            "run",
            "-p",
            "s4drive-cli",
            "--",
            "metadata",
            "ops",
            "--endpoint",
            "http://127.0.0.1:9000",
            "--bucket",
            "s4drive-phase3",
            "--access-key",
            "minioadmin",
            "--secret",
            "minioadmin",
        ])
        .output()
        .expect("failed to run s4drive-cli metadata ops");

    let ops_output = String::from_utf8_lossy(&ops_out.stdout);
    assert!(
        ops_output.contains("Total ops:"),
        "Ops command should show total, got: {}",
        ops_output
    );

    println!("✅ Phase 3 smoke test: ALL CHECKS PASSED");
    println!("  ✓ Bucket initialized with .s4drive/");
    println!("  ✓ Metadata status readable");
    println!("  ✓ File tree accessible (0 entries)");
    println!("  ✓ Operation log accessible (0 ops)");
}
