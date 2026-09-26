//! 基础启动测试 — 验证 QEMU 内 test framework 端到端运行（P1-T8）。
//!
//! ## 设计
//!
//! 完全自包含的测试二进制：自带 boot assembly + `_start64` 入口 + 测试框架 +
//! 测试运行器 + panic handler + bump allocator + 裸串口写入。
//!
//! 不依赖 `synapse_kernel` lib —— 避免与 `main.rs` 的 `#[global_allocator]` /
//! C 运行时符号冲突。
//!
//! ## 运行方式
//!
//! ```sh
//! # 一站式（build → 打包 → QEMU → 退出码翻译），仓库根目录执行：
//! python kernel/tests/run_tests.py
//! # 退出码：0 = 全部通过, 1 = 存在失败, 2 = 构建/打包/QEMU 异常
//! ```
//!
//! 等价的分步命令：
//!
//! ```sh
//! # 1. 编译测试二进制（注意：不能用裸 `cargo test` —— nightly-2026-09-23
//! #    在 Windows 上 test 模式 + -Zbuild-std 会构建两份 core（UNC 路径指纹
//! #    分裂），依赖 crate 触发 E0152 duplicate lang item；详见 run_tests.py）
//! cargo build --test basic_boot -p synapse-kernel \
//!     --target x86_64-bootloader.json \
//!     -Zjson-target-spec -Zbuild-std=core,alloc,compiler_builtins
//!
//! # 2. 打包 + QEMU 执行 + 退出码翻译（103→0 通过, 175→1 失败）
//! python kernel/tests/qemu_test_runner.py <上一步产物 ELF 路径>
//! ```

#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(test_runner::test_runner_main)]
#![reexport_test_harness_main = "test_main"]

extern crate alloc;

use core::arch::global_asm;
use core::ffi::{c_char, c_void};

// 链入启动 trampoline（共用 main.rs 的 boot.S，不修改）
global_asm!(include_str!("../src/boot.S"));

// ============================================================================
// C 运行时函数（lib 不重复提供 — 测试二进制自带）
// 签名与 C ABI 一致（c_void/c_char），避免 suspicious_runtime_symbol lint。
// ============================================================================

/// C 运行时 `memset`：以 `c` 填充 `n` 字节，返回 `s`。
#[no_mangle]
pub unsafe extern "C" fn memset(s: *mut c_void, c: i32, n: usize) -> *mut c_void {
    let dst = s;
    let val = c as u8;
    let count = n;
    core::arch::asm!(
        "rep stosb",
        inout("rdi") dst => _,
        in("al") val,
        inout("rcx") count => _,
        options(preserves_flags, nostack),
    );
    s
}

