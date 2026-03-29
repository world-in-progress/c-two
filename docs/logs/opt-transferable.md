# IPC v3 @transferable 序列化层性能优化计划

## Context

IPC v3 传输层已高度优化（buddy allocator + SHM + warm buffer），bytes 快速路径达到 37.4 GB/s。但使用 `@transferable` 自定义类型时，序列化层成为最大瓶颈：

- **bytes 快速路径**: 1GB echo = 51ms（2 次拷贝）
- **@transferable 路径**: 1GB echo = 891ms（9 次拷贝）
- **差距: 17.4 倍**

根因：一次 echo round-trip 产生 **9 次完整数据拷贝**，其中 6 次可消除。

---

## 拷贝预算分析

| # | 位置 | 可消除? | 消除方式 |
|---|------|---------|---------|
| 1 | Client serialize: `header + blob` 拼接 | ✅ | P0 scatter-write |
| 2 | Client `write_call_into` → SHM | ❌ | 基础传输拷贝 |
| 3 | Server deserialize: `bytes(raw)` | ✅ | P1 memoryview-aware |
| 4 | Server deserialize: `raw[16:]` slice | ✅ | P1 memoryview slicing |
| 5 | Server serialize: `header + blob` 拼接 | ✅ | P0 scatter-write |
| 6 | Server `write_reply_into` → SHM | ❌ | 基础传输拷贝 |
| 7 | Client warm buffer memmove | ❌ | 释放 SHM block 必需 |
| 8 | Client deserialize: `bytes(raw)` | ✅ | P1 memoryview-aware |
| 9 | Client deserialize: `raw[16:]` slice | ✅ | P1 memoryview slicing |

**理论最优: 3 次拷贝**（#2 写入请求 SHM + #6 写入响应 SHM + #7 从 SHM memmove 到 warm buffer）

---

## Phase 1: P1 — memoryview-aware Deserialize

**消除拷贝**: #3, #4, #8, #9（4 次）
**预期收益**: 9 → 5 次拷贝，1GB 延迟从 ~891ms 降至 ~500ms
**复杂度**: 低（~40 行改动）
**风险**: 极低（向后兼容）

### 核心洞察

`struct.unpack_from` 已支持 memoryview，`memoryview[16:]` 是零拷贝子视图（不像 `bytes[16:]` 会创建新对象）。问题仅在于用户代码习惯性地写 `bytes(raw)`。

### 实现方案

**1. Transferable 基类添加标志**

```python
# src/c_two/rpc/transferable.py — Transferable 基类
class Transferable(metaclass=TransferableMeta):
    __memoryview_aware__: bool = False  # 新增
```

**2. `@transfer` 装饰器中根据标志决定是否转换**

```python
# transferable.py — crm_to_com() 中 input deserialize 前 (~line 376)
if request is not None and input_transferable is not None:
    data = request
    if not getattr(input, '__memoryview_aware__', False) and isinstance(data, memoryview):
        data = bytes(data)  # 兼容旧代码
    deserialized_args = input_transferable(data)

# transferable.py — com_to_crm() 中 output deserialize 前 (~line 344)
if output_transferable:
    data = result_bytes
    if not getattr(output, '__memoryview_aware__', False) and isinstance(data, memoryview):
        data = bytes(data)  # 兼容旧代码
    return output_transferable(data)
```

**3. bytes 快速路径标记为 aware**

```python
# create_default_transferable 中
DynamicInputTransferable.__memoryview_aware__ = True
DynamicOutputTransferable.__memoryview_aware__ = True
```

**4. 用户侧 Payload 更新**

```python
@cc.transferable
class Payload:
    __memoryview_aware__ = True  # opt-in

    def deserialize(raw: bytes | memoryview) -> 'Payload':
        # 不再 bytes(raw)，直接用 memoryview
        tag, ts = struct.unpack_from('<Qd', raw, 0)
        blob = bytes(raw[16:])  # 仅在最终需要 bytes 对象时转换
        return Payload(tag=tag, timestamp=ts, blob=blob)
```

### 关键文件

- `src/c_two/rpc/transferable.py` — 添加标志、修改 wrapper 函数
- `tests/unit/test_transferable.py` — 添加 memoryview-aware 测试

---

## Phase 2: P0 — Scatter-Write Serialize

**消除拷贝**: #1, #5（2 次）
**预期收益**: 5 → 3 次拷贝，1GB 延迟从 ~500ms 降至 ~200ms
**复杂度**: 中（~80 行改动）
**风险**: 低（返回类型检测，不破坏旧代码）

### 核心洞察

