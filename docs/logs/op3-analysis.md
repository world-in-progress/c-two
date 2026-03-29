# IPC v3 @transferable 优化审计报告 (op3-analysis)

**审计日期**: 2026-03-26
**审计对象**: `doc/opt-transferable-results.md` 中的 7 个保留实验 + `doc/bench-realistic.md` 的一般路径测试
**审计重点**: (1) 作弊性；(2) 安全/并发回归；(3) buddy allocator 弹性伸缩；(4) transferable 进一步优化空间

---

## 一、优化作弊性审查

### 逐实验评估

| 实验 | 优化内容 | 对真实 CRM 有效？ | 评定 |
|------|---------|------------------|------|
| #1 `__memoryview_aware__` | 框架跳过 bytes(mv) 转换 | ✅ 任何 opt-in 的 @transferable 类 | **合法** |
| #2 scatter-write | serialize 返回 tuple 分段写入 | ✅ 任何返回 tuple 的 serialize | **合法** |
| #3 zero-copy deserialize | deserialize 直接操作 memoryview | ✅ 所有 buddy 路径请求 | **合法** |
| #7 auto_transfer 修复 | Priority 1 直接查找 | ✅ 正确性修复，使所有优化生效 | **合法（bug 修复）** |
| #8 deferred buddy free | 客户端返回 SHM memoryview | ✅ 所有 ≥1MB buddy 响应 | **合法但不安全** |
| #9 request block reuse | 复用上次 buddy block | ⚠️ 仅 server reuse 请求块时 | **偏 benchmark** |

### 综合评定

**标题数字 10.6x 加速（36.43ms → 3.44ms）的构成分析**：

bench-realistic.md 提供了关键对照——一般用户编写方式（不使用任何优化特性）的 @transferable：

| 规模 | 优化路径 P50 | 一般路径 P50 | 差距 |
|------|------------|------------|------|
| 10MB | 0.386ms | 2.824ms | 7.3× |
| 100MB | 2.962ms | 20.917ms | 7.1× |
| 1GB | 24.640ms | 936.071ms | 38.0× |

**一般路径 vs v2 的加速仍有 8.6×**（geomean ≥10MB），这完全来自传输层优化（buddy allocator、SHM、全双工），与序列化优化无关。这个数字是真实的。

**优化路径的额外 7-38× 提升**需要用户：
1. 设置 `__memoryview_aware__ = True`
2. serialize 返回 tuple
3. deserialize 直接操作 memoryview
4. CRM 返回输入的 memoryview 引用（仅 echo/proxy 场景）

其中 #4（scatter-reuse + deferred buddy free）是 echo 专属。去掉 #4，仅 #1-#3 的收益约为 **从 9 次拷贝降至 3 次**，预估 1GB 延迟 ~200ms（vs 优化路径 24.6ms vs 一般路径 936ms）。

**结论**：传输层 8.6× 加速是普遍适用的，无作弊。序列化层优化需要用户主动适配代码，收益真实但需明确沟通。scatter-reuse 路径（实验 6+7+9）几乎仅适用于 echo/proxy 场景。

---

## 二、安全性与并发性回归审查

### CRITICAL

#### OPT-C1 再现：Deferred Buddy Free 返回 SHM memoryview（use-after-free）

**文件**: `ipc_v3_client.py:349-354`

```python
if data_size >= _WARM_BUF_THRESHOLD:
    self._deferred_response_free = free_info
    return mv[5:]  # 返回指向 SHM 的 memoryview
```

**前次审计（op2-analysis.md）已将此标记为 CRITICAL**。当前代码中：
- 返回的 memoryview 指向 SHM 共享内存
- 下一次 `call()` 时释放或复用该 buddy block（line 251-260）
- 调用者如果存储引用、传给其他线程、或在回调中使用 → **use-after-free（segfault 或数据损坏）**

**更严重的问题**：`call()` 的 docstring (line 233-238) 声称 "返回 memoryview 指向客户端拥有的 warm buffer（安全持有，不依赖 SHM）"——这是**事实性错误**。warm buffer 路径已是死代码，实际返回的是 SHM 支持的 memoryview。

**修复建议**：
- 方案 A：默认走 `bytes(mv[5:])` 安全路径，添加 `_zero_copy=False` opt-in 参数(是否会产生新的拷贝从而降低性能？需要实验验证)
- 方案 B：至少修正 docstring 并在 `_flush_deferred_response_free` 中添加 `sys.getrefcount()` 检查

