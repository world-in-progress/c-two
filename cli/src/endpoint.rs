//! `c3 endpoint` — native local-endpoint inspection, reaping, and sweeps.
//!
//! This is a thin facade over the `c2-local` lifecycle surface. It derives the
//! OS endpoint through `LocalEndpoint`, parses credentials with the one Rust
//! codec, and reports exactly what the native layer returned. It never scans
//! arbitrary directories, uses only the platform native namespace,
//! never treats `WindowsNotApplicable` or `KernelManaged` as a live endpoint,
//! and never upgrades an interrupted round into full coverage.
//!
//! A failure is reported with a real non-zero exit status. `Busy`, `Stale`, and
//! `Unverified` are never written as success.

use anyhow::{Result, anyhow, bail};
use c2_core::{
    ENDPOINT_CREDENTIAL_MAX_BYTES, EndpointCredential, EndpointInspection, EndpointReapResult,
    EndpointSweep, EndpointUnverifiedReason, LocalEndpoint, SweepBatch, SweepBudget,
    inspect_endpoint, reap_endpoint,
};
use clap::{Args, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

/// Hard ceiling for one sweep batch's entry budget.
const MAX_SWEEP_ENTRIES: usize = 4096;
/// Hard ceiling for one sweep batch's wall-clock budget, in milliseconds.
const MAX_SWEEP_MS: u64 = 1000;
/// Default finite bound on sweep batches. A sweep is never unbounded.
const DEFAULT_MAX_BATCHES: u32 = 64;
/// Hard ceiling on sweep batches.
const MAX_BATCHES: u32 = 4096;

#[derive(Debug, Args)]
pub struct EndpointArgs {
    #[command(subcommand)]
    pub command: EndpointCommand,
}

#[derive(Debug, Subcommand)]
pub enum EndpointCommand {
    /// Inspect one logical local endpoint.
    Inspect(InspectArgs),
    /// Reap the exact endpoint object named by a credential file.
    Reap(ReapArgs),
    /// Run a bounded maintenance sweep over the platform native namespace.
    Sweep(SweepArgs),
}

#[derive(Debug, Args)]
pub struct InspectArgs {
    /// Logical IPC address, for example ipc://my_server.
    pub address: String,
}

#[derive(Debug, Args)]
pub struct ReapArgs {
    /// Logical IPC address, for example ipc://my_server.
    pub address: String,
    /// Credential JSON file produced by `inspect`.
    #[arg(long)]
    pub credential: PathBuf,
}

#[derive(Debug, Args)]
pub struct SweepArgs {
    /// Restrict maintenance to this logical IPC address. Repeat for multiple
    /// targets; omission explicitly selects the full native namespace.
    #[arg(long = "address", value_name = "IPC_ADDRESS")]
    pub addresses: Vec<String>,
    /// Maximum entries visited in one batch.
    #[arg(long, default_value_t = 64)]
    pub max_entries: usize,
    /// Maximum milliseconds of work in one batch.
    #[arg(long, default_value_t = 10)]
    pub max_ms: u64,
    /// Finite maximum number of batches to run.
    #[arg(long, default_value_t = DEFAULT_MAX_BATCHES)]
    pub max_batches: u32,
}

pub fn run(args: EndpointArgs) -> Result<ExitCode> {
    match args.command {
        EndpointCommand::Inspect(args) => inspect(args),
        EndpointCommand::Reap(args) => reap(args),
        EndpointCommand::Sweep(args) => sweep(args),
    }
}

fn inspect(args: InspectArgs) -> Result<ExitCode> {
    let endpoint = endpoint_for(&args.address)?;
    let report = match inspect_endpoint(&endpoint) {
        EndpointInspection::Absent => Report::status("absent"),
        EndpointInspection::Present(credential) => {
            let json = credential.to_json().map_err(|error| anyhow!("{error}"))?;
            Report::present(json)
        }
        // A kernel-managed namespace observes the platform, not a live
        // instance, so it is reported as not-applicable rather than alive.
        EndpointInspection::KernelManaged => {
            Report::status("not-applicable").with_reason(Some("kernel-managed".to_string()))
        }
        EndpointInspection::Unverified(reason) => {
            Report::status("unverified").with_reason(Some(reason_name(reason)))
        }
        EndpointInspection::IoError(error) => {
            Report::status("io-error").with_reason(io_reason(&error))
        }
    };
    report.emit()?;
    // Only an unverifiable or I/O-failed inspection is a failure. `absent` and
    // `not-applicable` are honest, successful observations.
    Ok(match report.status.as_str() {
        "unverified" | "io-error" => ExitCode::from(1),
        _ => ExitCode::SUCCESS,
    })
}

fn reap(args: ReapArgs) -> Result<ExitCode> {
    let credential = read_credential(&args.credential)?;
    // The strict credential and this operation use the same native derivation.
    let endpoint = endpoint_for(&args.address)?;
    // The credential must describe this exact endpoint; a mismatch is caught
    // natively as StaleTarget, but reject it early with a clear reason too.
    if credential.endpoint() != &endpoint {
        Report::status("stale-target")
            .with_reason(Some("credential-address-mismatch".to_string()))
            .emit()?;
        return Ok(ExitCode::from(1));
    }
    let result = reap_endpoint(&endpoint, &credential);
    let (status, reason) = reap_report(&result);
    Report::status(status).with_reason(reason).emit()?;
    Ok(match status {
        // Reaped and already-absent are the only successful terminal states.
        "reaped" | "already-absent" => ExitCode::SUCCESS,
        _ => ExitCode::from(1),
    })
}

fn sweep(args: SweepArgs) -> Result<ExitCode> {
    if args.max_entries == 0 || args.max_entries > MAX_SWEEP_ENTRIES {
        bail!("--max-entries must be between 1 and {MAX_SWEEP_ENTRIES}");
    }
    if args.max_ms == 0 || args.max_ms > MAX_SWEEP_MS {
        bail!("--max-ms must be between 1 and {MAX_SWEEP_MS}");
    }
    if args.max_batches == 0 || args.max_batches > MAX_BATCHES {
        bail!("--max-batches must be between 1 and {MAX_BATCHES}");
    }

    let endpoint = LocalEndpoint::from_address("ipc://c3-endpoint-sweep")?;
    // Native scope construction validates every logical address and the 4096
    // target limit before opening any iterator or taking a sweep lease.
    let mut sweep = if args.addresses.is_empty() {
        EndpointSweep::for_endpoint(&endpoint)
    } else {
        let scope = EndpointSweep::scope_for_addresses(&endpoint, &args.addresses)
            .map_err(|error| anyhow!("invalid endpoint sweep scope: {error}"))?;
        EndpointSweep::for_scope(&scope)
    }
    .map_err(|error| anyhow!("cannot open the {} namespace: {error}", endpoint.protocol()))?;
    let budget = SweepBudget {
        max_entries: args.max_entries,
        max_duration: std::time::Duration::from_millis(args.max_ms),
    };

    let started = std::time::Instant::now();
    let mut totals = Totals::default();
    let mut batches = 0u32;
    // Each batch is a fresh native iterator step. Entries are never collected
    // into a directory-wide list; only cumulative counters are retained.
    while batches < args.max_batches {
        let batch = sweep.next_batch(budget);
        batches += 1;
        totals.accumulate(&batch);
        let done = batch.round_complete || batch.round_interrupted;
        if done {
            totals.round_complete = batch.round_complete;
            totals.round_interrupted = batch.round_interrupted;
            totals.namespace_changed = batch.namespace_changed;
            break;
        }
    }

    // An unfinished round is reported honestly: coverage is not claimed.
    let exhausted = !totals.round_complete && !totals.round_interrupted;
    Report::new(if exhausted {
        "batch-limit"
    } else if totals.round_interrupted {
        "round-interrupted"
    } else {
        "complete"
    })
    .with_reason(if exhausted {
        Some("max-batches-reached".to_string())
    } else if totals.round_interrupted {
        Some("namespace-changed".to_string())
    } else {
        None
    })
    .with_sweep(&totals, batches, started.elapsed().as_millis())
    .emit()?;

    // A sweep that could not finish its round, or that observed a replaced or
    // vanished namespace, is not complete coverage.
    Ok(if totals.round_complete {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// Reads a credential file with one bounded open.
///
/// The path is rejected unless it is a regular file before the open, because
/// opening a FIFO for read blocks until a writer appears and a device can be
/// unbounded. The check uses `symlink_metadata`, which never follows a link
/// and never blocks, and the open is additionally `O_NONBLOCK` so a swapped
/// path still cannot park the caller. The opened handle is verified again, so
/// a path replaced between the check and the open cannot widen the read, and
/// the handle is capped with `take(MAX + 1)`: a file that grows after the open
/// stays bounded and the extra byte detects "too large" without trusting a
/// stale length. This is a read tool: it needs no delete permission and never
/// mutates the file.
fn read_credential(path: &PathBuf) -> Result<EndpointCredential> {
    use std::io::Read;

    let link_metadata = std::fs::symlink_metadata(path)
        .map_err(|error| anyhow!("cannot read credential {}: {error}", path.display()))?;
    if !link_metadata.file_type().is_file() {
        bail!("credential {} is not a regular file", path.display());
    }

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    // A FIFO must never block the open, even if the path was swapped after the
    // check above. The flag is not exposed portably, so it is applied on Unix
    // and the pre-check already covers the non-Unix case.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|error| anyhow!("cannot read credential {}: {error}", path.display()))?;
    // The handle is the authority: it is what will actually be read.
    let metadata = file
        .metadata()
        .map_err(|error| anyhow!("cannot read credential {}: {error}", path.display()))?;
    if !metadata.is_file() {
        bail!("credential {} is not a regular file", path.display());
    }
    let limit = ENDPOINT_CREDENTIAL_MAX_BYTES as u64;
    let mut bytes = Vec::with_capacity(ENDPOINT_CREDENTIAL_MAX_BYTES + 1);
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| anyhow!("cannot read credential {}: {error}", path.display()))?;
    if bytes.len() as u64 > limit {
        bail!(
            "credential {} exceeds the {ENDPOINT_CREDENTIAL_MAX_BYTES}-byte limit",
            path.display()
        );
    }
    let content = String::from_utf8(bytes)
        .map_err(|_| anyhow!("credential {} is not valid UTF-8", path.display()))?;
    EndpointCredential::from_json(&content).map_err(|error| anyhow!("{error}"))
}

fn reap_report(result: &EndpointReapResult) -> (&'static str, Option<String>) {
    match result {
        EndpointReapResult::Reaped => ("reaped", None),
        EndpointReapResult::AlreadyAbsent => ("already-absent", None),
        EndpointReapResult::Busy => ("busy", Some("coordinator-held".to_string())),
        EndpointReapResult::StaleTarget => ("stale-target", Some("identity-mismatch".to_string())),
        EndpointReapResult::Unverified(reason) => ("unverified", Some(reason_name(*reason))),
        EndpointReapResult::NotApplicable => {
            ("not-applicable", Some("no-filesystem-entry".to_string()))
        }
        EndpointReapResult::IoError(error) => ("io-error", io_reason(error)),
    }
}

fn reason_name(reason: EndpointUnverifiedReason) -> String {
    match reason {
        EndpointUnverifiedReason::UnsafeDirectory => "unsafe-directory",
        EndpointUnverifiedReason::Symlink => "symlink",
        EndpointUnverifiedReason::UnexpectedObject => "unexpected-object",
        EndpointUnverifiedReason::ForeignOwner => "foreign-owner",
        EndpointUnverifiedReason::MissingOwnership => "missing-ownership",
        EndpointUnverifiedReason::InvalidOwnership => "invalid-ownership",
        EndpointUnverifiedReason::InvalidRecord => "invalid-record",
        EndpointUnverifiedReason::RecordMismatch => "record-mismatch",
        EndpointUnverifiedReason::CoordinatorMissing => "coordinator-missing",
        EndpointUnverifiedReason::CoordinatorReplaced => "coordinator-replaced",
        EndpointUnverifiedReason::InitializationIncomplete => "initialization-incomplete",
    }
    .to_string()
}

fn io_reason(error: &std::io::Error) -> Option<String> {
    match error.raw_os_error() {
        Some(code) => Some(format!("{} (os error {code})", error.kind())),
        None => Some(error.kind().to_string()),
    }
}

#[derive(Default)]
struct Totals {
    entries_visited: usize,
    endpoints_examined: usize,
    reaped: usize,
    already_absent: usize,
    busy: usize,
    stale_target: usize,
    unverified: usize,
    io_errors: usize,
    not_applicable: usize,
    leases_retired: usize,
    round_complete: bool,
    round_interrupted: bool,
    namespace_changed: bool,
}

impl Totals {
    fn accumulate(&mut self, batch: &SweepBatch) {
        self.entries_visited += batch.entries_visited;
        self.endpoints_examined += batch.endpoints_examined;
        self.reaped += batch.reaped;
        self.already_absent += batch.already_absent;
        self.busy += batch.busy;
        self.stale_target += batch.stale_target;
        self.unverified += batch.unverified;
        self.io_errors += batch.io_errors;
        self.not_applicable += batch.not_applicable;
        self.leases_retired += batch.leases_retired;
    }
}

/// One honest JSON result line. `status` and `reason` are always emitted, so a
/// consumer never has to infer failure from missing fields.
struct Report {
    status: String,
    reason: Option<String>,
    credential: Option<String>,
    sweep: Option<serde_json::Value>,
}

impl Report {
    fn new(status: &str) -> Self {
        Self {
            status: status.to_string(),
            reason: None,
            credential: None,
            sweep: None,
        }
    }

    fn status(status: &str) -> Self {
        Self::new(status)
    }

    fn with_reason(mut self, reason: Option<String>) -> Self {
        self.reason = reason;
        self
    }

    fn present(credential: String) -> Self {
        Self {
            status: "present".to_string(),
            reason: None,
            credential: Some(credential),
            sweep: None,
        }
    }

    fn with_sweep(mut self, totals: &Totals, batches: u32, elapsed_ms: u128) -> Self {
        self.sweep = Some(serde_json::json!({
            "batches": batches,
            "elapsedMs": elapsed_ms,
            "entriesVisited": totals.entries_visited,
            "endpointsExamined": totals.endpoints_examined,
            "reaped": totals.reaped,
            "alreadyAbsent": totals.already_absent,
            "busy": totals.busy,
            "staleTarget": totals.stale_target,
            "unverified": totals.unverified,
            "ioErrors": totals.io_errors,
            "notApplicable": totals.not_applicable,
            "leasesRetired": totals.leases_retired,
            "roundComplete": totals.round_complete,
            "roundInterrupted": totals.round_interrupted,
            "namespaceChanged": totals.namespace_changed,
        }));
        self
    }

    fn emit(&self) -> Result<()> {
        let mut value = serde_json::Map::new();
        value.insert("status".to_string(), self.status.clone().into());
        value.insert(
            "reason".to_string(),
            match &self.reason {
                Some(reason) => reason.clone().into(),
                None => serde_json::Value::Null,
            },
        );
        if let Some(credential) = &self.credential {
            // The credential is embedded as the exact codec document, not
            // re-serialized from ad-hoc fields.
            value.insert(
                "credential".to_string(),
                serde_json::from_str(credential)
                    .map_err(|error| anyhow!("credential re-encoding failed: {error}"))?,
            );
        }
        if let Some(sweep) = &self.sweep {
            value.insert("sweep".to_string(), sweep.clone());
        }
        let line = serde_json::to_string(&serde_json::Value::Object(value))
            .map_err(|error| anyhow!("result serialization failed: {error}"))?;
        println!("{line}");
        Ok(())
    }
}

fn endpoint_for(address: &str) -> Result<LocalEndpoint> {
    LocalEndpoint::from_address(address).map_err(|error| anyhow!("invalid local endpoint: {error}"))
}
