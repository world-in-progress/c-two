# Unix compiler-cache configuration shared by native and manylinux builds.
# Storage credentials are provided by the setup action / container caller.

c2_enable_cpp_cache() {
  local cache_cmake_version cache_cmake_major cache_cmake_minor
  cache_cmake_version="$(cmake --version)" || return
  if [[ ! "$cache_cmake_version" =~ cmake\ version\ ([0-9]+)\.([0-9]+) ]]; then
    echo 'Cannot determine CMake version for compiler launchers' >&2
    return 1
  fi
  cache_cmake_major="${BASH_REMATCH[1]}"
  cache_cmake_minor="${BASH_REMATCH[2]}"
  if (( cache_cmake_major < 3 || (cache_cmake_major == 3 && cache_cmake_minor < 17) )); then
    echo 'C/C++ compiler-cache environment requires CMake >= 3.17' >&2
    return 1
  fi
  export CMAKE_C_COMPILER_LAUNCHER=sccache
  export CMAKE_CXX_COMPILER_LAUNCHER=sccache
  export SCCACHE_IDLE_TIMEOUT=0
  printf '%s\n' "$cache_cmake_version"
  printf 'C/C++ launcher: sccache; generator: %s\n' "${CMAKE_GENERATOR:-CMake platform default}"
}

c2_write_compiler_cache_stats() {
  local cache_stats_dir="$1"
  if ! mkdir -p "$cache_stats_dir"; then
    echo '::warning::Cannot create compiler-cache diagnostic directory'
    return 0
  fi
  if ! command -v sccache >/dev/null 2>&1; then
    echo '::warning::Compiler-cache statistics unavailable: sccache was not installed'
    return 0
  fi
  sccache --version > "$cache_stats_dir/version.txt" || true
  cmake --version > "$cache_stats_dir/cmake-version.txt" || true
  if sccache --show-stats --stats-format=json > "$cache_stats_dir/stats.json.tmp"; then
    mv "$cache_stats_dir/stats.json.tmp" "$cache_stats_dir/stats.json"
  else
    rm -f "$cache_stats_dir/stats.json.tmp"
    echo '::warning::Could not capture compiler-cache JSON statistics'
  fi
  sccache --show-adv-stats || true
}

c2_report_compiler_cache_on_exit() {
  C2_COMPILER_CACHE_STATS_DIR="$1"
  # Diagnostics never replace the build's exit code, including on failure.
  trap 'c2_cache_build_status=$?; trap - EXIT; c2_write_compiler_cache_stats "$C2_COMPILER_CACHE_STATS_DIR" || true; exit "$c2_cache_build_status"' EXIT
}
