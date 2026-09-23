---
name: modelkeep-admin
description: Operate a ModelKeep deployment through its Admin API for inventory, prefetch, refresh, verification, audit, and job monitoring. Use when an agent is asked to inspect or manage archived Hugging Face model or dataset snapshots; do not use it for deployment changes or archive deletion.
---

# ModelKeep Admin API operations

Read [`docs/admin-api.md`](../../../docs/admin-api.md) before calling the API. Treat it
as the route and contract reference.

- Obtain the management origin from a user-provided environment variable or ignored
  site configuration. Never invent, commit, or repeat a deployment hostname, token,
  operator identity, or raw private job record.
- Inspect service status and existing inventory before proposing a mutation. A `202`
  response means queued, not completed.
- Prefer immutable commit SHAs. Use mutable refs only when the user requests their
  resolution or refresh.
- Determine likely transfer/storage cost before a large prefetch and obtain user
  authorization when it is material. The API currently downloads the whole snapshot;
  do not imply that file patterns are supported.
- For mutations, send the CSRF header. For job submissions, also send a fresh
  idempotency key. Reuse a key only to retry the same uncertain HTTP submission; use a
  new key for an intentional new job.
- Poll the returned job ID at a bounded interval until a terminal state. Stop and
  report safe structured failure fields on `failed` or `cancelled`; do not blindly
  resubmit or expose raw upstream output.
- Do not cancel, restart containers, delete revisions, or change deployment/network
  configuration unless the user explicitly authorizes that separate action.
