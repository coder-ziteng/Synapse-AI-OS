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
    E_INVALID_ADDR, E_INVALID_CAP, E_NOT_FOUND, E_NOT_IMPLEMENTED, E_PERMISSION,
};
use synapse_cap::{ObjKind, Rights};
use synapse_proc::process::{FaultKind, Pid};

use crate::paging::AddressSpace;

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
/// - kernel_CS = 0x08（GDT index 1, RPL=0）→ **STAR[47:32] = 0x0008**
/// - user_CS = 0x2B（GDT index 5, RPL=3）
/// - STAR[63:48] = user_CS - 16 = 0x2B - 16 = 0x1B
/// - sysret CS = 0x1B + 16 = 0x2B ✓
/// - sysret SS = 0x1B + 8 = 0x23（user DS）✓
///
/// **P4 elf-smoke #GP 根因修复**：旧值 0x0000_001B_0000_0008 把两个字段整体
/// 放低了 16 位——STAR[47:32]=0x1B（syscall 入口 CS=0x18/SS=0x20，因 GDT
/// index 3 恰好是兼容 ring0 code 而"能跑"），STAR[63:48]=0（sysret 出口
/// CS=0x13/SS=0xb——非法选择子，但 sysretq 不校验描述符，用户态带病运行，
/// 直到下一次 ring3 中断把 0x13 压入帧、handler iretq 查 GDT → #GP）。
/// 正确布局：高 16 位 = 0x001B（sysret base），次 16 位 = 0x0008（syscall CS）。
const STAR_VALUE: u64 = 0x001B_0008_0000_0000u64;

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

/// 读 MSR（CPL=0）。
///
/// # Safety
/// 调用方必须处于 ring-0。
#[inline]
unsafe fn rdmsr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        asm!("rdmsr", out("eax") lo, out("edx") hi, in("ecx") msr, options(nostack));
    }
    ((hi as u64) << 32) | lo as u64
}

// ---------------------------------------------------------------------------
// STAR/LSTAR 看门狗（P4 elf-smoke #GP 回归保险）
// ---------------------------------------------------------------------------
//
// 根因（已修复，见上方 STAR_VALUE）：旧常量 0x0000_001B_0000_0008 把两个
// 16-bit 字段整体放低了一位——[63:48]=0（应为 0x1b）、[47:32]=0x1b（应为
// 0x08）、0x08 落到保留位 [15:0]。后果：sysretq 从 STAR[63:48]=0 算出
// CS=(0+16)|3=0x13（**kernel data 描述子**）、SS=(0+8)|3=0xb（**kernel code
// 描述子**）；sysretq **不校验描述符合法性**，用户态带着非法 CS/SS 照常运行
// （64-bit 平坦模型不看段基址），直到下一次 ring3 中断把 0x13/0xb 压入中断帧、
// handler `iretq` 查 GDT 校验 DPL/类型才 #GP（error_code 随被中断点 = 0x20/
// 0x13/0x0b）。syscall 入口因 STAR[47:32]=0x1b 指向 GDT index 3（恰为兼容
// ring0 code）而"能跑"，掩盖了 bug。ring3-smoke 经手搓 iretq 帧（硬编码
// cs=0x2b）+ kill 走 iretq-to-continuation、不经 sysretq 而幸免；真实 ELF
// 每次 syscall 都经 sysretq 返回 → 必中。
//
// 常量已归位，本看门狗转为**回归保险**：在内核入口 rdmsr 校验 STAR[63:48]==0x1b
// 且 LSTAR 未被踩，首个发现点报告一次（防未来误改常量或野指针写 MSR 再退化为
// 难查的延迟 #GP）。
static STAR_WATCHDOG_FIRED: AtomicBool = AtomicBool::new(false);

