# S4Drive Notes Specification

> **Дата:** 9 октября 2026.
>
> **Статус:** принятый архитектурный контракт, полностью реализованный и подтверждённый полным набором проверок (Unit, Fault injection, Browser UI, MinIO multi-device S01–S12, Native Desktop E2E D01–D08, Backup/Disaster recovery).
>
> **Совместимость:** S4 Native Mode, metadata schema v2. Контракт не добавляет remote entry types, descriptor fields или operation types.

---

## 1. Назначение и границы

Notes — встроенный редактор обычных Markdown-файлов с папками и подпапками, безопасным локальным автосохранением, общей синхронизацией S4Drive и независимым локальным Backup. Раздел находится в основной навигации после Dashboard и перед Files.

Первый выпуск включает создание заметок и папок на любом уровне, rename/move/delete/Undo, Raw/Preview, небольшую Markdown toolbar, поиск по названиям, offline-работу, конфликтные копии, Sync и Backup. Состояния и ошибки должны различать локальное сохранение и доставку на другие устройства.

WYSIWYG, realtime collaboration/CRDT, tags, backlinks, AI, вложения через editor, Mermaid/LaTeX, full-text search, календарь и отдельные аккаунты не входят в этот контракт. Нет отдельного sync engine, облачного формата Notes, настроек Notes, выбора отдельного каталога, расписания Backup или Import wizard.

Основная поставка — desktop Windows/Linux/macOS. Адаптивность обязательна в существующем маленьком окне. Android/iOS picker, lifecycle и background sync нельзя объявлять готовыми без отдельных платформенных проверок.

## 2. Хранение и идентичность

### 2.1. Фиксированный каталог

```text
<sync_folder>/
├── Notes/
│   ├── Work/
│   │   └── Client A/
│   │       └── Project Overview.md
│   ├── Personal/
│   └── Quick Note.md
└── .s4drive/
    └── notes-state/          # локальные intents/recovery, не контент Notes
```

Фиксированный путь — `<sync_folder>/Notes`; root вычисляется только как `configured_sync_root.join("Notes")`. Имя `Notes` не локализуется. Общая sync folder задаётся существующими Settings; Notes следует её lifecycle без независимой настройки пути. Команды открытия workspace не принимают пользовательский notes_root.

Заметка — UTF-8 `.md`, читаемый обычным редактором. Внутри нет обязательного JSON, UUID, front matter или H1. Title — имя файла с скрытым `.md`, а не первая строка текста; title rename не меняет body. Preview не преобразует исходный Markdown обратно из HTML.

`Notes/...` — логический путь дерева. Запись вида `s3://bucket/Notes/...` не задаёт физическую раскладку объектов: на сервере остаются `.s4drive/content/blobs/...`, metadata tree, journal и conditional writes S4 Native Mode. Бакет — источник правды для закоммиченного общего состояния; до remote commit новые saved bytes и намерения живут на устройстве.

SQLite остаётся восстанавливаемым индексом/очередью. Локальные recovery manifest и projections не меняют metadata schema v2. Если реализация потребует нового remote payload/формата, сначала нужен отдельный архитектурный выбор, повышение `SUPPORTED_SCHEMA_VERSION`, дополнение `MetadataEngine::upgrade_schema` и проверка остановки старого клиента. Нельзя обходить этот gate скрытым marker или новым типом операции.

### 2.2. Workspace и стабильные ids

- `WorkspaceId` — локальный UUID подключения к нормализованным endpoint/bucket/prefix/sync-root, без credentials. Изменение этих параметров создаёт новую session generation; старая команда завершается до переключения либо возвращает `workspace_changed`.
- `FileId` и `RevisionId` — существующие UUID. Notes не создаёт второй note id для уже индексированного файла. Rename/move сохраняет id, каноническая запись содержит `parent_id` и имя; путь вычисляется.
- Неиндексированному `.md` id резервируется через единый register/intake; watcher/scan принимает его. Папки используют существующий `folder_node_id(parent,name,generation)` с проверкой занятых и удалённых поколений.
- Новые offline ids могут быть provisional до сверки remote placement. Уже сохранённый существующий id не меняется незаметно. Remote name collision разрешается общей conflict policy, а не перепривязкой к чужому id.
- Relative paths — locator/cache. Recovery intent может хранить `from_relative`/`to_relative` для локального replay; это не remote canonical path.
- Selection хранится по id. Browser storage допускает только theme/view/expanded ids/scroll per workspace, без body, titles и секретов в global localStorage.

## 3. Системный корень и filesystem boundary

### 3.1. Создание, защита и внешние изменения

Первое открытие создаёт root только при его отсутствии и отсутствии прежнего root id, tombstone или неразобранного recovery. Пустая Notes может создаваться offline. При подключении клиент учитывает remote Notes до заявления «папка пуста». Существующая обычная папка принимается с её содержимым и индексированными ids; клиент ничего не очищает и не создаёт `Notes (2)`.

Файл, symlink/reparse point или case-colliding `notes` на месте root означает blocked state. Чужие данные не заменяются. Для единственной существующей case-insensitive папки можно предложить явный case-only rename в `Notes`.

Сам root защищён от rename/move/delete в Notes, новых general mutation APIs и Files. У root доступны только Open folder и Backup. Потомки можно менять и перемещать внутри Notes; Notes UI не переносит их за пределы root. Защита — правило приложения, а не permissions ОС: внешняя программа и старый клиент могут физически изменить каталог.

При внешнем rename/delete или remote move root:

1. Приостановить Notes mutations, сохранить drafts и показать recovery state.
2. Не создавать автоматически пустую Notes и не интерпретировать исчезновение как разрешение массово удалить поддерево. Общий engine получает явное root-protection событие.
3. Если прежний root id найден в другом локальном месте, предложить `Restore folder location`: tracked rename в фиксированную Notes только при свободном destination. Не выполнять автоматический ответный move по кругу устройств.
4. При delete использовать tombstone/local trash restore. Новый пустой root допустим только явным действием после объяснения; прежний deleted id не переиспользуется.
5. На чистом устройстве без root cache нельзя определить, какая произвольная папка раньше была Notes. Показать дерево в Files; похожее имя не доказательство идентичности. Это граница schema-v2 конвенции.

Общие exclude patterns сохраняются. Если Notes/`.md` исключены пользователем, local save и Backup работают, статус LocalOnly объясняется `Excluded from sync`; exclusions не меняются молча.

### 3.2. Имена, размеры и безопасное открытие

Новые имена принимают Unicode, пробелы и emoji, нормализуются NFC. Проверка name collision case-insensitive на всех платформах; коллизия никогда не разрешается overwrite. Компонент, включая `.md`, ограничен 120 Unicode scalar values и 240 UTF-8 bytes; глубина — до 32 папок под Notes; relative path — до 1024 UTF-8 bytes. Более строгие ограничения ОС сообщаются без truncation данных.

Запрещены пустые/whitespace-only имена, `.`, `..`, `/`, `\`, NUL/control chars, `< > : " | ? *`, trailing dot/space, `.s4drive*` служебные имена, `.md` без stem. `CON/PRN/AUX/NUL/COM1…9/LPT1…9` запрещены без учёта регистра и расширения. Внешние существующие имена не переименовываются автоматически: при невозможности безопасного редактирования — read-only/error; Backup сохраняет их, если archive path безопасен.

Read/save/delete выполняются по id с повторной проверкой ancestry до текущего root. Переданный frontend path не authority. Root и потомки symlink/junction/reparse запрещены для чтения, writes и Backup. Защита от замены ancestor между проверкой и open требует безопасного handle-relative capability filesystem API поддерживаемой библиотеки без собственного unsafe; одного `canonicalize(); starts_with()` недостаточно.

На Windows служебный hidden/system-файл `.s4drive-anchor` может находиться внутри Notes-каталога. Он не является заметкой, не показывается в дереве и удерживается только для защиты publication parent от in-place reparse conversion; пользовательские данные и Markdown bytes он не меняет.

Объекты с несколькими hardlinks не редактируются там, где это можно определить. Independent copy возможна только явным действием пользователя; клиент не меняет внешний файл через общий inode. Case-only rename сохраняет id, использует уникальное temporary name и durable intent, проверяет столкновения с другим объектом.

Raw edit ограничен 2 MiB UTF-8, Preview — 512 KiB. Большие файлы не обрезаются: read-only message/Open folder, обычная sync и Backup сохраняются. Invalid UTF-8 не заменяется U+FFFD с последующим save.

## 4. UI и навигация

### 4.1. Общие действия

Общая верхняя toolbar доступна независимо от selection: Notes, New note, New folder, Sync, Backup. Только New note — primary filled; остальные secondary/quiet. В compact режиме New note остаётся видимым, текстовые New folder/Sync/Backup находятся в More, sync status виден рядом. Нет `Sync to S3`, дубликата Manual Backup или декоративных действий без поведения.

Используются существующие light/dark themes, бирюзовый accent, radius 8 px и spacing 4/8/12/16/24. Общая sidebar не дублируется внутри Notes. Корень дерева ровно Notes, без фиктивного Notebooks. Editor один, status — компактная строка, без блока больших Save-кнопок.

### 4.2. Selection и раскладка

Tree selection и открытая note — отдельные состояния. Видимость middle list нельзя вычислять только из `activeNoteId`.

| Действие | Контекст | List и editor |
|---|---|---|
| Первый вход | Root folder | Дети корня + empty prompt; note автоматически не создаётся |
| Стрелка папки | Selection прежний | Только expand/collapse |
| Название папки | `folder(id)` | Непосредственные children; prompt либо последняя живая note этой папки |
| Note в tree или search | `note(id)` | Middle list скрыт, width 0, отсутствует в tab/accessibility tree; editor расширяется |
| Note в folder list | `folder(parentId)` | Folder context сохраняется; на wide list остаётся рядом с editor |
| Folder в list/breadcrumb | `folder(id)` | Дети этой папки; tree/breadcrumb синхронны; прежний buffer flush-ится |
| Back из editor | Запомненный folder/tree context | Восстановить исходные scroll и focus |
| New note/New folder | Выбранная folder либо parent выбранной note | Note открывается; новая folder выбирается |
| Rename активной note | Прежний id/context | Body, cursor, undo, Raw/Preview сохраняются |
| Delete selection | Ближайший живой parent | Его children, без случайного перехода в чужую note |

Поздний read A не меняет title/body B. Ответы привязаны к workspace id, note id и request generation.

Ширина измеряется `ResizeObserver` по Notes content после вычета основной sidebar:

| Доступная ширина | Layout |
|---|---|
| ≥ 960 px | Tree 200–240 px, folder list 220–280 px, editor min 440 px; note context — tree + editor |
| 640–959 px | Tree 180–220 px + main: folder list или editor с Back, без третьей колонки |
| < 640 px | Один main screen; Folders открывает tree drawer; выбор закрывает drawer; folder/editor push/back |

