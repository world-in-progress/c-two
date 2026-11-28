from __future__ import annotations

import hashlib
import io
import json
import struct
import sys
import urllib.error
import zipfile
from pathlib import Path

import pytest
import yaml

ROOT = Path(__file__).resolve().parents[2]
SCRIPTS = ROOT / ".github/scripts"
sys.path.insert(0, str(SCRIPTS))
import maven_publish as publish
import release_candidate as candidate

VERSION = "0.13.0-rc.2"
FASTDB = "0.4.0"
SHA = "a" * 40
DEPLOYMENT = "28570f16-da32-4c14-bd2e-c1acc0782365"


def workflow(name):
    path = ROOT / ".github/workflows" / name
    return yaml.load(path.read_text(encoding="utf-8"), Loader=yaml.BaseLoader)


def test_release_workflow_guards_publication_and_reuses_staging():
    release = workflow("java-package-release.yml")
    assert set(release["on"]) == {"workflow_dispatch"}
    inputs = release["on"]["workflow_dispatch"]["inputs"]
    assert inputs["candidate_run_id"]["required"] == "true"
    assert inputs["deployment_id"]["default"] == ""
    prepare = release["jobs"]["prepare"]
    publisher = release["jobs"]["publish"]
    for job in (prepare, publisher):
        assert "github.repository == 'world-in-progress/c-two'" in job["if"]
        assert "github.ref == 'refs/heads/main'" in job["if"]
        assert "github.event_name == 'workflow_dispatch'" in job["if"]
        assert "environment" not in job
    assert prepare["permissions"] == {"contents": "read", "actions": "read"}
    assert publisher["needs"] == "prepare"
    assert "needs.prepare.outputs.action == 'upload' || inputs.deployment_id != ''" in publisher["if"]
    assert any("github.run_attempt != '1' && inputs.deployment_id == ''" in step.get("if", "")
               for step in publisher["steps"])
    commands = "\n".join(step.get("run", "") for step in publisher["steps"])
    assert "maven_publish.py --staging-dir staging/maven" in commands
    assert '--deployment-id "$DEPLOYMENT_ID"' in commands
    assert all(word not in commands for word in ("cargo", "mvn", "javac"))
    artifacts = [step for step in publisher["steps"] if "download-artifact" in step.get("uses", "")]
    assert {step["with"]["name"] for step in artifacts} == {"promotion-staging", "promotion-plan"}
    retain = publisher["steps"][-1]
    assert retain["if"] == "always()"
    assert retain["with"]["name"] == "maven-deployment-state"
    assert set(retain["with"]["path"].split()) == {
        "maven-deployment/deployment.json", "maven-deployment/maven-publication-receipt.json",
    }


def test_candidate_builds_and_smokes_each_packaged_native_target():
    jobs = workflow("release-candidate.yml")["jobs"]
    native = jobs["java-native"]
    package = jobs["java-package"]
    smoke = jobs["java-smoke"]
    expected = {"linux-x86_64": "ubuntu-24.04", "macos-arm64": "macos-15",
                "windows-x86_64": "windows-2025"}
    for job in (native, smoke):
        rows = job["strategy"]["matrix"]["include"]
        assert len(rows) == len(expected)
        assert {row["platform"]: row["runner"] for row in rows} == expected
    assert "java-native" in package["needs"]
    assert "java-package" in smoke["needs"]
    assert "java-smoke" in jobs["manifest"]["needs"]
    assert native["steps"][-1]["with"]["name"].startswith("java-native-")
    assert package["steps"][-1]["with"]["name"] == "rc-java-package"
    assert any("cmp -s LICENSE sdk/java/src/main/resources/META-INF/LICENSE" in step.get("run", "") for step in package["steps"])
    assert (ROOT / "sdk/java/src/main/resources/META-INF/LICENSE").read_bytes() == (ROOT / "LICENSE").read_bytes()
    assert any(step.get("with", {}).get("name") == "rc-java-package" for step in smoke["steps"])
    assert any("java_package_smoke.py" in step.get("run", "") for step in smoke["steps"])


