# C-Two (Greenhouse v0.2.5)

[![PyPI version](https://badge.fury.io/py/c-two.svg)](https://badge.fury.io/py/c-two)

<p align="center">
<img align="center" width="150px" src="https://raw.githubusercontent.com/world-in-progress/c-two/main/doc/images/logo.png">
</p> 

C-Two is a **type-safe Remote Procedure Call (RPC) framework**, making it possible for resource-oriented classes to be remotely invoked across different processes or machines. It is specifically suited for distributed resource computation systems. The framework provides a structured abstraction layer that enables remote method calling between **Components** and **Core Resource Models (CRMs)** with automatic serialization and protocol-agnostic communication.

## Framework Overview

**The design philosophy of C-Two is not to define services, but to empower resources.**

In common scientific computation, resources encapsulating complex state and domain-specific operations need to be highly cohesive into specific units or interaction models. We call these resource-oriented abstractions as **Core Resource Models (CRMs)**. From the perspective of business logic, projects are not interested in where the resources are located and where they come from, but rather in how to interact with them and implement their high-level logic. We call these resource consumers as **Components**. C-Two can provide CRM location transparency and status source transparency, so that components can interact with CRMs as if they were local objects. This approach is particularly valuable for geographic information systems, scientific computing, and other domains requiring persistent resource management with distributed access patterns.

C-Two addresses the complexity of distributed computing by implementing a clear separation of concerns through interface-based programming and automatic type marshalling. The framework enables developers to write distributed applications using familiar local programming patterns while maintaining type safety across network boundaries.

### Core Architectural Principles

- **Resource-Oriented Design**: Focus on stateful computational resources rather than stateless function calls
- **Interface Segregation**: Clean separation between interface definitions (ICRM) and implementations (CRM)
- **Type Safety**: Compile-time and runtime type checking with automatic or customized serialization inference
- **Protocol Abstraction**: Transport-agnostic communication supporting multiple protocols (Thread, Memory, IPC, TCP, HTTP)

## Architecture

The framework implements a three-tier architecture:

<img src="https://raw.githubusercontent.com/world-in-progress/c-two/main/doc/images/architecture.png" alt="Architecture" width="1500" />

### Component Layer
Client-side computational units that consume remote resources through Interface of Core Resource Models (ICRM), providing network transparency for distributed operations.

### CRM Layer
Server-side CRMs maintaining persistent state and implementing domain-specific, low-level resource management, exposed through standardized ICRM interfaces.

### Transport Layer
Protocol-agnostic communication infrastructure supporting multiple transport mechanisms (Thread, Memory, IPC, TCP, HTTP) with automatic protocol detection, connection management and message serialization. The framework automatically selects the appropriate transport implementation based on the address scheme.

## Technical Requirements

- Python ≥ 3.10
- Type annotation support
- Network connectivity for distributed deployment (TCP, HTTP protocols)

## Installation

### PyPI Installation

Install the latest stable version from PyPI using pip:

```bash
pip install c-two
```

### Package Management with uv

For better dependency resolution, use [uv](https://github.com/astral-sh/uv):

```bash
# Add c-two to your project
uv add c-two
```

### Development Installation with uv

For development, testing, or accessing the latest features:

```bash
# Clone the repository
git clone https://github.com/world-in-progress/c-two.git

# Navigate to the project directory
cd c-two

# Install development dependencies
uv sync
```

## Implementation Guide

### 1. Resource Model Implementation (CRM)

Implement the CRM, which is just a regular Python class that can maintain resource state and implement domain-specific logic.
The only difference is that the CRM class must have a member function `terminate()` to handle resource cleanup when the CRM is released.

```python
from dataclasses import dataclass

@dataclass
class GridAttribute:
    level: int
    global_id: int
    elevation: float

# Define CRM ###########################################################

class Grid:
    def __init__(self, epsg: int, bounds: list[float], first_size: list[float], subdivide_rules: list[list[int]]):
        # Resource initialization
        pass
    
    def get_grid_infos(self, level: int, global_ids: list[int]) -> list[GridAttribute]:
        # Logic implementation of retrieving grid information
        pass
    
    def subdivide_grids(self, levels: list[int], global_ids: list[int]) -> list[str]:
        # Logic implementation of subdivision algorithm
        pass
    
    def delete_grids(self, levels: list[int], global_ids: list[int]) -> None:
        # Logic implementation of delete operation
        pass

    def terminate(self):
        # Logic implementation of resource cleanup
        pass

```

### 2. Interface Definition (ICRM)

Define remote interfaces using the `@icrm` decorator to establish contracts for distributed communication.
There is no need to demonstrate all methods of the CRM, only those that need to be exposed remotely.

```python
import c_two as cc
from dataclasses import dataclass

@dataclass
class GridAttribute:
    level: int
    global_id: int
    elevation: float

# Define ICRM ##############################################

@cc.icrm(namespace='cc.demo', version='0.1.0')
class IGrid:
    def get_grid_infos(self, level: int, global_ids: list[int]) -> list[GridAttribute]:
        """Retrieve grid information for specified level and identifiers"""
        ...
    
    def subdivide_grids(self, levels: list[int], global_ids: list[int]) -> list[str]:
        """Perform grid subdivision operations"""
        ...
```

### 3. Custom Data Type Definition (Optional)

Define serializable data structures using the `@transferable` decorator with any necessary serialization logic.
In this example, we modify the `GridAttribute` and create `GridAttributes` in ICRM file, to use Apache Arrow for efficient serialization.

```python
import c_two as cc
import pyarrow as pa

# @transferable will decorate the class as data class automatically
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
    
    The C-Two framework automatically handles serialization / deserialization
    when this type is detected as a parameter or return type in CRM methods
    (e.g., the return type of Grid.get_grid_infos()).
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

# Define ICRM ##############################################

@cc.icrm(namespace='cc.demo', version='0.1.0')
class IGrid:
    def get_grid_infos(self, level: int, global_ids: list[int]) -> list[GridAttribute]:
        """Retrieve grid information for specified level and identifiers"""
        ...
    
    def subdivide_grids(self, levels: list[int], global_ids: list[int]) -> list[str]:
        """Perform grid subdivision operations"""
        ...

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

Deploy the CRM as a networked service with automatic protocol detection.
Services are only accessible through the defined ICRM interfaces.

```python
import c_two as cc
from crm import Grid
from icrm import IGrid

# Resource initialization and server configuration
grid = Grid(epsg=2326, bounds=[...], first_size=[64.0, 64.0], subdivide_rules=[...])

# Thread server (in-process, thread-safe communication)
server = cc.rpc.Server("thread://grid_processor", IGrid, grid)

# Memory server (shared memory, high-performance local communication)
server = cc.rpc.Server("memory://grid_processor", IGrid, grid)

# IPC server (inter-process communication for Unix-like systems, local machine)
server = cc.rpc.Server("ipc:///tmp/grid_processor", IGrid, grid)

# TCP server (network-based, cross-machine)
server = cc.rpc.Server("tcp://localhost:5555", IGrid, grid)

# HTTP server (web-compatible, cross-platform)
server = cc.rpc.Server("http://localhost:8000", IGrid, grid)

# Same server lifecycle management regardless of protocol
server.start()
print('CRM server is running. Press Ctrl+C to stop.')
try:
    # Wait for server termination with a timeout or not
    server.wait_for_termination(timeout=10)
    print('Timeout reached, stopping CRM server...')
except KeyboardInterrupt:
    print('Keyboard interrupt received, stopping CRM server...')
finally:
    server.stop()
    print('CRM server stopped.')
```

### 5. Client Implementation

The framework provides seamless protocol-transparent connectivity to CRM services.

#### Script-Based Component Approach
```python
# Thread connection
with cc.compo.runtime.connect_crm('thread://grid_processor', IGrid) as grid:
    infos = grid.get_grid_infos(1, [0, 1, 2])
    keys = grid.subdivide_grids([1, 1], [0, 1])

# Memory connection
with cc.compo.runtime.connect_crm('memory://grid_processor', IGrid) as grid:
    infos = grid.get_grid_infos(1, [0, 1, 2])
    keys = grid.subdivide_grids([1, 1], [0, 1])

# IPC connection
with cc.compo.runtime.connect_crm('ipc:///tmp/grid_processor', IGrid) as grid:
    infos = grid.get_grid_infos(1, [0, 1, 2])
    keys = grid.subdivide_grids([1, 1], [0, 1])

# TCP connection
with cc.compo.runtime.connect_crm('tcp://localhost:5555', IGrid) as grid:
    infos = grid.get_grid_infos(1, [0, 1, 2])
    keys = grid.subdivide_grids([1, 1], [0, 1])

# HTTP connection
with cc.compo.runtime.connect_crm('http://localhost:8000', IGrid) as grid:
    infos = grid.get_grid_infos(1, [0, 1, 2])
    keys = grid.subdivide_grids([1, 1], [0, 1])

print(f'Retrieved {len(infos)} grid attributes, generated {len(keys)} subdivisions')
```

#### Function-Based Component Approach
```python
@cc.runtime.connect
def process_grids(grid: IGrid, target_level: int) -> list[str]:
    """Reusable component for grid processing operations"""
    infos = grid.get_grid_infos(target_level, [0, 1, 2, 3, 4])
    candidates = [info.global_id for info in infos if hasattr(info, 'elevation') and info.elevation > 0]
    return grid.subdivide_grids([target_level] * len(candidates), candidates)

# Protocol is determined automatically by the address
result = process_grids(1, crm_address='thread://grid_processor')      # use Thread
result = process_grids(1, crm_address='memory://grid_processor')      # use Memory
result = process_grids(1, crm_address='ipc:///tmp/grid_processor')    # use IPC
result = process_grids(1, crm_address='tcp://localhost:5555')         # use TCP
result = process_grids(1, crm_address='http://localhost:8000')        # use HTTP

# Or using a context manager with automatic protocol detection
with cc.compo.connect_crm('thread://grid_processor'):
    result = process_grids(1)
```

### 6. External System Integration

Integrate with external systems using the Model Context Protocol (MCP):

```python
from mcp.server.fastmcp import FastMCP
import compo  # module containing function-based components

mcp = FastMCP('GridAgent', instructions=cc.mcp.CC_INSTRUCTION)
cc.mcp.register_mcp_tools_from_compo_module(mcp, compo)

if __name__ == '__main__':
    mcp.run()
```