### IMPORTANT

#### Scatter-Reuse 路径 TOCTOU + Double-Free

**文件**: `ipc_v3_server.py:314-315`

Scatter-reuse 路径使用 `.get()` 而非 `.pop()` 读取 `_deferred_frees`：

```python
with self._deferred_frees_lock:
    info = self._deferred_frees.get(rid_key)  # 未移除条目！
```

**问题 1 (TOCTOU)**：在 `.get()` 和后续 SHM 写入之间，另一线程可能通过 `_free_deferred()` 释放该 block，导致 scatter-reuse 写入已释放的 SHM。

**问题 2 (Double-Free)**：scatter-reuse 成功后未移除 `_deferred_frees` 条目。客户端通过 BUDDY_REUSE 帧释放 block，但连接关闭时服务端的 `finally` 块再次释放同一 block → **double-free**。

**对比**：零拷贝 reuse 路径（line 268-271）已正确使用 `.pop()`：
```python
with self._deferred_frees_lock:
    info = self._deferred_frees.pop(rid_key, None)  # 原子移除
```

**修复**：line 314 改为 `.pop(rid_key, None)`，失败时 re-register（与 line 298-299 模式一致）。

#### SHM 原地覆写在无锁状态下执行

**文件**: `ipc_v3_server.py:280-282, 334-339`

零拷贝 reuse 和 scatter-reuse 路径都在 `_conn_lock` 之外写入 SHM。`seg_mv` 是在锁内快照的（line 243），但写入发生在锁外。如果 `destroy()` 并发调用 `conn.cleanup()`，底层 SHM 可能已 unmap → segfault。

**此问题自 op2-analysis.md 以来持续存在。**

### SUGGESTION

#### 死代码：Warm Buffer 基础设施

`ipc_v3_client.py` 中的 `_libc`、`_memmove`、`_memset`（lines 70-76）、`_response_buf`、`_ensure_response_buf()`（lines 222-231）全部为死代码。模块 import 时仍加载 libc（`ctypes.CDLL`），增加启动延迟。

#### Docstring 与实际行为不符

`call()` docstring (line 233-238) 描述 warm buffer 行为，与实际的 SHM memoryview 返回不一致。

---

## 三、Buddy Allocator 弹性伸缩与超限缓解评估

### 3.1 弹性伸缩能力

| 能力 | 状态 | 评估 |
|------|------|------|
| 懒创建 segment | ✅ 良好 | 首次 alloc 才创建，不浪费内存 |
| 按需扩展 segment | ✅ 良好 | 三层降级：现有段 → 新段 → dedicated |
| Dedicated segment GC | ✅ 良好 | 可配置延迟回收，避免抖动 |
| **Buddy segment 缩容** | ❌ **缺失** | 一旦创建永不释放，即使完全空闲 |
| **页面归还 OS** | ❌ **缺失** | 无 `madvise(MADV_DONTNEED)` 提示 |

**[IMPORTANT] Buddy segment 不可缩容**：以默认配置（`max_segments=8`，每段 256MB），一次突发分配可能创建 8 段共 2GB SHM，之后即使需求降为 0，这 2GB 永不释放。对长期运行的服务器这是严重的内存浪费。

**建议**：添加 `gc_buddy()` 方法，检测 `alloc_count == 0 && free_bytes == data_size` 的完全空闲段，延迟 unlink。需注意 segment 索引在活跃 `PoolAllocation` 中被引用的问题。

### 3.2 超限缓解策略

| 场景 | 处理方式 | 评估 |
|------|---------|------|
| 所有段满 + max_segments 达限 | 降级到 dedicated segment | ✅ 合理 |
| Dedicated 也达限 | 返回 Python 异常 | ✅ 合理 |
| **单次分配 > 4GB** | `assert!` **panic（进程崩溃）** | ❌ **CRITICAL** |
| **负数 `gc_delay_secs`** | `Duration::from_secs_f64` **panic** | ❌ **CRITICAL** |
| max_segments=0 + max_dedicated=0 | 所有分配立即失败 | ⚠️ 无验证 |

**[CRITICAL] >4GB 分配 panic**（`pool.rs:401-404`）：

```rust
assert!(seg.size() <= u32::MAX as usize,
    "dedicated segment exceeds 4GB limit");
```

