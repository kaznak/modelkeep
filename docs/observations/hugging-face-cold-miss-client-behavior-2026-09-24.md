# Hugging Face client cold-miss behavior observation — 2026-09-24 JST

This is an upstream client observation, not a ModelKeep policy definition. It records
how the supported `huggingface_hub` versions react when ModelKeep answers a request it
cannot serve yet, because a mirror that acquires on demand must sometimes say "not
ready" and the useful answer depends entirely on what the client does next.

Upstream behavior can change. Re-observe before changing the cold-miss contract.

## Environment

- `huggingface_hub` `0.36.0` and `1.27.0`, the two versions pinned by `flake.nix`
- Local stub upstream and a local ModelKeep; no public repository was downloaded
- Observed while implementing Issue 0069

## What was observed

### The resolve route

Both versions abandon a resolve `HEAD` after ten seconds. Against an eleven-second
delayed response, `0.36.0` raises `ReadTimeoutError (read timeout=10)` six times and
then `LocalEntryNotFoundError` about eighty-three seconds in.

This is the reported symptom. A client that gives up at ten seconds while the server is
still acquiring produces exactly the `status=000` with a zero-byte body recorded in the
v0.4.7 field report, with no way for the caller to tell it apart from a dead server.

Retry behavior by status on this route:

| Status | `0.36.0` | `1.27.0` |
| --- | --- | --- |
| `503` | retries, backs off 1, 2, 4, 8, 8 s, ignores `Retry-After` | retries, honours `Retry-After` |
| `504` | retries | retries |
| `429` | does not retry | retries |
| `425` | does not retry | does not retry |
| `500` | not measured | retries |

### The metadata routes

`revision` and `tree` behave differently, and the difference matters.

Neither version retries **any** status on these routes: `503`, `429`, `425`, `500` and
`504` all fail immediately. Nor does the ten-second read timeout apply. `0.36.0` was
still waiting at thirty seconds; `1.27.0` waited twelve seconds for a held request and
completed it, and was not measured beyond that. Neither was observed to give up.
`1.27.0` uses both `revision` and `tree`.

So on the metadata routes a slow acquisition is not a reported failure at all: the
client waits and the download eventually succeeds. Answering "not ready" there would
convert a slow success into an immediate failure, with no status available that buys a
retry.

### A cancelled acquisition

Measured against the deployment on 2026-09-24 with v0.4.9, by starting a request for a file
the archive did not hold and cancelling the acquisition it triggered from the admin plane.

A waiting request is released with `502` the moment the cancellation lands, not at the
deadline. Measured at 3.10 s against a 8 s deadline, for a `GET` on a ref-addressed URL and
for a `HEAD` on a commit-addressed URL alike, so neither the method nor the URL form changes
it.

`huggingface_hub` 1.27.0 does **not** stop there. Driving the same request through the real
client, the cancellation released its `HEAD` and the client retried on its own, hit the
deadline on a later attempt — reporting `HTTP Error 503 ... Rate limited. Waiting 9.0s
before retry [Retry 1/5]` — and then completed the download, 35.5 s in total.

That is the documented behaviour rather than a defect: a re-request legitimately starts new
work. What it means operationally is worth stating plainly, because it is easy to assume the
opposite. **Cancelling a client-driven acquisition does not stop the client.** It stops the
transfer that was in flight; a supported client will ask again within seconds and the work
restarts. Stopping the client is a separate action, and the in-flight view is how an operator
sees that an acquisition they cancelled has come back.

## Consequence for ModelKeep

The two routes need opposite defaults, which is not something to infer from the
protocol alone:

- a cold-miss resolve is bounded and answers `503` with `Retry-After`, because both
  clients retry it and neither will wait;
- a cold-miss metadata request keeps waiting by default, because both clients will wait
  and neither will retry.

## Reproducing

The matrix above was measured by hand against both pinned clients while implementing
Issue 0069. Be precise about what is pinned and what is not:

- the ModelKeep side of the contract — which status is answered, within which deadline,
  and that the acquisition keeps running and is joined by a retry — is held by the
  regression tests in `src/http.rs`. Those drive ModelKeep with a stub, not a real
  client, so they pin ModelKeep's behavior and not the client's.
- the client side is pinned for the row that the contract depends on: the
  `hf-client-integration-0-36` and `hf-client-integration-1-27` checks drive a real
  client through a deadline-exceeded `503` and require it to retry on its own and
  complete the download, having caused exactly one upstream acquisition. The remaining
  rows of the matrix are not pinned by any check; they were measured by hand for this
  record.

Treat every row above as an observation that can go stale, and re-measure before relying
on it. The supported-client checks are run with the exit status taken directly, not
through a pipe:

```sh
nix build --no-link '.#checks.x86_64-linux.hf-client-integration-1-27' > log 2>&1; echo $?
```
