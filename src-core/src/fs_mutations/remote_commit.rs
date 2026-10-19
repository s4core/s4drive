//! `remote-commit.json`: which remote operations a local request tried to
//! commit. It lives in the request's journal directory, next to its intent,
//! so it survives the loss of the SQLite index.

use std::sync::Mutex;

use cap_std::fs::Dir;
use serde::{Deserialize, Serialize};

use super::atomic::{read_optional, replace_json};
use super::journal::MutationIntent;
use crate::error::{CoreError, CoreResult};
use crate::metadata::ops::CommitJournal;
use crate::metadata::tree::TombstoneVersion;
use crate::metadata::types::Operation;
use crate::notes::types::{RequestId, WorkspaceId};

const RECEIPT_FILE: &str = "remote-commit.json";
const RECEIPT_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Receipt {
    version: u32,
    workspace_id: WorkspaceId,
    request_id: RequestId,
    payload_hash: String,
    initial_base_head: Option<String>,
    attempts: Vec<Operation>,
    committed_op_id: Option<String>,
    /// The deletion a restore undoes, as observed before its commit.
    #[serde(default)]
    observed_deletion: Option<TombstoneVersion>,
}

pub struct RemoteCommitReceipt {
    dir: Dir,
    receipt: Mutex<Receipt>,
}

impl RemoteCommitReceipt {
    /// Open the receipt of `intent` in its journal directory `dir`. A receipt
    /// written for another workspace, request or payload is an error, never
    /// a reason to commit again.
    pub fn open(dir: Dir, intent: &MutationIntent) -> CoreResult<Self> {
        let receipt = match read_optional(&dir, RECEIPT_FILE)? {
            Some(bytes) => {
                let receipt: Receipt = serde_json::from_slice(&bytes).map_err(|e| {
                    CoreError::Protocol(format!("Invalid remote commit receipt: {e}"))
                })?;
                if receipt.version != RECEIPT_VERSION {
                    return Err(CoreError::Protocol(
                        "Unsupported remote commit receipt version".into(),
                    ));
                }
                if receipt.workspace_id != intent.workspace_id
                    || receipt.request_id != intent.request_id
                    || receipt.payload_hash != intent.payload_hash
                {
                    return Err(CoreError::Protocol(
                        "Remote commit receipt belongs to another request".into(),
                    ));
                }
                receipt
            }
            None => Receipt {
                version: RECEIPT_VERSION,
                workspace_id: intent.workspace_id,
                request_id: intent.request_id,
                payload_hash: intent.payload_hash.clone(),
                initial_base_head: None,
                attempts: Vec::new(),
                committed_op_id: None,
                observed_deletion: None,
            },
        };
        Ok(Self {
            dir,
            receipt: Mutex::new(receipt),
        })
    }

    fn update(&self, change: impl FnOnce(&mut Receipt)) -> CoreResult<()> {
        let mut receipt = self
            .receipt
            .lock()
            .map_err(|e| CoreError::Internal(e.to_string()))?;
        let mut next = receipt.clone();
        change(&mut next);
        let bytes = serde_json::to_vec(&next).map_err(|e| CoreError::Internal(e.to_string()))?;
        replace_json(&self.dir, RECEIPT_FILE, &bytes)?;
        *receipt = next;
        Ok(())
    }

    /// The deletion this restore observed before its first commit attempt.
    pub fn observed_deletion(&self) -> CoreResult<Option<TombstoneVersion>> {
        self.read(|receipt| receipt.observed_deletion.clone())
    }

    /// Record the deletion a restore undoes; the first observation is kept,
    /// so a retry never adopts a deletion made after the restore.
    pub fn record_observed_deletion(&self, deletion: &TombstoneVersion) -> CoreResult<()> {
        if self.read(|receipt| receipt.observed_deletion.is_some())? {
            return Ok(());
        }
        self.update(|receipt| receipt.observed_deletion = Some(deletion.clone()))
    }

    fn read<T>(&self, view: impl FnOnce(&Receipt) -> T) -> CoreResult<T> {
        self.receipt
            .lock()
            .map(|receipt| view(&receipt))
            .map_err(|e| CoreError::Internal(e.to_string()))
    }
}

