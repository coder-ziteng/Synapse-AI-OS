//! 串口设备抽象。
//!
//! [`SerialDevice`] 定义最小串口接口（初始化、字节收发）。具体实现按架构在 `arch::` 子模块提供。
//!
//! # 设计要点
//!
//! * **零成本**：trait 方法均为 `&mut self`，编译期单态化，无 vtable。
//! * **no_std 兼容**：不依赖 `std::io`，使用自定义 `SerialError`。
//! * **可测试**：`cfg(test)` 下提供 `FakeSerial` 实现，可在宿主 `cargo test` 中验证上层逻辑。

/// 串口操作错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SerialError {
    /// 发送缓冲满（非阻塞模式下）。
    TxFull,
    /// 接收缓冲空（非阻塞模式下）。
    RxEmpty,
    /// 硬件故障（如 UART 未就绪）。
    HardwareFault,
}

/// 串口设备最小接口。
///
/// # 实现约定
///
/// * `init()` 必须在首次收发前调用，且只调用一次（或幂等）。
/// * `write_byte()` 阻塞等待 THR 空（典型 UART 行为）；如需非阻塞，实现方应返回 `Err(SerialError::TxFull)`。
/// * `read_byte()` 非阻塞：无数据时返回 `Err(SerialError::RxEmpty)`。
pub trait SerialDevice {
    /// 初始化串口（波特率、字长、停止位、奇偶校验）。
    fn init(&mut self);

    /// 发送单字节。阻塞直到发送完成。
    fn write_byte(&mut self, byte: u8) -> Result<(), SerialError>;

    /// 接收单字节。非阻塞：无数据时返回 `Err(SerialError::RxEmpty)`。
    fn read_byte(&mut self) -> Result<u8, SerialError>;

    /// 批量发送（默认实现逐字节调用 `write_byte`）。
    fn write_all(&mut self, data: &[u8]) -> Result<(), SerialError> {
        for &b in data {
            self.write_byte(b)?;
        }
        Ok(())
    }
}

/// 宿主测试用 fake 实现。
#[cfg(test)]
pub struct FakeSerial {
    pub tx_buf: std::vec::Vec<u8>,
    pub rx_buf: std::collections::VecDeque<u8>,
}

#[cfg(test)]
impl FakeSerial {
    pub fn new() -> Self {
        Self {
            tx_buf: std::vec::Vec::new(),
            rx_buf: std::collections::VecDeque::new(),
        }
    }

    pub fn push_rx(&mut self, data: &[u8]) {
        self.rx_buf.extend(data);
    }
}

#[cfg(test)]
impl SerialDevice for FakeSerial {
    fn init(&mut self) {
        // no-op
    }

    fn write_byte(&mut self, byte: u8) -> Result<(), SerialError> {
        self.tx_buf.push(byte);
        Ok(())
    }

    fn read_byte(&mut self) -> Result<u8, SerialError> {
        self.rx_buf.pop_front().ok_or(SerialError::RxEmpty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_serial_tx_rx() {
        let mut s = FakeSerial::new();
        s.init();
        s.write_byte(b'A').unwrap();
        assert_eq!(s.tx_buf, std::vec![b'A']);

        s.push_rx(&[b'B', b'C']);
        assert_eq!(s.read_byte().unwrap(), b'B');
        assert_eq!(s.read_byte().unwrap(), b'C');
        assert_eq!(s.read_byte(), Err(SerialError::RxEmpty));
    }
}
