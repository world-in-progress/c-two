# Cross-Language CLI Migration Design

**Date:** 2026-04-25
**Status:** Draft
**Scope:** Move the `c3` command-line interface out of the Python SDK and make it a cross-language runtime tool.

## Problem

The current `c3` CLI lives in `sdk/python/src/c_two/cli.py` and is published as
the Python package console script. That is awkward for a multi-language C-Two
runtime:

- `c3 relay` and `c3 registry` are cross-language runtime commands, not Python
  SDK features.
- future TypeScript, Go, or Rust-native SDKs should not need Python to start a
  relay or inspect the registry.
- `c3 dev generate-banner` is a repository development tool mixed into the user
  runtime CLI.
- the Python package currently owns `banner_unicode.txt`, even though the banner
  is a CLI presentation asset.

C-Two is still pre-1.0, so this migration does not need to preserve the old
Python CLI implementation for compatibility.

## Decision

Create a root-level CLI package:

```text
cli/
  Cargo.toml
  assets/
    banner_unicode.txt
  src/
    main.rs
    relay.rs
    registry.rs
    version.rs
```

The `cli` package builds a native `c3` binary. It owns runtime commands such as:

- `c3 relay`
- `c3 registry list-routes`
- `c3 registry resolve`
- `c3 registry peers`
- future runtime diagnostics such as `c3 doctor`

Language SDKs should not implement CLI business logic. If a package ecosystem
requires a launcher, it must remain an invocation adapter only:

1. locate the packaged `c3` binary for the current platform;
2. pass through `argv`;
3. exit with the binary's exit code.

## Repository Layout

```text
cli/
  Cargo.toml
  assets/
    banner_unicode.txt
  src/
    main.rs
    relay.rs
    registry.rs
    version.rs

tools/dev/
  c3_tool.py
  generate_banner.py

sdk/python/
  # no CLI implementation or launcher lives in the Python SDK
```

`cli/` sits at the repository root because it is a product-level entry point, not
a Python SDK module and not only a low-level Rust core crate.

## Banner Ownership

The generated banner belongs to the CLI:

```text
cli/assets/banner_unicode.txt
```

The generator belongs to developer tooling:

```text
tools/dev/generate_banner.py
```

The generator reads the source image from:

```text
docs/images/logo_bw.png
```

and writes the generated text asset into `cli/assets/banner_unicode.txt`.

The Rust CLI embeds the generated asset at compile time:

```rust
const BANNER: &str = include_str!("../assets/banner_unicode.txt");
```

This keeps `c3` as a single self-contained binary. SDK packages do not need to
copy or locate banner files at runtime.

After migration, remove the Python package-level banner asset and import-time
banner behavior:

```text
sdk/python/src/c_two/banner_unicode.txt
sdk/python/src/c_two/__init__.py LOGO_UNICODE loading
sdk/python/src/c_two/cli.py _print_banner()
```

## Packaging Model

The CLI is an independent runtime/admin product. The canonical release artifact
is the native `c3` binary produced by the root `cli/` crate and published by a
dedicated CLI release workflow. Language SDKs may consume those artifacts for
packaging convenience, but they do not own CLI behavior or the CLI release
lifecycle.

For Python wheels, build and publish the Python extension as today. Do not add a
`c3` console script, Python launcher, or packaged CLI binary to the Python SDK.
Python documentation may point users to the independent CLI release artifacts or
to the source-checkout development helper.

For future TypeScript, Go, Rust, and browser SDKs, do not assume the Python
wheel strategy is portable:

- Node/TypeScript can expose executables through `package.json#bin`, but
  platform-native binaries typically require per-platform packages or a small
  installer/wrapper package.
- Go libraries do not install binaries as a side effect of dependency
  resolution; the natural CLI path is a separate `go install` target.
- Rust's natural path is `cargo install` for the `c3` binary crate.
- Browser SDKs cannot run native executables and should not package `c3`.

SDK packaging should therefore treat `c3` as an external CLI product. Python may
bundle it as an early user convenience, but GitHub Release artifacts remain the
source of truth.

## Development CLI Entry Point

Source checkouts can run the Rust CLI directly:

```bash
cargo run --manifest-path cli/Cargo.toml -- relay --help
```

For repeated local usage, `tools/dev/c3_tool.py` provides a repository-level
development helper that is not part of any SDK package:

```bash
python tools/dev/c3_tool.py --build --link
c3 --version
```

`--build` compiles `cli/target/debug/c3`. `--link` links an existing binary into
a development bin directory. The helper prefers a persistent PATH directory such
as `~/.cargo/bin` when available; otherwise it falls back to an ignored
repository `.bin/` directory and prints the `export PATH=...` command. This
keeps the development entry point cross-language instead of tying it to Python's
`.venv`.

Standalone GitHub release artifacts include the native `c3` binaries so users
can install the CLI without installing any SDK.

## Release Workflow Naming

