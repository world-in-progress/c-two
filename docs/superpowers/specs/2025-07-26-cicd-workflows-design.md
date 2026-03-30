# CI/CD Workflows Design Spec

**Date**: 2025-07-26
**Scope**: P1 §2.3 — PR 测试 + 版本门控发布 + 多平台 wheel 构建
**Target repo**: `world-in-progress/c-two`

## 1. Overview

Two GitHub Actions workflows:

| Workflow | Trigger | Purpose |
|----------|---------|---------|
| `ci.yml` | `pull_request → main` | 多版本 Python 测试 |
| `release.yml` | `push → main` | 版本比对 → wheel 构建 → PyPI 发布 |

## 2. CI Workflow (`ci.yml`)

### Trigger & Concurrency

```yaml
on:
  pull_request:
    branches: [main]
concurrency:
  group: ci-${{ github.ref }}
  cancel-in-progress: true
```

- 同一 PR 多次 push → 取消旧运行，只保留最新
- 不监听 `push` 事件（release.yml 负责 main 分支）

### Test Matrix

| Python | Runner | 说明 |
|--------|--------|------|
| `3.12` | `ubuntu-latest` | 当前稳定版代表 |
| `3.14t` | `ubuntu-latest` | 自由线程前沿验证 |

### Job Steps

1. `actions/checkout@v4`
2. `actions/setup-python@v5` — `python-version: ${{ matrix.python }}`
3. `dtolnay/rust-toolchain@stable` — Rust 编译器
4. `astral-sh/setup-uv@v6` — uv 包管理器
5. `uv sync` — 安装依赖 + 编译 Rust 扩展
6. `uv run pytest tests/ -q --timeout=30` — 全量测试
   - `C2_RELAY_ADDRESS=""` 环境变量跳过 relay 测试

### 工具选择理由

| 工具 | 替代方案 | 选择理由 |
|------|---------|---------|
| `dtolnay/rust-toolchain` | `actions-rs/toolchain` | 更轻量、单文件、维护活跃 |
| `astral-sh/setup-uv` | 手动安装 uv | 官方 action，带缓存支持 |
| `setup-python` | 手动编译 | 原生支持 `3.14t` 后缀 |

## 3. Release Workflow (`release.yml`)

### Trigger

```yaml
on:
  push:
    branches: [main]
```

### Job 编排

```
version-check ──┬──> build-wheels ──┬──> publish
                └──> build-sdist  ──┘
```

所有构建 job 仅在 `should_release == 'true'` 时执行。

### Job 1: version-check

**Runner**: `ubuntu-latest`
**输出**: `should_release` (bool), `version` (string)

执行 `.github/scripts/check_version.py`:

1. 用 `tomllib` 读取 `pyproject.toml` 中的 `project.version`
2. 查询 `https://pypi.org/pypi/c-two/json` 获取 PyPI 当前版本
3. 用 `packaging.version.Version` 做 PEP 440 比较
4. 写入 `$GITHUB_OUTPUT`

**错误处理**:

| 场景 | 行为 |
|------|------|
| PyPI HTTP 404 | 首次发布，`should_release=true` |
| PyPI 不可达 / 超时 | 安全降级，`should_release=false` |
| 版本相同或更低 | `should_release=false` |

**依赖**: 仅 Python 标准库 + `packaging`（`pip install packaging`）

### Job 2: build-wheels

**依赖**: `needs: version-check`
**条件**: `if: needs.version-check.outputs.should_release == 'true'`

#### 构建矩阵

| Runner | Rust Target | 说明 |
|--------|-------------|------|
| `ubuntu-latest` | `x86_64-unknown-linux-gnu` | Linux x86_64 |
| `ubuntu-24.04-arm` | `aarch64-unknown-linux-gnu` | Linux ARM64 |
| `macos-latest` | `aarch64-apple-darwin` | macOS Apple Silicon |
| `macos-latest` | `x86_64-apple-darwin` | macOS Intel (交叉编译) |

#### Python 解释器矩阵

每个平台构建以下 wheel:
`python3.10 python3.11 python3.12 python3.13 python3.14 python3.14t`

