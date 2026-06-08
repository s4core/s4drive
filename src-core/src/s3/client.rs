use crate::config::Config;
use crate::error::{CoreError, CoreResult};
use crate::s3::retry::RetryPolicy;
use aws_sdk_s3::primitives::{ByteStream, Length};
use aws_sdk_s3::Client as S3Client;
use base64::{engine::general_purpose, Engine as _};
use md5::{Digest, Md5};
use std::future::Future;
use std::path::Path;
use tokio::io::AsyncWriteExt;

const MULTIPART_PART_SIZE: usize = 5 * 1024 * 1024;
const MAX_MULTIPART_PARTS: u64 = 10_000;
const FILE_STREAM_BUFFER_SIZE: usize = 64 * 1024;

/// Async object-store abstraction used by S4Drive's sync and metadata layers.
#[allow(async_fn_in_trait)]
pub trait S3ObjectStore {
    fn bucket(&self) -> &str;
    fn endpoint(&self) -> &str;

    async fn put_object(&self, key: &str, body: Vec<u8>) -> CoreResult<String>;
    async fn put_if_not_exists(&self, key: &str, body: Vec<u8>) -> CoreResult<bool>;
    async fn put_if_match(&self, key: &str, body: Vec<u8>, expected_etag: &str)
        -> CoreResult<bool>;
    async fn get_object(&self, key: &str) -> CoreResult<Vec<u8>>;
    async fn get_object_range(&self, key: &str, range: &str) -> CoreResult<Vec<u8>>;
    async fn head_object(&self, key: &str) -> CoreResult<ObjectMeta>;
    async fn delete_object(&self, key: &str) -> CoreResult<()>;
    async fn list_objects(&self, prefix: &str) -> CoreResult<Vec<String>>;
}

/// S3 adapter wrapping the aws-sdk-s3 client.
#[derive(Clone)]
pub struct S3Adapter {
    client: S3Client,
    bucket: String,
    endpoint: String,
    connected: bool,
    retry_policy: RetryPolicy,
}

/// Metadata about an S3 object.
#[derive(Debug, Clone)]
pub struct ObjectMeta {
    pub key: String,
    pub size: u64,
    pub etag: String,
    pub last_modified: String,
    pub version_id: Option<String>,
}

/// One page from ListObjectsV2.
#[derive(Debug, Clone)]
pub struct ListObjectsPage {
    pub keys: Vec<String>,
    pub common_prefixes: Vec<String>,
    pub next_continuation_token: Option<String>,
    pub is_truncated: bool,
}

/// Active multipart upload handle.
#[derive(Debug, Clone)]
pub struct MultipartUpload {
    pub key: String,
    pub upload_id: String,
}

