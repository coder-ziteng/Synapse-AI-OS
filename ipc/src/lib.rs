//! # synapse-ipc —— Synapse 微内核 IPC 核心
//!
//! 对齐 [Doc 03 IPC 消息与单拷贝路径](../../../docs/design/03-ipc-message-and-single-copy-path.md)：
//! 消息头 ABI、传输路径分类、异步 Notification、同步 Endpoint 队列状态机。
//!
//! ## 模块划分
//!
//! | 模块 | 职责 | 设计出处 |
//! |------|------|----------|
//! | [`header`] | `IpcHeader`（repr(C) 固定 ABI）+ version/header_len 校验 + AgentId 盖章 | Doc 03 §3.1, Doc 02 §4.5 |
//! | [`path`] | 传输路径分类（寄存器直传 / kmap 单拷贝 / 共享 grant） | Doc 03 §4.3 |
//! | [`notification`] | 位图语义异步通知（O(1) signal / 读清 poll） | Doc 03 §6.2 |
//! | [`endpoint`] | 同步端点队列状态机 + try_send + 对端死亡取消 | Doc 03 §2 / §5.1 |
//!
//! ## 边界（本 crate 不做的事）
//!
//! - 线程阻塞 / 唤醒 / 调度：内核集成层负责（本 crate 只返回状态转移结果）；
//! - kmap 页表操作与 memcpy：内核集成层负责（本 crate 只提供路径分类）；
//! - capability 校验与转移安装：复用 [`synapse_cap`]（`transfer_caps` atomic 语义）。
//!
//! ## 工程约束
//!
//! 与 `synapse-cap` 一致：`no_std` + 零外部依赖 + 禁止 `unsafe` + 宿主全量可测（NFR4/NFR5）。

#![no_std]
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod endpoint;
pub mod header;
pub mod notification;
pub mod path;

pub use endpoint::{Endpoint, RecvOutcome, SendOutcome, SendRequest, MAX_SEND_QUEUE};
pub use header::{AgentId, IpcHeader, IPC_ABI_VERSION, MAX_PAYLOAD};
pub use notification::Notification;
pub use path::{classify, TransferPath};
