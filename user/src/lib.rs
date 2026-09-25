//! Synapse 用户态库。
//!
//! Phase 1（P1-T1）仅占位，导出空 API 以便 workspace 编译通过。
//! Phase 4 起将引入：
//! * syscalls（`send` / `recv` / `reply` / `process_spawn` / `cap_*`）
//! * 与外交工具（Diplomat）的 IPC 客户端
//! * Agent 运行时原语（消息收发、TaskScheduler 客户端）
//!
//! 设计原则：
//! * 用户态库**仅做** syscall 封装与便利类型，**不**实现业务逻辑。
//! * 所有能力（capability）校验由内核强制；本库仅持有 cptr（u32 索引）。

#![no_std]

/// 用户态库版本号（与内核版本解耦；后续通过 Capability 传递）。
pub const USER_LIB_VERSION: u32 = 0x0001_0000;