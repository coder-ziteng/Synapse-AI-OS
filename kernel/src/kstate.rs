//! 全局内核状态：cap/ipc/proc 纯逻辑表的持有者（集成层核心）。
//!
//! ## 存储布局（全静态，无 alloc——P2-T3 内核堆落地前）
//!
//! | 静态 | 内容 | 出处 |
//! |------|------|------|
//! | `OBJECTS` | 系统级 [`ObjectTable`]（1024 槽 + generation） | Doc 01 §4.2 |
//! | `PROCS` | [`ProcessTable`]（含 AgentRegistry；`new` 自带 init 进程） | Doc 02 §5.2/§5.3 |
//! | `CAP_TABLES` | 每进程 [`CapTable`]，槽位 = `pid % MAX_PROCS`（Pcb 不含表，集成层持有） | Doc 01 §4 |
//! | `ENDPOINTS` / `NOTIFICATIONS` | IPC 实体，按 `ObjRef.index` 直接索引（容量 64，见下） | Doc 03 §2/§6.2 |
//!
//! **MVP 约束**：Endpoint/Notification 实体槽按对象 index 直接索引、容量 64。
//! 对象分配单调递增，早期对象 index 必 < 64；超限时 `k_alloc_object` 返回
//! `NoMemory`。P2 页帧分配器落地后可改为按需分配。
//!
//! ## 锁与临界区
//!
//! 每个表一把 [`SpinLock`]（IRQ-safe）。**锁序（防死锁，只准从小到大持有）**：
//! `OBJECTS → PROCS → CAP_TABLES → ENDPOINTS → NOTIFICATIONS`。
//! `transfer_caps` 要求 src/dst/objects 同一临界区（cap crate 文档约定）——
//! 由 [`k_transfer`] / [`k_install_initial`] 统一按锁序获取。
//!
//! ## 大对象初始化注意
//!
//! `KernelState` 总量 ~1MB，**严禁**在栈上构造整体再移动（内核栈仅 ~300KB，
//! 位于 0x16000..0x60008，溢出即踩页表）。因此逐表独立 `static`，
//! CapTable/Endpoint 等 6KB 级实体在槽位内按需构造（临时对象 ≤ 7KB）。

use crate::sync::SpinLock;

use synapse_cap::{
    transfer_caps, CapError, CapRef, CapTable, ObjectTable, ObjKind, ObjRef, Rights, TransferItem,
    MAX_TRANSFER,
};
use synapse_ipc::{Endpoint, Notification, RecvOutcome, SendOutcome, SendRequest};
use synapse_ipc::AgentId;
use synapse_proc::grant::{install_initial_caps, GrantItem, MAX_INITIAL_CAPS};
use synapse_proc::process::{ProcessTable, Pid, INIT_PID, MAX_PROCS};

/// Endpoint 实体槽容量（按 ObjRef.index 索引，MVP 约束见模块头）。
pub const MAX_ENDPOINTS: usize = 64;
/// Notification 实体槽容量（同上）。
pub const MAX_NOTIFICATIONS: usize = 64;

static OBJECTS: SpinLock<Option<ObjectTable>> = SpinLock::new(None);
static PROCS: SpinLock<Option<ProcessTable>> = SpinLock::new(None);
static CAP_TABLES: SpinLock<[Option<CapTable>; MAX_PROCS]> =
    SpinLock::new([const { None }; MAX_PROCS]);
static ENDPOINTS: SpinLock<[Option<Endpoint>; MAX_ENDPOINTS]> =
    SpinLock::new([const { None }; MAX_ENDPOINTS]);
static NOTIFICATIONS: SpinLock<[Option<Notification>; MAX_NOTIFICATIONS]> =
    SpinLock::new([const { None }; MAX_NOTIFICATIONS]);

const UNINIT: &str = "kstate accessed before kstate_init()";

/// 初始化全部内核表（boot 时调用一次；重复调用 panic）。
///
/// `ProcessTable::new(AgentId(1))` 已安装 init 进程（pid=1, Running,
/// INIT_QUOTA），init 的 agent_id 约定为 1（Doc 02 §5.2）。
pub fn kstate_init() {
    let mut objs = OBJECTS.lock();
    let mut procs = PROCS.lock();
    if objs.is_some() || procs.is_some() {
        panic!("kstate_init called twice");
    }
    // 各表在锁 guard 指向的静态位置就地构造（临时对象 ≤ ~20KB，栈安全）
    *objs = Some(ObjectTable::new());
    *procs = Some(ProcessTable::new(AgentId(1)));
    log::info!(
        "[kstate] initialized: objects(1024) procs({}) cap_tables({}) endpoints({}) notifications({})",
        MAX_PROCS, MAX_PROCS, MAX_ENDPOINTS, MAX_NOTIFICATIONS
    );
}

