#![no_std]
#![no_main]

use core::arch::global_asm;
use core::panic::PanicInfo;

global_asm!(include_str!("../test_mb.S"));

#[panic_handler]
fn panic(_: &PanicInfo) -> ! { loop {} }
