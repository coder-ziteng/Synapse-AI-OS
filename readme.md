# Synapse — AI 原生微内核

> 一个从零自研的 x86_64 微内核，目标是让 **AI Agent 成为一等公民**：
> 内核态只保留调度 / 内存 / IPC / 能力（Capability）原语，所有 AI 业务
> （外交工具、Agent 运行时、记忆系统）全部运行在用户态，受 Capability
> 权限模型与审计事件流约束。
>
> **当前状态**：✅ Phase 0~2 已完成（裸机点亮 → 内存 / 中断 / 异常框架 → TSC 时钟校准），
> 真机 QEMU smoke 全部通过。下一步：Phase 3 多任务与调度。
>
> **任务看板**：[`task.json`](task.json)（唯一事实来源） · **开发规则**：[`docs/design/rule.md`](docs/design/rule.md) · **设计文档索引**：[`docs/README.md`](docs/README.md)

---

## 1. 为什么做这个项目

现有 OS 把 AI Agent 当普通进程对待：权限粗粒度（文件系统 / 网络全有或全无）、
行为不可审计、资源不可核算。Synapse 的回答是：

- **Capability 安全模型** — Agent 的每项权限（对象访问、IPC、设备）都是可委托、
  可撤销、可审计的能力对象，支持 L1~L5 分级授权与高风险操作人工确认通道。
- **微内核 + 用户态外交工具** — 网络出口收敛到唯一持有网卡能力的外交工具进程，
  架构性保证"唯一网络出口"不变量；内核仅提供 IPC 与中断路由。
- **AI 原生原语** — 资源核算（FR8）、进程冻结 / 频率计数（FR10）、
  append-only 审计事件流（FR9）从内核层直接支撑 Agent 监督树与行为围栏。

设计取舍详见 [ADR 00：从零自研 vs 二次开发](docs/design/00-adr-from-scratch.md)
与 [需求目标.md](需求目标.md)（FR/NFR + 六阶段路线图）。

## 2. 架构总览

```text
┌─────────────────────────────────────────────────────────┐
│  用户态（规划中，Phase 4+）                               │
│  Agent 运行时 · 外交工具(smoltcp/virtio-net) · 记忆系统    │
├──────────────── syscall (18 个, ABI 见设计文档 02) ────────┤
│  内核态                                                  │
│  cap/   Capability 表 · 对象表 · 委托/撤销级联             │
│  ipc/   Endpoint(同步) · Notification(异步) · 单拷贝路径    │
│  proc/  进程表 · Agent 注册表 · spawn/exit/reap            │
│  sched/ TCB 状态机 · runqueue · sleep 队列 · FR8 核算 · FR10 频率计数│
│  kernel/ 引导链 · 页帧/堆 · GDT/TSS/IDT · PIC/PIT · TSC 时钟│
│  hal/   硬件抽象 Trait (Mmu/Interrupt/Timer/Serial)        │
│  audit/ 审计事件流（骨架）                                 │
└─────────────────────────────────────────────────────────┘
```

`cap/`、`ipc/`、`proc/`、`sched/` 为零依赖纯逻辑 crate（`#![deny(unsafe_code)]`），
宿主端 171 个单元测试全绿；内核集成层用 IRQ-safe SpinLock 包装后已在
QEMU 真机跑通端到端 smoke（spawn → 委托 → 撤销 → IPC → exit → reap）。

## 3. 进度（详细状态见 [task.json](task.json)）

| Phase | 内容 | 状态 |
| --- | --- | --- |
| P0 | 环境与基线（nightly-2026-09-23 锁定 + QEMU + 工具链验证） | ✅ 完成 |
| P1 | 裸机点亮与工程基建（三级 boot 链 · UART · log/panic 回溯 · CI · 测试框架） | ✅ 完成 (11/11) |
| P2 | 内存 / 中断 / 异常（E820 · 页帧分配器 · 内核堆 · GDT/TSS/IST · IDT · PIC/PIT · TSC 校准） | ✅ 完成 (7/7) |
| P3 | 多任务与调度（sched/ 纯逻辑 crate · switch_to 汇编 · 抢占模型 · Mutex · FR8/FR10 原语） | 🔄 进行中 (3/8)：T1 sched crate ✅ · T2 FR8 核算 ✅ · T3 FR10 频率计数 ✅ 56 测试全绿 |
| P4 | 用户态与 IPC（用户地址空间 · syscall · ELF 加载 · init 进程） | 未开始 |
| P4.5 | PCI 枚举与中断用户态化 | 未开始 |
| P5 | 外交工具与 Agent 雏形 | 未开始 |
| P6 | SMP / 存储栈 / IOMMU / 本地推理 / 跨 OS 外交 | 远期 |

## 4. 快速开始

### 前置条件

- Rust nightly（日期锁定于 `rust-toolchain.toml`）+ `rust-src` + `llvm-tools-preview`
- QEMU ≥ 7.0（`qemu-system-x86_64` 在 PATH 中）
- Python 3（`build_disk.py` 打包磁盘镜像）

