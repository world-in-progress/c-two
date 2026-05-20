use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::path::PathBuf;

fn valid_contract_json() -> String {
    r#"{
      "schema": "c-two.contract.v1",
      "crm": {
        "namespace": "test.contract",
        "name": "Portable",
        "version": "0.1.0"
      },
      "fingerprints": {
        "abi_hash": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "signature_hash": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
      },
      "methods": [
        {
          "access": "write",
          "buffer": "view",
          "name": "echo",
          "parameters": [
            {
              "name": "value",
              "kind": "POSITIONAL_OR_KEYWORD",
              "default": {"kind": "missing"},
              "type": {
                "kind": "codec",
                "codec": {
                  "kind": "codec_ref",
                  "id": "org.example.codec",
                  "version": "1",
                  "schema_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                  "portable": true
                }
              }
            }
          ],
          "return": {
            "kind": "codec",
            "codec": {
              "kind": "codec_ref",
              "id": "org.example.codec",
              "version": "1",
              "schema_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
              "portable": true
            }
          },
          "wire": {
            "input": {
              "kind": "codec_ref",
              "id": "org.example.codec",
              "version": "1",
              "schema_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
              "portable": true
            },
            "output": {
              "kind": "codec_ref",
              "id": "org.example.codec",
              "version": "1",
              "schema_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
              "portable": true
            }
          }
        }
      ]
    }"#
    .to_string()
}

fn invalid_pickle_contract_json() -> String {
    valid_contract_json().replace(
        r#""input": {
              "kind": "codec_ref",
              "id": "org.example.codec",
              "version": "1",
              "schema_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
              "portable": true
            }"#,
        r#""input": {
              "family": "python-pickle-default",
              "kind": "builtin",
              "portable": false,
              "version": "pickle-protocol-5"
            }"#,
    )
}

fn fastdb_call_db_contract_json() -> String {
    r#"{
      "schema": "c-two.contract.v1",
      "crm": {
        "namespace": "test.contract.fastdb",
        "name": "FastdbPortable",
        "version": "0.1.0"
      },
      "fingerprints": {
        "abi_hash": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "signature_hash": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
      },
      "methods": [
        {
          "access": "write",
          "buffer": "view",
          "name": "query",
          "parameters": [],
          "return": {
            "kind": "codec",
            "codec": {
              "kind": "codec_ref",
              "id": "org.fastdb.call-db",
              "version": "1",
              "schema": "fastdb.call-db.schema.v1",
              "schema_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
              "capabilities": ["bytes", "buffer-view"],
              "portable": true
            }
          },
          "wire": {
            "input": null,
            "output": {
              "kind": "codec_ref",
              "id": "org.fastdb.call-db",
              "version": "1",
              "schema": "fastdb.call-db.schema.v1",
              "schema_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
              "capabilities": ["bytes", "buffer-view"],
              "portable": true
            }
          }
        }
      ]
    }"#
    .to_string()
}

#[cfg(unix)]
fn fake_python(tempdir: &tempfile::TempDir, payload: &str) -> PathBuf {
    let path = tempdir.path().join("python-fake");
    let script =
        format!("#!/bin/sh\ncat <<'C_TWO_CONTRACT_JSON'\n{payload}\nC_TWO_CONTRACT_JSON\n");
    std::fs::write(&path, script).unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    path
}

#[cfg(unix)]
fn fake_python_requiring_arg(
    tempdir: &tempfile::TempDir,
    required_arg: &str,
    matched_payload: &str,
    missing_payload: &str,
) -> PathBuf {
    let path = tempdir.path().join("python-fake-requires-arg");
    let script = format!(
        "#!/bin/sh\ncase \" $* \" in\n  *\" {required_arg} \"*) cat <<'C_TWO_CONTRACT_MATCHED'\n{matched_payload}\nC_TWO_CONTRACT_MATCHED\n    ;;\n  *) cat <<'C_TWO_CONTRACT_MISSING'\n{missing_payload}\nC_TWO_CONTRACT_MISSING\n    ;;\nesac\n"
    );
    std::fs::write(&path, script).unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    path
}

#[cfg(unix)]
fn fake_fastdb_typescript_pythonpath(tempdir: &tempfile::TempDir) -> PathBuf {
    let package = tempdir.path().join("c_two/fastdb");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(tempdir.path().join("c_two/__init__.py"), "").unwrap();
    std::fs::write(package.join("__init__.py"), "").unwrap();
    std::fs::write(
        package.join("typescript.py"),
        r#"import sys

if len(sys.argv) != 5 or sys.argv[3] != "--schema":
    raise SystemExit(f"unexpected fastdb typescript generator args: {sys.argv[1:]}")

with open(sys.argv[2], "w", encoding="utf-8") as out:
    out.write("// fake C-Two FastDB helper generated by c_two.fastdb.typescript\n")
    out.write("export const GENERATED_C_TWO_FASTDB_HELPER = true;\n")
"#,
    )
    .unwrap();
    tempdir.path().to_path_buf()
}

fn assert_valid_contract_file(path: &Path) {
    let payload = std::fs::read_to_string(path).unwrap();
    c2_contract::validate_portable_contract_descriptor_json(payload.as_bytes()).unwrap();
}

#[test]
fn contract_help_lists_descriptor_commands() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args(["contract", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("artifacts"))
        .stdout(predicate::str::contains("codegen"))
        .stdout(predicate::str::contains("diagnose"))
        .stdout(predicate::str::contains("export"))
        .stdout(predicate::str::contains("infer"))
        .stdout(predicate::str::contains("validate"));
}

#[cfg(unix)]
#[test]
fn contract_artifacts_wraps_python_without_contract_validation() {
    let tempdir = tempfile::tempdir().unwrap();
    let artifacts = r#"[{"schema":"example.schema.v1","type":"Payload"}]"#;
    let python = fake_python(&tempdir, artifacts);
    let output = tempdir.path().join("payload-abi-artifacts.json");

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args([
        "contract",
        "artifacts",
        "example.contracts:Grid",
        "--python",
        python.to_str().unwrap(),
        "--out",
        output.to_str().unwrap(),
    ])
    .assert()
    .success()
    .stdout(predicate::str::is_empty());

    assert_eq!(std::fs::read_to_string(output).unwrap().trim(), artifacts);
}

#[cfg(unix)]
#[test]
fn contract_diagnose_wraps_python_and_validates_diagnostic_shape() {
    let tempdir = tempfile::tempdir().unwrap();
    let diagnostics = r#"[{"code":"python_only_pickle","message":"Python-only fallback","method":"echo","position":"input","severity":"warning"}]"#;
    let python = fake_python(&tempdir, diagnostics);
    let output = tempdir.path().join("diagnostics.json");

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args([
        "contract",
        "diagnose",
        "example.contracts:Grid",
        "--python",
        python.to_str().unwrap(),
        "--out",
        output.to_str().unwrap(),
    ])
    .assert()
    .success()
    .stdout(predicate::str::is_empty());

    assert_eq!(std::fs::read_to_string(output).unwrap().trim(), diagnostics);
}

