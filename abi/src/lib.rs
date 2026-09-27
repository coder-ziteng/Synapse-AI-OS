//! # synapse-abi —— syscall ABI 定义（用户态 / 内核共享）
//!
//! 对齐 [Doc 02 §4.1/§4.2/§4.5](../../../docs/design/02-userspace-abi-and-process-model.md)：
//! 调用约定（rax = syscall 号；rdi/rsi/rdx/r10/r8/r9 = 参数 1~6；返回 rax，
//! 负值 = 错误码）、18 个 syscall 号表、ABI 版本协商（`abi_query`）。
//!
//! ## 设计原则
//!
//! - **零依赖**：本 crate 只含常量、枚举与 `repr(C)` 布局——用户态与内核
//!   共享同一份定义，任何一侧的类型演进不得影响 ABI 本体；
//! - 参数一律以**原始整数**跨边界（`CapRef=u8` / `Pid=u32` / 指针 `u64`），
//!   语义类型（`Rights`、`Quota` 等）由各侧自行编解码；
//! - 结构体布局约束（Doc 02 §4.5）：跨边界结构必须 `repr(C)`、
//!   little-endian、新字段只追加尾部。
//!
//! ## 解码失败（`None`）的两种来源
//!
//! [`decode`] 在以下情况返回 `None`：
//!
//! 1. **未知 syscall 号**（含预留空洞）；
//! 2. **窄参数越界**：CapRef 参数 > 255、u32 参数 > `u32::MAX`
//!    ——即 Doc 02 §5.3 `FaultKind::IllegalSyscall` 定义的"未知号 / 参数越界"。
//!
//! 内核集成层将 `None` 统一按 `IllegalSyscall` 崩溃处理（杀进程 +
//! death notification），**不做静默截断**——截断会让脏高位参数命中
//! 合法槽位，构成混淆攻击面。

#![no_std]
#![deny(unsafe_code)]
#![warn(missing_docs)]

/// ABI major 版本（不兼容变更：删除 / 重解释 syscall，Doc 02 §4.5）。
pub const ABI_MAJOR: u16 = 0;

/// ABI minor 版本（向后兼容新增 syscall / 错误码 / rights 位时递增）。
///
/// 0.2（P4-T6）：错误码常量（§4.3）、clock_id、Timespec、mmap prot/flags
/// 位常量首次进入 crate 本体；syscall 号表与 decode 语义不变。
/// 0.3（P4-T7）：补齐扩展错误码 -11..-15（与 cap crate `CapError` /
/// Doc 03 §5.1 IPC 错误语义对齐；Doc 02 §4.3 表同步追加）。
/// 0.4（P4-T9c）：death notification 消息布局 [`DeathMsg`] + [`DEATH_LABEL`]
/// + `FAULT_*` 崩溃归因编码（Doc 02 §5.3；纯新增，向后兼容）。
pub const ABI_MINOR: u16 = 4;

/// `abi_query`（#18）返回值：`(major << 16) | minor`。
///
/// 用户态启动时调用，major 不匹配则拒绝运行（Doc 02 §4.5）。
pub const fn abi_query_value() -> u64 {
    ((ABI_MAJOR as u64) << 16) | (ABI_MINOR as u64)
}

// ============================================================================
// 错误码（Doc 02 §4.3）—— syscall 返回值 rax < 0 时的全集
// ============================================================================
//
// 规格差异注记（P4-T6，P4-T7 修订）：task.json deliverable 写"15 个错误码"，
// Doc 02 §4.3 原表仅列 -1..-10。P4-T7 查明：cap crate `CapError` 与 Doc 03
// §5.1（IPC 错误语义）实际使用 -11..-15（AbiMismatch/ObjectRetired/
// QuotaExceeded/PeerDied/Timeout），Doc 02 表滞后——按"扩展新错误码追加到
// 表尾"原则补齐 15 个并 UPDATE Doc 02 §4.3。

