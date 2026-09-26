//! syscall 底层调用 + 帧编码器。
//!
//! ## 调用约定（Doc 02 §4.1）
//!
//! | 寄存器 | 含义 |
//! |---|---|
//! | `rax` | syscall 号（输入）/ 返回值（输出，负值 = 错误码） |
//! | `rdi, rsi, rdx, r10, r8, r9` | 参数 1~6 |
//! | `rcx, r11` | 由 `syscall` 指令破坏（恢复为用户态须 `swapgs` 等） |
//!
//! ## 帧编码与解码的对称性
//!
//! 本模块的 `build_args_*` 函数把类型化参数编码成 `[u64; 6]`；内核集成层
//! 用 [`synapse_abi::decode`] 反向解码。**两端必须严格对称**——任何
//! 错位都会导致 decode 失败 → `IllegalSyscall` 杀进程。本 crate 的宿主
//! 测试通过 `decode(encode(...))` 闭环验证这一对称性（见
//! [`tests/user_core.rs`](../../tests/user_core.rs)）。

use synapse_abi::{SyscallFrame, SyscallId};

use crate::handle::CapRef;

/// `rax < 0` 标志位（Doc 02 §4.3 错误码统一为负值）。
pub const NEGATIVE_BIT: u64 = 1u64 << 63;

/// 用户态 syscall 入口：执行 `syscall` 指令并返回内核结果。
///
/// # Safety
///
/// - 调用方必须保证参数寄存器所引用的内存（指针参数指向的 buffer）
///   在 syscall 期间**保持有效**（内核可能跨页读取）；
/// - 调用方必须保证传给 `CapRef` 参数的索引 ≤ 255；
/// - 在非 Synapse 内核上调用此函数 = 非法指令陷阱（`#UD`），由宿主
///   OS 翻译为 SIGILL / STATUS_ILLEGAL_INSTRUCTION。本 crate 宿主测试
///   **不会**触发此路径，仅在 QEMU 上的真实 Synapse 内核下使用。
#[inline]
#[cfg_attr(target_arch = "x86_64", allow(unsafe_code))]
pub unsafe fn invoke(num: u64, args: [u64; 6]) -> i64 {
    #[cfg(target_arch = "x86_64")]
    {
        let ret: i64;
        core::arch::asm!(
            "syscall",
            inlateout("rax") num => ret,
            in("rdi") args[0],
            in("rsi") args[1],
            in("rdx") args[2],
            in("r10") args[3],
            in("r8")  args[4],
            in("r9")  args[5],
            lateout("rcx") _,
            lateout("r11") _,
        );
        ret
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        // 非 x86_64 目标（如 arm 用户态、PIC 引导测试）：编译期明确拒绝
        // 触发真正的 syscall，由宿主测试走 frame-encoder 闭环验证。
        let _ = (num, args);
        panic!("synapse-user::invoke requires x86_64 (syscall instruction unavailable)")
    }
}

/// 构造 syscall 帧（传入 `invoke`）。
#[inline]
pub fn frame(num: SyscallId, args: [u64; 6]) -> SyscallFrame {
    SyscallFrame { num: num.num(), args }
}

// ============================================================================
// 帧编码器（host-testable 纯函数；与 `synapse_abi::decode` 对称）
// ============================================================================
//
// 命名约定：`build_args_<syscall>(typed args) -> [u64; 6]`
// 每个编码器严格对应 `abi::decode` 中的同号 syscall，反向解码必须成功。
// args[5] 在 5 参 syscall 中保留为 0（Doc 02 §4.1 规定 6 个寄存器；空位
// 固定 0 便于内核端 `decode` 忽略）。

const ZERO_6: [u64; 6] = [0, 0, 0, 0, 0, 0];

/// `abi_query()` → `(major << 16) | minor`。
#[inline]
pub const fn build_args_abi_query() -> [u64; 6] {
    ZERO_6
}

/// `ipc_send(ep, msg, len, caps, n_caps)`。
#[inline]
pub const fn build_args_ipc_send(
    ep: CapRef,
    msg_ptr: u64,
    len: u64,
    caps_ptr: u64,
    n_caps: u32,
) -> [u64; 6] {
    [ep.as_u64(), msg_ptr, len, caps_ptr, n_caps as u64, 0]
}

/// `ipc_recv(ep, buf, cap_out)`。
#[inline]
pub const fn build_args_ipc_recv(ep: CapRef, buf_ptr: u64, cap_out_ptr: u64) -> [u64; 6] {
    [ep.as_u64(), buf_ptr, cap_out_ptr, 0, 0, 0]
}

