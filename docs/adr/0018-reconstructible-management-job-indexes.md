# ADR-0018: Reconstructible indexes for paged management job history

- Status: Accepted
- Date: 2026-09-23

## Context

Management jobs are durable JSON records used for operator history and restart
diagnostics. Loading every terminal record at startup makes memory and JSON decoding
cost grow with the full history, although the API and UI consume bounded pages.

SQLite would solve ordering and lookup, but adds a database lifecycle for metadata
that is already represented safely as ordinary JSON files.

## Decision

Keep each top-level `state/jobs/<job-id>.json` record as the durable job authority.
Maintain three credential-free, reconstructible filesystem indexes below
`state/jobs`:

- `by-created` entries encode `(created_at, job_id)` for deterministic bounded-memory
  page selection;
- `active` markers identify only queued or running records that need restart
  recovery;
- `idempotency` entries map a hashed key to its job and request hash without loading
  historical jobs into memory.

The service keeps only active jobs in memory. Terminal pages are selected by scanning
index entry names with a heap bounded by `limit + 1`, then only those JSON records are
decoded. Direct lookup validates the job ID and reads one regular JSON file.

An upgrade from the unindexed layout performs one full scan to build the indexes and
then records the completed migration. Index files never replace job JSON as the
authority and may be rebuilt. Model archive data is outside this indexing boundary.

## Consequences

- Normal startup and steady-state memory no longer scale with terminal job count.
- Directory enumeration remains proportional to index entry count until measurement
  justifies a different reconstructible index.
- The first upgrade startup performs a one-time full history scan.
- Manual removal of terminal JSON may leave harmless stale index entries; paging
  ignores missing records and stale idempotency entries are repaired when reused.
- This decision adds no automatic history deletion and no SQLite dependency.