/// C 运行时 `memcpy`：拷贝 `n` 字节（不处理重叠），返回 `d`。
#[no_mangle]
pub unsafe extern "C" fn memcpy(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void {
    let dst = d;
    let src = s;
    let count = n;
    core::arch::asm!(
        "rep movsb",
        inout("rdi") dst => _,
        inout("rsi") src => _,
        inout("rcx") count => _,
        options(preserves_flags, nostack),
    );
    d
}

/// C 运行时 `memcmp`：相等返回 0，不等返回首个差异字节的差值。
#[no_mangle]
pub unsafe extern "C" fn memcmp(a: *const c_void, b: *const c_void, n: usize) -> i32 {
    // C 语义：相等返回 0，不等返回差值（负/正）。
    // 注意：不能用 `rep cmpsb + sete + neg` —— 那样"相等"会返回 -1，
    // 导致 String/slice 的 PartialEq（底层走 memcmp）对相等内容判为不等。
    let (pa, pb) = (a as *const u8, b as *const u8);
    let mut i = 0;
    while i < n {
        let x = *pa.add(i) as i32;
        let y = *pb.add(i) as i32;
        if x != y {
            return x - y;
        }
        i += 1;
    }
    0
}

/// C 运行时 `strlen`：返回 NUL 结尾字符串长度。
#[no_mangle]
pub unsafe extern "C" fn strlen(s: *const c_char) -> usize {
    let p = s as *const u8;
    let mut len = 0;
    while *p.add(len) != 0 { len += 1; }
    len
}

/// C 运行时 `memmove`：拷贝 `n` 字节（正确处理重叠），返回 `d`。
#[no_mangle]
pub unsafe extern "C" fn memmove(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void {
    let (pd, ps) = (d as *mut u8, s as *const u8);
    if (pd as usize) < (ps as usize) || (pd as usize) >= (ps as usize).wrapping_add(n) {
        memcpy(d, s, n)
    } else {
        let mut i = n;
        while i > 0 {
            i -= 1;
            *pd.add(i) = *ps.add(i);
        }
        d
    }
}

// ============================================================================
// 测试专用基础设施
// ============================================================================

mod test_serial {
    //! 裸 UART 16550 写入（不依赖 kernel::serial）。
    //!
    //! 注意：UART 寄存器通过 x86 I/O 端口（in/out 指令）访问，不是内存映射！
    //! 使用 `core::ptr::write_volatile` 写地址 0x3F8 不会到达 UART 芯片。

    const COM1: u16 = 0x3F8;
    const LSR: u16 = 0x3FD;
    const IER: u16 = 0x3F9;
    const FCR: u16 = 0x3FA;
    const LCR: u16 = 0x3FB;
    const MCR: u16 = 0x3FC;

    /// 向 I/O 端口写一个字节（`out dx, al` 指令）。
    unsafe fn outb(port: u16, val: u8) {
        core::arch::asm!(
            "out dx, al",
            in("dx") port,
            in("al") val,
            options(nostack, preserves_flags),
        );
    }

    /// 从 I/O 端口读一个字节（`in al, dx` 指令）。
    unsafe fn inb(port: u16) -> u8 {
        let val: u8;
        core::arch::asm!(
            "in al, dx",
            in("dx") port,
            out("al") val,
            options(nostack, preserves_flags),
        );
        val
    }

    /// 完整初始化 UART 16550（115200 8N1，FIFO 启用）。
    pub fn init() {
        unsafe {
            outb(IER, 0x00);      // 禁用中断
            outb(LCR, 0x80);      // DLAB=1，访问波特率分频器
            outb(COM1, 0x01);     // 分频器低字节 = 1 (115200 波特)
            outb(IER, 0x00);      // 分频器高字节 = 0
            outb(LCR, 0x03);      // 8N1, DLAB=0
            outb(FCR, 0xC7);      // 启用 FIFO, 清空, 14字节触发
            outb(MCR, 0x0B);      // DTR + RTS + OUT2

            core::arch::asm!("cli", options(nostack, preserves_flags));
        }
    }

    pub fn write_byte(b: u8) {
        unsafe {
            // 等待 THR 空 (LSR bit5 = 1)
            while (inb(LSR) & 0x20) == 0 {}
            outb(COM1, b);
        }
    }

    pub fn write_str(s: &str) {
        for b in s.bytes() { write_byte(b); }
    }

    pub fn write_fmt(args: core::fmt::Arguments) {
        use core::fmt::Write;
        struct FmtWriter;
        impl Write for FmtWriter {
            fn write_str(&mut self, s: &str) -> core::fmt::Result {
                write_str(s);
                Ok(())
            }
        }
        let _ = FmtWriter.write_fmt(args);
    }
}

/// 串口格式化输出（不换行）。
#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => {{
        $crate::test_serial::write_fmt(format_args!($($arg)*));
    }};
}

/// 串口格式化输出（换行）。
#[macro_export]
macro_rules! kprintln {
    () => { $crate::test_serial::write_str("\n") };
    ($($arg:tt)*) => {{
        $crate::test_serial::write_fmt(format_args!($($arg)*));
        $crate::test_serial::write_str("\n");
    }};
}

mod test_alloc {
    //! 测试用 bump allocator（4MB 固定池）。

    use core::alloc::{GlobalAlloc, Layout};
    use core::ptr;

    const POOL_SIZE: usize = 4 * 1024 * 1024;
    static mut POOL: [u8; POOL_SIZE] = [0u8; POOL_SIZE];
    static mut OFFSET: usize = 0;

    pub struct BumpAlloc;

    unsafe impl GlobalAlloc for BumpAlloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let align = layout.align();
            let size = layout.size();
            let aligned = (OFFSET + align - 1) & !(align - 1);
            if aligned + size > POOL_SIZE { return ptr::null_mut(); }
            OFFSET = aligned + size;
            // addr_of_mut 避免对 static mut 取 &mut（static_mut_refs lint）
            ptr::addr_of_mut!(POOL).cast::<u8>().add(aligned)
        }
        unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
    }

    #[global_allocator]
    static ALLOCATOR: BumpAlloc = BumpAlloc;
}

