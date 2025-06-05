# C-Two (0.1.20)

<p align="center">
<img align="center" width="150px" src="https://raw.githubusercontent.com/world-in-progress/c-two/main/doc/images/logo.png">
</p>

C-Two (CC) is a **type-safe [RPC](https://en.wikipedia.org/wiki/Remote_procedure_call) framework** for distributed resource computation. It provides an abstraction layer between client components and remote Core Resource Models (CRM), enabling remote procedure calls with automatic serialization and multi-protocol support.

## Key Features

- **🔧 Decorator-Based Interface Definition**: Define and implement remote interfaces using `@icrm` and `@iicrm` decorators
- **🔒 Type Safety**: Type annotation support with automatic type checking and inference
- **⚡ Serialization**: Automatic serialization based on function signatures and type hints
- **🔄 Proxy Pattern**: Call remote methods through local proxy interfaces
- **📡 Connection Management**: Connection handling with context management support

## Architecture

The framework implements a three-layer architecture that separates concerns between client components, resource models, and transport protocols:

<img src="https://raw.githubusercontent.com/world-in-progress/c-two/main/doc/images/architecture.png" alt="Architecture" width="1500" />

### Component Layer
Client-side components that consume remote resources through ICRMs (Interface of Core Resource Models), providing abstraction over network communication.

### CRM Layer
Core Resource Models implementing business logic and resource management, exposed through standardized interfaces (ICRM).

### Transport Layer
Protocol-agnostic communication layer supporting multiple transport mechanisms (ZMQ, MCP) with connection pooling.

## Quick Start

### 1. Define an Interface (ICRM)

```python
import c_two as cc

@cc.icrm
class IGrid:
    def get_grid_infos(self, level: int, global_ids: list[int]) -> list[GridAttribute]:
        """Get grid information from remote resource"""
        ...
    
    def subdivide_grids(self, levels: list[int], global_ids: list[int]) -> list[str]:
        """Subdivide grids and return child keys"""
        ...
```

### 2. Implement the Resource Model (CRM)

```python
@cc.iicrm
class Grid(IGrid):
    def __init__(self, epsg: int, bounds: list, first_size: list[float], subdivide_rules: list[list[int]]):
        self.epsg = epsg
        self.bounds = bounds
        # initialization logic
    
    def get_grid_infos(self, level: int, global_ids: list[int]) -> list[GridAttribute]:
        # Implementation with automatic serialization
        return [GridAttribute(level=level, global_id=gid, ...) for gid in global_ids]
    
    def subdivide_grids(self, levels: list[int], global_ids: list[int]) -> list[str]:
        # Implementation logic
        return [f"{level+1}-{child_id}" for level, gid in zip(levels, global_ids) for child_id in children]
```

### 3. Define Custom Transferable Types

```python
@cc.transferable
class GridAttribute:
    level: int
    global_id: int
    elevation: float
    
    def serialize(data: 'GridAttribute') -> bytes:
        # Arrow-based serialization
        schema = pa.schema([...])
        table = pa.Table.from_pylist([data.__dict__], schema=schema)
        return cc.message.serialize_from_table(table)
    
    def deserialize(arrow_bytes: bytes) -> 'GridAttribute':
        row = cc.message.deserialize_to_rows(arrow_bytes)[0]
        return GridAttribute(**row)
```

### 4. Start the CRM Server

```python
# Server side
grid = Grid(epsg=2326, bounds=[...], first_size=[64.0, 64.0], subdivide_rules=[...])
server = cc.message.Server("tcp://localhost:5555", grid)
server.start()
server.wait_for_termination()
```

### 5. Client Usage

```python
# Script-style component usage
with cc.compo.runtime.connect_crm('tcp://localhost:5555', IGrid) as grid:
    infos = grid.get_grid_infos(1, [0, 1, 2])  # Remote call
    keys = grid.subdivide_grids([1, 1], [0, 1])
    print(f'Received {len(infos)} grid attributes, created {len(keys)} children)

# Function-style component definition
@cc.compo.runtime.connect
def process_grids(grid: IGrid, target_level: int) -> list[str]:
    """Reusable component that works with any IGrid implementation"""
    active_levels, active_ids = grid.get_active_grid_infos()
    grids_to_subdivide = [gid for level, gid in zip(active_levels, active_ids) if level == target_level]
    return grid.subdivide_grids([target_level] * len(grids_to_subdivide), grids_to_subdivide)

# Function-style component usage
result = process_grids(target_level=1, crm_address='tcp://localhost:5555')
# Or
with cc.compo.runtime.connect_crm('tcp://localhost:5555'):
    result = process_grids(grid, target_level=1)
    print(f'Subdivided grids: {result}')
```

### 6. MCP Integration

```python
# MCP server for external system integration
from mcp.server.fastmcp import FastMCP
import compo  # Your function-style component module

mcp = FastMCP('GridAgent', instructions=cc.mcp.CC_INSTRUCTION)
cc.mcp.register_mcp_tools_from_compo_module(mcp, compo)

if __name__ == '__main__':
    mcp.run()  # Exposes operations via MCP protocol
```

## Use Cases

C-Two is suitable for:

- **Distributed Computing**: Resource computations across multiple machines
- **Microservices Architecture**: Type-safe inter-service communication
- **Scientific Computing**: Data processing with custom transferable types
- **System Integration**: MCP protocol support for external system access
- **Component Reusability**: Reusable components across different CRM implementations

## Core Components

- **`@icrm`**: Define remote interface specifications
- **`@iicrm`**: Implement CRM classes with automatic method decoration
- **`@transferable`**: Create custom serializable data types
- **`@auto_transfer`**: Automatic serialization based on type hints
- **`@connect`**: Inject CRM connections into component functions
- **`connect_crm`**: Context manager for CRM connections
- **MCP Tools**: External system integration utilities

---

*C-Two provides a structured approach to distributed computing by abstracting remote procedure calls into familiar local function interfaces.*