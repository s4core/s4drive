use crate::config::Config;
use crate::error::{CoreError, CoreResult};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client as S3Client;

/// S3 adapter wrapping the aws-sdk-s3 client.
pub struct S3Adapter {
    client: S3Client,
    bucket: String,
    endpoint: String,
    connected: bool,
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

impl S3Adapter {
    /// Create a new S3 adapter from config.
    /// Connects to any S3-compatible endpoint.
    pub async fn new(config: &Config) -> CoreResult<Self> {
        let endpoint = config.s3.endpoint.clone();
        let bucket = config.s3.bucket.clone();
        let region = &config.s3.region;
        let access_key = &config.s3.access_key_id;
        let secret = config.s3.encrypted_secret_key.as_deref().unwrap_or("");

        let creds = aws_sdk_s3::config::Credentials::new(
            access_key, secret, None, None, "s4drive",
        );

        let s3_config = aws_sdk_s3::config::Builder::new()
            .endpoint_url(&endpoint)
            .region(aws_sdk_s3::config::Region::new(region.clone()))
            .credentials_provider(creds)
            .force_path_style(true)
            .behavior_version_latest()
            .build();

        let client = S3Client::from_conf(s3_config);

        Ok(Self {
            client,
            bucket,
            endpoint,
            connected: true,
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

    // ─── Basic Operations ───────────────────────────────────────────

    /// PUT an object — unconditional.
    pub async fn put_object(&self, key: &str, body: Vec<u8>) -> CoreResult<String> {
        let resp = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(body))
            .send()
            .await
            .map_err(|e| classify_s3_error(e, key))?;

        Ok(resp.e_tag().unwrap_or_default().trim_matches('"').to_string())
    }

    /// PUT with conditional If-None-Match.
    /// Returns Ok(true) if created, Ok(false) if already exists (412).
    pub async fn put_if_not_exists(&self, key: &str, body: Vec<u8>) -> CoreResult<bool> {
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
                if service_err.meta().code() == Some("PreconditionFailed")
                {
                    Ok(false)
                } else {
                    Err(CoreError::S3(format!("if-none-match failed ({}): {}",
                        service_err.meta().code().unwrap_or("?"),
                        service_err.meta().message().unwrap_or("?"),
                    )))
                }
            }
        }
    }

