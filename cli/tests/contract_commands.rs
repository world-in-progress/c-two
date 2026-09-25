use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

const RELEASE_DESCRIPTOR: &str =
    include_str!("../../tests/fixtures/contracts/portable-release.contract.json");
const RELEASE_REFERENCE: &str =
    include_str!("../../tests/fixtures/contracts/portable-release.ref.json");
const MATRIX_DESCRIPTORS: [(&str, &str, Option<&str>); 3] = [
    (
        "no-payload",
        include_str!("../../tests/fixtures/contracts/portable-no-payload.contract.json"),
        None,
    ),
    (
        "record-v1",
        include_str!("../../tests/fixtures/contracts/portable-record-v1.contract.json"),
        Some("record.v1"),
    ),
    (
        "object-graph-v1",
        include_str!("../../tests/fixtures/contracts/portable-object-graph-v1.contract.json"),
        Some("object_graph.v1"),
    ),
];

#[test]
fn contract_codegen_portable_project_tree() {
    let tempdir = tempfile::tempdir().unwrap();
    let contract = tempdir.path().join("portable.contract.json");
    std::fs::write(&contract, RELEASE_DESCRIPTOR).unwrap();

    for (target, module) in [
        ("rust", "rust/c_two_contract.rs"),
        ("python", "python/c_two_contract.py"),
        ("typescript", "typescript/c_two_contract.ts"),
    ] {
        let destination = tempdir.path().join(format!("generated-{target}"));
        let mut command = Command::cargo_bin("c3").unwrap();
        command
            .args([
                "contract",
                "codegen",
                target,
                contract.to_str().unwrap(),
                "--out-dir",
                destination.to_str().unwrap(),
            ])
            .assert()
            .success()
            .stdout(predicate::str::is_empty());

        assert!(destination.join("metadata/contract.json").is_file());
        assert!(
            destination
                .join("metadata/contract-release-ref.json")
                .is_file()
        );
        assert!(
            destination
                .join("metadata/composition-manifest.json")
                .is_file()
        );
        assert!(destination.join(module).is_file());
        assert!(!walk_files(&destination.join(target).join("payloads")).is_empty());

        let mut repeat = Command::cargo_bin("c3").unwrap();
        repeat
            .args([
                "contract",
                "codegen",
                target,
                contract.to_str().unwrap(),
                "--out-dir",
                destination.to_str().unwrap(),
            ])
            .assert()
            .failure()
            .stderr(predicate::str::contains(
                "artifact destination already exists",
            ));
    }
}

#[test]
fn portable_matrix_descriptors_codegen_through_one_cli_path() {
    let tempdir = tempfile::tempdir().unwrap();

    for (payload, descriptor, fastdb_profile) in MATRIX_DESCRIPTORS {
        let descriptor_value: serde_json::Value =
            serde_json::from_str(descriptor).expect("matrix descriptor JSON");
        let methods = descriptor_value["methods"]
            .as_array()
            .expect("matrix methods");
        assert_eq!(methods.len(), 1, "{payload}");
        match fastdb_profile {
            Some(profile) => {
                assert_eq!(
                    methods[0]["bindings"]["input"]["spec"]["profile"], profile,
                    "{payload}",
                );
                assert_eq!(
                    methods[0]["bindings"]["output"]["spec"]["profile"], profile,
                    "{payload}",
                );
            }
            None => {
                assert!(methods[0]["bindings"]["input"].is_null(), "{payload}");
                assert!(methods[0]["bindings"]["output"].is_null(), "{payload}");
            }
        }

        let descriptor_path = tempdir.path().join(format!("{payload}.contract.json"));
        std::fs::write(&descriptor_path, descriptor).unwrap();
        let mut canonical = Vec::new();
        let mut release_refs = Vec::new();
        for target in ["rust", "python"] {
            let destination = tempdir.path().join(format!("{payload}-{target}"));
            let mut command = Command::cargo_bin("c3").unwrap();
            command
                .args([
                    "contract",
                    "codegen",
                    target,
                    descriptor_path.to_str().unwrap(),
                    "--out-dir",
                    destination.to_str().unwrap(),
                ])
                .assert()
                .success()
                .stdout(predicate::str::is_empty());
            canonical.push(std::fs::read(destination.join("metadata/contract.json")).unwrap());
            release_refs.push(
                std::fs::read(destination.join("metadata/contract-release-ref.json")).unwrap(),
            );
            assert_eq!(
                destination.join(target).join("payloads").exists(),
                fastdb_profile.is_some(),
                "{payload} {target}",
            );
        }
        assert_eq!(canonical[0], canonical[1], "{payload}");
        assert_eq!(release_refs[0], release_refs[1], "{payload}");
    }
}