/// 无效 CapRef（不在进程 capability 表中 / 已撤销）。
pub const E_INVALID_CAP: i64 = -1;
/// 无效用户态地址（未映射 / 权限违例 / 越出用户窗口）。
pub const E_INVALID_ADDR: i64 = -2;
/// 内核内存不足（页帧 / 堆耗尽）。
pub const E_NO_MEMORY: i64 = -3;
/// 非阻塞操作本会阻塞。
pub const E_WOULD_BLOCK: i64 = -4;
/// 目标不存在（进程 / endpoint / 映射区域）。
pub const E_NOT_FOUND: i64 = -5;
/// Agent ID 冲突（注册表重名）。
pub const E_AGENT_ID_CONFLICT: i64 = -6;
/// 权限拒绝（capability rights 不足 / mmap prot 非法组合）。
pub const E_PERMISSION: i64 = -7;
/// 目标进程已冻结（FR10 行为围栏）。
pub const E_FROZEN: i64 = -8;
/// 目标进程为僵尸态。
pub const E_ZOMBIE: i64 = -9;
/// syscall 未实现（预留号 / 延后交付路径）。
pub const E_NOT_IMPLEMENTED: i64 = -10;
/// 用户态与内核 ABI 版本不兼容（消息头 version 校验，Doc 03 §3.1）。
pub const E_ABI_MISMATCH: i64 = -11;
/// 对象已撤销 / 退休（generation 不匹配，Doc 01 §4.2）。
pub const E_OBJECT_RETIRED: i64 = -12;
/// 进程资源配额耗尽（Doc 02 §5.5）。
pub const E_QUOTA_EXCEEDED: i64 = -13;
/// IPC 对端进程已退出（Doc 03 §5.1 对端死亡唤醒规则）。
pub const E_PEER_DIED: i64 = -14;
/// 操作超时（带超时变体预留，Doc 03 §9：Phase 5+）。
pub const E_TIMEOUT: i64 = -15;

// ============================================================================
// gettime（#30）常量与输出结构
// ============================================================================

/// `clock_id`：单调钟（TSC 校准，自引导起的纳秒；P4-T6 唯一可用钟）。
pub const CLOCK_MONOTONIC: u32 = 0;
/// `clock_id`：墙钟（RTC；未接硬件前返回 [`E_NOT_IMPLEMENTED`]）。
pub const CLOCK_WALL: u32 = 1;

/// `gettime` 输出结构（Doc 02 §4.5：`repr(C)`、little-endian、16 字节；
/// 新字段只允许追加尾部）。
///
/// 用户态不暴露 `rdtsc`：TSC 原始值仅内核可见，跨边界一律折算为
/// sec/nsec（防侧信道 + 频率校准集中化）。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timespec {
    /// 秒。
    pub sec: u64,
    /// 纳秒余量（0..999_999_999）。
    pub nsec: u64,
}

// ============================================================================
// mmap（#40）prot / flags 位
// ============================================================================
//
// 位值约定（P4-T6 定夺）：与内核 `synapse_vma::RegionFlags` 低位对齐
// （READ=bit0 / WRITE=bit1 / EXEC=bit2 / GROWABLE=bit3），使 decode 出的
// prot:u32 可零转换直通 VMA 层。注意与 Doc 01 capability `Rights` 的位值
// **不同**（Rights READ=bit3/WRITE=bit4/EXEC=bit5）——Doc 02 "Rights 原始位
// 低 3 位"表述有歧义，取 vma 约定并在 decision_log 记录。

/// `prot` 位：可读。
pub const PROT_READ: u32 = 1 << 0;
/// `prot` 位：可写。
pub const PROT_WRITE: u32 = 1 << 1;
/// `prot` 位：可执行。
pub const PROT_EXEC: u32 = 1 << 2;
/// `prot` 合法位全集（含此集合外的位 → [`E_PERMISSION`]）。
pub const PROT_MASK: u32 = PROT_READ | PROT_WRITE | PROT_EXEC;
/// `flags` 位：区域可增长（stack 语义，透传 VMA `GROWABLE`）。
pub const MAP_GROWABLE: u32 = 1 << 3;
/// `flags` 合法位全集（含此集合外的位 → [`E_NOT_IMPLEMENTED`]，未实现语义）。
pub const MAP_MASK: u32 = MAP_GROWABLE;

