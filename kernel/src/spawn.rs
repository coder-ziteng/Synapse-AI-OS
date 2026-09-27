//! process_spawn 内核实现 + smoke（P4-T9b）。
//!
//! ## 目标
//!
//! init 可 spawn 子进程，子进程跑 hello ELF 并正常 exit。验证：
//! - `proc.spawn()` 创建子 Pcb + 配额 carve
//! - `proc_ext::install_kstack()` 分配 per-process kstack
//! - 子进程在 ring-3 执行 hello ELF（syscall abi_query 往返）
//! - `process_exit` → KERNEL_FRAME iretq 回 `spawn_continuation`
//! - 续体清理子进程资源（AS / kstack / CapTable / proc 槽）
//! - FR8 账本归零
//!
//! ## MVP 约束
//!
//! - 仅 init (pid=1) 可 spawn（`synapse-proc::spawn` 强制）
//! - 子进程共享 init 的 kernel AS（不切独立 CR3 给 kernel；用户 AS 独立）
//! - death_endpoint 暂传 0（T9c 补 death notification）
//! - agent_id 取 `child_pid.0 + 100`（避免与 init=1 冲突）
//!
//! ## 流程
//!
//! 1. `spawn_smoke()` 在 boot 链路末尾调用（`elf_continuation` 之后）
//! 2. `proc.spawn(INIT_PID, SpawnParams{...})` → child_pid
//! 3. `proc_ext::install_kstack(child_pid)`
//! 4. `kstate::k_create_cap_table(child_pid)`
//! 5. `elfload::load_hello_into_as()` → (child_as, entry, stack_top)
//! 6. 武装 KERNEL_FRAME（rip=spawn_continuation, rsp=当前内核栈）
//! 7. 保存 init 状态（CR3, kstack_top）
//! 8. 切到子进程：`proc_ext::switch_to_process(child_pid)` + `gdt::set_rsp0` + `child_as.activate()`
//! 9. `ring3::enter_user_at(entry, stack_top - 8)` → iretq 到子进程
//! 10. 子进程跑 hello → process_exit → handle_process_exit → iretq 到 spawn_continuation
//! 11. `spawn_continuation` 恢复 init 状态 + 清理子资源 + QEMU exit 363

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use log::info;
use x86_64::instructions::segmentation::{CS, Segment};

use synapse_cap::{CapError, DEFAULT_QUOTA, ObjKind, Quota, Rights};
use synapse_proc::process::{Pid, SpawnParams, INIT_PID};
use synapse_ipc::AgentId;

use crate::page_frame::{free_frame, with_page_frames};
use crate::paging::AddressSpace;

/// spawn smoke 内部状态（spawn_continuation 读）。
static SPAWN_CHILD_PID: AtomicU64 = AtomicU64::new(0);
static SPAWN_OLD_CR3: AtomicU64 = AtomicU64::new(0);
static SPAWN_OLD_RSP0: AtomicU64 = AtomicU64::new(0);
static SPAWN_CHILD_AS_PTR: AtomicU64 = AtomicU64::new(0);
static SPAWN_BASE_USED: AtomicU64 = AtomicU64::new(0);
static SPAWN_DEATH_CAP: AtomicU64 = AtomicU64::new(0);
static SMOKE_DONE: AtomicBool = AtomicBool::new(false);

