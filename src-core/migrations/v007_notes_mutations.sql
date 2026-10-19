-- S4Drive Local Database Schema v7
-- Local Notes projection; bucket metadata keeps its existing schema.

CREATE TABLE IF NOT EXISTS notes_workspaces (
    workspace_id TEXT PRIMARY KEY NOT NULL,
    root_id TEXT,
    tree_generation INTEGER NOT NULL DEFAULT 0 CHECK (tree_generation >= 0),
    topology_hash TEXT
);

CREATE TABLE IF NOT EXISTS notes_generations (
    workspace_id TEXT NOT NULL,
    file_id TEXT NOT NULL,
    generation INTEGER NOT NULL DEFAULT 1 CHECK (generation >= 1),
    content_hash TEXT NOT NULL,
    PRIMARY KEY (workspace_id, file_id)
);

CREATE TABLE IF NOT EXISTS notes_mutations (
    workspace_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    file_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    stage TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (workspace_id, request_id)
);
