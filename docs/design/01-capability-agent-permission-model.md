# 设计文档 01：Capability 与 Agent 权限模型

> 状态：**PROPOSED 完成**（待确认升级为 DECIDED）
> 关联需求：NFR3 安全性、IPC `capability_token`、FR6/FR9
> 关联里程碑：Phase 4（用户态与 IPC）
> 最后更新：2026-09-25

---

## 0. 文档目的

定义 Synapse 内核的**能力（Capability）系统**与 **Agent 权限模型**。这是 Synapse "AI 原生 + 安全" 定位的核心卖点，也是与 Linux（DAC/MAC）拉开差距的关键。本文档回答：一个 token 由谁铸造、存在哪里、如何校验、如何委托与撤销。

### PROPOSED 决策汇总

| 决策项 | PROPOSED 方案 | 章节 |
|--------|--------------|------|
| 根 capability 铸造 | init 进程全量铸造（与 HAL 分离）| §3.1 |
| 撤销算法 | derivation tree 遍历（seL4 风格）| §3.4 |
| 委托语义 | 保留父子链（用于撤销）| §3.3 |
| 数据结构 | Capability.parent + CapTable 256 槽 + CapRef=u8 | §4 |
| 对象生命周期 | Live→Revoking→Retired→Freed 状态机 + generation 校验 | §4.2 |
| agent_id 管理 | init 进程统一管理命名空间 | §7 |

> ✅ Phase 4 核心设计决策已完成，剩余 TBD 为安全评审/性能基准相关。

---

## 1. 设计目标与非目标

### 1.1 目标
- 基于 **Object-Capability** 模型：权限 = 持有不可猜测的能力对象，"能力即权限"。
- 内核在每次跨边界调用（IPC / syscall）时以**可控开销**校验能力（满足 NFR2 微秒级）。
- 支持能力的**委托（delegation）**：Agent A 可将受限子集授予 Agent B。
- 支持能力的**撤销（revocation）**：撤销后派生能力一并失效。

### 1.2 非目标（首期裁剪）
- 不实现完整的 seL4 CNode 树（首期用扁平 per-process cap table）。
- 不实现跨地址空间的能力持久化 / 重启存活。
- 不做细粒度的信息流控制（IFC / Bell-LaPadula）。

---

## 2. 核心概念

### 2.1 能力对象（Capability）
一个能力 = `{ 指向内核对象的引用 + 权限位掩码 }`。

| 内核对象类型 | 示例权限位 |
|-------------|-----------|
| Endpoint（IPC 端点） | SEND / RECV / REPLY |
| MemoryRegion（内存区） | READ / WRITE / EXEC / GRANT |
| Device（MMIO 设备） | MAP / IRQ_BIND |
| Process / Thread | SCHED_SET / SIGNAL / DEBUG |

### 2.2 能力槽（Capability Slot）
- 每个进程持有一张 **CapTable**：`Vec<Option<Capability>>`，索引即 `cptr`（capability pointer）。
- 用户态 syscall 传递的是 `cptr`（u32 索引），**不是**裸指针。

### 2.3 Agent 身份
- `agent_id`：由**内核在发送路径上盖章**，用户态不可自报（防伪造）。
- 与 capability 的关系：`agent_id` 标识"谁"，capability 标识"能做什么"。

---

## 3. 能力的生命周期

```
铸造(Mint) → 持有(Hold) → 使用(Invoke) → 委托(Delegate) → 撤销(Revoke) → 销毁(Destroy)
```

### 3.1 铸造（Mint）
- 谁有权铸造根能力？**PROPOSED → init 进程全量铸造**（候选之二：HAL 分散铸造）。
  - **理由**：
    - **单一责任源**：所有能力均从 init 的根能力派生，撤销链路 / 委托链可全局追踪；
    - **对齐 seL4 模型**：seL4 用 root task（即 init 进程）持有全部根能力，再由 root task 通过 spawn + delegate 分配给其他用户态服务——经过形式化验证的设计；
    - **避免 HAL 分散铸造的复杂度**：HAL 分散铸造引入"多源能力管理"问题（哪个设备铸造了哪些能力、跨设备能力的依赖关系），对微内核收益不抵成本。
  - **init 进程职责**：
    1. 内核启动时，内核**仅为 init 进程**铸造"根 CapTable"（包含所有内核对象的根引用 + 全权限位）；
    2. init 进程通过 `spawn` 系统调用创建其他进程时，按需 `delegate` 子集能力给新进程（attenuation-only，不可放大）；
    3. NIC 等独占资源：init 启动外交工具时，**仅**向外交工具 cast NIC capability（见 §6.1 唯一网络出口不变量）。
  - **与 HAL 的关系**：HAL 仍负责注册设备对象（设备枚举、MMIO 地址、IRQ 号），但**设备对象本身由内核统一持有**；init 进程在启动期通过特殊 syscall（`root_cap_enumerate_devices`）一次性获取所有设备的 capability 引用——HAL 不直接铸造 capability。
