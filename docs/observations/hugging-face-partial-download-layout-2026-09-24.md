# Hugging Face partial-download layout observation — 20260924+0900

This is an upstream client observation, not a ModelKeep policy definition. It records
where the pinned `huggingface_hub` puts a partially downloaded file when `snapshot_download`
is given a `local_dir`, because Issue 0082 has to decide from measurement why a running
acquisition's reported progress under-reported on one measured transfer and tracked exactly
on another.

Upstream behavior can change. Re-observe before changing how ModelKeep counts bytes in
flight.

## Environment

- `huggingface_hub` `1.27.0`, built from `flake.nix` (`nix develop`), so the version is the
  deployed one
- Two ways of observing, kept apart below: asking the client's own path computation where a
  partial file goes, and watching a real `snapshot_download` while it ran
- `HF_HUB_DISABLE_XET=1` for the live run, to observe the plain HTTP path rather than Xet
- Observed while implementing Issue 0082

## What was observed

### A partial file lives beside its metadata sidecar, mirroring the repository's tree

`_local_folder.get_local_download_paths(local_dir, filename).incomplete_path(etag)` was asked
for four relative paths. Paths below are relative to
`<local_dir>/.cache/huggingface/download`.

| relative path in the repository | incomplete path |
|---|---|
| `config.json` | `8_PA_wEVGiVa2goH2H4KQOQpvVY=.deadbeef.incomplete` |
| `model-00001-of-00002.safetensors` | `aoe4E07IMh7reFyUkVoVk040mQk=.deadbeef.incomplete` |
| `onnx/model.onnx` | `onnx/ihhw_uFzBe-Y54_HOJQmXx4GS8A=.deadbeef.incomplete` |
| `onnx/quantized/model.onnx` | `onnx/quantized/ihhw_uFzBe-Y54_HOJQmXx4GS8A=.deadbeef.incomplete` |

So a file at the repository root has its partial data directly under `download/`, and a file
in a subdirectory has it under a subdirectory of the same shape. The name is a short hash of
the metadata file's name, then the etag, then `.incomplete`.

### A live transfer agrees

`hf-internal-testing/tiny-random-gpt2` was downloaded with `local_dir` and `max_workers=8`
while a watcher thread listed `download/` every 10 ms. Ten distinct `.incomplete` files were
seen across 247 samples, all directly under `download/`, which is consistent with that
repository having every file at its root.

## What this establishes for ModelKeep, and what it does not

ModelKeep's helper counts bytes in flight with

```python
download_metadata.glob("*.incomplete")
```

which is not recursive.

**Established: a selection whose large files live in subdirectories has none of its in-flight
bytes counted.** Two of the four paths above are invisible to that glob. This is a defect
reachable by inspection, independent of any deployment measurement.

**Established negatively: this does not explain Measurement A.** Issue 0082's Measurement A
was a prefetch of `Qwen/Qwen2.5-3B-Instruct`, whose shards are at the repository root, so the
glob saw them. The roughly 2.68 GB that the reported figure was missing there came from
somewhere else, and this observation rules out one candidate rather than finding the cause.

**Not established: where those bytes were.** The measurement that would settle it is a
transfer of the same shape with Xet disabled and then enabled, inspecting the staging
directory's actual size rather than the archive filesystem's free space — the shared-volume
comparison is what made the original report wrong. That needs the deployment and is not done.

## Method note

The table was produced by asking the client for the path, not by racing a live transfer to
observe it. A live run only samples whatever window it happens to catch, and the root-only
repository used here would never have exercised the subdirectory case at all — its watcher
reported nothing missed. Asking the path computation is decisive where sampling is not.

The client's `get_local_download_paths` is public API on `_local_folder`; nothing private was
called. ModelKeep still depends on where the client puts partial data, which is exactly the
kind of coupling `docs/testing-strategy.md` asks to pin with an observation rather than
assume.
