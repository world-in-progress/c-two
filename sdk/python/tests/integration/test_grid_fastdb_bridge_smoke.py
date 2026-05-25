from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import textwrap
from collections.abc import Iterator
from contextlib import suppress
from pathlib import Path

import pytest

import c_two as cc
import fastdb4py as fdb
from c_two.config.settings import settings
from c_two.crm.descriptor import build_contract_descriptor
from c_two.transport.registry import _ProcessRegistry


@pytest.fixture(autouse=True)
def clean_runtime():
    previous_relay = settings.relay_anchor_address
    _ProcessRegistry.reset()
    try:
        yield
    finally:
        _ProcessRegistry.reset()
        settings.relay_anchor_address = previous_relay


def _load_fastdb_grid(monkeypatch):
    root = Path(__file__).resolve().parents[4]
    pytest.importorskip('fastdb4py', reason='fastdb grid smoke requires fastdb4py')
    pytest.importorskip('pandas', reason='grid smoke tests require examples dependencies')
    pytest.importorskip('pyarrow', reason='grid smoke tests require examples dependencies')
    monkeypatch.syspath_prepend(str(root / 'examples/python'))
    for module_name in (
        'grid.fastdb_bridge',
        'grid.grid_fdb_crm',
        'grid.nested_grid',
        'grid.grid_py_crm',
    ):
        sys.modules.pop(module_name, None)

    from grid.fastdb_bridge import grid_fastdb_bridge
    from grid.grid_fdb_crm import GridFastdb, GridId
    from grid.nested_grid import NestedGrid

    return GridFastdb, GridId, NestedGrid, grid_fastdb_bridge


def _load_python_grid(monkeypatch):
    root = Path(__file__).resolve().parents[4]
    pytest.importorskip('pandas', reason='grid smoke tests require examples dependencies')
    pytest.importorskip('pyarrow', reason='grid smoke tests require examples dependencies')
    monkeypatch.syspath_prepend(str(root / 'examples/python'))
    for module_name in (
        'grid.nested_grid',
        'grid.grid_py_crm',
    ):
        sys.modules.pop(module_name, None)

    from grid.nested_grid import NestedGrid
    from grid.grid_py_crm import GridPython

    return GridPython, NestedGrid


def _grid_fastdb_subprocess_env(root: Path) -> dict[str, str]:
    paths = [str(root / 'examples/python')]
    env = os.environ.copy()
    existing = env.get('PYTHONPATH')
    if existing:
        paths.append(existing)
    env['PYTHONPATH'] = os.pathsep.join(paths)
    env['C2_RELAY_ANCHOR_ADDRESS'] = ''
    return env


def _make_grid_resource(NestedGrid):
    return NestedGrid(
        epsg=4326,
        bounds=[0.0, 0.0, 4.0, 4.0],
        first_size=[2.0, 2.0],
        subdivide_rules=[[2, 2], [2, 2], [2, 2]],
    )


def _load_fastdb_bridge_helpers():
    pytest.importorskip('fastdb4py', reason='fastdb bridge smoke requires fastdb4py')

    from c_two.fastdb.bridge import derive_c_two_bridge

    return fdb, derive_c_two_bridge


def _make_iterator_array_crm_and_resource(fdb):
    def values_crm(self, ids):
        ...

    values_crm.__annotations__ = {
        'ids': fdb.Array[fdb.I32],
        'return': fdb.Array[fdb.I32],
    }
    values_crm.__name__ = 'values'
    values_crm.__qualname__ = 'IteratorArrayCRM.values'
    IteratorArrayCRM = cc.crm(namespace='test.fastdb.iterator', version='0.1.0')(
        type('IteratorArrayCRM', (), {'values': values_crm}),
    )

    class IteratorArrayResource:
        def __init__(self):
            self.seen_inputs: list[list[int]] = []

        def values(self, ids):
            assert iter(ids) is ids
            materialized = list(ids)
            self.seen_inputs.append(materialized)
            return iter(item + 10 for item in materialized)

    IteratorArrayResource.values.__annotations__ = {
        'ids': Iterator[int],
        'return': Iterator[int],
    }
    return IteratorArrayCRM, IteratorArrayResource()


def _exercise_python_grid(grid) -> None:
    schema = grid.get_schema()
    assert schema.epsg == 4326
    assert schema.bounds == [0.0, 0.0, 4.0, 4.0]
    assert schema.first_size == [2.0, 2.0]
    assert schema.subdivide_rules == [[2, 2], [2, 2], [2, 2]]

    infos = grid.get_grid_infos(1, [0, 1])
    assert [info.global_id for info in infos] == [0, 1]
    assert all(bool(info.activate) for info in infos)
    assert infos[0].min_x == pytest.approx(0.0)
    assert infos[0].max_x == pytest.approx(2.0)

    active = grid.get_active_grid_infos()
    assert active[0][0] == 1
    assert active[1][0] == 0
    assert grid.hello('Python') == 'Hello, Python!'
    assert grid.none_hello('world') is None
    assert grid.none_hello('fallback') == 'Hello, fallback!'

    keys = grid.subdivide_grids([1], [0])
    assert keys == ['2-0', '2-1', '2-4', '2-5']
    assert grid.get_parent_keys([2], [5]) == ['1-0']


def _exercise_fastdb_grid(grid, GridId) -> None:
    schema, rules = grid.get_schema()
    assert len(schema) == 1
    assert schema[0].epsg == 4326
    assert schema[0].min_x == pytest.approx(0.0)
    assert schema[0].max_x == pytest.approx(4.0)
    assert [(rule.level, rule.width, rule.height) for rule in rules] == [(0, 2, 2), (1, 2, 2), (2, 2, 2)]

    infos = grid.get_grid_infos(1, (0, 1))
    assert [info.global_id for info in infos] == [0, 1]
    assert all(bool(info.activate) for info in infos)
    assert infos[0].min_x == pytest.approx(0.0)
    assert infos[0].max_x == pytest.approx(2.0)

    active = grid.get_active_grid_infos()
    assert active
    assert active[0].level == 1
    assert active[0].global_id == 0
    assert grid.hello('FastDB') == 'Hello, FastDB!'

    maybe = grid.none_hello('world')
    assert len(maybe) == 1
    assert bool(maybe[0].is_null) is True
    assert maybe[0].value == ''
    maybe = grid.none_hello('bridge')
    assert len(maybe) == 1
    assert bool(maybe[0].is_null) is False
    assert maybe[0].value == 'Hello, bridge!'

    keys = grid.subdivide_grids([1], [0])
    assert keys == ['2-0', '2-1', '2-4', '2-5']
    assert grid.get_parent_keys([2], [5]) == ['1-0']


def test_grid_py_crm_uses_pickle_fallback_refs(monkeypatch):
    GridPython, _NestedGrid = _load_python_grid(monkeypatch)

    descriptor = build_contract_descriptor(GridPython)
    methods = {method['name']: method for method in descriptor['methods']}
    encoded = json.dumps(descriptor, sort_keys=True)

    assert methods['get_schema']['wire']['output']['family'] == 'python-pickle-default'
    assert methods['get_grid_infos']['wire']['input']['family'] == 'python-pickle-default'
    assert methods['get_grid_infos']['wire']['output']['family'] == 'python-pickle-default'
    assert methods['none_hello']['wire']['output']['family'] == 'python-pickle-default'
    assert 'org.fastdb.call-db' not in encoded
    with pytest.raises(ValueError, match='python-pickle-default'):
        cc.export_contract_descriptor(GridPython)


def test_grid_python_fallback_works_thread_local(monkeypatch):
    GridPython, NestedGrid = _load_python_grid(monkeypatch)
    cc.register(
        GridPython,
        _make_grid_resource(NestedGrid),
        name='grid-python-thread',
    )

    grid = cc.connect(GridPython, name='grid-python-thread')
    try:
        assert grid.client._mode == 'thread'  # noqa: SLF001
        _exercise_python_grid(grid)
    finally:
        cc.close(grid)


def test_grid_python_fallback_works_direct_ipc(monkeypatch):
    GridPython, NestedGrid = _load_python_grid(monkeypatch)
    cc.register(
        GridPython,
        _make_grid_resource(NestedGrid),
        name='grid-python-ipc',
    )
    address = cc.server_address()
    assert address is not None

    grid = cc.connect(GridPython, name='grid-python-ipc', address=address)
    try:
        assert grid.client._mode == 'ipc'  # noqa: SLF001
        _exercise_python_grid(grid)
    finally:
        cc.close(grid)


