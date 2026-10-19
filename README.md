# S4Drive

**Google Drive / Dropbox-подобный клиент для любого S3-совместимого хранилища.**
Один бакет = один диск. Windows, Linux, macOS, Android, iOS. Rust Core + Tauri UI.

> **Статус:** ранняя альфа (v0.2.0). Работают Rust Core, CLI и desktop-приложение; mobile в разработке.
> Не используйте S4Drive как единственную копию важных данных.

## Почему S4Drive

- **Не GUI для rclone.** Собственный sync engine на Rust: полный контроль над метаданными, очередью, конфликтами и памятью.
- **Ни одного потерянного байта.** Ничего не перезаписывается молча, удаление — это tombstone в корзине, конфликт — сохранённая версия, а не ошибка.
- **Rename без потери истории.** У каждого файла стабильный `file_id` (UUID v7), не зависящий от пути.
- **Переживает сбои.** Очередь передач хранится в SQLite и переживает crash, обрыв сети и sleep/wake.
- **Tray-first на десктопе.** Закрытие окна прячет его в трей, синхронизация продолжается; выход — только из меню трея.

## Как это работает

S4Drive синхронизирует не файлы, а **операции над версиями файлов**. Все служебные данные лежат в бакете под префиксом `.s4drive/`:

```
.s4drive/
├── content/blobs/   # неизменяемые content-addressable блобы (BLAKE3)
├── meta/ops/        # append-only журнал операций
├── meta/snapshots/  # периодические снапшоты дерева файлов
├── meta/heads/      # указатель на текущее состояние (CAS через If-Match)
├── devices/         # зарегистрированные устройства
└── trash/           # tombstones и удалённые версии
```

Коммит изменения атомарен:

1. Загрузить блоб (`If-None-Match: *`).
2. Записать операцию в `meta/ops/` (`If-None-Match: *`).
3. Обновить head (`If-Match: <etag>`). На `412` клиент перечитывает head, применяет чужие операции и повторяет попытку.

Параллельные правки одного файла дают sibling-ревизии и конфликтную копию `name (conflict from <Device> <YYYY-MM-DD HH:MM>).ext`. Last-writer-wins для содержимого запрещён.

## Требования к S3

Нужен бэкенд уровня **Level 2 (Safe sync)**: strong read-after-write consistency, conditional writes (`If-Match` / `If-None-Match`), multipart upload, Range GET и стабильная пагинация LIST. Подходят, например, AWS S3, Cloudflare R2 и MinIO. Проверить свой бэкенд можно командой `check` (21 тест, см. [матрицу совместимости](docs/spec/s3-compatibility-matrix.md)).

| Level | Что умеет бэкенд | Что умеет S4Drive |
|-------|------------------|-------------------|
| 1 | PUT/GET/HEAD/DELETE, LIST, Range | только просмотр |
| **2** | + conditional writes, strong consistency | **двусторонняя синхронизация** |
| 3 | + S3 Versioning | + страховка через версии бакета |
| 4 | + change feed, Object Lock | + near real-time sync |

## Быстрый старт

Нужен Rust stable. Для локальных экспериментов поднимите MinIO:

```bash
docker run -d --name s4drive-minio -p 9000:9000 \
  -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin \
  minio/minio server /data
docker exec s4drive-minio mc alias set local http://127.0.0.1:9000 minioadmin minioadmin
docker exec s4drive-minio mc mb local/s4drive-test
```

Проверьте бакет, инициализируйте его и запустите синхронизацию папки:

```bash
S3=(--endpoint http://127.0.0.1:9000 --bucket s4drive-test --access-key minioadmin --secret minioadmin)

cargo run -p s4drive-cli -- check "${S3[@]}"                             # уровень совместимости
cargo run -p s4drive-cli -- init-bucket "${S3[@]}"                       # создать .s4drive/
cargo run -p s4drive-cli -- sync start "${S3[@]}" --local-path ~/S4Drive
```

Desktop-приложение стартует в трее, а подключение к бакету настраивается в онбординге:

```bash
cargo run -p s4drive-tauri
```

На Linux для сборки Tauri нужны `libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev libsoup-3.0-dev libxdo-dev`.

## CLI

| Команда | Назначение |
|---------|------------|
| `check` | Проверить совместимость S3-бакета |
| `init-bucket` | Инициализировать `.s4drive/` в бакете |
| `sync start` / `sync status` | Синхронизировать локальную папку с бакетом / показать состояние |
| `metadata status\|tree\|ops` | Дескриптор бакета, дерево файлов, журнал операций |
| `metadata retire-device` | Исключить старое устройство из GC watermarks |
| `restore-bucket` | Восстановить файлы из копии `.s4drive/` без приложения и доступа к S3 |
| `desktop …` | Автозапуск, `.desktop`-файл, расширения Nautilus/Thunar |
| `fm …` | Команды для интеграции с файловым менеджером |
| `notes …` | Бэкап, проверка целостности и извлечение заметок (`backup`, `verify-backup`, `extract-backup`) |

Полный список флагов: `cargo run -p s4drive-cli -- --help`.

## Встроенные заметки (Notes)

В S4Drive встроен локальный редактор заметок с древовидной структурой папок:

