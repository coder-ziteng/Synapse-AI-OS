//! # synapse-proc —— Synapse 微内核进程模型核心
//!
//! 对齐 [Doc 02 §5 进程/线程模型](../../../docs/design/02-userspace-abi-and-process-model.md)：
//! 生命周期状态机（spawn → exit/fault → zombie → reap）、agent_id 唯一性
//! 注册表、death notification、孤儿过继、行为围栏（freeze/thaw）、
//! 配额划拨与归还、spawn 初始 capability 授予。
//!
//! ## 模块划分
//!
//! | 模块 | 职责 | 设计出处 |
//! |------|------|----------|
//! | [`process`] | `ProcessTable` + `Pcb` + 状态机 + death signal + 孤儿过继 | Doc 02 §5.2/§5.3 |
//! | [`agent`] | 数值 agent_id 唯一性注册表（字符串映射归 init 用户态） | Doc 01 §7, Doc 02 §5.2 |
//! | [`grant`] | spawn 初始 caps 安装（复用 atomic transfer） | Doc 02 §5.2 |
//!
//! ## 边界（本 crate 不做的事）
//!
//! - 线程建模与调度：MVP-3 首期单线程（Doc 02 §5.1 约束），线程表 /
//!   调度状态由内核集成层负责；
//! - ELF 加载、页帧分配、CapTable 实际释放：集成层执行，本 crate 的
//!   状态转移即行动指令（释放顺序契约见 [`process`] 模块头注释）；
//! - death signal 投递：集成层走常规 IPC（`death_endpoint`），本 crate
//!   只构造 [`process::DeathSignal`]。
//!
//! ## 工程约束
//!
//! 与 `synapse-cap` / `synapse-ipc` 一致：`no_std` + 零外部依赖 +
//! 禁止 `unsafe` + 宿主全量可测（NFR4/NFR5）。

#![no_std]
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod agent;
pub mod grant;
pub mod process;

pub use agent::{AgentEntry, AgentRegistry};
pub use grant::{install_initial_caps, GrantItem, MAX_INITIAL_CAPS};
pub use process::{
    DeathSignal, FaultKind, Pid, Pcb, ProcState, ProcessTable, SpawnParams, INIT_PID, INIT_QUOTA,
    MAX_PROCS,
};