#[test]
fn contract_codegen_stdin_preserves_fastdb_cause_and_publishes_nothing_on_failure() {
    let tempdir = tempfile::tempdir().unwrap();
    let destination = tempdir.path().join("must-not-exist");
    let mut descriptor: serde_json::Value = serde_json::from_str(RELEASE_DESCRIPTOR).unwrap();
    descriptor["methods"][1]["bindings"]["input"]["spec"] = serde_json::json!({"schema": "wrong"});
    let fingerprints =
        c2_contract::derive_contract_fingerprints_json(descriptor.to_string().as_bytes()).unwrap();
    descriptor["fingerprints"]["abi_hash"] =
        serde_json::Value::String(fingerprints.abi_hash().to_string());
    descriptor["fingerprints"]["signature_hash"] =
        serde_json::Value::String(fingerprints.signature_hash().to_string());

    let mut command = Command::cargo_bin("c3").unwrap();
    command
        .args([
            "contract",
            "codegen",
            "python",
            "-",
            "--out-dir",
            destination.to_str().unwrap(),
        ])
        .write_stdin(descriptor.to_string())
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "FastDB failed for nested binding $.methods[1].bindings.input.spec",
        ));
    assert!(!destination.exists());
}

fn walk_files(root: &Path) -> Vec<std::path::PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(path) = pending.pop() {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let file_type = entry.file_type().unwrap();
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    files
}

fn valid_contract_json() -> String {
    RELEASE_DESCRIPTOR.to_string()
}

fn invalid_pickle_contract_json() -> String {
    let mut descriptor: serde_json::Value = serde_json::from_str(RELEASE_DESCRIPTOR).unwrap();
    descriptor["methods"][1]["bindings"]["input"]["kind"] =
        serde_json::Value::String("python-pickle-default".to_string());
    serde_json::to_string(&descriptor).unwrap()
}

#[cfg(unix)]
struct FakePython {
    pythonpath: PathBuf,
}

#[cfg(unix)]
impl FakePython {
    fn executable(&self) -> &'static str {
        "python3"
    }

    fn pythonpath(&self) -> &Path {
        &self.pythonpath
    }
}

#[cfg(unix)]
fn write_fake_python_package(tempdir: &tempfile::TempDir) -> PathBuf {
    let cli_package = tempdir.path().join("c_two/cli");
    std::fs::create_dir_all(&cli_package).unwrap();
    std::fs::write(tempdir.path().join("c_two/__init__.py"), "").unwrap();
    std::fs::write(cli_package.join("__init__.py"), "").unwrap();
    cli_package
}

#[cfg(unix)]
fn fake_python(tempdir: &tempfile::TempDir, payload: &str) -> FakePython {
    let cli_package = write_fake_python_package(tempdir);
    std::fs::write(cli_package.join("payload.txt"), payload).unwrap();
    std::fs::write(
        cli_package.join("contract.py"),
        r#"from pathlib import Path

print(Path(__file__).with_name("payload.txt").read_text(encoding="utf-8"), end="")
"#,
    )
    .unwrap();
    FakePython {
        pythonpath: tempdir.path().to_path_buf(),
    }
}

#[cfg(unix)]
fn fake_python_requiring_arg(
    tempdir: &tempfile::TempDir,
    required_arg: &str,
    matched_payload: &str,
    missing_payload: &str,
) -> FakePython {
    let cli_package = write_fake_python_package(tempdir);
    std::fs::write(cli_package.join("matched_payload.txt"), matched_payload).unwrap();
    std::fs::write(cli_package.join("missing_payload.txt"), missing_payload).unwrap();
    std::fs::write(
        cli_package.join("contract.py"),
        format!(
            r#"import sys
from pathlib import Path

payload = "matched_payload.txt" if {required_arg:?} in sys.argv[1:] else "missing_payload.txt"
print(Path(__file__).with_name(payload).read_text(encoding="utf-8"), end="")
"#
        ),
    )
    .unwrap();
    FakePython {
        pythonpath: tempdir.path().to_path_buf(),
    }
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
        .stdout(predicate::str::contains("codegen"))
        .stdout(predicate::str::contains("diagnose"))
        .stdout(predicate::str::contains("export"))
        .stdout(predicate::str::contains("infer"))
        .stdout(predicate::str::contains("validate"));
}