Окно при входе автоматически не увеличивается. Приёмка включает 400×500, 500×600, 1024×768, 1280×800 и 1440×900, а также границы 640/960 px доступной области. Panels имеют отдельный vertical scroll и `min-width/min-height: 0`. Horizontal scroll допустим только внутри code/table, не у всей страницы. На 200% zoom toolbar переносится/сворачивается без обрезания input и действий.

### 4.3. Tree, list, search и CRUD

Tree содержит folders и notes, с различимыми selected/hover/focus и отдельной hit area expand. List показывает имя, plain-text excerpt до 100 Unicode-символов, время и один status indicator; точное локальное время доступно в tooltip. Natural name sort, folders first, tie-break по id; autosave не пересортировывает строки по modified.

Search `Search note names…` ищет только названия note/folder, debounce 150 мс; результаты показывают breadcrumb. Clear возвращает прежний selection/scroll. Tree lazy-load по folder, list pages по 200; большие списки виртуализированы. Bodies не читаются для дерева; excerpt кэшируется после read/save и может отсутствовать.

More у потомка содержит Rename, Move to…, Delete; у note также Version history. Move to… использует доступный folder picker/breadcrumb и запрещает self/descendant. Drag-and-drop не требуется.

New note атомарно создаёт пустой `Untitled.md`, затем `Untitled (2).md` и т. д.; пустая заметка уже локальный файл. Один request id при double-click/retry не создаёт два файла. Title `Untitled` выделяется для rename, затем focus переходит в body. New folder предлагает `New folder`; Cancel ничего не создаёт, duplicate name даёт inline error. Title commit — Enter/blur, Escape возвращает старое имя, пустое имя не сохраняется.

Delete сначала flush-ит active buffer; save failure блокирует удаление. Один короткий dialog показывает имя note либо количество descendant notes/other files folder, сообщает о распространении удаления на другие устройства. Cancel выбран по умолчанию. Cancel возвращает focus/text. Toast Undo доступен минимум 10 секунд; Recently deleted — постоянное действие меню Notes.

### 4.4. Raw и read-only Preview

Новая note открывается в Raw. Видимые Raw/Preview имеют accessible name `Note view` и корректную pressed/tab semantics. Raw — textarea, monospace 14–15 px, line-height 1.55–1.65, platform spellcheck. Preview — обычный font 15–16 px, line-height 1.65, до 80ch, monospace code.

Toolbar Bold/Italic/Heading/Bulleted list/Numbered list/Task list/Link/Code вставляет Markdown вокруг selection или в начало строк, возвращает focus/selection и составляет одно Undo-действие. Link вставляет template без URL fetch. Tab остаётся обычной навигацией. Во время IME composition toolbar не изменяет selection; checkpoint после завершения composition, без половины surrogate pair.

Поддерживаются H1–H6, paragraphs, emphasis, nested lists, blockquotes, fenced/inline code, thematic breaks, links, tables, task lists. Task checkboxes в Preview только read-only; узкое проверенное renderer rule не исполняет HTML. Runtime code highlighter не нужен.

Переключение не меняет bytes/title/dirty state/undo. Raw и Preview хранят отдельный scroll. Preview показывает текущий unsaved buffer с прежним Saving status; generation guard отбрасывает устаревший renderer result. Не обещается точное построчное совпадение двух представлений.

### 4.5. Безопасность Markdown

Локально поставляемый markdown-it используется с `html: false`, `linkify: false`, `typographer: false`; output проходит DOMPurify с узким allowlist. Vendor versions/licenses/checksums фиксируются в поставке; runtime CDN/новый frontend bundler не вводятся.

HTML в source показывается текстом. Запрещены script/style/iframe/object/embed, event attributes, inline style, SVG/MathML, пользовательские id/name и DOM clobbering. Допустимы p/br/hr/h1–h6/em/strong/s/blockquote/ul/ol/li/pre/code/a/table/thead/tbody/tr/th/td. Attributes: href/title у a, start у ol, безопасные internal classes code/task, scope у th. Task representation disabled и создаётся renderer, а не произвольным HTML note. Titles/names выводятся как текст, не innerHTML.

После URL decoding/normalization запрещены `javascript:`, `data:`, `file:`, `tauri:`, `asset:`, custom schemes и protocol-relative URLs. http/https открываются только после пользовательского действия во внешнем browser через повторную Rust allowlist validation. `#heading` обрабатывается внутри Preview. Images — alt placeholder, без сети и произвольного local file access, исходная syntax сохраняется. CSP не расширяется wildcard/unsafe-eval/network img-src. Malicious note не должна вызывать IPC, сеть или webview navigation.

### 4.6. Доступность и ошибки

Tree: Up/Down — visible rows, Right — expand/first child, Left — collapse/parent, Home/End — first/last. Enter opens, F2 renames, Delete действует только при focus tree/list. ContextMenu/Shift+F10 открывает More; Escape закрывает dialog/drawer и возвращает focus.

Editor: Ctrl/Cmd+S local save, Ctrl/Cmd+B/I Markdown, Ctrl/Cmd+Z/redo обычный undo. Ctrl+N глобально не перехватывается; Ctrl/Cmd+Alt+N допускается только внутри Notes. Targets минимум 32 px desktop/44 px compact-touch; focus минимум 2 px, текстовый контраст ≥4.5:1, цвет не единственный status indicator. Dialog имеет имя/focus trap. Status/toast используют `aria-live=polite` без озвучивания каждого input; `prefers-reduced-motion` отключает переходы. Truncated names имеют доступное полное имя.

Основной UI использует английские строки и человеческие ошибки. Диагностика не содержит Markdown body, secrets, raw SQL или full text; S3/ETag/HTTP 412 не показываются в Notes.

