//! initramfs 引导记录（P4-T5）。
//!
//! ## 数据来源（build_disk.py 契约）
//!
//! `build_disk.py` 把 cpio newc initramfs 追加在 kernel.bin 之后；stage2
//! 的加载循环把 (kernel + initramfs) **连续**读入物理 0x200000+，然后在
//! 32-bit PM（平坦段、分页未开）往物理 [`INITRD_INFO_ADDR`] = 0x20100 写：
//!
//! ```text
//! +0x00 : u64 LE  initrd 物理基址（= 0x200000 + ksectors*512）
//! +0x08 : u64 LE  initrd 字节长度（0 = 镜像未打包 initramfs）
//! ```
//!
//! 0x20100 位于 E820 raw buffer（0x20000..0x200AC）之后的空闲低内存，
//! 两者都在页帧分配器的"低 1MB 保留区"内（page_frame.rs 步骤 1.5）。
//!
//! ## 消费方
//!
//! - [`crate::page_frame`] init：把 initrd 区间出账（防被分配清零）；
//! - [`crate::elfload`]：`bytes()` → cpio 提取 `hello` ELF → 装入用户 AS。

use core::sync::atomic::{AtomicU64, Ordering};

use log::info;

/// 引导记录物理地址（stage2 写入；布局见模块头）。
pub const INITRD_INFO_ADDR: usize = 0x20100;

static INITRD_BASE: AtomicU64 = AtomicU64::new(0);
static INITRD_SIZE: AtomicU64 = AtomicU64::new(0);
static INIT_DONE: AtomicU64 = AtomicU64::new(0);

/// 读取并校验引导记录（boot 链路上调一次：`memory_map_init` 之后、
/// `init_page_frame_allocator` 之前——分配器要消费 [`region`] 做保留）。
///
/// size > 0 时校验驻留内存首部的 cpio newc magic（构建契约破坏 → panic）。
pub fn initrd_init() {
    if INIT_DONE.swap(1, Ordering::SeqCst) != 0 {
        panic!("initrd_init called twice");
    }
    // SAFETY: 0x20100 在 boot 恒等映射低 1MB 内（stage2 跳转前已写完记录），
    // 8 字节对齐读两个 u64；本函数是记录的唯一 Rust 侧读者。
    let (base, size) = unsafe {
        let p = INITRD_INFO_ADDR as *const u64;
        (p.read_volatile(), p.add(1).read_volatile())
    };

    if size > 0 {
        // SAFETY: base 指向 stage2 刚加载的 initramfs（内核 0-4GB 恒等映射，
        // VA==PA）；size 为 stage2 实际写入的字节数。
        let slice = unsafe { core::slice::from_raw_parts(base as *const u8, size as usize) };
        assert!(
            synapse_elf::cpio::validate_magic(slice).is_ok(),
            "[initrd] bad cpio newc magic at base={:#x} size={:#x}",
            base,
            size
        );
    }

    INITRD_BASE.store(base, Ordering::SeqCst);
    INITRD_SIZE.store(size, Ordering::SeqCst);
    info!(
        "[initrd] boot record @ {:#x}: base={:#x} size={:#x} ({} KB)",
        INITRD_INFO_ADDR,
        base,
        size,
        size / 1024
    );
}

/// initramfs 驻留区间 `(物理基址, 字节长度)`；size==0（未打包）返回 None。
pub fn region() -> Option<(u64, u64)> {
    let size = INITRD_SIZE.load(Ordering::SeqCst);
    if size == 0 {
        None
    } else {
        Some((INITRD_BASE.load(Ordering::SeqCst), size))
    }
}

/// initramfs 字节切片（恒等映射直接可读；未打包返回 None）。
///
/// # Safety 说明
///
/// 区间由 page_frame 分配器出账保留（`init_page_frame_allocator` 步骤 2.5），
/// 生命周期 = 内核运行期，故以 `'static` 借出是安全的。
pub fn bytes() -> Option<&'static [u8]> {
    region().map(|(base, size)|
        // SAFETY: 见 region()；区间已出账保留，无别名写入者。
        unsafe { core::slice::from_raw_parts(base as *const u8, size as usize) })
}
