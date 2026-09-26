//! IDT + 异常处理（P2-T5）。
//!
//! 安装所有 x86_64 必需异常 handler：
//! - #DE (0) divide_error
//! - #BP (3) breakpoint
//! - #UD (6) invalid_opcode
//! - #DF (8) double_fault (IST1 栈)
//! - #GP (13) general_protection
//! - #PF (14) page_fault (解析错误码)

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

use x86_64::structures::idt::{
    InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode,
};

struct IdtCell(UnsafeCell<InterruptDescriptorTable>);
unsafe impl Sync for IdtCell {}

impl IdtCell {
    const fn new() -> Self {
        IdtCell(UnsafeCell::new(InterruptDescriptorTable::new()))
    }

    fn init_and_load(&self) {
        let idt = unsafe { &mut *self.0.get() };

        // 基础异常
        idt.divide_error.set_handler_fn(divide_error_handler);
        idt.breakpoint.set_handler_fn(breakpoint_handler);
        idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);

        // Double fault: 使用 naked trampoline 绕过 rustc 栈对齐 bug
        unsafe {
            idt.double_fault
                .set_handler_fn(double_fault_naked)
                .set_stack_index(0); // IST1
        }

        // General protection fault
        idt.general_protection_fault.set_handler_fn(general_protection_handler);

        // Page fault
        idt.page_fault.set_handler_fn(page_fault_handler);

        unsafe {
            let idt_ref: &'static InterruptDescriptorTable = &*self.0.get();
            idt_ref.load();
        }
    }
}

static IDT: IdtCell = IdtCell::new();
static INIT_DONE: AtomicBool = AtomicBool::new(false);

pub unsafe fn init_idt() {
    if INIT_DONE.swap(true, Ordering::SeqCst) {
        panic!("init_idt called twice");
    }
    IDT.init_and_load();
    log::info!("[idt] IDT loaded: #DE/#BP/#UD/#DF(IST1)/#GP/#PF");
}

// ============================================================================
// 异常处理器
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
// #DF handler: naked trampoline workaround
// ============================================================================
//
// rustc nightly 的 x86-interrupt ABI 对 (InterruptStackFrame, u64) -> ! 签名产生
// "offset is not a multiple of 16" 编译错误。用 naked asm 绕过：
// CPU push 48 bytes (SS+RSP+RFLAGS+CS+RIP+err_code)，栈已 16 对齐，
// 我们只需对齐后调用普通 C fn 即可。

#[naked]
#[no_mangle]
unsafe extern "x86-interrupt" fn double_fault_naked(
    _stack_frame: InterruptStackFrame,
    _error_code: u64,
) -> ! {
    core::arch::asm!(
        // Stack at entry: [...|SS|RSP|RFLAGS|CS|RIP|err_code] <- rsp
        // RIP is at [rsp + 16], err_code is at [rsp]
        "mov rdi, [rsp + 16]",   // rdi = RIP
        "mov rsi, [rsp]",        // rsi = error_code
        "and rsp, -16",          // align stack to 16
        "call double_fault_inner",
        "ud2",
        options(noreturn)
    )
}

#[no_mangle]
extern "C" fn double_fault_inner(ip: u64, error_code: u64) -> ! {
    panic!("EXCEPTION: #DF double fault (error_code={:#x}) at ip={:#x}", error_code, ip);
}

extern "x86-interrupt" fn general_protection_handler(
    stack_frame: InterruptStackFrame,
    error_code: u64,
) {
    panic!(
        "EXCEPTION: #GP general protection fault (error_code={:#x}) at ip={:#x}",
        error_code,
        stack_frame.instruction_pointer.as_u64()
    );
}

extern "x86-interrupt" fn page_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    // CR2 = 触发 #PF 的线性地址；可能非规范（罕见），用 read_raw 跳过 canonical 检查
    let fault_addr = Cr2::read_raw();

    log::error!(
        "[idt] #PF page fault at ip={:#x}, fault_addr={:#x}, error_code={:?}",
        stack_frame.instruction_pointer.as_u64(),
        fault_addr,
        error_code
    );

    panic!(
        "EXCEPTION: #PF page fault (fault_addr={:#x}, error_code={:?}) at ip={:#x}",
        fault_addr,
        error_code,
        stack_frame.instruction_pointer.as_u64()
    );
}
