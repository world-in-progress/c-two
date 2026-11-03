# Runtime 迟到连接：受控基线反例

本轮仅做反例与方案，生产代码未变。源码 `9e8da3aacbfe64f704279a61a1c7f7198574d6c0`，生产实现来源 `e0b5af2d8aabd225dc286d75e5d1ef5c6b492855`；安装的是该 checkout 本次构建的非 editable wheel，不能把包内 0.9.0 元数据当作发布版来源。平台为 macOS 27.0.1 arm64 / CPython 3.12。原生库 SHA-256、探针哈希及逐项事实见[机器记录](evidence/2026-10-28-runtime-late-connect-baseline.json)。

## 固定交错

测试以 Event 将 worker 停在 Python 已捕获 NativeSession、尚未调用真实 acquire 的位置。主线程完成公开 Runtime.shutdown 并确认换代后才释放它。额外两行在真实 native acquire 完成后阻断 Python 最终发布，再用原 ConnectAttempt 在一个已持有的锁上等到其原期限耗尽。没有改时钟、延长期限或伪造 Native 结果。

保留旧 session、返回代理或错误 traceback、只读 connection observation，直到真实清理完成。这样不能因 Python 垃圾回收恰好销毁旧 Runtime 而掩盖问题。每个用例最后由真实 peer 的关闭屏障收完泄露连接，再显式关闭两个 Runtime 与旧 session。

| 旧 Runtime | connect 预算 | shutdown 已报 completed/drained | connect 结果 | 逻辑 close 后物理关闭 | 再观察旧 session.shutdown 后物理关闭 |
| --- | --- | --- | --- | --- | --- |
| hostless | 尚有剩余 | true / true | 成功，真实 echo RPC 成功 | 否 | 是 |
| hosted | 尚有剩余 | true / true | 成功，真实 echo RPC 成功 | 否 | 否 |
| hostless | 最终发布处耗尽 | true / true | 原 CallDeadlineExceeded，stage=python_connect_finish | 否 | 是 |
| hosted | 最终发布处耗尽 | true / true | 原 CallDeadlineExceeded，stage=python_connect_finish | 否 | 否 |

最终命令为隔离解释器运行 `python -m pytest sdk/python/tests/unit/test_runtime_shutdown_publication.py -q -s --timeout=30 --basetemp=<task-temp>/pytest-final`，结果 **4 failed in 6.17s / exit 1**，全部失败在目标不变量断言；这是基线红灯证据，不能列为修复通过。完整失败输出见[日志](evidence/2026-10-28-runtime-late-connect-baseline.txt)。四项 finally 均成功，私有 endpoint root 中只剩 .gate/.gate.marker，socket/lease 为零。

最初构建因本机 C++ 标准头搜索路径失败，后续只为本批构建设置当前 SDKROOT/CXXFLAGS 后成功。第一次 hosted 用例误选了启动探活的 server 事件，修为选择 client side；期限用例随后把原有 CallDeadlineExceeded 与物理关闭断言分开。最终四行是在同一份测试源码上重跑得到，不把编译/夹具失败当作功能反例。

## 调用链结论

`registry.Runtime._connect` 出实例锁后保留旧 NativeSession；`Core.Runtime.acquire_ipc_client_with_deadline` 可再次取得该 Runtime 的可重开 pool；`ClientPool.close_all` 返回后递增 epoch 并恢复 acquire。`CRMProxy.terminate → PyCoreClient.close → ClientLease::drop` 只还引用，没有物理关闭屏障。有 Host 时 `Host.shutdown_with_timeout` 可直接返回缓存 terminal journal，无法覆盖之后重新进入池的连接；hostless 分支每次执行原缓存 drain，因此第二次关闭能补收。

源码还确认：relay fast path 与直接 IPC 共用同一个 Runtime pool；原 `FreshClientGuard`、Preparing/UnconfirmedIdle record 和 bounded settlement 已提供失败连接的精确 owner。所缺的是 Runtime 退役范围与这些既有 owner/事务的衔接，不应在 Python 异常分支再堆一份缓存关闭循环。

[修订方案](2026-10-28-runtime-retirement-core-plan.md)把预算耗尽、pending 持有/驱动/观察明确分开。Windows/Linux 本轮尚未跑此反例；不会以本机记录替代三端实现验收。确认后修复仍需完整 relay、跨语言、三端、GitHub Actions、零残留和独立审查。

本轮确认测试/构建进程已退出后，删除任务私有 target 与环境，共 434,855,936 字节（约 415 MiB）；再移除只剩 gate/marker 的四个任务私有临时 namespace 容器。源码、日志、哈希和回执保留，历史 namespace 未触碰。
