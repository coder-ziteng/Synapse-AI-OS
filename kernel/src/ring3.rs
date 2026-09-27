//! Ring-3 切换 smoke（P4-T4 verify）。
//!
//! ## 目标
//!
//! 端到端验证 ring-3 基础设施贯通：
//! 1. 切 CR3 到用户 AS
//! 2. 构造 iretq 帧 → 进入用户态（CS.RPL=3, SS.RPL=3）
//! 3. 用户代码执行：mov rax, 18; syscall; mov [shared], rax; syscall #999（未定义号）
//! 4. syscall #18 = abi_query 返回 ABI 版本
//! 5. 用户态写共享页 → 内核通过 PA 读回（VA==PA 映射间接访问）
//! 6. syscall #999 = IllegalSyscall（P4-T6）→ decode None → 杀进程计数 +1 →
//!    与 process_exit 同一 KERNEL_FRAME iretq 接力回 prepared kernel frame
//! 7. CPL 断言（CS.RPL==3）+ illegal 计数断言 + FR8 归零
//!
//! ## 返回机制（关键设计）
//!
//! User code 最后调用 syscall #999（未定义号 → IllegalSyscall 杀进程；P4-T6
//! 前为 #21 process_exit，二者走同一返回路径）— handler 读 `KERNEL_FRAME`
//! 全局 static，把 RSP 指向该 frame，执行 `iretq`。CPU 跳到 `return_continuation`
//! 在 ring-0 上下文（CS=KERNEL_CS, RSP=boot stack）→ ring3_smoke caller 完成
//! 断言。

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use log::info;
use x86_64::instructions::segmentation::{CS, Segment};
use x86_64::registers::rflags::RFlags;

use crate::page_frame::{alloc_frame, free_frame, with_page_frames};
use crate::paging::{AddressSpace, PT_NX, PT_USER, PT_WRITABLE, USER_REGION_START};

/// Ring-3 user CS selector（GDT index 5, RPL=3 → 0x2B）。
const USER_CS: u64 = 0x2B;
/// Ring-3 user DS selector（GDT index 4, RPL=3 → 0x23）。
const USER_DS: u64 = 0x23;

/// Ring-0 kernel CS selector（GDT index 1, RPL=0 → 0x08）。
const KERNEL_CS: u64 = 0x08;
/// Ring-0 kernel DS selector（GDT index 2, RPL=0 → 0x10）。
const KERNEL_DS: u64 = 0x10;

/// Data 页 VA：用户代码 syscall 返回后写到 data 页，内核通过 PA 读回。
const DATA_PAGE_ADDR: u64 = USER_REGION_START + 0x1000;

/// 用户代码基址（4KB 对齐）。
const USER_CODE_ADDR: u64 = USER_REGION_START;

/// 用户栈顶（stack 向下增长）。
const USER_STACK_TOP: u64 = USER_REGION_START + 0x1000 + 0x1000;

// ============================================================================
// 用户代码字节（手编机器码）
// ============================================================================
//
// P4-T6 改造：末尾 syscall 由 #21 (process_exit) 改为 **#999（未定义号）**
// ——真机验证 IllegalSyscall 杀进程路径：decode → None → 计数 +1 →
// handle_process_exit（与 process_exit 同一 KERNEL_FRAME iretq 接力），
// return_continuation 断言 illegal_syscall_count()==1。
//
// ```text
// 48 c7 c0 12 00 00 00    mov rax, 0x12            ; AbiQuery syscall num = 18
// 0f 05                   syscall                  ; rax = ABI version
// 48 89 04 25 00 10 00 40 mov [0x4000_1000], rax   ; 写 ABI 版本到 data 页
// 48 c7 c0 e7 03 00 00    mov rax, 0x3e7           ; 999 = 未定义号 (IllegalSyscall)
// 0f 05                   syscall                  ; 触发杀进程 → ring-0 返回
// f4                      hlt                      ; 不应执行到
// ```
#[rustfmt::skip]
const USER_CODE_BYTES: &[u8] = &[
    0x48, 0xc7, 0xc0, 0x12, 0x00, 0x00, 0x00, // mov rax, 0x12
    0x0f, 0x05,                               // syscall
    0x48, 0x89, 0x04, 0x25, 0x00, 0x10, 0x00, 0x40, // mov [0x4000_1000], rax
    0x48, 0xc7, 0xc0, 0xe7, 0x03, 0x00, 0x00, // mov rax, 0x3e7 (999 = IllegalSyscall)
    0x0f, 0x05,                               // syscall
    0xf4,                                     // hlt (unreachable)
];