// ============================================================================
// 测试用例（用 #[test_case] 标记，编译器自动收集到测试列表）
// ============================================================================

/// 基础算术验证
#[test_case]
fn arithmetic_basic() {
    assert_eq!(1 + 1, 2);
    assert_eq!(7 * 6, 42);
}

/// 串口输出验证
#[test_case]
fn serial_output_works() {
    kprintln!("    [basic_boot] serial output test - UART working");
}

/// 堆分配验证
#[test_case]
fn heap_allocation_works() {
    use alloc::vec;
    let v = vec![1u32, 2, 3, 4, 5];
    assert_eq!(v.len(), 5);
    assert_eq!(v[4], 5);
}

/// 字符串格式化验证（间接调用 memset/memcpy/memcmp）
#[test_case]
fn format_string_works() {
    use alloc::string::String;
    // 直接内容比较：底层走 memcmp（相等必须返回 0）
    let s = alloc::format!("count = {}", 42u32);
    assert_eq!(s, "count = 42");
    drop(s);
    let s2 = String::from("hello");
    assert_eq!(s2.len(), 5);
}

// ============================================================================
// 测试入口
// ============================================================================

/// 测试运行器（由 `#![test_runner(...)]` 指定）
///
/// 接收所有 `#[test_case]` 函数引用，依次执行，统计 pass/fail 计数，
/// 通过 isa-debug-exit 退出 QEMU。
///
/// 注意：custom_test_frameworks 要求 test_runner 返回 `()`，
/// `_start64` 在调用 `test_main()` 后兜底循环。
///
/// 退出码约定（避免与正常启动 0xB5→107 冲突）：
/// - 全部通过：val=0x33 → exit=((0x33&0x7F)<<1)|1 = 103
/// - 存在失败：val=0x55 → exit=((0x55&0x7F)<<1)|1 = 175
mod test_runner {
    pub fn test_runner_main(tests: &[&dyn Fn()]) {
        crate::test_serial::init();

        kprintln!("\n=== Synapse Kernel Test Suite ===");
        kprintln!("Running {} tests...", tests.len());

        let mut passed = 0u32;
        // panic handler 直接以 175 退出 QEMU，suite 内 failed 恒为 0；
        // 保留计数仅为输出格式统一。
        let failed = 0u32;

        // panic handler 通过退出 QEMU 上报失败；此处顺序执行，
        // 任一测试 panic 即终止整个 suite（exit code 175）。
        for (i, test) in tests.iter().enumerate() {
            kprint!("  test {} ... ", i);
            test();
            kprintln!("ok");
            passed += 1;
        }

        kprintln!();
        kprintln!("=== Results: {} passed, {} failed ===", passed, failed);

        let exit_code: u8 = if failed == 0 { 0x33 } else { 0x55 };
        unsafe {
            core::arch::asm!(
                "mov dx, 0x502",
                "mov al, {code}",
                "out dx, al",
                code = in(reg_byte) exit_code,
                options(nostack, preserves_flags),
            );
        }

        loop { unsafe { core::arch::asm!("hlt", options(nostack, preserves_flags)); } }
    }
}

/// 64-bit 入口（boot.S trampoline 跳转目标）。
/// 编译器生成的 `test_main`（由 `#![reexport_test_harness_main]`）会调用测试运行器。
/// `test_main` 永不返回（test_runner 退出 QEMU），但编译器生成的 main 返回 `()`，
/// 所以我们仍然需要一个兜底循环。
#[no_mangle]
pub extern "C" fn _start64() -> ! {
    test_main();
    // 兜底（正常不会到这里）
    loop {
        unsafe { core::arch::asm!("hlt", options(nostack, preserves_flags)); }
    }
}

/// 测试 panic handler — 任何 assert 失败都走这里，退出 QEMU with code 175
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    kprintln!("\n[TEST PANIC] {}", info);
    if let Some(loc) = info.location() {
        kprintln!("  at {}:{}:{}", loc.file(), loc.line(), loc.column());
    }
    // isa-debug-exit: val=0x55 → exit code ((0x55&0x7F)<<1)|1 = 175 = 存在失败
    unsafe {
        core::arch::asm!(
            "mov dx, 0x502",
            "mov al, 0x55",
            "out dx, al",
            options(nostack, preserves_flags),
        );
    }
    loop { unsafe { core::arch::asm!("hlt", options(nostack, preserves_flags)); } }
}