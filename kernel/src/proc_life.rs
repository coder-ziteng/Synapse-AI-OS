//! 进程生命周期的内核侧集成（P4-T9c/T9d/T9e）。
//!
//! ## 范围
//!
//! - **`current_is_spawned_child`**：当前 CR3 + PerCpu 上下文是否对应一个真实
//!   spawn 出的子进程（pid ≠ INIT_PID 且 PROC_EXT.user_as_ptr ≠ 0）。
//! - **`terminate_current`**：把当前进程按 [`FaultKind`] 归因 → 通过
//!   `death_endpoint` 投递 [`synapse_abi::DeathMsg`] → 释放资源
//!   （umem 清理 + AS 叶页 + AS drop + CapTable 销毁 + `release_resources`
//!   → Zombie）→ `handle_process_exit()` KERNEL_FRAME iretq 接力。**永不返回**。
//! - **`sys_reap`**：reap 后端（`process_reap` syscall #22 + spawn/crash
//!   续体内核侧调用）；`reap` 成功后回收 per-process kstack + 频率条目。
//! - **`recv_death_msg`**：续体侧从父进程持有的 death endpoint 读取
//!   [`DeathMsg`]（label 过滤）。
//!
//! ## 死信投递
//!
//! 内核向 death endpoint 注入一条 `SendRequest`：sender = `KERNEL_AGENT`
//! （`AgentId(0)`，注册表查无 → FR10 IPC 计数跳过；badge = pid 用于父进程
//! 路由定位 child），label = [`synapse_abi::DEATH_LABEL`]，payload 写入
//! [`DEATH_PAYLOAD_BUF`] 环形槽位（VA == PA 恒等映射，recv 时内核 PA-to-PA
//! 直读）。MVP 容量 4 槽——超出后新投递只 warn 不入队（不会无界堆积）。
//!
//! ## CR3 切换契约
//!
//! terminate 在子进程 CR3 仍激活时序：先 `cr3_write(KERNEL_CR3)`，再走
//! `umem::cleanup_all` + `AddressSpace::free_all_user_pages` + AS `Drop`
//! （`Drop::drop` 的 debug_assert 要求 `cr3 ≠ self.pml4`，切回内核 AS 才能
//! drop）。`KERNEL_CR3` 由 spawn_smoke 在第一次 `activate(child_as)` 前
//! 写入（= 内核 AS 的 PML4）。

use alloc::boxed::Box;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};

use log::{info, warn};

use synapse_abi::{DeathMsg, DEATH_LABEL, FAULT_GENERAL_PROTECTION, FAULT_ILLEGAL_INSTRUCTION,
    FAULT_ILLEGAL_SYSCALL, FAULT_NONE, FAULT_PANIC, FAULT_SEGFAULT};
use synapse_cap::{CapError, Rights, TransferItem, MAX_TRANSFER};
use synapse_ipc::{AgentId, RecvOutcome, SendOutcome, SendRequest};
use synapse_proc::process::{FaultKind, Pid, INIT_PID, DeathSignal};

use crate::kstate;
use crate::paging::{cr3_write, AddressSpace};
use crate::proc_ext;

/// 内核盖章的 AgentId（死信发送方）。`AgentRegistry::lookup(0)` 永不命中 → FR10
/// IPC 计数路径跳过（`k_ep_try_send` 调用 `record_ipc_rate` 反查 pid）。
pub const KERNEL_AGENT: AgentId = AgentId(0);

/// 死信 payload 环形槽数（同时在线的死亡事件上限）。超限后仅 warn，不 panic。
const DEATH_SLOTS: usize = 4;

/// 死信 payload 槽位（newtype + `UnsafeCell`，绕开 `static mut` 弃用并加 Sync）。
struct DeathSlotCell(UnsafeCell<DeathMsg>);
unsafe impl Sync for DeathSlotCell {}

/// 死信 payload 环形缓冲（identity-mapped，VA == PA）。
static DEATH_PAYLOAD_BUF: [DeathSlotCell; DEATH_SLOTS] = [
    const { DeathSlotCell(UnsafeCell::new(DeathMsg { pid: 0, exit_code: 0, fault: FAULT_NONE, rsv: 0 })) };
    DEATH_SLOTS
];
/// 下一写入槽位（环形，写入后 += 1 % DEATH_SLOTS）。
static DEATH_SLOT_CURSOR: AtomicU64 = AtomicU64::new(0);

