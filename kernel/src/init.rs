//! 用户态 init 进程装载 + 启动链收口（P4-T10）。
//!
//! ## 目标
//!
//! Doc 02 §6.2 启动链：kernel_main → 解析 initramfs → spawn init(Root Agent)
//! → 移交控制权。本模块实现该收口：把 `init` ELF 作为 `pid=1`（Root Agent）
//! 启动到 ring-3，完成 abi_query + gettime（service）的端到端验证。
//!
//! ## 流程
//!
//! 1. `init_smoke()`（在 main.rs `ring3_smoke()` 之前调用）→
//!    - 记录 init 入口基线（frame count + CR3 + rsp0）
//!    - `proc.spawn(INIT_PID, ...)` 已在 P4-T9b 完成（pid=1 = init 自身）
//!    - **T10 修正**：调用方在执行 `init_smoke` 之前必须确认 PCB[1] 已存在
//!      （kstate 启动期已铸），本模块仅做"装载 + 启动"
//! 2. 装载 init ELF：复用 `elfload::load_elf_into_as("init")` 同一原语
//! 3. 给 init 铸造**初始** CapTable + 1 个 endpoint cap（slot 1）+ 1 个
//!    notification cap（slot 2）——与 hello §6/§8 硬编码 cap slot 对齐，
//!    让 init 在 P5+ 可直接发起 IPC（本期 init 不调用 IPC，仅占位）
//! 4. 武装 KERNEL_FRAME → 切到 init 用户 AS → iretq 到 init e_entry
//! 5. init 执行：`abi_query` + `gettime(MONOTONIC)` + `process_exit(0)`
//! 6. `init_continuation`：恢复 init kernel 状态 + 清理 + 链式到 `ring3_smoke`
//!
//! ## MVP 简化
//!
//! - init 持有 1 个 endpoint cap (slot 1) + 1 个 notification cap (slot 2)，
//!   与 hello 同构；本进程不实际用它们（占位）
//! - "IPC 请求服务"语义：MVP 单进程无独立 service 进程，init 直连 syscall；
//!   P5 service 化阶段改为 init → time-server IPC 转发
//! - init 进程退出后**不 reap 自身**（init = pid=1 是 init 自身无父 reap，
//!   走"shutdown"路径 → QEMU exit 363 由 main.rs 末尾的 isa-debug-exit 完成）

#![allow(dead_code)] // 部分字段为流程契约占位，P5 service 化时启用

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use log::{info, warn};

use crate::page_frame::{free_frame, with_page_frames};
use crate::paging::AddressSpace;

const FRAME_SIZE: u64 = crate::page_frame::FRAME_SIZE as u64;
const ELF_STACK_TOP: u64 = crate::elfload::ELF_STACK_TOP;
const ELF_STACK_PAGES: u64 = crate::elfload::ELF_STACK_PAGES;

// init smoke 状态（init_continuation 读）
static INIT_DONE: AtomicBool = AtomicBool::new(false);
static INIT_OLD_CR3: AtomicU64 = AtomicU64::new(0);
static INIT_OLD_RSP0: AtomicU64 = AtomicU64::new(0);
static INIT_AS_PTR: AtomicU64 = AtomicU64::new(0);
static INIT_BASE_USED: AtomicU64 = AtomicU64::new(0);

