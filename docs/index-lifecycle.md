# Derived-index lifecycle

Engram tapes are immutable, permanent, and the source of truth. The SQLite index is a disposable cache that is fully reconstructible from those tapes. Replacing the index therefore uses an explicit operator-controlled lifecycle:

**rebuild → validate → atomic swap → deliberate retire**

This is the only sanctioned way to replace the derived index. It is a manual runbook, not an `engram gc` operation, and no step touches a tape.

## L1. Rebuild in staging

Never rebuild the live index in place. The live database is contended by the watcher and read-path writes; the in-place attempt during T1766 aborted with `database is locked`.

Create a staging directory. T1766 used `~/t1766-staging/`, containing:

- an isolated config whose `db:` points to a staging index path; and
- the same immutable tape directories as the live configuration, provided by a symlink or config path.

Run a full ingest of the tape corpus against that isolated staging config. The live index and watcher continue serving throughout the rebuild.

## L2. Validate the staging index

Before any swap, run `PRAGMA quick_check` against the staging database. It must return `ok`.

Run representative sanity queries as well:

- compare tape and session counts with the tape-directory census;
- run a known-good `engram explain` and `engram grep`; and
- compare results with the live index where applicable.

Discard and rebuild a staging index that fails validation. It must never be swapped into the live path.

## L3. Swap atomically

Rename the live index to a labelled sibling before replacing it. T1766 used names shaped like:

- `index.sqlite.pre-<ticket>` for the retired live index; and
- `index.sqlite.during-rebuild` for an interim watcher database.

Then use `mv` to put the validated staging index at the live path. Both renames must remain on the same filesystem so they are atomic.

Restart the writer onto the new index using its existing supervision mechanism. Do not add, modify, load, or unload a launchd entry. During T1766, the pre-existing keepalive respawned the watcher; the launchd entry itself was not touched.

## L4. Retire deliberately

Keep retired index artifacts after the swap as rollback insurance. T1766 retained both `index.sqlite.pre-t1766` (10.5 GB) and `index.sqlite.during-rebuild` (9.7 GB).

Delete a retired index only through a deliberate, manual operator action after the replacement has proven itself in service. No Engram command, script, cron job, daemon, or `engram gc` invocation may delete it.

## L5. Preserve the invariants

- Tapes are never touched by any lifecycle step.
- The index is disposable and fully reconstructible from tapes.
- This lifecycle is the only sanctioned way to replace the index.
- Staging scaffolds, including config files and symlinks, contain no data and may be kept for audit.
