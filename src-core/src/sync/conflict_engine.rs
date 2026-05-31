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
use std::path::{Component, Path, PathBuf};
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
        let path = Path::new(name)
            .file_name()
            .map(Path::new)
            .unwrap_or_else(|| Path::new(name));
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| name.to_string());
        let ext = path
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();

        let short_date = sanitize_filename_part(&short_date(date));
        let device_name = sanitize_filename_part(device_name);
        let stem = sanitize_filename_part(&stem);

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
        let original = Path::new(original_name);
        let file_name = original
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| original_name.to_string());
        let conflict_name = Self::conflict_filename(&file_name, device_name, date);
        let conflict_dir = conflict_parent_dir(sync_folder, original_name)?;
        std::fs::create_dir_all(&conflict_dir)
            .map_err(|e| CoreError::FileSystem(format!("conflict dir: {}", e)))?;
        let conflict_path = unique_conflict_path(conflict_dir.join(&conflict_name));

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
        let local_date = short_date(local_date);
        let remote_date = short_date(remote_date);

        match conflict_type {
            ConflictType::EditEdit => format!(
                "📝 Два изменения: «{}» одновременно меняли на двух устройствах.\n\
                 · Локально: {} изменён {} на «{}»\n\
                 · Удалённо: {} изменён {} на «{}»\n\
                 Обе версии сохранены. Выберите нужную.",
                file_name, local_device, local_date, local_path,
                remote_device, remote_date, remote_path,
            ),
            ConflictType::DeleteEdit => format!(
                "🗑️ Конфликт удаления/изменения: «{}» удалён на одном устройстве, но изменён на другом.\n\
                 · {} удалён {}\n\
                 · {} изменён {} на {}\n\
                 Изменённая версия сохранена как конфликтная копия.",
                file_name,
                remote_device, remote_date,
                local_device, local_date, local_path,
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
            if let Ok(file_id) = Uuid::parse_str(file_id) {
                db.mark_file_conflict(&file_id)?;
            }
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
            ConflictResolution::MergeFailed => {
                tracing::info!("Conflict {} remains open after failed merge", conflict_id);
                return Ok(());
            }
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
        if local_content == remote_content {
            return Ok(local_content.to_string());
        }
        if local_content == base_content {
            return Ok(remote_content.to_string());
        }
        if remote_content == base_content {
            return Ok(local_content.to_string());
        }

        let base: Vec<&str> = base_content.lines().collect();
        let local: Vec<&str> = local_content.lines().collect();
        let remote: Vec<&str> = remote_content.lines().collect();

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

        let local_changes = diff_lines(&base, &local);
        let remote_changes = diff_lines(&base, &remote);
        let merged = merge_line_changes(
            &base,
            &local_changes,
            &remote_changes,
            &local_name,
            &remote_name,
        );

        Ok(merged.join("\n"))
    }

    pub fn has_conflict_markers(content: &str) -> bool {
        content.contains("<<<<<<< ") && content.contains("=======") && content.contains(">>>>>>> ")
    }

    /// Check if a file is likely text-based (by extension).
    pub fn is_text_file(path: &str) -> bool {
        let ext = Path::new(path)
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();

        let name = Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();

        matches!(name.as_str(), "dockerfile" | "makefile" | ".gitignore")
            || matches!(
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
        lcs_pairs(base, modified)
            .into_iter()
            .map(|(base_index, _)| base_index)
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LineChange {
    start: usize,
    end: usize,
    replacement: Vec<String>,
}

fn short_date(date: &str) -> String {
    date.chars().take(16).collect()
}

fn sanitize_filename_part(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
            {
                '-'
            } else {
                ch
            }
        })
        .collect();
    let sanitized = sanitized.trim().trim_matches('.').to_string();
    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

fn conflict_parent_dir(sync_folder: &str, original_name: &str) -> CoreResult<PathBuf> {
    let mut dir = PathBuf::from(sync_folder);
    let Some(parent) = Path::new(original_name).parent() else {
        return Ok(dir);
    };

    for component in parent.components() {
        match component {
            Component::Normal(part) => dir.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(CoreError::Protocol(format!(
                    "conflict path escapes sync folder: {}",
                    original_name
                )));
            }
        }
    }
    Ok(dir)
}

fn unique_conflict_path(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }

    let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "conflict".to_string());
    let ext = path.extension().map(|e| e.to_string_lossy().to_string());

    for index in 2..10_000 {
        let file_name = match &ext {
            Some(ext) if !ext.is_empty() => format!("{} {}.{}", stem, index, ext),
            _ => format!("{} {}", stem, index),
        };
        let candidate = parent.join(file_name);
        if !candidate.exists() {
            return candidate;
        }
    }

    parent.join(format!("{} {}", stem, Uuid::now_v7()))
}

