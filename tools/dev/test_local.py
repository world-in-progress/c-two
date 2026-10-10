#!/usr/bin/env python3
"""Explicit local development sidecar; does not produce CI/release evidence."""

from __future__ import annotations

import argparse
from contextlib import ExitStack
from dataclasses import dataclass
from datetime import datetime, timezone
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import time
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))

from tools.dev import test_python as strict

ALL = ['core', 'cli', 'rust-sdk', 'native', 'node', 'repo', 'python']
TS = ROOT / 'core/foundation/c2-mem-ffi/bindings/typescript'
# These ignored Rust functions are child-process entrypoints executed by their
# parent tests; they are not missing coverage or optional environment gates.
RUST_IGNORED = {
    ('http_only_process_child', 'isolated process fixture, invoked by its parent test'),
    ('http_only_child', 'isolated process fixture, invoked by parent'),
}
NODE_PLATFORM_SKIPS = {
    'win32': {
        ('Runtime roots freeze on first successful or failed IO and shared contexts support live connections',
         'Unix custom-root transport gate; Windows root rejection is covered separately.'),
        ('native context snapshots resolver roots and env files without creating endpoint directories',
         'Unix roots are explicitly unsupported on Windows'),
        ('Runtime shares its first-attempt native snapshot with queries and reconnects in an isolated environment',
         'Unix env-root freeze; Windows root rejection remains separately covered'),
    },
    'posix': {('Windows context keeps current-SID pipe defaults and rejects Unix roots natively', 'Requires the real Windows platform')},
}
REPO_PLATFORM_SKIPS: set[tuple[str, str, str]] = {
    ('tests.repo.test_install_c3_powershell', 'test_installer_downloads_verifies_and_installs_from_release_base',
     'fixture is not a runnable Windows PE'),
    ('tests.repo.test_python_test_runner', 'test_main_normalizes_fixture_path_and_preserves_venv_python_link',
     'POSIX virtualenv interpreter symlink semantics'),
} if sys.platform == 'win32' else {
    ('tests.repo.test_local_test_platform', 'test_owned_process_diagnostics_name_scoped_processes', 'Windows Job process image diagnostics'),
    ('tests.repo.test_python_test_runner', 'test_capture_runs_windows_npm_without_shell', 'Windows npm.cmd requires the Node JavaScript entry'),
    *(('tests.repo.test_local_test_platform', f'test_compiler_helper_cleanup_requires_exact_scope[{params}]',
       'Windows MSVC scoped helper lifecycle') for params in ('False-False-False', 'True-False-False', 'False-True-False', 'False-False-True')),
}


class LocalError(Exception):
    pass


@dataclass
class Step:
    name: str
    command: list[str]
    link_mode: str = 'system'
    cwd: Path = ROOT
    strict_group: str | None = None


def selected_suites(suite: str) -> list[str]:
    return list(ALL) if suite == 'all' else [suite]


def required_tools(suite: str, *, prepare: bool) -> list[str]:
    selected = selected_suites(suite)
    required = {'git'}
    if any(s in selected for s in ('core', 'cli', 'rust-sdk', 'native', 'python', 'typescript', 'node')):
        required.add('cargo')
    if 'repo' in selected and not prepare:
        required.add('cargo')
    if any(s in selected for s in ('python', 'typescript', 'node')):
        required.update(('node', 'npm'))
    if prepare and any(s in selected for s in ('python', 'typescript')):
        required.update(('cmake', 'ninja'))
    if any(s in selected for s in ('python', 'repo')) or (prepare and 'typescript' in selected):
        required.add('uv')
    return sorted(required)


