# R02 唯一写所有权修复验收

2026-10-21。实施基线为 `a523967b3fc41082ce4273178ce3b15af3948960`，最终实现候选为 `2d1b1d1095f3ab6e4d229fb354262bfc25dd113e`。本记录及清单收尾是后续纯文档提交，不能替代构建来源。用户批准的范围见 [R02 方案](2026-10-20-r02-unique-write-ownership-evaluation.md)；本项作为正确性修复验收，不计作瘦身收益。

## 实现与接口边界

`MemHandle` 现在是仅由真实分配创建的私有、不可复制 owner，SHM owner 绑定不复用的进程内 pool 身份、代际、allocation id、原容量与逻辑长度。不同分配仍可共享借用 pool，写入须独占借用真实 owner；逻辑收缩保留原容量释放。裸坐标复制、释放、映射接纳等入口明确为 unsafe，保留原身份和范围检查；查询裸指针本身不再执行有越界前提的 `ptr.add`。

实际收帧位置创建请求/响应载体并固定原 pool；响应物化与 lease 从载体取 pool，不接受外配容器。预分配请求复用原相位与释放事务，在写锁内核对所属请求池，发布长度来自真实分配。准备响应持有真实 owner；`ResponsePool` 封住可安全替换的池内容，并以映射后的只读 guard 防止还原原始写锁。重连为新接收任务建立新 pool 容器，已持有回复继续引用原容器。没有新增池、锁、全局分配注册表或兼容入口。

低层 Rust API 有意破坏：坐标型 handle/SHM 变体不能再手造；raw 操作要求 unsafe；注入构造器移动 `MemPool`；预分配调用退出重复长度参数；callback 响应池改为 `ResponsePool`；响应物化/lease 不再接收外配 pool。仓内消费者、测试及当前设计文档已同步迁移。高层 Rust/Python 调用面、SHM 3、IPC 12、C/FFI ABI 5、wire、预算、快照与生命周期保持；C header、buddy/handshake wire 文件和 TypeScript 导出文件与基线逐字一致。

保证范围是协作式 IPC 协议下的本地 safe Rust 所有权。Python exporter/释放同步仍属 R06，失败释放后的 owner/charge 策略仍属 R07；本项没有改变二者，也不声称能阻止不合作进程直接破坏共享内存。

## 基线与候选证据

基线先运行独立消费者编译探针：1 个合法正例通过，2 个借用/移动控制例按 E0499、E0502/E0505 拒绝，18 个漏洞反例均被旧 API 接纳。没有执行重叠引用。候选的同组门禁逐例检查隐私、移动、unsafe 或类型诊断，正式门禁使用重新写入源码的方式，不用缺依赖或错误导入冒充拒绝。

另用真实双客户端在基线确认：A 的预分配块被 B 发布时，即使坐标、代际及 allocation id 相同，也可能读到 B 的数据。测试先关闭连接和服务端再断言；修复后错池拒绝、原块释放与未派发行为通过。destroy 后 dedicated mapping 重绑用元数据实验验证，未构造 payload 引用；实验 backing 经原生 destroy 与 ENOENT 确认回收。

辅助诊断曾因 `copy2` 保留旧时间戳而可能复用 Cargo 结果，已废弃该组零散证据，改为逐例新写入并重验全部 21 条基线探针；最终只读 guard 反例另以相同显式类型复核，基线通过、候选只报 E0308。仓内正式门禁未使用该辅助复制方式。

| 最终候选本机验证 | 结果 |
| --- | --- |
| 完整 Core workspace，含 relay | 1716 passed；其中 relay 416 |
| Rust SDK all-features / PyO3 Rust | 19 / 8 passed |
| Python ordinary / portable / TypeScript | 1231 / 25 / 14 passed |
| 真实 Rust/Python direct/relay 矩阵 | 18/18，严格收据校验通过 |
| 真实 TypeScript direct/relay 矩阵及持久连接 | 12/12，严格收据校验通过；持久连接用例通过 |
| Node | 61 passed；复用依赖闭包逐字未变的本批结果，线上另从最终候选构建 |
| 唯一写编译门禁 | 21 条探针通过，计作 Core 中 1 个集成门禁 |

本机 Core 的 5 个既有 ignored 为 4 个由父测试驱动的子进程入口及 1 个特权跨用户用例；Python 5 个和 Node 1 个既有 Windows 专属 skip 由线上 Windows 执行。没有新增 skip、放宽期限或降低行为断言。Node 复用记录绑定 `baa3025d` 构建及包哈希，并核对其 Node/FFI/config/mem/local 源码与锁文件到最终候选完全一致。

