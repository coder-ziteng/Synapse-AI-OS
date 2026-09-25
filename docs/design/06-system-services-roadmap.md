# 设计文档 06：系统服务层路线图 (System Services Layer Roadmap)

> 状态：**PROPOSED 完成**（待确认升级为 DECIDED）
> 关联需求：原始构想 [记忆系统/自进化/Agent 编排/本体模型/动态 UI/GUI 显示栈/数据层] 等系统层愿景
> 关联里程碑：**S1~S6 服务层阶段**（与 Synapse 内核层 Phase 1~6 并行）
> 最后更新：2026-09-25

### PROPOSED 决策汇总

| 决策项 | PROPOSED 方案 | 章节 |
|--------|--------------|------|
| 服务层依赖级别 | P0（内存态）/ P1（mock storage）/ P2（真实存储/网络/GPU）三级分类 | §2.1 |
| S4 矢量检索（首期） | 暴力召回跑通闭环 + `VectorIndex` trait 抽象 | §6.6 |
| S4 图存储 | 自研极简属性图（邻接表 + 页式存储，与 P6 存储栈同构）| §6.6 |
| 硬件信任根埋点 | `CryptoProvider` trait（算法无关）+ `TrustRoot` trait 占位 | §13.2 |
| 审计签名后量子迁移 | `Signature` 不透明类型（含算法标识），Phase 6 替换不改业务层 | §13.2.1 |
| HAL TrustRoot 首期 | 空实现 `NullTrustRoot`，方法全部返回 `NotImplemented` | §13.2.3 |
| 服务层强制规则 | 内核 P4 通过即开工 S1，不可无限推迟 | §12 |

> ✅ 系统服务层路线图核心设计决策已完成，剩余 TBD 为 S6 GUI 显示栈选型、WASM 沙箱性能基准、多编程环境边界等。

---

## 0. 文档目的

原始构想中"AI OS 区别于传统 OS 的灵魂"——**记忆系统、自进化系统、Agent 运行时、本体驱动的应用模型、图形化动态 UI、向量/图数据原生支持**——全部位于**内核之上的用户态系统服务层**，不在 Synapse 内核层（Phase 1~6）范围内。

本文档定义系统服务层的 **S1~S6 阶段路线图**，并标注与内核层的依赖关系，防止愿景流失。

> ⚠️ 内核层与系统服务层是**两层并行路线图**：
> - **Synapse Phase 1~6**（内核）：当前 [需求目标 §三](../../需求目标.md) 路线图。
> - **Service Phase S1~S6**（系统服务）：本文档。一旦内核足够可用即开工 S1；S 阶段可以比 P 阶段晚，但**不能晚到影响 P 阶段验收**。

---

## 1. 分层总览

```
┌─────────────────────────────────────────────────────────────────────────┐
│                       用户态（Synapse 之上）                              │
│                                                                          │
│   S6  应用本体 + 动态 UI + 显示栈                                        │
│       ▲                                                                  │
│       │ 消费                                                              │
│   S5  多 Agent 协作 + 安全审计 + 多编程环境 (WASM)                      │
│       ▲                                                                  │
│       │ 消费                                                              │
│   S4  记忆系统 + 自进化系统（OS 原生共享）                              │
│       ▲                                                                  │
│       │ 消费                                                              │
│   S3  资源管理：ComputeManager / StorageQuota / 监督树                  │
│       ▲                                                                  │
│       │ 消费                                                              │
│   S2  Agent 运行时：AgentBus / Planner / Executor                       │
│       ▲                                                                  │
│       │ 消费                                                              │
│   S1  基础服务：日志 / 监控 / 配置中心 / 安全网关                        │
│       │                                                                  │
│  ─────┼───────────── 内核态边界（IPC / syscall）─────────────────────── │
│   Synapse Phase 1~6（内核层）                                          │
└─────────────────────────────────────────────────────────────────────────┘
```

---

## 2. 服务层阶段总览

