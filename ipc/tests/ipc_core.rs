//! synapse-ipc 宿主单元测试（NFR4/NFR5）。
//!
//! 覆盖：消息头 ABI 校验 / 传输路径分类 / Notification 位图 /
//! Endpoint 队列状态机（FIFO、try_send 队满、对端死亡取消）。

use synapse_cap::{CapError, Rights, TransferItem, DEFAULT_QUOTA, MAX_TRANSFER};
use synapse_ipc::*;

fn req(sender: u32, label: u32) -> SendRequest {
    SendRequest {
        sender: AgentId(sender),
        badge: 0,
        label,
        payload_len: 0,
        payload_addr: 0x1000_0000,
        caps: [TransferItem { cptr: 0, mask: Rights::EMPTY }; MAX_TRANSFER],
        cap_count: 0,
    }
}

// ---------- header ----------

#[test]
fn header_layout_is_fixed_abi() {
    // repr(C) 1+1+1+1+2+4+4+4 = 20 字节，无隐式 padding
    assert_eq!(core::mem::size_of::<IpcHeader>(), 20);
}

#[test]
fn header_validate_ok_and_stamp() {
    let mut h = IpcHeader::new(7, 128, 2);
    assert_eq!(h.sender_agent, AgentId::UNSTAMPED);
    h.validate(&DEFAULT_QUOTA).unwrap();
    h.stamp(AgentId(42)); // 内核盖章唯一写入点
    assert_eq!(h.sender_agent, AgentId(42));
}

#[test]
fn header_version_mismatch_is_abi_mismatch() {
    let mut h = IpcHeader::new(0, 0, 0);
    h.version = IPC_ABI_VERSION + 1;
    assert_eq!(h.validate(&DEFAULT_QUOTA), Err(CapError::AbiMismatch));
    h.version = IPC_ABI_VERSION;
    h.header_len = 19; // 小于固定头长
    assert_eq!(h.validate(&DEFAULT_QUOTA), Err(CapError::AbiMismatch));
    h.header_len = 257; // 超过 MAX_HEADER_LEN
    assert_eq!(h.validate(&DEFAULT_QUOTA), Err(CapError::AbiMismatch));
    h.header_len = 24; // 合法：未来追加字段，接收方按 header_len 跳过
    h.validate(&DEFAULT_QUOTA).unwrap();
}

#[test]
fn header_flags_reserved_must_be_zero() {
    let mut h = IpcHeader::new(0, 0, 0);
    h.flags = 1;
    assert_eq!(h.validate(&DEFAULT_QUOTA), Err(CapError::AbiMismatch));
}

#[test]
fn header_cap_count_and_payload_limits() {
    let h = IpcHeader::new(0, 0, (MAX_TRANSFER + 1) as u8);
    assert_eq!(h.validate(&DEFAULT_QUOTA), Err(CapError::InvalidCap));

    let mut h = IpcHeader::new(0, MAX_PAYLOAD, 0);
    h.validate(&DEFAULT_QUOTA).unwrap(); // 恰好 4KB 合法
    h.payload_len = MAX_PAYLOAD + 1;
    assert_eq!(h.validate(&DEFAULT_QUOTA), Err(CapError::QuotaExceeded));

    // 进程配额收紧时以配额为准（min(quota.max_msg_size, 4KB)）
    let tight = synapse_cap::Quota { max_msg_size: 512, ..DEFAULT_QUOTA };
    let mut h = IpcHeader::new(0, 513, 0);
    assert_eq!(h.validate(&tight), Err(CapError::QuotaExceeded));
    h.payload_len = 512;
    h.validate(&tight).unwrap();
}

// ---------- path ----------

#[test]
fn path_classification_boundaries() {
    assert_eq!(classify(0), TransferPath::RegisterDirect);
    assert_eq!(classify(32), TransferPath::RegisterDirect);
    assert_eq!(classify(33), TransferPath::SingleCopy);
    assert_eq!(classify(4096), TransferPath::SingleCopy);
    assert_eq!(classify(4097), TransferPath::SharedGrant);
}

// ---------- notification ----------

#[test]
fn notification_bitmap_or_and_read_clear() {
    let mut n = Notification::new();
    assert_eq!(n.poll(), None); // 空 → None（内核将阻塞或 WouldBlock）
    n.signal(0b0001);
    n.signal(0b0100); // 多 IRQ 源聚合（OR）
    assert_eq!(n.peek(), 0b0101);
    assert_eq!(n.poll(), Some(0b0101)); // 读清
    assert_eq!(n.poll(), None);
    n.signal(0); // no-op
    assert_eq!(n.peek(), 0);
}