- token 熵源：capability 的不可猜测性依赖随机位。**TBD**：是否引入 RDRAND / 启动期熵池？

### 3.2 校验（Invoke / Verify）
- 校验时机：syscall 入口 + IPC 发送路径。
- 校验内容：cptr 是否有效 → 权限位是否覆盖本次操作 → 对象是否存活。
- 性能：O(1) 数组索引；**TBD** 是否需要在热路径缓存最近校验结果。

### 3.3 委托（Delegate）
- 语义：派生一个**权限子集**（attenuation-only，不可放大）。
- **PROPOSED → 保留父子关系**（用于撤销时级联）：
  - 每个 capability 对象额外维护 `parent: Option<CapRef>` 字段，指向铸造它的父 capability；
  - 根 capability（由 init 铸造）的 `parent = None`；
  - 开销：每个 capability 多一个 pointer（O(1) 空间），可接受；
  - **必要性**：撤销算法（§3.4）需要从被撤销的 capability 出发，遍历所有派生副本——若无父子链，则无法实现级联撤销。

### 3.4 撤销（Revoke）
- **PROPOSED → derivation tree 遍历**（seL4 式）：
  - 撤销某 capability 时，从该 capability 出发，沿 `parent` 反向遍历其所有派生副本，逐一失效；
  - 复杂度：O(n)，n = 该 capability 的派生树节点数（通常很小，因为微内核 capability 数量受控）；
  - 与 §3.3 父子链配合，实现级联撤销。
  - **候选算法比较**：
    | 算法 | 优势 | 劣势 | 决策 |
    |------|------|------|------|
    | **derivation tree 遍历** | 简单、即时失效、与 seL4 对齐 | O(n) 遍历（n 小则无碍）| ✅ PROPOSED |
    | epoch-based reclamation | 适合高并发、无锁 | 复杂、撤销有延迟、微内核不需要 | ❌ |
    | 版本号失效 | 极简 | 撤销有延迟（旧 cap 在版本号更新前仍可用）、不符合"即时撤销"语义 | ❌ |
- 与 IPC 在途消息的交互：撤销时已发出但未接收的消息如何处理？
  - **PROPOSED → 消息级 cap 校验**：消息到达接收方时，接收方内核再次校验消息中携带的 capability 是否仍有效——若已撤销，则消息丢弃 + 审计事件。

### 3.5 审计钩子（Audit Hook） *(原始构想新增)*
- 内核在以下**事件点**产生 append-only 审计事件（推送到独立审计服务，详见 [设计文档 03 §6.3 审计事件流](03-ipc-message-and-single-copy-path.md)）：
  - **Capability 校验**：每次 `cptr` 校验成功 / 失败均产生事件（含调用方 `agent_id`、目标对象、权限位）；
  - **Capability 创建 / 销毁 / 委托 / 撤销**；
  - **IPC send/recv/reply** 关键路径（防敏感对话被旁路）；
  - **进程生命周期**（spawn / exit / fault / freeze / thaw）。
- 事件内容**永不包含用户态业务数据原文**（防审计日志泄密），仅含对象引用、权限位、`agent_id`、通道类型等元信息。
- 与 capability 的关系：审计是 capability 校验的**被动副作用**，不参与权限决策；权限决策只看 capability，审计只看历史。

---

## 4. 数据结构草案

