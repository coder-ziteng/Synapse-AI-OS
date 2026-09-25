# Synapse 设计文档索引

> 本目录包含 Synapse AI 原生微内核的全部设计文档。
> 最后更新：2026-09-25

---

## 文档状态总览

| # | 文档 | 状态 | 关联阶段 | 核心决策数 |
|---|------|------|---------|-----------|
| 00 | [ADR：从零自研](design/00-adr-from-scratch.md) | **DECIDED** | Phase 0 | 1 |
| 01 | [Capability 与 Agent 权限模型](design/01-capability-agent-permission-model.md) | **PROPOSED 完成** | Phase 4 | 6 |
| 02 | [用户态 ABI 与进程模型](design/02-userspace-abi-and-process-model.md) | **PROPOSED 完成** | Phase 4 | 7 |
| 03 | [IPC 消息格式与单拷贝路径](design/03-ipc-message-and-single-copy-path.md) | **PROPOSED 完成** | Phase 4 | 6 |
| 04 | [外交工具通道架构](design/04-diplomat-channel-architecture.md) | **PROPOSED 完成** | Phase 5 | 5 |
| 05 | [跨 OS 外交协议](design/05-inter-os-diplomacy-protocol.md) | **PROPOSED 完成** | Phase 6+ | 7 |
| 06 | [系统服务层路线图](design/06-system-services-roadmap.md) | **PROPOSED 完成** | S1~S6 | 6 |

---

## 阅读顺序建议

### 新人快速了解
1. [需求目标.md](../需求目标.md) — 项目愿景、FR/NFR、路线图
2. Doc 00 ADR — 为什么从零自研
3. Doc 01 Capability — 安全模型核心
4. Doc 03 IPC — 微内核灵魂

### Phase 4 实现者
1. Doc 01 §3~4 — Capability 生命周期 + 数据结构
2. Doc 02 §2~4 — syscall ABI + 地址空间 + 进程模型
3. Doc 03 §3~5 — 消息格式 + 单拷贝路径 + cap transfer
4. Doc 01 §5 — Agent L1~L5 权限映射

### Phase 5 实现者
1. Doc 04 全文 — 外交工具内部架构
2. Doc 01 §6.1 — 唯一网络出口不变量
3. Doc 05 §2~4 — 跨 OS 身份与凭证（远期兼容性约束）

### 系统服务层 (S1~S6)
1. Doc 06 全文 — 服务层路线图
2. Doc 06 §13 — 硬件信任根埋点设计

---

## 跨文档一致性

以下对齐已在设计审查中验证：

| 对齐项 | 涉及文档 | 状态 |
|--------|---------|------|
| Capability 权限位 → syscall 校验 | 01 ↔ 02 | ✅ |
| CapRef=u8 → IPC cap transfer 1 byte | 01 ↔ 03 | ✅ |
| CryptoProvider Signature → 审计事件签名 | 06 ↔ 04 | ✅ |
| Endpoint vs Notification 职责分离 | 03 内部 | ✅ |
| parent 链 + 256 槽 + CapRef=u8 | 01 内部 | ✅ |

---

## 剩余 TBD 汇总

设计阶段可收敛的 TBD 已全部 PROPOSED。剩余 TBD 需要外部输入：

| 类别 | 数量 | 示例 |
|------|------|------|
| 安全评审 | 2 | token 熵源强度、审计事件分级 |
| 性能基准 | 3 | 审计批量 N/T 值、单页 IPC 延迟实测、WASM 沙箱性能 |
| 远期基础设施 | 4 | 跨 OS CRL 同步、自动仲裁、主动/被动分层阈值 |
| 驱动模型 | 2 | Notification mask/unmask、跨进程共享 |
| GUI/显示栈 | 2 | 合成器选型、渲染引擎选型 |

---

## 开发规则

见 [rule.md](design/rule.md) — task.json 维护规范、分支命名约定、协作流程。
