//! P4-T10 用户态 init 进程（Root Agent）。
//!
//! ## 职责（最小集合）
//!
//! 1. **`abi_query()`** — 版本协商：major 不匹配即拒绝运行（Doc 02 §4.5，
//!    编译期常量保证 ABI_MAJOR 与内核一致；minor 漂移是兼容的）。
//! 2. **`gettime(MONOTONIC)`** — 读内核单调钟，验证 FR2；打印 sec/ns。
//!    MVP 单进程无独立 service 进程，gettime 服务由内核直接提供（capability
//!    service 化在 Phase 5 service 阶段补：init 经 IPC 向 time-server 请求，
//!    time-server 持 cap 转发 gettime；本期直连 syscall 即可，IPC 路径在
//!    hello §6/§8 已覆盖）。
//! 3. **`process_exit(0)`** — 移交控制权回内核。
//!
//! ## 与 hello 的差异
//!
//! | 项 | hello | init |
//! |---|---|---|
//! | 定位 | syscall 全路径回归测试 | Root Agent（启动链首进程）|
//! | syscall 覆盖 | 19 个全路径 | 3 个（abi/gettime/exit） |
//! | mmap/IPC/Notification | 全跑 | 不跑（避免与 hello 重复） |
//! | 失败信号 | 触发 ring3 hlt #GP → QEMU exit 355 | 同上 |
//!
//! ## 关于"无 UART 输出"
//!
//! init 不打印到串口——`outb` 指令在 ring-3 触发 #GP。验证手段改走：
//! - **FR8 账本**：init 退出后内核续体比对基线，归零即无泄漏；
//! - **abi_query_count**：内核侧 ≥1 表示 init 真实执行过 syscall；
//! - **gettime 返回值**：Result<(), _> = Ok 表示内核成功写回 Timespec；
//! - **abort 反证**：`expect()` 触发 panic → ring3 hlt → #GP → 内核 panic
//!   → QEMU exit 355（区别于全过 363）。
//!
//! 串口日志阶段交付由内核侧 `init_continuation` 输出 "[init-smoke] PASS —
//! init (Root Agent) ran abi_query + gettime + process_exit" 即可。

#![no_std]
#![no_main]

use core::arch::asm;
use core::panic::PanicInfo;

use synapse_user::{abi_query, exit, gettime, clock_id::MONOTONIC};
use synapse_abi::Timespec;

/// 用户态入口（`user/init/linker.ld` 中 `ENTRY(_start)`）。
#[no_mangle]
pub extern "C" fn _start() -> ! {
    // ---- 1. abi_query 版本协商 ----
    let ver = abi_query().expect("[init] FAIL: abi_query syscall error");
    // major 不匹配立即拒绝（Doc 02 §4.5：编译期常量保证 ABI_MAJOR==0）
    assert_eq!(
        ver.major,
        synapse_abi::ABI_MAJOR,
        "[init] FAIL: ABI major mismatch (kernel={}, build={})",
        ver.major,
        synapse_abi::ABI_MAJOR,
    );
    // minor 仅作"消费过的版本"语义（无 UART，靠后续 FR8 + 续体 PASS 间接证明）

    // ---- 2. gettime(MONOTONIC) → 校验返回值 ----
    let mut ts = Timespec::default();
    unsafe {
        gettime(MONOTONIC, &mut ts).expect("[init] FAIL: gettime syscall error");
    }
    // nsec 域合理性检查（< 1e9；过大即内核折算 bug）
    assert!(
        ts.nsec < 1_000_000_000,
        "[init] FAIL: nsec out of range: {}",
        ts.nsec,
    );

    // ---- 3. process_exit(0)（`exit` 返回 `!`，函数在此发散）----
    exit(0);
}

/// panic = abort：停机循环 → ring-3 `hlt` #GP → 内核 panic → QEMU exit 355。
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { asm!("hlt") };
    }
}