```rust
// kernel/src/cap/types.rs

/// Capability 引用（指向本进程 CapTable 中的槽位）
pub type CapRef = u8;  // ★ 8-bit，对齐 §7 PROPOSED：per-process 256 槽上限

/// 能力对象
#[derive(Clone)]
pub struct Capability {
    pub obj: ObjRef,                     // 指向内核对象（带引用计数 / slot id）
    pub rights: Rights,                  // bitflags
    pub badge: u32,                      // 可选：用于区分同一 endpoint 的不同调用者
    pub parent: Option<CapRef>,          // ★ 父 capability 引用（对齐 §3.3 PROPOSED：保留父子链）
                                         // None = 根 capability（由 init 铸造）
                                         // 撤销时沿此字段反向遍历 derivation tree
}

bitflags! {
    pub struct Rights: u32 {
        const SEND  = 1 << 0;
        const RECV  = 1 << 1;
        const REPLY = 1 << 2;
        const READ  = 1 << 3;
        const WRITE = 1 << 4;
        const EXEC  = 1 << 5;
        const GRANT = 1 << 6;           // 是否允许再委托（派生子 capability）
    }
}

/// 每进程一张能力表
pub struct CapTable {
    slots: [Option<Capability>; 256],    // ★ 固定 256 槽（对齐 §7 PROPOSED）
    free_list: [u8; 256],                // 空闲槽位栈（O(1) 分配 / 释放）
    free_top: u8,                        // 栈顶指针
}

impl CapTable {
    /// 分配新槽位，返回 CapRef（8-bit 索引）
    pub fn alloc(&mut self) -> Option<CapRef> { /* ... */ }
    /// 释放槽位
    pub fn free(&mut self, cap: CapRef) { /* ... */ }
    /// 按 CapRef 查找 capability（O(1) 数组索引）
    pub fn get(&self, cap: CapRef) -> Option<&Capability> { /* ... */ }
}
```

> **设计约束**：
> - `Capability` 必须是 `Clone`（不可 `Copy`）—— 因为 `parent: Option<CapRef>` 字段需要追踪派生关系，拷贝时必须显式处理父子链；
> - `CapRef = u8`（而非 u16 / u32）—— IPC 消息中 capability 转移仅占 1 byte / cap，对齐 NFR2 微秒级延迟目标；
> - `CapTable::slots` 固定 256 项 —— 避免动态扩容带来的锁竞争（虽然首期单核，但锁语义按多核就绪）。

### 4.1 内核对象类型全集

除 §2.1 的基础四类外，下述对象类型由系统服务层与外交工具使用（**同一 CapTable 机制，不同对象语义**）：

| 内核对象类型 | 权限位 | 用途 | 引入阶段 |
|-------------|-------|------|---------|
| `Diplomat` | `REQUEST` / `REGISTER_CHANNEL` | Agent 向外交工具发起业务请求 | Phase 5 |
| `AuditLog` | `APPEND_KERNEL_EVENT` / `QUERY` | 审计事件写入 / 只读查询 | Phase 4 |
| `Storage` | `WRITE` / `READ` / `DELETE` | 业务节点持久化（必须经 Security Gateway）| Phase 6 |
| `WasmRuntime` | `EXEC` | L5 代码运行级沙箱执行 | S5 |
| `Process`（管理面）| `ADMIN`（freeze/thaw/kill）| 监督树 / 行为围栏 | Phase 4 |

> **原则**：内核只认识"对象 + 权限位"，不认识"L1~L5"或"业务通道"——后者是用户态策略（见 §5）。

### 4.2 内核对象生命周期与 Generation *(评审补充，对齐 [需求评审 §2.3](../requirements-review-and-supplement.md))*

**问题**：Capability 指向对象槽位（slot），若 slot 被释放后复用，旧 capability 可能"复活"指向新对象（use-after-free 变体）。

**PROPOSED → 统一对象状态机 + generation 校验**：

```text
Live ──► Revoking ──► Retired ──► Freed
  │          │            │
  │          │            └─ 对象已不可用，等待引用计数归零后释放内存
  │          └─ 正在撤销派生 capability，新 invoke 返回 EOBJECT_RETIRED
  └─ 正常可用状态
```

**ObjRef 带 generation**：

