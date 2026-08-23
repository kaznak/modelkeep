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

## Working configuration

The verified deployment uses two Tailscale Services hosted by the tagged QNAP node:

| Purpose | Tailscale Service | QNAP backend | Authorization |
|---|---|---|---|
| Hugging Face downloads | `svc:modelkeep` | `127.0.0.1:8090` | Service network grant |
| Management API/UI | `svc:modelkeep-admin` | `127.0.0.1:8091` | Service network grant plus `io.modelkeep/cap/admin` |

Do not use plain `tailscale serve --bg http://127.0.0.1:8090` on the QNAP. Without
`--service`, Serve configures the QNAP device hostname itself on HTTPS port 443 and
can replace or conflict with the route used for the QNAP administration UI. If this
happens, run `tailscale serve off` to restore access, then configure the two named
Services below.

## 1. Define and tag the Service host

In the Tailscale admin console:

1. Create `tag:service` under **Access controls → Tags**, with an appropriate owner
   such as `autogroup:admin`.
2. Assign `tag:service` to the QNAP node that runs ModelKeep. Tailscale Service hosts
   must be tagged nodes; otherwise Serve reports `service hosts must be tagged nodes`.
3. Under **Services → Advertised**, define `modelkeep` and `modelkeep-admin`, both on
   HTTPS port 443. Their policy names are `svc:modelkeep` and
   `svc:modelkeep-admin`.

Defining a Service does not yet make it reachable. The QNAP must advertise it and an
administrator must approve that advertisement in a later step.

## 2. Configure access policies

Network access to a Tailscale Service and the application capability consumed by
Serve have different destinations:

- use `svc:modelkeep` or `svc:modelkeep-admin` as `dst` for network access;
- use the actual Service Proxy node tag, `tag:service`, as `dst` for
  `io.modelkeep/cap/admin`.

The capability must not target `svc:modelkeep-admin`. That configuration permits the
HTTPS connection but does not cause Serve to forward the capability, so ModelKeep
returns `401 Unauthorized`.

If the tailnet retains its default **All users and devices** Policy (`src: ["*"]`,
`dst: ["*"]`, `ip: ["*"]`), network access is already allowed. Add one separate
Policy in the visual editor:

```json
{
  "src": ["autogroup:admin", "autogroup:owner"],
  "dst": ["tag:service"],
  "app": {
    "io.modelkeep/cap/admin": [{}]
  }
}
```

The visual editor accepts one grant object per Policy. Do not paste an outer
`{"grants": [...]}` wrapper into that field. The allow-all Policy grants connectivity
only; it does not grant ModelKeep administration.

For a restricted tailnet, replace the broad network Policy with three separate
Policies equivalent to:

```json
{
  "grants": [
    {
      "src": ["autogroup:member"],
      "dst": ["svc:modelkeep"],
      "ip": ["tcp:443"]
    },
    {
      "src": ["autogroup:admin", "autogroup:owner"],
      "dst": ["svc:modelkeep-admin"],
      "ip": ["tcp:443"]
    },
    {
      "src": ["autogroup:admin", "autogroup:owner"],
      "dst": ["tag:service"],
      "app": {
        "io.modelkeep/cap/admin": [{}]
      }
    }
  ]
}
```

The outer object above shows the combined policy-file representation; enter its three
inner objects separately when using the visual editor. Merge them with unrelated
tailnet policy rather than replacing it.

## 3. Start ModelKeep

The Compose mappings are deliberately bound to loopback. Port 8090 is the download
endpoint and port 8091 is the authenticated management API/UI. Start ModelKeep and
check both local backends before configuring ingress:

```sh
docker compose up -d
curl --fail http://127.0.0.1:8090/healthz
curl --fail http://127.0.0.1:8091/admin/
```

## 4. Advertise both Services from QNAP

After confirming Tailscale 1.92 or newer and starting ModelKeep, run:

```sh
tailscale serve --service=svc:modelkeep --bg http://127.0.0.1:8090
tailscale serve --service=svc:modelkeep-admin --accept-app-caps=io.modelkeep/cap/admin --bg http://127.0.0.1:8091
tailscale serve status --json
```

Tailscale Services run in background mode persistently. `--bg` is retained here
because it makes the intended persistence explicit and works with the verified QNAP
version.

The first command may report that administrator approval is required. In the
Tailscale admin console, open each Service under **Services → Advertised**, find the
pending QNAP advertisement in **Service hosts**, and select **Approve**. Approve both
`modelkeep` and `modelkeep-admin`. `tailscale serve status` showing a local proxy
configuration does not by itself mean that the Service advertisement has been
approved or propagated.

After approval, allow a short period for route propagation. The Services page should
show the QNAP host as connected for both Services.

Serve reports assigned URLs resembling:

```text
https://modelkeep.example-tailnet.ts.net
https://modelkeep-admin.example-tailnet.ts.net
```

## 5. Verify from a tailnet client

Verify both endpoints from a user included in the Policies:

```sh
curl --fail https://modelkeep.example-tailnet.ts.net/healthz
curl --fail https://modelkeep-admin.example-tailnet.ts.net/api/admin/v1/status
export HF_ENDPOINT=https://modelkeep.example-tailnet.ts.net
hf download Qwen/example-model
```

Open `https://modelkeep-admin.example-tailnet.ts.net/admin/` for archive inventory,
prefetch, refresh, verification, audit, and job status. The actual hostnames shown by
`tailscale serve status` are authoritative.

On QNAP, confirm that the management Serve configuration retained capability
forwarding:

```sh
tailscale serve status --json
```

The JSON Serve configuration must show
`io.modelkeep/cap/admin` in `AcceptAppCaps`. A `401 Unauthorized` from the management
API means the connection reached ModelKeep but Serve did not attach the required
capability. Check that the grant targets the QNAP's `tag:service`, then confirm that
the requesting user belongs to `autogroup:admin`, `autogroup:owner`, or the configured
administrator group.

For user-owned source devices, Serve also adds `Tailscale-User-Login` and
`Tailscale-User-Name`. ModelKeep shows that identity in the management UI and records
it on newly submitted jobs for operator accountability. These identity fields are not
authorization inputs: `io.modelkeep/cap/admin` remains required. Tagged source
devices do not have a Tailscale user identity and are displayed as a Tailscale
principal without an invented user name.

The bearer-token form appears only when `MODELKEEP_ADMIN_TOKEN` is non-empty and a
request is not already authorized through Tailscale. With the default QNAP
Tailscale-only configuration, an unauthorized page reports a Tailscale authorization
problem instead of asking for a token that is not configured.

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

## Troubleshooting

If a Service hostname times out:

1. Confirm that the Service exists under **Services → Advertised** and that the QNAP
   advertisement is approved and connected.
2. Confirm the proxy with `tailscale serve status --json` on QNAP.
3. Confirm the local backend with `curl --fail http://127.0.0.1:8090/healthz` or
   `curl --fail http://127.0.0.1:8091/admin/`.
4. Check the client Tailscale version. Clients 1.94 and newer accept Service routes
   without enabling general route acceptance. On Linux clients running 1.93 or
   earlier, enable it with `sudo tailscale set --accept-routes` or upgrade first.

If the management hostname responds with 401, test the backend authorization path on
QNAP without exposing the management port:

```sh
curl --fail \
  -H 'Tailscale-App-Capabilities: {"io.modelkeep/cap/admin":[{}]}' \
  http://127.0.0.1:8091/api/admin/v1/status
```

If that succeeds, ModelKeep and `MODELKEEP_TRUST_TAILSCALE_HEADERS` are configured;
the remaining problem is the Tailscale capability Policy or Serve forwarding. If it
fails, inspect the Container Station environment and ModelKeep logs before changing
Tailscale policy.

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
tailscale serve --service=svc:modelkeep --https=443 off
tailscale serve --service=svc:modelkeep-admin --https=443 off
```
