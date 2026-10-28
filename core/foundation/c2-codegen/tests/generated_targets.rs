use c2_codegen::{
    CodegenError, ContractArtifactSet, ContractCodegenOptions, ContractCodegenTarget,
    compile_contract_artifacts as compile_admitted_contract_artifacts,
};
use std::path::Path;
use std::process::Command;

const DESCRIPTOR: &str =
    include_str!("../../../../tests/fixtures/contracts/portable-release.contract.json");

fn compile_contract_artifacts(
    descriptor_json: &[u8],
    target: ContractCodegenTarget,
    options: &ContractCodegenOptions,
) -> Result<ContractArtifactSet, Box<CodegenError>> {
    let release = c2_contract::ContractRelease::from_descriptor_json(descriptor_json)
        .map_err(CodegenError::from)
        .map_err(Box::new)?;
    compile_admitted_contract_artifacts(&release, target, options).map_err(Box::new)
}

fn descriptor_with_method_names(first: &str, second: &str) -> String {
    let mut descriptor: serde_json::Value = serde_json::from_str(DESCRIPTOR).unwrap();
    descriptor["methods"][0]["name"] = serde_json::Value::String(first.to_string());
    descriptor["methods"][1]["name"] = serde_json::Value::String(second.to_string());
    let fingerprints =
        c2_contract::derive_contract_fingerprints_json(descriptor.to_string().as_bytes()).unwrap();
    descriptor["fingerprints"]["abi_hash"] =
        serde_json::Value::String(fingerprints.abi_hash().to_string());
    descriptor["fingerprints"]["signature_hash"] =
        serde_json::Value::String(fingerprints.signature_hash().to_string());
    descriptor.to_string()
}

#[test]
fn every_target_contains_a_c_two_owned_contract_module() {
    for (target, module_path) in [
        (ContractCodegenTarget::Rust, "rust/c_two_contract.rs"),
        (ContractCodegenTarget::Python, "python/c_two_contract.py"),
        (
            ContractCodegenTarget::TypeScript,
            "typescript/c_two_contract.ts",
        ),
    ] {
        let set =
            compile_contract_artifacts(DESCRIPTOR.as_bytes(), target, &Default::default()).unwrap();
        let module = set
            .get(module_path)
            .unwrap_or_else(|| panic!("missing C-Two module {module_path}"));
        let source = std::str::from_utf8(module.bytes()).unwrap();

        assert!(source.contains("c-two.contract.v2"));
        assert!(source.contains("test.contract-release"));
        assert!(source.contains("Portable"));
        assert!(source.contains("ping"));
        assert!(source.contains("echo"));
        assert!(
            source.contains("bec2fee73f9a2476c311de20e40e584d0c7ff6bcadd38c0b4fe3e683ec2540fe")
        );
        assert!(
            source.contains("c4cd2cf04caa63f12f702868524787c95c4f5bc00a7c7212bd1f807b32647940")
        );
        assert!(source.contains("require_spec_sha256") || source.contains("requireSpecSha256"));
        assert!(
            source.contains("open_copy")
                || source.contains("openCopy")
                || source.contains("open_owned")
        );
        assert!(source.contains("binary_bytes") || source.contains("binaryBytes"));
        assert!(!source.contains("record.v1"));
        assert!(!source.contains("object_graph.v1"));
        assert!(!source.contains("fastdb.payload.v1"));
        assert!(!source.contains("fastdb.schema.v1"));
        assert!(!source.contains("columnar.v1"));
        assert!(!source.contains("org.fastdb"));
        assert!(!source.contains("call-db"));
        assert!(!source.contains("call_db"));
        assert!(!source.contains("C2Codec"));
        assert!(!source.contains("codecRequirement"));
        assert!(!source.contains("c-two.contract.v1"));
        if target == ContractCodegenTarget::TypeScript {
            for transport_symbol in [
                "createIpcEncodedTransport",
                "createHttpRelayEncodedTransport",
                "createRelayAwareHttpEncodedTransport",
                "createNativeRequestShmWriter",
                "createNativeResponseShmReader",
            ] {
                assert!(
                    source.contains(transport_symbol),
                    "missing preserved TypeScript transport symbol {transport_symbol}",
                );
            }
            assert!(source.contains("hold_method_1_echo"));
            assert!(source.contains("releasable.invalidate.call(value)"));
        }
    }
}

