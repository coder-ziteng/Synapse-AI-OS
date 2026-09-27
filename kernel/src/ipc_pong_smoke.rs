//! P4-T7 Phase 2 ipc-pong smoke：ring3 send syscall 真机贯通（单 child 非阻塞路径）。
//!
//! ## 目标
//!
//! 把 "ring3 → kernel IPC send" 路径用 **真实子进程** 贯通，作为 Phase 2 收口的
//! 端到端证据：
//!
//! 1. spawn 一个子进程（pid>1），装载 `ipc-pong` ELF；
//! 2. ipc-pong 在 ring-3 执行：`abi_query` → `ipc_try_send(ep_cap=1, "init-ping\n", 9)`
//!    → `process_exit(0)`；
//! 3. 因无 receiver，`k_ipc_try_send` 走 Queued 路径 → 返 0 → IPC_TRY_SEND_COUNT++；
//! 4. `process_exit` → KERNEL_FRAME iretq 到 `ipc_pong_continuation`；
//! 5. 续体断言 `ipc_try_send_count() ≥ 1`（ring3 send 路径真跑过）+ 清理子资源
//!    + FR8 账本归零 + 链式到 `init::init_smoke()`。
//!
//! ## 与 kthread_ipc_smoke 的分工
//!
//! - `kthread_ipc_smoke`（P4-T7 Phase 1）：2 内核 kthread 阻塞 send/recv/reply 往返
//!   + cap transfer + 错误路径；**内核侧**完整覆盖；
//! - 本 smoke（Phase 2）：**ring3 子进程** 走 try_send syscall → 验证 syscall 分发
//!   层（cap resolve + user_mem_ok + Queued 入队）真机贯通；
//!
//! ## 不在本期（待 T14 idle thread 根治）
//!
//! 真双向并发两 ring3 进程 IPC（send/recv 阻塞）：当前 spawn 模型为串行
//! （init→child→init via KERNEL_FRAME），子进程阻塞时无可调度 kthread（init
//! kernel 不是 kthread，不进抢占队列）。需 idle thread（T14）+ 并发 spawn API。
//!
//! ## 在启动链中的位置
//!
//! `crash_continuation` → **ipc_pong_smoke** → `ipc_pong_continuation` →
//! `init::init_smoke()` → `init_continuation` → main.rs → QEMU exit 363。

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use log::info;
use x86_64::instructions::segmentation::{CS, Segment};

use synapse_cap::{ObjKind, Rights};
use synapse_proc::process::{Pid, SpawnParams, INIT_PID};
use synapse_ipc::AgentId;

use crate::page_frame::{free_frame, with_page_frames};
use crate::paging::AddressSpace;

// ---------------------------------------------------------------------------
// 内部状态（ipc_pong_continuation 读）
// ---------------------------------------------------------------------------

static PONG_CHILD_PID: AtomicU64 = AtomicU64::new(0);
static PONG_OLD_CR3: AtomicU64 = AtomicU64::new(0);
static PONG_OLD_RSP0: AtomicU64 = AtomicU64::new(0);
static PONG_CHILD_AS_PTR: AtomicU64 = AtomicU64::new(0);
static PONG_BASE_USED: AtomicU64 = AtomicU64::new(0);
static PONG_DEATH_CAP: AtomicU64 = AtomicU64::new(0);
static PONG_DONE: AtomicBool = AtomicBool::new(false);
/// 进 ring3 前的 ipc_try_send_count 基线——continuation 断言 post == pre + 1，
/// 证明计数增量确实来自 ring3 子进程的 try_send（而非 kthread_ipc_smoke 残留贡献）。
static PONG_SEND_COUNT_BASE: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// 主函数
// ---------------------------------------------------------------------------

