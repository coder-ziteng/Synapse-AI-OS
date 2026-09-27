//! # synapse-audit —— 内核审计事件层
//!
//! 对齐 [Doc 03 §6.3 审计事件流](../../../docs/design/03-ipc-message-and-single-copy-path.md)（FR9）：
//! 内核事件点建模 + 固定大小事件记录 + 环形队列（热路径不阻塞，
//! 审计服务后台批量消费）。
//!
//! ## 架构位置
//!
//! ```text
//! 内核事件点(cap校验/IPC/进程) ─push→ AuditQueue ─drain→ 审计服务(S5, 独立进程)
//!                                              └→ 聚合+签名 → append-only 存储
//! ```
//!
//! - 内核侧（本 crate）：构造事件 + 入队，**不签名、不落盘**；
//! - 签名主体是审计服务自身（Doc 03 §6.3），持有自己的密钥；
//! - 内核↔审计服务信道天然可信，仅需完整性，批量提交减少 IPC 次数。
//!
//! ## 泄密防线（FR9 / Doc 01 §5.2）
//!
//! [`event::AuditEvent`] **没有 payload 字段**，构造函数也不接收
//! 业务数据——从 API 层面保证"审计记录永不含用户态业务数据原文"。
//!
//! ## 工程约束
//!
//! 与 cap/ipc/proc 一致：`no_std` + 零外部依赖 + 禁止 `unsafe` +
//! 无锁（集成层负责临界区）+ 宿主全量可测（NFR4/NFR5）。

#![no_std]
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod event;
pub mod queue;

pub use event::{
    AuditEvent, CapOp, EventDetail, EventKind, IpcDir, ProcOp, SystemEvent,
};
pub use queue::{AuditQueue, DefaultAuditQueue, DEFAULT_QUEUE_CAPACITY};
