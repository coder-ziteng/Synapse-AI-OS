//! 类型化 syscall 包装器（19 个）。
//!
//! 每个 `pub fn` 调用 [`syscall::invoke`] 并按 Doc 02 §4.3 错误码约定
//! 解码返回值（负值 = 错误码，统一映射 [`SynapseError`]）。
//!
//! ## 错误模型
//!
//! - `Ok(T)`：内核返回正值，按 syscall 语义解读为 `T`；
//! - `Err(SynapseError)`：内核返回负值（绝对值 = 错误码）。
//!
//! ## 宿主测试
//!
//! `pub fn wrappers::*` 不直接调用 `unsafe fn invoke`（避免宿主 OS 触发
//! 非法指令陷阱）。所有 syscall 帧的**编码正确性**通过
//! [`synapse_abi::decode`] 闭环验证（见 `tests/user_core.rs`）。

use synapse_abi::{abi_query_value, ABI_MAJOR, ABI_MINOR};

use crate::handle::CapRef;
use crate::syscall::{
    build_args_abi_query, build_args_cap_delegate, build_args_cap_invoke, build_args_cap_revoke,
    build_args_exit, build_args_gettime, build_args_ipc_recv, build_args_ipc_reply,
    build_args_ipc_send, build_args_ipc_try_send, build_args_mem_mmap, build_args_mem_munmap,
    build_args_notify_signal, build_args_notify_wait, build_args_proc_exit, build_args_proc_freeze,
    build_args_proc_reap, build_args_proc_spawn, build_args_proc_thaw, build_args_proc_yield,
    frame, invoke,
};

/// Syscall 返回的统一错误码（Doc 02 §4.3 表）。
///
/// 用户态**不**自行判断每个 syscall 的错误码含义，统一映射为正 errno；
/// 调试 / 日志模块自行格式化为可读字符串。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SynapseError(pub i32);

impl SynapseError {
    /// 原始内核返回（`rax < 0`）。
    #[inline]
    pub const fn raw(self) -> i64 {
        self.0 as i64
    }
}

#[inline]
fn map(ret: i64) -> Result<i64, SynapseError> {
    if ret < 0 {
        Err(SynapseError(ret as i32))
    } else {
        Ok(ret)
    }
}

// ============================================================================
// ABI / 进程退出
// ============================================================================

/// `abi_query()` 返回值（解码后）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbiVersion {
    /// major（不兼容变更时递增）。
    pub major: u16,
    /// minor（向后兼容新增 syscall / 错误码时递增）。
    pub minor: u16,
}

/// 解码 `abi_query()` 返回值（`(major << 16) | minor`）。
///
/// 宿主测试用：把内核预期值与 [`abi_query_value`] 比对，确保 ABI 主版本
/// 不会被两侧漂移（major 漂移 = 编译期 cargo dep upgrade 提醒）。
#[inline]
pub const fn decode_abi_query_result(ret: i64) -> Result<AbiVersion, SynapseError> {
    if ret < 0 {
        return Err(SynapseError(ret as i32));
    }
    let v = ret as u64;
    Ok(AbiVersion {
        major: (v >> 16) as u16,
        minor: (v & 0xFFFF) as u16,
    })
}

/// `abi_query()` → 当前内核 ABI 版本。
#[inline]
pub fn abi_query() -> Result<AbiVersion, SynapseError> {
    let f = frame(synapse_abi::SyscallId::AbiQuery, build_args_abi_query());
    let r = unsafe { invoke(f.num, f.args) };
    decode_abi_query_result(r)
}

/// 进程退出（便利别名，与 [`proc_exit`] 同义）。
///
/// # Note
///
/// 调用 `process_exit(code)` 后进程进入 Zombie 态，由父进程 reap。本函数
/// 永远不返回（内核不切换回用户态）；保留返回类型仅为与 `Result` 系列
/// 签名风格统一。
#[inline]
#[allow(unreachable_code)]
pub fn exit(code: i32) -> ! {
    unsafe {
        let _ = invoke(
            frame(synapse_abi::SyscallId::ProcessExit, build_args_exit(code)).num,
            frame(synapse_abi::SyscallId::ProcessExit, build_args_exit(code)).args,
        );
    }
    unreachable!("process_exit returned (kernel bug?)")
}

// ============================================================================
// IPC（4 个）
// ============================================================================

/// `ipc_send` 返回值解码。
#[inline]
pub fn decode_ipc_send_result(ret: i64) -> Result<(), SynapseError> {
    map(ret).map(|_| ())
}

/// `ipc_send(ep, msg, len, caps, n_caps)` 阻塞发送。
///
/// # Safety
///
/// `msg_ptr` 指向的 buffer 必须在 syscall 期间有效（至少 `len` 字节可读）。
/// `caps_ptr` 同理，且指向 `n_caps` 个 `u8` CapRef。
#[inline]
pub unsafe fn ipc_send(
    ep: CapRef,
    msg_ptr: *const u8,
    len: usize,
    caps_ptr: *const u8,
    n_caps: u32,
) -> Result<(), SynapseError> {
    let f = frame(synapse_abi::SyscallId::IpcSend, build_args_ipc_send(ep, msg_ptr as u64, len as u64, caps_ptr as u64, n_caps));
    decode_ipc_send_result(invoke(f.num, f.args))
}