| Состояние | Текст / действие |
|---|---|
| Пустой root | `Your notes, in one place` / `Create your first note. Changes are saved automatically.` / New note |
| Пустая folder | `This folder is empty` / New note, New folder |
| Dirty buffer | `Saving…` |
| Durable local ACK | `Saved locally` |
| Offline/pending | `Saved locally · Waiting to sync` |
| Remote commit текущего token | `Synced` |
| Paused | `Saved locally · Sync paused` / Resume and sync |
| Account не подключён | `Saved locally · Connect storage to sync` / существующий Account |
| Save failed | `Couldn’t save this note. Your text is still open.` / persistent banner, Retry, Copy text |
| Conflict | `Another version was saved. Both copies are available.` / Review versions |
| Нет bytes offline | `This note isn’t available offline yet.` / Retry, edit disabled, не пустой editor |
| Root unavailable | `The Notes folder is unavailable.` / Open location / Restore folder location если возможно |
| Backup success | `Backup saved` / путь, Show in folder |
| Backup failure | `Couldn’t create the backup. Your notes are unchanged.` / Retry |
| Backup cancel | Нет error toast или success |

## 5. Core и IPC: типизированный контракт

### 5.1. Ответственность и DTO

Vanilla JS управляет представлением, вводом и безопасным Preview. Tauri bridge отвечает за native picker, URL opening, lifecycle и events. `NotesService` в Rust core управляет файловым repository, durable mutation intake/recovery и Backup, без Tauri/DOM types. Sync выполняет существующий общий engine; webview не получает произвольного filesystem write или shell execution.

Ниже описаны новые ожидаемые API, а не уже существующие команды. IPC fields snake_case, UUID — строки, timestamps — RFC3339 UTC. Paths относительные с `/`; исключение — выбранный пользователем output archive path в результате Backup.

| Тип | Контракт полей |
|---|---|
| `SaveToken` | `generation: u64`, `content_hash: String` |
| `NotesEntry` | `id: FileId`, `parent_id: Option<FileId>`, `kind: note/folder`, `name`, `display_name`, `relative_path`, `updated_at`, `sync_state`, `is_root: bool`, `available_locally: bool` |
| `NoteSyncState` | `local_only/pending/syncing/synced/paused/conflict/error` |
| `NoteDocument` | `entry: NotesEntry`, `body: String`, `token: SaveToken` |
| `SaveNoteRequest` | `workspace_id: WorkspaceId`, `request_id: UUID`, `note_id: FileId`, `expected: SaveToken`, `body: String` |
| `SaveNoteResult` | `note_id`, `request_id`, `token`, `saved_at`, `sync_state` |
| `NotesWorkspace` | `workspace_id`, `root: Option<NotesEntry>`, `availability: ready/missing/blocked`, `session_generation: u64`; ready требует root, blocked без известного id допускает None |
| `NotesPage` | `entries: Vec<NotesEntry>`, `next_cursor: Option<String>`, `tree_generation: u64` |
| `MutationResult` | `request_id`, `entry: NotesEntry`, `undo_id: Option<UUID>` |
| `DeletedEntry` | `undo_id`, `entry`, `deleted_at`, `expires_at`, `can_restore: bool` |
| `DeletedEntriesPage` | `entries: Vec<DeletedEntry>`, `next_cursor: Option<String>` |
| `SyncTicket` | `job_id: UUID`, `workspace_id`, `state: queued/running` |
| `BackupTicket` | `job_id: UUID`, `workspace_id` |
| `BackupResult` | `job_id`, `archive_path`, `files/directories/bytes: u64`, `created_at`, `includes_pending: bool` |
| `NotesErrorDto` | `code`, `message`, `retryable: bool`, `note_id: Option<FileId>` |
| `NotesEvent` | `workspace_id`, `sequence: u64`, `kind`, `entry_ids: Vec<FileId>`, `job_id: Option<UUID>`; без body |

SaveToken описывает локальную materialization, не remote revision. Hash обнаруживает внешние writes, generation — out-of-order/ABA внутри процесса. После restart generation восстанавливается из journal/index; при потере index edit sessions инвалидируются, documents перечитываются. Request id идемпотентен: повтор с прежним payload возвращает прежний результат, с другим payload — ошибка.

IPC body — исходная UTF-8 строка с реальными EOL/BOM. Textarea adapter хранит BOM/EOL отдельно и восстанавливает при save. Неизменённый buffer не записывается. Mixed LF/CRLF: edit mapping сохраняет окончания неизменённых строк; новые получают prevailing EOL, при равенстве LF. Hash вычисляется от реально сохранённых bytes.

Stable error codes: `invalid_name`, `name_exists`, `protected_root`, `outside_notes`, `symlink_not_allowed`, `stale_write`, `not_found`, `not_available_offline`, `save_failed`, `backup_failed`, `workspace_changed`, `sync_paused`, `cancelled`. Core использует typed `CoreError` cases и match mapping; UI не разбирает message string для логики.

Локальное чтение дополнительно возвращает `note_too_large`, `invalid_utf8` и `stale_cursor`. Последний означает, что список нужно запросить с первой страницы; страницы из разных поколений дерева не объединяются. Cursor привязан также к workspace, session и конкретной папке/поисковому запросу. Core ограничивает размер страницы диапазоном 1–200.

Локальные изменения дополнительно возвращают `request_mismatch` (request id уже использован с другим payload), `workspace_busy` (другой процесс удерживает root lock; `retryable=true`) и `mutation_unavailable` (безопасная публикация на платформе пока не реализована; `retryable=false`). `stale_write` содержит `note_id`; повторять сохранение с тем же expected token нельзя, нужно перечитать файл и сохранить пользовательский buffer для разрешения конфликта.

