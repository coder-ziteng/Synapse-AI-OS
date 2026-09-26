//! 内核日志实现（P1-T5）。
//!
//! 实现 `log::Log` trait，将日志输出到串口（COM1）。
//! 使用 `log::set_logger` 注册全局 logger。

use core::fmt::Write;
use log::{Level, Log, Metadata, Record, SetLoggerError};

use crate::serial::Uart16550;
use synapse_hal::serial::SerialDevice;

/// 全局 logger 实例。
struct KernelLogger;

impl Log for KernelLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Info
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            let mut uart = Uart16550;
            let _ = writeln!(
                uart,
                "[{}] {}",
                record.level(),
                record.args()
            );
        }
    }

    fn flush(&self) {
        // UART 无缓冲，无需 flush
    }
}

impl Write for Uart16550 {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for byte in s.bytes() {
            let _ = SerialDevice::write_byte(self, byte);
        }
        Ok(())
    }
}

static LOGGER: KernelLogger = KernelLogger;

/// 初始化全局 logger。
///
/// 必须在串口初始化后调用。
pub fn init() -> Result<(), SetLoggerError> {
    log::set_logger(&LOGGER).map(|()| log::set_max_level(log::LevelFilter::Info))
}