OS 允许创建 >4GB SHM 段（`DedicatedSegment::create` 成功），但随后 assert 导致进程崩溃。应改为 `Err()` 返回。

**[CRITICAL] 负数 gc_delay_secs panic**（`pool.rs:191`）：

```rust
let delay = Duration::from_secs_f64(self.config.dedicated_gc_delay_secs);
```

`Duration::from_secs_f64` 对负值 panic。FFI 层未验证此参数。

### 3.3 碎片化韧性

| 方面 | 状态 |
|------|------|
| Buddy merge 正确性 | ✅ 标准实现，双向递归合并 |
| Double-free 检测 | ✅ `is_free()` 检查 + 饱和减法 |
| 主动碎片整理 | ❌ 无（但 buddy 系统的合并即是碎片整理） |
| 小块 slab 优化 | ❌ 无（所有大小统一走 buddy） |

碎片化韧性在标准 buddy 系统水平，对于 IPC 场景（分配大小相对固定）是足够的。

### 3.4 配置鲁棒性

**[IMPORTANT] Rust 侧 `PoolConfig` 无验证**：FFI 层 `PyPoolConfig::new()` 验证了 power-of-2 和最小尺寸，但 Rust 原生代码可以构造任意 `PoolConfig`。`segment_size` 非 2 的幂、`min_block_size=0` 等不会被拦截，可能导致 allocator 内部 panic 或无限循环。

### 3.5 崩溃后恢复

| 场景 | 状态 |
|------|------|
| 正常 destroy() | ✅ 正确 unlink 所有 SHM |
| 进程崩溃（SIGKILL） | ❌ SHM 泄漏至重启 |
| 崩溃后 spinlock 残留 | ❌ 其他进程永久死锁 |
| 启动时清理 stale SHM | ❌ 无此机制 |

**[IMPORTANT] Spinlock 残留问题**：如果进程在持有 `ShmSpinlock` 时被 SIGKILL，锁停留在 LOCKED 状态。其他共享该 SHM 的进程自旋 10M 次后 panic ("deadlock detected")。

**建议**：考虑 PID-based lock ownership——attaching 进程检测 holder PID 是否仍存活，不存活则 force-unlock。

---

## 四、Transferable 机制进一步优化空间

### Tier 1：高收益、低难度

#### 4.1 消除一般路径中不必要的 `bytes(memoryview)`

**预期收益**：每方向消除 1 次完整拷贝（一般路径 1GB 约节省 ~200ms）
**难度**：低

自 Python 3.8 起，`pickle.loads()` 直接接受 memoryview。当前框架在 `__memoryview_aware__ = False` 时强制 `bytes(memoryview)` 转换，这对默认 pickle 反序列化器是**不必要的**。

**建议**：将 `__memoryview_aware__` 的默认值从 `False` 改为 `True`（或者更保守地：仅对 `create_default_transferable` 生成的 pickle-based 类设为 `True`，因为 `pickle.loads` 已确认支持 memoryview）。这是**最大的低垂果实**——零代码改动（用户侧），消除 2 次完整拷贝。

#### 4.2 简化默认输入序列化器——去除 dict 包装

**预期收益**：小消息吞吐量提升 ~10-30%
**难度**：低

当前默认输入序列化器（`transferable.py:193-204`）构建 `{param_name: value, '__serialized__': True}` dict 再 pickle。反序列化端解包 dict、检查 sentinel、重建有序列表、转 tuple。

直接 `pickle.dumps(args)` 作为 tuple，反序列化端 `pickle.loads(data)` 返回 tuple 即可。参数名映射从未被框架使用。这消除了每次调用的 dict 构造、sentinel 处理和有序重建开销。

#### 4.3 `serialize_into` 协议——直接写入 SHM

**预期收益**：每方向消除 1 次拷贝（合并 serialize + SHM 写入为单步）
**难度**：中

添加可选方法 `serialize_into(buf, offset, *args) -> int` 和 `serialized_size(*args) -> int`。当存在时，框架先计算大小、alloc buddy block、再调用 `serialize_into` 直接写入 SHM memoryview。

```python
class Transferable:
    def serialized_size(*args) -> int: ...    # 新：返回序列化字节数
    def serialize_into(buf, offset, *args) -> int: ...  # 新：直接写入 buf
```

对 pickle 路径无法直接适用（pickle 不支持写入预分配 buffer），但对所有自定义 @transferable 有效。

