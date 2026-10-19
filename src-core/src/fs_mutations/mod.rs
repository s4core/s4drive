//! Shared local writer coordination. Lock order: lifecycle, topology, sorted entries, database.
pub mod atomic;
pub mod journal;
pub mod remote_commit;

use crate::error::{CoreError, CoreResult};
use crate::metadata::types::FileId;
use crate::notes::types::{RequestId, WorkspaceId};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

type EntryLocks = HashMap<(WorkspaceId, FileId), Arc<Mutex<()>>>;

/// A snapshot of actual local bytes, independent of the asynchronously updated index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalContentToken {
    Missing,
    Hash(String),
}

impl LocalContentToken {
    const STORED_MISSING: &'static str = "missing";

    /// The form kept in a transfer job: `missing` or the `blake3:` hash.
    pub fn to_stored(&self) -> String {
        match self {
            Self::Missing => Self::STORED_MISSING.to_owned(),
            Self::Hash(hash) => hash.clone(),
        }
    }

    pub fn from_stored(stored: &str) -> Self {
        if stored == Self::STORED_MISSING {
            Self::Missing
        } else {
            Self::Hash(stored.to_owned())
        }
    }

    pub fn capture(path: &Path) -> CoreResult<Self> {
        let (parent_path, name) = publication_location(path)?;
        let parent = match Dir::open_ambient_dir(parent_path, cap_std::ambient_authority()) {
            Ok(parent) => parent,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::Missing),
            Err(error) => return Err(atomic::io_error(error)),
        };
        Self::capture_in(&parent, name)
    }

    fn capture_in(parent: &Dir, name: &std::ffi::OsStr) -> CoreResult<Self> {
        match parent.symlink_metadata(name) {
            Ok(metadata) if metadata.is_symlink() => return Err(CoreError::SymlinkNotAllowed),
            Ok(metadata) if !metadata.is_file() => {
                return Err(CoreError::FileSystem(
                    "The local item is not a regular file.".into(),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::Missing),
            Err(error) => return Err(atomic::io_error(error)),
        }
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32);
        }
        let mut file = parent.open_with(name, &options).map_err(atomic::io_error)?;
        if !file.metadata().map_err(atomic::io_error)?.is_file() {
            return Err(CoreError::FileSystem(
                "The local item is not a regular file.".into(),
            ));
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update_reader(&mut file).map_err(atomic::io_error)?;
        Ok(Self::Hash(format!("blake3:{}", hasher.finalize().to_hex())))
    }
}

#[derive(Default)]
pub struct MutationCoordinator {
    lifecycle: RwLock<()>,
    topology: RwLock<()>,
    entries: Mutex<EntryLocks>,
}
impl MutationCoordinator {
    /// Stage bytes before calling this synchronous boundary. The index callback runs under
    /// the same content guard as the token check and publication. On failure, retain staging:
    /// after an exchange it can contain the actual displaced bytes of an external writer.
    #[allow(clippy::too_many_arguments)]
    pub fn publish_staged<T>(
        &self,
        workspace: WorkspaceId,
        id: FileId,
        staged_path: &Path,
        target_path: &Path,
        expected: &LocalContentToken,
        after_publish: impl FnOnce() -> CoreResult<T>,
    ) -> CoreResult<T> {
        self.publish_staged_checked(
            workspace,
            id,
            staged_path,
            target_path,
            expected,
            || Ok(()),
            after_publish,
        )
    }

    /// Validate the current workspace/id/path binding while topology changes are excluded.
    #[allow(clippy::too_many_arguments)]
    pub fn publish_staged_checked<T>(
        &self,
        workspace: WorkspaceId,
        id: FileId,
        staged_path: &Path,
        target_path: &Path,
        expected: &LocalContentToken,
        validate_target: impl FnOnce() -> CoreResult<()>,
        after_publish: impl FnOnce() -> CoreResult<T>,
    ) -> CoreResult<T> {
        self.with_content(workspace, &[id], || {
            validate_target()?;
            if LocalContentToken::capture(target_path)? != *expected {
                return Err(CoreError::StaleWrite { note_id: id });
            }
            publish_checked_stage(staged_path, target_path, expected, id, after_publish)
        })
    }

