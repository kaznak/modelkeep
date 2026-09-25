---
status: open
priority: P3
related_adrs:
  - ADR-0022
created: 2026-09-26
updated: 2026-09-26
---
# Issue 0090: Consider reporting `lfs` from a recorded pointer size

- Status: Open
- Priority: P3
- Related ADR: ADR-0022

## Objective

Decide whether ModelKeep should report the Hub's `lfs` object on the metadata routes,
recording the pointer size it requires, so that clients naming cache blobs after
`lfs.oid` agree with clients naming them after the `ETag`.

## Problem

Reported from the GX10 deployment on 2026-09-26. llama.cpp, as read from its sources by
the reporter, names a cache blob after a `tree` entry's `lfs.oid` when present and its
`oid` otherwise. ModelKeep reports no `lfs` (ADR-0022 decision 7), so llama.cpp names the
blob after the git object id, while `huggingface_hub` names it after the `ETag`, which is
the sha256. A `~/.cache/huggingface` shared by both clients stores two copies of the same
file. Nothing fails.

ADR-0022 already names the prerequisite: an `lfs` object without `pointerSize` fails both
pinned clients, so reporting `lfs` faithfully requires recording a pointer size, which
is an addition to its decision 1 and a helper change. It also has to state what
`lfs.oid` is when upstream's LFS sha256 and ModelKeep's verified digest disagree.

## Acceptance criteria

- A decision recorded as an ADR superseding or refining ADR-0022 decision 7, or this
  issue closed with the reason it does not warrant one.

## Verification

Re-run the variant measurement in
`docs/observations/hugging-face-lfs-reporting-2026-09-24.md` for both pinned clients
before changing the response shape.

## Risks and assumptions

Duplicate blobs cost disk space only; no download is incorrect today.
