from __future__ import annotations

import numpy as np
import pytest

import c_two as cc
import fastdb4py as fdb
from c_two.transport.registry import _ProcessRegistry


@fdb.feature
class Vertex:
    vertex_id: fdb.U32
    x: fdb.F64
    y: fdb.F64
    z: fdb.F64


@fdb.feature
class Node:
    node_id: fdb.U32
    weight: fdb.F64
    anchor: Vertex
    neighbors: list[Vertex]


@cc.crm(namespace='test.fastdb-object-graph', version='0.1.0')
class Mesh:
    @cc.read
    def vertices(self) -> fdb.Batch[Vertex]:
        ...

    @cc.read
    def nodes(self) -> fdb.Batch[Node]:
        ...


class MeshResource:
    def vertices(self) -> fdb.Batch[Vertex]:
        batch = fdb.require(fdb.batch(Vertex, rows=3))
        idx = np.arange(3, dtype=np.uint32)
        batch.fill(
            vertex_id=idx,
            x=idx.astype(np.float64),
            y=idx.astype(np.float64) + 10.0,
            z=idx.astype(np.float64) + 20.0,
        )
        return batch

    def nodes(self) -> fdb.Batch[Node]:
        vertices = [
            Vertex(vertex_id=0, x=0.0, y=10.0, z=20.0),
            Vertex(vertex_id=1, x=1.0, y=11.0, z=21.0),
            Vertex(vertex_id=2, x=2.0, y=12.0, z=22.0),
        ]
        batch = fdb.Batch.allocate(Node, 0)
        batch.append(Node(
            node_id=100,
            weight=0.5,
            anchor=vertices[0],
            neighbors=[vertices[1], vertices[2]],
        ))
        batch.append(Node(
            node_id=200,
            weight=1.5,
            anchor=vertices[2],
            neighbors=[vertices[0]],
        ))
        return batch


@pytest.fixture(autouse=True)
def _cleanup_runtime():
    try:
        yield
    finally:
        cc.shutdown()
        _ProcessRegistry._instance = None


@pytest.mark.timeout(30)
def test_fastdb_vertex_columnar_and_node_object_graph_roundtrip_direct_ipc():
    cc.register(Mesh, MeshResource(), name='object-graph-mesh')
    address = cc.server_address()
    assert address is not None

    proxy = cc.connect(Mesh, name='object-graph-mesh', address=address)
    try:
        vertices = proxy.vertices()
        assert isinstance(vertices, fdb.Batch)
        assert list(vertices.column.vertex_id) == [0, 1, 2]
        assert list(vertices.column.z) == [20.0, 21.0, 22.0]

        nodes = proxy.nodes()
        assert isinstance(nodes, list)
        assert [node.node_id for node in nodes] == [100, 200]
        assert [node.anchor.vertex_id for node in nodes] == [0, 2]
        assert [
            [neighbor.vertex_id for neighbor in node.neighbors]
            for node in nodes
        ] == [[1, 2], [0]]

        with cc.hold(proxy.nodes)() as held:
            held_nodes = held.value
            assert [node.node_id for node in held_nodes] == [100, 200]
            with pytest.raises(RuntimeError, match='no retained buffer'):
                _ = held.unsafe_buffer
    finally:
        cc.close(proxy)


def test_fastdb_object_graph_borrowed_input_is_rejected():
    with pytest.raises(ValueError, match='buffer-view FDB input payload'):
        cc.register(
            Mesh,
            MeshResource(),
            name='object-graph-borrowed',
            input_lifetime={'nodes': cc.InputLifetime.BORROWED},
        )
