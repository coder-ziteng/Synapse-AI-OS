//! P4-T9e crash smoke 子进程（initramfs `crasher`）。
//!
//! 路径：
//! - `mmap 1 页` → 0（成功，写读回环）
//! - `mmap 64 页` → -13 E_QUOTA_EXCEEDED（max_pages=16 超限）
//! - `munmap 1 页` → 0
//! - `*0x5000_0000 = X` → 内核 #PF → `kill_path` → `terminate_current(SegFault)`
//! - 内核向 init death_endpoint 投递 `DeathMsg{ fault: FAULT_SEGFAULT }`
//!
//! 失败语义：任何 assert 失败 → panic（=abort）→ ring-3 `hlt` 循环 → 内核
//! panic（IllegalInstruction / #GP）→ QEMU exit **355**（区别于全过 363）。

#![no_std]
#![no_main]

use core::arch::asm;
use core::panic::PanicInfo;

use synapse_abi::{
    E_QUOTA_EXCEEDED, PROT_READ, PROT_WRITE, SyscallId,
};
use synapse_user::invoke;

const RW: u64 = (PROT_READ | PROT_WRITE) as u64;

macro_rules! sys {
    ($id:expr, [$($a:expr),* $(,)?]) => {
        unsafe { invoke($id.num(), [$($a as u64),*]) }
    };
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // ---- 1. mmap 1 页 RW（成功）
    let a1 = sys!(SyscallId::Mmap, [0, 0x1000, RW, 0, 0, 0]);
    assert!(a1 >= 0x4100_0000, "mmap 1 page: addr = {a1:#x}");
    unsafe {
        let p = a1 as *mut u64;
        p.write_volatile(0xC0DE_C0DE_C0DE_C0DE);
        assert_eq!(p.read_volatile(), 0xC0DE_C0DE_C0DE_C0DE, "mmap RW roundtrip");
    }

    // ---- 2. mmap 64 页 RW（quota 拒绝：max_pages=16）
    let r = sys!(SyscallId::Mmap, [0, 0x40_0000, RW, 0, 0, 0]);
    assert_eq!(
        r, E_QUOTA_EXCEEDED,
        "mmap 64 pages quota: expected -13 E_QUOTA_EXCEEDED, got {r}"
    );

    // ---- 3. munmap a1（释放 1 页配额）
    assert_eq!(
        sys!(SyscallId::Munmap, [a1 as u64, 0x1000, 0, 0, 0, 0]),
        0,
        "munmap a1"
    );

    // ---- 4. 写未映射页 0x5000_0000 → SegFault → terminate_current
    //     这条永远不会"成功返回"：内核走 terminate_current → KERNEL_FRAME iretq
    //     到 crash_continuation。如果走到这里，说明 #PF 没被内核接住（= panic）。
    let bad: *mut u64 = 0x5000_0000 as *mut u64;
    unsafe {
        bad.write_volatile(0xDEAD_BEEF);
    }
    // 不会到这里
    loop {
        unsafe { asm!("hlt") };
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { asm!("hlt") };
    }
}