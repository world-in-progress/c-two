# C-Two CLI (C3)

C3 **0.8.0** accompanies C-Two **0.12.x**, sharing its IPC 17, SHM 4, C/FFI ABI 9, relay mesh 7, Unix namespace format 3 and endpoint maintenance behavior. See the [0.12.0 release notes](../docs/releases/0.12.0.md).

Current source builds use IPC 18, C/FFI ABI 10 and relay mesh 9. `c3 relay` forwards keyed unary submission, recovery and outcome queries over the fixed HTTP endpoints defined in [ADR 0007](../docs/decisions/0007-call-metadata-and-admission-hooks.md).

Python does not own CLI behavior; do not add command behavior under `sdk/python/src/c_two`. `cli/` contains the Rust crate for `c3`, the native C-Two command-line interface. It starts relay servers and inspects relay registry state for C-Two deployments.

This guide covers the native c3 interface. Configuration is documented in the [configuration guide](../docs/reference/configuration.md); endpoint directories and their maintenance are described in the [lifecycle guide](../docs/reference/local-endpoint-lifecycle.md).

## Scope

`c3` owns product-level runtime commands:

- `c3 relay` starts the HTTP relay used for cross-machine discovery.
- `c3 bridge` starts a private rendezvous and encrypted packet forwarding server for iroh relay peers.
- `c3 registry list-routes` lists route names registered with a relay.
- `c3 registry peers` lists peer relays known by a mesh relay.
- `c3 registry force-leave` makes a mesh relay forget an unreachable relay.
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
c3 contract codegen java echo.contract.json --out-dir generated-java
```

The output contains C-Two contract/release/composition metadata, the target-specific C-Two adapter, and FastDB Core-owned payload modules. All four targets generate server-stream methods. TypeScript generates client bindings where `method_{index}_{name}` returns a `C2ResponseStream` and `hold_method_{index}_{name}` yields each item as a `C2HeldResult`. Java generates service and client bindings for JDK 17. Add `generated-java/java/src/main/java` as one source root and compile with the [C-Two Java SDK](../sdk/java/README.md) and FastDB Java 0.4.0. The Java source root contains both the C-Two adapter and the FastDB Core payload classes. Invalid or colliding paths, hash mismatches, FastDB errors, and existing destinations leave the destination unpublished. The descriptor supplies the nested FastDB specifications.

`c3 contract export` and `c3 contract diagnose` accept `--python` (resolved in order `--python` → process environment `C2_PYTHON` → `python3`), repeatable `--method` filters, `--out` (default stdout) and `--pretty`; `release-ref` also accepts `--pretty`, and `validate`, `release-ref` and `codegen` accept `-` as the descriptor path to read JSON from stdin.

`c3 contract diagnose` reports methods that remain Python-only. Portable payload structure is authored explicitly with `@cc.transfer(...)`.

## Configuration

Relay and bridge startup and `--dry-run` use the same Rust configuration resolver and capture the environment catalog once per invocation. Set `C2_ENV_FILE` to load a different file from the default `.env` in the current directory, or set it to an empty string to disable file loading.

```bash
C2_ENV_FILE=relay.env c3 relay --dry-run
```

Resolution order is explicit CLI flags, process environment, the environment file, then defaults. [Configuration](../docs/reference/configuration.md)

Color output follows common terminal conventions (checked in priority order):

- `NO_COLOR` disables the colored banner.
- `CLICOLOR=0` disables the colored banner.
- Any set `CLICOLOR_FORCE` forces banner color even when stdout is not a terminal, unless `NO_COLOR` or `CLICOLOR=0` is set.

## Relay

Start a relay:

```bash
c3 relay --bind 127.0.0.1:8080
```

The loopback listener serves Host registration, administration, local Mesh 9 peers, discovery and RPC. Add `--edge-bind` for external clients or a reverse proxy. The edge listener serves discovery and RPC without exposing local IPC addresses or server instance identity, even on a loopback bind.

Useful options:

| Option | Environment | Default | Purpose |
|---|---|---:|---|
| `--bind`, `-b` | `C2_RELAY_BIND` | `127.0.0.1:8080` | Loopback HTTP control address. Port `0` selects a free port. |
| `--edge-bind` | `C2_RELAY_EDGE_BIND` | unset | Optional business and discovery listen address. A non-loopback address defaults to TLS. |
| `--tls-cert`, `--tls-key` | `C2_RELAY_TLS_CERT`, `C2_RELAY_TLS_KEY` | unset | Edge PEM certificate chain and private key. Supply both. |
| `--tls-auto` | `C2_RELAY_TLS_AUTO` | false | Select a persistent self-signed certificate, including on loopback. |
| `--tls-san` | `C2_RELAY_TLS_SAN` | empty | Comma-separated additional DNS names and IP addresses for automatic TLS. |
| `--state-dir` | `C2_RELAY_STATE_DIR` | `~/.c3-relay` | Private directory for generated edge TLS credentials and the mesh endpoint key. Create its parent before startup. |
| `--insecure-plaintext` | `C2_RELAY_INSECURE_PLAINTEXT` | false | Explicitly allow a non-loopback plaintext edge. |
| `--seeds`, `-s` | `C2_RELAY_SEEDS` | empty | Comma-separated loopback HTTP URLs or `iroh://<endpoint-id>[@<ip:port>]` seed addresses. |
| `--bridge` | `C2_RELAY_BRIDGE` | unset | Private HTTPS bridge URL with optional certificate pin and QUIC address discovery port. |
| `--bridge-token` | `C2_RELAY_BRIDGE_TOKEN` | unset | Required with a bridge. Printable ASCII without whitespace. |
| `--cluster-secret` | `C2_RELAY_CLUSTER_SECRET` | unset | Shared secret for iroh peer authentication. Required with a bridge or iroh seed and distinct from the bridge token. |
| `--relay-id` | `C2_RELAY_ID` | generated | Stable relay identifier. With iroh enabled, the default derives from the persistent endpoint key. |
| `--advertise-url` | `C2_RELAY_ADVERTISE_URL` | derived from the bound address | Public URL clients use for this relay, including a reverse proxy URL. |
| `--ipc-root` | `C2_IPC_ROOT` | `/tmp/c2-v3-<uidhex>` (Unix) | Final owner-controlled endpoint directory; custom directories are application-provisioned, and `0755` is accepted when other identities cannot modify them. Not applicable on Windows, where upstream IPC uses TokenUser-scoped Named Pipes. |
| `--memory-budget-bytes` | `C2_MEMORY_BUDGET_BYTES` | 16 GiB | Memory for the relay's business forwarding and upstream IPC storage. Zero closes new forwarding admission. |
| `--file-spill-budget-bytes` | `C2_FILE_SPILL_BUDGET_BYTES` | 16 GiB | Temporary file-backed reassembly capacity. Zero disables file spill. |
| `--call-max-outstanding` | `C2_CALL_MAX_OUTSTANDING` | 0 if `M == 0`, else `max(64, floor(M / 65536))` | Concurrent forwarding transactions. Zero refuses all new forwarding. |
| `--upstream`, `-u` | none | empty | Pre-register an upstream as `NAME=SERVER_ID@ADDRESS`. `SERVER_ID` must match the IPC server handshake identity. Repeatable. |