### 一键构建 + 运行 + 判定（推荐）

```powershell
.\startAIOS.ps1              # 构建 → QEMU 无头运行 → 三重自动判定
.\startAIOS.ps1 -Test        # 运行 QEMU 内测试套件（P1-T8）
.\startAIOS.ps1 -BuildOnly   # 只构建 kernel_hd.img
```

成功判定（脚本自动检查）：QEMU 退出码 **363**、`logs\serial.log` 出现
`N/N checks passed` 且无 `[PANIC]`、`logs\debugcon-kernel.log` boot marker 序列完整。
所有运行日志统一落 `logs\` 子目录。

### 手动分步

```bash
cargo run -p synapse-xtask -- build   # 构建磁盘镜像
cargo run -p synapse-xtask -- run     # QEMU 运行（exit 1 属正常，看 logs\ 判定）
cargo test --workspace --exclude synapse-kernel   # 宿主单元测试
python kernel/tests/run_tests.py      # QEMU 测试套件
```

## 5. 仓库结构

| 路径 | 说明 |
| --- | --- |
| `kernel/` | 内核本体（引导链 boot.S、内存、中断、异常、集成层、smoke） |
| `cap/` `ipc/` `proc/` `sched/` | 纯逻辑 crate：Capability / IPC / 进程 / 调度（零 unsafe，宿主可测） |
| `hal/` | 硬件抽象 Trait + `cfg(test)` fake 实现 |
| `abi/` | 用户态 ABI（syscall 号表，18 个，设计文档 02 §4.2） |
| `audit/` | 审计事件流（骨架） |
| `user/` | 用户态程序（Phase 4 启用） |
| `xtask/` | 构建 / 运行 / CI 任务封装 |
| `docs/design/` | 设计文档 00~07 + rule.md 开发规则 |
| `scripts/` `build_disk.py` `verify-all.ps1` | 辅助脚本 |

## 6. 文档导航

| 入口 | 路径 |
| ------ | ------ |
| 需求目标（FR/NFR + 路线图） | [`需求目标.md`](需求目标.md) |
| 需求评审与补充方案 | [`docs/requirements-review-and-supplement.md`](docs/requirements-review-and-supplement.md) |
| Doc 00 ADR：从零自研 | [`docs/design/00-adr-from-scratch.md`](docs/design/00-adr-from-scratch.md) |
| Doc 01 Capability 与 Agent 权限模型 | [`docs/design/01-capability-agent-permission-model.md`](docs/design/01-capability-agent-permission-model.md) |
| Doc 02 用户态 ABI 与进程模型 | [`docs/design/02-userspace-abi-and-process-model.md`](docs/design/02-userspace-abi-and-process-model.md) |
| Doc 03 IPC 消息与单拷贝路径 | [`docs/design/03-ipc-message-and-single-copy-path.md`](docs/design/03-ipc-message-and-single-copy-path.md) |
| Doc 04 外交工具通道架构 | [`docs/design/04-diplomat-channel-architecture.md`](docs/design/04-diplomat-channel-architecture.md) |
| Doc 05 跨 OS 外交协议 | [`docs/design/05-inter-os-diplomacy-protocol.md`](docs/design/05-inter-os-diplomacy-protocol.md) |
| Doc 06 系统服务层路线图 | [`docs/design/06-system-services-roadmap.md`](docs/design/06-system-services-roadmap.md) |
| Doc 07 显示栈与空间外壳 | [`docs/design/07-display-stack-and-spatial-shell.md`](docs/design/07-display-stack-and-spatial-shell.md) |
| 设计文档索引（状态总览 + 阅读顺序） | [`docs/README.md`](docs/README.md) |
| **开发规则（含 README 同步协议）** | [`docs/design/rule.md`](docs/design/rule.md) |

## 7. 协作与贡献

本项目由紫藤（陈朕）带领多个AI Agent协作开发，核心约定：

1. **`task.json` 是看板唯一事实来源** — 修改需持 `.task.lock`（原子创建，用完即释放），
   decision_log 只追加不改写；每窗口独立 git 分支。
2. **README 同步协议（rule.md 第 8 条）** — 任何窗口完成任务提交时，必须在同一提交
   （或紧邻提交）中更新本 README 的受影响小节：状态行、进度表、快速开始、仓库结构。
   增量小编辑，禁止整篇重写他人正在维护的小节。
3. **完成标准** — 每个任务必须真机（QEMU）验证：退出码 + boot marker + 串口日志
   三重证据写入 task.json 的 `actual_approach`。
4. 面向 GitHub 开源维护：README 是新成员（人类与 AI Agent）的第一入口，
   价值主张与架构图必须保持最新。

---

*Synapse — 让 AI Agent 在能力约束与审计之下，成为操作系统的一等公民。*
