# S4Drive Conflict Policy

> **Дата:** 30 мая 2026

---

## 1. Philosophy

- Конфликты неизбежны при параллельной работе (человек + человек, человек + LLM-агент)
- Конфликт — это не ошибка, а выбор версии
- Приоритет: **никогда не терять данные** > удобство

## 2. Правила

1. **Last writer wins запрещён** для содержимого файла
2. Каждая локальная правка имеет base revision
3. Rename/move не конфликтуют с edit (file_id)
4. Delete → tombstone (не hard delete)
5. Conflict copy — fallback: `name (conflict from <Device> <Date>).ext`
6. UI обязан показать причину человеческим языком

## 3. Conflict Matrix

| Scenario | Resolution Strategy |
|----------|-------------------|
| Edit-edit (text) | 3-way merge. Если merge чистый → merged revision. Если нет → conflict resolver |
| Edit-edit (binary) | Sibling revisions + conflict copy |
| Rename + edit | **Не конфликт.** Edit к тому же file_id под новым именем |
| Delete + edit | "Deleted remotely, edited locally" conflict. Сохранить локальную правку |
| Create-create (same name) | Детерминированный winner (lexicographic device_id) |
| Rename-rename (different names) | Winner по device_id. UI conflict для второго |
| Case-only rename | Нормализация имён. Предупреждение |
| External S3 change | "External untrusted revision" |

## 4. Conflict Naming Convention

```
<original_name> (conflict from <Device Name> <YYYY-MM-DD> HH:MM).<ext>
```

Пример: `report (conflict from MacBook Pro 2026-05-30 14:32).docx`

## 5. User-Facing Messages

| Situation | Message |
|-----------|---------|
| Edit-edit | "This file was edited on <Device A> and <Device B> at the same time" |
| Delete-edit | "This file was deleted on <Device A> but edited locally" |
| Create-create | "Two files with the same name were created on different devices" |
| External change | "This file was modified by an external tool" |

## 6. Auto-Resolution Rules

- **Structural conflicts** (rename-rename, create-create): deterministic winner
- **Text files** (.txt, .md, .json, .yaml, .toml, .csv): 3-way merge attempt
- **Binary files** (everything else): always sibling versions
- **Delete-edit**: never auto-delete local changes