Examples:

```bash
c3 relay --bind 127.0.0.1:8080
c3 relay --bind 127.0.0.1:8080 --upstream grid=server@ipc://server
c3 relay --edge-bind 0.0.0.0:8443 --advertise-url https://relay.example.org:8443
c3 relay --bind 127.0.0.1:8081 --relay-id relay-b --seeds http://127.0.0.1:8080
```

Loopback edge listeners default to plaintext for a same-host reverse proxy. Cleartext listeners support HTTP/1.1 and HTTP/2 prior knowledge. TLS listeners negotiate `h2` or `http/1.1` with ALPN. Select an explicit certificate and key, an automatically generated identity, or plaintext. The relay prints the bound addresses and the dialable edge URL. Give it `--advertise-url` for a public hostname or IP when binding a wildcard. Without that setting, a wildcard produces a same-family loopback URL.

Automatic TLS generates `edge-cert.pem` and `edge-key.pem` in the state directory. The directory and generated files use Unix modes `0700` and `0600` or a protected Windows DACL for the current TokenUser. Each relay needing a distinct identity uses a separate state directory. The leaf certificate includes the advertised host, configured SANs, the non-wildcard edge IP and loopback names and addresses. Subsequent starts reuse the files and print the same leaf DER fingerprint. Configure clients with `C2_RELAY_CERT_SHA256=sha256:<hex>` for a pinned identity or `C2_RELAY_CA_CERT` for a custom PEM trust root. Configure Host registration with the loopback URL.

