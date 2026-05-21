# C-Two CLI (C3)

`cli/` contains the Rust crate for `c3`, the native C-Two command-line interface. It starts relay servers and inspects relay registry state for C-Two deployments.

## Scope

`c3` owns product-level runtime commands:

- `c3 relay` starts the HTTP relay used for cross-machine discovery.
- `c3 registry list-routes` lists resource names registered with a relay.
- `c3 registry peers` lists peer relays known by a mesh relay.

## Build and install for development

Run CLI development commands from the repository root. Build the debug binary with:

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
| none | `C2_RELAY_ROUTE_MAX_ATTEMPTS` | `3` | Maximum relay-aware route acquisition attempts before reporting failure; valid range is `1..=32` and `0` is treated as `1`. |
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

When running normally, `c3 relay` installs a Ctrl+C handler and stops the relay cleanly on interrupt.

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

Registry requests use a five-second HTTP timeout. By default, the CLI bypasses system proxy settings for relay inspection unless the relay layer is configured to use proxies.

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

Regenerate the banner from `docs/images/logo_bw.png` with:

```bash
python tools/dev/generate_banner.py
```

## Release

`c3` is released by `.github/workflows/cli-release.yml`. The release pipeline builds native binaries for:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `aarch64-apple-darwin`
- `x86_64-apple-darwin`

Each release asset is named `c3-${target}` and is accompanied by a `c3-${target}.sha256` checksum.

Install the latest released binary with the installer asset:

```bash
curl -fsSL https://github.com/world-in-progress/c-two/releases/latest/download/c3-installer.sh | sh
```

The installer auto-detects Linux/macOS and x86_64/aarch64 targets, verifies the downloaded checksum, and installs to `/usr/local/bin` when run as root or `~/.local/bin` otherwise. Pass `-b` to choose another directory:

```bash
curl -fsSL https://github.com/world-in-progress/c-two/releases/latest/download/c3-installer.sh | sh -s -- -b /usr/local/bin
```
