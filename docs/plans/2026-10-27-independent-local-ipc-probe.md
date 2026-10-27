# 独立本地 IPC 探测草案（已撤回）

状态：未实施；用户要求直接公开现有独立 Runtime 能力后，由[公开 Runtime 方案](2026-10-27-public-independent-runtime.md)替代。原草案的源码记录为 `a80380db`，仅作历史设计证据。

退出实施范围的内容包括专用 `probe_local_ipc` API、raw connect/accept 探测流程、专属结果数组、一次性 namespace 的 gate/marker 终结和同步 worker 适配。它们没有成为当前公开能力或兼容入口。

新方案公开现有实例方法，并让模块级 API 委托默认实例。预检复用实际 Host 启动与既有 PING/PONG；临时目录由创建它的调用方在确认原生关闭及相关清理成功后处理。详见替代方案中的 API、所有权、回归范围及基线验证门槛。
