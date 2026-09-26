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
// CPU push layout (48 bytes, 16-aligned):
//   [...|SS(8)|RSP(8)|RFLAGS(8)|CS(8)|RIP(8)|err_code(8)] <- rsp

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
    "mov rdi, [rsp + 8]",    // rdi = instruction_pointer (RIP)
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
    "mov rdi, [rsp + 8]",
    "mov rsi, [rsp]",
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

// #PF trampoline（P4-T2 起支持 expected-fault 恢复）
//
// page_fault_inner 返回恢复 RIP（非 0）= paging::probe_expect_pf 武装过的
// 期望内缺页：改写栈帧保存的 RIP 后 iretq 续跑（RIP 偏移修正同 #DF：+8）。
// 非期望缺页 inner 直接 panic!（发散），永不返回——`ud2` 兜底防御。
//
// 栈纪律：入口 rsp → [err|rip|cs|rflags|rsp|ss]；保存原 rsp 到对齐栈上，
// inner 返回后精确还原，再覆写 [frame+8]=resume、弃 err、iretq。
core::arch::global_asm!(
    ".global page_fault_trampoline_asm",
    "page_fault_trampoline_asm:",
    "mov rdi, [rsp + 8]",    // ip (RIP)
    "mov rsi, [rsp]",        // error_code
    "mov rdx, rsp",          // frame ptr（第 3 参，未用但便于调试断点）
    "and rsp, -16",
    "sub rsp, 16",
    "mov [rsp], rdx",        // 保存原 frame rsp
    "call page_fault_inner",
    "test rax, rax",
    "jz 1f",                 // rax==0（契约上不可能：非期望路径已 panic）→ 防御
    "mov rdx, [rsp]",
    "add rsp, 16",
    "mov rsp, rdx",          // 还原原始 frame rsp
    "mov [rsp + 8], rax",    // 覆写保存的 RIP = 恢复点
    "add rsp, 8",            // 弃 error_code → iretq 弹 RIP/CS/RFLAGS/RSP/SS
    "iretq",
    "1:",
    "ud2",
);

#[no_mangle]
extern "C" fn page_fault_inner(ip: u64, error_code: u64) -> u64 {
    let fault_addr = Cr2::read_raw();

    // expected-fault 钩子（P4-T2 smoke 专用，见 paging.rs 模块头）：
    // 已武装 → 消费钩子返回恢复 RIP；未武装 → 返回 0 走下方 panic。
    let resume = crate::paging::take_expected_fault(fault_addr);
    if resume != 0 {
        log::warn!(
            "[idt] #PF expected-fault resumed: fault_addr={:#x}, error_code={:#x}, ip={:#x}",
            fault_addr, error_code, ip
        );
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

extern "x86-interrupt" fn timer_interrupt_handler(_stack_frame: InterruptStackFrame) {
    crate::pit::timer_interrupt_handler();
    unsafe {
        crate::pic::send_eoi(0);
    }
}
