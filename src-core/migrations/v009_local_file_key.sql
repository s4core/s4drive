-- Device and inode (volume serial and file index on Windows) of the local file
-- when it was last bound to its index entry. A rename keeps it, so a file that
-- moved can be told apart from a new file created at its old path.
ALTER TABLE objects ADD COLUMN local_file_key TEXT;
CREATE INDEX idx_objects_local_file_key ON objects(local_file_key);
