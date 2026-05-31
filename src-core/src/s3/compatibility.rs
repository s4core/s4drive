use crate::s3::client::S3Adapter;
use crate::s3::error::classify_error_code;
use crate::s3::retry::RetryPolicy;

const TEST_PUT_GET: &str = "PUT/GET object";
const TEST_HEAD: &str = "HEAD object";
const TEST_DELETE: &str = "DELETE object";
const TEST_LIST_PREFIX_DELIMITER: &str = "LIST prefix/delimiter";
const TEST_LIST_PAGINATION: &str = "LIST pagination";
const TEST_RANGE_GET: &str = "Range GET";
const TEST_MULTIPART_UPLOAD: &str = "Multipart upload";
const TEST_MULTIPART_ABORT: &str = "Multipart abort";
const TEST_CHECKSUM_UPLOAD: &str = "Checksum upload";
const TEST_IF_NONE_MATCH: &str = "If-None-Match";
const TEST_IF_MATCH: &str = "If-Match";
const TEST_CONCURRENT_WRITES: &str = "Concurrent writes";
const TEST_CONSISTENCY_PUT: &str = "Consistency after PUT";
const TEST_CONSISTENCY_DELETE: &str = "Consistency after DELETE";
const TEST_KEY_EDGE_CASES: &str = "Unicode, long paths, case-sensitive keys";
const TEST_LARGE_FILE: &str = "Large file multipart";
const TEST_5XX_RETRY: &str = "5xx retry classification";
const TEST_CLOCK_SKEW: &str = "Clock skew classification";

const LEVEL1_REQUIRED: &[&str] = &[
    TEST_PUT_GET,
    TEST_HEAD,
    TEST_DELETE,
    TEST_LIST_PREFIX_DELIMITER,
    TEST_LIST_PAGINATION,
    TEST_RANGE_GET,
    TEST_KEY_EDGE_CASES,
];

const LEVEL2_REQUIRED: &[&str] = &[
    TEST_MULTIPART_UPLOAD,
    TEST_MULTIPART_ABORT,
    TEST_CHECKSUM_UPLOAD,
    TEST_IF_NONE_MATCH,
    TEST_IF_MATCH,
    TEST_CONCURRENT_WRITES,
    TEST_CONSISTENCY_PUT,
    TEST_CONSISTENCY_DELETE,
    TEST_LARGE_FILE,
    TEST_5XX_RETRY,
    TEST_CLOCK_SKEW,
];

/// Compatibility suite runtime options.
#[derive(Debug, Clone)]
pub struct CompatibilityOptions {
    pub large_file_bytes: usize,
}

impl Default for CompatibilityOptions {
    fn default() -> Self {
        Self {
            large_file_bytes: 100 * 1024 * 1024,
        }
    }
}

impl CompatibilityOptions {
    pub fn from_env() -> Self {
        let mut options = Self::default();
        if let Ok(value) = std::env::var("S4DRIVE_COMPAT_LARGE_BYTES") {
            if let Ok(bytes) = value.parse::<usize>() {
                options.large_file_bytes = bytes.max(6 * 1024 * 1024);
            }
        }
        options
    }
}

/// Result of a single compatibility test.
#[derive(Debug, Clone)]
pub struct TestResult {
    pub name: String,
    pub passed: bool,
    pub details: String,
}

/// Overall compatibility report for a bucket.
#[derive(Debug, Clone)]
pub struct CompatibilityReport {
    pub level: u32,
    pub tests_passed: Vec<String>,
    pub tests_failed: Vec<String>,
    pub details: Vec<TestResult>,
}

impl CompatibilityReport {
    pub fn is_level2_supported(&self) -> bool {
        self.level >= 2
    }
}

impl std::fmt::Display for CompatibilityReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "S4Drive Compatibility Report")?;
        writeln!(f, "{:-<40}", "")?;
        writeln!(f, "Level: {} ({})", self.level, level_name(self.level))?;
        writeln!(
            f,
            "Passed: {} / {}",
            self.tests_passed.len(),
            self.tests_passed.len() + self.tests_failed.len()
        )?;
        writeln!(f)?;

        if !self.tests_failed.is_empty() {
            writeln!(f, "FAILED TESTS:")?;
            for t in &self.tests_failed {
                writeln!(f, "  ✗ {}", t)?;
            }
            writeln!(f)?;
        }

        writeln!(f, "Details:")?;
        for t in &self.details {
            let icon = if t.passed { "✓" } else { "✗" };
            writeln!(f, "  {} {}", icon, t.name)?;
            if !t.details.is_empty() && !t.passed {
                writeln!(f, "     {}", t.details)?;
            }
        }
        Ok(())
    }
}