/// P4-T7 Phase 2：spawn ipc-pong 子进程 → ring3 try_send → 续体验证 send 计数。
///
/// 由 `crash_continuation` 链式接力调用（不返回）；续体 `ipc_pong_continuation`
/// 清理后链式到 `init::init_smoke()`。
pub fn ipc_pong_smoke() -> ! {
    if PONG_DONE.swap(true, Ordering::SeqCst) {
        panic!("ipc_pong_smoke twice");
    }
    info!("[ipc-pong-smoke] start");

    // 记录基线 used_frames
    let base_used = with_page_frames(|a| a.used_frames()) as u64;
    PONG_BASE_USED.store(base_used, Ordering::SeqCst);

    // 取当前内核栈 RSP + RFLAGS
    let ksp: u64;
    let krflags: u64;
    unsafe {
        asm!("mov {}, rsp", out(reg) ksp, options(nomem, nostack, preserves_flags));
        asm!("pushf; pop {}", out(reg) krflags, options(nomem, nostack, preserves_flags));
    }

    // 保存 init 状态（continuation 恢复用）
    let old_cr3 = crate::paging::cr3_read();
    let old_rsp0 = crate::gdt::rsp0_stack_top();
    PONG_OLD_CR3.store(old_cr3, Ordering::SeqCst);
    PONG_OLD_RSP0.store(old_rsp0, Ordering::SeqCst);

    // 1. proc.spawn() → child_pid（MVP：default quota；agent_id = tick+100）
    let child_pid = do_pong_spawn().expect("[ipc-pong-smoke] proc.spawn failed");
    info!("[ipc-pong-smoke]   ok: proc.spawn → child_pid={}", child_pid.0);
    PONG_CHILD_PID.store(child_pid.0 as u64, Ordering::SeqCst);

    // 2. install kstack
    crate::proc_ext::install_kstack(child_pid)
        .expect("[ipc-pong-smoke] install_kstack failed");
    let child_kstack_top = crate::proc_ext::kstack_top_of(child_pid)
        .expect("[ipc-pong-smoke] kstack_top_of failed");
    info!("[ipc-pong-smoke]   ok: kstack installed, top={:#x}", child_kstack_top);

    // 3. create CapTable
    crate::kstate::k_create_cap_table(child_pid)
        .expect("[ipc-pong-smoke] k_create_cap_table failed");
    info!("[ipc-pong-smoke]   ok: CapTable created");

    // 4. 给子进程铸造 endpoint 根 cap → slot 1（ipc-pong 硬编码 ep_cap=1）。
    //    新 endpoint 对象（不复用 init/spawn/crash 的——那些已被 destroy 或语义不同）。
    let child_ep_obj = crate::kstate::k_alloc_object(ObjKind::Endpoint)
        .expect("[ipc-pong-smoke] alloc child endpoint failed");
    let child_ep_cap = crate::kstate::k_mint_root(
        child_pid,
        child_ep_obj,
        Rights::SEND | Rights::RECV | Rights::REPLY | Rights::GRANT,
    )
    .expect("[ipc-pong-smoke] mint child ep cap failed");
    assert_eq!(
        child_ep_cap, 1,
        "[ipc-pong-smoke] child ep cap must land in slot 1, got {}",
        child_ep_cap
    );
    info!(
        "[ipc-pong-smoke]   ok: child endpoint minted → cap slot {} (obj {}:{})",
        child_ep_cap, child_ep_obj.index, child_ep_obj.generation
    );

    // 5. load ipc-pong ELF into child AS
    let (mut child_as, entry, stack_top) = crate::elfload::load_elf_into_as("ipc-pong");
    PONG_CHILD_AS_PTR.store(&mut child_as as *mut AddressSpace as u64, Ordering::SeqCst);
    // syscall 分发层走 current_as_ptr() 取激活的用户 AS——必须指向子进程 AS，
    // 否则 ipc_try_send 的 user_mem_ok walk_flags 会读错页表 → E_INVALID_ADDR。
    crate::elfload::set_current_as_ptr(&mut child_as as *mut AddressSpace as u64);
    crate::proc_ext::set_user_as(child_pid, &mut child_as as *mut AddressSpace as u64);

    // 6. 武装 KERNEL_FRAME（ipc_pong_continuation 为 RIP）
    unsafe {
        crate::ring3::write_kernel_frame(
            ipc_pong_continuation as *const () as u64,
            ksp,
            krflags,
        );
    }
    info!(
        "[ipc-pong-smoke]   ok: KERNEL_FRAME armed (rip={:#x} rsp={:#x})",
        ipc_pong_continuation as *const () as u64,
        ksp
    );

    // 7. 切到子进程上下文
    crate::proc_ext::switch_to_process(child_pid);
    unsafe { crate::gdt::set_rsp0(child_kstack_top) };
    crate::proc_life::set_kernel_cr3(old_cr3);
    unsafe { child_as.activate() };
    info!(
        "[ipc-pong-smoke]   switched to child: pid={} cr3={:#x} rsp0={:#x}",
        child_pid.0, child_as.pml4_phys(), child_kstack_top
    );

    // 8. 记录 ring3 try_send 前的计数基线（continuation 断言 post == pre + 1）
    PONG_SEND_COUNT_BASE.store(crate::ipc::ipc_try_send_count() as u64, Ordering::SeqCst);

    // 9. iretq 到 ipc-pong ELF entry
    info!(
        "[ipc-pong-smoke]   iretq to ipc-pong: entry={:#x} rsp={:#x}",
        entry,
        stack_top - 8
    );
    unsafe { crate::ring3::enter_user_at(entry, stack_top - 8) }
}

