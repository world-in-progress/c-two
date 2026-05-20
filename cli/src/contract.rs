use anyhow::{Result, anyhow};
use clap::{Args, Subcommand};
use std::io::{self, Read};
use std::process::Command as ProcessCommand;

#[derive(Debug, Args)]
pub struct ContractArgs {
    #[command(subcommand)]
    pub command: ContractCommand,
}

#[derive(Debug, Subcommand)]
pub enum ContractCommand {
    /// Export payload ABI artifact descriptors from a Python CRM class.
    Artifacts(PythonArtifactsArgs),
    /// Generate SDK artifacts from a portable descriptor.
    Codegen(CodegenArgs),
    /// Report fastdb-first portability diagnostics for a Python CRM class.
    Diagnose(PythonDiagnoseArgs),
    /// Export a portable descriptor from a Python CRM class.
    Export(PythonExportArgs),
    /// Infer a portable descriptor or diagnostics from a Python resource class.
    Infer(PythonInferArgs),
    /// Validate a portable c-two.contract.v1 descriptor.
    Validate(ValidateArgs),
}

#[derive(Debug, Args)]
pub struct ValidateArgs {
    /// Descriptor JSON path, or "-" to read from stdin.
    pub path: String,
}

#[derive(Debug, Args)]
pub struct CodegenArgs {
    #[command(subcommand)]
    pub command: CodegenCommand,
}

#[derive(Debug, Args)]
pub struct PythonDiagnoseArgs {
    /// Python CRM class target as module:ClassName.
    pub target: String,
    /// Python executable used for import/reflection. Defaults to C2_PYTHON or python3.
    #[arg(long)]
    pub python: Option<String>,
    /// Limit diagnostics to one CRM method; repeatable.
    #[arg(long = "method")]
    pub methods: Vec<String>,
    /// Write diagnostics JSON to this file instead of stdout.
    #[arg(long)]
    pub out: Option<String>,
    /// Pretty-print diagnostics JSON.
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Debug, Args)]
pub struct PythonArtifactsArgs {
    /// Python CRM class target as module:ClassName.
    pub target: String,
    /// Python executable used for import/reflection. Defaults to C2_PYTHON or python3.
    #[arg(long)]
    pub python: Option<String>,
    /// Limit artifacts to one CRM method; repeatable.
    #[arg(long = "method")]
    pub methods: Vec<String>,
    /// Write payload ABI artifact JSON to this file instead of stdout.
    #[arg(long)]
    pub out: Option<String>,
    /// Pretty-print payload ABI artifact JSON.
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Debug, Subcommand)]
pub enum CodegenCommand {
    /// Generate a TypeScript client and optional C-Two FastDB helpers.
    Typescript(TypeScriptCodegenArgs),
}

#[derive(Debug, Args)]
pub struct TypeScriptCodegenArgs {
    /// Descriptor JSON path, or "-" to read from stdin.
    pub path: String,
    /// Write generated TypeScript to this file instead of stdout.
    #[arg(long)]
    pub out: Option<String>,
    /// Fail when the descriptor references codecs without built-in TypeScript support.
    #[arg(long)]
    pub strict_codecs: bool,
    /// FastDB schema descriptor JSON file or artifact bundle for C-Two FastDB helper generation; repeatable.
    #[arg(long = "fastdb-schema")]
    pub fastdb_schemas: Vec<String>,
    /// Write generated C-Two FastDB TypeScript helpers to this file.
    #[arg(long = "fastdb-out")]
    pub fastdb_out: Option<String>,
    /// Python executable used for C-Two FastDB helper generation. Defaults to C2_PYTHON or python3.
    #[arg(long)]
    pub python: Option<String>,
}

#[derive(Debug, Args)]
pub struct PythonExportArgs {
    /// Python CRM class target as module:ClassName.
    pub target: String,
    /// Python executable used for import/reflection. Defaults to C2_PYTHON or python3.
    #[arg(long)]
    pub python: Option<String>,
    /// Limit export to one CRM method; repeatable.
    #[arg(long = "method")]
    pub methods: Vec<String>,
    /// Write descriptor JSON to this file instead of stdout.
    #[arg(long)]
    pub out: Option<String>,
    /// Pretty-print descriptor JSON.
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Debug, Args)]
pub struct PythonInferArgs {
    /// Python resource class target as module:ClassName.
    pub target: String,
    /// CRM namespace for the inferred projection.
    #[arg(long)]
    pub namespace: String,
    /// CRM version for the inferred projection.
    #[arg(long)]
    pub version: String,
    /// CRM class name for the inferred projection.
    #[arg(long)]
    pub name: Option<String>,
    /// Public resource method to expose; repeatable.
    #[arg(long = "method", required = true)]
    pub methods: Vec<String>,
    /// Python executable used for import/reflection. Defaults to C2_PYTHON or python3.
    #[arg(long)]
    pub python: Option<String>,
    /// Write portability diagnostics for the inferred projection instead of exporting a portable descriptor.
    #[arg(long)]
    pub diagnose: bool,
    /// Write payload ABI artifacts for the inferred projection instead of exporting a portable descriptor.
    #[arg(long)]
    pub artifacts: bool,
    /// Write descriptor, diagnostics, or payload ABI artifact JSON to this file instead of stdout.
    #[arg(long)]
    pub out: Option<String>,
    /// Pretty-print descriptor, diagnostics, or payload ABI artifact JSON.
    #[arg(long)]
    pub pretty: bool,
}