| 阶段 | 名称 | 核心交付 | 核心组件 | 起始前置 |
|------|------|---------|---------|---------|
| **S1** | 基础服务 | 可观察、最小可用 | 日志 / 监控 / 配置中心 / 安全网关 | 内核 P4（IPC + syscall）|
| **S2** | Agent 运行时 | Agent 可接收任务 | AgentBus / TaskScheduler / Planner / Executor | P4 + S1 |
| **S3** | 资源管理与监督 | 资源可控、故障可恢复 | ComputeManager / StorageQuota / 监督树 / 行为围栏 | P4 资源核算 + S2 |
| **S4** | 记忆与自进化 | Agent 有记忆、会反思 | STM / EpisodicMemory / SemanticMemory / Reflector / SkillPool | P6 存储栈 + S3 |
| **S5** | 多 Agent 与安全 | 多 Agent 协作 + 安全可控 | MultiAgentManager / SecurityGuard / 工具链 / WASM 沙箱 | S4 + P4.5/5 |
| **S6** | 本体模型与动态 UI | 应用 = 本体实例化 + AI 动态生成 UI | OntologyEngine / 渲染栈 / 显示驱动 | S5 + P6 GPU |

### 2.1 服务层依赖级别 *(评审补充，对齐 [需求评审 §2.10](../requirements-review-and-supplement.md))*

**问题**：S1~S6 各阶段对内核能力的依赖程度不同。若所有服务都等待完整存储栈（P6），则 S1/S2 无法在内核 P4 通过后立即开工，违反"强制规则"（§12）。

**PROPOSED → 三级依赖分类**：

| 级别 | 定义 | 可用 mock | 适用阶段 |
|------|------|----------|---------|
| **P0** | 不依赖持久化的内存态服务，可在 QEMU 上演示 | 无需 | S1（日志/监控）、S2（AgentBus）|
| **P1** | 依赖内核 IPC、进程和配额，但可使用 host-side/mock storage | initramfs 只读配置 + 内存态 KV | S1（配置中心）、S3（监督树）|
| **P2** | 依赖真实存储、网络或 GPU，必须等相应内核能力完成 | 无 | S4（向量/图库）、S5（WASM）、S6（显示栈）|

**各阶段首期承诺**：

| 阶段 | 首期承诺级别 | 降级策略 |
|------|-------------|---------|
| **S1** | P0 + P1 | 配置中心先用 initramfs 只读配置 + 内存态 KV；SQLite 推迟至 P2 |
| **S2** | P0 + P1 | Planner 调用远程 LLM API（走外交工具）；本地推理推迟至 P6 |
| **S3** | P1 | 监督树状态存内存；持久化回滚快照推迟至 P2 |
| **S4** | P2（但允许 P1 内存态过渡）| 向量/图库先用内存实现（§6.6）；trait 抽象从 S4.0 起存在 |
| **S5** | P2 | WASM 沙箱依赖进程隔离（P4 已有）；工具链注册表可先用内存态 |
| **S6** | P2 | 显示栈依赖 virtio-gpu（P6）；本体引擎可先用内存态图库 |

**关键约束**：

- S1/S2 **不允许**以"等待存储栈"为由推迟——P0/P1 级别的服务必须在内核 P4 通过后立即开工；
- 每个 S 阶段必须有**独立退出测试**，不能以"内核 Phase 完成"自动视为服务层完成；
- P1 → P2 迁移时，**trait 接口不变**（如 `VectorIndex` / `GraphStore` / `ConfigStore`），仅替换实现。

---

## 3. S1：基础服务

### 3.1 范围
- **日志服务**（Log Service）：统一日志采集 / 转发 / 持久化（依赖 P6 存储）。
- **监控服务**（Monitor Service）：CPU/内存/进程指标，导出给外交工具 → 远端可观测。
- **配置中心**（Config Service）：SQLite-backed KV，替代传统 ini/yaml（参考 NeuCore Phase 1）。
- **安全网关**（Security Gateway）：见 [设计文档 04 §6](04-diplomat-channel-architecture.md)。

