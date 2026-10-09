# Source development and testing

For source development, check out canonical main from [world-in-progress/c-two](https://github.com/world-in-progress/c-two). Use the matching c3 release's `rc-manifest.json` to identify release source; a development checkout is not a release artifact.

```bash
git clone https://github.com/world-in-progress/c-two.git
cd c-two
# Full interoperability tests, golden fixtures, and TypeScript fixtures also
# need the pinned FastDB source checkout as a sibling:
git clone --branch v0.2.1 --depth 1 https://github.com/world-in-progress/fastdb.git ../fastdb
# Core and Rust SDK tests use the published Rust bindings in system mode.
# Extract the matching Core SDK from the FastDB 0.2.1 release first:
export FASTDB_PAYLOAD_LINK_MODE=system
export FASTDB_PAYLOAD_SYSTEM_LIB_DIR=/absolute/path/to/fastdb-core-sdk/lib
case "$(uname -s)" in
  Darwin) export DYLD_LIBRARY_PATH="$FASTDB_PAYLOAD_SYSTEM_LIB_DIR${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}" ;;
  Linux) export LD_LIBRARY_PATH="$FASTDB_PAYLOAD_SYSTEM_LIB_DIR${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" ;;
esac
FASTDB_PAYLOAD_LINK_MODE=source uv sync        # compile the Python extension with static FastDB
FASTDB_PAYLOAD_LINK_MODE=source uv sync --group examples  # optional: pandas/pyarrow
FASTDB_PAYLOAD_LINK_MODE=source python tools/dev/c3_tool.py --build --link  # build and link the native c3 CLI
cp .env.example .env                          # optional: local environment configuration
```

Requires [uv](https://github.com/astral-sh/uv) and a Rust toolchain. The setup above uses the Core SDK for development and testing. Deployable CLI and Python-extension builds select `FASTDB_PAYLOAD_LINK_MODE=source` and statically link the same immutable FastDB 0.2.1 source; this is separate from the Core/Rust SDK system-link contract. Run the Python suite with `C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/ -q`, and the Rust suites with `cargo test --manifest-path core/Cargo.toml --workspace` and `cargo test --manifest-path sdk/rust/Cargo.toml --all-features`. Python 3.10 remains the supported minimum. On Windows, follow the [Windows build guide](windows-native-usage.md) instead of the script above.

Full interoperability tests also require Node 22, CMake/Ninja and Emscripten
5.0.2 (with `emcmake` on `PATH`). Install both sets of TypeScript dependencies
and the minimum Python interpreter before running those suites:

```bash
npm ci --prefix ../fastdb/ts/fastdb4ts
npm ci --prefix core/foundation/c2-mem-ffi/bindings/typescript
uv python install 3.10
```

The [CI setup](../.github/workflows/ci.yml) records the pinned Emscripten setup and
scopes FastDB system linking to Rust consumers. After building the Python
extension, use `uv run --no-sync pytest ...` when running with that system-link
environment so the test command does not rebuild the extension.