#[test]
fn contract_help_omits_removed_sidecar_artifact_commands() {
    let mut contract = Command::cargo_bin("c3").unwrap();
    let output = contract.args(["contract", "--help"]).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        !stdout
            .lines()
            .any(|line| line.split_whitespace().next() == Some("artifacts")),
        "{stdout}",
    );

    let mut infer = Command::cargo_bin("c3").unwrap();
    infer
        .args(["contract", "infer", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--artifacts").not());
}

#[test]
fn contract_help_lists_release_ref() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args(["contract", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("release-ref"));
}

#[cfg(unix)]
#[test]
fn contract_diagnose_wraps_python_and_validates_diagnostic_shape() {
    let tempdir = tempfile::tempdir().unwrap();
    let diagnostics = r#"[{"code":"python_only_pickle","message":"Python-only fallback","method":"echo","position":"input","severity":"warning"}]"#;
    let python = fake_python(&tempdir, diagnostics);
    let output = tempdir.path().join("diagnostics.json");

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.env("PYTHONPATH", python.pythonpath());
    cmd.args([
        "contract",
        "diagnose",
        "example.contracts:Grid",
        "--python",
        python.executable(),
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
    cmd.env("PYTHONPATH", python.pythonpath());
    cmd.args([
        "contract",
        "diagnose",
        "example.contracts:Grid",
        "--python",
        python.executable(),
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
    cmd.env("PYTHONPATH", python.pythonpath());
    cmd.args([
        "contract",
        "diagnose",
        "example.contracts:Grid",
        "--python",
        python.executable(),
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
        .stdout(predicate::str::contains("valid c-two.contract.v2"))
        .stdout(predicate::str::contains("sha256="));
}

#[test]
fn contract_validate_accepts_stdin() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args(["contract", "validate", "-"])
        .write_stdin(valid_contract_json())
        .assert()
        .success()
        .stdout(predicate::str::contains("stdin: valid c-two.contract.v2"));
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
        .stderr(predicate::str::contains(
            "$.methods[1].bindings.input.kind: binding kind must be \"fastdb\"",
        ));
}

#[test]
fn contract_release_ref_accepts_file_and_writes_canonical_output() {
    let tempdir = tempfile::tempdir().unwrap();
    let descriptor = tempdir.path().join("contract.json");
    std::fs::write(&descriptor, RELEASE_DESCRIPTOR).unwrap();

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.env("C2_PYTHON", "c-two-test-python-must-not-run");
    cmd.args(["contract", "release-ref", descriptor.to_str().unwrap()])
        .assert()
        .success()
        .stdout(format!("{}\n", RELEASE_REFERENCE.trim_end()));
}

#[test]
fn contract_release_ref_accepts_stdin_and_pretty_output_file() {
    let tempdir = tempfile::tempdir().unwrap();
    let output = tempdir.path().join("release-ref.json");
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args([
        "contract",
        "release-ref",
        "-",
        "--pretty",
        "--out",
        output.to_str().unwrap(),
    ])
    .write_stdin(RELEASE_DESCRIPTOR)
    .assert()
    .success()
    .stdout(predicate::str::is_empty());

    let written = std::fs::read_to_string(output).unwrap();
    assert!(written.ends_with('\n'));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&written).unwrap(),
        serde_json::from_str::<serde_json::Value>(RELEASE_REFERENCE).unwrap(),
    );
}

#[test]
fn contract_release_ref_rejects_invalid_descriptor() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args(["contract", "release-ref", "-"])
        .write_stdin(r#"{"schema":"not-c-two"}"#)
        .assert()
        .failure()
        .stderr(predicate::str::contains("contract descriptor invalid"));
}

#[cfg(unix)]
#[test]
fn contract_export_wraps_python_and_validates_output() {
    let tempdir = tempfile::tempdir().unwrap();
    let python = fake_python(&tempdir, &valid_contract_json());

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.env("PYTHONPATH", python.pythonpath());
    cmd.args([
        "contract",
        "export",
        "example.contracts:Grid",
        "--python",
        python.executable(),
        "--method",
        "echo",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains(r#""schema": "c-two.contract.v2""#));
}

#[cfg(unix)]
#[test]
fn contract_infer_wraps_python_and_writes_validated_output() {
    let tempdir = tempfile::tempdir().unwrap();
    let python = fake_python(&tempdir, &valid_contract_json());
    let output = tempdir.path().join("inferred.contract.json");

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.env("PYTHONPATH", python.pythonpath());
    cmd.args([
        "contract",
        "infer",
        "example.resources:GridResource",
        "--python",
        python.executable(),
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
    cmd.env("PYTHONPATH", python.pythonpath());
    cmd.args([
        "contract",
        "infer",
        "example.resources:GridResource",
        "--python",
        python.executable(),
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
    cmd.env("PYTHONPATH", python.pythonpath());
    cmd.args([
        "contract",
        "infer",
        "example.resources:GridResource",
        "--python",
        python.executable(),
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
    cmd.env("PYTHONPATH", python.pythonpath());
    cmd.args([
        "contract",
        "infer",
        "example.resources:GridResource",
        "--python",
        python.executable(),
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
fn contract_export_rejects_invalid_python_descriptor() {
    let tempdir = tempfile::tempdir().unwrap();
    let python = fake_python(&tempdir, &invalid_pickle_contract_json());

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.env("PYTHONPATH", python.pythonpath());
    cmd.args([
        "contract",
        "export",
        "example.contracts:Grid",
        "--python",
        python.executable(),
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains(
        "$.methods[1].bindings.input.kind: binding kind must be \"fastdb\"",
    ));
}
