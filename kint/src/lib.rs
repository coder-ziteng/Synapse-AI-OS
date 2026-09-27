//! # synapse-kint —— 内核集成层（host-testable mock dispatcher）
//!
//! 把 6 个纯逻辑 crate（`synapse-{abi,cap,ipc,proc,audit,user}`）粘合为
//! 宿主可测的 syscall 分发器，证明"用户侧帧编码 → 内核侧 decode → 子 crate
//! 操作 → 审计事件 → 返回"的完整闭环。
//!
//! ## 架构
//!
//! ```text
//! 用户态 (synapse-user)
//!   ↓ build_args_* + invoke (frame)
//! [KernelState::dispatch]  ← 本 crate
//!   ↓ abi::decode
//!   ↓ 匹配 Syscall 枚举
//!   ↓ 调用 synapse-{cap,ipc,proc}
//!   ↓ 推审计事件到 synapse-audit
//!   ↓ 返回 i64 (负值 = 错误码)
//! ```
//!
//! ## 实现范围（Phase 4 host-testable）
//!
//! **完整实现**：
//! - `AbiQuery` / `Yield`（无状态）
//! - `ProcessExit` / `ProcessReap` / `ProcessFreeze` / `ProcessThaw`（proc crate）
//! - `CapRevoke`（cap crate）
//! - `NotificationSignal` / `NotificationWait`（ipc crate）
//!
//! **桩实现**（返回 `-E_NOT_IMPLEMENTED`）：
//! - `IpcSend` / `IpcRecv` / `IpcReply` / `IpcTrySend`（需 buffer 处理）
//! - `ProcessSpawn` / `CapInvoke` / `CapDelegate`（复杂逻辑）
//! - `GetTime` / `Mmap` / `Munmap`（需时钟 / 虚拟内存）
//!
//! ## 后续 port
//!
//! 本 crate 的 `KernelState` 用 `Vec`（alloc）存 `CapTable` 等——宿主测试
//! 方便，但真实内核需换成固定数组（`kernel/src/`）。port 时：
//! - `KernelState` → `kernel/src/state.rs`（固定数组）
//! - `dispatch` → `kernel/src/syscall.rs`（真实 dispatcher）
//! - `current_pid` → 从 CPU 状态读取（`current` 指针）

#![no_std]
extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use synapse_abi::{abi_query_value, decode, Syscall, SyscallFrame};
use synapse_audit::{AuditEvent, AuditQueue, DefaultAuditQueue};
use synapse_cap::{CapError, CapTable, ObjectTable};
use synapse_ipc::{AgentId, Notification};
use synapse_proc::{Pid, ProcessTable, SpawnParams};

/// 错误码（Doc 02 §4.3 表）。
pub const E_NOT_IMPLEMENTED: i64 = -10;
/// 非法 capability 引用。
pub const E_INVALID_CAP: i64 = -1;
/// 非法用户态地址。
pub const E_INVALID_ADDR: i64 = -2;
/// 权限不足。
pub const E_PERMISSION: i64 = -7;

/// Mock 内核状态（宿主测试用；真实内核换成固定数组）。
///
/// `caps` / `notifs` 以 `Pid` 为索引（`caps[pid]`），Pid(0) 保留未用。
/// `current_pid` 由测试设置，模拟"当前进程发起 syscall"。
/// `user_memory` 模拟用户态地址空间（tests 写入指针数据，dispatcher 读取）。
pub struct KernelState {
    /// 进程表（synapse-proc）。
    pub procs: ProcessTable,
    /// 每进程 capability 表（indexed by pid）。
    pub caps: Vec<CapTable>,
    /// 审计队列（synapse-audit）。
    pub audit: DefaultAuditQueue,
    /// 每进程 notification（indexed by pid）。
    pub notifs: Vec<Notification>,
    /// 共享对象表（synapse-cap）。
    pub objects: ObjectTable,
    /// 当前进程 PID（测试设置；真实内核从 CPU 状态读取）。
    pub current_pid: Pid,
    /// Mock 用户态地址空间（tests 写入指针数据，dispatcher 读取 / 写入）。
    pub user_memory: BTreeMap<u64, Vec<u8>>,
    /// Mock 单调时钟计数器（gettime 每次调用 +1 纳秒）。
    pub time_counter: u64,
}

