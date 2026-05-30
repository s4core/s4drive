/// S3 adapter: low-level S3 operations with conditional write support.
///
/// Wraps the aws-sdk-s3 client and provides S4Drive-specific operations:
/// - Conditional writes (If-Match, If-None-Match)
/// - Multipart upload with resume
/// - Compatibility detection
/// - Error classification

pub mod client;
pub mod compatibility;
pub mod error;
pub mod retry;

pub use client::*;
pub use compatibility::*;
pub use error::*;
pub use retry::*;