def test_grid_fastdb_contract_uses_call_db_refs(monkeypatch):
    GridFastdb, _GridId, _NestedGrid, _bridge = _load_fastdb_grid(monkeypatch)

    descriptor = build_contract_descriptor(GridFastdb)
    methods = {method['name']: method for method in descriptor['methods']}
    encoded = json.dumps(descriptor, sort_keys=True)

    assert set(methods) == {
        'get_schema',
        'subdivide_grids',
        'get_parent_keys',
        'get_grid_infos',
        'get_active_grid_infos',
        'hello',
        'none_hello',
    }
    for method in methods.values():
        for direction in ('input', 'output'):
            wire_ref = method['wire'][direction]
            if wire_ref is not None:
                assert wire_ref['id'] == 'org.fastdb.call-db'
    assert methods['get_grid_infos']['wire']['input']['id'] == 'org.fastdb.call-db'
    assert methods['get_grid_infos']['wire']['output']['id'] == 'org.fastdb.call-db'
    assert methods['get_active_grid_infos']['wire']['output']['id'] == 'org.fastdb.call-db'
    assert 'python-pickle-default' not in encoded
    assert 'c-two.control.json' not in encoded

    portable = json.loads(cc.export_contract_descriptor(GridFastdb))
    portable_encoded = json.dumps(portable, sort_keys=True)
    assert portable['schema'] == 'c-two.contract.v1'
    assert 'python-pickle-default' not in portable_encoded


