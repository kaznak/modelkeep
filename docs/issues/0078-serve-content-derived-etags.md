---
status: open
priority: P0
related_adrs:
  - ADR-0001
  - ADR-0008
created: 2026-09-24
updated: 2026-09-24
---
# Issue 0078: Serve content-derived ETags

- Status: Open
- Priority: P0
- Related ADR: ADR-0001, ADR-0008

## Objective

Stop advertising an ETag that two different files can share, so a client cannot store or
serve one file's bytes as another's.

## Problem

File responses advertise `ETag: "{commit}-{size}"` (`src/http.rs:668`). Within one
revision the commit is constant, so **any two files of the same byte length share an
ETag**. Sharded weights are where this bites, because shard sizes are usually uniform.

HTTP does not require an ETag to be unique across URLs, so this is not a bare protocol
violation. It is a compatibility defect against the service ModelKeep presents itself as.
The Hub returns content hashes as validators — the `x-linked-etag` below is a file's
sha256 — and `huggingface_hub` builds a content-addressed blob store on that property:
each blob is named after the validator, and `snapshots/<commit>/<path>` is a symlink to it.
Against the real Hub that assumption holds. Against ModelKeep it does not, so two paths
with one ETag become two symlinks to one blob and whichever file is fetched second
overwrites the first.

`If-None-Match` is compared against the same synthesised value (`src/http.rs:674`), so a
client holding one file's validator and requesting another is told `304 Not Modified`. For
a cache keyed by URL that is harmless, because validation is per URL; it matters here
because the same collision has already merged the two files in a validator-keyed blob
store. The fix has to make this comparison content-derived too, for the same reason the
header must be.

Nothing signals the problem. The client reports success, the file count and byte total are
right, and the job record looks clean.

## Reported evidence (2026-09-24)

Two shards of one revision, identical ETag from ModelKeep:

```text
model-00006-of-00018.safetensors  etag: "1d4bf0f2...-3979553696"
model-00008-of-00018.safetensors  etag: "1d4bf0f2...-3979553696"
```

Upstream distinguishes them by content hash:

```text
x-linked-etag: "0bc5214fac607f0e6cc92eec3789d4b8559410ef9fce66621ba8158e8410dae0"
x-linked-etag: "80b0c49033e9a0d5762562aa12f4acdb7f54da586f3d0110f28c48d91cf07892"
```

The client cache after a prefetch that reported success held 32 symlinks over 23 distinct
blobs, with one blob shared by six links and another by five. Nine of eighteen weight
shards held the wrong bytes.

**The archive itself is intact.** Slices taken at the same offset from the two shards
differ, so ModelKeep stores and serves them distinctly. The defect is confined to the
advertised validator, so nothing has to be re-fetched from upstream.

## Why the existing tests miss it

Every fixture file in the test suites has a distinct length, so no two of them can collide.
The defect needs two files of equal size in one revision to appear at all. This is the
failure mode Issue 0068 described: each layer is tested, and the combination that matters
is not.

## Scope

- Derive the advertised validator from content. The manifest already records a sha256 for
  every file, and the tree route already serves it as `oid` (`src/http.rs:495`), so the
  value needed is present and requires no new acquisition.
- Decide, **from measurement against both pinned clients**, which headers to serve and in
  what form: whether the `ETag` itself carries the digest, whether `x-linked-etag` is also
  required, and what shape each client accepts as a blob name. Do not infer this from the
  upstream examples above; observe it.
- Note that the Hub does not use one kind of validator for everything: an LFS-managed file
  carries its sha256, while a small non-LFS file carries a git blob hash. The manifest
  records a sha256 for every file and nothing records what upstream advertised, so the two
  candidate fixes are to record the upstream validator during acquisition and pass it
  through, or to serve the recorded sha256 for every file. The second needs no new
  acquisition state but has to be shown to satisfy both clients for non-LFS files as well.
- Make `If-None-Match` compare the same content-derived value, so a match means the same
  bytes.
- Consider exposing per-file digests in the job record so a corrupt fetch is detectable
  through the Admin API rather than by inspecting a client's cache.

## Acceptance criteria

- Two files of equal size in one revision are served with different validators, and a
  real supported client stores them as distinct blobs with correct contents.
- `If-None-Match` carrying one file's validator does not produce `304` for a different
  file.
- A regression fixture contains at least two same-size files in one revision, and it fails
  against the current implementation.
- Both pinned client versions download a sharded fixture and every file's bytes match what
  the archive holds, verified by digest rather than by size.
- Range and `HEAD` responses carry the same validator as the full response.
- The chosen header surface is documented in `docs/modelkeep-api.md`, with the measurement
  recorded as an upstream observation.
- Existing revisions keep working without re-acquisition: the fix reads digests the
  manifest already holds. A revision whose manifest predates recorded digests, if any such
  revision exists, is identified and handled explicitly rather than served with a
  colliding validator.

## Verification

```sh
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

Run the supported-client integration checks against a fixture with same-size shards, and
compare each downloaded file's digest with the archive's recorded digest. Confirm the new
test fails before the fix.

## Operational note

**A server-side fix does not repair client caches that are already corrupt.** Any client
that downloaded a sharded repository from this deployment before the fix may hold blobs
that contain another file's bytes, and its own integrity checks will not notice, because
the bytes match the ETag it was given. Those caches have to be cleared and re-downloaded.
The reliable detection is the symlink count against the distinct-blob count, or `du` on
`blobs/`; `ls -lL` and `find -L -type l` both pass on a corrupt tree because they resolve
the shared blob repeatedly.

## Risks and assumptions

Changing the advertised validator changes what clients consider cached, so clients will
re-download files they believe they already have. That is the correct outcome here, since
what they hold may be wrong, but it should be stated in the release notes. The header
shape is a compatibility surface; if the fix adds or changes headers beyond correcting the
ETag value, record the decision in an ADR rather than only in this issue.