#[cfg(unix)]
#[test]
fn contract_diagnose_rejects_non_array_python_output() {
    let tempdir = tempfile::tempdir().unwrap();
    let python = fake_python(&tempdir, r#"{"code":"python_only_pickle"}"#);

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args([
        "contract",
        "diagnose",
        "example.contracts:Grid",
        "--python",
        python.to_str().unwrap(),
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains(
        "diagnostic output must be a JSON array",
    ));
}

#[cfg(unix)]
#[test]
fn contract_diagnose_rejects_non_object_diagnostics() {
    let tempdir = tempfile::tempdir().unwrap();
    let python = fake_python(&tempdir, r#"["python_only_pickle"]"#);

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args([
        "contract",
        "diagnose",
        "example.contracts:Grid",
        "--python",
        python.to_str().unwrap(),
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains(
        "diagnostic output must be a JSON array of objects",
    ));
}

#[test]
fn contract_validate_accepts_file() {
    let tempdir = tempfile::tempdir().unwrap();
    let path = tempdir.path().join("contract.json");
    std::fs::write(&path, valid_contract_json()).unwrap();

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args(["contract", "validate", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("valid c-two.contract.v1"))
        .stdout(predicate::str::contains("sha256="));
}

#[test]
fn contract_validate_accepts_stdin() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args(["contract", "validate", "-"])
        .write_stdin(valid_contract_json())
        .assert()
        .success()
        .stdout(predicate::str::contains("stdin: valid c-two.contract.v1"));
}

#[test]
fn contract_validate_rejects_pickle_wire_ref() {
    let tempdir = tempfile::tempdir().unwrap();
    let path = tempdir.path().join("contract.json");
    std::fs::write(&path, invalid_pickle_contract_json()).unwrap();

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args(["contract", "validate", path.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("python-pickle-default"));
}

#[cfg(unix)]
#[test]
fn contract_export_wraps_python_and_validates_output() {
    let tempdir = tempfile::tempdir().unwrap();
    let python = fake_python(&tempdir, &valid_contract_json());

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args([
        "contract",
        "export",
        "example.contracts:Grid",
        "--python",
        python.to_str().unwrap(),
        "--method",
        "echo",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains(r#""schema": "c-two.contract.v1""#));
}

#[cfg(unix)]
#[test]
fn contract_infer_wraps_python_and_writes_validated_output() {
    let tempdir = tempfile::tempdir().unwrap();
    let python = fake_python(&tempdir, &valid_contract_json());
    let output = tempdir.path().join("inferred.contract.json");

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args([
        "contract",
        "infer",
        "example.resources:GridResource",
        "--python",
        python.to_str().unwrap(),
        "--namespace",
        "example.grid",
        "--version",
        "0.1.0",
        "--name",
        "Grid",
        "--method",
        "echo",
        "--out",
        output.to_str().unwrap(),
    ])
    .assert()
    .success()
    .stdout(predicate::str::is_empty());

    assert_valid_contract_file(&output);
}

#[cfg(unix)]
#[test]
fn contract_infer_diagnose_wraps_python_and_validates_diagnostic_shape() {
    let tempdir = tempfile::tempdir().unwrap();
    let diagnostics = r#"[{"code":"python_only_pickle","message":"Python-only fallback","method":"echo","position":"input","severity":"warning"}]"#;
    let python =
        fake_python_requiring_arg(&tempdir, "--diagnose", diagnostics, &valid_contract_json());
    let output = tempdir.path().join("inferred.diagnostics.json");

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args([
        "contract",
        "infer",
        "example.resources:GridResource",
        "--python",
        python.to_str().unwrap(),
        "--namespace",
        "example.grid",
        "--version",
        "0.1.0",
        "--name",
        "Grid",
        "--method",
        "echo",
        "--diagnose",
        "--out",
        output.to_str().unwrap(),
    ])
    .assert()
    .success()
    .stdout(predicate::str::is_empty());

    assert_eq!(std::fs::read_to_string(output).unwrap().trim(), diagnostics);
}

#[cfg(unix)]
#[test]
fn contract_infer_diagnose_rejects_non_array_python_output() {
    let tempdir = tempfile::tempdir().unwrap();
    let python = fake_python(&tempdir, r#"{"code":"python_only_pickle"}"#);

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args([
        "contract",
        "infer",
        "example.resources:GridResource",
        "--python",
        python.to_str().unwrap(),
        "--namespace",
        "example.grid",
        "--version",
        "0.1.0",
        "--method",
        "echo",
        "--diagnose",
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains(
        "diagnostic output must be a JSON array",
    ));
}

#[cfg(unix)]
#[test]
fn contract_infer_diagnose_rejects_non_object_diagnostics() {
    let tempdir = tempfile::tempdir().unwrap();
    let python = fake_python(&tempdir, r#"["python_only_pickle"]"#);

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args([
        "contract",
        "infer",
        "example.resources:GridResource",
        "--python",
        python.to_str().unwrap(),
        "--namespace",
        "example.grid",
        "--version",
        "0.1.0",
        "--method",
        "echo",
        "--diagnose",
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains(
        "diagnostic output must be a JSON array of objects",
    ));
}

#[cfg(unix)]
#[test]
fn contract_infer_artifacts_wraps_python_and_validates_artifact_shape() {
    let tempdir = tempfile::tempdir().unwrap();
    let artifacts = r#"[{"schema":"example.schema.v1","type":"Payload"}]"#;
    let python =
        fake_python_requiring_arg(&tempdir, "--artifacts", artifacts, &valid_contract_json());
    let output = tempdir.path().join("inferred.payload-abi-artifacts.json");

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args([
        "contract",
        "infer",
        "example.resources:GridResource",
        "--python",
        python.to_str().unwrap(),
        "--namespace",
        "example.grid",
        "--version",
        "0.1.0",
        "--name",
        "Grid",
        "--method",
        "echo",
        "--artifacts",
        "--out",
        output.to_str().unwrap(),
    ])
    .assert()
    .success()
    .stdout(predicate::str::is_empty());

    assert_eq!(std::fs::read_to_string(output).unwrap().trim(), artifacts);
}

#[cfg(unix)]
#[test]
fn contract_infer_artifacts_rejects_non_array_python_output() {
    let tempdir = tempfile::tempdir().unwrap();
    let python = fake_python(&tempdir, r#"{"schema":"example.schema.v1"}"#);

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args([
        "contract",
        "infer",
        "example.resources:GridResource",
        "--python",
        python.to_str().unwrap(),
        "--namespace",
        "example.grid",
        "--version",
        "0.1.0",
        "--method",
        "echo",
        "--artifacts",
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains(
        "payload ABI artifact output must be a JSON array",
    ));
}

#[cfg(unix)]
#[test]
fn contract_infer_artifacts_rejects_non_object_artifacts() {
    let tempdir = tempfile::tempdir().unwrap();
    let python = fake_python(&tempdir, r#"["payload-abi-artifact"]"#);

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args([
        "contract",
        "infer",
        "example.resources:GridResource",
        "--python",
        python.to_str().unwrap(),
        "--namespace",
        "example.grid",
        "--version",
        "0.1.0",
        "--method",
        "echo",
        "--artifacts",
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains(
        "payload ABI artifact output must be a JSON array of objects",
    ));
}

#[cfg(unix)]
#[test]
fn contract_infer_rejects_conflicting_artifact_and_diagnostic_modes() {
    let tempdir = tempfile::tempdir().unwrap();
    let python = fake_python(&tempdir, "[]");

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args([
        "contract",
        "infer",
        "example.resources:GridResource",
        "--python",
        python.to_str().unwrap(),
        "--namespace",
        "example.grid",
        "--version",
        "0.1.0",
        "--method",
        "echo",
        "--diagnose",
        "--artifacts",
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains(
        "--diagnose and --artifacts cannot be used together",
    ));
}

#[cfg(unix)]
#[test]
fn contract_export_rejects_invalid_python_descriptor() {
    let tempdir = tempfile::tempdir().unwrap();
    let python = fake_python(&tempdir, &invalid_pickle_contract_json());

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args([
        "contract",
        "export",
        "example.contracts:Grid",
        "--python",
        python.to_str().unwrap(),
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains("python-pickle-default"));
}

#[test]
fn contract_codegen_typescript_writes_output() {
    let tempdir = tempfile::tempdir().unwrap();
    let contract = tempdir.path().join("contract.json");
    let output = tempdir.path().join("client.ts");
    std::fs::write(&contract, valid_contract_json()).unwrap();

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args([
        "contract",
        "codegen",
        "typescript",
        contract.to_str().unwrap(),
        "--out",
        output.to_str().unwrap(),
    ])
    .assert()
    .success()
    .stdout(predicate::str::is_empty());

    let generated = std::fs::read_to_string(output).unwrap();
    assert!(generated.contains("export class PortableClient"));
    assert!(generated.contains("export const PORTABLE_CODEC_REQUIREMENTS"));
}

#[test]
fn contract_codegen_typescript_hold_ignores_from_buffer_without_capability() {
    if std::process::Command::new("tsc")
        .arg("--version")
        .output()
        .is_err()
        || std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_err()
    {
        return;
    }

    let tempdir = tempfile::tempdir().unwrap();
    let contract = tempdir.path().join("contract.json");
    let output = tempdir.path().join("client.ts");
    let smoke = tempdir.path().join("smoke.ts");
    let tsconfig = tempdir.path().join("tsconfig.json");
    std::fs::write(&contract, valid_contract_json()).unwrap();

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args([
        "contract",
        "codegen",
        "typescript",
        contract.to_str().unwrap(),
        "--out",
        output.to_str().unwrap(),
    ])
    .assert()
    .success()
    .stdout(predicate::str::is_empty());

    std::fs::write(
        &smoke,
        r#"import {
  PORTABLE_CODEC_REQUIREMENTS,
  PortableClient,
  codecRequirementKey,
  createPortableClientCodecTransport,
  type C2CodecImplementation,
  type C2CodecRegistry,
  type C2EncodedClientTransport,
  type C2ResponsePayload,
} from "./client.js";

const requirement = PORTABLE_CODEC_REQUIREMENTS[0];
const registry: C2CodecRegistry = {
  [codecRequirementKey(requirement)]: {
    id: requirement.id,
    requirement,
    encode(_value: unknown): Uint8Array {
      return new Uint8Array([1]);
    },
    decode(payload: C2ResponsePayload): unknown {
      if (!(payload instanceof Uint8Array)) {
        throw new Error("ordinary decode expected Uint8Array payload");
      }
      return `decoded:${payload[0] ?? 0}`;
    },
    fromBuffer(_payload: C2ResponsePayload): unknown {
      throw new Error("fromBuffer must not be used without buffer-view capability");
    },
  },
};

const wire: C2EncodedClientTransport = {
  async call(_routeName, _contract, _method, _payload) {
    return new Uint8Array([7]);
  },
};

const client = new PortableClient(createPortableClientCodecTransport(wire, registry), "route");
const held = await client.hold_echo({ codecId: "org.example.codec", value: "x" });
if ((held.value as unknown) !== "decoded:7") {
  throw new Error(`hold used the wrong response path: ${String(held.value)}`);
}
const heldBuffer = held.buffer;
if (!(heldBuffer instanceof Uint8Array)) {
  throw new Error("hold did not expose a Uint8Array response buffer for a Uint8Array wire response");
}
if (heldBuffer[0] !== 7) {
  throw new Error(`hold did not expose the retained response buffer: ${heldBuffer[0]}`);
}
held.release();
let releasedValueThrown = false;
try {
  void held.value;
} catch (error) {
  releasedValueThrown = true;
  if (!String(error).includes("value no longer accessible")) {
    throw new Error(`unexpected released value error ${String(error)}`);
  }
}
if (!releasedValueThrown) {
  throw new Error("hold value remained accessible after release");
}
let releasedBufferThrown = false;
try {
  void held.buffer;
} catch (error) {
  releasedBufferThrown = true;
  if (!String(error).includes("buffer no longer accessible")) {
    throw new Error(`unexpected released buffer error ${String(error)}`);
  }
}
if (!releasedBufferThrown) {
  throw new Error("hold buffer remained accessible after release");
}

const badEncodeRegistry: C2CodecRegistry = {
  [codecRequirementKey(requirement)]: {
    id: requirement.id,
    requirement,
    encode(_value: unknown): Uint8Array {
      return "not-bytes" as unknown as Uint8Array;
    },
    decode(payload: C2ResponsePayload): unknown {
      if (!(payload instanceof Uint8Array)) {
        throw new Error("bad encode registry decode expected Uint8Array payload");
      }
      return `decoded:${payload[0] ?? 0}`;
    },
  },
};
let badEncodeReachedWire = false;
const badEncodeWire: C2EncodedClientTransport = {
  async call(_routeName, _contract, _method, _payload) {
    badEncodeReachedWire = true;
    return new Uint8Array([1]);
  },
};
let badEncodeThrown = false;
try {
  await new PortableClient(createPortableClientCodecTransport(badEncodeWire, badEncodeRegistry), "route").echo({ codecId: "org.example.codec", value: "x" });
} catch (error) {
  badEncodeThrown = true;
  if (!String(error).includes("encode result") || !String(error).includes("Uint8Array")) {
    throw new Error(`unexpected bad codec encode result error ${String(error)}`);
  }
}
if (!badEncodeThrown) {
  throw new Error("codec transport accepted a non-Uint8Array encode result");
}
if (badEncodeReachedWire) {
  throw new Error("codec transport forwarded a non-Uint8Array encode result to the wire transport");
}

const missingEncodeRegistry: C2CodecRegistry = {
  [codecRequirementKey(requirement)]: {
    id: requirement.id,
    requirement,
    encode: undefined as unknown as C2CodecRegistry[string]["encode"],
    decode(payload: C2ResponsePayload): unknown {
      if (!(payload instanceof Uint8Array)) {
        throw new Error("missing encode registry decode expected Uint8Array payload");
      }
      return `decoded:${payload[0] ?? 0}`;
    },
  },
};
let missingEncodeThrown = false;
try {
  await new PortableClient(createPortableClientCodecTransport(wire, missingEncodeRegistry), "route").echo({ codecId: "org.example.codec", value: "x" });
} catch (error) {
  missingEncodeThrown = true;
  if (!String(error).includes("implementation encode must be a function")) {
    throw new Error(`unexpected missing codec encode error ${String(error)}`);
  }
}
if (!missingEncodeThrown) {
  throw new Error("codec transport accepted a codec implementation without encode");
}

const missingDecodeRegistry: C2CodecRegistry = {
  [codecRequirementKey(requirement)]: {
    id: requirement.id,
    requirement,
    encode(_value: unknown): Uint8Array {
      return new Uint8Array([1]);
    },
    decode: undefined as unknown as C2CodecRegistry[string]["decode"],
  },
};
let missingDecodeThrown = false;
try {
  await new PortableClient(createPortableClientCodecTransport(wire, missingDecodeRegistry), "route").echo({ codecId: "org.example.codec", value: "x" });
} catch (error) {
  missingDecodeThrown = true;
  if (!String(error).includes("implementation decode must be a function")) {
    throw new Error(`unexpected missing codec decode error ${String(error)}`);
  }
}
if (!missingDecodeThrown) {
  throw new Error("codec transport accepted a codec implementation without decode");
}

const ownedResponse = { byteLength: 5, tag: "provider-owned" };
const ownedResponseWire: C2EncodedClientTransport = {
  async call(_routeName, _contract, _method, _payload) {
    return ownedResponse;
  },
};
let ownedResponseSeenByDecode = false;
const ownedResponseRegistry: C2CodecRegistry = {
  [codecRequirementKey(requirement)]: {
    id: requirement.id,
    requirement,
    encode(_value: unknown): Uint8Array {
      return new Uint8Array([1]);
    },
    decode(payload: C2ResponsePayload): unknown {
      if (payload !== ownedResponse) {
        throw new Error("codec did not receive provider-owned response payload");
      }
      ownedResponseSeenByDecode = true;
      return "decoded-owned-response";
    },
  },
};
const ownedResponseResult = await new PortableClient(createPortableClientCodecTransport(ownedResponseWire, ownedResponseRegistry), "route").echo({ codecId: "org.example.codec", value: "x" }) as unknown;
if (ownedResponseResult !== "decoded-owned-response" || !ownedResponseSeenByDecode) {
  throw new Error("codec transport did not pass provider-owned response payload to decode");
}

let decodeFailureResponseReleaseCount = 0;
const decodeFailureResponse = {
  byteLength: 4,
  release(): void {
    decodeFailureResponseReleaseCount += 1;
  },
};
const decodeFailureWire: C2EncodedClientTransport = {
  async call(_routeName, _contract, _method, _payload) {
    return decodeFailureResponse;
  },
};
const decodeFailureRegistry: C2CodecRegistry = {
  [codecRequirementKey(requirement)]: {
    id: requirement.id,
    requirement,
    encode(_value: unknown): Uint8Array {
      return new Uint8Array([1]);
    },
    decode(payload: C2ResponsePayload): unknown {
      if (payload !== decodeFailureResponse) {
        throw new Error("decode failure did not receive provider-owned response payload");
      }
      throw new Error("provider decode failed");
    },
  },
};
let decodeFailureThrown = false;
try {
  await new PortableClient(createPortableClientCodecTransport(decodeFailureWire, decodeFailureRegistry), "route").echo({ codecId: "org.example.codec", value: "x" });
} catch (error) {
  decodeFailureThrown = true;
  if (!String(error).includes("provider decode failed")) {
    throw new Error(`unexpected provider decode failure error ${String(error)}`);
  }
}
if (!decodeFailureThrown) {
  throw new Error("codec transport accepted a provider decode failure");
}
if (decodeFailureResponseReleaseCount !== 1) {
  throw new Error(`codec transport did not release provider-owned response after decode failure, got ${decodeFailureResponseReleaseCount}`);
}

let heldDecodeFallbackBufferReleaseCount = 0;
const heldDecodeFallbackResponse = {
  byteLength: 6,
  release(): void {
    heldDecodeFallbackBufferReleaseCount += 1;
  },
};
const heldDecodeFallbackWire: C2EncodedClientTransport = {
  async call(_routeName, _contract, _method, _payload) {
    return heldDecodeFallbackResponse;
  },
};
const heldDecodeFallbackRegistry: C2CodecRegistry = {
  [codecRequirementKey(requirement)]: {
    id: requirement.id,
    requirement,
    encode(_value: unknown): Uint8Array {
      return new Uint8Array([1]);
    },
    decode(payload: C2ResponsePayload): unknown {
      if (payload !== heldDecodeFallbackResponse) {
        throw new Error("held decode fallback did not receive provider-owned response payload");
      }
      return "held-decode-fallback";
    },
  },
};
const heldDecodeFallback = await new PortableClient(createPortableClientCodecTransport(heldDecodeFallbackWire, heldDecodeFallbackRegistry), "route").hold_echo({ codecId: "org.example.codec", value: "x" });
if ((heldDecodeFallback.value as unknown) !== "held-decode-fallback") {
  throw new Error("held decode fallback returned the wrong materialized value");
}
if (heldDecodeFallback.buffer !== heldDecodeFallbackResponse) {
  throw new Error("held decode fallback did not retain the provider-owned response buffer");
}
heldDecodeFallback.release();
if (heldDecodeFallbackBufferReleaseCount !== 1) {
  throw new Error(`held decode fallback did not release provider-owned response buffer, got ${heldDecodeFallbackBufferReleaseCount}`);
}

let missingCodecResponseReleaseCount = 0;
const missingCodecResponse = {
  byteLength: 8,
  release(): void {
    missingCodecResponseReleaseCount += 1;
  },
};
const missingCodecWire: C2EncodedClientTransport = {
  async call(_routeName, _contract, _method, _payload) {
    return missingCodecResponse;
  },
};
const missingCodecEntries: Record<string, C2CodecImplementation<string>> = {};
const missingCodecRegistry: C2CodecRegistry = missingCodecEntries;
missingCodecEntries[codecRequirementKey(requirement)] = {
  id: requirement.id,
  requirement,
  encode(_value: unknown): Uint8Array {
    delete missingCodecEntries[codecRequirementKey(requirement)];
    return new Uint8Array([1]);
  },
  decode(_payload: C2ResponsePayload): unknown {
    throw new Error("missing output codec lookup must fail before decode");
  },
};
let missingCodecThrown = false;
try {
  await new PortableClient(createPortableClientCodecTransport(missingCodecWire, missingCodecRegistry), "route").echo({ codecId: "org.example.codec", value: "x" });
} catch (error) {
  missingCodecThrown = true;
  if (!String(error).includes("requires application runtime implementation")) {
    throw new Error(`unexpected missing codec implementation error ${String(error)}`);
  }
}
if (!missingCodecThrown) {
  throw new Error("codec transport accepted a missing codec implementation");
}
if (missingCodecResponseReleaseCount !== 1) {
  throw new Error(`codec transport did not release provider-owned response after missing codec lookup, got ${missingCodecResponseReleaseCount}`);
}

const badResponseWire: C2EncodedClientTransport = {
  async call(_routeName, _contract, _method, _payload) {
    return "not-bytes" as unknown as Uint8Array;
  },
};
let badResponseThrown = false;
try {
  await new PortableClient(createPortableClientCodecTransport(badResponseWire, registry), "route").echo({ codecId: "org.example.codec", value: "x" });
} catch (error) {
  badResponseThrown = true;
  if (!String(error).includes("encoded transport response") || !String(error).includes("provider-owned response payload")) {
    throw new Error(`unexpected bad encoded response error ${String(error)}`);
  }
}
if (!badResponseThrown) {
  throw new Error("codec transport accepted a malformed wire response");
}
"#,
    )
    .unwrap();
    std::fs::write(
        &tsconfig,
        r#"{
  "compilerOptions": {
    "module": "NodeNext",
    "moduleResolution": "NodeNext",
    "target": "ES2022",
    "strict": true,
    "skipLibCheck": true,
    "outDir": "dist"
  },
  "include": ["*.ts"]
}
"#,
    )
    .unwrap();
    std::fs::write(tempdir.path().join("package.json"), r#"{"type":"module"}"#).unwrap();

    let tsc_status = std::process::Command::new("tsc")
        .arg("-p")
        .arg(&tsconfig)
        .current_dir(tempdir.path())
        .status()
        .unwrap();
    assert!(tsc_status.success());

    let node_status = std::process::Command::new("node")
        .arg(tempdir.path().join("dist").join("smoke.js"))
        .current_dir(tempdir.path())
        .status()
        .unwrap();
    assert!(node_status.success());
}

#[test]
fn contract_codegen_typescript_strict_rejects_external_codec() {
    let tempdir = tempfile::tempdir().unwrap();
    let contract = tempdir.path().join("contract.json");
    std::fs::write(&contract, valid_contract_json()).unwrap();

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args([
        "contract",
        "codegen",
        "typescript",
        contract.to_str().unwrap(),
        "--strict-codecs",
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains(
        "unsupported codec org.example.codec",
    ));
}

#[cfg(unix)]
#[test]
fn contract_codegen_typescript_fastdb_schema_runs_c_two_helper_generator() {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }

    let tempdir = tempfile::tempdir().unwrap();
    let contract = tempdir.path().join("fastdb.contract.json");
    let schema = tempdir.path().join("fastdb.payload-abi-artifacts.json");
    let output = tempdir.path().join("fastdb-client.ts");
    let fastdb_output = tempdir.path().join("fastdb-codecs.ts");
    let pythonpath = fake_fastdb_typescript_pythonpath(&tempdir);
    std::fs::write(&contract, fastdb_call_db_contract_json()).unwrap();
    std::fs::write(&schema, r#"[{"schema":"fastdb.call-db.schema.v1"}]"#).unwrap();

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.env("PYTHONPATH", pythonpath);
    cmd.args([
        "contract",
        "codegen",
        "typescript",
        contract.to_str().unwrap(),
        "--strict-codecs",
        "--out",
        output.to_str().unwrap(),
        "--fastdb-schema",
        schema.to_str().unwrap(),
        "--fastdb-out",
        fastdb_output.to_str().unwrap(),
        "--python",
        "python3",
    ])
    .assert()
    .success()
    .stdout(predicate::str::is_empty());

    let generated = std::fs::read_to_string(output).unwrap();
    assert!(generated.contains("export class FastdbPortableClient"));
    let fastdb_generated = std::fs::read_to_string(fastdb_output).unwrap();
    assert!(fastdb_generated.contains("GENERATED_C_TWO_FASTDB_HELPER"));
}

#[test]
fn contract_codegen_typescript_strict_fastdb_output_compiles_with_tsc() {
    if std::process::Command::new("tsc")
        .arg("--version")
        .output()
        .is_err()
        || std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_err()
    {
        return;
    }

    let tempdir = tempfile::tempdir().unwrap();
    let contract = tempdir.path().join("fastdb.contract.json");
    let output = tempdir.path().join("fastdb-client.ts");
    let smoke = tempdir.path().join("smoke.ts");
    std::fs::write(&contract, fastdb_call_db_contract_json()).unwrap();
    std::fs::write(tempdir.path().join("package.json"), r#"{"type":"module"}"#).unwrap();

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args([
        "contract",
        "codegen",
        "typescript",
        contract.to_str().unwrap(),
        "--strict-codecs",
        "--out",
        output.to_str().unwrap(),
    ])
    .assert()
    .success()
    .stdout(predicate::str::is_empty());

    std::fs::write(
        &smoke,
        r#"import {
  FASTDB_PORTABLE_CODEC_REQUIREMENTS,
  FASTDB_PORTABLE_CONTRACT,
  type C2CodecImplementation,
  type C2CodecRegistry,
  type C2EncodedClientTransport,
  type C2Fetch,
  type C2IpcConnection,
  type C2NodeIpcNet,
  type C2IpcRequestShmBlock,
  type C2IpcRequestShmWriter,
  type C2IpcResponseShmReader,
  type C2IpcResponseShmBlock,
  type C2MemFfiRequestPoolBinding,
  type C2MemFfiResponsePoolBinding,
  type C2MemFfiResponsePoolFactory,
  type C2NodePosixNativeBuddyRequestShmBackend,
  type C2NodePosixNativeBuddyResponseShmBackend,
  type C2NodePosixShmFileHandle,
  type C2NodePosixShmFileSystem,
  type C2ResponsePayload,
  C2CrmMethodError,
  C2HttpTransportError,
  C2IpcRouteNotFoundError,
  C2IpcTransportError,
  FastdbPortableClient,
  codecRequirementKey,
  createFastdbPortableClientCodecTransport,
  createHttpRelayEncodedTransport,
  createIpcEncodedTransport,
  createNodeIpcConnect,
  createC2MemFfiNativeBuddyRequestShmWriter,
  createC2MemFfiNativeBuddyResponseShmReader,
  createNodePosixDedicatedRequestShmWriter,
  createNodePosixDedicatedResponseShmReader,
  createNodePosixNativeBuddyRequestShmWriter,
  createNodePosixNativeBuddyResponseShmReader,
  createRelayAwareHttpEncodedTransport,
} from "./fastdb-client.js";

type SmokeByteArray = Uint8Array & { readonly buffer: ArrayBufferLike };

const fetchCalls: Array<{ input: string; init: { method?: string; headers?: Record<string, string>; body?: Uint8Array } }> = [];
const fetchImpl = async (input: string, init: { method?: string; headers?: Record<string, string>; body?: Uint8Array }) => {
  fetchCalls.push({ input, init });
  return {
    status: 200,
    async arrayBuffer(): Promise<ArrayBufferLike> {
      return (init.body ?? new Uint8Array()).buffer;
    },
    async text(): Promise<string> {
      return "";
    },
  };
};

const wire: C2EncodedClientTransport = {
  async call(_routeName, _contract, _method, payload) {
    return payload;
  },
};

const registryEntries: Record<string, C2CodecImplementation<string>> = {};
let heldCloseCount = 0;
for (const requirement of FASTDB_PORTABLE_CODEC_REQUIREMENTS) {
  registryEntries[codecRequirementKey(requirement)] = {
    id: requirement.id,
    requirement,
    encode(_value: unknown): Uint8Array {
      return new Uint8Array([1]);
    },
    decode(payload: Uint8Array): unknown {
      return payload.byteLength;
    },
    fromBuffer(payload: Uint8Array): unknown {
      return {
        payload,
        close(): void {
          heldCloseCount += 1;
        },
      };
    },
  };
}
const registry: C2CodecRegistry = registryEntries;

const client = new FastdbPortableClient(
  createFastdbPortableClientCodecTransport(wire, registry),
  "route",
);
const result: Promise<unknown> = client.query();
void result;
const heldFastdb = await client.hold_query();
if ((heldFastdb.value as { payload: Uint8Array }).payload[0] !== undefined) {
  throw new Error("held no-input query unexpectedly received a non-empty payload");
}
if (heldFastdb.buffer.byteLength !== 0) {
  throw new Error(`held no-input query exposed unexpected buffer length ${heldFastdb.buffer.byteLength}`);
}
heldFastdb.release();
if (heldCloseCount !== 1) {
  throw new Error(`held fromBuffer value was not closed exactly once, got ${heldCloseCount}`);
}
let fastdbReleasedValueThrown = false;
try {
  void heldFastdb.value;
} catch {
  fastdbReleasedValueThrown = true;
}
if (!fastdbReleasedValueThrown) {
  throw new Error("fastdb held value remained accessible after release");
}

const ownedHeldResponse = { byteLength: 6, tag: "provider-owned-held" };
const ownedHeldWire: C2EncodedClientTransport = {
  async call(_routeName, _contract, _method, _payload) {
    return ownedHeldResponse;
  },
};
const ownedHeldEntries: Record<string, C2CodecImplementation<string>> = {};
let ownedHeldCloseCount = 0;
let ownedHeldFromBufferSeen = false;
for (const requirement of FASTDB_PORTABLE_CODEC_REQUIREMENTS) {
  ownedHeldEntries[codecRequirementKey(requirement)] = {
    id: requirement.id,
    requirement,
    encode(_value: unknown): Uint8Array {
      return new Uint8Array([1]);
    },
    decode(_payload: C2ResponsePayload): unknown {
      throw new Error("held provider-owned response must use fromBuffer");
    },
    fromBuffer(payload: C2ResponsePayload): unknown {
      if (payload !== ownedHeldResponse) {
        throw new Error("fromBuffer did not receive provider-owned held response payload");
      }
      ownedHeldFromBufferSeen = true;
      return {
        payload,
        close(): void {
          ownedHeldCloseCount += 1;
        },
      };
    },
  };
}
const heldOwnedFastdb = await new FastdbPortableClient(
  createFastdbPortableClientCodecTransport(ownedHeldWire, ownedHeldEntries),
  "route",
).hold_query();
if ((heldOwnedFastdb.value as { payload: unknown }).payload !== ownedHeldResponse || !ownedHeldFromBufferSeen) {
  throw new Error("held provider-owned response did not reach fromBuffer");
}
if (heldOwnedFastdb.buffer !== ownedHeldResponse) {
  throw new Error("held provider-owned response was not retained as the held buffer");
}
heldOwnedFastdb.release();
if (ownedHeldCloseCount !== 1) {
  throw new Error(`held provider-owned response value was not closed exactly once, got ${ownedHeldCloseCount}`);
}

let failingFromBufferResponseReleaseCount = 0;
const failingFromBufferResponse = {
  byteLength: 7,
  release(): void {
    failingFromBufferResponseReleaseCount += 1;
  },
};
const failingFromBufferWire: C2EncodedClientTransport = {
  async call(_routeName, _contract, _method, _payload) {
    return failingFromBufferResponse;
  },
};
const failingFromBufferEntries: Record<string, C2CodecImplementation<string>> = {};
for (const requirement of FASTDB_PORTABLE_CODEC_REQUIREMENTS) {
  failingFromBufferEntries[codecRequirementKey(requirement)] = {
    id: requirement.id,
    requirement,
    encode(_value: unknown): Uint8Array {
      return new Uint8Array([1]);
    },
    decode(_payload: C2ResponsePayload): unknown {
      throw new Error("held provider-owned response must use fromBuffer");
    },
    fromBuffer(payload: C2ResponsePayload): unknown {
      if (payload !== failingFromBufferResponse) {
        throw new Error("failing fromBuffer did not receive provider-owned response payload");
      }
      throw new Error("provider fromBuffer failed");
    },
  };
}
let failingFromBufferThrown = false;
try {
  await new FastdbPortableClient(
    createFastdbPortableClientCodecTransport(failingFromBufferWire, failingFromBufferEntries),
    "route",
  ).hold_query();
} catch (error) {
  failingFromBufferThrown = true;
  if (!String(error).includes("provider fromBuffer failed")) {
    throw new Error(`unexpected provider fromBuffer failure error ${String(error)}`);
  }
}
if (!failingFromBufferThrown) {
  throw new Error("codec transport accepted a provider fromBuffer failure");
}
if (failingFromBufferResponseReleaseCount !== 1) {
  throw new Error(`codec transport did not release provider-owned response after fromBuffer failure, got ${failingFromBufferResponseReleaseCount}`);
}

const badFromBufferEntries: Record<string, C2CodecImplementation<string>> = {};
for (const requirement of FASTDB_PORTABLE_CODEC_REQUIREMENTS) {
  badFromBufferEntries[codecRequirementKey(requirement)] = {
    id: requirement.id,
    requirement,
    encode(_value: unknown): Uint8Array {
      return new Uint8Array([1]);
    },
    decode(payload: Uint8Array): unknown {
      return payload.byteLength;
    },
    fromBuffer: "not-a-function" as unknown as C2CodecImplementation<string>["fromBuffer"],
  };
}
const badFromBufferRegistry: C2CodecRegistry = badFromBufferEntries;
let badFromBufferThrown = false;
try {
  await new FastdbPortableClient(
    createFastdbPortableClientCodecTransport(wire, badFromBufferRegistry),
    "route",
  ).hold_query();
} catch (error) {
  badFromBufferThrown = true;
  if (!String(error).includes("implementation fromBuffer must be a function")) {
    throw new Error(`unexpected bad codec fromBuffer error ${String(error)}`);
  }
}
if (!badFromBufferThrown) {
  throw new Error("codec transport accepted a non-function fromBuffer implementation");
}

const httpWire = createHttpRelayEncodedTransport("http://relay.example/base/", { fetch: fetchImpl });
const httpResult = await httpWire.call("route/with/slash", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
if (httpResult[0] !== 1) {
  throw new Error("HTTP relay transport did not return response bytes");
}
if (fetchCalls.length !== 1) {
  throw new Error(`expected one fetch call, got ${fetchCalls.length}`);
}
const fetchCall = fetchCalls[0];
if (fetchCall.input !== "http://relay.example/base/route%2Fwith%2Fslash/query") {
  throw new Error(`unexpected relay URL ${fetchCall.input}`);
}
if (fetchCall.init.headers?.["x-c2-expected-crm-ns"] !== "test.contract.fastdb") {
  throw new Error("missing expected CRM namespace header");
}
if (fetchCall.init.headers?.["x-c2-expected-abi-hash"] !== "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef") {
  throw new Error("missing expected ABI hash header");
}
if (fetchCall.init.headers?.["x-c2-expected-signature-hash"] !== "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789") {
  throw new Error("missing expected signature hash header");
}
if (fetchCall.init.headers?.["Content-Type"] !== "application/octet-stream") {
  throw new Error("missing expected data-plane content type header");
}

const ipcTextEncoder = new TextEncoder();
const ipcTextDecoder = new TextDecoder();
const C2_IPC_SMOKE_FLAG_RESPONSE = 1 << 1;
const C2_IPC_SMOKE_FLAG_HANDSHAKE = 1 << 2;
const C2_IPC_SMOKE_FLAG_BUDDY = 1 << 6;
const C2_IPC_SMOKE_FLAG_CALL_V2 = 1 << 7;
const C2_IPC_SMOKE_FLAG_REPLY_V2 = 1 << 8;
const C2_IPC_SMOKE_FLAG_CHUNKED = 1 << 9;
const C2_IPC_SMOKE_FLAG_CHUNK_LAST = 1 << 10;
const C2_IPC_SMOKE_BUDDY_FLAG_DEDICATED = 1 << 0;
const C2_IPC_SMOKE_CAP_CALL_V2 = 1 << 0;
const C2_IPC_SMOKE_CAP_METHOD_IDX = 1 << 1;
const C2_IPC_SMOKE_CAP_CHUNKED = 1 << 2;

function ipcConcat(chunks: readonly SmokeByteArray[]): SmokeByteArray {
  const total = chunks.reduce((sum, chunk) => sum + chunk.byteLength, 0);
  const out = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    out.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return out;
}

function ipcU16LE(value: number): Uint8Array {
  const out = new Uint8Array(2);
  new DataView(out.buffer).setUint16(0, value, true);
  return out;
}

function ipcU32LE(value: number): Uint8Array {
  const out = new Uint8Array(4);
  new DataView(out.buffer).setUint32(0, value, true);
  return out;
}

function ipcU64LE(value: number): Uint8Array {
  const out = new Uint8Array(8);
  new DataView(out.buffer).setBigUint64(0, BigInt(value), true);
  return out;
}

function ipcTextField(value: string): Uint8Array {
  const bytes = ipcTextEncoder.encode(value);
  return ipcConcat([new Uint8Array([bytes.byteLength]), bytes]);
}

function ipcFrame(requestId: number, flags: number, payload: Uint8Array): Uint8Array {
  const out = new Uint8Array(16 + payload.byteLength);
  const view = new DataView(out.buffer);
  view.setUint32(0, 12 + payload.byteLength, true);
  view.setBigUint64(4, BigInt(requestId), true);
  view.setUint32(12, flags, true);
  out.set(payload, 16);
  return out;
}

function ipcFrameHeader(requestId: number, flags: number, totalLen: number): Uint8Array {
  const out = new Uint8Array(16);
  const view = new DataView(out.buffer);
  view.setUint32(0, totalLen, true);
  view.setBigUint64(4, BigInt(requestId), true);
  view.setUint32(12, flags, true);
  return out;
}

function ipcServerHandshakeFrame(options: { readonly routeName?: string; readonly abiHash?: string; readonly maxPayloadSize?: number; readonly shmPrefix?: string; readonly shmSegments?: readonly { readonly name: string; readonly size: number }[]; readonly capabilities?: number } = {}): Uint8Array {
  const routeName = options.routeName ?? "route";
  const abiHash = options.abiHash ?? FASTDB_PORTABLE_CONTRACT.abiHash;
  const shmPrefix = options.shmPrefix ?? "";
  const shmSegments = options.shmSegments ?? [];
  const encodedSegments = shmSegments.map((segment) => ipcConcat([ipcU32LE(segment.size), ipcTextField(segment.name)]));
  const payload = ipcConcat([
    new Uint8Array([10]),
    ipcTextField(shmPrefix),
    ipcU16LE(shmSegments.length),
    ...encodedSegments,
    ipcU16LE(options.capabilities ?? (C2_IPC_SMOKE_CAP_CALL_V2 | C2_IPC_SMOKE_CAP_METHOD_IDX | C2_IPC_SMOKE_CAP_CHUNKED)),
    ipcTextField("server-smoke"),
    ipcTextField("instance-smoke"),
    ipcU16LE(1),
    ipcTextField(routeName),
    ipcTextField(FASTDB_PORTABLE_CONTRACT.namespace),
    ipcTextField(FASTDB_PORTABLE_CONTRACT.name),
    ipcTextField(FASTDB_PORTABLE_CONTRACT.version),
    ipcTextField(abiHash),
    ipcTextField(FASTDB_PORTABLE_CONTRACT.signatureHash),
    ipcU64LE(options.maxPayloadSize ?? 1024),
    ipcU16LE(1),
    ipcTextField("query"),
    ipcU16LE(7),
  ]);
  return ipcFrame(0, C2_IPC_SMOKE_FLAG_HANDSHAKE, payload);
}

function ipcSuccessReplyFrame(requestId: number, payload: Uint8Array, extraFlags = 0): Uint8Array {
  return ipcFrame(requestId, C2_IPC_SMOKE_FLAG_RESPONSE | C2_IPC_SMOKE_FLAG_REPLY_V2 | extraFlags, ipcConcat([new Uint8Array([0]), payload]));
}

function ipcBuddyPayload(segIdx: number, offset: number, dataSize: number, dedicated = false): Uint8Array {
  return ipcConcat([
    ipcU16LE(segIdx),
    ipcU32LE(offset),
    ipcU32LE(dataSize),
    new Uint8Array([dedicated ? C2_IPC_SMOKE_BUDDY_FLAG_DEDICATED : 0]),
  ]);
}

function ipcBuddySuccessReplyFrame(requestId: number, segIdx: number, offset: number, dataSize: number, dedicated = false): Uint8Array {
  return ipcFrame(
    requestId,
    C2_IPC_SMOKE_FLAG_RESPONSE | C2_IPC_SMOKE_FLAG_REPLY_V2 | C2_IPC_SMOKE_FLAG_BUDDY,
    ipcConcat([ipcBuddyPayload(segIdx, offset, dataSize, dedicated), new Uint8Array([0])]),
  );
}

function ipcReplyChunkFrame(requestId: number, totalSize: number, totalChunks: number, chunkIndex: number, payload: Uint8Array, isLast: boolean): Uint8Array {
  const meta = ipcConcat([ipcU64LE(totalSize), ipcU32LE(totalChunks), ipcU32LE(chunkIndex)]);
  const flags = C2_IPC_SMOKE_FLAG_RESPONSE | C2_IPC_SMOKE_FLAG_REPLY_V2 | C2_IPC_SMOKE_FLAG_CHUNKED | (isLast ? C2_IPC_SMOKE_FLAG_CHUNK_LAST : 0);
  return ipcFrame(requestId, flags, ipcConcat([meta, payload]));
}

function ipcErrorReplyFrame(requestId: number, payload: Uint8Array): Uint8Array {
  return ipcFrame(requestId, C2_IPC_SMOKE_FLAG_RESPONSE | C2_IPC_SMOKE_FLAG_REPLY_V2, ipcConcat([new Uint8Array([1]), ipcU32LE(payload.byteLength), payload]));
}

function ipcRouteNotFoundReplyFrame(requestId: number, routeName: string): Uint8Array {
  const routeBytes = ipcTextEncoder.encode(routeName);
  return ipcFrame(requestId, C2_IPC_SMOKE_FLAG_RESPONSE | C2_IPC_SMOKE_FLAG_REPLY_V2, ipcConcat([new Uint8Array([2]), ipcU32LE(routeBytes.byteLength), routeBytes]));
}

function ipcFrameFlags(frame: Uint8Array): number {
  return new DataView(frame.buffer, frame.byteOffset, frame.byteLength).getUint32(12, true);
}

function ipcFrameRequestId(frame: Uint8Array): number {
  return Number(new DataView(frame.buffer, frame.byteOffset, frame.byteLength).getBigUint64(4, true));
}

function ipcFramePayload(frame: Uint8Array): Uint8Array {
  return frame.slice(16);
}

function ipcRequestChunkHeader(payload: Uint8Array): { readonly chunkIndex: number; readonly totalChunks: number; readonly dataOffset: number } {
  const view = new DataView(payload.buffer, payload.byteOffset, payload.byteLength);
  return {
    chunkIndex: view.getUint16(0, true),
    totalChunks: view.getUint16(2, true),
    dataOffset: 4,
  };
}

function ipcBuddyHeader(payload: Uint8Array): { readonly segmentIndex: number; readonly offset: number; readonly byteLength: number; readonly dedicated: boolean; readonly dataOffset: number } {
  const view = new DataView(payload.buffer, payload.byteOffset, payload.byteLength);
  return {
    segmentIndex: view.getUint16(0, true),
    offset: view.getUint32(2, true),
    byteLength: view.getUint32(6, true),
    dedicated: payload[10] === C2_IPC_SMOKE_BUDDY_FLAG_DEDICATED,
    dataOffset: 11,
  };
}

function ipcHandshakeSummary(payload: Uint8Array): { readonly prefix: string; readonly segments: readonly { readonly name: string; readonly size: number }[]; readonly capabilities: number } {
  let offset = 0;
  const version = payload[offset++];
  if (version !== 10) {
    throw new Error(`unexpected IPC client handshake version ${version}`);
  }
  const prefixLength = payload[offset++];
  const prefix = ipcTextDecoder.decode(payload.slice(offset, offset + prefixLength));
  offset += prefixLength;
  const view = new DataView(payload.buffer, payload.byteOffset, payload.byteLength);
  const segmentCount = view.getUint16(offset, true);
  offset += 2;
  const segments: { name: string; size: number }[] = [];
  for (let index = 0; index < segmentCount; index += 1) {
    const size = view.getUint32(offset, true);
    offset += 4;
    const nameLength = payload[offset++];
    const name = ipcTextDecoder.decode(payload.slice(offset, offset + nameLength));
    offset += nameLength;
    segments.push({ name, size });
  }
  const capabilities = view.getUint16(offset, true);
  return { prefix, segments, capabilities };
}

type FakeShmFile = { data: Uint8Array };

class FakeNodePosixShmHandle implements C2NodePosixShmFileHandle {
  closed = false;

  constructor(private readonly file: FakeShmFile) {
  }

  read(buffer: Uint8Array, offset: number, length: number, position: number): { bytesRead: number } {
    if (this.closed) {
      throw new Error("fake SHM handle read after close");
    }
    const available = Math.max(0, Math.min(length, this.file.data.byteLength - position));
    buffer.set(this.file.data.subarray(position, position + available), offset);
    return { bytesRead: available };
  }

  write(buffer: Uint8Array, offset: number, length: number, position: number): { bytesWritten: number } {
    if (this.closed) {
      throw new Error("fake SHM handle write after close");
    }
    const end = position + length;
    if (end > this.file.data.byteLength) {
      const grown = new Uint8Array(end);
      grown.set(this.file.data);
      this.file.data = grown;
    }
    this.file.data.set(buffer.subarray(offset, offset + length), position);
    return { bytesWritten: length };
  }

  truncate(length: number): void {
    if (this.closed) {
      throw new Error("fake SHM handle truncate after close");
    }
    const resized = new Uint8Array(length);
    resized.set(this.file.data.subarray(0, Math.min(length, this.file.data.byteLength)));
    this.file.data = resized;
  }

  close(): void {
    this.closed = true;
  }
}

class FakeNodePosixShmFileSystem implements C2NodePosixShmFileSystem {
  readonly files = new Map<string, FakeShmFile>();
  readonly unlinks: string[] = [];

  open(path: string, flags: string | number, _mode?: number): C2NodePosixShmFileHandle {
    const flagText = String(flags);
    let file = this.files.get(path);
    if (flagText.includes("w")) {
      file = { data: new Uint8Array() };
      this.files.set(path, file);
    }
    if (file === undefined) {
      throw new Error(`fake SHM file not found: ${path}`);
    }
    return new FakeNodePosixShmHandle(file);
  }

  unlink(path: string): void {
    if (!this.files.has(path)) {
      throw new Error(`fake SHM file not found: ${path}`);
    }
    this.unlinks.push(path);
    this.files.delete(path);
  }
}

class FakeNodePosixNativeBuddyBackend implements C2NodePosixNativeBuddyResponseShmBackend, C2NodePosixNativeBuddyRequestShmBackend {
  readonly prefix = "/cc2n0002";
  readonly segments = [{ name: "/cc2n0002_b0000", size: 4096 }];
  readonly responseReads: C2IpcResponseShmBlock[] = [];
  readonly responseReleases: C2IpcResponseShmBlock[] = [];
  readonly requestReleases: C2IpcRequestShmBlock[] = [];
  readonly requestConsumes: C2IpcRequestShmBlock[] = [];
  writeOffset = 128;

  readResponse(block: C2IpcResponseShmBlock, destination: Uint8Array): void {
    this.responseReads.push(block);
    destination.set([91, 92, 93]);
  }

  releaseResponse(block: C2IpcResponseShmBlock): void {
    this.responseReleases.push(block);
  }

  writeRequest(payload: Uint8Array): C2IpcRequestShmBlock {
    return {
      segmentIndex: 0,
      offset: this.writeOffset,
      byteLength: payload.byteLength,
      dedicated: false,
    };
  }

  releaseRequest(block: C2IpcRequestShmBlock): void {
    this.requestReleases.push(block);
  }

  markRequestConsumed(block: C2IpcRequestShmBlock): void {
    this.requestConsumes.push(block);
  }
}

class FakeC2MemFfiResponsePool implements C2MemFfiResponsePoolBinding {
  readonly reads: C2IpcResponseShmBlock[] = [];
  readonly releases: C2IpcResponseShmBlock[] = [];
  closeCount = 0;

  read(block: C2IpcResponseShmBlock, destination: Uint8Array): void {
    this.reads.push(block);
    destination.set([71, 72, 73]);
  }

  release(block: C2IpcResponseShmBlock): void {
    this.releases.push(block);
  }

  close(): void {
    this.closeCount += 1;
  }
}

class FakeC2MemFfiResponseFactory implements C2MemFfiResponsePoolFactory {
  readonly pools = new Map<string, FakeC2MemFfiResponsePool>();
  readonly options: Array<{ prefix: string; segmentSize: number; maxSegments: number; minBlockSize: number }> = [];

  createResponsePool(options: { readonly prefix: string; readonly segmentSize: number; readonly maxSegments: number; readonly minBlockSize: number }): C2MemFfiResponsePoolBinding {
    this.options.push({ prefix: options.prefix, segmentSize: options.segmentSize, maxSegments: options.maxSegments, minBlockSize: options.minBlockSize });
    const pool = new FakeC2MemFfiResponsePool();
    this.pools.set(options.prefix, pool);
    return pool;
  }
}

class FakeC2MemFfiRequestPool implements C2MemFfiRequestPoolBinding {
  readonly prefix = "/cc2nffi1";
  readonly segments = [{ name: "/cc2nffi1_b0000", size: 4096 }];
  readonly writes: SmokeByteArray[] = [];
  readonly releases: C2IpcRequestShmBlock[] = [];
  readonly consumed: C2IpcRequestShmBlock[] = [];
  closeCount = 0;
  writeOffset = 256;

  write(payload: Uint8Array): C2IpcRequestShmBlock {
    this.writes.push(payload.slice());
    return {
      segmentIndex: 0,
      offset: this.writeOffset,
      byteLength: payload.byteLength,
      dedicated: false,
    };
  }

  release(block: C2IpcRequestShmBlock): void {
    this.releases.push(block);
  }

  forgetConsumed(block: C2IpcRequestShmBlock): void {
    this.consumed.push(block);
  }

  close(): void {
    this.closeCount += 1;
  }
}

class FakeIpcConnection implements C2IpcConnection {
  readonly writes: SmokeByteArray[] = [];
  readonly readSizes: number[] = [];
  closed = false;
  private pending: Uint8Array;

  constructor(reads: readonly SmokeByteArray[]) {
    this.pending = ipcConcat(reads);
  }

  write(data: SmokeByteArray): void {
    this.writes.push(data.slice());
  }

  readExactly(byteLength: number): Uint8Array {
    this.readSizes.push(byteLength);
    if (byteLength > this.pending.byteLength) {
      throw new Error(`fake IPC read underflow for ${byteLength} bytes`);
    }
    const out = this.pending.slice(0, byteLength);
    this.pending = this.pending.slice(byteLength);
    return out;
  }

  close(): void {
    this.closed = true;
  }
}

class FailAfterWriteIpcConnection extends FakeIpcConnection {
  constructor(
    reads: readonly SmokeByteArray[],
    private readonly failAtWriteCount: number,
  ) {
    super(reads);
  }

  write(data: SmokeByteArray): void {
    super.write(data);
    if (this.writes.length === this.failAtWriteCount) {
      throw new Error(`fake IPC write failure at ${this.failAtWriteCount}`);
    }
  }
}

let ipcConnection: FakeIpcConnection | undefined;
const ipcWire = createIpcEncodedTransport("ipc://unit-ts", {
  connect(socketPath: string): C2IpcConnection {
    if (socketPath !== "/tmp/c_two_ipc/unit-ts.sock") {
      throw new Error(`unexpected IPC socket path ${socketPath}`);
    }
    ipcConnection = new FakeIpcConnection([
      ipcServerHandshakeFrame(),
      ipcSuccessReplyFrame(1, new Uint8Array([9, 8, 7])),
    ]);
    return ipcConnection;
  },
});
const ipcResult = await ipcWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1, 2, 3]));
if (ipcResult[0] !== 9 || ipcResult[1] !== 8 || ipcResult[2] !== 7) {
  throw new Error("IPC transport did not return inline reply bytes");
}
if (!ipcConnection || ipcConnection.writes.length !== 2) {
  throw new Error(`expected IPC handshake and call writes, got ${ipcConnection?.writes.length ?? 0}`);
}
if (ipcFrameFlags(ipcConnection.writes[0]) !== C2_IPC_SMOKE_FLAG_HANDSHAKE) {
  throw new Error("IPC transport did not send a handshake frame first");
}
const ipcCallFrame = ipcConnection.writes[1];
if (ipcFrameRequestId(ipcCallFrame) !== 1 || ipcFrameFlags(ipcCallFrame) !== C2_IPC_SMOKE_FLAG_CALL_V2) {
  throw new Error("IPC transport sent an invalid v2 call frame");
}
const ipcCallPayload = ipcFramePayload(ipcCallFrame);
const ipcCallRouteLength = ipcCallPayload[0];
const ipcCallRoute = ipcTextDecoder.decode(ipcCallPayload.slice(1, 1 + ipcCallRouteLength));
const ipcCallMethodIndexOffset = 1 + ipcCallRouteLength;
const ipcCallMethodIndex = new DataView(ipcCallPayload.buffer, ipcCallPayload.byteOffset, ipcCallPayload.byteLength).getUint16(ipcCallMethodIndexOffset, true);
const ipcCallBody = ipcCallPayload.slice(ipcCallMethodIndexOffset + 2);
if (ipcCallRoute !== "route" || ipcCallMethodIndex !== 7 || ipcCallBody[0] !== 1 || ipcCallBody[1] !== 2 || ipcCallBody[2] !== 3) {
  throw new Error("IPC transport did not encode route, method index, and payload correctly");
}
const ipcHandshakePayload = ipcFramePayload(ipcConnection.writes[0]);
const ipcClientCapabilities = new DataView(ipcHandshakePayload.buffer, ipcHandshakePayload.byteOffset, ipcHandshakePayload.byteLength).getUint16(4, true);
if ((ipcClientCapabilities & C2_IPC_SMOKE_CAP_CHUNKED) === 0) {
  throw new Error("IPC transport did not advertise chunked response capability");
}
await ipcWire.close();
if (!ipcConnection.closed) {
  throw new Error("IPC transport close did not close the underlying connection");
}

type C2NodeSmokeServerSocket = {
  on(event: "data", listener: (chunk: SmokeByteArray) => void): unknown;
  write(data: SmokeByteArray): boolean | void;
  end?(): void;
};
type C2NodeSmokeServer = {
  on(event: "connection", listener: (socket: C2NodeSmokeServerSocket) => void): unknown;
  once(event: "error", listener: (error: Error) => void): unknown;
  listen(path: string, callback: () => void): void;
  close(callback: () => void): void;
};
type C2NodeSmokeNet = C2NodeIpcNet & {
  createServer(): C2NodeSmokeServer;
};
type C2NodeSmokeFs = {
  mkdirSync(path: string, options: { readonly recursive: boolean }): void;
  rmSync(path: string, options: { readonly force: boolean }): void;
};
const importNodeBuiltin = new Function("specifier", "return import(specifier)") as (specifier: string) => Promise<unknown>;
const nodeNet = await importNodeBuiltin("node:net") as C2NodeSmokeNet;
const nodeFs = await importNodeBuiltin("node:fs") as C2NodeSmokeFs;
const nodeSocketRegion = `node-real-socket-${Date.now()}-${Math.floor(Math.random() * 1_000_000)}`;
const nodeSocketPath = `/tmp/c_two_ipc/${nodeSocketRegion}.sock`;
nodeFs.mkdirSync("/tmp/c_two_ipc", { recursive: true });
nodeFs.rmSync(nodeSocketPath, { force: true });
const nodeSocketServer = nodeNet.createServer();
let nodeSocketPending: SmokeByteArray = new Uint8Array();
let nodeSocketHandshakeSeen = false;
let nodeSocketCallSeen = false;
let nodeSocketServerFailure: Error | undefined;
nodeSocketServer.on("connection", (socket: C2NodeSmokeServerSocket) => {
  socket.on("data", (chunk: SmokeByteArray) => {
    if (nodeSocketServerFailure !== undefined) {
      return;
    }
    nodeSocketPending = ipcConcat([nodeSocketPending, chunk]);
    while (nodeSocketPending.byteLength >= 16) {
      const totalLen = new DataView(nodeSocketPending.buffer, nodeSocketPending.byteOffset, nodeSocketPending.byteLength).getUint32(0, true);
      const frameByteLength = 4 + totalLen;
      if (nodeSocketPending.byteLength < frameByteLength) {
        return;
      }
      const frame = nodeSocketPending.slice(0, frameByteLength);
      nodeSocketPending = nodeSocketPending.slice(frameByteLength);
      const requestId = ipcFrameRequestId(frame);
      const flags = ipcFrameFlags(frame);
      const payload = ipcFramePayload(frame);
      try {
        if (requestId === 0 && flags === C2_IPC_SMOKE_FLAG_HANDSHAKE) {
          nodeSocketHandshakeSeen = true;
          socket.write(ipcServerHandshakeFrame());
          continue;
        }
        if (requestId === 1 && flags === C2_IPC_SMOKE_FLAG_CALL_V2) {
          const routeLength = payload[0];
          const route = ipcTextDecoder.decode(payload.slice(1, 1 + routeLength));
          const methodIndexOffset = 1 + routeLength;
          const methodIndex = new DataView(payload.buffer, payload.byteOffset, payload.byteLength).getUint16(methodIndexOffset, true);
          const body = payload.slice(methodIndexOffset + 2);
          if (route !== "route" || methodIndex !== 7 || Array.from(body).join(",") !== "201,202,203") {
            throw new Error(`unexpected real Node IPC call frame route=${route} method=${methodIndex} body=${Array.from(body).join(",")}`);
          }
          nodeSocketCallSeen = true;
          socket.write(ipcSuccessReplyFrame(requestId, new Uint8Array([44, 45, 46])));
          continue;
        }
        throw new Error(`unexpected real Node IPC frame request=${requestId} flags=${flags}`);
      } catch (error) {
        nodeSocketServerFailure = error instanceof Error ? error : new Error(String(error));
        socket.end?.();
        return;
      }
    }
  });
});
await new Promise<void>((resolve, reject) => {
  nodeSocketServer.once("error", (error: Error) => reject(error));
  nodeSocketServer.listen(nodeSocketPath, resolve);
});
const nodeIpcWire = createIpcEncodedTransport(`ipc://${nodeSocketRegion}`, {
  connect: createNodeIpcConnect({ net: nodeNet }),
});
try {
  const nodeIpcResult = await nodeIpcWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([201, 202, 203]));
  if (Array.from(nodeIpcResult).join(",") !== "44,45,46") {
    throw new Error(`real Node IPC transport returned wrong bytes: ${Array.from(nodeIpcResult).join(",")}`);
  }
  if (!nodeSocketHandshakeSeen || !nodeSocketCallSeen || nodeSocketServerFailure !== undefined) {
    throw new Error(`real Node IPC smoke failed: handshake=${nodeSocketHandshakeSeen} call=${nodeSocketCallSeen} failure=${String(nodeSocketServerFailure)}`);
  }
} finally {
  await nodeIpcWire.close();
  await new Promise<void>((resolve) => nodeSocketServer.close(resolve));
  nodeFs.rmSync(nodeSocketPath, { force: true });
}

let ipcChunkedRequestConnection: FakeIpcConnection | undefined;
const ipcChunkedRequestWire = createIpcEncodedTransport("ipc://request-chunked", {
  connect(): C2IpcConnection {
    ipcChunkedRequestConnection = new FakeIpcConnection([
      ipcServerHandshakeFrame(),
      ipcSuccessReplyFrame(1, new Uint8Array([1])),
    ]);
    return ipcChunkedRequestConnection;
  },
  requestChunkSize: 2,
});
await ipcChunkedRequestWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1, 2, 3, 4, 5]));
if (!ipcChunkedRequestConnection || ipcChunkedRequestConnection.writes.length !== 4) {
  throw new Error(`expected IPC handshake plus 3 chunked request writes, got ${ipcChunkedRequestConnection?.writes.length ?? 0}`);
}
const ipcChunkedRequestFrame0 = ipcChunkedRequestConnection.writes[1];
const ipcChunkedRequestFrame1 = ipcChunkedRequestConnection.writes[2];
const ipcChunkedRequestFrame2 = ipcChunkedRequestConnection.writes[3];
if (ipcFrameRequestId(ipcChunkedRequestFrame0) !== 1 || ipcFrameRequestId(ipcChunkedRequestFrame1) !== 1 || ipcFrameRequestId(ipcChunkedRequestFrame2) !== 1) {
  throw new Error("IPC chunked request frames did not reuse one request id");
}
if (ipcFrameFlags(ipcChunkedRequestFrame0) !== (C2_IPC_SMOKE_FLAG_CALL_V2 | C2_IPC_SMOKE_FLAG_CHUNKED)) {
  throw new Error("IPC chunked request first frame flags were invalid");
}
if (ipcFrameFlags(ipcChunkedRequestFrame1) !== (C2_IPC_SMOKE_FLAG_CALL_V2 | C2_IPC_SMOKE_FLAG_CHUNKED)) {
  throw new Error("IPC chunked request middle frame flags were invalid");
}
if (ipcFrameFlags(ipcChunkedRequestFrame2) !== (C2_IPC_SMOKE_FLAG_CALL_V2 | C2_IPC_SMOKE_FLAG_CHUNKED | C2_IPC_SMOKE_FLAG_CHUNK_LAST)) {
  throw new Error("IPC chunked request final frame flags were invalid");
}
const ipcChunkedRequestPayload0 = ipcFramePayload(ipcChunkedRequestFrame0);
const ipcChunkedRequestPayload1 = ipcFramePayload(ipcChunkedRequestFrame1);
const ipcChunkedRequestPayload2 = ipcFramePayload(ipcChunkedRequestFrame2);
const ipcChunkedRequestHeader0 = ipcRequestChunkHeader(ipcChunkedRequestPayload0);
const ipcChunkedRequestHeader1 = ipcRequestChunkHeader(ipcChunkedRequestPayload1);
const ipcChunkedRequestHeader2 = ipcRequestChunkHeader(ipcChunkedRequestPayload2);
if (ipcChunkedRequestHeader0.chunkIndex !== 0 || ipcChunkedRequestHeader0.totalChunks !== 3 || ipcChunkedRequestHeader1.chunkIndex !== 1 || ipcChunkedRequestHeader1.totalChunks !== 3 || ipcChunkedRequestHeader2.chunkIndex !== 2 || ipcChunkedRequestHeader2.totalChunks !== 3) {
  throw new Error("IPC chunked request headers did not encode chunk indexes and total count");
}
const ipcChunkedRequestRouteLength = ipcChunkedRequestPayload0[ipcChunkedRequestHeader0.dataOffset];
const ipcChunkedRequestRoute = ipcTextDecoder.decode(ipcChunkedRequestPayload0.slice(ipcChunkedRequestHeader0.dataOffset + 1, ipcChunkedRequestHeader0.dataOffset + 1 + ipcChunkedRequestRouteLength));
const ipcChunkedRequestMethodIndexOffset = ipcChunkedRequestHeader0.dataOffset + 1 + ipcChunkedRequestRouteLength;
const ipcChunkedRequestMethodIndex = new DataView(ipcChunkedRequestPayload0.buffer, ipcChunkedRequestPayload0.byteOffset, ipcChunkedRequestPayload0.byteLength).getUint16(ipcChunkedRequestMethodIndexOffset, true);
const ipcChunkedRequestData0 = ipcChunkedRequestPayload0.slice(ipcChunkedRequestMethodIndexOffset + 2);
const ipcChunkedRequestData1 = ipcChunkedRequestPayload1.slice(ipcChunkedRequestHeader1.dataOffset);
const ipcChunkedRequestData2 = ipcChunkedRequestPayload2.slice(ipcChunkedRequestHeader2.dataOffset);
if (ipcChunkedRequestRoute !== "route" || ipcChunkedRequestMethodIndex !== 7 || Array.from(ipcChunkedRequestData0).join(",") !== "1,2" || Array.from(ipcChunkedRequestData1).join(",") !== "3,4" || Array.from(ipcChunkedRequestData2).join(",") !== "5") {
  throw new Error("IPC chunked request did not encode call control and split payload data correctly");
}

const ipcChunkedRequestNoCapabilityWire = createIpcEncodedTransport("ipc://request-chunked-no-capability", {
  connect(): C2IpcConnection {
    return new FakeIpcConnection([
      ipcServerHandshakeFrame({
        capabilities: C2_IPC_SMOKE_CAP_CALL_V2 | C2_IPC_SMOKE_CAP_METHOD_IDX,
      }),
    ]);
  },
  requestChunkSize: 2,
});
let ipcChunkedRequestNoCapabilityThrown = false;
try {
  await ipcChunkedRequestNoCapabilityWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1, 2, 3]));
} catch (error) {
  ipcChunkedRequestNoCapabilityThrown = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("does not support chunked request")) {
    throw new Error(`unexpected IPC chunked request capability error ${String(error)}`);
  }
}
if (!ipcChunkedRequestNoCapabilityThrown) {
  throw new Error("IPC transport sent chunked request frames to a server without chunked capability");
}

let ipcBadRequestChunkSizeThrown = false;
try {
  createIpcEncodedTransport("ipc://bad-request-chunk-size", {
    connect(): C2IpcConnection {
      throw new Error("bad request chunk size should fail before connect");
    },
    requestChunkSize: 0,
  });
} catch (error) {
  ipcBadRequestChunkSizeThrown = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("requestChunkSize")) {
    throw new Error(`unexpected IPC bad request chunk size error ${String(error)}`);
  }
}
if (!ipcBadRequestChunkSizeThrown) {
  throw new Error("IPC transport accepted an invalid requestChunkSize");
}

let ipcChunkedRequestWriteFailureConnection: FailAfterWriteIpcConnection | undefined;
let ipcChunkedRequestWriteFailureConnectCount = 0;
const ipcChunkedRequestWriteFailureWire = createIpcEncodedTransport("ipc://request-chunked-write-failure", {
  connect(): C2IpcConnection {
    ipcChunkedRequestWriteFailureConnectCount += 1;
    if (ipcChunkedRequestWriteFailureConnectCount > 1) {
      return new FakeIpcConnection([
        ipcServerHandshakeFrame(),
        ipcSuccessReplyFrame(2, new Uint8Array([6])),
      ]);
    }
    ipcChunkedRequestWriteFailureConnection = new FailAfterWriteIpcConnection([
      ipcServerHandshakeFrame(),
    ], 3);
    return ipcChunkedRequestWriteFailureConnection;
  },
  requestChunkSize: 2,
});
let ipcChunkedRequestWriteFailureThrown = false;
try {
  await ipcChunkedRequestWriteFailureWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1, 2, 3, 4, 5]));
} catch (error) {
  ipcChunkedRequestWriteFailureThrown = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("fake IPC write failure")) {
    throw new Error(`unexpected IPC chunked request write failure error ${String(error)}`);
  }
}
if (!ipcChunkedRequestWriteFailureThrown || !ipcChunkedRequestWriteFailureConnection?.closed) {
  throw new Error(`IPC transport did not close after a partial chunked request write failure: thrown=${ipcChunkedRequestWriteFailureThrown} closed=${ipcChunkedRequestWriteFailureConnection?.closed ?? false}`);
}
const ipcChunkedRequestRetryResult = await ipcChunkedRequestWriteFailureWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([6]));
if (ipcChunkedRequestWriteFailureConnectCount !== 2 || ipcChunkedRequestRetryResult[0] !== 6) {
  throw new Error(`IPC transport did not reconnect after a partial chunked request write failure: connects=${ipcChunkedRequestWriteFailureConnectCount} result=${Array.from(ipcChunkedRequestRetryResult).join(",")}`);
}

let ipcRequestShmConnection: FakeIpcConnection | undefined;
let ipcRequestShmWrittenPayload: Uint8Array | undefined;
let ipcRequestShmReleaseCount = 0;
const ipcRequestShmWriter: C2IpcRequestShmWriter = {
  prefix: "/cc2c0001",
  segments: [{ name: "/cc2c0001_b0000", size: 4096 }],
  write(payload: Uint8Array): C2IpcRequestShmBlock {
    ipcRequestShmWrittenPayload = payload.slice();
    return { segmentIndex: 0, offset: 128, byteLength: payload.byteLength, dedicated: false };
  },
  release(_block: C2IpcRequestShmBlock): void {
    ipcRequestShmReleaseCount += 1;
  },
};
const ipcRequestShmWire = createIpcEncodedTransport("ipc://request-shm", {
  connect(): C2IpcConnection {
    ipcRequestShmConnection = new FakeIpcConnection([
      ipcServerHandshakeFrame(),
      ipcSuccessReplyFrame(1, new Uint8Array([8])),
    ]);
    return ipcRequestShmConnection;
  },
  requestShmWriter: ipcRequestShmWriter,
  requestShmThreshold: 2,
});
const ipcRequestShmResult = await ipcRequestShmWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([31, 32, 33, 34]));
if (ipcRequestShmResult[0] !== 8) {
  throw new Error("IPC request SHM call did not return the server response");
}
if (!ipcRequestShmConnection || ipcRequestShmConnection.writes.length !== 2) {
  throw new Error(`expected IPC request SHM handshake plus call writes, got ${ipcRequestShmConnection?.writes.length ?? 0}`);
}
const ipcRequestShmHandshake = ipcHandshakeSummary(ipcFramePayload(ipcRequestShmConnection.writes[0]));
if (ipcRequestShmHandshake.prefix !== "/cc2c0001" || ipcRequestShmHandshake.segments.length !== 1 || ipcRequestShmHandshake.segments[0]?.name !== "/cc2c0001_b0000" || ipcRequestShmHandshake.segments[0]?.size !== 4096) {
  throw new Error(`IPC request SHM writer was not advertised in the client handshake: ${JSON.stringify(ipcRequestShmHandshake)}`);
}
if (!ipcRequestShmWrittenPayload || Array.from(ipcRequestShmWrittenPayload).join(",") !== "31,32,33,34") {
  throw new Error(`IPC request SHM writer did not receive the request payload: ${Array.from(ipcRequestShmWrittenPayload ?? new Uint8Array()).join(",")}`);
}
const ipcRequestShmFrame = ipcRequestShmConnection.writes[1];
if (ipcFrameFlags(ipcRequestShmFrame) !== (C2_IPC_SMOKE_FLAG_CALL_V2 | C2_IPC_SMOKE_FLAG_BUDDY)) {
  throw new Error("IPC request SHM frame did not use CALL_V2|BUDDY flags");
}
const ipcRequestShmPayload = ipcFramePayload(ipcRequestShmFrame);
const ipcRequestShmBuddy = ipcBuddyHeader(ipcRequestShmPayload);
if (ipcRequestShmBuddy.segmentIndex !== 0 || ipcRequestShmBuddy.offset !== 128 || ipcRequestShmBuddy.byteLength !== 4 || ipcRequestShmBuddy.dedicated) {
  throw new Error(`IPC request SHM frame encoded the wrong buddy coordinates: ${JSON.stringify(ipcRequestShmBuddy)}`);
}
const ipcRequestShmRouteLength = ipcRequestShmPayload[ipcRequestShmBuddy.dataOffset];
const ipcRequestShmRoute = ipcTextDecoder.decode(ipcRequestShmPayload.slice(ipcRequestShmBuddy.dataOffset + 1, ipcRequestShmBuddy.dataOffset + 1 + ipcRequestShmRouteLength));
const ipcRequestShmMethodIndexOffset = ipcRequestShmBuddy.dataOffset + 1 + ipcRequestShmRouteLength;
const ipcRequestShmMethodIndex = new DataView(ipcRequestShmPayload.buffer, ipcRequestShmPayload.byteOffset, ipcRequestShmPayload.byteLength).getUint16(ipcRequestShmMethodIndexOffset, true);
if (ipcRequestShmRoute !== "route" || ipcRequestShmMethodIndex !== 7 || ipcRequestShmPayload.byteLength !== ipcRequestShmMethodIndexOffset + 2) {
  throw new Error("IPC request SHM frame did not encode exactly buddy metadata plus call control");
}
if (ipcRequestShmReleaseCount !== 0) {
  throw new Error(`IPC request SHM released a non-dedicated block after successful server consumption: ${ipcRequestShmReleaseCount}`);
}

let ipcRequestShmZeroThresholdConnection: FakeIpcConnection | undefined;
const ipcRequestShmZeroThresholdWire = createIpcEncodedTransport("ipc://request-shm-zero-threshold", {
  connect(): C2IpcConnection {
    ipcRequestShmZeroThresholdConnection = new FakeIpcConnection([
      ipcServerHandshakeFrame(),
      ipcSuccessReplyFrame(1, new Uint8Array([11])),
    ]);
    return ipcRequestShmZeroThresholdConnection;
  },
  requestShmWriter: {
    prefix: "/cc2c0009",
    segments: [{ name: "/cc2c0009_b0000", size: 4096 }],
    write(payload: Uint8Array): C2IpcRequestShmBlock {
      return { segmentIndex: 0, offset: 1024, byteLength: payload.byteLength, dedicated: false };
    },
    release(_block: C2IpcRequestShmBlock): void {
      throw new Error("zero-threshold request SHM writer should not release non-dedicated success");
    },
  },
  requestShmThreshold: 0,
});
const ipcRequestShmZeroThresholdResult = await ipcRequestShmZeroThresholdWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([91]));
if (ipcRequestShmZeroThresholdResult[0] !== 11 || !ipcRequestShmZeroThresholdConnection) {
  throw new Error("IPC request SHM zero-threshold call did not complete");
}
if (ipcFrameFlags(ipcRequestShmZeroThresholdConnection.writes[1]) !== (C2_IPC_SMOKE_FLAG_CALL_V2 | C2_IPC_SMOKE_FLAG_BUDDY)) {
  throw new Error("IPC request SHM zero threshold did not route a non-empty request through buddy SHM");
}

let ipcRequestShmSnapshotConnection: FakeIpcConnection | undefined;
const ipcRequestShmSnapshotSegments = [{ name: "/cc2c0007_b0000", size: 4096 }];
const ipcRequestShmSnapshotWriter: C2IpcRequestShmWriter = {
  prefix: "/cc2c0007",
  segments: ipcRequestShmSnapshotSegments,
  write(payload: Uint8Array): C2IpcRequestShmBlock {
    return { segmentIndex: 0, offset: 512, byteLength: payload.byteLength, dedicated: false };
  },
  release(_block: C2IpcRequestShmBlock): void {
    throw new Error("snapshot request SHM writer should not release non-dedicated success");
  },
};
const ipcRequestShmSnapshotWire = createIpcEncodedTransport("ipc://request-shm-snapshot", {
  connect(): C2IpcConnection {
    ipcRequestShmSnapshotConnection = new FakeIpcConnection([
      ipcServerHandshakeFrame(),
      ipcSuccessReplyFrame(1, new Uint8Array([10])),
    ]);
    return ipcRequestShmSnapshotConnection;
  },
  requestShmWriter: ipcRequestShmSnapshotWriter,
  requestShmThreshold: 2,
});
ipcRequestShmSnapshotSegments[0] = { name: "/mutated_b0000", size: 1 };
const ipcRequestShmSnapshotResult = await ipcRequestShmSnapshotWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1, 2, 3, 4]));
if (ipcRequestShmSnapshotResult[0] !== 10 || !ipcRequestShmSnapshotConnection) {
  throw new Error("IPC request SHM snapshot call did not complete");
}
const ipcRequestShmSnapshotHandshake = ipcHandshakeSummary(ipcFramePayload(ipcRequestShmSnapshotConnection.writes[0]));
if (ipcRequestShmSnapshotHandshake.prefix !== "/cc2c0007" || ipcRequestShmSnapshotHandshake.segments[0]?.name !== "/cc2c0007_b0000" || ipcRequestShmSnapshotHandshake.segments[0]?.size !== 4096) {
  throw new Error(`IPC request SHM writer metadata was not snapshotted at transport creation: ${JSON.stringify(ipcRequestShmSnapshotHandshake)}`);
}

