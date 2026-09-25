# Synapse — AI 原生微内核

> **状态**：设计文档 Doc 01~06 全部 **PROPOSED 完成**（Phase 4/5/6+ 核心决策收敛）；代码阶段 Phase 1 进行中（P1-T5/T6/T7/T9 已完成，P1-T4 boot 调试中）。
>
> **任务管理**：[`task.json`](task.json) 是项目长记忆状态与任务计划的唯一来源，按 [`docs/design/rule.md`](docs/design/rule.md) 规则维护。
>
> **设计文档索引**：[`docs/README.md`](docs/README.md) — 状态总览、阅读顺序、跨文档一致性、剩余 TBD。

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

### 前置条件

- Rust nightly（日期锁定于 `rust-toolchain.toml`）+ `rust-src` + `llvm-tools-preview`
- QEMU ≥ 7.0（`qemu-system-x86_64`）
- Python 3（用于 `build_disk.py`）

### 构建内核 ELF

```bash
cargo build -p synapse-kernel --target x86_64-bootloader.json
```

### 生成可引导镜像

```bash
python build_disk.py target/x86_64-bootloader/debug/synapse-kernel kernel_hd.img
```

### QEMU 启动

```bash
qemu-system-x86_64 -nographic -drive file=kernel_hd.img,if=ide,format=raw -device isa-debugcon,iobase=0x501,chardev=dbg -chardev file,path=debug.log,id=dbg -device isa-debug-exit,iobase=0x501,exit-code=181
```

> **注意**：P1-T4 boot 调试中。当前 stage 2 → 64-bit 跳转后内核 `_start64` 的到达尚未完全验证。调试检查点输出见 `debug.log`（port 0x501）。

### 运行测试（HAL）

```bash
cargo test -p synapse-hal
```

### CI

见 `.github/workflows/ci.yml`（P1-T9 已完成）。
