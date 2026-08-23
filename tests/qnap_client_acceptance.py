#!/usr/bin/env python3
"""Client-side, phased acceptance suite for ModelKeep on QNAP.

The suite deliberately never modifies the server archive. Server-side operations
such as restart, reboot, upstream blocking, snapshot, and restore remain explicit
operator actions between phases.
"""

import argparse
import hashlib
import json
import os
import platform
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timezone
from pathlib import Path


SCHEMA_VERSION = 1
COMMIT_PATTERN = re.compile(r"[0-9a-f]{40}")
DIGEST_PATTERN = re.compile(r"sha256:[0-9a-f]{64}")
REQUIRED_PHASES = (
    "preflight",
    "cold",
    "warm",
    "offline",
    "post-container-restart",
    "post-qnap-reboot",
    "post-restore",
)


class AcceptanceError(RuntimeError):
    pass


def now():
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat()


def normalize_endpoint(value, label):
    endpoint = value.rstrip("/")
    parsed = urllib.parse.urlsplit(endpoint)
    if (
        parsed.scheme != "https"
        or not parsed.netloc
        or parsed.path
        or parsed.query
        or parsed.fragment
        or parsed.username
        or parsed.password
    ):
        raise AcceptanceError(f"{label} must be an HTTPS origin without a path")
    return endpoint


def validate_record(record):
    if record.get("schema_version") != SCHEMA_VERSION:
        raise AcceptanceError("unsupported acceptance record schema")
    config = record.get("configuration", {})
    if not COMMIT_PATTERN.fullmatch(config.get("revision", "")):
        raise AcceptanceError("revision must be a 40-character lowercase commit SHA")
    if any(not part for part in config.get("repo_id", "").split("/")) or len(
        config.get("repo_id", "").split("/")
    ) != 2:
        raise AcceptanceError("repo ID must have exactly namespace/name components")


def read_record(path):
    try:
        record = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as error:
        raise AcceptanceError(f"cannot read acceptance record: {error}") from error
    validate_record(record)
    return record


def write_record(path, record):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_text(json.dumps(record, indent=2, sort_keys=True) + "\n")
    os.replace(temporary, path)


def request(url, method="GET", headers=None, timeout=30):
    request_headers = {"User-Agent": "modelkeep-qnap-acceptance/1"}
    request_headers.update(headers or {})
    token = os.environ.get("MODELKEEP_ACCEPTANCE_ADMIN_TOKEN")
    if token and "/api/admin/" in url:
        request_headers["Authorization"] = f"Bearer {token}"
    return urllib.request.urlopen(
        urllib.request.Request(url, method=method, headers=request_headers),
        timeout=timeout,
    )


def expect_ok(url, timeout):
    try:
        with request(url, timeout=timeout) as response:
            if response.status != 200:
                raise AcceptanceError(f"{url} returned HTTP {response.status}")
    except (OSError, urllib.error.HTTPError) as error:
        raise AcceptanceError(f"cannot reach {url}: {error}") from error


def json_request(url, timeout, allow_not_found=False):
    try:
        with request(url, timeout=timeout) as response:
            return response.status, json.load(response)
    except urllib.error.HTTPError as error:
        if allow_not_found and error.code == 404:
            return 404, None
        if error.code == 401:
            raise AcceptanceError(
                f"admin authorization failed for {url}; verify the Tailscale app-cap grant"
            ) from error
        raise AcceptanceError(f"{url} returned HTTP {error.code}") from error
    except (OSError, json.JSONDecodeError) as error:
        raise AcceptanceError(f"cannot read JSON from {url}: {error}") from error


def repository_url(record):
    config = record["configuration"]
    namespace, repository = config["repo_id"].split("/")
    return (
        f"{config['admin_endpoint']}/api/admin/v1/repositories/"
        f"{urllib.parse.quote(namespace, safe='')}/{urllib.parse.quote(repository, safe='')}"
    )


def model_info_url(record):
    config = record["configuration"]
    namespace, repository = config["repo_id"].split("/")
    return (
        f"{config['endpoint']}/api/models/{urllib.parse.quote(namespace, safe='')}/"
        f"{urllib.parse.quote(repository, safe='')}/revision/{config['revision']}"
    )


def file_url(record, relative_path):
    config = record["configuration"]
    repository = "/".join(
        urllib.parse.quote(part, safe="") for part in config["repo_id"].split("/")
    )
    components = [urllib.parse.quote(part, safe="") for part in relative_path.split("/")]
    return (
        f"{config['endpoint']}/{repository}/resolve/{config['revision']}/"
        + "/".join(components)
    )


def commit_is_archived(inventory, commit):
    return any(revision.get("commit") == commit for revision in inventory.get("revisions", []))