fn level_name(level: u32) -> &'static str {
    match level {
        0 => "Not supported",
        1 => "Basic storage",
        2 => "Safe sync",
        3 => "Versioned reliable sync",
        4 => "Full S4 enhanced",
        _ => "Unknown",
    }
}

impl S3Adapter {
    /// Run the full S4Drive compatibility test suite.
    /// Tests are ordered by level: basic → conditional writes → advanced.
    pub async fn run_compatibility_test(&self) -> CompatibilityReport {
        self.run_compatibility_test_with_options(CompatibilityOptions::from_env())
            .await
    }

    pub async fn run_compatibility_test_with_options(
        &self,
        options: CompatibilityOptions,
    ) -> CompatibilityReport {
        let mut tests: Vec<TestResult> = Vec::new();
        let prefix = format!(".s4drive-compat-test-{}/", uuid::Uuid::now_v7());

        // ─── Level 1: Basic Storage ─────────────────────────────────

        tests.push(self.test_basic_put_get(&prefix).await);
        tests.push(self.test_head_object(&prefix).await);
        tests.push(self.test_delete_object(&prefix).await);
        tests.push(self.test_list_prefix_delimiter(&prefix).await);
        tests.push(self.test_list_pagination(&prefix).await);
        tests.push(self.test_range_get(&prefix).await);
        tests.push(self.test_key_edge_cases(&prefix).await);

        // ─── Level 2: Safe Sync ─────────────────────────────────────

        tests.push(self.test_conditional_if_none_match(&prefix).await);
        tests.push(self.test_conditional_if_match(&prefix).await);
        tests.push(self.test_concurrent_writes(&prefix).await);
        tests.push(self.test_consistency_after_put(&prefix).await);
        tests.push(self.test_consistency_after_delete(&prefix).await);
        tests.push(self.test_multipart_upload(&prefix).await);
        tests.push(self.test_multipart_abort(&prefix).await);
        tests.push(self.test_checksum_upload(&prefix).await);
        tests.push(
            self.test_large_file_multipart(&prefix, options.large_file_bytes)
                .await,
        );
        tests.push(test_5xx_retry_classification());
        tests.push(test_clock_skew_classification());

        // ─── Cleanup ────────────────────────────────────────────────
        self.cleanup_test_objects(&prefix).await;

        let level = calculate_level(&tests);
        let (passed, failed): (Vec<_>, Vec<_>) = tests.iter().cloned().partition(|t| t.passed);

        CompatibilityReport {
            level,
            tests_passed: passed.iter().map(|t| t.name.clone()).collect(),
            tests_failed: failed.iter().map(|t| t.name.clone()).collect(),
            details: tests,
        }
    }

    // ─── Individual Tests ───────────────────────────────────────────

    async fn test_basic_put_get(&self, prefix: &str) -> TestResult {
        let key = format!("{}basic-put-get", prefix);
        let data = b"Hello S4Drive!".to_vec();

        match self.put_object(&key, data.clone()).await {
            Ok(_) => match self.get_object(&key).await {
                Ok(got) if got == data => TestResult::ok(TEST_PUT_GET, "data matches"),
                Ok(got) => TestResult::fail(
                    TEST_PUT_GET,
                    &format!(
                        "data mismatch: got {} bytes, expected {}",
                        got.len(),
                        data.len()
                    ),
                ),
                Err(e) => TestResult::fail(TEST_PUT_GET, &e.to_string()),
            },
            Err(e) => TestResult::fail(TEST_PUT_GET, &e.to_string()),
        }
    }

    async fn test_head_object(&self, prefix: &str) -> TestResult {
        let key = format!("{}head-test", prefix);
        let data = b"head test data".to_vec();

        if let Err(e) = self.put_object(&key, data).await {
            return TestResult::fail(TEST_HEAD, &format!("setup failed: {}", e));
        }

        match self.head_object(&key).await {
            Ok(meta) => {
                if meta.size > 0 && !meta.etag.is_empty() {
                    TestResult::ok(
                        TEST_HEAD,
                        &format!("size={}, etag={}", meta.size, meta.etag),
                    )
                } else {
                    TestResult::fail(TEST_HEAD, "empty size or etag")
                }
            }
            Err(e) => TestResult::fail(TEST_HEAD, &e.to_string()),
        }
    }