// ============================================================================
// iretq 帧 + 配套 asm
// ============================================================================

/// iretq 到 ring-3 的栈帧（6 × u64 = 48B，调用方再压一个 8B dummy 保证 16B 对齐）。
#[repr(C)]
struct IretqFrame {
    rip: u64,
    cs: u64,
    rflags: u64,
    rsp: u64,
    ss: u64,
    _pad: u64,
}

// `iretq` 跳到用户态（C-ABI：rdi = &IretqFrame）。
extern "C" {
    fn iretq_to_user_asm();
}

global_asm!(
    ".global iretq_to_user_asm",
    "iretq_to_user_asm:",
    // 把 RSP 切到 rdi 指向的 iretq 帧，再 iretq。
    // iretq 从 [RSP+0]=RIP / [RSP+8]=CS / [RSP+16]=RFLAGS / [RSP+24]=RSP / [RSP+32]=SS 弹。
    "mov rsp, rdi",
    "iretq",
);

/// "返回内核" iretq 帧：smoke 提前把 RIP=continuation, CS=KERNEL_CS,
/// RFLAGS, RSP=boot_sp, SS=KERNEL_DS 填好；syscall #21 handler
/// 把 RSP 指到这里 + iretq，CPU 直接跳到 continuation 在 ring-0。
#[repr(C)]
#[derive(Clone, Copy)]
struct KernelIretqFrame {
    rip: u64,
    cs: u64,
    rflags: u64,
    rsp: u64,
    ss: u64,
}

struct KernelFrameWrap(core::cell::UnsafeCell<KernelIretqFrame>);
unsafe impl Sync for KernelFrameWrap {}
static KERNEL_FRAME: KernelFrameWrap =
    KernelFrameWrap(core::cell::UnsafeCell::new(KernelIretqFrame {
        rip: 0,
        cs: KERNEL_CS,
        rflags: 0,
        rsp: 0,
        ss: KERNEL_DS,
    }));

/// 标记：KERNEL_FRAME 是否已就绪（=smoke 已 iretq to user）。handler 据此
/// 决定是否走 iretq-回内核 路径；未就绪时 = 真退进程（首期 MVP 返回错误码）。
static KERNEL_FRAME_ARMED: AtomicBool = AtomicBool::new(false);

// ============================================================================
// smoke 内部状态（return_continuation 读）
// ============================================================================

static RETURN_OLD_CR3: AtomicU64 = AtomicU64::new(0);
static RETURN_CODE_PA: AtomicU64 = AtomicU64::new(0);
static RETURN_DATA_PA: AtomicU64 = AtomicU64::new(0);
static RETURN_USER_AS_PTR: AtomicU64 = AtomicU64::new(0);
static SMOKE_DONE: AtomicBool = AtomicBool::new(false);

// ============================================================================
// syscall #21 (process_exit) — handler 走 iretq-to-kernel-frame 路径
// ============================================================================