def download_manifest(root):
    manifest = {}
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root)
        if ".cache" in relative.parts or not path.is_file():
            continue
        digest = hashlib.sha256()
        size = 0
        with path.open("rb") as source:
            while chunk := source.read(1024 * 1024):
                size += len(chunk)
                digest.update(chunk)
        manifest[relative.as_posix()] = {"size": size, "sha256": digest.hexdigest()}
    if not manifest:
        raise AcceptanceError("hf download produced no model files")
    return manifest


def run_hf_download(record):
    if not shutil.which("hf"):
        raise AcceptanceError("hf CLI is not available in PATH")
    config = record["configuration"]
    with tempfile.TemporaryDirectory(prefix="modelkeep-acceptance-") as temporary:
        root = Path(temporary)
        destination = root / "model"
        environment = os.environ.copy()
        environment.update(
            {
                "HF_ENDPOINT": config["endpoint"],
                "HF_HOME": str(root / "hf-home"),
                "HF_HUB_DISABLE_XET": "1",
            }
        )
        # This acceptance model must be public. Do not send a client credential to
        # ModelKeep or accidentally make success depend on the GX10 token cache.
        environment.pop("HF_TOKEN", None)
        environment.pop("HUGGING_FACE_HUB_TOKEN", None)
        try:
            completed = subprocess.run(
                [
                    "hf",
                    "download",
                    config["repo_id"],
                    "--revision",
                    config["revision"],
                    "--local-dir",
                    str(destination),
                ],
                env=environment,
                timeout=config["download_timeout_seconds"],
                check=False,
            )
        except subprocess.TimeoutExpired as error:
            raise AcceptanceError("hf download exceeded the configured timeout") from error
        if completed.returncode != 0:
            raise AcceptanceError(
                f"hf download failed with exit code {completed.returncode}; "
                "inspect the client terminal and ModelKeep logs"
            )
        manifest = download_manifest(destination)
        return manifest, check_range(record, destination, manifest)


def check_range(record, downloaded_root, manifest):
    candidates = [(details["size"], path) for path, details in manifest.items() if details["size"]]
    if not candidates:
        raise AcceptanceError("download contains no non-empty file for Range validation")
    size, relative_path = max(candidates)
    length = min(size, 64)
    url = file_url(record, relative_path)
    timeout = record["configuration"]["request_timeout_seconds"]
    try:
        with request(url, method="HEAD", timeout=timeout) as response:
            if response.status != 200 or int(response.headers.get("Content-Length", "-1")) != size:
                raise AcceptanceError("HEAD response does not describe the archived file")
        with request(
            url,
            headers={"Range": f"bytes=0-{length - 1}"},
            timeout=timeout,
        ) as response:
            body = response.read()
            expected_range = f"bytes 0-{length - 1}/{size}"
            if response.status != 206 or response.headers.get("Content-Range") != expected_range:
                raise AcceptanceError("Range response has incorrect status or Content-Range")
    except (OSError, urllib.error.HTTPError) as error:
        raise AcceptanceError(f"Range request failed: {error}") from error
    with (downloaded_root / relative_path).open("rb") as source:
        expected = source.read(length)
    if body != expected:
        raise AcceptanceError("Range bytes differ from the downloaded file")
    return {"path": relative_path, "size": size, "range_bytes": length}


def check_endpoints(record, require_pullthrough=True):
    config = record["configuration"]
    timeout = config["request_timeout_seconds"]
    expect_ok(f"{config['endpoint']}/healthz", timeout)
    expect_ok(f"{config['endpoint']}/readyz", timeout)
    _, status = json_request(
        f"{config['admin_endpoint']}/api/admin/v1/status", timeout
    )
    if not status.get("ready"):
        raise AcceptanceError("management status reports archive not ready")
    if require_pullthrough and not status.get("pullthrough_enabled"):
        raise AcceptanceError("management status reports pull-through disabled")
    if status.get("principal", {}).get("auth_method") != "tailscale":
        raise AcceptanceError("management endpoint was not authenticated by Tailscale")
    return {
        "server_version": status.get("version"),
        "admin_auth_method": status.get("principal", {}).get("auth_method"),
        "admin_login": status.get("principal", {}).get("login"),
    }


def check_lan_boundary(record):
    config = record["configuration"]
    results = {}
    for port in (8090, 8091):
        try:
            connection = socket.create_connection(
                (config["qnap_lan_address"], port),
                timeout=config["request_timeout_seconds"],
            )
        except OSError as error:
            results[str(port)] = {"closed": True, "result": type(error).__name__}
        else:
            connection.close()
            raise AcceptanceError(f"QNAP LAN port {port} is reachable; stop deployment")
    return results


def phase_preflight(record):
    evidence = check_endpoints(record)
    evidence["lan_ports"] = check_lan_boundary(record)
    return evidence


