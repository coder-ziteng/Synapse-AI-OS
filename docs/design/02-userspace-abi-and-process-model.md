# 设计文档 02：用户态 ABI 与进程模型

> 状态：**PROPOSED 完成**（待确认升级为 DECIDED）
> 关联需求：FR5 用户态加载、FR10 进程冻结与监督原语
> 关联里程碑：Phase 4（用户态与 IPC）；监督树由系统服务层 S3 落地
> 最后更新：2026-09-25

---

## 0. 文档目的

定义 Synapse 用户态程序的**编译目标、加载方式、syscall 约定、进程/线程模型、地址空间布局**。这是"能加载并运行第一个用户态进程"（FR5）的前置设计，Phase 4 开工前必须收敛。

### PROPOSED 决策汇总

| 决策项 | PROPOSED 方案 | 章节 |
|--------|--------------|------|
| initramfs 格式 | cpio (newc) | §6.1 |
| ELF 加载基址 | 0x400000（Linux 传统值）| §3.1 |
| PIE 支持 | 首期不支持（固定加载）| §3.1 |
| 地址空间 | 用户空间 0x0~0x7FFF_FFFF_FFFF，内核映射 0xFFFF_8000_... | §3.1 |
| spawn 权限 | 仅 init 进程可 spawn | §5.2 |
| death notification | death endpoint + signal 消息 + reap syscall | §5.3 |
| syscall 表 | 19 个 syscall（IPC 4 + Notification 2 + Capability 3 + Process 6 + Time 1 + Memory 2 + ABI 1）| §4.2 |
| 错误码 | 自定义负值（-1~-127），15 个核心错误码 | §4.3 |
| ABI 版本策略 | abi_query syscall + 消息头 version + rights 只追加 | §4.5 |
| 每进程配额 | Quota struct（pages/threads/caps/endpoints/msg/pending/grants）| §5.5 |

> ✅ Phase 4 核心设计决策已完成，剩余 TBD 为信号模型/时钟校准相关。

---

## 1. 用户态编译目标（Target）

### 1.1 自定义 target json
裸机用户态不能用 `x86_64-unknown-linux-gnu`（那假设了 Linux ABI）。需要自定义 target，例如 `x86_64-synapse-user.json`：

```jsonc
{
  "llvm-target": "x86_64-unknown-none",
  "data-layout": "…",                 // TBD：对齐内核 target
  "arch": "x86_64",
  "target-endian": "little",
  "pointer-width": "64",
  "os": "none",
  "panic-strategy": "abort",          // 用户态 panic 直接 abort
  "disable-redzone": true,            // 裸机惯例
  "features": "…",                    // TBD：与内核一致，避免 ABI 漂移
  "linker-flavor": "ld.lld",
  "relocation-model": "static"        // 首期静态，见 §3
}
```

### 1.2 待决策
- [ ] 用户态是否允许 `std`？（首期建议 `no_std` + 极小 runtime，避免引入 libc）
- [ ] 是否提供极简 libc shim（影响 Phase 5 llama.cpp 可行性，见需求"AI 原生收敛"）

---

## 2. ELF 加载器

### 2.1 范围
- 首期**只支持静态 ELF**（`ET_EXEC` 或 `ET_DYN` PIE），**砍掉动态链接器**（ld.so）。
- 解析 `PT_LOAD` 段，按 `p_vaddr` 映射到用户地址空间，设置权限（R/W/X）。
- 入口点 `e_entry` 作为初始 RIP。

### 2.2 待决策
- [ ] 是否支持 PIE + 重定位（`R_X86_64_RELATIVE`）？还是固定加载地址即可？
- [ ] ELF 从哪里来（initramfs，见 §6）？
- [ ] 加载失败 / 段越界的错误处理路径

---

## 3. 地址空间布局

### 3.1 用户地址空间（PROPOSED 具体数值）