let ipcRequestShmCrmErrorReleaseCount = 0;
const ipcRequestShmCrmErrorWire = createIpcEncodedTransport("ipc://request-shm-crm-error", {
  connect(): C2IpcConnection {
    return new FakeIpcConnection([
      ipcServerHandshakeFrame(),
      ipcErrorReplyFrame(1, new Uint8Array([5, 6])),
    ]);
  },
  requestShmWriter: {
    prefix: "/cc2c0006",
    segments: [{ name: "/cc2c0006_b0000", size: 4096 }],
    write(payload: Uint8Array): C2IpcRequestShmBlock {
      return { segmentIndex: 0, offset: 64, byteLength: payload.byteLength, dedicated: false };
    },
    release(_block: C2IpcRequestShmBlock): void {
      ipcRequestShmCrmErrorReleaseCount += 1;
    },
  },
  requestShmThreshold: 2,
});
let ipcRequestShmCrmErrorThrown = false;
try {
  await ipcRequestShmCrmErrorWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([35, 36, 37]));
} catch (error) {
  ipcRequestShmCrmErrorThrown = true;
  if (!(error instanceof C2CrmMethodError) || error.payload[0] !== 5 || error.payload[1] !== 6) {
    throw new Error(`unexpected IPC request SHM CRM error ${String(error)}`);
  }
}
if (!ipcRequestShmCrmErrorThrown || ipcRequestShmCrmErrorReleaseCount !== 0) {
  throw new Error(`IPC request SHM released a non-dedicated block after CRM error consumption: thrown=${ipcRequestShmCrmErrorThrown} release=${ipcRequestShmCrmErrorReleaseCount}`);
}

let ipcRequestShmCrmErrorMarkConsumedCount = 0;
let ipcRequestShmCrmErrorMarkConsumedReleaseCount = 0;
const ipcRequestShmCrmErrorMarkConsumedFailureWire = createIpcEncodedTransport("ipc://request-shm-crm-error-mark-consumed", {
  connect(): C2IpcConnection {
    return new FakeIpcConnection([
      ipcServerHandshakeFrame(),
      ipcErrorReplyFrame(1, new Uint8Array([7, 8])),
    ]);
  },
  requestShmWriter: {
    prefix: "/cc2c0013",
    segments: [{ name: "/cc2c0013_b0000", size: 4096 }],
    write(payload: Uint8Array): C2IpcRequestShmBlock {
      return { segmentIndex: 0, offset: 96, byteLength: payload.byteLength, dedicated: false };
    },
    release(_block: C2IpcRequestShmBlock): void {
      ipcRequestShmCrmErrorMarkConsumedReleaseCount += 1;
    },
    markConsumed(_block: C2IpcRequestShmBlock): void {
      ipcRequestShmCrmErrorMarkConsumedCount += 1;
      throw new Error("synthetic markConsumed failure after CRM error");
    },
  },
  requestShmThreshold: 2,
});
let ipcRequestShmCrmErrorMarkConsumedThrown = false;
try {
  await ipcRequestShmCrmErrorMarkConsumedFailureWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([38, 39, 40]));
} catch (error) {
  ipcRequestShmCrmErrorMarkConsumedThrown = true;
  const message = String(error);
  if (!(error instanceof C2IpcTransportError) || !message.includes("requestShmWriter markConsumed failed") || !message.includes("synthetic markConsumed failure")) {
    throw new Error(`unexpected IPC request SHM CRM-error markConsumed failure ${String(error)}`);
  }
}
if (!ipcRequestShmCrmErrorMarkConsumedThrown || ipcRequestShmCrmErrorMarkConsumedCount !== 1 || ipcRequestShmCrmErrorMarkConsumedReleaseCount !== 0) {
  throw new Error(`IPC request SHM hid markConsumed failure after CRM error: thrown=${ipcRequestShmCrmErrorMarkConsumedThrown} mark=${ipcRequestShmCrmErrorMarkConsumedCount} release=${ipcRequestShmCrmErrorMarkConsumedReleaseCount}`);
}

let ipcRequestShmFailureConnection: FailAfterWriteIpcConnection | undefined;
let ipcRequestShmFailureReleaseCount = 0;
const ipcRequestShmWriteFailureWire = createIpcEncodedTransport("ipc://request-shm-write-failure", {
  connect(): C2IpcConnection {
    ipcRequestShmFailureConnection = new FailAfterWriteIpcConnection([
      ipcServerHandshakeFrame(),
    ], 2);
    return ipcRequestShmFailureConnection;
  },
  requestShmWriter: {
    prefix: "/cc2c0002",
    segments: [{ name: "/cc2c0002_b0000", size: 4096 }],
    write(payload: Uint8Array): C2IpcRequestShmBlock {
      return { segmentIndex: 0, offset: 256, byteLength: payload.byteLength, dedicated: false };
    },
    release(block: C2IpcRequestShmBlock): void {
      if (block.segmentIndex !== 0 || block.offset !== 256) {
        throw new Error(`unexpected IPC request SHM failure release block ${JSON.stringify(block)}`);
      }
      ipcRequestShmFailureReleaseCount += 1;
    },
  },
  requestShmThreshold: 2,
});
let ipcRequestShmWriteFailureThrown = false;
try {
  await ipcRequestShmWriteFailureWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([41, 42, 43]));
} catch (error) {
  ipcRequestShmWriteFailureThrown = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("fake IPC write failure")) {
    throw new Error(`unexpected IPC request SHM write failure error ${String(error)}`);
  }
}
if (!ipcRequestShmWriteFailureThrown || ipcRequestShmFailureReleaseCount !== 1 || !ipcRequestShmFailureConnection?.closed) {
  throw new Error(`IPC request SHM did not release and close after frame write failure: thrown=${ipcRequestShmWriteFailureThrown} release=${ipcRequestShmFailureReleaseCount} closed=${ipcRequestShmFailureConnection?.closed ?? false}`);
}

let ipcDedicatedRequestShmReleaseCount = 0;
const ipcDedicatedRequestShmWire = createIpcEncodedTransport("ipc://request-shm-dedicated", {
  connect(): C2IpcConnection {
    return new FakeIpcConnection([
      ipcServerHandshakeFrame(),
      ipcSuccessReplyFrame(1, new Uint8Array([9])),
    ]);
  },
  requestShmWriter: {
    prefix: "/cc2c0003",
    segments: [{ name: "/cc2c0003_b0000", size: 4096 }],
    write(payload: Uint8Array): C2IpcRequestShmBlock {
      return { segmentIndex: 0, offset: 0, byteLength: payload.byteLength, dedicated: true };
    },
    release(block: C2IpcRequestShmBlock): void {
      if (!block.dedicated) {
        throw new Error("dedicated request SHM release did not receive a dedicated block");
      }
      ipcDedicatedRequestShmReleaseCount += 1;
    },
  },
  requestShmThreshold: 2,
});
const ipcDedicatedRequestShmResult = await ipcDedicatedRequestShmWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([51, 52, 53]));
if (ipcDedicatedRequestShmResult[0] !== 9 || ipcDedicatedRequestShmReleaseCount !== 1) {
  throw new Error(`IPC request SHM did not release dedicated block after response: result=${Array.from(ipcDedicatedRequestShmResult).join(",")} release=${ipcDedicatedRequestShmReleaseCount}`);
}

let ipcDedicatedRequestShmCrmErrorReleaseCount = 0;
const ipcDedicatedRequestShmCrmErrorReleaseFailureWire = createIpcEncodedTransport("ipc://request-shm-dedicated-crm-error-release-failure", {
  connect(): C2IpcConnection {
    return new FakeIpcConnection([
      ipcServerHandshakeFrame(),
      ipcErrorReplyFrame(1, new Uint8Array([9, 7])),
    ]);
  },
  requestShmWriter: {
    prefix: "/cc2c0014",
    segments: [{ name: "/cc2c0014_b0000", size: 4096 }],
    write(payload: Uint8Array): C2IpcRequestShmBlock {
      return { segmentIndex: 0, offset: 0, byteLength: payload.byteLength, dedicated: true };
    },
    release(_block: C2IpcRequestShmBlock): void {
      ipcDedicatedRequestShmCrmErrorReleaseCount += 1;
      throw new Error("synthetic dedicated release failure after CRM error");
    },
  },
  requestShmThreshold: 2,
});
let ipcDedicatedRequestShmCrmErrorReleaseThrown = false;
try {
  await ipcDedicatedRequestShmCrmErrorReleaseFailureWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([54, 55, 56]));
} catch (error) {
  ipcDedicatedRequestShmCrmErrorReleaseThrown = true;
  const message = String(error);
  if (!(error instanceof C2IpcTransportError) || !message.includes("requestShmWriter release failed") || !message.includes("synthetic dedicated release failure")) {
    throw new Error(`unexpected IPC dedicated request SHM CRM-error release failure ${String(error)}`);
  }
}
if (!ipcDedicatedRequestShmCrmErrorReleaseThrown || ipcDedicatedRequestShmCrmErrorReleaseCount !== 1) {
  throw new Error(`IPC dedicated request SHM hid release failure after CRM error: thrown=${ipcDedicatedRequestShmCrmErrorReleaseThrown} release=${ipcDedicatedRequestShmCrmErrorReleaseCount}`);
}

let ipcDedicatedRequestShmReadFailureReleaseCount = 0;
const ipcDedicatedRequestShmReadFailureWire = createIpcEncodedTransport("ipc://request-shm-dedicated-read-failure", {
  connect(): C2IpcConnection {
    return new FakeIpcConnection([
      ipcServerHandshakeFrame(),
    ]);
  },
  requestShmWriter: {
    prefix: "/cc2c0008",
    segments: [{ name: "/cc2c0008_b0000", size: 4096 }],
    write(payload: Uint8Array): C2IpcRequestShmBlock {
      return { segmentIndex: 0, offset: 0, byteLength: payload.byteLength, dedicated: true };
    },
    release(_block: C2IpcRequestShmBlock): void {
      ipcDedicatedRequestShmReadFailureReleaseCount += 1;
    },
  },
  requestShmThreshold: 2,
});
let ipcDedicatedRequestShmReadFailureThrown = false;
try {
  await ipcDedicatedRequestShmReadFailureWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([61, 62, 63]));
} catch (error) {
  ipcDedicatedRequestShmReadFailureThrown = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("response read failed")) {
    throw new Error(`unexpected IPC dedicated request SHM response read failure ${String(error)}`);
  }
}
if (!ipcDedicatedRequestShmReadFailureThrown || ipcDedicatedRequestShmReadFailureReleaseCount !== 0) {
  throw new Error(`IPC request SHM released a dedicated block without a response frame: thrown=${ipcDedicatedRequestShmReadFailureThrown} release=${ipcDedicatedRequestShmReadFailureReleaseCount}`);
}

let ipcBadRequestShmBlockReleaseCount = 0;
const ipcBadRequestShmBlockWire = createIpcEncodedTransport("ipc://request-shm-bad-block", {
  connect(): C2IpcConnection {
    return new FakeIpcConnection([
      ipcServerHandshakeFrame(),
    ]);
  },
  requestShmWriter: {
    prefix: "/cc2c0004",
    segments: [{ name: "/cc2c0004_b0000", size: 4096 }],
    write(_payload: Uint8Array): C2IpcRequestShmBlock {
      return { segmentIndex: 0, offset: 0, byteLength: 4, dedicated: false };
    },
    release(_block: C2IpcRequestShmBlock): void {
      ipcBadRequestShmBlockReleaseCount += 1;
    },
  },
  requestShmThreshold: 2,
});
let ipcBadRequestShmBlockThrown = false;
try {
  await ipcBadRequestShmBlockWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1, 2, 3]));
} catch (error) {
  ipcBadRequestShmBlockThrown = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("byteLength")) {
    throw new Error(`unexpected IPC request SHM bad block error ${String(error)}`);
  }
}
if (!ipcBadRequestShmBlockThrown || ipcBadRequestShmBlockReleaseCount !== 1) {
  throw new Error(`IPC request SHM did not reject and release an invalid block: thrown=${ipcBadRequestShmBlockThrown} release=${ipcBadRequestShmBlockReleaseCount}`);
}

let ipcBadRequestShmWriterThrown = false;
try {
  createIpcEncodedTransport("ipc://bad-request-shm-writer", {
    connect(): C2IpcConnection {
      throw new Error("bad request SHM writer should fail before connect");
    },
    requestShmWriter: {
      prefix: "/cc2c0005",
      segments: [{ name: "/cc2c0005_b0000", size: 4096 }],
      write(_payload: Uint8Array): C2IpcRequestShmBlock {
        return { segmentIndex: 0, offset: 0, byteLength: 0, dedicated: false };
      },
      release: "not-a-function" as unknown as C2IpcRequestShmWriter["release"],
    },
  });
} catch (error) {
  ipcBadRequestShmWriterThrown = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("requestShmWriter")) {
    throw new Error(`unexpected IPC bad request SHM writer error ${String(error)}`);
  }
}
if (!ipcBadRequestShmWriterThrown) {
  throw new Error("IPC transport accepted an invalid requestShmWriter");
}

let ipcTooLongRequestShmPrefixThrown = false;
try {
  createIpcEncodedTransport("ipc://too-long-request-shm-prefix", {
    connect(): C2IpcConnection {
      throw new Error("too-long request SHM prefix should fail before connect");
    },
    requestShmWriter: {
      prefix: "/cc2n_prefix_that_is_too_long",
      segments: [],
      write(payload: Uint8Array): C2IpcRequestShmBlock {
        return { segmentIndex: 0, offset: 0, byteLength: payload.byteLength, dedicated: true };
      },
      release(_block: C2IpcRequestShmBlock): void {
      },
    },
  });
} catch (error) {
  ipcTooLongRequestShmPrefixThrown = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("prefix cannot exceed")) {
    throw new Error(`unexpected too-long request SHM prefix error ${String(error)}`);
  }
}
if (!ipcTooLongRequestShmPrefixThrown) {
  throw new Error("IPC transport accepted a requestShmWriter prefix that can panic the Rust peer pool");
}

const dedicatedResponseFs = new FakeNodePosixShmFileSystem();
dedicatedResponseFs.files.set("/dev/shm/cc2s0008_d0100", {
  data: (() => {
    const bytes = new Uint8Array(4096);
    bytes.set([71, 72, 73], 64);
    return bytes;
  })(),
});
const dedicatedResponseReader = createNodePosixDedicatedResponseShmReader({ fs: dedicatedResponseFs });
const dedicatedResponseDestination = new Uint8Array(3);
const dedicatedResponseReadResult = await dedicatedResponseReader.read({
  prefix: "/cc2s0008",
  segments: [],
  segmentIndex: 256,
  segmentName: "/cc2s0008_d0100",
  offset: 0,
  byteLength: 3,
  dedicated: true,
}, dedicatedResponseDestination);
if (dedicatedResponseReadResult !== undefined || Array.from(dedicatedResponseDestination).join(",") !== "71,72,73") {
  throw new Error(`Node POSIX dedicated response reader did not fill the destination: ${Array.from(dedicatedResponseDestination).join(",")}`);
}
await dedicatedResponseReader.release({
  prefix: "/cc2s0008",
  segments: [],
  segmentIndex: 256,
  segmentName: "/cc2s0008_d0100",
  offset: 0,
  byteLength: 3,
  dedicated: true,
});
const dedicatedResponseFile = dedicatedResponseFs.files.get("/dev/shm/cc2s0008_d0100");
if (!dedicatedResponseFile || new DataView(dedicatedResponseFile.data.buffer).getUint32(0, true) !== 1) {
  throw new Error("Node POSIX dedicated response reader did not mark read_done on release");
}