### 3.2 退出标准
- 所有用户态服务的日志经配置中心可被查询 / 过滤。
- 外交工具的扫描结果可被持久化追溯。
- 健康检查接口可被外部调用。

### 3.3 与外交工具的关系
- S1 的安全网关**就是外交工具的一部分**（文档 04），不另起新进程。
- 日志 / 监控 / 配置中心各为独立用户态服务，向外交工具发起 IPC。

---

## 4. S2：Agent 运行时

### 4.1 范围
原始构想明确："意图 → 本体匹配 → 工具编排 → 执行 → 事件反馈 → 状态更新"。S2 实现该循环的"无本体"版本。

| 组件 | 职责 |
|------|------|
| **AgentBus** | 统一事件总线（pub/sub），基于 IPC Notification |
| **TaskScheduler** | 任务 DAG 拆解 + 依赖管理 + 调度（调度器在内核层；这是用户态任务图调度）|
| **Planner** | 把自然语言意图拆解为 DAG 任务节点（首期调用远程 LLM API） |
| **Executor** | 执行任务节点，调用 Agent 工具 |
| **Agent Lifecycle Manager** | Agent 创建 / 启动 / 暂停 / 重启（基于 P2 的监督树） |

### 4.2 退出标准
- 主 Agent 可接收用户自然语言意图。
- 通过远程 LLM API 拆解为 DAG。
- 子 Agent 分工执行，事件流经 AgentBus 可观察。
- 一次完整 "意图→执行→结果" 端到端 demo。

### 4.3 与外交工具的关系
- Planner 调用远程 LLM 走 API Channel（文档 04 §5.4）。
- Executor 调工具若涉及网络，全部经外交工具。

---

## 5. S3：资源管理与监督

### 5.1 范围
原始构想："ComputeManager / StorageQuota / ResourceHub / 监督树"。

| 组件 | 职责 |
|------|------|
| **ComputeManager** | per-process CPU 时间核算（消费内核 Phase 3 资源核算原语）+ 配额 enforcement |
| **StorageQuota** | per-process 存储配额 + 强制走安全网关（文档 04 §6.4） |
| **Supervisor Tree** | 监督树：声明式重启策略（重启次数上限 → 降级 → 熔断） |
| **Behavior Fence** | 行为围栏：异常 syscall/IPC 频率检测 → 冻结进程（消费内核 freeze 原语） |

### 5.2 退出标准
- 主 Agent 可声明子 Agent 内存上限 + CPU 时间预算。
- 子 Agent 超额时自动冻结 + 通知主 Agent。
- 子 Agent 崩溃后监督树按策略重启。

### 5.3 与外交工具的关系
- ComputeManager 限制外交工具自身的 CPU/内存使用——外交工具是高安全特权进程，需严防其失控吞资源。

---

## 6. S4：记忆与自进化（灵魂模块）

原始构想："记忆能力沉淀为操作系统原生能力后，将迎来跃升"。

### 6.1 四层记忆架构（原始构想原文）
| 层 | 内容 | 存储 |
|------|------|------|
| **短期工作记忆 (STM)** | 当前会话上下文 | 内存（会话结束即释放）|
| **中期经验记忆 (EpisodicMemory)** | 近期成功 / 失败案例 | 向量数据库 + 元数据 |
| **长期知识记忆 (SemanticMemory)** | 结构化知识 / 技能依赖 / 核心决策日志 | 图数据库 |
| **档案记忆 (Archolife)** | 用户偏好 / 行为模式 | 嵌入式 KV（零 LLM 调用检索，如 MOBIMEM DisGraph）|

### 6.2 自进化系统（原始构想核心闭环）
```
执行轨迹 → 效果评估 → 反思分析 → 策略优化 → 技能沉淀
                                            ↓
                                       SkillPool（可复用模板）
```