/// 内核 AS 的 PML4 物理地址（spawn_smoke 进入子 CR3 前写入；terminate 切回用）。
static KERNEL_CR3: AtomicU64 = AtomicU64::new(0);

/// mmap 配额拒绝计数（P4-T9e 真机证据：crash_continuation 断言 ≥ 1）。
static QUOTA_DENIED: AtomicU64 = AtomicU64::new(0);
/// 死信投递：直接拷贝到等待接收方 / 入队 / 被丢（队列满 / cap 失效）。
static DEATH_DELIVERED: AtomicU64 = AtomicU64::new(0);
static DEATH_QUEUED: AtomicU64 = AtomicU64::new(0);
static DEATH_DROPPED: AtomicU64 = AtomicU64::new(0);

/// 由 boot 链路或 spawn_smoke 调用一次：记录内核 AS 的 PML4 物理地址
///（terminate 切回 CR3 用）。
pub fn set_kernel_cr3(cr3: u64) {
    KERNEL_CR3.store(cr3, Ordering::SeqCst);
}

/// 读内核 AS 的 CR3。
pub fn kernel_cr3() -> u64 {
    KERNEL_CR3.load(Ordering::SeqCst)
}

pub fn quota_denied_count() -> u64 {
    QUOTA_DENIED.load(Ordering::SeqCst)
}
pub fn death_delivered_count() -> u64 {
    DEATH_DELIVERED.load(Ordering::SeqCst)
}
pub fn death_queued_count() -> u64 {
    DEATH_QUEUED.load(Ordering::SeqCst)
}
pub fn death_dropped_count() -> u64 {
    DEATH_DROPPED.load(Ordering::SeqCst)
}

/// umem 配额拒绝时调用（T9e 真机证据）。
pub(crate) fn note_quota_denied() {
    QUOTA_DENIED.fetch_add(1, Ordering::SeqCst);
}

/// FaultKind → abi u32 编码（Doc 02 §5.3 death signal 编码契约）。
fn kind_to_abi(k: Option<FaultKind>) -> u32 {
    match k {
        None => FAULT_NONE,
        Some(FaultKind::SegFault) => FAULT_SEGFAULT,
        Some(FaultKind::Panic) => FAULT_PANIC,
        Some(FaultKind::IllegalSyscall) => FAULT_ILLEGAL_SYSCALL,
        Some(FaultKind::IllegalInstruction) => FAULT_ILLEGAL_INSTRUCTION,
        Some(FaultKind::GeneralProtection) => FAULT_GENERAL_PROTECTION,
    }
}

// ---------------------------------------------------------------------------
// spawned child 判定
// ---------------------------------------------------------------------------

/// 当前 PerCpu 上下文是否对应一个真实 spawn 出的子进程。
///
/// 条件：pid ≠ INIT_PID 且 proc 表条目 live 且 PROC_EXT.user_as_ptr ≠ 0
/// （= 子进程已装载用户 AS）。**init 自身不在此列**——init 走 legacy
/// KERNEL_FRAME relay 路径（boot 上下文）。
pub fn current_is_spawned_child() -> bool {
    let pid_raw = proc_ext::current_pid();
    if pid_raw == 0 {
        return false;
    }
    let pid = Pid(pid_raw);
    if pid == INIT_PID {
        return false;
    }
    let is_live = kstate::with_procs(|t| {
        t.get(pid).map(|p| p.state.is_live()).unwrap_or(false)
    });
    if !is_live {
        return false;
    }
    proc_ext::peek_user_as(pid)
}

// ---------------------------------------------------------------------------
// terminate_current
// ---------------------------------------------------------------------------