```
0x0000_0000_0000_0000  ┌───────────────────────────┐
                       │  NULL page guard (4KB)     │  ★ 捕获空指针解引用
0x0000_0000_0000_1000  ├───────────────────────────┤
                       │  (未映射保留区)              │
0x0000_0000_0040_0000  ├───────────────────────────┤  ← ★ PROPOSED: ELF 默认加载基址
                       │  .text / .rodata (RX)      │     对齐 Linux 传统值
                       ├───────────────────────────┤
                       │  .data / .bss (RW)         │
                       ├───────────────────────────┤
                       │  heap (向上增长)            │  ★ PROPOSED: heap 向上（对齐 Linux brk）
                       │        ↑                   │
                       │        ...                 │
                       │        ↓                   │
                       │  stack (向下增长)            │  ★ PROPOSED: stack 向下（x86_64 标准）
0x0000_7FFF_FFFF_E000  ├───────────────────────────┤  ← stack 起始（4KB 对齐）
                       │  (未映射 gap，8MB)          │  ★ 防 stack-heap 碰撞
0x0000_8000_0000_0000  ├───────────────────────────┤  ← ★ 内核映射区起点 (canonical hole 上沿)
                       │  内核映射区 (NX +           │
                       │   Ring3 不可访问)           │
0xFFFF_FFFF_FFFF_FFFF  └───────────────────────────┘
```

> **UPDATE（P4-T2, 2026-09-27，DECIDED）：ELF 加载基址改为 `0x4000_0000`（1GB）**。
> 原 PROPOSED `0x400000` 与现状冲突：内核经 boot.S 以 2MB 大页**恒等映射** 0-4GB，
> 镜像占 PA `[0x200000, 0x4cb000)`——用户 VA 0x400000 与内核自身代码/数据的 VA
> 完全重叠（同一地址空间内同一 VA 不能两者兼是）。1GB 基址落在 PDPT[0] entry 1
> 所辖 VA 区，其 PA 1-2GB 无物理内存（RAM ~128MB），每地址空间为该 entry 挂独立
> 清零 PD 即可与共享的内核 0-1GB 恒等映射（entry 0）零重叠、零大页拆分。
> 上方图中 `0x40_0000` 行以此更新为准；内核高半迁移（图中 0x8000_0000_0000 区）
> 维持远期方向不变。

**PROPOSED 决策**：
- ~~**ELF 加载基址 = `0x400000`**：Linux x86_64 传统默认值，工具链兼容性最佳；~~（已被上方 UPDATE 取代：基址 = `0x4000_0000`）
- **首期不支持 PIE**（Position-Independent Executable）：固定加载地址，砍掉重定位解析开销；Phase 5+ 视 ASLR 需求再引入；
- **heap 向上增长**（对齐 Linux brk 语义）；**stack 向下增长**（x86_64 标准）；
- **stack 起始 = `0x7FFF_FFFF_E000`**，与内核映射区保留 8MB gap（防 stack-heap 碰撞缓冲）。

### 3.2 内核映射隔离

- 内核高半映射区在用户页表中**标记为不可访问（present=0 或 NX + supervisor）**。
- 首期**不做 KPTI**（Meltdown 缓解），但在威胁模型中记为已知风险。

> ⚠️ **PCID 可选优化**（对齐 [需求评审 §3.3](../requirements-review-and-supplement.md)）：
> PCID（Process Context Identifier）可避免进程切换时全量刷 TLB，但**不应成为 Phase 4 的硬前置**。
> 建议：先实现正确的无 PCID 路径（每次切换刷 TLB），再以可选优化加入 PCID，并分别测量性能。
> 不能因为 PCID 延迟而阻塞第一个用户态进程。PCID 依赖 CR4/INVPCID、地址空间切换策略和 CPU 能力检测，复杂度不低。

### 3.3 用户内存区域管理（VMA-like）
按需分页需要一张"哪些区间合法、权限如何"的表：

```rust
pub struct UserMemoryRegion {
    pub start: VirtAddr,   // 页对齐
    pub end: VirtAddr,
    pub flags: RegionFlags, // READ | WRITE | EXEC | GROWABLE(stack/heap)
    pub kind: RegionKind,   // Code | Data | Stack | Heap | Mapped(device)
}
```
- 缺页处理：命中合法 region → 分配页帧并映射；未命中 → SIGSEGV 等价物（杀进程）。

