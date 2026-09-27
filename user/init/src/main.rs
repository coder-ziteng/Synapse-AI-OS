//! P4-T10 用户态 init 进程（Root Agent）。
//!
//! ## 职责（最小集合）
//!
//! 1. **`abi_query()`** — 版本协商：major 不匹配即拒绝运行（Doc 02 §4.5，
//!    编译期常量保证 ABI_MAJOR 与内核一致；minor 漂移是兼容的）。
//! 1.5 **cap syscall 证据链（P4-T13）** — ring3 走通 cap_delegate /
//!    cap_invoke / cap_revoke 三个 syscall 的正/反路径（衰减委托、非法
//!    cptr、无 GRANT 撤销拒绝、级联撤销后 handle 失效）。
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

use synapse_user::{abi_query, cap_delegate, cap_invoke, cap_revoke, exit, gettime,
    clock_id::MONOTONIC, CapRef, SynapseError};
use synapse_abi::Timespec;

// Rights 原始位（与 synapse_cap::Rights 同值，Doc 01 §4）：
// SEND=bit0, RECV=bit1, REPLY=bit2, GRANT=bit6。
const R_SEND: u32 = 1 << 0;
const R_GRANT: u32 = 1 << 6;

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

    // ---- 1.5 cap syscall 证据链（P4-T13：invoke/delegate/revoke ring3 走通）----
    // bootstrap 根 cap：slot 1 = endpoint（SEND|RECV|REPLY|GRANT）。
    let root = CapRef::new(1).expect("[init] FAIL: bad root cptr");

    // (a) 衰减委托：root(GRANT) → child(SEND-only)。child cptr 由内核写入栈变量。
    let mut child: u8 = 0;
    unsafe {
        cap_delegate(root, R_SEND, &mut child)
            .expect("[init] FAIL: cap_delegate(SEND) from root should succeed");
    }
    assert!(child != 0, "[init] FAIL: child cptr not written");

    // (b) 非法 cptr → 稳定 E_INVALID_CAP(-1)，绝不 panic（Phase 4 出口判据）。
    let bogus = CapRef::new(200).expect("[init] FAIL: bad bogus cptr");
    let r = unsafe { cap_invoke(bogus, 0, core::ptr::null()) };
    assert_eq!(
        r,
        Err(SynapseError(-1)),
        "[init] FAIL: invoke illegal cptr should be E_INVALID_CAP"
    );

    // (c) child 无 GRANT → revoke 拒绝 E_PERMISSION(-7)（attenuation 生效证据）。
    let child_ref = CapRef::new(child as u16).expect("[init] FAIL: bad child cptr");
    assert_eq!(
        cap_revoke(child_ref),
        Err(SynapseError(-7)),
        "[init] FAIL: revoke without GRANT should be E_PERMISSION"
    );

    // (d) 合法 cap 的 invoke：校验通过但 MVP 无对象 op 表 → E_NOT_IMPLEMENTED(-10)。
    let r = unsafe { cap_invoke(root, 0, core::ptr::null()) };
    assert_eq!(
        r,
        Err(SynapseError(-10)),
        "[init] FAIL: invoke valid cap should pass validation then E_NOT_IMPLEMENTED"
    );

    // (e) 带 GRANT 的委托 → revoke 级联成功 → 旧 handle 稳定失效 E_INVALID_CAP(-1)。
    let mut child2: u8 = 0;
    unsafe {
        cap_delegate(root, R_SEND | R_GRANT, &mut child2)
            .expect("[init] FAIL: cap_delegate(SEND|GRANT) should succeed");
    }
    let child2_ref = CapRef::new(child2 as u16).expect("[init] FAIL: bad child2 cptr");
    cap_revoke(child2_ref).expect("[init] FAIL: revoke with GRANT should succeed");
    let r = unsafe { cap_invoke(child2_ref, 0, core::ptr::null()) };
    assert_eq!(
        r,
        Err(SynapseError(-1)),
        "[init] FAIL: revoked handle should be E_INVALID_CAP"
    );

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