    async fn test_delete_object(&self, prefix: &str) -> TestResult {
        let key = format!("{}delete-test", prefix);
        let data = b"to be deleted".to_vec();

        if let Err(e) = self.put_object(&key, data).await {
            return TestResult::fail(TEST_DELETE, &format!("setup failed: {}", e));
        }

        if let Err(e) = self.delete_object(&key).await {
            return TestResult::fail(TEST_DELETE, &e.to_string());
        }

        match self.get_object(&key).await {
            Ok(_) => TestResult::fail(TEST_DELETE, "object still exists after delete"),
            Err(_) => TestResult::ok(TEST_DELETE, "object properly removed"),
        }
    }

    async fn test_list_prefix_delimiter(&self, prefix: &str) -> TestResult {
        let list_prefix = format!("{}folders/", prefix);
        let root_key = format!("{}root.txt", list_prefix);
        let nested_key = format!("{}nested/file.txt", list_prefix);
        let outside_key = format!("{}outside/file.txt", prefix);

        for key in [&root_key, &nested_key, &outside_key] {
            if let Err(e) = self.put_object(key, b"data".to_vec()).await {
                return TestResult::fail(
                    TEST_LIST_PREFIX_DELIMITER,
                    &format!("setup failed for '{}': {}", key, e),
                );
            }
        }

        match self
            .list_objects_page(&list_prefix, Some("/"), 1000, None)
            .await
        {
            Ok(page) => {
                let nested_prefix = format!("{}nested/", list_prefix);
                if page.keys.contains(&root_key)
                    && !page.keys.contains(&nested_key)
                    && page.common_prefixes.contains(&nested_prefix)
                {
                    TestResult::ok(
                        TEST_LIST_PREFIX_DELIMITER,
                        "delimiter returned immediate object and nested common prefix",
                    )
                } else {
                    TestResult::fail(
                        TEST_LIST_PREFIX_DELIMITER,
                        &format!(
                            "unexpected keys={:?}, common_prefixes={:?}",
                            page.keys, page.common_prefixes
                        ),
                    )
                }
            }
            Err(e) => TestResult::fail(TEST_LIST_PREFIX_DELIMITER, &e.to_string()),
        }
    }

    async fn test_list_pagination(&self, prefix: &str) -> TestResult {
        let page_prefix = format!("{}page/", prefix);
        let expected: Vec<String> = (0..5)
            .map(|i| format!("{}file-{:02}.txt", page_prefix, i))
            .collect();

        for key in &expected {
            if let Err(e) = self.put_object(key, b"x".repeat(10)).await {
                return TestResult::fail(TEST_LIST_PAGINATION, &format!("setup failed: {}", e));
            }
        }

        let first = match self.list_objects_page(&page_prefix, None, 2, None).await {
            Ok(page) => page,
            Err(e) => return TestResult::fail(TEST_LIST_PAGINATION, &e.to_string()),
        };

        if !first.is_truncated || first.next_continuation_token.is_none() {
            return TestResult::fail(
                TEST_LIST_PAGINATION,
                "first page was not truncated or had no ContinuationToken",
            );
        }

        let second = match self
            .list_objects_page(
                &page_prefix,
                None,
                1000,
                first.next_continuation_token.as_deref(),
            )
            .await
        {
            Ok(page) => page,
            Err(e) => return TestResult::fail(TEST_LIST_PAGINATION, &e.to_string()),
        };

        let mut listed = first.keys;
        listed.extend(second.keys);
        if expected.iter().all(|key| listed.contains(key)) {
            TestResult::ok(
                TEST_LIST_PAGINATION,
                &format!("ContinuationToken returned {} objects", listed.len()),
            )
        } else {
            TestResult::fail(
                TEST_LIST_PAGINATION,
                &format!("expected {:?}, got {:?}", expected, listed),
            )
        }
    }

    async fn test_range_get(&self, prefix: &str) -> TestResult {
        let key = format!("{}range", prefix);
        let data = b"0123456789ABCDEF".to_vec();

        if let Err(e) = self.put_object(&key, data).await {
            return TestResult::fail(TEST_RANGE_GET, &format!("setup failed: {}", e));
        }

        match self.get_object_range(&key, "bytes=0-4").await {
            Ok(part) if part == b"01234" => TestResult::ok(TEST_RANGE_GET, "first 5 bytes match"),
            Ok(part) => TestResult::fail(
                TEST_RANGE_GET,
                &format!("expected '01234', got {:?}", String::from_utf8_lossy(&part)),
            ),
            Err(e) => TestResult::fail(TEST_RANGE_GET, &e.to_string()),
        }
    }