---

## 4. 系统调用 ABI

### 4.1 调用约定（TBD，候选 x86_64 System V 风格）
| 寄存器 | 用途 |
|--------|------|
| `rax` | syscall 号 |
| `rdi, rsi, rdx, r10, r8, r9` | 参数 1~6 |
| `rax`（返回） | 返回值 / 负错误码 |

- 指令：`syscall`（需在 Phase 4 配置 `MSR_LSTAR` / `MSR_STAR` / `MSR_FMASK`）。
- 内核栈切换：通过 `MSR_GS_BASE` + TSS.RSP0 定位当前线程内核栈。

### 4.2 syscall 号分配表（PROPOSED）

| 号 | 名称 | 参数 | 说明 |
|----|------|------|------|
| **IPC** | | | |
| 0 | `ipc_send` | `ep: CapRef, msg: *const u8, len: usize, caps: *const CapRef, n_caps: usize` | 同步发送（阻塞直到 reply），见 Doc 03 |
| 1 | `ipc_recv` | `ep: CapRef, buf: *mut u8, cap: *mut CapRef` | 同步接收（阻塞直到消息）|
| 2 | `ipc_reply` | `ep: CapRef, msg: *const u8, len: usize` | 回复原发送方 |
| 3 | `ipc_try_send` | `ep: CapRef, msg: *const u8, len: usize` | 非阻塞发送（Doc 03 §9 PROPOSED）|
| **Notification** | | | |
| 4 | `notification_signal` | `notif: CapRef, bits: u32` | 发送 Notification 信号（位图 OR）|
| 5 | `notification_wait` | `notif: CapRef, mask: u32` | 等待 Notification（位图 AND），返回触发位 |
| **Capability** | | | |
| 10 | `cap_invoke` | `cap: CapRef, op: u32, args: *const u8` | 通用能力调用（对象特定操作）|
| 11 | `cap_delegate` | `parent: CapRef, rights: Rights, child: *mut CapRef` | 委托子 capability（Doc 01 §3.3）|
| 12 | `cap_revoke` | `cap: CapRef` | 撤销 capability（derivation tree 遍历，Doc 01 §3.4）|
| **Process** | | | |
| 20 | `process_spawn` | `elf: CapRef, args: *const u8, caps: *const CapRef, n_caps: usize, death_ep: CapRef` | 创建子进程（Doc 02 §5.2）|
| 21 | `process_exit` | `code: i32` | 当前进程退出 |
| 22 | `process_reap` | `pid: Pid` | 回收僵尸进程（Doc 02 §5.3）|
| 23 | `process_freeze` | `pid: Pid` | 冻结进程（Doc 02 §5.4，需 PROCESS_ADMIN cap）|
| 24 | `process_thaw` | `pid: Pid` | 解冻进程 |
| 25 | `yield` | | 主动让出 CPU |
| **Time** | | | |
| 30 | `gettime` | `clock_id: u32, ts: *mut Timespec` | 获取时间（单调时钟 / 墙钟）|
| **Memory** | | | |
| 40 | `mmap` | `addr: *mut u8, len: usize, prot: u32, flags: u32` | 映射内存区域 |
| 41 | `munmap` | `addr: *mut u8, len: usize` | 解除映射 |

**总计**：首期 18 个 syscall（IPC 4 + Notification 2 + Capability 3 + Process 6 + Time 1 + Memory 2）；加 §4.5 的 `abi_query`（#18）共 **19 个**。

### 4.3 错误码约定（PROPOSED）

**PROPOSED → 自定义错误码（负值返回）**：
- 不复用 Linux errno（Synapse 是独立内核，不依赖 Linux 语义）；
- 错误码为负值（`rax < 0` 表示错误），范围 `-1` ~ `-127`；
- 返回值 ≥ 0 表示成功（具体语义由 syscall 决定）。

