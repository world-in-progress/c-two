# R33 续批：退役旧 Python 手工分片投影链

状态：2026-10-24 已完成用户批准的两个 Python 手工包装及导出退役，不留兼容。实现提交 `921b48ee7e54f5b88cafff5e4e19a31d47d5d545` 已通过基线覆盖、完整 relay、跨语言、三系统及 GitHub Actions 10/10、本批端点零残留和独立审查，见[验收记录](2026-10-24-r33-python-manual-assembly-retirement-acceptance.md)。固定输入为 `ed6c761abcb2fcca7ccfac4b5cd89cd87762a58d`。本项归入 R33 的已完成子项；R35 继续暂缓。这里退役了一项公开 Python 手工能力，不能描述成完全没有 API 影响的内部删除。用户明确接受这部分支持面收窄，Rust 分片链及现行 IPC 行为保留。

## 调用链与原设计理由

名称澄清：`c2_wire::chunk::ChunkRegistry`、`c2_wire::assembler::ChunkAssembler` 和 `c2_wire::chunk::ReassemblyBacking` 是正式 IPC 使用的 Rust 类型，全部保留。候选中的 Python `c_two.mem.ChunkAssembler` 是 `mem_ffi.rs` 中通过 `#[pyclass(name = "ChunkAssembler")]` 暴露的 `PyChunkAssembler` 包装类；它内部调用同一个 Rust `ChunkAssembler`。Python `c_two.mem.MemHandle` 是 `PyMemHandle` 包装类，持有 `ReassemblyBacking`，也不是要删除 Rust `c2_mem::MemHandle`。

两条入口共享底层算法：正式 IPC 由 Rust registry 创建 assembler 并传递 backing；Python 手工入口先创建 PyChunkAssembler，再由其调用 Rust assembler，finish 后交给 PyMemHandle。候选仅评估退出后者的两个 Python 包装类及导出。若继续支持 Python 手工入口，包装层的 consumed/released 状态有语言绑定职责，不能称为无意义防御或重复重组算法；本项因此属于公开能力退役评估，不能等同于正式 IPC 运行时逻辑去重。

正式分片收发由 Rust 拥有：客户端 `c2-ipc/src/client.rs` 的接收循环和服务端 `c2-server/src/server.rs` 的分片请求处理进入 `c2-wire::chunk::ChunkRegistry`；registry 在接纳预算后建立 `ChunkAssembler::new_with_guard`，完成后移动 `ReassemblyBacking`。Python CRM 代理与 `RuntimeSession` 不构造 Python `ChunkAssembler` 或 `MemHandle`。两种 Python 类在仓内的实际构造消费者只剩 `sdk/python/tests/unit/test_chunk_assembler.py`；公开导出仍在 `c_two.mem` 和 `_native`，不能由仓内搜索推断仓外没有手工调用者。

`b7f1025c` 在 2026-04-01 增加 PyO3 的 `PyChunkAssembler` 与 `PyMemHandle`，让当时仍由 Python 管理的收发循环使用 Rust 分配和重组。`fb495c45` 接入 Python 服务端请求重组，`6bc970dc` 接入 Python 客户端回复重组；后者同时把旧 Python 重组测试迁到这两个类。`f608331e` 加入 frozen 与 Mutex，使 Python 手工调用具备同步保护。这些状态在当时有实际调用者。

同日的 `0444db8c` 和 `92745655` 分别移除旧 Python 客户端和服务端收发循环。今天的 PyO3 投影仍单独维护 assembler 的 consumed 状态、route_name/method_idx 元数据、MemHandle 的 released 状态、裸地址读取、显式释放和 Drop；固定的 512 chunks / 8 GiB 手工接纳参数也只服务这一入口。Rust 算法没有重复实现，重复的是已退出正式调用链的 Python 入口及生命周期投影。之后的预算、精确释放修复曾同步维护这些公开类，`f2657dcb`（2026-10-22）还修复了手工 assembler 元数据与 owner 同时消费；这些状态对于仍在调用公开类的用户确有职责。退役必须以支持范围决定为依据，不能把这些修复当作错误而撤销 Rust 本身的保护。

## 替换范围与净收益

| 范围 | 具体改动 | 静态预算 |
| --- | --- | ---: |
| `sdk/python/native/src/mem_ffi.rs` | 删除 PyChunkAssembler、AssemblerInner、PyMemHandle、MemHandleInner、两个模块注册、专属 imports 和唯一服务手工 assembler 的 pool_arc 私有方法；保留 PyMemPool 的容器与其余方法 | 净减约 267 行 |
| `sdk/python/src/c_two/mem/__init__.py` | 删除上述两个名称的 import 与 __all__，保留其余内存池和统计导出 | 净减 4 行 |
| `sdk/python/tests/unit/test_chunk_assembler.py` | 退役仅服务这条 Python 手工链的 329 行、17 个测试函数（参数展开约 19 项） | 删除 329 行 |
| `core/protocol/c2-wire/src/assembler.rs` 现有测试 | 边界数据、重复片拒绝后继续完成、部分 feed 后 abort；复用现有 pool helper | 预计新增 70–90 行 |
| `core/protocol/c2-wire/src/chunk/registry.rs` 现有测试 | 同池同连接不同 request 的多块交错、内容及预算独立 | 预计新增 25–35 行 |
| `core/protocol/c2-wire/src/chunk/backing.rs` 现有测试 | released 后 write_at 拒绝 | 预计新增 2–4 行 |
| Python 既有模块边界测试 | 检查两种名称同时退出 mem 与 _native，同时保留其余导入 | 预计新增 6–10 行 |
| 生产与测试合计 | 两层 Python 状态及专属测试一起退出；正式 Rust 算法、载体、预算、生命周期保留 | 生产约 −271 行，测试约 −190 至 −226 行；合计预计净减约 460–500 行 |