def child_environment(base: dict[str, str], tool_paths: list[Path], lib: Path | None, platform: str = sys.platform) -> dict[str, str]:
    env = {k: v for k, v in base.items() if not k.startswith(('C2_', 'CTWO_', 'PYTEST_XDIST_')) and k.upper() != 'PATH' and k != 'CARGO_TARGET_DIR'}
    old_path = next((v for k, v in base.items() if k.upper() == 'PATH'), '')
    paths = [str(p.resolve()) for p in tool_paths]
    if platform == 'win32' and lib:
        paths.insert(0, str(lib))
    env.update(PATH=os.pathsep.join([*paths, old_path]), PYTHONUTF8='1', C2_ENV_FILE='', C2_RELAY_ANCHOR_ADDRESS='',
               NO_PROXY='127.0.0.1,localhost', no_proxy='127.0.0.1,localhost')
    if platform == 'win32':
        # MSBuild's reusable workers otherwise outlive the completed build.
        env['MSBUILDDISABLENODEREUSE'] = '1'
    if lib:
        env.update(FASTDB_PAYLOAD_LINK_MODE='system', FASTDB_PAYLOAD_SYSTEM_LIB_DIR=str(lib))
        if platform != 'win32':
            key = 'DYLD_LIBRARY_PATH' if platform == 'darwin' else 'LD_LIBRARY_PATH'
            env[key] = os.pathsep.join([str(lib), env.get(key, '')])
    return env


def find_tool(name: str, environment: dict[str, str]) -> str:
    result = shutil.which(name, path=environment.get('PATH'))
    if not result:
        raise LocalError(f'missing tool: {name}; activate the toolchain or pass --tool-path DIRECTORY')
    return result


def npm_command(node: Path, *args: str) -> list[str]:
    candidates = [node.parent / 'node_modules/npm/bin/npm-cli.js',
                  node.parent.parent / 'lib/node_modules/npm/bin/npm-cli.js']
    for script in candidates:
        if script.is_file():
            return [str(node), str(script), *args]
    raise LocalError(f'npm CLI missing beside selected Node: {node}')


def packages(options) -> list[Path | None]:
    return [options.fastdb_package, options.c2_mem_package, options.typescript_package]


def package_args(options) -> list[str]:
    args = []
    for flag, path in zip(('fastdb-package', 'c2-mem-package', 'typescript-package'), packages(options)):
        if path:
            args += [f'--{flag}', str(path.resolve())]
    return args


def suite_steps(options, root: Path) -> list[Step]:
    output = options.output.resolve()
    python = os.path.abspath(os.path.expanduser(options.python))
    c3 = options.c3 or root / ('cli/target/debug/c3.exe' if os.name == 'nt' else 'cli/target/debug/c3')
    env = child_environment(dict(os.environ), options.tool_path, options.coresdk_lib)
    node = Path(shutil.which('node', path=env['PATH']) or 'node')
    commands = {
        'core': ['cargo', 'test', '--locked', '--manifest-path', 'core/Cargo.toml', '--workspace'],
        'cli': ['cargo', 'test', '--locked', '--manifest-path', 'cli/Cargo.toml'],
        'rust-sdk': ['cargo', 'test', '--locked', '--manifest-path', 'sdk/rust/Cargo.toml', '--all-features'],
        'native': ['cargo', 'test', '--locked', '--manifest-path', 'sdk/python/native/Cargo.toml'],
        'repo': [os.path.abspath(options.repo_python or python), '-m', 'pytest', '--confcutdir=tests/repo', 'tests/repo', '-q', '-rs', '--timeout=60',
                 f'--basetemp={output / "repo/tmp"}', f'--junitxml={output / "repo/junit.xml"}'],
        'python': [python, 'tools/dev/test_python.py', '--python', python, '--c3', str(c3.resolve()),
                   '--output', str(output / 'python/evidence'), '--workers', str(options.workers),
                   '--stage-timeout', str(options.stage_timeout), *package_args(options)],
        'typescript': strict.build_pytest_command(python, 'typescript', output / 'typescript', 1),
    }
    result = []
    for name in selected_suites(options.suite):
        command = npm_command(node, 'test', '--prefix', str(TS)) if name == 'node' else commands[name]
        result.append(Step(name, command, 'source' if name in ('cli', 'native') else 'system', root,
                           'typescript' if name == 'typescript' else None))
    return result


def probe(command: list[str], env: dict[str, str], cwd: Path = ROOT) -> str:
    result = subprocess.run(command, cwd=cwd, env=env, capture_output=True, encoding='utf-8', errors='replace', timeout=30)
    if result.returncode:
        raise LocalError(f'probe failed ({result.returncode}): {command}\n{result.stdout}\n{result.stderr}')
    return result.stdout.strip()


