//! 进程生命周期（T9c 雏形）。
//!
//! 当前为 stub：仅满足 `paging.rs` 等 callsite 的链接与编译，不实现真实
//! terminate_current 流程。完整死亡通知 / 资源释放 / Zombie 转换属 P4-T9c，
//! 这里仅占位。
//!
//! - [`current_is_spawned_child`] 在 MVP 阶段恒返回 `false`，因此
//!   `paging::handle_user_fault` 末尾的 T9c 早退分支不会被触发，smoke 维持
//!   原 panic 语义不变。
//! - [`terminate_current`] 在 MVP 阶段不可达——`current_is_spawned_child`
//!   守卫先关——直接 panic 兜底，避免 silently fall-through。

use synapse_proc::process::FaultKind;

/// 当前 ring-3 上下文是否为 spawn 出的子进程（init 不算）。
///
/// MVP 阶段：init 单进程，恒返回 `false`。T9c 接线 `proc_ext::current_pid` 与
/// `ProcessTable::pid_is_spawned_child(pid)` 后改为真正查询。
pub fn current_is_spawned_child() -> bool {
    false
}

/// 终止当前 ring-3 进程（T9c：fault → death notification → 资源释放 → Zombie）。
///
/// MVP stub：调用路径已被 `current_is_spawned_child` 关闭，正常测试不该进入；
/// 一旦进入则视为契约破坏，panic 兜底避免悄无声息 fall-through。
///
/// # Safety
///
/// 调用方需保证已进入 ring-3（CR3 / RFLAGS / rsp0 已切到目标进程上下文）。
/// 实现到位后此处将 iretq 进终止路径，本 stub 不返回。
pub unsafe fn terminate_current(_kind: Option<FaultKind>, _code: i32) -> ! {
    panic!("[proc_life] terminate_current stub called — T9c 未实现，当前应被 current_is_spawned_child 守卫拦截");
}