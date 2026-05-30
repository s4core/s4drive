# S4Drive Reliability Specification

> **Дата:** 30 мая 2026

---

## 1. Data Integrity Guarantees

S4Drive гарантирует, что при любых обстоятельствах (crash, network failure, concurrent access) не будет потерян ни один байт пользовательских данных.

### 1.1. Write Atomicity

Все операции записи проходят через атомарный протокол:

```
1. Content blob upload (If-None-Match) → success/fail
2. Metadata commit (If-Match on head) → success/fail/conflict
3. Local file replace (atomic rename) → success/fail
```

Если шаг 1 или 2 не удался — данные остаются в очереди. Никакого частичного состояния.

### 1.2. Read Integrity

- Каждый скачанный blob проверяется по checksum (SHA-256/BLAKE3)
- Если checksum не совпал — перекачать
- После 3 неудач — error в diagnostics

### 1.3. Conflict Safety

- **Last writer wins запрещён** для содержимого файлов
- При параллельных изменениях всегда создаются sibling revisions
- Пользователь явно выбирает версию

## 2. Crash Recovery

| Scenario | Recovery |
|----------|----------|
| Crash during content upload | Temp file очищается при старте. Pending op остаётся в очереди |
| Crash during metadata commit | Content blob уже загружен (blob остаётся). Commit retry при старте |
| Crash during local file replace | Staging file остаётся. При старте проверяется атомарность |
| Power loss during SQLite write | WAL mode гарантирует rollback незавершённых транзакций |
| Corrupt local DB | Integrity check → auto-repair from bucket metadata |

## 3. Network Resilience

- Все transfer jobs persistent (SQLite)
- Exponential backoff при ошибках: 1s → 2s → 4s → 8s → 30s → 60s → 300s
- Resume multipart upload после обрыва (не начинать заново)
- Graceful handling: 412 Precondition Failed, 409 Conflict, 5xx

## 4. Edge Cases

| Edge Case | Behavior |
|-----------|----------|
| Disk full | Pause sync. Уведомить UI. Resume после освобождения места |
| File locked by editor | Wait + retry. После N retry — skip + notify |
| Sleep/Wake | Save state before sleep. Partial rescan after wake |
| Clock skew (NTP) | Все timestamp'ы серверные. Logical clocks для ordering |
| Unicode filenames | Нормализация NFC. Запрет reserved names (CON, PRN, AUX) |
| Very long paths (>255) | Обрезка с предупреждением |
| Reserved Windows names | Переименование + предупреждение |
| Case-only rename | Разрешён. file_id остаётся тем же |
