//! 同步 Endpoint 核心状态机（对齐 [Doc 03 §2](../../../docs/design/03-ipc-message-and-single-copy-path.md)
//! 端点语义 + [§5.1](../../../docs/design/03-ipc-message-and-single-copy-path.md) 错误语义与对端死亡规则）。
//!
//! 本模块建模端点的**队列状态转移**（纯数据，无调度）：
//!
//! - `send`：有等待接收者 → `Delivered`（内核直拷 + 唤醒）；否则入队 `Queued`
//!   （内核将发送方线程置 Blocked 等待 reply）；队满 → `WouldBlock`（try_send）。
//! - `recv`：队非空 → 出队 `Message`；队空 → `Waiting`（内核将接收方线程置 Blocked）。
//! - 对端死亡（Doc 03 §5.1）：`cancel_sender` 摘除该 agent 全部排队请求，
//!   内核逐个唤醒并返回 `E_PEER_DIED`；**接收方等待中不受影响，继续阻塞**。
//!
//! 单接收者模型（Doc 03 §2.3 首期建议）：`receiver_waiting` 为布尔而非队列。
//! 线程阻塞/唤醒、reply 唤醒链由内核集成层负责（本 crate 无调度器依赖）。

use crate::header::AgentId;
use synapse_cap::{CapError, TransferItem, MAX_TRANSFER};

/// 发送队列深度（固定容量；队满时 try_send → `WouldBlock`，
/// 阻塞 send 由内核挂起线程，不占队列外内存）。
pub const MAX_SEND_QUEUE: usize = 32;

/// 一次待处理的发送请求（队列条目）。
///
/// payload 本体**不在队列中**——`Delivered`/出队时由内核走
/// [crate::path] 分类的路径直接拷贝到接收方地址空间（单拷贝原则）；
/// 队列只承载元数据 + cap 转移项。
#[derive(Clone, Copy, Debug)]
pub struct SendRequest {
    /// 发送方 agent（内核盖章值）。
    pub sender: AgentId,
    /// 发送方 endpoint capability 的 badge（seL4 badge 语义，区分调用者）。
    pub badge: u32,
    /// 用户标签。
    pub label: u32,
    /// 消息体字节数。
    pub payload_len: u32,
    /// 发送方用户态 payload 地址（内核单拷贝源，拷贝前校验 → `E_INVALID_ADDR`）。
    pub payload_addr: u64,
    /// 随消息转移的 capability（前 `cap_count` 项有效）。
    pub caps: [TransferItem; MAX_TRANSFER],
    /// 有效 cap 转移项数（≤ MAX_TRANSFER）。
    pub cap_count: usize,
}

/// send 的状态转移结果（内核集成层据此决定唤醒 / 挂起 / 返回错误）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    /// 接收方已在等待：内核直拷 payload + 安装 caps + 唤醒接收方；
    /// 发送方随后阻塞等待 reply（同步 RPC）。
    Delivered,
    /// 无等待接收者：请求已入队，内核将发送方线程置 Blocked。
    Queued,
}

/// recv 的状态转移结果。
#[derive(Clone, Copy, Debug)]
pub enum RecvOutcome {
    /// 取到队首请求（内核执行单拷贝 + cap 安装 + 唤醒发送方等 reply）。
    Message(SendRequest),
    /// 队空：已标记接收方等待，内核将接收方线程置 Blocked；
    /// 下一个 `send` 将直接 `Delivered`。
    Waiting,
}

/// 同步端点（内核对象 payload；由 `ObjectTable` 槽位承载生命周期）。
pub struct Endpoint {
    /// 环形队列。
    queue: [Option<SendRequest>; MAX_SEND_QUEUE],
    head: usize,
    count: usize,
    /// 单接收者等待标记（Doc 03 §2.3：首期单接收者）。
    receiver_waiting: bool,
}

impl Default for Endpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl Endpoint {
    /// 创建空端点。
    pub fn new() -> Endpoint {
        Endpoint {
            queue: [const { None }; MAX_SEND_QUEUE],
            head: 0,
            count: 0,
            receiver_waiting: false,
        }
    }

    /// 当前排队请求数。
    pub const fn queued(&self) -> usize {
        self.count
    }

