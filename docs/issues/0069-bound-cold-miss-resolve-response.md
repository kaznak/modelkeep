---
status: done
priority: P0
related_adrs:
  - ADR-0005
  - ADR-0008
  - ADR-0017
created: 2026-09-24
updated: 2026-09-24
---
# Issue 0069: Return a bounded, observable response for a cold-miss resolve

- Status: Done
- Priority: P0
- Related ADR: ADR-0005, ADR-0008, ADR-0017

## Objective

A `resolve` request that misses the archive must reach an observable outcome within a
bounded and documented time. A caller must never be left with an open connection that
is indistinguishable from a hung or dead server.

## Problem

Observed against v0.4.7 on the QNAP deployment between 2026-09-23 22:19 and 22:28 JST.
A resolve of a file that is not archived returns no response headers and no body, and
the client gives up on its own timeout:

```text
GET /openai-community/gpt2/resolve/main/config.json        status=000 size=0 t=40s (also 45s/60s/90s)
GET /Qwen/Qwen3-Coder-Next-GGUF/resolve/main/README.md     status=000 size=0 t=40s
```

Both repositories exist upstream. In the same window a ranged read of an archived file
returned `206` in 49 ms and the Admin status route returned `200` in 35 ms, so serving
and the control plane were healthy; only the upstream acquisition path was affected.

The code path explains the observation as designed-in unbounded latency rather than an
accidental stall:

- `file_response` (`src/http.rs:407`) resolves locally, and on `NotFound` calls
  `PullThrough::ensure_for_type` on a blocking task (`src/http.rs:450`) before any
  response header is produced;
- `fetch_and_publish` (`src/pullthrough.rs:310`) discards the caller's requested-file
  list and passes `files: Vec::new()` to the helper (`src/pullthrough.rs:342`), which
  is ADR-0008's deliberate complete-snapshot rule;
- nothing in `src/http.rs`, `src/pullthrough.rs`, `src/upstream.rs`, or
  `src/singleflight.rs` applies a deadline to that wait.

So a single-file cold miss blocks the HTTP response for as long as the *entire*
repository takes to acquire, with no header, no status, and no progress channel. For
`Qwen/Qwen3-Coder-Next-GGUF` (469.92 GB) on the measured ~6 Mbps uplink that is on the
order of seven days of a headerless connection; for `openai-community/gpt2` it is
still far beyond any default client timeout.

Consequences:

- every client and reverse-proxy timeout turns into an unclassifiable failure, and
  `docs/modelkeep-api.md` documents status classes but no latency or liveness contract
  for a cold miss;
- an operator cannot distinguish "acquiring normally" from "stalled" from the caller's
  side;
- the single-file acquisition route is effectively unusable, which is also what makes
  [Issue 0070](0070-select-a-file-subset-for-acquisition.md) urgent.

A process-restart test was running during the observation window, so an additional
independent stall is not excluded. The unbounded-wait defect stands on its own; the
reproduction below must be repeated on a quiescent instance before concluding that no
second cause exists.

## Write scope

- the cold-miss response contract, recorded as a new ADR (today it is implicit);
- `src/http.rs` resolve/HEAD acquisition entry points;
- `src/pullthrough.rs` where a client-driven acquisition is started and joined;
- operational events for acquisition liveness;
- `docs/modelkeep-api.md` failure and latency interpretation.

## Do not touch

- archive representation, immutability, or deletion policy;
- redirecting or falling back to upstream/Xet for the payload (core invariant 10);
- publication of partial data in order to answer sooner.

## Acceptance criteria

- A cold-miss `GET`/`HEAD` resolve for a repository that exists upstream returns a
  documented status within a bounded, configurable time, or streams the file; it never
  holds a connection with no headers past that bound.
- The chosen behavior is validated black-box against both supported real
  `hf` / `huggingface_hub` client versions; the client's reaction to the chosen status
  is observed, not assumed, and preserved as an integration test.
