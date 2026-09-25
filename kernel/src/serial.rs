//! Synapse kernel — UART 16550 串口驱动（P1-T2 占位版）。
//!
//! 直接操作 MMIO 寄存器实现最小输出能力；QEMU `-serial stdio` 会把 UART 数据打到 stdout。
//! 本期是 **绕过 HAL** 的直接驱动；P1-T7 抽到 `synapse-hal::serial::SerialDevice` trait 后，
//! 本文件将被替换为薄 wrapper（仅做 trait 实现）。
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
use uart_16550::SerialPort;
use x86_64::instructions::interrupts;

/// 物理地址 `0x3F8`（COM1）—— IBM PC 约定 + QEMU 默认映射。
pub const SERIAL_IO_PORT: u16 = 0x3F8;

/// 初始化 UART：8N1 + FIFO 使能 + 默认 38400 baud。
///
/// 必须在开中断前调用（且调用时本就没有中断）；后续中断开启后此函数仍可重入
/// （`SerialPort::init` 内部 `without_interrupts` 包装）。
///
/// # 实现说明
///
/// `uart_16550::SerialPort::new` 返回的是**未初始化**的 MMIO 视图（仅记录基址）。
/// 必须把对象**绑定到本地 `let mut` 变量**并显式调用 `.init()`，否则闭包按值捕获
/// 会导致结构体字段落入栈/调试填充字节（0xCC），运行期访问时错跳。
pub fn init() {
    interrupts::without_interrupts(|| {
        // SAFETY: 0x3F8 是 IBM PC 兼容的 COM1 基址；QEMU 默认映射；
        //         本对象生命周期仅在闭包内，绝不外泄。
        let mut port = unsafe { SerialPort::new(SERIAL_IO_PORT) };
        port.init();
    });
}

/// 在关中断下访问串口，避免与中断处理路径竞争。
///
/// # Examples
/// ```
/// use synapse_kernel::serial;
/// serial::with_lock(|p| p.send(b'X'));
/// ```
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