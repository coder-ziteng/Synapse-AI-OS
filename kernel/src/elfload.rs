//! 静态 ELF 加载器 + 真机 smoke（P4-T5）。
//!
//! ## 端到端流程（task.json P4-T5 verify）
//!
//! 1. [`crate::initrd::bytes`] 取 initramfs（stage2 连续加载在内核镜像后，
//!    0x20100 引导记录）→ `synapse_elf::cpio::find("hello")` 提取 ELF 字节；
//! 2. `synapse_elf::parse` 完整校验：ET_EXEC / x86-64 / PT_LOAD 文件边界 /
//!    页同余 / W^X / 用户窗口 [1GB, 2GB) / 段重叠 / entry 落点（Doc 02 §2）；
//! 3. 新建用户 [`AddressSpace`]：每个 PT_LOAD 页 alloc_frame → 清零（bss +
//!    防信息泄漏）→ 拷贝文件交集 → map_page（PT_USER | W? | NX?）；
//!    另映射 [`ELF_STACK_PAGES`] 页 RW+NX 用户栈；
//! 4. 复用 ring3 的 KERNEL_FRAME 接力机制：arm [`elf_continuation`] →
//!    iretq 进入 e_entry（ring-3）；
//! 5. hello 执行：`abi_query`(#18) 往返 → `process_exit`(#21) → handler
//!    iretq 回 [`elf_continuation`]（ring-0）；
//! 6. 断言 abi_query 计数 ≥ 1（= 用户 ELF 真实跑过 syscall）+ CR3 还原 +
//!    逐页 unmap/free + FR8 账本归零 → isa-debug-exit 363。
//!
//! ## 用户栈入口约定
//!
//! rustc 生成的 `extern "C" fn _start` 按"函数入口 rsp%16==8"（call 语义）
//! 做栈对齐假设；iretq 直达没有 call 压栈，故 RSP 取 `ELF_STACK_TOP - 8`
//! （TOP 16 对齐 → 入口 rsp%16==8，与 SysV 进程入口约定一致）。
//!
//! ## 与 P4-T9 的关系
//!
//! 本模块是 **smoke 级**加载器：无进程表 / 无 CapRef 委托 / fault 即 panic
//! （355 出口）。T9 在此之上接 process_spawn（仅 init 可 spawn）+ 崩溃处理
//! （FaultKind 归因 + death notification + reap）。

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use log::info;
use x86_64::instructions::segmentation::{CS, Segment};

use synapse_elf::cpio;
use synapse_elf::{LoadSegment, ParsedElf};

use crate::page_frame::{alloc_frame, free_frame, with_page_frames, FRAME_SIZE};
use crate::paging::{
    AddressSpace, PT_NX, PT_USER, PT_WRITABLE, USER_REGION_END, USER_REGION_START,
};

/// 用户栈顶 VA（hello 栈需求极小；16B 对齐，入口 RSP = TOP - 8）。
const ELF_STACK_TOP: u64 = 0x4080_0000;
/// 用户栈页数（16KB）。
const ELF_STACK_PAGES: u64 = 4;
/// initramfs 内目标文件名（xtask `initramfs.rs` 打包约定）。
const HELLO_NAME: &str = "hello";

const FRAME: u64 = FRAME_SIZE as u64;

// smoke 内部状态（elf_continuation 读）
static OLD_CR3: AtomicU64 = AtomicU64::new(0);
static AS_PTR: AtomicU64 = AtomicU64::new(0);
static BASE_USED: AtomicU64 = AtomicU64::new(0);
static SMOKE_DONE: AtomicBool = AtomicBool::new(false);

// ============================================================================
// 提取 + 解析
// ============================================================================

/// 从 initramfs 提取并解析 hello ELF（smoke 与 continuation 清理各调一次，
/// 输入驻留内存不变 → 结果确定）。
fn extract_and_parse() -> ParsedElf<'static> {
    let initrd = crate::initrd::bytes().expect("[elf-smoke] initramfs missing (0x20100 size=0)");
    let elf_bytes = cpio::find(initrd, HELLO_NAME)
        .expect("[elf-smoke] cpio malformed")
        .unwrap_or_else(|| panic!("[elf-smoke] '{HELLO_NAME}' not in initramfs"));
    let cfg = synapse_elf::LoadConfig {
        user_base: USER_REGION_START,
        user_limit: USER_REGION_END,
    };
    synapse_elf::parse(elf_bytes, &cfg).expect("[elf-smoke] ELF rejected by parser")
}

// ============================================================================
// 装载（映射 + 拷贝）
// ============================================================================

