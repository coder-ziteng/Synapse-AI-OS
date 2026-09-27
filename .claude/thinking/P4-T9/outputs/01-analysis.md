# P4-T9 01-analysis：现状盘点

## 契约（Doc 02 §5.2/§5.3/§5.5）

### spawn 语义
- 签名：`spawn(elf_ref, args_ptr, caps, death_ep) -> Pid`
- 仅 init 可 spawn（Phase 4 限制，Phase 5+ 持 PROCESS::SPAWN cap 即可）
- 子 CapTable：slot 0 空，slot 1..N = 父 caps 委托（attenuation-only）
- agent_id 唯一性检查（`E_AGENT_ID_CONFLICT`）

### 生命周期与"收尸"
- 状态机：`Created → Running ⇄ Blocked → Exited/Faulted → Zombie → (释放)`
- **资源释放顺序**：页帧 → CapTable → agent_id → pid（Doc 02 §5.3 硬契约）
- Death notification：进程死亡时向 `death_endpoint` 投递 `{pid, exit_code, fault_reason}` 消息，父进程 recv
- 孤儿过继：父死亡 → 子 parent 改 init
- 冻结（freeze/thaw）：正交于生命周期，冻结后不可调度，向其 IPC → `E_FROZEN`

### 配额（Quota）
- 7 维度：`max_pages / max_threads / max_caps / max_endpoints / max_msg_size / max_pending_ipc / max_grants`
- spawn 时从父剩余**划拨**（carve），不可超过；reap 时**归还**（uncarve）
- 超限 `E_QUOTA_EXCEEDED`（不阻塞、不 panic）
- `DEFAULT_QUOTA`：max_pages=4096 (16MB), threads=16, caps=64, endpoints=8, msg_size=4096, pending_ipc=32, grants=8
- `INIT_QUOTA`：max_pages=4GB, threads=64, caps=256, endpoints=64, pending_ipc=256, grants=64（内核启动时设定）

## 现有 API（synapse-proc / synapse-cap / kstate）

### synapse-proc crate（纯逻辑，已 14/14 宿主测）
- `ProcessTable::new(init_agent)` — 自装 init pid=1
- `spawn(parent, SpawnParams{agent, quota, death_endpoint}) -> Result<Pid, CapError>`
- `exit(pid, code) / fault(pid, kind, code) -> Result<DeathSignal, CapError>`
- `release_resources(pid)` — 前置释放页帧 + CapTable 后调用
- `reap(reaper, pid)` — 权限校验 reaper 必须为父或 init
- `set_running / set_blocked / freeze / thaw / ensure_accepts_ipc`
- 全状态转移函数返回 `CapError`（NotFound/Zombie/Frozen/Permission/QuotaExceeded/AgentIdConflict/NoMemory）

### kernel/src/kstate.rs（已落地）
- 5 表静态：`OBJECTS / PROCS / CAP_TABLES / ENDPOINTS / NOTIFICATIONS`
- `kstate_init()` boot 一次钩子
- 锁序：`OBJECTS → PROCS → CAP_TABLES → ENDPOINTS → NOTIFICATIONS`
- `cap_slot(pid) = pid % MAX_PROCS`
- `MAX_ENDPOINTS = MAX_NOTIFICATIONS = 64`

### kernel/src/syscall.rs（已有）
- MSR 配置 + syscall_entry_asm（swapgs + 切内核栈 + push GPR + dispatch）
- 6 syscall 已实现：AbiQuery / ProcessExit / Yield / GetTime / Mmap / Munmap
- dispatch 内 `match other { Syscall::IpcSend/Recv/Reply/TrySend => crate::ipc::* }` — T7 已接线

## 缺口清单（T9 需补）

| 子项 | 当前状态 | T9 需补 |
|---|---|---|
| **per-process kstack** | 仅 boot thread kstack | 每 spawn 进程分配 16KB + 调度时切 TSS.RSP0 |
| **process_spawn syscall (#20)** | 未实现 | ELF 加载 + cap 委托 + kstack + proc.spawn + iretq 首次进入 |
| **process_reap syscall (#22)** | 未实现 | proc.reap + 资源释放触发 |
| **process_freeze / thaw (#23/#24)** | proc 状态机已备 | syscall 接线 |
| **Fault 归因** | #PF/#GP/IllegalSyscall 进 kill_path | 接入 proc.fault(FaultKind::*) |
| **Death notification** | 未实现 | proc.exit/fault → IPC 投递 DeathSignal 到 death_endpoint |
| **Quota enforce** | 未实现 | per-proc QuotaUsage 在 mmap/spawn 路径校验 |
| **多 TSS.RSP0 切换** | 单 kstack | TSS per-process + syscall_entry_asm 选 kstack |

## 风险点

1. **per-process kstack + TSS.RSP0 切换**：当前 `syscall_entry_asm` 硬编码 `gs:[0]` 取 kstack_top。需在 syscall 入口先读 current_pid（per-CPU），再用其索引 per-process kstack。MVP-3 单线程意味着 kstack == process stack，但语义要分开（kstack 是内核栈，process stack 是用户栈）。
2. **ELF 加载到用户 AS**：当前 P4-T5 只跑 init（pid=1）。spawn 需要重复此流程 + 把 init 的 caps 委托给子 + 子进程独立的 AS。
3. **Death notification 投递**：必须持有 ENDPOINTS 锁 + 解析 death_endpoint CapRef → ObjRef → Endpoint。当前 ipc.rs 的 `k_ipc_send` 是为 init 设计的，需要扩展到任意 pid → death_endpoint 投递。
4. **MVP-3 单线程约束**：每进程单线程。"per-thread kstack" 实际是"per-process kstack"，但为了 Phase 5+ 兼容，TSS.RSP0 应放在 per-process 数据结构上而非全局。
5. **Cap slot 0 是 NULL trap**：子进程 slot 0 必须保留空；初始 caps 从 slot 1 开始放。这与 ipc.rs 当前 ep_cap = 1 的约定兼容。

## 结论

T9 工作量 **2 窗口**估计合理。synapse-proc 已是可直接消费的 API，主要工程量在：
- 内核集成层（kstack/TSS/spawn/调度切换）≈ 1 窗口
- syscall + 真机 smoke（spawn/reap/freeze + fault 归因 + death notification + quota enforce）≈ 1 窗口

可拆为 5 个子模块独立推进：
- T9a per-process kstack + TSS.RSP0 切换（解 boot fence）
- T9b process_spawn syscall + 子进程 ELF 加载
- T9c crash handling + death notification
- T9d process_reap syscall
- T9e quota enforce（mmap 路径 + spawn 校验已由 proc crate 覆盖，仅需 mmap 路径）

每个子模块编译 + 真机 smoke 独立闭环后才推进下一个。