def test_counts(name: str, log: str, junit: Path | None) -> dict:
    if junit is not None:
        xml = ET.parse(junit).getroot()
        counts = {k: sum(int(s.get(k, 0)) for s in xml.iter('testsuite'))
                  for k in ('tests', 'failures', 'errors', 'skipped')}
        for case in xml.iter('testcase'):
            skipped = case.find('skipped')
            if skipped is not None:
                identity = (case.get('classname', ''), case.get('name', ''), skipped.get('message', ''))
                if identity not in REPO_PLATFORM_SKIPS:
                    raise LocalError(f'unrecognized skip: {identity}')
        if counts['failures'] or counts['errors']:
            raise LocalError(f'JUnit reports failed tests: {counts}')
        if counts['tests'] <= counts['skipped']:
            raise LocalError(f'JUnit contains no executed tests: {counts}')
        return counts
    if name in ('core', 'cli', 'rust-sdk', 'native'):
        summaries = re.findall(r'test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out;', log)
        if not summaries:
            raise LocalError('Cargo test summary missing')
        counts = dict(zip(('passed', 'failed', 'ignored', 'measured', 'filtered'),
                          (sum(int(row[i]) for row in summaries) for i in range(5))))
        ignored = re.findall(r'^test (.*?) \.\.\. ignored(?:, (.*))?$', log, re.MULTILINE)
        if counts['ignored'] != len(ignored) or any(tuple(row) not in RUST_IGNORED for row in ignored):
            raise LocalError(f'unrecognized ignored Rust test: {ignored}')
        if counts['failed'] or counts['filtered'] or not counts['passed']:
            raise LocalError(f'incomplete Rust tests: {counts}')
        return counts
    if name == 'node':
        counts = {key: int(re.findall(rf'^# {key} (\d+)$', log, re.MULTILINE)[-1])
                  for key in ('tests', 'pass', 'fail', 'cancelled', 'skipped', 'todo')
                  if re.findall(rf'^# {key} (\d+)$', log, re.MULTILINE)}
        skips = re.findall(r'^ok \d+ - (.*?) # SKIP(?: (.*))?$', log, re.MULTILINE)
        allowed = NODE_PLATFORM_SKIPS['win32' if sys.platform == 'win32' else 'posix']
        if counts.get('skipped', 0) != len(skips) or any(tuple(row) not in allowed for row in skips) or counts.get('todo', 0):
            raise LocalError(f'unrecognized Node skip/todo: {skips}, {counts}')
        if counts.get('fail') or counts.get('cancelled') or not counts.get('pass'):
            raise LocalError(f'incomplete Node tests: {counts}')
        return counts
    return {}


