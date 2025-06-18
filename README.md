# C-Two (0.1.21)

C-Two is a **type-safe Remote Procedure Call (RPC) framework** designed for distributed resource computation systems. The framework provides a structured abstraction layer that enables remote method invocation between client components and Core Resource Models (CRM) with automatic serialization and protocol-agnostic communication.

## Framework Overview

C-Two addresses the complexity of distributed computing by implementing a clear separation of concerns through interface-based programming and automatic type marshalling. The framework enables developers to write distributed applications using familiar local programming patterns while maintaining type safety across network boundaries.

### Core Architectural Principles

- **Interface Segregation**: Clean separation between interface definitions (ICRM) and implementations (CRM)
- **Type Safety**: Compile-time and runtime type checking with automatic serialization inference
- **Protocol Abstraction**: Transport-agnostic communication supporting multiple protocols (ZMQ, MCP)
- **Resource Isolation**: Encapsulation of computational resources behind well-defined interfaces

## Architecture

The framework implements a three-tier architecture:

<img src="https://raw.githubusercontent.com/world-in-progress/c-two/main/doc/images/architecture.png" alt="Architecture" width="1500" />

### Component Layer
Client-side computational units that consume remote resources through Interface of Core Resource Models (ICRM), providing network transparency for distributed operations.

### CRM Layer
Server-side Core Resource Models implementing domain-specific business logic and resource management, exposed through standardized ICRM interfaces.

### Transport Layer
Protocol-agnostic communication infrastructure supporting multiple transport mechanisms with connection management and message serialization.

## Implementation Guide

### 1. Interface Definition (ICRM)

Define remote interfaces using the `@icrm` decorator to establish contracts for distributed communication:

```python
import c_two as cc

@cc.icrm
class IGrid:
    def get_grid_infos(self, level: int, global_ids: list[int]) -> list[GridAttribute]:
        """Retrieve grid information for specified level and identifiers"""
        ...
    
    def subdivide_grids(self, levels: list[int], global_ids: list[int]) -> list[str]:
        """Perform grid subdivision operations"""
        ...
```

### 2. Resource Model Implementation (CRM)

Implement the Core Resource Model using the `@iicrm` decorator:

```python
@cc.iicrm
class Grid(IGrid):
    def __init__(self, epsg: int, bounds: list, first_size: list[float], subdivide_rules: list[list[int]]):
        self.epsg = epsg
        self.bounds = bounds
        # Resource initialization
    
    def get_grid_infos(self, level: int, global_ids: list[int]) -> list[GridAttribute]:
        # Business logic implementation with automatic serialization
        return [GridAttribute(level=level, global_id=gid, ...) for gid in global_ids]
    
    def subdivide_grids(self, levels: list[int], global_ids: list[int]) -> list[str]:
        # Subdivision algorithm implementation
        return [f"{level+1}-{child_id}" for level, gid in zip(levels, global_ids) for child_id in children]
```

### 3. Custom Data Type Definition

Define serializable data structures using the `@transferable` decorator with any necessary serialization logic:

```python
import pyarrow as pa

@cc.transferable
class GridAttribute:
    level: int
    global_id: int
    elevation: float
    
    def serialize(data: 'GridAttribute') -> bytes:
        schema = pa.schema([
            pa.field('level', pa.int32()),
            pa.field('global_id', pa.int32()),
            pa.field('elevation', pa.float64())
        ])
        table = pa.Table.from_pylist([data.__dict__], schema=schema)
        return serialize_from_table(table)
    
    def deserialize(arrow_bytes: bytes) -> 'GridAttribute':
        row = deserialize_to_rows(arrow_bytes)[0]
        return GridAttribute(**row)

@cc.transferable
class GridAttributes:
    """
    A collection of GridAttribute objects with built-in serialization capabilities.
    
    The C-Two framework automatically handles serialization/deserialization when this type
    is detected as a parameter or return type in CRM methods (e.g., the return type of
    Grid.get_grid_infos()).
    """
    def serialize(data: list[GridAttribute]) -> bytes:
        schema = pa.schema([
            pa.field('attribute_bytes', pa.list_(pa.binary())),
        ])

        data_dict = {
            'attribute_bytes': [GridAttribute.serialize(grid) for grid in data]
        }
        
        table = pa.Table.from_pylist([data_dict], schema=schema)
        return serialize_from_table(table)

    def deserialize(arrow_bytes: bytes) -> list[GridAttribute]:
        table = deserialize_to_table(arrow_bytes)
        
        grid_bytes = table.column('attribute_bytes').to_pylist()[0]
        
        return [GridAttribute.deserialize(grid_byte) for grid_byte in grid_bytes]

# Helpers ##################################################

def serialize_from_table(table: pa.Table) -> bytes:
    sink = pa.BufferOutputStream()
    with pa.ipc.new_stream(sink, table.schema) as writer:
        writer.write_table(table)
    binary_data = sink.getvalue().to_pybytes()
    return binary_data

def deserialize_to_rows(serialized_data: bytes) -> dict:
    buffer = pa.py_buffer(serialized_data)

    with pa.ipc.open_stream(buffer) as reader:
        table = reader.read_all()

    return table.to_pylist()
```