- **Стандартный Markdown:** заметки сохраняются как обычные UTF-8 `.md` файлы в фиксированном каталоге `<sync_folder>/Notes`. Нет проприетарных баз, закрытых форматов или vendor lock-in. Файлы можно открывать и редактировать любым внешним редактором (VS Code, Obsidian, vim, Typora).
- **Безопасное автосохранение:** локальные правки сохраняются атомарно (debounce 500 мс / макс 2 с). При параллельных правках с других устройств или внешних редакторов обе версии сохраняются (конфликтная копия), тихой перезаписи нет.
- **Синхронизация через S4 Native Mode:** заметки синхронизируются через общий контур S4Drive с условными записями `If-Match` и транзакциями журнала метаданных.
- **Безопасный Preview:** предварительный просмотр Markdown парсится локально и фильтруется через строгий sanitizer (DOMPurify). Внешние сетевые ресурсы заблокированы. Для сверхбольших документов (>512 KiB) действует защитное ограничение рендеринга.
- **Автономный Backup:** экспорт всего дерева заметок в самодостаточный ZIP-архив с криптографическим манифестом (`manifest.json`, BLAKE3). Заметки можно извлечь из архива в любую папку без S4Drive и без доступа к облаку.

Подробности в [docs/spec/notes.md](docs/spec/notes.md) и [docs/notes-backup.md](docs/notes-backup.md).

Если удалить или переместить `S4Drive` или `S4Drive/Notes` вне приложения, синхронизация останавливается. Поверх всех вкладок появляется предупреждение, в трее — восклицательный знак. **Restore data from server** возвращает сохранённые на сервере данные; несинхронизированные правки можно вернуть из оригинала или Backup. **Удалить локально и с сервера** подтверждает удаление содержимого пропавшей папки на сервере и других устройствах. До выбора удаление не распространяется. Выбранное действие сохраняется при сбое и перезапуске. Успешный **Test Connection** проверяет подключение к хранилищу, а не наличие локальной папки.

На вкладке **Logs** можно выбрать детализацию, увидеть и скопировать путь к логу или открыть его папку. Desktop не использует `RUST_LOG`. Логи занимают суммарно до **150 МБ**: три файла по 50 МБ, старые перезаписываются при ротации. На Linux каталог — `$XDG_STATE_HOME/s4drive/logs`, по умолчанию:

```bash
tail -f ~/.local/state/s4drive/logs/s4drive.log
```

На Windows — `%LOCALAPPDATA%\S4Drive\logs`, на macOS — `~/Library/Logs/S4Drive`. Логи хранятся вне временного каталога, чтобы оставаться доступными после перезагрузки.

## Восстановление без приложения

Если сохранился префикс `.s4drive/`, файлы можно вернуть без GUI, локальной БД и даже без S3:

```bash
aws s3 sync s3://MY_BUCKET/.s4drive ./s4drive-backup
s4drive-cli restore-bucket --input ./s4drive-backup --output ./restored-files
```

Подробности в [docs/disaster-recovery.md](docs/disaster-recovery.md).

## Разработка

```
src-core/    Rust Core: S3-адаптер, metadata-протокол, sync и conflict engine, SQLite, watcher, очередь передач
src-cli/     CLI (s4drive-cli)
src-tauri/   Desktop-приложение на Tauri v2 (бинарь s4drive)
docs/spec/   Спецификации
scripts/     MinIO для тестов, сборка Linux-пакетов в Docker
```

Перед коммитом:

```bash
cargo fmt --check
cargo clippy --workspace -- -D warnings
cargo test --workspace

# e2e: два устройства, sync, rename, delete, конфликт, restore — против MinIO на localhost:9000
scripts/ci_e2e.sh
```

В production-коде запрещены `.unwrap()` и `unsafe`. Архитектурные решения (Rust Core, S4 Native Mode, `file_id`, CAS через conditional writes, tray-first) меняются только через ADR.

Сборка пакетов для Linux (deb, rpm, AppImage) в Docker; результат появится в `dist/linux/`:

```bash
docker run --rm -v "$PWD":/workspace -e HOST_UID=$(id -u) -e HOST_GID=$(id -g) \
  ubuntu:22.04 bash /workspace/scripts/build-linux-docker.sh
```

Релизы собирает только CI по тегу `vX.Y.Z`. Версия в теге должна совпадать с `Cargo.toml` и `src-tauri/tauri.conf.json`.

## Дорожная карта

| Версия | Содержание |
|--------|------------|
| v0.1 | Sync engine, `.s4drive/` протокол, persistent queue, конфликтные копии, CLI |
| v1.0 | Desktop (трей, файловый менеджер, версии, корзина, Sync Doctor) и mobile (browse, upload, offline) |
| v1.5 | Интеграции с ОС: Cloud Files API, File Provider, DocumentsProvider, selective sync |
| v2.0 | Шаринг, команды, E2EE, delta sync, web-приложение, FUSE |

Подробнее в [docs/spec/scope-v1.md](docs/spec/scope-v1.md).

## Документация

- [Техническая конституция](docs/spec/tech-constitution.md): неизменяемые принципы проекта
- [Product spec](docs/spec/product-spec.md) и [UX-принципы](docs/spec/ux-principles.md)
- [Семантика синхронизации](docs/spec/sync-semantics.md), [политика конфликтов](docs/spec/conflict-policy.md), [надёжность](docs/spec/reliability-spec.md)
- [Спецификация заметок (Notes)](docs/spec/notes.md) и [резервное копирование заметок](docs/notes-backup.md)
- [Матрица совместимости S3](docs/spec/s3-compatibility-matrix.md)

## Лицензия

См. [LICENSE](LICENSE).
