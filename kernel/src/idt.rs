//! IDT + 异常处理（P2-T5）。
//!
//! 安装所有 x86_64 必需异常 handler：
//! - #DE (0) divide_error
//! - #BP (3) breakpoint
//! - #UD (6) invalid_opcode
//! - #DF (8) double_fault (IST1 栈)
//! - #GP (13) general_protection
//! - #PF (14) page_fault (解析错误码)
//!
//! ## 已知问题与 workaround
//!
//! rustc nightly (2026-09-23) 的 `extern "x86-interrupt"` ABI codegen 对带错误码的 handler
//! 产生 "offset is not a multiple of 16" 编译错误。本模块对 #DF/#GP/#PF handler 使用 `global_asm!`
//! 在汇编层实现 trampoline，绕过 rustc 的 x86-interrupt codegen bug。

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame};

struct IdtCell(UnsafeCell<InterruptDescriptorTable>);
unsafe impl Sync for IdtCell {}

impl IdtCell {
    const fn new() -> Self {
        IdtCell(UnsafeCell::new(InterruptDescriptorTable::new()))
    }

    fn init_and_load(&self) {
        let idt = unsafe { &mut *self.0.get() };

        idt.divide_error.set_handler_fn(divide_error_handler);
        idt.breakpoint.set_handler_fn(breakpoint_handler);
        idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);

        // Double fault: 使用 set_handler_addr + global_asm! trampoline 绕过 rustc 栈对齐 bug
        unsafe {
            idt.double_fault
                .set_handler_addr(x86_64::VirtAddr::new(
                    double_fault_trampoline_asm as *const () as u64
                ))
                .set_stack_index(0); // IST1
        }

        // General protection: 同样使用 trampoline
        unsafe {
            idt.general_protection_fault
                .set_handler_addr(x86_64::VirtAddr::new(
                    general_protection_trampoline_asm as *const () as u64
                ));
        }

        // Page fault: 同样使用 trampoline
        unsafe {
            idt.page_fault
                .set_handler_addr(x86_64::VirtAddr::new(
                    page_fault_trampoline_asm as *const () as u64
                ));
        }

        // IRQ 0: 定时器中断 (PIT Channel 0) — vector 32 (IRQ_OFFSET + 0)
        // slice_mut 索引用实际 vector 号（不是相对 interrupts 数组的偏移）
        idt.slice_mut(32..33)[0].set_handler_fn(timer_interrupt_handler);

        unsafe {
            let idt_ref: &'static InterruptDescriptorTable = &*self.0.get();
            idt_ref.load();
        }
    }
}

static IDT: IdtCell = IdtCell::new();
static INIT_DONE: AtomicBool = AtomicBool::new(false);

/// 初始化并加载 IDT（安装全部异常/中断 handler）。
///
/// # Safety
///
/// 必须在 GDT/TSS（含 IST1）初始化之后、开启中断之前调用；只允许调用一次，
/// 重复调用会 panic。
pub unsafe fn init_idt() {
    if INIT_DONE.swap(true, Ordering::SeqCst) {
        panic!("init_idt called twice");
    }
    IDT.init_and_load();
    log::info!("[idt] IDT loaded: #DE/#BP/#UD/#DF(IST1)/#GP/#PF");
}

/// 读回当前 IDT 基址（smoke 验证用）。
pub fn current_idt_base() -> u64 {
    x86_64::instructions::tables::sidt().base.as_u64()
}

/// 读回当前 IDT 限长（smoke 验证用）。
pub fn current_idt_limit() -> u16 {
    x86_64::instructions::tables::sidt().limit
}

// ============================================================================
// 异常处理器（无错误码的直接使用 x86-interrupt ABI）
// ============================================================================

extern "x86-interrupt" fn divide_error_handler(stack_frame: InterruptStackFrame) {
    panic!(
        "EXCEPTION: #DE divide error at ip={:#x}",
        stack_frame.instruction_pointer.as_u64()
    );
}