[GitHub Actions 39523167030](https://github.com/world-in-progress/c-two/actions/runs/39523167030) 的 attempt 1 绑定最终候选及 FastDB `4f99f86a662b0e950a0dd29800c25a1c9fca4def`，9/9 作业通过。线上使用未重定向的候选源码；本机隔离副本仅含 5 处默认测试命名空间及对应期望的重定向。

Host 逐份校验了 5 份线上回执：两份源码 SHA 均与预期一致，适用门禁集合与仓内定义完全一致，118 个步骤全部退出 0 且子进程终止；保留文本证据的 854 个文件哈希一致。每份回执均通过 18/18 Rust/Python、12/12 TypeScript 矩阵校验，四个完整 scope 的 Core 日志均包含 R02 编译门禁通过。

| 平台 | scope | 通过步骤 | GitHub artifact ID |
| --- | --- | ---: | --- |
| Ubuntu 24.04 / Python 3.12 | full | 25 | 12311804290 |
| macOS 15 / Python 3.12 | full | 25 | 12313915411 |
| Windows 2022 / Python 3.12 | full | 26 | 12312444587 |
| Windows 2025 / Python 3.12 | full | 26 | 12312660379 |
| Ubuntu 24.04 / Python 3.14t | python | 16 | 12312468627 |

## 独立审查

新版 hey-my-buddy 0.50.0 多轮审查发现并关闭了预分配长度/错池、注入池与响应池可替换、普通只读 guard 可还原写锁的入口。首轮两处测试迁移错误也已修正：buddy/dedicated 释放断言恢复真实计数和 native 回收依据，请求转发的文本检查限定到请求段；Python 物化断言迁移到新签名，其错误阶段和禁止吞错断言保留。

早期候选 `047cd58b`、`baa3025d`、`858de53f` 均未作为交付验收；前两次 Actions 39470524323、39473694958 已取消。最终只读审查 `2f2e8712-8802-4a09-a0e5-57bc999fa37a` 固定输入 `2d1b1d10`，核对实际 `lock_api` 实现、不可恢复写锁的 guard 及原持锁/释放行为，无剩余阻断。Host 核对固定产物 `23f3b2d2-3fb5-4bc9-a63a-0f9b17d00e63`、源码及空补丁后验收，Buddy 工作树已回收。独立静态审查与 Host 运行验证分别记录。

## 同环境开销

Apple M1 Max、macOS 27.0.1 arm64、rustc 1.91.0，同一 benchmark 源码，基线与最终候选交替运行 9 轮，以下为各组中位数。分配循环包括 64 字节写/读与释放；这是局部微基准，不是整次 RPC 吞吐。为观察私有类型只临时增加 `size_of` getter，计时路径未变。

| 用例 | 基线 ns/op | 候选 ns/op | 中位数变化 |
| --- | ---: | ---: | ---: |
| 响应池只读 guard | 9.870 | 9.905 | +0.36% |
| 4 KiB 单块分配/访问/释放 | 219.292 | 229.690 | +4.74% |
| 64 KiB 单块分配/访问/释放 | 143.888 | 153.486 | +6.67% |
| 4 KiB 双块分配/访问/释放 | 296.722 | 312.188 | +5.21% |
| 64 KiB 重组、trim 至 60 KiB、复制/释放 | 2498.911 | 2518.934 | +0.80% |

分配循环增加约 9.6–15.5 ns，存在可测成本；只读 guard 与重组的中位数差低于 1%，样本区间重叠，不声明性能提升或零开销。pool 身份只在创建 pool 时增加一次受检原子序号更新；每次分配没有新增原子序号、堆分配或 payload 拷贝。

本机实际布局：`MemPool` 376→384 B，`MemHandle` 保持 80 B；`RequestReleaseState` 64→144 B，在原 Arc 分配内增加 80 B；`ResponseMeta` 32→88 B。`ReassemblyBacking`、`RequestData`、`RequestLease`、`ResponseData` 各 136→112 B，`ResponseLease` 144→112 B，`RequestBlock` 与 dedicated retire permit 均保持 8 B。这些是本平台布局，不能外推为其他平台的字节数。

## 实际增量与清理

按基线至本轮收尾的物理改动行统计，注释/空行随所在代码计数，Rust `cfg(test)`、test-support 与测试夹具计入测试：

| 分类 | 新增 | 删除 | 净增 |
| --- | ---: | ---: | ---: |
| 生产代码 | 951 | 747 | 204 |
| 测试 | 1483 | 957 | 526 |
| 文档 | 157 | 18 | 139 |

测试增量高于方案中 150–300 行估算，主要是沿完整 safe 调用链补齐 18 个独立漏洞探针及真实错池、发布、重连回归；没有新增测试依赖或框架。生产增量在原估算内，目的为封闭已复现的所有权缺口。

本次所有隔离命名空间 socket/lease 零残留，历史目录仅 lstat 元数据且保持不变，未扫描内部文件或进行冷切换。确认无构建、测试、runtime 使用后，删除本次 23 处 target、native、环境和打包产物，逻辑大小 4,149,257,422 字节（3.86 GiB）；文件系统空闲空间实际增加约 3.44 GiB。收尾另清理回执校验产生的 82,985 字节 Python 缓存，并原生归档本次隔离工作树；源码、Git、日志、回执和性能原始样本保留。没有为文档收尾重跑已通过的构建。

诊断证据位于 `~/.codex/diagnostics/c2-r02-implementation-20261021-1zef_zmt/`，包括最终源码/命名空间映射、基线逐例审计、测试日志及哈希、跨语言收据、CI 回执、性能样本、独立审查、端点验收和清理结果。CI 下载仅保留文本证据，包与 native 二进制不进入 Git。
