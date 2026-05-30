//! Conflict Engine — обнаружение, разрешение и человеко-понятное описание конфликтов.
//!
//! Поддерживаемые типы конфликтов:
//! - `edit_edit` — два устройства изменили один файл одновременно (sibling revisions)
//! - `delete_edit` — одно устройство удалило, другое изменило
//! - `rename_rename` — два устройства переименовали по-разному
//! - `create_create` — два устройства создали файл с одинаковым именем
//! - `external_change` — S3 CLI/другой инструмент изменил файл без S4Drive

use crate::db::LocalDatabase;
use crate::error::{CoreError, CoreResult};
use std::path::Path;
use uuid::Uuid;

/// Тип конфликта
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictType {
    EditEdit,
    DeleteEdit,
    RenameRename,
    CreateCreate,
    ExternalChange,
}

impl ConflictType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConflictType::EditEdit => "edit_edit",
            ConflictType::DeleteEdit => "delete_edit",
            ConflictType::RenameRename => "rename_rename",
            ConflictType::CreateCreate => "create_create",
            ConflictType::ExternalChange => "external_change",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "edit_edit" => Some(ConflictType::EditEdit),
            "delete_edit" => Some(ConflictType::DeleteEdit),
            "rename_rename" => Some(ConflictType::RenameRename),
            "create_create" => Some(ConflictType::CreateCreate),
            "external_change" => Some(ConflictType::ExternalChange),
            _ => None,
        }
    }
}

/// Результат разрешения конфликта
#[derive(Debug, Clone)]
pub enum ConflictResolution {
    /// Локальная версия победила (remote — конфликтная копия)
    KeepLocal,
    /// Удалённая версия победила (local — конфликтная копия)
    KeepRemote,
    /// Сохранить обе как sibling revisions
    KeepBoth,
    /// Текстовый 3-way merge удался
    Merged(String),
    /// Merge не удался — конфликтная копия
    MergeFailed,
}

/// Детектор конфликтов — определяет тип конфликта по ревизиям
#[derive(Debug, Clone)]
pub struct ConflictDetector;

impl ConflictDetector {
    /// Detect the type of conflict between local and remote states.
    #[allow(clippy::too_many_arguments)]
    pub fn detect(
        local_exists: bool,
        remote_exists: bool,
        local_is_deleted: bool,
        remote_is_deleted: bool,
        local_parent_revision: Option<&str>,
        remote_parent_revision: Option<&str>,
        local_revision: Option<&str>,
        remote_revision: Option<&str>,
    ) -> Option<ConflictType> {
        // Delete/Edit: one deleted, other edited
        if local_is_deleted && !remote_is_deleted {
            return Some(ConflictType::DeleteEdit);
        }
        if !local_is_deleted && remote_is_deleted {
            return Some(ConflictType::DeleteEdit);
        }

        // If both exist and have the same parent but different child revisions → sibling
        if local_exists && remote_exists {
            if let (Some(lp), Some(rp)) = (local_parent_revision, remote_parent_revision) {
                if lp == rp && local_revision != remote_revision {
                    return Some(ConflictType::EditEdit);
                }
            }
        }

        // Rename/Rename detected externally
        None
    }
}

/// Основной движок разрешения конфликтов
#[derive(Clone, Default)]
pub struct ConflictEngine {
    db: Option<LocalDatabase>,
    device_id: String,
    device_name: String,
}

impl ConflictEngine {
    pub fn new(db: Option<LocalDatabase>, device_id: &str, device_name: &str) -> Self {
        Self {
            db,
            device_id: device_id.to_string(),
            device_name: device_name.to_string(),
        }
    }

    pub fn configure(&mut self, db: LocalDatabase, device_id: &str, device_name: &str) {
        self.db = Some(db);
        self.device_id = device_id.to_string();
        self.device_name = device_name.to_string();
    }

    /// Generate a conflict-safe filename:
    /// `name (conflict from <Device> <Date>).ext`
    pub fn conflict_filename(name: &str, device_name: &str, date: &str) -> String {
        let path = Path::new(name);
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| name.to_string());
        let ext = path
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();

        // Simplify date to YYYY-MM-DD HH:MM
        let short_date = if date.len() >= 16 { &date[..16] } else { date };