def check(options, env: dict[str, str]) -> dict:
    from tools.dev.test_platform import core_library_name, validate_node_runtime
    report = {'platform': sys.platform, 'tools': {}, 'errors': []}
    tools = required_tools(options.suite, prepare=options.action == 'prepare')
    if all(packages(options)):
        tools = [t for t in tools if t not in ('cmake', 'ninja')]
    for name in tools:
        try:
            executable = find_tool(name, env)
            command = npm_command(Path(find_tool('node', env)), '--version') if name == 'npm' else [executable, '--version']
            report['tools'][name] = {'path': executable, 'version': probe(command, env)}
        except (LocalError, OSError, subprocess.TimeoutExpired) as error:
            report['errors'].append(str(error))
    if 'node' in report['tools']:
        try:
            node = report['tools']['node']['path']
            napi = probe([node, '-p', 'process.versions.napi'], env)
            validate_node_runtime(report['tools']['node']['version'], napi)
            report['tools']['node']['napi'] = napi
        except (ValueError, OSError, strict.RunnerError, LocalError) as error:
            report['errors'].append(str(error))
    needs_sdk = any(s in selected_suites(options.suite) for s in ('core', 'rust-sdk', 'python', 'typescript'))
    if needs_sdk and options.action != 'prepare':
        library = options.coresdk_lib / core_library_name(sys.platform) if options.coresdk_lib else None
        if library is None or not library.is_file():
            report['errors'].append('verified CoreSDK required: pass --coresdk-lib ABSOLUTE_LIB_DIRECTORY or use prepare')
        elif os.name == 'nt' and not (library.parent / 'fastdb.lib').is_file():
            report['errors'].append('Windows CoreSDK import library fastdb.lib is missing')
    if os.name == 'nt' and 'cargo' in report['tools']:
        try:
            find_tool('cl', env)
        except LocalError:
            report['errors'].append('MSVC compiler unavailable: run from Developer PowerShell / vcvars64 environment')
    selected = selected_suites(options.suite)
    if options.action != 'prepare' and 'repo' in selected:
        try:
            probe([os.path.abspath(options.repo_python or options.python), '-c',
                   'import pytest,pytest_timeout,xdist,packaging,yaml,maturin,puccinialin'], env)
        except (LocalError, OSError, subprocess.TimeoutExpired) as error:
            report['errors'].append(f'repository test environment not prepared: {error}')
        if not (shutil.which('pwsh', path=env['PATH']) or shutil.which('powershell', path=env['PATH'])):
            report['errors'].append('PowerShell required for repository installer tests; install pwsh or select its --tool-path')
    if options.action == 'prepare' and not all(packages(options)) and any(s in selected for s in ('python', 'typescript')):
        if not options.emsdk or not (options.emsdk / 'upstream/emscripten/emcmake.py').is_file():
            report['errors'].append('activated Emscripten SDK required: pass --emsdk DIRECTORY')
        else:
            version = probe([options.python, str(options.emsdk / 'upstream/emscripten/emcc.py'), '--version'], env)
            if '5.0.2' not in version.splitlines()[0]:
                report['errors'].append('Emscripten 5.0.2 required for reproducible local payload builds')
    if options.action != 'prepare' and any(s in selected for s in ('python', 'typescript')):
        try:
            python = os.path.abspath(options.python)
            info = json.loads(probe([python, '-c', strict.PROBE], env).splitlines()[-1])
            if Path(info['python_sdk']).resolve().parent != (ROOT / 'sdk/python/src/c_two').resolve():
                raise LocalError('selected Python SDK does not belong to this checkout; run prepare first')
            c3 = options.c3 or ROOT / ('cli/target/debug/c3.exe' if os.name == 'nt' else 'cli/target/debug/c3')
            probe([str(c3), '--version'], env)
            report['input_hashes'] = {str(c3.resolve()): strict.sha256(c3), info['native']: info['native_sha256']}
            for path in packages(options):
                if path and not path.is_file():
                    raise LocalError(f'package missing: {path}')
                if path:
                    report['input_hashes'][str(path.resolve())] = strict.sha256(path)
        except (LocalError, OSError, subprocess.TimeoutExpired) as error:
            report['errors'].append(str(error))
    report['status'] = 'failed' if report['errors'] else 'passed'
    return report