pub fn run(args: ContractArgs) -> Result<()> {
    match args.command {
        ContractCommand::Artifacts(args) => artifacts(args),
        ContractCommand::Codegen(args) => codegen(args),
        ContractCommand::Diagnose(args) => diagnose(args),
        ContractCommand::Export(args) => export(args),
        ContractCommand::Infer(args) => infer(args),
        ContractCommand::Validate(args) => validate(&args.path),
    }
}

fn artifacts(args: PythonArtifactsArgs) -> Result<()> {
    let mut py_args = vec![
        "-m".to_string(),
        "c_two.cli.contract".to_string(),
        "artifacts".to_string(),
        args.target,
    ];
    for method in args.methods {
        py_args.push("--method".to_string());
        py_args.push(method);
    }
    if args.pretty {
        py_args.push("--pretty".to_string());
    }
    let payload = run_python_contract(args.python.as_deref(), &py_args)?;
    validate_artifact_payload(&payload)?;
    write_payload(&payload, args.out.as_deref())
}

fn diagnose(args: PythonDiagnoseArgs) -> Result<()> {
    let mut py_args = vec![
        "-m".to_string(),
        "c_two.cli.contract".to_string(),
        "diagnose".to_string(),
        args.target,
    ];
    for method in args.methods {
        py_args.push("--method".to_string());
        py_args.push(method);
    }
    if args.pretty {
        py_args.push("--pretty".to_string());
    }
    let payload = run_python_contract(args.python.as_deref(), &py_args)?;
    validate_diagnostic_payload(&payload)?;
    write_payload(&payload, args.out.as_deref())
}

fn validate(path: &str) -> Result<()> {
    let payload = read_payload(path)?;
    c2_contract::validate_portable_contract_descriptor_json(payload.as_bytes())
        .map_err(|err| anyhow!("{err}"))?;
    let digest = c2_contract::contract_descriptor_sha256_hex(payload.as_bytes())
        .map_err(|err| anyhow!("{err}"))?;
    let label = if path == "-" { "stdin" } else { path };
    println!("{label}: valid c-two.contract.v1 sha256={digest}");
    Ok(())
}

fn codegen(args: CodegenArgs) -> Result<()> {
    match args.command {
        CodegenCommand::Typescript(args) => codegen_typescript(args),
    }
}

fn codegen_typescript(args: TypeScriptCodegenArgs) -> Result<()> {
    if args.fastdb_out.is_some() && args.fastdb_schemas.is_empty() {
        return Err(anyhow!(
            "--fastdb-out requires at least one --fastdb-schema"
        ));
    }
    if !args.fastdb_schemas.is_empty() && args.fastdb_out.is_none() {
        return Err(anyhow!("--fastdb-schema requires --fastdb-out"));
    }
    if !args.fastdb_schemas.is_empty() && args.path == "-" {
        return Err(anyhow!(
            "--fastdb-schema requires a descriptor file path, not stdin"
        ));
    }
    let payload = read_payload(&args.path)?;
    let generated = c2_codegen::generate_typescript_client(
        payload.as_bytes(),
        c2_codegen::TypeScriptOptions {
            strict_codecs: args.strict_codecs,
        },
    )
    .map_err(|err| anyhow!("{err}"))?;
    write_payload(&generated, args.out.as_deref())?;
    if let Some(fastdb_out) = args.fastdb_out.as_deref() {
        run_python_fastdb_typescript(
            args.python.as_deref(),
            &args.path,
            fastdb_out,
            &args.fastdb_schemas,
        )?;
    }
    Ok(())
}

fn run_python_fastdb_typescript(
    python: Option<&str>,
    contract_path: &str,
    output_path: &str,
    schema_paths: &[String],
) -> Result<()> {
    let mut py_args = vec![
        "-m".to_string(),
        "c_two.fastdb.typescript".to_string(),
        contract_path.to_string(),
        output_path.to_string(),
    ];
    for schema_path in schema_paths {
        py_args.push("--schema".to_string());
        py_args.push(schema_path.clone());
    }
    let payload = run_python_contract(python, &py_args)?;
    if !payload.trim().is_empty() {
        return Err(anyhow!(
            "C-Two FastDB TypeScript helper generation wrote unexpected stdout: {}",
            payload.trim()
        ));
    }
    Ok(())
}