    async fn test_key_edge_cases(&self, prefix: &str) -> TestResult {
        let long_name = "a".repeat(220);
        let keys = [
            format!("{}unicode/文件-имя-файла.txt", prefix),
            format!("{}case/File.txt", prefix),
            format!("{}case/file.txt", prefix),
            format!("{}special/space #+;=@[].txt", prefix),
            format!("{}long/{}/file.txt", prefix, long_name),
        ];

        for key in &keys {
            if let Err(e) = self.put_object(key, b"edge case".to_vec()).await {
                return TestResult::fail(
                    TEST_KEY_EDGE_CASES,
                    &format!("put failed for '{}': {}", key, e),
                );
            }
            match self.get_object(key).await {
                Ok(got) if got == b"edge case" => {}
                Ok(got) => {
                    return TestResult::fail(
                        TEST_KEY_EDGE_CASES,
                        &format!("data mismatch for '{}': {} bytes", key, got.len()),
                    )
                }
                Err(e) => {
                    return TestResult::fail(
                        TEST_KEY_EDGE_CASES,
                        &format!("get failed for '{}': {}", key, e),
                    )
                }
            }
        }

        match self.list_objects(&format!("{}case/", prefix)).await {
            Ok(listed) if listed.contains(&keys[1]) && listed.contains(&keys[2]) => TestResult::ok(
                TEST_KEY_EDGE_CASES,
                "unicode, long, special, and case keys work",
            ),
            Ok(listed) => TestResult::fail(
                TEST_KEY_EDGE_CASES,
                &format!("case-sensitive keys missing from list: {:?}", listed),
            ),
            Err(e) => TestResult::fail(TEST_KEY_EDGE_CASES, &e.to_string()),
        }
    }

    async fn test_conditional_if_none_match(&self, prefix: &str) -> TestResult {
        let key = format!("{}if-none-match", prefix);
        let data = b"first version".to_vec();

        match self.put_if_not_exists(&key, data).await {
            Ok(true) => {}
            Ok(false) => return TestResult::fail(TEST_IF_NONE_MATCH, "first write was blocked"),
            Err(e) => return TestResult::fail(TEST_IF_NONE_MATCH, &e.to_string()),
        }

        match self.put_if_not_exists(&key, b"second".to_vec()).await {
            Ok(false) => TestResult::ok(TEST_IF_NONE_MATCH, "second write correctly blocked"),
            Ok(true) => TestResult::fail(TEST_IF_NONE_MATCH, "second write overwrote object"),
            Err(e) => TestResult::fail(TEST_IF_NONE_MATCH, &e.to_string()),
        }
    }

    async fn test_conditional_if_match(&self, prefix: &str) -> TestResult {
        let key = format!("{}if-match", prefix);

        if let Err(e) = self.put_object(&key, b"v1".to_vec()).await {
            return TestResult::fail(TEST_IF_MATCH, &format!("setup failed: {}", e));
        }

        let meta = match self.head_object(&key).await {
            Ok(m) => m,
            Err(e) => return TestResult::fail(TEST_IF_MATCH, &e.to_string()),
        };

        match self.put_if_match(&key, b"v2".to_vec(), &meta.etag).await {
            Ok(true) => {}
            Ok(false) => return TestResult::fail(TEST_IF_MATCH, "valid ETag was rejected"),
            Err(e) => return TestResult::fail(TEST_IF_MATCH, &e.to_string()),
        }

        match self.put_if_match(&key, b"v3".to_vec(), "wrong-etag").await {
            Ok(false) => TestResult::ok(TEST_IF_MATCH, "wrong ETag correctly rejected"),
            Ok(true) => TestResult::fail(TEST_IF_MATCH, "wrong ETag was accepted"),
            Err(e) => TestResult::fail(TEST_IF_MATCH, &e.to_string()),
        }
    }