总计: 4 平台 × 6 解释器 = 24 个 wheel

#### 构建步骤

1. `actions/checkout@v4`
2. `PyO3/maturin-action@v1`:
   - `command: build`
   - `target: ${{ matrix.target }}`
   - `args: --release --out dist --interpreter python3.10 python3.11 python3.12 python3.13 python3.14 python3.14t`
   - `manylinux: auto` (Linux only)
3. `actions/upload-artifact@v4`:
   - `name: wheels-${{ matrix.target }}`
   - `path: dist/*.whl`

#### manylinux 策略

使用 `manylinux: auto`，maturin-action 自动选择兼容性最广的 manylinux 标准。

### Job 3: build-sdist

**Runner**: `ubuntu-latest`
**依赖**: `needs: version-check`
**条件**: `if: needs.version-check.outputs.should_release == 'true'`

步骤:
1. `actions/checkout@v4`
2. `PyO3/maturin-action@v1` — `command: sdist`, `args: --out dist`
3. `actions/upload-artifact@v4` — `name: sdist`, `path: dist/*.tar.gz`

sdist 允许无预编译 wheel 平台的用户从源码安装（需本地 Rust 工具链）。

### Job 4: publish

**Runner**: `ubuntu-latest`
**依赖**: `needs: [build-wheels, build-sdist]`
**Environment**: `pypi` (GitHub Environment 保护)
**权限**: `id-token: write` (OIDC Trusted Publishing)

步骤:
1. `actions/download-artifact@v4` — `merge-multiple: true` 合并所有 artifact
2. `pypa/gh-action-pypi-publish@release/v1` — 零配置发布（OIDC 认证）

## 4. PyPI Trusted Publishing

### OIDC 配置（已完成）

PyPI 端已配置 Trusted Publisher:
- Owner: `world-in-progress`
- Repository: `c-two`
- Workflow: `release.yml`
- Environment: `pypi`

### GitHub Environment 配置（需设置）

在 `world-in-progress/c-two` repo Settings → Environments 创建 `pypi` environment:
- 可选: 添加 deployment protection rules（如人工审批）
- 可选: 限制 branches 为 `main`

## 5. 文件清单

| 文件路径 | 用途 |
|----------|------|
| `.github/workflows/ci.yml` | PR 测试工作流 |
| `.github/workflows/release.yml` | 发布工作流 |
| `.github/scripts/check_version.py` | 版本比对脚本 |

## 6. macOS x86_64 交叉编译说明

在 Apple Silicon runner (`macos-latest`) 上交叉编译 Intel 目标:

- `maturin-action` 传入 `target: x86_64-apple-darwin`
- maturin 自动添加 Rust target: `rustup target add x86_64-apple-darwin`
- 无需 Intel runner（`macos-13` 已废弃，`macos-15-intel` 临时方案）

交叉编译仅涉及 Rust native extension 部分。Pure Python 代码无需编译。

## 7. 边缘情况与已知限制

### 限制

1. **Windows 不支持**: 项目依赖 POSIX 特性（UDS、SHM）。待 Windows 适配后再加 CI
2. **发布失败重试**: 部分 wheel 已上传时无法自动重试。需手动在 PyPI 删除版本后重跑
3. **Python 3.14t wheel**: 自由线程 Python 的 wheel tag 为 `cp314t`。不确定所有平台的 `setup-python` 都能正确提供 3.14t 解释器用于 maturin 构建

### 安全

- 无 API token 存储 — OIDC Trusted Publishing 完全消除 secret 泄漏风险
- `id-token: write` 权限仅在 publish job 上声明，最小权限原则
- GitHub Environment `pypi` 可额外加保护规则

## 8. 后续演进

| 时间节点 | 动作 |
|----------|------|
| Windows 适配完成后 | 加入 `windows-latest` 到 CI 和 release 矩阵 |
| ARM runner 费用过高时 | 回退到 QEMU 交叉编译 Linux aarch64 |
| Python 3.15 发布时 | 更新解释器矩阵 |
| PR 测试需覆盖更多版本时 | 扩展 CI matrix（3.10, 3.11, 3.13 等） |