    pub fn with_content<T>(
        &self,
        workspace: WorkspaceId,
        ids: &[FileId],
        action: impl FnOnce() -> CoreResult<T>,
    ) -> CoreResult<T> {
        let _lifecycle = self.lifecycle.read().map_err(poison)?;
        let _topology = self.topology.read().map_err(poison)?;
        self.with_entries(workspace, ids, action)
    }
    pub fn with_topology<T>(
        &self,
        workspace: WorkspaceId,
        ids: &[FileId],
        action: impl FnOnce() -> CoreResult<T>,
    ) -> CoreResult<T> {
        let _lifecycle = self.lifecycle.read().map_err(poison)?;
        let _topology = self.topology.write().map_err(poison)?;
        self.with_entries(workspace, ids, action)
    }
    fn with_entries<T>(
        &self,
        workspace: WorkspaceId,
        ids: &[FileId],
        action: impl FnOnce() -> CoreResult<T>,
    ) -> CoreResult<T> {
        let mut ids = ids.to_vec();
        ids.sort();
        ids.dedup();
        let locks: Vec<_> = {
            let mut entries = self.entries.lock().map_err(poison)?;
            ids.into_iter()
                .map(|id| Arc::clone(entries.entry((workspace, id)).or_default()))
                .collect()
        };
        let _guards: Vec<_> = locks
            .iter()
            .map(|lock| lock.lock().map_err(poison))
            .collect::<CoreResult<_>>()?;
        action()
    }
}

fn publication_location(path: &Path) -> CoreResult<(&Path, &std::ffi::OsStr)> {
    let name = path.file_name().ok_or_else(|| {
        CoreError::FileSystem("A publication destination must have a file name.".into())
    })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    Ok((parent.unwrap_or_else(|| Path::new(".")), name))
}