fn export(args: PythonExportArgs) -> Result<()> {
    let mut py_args = vec![
        "-m".to_string(),
        "c_two.cli.contract".to_string(),
        "export".to_string(),
        args.target,
    ];
    for method in args.methods {
        py_args.push("--method".to_string());
        py_args.push(method);
    }
    if args.pretty {
        py_args.push("--pretty".to_string());
    }
    let payload = run_python_contract(args.python.as_deref(), &py_args)?;
    validate_descriptor_payload(&payload)?;
    write_payload(&payload, args.out.as_deref())
}

fn infer(args: PythonInferArgs) -> Result<()> {
    if args.diagnose && args.artifacts {
        return Err(anyhow!(
            "--diagnose and --artifacts cannot be used together"
        ));
    }
    let mut py_args = vec![
        "-m".to_string(),
        "c_two.cli.contract".to_string(),
        "infer".to_string(),
        args.target,
        "--namespace".to_string(),
        args.namespace,
        "--version".to_string(),
        args.version,
    ];
    if let Some(name) = args.name {
        py_args.push("--name".to_string());
        py_args.push(name);
    }
    for method in args.methods {
        py_args.push("--method".to_string());
        py_args.push(method);
    }
    if args.diagnose {
        py_args.push("--diagnose".to_string());
    }
    if args.artifacts {
        py_args.push("--artifacts".to_string());
    }
    if args.pretty {
        py_args.push("--pretty".to_string());
    }
    let payload = run_python_contract(args.python.as_deref(), &py_args)?;
    if args.artifacts {
        validate_artifact_payload(&payload)?;
    } else if args.diagnose {
        validate_diagnostic_payload(&payload)?;
    } else {
        validate_descriptor_payload(&payload)?;
    }
    write_payload(&payload, args.out.as_deref())
}

fn run_python_contract(python: Option<&str>, args: &[String]) -> Result<String> {
    let python = python
        .map(ToOwned::to_owned)
        .or_else(|| std::env::var("C2_PYTHON").ok())
        .unwrap_or_else(|| "python3".to_string());
    let output = ProcessCommand::new(&python)
        .args(args)
        .output()
        .map_err(|err| anyhow!("failed to run Python contract command {python:?}: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else if !stdout.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            format!("exit status {}", output.status)
        };
        return Err(anyhow!("python contract command failed: {detail}"));
    }
    String::from_utf8(output.stdout)
        .map_err(|err| anyhow!("python contract command emitted non-UTF-8 output: {err}"))
}

fn validate_descriptor_payload(payload: &str) -> Result<()> {
    c2_contract::validate_portable_contract_descriptor_json(payload.as_bytes())
        .map_err(|err| anyhow!("{err}"))
}

fn validate_artifact_payload(payload: &str) -> Result<()> {
    let parsed: serde_json::Value = serde_json::from_str(payload)
        .map_err(|err| anyhow!("payload ABI artifact output is not valid JSON: {err}"))?;
    match parsed {
        serde_json::Value::Array(items) if items.iter().all(|item| item.is_object()) => Ok(()),
        serde_json::Value::Array(_) => Err(anyhow!(
            "payload ABI artifact output must be a JSON array of objects"
        )),
        _ => Err(anyhow!("payload ABI artifact output must be a JSON array")),
    }
}

fn validate_diagnostic_payload(payload: &str) -> Result<()> {
    let parsed: serde_json::Value = serde_json::from_str(payload)
        .map_err(|err| anyhow!("diagnostic output is not valid JSON: {err}"))?;
    match parsed {
        serde_json::Value::Array(items) if items.iter().all(|item| item.is_object()) => Ok(()),
        serde_json::Value::Array(_) => {
            Err(anyhow!("diagnostic output must be a JSON array of objects"))
        }
        _ => Err(anyhow!("diagnostic output must be a JSON array")),
    }
}

fn write_payload(payload: &str, out: Option<&str>) -> Result<()> {
    if let Some(path) = out {
        std::fs::write(path, payload)
            .map_err(|err| anyhow!("failed to write payload to {path}: {err}"))?;
    } else {
        print!("{payload}");
        if !payload.ends_with('\n') {
            println!();
        }
    }
    Ok(())
}

fn read_payload(path: &str) -> Result<String> {
    if path == "-" {
        let mut payload = String::new();
        io::stdin()
            .read_to_string(&mut payload)
            .map_err(|err| anyhow!("failed to read descriptor from stdin: {err}"))?;
        return Ok(payload);
    }
    std::fs::read_to_string(path)
        .map_err(|err| anyhow!("failed to read descriptor from {path}: {err}"))
}