let nonDedicatedResponseRejected = false;
try {
  await dedicatedResponseReader.read({
    prefix: "/cc2s0008",
    segments: [{ name: "/cc2s0008_b0000", size: 4096 }],
    segmentIndex: 0,
    segmentName: "/cc2s0008_b0000",
    offset: 0,
    byteLength: 3,
    dedicated: false,
  });
} catch (error) {
  nonDedicatedResponseRejected = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("only supports dedicated")) {
    throw new Error(`unexpected non-dedicated Node POSIX SHM reader error ${String(error)}`);
  }
}
if (!nonDedicatedResponseRejected) {
  throw new Error("Node POSIX dedicated response reader accepted a non-dedicated buddy block");
}

let dedicatedNonzeroOffsetRejected = false;
try {
  await dedicatedResponseReader.read({
    prefix: "/cc2s0008",
    segments: [],
    segmentIndex: 256,
    segmentName: "/cc2s0008_d0100",
    offset: 1,
    byteLength: 3,
    dedicated: true,
  });
} catch (error) {
  dedicatedNonzeroOffsetRejected = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("offset must be 0")) {
    throw new Error(`unexpected nonzero dedicated offset error ${String(error)}`);
  }
}
if (!dedicatedNonzeroOffsetRejected) {
  throw new Error("Node POSIX dedicated response reader accepted a nonzero dedicated offset");
}

let nodeDedicatedRequestConnection: FakeIpcConnection | undefined;
const nodeDedicatedRequestFs = new FakeNodePosixShmFileSystem();
const nodeDedicatedRequestWriter = createNodePosixDedicatedRequestShmWriter({
  fs: nodeDedicatedRequestFs,
  prefix: "/cc2n0001",
  startSegmentIndex: 256,
});
const nodeDedicatedRequestWire = createIpcEncodedTransport("ipc://node-dedicated-request-shm", {
  connect(): C2IpcConnection {
    nodeDedicatedRequestConnection = new FakeIpcConnection([
      ipcServerHandshakeFrame(),
      ipcSuccessReplyFrame(1, new Uint8Array([12])),
    ]);
    return nodeDedicatedRequestConnection;
  },
  requestShmWriter: nodeDedicatedRequestWriter,
  requestShmThreshold: 2,
});
const nodeDedicatedRequestResult = await nodeDedicatedRequestWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([81, 82, 83]));
if (nodeDedicatedRequestResult[0] !== 12 || !nodeDedicatedRequestConnection) {
  throw new Error("Node POSIX dedicated request writer call did not complete");
}
const nodeDedicatedRequestHandshake = ipcHandshakeSummary(ipcFramePayload(nodeDedicatedRequestConnection.writes[0]));
if (nodeDedicatedRequestHandshake.prefix !== "/cc2n0001" || nodeDedicatedRequestHandshake.segments.length !== 0) {
  throw new Error(`Node POSIX dedicated request writer advertised unexpected handshake metadata: ${JSON.stringify(nodeDedicatedRequestHandshake)}`);
}
const nodeDedicatedRequestBuddy = ipcBuddyHeader(ipcFramePayload(nodeDedicatedRequestConnection.writes[1]));
if (!nodeDedicatedRequestBuddy.dedicated || nodeDedicatedRequestBuddy.segmentIndex !== 256 || nodeDedicatedRequestBuddy.offset !== 0 || nodeDedicatedRequestBuddy.byteLength !== 3) {
  throw new Error(`Node POSIX dedicated request writer returned wrong buddy metadata: ${JSON.stringify(nodeDedicatedRequestBuddy)}`);
}
if (nodeDedicatedRequestFs.unlinks.join(",") !== "/dev/shm/cc2n0001_d0100") {
  throw new Error(`Node POSIX dedicated request writer did not unlink after response: ${nodeDedicatedRequestFs.unlinks.join(",")}`);
}
let duplicateDedicatedRequestReleaseRejected = false;
try {
  await nodeDedicatedRequestWriter.release({
    segmentIndex: 256,
    offset: 0,
    byteLength: 3,
    dedicated: true,
  });
} catch (error) {
  duplicateDedicatedRequestReleaseRejected = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("unknown or already released")) {
    throw new Error(`unexpected duplicate dedicated request release error ${String(error)}`);
  }
}
if (!duplicateDedicatedRequestReleaseRejected) {
  throw new Error("Node POSIX dedicated request writer released the same block twice");
}
if (nodeDedicatedRequestFs.unlinks.join(",") !== "/dev/shm/cc2n0001_d0100") {
  throw new Error(`Node POSIX dedicated request writer unlinked an unowned block: ${nodeDedicatedRequestFs.unlinks.join(",")}`);
}

const nativeBuddyBackend = new FakeNodePosixNativeBuddyBackend();
const nativeBuddyResponseReader = createNodePosixNativeBuddyResponseShmReader({ backend: nativeBuddyBackend });
const nativeBuddyDestination = new Uint8Array(3);
const nativeBuddyReadResult = await nativeBuddyResponseReader.read({
  prefix: "/cc2s0009",
  segments: [{ name: "/cc2s0009_b0000", size: 4096 }],
  segmentIndex: 0,
  segmentName: "/cc2s0009_b0000",
  offset: 128,
  byteLength: 3,
  dedicated: false,
}, nativeBuddyDestination);
if (nativeBuddyReadResult !== undefined || Array.from(nativeBuddyDestination).join(",") !== "91,92,93") {
  throw new Error(`Node POSIX native buddy response reader did not fill the destination: ${Array.from(nativeBuddyDestination).join(",")}`);
}
await nativeBuddyResponseReader.release({
  prefix: "/cc2s0009",
  segments: [{ name: "/cc2s0009_b0000", size: 4096 }],
  segmentIndex: 0,
  segmentName: "/cc2s0009_b0000",
  offset: 128,
  byteLength: 3,
  dedicated: false,
});
if (nativeBuddyBackend.responseReads.length !== 1 || nativeBuddyBackend.responseReleases.length !== 1) {
  throw new Error("Node POSIX native buddy response reader did not delegate read and release");
}

const nativeBuddyOwnedRead = await nativeBuddyResponseReader.read({
  prefix: "/cc2s0009",
  segments: [{ name: "/cc2s0009_b0000", size: 4096 }],
  segmentIndex: 0,
  segmentName: "/cc2s0009_b0000",
  offset: 256,
  byteLength: 3,
  dedicated: false,
});
if (!(nativeBuddyOwnedRead instanceof Uint8Array) || Array.from(nativeBuddyOwnedRead).join(",") !== "91,92,93") {
  throw new Error("Node POSIX native buddy response reader did not return an owned Uint8Array without destination");
}

const nativeBuddyReadCountBeforeDedicatedBlock = nativeBuddyBackend.responseReads.length;
let nativeBuddyDedicatedBlockRejected = false;
try {
  await nativeBuddyResponseReader.read({
    prefix: "/cc2s0009",
    segments: [],
    segmentIndex: 256,
    segmentName: "/cc2s0009_d0100",
    offset: 0,
    byteLength: 3,
    dedicated: true,
  }, new Uint8Array(3));
} catch (error) {
  nativeBuddyDedicatedBlockRejected = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("only supports non-dedicated")) {
    throw new Error(`unexpected native buddy dedicated response block error ${String(error)}`);
  }
}
if (!nativeBuddyDedicatedBlockRejected || nativeBuddyBackend.responseReads.length !== nativeBuddyReadCountBeforeDedicatedBlock) {
  throw new Error("Node POSIX native buddy response reader accepted a dedicated block");
}

const aliasReturningNativeBuddyReader = createNodePosixNativeBuddyResponseShmReader({
  backend: {
    readResponse(_block: C2IpcResponseShmBlock, _destination: Uint8Array): Uint8Array {
      return new Uint8Array([1, 2, 3]);
    },
    releaseResponse(_block: C2IpcResponseShmBlock): void {
    },
  } as unknown as C2NodePosixNativeBuddyResponseShmBackend,
});
const aliasReturningNativeBuddyDestination = new Uint8Array(3);
const aliasReturningNativeBuddyResult = await aliasReturningNativeBuddyReader.read({
  prefix: "/cc2s0009",
  segments: [{ name: "/cc2s0009_b0000", size: 4096 }],
  segmentIndex: 0,
  segmentName: "/cc2s0009_b0000",
  offset: 128,
  byteLength: 3,
  dedicated: false,
}, aliasReturningNativeBuddyDestination);
if (aliasReturningNativeBuddyResult !== undefined || Array.from(aliasReturningNativeBuddyDestination).join(",") !== "1,2,3") {
  throw new Error("Node POSIX native buddy response reader exposed or ignored a returned aliasable buffer");
}

let nativeBuddyWrongReturnReleaseCount = 0;
let nativeBuddyWrongReturnRejected = false;
try {
  const wrongReturningNativeBuddyReader = createNodePosixNativeBuddyResponseShmReader({
    backend: {
      readResponse(_block: C2IpcResponseShmBlock, _destination: Uint8Array): Uint8Array {
        return new Uint8Array([1, 2]);
      },
      releaseResponse(_block: C2IpcResponseShmBlock): void {
        nativeBuddyWrongReturnReleaseCount += 1;
      },
    } as unknown as C2NodePosixNativeBuddyResponseShmBackend,
  });
  await wrongReturningNativeBuddyReader.read({
    prefix: "/cc2s0009",
    segments: [{ name: "/cc2s0009_b0000", size: 4096 }],
    segmentIndex: 0,
    segmentName: "/cc2s0009_b0000",
    offset: 128,
    byteLength: 3,
    dedicated: false,
  }, new Uint8Array(3));
} catch (error) {
  nativeBuddyWrongReturnRejected = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("returned 2 bytes")) {
    throw new Error(`unexpected native buddy wrong-return error ${String(error)}`);
  }
}
if (!nativeBuddyWrongReturnRejected || nativeBuddyWrongReturnReleaseCount !== 1) {
  throw new Error(`Node POSIX native buddy response reader failed to release after invalid returned bytes: rejected=${nativeBuddyWrongReturnRejected} release=${nativeBuddyWrongReturnReleaseCount}`);
}

const nativeBuddyReadCountBeforeDedicatedOffset = nativeBuddyBackend.responseReads.length;
let ipcDedicatedResponseOffsetRejected = false;
try {
  const dedicatedOffsetWire = createIpcEncodedTransport("ipc://dedicated-response-offset", {
    connect(): C2IpcConnection {
      return new FakeIpcConnection([
        ipcServerHandshakeFrame({ shmPrefix: "/cc2s0009" }),
        ipcBuddySuccessReplyFrame(1, 256, 1, 3, true),
      ]);
    },
    responseShmReader: nativeBuddyResponseReader,
  });
  await dedicatedOffsetWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  ipcDedicatedResponseOffsetRejected = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("dedicated SHM response offset must be 0")) {
    throw new Error(`unexpected dedicated response offset error ${String(error)}`);
  }
}
if (!ipcDedicatedResponseOffsetRejected) {
  throw new Error("IPC transport accepted a dedicated SHM response with nonzero offset");
}
if (nativeBuddyBackend.responseReads.length !== nativeBuddyReadCountBeforeDedicatedOffset) {
  throw new Error("IPC transport invoked responseShmReader for a dedicated response with nonzero offset");
}

let nodeNativeBuddyRequestConnection: FakeIpcConnection | undefined;
const nodeNativeBuddyRequestWriter = createNodePosixNativeBuddyRequestShmWriter({ backend: nativeBuddyBackend });
const nodeNativeBuddyRequestWire = createIpcEncodedTransport("ipc://node-native-buddy-request-shm", {
  connect(): C2IpcConnection {
    nodeNativeBuddyRequestConnection = new FakeIpcConnection([
      ipcServerHandshakeFrame(),
      ipcSuccessReplyFrame(1, new Uint8Array([13])),
    ]);
    return nodeNativeBuddyRequestConnection;
  },
  requestShmWriter: nodeNativeBuddyRequestWriter,
  requestShmThreshold: 2,
});
const nodeNativeBuddyRequestResult = await nodeNativeBuddyRequestWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([84, 85, 86]));
if (nodeNativeBuddyRequestResult[0] !== 13 || !nodeNativeBuddyRequestConnection) {
  throw new Error("Node POSIX native buddy request writer call did not complete");
}
const nodeNativeBuddyRequestHandshake = ipcHandshakeSummary(ipcFramePayload(nodeNativeBuddyRequestConnection.writes[0]));
if (nodeNativeBuddyRequestHandshake.prefix !== "/cc2n0002" || nodeNativeBuddyRequestHandshake.segments.length !== 1 || nodeNativeBuddyRequestHandshake.segments[0].name !== "/cc2n0002_b0000" || nodeNativeBuddyRequestHandshake.segments[0].size !== 4096) {
  throw new Error(`Node POSIX native buddy request writer advertised unexpected handshake metadata: ${JSON.stringify(nodeNativeBuddyRequestHandshake)}`);
}
const nodeNativeBuddyRequestBuddy = ipcBuddyHeader(ipcFramePayload(nodeNativeBuddyRequestConnection.writes[1]));
if (nodeNativeBuddyRequestBuddy.dedicated || nodeNativeBuddyRequestBuddy.segmentIndex !== 0 || nodeNativeBuddyRequestBuddy.offset !== 128 || nodeNativeBuddyRequestBuddy.byteLength !== 3) {
  throw new Error(`Node POSIX native buddy request writer returned wrong buddy metadata: ${JSON.stringify(nodeNativeBuddyRequestBuddy)}`);
}
if (nativeBuddyBackend.requestReleases.length !== 0) {
  throw new Error("Node POSIX native buddy request writer released a server-consumed non-dedicated block");
}
if (nativeBuddyBackend.requestConsumes.length !== 1 || nativeBuddyBackend.requestConsumes[0]?.offset !== 128) {
  throw new Error("Node POSIX native buddy request writer did not mark the server-consumed non-dedicated block");
}

const c2MemFfiResponseFactory = new FakeC2MemFfiResponseFactory();
const c2MemFfiResponseReader = createC2MemFfiNativeBuddyResponseShmReader({
  binding: c2MemFfiResponseFactory,
  maxSegments: 8,
  minBlockSize: 4096,
});
const c2MemFfiDestination = new Uint8Array(3);
await c2MemFfiResponseReader.read({
  prefix: "/cc2sffi1",
  segments: [{ name: "/cc2sffi1_b0000", size: 8192 }],
  segmentIndex: 0,
  segmentName: "/cc2sffi1_b0000",
  offset: 512,
  byteLength: 3,
  dedicated: false,
}, c2MemFfiDestination);
if (Array.from(c2MemFfiDestination).join(",") !== "71,72,73") {
  throw new Error(`c2-mem-ffi response reader did not fill destination: ${Array.from(c2MemFfiDestination).join(",")}`);
}
await c2MemFfiResponseReader.release({
  prefix: "/cc2sffi1",
  segments: [{ name: "/cc2sffi1_b0000", size: 8192 }],
  segmentIndex: 0,
  segmentName: "/cc2sffi1_b0000",
  offset: 512,
  byteLength: 3,
  dedicated: false,
});
const c2MemFfiResponsePool = c2MemFfiResponseFactory.pools.get("/cc2sffi1");
if (!c2MemFfiResponsePool || c2MemFfiResponsePool.reads.length !== 1 || c2MemFfiResponsePool.releases.length !== 1) {
  throw new Error("c2-mem-ffi response reader did not delegate read and release through the prefix pool");
}
if (c2MemFfiResponseFactory.options.length !== 1 || c2MemFfiResponseFactory.options[0].segmentSize !== 8192 || c2MemFfiResponseFactory.options[0].maxSegments !== 8 || c2MemFfiResponseFactory.options[0].minBlockSize !== 4096) {
  throw new Error(`c2-mem-ffi response reader created pool with wrong options: ${JSON.stringify(c2MemFfiResponseFactory.options)}`);
}
await c2MemFfiResponseReader.close();
if (c2MemFfiResponsePool.closeCount !== 1) {
  throw new Error("c2-mem-ffi response reader did not close the created pool");
}
await c2MemFfiResponseReader.close();
if (c2MemFfiResponsePool.closeCount !== 1) {
  throw new Error("c2-mem-ffi response reader close was not idempotent");
}
let c2MemFfiResponseAfterCloseRejected = false;
try {
  await c2MemFfiResponseReader.read({
    prefix: "/cc2sffi1",
    segments: [{ name: "/cc2sffi1_b0000", size: 8192 }],
    segmentIndex: 0,
    segmentName: "/cc2sffi1_b0000",
    offset: 512,
    byteLength: 3,
    dedicated: false,
  }, new Uint8Array(3));
} catch (error) {
  c2MemFfiResponseAfterCloseRejected = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("is closed")) {
    throw new Error(`unexpected c2-mem-ffi response after close error ${String(error)}`);
  }
}
if (!c2MemFfiResponseAfterCloseRejected || c2MemFfiResponseFactory.options.length !== 1) {
  throw new Error("c2-mem-ffi response reader created a pool after close");
}

const c2MemFfiUnreadReleaseReader = createC2MemFfiNativeBuddyResponseShmReader({
  binding: c2MemFfiResponseFactory,
  segmentSize: 8192,
});
await c2MemFfiUnreadReleaseReader.release({
  prefix: "/cc2sffi-unread",
  segments: [],
  segmentIndex: 0,
  segmentName: "/cc2sffi-unread_b0000",
  offset: 0,
  byteLength: 3,
  dedicated: false,
});
const c2MemFfiUnreadReleasePool = c2MemFfiResponseFactory.pools.get("/cc2sffi-unread");
if (!c2MemFfiUnreadReleasePool || c2MemFfiUnreadReleasePool.reads.length !== 0 || c2MemFfiUnreadReleasePool.releases.length !== 1) {
  throw new Error("c2-mem-ffi response reader did not delegate unread release through a prefix pool");
}
const c2MemFfiUnreadReleaseOptions = c2MemFfiResponseFactory.options.slice();
if (c2MemFfiUnreadReleaseOptions.length !== 2 || c2MemFfiUnreadReleaseOptions[1].prefix !== "/cc2sffi-unread" || c2MemFfiUnreadReleaseOptions[1].segmentSize !== 8192) {
  throw new Error(`c2-mem-ffi response reader created unread-release pool with wrong options: ${JSON.stringify(c2MemFfiResponseFactory.options)}`);
}
await c2MemFfiUnreadReleaseReader.close();
if (c2MemFfiUnreadReleasePool.closeCount !== 1) {
  throw new Error("c2-mem-ffi response reader did not close the unread-release pool");
}

const c2MemFfiRequestPool = new FakeC2MemFfiRequestPool();
const c2MemFfiRequestWriter = createC2MemFfiNativeBuddyRequestShmWriter({ pool: c2MemFfiRequestPool });
let c2MemFfiRequestConnection: FakeIpcConnection | undefined;
const c2MemFfiRequestWire = createIpcEncodedTransport("ipc://c2-mem-ffi-request-shm", {
  connect(): C2IpcConnection {
    c2MemFfiRequestConnection = new FakeIpcConnection([
      ipcServerHandshakeFrame(),
      ipcSuccessReplyFrame(1, new Uint8Array([14])),
    ]);
    return c2MemFfiRequestConnection;
  },
  requestShmWriter: c2MemFfiRequestWriter,
  requestShmThreshold: 2,
});
const c2MemFfiRequestResult = await c2MemFfiRequestWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([94, 95, 96]));
if (c2MemFfiRequestResult[0] !== 14 || !c2MemFfiRequestConnection) {
  throw new Error("c2-mem-ffi request writer call did not complete");
}
const c2MemFfiRequestHandshake = ipcHandshakeSummary(ipcFramePayload(c2MemFfiRequestConnection.writes[0]));
if (c2MemFfiRequestHandshake.prefix !== "/cc2nffi1" || c2MemFfiRequestHandshake.segments.length !== 1 || c2MemFfiRequestHandshake.segments[0].size !== 4096) {
  throw new Error(`c2-mem-ffi request writer advertised unexpected handshake metadata: ${JSON.stringify(c2MemFfiRequestHandshake)}`);
}
const c2MemFfiRequestBuddy = ipcBuddyHeader(ipcFramePayload(c2MemFfiRequestConnection.writes[1]));
if (c2MemFfiRequestBuddy.dedicated || c2MemFfiRequestBuddy.segmentIndex !== 0 || c2MemFfiRequestBuddy.offset !== 256 || c2MemFfiRequestBuddy.byteLength !== 3) {
  throw new Error(`c2-mem-ffi request writer returned wrong buddy metadata: ${JSON.stringify(c2MemFfiRequestBuddy)}`);
}
if (c2MemFfiRequestPool.releases.length !== 0 || c2MemFfiRequestPool.consumed.length !== 1 || c2MemFfiRequestPool.consumed[0]?.offset !== 256) {
  throw new Error("c2-mem-ffi request writer did not transfer accepted non-dedicated ownership via forgetConsumed");
}
c2MemFfiRequestPool.writeOffset = 4096;
let c2MemFfiInvalidRequestConnection: FakeIpcConnection | undefined;
let c2MemFfiInvalidRequestRejected = false;
try {
  const c2MemFfiInvalidRequestWire = createIpcEncodedTransport("ipc://c2-mem-ffi-invalid-request-shm", {
    connect(): C2IpcConnection {
      c2MemFfiInvalidRequestConnection = new FakeIpcConnection([ipcServerHandshakeFrame()]);
      return c2MemFfiInvalidRequestConnection;
    },
    requestShmWriter: c2MemFfiRequestWriter,
    requestShmThreshold: 2,
  });
  await c2MemFfiInvalidRequestWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([97, 98, 99]));
} catch (error) {
  c2MemFfiInvalidRequestRejected = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("exceeds segment")) {
    throw new Error(`unexpected c2-mem-ffi invalid request error ${String(error)}`);
  }
}
if (!c2MemFfiInvalidRequestRejected || !c2MemFfiInvalidRequestConnection || c2MemFfiInvalidRequestConnection.writes.length !== 1) {
  throw new Error("c2-mem-ffi request writer accepted an invalid block or wrote a buddy frame");
}
const c2MemFfiRequestReleasesAfterInvalid = Array.from(c2MemFfiRequestPool.releases);
if (c2MemFfiRequestReleasesAfterInvalid.length !== 1 || c2MemFfiRequestReleasesAfterInvalid[0].offset !== 4096) {
  throw new Error("c2-mem-ffi request writer did not release an invalid returned block locally");
}
await c2MemFfiRequestWriter.close();
if (c2MemFfiRequestPool.closeCount !== 1) {
  throw new Error("c2-mem-ffi request writer did not close the request pool");
}
await c2MemFfiRequestWriter.close();
if (c2MemFfiRequestPool.closeCount !== 1) {
  throw new Error("c2-mem-ffi request writer close was not idempotent");
}
let c2MemFfiRequestAfterCloseRejected = false;
try {
  await c2MemFfiRequestWriter.write(new Uint8Array([1]));
} catch (error) {
  c2MemFfiRequestAfterCloseRejected = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("is closed")) {
    throw new Error(`unexpected c2-mem-ffi request after close error ${String(error)}`);
  }
}
if (!c2MemFfiRequestAfterCloseRejected || c2MemFfiRequestPool.writes.length !== 2) {
  throw new Error("c2-mem-ffi request writer wrote after close");
}

let nativeBuddyInvalidRequestConnection: FakeIpcConnection | undefined;
nativeBuddyBackend.writeOffset = 4096;
let nativeBuddyInvalidRequestRejected = false;
try {
  const nodeNativeBuddyInvalidWire = createIpcEncodedTransport("ipc://node-native-buddy-invalid-request-shm", {
    connect(): C2IpcConnection {
      nativeBuddyInvalidRequestConnection = new FakeIpcConnection([ipcServerHandshakeFrame()]);
      return nativeBuddyInvalidRequestConnection;
    },
    requestShmWriter: nodeNativeBuddyRequestWriter,
    requestShmThreshold: 2,
  });
  await nodeNativeBuddyInvalidWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([87, 88, 89]));
} catch (error) {
  nativeBuddyInvalidRequestRejected = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("exceeds segment")) {
    throw new Error(`unexpected native buddy invalid request error ${String(error)}`);
  }
}
if (!nativeBuddyInvalidRequestRejected) {
  throw new Error("Node POSIX native buddy request writer accepted an out-of-range block");
}
if (!nativeBuddyInvalidRequestConnection || nativeBuddyInvalidRequestConnection.writes.length !== 1) {
  throw new Error("Node POSIX native buddy invalid request wrote a buddy call frame");
}
const nativeBuddyRequestReleasesAfterInvalid = Array.from(nativeBuddyBackend.requestReleases);
if (nativeBuddyRequestReleasesAfterInvalid.length !== 1 || nativeBuddyRequestReleasesAfterInvalid[0].offset !== 4096) {
  throw new Error("Node POSIX native buddy request writer did not release an invalid block locally");
}

let nativeBuddyDedicatedRequestReleaseCount = 0;
let nativeBuddyDedicatedRequestConnection: FakeIpcConnection | undefined;
let nativeBuddyDedicatedRequestRejected = false;
try {
  const dedicatedReturningNativeBuddyWriter = createNodePosixNativeBuddyRequestShmWriter({
    backend: {
      prefix: "/cc2n0003",
      segments: [{ name: "/cc2n0003_b0000", size: 4096 }],
      writeRequest(payload: Uint8Array): C2IpcRequestShmBlock {
        return { segmentIndex: 0, offset: 0, byteLength: payload.byteLength, dedicated: true };
      },
      releaseRequest(block: C2IpcRequestShmBlock): void {
        if (!block.dedicated) {
          throw new Error("dedicated-returning native buddy backend released the wrong block");
        }
        nativeBuddyDedicatedRequestReleaseCount += 1;
      },
    },
  });
  const dedicatedReturningNativeBuddyWire = createIpcEncodedTransport("ipc://node-native-buddy-dedicated-request-shm", {
    connect(): C2IpcConnection {
      nativeBuddyDedicatedRequestConnection = new FakeIpcConnection([ipcServerHandshakeFrame()]);
      return nativeBuddyDedicatedRequestConnection;
    },
    requestShmWriter: dedicatedReturningNativeBuddyWriter,
    requestShmThreshold: 2,
  });
  await dedicatedReturningNativeBuddyWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([87, 88, 89]));
} catch (error) {
  nativeBuddyDedicatedRequestRejected = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("must return non-dedicated")) {
    throw new Error(`unexpected native buddy dedicated request error ${String(error)}`);
  }
}
if (!nativeBuddyDedicatedRequestRejected) {
  throw new Error("Node POSIX native buddy request writer accepted a dedicated block");
}
if (!nativeBuddyDedicatedRequestConnection || nativeBuddyDedicatedRequestConnection.writes.length !== 1 || nativeBuddyDedicatedRequestReleaseCount !== 1) {
  throw new Error("Node POSIX native buddy request writer did not release a rejected dedicated block before writing a buddy call frame");
}

let dedicatedOffsetRequestReleaseCount = 0;
let dedicatedOffsetRequestConnection: FakeIpcConnection | undefined;
let ipcDedicatedRequestOffsetRejected = false;
try {
  const dedicatedOffsetRequestWire = createIpcEncodedTransport("ipc://dedicated-request-offset", {
    connect(): C2IpcConnection {
      dedicatedOffsetRequestConnection = new FakeIpcConnection([ipcServerHandshakeFrame()]);
      return dedicatedOffsetRequestConnection;
    },
    requestShmWriter: {
      prefix: "/cc2n0003",
      segments: [],
      write(payload: Uint8Array): C2IpcRequestShmBlock {
        return { segmentIndex: 256, offset: 1, byteLength: payload.byteLength, dedicated: true };
      },
      release(_block: C2IpcRequestShmBlock): void {
        dedicatedOffsetRequestReleaseCount += 1;
      },
    },
    requestShmThreshold: 2,
  });
  await dedicatedOffsetRequestWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([90, 91, 92]));
} catch (error) {
  ipcDedicatedRequestOffsetRejected = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("dedicated block offset must be 0")) {
    throw new Error(`unexpected dedicated request offset error ${String(error)}`);
  }
}
if (!ipcDedicatedRequestOffsetRejected) {
  throw new Error("IPC transport accepted a dedicated request SHM block with nonzero offset");
}
if (!dedicatedOffsetRequestConnection || dedicatedOffsetRequestConnection.writes.length !== 1 || dedicatedOffsetRequestReleaseCount !== 1) {
  throw new Error("IPC transport did not release a rejected dedicated request block before writing a buddy call frame");
}

const ipcAllocatedResponse = { byteLength: 3, tag: "ipc-allocated-response" };
const ipcAllocatedBytes = new Uint8Array(3);
let ipcAllocatedReleaseCount = 0;
const ipcAllocatedWire = createIpcEncodedTransport("ipc://allocated", {
  connect(): C2IpcConnection {
    return new FakeIpcConnection([
      ipcServerHandshakeFrame(),
      ipcSuccessReplyFrame(1, new Uint8Array([4, 5, 6])),
    ]);
  },
  responsePayloadAllocator(byteLength: number) {
    if (byteLength !== 3) {
      throw new Error(`unexpected IPC allocated response length ${byteLength}`);
    }
    return {
      payload: ipcAllocatedResponse,
      view: ipcAllocatedBytes,
      release(): void {
        ipcAllocatedReleaseCount += 1;
      },
    };
  },
});
const ipcAllocatedResult = await ipcAllocatedWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
if (ipcAllocatedResult !== ipcAllocatedResponse) {
  throw new Error("IPC transport did not return provider-owned allocated response");
}
if (ipcAllocatedBytes[0] !== 4 || ipcAllocatedBytes[1] !== 5 || ipcAllocatedBytes[2] !== 6) {
  throw new Error(`IPC transport did not copy inline reply into allocated response bytes: ${Array.from(ipcAllocatedBytes).join(",")}`);
}
if (ipcAllocatedReleaseCount !== 0) {
  throw new Error("IPC transport released a successful allocated response");
}

const ipcChunkedWire = createIpcEncodedTransport("ipc://chunked", {
  connect(): C2IpcConnection {
    return new FakeIpcConnection([
      ipcServerHandshakeFrame(),
      ipcReplyChunkFrame(1, 5, 2, 0, new Uint8Array([10, 11, 12]), false),
      ipcReplyChunkFrame(1, 5, 2, 1, new Uint8Array([13, 14]), true),
    ]);
  },
});
const ipcChunkedResult = await ipcChunkedWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
if (ipcChunkedResult[0] !== 10 || ipcChunkedResult[1] !== 11 || ipcChunkedResult[2] !== 12 || ipcChunkedResult[3] !== 13 || ipcChunkedResult[4] !== 14) {
  throw new Error(`IPC transport did not assemble chunked response bytes: ${Array.from(ipcChunkedResult).join(",")}`);
}

const ipcChunkedAllocatedResponse = { byteLength: 4, tag: "ipc-chunked-allocated-response" };
const ipcChunkedAllocatedBytes = new Uint8Array(4);
const ipcChunkedAllocatedWire = createIpcEncodedTransport("ipc://chunked-allocated", {
  connect(): C2IpcConnection {
    return new FakeIpcConnection([
      ipcServerHandshakeFrame(),
      ipcReplyChunkFrame(1, 4, 2, 0, new Uint8Array([21, 22]), false),
      ipcReplyChunkFrame(1, 4, 2, 1, new Uint8Array([23, 24]), true),
    ]);
  },
  responsePayloadAllocator(byteLength: number) {
    if (byteLength !== 4) {
      throw new Error(`unexpected IPC chunked allocated response length ${byteLength}`);
    }
    return {
      payload: ipcChunkedAllocatedResponse,
      view: ipcChunkedAllocatedBytes,
    };
  },
});
const ipcChunkedAllocatedResult = await ipcChunkedAllocatedWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
if (ipcChunkedAllocatedResult !== ipcChunkedAllocatedResponse || ipcChunkedAllocatedBytes[0] !== 21 || ipcChunkedAllocatedBytes[3] !== 24) {
  throw new Error("IPC transport did not copy chunked response into the allocated payload");
}

