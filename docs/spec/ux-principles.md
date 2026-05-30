# S4Drive UX Principles

> **Дата:** 30 мая 2026

---

## 1. Core Principles

### 1.1. Hide S3 Complexity
Обычный пользователь не должен видеть термин "S3". Только "Диск", "Папки", "Файлы", "Синхронизировано". S3-термины — только в Advanced Settings.

### 1.2. Human-Language Errors
- ❌ "HTTP 502 Bad Gateway"
- ✅ "No connection to server. Check your internet and try again."
- ❌ "ETag mismatch: 412 Precondition Failed"
- ✅ "Another device changed this file at the same time."

### 1.3. Undo Everything
All destructive actions (delete, overwrite, conflict resolution) must have undo/restore.

### 1.4. Conflict is Not an Error
Never say "Sync failed". Say "This file needs your attention."

### 1.5. Desktop Tray-First
- App runs in tray (background)
- Click tray icon → open window
- Close window → hide to tray
- Exit → tray menu only

### 1.6. Mobile Cloud-First
- Files are cloud-only by default
- Pin for offline access
- Background sync is best-effort

## 2. Status Language

| Technical | User-Facing |
|-----------|-------------|
| Synced | ✓ Synced |
| Uploading | ↻ Uploading (45%) |
| Downloading | ↻ Downloading (3 of 12) |
| ModifiedLocally | Pending upload |
| ModifiedRemotely | Update available |
| Conflicted | ✗ Needs your attention |
| Locked | 🔒 Being edited on MacBook |
| Deleted (tombstone) | In trash — can restore |

## 3. Visual Status Badges

- ✓ Green check — Synced
- ↻ Blue arrows — Uploading/Downloading
- ✗ Red exclamation — Conflict
- ☁ Cloud icon — Cloud only
- ⬇ Download icon — Pinned offline
- 🔒 Lock — Locked by another device
- ⚠ Warning — Error

## 4. Onboarding Flow

1. Welcome screen
2. Choose: connect / create / advanced S3
3. Endpoint + credentials + test connection
4. Local folder selection
5. Sync scope: everything / selected / online-only
6. Finish → show tray behavior → start sync

## 5. Desktop Window Behavior

- Default size: ~400×600 px (Dropbox style)
- Remember last size and position
- Resizable but with min limits (400×500)
- Close → hide to tray
- Focus from tray click

## 6. Mobile Behavior

- Fullscreen (standard mobile UX)
- Bottom navigation
- Long press for context actions
- Share sheet integration
- Background sync via WorkManager/BG Tasks