/// P4-T9b spawn smoke：由 `elf_continuation` 链式接力调用。
///
/// spawn 一个子进程跑 hello ELF → 子 process_exit → spawn_continuation 清理 → exit 363。
pub fn spawn_smoke() -> ! {
    if SMOKE_DONE.swap(true, Ordering::SeqCst) {
        panic!("spawn_smoke twice");
    }
    info!("[spawn-smoke] start");

    // 记录基线 used_frames
    let base_used = with_page_frames(|a| a.used_frames()) as u64;
    SPAWN_BASE_USED.store(base_used, Ordering::SeqCst);

    // 取当前内核栈 RSP + RFLAGS（spawn_continuation 恢复用）
    let ksp: u64;
    let krflags: u64;
    unsafe {
        asm!("mov {}, rsp", out(reg) ksp, options(nomem, nostack, preserves_flags));
        asm!("pushf; pop {}", out(reg) krflags, options(nomem, nostack, preserves_flags));
    }

    // 保存 init 状态
    let old_cr3 = crate::paging::cr3_read();
    let old_rsp0 = crate::gdt::rsp0_stack_top();
    SPAWN_OLD_CR3.store(old_cr3, Ordering::SeqCst);
    SPAWN_OLD_RSP0.store(old_rsp0, Ordering::SeqCst);

    // 1. proc.spawn() → child_pid
    let child_pid = do_spawn(DEFAULT_QUOTA).expect("[spawn-smoke] proc.spawn failed");
    info!("[spawn-smoke]   ok: proc.spawn → child_pid={}", child_pid.0);
    SPAWN_CHILD_PID.store(child_pid.0 as u64, Ordering::SeqCst);

    // 2. install kstack
    crate::proc_ext::install_kstack(child_pid)
        .expect("[spawn-smoke] install_kstack failed");
    let child_kstack_top = crate::proc_ext::kstack_top_of(child_pid)
        .expect("[spawn-smoke] kstack_top_of failed");
    info!("[spawn-smoke]   ok: kstack installed, top={:#x}", child_kstack_top);

    // 3. create CapTable
    crate::kstate::k_create_cap_table(child_pid)
        .expect("[spawn-smoke] k_create_cap_table failed");
    info!("[spawn-smoke]   ok: CapTable created");

    // 3b. 给子进程铸造**自有**的 endpoint 根 cap → slot 1。
    //     hello §6 IPC 用例硬编码 ep_cap=1，而子进程 CapTable 是新建空表：
    //     不铸则 6.2 IpcRecv(cptr=1) 的 resolve_endpoint 失败 → E_INVALID_CAP(-1)，
    //     但 hello 期望 boot 围栏的 E_WOULD_BLOCK(-4) → assert 失败 panic → ring3
    //     hlt #GP（真机暴露：子进程曾走 kill 路径而非自身 process_exit 退出）。
    //     必须用**全新** endpoint，不能复用 init 的 bootstrap ep：init 的 hello
    //     §6.6 try_send 已向其 Queued 一条消息，子进程 6.2 recv 会出队 Message
    //     而非 Waiting，同样打破 -4 期望。
    let child_ep_obj = crate::kstate::k_alloc_object(synapse_cap::ObjKind::Endpoint)
        .expect("[spawn-smoke] alloc child endpoint failed");
    let child_ep_cap = crate::kstate::k_mint_root(
        child_pid,
        child_ep_obj,
        synapse_cap::Rights::SEND
            | synapse_cap::Rights::RECV
            | synapse_cap::Rights::REPLY
            | synapse_cap::Rights::GRANT,
    )
    .expect("[spawn-smoke] mint child ep cap failed");
    assert_eq!(
        child_ep_cap, 1,
        "[spawn-smoke] child ep cap must land in slot 1 (hello hardcodes ep_cap=1), got {}",
        child_ep_cap
    );
    info!(
        "[spawn-smoke]   ok: child endpoint minted → cap slot {} (obj {}:{})",
        child_ep_cap, child_ep_obj.index, child_ep_obj.generation
    );

    // 3b-bis. P4-T8：给子进程铸造自有 notification 根 cap → slot 2。
    //     hello §8 Notification signal/wait 用例硬编码 no_cap=2，与 init 的
    //     bootstrap no_cap 同槽号；子进程 CapTable 是新建空表，必须铸则 §8
    //     NotificationSignal(cptr=2) 走 resolve_notification 失败 → -1 → assert
    //     panic → ring3 #GP → terminate_current → FAULT_GENERAL_PROTECTION(5)
    //     与 spawn-smoke §3d FAULT_NONE 期望不符。用全新 no_obj（不复用 init 的
    //     bootstrap no——init hello §8 signal 已写入位图，复用会让子首次 wait 即
    //     拿到非零脏数据）。
    let child_no_obj = crate::kstate::k_alloc_object(synapse_cap::ObjKind::Notification)
        .expect("[spawn-smoke] alloc child notification failed");
    let child_no_cap = crate::kstate::k_mint_root(
        child_pid,
        child_no_obj,
        synapse_cap::Rights::SEND | synapse_cap::Rights::RECV,
    )
    .expect("[spawn-smoke] mint child no cap failed");
    assert_eq!(
        child_no_cap, 2,
        "[spawn-smoke] child no cap must land in slot 2 (hello hardcodes no_cap=2), got {}",
        child_no_cap
    );
    info!(
        "[spawn-smoke]   ok: child notification minted → cap slot {} (obj {}:{})",
        child_no_cap, child_no_obj.index, child_no_obj.generation
    );

    // 4. load hello ELF into child AS
    let (mut child_as, entry, stack_top) = crate::elfload::load_hello_into_as();
    SPAWN_CHILD_AS_PTR.store(&mut child_as as *mut AddressSpace as u64, Ordering::SeqCst);
    // syscall 分发层（gettime/mmap/munmap）经 elfload::current_as_ptr() 取激活的
    // 用户 AS 走页表——必须指向子进程 AS。否则子进程 gettime 会 walk elf_continuation
    // 已 drop 的陈旧 init AS → E_INVALID_ADDR → hello §3 assert panic（真机暴露：
    // abi_query/yield 不碰 AS 故正常，gettime 一碰即 ring3 #GP）。
    crate::elfload::set_current_as_ptr(&mut child_as as *mut AddressSpace as u64);
    // P4-T9c：把子 AS 指针写入 PROC_EXT[pid].user_as_ptr，fault terminate 路径
    // （proc_life::terminate_current）据此回收用户页（unmap+free）+ drop AS。
    crate::proc_ext::set_user_as(child_pid, &mut child_as as *mut AddressSpace as u64);

    // 5. 武装 KERNEL_FRAME（spawn_continuation 为 RIP）
    unsafe {
        crate::ring3::write_kernel_frame(
            spawn_continuation as *const () as u64,
            ksp,
            krflags,
        );
    }
    info!(
        "[spawn-smoke]   ok: KERNEL_FRAME armed (rip={:#x} rsp={:#x})",
        spawn_continuation as *const () as u64,
        ksp
    );

    // 6. 切到子进程上下文
    crate::proc_ext::switch_to_process(child_pid);
    unsafe { crate::gdt::set_rsp0(child_kstack_top) };
    // P4-T9c：记录内核 AS PML4 物理地址——terminate_current 切回 CR3 用
    //（必须在 activate 之前：kernel_cr3 自身就是被覆盖的那个值）
    crate::proc_life::set_kernel_cr3(old_cr3);
    unsafe { child_as.activate() };
    info!(
        "[spawn-smoke]   switched to child: pid={} cr3={:#x} rsp0={:#x}",
        child_pid.0, child_as.pml4_phys(), child_kstack_top
    );

    // Debug: 验证 kstack 在新 CR3 下可写
    unsafe {
        let probe_addr = child_kstack_top; // top of kstack
        core::ptr::write_volatile(probe_addr as *mut u64, 0xDEADBEEF);
        let val = core::ptr::read_volatile(probe_addr as *mut u64);
        info!("[spawn-smoke]   kstack probe: [{:#x}] = {:#x} (expected 0xdeadbeef)", probe_addr, val);
        assert_eq!(val, 0xDEADBEEF, "kstack not writable after CR3 switch!");
    }

    // Debug: 验证 PerCpu.kstack_top (gs:[8]) 可读且值正确
    unsafe {
        let per_cpu_addr = crate::proc_ext::per_cpu_pointer();
        let kstack_top_from_percpu = core::ptr::read_volatile((per_cpu_addr + 8) as *const u64);
        info!(
            "[spawn-smoke]   PerCpu probe: per_cpu@{:#x}, gs:[8] (kstack_top) = {:#x} (expected {:#x})",
            per_cpu_addr, kstack_top_from_percpu, child_kstack_top
        );
        assert_eq!(kstack_top_from_percpu, child_kstack_top, "PerCpu.kstack_top mismatch!");
    }

    // Debug: 验证子进程用户栈也可访问
    unsafe {
        let user_stack_probe = stack_top - 8; // 入口 RSP
        core::ptr::write_volatile(user_stack_probe as *mut u64, 0xCAFEBABE);
        let val = core::ptr::read_volatile(user_stack_probe as *mut u64);
        info!(
            "[spawn-smoke]   user stack probe: [{:#x}] = {:#x} (expected 0xcafebabe)",
            user_stack_probe, val
        );
        assert_eq!(val, 0xCAFEBABE, "user stack not writable!");
    }

    // 7. iretq 到子进程 entry
    info!(
        "[spawn-smoke]   iretq to child: entry={:#x} rsp={:#x}",
        entry,
        stack_top - 8
    );
    // SAFETY: entry/栈页均已映射且权限正确；KERNEL_FRAME 已武装。
    unsafe { crate::ring3::enter_user_at(entry, stack_top - 8) }
}

