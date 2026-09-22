# QNAP client acceptance suite

Issue 0026 requires evidence from the actual QNAP and GX10 because generic CI cannot
certify Container Station, QNAP storage, Tailscale routing, reboot recovery, or a
site snapshot restore. Run this suite on the GX10 or another intended tailnet client.
It uses the real `hf download` command with a new temporary client cache for every
phase and writes evidence to one JSON record.

The suite is read-only with respect to server administration. It does not restart or
reboot QNAP, alter firewall rules, create snapshots, restore data, or delete either
the client cache or ModelKeep archive. Perform those explicit operator actions
between phases as described below.

## Prerequisites

- Clone the exact ModelKeep revision being accepted and install Nix on the client.
- For a new archive share, complete and remove the one-shot `compose.init.yaml`
  Application before deploying the normal `compose.yaml` Application.
- Deploy the same immutable image tag in both Compose files and record its `sha256:`
  digest.
- Configure and approve the download and administration Tailscale Services.
- Choose a small public model and an immutable 40-character commit that is not yet
  in ModelKeep. Do not use a mutable ref such as `main`.
- Run the LAN-boundary check from a client that can ordinarily route to the QNAP LAN
  address. An unreachable LAN is not evidence that a reachable LAN port is closed.

The management endpoint must authenticate this client through Tailscale. The suite
rejects bearer-only authentication so a missing application-capability grant cannot
be hidden by a token. The download phase removes client-side `HF_TOKEN` variables;
use a public acceptance model and keep upstream credentials on ModelKeep only.

## Initialize the record

Copy the tracked template to the Git-ignored local configuration path once:

```sh
cp docs/deployment/qnap-acceptance-config.example.json qnap-acceptance.config.json
```

Edit `qnap-acceptance.config.json` with the actual site values. It must not contain
credentials. In particular, set distinct download and administration HTTPS origins,
an actual small public repository and immutable commit, and the deployed image
digest. The repository root ignores both this local config and generated acceptance
records so private hostnames and site details cannot be committed accidentally.

Initialize the evidence record from that file:

```sh
nix run .#qnap-client-acceptance -- init qnap-acceptance.json
```

Use `--config <path>` only when the local configuration has a different name. The
record is updated atomically after every phase, including failed attempts. `init`
rejects a shared download/admin origin before any network checks, rather than later
reporting an opaque `/healthz` 404 from the administration listener.

## Run the phases

First verify tailnet HTTPS, Tailscale-authenticated management access, archive
readiness, and that direct LAN ports 8090 and 8091 reject connections:

```sh
nix run .#qnap-client-acceptance -- preflight qnap-acceptance.json
```

The cold phase verifies through the management API that the selected commit is not
already archived. It then runs `hf download`, verifies publication of that exact
commit, records SHA-256 for every downloaded model file, and checks `HEAD` and a byte
Range against one downloaded file:

```sh
nix run .#qnap-client-acceptance -- cold qnap-acceptance.json
nix run .#qnap-client-acceptance -- warm qnap-acceptance.json
```

The warm phase uses another empty temporary HF cache and requires byte-identical
output. Next, block Hugging Face/upstream access from the QNAP while preserving
tailnet access from the client. Verify the block independently; the client cannot
observe the QNAP's outbound firewall directly. The confirmation flag records that
operator assertion and prevents an accidental online test from being labelled
offline:

```sh
nix run .#qnap-client-acceptance -- offline qnap-acceptance.json \
  --confirm-upstream-blocked
```

Restore normal outbound policy. Restart only the Container Station application,
wait for it to become healthy, and run:

```sh
nix run .#qnap-client-acceptance -- post-container-restart qnap-acceptance.json
```

Reboot QNAP, wait for Container Station and both approved Tailscale Services to
recover, and run:

```sh
nix run .#qnap-client-acceptance -- post-qnap-reboot qnap-acceptance.json
```

Finally perform the snapshot/backup restore drill from
[qnap-production-runbook.md](qnap-production-runbook.md): restore into a new empty
share, point the same pinned image and Tailscale Services at that copy, omit fetch
credentials/helpers, and block upstream. After independently confirming that the
restored copy is active:

```sh
nix run .#qnap-client-acceptance -- post-restore qnap-acceptance.json \
  --confirm-upstream-blocked \
  --confirm-restored-copy
```

Return the production service to its intended archive and network policy. Render a
human-readable record only after all required phases have passed:

```sh
nix run .#qnap-client-acceptance -- finish qnap-acceptance.json \
  --output qnap-acceptance.md
```

Retain both JSON and Markdown records with the deployment record. Convert any failed
hardware behavior into a focused issue; do not edit a failed phase to `passed`.