#[test]
fn target_method_symbols_include_the_stable_index_before_sanitized_names() {
    let descriptor = descriptor_with_method_names("echo", "hold_echo");
    for (target, module_path) in [
        (ContractCodegenTarget::Rust, "rust/c_two_contract.rs"),
        (ContractCodegenTarget::Python, "python/c_two_contract.py"),
        (
            ContractCodegenTarget::TypeScript,
            "typescript/c_two_contract.ts",
        ),
    ] {
        let set = compile_contract_artifacts(
            descriptor.as_bytes(),
            target,
            &ContractCodegenOptions::default(),
        )
        .unwrap();
        let source = std::str::from_utf8(set.get(module_path).unwrap().bytes()).unwrap();
        assert!(source.contains("method_0_echo"));
        assert!(source.contains("method_1_hold_echo"));
        if target == ContractCodegenTarget::TypeScript {
            assert!(source.contains("hold_method_0_echo"));
            assert!(source.contains("hold_method_1_hold_echo"));
        }
    }
}

#[test]
fn target_modules_are_deterministic_and_composed_with_core_payload_artifacts() {
    for (target, root, suffix) in [
        (ContractCodegenTarget::Rust, "rust", ".rs"),
        (ContractCodegenTarget::Python, "python", ".py"),
        (ContractCodegenTarget::TypeScript, "typescript", ".ts"),
    ] {
        let options = ContractCodegenOptions::default();
        let first = compile_contract_artifacts(DESCRIPTOR.as_bytes(), target, &options).unwrap();
        let second = compile_contract_artifacts(DESCRIPTOR.as_bytes(), target, &options).unwrap();
        assert_eq!(first, second);
        assert!(
            first
                .get(&format!("{root}/c_two_contract{suffix}"))
                .is_some()
        );
        assert!(first.artifacts().iter().any(|artifact| {
            artifact
                .relative_path()
                .starts_with(&format!("{root}/payloads/"))
                && artifact.relative_path().ends_with(suffix)
        }));
    }
}

#[test]
fn no_payload_contract_still_has_a_real_c_two_target_module() {
    let mut descriptor: serde_json::Value = serde_json::from_str(DESCRIPTOR).unwrap();
    descriptor["methods"].as_array_mut().unwrap().truncate(1);
    let fingerprints =
        c2_contract::derive_contract_fingerprints_json(descriptor.to_string().as_bytes()).unwrap();
    descriptor["fingerprints"]["abi_hash"] =
        serde_json::Value::String(fingerprints.abi_hash().to_string());
    descriptor["fingerprints"]["signature_hash"] =
        serde_json::Value::String(fingerprints.signature_hash().to_string());

    for (target, module_path) in [
        (ContractCodegenTarget::Rust, "rust/c_two_contract.rs"),
        (ContractCodegenTarget::Python, "python/c_two_contract.py"),
        (
            ContractCodegenTarget::TypeScript,
            "typescript/c_two_contract.ts",
        ),
    ] {
        let set = compile_contract_artifacts(
            descriptor.to_string().as_bytes(),
            target,
            &Default::default(),
        )
        .unwrap();
        assert!(set.get(module_path).is_some());
        assert!(
            !set.artifacts()
                .iter()
                .any(|artifact| { artifact.relative_path().contains("/payloads/") })
        );
    }
}