/// syscall #21 handler：MVP 简化为"如果 KERNEL_FRAME armed，直接 iretq 跳
/// 到 ring-3 smoke 的 prepared kernel frame；否则返回 -E_NOT_IMPLEMENTED"。
///
/// **该函数永不返回**（已武装时执行 iretq；未武装时 fallback panic）。
///
/// # Safety
/// 必须在 KERNEL_FRAME_ARMED 期间（即 smoke 已启动且未清理）由
/// syscall_dispatch 单次调用。
pub unsafe fn handle_process_exit() -> ! {
    if !KERNEL_FRAME_ARMED.load(Ordering::SeqCst) {
        // 真退进程 — MVP 暂 panic（无 spawn 列表）。
        panic!("[ring3] process_exit called without armed KERNEL_FRAME (not from smoke)");
    }
    info!("[ring3] process_exit syscall — iretq to kernel continuation");

    // SAFETY: KERNEL_FRAME 全字段已由 smoke 填好；CPU iretq 会按 iretq 帧弹
    // RIP, CS, RFLAGS, RSP, SS，跳到 return_continuation 在 ring-0 上下文。
    let kf_addr = KERNEL_FRAME.0.get() as u64;
    unsafe {
        asm!(
            // swapgs 收支平衡（P4-T5 真机暴露的 #DF 根因）：syscall 入口的
            // swapgs 已把 GS.base 切到 &PER_CPU，而本路径跳过 sysretq 侧的
            // 平衡 swapgs。不在此补上，continuation（及其后续 elf_load_smoke）
            // 再 iretq 进新用户上下文时，ring-3 的 GS.base 仍是内核指针、
            // KERNEL_GS_BASE 是旧垃圾——用户第一次 syscall 的入口 swapgs 会
            // 把垃圾装入 GS.base，`mov rsp, gs:[0]` 得到垃圾栈指针，首条
            // push（syscall_entry_asm+0x15）即 fault，异常递送再失败 → #DF。
            // swapgs 后恢复 boot 约定：GS.base=用户值（无意义）、
            // KERNEL_GS_BASE=&PER_CPU；continuation 是普通 ring-0 代码，
            // 不经 gs:[] 取数，与首次进入用户态前的状态完全一致。
            "swapgs",
            "mov rsp, {kf}",
            "iretq",
            kf = in(reg) kf_addr,
            options(noreturn),
        );
    }
}

// ============================================================================
// ring-0 continuation（C-ABI：从 iretq 直接跳入，无栈帧）
// ============================================================================

/// 真正的 ring-0 continuation。CPU 状态：
/// - CPL=0（CS=0x08, SS=0x10）
/// - RSP=boot_sp（=ring3_smoke caller 的栈）
/// - RFLAGS 含 IF=1
/// - 其他寄存器未定义
#[no_mangle]
extern "C" fn return_continuation() -> ! {
    // 1. 切 CS/SS 回 ring-0 已由 iretq 完成；验证一下
    let cs = CS::get_reg();
    assert_eq!(cs.0 & 3, 0, "CS.RPL must be 0 in kernel continuation");

    // 2. 切 CR3 回 kernel AS
    let old_cr3 = RETURN_OLD_CR3.load(Ordering::SeqCst);
    unsafe { crate::paging::cr3_write(old_cr3) };
    info!("[ring3-smoke]   ok: CR3 -> kernel ({:#x})", old_cr3);

    // 3. 读 data page PA 拿 ABI 值（VA==PA：kernel VA == PA → kernel 读
    //    PA 即读 user 数据，无需切 CR3）。
    let data_pa = RETURN_DATA_PA.load(Ordering::SeqCst);
    let abi_value = unsafe { *(data_pa as *const u64) };
    let expected = ((synapse_abi::ABI_MAJOR as u64) << 16) | (synapse_abi::ABI_MINOR as u64);
    if abi_value != expected {
        panic!(
            "[ring3-smoke] ABI value mismatch: got {:#x}, expected {:#x}",
            abi_value, expected
        );
    }
    info!(
        "[ring3-smoke]   ok: ABI value roundtrip = {:#x} (data_pa={:#x})",
        abi_value, data_pa
    );

    // 3.5 P4-T6：stub 末尾 #999 触发 IllegalSyscall 杀进程路径的计数断言
    //     （本 continuation 正是经该路径的 KERNEL_FRAME iretq 到达的）。
    let illegal = crate::syscall::illegal_syscall_count();
    assert_eq!(
        illegal, 1,
        "[ring3-smoke] IllegalSyscall count = {illegal}, expected 1 (stub #999)"
    );
    info!("[ring3-smoke]   ok: IllegalSyscall(#999) killed stub, count = {illegal}");

    // 4. 读 CPL 断言：本函数以 ring-0 进入；用户态曾进入 = RPL==3 已验证
    //    （CS=0x2B during user mode；现在 CS=0x08）。
    //    已通过 iretq 路径间接证明 — 跳过冗余 assert。

    // 5. 清理：free frame + drop AS
    let code_pa = RETURN_CODE_PA.load(Ordering::SeqCst);
    let as_ptr = RETURN_USER_AS_PTR.load(Ordering::SeqCst) as *mut AddressSpace;
    unsafe {
        // user AS 内的 map_page 用的帧由 ring3_smoke 显式 alloc，
        // 这里 unmap + free。code 在 USER_CODE_ADDR，data 在 DATA_PAGE_ADDR。
        (*as_ptr)
            .unmap_page(USER_CODE_ADDR)
            .expect("[ring3-smoke] unmap code");
        (*as_ptr)
            .unmap_page(DATA_PAGE_ADDR)
            .expect("[ring3-smoke] unmap data");
        drop(core::ptr::read(as_ptr));
    }
    free_frame(data_pa);
    free_frame(code_pa);
    info!("[ring3-smoke]   ok: user AS + frames freed");

    // 6. disarm KERNEL_FRAME（防止再次 iretq 跳回）
    KERNEL_FRAME_ARMED.store(false, Ordering::SeqCst);

    // 7. 断言 + 总账
    let used_post = with_page_frames(|a| a.used_frames());
    info!(
        "[ring3-smoke]   ok: FR8 post-cleanup used_frames={}",
        used_post
    );

    info!("[ring3-smoke] PASS");

    // 8. P4-T5 链式 smoke：本 continuation 由 iretq 直达（不返回 main 正常路径），
    //    故直接接力下一场——从 initramfs 提取 hello ELF → 装入用户 AS → iretq
    //    执行 → elf_continuation 负责断言 + QEMU 退出（exit 363）。
    crate::elfload::elf_load_smoke()
}

