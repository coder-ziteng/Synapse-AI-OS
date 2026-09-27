//! Synapse 硬件抽象层（HAL）。
//!
//! 架构无关的核心 trait 与按目标架构的具体实现。设计目标：
//!
//! * **零成本抽象**：通过 `#[cfg(target_arch = "...")]` 静态分发，避免 vtable 开销。
//! * **宿主可测**：`cfg(test)` 下提供 fake impl，使架构无关逻辑可在 `cargo test` 中验证。
//!
//! # 当前 trait（P1-T6 落实）
//!
//! | Trait                  | 职责                                       |
//! | ---------------------- | ------------------------------------------ |
//! | [`serial::SerialDevice`] | UART 16550 等串口设备（首个 HAL device） |
//!
//! # 计划 trait（后续 Phase）
//!
//! | Trait                  | 职责                                       |
//! | ---------------------- | ------------------------------------------ |
//! | `mmu::Mmu`             | 物理/虚拟地址映射、缺页处理                |
//! | `interrupt::InterruptController` | 中断注册/使能/屏蔽、EOI              |
//! | `timer::Timer`         | 定时器中断与时间源                         |
//! | `context::ContextSwitch` | 线程上下文切换                           |

#![no_std]

pub mod serial;

#[cfg(test)]
extern crate std;
