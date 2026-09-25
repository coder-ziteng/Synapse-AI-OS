# 设计文档 03：IPC 消息格式与单拷贝路径

> 状态：DRAFT / 设计细化（含审计事件流）
> 关联需求：FR4 IPC、NFR2 微秒级延迟、FR9 审计事件源
> 关联里程碑：Phase 4（用户态与 IPC）
> 最后更新：2026-09-25

---

## 0. 文档目的

定义 Synapse 微内核"灵魂"——IPC 机制的**消息格式、端点语义、传输路径（单拷贝）、能力转移、以及与调度/中断的交互**。目标是达成 NFR2 的微秒级延迟，同时承载 AI 原生的 `agent_id` / capability 校验。

> ⚠️ 本文档为骨架，各章节列出**待决策问题（TBD）**。

---

## 1. 设计目标与非目标

### 1.1 目标
- **单次拷贝**（single-copy）：数据从发送方地址空间**直接**写入接收方，不经内核中转缓冲区二次拷贝（L4/seL4 标准做法）。
- 同步 IPC 为主（`send` / `recv` / `reply`），语义清晰、易于能力校验。
- 消息头承载 `agent_id`（内核盖章）与可选 capability 转移。
- 大对象走**共享内存 grant**，而非塞进消息体。

### 1.2 非目标（首期裁剪）
- 不做异步 / 多播 IPC（首期点对点同步）。
- 不做优先级继承（priority inheritance）——记为已知风险，见 §7。
- 不做用户态零拷贝 DMA 直通。

---

## 2. 端点（Endpoint）语义

### 2.1 端点对象
- 一个 **Endpoint** 是一个内核对象，由 capability 引用（见文档 01）。
- 内部维护：发送队列（blocked senders）、接收者状态。

### 2.2 三种操作
| 操作 | 语义 |
|------|------|
| `send(ep, msg)` | 阻塞，直到接收方 `recv` 并处理；发送方随后阻塞等待 `reply`（同步 RPC） |
| `recv(ep)` | 阻塞，直到有发送方；收到后可处理并 `reply` |
| `reply(ep, msg)` | 唤醒原发送方，完成一次 RPC 往返 |

### 2.3 待决策
- [ ] `send` 是否需要非阻塞 / 带超时变体（`try_send` / `send_timeout`）？
- [ ] 一个端点是否允许多接收者（竞争消费）？首期建议单接收者。

---

## 3. 消息格式

### 3.1 消息头（内核可见，固定布局）
```rust
#[repr(C)]
pub struct IpcHeader {
    pub sender_agent: AgentId,   // ★ 由内核在发送路径盖章，用户态不可写
    pub label: u32,              // 用户自定义标签（区分同一 endpoint 的请求类型）
    pub payload_len: u32,        // 消息体字节数
    pub cap_transfer_count: u8,  // 随消息转移的能力数量（0..=N）
    pub flags: u8,               // 保留
}
```

### 3.2 消息体（payload）
- **小消息**（≤ 阈值，如 1 个寄存器组 / 一页）：走 §4 单拷贝路径。
- **大对象**：payload 只放一个**共享内存 grant 描述符**（MemoryRegion capability + offset），实际数据不拷贝。

### 3.3 待决策
- [ ] payload 内联阈值（寄存器直传 vs 单页拷贝的分界）
- [ ] 消息体最大长度上限
- [ ] `label` 语义是否够用（对比 seL4 的 badge）

---

## 4. 单拷贝传输路径（核心）

### 4.1 同步 IPC 的天然优势
同步语义下，`send` 时接收方**必然已知**（或即将在 `recv` 时确定）。内核可在 `send` 陷入时：

```
发送方 send(ep, buf, len)
  → 内核找到接收方（或挂起等待 recv）
  → 内核直接把 buf 从【发送方地址空间】拷贝到【接收方地址空间】
  → 一次 memcpy，零中转缓冲区
  → 唤醒接收方
```

对比"双拷贝"（发送方→内核 buf→接收方）省掉一次拷贝与一份内核内存。

### 4.2 拷贝时的地址空间处理
- 发送方与接收方页表不同，内核需能同时访问两者：
  - **TBD 方案 A**：临时映射（kmap）发送方物理页到内核窗口后 `memcpy`。
  - **TBD 方案 B**：在接收方页表中临时映射发送方物理页。
- 拷贝期间需处理缺页（发送方页可能未驻留）。

### 4.3 寄存器直传（fast path）
- 极小消息（≤ 4~6 个 word）可直接放在 syscall 寄存器 / 内核栈帧里传递，**完全跳过 memcpy**。

---

## 5. 能力转移（Cap Transfer）

- `send` 可携带 N 个 capability，内核将它们**安装到接收方 CapTable**（见文档 01 §3.3 委托语义）。
- 转移遵循 attenuation：接收方获得的权限 ≤ 发送方持有的权限。
- **TBD**：转移失败（接收方 cap table 满）时的回滚策略。

---

## 6. 与调度 / 中断的交互

### 6.1 阻塞与唤醒
- `send` 无接收者 → 发送方线程置 Blocked，挂到 endpoint 队列，触发调度。
- `recv` 无消息 → 接收方线程置 Blocked。
- `reply` → 唤醒对应发送方，可能触发抢占（见需求 sched 的 `need_resched` 模型）。

