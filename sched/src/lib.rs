//! # synapse-sched —— Synapse 调度核心纯逻辑（P3-T1）
//!
//! 对齐需求：[`需求目标.md` §三 Phase 3](../../../需求目标.md)（TCB/生命周期/RR+优先级/抢占模型）。
//!
//! ## 模块划分
//!
//! | 模块 | 职责 |
//! |------|------|
//! | [`types`] | `ThreadId`（index+generation）/ `ThreadState` 状态机 / `Priority` / `ContextSlot` / `SchedError` |
//! | [`tcb`] | `ThreadTable`：固定槽位 TCB 存储 + generation 复用防 ABA + 合法状态迁移校验 |
//! | [`runqueue`] | `RunQueue`：NQ 级优先级位图 + 每级 FIFO 环（同级 Round-Robin） |
//! | [`sleepq`] | `SleepQueue`：deadline 队列，`pop_due(now)` 按最早到期序唤醒 |
//! | [`accounting`] | FR8 资源核算：`PageLedger` per-process 内存页账本（CPU 时间在 `Scheduler::account_cpu`） |
//! | [`scheduler`] | `Scheduler` 门面：spawn/block/unblock/sleep/wake/freeze/thaw/exit/reap + tick 时间片 + `schedule()` 决策 |
//!
//! ## 抢占模型（需求评审修订）
//!
//! 本 crate 是**纯决策逻辑**：`tick(now)` 只置 `need_resched` 标志，`schedule(now)`
//! 返回 [`SwitchDecision`]（Switch / KeepCurrent / Idle），**不做任何硬件操作**。
//! 内核集成层（P3-T6）负责在**中断返回边界**与**阻塞点**消费该标志并调用
//! `switch_to`（P3-T5 手写汇编）——避免"在 ISR 内站在被抢占线程内核栈上切换"的陷阱。
//!
//! ## 工程约束（与 cap/ipc/proc 同一纪律）
//!
//! - `no_std` + 零依赖 + 固定数组（不使用 alloc），宿主 `cargo test` 全量可测。
//! - 禁止 `unsafe`（crate 级 deny）；锁语义（Spinlock + 关中断）由内核集成层负责。
//! - 上下文存储 [`ContextSlot`] 对本 crate 不透明；内核 P3-T4/T5 按 SysV AMD64
//!   callee-saved 约定（rbx/rbp/r12~r15/rsp/rip）读写。

#![no_std]
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod accounting;
pub mod runqueue;
pub mod scheduler;
pub mod sleepq;
pub mod tcb;
pub mod types;

pub use accounting::{PageLedger, MAX_PROC_LEDGERS};
pub use runqueue::RunQueue;
pub use scheduler::{Scheduler, SwitchDecision, DEFAULT_TIME_SLICE};
pub use sleepq::SleepQueue;
pub use tcb::{ThreadTable, MAX_THREADS};
pub use types::{ContextSlot, Priority, SchedError, ThreadId, ThreadState, CTX_WORDS, NQ};