#[test]
fn generated_rust_project_compiles_against_public_c_two_and_fastdb_apis() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap();
    let tempdir = tempfile::tempdir().unwrap();
    let generated = tempdir.path().join("generated");
    compile_contract_artifacts(
        DESCRIPTOR.as_bytes(),
        ContractCodegenTarget::Rust,
        &Default::default(),
    )
    .unwrap()
    .publish_new_tree(&generated)
    .unwrap();

    // Inject a wrong expected digest only at the post-open held guard. The
    // payload still opens through real FastDB, so this exercises generated
    // cleanup after validation rather than the earlier open-failure path.
    let module = generated.join("rust/c_two_contract.rs");
    let source = std::fs::read_to_string(&module).unwrap();
    assert_eq!(source.matches("payload.require_spec_sha256(").count(), 1);
    let source = source
        .lines()
        .map(|line| {
            if line
                .trim_start()
                .starts_with("payload.require_spec_sha256(")
            {
                "        payload.require_spec_sha256(&[0; 32])"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&module, source).unwrap();

    let manifest = format!(
        r#"[package]
name = "c-two-generated-contract-check"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
c-two = {{ version = "0.1.0", path = {c_two:?} }}
fastdb = "=0.2.3"
c2-ipc = {{ path = {ipc:?} }}
c2-mem = {{ path = {mem:?} }}
c2-wire = {{ path = {wire:?} }}
parking_lot = "0.12"
"#,
        c_two = repository.join("sdk/rust"),
        ipc = repository.join("core/transport/c2-ipc"),
        mem = repository.join("core/foundation/c2-mem"),
        wire = repository.join("core/protocol/c2-wire"),
    );
    std::fs::write(tempdir.path().join("Cargo.toml"), manifest).unwrap();
    // Reuse the graph available to the enclosing Core test invocation, rather
    // than freshly selecting registry versions for this temporary root.
    std::fs::copy(
        repository.join("core/Cargo.lock"),
        tempdir.path().join("Cargo.lock"),
    )
    .expect("seed generated project dependencies from the tested Core workspace");
    std::fs::create_dir(tempdir.path().join("src")).unwrap();
    std::fs::write(
        tempdir.path().join("src/lib.rs"),
        "#[path = \"../generated/rust/c_two_contract.rs\"]\npub mod contract;\n",
    )
    .unwrap();
    std::fs::write(
        tempdir.path().join("src/main.rs"),
        r##"use std::sync::Arc;
use c_two_generated_contract_check::contract;
use c_two::generated::{AdapterFailurePhase, HeldResponse, fastdb_adapter_error};
use c2_ipc::{ResponseData, ResponseLease};
use c2_mem::{MemPool, PoolConfig};
use c2_wire::chunk::ReassemblyBacking;
use fastdb::{BuildPolicy, Builder, CompiledSpec};
use parking_lot::RwLock;

fn main() {
    let spec = CompiledSpec::compile(br#"{
        "schema":"fastdb.payload.v1", "profile":"record.v1",
        "entries":[{"id":"value","cardinality":"one","type":{"kind":"str","nullable":true}}],
        "components":[]
    }"#).unwrap();
    let mut builder = Builder::create(&spec).unwrap();
    builder.entry_begin(0, 1).unwrap().value_str("held").unwrap();
    let payload = builder.freeze().unwrap().execute(BuildPolicy::AllowStaging).unwrap().payload;
    let expected = fastdb_adapter_error(
        AdapterFailurePhase::ClientOutputDeserializing,
        payload.require_spec_sha256(&[0; 32]).unwrap_err(),
    );
    let bytes = payload.binary_bytes().unwrap();
    let pool = Arc::new(RwLock::new(MemPool::new(PoolConfig::default())));
    let budget = pool.read().budget().unwrap().clone();
    let mut backing = ReassemblyBacking::admit(Arc::clone(&pool), 1, bytes.len()).unwrap();
    backing.write_at(0, &bytes).unwrap();
    let response = HeldResponse::from_response_lease(ResponseLease::new(ResponseData::Handle(backing))).unwrap();
    let original = std::mem::replace(&mut *pool.write(), MemPool::new(PoolConfig::default()));
    let error = contract::decode_method_1_echo_output_held(response).unwrap_err();
    match error {
        c_two::Error::Semantic(error) => assert_eq!(error, expected),
        error => panic!("primary semantic error was replaced: {error}"),
    }
    assert_eq!(budget.snapshot().abandoned_releases, 1);
    assert_eq!(budget.snapshot().reassembly.used_bytes, bytes.len() as u64);
    *pool.write() = original;
    assert_eq!(pool.read().stats().alloc_count, 1);
    pool.write().destroy();
}
"##,
    )
    .unwrap();

    let output = Command::new(env!("CARGO"))
        .args(["run", "--quiet", "--offline"])
        .current_dir(tempdir.path())
        .env(
            "CARGO_TARGET_DIR",
            std::env::var_os("CARGO_TARGET_DIR")
                .map(Into::into)
                .unwrap_or_else(|| repository.join("core/target")),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "generated Rust project failed to compile:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.lines().any(|line| {
            line.starts_with("Warning: held payload decode cleanup failed:")
                && line.contains("pool owner incarnation mismatch")
        }),
        "generated held cleanup discarded its failure reason:\n{stderr}",
    );
    assert_eq!(
        stderr
            .matches("held payload decode cleanup failed:")
            .count(),
        1
    );
    assert_eq!(
        stderr
            .matches("final Rust held value release failed:")
            .count(),
        1
    );
}

