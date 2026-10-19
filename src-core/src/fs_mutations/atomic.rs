use crate::error::{CoreError, CoreResult};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use std::io::{self, Read, Write};
#[cfg(windows)]
use std::path::Path;

#[cfg(windows)]
pub const WINDOWS_ANCHOR: &str = ".s4drive-anchor";
#[cfg(windows)]
const WINDOWS_SHARE_READ_WRITE: u32 = 0x0000_0001 | 0x0000_0002;

pub fn io_error(error: io::Error) -> CoreError {
    CoreError::FileSystem(error.to_string())
}
pub fn sync_dir(dir: &Dir) -> CoreResult<()> {
    #[cfg(unix)]
    dir.open(".")
        .map_err(io_error)?
        .sync_all()
        .map_err(io_error)?;
    // Windows cannot open a directory as a file to flush it.
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}
pub fn write_exclusive(dir: &Dir, name: &str, bytes: &[u8]) -> CoreResult<()> {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    let mut file = dir.open_with(name, &options).map_err(io_error)?;
    file.write_all(bytes).map_err(io_error)?;
    file.sync_all().map_err(io_error)?;
    sync_dir(dir)
}
pub fn read_optional(dir: &Dir, name: &str) -> CoreResult<Option<Vec<u8>>> {
    let metadata = match dir.symlink_metadata(name) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_error(e)),
    };
    if metadata.is_symlink() {
        return Err(CoreError::SymlinkNotAllowed);
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let mut file = dir.open_with(name, &options).map_err(io_error)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(io_error)?;
    Ok(Some(bytes))
}
pub fn replace_json(dir: &Dir, name: &str, bytes: &[u8]) -> CoreResult<()> {
    let temporary = format!("{}.tmp", uuid::Uuid::now_v7());
    write_exclusive(dir, &temporary, bytes)?;
    dir.rename(&temporary, dir, name).map_err(io_error)?;
    sync_dir(dir)
}
/// No-replace rename publishes a single owned inode; existing user bytes are never replaced.
pub fn publish_new(stage_dir: &Dir, stage: &str, parent: &Dir, name: &str) -> CoreResult<()> {
    move_new(stage_dir, stage, parent, name)
}

/// Exchange leaves the actual displaced entry under the staging locator, even if a writer replaced its inode.
/// The caller records the armed inode before this syscall and synchronizes both directories afterward.
pub fn exchange(stage_dir: &Dir, stage: &str, parent: &Dir, name: &str) -> CoreResult<()> {
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))]
    return rustix::fs::renameat_with(
        stage_dir,
        stage,
        parent,
        name,
        rustix::fs::RenameFlags::EXCHANGE,
    )
    .map_err(|error| io_error(error.into()));
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    )))]
    {
        let _ = (stage_dir, stage, parent, name);
        Err(CoreError::MutationUnavailable)
    }
}

pub fn move_new(from: &Dir, name: &str, to: &Dir, target: &str) -> CoreResult<()> {
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))]
    rustix::fs::renameat_with(from, name, to, target, rustix::fs::RenameFlags::NOREPLACE).map_err(
        |e| {
            if e == rustix::io::Errno::EXIST {
                CoreError::NameExists(target.into())
            } else {
                io_error(e.into())
            }
        },
    )?;
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    )))]
    {
        let _ = (from, name, to, target);
        return Err(CoreError::FileSystem(
            "Safe folder moves are unavailable on this platform.".into(),
        ));
    }
    #[allow(unreachable_code)]
    {
        sync_dir(from)?;
        sync_dir(to)
    }
}

#[cfg(windows)]
fn windows_move_error(error: io::Error, target: &Path) -> CoreError {
    if error.kind() == io::ErrorKind::AlreadyExists
        || matches!(error.raw_os_error(), Some(80 | 183))
    {
        CoreError::NameExists(target.display().to_string())
    } else {
        io_error(error)
    }
}