def native_headers():
    elf = bytearray(64)
    elf[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<H", elf, 18, 62)
    macho = bytearray(32)
    macho[:4] = b"\xcf\xfa\xed\xfe"
    struct.pack_into("<I", macho, 4, 0x0100000c)
    pe = bytearray(88)
    pe[:2] = b"MZ"
    struct.pack_into("<I", pe, 60, 64)
    pe[64:70] = b"PE\0\0\x64\x86"
    return dict(zip(candidate.JAVA_NATIVE_LIBRARIES, (elf, macho, pe), strict=True))


def make_package(root, *, corrupt=None):
    main, sources, javadoc, pom = (root / name for name in candidate.java_filenames(VERSION))
    with zipfile.ZipFile(main, "w") as jar:
        jar.writestr("io/ctwo/Runtime.class", b"runtime")
        jar.writestr("META-INF/LICENSE", b"license")
        for platform, header in native_headers().items():
            if corrupt == platform:
                machine_offset = {"linux-x86_64": 18, "macos-arm64": 4, "windows-x86_64": 68}
                header[machine_offset[platform]] ^= 1
            jar.writestr(f"META-INF/native/{platform}/{candidate.JAVA_NATIVE_LIBRARIES[platform]}", header)
    with zipfile.ZipFile(sources, "w") as jar:
        jar.writestr("io/ctwo/Runtime.java", b"source")
        jar.writestr("META-INF/LICENSE", b"license")
        if corrupt == "sources":
            jar.writestr("META-INF/native/", b"")
    with zipfile.ZipFile(javadoc, "w") as jar:
        jar.writestr("index.html", "<html></html>")
    optional_tag = "<optional>true</optional>" if corrupt == "optional" else ""
    pom.write_text(f"""<project xmlns="http://maven.apache.org/POM/4.0.0">
        <groupId>{candidate.MAVEN_GROUP}</groupId><artifactId>c-two-sdk</artifactId>
        <version>{VERSION}</version><dependencies><dependency>
        <groupId>{candidate.MAVEN_GROUP}</groupId><artifactId>fastdb-java</artifactId>
        <version>{"9.9.9" if corrupt == "pom" else FASTDB}</version>{optional_tag}
        </dependency></dependencies></project>""", encoding="utf-8")


@pytest.mark.parametrize("corrupt, error", [
    (None, None),
    ("linux-x86_64", "linux-x86_64 binary header"),
    ("macos-arm64", "macos-arm64 binary header"),
    ("windows-x86_64", "windows-x86_64 binary header"),
    ("sources", "sources JAR contains META-INF/native/"),
    ("pom", "fastdb-java:0.4.0"),
    ("optional", "fastdb-java:0.4.0"),
])
def test_inspect_java_seals_only_valid_maven_files(tmp_path, capsys, corrupt, error):
    make_package(tmp_path, corrupt=corrupt)
    output = tmp_path / "java-inspect.json"
    result = candidate.main([
        "inspect-java", "--root", str(tmp_path), "--version", VERSION,
        "--expect-fastdb-version", FASTDB, "--source-sha", SHA, "--output", str(output),
    ])
    if error:
        assert result == 1 and not output.exists()
        assert error in capsys.readouterr().err
    else:
        report = json.loads(output.read_text())
        assert result == 0
        assert report["source_commit_sha"] == SHA and report["status"] == "passed"
        assert report["files"] == {
            path.name: hashlib.sha256(path.read_bytes()).hexdigest()
            for path in (tmp_path / name for name in candidate.java_filenames(VERSION))
        }


@pytest.mark.parametrize("version", ["0.0.0", "1.2.3-alpha.0", "1.2.3-beta.1", VERSION])
def test_java_version_accepts_the_stable_and_prerelease_forms(version):
    assert candidate.validate_java_version(version) == ("-" in version)


@pytest.mark.parametrize("version", ["01.2.3", "1.2.3-rc.01", "1.2.3+build", "1.2.3rc1", "1.2.3-rc.1\n"])
def test_java_version_rejects_nonrelease_forms(version):
    with pytest.raises(candidate.HelperError, match="Java version"):
        candidate.validate_java_version(version)


def test_manifest_binds_all_three_smokes_to_the_main_jar(tmp_path):
    make_package(tmp_path)
    inspection = candidate.inspect_java(tmp_path, VERSION, FASTDB, SHA)
    (tmp_path / "java-inspect.json").write_text(json.dumps(inspection))
    main_sha = inspection["files"][candidate.java_filenames(VERSION)[0]]
    for platform in candidate.JAVA_NATIVE_LIBRARIES:
        (tmp_path / f"java-smoke-{platform}.json").write_text(json.dumps({
            "schema": candidate.JAVA_SMOKE_SCHEMA, "source_commit_sha": SHA, "status": "passed",
            "version": VERSION, "fastdb_version": FASTDB, "platform": platform, "jar_sha256": main_sha,
        }))
    files = list(tmp_path.iterdir())
    candidate.main(["checksum", *map(str, files)])
    output = tmp_path / "rc-manifest.json"
    args = ["manifest", "--root", str(tmp_path), "--output", str(output), "--source-sha", SHA,
            "--meta", f"java_version={VERSION}", "--meta", f"fastdb_version={FASTDB}",
            "--expect-java", "4", "--expect-java-version", VERSION, "--java-prefix", f"c-two-sdk-{VERSION}",
            "--expect-json-prefix", "java-inspect", "--expect-json-prefix", "java-smoke-",
            "--expect-json-count", "java-inspect=1", "--expect-json-count", "java-smoke-=3"]
    assert candidate.main(args) == 0
    manifest = json.loads(output.read_text())
    assert manifest["counts"]["java"] == 4
    assert {entry["name"] for entry in manifest["java"]} == set(candidate.java_filenames(VERSION))
    smoke = tmp_path / "java-smoke-macos-arm64.json"
    receipt = json.loads(smoke.read_text())
    receipt["jar_sha256"] = "b" * 64
    smoke.write_text(json.dumps(receipt))
    candidate.main(["checksum", str(smoke)])
    assert candidate.main(args) == 1


def test_central_upload_uses_octet_stream_and_automatic_publication():
    def open_request(request, *, timeout):
        assert request.method == "POST"
        assert request.full_url.endswith(f"/upload?name=c-two-sdk-{VERSION}&publishingType=AUTOMATIC")
        assert request.get_header("Authorization") == "Bearer encoded"
        assert request.get_header("Content-type").startswith("multipart/form-data; boundary=")
        assert b'Content-Disposition: form-data; name="bundle"; filename="central-bundle.zip"' in request.data
        assert b"Content-Type: application/octet-stream\r\n\r\nzip contents" in request.data
        assert request.data.count(b"Content-Type:") == 1
        return io.BytesIO(DEPLOYMENT.encode())

    assert publish.upload_bundle(open_request, "Bearer encoded", VERSION, b"zip contents") == DEPLOYMENT


@pytest.mark.parametrize("code", [429, 500, 502, 503, 504, None])
def test_central_read_retries_transient_errors_with_backoff(monkeypatch, code):
    delays = []
    failures = 2
    monkeypatch.setattr(publish.time, "sleep", delays.append)

    def open_request(url, *, timeout):
        nonlocal failures
        if failures:
            failures -= 1
            if code is None:
                raise urllib.error.URLError("temporary DNS failure")
            raise urllib.error.HTTPError(url, code, "temporary", None, None)
        return io.BytesIO(b"content")

    assert publish.retry_read("https://repo1.maven.org", opener=open_request) == b"content"
    assert delays == [1, 2]


def test_central_repository_404_is_missing_during_prepare_and_retried_after_publish(monkeypatch):
    delays = []
    monkeypatch.setattr(publish.time, "sleep", delays.append)

    def missing(url, *, timeout):
        raise urllib.error.HTTPError(url, 404, "missing", None, None)

    assert publish.retry_read("https://repo1.maven.org", opener=missing) is None
    assert not delays
    with pytest.raises(publish.MavenPublishError, match="HTTP 404"):
        publish.retry_read("https://repo1.maven.org", opener=missing, retry_missing=True)
    assert delays == [1, 2, 4, 8, 16, 30, 30]


def test_central_failed_status_reports_details_without_credentials():
    def failed(request, *, timeout):
        assert request.method == "POST"
        assert request.full_url.endswith(f"/status?id={DEPLOYMENT}")
        return io.BytesIO(json.dumps({
            "deploymentId": DEPLOYMENT, "deploymentState": "FAILED",
            "errors": {"": ["invalid signature", "Bearer credentials"]},
        }).encode())

    with pytest.raises(publish.MavenPublishError, match="invalid signature") as raised:
        publish.wait_published(failed, "Bearer credentials", DEPLOYMENT, VERSION)
    assert "credentials" not in str(raised.value)


@pytest.fixture
def publication(tmp_path, monkeypatch):
    staging = tmp_path / "staging"
    staging.mkdir()
    make_package(staging)
    hashes = {path.name: candidate.sha256_file(path) for path in staging.iterdir()}
    plan = {
        "schema": "c-two.release-promotion-plan.v1", "repository": "world-in-progress/c-two",
        "target": "maven-central", "versions": {"java_version": VERSION}, "source_sha": SHA,
        "maven_central": {"coordinate": f"{candidate.MAVEN_GROUP}:c-two-sdk", "action": "upload",
                          "version": VERSION, "files": [
                              {"name": name, "sha256": digest} for name, digest in hashes.items()
                          ]},
    }
    plan_path = tmp_path / "promotion-plan.json"
    plan_path.write_text(json.dumps(plan))
    monkeypatch.setenv("MAVEN_CENTRAL_USERNAME", "portal-user")
    monkeypatch.setenv("MAVEN_CENTRAL_PASSWORD", "portal-token")
    return staging, plan_path, tmp_path / "work", hashes


@pytest.mark.parametrize("resume", [False, True])
def test_publication_persists_deployment_before_poll_and_verifies_downloads(publication, monkeypatch, resume):
    staging, plan, work, hashes = publication
    calls = []

    def operation(label, result):
        def run(*args):
            calls.append(label)
            return result
        return run

    def wait(opener, token, identifier, version):
        calls.append("poll")
        deployment = json.loads((work / "deployment.json").read_text())
        assert deployment["deployment_id"] == identifier == DEPLOYMENT
        assert deployment["source_commit_sha"] == SHA and deployment["files"] == hashes

    def remote(version, name, *, retry_missing):
        assert retry_missing is True
        calls.append(name)
        return hashes[name]

    monkeypatch.setattr(publish, "sign_bundle", operation("sign", b"bundle"))
    monkeypatch.setattr(publish, "upload_bundle", operation("upload", DEPLOYMENT))
    monkeypatch.setattr(publish, "wait_published", wait)
    monkeypatch.setattr(publish, "remote_digest", remote)
    args = ["--staging-dir", str(staging), "--plan", str(plan), "--work-dir", str(work)]
    if resume:
        args += ["--deployment-id", DEPLOYMENT]
    assert publish.main(args) == 0
    assert calls == ([] if resume else ["sign", "upload"]) + ["poll", *candidate.java_filenames(VERSION)]
    receipt = json.loads((work / "maven-publication-receipt.json").read_text())
    assert receipt["status"] == "passed" and receipt["files"] == hashes


def test_publication_rejects_changed_staging_and_remote_bytes(publication, monkeypatch):
    staging, plan, work, hashes = publication
    file = staging / candidate.java_filenames(VERSION)[0]
    original = file.read_bytes()
    file.write_bytes(b"changed")
    with pytest.raises(publish.MavenPublishError, match="staged Maven file .* SHA-256"):
        publish.publish(staging, plan, work, DEPLOYMENT)
    assert not work.exists()

    file.write_bytes(original)
    monkeypatch.setattr(publish, "wait_published", lambda *args: None)
    monkeypatch.setattr(publish, "remote_digest", lambda *args, **kwargs: "b" * 64)
    with pytest.raises(publish.MavenPublishError, match="published Maven file .* SHA-256"):
        publish.publish(staging, plan, work, DEPLOYMENT)
    assert (work / "deployment.json").is_file()
    assert not (work / "maven-publication-receipt.json").exists()


def test_signing_bundle_contains_detached_signatures_and_checksums(publication, monkeypatch, tmp_path):
    staging, _, _, _ = publication
    monkeypatch.setenv("MAVEN_GPG_PRIVATE_KEY", "private-key")
    monkeypatch.setenv("MAVEN_GPG_PASSPHRASE", "private-passphrase")
    monkeypatch.setattr(publish.shutil, "which", lambda command: None)
    imports = []

    def gpg(command, *, input, env, **kwargs):
        assert {"private-key", "private-passphrase"}.isdisjoint(command)
        assert all(not key.startswith("MAVEN_") for key in env)
        assert Path(env["GNUPGHOME"]).stat().st_mode & 0o777 == 0o700
        if "--import" in command:
            imports.append(input)
        else:
            assert input == b"private-passphrase\n"
            assert ["--passphrase-fd", "0", "--armor", "--detach-sign"] == command[5:9]
            Path(command[command.index("--output") + 1]).write_bytes(b"signature")
        return publish.subprocess.CompletedProcess(command, 0)

    monkeypatch.setattr(publish.subprocess, "run", gpg)
    names = candidate.java_filenames(VERSION)
    bundle = publish.sign_bundle(staging, VERSION, names, tmp_path)
    assert imports == [b"private-key"]
    with zipfile.ZipFile(io.BytesIO(bundle)) as archive:
        assert len(archive.namelist()) == 16
        for name in names:
            entry = f"io/github/world-in-progress/c-two-sdk/{VERSION}/{name}"
            data = (staging / name).read_bytes()
            assert archive.read(entry) == data
            assert archive.read(entry + ".asc") == b"signature"
            assert archive.read(entry + ".md5").decode() == hashlib.md5(data).hexdigest()
            assert archive.read(entry + ".sha1").decode() == hashlib.sha1(data).hexdigest()


def test_maven_central_resume_allows_partial_repo1_visibility(tmp_path, monkeypatch):
    import promote_release_candidate as promote

    extraction = tmp_path / "extracted"
    extraction.mkdir()
    make_package(extraction)
    entries = [{"name": name, "bytes": (extraction / name).stat().st_size,
                "sha256": candidate.sha256_file(extraction / name)}
               for name in candidate.java_filenames(VERSION)]
    first = entries[0]
    monkeypatch.setattr(
        promote.maven_publish, "remote_digest",
        lambda version, name: first["sha256"] if name == first["name"] else None,
    )
    with pytest.raises(promote.PromotionError, match="partially published"):
        promote.maven_central_state(extraction, entries, tmp_path / "s1/maven", tmp_path / "s1", VERSION, None)
    resumed = promote.maven_central_state(extraction, entries, tmp_path / "s2/maven", tmp_path / "s2", VERSION, DEPLOYMENT)
    assert resumed["action"] == "upload"
    assert [f["disposition"] for f in resumed["files"]] == ["verified-existing", "upload", "upload", "upload"]
    assert all((tmp_path / "s2/maven" / e["name"]).is_file() for e in entries)
