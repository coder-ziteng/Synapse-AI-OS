//! syscall/sysret + iretq 首次进入用户态 (P4-T4)。
//!
//! ## 范围
//!
//! - 配置 MSR_STAR / MSR_LSTAR / MSR_FMASK / IA32_KERNEL_GS_BASE，
//!   把 `syscall` 指令接到 [`syscall_entry_asm`]；
//! - 实现最小 syscall 分发骨架：仅 `AbiQuery`（#18）返回 `(major<<16)|minor`，
//!   其余号 → `-E_NOT_IMPLEMENTED` (=-10)。P4-T6 扩展；
//! - per-CPU 数据 `PerCpu { kstack_top: u64 }` 通过 IA32_KERNEL_GS_BASE
//!   寻址（单核 MVP 仅一个实例；Phase 5+ 多核时改为 per-core）；
//! - 提供 [`init_syscall`] 在 boot 链路上调一次（在 `idt::init_idt` 之后）。
//!
//! ## 不在本模块
//!
//! - 用户态地址合法性校验 / copy_from_user — P4-T6 syscall 分发层补；
//! - per-thread 内核栈（每线程独立 kstack_top）— Phase 5+ 调度器接；
//! - 用户态 ELF 加载 / init 进程 spawn — P4-T5/T9。
//!
//! ## swapgs 纪律（AMD64 Vol.2 §4 syscall/sysret）
//!
//! - 进入：先 `swapgs`（让 GS.base 切到 per-CPU kernel GS，访问 kstack_top）；
//! - 返回：再 `swapgs`（切回 user GS）；
//! - GS.base 在用户态可被用户程序写 — 内核态数据绝不能依赖 GS.base 直读，
//!   一律经 `swapgs` 后的 kernel GS 取。
//!
//! ## 嵌套中断策略
//!
//! - `syscall` 不屏蔽中断（FMASK=0，保留 RFLAGS.IF）— 中断可在 syscall
//!   执行期间嵌套，IDT handler 沿用现有 ring-0 路径（idt::init_idt 已装）；
//! - 内核栈深度：`syscall 入口 push 9 caller-saved` ≈ 72B + C 栈
//!   (≈ 数百 B) + 嵌套 IRQ 栈帧 ≈ < 1KB，离 16KB 上限远，余量充足；
//! - 嵌套 IRQ 自身若再触发 syscall → 死锁风险（MVP 接受：内核不嵌套 syscall）；
//!   进程调度 hook 接进来后（T6+）用 per-thread 标志位防重入。

use core::arch::asm;
use core::cell::UnsafeCell;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use synapse_abi::{SyscallFrame, SyscallId, ABI_MAJOR, ABI_MINOR};

// ---------------------------------------------------------------------------
// MSR 常量（AMD64 Manual Vol.2 §5 MSRs）
// ---------------------------------------------------------------------------

const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_FMASK: u32 = 0xC000_0084;
const MSR_KERNEL_GS_BASE: u32 = 0xC000_0102;

/// STAR 寄存器布局（Linux 习惯）：
/// - bits 47:32 = kernel CS（syscall 时 CPU 把它 +8 作 SS、+16 作 CS）
/// - bits 63:48 = user CS base（sysret 时 CPU 把它 +8 作 SS、+16 作 CS）
///
/// 设 STAR = `(0x08 << 32) | 0x23`：syscall 路径 CS=0x08+16=0x18?
/// 实际 AMD64 Vol.2 §4.1：syscall 把 STAR[63:48] 加载到 CS（不是 STAR[47:32]）。
/// 重读：STAR[47:32] 用于 syscall CS 加载，STAR[63:48] 用于 sysret CS 加载。
///
/// 正确布局（Linux 实测）：
/// - STAR[47:32] = 0x10（kernel DS），syscall 路径 CS = 0x10 + 8? 不，
///   Linux 写的是 0x08 kernel CS：syscall 入口 CPL=0，CS = STAR[47:32]+8?
///   Vol.2 表 4-1：syscall 设 CS.Sel = STAR[47:32] + 8? 不，CS.Sel = STAR[63:48]
///   也不对 — 实际 Linux 把 STAR = (CS_K << 32) | (CS_U-16)，CS_U=0x33：
///   STAR = (0x08 << 32) | 0x23 —— syscall 入口 CS = STAR[47:32]=0x08，
///   sysret 出口 CS = STAR[63:48]+16=0x33。
///
/// 我们对齐 Linux：STAR = (kernel_CS << 32) | (user_CS - 16)，
/// 其中 user_CS = 0x33（ring-3 code sel），sysret 把它读回；user_DS = 0x2B，
/// sysret SS = (user_CS - 16) + 8 = 0x23 + 8 = 0x2B。
const STAR_VALUE: u64 = 0x0000_0008_0000_0023u64;

/// FMASK：syscall 入口自动清除的 RFLAGS 位。设 0 = 保留全部（含 IF），
/// 允许中断嵌套。
const FMASK_VALUE: u64 = 0;