Текущая реализация сравнивает имена через NFC → Unicode lowercase → NFC. Existing names сохраняются в исходном виде; `note_filename` создаёт NFC-имя с единственным `.md` и валидным stem. Локальный root binding и файловая identity живут только в индексе/checkpoints. Проверенный directory handle сохраняется до конца read/list traversal; повторное открытие root по имени после проверки не допускается.

### 5.2. Команды

Core methods имеют те же имена без `notes_`, возвращают `CoreResult<T>`; local I/O/coordinator operations async. Sync request — scheduler bridge, а не network method локального repository.

| IPC command | Вход | Результат / правило |
|---|---|---|
| `notes_open_workspace` | Без path | `NotesWorkspace` из текущего AppState |
| `notes_list_children` | workspace_id, parent_id, cursor?, limit=200 | `NotesPage`; parent внутри Notes |
| `notes_search_names` | workspace_id, query, cursor?, limit=200 | `NotesPage`; query до 256 chars, stable sort |
| `notes_read` | workspace_id, note_id | `NoteDocument`; UTF-8/size/availability validation |
| `notes_create_note` | workspace_id, request_id, parent_id | `MutationResult`; atomic default name |
| `notes_create_folder` | workspace_id, request_id, parent_id, name | `MutationResult` |
| `notes_save` | `SaveNoteRequest` | `SaveNoteResult`; только durable local ACK |
| `notes_rename` | workspace_id, request_id, id, new_name | `MutationResult`; `.md` в UI не вводится |
| `notes_move` | workspace_id, request_id, id, destination_parent_id | `MutationResult`; no cycle/overwrite |
| `notes_delete` | workspace_id, request_id, id | `MutationResult` с undo_id |
| `notes_list_deleted` | workspace_id, cursor?, limit=200 | `DeletedEntriesPage`; доступные on-device manifests |
| `notes_restore` | workspace_id, request_id, undo_id, name_override? | `MutationResult`; parent восстановить первым либо предложить Notes root |
| `notes_sync` | workspace_id | `SyncTicket`; UI flush перед request |
| `notes_backup` | workspace_id | `Option<BackupTicket>`; None при picker cancel |
| `notes_cancel_backup` | workspace_id, job_id | `()`; до final publish |
| `notes_job_status` | workspace_id, job_id | Tagged `SyncJobStatus`/`BackupJobStatus` |
| `notes_open_folder` | workspace_id, id | `()`; только реальная папка root/потомок |
| `notes_restore_root_location` | workspace_id, request_id | `NotesWorkspace`; detected root tracked rename в Notes, без произвольного destination |
| `notes_open_link` | URL | `()`; http/https с Rust validation |
| `notes_flush_ack` | close_request_id, session_generation, success | `()`; lifecycle handshake |

Job status: `state: queued/running/succeeded/failed/cancelled`, `progress_done/total: Option<u64>`, `result: Option<BackupResult>` либо Sync `outcome: synced/pending/needs_attention`, `error: Option<NotesErrorDto>`. Завершённые tickets живут в ограниченном локальном session cache; после restart UI читает files state заново.

`notes:changed` и `notes:job-progress` сообщают о необходимости refresh, без body. Events могут потеряться; tab return/window focus refresh-ит данные, jobs доступны через polling статуса. Старые workspace/session events игнорируются.

Core Backup API: `NotesService::backup_to(destination: &Path, cancellation: CancellationToken) -> CoreResult<BackupResult>`. CancellationToken — core wrapper над `Arc<AtomicBool>` с `cancel/is_cancelled`, без Tauri types. Picker выбирает destination, Rust валидирует и передаёт core. Произвольный destination из JS не принимается.

## 6. Автосохранение, recovery и общая синхронизация

### 6.1. Durable local save

Autosave запускается после 500 мс без ввода; при непрерывном вводе checkpoint каждые 2 с. Ctrl/Cmd+S немедленно инициирует local save. Эти интервалы ограничивают ещё не подтверждённый input: power loss/kill может потерять текст после последнего durable ACK. Нельзя обещать durability каждого нажатия.

Для каждой note один in-flight save. Input обновляет latest buffer; после ACK отправляется следующий snapshot, если он отличается. Session принадлежит конкретной note/workspace; навигация не подменяет её identity. ACK старой sequence не очищает dirty новой; таймер/flush не теряет edits на завершении drain. Failure сохраняет старую saved sequence, buffer и request id для безопасного retry после потерянного ACK.

Порядок save:

1. Проверить workspace/session, id/root boundary и request id.
2. Взять общий per-entry coordinator lock; rename/delete/download/restore участвуют в нём.
3. Сравнить актуальные bytes/hash/generation с expected; несовпадение возвращает `stale_write` без overwrite, editor buffer остаётся доступен.
4. Записать и fsync локальные intent/recovery payload в `.s4drive/notes-state/`: version 1, workspace/request/file ids, expected/target hash, kind/stage, без secrets.
5. Создать unique staging на том же filesystem exclusive create; write/flush/sync_all. Старый файл остаётся до публикации.
6. Перед publish снова проверить fingerprint/hash и сохранить recovery preimage. Mutex не блокирует внешние программы; preimage защищает наблюдённую версию в узкой гонке перед rename.
7. Атомарно заменить через безопасный filesystem API, проверить final hash, синхронизировать parent directory где поддерживается. Sharing violation оставляет old file и retry/error. Destination никогда не удаляется ради rename.
8. В короткой index transaction увеличить generation, зарегистрировать pending mutation/transfer с прежним FileId/base revision. Новый hash не считается remotely synced.
9. Отметить journal published/indexed, вернуть ACK с token. Preimage очищается только после проверки, что это не непризнанная внешняя правка, и завершения recovery/commit. Неразобранные preimages не удаляются по возрасту.

