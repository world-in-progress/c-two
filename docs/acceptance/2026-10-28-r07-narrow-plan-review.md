# R07 收窄方案的独立审查与基线复核

状态：方案及基线阶段结束，等待用户确认实施；生产代码和正式测试源码均未修改，R07 尚未关闭，不发版。

生产基线 `ba6179a937d26419cf1d0bf28e0d74ba5867c256`，审查 workspace 输入 `d8ae4e764e5599d843380c852a49d96bfb6ac816`。方案见[当前 R07 方案](2026-10-28-r07-complete-release-followup.md)，基线见[固定收据](evidence/2026-10-28-r07-narrow-baseline/receipt.json)。方案修改后的生产目录仍与审查输入相同。

独立审查使用 hey-my-buddy skill 0.56.0，run `1502befc-9721-4630-9398-d1869a2c3adc`，Codex `gpt-6.1-sol / max`。初次路由排队后的停止仅形成部分调查；恢复后的完整审查和两轮差量复核分别覆盖 `02fb0b71`、`4dd29a57`、`84c9ecf3`。不能把前一份较大方案的审查结论挪用到本方案。

审查提出并落实的事项如下：

- helper 的 `Err` 不得与 `Ok(None)` 一起进入成功 fallback。
- Peer 的 trailing-inline、chunk 与拒绝丢弃清理共用原 carrier，并保留内部 FreeResult 供原 idle GC 使用。
- pending 的 `Ok(false)` 暂忙与实际释放 `Err` 分流，真实错误不得落入每 5 ms 的恢复循环。
- Python 3.10 也保留主异常及 cleanup 原因，不依赖 add_note 是否存在。
- dedicated 未 settle 可能一直保留至原 pool teardown，不把 read_done 写成必然能被 GC 退休。
- 补齐消费式跨 crate 终结链，消耗原 Option 向下 abandon，不再次 free 或执行 invalidator。

最后一项中，响应方向的接口链已由独立审查复核；请求方向 `RequestLease::abandon(self)` 与 CoreCallback 两个出口在审查最后指出后，由 Host 按具体建议补入 `e1bd9b33`，并对照 `host.rs` 的 copy 失败和 service.invoke 后释放失败两个出口逐项核对。这一最后的文档补充没有再跑一轮独立审查，不记作额外一次无意见审查，也没有修改 Host 析构或生产实现。

基线夹具初轮错误地把 C2_IPC_ROOT 当成 pair 的隔离设置，实际访问了默认命名空间；其临时端点由原生 pair close 回收，未手动清理历史文件。修正后的临时 feature 显式使用 with_unix_root context，四项红色反例重做结果一致，旧隔离声明已撤回。此处没有声称 Windows 实测或修复通过。

独立审查以不创建临时文件的只读命令重新计算了收据列出的 5 个证据文件及两个原生产函数的 SHA-256，全部匹配；此前 here-document 在只读权限下失败的尝试不计验证通过。Host 同时检查固定补丁可应用、链接和文件哈希、显式 context、socket/lease 零残留及真实清理收据。

只读产物 `e93b0c79-ca4f-4bbd-8b41-c5a4508496b9` 的 sealed patch 已实际读取，为空，SHA-256 为 `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`。Host 接受调查和修订建议并明确记录最后补充由 Host 核对，工具状态为 accepted/completed，审查 checkout 已回收；这不是用户实施确认或发布批准。

两次隔离 Cargo target 分别删除 458964090 和 458971693 字节；source/endpoint 临时目录也已移除，保留文本日志与补丁。没有清理默认命名空间或历史目录。实施后的完整 relay、跨语言、三系统、性能对照及确切候选 GitHub Actions 尚未执行，按当前方案在获得实施确认后验收。
