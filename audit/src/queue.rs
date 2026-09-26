//! 审计事件环形队列（对齐 [Doc 03 §6.3](../../../docs/design/03-ipc-message-and-single-copy-path.md)
//! 性能考量：IPC 热路径不阻塞审计，事件入队，审计服务后台批量消费）。
//!
//! 策略：**队满覆盖最旧**（drop-oldest）——审计永不阻塞内核热路径
//! （NFR2 优先）；覆盖次数计入 [`AuditQueue::overflow`]，审计服务
//! 可将 overflow > 0 自身作为一条系统事件上报（丢失可见，不静默）。
//!
//! 锁语义与其余 crate 一致：本结构无锁，多核就绪由内核集成层负责
//! （首期单核关中断临界区；远期可换真无锁 SPSC，接口不变）。
//! 批量提交参数 N / T 为 TBD（Doc 03 §9，待性能基准），队列侧
//! 以 [`AuditQueue::drain`] 支持任意批量。

use crate::event::AuditEvent;

/// 默认队列容量（256 条 × sizeof(AuditEvent) ≈ 12 KB 内核常驻，
/// 足够吸收审计服务一次调度间隔内的事件峰值；实现层常量可调）。
pub const DEFAULT_QUEUE_CAPACITY: usize = 256;

/// 固定容量审计事件环形队列。
pub struct AuditQueue<const N: usize> {
    buf: [Option<AuditEvent>; N],
    /// 下一写入位置。
    head: usize,
    /// 当前存量。
    count: usize,
    /// 累计覆盖（丢弃）条数。
    overflow: u64,
}

impl<const N: usize> Default for AuditQueue<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> AuditQueue<N> {
    /// 创建空队列。
    ///
    /// # Panics
    ///
    /// 编译期常量 `N = 0` 时 panic（无意义容量，属集成层配置错误）。
    pub fn new() -> AuditQueue<N> {
        assert!(N > 0, "AuditQueue capacity must be > 0");
        AuditQueue { buf: [const { None }; N], head: 0, count: 0, overflow: 0 }
    }

    /// 容量。
    pub const fn capacity(&self) -> usize {
        N
    }

    /// 当前存量。
    pub const fn len(&self) -> usize {
        self.count
    }

    /// 是否为空。
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// 是否已满（下一次 push 将覆盖最旧）。
    pub const fn is_full(&self) -> bool {
        self.count == N
    }

    /// 累计覆盖条数（丢失可见性，见模块头注释）。
    pub const fn overflow(&self) -> u64 {
        self.overflow
    }

    /// 入队（O(1)，永不失败、永不阻塞）：
    /// 队满时覆盖最旧一条并递增 [`Self::overflow`]。
    ///
    /// 返回被覆盖的旧事件（正常入队为 `None`）——集成层可选择
    /// 将其转发到备用慢速通道。
    pub fn push(&mut self, ev: AuditEvent) -> Option<AuditEvent> {
        let evicted = if self.count == N {
            // 队满：tail 即 head（下一写入位置就是最旧条目）
            let old = self.buf[self.head].take();
            self.overflow += 1;
            old
        } else {
            None
        };
        debug_assert!(self.buf[self.head].is_none());
        self.buf[self.head] = Some(ev);
        self.head = (self.head + 1) % N;
        if self.count < N {
            self.count += 1;
        }
        evicted
    }

    /// 出队最旧一条（审计服务消费路径）；空 → `None`。
    pub fn pop(&mut self) -> Option<AuditEvent> {
        if self.count == 0 {
            return None;
        }
        let tail = (self.head + N - self.count) % N;
        let ev = self.buf[tail].take();
        self.count -= 1;
        ev
    }

    /// 批量出队（最旧优先）至 `out`，返回实际条数（批量提交路径，
    /// Doc 03 §6.3"批量提交减少每事件 IPC 次数"）。
    pub fn drain(&mut self, out: &mut [AuditEvent]) -> usize {
        let mut n = 0;
        while n < out.len() {
            match self.pop() {
                Some(ev) => {
                    out[n] = ev;
                    n += 1;
                }
                None => break,
            }
        }
        n
    }

    /// 清空（审计服务重启 / 测试路径）。overflow 计数保留（丢失历史不抹除）。
    pub fn clear(&mut self) {
        for slot in self.buf.iter_mut() {
            *slot = None;
        }
        self.head = 0;
        self.count = 0;
    }
}

/// 默认容量别名（集成层直接持有 `DefaultAuditQueue`）。
pub type DefaultAuditQueue = AuditQueue<DEFAULT_QUEUE_CAPACITY>;