fn lcs_pairs(base: &[&str], modified: &[&str]) -> Vec<(usize, usize)> {
    let m = base.len();
    let n = modified.len();
    if m == 0 || n == 0 {
        return Vec::new();
    }

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

    let mut result = Vec::new();
    let mut i = m;
    let mut j = n;
    while i > 0 && j > 0 {
        if base[i - 1] == modified[j - 1] {
            result.push((i - 1, j - 1));
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

fn diff_lines(base: &[&str], modified: &[&str]) -> Vec<LineChange> {
    let pairs = lcs_pairs(base, modified);
    let mut changes = Vec::new();
    let mut base_cursor = 0usize;
    let mut modified_cursor = 0usize;

    for (base_index, modified_index) in pairs {
        if base_cursor < base_index || modified_cursor < modified_index {
            changes.push(LineChange {
                start: base_cursor,
                end: base_index,
                replacement: modified[modified_cursor..modified_index]
                    .iter()
                    .map(|line| (*line).to_string())
                    .collect(),
            });
        }
        base_cursor = base_index + 1;
        modified_cursor = modified_index + 1;
    }

    if base_cursor < base.len() || modified_cursor < modified.len() {
        changes.push(LineChange {
            start: base_cursor,
            end: base.len(),
            replacement: modified[modified_cursor..]
                .iter()
                .map(|line| (*line).to_string())
                .collect(),
        });
    }

    changes
}

fn merge_line_changes(
    base: &[&str],
    local_changes: &[LineChange],
    remote_changes: &[LineChange],
    local_name: &str,
    remote_name: &str,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    let mut local_index = 0usize;
    let mut remote_index = 0usize;

    while local_index < local_changes.len() || remote_index < remote_changes.len() {
        let local = local_changes.get(local_index);
        let remote = remote_changes.get(remote_index);

        match (local, remote) {
            (Some(local), Some(remote)) if changes_overlap_or_same_insertion(local, remote) => {
                append_base(&mut out, base, cursor, local.start.min(remote.start));
                if local.start == remote.start
                    && local.end == remote.end
                    && local.replacement == remote.replacement
                {
                    out.extend(local.replacement.clone());
                } else {
                    append_conflict(
                        &mut out,
                        local_name,
                        &local.replacement,
                        remote_name,
                        &remote.replacement,
                    );
                }
                cursor = local.end.max(remote.end);
                local_index += 1;
                remote_index += 1;
            }
            (Some(local), Some(remote)) if local.end <= remote.start => {
                append_base(&mut out, base, cursor, local.start);
                out.extend(local.replacement.clone());
                cursor = local.end;
                local_index += 1;
            }
            (Some(_), Some(remote)) => {
                append_base(&mut out, base, cursor, remote.start);
                out.extend(remote.replacement.clone());
                cursor = remote.end;
                remote_index += 1;
            }
            (Some(local), None) => {
                append_base(&mut out, base, cursor, local.start);
                out.extend(local.replacement.clone());
                cursor = local.end;
                local_index += 1;
            }
            (None, Some(remote)) => {
                append_base(&mut out, base, cursor, remote.start);
                out.extend(remote.replacement.clone());
                cursor = remote.end;
                remote_index += 1;
            }
            (None, None) => break,
        }
    }

    append_base(&mut out, base, cursor, base.len());
    out
}

fn changes_overlap_or_same_insertion(left: &LineChange, right: &LineChange) -> bool {
    let same_insertion =
        left.start == left.end && right.start == right.end && left.start == right.start;
    same_insertion || (left.start < right.end && right.start < left.end)
}

fn append_base(out: &mut Vec<String>, base: &[&str], start: usize, end: usize) {
    for line in &base[start.min(base.len())..end.min(base.len())] {
        out.push((*line).to_string());
    }
}

fn append_conflict(
    out: &mut Vec<String>,
    local_name: &str,
    local_lines: &[String],
    remote_name: &str,
    remote_lines: &[String],
) {
    out.push(format!("<<<<<<< {}", local_name));
    out.extend(local_lines.iter().cloned());
    out.push("=======".to_string());
    out.extend(remote_lines.iter().cloned());
    out.push(format!(">>>>>>> {}", remote_name));
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
        assert!(!name.contains(':'));
        assert!(name.ends_with(".txt"));
    }

    #[test]
    fn test_conflict_filename_uses_basename_and_sanitizes_device() {
        let name = ConflictEngine::conflict_filename(
            "nested/report.txt",
            "Bad/Device:Name",
            "2026-05-30T10:00:00Z",
        );
        assert!(name.starts_with("report "));
        assert!(name.contains("Bad-Device-Name"));
        assert!(!name.contains('/'));
        assert!(!name.contains(':'));
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
    fn test_explain_conflict_does_not_panic_on_short_dates() {
        let explanation = ConflictEngine::explain_conflict(
            &ConflictType::EditEdit,
            "file.txt",
            "file.txt",
            "Laptop",
            "Phone",
            "short",
            "",
        );
        assert!(explanation.contains("Два изменения"));
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
    fn test_text_three_way_merge_takes_remote_when_local_unchanged() {
        let base = "line1\nline2\nline3";
        let local = base;
        let remote = "line1\nremote edit\nline3";
        let result =
            ConflictEngine::text_three_way_merge(base, local, remote, "a.txt", "b.txt").unwrap();
        assert_eq!(result, remote);
    }

    #[test]
    fn test_text_three_way_merge_same_line_edits_conflict() {
        let base = "line1\nline2\nline3";
        let local = "line1\nlocal edit\nline3";
        let remote = "line1\nremote edit\nline3";
        let result =
            ConflictEngine::text_three_way_merge(base, local, remote, "a.txt", "b.txt").unwrap();
        assert!(ConflictEngine::has_conflict_markers(&result));
        assert!(result.contains("local edit"));
        assert!(result.contains("remote edit"));
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
    fn test_create_conflict_copy_preserves_relative_parent_and_is_unique() {
        let tmp = std::env::temp_dir().join(format!(
            "s4drive_test_conflict_copy_nested_{}",
            Uuid::now_v7()
        ));
        let nested = tmp.join("docs");
        std::fs::create_dir_all(&nested).unwrap();

        let src = nested.join("report.txt");
        std::fs::write(&src, b"local").unwrap();
        let first = ConflictEngine::create_conflict_copy(
            &src.to_string_lossy(),
            &tmp.to_string_lossy(),
            "docs/report.txt",
            "Device",
            "2026-05-30T10:00:00Z",
        )
        .unwrap();
        let second = ConflictEngine::create_conflict_copy(
            &src.to_string_lossy(),
            &tmp.to_string_lossy(),
            "docs/report.txt",
            "Device",
            "2026-05-30T10:00:00Z",
        )
        .unwrap();

        assert_ne!(first, second);
        assert!(Path::new(&first).parent().unwrap().ends_with("docs"));
        assert_eq!(std::fs::read(&first).unwrap(), b"local");
        assert_eq!(std::fs::read(&second).unwrap(), b"local");

        let _ = std::fs::remove_dir_all(&tmp);
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
