//! P4-T6/T7 syscall 测试程序（initramfs `hello`）。
//!
//! 从 P4-T1 三段式占位升级为**7 syscall 全路径验证**（task.json P4-T6/T7
//! verify：成功路径 + 每个错误路径负错误码断言）：
//!
//! | # | syscall | 覆盖 |
//! | --- | --- | --- |
//! | 1 | `abi_query`(18) | 返回 `(MAJOR<<16)|MINOR` = 0x3（0.3） |
//! | 2 | `yield`(25) | 成功返回 0（接 P3 调度器） |
//! | 3 | `gettime`(30) | MONOTONIC 成功 + nsec 值域 + 两次调用单调不减；WALL → -10；未知钟 → -5；坏指针/只读页 → -2 |
//! | 4 | `mmap`(40) | 内核选址 RW/RX/GROWABLE + 显式地址；写读回环；len=0/未对齐/越窗/重叠 → -2；W+X/无 R/未知 prot 位 → -7；未知 flags 位 → -10 |
//! | 5 | `munmap`(41) | 精确解除成功；重复/部分/未登记 → -5；未对齐/len=0 → -2 |
//! | 6 | `ipc_try_send`(3) / `ipc_send`(0) / `ipc_recv`(1) | T7a：try_send 无 receiver → Queued 0；cptr=0/越界/n_caps 超限 → -1；blocking send/recv 在 boot 围栏下 → -4 |
//! | 7 | `process_exit`(21) | code=0 终结（内核 KERNEL_FRAME iretq 接力 elf_continuation） |
//!
//! 失败语义：任何 assert 失败 → panic（=abort）→ ring-3 停机循环触发
//! #GP/#UD → 内核 panic → QEMU exit **355**（区别于全过 363）。
//!
//! IllegalSyscall（未知号）路径不在本程序测——触发即被杀，无法继续后续
//! 断言；由 ring3 stub（#999）覆盖，内核侧 illegal_syscall_count()==1 断言。

#![no_std]
#![no_main]

use core::arch::asm;
use core::panic::PanicInfo;

use synapse_abi::{
    abi_query_value, SyscallId, Timespec, CLOCK_MONOTONIC, CLOCK_WALL, E_INVALID_ADDR,
    E_INVALID_CAP, E_NO_MEMORY, E_NOT_FOUND, E_NOT_IMPLEMENTED, E_PERMISSION, E_WOULD_BLOCK,
    MAP_GROWABLE, PROT_EXEC, PROT_READ, PROT_WRITE,
};
use synapse_user::invoke;

const RW: u64 = (PROT_READ | PROT_WRITE) as u64;
const RX: u64 = (PROT_READ | PROT_EXEC) as u64;

/// 便利宏：invoke(id, args) 简写。
macro_rules! sys {
    ($id:expr, [$($a:expr),* $(,)?]) => {
        unsafe { invoke($id.num(), [$($a as u64),*]) }
    };
}