// ============================================================================
// death notification（Doc 02 §5.3，P4-T9c）
// ============================================================================
//
// 进程进入 Exited/Faulted 时，内核向其 spawn 时注册的 `death_endpoint`
// 投递一条常规 IPC 消息（复用 Endpoint 队列，无新原语）：
// `label == DEATH_LABEL`，payload = 16 字节 [`DeathMsg`]。
// 父进程 `recv(death_endpoint)` 后按 label 识别，读取 payload 归因崩溃，
// 随后 `process_reap(pid)`（#22）彻底释放僵尸。

/// death signal 消息的 label（IPC 消息头 label 字段；用户按此过滤）。
pub const DEATH_LABEL: u32 = 0x4445_4144; // "DEAD"

/// `fault` 编码：正常退出（`process_exit`），无崩溃。
pub const FAULT_NONE: u32 = 0;
/// `fault` 编码：段错误（访问未映射 / 权限不符地址，#PF kill 路径）。
pub const FAULT_SEGFAULT: u32 = 1;
/// `fault` 编码：用户态 panic。
pub const FAULT_PANIC: u32 = 2;
/// `fault` 编码：非法 syscall（未知号 / 窄参数越界）。
pub const FAULT_ILLEGAL_SYSCALL: u32 = 3;
/// `fault` 编码：非法指令（#UD）。
pub const FAULT_ILLEGAL_INSTRUCTION: u32 = 4;
/// `fault` 编码：一般保护异常（ring-3 #GP：hlt / 特权指令 / 段违规）。
pub const FAULT_GENERAL_PROTECTION: u32 = 5;

/// death notification payload（`repr(C)`、little-endian、16 字节；
/// 新字段只允许追加尾部，Doc 02 §4.5 布局纪律同 [`Timespec`]）。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeathMsg {
    /// 终止进程的 pid。
    pub pid: u32,
    /// 退出码（正常退出 = `process_exit` 参数；崩溃 = 内核填充的异常码，
    /// 如 #PF error_code / 非法 syscall 号）。
    pub exit_code: i32,
    /// 崩溃归因（`FAULT_*` 编码；正常退出 = [`FAULT_NONE`]）。
    pub fault: u32,
    /// 保留（对齐填充；读方必须忽略）。
    pub rsv: u32,
}

/// syscall 号全集（Doc 02 §4.2 分配表 + §4.5 abi_query）。
///
/// 号段规划：IPC 0~3 / Notification 4~5 / Capability 10~12 / ABI 18 /
/// Process 20~25 / Time 30 / Memory 40~41。空洞为预留（追加不重排）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum SyscallId {
    /// 同步发送（阻塞直到 reply）。
    IpcSend = 0,
    /// 同步接收（阻塞直到消息）。
    IpcRecv = 1,
    /// 回复原发送方。
    IpcReply = 2,
    /// 非阻塞发送（Doc 03 §9）。
    IpcTrySend = 3,
    /// Notification 位图 OR 投递。
    NotificationSignal = 4,
    /// Notification 等待（返回触发位）。
    NotificationWait = 5,
    /// 通用能力调用（对象特定操作）。
    CapInvoke = 10,
    /// 委托子 capability（attenuation-only）。
    CapDelegate = 11,
    /// 撤销 capability（derivation tree 级联）。
    CapRevoke = 12,
    /// ABI 版本查询，返回 `(major << 16) | minor`。
    AbiQuery = 18,
    /// 创建子进程（首期仅 init 可调用）。
    ProcessSpawn = 20,
    /// 当前进程退出。
    ProcessExit = 21,
    /// 回收僵尸进程。
    ProcessReap = 22,
    /// 冻结进程（行为围栏，需 PROCESS::ADMIN）。
    ProcessFreeze = 23,
    /// 解冻进程。
    ProcessThaw = 24,
    /// 主动让出 CPU。
    Yield = 25,
    /// 获取时间（单调钟 / 墙钟）。
    GetTime = 30,
    /// 映射用户内存区域。
    Mmap = 40,
    /// 解除映射。
    Munmap = 41,
}