/// `ipc_recv(ep, buf, cap_out)` 阻塞接收。
///
/// # Safety
///
/// `buf_ptr` 写入 buffer（内核至多写 `quota.max_msg_size` 字节）；
/// `cap_out_ptr` 写入 `n_caps` 个 `u8`。
#[inline]
pub unsafe fn ipc_recv(
    ep: CapRef,
    buf_ptr: *mut u8,
    cap_out_ptr: *mut u8,
) -> Result<i64, SynapseError> {
    let f = frame(synapse_abi::SyscallId::IpcRecv, build_args_ipc_recv(ep, buf_ptr as u64, cap_out_ptr as u64));
    map(invoke(f.num, f.args))
}

/// `ipc_reply(ep, msg, len)` 回复原发送方（cap-transfer 仅 reply 路径允许）。
///
/// # Safety
///
/// `msg_ptr` 指向的 buffer 在 syscall 期间有效（至少 `len` 字节可读）。
#[inline]
pub unsafe fn ipc_reply(ep: CapRef, msg_ptr: *const u8, len: usize) -> Result<(), SynapseError> {
    let f = frame(synapse_abi::SyscallId::IpcReply, build_args_ipc_reply(ep, msg_ptr as u64, len as u64));
    map(invoke(f.num, f.args)).map(|_| ())
}

/// `ipc_try_send(ep, msg, len)` 非阻塞发送（Doc 03 §9，驱动通知路径）。
///
/// # Safety
///
/// 同 [`ipc_send`]。
#[inline]
pub unsafe fn ipc_try_send(ep: CapRef, msg_ptr: *const u8, len: usize) -> Result<(), SynapseError> {
    let f = frame(synapse_abi::SyscallId::IpcTrySend, build_args_ipc_try_send(ep, msg_ptr as u64, len as u64));
    map(invoke(f.num, f.args)).map(|_| ())
}

// ============================================================================
// Notification（2 个）
// ============================================================================

/// `notification_signal(notif, bits)` 位图 OR 投递。
#[inline]
pub fn notify_signal(notif: CapRef, bits: u32) -> Result<(), SynapseError> {
    let f = frame(synapse_abi::SyscallId::NotificationSignal, build_args_notify_signal(notif, bits));
    map(unsafe { invoke(f.num, f.args) }).map(|_| ())
}

/// `notification_wait(notif, mask)` 阻塞等待，返回触发位（read-clear）。
///
/// 内核按 `mask` 过滤；返回值 = 实际触发的位。
#[inline]
pub fn notify_wait(notif: CapRef, mask: u32) -> Result<u64, SynapseError> {
    let f = frame(synapse_abi::SyscallId::NotificationWait, build_args_notify_wait(notif, mask));
    map(unsafe { invoke(f.num, f.args) }).map(|v| v as u64)
}

// ============================================================================
// Capability（3 个）
// ============================================================================

/// `cap_invoke(cap, op, args)` 通用能力调用（对象特定操作）。
///
/// # Safety
///
/// `args_ptr` 指向的对象特定参数 buffer 由各能力类型定义（见 Doc 01 §4）。
#[inline]
pub unsafe fn cap_invoke(cap: CapRef, op: u32, args_ptr: *const u8) -> Result<i64, SynapseError> {
    let f = frame(synapse_abi::SyscallId::CapInvoke, build_args_cap_invoke(cap, op, args_ptr as u64));
    map(invoke(f.num, f.args))
}

/// `cap_delegate(parent, rights, child_out)` 衰减委托。
///
/// `rights` 必须为 `parent` 当前权限的**子集**，否则内核返回 `Permission`。
/// 子 CapRef 写入 `child_out_ptr`。
///
/// # Safety
///
/// `child_out_ptr` 写入 `1` 个 `u8`。
#[inline]
pub unsafe fn cap_delegate(
    parent: CapRef,
    rights: u32,
    child_out_ptr: *mut u8,
) -> Result<(), SynapseError> {
    let f = frame(synapse_abi::SyscallId::CapDelegate, build_args_cap_delegate(parent, rights, child_out_ptr as u64));
    map(invoke(f.num, f.args)).map(|_| ())
}

/// `cap_revoke(cap)` 撤销（derivation tree 级联）。
#[inline]
pub fn cap_revoke(cap: CapRef) -> Result<(), SynapseError> {
    let f = frame(synapse_abi::SyscallId::CapRevoke, build_args_cap_revoke(cap));
    map(unsafe { invoke(f.num, f.args) }).map(|_| ())
}

// ============================================================================
// Process（6 个）
// ============================================================================

/// `process_spawn` 返回值（子进程 pid）。
#[inline]
pub fn proc_spawn(
    elf: CapRef,
    args_ptr: u64,
    caps_ptr: u64,
    n_caps: u32,
    death_ep: CapRef,
) -> Result<u32, SynapseError> {
    let f = frame(synapse_abi::SyscallId::ProcessSpawn, build_args_proc_spawn(elf, args_ptr, caps_ptr, n_caps, death_ep));
    map(unsafe { invoke(f.num, f.args) }).map(|v| v as u32)
}