Use `--dry-run` to validate and print the resolved listener and TLS options, budgets and slot limit. It keeps configured ports, including `0`, and exits before opening listeners, loading or generating certificates, or registering upstreams.

```bash
c3 relay --bind 127.0.0.1:9999 --memory-budget-bytes 134217728 --dry-run
```

The relay freezes one role domain at startup, shared by all business forwards and upstream IPC connections, including reconnects. Its memory budget `M` covers request bodies, forwarding windows and upstream IPC storage. The first reservation may wait for capacity. Pre-admission error replies have a bounded control response if memory is unavailable. A response reuses its reserved download window across slices. Capacity refusal returns HTTP 503 for exhausted call slots and leaves the route registered. A positive call limit with zero memory is invalid. [Relay memory policy](../docs/reference/memory-policy.md#standalone-relay)

Each forwarding transaction owns its input and upstream operation after its HTTP waiter disconnects. Disconnecting cancels nothing for a unary call; for a server stream the relay submits cancellation upstream when the caller drops the response. Ctrl+C closes admission, ends the server streams the relay forwards between their steps with `CallCancelled` naming relay shutdown, and waits for actual forwarding completion and native-client cleanup; unary calls already forwarded still finish. A resource method that never returns, or a stream client that has stopped reading, can keep shutdown pending.

```bash
c3 relay --bind 127.0.0.1:8080 --memory-budget-bytes 134217728 --file-spill-budget-bytes 268435456 --call-max-outstanding 128
```

The relay evicts unreferenced upstream IPC connections after 60 seconds without stopping the Host. Reconnect preserves full contract and server/instance identity validation; it does not replay ambiguous data-plane failures. Before a call hands its input to a transport, Core acquires the client's route again at most once, under that call's deadline, when its target is known to be gone; this bound is fixed and is not configurable through SDK settings. See the [memory policy](../docs/reference/memory-policy.md) for lazy allocation, fallback and retained-owner accounting.

When running normally, `c3 relay` installs a Ctrl+C handler and stops the relay cleanly on interrupt.

`c3 relay` also serves resource-owned topic subscriptions ([ADR 0005](../docs/decisions/0005-publish-subscribe.md)). Each local route's declared topics become available at `POST /_c2/topics/subscribe?route=<pct>&topic=<pct>` with `application/vnd.c-two.topic-stream;version=2`. The relay opens one shared upstream IPC subscription per registration and topic and fans out a single budgeted copy to all downstream HTTP subscribers, including across two-tier cross-relay hops. Ctrl+C ends forwarded subscriptions with `CallCancelled` naming relay shutdown after the frame each subscriber already selected. See [relay topic forwarding](../docs/reference/contracts-and-routing.md#relay-topic-forwarding).

## Bridge

Run `c3 bridge` on a reachable machine to give relay endpoints a common place to discover addresses and forward encrypted packets when a direct link is unavailable. It accepts authenticated bridge clients over HTTPS and offers QUIC address discovery over UDP. It holds the bridge token and TLS identity.

| Option | Environment | Default |
|---|---|---|
| `--bind` | `C2_BRIDGE_BIND` | `0.0.0.0:8443` |
| `--quic-bind` | `C2_BRIDGE_QUIC_BIND` | `0.0.0.0:7842`, set `off` to disable |
| `--tls-cert`, `--tls-key` | `C2_BRIDGE_TLS_CERT`, `C2_BRIDGE_TLS_KEY` | Generated self-signed identity. Supply both for an explicit PEM chain and key. |
| `--state-dir` | `C2_BRIDGE_STATE_DIR` | `~/.c3-bridge` |
| `--token` | `C2_BRIDGE_TOKEN` | Generated and persisted; an explicit token must be printable ASCII without whitespace |

Start the bridge, copy the printed `--bridge` URL, replace `<HOST>` with a reachable address when present, and share its token with relay operators. A generated bridge identity uses `bridge-cert.pem` and `bridge-key.pem`. The generated token is stored in `bridge-token` as 64 lowercase hex digits. The owner-only directory and files persist across restarts. Supply `--dry-run` to resolve settings without opening sockets or generating files. Dry-run prints `token=explicit` or `token=auto`. Ctrl+C stops the server.

A relay uses `--bridge 'https://host:8443?pin=<64-lower-hex>&quic=off'` with the bridge token. Omit `pin` for a publicly trusted certificate. The default address discovery port is 7842. Set `quic=<port>` for another port or `quic=off` to disable it. Keep the cluster secret on the relays so they can authenticate each other end to end. Relay dry-run reports both credentials as set or unset.

Relay startup stores `mesh-key.ed25519` in its private state directory and prints an `iroh://<endpoint-id>` seed for other relays. With a shared bridge, the endpoint ID is sufficient to reach a relay behind NAT. For a direct seed without a bridge, append a reachable `@<ip:port>` hint. The default relay ID derives from the persistent endpoint key, so keep the state directory across restarts.

Mesh 9 bootstraps and exchanges routes over authenticated iroh connections across machines. Relays use their shared cluster secret to authenticate each other and keep peer control and RPC data on separate QUIC connections. iroh prefers a direct UDP path and forwards encrypted traffic through the bridge over HTTPS/WebSocket when direct connectivity fails. HTTP seeds and `/_peer/*` are available on the loopback control listener for local meshes. [ADR 0008](../docs/decisions/0008-relay-tls-http2-and-iroh-mesh.md)

SDK clients need only the HTTP(S) URL of a relay anchor. If a route is reachable only over iroh, or the edge directs the client through the anchor, discovery sets `proxy_via_anchor: true`. The client uses its configured anchor URL, including any reverse proxy path prefix and TLS trust. The anchor forwards to the route owner, which connects to the Host over IPC. [Relay discovery and forwarding](../docs/reference/contracts-and-routing.md#relay-discovery-and-forwarding)

## Local endpoint maintenance

Only pass logical addresses; Rust selects the OS endpoint. Credentials carry format metadata, never a backend choice. On Unix, save the credential after server readiness, then confirm the child has exited with an OS wait before reaping:

```bash
c3 endpoint inspect ipc://this-run-server > endpoint-inspection.json
python3 -c 'import json; r=json.load(open("endpoint-inspection.json")); assert r["status"] == "present"; print(json.dumps(r["credential"]))' > endpoint-credential.json
# After the supervisor has confirmed child exit:
c3 endpoint reap ipc://this-run-server --credential endpoint-credential.json
c3 endpoint sweep --address ipc://this-run-server --max-entries 64 --max-ms 10 --max-batches 64
```

Inspect and sweep accept `--ipc-root` (or `C2_IPC_ROOT`) to select a custom final Unix directory; `reap` always operates on the endpoint context recorded inside the credential file, and passing `--ipc-root` to `reap` only checks that it matches that recorded context before touching anything. The application provisions custom directories with current-user ownership, read/write/traverse access and no write access for other identities; `0755` is accepted, while default directories are created `0700`. The CLI captures the endpoint context with the inspection, so a credential from one root never touches an object under another. Custom-root routes discovered through a relay answer over HTTP when the relay's local namespace does not match; the namespace hint is a routing-domain signal, not an authentication credential. On Windows these commands keep their TokenUser-scoped Named Pipe behavior and `--ipc-root` is rejected as not applicable.

Unix endpoints use namespace disk format 3 and credential schema 4 without `device`. Default directory names include the disk format version; custom roots gain no suffix. Old default directories remain untouched, and old Unix credential schemas are rejected. Upgrade local SDKs, c3 and Node consumers together; follow the [0.12.0 migration notes](../docs/releases/0.12.0.md) (and [0.10.0 notes](../docs/releases/0.10.0.md) before rebuilding a custom root from pre-0.10 formats that reports `unsupported-namespace-format`).

Inspect reports observation, not process liveness. Reap refuses stale or unverified objects. `sweep` accepts repeatable `--address` flags to scope cleanup to specific addresses of the current run (omitting `--address` sweeps the entire selected namespace); report completion only when CLI JSON `sweep.roundComplete` is true — if the batch limit is reached before the round completes, `c3 endpoint sweep` exits with code 1. The millisecond budget is a scheduling target, not hard real-time filesystem behavior. Windows reports kernel-managed/not-applicable cleanup. See the [lifecycle guide](../docs/reference/local-endpoint-lifecycle.md); do not remove unknown historical directories based on age, PID or connection failure.

## Registry

Registry commands query an already-running relay over HTTP. Pass the relay URL with `--relay` or `-r`.

List registered routes:

```bash
c3 registry list-routes --relay http://127.0.0.1:8080
```

Each line names one registration: the route name, the relay that holds it and its address, the registration time, and that relay's status as this relay sees it (Alive, Suspect or Dead). A line ends with `route conflict` when more than one live registration of the name exists; new acquisitions of such a name fail with `RouteConflict` until the deployer unregisters one copy. A name whose registrations are all on Suspect or Dead relays stays held: it does not resolve and cannot be registered elsewhere until that relay comes back and withdraws it, or the deployer force-leaves the relay below. A Host that ended without unregistering keeps its name at its relay; withdraw it there with `POST /_admin/unregister` and a JSON body with `name` and `server_id`, and the relay passes the withdrawal on to its peers.

List known mesh peers:

```bash
c3 registry peers --relay http://127.0.0.1:8080
```

Both listings display the relay’s HTTP URL when available, or `iroh://<endpoint-id>` for an iroh-only address. The iroh address is for relay peering. Use the anchor’s HTTP(S) URL for SDK clients.

Force an unreachable relay to leave the mesh:

```bash
c3 registry force-leave relay-c --relay http://127.0.0.1:8080
```

The relay given with `--relay` accepts only a relay it currently sees as Suspect or Dead. It forgets that relay together with its routes and tombstones and passes the declaration on to its live peers; each peer applies it only if it also sees that relay as Suspect or Dead. Relays notice a lost peer at slightly different times, so a relay that still saw it as Alive when the declaration arrived keeps it: check `c3 registry peers` on the other relays and run the command there once they show it as Suspect or Dead. The command prints `Force-left relay-c.` and exits 0. A relay that is still Alive in that relay's view, the relay itself, an unknown relay and an invalid relay ID are refused, and the command prints the relay's reason and exits 1. Force-leave is not a ban: a force-left relay that becomes reachable again joins the relays it still remembers, and after a restart it joins through its seeds. A restarted relay without seeds starts alone, because no relay looks for it any more.

Registry requests use a five-second HTTP timeout. A failed request, a refusal or a malformed response fails the command with an error message and exit code 1. By default, the CLI bypasses system proxy settings for relay inspection unless `C2_RELAY_USE_PROXY` is enabled in the invoking `c3` process environment or `.env`.

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

The coordinated [c3 0.8.0 release entry](https://github.com/world-in-progress/c-two/releases/tag/c3-v0.8.0) uses the following binary target convention. Installation and checksum verification are in the [Windows guide](../docs/reference/windows-native-usage.md):

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `aarch64-apple-darwin`
- `x86_64-apple-darwin`
- `x86_64-pc-windows-msvc`
- `aarch64-pc-windows-msvc`

Unix assets are named `c3-${target}`; Windows uses `c3-${target}.exe`. Each has a matching `.sha256`; installers verify it before installation. The matching release's `rc-manifest.json` identifies the artifact source.

### Linux / macOS

```bash
curl -fsSL https://github.com/world-in-progress/c-two/releases/latest/download/c3-installer.sh | sh -s -- --version 0.8.0
```

The installer detects Linux/macOS and x86_64/aarch64. It defaults to `/usr/local/bin` as root or `~/.local/bin` otherwise. Select another directory:

```bash
curl -fsSL https://github.com/world-in-progress/c-two/releases/latest/download/c3-installer.sh | sh -s -- --version 0.8.0 --bin-dir "$HOME/bin"
```

### Windows x64 / ARM64

Download the PowerShell installer and select the coordinated version explicitly:

```powershell
Invoke-WebRequest -UseBasicParsing -Uri 'https://github.com/world-in-progress/c-two/releases/latest/download/c3-installer.ps1' -OutFile .\c3-installer.ps1
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\c3-installer.ps1 -Version 0.8.0
$env:PATH = "$env:LOCALAPPDATA\Programs\c3;$env:PATH"
c3.exe --version
```

PowerShell 5.1 is supported. Automatic architecture detection uses Windows `IsWow64Process2` (Windows 10 version 1709 / Windows Server 2016 version 1709 or later), including from emulated shells. The default per-user directory is `$env:LOCALAPPDATA\Programs\c3`; `-BinDir` selects another directory. The installer verifies SHA-256 and copies the binary to `c3.exe`, without requiring an administrator or persistently registering PATH. The example adds it to the current session only. Architecture selection follows the operating system, including when PowerShell runs through x64 emulation on Windows ARM64. For direct exe download, checksum verification and relay execution, see [Windows installation](../docs/reference/windows-native-usage.md#install-python-and-c3).
