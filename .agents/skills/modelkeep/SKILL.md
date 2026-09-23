---
name: modelkeep
description: Use a ModelKeep deployment through its Hugging Face-compatible download endpoint and Admin API for model or dataset retrieval, inventory, prefetch, refresh, verification, audit, and job monitoring. Use when an agent needs to retrieve or manage archived Hugging Face snapshots; do not use it for deployment changes or archive deletion.
---

# ModelKeep operations

Read [`docs/modelkeep-api.md`](../../../docs/modelkeep-api.md) to choose the correct
interface. For management operations, also read
[`docs/admin-api.md`](../../../docs/admin-api.md) before calling the Admin API.

## Route the task

- For normal model or dataset retrieval, use a supported official `hf` or
  `huggingface_hub` client with the download origin. Do not hand-code the Hub protocol
  unless diagnosing compatibility and do not implement a fallback that bypasses
  ModelKeep.
- For inventory, explicit prefetch or refresh, verification, audit, and job status,
  use the separate versioned Admin API.
- Obtain each origin from a user-provided environment variable or ignored site
  configuration. Never use the Admin origin as `HF_ENDPOINT`, or invent, commit, or
  repeat a deployment hostname, token, operator identity, or raw private job record.

A warm download reads the archive, but a cold download can acquire and permanently
archive a complete upstream snapshot. Determine likely transfer and storage cost
before requesting an unknown large repository and obtain user authorization when it
is material. The current acquisition path does not support allow/ignore patterns.

## Admin operation safety

- Inspect service status and existing inventory before proposing a mutation. A `202`
  response means queued, not completed.
- Prefer immutable commit SHAs. Use mutable refs only when the user requests their
  resolution or refresh.
- For mutations, send the CSRF header. For job submissions, also send a fresh
  idempotency key. Reuse a key only to retry the same uncertain HTTP submission; use a
  new key for an intentional new job.
- Poll the returned job ID at a bounded interval until a terminal state. Stop and
  report safe structured failure fields on `failed` or `cancelled`; do not blindly
  resubmit or expose raw upstream output.
- Do not cancel, restart containers, delete revisions, or change deployment or network
  configuration unless the user explicitly authorizes that separate action.
