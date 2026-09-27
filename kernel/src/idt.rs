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

// #GP trampoline（RIP 偏移修正同 #DF）
core::arch::global_asm!(
    ".global general_protection_trampoline_asm",
    "general_protection_trampoline_asm:",
    "mov rdi, [rsp + 8]",    // rdi = RIP
    "mov rsi, [rsp]",        // rsi = error_code
    "and rsp, -16",
    "call general_protection_inner",
    "ud2",
);

#[no_mangle]
extern "C" fn general_protection_inner(ip: u64, error_code: u64) -> ! {
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
// 修复：trampoline 在调 inner 前 push 9 个 caller-saved，inner 返回后 pop 复原；
// resume RIP 借 callee-saved r12 跨调用传递。
//
// 栈纪律：入口 rsp → [err|rip|cs|rflags|rsp|ss]；先 push 9 GPR → [9 regs | cpu frame]，
// 然后对齐 + call → [pad | ret | 9 regs | cpu frame]；ret 后复原 rsp，逐项 pop GPR，
// 再覆写 [frame+8]=resume、弃 err、iretq。
core::arch::global_asm!(
    ".global page_fault_trampoline_asm",
    "page_fault_trampoline_asm:",
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
    // 此时 [rsp] = rax (top), [rsp+72] = cpu frame 的 err, [rsp+80] = RIP
    "mov rdi, [rsp + 9*8 + 8]",   // rdi = ip (RIP)
    "mov rsi, [rsp + 9*8]",       // rsi = error_code
    // 对齐 + 调 inner；rax 返回 resume RIP（0 = panic 路径）
    "mov rdx, rsp",
    "and rsp, -16",
    "sub rsp, 16",
    "mov [rsp], rdx",             // 保存对齐前 rsp（指向 rax 槽）
    "call page_fault_inner",
    "mov r12, rax",               // resume RIP 借 callee-saved r12 跨 pop 传递
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
    // 此时 rsp → cpu frame（[err|rip|cs|rflags|rsp|ss]）
    "mov [rsp + 8], r12",         // 覆写保存的 RIP = resume
    "add rsp, 8",                 // 弃 error_code → iretq 弹 RIP/CS/RFLAGS/RSP/SS
    "iretq",
    // panic 路径：仍需清理 9 GPR 再 ud2（让 #UD handler 拿到干净栈）
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
    "ud2",
);

#[no_mangle]
extern "C" fn page_fault_inner(ip: u64, error_code: u64) -> u64 {
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