/// -10 = `E_NOT_IMPLEMENTED`（Doc 02 §4.3）。
const E_NOT_IMPLEMENTED: i64 = -10;

// syscall 入口汇编（global_asm!）。
//
// AMD64 syscall 入口约定（Vol.2 §4.1）：
// - 写入 RCX = 用户态 RIP（syscall 下一条地址）
// - 写入 R11 = 用户态 RFLAGS
// - RIP 跳到 MSR_LSTAR
// - CPL=0，CS/SS = STAR[47:32]/STAR[63:48]+?（见 STAR_VALUE 注释）
// - RSP 仍是用户态值 — handler 必须自己切到内核栈
//
// ## 入口动作
// 1. `swapgs` — 让 GS.base 切到 IA32_KERNEL_GS_BASE（= &PER_CPU）
// 2. `mov rsp, gs:[0]` — 加载 kstack_top
// 3. push GPR + 调用 C-ABI 分发
// 4. 复原 + `swapgs` + `sysretq`
extern "C" {
    fn syscall_entry_asm();
}

core::arch::global_asm!(
    ".global syscall_entry_asm",
    "syscall_entry_asm:",
    // 保存用户态 RSP/RIP/RFLAGS 到 callee-saved（避开后续 push 覆盖）
    "mov r12, rsp",          // r12 = user_RSP
    "mov r13, rcx",          // r13 = user_RIP（syscall 指令已写入 rcx）
    "mov r14, r11",          // r14 = user_RFLAGS（syscall 指令已写入 r11）

    // swapgs：GS.base ← IA32_KERNEL_GS_BASE = &PER_CPU
    "swapgs",
    // 加载内核栈顶（per-CPU 数据首字段 = kstack_top）
    "mov rsp, gs:[0]",

    // 推送 SyscallFrame（synapse_abi 布局：num(offset 0)=rax, args[0..5](offset 8..48)=rdi/rsi/rdx/r10/r8/r9,
    // user_RSP/RIP/RFLAGS 保留供 sysret 还原(offset 56+))。
    //
    // **push 顺序 = 栈内存布局反转**：栈向低地址增长，LAST push 落在最低地址。
    // 要让 rax 落在 offset 0（=SyscallFrame.num 槽），必须最后 push rax。
    // 同理 rdi 必须最后 push args 才落在 offset 8 = args[0]。
    // 先 push user_RSP/RIP/RFLAGS（最高 3 个偏移 56/64/72），
    // 再 push args[5..0]（r9 最高→offset 48，rdi 次之→offset 8），
    // 最后 push rax（offset 0 = num 槽）。
    "push r14",              // offset 72: user_RFLAGS
    "push r13",              // offset 64: user_RIP
    "push r12",              // offset 56: user_RSP
    "push r9",               // offset 48: args[5]
    "push r8",               // offset 40: args[4]
    "push r10",              // offset 32: args[3]
    "push rdx",              // offset 24: args[2]
    "push rsi",              // offset 16: args[1]
    "push rdi",              // offset 8:  args[0]
    "push rax",              // offset 0:  num (= SyscallFrame.num)

    // rdi = &SyscallFrame（rax 槽 = SyscallFrame.num 位置）
    "mov rdi, rsp",
    "call {dispatch}",

    // syscall_dispatch 返回 i64 在 rax — dispatcher 已写回 frame.num 槽。
    // 栈上布局（低→高）：rax, rdi, rsi, rdx, r10, r8, r9, user_RSP, user_RIP, user_RFLAGS
    // pop 顺序必须从低偏移到高偏移：先 pop rax（返回值），再 pop 6 args，
    // 最后 add rsp, 24 跳过栈上的 user_RSP/RIP/RFLAGS 三份（r12/r13/r14 仍持有副本
    // 用于 sysretq 的 rcx/r11 装载）。
    "pop rax",               // offset 0: dispatch 返回的 syscall 结果
    "pop rdi",               // offset 8:  args[0] 还原
    "pop rsi",               // offset 16: args[1] 还原
    "pop rdx",               // offset 24: args[2] 还原
    "pop r10",               // offset 32: args[3] 还原
    "pop r8",                // offset 40: args[4] 还原
    "pop r9",                // offset 48: args[5] 还原
    "add rsp, 24",           // 弃栈上的 user_RSP/RIP/RFLAGS（offset 56/64/72）

    // 准备 sysretq：RCX=user_RIP，R11=user_RFLAGS（从 callee-saved r12/r13/r14 取回）
    "mov rcx, r13",
    "mov r11, r14",
    "swapgs",                // GS.base 切回 user GS
    "sysretq",
    "ud2",                   // 不应落到

    dispatch = sym syscall_dispatch,
);

// ---------------------------------------------------------------------------
// Per-CPU 数据（IA32_KERNEL_GS_BASE → &KSTACK_TOP_ADDR，gs:[0] = kstack_top）
// ---------------------------------------------------------------------------

