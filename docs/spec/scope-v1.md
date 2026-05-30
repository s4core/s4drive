# S4Drive Scope Definition

> **Дата:** 30 мая 2026

---

## Phases Overview

```
v0.1 (alpha):  Core + Sync MVP     ≈ 4-6 недель
v1.0 (stable): Full desktop + mobile  ≈ 4-6 месяцев
v1.5:          OS integrations        ≈ +3-4 месяца
v2.0:          Advanced features      ≈ +6 месяцев
```

## v0.1 — Core + Sync MVP (4-6 weeks)

**Цель:** Доказать, что sync engine работает надёжно.

- [ ] S3 Connection (any Level 2+ bucket)
- [ ] S4 Native bucket mode (.s4drive/ structure)
- [ ] One sync folder → one bucket
- [ ] Two-way sync (incremental)
- [ ] Initial upload/download
- [ ] Persistent queue (survives crash)
- [ ] Rename preserves file_id
- [ ] Delete → tombstone
- [ ] Basic conflict detection + conflict copy
- [ ] Pause/resume sync
- [ ] CLI-only (no GUI yet)

## v1.0 — Full Desktop + Mobile (4-6 months)

**Desktop (Windows, macOS, Linux):**
- [ ] Tauri window + tray
- [ ] Tray-first behavior (close→hide, exit from tray)
- [ ] File browser UI (list/grid)
- [ ] Drag-and-drop upload
- [ ] Context menu (rename, move, delete, version history)
- [ ] Settings screen (sync folder, polling, bandwidth, excludes)
- [ ] Account screen (S3 connection)
- [ ] Transfers screen (progress)
- [ ] Conflicts screen (resolver)
- [ ] Activity log
- [ ] Version history + restore
- [ ] Trash + restore
- [ ] Search
- [ ] Dark/light mode
- [ ] Onboarding flow
- [ ] Autostart
- [ ] System notifications
- [ ] Diagnostics page (Sync Doctor)
- [ ] Signed auto-updates

**Mobile (Android, iOS):**
- [ ] Browse files/folders
- [ ] Upload/download
- [ ] Offline files (manual pin)
- [ ] Share to S4Drive
- [ ] Open from S4Drive
- [ ] Dark/light mode

**Security:**
- [ ] OS keychain for credentials
- [ ] TLS validation
- [ ] Device registration
- [ ] Signed metadata ops
- [ ] Secure logging (no secrets in logs)

## v1.5 — OS Integrations (+3-4 months)

**Desktop:**
- [ ] Windows Cloud Files API (placeholder files)
- [ ] macOS File Provider extension
- [ ] Android DocumentsProvider
- [ ] iOS File Provider extension
- [ ] Online-only files (cloud-only by default)
- [ ] Explorer/Finder context menu + status badges
- [ ] Android WorkManager background sync
- [ ] iOS Background Tasks sync
- [ ] Linux DE extensions (Nautilus, Dolphin)
- [ ] Push notifications (FCM/APNS)

**Features:**
- [ ] Selective sync (choose folders)
- [ ] Bandwidth limiting
- [ ] Camera auto-upload (mobile)
- [ ] App lock (PIN/biometrics)

## v2.0 — Advanced (+6 months)

- [ ] Sharing (links, permissions, expiry)
- [ ] Team/multi-user support
- [ ] Compatibility Mode (files as plain S3 keys)
- [ ] Client-side E2EE
- [ ] Delta sync (chunked files)
- [ ] Rich previews (images, PDF, video, audio)
- [ ] Full-text search
- [ ] Agent API (for LLM agents)
- [ ] Web app / admin console
- [ ] Linux FUSE mount
- [ ] Real-time collaboration (text)