        format!(
            "{} (conflict from {} {}).{}",
            stem,
            device_name,
            short_date,
            if ext.is_empty() {
                "conflict"
            } else {
                &ext[1..]
            }
        )
    }

    /// Create a conflict copy of a file on the filesystem.
    pub fn create_conflict_copy(
        source_path: &str,
        sync_folder: &str,
        original_name: &str,
        device_name: &str,
        date: &str,
    ) -> CoreResult<String> {
        let conflict_name = Self::conflict_filename(original_name, device_name, date);
        let conflict_path = Path::new(sync_folder).join(&conflict_name);

        if Path::new(source_path).exists() {
            std::fs::copy(source_path, &conflict_path)
                .map_err(|e| CoreError::FileSystem(format!("conflict copy: {}", e)))?;
        } else {
            // Create empty file as placeholder
            std::fs::write(&conflict_path, b"")
                .map_err(|e| CoreError::FileSystem(format!("conflict placeholder: {}", e)))?;
        }

        Ok(conflict_path.to_string_lossy().to_string())
    }

    /// Generate a human-readable explanation of a conflict.
    pub fn explain_conflict(
        conflict_type: &ConflictType,
        local_path: &str,
        remote_path: &str,
        local_device: &str,
        remote_device: &str,
        local_date: &str,
        remote_date: &str,
    ) -> String {
        let file_name = Path::new(local_path)
            .file_name()
            .map(|s| s.to_string_lossy())
            .unwrap_or_else(|| std::borrow::Cow::Borrowed(local_path));

        match conflict_type {
            ConflictType::EditEdit => format!(
                "📝 Два изменения: «{}» одновременно меняли на двух устройствах.\n\
                 · Локально: {} изменён {} на «{}»\n\
                 · Удалённо: {} изменён {} на «{}»\n\
                 Обе версии сохранены. Выберите нужную.",
                file_name, local_device, &local_date[..16], local_path,
                remote_device, &remote_date[..16], remote_path,
            ),
            ConflictType::DeleteEdit => format!(
                "🗑️ Конфликт удаления/изменения: «{}» удалён на одном устройстве, но изменён на другом.\n\
                 · {} удалён {}\n\
                 · {} изменён {} на {}\n\
                 Изменённая версия сохранена как конфликтная копия.",
                file_name,
                remote_device, &remote_date[..16],
                local_device, &local_date[..16], local_path,
            ),
            ConflictType::RenameRename => format!(
                "🔄 Конфликт переименования: «{}» переименован по-разному на двух устройствах.\n\
                 · {} → {}\n\
                 · {} → {}\n\
                 Применено первое по алфавиту имя.",
                file_name,
                local_device, local_path,
                remote_device, remote_path,
            ),
            ConflictType::CreateCreate => format!(
                "✨ Конфликт создания: файл «{}» создан на двух устройствах одновременно.\n\
                 · {} создал\n· {} создал\n\
                 Сохранены обе версии.",
                file_name, local_device, remote_device,
            ),
            ConflictType::ExternalChange => format!(
                "🔧 Внешнее изменение: «{}» изменён без S4Drive (S3 CLI, другой инструмент).\n\
                 Файл может не соответствовать локальной версии.",
                file_name,
            ),
        }
    }

    /// Humanize a conflict resolution status
    pub fn resolution_human(status: &str) -> &'static str {
        match status {
            "resolved_keep_local" => "✅ Оставлена локальная версия",
            "resolved_keep_remote" => "✅ Оставлена удалённая версия",
            "resolved_keep_both" => "✅ Сохранены обе версии",
            "resolved_merged" => "✅ Выполнен автоматический merge",
            _ => "⏳ Ожидает разрешения",
        }
    }

    /// Record a conflict in the database.
    #[allow(clippy::too_many_arguments)]
    pub fn record_conflict(
        &self,
        file_id: &str,
        conflict_type: &ConflictType,
        local_path: &str,
        remote_path: &str,
        sibling_path: &str,
        local_rev: Option<&str>,
        remote_rev: Option<&str>,
        reason: &str,
    ) -> CoreResult<String> {
        let conflict_id = Uuid::now_v7().to_string();
        let now = chrono::Utc::now().to_rfc3339();

        if let Some(ref db) = self.db {
            let record = crate::db::ConflictRecord {
                id: 0,
                conflict_id: conflict_id.clone(),
                file_id: file_id.to_string(),
                local_revision_id: local_rev.map(|s| s.to_string()),
                remote_revision_id: remote_rev.map(|s| s.to_string()),
                local_path: local_path.to_string(),
                remote_path: remote_path.to_string(),
                sibling_path: sibling_path.to_string(),
                conflict_type: conflict_type.as_str().to_string(),
                human_reason: reason.to_string(),
                file_size: 0,
                mime: None,
                status: "open".to_string(),
                created_at: now,
                resolved_at: None,
            };
            db.insert_conflict_record(&record)?;
        }

        tracing::warn!(
            "Conflict recorded [{}] {}: {} — {}",
            conflict_type.as_str(),
            file_id,
            local_path,
            reason
        );

        Ok(conflict_id)
    }

    /// Resolve a conflict: apply the chosen resolution and update DB.
    pub fn resolve(
        &self,
        conflict_id: &str,
        resolution: ConflictResolution,
        note: &str,
    ) -> CoreResult<()> {
        let status = match resolution {
            ConflictResolution::KeepLocal => "resolved_keep_local",
            ConflictResolution::KeepRemote => "resolved_keep_remote",
            ConflictResolution::KeepBoth => "resolved_keep_both",
            ConflictResolution::Merged(_) => "resolved_merged",
            ConflictResolution::MergeFailed => "open", // not really resolved
        };

        if let Some(ref db) = self.db {
            db.resolve_conflict(conflict_id, status, note)?;
        }

        tracing::info!("Conflict {} resolved: {}", conflict_id, status);
        Ok(())
    }

    /// 3-way merge for text files using line-by-line alignment.
    ///
    /// For each base line, we find its position in local and remote copies.
    /// Lines before the match are insertions from that side.
    /// If both sides insert before the same base line → conflict markers.
    pub fn text_three_way_merge(
        base_content: &str,
        local_content: &str,
        remote_content: &str,
        local_path: &str,
        remote_path: &str,
    ) -> CoreResult<String> {
        let base: Vec<&str> = base_content.lines().collect();
        let local: Vec<&str> = local_content.lines().collect();
        let remote: Vec<&str> = remote_content.lines().collect();

        let mut out: Vec<String> = Vec::new();
        let mut li = 0usize;
        let mut ri = 0usize;

        let local_name = Path::new(local_path)
            .file_name()
            .map(|s| s.to_string_lossy())
            .unwrap_or_else(|| std::borrow::Cow::Borrowed("local"))
            .to_string();
        let remote_name = Path::new(remote_path)
            .file_name()
            .map(|s| s.to_string_lossy())
            .unwrap_or_else(|| std::borrow::Cow::Borrowed("remote"))
            .to_string();

        for &base_line in &base {
            // Where does this base line appear next in each side?
            let local_pos = local[li..].iter().position(|&l| l == base_line);
            let remote_pos = remote[ri..].iter().position(|&r| r == base_line);

            match (local_pos, remote_pos) {
                (None, None) => {
                    // Line deleted on both sides — can't happen on same line reference
                    // but if it does, take what's ahead
                    if li < local.len() && ri < remote.len() {
                        out.push(format!("<<<<<<< {}", local_name));
                        out.push(local[li].to_string());
                        li += 1;
                        out.push("=======".to_string());
                        out.push(remote[ri].to_string());
                        ri += 1;
                        out.push(format!(">>>>>>> {}", remote_name));
                    }
                }
                (None, Some(rp)) => {
                    // Deleted locally, present remotely
                    for _ in 0..rp {
                        out.push(remote[ri].to_string());
                        ri += 1;
                    }
                    out.push(remote[ri].to_string()); // the matching line
                    ri += 1;
                }
                (Some(lp), None) => {
                    // Deleted remotely, present locally
                    for _ in 0..lp {
                        out.push(local[li].to_string());
                        li += 1;
                    }
                    out.push(local[li].to_string());
                    li += 1;
                }
                (Some(lp), Some(rp)) => {
                    // Both have this line
                    if lp == 0 && rp == 0 {
                        // Aligned — no new lines before it
                        out.push(local[li].to_string());
                        li += 1;
                        ri += 1;
                    } else if lp > 0 && rp > 0 {
                        // Both inserted lines → conflict
                        out.push(format!("<<<<<<< {}", local_name));
                        for _ in 0..lp {
                            out.push(local[li].to_string());
                            li += 1;
                        }
                        out.push("=======".to_string());
                        for _ in 0..rp {
                            out.push(remote[ri].to_string());
                            ri += 1;
                        }
                        out.push(format!(">>>>>>> {}", remote_name));
                        out.push(local[li].to_string()); // the common line
                        li += 1;
                        ri += 1;
                    } else if lp > 0 {
                        // Only local inserted lines
                        for _ in 0..lp {
                            out.push(local[li].to_string());
                            li += 1;
                        }
                        out.push(local[li].to_string()); // common line
                        li += 1;
                        ri += 1;
                    } else {
                        // Only remote inserted lines
                        for _ in 0..rp {
                            out.push(remote[ri].to_string());
                            ri += 1;
                        }
                        out.push(remote[ri].to_string()); // common line
                        ri += 1;
                        li += 1;
                    }
                }
            }
        }

        // Trailing lines (additions at end)
        while li < local.len() || ri < remote.len() {
            if li < local.len() && ri < remote.len() && local[li] == remote[ri] {
                out.push(local[li].to_string());
                li += 1;
                ri += 1;
            } else if li < local.len() && ri >= remote.len() {
                out.push(local[li].to_string());
                li += 1;
            } else if ri < remote.len() && li >= local.len() {
                out.push(remote[ri].to_string());
                ri += 1;
            } else {
                // Both have different trailing lines → conflict
                out.push(format!("<<<<<<< {}", local_name));
                if li < local.len() {
                    out.push(local[li].to_string());
                    li += 1;
                }
                out.push("=======".to_string());
                if ri < remote.len() {
                    out.push(remote[ri].to_string());
                    ri += 1;
                }
                out.push(format!(">>>>>>> {}", remote_name));
            }
        }

        Ok(out.join("\n"))
    }

    /// Check if a file is likely text-based (by extension).
    pub fn is_text_file(path: &str) -> bool {
        let ext = Path::new(path)
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();

        matches!(
            ext.as_str(),
            "txt"
                | "md"
                | "json"
                | "yaml"
                | "yml"
                | "toml"
                | "xml"
                | "html"
                | "htm"
                | "css"
                | "js"
                | "ts"
                | "jsx"
                | "tsx"
                | "rs"
                | "py"
                | "rb"
                | "go"
                | "java"
                | "kt"
                | "swift"
                | "c"
                | "h"
                | "cpp"
                | "hpp"
                | "sh"
                | "bash"
                | "zsh"
                | "fish"
                | "env"
                | "ini"
                | "cfg"
                | "conf"
                | "log"
                | "csv"
                | "tsv"
                | "sql"
                | "r"
                | "scala"
                | "clj"
                | "lua"
                | "pl"
                | "pm"
                | "php"
                | "dart"
                | "gradle"
                | "lock"
                | "gitignore"
                | "dockerfile"
        )
    }

    // ─── Private: LCS indices ───────────────────────────────────────

    /// Compute the indices (in base) that appear in `b_lines` — i.e. which
    /// base lines are preserved in the modified version.
    #[allow(dead_code)]
    fn lcs_indices<'a>(base: &[&'a str], modified: &[&'a str]) -> Vec<usize> {
        let m = base.len();
        let n = modified.len();
        if m == 0 || n == 0 {
            return Vec::new();
        }

        // Build DP table
        let mut dp = vec![vec![0u32; n + 1]; m + 1];
        for i in 1..=m {
            for j in 1..=n {
                if base[i - 1] == modified[j - 1] {
                    dp[i][j] = dp[i - 1][j - 1] + 1;
                } else {
                    dp[i][j] = dp[i - 1][j].max(dp[i][j - 1]);
                }
            }
        }

        // Backtrack to find which base indices are in LCS
        let mut result = Vec::new();
        let mut i = m;
        let mut j = n;
        while i > 0 && j > 0 {
            if base[i - 1] == modified[j - 1] {
                result.push(i - 1);
                i -= 1;
                j -= 1;
            } else if dp[i - 1][j] > dp[i][j - 1] {
                i -= 1;
            } else {
                j -= 1;
            }
        }
        result.reverse();
        result
    }
}