/// 终止当前进程（fault / exit 归因 + death 投递 + 资源释放 + Zombie），
/// 经 `KERNEL_FRAME` iretq 回到续体。**永不返回**。
///
/// # Safety
///
/// 调用方须确认 `current_is_spawned_child()` 为真，且 CR3 / RFLAGS / rsp0 已切
/// 到目标进程上下文（fault/syscall/exit handler 入口惯例）。
pub unsafe fn terminate_current(kind: Option<FaultKind>, code: i32) -> ! {
    let pid = Pid(proc_ext::current_pid());

    // 1. Proc 表状态转换：exit/fault → Exited/Faulted，并抽取
    //    death_endpoint / parent / DeathSignal。fields 在转换前后不变，
    //    可同临界区内一气读完。
    let (death_cap, parent, signal) = kstate::with_procs(|t| {
        let sig = match kind {
            Some(k) => t.fault(pid, k, code),
            None => t.exit(pid, code),
        }
        .expect("terminate_current on live child");
        let pcb = t.get(pid).expect("pcb just transitioned");
        (pcb.death_endpoint, pcb.parent, sig)
    });

    // 2. 切 CR3 回内核 AS（AddressSpace::drop 的 debug_assert 契约：
    //    cr3 ≠ self.pml4）。后续 cleanup 全部走 PA 恒等映射，与 CR3 无关。
    let kcr3 = kernel_cr3();
    if kcr3 != 0 {
        cr3_write(kcr3);
    }

    // 3. 物理资源回收：umem → AS 叶页 → AS drop → CapTable。
    let as_ptr = proc_ext::take_user_as(pid);
    if as_ptr != 0 {
        // SAFETY: as_ptr 由 Box::into_raw 产生，终止路径独占。
        unsafe {
            crate::umem::cleanup_all(as_ptr as *mut AddressSpace);
            let mut boxed: Box<AddressSpace> = Box::from_raw(as_ptr as *mut AddressSpace);
            let freed = boxed.free_all_user_pages();
            drop(boxed);
            info!(
                "[proc_life] child pid={} AS cleaned, freed {} leaf pages",
                pid.0, freed
            );
        }
    }
    kstate::k_destroy_cap_table(pid);

    // 4. Proc 表：Exited/Faulted → Zombie（释放 pid 之前的最后一步）。
    let _ = kstate::with_procs(|t| t.release_resources(pid));

    // 5. Death notification 投递：lock 序 CAP_TABLES → ENDPOINTS（先于 handle_process_exit，
    //    因后者永不返回）。
    deliver_death(parent, death_cap, pid, &signal, kind);

    // 6. KERNEL_FRAME iretq 接力回到续体（spawn_continuation / crash_continuation）。
    // SAFETY: KERNEL_FRAME 已武装。
    unsafe { crate::ring3::handle_process_exit() }
}