```rust
/// 对象引用 = slot index + generation（防 slot 复用攻击）
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ObjRef {
    pub index: u32,       // 内核对象表索引
    pub generation: u32,  // ★ 每次 slot 复用时递增
}

/// 对象状态
pub enum ObjState {
    Live,
    Revoking,    // 撤销进行中（§3.4 derivation tree 遍历）
    Retired,     // 已撤销，不可 invoke
    Freed,       // 内存已释放（不应被引用）
}
```

**校验规则**：

- `CapTable::get(cptr)` 返回 capability 后，内核**必须**校验 `cap.obj.generation == obj_table[cap.obj.index].generation`；
- generation 不匹配 → 返回 `EOBJECT_RETIRED`（-10），**不 panic**；
- slot 复用时 generation 必须递增（wrap-around 用 u32 足够，2^32 次复用不现实）；
- 对象进入 `Revoking` 状态后，所有新 invoke 立即返回错误，**不等待遍历完成**（避免阻塞热路径）。

**进程退出时的对象回收顺序**（对齐 [需求评审 §5 跨模块契约](../requirements-review-and-supplement.md)）：

1. 标记进程为 `Exiting`，拒绝新 syscall；
2. 撤销该进程持有的所有 capability（进入 `Revoking`）；
3. 唤醒所有阻塞在该进程相关 Endpoint/Notification 上的线程（`PeerDied` 错误）；
4. 回收地址空间（页表 + 物理页）；
5. 回收内核对象（TCB、CapTable 等）；
6. 发送 death notification 给父进程（若已注册）。

> **设计约束**：generation 校验是 O(1) 比较，不影响 NFR2 微秒级目标。

---

## 5. Agent 五级权限映射（L1~L5） *(原始构想新增)*

原始构想："五级权限划分（L1~L5）不可跨级自动授权"。**内核 capability 是底层原语，Agent 行为级权限（L1~L5）是上层策略**——两者分层但需映射。

| Agent 行为级 | 含义 | 必须持有的 capability 子集 |
|-------------|------|-----------------------------|
| **L1 读取类** | 读文件、查数据 | `MemoryRegion::READ` + `Endpoint::RECV`（只读 endpoint）|
| **L2 修改/写入类** | 写文件、改数据 | L1 + `MemoryRegion::WRITE` |
| **L3 删除类** | 删除文件 / 对象 | L2 + `MemoryRegion::WRITE`（含不可逆写入）+ 单独 `DELETE` 位 |
| **L4 对外交互类** | 调用外部 API / 收发网络 | L1 + `Diplomat::REQUEST`（外交工具代发的 capability）+ `Endpoint::SEND`（向外交工具）|
| **L5 代码运行类** | 执行任意代码 / WASM 沙箱 | L4 + `WASM_RUNTIME::EXEC` 或独立沙箱进程 capability |

**强制规则**：
- **不可跨级自动授权**：L1 capability **不能**隐式包含 L2+ 权限；高层级必须显式授予。
- **高风险必须经用户确认**（原始构想）：L4 / L5 操作触发 GUI 弹窗 / 语音确认（系统服务层 S5 实现）。
- **授权链路可审计**：每次 capability 授予产生审计事件（§3.5），形成授权链。

> **TBD**：L1~L5 是策略层概念，存于用户态 SecurityGuard；内核仅负责强制 capability 强制规则（"不可跨级"靠 SecurityGuard 在授予时拦截，不靠内核理解 L1~L5）。

### 5.1 人工确认通道的时间差问题 *(评审补充)*

**问题**：L4（对外交互）在 **Phase 5** 随外交工具上线即成为现实风险，而 GUI 确认弹窗属于 **S6**——中间存在跨越 P5 / S1~S5 的**确认通道真空期**。若无预案，"高风险必须经用户确认"这条规则在很长一段时间内是空转的。

**分阶段预案**（确认通道随可用界面能力逐级替换，**规则本身始终生效**）：

| 阶段 | 可用界面 | 确认通道 | 适用场景 |
|------|---------|---------|---------|
| **P5 ~ S4** | 仅串口控制台 | **串口交互确认**：控制台打印待确认动作摘要 + `y/n` 阻塞等待；超时（默认 30s）按拒绝处理 | 开发 / 单机调试 |
| **S5** | 无 GUI，但有 Toolchain 服务 | **策略文件白名单 + 审计留痕**：事前声明允许的域名 / API 端点，命中白名单免确认，其余一律拒绝 | 无人值守长稳测试 |
| **S6** | 有 GUI | **GUI 弹窗 / 语音确认**（原始构想目标形态） | 产品态 |

