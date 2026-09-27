//! syscall/sysret + iretq 首次进入用户态 (P4-T4)。
//!
//! ## 范围
//!
//! - 配置 MSR_STAR / MSR_LSTAR / MSR_FMASK / IA32_KERNEL_GS_BASE，
//!   把 `syscall` 指令接到 [`syscall_entry_asm`]；
//! - **P4-T6 完整分发**：`synapse_abi::decode` 严格解码（未知号 / 窄参数
//!   越界 → `None` = IllegalSyscall，杀进程，不静默截断）；实现
//!   AbiQuery(#18) / ProcessExit(#21) / Yield(#25) / GetTime(#30) /
//!   Mmap(#40) / Munmap(#41)，其余已定义号 → `E_NOT_IMPLEMENTED`；
//! - 用户指针前置校验：[`user_mem_ok`] 走 [`AddressSpace::walk_flags`]
//!   检查 present + PT_USER + PT_WRITABLE（拒绝而非 fault）；
//! - per-CPU 数据 `PerCpu { kstack_top: u64 }` 通过 IA32_KERNEL_GS_BASE
//!   寻址（单核 MVP 仅一个实例；Phase 5+ 多核时改为 per-core）；
//! - 提供 [`init_syscall`] 在 boot 链路上调一次（在 `idt::init_idt` 之后）。
//!
//! ## 返回值写回纪律（asm glue 契约）
//!
//! `syscall_entry_asm` 在 dispatch 返回后 **`pop rax` 从 frame.num 槽取
//! 返回值**（C-ABI rax 被 pop 覆盖）——分发层必须把结果写回 `frame.num`。
//! [`syscall_dispatch`] 外壳统一写回，内部只算值。
//!
//! ## 不在本模块
//!
//! - per-thread 内核栈（每线程独立 kstack_top）— Phase 5+ 调度器接；
//! - FaultKind 归因 / death notification / reap — P4-T9 进程表接线；
//! - IPC / cap 系 syscall — P4-T7/T8。
//!
//! ## swapgs 纪律（AMD64 Vol.2 §4 syscall/sysret）
//!
//! - 进入：先 `swapgs`（让 GS.base 切到 per-CPU kernel GS，访问 kstack_top）；
//! - 返回：再 `swapgs`（切回 user GS）；**绕过 sysretq 的路径必须自行配平**
//!   （`ring3::handle_process_exit` 的 iretq-to-continuation 在跳前补 `swapgs`，
//!   否则后续再进用户态时首次 syscall 的 swapgs 会装入垃圾 GS.base → #DF）；
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
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use synapse_abi::{
    abi_query_value, decode, Syscall, SyscallFrame, Timespec, CLOCK_MONOTONIC, CLOCK_WALL,
    E_INVALID_ADDR, E_NOT_FOUND, E_NOT_IMPLEMENTED,
};

use crate::paging::{AddressSpace, PT_USER, PT_WRITABLE};

// ---------------------------------------------------------------------------
// MSR 常量（AMD64 Manual Vol.2 §5 MSRs）
// ---------------------------------------------------------------------------

const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_FMASK: u32 = 0xC000_0084;
const MSR_KERNEL_GS_BASE: u32 = 0xC000_0102;

/// STAR 寄存器布局（AMD64 Vol.2 §4）：
/// - bits 47:32 = kernel CS（syscall 入口 CPL=0，CS = STAR[47:32]）
/// - bits 63:48 = user CS - 16（sysret 出口 CS = STAR[63:48] + 16，SS = STAR[63:48] + 8）
///
/// 我们对齐 Linux 习惯：
/// - kernel_CS = 0x08（GDT index 1, RPL=0）
/// - user_CS = 0x2B（GDT index 5, RPL=3）
/// - STAR[63:48] = user_CS - 16 = 0x2B - 16 = 0x1B
/// - sysret CS = 0x1B + 16 = 0x2B ✓
/// - sysret SS = 0x1B + 8 = 0x23（user DS）✓
const STAR_VALUE: u64 = 0x0000_001B_0000_0008u64;