impl CommitJournal for RemoteCommitReceipt {
    fn attempts(&self) -> CoreResult<Vec<Operation>> {
        self.read(|receipt| receipt.attempts.clone())
    }

    fn record_attempt(&self, operation: &Operation) -> CoreResult<()> {
        if self.read(|receipt| receipt.attempts.iter().any(|a| a.op_id == operation.op_id))? {
            return Ok(());
        }
        self.update(|receipt| {
            receipt
                .initial_base_head
                .get_or_insert_with(|| operation.base_head.clone());
            receipt.attempts.push(operation.clone());
        })
    }

    fn record_committed(&self, op_id: &str) -> CoreResult<()> {
        match self.read(|receipt| receipt.committed_op_id.clone())? {
            Some(known) if known == op_id => Ok(()),
            Some(known) => Err(CoreError::Protocol(format!(
                "request already committed operation {known}, not {op_id}"
            ))),
            None => self.update(|receipt| receipt.committed_op_id = Some(op_id.to_string())),
        }
    }

    fn committed(&self) -> CoreResult<Option<String>> {
        self.read(|receipt| receipt.committed_op_id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::types::{Effects, OpType, Preconditions};

    fn intent_dir() -> (tempfile::TempDir, Dir) {
        let temp = tempfile::tempdir().unwrap();
        let dir = Dir::open_ambient_dir(temp.path(), cap_std::ambient_authority()).unwrap();
        (temp, dir)
    }

    fn intent(payload: &str) -> MutationIntent {
        serde_json::from_value(serde_json::json!({
            "version": 1, "sequence": 1,
            "workspace_id": uuid::Uuid::nil(), "request_id": uuid::Uuid::nil(),
            "file_id": uuid::Uuid::nil(), "root_id": uuid::Uuid::nil(),
            "root_identity": "root", "payload_hash": payload,
            "expected_hash": null, "target_hash": null, "source_identity": null,
            "kind": "rename", "stage": "indexed", "created_at": "2026-10-09T00:00:00Z",
            "from_relative": null, "to_relative": "Notes/A", "temporary": ".t",
            "baseline": [], "changes": [], "token": null, "undo_id": null,
            "restore_of": null, "acknowledgement": null, "no_op": false
        }))
        .unwrap()
    }

    fn operation(op_id: &str) -> Operation {
        Operation {
            op_id: op_id.into(),
            device_id: uuid::Uuid::nil(),
            actor_id: "device".into(),
            logical_clock: 1,
            base_head: "base".into(),
            target_file_id: Some(uuid::Uuid::nil()),
            op_type: OpType::Rename,
            preconditions: Preconditions {
                expected_etag: None,
                expected_version_id: None,
                file_exists: true,
                parent_exists: true,
            },
            effects: Effects {
                new_revision_id: None,
                new_content_ref: None,
                new_name: Some("A".into()),
                new_parent_id: None,
                deleted: false,
            },
            timestamp: "2026-10-09T00:00:00Z".into(),
            signature: None,
        }
    }

    #[test]
    fn attempts_and_commit_survive_reopen() {
        let (_temp, dir) = intent_dir();
        let receipt = RemoteCommitReceipt::open(dir.try_clone().unwrap(), &intent("p")).unwrap();
        receipt.record_attempt(&operation("one")).unwrap();
        receipt.record_attempt(&operation("one")).unwrap();
        receipt.record_attempt(&operation("two")).unwrap();
        receipt.record_committed("two").unwrap();

        let reopened = RemoteCommitReceipt::open(dir, &intent("p")).unwrap();
        let ids: Vec<_> = reopened
            .attempts()
            .unwrap()
            .into_iter()
            .map(|op| op.op_id)
            .collect();
        assert_eq!(ids, ["one", "two"]);
        assert_eq!(reopened.committed().unwrap().as_deref(), Some("two"));
        assert!(reopened.record_committed("one").is_err());
    }

    #[test]
    fn receipt_of_another_payload_is_rejected() {
        let (_temp, dir) = intent_dir();
        let receipt = RemoteCommitReceipt::open(dir.try_clone().unwrap(), &intent("p")).unwrap();
        receipt.record_attempt(&operation("one")).unwrap();
        assert!(matches!(
            RemoteCommitReceipt::open(dir, &intent("other")),
            Err(CoreError::Protocol(_))
        ));
    }
}
