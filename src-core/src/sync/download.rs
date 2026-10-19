//! Download Engine — безопасная загрузка файлов из S3.

use super::conflict_engine::ConflictEngine;
use crate::error::{CoreError, CoreResult};
use crate::fs_mutations::{LocalContentToken, MutationCoordinator};
use crate::metadata::types::FileId;
use crate::notes::types::WorkspaceId;
use crate::s3::S3Adapter;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

/// An owned staging locator. Failed publication deliberately retains it for conflict handling.
#[derive(Debug)]
pub struct StagedDownload {
    pub path: PathBuf,
    pub target_path: PathBuf,
    pub expected: LocalContentToken,
    pub size: u64,
}

impl StagedDownload {
    async fn reserve(target_path: &Path, expected: LocalContentToken) -> CoreResult<Self> {
        let parent = target_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| CoreError::FileSystem(format!("create download parent: {error}")))?;
        let path = parent.join(format!(".s4drive_staging_{}", uuid::Uuid::now_v7()));
        tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
            .map_err(|error| CoreError::FileSystem(format!("reserve download stage: {error}")))?;
        Ok(Self {
            path,
            target_path: target_path.to_path_buf(),
            expected,
            size: 0,
        })
    }

    /// The expected token must be captured before obtaining bytes from the network.
    pub async fn stage_bytes(
        target_path: &Path,
        bytes: &[u8],
        expected: LocalContentToken,
    ) -> CoreResult<Self> {
        let mut staged = Self::reserve(target_path, expected).await?;
        let result = async {
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&staged.path)
                .await
                .map_err(|error| CoreError::FileSystem(format!("open download stage: {error}")))?;
            file.write_all(bytes)
                .await
                .map_err(|error| CoreError::FileSystem(format!("write download stage: {error}")))?;
            file.sync_all()
                .await
                .map_err(|error| CoreError::FileSystem(format!("sync download stage: {error}")))?;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            let _ = tokio::fs::remove_file(&staged.path).await;
            return Err(error);
        }
        staged.size = bytes.len() as u64;
        Ok(staged)
    }

    pub fn publish<T>(
        &self,
        coordinator: &MutationCoordinator,
        workspace: WorkspaceId,
        id: FileId,
        after_publish: impl FnOnce() -> CoreResult<T>,
    ) -> CoreResult<T> {
        self.publish_checked(coordinator, workspace, id, || Ok(()), after_publish)
    }

    /// Publish over the local content this download was decided against. If
    /// the file changed since, its current bytes are first kept as a conflict
    /// copy beside it, so a download never replaces bytes nobody has seen.
    /// `relative_path` names the file inside `sync_folder`.
    #[allow(clippy::too_many_arguments)]
    pub fn publish_keeping_local(
        mut self,
        coordinator: &MutationCoordinator,
        workspace: WorkspaceId,
        id: FileId,
        sync_folder: &str,
        relative_path: &str,
        device_name: &str,
    ) -> CoreResult<PublishedDownload> {
        match self.publish(coordinator, workspace, id, || Ok(())) {
            Ok(()) => {
                return Ok(PublishedDownload {
                    size: self.size,
                    conflict_copy: None,
                })
            }
            Err(CoreError::StaleWrite { .. }) => {}
            Err(error) => return Err(error),
        }
        // If the file changes again before the publish below, that publish is
        // refused and the next attempt keeps the newer bytes too.
        self.expected = LocalContentToken::capture(&self.target_path)?;
        let conflict_copy = match self.expected {
            LocalContentToken::Missing => None,
            LocalContentToken::Hash(_) => Some(ConflictEngine::create_conflict_copy(
                &self.target_path.to_string_lossy(),
                sync_folder,
                relative_path,
                device_name,
                &chrono::Utc::now().to_rfc3339(),
            )?),
        };
        self.publish(coordinator, workspace, id, || Ok(()))?;
        Ok(PublishedDownload {
            size: self.size,
            conflict_copy,
        })
    }

    pub fn publish_checked<T>(
        &self,
        coordinator: &MutationCoordinator,
        workspace: WorkspaceId,
        id: FileId,
        validate_target: impl FnOnce() -> CoreResult<()>,
        after_publish: impl FnOnce() -> CoreResult<T>,
    ) -> CoreResult<T> {
        coordinator.publish_staged_checked(
            workspace,
            id,
            &self.path,
            &self.target_path,
            &self.expected,
            validate_target,
            after_publish,
        )
    }
}

/// A published download, and where the local bytes it replaced were kept.
#[derive(Debug)]
pub struct PublishedDownload {
    pub size: u64,
    pub conflict_copy: Option<String>,
}

#[derive(Clone)]
pub struct DownloadEngine {
    s3: S3Adapter,
    sync_folder: String,
}

impl DownloadEngine {
    pub fn new(s3: S3Adapter, sync_folder: String) -> Self {
        Self { s3, sync_folder }
    }

    pub fn s3(&self) -> &S3Adapter {
        &self.s3
    }