/// pid → CapTable 槽位。
pub fn cap_slot(pid: Pid) -> usize {
    (pid.0 % MAX_PROCS as u32) as usize
}

// ---------- 单表访问器（None → panic，boot 顺序保证不发生） ----------

/// 对 [`ObjectTable`] 的临界区访问。
pub fn with_objects<R>(f: impl FnOnce(&mut ObjectTable) -> R) -> R {
    let mut g = OBJECTS.lock();
    f(g.as_mut().expect(UNINIT))
}

/// 对 [`ProcessTable`] 的临界区访问。
pub fn with_procs<R>(f: impl FnOnce(&mut ProcessTable) -> R) -> R {
    let mut g = PROCS.lock();
    f(g.as_mut().expect(UNINIT))
}

/// 对某 pid 的 [`CapTable`] 的临界区访问（表不存在 → panic——调用方负责生命周期）。
pub fn with_cap_table<R>(pid: Pid, f: impl FnOnce(&mut CapTable) -> R) -> R {
    let mut g = CAP_TABLES.lock();
    let slot = &mut g[cap_slot(pid)];
    f(slot.as_mut().expect("cap table not created for pid"))
}

// ---------- 对象分配 + 实体存储接线 ----------

/// 分配内核对象并接线实体存储：
/// `Endpoint`/`Notification` 类型同时在实体表落位（按 `ObjRef.index` 索引）。
pub fn k_alloc_object(kind: ObjKind) -> Result<ObjRef, CapError> {
    // 锁序: OBJECTS → ENDPOINTS/NOTIFICATIONS
    let mut objs = OBJECTS.lock();
    let objs = objs.as_mut().expect(UNINIT);
    let r = objs.alloc(kind)?;
    let idx = r.index as usize;
    match kind {
        ObjKind::Endpoint => {
            if idx >= MAX_ENDPOINTS {
                objs.free(r)?;
                return Err(CapError::NoMemory);
            }
            let mut eps = ENDPOINTS.lock();
            eps[idx] = Some(Endpoint::new());
        }
        ObjKind::Notification => {
            if idx >= MAX_NOTIFICATIONS {
                objs.free(r)?;
                return Err(CapError::NoMemory);
            }
            let mut ns = NOTIFICATIONS.lock();
            ns[idx] = Some(Notification::new());
        }
        _ => {}
    }
    Ok(r)
}

/// 释放对象（free 前调用方应先走 begin_revoke/retire 状态机）；
/// 同步清理实体槽。`Retired`/已 `Revoking` 状态也可释放——本函数
/// 不校验 `Live`，由调用方按状态机推进保证。实体槽按 `index` 直接清零
/// （后续 alloc 同 index 会覆盖；不对 kind 做查表以容忍非 Live 状态）。
pub fn k_free_object(obj: ObjRef) -> Result<(), CapError> {
    let idx = obj.index as usize;
    if idx < MAX_ENDPOINTS {
        ENDPOINTS.lock()[idx] = None;
    }
    if idx < MAX_NOTIFICATIONS {
        NOTIFICATIONS.lock()[idx] = None;
    }
    with_objects(|o| o.free(obj))
}

// ---------- CapTable 生命周期（spawn / reap 挂钩） ----------

/// 为 pid 建 CapTable（slot 0 NULL trap 由 `CapTable::new` 内建）。
/// 槽已被占 → `NoMemory`（pid 槽复用冲突，理论上 reap 后不会发生）。
pub fn k_create_cap_table(pid: Pid) -> Result<(), CapError> {
    let mut g = CAP_TABLES.lock();
    let slot = &mut g[cap_slot(pid)];
    if slot.is_some() {
        return Err(CapError::NoMemory);
    }
    *slot = Some(CapTable::new());
    Ok(())
}

/// 销毁 pid 的 CapTable（资源释放顺序契约第 2 步：页帧 → **CapTable** →
/// agent_id → pid，后两步由 `ProcessTable::reap` 执行）。
pub fn k_destroy_cap_table(pid: Pid) {
    CAP_TABLES.lock()[cap_slot(pid)] = None;
}

/// 在 pid 表中铸造根 capability（`parent = None`，init 铸造路径，Doc 01 §4）。
pub fn k_mint_root(pid: Pid, obj: ObjRef, rights: Rights) -> Result<CapRef, CapError> {
    with_cap_table(pid, |t| {
        let cap = synapse_cap::Capability::root(obj, rights);
        t.alloc(cap)
    })
}

// ---------- 跨进程转移（transfer_caps 的锁包装） ----------

