//! synapse-kint 宿主集成测试。
//!
//! 验证"用户侧帧编码 → 内核侧 decode → 子 crate 操作 → 审计事件 → 返回"
//! 的完整闭环在 host 上跑通。

use synapse_abi::{SyscallFrame, SyscallId};
use synapse_kint::{
    dispatch, KernelState, E_INVALID_CAP, E_NOT_IMPLEMENTED,
};
use synapse_proc::Pid;
use synapse_user::{
    build_args_abi_query, build_args_cap_revoke, build_args_proc_exit, build_args_proc_freeze,
    build_args_proc_reap, build_args_proc_thaw, build_args_proc_yield, build_frame,
    CapRef,
};

// ============================================================================
// AbiQuery / Yield（无状态）
// ============================================================================

#[test]
fn abi_query_roundtrip_via_dispatch() {
    let mut state = KernelState::new();
    let frame = build_frame(SyscallId::AbiQuery, build_args_abi_query());
    let ret = dispatch(&mut state, &frame);
    // 返回值 = (major << 16) | minor = (0 << 16) | 1 = 1
    assert_eq!(ret, 1);
}

#[test]
fn yield_returns_zero() {
    let mut state = KernelState::new();
    let frame = build_frame(SyscallId::Yield, build_args_proc_yield());
    let ret = dispatch(&mut state, &frame);
    assert_eq!(ret, 0);
}

// ============================================================================
// ProcessExit / ProcessReap
// ============================================================================

#[test]
fn process_exit_then_reap() {
    let mut state = KernelState::new();
    // init 退出（code = 42）
    let frame = build_frame(SyscallId::ProcessExit, build_args_proc_exit(42));
    let ret = dispatch(&mut state, &frame);
    assert_eq!(ret, 0);
    // init 进入 Exited 态（非 Zombie）
    assert!(state.procs.get(Pid(1)).is_some());
    // 需要 release_resources 才能 reap（两阶段终止）
    state.procs.release_resources(Pid(1)).unwrap();
    // reap init（reaper = init 自己，合法）
    let frame = build_frame(SyscallId::ProcessReap, build_args_proc_reap(1));
    let ret = dispatch(&mut state, &frame);
    assert_eq!(ret, 0);
    // init 已被释放（slot 清空）
    assert!(state.procs.get(Pid(1)).is_none());
}

#[test]
fn process_exit_audit_event() {
    let mut state = KernelState::new();
    let frame = build_frame(SyscallId::ProcessExit, build_args_proc_exit(99));
    dispatch(&mut state, &frame);
    // 审计队列应有一条 ProcessExit 事件
    assert_eq!(state.audit.len(), 1);
    let ev = state.audit.pop().unwrap();
    assert_eq!(ev.kind, synapse_audit::EventKind::Process);
    if let synapse_audit::EventDetail::Process { op, code, .. } = ev.detail {
        assert_eq!(op, synapse_audit::ProcOp::Exit);
        assert_eq!(code, 99);
    } else {
        panic!("expected Process detail");
    }
}

// ============================================================================
// ProcessFreeze / ProcessThaw
// ============================================================================

#[test]
fn freeze_then_thaw_init() {
    let mut state = KernelState::new();
    // freeze init
    let frame = build_frame(SyscallId::ProcessFreeze, build_args_proc_freeze(1));
    let ret = dispatch(&mut state, &frame);
    assert_eq!(ret, 0);
    // thaw init
    let frame = build_frame(SyscallId::ProcessThaw, build_args_proc_thaw(1));
    let ret = dispatch(&mut state, &frame);
    assert_eq!(ret, 0);
}

#[test]
fn freeze_nonexistent_pid_returns_not_found() {
    let mut state = KernelState::new();
    let frame = build_frame(SyscallId::ProcessFreeze, build_args_proc_freeze(999));
    let ret = dispatch(&mut state, &frame);
    // Pid(999) 不存在 → NotFound = -5
    assert_eq!(ret, -5);
}

// ============================================================================
// CapRevoke
// ============================================================================

