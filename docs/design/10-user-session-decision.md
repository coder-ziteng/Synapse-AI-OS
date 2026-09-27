# 设计文档 10：用户会话体系决策 (User Session Model Decision)

> 状态：**DECIDED**（候选 C 双轨制已由用户批准，2026-09-27）
> 关联需求：[需求目标 GAP-1/R13「用户会话与登录体系（决策点）」](../需求目标.md)、task.json `requirement_gaps` GAP-1
> 关联任务：**P6.11-T1**（本任务：决策报告）→ 解锁 **P6.11-T2**（多用户会话 + 锁屏实施）
> 关联阶段：Phase 6 + S6 桌面（空间外壳，[Doc 07](07-display-stack-and-spatial-shell.md)）设计同步收敛——**不阻塞 S6 自身推进**
> 最后更新：2026-09-27

---

## DECIDED 决策汇总

| 决策项 | DECIDED 方案 | 章节 |
|--------|-------------|------|
| 会话体系路径 | **候选 C 双轨制**：capability 命名空间做内核隔离强制 + 会话 token 做用户态认证交互 | §2/§3 |
| 两轨接缝 | SessionManager 把 token 验证结果翻译为 cap 铸造/撤销；token 不进内核，内核唯一强制点仍 = cap（D1 不破） | §3 |
| 登出/锁屏语义 | 登出 = 撤销会话根 cap → 派生子树级联失效（复用 Doc 01 §3.4 既有机制，无新内核原语） | §1 D3/§3 |
| 身份扩展 | agent_id 命名空间加会话维 `(user_session, agent_id)`，内核盖章机制不动 | §1 D4 |
| 首期形态 | 单用户默认不启用多用户维（现行为零破坏），启用条件 = S6 进入多用户/锁屏实施 | §3 |
| 安全边界（诚实登记） | R6/R9 缺位下，首期 token = 在场性断言 + UI 锁，非强安全边界 | §1 D6 |

---

## 0. 文档目的

现状是**单用户 capability 模型**：无登录界面 / 用户账户 / 会话隔离 / 锁屏（GAP-1，2026-09-27 登记）。若产品形态（S6 桌面 = 空间外壳 + 虚拟人）需要多用户或会话管理，权限层与交互层如何落子必须先收敛，否则 P6.11-T2 无法启动。本文档回答四个问题：

1. 会话/登录体系受哪些既有架构约束（哪些是碰不得的不变量）？
2. 三个候选（capability 命名空间 / 会话 token / 双轨）各自的隔离力、认证力与工作量如何？
3. 推荐方案的内核侧/用户侧职责如何切分？
4. 与 S6 桌面设计的收敛点是什么？

## 1. 问题约束

| # | 约束 | 出处 | 对会话体系的影响 |
|---|------|------|-----------------|
| D1 | **单一强制点**：权限 = 持有不可猜测的能力对象，内核在每次 syscall/IPC 校验；不引入第二权威源 | [Doc 01 §1.1/§2.1](01-capability-agent-permission-model.md) | 会话 token 若直接进内核当权限用 = 破坏模型；token 只能活在用户态认证交互层，最终**翻译成 cap 铸造/撤销** |
| D2 | CapTable 为 **per-process 扁平表**（256 槽，CapRef=u8），首期明确**不做** CNode 树 | [Doc 01 §1.2/§2.2/§7](01-capability-agent-permission-model.md) | "per-user CapTable"不是新内核结构，而是**会话根进程派生子树**的视图：同一用户会话 = 同一棵 derivation tree |
| D3 | 撤销 = **derivation tree 级联**（seL4 风格），委托保留父子链 | [Doc 01 §3.3/§3.4](01-capability-agent-permission-model.md) | 登出/锁屏的强制语义天然存在：撤销会话根 cap → 整棵派生子树失效；**无需新内核机制** |
| D4 | `agent_id` 内核盖章、init 统一管理命名空间（防伪造 R5） | [Doc 01 §2.3/§7](01-capability-agent-permission-model.md) | 多用户 = agent_id 命名空间加一维 `(user_session, agent_id)`；盖章机制不变，向后兼容 |
| D5 | GUI/Input/显示 cap 由 shell 进程独占，锁屏属交互层；语音/手势输入走 InputFusionBus | [Doc 07 §1.3/§6](07-display-stack-and-spatial-shell.md) | 锁屏=shell 独占前台 + InputService 拦截输入；解锁的**验证**是用户态服务间协议，**执行**仍是 cap 重授予 |
| D6 | 无持久化存储（R6）、无硬件信任根（R9） | 需求目标 §五 | 会话跨重启恢复受存储栈排期约束（早期内存态近似）；口令派生强度受限——首期 token 定位为**在场性断言 + UI 锁**，非强安全边界（诚实登记） |
| D7 | 跨 OS 身份走 DID/VC（Phase 6+） | [Doc 05](05-inter-os-diplomacy-protocol.md) | 本地会话 ≠ 跨 OS 外交身份，两套体系不混用、不互相冒充 |