impl KernelState {
    /// 新建 mock 内核状态（安装 init 进程 + 1 个 test agent）。
    pub fn new() -> Self {
        let init_agent = AgentId(1);
        let procs = ProcessTable::new(init_agent);
        let mut caps = Vec::with_capacity(256);
        caps.push(CapTable::new()); // Pid(0) 保留
        caps.push(CapTable::new()); // init Pid(1)
        let mut notifs = Vec::with_capacity(256);
        notifs.push(Notification::new()); // Pid(0)
        notifs.push(Notification::new()); // Pid(1)
        KernelState {
            procs,
            caps,
            audit: AuditQueue::new(),
            notifs,
            objects: ObjectTable::new(),
            current_pid: Pid(1), // init
            user_memory: BTreeMap::new(),
            time_counter: 0,
        }
    }
}

impl Default for KernelState {
    fn default() -> Self {
        Self::new()
    }
}

/// syscall 分发器（宿主可测；后续 port 到 `kernel/src/syscall.rs`）。
///
/// 返回 `i64`：负值 = 错误码（Doc 02 §4.3），非负 = 成功值。
pub fn dispatch(state: &mut KernelState, frame: &SyscallFrame) -> i64 {
    let syscall = match decode(frame) {
        Some(s) => s,
        None => return E_NOT_IMPLEMENTED, // 未知号 / 参数越界 = IllegalSyscall
    };

    match syscall {
        Syscall::AbiQuery => handle_abi_query(),
        Syscall::Yield => handle_yield(),
        Syscall::ProcessExit { code } => handle_process_exit(state, code),
        Syscall::ProcessReap { pid } => handle_process_reap(state, Pid(pid)),
        Syscall::ProcessFreeze { pid } => handle_process_freeze(state, Pid(pid)),
        Syscall::ProcessThaw { pid } => handle_process_thaw(state, Pid(pid)),
        Syscall::ProcessSpawn { elf, args, caps, n_caps, death_ep } => {
            handle_process_spawn(state, elf, args, caps, n_caps, death_ep)
        }
        Syscall::CapRevoke { cap } => handle_cap_revoke(state, cap),
        Syscall::CapDelegate { parent, rights, child_out } => {
            handle_cap_delegate(state, parent, rights, child_out)
        }
        Syscall::NotificationSignal { notif, bits } => handle_notification_signal(state, notif, bits),
        Syscall::NotificationWait { notif, mask } => handle_notification_wait(state, notif, mask),
        Syscall::GetTime { clock_id, ts_out } => handle_gettime(state, clock_id, ts_out),
        // 桩实现
        Syscall::IpcSend { .. }
        | Syscall::IpcRecv { .. }
        | Syscall::IpcReply { .. }
        | Syscall::IpcTrySend { .. }
        | Syscall::CapInvoke { .. }
        | Syscall::Mmap { .. }
        | Syscall::Munmap { .. } => E_NOT_IMPLEMENTED,
    }
}

// ============================================================================
// Handler 实现
// ============================================================================

fn handle_abi_query() -> i64 {
    abi_query_value() as i64
}

fn handle_yield() -> i64 {
    0 // stub: 真实内核切换上下文
}

fn handle_process_exit(state: &mut KernelState, code: i32) -> i64 {
    let pid = state.current_pid;
    match state.procs.exit(pid, code) {
        Ok(_) => {
            state.audit.push(AuditEvent::process(
                AgentId(0), // UNSTAMPED for process events
                synapse_audit::ProcOp::Exit,
                AgentId(0),
                pid.0,
                code,
                0,
            ));
            0
        }
        Err(e) => cap_error_to_errno(e),
    }
}

fn handle_process_reap(state: &mut KernelState, pid: Pid) -> i64 {
    let reaper = state.current_pid;
    match state.procs.reap(reaper, pid) {
        Ok(_) => 0,
        Err(e) => cap_error_to_errno(e),
    }
}

fn handle_process_freeze(state: &mut KernelState, pid: Pid) -> i64 {
    match state.procs.freeze(pid) {
        Ok(_) => 0,
        Err(e) => cap_error_to_errno(e),
    }
}

fn handle_process_thaw(state: &mut KernelState, pid: Pid) -> i64 {
    match state.procs.thaw(pid) {
        Ok(_) => 0,
        Err(e) => cap_error_to_errno(e),
    }
}

fn handle_cap_revoke(state: &mut KernelState, cap_ref: u8) -> i64 {
    let pid = state.current_pid;
    let idx = pid.0 as usize;
    if idx >= state.caps.len() {
        return E_INVALID_CAP;
    }
    match state.caps[idx].revoke_cascade(cap_ref) {
        Ok(n) => n as i64,
        Err(e) => cap_error_to_errno(e),
    }
}

