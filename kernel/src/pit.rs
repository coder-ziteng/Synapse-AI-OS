//! PIT (Programmable Interval Timer, Intel 8253/8254) 实现。
//!
//! PIT 提供系统时钟，用于时间片调度和延时。
//!
//! PIT 有 3 个通道：
//! - Channel 0: IRQ 0 (系统时钟，用于调度)
//! - Channel 1: DRAM 刷新 (现代系统已废弃)
//! - Channel 2: PC 扬声器 (可选)
//!
//! 我们使用 Channel 0，频率 1193182 Hz (基础时钟)。
//! 通过设置 divider 控制中断频率：
//! - 100 Hz: divider = 11932 (~10ms 时间片)
//! - 1000 Hz: divider = 1193 (~1ms 高精度)

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::instructions::port::Port;

/// PIT I/O 端口
const PIT_CHANNEL0_DATA: u16 = 0x40;
const PIT_COMMAND: u16 = 0x43;

/// PIT 基础频率 (Hz)
pub const PIT_FREQUENCY: u32 = 1193182;

/// 默认目标频率 (100 Hz = 10ms 时间片)
pub const DEFAULT_FREQUENCY: u32 = 100;

/// PIT 命令字节
const PIT_CHANNEL0: u8 = 0x00;
const PIT_LOHI: u8 = 0x30; // Access mode: lo/hi byte
const PIT_MODE3: u8 = 0x06; // Mode 3: Square wave generator

/// PIT 控制器
pub struct Pit {
    channel0_data: Port<u8>,
    command: Port<u8>,
    frequency: u32,
}

impl Pit {
    /// 创建 PIT 实例
    pub const fn new() -> Self {
        Pit {
            channel0_data: Port::new(PIT_CHANNEL0_DATA),
            command: Port::new(PIT_COMMAND),
            frequency: 0,
        }
    }

    /// 初始化 PIT
    pub unsafe fn init(&mut self, frequency: u32) {
        // 计算分频值
        let divisor = PIT_FREQUENCY / frequency;

        // 设置命令字节：Channel 0 + lo/hi + Mode 3
        let command_byte = PIT_CHANNEL0 | PIT_LOHI | PIT_MODE3;
        self.command.write(command_byte);

        // 写入分频值 (先低字节，后高字节)
        let divisor_lo = (divisor & 0xFF) as u8;
        let divisor_hi = ((divisor >> 8) & 0xFF) as u8;

        self.channel0_data.write(divisor_lo);
        self.channel0_data.write(divisor_hi);

        self.frequency = frequency;
    }

    /// 获取当前频率
    pub fn frequency(&self) -> u32 {
        self.frequency
    }

    /// 读取当前计数器值 (用于校准)
    pub unsafe fn read_counter(&mut self) -> u16 {
        // 发送 latch 命令 (read current count)
        self.command.write(PIT_CHANNEL0 | 0x00); // Latch command

        let lo = self.channel0_data.read() as u16;
        let hi = self.channel0_data.read() as u16;

        (hi << 8) | lo
    }
}

/// 全局 PIT 实例（`UnsafeCell` 包装，避免 `static mut` 引用导致的 UB 警告）。
///
/// 单核 MVP 下：`init()`/`read_counter()` 在中断关闭时调用；不与其他访问并发。
struct PitCell(UnsafeCell<Pit>);
unsafe impl Sync for PitCell {}

static PIT: PitCell = PitCell(UnsafeCell::new(Pit::new()));

/// 初始化 PIT
pub unsafe fn init(frequency: u32) {
    (*PIT.0.get()).init(frequency);
    log::info!("[pit] PIT initialized: target frequency={} Hz, base={} Hz",
               frequency, PIT_FREQUENCY);
}

/// 获取当前频率
pub unsafe fn frequency() -> u32 {
    (*PIT.0.get()).frequency()
}

/// 读取当前计数器值
pub unsafe fn read_counter() -> u16 {
    (*PIT.0.get()).read_counter()
}

/// IRQ 0 计数器 (每 10ms +1)。
///
/// 使用 `AtomicU64` 而非 `static mut`：中断上下文中 `+= 1`（fetch_add）与
/// 主线程读取（load）之间不存在数据竞争（Relaxed 足够，仅需原子性，不需排序保证）。
static TICK_COUNT: AtomicU64 = AtomicU64::new(0);

/// 时钟中断处理器 (由 IDT 调用)
pub fn timer_interrupt_handler() {
    TICK_COUNT.fetch_add(1, Ordering::Relaxed);

    // 未来：在这里调用调度器检查是否需要切换任务
    // scheduler::timer_tick();
}

/// 获取 tick 计数
pub fn tick_count() -> u64 {
    TICK_COUNT.load(Ordering::Relaxed)
}

/// 获取自启动以来的毫秒数（基于**实际配置频率**，而非硬编码默认值）。
///
/// P2-T7 修正：原实现用 `1000 / DEFAULT_FREQUENCY` 硬编码 10ms/tick，
/// 若 `init()` 传入非默认频率（如 1000Hz）换算即错误。
pub fn milliseconds() -> u64 {
    let ticks = tick_count();
    // SAFETY: 只读 PIT 配置字段；初始化前 frequency==0，此时返回 0。
    let freq = unsafe { frequency() } as u64;
    if freq == 0 {
        return 0;
    }
    ticks * 1000 / freq
}

/// 忙等待指定毫秒
pub fn busy_wait_ms(ms: u64) {
    let start = milliseconds();
    let target = start + ms;

    while milliseconds() < target {
        // 忙等待
        core::hint::spin_loop();
    }
}
