# OP4: op3-analysis.md 修缮结果

基于 `doc/op3-analysis.md` 审查报告，本次修缮涵盖安全修复、性能优化和单元测试三个方面。

**分支**: `autoresearch/op3-fixes`（基于 `autoresearch/transferable-opt` @ `009ed2b`）

---

## 1. 安全修复

### 1.1 OPT-C1: 延迟释放 use-after-free（CRITICAL）

**问题**: `call()` 对 ≥1MB buddy 响应返回 `mv[5:]`（SHM memoryview），延迟到下次 `call()` 才释放 buddy block。调用方持有悬空引用。

**修复**:
- `call()` 默认返回 `bytes(mv[5:])`（安全拷贝），立即释放 buddy block
- 移除死代码：libc memmove/memset 加载、`_ensure_response_buf`、`_response_buf` 字段
- 修正错误 docstring（"server→client" 改为 "client→server"）

**文件**: `src/c_two/rpc/ipc/ipc_v3_client.py`

### 1.2 SCATTER-TOCTOU: 散射复用竞态（CRITICAL）

**问题**: 散射复用路径使用 `.get()` 读取 `_deferred_frees`，条目未移除。并发连接关闭时可能触发 double-free。

**修复**:
- `.get()` → `.pop()`（原子移除）
- 失败时重新注册条目，防止泄漏

**文件**: `src/c_two/rpc/ipc/ipc_v3_server.py`

### 1.3 BUDDY-PANIC-1: >4GB 断言崩溃（CRITICAL）

**问题**: dedicated segment 超过 4GB 时 `assert!(seg.size() <= u32::MAX)` 导致 panic。

**修复**: `assert!` → `Err()` 返回，Python 层收到 `RuntimeError`。

**文件**: `rust/c2_buddy/src/pool.rs`

### 1.4 BUDDY-PANIC-2: 负 gc_delay panic（CRITICAL）

**问题**: `Duration::from_secs_f64(negative)` 在 Rust 中 panic。

**修复**:
- `gc_dedicated()` 中负值 clamp 到 `Duration::ZERO`
- FFI 层添加 NaN 验证

**文件**: `rust/c2_buddy/src/pool.rs`, `rust/c2_buddy/src/ffi.rs`

### 1.5 配置验证强化

- `segment_size` 必须为 2 的幂
- `dedicated_gc_delay_secs` 不能为 NaN
- 这些在 `PyPoolConfig::new()` 中验证

---

## 2. 性能优化

### 2.1 pickle 默认 `__memoryview_aware__=True`（op3-analysis 4.1）

**问题**: 默认 pickle 路径的 `DynamicInputTransferable` 和 `DynamicOutputTransferable` 未设置 `__memoryview_aware__`，导致每次 RPC 传输层强制 `bytes(memoryview)` 转换 ×2（请求+响应）。

**修复**: 两个动态类均设置 `__memoryview_aware__ = True`。`pickle.loads()` 自 Python 3.8 起原生支持 memoryview。

**效果**: 1GB 场景 892ms → 702ms（**-21%**），geomean P50 35.5 → 34.9ms（-1.7%）

**文件**: `src/c_two/rpc/transferable.py`

### 2.2 简化默认输入序列化器（op3-analysis 4.2）

**问题**: 旧路径使用 `pickle.dumps({param_name: value, '__serialized__': True})`，需要构建 dict、插入哨兵值、按参数名排序重建——复杂且冗余。

**修复**: 改为直接 `pickle.dumps(args)` / `pickle.dumps(arg)`。移除 dict 包装、`__serialized__` 哨兵、有序重建逻辑。

**效果**: 性能中性（geomean 34.9 → 35.4ms，噪声范围），代码行数减少约 30 行。

**文件**: `src/c_two/rpc/transferable.py`

---

## 3. 安全单元测试

将 op3-analysis.md 中识别的安全问题转化为可持续验证的单元测试，共新增 15 个测试用例。

### 3.1 Buddy Pool 安全测试（`test_buddy_pool.py`）

| 测试 | 覆盖问题 | 验证内容 |
|------|---------|---------|
| `test_oversized_dedicated_returns_error_not_panic` | BUDDY-PANIC-1 | 5GB alloc 返回 RuntimeError，不 panic |
| `test_negative_gc_delay_no_panic` | BUDDY-PANIC-2 | gc_delay=-1.0 时 dedicated GC 不 panic |
| `test_nan_gc_delay_rejected` | 配置验证 | NaN gc_delay 被拒绝 |
| `test_non_power_of_two_segment_rejected` | 配置验证 | 非 2 的幂 segment_size 被拒绝 |
| `test_double_free_does_not_corrupt` | R-C5 相关 | double-free 后 alloc_count ≥ 0 |
| `test_alloc_after_double_free_still_works` | R-C5 相关 | double-free 后 pool 仍可正常使用 |
| `test_exhaustion_returns_error` | 段耗尽 | buddy + 无 dedicated 时 alloc 返回错误 |
| `test_dedicated_exhaustion_returns_error` | 段耗尽 | max_dedicated 满后 oversized alloc 返回错误 |

### 3.2 IPC v3 协议安全测试（`test_ipc_v3.py`）

| 测试 | 覆盖问题 | 验证内容 |
|------|---------|---------|
| `test_buddy_response_returns_bytes` | OPT-C1 | buddy 路径返回 bytes 类型（非 memoryview） |
| `test_handshake_version_mismatch` | 协议安全 | 版本不匹配时抛出异常 |
| `test_payload_roundtrip_dedicated` | 协议正确性 | dedicated 段 payload 编解码正确 |
| `test_payload_roundtrip_normal` | 协议正确性 | 普通 payload free_coords == data_coords |
| `test_empty_handshake_segments` | 边界条件 | 空段列表 handshake 不崩溃 |
| `test_rapid_connect_disconnect` | 资源泄漏 | 10 次快速连接/断开无泄漏 |
| `test_concurrent_clients_heavy` | 并发安全 | 8 线程 × 20 次迭代压力测试 |