#[test]
fn cap_revoke_invalid_cap_returns_error() {
    let mut state = KernelState::new();
    // 尝试 revoke 一个不存在的 cap（slot 0 是 NULL trap）
    let frame = build_frame(SyscallId::CapRevoke, build_args_cap_revoke(CapRef::new(0).unwrap()));
    let ret = dispatch(&mut state, &frame);
    // slot 0 是 NULL → InvalidCap = -1
    assert_eq!(ret, -1);
}

#[test]
fn cap_revoke_out_of_bounds_returns_invalid_cap() {
    let mut state = KernelState::new();
    // current_pid 超出 caps Vec 范围
    state.current_pid = Pid(999);
    let frame = build_frame(SyscallId::CapRevoke, build_args_cap_revoke(CapRef::new(5).unwrap()));
    let ret = dispatch(&mut state, &frame);
    assert_eq!(ret, E_INVALID_CAP);
}

// ============================================================================
// NotificationSignal / NotificationWait
// ============================================================================

#[test]
fn notification_signal_and_poll() {
    let mut state = KernelState::new();
    // signal 位 0xAA
    let frame = build_frame(
        SyscallId::NotificationSignal,
        synapse_user::build_args_notify_signal(CapRef::new(0).unwrap(), 0xAA),
    );
    let ret = dispatch(&mut state, &frame);
    assert_eq!(ret, 0);
    // poll 返回 0xAA
    let frame = build_frame(
        SyscallId::NotificationWait,
        synapse_user::build_args_notify_wait(CapRef::new(0).unwrap(), 0xFF),
    );
    let ret = dispatch(&mut state, &frame);
    assert_eq!(ret, 0xAA);
    // 再次 poll 返回 0（已 read-clear）
    let frame = build_frame(
        SyscallId::NotificationWait,
        synapse_user::build_args_notify_wait(CapRef::new(0).unwrap(), 0xFF),
    );
    let ret = dispatch(&mut state, &frame);
    assert_eq!(ret, 0);
}

#[test]
fn notification_wait_out_of_bounds_returns_invalid_cap() {
    let mut state = KernelState::new();
    state.current_pid = Pid(999);
    let frame = build_frame(
        SyscallId::NotificationWait,
        synapse_user::build_args_notify_wait(CapRef::new(0).unwrap(), 0xFF),
    );
    let ret = dispatch(&mut state, &frame);
    assert_eq!(ret, E_INVALID_CAP);
}

// ============================================================================
// 桩实现（返回 -E_NOT_IMPLEMENTED）
// ============================================================================

#[test]
fn ipc_send_stub_returns_not_implemented() {
    let mut state = KernelState::new();
    let frame = build_frame(
        SyscallId::IpcSend,
        synapse_user::build_args_ipc_send(CapRef::new(5).unwrap(), 0x1000, 64, 0, 0),
    );
    let ret = dispatch(&mut state, &frame);
    assert_eq!(ret, E_NOT_IMPLEMENTED);
}

#[test]
fn mmap_stub_returns_not_implemented() {
    let mut state = KernelState::new();
    let frame = build_frame(
        SyscallId::Mmap,
        synapse_user::build_args_mem_mmap(0, 0x1000, 0b111, 0),
    );
    let ret = dispatch(&mut state, &frame);
    assert_eq!(ret, E_NOT_IMPLEMENTED);
}

#[test]
fn gettime_stub_returns_not_implemented() {
    let mut state = KernelState::new();
    let frame = build_frame(
        SyscallId::GetTime,
        synapse_user::build_args_gettime(0, 0x2000),
    );
    let ret = dispatch(&mut state, &frame);
    assert_eq!(ret, E_NOT_IMPLEMENTED);
}

// ============================================================================
// 错误路径：未知 syscall 号
// ============================================================================

#[test]
fn unknown_syscall_number_returns_not_implemented() {
    let mut state = KernelState::new();
    // 构造一个未知号（999）的 frame
    let frame = SyscallFrame { num: 999, args: [0; 6] };
    let ret = dispatch(&mut state, &frame);
    // 未知号 → decode 返回 None → E_NOT_IMPLEMENTED
    assert_eq!(ret, E_NOT_IMPLEMENTED);
}