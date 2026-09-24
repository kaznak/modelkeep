---
status: open
priority: P0
related_adrs:
  - ADR-0003
  - ADR-0005
created: 2026-09-24
updated: 2026-09-24
---
# Issue 0086: Give the official client a writable cache

- Status: Open
- Priority: P0
- Related ADR: ADR-0003, ADR-0005

## Objective

Make the deployment able to acquire a Xet-backed file, by giving the client ModelKeep
delegates to a cache directory it can actually write.

## Problem

Every Xet-backed file fails on the deployment. Measured on 2026-09-24 with v0.4.9, by
running the official client inside the container:

```text
OSError: I/O error: I/O error: Permission denied (os error 13)
  at xet_get -> session.new_file_download_group(...)
```

The cause, also measured inside the container:

```text
/tmp:      mode=0o40755  uid=0      gid=0
/data:     mode=0o40755  uid=10001  gid=10001
process euid/egid: 10001 10001
HF_HOME: /tmp/huggingface
mkdir /tmp/huggingface/probe: FAILED PermissionError [Errno 13]
tmpfs free bytes: 8273739776
```

`compose.yaml` sets `HF_HOME: /tmp/huggingface` and declares `tmpfs: - /tmp` with no mode.
Docker mounts that tmpfs root-owned and `0755`, and the image runs as `10001:10001`
(`flake.nix`), so `HF_HOME` can never be created. Size is not the problem: 8.3 GB is free.

Nothing else noticed, because nothing else needs it. A file served from the CDN is written
straight into `local_dir` under `/data`, which the process owns. Only the Xet path keeps a
cache of its own, so only Xet-backed files fail.

That also explains the history. The same repository transferred 8.94 GB earlier the same
day; that was before a token was configured, and without a token the client had not taken
the Xet path at all.

## Why this is a product defect and not a site misconfiguration

ADR-0005 delegates upstream acquisition to the official client and ADR-0003 declines to
implement Xet. Delegating the work does not delegate the responsibility for the environment
the delegate needs. ModelKeep ships the image, ships the compose file, chooses the
non-root user, and chooses `HF_HOME`; the combination cannot work for the transport the Hub
uses for large files.

AGENTS.md already states the rule this breaks: container state belongs under explicit
mounted paths, not the image filesystem. `HF_HOME` points at neither a mounted path nor a
usable one.

## Scope

- Point the client's cache at a location the runtime user can write.
- Prefer a mounted path over the tmpfs, because the Xet chunk cache surviving a restart is
  what makes a resumed Xet transfer cheap, which is the property ADR-0017 exists to
  protect. `HF_XET_CACHE` scopes that to the Xet cache alone rather than moving all of
  `HF_HOME`.
- Give `/tmp` a mode the runtime user can write, so the next thing that needs a scratch
  directory does not fail the same way.
- Keep the credential out of it: the token arrives by environment and nothing should write
  it to the archive volume.

## Do not

- implement or work around Xet in ModelKeep (ADR-0003);
- run the container as root, or drop the read-only root filesystem, to sidestep this;
- disable Xet as the permanent answer. `HF_HUB_DISABLE_XET` is a usable fallback and worth
  documenting, but it forgoes the transport the Hub selects for exactly the files this
  archive is for.

## Acceptance criteria

- A Xet-backed file acquires successfully on a deployment built from this repository.
- The cache path is writable by the runtime user, is under a mounted path, and its location
  is documented with the deployment.
- `/tmp` in the shipped compose is writable by the runtime user.
- A check asserts the shipped compose gives the client a writable cache under a mounted
  path, and fails if that is removed. The existing `compose-network-boundary` check already
  asserts compose properties and is the natural place.
- The deployment documentation states which paths the client writes to and why, so an
  operator sizing the volume knows about them.

## Verification

```sh
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

The deterministic checks cannot exercise a real Xet transfer, which needs the network and a
token. What they can assert is the compose invariant. The transfer itself has to be
confirmed once on the deployment and the result recorded here.

## Risks and assumptions

A durable Xet cache consumes archive volume, bounded by the client's own retention settings
rather than by ModelKeep, so the deployment documentation has to say so and the operator has
to size for it. If that is unacceptable, the tmpfs with a usable mode is the alternative,
and the cost is that a restart makes a resumed Xet transfer re-fetch chunks — which should
be stated rather than discovered.

## Related

Issue 0084 is why this took an afternoon to find: the helper exits 1 without printing the
exception and the parent discards its stderr, so the deployment reported
`error_class: "failed"` and nothing else. The chain above was reconstructed by running the
client by hand inside the container.

## Field verification of the fix (2026-09-24, v0.4.9)

Setting `HF_XET_CACHE` to a path under the mounted archive volume, owned by the runtime
user, and recreating the container was sufficient. The same filtered prefetch that had
failed four times in a row then ran:

```text
transferring: 1   transferred_bytes: 9,051,739,286
job: running / downloading / resumed = true
progress: 9,051,739,286 / 73,025,919,893
```

The retained staging was adopted — `resumed: true` at the recorded 8,943,738,646 bytes —
and the transfer moved past it, which is what distinguishes a working Xet path from the
earlier failures that stopped at the first Xet-backed file.

So the diagnosis holds and the scope above is right: the client needs a writable cache, and
a mounted path is where it belongs. `HF_XET_CACHE` alone was enough for this transport;
whether anything else under `HF_HOME` needs to be writable is still unverified, and `/tmp`
is still mounted root-owned `0755` in the shipped compose, so the acceptance criteria stand
as written.

**What remains for this issue is the repository side, not the deployment**: the shipped
`compose.yaml` still sets `HF_HOME` to an unwritable path and declares `/tmp` without a
mode, so a deployment built from this repository still cannot acquire a Xet-backed file
until that is changed and a check pins it.
