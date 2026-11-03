# C-Two CLI (C3)

C3 **0.6.0** accompanies C-Two **0.10.x**, sharing its IPC handshake 14, Unix namespace format 3 and endpoint maintenance behavior. See the [release notes](../docs/releases/0.10.0.md).

Python does not own CLI behavior; do not add command behavior under `sdk/python/src/c_two`. `cli/` contains the Rust crate for `c3`, the native C-Two command-line interface. It starts relay servers and inspects relay registry state for C-Two deployments.

This guide covers the native c3 interface. Configuration is documented in the [configuration guide](../docs/reference/configuration.md); endpoint directories and their maintenance are described in the [lifecycle guide](../docs/reference/local-endpoint-lifecycle.md).

## Scope

`c3` owns product-level runtime commands:

- `c3 relay` starts the HTTP relay used for cross-machine discovery.
- `c3 registry list-routes` lists route names registered with a relay.
- `c3 registry peers` lists peer relays known by a mesh relay.
- `c3 endpoint inspect/reap/sweep` projects native local endpoint maintenance.
- `c3 contract` validates/releases `c-two.contract.v2` descriptors and composes complete Rust, Python, or TypeScript project trees.

## Build and install for development

Building c3 requires Rust 1.95 or newer. Run CLI development commands from the repository root. Build the debug binary with:

```bash
cargo build --manifest-path cli/Cargo.toml
```

For a release binary:

```bash
cargo build --manifest-path cli/Cargo.toml --release
```

The debug binary is written to `cli/target/debug/c3`; the release binary is written to `cli/target/release/c3`.

For repeated source-checkout use, build and link the local binary with the repository helper:

```bash
python tools/dev/c3_tool.py --build --link
c3 --version
```

`tools/dev/c3_tool.py` links into a PATH directory when possible, such as `~/.cargo/bin`; otherwise it falls back to the repository-local `.bin/` directory and prints the PATH entry to add.

Run the binary without linking it by using Cargo directly:

```bash
cargo run --manifest-path cli/Cargo.toml -- --help
cargo run --manifest-path cli/Cargo.toml -- relay --help
```

## Portable contracts

Python CRM classes can export an explicit `c-two.contract.v2` descriptor. The Rust CLI validates that outer contract, derives a route-independent release reference, and delegates each opaque nested FastDB specification to the official FastDB Core projection:

```bash
c3 contract export mypkg.contracts:Echo \
  --python .venv/bin/python \
  --out echo.contract.json
c3 contract validate echo.contract.json
c3 contract release-ref echo.contract.json --out echo.release-ref.json
```

Generate one complete project tree at an absent destination:

```bash
c3 contract codegen rust echo.contract.json --out-dir generated-rust
c3 contract codegen python echo.contract.json --out-dir generated-python
c3 contract codegen typescript echo.contract.json --out-dir generated-typescript
```

The output contains C-Two contract/release/composition metadata, the target-specific C-Two adapter, and FastDB Core-owned payload modules. Generation is fail-closed: invalid or colliding paths, hash mismatches, FastDB errors, and existing destinations produce no partial winning tree. There is no separate payload sidecar input.

`c3 contract diagnose` reports methods that remain Python-only. Portable payload structure is authored explicitly with `@cc.transfer(...)`.

## Configuration

The shared Rust config resolver loads runtime environment variables from `.env` in the current working directory before parsing command-line overrides. Set `C2_ENV_FILE` to load a different file; set `C2_ENV_FILE=""` to disable env-file loading:

```bash
C2_ENV_FILE=relay.env c3 relay --dry-run
```

Command-line flags override environment-backed defaults.

Color output follows common terminal conventions:

- `NO_COLOR` disables the colored banner.
- `CLICOLOR=0` disables the colored banner.
- `CLICOLOR_FORCE=1` forces banner color even when stdout is not a terminal, unless `NO_COLOR` is also set.

## Relay

Start a relay:

```bash
c3 relay --bind 0.0.0.0:8080
```

Relay HTTP and mesh endpoints are not public-facing APIs. Bind to `0.0.0.0` only inside a trusted deployment boundary, and restrict access with private networking, firewall rules, Kubernetes NetworkPolicy, service mesh policy, or ingress authentication.

Useful options:

| Option | Environment | Default | Purpose |
|---|---|---:|---|
| `--bind`, `-b` | `C2_RELAY_BIND` | `0.0.0.0:8080` | HTTP listen address. |
| `--idle-timeout` | `C2_RELAY_IDLE_TIMEOUT` | `60` | Seconds before idle upstream IPC connections are evicted. Use `0` to disable time-based eviction. |
| `--seeds`, `-s` | `C2_RELAY_SEEDS` | empty | Comma-separated seed relay URLs for mesh mode. |
| `--relay-id` | `C2_RELAY_ID` | generated | Stable relay identifier for the mesh protocol. |
| `--advertise-url` | `C2_RELAY_ADVERTISE_URL` | derived | Public URL other relays should use to reach this relay. |
| `--ipc-root` | `C2_IPC_ROOT` | `/tmp/c2-v3-<uidhex>` (Unix) | Final owner-controlled endpoint directory; custom directories are application-provisioned, and `0755` is accepted when other identities cannot modify them. Not applicable on Windows, where upstream IPC uses TokenUser-scoped Named Pipes. |
| `--ipc-pool-enabled <true\|false>` | `C2_IPC_POOL_ENABLED` | Rust resolver | Enable buddy for data-plane upstream IPC; `false` still permits dedicated SHM. |
| `--ipc-shm-backing-budget-bytes` | `C2_IPC_SHM_BACKING_BUDGET_BYTES` | 8 GiB | Shared upstream buddy/dedicated backing budget. |
| `--ipc-file-backing-budget-bytes` | `C2_IPC_FILE_BACKING_BUDGET_BYTES` | 16 GiB | Shared upstream file backing budget. |
| `--ipc-live-reassembly-budget-bytes` | `C2_IPC_LIVE_REASSEMBLY_BUDGET_BYTES` | 8 GiB | Shared upstream reassembly/retention capacity budget. |
| `--call-max-outstanding` | `C2_CALL_MAX_OUTSTANDING` | 1,024 | Outstanding forwarding transactions; zero refuses all new forwarding. |
| `--call-retained-input-budget-bytes` | `C2_CALL_RETAINED_INPUT_BUDGET_BYTES` | 16 GiB | Retained forwarding input; zero refuses positive input. |
| `--upstream`, `-u` | none | empty | Pre-register an upstream as `NAME=SERVER_ID@ADDRESS`. `SERVER_ID` must match the IPC server handshake identity. Repeatable. |

Examples:

```bash
c3 relay --bind 127.0.0.1:8080
c3 relay --bind 0.0.0.0:8080 --upstream grid=server@ipc://server
c3 relay --relay-id relay-a --advertise-url http://relay-a:8080 --seeds http://relay-b:8080,http://relay-c:8080
```

Use `--dry-run` to validate relay configuration and print the effective values without starting the server:

```bash
c3 relay --bind 127.0.0.1:9999 --idle-timeout 10 --dry-run
```

The relay resolves and freezes its own upstream IPC policy at startup; an application's `cc.set_client()` does not configure this separate process. All data-plane upstream request/reassembly pools share the relay's budget, including after idle eviction and reconnect. Zero budgets reject positive reservations, and capacity refusal follows the existing transport error paths. Budget charges remain until storage is actually released. Control registration proof/watch contexts, peer mappings and HTTP buffers are outside this budget. Other IPC fields use the Rust resolver's environment inputs, including `C2_IPC_POOL_PREWARM_SEGMENTS`, `C2_IPC_POOL_MIN_RETAINED_SEGMENTS` and `C2_IPC_POOL_DECAY_SECONDS`. Disabled buddy requires zero prewarm.

Forwarding limits freeze at startup and apply to every business request, including callers with unlimited waits. Known input lengths reserve capacity before request transfer; unknown lengths charge checked growth as bytes arrive. Capacity refusal does not withdraw a route. These limits are independent of the upstream storage budgets and do not measure process RSS.

Each forwarding transaction owns its input and upstream operation after its HTTP waiter disconnects. Ctrl+C closes admission and waits for actual forwarding completion and native-client cleanup. A resource method that never returns can keep shutdown pending; disconnecting the caller does not cancel that method.

```bash
c3 relay --bind 127.0.0.1:8080 --idle-timeout 10 \
  --ipc-pool-enabled false \
  --ipc-shm-backing-budget-bytes 134217728 \
  --ipc-file-backing-budget-bytes 268435456 \
  --ipc-live-reassembly-budget-bytes 134217728
```

Idle eviction disconnects the relay's cached IPC connection, not the resource server. Reconnect preserves full contract and server/instance identity validation; it does not replay ambiguous data-plane failures. Core relay-aware clients make one route observation followed by at most one pre-dispatch refresh; this bound is fixed and is not configurable through SDK settings. See the [memory policy](../docs/reference/memory-policy.md) for lazy allocation, fallback and retained-owner accounting.

When running normally, `c3 relay` installs a Ctrl+C handler and stops the relay cleanly on interrupt.

## Local endpoint maintenance

Only pass logical addresses; Rust selects the OS endpoint. Credentials carry format metadata, never a backend choice. On Unix, save the credential after server readiness, then confirm the child has exited with an OS wait before reaping:

```bash
c3 endpoint inspect ipc://this-run-server > endpoint-inspection.json
python3 -c 'import json; r=json.load(open("endpoint-inspection.json")); assert r["status"] == "present"; print(json.dumps(r["credential"]))' > endpoint-credential.json
# After the supervisor has confirmed child exit:
c3 endpoint reap ipc://this-run-server --credential endpoint-credential.json
c3 endpoint sweep --address ipc://this-run-server --max-entries 64 --max-ms 10 --max-batches 64
```

