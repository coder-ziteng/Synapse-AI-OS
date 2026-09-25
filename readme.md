# Synapse — AI 原生微内核

> **状态**：docs 阶段完成（`docs/design/00-06` + `docs/requirements-review-and-supplement.md`）；代码阶段 Phase 0/1 进行中。
>
> **任务管理**：[`task.json`](task.json) 是项目长记忆状态与任务计划的唯一来源，按 [`docs/design/rule.md`](docs/design/rule.md) 规则维护。

---

## 1. 项目目标

构建面向 AI Agent 原生运行的微内核：内核态仅保留调度/内存/IPC/能力原语，所有 AI 业务逻辑下沉到用户态（外交工具 + Agent 运行时 + 记忆系统）。详见 [`docs/design/00-adr-from-scratch.md`](docs/design/00-adr-from-scratch.md)（已接受：从零自研 vs 二次开发）。

## 2. 文档导航

| 入口 | 路径 |
|------|------|
| 需求目标（FR/NFR + 路线图）| [`需求目标.md`](需求目标.md) |
| 需求评审与补充方案 | [`docs/requirements-review-and-supplement.md`](docs/requirements-review-and-supplement.md) |
| 设计文档 00 ADR | [`docs/design/00-adr-from-scratch.md`](docs/design/00-adr-from-scratch.md) |
| 设计文档 01 Capability | [`docs/design/01-capability-agent-permission-model.md`](docs/design/01-capability-agent-permission-model.md) |
| 设计文档 02 用户态 ABI | [`docs/design/02-userspace-abi-and-process-model.md`](docs/design/02-userspace-abi-and-process-model.md) |
| 设计文档 03 IPC | [`docs/design/03-ipc-message-and-single-copy-path.md`](docs/design/03-ipc-message-and-single-copy-path.md) |
| 设计文档 04 外交工具 | [`docs/design/04-diplomat-channel-architecture.md`](docs/design/04-diplomat-channel-architecture.md) |
| 设计文档 05 跨 OS 外交 | [`docs/design/05-inter-os-diplomacy-protocol.md`](docs/design/05-inter-os-diplomacy-protocol.md) |
| 设计文档 06 系统服务层路线图 | [`docs/design/06-system-services-roadmap.md`](docs/design/06-system-services-roadmap.md) |
| 开发规则 | [`docs/design/rule.md`](docs/design/rule.md) |

## 3. 路线图（高层）

- **Phase 0** — 环境与基线（Rust nightly + QEMU + bootimage）
- **Phase 1** — 裸机点亮 + 工程基建（QEMU Hello Synapse）
- **Phase 2** — 内存 / 中断 / 异常框架
- **Phase 3** — 多任务与调度（FR8/FR10 原语）
- **Phase 4** — 用户态与 IPC（Capability + 单拷贝路径）
- **Phase 4.5** — PCI 枚举与中断用户态化
- **Phase 5** — 外交工具 + Agent 雏形
- **Phase 6** — SMP / 存储栈 / 跨 OS 外交 / 硬件信任根

并行：**S1~S6 系统服务层**（详见设计文档 06）。

## 4. 当前任务

见 [`task.json`](task.json) 的 `session.current_phase` / `current_task`。Phase 0 阻塞于工具链缺失，需用户侧执行安装（参见 task.json `env_prerequisites`）。

## 5. 构建与运行

> 待 Phase 1 完成后填入。

```bash
# 占位（Phase 1 完成后启用）
cargo bootimage --bin synapse-kernel
cargo run -p xtask -- run    # QEMU 启动
```