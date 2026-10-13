# 原生构建与测试复用：上游能力和 C-Two 试验边界

日期：2026-10-12。范围为 Rust Core / CLI / PyO3 扩展及 FastDB C++/CMake/SWIG 的 GitHub Actions 构建、测试；以下是文档与当前工作树的核对，不是性能实测。PR #182 的 CI 6m36、RC 16m37、Windows 16m52、49 个 job 全绿是已有基线，后续收益必须用同源、同 runner 类别的重复运行验证。

## 编译缓存：先补 C/C++ 编译入口，再测命中率

**上游已说明。** sccache 通过 `RUSTC_WRAPPER` 包装 Rust 编译，也支持 C/C++ 编译；Rust 的 `bin`、`cdylib` 等调用系统链接器的 crate 及增量编译 crate 无法缓存，默认绝对路径不同也会降低命中率。它的 Rust key 包含工具链路径、sysroot 和参数，C/C++ key 包含预处理结果、编译器和参数等；因此缓存不能当作跨 OS、目标三元组、MSVC 工具链或不同 ABI 的构建产物共享机制。[sccache README](https://github.com/mozilla/sccache/blob/main/README.md)、[sccache 缓存键](https://github.com/mozilla/sccache/blob/main/docs/Caching.md)。CMake 的 `CMAKE_C_COMPILER_LAUNCHER` / `CMAKE_CXX_COMPILER_LAUNCHER` 在目标创建时初始化对应属性，Ninja 与 Makefile 生成器会用其包装编译命令；这是把 FastDB C/C++ 编译接入 sccache 的正式入口。[CMake compiler launcher](https://cmake.org/cmake/help/latest/prop_tgt/LANG_COMPILER_LAUNCHER.html)。

**当前证据与 C-Two 假设。** `windows-native.yml` 已在原生门禁设置 `RUSTC_WRAPPER=sccache`；RC wheel 的 maturin action 已设置 `sccache: true`；Windows 的 `rust-cache` 覆盖 Core、CLI、Rust SDK、Python native 和 FastDB Rust 各自 target 目录。当前 C-Two 工作流及 `tools/ci/windows_native.py` 未显式配置 CMake 的 C/C++ compiler launcher，故 *FastDB C++ 编译可能仍是未覆盖的一段*；需核对实际 FastDB configure 日志和编译命令，不能仅凭 Rust wrapper 推断。第一项窄实验是在 FastDB 原生 CMake 配置阶段传入两个 launcher 参数，仅对该 build tree 生效，并记录 `sccache --show-stats` 的 C/C++ 请求、命中、不可缓存原因以及 configure/compile/link/SWIG 各段耗时。SWIG 代码生成、系统链接、wheel 修复和测试执行本身不因编译对象缓存而消失。Core 的 `Cargo.toml` 明确配置 release `lto = "fat"`；没有证据可把该配置推及独立 CLI 或 Python native manifest。fat LTO 的最终链接尤其需要单独计时。

`Swatinem/rust-cache` 的 `workspaces` 映射决定保存哪些 target 目录，默认 key 包含当前 rustc、Cargo 文件及 `CARGO`/`CC`/`CXX`/`CMAKE`/`RUST` 前缀环境；`shared-key`、`cache-targets` 和保存策略均可配置。[rust-cache 官方 README](https://github.com/Swatinem/rust-cache)。C-Two 应先记录每个 job 实际 restore key、target 路径、下载/上传时间和缓存大小：独立 Cargo target 目录、feature/profile/ABI 差异和频繁变动的 key 都可能让“已有缓存”仍反复编译。不要把不同工具链或 Python ABI 的 target 目录硬合并。缓存配置调整以“编译耗时 + 缓存传输耗时”的净值衡量；若 sccache 报路径相关 miss，再评估 `SCCACHE_BASEDIRS`，先证明不会混淆不同源码树。[sccache README](https://github.com/mozilla/sccache/blob/main/README.md)。

## Rust 测试：构建一次后按实际资源边界分发

**上游已说明。** `cargo nextest archive` 可保存测试二进制、相关动态库、非测试辅助二进制和部分 build-script 输出；`run --archive-file` 可在另一 job 使用它。运行端需同一源码 revision，并自行传送 archive，建议固定相同 nextest 版本；源码 fixture 和外部运行环境仍需存在。[nextest 归档](https://nexte.st/docs/ci-features/archiving/)。`--partition slice:m/n` 按已筛选测试轮流分配，`hash:m/n` 提供更稳定但小集合可能不均匀的桶；测试组可用 `max-threads` 限定一类测试的并发，`threads-required` 占用全局及组内容量。[nextest 分片](https://nexte.st/docs/ci-features/partitioning/)、[nextest 测试组](https://nexte.st/docs/configuration/test-groups/)。

**C-Two 适用性假设。** 优先在耗时相同的 Rust 测试集合中，按 manifest、OS/架构、feature、profile 建一份 archive，再比较归档上传/下载与当前重复编译时间；Core、CLI、Rust SDK、Python native 是不同构建单元，不能直接声称“一份归档供全部 49 job”。需要 IPC endpoint、共享内存、relay 和生命周期的用例，应按资源冲突范围配置组/线程配额；组只约束同一个 nextest 进程，跨 job 的端点隔离仍靠 runner 与测试地址。先以单 OS/单构建配置对比 `cargo test` 的完整测试清单、结果与 nextest 清单，确认 doctest、外部 fixture、动态库及辅助二进制没有缺口，再考虑分片；archive 节省的是重复测试编译，不能代替各 OS 上运行测试或现有 wheel 安装后的真 IPC/relay 证明。

Python 侧已有 `tools/dev/test_python.py`：普通 pytest 文件可用有界 xdist `--dist=loadfile`，portable 18 行与 TypeScript 12 行矩阵保持串行且完整；正常 `ci.yml` 目前仍直接调用 pytest。可把这个既有入口作为单独 CI 实验，严格保留矩阵收据与 Python 3.10…3.14/3.14t、Windows 2022/2025 的实际运行证据。`docs/reports/2026-10-02-python-parallel-validation.md` 是历史本机证据，不能外推新的 CI 收益。

## ABI：条件性减少常规 wheel，3.14t 仍单独构建

PyO3 0.29 文档建议：启用 `abi3` 并以最低支持版本构建，可让一个常规 CPython wheel 覆盖多个版本；但在 Python 3.14 free-threaded 上，`abi3` 会退回版本专用扩展，需 `cp314t` wheel。`abi3t` 从 Python 3.15 起才覆盖 free-threaded 与常规解释器，且需要相应 PyO3/maturin 支持与更受限 API。[PyO3 0.29 构建与分发](https://pyo3.rs/v0.29.3/building-and-distribution/)。C-Two 当前 PyO3 依赖为 0.29、maturin 为 `>=1.7,<2.0`，RC 逐 ABI 构建并逐行安装运行。试用 `abi3-py310` 之前必须审查 C-Two native 与依赖的 Limited API 兼容性、wheel 标签和 FastDB 对各解释器的包可用性，并在每个声明支持的 CPython 版本安装同一 wheel 运行真功能；3.14t 的独立 wheel 与运行门禁保留。由此可减少多少构建行，是完成上述审查和测量后的项目结论。

## 建议的实验顺序

1. 取同源基线，按 job 分出 FastDB C++、Rust compile、link/LTO、SWIG、wheel、测试及缓存传输时长，记录 sccache 命中与不可缓存原因；先评估 CMake launcher 的局部接线。
2. 在一个 Rust OS/feature/profile 组合中试 nextest archive 与资源组，对照原 `cargo test` 清单及真测试结果；只有归档传输加分片后的关键路径更短才扩大覆盖。
3. 独立验证 `abi3-py310` 的 API、wheel 标签和所有常规 CPython 运行结果，维持 `cp314t`；以净构建时间、包体和下游 FastDB 可安装性判断是否改 RC 矩阵。

上述实验均不能以编译缓存命中、二进制构建成功或归档测试成功，替代 Windows 两代 runner、各 OS/ABI 安装后 IPC/relay、共享内存和生命周期的现有运行证据。

Host 补充核对：C-Two 当前直接使用 `pyo3::buffer::PyBuffer` 与 `ffi::Py_buffer`。PyO3 0.29.3 的 [buffer 模块](https://github.com/PyO3/pyo3/blob/v0.29.3/src/buffer.rs#L1-L3) 以 `any(not(Py_LIMITED_API), Py_3_11)` 为编译条件，CPython 的 [Py_buffer Stable ABI](https://docs.python.org/3/c-api/buffer.html#c.Py_buffer) 也从 3.11 起。因此前述 `abi3-py310` 是通用工具能力，不是当前 C-Two 可直接开启的选项；在维持 3.10 要求下需处理这项明确的 API 约束。已有 Windows FastDB wheel 构建日志记录 Visual Studio 生成器，CMake launcher 方案也应先在 Make/Ninja 实际配置上验证。