let ipcImpossibleChunkLayoutConnection: FakeIpcConnection | undefined;
const ipcImpossibleChunkLayoutWire = createIpcEncodedTransport("ipc://chunked-impossible-layout", {
  connect(): C2IpcConnection {
    ipcImpossibleChunkLayoutConnection = new FakeIpcConnection([
      ipcServerHandshakeFrame(),
      ipcReplyChunkFrame(1, 5, 2, 0, new Uint8Array([51, 52, 53, 54, 55]), false),
    ]);
    return ipcImpossibleChunkLayoutConnection;
  },
});
let ipcImpossibleChunkLayoutThrown = false;
try {
  await ipcImpossibleChunkLayoutWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  ipcImpossibleChunkLayoutThrown = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("chunk layout") || !String(error).includes("total_size")) {
    throw new Error(`unexpected IPC impossible chunk layout error ${String(error)}`);
  }
}
if (!ipcImpossibleChunkLayoutThrown) {
  throw new Error("IPC transport accepted an impossible chunked response layout");
}
if ((ipcImpossibleChunkLayoutConnection?.readSizes.filter((size) => size === 16).length ?? 0) > 2) {
  throw new Error(`IPC transport attempted to read another frame after impossible chunked layout: ${ipcImpossibleChunkLayoutConnection?.readSizes.join(",")}`);
}

const ipcShortFinalChunkWire = createIpcEncodedTransport("ipc://chunked-short-final", {
  connect(): C2IpcConnection {
    return new FakeIpcConnection([
      ipcServerHandshakeFrame(),
      ipcReplyChunkFrame(1, 5, 2, 0, new Uint8Array([31, 32, 33]), false),
      ipcReplyChunkFrame(1, 5, 2, 1, new Uint8Array([34]), true),
    ]);
  },
});
let ipcShortFinalChunkThrown = false;
try {
  await ipcShortFinalChunkWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  ipcShortFinalChunkThrown = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("final chunk") || !String(error).includes("total_size")) {
    throw new Error(`unexpected IPC short final chunk error ${String(error)}`);
  }
}
if (!ipcShortFinalChunkThrown) {
  throw new Error("IPC transport accepted a chunked response whose final chunk did not complete total_size");
}

const ipcEmptyFinalChunkWire = createIpcEncodedTransport("ipc://chunked-empty-final", {
  connect(): C2IpcConnection {
    return new FakeIpcConnection([
      ipcServerHandshakeFrame(),
      ipcReplyChunkFrame(1, 5, 3, 0, new Uint8Array([41, 42]), false),
      ipcReplyChunkFrame(1, 5, 3, 1, new Uint8Array([43, 44]), false),
      ipcReplyChunkFrame(1, 5, 3, 2, new Uint8Array([]), true),
    ]);
  },
});
let ipcEmptyFinalChunkThrown = false;
try {
  await ipcEmptyFinalChunkWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  ipcEmptyFinalChunkThrown = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("final chunk") || !String(error).includes("empty")) {
    throw new Error(`unexpected IPC empty final chunk error ${String(error)}`);
  }
}
if (!ipcEmptyFinalChunkThrown) {
  throw new Error("IPC transport accepted an empty final chunked response frame");
}

const ipcMalformedChunkedWire = createIpcEncodedTransport("ipc://malformed-chunked", {
  connect(): C2IpcConnection {
    return new FakeIpcConnection([
      ipcServerHandshakeFrame(),
      ipcReplyChunkFrame(1, 4, 2, 0, new Uint8Array([35, 36]), false),
      ipcReplyChunkFrame(1, 5, 2, 1, new Uint8Array([37, 38]), true),
    ]);
  },
});
let ipcMalformedChunkedThrown = false;
try {
  await ipcMalformedChunkedWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  ipcMalformedChunkedThrown = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("metadata changed")) {
    throw new Error(`unexpected IPC malformed chunked response error ${String(error)}`);
  }
}
if (!ipcMalformedChunkedThrown) {
  throw new Error("IPC transport accepted inconsistent chunked response metadata");
}

let ipcBadAllocatedReleaseCount = 0;
const ipcBadAllocatedWire = createIpcEncodedTransport("ipc://bad-allocated", {
  connect(): C2IpcConnection {
    return new FakeIpcConnection([
      ipcServerHandshakeFrame(),
      ipcSuccessReplyFrame(1, new Uint8Array([7, 8, 9])),
    ]);
  },
  responsePayloadAllocator(_byteLength: number) {
    return {
      payload: { byteLength: 3, tag: "bad-ipc-allocated-response" },
      view: new Uint8Array(2),
      release(): void {
        ipcBadAllocatedReleaseCount += 1;
      },
    };
  },
});
let ipcBadAllocatedThrown = false;
try {
  await ipcBadAllocatedWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  ipcBadAllocatedThrown = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("responsePayloadAllocator")) {
    throw new Error(`unexpected IPC bad allocator error ${String(error)}`);
  }
}
if (!ipcBadAllocatedThrown) {
  throw new Error("IPC transport accepted an invalid allocated response");
}
if (ipcBadAllocatedReleaseCount !== 1) {
  throw new Error(`IPC transport did not release invalid allocated response, got ${ipcBadAllocatedReleaseCount}`);
}

const ipcCrmErrorWire = createIpcEncodedTransport("ipc://crm-error", {
  connect(): C2IpcConnection {
    return new FakeIpcConnection([
      ipcServerHandshakeFrame(),
      ipcErrorReplyFrame(1, new Uint8Array([51, 58, 120])),
    ]);
  },
});
let ipcCrmErrorThrown = false;
try {
  await ipcCrmErrorWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array());
} catch (error) {
  ipcCrmErrorThrown = true;
  if (!(error instanceof C2CrmMethodError) || error.payload[0] !== 51 || error.payload[2] !== 120) {
    throw new Error(`unexpected IPC CRM error ${String(error)}`);
  }
}
if (!ipcCrmErrorThrown) {
  throw new Error("IPC transport accepted a CRM error reply");
}

const ipcRouteMissingWire = createIpcEncodedTransport("ipc://route-missing", {
  connect(): C2IpcConnection {
    return new FakeIpcConnection([
      ipcServerHandshakeFrame(),
      ipcRouteNotFoundReplyFrame(1, "route"),
    ]);
  },
});
let ipcRouteMissingThrown = false;
try {
  await ipcRouteMissingWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array());
} catch (error) {
  ipcRouteMissingThrown = true;
  if (!(error instanceof C2IpcRouteNotFoundError) || error.routeName !== "route") {
    throw new Error(`unexpected IPC route-not-found error ${String(error)}`);
  }
}
if (!ipcRouteMissingThrown) {
  throw new Error("IPC transport accepted a route-not-found reply");
}

let ipcBuddyReaderSeen: C2IpcResponseShmBlock | undefined;
let ipcBuddyReleaseCount = 0;
let ipcBuddyReturnedView: Uint8Array | undefined;
const ipcBuddyWire = createIpcEncodedTransport("ipc://buddy", {
  connect(): C2IpcConnection {
    return new FakeIpcConnection([
      ipcServerHandshakeFrame({
        shmPrefix: "/cc2s0001",
        shmSegments: [{ name: "/cc2s0001_b0000", size: 4096 }],
      }),
      ipcBuddySuccessReplyFrame(1, 0, 32, 3),
    ]);
  },
  responseShmReader: {
    read(block: C2IpcResponseShmBlock): Uint8Array {
      ipcBuddyReaderSeen = block;
      ipcBuddyReturnedView = new Uint8Array([61, 62, 63]);
      return ipcBuddyReturnedView;
    },
    release(block: C2IpcResponseShmBlock): void {
      if (block.segmentName !== "/cc2s0001_b0000") {
        throw new Error(`unexpected IPC SHM block release ${block.segmentName}`);
      }
      ipcBuddyReleaseCount += 1;
    },
  },
});
const ipcBuddyResult = await ipcBuddyWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array());
if (ipcBuddyResult[0] !== 61 || ipcBuddyResult[1] !== 62 || ipcBuddyResult[2] !== 63) {
  throw new Error(`IPC transport did not return SHM reply bytes: ${Array.from(ipcBuddyResult).join(",")}`);
}
if (ipcBuddyResult === ipcBuddyReturnedView) {
  throw new Error("IPC transport returned the same no-allocator SHM reader view after release");
}
if (!ipcBuddyReaderSeen || ipcBuddyReaderSeen.prefix !== "/cc2s0001" || ipcBuddyReaderSeen.segmentName !== "/cc2s0001_b0000" || ipcBuddyReaderSeen.segmentIndex !== 0 || ipcBuddyReaderSeen.offset !== 32 || ipcBuddyReaderSeen.byteLength !== 3 || ipcBuddyReaderSeen.dedicated) {
  throw new Error(`IPC transport passed incorrect SHM block metadata: ${JSON.stringify(ipcBuddyReaderSeen)}`);
}
if (ipcBuddyReleaseCount !== 1) {
  throw new Error(`IPC transport did not release a successful SHM response: ${ipcBuddyReleaseCount}`);
}

const ipcBuddyAllocatedResponse = { byteLength: 4, tag: "ipc-buddy-allocated-response" };
const ipcBuddyAllocatedBytes = new Uint8Array(4);
let ipcBuddyAllocatedReleaseCount = 0;
let ipcBuddyAllocatedShmReleaseCount = 0;
const ipcBuddyAllocatedWire = createIpcEncodedTransport("ipc://buddy-allocated", {
  connect(): C2IpcConnection {
    return new FakeIpcConnection([
      ipcServerHandshakeFrame({
        shmPrefix: "/cc2s0002",
        shmSegments: [{ name: "/cc2s0002_b0000", size: 4096 }],
      }),
      ipcBuddySuccessReplyFrame(1, 0, 64, 4),
    ]);
  },
  responsePayloadAllocator(byteLength: number) {
    if (byteLength !== 4) {
      throw new Error(`unexpected IPC buddy allocated response length ${byteLength}`);
    }
    return {
      payload: ipcBuddyAllocatedResponse,
      view: ipcBuddyAllocatedBytes,
      release(): void {
        ipcBuddyAllocatedReleaseCount += 1;
      },
    };
  },
  responseShmReader: {
    read(block: C2IpcResponseShmBlock, destination?: Uint8Array): void {
      if (block.byteLength !== 4 || destination?.byteLength !== 4) {
        throw new Error("IPC buddy allocated SHM reader did not receive the allocator destination");
      }
      destination.set([71, 72, 73, 74]);
    },
    release(block: C2IpcResponseShmBlock): void {
      if (block.segmentName !== "/cc2s0002_b0000") {
        throw new Error(`unexpected allocated IPC SHM block release ${block.segmentName}`);
      }
      ipcBuddyAllocatedShmReleaseCount += 1;
    },
  },
});
const ipcBuddyAllocatedResult = await ipcBuddyAllocatedWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array());
if (ipcBuddyAllocatedResult !== ipcBuddyAllocatedResponse || ipcBuddyAllocatedBytes[0] !== 71 || ipcBuddyAllocatedBytes[3] !== 74) {
  throw new Error("IPC transport did not read SHM reply into the allocated payload");
}
if (ipcBuddyAllocatedReleaseCount !== 0) {
  throw new Error("IPC transport released a successful SHM allocated response");
}
if (ipcBuddyAllocatedShmReleaseCount !== 1) {
  throw new Error(`IPC transport did not release a successful allocated SHM response: ${ipcBuddyAllocatedShmReleaseCount}`);
}

let ipcDedicatedBuddyReaderSeen: C2IpcResponseShmBlock | undefined;
let ipcDedicatedBuddyReleaseCount = 0;
const ipcDedicatedBuddyWire = createIpcEncodedTransport("ipc://buddy-dedicated", {
  connect(): C2IpcConnection {
    return new FakeIpcConnection([
      ipcServerHandshakeFrame({
        shmPrefix: "/cc2s0005",
        shmSegments: [{ name: "/cc2s0005_b0000", size: 4 }],
      }),
      ipcBuddySuccessReplyFrame(1, 0, 0, 8, true),
    ]);
  },
  responseShmReader: {
    read(block: C2IpcResponseShmBlock): Uint8Array {
      ipcDedicatedBuddyReaderSeen = block;
      return new Uint8Array([81, 82, 83, 84, 85, 86, 87, 88]);
    },
    release(block: C2IpcResponseShmBlock): void {
      if (!block.dedicated) {
        throw new Error("dedicated SHM response release did not receive a dedicated block");
      }
      ipcDedicatedBuddyReleaseCount += 1;
    },
  },
});
const ipcDedicatedBuddyResult = await ipcDedicatedBuddyWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array());
if (ipcDedicatedBuddyResult[0] !== 81 || ipcDedicatedBuddyResult[7] !== 88) {
  throw new Error(`IPC transport did not return dedicated SHM reply bytes: ${Array.from(ipcDedicatedBuddyResult).join(",")}`);
}
if (!ipcDedicatedBuddyReaderSeen || !ipcDedicatedBuddyReaderSeen.dedicated || ipcDedicatedBuddyReaderSeen.segmentName !== "/cc2s0005_d0000" || ipcDedicatedBuddyReaderSeen.byteLength !== 8) {
  throw new Error(`IPC transport passed incorrect dedicated SHM block metadata: ${JSON.stringify(ipcDedicatedBuddyReaderSeen)}`);
}
if (ipcDedicatedBuddyReleaseCount !== 1) {
  throw new Error(`IPC transport did not release a successful dedicated SHM response: ${ipcDedicatedBuddyReleaseCount}`);
}

const ipcBuddyReleaseFailureResponse = { byteLength: 2, tag: "ipc-buddy-release-failure-response" };
let ipcBuddyReleaseFailureAllocatedReleaseCount = 0;
const ipcBuddyReleaseFailureWire = createIpcEncodedTransport("ipc://buddy-release-failure", {
  connect(): C2IpcConnection {
    return new FakeIpcConnection([
      ipcServerHandshakeFrame({
        shmPrefix: "/cc2s0006",
        shmSegments: [{ name: "/cc2s0006_b0000", size: 4096 }],
      }),
      ipcBuddySuccessReplyFrame(1, 0, 16, 2),
    ]);
  },
  responsePayloadAllocator(byteLength: number) {
    return {
      payload: ipcBuddyReleaseFailureResponse,
      view: new Uint8Array(byteLength),
      release(): void {
        ipcBuddyReleaseFailureAllocatedReleaseCount += 1;
      },
    };
  },
  responseShmReader: {
    read(_block: C2IpcResponseShmBlock, destination?: Uint8Array): void {
      destination?.set([91, 92]);
    },
    release(_block: C2IpcResponseShmBlock): void {
      throw new Error("release failed after read");
    },
  },
});
let ipcBuddyReleaseFailureThrown = false;
try {
  await ipcBuddyReleaseFailureWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array());
} catch (error) {
  ipcBuddyReleaseFailureThrown = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("responseShmReader release failed")) {
    throw new Error(`unexpected IPC SHM release failure error ${String(error)}`);
  }
}
if (!ipcBuddyReleaseFailureThrown || ipcBuddyReleaseFailureAllocatedReleaseCount !== 1) {
  throw new Error(`IPC transport did not release allocated payload after SHM release failure: thrown=${ipcBuddyReleaseFailureThrown} release=${ipcBuddyReleaseFailureAllocatedReleaseCount}`);
}

let ipcBadReaderShapeThrown = false;
try {
  createIpcEncodedTransport("ipc://bad-reader-shape", {
    connect(): C2IpcConnection {
      throw new Error("bad SHM reader shape should fail before connect");
    },
    responseShmReader: {
      read(_block: C2IpcResponseShmBlock): Uint8Array {
        return new Uint8Array();
      },
    } as unknown as C2IpcResponseShmReader,
  });
} catch (error) {
  ipcBadReaderShapeThrown = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("responseShmReader release must be a function")) {
    throw new Error(`unexpected IPC SHM reader shape error ${String(error)}`);
  }
}
if (!ipcBadReaderShapeThrown) {
  throw new Error("IPC transport accepted an SHM reader without release");
}

let ipcBuddyBadAllocatedReleaseCount = 0;
let ipcBuddyBadAllocatedReaderCalled = false;
const ipcBuddyBadAllocatedWire = createIpcEncodedTransport("ipc://buddy-bad-allocated", {
  connect(): C2IpcConnection {
    return new FakeIpcConnection([
      ipcServerHandshakeFrame({
        shmPrefix: "/cc2s0004",
        shmSegments: [{ name: "/cc2s0004_b0000", size: 4096 }],
      }),
      ipcBuddySuccessReplyFrame(1, 0, 64, 4),
    ]);
  },
  responsePayloadAllocator(_byteLength: number) {
    return {
      payload: { byteLength: 4, tag: "bad-ipc-buddy-allocated-response" },
      view: new Uint8Array(3),
      release(): void {
        ipcBuddyBadAllocatedReleaseCount += 1;
      },
    };
  },
  responseShmReader: {
    read(_block: C2IpcResponseShmBlock, _destination?: Uint8Array): void {
      ipcBuddyBadAllocatedReaderCalled = true;
    },
    release(_block: C2IpcResponseShmBlock): void {
      throw new Error("invalid allocator should fail before SHM release");
    },
  },
});
let ipcBuddyBadAllocatedThrown = false;
try {
  await ipcBuddyBadAllocatedWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array());
} catch (error) {
  ipcBuddyBadAllocatedThrown = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("responsePayloadAllocator")) {
    throw new Error(`unexpected IPC bad SHM allocator error ${String(error)}`);
  }
}
if (!ipcBuddyBadAllocatedThrown) {
  throw new Error("IPC transport accepted an invalid SHM allocated response");
}
if (ipcBuddyBadAllocatedReleaseCount !== 1 || ipcBuddyBadAllocatedReaderCalled) {
  throw new Error(`IPC transport did not stop and release invalid SHM allocator before reader: release=${ipcBuddyBadAllocatedReleaseCount} reader=${ipcBuddyBadAllocatedReaderCalled}`);
}

let ipcBuddyReaderFailureReleaseCount = 0;
let ipcBuddyReaderFailureShmReleaseCount = 0;
const ipcBuddyReaderFailureWire = createIpcEncodedTransport("ipc://buddy-reader-failure", {
  connect(): C2IpcConnection {
    return new FakeIpcConnection([
      ipcServerHandshakeFrame({
        shmPrefix: "/cc2s0006",
        shmSegments: [{ name: "/cc2s0006_b0000", size: 4096 }],
      }),
      ipcBuddySuccessReplyFrame(1, 0, 64, 4),
    ]);
  },
  responsePayloadAllocator(_byteLength: number) {
    return {
      payload: { byteLength: 4, tag: "ipc-buddy-reader-failure-response" },
      view: new Uint8Array(4),
      release(): void {
        ipcBuddyReaderFailureReleaseCount += 1;
      },
    };
  },
  responseShmReader: {
    read(_block: C2IpcResponseShmBlock, _destination?: Uint8Array): void {
      throw new Error("reader failed after allocation");
    },
    release(_block: C2IpcResponseShmBlock): void {
      ipcBuddyReaderFailureShmReleaseCount += 1;
    },
  },
});
let ipcBuddyReaderFailureThrown = false;
try {
  await ipcBuddyReaderFailureWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array());
} catch (error) {
  ipcBuddyReaderFailureThrown = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("responseShmReader") || !String(error).includes("reader failed after allocation")) {
    throw new Error(`unexpected IPC SHM reader failure error ${String(error)}`);
  }
}
if (!ipcBuddyReaderFailureThrown || ipcBuddyReaderFailureReleaseCount !== 1) {
  throw new Error(`IPC transport did not release allocated SHM response after reader failure: thrown=${ipcBuddyReaderFailureThrown} release=${ipcBuddyReaderFailureReleaseCount}`);
}
if (ipcBuddyReaderFailureShmReleaseCount !== 0) {
  throw new Error(`IPC transport released an SHM block after a failed read: ${ipcBuddyReaderFailureShmReleaseCount}`);
}

let ipcBuddyWrongLengthReleaseCount = 0;
const ipcBuddyWrongLengthWire = createIpcEncodedTransport("ipc://buddy-wrong-length", {
  connect(): C2IpcConnection {
    return new FakeIpcConnection([
      ipcServerHandshakeFrame({
        shmPrefix: "/cc2s0007",
        shmSegments: [{ name: "/cc2s0007_b0000", size: 4096 }],
      }),
      ipcBuddySuccessReplyFrame(1, 0, 64, 4),
    ]);
  },
  responseShmReader: {
    read(_block: C2IpcResponseShmBlock): Uint8Array {
      return new Uint8Array([1, 2, 3]);
    },
    release(_block: C2IpcResponseShmBlock): void {
      ipcBuddyWrongLengthReleaseCount += 1;
    },
  },
});
let ipcBuddyWrongLengthThrown = false;
try {
  await ipcBuddyWrongLengthWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array());
} catch (error) {
  ipcBuddyWrongLengthThrown = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("returned 3 bytes") || !String(error).includes("expected 4")) {
    throw new Error(`unexpected IPC SHM wrong-length reader error ${String(error)}`);
  }
}
if (!ipcBuddyWrongLengthThrown) {
  throw new Error("IPC transport accepted a wrong-length SHM reader result");
}
if (ipcBuddyWrongLengthReleaseCount !== 1) {
  throw new Error(`IPC transport did not release an SHM block after a wrong-length reader result: ${ipcBuddyWrongLengthReleaseCount}`);
}

const ipcMissingShmReaderWire = createIpcEncodedTransport("ipc://buddy-missing-reader", {
  connect(): C2IpcConnection {
    return new FakeIpcConnection([
      ipcServerHandshakeFrame({
        shmPrefix: "/cc2s0003",
        shmSegments: [{ name: "/cc2s0003_b0000", size: 4096 }],
      }),
      ipcBuddySuccessReplyFrame(1, 0, 32, 3),
    ]);
  },
});
let ipcBuddyThrown = false;
try {
  await ipcMissingShmReaderWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array());
} catch (error) {
  ipcBuddyThrown = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("responseShmReader")) {
    throw new Error(`unexpected IPC missing SHM reader error ${String(error)}`);
  }
}
if (!ipcBuddyThrown) {
  throw new Error("IPC transport accepted an SHM response without a reader");
}

const ipcMalformedBuddyWire = createIpcEncodedTransport("ipc://buddy-malformed", {
  connect(): C2IpcConnection {
    return new FakeIpcConnection([
      ipcServerHandshakeFrame(),
      ipcSuccessReplyFrame(1, new Uint8Array(), C2_IPC_SMOKE_FLAG_BUDDY),
    ]);
  },
  responseShmReader: {
    read(_block: C2IpcResponseShmBlock): Uint8Array {
      throw new Error("malformed buddy response must fail before reader");
    },
    release(_block: C2IpcResponseShmBlock): void {
      throw new Error("malformed buddy response must fail before release");
    },
  },
});
let ipcMalformedBuddyThrown = false;
try {
  await ipcMalformedBuddyWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array());
} catch (error) {
  ipcMalformedBuddyThrown = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("buddy payload") || !String(error).includes("truncated")) {
    throw new Error(`unexpected IPC malformed buddy response error ${String(error)}`);
  }
}
if (!ipcMalformedBuddyThrown) {
  throw new Error("IPC transport accepted a truncated SHM buddy response");
}

let ipcOutOfRangeBuddyReaderCalled = false;
const ipcOutOfRangeBuddyWire = createIpcEncodedTransport("ipc://buddy-out-of-range", {
  connect(): C2IpcConnection {
    return new FakeIpcConnection([
      ipcServerHandshakeFrame({
        shmPrefix: "/cc2s0005",
        shmSegments: [{ name: "/cc2s0005_b0000", size: 64 }],
      }),
      ipcBuddySuccessReplyFrame(1, 0, 63, 2),
    ]);
  },
  responseShmReader: {
    read(_block: C2IpcResponseShmBlock): Uint8Array {
      ipcOutOfRangeBuddyReaderCalled = true;
      return new Uint8Array([1, 2]);
    },
    release(_block: C2IpcResponseShmBlock): void {
      throw new Error("out-of-range response should fail before release");
    },
  },
});
let ipcOutOfRangeBuddyThrown = false;
try {
  await ipcOutOfRangeBuddyWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array());
} catch (error) {
  ipcOutOfRangeBuddyThrown = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("SHM response range") || !String(error).includes("exceeds segment")) {
    throw new Error(`unexpected IPC out-of-range SHM response error ${String(error)}`);
  }
}
if (!ipcOutOfRangeBuddyThrown || ipcOutOfRangeBuddyReaderCalled) {
  throw new Error(`IPC transport accepted an out-of-range SHM response: thrown=${ipcOutOfRangeBuddyThrown} reader=${ipcOutOfRangeBuddyReaderCalled}`);
}

let badIpcAddressThrown = false;
try {
  createIpcEncodedTransport("ipc://bad/name", {
    connect(): C2IpcConnection {
      throw new Error("invalid address should not connect");
    },
  });
} catch (error) {
  badIpcAddressThrown = true;
  if (!(error instanceof C2IpcTransportError)) {
    throw new Error(`unexpected invalid IPC address error ${String(error)}`);
  }
}
if (!badIpcAddressThrown) {
  throw new Error("IPC transport accepted an invalid region address");
}

let ipcContractMismatchConnection: FakeIpcConnection | undefined;
const ipcContractMismatchWire = createIpcEncodedTransport("ipc://contract-mismatch", {
  connect(): C2IpcConnection {
    ipcContractMismatchConnection = new FakeIpcConnection([
      ipcServerHandshakeFrame({
        abiHash: "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
      }),
    ]);
    return ipcContractMismatchConnection;
  },
});
let ipcContractMismatchThrown = false;
try {
  await ipcContractMismatchWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array());
} catch (error) {
  ipcContractMismatchThrown = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("expected CRM contract")) {
    throw new Error(`unexpected IPC contract mismatch error ${String(error)}`);
  }
}
if (!ipcContractMismatchThrown || ipcContractMismatchConnection?.writes.length !== 1) {
  throw new Error("IPC transport did not stop after handshake contract mismatch");
}

let ipcPayloadTooLargeConnection: FakeIpcConnection | undefined;
const ipcPayloadTooLargeWire = createIpcEncodedTransport("ipc://payload-too-large", {
  connect(): C2IpcConnection {
    ipcPayloadTooLargeConnection = new FakeIpcConnection([
      ipcServerHandshakeFrame({ maxPayloadSize: 2 }),
    ]);
    return ipcPayloadTooLargeConnection;
  },
});
let ipcPayloadTooLargeThrown = false;
try {
  await ipcPayloadTooLargeWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1, 2, 3]));
} catch (error) {
  ipcPayloadTooLargeThrown = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("max_payload_size")) {
    throw new Error(`unexpected IPC payload-too-large error ${String(error)}`);
  }
}
if (!ipcPayloadTooLargeThrown || ipcPayloadTooLargeConnection?.writes.length !== 1) {
  throw new Error("IPC transport did not stop oversized payloads before sending a call frame");
}

let ipcMissingMethodConnection: FakeIpcConnection | undefined;
const ipcMissingMethodWire = createIpcEncodedTransport("ipc://missing-method", {
  connect(): C2IpcConnection {
    ipcMissingMethodConnection = new FakeIpcConnection([
      ipcServerHandshakeFrame(),
    ]);
    return ipcMissingMethodConnection;
  },
});
let ipcMissingMethodThrown = false;
try {
  await ipcMissingMethodWire.call("route", FASTDB_PORTABLE_CONTRACT, "missing", new Uint8Array());
} catch (error) {
  ipcMissingMethodThrown = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("does not advertise method")) {
    throw new Error(`unexpected IPC missing-method error ${String(error)}`);
  }
}
if (!ipcMissingMethodThrown || ipcMissingMethodConnection?.writes.length !== 1) {
  throw new Error("IPC transport did not stop missing methods before sending a call frame");
}

let ipcOversizedResponseFrameConnection: FakeIpcConnection | undefined;
const ipcOversizedResponseFrameWire = createIpcEncodedTransport("ipc://oversized-response-frame", {
  connect(): C2IpcConnection {
    ipcOversizedResponseFrameConnection = new FakeIpcConnection([
      ipcServerHandshakeFrame(),
      ipcFrameHeader(1, C2_IPC_SMOKE_FLAG_RESPONSE | C2_IPC_SMOKE_FLAG_REPLY_V2, 2147483660),
    ]);
    return ipcOversizedResponseFrameConnection;
  },
});
let ipcOversizedResponseFrameThrown = false;
try {
  await ipcOversizedResponseFrameWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array());
} catch (error) {
  ipcOversizedResponseFrameThrown = true;
  if (!(error instanceof C2IpcTransportError) || !String(error).includes("frame payload length") || !String(error).includes("no greater than")) {
    throw new Error(`unexpected IPC oversized frame payload error ${String(error)}`);
  }
}
if (!ipcOversizedResponseFrameThrown) {
  throw new Error("IPC transport accepted an oversized response frame payload length");
}
if (ipcOversizedResponseFrameConnection?.readSizes.some((size) => size > 2_147_483_647)) {
  throw new Error(`IPC transport attempted to read an oversized response payload: ${ipcOversizedResponseFrameConnection.readSizes.join(",")}`);
}

const allocatedResponse = { byteLength: 3, tag: "allocated-response" };
const allocatedBytes = new Uint8Array(3);
let allocatedReleaseCount = 0;
const streamingFetch: C2Fetch = async (_input, _init) => {
  const chunks = [new Uint8Array([4, 5]), new Uint8Array([6])];
  let index = 0;
  return {
    status: 200,
    headers: {
      get(name: string): string | null {
        return name.toLowerCase() === "content-length" ? "3" : null;
      },
    },
    body: {
      getReader() {
        return {
          async read(): Promise<{ done: boolean; value?: SmokeByteArray }> {
            const value = chunks[index++];
            if (value === undefined) {
              return { done: true };
            }
            return { done: false, value };
          },
          releaseLock(): void {},
        };
      },
    },
    async arrayBuffer(): Promise<ArrayBufferLike> {
      throw new Error("streaming response allocator should not fall back to arrayBuffer");
    },
    async text(): Promise<string> {
      return "";
    },
  };
};
const allocatedWire = createHttpRelayEncodedTransport("http://relay.example/base/", {
  fetch: streamingFetch,
  responsePayloadAllocator(byteLength: number) {
    if (byteLength !== 3) {
      throw new Error(`unexpected allocated response length ${byteLength}`);
    }
    return {
      payload: allocatedResponse,
      view: allocatedBytes,
      release(): void {
        allocatedReleaseCount += 1;
      },
    };
  },
});
const allocatedResult = await allocatedWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
if (allocatedResult !== allocatedResponse) {
  throw new Error("HTTP relay transport did not return provider-owned allocated response");
}
if (allocatedBytes[0] !== 4 || allocatedBytes[1] !== 5 || allocatedBytes[2] !== 6) {
  throw new Error(`HTTP relay transport did not stream into allocated response bytes: ${Array.from(allocatedBytes).join(",")}`);
}
if (allocatedReleaseCount !== 0) {
  throw new Error("HTTP relay transport released a successful allocated response");
}

const missingLengthFetch: C2Fetch = async (_input, _init) => {
  return {
    status: 200,
    headers: {
      get(_name: string): string | null {
        return null;
      },
    },
    body: {
      getReader() {
        throw new Error("response allocator should not read a stream without Content-Length");
      },
    },
    async arrayBuffer(): Promise<ArrayBufferLike> {
      throw new Error("response allocator must not fall back to arrayBuffer without Content-Length");
    },
    async text(): Promise<string> {
      return "";
    },
  };
};
let missingLengthThrown = false;
try {
  await createHttpRelayEncodedTransport("http://relay.example/base/", {
    fetch: missingLengthFetch,
    responsePayloadAllocator(_byteLength: number) {
      throw new Error("allocator should not be called without Content-Length");
    },
  }).call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  missingLengthThrown = true;
  if (!(error instanceof C2HttpTransportError) || !String(error).includes("responsePayloadAllocator") || !String(error).includes("Content-Length")) {
    throw new Error(`unexpected missing Content-Length allocator error ${String(error)}`);
  }
}
if (!missingLengthThrown) {
  throw new Error("HTTP relay response allocator accepted a response without Content-Length");
}

let oversizedNoAllocatorArrayBufferCalls = 0;
const oversizedNoAllocatorFetch: C2Fetch = async (_input, _init) => {
  return {
    status: 200,
    headers: {
      get(name: string): string | null {
        return name.toLowerCase() === "content-length" ? "2147483648" : null;
      },
    },
    body: null,
    async arrayBuffer(): Promise<ArrayBufferLike> {
      oversizedNoAllocatorArrayBufferCalls += 1;
      throw new Error("oversized Content-Length response should fail before arrayBuffer");
    },
    async text(): Promise<string> {
      return "";
    },
  };
};
let oversizedNoAllocatorThrown = false;
try {
  await createHttpRelayEncodedTransport("http://relay.example/base/", {
    fetch: oversizedNoAllocatorFetch,
  }).call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  oversizedNoAllocatorThrown = true;
  if (!(error instanceof C2HttpTransportError) || !String(error).includes("Content-Length") || !String(error).includes("no greater than")) {
    throw new Error(`unexpected oversized no-allocator Content-Length error ${String(error)}`);
  }
}
if (!oversizedNoAllocatorThrown) {
  throw new Error("HTTP relay transport accepted an oversized no-allocator Content-Length");
}
if (oversizedNoAllocatorArrayBufferCalls !== 0) {
  throw new Error(`HTTP relay response arrayBuffer ran after oversized no-allocator Content-Length, got ${oversizedNoAllocatorArrayBufferCalls}`);
}

