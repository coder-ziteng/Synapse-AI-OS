//! Synapse kernel binary entry point.
//!
//! Phase 1（P1-T2 + P1-T2-bis）实现：
//! * `boot.S` trampoline：符号 `_start`，从 QEMU PVH 加载（32-bit 保护模式）开始，
//!   完成 4 级页表建立 + 切换到 64-bit 长模式 + 跳到 `_start64`。
//! * `_start64`（64-bit Rust）初始化 UART 16550 + 打印 "Hello, Synapse!" + 死循环。
//!
//! 入口约定：
//! * 链接器 entry 是 `_start`（在 `boot.S` 中定义）。
//! * `_start64` 由 trampoline 远跳到达。
//! * 不返回；崩溃走 [`panic_handler`]。

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

pub mod serial;
pub mod logger;
pub mod sync;
pub mod kstate;
pub mod memory_map;
pub mod page_frame;
pub mod heap;
pub mod gdt;
pub mod idt;
pub mod pic;
pub mod pit;
pub mod clock;
pub mod kthread;
pub mod paging;
pub mod mutex;
pub mod bootstrap;
pub mod smoke;
pub mod syscall;
pub mod ring3;
pub mod initrd;
pub mod elfload;

// 把 trampoline 汇编链入二进制；`boot.S` 中 `.global _start` 提供链接器 entry。
global_asm!(include_str!("boot.S"));

// `core::fmt` 等格式化代码路径会调用 memset/memcpy/memcmp。
// compiler_builtins rlib 在 x86_64-unknown-none 上是 “thin wrapper”，并不真提供这些
// —— 我们必须自己填。ABI: rdi=dst, rsi=src, rdx=n，返回 dst。
//
// 签名必须与 C 运行时符号的期望原型一致（c_void/c_int/c_char），否则触发
// `suspicious_runtime_symbol_definitions` lint；每个函数补文档满足 `-W missing-docs`。
use core::ffi::{c_char, c_int, c_void};