/// syscall 功能域（内核分发的第一级路由）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyscallClass {
    /// IPC（0~3）。
    Ipc,
    /// Notification（4~5）。
    Notification,
    /// Capability（10~12）。
    Capability,
    /// ABI（18）。
    Abi,
    /// Process（20~25）。
    Process,
    /// Time（30）。
    Time,
    /// Memory（40~41）。
    Memory,
}

impl SyscallId {
    /// 从 syscall 号解码；未知号 / 预留空洞 → `None`。
    pub const fn from_num(num: u64) -> Option<SyscallId> {
        match num {
            0 => Some(SyscallId::IpcSend),
            1 => Some(SyscallId::IpcRecv),
            2 => Some(SyscallId::IpcReply),
            3 => Some(SyscallId::IpcTrySend),
            4 => Some(SyscallId::NotificationSignal),
            5 => Some(SyscallId::NotificationWait),
            10 => Some(SyscallId::CapInvoke),
            11 => Some(SyscallId::CapDelegate),
            12 => Some(SyscallId::CapRevoke),
            18 => Some(SyscallId::AbiQuery),
            20 => Some(SyscallId::ProcessSpawn),
            21 => Some(SyscallId::ProcessExit),
            22 => Some(SyscallId::ProcessReap),
            23 => Some(SyscallId::ProcessFreeze),
            24 => Some(SyscallId::ProcessThaw),
            25 => Some(SyscallId::Yield),
            30 => Some(SyscallId::GetTime),
            40 => Some(SyscallId::Mmap),
            41 => Some(SyscallId::Munmap),
            _ => None,
        }
    }

    /// 原始 syscall 号。
    pub const fn num(self) -> u64 {
        self as u64
    }

    /// 所属功能域。
    pub const fn class(self) -> SyscallClass {
        match self {
            SyscallId::IpcSend
            | SyscallId::IpcRecv
            | SyscallId::IpcReply
            | SyscallId::IpcTrySend => SyscallClass::Ipc,
            SyscallId::NotificationSignal | SyscallId::NotificationWait => {
                SyscallClass::Notification
            }
            SyscallId::CapInvoke | SyscallId::CapDelegate | SyscallId::CapRevoke => {
                SyscallClass::Capability
            }
            SyscallId::AbiQuery => SyscallClass::Abi,
            SyscallId::ProcessSpawn
            | SyscallId::ProcessExit
            | SyscallId::ProcessReap
            | SyscallId::ProcessFreeze
            | SyscallId::ProcessThaw
            | SyscallId::Yield => SyscallClass::Process,
            SyscallId::GetTime => SyscallClass::Time,
            SyscallId::Mmap | SyscallId::Munmap => SyscallClass::Memory,
        }
    }
}

/// syscall 入口帧（`repr(C)`）：汇编 glue 从寄存器填充后交给分发层。
///
/// 布局：`num`(rax) + `args[6]`(rdi, rsi, rdx, r10, r8, r9)，共 56 字节。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SyscallFrame {
    /// syscall 号（rax）。
    pub num: u64,
    /// 参数 1~6（rdi/rsi/rdx/r10/r8/r9 顺序）。
    pub args: [u64; 6],
}

