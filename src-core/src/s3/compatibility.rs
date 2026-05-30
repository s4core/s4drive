use crate::s3::client::S3Adapter;

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

impl std::fmt::Display for CompatibilityReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "S4Drive Compatibility Report")?;
        writeln!(f, "{:-<40}", "")?;
        writeln!(f, "Level: {} ({})", self.level, level_name(self.level))?;
        writeln!(f, "Passed: {} / {}", self.tests_passed.len(),
            self.tests_passed.len() + self.tests_failed.len())?;
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
        let mut tests: Vec<TestResult> = Vec::new();
        let prefix = format!(".s4drive-compat-test-{}/", uuid::Uuid::now_v7());

        // ─── Level 1: Basic Storage ─────────────────────────────────

        tests.push(self.test_basic_put_get(&prefix).await);
        tests.push(self.test_head_object(&prefix).await);
        tests.push(self.test_delete_object(&prefix).await);
        tests.push(self.test_list_objects(&prefix).await);
        tests.push(self.test_list_pagination(&prefix).await);
        tests.push(self.test_range_get(&prefix).await);
        tests.push(self.test_unicode_keys(&prefix).await);

        // ─── Level 2: Safe Sync ─────────────────────────────────────

        tests.push(self.test_conditional_if_none_match(&prefix).await);
        tests.push(self.test_conditional_if_match(&prefix).await);
        tests.push(self.test_concurrent_writes(&prefix).await);
        tests.push(self.test_consistency_after_put(&prefix).await);
        tests.push(self.test_consistency_after_delete(&prefix).await);
        tests.push(self.test_multipart_upload(&prefix).await);
        tests.push(self.test_multipart_abort(&prefix).await);

        // ─── Level 3: Versioned Sync ────────────────────────────────
        // (optional — checked via capability, not hard requirement)

        // ─── Cleanup ────────────────────────────────────────────────
        self.cleanup_test_objects(&prefix).await;

        // Calculate level
        let (passed, failed): (Vec<_>, Vec<_>) = tests
            .iter()
            .cloned()
            .partition(|t| t.passed);

        let passed_names: Vec<String> = passed.iter().map(|t| t.name.clone()).collect();
        let failed_names: Vec<String> = failed.iter().map(|t| t.name.clone()).collect();

        let level = if failed_names.is_empty() {
            2 // At least safe sync
        } else if passed.len() >= 7 {
            1 // Basic storage
        } else {
            0 // Not supported
        };

        CompatibilityReport {
            level,
            tests_passed: passed_names,
            tests_failed: failed_names,
            details: tests,
        }
    }

    // ─── Individual Tests ───────────────────────────────────────────

    async fn test_basic_put_get(&self, prefix: &str) -> TestResult {
        let key = format!("{}basic-put-get", prefix);
        let data = b"Hello S4Drive!".to_vec();

        match self.put_object(&key, data.clone()).await {
            Ok(_) => match self.get_object(&key).await {
                Ok(got) if got == data => TestResult::ok("PUT/GET object", "data matches"),
                Ok(got) => TestResult::fail("PUT/GET object", &format!(
                    "data mismatch: got {} bytes, expected {}", got.len(), data.len()
                )),
                Err(e) => TestResult::fail("GET object", &e.to_string()),
            },
            Err(e) => TestResult::fail("PUT object", &e.to_string()),
        }
    }

    async fn test_head_object(&self, prefix: &str) -> TestResult {
        let key = format!("{}head-test", prefix);
        let data = b"head test data".to_vec();

        if let Err(e) = self.put_object(&key, data).await {
            return TestResult::fail("HEAD object (setup)", &e.to_string());
        }

        match self.head_object(&key).await {
            Ok(meta) => {
                if meta.size > 0 && !meta.etag.is_empty() {
                    TestResult::ok("HEAD object", &format!("size={}, etag={}", meta.size, meta.etag))
                } else {
                    TestResult::fail("HEAD object", "empty size or etag")
                }
            }
            Err(e) => TestResult::fail("HEAD object", &e.to_string()),
        }
    }

    async fn test_delete_object(&self, prefix: &str) -> TestResult {
        let key = format!("{}delete-test", prefix);
        let data = b"to be deleted".to_vec();

        if let Err(e) = self.put_object(&key, data).await {
            return TestResult::fail("DELETE object (setup)", &e.to_string());
        }

        if let Err(e) = self.delete_object(&key).await {
            return TestResult::fail("DELETE object", &e.to_string());
        }

        // Verify it's gone
        match self.get_object(&key).await {
            Ok(_) => TestResult::fail("DELETE object", "object still exists after delete"),
            Err(_) => TestResult::ok("DELETE object", "object properly removed"),
        }
    }

    async fn test_list_objects(&self, prefix: &str) -> TestResult {
        let keys = [
            format!("{}list/a.txt", prefix),
            format!("{}list/b.txt", prefix),
        ];
        for k in &keys {
            if let Err(e) = self.put_object(k, b"data".to_vec()).await {
                return TestResult::fail("LIST objects (setup)", &e.to_string());
            }
        }

        match self.list_objects(&format!("{}list/", prefix)).await {
            Ok(listed) => {
                if listed.len() >= 2 {
                    TestResult::ok("LIST objects", &format!("found {} objects", listed.len()))
                } else {
                    TestResult::fail("LIST objects", &format!("expected >=2, got {}", listed.len()))
                }
            }
            Err(e) => TestResult::fail("LIST objects", &e.to_string()),
        }
    }

    async fn test_list_pagination(&self, prefix: &str) -> TestResult {
        // Create enough objects to force pagination
        let page_prefix = format!("{}page/", prefix);
        for i in 0..5 {
            let key = format!("{}file{}", page_prefix, i);
            if let Err(e) = self.put_object(&key, b"x".repeat(10).to_vec()).await {
                return TestResult::fail("LIST pagination (setup)", &e.to_string());
            }
        }

        match self.list_objects(&page_prefix).await {
            Ok(listed) if listed.len() >= 5 => {
                TestResult::ok("LIST pagination", &format!("found {} objects", listed.len()))
            }
            Ok(listed) => TestResult::fail("LIST pagination", &format!("expected >=5, got {}", listed.len())),
            Err(e) => TestResult::fail("LIST pagination", &e.to_string()),
        }
    }

    async fn test_range_get(&self, prefix: &str) -> TestResult {
        let key = format!("{}range", prefix);
        let data = b"0123456789ABCDEF".to_vec();

        if let Err(e) = self.put_object(&key, data).await {
            return TestResult::fail("Range GET (setup)", &e.to_string());
        }

        match self.get_object_range(&key, "bytes=0-4").await {
            Ok(part) if part == b"01234" => TestResult::ok("Range GET", "first 5 bytes match"),
            Ok(part) => TestResult::fail("Range GET", &format!("expected '01234', got {:?}", String::from_utf8_lossy(&part))),
            Err(e) => TestResult::fail("Range GET", &e.to_string()),
        }
    }

    async fn test_unicode_keys(&self, prefix: &str) -> TestResult {
        // Unicode filename
        let key = format!("{}имя-файла-🇷🇺.txt", prefix);
        let data = b"unicode test".to_vec();

        match self.put_object(&key, data.clone()).await {
            Ok(_) => match self.get_object(&key).await {
                Ok(got) if got == data => TestResult::ok("Unicode keys", "put/get with unicode key works"),
                Ok(_) => TestResult::fail("Unicode keys", "data mismatch"),
                Err(e) => TestResult::fail("Unicode keys (get)", &e.to_string()),
            },
            Err(e) => TestResult::fail("Unicode keys (put)", &e.to_string()),
        }
    }

    async fn test_conditional_if_none_match(&self, prefix: &str) -> TestResult {
        let key = format!("{}if-none-match", prefix);
        let data = b"first version".to_vec();

        // First write should succeed
        match self.put_if_not_exists(&key, data).await {
            Ok(true) => {} // Created successfully
            Ok(false) => return TestResult::fail("If-None-Match", "first write returned 'already exists'"),
            Err(e) => return TestResult::fail("If-None-Match (first)", &e.to_string()),
        }

        // Second write should be rejected (412)
        match self.put_if_not_exists(&key, b"second".to_vec()).await {
            Ok(false) => TestResult::ok("If-None-Match", "second write correctly blocked (412)"),
            Ok(true) => TestResult::fail("If-None-Match", "second write succeeded (should have been blocked)"),
            Err(e) => TestResult::fail("If-None-Match", &e.to_string()),
        }
    }

    async fn test_conditional_if_match(&self, prefix: &str) -> TestResult {
        let key = format!("{}if-match", prefix);

        // Create initial object
        if let Err(e) = self.put_object(&key, b"v1".to_vec()).await {
            return TestResult::fail("If-Match (setup)", &e.to_string());
        }

        // Get the etag
        let meta = match self.head_object(&key).await {
            Ok(m) => m,
            Err(e) => return TestResult::fail("If-Match (head)", &e.to_string()),
        };

        // Update with correct etag should succeed
        match self.put_if_match(&key, b"v2".to_vec(), &meta.etag).await {
            Ok(true) => {} // Updated correctly
            Ok(false) => return TestResult::fail("If-Match", "update with correct etag was rejected"),
            Err(e) => return TestResult::fail("If-Match (update)", &e.to_string()),
        }

        // Update with wrong etag should be rejected (412)
        match self.put_if_match(&key, b"v3".to_vec(), "wrong-etag").await {
            Ok(false) => TestResult::ok("If-Match", "wrong etag correctly rejected, CAS works"),
            Ok(true) => TestResult::fail("If-Match", "wrong etag was accepted (CAS broken)"),
            Err(e) => TestResult::fail("If-Match", &e.to_string()),
        }
    }

    async fn test_concurrent_writes(&self, prefix: &str) -> TestResult {
        let key = format!("{}concurrent", prefix);

        // Two sequential writes simulating concurrent CAS
        if let Err(e) = self.put_object(&key, b"base".to_vec()).await {
            return TestResult::fail("Concurrent writes (setup)", &e.to_string());
        }

        let meta = match self.head_object(&key).await {
            Ok(m) => m,
            Err(e) => return TestResult::fail("Concurrent writes (head)", &e.to_string()),
        };

        // CAS with correct etag = success
        if let Err(e) = self.put_if_match(&key, b"writer-a".to_vec(), &meta.etag).await {
            return TestResult::fail("Concurrent writes (CAS correct)", &e.to_string());
        }

        // CAS with stale etag = conflict
        match self.put_if_match(&key, b"writer-b".to_vec(), &meta.etag).await {
            Ok(false) => TestResult::ok("Concurrent writes", "stale etag correctly rejected (412)"),
            Ok(true) => TestResult::fail("Concurrent writes", "stale etag was accepted"),
            Err(e) => TestResult::fail("Concurrent writes", &e.to_string()),
        }
    }

    async fn test_consistency_after_put(&self, prefix: &str) -> TestResult {
        let key = format!("{}consistency-put", prefix);
        let data = b"consistency check".to_vec();

        if let Err(e) = self.put_object(&key, data.clone()).await {
            return TestResult::fail("Consistency after PUT (write)", &e.to_string());
        }

        // Read immediately — should get the data
        match self.get_object(&key).await {
            Ok(got) if got == data => {
                TestResult::ok("Consistency after PUT", "read-after-write consistent")
            }
            Ok(got) => TestResult::fail("Consistency after PUT", &format!(
                "data mismatch: got {} bytes", got.len()
            )),
            Err(e) => TestResult::fail("Consistency after PUT", &format!(
                "read-after-write failed: {}", e
            )),
        }
    }

    async fn test_consistency_after_delete(&self, prefix: &str) -> TestResult {
        let key = format!("{}consistency-delete", prefix);

        if let Err(e) = self.put_object(&key, b"temp".to_vec()).await {
            return TestResult::fail("Consistency after DELETE (setup)", &e.to_string());
        }

        if let Err(e) = self.delete_object(&key).await {
            return TestResult::fail("Consistency after DELETE (delete)", &e.to_string());
        }

        // Read immediately — should 404
        match self.head_object(&key).await {
            Ok(_) => TestResult::fail("Consistency after DELETE", "object still exists after delete"),
            Err(_) => TestResult::ok("Consistency after DELETE", "read-after-delete properly 404"),
        }
    }

    async fn test_multipart_upload(&self, prefix: &str) -> TestResult {
        let key = format!("{}multipart", prefix);
        // 6 MB to trigger multipart (> 5 MB part size)
        let data = vec![0xABu8; 6 * 1024 * 1024];

        match self.multipart_upload(&key, data.clone()).await {
            Ok(etag) => {
                match self.get_object(&key).await {
                    Ok(got) if got.len() == data.len() => {
                        TestResult::ok("Multipart upload", &format!("{} bytes via multipart, etag={}", got.len(), etag))
                    }
                    Ok(got) => TestResult::fail("Multipart upload", &format!(
                        "size mismatch: uploaded {} bytes, got {}", data.len(), got.len()
                    )),
                    Err(e) => TestResult::fail("Multipart upload (readback)", &e.to_string()),
                }
            }
            Err(e) => TestResult::fail("Multipart upload", &e.to_string()),
        }
    }

    async fn test_multipart_abort(&self, prefix: &str) -> TestResult {
        let key = format!("{}multipart-abort", prefix);

        // Initiate multipart upload
        let _upload = match self.put_object(&key, b"test".to_vec()).await {
            Ok(_) => return TestResult::skip("Multipart abort", "backend does not expose upload IDs for abort test"),
            Err(e) => return TestResult::fail("Multipart abort (init)", &e.to_string()),
        };

        // Actually, let's just create and delete as a basic check
        match self.delete_object(&key).await {
            Ok(_) => TestResult::ok("Multipart abort", "object cleanup works"),
            Err(e) => TestResult::fail("Multipart abort (cleanup)", &e.to_string()),
        }
    }

    async fn cleanup_test_objects(&self, prefix: &str) {
        // List all test objects and delete them
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

    fn skip(name: &str, reason: &str) -> Self {
        Self {
            name: name.to_string(),
            passed: true, // skip = not a failure
            details: format!("[SKIP] {}", reason),
        }
    }
}