// ---------- endpoint ----------

#[test]
fn endpoint_fifo_order() {
    let mut ep = Endpoint::new();
    ep.try_send(req(1, 100)).unwrap();
    ep.try_send(req(2, 200)).unwrap();
    ep.try_send(req(3, 300)).unwrap();
    assert_eq!(ep.queued(), 3);

    for expected in [100, 200, 300] {
        match ep.recv() {
            RecvOutcome::Message(m) => assert_eq!(m.label, expected),
            RecvOutcome::Waiting => panic!("queue non-empty"),
        }
    }
    assert_eq!(ep.queued(), 0);
}

#[test]
fn endpoint_recv_empty_marks_waiting_then_direct_deliver() {
    let mut ep = Endpoint::new();
    assert!(matches!(ep.recv(), RecvOutcome::Waiting));
    assert!(ep.has_waiting_receiver());
    // 接收方等待中 → 下一个 send 直接 Delivered 并清除等待标记
    assert_eq!(ep.try_send(req(1, 0)), Ok(SendOutcome::Delivered));
    assert!(!ep.has_waiting_receiver());
    assert_eq!(ep.queued(), 0); // 未入队（直拷路径）
}

#[test]
fn endpoint_queue_full_would_block() {
    let mut ep = Endpoint::new();
    for i in 0..MAX_SEND_QUEUE {
        assert_eq!(ep.try_send(req(1, i as u32)), Ok(SendOutcome::Queued));
    }
    assert_eq!(ep.try_send(req(1, 999)), Err(CapError::WouldBlock));
    // 出队一个后恢复可入队
    assert!(ep.try_recv().is_some());
    assert_eq!(ep.try_send(req(1, 999)), Ok(SendOutcome::Queued));
}

#[test]
fn endpoint_ring_wraparound_keeps_fifo() {
    let mut ep = Endpoint::new();
    // 反复入队出队，跨越环形边界
    for round in 0..3 {
        for i in 0..MAX_SEND_QUEUE {
            ep.try_send(req(1, (round * MAX_SEND_QUEUE + i) as u32)).unwrap();
        }
        for i in 0..MAX_SEND_QUEUE {
            let m = ep.try_recv().unwrap();
            assert_eq!(m.label, (round * MAX_SEND_QUEUE + i) as u32);
        }
    }
    assert_eq!(ep.queued(), 0);
}

#[test]
fn endpoint_cancel_sender_peer_died() {
    let mut ep = Endpoint::new();
    ep.try_send(req(1, 10)).unwrap();
    ep.try_send(req(2, 20)).unwrap();
    ep.try_send(req(1, 30)).unwrap();
    ep.try_send(req(3, 40)).unwrap();

    // agent 1 退出 → 摘除其两条请求（内核逐个唤醒返回 E_PEER_DIED）
    assert_eq!(ep.cancel_sender(AgentId(1)), 2);
    assert_eq!(ep.queued(), 2);
    // 其余保持 FIFO
    assert_eq!(ep.try_recv().unwrap().label, 20);
    assert_eq!(ep.try_recv().unwrap().label, 40);

    // 无匹配的 cancel 为 no-op
    assert_eq!(ep.cancel_sender(AgentId(9)), 0);
}

#[test]
fn endpoint_cancel_sender_does_not_disturb_waiting_receiver() {
    // Doc 03 §5.1：接收方阻塞在 recv，所有潜在发送方退出 → 继续阻塞，不返回错误
    let mut ep = Endpoint::new();
    ep.try_send(req(1, 0)).unwrap();
    assert!(matches!(ep.recv(), RecvOutcome::Message(_)));
    assert!(matches!(ep.recv(), RecvOutcome::Waiting));
    assert_eq!(ep.cancel_sender(AgentId(1)), 0);
    assert!(ep.has_waiting_receiver()); // 等待标记不受影响
}

#[test]
fn endpoint_rejects_oversize_cap_batch() {
    let mut ep = Endpoint::new();
    let mut r = req(1, 0);
    r.cap_count = MAX_TRANSFER + 1;
    assert_eq!(ep.try_send(r), Err(CapError::InvalidCap));
}