#[test]
fn generated_typescript_project_typechecks_against_fastdb_source() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap();
    let fastdb_typescript = repository.parent().unwrap().join("fastdb/ts/fastdb4ts");
    // npm's `.bin/tsc` entry is a POSIX shell shim that CreateProcess cannot
    // execute on Windows; run the compiler's JavaScript entry with Node.
    let tsc = fastdb_typescript.join("node_modules/typescript/bin/tsc");
    let payload_module = fastdb_typescript.join("src/payload/index.ts");
    if !tsc.is_file() || !payload_module.is_file() {
        eprintln!(
            "skipping sibling composition check because FastDB TypeScript source is unavailable"
        );
        return;
    }

    let tempdir = tempfile::tempdir().unwrap();
    let generated = tempdir.path().join("generated");
    let descriptor = descriptor_with_method_names("echo", "hold_echo");
    compile_contract_artifacts(
        descriptor.as_bytes(),
        ContractCodegenTarget::TypeScript,
        &Default::default(),
    )
    .unwrap()
    .publish_new_tree(&generated)
    .unwrap();

    let config = serde_json::json!({
        "compilerOptions": {
            "target": "ES2022",
            "module": "ES2022",
            "moduleResolution": "Bundler",
            "lib": ["ES2022", "DOM"],
            "strict": true,
            "skipLibCheck": true,
            "noEmit": true,
            "noUnusedLocals": true,
            "noUnusedParameters": true,
            "baseUrl": tempdir.path(),
            "paths": {
                "fastdb4ts/payload": [payload_module],
            },
        },
        "include": [
            generated.join("typescript/**/*.ts"),
        ],
    });
    let config_path = tempdir.path().join("tsconfig.json");
    std::fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();

    let output = Command::new("node")
        .arg(&tsc)
        .args(["--project", config_path.to_str().unwrap()])
        .current_dir(tempdir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "generated TypeScript project failed to typecheck:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn generated_python_reports_cleanup_failures_without_replacing_primary() {
    let tempdir = tempfile::tempdir().unwrap();
    let generated = tempdir.path().join("generated");
    compile_contract_artifacts(
        DESCRIPTOR.as_bytes(),
        ContractCodegenTarget::Python,
        &Default::default(),
    )
    .unwrap()
    .publish_new_tree(&generated)
    .unwrap();
    let script = r#"import contextlib
import importlib.util
import io
import sys
import types

primary = ValueError("primary decode failed")
cleanup = RuntimeError("payload cleanup failed")
events = []
class CompiledSpec:
    @staticmethod
    def compile(*args, **kwargs):
        return CompiledSpec()
class Payload:
    @staticmethod
    def open_copy(*args, **kwargs):
        return Payload()
    def require_spec_sha256(self, expected):
        raise primary
    def close(self):
        events.append("payload:close")
        raise cleanup

package = types.ModuleType("fastdb4py")
package.__path__ = []
payload = types.ModuleType("fastdb4py.payload")
for name, value in {"Payload": Payload, "CompiledSpec": CompiledSpec,
                    "Builder": type("Builder", (), {}), "View": type("View", (), {}),
                    "GraphIdentity": type("GraphIdentity", (), {}),
                    "ObjectHandle": type("ObjectHandle", (), {}), "PayloadError": Exception}.items():
    setattr(payload, name, value)
sys.modules[package.__name__] = package
sys.modules[payload.__name__] = payload
spec = importlib.util.spec_from_file_location("contract", sys.argv[1])
contract = importlib.util.module_from_spec(spec)
sys.modules["contract"] = contract
spec.loader.exec_module(contract)

class Response:
    def __init__(self, fail_copy):
        self.fail_copy = fail_copy
    def __bytes__(self):
        if self.fail_copy:
            raise primary
        return b"response"
    def release(self):
        events.append("response:release")
        raise cleanup

for fail_copy in (True, False):
    events.clear()
    stderr = io.StringIO()
    with contextlib.redirect_stderr(stderr):
        try:
            contract._copy_and_release_response(Response(fail_copy))
        except BaseException as error:
            assert error is (primary if fail_copy else cleanup), error
        else:
            raise AssertionError("missing response failure")
    assert events == ["response:release"], events
    warning = stderr.getvalue()
    assert (warning.count("Warning:") == 1 and "payload cleanup failed" in warning) if fail_copy else not warning, warning

events.clear()
stderr = io.StringIO()
with contextlib.redirect_stderr(stderr):
    try:
        contract.decode_method_1_echo_output(b"response")
    except BaseException as error:
        assert error is primary, error
    else:
        raise AssertionError("missing digest failure")
assert events == ["payload:close"], events
assert stderr.getvalue().count("Warning:") == 1 and "payload cleanup failed" in stderr.getvalue(), stderr.getvalue()
"#;
    let output = Command::new(if cfg!(windows) { "python" } else { "python3" })
        .args(["-c", script])
        .arg(generated.join("python/c_two_contract.py"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "generated Python discarded a cleanup failure or replaced its primary error:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn generated_typescript_payload_lifecycle_is_failure_safe() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap();
    let fastdb_typescript = repository.parent().unwrap().join("fastdb/ts/fastdb4ts");
    // See the typecheck test: Windows cannot execute the POSIX `.bin/tsc` shim.
    let tsc = fastdb_typescript.join("node_modules/typescript/bin/tsc");
    let node = Command::new("node").arg("--version").output();
    if !tsc.is_file() || node.is_err() {
        eprintln!(
            "skipping generated TypeScript lifecycle check because TypeScript/Node tooling is unavailable"
        );
        return;
    }

    let tempdir = tempfile::tempdir().unwrap();
    let generated = tempdir.path().join("generated");
    compile_contract_artifacts(
        DESCRIPTOR.as_bytes(),
        ContractCodegenTarget::TypeScript,
        &Default::default(),
    )
    .unwrap()
    .publish_new_tree(&generated)
    .unwrap();

    let package = tempdir.path().join("node_modules/fastdb4ts");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("package.json"),
        r#"{"type":"module","exports":{"./payload":{"types":"./payload.d.ts","default":"./payload.js"}}}"#,
    )
    .unwrap();
    std::fs::write(
        package.join("payload.d.ts"),
        r#"export const events: string[];
export function setFailInvalidate(value: boolean): void;
export function setFailResponseRequire(value: boolean): void;
export function setFailDispose(value: boolean): void;
export class PayloadError extends Error {
  readonly code: number;
  readonly symbol: string;
  readonly path: string;
  readonly detailsJson: string;
}
export class CompiledSpec {
  static compile(source: Uint8Array): CompiledSpec;
}
export class View {
  clone(): View;
  length(): bigint;
  at(index: bigint): View;
  materialize(): View;
  dispose(): void;
}
export class Builder {
  requireSpecSha256(expected: Uint8Array): void;
  entryBegin(index: number, valueCount: bigint): Builder;
}
export class Payload {
  constructor(label?: string);
  static openCopy(spec: CompiledSpec, source: Uint8Array): Payload;
  requireSpecSha256(expected: Uint8Array): void;
  binaryBytes(): Uint8Array;
  entryView(index: number): View;
  invalidate(): void;
  dispose(): void;
}
"#,
    )
    .unwrap();
    std::fs::write(
        package.join("payload.js"),
        r#"export const events = [];
let failInvalidate = false;
let failResponseRequire = false;
let failDispose = false;
export function setFailInvalidate(value) {
  failInvalidate = value;
}
export function setFailResponseRequire(value) {
  failResponseRequire = value;
}
export function setFailDispose(value) {
  failDispose = value;
}
export class PayloadError extends Error {
  constructor(code, symbol, path, message, detailsJson) {
    super(message);
    this.name = "PayloadError";
    this.code = code;
    this.symbol = symbol;
    this.path = path;
    this.detailsJson = detailsJson;
  }
}
export class CompiledSpec {
  static compile(_source) {
    return new CompiledSpec();
  }
}
export class View {
  clone() { return this; }
  length() { return 0n; }
  at(_index) { return this; }
  materialize() { return this; }
  dispose() {}
}
export class Builder {
  requireSpecSha256(_expected) {}
  entryBegin(_index, _valueCount) { return this; }
}
export class Payload {
  constructor(label = "request") {
    this.label = label;
  }
  static openCopy(_spec, _source) {
    events.push("response:openCopy");
    return new Payload("response");
  }
  requireSpecSha256(_expected) {
    events.push(`${this.label}:require`);
    if (this.label === "response" && failResponseRequire) {
      throw new Error("response digest guard failed");
    }
  }
  binaryBytes() {
    events.push(`${this.label}:binary`);
    return new Uint8Array([1]);
  }
  entryView(_index) {
    return new View();
  }
  invalidate() {
    events.push(`${this.label}:invalidate`);
    if (failInvalidate) {
      throw new Error("response invalidate failed");
    }
  }
  dispose() {
    events.push(`${this.label}:dispose`);
    if (failDispose) {
      throw new Error("response dispose failed");
    }
  }
}
"#,
    )
    .unwrap();
    std::fs::write(tempdir.path().join("package.json"), r#"{"type":"module"}"#).unwrap();
    std::fs::write(
        tempdir.path().join("smoke.ts"),
        r#"import { ContractClient, type C2EncodedClientTransport } from "./generated/typescript/c_two_contract.js";
import {
  Payload,
  events,
  setFailInvalidate,
  setFailResponseRequire,
  setFailDispose,
} from "fastdb4ts/payload";

const warnings: string[] = [];
console.warn = (...args: unknown[]): void => { warnings.push(args.map(String).join(" ")); };
let responseReleaseThrows = false;
const transportReleaseError = new Error("transport release failed");
const transport: C2EncodedClientTransport = {
  async call(_routeName, _contract, _method, _payload) {
    events.push("transport:call");
    const response = new Uint8Array([7]) as Uint8Array & { release(): void };
    response.release = (): void => {
      events.push("transport:release");
      if (responseReleaseThrows) {
        throw transportReleaseError;
      }
    };
    return response;
  },
};
const client = new ContractClient(transport, "payload-route");

function requireEvents(expected: readonly string[]): void {
  const actual = JSON.stringify(events);
  const wanted = JSON.stringify(expected);
  if (actual !== wanted) {
    throw new Error(`unexpected lifecycle events ${actual}; expected ${wanted}`);
  }
}

events.length = 0;
const held = await client.hold_method_1_echo(new Payload());
requireEvents([
  "request:require",
  "request:binary",
  "transport:call",
  "response:openCopy",
  "response:require",
]);
held.release();
requireEvents([
  "request:require",
  "request:binary",
  "transport:call",
  "response:openCopy",
  "response:require",
  "response:invalidate",
  "response:dispose",
  "transport:release",
]);

events.length = 0;
const failingHeld = await client.hold_method_1_echo(new Payload());
events.length = 0;
setFailInvalidate(true);
let invalidateFailure: unknown;
try {
  failingHeld.release();
} catch (error) {
  invalidateFailure = error;
}
setFailInvalidate(false);
if (!String(invalidateFailure).includes("response invalidate failed")) {
  throw new Error(`missing held invalidation failure: ${String(invalidateFailure)}`);
}
requireEvents([
  "response:invalidate",
  "response:dispose",
  "transport:release",
]);

events.length = 0;
setFailResponseRequire(true);
let digestFailure: unknown;
try {
  await client.method_1_echo(new Payload());
} catch (error) {
  digestFailure = error;
}
setFailResponseRequire(false);
if (!String(digestFailure).includes("response digest guard failed")) {
  throw new Error(`missing response digest failure: ${String(digestFailure)}`);
}
requireEvents([
  "request:require",
  "request:binary",
  "transport:call",
  "response:openCopy",
  "response:require",
  "response:dispose",
  "transport:release",
]);

events.length = 0;
responseReleaseThrows = true;
let releaseFailure: unknown;
try {
  await client.method_1_echo(new Payload());
} catch (error) {
  releaseFailure = error;
}
responseReleaseThrows = false;
if (!String(releaseFailure).includes("transport release failed")) {
  throw new Error(`missing transport release failure: ${String(releaseFailure)}`);
}
requireEvents([
  "request:require",
  "request:binary",
  "transport:call",
  "response:openCopy",
  "response:require",
  "transport:release",
  "response:invalidate",
  "response:dispose",
]);

for (const hold of [false, true]) {
  events.length = 0;
  warnings.length = 0;
  setFailResponseRequire(true);
  setFailDispose(true);
  responseReleaseThrows = true;
  let primary: unknown;
  try {
    if (hold) {
      await client.hold_method_1_echo(new Payload());
    } else {
      await client.method_1_echo(new Payload());
    }
  } catch (error) {
    primary = error;
  }
  setFailResponseRequire(false);
  setFailDispose(false);
  responseReleaseThrows = false;
  if (!String(primary).includes("response digest guard failed")) {
    throw new Error(`lost primary digest error: ${String(primary)}`);
  }
  if (warnings.length !== 2 ||
      !warnings[0]?.includes("Warning:") || !warnings[0]?.includes("response dispose failed") ||
      !warnings[1]?.includes("Warning:") || !warnings[1]?.includes("transport release failed")) {
    throw new Error(`discarded decode cleanup failures: ${JSON.stringify(warnings)}`);
  }
  requireEvents([
    "request:require", "request:binary", "transport:call", "response:openCopy",
    "response:require", "response:dispose", "transport:release",
  ]);
}

warnings.length = 0;
responseReleaseThrows = true;
setFailInvalidate(true);
let primary: unknown;
try {
  await client.method_1_echo(new Payload());
} catch (error) {
  primary = error;
}
responseReleaseThrows = false;
setFailInvalidate(false);
if (primary !== transportReleaseError || warnings.length !== 1 ||
    !warnings[0]?.includes("Warning:") || !warnings[0]?.includes("response invalidate failed")) {
  throw new Error(`discarded value cleanup error or replaced primary: ${JSON.stringify(warnings)}`);
}

const multiplyFailingHeld = await client.hold_method_1_echo(new Payload());
events.length = 0;
warnings.length = 0;
responseReleaseThrows = true;
setFailInvalidate(true);
setFailDispose(true);
let firstReleaseError: unknown;
try {
  multiplyFailingHeld.release();
} catch (error) {
  firstReleaseError = error;
}
multiplyFailingHeld.release();
responseReleaseThrows = false;
setFailInvalidate(false);
setFailDispose(false);
if (!String(firstReleaseError).includes("response invalidate failed") || warnings.length !== 2 ||
    !warnings[0]?.includes("response dispose failed") || !warnings[1]?.includes("transport release failed")) {
  throw new Error(`lost secondary held cleanup errors: ${JSON.stringify(warnings)}`);
}
requireEvents(["response:invalidate", "response:dispose", "transport:release"]);
"#,
    )
    .unwrap();
    let config = serde_json::json!({
        "compilerOptions": {
            "target": "ES2022",
            "module": "NodeNext",
            "moduleResolution": "NodeNext",
            "lib": ["ES2022", "DOM"],
            "strict": true,
            "skipLibCheck": true,
            "noUnusedLocals": true,
            "noUnusedParameters": true,
            "outDir": "dist",
        },
        "include": ["generated/typescript/**/*.ts", "smoke.ts"],
    });
    let config_path = tempdir.path().join("tsconfig.json");
    std::fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();

    let typecheck = Command::new("node")
        .arg(&tsc)
        .args(["--project", config_path.to_str().unwrap()])
        .current_dir(tempdir.path())
        .output()
        .unwrap();
    assert!(
        typecheck.status.success(),
        "generated TypeScript lifecycle project failed to compile:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&typecheck.stdout),
        String::from_utf8_lossy(&typecheck.stderr),
    );
    let runtime = Command::new("node")
        .arg(tempdir.path().join("dist/smoke.js"))
        .current_dir(tempdir.path())
        .output()
        .unwrap();
    assert!(
        runtime.status.success(),
        "generated TypeScript lifecycle project failed at runtime:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&runtime.stdout),
        String::from_utf8_lossy(&runtime.stderr),
    );
}