    /// PUT with conditional If-Match (CAS).
    /// Returns Ok(true) if updated, Ok(false) if precondition failed (412).
    pub async fn put_if_match(&self, key: &str, body: Vec<u8>, expected_etag: &str) -> CoreResult<bool> {
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
                if service_err.meta().code() == Some("PreconditionFailed")
                {
                    Ok(false)
                } else {
                    Err(CoreError::S3(format!("if-match failed ({}): {}",
                        service_err.meta().code().unwrap_or("?"),
                        service_err.meta().message().unwrap_or("?"),
                    )))
                }
            }
        }
    }

    /// GET object data by key.
    pub async fn get_object(&self, key: &str) -> CoreResult<Vec<u8>> {
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| classify_s3_error(e, key))?;

        let data = resp
            .body
            .collect()
            .await
            .map_err(|e| CoreError::S3(format!("failed to read body: {}", e)))?
            .into_bytes()
            .to_vec();

        Ok(data)
    }

    /// GET object with range (for resume/partial download).
    pub async fn get_object_range(&self, key: &str, range: &str) -> CoreResult<Vec<u8>> {
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .range(range)
            .send()
            .await
            .map_err(|e| classify_s3_error(e, key))?;

        let data = resp
            .body
            .collect()
            .await
            .map_err(|e| CoreError::S3(format!("failed to read body: {}", e)))?
            .into_bytes()
            .to_vec();

        Ok(data)
    }

    /// HEAD object — get metadata without downloading.
    pub async fn head_object(&self, key: &str) -> CoreResult<ObjectMeta> {
        let resp = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| classify_s3_error(e, key))?;

        let size = resp.content_length().unwrap_or(0).max(0) as u64;

        Ok(ObjectMeta {
            key: key.to_string(),
            size,
            etag: resp.e_tag().unwrap_or_default().trim_matches('"').to_string(),
            last_modified: resp
                .last_modified()
                .map(|d| d.to_string())
                .unwrap_or_default(),
            version_id: resp.version_id().map(|s| s.to_string()),
        })
    }

    /// DELETE an object.
    pub async fn delete_object(&self, key: &str) -> CoreResult<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| classify_s3_error(e, key))?;
        Ok(())
    }

    /// List objects with a given prefix.
    pub async fn list_objects(&self, prefix: &str) -> CoreResult<Vec<String>> {
        let mut keys = Vec::new();
        let mut continuation_token: Option<String> = None;

        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix)
                .max_keys(1000);

            if let Some(ref token) = continuation_token {
                req = req.continuation_token(token);
            }

            let resp = req
                .send()
                .await
                .map_err(|e| classify_s3_error(e, prefix))?;

            for obj in resp.contents().iter() {
                if let Some(key) = obj.key() {
                    keys.push(key.to_string());
                }
            }

            if resp.is_truncated() == Some(true) {
                continuation_token = resp
                    .next_continuation_token()
                    .map(|s| s.to_string());
            } else {
                break;
            }
        }

        Ok(keys)
    }

    /// Check if a bucket exists and is accessible.
    pub async fn check_bucket_access(&self) -> CoreResult<bool> {
        match self
            .client
            .head_bucket()
            .bucket(&self.bucket)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(err) => {
                let service_err = err.into_service_error();
                let code = service_err.meta().code().unwrap_or("unknown");
                Err(CoreError::S3(format!("bucket access denied: {}", code)))
            }
        }
    }

    // ─── Multipart Upload ───────────────────────────────────────────

    /// Upload a file using multipart upload with 5 MB parts.
    pub async fn multipart_upload(&self, key: &str, data: Vec<u8>) -> CoreResult<String> {
        let part_size: usize = 5 * 1024 * 1024;
        let total_parts = (data.len() + part_size - 1) / part_size;

        // Initiate
        let upload = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| classify_s3_error(e, key))?;

        let upload_id = upload.upload_id().unwrap_or_default().to_string();

        // Upload parts
        let mut completed_parts: Vec<aws_sdk_s3::types::CompletedPart> = Vec::with_capacity(total_parts);

        for i in 0..total_parts {
            let start = i * part_size;
            let end = std::cmp::min(start + part_size, data.len());
            let chunk = data[start..end].to_vec();
            let part_number = (i + 1) as i32;

            let part_resp = self
                .client
                .upload_part()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(&upload_id)
                .part_number(part_number)
                .body(ByteStream::from(chunk))
                .send()
                .await
                .map_err(|e| classify_s3_error(e, key))?;

            completed_parts.push(
                aws_sdk_s3::types::CompletedPart::builder()
                    .e_tag(part_resp.e_tag().unwrap_or_default())
                    .part_number(part_number)
                    .build(),
            );
        }

        // Complete
        let completed = aws_sdk_s3::types::CompletedMultipartUpload::builder()
            .set_parts(Some(completed_parts))
            .build();

        let result = self
            .client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(&upload_id)
            .multipart_upload(completed)
            .send()
            .await
            .map_err(|e| classify_s3_error(e, key))?;

        Ok(result.e_tag().unwrap_or_default().trim_matches('"').to_string())
    }

    /// Abort a multipart upload (cleanup).
    pub async fn abort_multipart_upload(&self, key: &str, upload_id: &str) -> CoreResult<()> {
        self.client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .send()
            .await
            .map_err(|e| classify_s3_error(e, key))?;
        Ok(())
    }

    /// Upload a metadata object (JSON) as a small blob.
    pub async fn put_metadata(&self, key: &str, json_body: &str) -> CoreResult<String> {
        let etag = self.put_object(key, json_body.as_bytes().to_vec()).await?;
        Ok(etag)
    }
}

/// Classify S3 errors into CoreError variants.
pub(crate) fn classify_s3_error(
    e: impl std::fmt::Display,
    context: &str,
) -> CoreError {
    let msg = format!("s3 error ({}): {}", context, e);
    let msg_lower = msg.to_lowercase();

    if msg_lower.contains("412") || msg_lower.contains("precondition") {
        CoreError::S3(format!("412 PreconditionFailed: {}", context))
    } else if msg_lower.contains("404") || msg_lower.contains("not found") {
        CoreError::NotFound(format!("not found: {}", context))
    } else if msg_lower.contains("403") || msg_lower.contains("forbidden") || msg_lower.contains("accessdenied") {
        CoreError::Auth(format!("access denied: {}", context))
    } else if msg_lower.contains("409") || msg_lower.contains("conflict") {
        CoreError::Conflict(format!("conflict: {}", context))
    } else {
        CoreError::S3(msg)
    }
}