/// 内核向 death_endpoint 投递 [`DeathMsg`]。
///
/// 锁序 CAP_TABLES → ENDPOINTS（与 kstate 约定一致）。投递失败仅 warn，不
/// 影响进程 terminate 主路径（父进程错过了通知，proc 表查询仍能获知 pid
/// 是否 reap）。
fn deliver_death(parent: Pid, death_cap: u8, dying_pid: Pid, signal: &DeathSignal, kind: Option<FaultKind>) {
    if death_cap == 0 {
        warn!(
            "[proc_life] pid={} no death_endpoint registered; signal dropped",
            dying_pid.0
        );
        DEATH_DROPPED.fetch_add(1, Ordering::SeqCst);
        return;
    }

    // 写 payload 到环形槽（identity-mapped，VA == PA）。
    let slot_idx = DEATH_SLOT_CURSOR.fetch_add(1, Ordering::SeqCst) as usize % DEATH_SLOTS;
    let payload_pa;
    let msg = DeathMsg {
        pid: dying_pid.0,
        exit_code: signal.exit_code,
        fault: kind_to_abi(kind),
        rsv: 0,
    };
    // SAFETY: 槽位由 AtomicU64 cursor 串行化；写入 16 字节对齐结构。
    unsafe {
        let cell = &DEATH_PAYLOAD_BUF[slot_idx].0;
        core::ptr::write(cell.get(), msg);
        payload_pa = cell.get() as u64;
    }

    // 解析父 cap 表 → ObjRef。
    let obj_opt = kstate::with_cap_table(parent, |t| t.get(death_cap).ok().map(|c| c.obj));
    let Some(obj) = obj_opt else {
        warn!(
            "[proc_life] pid={} death_endpoint cap={} in parent {} invalid; signal dropped",
            dying_pid.0, death_cap, parent.0
        );
        DEATH_DROPPED.fetch_add(1, Ordering::SeqCst);
        return;
    };

    let req = SendRequest {
        sender: KERNEL_AGENT,
        badge: dying_pid.0,
        label: DEATH_LABEL,
        payload_len: core::mem::size_of::<DeathMsg>() as u32,
        payload_addr: payload_pa,
        caps: [TransferItem { cptr: 0, mask: Rights::EMPTY }; MAX_TRANSFER],
        cap_count: 0,
    };

    match kstate::k_ep_try_send(obj, req) {
        Ok(SendOutcome::Delivered) => {
            DEATH_DELIVERED.fetch_add(1, Ordering::SeqCst);
            info!(
                "[proc_life] pid={} death signal DELIVERED (label={:#x}) to parent {} ep {}:{}",
                dying_pid.0, DEATH_LABEL, parent.0, obj.index, obj.generation
            );
        }
        Ok(SendOutcome::Queued) => {
            DEATH_QUEUED.fetch_add(1, Ordering::SeqCst);
            info!(
                "[proc_life] pid={} death signal QUEUED to parent {} ep {}:{}",
                dying_pid.0, parent.0, obj.index, obj.generation
            );
        }
        Err(e) => {
            DEATH_DROPPED.fetch_add(1, Ordering::SeqCst);
            warn!(
                "[proc_life] pid={} death signal DROPPED: {:?} (errno={})",
                dying_pid.0,
                e,
                e.errno()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// sys_reap（process_reap syscall #22 + 续体调用）
// ---------------------------------------------------------------------------

/// reap 后端：ProcessTable::reap + kstack uninstall + FR10 频率条目回收。
///
/// 失败返回原始 `CapError`（调用方映射为错误码）。成功后 Zombie → slot 释放 +
/// agent_id 释放 + 父配额划拨归还。
pub fn sys_reap(reaper: Pid, target: Pid) -> Result<(), CapError> {
    kstate::with_procs(|t| t.reap(reaper, target))?;
    // reap 成功后清理 per-process 资源（kstack + FR10 频率表）。
    proc_ext::uninstall_kstack(target);
    kstate::k_rate_unregister(target);
    Ok(())
}

// ---------------------------------------------------------------------------
// recv_death_msg（续体侧）
// ---------------------------------------------------------------------------

/// 从父进程持有的 death endpoint 读取一条 death 消息（label 过滤）。
///
/// 续体（如 `spawn_continuation` / `crash_continuation`）调用。返回 `None` =
/// 队列空 / 队首 label 不匹配。**不会**在 Waiting 时挂起（续体上下文不能阻塞）；
/// 调用方契约 = death 投递在先、recv 在后。
pub fn recv_death_msg(parent: Pid, death_cap: u8) -> Option<DeathMsg> {
    if death_cap == 0 {
        return None;
    }
    let obj = kstate::with_cap_table(parent, |t| t.get(death_cap).ok().map(|c| c.obj))?;
    // 反复 dequeue 直到遇到 DEATH_LABEL（或队列耗尽）。
    loop {
        match kstate::k_ep_recv(obj) {
            Ok(RecvOutcome::Message(req)) => {
                if req.label == DEATH_LABEL {
                    // SAFETY: payload_pa 是环形槽的物理地址（identity mapping）。
                    let msg = unsafe { *(req.payload_addr as *const DeathMsg) };
                    return Some(msg);
                }
                // 非 death label 消息（理论上 smoke 不会发生；保守跳过）
                info!(
                    "[proc_life] recv_death_msg: skipped non-death label={:#x}",
                    req.label
                );
                continue;
            }
            Ok(RecvOutcome::Waiting) => {
                // 续体上下文不应阻塞；回滚 receiver_waiting 并返回 None。
                let _ = kstate::k_ep_cancel_recv(obj);
                return None;
            }
            Err(e) => {
                warn!("[proc_life] recv_death_msg: k_ep_recv failed: {:?}", e);
                return None;
            }
        }
    }
}