/// 段权限 → 页表叶 flags（W^X：可写段强制 NX）。
fn leaf_flags(seg: &LoadSegment) -> u64 {
    let mut flags = PT_USER;
    if seg.writable() {
        flags |= PT_WRITABLE;
    }
    if !seg.exec() {
        flags |= PT_NX;
    }
    flags
}

/// 把一个 PT_LOAD 段装入用户 AS：逐页 alloc → 清零 → 拷贝文件交集 → map。
///
/// 清零覆盖两类字节：页内 vaddr 之前的头部空洞、memsz>filesz 的 bss 尾部
/// （ELF 语义 + 防物理页残留信息泄漏，Doc 02 §2.3）。
fn map_segment(as_user: &mut AddressSpace, seg: &LoadSegment) {
    let flags = leaf_flags(seg);
    for i in 0..seg.page_count() {
        let va = seg.page_start() + i * FRAME;
        let frame = alloc_frame().expect("[elf-smoke] frame for PT_LOAD");
        // SAFETY: frame 为刚分配的独占物理页（内核 VA==PA 恒等映射）。
        unsafe {
            core::ptr::write_bytes(frame as *mut u8, 0, FRAME_SIZE);
            let data_start = va.max(seg.vaddr);
            let data_end = (va + FRAME).min(seg.vaddr + seg.filesz);
            if data_start < data_end {
                let src = seg
                    .file_data
                    .as_ptr()
                    .add((data_start - seg.vaddr) as usize);
                core::ptr::copy_nonoverlapping(
                    src,
                    (frame + (data_start - va)) as *mut u8,
                    (data_end - data_start) as usize,
                );
            }
        }
        as_user
            .map_page(va, frame, flags)
            .unwrap_or_else(|e| panic!("[elf-smoke] map {va:#x}: {e:?}"));
    }
    info!(
        "[elf-smoke]   ok: PT_LOAD [{:#x}..{:#x}) filesz={:#x} memsz={:#x} flags={:#x} ({} pages)",
        seg.vaddr,
        seg.vaddr + seg.memsz,
        seg.filesz,
        seg.memsz,
        seg.flags,
        seg.page_count()
    );
}

/// 映射用户栈（RW + NX，零页）。
fn map_stack(as_user: &mut AddressSpace) {
    let base = ELF_STACK_TOP - ELF_STACK_PAGES * FRAME;
    for va in (base..ELF_STACK_TOP).step_by(FRAME as usize) {
        let frame = alloc_frame().expect("[elf-smoke] frame for stack");
        // SAFETY: 独占新帧，恒等映射清零。
        unsafe { core::ptr::write_bytes(frame as *mut u8, 0, FRAME_SIZE) };
        as_user
            .map_page(va, frame, PT_USER | PT_WRITABLE | PT_NX)
            .unwrap_or_else(|e| panic!("[elf-smoke] map stack {va:#x}: {e:?}"));
    }
    info!(
        "[elf-smoke]   ok: stack [{} pages] base={:#x} top={:#x} (entry rsp={:#x})",
        ELF_STACK_PAGES,
        base,
        ELF_STACK_TOP,
        ELF_STACK_TOP - 8
    );
}

// ============================================================================
// smoke 主流程
// ============================================================================

/// P4-T5 真机 smoke：由 `ring3::return_continuation` 链式接力调用（不返回）。
pub fn elf_load_smoke() -> ! {
    if SMOKE_DONE.swap(true, Ordering::SeqCst) {
        panic!("elf_load_smoke twice");
    }
    info!("[elf-smoke] start");
    BASE_USED.store(
        with_page_frames(|a| a.used_frames()) as u64,
        Ordering::SeqCst,
    );

    // 取 smoke 入口 RSP + RFLAGS（elf_continuation 经 KERNEL_FRAME 还原）
    let ksp: u64;
    let krflags: u64;
    unsafe {
        asm!("mov {}, rsp", out(reg) ksp, options(nomem, nostack, preserves_flags));
        asm!("pushf; pop {}", out(reg) krflags, options(nomem, nostack, preserves_flags));
    }

    // 1. initramfs → cpio → ELF → 完整校验
    let parsed = extract_and_parse();
    info!(
        "[elf-smoke]   ok: extracted '{}' + ELF parsed: entry={:#x}, {} PT_LOAD",
        HELLO_NAME,
        parsed.entry,
        parsed.segment_count
    );

    // 2. 新建用户 AS + 装段 + 栈
    let mut as_user = AddressSpace::new().expect("[elf-smoke] frames for AS");
    for seg in parsed.loads() {
        map_segment(&mut as_user, seg);
    }
    map_stack(&mut as_user);

    // 3. 记录 continuation 所需状态
    OLD_CR3.store(crate::paging::cr3_read(), Ordering::SeqCst);
    AS_PTR.store(&mut as_user as *mut AddressSpace as u64, Ordering::SeqCst);

    // 4. 武装 KERNEL_FRAME（process_exit #21 → iretq 回 elf_continuation）
    // SAFETY: elf_continuation 是 extern "C" -> ! 的 ring-0 入口；ksp/krflags
    // 为本函数刚捕获的可恢复上下文。
    unsafe {
        crate::ring3::write_kernel_frame(
            elf_continuation as *const () as u64,
            ksp,
            krflags,
        )
    };
    info!(
        "[elf-smoke]   ok: KERNEL_FRAME armed (rip={:#x} rsp={:#x})",
        elf_continuation as *const () as u64,
        ksp
    );

    // 5. CR3 → 用户 AS；iretq 进入 e_entry（ring-3）
    // SAFETY: as_user 页表完整；AS_PTR 已记录（continuation 负责 drop）。
    unsafe { as_user.activate() };
    info!(
        "[elf-smoke]   iretq to user ELF: entry={:#x} (CR3={:#x})",
        parsed.entry,
        as_user.pml4_phys()
    );
    // SAFETY: entry/栈页均已映射且权限正确（parse 校验 entry ∈ RX 段）。
    unsafe { crate::ring3::enter_user_at(parsed.entry, ELF_STACK_TOP - 8) }
}

