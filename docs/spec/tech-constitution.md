# S4Drive Technical Constitution

> **Утверждено:** 30 мая 2026
> **Статус:** Active

Это инженерная конституция проекта. Нарушение любого пункта требует формального пересмотра через ADR.

---

## 1. Определение продукта

**S4Drive** — это Drive-like приложение поверх S3, а не GUI для rclone.

- S3-бакет = диск
- S4 Native Mode — основной режим хранения
- Compatibility Mode — опционально, позже

## 2. Фундаментальные принципы

### 2.1. No silent overwrite
Никогда не перезаписывать данные без ведома пользователя. Конфликт — это сохранённая версия, а не ошибка.

### 2.2. No immediate hard delete
Удаление = tombstone. Файл перемещается в корзину, а не уничтожается. Hard delete — только по явному действию пользователя или по истечении retention period.

### 2.3. Conflicts are preserved versions
Конфликт не уничтожает ни одну из версий. Пользователь всегда может восстановить любую конфликтную версию.

### 2.4. Rust Core обязателен
Вся критичная логика (синхронизация, S3, БД) — на Rust. UI — только отображение.

### 2.5. Tauri — desktop shell
Tauri v2 для desktop. Native mobile plugins для Android/iOS production.

### 2.6. Собственный sync engine
Не rclone, не s3cmd. Полный контроль над metadata, конфликтами, очередью, памятью.

### 2.7. file_id — стабильный идентификатор
Каждый файл/папка имеет UUID file_id, не зависящий от пути. rename = операция над file_id, не delete+create.

### 2.8. Conditional writes обязательны
Без If-Match/If-None-Match синхронизация не считается надёжной. S3 backend обязан поддерживать Level 2 совместимости.

## 3. Reliability Rules

1. Не перезаписывать локальный файл напрямую — сначала staging
2. Не удалять content blob сразу — tombstone + retention
3. Не считать upload успешным до metadata commit
4. Не считать commit успешным до successful conditional write
5. Не удалять local pending changes при ошибке
6. При любом сомнении — conflict, а не overwrite
7. Все transfer jobs persistent (SQLite)
8. Любой crash должен восстанавливаться

## 4. UX Principles

1. Никаких S3-терминов в основном UI
2. Ошибки — человеческим языком
3. Все destructive actions имеют undo/restore
4. Конфликт — не ошибка, а выбор версии
5. Приложение работает в фоне/трее на desktop
6. Закрытие окна = hide, не exit
7. Exit — только через tray menu