- Acquisition started by a request that later times out or disconnects is handled by an
  explicit decision: either it continues and a subsequent request joins or resumes it
  under ADR-0017 staging identity, or it is aborted. A second full download of the same
  revision must not be started by a retry.
- Operational events distinguish waiting on a live acquisition from a stalled one:
  `archive_miss` is followed by acquisition progress carrying byte movement, or by a
  deadline/stall event. A constant byte counter is not reported as progress.
- No partial data becomes observable as a completed object when the request is cut off,
  and staging remains resumable rather than orphaned.
- `docs/modelkeep-api.md` states the cold-miss contract and the operator action it
  implies (submit a prefetch job, then read warm).

## Verification

```sh
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

Add black-box tests using a fixture upstream whose acquisition is slow or never
completes:

- a cold-miss resolve returns the documented class within the bound;
- concurrent cold misses for the same revision remain single-flight and are all
  bounded;
- an interrupted cold miss publishes nothing and leaves resumable staging;
- `HEAD` obeys the same bound as `GET`.

Run the supported real-client integration suite. Then repeat the original
reproduction on a quiescent QNAP instance with no restart test in flight, and record
the sanitized result here.

## Risks and assumptions

The correct status and retry behavior depend on what the supported clients do; this
must be evidence-driven per the protocol-compatibility rules and not guessed. Aborting
an in-flight acquisition on client disconnect can discard hours of transfer, so the
interaction with ADR-0010 and ADR-0017 staging reuse must be decided deliberately. The
bound does not by itself make large cold misses usable; that depends on acquisition
granularity, tracked in Issue 0070.

## Implementation status

Implemented on 2026-09-24. Verified on x86_64-linux with `cargo fmt --check`,
`cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-features`,
and `nix flake check`, each with its exit status taken directly rather than through a
pipe.

A cold-miss resolve answers `503` with `Retry-After` after a configurable deadline,
default eight seconds, while the acquisition continues on its own thread and a retry
joins it instead of starting a second one. Both pinned clients retry that status and
complete the download; the supported-client checks drive a real client through the
deadline and require exactly one upstream acquisition.

The metadata routes deliberately keep waiting by default. Measurement showed the
supported clients retry no status there and do not time out, so bounding them would have
turned a slow first download into an immediate failure for most repositories. An
operator can bound them with `MODELKEEP_METADATA_COLD_MISS_DEADLINE_SECONDS`, and the
documentation states the cost. The measurements are recorded in
[`hugging-face-cold-miss-client-behavior-2026-09-24.md`](../observations/hugging-face-cold-miss-client-behavior-2026-09-24.md).

The reported symptom had two causes. The larger one was that a single-file miss acquired
the whole repository, which Issue 0070 fixed.

`nix flake check` omits aarch64-linux as an incompatible system, so the QNAP release
architecture is covered by the native GitHub Actions jobs, not by this run.

**Remaining before this issue can close**: repeat the original reproduction on a
quiescent QNAP instance, with no restart test in flight, and record the sanitized result
here. That was not done in this work and cannot be done away from the deployment.

## Field verification (2026-09-24, v0.4.9)

The original reproduction was repeated on the QNAP deployment after a restart, with no
other test in flight, which is the quiescent condition this issue required.

```text
GET /openai-community/gpt2/resolve/main/config.json
  v0.4.7 as reported:  status=000  size=0    t=40.0s   (no response headers)
  v0.4.9 measured:     status=200  size=665  t=2.1s
```

The response carried a content-derived `ETag` and `x-repo-commit`. The repository was not
archived beforehand; afterwards the archive held exactly one file, `config.json`, while the
tree route reported all 26 files the commit contains. So the symptom is gone for the reason
expected: the request acquired the one path it asked for instead of the repository.

The larger of the two causes was the whole-repository acquisition fixed by Issue 0070. The
deadline itself did not come into play here, because acquiring 665 bytes finishes well inside
it; the bounded-response path remains covered by the deterministic checks rather than by this
measurement.

Nothing in this issue remains unverified. **Done.**