extern "x86-interrupt" fn breakpoint_handler(stack_frame: InterruptStackFrame) {
    log::info!(
        "[idt] #BP breakpoint at ip={:#x}",
        stack_frame.instruction_pointer.as_u64()
    );
}

extern "x86-interrupt" fn invalid_opcode_handler(stack_frame: InterruptStackFrame) {
    panic!(
        "EXCEPTION: #UD invalid opcode at ip={:#x}",
        stack_frame.instruction_pointer.as_u64()
    );
}

// ============================================================================
// #DF/#GP/#PF handler: global_asm! trampoline workaround
// ============================================================================
//
// rustc nightly 的 x86-interrupt ABI 对带错误码的 handler 产生
// "offset is not a multiple of 16" 编译错误。用 global_asm! 在汇编层实现 trampoline，
// 调用普通 C 函数完成实际 panic 逻辑。
//
// CPU push layout (ring 0 → ring 0, no privilege change, 32 bytes):
//   [...|RFLAGS(8)|CS(8)|RIP(8)|err_code(8)] <- rsp
// For IST handlers (DF), same layout applies since exceptions originate in ring 0.
// For exceptions with privilege change (ring 3 → 0), SS/RSP(old) are pushed too:
//   [...|SS(8)|RSP(8)|RFLAGS(8)|CS(8)|RIP(8)|err_code(8)] <- rsp  (48 bytes)

extern "C" {
    fn double_fault_trampoline_asm();
    fn general_protection_trampoline_asm();
    fn page_fault_trampoline_asm();
}

// #DF trampoline
// 帧偏移修正（P4-T2）：CPU 压栈序 [SS|RSP|RFLAGS|CS|RIP|err]，err 在 [rsp+0]、
// **RIP 在 [rsp+8]**、CS 在 [rsp+16]——原实现读 +16 拿到的是 CS（此前 #DF 日志
// "ip=0x8" 即内核 CS 选择子，非真实 RIP）。
core::arch::global_asm!(
    ".global double_fault_trampoline_asm",
    "double_fault_trampoline_asm:",
    "mov rdi, [rsp + 8]",    // rdi = RIP（ring-0 异常帧中 RIP 在 [rsp+8]；[rsp+16] 是 CS）
    "mov rsi, [rsp]",        // rsi = error_code
    "and rsp, -16",
    "call double_fault_inner",
    "ud2",
);

#[no_mangle]
extern "C" fn double_fault_inner(ip: u64, error_code: u64) -> ! {
    panic!(
        "EXCEPTION: #DF double fault (error_code={:#x}) at ip={:#x}",
        error_code, ip
    );
}

// #GP trampoline（RIP 偏移修正同 #DF；P4-T9c 新增：读 CS 区分 ring0/ring3）
//
// CPU 帧（ring0 异常 / ring3 异常都相同偏移）：
//   [rsp+0]  = error_code
//   [rsp+8]  = RIP
//   [rsp+16] = CS（低 2 位 = CPL：0=ring0，3=ring3）
//
// ring0 #GP = 内核 bug → panic；ring3 #GP = 用户违规（如 ring3 执行 hlt /
// 非法 MSR 访问 / 违反段选择子规则）→ 杀进程（handle_process_exit 走
// KERNEL_FRAME iretq 回到内核延续，永不返回到 faulting 用户代码）。
core::arch::global_asm!(
    ".global general_protection_trampoline_asm",
    "general_protection_trampoline_asm:",
    "mov rdi, [rsp + 8]",    // rdi = RIP
    "mov rsi, [rsp]",        // rsi = error_code
    "mov rdx, [rsp + 16]",   // rdx = CS（低 2 位 = CPL）
    "and rsp, -16",
    "call general_protection_inner",
    "ud2",
);