/// proc.spawn 封装：构造 SpawnParams 并调用 ProcessTable::spawn。
///
/// `quota` 由调用方传入（hello 用 [`DEFAULT_QUOTA`]，crasher 用小配额）。agent_id
/// 走 tick+100 单调递增（确保与 init=1 不冲突）。
fn do_spawn(quota: Quota) -> Result<Pid, CapError> {
    // MVP: agent_id = child 预估值（实际 pid 由 proc 表分配，这里用递增 ID 避开冲突）
    // 实际 pid 由 proc.spawn 返回；agent_id 只要唯一即可。
    // 用 tick count 做简单唯一化。
    let agent_id_raw = crate::pit::tick_count().wrapping_add(100) as u32;
    let agent = AgentId(agent_id_raw);

    // P4-T9c：铸造**专用** death endpoint——init 持有一个 recv-only cap，子进程死亡时
    // 内核向其投递 DeathMsg。新建独立 endpoint 而非复用 init bootstrap ep：避免子进程
    // IPC recv 误把 death msg 当普通消息读出（label 过滤可解，但语义上独立更清晰）。
    let death_obj = crate::kstate::k_alloc_object(ObjKind::Endpoint)
        .expect("[spawn] alloc death endpoint failed");
    let death_cap = crate::kstate::k_mint_root(
        INIT_PID,
        death_obj,
        Rights::RECV,
    )
    .expect("[spawn] mint death cap for init failed");
    info!(
        "[spawn]   ok: init death_endpoint minted → cap slot {} (obj {}:{})",
        death_cap, death_obj.index, death_obj.generation
    );

    let params = SpawnParams {
        agent,
        quota,
        death_endpoint: death_cap,
    };

    let child_pid = crate::kstate::with_procs(|t| t.spawn(INIT_PID, params))?;
    // T9c 续体要 recv 这个 death_ep，先静态存下 init 的 cap slot。
    SPAWN_DEATH_CAP.store(death_cap as u64, Ordering::SeqCst);
    Ok(child_pid)
}

