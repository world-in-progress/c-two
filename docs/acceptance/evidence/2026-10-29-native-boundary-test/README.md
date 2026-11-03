2026-10-29 native boundary and failed-release regression evidence

输入提交：f04cf89ecf2dac11c978faabd282f0395684e038。真实分配测试恢复自 f73b5dde 的 core_ffi.rs，并补上直接 release() 再次失败时保留 tracking 的断言。去掉新增的 cfg(test) 测试后，该 Rust 文件与输入提交逐字节一致；生产释放逻辑的变异已还原。

边界检查通过 TOML 解析只读取 dependencies、build-dependencies 和目标专属的同类表，并检查重命名依赖的 package 字段。dev-dependencies、目标专属开发依赖、说明文字和注释不会被误报。tokio::runtime::Builder:: 的原有检查保留。Python 3.10 复用 pytest 已有的 tomli 依赖，Python 3.11 及以上使用 tomllib，未新增 Python 依赖。

真实重组分配为 512 字节。失效回调产生的 ValueError 保留清理异常及原 KeyError 的上下文链；原生释放失败后 tracking 保持 1 个 lease、1 个 hold、512 字节。恢复原 pool 后重试成功，tracking 与重组预算均归零，abandoned_releases 为 0。最终放弃后 tracking 归零，abandoned_releases 为 1，未确认释放的重组预算仍为 512 字节。

| 验证 | 实际结果 |
| --- | --- |
| 原边界检查，恢复开发依赖后 | 1 failed，退出码 1 |
| Python 3.10 依赖边界及负例 | 26 passed，41 deselected，退出码 0 |
| release() 提前归还 tracking 的变异 | 1 failed，断言实际 (0, 0)、预期 (1, 1)，退出码 101 |
| 还原后的同一原生测试 | 1 passed，18 filtered out，退出码 0 |
| 完整 PyO3 原生入口 | 19 passed，0 failed，0 ignored；0 doc-tests；退出码 0 |
| 完整 Python 边界测试文件 | 67 passed，退出码 0 |

Python 3.10 的 26 个用例包含在边界文件的 67 个用例内，计数不应相加。原始命令和输出保留在 boundary-red-green.log、native-release-red-green.log 与 affected-suites.log；仅脱敏本机路径并去掉行末空白，原生入口使用 terse 格式，未保存逐项 ok 列表。

本机为 macOS ARM64，Rust 1.91.0；解释器为 Python 3.10.17 与 Python 3.12.10。FastDB Core SDK 由仓库 .github/scripts/prepare_fastdb_sdk.py 下载并验证，版本 0.2.3，源提交 abba785ef2a7d3fbee1bbc608393a3e6c7b042c4，ARM64 归档 SHA-256 为 5187a11cf47d24bbdbf6a0541ade2e6797a04d763df52971b45a0b3d94c69157。扩展和原生测试使用该临时 SDK 的 system link。

所有 Rust 构建使用同一个 CARGO_TARGET_DIR=$VALIDATION_DIR/cargo-target；使用临时 UV_PROJECT_ENVIRONMENT、UV_CACHE_DIR、UV_NO_SYNC=1、CARGO_INCREMENTAL=0、CARGO_PROFILE_DEV_DEBUG=0、CARGO_PROFILE_TEST_DEBUG=0、CARGO_PROFILE_DEV_STRIP=none、CARGO_PROFILE_TEST_STRIP=none，并设置临时 SDK 的 FASTDB_PAYLOAD_SYSTEM_LIB_DIR 与 DYLD_LIBRARY_PATH。扩展通过 uv sync --locked --no-default-groups --group ci-policy 构建，MATURIN_PEP517_ARGS 为 --profile dev。本机源码模式构建未找到 C++ 标准头文件，因此采用任务授权的校验 SDK；关闭 strip 解决了已观察到的 LINKEDIT 对齐加载错误，对应 [Rust 上游记录](https://github.com/rust-lang/rust/issues/157750)。完整 Python 测试以 python -m pytest 调用，以保留设置的动态库搜索环境；长路径下的 pytest 启动器是 shell 脚本。临时 strip/codesign 尝试的产物已经由关闭 strip 的重建覆盖，最终验证使用重建后的扩展。

本次受影响测试没有未解决失败。未运行需要同级 FastDB 源码的 c2-codegen tests/generated_rust_sdk.rs、跨语言矩阵、完整 Python 套件或跨平台 CI；按冻结范围由 Host 在验收环境运行。这些记录只证明本地受影响测试及变异敏感性，不构成发布、安装或跨平台验收证据。

全部命令退出后已删除唯一 Cargo target、两个自建虚拟环境、临时 uv 缓存、下载的 FastDB SDK 与归档、构建脚本与原始临时日志，所在临时树删除前占 1583508 KiB；同时删除本次生成的 Python 原生扩展和 9 个新建缓存目录。已确认上述路径均不存在，保留的只有本证据、测试和开发依赖改动。