fn publish_checked_stage<T>(
    staged_path: &Path,
    target_path: &Path,
    expected: &LocalContentToken,
    id: FileId,
    after_publish: impl FnOnce() -> CoreResult<T>,
) -> CoreResult<T> {
    if staged_path == target_path {
        return Err(CoreError::FileSystem(
            "Staging must be separate from the local file.".into(),
        ));
    }
    let (stage_parent_path, stage_name) = publication_location(staged_path)?;
    let (target_parent_path, target_name) = publication_location(target_path)?;
    let stage_parent = Dir::open_ambient_dir(stage_parent_path, cap_std::ambient_authority())
        .map_err(atomic::io_error)?;
    let target_parent = Dir::open_ambient_dir(target_parent_path, cap_std::ambient_authority())
        .map_err(atomic::io_error)?;
    // Windows moves by path; the names are for the rustix calls.
    #[cfg(windows)]
    let _ = target_name;
    #[cfg(windows)]
    let (_stage_anchor, _target_anchor) = (
        atomic::hold_anchor(&stage_parent)?,
        atomic::hold_anchor(&target_parent)?,
    );
    if !stage_parent
        .symlink_metadata(stage_name)
        .map_err(atomic::io_error)?
        .is_file()
    {
        return Err(CoreError::FileSystem(
            "Staging must be a regular file.".into(),
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).follow(FollowSymlinks::No);
    stage_parent
        .open_with(stage_name, &options)
        .map_err(atomic::io_error)?
        .sync_all()
        .map_err(atomic::io_error)?;

    if *expected == LocalContentToken::Missing {
        #[cfg(windows)]
        let published = atomic::move_new_paths(staged_path, target_path);
        #[cfg(not(windows))]
        let published = rustix_publish_new(&stage_parent, stage_name, &target_parent, target_name);
        published.map_err(|error| match error {
            CoreError::NameExists(_) => CoreError::StaleWrite { note_id: id },
            error => error,
        })?;
        atomic::sync_dir(&stage_parent)?;
        atomic::sync_dir(&target_parent)?;
        return after_publish();
    }

    #[cfg(windows)]
    let displaced_path = staged_path.with_file_name(format!(
        ".s4drive_staging_displaced_{}",
        uuid::Uuid::now_v7()
    ));
    #[cfg(windows)]
    atomic::exchange_paths(staged_path, target_path, &displaced_path)?;
    #[cfg(not(windows))]
    {
        rustix_exchange(&stage_parent, stage_name, &target_parent, target_name)?;
    }
    #[cfg(not(windows))]
    let displaced_path = staged_path.to_path_buf();
    tracing::debug!(displaced = %displaced_path.display(), "Retaining displaced publication bytes until verification and index update");
    atomic::sync_dir(&stage_parent)?;
    atomic::sync_dir(&target_parent)?;
    if LocalContentToken::capture(&displaced_path)? != *expected {
        tracing::warn!(displaced = %displaced_path.display(), "External edit retained during local publication");
        return Err(CoreError::StaleWrite { note_id: id });
    }
    let result = after_publish()?;
    std::fs::remove_file(&displaced_path).map_err(atomic::io_error)?;
    atomic::sync_dir(&stage_parent)?;
    Ok(result)
}

#[cfg(not(windows))]
fn rustix_publish_new(
    stage_parent: &Dir,
    stage_name: &std::ffi::OsStr,
    target_parent: &Dir,
    target_name: &std::ffi::OsStr,
) -> CoreResult<()> {
    #[cfg(unix)]
    return rustix::fs::renameat_with(
        stage_parent,
        stage_name,
        target_parent,
        target_name,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(|error| {
        if error == rustix::io::Errno::EXIST {
            CoreError::NameExists(target_name.to_string_lossy().into_owned())
        } else {
            atomic::io_error(error.into())
        }
    });
    #[cfg(not(unix))]
    Err(CoreError::MutationUnavailable)
}

#[cfg(not(windows))]
fn rustix_exchange(
    stage_parent: &Dir,
    stage_name: &std::ffi::OsStr,
    target_parent: &Dir,
    target_name: &std::ffi::OsStr,
) -> CoreResult<()> {
    #[cfg(unix)]
    return rustix::fs::renameat_with(
        stage_parent,
        stage_name,
        target_parent,
        target_name,
        rustix::fs::RenameFlags::EXCHANGE,
    )
    .map_err(|error| atomic::io_error(error.into()));
    #[cfg(not(unix))]
    Err(CoreError::MutationUnavailable)
}
fn poison<T>(error: std::sync::PoisonError<T>) -> CoreError {
    CoreError::Internal(error.to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MutationCheckpoint {
    BeforeIntent,
    AfterIntent,
    BeforeStage,
    AfterStage,
    BeforePreimage,
    AfterPreimage,
    BeforeReplace,
    AfterReplace,
    AfterExchange,
    BeforeCapture,
    AfterCapture,
    BeforePublish,
    AfterPublish,
    BeforeTopologyMove,
    AfterTopologyMove,
    BeforeTopologyRollback,
    AfterTopologyRollback,
    BeforeIndex,
    AfterIndex,
    BeforeAck,
    AfterAck,
}
/// A narrow filesystem fault/observation boundary; all production durability steps still run.
pub trait MutationObserver: Send + Sync {
    fn checkpoint(&self, request: RequestId, checkpoint: MutationCheckpoint)
        -> std::io::Result<()>;
}
impl<F> MutationObserver for F
where
    F: Fn(RequestId, MutationCheckpoint) -> std::io::Result<()> + Send + Sync,
{
    fn checkpoint(
        &self,
        request: RequestId,
        checkpoint: MutationCheckpoint,
    ) -> std::io::Result<()> {
        self(request, checkpoint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory_retains_bytes(parent: &Path, bytes: &[u8]) -> bool {
        std::fs::read_dir(parent).unwrap().any(|entry| {
            let path = entry.unwrap().path();
            std::fs::read(path).is_ok_and(|actual| actual == bytes)
        })
    }

    #[test]
    fn publication_preserves_displaced_bytes_when_the_index_update_fails() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("note.md");
        let staged = temp.path().join("download.tmp");
        std::fs::write(&target, b"original").unwrap();
        let expected = LocalContentToken::capture(&target).unwrap();
        std::fs::write(&staged, b"remote").unwrap();

        let result = MutationCoordinator::default().publish_staged(
            uuid::Uuid::now_v7(),
            uuid::Uuid::now_v7(),
            &staged,
            &target,
            &expected,
            || -> CoreResult<()> { Err(CoreError::Database("injected index failure".into())) },
        );

        assert!(matches!(result, Err(CoreError::Database(_))));
        assert_eq!(std::fs::read(&target).unwrap(), b"remote");
        assert!(directory_retains_bytes(temp.path(), b"original"));
    }

    #[test]
    fn publication_preserves_an_external_edit_in_the_check_exchange_window() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("note.md");
        let staged = temp.path().join("download.tmp");
        std::fs::write(&target, b"original").unwrap();
        let expected = LocalContentToken::capture(&target).unwrap();
        std::fs::write(&staged, b"remote").unwrap();
        std::fs::write(&target, b"external edit after the final token check").unwrap();

        let result =
            publish_checked_stage(&staged, &target, &expected, uuid::Uuid::now_v7(), || Ok(()));

        assert!(matches!(result, Err(CoreError::StaleWrite { .. })));
        assert!(directory_retains_bytes(
            temp.path(),
            b"external edit after the final token check"
        ));
    }

    #[test]
    fn missing_publication_never_replaces_an_unexpected_destination() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("note.md");
        let staged = temp.path().join("download.tmp");
        std::fs::write(&staged, b"remote").unwrap();
        std::fs::write(&target, b"unexpected local note").unwrap();

        let result = publish_checked_stage(
            &staged,
            &target,
            &LocalContentToken::Missing,
            uuid::Uuid::now_v7(),
            || Ok(()),
        );

        assert!(matches!(result, Err(CoreError::StaleWrite { .. })));
        assert_eq!(std::fs::read(&target).unwrap(), b"unexpected local note");
        assert_eq!(std::fs::read(&staged).unwrap(), b"remote");
    }

    #[test]
    fn staged_publication_rejects_content_changed_after_staging() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("note.md");
        let staged = temp.path().join("download.tmp");
        std::fs::write(&target, b"original").unwrap();
        let expected = LocalContentToken::capture(&target).unwrap();
        std::fs::write(&staged, b"remote").unwrap();
        std::fs::write(&target, b"new local edit").unwrap();
        let id = uuid::Uuid::now_v7();

        let result = MutationCoordinator::default().publish_staged(
            uuid::Uuid::now_v7(),
            id,
            &staged,
            &target,
            &expected,
            || Ok(()),
        );

        assert!(matches!(result, Err(CoreError::StaleWrite { note_id }) if note_id == id));
        assert_eq!(std::fs::read(&target).unwrap(), b"new local edit");
        assert_eq!(std::fs::read(&staged).unwrap(), b"remote");
    }

    #[test]
    fn staged_publication_replaces_expected_content_and_runs_index_update() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("note.md");
        let staged = temp.path().join("download.tmp");
        std::fs::write(&target, b"original").unwrap();
        let expected = LocalContentToken::capture(&target).unwrap();
        std::fs::write(&staged, b"remote").unwrap();

        let result = MutationCoordinator::default()
            .publish_staged(
                uuid::Uuid::now_v7(),
                uuid::Uuid::now_v7(),
                &staged,
                &target,
                &expected,
                || {
                    assert_eq!(std::fs::read(&target).unwrap(), b"remote");
                    Ok(42)
                },
            )
            .unwrap();

        assert_eq!(result, 42);
        assert_eq!(std::fs::read(&target).unwrap(), b"remote");
        assert!(!staged.exists());
    }

    #[test]
    fn staged_publication_rejects_a_file_created_after_missing_token() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("note.md");
        let staged = temp.path().join("download.tmp");
        let expected = LocalContentToken::capture(&target).unwrap();
        std::fs::write(&staged, b"remote").unwrap();
        std::fs::write(&target, b"new local note").unwrap();

        let result = MutationCoordinator::default().publish_staged(
            uuid::Uuid::now_v7(),
            uuid::Uuid::now_v7(),
            &staged,
            &target,
            &expected,
            || -> CoreResult<()> { panic!("a stale publication must not update the index") },
        );

        assert!(matches!(result, Err(CoreError::StaleWrite { .. })));
        assert_eq!(std::fs::read(&target).unwrap(), b"new local note");
    }

    #[test]
    fn staged_publication_creates_an_expected_missing_file() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("note.md");
        let staged = temp.path().join("download.tmp");
        let expected = LocalContentToken::capture(&target).unwrap();
        std::fs::write(&staged, b"remote").unwrap();

        MutationCoordinator::default()
            .publish_staged(
                uuid::Uuid::now_v7(),
                uuid::Uuid::now_v7(),
                &staged,
                &target,
                &expected,
                || Ok(()),
            )
            .unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"remote");
        assert!(!staged.exists());
    }
}