// ============================================================================
// ring-0 continuation（子进程 process_exit 经 KERNEL_FRAME iretq 到此）
// ============================================================================

/// spawn 续体：子进程退出后恢复 init 状态 + 清理子资源 + exit 363。
#[no_mangle]
extern "C" fn spawn_continuation() -> ! {
    // 1. 确证 ring-0
    let cs = CS::get_reg();
    assert_eq!(cs.0 & 3, 0, "CS.RPL must be 0 in spawn continuation");
    info!("[spawn-smoke]   ok: continuation in ring-0");

    // 2. 恢复 init 上下文
    let old_cr3 = SPAWN_OLD_CR3.load(Ordering::SeqCst);
    let old_rsp0 = SPAWN_OLD_RSP0.load(Ordering::SeqCst);
    unsafe { crate::paging::cr3_write(old_cr3) };
    unsafe { crate::gdt::set_rsp0(old_rsp0) };
    crate::proc_ext::switch_to_process(INIT_PID);
    info!(
        "[spawn-smoke]   ok: restored init context: cr3={:#x} rsp0={:#x}",
        old_cr3, old_rsp0
    );

    // 3. 清理子进程资源
    let child_pid = Pid(SPAWN_CHILD_PID.load(Ordering::SeqCst) as u32);
    let as_ptr = SPAWN_CHILD_AS_PTR.load(Ordering::SeqCst) as *mut AddressSpace;

    // P4-T9c：判定子进程是 process_exit 路径（Exited）还是 fault terminate 路径
    //（Zombie）。后者已被 proc_life::terminate_current 完成 AS/CapTable 清理，
    // 此处仅做 kstack + 频率条目收尾，不重复 unmap/drop。
    let child_state = crate::kstate::with_procs(|t| {
        t.get(child_pid).map(|p| p.state)
    });
    let already_terminated = matches!(
        child_state,
        Some(synapse_proc::process::ProcState::Zombie)
    );
    info!(
        "[spawn-smoke]   child state at continuation entry: {:?} (already_terminated={})",
        child_state, already_terminated
    );

    // 3a. 清理子 AS（unmap + free 所有帧 + drop AS）— 仅 process_exit 路径需要
    if !already_terminated && !as_ptr.is_null() {
        let child_as = unsafe { &mut *as_ptr };
        // 清理 ELF 段 + 栈（用 extract_and_parse 重走解析，确定性）
        let parsed = crate::elfload::extract_and_parse();
        let frame_size = crate::page_frame::FRAME_SIZE as u64;
        let mut freed = 0u64;
        for seg in parsed.loads() {
            for i in 0..seg.page_count() {
                let va = seg.page_start() + i * frame_size;
                if let Ok(pa) = child_as.unmap_page(va) {
                    free_frame(pa);
                    freed += 1;
                }
            }
        }
        // 清理栈
        let stack_top = crate::elfload::ELF_STACK_TOP;
        let stack_pages = crate::elfload::ELF_STACK_PAGES;
        let stack_base = stack_top - stack_pages * frame_size;
        for va in (stack_base..stack_top).step_by(frame_size as usize) {
            if let Ok(pa) = child_as.unmap_page(va) {
                free_frame(pa);
                freed += 1;
            }
        }
        // 清理 umem 遗留（mmap 区域）
        unsafe { crate::umem::cleanup_all(child_as) };
        // drop AS（归还页表帧）
        unsafe { core::ptr::drop_in_place(as_ptr) };
        info!("[spawn-smoke]   ok: child AS cleaned, freed {} frames", freed);
    } else if already_terminated {
        info!("[spawn-smoke]   ok: child AS already cleaned by terminate_current (fault path)");
    }

    // 3b. 清理 kstack — 两路径都需要
    crate::proc_ext::uninstall_kstack(child_pid);
    info!("[spawn-smoke]   ok: kstack uninstalled");

    // 3c. 清理 CapTable — 仅 process_exit 路径需要（terminate_current 已做）
    if !already_terminated {
        crate::kstate::k_destroy_cap_table(child_pid);
        info!("[spawn-smoke]   ok: CapTable destroyed");
    } else {
        info!("[spawn-smoke]   ok: CapTable already destroyed by terminate_current");
    }

    // 3d. P4-T9c：从 init 持有的 death endpoint 读取死亡消息——验证内核
    //     terminate_current 投递的 DeathMsg 确实落入 parent cap table 能
    //     看到的 endpoint 队列（label 过滤后取出）。
    let death_cap = SPAWN_DEATH_CAP.load(Ordering::SeqCst) as u8;
    if death_cap != 0 {
        if let Some(msg) = crate::proc_life::recv_death_msg(INIT_PID, death_cap) {
            info!(
                "[spawn-smoke]   ok: death msg recv: pid={} exit_code={} fault={} (expect normal-exit)",
                msg.pid, msg.exit_code, msg.fault
            );
            // 正常 process_exit → fault 应为 FAULT_NONE(0)
            assert_eq!(msg.fault, synapse_abi::FAULT_NONE,
                "[spawn-smoke] expected FAULT_NONE for clean process_exit, got {}", msg.fault);
            assert_eq!(msg.pid, child_pid.0,
                "[spawn-smoke] death msg pid mismatch");
        } else {
            info!("[spawn-smoke]   warn: death msg recv returned None (queue empty or wrong label)");
        }
    }

    // 3e. proc.reap() — 释放 Pcb + 配额 uncarve + agent 注销
    //     T9d：现在子进程已被 terminate_current 推到 Zombie，sys_reap 可执行。
    //     reap 成功 → per-process kstack 已被 proc_life::sys_reap 释放，
    //     此处 uninstall_kstack 是 no-op（双重保险）。
    let reap_ok = crate::proc_life::sys_reap(INIT_PID, child_pid).is_ok();
    if reap_ok {
        info!("[spawn-smoke]   ok: proc reap succeeded (T9d)");
    } else {
        info!("[spawn-smoke]   warn: proc reap failed (already terminated or wrong state)");
    }

    // 3e. 清理频率计数
    crate::kstate::k_rate_unregister(child_pid);

    // 4. 验证 FR8 账本归零
    let used_post = with_page_frames(|a| a.used_frames()) as u64;
    let base = SPAWN_BASE_USED.load(Ordering::SeqCst);
    assert_eq!(
        used_post, base,
        "[spawn-smoke] FR8 leak: used {used_post} != baseline {base}"
    );
    info!("[spawn-smoke]   ok: FR8 ledger back to baseline ({base})");

    info!("[spawn-smoke] PASS");

    // 5. 链式到 crash_smoke（crasher 子进程：SegFault 路径）
    crash_smoke()
}

