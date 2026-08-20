# Issue 0002: Stream large file responses

- Status: Open
- Priority: P1
- Related ADR: ADR-0001

## Objective

Serve large model files and byte ranges without buffering the entire response in
server memory.

## Problem

The current HTTP implementation reads the requested range into a `Vec<u8>` before
returning the response. Large model shards or broad ranges can therefore create high
memory pressure and reduce concurrency.

## Scope

- Use a streaming response backed by the archived file.
- Preserve `HEAD`, `Range`, `206`, `Content-Length`, `Content-Range`, `ETag`, and
  conditional request behavior.
- Keep path validation and immutable archive semantics unchanged.

## Acceptance criteria

- Memory usage is bounded independently of requested file size.
- Full-file and partial-file downloads remain byte-identical.
- HEAD requests do not open or stream a response body.
- A client can download a large fixture without an unbounded allocation.

## Verification

```sh
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
```

Add an integration test using a file larger than the configured stream buffer and a
Range request.

## Risks and assumptions

The implementation must avoid keeping a file descriptor or blocking file operation
alive longer than the response requires.
