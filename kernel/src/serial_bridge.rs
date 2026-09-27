// Implement minimal serial interface used by io.rs
// re-export or implement serial_write_byte
pub fn serial_write_byte(b: u8) {
    // reuse existing UART HAL or uart_16550 crate; kernel already has serial.rs
    crate::serial::serial_write_byte(b)
}