/// `ipc_reply(ep, msg, len)`。
#[inline]
pub const fn build_args_ipc_reply(ep: CapRef, msg_ptr: u64, len: u64) -> [u64; 6] {
    [ep.as_u64(), msg_ptr, len, 0, 0, 0]
}

/// `ipc_try_send(ep, msg, len)`。
#[inline]
pub const fn build_args_ipc_try_send(ep: CapRef, msg_ptr: u64, len: u64) -> [u64; 6] {
    [ep.as_u64(), msg_ptr, len, 0, 0, 0]
}

/// `notification_signal(notif, bits)`。
#[inline]
pub const fn build_args_notify_signal(notif: CapRef, bits: u32) -> [u64; 6] {
    [notif.as_u64(), bits as u64, 0, 0, 0, 0]
}

/// `notification_wait(notif, mask)`。
#[inline]
pub const fn build_args_notify_wait(notif: CapRef, mask: u32) -> [u64; 6] {
    [notif.as_u64(), mask as u64, 0, 0, 0, 0]
}

/// `cap_invoke(cap, op, args)`。
#[inline]
pub const fn build_args_cap_invoke(cap: CapRef, op: u32, args_ptr: u64) -> [u64; 6] {
    [cap.as_u64(), op as u64, args_ptr, 0, 0, 0]
}

/// `cap_delegate(parent, rights, child_out)`。
#[inline]
pub const fn build_args_cap_delegate(parent: CapRef, rights: u32, child_out_ptr: u64) -> [u64; 6] {
    [parent.as_u64(), rights as u64, child_out_ptr, 0, 0, 0]
}

/// `cap_revoke(cap)`。
#[inline]
pub const fn build_args_cap_revoke(cap: CapRef) -> [u64; 6] {
    [cap.as_u64(), 0, 0, 0, 0, 0]
}

/// `process_spawn(elf, args, caps, n_caps, death_ep)`。
#[inline]
pub const fn build_args_proc_spawn(
    elf: CapRef,
    args_ptr: u64,
    caps_ptr: u64,
    n_caps: u32,
    death_ep: CapRef,
) -> [u64; 6] {
    [elf.as_u64(), args_ptr, caps_ptr, n_caps as u64, death_ep.as_u64(), 0]
}

/// `process_exit(code)`。
///
/// # Note
///
/// `code: i32` → `u64` 走补码扩展（`as u64` 在 Rust 是零扩展；负数会丢
/// 符号位）。**故意**不直接零扩展：内核端按低 32 位补码解释（`a[0] as i32`），
/// 与 `process_exit(code: i32)` 的语义一致。`-1i64` 转 u64 是
/// `0xFFFFFFFFFFFFFFFF`，低 32 位 = `0xFFFFFFFF` = `-1i32`，匹配。
#[inline]
pub const fn build_args_proc_exit(code: i32) -> [u64; 6] {
    [code as u64, 0, 0, 0, 0, 0]
}

/// `process_reap(pid)`。
#[inline]
pub const fn build_args_proc_reap(pid: u32) -> [u64; 6] {
    [pid as u64, 0, 0, 0, 0, 0]
}

/// `process_freeze(pid)`。
#[inline]
pub const fn build_args_proc_freeze(pid: u32) -> [u64; 6] {
    [pid as u64, 0, 0, 0, 0, 0]
}

/// `process_thaw(pid)`。
#[inline]
pub const fn build_args_proc_thaw(pid: u32) -> [u64; 6] {
    [pid as u64, 0, 0, 0, 0, 0]
}

/// `yield()`。
#[inline]
pub const fn build_args_proc_yield() -> [u64; 6] {
    ZERO_6
}

/// `gettime(clock_id, ts_out)`。
#[inline]
pub const fn build_args_gettime(clock_id: u32, ts_out_ptr: u64) -> [u64; 6] {
    [clock_id as u64, ts_out_ptr, 0, 0, 0, 0]
}

/// `mmap(addr, len, prot, flags)`。
#[inline]
pub const fn build_args_mem_mmap(addr: u64, len: u64, prot: u32, flags: u32) -> [u64; 6] {
    [addr, len, prot as u64, flags as u64, 0, 0]
}

/// `munmap(addr, len)`。
#[inline]
pub const fn build_args_mem_munmap(addr: u64, len: u64) -> [u64; 6] {
    [addr, len, 0, 0, 0, 0]
}

/// `exit(code)` 便利别名（与 `process_exit` 同义；保留便于上层语义化调用）。
#[inline]
pub const fn build_args_exit(code: i32) -> [u64; 6] {
    build_args_proc_exit(code)
}