def execute_steps(steps: list[Step], output: Path, environment: dict[str, str], root: Path, timeout: float, *, stop_on_failure=False, resources_leased=False) -> dict:
    from tools.dev.test_platform import ProcessTree, resource_lease
    try:
        output.mkdir(parents=True, exist_ok=False)
    except FileExistsError as error:
        raise LocalError(f'output already exists: {output}') from error
    report = {'schema': 'c-two.local-test-run.v1', 'evidence_stage': 'development', 'platform': sys.platform,
              'started_at': datetime.now(timezone.utc).isoformat(), 'status': 'running', 'steps': []}
    blocked = False
    for step in steps:
        if blocked:
            report['steps'].append({'id': step.name, 'status': 'blocked', 'reason': 'earlier stage did not complete'})
            continue
        directory = output / step.name
        env = {**environment, 'FASTDB_PAYLOAD_LINK_MODE': step.link_mode}
        print(f'{step.name}: {subprocess.list2cmdline(step.command)}', flush=True)
        if step.name == 'typescript':
            env.update(CTWO_PYTHON_STAGE_DIR=str(directory), C2_TYPESCRIPT_RECEIPT=str(directory / 'typescript.v1.json'),
                       C2_TYPESCRIPT_EVIDENCE_STAGE='development')
        # The strict Python subprocess acquires all its own resource leases.
        with ExitStack() as stack:
            if step.name != 'python' and not resources_leased:
                stack.enter_context(resource_lease(root))
            planned = None
            if step.strict_group:
                collect = directory.parent / (step.name + '-collection')
                record = strict.run_step(strict.build_pytest_command(step.command[0], step.strict_group, collect, 1, collect=True),
                                         collect, strict.stage_environment(env, collect), timeout)
                if record['status'] != 'passed' or not record['cleanup_confirmed']:
                    report['steps'].append({'id': step.name + '-collection', **record})
                    blocked = True
                    continue
                planned = strict.collected(collect)
            directory.mkdir()
            started = time.monotonic()
            record = {'id': step.name, 'command': step.command, 'cwd': str(step.cwd), 'link_mode': step.link_mode,
                      'status': 'failed', 'exit_code': None, 'cleanup_confirmed': False}
            tree = None
            with (directory / 'output.log').open('wb') as log:
                try:
                    tree = ProcessTree(step.command, cwd=step.cwd, env=env, stdout=log)
                    record['exit_code'] = tree.process.wait(timeout=timeout)
                    deadline = time.monotonic() + 2
                    while tree.alive() and time.monotonic() < deadline:
                        time.sleep(.05)
                    helpers = tree.reap_compiler_helpers(env)
                    if helpers:
                        record['compiler_helper_cleanup'] = helpers
                    record['cleanup_confirmed'] = not tree.alive()
                    record['status'] = 'passed' if record['exit_code'] == 0 and record['cleanup_confirmed'] else 'failed'
                    if tree.alive():
                        record['residual_processes'] = tree.diagnostics()
                        record['error'] = 'residual child process tree; forced cleanup, stage failed'
                        tree.stop()
                    if planned is not None and record['status'] == 'passed':
                        strict.validate_execution(directory, planned, 1)
                        from tools.local_rc.typescript_receipt import load_and_validate_receipt
                        load_and_validate_receipt(directory / 'typescript.v1.json', expected_stage='development')
                except (subprocess.TimeoutExpired, KeyboardInterrupt) as error:
                    record['status'] = 'cancelled' if isinstance(error, KeyboardInterrupt) else 'timed_out'
                    record['error'] = str(error)
                    if tree:
                        record['residual_processes'] = tree.diagnostics()
                        tree.stop()
                        record['exit_code'] = tree.process.returncode
                except (OSError, ValueError, strict.RunnerError) as error:
                    record['error'] = str(error)
                    record['status'] = 'failed'
                finally:
                    if tree:
                        try:
                            tree.close()
                        except OSError as error:
                            record['status'] = 'failed'
                            record['cleanup_confirmed'] = False
                            record['cleanup_error'] = str(error)
                    record['duration_seconds'] = time.monotonic() - started
            if record['status'] == 'passed' and step.name in ('core', 'cli', 'rust-sdk', 'native', 'node', 'repo'):
                try:
                    record['test_counts'] = test_counts(step.name, (directory / 'output.log').read_text(encoding='utf-8', errors='replace'),
                                                       directory / 'junit.xml' if step.name == 'repo' else None)
                except (LocalError, OSError, ET.ParseError, ValueError) as error:
                    record.update(status='failed', error=str(error))
            strict.write_json(directory / 'step.json', record)
            report['steps'].append(record)
            blocked = not record['cleanup_confirmed'] or (stop_on_failure and record['status'] != 'passed')
        strict.write_json(output / 'run.json', report)
    report['status'] = 'passed' if steps and all(s['status'] == 'passed' for s in report['steps']) else 'failed'
    report['finished_at'] = datetime.now(timezone.utc).isoformat()
    strict.write_json(output / 'run.json', report)
    return report


