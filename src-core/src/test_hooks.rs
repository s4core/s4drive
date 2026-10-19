//! Test-only checkpoints for system tests; compiled only with the
//! `notes-test-hooks` feature, so release builds hold none of this code.
//!
//! The test environment selects one checkpoint. When a process reaches it, the
//! process writes `ready.json` into the gate directory and waits for a
//! `release` file. In that window the test acts (saves, kills the process) at
//! a known point, instead of guessing with sleeps. A checkpoint fires once per
//! process; later passes go through, so a released test continues normally.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::error::{CoreError, CoreResult};

/// Private directory of one gate; must not hold a `ready.json` of an earlier test.
pub const GATE_DIR_ENV: &str = "S4_E2E_GATE_DIR";
/// Exact checkpoint name, e.g. `upload_after_blob_before_generation_recheck`.
pub const CHECKPOINT_ENV: &str = "S4_E2E_CHECKPOINT";
/// Optional filters: the gate fires only for this file and/or request.
pub const FILE_ID_ENV: &str = "S4_E2E_FILE_ID";
pub const REQUEST_ID_ENV: &str = "S4_E2E_REQUEST_ID";
/// Seconds to wait for the release before failing (default 120).
pub const TIMEOUT_ENV: &str = "S4_E2E_GATE_TIMEOUT";

pub const UPLOAD_AFTER_BLOB: &str = "upload_after_blob_before_generation_recheck";
pub const DOWNLOAD_AFTER_STAGE: &str = "download_after_stage_before_publish";
pub const REMOTE_AFTER_OP: &str = "remote_after_op_before_head_cas";
pub const REMOTE_AFTER_HEAD_CAS: &str = "remote_after_head_cas_before_receipt";
pub const INTAKE_AFTER_COMMIT: &str = "intake_after_commit_before_local_marker";
pub const REMOTE_POLL_BEFORE_OP: &str = "remote_poll_before_op";
/// Filtered by the file's key relative to the sync folder, not by an id.
pub const SCAN_BEFORE_NOTES_CHECK: &str = "scan_before_notes_check";

const POLL: Duration = Duration::from_millis(50);

#[derive(Debug)]
struct Gate {
    dir: PathBuf,
    checkpoint: String,
    file_id: Option<String>,
    request_id: Option<String>,
    timeout: Duration,
    fired: AtomicBool,
}

impl Gate {
    fn from_env() -> Option<Self> {
        let var = |name| std::env::var(name).ok().filter(|v: &String| !v.is_empty());
        Some(Self {
            dir: PathBuf::from(var(GATE_DIR_ENV)?),
            checkpoint: var(CHECKPOINT_ENV)?,
            file_id: var(FILE_ID_ENV),
            request_id: var(REQUEST_ID_ENV),
            timeout: Duration::from_secs(
                var(TIMEOUT_ENV).and_then(|v| v.parse().ok()).unwrap_or(120),
            ),
            fired: AtomicBool::new(false),
        })
    }

    /// Whether this pass is the one the test waits for. Claims the gate.
    fn claims(&self, checkpoint: &str, file_id: Option<&str>, request_id: Option<&str>) -> bool {
        let matches = |wanted: &Option<String>, actual: Option<&str>| {
            wanted.as_deref().is_none_or(|w| actual == Some(w))
        };
        self.checkpoint == checkpoint
            && matches(&self.file_id, file_id)
            && matches(&self.request_id, request_id)
            && !self.fired.swap(true, Ordering::SeqCst)
    }

    fn announce(&self, file_id: Option<&str>, request_id: Option<&str>) -> CoreResult<()> {
        let ready = serde_json::json!({
            "checkpoint": self.checkpoint,
            "file_id": file_id,
            "request_id": request_id,
            "pid": std::process::id(),
            "attempt": 1,
        });
        let target = self.dir.join("ready.json");
        if target.exists() {
            return Err(CoreError::Internal(format!(
                "test gate {} was already used; every attempt needs a fresh gate",
                self.dir.display()
            )));
        }
        let staged = self.dir.join(format!("ready.{}.tmp", std::process::id()));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)
            .map_err(|e| gate_error(&self.dir, e))?;
        file.write_all(ready.to_string().as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|e| gate_error(&self.dir, e))?;
        fs::rename(&staged, &target).map_err(|e| gate_error(&self.dir, e))
    }

    fn released(&self) -> bool {
        self.dir.join("release").exists()
    }

    fn timed_out(&self) -> CoreError {
        CoreError::Internal(format!(
            "test checkpoint {} was not released within {}s",
            self.checkpoint,
            self.timeout.as_secs()
        ))
    }
}

