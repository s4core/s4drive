//! S4Drive Core — Rust sync engine for S3-backed cloud storage.
//!
//! This crate implements the core business logic of S4Drive:
//! - S3 adapter with conditional writes
//! - Local SQLite metadata index
//! - File watcher for local changes
//! - Metadata protocol (file_id, revision graph, operation log)
//! - Sync engine (two-way sync with CAS commits)
//! - Conflict resolution (sibling revisions, conflict copies)
//! - Transfer queue (persistent upload/download)
//! - Diagnostics and health monitoring

pub mod config;
pub mod core;
pub mod db;
pub mod diagnostics;
pub mod error;
pub mod metadata;
pub mod s3;
pub mod sync;
pub mod transfer;
pub mod watcher;

// Re-export key types
pub use core::S4DriveCore;
pub use error::CoreError;
pub use metadata::types::*;
