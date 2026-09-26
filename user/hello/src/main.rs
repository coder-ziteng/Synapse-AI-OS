//! 第一个静态用户态 ELF（P4-T1 交付物）。
//!
//! 目标不是功能，而是**打通用户态工具链**：
//! no_std + 自定义 target（`x86_64-synapse-user.json`）+ 专用链接脚本
//! （`user/hello/linker.ld`，基址 0x4000_0000 = 1GB，见脚本头注释的
//! 基址改址原因）→ 产出可被 P4-T5 内核 ELF 加载器
//! 消费的 ET_EXEC 产物（xtask user 子命令做 ELF 头断言）。
//!
//! 三段式主体（对齐 task.json P4-T1 deliverables）：
//! 1. `abi_query` 占位：ABI 版本协商（内核侧接线在 P4-T6；当前 syscall
//!    会落入内核未处理路径，返回值仅记录、不断言）；
//! 2. `process_exit` 占位：请求内核终止本进程；
//! 3. 死循环兜底：exit 未接线前 `hlt` 等待。注意 ring3 执行 `hlt` 会触发
//!    #GP——这是**期望行为**（P4-T4 接线后走 fault 路径杀进程，不伤内核）。
//!
//! 内存约定：无栈溢出保护（P4-T3 VMA 提供 stack region + guard page 前，
//! 本 bin 栈由加载方静态划定，只做叶调用级操作）。

#![no_std]
#![no_main]

use core::arch::asm;
use core::panic::PanicInfo;
use synapse_abi::SyscallId;

/// 用户态入口（`user/linker.ld` 中 `ENTRY(_start)`，e_entry 指向此处）。
#[no_mangle]
pub extern "C" fn _start() -> ! {
    // ① abi_query：返回 (ABI_MAJOR << 16) | ABI_MINOR（Doc 02 §4.2）。
    let abi = unsafe { synapse_user::invoke(SyscallId::AbiQuery as u64, [0; 6]) };
    let _abi_major = ((abi >> 16) & 0xFFFF) as u16;
    let _abi_minor = (abi & 0xFFFF) as u16;
    // P4-T10 起：major != synapse_abi::ABI_MAJOR → 拒绝运行。
    // T1 阶段内核尚未接线 syscall 分发，不做断言。

    // ② exit 占位：请求内核终止本进程（接线在 P4-T6）。
    unsafe { synapse_user::invoke(SyscallId::ProcessExit as u64, [0; 6]) };

    // ③ 死循环兜底。
    loop {
        unsafe { asm!("hlt") };
    }
}

/// panic = abort（target json 约定）：用户态 panic 直接停机循环，
/// 由内核 fault/timeout 路径收尸（P4-T9）。不打印（无 stdout，
/// 早期调试走 syscall 日志是后续任务）。
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { asm!("hlt") };
    }
}
