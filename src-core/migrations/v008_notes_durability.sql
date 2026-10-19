-- Durable journal projections; payloads and restore manifests remain on disk.
CREATE TABLE local_mutations (
    request_id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL, file_id TEXT NOT NULL,
    kind TEXT NOT NULL, stage TEXT NOT NULL, journal_path TEXT NOT NULL, created_at TEXT NOT NULL
);
CREATE INDEX idx_local_mutations_workspace_stage_created ON local_mutations(workspace_id,stage,created_at);
ALTER TABLE notes_generations RENAME TO note_local_versions;
CREATE TABLE notes_roots (
    workspace_id TEXT PRIMARY KEY, root_file_id TEXT NOT NULL, availability TEXT NOT NULL
);
INSERT INTO notes_roots SELECT workspace_id,root_id,'ready' FROM notes_workspaces WHERE root_id IS NOT NULL;
ALTER TABLE notes_workspaces DROP COLUMN root_id;
ALTER TABLE notes_mutations RENAME TO legacy_notes_mutations;

-- Supplemental entry fields only; placement/hash/current revision remain canonical in objects.
ALTER TABLE objects ADD COLUMN entry_metadata TEXT;