### 4. Server Deployment

Deploy the CRM as a networked service:

```python
# Resource initialization and server configuration
grid = Grid(epsg=2326, bounds=[...], first_size=[64.0, 64.0], subdivide_rules=[...])
server = cc.message.Server("tcp://localhost:5555", grid)
server.start()
server.wait_for_termination()
```

### 5. Client Implementation

#### Script-Based Component Approach
```python
with cc.compo.runtime.connect_crm('tcp://localhost:5555', IGrid) as grid:
    infos = grid.get_grid_infos(1, [0, 1, 2])
    keys = grid.subdivide_grids([1, 1], [0, 1])
    print(f'Retrieved {len(infos)} grid attributes, generated {len(keys)} subdivisions')
```

#### Function-Based Component Approach
```python
@cc.compo.runtime.connect
def process_grids(grid: IGrid, target_level: int) -> list[str]:
    """Reusable component for grid processing operations"""
    infos = grid.get_grid_infos(target_level, [0, 1, 2, 3, 4])
    candidates = [info.global_id for info in infos if hasattr(info, 'elevation') and info.elevation > 0]
    return grid.subdivide_grids([target_level] * len(candidates), candidates)

# Execution with automatic connection injection
result = process_grids(1, crm_address='tcp://localhost:5555')

# Or using a context manager
with cc.compo.runtime.connect_crm('tcp://localhost:5555'):
    result = process_grids(1)
```

### 6. External System Integration

Integrate with external systems using the Model Context Protocol (MCP):

```python
from mcp.server.fastmcp import FastMCP
import compo  # Component module

mcp = FastMCP('GridAgent', instructions=cc.mcp.CC_INSTRUCTION)
cc.mcp.register_mcp_tools_from_compo_module(mcp, compo)

if __name__ == '__main__':
    mcp.run()
```

## Application Domains

C-Two is particularly suited for:

- **Distributed Scientific Computing**: High-performance computing applications requiring resource distribution
- **Microservices Architecture**: Type-safe inter-service communication in distributed systems
- **Computational Resource Management**: Systems requiring dynamic resource allocation and computation distribution
- **System Integration**: Applications requiring protocol-agnostic communication with external systems
- **Modular Component Systems**: Reusable computational components across different resource implementations

## Framework Components

### Core Decorators
- **`@icrm`**: Interface definition for remote resource specifications
- **`@iicrm`**: Implementation marker for Core Resource Models
- **`@transferable`**: Custom serializable data type definition
- **`@auto_transfer`**: Automatic serialization based on type annotations
- **`@connect`**: Connection injection for component functions

### Runtime Components
- **`connect_crm`**: Context manager for CRM connection lifecycle management
- **`Server`**: CRM service deployment infrastructure
- **`Client`**: Remote resource access client

### Integration Utilities
- **MCP Tools**: Model Context Protocol integration for external system communication

## Technical Requirements

- Python ≥ 3.10
- Type annotation support
- Network connectivity for distributed deployment

---

*C-Two provides a principled approach to distributed computing through type-safe abstractions and protocol-agnostic communication, enabling developers to build robust distributed systems with familiar programming patterns.*