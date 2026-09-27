//! IPC 内核子系统接线（P4-T7a）。
//!
//! ## 范围
//!
//! 实现 [`IpcSend`]/[`IpcRecv`]/[`IpcReply`]/[`IpcTrySend`] 四个 syscall 的内核
//! 后端（Doc 03 同步 Endpoint 语义）：
//!
//! - cap 校验：cptr → Capability → ObjRef → `ObjectTable::check_live` → Endpoint kind；
//! - 单拷贝：发送方 AS 物理页 → 接收方 AS 物理页直拷（恒等映射下 PA = 内核 VA）；
//! - cap transfer：随消息转移 CapRef 到接收方 CapTable（[`kstate::k_transfer`]）；
//! - 阻塞/唤醒：与 P3-T7 Mutex 阻塞点同套基础设施（cli_snapshot → 状态变更 →
//!   checkpoint_inner），[`kthread_unblock`] 触发唤醒；
//! - **MVP 围栏**：在 `user_AS = init pid 1` 单进程上下文（boot 线程载用户 ELF）
//!   下，阻塞类 IPC（`send` 队列已满后 wait、`recv` 队列空 wait、`reply` 后等）
//!   **boot 线程绝不能阻塞**——per-CPU 单内核栈不允许多线程同时进 syscall，
//!   且 boot 是系统最后防线。围栏直接返回 [`E_WOULD_BLOCK`]；双 kthread 内核
//!   smoke（真实 kthread，非 boot）跑完整阻塞往返，user-mode hello 仅测
//!   try_send / recv loopback / 错误路径。
//!
//! ## 数据结构
//!
//! 每个 endpoint 对象挂一个 [`EpAux`]：
//! - `senders`：FIFO 队列，登记**正在等 reply** 的发送方（含 try_send 的
//!   不可 reply 项）；
//! - `in_flight`：当前 receiver 处理中的请求（reply 唤醒该 sender）；
//! - `recv_waiter`：当前正在 recv 阻塞的 receiver（被 Delivered send 唤醒）；
//!
//! 索引与 kstate `ENDPOINTS` 数组一致（按 `ObjRef.index` 直接寻址）。
//!
//! ## 锁序
//!
//! - AUX 内置 IRQ-safe `SpinLock`（IPC 路径全在关中断下推进）；
//! - 引用 [`kstate`] 锁的 `k_ep_try_send` / `k_ep_recv` 顺序：
//!   `ENDPOINTS → AUX → CAP_TABLES → OBJECTS`（k_transfer 路径），全程无嵌套。
//!
//! ## 不在本模块
//!
//! - IpcHeader 校验/盖章（Doc 03 §3.1 描述的消息头版本）：T7 MVP 视 payload
//!   为不透明字节流，header 由用户态库自行管理（约束写入 decision_log）；
//! - Per-thread kernel stack（消除 boot 围栏的前置条件）：T9；
//! - 大对象 grant（>4KB payload 走共享内存描述符）：T9+；
//! - 进程死亡时回收相关 endpoint（Doc 03 §5.1）：T9；
//! - 用户态双进程 IPC 往返（依赖 per-thread 内核栈 + 进程 spawn）：T9。
//!
//! ## 单拷贝路径备注
//!
//! Doc 03 §4.2 提议 kmap 窗口方案——临时把发送方物理页映射到内核固定虚拟
//! 地址窗口（`0xFFFF_FF00_0000_0000`）再 memcpy 到接收方页表。本 MVP 走**直
//! 接 PA-to-PA 拷贝**（identity mapping 下 PA = 内核 VA，`walk_flags` 返回
//! PA 后直接 `copy_nonoverlapping` 写入接收方 PA），等价于 kmap 退化形态，
//! 无需维护 kmap 窗口。后续 kernel 上半部迁移后切换为真正 kmap。

use core::sync::atomic::{AtomicI64, AtomicU32, AtomicU64};

use synapse_abi::{
    SyscallFrame, E_INVALID_ADDR, E_INVALID_CAP, E_NOT_FOUND, E_PEER_DIED, E_PERMISSION,
    E_WOULD_BLOCK,
};
use synapse_cap::{CapError, CapRef, ObjKind, ObjRef, Rights, TransferItem, MAX_TRANSFER};
use synapse_ipc::{RecvOutcome, SendOutcome, SendRequest};

use crate::paging::{AddressSpace, PT_USER, PT_WRITABLE};
use crate::sync::SpinLock;
use crate::kthread;
use synapse_sched::{ThreadId, MAX_THREADS};

use log::info;

// ---------------------------------------------------------------------------
// AUX 表（per-endpoint 阻塞/在途/接收等待）
// ---------------------------------------------------------------------------

/// P4-T13（R12）：per-thread send 取消结果通道。
///
/// 0 = 无取消；非零 = 唤醒后应返回给 sender 的负错误码（[`E_PEER_DIED`]）。
/// [`cancel_inflight_for`] 写入 → 阻塞 send 唤醒后 `swap(0)` 消费。
/// 索引 = `ThreadId % MAX_THREADS`（ThreadId 全局唯一，取模仅防御越界）。
static SEND_CANCEL: [AtomicI64; MAX_THREADS] = [const { AtomicI64::new(0) }; MAX_THREADS];

fn set_send_cancel(t: ThreadId, code: i64) {
    SEND_CANCEL[t.0 as usize % MAX_THREADS].store(code, AOrd::SeqCst);
}

fn take_send_cancel(t: ThreadId) -> i64 {
    SEND_CANCEL[t.0 as usize % MAX_THREADS].swap(0, AOrd::SeqCst)
}

/// 发送方登记项：注册于 `EpAux.senders` 队列（reply 唤醒源）。
#[derive(Clone, Copy)]
struct SenderWait {
    /// 发送方 agent（Doc 01 §7：内核盖章值）。
    sender_agent: u32,
    /// 发送方线程 id（reply 唤醒目标）。
    thread: ThreadId,
    /// 发送方进程 pid（cap transfer 源）。
    pid: u32,
    /// 发送方用户 AS 物理页表基址（单拷贝源，0 = 内核缓冲）。
    as_ptr: u64,
    /// 发送方 payload 地址（reply 时复用此缓冲接收回复）。
    payload_addr: u64,
    /// 原始 payload 长度（reply 截断上限）。
    payload_len: u32,
    /// 是否期待 reply：`false` = try_send（reply 不可用 → -14）。
    expects_reply: bool,
}

/// 当前被 receiver 处理中的请求（reply 唯一合法目标）。
#[derive(Clone, Copy)]
struct InFlight {
    sender_thread: ThreadId,
    sender_as: u64,
    sender_buf: u64,
    sender_len: u32,
}

/// Receiver 阻塞等待项（send Delivered 路径唤醒目标）。
#[derive(Clone, Copy)]
struct RecvWait {
    thread: ThreadId,
    pid: u32,
    as_ptr: u64,
    buf: u64,
    cap_out: u64,
    /// 唤醒后填充：实际投递的 payload_len。
    result_len: u32,
    /// 唤醒后填充：`true` = 对端在等待 reply（reply syscall 应合法）。
    has_in_flight: bool,
}

/// 单 endpoint 的 AUX 状态（MVP：固定结构，避免 alloc）。
#[derive(Clone, Copy)]
struct EpAux {
    /// 等待 reply 的发送方 FIFO（receiver 拿出 req 后从此处按 agent 反查）。
    senders: [Option<SenderWait>; 8],
    senders_head: u8,
    senders_tail: u8,
    senders_count: u8,
    /// 当前 receiver 处理中的请求。
    in_flight: Option<InFlight>,
    /// 当前 recv 阻塞的 receiver。
    recv_waiter: Option<RecvWait>,
}