Inspect, reap and sweep accept `--ipc-root` (or `C2_IPC_ROOT`) to address endpoints in a custom final Unix directory. The application provisions it with current-user ownership, read/write/traverse access and no write access for other identities; `0755` is accepted, while default directories are created `0700`. The CLI captures the endpoint context with the inspection and reap operates strictly on that captured context, so a credential from one root never touches an object under another. Custom-root routes discovered through a relay answer over HTTP when the relay's local namespace does not match; the namespace hint is a routing-domain signal, not an authentication credential. On Windows these commands keep their TokenUser-scoped Named Pipe behavior and `--ipc-root` is rejected as not applicable.

Unix endpoints use namespace disk format 3 and credential schema 4 without `device`. Default directory names include the disk format version; custom roots gain no suffix. Old default directories remain untouched, and old Unix credential schemas are rejected. Upgrade local SDKs, c3 and Node consumers together; follow the [0.10.0 migration notes](../docs/releases/0.10.0.md) before rebuilding a custom root that reports `unsupported-namespace-format`.

Inspect reports observation, not process liveness. Reap refuses stale or unverified objects. Keep sweep addresses scoped to the current run; report completion only when CLI JSON `sweep.roundComplete` is true, not merely when the batch limit is reached. The millisecond budget is a scheduling target, not hard real-time filesystem behavior. Windows reports kernel-managed/not-applicable cleanup. See the [lifecycle guide](../docs/reference/local-endpoint-lifecycle.md); do not remove unknown historical directories based on age, PID or connection failure.

## Registry

Registry commands query an already-running relay over HTTP. Pass the relay URL with `--relay` or `-r`.

List registered routes:

```bash
c3 registry list-routes --relay http://127.0.0.1:8080
```

List known mesh peers:

```bash
c3 registry peers --relay http://127.0.0.1:8080
```

Registry requests use a five-second HTTP timeout. Malformed route or peer responses fail the command with a JSON error. By default, the CLI bypasses system proxy settings for relay inspection unless the relay layer is configured to use proxies.

## Development

Run all CLI tests:

```bash
cargo test --manifest-path cli/Cargo.toml
```

Useful narrower checks:

```bash
cargo test --manifest-path cli/Cargo.toml --test cli_help
cargo test --manifest-path cli/Cargo.toml --test relay_args
cargo test --manifest-path cli/Cargo.toml --test registry_commands
```

Run the Rust core tests separately when CLI changes touch shared relay behavior under `core/`:

```bash
cargo test --manifest-path core/Cargo.toml --workspace
```

The CLI embeds its banner at compile time from `cli/assets/banner_unicode.txt`:

```rust
const BANNER: &str = include_str!("../assets/banner_unicode.txt");
```

Regenerate the banner from `docs/assets/logo_bw.png` with:

```bash
uv run --python 3.11 --group banner tools/dev/generate_banner.py
```

## Release

The coordinated [c3 0.6.0 release entry](https://github.com/world-in-progress/c-two/releases/tag/c3-v0.6.0) uses the following binary target convention. Installation and checksum verification are in the [Windows guide](../docs/reference/windows-native-usage.md):

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `aarch64-apple-darwin`
- `x86_64-apple-darwin`
- `x86_64-pc-windows-msvc`
- `aarch64-pc-windows-msvc`

Unix assets are named `c3-${target}`; Windows uses `c3-${target}.exe`. Each has a matching `.sha256`; installers verify it before installation. The matching release's `rc-manifest.json` identifies the artifact source.

### Linux / macOS

```bash
curl -fsSL https://github.com/world-in-progress/c-two/releases/latest/download/c3-installer.sh | sh -s -- --version 0.6.0
```

The installer detects Linux/macOS and x86_64/aarch64. It defaults to `/usr/local/bin` as root or `~/.local/bin` otherwise. Select another directory:

```bash
curl -fsSL https://github.com/world-in-progress/c-two/releases/latest/download/c3-installer.sh | sh -s -- --version 0.6.0 --bin-dir "$HOME/bin"
```

### Windows x64 / ARM64

Download the PowerShell installer and select the coordinated version explicitly:

```powershell
Invoke-WebRequest -UseBasicParsing -Uri 'https://github.com/world-in-progress/c-two/releases/latest/download/c3-installer.ps1' -OutFile .\c3-installer.ps1
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\c3-installer.ps1 -Version 0.6.0
$env:PATH = "$env:LOCALAPPDATA\Programs\c3;$env:PATH"
c3.exe --version
```

PowerShell 5.1 is supported. Automatic architecture detection uses Windows `IsWow64Process2` (Windows 10 version 1709 / Windows Server 2016 version 1709 or later), including from emulated shells. The default per-user directory is `$env:LOCALAPPDATA\Programs\c3`; `-BinDir` selects another directory. The installer verifies SHA-256 and copies the binary to `c3.exe`, without requiring an administrator or persistently registering PATH. The example adds it to the current session only. Architecture selection follows the operating system, including when PowerShell runs through x64 emulation on Windows ARM64. For direct exe download, checksum verification and relay execution, see [Windows installation](../docs/reference/windows-native-usage.md#install-python-and-c3).