- **GEPA 算法**（类反向传播优化 Prompt）：100~500 次评估收敛。
- **技能自动提炼**：从操作日志聚类 → 抽象为可复用模板。
- **梦境记忆整理**：定时对长期记忆去重 / 合并 / 修剪。

### 6.3 安全约束（关键）
- 自进化**必须在隔离环境异步运行**（独立进程，受 ComputeManager 限制）。
- 变更**可回滚、可追溯**：每次策略更新写入审计 + 生成可回滚的快照。
- **不污染主系统**：进化进程被冻结时，主 Agent 正常运行。

### 6.4 退出标准
- 主 Agent 能跨会话记住用户偏好（档案记忆）。
- Agent 能从失败中反思出"下次遇到 X 该做 Y"（中期经验记忆）。
- 长期记忆中的过时条目被自动清理。

### 6.5 前置依赖
- 向量数据库 + 图数据库的 OS 原生集成（依赖 P6 存储栈 + 数据层服务）。
- GEPA 算法可独立于内核，是**用户态库**。

### 6.6 向量 / 图数据库的提供方 *(评审补充：此前只写了"需要"，未写"谁给")*

S4 的两个存储引擎是**外部依赖缺口**，必须在 S4 启动前决策；否则 S4 会被动等待，与"强制规则"（§12）冲突。

| 引擎                                    | 候选路径                                                                                    | 优势                                        | 风险                                |
| --------------------------------------- | ------------------------------------------------------------------------------------------- | ------------------------------------------- | ----------------------------------- |
| **矢量检索**（EpisodicMemory）          | (a) 自研 HNSW 最小实现；(b) 移植 `hnswlib` / `usearch`；(c) 纯暴力召回（数据量小时）        | (b) 成熟、性能好；(c) 零依赖                | (b) C++/依赖重；(a) 调优成本高      |
| **图存储**（SemanticMemory / 本体）     | (a) 自研极简属性图（邻接表 + 页式存储）；(b) 移植 SQLite + 递归 CTE；(c) 用关系表模拟图     | (a) 与 P6 存储栈同构、可控；(b) 立刻可用    | (b) 与"OS 原生"叙事不符             |

**倾向（TBD，S4 启动前收敛）**：

- 矢量：先 **(c) 暴力召回**跑通闭环（S4.0），数据量增长后再换 (a)/(b) —— 接口层先抽象为 `VectorIndex` trait，避免锁定。
- 图：**(a) 自研极简属性图**，与 P6 存储栈共用页式存储，保持"数据层原生"定位。

**跨阶段约束**：S4 的早期实现**允许内存态**（§12 缓解项），但 **`VectorIndex` / `GraphStore` trait 必须从 S4.0 起就存在**，否则后期替换引擎将波及自进化、本体推理等所有消费方。

---

## 7. S5：多 Agent 与安全

### 7.1 范围
原始构想："MultiAgentManager / SecurityGuard / 工具链 / 多编程环境"。

| 组件 | 职责 |
|------|------|
| **MultiAgentManager** | 主 Agent + 子 Agent 协作框架 / DAG 协作 / 共识机制 |
| **SecurityGuard** | 进程级安全策略 enforcement（消费 S3 行为围栏）|
| **工具链服务** | L5"代码运行级"工具的统一注册 + 隔离执行 |
| **WASM 沙箱** | 多编程环境的进程内沙箱（执行任意代码不污染主进程）|

### 7.2 五级权限 enforcement
消费文档 01 中 L1~L5 映射到 capability 集合：
- L1 读 / L2 写 / L3 删 / L4 对外交互 / L5 代码运行
- **不可跨级自动授权**；高风险（L4/L5）必须经用户确认（GUI 弹窗 / 语音确认）

### 7.3 退出标准
- 主 Agent 可编排多个子 Agent（含 DAG 共识）。
- 用户运行任意代码段（如 Python 风格的脚本）经 WASM 沙箱隔离。
- L4/L5 操作弹出确认对话框。

