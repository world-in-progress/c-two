# 多平台 CI 优化：社区实践与 C-Two 的适用方案

日期：2026-10-12。对照 C-Two `e42c64e3a464eb9d836de0f4687ac480d5fb8ddc` / PR #182。本文记录调研与候选实验，不代表这些新方案已经实施或取得提速。

社区已有与本仓库十分接近的做法。对当前瓶颈，优先级是：按目标平台组织依赖链、将 macOS 的多个 ABI 构建组成适当大小的任务、补齐 C/C++ 编译缓存。PR 精简矩阵、nextest 归档、Stable ABI 也有成熟工具支持，但分别涉及验证发生时间、测试执行方式和扩展 API，需要单独评估。

## 当前测量说明了什么

本轮完成的优化把 62 个 job 减为 49 个；累计 runner 时间从 165.6 降至 152.8 分钟。最终 [Release Candidate](https://github.com/world-in-progress/c-two/actions/runs/38231359378) 仍需 16 分 37 秒，最长单个 macOS job 排队 9 分 12 秒；[Windows Native](https://github.com/world-in-progress/c-two/actions/runs/38231359372) 为 16 分 52 秒。它们是同一天的成功运行样本，包含机器与排队差异，不能把观察到的变化全部归因于某一个配置。

GitHub 当前公布的 Free、Pro、Team 默认 macOS 并发上限均为 5，且普通与 larger macOS runners 共享这个上限。本仓库的执行波次与这种限额相符，但本次没有确认组织实际套餐或特殊额度。仅增加矩阵行或更换 larger runner，不保证增加并发槽位。[官方限额](https://docs.github.com/en/actions/reference/limits#job-concurrency-limits-for-github-hosted-runners)

源码还存在可以直接解释的依赖等待：[native wheel job 的 `needs: [context, cli]`](https://github.com/world-in-progress/c-two/blob/e42c64e3a464eb9d836de0f4687ac480d5fb8ddc/.github/workflows/release-candidate.yml#L460-L461) 要等全部五个 CLI 子任务完成。该次运行中，相对于各自 CLI 完成时间，等待最慢 CLI 又占用了 macOS ARM64 139 秒、Windows 96 秒、Linux ARM64 108 秒、Linux x86_64 60 秒。这是可提前开放下游任务的窗口，不等于最终总时间一定缩短相同秒数。

## 有源码或正式文档支撑的做法

| 做法 | 社区证据 | 在 C-Two 中的判断 |
| --- | --- | --- |
| 一个目标平台 job 构建多个 Python ABI | [Pydantic 的 `core-build`](https://github.com/pydantic/pydantic/blob/3eca5695dabc5a451ccfc0f4afa802166e61e27a/.github/workflows/ci.yml#L79-L203) 用一条 maturin 命令传入多个解释器；[cibuildwheel](https://cibuildwheel.pypa.io/en/stable/options/#test-command) 提供逐 wheel 安装测试钩子 | 可先试 macOS 每个架构两个分组，每组三个 ABI。macOS wheel job 可由 12 个变为 4 个；仍产出全部 12 份 ABI 证据。单独实施此布局时总 job 数可由 49 变为 41，这是数量推导，尚无耗时实测。 |
| 构建产物与安装验证分开管理，但按平台就近执行 | [Polars 的发布流程](https://github.com/pola-rs/polars/blob/6673f6d371951b6c712d656a6851ad1d5f389863/.github/workflows/release-python.yml#L342-L374) 在平台构建 job 内安装 wheel 并检查构建提交；Pydantic 则有下载 wheel 后的独立消费者 job | #182 已采用相关思路。借鉴编排方式时保留本仓库更严格的逐 ABI、精确字节、IPC/relay 和生命周期证据；上述项目的 smoke 范围不能直接等同我们的 30 行验证。 |
| 各目标有独立依赖链，最后统一汇总 | GitHub 支持以 [matrix 调用 reusable workflow](https://docs.github.com/en/actions/how-tos/reuse-automations/reuse-workflows#using-a-matrix-strategy-with-a-reusable-workflow)，依赖由各 job 的 `needs` 指定 | 可组织为每目标的 CLI → wheel/ABI → smoke，最后 manifest 等待全部目标。这样 Windows/ARM64 不必先等 Intel macOS CLI。可复用 workflow 本身不减少 runner；收益来自依赖关系改变。 |
| 普通 PR 与完整发布矩阵分层 | [Pydantic](https://github.com/pydantic/pydantic/blob/3eca5695dabc5a451ccfc0f4afa802166e61e27a/.github/workflows/ci.yml#L79-L82) 在 main、tag 或 `full build` 标签下做宽构建；[uv](https://github.com/astral-sh/uv/blob/139d0914ea9563ddcd7071ed285f48d56f1ed4f4/.github/workflows/plan.yml#L220-L245) 由计划结果和标签决定 release binaries 构建 | 这能减少反复推送时全矩阵的运行次数，但会改变何时取得全量证据。若仍要求合并前全量，应把完整门禁放在合并队列或明确的预合并验证中，不能直接移到合并后并宣称保障不变。 |
| Rust 测试构建一次，再分发执行 | [nextest archive](https://nexte.st/docs/ci-features/archiving/) 可保留测试二进制及相关文件；支持 [分片](https://nexte.st/docs/ci-features/partitioning/) 和 [资源组](https://nexte.st/docs/configuration/test-groups/) | 适合重复构建相同 OS/manifest/features/profile 测试集时试点。不能跨 OS 共享测试二进制；还要对齐 doctest、fixtures、动态库、辅助程序和 IPC/SHM 资源隔离，并计算归档传输的净成本。 |

Windows 分组有一个具体约束：[maturin-action 官方 README](https://github.com/PyO3/maturin-action/blob/acb699e6734322acb57a0b1a5b4b62d01d68156d/README.md#windows-build-the-regular-and-free-threaded-interpreters-in-separate-jobs) 指出，同一次 `setup-python` 安装同版本普通与 free-threaded CPython 可能失败，建议拆 job。因此先在 macOS 试分组；Windows 的 3.14t 保持独立，除非另行证明解释器安装与选择方式可靠。

## 另外四个容易被忽略的边界

GitHub 已于 **2026-06-25** 增加 job 内 `background`、`parallel` 与等待步骤。这允许在同一个已准备好环境的 runner 上保留独立日志并执行并行工作；`parallel` 不能写在 composite action 内部。它适合作为分组后的编排手段，但不能增加机器的 CPU/内存，也不能让多个步骤同时修改同一 venv、Cargo target 或输出目录。[官方公告](https://github.blog/changelog/2026-06-25-actions-steps-can-now-be-run-in-parallel/)、[语法说明](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idstepsparallel)

本机实测 `actionlint 1.7.12` 对一个仅含两个 `parallel` 子步骤的最小临时 workflow 报语法错误，认为该 step 缺少 `run` / `uses`。这说明现有本地校验工具尚不能直接验证这套新语法；本次没有运行托管 runner 试验。实施前需先解决校验工具兼容性并验证失败/取消传播。ABI 构建分组本身不依赖新语法，也可以先采用顺序构建、独立消费者验证。

现有 `RUSTC_WRAPPER=sccache` 和 maturin 的 `sccache: true` 不能证明 FastDB 的 C/C++ 编译也命中缓存。sccache 官方说明 Rust 的最终链接类 crate（如 `bin`、`cdylib`）也不属于可缓存范围。[Rust 缓存限制](https://github.com/mozilla/sccache/blob/main/docs/Rust.md) CMake 的 C/CXX compiler launcher 是接入编译缓存的标准接口，但文档明确涉及 Makefile/Ninja 生成器。[CMake 文档](https://cmake.org/cmake/help/latest/prop_tgt/LANG_COMPILER_LAUNCHER.html) 本线程保留的 [Windows 2022 日志](https://github.com/world-in-progress/c-two/actions/runs/38218775294/job/114709692537) 中，FastDB wheel 使用 Visual Studio 17 2022 生成器；另一 Windows 镜像记录了 Visual Studio 18 2026。不能只加 launcher 环境变量就断言 Windows 已接通缓存。先在 Linux/macOS 测 C/C++ 请求与命中，Windows 再单独评估生成器或对应工具链接入。

缓存需要控制体积和键的数量。本次只读 API 快照为 1,555 个缓存、10,523,271,185 字节，约 9.80 GiB；没有核实仓库是否设置了额外容量。GitHub 默认额度为 10 GB 量级，超过配置容量后按访问时间淘汰，官方明确说明可能出现 cache thrashing。[缓存淘汰规则](https://docs.github.com/en/actions/reference/workflows-and-actions/dependency-caching#usage-limits-and-eviction-policy) 这份快照只能说明值得检查容量，不能证明已经发生反复淘汰。应统计恢复/保存耗时、键变化、命中率和净构建时间，再决定保留 target 目录、编译对象缓存或主分支预热的组合。[rust-cache 可配置的范围和键](https://github.com/Swatinem/rust-cache)

`abi3` 也不是本仓库的一项直接开关优化。虽然 PyO3 支持减少常规 CPython wheel 构建，但 3.14t 仍需要专用 wheel。[PyO3 0.29 文档](https://pyo3.rs/v0.29.3/building-and-distribution/) 更具体地，C-Two 当前直接使用 `pyo3::buffer::PyBuffer` 和 `ffi::Py_buffer`；[PyO3 buffer 源码](https://github.com/PyO3/pyo3/blob/v0.29.3/src/buffer.rs#L1-L3) 仅在非 Limited API 或 Python 3.11+ 下启用，CPython 也把 [Py_buffer 的 Stable ABI 起点](https://docs.python.org/3/c-api/buffer.html#c.Py_buffer) 定在 3.11。因此不能直接启用 `abi3-py310` 就覆盖当前要求的 Python 3.10；这会涉及扩展 API 与打包契约评估，而非单纯 CI 整理。

## 建议先做的三个实验

1. **按目标拆开依赖链。** 保留全部 job 的验证内容，仅让各平台消费自己的 CLI 产物，最后做全量 manifest。先确认实际输入关系，再测调度等待是否缩短。
2. **macOS ABI 两组试验。** 每个架构先分成 `3.10/3.11/3.12` 和 `3.13/3.14/3.14t`，每组准备一次环境。逐 ABI 保留解释器、wheel 标签、安装、来源和收据验证；比较总等待与 runner 用量，避免只有检查条目变少、关键路径却变长。
3. **Linux/macOS FastDB C++ 缓存试验。** 核对真实生成器、配置参数和编译命令后接入 launcher，分别记录 C/C++、Rust、链接、SWIG、测试和缓存传输时间。Windows 按实际 Visual Studio 工具链单独验证。

三项分开测，不在同一个实验中同时改矩阵、缓存、runner 规格和测试集合。固定 C-Two/FastDB 来源及工具链，交替运行基线和候选，区分首次冷缓存与预热后的结果；至少保留三对成功运行，比较中位数、排队时间和累计 runner 时间。缺行、跳过、失败运行不能计作提速样本。普通 Python 测试并行可后续使用仓库已有 `tools/dev/test_python.py` 入口评估，保留其文件分组、资源约束及 portable/TypeScript 串行矩阵。

详细来源记录见 [矩阵与项目实践](matrix-and-projects.md) 和 [原生构建与测试工具](native-build-and-test.md)。