// ============================================================================
// ring-0 continuation（hello 的 process_exit 经 KERNEL_FRAME iretq 到此）
// ============================================================================

/// CPU 状态：CPL=0、RSP=ksp（smoke 入口）、RFLAGS 已还原；其余寄存器未定义。
#[no_mangle]
extern "C" fn elf_continuation() -> ! {
    // 1. 确证 ring-0
    let cs = CS::get_reg();
    assert_eq!(cs.0 & 3, 0, "CS.RPL must be 0 in elf continuation");

    // 2. hello 真实执行过的内核侧证据：abi_query 分发计数 ≥ 1
    let n = crate::syscall::abi_query_count();
    assert!(n >= 1, "[elf-smoke] abi_query count = {n}, expected >= 1");
    info!("[elf-smoke]   ok: hello ran in ring-3, abi_query count = {n}");

    // 3. CR3 → kernel AS
    let old_cr3 = OLD_CR3.load(Ordering::SeqCst);
    unsafe { crate::paging::cr3_write(old_cr3) };
    info!("[elf-smoke]   ok: CR3 -> kernel ({old_cr3:#x})");

    // 4. 清理：重走解析（输入驻留不变 → 确定性），逐页 unmap + free；drop AS
    let parsed = extract_and_parse();
    let as_ptr = AS_PTR.load(Ordering::SeqCst) as *mut AddressSpace;
    let mut freed = 0u64;
    // SAFETY: AS_PTR 由 smoke 写入且未被 drop；本函数是唯一后续使用者。
    unsafe {
        for seg in parsed.loads() {
            let flags = leaf_flags(seg);
            let _ = flags;
            for i in 0..seg.page_count() {
                let va = seg.page_start() + i * FRAME;
                let pa = (*as_ptr).unmap_page(va).expect("[elf-smoke] unmap PT_LOAD");
                free_frame(pa);
                freed += 1;
            }
        }
        let stack_base = ELF_STACK_TOP - ELF_STACK_PAGES * FRAME;
        for va in (stack_base..ELF_STACK_TOP).step_by(FRAME as usize) {
            let pa = (*as_ptr).unmap_page(va).expect("[elf-smoke] unmap stack");
            free_frame(pa);
            freed += 1;
        }
        drop(core::ptr::read(as_ptr)); // AS drop：归还页表帧
    }
    info!("[elf-smoke]   ok: unmapped+freed {freed} pages + user AS dropped");

    // 5. FR8 账本归零（回到 smoke 入口基线）
    let used_post = with_page_frames(|a| a.used_frames()) as u64;
    let base = BASE_USED.load(Ordering::SeqCst);
    assert_eq!(
        used_post, base,
        "[elf-smoke] FR8 leak: used {used_post} != baseline {base}"
    );
    info!("[elf-smoke]   ok: FR8 ledger back to baseline ({base})");

    info!("[elf-smoke] PASS");

    // 6. 主动退出 QEMU：isa-debug-exit (0x502) 写 0xB5 → exit 363（成功出口）。
    unsafe {
        asm!(
            "mov dx, 0x502",
            "mov al, 0xB5",
            "out dx, al",
            options(nostack, preserves_flags),
        );
    }

    // 不应到这里（isa-debug-exit 已触发 QEMU 退出）
    loop {
        unsafe { asm!("hlt", options(nostack, preserves_flags)) };
    }
}