// ============================================================================
// crash smoke（P4-T9e）：crasher 子进程小配额 + 写未映射页 → SegFault
// ============================================================================

/// crash smoke 状态（与 spawn smoke 共享 BASE_USED 基线）。
static CRASH_BASE_USED: AtomicU64 = AtomicU64::new(0);
static CRASH_DONE: AtomicBool = AtomicBool::new(false);

/// P4-T9e crash smoke：由 `spawn_continuation` 链式接力调用。
///
/// spawn crasher 子进程（小配额 max_pages=16）→ 子进程 mmap 1 页 OK → mmap 64
/// 页 → quota 拒绝 (-13) → munmap 1 页 → 写未映射 0x5000_0000 → SegFault → 内核
/// 走 `terminate_current(SegFault)` → 链式到 `crash_continuation` 清理 + exit 363。
pub fn crash_smoke() -> ! {
    if CRASH_DONE.swap(true, Ordering::SeqCst) {
        panic!("crash_smoke twice");
    }
    info!("[crash-smoke] start");

    // 记录本 phase 入口基线（hello 子进程已 reap → AS 已回收，但本 phase
    // 自身仍要从入口计起）
    let base_used = with_page_frames(|a| a.used_frames()) as u64;
    CRASH_BASE_USED.store(base_used, Ordering::SeqCst);

    // 取当前内核栈 RSP + RFLAGS
    let ksp: u64;
    let krflags: u64;
    unsafe {
        asm!("mov {}, rsp", out(reg) ksp, options(nomem, nostack, preserves_flags));
        asm!("pushf; pop {}", out(reg) krflags, options(nomem, nostack, preserves_flags));
    }

    // 保存 init 状态
    let old_cr3 = crate::paging::cr3_read();
    let old_rsp0 = crate::gdt::rsp0_stack_top();
    SPAWN_OLD_CR3.store(old_cr3, Ordering::SeqCst);
    SPAWN_OLD_RSP0.store(old_rsp0, Ordering::SeqCst);

    // 1. proc.spawn(crasher, small_quota) → child_pid
    let small_quota = Quota {
        max_pages: 16,
        max_threads: 1,
        max_caps: 4,
        max_endpoints: 1,
        max_msg_size: 64,
        max_pending_ipc: 1,
        max_grants: 1,
    };
    let child_pid = do_spawn(small_quota).expect("[crash-smoke] proc.spawn failed");
    info!("[crash-smoke]   ok: proc.spawn → child_pid={} (name=crasher, max_pages=16)", child_pid.0);
    SPAWN_CHILD_PID.store(child_pid.0 as u64, Ordering::SeqCst);

    // 2. install kstack
    crate::proc_ext::install_kstack(child_pid)
        .expect("[crash-smoke] install_kstack failed");
    let child_kstack_top = crate::proc_ext::kstack_top_of(child_pid)
        .expect("[crash-smoke] kstack_top_of failed");

    // 3. create CapTable
    crate::kstate::k_create_cap_table(child_pid)
        .expect("[crash-smoke] k_create_cap_table failed");

    // 3b. crasher 不消费 ep，跳过
    // 4. load crasher ELF into child AS
    let (mut child_as, entry, stack_top) = crate::elfload::load_elf_into_as("crasher");
    SPAWN_CHILD_AS_PTR.store(&mut child_as as *mut AddressSpace as u64, Ordering::SeqCst);
    crate::elfload::set_current_as_ptr(&mut child_as as *mut AddressSpace as u64);
    crate::proc_ext::set_user_as(child_pid, &mut child_as as *mut AddressSpace as u64);

    // 5. 武装 KERNEL_FRAME（crash_continuation 为 RIP）
    unsafe {
        crate::ring3::write_kernel_frame(
            crash_continuation as *const () as u64,
            ksp,
            krflags,
        );
    }
    info!(
        "[crash-smoke]   ok: KERNEL_FRAME armed (rip={:#x} rsp={:#x})",
        crash_continuation as *const () as u64,
        ksp
    );

    // 6. 切到子进程上下文
    crate::proc_ext::switch_to_process(child_pid);
    unsafe { crate::gdt::set_rsp0(child_kstack_top) };
    crate::proc_life::set_kernel_cr3(old_cr3);
    unsafe { child_as.activate() };
    info!(
        "[crash-smoke]   switched to child: pid={} cr3={:#x} rsp0={:#x}",
        child_pid.0, child_as.pml4_phys(), child_kstack_top
    );

    // 7. iretq 到子进程 entry
    info!(
        "[crash-smoke]   iretq to crasher: entry={:#x} rsp={:#x}",
        entry,
        stack_top - 8
    );
    // SAFETY: entry/栈页均已映射且权限正确；KERNEL_FRAME 已武装。
    unsafe { crate::ring3::enter_user_at(entry, stack_top - 8) }
}

