# ADR 00：从零自研微内核（vs seL4/Zircon 二次开发）

> 状态：**ACCEPTED**
> 日期：2026-09-25
> 关联：[需求目标 §0 项目概述](../../需求目标.md)、[设计文档 01](01-capability-agent-permission-model.md)、[设计文档 02](02-userspace-abi-and-process-model.md)、[设计文档 03](03-ipc-message-and-single-copy-path.md)
> 决策者：项目发起人
> 影响范围：内核架构、路线图、技术栈、生态策略

---

## 1. 背景 (Context)

项目目标为构建面向 AI Agent 原生运行的微内核（代号 Synapse）。个人开发者单兵推进，需在以下三条候选路线中决策：

| 路线 | 代表 | 优势 | 劣势 |
|------|------|------|------|
| A. **从零自研 Rust 微内核** | 本项目（Synapse） | 完全可控、深度学习、AI 原生定制 | 工作量大、生态建设从零起步 |
| B. **seL4 二次开发** | seL4 | 形式化验证、约 1 万行、x86/ARM/RISC-V 三架构齐备 | C 语言、改造成本仍不小、与 AI 原生能力模型需重新对位 |
| C. **Zircon/Fuchsia 二次开发** | Google Fuchsia | 驱动框架成熟、模块化、C++ | C++ 不利于内存安全宣称、改造成本高 |
| D. **Linux 极简裁剪** | NeuCore OS | 借力生态、上手快 | 宏内核与 Agent 沙箱目标冲突、安全边界难以做到位 |

原始构想对话中，AI 助手曾建议 B（seL4 二次开发）。经项目发起人决策，最终选定 A。

---

## 2. 决策 (Decision)

**采用路线 A：从零自研 Rust 微内核，不基于任何现有微内核进行二次开发。**

实施约束：

- 内核核心层（`kernel/src/{mm,sched,ipc,syscall}`）由本项目原创实现，不 fork 任何现有微内核代码。
- 借鉴对象限于**设计思想与接口约定**（不复制代码）：
  - seL4：CNode / Endpoint / Notification / 单拷贝 IPC / capability 语义
  - L4：fpage / grant / map
  - Linux：VMA 区域管理、PCID、CPUSET 思想
- 仅在以下层使用成熟第三方 crate，**不视为"二次开发内核"**：
  - 引导：[`bootloader` v0.9.x](../../需求目标.md)（已锁定）
  - x86 寄存器与特权级操作：`x86_64` crate
  - 串口：`uart_16550` crate
  - 日志：`log` crate
  - 构建/测试：`cargo` / `bootimage` / QEMU test_framework
- HAL Trait 设计受 seL4 的 kernel/interface 分离思想启发，但所有代码均为本项目原创。

---

## 3. 理由 (Rationale)

### 3.1 自主可控（首要）
- 项目目标明确为"完全自主可控"。基于第三方微内核（含 C 代码）二次开发，无法做到对**每一行内核代码**的掌握与审查。
- 跨 OS 安全对抗（原始构想核心命题）的可信论证，依赖于内核实现的可审计性。

### 3.2 AI 原生定制不可妥协
- 内核层 IPC 消息头需要 `agent_id`（AI 身份）与 capability token；capability 对象需要覆盖 Agent 行为级（L1~L5）抽象。这些在现有微内核中要么不存在，要么需要破坏性改造。
- Capability 撤销算法需与自进化系统的"演练回滚"耦合——非通用微内核能力。

### 3.3 深度学习价值
- 个人推进微内核项目**本身就是学习载体**。二次开发可绕过最核心的概念（页表、中断、上下文切换），丧失最大学习收益。
- 学习成果沉淀为长期复利（与"AI 操作系统"愿景匹配）。

### 3.4 工程量可控的边界
- 借力 `bootloader` / `x86_64` / `uart_16550` 等成熟 crate，省去 6~12 个月的样板代码工作。
- 1 万行内核代码 + 1 万行用户态（外交工具 + 最小 agent），单人 18~24 个月可达 MVP。
- NeuCore / KernelGPT / SanjayOS 已有单人 AI 原生内核成功案例。

---

## 4. 后果 (Consequences)

### 4.1 正面
- 100% 自控：所有内核代码可逐行审计。
- AI 原生能力可深度定制：IPC / capability / 审计 / 资源核算均可按需扩展。
- 学习成果沉淀为长期资产。

### 4.2 负面 / 风险
- **无形式化验证**：seL4 的核心 IPC / 调度经过形式化证明，本项目短期内只能靠测试与代码审查。缓解：Phase 1 起即建立 QEMU 集成测试 + 宿主单元测试 + CI（见 NFR5）。
- **生态空白**：无现成驱动（virtio / AHCI / NVMe）可移植，需逐设备用户态实现。缓解：Phase 4.5（PCI）→ Phase 5（virtio-net）→ Phase 6（存储栈）逐步扩充。
- **时间线风险**：原路线图 Phase 1~5 已是单人乐观估算（[需求目标 R8](../../需求目标.md)）；路线 A 不会让工作总量变小，但可通过 AI 辅助加速样板代码（避坑指南 §4：底层汇编与页表代码除外）。

### 4.3 与原对话建议的差异
- 原对话中"个人推荐 seL4"是基于"复用成熟框架降低工程量"的普适建议；本决策基于"自主可控 + 学习 + AI 原生定制"的更高优先级。
- 一旦动摇此决策，须重新走 ADR 流程（开新 ADR），不允许在 PR 中静默翻转。

---

## 5. 替代方案与反驳 (Alternatives & Rebuttals)

| 替代方案 | 主要论点 | 否决理由 |
|----------|---------|---------|
| B. seL4 二次开发 | 形式化验证、跨架构齐备、约 1 万行 | C 代码与 AI 原生 capability 模型改造不彻底；自主可控程度不足 |
| C. Zircon 二次开发 | 驱动框架成熟 | C++ 与项目"内存安全宣称"不一致；改造面与 B 相当 |
| D. Linux 极简裁剪 (NeuCore 路线) | 借力生态 | 宏内核架构与"Agent 沙箱"目标冲突；安全边界难做到位 |
| E. hybrid：从零核心 + 复用 seL4 验证套件 | 兼顾学习与可信 | seL4 验证套件强依赖其代码结构，移植成本接近 B |

---

## 6. 触发复审的条件 (Re-evaluation Triggers)

出现以下任一情况须重新发起 ADR 评估：

- 个人开发者精力 / 时间窗口发生数量级变化；
- 路线图 Phase 4 末出现无法解决的稳定性问题（连续 6 个月 Phase 4 无可演示进展）；
- 上游出现开源 Rust 微内核（Rust 编写、AI 原生、形式化验证、跨架构）且满足 NFR1~4。

---

## 7. 参考 (References)

- seL4 formal verification: https://sel4.systems/Verification/
- Fuchsia Zircon: https://fuchsia.dev/fuchsia-src/concepts/kernel
- NeuCore OS / KernelGPT / SanjayOS（单人 AI 原生内核案例）
- 原始构想对话记录 [`原始思路.md`](../../原始思路.md)