| 错误码 | 名称 | 说明 |
|--------|------|------|
| -1 | `E_INVALID_CAP` | capability 引用无效（cptr 越界 / 权限不足 / 对象已撤销）|
| -2 | `E_INVALID_ADDR` | 用户态地址非法（未映射 / 权限不足 / 对齐错误）|
| -3 | `E_NO_MEMORY` | 内核内存不足（页帧 / CapTable 槽位）|
| -4 | `E_WOULD_BLOCK` | 非阻塞操作无法立即完成（`ipc_try_send` 队列满）|
| -5 | `E_NOT_FOUND` | 对象不存在（pid / agent_id / endpoint）|
| -6 | `E_AGENT_ID_CONFLICT` | agent_id 重复（spawn 时，Doc 02 §5.2）|
| -7 | `E_PERMISSION` | 权限不足（rights 位不覆盖本次操作）|
| -8 | `E_FROZEN` | 目标进程已冻结（freeze / thaw / IPC 到冻结进程）|
| -9 | `E_ZOMBIE` | 目标进程已退出（需先 reap）|
| -10 | `E_NOT_IMPLEMENTED` | syscall 未实现（预留）|

**设计原则**：
- 错误码数量控制在 127 以内（7-bit，便于序列化）；
- 每个错误码对应明确的失败场景，便于用户态处理；
- 扩展新错误码时，追加到表尾，不修改已有编号。

### 4.4 待决策

- [ ] syscall 是否可重启（被信号/抢占打断后）—— 需信号模型确认后决策

### 4.5 ABI 版本策略 *(评审补充，对齐 [需求评审 §2.5](../requirements-review-and-supplement.md))*

**问题**：syscall 号表、消息头、Capability 权限位均为草案。若无版本策略，用户态程序一旦编译即与内核强绑定，无法独立演进。

**PROPOSED → 三层版本控制**：

| 层级 | 版本载体 | 兼容规则 |
|------|---------|---------|
| **syscall ABI** | `abi_query` syscall 返回 `{major, minor}` | major 不兼容变更（删除/重解释 syscall）；minor 向后兼容新增 |
| **IPC 消息头** | `IpcHeader.version: u8` + `header_len: u16` | 接收方按 `header_len` 跳过未知尾部字段；version 不匹配 → `E_ABI_MISMATCH` |
| **Capability rights** | 权限位只允许追加（bit 7~31 保留） | 删除或重解释权限位 → ABI major 版本提升 |

**abi_query syscall**（新增，编号 18）：

```rust
/// 查询内核支持的 ABI 版本
/// 返回：(major << 16) | minor
/// 用户态启动时调用，版本不匹配则拒绝运行
SYS_ABI_QUERY = 18,
```

**结构体布局约束**：

- 所有跨边界结构体必须 `#[repr(C)]`，明确字节序（little-endian）、对齐、大小；
- 禁止直接把 Rust 私有布局当 ABI（如 `#[repr(Rust)]` 的结构体不得跨越 syscall 边界）；
- 新增字段只能追加到结构体尾部，且 `header_len` / `size` 字段必须同步更新；
- 错误码采用固定负数枚举（§4.3），不复用宿主 OS 的未承诺扩展。

**新增错误码**：

| 错误码 | 名称 | 说明 |
|--------|------|------|
| -11 | `E_ABI_MISMATCH` | 用户态与内核 ABI 版本不兼容 |
| -12 | `E_OBJECT_RETIRED` | 对象已撤销/退休（generation 不匹配，对齐 Doc 01 §4.2）|
| -13 | `E_QUOTA_EXCEEDED` | 进程资源配额耗尽（对齐 §5.5）|
| -14 | `E_PEER_DIED` | IPC 对端进程已退出 |
| -15 | `E_TIMEOUT` | 操作超时（预留，首期仅 `try_send` 非阻塞）|

---

## 5. 进程 / 线程模型

### 5.1 定义

- **进程（Process / AddressSpace）** = 一套用户页表 + CapTable + `agent_id`。
- **线程（Thread）** = 调度实体，隶属于某进程，共享地址空间。
- 首期：**单进程内可多线程**，进程间强隔离。

