# R33 Python 手工分片包装退役验收

用户批准退役 `c_two.mem.ChunkAssembler`、`c_two.mem.MemHandle` 及对应 `_native` 导出，不留兼容。实现提交为 `921b48ee7e54f5b88cafff5e4e19a31d47d5d545`，基线为 `ed6c761abcb2fcca7ccfac4b5cd89cd87762a58d`，对应[批准方案](2026-10-24-r33-python-manual-assembly-retirement.md)。Rust `ChunkRegistry → ChunkAssembler → ReassemblyBacking`、Rust MemHandle 及正式 IPC 行为保持。R35 继续暂缓，该独立批次未发版，现已随 [0.9.0](2026-10-25-090-windows-release-acceptance.md) 发布；本记录中的协议编号对应当时的独立实现基线。

## 替换与实际净量

| 范围 | 新增 | 删除 | 净变化 |
| --- | ---: | ---: | ---: |
| 生产 | 1 | 272 | −271 |
| 测试 | 117 | 332 | −215 |
| 生产与测试合计 | 118 | 604 | −486 |
| 文档，含方案、清单及本记录 | 120 | 8 | +112 |

按基线到本轮收尾的 Git 物理行统计，包含注释和空行。生产变化仅为两个 PyO3 包装、其状态、注册和 import/__all__，以及唯一服务该手工入口的私有 pool_arc 访问器；PyMemPool 的 Arc/RwLock 容器和其余公开方法保留。Rust 正式分片三个文件仅修改 #[cfg(test)] 区，生产部分逐字节一致，其他 Core、Rust SDK、C/FFI、wire 和配置文件未改。

这是经明确批准的 Python 公开能力退役：手工构造 assembler、feed/finish/abort 及结果 handle 的裸地址、write_at/release 不再提供，Python 没有等价的兼容入口。Rust 低层 API 与 Python RPC 自动分片继续可用；Python 内存池、统计、ResponseBuffer、HeldResult、FastDB/pickle 路径保留。C/FFI ABI 5、IPC 12、SHM 3、owner control 1、业务帧与 CRM 哈希不变。本项不声明 RPC 延迟、吞吐或 RSS 改善。

## 基线与覆盖迁移

删除前，原 Python 手工链 19 项全部通过。三个 Rust 测试文件补齐后、生产仍为原实现时，c2-wire 164 项通过，证明提交为 `80925057a9692b889618a09d1cde85501715f448`。导出退役断言在旧扩展上按预期失败于名称仍在 mem.__all__；候选重建后该断言及剩余内存池行为通过。

基线新增覆盖包括空数据、1 字节步长、100 片、1 字节短尾、逆序、重复片拒绝后 received 不变且仍可继续完成、部分接收后 abort 释放、同池同连接两请求交错隔离，以及文件/buddy backing 已释放后拒绝写入。Host 将交错用例调整为第一份结果先释放，第二份尚未完成的请求随后继续接收并完成，检查完整内容、独立收费及最终零分配。旧 Python 元数据 setter、consumed/repr 和已退役类型的外观断言随 API 退出，没有在 Rust 重建同一包装。

| 环境与范围 | 实际结果 |
| --- | --- |
| macOS 基线 Python 手工链 | 19 passed，0 skip |
| macOS 原 Rust 生产实现上的迁移覆盖 | c2-wire 164 passed，0 ignored |
| macOS 候选内存池、预算、held、生命周期 | 74 passed，0 skip |
| macOS Rust/Python 跨语言 | 25 passed，18/18 direct/relay receipt |
| Windows 11 x64 开发机 wire/memory | c2-wire 164 + c2-mem 182 passed，0 ignored |
| Windows 11 x64 候选 Python 内存回归 | 74 passed，0 skip |
| Windows 11 x64 完整 relay | Rust 412 + Python 54 passed，0 skip/ignored |
| Windows 11 x64 Rust/Python/Node 及持久连接 | 41 passed，0 skip；portable 18/18、TypeScript 12/12 |
| GitHub Actions 确切候选 | [首轮 10/10 成功](https://github.com/Dsssyc/c-two/actions/runs/40053047208)，Linux/macOS/Windows x64/ARM64，含 3.14t |

Windows 开发机检出与确切候选一致，使用 rustc 1.99.1、Python 3.12.13、Node 22.23.4。26 个日志和 receipt 已下载，逐一校验大小与 SHA-256；打包证据 SHA-256 为 `739f60b3a0b86218d924bd5813024a3dba667267909e1d146bd061c6514ea5a2`。两种跨语言矩阵使用仓库原严格校验器验收，未跳过测试或放宽断言与时限。完整 macOS/Linux 源码及 relay 由干净 CI 执行，本机没有通过全局 C2_IPC_ROOT 覆盖来运行默认命名空间的 Rust relay 套件。

## 独立审查与清理

hey-my-buddy 测试补丁 run `3f0d5489-92c8-42f3-8b87-806e716d26e4`、artifact `8102b5f8-bc03-466d-afa0-76c1d82060d7` 已由 Host 核对、调整上述顺序并实际测试后接受。独立实现审查 run `10cc6170-3e3c-45d4-977a-aba8ed8b4004`、artifact `c32986f7-f160-417e-a2b3-1c8d623e6b87` 绑定确切候选，Standards/Spec 均无 P0–P3 发现；Host 已验收报告并回收两个工作区。独立审查是静态检查，运行结果由 Host 分别核验。

macOS 本批隔离端点及测试临时目录 socket/lease 为 0；Windows 本批 Named Pipe 与所属进程为 0，历史目录未清理或冷切换。本机 Cargo target、target 链接、临时原生扩展、FastDB CoreSDK 和 Go 构建缓存已清理；后续原生测试需重建。Windows 本批 D 盘目录与临时目录已删除，回收 3,063,716,201 个逻辑字节，保留原 Rust 工具链。原始失败/成功日志、源码补丁、哈希和 receipt 留在 `~/.codex/diagnostics/c2-r33-retire-0637wem8/`。

私有准备脚本最初把 FastDB SDK 输出与日志放在同一目录，被现有非空目录检查拒绝；分开目录后准备成功，没有进入或重跑测试。Windows PowerShell 日志按 UTF-16 BOM 解码后完成统计；原字节和哈希保留。Go 模块缓存清理需要先恢复本批目录的 owner 写权限，只处理本批缓存。这些验证/清理接入修正没有改变生产代码或验收条件。

仅向 fork 的本次 CI 分支推送候选，没有推送 main、上游、标签或发布包。最终实现提交与 CI、独立审查相同，收尾另记文档提交。