---

## 8. S6：本体模型与动态 UI

### 8.1 范围
原始构想："应用的本质使用一堆本体模型定义"——应用 = 本体模型 + 事件链 + AI 行为。

| 组件 | 职责 |
|------|------|
| **OntologyEngine** | 本体模型存储 + 推理（图数据库为底座）|
| **EventRuleEngine** | 硬/软约束规则执行 + 事件响应链 |
| **DynamicUIGenerator** | 根据意图 + 本体实时生成 UI |
| **显示栈** | virtio-gpu 驱动 + 合成器 + 渲染管线 |
| **应用注册表** | 应用 = 本体模型实例化（运行时可加载）|

### 8.2 GUI 显示栈选型（待决策）
- 显示驱动：virtio-gpu（QEMU 支持）
- 合成器：**TBD**（自研最小合成器 vs 移植现成方案）
- 渲染：浏览器引擎（Tauri 风格 WebView vs 自研最小渲染器 vs Skia 子集）
- **TBD**：选择路径对路线图影响巨大，建议 S6 启动前专门评审。

### 8.3 退出标准
- 动态 UI 可基于意图实时生成（demo：用户说"整理会议录音" → UI 自动出现录音列表 + 任务进度面板）。
- 显示栈在 QEMU virtio-gpu 上跑通 60 FPS。

---

## 9. 与 Synapse 内核层的依赖矩阵

| 服务组件 | 依赖内核特性 | 内核里程碑 |
|----------|-------------|-----------|
| 日志 / 监控 / 配置中心 | 文件读写 + IPC | P4 |
| 安全网关 | 网卡能力 + IRQ 转发 + 通道 | P4.5 + P5 |
| AgentBus | Notification 原语 + capability | P4 |
| ComputeManager | 资源核算原语 | P3 + P4 |
| StorageQuota | 文件系统 + capability | P6 存储栈 |
| Supervisor Tree | 监督原语 + death notification | P4 |
| 行为围栏 | freeze 原语 + 频率计数 | P3 + P4 |
| 记忆 / 自进化 | 存储栈 + 进程隔离 | P6 存储栈 |
| WASM 沙箱 | 进程隔离 + capability | P4 |
| 显示栈 | virtio-gpu 驱动 + GUI 进程 | P6 显示驱动 |

---

## 10. 与 NeuCore OS Phase 0~8 的映射

原始构想对话中 NeuCore 路线图作为参考。本文 S 阶段与之对位：

| NeuCore Phase | 对应 Synapse Service Phase |
|--------------|--------------------------|
| Phase 0 空壳启动 | 内核 P1 |
| Phase 1 基础服务（SQLite/Web/网关/日志）| S1 |
| Phase 2 Agent 运行时（AgentBus/Scheduler/Planner/Executor）| S2 |
| Phase 3 资源管理（Isolator/Manager/Quota/Hub）| S3 |
| Phase 4 记忆系统（STM/Episodic/Semantic/MemoryEngine/Reflector）| S4 |
| Phase 5 模型集成（llama.cpp + Planner）| S4 + S5（按 §6.3 AI 收敛决策选择路径）|
| Phase 6 多 Agent + 安全 | S5 |
| Phase 7 安全审计 + 角色鉴权 + 镜像裁剪 + 签名 + 长稳 | S5 + S6 持续 |
| Phase 8 多架构（ARM/RKNN）| 与 Synapse Phase 6 并行 |

---

## 11. 服务层里程碑顺序建议

```
P4 IPC 通 → S1 基础服务落地（最小可用）
         ↓
         S2 Agent 运行时（demo：远程 LLM API 调用）
         ↓
         S3 资源管理 + 监督树（demo：子 Agent 超额被冻结 / 崩溃重启）
         ↓
P5 外交工具 → S4 记忆系统接入（demo：跨会话记住偏好）
         ↓
         S5 多 Agent + WASM + 安全围栏（demo：用户跑任意代码）
         ↓
P6 存储栈 + virtio-gpu → S6 本体 + 动态 UI（demo：意图 → 实时 UI）
```