## 2. 三候选对比

### 候选 A：capability 命名空间（per-user CapTable + 命名空间隔离）

- **内容**：多用户映射为 cap 派生树的命名空间维度——每个用户会话挂接一棵以会话根进程为根的派生子树，仅同 namespace cap 可互相访问/引用。
- **优点**：完全长在 D2/D3 既有机制上（派生 + 级联撤销），内核改动最小；隔离有强制力（内核校验）；与 agent_id 盖章（D4）正交兼容。
- **缺点**：**没有认证语义**——"谁有权创建/解锁一个会话命名空间"缺凭据载体；锁屏解锁、多用户切换 UI 没有标准 bearer；纯 A 方案会把认证悄悄推给"物理在场默认信任"，与产品形态（桌面多人使用）不匹配。
- **工作量**：内核侧小（命名空间一维 + init 铸造策略）；交互侧缺口未闭合。

### 候选 B：会话 token（认证 → token 颁发 + 撤销）

- **内容**：传统登录会话模型——认证服务校验凭据 → 颁发 session token（颁发/撤销/过期三态）→ 持 token 获得操作权。
- **优点**：认证/生命周期语义完整，行业熟悉度高；与 S6 登录/锁屏 UI 直接对得上。
- **缺点**：token 若无内核强制力则退化为 **UI 装饰**（绕过 shell 的进程照样访问 cap）；若要进内核强制则直接违反 D1（第二权威源 + 与 cap 校验双轨打架）；且 token→cap 的映射仍要落回 A 的派生树机制，B 单飞是**半套方案**。
- **工作量**：token 服务本体不大，但"要么无强制力、要么破模型"的二选一困局无解。

### 候选 C：双轨（capability 命名空间做隔离 + 会话 token 做认证）— 推荐

- **内容**：两轨各就各位，接缝只有一处：
  1. **认证交互层（用户态）**：SessionManager 服务（S1 家族）校验凭据 → 颁发/撤销/过期 session token；token 只在 shell ↔ SessionManager ↔ 锁屏 UI 间流转，**不进内核**；
  2. **隔离强制层（内核）**：token 验证通过后，SessionManager（受 init 委托）铸造该会话的根 cap 并建立派生子树（= 命名空间）；解锁 = 重新授予，登出 = 撤销根 cap 级联失效（D3 现成机制）；
  3. agent_id 命名空间加会话维：`(user_session, agent_id)`，盖章不变。
- **优点**：认证有 bearer、隔离有强制点、D1 不破（token 最终翻译为 cap 操作，内核仍只认 cap）；A 与 B 的缺口互补成闭环。
- **缺点**：两个组件（SessionManager + 命名空间维）比 A 单轨多一层协议；首期 D6 约束下口令路径强度有限（已诚实定位：在场性断言）。
- **工作量**：中——主体是用户态服务 + init 铸造策略扩展；内核仅加命名空间一维，无新原语。P6.11-T2 已按 1 窗口排期，吻合。

### 对比矩阵

| 维度 | A：cap 命名空间 | B：会话 token | C：双轨（推荐） |
|------|----------------|---------------|----------------|
| 隔离强制力（内核级） | ✅ | ❌（无）/ ⚠️（破 D1） | ✅ |
| 认证 / 解锁语义 | ❌ 缺 bearer | ✅ | ✅ |
| 架构一致性（Doc 01 不变量） | ✅ | ❌ 二难 | ✅ |
| 新内核机制需求 | 命名空间一维 | 无（但无牙齿） | 命名空间一维（同 A） |
| S6 锁屏/切换 UI 对接 | 勉强 | 直接 | 直接 |
| 登出=级联撤销复用 D3 | ✅ | ❌ 未定义 | ✅ |
| 工作量 | 小（留窟窿） | 小（假装有） | 中（闭环） |

