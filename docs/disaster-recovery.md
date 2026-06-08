# Disaster Recovery

S4Drive stores all user data needed for offline recovery inside the bucket
metadata prefix `.s4drive/`.

## What Must Be Preserved

- `.s4drive/content/blobs/` contains immutable file content blobs.
- `.s4drive/meta/snapshots/` contains compact file-tree snapshots.
- `.s4drive/meta/ops/` contains operations newer than or between snapshots.
- `.s4drive/meta/heads/current` points to the latest operation.

If these directories are present, files can be restored without the GUI app and
without a working local database.

## Restore From A Local Bucket Copy

First copy the bucket metadata prefix locally. For example:

```bash
aws s3 sync s3://MY_BUCKET/.s4drive ./s4drive-backup
```

Then restore the live file tree:

```bash
s4drive-cli restore-bucket --input ./s4drive-backup --output ./restored-files
```

The command also accepts a real `.s4drive` directory:

```bash
s4drive-cli restore-bucket --input ./.s4drive --output ./restored-files
```

## Behavior

- Restores the latest live file tree from the newest snapshot plus operation log
  tail when available.
- Copies blobs back to their original paths under `--output`.
- Refuses to write outside `--output`.
- Does not overwrite existing output files unless `--overwrite` is passed.
- If an output path already exists, writes a conflict-suffixed copy instead.
- Reports missing blobs and exits non-zero when the restore is incomplete.

Deleted files are not restored by default. If their tombstones and blobs still
exist under `.s4drive/trash/tombstones/` and `.s4drive/content/blobs/`, a future
restore mode can recover them explicitly.