let oversizedLengthAllocatorCalls = 0;
let oversizedLengthBodyReadCount = 0;
const oversizedLengthFetch: C2Fetch = async (_input, _init) => {
  return {
    status: 200,
    headers: {
      get(name: string): string | null {
        return name.toLowerCase() === "content-length" ? "2147483648" : null;
      },
    },
    body: {
      getReader() {
        oversizedLengthBodyReadCount += 1;
        throw new Error("oversized Content-Length response should fail before reading body");
      },
    },
    async arrayBuffer(): Promise<ArrayBufferLike> {
      throw new Error("oversized Content-Length response allocator should not fall back to arrayBuffer");
    },
    async text(): Promise<string> {
      return "";
    },
  };
};
let oversizedLengthThrown = false;
try {
  await createHttpRelayEncodedTransport("http://relay.example/base/", {
    fetch: oversizedLengthFetch,
    responsePayloadAllocator(_byteLength: number) {
      oversizedLengthAllocatorCalls += 1;
      return {
        payload: { byteLength: 0, tag: "oversized-content-length" },
        view: new Uint8Array(0),
      };
    },
  }).call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  oversizedLengthThrown = true;
  if (!(error instanceof C2HttpTransportError) || !String(error).includes("Content-Length") || !String(error).includes("no greater than")) {
    throw new Error(`unexpected oversized Content-Length allocator error ${String(error)}`);
  }
}
if (!oversizedLengthThrown) {
  throw new Error("HTTP relay response allocator accepted an oversized Content-Length");
}
if (oversizedLengthAllocatorCalls !== 0) {
  throw new Error(`HTTP relay response allocator ran after oversized Content-Length, got ${oversizedLengthAllocatorCalls}`);
}
if (oversizedLengthBodyReadCount !== 0) {
  throw new Error(`HTTP relay response body was read after oversized Content-Length, got ${oversizedLengthBodyReadCount}`);
}

let oversizedCrmErrorArrayBufferCalls = 0;
const oversizedCrmErrorFetch: C2Fetch = async (_input, _init) => {
  return {
    status: 500,
    headers: {
      get(name: string): string | null {
        return name.toLowerCase() === "content-length" ? "2147483648" : null;
      },
    },
    body: null,
    async arrayBuffer(): Promise<ArrayBufferLike> {
      oversizedCrmErrorArrayBufferCalls += 1;
      throw new Error("oversized CRM error response should fail before arrayBuffer");
    },
    async text(): Promise<string> {
      return "";
    },
  };
};
let oversizedCrmErrorThrown = false;
try {
  await createHttpRelayEncodedTransport("http://relay.example/base/", {
    fetch: oversizedCrmErrorFetch,
  }).call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  oversizedCrmErrorThrown = true;
  if (!(error instanceof C2HttpTransportError) || !String(error).includes("Content-Length") || !String(error).includes("no greater than")) {
    throw new Error(`unexpected oversized CRM error Content-Length error ${String(error)}`);
  }
}
if (!oversizedCrmErrorThrown) {
  throw new Error("HTTP relay transport accepted an oversized CRM error Content-Length");
}
if (oversizedCrmErrorArrayBufferCalls !== 0) {
  throw new Error(`HTTP relay CRM error arrayBuffer ran after oversized Content-Length, got ${oversizedCrmErrorArrayBufferCalls}`);
}

const mismatchedNoAllocatorFetch: C2Fetch = async (_input, _init) => {
  return {
    status: 200,
    headers: {
      get(name: string): string | null {
        return name.toLowerCase() === "content-length" ? "3" : null;
      },
    },
    body: null,
    async arrayBuffer(): Promise<ArrayBufferLike> {
      return new Uint8Array([1, 2]).buffer;
    },
    async text(): Promise<string> {
      return "";
    },
  };
};
let mismatchedNoAllocatorThrown = false;
try {
  await createHttpRelayEncodedTransport("http://relay.example/base/", {
    fetch: mismatchedNoAllocatorFetch,
  }).call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  mismatchedNoAllocatorThrown = true;
  if (!(error instanceof C2HttpTransportError) || !String(error).includes("does not match Content-Length")) {
    throw new Error(`unexpected mismatched no-allocator Content-Length error ${String(error)}`);
  }
}
if (!mismatchedNoAllocatorThrown) {
  throw new Error("HTTP relay transport accepted a no-allocator response shorter than Content-Length");
}

const mismatchedCrmErrorFetch: C2Fetch = async (_input, _init) => {
  return {
    status: 500,
    headers: {
      get(name: string): string | null {
        return name.toLowerCase() === "content-length" ? "3" : null;
      },
    },
    body: null,
    async arrayBuffer(): Promise<ArrayBufferLike> {
      return new Uint8Array([9, 8]).buffer;
    },
    async text(): Promise<string> {
      return "";
    },
  };
};
let mismatchedCrmErrorThrown = false;
try {
  await createHttpRelayEncodedTransport("http://relay.example/base/", {
    fetch: mismatchedCrmErrorFetch,
  }).call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  mismatchedCrmErrorThrown = true;
  if (!(error instanceof C2HttpTransportError) || !String(error).includes("does not match Content-Length")) {
    throw new Error(`unexpected mismatched CRM error Content-Length error ${String(error)}`);
  }
}
if (!mismatchedCrmErrorThrown) {
  throw new Error("HTTP relay transport accepted a CRM error response shorter than Content-Length");
}

const unknownLengthAllocatedResponse = { byteLength: 3, tag: "unknown-length-allocated-response" };
const unknownLengthAllocatedBytes = new Uint8Array(3);
let unknownLengthAllocatedReleaseCount = 0;
const unknownLengthFetch: C2Fetch = async (_input, _init) => {
  const chunks = [new Uint8Array([7]), new Uint8Array([8, 9])];
  let index = 0;
  return {
    status: 200,
    headers: {
      get(_name: string): string | null {
        return null;
      },
    },
    body: {
      getReader() {
        return {
          async read(): Promise<{ done: boolean; value?: SmokeByteArray }> {
            const value = chunks[index++];
            if (value === undefined) {
              return { done: true };
            }
            return { done: false, value };
          },
          releaseLock(): void {},
        };
      },
    },
    async arrayBuffer(): Promise<ArrayBufferLike> {
      throw new Error("unknown-length response allocator should not fall back to arrayBuffer");
    },
    async text(): Promise<string> {
      return "";
    },
  };
};
const unknownLengthWire = createHttpRelayEncodedTransport("http://relay.example/base/", {
  fetch: unknownLengthFetch,
  responsePayloadUnknownLengthStrategy: "buffer",
  responsePayloadUnknownLengthMaxBytes: 3,
  responsePayloadAllocator(byteLength: number) {
    if (byteLength !== 3) {
      throw new Error(`unexpected unknown-length allocated response length ${byteLength}`);
    }
    return {
      payload: unknownLengthAllocatedResponse,
      view: unknownLengthAllocatedBytes,
      release(): void {
        unknownLengthAllocatedReleaseCount += 1;
      },
    };
  },
});
const unknownLengthResult = await unknownLengthWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
if (unknownLengthResult !== unknownLengthAllocatedResponse) {
  throw new Error("HTTP relay transport did not return provider-owned unknown-length allocated response");
}
if (unknownLengthAllocatedBytes[0] !== 7 || unknownLengthAllocatedBytes[1] !== 8 || unknownLengthAllocatedBytes[2] !== 9) {
  throw new Error(`HTTP relay transport did not buffer unknown-length response bytes into allocated payload: ${Array.from(unknownLengthAllocatedBytes).join(",")}`);
}
if (unknownLengthAllocatedReleaseCount !== 0) {
  throw new Error("HTTP relay transport released a successful unknown-length allocated response");
}

const reusedUnknownLengthAllocatedResponse = { byteLength: 4, tag: "reused-unknown-length-allocated-response" };
const reusedUnknownLengthAllocatedBytes = new Uint8Array(4);
const reusedUnknownLengthChunk = new Uint8Array(2);
let reusedUnknownLengthReadCount = 0;
const reusedUnknownLengthFetch: C2Fetch = async (_input, _init) => {
  return {
    status: 200,
    headers: {
      get(_name: string): string | null {
        return null;
      },
    },
    body: {
      getReader() {
        return {
          async read(): Promise<{ done: boolean; value?: SmokeByteArray }> {
            reusedUnknownLengthReadCount += 1;
            if (reusedUnknownLengthReadCount === 1) {
              reusedUnknownLengthChunk[0] = 10;
              reusedUnknownLengthChunk[1] = 11;
              return { done: false, value: reusedUnknownLengthChunk };
            }
            if (reusedUnknownLengthReadCount === 2) {
              reusedUnknownLengthChunk[0] = 12;
              reusedUnknownLengthChunk[1] = 13;
              return { done: false, value: reusedUnknownLengthChunk };
            }
            return { done: true };
          },
          releaseLock(): void {},
        };
      },
    },
    async arrayBuffer(): Promise<ArrayBufferLike> {
      throw new Error("reused unknown-length response allocator should not fall back to arrayBuffer");
    },
    async text(): Promise<string> {
      return "";
    },
  };
};
const reusedUnknownLengthWire = createHttpRelayEncodedTransport("http://relay.example/base/", {
  fetch: reusedUnknownLengthFetch,
  responsePayloadUnknownLengthStrategy: "buffer",
  responsePayloadUnknownLengthMaxBytes: 4,
  responsePayloadAllocator(byteLength: number) {
    if (byteLength !== 4) {
      throw new Error(`unexpected reused unknown-length allocated response length ${byteLength}`);
    }
    return {
      payload: reusedUnknownLengthAllocatedResponse,
      view: reusedUnknownLengthAllocatedBytes,
    };
  },
});
const reusedUnknownLengthResult = await reusedUnknownLengthWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
if (reusedUnknownLengthResult !== reusedUnknownLengthAllocatedResponse) {
  throw new Error("HTTP relay transport did not return provider-owned reused unknown-length allocated response");
}
if (reusedUnknownLengthAllocatedBytes[0] !== 10 || reusedUnknownLengthAllocatedBytes[1] !== 11 || reusedUnknownLengthAllocatedBytes[2] !== 12 || reusedUnknownLengthAllocatedBytes[3] !== 13) {
  throw new Error(`HTTP relay transport did not snapshot reused unknown-length response chunks: ${Array.from(reusedUnknownLengthAllocatedBytes).join(",")}`);
}

let unknownLengthOverLimitAllocatorCalls = 0;
let unknownLengthOverLimitReleaseLockCount = 0;
const unknownLengthOverLimitFetch: C2Fetch = async (_input, _init) => {
  const chunks = [new Uint8Array([1, 2]), new Uint8Array([3])];
  let index = 0;
  return {
    status: 200,
    headers: {
      get(_name: string): string | null {
        return null;
      },
    },
    body: {
      getReader() {
        return {
          async read(): Promise<{ done: boolean; value?: SmokeByteArray }> {
            const value = chunks[index++];
            if (value === undefined) {
              return { done: true };
            }
            return { done: false, value };
          },
          releaseLock(): void {
            unknownLengthOverLimitReleaseLockCount += 1;
          },
        };
      },
    },
    async arrayBuffer(): Promise<ArrayBufferLike> {
      throw new Error("over-limit unknown-length allocator should not fall back to arrayBuffer");
    },
    async text(): Promise<string> {
      return "";
    },
  };
};
let unknownLengthOverLimitThrown = false;
try {
  await createHttpRelayEncodedTransport("http://relay.example/base/", {
    fetch: unknownLengthOverLimitFetch,
    responsePayloadUnknownLengthStrategy: "buffer",
    responsePayloadUnknownLengthMaxBytes: 2,
    responsePayloadAllocator(_byteLength: number) {
      unknownLengthOverLimitAllocatorCalls += 1;
      return {
        payload: { byteLength: 0, tag: "over-limit" },
        view: new Uint8Array(0),
      };
    },
  }).call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  unknownLengthOverLimitThrown = true;
  if (!(error instanceof C2HttpTransportError) || !String(error).includes("responsePayloadUnknownLengthMaxBytes")) {
    throw new Error(`unexpected unknown-length max bytes error ${String(error)}`);
  }
}
if (!unknownLengthOverLimitThrown) {
  throw new Error("HTTP relay transport accepted an over-limit unknown-length allocated response");
}
if (unknownLengthOverLimitAllocatorCalls !== 0) {
  throw new Error(`HTTP relay response allocator ran after unknown-length max was exceeded, got ${unknownLengthOverLimitAllocatorCalls}`);
}
if (unknownLengthOverLimitReleaseLockCount !== 1) {
  throw new Error(`HTTP relay response stream lock was not released after unknown-length max failure, got ${unknownLengthOverLimitReleaseLockCount}`);
}

let invalidUnknownLengthStrategyThrown = false;
try {
  createHttpRelayEncodedTransport("http://relay.example/base/", {
    fetch: fetchImpl,
    responsePayloadUnknownLengthStrategy: "copy" as never,
  });
} catch (error) {
  invalidUnknownLengthStrategyThrown = true;
  if (!String(error).includes("responsePayloadUnknownLengthStrategy")) {
    throw new Error(`unexpected invalid unknown-length strategy error ${String(error)}`);
  }
}
if (!invalidUnknownLengthStrategyThrown) {
  throw new Error("HTTP relay transport accepted an invalid responsePayloadUnknownLengthStrategy");
}

let invalidUnknownLengthMaxBytesThrown = false;
try {
  createHttpRelayEncodedTransport("http://relay.example/base/", {
    fetch: fetchImpl,
    responsePayloadUnknownLengthStrategy: "buffer",
    responsePayloadUnknownLengthMaxBytes: -1,
  });
} catch (error) {
  invalidUnknownLengthMaxBytesThrown = true;
  if (!String(error).includes("responsePayloadUnknownLengthMaxBytes")) {
    throw new Error(`unexpected invalid unknown-length max bytes error ${String(error)}`);
  }
}
if (!invalidUnknownLengthMaxBytesThrown) {
  throw new Error("HTTP relay transport accepted an invalid responsePayloadUnknownLengthMaxBytes");
}

let missingBodyAllocatorCalls = 0;
const missingBodyFetch: C2Fetch = async (_input, _init) => {
  return {
    status: 200,
    headers: {
      get(name: string): string | null {
        return name.toLowerCase() === "content-length" ? "1" : null;
      },
    },
    body: null,
    async arrayBuffer(): Promise<ArrayBufferLike> {
      throw new Error("response allocator must not fall back to arrayBuffer without a body stream");
    },
    async text(): Promise<string> {
      return "";
    },
  };
};
let missingBodyThrown = false;
try {
  await createHttpRelayEncodedTransport("http://relay.example/base/", {
    fetch: missingBodyFetch,
    responsePayloadAllocator(_byteLength: number) {
      missingBodyAllocatorCalls += 1;
      return {
        payload: { byteLength: 1, tag: "missing-body" },
        view: new Uint8Array(1),
      };
    },
  }).call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  missingBodyThrown = true;
  if (!(error instanceof C2HttpTransportError) || !String(error).includes("body stream is required")) {
    throw new Error(`unexpected missing body allocator error ${String(error)}`);
  }
}
if (!missingBodyThrown) {
  throw new Error("HTTP relay response allocator accepted a response without a body stream");
}
if (missingBodyAllocatorCalls !== 0) {
  throw new Error(`HTTP relay response allocator allocated without a body stream, got ${missingBodyAllocatorCalls}`);
}

let readerFailureReleaseCount = 0;
const readerFailureFetch: C2Fetch = async (_input, _init) => {
  return {
    status: 200,
    headers: {
      get(name: string): string | null {
        return name.toLowerCase() === "content-length" ? "2" : null;
      },
    },
    body: {
      getReader() {
        throw new Error("getReader failed");
      },
    },
    async arrayBuffer(): Promise<ArrayBufferLike> {
      throw new Error("response allocator must not fall back to arrayBuffer after reader failure");
    },
    async text(): Promise<string> {
      return "";
    },
  };
};
let readerFailureThrown = false;
try {
  await createHttpRelayEncodedTransport("http://relay.example/base/", {
    fetch: readerFailureFetch,
    responsePayloadAllocator(byteLength: number) {
      return {
        payload: { byteLength, tag: "reader-failure-allocation" },
        view: new Uint8Array(byteLength),
        release(): void {
          readerFailureReleaseCount += 1;
        },
      };
    },
  }).call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  readerFailureThrown = true;
  if (!(error instanceof C2HttpTransportError) || !String(error).includes("getReader failed")) {
    throw new Error(`unexpected reader failure allocator error ${String(error)}`);
  }
}
if (!readerFailureThrown) {
  throw new Error("HTTP relay response allocator accepted a reader failure");
}
if (readerFailureReleaseCount !== 1) {
  throw new Error(`HTTP relay response allocator did not release after reader failure, got ${readerFailureReleaseCount}`);
}

let readerFailureThrowingReleaseThrown = false;
try {
  await createHttpRelayEncodedTransport("http://relay.example/base/", {
    fetch: readerFailureFetch,
    responsePayloadAllocator(byteLength: number) {
      return {
        payload: { byteLength, tag: "reader-failure-throwing-release" },
        view: new Uint8Array(byteLength),
        release(): void {
          throw new Error("allocator cleanup failed");
        },
      };
    },
  }).call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  readerFailureThrowingReleaseThrown = true;
  const message = String(error);
  if (!(error instanceof C2HttpTransportError) || !message.includes("getReader failed") || message.includes("allocator cleanup failed")) {
    throw new Error(`allocator cleanup failure masked original reader failure: ${message}`);
  }
}
if (!readerFailureThrowingReleaseThrown) {
  throw new Error("HTTP relay response allocator accepted reader failure with throwing release");
}

let readerFailurePayloadReleaseCount = 0;
const readerFailurePayload = {
  byteLength: 2,
  release(): void {
    readerFailurePayloadReleaseCount += 1;
  },
};
try {
  await createHttpRelayEncodedTransport("http://relay.example/base/", {
    fetch: readerFailureFetch,
    responsePayloadAllocator(byteLength: number) {
      return {
        payload: readerFailurePayload,
        view: new Uint8Array(byteLength),
      };
    },
  }).call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  if (!(error instanceof C2HttpTransportError) || !String(error).includes("getReader failed")) {
    throw new Error(`unexpected reader payload-release allocator error ${String(error)}`);
  }
}
if (readerFailurePayloadReleaseCount !== 1) {
  throw new Error(`HTTP relay response allocator did not release provider-owned payload after reader failure, got ${readerFailurePayloadReleaseCount}`);
}

let malformedAllocationReleaseCount = 0;
const malformedAllocationFetch: C2Fetch = async (_input, _init) => {
  return {
    status: 200,
    headers: {
      get(name: string): string | null {
        return name.toLowerCase() === "content-length" ? "2" : null;
      },
    },
    body: {
      getReader() {
        throw new Error("malformed allocation should fail before reading stream");
      },
    },
    async arrayBuffer(): Promise<ArrayBufferLike> {
      throw new Error("response allocator must not fall back to arrayBuffer after malformed allocation");
    },
    async text(): Promise<string> {
      return "";
    },
  };
};
let malformedAllocationThrown = false;
try {
  await createHttpRelayEncodedTransport("http://relay.example/base/", {
    fetch: malformedAllocationFetch,
    responsePayloadAllocator(byteLength: number) {
      return {
        payload: { byteLength, tag: "malformed-allocation" },
        view: new Uint8Array(byteLength + 1),
        release(): void {
          malformedAllocationReleaseCount += 1;
        },
      };
    },
  }).call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  malformedAllocationThrown = true;
  if (!(error instanceof C2HttpTransportError) || !String(error).includes("view length must match expected response byte length")) {
    throw new Error(`unexpected malformed allocation error ${String(error)}`);
  }
}
if (!malformedAllocationThrown) {
  throw new Error("HTTP relay response allocator accepted a malformed allocation");
}
if (malformedAllocationReleaseCount !== 1) {
  throw new Error(`HTTP relay response allocator did not release malformed allocation, got ${malformedAllocationReleaseCount}`);
}

async function expectAllocatorStreamFailure(label: string, contentLength: string, chunks: readonly unknown[], expectedMessage: string): Promise<void> {
  let releaseCount = 0;
  let index = 0;
  const malformedStreamFetch: C2Fetch = async (_input, _init) => {
    return {
      status: 200,
      headers: {
        get(name: string): string | null {
          return name.toLowerCase() === "content-length" ? contentLength : null;
        },
      },
      body: {
        getReader() {
          return {
            async read(): Promise<{ done: boolean; value?: SmokeByteArray }> {
              if (index >= chunks.length) {
                return { done: true };
              }
              const value = chunks[index++];
              return { done: false, value: value as Uint8Array };
            },
            releaseLock(): void {},
          };
        },
      },
      async arrayBuffer(): Promise<ArrayBufferLike> {
        throw new Error(`${label} allocator failure must not fall back to arrayBuffer`);
      },
      async text(): Promise<string> {
        return "";
      },
    };
  };
  let thrown = false;
  try {
    await createHttpRelayEncodedTransport("http://relay.example/base/", {
      fetch: malformedStreamFetch,
      responsePayloadAllocator(byteLength: number) {
        return {
          payload: { byteLength, tag: label },
          view: new Uint8Array(byteLength),
          release(): void {
            releaseCount += 1;
          },
        };
      },
    }).call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
  } catch (error) {
    thrown = true;
    if (!(error instanceof C2HttpTransportError) || !String(error).includes(expectedMessage)) {
      throw new Error(`unexpected ${label} allocator stream error ${String(error)}`);
    }
  }
  if (!thrown) {
    throw new Error(`HTTP relay response allocator accepted malformed stream: ${label}`);
  }
  if (releaseCount !== 1) {
    throw new Error(`HTTP relay response allocator did not release after ${label}, got ${releaseCount}`);
  }
}
await expectAllocatorStreamFailure("short stream", "3", [new Uint8Array([1, 2])], "ended before Content-Length");
await expectAllocatorStreamFailure("long stream", "2", [new Uint8Array([1, 2, 3])], "exceeded Content-Length");
await expectAllocatorStreamFailure("non-byte stream", "1", ["not bytes"], "stream chunk must be a Uint8Array");
fetchCalls.length = 0;
const customHeaderWire = createHttpRelayEncodedTransport("http://relay.example/base/", {
  fetch: fetchImpl,
  headers: { "x-app-trace": "trace-1" },
});
await customHeaderWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([2]));
if (fetchCalls[0]?.init.headers?.["x-app-trace"] !== "trace-1") {
  throw new Error("HTTP relay transport dropped non-reserved custom headers");
}
fetchCalls.length = 0;
const protoHeaders = Object.create(null) as Record<string, string>;
protoHeaders["__proto__"] = "trace-proto";
const magicHeaderWire = createHttpRelayEncodedTransport("http://relay.example/base/", {
  fetch: fetchImpl,
  headers: protoHeaders,
});
await magicHeaderWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([2]));
const magicHeaders = fetchCalls[0]?.init.headers;
if (!Object.prototype.hasOwnProperty.call(magicHeaders ?? {}, "__proto__") || magicHeaders?.["__proto__"] !== "trace-proto") {
  throw new Error("HTTP relay transport dropped valid custom header names with Object prototype semantics");
}

const reservedHeaderCases: Array<[string, Record<string, string>]> = [
  ["exact CRM namespace", { "x-c2-expected-crm-ns": "evil" }],
  ["case-insensitive CRM namespace", { "X-C2-Expected-CRM-NS": "evil" }],
  ["case-insensitive content type", { "content-type": "text/plain" }],
];
for (const [label, headers] of reservedHeaderCases) {
  let reservedHeaderThrown = false;
  try {
    createHttpRelayEncodedTransport("http://relay.example/base/", {
      fetch: fetchImpl,
      headers,
    });
  } catch (error) {
    reservedHeaderThrown = true;
    if (!String(error).includes("reserved C-Two HTTP header")) {
      throw new Error(`unexpected reserved header error for ${label}: ${String(error)}`);
    }
  }
  if (!reservedHeaderThrown) {
    throw new Error(`HTTP relay transport accepted reserved header override for ${label}`);
  }
}
let relayAwareReservedHeaderThrown = false;
try {
  createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
    fetch: fetchImpl,
    headers: { "X-C2-Expected-Signature-Hash": "evil" },
  });
} catch (error) {
  relayAwareReservedHeaderThrown = true;
  if (!String(error).includes("reserved C-Two HTTP header")) {
    throw new Error(`unexpected relay-aware reserved header error ${String(error)}`);
  }
}
if (!relayAwareReservedHeaderThrown) {
  throw new Error("relay-aware transport accepted a reserved header override");
}
let badHeaderValueThrown = false;
try {
  createHttpRelayEncodedTransport("http://relay.example/base/", {
    fetch: fetchImpl,
    headers: { "x-app-trace": 1 } as unknown as Record<string, string>,
  });
} catch (error) {
  badHeaderValueThrown = true;
  if (!String(error).includes("x-app-trace")) {
    throw new Error(`unexpected invalid header value error ${String(error)}`);
  }
}
if (!badHeaderValueThrown) {
  throw new Error("HTTP relay transport accepted a non-string header value");
}
const invalidHeaderSyntaxCases: Array<[string, Record<string, string>, string]> = [
  ["colon header name", { "x-app:trace": "trace-1" }, "header name"],
  ["newline header value", { "x-app-trace": "trace-1\r\ninjected: yes" }, "header value"],
];
for (const [label, headers, expectedMessage] of invalidHeaderSyntaxCases) {
  let invalidHeaderSyntaxThrown = false;
  try {
    createHttpRelayEncodedTransport("http://relay.example/base/", {
      fetch: fetchImpl,
      headers,
    });
  } catch (error) {
    invalidHeaderSyntaxThrown = true;
    if (!String(error).includes(expectedMessage)) {
      throw new Error(`unexpected invalid header syntax error for ${label}: ${String(error)}`);
    }
    if (label === "colon header name" && String(error).includes("x-app:trace")) {
      throw new Error(`invalid header name diagnostic echoed malformed header name for ${label}: ${String(error)}`);
    }
  }
  if (!invalidHeaderSyntaxThrown) {
    throw new Error(`HTTP relay transport accepted invalid custom header syntax for ${label}`);
  }
}
let relayAwareBadHeaderSyntaxThrown = false;
try {
  createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
    fetch: fetchImpl,
    headers: { "x-app-trace": "trace-1\nsecond-line" },
  });
} catch (error) {
  relayAwareBadHeaderSyntaxThrown = true;
  if (!String(error).includes("header value")) {
    throw new Error(`unexpected relay-aware invalid header syntax error ${String(error)}`);
  }
}
if (!relayAwareBadHeaderSyntaxThrown) {
  throw new Error("relay-aware transport accepted invalid custom header syntax");
}
let badBaseUrlThrown = false;
try {
  createHttpRelayEncodedTransport(42 as unknown as string, { fetch: fetchImpl });
} catch (error) {
  badBaseUrlThrown = true;
  if (!String(error).includes("base URL")) {
    throw new Error(`unexpected bad base URL error ${String(error)}`);
  }
}
if (!badBaseUrlThrown) {
  throw new Error("HTTP relay transport accepted a non-string base URL");
}
const invalidBaseUrlCases: Array<[string, string, () => void]> = [
  ["relative base", "/relay", () => { createHttpRelayEncodedTransport("/relay", { fetch: fetchImpl }); }],
  ["ftp base", "ftp://relay.example/base", () => { createHttpRelayEncodedTransport("ftp://relay.example/base", { fetch: fetchImpl }); }],
  ["query base", "http://relay.example/base?debug=1", () => { createHttpRelayEncodedTransport("http://relay.example/base?debug=1", { fetch: fetchImpl }); }],
  ["fragment base", "http://relay.example/base#debug", () => { createHttpRelayEncodedTransport("http://relay.example/base#debug", { fetch: fetchImpl }); }],
  ["leading whitespace base", " http://relay.example/base", () => { createHttpRelayEncodedTransport(" http://relay.example/base", { fetch: fetchImpl }); }],
  ["path whitespace base", "http://relay.example/base path", () => { createHttpRelayEncodedTransport("http://relay.example/base path", { fetch: fetchImpl }); }],
  ["credentials base", "http://user:pass@relay.example/base", () => { createHttpRelayEncodedTransport("http://user:pass@relay.example/base", { fetch: fetchImpl }); }],
  ["relative anchor", "/anchor", () => { createRelayAwareHttpEncodedTransport("/anchor", { fetch: fetchImpl }); }],
  ["ftp anchor", "ftp://anchor.example/root", () => { createRelayAwareHttpEncodedTransport("ftp://anchor.example/root", { fetch: fetchImpl }); }],
  ["query anchor", "http://anchor.example/root?debug=1", () => { createRelayAwareHttpEncodedTransport("http://anchor.example/root?debug=1", { fetch: fetchImpl }); }],
  ["trailing whitespace anchor", "http://anchor.example/root ", () => { createRelayAwareHttpEncodedTransport("http://anchor.example/root ", { fetch: fetchImpl }); }],
  ["credentials anchor", "http://user:pass@anchor.example/root", () => { createRelayAwareHttpEncodedTransport("http://user:pass@anchor.example/root", { fetch: fetchImpl }); }],
];
for (const [label, value, makeTransport] of invalidBaseUrlCases) {
  let invalidUrlThrown = false;
  try {
    makeTransport();
  } catch (error) {
    invalidUrlThrown = true;
    if (!String(error).includes("HTTP(S)") && !String(error).includes("query or fragment") && !String(error).includes("whitespace") && !String(error).includes("credentials")) {
      throw new Error(`unexpected invalid URL error for ${label} ${value}: ${String(error)}`);
    }
  }
  if (!invalidUrlThrown) {
    throw new Error(`generated HTTP transport accepted invalid relay URL for ${label}: ${value}`);
  }
}
let badFetchThrown = false;
try {
  createHttpRelayEncodedTransport("http://relay.example/base/", { fetch: 42 as unknown as C2Fetch });
} catch (error) {
  badFetchThrown = true;
  if (!String(error).includes("fetch")) {
    throw new Error(`unexpected bad fetch error ${String(error)}`);
  }
}
if (!badFetchThrown) {
  throw new Error("HTTP relay transport accepted a non-function fetch option");
}
let badOptionsThrown = false;
try {
  createHttpRelayEncodedTransport("http://relay.example/base/", null as never);
} catch (error) {
  badOptionsThrown = true;
  if (!String(error).includes("transport options")) {
    throw new Error(`unexpected bad options error ${String(error)}`);
  }
}
if (!badOptionsThrown) {
  throw new Error("HTTP relay transport accepted null transport options");
}
let badRelayAwareOptionsThrown = false;
try {
  createRelayAwareHttpEncodedTransport("http://anchor.example/root/", [] as never);
} catch (error) {
  badRelayAwareOptionsThrown = true;
  if (!String(error).includes("transport options")) {
    throw new Error(`unexpected relay-aware bad options error ${String(error)}`);
  }
}
if (!badRelayAwareOptionsThrown) {
  throw new Error("relay-aware transport accepted array transport options");
}
const originalGlobalFetch = (globalThis as unknown as { fetch?: unknown }).fetch;
try {
  let missingGlobalFetchThrown = false;
  (globalThis as unknown as { fetch?: unknown }).fetch = undefined;
  try {
    createHttpRelayEncodedTransport("http://relay.example/base/");
  } catch (error) {
    missingGlobalFetchThrown = true;
    if (!String(error).includes("fetch implementation")) {
      throw new Error(`unexpected missing global fetch error ${String(error)}`);
    }
  }
  if (!missingGlobalFetchThrown) {
    throw new Error("HTTP relay transport accepted a missing global fetch implementation");
  }
  let nonFunctionGlobalFetchThrown = false;
  (globalThis as unknown as { fetch?: unknown }).fetch = 42;
  try {
    createHttpRelayEncodedTransport("http://relay.example/base/");
  } catch (error) {
    nonFunctionGlobalFetchThrown = true;
    if (!String(error).includes("fetch implementation")) {
      throw new Error(`unexpected non-function global fetch error ${String(error)}`);
    }
  }
  if (!nonFunctionGlobalFetchThrown) {
    throw new Error("HTTP relay transport accepted a non-function global fetch implementation");
  }
} finally {
  (globalThis as unknown as { fetch?: unknown }).fetch = originalGlobalFetch;
}