// ---------------------------------------------------------------------------
// proc.spawn 最小封装（仅 ipc-pong smoke 用；不引 synapse_cap::Quota 依赖）
// ---------------------------------------------------------------------------

/// ipc-pong smoke 专用 spawn：铸造 death endpoint + 调 proc.spawn。
fn do_pong_spawn() -> Result<Pid, synapse_cap::CapError> {
    let agent_id_raw = crate::pit::tick_count().wrapping_add(100) as u32;
    let agent = AgentId(agent_id_raw);

    // 铸造 init 持有的 death endpoint（ipc_pong_continuation 读取 DeathMsg）
    let death_obj = crate::kstate::k_alloc_object(ObjKind::Endpoint)
        .expect("[ipc-pong] alloc death endpoint failed");
    let death_cap = crate::kstate::k_mint_root(
        INIT_PID,
        death_obj,
        Rights::RECV,
    )
    .expect("[ipc-pong] mint death cap for init failed");
    info!(
        "[ipc-pong-smoke]   ok: death_endpoint minted → cap slot {} (obj {}:{})",
        death_cap, death_obj.index, death_obj.generation
    );

    let params = SpawnParams {
        agent,
        quota: synapse_cap::DEFAULT_QUOTA,
        death_endpoint: death_cap,
    };

    let child_pid = crate::kstate::with_procs(|t| t.spawn(INIT_PID, params))?;
    PONG_DEATH_CAP.store(death_cap as u64, Ordering::SeqCst);
    Ok(child_pid)
}

// ============================================================================
// ring-0 continuation（ipc-pong process_exit 经 KERNEL_FRAME iretq 到此）
// ============================================================================

