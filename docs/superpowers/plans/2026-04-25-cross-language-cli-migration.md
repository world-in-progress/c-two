# Cross-Language CLI Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move `c3` out of the Python SDK into a root-level native CLI with an independent release pipeline.

**Architecture:** Add a root `cli/` Rust crate that owns the `c3` binary and embeds its banner asset. Reuse the existing `c2-http` relay server and HTTP registry endpoints instead of duplicating relay logic. Remove Python SDK CLI ownership, move banner generation to `tools/dev/`, add a source-checkout `c3_tool.py` helper, and keep Python package CD focused on Python wheels/sdist.

**Tech Stack:** Rust 2024, clap, c2-http relay feature, reqwest, serde_json, Python stdlib developer tooling, maturin, GitHub Actions.

---

## File Structure

- Create `cli/Cargo.toml`: root-level Rust CLI package.
- Create `cli/assets/banner_unicode.txt`: generated CLI banner asset.
- Create `cli/src/main.rs`: top-level clap parser and command dispatch.
- Create `cli/src/relay.rs`: `c3 relay` command using `c2_http::relay::RelayServer`.
- Create `cli/src/registry.rs`: `c3 registry` HTTP admin commands.
- Create `cli/src/version.rs`: version output helper.
- Create `tools/dev/c3_tool.py`: source-checkout development helper for building and linking local `c3`.
- Create `tools/dev/generate_banner.py`: developer-only banner generator.
- Modify `sdk/python/pyproject.toml`: remove Python-owned `c3` script and CLI package data.
- Modify `sdk/python/src/c_two/__init__.py`: remove package banner loading.
- Delete `sdk/python/src/c_two/banner_unicode.txt`: banner moves to CLI asset.
- Delete `sdk/python/src/c_two/cli.py`: old Python CLI implementation removed after native CLI parity.
- Delete `sdk/python/tests/unit/test_cli_relay.py`: Python SDK no longer owns CLI relay command tests.
- Modify `sdk/python/tests/unit/test_relay_graceful_shutdown.py`: remove old `c_two.cli` import coverage.
- Create `cli/tests/cli_help.rs`: Rust CLI smoke tests for help/version parsing.
- Create `cli/tests/registry_commands.rs`: Rust tests for registry command response formatting.
- Rename `.github/workflows/release.yml` to `.github/workflows/python-package-release.yml`.
- Create `.github/workflows/cli-release.yml`: canonical standalone `c3` release workflow.
- Modify `.github/workflows/python-package-release.yml`: keep Python package release isolated from standalone CLI artifacts.
- Modify `.github/workflows/ci.yml`: add native CLI build/help smoke checks.
- Modify `README.md`, `README.zh-CN.md`, `examples/README.md`, `.github/copilot-instructions.md`: describe `c3` as cross-language CLI and update workflow naming.
- Modify `core/Cargo.toml`: include `../cli` in the workspace only if Cargo accepts an outside-member path from `core`; otherwise keep `cli` as an independent package and use `cargo build --manifest-path cli/Cargo.toml`.
- Modify or remove `core/transport/c2-http/src/main.rs` and its `[[bin]]` entry: prevent a second relay CLI from diverging.

## Task 1: Scaffold Native CLI Crate

**Files:**
- Create: `cli/Cargo.toml`
- Create: `cli/src/main.rs`
- Create: `cli/src/version.rs`
- Create: `cli/tests/cli_help.rs`

- [ ] **Step 1: Write failing CLI smoke tests**

Create `cli/tests/cli_help.rs`:

```rust
use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn c3_help_lists_runtime_commands() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("relay"))
        .stdout(predicate::str::contains("registry"));
}

#[test]
fn c3_version_prints_package_version() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run:

```bash
cargo test --manifest-path cli/Cargo.toml --test cli_help
```

Expected: FAIL because `cli/Cargo.toml` and `c3` binary do not exist.

- [ ] **Step 3: Create `cli/Cargo.toml`**

Create `cli/Cargo.toml`:

```toml
[package]
name = "c2-cli"
version = "0.1.0"
edition = "2024"
description = "Cross-language command-line interface for C-Two"

[[bin]]
name = "c3"
path = "src/main.rs"

[dependencies]
anyhow = "1"
clap = { version = "4", features = ["derive", "env"] }
ctrlc = "3"
c2-config = { path = "../core/foundation/c2-config" }
c2-http = { path = "../core/transport/c2-http", features = ["relay"] }
percent-encoding = "2"
reqwest = { version = "0.12", default-features = false, features = ["blocking", "json", "rustls-tls"] }
serde_json = "1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