fn gate_error(dir: &Path, error: std::io::Error) -> CoreError {
    CoreError::Internal(format!("test gate {}: {error}", dir.display()))
}

/// The gate of this process. Until the environment names one, every call
/// reads it again, so a test may choose its checkpoint after setup syncs.
fn gate() -> Option<&'static Gate> {
    static GATE: OnceLock<Gate> = OnceLock::new();
    if let Some(gate) = GATE.get() {
        return Some(gate);
    }
    let gate = Gate::from_env()?;
    Some(GATE.get_or_init(|| gate))
}

/// The gate this pass must wait at, already announced, with its deadline.
fn arm(
    checkpoint: &str,
    file_id: Option<&str>,
    request_id: Option<&str>,
) -> CoreResult<Option<(&'static Gate, Instant)>> {
    let Some(gate) = gate().filter(|g| g.claims(checkpoint, file_id, request_id)) else {
        return Ok(None);
    };
    gate.announce(file_id, request_id)?;
    Ok(Some((gate, Instant::now() + gate.timeout)))
}

/// Wait at `checkpoint` if the test environment selected it. A missing
/// release is an error, never a silent continue.
pub async fn checkpoint(
    checkpoint: &str,
    file_id: Option<&str>,
    request_id: Option<&str>,
) -> CoreResult<()> {
    let Some((gate, deadline)) = arm(checkpoint, file_id, request_id)? else {
        return Ok(());
    };
    while !gate.released() {
        if Instant::now() >= deadline {
            return Err(gate.timed_out());
        }
        tokio::time::sleep(POLL).await;
    }
    Ok(())
}

/// `checkpoint` for synchronous code that already runs off the async runtime,
/// such as a local mutation observer.
pub fn checkpoint_blocking(
    checkpoint: &str,
    file_id: Option<&str>,
    request_id: Option<&str>,
) -> CoreResult<()> {
    let Some((gate, deadline)) = arm(checkpoint, file_id, request_id)? else {
        return Ok(());
    };
    while !gate.released() {
        if Instant::now() >= deadline {
            return Err(gate.timed_out());
        }
        std::thread::sleep(POLL);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate_in(dir: &Path, timeout: Duration) -> Gate {
        Gate {
            dir: dir.to_path_buf(),
            checkpoint: UPLOAD_AFTER_BLOB.into(),
            file_id: Some("file-a".into()),
            request_id: None,
            timeout,
            fired: AtomicBool::new(false),
        }
    }

    #[test]
    fn gate_fires_once_and_only_for_its_checkpoint_and_file() {
        let temp = tempfile::tempdir().unwrap();
        let gate = gate_in(temp.path(), Duration::from_secs(1));
        assert!(!gate.claims(DOWNLOAD_AFTER_STAGE, Some("file-a"), None));
        assert!(!gate.claims(UPLOAD_AFTER_BLOB, Some("file-b"), None));
        assert!(gate.claims(UPLOAD_AFTER_BLOB, Some("file-a"), Some("any")));
        assert!(!gate.claims(UPLOAD_AFTER_BLOB, Some("file-a"), None));
    }

    #[test]
    fn ready_marker_names_the_point_and_a_reused_gate_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let gate = gate_in(temp.path(), Duration::from_secs(1));
        gate.announce(Some("file-a"), Some("req")).unwrap();
        let ready: serde_json::Value =
            serde_json::from_slice(&fs::read(temp.path().join("ready.json")).unwrap()).unwrap();
        assert_eq!(ready["checkpoint"], UPLOAD_AFTER_BLOB);
        assert_eq!(ready["file_id"], "file-a");
        assert_eq!(ready["pid"], std::process::id());
        assert!(gate.announce(Some("file-a"), None).is_err());
    }

    #[test]
    fn an_unreleased_gate_fails_instead_of_continuing() {
        let temp = tempfile::tempdir().unwrap();
        let gate = gate_in(temp.path(), Duration::from_millis(100));
        assert!(!gate.released());
        let error = gate.timed_out();
        assert!(error.to_string().contains(UPLOAD_AFTER_BLOB));
        fs::write(temp.path().join("release"), b"").unwrap();
        assert!(gate.released());
    }
}