/// 解码后的 syscall（参数已按 Doc 02 §4.2 号表拆位；指针一律 `u64`，
/// 地址合法性校验属内核集成层职责 → `E_INVALID_ADDR`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Syscall {
    /// `ipc_send(ep, msg, len, caps, n_caps)`。
    IpcSend {
        /// endpoint CapRef。
        ep: u8,
        /// 用户态消息 buffer 指针。
        msg: u64,
        /// 消息字节数。
        len: u64,
        /// CapRef 数组指针（每 cap 1 byte）。
        caps: u64,
        /// 转移 cap 数。
        n_caps: u64,
    },
    /// `ipc_recv(ep, buf, cap_out)`。
    IpcRecv {
        /// endpoint CapRef。
        ep: u8,
        /// 接收 buffer 指针。
        buf: u64,
        /// 接收 cap 输出指针。
        cap_out: u64,
    },
    /// `ipc_reply(ep, msg, len)`。
    IpcReply {
        /// endpoint CapRef。
        ep: u8,
        /// 回复消息指针。
        msg: u64,
        /// 字节数。
        len: u64,
    },
    /// `ipc_try_send(ep, msg, len)`。
    IpcTrySend {
        /// endpoint CapRef。
        ep: u8,
        /// 消息指针。
        msg: u64,
        /// 字节数。
        len: u64,
    },
    /// `notification_signal(notif, bits)`。
    NotificationSignal {
        /// Notification CapRef。
        notif: u8,
        /// 位图。
        bits: u32,
    },
    /// `notification_wait(notif, mask)`。
    NotificationWait {
        /// Notification CapRef。
        notif: u8,
        /// 等待掩码。
        mask: u32,
    },
    /// `cap_invoke(cap, op, args)`。
    CapInvoke {
        /// 目标 capability。
        cap: u8,
        /// 对象特定操作码。
        op: u32,
        /// 操作参数 buffer 指针。
        args: u64,
    },
    /// `cap_delegate(parent, rights, child_out)`。
    CapDelegate {
        /// 父 capability。
        parent: u8,
        /// 衰减掩码（Rights 原始位）。
        rights: u32,
        /// 子 CapRef 输出指针。
        child_out: u64,
    },
    /// `cap_revoke(cap)`。
    CapRevoke {
        /// 目标 capability。
        cap: u8,
    },
    /// `abi_query()` → `(major << 16) | minor`。
    AbiQuery,
    /// `process_spawn(elf, args, caps, n_caps, death_ep)`。
    ProcessSpawn {
        /// ELF 镜像 MemoryRegion CapRef。
        elf: u8,
        /// 参数字符串指针。
        args: u64,
        /// 初始 CapRef 数组指针。
        caps: u64,
        /// 初始 cap 数。
        n_caps: u64,
        /// death endpoint CapRef（父进程持有）。
        death_ep: u8,
    },
    /// `process_exit(code)`。
    ProcessExit {
        /// 退出码。
        code: i32,
    },
    /// `process_reap(pid)`。
    ProcessReap {
        /// 目标僵尸进程。
        pid: u32,
    },
    /// `process_freeze(pid)`。
    ProcessFreeze {
        /// 目标进程。
        pid: u32,
    },
    /// `process_thaw(pid)`。
    ProcessThaw {
        /// 目标进程。
        pid: u32,
    },
    /// `yield()`。
    Yield,
    /// `gettime(clock_id, ts_out)`。
    GetTime {
        /// 时钟 ID（0 = 单调钟，1 = 墙钟）。
        clock_id: u32,
        /// Timespec 输出指针。
        ts_out: u64,
    },
    /// `mmap(addr, len, prot, flags)`。
    Mmap {
        /// 建议地址（0 = 内核选址）。
        addr: u64,
        /// 字节数（页对齐）。
        len: u64,
        /// 保护位（Rights 原始位低 3 位：R/W/X）。
        prot: u32,
        /// 映射标志（GROWABLE 等）。
        flags: u32,
    },
    /// `munmap(addr, len)`。
    Munmap {
        /// 起始地址。
        addr: u64,
        /// 字节数。
        len: u64,
    },
}

impl Syscall {
    /// 所属 syscall 号。
    pub const fn id(self) -> SyscallId {
        match self {
            Syscall::IpcSend { .. } => SyscallId::IpcSend,
            Syscall::IpcRecv { .. } => SyscallId::IpcRecv,
            Syscall::IpcReply { .. } => SyscallId::IpcReply,
            Syscall::IpcTrySend { .. } => SyscallId::IpcTrySend,
            Syscall::NotificationSignal { .. } => SyscallId::NotificationSignal,
            Syscall::NotificationWait { .. } => SyscallId::NotificationWait,
            Syscall::CapInvoke { .. } => SyscallId::CapInvoke,
            Syscall::CapDelegate { .. } => SyscallId::CapDelegate,
            Syscall::CapRevoke { .. } => SyscallId::CapRevoke,
            Syscall::AbiQuery => SyscallId::AbiQuery,
            Syscall::ProcessSpawn { .. } => SyscallId::ProcessSpawn,
            Syscall::ProcessExit { .. } => SyscallId::ProcessExit,
            Syscall::ProcessReap { .. } => SyscallId::ProcessReap,
            Syscall::ProcessFreeze { .. } => SyscallId::ProcessFreeze,
            Syscall::ProcessThaw { .. } => SyscallId::ProcessThaw,
            Syscall::Yield => SyscallId::Yield,
            Syscall::GetTime { .. } => SyscallId::GetTime,
            Syscall::Mmap { .. } => SyscallId::Mmap,
            Syscall::Munmap { .. } => SyscallId::Munmap,
        }
    }
}

