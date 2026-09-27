//! P4-T7 Phase 2 ipc-pong 用户态 bin。
//!
//! 目的：把 ring3 → kernel IPC send 路径真机贯通，作为 Phase 2 收口的
//! "ring3 进程能发 IPC" 端到端证据。
//!
//! ## 程序行为
//!
//! 1. `abi_query()` 版本协商（major 不匹配即 panic → ring3 hlt #GP → exit 355）
//! 2. `ipc_try_send(ep_cap=1, "init-ping\n", 9)` —— ep_cap slot 1 由 ipc_pong_smoke
//!    铸造（与 hello §6 / spawn_smoke 同构）；MVP 下无 receiver → 期望返 0
//!    （Queued 路径，P4-T7 单 kthread smoke 验证过）
//! 3. `process_exit(0)` —— KERNEL_FRAME 接力到 ipc_pong_continuation，
//!    continuation 端断言内核侧 `ipc_try_send_count() >= 1`（ring3 send
//!    路径证据；子 AS 已随 exit 释放，AUX.senders payload 不可再读）
//!
//! ## 失败语义
//!
//! - 任何 assert 失败 → panic（=abort）→ ring-3 停机循环触发 #GP/#UD
//!   → 内核 panic → QEMU exit **355**（区别于全过 363）
//!
//! ## 与 hello §6 的关系
//!
//! hello §6 boot fence（6.1/6.2 blocking send/recv → E_WOULD_BLOCK）保留，
//! 因为 hello 是 boot thread 跑的单用户进程，无法 spawn 对端。本 bin 是
//! ipc_pong_smoke 内核接力 spawn 出来的 **child 进程**，持独立 kstack +
//! CR3，不在 boot fence 内。
//!
//! ## Phase 2 暂未覆盖（待 T14）
//!
//! - **真双向并发 send/recv**：本 bin 走 try_send（无 receiver → Queued）；
//!   双进程互发需 idle 线程（T14）+ 并发 spawn API（P9.5），不在 T7 范围。
//! - **blocking ipc_send/recv**：单 child 阻塞时无可调度线程（init kernel
//!   不是 kthread）= idle panic。kthread 级阻塞唤醒由 kthread_ipc_smoke
//!   真机覆盖（task.json T7 actual_approach）。

#![no_std]
#![no_main]

use core::arch::asm;
use core::panic::PanicInfo;

use synapse_user::{abi_query, exit, ipc_try_send, CapRef};

/// 用户态入口（`user/ipc-pong/linker.ld` 中 `ENTRY(_start)`）。
#[no_mangle]
pub extern "C" fn _start() -> ! {
    // ---- 1. abi_query 版本协商 ----
    let ver = abi_query().expect("[ipc-pong] FAIL: abi_query syscall error");
    assert_eq!(
        ver.major,
        synapse_abi::ABI_MAJOR,
        "[ipc-pong] FAIL: ABI major mismatch",
    );

    // ---- 2. ipc_try_send(ep_cap=1, "init-ping\n", 10) → 期望 Ok（Queued 路径）----
    let payload: [u8; 10] = *b"init-ping\n";
    let ep = CapRef::new(1).unwrap();
    // 注意：ring-3 不做 UART 直写（IOPL=0 且无 TSS IO bitmap → outb 即 #GP
    // → terminate_current(GeneralProtection) → death fault=5 ≠ FAULT_NONE，
    // continuation 断言必炸）。成功路径零输出，证据由内核侧
    // ipc_try_send_count 前后差值承担（ipc_pong_smoke 记录 baseline）。
    let r = unsafe { ipc_try_send(ep, payload.as_ptr(), payload.len()) };
    assert!(r.is_ok(), "[ipc-pong] FAIL: ipc_try_send err (expected Queued=Ok)");

    // ---- 3. process_exit(0)：KERNEL_FRAME 接力到 ipc_pong_continuation ----
    exit(0);
}

// ============================================================================
// 用户态 print 帮助函数（无 std，走 16550 UART 直写）
// ============================================================================
//
// 与 hello/init/crash 同款实现：避免引入完整 std::io::Write —— 本进程只打印
// 少量字符串。内核串口 UART=0x3F8（Doc 02 §5.2），写寄存器即可输出字符。

const COM1: u16 = 0x3F8;

#[inline]
unsafe fn outb(port: u16, val: u8) {
    asm!("out dx, al", in("dx") port, in("al") val, options(nostack, preserves_flags));
}

fn putc(b: u8) {
    unsafe {
        outb(COM1, b);
    }
}

fn print_str(s: &str) {
    for &b in s.as_bytes() {
        putc(b);
    }
}

/// panic handler：与 hello/init/crash 同款 — 死循环触发 ring3 hlt #GP，
/// 让内核 panic → QEMU exit 355。
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    print_str("[ipc-pong] PANIC\n");
    loop {
        unsafe { asm!("hlt") };
    }
}
