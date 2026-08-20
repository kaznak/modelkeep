---
status: open
priority: P2
related_adrs:
  - ADR-0006
created: 2026-08-21
updated: 2026-08-21
---

# Issue 0011: Document QNAP archive permissions

- Status: Open
- Priority: P2
- Related ADR: ADR-0006

## Objective

Document and validate the host-side UID/GID and ACL requirements for the non-root QNAP deployment.

## Problem

The hardened container uses a non-root identity with a bind-mounted archive such as `/share/LLM/modelkeep:/data`. Container Station does not guarantee that the host directory is writable by the numeric UID/GID inside the image. A failed first deployment may tempt operators to run as root or make the share world-writable.

## Scope

* Document the image UID/GID and the QNAP-side access required for the archive.
* Identify which paths require write access and which state is durable.
* Provide a small permission preflight before a large import or fetch.
* Keep the container non-root and do not recommend `chmod 777`.
* Consider configurable PUID/PGID only if it does not materially complicate the minimal Nix image.

## Acceptance criteria

* QNAP deployment documentation includes a concrete permission preflight.
* The container remains non-root.
* No world-writable workaround is required.
* An unwritable archive produces a clear diagnostic before a large operation begins.
* Instructions match the Compose deployment.

## Verification

Review the Compose deployment and execute the documented preflight with the effective container UID/GID. Verify creation and removal of a tiny staging object under the configured archive temporary directory.

## Risks and assumptions

QNAP GUI ACLs and Unix numeric ownership may not map identically in every configuration. Document the tested deployment method rather than assuming one universal permission model.