impl S3Adapter {
    /// Create a new S3 adapter from config.
    /// Connects to any S3-compatible endpoint.
    pub async fn new(config: &Config) -> CoreResult<Self> {
        let endpoint = config.s3.endpoint.clone();
        let bucket = config.s3.bucket.clone();
        let region = &config.s3.region;
        let access_key = &config.s3.access_key_id;
        let secret = config.s3.secret_key_fallback.as_deref().unwrap_or("");

        let creds = aws_sdk_s3::config::Credentials::new(access_key, secret, None, None, "s4drive");

        let s3_config = aws_sdk_s3::config::Builder::new()
            .endpoint_url(&endpoint)
            .region(aws_sdk_s3::config::Region::new(region.clone()))
            .credentials_provider(creds)
            .force_path_style(true)
            .behavior_version_latest()
            .build();

        let client = S3Client::from_conf(s3_config);
        let retry_policy = RetryPolicy {
            max_attempts: config.core.max_retries,
            ..Default::default()
        };

        Ok(Self {
            client,
            bucket,
            endpoint,
            connected: true,
            retry_policy,
        })
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    async fn with_retry<T, Fut, F>(&self, mut operation: F) -> CoreResult<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = CoreResult<T>>,
    {
        let mut attempt = 0;
        loop {
            match operation().await {
                Ok(value) => return Ok(value),
                Err(err) if self.retry_policy.should_retry(&err, attempt) => {
                    let delay = self.retry_policy.delay_for_attempt(attempt);
                    tracing::warn!(
                        "retrying S3 operation after attempt {} failed: {}",
                        attempt + 1,
                        err
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }

    // ─── Basic Operations ───────────────────────────────────────────

    /// PUT an object — unconditional.
    pub async fn put_object(&self, key: &str, body: Vec<u8>) -> CoreResult<String> {
        self.with_retry(|| {
            let body = body.clone();
            async move {
                let resp = self
                    .client
                    .put_object()
                    .bucket(&self.bucket)
                    .key(key)
                    .body(ByteStream::from(body))
                    .send()
                    .await
                    .map_err(|err| {
                        let service_err = err.into_service_error();
                        let code = service_err.meta().code().map(ToString::to_string);
                        classify_s3_service_error(code.as_deref(), service_err, key)
                    })?;

                Ok(normalize_etag(resp.e_tag()))
            }
        })
        .await
    }

    /// PUT an object with Content-MD5 so the S3 backend validates upload integrity.
    pub async fn put_object_with_checksum(&self, key: &str, body: Vec<u8>) -> CoreResult<String> {
        let content_md5 = content_md5_base64(&body);
        self.with_retry(|| {
            let body = body.clone();
            let content_md5 = content_md5.clone();
            async move {
                let resp = self
                    .client
                    .put_object()
                    .bucket(&self.bucket)
                    .key(key)
                    .content_md5(content_md5)
                    .body(ByteStream::from(body))
                    .send()
                    .await
                    .map_err(|err| {
                        let service_err = err.into_service_error();
                        let code = service_err.meta().code().map(ToString::to_string);
                        classify_s3_service_error(code.as_deref(), service_err, key)
                    })?;

                Ok(normalize_etag(resp.e_tag()))
            }
        })
        .await
    }

    /// PUT with conditional If-None-Match.
    /// Returns Ok(true) if created, Ok(false) if already exists (412).
    pub async fn put_if_not_exists(&self, key: &str, body: Vec<u8>) -> CoreResult<bool> {
        self.with_retry(|| {
            let body = body.clone();
            async move {
                let result = self
                    .client
                    .put_object()
                    .bucket(&self.bucket)
                    .key(key)
                    .body(ByteStream::from(body))
                    .if_none_match("*")
                    .send()
                    .await;

                match result {
                    Ok(_) => Ok(true),
                    Err(err) => {
                        let service_err = err.into_service_error();
                        let code = service_err.meta().code().map(ToString::to_string);
                        if is_precondition_failed_error(code.as_deref(), &service_err) {
                            Ok(false)
                        } else {
                            Err(classify_s3_service_error(code.as_deref(), service_err, key))
                        }
                    }
                }
            }
        })
        .await
    }

    /// PUT a local file via a retryable filesystem-backed stream with If-None-Match.
    pub async fn put_file_if_not_exists(
        &self,
        key: &str,
        path: &Path,
        size: u64,
    ) -> CoreResult<bool> {
        let path = path.to_path_buf();
        self.with_retry(|| {
            let path = path.clone();
            async move {
                let body = file_byte_stream(&path, 0, size).await?;
                let result = self
                    .client
                    .put_object()
                    .bucket(&self.bucket)
                    .key(key)
                    .body(body)
                    .if_none_match("*")
                    .send()
                    .await;

                match result {
                    Ok(_) => Ok(true),
                    Err(err) => {
                        let service_err = err.into_service_error();
                        let code = service_err.meta().code().map(ToString::to_string);
                        if is_precondition_failed_error(code.as_deref(), &service_err) {
                            Ok(false)
                        } else {
                            Err(classify_s3_service_error(code.as_deref(), service_err, key))
                        }
                    }
                }
            }
        })
        .await
    }

    /// PUT with conditional If-Match (CAS).
    /// Returns Ok(true) if updated, Ok(false) if precondition failed (412).
    pub async fn put_if_match(
        &self,
        key: &str,
        body: Vec<u8>,
        expected_etag: &str,
    ) -> CoreResult<bool> {
        let expected_etag = quote_etag_for_condition(expected_etag);
        self.with_retry(|| {
            let body = body.clone();
            let expected_etag = expected_etag.clone();
            async move {
                let result = self
                    .client
                    .put_object()
                    .bucket(&self.bucket)
                    .key(key)
                    .body(ByteStream::from(body))
                    .if_match(expected_etag)
                    .send()
                    .await;

                match result {
                    Ok(_) => Ok(true),
                    Err(err) => {
                        let service_err = err.into_service_error();
                        let code = service_err.meta().code().map(ToString::to_string);
                        if is_precondition_failed_error(code.as_deref(), &service_err) {
                            Ok(false)
                        } else {
                            Err(classify_s3_service_error(code.as_deref(), service_err, key))
                        }
                    }
                }
            }
        })
        .await
    }

    /// GET object data by key.
    pub async fn get_object(&self, key: &str) -> CoreResult<Vec<u8>> {
        self.with_retry(|| async move {
            let resp = self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
                .map_err(|err| {
                    let service_err = err.into_service_error();
                    let code = service_err.meta().code().map(ToString::to_string);
                    classify_s3_service_error(code.as_deref(), service_err, key)
                })?;

            let data = resp
                .body
                .collect()
                .await
                .map_err(|e| CoreError::S3(format!("failed to read body: {}", e)))?
                .into_bytes()
                .to_vec();

            Ok(data)
        })
        .await
    }

    /// GET object with range (for resume/partial download).
    pub async fn get_object_range(&self, key: &str, range: &str) -> CoreResult<Vec<u8>> {
        self.with_retry(|| async move {
            let resp = self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(key)
                .range(range)
                .send()
                .await
                .map_err(|err| {
                    let service_err = err.into_service_error();
                    let code = service_err.meta().code().map(ToString::to_string);
                    classify_s3_service_error(code.as_deref(), service_err, key)
                })?;

            let data = resp
                .body
                .collect()
                .await
                .map_err(|e| CoreError::S3(format!("failed to read body: {}", e)))?
                .into_bytes()
                .to_vec();

            Ok(data)
        })
        .await
    }

    /// Stream an object directly to a local path.
    pub async fn download_object_to_file(&self, key: &str, path: &Path) -> CoreResult<u64> {
        let path = path.to_path_buf();
        self.with_retry(|| {
            let path = path.clone();
            async move {
                let resp = self
                    .client
                    .get_object()
                    .bucket(&self.bucket)
                    .key(key)
                    .send()
                    .await
                    .map_err(|err| {
                        let service_err = err.into_service_error();
                        let code = service_err.meta().code().map(ToString::to_string);
                        classify_s3_service_error(code.as_deref(), service_err, key)
                    })?;

                let mut reader = resp.body.into_async_read();
                let mut file = tokio::fs::File::create(&path).await.map_err(|e| {
                    CoreError::FileSystem(format!("create {}: {}", path.display(), e))
                })?;
                let bytes = tokio::io::copy(&mut reader, &mut file).await.map_err(|e| {
                    CoreError::FileSystem(format!("write {}: {}", path.display(), e))
                })?;
                file.flush().await.map_err(|e| {
                    CoreError::FileSystem(format!("flush {}: {}", path.display(), e))
                })?;

                Ok(bytes)
            }
        })
        .await
    }

    /// HEAD object — get metadata without downloading.
    pub async fn head_object(&self, key: &str) -> CoreResult<ObjectMeta> {
        self.with_retry(|| async move {
            let resp = self
                .client
                .head_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
                .map_err(|err| {
                    let service_err = err.into_service_error();
                    let code = service_err.meta().code().map(ToString::to_string);
                    classify_s3_service_error(code.as_deref(), service_err, key)
                })?;

            let size = resp.content_length().unwrap_or(0).max(0) as u64;

            Ok(ObjectMeta {
                key: key.to_string(),
                size,
                etag: normalize_etag(resp.e_tag()),
                last_modified: resp
                    .last_modified()
                    .map(|d| d.to_string())
                    .unwrap_or_default(),
                version_id: resp.version_id().map(|s| s.to_string()),
            })
        })
        .await
    }

    /// DELETE an object.
    pub async fn delete_object(&self, key: &str) -> CoreResult<()> {
        self.with_retry(|| async move {
            self.client
                .delete_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
                .map_err(|err| {
                    let service_err = err.into_service_error();
                    let code = service_err.meta().code().map(ToString::to_string);
                    classify_s3_service_error(code.as_deref(), service_err, key)
                })?;
            Ok(())
        })
        .await
    }

    /// List objects with a given prefix.
    pub async fn list_objects(&self, prefix: &str) -> CoreResult<Vec<String>> {
        let mut keys = Vec::new();
        let mut continuation_token: Option<String> = None;

        loop {
            let page = self
                .list_objects_page(prefix, None, 1000, continuation_token.as_deref())
                .await?;
            keys.extend(page.keys);

            if page.is_truncated {
                continuation_token = page.next_continuation_token;
                if continuation_token.is_none() {
                    return Err(CoreError::S3(format!(
                        "LIST for '{}' was truncated without ContinuationToken",
                        prefix
                    )));
                }
            } else {
                break;
            }
        }

        Ok(keys)
    }

    /// List one page with optional delimiter, used by compatibility detection.
    pub async fn list_objects_page(
        &self,
        prefix: &str,
        delimiter: Option<&str>,
        max_keys: i32,
        continuation_token: Option<&str>,
    ) -> CoreResult<ListObjectsPage> {
        let max_keys = max_keys.clamp(1, 1000);
        self.with_retry(|| async move {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix)
                .max_keys(max_keys);

            if let Some(delimiter) = delimiter {
                req = req.delimiter(delimiter);
            }

            if let Some(token) = continuation_token {
                req = req.continuation_token(token);
            }

            let resp = req.send().await.map_err(|err| {
                let service_err = err.into_service_error();
                let code = service_err.meta().code().map(ToString::to_string);
                classify_s3_service_error(code.as_deref(), service_err, prefix)
            })?;
            let keys = resp
                .contents()
                .iter()
                .filter_map(|obj| obj.key().map(ToString::to_string))
                .collect();
            let common_prefixes = resp
                .common_prefixes()
                .iter()
                .filter_map(|prefix| prefix.prefix().map(ToString::to_string))
                .collect();

            Ok(ListObjectsPage {
                keys,
                common_prefixes,
                next_continuation_token: resp.next_continuation_token().map(ToString::to_string),
                is_truncated: resp.is_truncated().unwrap_or(false),
            })
        })
        .await
    }

    /// Check if a bucket exists and is accessible.
    /// Returns Ok(()) if accessible, Err with details if not.
    pub async fn check_bucket_access(&self) -> CoreResult<()> {
        self.with_retry(|| async move {
            self.client
                .head_bucket()
                .bucket(&self.bucket)
                .send()
                .await
                .map(|_| ())
                .map_err(|err| {
                    let service_err = err.into_service_error();
                    let code = service_err.meta().code().unwrap_or("unknown");
                    CoreError::S3(format!("bucket access denied: {}", code))
                })
        })
        .await
    }

    /// Lightweight runtime guard for S4Drive's minimum safe-sync requirements.
    ///
    /// The full compatibility report is intentionally left to the CLI because it
    /// includes heavier checks such as large multipart uploads.
    pub async fn verify_level2_prerequisites(&self) -> CoreResult<()> {
        let prefix = format!(".s4drive-compat-min-{}/", uuid::Uuid::now_v7());
        let key = format!("{}cas", prefix);
        let result = async {
            self.put_object(&key, b"base".to_vec()).await?;
            let meta = self.head_object(&key).await?;
            if !self
                .put_if_match(&key, b"updated".to_vec(), &meta.etag)
                .await?
            {
                return Err(CoreError::Config(
                    "S3 bucket is below S4Drive Level 2: If-Match rejected a valid ETag".into(),
                ));
            }
            if self
                .put_if_match(&key, b"stale".to_vec(), &meta.etag)
                .await?
            {
                return Err(CoreError::Config(
                    "S3 bucket is below S4Drive Level 2: stale If-Match write was accepted".into(),
                ));
            }

            let create_key = format!("{}create-once", prefix);
            if !self
                .put_if_not_exists(&create_key, b"first".to_vec())
                .await?
            {
                return Err(CoreError::Config(
                    "S3 bucket is below S4Drive Level 2: If-None-Match rejected a new object"
                        .into(),
                ));
            }
            if self
                .put_if_not_exists(&create_key, b"second".to_vec())
                .await?
            {
                return Err(CoreError::Config(
                    "S3 bucket is below S4Drive Level 2: If-None-Match overwrote an object".into(),
                ));
            }

            if self.get_object(&key).await? != b"updated" {
                return Err(CoreError::Config(
                    "S3 bucket is below S4Drive Level 2: read-after-write is inconsistent".into(),
                ));
            }
            self.delete_object(&key).await?;
            if self.head_object(&key).await.is_ok() {
                return Err(CoreError::Config(
                    "S3 bucket is below S4Drive Level 2: read-after-delete is inconsistent".into(),
                ));
            }

            Ok(())
        }
        .await;

        self.cleanup_prefix_best_effort(&prefix).await;
        result.map_err(|err| match err {
            CoreError::Config(_) => err,
            other => CoreError::Config(format!(
                "S3 bucket is below S4Drive Level 2 or cannot be verified: {}",
                other
            )),
        })
    }

    async fn cleanup_prefix_best_effort(&self, prefix: &str) {
        if let Ok(keys) = self.list_objects(prefix).await {
            for key in keys {
                let _ = self.delete_object(&key).await;
            }
        }
    }

    // ─── Multipart Upload ───────────────────────────────────────────

    /// Start a multipart upload and return its upload ID.
    pub async fn create_multipart_upload(&self, key: &str) -> CoreResult<MultipartUpload> {
        self.with_retry(|| async move {
            let upload = self
                .client
                .create_multipart_upload()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
                .map_err(|err| {
                    let service_err = err.into_service_error();
                    let code = service_err.meta().code().map(ToString::to_string);
                    classify_s3_service_error(code.as_deref(), service_err, key)
                })?;

            let upload_id = upload.upload_id().ok_or_else(|| {
                CoreError::S3(format!(
                    "create multipart upload for '{}' returned no upload id",
                    key
                ))
            })?;

            Ok(MultipartUpload {
                key: key.to_string(),
                upload_id: upload_id.to_string(),
            })
        })
        .await
    }

    /// Upload one multipart part and return its ETag.
    pub async fn upload_multipart_part(
        &self,
        key: &str,
        upload_id: &str,
        part_number: i32,
        data: Vec<u8>,
    ) -> CoreResult<String> {
        self.with_retry(|| {
            let data = data.clone();
            async move {
                let part_resp = self
                    .client
                    .upload_part()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .part_number(part_number)
                    .body(ByteStream::from(data))
                    .send()
                    .await
                    .map_err(|err| {
                        let service_err = err.into_service_error();
                        let code = service_err.meta().code().map(ToString::to_string);
                        classify_s3_service_error(code.as_deref(), service_err, key)
                    })?;

                let etag = part_resp.e_tag().ok_or_else(|| {
                    CoreError::S3(format!(
                        "upload part {} for '{}' returned no ETag",
                        part_number, key
                    ))
                })?;
                Ok(normalize_etag(Some(etag)))
            }
        })
        .await
    }

    /// Upload one multipart part from a local file range.
    pub async fn upload_multipart_file_part(
        &self,
        key: &str,
        upload_id: &str,
        part_number: i32,
        path: &Path,
        offset: u64,
        size: u64,
    ) -> CoreResult<String> {
        if size == 0 {
            return Err(CoreError::S3(format!(
                "multipart part {} for '{}' is empty",
                part_number, key
            )));
        }

        let path = path.to_path_buf();
        self.with_retry(|| {
            let path = path.clone();
            async move {
                let body = file_byte_stream(&path, offset, size).await?;
                let part_resp = self
                    .client
                    .upload_part()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .part_number(part_number)
                    .body(body)
                    .send()
                    .await
                    .map_err(|err| {
                        let service_err = err.into_service_error();
                        let code = service_err.meta().code().map(ToString::to_string);
                        classify_s3_service_error(code.as_deref(), service_err, key)
                    })?;

                let etag = part_resp.e_tag().ok_or_else(|| {
                    CoreError::S3(format!(
                        "upload file part {} for '{}' returned no ETag",
                        part_number, key
                    ))
                })?;
                Ok(normalize_etag(Some(etag)))
            }
        })
        .await
    }

    /// Upload a file using multipart upload with 5 MiB parts.
    pub async fn multipart_upload(&self, key: &str, data: Vec<u8>) -> CoreResult<String> {
        let upload = self.create_multipart_upload(key).await?;
        let result = self
            .complete_multipart_upload_from_bytes(key, &upload.upload_id, data)
            .await;

        if result.is_err() {
            let _ = self.abort_multipart_upload(key, &upload.upload_id).await;
        }

        result
    }

    /// Upload a local file using multipart upload with no full-file allocation.
    pub async fn multipart_upload_file_if_not_exists(
        &self,
        key: &str,
        path: &Path,
        size: u64,
    ) -> CoreResult<bool> {
        if size == 0 {
            return self.put_file_if_not_exists(key, path, size).await;
        }

        let upload = self.create_multipart_upload(key).await?;
        let result = self
            .complete_multipart_upload_from_file_if_not_exists(key, &upload.upload_id, path, size)
            .await;

        match result {
            Ok(true) => Ok(true),
            Ok(false) => {
                let _ = self.abort_multipart_upload(key, &upload.upload_id).await;
                Ok(false)
            }
            Err(err) => {
                let _ = self.abort_multipart_upload(key, &upload.upload_id).await;
                Err(err)
            }
        }
    }

    async fn complete_multipart_upload_from_bytes(
        &self,
        key: &str,
        upload_id: &str,
        data: Vec<u8>,
    ) -> CoreResult<String> {
        if data.is_empty() {
            return Err(CoreError::S3(format!(
                "multipart upload for '{}' requires at least one byte",
                key
            )));
        }

        let total_parts = multipart_part_count(data.len() as u64)? as usize;
        let mut completed_parts: Vec<aws_sdk_s3::types::CompletedPart> =
            Vec::with_capacity(total_parts);

        for i in 0..total_parts {
            let start = i * MULTIPART_PART_SIZE;
            let end = std::cmp::min(start + MULTIPART_PART_SIZE, data.len());
            let chunk = data[start..end].to_vec();
            let part_number = (i + 1) as i32;
            let etag = self
                .upload_multipart_part(key, upload_id, part_number, chunk)
                .await?;

            completed_parts.push(
                aws_sdk_s3::types::CompletedPart::builder()
                    .e_tag(quote_etag_for_condition(&etag))
                    .part_number(part_number)
                    .build(),
            );
        }

        let completed = aws_sdk_s3::types::CompletedMultipartUpload::builder()
            .set_parts(Some(completed_parts))
            .build();

        self.with_retry(|| {
            let completed = completed.clone();
            async move {
                let result = self
                    .client
                    .complete_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .multipart_upload(completed)
                    .send()
                    .await
                    .map_err(|err| {
                        let service_err = err.into_service_error();
                        let code = service_err.meta().code().map(ToString::to_string);
                        classify_s3_service_error(code.as_deref(), service_err, key)
                    })?;

                Ok(normalize_etag(result.e_tag()))
            }
        })
        .await
    }

    async fn complete_multipart_upload_from_file_if_not_exists(
        &self,
        key: &str,
        upload_id: &str,
        path: &Path,
        size: u64,
    ) -> CoreResult<bool> {
        let total_parts = multipart_part_count(size)?;
        let mut completed_parts: Vec<aws_sdk_s3::types::CompletedPart> =
            Vec::with_capacity(total_parts as usize);

        for index in 0..total_parts {
            let offset = u64::from(index) * MULTIPART_PART_SIZE as u64;
            let part_size = (size - offset).min(MULTIPART_PART_SIZE as u64);
            let part_number = (index + 1) as i32;
            let etag = self
                .upload_multipart_file_part(key, upload_id, part_number, path, offset, part_size)
                .await?;

            completed_parts.push(
                aws_sdk_s3::types::CompletedPart::builder()
                    .e_tag(quote_etag_for_condition(&etag))
                    .part_number(part_number)
                    .build(),
            );
        }

        let completed = aws_sdk_s3::types::CompletedMultipartUpload::builder()
            .set_parts(Some(completed_parts))
            .build();

        self.with_retry(|| {
            let completed = completed.clone();
            async move {
                let result = self
                    .client
                    .complete_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .multipart_upload(completed)
                    .if_none_match("*")
                    .send()
                    .await;

                match result {
                    Ok(_) => Ok(true),
                    Err(err) => {
                        let service_err = err.into_service_error();
                        let code = service_err.meta().code().map(ToString::to_string);
                        if is_precondition_failed_error(code.as_deref(), &service_err) {
                            Ok(false)
                        } else {
                            Err(classify_s3_service_error(code.as_deref(), service_err, key))
                        }
                    }
                }
            }
        })
        .await
    }

    /// Abort a multipart upload (cleanup).
    pub async fn abort_multipart_upload(&self, key: &str, upload_id: &str) -> CoreResult<()> {
        self.with_retry(|| async move {
            self.client
                .abort_multipart_upload()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(upload_id)
                .send()
                .await
                .map_err(|err| {
                    let service_err = err.into_service_error();
                    let code = service_err.meta().code().map(ToString::to_string);
                    classify_s3_service_error(code.as_deref(), service_err, key)
                })?;
            Ok(())
        })
        .await
    }

    /// Upload a metadata object (JSON) as a small blob.
    pub async fn put_metadata(&self, key: &str, json_body: &str) -> CoreResult<String> {
        let etag = self.put_object(key, json_body.as_bytes().to_vec()).await?;
        Ok(etag)
    }
}

impl S3ObjectStore for S3Adapter {
    fn bucket(&self) -> &str {
        self.bucket()
    }

    fn endpoint(&self) -> &str {
        self.endpoint()
    }

    async fn put_object(&self, key: &str, body: Vec<u8>) -> CoreResult<String> {
        S3Adapter::put_object(self, key, body).await
    }

    async fn put_if_not_exists(&self, key: &str, body: Vec<u8>) -> CoreResult<bool> {
        S3Adapter::put_if_not_exists(self, key, body).await
    }

    async fn put_if_match(
        &self,
        key: &str,
        body: Vec<u8>,
        expected_etag: &str,
    ) -> CoreResult<bool> {
        S3Adapter::put_if_match(self, key, body, expected_etag).await
    }

    async fn get_object(&self, key: &str) -> CoreResult<Vec<u8>> {
        S3Adapter::get_object(self, key).await
    }

    async fn get_object_range(&self, key: &str, range: &str) -> CoreResult<Vec<u8>> {
        S3Adapter::get_object_range(self, key, range).await
    }

    async fn head_object(&self, key: &str) -> CoreResult<ObjectMeta> {
        S3Adapter::head_object(self, key).await
    }

    async fn delete_object(&self, key: &str) -> CoreResult<()> {
        S3Adapter::delete_object(self, key).await
    }

    async fn list_objects(&self, prefix: &str) -> CoreResult<Vec<String>> {
        S3Adapter::list_objects(self, prefix).await
    }
}

async fn file_byte_stream(path: &Path, offset: u64, size: u64) -> CoreResult<ByteStream> {
    ByteStream::read_from()
        .path(path)
        .offset(offset)
        .length(Length::Exact(size))
        .buffer_size(FILE_STREAM_BUFFER_SIZE)
        .build()
        .await
        .map_err(|e| CoreError::FileSystem(format!("stream {}: {}", path.display(), e)))
}

pub(crate) fn multipart_part_count(size: u64) -> CoreResult<u32> {
    if size == 0 {
        return Err(CoreError::S3(
            "multipart upload requires at least one byte".into(),
        ));
    }

    let parts = size.div_ceil(MULTIPART_PART_SIZE as u64);
    if parts > MAX_MULTIPART_PARTS {
        return Err(CoreError::S3(format!(
            "multipart upload requires {} parts, above the S3 limit of {}",
            parts, MAX_MULTIPART_PARTS
        )));
    }

    Ok(parts as u32)
}

fn normalize_etag(etag: Option<&str>) -> String {
    etag.unwrap_or_default().trim_matches('"').to_string()
}

fn quote_etag_for_condition(etag: &str) -> String {
    let trimmed = etag.trim();
    if trimmed.starts_with('"') && trimmed.ends_with('"') {
        trimmed.to_string()
    } else {
        format!("\"{}\"", trimmed.trim_matches('"'))
    }
}

fn content_md5_base64(body: &[u8]) -> String {
    let digest = Md5::digest(body);
    general_purpose::STANDARD.encode(digest)
}

fn is_precondition_failed_error(code: Option<&str>, e: impl std::fmt::Display) -> bool {
    if code
        .map(|code| code.eq_ignore_ascii_case("PreconditionFailed"))
        .unwrap_or(false)
    {
        return true;
    }

    let raw = e.to_string().to_ascii_lowercase();
    raw.contains("preconditionfailed") || raw.contains("precondition failed") || raw.contains("412")
}

/// Classify S3 errors into CoreError variants.
pub(crate) fn classify_s3_error(e: impl std::fmt::Display, context: &str) -> CoreError {
    let raw = e.to_string();
    let msg = format!("s3 error ({}): {}", context, raw);
    let msg_lower = msg.to_ascii_lowercase();

    if msg_lower.contains("412") || msg_lower.contains("preconditionfailed") {
        CoreError::Conflict(format!("412 PreconditionFailed: {}", context))
    } else if msg_lower.contains("404")
        || msg_lower.contains("not found")
        || msg_lower.contains("nosuchkey")
        || msg_lower.contains("notfound")
    {
        CoreError::NotFound(format!("not found: {}", context))
    } else if msg_lower.contains("403")
        || msg_lower.contains("forbidden")
        || msg_lower.contains("accessdenied")
    {
        CoreError::Auth(format!("access denied: {}", context))
    } else if msg_lower.contains("409") || msg_lower.contains("conflict") {
        CoreError::Conflict(format!("conflict: {}", context))
    } else if msg_lower.contains("timeout")
        || msg_lower.contains("dispatchfailure")
        || msg_lower.contains("connection reset")
    {
        CoreError::Network(msg)
    } else {
        CoreError::S3(msg)
    }
}

fn classify_s3_service_error(
    code: Option<&str>,
    e: impl std::fmt::Display,
    context: &str,
) -> CoreError {
    match code.filter(|code| !code.is_empty()) {
        Some(code) => classify_s3_error(format!("{}: {}", code, e), context),
        None => classify_s3_error(e, context),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etags_are_quoted_for_if_match_headers() {
        assert_eq!(quote_etag_for_condition("abc"), "\"abc\"");
        assert_eq!(quote_etag_for_condition("\"abc\""), "\"abc\"");
    }

    #[test]
    fn content_md5_is_standard_base64() {
        assert_eq!(content_md5_base64(b"hello"), "XUFAKrxLKna5cZ2REBfFkg==");
    }

    #[test]
    fn precondition_failed_detection_handles_code_and_text() {
        assert!(is_precondition_failed_error(
            Some("PreconditionFailed"),
            "service error"
        ));
        assert!(is_precondition_failed_error(
            None,
            "service error: 412 Precondition Failed"
        ));
        assert!(is_precondition_failed_error(
            None,
            "service error: PreconditionFailed"
        ));
        assert!(!is_precondition_failed_error(
            Some("NoSuchKey"),
            "service error: 404"
        ));
    }

    #[test]
    fn multipart_part_count_enforces_s3_boundaries() {
        assert!(multipart_part_count(0).is_err());
        assert_eq!(
            multipart_part_count(MULTIPART_PART_SIZE as u64).expect("one full part"),
            1
        );
        assert_eq!(
            multipart_part_count(MULTIPART_PART_SIZE as u64 + 1).expect("two parts"),
            2
        );

        let too_large = (MAX_MULTIPART_PARTS * MULTIPART_PART_SIZE as u64) + 1;
        assert!(multipart_part_count(too_large).is_err());
    }

    #[test]
    fn string_error_classifier_maps_required_statuses() {
        assert!(matches!(
            classify_s3_error("service error: 404 NoSuchKey", "k"),
            CoreError::NotFound(_)
        ));
        assert!(matches!(
            classify_s3_service_error(Some("NoSuchKey"), "service error", "k"),
            CoreError::NotFound(_)
        ));
        assert!(matches!(
            classify_s3_error("service error: 403 AccessDenied", "k"),
            CoreError::Auth(_)
        ));
        assert!(matches!(
            classify_s3_error("service error: 409 Conflict", "k"),
            CoreError::Conflict(_)
        ));
        assert!(matches!(
            classify_s3_error("service error: 412 PreconditionFailed", "k"),
            CoreError::Conflict(_)
        ));
        assert!(matches!(
            classify_s3_error("dispatch failure: timeout", "k"),
            CoreError::Network(_)
        ));
    }
}