fn handle_notification_signal(state: &mut KernelState, _notif_ref: u8, bits: u32) -> i64 {
    let pid = state.current_pid;
    let idx = pid.0 as usize;
    if idx >= state.notifs.len() {
        return E_INVALID_CAP;
    }
    // stub: 真实内核需校验 notif_ref 是合法 notification cap
    state.notifs[idx].signal(bits as u64);
    0
}

fn handle_notification_wait(state: &mut KernelState, _notif_ref: u8, _mask: u32) -> i64 {
    let pid = state.current_pid;
    let idx = pid.0 as usize;
    if idx >= state.notifs.len() {
        return E_INVALID_CAP;
    }
    // stub: 真实内核需阻塞 + 校验 mask
    match state.notifs[idx].poll() {
        Some(bits) => bits as i64,
        None => 0, // 无事件（真实内核会阻塞）
    }
}

fn handle_process_spawn(
    state: &mut KernelState,
    _elf_ref: u8,
    _args_ptr: u64,
    caps_ptr: u64,
    n_caps: u64,
    death_ep: u8,
) -> i64 {
    let parent_pid = state.current_pid;
    let parent_idx = parent_pid.0 as usize;
    if parent_idx >= state.caps.len() {
        return E_INVALID_CAP;
    }
    // 从 user_memory 读取初始 caps 数组
    let initial_caps: Vec<u8> = match state.user_memory.get(&caps_ptr) {
        Some(data) if data.len() >= n_caps as usize => data[..n_caps as usize].to_vec(),
        _ => return E_INVALID_ADDR,
    };
    // stub: 真实内核需校验 elf_ref 是合法 ELF 镜像 cap
    // stub: 真实内核需校验 death_ep 是合法 endpoint cap
    // 创建子进程（简化：使用默认 quota，不安装初始 caps）
    let child_agent = AgentId(100 + state.procs.live_count() as u32);
    let child_quota = synapse_cap::DEFAULT_QUOTA;
    let params = SpawnParams {
        agent: child_agent,
        quota: child_quota,
        death_endpoint: death_ep,
    };
    match state.procs.spawn(parent_pid, params) {
        Ok(child_pid) => {
            // 扩展 caps/notifs 以支持新 pid
            let child_idx = child_pid.0 as usize;
            while state.caps.len() <= child_idx {
                state.caps.push(CapTable::new());
                state.notifs.push(Notification::new());
            }
            // stub: 真实内核需调用 cap::transfer_caps 安装初始 caps
            let _ = initial_caps;
            child_pid.0 as i64
        }
        Err(e) => cap_error_to_errno(e),
    }
}

fn handle_cap_delegate(
    state: &mut KernelState,
    parent: u8,
    rights: u32,
    child_out: u64,
) -> i64 {
    let pid = state.current_pid;
    let idx = pid.0 as usize;
    if idx >= state.caps.len() {
        return E_INVALID_CAP;
    }
    // 将 u32 权限位转为 Rights
    let rights_obj = synapse_cap::Rights::from_bits(rights);
    match state.caps[idx].delegate(parent, rights_obj) {
        Ok(child_cptr) => {
            // 写入 child_cap 到 user_memory
            state.user_memory.insert(child_out, alloc::vec![child_cptr]);
            0
        }
        Err(e) => cap_error_to_errno(e),
    }
}

fn handle_gettime(state: &mut KernelState, clock_id: u32, ts_out: u64) -> i64 {
    // Mock: 返回单调时钟计数器（每次调用 +1 纳秒）
    let _ = clock_id; // stub: 真实内核需区分 MONOTONIC / WALL
    let time = state.time_counter;
    state.time_counter += 1;
    // 写入 Timespec { tv_sec: u64, tv_nsec: u32 } = 12 字节
    let mut timespec = alloc::vec![0u8; 12];
    timespec[..8].copy_from_slice(&time.to_le_bytes());
    timespec[8..12].copy_from_slice(&0u32.to_le_bytes()); // tv_nsec = 0
    state.user_memory.insert(ts_out, timespec);
    0
}

/// CapError → errno 映射（Doc 02 §4.3）。
fn cap_error_to_errno(e: CapError) -> i64 {
    match e {
        CapError::InvalidCap => -1,
        CapError::InvalidAddr => -2,
        CapError::NoMemory => -3,
        CapError::WouldBlock => -4,
        CapError::NotFound => -5,
        CapError::AgentIdConflict => -6,
        CapError::Permission => -7,
        CapError::Frozen => -8,
        CapError::Zombie => -9,
        CapError::NotImplemented => -10,
        CapError::AbiMismatch => -11,
        CapError::ObjectRetired => -12,
        CapError::QuotaExceeded => -13,
        CapError::PeerDied => -14,
        CapError::Timeout => -15,
    }
}