生产删除量来自对固定源码的内存中删除预览，没有把预览写入工作区；测试增量是包含缺口覆盖的预算，待实施后按实际 diff 核算。收益是减少需要维护和发布的手工重组/释放投影，不声明正式 RPC 延迟或吞吐提升。保留 `MemPool` 的 Arc/RwLock 形状，不顺带改其所有权；不移除仍被其他绑定使用的 c2-wire 依赖。

## 测试覆盖迁移

先在未删除的基线上运行原 Python 手工链测试及调整后的 Rust 测试。已有 Rust 单测覆盖单片、顺序/乱序、多片短尾和精确长度、重复片拒绝、abort 后预算与分配归还、未完成 finish 错误、非法数量/尺寸、预算拒绝与 backing 保活；`chunk/backing.rs` 已覆盖 trim 保留原容量、成功释放幂等及失败保留 owner/charge。

补齐的具体观察包括空数据片、chunk_size=1、100 个小片、最后一片仅一个字节、重复片拒绝后 received 不变且仍能正确完成、同池两个 assembler 交错推进后内容独立；在既有顺序/乱序/单片用例内用固定输入和显式断言覆盖，在 registry 既有模块补同池同连接两个 request 的确定性交错：释放一项后另一项仍能完成，检查两份完整内容及预算；在 backing 的既有已释放用例补 write_at 拒绝。元数据 setter、consumed 后 Python 属性、repr 与异常类别属于被退役 API 的专属行为，随接口删除；真实 route context、permit、backing 释放与预算行为仍由 registry/transport 现有测试验证。

PyMemPool 的分配、读写、精确释放及模块导入测试继续保留。真实 Python/Rust/Node 大消息与持久连接回归继续覆盖正常 IPC 分片收发，不能用低层 Rust assembler 单测替代跨语言门禁。

## API、ABI、协议与语义影响

明确删除公开 `c_two.mem.ChunkAssembler`、`c_two.mem.MemHandle` 及其 `_native` 导出；不留别名、兼容包装或替代 Python 手工重组入口。手工调用 `ChunkAssembler(pool, ...)`、`feed_chunk/finish/abort`，再通过结果的 `buffer_info/write_at/release` 使用裸 backing 的代码将不能继续导入和运行。Python 没有另外一个等价手工 assembler API；有此需求的调用者须改用现行 RPC 自动分片，或在 Rust 使用保留的低层重组 API，不能宣称无须迁移。

保留 Python `MemPool`、`PoolAlloc`、`PoolConfig`、`PoolStats`、内存统计、ResponseBuffer、HeldResult 与正式 FastDB/pickle 路径。Rust 的 `ChunkAssembler`、`MemHandle`、`ReassemblyBacking` 及公开签名均不变；C/FFI ABI 5、IPC 12、SHM 3、owner control 1、业务帧与 CRM 哈希不变。预算接纳、原容量计费、身份和代际、快照、取消及存储先释放再归还预算的顺序保持。R06/R07 不混入。

主要风险是误认调用者、删掉仍有职责的池 API，以及删除 Python 测试时漏掉真实重组边界。控制范围为上述两种 Python 类及其唯一专属辅助；基线覆盖通过后再删除，核对 Rust 生产源、ABI 和协议 diff 为空。文档同步现行导出表与 unreleased 迁移说明；历史报告和设计说明保留其时间语境。

## 确认后的实施与验收

先补并运行基线覆盖，再同步删除生产投影和专属测试；针对性运行 c2-wire assembler/registry/backing、Python mem pool/模块边界/held 与预算用例。然后完成完整 relay、真实 Rust/Python/Node 跨语言、持久连接和大消息分片。Windows 优先开发机 Named Pipe；确切实现提交必须通过 GitHub Actions Linux、macOS、Windows x64/ARM64 及适用 3.14t，接受独立实现审查后再提交。只验收本批创建的 socket/lease/Named Pipe 零残留，及时清理构建产物，历史目录不动。

验收结果：原 Python 手工链 19 项全部通过；补齐后的原 Rust 生产实现通过 c2-wire 164 项。导出退役断言在旧原生扩展上按预期失败，删除包装并重建后通过。生产净减 271 行，测试净减 215 行，合计净减 486 行。Windows 开发机完成完整 relay 和 Rust/Python/Node 实跑，确切实现的 GitHub Actions 首轮 10/10 成功，包含 Linux、macOS、Windows x64/ARM64 与 3.14t；独立审查无 P0–P3 发现，本批端点零残留，构建产物和两个委派工作区已清理。详细计数、证据和文档增量见验收记录。

## 独立复核与 Host 取舍

hey-my-buddy 只读复核 run `fdec0bdd-a6ef-4289-8a1b-d38ae6989b94` 核验同一输入，确认旧生产消费链已经退出、PyMemHandle 没有第二个工厂，并逐项映射 17 个函数、19 个用例。它提出两点需明确：这项手工能力仍独立可用，不能仅为减量删除；删除其唯一调用者后，6 行 crate-private pool_arc 会变成孤儿。Host 将前者列为本方案的公开支持范围取舍，将后者明确纳入同一候选，但保持 PyMemPool 公开 API、容器和池逻辑不变。已采用其指出的重复后继续、下界输入、交错隔离及写入拒绝覆盖缺口，因此修正了测试增量预算。

复核的附加 AST 清单因只读 shell 无法创建 heredoc 临时文件而未执行；Host 已在授权工作区用 AST 核对测试函数数量、参数展开与调用点，并在内存中核对删除区间，未实施删除或构建。此处是分析证据，不是实施、基线回归、平台验证或最终验收。