/// 原子转移 caps：src pid → dst pid（锁序 OBJECTS → CAP_TABLES，
/// 满足 `transfer_caps` 的"同一关中断临界区"约定）。
pub fn k_transfer(
    src: Pid,
    dst: Pid,
    items: &[TransferItem],
) -> Result<[CapRef; MAX_TRANSFER], CapError> {
    if src == dst {
        return Err(CapError::InvalidCap);
    }
    let objs_g = OBJECTS.lock();
    let objs = objs_g.as_ref().expect(UNINIT);
    let mut tables = CAP_TABLES.lock();
    let (si, di) = (cap_slot(src), cap_slot(dst));
    let (s_ref, d_ref) = two_slots(&mut tables, si, di)?;
    transfer_caps(s_ref, d_ref, items, None, objs)
}

/// spawn 初始 caps 安装（[`install_initial_caps`] 的锁包装，语义同 [`k_transfer`]）。
pub fn k_install_initial(
    parent: Pid,
    child: Pid,
    items: &[GrantItem],
) -> Result<[CapRef; MAX_INITIAL_CAPS], CapError> {
    let objs_g = OBJECTS.lock();
    let objs = objs_g.as_ref().expect(UNINIT);
    let mut tables = CAP_TABLES.lock();
    let (pi, ci) = (cap_slot(parent), cap_slot(child));
    let (p_ref, c_ref) = two_slots(&mut tables, pi, ci)?;
    install_initial_caps(p_ref, c_ref, items, objs)
}

/// 从同一数组取两个不同槽的可变引用（任一为空 → `NotFound`）。
fn two_slots<'a>(
    tables: &'a mut [Option<CapTable>; MAX_PROCS],
    i: usize,
    j: usize,
) -> Result<(&'a mut CapTable, &'a mut CapTable), CapError> {
    if i == j {
        return Err(CapError::InvalidCap);
    }
    let (lo, hi) = if i < j { (i, j) } else { (j, i) };
    let (left, right) = tables.split_at_mut(hi);
    let a = left[lo].as_mut().ok_or(CapError::NotFound)?;
    let b = right[0].as_mut().ok_or(CapError::NotFound)?;
    if i < j {
        Ok((a, b))
    } else {
        Ok((b, a))
    }
}

// ---------- IPC 实体操作（ENDPOINTS / NOTIFICATIONS 锁包装） ----------

/// 对某 Endpoint 对象非阻塞发送（对象须为已接线的 Endpoint，否则 `InvalidCap`）。
pub fn k_ep_try_send(obj: ObjRef, req: SendRequest) -> Result<SendOutcome, CapError> {
    let mut eps = ENDPOINTS.lock();
    let ep = eps
        .get_mut(obj.index as usize)
        .and_then(|s| s.as_mut())
        .ok_or(CapError::InvalidCap)?;
    ep.try_send(req)
}

/// 对某 Endpoint 对象接收（`Waiting` 时集成层应将线程置 Blocked——MVP 无调度器，
/// smoke 场景仅验证状态转移）。
pub fn k_ep_recv(obj: ObjRef) -> Result<RecvOutcome, CapError> {
    let mut eps = ENDPOINTS.lock();
    let ep = eps
        .get_mut(obj.index as usize)
        .and_then(|s| s.as_mut())
        .ok_or(CapError::InvalidCap)?;
    Ok(ep.recv())
}

/// 对端死亡回收：摘除 `sender` 全部排队请求，返回摘除数。
pub fn k_ep_cancel_sender(obj: ObjRef, sender: AgentId) -> Result<usize, CapError> {
    let mut eps = ENDPOINTS.lock();
    let ep = eps
        .get_mut(obj.index as usize)
        .and_then(|s| s.as_mut())
        .ok_or(CapError::InvalidCap)?;
    Ok(ep.cancel_sender(sender))
}

/// Notification 投递事件位（OR 聚合；中断路径将来直接调用）。
pub fn k_notify_signal(obj: ObjRef, bits: u64) -> Result<(), CapError> {
    let mut ns = NOTIFICATIONS.lock();
    let n = ns
        .get_mut(obj.index as usize)
        .and_then(|s| s.as_mut())
        .ok_or(CapError::InvalidCap)?;
    n.signal(bits);
    Ok(())
}

/// Notification 非阻塞取出（读清语义；`None` 时集成层应置 Blocked）。
pub fn k_notify_poll(obj: ObjRef) -> Result<Option<u64>, CapError> {
    let mut ns = NOTIFICATIONS.lock();
    let n = ns
        .get_mut(obj.index as usize)
        .and_then(|s| s.as_mut())
        .ok_or(CapError::InvalidCap)?;
    Ok(n.poll())
}

/// init 进程 pid 常量转发（smoke/bootstrap 使用）。
pub const INIT: Pid = INIT_PID;

/// init 进程 agent_id 常量转发（Doc 02 §5.2 约定为 1；`ProcessTable::new` 注册此值）。
pub const INIT_AGENT: AgentId = AgentId(1);