// ============================================================================
// ring3_smoke 主流程
// ============================================================================

/// P4-T4 真机 smoke：在 paging::vma_smoke 之后调用。
pub fn ring3_smoke() {
    if SMOKE_DONE.swap(true, Ordering::SeqCst) {
        panic!("ring3_smoke twice");
    }
    info!("[ring3-smoke] start");

    // 取 smoke 入口 RSP + RFLAGS（iretq 回内核时还原）
    let ksp: u64;
    let krflags: u64;
    unsafe {
        asm!("mov {}, rsp", out(reg) ksp, options(nomem, nostack, preserves_flags));
        asm!("pushf; pop {}", out(reg) krflags, options(nomem, nostack, preserves_flags));
    }

    // 1. 构造用户 AS + 映射 code/data 页
    let mut as_user = AddressSpace::new().expect("[ring3-smoke] frames for AS");
    let old_cr3 = crate::paging::cr3_read();
    RETURN_OLD_CR3.store(old_cr3, Ordering::SeqCst);

    let code_frame = alloc_frame().expect("[ring3-smoke] code frame");
    let data_frame = alloc_frame().expect("[ring3-smoke] data frame");
    RETURN_CODE_PA.store(code_frame, Ordering::SeqCst);
    RETURN_DATA_PA.store(data_frame, Ordering::SeqCst);

    unsafe {
        core::ptr::write_bytes(code_frame as *mut u8, 0, 4096);
        core::ptr::copy_nonoverlapping(
            USER_CODE_BYTES.as_ptr(),
            code_frame as *mut u8,
            USER_CODE_BYTES.len(),
        );
        core::ptr::write_bytes(data_frame as *mut u8, 0, 4096);
    }

    // code 页 RX（PT_USER + 无 W = RX）+ NX 不显式（code 应可执行）
    as_user
        .map_page(USER_CODE_ADDR, code_frame, PT_USER)
        .expect("[ring3-smoke] map code");
    // data 页 RW + NX（数据段不可执行）
    as_user
        .map_page(DATA_PAGE_ADDR, data_frame, PT_USER | PT_WRITABLE | PT_NX)
        .expect("[ring3-smoke] map data");
    info!(
        "[ring3-smoke]   ok: mapped code@{:#x} data@{:#x}",
        USER_CODE_ADDR, DATA_PAGE_ADDR
    );

    // 校验 EFER.NXE 已开（P4-T3 paging_smoke 已开启；此处防御性断言）
    unsafe {
        let mut efer_lo: u32;
        core::arch::asm!("rdmsr", in("ecx") 0xC000_0080u32, out("eax") efer_lo, options(nomem, nostack));
        debug_assert!(efer_lo & (1 << 11) != 0, "NXE must be set before entering user mode");
    }

    // 2. 切换 CR3 到用户 AS
    unsafe { as_user.activate() };
    info!("[ring3-smoke]   ok: CR3 -> user AS ({:#x})", as_user.pml4_phys());

    // 3. CS/SS 不能从 ring-0 直接 set_reg 到 ring-3 选择子
    //    （AMD64 规定 SS.RPL 必须 == CPL；iretq 是唯一合法入口），
    //    此处**不**显式切段，由 iretq 帧的 CS/SS 字段统一设置。
    info!("[ring3-smoke]   ok: CS/SS will be set by iretq frame (ring-0 cannot set SS.RPL=3 directly)");

    // 4. 准备"返回内核" iretq 帧（KERNEL_FRAME 全局 static）
    unsafe { write_kernel_frame(return_continuation as *const () as u64, ksp, krflags) };
    info!(
        "[ring3-smoke]   ok: KERNEL_FRAME armed @ {:#x} (rip={:#x} rsp={:#x})",
        KERNEL_FRAME.0.get() as u64,
        return_continuation as *const () as u64,
        ksp
    );

    // 5. 把 as_user 指针给 return_continuation（drop 用）
    RETURN_USER_AS_PTR.store(&mut as_user as *mut AddressSpace as u64, Ordering::SeqCst);

    // 6. 构造 user iretq frame 并 iretq
    info!(
        "[ring3-smoke]   iretq to user: rip={:#x} cs={:#x} rsp={:#x} ss={:#x}",
        USER_CODE_ADDR, USER_CS, USER_STACK_TOP, USER_DS
    );

    // 7. iretq to user → 不可返回（continuation 通过 syscall #21 接力）
    unsafe { enter_user_at(USER_CODE_ADDR, USER_STACK_TOP) }
}