let malformedContractFetchCalls = 0;
const malformedContractWire = createHttpRelayEncodedTransport("http://relay.example/base/", {
  fetch: async () => {
    malformedContractFetchCalls += 1;
    return {
      status: 200,
      async arrayBuffer(): Promise<ArrayBufferLike> {
        return new Uint8Array().buffer;
      },
      async text(): Promise<string> {
        return "";
      },
    };
  },
});
let malformedContractThrown = false;
try {
  await malformedContractWire.call(
    "route",
    { ...FASTDB_PORTABLE_CONTRACT, namespace: "" },
    "query",
    new Uint8Array([1]),
  );
} catch (error) {
  malformedContractThrown = true;
  if (!String(error).includes("namespace")) {
    throw new Error(`unexpected malformed route contract error ${String(error)}`);
  }
}
if (!malformedContractThrown) {
  throw new Error("HTTP relay transport accepted a malformed route contract identity");
}
if (malformedContractFetchCalls !== 0) {
  throw new Error(`malformed route contract identity reached fetch ${malformedContractFetchCalls} time(s)`);
}
const malformedContractTextCases: Array<[string, typeof FASTDB_PORTABLE_CONTRACT, string]> = [
  ["control namespace", { ...FASTDB_PORTABLE_CONTRACT, namespace: "test.contract.fastdb\r\nx-injected: yes" }, "namespace"],
  ["too-long namespace", { ...FASTDB_PORTABLE_CONTRACT, namespace: "a".repeat(256) }, "cannot exceed"],
  ["slash name", { ...FASTDB_PORTABLE_CONTRACT, name: "Fastdb/Portable" }, "name"],
  ["backslash version", { ...FASTDB_PORTABLE_CONTRACT, version: "0.1.0\\dev" }, "version"],
];
for (const [label, contract, expectedMessage] of malformedContractTextCases) {
  let malformedContractTextThrown = false;
  try {
    await malformedContractWire.call(
      "route",
      contract,
      "query",
      new Uint8Array([1]),
    );
  } catch (error) {
    malformedContractTextThrown = true;
    if (!String(error).includes(expectedMessage)) {
      throw new Error(`unexpected malformed route contract text error for ${label}: ${String(error)}`);
    }
  }
  if (!malformedContractTextThrown) {
    throw new Error(`HTTP relay transport accepted malformed route contract text for ${label}`);
  }
}
if (malformedContractFetchCalls !== 0) {
  throw new Error(`malformed route contract text reached fetch ${malformedContractFetchCalls} time(s)`);
}

let emptyRouteFetchCalls = 0;
const emptyRouteWire = createHttpRelayEncodedTransport("http://relay.example/base/", {
  fetch: async () => {
    emptyRouteFetchCalls += 1;
    return {
      status: 200,
      async arrayBuffer(): Promise<ArrayBufferLike> {
        return new Uint8Array().buffer;
      },
      async text(): Promise<string> {
        return "";
      },
    };
  },
});
let emptyRouteThrown = false;
try {
  await emptyRouteWire.call("", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  emptyRouteThrown = true;
  if (!String(error).includes("routeName")) {
    throw new Error(`unexpected empty routeName error ${String(error)}`);
  }
}
if (!emptyRouteThrown) {
  throw new Error("HTTP relay transport accepted an empty routeName");
}
let emptyMethodThrown = false;
try {
  await emptyRouteWire.call("route", FASTDB_PORTABLE_CONTRACT, "", new Uint8Array([1]));
} catch (error) {
  emptyMethodThrown = true;
  if (!String(error).includes("method")) {
    throw new Error(`unexpected empty method error ${String(error)}`);
  }
}
if (!emptyMethodThrown) {
  throw new Error("HTTP relay transport accepted an empty method");
}
if (emptyRouteFetchCalls !== 0) {
  throw new Error(`empty routeName/method reached fetch ${emptyRouteFetchCalls} time(s)`);
}
const malformedRoutePathCases: Array<[string, string, string, string]> = [
  ["routeName whitespace", " route", "query", "routeName"],
  ["routeName control", "route\nInjected", "query", "control characters"],
  ["routeName backslash", "route\\dev", "query", "path or tag separators"],
  ["method whitespace", "route", " query", "method"],
  ["method control", "route", "query\r\nInjected", "control characters"],
  ["method slash", "route", "query/extra", "path or tag separators"],
  ["method backslash", "route", "query\\extra", "path or tag separators"],
];
for (const [label, routeName, method, expectedMessage] of malformedRoutePathCases) {
  let malformedRoutePathThrown = false;
  try {
    await emptyRouteWire.call(routeName, FASTDB_PORTABLE_CONTRACT, method, new Uint8Array([1]));
  } catch (error) {
    malformedRoutePathThrown = true;
    if (!String(error).includes(expectedMessage)) {
      throw new Error(`unexpected malformed route path text error for ${label}: ${String(error)}`);
    }
  }
  if (!malformedRoutePathThrown) {
    throw new Error(`HTTP relay transport accepted malformed route path text for ${label}`);
  }
}
if (emptyRouteFetchCalls !== 0) {
  throw new Error(`malformed routeName/method reached fetch ${emptyRouteFetchCalls} time(s)`);
}
let routeNameSlashFetchCalls = 0;
const routeNameSlashWire = createHttpRelayEncodedTransport("http://relay.example/base/", {
  fetch: async (input: string | URL) => {
    routeNameSlashFetchCalls += 1;
    if (!String(input).includes("/route%2Fdev/query")) {
      throw new Error(`routeName slash was not encoded as one path segment: ${String(input)}`);
    }
    return {
      status: 200,
      async arrayBuffer(): Promise<ArrayBufferLike> {
        return new Uint8Array().buffer;
      },
      async text(): Promise<string> {
        return "";
      },
    };
  },
});
await routeNameSlashWire.call("route/dev", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
if (routeNameSlashFetchCalls !== 1) {
  throw new Error(`routeName slash should reach fetch once, got ${routeNameSlashFetchCalls}`);
}
let nonBytesPayloadThrown = false;
try {
  await emptyRouteWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", "not-bytes" as unknown as Uint8Array);
} catch (error) {
  nonBytesPayloadThrown = true;
  if (!String(error).includes("payload") || !String(error).includes("Uint8Array")) {
    throw new Error(`unexpected non-Uint8Array payload error ${String(error)}`);
  }
}
if (!nonBytesPayloadThrown) {
  throw new Error("HTTP relay transport accepted a non-Uint8Array payload");
}
if (emptyRouteFetchCalls !== 0) {
  throw new Error(`non-Uint8Array payload reached fetch ${emptyRouteFetchCalls} time(s)`);
}
let emptyRelayAwareFetchCalls = 0;
const emptyRelayAwareWire = createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
  fetch: async () => {
    emptyRelayAwareFetchCalls += 1;
    return {
      status: 200,
      async arrayBuffer(): Promise<ArrayBufferLike> {
        return new Uint8Array().buffer;
      },
      async text(): Promise<string> {
        return "[]";
      },
    };
  },
});
let emptyRelayAwareRouteThrown = false;
try {
  await emptyRelayAwareWire.call("", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  emptyRelayAwareRouteThrown = true;
  if (!String(error).includes("routeName")) {
    throw new Error(`unexpected relay-aware empty routeName error ${String(error)}`);
  }
}
if (!emptyRelayAwareRouteThrown) {
  throw new Error("relay-aware transport accepted an empty routeName");
}
let emptyRelayAwareMethodThrown = false;
try {
  await emptyRelayAwareWire.call("route", FASTDB_PORTABLE_CONTRACT, "", new Uint8Array([1]));
} catch (error) {
  emptyRelayAwareMethodThrown = true;
  if (!String(error).includes("method")) {
    throw new Error(`unexpected relay-aware empty method error ${String(error)}`);
  }
}
if (!emptyRelayAwareMethodThrown) {
  throw new Error("relay-aware transport accepted an empty method");
}
let relayAwareMalformedContractTextThrown = false;
try {
  await emptyRelayAwareWire.call(
    "route",
    { ...FASTDB_PORTABLE_CONTRACT, namespace: "test.contract.fastdb\r\nx-injected: yes" },
    "query",
    new Uint8Array([1]),
  );
} catch (error) {
  relayAwareMalformedContractTextThrown = true;
  if (!String(error).includes("namespace")) {
    throw new Error(`unexpected relay-aware malformed route contract text error ${String(error)}`);
  }
}
if (!relayAwareMalformedContractTextThrown) {
  throw new Error("relay-aware transport accepted malformed route contract text");
}
let nonBytesRelayAwarePayloadThrown = false;
try {
  await emptyRelayAwareWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", "not-bytes" as unknown as Uint8Array);
} catch (error) {
  nonBytesRelayAwarePayloadThrown = true;
  if (!String(error).includes("payload") || !String(error).includes("Uint8Array")) {
    throw new Error(`unexpected relay-aware non-Uint8Array payload error ${String(error)}`);
  }
}
if (!nonBytesRelayAwarePayloadThrown) {
  throw new Error("relay-aware transport accepted a non-Uint8Array payload");
}
for (const [label, routeName, method, expectedMessage] of malformedRoutePathCases) {
  let malformedRelayAwareRoutePathThrown = false;
  try {
    await emptyRelayAwareWire.call(routeName, FASTDB_PORTABLE_CONTRACT, method, new Uint8Array([1]));
  } catch (error) {
    malformedRelayAwareRoutePathThrown = true;
    if (!String(error).includes(expectedMessage)) {
      throw new Error(`unexpected relay-aware malformed route path text error for ${label}: ${String(error)}`);
    }
  }
  if (!malformedRelayAwareRoutePathThrown) {
    throw new Error(`relay-aware transport accepted malformed route path text for ${label}`);
  }
}
if (emptyRelayAwareFetchCalls !== 0) {
  throw new Error(`relay-aware malformed routeName/method/contract text reached fetch ${emptyRelayAwareFetchCalls} time(s)`);
}

const throwingFetchWire = createHttpRelayEncodedTransport("http://relay.example/base/", {
  fetch: async () => {
    throw new Error("temporary data-plane transport failure");
  },
});
let throwingFetchError: unknown;
try {
  await throwingFetchWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  throwingFetchError = error;
}
if (!(throwingFetchError instanceof C2HttpTransportError)) {
  throw new Error(`expected C2HttpTransportError for data-plane fetch failure, got ${String(throwingFetchError)}`);
}
if (!String(throwingFetchError).includes("C-Two HTTP relay call fetch failed")) {
  throw new Error(`unexpected data-plane fetch error ${String(throwingFetchError)}`);
}

const brokenBodyWire = createHttpRelayEncodedTransport("http://relay.example/base/", {
  fetch: async () => ({
    status: 200,
    async arrayBuffer(): Promise<ArrayBufferLike> {
      throw new Error("response body unavailable");
    },
    async text(): Promise<string> {
      return "";
    },
  }),
});
let brokenBodyError: unknown;
try {
  await brokenBodyWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  brokenBodyError = error;
}
if (!(brokenBodyError instanceof C2HttpTransportError)) {
  throw new Error(`expected C2HttpTransportError for data-plane body failure, got ${String(brokenBodyError)}`);
}
if (!String(brokenBodyError).includes("C-Two HTTP relay call body read failed")) {
  throw new Error(`unexpected data-plane body error ${String(brokenBodyError)}`);
}

const brokenCrmErrorBodyWire = createHttpRelayEncodedTransport("http://relay.example/base/", {
  fetch: async () => ({
    status: 500,
    async arrayBuffer(): Promise<ArrayBufferLike> {
      throw new Error("crm error body unavailable");
    },
    async text(): Promise<string> {
      return "";
    },
  }),
});
let brokenCrmErrorBodyError: unknown;
try {
  await brokenCrmErrorBodyWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  brokenCrmErrorBodyError = error;
}
if (!(brokenCrmErrorBodyError instanceof C2HttpTransportError)) {
  throw new Error(`expected C2HttpTransportError for CRM-error body failure, got ${String(brokenCrmErrorBodyError)}`);
}
if (!String(brokenCrmErrorBodyError).includes("C-Two HTTP relay call body read failed")) {
  throw new Error(`unexpected CRM-error body read error ${String(brokenCrmErrorBodyError)}`);
}

const brokenRelayErrorBodyWire = createHttpRelayEncodedTransport("http://relay.example/base/", {
  fetch: async () => ({
    status: 404,
    async arrayBuffer(): Promise<ArrayBufferLike> {
      return new Uint8Array().buffer;
    },
    async text(): Promise<string> {
      throw new Error("relay error body unavailable");
    },
  }),
});
let brokenRelayErrorBodyError: unknown;
try {
  await brokenRelayErrorBodyWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  brokenRelayErrorBodyError = error;
}
if (!(brokenRelayErrorBodyError instanceof C2HttpTransportError)) {
  throw new Error(`expected C2HttpTransportError for relay-error body failure, got ${String(brokenRelayErrorBodyError)}`);
}
if (!String(brokenRelayErrorBodyError).includes("C-Two HTTP relay call body read failed")) {
  throw new Error(`unexpected relay-error body read error ${String(brokenRelayErrorBodyError)}`);
}

let oversizedRelayErrorTextCalls = 0;
const oversizedRelayErrorBodyWire = createHttpRelayEncodedTransport("http://relay.example/base/", {
  fetch: async () => ({
    status: 404,
    headers: {
      get(name: string): string | null {
        return name.toLowerCase() === "content-length" ? "2147483648" : null;
      },
    },
    async arrayBuffer(): Promise<ArrayBufferLike> {
      return new Uint8Array().buffer;
    },
    async text(): Promise<string> {
      oversizedRelayErrorTextCalls += 1;
      throw new Error("oversized relay error text should fail before text()");
    },
  }),
});
let oversizedRelayErrorBodyThrown = false;
try {
  await oversizedRelayErrorBodyWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  oversizedRelayErrorBodyThrown = true;
  if (!(error instanceof C2HttpTransportError) || !String(error).includes("Content-Length") || !String(error).includes("no greater than")) {
    throw new Error(`unexpected oversized relay-error text body error ${String(error)}`);
  }
}
if (!oversizedRelayErrorBodyThrown) {
  throw new Error("HTTP relay transport accepted an oversized relay-error text body");
}
if (oversizedRelayErrorTextCalls !== 0) {
  throw new Error(`HTTP relay transport read oversized relay-error text ${oversizedRelayErrorTextCalls} time(s)`);
}

const neverFetchImpl = async (_input: string, init: { method?: string; headers?: Record<string, string>; body?: Uint8Array; signal?: { addEventListener(type: "abort", listener: () => void, options?: { readonly once?: boolean }): void } }) => {
  if (!init.signal) {
    throw new Error("timeout fetch did not receive AbortSignal");
  }
  return await new Promise<never>((_resolve, reject) => {
    init.signal?.addEventListener("abort", () => reject(new Error("fetch aborted")), { once: true });
  });
};
const timeoutWire = createHttpRelayEncodedTransport("http://relay.example/base/", {
  fetch: neverFetchImpl,
  callTimeoutMs: 1,
});
let callTimeoutThrown = false;
try {
  await timeoutWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  callTimeoutThrown = true;
  if (!String(error).includes("timed out after 1ms")) {
    throw new Error(`unexpected call timeout error ${String(error)}`);
  }
}
if (!callTimeoutThrown) {
  throw new Error("HTTP relay transport accepted a hanging call without timeout");
}
let badCallTimeoutThrown = false;
try {
  createHttpRelayEncodedTransport("http://relay.example/base/", {
    fetch: fetchImpl,
    callTimeoutMs: -1,
  });
} catch (error) {
  badCallTimeoutThrown = true;
  if (!String(error).includes("callTimeoutMs")) {
    throw new Error(`unexpected callTimeoutMs error ${String(error)}`);
  }
}
if (!badCallTimeoutThrown) {
  throw new Error("HTTP relay transport accepted a negative callTimeoutMs");
}
let fractionalCallTimeoutThrown = false;
try {
  createHttpRelayEncodedTransport("http://relay.example/base/", {
    fetch: fetchImpl,
    callTimeoutMs: 0.5,
  });
} catch (error) {
  fractionalCallTimeoutThrown = true;
  if (!String(error).includes("callTimeoutMs")) {
    throw new Error(`unexpected fractional callTimeoutMs error ${String(error)}`);
  }
}
if (!fractionalCallTimeoutThrown) {
  throw new Error("HTTP relay transport accepted a fractional callTimeoutMs");
}

const relayFetchCalls: Array<{ input: string; init: { method?: string; headers?: Record<string, string>; body?: Uint8Array } }> = [];
const relayFetchImpl = async (input: string, init: { method?: string; headers?: Record<string, string>; body?: Uint8Array }) => {
  relayFetchCalls.push({ input, init });
  if (init.method === "GET") {
    return {
      status: 200,
      async arrayBuffer(): Promise<ArrayBufferLike> {
        return new Uint8Array().buffer;
      },
      async text(): Promise<string> {
        return JSON.stringify([
          {
            name: "route",
            relay_url: "http://stale.example/base/",
            crm_ns: "test.contract.fastdb",
            crm_name: "FastdbPortable",
            crm_ver: "0.1.0",
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            max_payload_size: 1048576,
          },
          {
            name: "route",
            relay_url: "http://live.example/base/",
            crm_ns: "test.contract.fastdb",
            crm_name: "FastdbPortable",
            crm_ver: "0.1.0",
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            max_payload_size: 1048576,
          },
        ]);
      },
    };
  }
  if (input.startsWith("http://stale.example/base/")) {
    return {
      status: 502,
      async arrayBuffer(): Promise<ArrayBufferLike> {
        return new Uint8Array().buffer;
      },
      async text(): Promise<string> {
        return JSON.stringify({ error: "UpstreamUnavailable" });
      },
    };
  }
  return {
    status: 200,
    async arrayBuffer(): Promise<ArrayBufferLike> {
      return (init.body ?? new Uint8Array()).buffer;
    },
    async text(): Promise<string> {
      return "";
    },
  };
};

const relayAwareWire = createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
  fetch: relayFetchImpl,
  maxAttempts: 2,
});
const relayAwareResult = await relayAwareWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([7]));
if (relayAwareResult[0] !== 7) {
  throw new Error("relay-aware HTTP transport did not return response bytes");
}
if (relayFetchCalls.length !== 4) {
  throw new Error(`expected resolve, stale call, refreshed resolve, and live call, got ${relayFetchCalls.length}`);
}
if (relayFetchCalls[0].input !== "http://anchor.example/root/_resolve/route?crm_ns=test.contract.fastdb&crm_name=FastdbPortable&crm_ver=0.1.0&abi_hash=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef&signature_hash=abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789") {
  throw new Error(`unexpected relay resolve URL ${relayFetchCalls[0].input}`);
}
if (relayFetchCalls[0].init.method !== "GET") {
  throw new Error("relay-aware transport did not resolve with GET");
}
if (relayFetchCalls[1].input !== "http://stale.example/base/route/query") {
  throw new Error(`unexpected stale candidate URL ${relayFetchCalls[1].input}`);
}
if (relayFetchCalls[2].input !== relayFetchCalls[0].input) {
  throw new Error(`stale route did not force a fresh resolve, got ${relayFetchCalls[2].input}`);
}
if (relayFetchCalls[3].input !== "http://live.example/base/route/query") {
  throw new Error(`unexpected live candidate URL ${relayFetchCalls[3].input}`);
}
relayFetchCalls.length = 0;
const relayAwareCachedResult = await relayAwareWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([8]));
if (relayAwareCachedResult[0] !== 8) {
  throw new Error("relay-aware HTTP transport did not return cached-route response bytes");
}
if (relayFetchCalls.length !== 1) {
  throw new Error(`expected cached route to skip resolve, got ${relayFetchCalls.length} fetch calls`);
}
if (String(relayFetchCalls[0].input) !== "http://live.example/base/route/query") {
  throw new Error(`unexpected cached-route URL ${relayFetchCalls[0].input}`);
}

const relayAwareAllocatedResponse = { byteLength: 2, tag: "relay-aware-allocated-response" };
const relayAwareAllocatedBytes = new Uint8Array(2);
let relayAwareAllocatedCount = 0;
const relayAwareAllocatedFetchCalls: Array<{ input: string; init: { method?: string; headers?: Record<string, string>; body?: Uint8Array } }> = [];
const relayAwareAllocatedFetch: C2Fetch = async (input: string, init: { method?: string; headers?: Record<string, string>; body?: Uint8Array }) => {
  relayAwareAllocatedFetchCalls.push({ input, init });
  if (init.method === "GET") {
    return {
      status: 200,
      async arrayBuffer(): Promise<ArrayBufferLike> {
        return new Uint8Array().buffer;
      },
      async text(): Promise<string> {
        return JSON.stringify([
          {
            name: "route",
            relay_url: "http://allocated.example/base/",
            crm_ns: "test.contract.fastdb",
            crm_name: "FastdbPortable",
            crm_ver: "0.1.0",
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            max_payload_size: 1048576,
          },
        ]);
      },
    };
  }
  const chunks = [new Uint8Array([8]), new Uint8Array([9])];
  let index = 0;
  return {
    status: 200,
    headers: {
      get(_name: string): string | null {
        return null;
      },
    },
    body: {
      getReader() {
        return {
          async read(): Promise<{ done: boolean; value?: SmokeByteArray }> {
            const value = chunks[index++];
            if (value === undefined) {
              return { done: true };
            }
            return { done: false, value };
          },
          releaseLock(): void {},
        };
      },
    },
    async arrayBuffer(): Promise<ArrayBufferLike> {
      throw new Error("relay-aware response allocator should not fall back to arrayBuffer");
    },
    async text(): Promise<string> {
      return "";
    },
  };
};
const relayAwareAllocatedWire = createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
  fetch: relayAwareAllocatedFetch,
  responsePayloadUnknownLengthStrategy: "buffer",
  responsePayloadUnknownLengthMaxBytes: 2,
  responsePayloadAllocator(byteLength: number) {
    relayAwareAllocatedCount += 1;
    if (byteLength !== 2) {
      throw new Error(`unexpected relay-aware allocated response length ${byteLength}`);
    }
    return {
      payload: relayAwareAllocatedResponse,
      view: relayAwareAllocatedBytes,
    };
  },
});
const relayAwareAllocatedResult = await relayAwareAllocatedWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
if (relayAwareAllocatedResult !== relayAwareAllocatedResponse) {
  throw new Error("relay-aware HTTP transport did not return provider-owned allocated response");
}
if (relayAwareAllocatedBytes[0] !== 8 || relayAwareAllocatedBytes[1] !== 9) {
  throw new Error(`relay-aware HTTP transport did not stream into allocated response bytes: ${Array.from(relayAwareAllocatedBytes).join(",")}`);
}
if (relayAwareAllocatedFetchCalls.length !== 2 || relayAwareAllocatedFetchCalls[1]?.input !== "http://allocated.example/base/route/query") {
  throw new Error("relay-aware response allocator did not use the resolved data-plane route");
}
if (relayAwareAllocatedCount !== 1) {
  throw new Error(`relay-aware response allocator should apply only to data-plane response, got ${relayAwareAllocatedCount} allocation(s)`);
}

const relayHeaderFetchCalls: Array<{ input: string; init: { method?: string; headers?: Record<string, string>; body?: Uint8Array } }> = [];
const relayHeaderFetchImpl = async (input: string, init: { method?: string; headers?: Record<string, string>; body?: Uint8Array }) => {
  relayHeaderFetchCalls.push({ input, init });
  if (init.method === "GET") {
    return {
      status: 200,
      async arrayBuffer(): Promise<ArrayBufferLike> {
        return new Uint8Array().buffer;
      },
      async text(): Promise<string> {
        return JSON.stringify([
          {
            name: "route",
            relay_url: "http://header.example/base/",
            crm_ns: "test.contract.fastdb",
            crm_name: "FastdbPortable",
            crm_ver: "0.1.0",
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            max_payload_size: 1048576,
          },
        ]);
      },
    };
  }
  return {
    status: 200,
    async arrayBuffer(): Promise<ArrayBufferLike> {
      return (init.body ?? new Uint8Array()).buffer;
    },
    async text(): Promise<string> {
      return "";
    },
  };
};
const relayHeaderWire = createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
  fetch: relayHeaderFetchImpl,
  headers: { "x-app-trace": "trace-2" },
});
await relayHeaderWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([9]));
if (relayHeaderFetchCalls.length !== 2) {
  throw new Error(`expected relay-aware header smoke to resolve and call once, got ${relayHeaderFetchCalls.length}`);
}
if (relayHeaderFetchCalls[0].init.headers?.["x-app-trace"] !== "trace-2") {
  throw new Error("relay-aware resolve dropped non-reserved custom headers");
}
if (relayHeaderFetchCalls[1].init.headers?.["x-app-trace"] !== "trace-2") {
  throw new Error("relay-aware data-plane call dropped non-reserved custom headers");
}
relayHeaderFetchCalls.length = 0;
const relayProtoHeaders = Object.create(null) as Record<string, string>;
relayProtoHeaders["__proto__"] = "trace-relay-proto";
const relayProtoHeaderWire = createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
  fetch: relayHeaderFetchImpl,
  headers: relayProtoHeaders,
});
await relayProtoHeaderWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([10]));
if (relayHeaderFetchCalls.length !== 2) {
  throw new Error(`expected relay-aware prototype-header smoke to resolve and call once, got ${relayHeaderFetchCalls.length}`);
}
const relayProtoResolveHeaders = relayHeaderFetchCalls[0].init.headers;
if (!Object.prototype.hasOwnProperty.call(relayProtoResolveHeaders ?? {}, "__proto__") || relayProtoResolveHeaders?.["__proto__"] !== "trace-relay-proto") {
  throw new Error("relay-aware resolve dropped valid custom header names with Object prototype semantics");
}
const relayProtoCallHeaders = relayHeaderFetchCalls[1].init.headers;
if (!Object.prototype.hasOwnProperty.call(relayProtoCallHeaders ?? {}, "__proto__") || relayProtoCallHeaders?.["__proto__"] !== "trace-relay-proto") {
  throw new Error("relay-aware data-plane call dropped valid custom header names with Object prototype semantics");
}

const resolveTransportRetryFetchCalls: Array<{ input: string; init: { method?: string; headers?: Record<string, string>; body?: Uint8Array } }> = [];
const resolveTransportRetryFetchImpl = async (input: string, init: { method?: string; headers?: Record<string, string>; body?: Uint8Array }) => {
  resolveTransportRetryFetchCalls.push({ input, init });
  if (init.method === "GET" && resolveTransportRetryFetchCalls.filter((call) => call.init.method === "GET").length === 1) {
    throw new Error("temporary resolve transport failure");
  }
  if (init.method === "GET") {
    return {
      status: 200,
      async arrayBuffer(): Promise<ArrayBufferLike> {
        return new Uint8Array().buffer;
      },
      async text(): Promise<string> {
        return JSON.stringify([
          {
            name: "route",
            relay_url: "http://resolve-retry.example/base/",
            crm_ns: "test.contract.fastdb",
            crm_name: "FastdbPortable",
            crm_ver: "0.1.0",
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            max_payload_size: 1048576,
          },
        ]);
      },
    };
  }
  return {
    status: 200,
    async arrayBuffer(): Promise<ArrayBufferLike> {
      return (init.body ?? new Uint8Array()).buffer;
    },
    async text(): Promise<string> {
      return "";
    },
  };
};
const resolveTransportRetryWire = createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
  fetch: resolveTransportRetryFetchImpl,
  maxAttempts: 2,
});
const resolveTransportRetryResult = await resolveTransportRetryWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([9]));
if (resolveTransportRetryResult[0] !== 9) {
  throw new Error("relay-aware HTTP transport did not return response after resolve transport retry");
}
if (resolveTransportRetryFetchCalls.length !== 3) {
  throw new Error(`expected failed resolve, retried resolve, and data-plane call, got ${resolveTransportRetryFetchCalls.length}`);
}
if (resolveTransportRetryFetchCalls[2].input !== "http://resolve-retry.example/base/route/query") {
  throw new Error(`unexpected resolve transport retry data-plane URL ${resolveTransportRetryFetchCalls[2].input}`);
}

const resolveShapeRetryFetchCalls: Array<{ input: string; init: { method?: string; headers?: Record<string, string>; body?: Uint8Array } }> = [];
const resolveShapeRetryFetchImpl = async (input: string, init: { method?: string; headers?: Record<string, string>; body?: Uint8Array }) => {
  resolveShapeRetryFetchCalls.push({ input, init });
  if (init.method === "GET" && resolveShapeRetryFetchCalls.filter((call) => call.init.method === "GET").length === 1) {
    return {
      status: 200,
      async arrayBuffer(): Promise<ArrayBufferLike> {
        return new Uint8Array().buffer;
      },
      async text(): Promise<string> {
        return JSON.stringify({ route: "not-an-array" });
      },
    };
  }
  if (init.method === "GET") {
    return {
      status: 200,
      async arrayBuffer(): Promise<ArrayBufferLike> {
        return new Uint8Array().buffer;
      },
      async text(): Promise<string> {
        return JSON.stringify([
          {
            name: "route",
            relay_url: "http://shape-retry.example/base/",
            crm_ns: "test.contract.fastdb",
            crm_name: "FastdbPortable",
            crm_ver: "0.1.0",
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            max_payload_size: 1048576,
          },
        ]);
      },
    };
  }
  return {
    status: 200,
    async arrayBuffer(): Promise<ArrayBufferLike> {
      return (init.body ?? new Uint8Array()).buffer;
    },
    async text(): Promise<string> {
      return "";
    },
  };
};
const resolveShapeRetryWire = createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
  fetch: resolveShapeRetryFetchImpl,
  maxAttempts: 2,
});
const resolveShapeRetryResult = await resolveShapeRetryWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([10]));
if (resolveShapeRetryResult[0] !== 10) {
  throw new Error("relay-aware HTTP transport did not return response after resolve shape retry");
}
if (resolveShapeRetryFetchCalls.length !== 3) {
  throw new Error(`expected malformed resolve, retried resolve, and data-plane call, got ${resolveShapeRetryFetchCalls.length}`);
}
if (resolveShapeRetryFetchCalls[2].input !== "http://shape-retry.example/base/route/query") {
  throw new Error(`unexpected resolve shape retry data-plane URL ${resolveShapeRetryFetchCalls[2].input}`);
}

let badMaxAttemptsThrown = false;
try {
  createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
    fetch: relayFetchImpl,
    maxAttempts: -1,
  });
} catch (error) {
  badMaxAttemptsThrown = true;
  if (!String(error).includes("maxAttempts")) {
    throw new Error(`unexpected maxAttempts error ${String(error)}`);
  }
}
if (!badMaxAttemptsThrown) {
  throw new Error("relay-aware transport accepted a negative maxAttempts");
}
let fractionalMaxAttemptsThrown = false;
try {
  createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
    fetch: relayFetchImpl,
    maxAttempts: 1.5,
  });
} catch (error) {
  fractionalMaxAttemptsThrown = true;
  if (!String(error).includes("maxAttempts")) {
    throw new Error(`unexpected fractional maxAttempts error ${String(error)}`);
  }
}
if (!fractionalMaxAttemptsThrown) {
  throw new Error("relay-aware transport accepted a fractional maxAttempts");
}
let oversizedMaxAttemptsThrown = false;
try {
  createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
    fetch: relayFetchImpl,
    maxAttempts: 33,
  });
} catch (error) {
  oversizedMaxAttemptsThrown = true;
  if (!String(error).includes("maxAttempts")) {
    throw new Error(`unexpected oversized maxAttempts error ${String(error)}`);
  }
}
if (!oversizedMaxAttemptsThrown) {
  throw new Error("relay-aware transport accepted an oversized maxAttempts");
}
const zeroMaxFetchCalls: Array<{ input: string; init: { method?: string; headers?: Record<string, string>; body?: Uint8Array } }> = [];
const zeroMaxFetchImpl = async (input: string, init: { method?: string; headers?: Record<string, string>; body?: Uint8Array }) => {
  zeroMaxFetchCalls.push({ input, init });
  if (init.method === "GET") {
    return {
      status: 200,
      async arrayBuffer(): Promise<ArrayBufferLike> {
        return new Uint8Array().buffer;
      },
      async text(): Promise<string> {
        return JSON.stringify([
          {
            name: "route",
            relay_url: "http://zero-max.example/base/",
            crm_ns: "test.contract.fastdb",
            crm_name: "FastdbPortable",
            crm_ver: "0.1.0",
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            max_payload_size: 1048576,
          },
        ]);
      },
    };
  }
  return {
    status: 200,
    async arrayBuffer(): Promise<ArrayBufferLike> {
      return (init.body ?? new Uint8Array()).buffer;
    },
    async text(): Promise<string> {
      return "";
    },
  };
};
const zeroMaxWire = createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
  fetch: zeroMaxFetchImpl,
  maxAttempts: 0,
});
const zeroMaxResult = await zeroMaxWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([6]));
if (zeroMaxResult[0] !== 6) {
  throw new Error("relay-aware HTTP transport did not normalize maxAttempts 0 to one attempt");
}
if (zeroMaxFetchCalls.length !== 2) {
  throw new Error(`expected maxAttempts 0 to make one resolve and one data-plane call, got ${zeroMaxFetchCalls.length}`);
}