def phase_cold(record):
    config = record["configuration"]
    timeout = config["request_timeout_seconds"]
    endpoint = check_endpoints(record)
    status, inventory = json_request(repository_url(record), timeout, allow_not_found=True)
    if status == 200 and commit_is_archived(inventory, config["revision"]):
        raise AcceptanceError("cold-test commit is already archived; choose another immutable commit")
    manifest, range_result = run_hf_download(record)
    status, inventory = json_request(repository_url(record), timeout)
    if status != 200 or not commit_is_archived(inventory, config["revision"]):
        raise AcceptanceError("download succeeded but the immutable commit is not archived")
    _, model_info = json_request(model_info_url(record), timeout)
    if model_info.get("sha") != config["revision"]:
        raise AcceptanceError("ModelKeep resolved a different commit")
    record["baseline"] = {
        "files": manifest,
        "file_count": len(manifest),
        "logical_bytes": sum(item["size"] for item in manifest.values()),
    }
    return {**endpoint, "download": record["baseline"], "range": range_result}


def phase_download(record, require_pullthrough=True):
    endpoint = check_endpoints(record, require_pullthrough=require_pullthrough)
    manifest, range_result = run_hf_download(record)
    baseline = record.get("baseline", {}).get("files")
    if not baseline:
        raise AcceptanceError("cold phase has not established a baseline")
    if manifest != baseline:
        raise AcceptanceError("fresh client download differs from the cold baseline")
    return {
        **endpoint,
        "download": {
            "file_count": len(manifest),
            "logical_bytes": sum(item["size"] for item in manifest.values()),
        },
        "range": range_result,
    }


def run_phase(path, name, function):
    record = read_record(path)
    if record.get("phases", {}).get(name, {}).get("status") == "passed":
        raise AcceptanceError(f"{name} phase already passed; preserve its evidence")
    if name != "preflight" and record.get("phases", {}).get("preflight", {}).get("status") != "passed":
        raise AcceptanceError("run the preflight phase successfully first")
    started = now()
    try:
        evidence = function(record)
    except Exception as error:
        record.setdefault("phases", {})[name] = {
            "status": "failed",
            "started_at": started,
            "finished_at": now(),
            "error": str(error),
        }
        write_record(path, record)
        raise
    record.setdefault("phases", {})[name] = {
        "status": "passed",
        "started_at": started,
        "finished_at": now(),
        "evidence": evidence,
    }
    write_record(path, record)


def initialize(args):
    path = Path(args.record)
    if path.exists() and not args.force:
        raise AcceptanceError(f"record already exists: {path}; use --force to replace it")
    revision = args.revision.lower()
    record = {
        "schema_version": SCHEMA_VERSION,
        "created_at": now(),
        "configuration": {
            "endpoint": normalize_endpoint(args.endpoint, "download endpoint"),
            "admin_endpoint": normalize_endpoint(args.admin_endpoint, "admin endpoint"),
            "qnap_lan_address": args.qnap_lan_address,
            "repo_id": args.repo_id,
            "revision": revision,
            "request_timeout_seconds": args.request_timeout,
            "download_timeout_seconds": args.download_timeout,
        },
        "site": {
            "operator": args.operator,
            "qnap_model": args.qnap_model,
            "qts_version": args.qts_version,
            "container_station_version": args.container_station_version,
            "archive_share_and_acl": args.archive_share_and_acl,
            "snapshot_mechanism_and_retention": args.snapshot_mechanism_and_retention,
            "external_backup_target": args.external_backup_target,
            "image_tag": args.image_tag,
            "image_digest": args.image_digest,
        },
        "client": {
            "hostname": socket.gethostname(),
            "platform": platform.platform(),
            "python": platform.python_version(),
        },
        "phases": {},
    }
    validate_record(record)
    if not DIGEST_PATTERN.fullmatch(args.image_digest):
        raise AcceptanceError("image digest must be sha256 followed by 64 lowercase hex digits")
    if args.request_timeout <= 0 or args.download_timeout <= 0:
        raise AcceptanceError("timeouts must be positive")
    write_record(path, record)
    print(f"initialized {path}")


