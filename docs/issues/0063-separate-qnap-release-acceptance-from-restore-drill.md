---
status: in_progress
priority: P1
related_adrs:
  - ADR-0011
  - ADR-0013
created: 2026-09-23
updated: 2026-09-23
---
# Issue 0063: Separate QNAP release acceptance from the restore drill

- Status: In progress
- Priority: P1
- Related ADR: ADR-0011, ADR-0013

## Objective

Make the hardware acceptance record complete after ModelKeep-specific deployment
checks, while retaining the snapshot/backup restore phase as an optional, more
thorough QNAP disaster-recovery drill.

## Problem

The current `finish` command requires `post-restore`. That couples application
release acceptance to validation of QNAP snapshot, backup, ACL, and restore
configuration. Those are valuable operational checks, but they validate the site's
DR procedure rather than each ModelKeep release.

## Acceptance criteria

- Required release acceptance remains preflight, cold, warm, offline,
  post-container-restart, and post-qnap-reboot.
- `finish` succeeds when those required phases pass without `post-restore`.
- `post-restore` remains runnable with explicit upstream-blocked and restored-copy
  confirmations.
- A generated summary labels restore validation optional and records its result when
  it has been run.
- Existing schema-version-1 records remain readable.
- Documentation treats restore validation as an initial/configuration-change/periodic
  DR drill for operators who want the additional assurance.

## Verification

```sh
nix build .#checks.x86_64-linux.qnap-client-acceptance-tests --no-link
nix flake check
```

## Risks and assumptions

This changes completion policy, not any server or archive behavior. A passed release
acceptance record must not imply that the site's backup and restore procedure has
also been tested.
