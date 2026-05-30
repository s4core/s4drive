# S4Drive Product Spec

> **Версия:** 1.0
> **Дата:** 30 мая 2026

---

## Elevator Pitch

S4Drive — это Google Drive/Dropbox/OneDrive-подобный клиент для любого S3-совместимого хранилища. Работает на Windows, Linux, macOS, Android и iOS. Быстрый, красивый, надёжный.

## Core Features (v1.0)

### S3 Integration
- Подключение к любому S3-совместимому хранилищу (AWS S3, Cloudflare R2, Backblaze B2, MinIO, custom S3)
- Один бакет = один диск
- Совместимость Level 2+ (conditional writes, strong consistency, multipart upload)

### Two-Way Sync
- Полная двусторонняя синхронизация между всеми устройствами
- Собственный Rust sync engine (не rclone)
- Persistent queue — переживает crash, обрыв сети, sleep/wake
- Rename сохраняет file_id — история не теряется

### Desktop
- Фоновый режим с иконкой в системном трее
- Окно среднего размера (~400×600 px, Dropbox-стиль)
- Закрытие окна → скрыть в трей, синхронизация продолжается
- Выход → только через контекстное меню трея
- Autostart, notifications

### File Management
- Файловый менеджер (list/grid view)
- Drag-and-drop загрузка из ОС
- Контекстное меню: rename, move, delete, version history, share (v1.5)
- Поиск

### Version History
- Каждая версия файла сохраняется
- Восстановление любой версии
- Конфликтные копии при параллельном редактировании

### Trash
- Удалённые файлы попадают в корзину
- Восстановление из корзины
- Автоматическая очистка после retention period

### Mobile (browse/upload/download/offline)
- Просмотр файлов и папок
- Загрузка/выгрузка
- Offline-доступ для выбранных файлов
- Share to S4Drive / Open from S4Drive

### Security
- OS keychain для хранения credentials
- TLS in transit
- Signed updates
- Device registration

## Non-Goals (v1.0)
- Real-time collaboration (v2.0)
- E2EE (v2.0)
- Web app (v2.0)
- Sharing links (v1.5)
- Team/multi-user support (v2.0)
- Full-text search (v2.0)

## Platform Matrix

| Platform | v1.0 | v1.5 | v2.0 |
|----------|------|------|------|
| Windows | ✅ Sync folder + tray | ✅ Cloud Files API | ✅ Shell extensions |
| macOS | ✅ Sync folder + tray | ✅ File Provider | ✅ Spotlight |
| Linux | ✅ Sync folder + tray | ✅ DE extensions | ✅ FUSE |
| Android | ✅ Browse/upload/offline | ✅ WorkManager + SAF | ✅ Camera upload |
| iOS | ✅ Browse/upload/offline | ✅ File Provider + BG | ✅ Push notifications |