---

## 12. 关键风险与待决策

| 风险 | 说明 | 缓解 |
|------|------|------|
| S 阶段被无限推迟 | 个人开发者精力有限，内核未稳就上 S | **强制规则**：内核 P4 通过即开工 S1；S1 通过即开工 S2，依此类推 |
| 数据层依赖存储栈 | S4 需要存储栈；P6 存储栈复杂 | S4 早期可用内存实现（牺牲持久化换取进展）|
| GUI 显示栈工作量巨大 | 浏览器引擎移植堪比内核 | S6 启动前专门评审；考虑先用 Tauri-WebView 验证思路 |
| WASM 沙箱性能 | WASM 启动 + JIT 不一定划算 | S5 启动前 benchmark；可改用纯 Rust 沙箱 |
| 多编程环境的边界 | "原生支持多种编程环境"承诺过大 | **TBD**：是否只承诺 WASM + Rust，足够覆盖大多数工具 |

---

## 13. 硬件信任根的设计归属 *(评审补充)*

原始构想提出"芯片—系统全链路信任"三件套：**TPM 可信启动链、TEE 可信执行环境、后量子密码（ML-KEM / ML-DSA / SLH-DSA）**。此前它们只在 [需求目标 R9](../../需求目标.md) 中占一行、在 Phase 6 待办中占一行，**没有设计归属**——这正是"愿景流失"的典型风险点。

本节明确归属与前置，确保在 Phase 6 决策时不是从零开始。

### 13.1 三项能力的分层归属

| 能力                                             | 归属层                                                            | 与内核的关系                                          | 引入阶段 |
| ------------------------------------------------ | ----------------------------------------------------------------- | ----------------------------------------------------- | -------- |
| **TPM 可信启动链**（度量启动 + 密封存储）        | **引导层 + 内核**（HAL 需暴露 TPM 接口）                          | 度量值需覆盖内核镜像；审计事件可密封到 TPM PCR        | Phase 6+ |
| **TEE 可信执行环境**                             | **内核 + 硬件抽象**（如 AMD SEV / Intel TDX / ARM CCA）           | 核心里"受保护 Agent 的地址空间"需硬件隔离支撑         | Phase 6+ |
| **后量子密码**                                   | **纯用户态库**（外交工具 + 审计服务）                             | 与内核解耦，仅影响签名算法选择                        | 可提前   |

### 13.2 现在就要做的"廉价埋点"（避免 Phase 6 返工）

- **`CryptoProvider` trait 抽象**：外交工具与审计服务的签名 / 密钥交换**不得直接硬编码算法**。首期实现 Ed25519 + X25519，但接口按"可替换为 ML-DSA / ML-KEM"设计。这是**唯一必须现在做**的事，成本极低。
- **审计事件的签名字段预留算法标识**（如 `sig_alg: u8`），避免后期迁移时改变事件格式。
- **HAL 预留 `TrustRoot` trait**（可为空实现），不实现任何方法，仅占位——防止 Phase 6 时侵入式修改核心层。

#### 13.2.1 `CryptoProvider` trait 接口草案 *(本窗口新增)*