> ⚠️ **MVP-3 约束**（对齐 [需求评审 §3.4](../requirements-review-and-supplement.md)）：
> **首个用户态进程（init）建议先限制为单线程**。待地址空间、syscall、fault、退出回收稳定后，再开放同进程多线程。
> 理由：进程、线程、地址空间和 death notification 同时引入会产生过多交叉状态，增加调试难度。
> 多线程支持在 MVP-4 之后开放，不阻塞 Phase 4 退出标准。

### 5.2 spawn 语义
- 无 `fork`（不复制地址空间）。只有 `spawn(elf, args, caps_to_grant)`：从 ELF 新建进程。
- **PROPOSED → 初始能力授予方式**：
  - `spawn` syscall 签名：`spawn(elf_ref: CapRef, args_ptr: *const u8, caps: &[CapRef]) -> Result<Pid>`；
  - `caps` 参数是父进程 CapTable 中的 CapRef 数组，内核将这些 capability **委托（delegate）** 给子进程（attenuation-only，子进程获得的权限 ≤ 父进程持有）；
  - 子进程 CapTable 初始状态：slot 0 = 空（NULL trap），slot 1..N = 父进程授予的初始 caps；
  - **agent_id 分配**：父进程在 `spawn` 时指定子进程的 `agent_id`（字符串），内核检查唯一性（由 init 进程维护全局 agent_id 命名空间，对齐 Doc 01 §7 PROPOSED）；若重复 → 返回 `E_AGENT_ID_CONFLICT`；
  - **首期限制**：仅 init 进程可 spawn 其他进程（防止不受控进程树）；后续 Phase 5+ 放开至持有 `PROCESS::SPAWN` capability 的进程。

### 5.3 进程生命周期与"收尸"
```
Created → Running → (Blocked) → Exited/Faulted → Reaped
```
- 进程崩溃（段错误 / panic / 非法 syscall）→ 内核标记 Faulted。
- **PROPOSED → death notification 机制**：
  - 每个进程在 spawn 时注册一个 `death_endpoint: CapRef`（指向父进程持有的 Endpoint）；
  - 进程进入 Exited / Faulted 状态时，内核向该 endpoint 投递一条 **death signal 消息**：`{ pid: Pid, exit_code: i32, fault_reason: Option<FaultKind> }`；
  - 父进程通过常规 `recv(death_endpoint)` 获取子进程终止通知（复用 IPC 机制，无需新原语）；
  - 父进程收到通知后负责回收子进程资源（页帧、CapTable、agent_id 命名空间条目）；
- **PROPOSED → 僵尸进程回收时机**：
  - 子进程 Exited 后**不立即释放资源**，进入 Zombie 状态（保留 pid + 退出码，释放页帧和 CapTable）；
  - 父进程 `recv` death notification 后，通过 `reap(pid)` syscall 彻底释放 zombie（释放 pid + agent_id）；
  - **孤儿进程处理**：若父进程先于子进程退出 → 子进程被"过继"给 init 进程（init 作为默认收尸人）；
- **资源释放顺序**：页帧 → CapTable → agent_id → pid（从用户态资源到内核资源，逐步释放）。

### 5.4 监督树与重启策略 *(原始构想新增)*

原始构想：L3 治理层"自愈机制"——一个 Agent 崩溃不影响整体。

**模型**：用户态监督树（系统服务层 S3 落地），**内核只提供原语**：

| 内核原语 | 用途 |
|---------|------|
| `death_notification`（§5.3） | 子进程终止事件通知父进程 |
| `process_freeze/thaw` syscall（§4.2）| 行为围栏冻结 / 解冻进程 |
| 资源核算（FR8） | 监督器判定"该 Agent 是否滥用资源" |
| 审计事件（FR9） | 监督决策的输入数据 |

**监督器（用户态进程）**的职责：

1. 启动子进程时声明 `RestartPolicy`：
   ```rust
   pub struct RestartPolicy {
       pub max_restarts: u32,           // 在 window_secs 内最多重启次数
       pub window_secs: u32,
       pub backoff: BackoffStrategy,    // Linear | Exponential
       pub on_exhausted: ExhaustedAction, // Degrade | Freeze | AlertUser
       pub preserve_state: bool,        // 是否保留状态快照（自进化先决条件）
   }
   ```
