# C-Two (0.1.19)

<p align="center">
<img align="center" width="150px" src="https://raw.githubusercontent.com/world-in-progress/c-two/main/doc/images/logo.png">
</p>

C-Two (CC) is a **type-safe [RPC](https://en.wikipedia.org/wiki/Remote_procedure_call) framework** designed for distributed resource computation. It provides a seamless abstraction layer between client components and remote Core Resource Models (CRM), enabling transparent remote procedure calls with automatic serialization and multi-protocol support.

## Key Features

- **🔧 Decorator-Driven Interface**: Define and implement remote interfaces using `@icrm` and `@iicrm` decorators
- **🔒 Type Safety**: Full type annotation support with automatic type checking and inference
- **⚡ Auto-Transfer**: Intelligent serialization based on function signatures and type hints
- **🔄 Transparent Proxying**: Call remote methods as if they were local functions
- **📡 Connection Management**: Flexible connection handling with context management

## Architecture

The framework implements a three-layer architecture separating concerns between client components, resource models, and transport protocols:

<img src="https://raw.githubusercontent.com/world-in-progress/c-two/main/doc/images/architecture.png" alt="Architecture" width="1500" />

### Component Layer
Client-side components that consume remote resources through ICRM interfaces, providing a clean abstraction over network complexity.

### CRM Layer
Core Resource Models implementing the actual business logic and resource management, exposed through standardized interfaces.

### Transport Layer
Protocol-agnostic communication layer supporting multiple transport mechanisms (ZMQ, gRPC, MCP) with automatic load balancing and connection pooling.

## Quick Start

### 1. Define an Interface

```python
import c_two as cc

@cc.icrm
class IGrid:
    def get_grid_infos(self) -> list[GridAttribute]:
        """Get grid information from remote resource"""
        ...
```

### 2. Implement the Resource Model

```python
@cc.iicrm
class Grid(IGrid):
    @cc.auto_transfer
    def get_grid_infos(self) -> list[GridAttribute]:
        # Actual implementation with automatic serialization
        return [GridAttribute(id=1, name="grid1")]
```

### 3. Connect and Use

```python
# Client side - transparent remote calls
with cc.connect_crm("tcp://localhost:5555", IGrid) as grid:
    infos = grid.get_grid_infos()  # Seamless remote call
    print(f"Received {len(infos)} grid attributes")
```

## Use Cases

C-Two is particularly well-suited for:

- **Distributed Computing**: Resource-intensive computations across multiple machines
- **Microservices Architecture**: Type-safe inter-service communication
- **Scientific Computing**: High-performance data processing
- **Real-time Systems**: Low-latency remote procedure calls with connection pooling

---

*C-Two bridges the gap between local and remote computing, making distributed systems feel as natural as local function calls.*