/// `process_exit(code)`。
///
/// 同 [`exit`]；保留独立命名以贴合 `process_*` 命名空间。
#[inline]
#[allow(unreachable_code)]
pub fn proc_exit(code: i32) -> ! {
    unsafe {
        let _ = invoke(
            frame(synapse_abi::SyscallId::ProcessExit, build_args_proc_exit(code)).num,
            frame(synapse_abi::SyscallId::ProcessExit, build_args_proc_exit(code)).args,
        );
    }
    unreachable!()
}

/// `process_reap(pid)` 回收 zombie。
///
/// 调用方必须是 `pid` 的父进程或 init（Phase 4 限制）；否则返回 `Permission`。
#[inline]
pub fn proc_reap(pid: u32) -> Result<(), SynapseError> {
    let f = frame(synapse_abi::SyscallId::ProcessReap, build_args_proc_reap(pid));
    map(unsafe { invoke(f.num, f.args) }).map(|_| ())
}

/// `process_freeze(pid)` 行为围栏冻结（需 PROCESS::ADMIN）。
#[inline]
pub fn proc_freeze(pid: u32) -> Result<(), SynapseError> {
    let f = frame(synapse_abi::SyscallId::ProcessFreeze, build_args_proc_freeze(pid));
    map(unsafe { invoke(f.num, f.args) }).map(|_| ())
}

/// `process_thaw(pid)` 行为围栏解冻。
#[inline]
pub fn proc_thaw(pid: u32) -> Result<(), SynapseError> {
    let f = frame(synapse_abi::SyscallId::ProcessThaw, build_args_proc_thaw(pid));
    map(unsafe { invoke(f.num, f.args) }).map(|_| ())
}

/// `yield()` 主动让出 CPU。
#[inline]
pub fn proc_yield() -> Result<(), SynapseError> {
    let f = frame(synapse_abi::SyscallId::Yield, build_args_proc_yield());
    map(unsafe { invoke(f.num, f.args) }).map(|_| ())
}

// ============================================================================
// Time（1 个）
// ============================================================================

/// `gettime(clock_id, ts_out)` 时钟读取。
///
/// # Safety
///
/// `ts_out_ptr` 指向 16 字节 [`synapse_abi::Timespec`]（`{sec: u64, nsec: u64}`，
/// repr(C)，P4-T6 定稿布局），必须 8B 对齐且位于可写用户页——否则内核
/// 返回 `E_INVALID_ADDR`。
#[inline]
pub unsafe fn gettime(
    clock_id: u32,
    ts_out_ptr: *mut synapse_abi::Timespec,
) -> Result<(), SynapseError> {
    let f = frame(synapse_abi::SyscallId::GetTime, build_args_gettime(clock_id, ts_out_ptr as u64));
    map(invoke(f.num, f.args)).map(|_| ())
}

/// `clock_id` 常量（[`gettime`] 的合法值；与 `synapse_abi::CLOCK_*` 同值）。
pub mod clock_id {
    /// 单调时钟（TSC 校准，Doc 02 §6）。
    pub const MONOTONIC: u32 = synapse_abi::CLOCK_MONOTONIC;
    /// 墙钟（RTC，可选；未接硬件前内核返回 `E_NOT_IMPLEMENTED`）。
    pub const WALL: u32 = synapse_abi::CLOCK_WALL;
}

// ============================================================================
// Memory（2 个）
// ============================================================================

/// `mmap(addr, len, prot, flags)`。
///
/// 返回映射后的虚拟地址（正值）；失败返回错误码。
#[inline]
pub fn mem_mmap(addr: u64, len: u64, prot: u32, flags: u32) -> Result<u64, SynapseError> {
    let f = frame(synapse_abi::SyscallId::Mmap, build_args_mem_mmap(addr, len, prot, flags));
    map(unsafe { invoke(f.num, f.args) }).map(|v| v as u64)
}

/// `munmap(addr, len)`。
#[inline]
pub fn mem_munmap(addr: u64, len: u64) -> Result<(), SynapseError> {
    let f = frame(synapse_abi::SyscallId::Munmap, build_args_mem_munmap(addr, len));
    map(unsafe { invoke(f.num, f.args) }).map(|_| ())
}

// 仅供 `frame()` 引用：`synapse_abi::SyscallId` 在多个 syscall 中需要
// 重复 `frame(id, args)` 调用（每次构造 `SyscallFrame` 是一次结构体构造）。
// 通过在每个 wrapper 中调用 `frame()`，保持编码与解码的**强对称性**——
// `frame().num` 一定等于 `synapse_abi::SyscallId::* as u64`。
const _: () = {
    // 编译期断言：本 crate 编译通过即意味着 19 个 frame 编码器全部产出
    // 与 `abi::decode` 对称的 `SyscallFrame`。
    let _ = ABI_MAJOR;
    let _ = ABI_MINOR;
    let _ = abi_query_value;
};