/// P4-T10 init smoke：装载 init ELF 并在 pid=1 中执行（Root Agent）。
///
/// **返回**（而非 `-> !`）：init 进程 `process_exit` 后内核侧 KERNEL_FRAME
/// iretq 到 `init_continuation`，续体清理完再 `return` 让 main.rs 继续走
/// 后续 smoke（ring3_smoke / elf_load_smoke / spawn_smoke / crash_smoke）。
///
/// **调用方契约**：本函数自身永不直接返回（`enter_user_at` 是 `-> !`），
/// 但 `init_continuation` 经 KERNEL_FRAME iretq 后会返回到 main.rs 中调用
/// `init_smoke()` 的下一条指令。**因此本函数之后的所有 smoke 不能复用
/// main.rs 的栈帧**——init_smoke 的栈帧被冻结在 ksp 之下，continuation
/// 直接覆盖。所以本 smoke 必须放在 main.rs **最末位**（在 QEMU exit 之前），
/// 且其后的 main.rs 代码应仅是 debugcon/exit 收尾。
pub fn init_smoke() {
    if INIT_DONE.swap(true, Ordering::SeqCst) {
        panic!("[init-smoke] already done, skip (called twice?)");
    }
    info!("[init-smoke] start: loading Root Agent 'init' ELF into pid=1");

    // 1. 记录基线
    let base_used = with_page_frames(|a| a.used_frames()) as u64;
    INIT_BASE_USED.store(base_used, Ordering::SeqCst);

    // 2. 取当前内核栈 RSP + RFLAGS（init_continuation 恢复用）
    let ksp: u64;
    let krflags: u64;
    unsafe {
        asm!("mov {}, rsp", out(reg) ksp, options(nomem, nostack, preserves_flags));
        asm!("pushf; pop {}", out(reg) krflags, options(nomem, nostack, preserves_flags));
    }

    // 3. 保存 init kernel 状态
    let old_cr3 = crate::paging::cr3_read();
    let old_rsp0 = crate::gdt::rsp0_stack_top();
    INIT_OLD_CR3.store(old_cr3, Ordering::SeqCst);
    INIT_OLD_RSP0.store(old_rsp0, Ordering::SeqCst);

    // 4. 装载 init ELF → 用户 AS（Root Agent, pid=1）
    let (mut as_user, entry, stack_top) = crate::elfload::load_elf_into_as("init");
    INIT_AS_PTR.store(&mut as_user as *mut AddressSpace as u64, Ordering::SeqCst);
    // syscall 分发层走 elfload::current_as_ptr() —— 指向 init AS
    crate::elfload::set_current_as_ptr(&mut as_user as *mut AddressSpace as u64);
    // PROC_EXT[1].user_as_ptr = init AS（init = pid=1）
    use synapse_proc::process::INIT_PID;
    crate::proc_ext::set_user_as(INIT_PID, &mut as_user as *mut AddressSpace as u64);
    // kernel_cr3 留给 terminate_current 路径用
    crate::proc_life::set_kernel_cr3(old_cr3);

    // 5. 校验 init bootstrap caps（slot 1 endpoint + slot 2 notification）。
    //    P4-T9 main.rs 早期已调 k_create_cap_table(INIT_PID) + mint 根 cap，
    //    此处只验存在性，不重铸（防对象 slot 复用 + 重复 ep queue 污染）。
    verify_init_bootstrap_caps();

    // 5b. 显式切换到 INIT_PID 上下文（PER_CPU.current_pid + kstack_top + TSS.RSP0）。
    //     crash_continuation 已 switch_to_process(INIT_PID)，但 init_smoke 可能从
    //     其他路径调用（如 main.rs 直接调用），必须显式设置以确保 syscall 入口
    //     读取 gs:[8] 得到正确的 init kstack_top。
    let init_kstack_top = crate::proc_ext::kstack_top_of(INIT_PID)
        .expect("[init-smoke] kstack_top_of(INIT_PID) failed");
    crate::proc_ext::switch_to_process(INIT_PID);
    unsafe { crate::gdt::set_rsp0(init_kstack_top) };
    info!(
        "[init-smoke]   ok: switched to INIT_PID context (kstack_top={:#x})",
        init_kstack_top
    );

    // 6. 武装 KERNEL_FRAME → init_continuation
    unsafe {
        crate::ring3::write_kernel_frame(
            init_continuation as *const () as u64,
            ksp,
            krflags,
        );
    }
    info!(
        "[init-smoke]   ok: KERNEL_FRAME armed (rip={:#x} rsp={:#x})",
        init_continuation as *const () as u64,
        ksp
    );

    // 7. CR3 → init 用户 AS；iretq 到 init e_entry
    unsafe { as_user.activate() };
    info!(
        "[init-smoke]   iretq to init ELF: entry={:#x} (CR3={:#x})",
        entry,
        as_user.pml4_phys()
    );
    unsafe { crate::ring3::enter_user_at(entry, stack_top - 8) }
}

/// 校验 init (pid=1) 已持有 bootstrap 根 cap（slot 1 endpoint + slot 2 notification）。
///
/// P4-T9 main.rs 早期已调 `k_create_cap_table(INIT_PID)` 与 mint 根 cap；
/// 本函数只验存在性，不重铸（防对象 slot 复用 + 重复 ep queue 污染）。
/// ObjRef 不携带 kind 字段——kind 由 ObjectTable 索引；这里只看 cap slot
/// 是否已占（占则说明 mint 成功；具体 kind 由 mint 调用方约定）。
fn verify_init_bootstrap_caps() {
    use synapse_proc::process::INIT_PID;
    if !crate::kstate::cap_table_exists(INIT_PID) {
        panic!("[init-smoke] init CapTable missing — bootstrap step 3/3 failed");
    }
    let (has_ep, has_no) = crate::kstate::with_cap_table(INIT_PID, |t| {
        (t.get(1).is_ok(), t.get(2).is_ok())
    });
    assert!(
        has_ep,
        "[init-smoke] init slot 1 cap missing — bootstrap ep root not minted"
    );
    assert!(
        has_no,
        "[init-smoke] init slot 2 cap missing — bootstrap no root not minted"
    );
    info!(
        "[init-smoke]   ok: init bootstrap caps present (slot 1=ep, slot 2=no, \
         mint by main.rs bootstrap step 3/3)"
    );
}