def preparation_steps(options, staging: Path) -> list[Step]:
    env = child_environment(dict(os.environ), options.tool_path, options.coresdk_lib)
    selected = selected_suites(options.suite)
    result = []
    if options.coresdk_lib is None and any(s in selected for s in ('core', 'rust-sdk', 'python', 'typescript')):
        options.coresdk_lib = staging / 'coresdk/lib'
        result.append(Step('coresdk', [options.python, '.github/scripts/prepare_fastdb_sdk.py', '--output', str(staging / 'coresdk')]))
    for suite, manifest in (('core', 'core/Cargo.toml'), ('rust-sdk', 'sdk/rust/Cargo.toml'), ('native', 'sdk/python/native/Cargo.toml')):
        if suite in selected:
            result.append(Step(f'{suite}-dependencies', ['cargo', 'fetch', '--locked', '--manifest-path', manifest],
                               'source' if suite == 'native' else 'system'))
    if any(s in selected for s in ('cli', 'python', 'typescript')):
        result.append(Step('c3-build', ['cargo', 'build', '--locked', '--manifest-path', 'cli/Cargo.toml', '--bins'], 'source'))
    if any(s in selected for s in ('python', 'typescript')):
        result.append(Step('python-sync', ['uv', 'sync', '--locked', '--python', options.python], 'source'))
    if 'repo' in selected:
        repo_venv = staging / 'repo-venv'
        repo_python = repo_venv / ('Scripts/python.exe' if os.name == 'nt' else 'bin/python')
        result.extend((Step('repo-venv', ['uv', 'venv', '--python', options.python, str(repo_venv)]),
                       Step('repo-dependencies', ['uv', 'pip', 'install', '--python', str(repo_python),
                                                'pytest', 'pytest-timeout', 'pytest-xdist==3.8.0', 'packaging', 'pyyaml', 'maturin', 'puccinialin'])))
    if 'repo' in selected and options.release_fixtures is None:
        result.append(Step('release-fixtures', [options.python, str(Path(__file__).resolve()), '_release-fixtures', str(staging / 'release-fixtures')]))
    if 'python' in selected:
        result.append(Step('python310', ['uv', 'python', 'install', '3.10'], 'source'))
    if not any(s in selected for s in ('python', 'typescript', 'node')):
        return result
    node = Path(find_tool('node', env))
    mem_steps = [Step('c2-mem-npm', npm_command(node, 'ci', '--prefix', str(TS))),
                 Step('c2-mem-build', [str(node), str(TS / 'scripts/build-runtime.mjs')])]
    if all(packages(options)) or selected == ['node']:
        return result + mem_steps
    source = options.fastdb_source or ROOT.parent / 'fastdb'
    if probe(['git', '-C', str(source), 'rev-parse', 'HEAD'], env) != strict.FASTDB_SHA:
        raise LocalError('FastDB source revision does not match pinned commit')
    if probe(['git', '-C', str(source), 'status', '--porcelain'], env):
        raise LocalError('FastDB source must be clean before preparing paired JS/WASM')
    copy = staging / 'fastdb'
    package = copy / 'ts/fastdb4ts'
    tarballs = staging / 'packages'
    tarballs.mkdir(parents=True)
    emcmake = options.emsdk / 'upstream/emscripten/emcmake.py'
    result += [
        Step('fastdb-copy', ['git', 'clone', '--no-hardlinks', '--no-checkout', str(source), str(copy)]),
        Step('fastdb-checkout', ['git', '-C', str(copy), 'checkout', '--detach', strict.FASTDB_SHA]),
        Step('fastdb-npm', npm_command(node, 'ci', '--prefix', str(package))),
        Step('fastdb-wasm-configure', [options.python, str(emcmake), 'cmake', '-G', 'Ninja', '-S', str(copy / 'ts/embind'),
                                    '-B', str(copy / 'build-wasm'), '-DCMAKE_BUILD_TYPE=Release']),
        Step('fastdb-wasm-build', ['cmake', '--build', str(copy / 'build-wasm'), '--target', 'fastdb4ts']),
        Step('fastdb-tsc', [str(node), str(package / 'node_modules/typescript/bin/tsc'), '-p', str(package / 'tsconfig.json')]),
        Step('fastdb-assets', [options.python, str(Path(__file__).resolve()), '_assets', str(copy)]),
        Step('fastdb-pack', npm_command(node, 'pack', '--ignore-scripts', '--pack-destination', str(tarballs), str(package))),
        *mem_steps,
        Step('c2-mem-pack', npm_command(node, 'pack', '--ignore-scripts', '--pack-destination', str(tarballs), str(TS))),
        Step('typescript-pack', npm_command(node, 'pack', '--ignore-scripts', '--pack-destination', str(tarballs), str(TS / 'node_modules/typescript'))),
    ]
    return result