The existing workflow is `.github/workflows/release.yml` and is named `Release`,
but it currently publishes only the Python package. Once C-Two has a
cross-language CLI and multiple SDKs, a generic `release` workflow name becomes
misleading.

Rename the current Python packaging workflow to a Python-specific CD workflow:

```text
.github/workflows/python-package-release.yml
name: Python Package Release
```

Future workflows can then be explicit:

```text
.github/workflows/cli-release.yml
name: CLI Release

.github/workflows/typescript-package-release.yml
name: TypeScript Package Release

.github/workflows/release-orchestrator.yml
name: Release Orchestrator
```

This naming keeps each artifact's lifecycle clear. The Python package workflow
may still consume CLI binaries, but it should not be the owner of the whole
cross-language release process.

## CLI Release Workflow

`.github/workflows/cli-release.yml` is the canonical CLI release pipeline. It
builds `c3` for each supported target, uploads workflow artifacts for manual
inspection, and publishes GitHub Release assets on version tags.

Initial targets:

```text
x86_64-unknown-linux-gnu
aarch64-unknown-linux-gnu
aarch64-apple-darwin
x86_64-apple-darwin
```

Linux builds should use a manylinux-compatible environment so standalone
binaries and Python wheel-embedded binaries do not depend on the GitHub runner's
newer glibc.

Each target publishes:

```text
c3-${target}
c3-${target}.sha256
```

Future package-manager entry points such as Homebrew, Winget, Scoop, npm, or
Cargo should reference or rebuild from this canonical CLI product line rather
than treating any SDK as the CLI owner.

## Python Package CD Changes

The Python package CD workflow should be
`.github/workflows/python-package-release.yml`. Today the equivalent job is in
`.github/workflows/release.yml`: `build-wheels` runs a platform matrix and calls
`PyO3/maturin-action` inside `sdk/python` to produce wheels. The CLI migration
must modify that Python package job, not add a separate disconnected wheel
pipeline.

Current wheel matrix:

```yaml
matrix:
  include:
    - os: ubuntu-latest
      target: x86_64-unknown-linux-gnu
    - os: ubuntu-24.04-arm
      target: aarch64-unknown-linux-gnu
    - os: macos-latest
      target: aarch64-apple-darwin
    - os: macos-latest
      target: x86_64-apple-darwin
```

Required wheel build order for each matrix row:

1. checkout repository;
2. install Rust toolchain for `matrix.target`;
3. run the existing `PyO3/maturin-action@v1` wheel build;
4. upload wheels as today.

The source distribution should not include a prebuilt platform binary. Source
checkout developers should use `cargo run` or `tools/dev/c3_tool.py`; source
package installation should not make the Python SDK responsible for CLI
ownership.

Standalone CLI artifact upload belongs to `.github/workflows/cli-release.yml`,
not to the Python package release workflow. This keeps PyPI publishing isolated
from non-Python artifacts and prevents the Python SDK from becoming the
cross-language CLI owner.

The CI workflow should add at least one Linux check that builds and smoke-tests
the native CLI:

```bash
cargo build --manifest-path cli/Cargo.toml
cli/target/debug/c3 --version
cli/target/debug/c3 relay --help
cli/target/debug/c3 registry --help
```

This catches CLI regressions without requiring Python packaging.

## Industrial Analogues

This follows the pattern used by tooling with a native engine and multiple
language/package-manager entry points:

- esbuild: JavaScript package installs a platform-specific native executable.
- Sentry CLI: npm package wraps/downloads a native CLI binary.
- Biome: provides standalone native CLI binaries and package-manager entry
  points.
- Prisma: language-facing packages depend on native engine binaries.
- Ruff: Rust-native CLI distributed through Python and standalone installers.

The shared pattern is: the CLI/engine is its own artifact; language packages are
distribution and invocation adapters.

## Migration Steps

1. Create root-level `cli/` Rust package with a `c3` binary.
2. Move `relay` command behavior from `sdk/python/src/c_two/cli.py` into the
   native CLI.
3. Move `registry` commands into the native CLI using HTTP requests directly.
4. Move banner generation code into `tools/dev/generate_banner.py`.
5. Generate and commit `cli/assets/banner_unicode.txt`.
6. Remove runtime banner loading from `c_two.__init__`.
7. Remove the Python `c3` console script and any Python CLI launcher.
8. Rename `.github/workflows/release.yml` to
   `.github/workflows/python-package-release.yml` with workflow name
   `Python Package Release`.
9. Keep the Python package release workflow focused on Python wheels and sdist.
10. Update README and examples to describe `c3` as the cross-language CLI.
11. Remove the old Python `cli.py` once the native CLI covers the runtime
    commands.

## Non-Goals

- Do not preserve `c3 dev generate-banner` as a user-facing CLI command.
- Do not let language SDK shims duplicate command behavior.
- Do not make the CLI depend on Python.
- Do not require runtime access to banner asset files.
