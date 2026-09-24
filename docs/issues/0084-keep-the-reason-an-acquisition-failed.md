---
status: open
priority: P1
created: 2026-09-24
updated: 2026-09-24
---
# Issue 0084: Keep the reason an acquisition failed

- Status: Open
- Priority: P1
- Related ADR: None

## Objective

Make an acquisition failure say why, so the operationally meaningful classes AGENTS.md
requires are actually distinguishable instead of collapsing into one word.

## Problem

The reason is destroyed twice over, at two layers, before anyone can read it.

**The helper discards its own exception.** `upstream/hf_fetch.py` ends with:

```python
except (RepositoryNotFoundError,):
    sys.exit(11)
except GatedRepoError:
    sys.exit(12)
except HfHubHTTPError as error:
    ...
    sys.exit(1)
except (ConnectionError, TimeoutError):
    sys.exit(10)
except Exception:
    sys.exit(1)
```

Four cases get a code. Everything else exits `1` **with nothing printed at all** — no
message, no traceback, no typed event on the protocol channel it already owns.

**The parent discards what is left.** `src/upstream.rs` spawns the helper with
`stderr(Stdio::null())`, so any diagnostic the client wrote is gone before the parent could
see it. That choice exists to keep credentials and signed URLs out of the logs, which is
right; discarding the whole stream is how it was achieved.

So exit `1` becomes `UpstreamError::Failed`, becomes `error_class: "failed"`, and an
operator reads `"upstream acquisition failed"`. AGENTS.md's Error handling section asks for
upstream unavailable, upstream not found, authorization failure, integrity mismatch, invalid
range, unsafe path, disk full, interrupted, and internal index failure to be distinguished.
Three of those have exit codes. The rest are one bucket.

## What it cost, concretely

On 2026-09-24 a filtered prefetch of a Xet-backed 72 GB file failed on the deployment. The
job said `error_class: "failed"`. Establishing anything at all took: reading the container
logs, confirming the file was fetchable from elsewhere, checking whether the image carried
`hf_xet`, comparing the deployment against a local client, testing whether upstream worked
at all for that deployment, checking for an OOM kill, and finally running the helper by hand
inside the container — which printed the Xet runtime's own logs and then ended at exit `1`
with no error of its own. The reason is still unknown at the time of writing.

None of that work was necessary. The client raised an exception with a message; the helper
threw it away.

## Scope

- The helper reports its failure on the protocol channel as a typed event, carrying a class
  and a message it has sanitized itself — never a token, a signed URL, or a raw header.
- The parent maps that class onto the existing `UpstreamError` variants, adding variants only
  where an operator would act differently.
- The job record and the structured event carry the class and the sanitized message.
- Where the helper cannot classify, it still reports the exception type and a truncated,
  sanitized message rather than nothing.

## Do not

- log credentials, signed URLs, or authorization headers: the reason this stream is discarded
  today is a real constraint, not an accident;
- pipe the helper's stderr without draining it — the client integration harness deadlocked
  exactly that way on 2026-09-24, and the Xet runtime alone emits dozens of lines at startup;
- widen `UpstreamError` for classes nothing acts on.

## Acceptance criteria

- A failing acquisition records a class distinguishing at least: upstream unavailable, not
  found, unauthorized, rate limited, transport or client failure, and helper-contract
  failure.
- The sanitized message reaches the job record and the structured event.
- A test asserts that a token-shaped and a signed-URL-shaped string in an exception message
  do not reach the record or the log.
- A test drives a failure the current code reports as `failed` and asserts the specific class.
- The helper emitting no failure event at all is itself reported as a helper-contract failure
  rather than as a generic upstream failure.

## Verification

```sh
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

Add fixtures raising each class from the helper, and confirm the recorded class and message.
Confirm the credential-shaped strings are absent from both the job record and the event
stream.

## Risks and assumptions

The sanitization is the hard part, and it has to happen in the helper, where the exception
and its context are known, rather than by pattern-matching text in the parent. A class the
helper cannot determine must degrade to something honest rather than to a guess; reporting
"transport failure" for an authorization problem would be worse than reporting that the class
is unknown.
