// Minimal console IO: write bytes to uart/serial
use crate::serial;

/// Write whole buffer to console (serial). Synchronous.
pub fn write_console_block(buf: &[u8]) {
    for &b in buf {
        // map LF to CRLF if desired; keep raw for now
        serial::serial_write_byte(b);
    }
}