// ============================================================================
// ring-0 continuation：crasher SegFault 经 terminate_current → handle_process_exit
// → KERNEL_FRAME iretq 到此处 → 清理 + exit 363
// ============================================================================

/// crash 续体：crasher 子进程因 SegFault 被 terminate_current 经 KERNEL_FRAME
/// iretq 到此处。验证：
/// - death msg.fault == FAULT_SEGFAULT(1)
/// - proc_life::quota_denied_count ≥ 1（crasher mmap 超配额触发）
/// - proc_life::death_delivered_count + death_queued_count ≥ 1
/// - FR8 账本归零
#[no_mangle]
extern "C" fn crash_continuation() -> ! {
    let cs = CS::get_reg();
    assert_eq!(cs.0 & 3, 0, "CS.RPL must be 0 in crash continuation");
    info!("[crash-smoke]   ok: continuation in ring-0");

    let pid = Pid(SPAWN_CHILD_PID.load(Ordering::SeqCst) as u32);

    // 恢复 init 上下文
    let old_cr3 = SPAWN_OLD_CR3.load(Ordering::SeqCst);
    let old_rsp0 = SPAWN_OLD_RSP0.load(Ordering::SeqCst);
    unsafe { crate::paging::cr3_write(old_cr3) };
    unsafe { crate::gdt::set_rsp0(old_rsp0) };
    crate::proc_ext::switch_to_process(INIT_PID);

    // crasher 走 terminate_current 路径 → AS/CapTable/proc 已被 proc_life 清理
    //（k_destroy_cap_table + umem::cleanup_all + AS unmap/free/drop + release_resources）。
    // 此处只摘 kstack（proc_life::sys_reap 内部还会再调一次 = no-op）。
    crate::proc_ext::uninstall_kstack(pid);
    info!("[crash-smoke]   ok: kstack uninstalled");

    // 读 death msg（crasher 期望 FAULT_SEGFAULT）
    let death_cap = SPAWN_DEATH_CAP.load(Ordering::SeqCst) as u8;
    let mut saw_segfault = false;
    if death_cap != 0 {
        if let Some(msg) = crate::proc_life::recv_death_msg(INIT_PID, death_cap) {
            info!(
                "[crash-smoke]   ok: crasher death msg: pid={} exit_code={} fault={} (expect FAULT_SEGFAULT=1)",
                msg.pid, msg.exit_code, msg.fault
            );
            assert_eq!(msg.pid, pid.0, "[crash-smoke] crasher death msg pid mismatch");
            assert_eq!(
                msg.fault, synapse_abi::FAULT_SEGFAULT,
                "[crash-smoke] expected FAULT_SEGFAULT for SegFault, got {}",
                msg.fault
            );
            saw_segfault = true;
        } else {
            info!("[crash-smoke]   warn: crasher death msg recv returned None");
        }
    }
    assert!(saw_segfault, "[crash-smoke] no SegFault death msg received");

    // 内核侧 quota 拒绝计数断言（crasher mmap 64 页必触发）
    let qd = crate::proc_life::quota_denied_count();
    assert!(
        qd >= 1,
        "[crash-smoke] quota_denied_count = {qd}, expected >= 1"
    );
    info!("[crash-smoke]   ok: quota_denied_count = {qd} (>= 1)");

    // death delivery 计数（DEATH_LABEL 投递到 init death_endpoint）
    let dd = crate::proc_life::death_delivered_count();
    let dq = crate::proc_life::death_queued_count();
    info!(
        "[crash-smoke]   death stats: delivered={dd} queued={dq} dropped={}",
        crate::proc_life::death_dropped_count()
    );
    assert!(
        dd + dq >= 1,
        "[crash-smoke] no death msg delivered or queued"
    );

    // sys_reap 回收（再次 no-op，proc_life 终止路径已 release_resources → Zombie）
    let reap_ok = crate::proc_life::sys_reap(INIT_PID, pid).is_ok();
    assert!(reap_ok, "[crash-smoke] crasher reap failed");
    crate::kstate::k_rate_unregister(pid);
    info!("[crash-smoke]   ok: crasher reaped");

    // FR8 账本归零
    let used_post = with_page_frames(|a| a.used_frames()) as u64;
    let base = CRASH_BASE_USED.load(Ordering::SeqCst);
    assert_eq!(
        used_post, base,
        "[crash-smoke] FR8 leak: used {used_post} != baseline {base}"
    );
    info!("[crash-smoke]   ok: FR8 ledger back to baseline ({base})");

    info!("[crash-smoke] PASS");

    // QEMU exit 363
    unsafe {
        asm!(
            "mov dx, 0x502",
            "mov al, 0xB5",
            "out dx, al",
            options(nostack, preserves_flags),
        );
    }
    loop {
        unsafe { asm!("hlt", options(nostack, preserves_flags)) };
    }
}