### Tier 2：中等收益、中等难度

#### 4.4 Wire 层 method name 缓存

**预期收益**：每次请求节省 ~1μs（bytes→str decode + 对象分配）
**难度**：低

`decode()` 中 `bytes(buf[3:header_end]).decode('utf-8')` 对每个请求重复解码方法名。方法名来自固定集合，添加 `dict[bytes, str]` 缓存可消除重复分配。

#### 4.5 跳过 Envelope 对象创建

**预期收益**：每次请求节省 ~200-500ns（dataclass init + 字段赋值）
**难度**：中

服务端对每个请求调用 `decode()` 创建 `Envelope` 对象。对于 CRM_CALL（最常见情况），可以像客户端对 CRM_REPLY 那样内联解析，跳过 Envelope 创建。

#### 4.6 一般路径的 Warm Buffer

**预期收益**：消除大数据响应的 page fault 开销
**难度**：中

当前 warm buffer 因 deferred free 路径而成为死代码。如果改为默认安全路径（Fix OPT-C1），warm buffer 应被重新激活以服务一般路径的大响应。

### Tier 3：低收益或需架构变更

#### 4.7 替代 pickle 的序列化器

对常见简单类型（int, float, str, bytes, list, dict），msgpack 比 pickle 快 2-3x。但仅对高频小消息有意义——大载荷场景瓶颈在拷贝而非序列化格式。

#### 4.8 批量/流水线 RPC

客户端一次发送 N 个请求、服务端批量处理、一次返回 N 个结果。消除 N-1 次 round-trip。需要 API 变更和并发模型调整，暂不建议。

---

## 五、问题汇总

### CRITICAL

| ID | 问题 | 来源 |
|----|------|------|
| OPT-C1（再现） | Deferred buddy free 返回 SHM memoryview，下次 call() 后失效。docstring 错误声称 warm buffer | op2 遗留 |
| BUDDY-PANIC-1 | >4GB dedicated 分配 `assert!` 导致进程崩溃 | 新发现 |
| BUDDY-PANIC-2 | 负数 `gc_delay_secs` 导致 `Duration::from_secs_f64` panic | 新发现 |

### IMPORTANT

| ID | 问题 | 来源 |
|----|------|------|
| SCATTER-TOCTOU | scatter-reuse 用 `.get()` 非 `.pop()`，TOCTOU + double-free | 新发现 |
| SHM-WRITE-NOLOCK | 原地覆写 SHM 在无锁状态，并发 destroy 可 segfault | op2 遗留 |
| BUDDY-NO-SHRINK | Buddy segment 不可缩容，突发后 2GB 永不释放 | 新发现 |
| BUDDY-NO-STALE-CLEANUP | 进程崩溃后 SHM 泄漏，无启动清理机制 | 新发现 |
| SPINLOCK-CRASH | 进程崩溃时 spinlock 残留，其他进程死锁 | 新发现 |
| CONFIG-NO-VALIDATE | Rust 侧 PoolConfig 无验证，pathological 值导致 panic | 新发现 |

### SUGGESTION

| ID | 问题 |
|----|------|
| 死代码 | warm buffer 基础设施 + libc 加载 |
| Docstring | call() 文档与实际行为不符 |
| 测试覆盖 | deferred free 生命周期、request reuse、scatter-reuse 无直接测试 |

---

## 六、建议优先行动

### 必须在合并前修复

1. **OPT-C1**：deferred buddy free 改为默认安全路径（bytes 拷贝），零拷贝 opt-in
2. **SCATTER-TOCTOU**：scatter-reuse 路径 `.get()` → `.pop()`
3. **BUDDY-PANIC-1**：>4GB assert → Err 返回
4. **BUDDY-PANIC-2**：gc_delay_secs 验证 ≥ 0

### 短期建议

5. **4.1**：`__memoryview_aware__` 对 pickle 默认类设为 True（消除 2 次拷贝，零用户改动）
6. **4.2**：简化默认输入序列化器（去除 dict 包装）
7. 修正 docstring、清理死代码

### 中期建议

8. **BUDDY-NO-SHRINK**：添加 `gc_buddy()` 空闲段回收
9. **SPINLOCK-CRASH**：PID-based lock ownership
10. **4.3**：serialize_into 协议
11. **BUDDY-NO-STALE-CLEANUP**：启动时扫描清理 stale SHM