/// FMASK：syscall 入口自动清除的 RFLAGS 位。设 0 = 保留全部（含 IF），
/// 允许中断嵌套。
const FMASK_VALUE: u64 = 0;

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
    // 加载当前进程 kstack_top（P4-T9a：per-process kstack，gs:[8] = kstack_top）
    // gs:[0] = current_pid（cap lookup 等用，本处不读）
    "mov rsp, gs:[8]",

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
    // pop 顺序必须从低偏移到高偏移：先 pop rax（返回值），再 pop 6 args。
    //
    // **user RSP 必须由内核显式还原**（P4-T5 真机暴露）：sysretq 不弹 RSP —
    // 原实现 `add rsp, 24` 后 RSP 停在 kstack_top，用户态带着内核栈指针返回，
    // 真实 ELF（rustc 生成 push/pop）立刻 #PF。T4 手写 stub 不触栈故未暴露。
    // r12/r13/r14 为 SysV callee-saved，dispatch（Rust extern "C"）保证还原。
    "pop rax",               // offset 0: dispatch 返回的 syscall 结果
    "pop rdi",               // offset 8:  args[0] 还原
    "pop rsi",               // offset 16: args[1] 还原
    "pop rdx",               // offset 24: args[2] 还原
    "pop r10",               // offset 32: args[3] 还原
    "pop r8",                // offset 40: args[4] 还原
    "pop r9",                // offset 48: args[5] 还原
    "mov rsp, r12",          // 还原 user RSP（跳过 offset 56/64/72 三个保留槽）

    // 准备 sysretq：RCX=user_RIP，R11=user_RFLAGS（从 callee-saved r12/r13/r14 取回）
    "mov rcx, r13",
    "mov r11, r14",
    "swapgs",                // GS.base 切回 user GS
    "sysretq",
    "ud2",                   // 不应落到

    dispatch = sym syscall_dispatch,
);

// ---------------------------------------------------------------------------
// Per-CPU 数据由 `proc_ext::PerCpu` 提供（P4-T9a：current_pid + kstack_top）。
// 此处只负责 MSR 配置。
// ---------------------------------------------------------------------------

static INIT_DONE: AtomicBool = AtomicBool::new(false);