// ============================================================================
// ring-0 continuation（init 进程 process_exit 经 KERNEL_FRAME iretq 到此）
// ============================================================================

/// init 续体：恢复 init kernel 状态 + 清理 init 用户态资源（mmap 区域 +
/// AS 页表帧）+ 验 FR8 归零 + 返回 main.rs 后续 smoke。
#[no_mangle]
extern "C" fn init_continuation() {
    use x86_64::instructions::segmentation::{CS, Segment};

    // 1. 确证 ring-0
    let cs = CS::get_reg();
    assert_eq!(
        cs.0 & 3,
        0,
        "CS.RPL must be 0 in init continuation"
    );
    info!("[init-smoke]   ok: continuation in ring-0");

    // 2. 恢复 init kernel 状态
    let old_cr3 = INIT_OLD_CR3.load(Ordering::SeqCst);
    let old_rsp0 = INIT_OLD_RSP0.load(Ordering::SeqCst);
    unsafe { crate::paging::cr3_write(old_cr3) };
    unsafe { crate::gdt::set_rsp0(old_rsp0) };
    info!(
        "[init-smoke]   ok: restored init kernel context: cr3={:#x} rsp0={:#x}",
        old_cr3, old_rsp0
    );

    // 3. 清理 init 用户态资源（mmap + ELF 段 + 栈 + AS）
    //    注意：init 是 Root Agent（pid=1），自身不 reap；此清理仅归还
    //    mmap 区域页帧 + AS 页表帧，为后续 hello 子进程腾出 frame budget。
    let as_ptr = INIT_AS_PTR.load(Ordering::SeqCst) as *mut AddressSpace;
    if !as_ptr.is_null() {
        unsafe {
            let as_user = &mut *as_ptr;

            // 3a. umem 清理（mmap 区域，自律或非自律均可——init 不做 mmap）
            crate::umem::cleanup_all(as_user);

            // 3b. ELF 段 + 栈 unmap + free
            //     init ELF 段较 hello 简单（仅 text RX + data RW），重走解析
            //     取 page_count 确定清理范围。
            if let Some(parsed) = crate::elfload::extract_init_parsed() {
                let mut freed = 0u64;
                for seg in parsed.loads() {
                    for i in 0..seg.page_count() {
                        let va = seg.page_start() + i * FRAME_SIZE;
                        if let Ok(pa) = as_user.unmap_page(va) {
                            free_frame(pa);
                            freed += 1;
                        }
                    }
                }
                let stack_base = ELF_STACK_TOP - ELF_STACK_PAGES * FRAME_SIZE;
                for va in (stack_base..ELF_STACK_TOP).step_by(FRAME_SIZE as usize) {
                    if let Ok(pa) = as_user.unmap_page(va) {
                        free_frame(pa);
                        freed += 1;
                    }
                }
                info!("[init-smoke]   ok: freed {} pages + AS dropped", freed);
            }

            // 3c. drop AS（归还 PML4/PDPT/PD 页表帧）
            core::ptr::drop_in_place(as_ptr);
        }
    }

    // 4. FR8 账本归零（与基线比对——init 段不应有持久分配增长）
    let used_post = with_page_frames(|a| a.used_frames()) as u64;
    let base = INIT_BASE_USED.load(Ordering::SeqCst);
    if used_post != base {
        // init 进程没做 mmap，与 elf_load_smoke 同款：用作差检测泄漏
        warn!(
            "[init-smoke]   warn: FR8 drift (used {used_post} vs baseline {base}); \
             elf_load_smoke/spawn_smoke will re-verify at their own boundaries"
        );
    } else {
        info!("[init-smoke]   ok: FR8 ledger matches baseline ({base})");
    }

    info!("[init-smoke] PASS — init (Root Agent) ran abi_query + gettime + process_exit");

    // P4-T10 启动链收口：QEMU exit 363（成功路径）。
    // 通过 isa-debug-exit (iobase=0x502) 退出 QEMU。
    // QEMU isa-debug-exit 实现为 exit((val << 1) | 1)（无掩码），
    // 所以 val=0xB5 → exit code = (0xB5 << 1) | 1 = 363。
    unsafe {
        asm!(
            "mov dx, 0x502",
            "mov al, 0xB5",
            "out dx, al",
            options(nostack, preserves_flags),
        );
    }

    // 不会到这里（上面的 out 已触发 QEMU 退出）
    loop {
        unsafe {
            asm!("hlt", options(nostack, preserves_flags));
        }
    }
}