### 6.2 中断线程化
用户态驱动收中断的路径：
```
硬件 IRQ → 内核 IDT → 在独立内核栈快速收 IRQ
        → 内核向"绑定该 IRQ 的 Notification/Endpoint"投递一条消息
        → EOI
        → 用户态驱动 recv 到通知，处理，再 ack
```
- **TBD**：中断通知用同步 Endpoint 还是异步 Notification 原语（seL4 用独立 Notification 对象）？建议引入轻量异步 Notification。

### 6.3 审计事件流 *(原始构想新增)*

原始构想："全链路审计：请求 ID、Agent 身份、输入摘要、权限校验结果、处置动作，**审计记录不可篡改**"。

**架构**：

```
┌──────────────────┐        ┌──────────────────┐        ┌────────────────┐
│  内核事件点      │  推   │  审计服务         │  持久  │  只读存储       │
│  (cap校验 / IPC) │ ─────► │  (独立进程)       │ ─────► │  (append-only) │
└──────────────────┘        │  • 聚合           │        └────────────────┘
                            │  • 签名           │
                            │  • 暴露查询接口   │
                            └──────────────────┘
                                     ▲
                                     │ 查询（只读）
                            ┌────────┴──────────┐
                            │  SecurityGuard    │
                            │  外交工具 / 用户  │
                            └───────────────────┘
```

**内核产出的事件点**（FR9）：

| 事件 | 触发时机 | 包含字段 |
|------|---------|---------|
| `CapVerify{ok,fail}` | 每次 capability 校验 | 调用方 `agent_id` + 目标对象 + 权限位 |
| `CapMint/Destroy/Delegate/Revoke` | capability 生命周期 | 对象类型 + 来源/去向 `agent_id` |
| `IpcSend/Recv/Reply` | IPC 关键路径 | sender/receiver `agent_id` + endpoint 引用 + label（**不含 payload 内容**）|
| `ProcessSpawn/Exit/Fault/Freeze/Thaw` | 进程生命周期 | `agent_id` + 父进程 + 退出码/错误 |
| `AuditLog` 系统事件 | 自身启动 / 配置变更 | 时间戳 + 启动参数 |

**不可篡改保证**：
- 内核事件由内核在事件点直接构造（`agent_id` 已盖章，伪造无意义），通过内核↔审计服务的专用 IPC channel 推送。
- **签名主体是审计服务自身**：审计服务持有自己的密钥，对聚合后的批事件签名落盘；外交工具与一般用户进程均**不可改写**审计存储（独立 capability 隔离，详见 [设计文档 01 §4.1](01-capability-agent-permission-model.md) 的 `AuditLog` 对象）。
- 内核↔审计服务的 channel 自身**不需加密**（内核信道天然可信），仅需完整性；批量提交减少每事件 IPC 次数。
- 外交工具自身产出的业务级审计（通道层，详见 [设计文档 04 §8](04-diplomat-channel-architecture.md)）由外交工具密钥签名，附加在审计服务的批事件中。
- 审计服务对外暴露只读查询接口供 SecurityGuard / 用户查询。
- **TBD**：远期是否需要持久化到 TPM 受保护区域（Phase 6+）。

**性能考量**：
- IPC 热路径不阻塞审计：事件入无锁队列，审计服务后台消费。
- 批量提交（每 N 条或每 T ms）减少 syscall 次数。
- **TBD**：N 与 T 的默认值；需要权衡实时性 vs 吞吐。

---

## 7. 已知风险与技术债

| 风险 | 说明 | 缓解 |
|------|------|------|
| **优先级反转** | 高优先级 Agent 同步等待低优先级服务，中优先级抢占低优先级 → 高优先级被间接阻塞 | 首期不处理；记录为 Phase 6 债务（priority inheritance） |
| **IRQ 风暴** | 故障/恶意用户态驱动不 ack 中断 | 内核侧 IRQ storm 检测 + 自动屏蔽 |
| **拷贝期缺页** | 单拷贝路径中发送方页未驻留 | 拷贝前 prefault 或处理嵌套缺页 |
| **`agent_id` 伪造** | 若信任用户态自报则权限校验失效 | 强制内核盖章（§3.1） |

---

## 8. 性能预算（对齐 NFR2）

| 操作 | 目标延迟 | 说明 |
|------|---------|------|
| 寄存器直传 round-trip | < 1 µs | fast path |
| 单页（4KB）单拷贝 round-trip | < 3 µs | TBD，需实测 |
| 跨地址空间大对象 grant | O(1) | 只传描述符，不拷数据 |

---

## 9. 待决策清单（Phase 4 前必须收敛）

- [ ] 单拷贝的地址空间访问方案（kmap vs 临时映射接收方页表）
- [ ] payload 内联阈值与最大长度
- [ ] 中断通知：复用 Endpoint vs 独立 Notification 原语
- [ ] cap transfer 失败回滚
- [ ] 是否首期就提供 `try_send` / 超时变体
- [ ] 审计事件批量提交的 N / T 默认值（实时性 vs 吞吐）
- [ ] 审计服务的独占 capability 由谁铸造（init？外交工具自铸造？）
- [ ] 审计事件是否需要分类（DEBUG/INFO/WARN/CRITICAL）与持久化分级

---

## 10. 参考资料

- L4 微内核 IPC 单拷贝设计（Jochen Liedtke）
- seL4 Manual：Endpoints / Notifications / Message Registers
- 《Microkernel IPC》single-copy vs double-copy 分析