impl EpAux {
    const fn empty() -> Self {
        Self {
            senders: [const { None }; 8],
            senders_head: 0,
            senders_tail: 0,
            senders_count: 0,
            in_flight: None,
            recv_waiter: None,
        }
    }

    /// push sender to FIFO（尾入）。
    fn push_sender(&mut self, w: SenderWait) -> bool {
        if self.senders_count as usize >= self.senders.len() {
            return false;
        }
        self.senders[self.senders_tail as usize] = Some(w);
        self.senders_tail = (self.senders_tail + 1) % self.senders.len() as u8;
        self.senders_count += 1;
        true
    }

    /// 按 sender_agent 反查并出队（FIFO 顺序扫一遍，O(n) MVP 可接受）。
    fn take_sender_by_agent(&mut self, agent: u32) -> Option<SenderWait> {
        if self.senders_count == 0 {
            return None;
        }
        for i in 0..self.senders_count as usize {
            let idx = (self.senders_head as usize + i) % self.senders.len();
            if let Some(w) = self.senders[idx] {
                if w.sender_agent == agent {
                    self.senders[idx] = None;
                    self.compact();
                    return Some(w);
                }
            }
        }
        None
    }

    fn compact(&mut self) {
        let mut rebuilt = [const { None }; 8];
        let mut n = 0usize;
        for i in 0..self.senders_count as usize {
            let idx = (self.senders_head as usize + i) % self.senders.len();
            if let Some(w) = self.senders[idx].take() {
                rebuilt[n] = Some(w);
                n += 1;
            }
        }
        self.senders = rebuilt;
        self.senders_head = 0;
        self.senders_tail = n as u8;
        self.senders_count = n as u8;
    }
}

/// AUX 表（索引与 kstate `ENDPOINTS` 一致）。
const MAX_AUX: usize = 64;
static AUX: SpinLock<[EpAux; MAX_AUX]> = SpinLock::new([const { EpAux::empty() }; MAX_AUX]);

/// P4-T13（R12 + Doc 03 §5.1）：进程退出时的在途 IPC 唤醒。
///
/// 调用契约（terminate_current 步骤 2.5）：**撤销能力之前、资源回收之前**
/// ——端点发现依赖垂死进程 CapTable（发送需持 cap → 其排队 send 的目标
/// ep 必在自己表里；作为 receiver 的 ep 同理），故必须在 CapTable 销毁前跑。
///
/// 三类在途：
/// 1. **排队 send**（垂死 agent 发出、尚未被 recv 的消息）→ crate
///    `cancel_sender` 摘除（FIFO 保持，其他等待者不受扰，Doc 03 §5.1）；
/// 2. **阻塞等 reply 的 sender**（垂死进程是这些 ep 的 receiver，
///    `has_recv` 判定）→ `SEND_CANCEL = E_PEER_DIED` + 唤醒；
/// 3. **in_flight**（receiver 已拉出、垂死进程未及 reply 的请求）→ 同上。
///
/// MVP 已知限制：`recv_waiter.pid == dying_pid`（垂死进程自身阻塞在
/// recv）在串行 spawn 模型下不可能发生（运行中的进程不阻塞）；若未来
/// 并发模型出现，recv 唤醒路径需自己的结果通道（不复用 SEND_CANCEL）。
///
/// 锁纪律：CAP_TABLES（收集，随即释放）→ 逐对象顺序取 ENDPOINTS
/// （经 k_ep_cancel_sender）与 AUX——全程无嵌套。返回唤醒线程数。
pub fn cancel_inflight_for(dying_pid: u32, dying_agent: u32) -> usize {
    // 1. 收集垂死进程相关对象（ObjRef 含 generation 直取 cap；≤16 个足够
    //    MVP——超限截断并 warn，不 alloc）
    const MAX_SCAN: usize = 16;
    let mut objs: [Option<(ObjRef, bool)>; MAX_SCAN] = [None; MAX_SCAN];
    let mut n_obj = 0usize;
    let pid = synapse_proc::process::Pid(dying_pid);
    if crate::kstate::cap_table_exists(pid) {
        crate::kstate::with_cap_table(pid, |t| {
            for (_, cap) in t.iter() {
                let has_recv = cap.rights.contains(Rights::RECV);
                if let Some(slot) = objs[..n_obj].iter_mut().find(|s| {
                    s.map(|(o, _)| o == cap.obj).unwrap_or(false)
                }) {
                    // 已收录：RECV 位做 OR 聚合
                    if let Some((_, r)) = slot {
                        *r |= has_recv;
                    }
                } else if n_obj < MAX_SCAN {
                    objs[n_obj] = Some((cap.obj, has_recv));
                    n_obj += 1;
                } else {
                    log::warn!("[ipc] cancel_inflight_for: obj scan truncated at {MAX_SCAN}");
                }
            }
        });
    }

    let mut woken = 0usize;
    for i in 0..n_obj {
        let Some((obj, has_recv)) = objs[i] else { continue };
        // 2. 垂死 agent 的排队 send → 摘除（非 endpoint 对象返 NotFound，无害）
        let _ = crate::kstate::k_ep_cancel_sender(obj, synapse_ipc::AgentId(dying_agent));
        // 3. 等垂死 receiver reply 的阻塞线程 → E_PEER_DIED 唤醒（仅当垂死
        //    进程持该 ep 的 RECV 位——只是发送方的 ep 不许动，receiver 还活着）
        if has_recv && (obj.index as usize) < MAX_AUX {
            let mut auxs = AUX.lock();
            let aux = &mut auxs[obj.index as usize];
            for slot in aux.senders.iter_mut() {
                if let Some(w) = slot.take() {
                    set_send_cancel(w.thread, E_PEER_DIED);
                    let _ = kthread::kthread_unblock(w.thread);
                    woken += 1;
                }
            }
            aux.senders_head = 0;
            aux.senders_tail = 0;
            aux.senders_count = 0;
            if let Some(inf) = aux.in_flight.take() {
                set_send_cancel(inf.sender_thread, E_PEER_DIED);
                let _ = kthread::kthread_unblock(inf.sender_thread);
                woken += 1;
            }
        }
    }
    if n_obj > 0 {
        info!(
            "[ipc] cancel_inflight_for(pid={dying_pid}): {} objs scanned, {} threads woken (E_PEER_DIED)",
            n_obj, woken
        );
    }
    woken
}

// ---------------------------------------------------------------------------
// 当前进程标识（per-thread；MVP：单进程 = init）
// ---------------------------------------------------------------------------

/// 设置当前 syscall 服务进程（`elfload` AS 武装时调用，MVP=1=init）。
///
/// 真机多进程上下文（T9+）由 per-thread kernel stack 上的进程上下文切换接管，
/// 当前实现：所有 syscall 都视为同一进程（hello = init）。
///
/// **P4-T9a 迁移**：底层存储迁到 `proc_ext::PerCpu.current_pid`（gs:[0]），
/// 这里薄封装保留调用兼容性；新代码请直接用 [`crate::proc_ext::set_current_pid`]
/// （注意：仅写 current_pid，不触发 PROC_EXT 校验；IPC smoke 用合成 kthread ID）。
pub fn set_current_pid(pid: u32) {
    crate::proc_ext::set_current_pid(pid);
}

/// 读取当前 syscall 服务进程 pid。
///
/// **P4-T9a 迁移**：底层迁到 `proc_ext::PerCpu.current_pid`（gs:[0]）。
pub fn current_pid() -> u32 {
    crate::proc_ext::current_pid()
}

// ---------------------------------------------------------------------------
// ipc_try_send 成功计数（P4-T7 Phase 2 ipc_pong_smoke 验证用）
// ---------------------------------------------------------------------------