`header + data.blob` 拼接为 1GB 数据产生一次完整 memcpy。如果 `serialize` 返回 `(header, blob)` 元组，框架逐段写入 SHM，避免拼接。

### 实现方案

**1. `wire.py` 添加辅助函数和 scatter-write 支持**

```python
# src/c_two/rpc/util/wire.py

def payload_total_size(payload) -> int:
    """计算 payload 总大小，支持 bytes|memoryview 或 tuple[bytes|memoryview, ...]"""
    if payload is None:
        return 0
    if isinstance(payload, (list, tuple)):
        return sum(len(s) for s in payload)
    return len(payload)
```

**2. `write_call_into` 支持分段 payload**

```python
def write_call_into(buf, offset, method_name, payload=None) -> int:
    # ... 写 header (msg_type + method_name) 不变 ...
    pos = offset + header_end
    if payload is not None:
        if isinstance(payload, (list, tuple)):
            for segment in payload:
                seg_len = len(segment)
                buf[pos:pos + seg_len] = segment
                pos += seg_len
        else:
            payload_len = len(payload)
            buf[pos:pos + payload_len] = payload
            pos += payload_len
    return pos - offset
```

**3. `write_reply_into` 同样支持分段 result_data**

```python
def write_reply_into(buf, offset, error_data=None, result_data=None) -> int:
    # ... 写 msg_type + error_len + error_data 不变 ...
    if result_len > 0:
        pos = offset + 5 + error_len
        if isinstance(result_data, (list, tuple)):
            for segment in result_data:
                seg_len = len(segment)
                buf[pos:pos + seg_len] = segment
                pos += seg_len
        else:
            buf[pos:pos + result_len] = result_data
    return REPLY_HEADER_FIXED + error_len + result_len
```

**4. Client `call()` 使用 `payload_total_size`**

```python
# ipc_v3_client.py — call() 中
from ..util.wire import payload_total_size
payload_len = payload_total_size(args)
wire_size = call_wire_size(method_name, payload_len)
```

**5. Server `reply()` 处理 tuple result_bytes**

```python
# ipc_v3_server.py — reply() 中
result_len = payload_total_size(result_bytes) if result_bytes else 0
# 注意：reuse 路径 (BUDDY_REUSE) 检查 isinstance(result_bytes, memoryview)
# tuple 不是 memoryview，自动走 alloc+copy 路径，无需额外处理
```

**6. 用户侧 serialize 更新**

```python
def serialize(data: 'Payload') -> tuple[bytes, bytes]:
    header = struct.pack('<Qd', data.tag, data.timestamp)
    return (header, data.blob)  # 无拼接！框架 scatter-write
```

### 关键文件

- `src/c_two/rpc/util/wire.py` — scatter-write 支持 + `payload_total_size`
- `src/c_two/rpc/ipc/ipc_v3_client.py` — payload size 计算
- `src/c_two/rpc/ipc/ipc_v3_server.py` — reply 中处理 tuple
- `src/c_two/rpc/transferable.py` — 传递 tuple 不做额外包装

### 注意事项

- **BUDDY_REUSE 路径安全**: reuse 检测 `isinstance(result_bytes, memoryview)` 对 tuple 返回 False，自动走普通 alloc+copy 路径
- **向后兼容**: serialize 返回 bytes 时走原有路径，返回 tuple 时走 scatter-write
- **`call_wire_size` 函数**: 需要接受 `payload_len: int` 参数（已有），调用方改为传 `payload_total_size(args)`

---

## 预期性能目标

| 状态 | 拷贝次数 | 1GB 预估延迟 | 吞吐量 |
|------|---------|-------------|--------|
| 当前 @transferable | 9 | 891ms | 1.12 GB/s |
| +P1 (memoryview-aware) | 5 | ~500ms | ~2.0 GB/s |
| +P0+P1 | 3 | ~200ms | ~5.0 GB/s |
| bytes 快速路径（参考） | 2 | 51ms | 37.4 GB/s |

P0+P1 后残余 4x 差距来自：serialize/deserialize 中 `struct.pack`/`unpack_from` 的 Python 调用开销、`bytes(raw[16:])` 的最终物化拷贝（用户需要 bytes 对象）、scatter-write 的多次 slice 赋值 vs 单次连续写入。

---

## 实施顺序

1. 实施 P1 — 添加 `__memoryview_aware__` 标志和兼容 wrapper
2. 更新 benchmark Payload — 设置标志，移除 `bytes(raw)`
3. 运行 benchmark 测量 — 确认 P1 收益
4. 实施 P0 — scatter-write 支持
5. 更新 benchmark Payload — serialize 返回 tuple
6. 运行 benchmark 测量 — 确认 P0+P1 综合收益
7. 全量测试 — `uv run pytest -q` 确保无回归