def copy_assets(source: Path) -> None:
    package = source / 'ts/fastdb4ts'
    destination = package / 'dist/wasm'
    destination.mkdir(parents=True, exist_ok=True)
    for path in (package / 'src/wasm').iterdir():
        if path.suffix in ('.js', '.wasm') or path.name.endswith('.d.ts'):
            shutil.copy2(path, destination / path.name)
    for name in ('LICENSE', 'THIRD_PARTY_NOTICES.txt'):
        shutil.copy2(source / name, package / 'dist' / name)


def prepare_release_fixtures(destination: Path) -> None:
    """Retain official archives for repository tests using the existing pins."""
    spec = importlib.util.spec_from_file_location('local_fastdb_sdk_prepare', ROOT / '.github/scripts/prepare_fastdb_sdk.py')
    helper = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helper)
    destination.mkdir(parents=True, exist_ok=False)
    data = helper.fetch_release_manifest(None)
    manifest = helper.verify_release_manifest(data)
    (destination / helper.RELEASE_MANIFEST_NAME).write_bytes(data)
    for target in ('x86_64-unknown-linux-gnu', 'aarch64-apple-darwin', 'x86_64-pc-windows-msvc'):
        entry = helper.core_bundle_entry(manifest, target)
        downloaded = helper.fetch_core_bundle(entry, None, destination / 'verification')
        downloaded.replace(destination / entry['name'])


def parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('action', choices=('check', 'prepare', 'run'))
    p.add_argument('--suite', choices=(*ALL, 'typescript', 'all'), default='all')
    p.add_argument('--output', type=Path, required=True, help='new development evidence directory')
    p.add_argument('--python', default=sys.executable)
    p.add_argument('--repo-python', help='prepared repository-test interpreter; defaults to --python')
    p.add_argument('--c3', type=Path)
    p.add_argument('--coresdk-lib', type=Path, default=Path(os.environ['FASTDB_PAYLOAD_SYSTEM_LIB_DIR']) if os.environ.get('FASTDB_PAYLOAD_SYSTEM_LIB_DIR') else None)
    p.add_argument('--tool-path', type=Path, action='append', default=[], help='prepend tool directory to child PATH; repeatable')
    p.add_argument('--emsdk', type=Path, default=Path(os.environ['EMSDK']) if os.environ.get('EMSDK') else None)
    p.add_argument('--fastdb-source', type=Path)
    p.add_argument('--release-fixtures', type=Path, default=Path(os.environ['C2_DEV_FASTDB_RELEASE_FIXTURES']) if os.environ.get('C2_DEV_FASTDB_RELEASE_FIXTURES') else None)
    p.add_argument('--workers', type=int, choices=(1, 2, 4), default=1)
    p.add_argument('--stage-timeout', type=float, default=3600)
    for name in ('fastdb', 'c2-mem', 'typescript'):
        p.add_argument(f'--{name}-package', type=Path)
    return p


def validate_options(options) -> None:
    if not math.isfinite(options.stage_timeout) or not 0 < options.stage_timeout <= 14400:
        raise LocalError('stage timeout must be finite in (0,14400] seconds')
    if any(packages(options)) and not all(packages(options)):
        raise LocalError('three TypeScript package paths must be provided together')
    for path in packages(options):
        if path and not path.is_file():
            raise LocalError(f'package missing: {path}')
    if any(os.environ.get(k) for k in ('PYTEST_ADDOPTS', 'PYTEST_PLUGINS', 'PYTEST_DISABLE_PLUGIN_AUTOLOAD')):
        raise LocalError('clear inherited PYTEST overrides before local validation')
    if options.coresdk_lib and not options.coresdk_lib.is_absolute():
        raise LocalError('CoreSDK library directory must be absolute')