    /// 是否有接收方在等待。
    pub const fn has_waiting_receiver(&self) -> bool {
        self.receiver_waiting
    }

    /// 撤销 `recv()` 置起的接收方等待标记（状态回滚）。
    ///
    /// 集成层调用 `recv()` 得到 `Waiting` 后**决定不阻塞**时（如 boot 围栏返回
    /// E_WOULD_BLOCK）必须回滚：否则残留的 `receiver_waiting=true` 会让后续
    /// `try_send` 误走 `Delivered` 路径，而集成层并未登记 waiter →
    /// E_NOT_FOUND（P4 elf-smoke hello 6.6 首次真机暴露的状态泄漏）。
    pub fn cancel_recv(&mut self) {
        self.receiver_waiting = false;
    }

    /// 非阻塞发送（`try_send`，Doc 03 §9：首期唯一非阻塞变体）。
    ///
    /// - 接收方等待中 → [`SendOutcome::Delivered`]（清除等待标记）；
    /// - 队未满 → [`SendOutcome::Queued`]；
    /// - 队满 → [`CapError::WouldBlock`]（-4，不阻塞不 panic）。
    pub fn try_send(&mut self, req: SendRequest) -> Result<SendOutcome, CapError> {
        if req.cap_count > MAX_TRANSFER {
            return Err(CapError::InvalidCap);
        }
        if self.receiver_waiting {
            self.receiver_waiting = false;
            return Ok(SendOutcome::Delivered);
        }
        if self.count == MAX_SEND_QUEUE {
            return Err(CapError::WouldBlock);
        }
        let tail = (self.head + self.count) % MAX_SEND_QUEUE;
        self.queue[tail] = Some(req);
        self.count += 1;
        Ok(SendOutcome::Queued)
    }

    /// 接收（阻塞语义的纯状态部分）。
    ///
    /// - 队非空 → [`RecvOutcome::Message`]（FIFO 出队）；
    /// - 队空 → [`RecvOutcome::Waiting`]，置接收方等待标记；
    ///   此状态下所有潜在发送方退出也**继续等待**（Doc 03 §5.1：
    ///   recv 等待期间对端死亡不返回错误）。
    pub fn recv(&mut self) -> RecvOutcome {
        if let Some(req) = self.dequeue() {
            RecvOutcome::Message(req)
        } else {
            self.receiver_waiting = true;
            RecvOutcome::Waiting
        }
    }

    /// 非阻塞接收（内核集成层将 `Waiting` 映射为 `E_WOULD_BLOCK`）。
    pub fn try_recv(&mut self) -> Option<SendRequest> {
        self.dequeue()
    }

    /// 对端死亡回收（Doc 03 §5.1 / Doc 01 §4.2 进程退出清理）：
    /// 摘除该 agent 的全部排队请求并返回（内核逐个唤醒 → `E_PEER_DIED`）。
    ///
    /// 接收方等待标记不受影响（继续阻塞等待新发送方）。
    /// 返回被摘除的请求数。
    pub fn cancel_sender(&mut self, sender: AgentId) -> usize {
        let mut cancelled = 0usize;
        for slot in self.queue.iter_mut() {
            if let Some(req) = slot {
                if req.sender == sender {
                    *slot = None;
                    cancelled += 1;
                }
            }
        }
        if cancelled > 0 {
            self.compact();
        }
        cancelled
    }

    /// FIFO 出队。
    fn dequeue(&mut self) -> Option<SendRequest> {
        let req = self.queue[self.head].take()?;
        self.head = (self.head + 1) % MAX_SEND_QUEUE;
        self.count -= 1;
        Some(req)
    }

    /// cancel 打洞后重整环形队列（管理路径，O(n)；热路径不经过）。
    fn compact(&mut self) {
        let mut rebuilt: [Option<SendRequest>; MAX_SEND_QUEUE] =
            [const { None }; MAX_SEND_QUEUE];
        let mut n = 0usize;
        for i in 0..MAX_SEND_QUEUE {
            let idx = (self.head + i) % MAX_SEND_QUEUE;
            if let Some(req) = self.queue[idx].take() {
                rebuilt[n] = Some(req);
                n += 1;
            }
        }
        self.queue = rebuilt;
        self.head = 0;
        self.count = n;
    }
}