2. 收到 death notification → 按策略决定：重启 / 降级 / 冻结 / 通知用户。
3. 重启次数超限 → `ExhaustedAction` 触发，避免"反复崩溃风暴"。

**冷启动默认策略**（`init` 进程对每个必需服务声明）：
- 外交工具：`max_restarts=3 / 10s`，exhausted → 通知用户（核心服务）；
- 用户态 Agent：`max_restarts=10 / 60s`，exhausted → 冻结 + 通知主 Agent（业务降级）。

**重要约束**：
- 监督树是**用户态**模型，内核不强制（避免内核被绑死策略）；但内核保证原语可用。
- 自进化系统（系统服务层 S4）复用此监督树 + `preserve_state` 机制作为"策略回滚"的载体。
- **TBD**：监督器自身崩溃如何处理？候选：每个监督器被更高层监督器看管（递归）；或 kernel watchdog 监督 init。

### 5.5 每进程资源配额 (Per-Process Quota) *(评审补充，对齐 [需求评审 §2.4](../requirements-review-and-supplement.md))*

**问题**：Capability 模型解决"能不能做"，但未解决"能做多少"。恶意或失控 Agent 可耗尽页帧、cap 槽位、Endpoint 队列、线程数等资源，导致内核或其他进程不可用。

**PROPOSED → 最小配额模型（Phase 4 引入）**：

```rust
/// 每进程资源配额
pub struct Quota {
    pub max_pages: u32,           // 物理页帧上限（含内核映射）
    pub max_threads: u16,         // 线程数上限
    pub max_caps: u16,            // CapTable 槽位上限（≤ 256）
    pub max_endpoints: u16,       // 持有的 Endpoint 对象数上限
    pub max_msg_size: u32,        // 单条 IPC 消息最大字节数（≤ 4KB）
    pub max_pending_ipc: u16,     // 未完成 IPC 请求数上限
    pub max_grants: u16,          // 共享内存 grant 数上限
}

/// 默认配额（init 进程授予子进程时的初始值）
pub const DEFAULT_QUOTA: Quota = Quota {
    max_pages: 4096,              // 16 MB
    max_threads: 16,
    max_caps: 64,                 // 保守值，远低于 256 上限
    max_endpoints: 8,
    max_msg_size: 4096,           // 4 KB
    max_pending_ipc: 32,
    max_grants: 8,
};
```

**配额管理规则**：

| 规则 | 说明 |
|------|------|
| **授予** | `spawn` 时父进程从自身配额中划拨子进程配额（不可超过父进程剩余）|
| **超限** | 返回 `E_QUOTA_EXCEEDED`（-13），**不阻塞、不 panic** |
| **调整** | 持有 `Process::ADMIN` capability 的进程可调整子进程配额（如监督树降级）|
| **init 预算** | init 进程拥有全局初始预算（内核启动时设定），不可被其他进程修改 |
| **回收** | 进程退出时配额归还父进程（或全局池，若父进程已退出）|

**与 FR8 资源核算的关系**：

- FR8 提供**计数器**（per-thread CPU 时间、per-process 内存页数）；
- 本节提供**配额上限**（enforcement）；
- 计数器 ≤ 配额 → 允许；计数器 > 配额 → 拒绝新分配 + 审计事件。

**首期裁剪**：

- 不做 CPU 时间配额（依赖 PIT 校准，推迟至 Phase 3 完成后）；
- 不做网络请求配额（外交工具用户态策略，非内核职责）；
- 配额检查在分配路径上，O(1) 比较，不影响 NFR2。

---

## 6. initramfs 与启动流程

