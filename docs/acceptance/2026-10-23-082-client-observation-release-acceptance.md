# 0.8.2 relay 观察恢复与客户端连接事件验收

[C-Two 0.8.2](https://pypi.org/project/c-two/0.8.2/) 与 [c3 0.4.1](https://github.com/world-in-progress/c-two/releases/tag/c3-v0.4.1) 已发布。发行来源为 `2aa60fe0d259aa3364b4d49fc74797672decab66`，来自 [PR #195](https://github.com/world-in-progress/c-two/pull/195)，其树与最终 PR 候选 `c94aa83c1c7bd7cfb6dce4504ff34e4fe1ced990` 相同。本轮基线为 `7ff2bf512f0d8d151d32c5e90929ba66deda47ba`，生产实现为 `fc8ad41d0761ed1cfe578d892d6a78be856ff971`。本验收及后续纯文档提交不是发行产物来源。

R18 单列正确性修复：到达 1024 owner 容量时保留 unavailable，复用已有一秒重试；永久生命周期关闭才结束 watcher。任务与 unavailable 记录共用既有锁及任务 token，但记录仍有独立寿命，stop 不清空它。unpublished/closing 计数、容量上限、路由身份、代际和释放顺序保持，没有新增管理器或重试策略。

客户端和服务端共用 Runtime 原有连接事件源。`cc.connection_events()` 观察受管理 outgoing 和 accepted 物理连接，以 `side` 区分，ID 与 observation 相同，终态来自原接收/心跳/关闭权威。Python 的阻塞等待统一为释放 GIL 的 `ConnectionEvents.recv()`，`close()` 可从另一线程唤醒；删除 Python `wait_closed()` 和专用 Core 同步桥。保留 snapshot、原生异步关闭屏障和有界 1024 条队列，未新增逐连接线程、reader 或轮询。逻辑代理关闭不冒充物理断连。

公开 `c_two.error.CoreError`，继承 `CCBaseError` / `RuntimeError`，字段完整声明且逐实例存在。删除 `_native.CoreError` 导出，无别名。原始外层消息、semantic bytes、OS 原因与重试分类保持；Core transport_phase 的两种分类与对端任意 semantic 诊断值明确区分。因公共基类扩大，调用包装处改为仅让既有 `CCError` 原样穿过，避免 CoreError 绕过原有 call-stage 包装。

IPC 12、SHM 3、C/FFI ABI 5、owner-control 1、业务帧和 CRM 哈希不变；FastDB 保持 0.2.3。R16 和旧 ShmBuffer 的先前独立验收随本次发行交付，其增减不混入本轮统计。R06/R07 和两份控制会话重构继续暂缓。

## 基线与本机证据

原发布版 0.8.1 的隔离三服务实验得到不同 connection_id，但客户端 aggregate feed 无打开/关闭事件；旧 wait_closed 收到 peer_eof，逻辑 close 后继续等待，shutdown 才返回 local_close，公开 CoreError 缺失。R18 的真实满容量反例在旧生产实现上失败于“容量归还后原 upstream watcher 未恢复”，收尾仍排空 owner 与私有端点。

macOS 定向 Python 231 项通过、6 项 Windows 专属场景条件跳过；真实握手失败/期限到期成对事件、重连代次、服务端构造、R18 满容量/状态/清理以及 native Rust 8 项通过。后续错误任意语义细节、copy/pickle、并发 recv/跨线程 close 均有实际回归。测试入口修正后 83 项通过、1 项既有 Windows runner 专属条件跳过。

Windows 11 x64 开发机在 `dac43117` 执行，其生产树与最终候选相同：Python 定向 221 通过、16 项 Unix 条件跳过；Rust relay 412、Python relay 54、portable 跨语言 39、native Rust 8、Node 59、真实持久 Node 2 通过，保留 18 行 Rust/Python 及 12 行生成 TypeScript 收据。45 份证据逐一核对大小与 SHA-256，任务进程和 lease 检查为零；Named Pipe 的关闭由原生关闭及夹具收尾断言证明。随后移除 3,879,279,689 字节任务产物，保留已安装的 Rust 工具链。

Windows 初始 native test 未继承 Python 安装路径，复用 SDK 解释器增加 18 行测试入口，统一 base prefix、模块路径及 Windows DLL PATH；这属于测试接入。CLI lock 版本漏同步造成一轮构建失败，已修正为 0.4.1。开发机缺少 CMake/Ninja/hey 和源码 Git 元数据时，只补齐隔离前提后重跑受影响项，未重复已通过的整套测试。

## 完整门禁与独立审查

最终 [PR Source Validation](https://github.com/world-in-progress/c-two/actions/runs/39938148830) 绑定 `c94aa83c`，10/10 job 成功；[主线 Source Validation](https://github.com/world-in-progress/c-two/actions/runs/39942349170) 绑定 `2aa60fe0`，也为 10/10 job 成功。两轮均包含 Linux、macOS、Windows 2022/2025、原生 Windows ARM64 与 Linux Python 3.14t，完整 relay、跨语言与普通用户消费者按适用平台执行。发行阶段继续绑定同一主线来源：

| 阶段 | 实际结果 |
| --- | --- |
| [Release Candidate 39948762550](https://github.com/world-in-progress/c-two/actions/runs/39948762550) | 38 job 成功，1 个既定的 Linux ARM64 重复 sdist 分支跳过 |
| [Python 发布预演](https://github.com/world-in-progress/c-two/actions/runs/39951202646) / [CLI 发布预演](https://github.com/world-in-progress/c-two/actions/runs/39951205132) | 两份计划成功校验，按 dry-run 设计不上传 |
| [Python 正式发布](https://github.com/world-in-progress/c-two/actions/runs/39951565265) / [CLI 正式发布](https://github.com/world-in-progress/c-two/actions/runs/39951570298) | 各 2/2 job 成功，直接晋升已验证字节 |

PyPI 的受保护环境审批绑定已核对的来源、清单及此前用户的发布授权。

前一轮 CI 39931323867 的 Linux/macOS/3.14t 通过，三个 Windows 作业的 pytest ordinary 均退出 0，但 strict audit 拒绝新增 Unix wrapper 的未登记 skip。候选把两个平台 wrapper 合并为已含平台分支的一个真实测试，保留全部错误/关闭/清理断言，删除退役 skip 许可项，净减 11 行。没有放宽断言、跳过测试或靠重跑掩盖失败。

hey-my-buddy 0.51.1 的固定产物独立审查已接受：完整实现复核修正 transport_phase 类型说明、服务端 side 和真实边界覆盖；最终测试入口再做两文件只读复核。审查与执行验证分别记录；只读审查未代替本机、开发机或 CI 测试。

## 实际增量

| 范围 | 新增 | 删除 | 净量 |
| --- | ---: | ---: | ---: |
| 生产代码 | 322 | 117 | +205 |
| 测试与测试入口 | 889 | 202 | +687 |
| 文档 | 236 | 94 | +142 |
| 版本、锁文件与测试依赖 | 13 | 4 | +9 |
| 合计 | 1460 | 417 | +1043 |

统计绑定基线至 c94aa83c；Rust cfg(test) 按测试归类，生产文件内 API 注释计入生产，不含后续验收文档。R18 子项生产 +70/-21（净 +49），测试 +368/-23（净 +345），另一个测试依赖条目。本轮是正确性与接口补全，不声明瘦身或性能提升。发版收束的清单、changelog、验收文档与机器收据另新增 366、删除 14，净增 352 行，未再修改生产代码或测试。

## 发行与清理

候选清单 SHA-256 为 `f4fd1c94aa51b8a8b369c0a06765e70e5422e308daec2fbf900763902ede9720`。它包含 35 个 wheel、1 个 sdist、6 个 CLI、2 个安装器、9 个 FastDB 证明文件和 56 份收据。PyPI 全部 36 个文件及 GitHub 全部 17 个附件的名称、大小与 SHA-256 均与晋升计划一致；无 yanked 文件。Windows ARM64 的 cp311、cp312、cp313、cp314、cp314t 五个 wheel 齐全，其他平台继续覆盖 3.10。实际 `c3-v0.4.1` 标签也指向上述发行来源。

在源码目录外的全新 Python 3.13.3 环境，从官方 PyPI 只安装二进制 wheel，确认包来自该环境、版本为 0.8.2、未加载 FastDB/NumPy/PyArrow。一个消费者观察三个真实服务的 opened；杀死对端与正常关闭均得到 peer_eof，本地 shutdown 得到 local_close，事件与 snapshot 一致；逻辑代理 close 未伪装成物理关闭。公开 CoreError 捕获、完整字段、copy/pickle、旧接口缺失及 socket/lease 零残留均通过。

日志、摘要和收据保留，已验收的 Buddy checkout 已回收。当前任务的 Cargo target、临时源码归档、输入包、native 扩展及临时 wheel 安装环境已分批清理；最后一次本机清理另移除 45,066,538 字节。原有 Python 开发依赖保留，native 扩展下次使用须重新构建。旧 `slimming-batches` worktree 被应用确认属于另一任务，归档被拒绝，故保留其源码与 `socu/076-ci-release-gates` 分支。本轮已完成的 R16、ShmBuffer、0.8.2 验证分支已清理。历史 IPC 目录不清理、不迁移，只处理本次精确识别的端点。

用户随后要求紧急释放磁盘空间：确认无活动编译或打开文件后，删除 9 个闲置 Cargo target，按磁盘占用计 2,006,691,840 字节（1.87 GiB），包括旧验证、临时目录及 uv 缓存中的派生产物。源码、Rust 工具链、日志及历史 IPC 记录保留；当时可用空间由约 7.6 GiB 回升到 9.7 GiB。该额外清理与当前实现增量分开记录。

结构化来源、发布文件核对与清理结果见 [evidence](evidence/2026-10-23-082-client-observation-release-acceptance.json)。本记录与清单以独立纯文档提交合入，随后同步本地、fork 和 upstream；同步不改变已发布包的源码来源。