**关键约束**：

- Phase 5 起，外交工具的 L4 动作**默认拒绝**——除非命中显式配置的白名单，否则必须有一条可用的确认通道放行。**不允许"确认通道不可用 → 默认放行"**。
- 超时即拒绝（fail-closed），不得 fail-open。
- 每次确认 / 拒绝同样产生审计事件，形成授权链（§3.5）。

---

## 6. 与其他子系统的交互

- **IPC（见文档 03）**：IPC 消息头携带 `agent_id`（内核盖章）+ 可选 `capability` 随消息转移（cap transfer，seL4 风格）。
- **用户态驱动（Phase 5）**：外交工具通过持有 `Device + IRQ_BIND` 能力来合法接管 MMIO 与中断。
- **调度（见需求 sched）**：`SCHED_SET` 能力控制谁能改优先级，防止 Agent 互相抢占。

### 6.1 唯一网络出口不变量 *(原始构想新增)*

原始构想："只要有联网行为就必须通过外交工具进行处理和转发"。

**架构性保证**（不依赖应用自律）：

- 网卡的 `Device` + `IRQ_BIND` capability **仅**在内核启动期为外交工具进程铸造一次，其他进程**物理上无法获取**。
- 内核 Capability 创建路径中加白名单：网卡的 capability 仅允许 mint 到外交工具的 `agent_id`，其他 mint 请求一律拒绝并审计。
- 任何用户态进程尝试直接 mmap 网卡 BAR 范围（含 PCI 枚举出的 MMIO）→ 缺页处理判定为非法访问，杀进程 + 审计事件。
- 即使 DNS / 时间同步等"系统级"网络服务，也必须走外交工具白名单通道（详见 [设计文档 04 §3](04-diplomat-channel-architecture.md)）。

**Phase 5 退出测试**（不可绕过的验证）：
- E1：尝试注册 NIC capability 的非外交进程 → 内核拒绝 + 审计事件。
- E2：外交工具被 freeze 时发起 HTTP 请求 → 连接超时或拒绝（验证"外交冻结即断网"）。
- E3：尝试绕过外交工具构造 L2 帧 → 因无 NIC capability，无法发出任何字节。

---

## 7. 待决策清单（Phase 4 前必须收敛）

- [x] ~~根能力铸造策略（init 全量 vs HAL 分散）~~ → **PROPOSED：init 进程全量铸造**，理由见 §3.1
- [ ] token 熵源与不可猜测性强度（需安全评审，保留 TBD）
- [x] ~~撤销算法选型（derivation tree vs epoch vs version）~~ → **PROPOSED：derivation tree 遍历**，理由见 §3.4
- [x] ~~委托是否保留父子链~~ → **PROPOSED：保留**（撤销级联的必要前提），理由见 §3.3
- [x] ~~cap table 上限与增长策略~~ → **PROPOSED：per-process 上限 256 槽位**（8-bit cptr 索引），线性扫描空闲槽；微内核单进程 cap 数量受控，256 足够
- [x] ~~是否支持 capability 随 IPC 消息转移（cap transfer）~~ → **PROPOSED：支持**（seL4 风格，send 携带 N 个 cptr，内核安装到接收方 CapTable）
- [x] ~~`agent_id` 分配与管理（谁负责命名空间）~~ → **PROPOSED：init 进程统一管理**（与根能力铸造策略对齐：init 在 spawn 子进程时分配 agent_id，写入进程结构体，内核盖章到 IPC 消息头）
- [ ] 审计事件的批量策略（每事件 IPC vs 批量提交，对热路径影响）—— 需性能基准测试后决策
- [ ] L4 / L5 操作的用户确认通道分阶段预案（§5.1）：串口确认的超时值、白名单配置格式；GUI 弹窗形态在 S6 落地
- [ ] 唯一网络出口的白名单策略：DNS / NTP 等系统级服务的白名单由谁维护（外交工具自维护 vs 内核静态配置）

---

## 8. 参考资料

- seL4 Capability 模型（CNode / CSpace）
- Fuchsia Zircon handles
- Object-capability model（Wikipedia / Mark Miller 论文）
