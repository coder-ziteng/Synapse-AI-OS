//! Synapse kernel — UART 16550 串口驱动（P1-T7 HAL 化版本）。
//!
//! 实现 `synapse_hal::serial::SerialDevice` trait，底层仍用 `uart_16550` crate。
//! 中断安全通过 `interrupts::without_interrupts` 保证。
//!
//! # 寄存器（基地址 0x3F8，COM1）
//!
//! | 偏移 | 寄存器        | 用途                              |
//! | ---- | ------------- | --------------------------------- |
//! | 0    | RBR / THR     | 接收/发送缓冲                     |
//! | 1    | IER           | 中断使能（本期全关）               |
//! | 2    | FCR / IIR     | FIFO 控制 / 中断状态              |
//! | 3    | LCR           | 线路控制（DLAB=1 时为分频低字节） |
//! | 4    | MCR           | Modem 控制                        |
//! | 5    | LSR           | 线路状态（bit5 = THR 空）         |

use core::fmt::{self, Write};
use synapse_hal::serial::{SerialDevice, SerialError};
use uart_16550::SerialPort;
use x86_64::instructions::interrupts;

/// 物理地址 `0x3F8`（COM1）—— IBM PC 约定 + QEMU 默认映射。
pub const SERIAL_IO_PORT: u16 = 0x3F8;

/// UART 16550 设备（零尺寸标记类型）。
///
/// 实际硬件状态在全局 `SerialPort` 实例中；本类型仅提供 trait 实现的命名空间。
pub struct Uart16550;

impl SerialDevice for Uart16550 {
    fn init(&mut self) {
        interrupts::without_interrupts(|| {
            // SAFETY: 0x3F8 是 IBM PC 兼容的 COM1 基址；QEMU 默认映射。
            let mut port = unsafe { SerialPort::new(SERIAL_IO_PORT) };
            port.init();
        });
    }

    fn write_byte(&mut self, byte: u8) -> Result<(), SerialError> {
        interrupts::without_interrupts(|| {
            let mut port = unsafe { SerialPort::new(SERIAL_IO_PORT) };
            port.send(byte);
            Ok(())
        })
    }

    fn read_byte(&mut self) -> Result<u8, SerialError> {
        // TODO: 实现非阻塞读取（检查 LSR bit0）
        // 当前占位：始终返回 RxEmpty
        Err(SerialError::RxEmpty)
    }
}

/// 串口硬件初始化标记（原 `static mut GLOBAL_SERIAL: Option<Uart16550>` 已移除：
/// `Uart16550` 是零尺寸标记类型，无实例状态可存，`static mut` 只剩"是否初始化过"
/// 一个 bit 的语义，且触发 `static_mut_refs` 2024 弃用警告）。
static SERIAL_INITIALIZED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// 初始化全局串口（必须在首次使用前调用；重复调用幂等）。
pub fn init() {
    use core::sync::atomic::Ordering;
    SERIAL_INITIALIZED.store(true, Ordering::Relaxed);
    // 真正的硬件初始化（16550 寄存器编程）在 trait init 内完成，关中断保护。
    SerialDevice::init(&mut Uart16550);
}

/// 串口是否已初始化（诊断用）。
pub fn is_initialized() -> bool {
    use core::sync::atomic::Ordering;
    SERIAL_INITIALIZED.load(Ordering::Relaxed)
}

/// 在关中断下访问串口，避免与中断处理路径竞争。
pub fn with_lock<F, R>(f: F) -> R
where
    F: FnOnce(&mut SerialPort) -> R,
{
    interrupts::without_interrupts(|| {
        // SAFETY: 同 init()；构造只在闭包内使用一次，结构体绑定到本地变量。
        let mut port = unsafe { SerialPort::new(SERIAL_IO_PORT) };
        f(&mut port)
    })
}

/// 格式化输出到串口。`interrupts::without_interrupts` 包裹以防字符被中断撕裂。
#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => {{
        $crate::serial::with_lock(|w| {
            use core::fmt::Write as _;
            let _ = write!(w, $($arg)*);
        });
    }};
}

/// 同 `kprint!` 但末尾追加换行。
#[macro_export]
macro_rules! kprintln {
    () => { $crate::kprint!("\n") };
    ($($arg:tt)*) => {{
        $crate::serial::with_lock(|w| {
            use core::fmt::Write as _;
            let _ = writeln!(w, $($arg)*);
        });
    }};
}

/// 简单的 `Write` 实现 — 备用；Phase 5 引入 `log` crate 后将废弃。
pub struct Stdout;

impl Write for Stdout {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        with_lock(|p| {
            for byte in s.bytes() {
                p.send(byte);
            }
        });
        Ok(())
    }
}