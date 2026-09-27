# P4-T9 02-architecture：内核集成层架构

## 设计原则

1. **复用 synapse-proc 纯逻辑**：状态机 / 配额划拨 / agent_id 注册全交给 crate；本层只做**资源实际释放**和**跨表侧效对接**
2. **per-process kstack + TSS.RSP0**：MVP-3 单线程意味着"per-thread kstack"实质是"per-process kstack"；TSS.RSP0 切换点 = 每次准备 iretq 进用户态前（spawn child / return from kernel）
3. **同步 spawn（co-routine 语义）**：MVP-3 单线程无调度器，spawn 即"调用方 init 让出 → 子进程跑 → 子退出/系统调用 → init 收回控制权"。Phase 5+ 加调度器再切异步
4. **Death notification = IPC**：proc.exit/fault 返 DeathSignal → 内核走 ENDPOINTS 锁 → `Endpoint::try_send` 投递；用户态父进程正常 recv 即可
5. **不破坏现有 ABI**：T9 只新增 syscall (#20 spawn / #22 reap / #23 freeze / #24 thaw)；其余 #21 process_exit 在 T6 已实装

## 模块划分

### kernel/src/proc_ext.rs（新建）
**职责**：synapse-proc Pcb 的内核侧扩展（synapse-proc 是 `#![deny(unsafe_code)]` 纯逻辑，不能直接放裸指针）。

```rust
/// per-process 内核侧扩展（Pcb 的 sibling，不污染纯逻辑 crate）
pub struct ProcExt {
    pub kstack_bottom: u64,   // 16KB 栈底（含 guard page）
    pub kstack_top: u64,      // 栈顶 = kstack_bottom + 16KB - 8
    pub as_user: Option<AddressSpace>,  // 进程独立 AS（None = init 共享内核 AS？）
    pub death_signal: Option<DeathSignal>, // exit/fault 时的死亡信号（已投递到 death_endpoint）
}

static PROC_EXT: SpinLock<[Option<ProcExt>; MAX_PROCS]>
    = SpinLock::new([const { None }; MAX_PROCS]);
```

注：`death_signal` 暂存是因为 spawn 是同步的——子进程死亡信号需保留到 init 主动 reap；reap 后清空。

### kernel/src/kstack.rs（新建）
**职责**：16KB per-process 内核栈分配 / 释放 / guard page 防溢出。

```rust
const KSTACK_SIZE: usize = 16 * 1024;  // 16KB（含 1 页 guard）

/// 分配 per-process kstack（1 帧 guard + 4 帧栈 = 20KB；guard 标记 PROT_NONE）
/// 失败返 None（page_frame 耗尽）
pub fn kstack_alloc() -> Option<(u64 /*bottom*/, u64 /*top*/)>;

/// 释放（reap 时调用）
pub fn kstack_free(bottom: u64);
```

guard page 用 VMA region `RegionKind::Mapped` + `RegionFlags::empty()` 实现；首期不在 VMA 表里登记，只在 frame 里标 PROT_NONE 即可（用户态不映射，但 kernel 内部需防踩）。

实际简化：MVP 首期不做 guard page，依赖 16KB 栈深度本身 + panic handler 栈回溯覆盖。Phase 5+ 加 guard。

### kernel/src/spawn.rs（新建）
**职责**：process_spawn 内核实现 = ELF 加载 + cap 委托 + kstack 分配 + proc.spawn + iretq。

```rust
/// spawn 子进程（syscall #20 dispatcher 调用）。
/// 
/// 输入：parent_pid, elf_ref CapRef, args_ptr, caps_ptr, n_caps, death_ep CapRef
/// 输出：新 Pid（成功）/ 负错误码
///
/// 流程：
/// 1. 校验 parent_pid == INIT_PID (MVP 限制)
/// 2. 解析 elf_ref → ObjRef → 校验 ObjKind=Code + rights (READ | EXEC)
/// 3. 读取 ELF 数据（kernel cap-walk 共享页 or 暂存 frame）
/// 4. proc.spawn(parent, SpawnParams{agent, quota=DEFAULT, death_ep})
///    → 获得新 pid
/// 5. kstack_alloc() → PROC_EXT[pid].{kstack_bottom, kstack_top}
/// 6. 解析 caps 数组 → synapse_cap::transfer_caps(parent → child, items)
///    失败则 proc.release_resources + kstack_free 回滚
/// 7. elfload::load_into(child_as, elf_data) → 装入子 AS
/// 8. elfload::setup_user_stack(child_as, args) → 用户栈
/// 9. 构造子进程初始 iretq 帧（ss/rsp/rflags/rip/cs）
/// 10. 切换 AS (CR3) + TSS.RSP0 = child kstack_top
/// 11. iretq — 子进程开始执行
/// 12. 子进程通过 process_exit / fault 返回 → 控制流回到这里
/// 13. 子死亡信号投递到 death_endpoint
/// 14. 返回子 Pid 给父进程
pub fn k_process_spawn(frame: &SyscallFrame) -> i64;
```

### kernel/src/reap.rs（新建，可合并到 spawn.rs）
**职责**：process_reap + 资源释放触发。

```rust
/// reap 子进程（syscall #22 dispatcher 调用）。
///
/// 流程：
/// 1. 校验 caller 是目标 pid 的 parent（或 init 收尸孤儿）
/// 2. 校验目标 state == Zombie
/// 3. 释放：cap_table → as_user (unmap all + free frames) → kstack_free
/// 4. proc.reap(caller, target_pid) → agent_id 释放 + pid 槽释放 + 配额归还
pub fn k_process_reap(frame: &SyscallFrame) -> i64;
```

### kernel/src/freeze.rs（极小，可合并）
**职责**：process_freeze / thaw syscall 接线。

```rust
pub fn k_process_freeze(frame: &SyscallFrame) -> i64;  // #23
pub fn k_process_thaw(frame: &SyscallFrame) -> i64;    // #24
```

### kernel/src/fault.rs（新建）
**职责**：#PF / #GP / IllegalSyscall 归因到 FaultKind。

```rust
/// 把异常归因到 FaultKind（idt::page_fault_inner / idt::gp_inner / 
/// dispatch::unimplemented 末尾调用）
pub fn fault_process(pid: Pid, kind: FaultKind, code: i32) -> DeathSignal;
```

逻辑：`proc.fault(pid, kind, code) → DeathSignal` → 通过 spawn.rs 的死亡投递路径走 ENDPOINTS。

### kernel/src/smoke.rs（扩）
T9 smoke：init (hello) spawn child (init_test)，child 非法访问 → fault → death notification → init recv → reap。

### syscall.rs 改动
- dispatch `IpcSend/Recv/Reply/TrySend` 旁追加 4 行：
  - `Syscall::ProcessSpawn { .. } => crate::spawn::k_process_spawn(frame)`
  - `Syscall::ProcessReap { .. } => crate::spawn::k_process_reap(frame)`
  - `Syscall::ProcessFreeze { .. } => crate::freeze::k_process_freeze(frame)`
  - `Syscall::ProcessThaw { .. } => crate::freeze::k_process_thaw(frame)`
- 同时需要 `synapse_abi::decode` 支持这 4 个新 syscall 的 ABI 帧解码（user crate 也需生成新 invoke 包装）

### abi/src/lib.rs + user/src/wrappers.rs 改动
- 加 `Syscall::ProcessSpawn/Reap/Freeze/Thaw` 枚举 + decode 帧布局
- user crate 加 `pub unsafe fn invoke_spawn/reap/freeze/thaw`
- ABI_MINOR: 3 → 4（T9 增量）

## 锁顺序（沿用 kstate.rs 约定 + 扩展）

```
OBJECTS → PROCS → CAP_TABLES → ENDPOINTS → NOTIFICATIONS
                  ↓
              PROC_EXT（新增，排在 PROCS 之后、CAP_TABLES 之前或之后均可——不会和 cap transfer 路径冲突）
                  ↓
              per-process kstack/AS（不进全局表，spawn/reap 单进程内串行）
```

## MVP-3 单线程限制下的关键约束

1. **同步 spawn = init 让出 → 子跑 → 子返**：子进程 iretq 后独占 CPU，直到 process_exit / fault。无需 runqueue。
2. **TSS.RSP0 单值**：单 CPU 单进程串行，TSS.RSP0 永远是"当前正要执行的用户进程"的 kstack_top。切换点：
   - spawn 后 iretq 前：设 TSS.RSP0 = child.kstack_top
   - 子 iretq 回 kernel（exit/fault）：kernel 处理完 → iretq 回 init → 设 TSS.RSP0 = init.kstack_top
3. **per-process kstack 不能同时两线程用**：MVP-3 单线程，无嵌套 syscall 风险。
4. **mmap quota enforce**：当前 mmap 是 init 单进程，quota 总是 init.usage.pages ≤ INIT_QUOTA.max_pages。T9 加 quota 后，mmap 入口检查 current_pid.usage.pages + len/4KB ≤ current_pid.quota.max_pages。

## 文件清单

| 文件 | 动作 | 行数估 |
|---|---|---|
| kernel/src/proc_ext.rs | 新建 | ~80 |
| kernel/src/kstack.rs | 新建 | ~60 |
| kernel/src/spawn.rs | 新建（spawn + reap 合并） | ~350 |
| kernel/src/freeze.rs | 新建 | ~50 |
| kernel/src/fault.rs | 新建（极小） | ~30 |
| kernel/src/smoke.rs | 扩 | ~50 |
| kernel/src/main.rs | 加 4 个 `pub mod` | 5 |
| kernel/src/syscall.rs | dispatch 加 4 行 | 5 |
| abi/src/lib.rs | Syscall 加 4 变体 + 帧解码 | ~80 |
| abi/src/lib.rs | ABI_MINOR 3→4 | 2 |
| user/src/wrappers.rs | invoke 加 4 个 | ~40 |
| user/hello/src/main.rs | spawn smoke（先验版本基础上加 7..N） | ~80 |
| task.json | P4-T9 actual_approach 填 | ~10 |
| decision_log | P4-T9 条目 | 1 |

合计 ~840 行新增 + ~200 行修改。

## 风险与回退

1. **per-process AS 加载 ELF 路径**：现有 elf_load_smoke 是给 init 用的 hardcoded 地址。spawn 需要参数化的 ELF loader（输入 ELF bytes + 输出 child AS）。最小化：抽出 `elf_load_into(as, elf_bytes, entry_out, user_rsp_out)` 函数。
2. **death_endpoint 投递需解析父进程的 cap**：内核走 CAP_TABLES[parent_pid] → cap slot → ObjRef → ENDPOINTS[idx].try_send。失败（cap revoked / endpoint full / wrong ObjKind）→ 退化为 `log::warn!`（Phase 5+ 父进程阻塞时唤醒）。
3. **同步 spawn 阻塞 init**：smoke 测试 init 必须 yield / recv 后才能再次 spawn。Hello 程序加 spawn 子后 recv death signal 的循环。
4. **子进程无 init 全套 caps**：当前 ipc.rs ep_cap slot 1 是 init 的根 endpoint。子进程若没有这个 cap 就没法发 IPC。但 spawn 只 delegate 父显式给的 caps——MVP 测试可以让 init 给子也 delegate ep_cap。

## 验收口径（真机 smoke）

```text
T9 真机 smoke（追加到现有 kthread_ipc_smoke 之后）：
1. ipc::kthread_spawn_smoke():
   a. init spawn child (hello2 = 子 hello) — 成功返 pid
   b. child 跑 → process_exit(0) → death notification 到 init death_ep
   c. init recv → 验证 {pid=child, exit_code=0, fault=None}
   d. init reap child → 资源归零断言
   e. child 故意 #PF（NULL deref）→ fault → death notification {pid, code, fault=SegFault}
   f. init reap → 资源归零

Marker: 现有 u/v 之后加 w/x
```

三重判定：exit=363 + 无 PANIC + 5/5 smoke 全过。

## 待决策（02 → 03 时）

1. **per-process AS**：MVP 是"init 和 child 共享同一 CR3 但各自有独立 VMA" 还是 "child 独立 AS / CR3"？前者简单（无需 AS 切换）但无法做隔离；后者符合设计但工程量大。
   - 倾向：MVP 沿用 init 的 AS（child 在 init AS 上有专属 VMA 区），Phase 5+ 再切独立 AS。doc §3.3 VMA 支持 per-region owner 即可。
2. **child user stack**：init 的用户栈在 init AS 的某段（elf_load_smoke 已固化 4 页 RW+NX）。spawn 时给 child 单独分配？还是 init 的栈被复用？
   - 倾向：child 独立用户栈（同 AS 内另一段 VMA），避免与 init 串台。

这两个决策定 03-planning 之前要敲定。