    async fn test_concurrent_writes(&self, prefix: &str) -> TestResult {
        let key = format!("{}concurrent", prefix);

        if let Err(e) = self.put_object(&key, b"base".to_vec()).await {
            return TestResult::fail(TEST_CONCURRENT_WRITES, &format!("setup failed: {}", e));
        }

        let meta = match self.head_object(&key).await {
            Ok(m) => m,
            Err(e) => return TestResult::fail(TEST_CONCURRENT_WRITES, &e.to_string()),
        };

        let writer_a = self.put_if_match(&key, b"writer-a".to_vec(), &meta.etag);
        let writer_b = self.put_if_match(&key, b"writer-b".to_vec(), &meta.etag);

        match tokio::join!(writer_a, writer_b) {
            (Ok(true), Ok(false)) | (Ok(false), Ok(true)) => {
                TestResult::ok(TEST_CONCURRENT_WRITES, "one write won and one got 412")
            }
            (a, b) => TestResult::fail(
                TEST_CONCURRENT_WRITES,
                &format!(
                    "expected exactly one success, got left={:?}, right={:?}",
                    a, b
                ),
            ),
        }
    }

    async fn test_consistency_after_put(&self, prefix: &str) -> TestResult {
        let key = format!("{}consistency-put", prefix);
        let data = b"consistency check".to_vec();

        if let Err(e) = self.put_object(&key, data.clone()).await {
            return TestResult::fail(TEST_CONSISTENCY_PUT, &e.to_string());
        }

        match self.get_object(&key).await {
            Ok(got) if got == data => {
                TestResult::ok(TEST_CONSISTENCY_PUT, "read-after-write consistent")
            }
            Ok(got) => TestResult::fail(
                TEST_CONSISTENCY_PUT,
                &format!("data mismatch: got {} bytes", got.len()),
            ),
            Err(e) => TestResult::fail(TEST_CONSISTENCY_PUT, &e.to_string()),
        }
    }

    async fn test_consistency_after_delete(&self, prefix: &str) -> TestResult {
        let key = format!("{}consistency-delete", prefix);

        if let Err(e) = self.put_object(&key, b"temp".to_vec()).await {
            return TestResult::fail(TEST_CONSISTENCY_DELETE, &format!("setup failed: {}", e));
        }

        if let Err(e) = self.delete_object(&key).await {
            return TestResult::fail(TEST_CONSISTENCY_DELETE, &e.to_string());
        }

        match self.head_object(&key).await {
            Ok(_) => TestResult::fail(TEST_CONSISTENCY_DELETE, "object still exists after delete"),
            Err(_) => TestResult::ok(TEST_CONSISTENCY_DELETE, "read-after-delete returned 404"),
        }
    }

    async fn test_multipart_upload(&self, prefix: &str) -> TestResult {
        let key = format!("{}multipart", prefix);
        let data = vec![0xABu8; 6 * 1024 * 1024];

        match self.multipart_upload(&key, data.clone()).await {
            Ok(etag) => match self.get_object(&key).await {
                Ok(got) if got == data => TestResult::ok(
                    TEST_MULTIPART_UPLOAD,
                    &format!("{} bytes via multipart, etag={}", got.len(), etag),
                ),
                Ok(got) => TestResult::fail(
                    TEST_MULTIPART_UPLOAD,
                    &format!("uploaded {} bytes, got {}", data.len(), got.len()),
                ),
                Err(e) => TestResult::fail(TEST_MULTIPART_UPLOAD, &e.to_string()),
            },
            Err(e) => TestResult::fail(TEST_MULTIPART_UPLOAD, &e.to_string()),
        }
    }

    async fn test_multipart_abort(&self, prefix: &str) -> TestResult {
        let key = format!("{}multipart-abort", prefix);
        let upload = match self.create_multipart_upload(&key).await {
            Ok(upload) => upload,
            Err(e) => return TestResult::fail(TEST_MULTIPART_ABORT, &e.to_string()),
        };

        if let Err(e) = self
            .upload_multipart_part(&key, &upload.upload_id, 1, vec![0xCD; 5 * 1024 * 1024])
            .await
        {
            let _ = self.abort_multipart_upload(&key, &upload.upload_id).await;
            return TestResult::fail(TEST_MULTIPART_ABORT, &format!("part upload failed: {}", e));
        }

        match self.abort_multipart_upload(&key, &upload.upload_id).await {
            Ok(()) => match self.head_object(&key).await {
                Ok(_) => TestResult::fail(TEST_MULTIPART_ABORT, "aborted upload created object"),
                Err(_) => TestResult::ok(TEST_MULTIPART_ABORT, "multipart upload aborted cleanly"),
            },
            Err(e) => TestResult::fail(TEST_MULTIPART_ABORT, &e.to_string()),
        }
    }

