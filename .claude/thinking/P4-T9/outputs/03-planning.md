# P4-T9 03-planning：子任务拆分 + 实现顺序

## 子模块依赖图

```
T9a per-process kstack + TSS.RSP0 切换
   │
   ├── T9b process_spawn syscall（依赖 T9a 才有子 kstack）
   │      │
   │      ├── T9c crash handling + death notification（依赖 T9b 才有子 pid）
   │      │      │
   │      │      └── T9d process_reap（依赖 T9c 才有 zombie）
   │      │
   │      └── T9e quota enforce in mmap（依赖 T9b 当前进程 pid 概念）
```

## MVP 决策（02 遗留）

1. **per-process AS**：MVP 沿用 init 的 AS（child 在 init AS 上专属 VMA 区）。独立 AS / CR3 切换推迟 Phase 5+。理由：现有 init AS 的 PML4 已建立用户页表，child 直接 map 进同一 AS 的高地址段即可，节省 PML4 切换 + AS 切换两段工程。
2. **child user stack**：child 独立用户栈 VMA（如 0x6000_0000 起的 4 页 RW+NX），避免与 init 栈串台。
3. **T9e 范围**：mmap 路径加 quota check（`current_pid.usage.pages + len/4KB ≤ quota.max_pages`）。spawn 配额已在 `synapse-proc::spawn` 内由 `check_spawn_grant` 覆盖，集成层不重复。reap 配额由 `synapse-proc::reap` 内 `uncarve` 覆盖。

## T9a per-process kstack（首个实现）

**目标**：解除 T7 Phase 2 的 boot fence，使任意用户进程都可独立运行 syscall。

**实现**：
- `kernel/src/kstack.rs`：`kstack_alloc()`（20KB = 4×4KB 页，1 guard + 3 usable） + `kstack_free(bottom)`
- `kernel/src/proc_ext.rs`：`PROC_EXT: [Option<ProcExt>; MAX_PROCS]`，PROC_EXT[pid] = {kstack_bottom, kstack_top, as_user, ...}
- init 启动时 PROC_EXT[INIT_PID] 用现有的 boot kstack（不重新分配，节省时间）
- `syscall.rs`：syscall_entry_asm 改 `gs:[0]` 读 kstack_top 为 `gs:[8]`（per-process kstack_top slot），保留 `gs:[0]` 为 current_pid（cap lookup 用）

**smoke**：T9a 自身可与 T7 ipc smoke 同框——确认 init 改 per-process kstack 后 syscall 仍正常。

## T9b process_spawn syscall（核心）

**目标**：init 可 spawn 子进程，子进程跑 hello 风格的用户代码并正常 exit。

**实现**：
- `abi/src/lib.rs`：加 `Syscall::ProcessSpawn { elf: CapRef, args: u64, caps: u64, n_caps: usize, death_ep: CapRef }`
- `abi/src/lib.rs`：`decode` 支持 #20；ABI_MINOR 3→4
- `user/src/wrappers.rs`：`invoke_process_spawn`
- `kernel/src/spawn.rs`：
  - `k_process_spawn(frame)` 主入口
  - 解析 parent (MVP 必须 INIT_PID) + elf_ref
  - 调用 `kstate::proc.spawn(parent, SpawnParams{agent, quota=DEFAULT, death_ep})` 拿新 pid
  - `kstack_alloc()` 写 PROC_EXT[new_pid]
  - 复用 `elfload::load_into(as, elf_data, &mut entry, &mut user_rsp)` 装入子进程
  - `synapse_cap::transfer_caps(parent → new_pid, items)` 委托初始 caps（slot 1..N）
  - 设置子进程 iretq 帧 RIP=entry / CS=0x2B / RFLAGS / RSP=user_rsp / SS=0x23
  - TSS.RSP0 = PROC_EXT[new_pid].kstack_top
  - **同步**：KERNEL_FRAME 接力 → iretq → 子进程跑 → 子 process_exit/fault iretq 回 → 控制流回 spawn → 投递 DeathSignal → 返回 pid
- `kernel/src/elfload.rs`：抽出 `elf_load_into(as, elf_bytes) -> (entry, user_rsp)` 公共函数，init 和 child 共用

**smoke**：init spawn 一个 dummy 子（同样的 hello2 二进制，但 arg = "child1"），子正常 exit(0)，spawn 返新 pid 给 init。

## T9c crash + death notification

**目标**：子进程非法访问 #PF → 内核归因 FaultKind::SegFault → 投递 DeathSignal → init recv 收到。

**实现**：
- `kernel/src/fault.rs`：`fault_process(pid, kind, code) -> DeathSignal`（调用 proc.fault + 走投递）
- `kernel/src/spawn.rs`：`deliver_death_signal(signal)` 投递函数：
  - 取 signal.pid 的 parent = `proc.get(pid).parent`
  - 取 parent.death_endpoint CapRef → 走 CAP_TABLES[parent_pid][slot] → ObjRef → ENDPOINTS[idx]
  - serialize DeathSignal 为 12 字节（pid u32 + exit_code i32 + fault_reason u8）
  - `Endpoint::try_send(payload)` 失败 → `log::warn!`