@pytest.mark.timeout(180)
def test_grid_fastdb_export_feeds_fastdb_typescript_codegen(monkeypatch, tmp_path):
    GridFastdb, _GridId, _NestedGrid, _bridge = _load_fastdb_grid(monkeypatch)

    from c_two.fastdb.typescript import generate_c_two_typescript_helpers

    portable = json.loads(cc.export_contract_descriptor(GridFastdb))
    artifacts = json.loads(cc.export_contract_payload_abi_artifacts(GridFastdb))
    methods = {method['name']: method for method in portable['methods']}

    artifact_hashes = {
        artifact['schema']: set()
        for artifact in artifacts
        if isinstance(artifact, dict) and isinstance(artifact.get('schema'), str)
    }
    from fastdb4py.schema import schema_sha256
    for artifact in artifacts:
        artifact_hashes[artifact['schema']].add(schema_sha256(artifact))
    assert methods['get_grid_infos']['wire']['input']['schema_sha256'] in artifact_hashes['fastdb.call-db.schema.v1']
    assert methods['get_grid_infos']['wire']['output']['schema_sha256'] in artifact_hashes['fastdb.call-db.schema.v1']
    assert methods['get_active_grid_infos']['wire']['output']['schema_sha256'] in artifact_hashes['fastdb.call-db.schema.v1']
    assert any(
        artifact.get('schema') == 'fastdb.schema.v1'
        and artifact.get('feature', {}).get('name') == 'FastdbGridAttribute'
        for artifact in artifacts
    )
    assert any(
        artifact.get('schema') == 'fastdb.schema.v1'
        and artifact.get('feature', {}).get('name') == 'GridId'
        for artifact in artifacts
    )

    generated = generate_c_two_typescript_helpers(
        portable,
        artifacts,
    )

    assert 'export class FastdbGridAttribute extends Feature' in generated
    assert 'export const GET_GRID_INFOS_INPUT_CALL_DB_CODEC' in generated
    assert 'export const GET_GRID_INFOS_OUTPUT_CALL_DB_CODEC' in generated
    assert 'export const GET_ACTIVE_GRID_INFOS_OUTPUT_CALL_DB_CODEC' in generated
    assert 'python-pickle-default' not in generated

    npm = shutil.which('npm')
    node = shutil.which('node')
    tsc = shutil.which('tsc')
    cargo = shutil.which('cargo')
    c2_mem_ffi_ts = Path(__file__).resolve().parents[4] / 'core/foundation/c2-mem-ffi/bindings/typescript'
    if npm is None or node is None or tsc is None or cargo is None or not c2_mem_ffi_ts.exists():
        pytest.skip('grid FastDB TypeScript helper smoke requires npm, node, tsc, cargo, and c2-mem-ffi TypeScript bindings')

    subprocess.run([npm, '--prefix', str(c2_mem_ffi_ts), 'run', 'pretest'], check=True)
    package_dir = tmp_path / 'grid-fastdb-ts'
    package_dir.mkdir()
    (package_dir / 'package.json').write_text(json.dumps({
        'type': 'module',
        'dependencies': {
            'fastdb4ts': '0.0.3',
        },
    }))
    subprocess.run([npm, 'install', '--ignore-scripts', '--no-audit', '--fund=false'], check=True, cwd=package_dir)
    contract_path = package_dir / 'grid.contract.json'
    c_two_client_path = package_dir / 'grid-client.ts'
    contract_path.write_text(json.dumps(portable))
    subprocess.run([
        cargo,
        'run',
        '--quiet',
        '--manifest-path',
        str(Path(__file__).resolve().parents[4] / 'cli/Cargo.toml'),
        '--',
        'contract',
        'codegen',
        'typescript',
        str(contract_path),
        '--strict-codecs',
        '--out',
        str(c_two_client_path),
    ], check=True, cwd=Path(__file__).resolve().parents[4])
    node_modules = package_dir / 'node_modules'
    c2_scope = node_modules / '@c-two'
    c2_scope.mkdir(exist_ok=True)
    (c2_scope / 'c2-mem-ffi').symlink_to(c2_mem_ffi_ts, target_is_directory=True)
    (package_dir / 'tsconfig.json').write_text(json.dumps({
        'compilerOptions': {
            'module': 'NodeNext',
            'moduleResolution': 'NodeNext',
            'target': 'ES2022',
            'strict': True,
            'skipLibCheck': True,
            'outDir': 'dist',
        },
        'include': ['*.ts'],
    }))
    (package_dir / 'fastdb-c2-codecs.ts').write_text(generated)
    smoke_source = textwrap.dedent("""\
        import { initFastdb, ORM } from 'fastdb4ts';
        import {
          FastdbGridAttribute,
          GridId,
          GET_ACTIVE_GRID_INFOS_OUTPUT_CALL_DB_CODEC,
          GET_GRID_INFOS_INPUT_CALL_DB_CODEC,
          GET_GRID_INFOS_OUTPUT_CALL_DB_CODEC,
          createFastdbC2CodecRegistry,
          createFastdbC2ResponsePayloadAllocator,
          createGridFastdbTypedClient,
        } from './fastdb-c2-codecs.js';
        import {
          GRID_FASTDB_CONTRACT,
          GridFastdbClient,
          createGridFastdbClientCodecTransport,
          createRelayAwareHttpEncodedTransport,
        } from './grid-client.js';

        function assertEqual(actual: unknown, expected: unknown): void {
          if (actual !== expected) {
            throw new Error(`expected ${String(expected)}, got ${String(actual)}`);
          }
        }

        function assertDeepEqual(actual: unknown, expected: unknown): void {
          if (JSON.stringify(actual) !== JSON.stringify(expected)) {
            throw new Error(`expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
          }
        }

        function streamedResponse(payload: Uint8Array): {
          status: number;
          headers: { get(name: string): string | null };
          body: { getReader(): { read(): Promise<{ done: boolean; value?: Uint8Array }>; releaseLock(): void } };
          arrayBuffer(): Promise<ArrayBuffer>;
          text(): Promise<string>;
        } {
          const chunks = [
            payload.subarray(0, Math.floor(payload.byteLength / 2)),
            payload.subarray(Math.floor(payload.byteLength / 2)),
          ];
          let index = 0;
          return {
            status: 200,
            headers: {
              get(name: string): string | null {
                return name.toLowerCase() === 'content-length' ? String(payload.byteLength) : null;
              },
            },
            body: {
              getReader() {
                return {
                  async read(): Promise<{ done: boolean; value?: Uint8Array }> {
                    const value = chunks[index++];
                    if (value === undefined) {
                      return { done: true };
                    }
                    return { done: false, value };
                  },
                  releaseLock(): void {},
                };
              },
            },
            async arrayBuffer(): Promise<ArrayBuffer> {
              throw new Error('response allocator should stream directly into FastDB-owned bytes');
            },
            async text(): Promise<string> {
              return '';
            },
          };
        }

        const module = await initFastdb();
        const responsePayloadAllocator = createFastdbC2ResponsePayloadAllocator(module);

        const originalPush = ORM.prototype.push;
        (ORM.prototype as any).push = function pushShouldNotRun(): void {
          throw new Error('row push fallback used');
        };

        try {
          const input = GET_GRID_INFOS_INPUT_CALL_DB_CODEC.decode(
            GET_GRID_INFOS_INPUT_CALL_DB_CODEC.encode([1, [0, 1]])
          ) as any[];
          assertDeepEqual(input, [1, [0, 1]]);

          const attrs = [
            new FastdbGridAttribute({
              level: 1,
              type_: 2,
              activate: true,
              global_id: 0,
              deleted: false,
              elevation: 12.5,
              local_id: 7,
              min_x: 0,
              min_y: 0,
              max_x: 2,
              max_y: 2,
            }),
          ];
          const decodedAttrs = GET_GRID_INFOS_OUTPUT_CALL_DB_CODEC.decode(
            GET_GRID_INFOS_OUTPUT_CALL_DB_CODEC.encode(attrs)
          ) as any[];
          assertEqual(decodedAttrs[0].activate, true);
          assertEqual(decodedAttrs[0].type_, 2);
          assertEqual(decodedAttrs[0].max_y, 2);

          const ids = [new GridId({ level: 1, global_id: 0 }), new GridId({ level: 1, global_id: 1 })];
          const decodedIds = GET_ACTIVE_GRID_INFOS_OUTPUT_CALL_DB_CODEC.decode(
            GET_ACTIVE_GRID_INFOS_OUTPUT_CALL_DB_CODEC.encode(ids)
          ) as any[];
          assertEqual(decodedIds.length, 2);
          assertEqual(decodedIds[1].global_id, 1);

          const heldIds = GET_ACTIVE_GRID_INFOS_OUTPUT_CALL_DB_CODEC.view(
            GET_ACTIVE_GRID_INFOS_OUTPUT_CALL_DB_CODEC.encode(ids)
          );
          try {
            const columns = heldIds.table('return_0').column as any;
            assertEqual(columns.global_id.get(1), 1);
          } finally {
            heldIds.close();
          }

          const fetchCalls: Array<{
            input: string;
            init: { method?: string; headers?: Record<string, string>; body?: Uint8Array };
          }> = [];
          const fetchImpl = async (
            input: string,
            init: { method?: string; headers?: Record<string, string>; body?: Uint8Array },
          ) => {
            fetchCalls.push({ input, init });
            if (init.method === 'GET') {
              return {
                status: 200,
                async arrayBuffer(): Promise<ArrayBuffer> {
                  return new ArrayBuffer(0);
                },
                async text(): Promise<string> {
                  return JSON.stringify([
                    {
                      name: 'examples/grid',
                      relay_url: 'http://relay.example/base/',
                      crm_ns: GRID_FASTDB_CONTRACT.namespace,
                      crm_name: GRID_FASTDB_CONTRACT.name,
                      crm_ver: GRID_FASTDB_CONTRACT.version,
                      abi_hash: GRID_FASTDB_CONTRACT.abiHash,
                      signature_hash: GRID_FASTDB_CONTRACT.signatureHash,
                      max_payload_size: 1048576,
                    },
                  ]);
                },
              };
            }
            const inputArgs = GET_GRID_INFOS_INPUT_CALL_DB_CODEC.decode(init.body ?? new Uint8Array()) as any[];
            assertDeepEqual(inputArgs, [1, [0, 1]]);
            const responsePayload = GET_GRID_INFOS_OUTPUT_CALL_DB_CODEC.encode(attrs);
            return streamedResponse(responsePayload);
          };
          const c2Client = new GridFastdbClient(
            createGridFastdbClientCodecTransport(
              createRelayAwareHttpEncodedTransport('http://anchor.example/root/', {
                fetch: fetchImpl,
                responsePayloadAllocator,
              }),
              createFastdbC2CodecRegistry(),
            ),
            'examples/grid',
          );
          const typedClient = createGridFastdbTypedClient(c2Client);
          const rpcAttrs: FastdbGridAttribute[] = await typedClient.get_grid_infos(1, [0, 1]);
          assertEqual(rpcAttrs[0].global_id, 0);
          assertEqual(rpcAttrs[0].activate, true);
          if (fetchCalls.length !== 2) {
            throw new Error(`expected resolve plus data-plane call, got ${fetchCalls.length}`);
          }
          const expectedResolveUrl = `http://anchor.example/root/_resolve/examples%2Fgrid?crm_ns=${encodeURIComponent(GRID_FASTDB_CONTRACT.namespace)}&crm_name=${encodeURIComponent(GRID_FASTDB_CONTRACT.name)}&crm_ver=${encodeURIComponent(GRID_FASTDB_CONTRACT.version)}&abi_hash=${encodeURIComponent(GRID_FASTDB_CONTRACT.abiHash)}&signature_hash=${encodeURIComponent(GRID_FASTDB_CONTRACT.signatureHash)}`;
          if (fetchCalls[0].input !== expectedResolveUrl) {
            throw new Error(`unexpected resolve URL ${fetchCalls[0].input}`);
          }
          if (fetchCalls[1].input !== 'http://relay.example/base/examples%2Fgrid/get_grid_infos') {
            throw new Error(`unexpected data-plane URL ${fetchCalls[1].input}`);
          }
          if (fetchCalls[1].init.headers?.['x-c2-expected-crm-ns'] !== GRID_FASTDB_CONTRACT.namespace) {
            throw new Error('missing expected CRM namespace header');
          }

          fetchCalls.length = 0;
          const heldRpcAttrs = await typedClient.hold_get_grid_infos(1, [0, 1]);
          try {
            const heldView = heldRpcAttrs.value as any;
            const columns = heldView.table('return_0').column as any;
            assertEqual(columns.global_id.get(0), 0);
            assertEqual(columns.activate.get(0), 1);
          } finally {
            heldRpcAttrs.release();
          }
          try {
            (heldRpcAttrs.value as any).table('return_0');
            throw new Error('held call-db view remained open after release');
          } catch (error) {
            const message = String(error);
            if (!message.includes('view is closed') && !message.includes('C2HeldResult released')) {
              throw error;
            }
          }
          if (fetchCalls.length !== 1) {
            throw new Error(`expected held call to reuse cached resolve and perform one data-plane call, got ${fetchCalls.length}`);
          }
          if (fetchCalls[0].input !== 'http://relay.example/base/examples%2Fgrid/get_grid_infos') {
            throw new Error(`unexpected cached held data-plane URL ${fetchCalls[0].input}`);
          }
        } finally {
          (ORM.prototype as any).push = originalPush;
        }
        """)
    (package_dir / 'smoke.ts').write_text(smoke_source)
    subprocess.run([tsc, '-p', str(package_dir / 'tsconfig.json')], check=True, cwd=package_dir)
    assert (node_modules / 'fastdb4ts/dist/wasm/fastdb4ts.wasm').exists()
    subprocess.run([node, str(package_dir / 'dist' / 'smoke.js')], check=True, cwd=package_dir)
    if sys.platform.startswith('linux') and os.environ.get('C2_RUN_NODE_NATIVE_IPC_SMOKE') != '1':
        pytest.skip('Node native c2-mem-ffi IPC smoke is opt-in on Linux; FastDB TS codec/codegen smoke already ran')

    previous_shm_threshold = settings._shm_threshold  # noqa: SLF001
    cc.set_transport_policy(shm_threshold=1)
    try:
        bridge = _bridge()
        cc.register(
            GridFastdb,
            _make_grid_resource(_NestedGrid),
            name='examples/grid',
            bridge=bridge,
        )
        address = cc.server_address()
        assert address is not None
        real_ipc_source = textwrap.dedent("""\
        import {
          createNodeIpcConnect,
          createC2MemFfiRequestPoolFromSymbols,
          createC2MemFfiResponsePoolFromSymbols,
          loadBundledC2MemFfiNodeNativeSymbols,
        } from '@c-two/c2-mem-ffi';
        import { initFastdb } from 'fastdb4ts';
        import {
          createFastdbC2CodecRegistry,
          createFastdbC2ResponsePayloadAllocator,
          createGridFastdbTypedClient,
        } from './dist/fastdb-c2-codecs.js';
        import {
          GridFastdbClient,
          createC2MemFfiNativeBuddyRequestShmWriter,
          createC2MemFfiNativeBuddyResponseShmReader,
          createGridFastdbClientCodecTransport,
          createIpcEncodedTransport,
        } from './dist/grid-client.js';

        const address = __ADDRESS__;

        function assertEqual(actual, expected, label) {
          if (actual !== expected) {
            throw new Error(`${label}: expected ${String(expected)}, got ${String(actual)}`);
          }
        }

        const module = await initFastdb();
        const fastdbResponsePayloadAllocator = createFastdbC2ResponsePayloadAllocator(module);
        const { requestSymbols, responseSymbols } = loadBundledC2MemFfiNodeNativeSymbols();
        const requestPool = await createC2MemFfiRequestPoolFromSymbols(requestSymbols, {
          prefix: `/cc2ts${process.pid.toString(36)}`,
          segmentSize: 65536,
          maxSegments: 1,
          minBlockSize: 4096,
        });
        const baseRequestShmWriter = createC2MemFfiNativeBuddyRequestShmWriter({ pool: requestPool });
        const baseResponseShmReader = createC2MemFfiNativeBuddyResponseShmReader({
          binding: {
            createResponsePool(options) {
              return createC2MemFfiResponsePoolFromSymbols(responseSymbols, options);
            },
          },
        });
        let requestWrites = 0;
        let requestReleases = 0;
        let requestConsumed = 0;
        let responseReads = 0;
        let responseReleases = 0;
        let allocatorCalls = 0;
        const responsePayloadAllocator = (byteLength) => {
          allocatorCalls += 1;
          return fastdbResponsePayloadAllocator(byteLength);
        };
        const requestShmWriter = {
          prefix: baseRequestShmWriter.prefix,
          segments: baseRequestShmWriter.segments,
          async write(payload) {
            requestWrites += 1;
            return await baseRequestShmWriter.write(payload);
          },
          async release(block) {
            requestReleases += 1;
            return await baseRequestShmWriter.release(block);
          },
          async markConsumed(block) {
            requestConsumed += 1;
            return await baseRequestShmWriter.markConsumed(block);
          },
          async close() {
            return await baseRequestShmWriter.close();
          },
        };
        const responseShmReader = {
          async read(block, destination) {
            responseReads += 1;
            return await baseResponseShmReader.read(block, destination);
          },
          async release(block) {
            responseReleases += 1;
            return await baseResponseShmReader.release(block);
          },
          async close() {
            return await baseResponseShmReader.close();
          },
        };
        const ipcTransport = createIpcEncodedTransport(address, {
          connect: createNodeIpcConnect(),
          requestShmWriter,
          requestShmThreshold: 0,
          responseShmReader,
          responsePayloadAllocator,
        });
        const noAllocatorIpcTransport = createIpcEncodedTransport(address, {
          connect: createNodeIpcConnect(),
          requestShmWriter,
          requestShmThreshold: 0,
          responseShmReader,
        });
        const noRequestShmIpcTransport = createIpcEncodedTransport(address, {
          connect: createNodeIpcConnect(),
          responseShmReader,
        });
        const noRequestShmAllocatorIpcTransport = createIpcEncodedTransport(address, {
          connect: createNodeIpcConnect(),
          responseShmReader,
          responsePayloadAllocator,
        });
        let staleRouteIpcTransport;
        try {
          const client = new GridFastdbClient(
            createGridFastdbClientCodecTransport(ipcTransport, createFastdbC2CodecRegistry()),
            'examples/grid',
          );
          const typedClient = createGridFastdbTypedClient(client);
          const attrs = await typedClient.get_grid_infos(1, [0, 1]);
          assertEqual(attrs.length, 2, 'ordinary IPC attr count');
          assertEqual(attrs[0].global_id, 0, 'ordinary IPC attr global_id');
          assertEqual(attrs[1].global_id, 1, 'ordinary IPC attr global_id');
          const held = await typedClient.hold_get_grid_infos(1, [0, 1]);
          try {
            const table = held.value.table('return_0');
            assertEqual(table.column.global_id.get(0), 0, 'held IPC attr global_id');
            assertEqual(table.column.activate.get(1), 1, 'held IPC attr activate');
          } finally {
            held.release();
          }
          assertEqual(requestWrites, 2, 'request SHM write count');
          assertEqual(requestConsumed, 2, 'request SHM consumed count');
          assertEqual(requestReleases, 0, 'request SHM local release count');
          assertEqual(responseReads, 2, 'response SHM read count');
          assertEqual(responseReleases, 2, 'response SHM release count');
          assertEqual(allocatorCalls, 2, 'allocator-backed response count');

          await ipcTransport.close();

          const noAllocatorClient = new GridFastdbClient(
            createGridFastdbClientCodecTransport(noAllocatorIpcTransport, createFastdbC2CodecRegistry()),
            'examples/grid',
          );
          const noAllocatorTypedClient = createGridFastdbTypedClient(noAllocatorClient);
          const noAllocatorAttrs = await noAllocatorTypedClient.get_grid_infos(1, [0, 1]);
          assertEqual(noAllocatorAttrs.length, 2, 'no-allocator IPC attr count');
          assertEqual(noAllocatorAttrs[0].global_id, 0, 'no-allocator IPC attr global_id');
          assertEqual(noAllocatorAttrs[1].global_id, 1, 'no-allocator IPC attr global_id');
          assertEqual(requestWrites, 3, 'no-allocator request SHM write count');
          assertEqual(requestConsumed, 3, 'no-allocator request SHM consumed count');
          assertEqual(requestReleases, 0, 'no-allocator request SHM local release count');
          assertEqual(responseReads, 3, 'no-allocator response SHM read count');
          assertEqual(responseReleases, 3, 'no-allocator response SHM release count');
          assertEqual(allocatorCalls, 2, 'no-allocator response must not call allocator');

          const noRequestShmClient = new GridFastdbClient(
            createGridFastdbClientCodecTransport(noRequestShmIpcTransport, createFastdbC2CodecRegistry()),
            'examples/grid',
          );
          const noRequestShmTypedClient = createGridFastdbTypedClient(noRequestShmClient);
          const noRequestShmAttrs = await noRequestShmTypedClient.get_grid_infos(1, [0, 1]);
          assertEqual(noRequestShmAttrs.length, 2, 'no-request-SHM IPC attr count');
          assertEqual(noRequestShmAttrs[0].global_id, 0, 'no-request-SHM IPC attr global_id');
          assertEqual(noRequestShmAttrs[1].global_id, 1, 'no-request-SHM IPC attr global_id');
          assertEqual(requestWrites, 3, 'no-request-SHM IPC must not write request SHM');
          assertEqual(requestConsumed, 3, 'no-request-SHM IPC must not consume request SHM');
          assertEqual(requestReleases, 0, 'no-request-SHM IPC request SHM local release count');
          assertEqual(responseReads, 4, 'no-request-SHM IPC response SHM read count');
          assertEqual(responseReleases, 4, 'no-request-SHM IPC response SHM release count');
          assertEqual(allocatorCalls, 2, 'no-request-SHM IPC response must not call allocator');

          const noRequestShmAllocatorClient = new GridFastdbClient(
            createGridFastdbClientCodecTransport(noRequestShmAllocatorIpcTransport, createFastdbC2CodecRegistry()),
            'examples/grid',
          );
          const noRequestShmAllocatorTypedClient = createGridFastdbTypedClient(noRequestShmAllocatorClient);
          const noRequestShmAllocatorAttrs = await noRequestShmAllocatorTypedClient.get_grid_infos(1, [0, 1]);
          assertEqual(noRequestShmAllocatorAttrs.length, 2, 'no-request-SHM allocator IPC attr count');
          assertEqual(noRequestShmAllocatorAttrs[0].global_id, 0, 'no-request-SHM allocator IPC attr global_id');
          assertEqual(noRequestShmAllocatorAttrs[1].global_id, 1, 'no-request-SHM allocator IPC attr global_id');
          assertEqual(requestWrites, 3, 'no-request-SHM allocator IPC must not write request SHM');
          assertEqual(requestConsumed, 3, 'no-request-SHM allocator IPC must not consume request SHM');
          assertEqual(requestReleases, 0, 'no-request-SHM allocator IPC request SHM local release count');
          assertEqual(responseReads, 5, 'no-request-SHM allocator IPC response SHM read count');
          assertEqual(responseReleases, 5, 'no-request-SHM allocator IPC response SHM release count');
          assertEqual(allocatorCalls, 3, 'no-request-SHM allocator IPC response allocator count');

          const noRequestShmHeldAttrs = await noRequestShmTypedClient.hold_get_grid_infos(1, [0, 1]);
          const noRequestShmHeldAttrBuffer = noRequestShmHeldAttrs.buffer;
          if (!(noRequestShmHeldAttrBuffer instanceof Uint8Array) || noRequestShmHeldAttrBuffer.byteLength <= 0) {
            throw new Error('held no-request-SHM IPC attr buffer was not retained as owned bytes');
          }
          try {
            const table = noRequestShmHeldAttrs.value.table('return_0');
            assertEqual(table.column.global_id.get(0), 0, 'held no-request-SHM IPC attr first global_id');
            assertEqual(table.column.activate.get(1), 1, 'held no-request-SHM IPC attr activate');
          } finally {
            noRequestShmHeldAttrs.release();
          }
          try {
            noRequestShmHeldAttrs.value.table('return_0');
            throw new Error('held no-request-SHM call-db view remained open after release');
          } catch (error) {
            const message = String(error);
            if (!message.includes('view is closed') && !message.includes('C2HeldResult released')) {
              throw error;
            }
          }
          try {
            void noRequestShmHeldAttrs.buffer;
            throw new Error('held no-request-SHM IPC attr buffer remained accessible after release');
          } catch (error) {
            if (!String(error).includes('C2HeldResult released')) {
              throw error;
            }
          }
          assertEqual(requestWrites, 3, 'held no-request-SHM IPC must not write request SHM');
          assertEqual(requestConsumed, 3, 'held no-request-SHM IPC must not consume request SHM');
          assertEqual(requestReleases, 0, 'held no-request-SHM IPC request SHM local release count');
          assertEqual(responseReads, 6, 'held no-request-SHM IPC response SHM read count');
          assertEqual(responseReleases, 6, 'held no-request-SHM IPC response SHM release count');
          assertEqual(allocatorCalls, 3, 'held no-request-SHM IPC response must not call allocator');

          const noRequestShmAllocatorHeldAttrs = await noRequestShmAllocatorTypedClient.hold_get_grid_infos(1, [0, 1]);
          const noRequestShmAllocatorHeldAttrBuffer = noRequestShmAllocatorHeldAttrs.buffer;
          if (
            typeof noRequestShmAllocatorHeldAttrBuffer !== 'object'
            || noRequestShmAllocatorHeldAttrBuffer === null
            || typeof noRequestShmAllocatorHeldAttrBuffer.byteLength !== 'number'
            || noRequestShmAllocatorHeldAttrBuffer.byteLength <= 0
          ) {
            throw new Error('held no-request-SHM allocator IPC attr buffer was not retained as a FastDB-owned payload');
          }
          if (noRequestShmAllocatorHeldAttrBuffer instanceof Uint8Array || noRequestShmAllocatorHeldAttrBuffer instanceof ArrayBuffer) {
            throw new Error('held no-request-SHM allocator IPC attr buffer did not use FastDB-owned allocator storage');
          }
          try {
            const table = noRequestShmAllocatorHeldAttrs.value.table('return_0');
            assertEqual(table.column.global_id.get(0), 0, 'held no-request-SHM allocator IPC attr first global_id');
            assertEqual(table.column.activate.get(1), 1, 'held no-request-SHM allocator IPC attr activate');
          } finally {
            noRequestShmAllocatorHeldAttrs.release();
          }
          try {
            noRequestShmAllocatorHeldAttrs.value.table('return_0');
            throw new Error('held no-request-SHM allocator call-db view remained open after release');
          } catch (error) {
            const message = String(error);
            if (!message.includes('view is closed') && !message.includes('C2HeldResult released')) {
              throw error;
            }
          }
          try {
            void noRequestShmAllocatorHeldAttrs.buffer;
            throw new Error('held no-request-SHM allocator IPC attr buffer remained accessible after release');
          } catch (error) {
            if (!String(error).includes('C2HeldResult released')) {
              throw error;
            }
          }
          assertEqual(requestWrites, 3, 'held no-request-SHM allocator IPC must not write request SHM');
          assertEqual(requestConsumed, 3, 'held no-request-SHM allocator IPC must not consume request SHM');
          assertEqual(requestReleases, 0, 'held no-request-SHM allocator IPC request SHM local release count');
          assertEqual(responseReads, 7, 'held no-request-SHM allocator IPC response SHM read count');
          assertEqual(responseReleases, 7, 'held no-request-SHM allocator IPC response SHM release count');
          assertEqual(allocatorCalls, 4, 'held no-request-SHM allocator IPC response allocator count');

          const activeIds = await noAllocatorTypedClient.get_active_grid_infos();
          assertEqual(activeIds.length, 4, 'output-only IPC active id count');
          assertEqual(activeIds[0].level, 1, 'output-only IPC active level');
          assertEqual(activeIds[0].global_id, 0, 'output-only IPC active first global_id');
          assertEqual(activeIds[3].global_id, 3, 'output-only IPC active last global_id');
          assertEqual(requestWrites, 3, 'output-only IPC must not write request SHM');
          assertEqual(requestConsumed, 3, 'output-only IPC must not consume request SHM');
          assertEqual(requestReleases, 0, 'output-only IPC request SHM local release count');
          assertEqual(responseReads, 8, 'output-only IPC response SHM read count');
          assertEqual(responseReleases, 8, 'output-only IPC response SHM release count');
          assertEqual(allocatorCalls, 4, 'output-only IPC response must not call allocator');

          const heldActiveIds = await noAllocatorTypedClient.hold_get_active_grid_infos();
          try {
            const table = heldActiveIds.value.table('return_0');
            assertEqual(table.column.level.get(0), 1, 'held output-only IPC active level');
            assertEqual(table.column.global_id.get(3), 3, 'held output-only IPC active last global_id');
          } finally {
            heldActiveIds.release();
          }
          try {
            heldActiveIds.value.table('return_0');
            throw new Error('held output-only call-db view remained open after release');
          } catch (error) {
            const message = String(error);
            if (!message.includes('view is closed') && !message.includes('C2HeldResult released')) {
              throw error;
            }
          }
          assertEqual(requestWrites, 3, 'held output-only IPC must not write request SHM');
          assertEqual(requestConsumed, 3, 'held output-only IPC must not consume request SHM');
          assertEqual(requestReleases, 0, 'held output-only IPC request SHM local release count');
          assertEqual(responseReads, 9, 'held output-only IPC response SHM read count');
          assertEqual(responseReleases, 9, 'held output-only IPC response SHM release count');
          assertEqual(allocatorCalls, 4, 'held output-only IPC response must not call allocator');

          const noAllocatorHeldAttrs = await noAllocatorTypedClient.hold_get_grid_infos(1, [0, 1]);
          const noAllocatorHeldAttrBuffer = noAllocatorHeldAttrs.buffer;
          if (!(noAllocatorHeldAttrBuffer instanceof Uint8Array) || noAllocatorHeldAttrBuffer.byteLength <= 0) {
            throw new Error('held no-allocator IPC attr buffer was not retained as owned bytes');
          }
          try {
            const table = noAllocatorHeldAttrs.value.table('return_0');
            assertEqual(table.column.global_id.get(0), 0, 'held no-allocator IPC attr first global_id');
            assertEqual(table.column.activate.get(1), 1, 'held no-allocator IPC attr activate');
          } finally {
            noAllocatorHeldAttrs.release();
          }
          try {
            noAllocatorHeldAttrs.value.table('return_0');
            throw new Error('held no-allocator call-db view remained open after release');
          } catch (error) {
            const message = String(error);
            if (!message.includes('view is closed') && !message.includes('C2HeldResult released')) {
              throw error;
            }
          }
          try {
            void noAllocatorHeldAttrs.buffer;
            throw new Error('held no-allocator IPC attr buffer remained accessible after release');
          } catch (error) {
            if (!String(error).includes('C2HeldResult released')) {
              throw error;
            }
          }
          assertEqual(requestWrites, 4, 'held no-allocator IPC request SHM write count');
          assertEqual(requestConsumed, 4, 'held no-allocator IPC request SHM consumed count');
          assertEqual(requestReleases, 0, 'held no-allocator IPC request SHM local release count');
          assertEqual(responseReads, 10, 'held no-allocator IPC response SHM read count');
          assertEqual(responseReleases, 10, 'held no-allocator IPC response SHM release count');
          assertEqual(allocatorCalls, 4, 'held no-allocator IPC response must not call allocator');

          staleRouteIpcTransport = createIpcEncodedTransport(address, {
            connect: createNodeIpcConnect(),
            requestShmWriter,
            requestShmThreshold: 0,
            responseShmReader,
          });
          const staleRouteClient = new GridFastdbClient(
            createGridFastdbClientCodecTransport(staleRouteIpcTransport, createFastdbC2CodecRegistry()),
            'examples/grid',
          );
          const staleRouteTypedClient = createGridFastdbTypedClient(staleRouteClient);
          const staleRoutePrime = await staleRouteTypedClient.get_active_grid_infos();
          assertEqual(staleRoutePrime.length, 4, 'stale-route prime IPC active id count');
          assertEqual(requestWrites, 4, 'stale-route prime IPC must not write request SHM');
          assertEqual(requestConsumed, 4, 'stale-route prime IPC must not consume request SHM');
          assertEqual(requestReleases, 0, 'stale-route prime IPC request SHM local release count');
          assertEqual(responseReads, 11, 'stale-route prime IPC response SHM read count');
          assertEqual(responseReleases, 11, 'stale-route prime IPC response SHM release count');
          assertEqual(allocatorCalls, 4, 'stale-route prime IPC response allocator count');
          await new Promise((resolve, reject) => {
            process.stdout.write('C2_STALE_ROUTE_READY\\n', (error) => {
              if (error) {
                reject(error);
              } else {
                resolve(undefined);
              }
            });
          });
          process.stdin.resume();
          await new Promise((resolve) => process.stdin.once('data', resolve));
          process.stdin.pause();
          try {
            await staleRouteTypedClient.get_grid_infos(1, [0, 1]);
            throw new Error('stale-route IPC call unexpectedly succeeded');
          } catch (error) {
            const message = String(error);
            if (error?.name !== 'C2IpcRouteNotFoundError' || !message.includes('examples/grid')) {
              throw error;
            }
          }
          await staleRouteIpcTransport.close();
          staleRouteIpcTransport = undefined;
          assertEqual(requestWrites, 5, 'stale-route route-not-found IPC request SHM write count');
          assertEqual(requestConsumed, 5, 'stale-route route-not-found IPC request SHM consumed count');
          assertEqual(requestReleases, 0, 'stale-route route-not-found IPC request SHM local release count');
          assertEqual(responseReads, 11, 'stale-route route-not-found IPC must not read response SHM');
          assertEqual(responseReleases, 11, 'stale-route route-not-found IPC response SHM release count');
          assertEqual(allocatorCalls, 4, 'stale-route route-not-found IPC response allocator count');
        } finally {
          if (staleRouteIpcTransport !== undefined) {
            await staleRouteIpcTransport.close();
          }
          await ipcTransport.close();
          await noAllocatorIpcTransport.close();
          await noRequestShmIpcTransport.close();
          await noRequestShmAllocatorIpcTransport.close();
          await requestShmWriter.close();
          await responseShmReader.close();
        }
        """)
        real_ipc_source = real_ipc_source.replace('__ADDRESS__', json.dumps(address))
        (package_dir / 'real-ipc-smoke.mjs').write_text(real_ipc_source)
        process = subprocess.Popen(
            [node, str(package_dir / 'real-ipc-smoke.mjs')],
            cwd=package_dir,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        assert process.stdout is not None
        stdout_prefix = []
        ready = False
        while True:
            line = process.stdout.readline()
            if line == '':
                break
            stdout_prefix.append(line)
            if line.strip() == 'C2_STALE_ROUTE_READY':
                ready = True
                break
        if not ready:
            stdout_tail, stderr = process.communicate()
            raise AssertionError(
                'real IPC smoke exited before stale-route synchronization marker\n'
                f'returncode: {process.returncode}\n'
                f'stdout:\n{"".join(stdout_prefix)}{stdout_tail}\n'
                f'stderr:\n{stderr}',
            )
        try:
            cc.unregister('examples/grid')
        except Exception:
            process.kill()
            process.communicate()
            raise
        try:
            stdout_tail, stderr = process.communicate(input='\n', timeout=120)
        except subprocess.TimeoutExpired as exc:
            process.kill()
            stdout_tail, stderr = process.communicate()
            raise AssertionError(
                'real IPC smoke timed out after stale-route synchronization marker\n'
                f'stdout:\n{"".join(stdout_prefix)}{stdout_tail}\n'
                f'stderr:\n{stderr}',
            ) from exc
        stdout = ''.join(stdout_prefix) + stdout_tail
        if process.returncode != 0:
            raise subprocess.CalledProcessError(
                process.returncode,
                [node, str(package_dir / 'real-ipc-smoke.mjs')],
                output=stdout,
                stderr=stderr,
            )
    finally:
        settings.shm_threshold = previous_shm_threshold
        with suppress(Exception):
            cc.unregister('examples/grid')
        with suppress(Exception):
            cc.unregister('examples/grid-error')


@pytest.mark.timeout(180)
def test_grid_fastdb_c3_artifacts_cli_feeds_fastdb_codegen(tmp_path):
    root = Path(__file__).resolve().parents[4]
    pytest.importorskip('fastdb4py', reason='grid FastDB CLI smoke requires fastdb4py')
    cargo = shutil.which('cargo')
    if cargo is None:
        pytest.skip('grid FastDB CLI smoke requires cargo')

    contract_path = tmp_path / 'grid.contract.json'
    artifacts_path = tmp_path / 'grid.payload-abi-artifacts.json'
    generated_client_path = tmp_path / 'grid-client.ts'
    generated_path = tmp_path / 'grid-fastdb-codecs.ts'
    target = 'grid.grid_fdb_crm:GridFastdb'
    env = _grid_fastdb_subprocess_env(root)
    c3 = [
        cargo,
        'run',
        '--quiet',
        '--manifest-path',
        str(root / 'cli/Cargo.toml'),
        '--',
    ]

    subprocess.run(
        [
            *c3,
            'contract',
            'export',
            target,
            '--python',
            sys.executable,
            '--out',
            str(contract_path),
        ],
        check=True,
        cwd=root,
        env=env,
    )
    subprocess.run(
        [
            *c3,
            'contract',
            'artifacts',
            target,
            '--python',
            sys.executable,
            '--out',
            str(artifacts_path),
        ],
        check=True,
        cwd=root,
        env=env,
    )
    subprocess.run(
        [*c3, 'contract', 'validate', str(contract_path)],
        check=True,
        cwd=root,
        env=env,
    )
    subprocess.run(
        [
            *c3,
            'contract',
            'codegen',
            'typescript',
            str(contract_path),
            '--strict-codecs',
            '--fastdb-schema',
            str(artifacts_path),
            '--fastdb-out',
            str(generated_path),
            '--out',
            str(generated_client_path),
            '--python',
            sys.executable,
        ],
        check=True,
        cwd=root,
        env=env,
    )

    contract = json.loads(contract_path.read_text())
    artifacts = json.loads(artifacts_path.read_text())
    generated = generated_path.read_text()
    assert contract['schema'] == 'c-two.contract.v1'
    assert any(artifact.get('schema') == 'fastdb.call-db.schema.v1' for artifact in artifacts)
    assert any(
        artifact.get('schema') == 'fastdb.schema.v1'
        and artifact.get('feature', {}).get('name') == 'FastdbGridAttribute'
        for artifact in artifacts
    )
    assert any(
        artifact.get('schema') == 'fastdb.schema.v1'
        and artifact.get('feature', {}).get('name') == 'GridId'
        for artifact in artifacts
    )
    assert 'GET_GRID_INFOS_INPUT_CALL_DB_CODEC' in generated
    assert 'GET_ACTIVE_GRID_INFOS_OUTPUT_CALL_DB_CODEC' in generated
    assert 'python-pickle-default' not in generated


@pytest.mark.timeout(180)
def test_resource_infer_c3_artifacts_cli_feeds_fastdb_codegen(tmp_path):
    root = Path(__file__).resolve().parents[4]
    pytest.importorskip('fastdb4py', reason='resource infer FastDB CLI smoke requires fastdb4py')
    cargo = shutil.which('cargo')
    if cargo is None:
        pytest.skip('resource infer FastDB CLI smoke requires cargo')

    module_path = tmp_path / 'infer_fastdb_resource.py'
    module_path.write_text(textwrap.dedent(
        '''
        import fastdb4py as fdb

        @fdb.feature
        class Point:
            id: fdb.I32
            x: fdb.F64

        class Resource:
            def points(self, ids: fdb.Array[fdb.I32]) -> fdb.Batch[Point]:
                ...
        ''',
    ))
    diagnostics_path = tmp_path / 'inferred.diagnostics.json'
    contract_path = tmp_path / 'inferred.contract.json'
    artifacts_path = tmp_path / 'inferred.payload-abi-artifacts.json'
    generated_client_path = tmp_path / 'inferred-client.ts'
    generated_path = tmp_path / 'inferred-fastdb-codecs.ts'
    target = 'infer_fastdb_resource:Resource'
    env = _grid_fastdb_subprocess_env(root)
    env['PYTHONPATH'] = os.pathsep.join([str(tmp_path), env['PYTHONPATH']])
    c3 = [
        cargo,
        'run',
        '--quiet',
        '--manifest-path',
        str(root / 'cli/Cargo.toml'),
        '--',
    ]
    infer_base = [
        *c3,
        'contract',
        'infer',
        target,
        '--python',
        sys.executable,
        '--namespace',
        'test.inferred.fastdb',
        '--version',
        '0.1.0',
        '--name',
        'InferredFastdb',
        '--method',
        'points',
    ]

    subprocess.run(
        [*infer_base, '--diagnose', '--out', str(diagnostics_path)],
        check=True,
        cwd=root,
        env=env,
    )
    subprocess.run(
        [*infer_base, '--artifacts', '--out', str(artifacts_path)],
        check=True,
        cwd=root,
        env=env,
    )
    subprocess.run(
        [*infer_base, '--out', str(contract_path)],
        check=True,
        cwd=root,
        env=env,
    )
    subprocess.run(
        [*c3, 'contract', 'validate', str(contract_path)],
        check=True,
        cwd=root,
        env=env,
    )
    subprocess.run(
        [
            *c3,
            'contract',
            'codegen',
            'typescript',
            str(contract_path),
            '--strict-codecs',
            '--fastdb-schema',
            str(artifacts_path),
            '--fastdb-out',
            str(generated_path),
            '--out',
            str(generated_client_path),
            '--python',
            sys.executable,
        ],
        check=True,
        cwd=root,
        env=env,
    )

    diagnostics = json.loads(diagnostics_path.read_text())
    contract = json.loads(contract_path.read_text())
    artifacts = json.loads(artifacts_path.read_text())
    generated = generated_path.read_text()
    artifact_hashes = {
        artifact['schema']: set()
        for artifact in artifacts
        if isinstance(artifact, dict) and isinstance(artifact.get('schema'), str)
    }
    from fastdb4py.schema import schema_sha256
    for artifact in artifacts:
        artifact_hashes[artifact['schema']].add(schema_sha256(artifact))
    methods = {method['name']: method for method in contract['methods']}
    assert diagnostics == []
    assert contract['schema'] == 'c-two.contract.v1'
    assert contract['crm']['name'] == 'InferredFastdb'
    assert methods['points']['wire']['input']['schema_sha256'] in artifact_hashes['fastdb.call-db.schema.v1']
    assert methods['points']['wire']['output']['schema_sha256'] in artifact_hashes['fastdb.call-db.schema.v1']
    assert any(artifact.get('schema') == 'fastdb.call-db.schema.v1' for artifact in artifacts)
    assert any(
        artifact.get('schema') == 'fastdb.schema.v1'
        and artifact.get('feature', {}).get('name') == 'Point'
        for artifact in artifacts
    )
    assert 'POINTS_INPUT_CALL_DB_CODEC' in generated
    assert 'POINTS_OUTPUT_CALL_DB_CODEC' in generated
    assert 'python-pickle-default' not in contract_path.read_text()
    assert 'python-pickle-default' not in generated


def test_grid_fastdb_bridge_works_thread_local(monkeypatch):
    GridFastdb, GridId, NestedGrid, grid_fastdb_bridge = _load_fastdb_grid(monkeypatch)
    cc.register(
        GridFastdb,
        _make_grid_resource(NestedGrid),
        name='grid-fastdb-thread',
        bridge=grid_fastdb_bridge(),
    )

    grid = cc.connect(GridFastdb, name='grid-fastdb-thread')
    try:
        assert grid.client._mode == 'thread'  # noqa: SLF001
        _exercise_fastdb_grid(grid, GridId)
    finally:
        cc.close(grid)


def test_grid_fastdb_bridge_works_direct_ipc(monkeypatch):
    GridFastdb, GridId, NestedGrid, grid_fastdb_bridge = _load_fastdb_grid(monkeypatch)
    cc.register(
        GridFastdb,
        _make_grid_resource(NestedGrid),
        name='grid-fastdb-ipc',
        bridge=grid_fastdb_bridge(),
    )
    address = cc.server_address()
    assert address is not None

    grid = cc.connect(GridFastdb, name='grid-fastdb-ipc', address=address)
    try:
        assert grid.client._mode == 'ipc'  # noqa: SLF001
        _exercise_fastdb_grid(grid, GridId)
    finally:
        cc.close(grid)


def test_grid_fastdb_input_uses_view_mode_without_resource_input_hold(monkeypatch):
    GridFastdb, _GridId, NestedGrid, grid_fastdb_bridge = _load_fastdb_grid(monkeypatch)
    from c_two.crm.payload_plan import PayloadPlanKind

    input_binding = GridFastdb.get_grid_infos._input_payload_binding  # noqa: SLF001
    assert GridFastdb.get_grid_infos._input_buffer_mode == 'view'  # noqa: SLF001
    assert input_binding.kind is PayloadPlanKind.FDB
    assert input_binding.supports_retained_view is True

    cc.register(
        GridFastdb,
        _make_grid_resource(NestedGrid),
        name='grid-fastdb-input-view',
        bridge=grid_fastdb_bridge(),
    )
    address = cc.server_address()
    assert address is not None

    grid = cc.connect(GridFastdb, name='grid-fastdb-input-view', address=address)
    try:
        infos = grid.get_grid_infos(1, [0, 1])
        assert [info.global_id for info in infos] == [0, 1]
        stats = cc.hold_stats()
        assert stats['by_direction']['resource_input']['active_holds'] == 0
    finally:
        cc.close(grid)


def test_fastdb_input_defaults_to_materialized_python_values(monkeypatch):
    pytest.importorskip('fastdb4py', reason='fastdb input lifetime tests require fastdb4py')

    @cc.crm(namespace='demo.fastdb.input_lifetime', version='0.1.0')
    class IntArraySum:
        def total(self, values: fdb.Array[fdb.I32]) -> fdb.I32:
            ...

    class IntArraySumResource:
        def __init__(self):
            self.values = None

        def total(self, values):
            self.values = values
            return sum(values)

    resource = IntArraySumResource()
    cc.register(IntArraySum, resource, name='fastdb-materialized-input')
    address = cc.server_address()
    assert address is not None

    client = cc.connect(IntArraySum, name='fastdb-materialized-input', address=address)
    try:
        assert client.total([1, 2, 3]) == 6
        assert resource.values == [1, 2, 3]
        assert type(resource.values) is list
    finally:
        cc.close(client)


def test_fastdb_input_lifetime_borrowed_passes_view_then_releases(monkeypatch):
    pytest.importorskip('fastdb4py', reason='fastdb input lifetime tests require fastdb4py')

    @cc.crm(namespace='demo.fastdb.input_lifetime', version='0.1.0')
    class IntArraySum:
        def total(self, values: fdb.Array[fdb.I32]) -> fdb.I32:
            ...

    class IntArraySumResource:
        def __init__(self):
            self.values = None

        def total(self, values: fdb.Array[fdb.I32]) -> fdb.I32:
            self.values = values
            assert type(values).__name__ == 'FastdbCallArrayView'
            return sum(values)

    resource = IntArraySumResource()
    cc.register(
        IntArraySum,
        resource,
        name='fastdb-borrowed-input',
        input_lifetime={'total': cc.InputLifetime.BORROWED},
    )
    address = cc.server_address()
    assert address is not None

    client = cc.connect(IntArraySum, name='fastdb-borrowed-input', address=address)
    try:
        assert client.total([1, 2, 3]) == 6
        with pytest.raises(fdb.FdbViewInvalidatedError):
            len(resource.values)
        stats = cc.hold_stats()
        assert stats['by_direction']['resource_input']['active_holds'] == 0
    finally:
        cc.close(client)


def test_fastdb_borrowed_single_feature_input_invalidates_after_call(monkeypatch):
    import fastdb4py as fdb

    class Point:
        pass

    Point.__annotations__ = {'x': fdb.F64, 'y': fdb.F64}
    Point = fdb.feature(Point)

    class PointSum:
        def total(self, point):
            ...

    PointSum.total.__annotations__ = {'point': Point, 'return': fdb.F64}
    PointSum = cc.crm(namespace='demo.fastdb.input_lifetime.feature', version='0.1.0')(PointSum)

    class PointSumResource:
        def __init__(self):
            self.point = None

        def total(self, point):
            self.point = point
            return point.x + point.y

    PointSumResource.total.__annotations__ = {'point': Point, 'return': fdb.F64}

    resource = PointSumResource()
    cc.register(
        PointSum,
        resource,
        name='fastdb-borrowed-feature-input',
        input_lifetime={'total': cc.InputLifetime.BORROWED},
    )
    address = cc.server_address()
    assert address is not None

    client = cc.connect(PointSum, name='fastdb-borrowed-feature-input', address=address)
    try:
        assert client.total(Point(x=1.5, y=2.5)) == pytest.approx(4.0)
        with pytest.raises(fdb.FdbViewInvalidatedError):
            _ = resource.point.x
    finally:
        cc.close(client)


def test_fastdb_borrowed_input_can_be_retained_by_materializing(monkeypatch):
    import fastdb4py as fdb

    class Point:
        pass

    Point.__annotations__ = {'x': fdb.F64, 'y': fdb.F64}
    Point = fdb.feature(Point)

    class PointSum:
        def total(self, point):
            ...

    PointSum.total.__annotations__ = {'point': Point, 'return': fdb.F64}
    PointSum = cc.crm(namespace='demo.fastdb.input_lifetime.materialize', version='0.1.0')(PointSum)

    class PointSumResource:
        def __init__(self):
            self.point = None

        def total(self, point):
            self.point = fdb.materialize(point)
            return point.x + point.y

    PointSumResource.total.__annotations__ = {'point': Point, 'return': fdb.F64}

    resource = PointSumResource()
    cc.register(
        PointSum,
        resource,
        name='fastdb-borrowed-feature-materialized-input',
        input_lifetime={'total': cc.InputLifetime.BORROWED},
    )
    address = cc.server_address()
    assert address is not None

    client = cc.connect(PointSum, name='fastdb-borrowed-feature-materialized-input', address=address)
    try:
        assert client.total(Point(x=3.5, y=4.5)) == pytest.approx(8.0)
        assert resource.point.x == pytest.approx(3.5)
        assert resource.point.y == pytest.approx(4.5)
    finally:
        cc.close(client)


def test_borrowed_input_lifetime_rejects_python_pickle_fallback():
    @cc.crm(namespace='demo.fastdb.input_lifetime', version='0.1.0')
    class PythonListSum:
        def total(self, values: list[int]) -> int:
            ...

    class PythonListSumResource:
        def total(self, values: list[int]) -> int:
            return sum(values)

    with pytest.raises(ValueError, match='requires a buffer-view FDB input payload'):
        cc.register(
            PythonListSum,
            PythonListSumResource(),
            name='python-borrowed-input',
            input_lifetime={'total': cc.InputLifetime.BORROWED},
        )


def test_borrowed_input_lifetime_rejects_bridge_input(monkeypatch):
    pytest.importorskip('fastdb4py', reason='fastdb input lifetime tests require fastdb4py')

    @cc.crm(namespace='demo.fastdb.input_lifetime', version='0.1.0')
    class IntArraySum:
        def total(self, values: fdb.Array[fdb.I32]) -> fdb.I32:
            ...

    class IntArraySumResource:
        def total(self, values: fdb.Array[fdb.I32]) -> fdb.I32:
            return sum(values)

    with pytest.raises(ValueError, match='cannot be combined with bridge.input'):
        cc.register(
            IntArraySum,
            IntArraySumResource(),
            name='fastdb-borrowed-bridge-input',
            bridge={'total': cc.bridge(input=lambda values: (values,))},
            input_lifetime={'total': cc.InputLifetime.BORROWED},
        )


def test_borrowed_input_lifetime_rejects_missing_resource_annotation(monkeypatch):
    pytest.importorskip('fastdb4py', reason='fastdb input lifetime tests require fastdb4py')

    @cc.crm(namespace='demo.fastdb.input_lifetime', version='0.1.0')
    class IntArraySum:
        def total(self, values: fdb.Array[fdb.I32]) -> fdb.I32:
            ...

    class IntArraySumResource:
        def total(self, values):
            return sum(values)

    with pytest.raises(TypeError, match='requires resource parameter'):
        cc.register(
            IntArraySum,
            IntArraySumResource(),
            name='fastdb-borrowed-missing-resource-annotation',
            input_lifetime={'total': cc.InputLifetime.BORROWED},
        )


def test_borrowed_input_lifetime_rejects_extra_resource_parameter(monkeypatch):
    pytest.importorskip('fastdb4py', reason='fastdb input lifetime tests require fastdb4py')

    @cc.crm(namespace='demo.fastdb.input_lifetime.extra_param', version='0.1.0')
    class IntArraySum:
        def total(self, values: fdb.Array[fdb.I32]) -> fdb.I32:
            ...

    class IntArraySumResource:
        def total(self, values: fdb.Array[fdb.I32], scale: fdb.I32 = 1) -> fdb.I32:
            return sum(values) * scale

    with pytest.raises(TypeError, match='same positional parameters'):
        cc.register(
            IntArraySum,
            IntArraySumResource(),
            name='fastdb-borrowed-extra-resource-parameter',
            input_lifetime={'total': cc.InputLifetime.BORROWED},
        )


def test_grid_fastdb_hold_returns_retained_view_for_direct_ipc(monkeypatch):
    GridFastdb, _GridId, NestedGrid, grid_fastdb_bridge = _load_fastdb_grid(monkeypatch)
    cc.register(
        GridFastdb,
        _make_grid_resource(NestedGrid),
        name='grid-fastdb-hold',
        bridge=grid_fastdb_bridge(),
    )
    address = cc.server_address()
    assert address is not None

    grid = cc.connect(GridFastdb, name='grid-fastdb-hold', address=address)
    try:
        held = cc.hold(grid.get_active_grid_infos)()
        view = held.value
        row = view[0]
        level_column = view.column.level
        assert row.level == 1
        assert row.global_id == 0
        assert level_column[0] == 1
        owned = view.to_owned()
        assert [item.global_id for item in owned] == [0, 1, 2, 3]
        held.release()
        assert [item.global_id for item in owned] == [0, 1, 2, 3]
        import fastdb4py as fdb
        with pytest.raises(fdb.FdbViewInvalidatedError):
            _ = row.level
        with pytest.raises(fdb.FdbViewInvalidatedError):
            _ = level_column[0]
    finally:
        cc.close(grid)


def test_grid_fastdb_hold_returns_view_for_explicit_http_relay(monkeypatch, start_c3_relay):
    GridFastdb, _GridId, NestedGrid, grid_fastdb_bridge = _load_fastdb_grid(monkeypatch)
    relay = start_c3_relay()
    cc.set_relay_anchor(relay.url)
    cc.register(
        GridFastdb,
        _make_grid_resource(NestedGrid),
        name='grid-fastdb-hold-relay',
        bridge=grid_fastdb_bridge(),
    )

    grid = cc.connect(GridFastdb, name='grid-fastdb-hold-relay', address=relay.url)
    try:
        assert grid.client._mode == 'http'  # noqa: SLF001
        with cc.hold(grid.get_active_grid_infos)() as held:
            view = held.value
            row = view[0]
            assert row.level == 1
            assert row.global_id == 0
            columns = view.column
            level_column = columns.level
            assert level_column[0] == 1
            assert columns.global_id[0] == 0
        import fastdb4py as fdb
        with pytest.raises(fdb.FdbViewInvalidatedError):
            _ = row.level
        with pytest.raises(fdb.FdbViewInvalidatedError):
            _ = level_column[0]
    finally:
        cc.close(grid)


def test_grid_fastdb_bridge_rejects_mismatched_active_output_lengths(monkeypatch):
    _GridFastdb, _GridId, _NestedGrid, grid_fastdb_bridge = _load_fastdb_grid(monkeypatch)
    bridge = grid_fastdb_bridge()

    with pytest.raises(ValueError, match='same length'):
        bridge['get_active_grid_infos'].output(([1, 1], [0]))


def test_grid_fastdb_bridge_works_explicit_http_relay(monkeypatch, start_c3_relay):
    GridFastdb, GridId, NestedGrid, grid_fastdb_bridge = _load_fastdb_grid(monkeypatch)
    relay = start_c3_relay()
    cc.set_relay_anchor(relay.url)
    cc.register(
        GridFastdb,
        _make_grid_resource(NestedGrid),
        name='grid-fastdb-relay',
        bridge=grid_fastdb_bridge(),
    )

    grid = cc.connect(GridFastdb, name='grid-fastdb-relay', address=relay.url)
    try:
        assert grid.client._mode == 'http'  # noqa: SLF001
        _exercise_fastdb_grid(grid, GridId)
    finally:
        cc.close(grid)


def test_fastdb_iterator_array_input_bridge_across_transports(monkeypatch, start_c3_relay):
    fdb, derive_c_two_bridge = _load_fastdb_bridge_helpers()

    IteratorArrayCRM, thread_resource = _make_iterator_array_crm_and_resource(fdb)
    cc.register(
        IteratorArrayCRM,
        thread_resource,
        name='iterator-array-thread',
        bridge=derive_c_two_bridge(cc, IteratorArrayCRM, thread_resource),
    )
    thread_client = cc.connect(IteratorArrayCRM, name='iterator-array-thread')
    try:
        assert thread_client.values([1, 2]) == [11, 12]
        assert thread_resource.seen_inputs == [[1, 2]]
    finally:
        cc.close(thread_client)

    IteratorArrayCRM, ipc_resource = _make_iterator_array_crm_and_resource(fdb)
    cc.register(
        IteratorArrayCRM,
        ipc_resource,
        name='iterator-array-ipc',
        bridge=derive_c_two_bridge(cc, IteratorArrayCRM, ipc_resource),
    )
    address = cc.server_address()
    assert address is not None
    ipc_client = cc.connect(IteratorArrayCRM, name='iterator-array-ipc', address=address)
    try:
        assert ipc_client.values([3, 4]) == [13, 14]
        assert ipc_resource.seen_inputs == [[3, 4]]
    finally:
        cc.close(ipc_client)

    IteratorArrayCRM, relay_resource = _make_iterator_array_crm_and_resource(fdb)
    relay = start_c3_relay()
    cc.set_relay_anchor(relay.url)
    cc.register(
        IteratorArrayCRM,
        relay_resource,
        name='iterator-array-relay',
        bridge=derive_c_two_bridge(cc, IteratorArrayCRM, relay_resource),
    )
    relay_client = cc.connect(IteratorArrayCRM, name='iterator-array-relay', address=relay.url)
    try:
        assert relay_client.values([5, 6]) == [15, 16]
        assert relay_resource.seen_inputs == [[5, 6]]
    finally:
        cc.close(relay_client)