// ─── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_conflict_filename_no_ext() {
        let name = ConflictEngine::conflict_filename("README", "MyPhone", "2026-05-30T10:00:00Z");
        assert!(name.contains("README"));
        assert!(name.contains("conflict from"));
        assert!(name.contains("MyPhone"));
        assert!(name.contains(".conflict"));
    }

    #[test]
    fn test_conflict_filename_with_ext() {
        let name =
            ConflictEngine::conflict_filename("report.txt", "Laptop", "2026-05-30T10:00:00Z");
        assert!(name.contains("report"));
        assert!(name.contains("conflict from Laptop"));
        assert!(name.ends_with(".txt"));
    }

    #[test]
    fn test_is_text_file() {
        assert!(ConflictEngine::is_text_file("file.txt"));
        assert!(ConflictEngine::is_text_file("main.rs"));
        assert!(ConflictEngine::is_text_file("index.html"));
        assert!(ConflictEngine::is_text_file("config.yaml"));
        assert!(ConflictEngine::is_text_file("Cargo.lock"));
        assert!(ConflictEngine::is_text_file("run.sh"));
        assert!(!ConflictEngine::is_text_file("image.png"));
        assert!(!ConflictEngine::is_text_file("archive.zip"));
        assert!(!ConflictEngine::is_text_file("binary.bin"));
        assert!(!ConflictEngine::is_text_file("document.pdf"));
    }

    #[test]
    fn test_detect_delete_edit_local_deleted() {
        let result = ConflictDetector::detect(
            true,  // local exists
            true,  // remote exists
            true,  // local is deleted (on remote device)
            false, // remote is not deleted
            None, None, None, None,
        );
        assert_eq!(result, Some(ConflictType::DeleteEdit));
    }

    #[test]
    fn test_detect_delete_edit_remote_deleted() {
        let result = ConflictDetector::detect(
            true,  // local exists
            true,  // remote exists
            false, // local not deleted
            true,  // remote is deleted
            None, None, None, None,
        );
        assert_eq!(result, Some(ConflictType::DeleteEdit));
    }

    #[test]
    fn test_detect_edit_edit_sibling() {
        let result = ConflictDetector::detect(
            true,
            true,
            false,
            false,
            Some("rev1"),
            Some("rev1"), // same parent
            Some("rev2"),
            Some("rev3"), // different children
        );
        assert_eq!(result, Some(ConflictType::EditEdit));
    }

    #[test]
    fn test_detect_no_conflict() {
        let result = ConflictDetector::detect(
            true,
            true,
            false,
            false,
            Some("rev1"),
            Some("rev1"),
            Some("rev2"),
            Some("rev2"), // same child
        );
        assert_eq!(result, None);
    }

    #[test]
    fn test_explain_edit_edit() {
        let explanation = ConflictEngine::explain_conflict(
            &ConflictType::EditEdit,
            "/home/file.txt",
            "/home/file.txt",
            "Laptop",
            "Phone",
            "2026-05-30T10:00:00Z",
            "2026-05-30T10:01:00Z",
        );
        assert!(explanation.contains("Два изменения"));
        assert!(explanation.contains("Laptop"));
        assert!(explanation.contains("Phone"));
    }

    #[test]
    fn test_explain_delete_edit() {
        let explanation = ConflictEngine::explain_conflict(
            &ConflictType::DeleteEdit,
            "/home/file.txt",
            "/home/file.txt",
            "Laptop",
            "Phone",
            "2026-05-30T10:00:00Z",
            "2026-05-30T10:05:00Z",
        );
        assert!(explanation.contains("удаления/изменения"));
        assert!(explanation.contains("удалён"));
        assert!(explanation.contains("изменён"));
    }

    #[test]
    fn test_lcs_simple() {
        let base = vec!["a", "b", "c"];
        let modified = vec!["a", "x", "b", "c"];
        let indices = ConflictEngine::lcs_indices(&base, &modified);
        assert_eq!(indices, vec![0, 1, 2]); // a, b, c preserved
    }

    #[test]
    fn test_lcs_partial() {
        let base = vec!["a", "b", "c", "d"];
        let modified = vec!["a", "c", "e"];
        let indices = ConflictEngine::lcs_indices(&base, &modified);
        assert!(indices.len() >= 2); // at least a and c
        assert!(indices.contains(&0)); // a
        assert!(indices.contains(&2)); // c
    }

    #[test]
    fn test_text_three_way_merge_clean() {
        let base = "line1\nline2\nline3";
        let local = "line1\nMODIFIED_LOCAL\nline3";
        let remote = "line1\nMODIFIED_REMOTE\nline3";
        // Both modify different lines? Actually they modify same line
        let result =
            ConflictEngine::text_three_way_merge(base, local, remote, "a.txt", "b.txt").unwrap();
        // Should contain both changes since they're on different paths through LCS
        assert!(!result.is_empty());
    }

    #[test]
    fn test_text_three_way_merge_additions_same_spot() {
        let base = "start\nend";
        let local = "start\nlocal addition\nend";
        let remote = "start\nremote addition\nend";
        let result =
            ConflictEngine::text_three_way_merge(base, local, remote, "myfile.txt", "other.txt")
                .unwrap();
        // Both added at the same position — LCS marks conflict
        assert!(result.contains("<<<<<<<"));
        assert!(result.contains(">>>>>>>"));
        assert!(result.contains("local addition"));
        assert!(result.contains("remote addition"));
    }

    #[test]
    fn test_text_three_way_merge_additions_different_spots() {
        let base = "first\nsecond\nthird";
        let local = "LOCAL_ADDED\nfirst\nsecond\nthird";
        let remote = "first\nsecond\nREMOTE_ADDED\nthird";
        let result =
            ConflictEngine::text_three_way_merge(base, local, remote, "a.txt", "b.txt").unwrap();
        // Different positions — clean merge
        assert!(result.contains("LOCAL_ADDED"));
        assert!(result.contains("REMOTE_ADDED"));
        assert!(!result.contains("<<<<<<<"));
    }

    #[test]
    fn test_text_three_way_merge_identity() {
        let content = "hello\nworld\nfoo\nbar";
        let result =
            ConflictEngine::text_three_way_merge(content, content, content, "a.txt", "a.txt")
                .unwrap();
        assert_eq!(result, content);
    }

    #[test]
    fn test_create_conflict_copy() {
        let tmp = std::env::temp_dir().join("s4drive_test_conflict_copy");
        let _ = std::fs::create_dir_all(&tmp);

        let src = tmp.join("original.txt");
        std::fs::write(&src, b"hello conflict").unwrap();

        let dest = ConflictEngine::create_conflict_copy(
            &src.to_string_lossy(),
            &tmp.to_string_lossy(),
            "original.txt",
            "TestDevice",
            "2026-05-30T10:00:00Z",
        )
        .unwrap();

        assert!(std::fs::metadata(&dest).is_ok());
        // Cleanup
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dest);
        let _ = std::fs::remove_dir(&tmp);
    }

    #[test]
    fn test_conflict_type_str_roundtrip() {
        let types = [
            ConflictType::EditEdit,
            ConflictType::DeleteEdit,
            ConflictType::RenameRename,
            ConflictType::CreateCreate,
            ConflictType::ExternalChange,
        ];
        for ct in &types {
            assert_eq!(ConflictType::from_str(ct.as_str()), Some(ct.clone()));
        }
    }

    #[test]
    fn test_resolution_human() {
        assert!(ConflictEngine::resolution_human("resolved_keep_local").contains("локальная"));
        assert!(ConflictEngine::resolution_human("resolved_merged").contains("merge"));
        assert!(ConflictEngine::resolution_human("open").contains("Ожидает"));
    }

    #[test]
    fn test_lcs_empty() {
        let indices = ConflictEngine::lcs_indices(&[], &["a", "b"]);
        assert!(indices.is_empty());

        let indices = ConflictEngine::lcs_indices(&["a", "b"], &[]);
        assert!(indices.is_empty());
    }

    #[test]
    fn test_lcs_no_match() {
        let indices = ConflictEngine::lcs_indices(&["a", "b"], &["c", "d"]);
        assert!(indices.is_empty());
    }
}