#[no_mangle]
extern "C" fn general_protection_inner(ip: u64, error_code: u64, cs: u64) -> ! {
    crate::syscall::star_watchdog("gp-fault");
    if cs & 3 == 3 {
        // ring3 #GP：用户态违规（hlt / 非法 MSR / 段选择子违规等）。
        // 走 ProcessExit/IllegalSyscall 同一 kill 路径（KERNEL_FRAME iretq
        // 接力回内核延续，永不返回 faulting 用户代码）。
        log::warn!(
            "[idt] #GP ring3 user fault (error_code={:#x}) at ip={:#x} — killing process",
            error_code, ip
        );
        // SAFETY: smoke 上下文已武装 KERNEL_FRAME（未武装 = 内核契约破坏，
        // handle_process_exit 内部 panic 兜底）。
        unsafe { crate::ring3::handle_process_exit() }
    }
    panic!(
        "EXCEPTION: #GP general protection fault (error_code={:#x}) at ip={:#x}",
        error_code, ip
    );
}

// #PF trampoline（P4-T2 起支持 expected-fault 恢复；P4-T3 强化：保存 caller-saved
// GPR 后再调 inner，让需求分页路径的 faulting 指令能正确重执）
//
// page_fault_inner 返回恢复 RIP（非 0）= paging::probe_expect_pf 武装过的
// 期望内缺页，或按需分页成功（返回原 ip = 重执 faulting 指令）：改写栈帧
// 保存的 RIP 后 iretq 续跑。非期望缺页 inner 直接 panic!（发散），永不返回。
//
// **GPR 保存的必要性**：x86-64 #PF CPU 帧不含通用寄存器（仅 SS/RSP/RFLAGS/CS/RIP/err）。
// 而 C-ABI inner 可任意破坏 caller-saved（rax/rcx/rdx/rsi/rdi/r8-r11）——若 faulting
// 指令以 rax 等作内存操作数基址（如 `mov rax, [rax]`），handler 返回后 rax 已被
// 覆写为恢复点 ip，iretq 重执时 `mov rax, [rax]` 会从 ip 自身读字节而非原目标地址
// （T3 vma-smoke 首次真机暴露：read_volatile(code_base) 返回 faulting 指令自身的字节）。
// 修复：trampoline 在调 inner 前 push 9 个 caller-saved，inner 返回后 pop 复原。
//
// **r12 保存（P4-T9b 修复）**：resume RIP 需跨 9 个 pop 传递，原实现借用 callee-saved
// r12 但未保存/恢复原值——若编译器将活跃局部变量（如 paging_smoke 的 f1）分配到 r12，
// #PF handler 返回后该变量被静默覆写为 resume RIP，导致 unmap 返回值比对失败。
// 修复：push r12 与 caller-saved 一起保存，pop GPR 后再 pop r12 → rax 用于 patch。
//
// 栈纪律：入口 rsp → [err|rip|cs|rflags|rsp|ss]；先 push r12 + 9 GPR → [10 regs | cpu
// frame]，然后对齐 + call → [pad | ret | 10 regs | cpu frame]；ret 后复原 rsp，逐项
// pop GPR，再 pop r12 → rax 覆写 [frame+8]=resume、弃 err、iretq。
core::arch::global_asm!(
    ".global page_fault_trampoline_asm",
    "page_fault_trampoline_asm:",
    // 保存 callee-saved r12（用作 resume RIP 跨 pop 传递的临时寄存器，
    // 必须保存原值以防破坏调用者的活跃变量）
    "push r12",
    // 保存 caller-saved GPR（rax/rcx/rdx/rsi/rdi/r8-r11），让 faulting 指令的
    // 寄存器操作数在重执时保持原值
    "push r11",
    "push r10",
    "push r9",
    "push r8",
    "push rdi",
    "push rsi",
    "push rdx",
    "push rcx",
    "push rax",
    // 此时 [rsp] = rax (top), [rsp+80] = r12, [rsp+88] = cpu frame 的 err,
    // [rsp+96] = RIP（10 个 push = 80 字节）
    "mov rdi, [rsp + 10*8 + 8]",  // rdi = ip (RIP)
    "mov rsi, [rsp + 10*8]",      // rsi = error_code
    // 对齐 + 调 inner；rax 返回 resume RIP（0 = panic 路径）
    "mov rdx, rsp",
    "and rsp, -16",
    "sub rsp, 16",
    "mov [rsp], rdx",             // 保存对齐前 rsp（指向 rax 槽）
    "call page_fault_inner",
    "mov r12, rax",               // resume RIP 借 r12 跨 pop 传递
    "test rax, rax",
    "jz 1f",                      // 0 = 非期望/未处理 → panic 防御
    // 复原栈，逐项 pop caller-saved
    "mov rdx, [rsp]",
    "add rsp, 16",
    "mov rsp, rdx",
    "pop rax",
    "pop rcx",
    "pop rdx",
    "pop rsi",
    "pop rdi",
    "pop r8",
    "pop r9",
    "pop r10",
    "pop r11",
    // rsp → [saved_r12][err][rip][cs|rflags|rsp|ss]；r12 = resume RIP。
    // 用 xchg 把 resume 直接换进 RIP 槽（不碰任何已恢复的 GPR——此前版本
    // `mov rax, r12` 会覆写刚 pop 回来的 rax，导致重执 `mov rax,[rax]` 类
    // faulting 指令时 rax = resume RIP，从内核代码段读出指令字节当数据）。
    "xchg r12, [rsp + 16]",       // [rip 槽] = resume；r12 = 旧 rip（弃用）
    "pop r12",                    // 恢复 r12 callee-saved 原值；rsp → err 槽
    "add rsp, 8",                 // 弃 error_code → iretq 弹 RIP/CS/RFLAGS/RSP/SS
    "iretq",
    // panic 路径：仍需清理 10 GPR（r12 + 9 caller-saved）再 ud2
    "1:",
    "mov rdx, [rsp]",
    "add rsp, 16",
    "mov rsp, rdx",
    "pop rax",
    "pop rcx",
    "pop rdx",
    "pop rsi",
    "pop rdi",
    "pop r8",
    "pop r9",
    "pop r10",
    "pop r11",
    "pop r12",                    // 恢复 callee-saved r12 原值
    "ud2",
);