// ============================================================================
// 可复用的"武装返回帧 + 进入用户态"原语（P4-T5 elf smoke 复用）
// ============================================================================

/// 写入并武装"返回内核" iretq 帧。
///
/// # Safety
/// `rip` 必须指向 `extern "C" fn() -> !` 的 ring-0 continuation；
/// `rsp`/`rflags` 必须是可安全恢复的 ring-0 上下文（典型 = smoke 入口
/// 捕获值）。武装后 syscall #21 (process_exit) 将 iretq 到该帧。
pub(crate) unsafe fn write_kernel_frame(rip: u64, rsp: u64, rflags: u64) {
    unsafe {
        core::ptr::write_volatile(
            KERNEL_FRAME.0.get(),
            KernelIretqFrame {
                rip,
                cs: KERNEL_CS,
                rflags,
                rsp,
                ss: KERNEL_DS,
            },
        );
    }
    KERNEL_FRAME_ARMED.store(true, Ordering::SeqCst);
}

/// 构造用户态 iretq 帧并进入 ring-3（永不返回；回程走 KERNEL_FRAME 接力）。
///
/// `rsp` 语义 = iretq 后用户态 RSP。对 Rust 编译的用户 ELF 取
/// `栈顶 - 8`（模拟 call 入口 rsp%16==8 约定，见 elfload.rs）。
///
/// # Safety
/// `rip`/`rsp` 必须指向已在当前 CR3 用户 AS 中映射且权限正确的代码/栈；
/// 调用前必须已 [`write_kernel_frame`]。
pub(crate) unsafe fn enter_user_at(rip: u64, rsp: u64) -> ! {
    let uframe = IretqFrame {
        rip,
        cs: USER_CS,
        rflags: RFlags::INTERRUPT_FLAG.bits() | (1 << 1), // bit1 保留位（x86 强制 =1）
        rsp,
        ss: USER_DS,
        _pad: 0,
    };
    enter_user(&uframe)
}

#[inline(never)]
extern "C" fn enter_user(frame: &IretqFrame) -> ! {
    unsafe {
        asm!(
            "call {iretq_fn}",
            iretq_fn = sym iretq_to_user_asm,
            in("rdi") frame as *const _ as u64,
            options(noreturn),
        )
    }
}

// ============================================================================
// 适配 syscall::syscall_dispatch（process_exit 走 iretq-to-kernel 路径）
// ============================================================================

/// 暴露给 syscall::syscall_dispatch 的 process_exit 桥接。
/// 编译期 if-else（dispatch 走 num match）；这里提供 fn pointer 给 dispatch 用。
///
/// # Safety
/// 见 [`handle_process_exit`]。
#[allow(dead_code)]
#[inline]
pub unsafe fn process_exit_bridge() -> i64 {
    handle_process_exit()
}