Гарантия для сторонних редакторов — сохранение всех наблюдённых версий и конфликтная копия при неопределённости. Клиент не обещает линейный CAS с любой программой, игнорирующей файловые блокировки. Тестируется управляемая внешняя запись между последней проверкой и publish.

Recovery выполняется до scan/watcher. Final hash=target — восстановить index/queue с прежним id; hash=expected — завершить publish после проверки intent; ни тот ни другой — оставить внешний файл, сохранить target payload conflict copy. Replay идемпотентен. Filesystem/SQLite не имеют общей транзакции; согласованность даёт journal/replay.

### 6.2. Mutation intake, удаление и восстановление

Create/rename/move/delete/restore сначала фиксируют intent, затем filesystem, затем очередь. Watcher — дополнительное наблюдение, дедупликация по FileId/operation/request/hashes, без временного «игнорировать события 2 секунды». Scan после restart не выдаёт новый id уже применённой mutation. Temporary/recovery/trash исключены из upload.

Folder placement/delete/restore — одна semantic operation над root id, descendants не превращаются в N delete/create. Lock order во всех writers: workspace lifecycle guard → topology barrier (read content, write topology/capture) → entry locks по sorted FileId → короткая DB transaction. Network await и ZIP compression вне DB lock; release reverse order.

Delete перемещает subtree в существующую local trash с прежними ids и undo manifest, без `remove_dir_all` пользовательского дерева. Manifest до перемещения сохраняет полные FileEntry descendants, parent/name, ids, revision/content references и local bytes locator, независимо от SQLite. Remote delete, наблюдаемая на этом устройстве, делает такой же manifest до удаления materialization. Pending upload отменяется/помечается по id; актуальный intent повторно проверяется до commit, чтобы delete не воскресал.

До первого remote commit create+delete можно свернуть в очереди, сохраняя local Undo/recovery. После commit удаление — tombstone. Manifest+bytes hydrated/cached Notes хранятся не меньше показанного retention, сейчас 90 дней; unresolved drafts не удаляются cleanup.

Recently deleted drawer подписан `Recently deleted on this device`: показываются только удаления с полным on-device recovery manifest. Чистое устройство не может получить полное deleted folder tree только из текущего tombstone blob references. При отсутствии manifest — честная недоступность restore и существующие recovery/Backup средства, без выдуманных имён по hashes. Полная облачная корзина потребовала бы отдельного schema решения.

Undo возвращает прежний id; занятое имя требует свободного name, чужой файл не заменяется. Restore pipeline проверяет tombstone/current head, готовит local subtree из manifest, восстанавливает entries conditional writes и коммитит одну semantic Restore root. Потомки становятся достижимы после успешного root restore; failure оставляет replayable intent. Remote обработчик reconciles/downloads subtree на B. Существующая version API не считается автоматически готовой folder undelete; расширение remote semantics требует schema gate.

Локальные projections: durable request/stage/journal references, per-file hash/generation и root id/availability. Тело note и единственный restore manifest не хранятся только в SQLite; projections восстанавливаются из journal. Committed data восстановимы из бакета, uncommitted local intent — из journal. После потери всего устройства доступно только ранее committed либо Backup содержимое.

### 6.3. Sync и статусы поколений

Перед Sync UI ждёт durable save всех Notes sessions; failure сохраняет focus/banner и не показывает Synced. Кнопка запрашивает общий S4Drive sync pass, tooltip `Sync changes on this device`; это не изолированный Notes-only обмен. При pause действие называется `Resume and sync`, pause не снимается скрытно. Offline возвращает понятный pending/error, local editing продолжается.

Общий scheduler объединяет manual/auto requests, не создаёт параллельные независимые run_once/watchers/DB. Running ticket показывает spinner и блокирует повторный click, но editing остаётся доступным. Ticket фиксирует target generations при запуске; интерактивный deadline 120 с, затем Pending с фоновым продолжением/backoff, без вечного ожидания пустой очереди при постоянном вводе.

Success/Synced означает, что target local generation закоммичена, её pending/conflict jobs завершены и доступный remote head применён. Старый ACK не делает новую generation Synced. Upload использует immutable content snapshot либо повторную hash validation. Сам blob без metadata commit не означает успех: immutable writes используют `If-None-Match: *`, journal head CAS — `If-Match`.

Download/restore повторно проверяют local token перед publish. Dirty buffer не затирается: external update pending, buffer сначала сохраняется recovery, затем общий conflict workflow. Remote rename обновляет breadcrumb/title без потери body/selection. Remote delete при dirty buffer и folder delete с новыми local children сохраняют изменения конфликтной копией. Общая conflict policy хранит обе версии (копия `name (conflict from …)`); собственный LWW/merge Notes не вводится. Existing core text auto-merge возможен только по его правилам.

### 6.4. Навигация и tray lifecycle

Note/route/workspace switch, hide/blur и начало Backup инициируют flush. Buffer живёт до успеха; failure блокирует именно уход с unsaved session и показывает Retry/Stay/Copy text. После hide процесс продолжает flush. Close остаётся hide-to-tray, Exit — только tray menu; `beforeunload` не единственная lifecycle защита.