/// Windows no-replace move using the audited `atomicwrites` wrapper. The caller
/// pins both parent directories with `hold_anchor` before entering this boundary.
#[cfg(windows)]
pub fn move_new_paths(from: &Path, to: &Path) -> CoreResult<()> {
    atomicwrites::move_atomic(from, to).map_err(|error| windows_move_error(error, to))
}

/// Windows replacement with durable displaced capture. It is intentionally two
/// no-replace moves: if the second move fails, both objects remain in journaled
/// locations and recovery can retry or create a conflict copy without deleting
/// a competing destination.
#[cfg(windows)]
pub fn exchange_paths(stage: &Path, target: &Path, displaced: &Path) -> CoreResult<()> {
    move_new_paths(target, displaced)?;
    move_new_paths(stage, target)
}

/// Keep a hidden system file in every Windows directory used as a publication
/// parent. A reparse point can only be installed on an empty directory, while
/// this handle is open without delete sharing for the whole rename boundary.
#[cfg(windows)]
pub fn hold_anchor(dir: &Dir) -> CoreResult<cap_std::fs::File> {
    use cap_std::fs::OpenOptionsExt;

    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No)
        // FILE_SHARE_READ | FILE_SHARE_WRITE; deliberately omit
        // FILE_SHARE_DELETE so the parent cannot be renamed or removed.
        .share_mode(WINDOWS_SHARE_READ_WRITE)
        // FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM. These are file
        // attributes, not access flags, and keep the marker out of normal UI.
        .custom_flags(0x0000_0002 | 0x0000_0004);
    match dir.open_with(WINDOWS_ANCHOR, &options) {
        Ok(file) => {
            file.sync_all().map_err(io_error)?;
            sync_dir(dir)?;
            Ok(file)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = dir.symlink_metadata(WINDOWS_ANCHOR).map_err(io_error)?;
            if metadata.is_symlink() {
                return Err(CoreError::SymlinkNotAllowed);
            }
            if !metadata.is_file() {
                return Err(CoreError::OutsideNotes);
            }
            let mut existing = OpenOptions::new();
            existing
                .read(true)
                .write(true)
                .follow(FollowSymlinks::No)
                .share_mode(WINDOWS_SHARE_READ_WRITE);
            let file = dir.open_with(WINDOWS_ANCHOR, &existing).map_err(io_error)?;
            if !file.metadata().map_err(io_error)?.is_file() {
                return Err(CoreError::OutsideNotes);
            }
            Ok(file)
        }
        Err(error) => Err(io_error(error)),
    }
}

pub fn read_body_optional(
    dir: &Dir,
    name: &str,
    limit: u64,
    validate: impl FnOnce(&cap_std::fs::Metadata) -> CoreResult<()>,
) -> CoreResult<Option<Vec<u8>>> {
    if dir.symlink_metadata(name).is_ok_and(|m| m.is_symlink()) {
        return Err(CoreError::SymlinkNotAllowed);
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32);
    }
    let file = match dir.open_with(name, &options) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error)),
    };
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file() {
        return Err(CoreError::OutsideNotes);
    }
    if metadata.len() > limit {
        return Err(CoreError::NoteTooLarge {
            size: metadata.len(),
            limit,
        });
    }
    validate(&metadata)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > limit {
        return Err(CoreError::NoteTooLarge {
            size: bytes.len() as u64,
            limit,
        });
    }
    Ok(Some(bytes))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_body_read_caps_growth_after_opened_metadata_validation() {
        let temp = tempfile::tempdir().unwrap();
        let dir = Dir::open_ambient_dir(temp.path(), cap_std::ambient_authority()).unwrap();
        std::fs::write(temp.path().join("note"), b"").unwrap();
        let limit = 64;
        let result = read_body_optional(&dir, "note", limit, |_| {
            std::fs::write(temp.path().join("note"), vec![b'X'; 4096]).unwrap();
            Ok(())
        });
        assert!(
            matches!(
                result,
                Err(CoreError::NoteTooLarge {
                    size: 65,
                    limit: 64
                })
            ),
            "growth must be bounded to limit + 1: {result:?}"
        );
    }
}