    async fn test_checksum_upload(&self, prefix: &str) -> TestResult {
        let key = format!("{}checksum", prefix);
        let data = b"checksum validated upload".to_vec();

        match self.put_object_with_checksum(&key, data.clone()).await {
            Ok(_) => match self.get_object(&key).await {
                Ok(got) if got == data => {
                    TestResult::ok(TEST_CHECKSUM_UPLOAD, "Content-MD5 upload validated")
                }
                Ok(got) => TestResult::fail(
                    TEST_CHECKSUM_UPLOAD,
                    &format!("data mismatch: got {} bytes", got.len()),
                ),
                Err(e) => TestResult::fail(TEST_CHECKSUM_UPLOAD, &e.to_string()),
            },
            Err(e) => TestResult::fail(TEST_CHECKSUM_UPLOAD, &e.to_string()),
        }
    }

    async fn test_large_file_multipart(&self, prefix: &str, bytes: usize) -> TestResult {
        let key = format!("{}large-multipart", prefix);
        let data = vec![0x5Au8; bytes];

        match self.multipart_upload(&key, data.clone()).await {
            Ok(_) => match self.head_object(&key).await {
                Ok(meta) if meta.size == bytes as u64 => TestResult::ok(
                    TEST_LARGE_FILE,
                    &format!("{} bytes uploaded via multipart", bytes),
                ),
                Ok(meta) => TestResult::fail(
                    TEST_LARGE_FILE,
                    &format!("expected {} bytes, head reported {}", bytes, meta.size),
                ),
                Err(e) => TestResult::fail(TEST_LARGE_FILE, &e.to_string()),
            },
            Err(e) => TestResult::fail(TEST_LARGE_FILE, &e.to_string()),
        }
    }

    async fn cleanup_test_objects(&self, prefix: &str) {
        if let Ok(keys) = self.list_objects(prefix).await {
            for key in keys {
                let _ = self.delete_object(&key).await;
            }
        }
    }
}

// ─── Helper impls ──────────────────────────────────────────────────────

impl TestResult {
    fn ok(name: &str, details: &str) -> Self {
        Self {
            name: name.to_string(),
            passed: true,
            details: details.to_string(),
        }
    }

    fn fail(name: &str, details: &str) -> Self {
        Self {
            name: name.to_string(),
            passed: false,
            details: details.to_string(),
        }
    }
}

fn test_5xx_retry_classification() -> TestResult {
    let classified = classify_error_code("InternalError", 500);
    let policy = RetryPolicy::default();
    let core_error = crate::error::CoreError::S3("HTTP 500 InternalError".into());

    if classified.is_retryable && policy.should_retry(&core_error, 0) {
        TestResult::ok(TEST_5XX_RETRY, "5xx errors are retryable")
    } else {
        TestResult::fail(TEST_5XX_RETRY, "5xx error was not classified as retryable")
    }
}

fn test_clock_skew_classification() -> TestResult {
    let classified = classify_error_code("RequestTimeTooSkewed", 403);
    if classified.message.contains("Clock skew") && !classified.is_retryable {
        TestResult::ok(
            TEST_CLOCK_SKEW,
            "clock skew produces a human-readable error",
        )
    } else {
        TestResult::fail(
            TEST_CLOCK_SKEW,
            &format!("unexpected classification: {:?}", classified),
        )
    }
}

fn calculate_level(tests: &[TestResult]) -> u32 {
    let level1 = LEVEL1_REQUIRED.iter().all(|name| test_passed(tests, name));
    let level2 = level1 && LEVEL2_REQUIRED.iter().all(|name| test_passed(tests, name));

    if level2 {
        2
    } else if level1 {
        1
    } else {
        0
    }
}

fn test_passed(tests: &[TestResult], name: &str) -> bool {
    tests.iter().any(|test| test.name == name && test.passed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_requires_all_level1_tests() {
        let mut results: Vec<TestResult> = LEVEL1_REQUIRED
            .iter()
            .map(|name| TestResult::ok(name, "ok"))
            .collect();
        assert_eq!(calculate_level(&results), 1);

        results.retain(|result| result.name != TEST_RANGE_GET);
        assert_eq!(calculate_level(&results), 0);
    }

    #[test]
    fn level2_requires_safe_sync_tests() {
        let mut results: Vec<TestResult> = LEVEL1_REQUIRED
            .iter()
            .chain(LEVEL2_REQUIRED.iter())
            .map(|name| TestResult::ok(name, "ok"))
            .collect();
        assert_eq!(calculate_level(&results), 2);

        results.retain(|result| result.name != TEST_IF_MATCH);
        assert_eq!(calculate_level(&results), 1);
    }
}
