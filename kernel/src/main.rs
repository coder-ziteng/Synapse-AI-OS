//! Synapse kernel binary entry point.
//!
//! Phase 1（P1-T2 + P1-T2-bis）实现：
//! * `boot.S` trampoline：符号 `_start`，从 QEMU PVH 加载（32-bit 保护模式）开始，
//!   完成 4 级页表建立 + 切换到 64-bit 长模式 + 跳到 `_start64`。
//! * `_start64`（64-bit Rust）初始化 UART 16550 + 打印 "Hello, Synapse!" + 死循环。
//!
//! 入口约定：
//! * 链接器 entry 是 `_start`（在 `boot.S` 中定义）。
//! * `_start64` 由 trampoline 远跳到达。
//! * 不返回；崩溃走 [`panic_handler`]。

#![no_std]
#![no_main]

use core::panic::PanicInfo;
use core::arch::global_asm;

pub mod serial;

// 把 trampoline 汇编链入二进制；`boot.S` 中 `.global _start` 提供链接器 entry。
global_asm!(include_str!("boot.S"));

/// Kernel 64-bit 入口（长模式 + 4 级页表已建、栈有效）。
///
/// 由 `boot.S` trampoline 长跳过来；本函数是 Rust 64-bit 代码的真正起点。
///
/// # Safety
///
/// * 调用方必须保证：CPL=0、长模式已开、4 级页表已建、栈有效。
/// * 本函数永不返回 (`-> !`)。
#[no_mangle]
pub extern "C" fn _start64() -> ! {
    // 紧贴入口就写 debug-exit，看是否能跑通整个链路；UART 在串口测试
    unsafe {
        core::arch::asm!(
            "mov dx, 0x501",
            "mov al, 0x41",      // 0x41 = 'A' for "Arrived in _start64"
            "out dx, al",
            options(nostack, preserves_flags),
        );
    }

    serial::init();
    kprintln!("Hello, Synapse!");

    // 进入低功耗停机；Phase 3 起改为调度器就绪队列等待。
    loop {
        x86_64::instructions::hlt();
    }
}

/// Panic handler。
///
/// P1-T2 版本：仅打印位置信息 + 死循环。
/// P1-T5 升级为带栈回转的完整版。
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kprintln!("[PANIC] {}", info);
    loop {
        x86_64::instructions::hlt();
    }
}
