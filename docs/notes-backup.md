# Notes Backup, Verification, and Disaster Recovery

This document describes the backup archive format, integrity verification guarantees, security safeguards, and disaster recovery procedures for **S4Drive Notes**.

---

## 1. Overview & Principles

S4Drive Notes backup creates a standalone, portable snapshot of all user notes and folders as a standard ZIP archive.

Key principles:
- **Zero Lock-in**: The backup contains plain UTF-8 Markdown (`.md`) files and folders. No S4Drive software, proprietary database, or remote cloud connection is required to inspect or restore notes.
- **Strict Destination Isolation**: The backup archive must always be written **outside** the S4Drive synchronization root folder. Writing archives inside the sync directory is strictly blocked to avoid accidental recursion and unnecessary upload loops.
- **Atomic, Safe Publication**: Archives are written into a temporary staging file alongside the target destination and published atomically. If an archive with the same name already exists, the operation fails without overwriting or destroying existing backup archives.
- **Durable Snapshot Consistency**: An internal barrier prevents concurrent topology changes during directory capture. If external processes modify notes mid-capture, the capture process automatically retries up to three times to obtain a consistent snapshot.
- **Self-Verifying Archive**: Every archive is validated immediately after writing (checking paths, sizes, and BLAKE3 hashes against the embedded manifest) before it is committed as successful.

---

## 2. Archive Format v1

An S4Drive Notes backup is a standard unencrypted ZIP archive formatted as follows:

```text
S4Drive-Notes-YYYYMMDD-HHmmss.zip
├── manifest.json
└── Notes/
    ├── Work/
    │   └── Project Plan.md
    ├── Personal/                  # Empty directories are preserved
    └── Quick Note.md
```

### Archive Structure

- **`manifest.json`**: Located at the root of the archive. Contains metadata and BLAKE3 checksums for every entry.
- **`Notes/`**: Top-level directory containing all user notes and folder trees.
  - Subdirectories retain their exact folder structure.
  - Notes are saved as UTF-8 encoded text files.
  - Non-Markdown files residing in the Notes folder are included to ensure complete preservation.
  - Excluded from archive: ephemeral staging artifacts, lockfiles, temporary files (`.s4tmp-*`).

### Manifest Specification (`manifest.json`)

The manifest contains verifiable cryptographic metadata:

```json
{
  "format": "s4drive-notes-backup",
  "version": 1,
  "created_at": "2026-10-08T12:00:00Z",
  "app_version": "0.2.0",
  "includes_pending": false,
  "entries": [
    {
      "path": "Notes",
      "kind": "directory",
      "size": 0,
      "blake3": null
    },
    {
      "path": "Notes/Quick Note.md",
      "kind": "file",
      "size": 128,
      "blake3": "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
    }
  ]
}
```

> **Privacy & Security Note**: `manifest.json` deliberately excludes cloud credentials, S3 bucket names, endpoint URLs, local user account paths, device tokens, and database internal identifiers.

---

## 3. Security Safeguards

S4Drive implements defenses against malicious or corrupted archives:

| Vector | Protection Mechanism |
|---|---|
| **Path Traversal** | Archive entries starting with `/`, containing `..`, containing backslashes `\`, or having Windows drive letters (`C:`) are rejected by the verifier and extractor. |
| **Symlink Attacks** | Symbolic links, hard links, junction points, and special device files are strictly prohibited. |
| **Zip Bombs** | Strict extraction bounds are enforced: maximum 100,000 entries, maximum 8 GiB per single file, maximum 20 GiB total uncompressed size. Streamed bytes are counted on-the-fly and truncated with an error if bounds are exceeded. |
| **Collision Attacks** | Every path has exactly one meaning: `.` or empty components, repeated `/`, and names not valid on Windows (reserved names such as `CON`, a trailing dot or space, `<>:"\|?*`) are rejected. Duplicate paths and names equal after Unicode normalization and case folding (e.g. `Note.md` and `note.md`) are rejected too. Backup refuses to include such a name, so it never writes an archive the verifier rejects. |
| **Incomplete Archives** | `manifest.json` and the archive must list exactly the same files and folders, including empty folders and the `Notes` root, with the same kind on both sides. A missing, extra or duplicated entry, or a second `manifest.json`, marks the archive corrupted. |
| **Integrity Tampering** | Every entry's uncompressed BLAKE3 hash is compared against `manifest.json`, during verification and again while extracting. Any discrepancy halts extraction and marks the archive corrupted. |

---

## 4. Verification and Extraction

### Verifying an Archive

Verification checks all manifest entries against the archive contents and re-computes BLAKE3 hashes for every file without writing anything to disk.

Using S4Drive Core CLI:
```bash
s4drive-cli notes verify-backup --archive /path/to/S4Drive-Notes-20261008-120000.zip
```

### Extracting an Archive

To prevent accidental data overwrites or file leakage, S4Drive's built-in extractor **requires the target output directory to be completely empty**:

```bash
mkdir -p /path/to/RestoredNotes
s4drive-cli notes extract-backup \
  --archive /path/to/S4Drive-Notes-20261008-120000.zip \
  --output /path/to/RestoredNotes
```

If the target directory contains any existing files or folders, extraction aborts immediately. Files are written to a private staging folder inside the target and moved into place only after every entry has been checked; nothing that appears in the target meanwhile is replaced or removed. The result is `Notes/` and a copy of `manifest.json`.

---

## 5. Disaster Recovery without S4Drive

If S4Drive is not installed or the local database is lost, you can inspect and recover notes using standard operating system utilities.

### Linux / macOS

1. Unzip the archive into an empty folder:
   ```bash
   mkdir -p ~/RestoredNotes
   unzip S4Drive-Notes-YYYYMMDD-HHmmss.zip -d ~/RestoredNotes
   ```

2. Inspect the manifest:
   ```bash
   cat ~/RestoredNotes/manifest.json
   ```

3. View or edit notes directly with any text editor:
   ```bash
   cat ~/RestoredNotes/Notes/"Quick Note.md"
   ```

4. *(Optional)* Verify BLAKE3 hashes using `b3sum`:
   ```bash
   cd ~/RestoredNotes
   b3sum Notes/"Quick Note.md"
   ```

### Windows

1. Right-click the `.zip` archive in File Explorer and select **Extract All…**.
2. Select a new empty folder (e.g., `C:\RestoredNotes`).
3. Open any `.md` file with Notepad, VS Code, or your preferred Markdown editor.

---

## 6. Restoring Notes to S4Drive

To restore notes back into S4Drive:

1. Extract the backup archive to a temporary directory outside the sync folder.
2. In your desktop file manager, navigate to the extracted `Notes/` directory.
3. Copy or drag the desired files and folders into your S4Drive `Notes/` directory.
4. S4Drive will automatically detect the files, assign persistent stable IDs, and synchronize them across your devices.