/// syscall/sysret + MSR 初始化（boot 链路调一次，在 `idt::init_idt` 之后）。
///
/// # Safety
///
/// - 必须在 long mode + CPL=0 + IDT 已装 + GDT 含 ring-3 描述符之后调用；
/// - 必须先调用 `proc_ext::init_proc_ext()`（TSS.RSP0 已就绪）；
/// - 重复调用会 panic。
pub unsafe fn init_syscall() {
    if INIT_DONE.swap(true, Ordering::SeqCst) {
        panic!("init_syscall called twice");
    }

    // 1. PROC_EXT + PerCpu（init 必须已调用；TSS.RSP0 + GDT ring-3 描述符已就绪）
    crate::proc_ext::init_proc_ext();

    // 2. 写 MSR
    wrmsr(MSR_STAR, STAR_VALUE);
    wrmsr(MSR_LSTAR, syscall_entry_asm as *const () as u64);
    wrmsr(MSR_FMASK, FMASK_VALUE);

    // 3. IA32_KERNEL_GS_BASE = &PER_CPU（asm `gs:[8]` 读 kstack_top）
    let gs_base = crate::proc_ext::per_cpu_pointer();
    wrmsr(MSR_KERNEL_GS_BASE, gs_base);

    log::info!(
        "[syscall] MSRs: STAR={:#x} LSTAR={:#x} FMASK={:#x} GS_BASE={:#x} kstack_top={:#x}",
        STAR_VALUE,
        syscall_entry_asm as *const () as u64,
        FMASK_VALUE,
        gs_base,
        crate::proc_ext::current_kstack_top(),
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

/// abi_query 成功分发计数（P4-T5 elf smoke 断言"用户 ELF 真实执行过
/// syscall 往返"的内核侧证据；P4-T9 进程表接线后并入 per-process 统计）。
static ABI_QUERY_COUNT: AtomicU64 = AtomicU64::new(0);

/// 读取 abi_query 分发计数。
pub fn abi_query_count() -> u64 {
    ABI_QUERY_COUNT.load(Ordering::SeqCst)
}

/// IllegalSyscall（decode → None）触发计数（P4-T6：ring3 stub 用号 999
/// 真机验证杀进程路径；T9 接 FaultKind 归因 + death notification）。
static ILLEGAL_SYSCALL_COUNT: AtomicU64 = AtomicU64::new(0);

/// 读取 IllegalSyscall 计数。
pub fn illegal_syscall_count() -> u64 {
    ILLEGAL_SYSCALL_COUNT.load(Ordering::SeqCst)
}

/// syscall 主分发（C-ABI 外壳）：算值 → **写回 frame.num**（asm glue 的
/// `pop rax` 从 num 槽取返回值，见模块头"返回值写回纪律"）→ 返回。
///
/// 入口约定：
/// - `rdi` = `&mut SyscallFrame`（栈布局：rax=num / args[0..5] = rdi,rsi,rdx,r10,r8,r9）
/// - 返回值 = `i64`（≥0 成功，<0 错误码，Doc 02 §4.3）
#[no_mangle]
extern "C" fn syscall_dispatch(frame: &mut SyscallFrame) -> i64 {
    let ret = dispatch_inner(frame);
    frame.num = ret as u64;
    ret
}

fn dispatch_inner(frame: &SyscallFrame) -> i64 {
    log::info!("[syscall] dispatch: num={} ({:#x})", frame.num, frame.num);
    let Some(sc) = decode(frame) else {
        // IllegalSyscall（Doc 02 §5.3）：未知号 / 窄参数越界。不静默截断、
        // 不按 E_NOT_IMPLEMENTED 温和返回——杀进程语义。MVP smoke 下与
        // process_exit 同走 KERNEL_FRAME iretq 接力（T9 换成 FaultKind
        // 归因 + death notification + reap）。
        ILLEGAL_SYSCALL_COUNT.fetch_add(1, Ordering::SeqCst);
        log::error!(
            "[syscall] IllegalSyscall: num={} ({:#x}) args=[{:#x} {:#x} {:#x} {:#x} {:#x} {:#x}] — killing process",
            frame.num, frame.num,
            frame.args[0], frame.args[1], frame.args[2],
            frame.args[3], frame.args[4], frame.args[5],
        );
        // SAFETY: smoke 上下文已武装 KERNEL_FRAME（未武装 = 内核契约破坏，
        // handle_process_exit 内部 panic 兜底）。
        unsafe { crate::ring3::handle_process_exit() }
    };

    match sc {
        Syscall::AbiQuery => {
            ABI_QUERY_COUNT.fetch_add(1, Ordering::SeqCst);
            abi_query_value() as i64
        }
        Syscall::ProcessExit { code } => {
            log::info!("[syscall] process_exit(code={code})");
            // 委托 ring3::handle_process_exit（永不返回）。
            // SAFETY: smoke 上下文已武装 KERNEL_FRAME。
            unsafe { crate::ring3::handle_process_exit() }
        }
        Syscall::Yield => {
            // 接 P3 调度器：当前线程让出（单 runnable 线程时近似 no-op；
            // r12/r13/r14 为 callee-saved，switch_to 保证跨切换还原）。
            crate::kthread::kthread_yield();
            0
        }
        Syscall::GetTime { clock_id, ts_out } => with_user_as(|as_ptr| {
            // SAFETY: with_user_as 已校验 as_ptr 非空（= 激活的用户 AS）。
            unsafe { sys_gettime(as_ptr, clock_id, ts_out) }
        }),
        Syscall::Mmap { addr, len, prot, flags } => with_user_as(|as_ptr| {
            // SAFETY: 同上。
            unsafe { crate::umem::sys_mmap(as_ptr, addr, len, prot, flags) }
        }),
        Syscall::Munmap { addr, len } => with_user_as(|as_ptr| {
            // SAFETY: 同上。
            unsafe { crate::umem::sys_munmap(as_ptr, addr, len) }
        }),
        other => {
            match other {
                Syscall::IpcSend { .. } => crate::ipc::k_ipc_send(frame),
                Syscall::IpcRecv { .. } => crate::ipc::k_ipc_recv(frame),
                Syscall::IpcReply { .. } => crate::ipc::k_ipc_reply(frame),
                Syscall::IpcTrySend { .. } => crate::ipc::k_ipc_try_send(frame),
                _ => {
                    log::warn!("[syscall] unimplemented syscall {:?} (num={})", other.id(), frame.num);
                    E_NOT_IMPLEMENTED
                }
            }
        }
    }
}

/// 取当前用户 AS 并执行（mmap/munmap/gettime 公共前置）。
///
/// AS 未武装（= 非 smoke/进程上下文的裸调用）时返回 [`E_INVALID_ADDR`]
/// 并记 error——单核 MVP 下用户 syscall 只可能发生在 elf smoke 窗口内。
fn with_user_as(f: impl FnOnce(*mut AddressSpace) -> i64) -> i64 {
    let p = crate::elfload::current_as_ptr();
    if p == 0 {
        log::error!("[syscall] syscall needing user AS with none armed");
        return E_INVALID_ADDR;
    }
    f(p as *mut AddressSpace)
}

/// 用户指针区间校验：[va, va+len) 每页 present + PT_USER（+ PT_WRITABLE
/// 若 `need_write`）。拒绝而非 fault——syscall 集成层不允许触发 #PF 路径
/// （demand paging 只对 VMA 登记的懒映射区生效，gettime 输出指针不在其列）。
fn user_mem_ok(as_user: &AddressSpace, va: u64, len: u64, need_write: bool) -> bool {
    if len == 0 {
        return false;
    }
    let Some(end) = va.checked_add(len) else { return false };
    let mut page = va & !0xFFF;
    while page < end {
        match as_user.walk_flags(page) {
            Some((_, flags)) => {
                if flags & PT_USER == 0 {
                    return false;
                }
                if need_write && flags & PT_WRITABLE == 0 {
                    return false;
                }
            }
            None => return false,
        }
        page += 0x1000;
    }
    true
}

/// `gettime(clock_id, ts_out)`（#30）实现。
///
/// - `CLOCK_MONOTONIC`(0)：clock.rs TSC 单调钟（ns since boot）折算
///   sec/nsec 写入 `ts_out`（16B [`Timespec`]，经恒等映射 PA 写入）；
/// - `CLOCK_WALL`(1)：RTC 未接硬件 → [`E_NOT_IMPLEMENTED`]（deliverable ③
///   明确延后）；用户态不暴露 rdtsc，TSC 原始值仅内核可见；
/// - 未知 clock_id → [`E_NOT_FOUND`]；`ts_out` 未 8B 对齐或指向不可写 /
///   未映射 / 非用户页 → [`E_INVALID_ADDR`]。
///
/// # Safety
/// `as_ptr` 必须指向当前激活的用户 [`AddressSpace`]。
unsafe fn sys_gettime(as_ptr: *mut AddressSpace, clock_id: u32, ts_out: u64) -> i64 {
    if clock_id == CLOCK_WALL {
        return E_NOT_IMPLEMENTED;
    }
    if clock_id != CLOCK_MONOTONIC {
        return E_NOT_FOUND;
    }
    // Timespec 两个 u64 字段：要求 8B 对齐，保证 sec/nsec 各自不跨页
    if ts_out % 8 != 0 {
        return E_INVALID_ADDR;
    }
    // SAFETY: 调用方契约。
    let as_user = unsafe { &*as_ptr };
    if !user_mem_ok(as_user, ts_out, core::mem::size_of::<Timespec>() as u64, true) {
        return E_INVALID_ADDR;
    }

    let ns = crate::clock::monotonic_ns();
    let ts = Timespec { sec: ns / 1_000_000_000, nsec: ns % 1_000_000_000 };

    // 经 walk_flags 取 PA（含页内偏移）后用恒等映射直写——不依赖当前
    // CR3 的用户映射可达性（ring-0 写用户页也绕开 U/S 位语义争议）。
    // SAFETY: 已校验两页 present + US + W；PA 为恒等映射可写内核视角。
    unsafe {
        let (pa_sec, _) = as_user.walk_flags(ts_out).unwrap_unchecked();
        let (pa_nsec, _) = as_user.walk_flags(ts_out + 8).unwrap_unchecked();
        (pa_sec as *mut u64).write_volatile(ts.sec);
        (pa_nsec as *mut u64).write_volatile(ts.nsec);
    }
    0
}

// ---------------------------------------------------------------------------
// 首次进入用户态：构造 iretq 帧（`iretq_to_user_asm` 由 `ring3` 模块定义）
// ---------------------------------------------------------------------------