```rust
// user/diplomat/src/crypto/provider.rs (概念设计)
pub trait CryptoProvider: Send + Sync {
    /// 签名（算法作为参数，支持后量子迁移不破坏业务层）
    fn sign(&self, alg: SigAlgorithm, msg: &[u8]) -> Result<Signature, CryptoError>;
    fn verify(&self, alg: SigAlgorithm, msg: &[u8], sig: &Signature) -> Result<bool, CryptoError>;

    /// 密钥交换（ephemeral 模式，每次会话重新协商）
    fn kx_ephemeral(&self, alg: KxAlgorithm) -> Result<(KxPublic, KxPrivate), CryptoError>;
    fn kx_derive(&self, alg: KxAlgorithm, priv: &KxPrivate, peer_pub: &KxPublic)
        -> Result<SharedSecret, CryptoError>;

    /// 哈希（审计事件指纹 / 内容去重）
    fn hash(&self, alg: HashAlgorithm, data: &[u8]) -> Digest;
}

// 算法标识枚举（首期只实现第一项，预留后量子位）
pub enum SigAlgorithm { Ed25519,        /* Phase 6: */ MlDsa65, SlhDsaSha2Small }
pub enum KxAlgorithm { X25519,         /* Phase 6: */ MlKem768, MlKem1024 }
pub enum HashAlgorithm { Sha256,       /* Phase 6: */ Sha3_256, Blake3 }

// 签名不透明封装 —— 业务层只能"持有 + 序列化"，不解释字节内容
pub struct Signature { alg: SigAlgorithm, bytes: Vec<u8> }
```

**关键设计约束**：
- `Signature` 是不透明类型：业务层不得解析内部字节，避免算法迁移时业务层代码扩散改动
- `CryptoProvider` 以 `Arc<dyn CryptoProvider>` 单例注入，首期注入 Ed25519 实现，Phase 6 替换 ML-DSA 实现**不改业务层**
- 审计事件签名字段使用 `Signature`（见 §13.2.2），与外交工具跨层解耦

#### 13.2.2 审计事件 `sig_alg` 字段 *(跨文档对齐 Doc 04 §8.1)*

将 Doc 04 §8.1 的 `signature: [u8;64]` 升级为 `signature: Signature`（包含算法标识），Phase 6 后量子迁移**不破坏审计事件格式**。详见 Doc 04 §8.1 同步修订。

#### 13.2.3 HAL `TrustRoot` trait 占位 *(本窗口新增)*

```rust
// kernel/src/hal/trust_root.rs (概念设计，首期全部为空实现)
pub trait TrustRoot: Send + Sync {
    /// TPM PCR 扩展（Phase 6+）
    fn extend_pcr(&self, _pcr: u32, _digest: &[u8; 32]) -> Result<(), HalError> {
        Err(HalError::NotImplemented)
    }
    /// TPM 密封（Phase 6+）
    fn seal(&self, _data: &[u8], _policy: &SealPolicy) -> Result<Vec<u8>, HalError> {
        Err(HalError::NotImplemented)
    }
    /// TPM 解封（Phase 6+）
    fn unseal(&self, _sealed: &[u8]) -> Result<Vec<u8>, HalError> {
        Err(HalError::NotImplemented)
    }
}

// 首期默认实现：无 TPM/TEE 环境
pub struct NullTrustRoot;
impl TrustRoot for NullTrustRoot {}
```

**设计原则**：
- trait 方法签名按 Phase 6 TPM 2.0 规范设计，首期**全部返回 `NotImplemented`**
- 任何代码路径**不得假设 TrustRoot 可用**——必须先查询 trait 能力（`if trust_root.extend_pcr(...).is_ok()`）
- Phase 6 集成真实 TPM 时，只需实现新 struct（如 `Tpm2TrustRoot`），不改 trait 接口

### 13.3 明确不做（首期）

- 不集成 TPM / TEE（QEMU 软件模拟无意义，且会污染内核复杂度）。
- 不实现后量子算法（依赖 crate 成熟度）。

> **风险登记**：见 [需求目标 R9](../../需求目标.md)。本节的作用是让 R9 从"一行风险"变成"有归属、有埋点、有阶段"的可执行条目。

---

## 14. 参考 (References)

- 原始构想对话 [`../../原始思路.md`](../../原始思路.md)
- NeuCore OS / KernelGPT / SanjayOS（参考路线图）
- Palantir Foundry / AIP（本体驱动应用参考）
- 华为 openEuler 异构融合 OS
- MOBIMEM DisGraph（档案记忆检索方案）
- GEPA 算法（Prompt 反向传播优化）