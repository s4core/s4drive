# S4Drive Sync Semantics Specification

> **Дата:** 30 мая 2026

---

## 1. Core Model

S4Drive синхронизирует не "файлы", а **операции над версиями файлов**.

Состояние файла описывается:
- `file_id` — стабильный идентификатор (UUID v7)
- История revisions (revision graph)
- `version_vector` — векторные часы для определения concurrent изменений

## 2. Local Change Flow

```
File watcher event
    → Debounce (500ms stability)
    → Fingerprint check (size + mtime)
    → Content hash (SHA-256/BLAKE3)
    → Compare with local DB
    → If changed: create pending revision → upload queue
    → Upload blob to S3 (If-None-Match)
    → Metadata commit (CAS on head with If-Match)
    → If success: mark synced
    → If 412: conflict resolution
```

## 3. Remote Change Flow

```
Poll S3 head (or change feed)
    → Download new ops
    → Verify signatures
    → Apply to local metadata graph
    → Download needed content blobs
    → Atomic replace local file (staging → rename)
    → If local file also changed: conflict
```

## 4. CAS Commit Protocol

```
1. Read current head pointer → get ETag
2. Write operation to .s4drive/meta/ops/ (If-None-Match: *)
3. Write head pointer (If-Match: <etag>)
4. If 412: re-read head → apply remote ops → retry
```

## 5. Conflict Detection

Конфликт = два устройства создали revision от одной base revision.

Детектируется через:
- **CAS failure** (412 Precondition Failed) при обновлении head
- **Version vector comparison**: ни один vector не доминирует над другим
- **Revision graph**: sibling revisions (один parent, два разных child)

## 6. Rename Semantics

- Rename — операция над `file_id`, не delete+create
- file_id остаётся тем же
- История версий сохраняется
- Edit после rename не конфликтует (тот же file_id)

## 7. Delete Semantics

- Delete → tombstone (не hard delete)
- Файл убирается из видимого дерева
- Content blobs сохраняются (retention period)
- Restore = восстановление из tombstone
- Hard delete = только по явному действию + подтверждению

## 8. Polling Strategy

| Состояние | Интервал |
|-----------|----------|
| Активная синхронизация | 30s |
| Пауза (нет изменений > 1 час) | 60s → 120s → 300s |
| После изменения | 30s |
| Если change feed доступен | Near real-time (SSE/WebSocket) |