/// C 运行时 `memset`：把 `s` 起 `n` 字节填充为 `c`（取低 8 位），返回 `s`。
#[no_mangle]
pub unsafe extern "C" fn memset(s: *mut c_void, c: c_int, n: usize) -> *mut c_void {
    let dst = s as *mut u8;
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

/// C 运行时 `memcpy`：从 `s` 向 `d` 拷贝 `n` 字节（不允许重叠），返回 `d`。
#[no_mangle]
pub unsafe extern "C" fn memcpy(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void {
    let dst = d as *mut u8;
    let src = s as *const u8;
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

/// C 运行时 `memcmp`：比较 `a`/`b` 起 `n` 字节。
///
/// C 语义：相等返回 0，不等返回首个差异字节的差值（负/正）。
/// 注意：不能用 `rep cmpsb + sete + neg` —— 那样"相等"会返回 -1，
/// 导致 String/slice 的 PartialEq（底层走 memcmp）对相等内容判为不等。
/// （与 P1-T8 basic_boot.rs 中发现并修复的 bug 同源。）
#[no_mangle]
pub unsafe extern "C" fn memcmp(a: *const c_void, b: *const c_void, n: usize) -> c_int {
    let a = a as *const u8;
    let b = b as *const u8;
    let mut i = 0;
    while i < n {
        let x = *a.add(i) as c_int;
        let y = *b.add(i) as c_int;
        if x != y {
            return x - y;
        }
        i += 1;
    }
    0
}

/// C 运行时 `strlen`：返回 NUL 结尾字符串 `s` 的长度（不含 NUL）。
#[no_mangle]
pub unsafe extern "C" fn strlen(s: *const c_char) -> usize {
    let mut len = 0;
    while *s.add(len) != 0 {
        len += 1;
    }
    len
}

/// C 运行时 `memmove`：从 `s` 向 `d` 拷贝 `n` 字节（允许重叠），返回 `d`。
///
/// 重叠方向判定：`d < s` 或 `d` 完全落在 `s` 区间之后 → 正向拷贝安全；
/// 否则（`d` 与 `s` 重叠且 `d > s`）必须反向拷贝防止先写后读。
#[no_mangle]
pub unsafe extern "C" fn memmove(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void {
    let d_u8 = d as *mut u8;
    let s_u8 = s as *const u8;
    if (d as usize) < (s as usize) || (d as usize) >= (s as usize).wrapping_add(n) {
        // 不重叠或 d 在 s 之前：正向拷贝
        memcpy(d, s, n)
    } else {
        // 重叠且 d 在 s 之后：反向拷贝
        let mut i = n;
        while i > 0 {
            i -= 1;
            *d_u8.add(i) = *s_u8.add(i);
        }
        d
    }
}

/// 引导插桩：向 debugcon 0x501 写一个字节（定位启动崩溃点用，P1 收尾后可删）。
fn boot_marker(ch: u8) {
    unsafe {
        asm!(
            "out dx, al",
            in("dx") 0x501u16,
            in("al") ch,
            options(nostack, preserves_flags),
        );
    }
}

/// Kernel 64-bit 入口（长模式 + 4 级页表已建、栈有效）。
///
/// 由 `build_disk.py` stage2 trampoline（16→32→64）`retf` 过来；
/// 本函数是 Rust 64-bit 代码的真正起点。入口时 `rsp = 0xEFFF8`（满足
/// Rust ABI：入口 `rsp % 16 == 8`）。
///
/// # Safety
///
/// * 调用方必须保证：CPL=0、长模式已开、4 级页表已建（0-4GB 恒等映射）、栈有效。
/// * 本函数永不返回 (`-> !`)。
#[no_mangle]
pub extern "C" fn _start64() -> ! {
    // 引导链路标记：debugcon 0x501 输出 'A','B'（与 stage2 的 0x402 诊断配合）
    unsafe {
        asm!(
            "mov dx, 0x501",
            "mov al, 0x41",      // 'A' = arrived in _start64
            "out dx, al",
            "mov al, 0x42",      // 'B' = first asm block executed
            "out dx, al",
            options(nostack, preserves_flags),
        );
    }

    // P1-T5 真实路径：串口 + 全局 logger 初始化，之后走 log 宏
    // 插桩: 'C'/'D'/'E'/'F' 标记各步骤（debugcon 0x501，定位崩溃用）
    boot_marker(b'C');
    serial::init();
    boot_marker(b'D');
    let _ = logger::init();
    boot_marker(b'E');

    log::info!("Hello, Synapse!");
    kprintln!("[boot] _start64: long mode + 4-level paging active");
    boot_marker(b'F');

    // P2-T3 集成层（cap/ipc/proc 真机验证）：G/H 标记 bootstrap，J/K 标记 memory_map，I 标记 smoke
    boot_marker(b'G');
    let refs = bootstrap::kernel_bootstrap();
    boot_marker(b'H');

    // P2-T1 Memory Map：解析 boot.S 在 32-bit 保护模式写入 0x20000 的 E820 buffer
    boot_marker(b'J');
    memory_map::memory_map_init();
    boot_marker(b'K');

    // P4-T5 initramfs：读 stage2 写的 0x20100 {base,size} 记录 + cpio magic 校验。
    // 必须在 page_frame init 之前——分配器步骤 2.5 消费 initrd::region() 出账保留。
    boot_marker(b'm');
    initrd::initrd_init();
    boot_marker(b'n');

    // P2-T2 物理页帧分配器：消费 MEMORY_MAP，bitmap 管理 4KB 帧
    boot_marker(b'L');
    page_frame::init_page_frame_allocator();
    boot_marker(b'M');

    // P2-T3 内核堆：基于 page_frame 的 first-fit 分配器（1MB 池）
    boot_marker(b'N');
    heap::init_heap(256); // 256 页 = 1MB
    boot_marker(b'O');

    // P2-T4 GDT/TSS：运行时 GDT（含 ring-3 预留 + TSS）+ IST1=double fault 栈
    boot_marker(b'P');
    let _gdt_sels = unsafe { gdt::init_gdt_tss() };
    boot_marker(b'Q');

    // P2-T5 IDT：装好 #DE/#BP/#UD/#DF(IST1)/#GP/#PF handler
    boot_marker(b'R');
    unsafe { idt::init_idt() };
    boot_marker(b'S');

    // P4-T4 syscall 接线：MSR_STAR/LSTAR/FMASK + IA32_KERNEL_GS_BASE（须在 IDT 之后）
    boot_marker(b'i');
    unsafe { syscall::init_syscall() };
    boot_marker(b'j');

    // P2-T6 PIC + PIT：中断控制器 + 定时器
    boot_marker(b'T');
    unsafe { pic::init() };
    boot_marker(b'U');
    unsafe { pit::init(pit::DEFAULT_FREQUENCY) };
    boot_marker(b'V');
    unsafe { pic::enable_irq(0) }; // 解除 IRQ 0 (定时器) 屏蔽
    boot_marker(b'W');
    x86_64::instructions::interrupts::enable(); // 开启中断
    boot_marker(b'X');

    // P2-T7 时钟基准：TSC ↔ PIT 交叉校准（需中断开启 + PIT 已跑，阻塞 ~100ms）
    boot_marker(b'Y');
    clock::calibrate();
    boot_marker(b'Z');

    smoke::run_integration_smoke(&refs);
    boot_marker(b'I');

    // P3-T4 内核线程基建 smoke：boot 线程收编 + kthread 创建只入队不切换 + 栈对齐断言
    boot_marker(b'a');
    kthread::kthread_smoke();
    boot_marker(b'b');

    // P3-T5 switch_to 真机 smoke：上下文切换双向贯通
    boot_marker(b'c');
    kthread::kthread_switch_smoke();
    boot_marker(b'd');

    // P3-T6 抢占模型 smoke：3 worker 并发计数/打印 + 时间片抢占 + yield/sleep/exit
    //（s6 分支上暂时注释——保留原状；marker e/f 归 P3-T6）
    boot_marker(b'e');
    // kthread::kthread_preempt_smoke();
    boot_marker(b'f');

    // P3-T7 Mutex 睡眠锁 + sleep_until_ms 真机 smoke
    boot_marker(b'g');
    kthread::kthread_mutex_smoke();
    boot_marker(b'h');

    // P4-T2 分页 smoke：AddressSpace 新建 → CR3 切换 → 用户页读写 → #PF 期望故障 → FR8 归零
    //（合并注：原用 marker e/f，与 P3-T6 撞号 → 迁 o/p）
    boot_marker(b'o');
    paging::paging_smoke();
    boot_marker(b'p');

    // P4-T3 VMA / demand paging smoke：VMA 注册 → 按需分页 → 权限违例/未注册 → kill 骨架 → NULL 守卫 → FR8 归零
    //（合并注：原用 marker g/h，与 P3-T7 撞号 → 迁 q/r）
    boot_marker(b'q');
    paging::vma_smoke();
    boot_marker(b'r');

    // P4-T4 Ring3 切换 smoke：用户态 _start → syscall abi_query 往返 → CPL==3 断言
    // → continuation 链式接力 P4-T5 elf_load_smoke（exit 363 在 elf_continuation 发出）
    boot_marker(b'k');
    ring3::ring3_smoke();
    boot_marker(b'l');

    // 通过 isa-debug-exit (iobase=0x502) 退出 QEMU。
    // QEMU isa-debug-exit 实现为 exit((val << 1) | 1)（无掩码），
    // 所以 val=0xB5 → exit code = (0xB5 << 1) | 1 = 363。
    unsafe {
        asm!(
            "mov dx, 0x502",
            "mov al, 0xB5",
            "out dx, al",
            options(nostack, preserves_flags),
        );
    }

    // 不会到这里（上面的 out 已触发 QEMU 退出）
    loop {
        unsafe {
            asm!("hlt", options(nostack, preserves_flags));
        }
    }
}

/// Panic handler（P1-T5 完整版）。
///
/// 输出：panic 消息 + 位置（文件:行:列）+ 基于 rbp 链的栈回转。
/// 依赖 `.cargo/config.toml` 中 `-C force-frame-pointers=yes`。
/// 回转打印的是裸地址，用 `rust-addr2line -e target/.../synapse-kernel -f 0xADDR`
/// 可离线解析出函数名+行号。
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    x86_64::instructions::interrupts::disable();

    kprintln!("[PANIC] {}", info);
    if let Some(loc) = info.location() {
        kprintln!("  at {}:{}:{}", loc.file(), loc.line(), loc.column());
    }

    // ---- 帧指针栈回转: rbp → [rbp]=上一帧 rbp, [rbp+8]=返回地址 ----
    kprintln!("backtrace (most recent call first):");
    unsafe {
        let mut rbp: usize;
        asm!("mov {}, rbp", out(reg) rbp, options(nostack, preserves_flags));
        for frame in 0..32usize {
            // 合理性检查：帧指针必须 8 字节对齐且位于已映射的低 4GB
            if rbp == 0 || rbp % 8 != 0 || rbp >= 0x1_0000_0000 - 16 {
                break;
            }
            let next_rbp = *(rbp as *const usize);
            let ret_addr = *((rbp + 8) as *const usize);
            if ret_addr == 0 {
                break;
            }
            kprintln!("  #{:02} 0x{:016x}", frame, ret_addr);
            // 帧指针必须单调递增（栈向低地址增长），否则链已损坏
            if next_rbp <= rbp {
                break;
            }
            rbp = next_rbp;
        }
    }

    // panic 也要产出**确定性退出码**（否则 QEMU 挂死在下面的 hlt 循环，
    // CI/一键脚本只能靠超时判定失败）。
    // isa-debug-exit (iobase=0x502)：exit code = (val << 1) | 1
    //   val=0xB1 → 355 = kernel panic 出口（区别于成功路径 0xB5 → 363）
    unsafe {
        asm!(
            "mov dx, 0x502",
            "mov al, 0xB1",
            "out dx, al",
            options(nostack, preserves_flags),
        );
    }

    loop {
        x86_64::instructions::hlt();
    }
}