const onlyStaleFetchCalls: Array<{ input: string; init: { method?: string; headers?: Record<string, string>; body?: Uint8Array } }> = [];
const onlyStaleFetchImpl = async (input: string, init: { method?: string; headers?: Record<string, string>; body?: Uint8Array }) => {
  onlyStaleFetchCalls.push({ input, init });
  if (init.method === "GET") {
    return {
      status: 200,
      async arrayBuffer(): Promise<ArrayBufferLike> {
        return new Uint8Array().buffer;
      },
      async text(): Promise<string> {
        return JSON.stringify([
          {
            name: "route",
            relay_url: "http://only-stale.example/base/",
            crm_ns: "test.contract.fastdb",
            crm_name: "FastdbPortable",
            crm_ver: "0.1.0",
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            max_payload_size: 1048576,
          },
        ]);
      },
    };
  }
  return {
    status: 502,
    async arrayBuffer(): Promise<ArrayBufferLike> {
      return new Uint8Array().buffer;
    },
    async text(): Promise<string> {
      return JSON.stringify({ error: "UpstreamUnavailable" });
    },
  };
};
const onlyStaleWire = createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
  fetch: onlyStaleFetchImpl,
  maxAttempts: 3,
});
let onlyStaleThrown = false;
try {
  await onlyStaleWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([7]));
} catch (error) {
  onlyStaleThrown = true;
  if (!String(error).includes("status 502")) {
    throw new Error(`unexpected only-stale route error ${String(error)}`);
  }
}
if (!onlyStaleThrown) {
  throw new Error("relay-aware transport accepted an only-stale route set");
}
if (onlyStaleFetchCalls.length !== 3) {
  throw new Error(`expected resolve, stale call, and one refreshed resolve for only-stale route, got ${onlyStaleFetchCalls.length}`);
}

const noCacheFetchCalls: Array<{ input: string; init: { method?: string; headers?: Record<string, string>; body?: Uint8Array } }> = [];
const noCacheFetchImpl = async (input: string, init: { method?: string; headers?: Record<string, string>; body?: Uint8Array }) => {
  noCacheFetchCalls.push({ input, init });
  if (init.method === "GET") {
    return {
      status: 200,
      async arrayBuffer(): Promise<ArrayBufferLike> {
        return new Uint8Array().buffer;
      },
      async text(): Promise<string> {
        return JSON.stringify([
          {
            name: "route",
            relay_url: "http://nocache.example/base/",
            crm_ns: "test.contract.fastdb",
            crm_name: "FastdbPortable",
            crm_ver: "0.1.0",
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            max_payload_size: 1048576,
          },
        ]);
      },
    };
  }
  return {
    status: 200,
    async arrayBuffer(): Promise<ArrayBufferLike> {
      return (init.body ?? new Uint8Array()).buffer;
    },
    async text(): Promise<string> {
      return "";
    },
  };
};
const noCacheWire = createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
  fetch: noCacheFetchImpl,
  routeCacheTtlMs: 0,
});
await noCacheWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
await noCacheWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([2]));
if (noCacheFetchCalls.length !== 4) {
  throw new Error(`expected disabled route cache to resolve both calls, got ${noCacheFetchCalls.length} fetch calls`);
}
let badCacheTtlThrown = false;
try {
  createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
    fetch: noCacheFetchImpl,
    routeCacheTtlMs: -1,
  });
} catch (error) {
  badCacheTtlThrown = true;
  if (!String(error).includes("routeCacheTtlMs")) {
    throw new Error(`unexpected routeCacheTtlMs error ${String(error)}`);
  }
}
if (!badCacheTtlThrown) {
  throw new Error("relay-aware transport accepted a negative routeCacheTtlMs");
}
let fractionalCacheTtlThrown = false;
try {
  createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
    fetch: noCacheFetchImpl,
    routeCacheTtlMs: 0.5,
  });
} catch (error) {
  fractionalCacheTtlThrown = true;
  if (!String(error).includes("routeCacheTtlMs")) {
    throw new Error(`unexpected fractional routeCacheTtlMs error ${String(error)}`);
  }
}
if (!fractionalCacheTtlThrown) {
  throw new Error("relay-aware transport accepted a fractional routeCacheTtlMs");
}

const resolveTimeoutWire = createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
  fetch: neverFetchImpl,
  resolveTimeoutMs: 1,
});
let resolveTimeoutThrown = false;
try {
  await resolveTimeoutWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  resolveTimeoutThrown = true;
  if (!String(error).includes("timed out after 1ms")) {
    throw new Error(`unexpected resolve timeout error ${String(error)}`);
  }
}
if (!resolveTimeoutThrown) {
  throw new Error("relay-aware transport accepted a hanging resolve without timeout");
}
let badResolveTimeoutThrown = false;
try {
  createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
    fetch: relayFetchImpl,
    resolveTimeoutMs: -1,
  });
} catch (error) {
  badResolveTimeoutThrown = true;
  if (!String(error).includes("resolveTimeoutMs")) {
    throw new Error(`unexpected resolveTimeoutMs error ${String(error)}`);
  }
}
if (!badResolveTimeoutThrown) {
  throw new Error("relay-aware transport accepted a negative resolveTimeoutMs");
}
let fractionalResolveTimeoutThrown = false;
try {
  createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
    fetch: relayFetchImpl,
    resolveTimeoutMs: 0.5,
  });
} catch (error) {
  fractionalResolveTimeoutThrown = true;
  if (!String(error).includes("resolveTimeoutMs")) {
    throw new Error(`unexpected fractional resolveTimeoutMs error ${String(error)}`);
  }
}
if (!fractionalResolveTimeoutThrown) {
  throw new Error("relay-aware transport accepted a fractional resolveTimeoutMs");
}

const resolveRetryFetchCalls: Array<{ input: string; init: { method?: string; headers?: Record<string, string>; body?: Uint8Array } }> = [];
let resolveRetryAttempts = 0;
const resolveRetryFetchImpl = async (input: string, init: { method?: string; headers?: Record<string, string>; body?: Uint8Array }) => {
  resolveRetryFetchCalls.push({ input, init });
  if (init.method === "GET") {
    resolveRetryAttempts += 1;
    if (resolveRetryAttempts === 1) {
      return {
        status: 503,
        async arrayBuffer(): Promise<ArrayBufferLike> {
          return new Uint8Array().buffer;
        },
        async text(): Promise<string> {
          return JSON.stringify({ error: "RelayTemporarilyUnavailable" });
        },
      };
    }
    return {
      status: 200,
      async arrayBuffer(): Promise<ArrayBufferLike> {
        return new Uint8Array().buffer;
      },
      async text(): Promise<string> {
        return JSON.stringify([
          {
            name: "route",
            relay_url: "http://resolve-retry.example/base/",
            crm_ns: "test.contract.fastdb",
            crm_name: "FastdbPortable",
            crm_ver: "0.1.0",
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            max_payload_size: 1048576,
          },
        ]);
      },
    };
  }
  return {
    status: 200,
    async arrayBuffer(): Promise<ArrayBufferLike> {
      return (init.body ?? new Uint8Array()).buffer;
    },
    async text(): Promise<string> {
      return "";
    },
  };
};
const resolveRetryWire = createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
  fetch: resolveRetryFetchImpl,
  maxAttempts: 2,
});
const resolveRetryResult = await resolveRetryWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([5]));
if (resolveRetryResult[0] !== 5) {
  throw new Error("relay-aware transport did not return response bytes after resolve retry");
}
if (resolveRetryFetchCalls.length !== 3) {
  throw new Error(`expected failed resolve, retried resolve, and data-plane call, got ${resolveRetryFetchCalls.length}`);
}
if (resolveRetryFetchCalls[0].input !== resolveRetryFetchCalls[1].input) {
  throw new Error("resolve retry did not repeat the same contract-scoped resolve URL");
}
if (resolveRetryFetchCalls[2].input !== "http://resolve-retry.example/base/route/query") {
  throw new Error(`unexpected resolve-retry data-plane URL ${resolveRetryFetchCalls[2].input}`);
}

const resolveErrorBodyFetchImpl = async (_input: string, init: { method?: string; headers?: Record<string, string>; body?: Uint8Array }) => {
  if (init.method === "GET") {
    return {
      status: 503,
      async arrayBuffer(): Promise<ArrayBufferLike> {
        return new Uint8Array().buffer;
      },
      async text(): Promise<string> {
        throw new Error("resolve error body unavailable");
      },
    };
  }
  throw new Error("resolve error-body smoke should not reach data-plane fetch");
};
const resolveErrorBodyWire = createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
  fetch: resolveErrorBodyFetchImpl,
  maxAttempts: 1,
});
let resolveErrorBodyThrown = false;
try {
  await resolveErrorBodyWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([5]));
} catch (error) {
  resolveErrorBodyThrown = true;
  if (!(error instanceof C2HttpTransportError)) {
    throw new Error(`expected C2HttpTransportError for resolve error-body read failure, got ${String(error)}`);
  }
  if (!String(error).includes("C-Two relay resolve body read failed")) {
    throw new Error(`unexpected resolve error-body read failure ${String(error)}`);
  }
}
if (!resolveErrorBodyThrown) {
  throw new Error("relay-aware transport accepted a resolve error response whose body could not be read");
}

let oversizedResolveErrorTextCalls = 0;
const oversizedResolveErrorFetchImpl = async (_input: string, init: { method?: string; headers?: Record<string, string>; body?: Uint8Array }) => {
  if (init.method === "GET") {
    return {
      status: 503,
      headers: {
        get(name: string): string | null {
          return name.toLowerCase() === "content-length" ? "2147483648" : null;
        },
      },
      async arrayBuffer(): Promise<ArrayBufferLike> {
        return new Uint8Array().buffer;
      },
      async text(): Promise<string> {
        oversizedResolveErrorTextCalls += 1;
        throw new Error("oversized resolve error text should fail before text()");
      },
    };
  }
  throw new Error("oversized resolve error-body smoke should not reach data-plane fetch");
};
let oversizedResolveErrorThrown = false;
try {
  await createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
    fetch: oversizedResolveErrorFetchImpl,
    maxAttempts: 1,
  }).call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([5]));
} catch (error) {
  oversizedResolveErrorThrown = true;
  if (!(error instanceof C2HttpTransportError) || !String(error).includes("Content-Length") || !String(error).includes("no greater than")) {
    throw new Error(`unexpected oversized resolve error text body error ${String(error)}`);
  }
}
if (!oversizedResolveErrorThrown) {
  throw new Error("relay-aware transport accepted an oversized resolve error text body");
}
if (oversizedResolveErrorTextCalls !== 0) {
  throw new Error(`relay-aware transport read oversized resolve error text ${oversizedResolveErrorTextCalls} time(s)`);
}

let oversizedResolveSuccessTextCalls = 0;
const oversizedResolveSuccessFetchImpl = async (_input: string, init: { method?: string; headers?: Record<string, string>; body?: Uint8Array }) => {
  if (init.method === "GET") {
    return {
      status: 200,
      headers: {
        get(name: string): string | null {
          return name.toLowerCase() === "content-length" ? "2147483648" : null;
        },
      },
      async arrayBuffer(): Promise<ArrayBufferLike> {
        return new Uint8Array().buffer;
      },
      async text(): Promise<string> {
        oversizedResolveSuccessTextCalls += 1;
        throw new Error("oversized resolve success text should fail before text()");
      },
    };
  }
  throw new Error("oversized resolve success-body smoke should not reach data-plane fetch");
};
let oversizedResolveSuccessThrown = false;
try {
  await createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
    fetch: oversizedResolveSuccessFetchImpl,
    maxAttempts: 1,
  }).call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([5]));
} catch (error) {
  oversizedResolveSuccessThrown = true;
  if (!(error instanceof C2HttpTransportError) || !String(error).includes("Content-Length") || !String(error).includes("no greater than")) {
    throw new Error(`unexpected oversized resolve success text body error ${String(error)}`);
  }
}
if (!oversizedResolveSuccessThrown) {
  throw new Error("relay-aware transport accepted an oversized resolve success text body");
}
if (oversizedResolveSuccessTextCalls !== 0) {
  throw new Error(`relay-aware transport read oversized resolve success text ${oversizedResolveSuccessTextCalls} time(s)`);
}

const contractCacheFetchCalls: Array<{ input: string; init: { method?: string; headers?: Record<string, string>; body?: Uint8Array } }> = [];
const alternateContract = {
  ...FASTDB_PORTABLE_CONTRACT,
  signatureHash: "1111111111111111111111111111111111111111111111111111111111111111",
};
const contractCacheFetchImpl = async (input: string, init: { method?: string; headers?: Record<string, string>; body?: Uint8Array }) => {
  contractCacheFetchCalls.push({ input, init });
  const signatureHash = init.method === "GET"
    ? (
      input.includes(alternateContract.signatureHash)
        ? alternateContract.signatureHash
        : FASTDB_PORTABLE_CONTRACT.signatureHash
    )
    : init.headers?.["x-c2-expected-signature-hash"];
  if (init.method === "GET") {
    return {
      status: 200,
      async arrayBuffer(): Promise<ArrayBufferLike> {
        return new Uint8Array().buffer;
      },
      async text(): Promise<string> {
        return JSON.stringify([
          {
            name: "route",
            relay_url: "http://contract-cache.example/base/",
            crm_ns: "test.contract.fastdb",
            crm_name: "FastdbPortable",
            crm_ver: "0.1.0",
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            signature_hash: signatureHash,
            max_payload_size: 1048576,
          },
        ]);
      },
    };
  }
  if (init.headers?.["x-c2-expected-signature-hash"] !== signatureHash) {
    throw new Error("data-plane call reused a route cache entry from another signature hash");
  }
  return {
    status: 200,
    async arrayBuffer(): Promise<ArrayBufferLike> {
      return (init.body ?? new Uint8Array()).buffer;
    },
    async text(): Promise<string> {
      return "";
    },
  };
};
const contractCacheWire = createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
  fetch: contractCacheFetchImpl,
});
await contractCacheWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
await contractCacheWire.call("route", alternateContract, "query", new Uint8Array([2]));
if (contractCacheFetchCalls.length !== 4) {
  throw new Error(`expected same route with different contract hash to resolve twice, got ${contractCacheFetchCalls.length} fetch calls`);
}

const fatalFetchCalls: Array<{ input: string; init: { method?: string; headers?: Record<string, string>; body?: Uint8Array } }> = [];
const fatalFetchImpl = async (input: string, init: { method?: string; headers?: Record<string, string>; body?: Uint8Array }) => {
  fatalFetchCalls.push({ input, init });
  if (init.method === "GET") {
    return {
      status: 200,
      async arrayBuffer(): Promise<ArrayBufferLike> {
        return new Uint8Array().buffer;
      },
      async text(): Promise<string> {
        return JSON.stringify([
          {
            name: "route",
            relay_url: "http://fatal.example/base/",
            crm_ns: "test.contract.fastdb",
            crm_name: "FastdbPortable",
            crm_ver: "0.1.0",
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            max_payload_size: 1048576,
          },
        ]);
      },
    };
  }
  return {
    status: 502,
    async arrayBuffer(): Promise<ArrayBufferLike> {
      return new Uint8Array().buffer;
    },
    async text(): Promise<string> {
      return JSON.stringify({ error: "RelayRemotePayloadChunkConfigInvalid" });
    },
  };
};
const fatalWire = createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
  fetch: fatalFetchImpl,
  maxAttempts: 2,
});
let fatalThrown = false;
try {
  await fatalWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([9]));
} catch (error) {
  fatalThrown = true;
  if (!String(error).includes("status 502")) {
    throw new Error(`unexpected fatal relay error ${String(error)}`);
  }
}
if (!fatalThrown) {
  throw new Error("relay-aware HTTP transport retried or swallowed a non-stale 502 data-plane error");
}
if (fatalFetchCalls.length !== 2) {
  throw new Error(`expected one resolve and one data-plane call for non-stale 502, got ${fatalFetchCalls.length}`);
}

const payloadLimitFetchCalls: Array<{ input: string; init: { method?: string; headers?: Record<string, string>; body?: Uint8Array } }> = [];
const payloadLimitFetchImpl = async (input: string, init: { method?: string; headers?: Record<string, string>; body?: Uint8Array }) => {
  payloadLimitFetchCalls.push({ input, init });
  if (init.method === "GET") {
    return {
      status: 200,
      async arrayBuffer(): Promise<ArrayBufferLike> {
        return new Uint8Array().buffer;
      },
      async text(): Promise<string> {
        return JSON.stringify([
          {
            name: "route",
            relay_url: "http://too-small.example/base/",
            crm_ns: "test.contract.fastdb",
            crm_name: "FastdbPortable",
            crm_ver: "0.1.0",
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            max_payload_size: 1,
          },
          {
            name: "route",
            relay_url: "http://large-enough.example/base/",
            crm_ns: "test.contract.fastdb",
            crm_name: "FastdbPortable",
            crm_ver: "0.1.0",
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            max_payload_size: 8,
          },
        ]);
      },
    };
  }
  if (input.startsWith("http://too-small.example/base/")) {
    throw new Error("relay-aware transport called a route whose max_payload_size is too small");
  }
  return {
    status: 200,
    async arrayBuffer(): Promise<ArrayBufferLike> {
      return (init.body ?? new Uint8Array()).buffer;
    },
    async text(): Promise<string> {
      return "";
    },
  };
};
const payloadLimitWire = createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
  fetch: payloadLimitFetchImpl,
  maxAttempts: 1,
});
const payloadLimitResult = await payloadLimitWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([4, 5]));
if (payloadLimitResult[0] !== 4 || payloadLimitResult[1] !== 5) {
  throw new Error("relay-aware transport did not call the large-enough payload route");
}
if (payloadLimitFetchCalls.length !== 2) {
  throw new Error(`expected resolve plus one large-enough data-plane call, got ${payloadLimitFetchCalls.length}`);
}
if (payloadLimitFetchCalls[1].input !== "http://large-enough.example/base/route/query") {
  throw new Error(`unexpected payload-limit candidate URL ${payloadLimitFetchCalls[1].input}`);
}

const oversizedFetchCalls: Array<{ input: string; init: { method?: string; headers?: Record<string, string>; body?: Uint8Array } }> = [];
const oversizedFetchImpl = async (input: string, init: { method?: string; headers?: Record<string, string>; body?: Uint8Array }) => {
  oversizedFetchCalls.push({ input, init });
  if (init.method !== "GET") {
    throw new Error("relay-aware transport sent a data-plane call after every candidate exceeded max_payload_size");
  }
  return {
    status: 200,
    async arrayBuffer(): Promise<ArrayBufferLike> {
      return new Uint8Array().buffer;
    },
    async text(): Promise<string> {
      return JSON.stringify([
        {
          name: "route",
          relay_url: "http://tiny.example/base/",
          crm_ns: "test.contract.fastdb",
          crm_name: "FastdbPortable",
          crm_ver: "0.1.0",
          abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
          signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
          max_payload_size: 1,
        },
      ]);
    },
  };
};
const oversizedWire = createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
  fetch: oversizedFetchImpl,
  maxAttempts: 3,
});
let oversizedThrown = false;
try {
  await oversizedWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([4, 5]));
} catch (error) {
  oversizedThrown = true;
  if (!String(error).includes("status 413")) {
    throw new Error(`unexpected oversized payload error ${String(error)}`);
  }
}
if (!oversizedThrown) {
  throw new Error("relay-aware transport accepted a payload larger than every candidate max_payload_size");
}
if (oversizedFetchCalls.length !== 1) {
  throw new Error(`expected only one resolve call for oversized payload, got ${oversizedFetchCalls.length}`);
}

const malformedLimitFetchCalls: Array<{ input: string; init: { method?: string; headers?: Record<string, string>; body?: Uint8Array } }> = [];
const malformedLimitFetchImpl = async (input: string, init: { method?: string; headers?: Record<string, string>; body?: Uint8Array }) => {
  malformedLimitFetchCalls.push({ input, init });
  if (init.method !== "GET") {
    throw new Error("relay-aware transport sent a data-plane call after a malformed max_payload_size");
  }
  return {
    status: 200,
    async arrayBuffer(): Promise<ArrayBufferLike> {
      return new Uint8Array().buffer;
    },
    async text(): Promise<string> {
      return JSON.stringify([
        {
          name: "route",
          relay_url: "http://malformed.example/base/",
          crm_ns: "test.contract.fastdb",
          crm_name: "FastdbPortable",
          crm_ver: "0.1.0",
          abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
          signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
          max_payload_size: 0,
        },
      ]);
    },
  };
};
const malformedLimitWire = createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
  fetch: malformedLimitFetchImpl,
  maxAttempts: 1,
});
let malformedLimitThrown = false;
try {
  await malformedLimitWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  malformedLimitThrown = true;
  if (!String(error).includes("missing positive integer field max_payload_size")) {
    throw new Error(`unexpected malformed max_payload_size error ${String(error)}`);
  }
}
if (!malformedLimitThrown) {
  throw new Error("relay-aware transport accepted a malformed max_payload_size");
}
if (malformedLimitFetchCalls.length !== 1) {
  throw new Error(`expected only one resolve call for malformed max_payload_size, got ${malformedLimitFetchCalls.length}`);
}

const mismatchedRouteFetchCalls: Array<{ input: string; init: { method?: string; headers?: Record<string, string>; body?: Uint8Array } }> = [];
const mismatchedRouteFetchImpl = async (input: string, init: { method?: string; headers?: Record<string, string>; body?: Uint8Array }) => {
  mismatchedRouteFetchCalls.push({ input, init });
  if (init.method !== "GET") {
    throw new Error("relay-aware transport sent a data-plane call after a mismatched resolve route");
  }
  return {
    status: 200,
    async arrayBuffer(): Promise<ArrayBufferLike> {
      return new Uint8Array().buffer;
    },
    async text(): Promise<string> {
      return JSON.stringify([
        {
          name: "route",
          relay_url: "http://wrong-contract.example/base/",
          crm_ns: "test.contract.fastdb",
          crm_name: "WrongContract",
          crm_ver: "0.1.0",
          abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
          signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
          max_payload_size: 1048576,
        },
      ]);
    },
  };
};
const mismatchedRouteWire = createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
  fetch: mismatchedRouteFetchImpl,
  maxAttempts: 2,
});
let mismatchedRouteThrown = false;
try {
  await mismatchedRouteWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  mismatchedRouteThrown = true;
  if (!String(error).includes("does not match the expected CRM contract")) {
    throw new Error(`unexpected mismatched route error ${String(error)}`);
  }
}
if (!mismatchedRouteThrown) {
  throw new Error("relay-aware transport accepted a mismatched resolve route");
}
if (mismatchedRouteFetchCalls.length !== 1) {
  throw new Error(`expected only one resolve call for mismatched resolve route, got ${mismatchedRouteFetchCalls.length}`);
}

const invalidResolvedRelayUrlFetchCalls: Array<{ input: string; init: { method?: string; headers?: Record<string, string>; body?: Uint8Array } }> = [];
const invalidResolvedRelayUrlFetchImpl = async (input: string, init: { method?: string; headers?: Record<string, string>; body?: Uint8Array }) => {
  invalidResolvedRelayUrlFetchCalls.push({ input, init });
  if (init.method !== "GET") {
    throw new Error(`invalid resolved relay URL case reached data-plane fetch ${input}`);
  }
  return {
    status: 200,
    async arrayBuffer(): Promise<ArrayBufferLike> {
      return new Uint8Array().buffer;
    },
    async text(): Promise<string> {
      return JSON.stringify([
        {
          name: "route",
          relay_url: "/not-an-absolute-relay-url",
          crm_ns: "test.contract.fastdb",
          crm_name: "FastdbPortable",
          crm_ver: "0.1.0",
          abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
          signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
          max_payload_size: 1048576,
        },
      ]);
    },
  };
};
const invalidResolvedRelayUrlWire = createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
  fetch: invalidResolvedRelayUrlFetchImpl,
  maxAttempts: 1,
});
let invalidResolvedRelayUrlThrown = false;
try {
  await invalidResolvedRelayUrlWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  invalidResolvedRelayUrlThrown = true;
  if (!String(error).includes("payload shape invalid") || !String(error).includes("HTTP(S)")) {
    throw new Error(`unexpected invalid resolved relay URL error ${String(error)}`);
  }
}
if (!invalidResolvedRelayUrlThrown) {
  throw new Error("relay-aware transport accepted an invalid resolved relay URL");
}
if (invalidResolvedRelayUrlFetchCalls.length !== 1) {
  throw new Error(`expected only one resolve call for invalid resolved relay URL, got ${invalidResolvedRelayUrlFetchCalls.length}`);
}

const whitespaceResolvedRelayUrlFetchCalls: Array<{ input: string; init: { method?: string; headers?: Record<string, string>; body?: Uint8Array } }> = [];
const whitespaceResolvedRelayUrlFetchImpl = async (input: string, init: { method?: string; headers?: Record<string, string>; body?: Uint8Array }) => {
  whitespaceResolvedRelayUrlFetchCalls.push({ input, init });
  if (init.method !== "GET") {
    throw new Error(`whitespace resolved relay URL case reached data-plane fetch ${input}`);
  }
  return {
    status: 200,
    async arrayBuffer(): Promise<ArrayBufferLike> {
      return new Uint8Array().buffer;
    },
    async text(): Promise<string> {
      return JSON.stringify([
        {
          name: "route",
          relay_url: "http://whitespace.example/base ",
          crm_ns: "test.contract.fastdb",
          crm_name: "FastdbPortable",
          crm_ver: "0.1.0",
          abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
          signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
          max_payload_size: 1048576,
        },
      ]);
    },
  };
};
const whitespaceResolvedRelayUrlWire = createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
  fetch: whitespaceResolvedRelayUrlFetchImpl,
  maxAttempts: 1,
});
let whitespaceResolvedRelayUrlThrown = false;
try {
  await whitespaceResolvedRelayUrlWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  whitespaceResolvedRelayUrlThrown = true;
  if (!String(error).includes("payload shape invalid") || !String(error).includes("whitespace")) {
    throw new Error(`unexpected whitespace resolved relay URL error ${String(error)}`);
  }
}
if (!whitespaceResolvedRelayUrlThrown) {
  throw new Error("relay-aware transport accepted a whitespace-containing resolved relay URL");
}
if (whitespaceResolvedRelayUrlFetchCalls.length !== 1) {
  throw new Error(`expected only one resolve call for whitespace resolved relay URL, got ${whitespaceResolvedRelayUrlFetchCalls.length}`);
}

const credentialsResolvedRelayUrlFetchCalls: Array<{ input: string; init: { method?: string; headers?: Record<string, string>; body?: Uint8Array } }> = [];
const credentialsResolvedRelayUrlFetchImpl = async (input: string, init: { method?: string; headers?: Record<string, string>; body?: Uint8Array }) => {
  credentialsResolvedRelayUrlFetchCalls.push({ input, init });
  if (init.method !== "GET") {
    throw new Error(`credentials resolved relay URL case reached data-plane fetch ${input}`);
  }
  return {
    status: 200,
    async arrayBuffer(): Promise<ArrayBufferLike> {
      return new Uint8Array().buffer;
    },
    async text(): Promise<string> {
      return JSON.stringify([
        {
          name: "route",
          relay_url: "http://user:pass@credentials.example/base",
          crm_ns: "test.contract.fastdb",
          crm_name: "FastdbPortable",
          crm_ver: "0.1.0",
          abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
          signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
          max_payload_size: 1048576,
        },
      ]);
    },
  };
};
const credentialsResolvedRelayUrlWire = createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
  fetch: credentialsResolvedRelayUrlFetchImpl,
  maxAttempts: 1,
});
let credentialsResolvedRelayUrlThrown = false;
try {
  await credentialsResolvedRelayUrlWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
} catch (error) {
  credentialsResolvedRelayUrlThrown = true;
  if (!String(error).includes("payload shape invalid") || !String(error).includes("credentials")) {
    throw new Error(`unexpected credentials resolved relay URL error ${String(error)}`);
  }
}
if (!credentialsResolvedRelayUrlThrown) {
  throw new Error("relay-aware transport accepted a credentials-containing resolved relay URL");
}
if (credentialsResolvedRelayUrlFetchCalls.length !== 1) {
  throw new Error(`expected only one resolve call for credentials resolved relay URL, got ${credentialsResolvedRelayUrlFetchCalls.length}`);
}

const cacheScopedContract = {
  ...FASTDB_PORTABLE_CONTRACT,
  name: "FastdbPortableAlt",
  signatureHash: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
};
const cacheScopeFetchCalls: Array<{ input: string; init: { method?: string; headers?: Record<string, string>; body?: Uint8Array } }> = [];
let cacheScopeADataPlaneCalls = 0;
const cacheScopeFetchImpl = async (input: string, init: { method?: string; headers?: Record<string, string>; body?: Uint8Array }) => {
  cacheScopeFetchCalls.push({ input, init });
  if (init.method === "GET") {
    const isAlt = input.includes("crm_name=FastdbPortableAlt");
    return {
      status: 200,
      async arrayBuffer(): Promise<ArrayBufferLike> {
        return new Uint8Array().buffer;
      },
      async text(): Promise<string> {
        return JSON.stringify([
          {
            name: "route",
            relay_url: isAlt ? "http://contract-b.example/base/" : "http://contract-a.example/base/",
            crm_ns: "test.contract.fastdb",
            crm_name: isAlt ? "FastdbPortableAlt" : "FastdbPortable",
            crm_ver: "0.1.0",
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            signature_hash: isAlt
              ? "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
              : "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            max_payload_size: 1048576,
          },
        ]);
      },
    };
  }
  if (input.startsWith("http://contract-a.example/base/")) {
    cacheScopeADataPlaneCalls += 1;
    if (cacheScopeADataPlaneCalls >= 2) {
      return {
        status: 404,
        async arrayBuffer(): Promise<ArrayBufferLike> {
          return new Uint8Array().buffer;
        },
        async text(): Promise<string> {
          return JSON.stringify({ error: "ResourceNotFound" });
        },
      };
    }
  }
  return {
    status: 200,
    async arrayBuffer(): Promise<ArrayBufferLike> {
      return (init.body ?? new Uint8Array()).buffer;
    },
    async text(): Promise<string> {
      return "";
    },
  };
};
const cacheScopeWire = createRelayAwareHttpEncodedTransport("http://anchor.example/root/", {
  fetch: cacheScopeFetchImpl,
  maxAttempts: 2,
  routeCacheTtlMs: 60_000,
});
await cacheScopeWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([1]));
await cacheScopeWire.call("route", cacheScopedContract, "query", new Uint8Array([2]));
const cacheScopeResolveCountAfterWarmup = cacheScopeFetchCalls.filter((call) => call.init.method === "GET").length;
if (cacheScopeResolveCountAfterWarmup !== 2) {
  throw new Error(`expected two contract-scoped resolve calls after warmup, got ${cacheScopeResolveCountAfterWarmup}`);
}
let staleCacheScopeThrown = false;
try {
  await cacheScopeWire.call("route", FASTDB_PORTABLE_CONTRACT, "query", new Uint8Array([3]));
} catch (error) {
  staleCacheScopeThrown = true;
  if (!String(error).includes("ResourceNotFound")) {
    throw new Error(`unexpected stale cache-scope route error ${String(error)}`);
  }
}
if (!staleCacheScopeThrown) {
  throw new Error("cache-scope stale route did not throw");
}
const cacheScopeResolveCountAfterStale = cacheScopeFetchCalls.filter((call) => call.init.method === "GET").length;
await cacheScopeWire.call("route", cacheScopedContract, "query", new Uint8Array([4]));
const cacheScopeResolveCountAfterAltReuse = cacheScopeFetchCalls.filter((call) => call.init.method === "GET").length;
if (cacheScopeResolveCountAfterAltReuse !== cacheScopeResolveCountAfterStale) {
  throw new Error("stale route invalidation for one CRM contract evicted another contract cache entry with the same route name");
}
"#,
    )
    .unwrap();

    let tsc_status = std::process::Command::new("tsc")
        .args([
            "--noEmit",
            "--strict",
            "--target",
            "ES2022",
            "--module",
            "NodeNext",
            "--moduleResolution",
            "NodeNext",
            output.to_str().unwrap(),
            smoke.to_str().unwrap(),
        ])
        .current_dir(tempdir.path())
        .status()
        .unwrap();
    assert!(tsc_status.success());

    let emit_status = std::process::Command::new("tsc")
        .args([
            "--strict",
            "--target",
            "ES2022",
            "--module",
            "NodeNext",
            "--moduleResolution",
            "NodeNext",
            output.to_str().unwrap(),
            smoke.to_str().unwrap(),
        ])
        .current_dir(tempdir.path())
        .status()
        .unwrap();
    assert!(emit_status.success());

    let node_status = std::process::Command::new("node")
        .arg(tempdir.path().join("smoke.js"))
        .current_dir(tempdir.path())
        .status()
        .unwrap();
    assert!(node_status.success());
}
