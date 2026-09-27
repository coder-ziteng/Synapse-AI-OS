//! # synapse-cap —— Synapse 微内核能力子系统核心
//!
//! 对齐设计文档：
//!
//! - [Doc 01 能力/Agent/权限模型](../../../docs/design/01-capability-agent-permission-model.md)
//! - [Doc 02 用户态 ABI 与进程模型](../../../docs/design/02-userspace-abi-and-process-model.md)（§4.3 错误码、§4.5 ABI 版本、§5.5 配额）
//! - [Doc 03 IPC 消息与单拷贝路径](../../../docs/design/03-ipc-message-and-single-copy-path.md)（§5 能力转移）
//!
//! ## 模块划分
//!
//! | 模块 | 职责 | 设计出处 |
//! |------|------|----------|
//! | [`error`] | 统一错误码 + errno 映射（-1..-15） | Doc 02 §4.3/§4.5 |
//! | [`rights`] | 权限位掩码（只追加，bit 7..31 保留） | Doc 01 §4 |
//! | [`types`] | `CapRef` / `ObjRef` / `Capability` / `ObjState` / `ObjKind` | Doc 01 §4/§4.1/§4.2 |
//! | [`table`] | 每进程 256 槽 CapTable + 委托 + 级联撤销 | Doc 01 §4/§3.3/§3.4 |
//! | [`object`] | 内核对象表 + generation 校验 + 生命周期状态机 | Doc 01 §4.2 |
//! | [`quota`] | 每进程资源配额（O(1) 检查） | Doc 02 §5.5 |
//! | [`transfer`] | 跨进程 atomic all-or-nothing 能力转移 | Doc 03 §5 |
//!
//! ## 工程约束
//!
//! - `no_std` + 零依赖：可直接链入内核；宿主 `cargo test` 全量可测（NFR4/NFR5）。
//! - 本 crate 无锁：多核就绪的锁语义（Spinlock + 关中断临界区）由内核集成层负责。
//! - 禁止 `unsafe`（crate 级 deny）。

#![no_std]
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod object;
pub mod quota;
pub mod rights;
pub mod table;
pub mod transfer;
pub mod types;

pub use error::CapError;
pub use object::ObjectTable;
pub use quota::{Quota, QuotaUsage, Resource, DEFAULT_QUOTA};
pub use rights::Rights;
pub use table::CapTable;
pub use transfer::{transfer_caps, TransferItem, MAX_TRANSFER};
pub use types::{CapRef, Capability, ObjKind, ObjRef, ObjState, CAP_TABLE_SIZE};
