# S4Drive S3 Compatibility Matrix

> **Дата:** 30 мая 2026

---

## Compatibility Levels

| Level | Name | Requirements | S4Drive Support |
|-------|------|-------------|-----------------|
| 0 | Not supported | Не проходит базовые PUT/GET/DELETE | ❌ Отказ |
| 1 | Basic storage | PUT/GET/HEAD/DELETE + LIST + Range GET | ⚠️ Только просмотр (read-only) |
| **2** | **Safe sync** | Level 1 + Conditional writes (If-Match, If-None-Match) + Strong consistency | ✅ **Full two-way sync** |
| 3 | Versioned sync | Level 2 + S3 Versioning | ✅ + Version recovery safety net |
| 4 | Enhanced | Level 3 + Change feed + Object Lock | ✅ + Real-time sync + metadata protection |

## Test Suite (Level 2 - Safe Sync)

| # | Test | Expected |
|---|------|----------|
| 1 | PUT object → GET object | Content matches |
| 2 | HEAD object | Correct size, ETag, metadata |
| 3 | DELETE object → GET | 404 Not Found |
| 4 | LIST with prefix/delimiter | Correct pagination |
| 5 | LIST with ContinuationToken | Stable pagination |
| 6 | Range GET (first 1KB) | Correct range |
| 7 | Multipart upload (5 parts) | Complete → single object |
| 8 | Abort multipart | Cleanup (no orphan parts) |
| 9 | Conditional write: If-Match (correct ETag) | Success (200) |
| 10 | Conditional write: If-Match (wrong ETag) | 412 Precondition Failed |
| 11 | Conditional write: If-None-Match: * (no existing) | Success (200) |
| 12 | Conditional write: If-None-Match: * (existing) | 412 Precondition Failed |
| 13 | Concurrent writes (2 clients, same object) | One success, one 412 |
| 14 | Strong read-after-write: PUT → GET | Immediate visibility |
| 15 | Strong read-after-write: DELETE → GET | Immediate 404 |
| 16 | Large file (100 MB) multipart | Success |
| 17 | Unicode key (文件名.txt) | Success |
| 18 | Special characters in key | Success |
| 19 | Very long path (>200 chars) | Success |
| 20 | Network interruption during upload | Retryable error |
| 21 | 5xx server error → client retry | Eventual success |

## Custom S3 Requirements

Your S3 backend MUST pass Level 2 tests for S4Drive to work reliably.

Minimum required:
- Strong read-after-write consistency
- Conditional writes (If-Match, If-None-Match)
- Correct error codes (412, 404, 409, 403, 5xx)
- Multipart upload
- Range GET
- Stable LIST pagination

Optional but recommended:
- S3 Versioning
- Change feed (WebSocket/SSE)
- Server-side encryption
- Object Lock
- Short-lived credentials