- `kernel/src/idt.rs`：page_fault_inner 末尾 + gp_inner 末尾 + dispatch_unimplemented 末尾 → 调用 fault_process
- 投递路径与 T9b spawn 返回路径共享（子死亡时 spawn 同步路径自动 deliver）

**smoke**：spawn 一个 fault 子（子二进制故意 NULL deref）→ 子 #PF → init spawn() 返回时已收到 DeathSignal {fault=SegFault} → 验证 exit_code + fault_reason。

## T9d process_reap

**目标**：init reap 释放 zombie 子进程的所有资源。

**实现**：
- `kernel/src/spawn.rs`：`k_process_reap(frame)` 主入口
- 解析 target pid
- 校验 caller（parent or init）+ target.Zombie（否则 `E_ZOMBIE` 或 `E_PERMISSION`）
- 释放顺序（Doc 02 §5.3）：**先 cap_table[pid] 清空 → 再 as_user unmap all → 再 kstack_free → 最后 proc.reap(caller, pid)**
  - 注：CapTable 释放走 `CAP_TABLES[pid] = None`，物理 cap entry 由 cap crate 内部处理（不需手动）
  - as_user 释放走 `paging::AddressSpace::Drop`（已实现，FR8 出账）
  - kstack_free 走 page_frame 归还
  - proc.reap 由 synapse-proc 完成（agent_id 释放 + pid 槽释放 + 配额 uncarve）

**smoke**：init 收完 child death → reap → 资源账本归零断言（page_frame.used 减少 + PROCS live_count 减少 + cap_slot[child_pid] 为 None）。

## T9e quota enforce in mmap

**目标**：每个 mmap syscall 校验 current_pid quota。

**实现**：
- `kernel/src/umem.rs`：`sys_mmap` 入口加 quota check：
  - 取 current_pid（per-CPU）
  - `usage.pages + (len + 0xFFF) / 0x1000 > quota.max_pages` → `E_QUOTA_EXCEEDED`
  - 成功后 `usage.pages += ...`
- `kernel/src/umem.rs`：`sys_munmap` 对应 `usage.pages -= ...`

**smoke**：init mmap 4MB 反复试错 → 第 N 次返 `E_QUOTA_EXCEEDED`。MVP 用 init 16MB DEFAULT_QUOTA 测试容易达到。

## 实现顺序（本轮）

按 T9a → T9b → T9c → T9d 顺序，每个子模块独立编译 + 真机 smoke 后推进。T9e 视余力附在最后。

| # | 子模块 | 估计 LOC | smoke 标志 |
|---|---|---|---|
| T9a | kstack + proc_ext + TSS.RSP0 | ~140 | init syscall 仍 exit 363 + IPC smoke 全过 |
| T9b | spawn syscall + ELF 装载参数化 | ~400 | init spawn 子 + 子 exit(0) → init 拿到 pid |
| T9c | fault + death notification | ~100 | 子 NULL deref → init 拿到 fault=SegFault |
| T9d | reap syscall | ~80 | reap 后资源账本归零 |
| T9e | quota in mmap | ~30 | 第 N 次 mmap → E_QUOTA_EXCEEDED |

合计 ~750 行 + abi/user 同步 ~120 行 + smoke ~100 行 ≈ **970 行新增**。

## 同步看板 / 文档

每子模块 commit 后：
1. `task.json` P4-T9 状态 `in_progress` → `done`，`actual_approach` 字段填本子模块关键发现
2. `decision_log` 追加 T9 条目（同步 spawn / child AS 共享 / VMA 区域分配等设计取舍）
3. `README.md` §3 P4 行更新（仅描述变更，行为不变）
4. `docs/design/02-userspace-abi-and-process-model.md` 同步 §5.2 同步 spawn 语义

## 验收最终态

- `cargo check --workspace` 干净
- `cargo clippy --workspace` 干净（除 vma::Default 等遗留问题）
- 真机 smoke 全过：T7 既有 IPC smoke + T9 新增 spawn smoke + 5 条变体（正常 exit / fault #PF / fault 非法指令 / reap 后资源归零 / quota 超限）
- exit code 363 + markers 完整（既有 Iabcdef + u/v + w/x）

## 与其他窗口的边界

- 本窗口只动 `kernel/src/`、`abi/src/`、`user/src/`、`user/hello/src/`、`task.json`、`README.md`、`docs/design/02*`
- `xtask/`、`display/`、`S6 相关`：不动
- `synapse-proc` / `synapse-cap` / `synapse-ipc`：不动（已满足接口）

## 暂停点 / 下轮可接

每个子模块 commit 后是一个自然暂停点。下个窗口可从 TODO 列表的下一项继续（不要从头分析）。