/// 从入口帧解码 syscall（分发层第一步，O(1) match）。
///
/// 参数拆位约定（Doc 02 §4.1/§4.2）：
/// - `CapRef`（u8）/ `u32` 参数**严格校验**：寄存器值超出目标类型
///   范围 → `None`（参数越界 = IllegalSyscall，见 crate 头注释，
///   不静默截断，防混淆攻击）；
/// - `i32`（exit code）按低 32 位补码解释（全 64 位任意值合法）；
/// - 指针 / 长度一律原样 `u64` 透传，地址合法性属集成层
///   （`E_INVALID_ADDR`）。
pub fn decode(frame: &SyscallFrame) -> Option<Syscall> {
    let a = &frame.args;
    let cap = |i: usize| -> Option<u8> { u8::try_from(a[i]).ok() };
    let w32 = |i: usize| -> Option<u32> { u32::try_from(a[i]).ok() };
    let id = SyscallId::from_num(frame.num)?;
    Some(match id {
        SyscallId::IpcSend => Syscall::IpcSend {
            ep: cap(0)?,
            msg: a[1],
            len: a[2],
            caps: a[3],
            n_caps: a[4],
        },
        SyscallId::IpcRecv => Syscall::IpcRecv { ep: cap(0)?, buf: a[1], cap_out: a[2] },
        SyscallId::IpcReply => Syscall::IpcReply { ep: cap(0)?, msg: a[1], len: a[2] },
        SyscallId::IpcTrySend => Syscall::IpcTrySend { ep: cap(0)?, msg: a[1], len: a[2] },
        SyscallId::NotificationSignal => {
            Syscall::NotificationSignal { notif: cap(0)?, bits: w32(1)? }
        }
        SyscallId::NotificationWait => Syscall::NotificationWait { notif: cap(0)?, mask: w32(1)? },
        SyscallId::CapInvoke => Syscall::CapInvoke { cap: cap(0)?, op: w32(1)?, args: a[2] },
        SyscallId::CapDelegate => {
            Syscall::CapDelegate { parent: cap(0)?, rights: w32(1)?, child_out: a[2] }
        }
        SyscallId::CapRevoke => Syscall::CapRevoke { cap: cap(0)? },
        SyscallId::AbiQuery => Syscall::AbiQuery,
        SyscallId::ProcessSpawn => Syscall::ProcessSpawn {
            elf: cap(0)?,
            args: a[1],
            caps: a[2],
            n_caps: a[3],
            death_ep: cap(4)?,
        },
        SyscallId::ProcessExit => Syscall::ProcessExit { code: a[0] as i32 },
        SyscallId::ProcessReap => Syscall::ProcessReap { pid: w32(0)? },
        SyscallId::ProcessFreeze => Syscall::ProcessFreeze { pid: w32(0)? },
        SyscallId::ProcessThaw => Syscall::ProcessThaw { pid: w32(0)? },
        SyscallId::Yield => Syscall::Yield,
        SyscallId::GetTime => Syscall::GetTime { clock_id: w32(0)?, ts_out: a[1] },
        SyscallId::Mmap => {
            Syscall::Mmap { addr: a[0], len: a[1], prot: w32(2)?, flags: w32(3)? }
        }
        SyscallId::Munmap => Syscall::Munmap { addr: a[0], len: a[1] },
    })
}