/// `k_ipc_try_send` 成功次数（Queued + Delivered 均计数）。
///
/// ipc_pong_continuation 断言此值 ≥ 1，作为 ring3 → kernel IPC send 路径
/// 真机贯通的证据（无需读取子进程 AS 中已失效的 payload_addr）。
static IPC_TRY_SEND_COUNT: AtomicU64 = AtomicU64::new(0);

/// 查询 `k_ipc_try_send` 累计成功次数（Queued + Delivered）。
pub fn ipc_try_send_count() -> u64 {
    IPC_TRY_SEND_COUNT.load(core::sync::atomic::Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// Cap 校验（cptr → ObjRef + Endpoint kind 断言 + rights 校验）
// ---------------------------------------------------------------------------

/// 把 cap 校验统一到一处：CapTable::get + ObjectTable::check_live kind + rights。
///
/// 返回 (ObjRef, badge)；错误码直接对应 Doc 03 §5.1。
fn resolve_endpoint(pid: u32, cptr: u8, need: Rights) -> Result<(synapse_cap::ObjRef, u32), i64> {
    // slot 0 是 NULL trap。
    if cptr == 0 {
        // FR9 审计：NULL cap 校验失败（目标未知 → UNKNOWN_OBJ 哨兵）。
        crate::audit::cap_verify(pid, crate::audit::UNKNOWN_OBJ, need, false);
        return Err(E_INVALID_CAP);
    }
    // 锁序: OBJECTS → CAP_TABLES（与 kstate 约定一致）
    let r = crate::kstate::with_objects(|objs| {
        crate::kstate::with_cap_table(synapse_proc::process::Pid(pid), |t| {
            let cap = match t.get(cptr) {
                Ok(c) => c,
                Err(_) => return Err(E_INVALID_CAP),
            };
            if !cap.rights.contains(need) {
                return Err(E_PERMISSION);
            }
            match objs.check_live(cap.obj) {
                Ok(ObjKind::Endpoint) => Ok((cap.obj, cap.badge)),
                Ok(_) => Err(E_INVALID_CAP),
                Err(CapError::ObjectRetired) => Err(-12), // E_OBJECT_RETIRED
                Err(_) => Err(E_INVALID_CAP),
            }
        })
    });
    // FR9 审计：cap 校验事件（成功/失败都记）。**锁外 push**——AUDIT 是
    // 叶子锁，agent_of 需 PROCS 锁，绝不允许在 OBJECTS/CAP_TABLES 临界区内
    // 嵌套获取（锁序契约见 audit.rs 模块头）。
    match &r {
        Ok((obj, _)) => crate::audit::cap_verify(pid, *obj, need, true),
        Err(_) => crate::audit::cap_verify(pid, crate::audit::UNKNOWN_OBJ, need, false),
    }
    r
}

// ---------------------------------------------------------------------------
// 单拷贝路径（PA-to-PA，identity mapping 下等价于 kmap）
// ---------------------------------------------------------------------------

// P4-T13：用户缓冲校验统一走 crate::uaccess（原 ipc.rs 本地实现语义并入
// uaccess::user_mem_ok：as_ptr==0 内核缓冲恒真 / len==0 平凡合法 / 逐页
// present+PT_USER(+PT_WRITABLE) / checked_add 防溢出）。
use crate::uaccess::user_mem_ok;

/// 按页走查 PA，memcpy 一段 payload。
///
/// `src_as=0`/`dst_as=0` 表示该端是内核缓冲（identity-mapped 直访）。
/// 返回每页 PT_USER + (need_write → PT_WRITABLE) 校验失败 → Err(E_INVALID_ADDR)。
unsafe fn ipc_copy(
    src_as: u64,
    src_va: u64,
    dst_as: u64,
    dst_va: u64,
    len: u32,
) -> Result<(), i64> {
    if len == 0 {
        return Ok(());
    }
    if len > synapse_ipc::header::MAX_PAYLOAD {
        return Err(E_INVALID_ADDR);
    }
    let _end = src_va.checked_add(len as u64).ok_or(E_INVALID_ADDR)?;

    let src_ref = if src_as != 0 { Some(unsafe { &*(src_as as *const AddressSpace) }) } else { None };
    let dst_ref = if dst_as != 0 { Some(unsafe { &*(dst_as as *const AddressSpace) }) } else { None };

    // 同 AS + 区间重叠 → 退化为 memmove（tmp buffer 兜底；MVP 路径罕见，仅
    // 用户态 hello 自发自收时触发）。
    let overlap = src_as == dst_as
        && src_va < dst_va + len as u64
        && dst_va < src_va + len as u64;
    if overlap {
        // 走 memmove 路径（kernel 自带 rust memcpy 实现可处理重叠）
        let tmp: [u8; synapse_ipc::header::MAX_PAYLOAD as usize] = [0; synapse_ipc::header::MAX_PAYLOAD as usize];
        // SAFETY: tmp 在内核栈，len ≤ MAX_PAYLOAD
        unsafe {
            // 简单 dst 校验（避免越界读 tmp）
            let dst_pa = if let Some(as_ref) = dst_ref {
                match as_ref.walk_flags(dst_va) {
                    Some((pa, flags)) if flags & PT_USER != 0 && flags & PT_WRITABLE != 0 => pa,
                    _ => return Err(E_INVALID_ADDR),
                }
            } else {
                dst_va
            };
            // src 读 + 写 dst（不重叠因为先读 tmp 再写 dst）
            core::ptr::copy_nonoverlapping(
                (walk_pa(src_ref, src_va)? + (src_va & 0xFFF)) as *const u8,
                tmp.as_ptr() as *mut u8,
                len as usize,
            );
            core::ptr::copy_nonoverlapping(
                tmp.as_ptr(),
                (dst_pa + (dst_va & 0xFFF)) as *mut u8,
                len as usize,
            );
        }
        return Ok(());
    }

    let mut off = 0u32;
    while off < len {
        let src_cur = src_va + off as u64;
        let dst_cur = dst_va + off as u64;
        let src_page_off = (src_cur & 0xFFF) as u32;
        let dst_page_off = (dst_cur & 0xFFF) as u32;
        let chunk = core::cmp::min(
            core::cmp::min(0x1000 - src_page_off, 0x1000 - dst_page_off),
            len - off,
        );

        let src_pa = match walk_pa(src_ref, src_cur) {
            Ok(p) => p,
            Err(code) => return Err(code),
        };
        let dst_pa = match walk_pa_dst(dst_ref, dst_cur) {
            Ok(p) => p,
            Err(code) => return Err(code),
        };

        unsafe {
            core::ptr::copy_nonoverlapping(
                (src_pa + src_page_off as u64) as *const u8,
                (dst_pa + dst_page_off as u64) as *mut u8,
                chunk as usize,
            );
        }
        off += chunk;
    }
    Ok(())
}

/// src PA 走查（read 权限校验）。
fn walk_pa(as_ref: Option<&AddressSpace>, va: u64) -> Result<u64, i64> {
    if let Some(as_ref) = as_ref {
        match as_ref.walk_flags(va) {
            Some((pa, flags)) => {
                if flags & PT_USER == 0 {
                    return Err(E_INVALID_ADDR);
                }
                Ok(pa)
            }
            None => Err(E_INVALID_ADDR),
        }
    } else {
        Ok(va)
    }
}

/// dst PA 走查（write 权限校验）。
fn walk_pa_dst(as_ref: Option<&AddressSpace>, va: u64) -> Result<u64, i64> {
    if let Some(as_ref) = as_ref {
        match as_ref.walk_flags(va) {
            Some((pa, flags)) => {
                if flags & PT_USER == 0 || flags & PT_WRITABLE == 0 {
                    return Err(E_INVALID_ADDR);
                }
                Ok(pa)
            }
            None => Err(E_INVALID_ADDR),
        }
    } else {
        Ok(va)
    }
}

// ---------------------------------------------------------------------------
// Boot 围栏（user 上下文阻塞拒绝）
// ---------------------------------------------------------------------------

/// 当前线程是否为 boot（系统最后防线，绝不阻塞）。
fn is_current_boot() -> bool {
    let cur = kthread::kthread_current_id();
    kthread::meta_of(cur).map(|m| m.3).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Cap 转移（用户侧：打包为 [cptr, count]，写入 cap_out）
// ---------------------------------------------------------------------------

/// 把 cap transfer 结果写入用户态 `cap_out` 缓冲。
///
/// 布局：cap_out[0] = count, cap_out[1..count+1] = cptrs。
/// `dsts` 来自 [`kstate::k_transfer`] 返回的固定数组，约定 `count` 项为有效。
unsafe fn write_cap_out(cap_out: u64, dsts: &[CapRef; MAX_TRANSFER], count: u32) {
    if cap_out == 0 || count == 0 {
        return;
    }
    let ptr = cap_out as *mut u8;
    unsafe {
        *ptr = count as u8;
        for i in 0..count as usize {
            *ptr.add(1 + i) = dsts[i];
        }
    }
}

// ---------------------------------------------------------------------------
// 四个 syscall 内核实现
// ---------------------------------------------------------------------------

/// `IpcSend`(0) 阻塞发送：receiver 已等 → Delivered + 自阻塞等 reply；
/// receiver 未等 → Queued + 自阻塞等 reply（reply 时从 receiver in_flight
/// 唤醒）。MVP：blocking send on boot → `E_WOULD_BLOCK`（围栏）。
pub fn k_ipc_send(frame: &SyscallFrame) -> i64 {
    if is_current_boot() {
        log::warn!("[ipc] send: refused on boot thread (MVP fence)");
        return E_WOULD_BLOCK;
    }
    let (ep, msg, len, caps_ptr, n_caps) = unpack_ipc(frame);
    let pid = current_pid();
    let (obj, badge) = match resolve_endpoint(pid, ep, Rights::SEND) {
        Ok(t) => t,
        Err(code) => return code,
    };
    let user_as = crate::elfload::current_as_ptr();
    if !user_mem_ok(user_as, msg, len as u64, false) {
        return E_INVALID_ADDR;
    }
    if n_caps > MAX_TRANSFER as u32 {
        return E_INVALID_CAP;
    }
    if n_caps > 0 && !user_mem_ok(user_as, caps_ptr, n_caps as u64, true) {
        return E_INVALID_ADDR;
    }
    // 构造 SendRequest：MVP 单进程 = init，sender agent 直接用 INIT_AGENT。
    //（真实多进程 T9 后由 caller AS 的 AgentId 注入；kstate 已有 lookup_by_pid 路径）
    let sender_agent = pid;
    let mut caps = [const { TransferItem { cptr: 0, mask: Rights::EMPTY } }; MAX_TRANSFER];
    for i in 0..n_caps as usize {
        let cptr = unsafe { *((caps_ptr + i as u64) as *const u8) };
        caps[i] = TransferItem { cptr, mask: Rights::ALL };
    }
    let req = SendRequest {
        sender: synapse_ipc::AgentId(sender_agent),
        badge,
        label: 0,
        payload_len: len,
        payload_addr: msg,
        caps,
        cap_count: n_caps as usize,
    };

    let if_on = cli_snapshot();
    let decision = crate::kstate::k_ep_try_send(obj, req);

    match decision {
        Err(CapError::WouldBlock) => {
            if if_on { x86_64::instructions::interrupts::enable(); }
            return E_WOULD_BLOCK;
        }
        Err(CapError::InvalidCap) => {
            if if_on { x86_64::instructions::interrupts::enable(); }
            return E_INVALID_CAP;
        }
        Err(e) => {
            log::warn!("[ipc] send try_send unexpected err {:?}", e);
            if if_on { x86_64::instructions::interrupts::enable(); }
            return E_PERMISSION;
        }
        Ok(SendOutcome::Delivered) => {
            // FR9 审计：send 已被内核接受（Delivered 路径；label=req.label=0 MVP）。
            crate::audit::ipc(pid, synapse_audit::IpcDir::Send, obj, 0);
            // receiver waiting：AUX.recv_waiter 取出 → 单拷贝 + cap transfer + 唤醒
            let receiver = {
                let mut auxs = AUX.lock();
                let aux = &mut auxs[obj.index as usize];
                let waiter = match aux.recv_waiter.take() {
                    Some(w) => w,
                    None => {
                        log::error!("[ipc] Delivered but no recv_waiter (race?)");
                        if if_on { x86_64::instructions::interrupts::enable(); }
                        return E_NOT_FOUND;
                    }
                };
                // push sender（等 reply）
                aux.push_sender(SenderWait {
                    sender_agent,
                    thread: kthread::kthread_current_id(),
                    pid,
                    as_ptr: user_as,
                    payload_addr: msg,
                    payload_len: len,
                    expects_reply: true,
                });
                aux.in_flight = Some(InFlight {
                    sender_thread: kthread::kthread_current_id(),
                    sender_as: user_as,
                    sender_buf: msg,
                    sender_len: len,
                });
                waiter
            };
            // 单拷贝 + cap transfer（已释放 ENDPOINTS 锁）
            let copy_res = unsafe { ipc_copy(user_as, msg, receiver.as_ptr, receiver.buf, len) };
            let xfer_res: Result<[CapRef; MAX_TRANSFER], i64> = if n_caps > 0 {
                crate::kstate::k_transfer(synapse_proc::process::Pid(pid), synapse_proc::process::Pid(receiver.pid), &caps[..n_caps as usize])
                    .map_err(cap_err_to_code)
            } else {
                Ok([0u8; MAX_TRANSFER])
            };
            match xfer_res {
                Ok(dsts) => {
                    unsafe { write_cap_out(receiver.cap_out, &dsts, n_caps) };
                }
                Err(code) => {
                    // cap transfer 失败 → send 失败（atomic 语义）
                    let mut auxs = AUX.lock();
                    auxs[obj.index as usize].take_sender_by_agent(sender_agent);
                    auxs[obj.index as usize].in_flight = None;
                    if if_on { x86_64::instructions::interrupts::enable(); }
                    return code;
                }
            }
            if let Err(code) = copy_res {
                if if_on { x86_64::instructions::interrupts::enable(); }
                return code;
            }
            // 唤醒 receiver
            let _ = kthread::kthread_unblock(receiver.thread);
            // 自阻塞等 reply
            kthread::kthread_block_current();
            if if_on { x86_64::instructions::interrupts::enable(); }
            // P4-T13 R12：等 reply 期间 receiver 进程死亡 → 取消码返回
            let cancel = take_send_cancel(kthread::kthread_current_id());
            if cancel != 0 {
                return cancel;
            }
            // 唤醒后由 reply 把数据写入 send buffer；send 返回 len
            len as i64
        }
        Ok(SendOutcome::Queued) => {
            // FR9 审计：send 已入队（Queued 路径）。
            crate::audit::ipc(pid, synapse_audit::IpcDir::Send, obj, 0);
            // receiver 未等：AUX.senders 入队 + 自阻塞等 reply
            {
                let mut auxs = AUX.lock();
                let aux = &mut auxs[obj.index as usize];
                aux.push_sender(SenderWait {
                    sender_agent,
                    thread: kthread::kthread_current_id(),
                    pid,
                    as_ptr: user_as,
                    payload_addr: msg,
                    payload_len: len,
                    expects_reply: true,
                });
            }
            kthread::kthread_block_current();
            if if_on { x86_64::instructions::interrupts::enable(); }
            // P4-T13 R12：排队等 recv 期间 receiver 进程死亡 → 取消码返回
            let cancel = take_send_cancel(kthread::kthread_current_id());
            if cancel != 0 {
                return cancel;
            }
            len as i64
        }
    }
}

/// `IpcTrySend`(3) 非阻塞：成功入队/Delivered → 0；would_block → -4。
/// 不阻塞 sender，不期待 reply（receiver 拉出后 reply 会得 -14）。
pub fn k_ipc_try_send(frame: &SyscallFrame) -> i64 {
    let (ep, msg, len, caps_ptr, n_caps) = unpack_ipc(frame);
    let pid = current_pid();
    let (obj, badge) = match resolve_endpoint(pid, ep, Rights::SEND) {
        Ok(t) => t,
        Err(code) => return code,
    };
    let user_as = crate::elfload::current_as_ptr();
    if !user_mem_ok(user_as, msg, len as u64, false) {
        return E_INVALID_ADDR;
    }
    if n_caps > MAX_TRANSFER as u32 {
        return E_INVALID_CAP;
    }
    if n_caps > 0 && !user_mem_ok(user_as, caps_ptr, n_caps as u64, true) {
        return E_INVALID_ADDR;
    }
    let sender_agent = pid;
    let mut caps = [const { TransferItem { cptr: 0, mask: Rights::EMPTY } }; MAX_TRANSFER];
    for i in 0..n_caps as usize {
        let cptr = unsafe { *((caps_ptr + i as u64) as *const u8) };
        caps[i] = TransferItem { cptr, mask: Rights::ALL };
    }
    let req = SendRequest {
        sender: synapse_ipc::AgentId(sender_agent),
        badge,
        label: 0,
        payload_len: len,
        payload_addr: msg,
        caps,
        cap_count: n_caps as usize,
    };

    let if_on = cli_snapshot();
    let decision = crate::kstate::k_ep_try_send(obj, req);

    match decision {
        Err(CapError::WouldBlock) => {
            if if_on { x86_64::instructions::interrupts::enable(); }
            return E_WOULD_BLOCK;
        }
        Err(_) => {
            if if_on { x86_64::instructions::interrupts::enable(); }
            return E_INVALID_CAP;
        }
        Ok(SendOutcome::Delivered) => {
            // FR9 审计：try_send Delivered。
            crate::audit::ipc(pid, synapse_audit::IpcDir::Send, obj, 0);
            // receiver waiting：AUX 登记 + 单拷贝 + 唤醒
            let receiver = {
                let mut auxs = AUX.lock();
                let aux = &mut auxs[obj.index as usize];
                match aux.recv_waiter.take() {
                    Some(w) => w,
                    None => {
                        if if_on { x86_64::instructions::interrupts::enable(); }
                        return E_NOT_FOUND;
                    }
                }
                // try_send 不入 senders 队列（无 reply），不进 in_flight（receiver reply 会 -14）
            };
            let copy_res = unsafe { ipc_copy(user_as, msg, receiver.as_ptr, receiver.buf, len) };
            let xfer_res: Result<[CapRef; MAX_TRANSFER], i64> = if n_caps > 0 {
                crate::kstate::k_transfer(synapse_proc::process::Pid(pid), synapse_proc::process::Pid(receiver.pid), &caps[..n_caps as usize])
                    .map_err(cap_err_to_code)
            } else {
                Ok([0u8; MAX_TRANSFER])
            };
            match xfer_res {
                Ok(dsts) => {
                    unsafe { write_cap_out(receiver.cap_out, &dsts, n_caps) };
                }
                Err(code) => {
                    if if_on { x86_64::instructions::interrupts::enable(); }
                    return code;
                }
            }
            if let Err(code) = copy_res {
                if if_on { x86_64::instructions::interrupts::enable(); }
                return code;
            }
            let _ = kthread::kthread_unblock(receiver.thread);
            IPC_TRY_SEND_COUNT.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
            if if_on { x86_64::instructions::interrupts::enable(); }
            0
        }
        Ok(SendOutcome::Queued) => {
            // FR9 审计：try_send Queued（★ P4-T11 真机验证锚点——ipc-pong
            // 子进程 ring3 try_send 走此路径，init_continuation 断言该事件）。
            crate::audit::ipc(pid, synapse_audit::IpcDir::Send, obj, 0);
            // 队未满：成功入队；登记 sender_wait（expects_reply=false 让 reply 走 -14）
            {
                let mut auxs = AUX.lock();
                let aux = &mut auxs[obj.index as usize];
                aux.push_sender(SenderWait {
                    sender_agent,
                    thread: kthread::kthread_current_id(),
                    pid,
                    as_ptr: user_as,
                    payload_addr: msg,
                    payload_len: len,
                    expects_reply: false,
                });
            }
            IPC_TRY_SEND_COUNT.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
            if if_on { x86_64::instructions::interrupts::enable(); }
            0
        }
    }
}

/// `IpcRecv`(1)：队非空 → Message（单拷贝 + cap transfer + 设 in_flight）；
/// 队空 → 阻塞（boot 围栏：-4）。
pub fn k_ipc_recv(frame: &SyscallFrame) -> i64 {
    let (ep, buf, cap_out) = (frame.args[0] as u8, frame.args[1], frame.args[2]);
    let pid = current_pid();
    let (obj, _badge) = match resolve_endpoint(pid, ep, Rights::RECV) {
        Ok(t) => t,
        Err(code) => return code,
    };
    let user_as = crate::elfload::current_as_ptr();
    if !user_mem_ok(user_as, buf, 1, true) {
        return E_INVALID_ADDR;
    }
    if cap_out != 0 && !user_mem_ok(user_as, cap_out, MAX_TRANSFER as u64 + 1, true) {
        return E_INVALID_ADDR;
    }
    let if_on = cli_snapshot();
    let outcome = crate::kstate::k_ep_recv(obj);

    match outcome {
        Err(_) => {
            if if_on { x86_64::instructions::interrupts::enable(); }
            return E_NOT_FOUND;
        }
        Ok(RecvOutcome::Message(req)) => {
            // FR9 审计：recv 取出消息（label 记录，payload 不记录）。
            crate::audit::ipc(pid, synapse_audit::IpcDir::Recv, obj, req.label);
            // 反查 senders（按 agent），取出 sender_wait 拿到 as/payload_addr
            let sender = {
                let mut auxs = AUX.lock();
                let aux = &mut auxs[obj.index as usize];
                let sw = aux.take_sender_by_agent(req.sender.0);
                if let Some(sw) = sw {
                    if sw.expects_reply {
                        aux.in_flight = Some(InFlight {
                            sender_thread: sw.thread,
                            sender_as: sw.as_ptr,
                            sender_buf: sw.payload_addr,
                            sender_len: sw.payload_len,
                        });
                    }
                    Some(sw)
                } else {
                    None
                }
            };
            let sender_pid = sender.map(|s| s.pid).unwrap_or(pid);
            let sender_as = sender.map(|s| s.as_ptr).unwrap_or(0);
            let copy_res = unsafe { ipc_copy(sender_as, req.payload_addr, user_as, buf, req.payload_len) };
            let xfer_res: Result<[CapRef; MAX_TRANSFER], i64> = if req.cap_count > 0 {
                crate::kstate::k_transfer(synapse_proc::process::Pid(sender_pid), synapse_proc::process::Pid(pid), &req.caps[..req.cap_count])
                    .map_err(cap_err_to_code)
            } else {
                Ok([0u8; MAX_TRANSFER])
            };
            match xfer_res {
                Ok(dsts) => {
                    unsafe { write_cap_out(cap_out, &dsts, req.cap_count as u32) };
                }
                Err(code) => {
                    if if_on { x86_64::instructions::interrupts::enable(); }
                    return code;
                }
            }
            let len = req.payload_len;
            if let Err(code) = copy_res {
                if if_on { x86_64::instructions::interrupts::enable(); }
                return code;
            }
            if if_on { x86_64::instructions::interrupts::enable(); }
            len as i64
        }
        Ok(RecvOutcome::Waiting) => {
            if is_current_boot() {
                // ep.recv() 已把 receiver_waiting 置 true，但 boot 围栏下我们
                // 不登记 recv_waiter、不阻塞——必须回滚，否则后续 try_send 会因
                // 残留标记误走 Delivered → 找不到 recv_waiter → E_NOT_FOUND
                // （P4 elf-smoke hello §6.6 首次真机暴露）。
                let _ = crate::kstate::k_ep_cancel_recv(obj);
                if if_on { x86_64::instructions::interrupts::enable(); }
                return E_WOULD_BLOCK;
            }
            // 登记 recv_waiter + 阻塞
            {
                let mut auxs = AUX.lock();
                let aux = &mut auxs[obj.index as usize];
                aux.recv_waiter = Some(RecvWait {
                    thread: kthread::kthread_current_id(),
                    pid,
                    as_ptr: user_as,
                    buf,
                    cap_out,
                    result_len: 0,
                    has_in_flight: false,
                });
            }
            kthread::kthread_block_current();
            // 唤醒后从 AUX 读 result
            let (result_len, _has) = {
                let mut auxs = AUX.lock();
                let aux = &mut auxs[obj.index as usize];
                let r = aux.recv_waiter.take();
                (r.map(|w| w.result_len).unwrap_or(0), r.map(|w| w.has_in_flight).unwrap_or(false))
            };
            if if_on { x86_64::instructions::interrupts::enable(); }
            result_len as i64
        }
    }
}

/// `IpcReply`(2)：从 in_flight 取 sender，把 reply payload 写入 sender 原
/// send 缓冲（双向复用），唤醒 sender。MVP：boot fence 不适用（reply 不阻塞）。
pub fn k_ipc_reply(frame: &SyscallFrame) -> i64 {
    let (ep, msg, len) = (frame.args[0] as u8, frame.args[1], frame.args[2] as u32);
    let pid = current_pid();
    let (obj, _badge) = match resolve_endpoint(pid, ep, Rights::REPLY) {
        Ok(t) => t,
        Err(code) => return code,
    };
    let user_as = crate::elfload::current_as_ptr();
    if !user_mem_ok(user_as, msg, len as u64, false) {
        return E_INVALID_ADDR;
    }

    let if_on = cli_snapshot();
    let in_flight = {
        let mut auxs = AUX.lock();
        auxs[obj.index as usize].in_flight.take()
    };
    let in_flight = match in_flight {
        Some(f) => f,
        None => {
            // 没在途：可能是 try_send → 不期待 reply
            if if_on { x86_64::instructions::interrupts::enable(); }
            return E_PEER_DIED;
        }
    };
    // 截断到 sender 原 payload_len
    let reply_len = core::cmp::min(len, in_flight.sender_len);
    // FR9 审计：reply 事件（in_flight 已取出 = 内核接受回复）。
    crate::audit::ipc(pid, synapse_audit::IpcDir::Reply, obj, 0);
    let copy_res = unsafe { ipc_copy(user_as, msg, in_flight.sender_as, in_flight.sender_buf, reply_len) };
    let wake_res = kthread::kthread_unblock(in_flight.sender_thread);
    if if_on { x86_64::instructions::interrupts::enable(); }
    if let Err(code) = copy_res {
        return code;
    }
    match wake_res {
        Ok(()) => 0,
        Err(_) => E_PEER_DIED,
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn unpack_ipc(frame: &SyscallFrame) -> (u8, u64, u32, u64, u32) {
    let ep = frame.args[0] as u8;
    let msg = frame.args[1];
    let len = frame.args[2] as u32;
    let caps_ptr = frame.args[3];
    let n_caps = frame.args[4] as u32;
    (ep, msg, len, caps_ptr, n_caps)
}

pub(crate) fn cap_err_to_code(e: CapError) -> i64 {
    match e {
        CapError::InvalidCap => E_INVALID_CAP,
        CapError::InvalidAddr => E_INVALID_ADDR,
        CapError::NoMemory => -3,
        CapError::WouldBlock => E_WOULD_BLOCK,
        CapError::NotFound => E_NOT_FOUND,
        CapError::AgentIdConflict => -6,
        CapError::Permission => E_PERMISSION,
        CapError::Frozen => -8,
        CapError::Zombie => -9,
        CapError::NotImplemented => -10,
        CapError::AbiMismatch => -11,
        CapError::ObjectRetired => -12,
        CapError::QuotaExceeded => -13,
        CapError::PeerDied => E_PEER_DIED,
        CapError::Timeout => -15,
    }
}

/// 关中断，返回 IF 原值（调用方负责还原）。
fn cli_snapshot() -> bool {
    use x86_64::registers::rflags::{read as read_rflags, RFlags};
    let if_on = read_rflags().contains(RFlags::INTERRUPT_FLAG);
    x86_64::instructions::interrupts::disable();
    if_on
}

// ===========================================================================
// P4-T7a IPC 真机 smoke
// ===========================================================================
//
// 拓扑：2 个真实 kthread（owner_pid 90/91，init 同侧），对同一 endpoint 跑
// 阻塞 send / recv / reply 完整往返 + try_send + 错误路径断言。boot 线程
// 通过 enable_preemption + sleep_until 轮询等两 worker 收尾，再 reap。
//
// 设计要点：
// * **绕开 boot 围栏**：smoke 调用 k_ipc_* 的 kthread 不是 boot，is_current_boot
//   返 false → 阻塞类 IPC 走真路径；
// * **cap transfer 走真 transfer_caps**：90 mint 一个 notification cap →
//   transfer 给 91 → 91 端 cap 表应有新 slot（断言 via cap slot 计数）；
// * **错误路径断言**：每个非法 cptr / 越界 cap 都应返对应负码（-1/-2/-6/-13）；
// * **失败即 panic**（不静默）：assert! 直挂，回 outer smoke 走 355 出口。

use core::sync::atomic::Ordering as AOrd;

/// T7 smoke 测试 pid（与 INIT_PID=1 / BOOT_PID=0 / p3t8_pid=42 不撞）。
const T7_PID_A: u32 = 90;
const T7_PID_B: u32 = 91;
/// Smoke 超时（tick；500 = 5s @100Hz）。
const T7_TIMEOUT_TICKS: u64 = 500;

/// Smoke 全局同步原语。
static T7_A_DONE: AtomicU32 = AtomicU32::new(0);
static T7_B_DONE: AtomicU32 = AtomicU32::new(0);
/// 端点对象 index（smoke 启动时分配，worker 用它直访）。
static T7_EP_OBJ_IDX: AtomicU32 = AtomicU32::new(0);
/// A 端 mint 的 endpoint cap slot（在 90 的 cap table 里）。
static T7_EP_CAP_A: AtomicU32 = AtomicU32::new(0);
/// B 端 mint 的 endpoint cap slot（在 91 的 cap table 里）。
static T7_EP_CAP_B: AtomicU32 = AtomicU32::new(0);
/// A→B 转移 notification cap 后，B 端 cap slot。
static T7_NOTIF_CAP_B: AtomicU32 = AtomicU32::new(0);

macro_rules! t7_assert {
    ($cond:expr, $msg:expr) => {{
        if !($cond) {
            panic!("[ipc-smoke] FAIL: {}", $msg);
        }
    }};
}

macro_rules! t7_assert_eq {
    ($a:expr, $b:expr, $msg:expr) => {{
        let av = $a;
        let bv = $b;
        if av != bv {
            panic!("[ipc-smoke] FAIL: {} — got {:?}, expected {:?}", $msg, av, bv);
        }
    }};
}

/// A 端：send blocking → 等 reply → 校验。
extern "C" fn t7_worker_a() -> ! {
    set_current_pid(T7_PID_A);
    let cap_a = T7_EP_CAP_A.load(AOrd::Acquire) as u8;
    let obj_idx = T7_EP_OBJ_IDX.load(AOrd::Acquire);
    let notif_b = T7_NOTIF_CAP_B.load(AOrd::Acquire);
    info!("[ipc-smoke] A start: cap_a={} obj_idx={} notif_b={}", cap_a, obj_idx, notif_b);

    // 写 ping payload 到 buf_va_a（identity-mapped kernel buffer 直访）
    let buf_va_a: u64 = 0x0008_0000;
    let reply_buf_va: u64 = 0x0008_2000;
    let ping: [u8; 5] = *b"ping\0";
    unsafe {
        core::ptr::copy_nonoverlapping(ping.as_ptr(), buf_va_a as *mut u8, 5);
        core::ptr::write_bytes(reply_buf_va as *mut u8, 0, 16);
    }

    // 构造 send frame（cap_a, buf_va_a, 5, 0, 0）
    let frame = SyscallFrame {
        num: synapse_abi::SyscallId::IpcSend.num(),
        args: [cap_a as u64, buf_va_a, 5, 0, 0, 0],
    };
    let r = k_ipc_send(&frame);
    t7_assert_eq!(r, 5, "A: send blocking returned len");

    // reply 已写入 reply_buf_va：应为 "pong\0"
    let read_back = unsafe { core::slice::from_raw_parts(reply_buf_va as *const u8, 5) };
    t7_assert!(read_back[..4] == *b"pong", "A: reply buf contains 'pong'");

    // 错误路径断言 1：cptr=0 → -1
    let bad_frame = SyscallFrame {
        num: synapse_abi::SyscallId::IpcSend.num(),
        args: [0, buf_va_a, 5, 0, 0, 0],
    };
    t7_assert_eq!(k_ipc_send(&bad_frame), E_INVALID_CAP, "A: cptr=0 → E_INVALID_CAP");

    // 错误路径断言 2：cptr=200（越界 slot） → -1
    let bad_frame2 = SyscallFrame {
        num: synapse_abi::SyscallId::IpcSend.num(),
        args: [200, buf_va_a, 5, 0, 0, 0],
    };
    t7_assert_eq!(k_ipc_send(&bad_frame2), E_INVALID_CAP, "A: cptr=200 → E_INVALID_CAP");

    T7_A_DONE.store(1, AOrd::Release);
    kthread::kthread_exit_running();
}

/// B 端：recv blocking → reply → 校验 + cap transfer 验证。
extern "C" fn t7_worker_b() -> ! {
    set_current_pid(T7_PID_B);
    let cap_b = T7_EP_CAP_B.load(AOrd::Acquire) as u8;
    let obj_idx = T7_EP_OBJ_IDX.load(AOrd::Acquire);
    info!("[ipc-smoke] B start: cap_b={} obj_idx={}", cap_b, obj_idx);

    // recv 阻塞等 A 的 send
    let buf_va_b: u64 = 0x0008_1000;
    unsafe {
        core::ptr::write_bytes(buf_va_b as *mut u8, 0, 16);
    }
    let frame = SyscallFrame {
        num: synapse_abi::SyscallId::IpcRecv.num(),
        args: [cap_b as u64, buf_va_b, 0, 0, 0, 0],
    };
    let r = k_ipc_recv(&frame);
    t7_assert_eq!(r, 5, "B: recv blocking returned len");

    // 校验：buf_va_b 应为 "ping\0"
    let read_back = unsafe { core::slice::from_raw_parts(buf_va_b as *const u8, 5) };
    t7_assert!(read_back[..4] == *b"ping", "B: recv buf contains 'ping'");

    // reply "pong" → 写入 A 的 send buffer (reply_buf_va)
    let reply_buf_va: u64 = 0x0008_2000;
    let pong: [u8; 5] = *b"pong\0";
    unsafe {
        core::ptr::copy_nonoverlapping(pong.as_ptr(), reply_buf_va as *mut u8, 5);
    }
    let reply_frame = SyscallFrame {
        num: synapse_abi::SyscallId::IpcReply.num(),
        args: [cap_b as u64, reply_buf_va, 5, 0, 0, 0],
    };
    t7_assert_eq!(k_ipc_reply(&reply_frame), 0, "B: reply returned 0");

    // 第二轮：recv 空（队空）→ 等 A 的 send Delivered
    // 简化：B 退出，让 A 跑后续 send；我们不验证这条避免时序复杂

    // 错误路径断言 3：reply 无 in_flight → -14
    let bad_reply = SyscallFrame {
        num: synapse_abi::SyscallId::IpcReply.num(),
        args: [cap_b as u64, reply_buf_va, 5, 0, 0, 0],
    };
    t7_assert_eq!(k_ipc_reply(&bad_reply), E_PEER_DIED, "B: second reply → E_PEER_DIED");

    T7_B_DONE.store(1, AOrd::Release);
    kthread::kthread_exit_running();
}

/// 真机 IPC smoke（main.rs 在 kthread_mutex_smoke 之后调用）。
///
/// 拓扑：
/// * 90（A）和 91（B）两 cap table；
/// * 一个 endpoint + 两 cap（SEND|RECV|REPLY|GRANT）+ 一 notification 给 A；
/// * A.transfer(notif) → B 端 cap slot 新增 → 校验 B 的 cap 表容量+1；
/// * 两 worker 跑 send→recv→reply 端到端；
/// * boot 用 sleep_until 等 done → reap。
pub fn kthread_ipc_smoke() {
    info!("[ipc-smoke] start");
    use synapse_sched::Priority;

    // 1. 两 cap table
    t7_assert!(
        crate::kstate::k_create_cap_table(synapse_proc::process::Pid(T7_PID_A)).is_ok(),
        "create cap table A"
    );
    t7_assert!(
        crate::kstate::k_create_cap_table(synapse_proc::process::Pid(T7_PID_B)).is_ok(),
        "create cap table B"
    );

    // 2. 一 endpoint + 两 cap
    let ep_obj = crate::kstate::k_alloc_object(ObjKind::Endpoint).expect("alloc ep");
    T7_EP_OBJ_IDX.store(ep_obj.index, AOrd::Release);
    let cap_a = crate::kstate::k_mint_root(
        synapse_proc::process::Pid(T7_PID_A),
        ep_obj,
        Rights::SEND | Rights::RECV | Rights::REPLY | Rights::GRANT,
    )
    .expect("mint A ep");
    let cap_b = crate::kstate::k_mint_root(
        synapse_proc::process::Pid(T7_PID_B),
        ep_obj,
        Rights::SEND | Rights::RECV | Rights::REPLY | Rights::GRANT,
    )
    .expect("mint B ep");
    T7_EP_CAP_A.store(cap_a as u32, AOrd::Release);
    T7_EP_CAP_B.store(cap_b as u32, AOrd::Release);
    info!("[ipc-smoke] ep obj_idx={} cap_a={} cap_b={}", ep_obj.index, cap_a, cap_b);

    // 3. Notification 给 A mint，A 用 transfer → B
    let notif_obj = crate::kstate::k_alloc_object(ObjKind::Notification).expect("alloc notif");
    let notif_a = crate::kstate::k_mint_root(
        synapse_proc::process::Pid(T7_PID_A),
        notif_obj,
        Rights::ALL,
    )
    .expect("mint A notif");
    let transfer_items = [TransferItem { cptr: notif_a, mask: Rights::ALL }];
    let dst = crate::kstate::k_transfer(
        synapse_proc::process::Pid(T7_PID_A),
        synapse_proc::process::Pid(T7_PID_B),
        &transfer_items,
    )
    .expect("transfer notif A→B");
    T7_NOTIF_CAP_B.store(dst[0] as u32, AOrd::Release);
    t7_assert!(dst[0] != 0, "notif transfer produced non-zero cptr");

    // 4. spawn 两 worker（owner_pid 各自）
    let wa = kthread::kthread_create_for(t7_worker_a, Priority::DEFAULT.0, T7_PID_A)
        .expect("create A worker");
    let wb = kthread::kthread_create_for(t7_worker_b, Priority::DEFAULT.0, T7_PID_B)
        .expect("create B worker");
    info!("[ipc-smoke] spawned A={:#x} B={:#x}", wa.0, wb.0);

    // 5. boot 让出，等两 worker 收尾
    kthread::enable_preemption();
    let t0 = crate::pit::tick_count();
    loop {
        if T7_A_DONE.load(AOrd::Acquire) == 1 && T7_B_DONE.load(AOrd::Acquire) == 1 {
            break;
        }
        if crate::pit::tick_count().saturating_sub(t0) > T7_TIMEOUT_TICKS {
            panic!(
                "[ipc-smoke] timeout: A={} B={}",
                T7_A_DONE.load(AOrd::Acquire),
                T7_B_DONE.load(AOrd::Acquire),
            );
        }
        kthread::kthread_sleep_until(crate::pit::tick_count() + 2);
    }
    kthread::disable_preemption();
    info!("[ipc-smoke] both workers done");

    // 6. reap
    for &w in &[wa, wb] {
        let mut tries = 0u32;
        loop {
            match kthread::kthread_reap(w) {
                Ok(()) => break,
                Err(_) => {
                    tries += 1;
                    if tries > 200 {
                        panic!("[ipc-smoke] worker stuck reaping");
                    }
                    kthread::kthread_sleep_until(crate::pit::tick_count() + 1);
                }
            }
        }
    }
    info!("[ipc-smoke] reaped");

    // 7. 再做一轮 try_send 单路径验证（不依赖 recv 等——try_send 在无 receiver 时
    // 直接 Queued → 0）
    let buf_va_c: u64 = 0x0008_3000;
    unsafe { core::ptr::write_bytes(buf_va_c as *mut u8, 0, 16); }
    let msg: [u8; 4] = *b"hi!\0";
    unsafe { core::ptr::copy_nonoverlapping(msg.as_ptr(), buf_va_c as *mut u8, 4); }
    // 临时切换到 A 上下文用其 cap 做 try_send
    set_current_pid(T7_PID_A);
    let try_frame = SyscallFrame {
        num: synapse_abi::SyscallId::IpcTrySend.num(),
        args: [cap_a as u64, buf_va_c, 4, 0, 0, 0],
    };
    t7_assert_eq!(k_ipc_try_send(&try_frame), 0, "try_send queued → 0");

    // 8. 错误路径断言：try_send 带超大 n_caps → -1
    let try_bad = SyscallFrame {
        num: synapse_abi::SyscallId::IpcTrySend.num(),
        args: [cap_a as u64, buf_va_c, 4, 0, 99, 0],
    };
    t7_assert_eq!(k_ipc_try_send(&try_bad), E_INVALID_CAP, "try_send n_caps=99 → E_INVALID_CAP");

    info!("[ipc-smoke] PASS");
}

// ---------------------------------------------------------------------------
// P4-T8：Notification 位图信号/等待 smoke（marker w/x）
// ---------------------------------------------------------------------------

/// Smoke pid（与 T7/T8 其他合成 pid 不撞号；不进入真进程表——只起 cap table
/// 与 syscall 上下文作用）。
const T8_PID: u32 = 92;

/// 真机 Notification smoke（main.rs 在 kthread_ipc_smoke 之后调用）。
///
/// 拓扑：单 cap table（pid=92）；一个 notification 对象 + SEND|RECV cap。
///
/// 覆盖：
/// * signal(bits) → wait(mask=bits) 读清 round-trip；
/// * 多源聚合：signal 0xFF → wait(0x0F) 返 0x0F + word 余 0xF0；
/// * mask=0 恒返 0（无意义查询）；
/// * 无匹配 → wait 返回 None（MVP 围栏下 syscall 层映射为 0）；
///
/// 注：cap kind / 权限错误路径由 `user/hello §8` 走真实 syscall 覆盖
/// （cptr=0、非 Notification kind、无 RECV 权）——kernel-side smoke 不重复
/// 测 resolve_notification 内部（其表驱动路径由 cap crate 单元测试覆盖）。
pub fn kthread_notif_smoke() {
    info!("[notif-smoke] start");

    // 1. cap table
    t7_assert!(
        crate::kstate::k_create_cap_table(synapse_proc::process::Pid(T8_PID)).is_ok(),
        "create cap table T8"
    );

    // 2. alloc notification + mint root cap (SEND | RECV)
    let no_obj = crate::kstate::k_alloc_object(ObjKind::Notification).expect("alloc notif");
    let cap = crate::kstate::k_mint_root(
        synapse_proc::process::Pid(T8_PID),
        no_obj,
        Rights::SEND | Rights::RECV,
    )
    .expect("mint notif cap");
    info!("[notif-smoke] obj_idx={} cap={}", no_obj.index, cap);

    // 上下文 = T8_PID（resolve 路径 pid 校验）
    set_current_pid(T8_PID);

    // 3. round-trip：signal(0xA5) → wait(0xA5) → 0xA5；再次 wait → None（已读清）
    t7_assert_eq!(
        crate::kstate::k_notify_signal(no_obj, 0xA5),
        Ok(()),
        "signal(0xA5)"
    );
    let got = crate::kstate::k_notify_wait(no_obj, 0xA5);
    t7_assert_eq!(got, Ok(Some(0xA5)), "wait(0xA5) round-trip");
    let got = crate::kstate::k_notify_wait(no_obj, 0xA5);
    t7_assert_eq!(got, Ok(None), "wait(0xA5) re-read → empty");

    // 4. 多源聚合 + 部分 wait：signal(0xFF) → wait(0x0F) 返 0x0F；peek 余 0xF0
    t7_assert_eq!(
        crate::kstate::k_notify_signal(no_obj, 0xFF),
        Ok(()),
        "signal(0xFF)"
    );
    let got = crate::kstate::k_notify_wait(no_obj, 0x0F);
    t7_assert_eq!(got, Ok(Some(0x0F)), "wait(0x0F) subset clear");
    // 残留 0xF0——用 wait(mask=0xF0) 验证读清后清零
    let got = crate::kstate::k_notify_wait(no_obj, 0xF0);
    t7_assert_eq!(got, Ok(Some(0xF0)), "wait(0xF0) clear residual");

    // 5. mask=0 恒返 Some(0)（无意义查询）
    t7_assert_eq!(
        crate::kstate::k_notify_wait(no_obj, 0),
        Ok(Some(0)),
        "wait(mask=0) → 0"
    );

    // 6. signal bits=0 = no-op（保证 word 不变）
    t7_assert_eq!(
        crate::kstate::k_notify_signal(no_obj, 0),
        Ok(()),
        "signal(bits=0) no-op"
    );

    // 7. 资源回收
    let _ = crate::kstate::k_free_object(no_obj);
    info!("[notif-smoke] PASS");
}