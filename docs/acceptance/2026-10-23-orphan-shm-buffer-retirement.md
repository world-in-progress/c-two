# 旧 Python ShmBuffer 投影退役方案

状态：已按用户确认边界完成，[验收记录](2026-10-23-shm-buffer-retirement.md)，并随 0.8.2 发布，见[发布记录](2026-10-23-082-client-observation-release-acceptance.md)。分析输入为 `723a9130d9a26d9903d56226b6170f4143a19a29`，实现候选为 `fa62f7a3f5227626a49066d1ae51dacb88a2741a`。本项是在复核剩余 R06 时发现的独立残留，不等同于修复或关闭 R06。

## 选择与历史依据

建议整块退役 `sdk/python/native/src/shm_buffer.rs`。该模块仍导出 `_native.ShmBuffer`，维护 Inline/PeerShm 两种 backing、导出计数、保留租约、显式 release 和 Drop，但现行源码没有构造调用者。删除它无需新增抽象或迁移活跃对象。

原设计确有用途：旧 `PyCrmCallback` 把 `RequestData` 包装成 `PyShmBuffer`，允许 Python 通过 memoryview 访问 backing；导出计数防止 view 存活期间释放，Drop 负责兜底回收。`5a31f591` 为该对象附加了 retained lease 统计。`6e6519a8` 将 Python SDK 接入 Core，删除整个 `server_ffi.rs` 及这些构造调用，却留下了类型和模块注册。之后的分配身份等迁移继续维护了这份孤立表示。

现行请求链为：

```text
RequestData → RequestLease.copy_bytes()
            → EncodedService.invoke(Vec<u8>)
            → PyCoreRequestBuffer → Python memoryview
```

Core 持有原 transport lease，直至服务返回或展开；Python buffer 持有请求快照。`e21e79c4` 已明确采用 owned Vec 服务边界，并让 Python 接管同一 Vec，删除第二次复制。响应走 `HeldResponse → PyCoreResponse`。退役旧类型不新增复制，不改变这些所有权和释放边界。

`shm_buffer` 是 `pub(crate)` 模块；两个构造器没有 Rust 调用者，Python 类没有 `#[new]`。现行 Python 测试只有一处 `hasattr(_native.ShmBuffer, "track_retained")`，没有使用该类实例。需要将 `_native.ShmBuffer` 导出删除视为低层 Python API 变化，不能由“仓内没有消费者”推导外部无人 import 或反射查询。

## 已做的隔离核验

在 macOS、CPython 3.13、已发布 `c-two==0.8.1` wheel 上进行真实 IPC 探针。发布来源 `ff256e6a7538a8a0a81dcd19dfc640cce40f48a3` 与当前输入的 `shm_buffer.rs`、`core_ffi.rs`、Core `lifetime.rs` 没有差异。

- 小请求和 1 MiB 请求到达 Python dispatcher 时，实际类型均为 `CoreRequestBuffer`；响应实际类型为 `ResponseBuffer`。
- `_native.ShmBuffer()` 抛出 `TypeError: cannot create 'builtins.ShmBuffer' instances`。
- 真实 ResponseBuffer 导出期间 release 拒绝，解除导出后 release 成功。
- 探针只使用本次创建的隔离端点根目录；close/shutdown 后 socket/lease 残留为 0。

这些结果证明已发布基线的调用路径；尚未实施删除，不代表候选已通过三系统或 GitHub Actions 验收。

## 替换范围与净量

| 范围 | 拟议改动与物理行净量，含注释和空行 |
| --- | --- |
| 生产 | 删除 `shm_buffer.rs` 全部 335 行；删除 `lib.rs` 的条件模块声明和注册调用共 3 行，合计净减 338 行。修正 `server/native.py` 与 `connection.rs` 的旧类型注释，不改运行逻辑。 |
| 测试 | 删除孤立的 hasattr 用例及冒充 ResponseBuffer 覆盖的重复 tracker 用例；在已有真实 request snapshot 和 response memoryview 用例中补 retained 统计、view 存活时拒绝释放、释放后统计归零。预计测试净增 10–35 行，不新建测试框架。 |
| 合计 | 生产与测试预计净减约 300–330 行；实施后按实际 numstat 单独报告。新增方案、验收和 API 说明不混入代码净量。 |

