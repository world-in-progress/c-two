# C-Two Vision

本目录收录 C-Two 的长期愿景与架构定位文档。与 `docs/plans/`、`docs/superpowers/specs/` 这类
落地到具体版本/PR 的文档不同，`vision/` 下的内容回答的是 **"我们最终要成为什么"**，并为下游
框架（Toodle、Gridmen 等）提供协议边界的锚点。

## 索引

| 文档 | 主题 |
|------|------|
| [`sota-patterns.md`](./sota-patterns.md) | C-Two RPC 的 SOTA 设计模式（隐式服务、注册-获取、资源托管） |
| [`endgame-architecture.md`](./endgame-architecture.md) | 分布式资源运行时的 endgame 架构：Toodle / 智能体 / 人在回路 |

## 读者指南

- **C-Two 贡献者**：把 `vision/` 当作协议边界的约束来源。任何新特性在进入 `plans/` 之前，
  都应该能在 vision 里找到对应的动机。
- **Toodle / Gridmen / 下游框架开发者**：`endgame-architecture.md` 刻画了你们赖以构建的
  运行时画像。如果你发现某个边界划错了，issue 提上来，我们一起修正 vision。
- **Agent / Extension 作者**：`endgame-architecture.md` 的第 7 节专门讨论 CRM 契约作为
  Agent Tool Schema 的用法，以及人在回路的约束；第 3 节的"计算模型"解释了 Client /
  CRM / 复合进程 三种角色的边界。