/// 内核栈顶（per-CPU；MVP 单核 static；Phase 5+ 多核改为 per-core 数组）。
///
/// 放在独立 8B 对齐位置而非 PerCpu struct — 简化 IA32_KERNEL_GS_BASE 写入
/// （直接 wrmsr 该值地址）；asm `gs:[0]` 读首字段 = kstack_top。
#[repr(align(8))]
#[allow(dead_code)]
struct KstackTopCell(u64);

// SAFETY: 单核 MVP；init_syscall 唯一写入者，syscall_entry_asm 是 asm 读者
// （CPU 直接读 GS.base 指向地址，无 Rust 借用），串行访问不并发。
struct KstackTopWrap(UnsafeCell<KstackTopCell>);
unsafe impl Sync for KstackTopWrap {}
static KSTACK_TOP_WRAP: KstackTopWrap = KstackTopWrap(UnsafeCell::new(KstackTopCell(0)));

fn kstack_top_addr() -> u64 {
    &KSTACK_TOP_WRAP.0 as *const UnsafeCell<KstackTopCell> as *const u64 as u64
}

// ---------------------------------------------------------------------------
// 启动时初始化（写 MSR）
// ---------------------------------------------------------------------------

static INIT_DONE: AtomicBool = AtomicBool::new(false);

/// syscall/sysret + MSR 初始化（boot 链路调一次，在 `idt::init_idt` 之后）。
///
/// # Safety
///
/// - 必须在 long mode + CPL=0 + IDT 已装 + GDT 含 ring-3 描述符之后调用；
/// - 重复调用会 panic。
pub unsafe fn init_syscall() {
    if INIT_DONE.swap(true, Ordering::SeqCst) {
        panic!("init_syscall called twice");
    }

    // 1. 填 per-CPU kstack_top = TSS.RSP0（gdt 模块权威）
    let ksp = crate::gdt::rsp0_stack_top();
    unsafe {
        ptr::write_volatile(
            KSTACK_TOP_WRAP.0.get() as *mut u64,
            ksp,
        );
    }

    // 2. 写 MSR
    wrmsr(MSR_STAR, STAR_VALUE);
    wrmsr(MSR_LSTAR, syscall_entry_asm as *const () as u64);
    wrmsr(MSR_FMASK, FMASK_VALUE);

    // 3. IA32_KERNEL_GS_BASE = &KSTACK_TOP_WRAP（asm `gs:[0]` 读首字段）
    let gs_base = kstack_top_addr();
    wrmsr(MSR_KERNEL_GS_BASE, gs_base);

    log::info!(
        "[syscall] MSRs: STAR={:#x} LSTAR={:#x} FMASK={:#x} GS_BASE={:#x} kstack_top={:#x}",
        STAR_VALUE,
        syscall_entry_asm as *const () as u64,
        FMASK_VALUE,
        gs_base,
        ksp,
    );
}

/// 写 MSR（CPL=0）。
///
/// # Safety
/// 调用方必须处于 ring-0。
#[inline]
unsafe fn wrmsr(msr: u32, value: u64) {
    let lo = value as u32;
    let hi = (value >> 32) as u32;
    asm!(
        "wrmsr",
        in("ecx") msr,
        in("eax") lo,
        in("edx") hi,
        options(nostack, preserves_flags),
    );
}

// ---------------------------------------------------------------------------
// C-ABI 分发（syscall 入口 asm 调用此函数）
// ---------------------------------------------------------------------------

/// syscall 主分发（C-ABI）。
///
/// 入口约定：
/// - `rdi` = `&mut SyscallFrame`（栈布局：rax=num / args[0..5] = rdi,rsi,rdx,r10,r8,r9）
/// - 返回值 = `i64`（≥0 成功，<0 错误码）
///
/// ## MVP
/// 仅实现 `AbiQuery`（#18）和 `ProcessExit`（#21，P4-T4 ring3 smoke 专用）；
/// 其余返回 `E_NOT_IMPLEMENTED`。P4-T6 扩展 cap_invoke / gettime / yield /
/// exit / mmap / munmap。
#[no_mangle]
extern "C" fn syscall_dispatch(frame: &mut SyscallFrame) -> i64 {
    let num = frame.num;
    match SyscallId::from_num(num) {
        Some(SyscallId::AbiQuery) => {
            let v = ((ABI_MAJOR as u64) << 16) | (ABI_MINOR as u64);
            frame.num = v;
            v as i64
        }
        Some(SyscallId::ProcessExit) => {
            // 委托 ring3::handle_process_exit（永不返回）。
            // SAFETY: smoke 上下文已武装 KERNEL_FRAME。
            unsafe { crate::ring3::handle_process_exit() };
        }
        Some(_) => {
            log::warn!("[syscall] unimplemented syscall num={}", num);
            E_NOT_IMPLEMENTED
        }
        None => {
            log::warn!("[syscall] unknown syscall num={}", num);
            E_NOT_IMPLEMENTED
        }
    }
}

// ---------------------------------------------------------------------------
// 首次进入用户态：构造 iretq 帧（`iretq_to_user_asm` 由 `ring3` 模块定义）
// ---------------------------------------------------------------------------