def render_summary(record):
    config = record["configuration"]
    site = record["site"]
    lines = [
        "# ModelKeep QNAP acceptance record",
        "",
        f"- Created: {record['created_at']}",
        f"- Operator: {site['operator']}",
        f"- Client: {record['client']['hostname']} ({record['client']['platform']})",
        f"- QNAP: {site['qnap_model']} / {site['qts_version']}",
        f"- Container Station: {site['container_station_version']}",
        f"- Archive share and ACL: {site['archive_share_and_acl']}",
        f"- Snapshot: {site['snapshot_mechanism_and_retention']}",
        f"- External backup: {site['external_backup_target']}",
        f"- Image: {site['image_tag']}@{site['image_digest']}",
        f"- Endpoint: {config['endpoint']}",
        f"- Repository: {config['repo_id']}@{config['revision']}",
        "",
        "## Phases",
        "",
        "| Phase | Status | Finished (UTC) |",
        "| --- | --- | --- |",
    ]
    for name in REQUIRED_PHASES:
        phase = record.get("phases", {}).get(name, {})
        lines.append(
            f"| {name} | {phase.get('status', 'missing')} | {phase.get('finished_at', '')} |"
        )
    baseline = record.get("baseline", {})
    lines.extend(
        [
            "",
            "## Download baseline",
            "",
            f"- Files: {baseline.get('file_count', 0)}",
            f"- Logical bytes: {baseline.get('logical_bytes', 0)}",
            "",
        ]
    )
    return "\n".join(lines)


def finish(args):
    path = Path(args.record)
    record = read_record(path)
    missing = [
        name
        for name in REQUIRED_PHASES
        if record.get("phases", {}).get(name, {}).get("status") != "passed"
    ]
    if missing:
        raise AcceptanceError("acceptance is incomplete: " + ", ".join(missing))
    output = Path(args.output) if args.output else path.with_suffix(".md")
    output.write_text(render_summary(record))
    record["completed_at"] = now()
    record["summary"] = str(output)
    write_record(path, record)
    print(f"acceptance passed; summary written to {output}")


def require_confirmation(args, attribute, message):
    if not getattr(args, attribute):
        raise AcceptanceError(message)


def confirmed_download(record, confirmations, require_pullthrough=True):
    evidence = phase_download(record, require_pullthrough=require_pullthrough)
    evidence["operator_confirmations"] = confirmations
    return evidence


def parser():
    root = argparse.ArgumentParser(description=__doc__)
    commands = root.add_subparsers(dest="command", required=True)
    init = commands.add_parser("init", help="create a site acceptance record")
    init.add_argument("record")
    init.add_argument("--endpoint", required=True)
    init.add_argument("--admin-endpoint", required=True)
    init.add_argument("--qnap-lan-address", required=True)
    init.add_argument("--repo-id", required=True)
    init.add_argument("--revision", required=True)
    init.add_argument("--operator", required=True)
    init.add_argument("--qnap-model", required=True)
    init.add_argument("--qts-version", required=True)
    init.add_argument("--container-station-version", required=True)
    init.add_argument("--archive-share-and-acl", required=True)
    init.add_argument("--snapshot-mechanism-and-retention", required=True)
    init.add_argument("--external-backup-target", required=True)
    init.add_argument("--image-tag", required=True)
    init.add_argument("--image-digest", required=True)
    init.add_argument("--request-timeout", type=int, default=10)
    init.add_argument("--download-timeout", type=int, default=7200)
    init.add_argument("--force", action="store_true")

    for name in REQUIRED_PHASES:
        phase = commands.add_parser(name)
        phase.add_argument("record")
        if name == "offline":
            phase.add_argument("--confirm-upstream-blocked", action="store_true")
        if name == "post-restore":
            phase.add_argument("--confirm-upstream-blocked", action="store_true")
            phase.add_argument("--confirm-restored-copy", action="store_true")

    final = commands.add_parser("finish", help="validate phases and render Markdown")
    final.add_argument("record")
    final.add_argument("--output")
    return root


def main():
    args = parser().parse_args()
    try:
        if args.command == "init":
            initialize(args)
            return
        if args.command == "finish":
            finish(args)
            return
        if args.command == "offline":
            require_confirmation(
                args,
                "confirm_upstream_blocked",
                "offline phase requires --confirm-upstream-blocked",
            )
        if args.command == "post-restore":
            require_confirmation(
                args,
                "confirm_upstream_blocked",
                "restore phase requires --confirm-upstream-blocked",
            )
            require_confirmation(
                args,
                "confirm_restored_copy",
                "restore phase requires --confirm-restored-copy",
            )
        if args.command == "preflight":
            function = phase_preflight
        elif args.command == "cold":
            function = phase_cold
        elif args.command == "offline":
            function = lambda record: confirmed_download(record, ["upstream_blocked"])
        elif args.command == "post-restore":
            function = lambda record: confirmed_download(
                record,
                ["upstream_blocked", "restored_copy_active"],
                require_pullthrough=False,
            )
        else:
            function = phase_download
        run_phase(Path(args.record), args.command, function)
        print(f"{args.command}: passed")
    except AcceptanceError as error:
        print(f"{args.command}: FAILED: {error}", file=sys.stderr)
        raise SystemExit(1) from error


if __name__ == "__main__":
    main()