Tray Exit: Rust close_request_id → UI flush → `notes_flush_ack` → Rust pending writes → exit. За 5 с без успеха exit отменяется и окно показывает ошибку. При отсутствии webview/sessions ждать только backend writes; повторные Exit coalesce. Kill/power loss handshake не исполняет, гарантия ограничена durable ACK/recovery.

Смена Account/общей sync folder сначала flush-ит прежний workspace, затем создаёт новый service. Старые async responses/events фильтруются session generation. Заметки между разными бакетами автоматически не переносятся.

## 7. Независимый локальный Backup

### 7.1. Полнота и destination

Backup: flush sessions → native Save dialog с `S4Drive-Notes-YYYYMMDD-HHmmss.zip` → snapshot/progress → verification → atomic final publish → `Backup saved` и Show in folder. Picker cancel не создаёт архив. Backup не меняет sync status и не означает delivery на другое устройство.

Destination находится за пределами **всей sync folder**, а также recovery/trash/config paths. Это предотвращает отправку копии в то же хранилище и цикл. Существующий destination никогда не перезаписывается: нужен новый filename, без overwrite confirmation. Конкурентное появление destination перед publish — name conflict, прежний архив сохранён.

Включаются все обычные files/directories под Notes: вложенные `.md`, non-md, hidden user files, пустые folders, сохранённые pending/offline bytes. История, local trash, exact service staging names, credentials, SQL и recovery internals исключены. Symlink/reparse/socket/device/небезопасный path — failure всего Backup с безопасными relative names; недопустим success с молчаливым пропуском. UI ограничивает длинный список, полный report доступен локально.

Индексированные remote descendants участвуют в проверке полноты, не только локальный `read_dir`. Cloud-only bytes online сначала hydrate через общий engine; offline — отказ с количеством недоступных files. Неполный архив не называется Backup всех Notes. ZIP не заявляется зашифрованным: имена и содержимое — пользовательские данные.

### 7.2. Формат v1

```text
S4Drive-Notes-20261005-183000.zip
├── manifest.json
└── Notes/
    ├── Work/
    │   └── Meeting Notes.md
    ├── Personal/             # сохраняется даже пустой
    └── Quick Note.md
```

Manifest содержит `format: "s4drive-notes-backup"`, `version: 1`, `created_at`, `app_version`, `includes_pending` и `entries`. Каждый entry имеет relative `path`, `kind: file/directory`, `size`; file также BLAKE3 `blake3`. Manifest не содержит remote endpoints/bucket, credentials/device secrets, абсолютные source paths, content duplicates, journal или SQLite.

Лимиты writer/verifier/extractor одинаковы: максимум **100 000 entries**, **20 GiB** total uncompressed bytes, **8 GiB** один file. Считаются declared и фактические streamed bytes; чтение прекращается при превышении, ZIP headers не считаются достаточной защитой. Превышение — понятная ошибка до final publish, с возможностью обычного копирования каталога.

### 7.3. Snapshot и публикация

1. Взять Notes mutation barrier, перечислить topology/versions, скопировать bytes через safe handles во временный snapshot вне Notes. Посчитать hash, проверить stat/hash после копии.
2. Повторно проверить topology/fingerprints всего дерева. При внешних изменениях повторить capture до 3 раз, затем `Notes changed during backup. Try again.`. Snapshot не объявляется строгой filesystem transaction со сторонними writers.
3. Освободить barrier после capture; topology actions имеют короткий busy state только во время capture. Архив строится из snapshot streaming без всего дерева в RAM, editing может продолжаться после capture.
4. Записать temporary ZIP в destination directory, добавить manifest, close/fsync.
5. Заново открыть ZIP, сверить полный paths list, sizes, BLAKE3 каждого decompressed file и empty directories. ZIP CRC не заменяет эту verification.
6. Атомарно опубликовать final без overwrite. Race target появления возвращает conflict; cleanup удаляет только свои temp files, не чужой destination.

Cancel/disk full/failure до публикации не оставляют final archive; Notes и прежние Backup сохраняются. Отмена относится к job до final publication.

### 7.4. Проверяемое восстановление

Обычные Markdown/files можно читать из ZIP без S4Drive. Восстановление первого выпуска: распаковать в **новую пустую папку**, проверить manifest hashes, открыть `.md`. Для возврата в S4Drive нужные files копируются через Files/файловый менеджер с безопасным новым именем; автоматического overwrite/Import поверх Notes нет.

Ожидаемые CLI/test helper contracts:

```text
notes verify-backup --archive PATH
notes extract-backup --archive PATH --output EMPTY_DIR
```

Verifier ничего не пишет. Extractor пишет только в empty output, не принимает absolute paths, `..`, backslash traversal, drive letters, symlink entries, duplicate paths и case collisions. Проверяет manifest/path set, declared sizes/total до распаковки, фактические streamed sizes/hashes во время неё. Entries сначала идут в собственный staging внутри нового output directory, публикуются только после проверки. Failure удаляет лишь свой staging, никогда не соседние files. Helpers не становятся произвольной shell-командой webview.

## 8. Требуемые проверки и критерии приёмки

Ни один пункт ниже не означает уже пройденный тест. Любое изменение поведения требует test, bugfix — regression test. Unit/local integration работают без сети с fake `S3ObjectStore`; MinIO-dependent Rust tests помечаются `#[ignore = "requires MinIO"]`. System scripts используют `S4_E2E_*`, уникальные buckets, отдельные state/sync roots A/B и cleanup. Secrets не hardcode-ятся. Проверяется logical tree, ids, hashes и committed ops, а не лишь наличие blobs или timestamps.