## 3. 推荐方案

**推荐 C：双轨制。** 一句话职责切分：**token 回答"你是谁、是否在场"（用户态认证交互层），capability 命名空间回答"你能动什么"（内核强制层）；两轨唯一接缝 = SessionManager 把认证结果翻译为 cap 铸造/撤销。**

首期形态兼容性：单用户默认**不启用**多用户维度（现行为零破坏），C 是扩展位而非重构；启用条件 = S6 桌面进入多用户/锁屏实施。

## 4. 与 S6 桌面的收敛点（不阻塞 S6 自身推进）

| 收敛点 | 内容 | Doc 07 对应 |
|--------|------|------------|
| 锁屏前台 | shell 独占显示 + InputService 拦截非解锁输入 | §1.3 进程拓扑（shell 持 GUI/Input cap） |
| 登录/解锁 UI | DynamicUIGenerator 生成为普通卡片面板，无特权通路 | §5 UI 生成 |
| 输入凭据 | 口令走键盘事件流；生物特征远期依赖 GAP-2/GAP-3 外设栈 | §6 输入融合 |
| 会话切换动画 | 纯视觉，状态由 SessionManager 驱动 | §4 空间外壳 |

## 5. 决策记录

| 项 | 内容 |
|----|------|
| 决策点 | P6.11-T1：用户会话体系路径选择（A / B / C） |
| 决策选项 | A cap 命名空间 · B 会话 token · C 双轨（推荐） |
| 决策结果 | **候选 C 双轨制**：capability 命名空间做内核隔离强制 + 会话 token 做用户态认证交互；唯一接缝 = SessionManager 把认证结果翻译为 cap 铸造/撤销；首期单用户默认不启用 |
| 决策人 / 日期 | 用户（AskUserQuestion 批准）/ 2026-09-27 |
| 依据 | 本文档 §1~§3；GAP-1/R13；[Doc 01 §3.3/§3.4/§7](01-capability-agent-permission-model.md) |
| 下游 | P6.11-T2 按 C 路径实施（per-user 派生子树 + token 三态 + 锁屏 shell 独占前台 + 会话切换）；task.json decision_log 同步 |

## 6. 与现有文档一致性自检

- **Doc 01**：D1~D4 全部满足——内核唯一强制点=cap；无 CNode 树需求（命名空间仅派生子树视图 + agent_id 加维）；登出复用 derivation tree 级联撤销；agent_id 盖章机制不动只扩命名空间。✅
- **Doc 02**：进程模型 parent 链与 spawn 语义不动；SessionManager 作为 init 委托的用户态服务接入既有 spawn/SupervisorTree 框架。✅
- **Doc 07**：§1.3/§5/§6 收敛点见 §4 表，S6 设计不因本决策改结构。✅
- **Doc 05 / GAP-1**：本地会话与跨 OS DID/VC 身份分界明确（D7）；GAP-1 描述中的候选原文「capability 命名空间 + 会话 token」即本方案 C 的两轨。✅
- **需求目标 R6/R9**：会话持久化与凭据强度受存储栈/信任根缺位约束，已在 D6 登记为已知边界，不假装已解决。✅

## 7. 参考 (References)

- task.json P6.11-T1/T2 任务定义、`requirement_gaps` GAP-1、`.claude/thinking/P6/notes/03-planning.md`
- [Doc 01 Capability 与 Agent 权限模型](01-capability-agent-permission-model.md) §1.2/§2/§3.3/§3.4/§7
- [Doc 02 用户态 ABI 与进程模型](02-userspace-abi-and-process-model.md)、[Doc 05 跨 OS 外交协议](05-inter-os-diplomacy-protocol.md)、[Doc 07 显示栈与空间外壳](07-display-stack-and-spatial-shell.md)
- [需求目标.md](../需求目标.md) §三 Phase 6、§五 R13
