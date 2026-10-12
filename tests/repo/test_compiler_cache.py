"""Exercise the Unix cache hook's configuration and build-exit contract."""

import json
import os
import subprocess
from pathlib import Path

import pytest


SCRIPT = Path(__file__).resolve().parents[2] / ".github/scripts/compiler_cache.sh"


@pytest.mark.parametrize("version,accepted", [("3.16.9", False), ("3.17.0", True), ("4.4.2", True)])
def test_cpp_launcher_requires_a_cmake_version_that_reads_the_environment(version, accepted):
    result = subprocess.run(
        ["bash", "-c", """
set -euo pipefail
source "$1"
cmake() { printf 'cmake version %s\n' "$TEST_CMAKE_VERSION"; }
c2_enable_cpp_cache
printf 'launchers=%s,%s\n' "$CMAKE_C_COMPILER_LAUNCHER" "$CMAKE_CXX_COMPILER_LAUNCHER"
""", "test", SCRIPT.as_posix()],
        env={**os.environ, "TEST_CMAKE_VERSION": version}, capture_output=True, text=True,
    )
    assert (result.returncode == 0) is accepted
    if accepted:
        assert "launchers=sccache,sccache" in result.stdout
    else:
        assert "requires CMake >= 3.17" in result.stderr
        assert "launchers=" not in result.stdout


@pytest.mark.parametrize("build_status", [0, 19])
@pytest.mark.parametrize("stats_fail", [False, True])
def test_cache_diagnostics_never_replace_the_build_exit_status(tmp_path, build_status, stats_fail):
    output = tmp_path / "compiler statistics"
    result = subprocess.run(
        ["bash", "-c", """
set -euo pipefail
source "$1"
cmake() { printf 'cmake version 4.4.2\n'; }
sccache() {
  if [[ "$1" == --show-stats ]]; then
    if [[ "$TEST_STATS_FAIL" == true ]]; then return 7; fi
    printf '{"cache_hits":{"counts":{"C/C++":3}}}\n'
  else
    printf 'compiler cache diagnostic\n'
  fi
}
c2_report_compiler_cache_on_exit "$2"
exit "$TEST_BUILD_STATUS"
""", "test", SCRIPT.as_posix(), output.as_posix()],
        env={**os.environ, "TEST_BUILD_STATUS": str(build_status), "TEST_STATS_FAIL": str(stats_fail).lower()},
        capture_output=True, text=True,
    )
    assert result.returncode == build_status
    assert not (output / "stats.json.tmp").exists()
    if stats_fail:
        assert not (output / "stats.json").exists()
        assert "Could not capture" in result.stdout
    else:
        assert json.loads((output / "stats.json").read_text())["cache_hits"]["counts"]["C/C++"] == 3