| Уровень | Обязательные сценарии |
|---|---|
| Rust unit/local integration | First run/existing root/idempotent open; Unicode/name/byte/depth limits; traversal/Notes2/symlink/ancestor swap; request-id retries; stale writes и ABA; save-vs-rename/move/delete; folder operation count/id stability/cycles; disk full/permissions/sharing violation/parent sync failure; LF/CRLF/mixed EOL/BOM/Unicode/empty round-trip; invalid UTF-8/size read-only; delete/Undo after restart/name collision; index rebuild и journal recovery; root external rename/delete; exclusions |
| Deterministic fault injection | Intent fsync, staging fsync, preimage, before/after atomic publish, index commit, lost ACK; recovery повторяется без duplicate ids или partial destination; controlled external writer между final validation и rename сохраняет версии; pending upload не воскресает после delete |
| Autosave JS unit | Fake clock и deferred promises, без sleeps: 500 ms/2 s, один in-flight, старый ACK/новый edit, timer-during-flush, retry lost ACK, IME, workspace generation; реальные assertions без пустого «успешного» harness |
| Browser UI с mock IPC | Tree note скрывает list из layout/a11y; list note сохраняет wide context; widths 640/960 и маленькие окна; drawer/back focus/scroll; stale read A/B; actions/parents во всех contexts; save error остаётся persistent с buffer; Raw/Preview/toolbar undo; security corpus без IPC/network/clobbering; local-vs-synced generations; delete Cancel/Undo; keyboard-only/zoom/themes/reduced motion; virtualized 10 000 names |
| Backup local integration | Nested/non-md/hidden/empty folders/pending bytes; exact hashes после verify/extract; unsafe paths/symlink/special file failure; unavailable bytes; capture writer race; cancel/disk full; inside-whole-sync-root destination; existing/concurrent target no-clobber; hash mismatch/bomb/duplicate/case/traversal archives; empty output enforcement и отсутствие writes вне него |
| MinIO two-device A/B и clean C | Create/nested/empty/offline restart; rename/move subtree с прежними ids и одной semantic op; parallel edits в обоих reconnect orders; delete-vs-edit/new-child conflicts; blocked upload с новым save; download-vs-local save; blob success/CAS failure/lost reply без premature Synced; committed delete/Undo/restart/name collision, folder restore реально на B; rebuild C без SQLite/disaster restore; offline Backup/extract; root external change и exclusions при продолжающемся общем sync |
| Настоящий desktop E2E | Реальный Tauri WebView → IPC → core → local bytes; Sync → CAS → B → UI; layout/input/themes; close-hide/tray reopen/Exit/relaunch; kill после durable ACK; native picker/ZIP; external editor с dirty buffer; CRUD/Undo/root protection. Выполненные ОС перечисляются явно; browser mock не выдаётся за native E2E |

Гонки и crash boundaries системных сценариев достигаются не паузами, а test checkpoints (`src-core/src/test_hooks.rs`), которые компилируются только с feature `notes-test-hooks` (CLI) или `notes-e2e` (desktop) и выбираются через `S4_E2E_GATE_DIR`/`S4_E2E_CHECKPOINT`/`S4_E2E_FILE_ID`/`S4_E2E_REQUEST_ID`: процесс пишет `ready.json` и ждёт `release` с ограниченным timeout. Checkpoints: `upload_after_blob_before_generation_recheck`, `download_after_stage_before_publish`, `remote_after_op_before_head_cas`, `remote_after_head_cas_before_receipt`, `intake_after_commit_before_local_marker` и локальные стадии `MutationCheckpoint` (`AfterStage`, `AfterPublish`, `BeforeIndex`, `BeforeAck`). Сценарий, не достигший своего checkpoint, падает. Release без этих features не содержит ни кода hooks, ни команды `notes test-hooks`. Desktop-сборка с `notes-e2e` дополнительно умеет: сообщать pid и boot nonce процесса, закрывать окно и запускать Exit тем же путём, что и пункт меню трея, отказывать мутации с `StorageFull` и один раз терять exit ACK (`src-tauri/src/notes_e2e.rs`). Ничего из этого не сохраняет, не подтверждает и не завершает приложение само.

Mock picker допустим только в test build для deterministic destination и не заменяет ручной picker check каждой desktop ОС. Fixtures/mock IPC не включаются в release; production без Tauri показывает unsupported environment, а не fake success. Security tests проверяют HTML/attribute/encoded URL corpus и отсутствие shell/network/filesystem полномочий от Markdown.

Acceptance performance targets для обычного desktop/SSD с 10 000 notes × 2 KiB, 1 000 children и editor 100 KiB:

- Cached screen usable ≤1 с; cached note open p95 ≤150 мс.
- Input p95 ≤50 мс, без обычных main-thread tasks >100 мс.
- Local save ACK p95 ≤250 мс после debounce; медленный диск показывает Saving без блокировки UI.
- Preview 100 KiB ≤300 мс; hidden Preview не перерендеривается при input, >512 KiB показывает limit.
- DOM tree/list ограничен viewport+overscan, input не перерисовывает всё дерево; idle Notes не имеет отдельного S3 polling.
- Backup streaming, дополнительная память ≤64 MiB на 1 GiB дерева; недостаток места snapshot+archive даёт явную ошибку.

Эти числа — цели, а не результаты текущего запуска. Отчёт фиксирует OS/CPU/RAM, data sizes и p50/p95, реальные failures/limitations. Выполненные baseline/feature checks подтверждаются output; принятая архитектура сама по себе не объявляет Notes готовыми.
