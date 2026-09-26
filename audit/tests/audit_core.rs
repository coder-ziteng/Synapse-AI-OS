//! synapse-audit 宿主单元测试。
//!
//! 覆盖：事件构造（全 5 类事件点）/ 环形队列 FIFO / 队满覆盖最旧 +
//! overflow 计数 / 批量 drain / clear 语义。

use synapse_audit::*;
use synapse_cap::{ObjKind, ObjRef, Rights};
use synapse_ipc::AgentId;

fn ep_ref() -> ObjRef {
    ObjRef { index: 3, generation: 2 }
}

// ---------- 事件构造 ----------

#[test]
fn event_constructors_cover_all_kinds() {
    let a = AgentId(10);
    let b = AgentId(20);

    let e = AuditEvent::cap_verify(a, ep_ref(), Rights::SEND, true, 100);
    assert_eq!(e.kind, EventKind::CapVerify);
    assert_eq!(e.actor, a);
    assert_eq!(e.timestamp, 100);
    assert_eq!(
        e.detail,
        EventDetail::CapVerify { target: ep_ref(), rights: Rights::SEND, ok: true }
    );

    let e = AuditEvent::cap_lifecycle(a, CapOp::Delegate, ObjKind::Endpoint, a, b, 101);
    assert_eq!(e.kind, EventKind::CapLifecycle);
    assert_eq!(
        e.detail,
        EventDetail::CapLifecycle {
            op: CapOp::Delegate,
            obj: ObjKind::Endpoint,
            source: a,
            dest: b
        }
    );

    let e = AuditEvent::ipc(a, IpcDir::Send, ep_ref(), 7, 102);
    assert_eq!(e.kind, EventKind::Ipc);
    assert_eq!(e.detail, EventDetail::Ipc { dir: IpcDir::Send, endpoint: ep_ref(), label: 7 });

    let e = AuditEvent::process(a, ProcOp::Spawn, b, 1, 0, 103);
    assert_eq!(e.kind, EventKind::Process);
    assert_eq!(
        e.detail,
        EventDetail::Process { op: ProcOp::Spawn, agent: b, parent_pid: 1, code: 0 }
    );

    let e = AuditEvent::system(SystemEvent::Startup, 42, 104);
    assert_eq!(e.kind, EventKind::System);
    assert_eq!(e.actor, AgentId::UNSTAMPED); // 系统事件无触发者
    assert_eq!(e.detail, EventDetail::System { event: SystemEvent::Startup, param: 42 });
}

// ---------- 队列 ----------

#[test]
fn queue_fifo_order() {
    let mut q: AuditQueue<8> = AuditQueue::new();
    assert!(q.is_empty());
    assert_eq!(q.capacity(), 8);
    for i in 0..5 {
        assert_eq!(q.push(AuditEvent::system(SystemEvent::Startup, i, i as u64)), None);
    }
    assert_eq!(q.len(), 5);
    assert!(!q.is_full());
    for i in 0..5 {
        let ev = q.pop().unwrap();
        assert_eq!(ev.timestamp, i as u64); // 最旧优先
    }
    assert_eq!(q.pop(), None);
}

#[test]
fn queue_full_evicts_oldest_and_counts_overflow() {
    let mut q: AuditQueue<4> = AuditQueue::new();
    for i in 0..4 {
        q.push(AuditEvent::system(SystemEvent::Startup, i, i as u64));
    }
    assert!(q.is_full());
    assert_eq!(q.overflow(), 0);

    // 第 5 条 → 覆盖 ts=0，返回被覆盖者
    let evicted = q.push(AuditEvent::system(SystemEvent::Startup, 99, 4)).unwrap();
    assert_eq!(evicted.timestamp, 0);
    assert_eq!(q.overflow(), 1);
    assert_eq!(q.len(), 4); // 存量不超容量

    // 队列内容 = ts 1,2,3,4（最旧的 0 已丢）
    let mut out = [evicted; 4];
    assert_eq!(q.drain(&mut out), 4);
    let ts: [u64; 4] = [out[0].timestamp, out[1].timestamp, out[2].timestamp, out[3].timestamp];
    assert_eq!(ts, [1, 2, 3, 4]);

    // drain 后队列为空：非满入队不产生覆盖
    q.push(AuditEvent::system(SystemEvent::Startup, 0, 10));
    assert_eq!(q.overflow(), 1);
    // 重新填满再压一条 → overflow 累计到 2
    for ts in 11..14 {
        q.push(AuditEvent::system(SystemEvent::Startup, 0, ts));
    }
    assert!(q.is_full());
    q.push(AuditEvent::system(SystemEvent::Startup, 0, 14));
    assert_eq!(q.overflow(), 2);
}

#[test]
fn queue_drain_partial_batch() {
    let mut q: AuditQueue<16> = AuditQueue::new();
    for i in 0..10 {
        q.push(AuditEvent::system(SystemEvent::Startup, i, i as u64));
    }
    let dummy = AuditEvent::system(SystemEvent::Startup, u32::MAX, u64::MAX);
    let mut batch = [dummy; 4];
    // 批量 4：审计服务一次提交
    assert_eq!(q.drain(&mut batch), 4);
    assert_eq!(batch[0].timestamp, 0);
    assert_eq!(batch[3].timestamp, 3);
    assert_eq!(q.len(), 6);
    // out 大于存量：只返回存量
    let mut big = [dummy; 32];
    assert_eq!(q.drain(&mut big), 6);
    assert!(q.is_empty());
    assert_eq!(q.drain(&mut big), 0);
}

#[test]
fn queue_clear_keeps_overflow_history() {
    let mut q: AuditQueue<2> = AuditQueue::new();
    q.push(AuditEvent::system(SystemEvent::Startup, 0, 0));
    q.push(AuditEvent::system(SystemEvent::Startup, 0, 1));
    q.push(AuditEvent::system(SystemEvent::Startup, 0, 2)); // 覆盖 1 次
    assert_eq!(q.overflow(), 1);
    q.clear();
    assert!(q.is_empty());
    assert_eq!(q.overflow(), 1); // 丢失历史不抹除
    // clear 后可继续正常使用
    q.push(AuditEvent::system(SystemEvent::Startup, 0, 9));
    assert_eq!(q.pop().unwrap().timestamp, 9);
}

#[test]
fn default_queue_capacity_alias() {
    let q = DefaultAuditQueue::new();
    assert_eq!(q.capacity(), DEFAULT_QUEUE_CAPACITY);
    assert_eq!(DEFAULT_QUEUE_CAPACITY, 256);
}
