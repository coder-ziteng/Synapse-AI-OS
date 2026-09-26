//! Synapse 用户态库。
//!
//! Phase 4 入口：从本窗口的 `synapse-abi`（SyscallId 号表 + decode）出
//! 发，把 19 个 syscall 变成类型安全、可在宿主单测的包装层。
//!
//! ## 与内核集成层的边界
//!
//! ```text
//! 用户程序 / 外交工具
//!        ↓ 调用
//! synapse-user (本 crate)        ← 纯逻辑：帧编码 + 类型化 + unsafe invoke
//!        ↓ syscall 指令
//! [Synapse 内核 syscall 入口]    ← 另一窗口（boot）管辖
//! ```
//!
//! - 本 crate **不**实现业务逻辑，只做 syscall ABI 包装；
//! - 所有能力校验由内核强制；用户态仅持有 `cptr`（`u8` 索引）；
//! - `unsafe fn invoke` 是用户态 syscall 入口的**唯二**合法 unsafe 集中点
//!   （另一处为 `asm!`），其余 API 一律 `safe`。
//!
//! ## 宿主测试策略
//!
//! `unsafe fn invoke` 在宿主（Windows / Linux）上**不会**执行真正的
//! syscall 指令——目标内核无 `MSR_LSTAR` 配置，宿主测试改用 frame-encoding
//! 验证（见 [`syscall::build_args_*`]）和结果解码（[`wrappers::decode_*`]），
//! 保证 syscall 包装层在用户态一侧**逻辑正确**，**真实内核交互**由
//! Phase 4 boot 集成完成后在 QEMU 上端到端验证。

#![no_std]

pub mod handle;
pub mod syscall;
pub mod wrappers;

pub use handle::{CapError, CapRef};
pub use synapse_abi::SyscallFrame;
pub use synapse_cap::{ObjKind, ObjRef, Rights};
pub use synapse_ipc::AgentId;
pub use syscall::{invoke, NEGATIVE_BIT};
pub use wrappers::{
    abi_query, clock_id, exit, ipc_send, ipc_recv, ipc_reply, ipc_try_send,
    notify_signal, notify_wait,
    cap_invoke, cap_delegate, cap_revoke,
    proc_spawn, proc_exit, proc_yield, proc_reap, proc_freeze, proc_thaw,
    mem_mmap, mem_munmap, gettime,
    decode_abi_query_result, AbiVersion, SynapseError,
};

pub use synapse_abi::abi_query_value;

// 帧编码器（host-testable；上层可按需直接构造 syscall 帧，跳过 `unsafe`）
pub use syscall::{
    build_args_abi_query, build_args_cap_delegate, build_args_cap_invoke, build_args_cap_revoke,
    build_args_exit, build_args_gettime, build_args_ipc_recv, build_args_ipc_reply,
    build_args_ipc_send, build_args_ipc_try_send, build_args_mem_mmap, build_args_mem_munmap,
    build_args_notify_signal, build_args_notify_wait, build_args_proc_exit,
    build_args_proc_freeze, build_args_proc_reap, build_args_proc_spawn, build_args_proc_thaw,
    build_args_proc_yield, frame as build_frame,
};

/// 用户态库版本号（与内核版本解耦；后续可通过 Capability 传递）。
pub const USER_LIB_VERSION: u32 = 0x0001_0000;