/// 校验 STAR/LSTAR 完整性；发现破坏时仅报告一次（含发现点 tag）。
pub(crate) fn star_watchdog(tag: &str) {
    let star = unsafe { rdmsr(MSR_STAR) };
    let lstar = unsafe { rdmsr(MSR_LSTAR) };
    let expect_lstar = syscall_entry_asm as *const () as u64;
    if (star >> 48 != 0x1b || lstar != expect_lstar)
        && !STAR_WATCHDOG_FIRED.swap(true, Ordering::SeqCst)
    {
        log::error!(
            "[star-diag] MSR CORRUPT first seen at [{tag}]: STAR={star:#x} (hi16 must be 0x1b) LSTAR={lstar:#x} (expect {expect_lstar:#x})"
        );
    }
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
    star_watchdog("syscall-dispatch");
    let ret = dispatch_inner(frame);
    frame.num = ret as u64;
    ret
}

fn dispatch_inner(frame: &SyscallFrame) -> i64 {
    log::info!("[syscall] dispatch: num={} ({:#x})", frame.num, frame.num);
    let Some(sc) = decode(frame) else {
        // IllegalSyscall（Doc 02 §5.3）：未知号 / 窄参数越界。不静默截断、
        // 不按 E_NOT_IMPLEMENTED 温和返回——杀进程语义。
        ILLEGAL_SYSCALL_COUNT.fetch_add(1, Ordering::SeqCst);
        log::error!(
            "[syscall] IllegalSyscall: num={} ({:#x}) args=[{:#x} {:#x} {:#x} {:#x} {:#x} {:#x}] — killing process",
            frame.num, frame.num,
            frame.args[0], frame.args[1], frame.args[2],
            frame.args[3], frame.args[4], frame.args[5],
        );
        // 真实 spawned child → 归因 IllegalSyscall + death notification + reap。
        // init / 集成 smoke 未武装 KERNEL_FRAME 的裸调用 → 走 legacy 接力。
        if crate::proc_life::current_is_spawned_child() {
            // SAFETY: spawned child 上下文，KERNEL_FRAME 已武装。
            unsafe {
                crate::proc_life::terminate_current(
                    Some(FaultKind::IllegalSyscall),
                    frame.num as i32,
                )
            }
        }
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
            // 真实 spawned child → 走 terminate_current：状态转换 Exited +
            // 释放资源 + 投递 DeathMsg + reap-able，**永不返回**。
            // init / 集成 smoke → 走 legacy handle_process_exit KERNEL_FRAME 接力。
            if crate::proc_life::current_is_spawned_child() {
                // SAFETY: spawned child 上下文，KERNEL_FRAME 已武装。
                unsafe { crate::proc_life::terminate_current(None, code) }
            }
            // FR9 审计：legacy 路径（init / 集成 smoke）的 exit 事件——
            // terminate_current 路径在其内部自记，此处只补 legacy 分支。
            // 必须在 handle_process_exit（永不返回）之前。
            {
                let pid = crate::proc_ext::current_pid();
                let agent = crate::audit::agent_of_pub(pid);
                crate::audit::process_ev(
                    agent,
                    synapse_audit::ProcOp::Exit,
                    agent,
                    u32::MAX,
                    code,
                );
            }
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
                Syscall::NotificationSignal { notif, bits } => {
                    sys_notify_signal(notif, bits)
                }
                Syscall::NotificationWait { notif, mask } => {
                    sys_notify_wait(notif, mask)
                }
                // P4-T13：capability syscall 接线（校验语义见 capsys 模块头）
                Syscall::CapInvoke { .. } => crate::capsys::k_cap_invoke(frame),
                Syscall::CapDelegate { .. } => crate::capsys::k_cap_delegate(frame),
                Syscall::CapRevoke { .. } => crate::capsys::k_cap_revoke(frame),
                Syscall::ProcessReap { pid } => {
                    // P4-T9d：reap 父进程持有的 zombie child。调用方 = caller
                    // （smoke 内是 init / spawn_continuation）；错误映射走
                    // ipc::cap_err_to_code（CapError → Doc 02 §4.3 错误码）。
                    let caller = Pid(crate::proc_ext::current_pid());
                    let target = Pid(pid);
                    match crate::proc_life::sys_reap(caller, target) {
                        Ok(()) => 0,
                        Err(e) => {
                            log::warn!(
                                "[syscall] process_reap(pid={}) failed: {:?}",
                                pid, e
                            );
                            crate::ipc::cap_err_to_code(e)
                        }
                    }
                }
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

/// 用户指针区间校验（P4-T13 起统一走 [`crate::uaccess`]；本包装保留
/// `&AddressSpace` 签名 + syscall 侧「零长即拒」语义——gettime 输出 16B
/// 定长，len==0 是调用方 bug 而非合法空缓冲）。
fn user_mem_ok(as_user: &AddressSpace, va: u64, len: u64, need_write: bool) -> bool {
    if len == 0 {
        return false;
    }
    crate::uaccess::user_mem_ok(as_user as *const AddressSpace as u64, va, len, need_write)
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

/// `notification_signal(notif, bits)` (#4) — 位图 OR 聚合（P4-T8）。
///
/// 把 `bits` 加到 notif 对象的位图（已有位保留）；无对象/无 cap/非 Notification
/// kind/无 SEND 权限 → 错误码。**MVP 围栏**：boot 线程永不禁用 → 当前直接放行
/// （signal 无须阻塞路径，无 boot fence 风险）。
fn sys_notify_signal(cptr: u8, bits: u32) -> i64 {
    let pid = crate::proc_ext::current_pid();
    let (obj, _) = match resolve_notification(pid, cptr, Rights::SEND) {
        Ok(t) => t,
        Err(code) => return code,
    };
    match crate::kstate::k_notify_signal(obj, bits as u64) {
        Ok(()) => 0,
        Err(e) => crate::ipc::cap_err_to_code(e),
    }
}

/// `notification_wait(notif, mask)` (#5) — 位图 AND + 读清（P4-T8）。
///
/// 非阻塞：当前 `word & mask` 非零 → 清除这些位并返回；否则返回 0
/// （MVP 围栏下不阻塞——per-CPU 单内核栈 + boot 是系统最后防线，
/// 真正阻塞 wait 由 T9 per-thread kstack 后接出，本期先返 0
/// 而非 E_WOULD_BLOCK 以便同进程自 signal/wait round-trip 测试）。
/// `mask = 0` 恒返 0（无意义查询）。
fn sys_notify_wait(cptr: u8, mask: u32) -> i64 {
    let pid = crate::proc_ext::current_pid();
    let (obj, _) = match resolve_notification(pid, cptr, Rights::RECV) {
        Ok(t) => t,
        Err(code) => return code,
    };
    match crate::kstate::k_notify_wait(obj, mask as u64) {
        Ok(Some(matched)) => matched as i64,
        Ok(None) => 0, // 无匹配 = 阻塞语义；MVP 围栏下返 0
        Err(e) => crate::ipc::cap_err_to_code(e),
    }
}

/// 从当前 pid 的 cap 表中解析 notification 对象（与 `ipc::resolve_endpoint`
/// 同形；分模块避免 ipc 模块头膨胀 notification 概念）。
///
/// 返回 `(ObjRef, badge)`。失败映射：cap 槽空 / 越界 / 非 Notification kind →
/// `E_INVALID_CAP`（-1）；权限不足 → `E_PERMISSION`（-7）；对象已 retired →
/// `E_OBJECT_RETIRED`（-12）。
fn resolve_notification(pid: u32, cptr: u8, need: Rights) -> Result<(synapse_cap::ObjRef, u32), i64> {
    if cptr == 0 {
        return Err(E_INVALID_CAP);
    }
    // 锁序: OBJECTS → CAP_TABLES
    crate::kstate::with_objects(|objs| {
        crate::kstate::with_cap_table(Pid(pid), |t| {
            let cap = match t.get(cptr) {
                Ok(c) => c,
                Err(_) => return Err(E_INVALID_CAP),
            };
            if !cap.rights.contains(need) {
                return Err(E_PERMISSION);
            }
            match objs.check_live(cap.obj) {
                Ok(ObjKind::Notification) => Ok((cap.obj, cap.badge)),
                Ok(_) => Err(E_INVALID_CAP),
                Err(synapse_cap::CapError::ObjectRetired) => Err(-12),
                Err(_) => Err(E_INVALID_CAP),
            }
        })
    })
}

// ---------------------------------------------------------------------------
// 首次进入用户态：构造 iretq 帧（`iretq_to_user_asm` 由 `ring3` 模块定义）
// ---------------------------------------------------------------------------