保留已有请求 Vec 原地址、成功/失败返回后的 view 寿命、重复释放、held/borrowed、预算和 SHM 精确释放覆盖。测试先在未删模块的基线通过，再删除旧模块。不得通过删断言达到目标行数。

同步改正 `AGENTS.md` 中仍把 `RequestData → PyShmBuffer → memoryview` 当成现行链的陈旧要求，说明 Core 快照与 move 边界；更新仍标为 Architecture reference 的 `docs/plans/2026-05-04-thin-sdk-rust-core-boundary.md` 的现行约束，以及 native dispatch 参数说明、相邻 Core 注释和 changelog。沿用 `docs/python-usage.md` 的 copy-backed 与 hold 寿命表述。旧阶段验收、历史报告和 artifact manifest 保留历史事实；`Connection` 上仍有消费者的方法仅改陈旧注释，不连带删除。

## 接口和语义边界

- 删除低层 Python `_native.ShmBuffer` 名称，不留 alias、空壳或兼容工厂。依赖该名称的手工 import/反射会失败，需删除相应依赖。
- 保留 `ResponseBuffer`、`CoreRequestBuffer`、高层 Python/Rust SDK 和现行 buffer 生命周期；不改公开 Rust API、C/FFI ABI 5、IPC 12、SHM 3、CRM 哈希或业务帧。
- 不修活跃 buffer 的导出/释放同步、flags 接纳或 invalidator 重入。这些属于 R06 正确性范围，仍须单独基线反例、设计与确认。
- 不宣称吞吐或复制次数改善。可确认的收益是移除一整套无消费者的 backing/导出/租约/释放实现，以及失真的架构指引。

主要风险是遗漏动态消费者、误把历史零拷贝设计当作当前路径，以及削弱真实 retained 生命周期覆盖。全仓构造/注册追踪、真实请求/响应基线和现有跨语言矩阵分别约束这些风险。

## 确认后的验收

先补基线覆盖，再同步删除生产模块与孤立测试；完成 Python native buffer/held/borrowed 定向回归、完整 relay、真实 Rust/Python/Node 跨语言和持久调用回归。macOS/Linux/Windows 均执行适用门禁，Windows 优先开发机；低层 Python API 删除还须对确切候选完成 GitHub Actions 跨平台验证，包含既有 Windows ARM64 和 3.14t lanes。

完成 socket/lease 零残留与独立实现审查后再提交，报告代码净量。历史目录不动；及时清理本批构建与隔离产物。

## 独立方案复核

使用 hey-my-buddy 0.51.1 对固定输入进行两次独立只读复核。R06 状态合并复核指出响应回调重入及两种 release 错误路径的不同，完整正确性修复预计测试增量超过生产减量，因此本批优先选择旧投影退役。

旧投影退役复核（run `b37726c4-6885-4412-875d-5a30284c321d`，artifact `18c7229e-08d2-41eb-8ae1-7eeb0e2ad9f7`）确认没有实际构造者、工厂、序列化恢复入口、生成器消费者或仓外可见的 Rust 导出；建议直接删除，生产净减 338 行。Host 另行完成上述 wheel 探针，采纳公开 Python 名称删除、当前架构参考更新和保留活跃 Connection 方法的边界。两个只读 artifact 均无源码改动。

实施实际生产净减 338 行，测试净增 2 行，合计净减 336 行。基线覆盖、完整 relay、真实跨语言、本机 macOS/Windows、确切候选 GitHub Actions 10/10（含 Linux、Windows ARM64 与 3.14t）、端点零残留和独立实现审查均完成。R06/R07 与两份控制会话重构继续单独保留。