**测试总数**: 701 → 716（+15）

---

## 4. Benchmark 结果

### 4.1 实验迭代记录

| # | commit | Geomean P50 (ms) | 状态 | 描述 |
|---|--------|-------------------|------|------|
| 0 | 009ed2b | 36.832 | baseline | 未修改的一般 @transferable 路径 |
| 1 | acb60f4 | 36.539 | keep | OPT-C1: 安全 bytes() 默认值，移除死代码 |
| 2 | 4b22c60 | 35.478 | keep | SCATTER-TOCTOU: .get() → .pop() 安全修复 |
| 3 | 2fe9490 | 35.532 | keep | BUDDY-PANIC-1/2: assert → Err，gc_delay 验证 |
| 4 | 1fedc4b | 34.891 | keep | pickle __memoryview_aware__=True (1GB: 892→702ms) |
| 5 | 9f25a7e | 35.391 | keep | 简化默认输入序列化器（代码质量） |
| 6 | ac1376d | 35.884 | keep | 安全单元测试（+15 测试，无回归） |

### 4.2 最终 Benchmark（一般 @transferable 路径，realistic CRM）

```
    Size |     v3 P50 |     v2 P50 |  Speedup |    v3 Tput |    v2 Tput
------------------------------------------------------------------------------------------
     64B |    0.117ms |    0.132ms |    1.13x |     0.00GB/s |     0.00GB/s
     1KB |    0.118ms |    0.130ms |    1.10x |     0.01GB/s |     0.01GB/s
     4KB |    0.228ms |    0.134ms |    0.59x |     0.02GB/s |     0.03GB/s
    64KB |    0.158ms |    0.298ms |    1.89x |     0.37GB/s |     0.19GB/s
     1MB |    0.406ms |    2.091ms |    5.15x |     2.36GB/s |     0.45GB/s
    10MB |    2.920ms |   23.581ms |    8.08x |     3.32GB/s |     0.41GB/s
    50MB |   10.177ms |  102.334ms |   10.06x |     4.71GB/s |     0.46GB/s
   100MB |   19.326ms |  209.392ms |   10.83x |     4.92GB/s |     0.47GB/s
   500MB |  134.814ms | 1103.028ms |    8.18x |     3.51GB/s |     0.45GB/s
     1GB |  768.456ms | 5743.469ms |    7.47x |     1.32GB/s |     0.18GB/s

  Geomean P50 (≥10MB): v3=35.884ms  v2=317.000ms  speedup=8.8x
```

---

## 5. 分析与结论

### 5.1 安全改进总结

本次修缮解决了 4 个 CRITICAL 级别问题（OPT-C1 use-after-free、SCATTER-TOCTOU double-free、BUDDY-PANIC-1 >4GB crash、BUDDY-PANIC-2 负数 panic）和多个 IMPORTANT 级别问题（配置验证、死代码清理）。

所有安全修复对性能无负面影响——geomean 从 36.832ms 优化至 35.884ms（-2.6%），在噪声范围内。

### 5.2 性能分析

- **v3 vs v2 一般路径**: 8.8× 加速（geomean P50 ≥10MB）
- **最大加速点**: 100MB 达到 10.83×，这是 buddy SHM 零拷贝传输优势最大的区间
- **4KB 异常**: v3 比 v2 慢（0.59×），因为 buddy 握手 + SHM 查找开销在小 payload 上不划算。4KB < shm_threshold 时走 inline 路径，此开销主要来自 buddy pool 初始化
- **pickle __memoryview_aware__**: 对 1GB 场景单独贡献 21% 提升，验证了零拷贝在大 payload 路径上的价值
- **序列化器简化**: 性能中性但显著提升代码可维护性

### 5.3 op3-analysis.md 覆盖情况

| 项目 | 状态 | 备注 |
|------|------|------|
| OPT-C1 (deferred free UAF) | ✅ 已修复 | 默认安全 bytes() 拷贝 |
| SCATTER-TOCTOU (.get→.pop) | ✅ 已修复 | 原子移除防 double-free |
| BUDDY-PANIC-1 (>4GB assert) | ✅ 已修复 | Err 替代 assert |
| BUDDY-PANIC-2 (负 gc_delay) | ✅ 已修复 | clamp + NaN 验证 |
| 4.1 pickle memoryview_aware | ✅ 已优化 | -21% at 1GB |
| 4.2 简化序列化器 | ✅ 已简化 | 移除 dict 包装 |
| 4.3 serialize_into | ⏭️ 跳过 | 需要 Transferable 协议变更，收益不确定 |
| 4.4 wire method name cache | ⏭️ 跳过 | ~1μs/call，优先级低 |
| 死代码清理 | ✅ 已清理 | warm buffer、libc 加载代码移除 |
| 安全单元测试 | ✅ 已完成 | +15 测试，716 总数 |

### 5.4 Commit 日志

```
009ed2b  baseline (from transferable-opt)
acb60f4  OPT-C1 fix — safe bytes() default, remove dead warm buffer
4b22c60  SCATTER-TOCTOU fix — .get() to .pop() in scatter-reuse
2fe9490  BUDDY-PANIC-1/2 — assert to Err, gc_delay validation
1fedc4b  4.1 pickle default __memoryview_aware__=True
9f25a7e  4.2 simplify default input serializer
ac1376d  safety unit tests from op3-analysis.md audit
```