[dev-dependencies]
assert_cmd = "2"
predicates = "3"
tempfile = "3"
```

- [ ] **Step 4: Create minimal command parser**

Create `cli/src/main.rs`:

```rust
mod version;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "c3", version, about = "C-Two command-line interface")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Start the C-Two HTTP relay server.
    Relay,
    /// Query relay registry state.
    Registry,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Relay => {
            println!("relay command is not implemented yet");
        }
        Commands::Registry => {
            println!("registry command is not implemented yet");
        }
    }
    Ok(())
}
```

Create `cli/src/version.rs`:

```rust
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
```

- [ ] **Step 5: Run tests to verify they pass**

Run:

```bash
cargo test --manifest-path cli/Cargo.toml --test cli_help
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add cli/Cargo.toml cli/src/main.rs cli/src/version.rs cli/tests/cli_help.rs
git commit -m "feat(cli): scaffold native c3 command"
```

## Task 2: Move Banner Asset to Native CLI

**Files:**
- Create: `cli/assets/banner_unicode.txt`
- Create: `tools/dev/generate_banner.py`
- Modify: `cli/src/main.rs`
- Test: `cli/tests/cli_help.rs`

- [ ] **Step 1: Add failing banner test**

Append to `cli/tests/cli_help.rs`:

```rust
#[test]
fn c3_help_includes_embedded_banner() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("C-Two"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run:

```bash
cargo test --manifest-path cli/Cargo.toml --test cli_help c3_help_includes_embedded_banner
```

Expected: FAIL because the native CLI does not embed the generated banner yet.

- [ ] **Step 3: Move/generate banner asset**

Create `cli/assets/banner_unicode.txt` by copying the current generated asset:

```bash
mkdir -p cli/assets
cp sdk/python/src/c_two/banner_unicode.txt cli/assets/banner_unicode.txt
```

Create `tools/dev/generate_banner.py`:

```python
#!/usr/bin/env python3
from __future__ import annotations

import argparse
from pathlib import Path


def image_to_halfblock(img, width: int, threshold: int) -> str:
    from PIL import Image as _Img

    aspect = img.height / img.width
    height = int(width * aspect) // 2 * 2
    img = img.resize((width, height), _Img.Resampling.LANCZOS)

    lines: list[str] = []
    for y in range(0, height, 2):
        row: list[str] = []
        for x in range(width):
            top = img.getpixel((x, y)) > threshold
            bot = img.getpixel((x, y + 1)) > threshold if y + 1 < height else False
            if top and bot:
                row.append("█")
            elif top:
                row.append("▀")
            elif bot:
                row.append("▄")
            else:
                row.append(" ")
        lines.append("".join(row).rstrip())
    return "\n".join(lines)


def main() -> None:
    parser = argparse.ArgumentParser(description="Generate c3 CLI banner asset.")
    parser.add_argument("--width", "-w", type=int, default=50)
    parser.add_argument("--threshold", "-t", type=int, default=100)
    parser.add_argument("--image", "-i", type=Path, default=Path("docs/images/logo_bw.png"))
    parser.add_argument("--out", "-o", type=Path, default=Path("cli/assets/banner_unicode.txt"))
    args = parser.parse_args()

    from PIL import Image

    img_gray = Image.open(args.image).convert("L")
    art = image_to_halfblock(img_gray, width=args.width, threshold=args.threshold)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(art + "\n", encoding="utf-8")
    print(f"{args.out} ({len(art.splitlines())} lines)")


if __name__ == "__main__":
    main()
```

- [ ] **Step 4: Embed banner in CLI help**

Modify `cli/src/main.rs`:

```rust
mod version;

use anyhow::Result;
use clap::{Parser, Subcommand};

const BANNER: &str = include_str!("../assets/banner_unicode.txt");

#[derive(Debug, Parser)]
#[command(
    name = "c3",
    version,
    about = "C-Two command-line interface",
    before_help = BANNER,
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Start the C-Two HTTP relay server.
    Relay,
    /// Query relay registry state.
    Registry,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Relay => {
            println!("relay command is not implemented yet");
        }
        Commands::Registry => {
            println!("registry command is not implemented yet");
        }
    }
    Ok(())
}
```

- [ ] **Step 5: Run tests and generator smoke check**

Run:

```bash
cargo test --manifest-path cli/Cargo.toml --test cli_help
uv run python tools/dev/generate_banner.py --help
```

Expected: PASS and help text prints.

- [ ] **Step 6: Commit**

```bash
git add cli/assets/banner_unicode.txt tools/dev/generate_banner.py cli/src/main.rs cli/tests/cli_help.rs
git commit -m "feat(cli): embed banner asset"
```

## Task 3: Implement Native Relay Command

**Files:**
- Create: `cli/src/relay.rs`
- Modify: `cli/src/main.rs`
- Test: `cli/tests/cli_help.rs`
- Test: `cli/tests/relay_args.rs`

- [ ] **Step 1: Write failing relay help tests**

Create `cli/tests/relay_args.rs`:

```rust
use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn relay_help_exposes_mesh_and_idle_options() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args(["relay", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--bind"))
        .stdout(predicate::str::contains("--idle-timeout"))
        .stdout(predicate::str::contains("--seeds"))
        .stdout(predicate::str::contains("--relay-id"))
        .stdout(predicate::str::contains("--advertise-url"))
        .stdout(predicate::str::contains("--upstream"));
}

#[test]
fn relay_rejects_invalid_upstream_format() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args(["relay", "--upstream", "grid-ipc://server", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Expected NAME=ADDRESS"));
}

#[test]
fn relay_dry_run_accepts_valid_configuration() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args([
            "relay",
            "--bind",
            "127.0.0.1:9999",
            "--idle-timeout",
            "10",
            "--seeds",
            "http://127.0.0.1:8301,http://127.0.0.1:8302",
            "--relay-id",
            "relay-a",
            "--advertise-url",
            "http://relay-a:9999",
            "--upstream",
            "grid=ipc://server",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("relay-a"))
        .stdout(predicate::str::contains("grid=ipc://server"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cargo test --manifest-path cli/Cargo.toml --test relay_args
```

Expected: FAIL because `relay` options are not implemented.

- [ ] **Step 3: Implement relay parser and config**

Create `cli/src/relay.rs`:

```rust
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Result};
use c2_config::RelayConfig;
use c2_http::relay::RelayServer;
use clap::Args;

#[derive(Debug, Args)]
pub struct RelayArgs {
    #[arg(long, short = 'b', env = "C2_RELAY_BIND", default_value = "0.0.0.0:8080")]
    pub bind: String,

    #[arg(long = "upstream", short = 'u', value_parser = parse_upstream)]
    pub upstreams: Vec<(String, String)>,

    #[arg(long = "idle-timeout", env = "C2_RELAY_IDLE_TIMEOUT", default_value_t = 300)]
    pub idle_timeout_secs: u64,

    #[arg(long, short = 's', env = "C2_RELAY_SEEDS", value_delimiter = ',')]
    pub seeds: Vec<String>,

    #[arg(long = "relay-id", env = "C2_RELAY_ID")]
    pub relay_id: Option<String>,

    #[arg(long = "advertise-url", env = "C2_RELAY_ADVERTISE_URL")]
    pub advertise_url: Option<String>,

    #[arg(long, hide = true)]
    pub dry_run: bool,
}

pub fn parse_upstream(value: &str) -> Result<(String, String), String> {
    let Some((name, address)) = value.split_once('=') else {
        return Err(format!("Invalid upstream {value:?}. Expected NAME=ADDRESS"));
    };
    let name = name.trim();
    let address = address.trim();
    if name.is_empty() {
        return Err("Upstream name cannot be empty".to_string());
    }
    if address.is_empty() {
        return Err("Upstream address cannot be empty".to_string());
    }
    Ok((name.to_string(), address.to_string()))
}

pub fn run(args: RelayArgs) -> Result<()> {
    let config = RelayConfig {
        bind: args.bind.clone(),
        relay_id: args.relay_id.clone().unwrap_or_else(RelayConfig::generate_relay_id),
        advertise_url: args.advertise_url.clone().unwrap_or_default(),
        seeds: args.seeds.clone(),
        idle_timeout_secs: args.idle_timeout_secs,
        ..Default::default()
    };

    if args.dry_run {
        println!("bind={}", config.bind);
        println!("relay_id={}", config.relay_id);
        println!("advertise_url={}", config.effective_advertise_url());
        println!("idle_timeout={}", config.idle_timeout_secs);
        for (name, address) in &args.upstreams {
            println!("upstream={name}={address}");
        }
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let mut relay = RelayServer::start(config).map_err(|e| anyhow!("failed to start relay: {e}"))?;
    for (name, address) in args.upstreams {
        relay
            .register_upstream(&name, &address)
            .map_err(|e| anyhow!("failed to register upstream {name:?}: {e}"))?;
    }

    println!("C-Two relay listening at {}", display_url(&args.bind));
    println!("Press Ctrl+C to stop.");

    let (tx, rx) = std::sync::mpsc::channel();
    ctrlc::set_handler(move || {
        let _ = tx.send(());
    })
    .map_err(|e| anyhow!("failed to install Ctrl+C handler: {e}"))?;

    let _ = rx.recv();
    relay.stop().map_err(|e| anyhow!("failed to stop relay: {e}"))?;
    thread::sleep(Duration::from_millis(20));
    Ok(())
}

fn display_url(bind: &str) -> String {
    let Some((host, port)) = bind.rsplit_once(':') else {
        return format!("http://127.0.0.1:{bind}");
    };
    let host = if host == "0.0.0.0" || host == "::" {
        "127.0.0.1"
    } else {
        host
    };
    format!("http://{host}:{port}")
}
```

- [ ] **Step 4: Wire relay command in `main.rs`**

Modify `cli/src/main.rs`:

```rust
mod relay;
mod version;

use anyhow::Result;
use clap::{Parser, Subcommand};

const BANNER: &str = include_str!("../assets/banner_unicode.txt");

#[derive(Debug, Parser)]
#[command(
    name = "c3",
    version,
    about = "C-Two command-line interface",
    before_help = BANNER,
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Start the C-Two HTTP relay server.
    Relay(relay::RelayArgs),
    /// Query relay registry state.
    Registry,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Relay(args) => relay::run(args),
        Commands::Registry => {
            println!("registry command is not implemented yet");
            Ok(())
        }
    }
}
```

- [ ] **Step 5: Run relay tests**

Run:

```bash
cargo test --manifest-path cli/Cargo.toml --test relay_args
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add cli/src/main.rs cli/src/relay.rs cli/tests/relay_args.rs cli/Cargo.toml
git commit -m "feat(cli): implement native relay command"
```

## Task 4: Implement Native Registry Commands

**Files:**
- Create: `cli/src/registry.rs`
- Modify: `cli/src/main.rs`
- Test: `cli/tests/registry_commands.rs`

- [ ] **Step 1: Write failing registry command tests**

Create `cli/tests/registry_commands.rs`:

```rust
use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn registry_help_lists_subcommands() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args(["registry", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("list-routes"))
        .stdout(predicate::str::contains("resolve"))
        .stdout(predicate::str::contains("peers"));
}

#[test]
fn registry_requires_relay_url() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args(["registry", "list-routes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--relay"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cargo test --manifest-path cli/Cargo.toml --test registry_commands
```

Expected: FAIL because registry subcommands are not implemented.

- [ ] **Step 3: Implement registry commands**

Create `cli/src/registry.rs`:

```rust
use anyhow::{anyhow, Result};
use clap::{Args, Subcommand};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::Value;

#[derive(Debug, Args)]
pub struct RegistryArgs {
    #[command(subcommand)]
    pub command: RegistryCommand,
}

#[derive(Debug, Subcommand)]
pub enum RegistryCommand {
    /// List all registered routes on a relay.
    ListRoutes(RelayOnly),
    /// Resolve a resource name through a relay.
    Resolve(ResolveArgs),
    /// List known peer relays.
    Peers(RelayOnly),
}

#[derive(Debug, Args)]
pub struct RelayOnly {
    #[arg(long, short = 'r')]
    pub relay: String,
}

#[derive(Debug, Args)]
pub struct ResolveArgs {
    #[arg(long, short = 'r')]
    pub relay: String,
    pub name: String,
}

pub fn run(args: RegistryArgs) -> Result<()> {
    match args.command {
        RegistryCommand::ListRoutes(args) => list_routes(&args.relay),
        RegistryCommand::Resolve(args) => resolve(&args.relay, &args.name),
        RegistryCommand::Peers(args) => peers(&args.relay),
    }
}

fn get_json(relay: &str, path: &str) -> Result<Value> {
    let url = format!("{}/{}", relay.trim_end_matches('/'), path.trim_start_matches('/'));
    let builder = reqwest::blocking::Client::builder();
    let builder = if c2_http::relay_use_proxy() {
        builder
    } else {
        builder.no_proxy()
    };
    let client = builder
        .build()
        .map_err(|e| anyhow!("failed to build HTTP client: {e}"))?;
    let response = client
        .get(&url)
        .send()
        .map_err(|e| anyhow!("request failed for {url}: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(anyhow!("relay returned HTTP {status} for {url}"));
    }
    response
        .json::<Value>()
        .map_err(|e| anyhow!("invalid JSON from {url}: {e}"))
}

fn list_routes(relay: &str) -> Result<()> {
    let value = get_json(relay, "/_routes")?;
    let routes = value.get("routes").and_then(Value::as_array).cloned().unwrap_or_default();
    if routes.is_empty() {
        println!("No routes registered.");
        return Ok(());
    }
    for route in routes {
        if let Some(name) = route.get("name").and_then(Value::as_str) {
            println!("{name}");
        }
    }
    Ok(())
}

fn resolve(relay: &str, name: &str) -> Result<()> {
    let name = utf8_percent_encode(name, NON_ALPHANUMERIC).to_string();
    let value = get_json(relay, &format!("/_resolve/{name}"))?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn peers(relay: &str) -> Result<()> {
    let value = get_json(relay, "/_peers")?;
    let peers = value.as_array().cloned().unwrap_or_default();
    if peers.is_empty() {
        println!("No peers known.");
        return Ok(());
    }
    for peer in peers {
        let relay_id = peer.get("relay_id").and_then(Value::as_str).unwrap_or("?");
        let url = peer.get("url").and_then(Value::as_str).unwrap_or("?");
        let status = peer.get("status").and_then(Value::as_str).unwrap_or("unknown");
        println!("{relay_id} ({url}) - {status}");
    }
    Ok(())
}
```

- [ ] **Step 4: Wire registry command in `main.rs`**

Modify `cli/src/main.rs` command enum and match:

```rust
mod registry;
mod relay;
mod version;

use anyhow::Result;
use clap::{Parser, Subcommand};

const BANNER: &str = include_str!("../assets/banner_unicode.txt");

#[derive(Debug, Parser)]
#[command(
    name = "c3",
    version,
    about = "C-Two command-line interface",
    before_help = BANNER,
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Start the C-Two HTTP relay server.
    Relay(relay::RelayArgs),
    /// Query relay registry state.
    Registry(registry::RegistryArgs),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Relay(args) => relay::run(args),
        Commands::Registry(args) => registry::run(args),
    }
}
```

- [ ] **Step 5: Run registry tests**

Run:

```bash
cargo test --manifest-path cli/Cargo.toml --test registry_commands
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add cli/src/main.rs cli/src/registry.rs cli/tests/registry_commands.rs cli/Cargo.toml
git commit -m "feat(cli): implement registry commands"
```

## Task 5: Remove Old Rust Relay Binary Entrypoint

**Files:**
- Modify: `core/transport/c2-http/Cargo.toml`
- Delete: `core/transport/c2-http/src/main.rs`

- [ ] **Step 1: Verify duplicate binary exists**

Run:

```bash
rg -n "\\[\\[bin\\]\\]|c2-relay|src/main.rs" core/transport/c2-http
```

Expected: output includes `[[bin]]` in `core/transport/c2-http/Cargo.toml` and `core/transport/c2-http/src/main.rs`.

- [ ] **Step 2: Remove `c2-relay` bin stanza**

Modify `core/transport/c2-http/Cargo.toml` by deleting:

```toml
[[bin]]
name = "c2-relay"
path = "src/main.rs"
required-features = ["relay"]
```

- [ ] **Step 3: Delete old binary file**

Delete:

```text
core/transport/c2-http/src/main.rs
```

- [ ] **Step 4: Verify no duplicate relay CLI remains**

Run:

```bash
rg -n "c2-relay|\\[\\[bin\\]\\]" core/transport/c2-http
cargo check --manifest-path cli/Cargo.toml
cargo test -p c2-http --features relay --manifest-path core/Cargo.toml
```

Expected: no `c2-relay` bin stanza remains, CLI checks, and c2-http relay tests pass.

- [ ] **Step 5: Commit**

```bash
git add core/transport/c2-http/Cargo.toml
git rm core/transport/c2-http/src/main.rs
git commit -m "refactor: move relay binary ownership to c3 cli"
```

## Task 6: Remove Python SDK CLI Ownership

**Files:**
- Modify: `sdk/python/pyproject.toml`
- Modify: `.github/workflows/python-package-release.yml`
- Delete: `sdk/python/src/c_two/cli.py`
- Delete: `sdk/python/src/c_two/_cli_shim.py`
- Delete: `sdk/python/src/c_two/_bin/.gitkeep`
- Delete: `sdk/python/tests/unit/test_cli_relay.py`
- Modify: `sdk/python/tests/unit/test_python_package_release_workflow.py`
- Modify: `sdk/python/tests/README.md`

- [ ] **Step 1: Add regression checks**

Ensure Python packaging tests assert that:

- `.github/workflows/python-package-release.yml` does not build or inject `c3`;
- `sdk/python/pyproject.toml` has no `[project.scripts]` `c3` entry;
- `[tool.maturin]` does not package `src/c_two/_bin/*`;
- `build-backend` is plain `maturin`.

- [ ] **Step 2: Remove Python CLI files**

Delete the Python-owned CLI implementation and launcher files:

```text
sdk/python/src/c_two/cli.py
sdk/python/src/c_two/_cli_shim.py
sdk/python/src/c_two/_bin/.gitkeep
sdk/python/tests/unit/test_cli_relay.py
```

- [ ] **Step 3: Restore Python packaging to SDK-only**

Remove `[project.scripts]`, `_bin` package-data includes, and custom CLI build
backend plumbing from `sdk/python/pyproject.toml`. The Python SDK should build
only the Python extension and package code.

- [ ] **Step 4: Keep standalone CLI in CLI release workflow**

Remove all `Build c3 CLI` and `Inject c3 CLI into Python package` steps from
`.github/workflows/python-package-release.yml`. Standalone `c3` artifacts belong
to `.github/workflows/cli-release.yml`.

- [ ] **Step 5: Run tests**

```bash
uv run pytest sdk/python/tests/unit/test_python_package_release_workflow.py sdk/python/tests/unit/test_c3_tool.py -q --timeout=60
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add sdk/python/pyproject.toml .github/workflows/python-package-release.yml sdk/python/tests/unit/test_python_package_release_workflow.py sdk/python/tests/README.md
git rm sdk/python/src/c_two/cli.py sdk/python/src/c_two/_cli_shim.py sdk/python/src/c_two/_bin/.gitkeep sdk/python/tests/unit/test_cli_relay.py
git commit -m "refactor(python): remove sdk-owned c3 cli entrypoint"
```

## Task 7: Remove Python Runtime Banner Loading

**Files:**
- Modify: `sdk/python/src/c_two/__init__.py`
- Delete: `sdk/python/src/c_two/banner_unicode.txt`
- Test: `sdk/python/tests/unit/test_check_version.py`

- [ ] **Step 1: Write failing import test**

Append to `sdk/python/tests/unit/test_check_version.py`:

```python
def test_import_does_not_expose_logo_banner():
    import c_two

    assert not hasattr(c_two, "LOGO_UNICODE")
```

- [ ] **Step 2: Run test to verify it fails**

Run:

```bash
uv run pytest sdk/python/tests/unit/test_check_version.py::test_import_does_not_expose_logo_banner -q --timeout=60
```

Expected: FAIL because `c_two.LOGO_UNICODE` exists.

- [ ] **Step 3: Remove banner loading**

Modify `sdk/python/src/c_two/__init__.py` so it contains only runtime exports:

```python
from importlib.metadata import version
__version__ = version('c-two')

from . import error
from .crm.meta import crm, read, write, on_shutdown
from .crm.transferable import transferable
from .crm.transferable import transfer, hold, HeldResult
from .transport.registry import (
    set_config,
    set_server,
    set_client,
    set_relay,
    register,
    connect,
    close,
    unregister,
    server_address,
    shutdown,
    serve,
    hold_stats,
)
```

Delete:

```text
sdk/python/src/c_two/banner_unicode.txt
```

- [ ] **Step 4: Run import test**

Run:

```bash
uv run pytest sdk/python/tests/unit/test_check_version.py::test_import_does_not_expose_logo_banner -q --timeout=60
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add sdk/python/src/c_two/__init__.py sdk/python/tests/unit/test_check_version.py
git rm sdk/python/src/c_two/banner_unicode.txt
git commit -m "refactor(python): remove runtime banner asset"
```

## Task 8: Rename and Update Python Package Release Workflow

**Files:**
- Move: `.github/workflows/release.yml` -> `.github/workflows/python-package-release.yml`
- Modify: `.github/workflows/python-package-release.yml`

- [ ] **Step 1: Move workflow file and rename workflow**

Run:

```bash
git mv .github/workflows/release.yml .github/workflows/python-package-release.yml
```

Modify the first line of `.github/workflows/python-package-release.yml`:

```yaml
name: Python Package Release
```

- [ ] **Step 2: Keep Python release SDK-only**

Ensure `build-wheels.steps` installs Rust for the native extension and then runs
`PyO3/maturin-action@v1` directly. Do not add `Build c3 CLI`, `Inject c3 CLI
into Python package`, or any `sdk/python/src/c_two/_bin` packaging steps here;
standalone CLI artifacts belong to `.github/workflows/cli-release.yml`.

```yaml
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: ${{ matrix.target }}

      - uses: PyO3/maturin-action@v1
        with:
          command: build
          working-directory: sdk/python
```

- [ ] **Step 3: Keep existing maturin wheel build**

Ensure the existing `PyO3/maturin-action@v1` step still produces Python wheels:

```yaml
      - uses: PyO3/maturin-action@v1
        with:
          command: build
          working-directory: sdk/python
          target: ${{ matrix.target }}
          args: >-
            --release --out dist
            --interpreter python3.10 python3.11 python3.12
            python3.13 python3.14 python3.14t
          manylinux: auto
```

- [ ] **Step 4: Validate workflow syntax locally**

Run:

```bash
ruby -e 'require "yaml"; Dir[".github/workflows/*.yml"].each { |p| YAML.load_file(p) }; puts "workflow yaml ok"'
```

Expected: prints `workflow yaml ok`.

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/python-package-release.yml
git commit -m "ci: make python package release workflow build c3"
```

## Task 9: Add Independent CLI Release Workflow

**Files:**
- Create: `.github/workflows/cli-release.yml`
- Test: `sdk/python/tests/unit/test_cli_release_workflow.py`

- [ ] **Step 1: Add workflow structure tests**

Create `sdk/python/tests/unit/test_cli_release_workflow.py` to assert that the
CLI release workflow:

- is named `CLI Release`;
- supports `workflow_dispatch` and version tag pushes;
- builds Linux x86_64, Linux aarch64, macOS arm64, and macOS x86_64;
- packages `c3-${target}` and `c3-${target}.sha256`;
- publishes GitHub Release assets only from the dedicated CLI workflow.

- [ ] **Step 2: Create `.github/workflows/cli-release.yml`**

The workflow should build `cli/Cargo.toml` for each supported target, package
artifacts under `cli-dist/`, upload workflow artifacts named `cli-${target}`,
and publish `dist/c3-*` through `softprops/action-gh-release@v2` on tag pushes.

- [ ] **Step 3: Keep Python release isolated**

Ensure `.github/workflows/python-package-release.yml` only uploads Python wheels
and sdist artifacts. It may build and inject `c3` into wheels, but standalone
CLI artifact upload belongs to `cli-release.yml`.

- [ ] **Step 4: Validate workflow syntax**

Run:

```bash
ruby -e 'require "yaml"; Dir[".github/workflows/*.yml"].each { |p| YAML.load_file(p) }; puts "workflow yaml ok"'
```

Expected: prints `workflow yaml ok`.

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/cli-release.yml .github/workflows/python-package-release.yml sdk/python/tests/unit/test_cli_release_workflow.py sdk/python/tests/unit/test_python_package_release_workflow.py
git commit -m "ci: add independent cli release workflow"
```

## Task 10: Add Source-Checkout CLI Development Helper

**Files:**
- Create: `tools/dev/c3_tool.py`
- Modify: `.gitignore`
- Test: `sdk/python/tests/unit/test_c3_tool.py`

- [ ] **Step 1: Add tests for `c3_tool.py`**

Cover these behaviors:

- `--build` invokes `cargo build --manifest-path cli/Cargo.toml` and returns `cli/target/debug/c3`;
- `--link` links an existing binary without rebuilding;
- existing unrelated `c3` entries are not replaced unless `--force` is passed;
- the default link directory prefers repo `.bin` if already on `PATH`, otherwise a persistent cargo bin directory, otherwise repo `.bin`.

- [ ] **Step 2: Implement `tools/dev/c3_tool.py`**

The helper should support:

```bash
python tools/dev/c3_tool.py --build
python tools/dev/c3_tool.py --link
python tools/dev/c3_tool.py --build --link
python tools/dev/c3_tool.py --build --link --bin-dir ~/.cargo/bin
```

`--build` and `--link` are independent so opening a new terminal does not force
a rebuild when the binary already exists.

- [ ] **Step 3: Ignore local repository bin directory**

Add `.bin/` to `.gitignore`.

- [ ] **Step 4: Update developer docs**

Replace source-checkout relay instructions that call `cli/target/debug/c3`
directly with:

```bash
python tools/dev/c3_tool.py --build --link
c3 relay -b 127.0.0.1:8300
```

- [ ] **Step 5: Commit**

```bash
git add tools/dev/c3_tool.py sdk/python/tests/unit/test_c3_tool.py .gitignore README.md README.zh-CN.md examples/README.md
git commit -m "tools: add source checkout c3 development helper"
```

## Task 11: Add Native CLI CI Smoke Test

**Files:**
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Add CLI smoke test job**

Add a new job to `.github/workflows/ci.yml`:

```yaml
  cli:
    runs-on: ubuntu-latest
    name: cli
    steps:
      - uses: actions/checkout@v6
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
        with:
          workspaces: |
            core -> target
            cli -> target
      - name: Build c3
        run: cargo build --manifest-path cli/Cargo.toml
      - name: Smoke test c3
        run: |
          cli/target/debug/c3 --version
          cli/target/debug/c3 --help
          cli/target/debug/c3 relay --help
          cli/target/debug/c3 registry --help
```

- [ ] **Step 2: Validate workflow syntax**

Run:

```bash
ruby -e 'require "yaml"; Dir[".github/workflows/*.yml"].each { |p| YAML.load_file(p) }; puts "workflow yaml ok"'
```

Expected: prints `workflow yaml ok`.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: add native cli smoke tests"
```

## Task 10: Update Documentation

**Files:**
- Modify: `README.md`
- Modify: `README.zh-CN.md`
- Modify: `examples/README.md`
- Modify: `.github/copilot-instructions.md`

- [ ] **Step 1: Replace Python-owned CLI language**

In `README.md`, update the CLI description to:

```markdown
The `c3` command is C-Two's cross-language native CLI. Python wheels include a
thin launcher that executes the packaged `c3` binary.
```

Keep relay examples using:

```bash
c3 relay --bind 0.0.0.0:8080
```

- [ ] **Step 2: Update Chinese README**

In `README.zh-CN.md`, add:

```markdown
`c3` 是 C-Two 的跨语言原生 CLI。Python wheel 只提供一个很薄的启动器，
实际命令逻辑由随包分发的 `c3` 二进制执行。
```

- [ ] **Step 3: Update examples README**

In `examples/README.md`, ensure the relay section says:

```markdown
`c3` is the native C-Two CLI. From a source checkout, build it once and link it
into a development bin directory:
```

- [ ] **Step 4: Update Copilot instructions**

In `.github/copilot-instructions.md`, replace the old CLI section with:

```markdown
### CLI
The `c3` command is implemented by the root `cli/` Rust package. Python does
not own CLI behavior. For source-checkout development, use
`python tools/dev/c3_tool.py --build --link` to build and link a local `c3`.
Do not add CLI command behavior under `sdk/python/src/c_two`.
```

- [ ] **Step 5: Run docs grep**

Run:

```bash
rg -n "c_two\\.cli|dev generate-banner|Python CLI|sdk/python/src/c_two/cli.py" README.md README.zh-CN.md examples .github sdk/python/README.md
```

Expected: no stale user-facing instructions that describe Python as the CLI owner. Historical docs under `docs/superpowers/plans/` may still mention old paths.

- [ ] **Step 6: Commit**

```bash
git add README.md README.zh-CN.md examples/README.md .github/copilot-instructions.md
git commit -m "docs: document native cross-language c3 cli"
```

## Task 11: Final Verification

**Files:**
- All files touched by Tasks 1-10.

- [ ] **Step 1: Run Rust CLI tests**

Run:

```bash
cargo test --manifest-path cli/Cargo.toml
```

Expected: PASS.

- [ ] **Step 2: Run Rust core tests affected by relay reuse**

Run:

```bash
cargo test --manifest-path core/Cargo.toml -p c2-http --features relay
cargo check --manifest-path core/Cargo.toml -p c2-ffi --features python
```

Expected: PASS.

- [ ] **Step 3: Run Python tests affected by CLI migration**

Run:

```bash
C2_RELAY_ADDRESS= uv run pytest \
  sdk/python/tests/unit/test_python_package_release_workflow.py \
  sdk/python/tests/unit/test_c3_tool.py \
  sdk/python/tests/unit/test_relay_graceful_shutdown.py \
  sdk/python/tests/unit/test_check_version.py \
  sdk/python/tests/integration/test_python_examples.py \
  -q --timeout=60
```

Expected: PASS.

- [ ] **Step 4: Run workflow syntax validation**

Run:

```bash
ruby -e 'require "yaml"; Dir[".github/workflows/*.yml"].each { |p| YAML.load_file(p) }; puts "workflow yaml ok"'
```

Expected: prints `workflow yaml ok`.

- [ ] **Step 5: Run stale-code scans**

Run:

```bash
rg -n "from c_two\\.cli|import c_two\\.cli|sdk/python/src/c_two/cli.py|c3 dev generate-banner|LOGO_UNICODE" sdk/python/src sdk/python/tests README.md README.zh-CN.md examples .github sdk/python/README.md
rg -n "c2-relay|core/transport/c2-http/src/main.rs" core cli .github README.md README.zh-CN.md examples
```

Expected: no stale runtime references. Mentions inside historical docs are acceptable only under `docs/superpowers/`.

- [ ] **Step 6: Run diff whitespace check**

Run:

```bash
git diff --check
```

Expected: no output.

- [ ] **Step 7: Commit final verification notes if docs changed**

If verification reveals docs-only fixes, commit them:

```bash
git add <fixed-doc-files>
git commit -m "docs: clean up cli migration references"
```

Expected: no commit needed if prior tasks were complete.

---

## Self-Review

- Spec coverage: the plan covers root `cli/`, SDK shims, banner asset ownership, Python CD workflow renaming/injection, standalone artifacts, CI smoke tests, docs, and Python banner removal.
- Placeholder scan: no `TBD` or unspecified implementation steps remain.
- Type consistency: Rust modules use `relay::RelayArgs`, `registry::RegistryArgs`, and `c3` binary naming consistently across tasks.
