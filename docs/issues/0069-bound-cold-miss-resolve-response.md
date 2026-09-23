---
status: open
priority: P0
related_adrs:
  - ADR-0005
  - ADR-0008
  - ADR-0017
created: 2026-09-24
updated: 2026-09-24
---
# Issue 0069: Return a bounded, observable response for a cold-miss resolve

- Status: Open
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
