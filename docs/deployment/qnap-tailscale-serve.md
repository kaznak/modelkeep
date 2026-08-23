# QNAP Tailscale Serve ingress

The supported QNAP deployment exposes ModelKeep only inside the tailnet. Tailscale
runs as the official QNAP host application; it is intentionally not installed in the
ModelKeep container.

## Prerequisites

Install and connect Tailscale from QNAP App Center. Tailscale Services and application
capability forwarding require Tailscale 1.92 or newer. If App Center offers an older
version, follow Tailscale's official
[manual QPKG installation instructions](https://tailscale.com/docs/integrations/qnap#manual-installation-steps)
and select the package matching the NAS architecture from the
[stable QNAP package directory](https://pkgs.tailscale.com/stable/#qnap).
Install the downloaded `.qpkg` with App Center's **Install Manually** action; do not
replace only the CLI binary. Perform the update through the NAS LAN interface because
Tailscale connectivity can be interrupted while the package restarts.

Enable MagicDNS and HTTPS certificates for the tailnet. These are administrative
actions: QNAP must first be authenticated as a tailnet node, and enabling tailnet
HTTPS may require approval in the Tailscale admin console. The first `tailscale serve`
invocation can print a URL for that one-time approval.

SSH to the QNAP with an account allowed to administer the Tailscale QPKG and confirm
that the installed CLI has Serve support:

```sh
tailscale version
tailscale serve --help
```

If `tailscale` is not in `PATH`, locate the QPKG and invoke its binary explicitly:

```sh
getcfg Tailscale Install_Path -f /etc/config/qpkg.conf
```

For example, if that prints `/share/CACHEDEV1_DATA/.qpkg/Tailscale`, use
`/share/CACHEDEV1_DATA/.qpkg/Tailscale/tailscale` in the commands below.

## Start ModelKeep and Serve

The Compose mappings are deliberately bound to loopback. Port 8090 is the download
endpoint and port 8091 is the authenticated management API/UI. Start ModelKeep and
check both local backends before configuring ingress:

```sh
docker compose up -d
curl --fail http://127.0.0.1:8090/healthz
curl --fail http://127.0.0.1:8091/admin/
```

After confirming Tailscale 1.92 or newer, configure two persistent, tailnet-only HTTPS
services on the QNAP host so routine model downloads and privileged administration
have different hostnames and grants:

```sh
tailscale serve --service=svc:modelkeep --bg http://127.0.0.1:8090
tailscale serve --service=svc:modelkeep-admin --accept-app-caps=io.modelkeep/cap/admin --bg http://127.0.0.1:8091
tailscale serve status
```

Serve reports assigned URLs resembling:

```text
https://modelkeep.example-tailnet.ts.net
https://modelkeep-admin.example-tailnet.ts.net
```

Verify it from an allowed tailnet client and then use it as the Hugging Face endpoint:

```sh
curl --fail https://modelkeep.example-tailnet.ts.net/healthz
export HF_ENDPOINT=https://modelkeep.example-tailnet.ts.net
hf download Qwen/example-model
```

Open `https://modelkeep-admin.example-tailnet.ts.net/admin/` for archive inventory,
prefetch, refresh, verification, audit, and job status. The actual hostnames shown by
`tailscale serve status` are authoritative.

Grant ordinary clients access only to `svc:modelkeep`. Grant administrators access to
`svc:modelkeep-admin` and attach the `io.modelkeep/cap/admin` application capability.
The management Serve command forwards that authorized capability in the
`Tailscale-App-Capabilities` header. ModelKeep trusts this header only on its separate
loopback-published management listener; direct LAN publication of port 8091 would
break that trust boundary.

For example, after defining `group:modelkeep-admins`, the relevant grants are:

```json
{
  "grants": [
    {
      "src": ["autogroup:member"],
      "dst": ["svc:modelkeep"],
      "ip": ["tcp:443"]
    },
    {
      "src": ["group:modelkeep-admins"],
      "dst": ["svc:modelkeep-admin"],
      "ip": ["tcp:443"],
      "app": {
        "io.modelkeep/cap/admin": [{}]
      }
    }
  ]
}
```

Merge these with the tailnet's existing policy rather than replacing unrelated
grants. Use the policy editor's validation before saving.

If the installed QNAP Tailscale version does not yet support Services and application
capabilities, upgrade it. As a temporary fallback, set a strong
`MODELKEEP_ADMIN_TOKEN`, disable `MODELKEEP_TRUST_TAILSCALE_HEADERS`, proxy port 8091
with ordinary tailnet-only Serve, and enter the token in the UI. The browser stores it
only in the current tab's session storage.

Use Tailscale grants to restrict which users or devices may reach each HTTPS service;
do not rely on a broad allow-all tailnet policy. Download clients do not need a
separate ModelKeep password because their node identity and connection authorization
are enforced by Tailscale before Serve forwards the request. Never run `tailscale
funnel` for either ModelKeep service.

This does not replace Hugging Face upstream authentication. `HF_TOKEN`, when required
for a private or gated upstream repository, remains only in the ModelKeep deployment
environment. It is unrelated to tailnet client identity and must never be placed in
the Serve URL or tailnet policy.

## Boundary checks and recovery

From another LAN machine that is not using the tailnet, the direct backend must not
be reachable:

```sh
curl --connect-timeout 3 http://QNAP_LAN_ADDRESS:8090/healthz
curl --connect-timeout 3 http://QNAP_LAN_ADDRESS:8091/admin/
```

That command is expected to fail. If it succeeds, stop deployment and inspect the
effective Compose port mapping before use.

Background Serve configuration survives Tailscale restarts. After a QNAP reboot,
check both layers separately:

```sh
curl --fail http://127.0.0.1:8090/healthz
curl --fail http://127.0.0.1:8091/admin/
tailscale serve status
curl --fail https://modelkeep.example-tailnet.ts.net/readyz
```

To remove the HTTPS ingress without stopping ModelKeep, run:

```sh
tailscale serve --service=svc:modelkeep off
tailscale serve --service=svc:modelkeep-admin off
```
