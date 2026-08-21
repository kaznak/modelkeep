# QNAP Tailscale Serve ingress

The supported QNAP deployment exposes ModelKeep only inside the tailnet. Tailscale
runs as the official QNAP host application; it is intentionally not installed in the
ModelKeep container.

## Prerequisites

Install and connect Tailscale from QNAP App Center. Enable MagicDNS and HTTPS
certificates for the tailnet. These are administrative actions: QNAP must first be
authenticated as a tailnet node, and enabling tailnet HTTPS may require approval in
the Tailscale admin console. The first `tailscale serve` invocation can print a URL
for that one-time approval.

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

The Compose mapping is deliberately `127.0.0.1:8090:8090`. Start ModelKeep and check
the local backend before configuring ingress:

```sh
docker compose up -d
curl --fail http://127.0.0.1:8090/healthz
```

Configure persistent tailnet-only HTTPS proxying on the QNAP host:

```sh
tailscale serve --bg http://127.0.0.1:8090
tailscale serve status
```

Serve reports the assigned URL, for example:

```text
https://qnap-name.example-tailnet.ts.net
```

Verify it from an allowed tailnet client and then use it as the Hugging Face endpoint:

```sh
curl --fail https://qnap-name.example-tailnet.ts.net/healthz
export HF_ENDPOINT=https://qnap-name.example-tailnet.ts.net
hf download Qwen/example-model
```

Use Tailscale grants to restrict which users or devices may reach the QNAP HTTPS
service; do not rely on a broad allow-all tailnet policy. Clients do not need a
separate ModelKeep password because their node identity and connection authorization
are enforced by Tailscale before Serve forwards the request. Never run `tailscale
funnel` for ModelKeep.

This does not replace Hugging Face upstream authentication. `HF_TOKEN`, when required
for a private or gated upstream repository, remains only in the ModelKeep deployment
environment. It is unrelated to tailnet client identity and must never be placed in
the Serve URL or tailnet policy.

## Boundary checks and recovery

From another LAN machine that is not using the tailnet, the direct backend must not
be reachable:

```sh
curl --connect-timeout 3 http://QNAP_LAN_ADDRESS:8090/healthz
```

That command is expected to fail. If it succeeds, stop deployment and inspect the
effective Compose port mapping before use.

Background Serve configuration survives Tailscale restarts. After a QNAP reboot,
check both layers separately:

```sh
curl --fail http://127.0.0.1:8090/healthz
tailscale serve status
curl --fail https://qnap-name.example-tailnet.ts.net/readyz
```

To remove the HTTPS ingress without stopping ModelKeep, run:

```sh
tailscale serve off
```