/// ipc-pong 续体：验证 ring3 send 计数 + 恢复 init 上下文 + 清理子资源 + 链式
/// 到 `init::init_smoke()`。
#[no_mangle]
extern "C" fn ipc_pong_continuation() -> ! {
    // 1. 确证 ring-0
    let cs = CS::get_reg();
    assert_eq!(cs.0 & 3, 0, "CS.RPL must be 0 in ipc_pong continuation");
    info!("[ipc-pong-smoke]   ok: continuation in ring-0");

    // 2. ★ Phase 2 核心证据：ipc_try_send_count == baseline + 1
    //    基线在 enter_user_at 前记录（kthread_ipc_smoke 的贡献已计入基线），
    //    增量 == 1 严格证明 ring3 子进程的 try_send 走通了 Queued 路径。
    let base_cnt = PONG_SEND_COUNT_BASE.load(Ordering::SeqCst);
    let cnt = crate::ipc::ipc_try_send_count() as u64;
    assert_eq!(
        cnt,
        base_cnt + 1,
        "[ipc-pong-smoke] ipc_try_send_count = {cnt}, expected baseline {base_cnt} + 1 \
         (ring3 send path not reached)"
    );
    info!(
        "[ipc-pong-smoke]   ok: ipc_try_send_count {} -> {} (ring3 send path confirmed)",
        base_cnt, cnt
    );

    // 3. 恢复 init 上下文
    let old_cr3 = PONG_OLD_CR3.load(Ordering::SeqCst);
    let old_rsp0 = PONG_OLD_RSP0.load(Ordering::SeqCst);
    unsafe { crate::paging::cr3_write(old_cr3) };
    unsafe { crate::gdt::set_rsp0(old_rsp0) };
    crate::proc_ext::switch_to_process(INIT_PID);
    info!(
        "[ipc-pong-smoke]   ok: restored init context: cr3={:#x} rsp0={:#x}",
        old_cr3, old_rsp0
    );

    // 4. 清理子进程资源
    let child_pid = Pid(PONG_CHILD_PID.load(Ordering::SeqCst) as u32);
    let as_ptr = PONG_CHILD_AS_PTR.load(Ordering::SeqCst) as *mut AddressSpace;

    // ipc-pong 走 process_exit（Exited）——非 fault terminate，此处负责清理 AS
    if !as_ptr.is_null() {
        let child_as = unsafe { &mut *as_ptr };
        // 清理 ipc-pong ELF 段（重走解析，确定性）
        let parsed = crate::elfload::extract_and_parse_named("ipc-pong");
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
        // drop AS（归还页表帧）
        unsafe { core::ptr::drop_in_place(as_ptr) };
        info!("[ipc-pong-smoke]   ok: child AS cleaned, freed {} frames", freed);
    }

    // 5. 清理 kstack
    crate::proc_ext::uninstall_kstack(child_pid);
    info!("[ipc-pong-smoke]   ok: kstack uninstalled");

    // 6. 清理 CapTable
    crate::kstate::k_destroy_cap_table(child_pid);
    info!("[ipc-pong-smoke]   ok: CapTable destroyed");

    // 7. 读 death msg（ipc-pong 正常退出 → fault = FAULT_NONE）
    let death_cap = PONG_DEATH_CAP.load(Ordering::SeqCst) as u8;
    if death_cap != 0 {
        if let Some(msg) = crate::proc_life::recv_death_msg(INIT_PID, death_cap) {
            info!(
                "[ipc-pong-smoke]   ok: death msg: pid={} exit_code={} fault={} (expect FAULT_NONE=0)",
                msg.pid, msg.exit_code, msg.fault
            );
            assert_eq!(
                msg.fault, synapse_abi::FAULT_NONE,
                "[ipc-pong-smoke] expected FAULT_NONE for clean process_exit, got {}",
                msg.fault
            );
            assert_eq!(
                msg.pid, child_pid.0,
                "[ipc-pong-smoke] death msg pid mismatch"
            );
        } else {
            info!("[ipc-pong-smoke]   warn: death msg recv returned None");
        }
    }

    // 8. proc reap + 频率注销
    let reap_ok = crate::proc_life::sys_reap(INIT_PID, child_pid).is_ok();
    if reap_ok {
        info!("[ipc-pong-smoke]   ok: proc reap succeeded");
    } else {
        info!("[ipc-pong-smoke]   warn: proc reap failed");
    }
    crate::kstate::k_rate_unregister(child_pid);

    // 9. FR8 账本归零
    let used_post = with_page_frames(|a| a.used_frames()) as u64;
    let base = PONG_BASE_USED.load(Ordering::SeqCst);
    assert_eq!(
        used_post, base,
        "[ipc-pong-smoke] FR8 leak: used {used_post} != baseline {base}"
    );
    info!("[ipc-pong-smoke]   ok: FR8 ledger back to baseline ({base})");

    info!("[ipc-pong-smoke] PASS — ring3 IPC send path confirmed");

    // 10. 链式到 init_smoke（P4-T10 Root Agent 启动链收口）
    //     ipc_pong_continuation 代替 main.rs 调用 init_smoke，因为整条
    //     crash → ipc_pong → init 链都在 continuation 栈上推进，main.rs
    //     的栈帧在 ring3_smoke 调用后已被冻结（return_continuation →
    //     elf_load_smoke 链式推进，不返回 main.rs 正常路径）。
    crate::init::init_smoke();

    // 11. init_smoke 返回（init_continuation iretq 回本调用点）。
    //     init_continuation 恢复的 RSP 由 init_smoke 捕获（= 本函数调用
    //     init_smoke 时的 RSP），经 init_continuation iretq 后回到此处。
    //     本函数是 `-> !`，必须在此发出 QEMU exit（main.rs 的同款 out 0xB5
    //     是不可达死代码——return_continuation 链式到 elf_load_smoke 后不
    //     返回 main.rs）。
    unsafe {
        asm!(
            "mov dx, 0x502",
            "mov al, 0xB5",
            "out dx, al",
            options(nostack, preserves_flags),
        );
    }
    loop {
        unsafe { asm!("hlt", options(nostack, preserves_flags)); }
    }
}