#[no_mangle]
extern "C" fn page_fault_inner(ip: u64, error_code: u64) -> u64 {
    crate::syscall::star_watchdog("page-fault");
    let fault_addr = Cr2::read_raw();

    // expected-fault 钩子（P4-T2 smoke 专用，见 paging.rs 模块头）：
    // 已武装 → 消费钩子返回恢复 RIP；未武装 → 落入下一阶段（按需分页/kill 骨架）。
    let resume = crate::paging::take_expected_fault(fault_addr);
    if resume != 0 {
        log::warn!(
            "[idt] #PF expected-fault resumed: fault_addr={:#x}, error_code={:#x}, ip={:#x}",
            fault_addr, error_code, ip
        );
        return resume;
    }

    // P4-T3: demand paging（VMA 命中 → 分配帧+映射+重执）或 kill 骨架（未命中/权限违例/越界）
    // 返回非 0 = resume RIP；返回 0 = 落入下方 panic（真实内核 fault）
    let resume = crate::paging::handle_user_fault(fault_addr, error_code, ip);
    if resume != 0 {
        return resume;
    }

    log::error!(
        "[idt] #PF page fault at ip={:#x}, fault_addr={:#x}, error_code={:#x}",
        ip, fault_addr, error_code
    );

    panic!(
        "EXCEPTION: #PF page fault (fault_addr={:#x}, error_code={:#x}) at ip={:#x}",
        fault_addr, error_code, ip
    );
}

// ============================================================================
// 硬件中断处理器
// ============================================================================

/// IRQ0 定时器：tick 计数 → **EOI 先行** → 中断返回边界调度检查点（P3-T6）。
///
/// EOI 必须先于 `on_timer_irq`：检查点可能把本线程切走（xv6 同模型，详见
/// kthread.rs P3-T6 模块头）——若切换发生在 EOI 之前，PIC 仍处于 IRQ0 屏蔽
/// 状态，接管线程将永远收不到下一次定时器中断（调度死锁）。
extern "x86-interrupt" fn timer_interrupt_handler(_stack_frame: InterruptStackFrame) {
    crate::pit::timer_interrupt_handler();
    unsafe {
        crate::pic::send_eoi(0);
    }
    crate::kthread::on_timer_irq();
}
