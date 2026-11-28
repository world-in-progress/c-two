#!/usr/bin/env python3
"""Sign and publish the exact Java files staged from a qualified candidate."""
from __future__ import annotations

import argparse
import base64
import hashlib
import http.client
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
import zipfile
from pathlib import Path

from release_candidate import (
    HelperError, JAVA_ARTIFACT, MAVEN_GROUP, java_filenames, sha256_file,
)

CENTRAL_REPOSITORY = "https://repo1.maven.org/maven2"
PUBLISHER_API = "https://central.sonatype.com/api/v1/publisher"
RETRYABLE_HTTP = {429, 500, 502, 503, 504}
NETWORK_ERRORS = (urllib.error.URLError, OSError, http.client.HTTPException)
RETRY_ATTEMPTS = 8
REQUEST_TIMEOUT_S = 30


class MavenPublishError(RuntimeError):
    pass


class PortalRedirectHandler(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        # Do not forward Central credentials through an HTTP redirect.
        return None


def maven_path(version: str) -> str:
    return f"{MAVEN_GROUP.replace('.', '/')}/{JAVA_ARTIFACT}/{version}"


def deployment_id(value: str) -> str:
    try:
        parsed = uuid.UUID(value)
    except (AttributeError, ValueError) as error:
        raise MavenPublishError("Central deployment ID must be a UUID") from error
    if str(parsed) != value:
        raise MavenPublishError("Central deployment ID must be a canonical UUID")
    return value


def retry_read(request, *, retry_missing: bool = False, opener=urllib.request.urlopen) -> bytes | None:
    for attempt in range(RETRY_ATTEMPTS):
        try:
            with opener(request, timeout=REQUEST_TIMEOUT_S) as response:
                return response.read()
        except urllib.error.HTTPError as error:
            code = error.code
            error.close()
            if code == 404 and not retry_missing:
                return None
            if code not in RETRYABLE_HTTP and not (code == 404 and retry_missing):
                raise MavenPublishError(f"Central request failed with HTTP {code}") from None
            failure = f"HTTP {code}"
        except NETWORK_ERRORS as error:
            failure = type(error).__name__
        if attempt + 1 < RETRY_ATTEMPTS:
            time.sleep(min(2 ** attempt, 30))
    raise MavenPublishError(f"Central request failed after {RETRY_ATTEMPTS} attempts ({failure})")


def remote_digest(version: str, name: str, *, retry_missing: bool = False) -> str | None:
    data = retry_read(f"{CENTRAL_REPOSITORY}/{maven_path(version)}/{name}", retry_missing=retry_missing)
    return hashlib.sha256(data).hexdigest() if data is not None else None


def secret(name: str) -> str:
    value = os.environ.get(name)
    if not value:
        raise MavenPublishError(f"{name} is required")
    return value


def sign_bundle(staging: Path, version: str, names: tuple[str, ...], scratch: Path) -> bytes:
    keyring = scratch / "gnupg"
    keyring.mkdir(mode=0o700)
    signing_env = {key: value for key, value in os.environ.items() if not key.startswith("MAVEN_")}
    signing_env["GNUPGHOME"] = str(keyring)
    gpg = ["gpg", "--batch", "--yes", "--pinentry-mode", "loopback"]
    private_key = secret("MAVEN_GPG_PRIVATE_KEY").encode()
    passphrase = (secret("MAVEN_GPG_PASSPHRASE") + "\n").encode()

    def run_gpg(command: list[str], data: bytes, operation: str) -> None:
        result = subprocess.run(gpg + command, input=data, env=signing_env,
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                timeout=60, check=False)
        if result.returncode:
            raise MavenPublishError(f"GPG {operation} failed (exit {result.returncode})")

    try:
        run_gpg(["--import"], private_key, "key import")
        bundle = scratch / "central-bundle.zip"
        with zipfile.ZipFile(bundle, "w", compression=zipfile.ZIP_DEFLATED) as archive:
            for name in names:
                source = staging / name
                signature = scratch / f"{name}.asc"
                run_gpg(["--passphrase-fd", "0", "--armor", "--detach-sign",
                         "--output", str(signature), str(source)], passphrase, f"signing {name}")
                entry = f"{maven_path(version)}/{name}"
                archive.write(source, entry)
                archive.write(signature, f"{entry}.asc")
                data = source.read_bytes()
                archive.writestr(f"{entry}.md5", hashlib.md5(data, usedforsecurity=False).hexdigest())
                archive.writestr(f"{entry}.sha1", hashlib.sha1(data, usedforsecurity=False).hexdigest())
        return bundle.read_bytes()
    finally:
        if shutil.which("gpgconf"):
            subprocess.run(["gpgconf", "--kill", "gpg-agent"], env=signing_env,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30, check=False)


def upload_bundle(opener, auth: str, version: str, bundle: bytes) -> str:
    boundary = uuid.uuid4().hex
    body = (
        f"--{boundary}\r\n"
        'Content-Disposition: form-data; name="bundle"; filename="central-bundle.zip"\r\n'
        "Content-Type: application/octet-stream\r\n\r\n"
    ).encode() + bundle + f"\r\n--{boundary}--\r\n".encode()
    query = urllib.parse.urlencode({"name": f"{JAVA_ARTIFACT}-{version}", "publishingType": "AUTOMATIC"})
    request = urllib.request.Request(
        f"{PUBLISHER_API}/upload?{query}", data=body, method="POST",
        headers={"Authorization": auth, "Content-Type": f"multipart/form-data; boundary={boundary}"},
    )
    try:
        # An accepted upload can lose its response. Only status and registry
        # reads can be retried without risking a duplicate deployment.
        with opener(request, timeout=REQUEST_TIMEOUT_S) as response:
            return deployment_id(response.read(128).decode("ascii").strip())
    except urllib.error.HTTPError as error:
        error.close()
        raise MavenPublishError(f"Central upload failed with HTTP {error.code}") from None
    except NETWORK_ERRORS:
        raise MavenPublishError("Central upload response was lost. Recover using the Portal deployment ID.") from None


def wait_published(opener, auth: str, identifier: str, version: str) -> None:
    request = urllib.request.Request(
        f"{PUBLISHER_API}/status?{urllib.parse.urlencode({'id': identifier})}",
        method="POST", headers={"Authorization": auth},
    )
    expected = f"pkg:maven/{MAVEN_GROUP}/{JAVA_ARTIFACT}@{version}"
    deadline = time.monotonic() + 20 * 60
    while time.monotonic() < deadline:
        data = retry_read(request, opener=opener, retry_missing=True)
        status = json.loads(data)
        if not isinstance(status, dict) or status.get("deploymentId") != identifier:
            raise MavenPublishError("Central returned an unexpected deployment ID")
        purls = status.get("purls")
        if purls and purls != [expected]:
            raise MavenPublishError(f"Central deployment {identifier} does not match {expected}")
        state = status.get("deploymentState")
        if state == "FAILED":
            detail = json.dumps(status.get("errors", {}))
            # Only publish diagnostics to the log, with credentials removed.
            secrets = [auth, auth.removeprefix("Bearer ")]
            secrets.extend(os.environ.get(key, "") for key in (
                "MAVEN_CENTRAL_USERNAME", "MAVEN_CENTRAL_PASSWORD",
                "MAVEN_GPG_PRIVATE_KEY", "MAVEN_GPG_PASSPHRASE",
            ))
            for value in sorted(filter(None, secrets), key=len, reverse=True):
                detail = detail.replace(json.dumps(value)[1:-1], "[redacted]")
            raise MavenPublishError(f"Central deployment {identifier} failed: {detail}")
        if state == "PUBLISHED":
            if purls != [expected]:
                raise MavenPublishError(f"Central deployment {identifier} does not bind {expected}")
            return
        if state not in {"PENDING", "VALIDATING", "VALIDATED", "PUBLISHING"}:
            raise MavenPublishError(f"Central deployment {identifier} returned an unknown state")
        time.sleep(10)
    raise MavenPublishError(f"Central deployment {identifier} did not reach PUBLISHED before the deadline")


def write_json(path: Path, data: dict) -> None:
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    temporary.replace(path)


def publish(staging: Path, plan_path: Path, work: Path, resume_id: str | None) -> None:
    plan = json.loads(plan_path.read_text(encoding="utf-8"))
    if (plan["schema"] != "c-two.release-promotion-plan.v1"
            or plan["repository"] != "world-in-progress/c-two" or plan["target"] != "maven-central"):
        raise MavenPublishError("expected a canonical Maven Central promotion plan")
    version = plan["versions"]["java_version"]
    names = java_filenames(version)
    group = plan["maven_central"]
    if group["version"] != version or group["coordinate"] != f"{MAVEN_GROUP}:{JAVA_ARTIFACT}":
        raise MavenPublishError("Maven Central plan coordinate/version mismatch")
    if group["action"] not in {"upload", "noop"} or (group["action"] == "noop" and not resume_id):
        raise MavenPublishError("Maven Central plan does not authorize this publication")
    files = group["files"]
    hashes = {entry["name"]: entry["sha256"] for entry in files}
    if (len(files) != len(names) or set(hashes) != set(names)
            or {path.name for path in staging.iterdir()} != set(names)):
        raise MavenPublishError("Maven staging inventory differs from the promotion plan")
    for name in names:
        path = staging / name
        if not path.is_file() or path.is_symlink() or sha256_file(path) != hashes[name]:
            raise MavenPublishError(f"staged Maven file {name} differs from the promotion plan SHA-256")

    token = f"{secret('MAVEN_CENTRAL_USERNAME')}:{secret('MAVEN_CENTRAL_PASSWORD')}"
    auth = "Bearer " + base64.b64encode(token.encode()).decode("ascii")
    opener = urllib.request.build_opener(PortalRedirectHandler()).open
    work.mkdir(parents=True, exist_ok=True)
    identity = {
        "coordinate": f"{MAVEN_GROUP}:{JAVA_ARTIFACT}:{version}",
        "source_commit_sha": plan["source_sha"],
        "files": hashes,
    }
    if resume_id:
        identifier = deployment_id(resume_id)
    else:
        with tempfile.TemporaryDirectory(prefix="maven-signing-") as directory:
            bundle = sign_bundle(staging, version, names, Path(directory))
            identifier = upload_bundle(opener, auth, version, bundle)
            write_json(work / "deployment.json", {**identity, "deployment_id": identifier})
    if resume_id:
        write_json(work / "deployment.json", {**identity, "deployment_id": identifier})
    print(f"Central deployment {identifier}")
    wait_published(opener, auth, identifier, version)
    for name in names:
        if remote_digest(version, name, retry_missing=True) != hashes[name]:
            raise MavenPublishError(f"published Maven file {name} differs from the promotion plan SHA-256")
    write_json(work / "maven-publication-receipt.json", {
        **identity, "schema": "c-two.maven-publication-receipt.v1",
        "deployment_id": identifier, "status": "passed",
    })
    print(f"Verified all four Maven Central files for {identity['coordinate']}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--staging-dir", type=Path, required=True)
    parser.add_argument("--plan", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--deployment-id")
    args = parser.parse_args(argv)
    try:
        publish(args.staging_dir, args.plan, args.work_dir, args.deployment_id)
    except (MavenPublishError, HelperError, OSError, ValueError, KeyError, TypeError,
            subprocess.TimeoutExpired) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