### 6.1 initramfs 格式
- **PROPOSED → cpio (newc)**：
  - **理由**：
    - **Linux 标准格式**：内核原生支持解析（参考 Linux initramfs），成熟稳定；
    - **极简**：无专利、跨平台、纯文本 header + 数据流；
    - **Rust 生态支持**：`cpio` crate 可直接使用；
    - **对齐 seL4 / Redox**：两者均用 cpio 或类似扁平格式。
  - **候选格式比较**：
    | 格式 | 优势 | 劣势 | 决策 |
    |------|------|------|------|
    | **cpio (newc)** | 标准、极简、Rust crate 可用 | 无压缩（但 initramfs 本身不大）| ✅ PROPOSED |
    | ustar tar | 广泛支持 | 512-byte 块对齐浪费空间、略复杂 | ❌ |
    | 自定义扁平格式 | 完全可控 | 无生态、重复造轮子、与 NFR1 极简原则冲突 | ❌ |
  - **加载方式**：由 bootloader 或内核早期加载到内存，作为一个 MemoryRegion 暴露给内核；内核解析 cpio header，提取 `/init`（外交工具根进程）→ spawn → 移交控制权。

### 6.2 启动链
```
bootloader → kernel_main → mm/sched/ipc init
          → 解析 initramfs → spawn init(Root Agent)
          → init 通过 IPC 拉起外交工具 / 其他 Agent
```

---

## 7. 时钟与时间服务

- `gettime` syscall：暴露单调时钟（TSC 校准后）+ 墙钟（RTC，若可用）。
- **TBD**：TSC 校准方式（PIT / APIC timer 反推）、时间精度、是否暴露给用户态直接读 TSC（`rdtsc` 权限）。

---

## 8. 待决策清单（Phase 4 前必须收敛）

- [x] ~~用户态 target json 完整字段~~ → **DECIDED（P4-T1, 2026-09-27）：`x86_64-synapse-user.json`，llvm-target / data-layout / features 与内核 `x86_64-bootloader.json` 逐字段一致（避免 ABI 漂移）；panic=abort · disable-redzone · relocation-model=static · ld.lld + `user/hello/linker.ld`（基址 0x4000_0000 = 1GB——原 0x400000 因与内核恒等映射同 VA 冲突废弃，见 §3.1 UPDATE；text RX / data RW 双 PT_LOAD，W^X）。构建入口 `xtask user`：`--manifest-path user/hello` + `-Zbuild-std=core,alloc`，产物过 ELF 头断言（ET_EXEC / entry∈基址区 / 无 PT_DYNAMIC）。实测坑：lld 的 `--script=` 相对包根解析；compiler_builtins 在 os=none 上不提供 memset（`user/src/runtime.rs` cfg 门控补齐，与内核 main.rs 同款）**
- [ ] 是否提供 `std` / libc shim —— 需用户态应用需求明确后决策（首期建议 `no_std`）
- [x] ~~静态 ELF 加载基址与是否支持 PIE~~ → **PROPOSED：基址 = 0x400000，首期不支持 PIE**，理由见 §3.1
- [ ] syscall 号表最终版 + 错误码约定 —— 需实现阶段逐步固化
- [x] ~~initramfs 打包格式~~ → **PROPOSED：cpio (newc)**，理由见 §6.1
- [x] ~~spawn 时的初始能力授予方式~~ → **PROPOSED：父进程通过 CapRef 数组委托，首期仅 init 可 spawn**，理由见 §5.2
- [x] ~~进程崩溃的 death notification 机制~~ → **PROPOSED：death endpoint + signal 消息 + reap syscall**，理由见 §5.3
- [ ] gettime 的时钟源与校准 —— 需内核 Phase 2 定时器实现后决策
- [ ] 监督器自身崩溃的兜底（kernel watchdog 还是递归监督？）—— 需安全评审
- [ ] `RestartPolicy` 是否在 spawn syscall 内核入参暴露给进程，还是完全由用户态监督器解析配置 —— 需 API 设计评审
- [ ] 进程冻结时其持有的 capability 是否同步冻结（防止持有者调用已冻结对象）—— 需 capability 语义评审

---

## 9. 参考资料

- Philipp Oppermann《Writing an OS in Rust》用户态章节
- x86_64 `syscall`/`sysret` ABI（AMD64 Architecture Programmer's Manual Vol.2）
- ELF Specification（System V ABI）
- seL4 / Redox 进程模型