/// 用户态入口（`user/hello/linker.ld` 中 `ENTRY(_start)`）。
#[no_mangle]
pub extern "C" fn _start() -> ! {
    // ---- 1. abi_query：版本协商（P4-T6 起 minor=2：错误码/Timespec/prot 位入 crate）
    let v = sys!(SyscallId::AbiQuery, [0, 0, 0, 0, 0, 0]);
    assert_eq!(v as u64, abi_query_value(), "abi_query value");
    assert_eq!((v >> 16) as u16, synapse_abi::ABI_MAJOR, "abi major");

    // ---- 2. yield：调度器让出，成功返回 0
    assert_eq!(sys!(SyscallId::Yield, [0, 0, 0, 0, 0, 0]), 0, "yield");

    // ---- 3. gettime
    let mut ts1 = Timespec::default();
    let r = sys!(SyscallId::GetTime, [CLOCK_MONOTONIC, &mut ts1 as *mut _ as u64, 0, 0, 0, 0]);
    assert_eq!(r, 0, "gettime monotonic");
    assert!(ts1.nsec < 1_000_000_000, "nsec range");
    let mut ts2 = Timespec::default();
    let r = sys!(SyscallId::GetTime, [CLOCK_MONOTONIC, &mut ts2 as *mut _ as u64, 0, 0, 0, 0]);
    assert_eq!(r, 0, "gettime monotonic 2nd");
    assert!((ts2.sec, ts2.nsec) >= (ts1.sec, ts1.nsec), "monotonic non-decreasing");
    // 墙钟延后（RTC 未接）→ E_NOT_IMPLEMENTED
    assert_eq!(
        sys!(SyscallId::GetTime, [CLOCK_WALL, &mut ts2 as *mut _ as u64, 0, 0, 0, 0]),
        E_NOT_IMPLEMENTED,
        "gettime wall -> -10"
    );
    // 未知 clock_id → E_NOT_FOUND
    assert_eq!(
        sys!(SyscallId::GetTime, [7, &mut ts2 as *mut _ as u64, 0, 0, 0, 0]),
        E_NOT_FOUND,
        "gettime unknown clock -> -5"
    );
    // 坏指针（NULL guard 区，未映射）→ E_INVALID_ADDR
    assert_eq!(sys!(SyscallId::GetTime, [CLOCK_MONOTONIC, 0x1000, 0, 0, 0, 0]), E_INVALID_ADDR, "gettime bad ptr -> -2");
    // 只读页（本 ELF 代码段 RX 无 W）→ E_INVALID_ADDR
    assert_eq!(sys!(SyscallId::GetTime, [CLOCK_MONOTONIC, 0x4000_0000, 0, 0, 0, 0]), E_INVALID_ADDR, "gettime RO ptr -> -2");

    // ---- 4. mmap 成功路径
    // 4a. 内核选址 RW 2 页 + eager 零页写读回环
    let a1 = sys!(SyscallId::Mmap, [0, 0x2000, RW, 0, 0, 0]);
    assert!(a1 >= 0x4100_0000, "mmap RW arena addr");
    unsafe {
        let p = a1 as *mut u64;
        assert_eq!(p.read_volatile(), 0, "mmap eager zero page");
        p.write_volatile(0xDEAD_BEEF_CAFE_1234);
        assert_eq!(p.read_volatile(), 0xDEAD_BEEF_CAFE_1234, "mmap RW roundtrip");
    }
    // 4b. RX 1 页（可执行只读；W^X 合法组合）
    let a2 = sys!(SyscallId::Mmap, [0, 0x1000, RX, 0, 0, 0]);
    assert!(a2 >= a1 + 0x2000, "mmap RX addr above a1");
    // 4c. RW + GROWABLE（堆语义 flag 透传 VMA）
    let a3 = sys!(SyscallId::Mmap, [0, 0x1000, RW, MAP_GROWABLE, 0, 0]);
    assert!(a3 >= a2 + 0x1000, "mmap GROWABLE addr");
    // 4d. 显式地址（arena 上方 16MB 处，页对齐、无重叠）
    let hint = ((a3 as u64 + 0x10_0000) & !0xFFF) as i64;
    let a4 = sys!(SyscallId::Mmap, [hint as u64, 0x1000, RW, 0, 0, 0]);
    assert_eq!(a4, hint, "mmap explicit addr honored");

    // 4e. mmap 错误路径
    assert_eq!(sys!(SyscallId::Mmap, [0, 0, RW, 0, 0, 0]), E_INVALID_ADDR, "mmap len=0 -> -2");
    assert_eq!(sys!(SyscallId::Mmap, [0, 0x1001, RW, 0, 0, 0]), E_INVALID_ADDR, "mmap unaligned len -> -2");
    assert_eq!(sys!(SyscallId::Mmap, [1, 0x1000, RW, 0, 0, 0]), E_INVALID_ADDR, "mmap unaligned addr -> -2");
    assert_eq!(sys!(SyscallId::Mmap, [0x8000_0000u64, 0x1000, RW, 0, 0, 0]), E_INVALID_ADDR, "mmap out-of-window -> -2");
    assert_eq!(sys!(SyscallId::Mmap, [a1 as u64, 0x1000, RW, 0, 0, 0]), E_INVALID_ADDR, "mmap overlap -> -2");
    assert_eq!(
        sys!(SyscallId::Mmap, [0, 0x1000, (PROT_WRITE | PROT_EXEC) as u64, 0, 0, 0]),
        E_PERMISSION,
        "mmap W+X -> -7"
    );
    assert_eq!(sys!(SyscallId::Mmap, [0, 0x1000, 0, 0, 0, 0]), E_PERMISSION, "mmap no-R -> -7");
    assert_eq!(sys!(SyscallId::Mmap, [0, 0x1000, 0x80, 0, 0, 0]), E_PERMISSION, "mmap unknown prot bit -> -7");
    assert_eq!(sys!(SyscallId::Mmap, [0, 0x1000, RW, 0x100, 0, 0]), E_NOT_IMPLEMENTED, "mmap unknown flags bit -> -10");
    // 撞已映射页（本 ELF 代码段基址，无 VMA 但 map_page AlreadyMapped）→ -2
    assert_eq!(sys!(SyscallId::Mmap, [0x4000_0000u64, 0x1000, RW, 0, 0, 0]), E_INVALID_ADDR, "mmap onto ELF text -> -2");
    // 巨型 len 超 arena → E_NO_MEMORY（2GB 窗口装不下 1GB 请求）
    assert_eq!(sys!(SyscallId::Mmap, [0, 0x4000_0000u64, RW, 0, 0, 0]), E_NO_MEMORY, "mmap huge len -> -3");

    // ---- 5. munmap
    assert_eq!(sys!(SyscallId::Munmap, [a1 as u64, 0x2000, 0, 0, 0, 0]), 0, "munmap a1");
    assert_eq!(sys!(SyscallId::Munmap, [a1 as u64, 0x2000, 0, 0, 0, 0]), E_NOT_FOUND, "munmap twice -> -5");
    assert_eq!(sys!(SyscallId::Munmap, [a2 as u64, 0x2000, 0, 0, 0, 0]), E_NOT_FOUND, "munmap len mismatch -> -5");
    assert_eq!(sys!(SyscallId::Munmap, [a2 as u64 + 1, 0x1000, 0, 0, 0, 0]), E_INVALID_ADDR, "munmap unaligned -> -2");
    assert_eq!(sys!(SyscallId::Munmap, [a2 as u64, 0, 0, 0, 0, 0]), E_INVALID_ADDR, "munmap len=0 -> -2");
    assert_eq!(sys!(SyscallId::Munmap, [0x5000_0000u64, 0x1000, 0, 0, 0, 0]), E_NOT_FOUND, "munmap never-mapped -> -5");
    // 自律清理（elf_continuation 断言 cleanup 后 ACTIVE=0 + FR8 归零）
    assert_eq!(sys!(SyscallId::Munmap, [a2 as u64, 0x1000, 0, 0, 0, 0]), 0, "munmap a2");
    assert_eq!(sys!(SyscallId::Munmap, [a3 as u64, 0x1000, 0, 0, 0, 0]), 0, "munmap a3");
    assert_eq!(sys!(SyscallId::Munmap, [a4 as u64, 0x1000, 0, 0, 0, 0]), 0, "munmap a4");

    // ---- 6. IPC 内核接线 smoke（P4-T7a）：init 自带 ep cap slot 1（bootstrap mint）
//      围栏优先：blocking send/recv 在 boot thread → -4（per-CPU 单 kstack
//      + boot 是系统最后防线，user-mode 双进程 IPC 阻塞由 T9 per-thread kstack
//      解决，本节只验证非阻塞路径 + 错误路径）。
//      错误路径：cptr=0 → -1；cptr=200 → -1；n_caps 越界 → -1。
    let ep_cap: u64 = 1; // bootstrap 固定：init 的 ep 根 cap slot（NULL=0 之后第一个 alloc）
    let mut snd = [0u8; 16];
    snd[..5].copy_from_slice(b"hello");
    // 6.1 围栏：blocking send 在 boot thread → E_WOULD_BLOCK
    assert_eq!(
        sys!(SyscallId::IpcSend, [ep_cap, &mut snd as *mut _ as u64, 5, 0, 0, 0]),
        E_WOULD_BLOCK,
        "send blocking on boot -> -4"
    );
    // 6.2 围栏：blocking recv 空队列在 boot thread → E_WOULD_BLOCK
    assert_eq!(
        sys!(SyscallId::IpcRecv, [ep_cap, &mut snd as *mut _ as u64, 0, 0, 0, 0]),
        E_WOULD_BLOCK,
        "recv blocking on boot -> -4"
    );
    // 6.3 错误路径：cptr=0 → E_INVALID_CAP
    assert_eq!(
        sys!(SyscallId::IpcTrySend, [0, &mut snd as *mut _ as u64, 5, 0, 0, 0]),
        E_INVALID_CAP,
        "try_send cptr=0 -> -1"
    );
    // 6.4 错误路径：cptr 越界 → E_INVALID_CAP
    assert_eq!(
        sys!(SyscallId::IpcTrySend, [200, &mut snd as *mut _ as u64, 5, 0, 0, 0]),
        E_INVALID_CAP,
        "try_send cptr=200 -> -1"
    );
    // 6.5 错误路径：n_caps > MAX_TRANSFER → E_INVALID_CAP
    assert_eq!(
        sys!(SyscallId::IpcTrySend, [ep_cap, &mut snd as *mut _ as u64, 5, 0, 99, 0]),
        E_INVALID_CAP,
        "try_send n_caps=99 -> -1"
    );
    // 6.6 try_send 非阻塞路径 → 0（无 receiver，Queued；MVP 围栏下不读不复制）
    let r = sys!(
        SyscallId::IpcTrySend,
        [ep_cap, &mut snd as *mut _ as u64, 5, 0, 0, 0]
    );
    assert_eq!(r, 0, "try_send queued -> 0");

    // ---- 7. process_exit(0)：内核 KERNEL_FRAME iretq 接力 elf_continuation
    unsafe { invoke(SyscallId::ProcessExit.num(), [0, 0, 0, 0, 0, 0]) };

    // 不应到这里（exit 不回用户态）
    loop {
        unsafe { asm!("hlt") };
    }
}

/// panic = abort（target json 约定）：停机循环 → ring-3 `hlt` #GP →
/// 内核 panic → QEMU exit 355（测试失败的确定性信号）。
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { asm!("hlt") };
    }
}