def main(argv=None) -> int:
    if argv is None and len(sys.argv) == 3 and sys.argv[1] in ('_assets', '_release-fixtures'):
        action = copy_assets if sys.argv[1] == '_assets' else prepare_release_fixtures
        action(Path(sys.argv[2]))
        return 0
    options = parser().parse_args(argv)
    def cancel(*_):
        raise KeyboardInterrupt
    previous = signal.signal(signal.SIGTERM, cancel)
    try:
        validate_options(options)
        options.output = options.output.resolve()
        options.python = os.path.abspath(os.path.expanduser(options.python))
        if options.output.exists():
            raise LocalError(f'output already exists: {options.output}')
        env = child_environment(dict(os.environ), options.tool_path, options.coresdk_lib)
        preflight = check(options, env)
        if options.action == 'check' or preflight['status'] != 'passed':
            options.output.mkdir(parents=True)
            strict.write_json(options.output / 'check.json', preflight)
            print(json.dumps(preflight, ensure_ascii=False, indent=2))
            return 0 if preflight['status'] == 'passed' else 1
        c3 = options.c3 or ROOT / ('cli/target/debug/c3.exe' if os.name == 'nt' else 'cli/target/debug/c3')
        env.update(CTWO_TEST_C3_BIN=str(c3.resolve()), C2_PORTABLE_MATRIX_C3_BIN=str(c3.resolve()))
        for key, path in zip(('C2_TYPESCRIPT_FASTDB_PACKAGE', 'C2_TYPESCRIPT_C2_MEM_PACKAGE', 'C2_TYPESCRIPT_COMPILER_PACKAGE'), packages(options)):
            if path:
                env[key] = str(path.resolve())
        env['C2_TYPESCRIPT_FASTDB_SOURCE_SHA'] = strict.FASTDB_SHA
        if options.release_fixtures:
            env['C2_DEV_FASTDB_RELEASE_FIXTURES'] = str(options.release_fixtures.resolve())
        if options.action == 'prepare':
            staging = options.output.parent / (options.output.name + '-inputs')
            staging.mkdir(parents=True, exist_ok=False)
            with ExitStack() as stack:
                from tools.dev.test_platform import resource_lease
                resources = (ROOT, ROOT / '.venv', ROOT / 'core/target', TS, options.fastdb_source or ROOT.parent / 'fastdb')
                for resource in sorted(set(resources)):
                    stack.enter_context(resource_lease(resource))
                steps = preparation_steps(options, staging)
                env = child_environment(env, [], options.coresdk_lib)
                report = execute_steps(steps, options.output, env, ROOT, options.stage_timeout, stop_on_failure=True, resources_leased=True)
            report['prepared_inputs'] = {'coresdk_lib': str(options.coresdk_lib), 'staging': str(staging),
                                          'repo_python': str(staging / 'repo-venv' / ('Scripts/python.exe' if os.name == 'nt' else 'bin/python')),
                                          'release_fixtures': str(options.release_fixtures or staging / 'release-fixtures'),
                                          'packages': {str(p): strict.sha256(p) for p in (staging / 'packages').glob('*.tgz')},
                                          'supplied_packages': {str(p.resolve()): strict.sha256(p) for p in packages(options) if p}}
        else:
            report = execute_steps(suite_steps(options, ROOT), options.output, env, ROOT, options.stage_timeout)
            for path, digest in preflight.get('input_hashes', {}).items():
                if strict.sha256(Path(path)) != digest:
                    report.update(status='failed', input_error=f'input changed during run: {path}')
        report['preflight'] = preflight
        report['source_sha'] = probe(['git', 'rev-parse', 'HEAD'], env)
        report['tracked_diff_sha256'] = hashlib.sha256(probe(['git', 'diff', '--binary', 'HEAD'], env).encode()).hexdigest()
        strict.write_json(options.output / 'run.json', report)
        print(f"{report['status']}: {options.output / 'run.json'}")
        return 0 if report['status'] == 'passed' else 1
    except (LocalError, strict.RunnerError, OSError, ValueError, subprocess.TimeoutExpired) as error:
        print(f'local test: {error}', file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print('local test cancelled; no passing evidence', file=sys.stderr)
        return 130
    finally:
        signal.signal(signal.SIGTERM, previous)


if __name__ == '__main__':
    raise SystemExit(main())