    /// Stage the remote bytes. `decided` is the local content the download
    /// was decided against; without it the content now is taken.
    pub async fn stage_file(
        &self,
        s3_key: &str,
        local_path: impl AsRef<Path>,
        decided: Option<LocalContentToken>,
    ) -> CoreResult<StagedDownload> {
        let target_path = local_path.as_ref();
        if !Path::new(&self.sync_folder).is_dir() {
            return Err(CoreError::FileSystem(
                "The local sync folder disappeared during download.".into(),
            ));
        }
        let expected = match decided {
            Some(token) => token,
            None => LocalContentToken::capture(target_path)?,
        };
        let root =
            cap_std::fs::Dir::open_ambient_dir(&self.sync_folder, cap_std::ambient_authority())
                .map_err(|e| CoreError::FileSystem(e.to_string()))?;
        let relative = target_path.strip_prefix(&self.sync_folder).map_err(|_| {
            CoreError::Protocol("Download target is outside the sync folder".into())
        })?;
        if let Some(parent) = relative.parent() {
            root.create_dir_all(parent)
                .map_err(|e| CoreError::FileSystem(e.to_string()))?;
        }
        let stage_name =
            relative.with_file_name(format!(".s4drive_staging_{}", uuid::Uuid::now_v7()));
        root.open_with(
            &stage_name,
            cap_std::fs::OpenOptions::new().write(true).create_new(true),
        )
        .map_err(|e| CoreError::FileSystem(e.to_string()))?;
        let mut staged = StagedDownload {
            path: Path::new(&self.sync_folder).join(stage_name),
            target_path: target_path.into(),
            expected,
            size: 0,
        };
        match self.s3.download_object_to_file(s3_key, &staged.path).await {
            Ok(size) => staged.size = size,
            Err(error) => {
                let _ = tokio::fs::remove_file(&staged.path).await;
                return Err(error);
            }
        }
        Ok(staged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_mutations::{LocalContentToken, MutationCoordinator};

    #[tokio::test]
    async fn moved_remote_only_download_does_not_publish_at_its_old_path() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("old-note.md");
        let staged = StagedDownload::stage_bytes(&target, b"remote", LocalContentToken::Missing)
            .await
            .unwrap();
        let id = uuid::Uuid::now_v7();

        let result = staged.publish_checked(
            &MutationCoordinator::default(),
            uuid::Uuid::now_v7(),
            id,
            || Err(CoreError::StaleWrite { note_id: id }),
            || -> CoreResult<()> { panic!("a moved file must not update the index") },
        );

        assert!(matches!(result, Err(CoreError::StaleWrite { note_id }) if note_id == id));
        assert!(!target.exists());
        assert_eq!(std::fs::read(&staged.path).unwrap(), b"remote");
    }

    #[tokio::test]
    async fn staged_download_preserves_a_new_local_save() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("note.md");
        std::fs::write(&target, b"original").unwrap();
        let expected = LocalContentToken::capture(&target).unwrap();
        let staged = StagedDownload::stage_bytes(&target, b"remote", expected)
            .await
            .unwrap();
        std::fs::write(&target, b"new local save").unwrap();

        let result = staged.publish(
            &MutationCoordinator::default(),
            uuid::Uuid::now_v7(),
            uuid::Uuid::now_v7(),
            || Ok(()),
        );

        assert!(matches!(result, Err(CoreError::StaleWrite { .. })));
        assert_eq!(std::fs::read(&target).unwrap(), b"new local save");
        assert_eq!(std::fs::read(&staged.path).unwrap(), b"remote");
    }

    #[tokio::test]
    async fn n19_download_keeps_a_save_made_after_it_was_decided() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("Notes");
        std::fs::create_dir(&folder).unwrap();
        let target = folder.join("note.md");
        std::fs::write(&target, b"base").unwrap();
        let decided = LocalContentToken::capture(&target).unwrap();
        std::fs::write(&target, b"saved after the decision").unwrap();
        let staged = StagedDownload::stage_bytes(&target, b"remote", decided)
            .await
            .unwrap();

        let published = staged
            .publish_keeping_local(
                &MutationCoordinator::default(),
                uuid::Uuid::nil(),
                uuid::Uuid::now_v7(),
                &temp.path().to_string_lossy(),
                "Notes/note.md",
                "device-a",
            )
            .unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"remote");
        let copy = published.conflict_copy.expect("the local save is kept");
        assert_eq!(std::fs::read(&copy).unwrap(), b"saved after the decision");
        assert!(copy.contains("conflict"), "{copy}");
        let left: Vec<_> = std::fs::read_dir(&folder)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".s4drive_staging_"))
            .collect();
        assert!(left.is_empty(), "staging left behind: {left:?}");
    }

    #[tokio::test]
    async fn n19_download_over_unchanged_content_makes_no_copy() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("note.md");
        std::fs::write(&target, b"base").unwrap();
        let decided = LocalContentToken::capture(&target).unwrap();
        let staged = StagedDownload::stage_bytes(&target, b"remote", decided)
            .await
            .unwrap();

        let published = staged
            .publish_keeping_local(
                &MutationCoordinator::default(),
                uuid::Uuid::nil(),
                uuid::Uuid::now_v7(),
                &temp.path().to_string_lossy(),
                "note.md",
                "device-a",
            )
            .unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"remote");
        assert_eq!(published.conflict_copy, None);
        assert_eq!(published.size, 6);
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn staged_download_publishes_without_colliding_with_another_stage() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("nested/note.md");
        let first = StagedDownload::stage_bytes(&target, b"first", LocalContentToken::Missing)
            .await
            .unwrap();
        let second = StagedDownload::stage_bytes(&target, b"second", LocalContentToken::Missing)
            .await
            .unwrap();
        assert_ne!(first.path, second.path);

        first
            .publish(
                &MutationCoordinator::default(),
                uuid::Uuid::now_v7(),
                uuid::Uuid::now_v7(),
                || Ok(()),
            )
            .unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"first");
        assert_eq!(std::fs::read(&second.path).unwrap(), b"second");
        assert_eq!(first.size, 5);
    }
}