---

## 验证方法

```bash
# 1. 单元测试
uv run pytest tests/unit/test_transferable.py -v

# 2. 全量测试
uv run pytest -q

# 3. @transferable benchmark
uv run python temp/v2_v3_general_bench.py

# 4. bytes 快速路径 benchmark（确认不回归）
uv run python measure_v3.py
```

---

## 优化提示词（供 AI 编码助手使用）

### 提示词 1: Phase 1 — memoryview-aware Deserialize

```
你在 rust-buddy 分支上工作。目标：为 @transferable 添加 memoryview-aware 反序列化支持，
消除不必要的 bytes(memoryview) 拷贝。

背景：IPC v3 的 SHM 传输层已提供 memoryview 零拷贝，但 @transferable 的 deserialize
通常做 bytes(raw) 转换，产生 4 次不必要拷贝。

任务：
1. 在 src/c_two/rpc/transferable.py 的 Transferable 基类中添加类属性
   __memoryview_aware__: bool = False
2. 在 @transfer 装饰器的两个 wrapper 函数中添加兼容逻辑：
   - com_to_crm() (客户端→CRM方向): 在调用 output_transferable(result_bytes) 前，
     如果 output transferable 没有 __memoryview_aware__=True 且 result_bytes
     是 memoryview，先转为 bytes
   - crm_to_com() (CRM→组件方向): 在调用 input_transferable(request) 前，同样检查
3. 在 create_default_transferable 中，为 bytes 快速路径的
   DynamicInputTransferable 和 DynamicOutputTransferable 设置
   __memoryview_aware__ = True
4. 在 TransferableMeta 元类中，确保用户定义的 @cc.transferable 类
   如果设置了 __memoryview_aware__ = True 会被正确传播

约束：
- 所有 642+ 测试必须继续通过 (uv run pytest -q)
- 不修改 IPC v2 代码
- 向后兼容：未设置 __memoryview_aware__ 的旧 transferable 类行为不变
- 每个逻辑修改对应一个独立 commit

关键文件：
- src/c_two/rpc/transferable.py（主要修改）
- tests/unit/test_transferable.py（添加测试）
```

### 提示词 2: Phase 2 — Scatter-Write Serialize

```
你在 rust-buddy 分支上工作（已完成 Phase 1 memoryview-aware）。目标：支持 serialize
返回多段 buffer 元组，框架分段写入 SHM 避免拼接拷贝。

任务：
1. 在 src/c_two/rpc/util/wire.py 中：
   - 添加 payload_total_size(payload) -> int 函数
   - 修改 write_call_into: payload 参数支持 tuple[bytes|memoryview, ...]，
     tuple 时逐段写入 buf
   - 修改 write_reply_into: result_data 参数同上
2. 在 src/c_two/rpc/ipc/ipc_v3_client.py 的 call() 中：
   - 计算 wire_size 时使用 payload_total_size(args)
   - 将 args（可能是 tuple）直接传给 write_call_into
3. 在 src/c_two/rpc/ipc/ipc_v3_server.py 的 reply() 中：
   - result_bytes 可能是 tuple，计算 total_wire 时使用 payload_total_size
   - 传给 write_reply_into 时保持 tuple 格式
4. 在 src/c_two/rpc/transferable.py 中确保 serialize 返回的 tuple 不被额外包装

约束：
- 所有 642+ 测试必须继续通过
- 向后兼容：serialize 返回 bytes 走原有路径，返回 tuple 走 scatter-write
- reuse 优化路径：tuple 不是 memoryview，不走 BUDDY_REUSE，自动走 alloc+copy

关键文件：
- src/c_two/rpc/util/wire.py（核心改动）
- src/c_two/rpc/ipc/ipc_v3_client.py（call 路径）
- src/c_two/rpc/ipc/ipc_v3_server.py（reply 路径）
```

### 提示词 3: Benchmark 测量

```
你在 rust-buddy 分支上工作（已完成 Phase 1 + Phase 2）。目标：运行 benchmark
并生成对比报告。

任务：
1. 更新 benchmark Payload 类型以使用新特性：
   - 设置 __memoryview_aware__ = True
   - serialize 返回 (header, blob) 元组
   - deserialize 不再 bytes(raw)，直接用 memoryview
2. 运行 temp/v2_v3_general_bench.py，记录优化后数据
3. 运行 measure_v3.py 确认 bytes 快速路径未回归
4. 生成对比报告写入 doc/bench-general-optimized.md

使用 autoresearch 方法：先建立基线，实施 P1 后测量，再实施 P0 后测量，
记录每步独立贡献。
```

