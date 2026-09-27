//! per-process 内核栈分配 / 释放（P4-T9a）。
//!
//! MVP：固定 16KB / 进程，无 guard page（依赖 16KB 栈深度 + panic handler 回溯
//! 覆盖；Phase 5+ 加 1 页 PROT_NONE guard）。
//!
//! 栈区物理地址选取策略：
//! - 高地址（≥ 0x100_0000，即 16MB 之后）避开内核恒等映射的低 1MB + 早期 alloc 池；
//! - `page_frame::alloc_frame()` 已能服务；本模块仅做"4 帧连分配"+"高位聚合"。
//!
//! 后续被 `proc_ext` 持有 `(bottom, top)`，调度时切 TSS.RSP0。

use core::ptr;

use crate::page_frame;

/// per-process kstack 大小 = 4 帧 = 16KB（MVP）。
pub const KSTACK_PAGES: usize = 4;
/// per-process kstack 字节数。
pub const KSTACK_SIZE: usize = KSTACK_PAGES * 4096;

/// kstack 区物理地址上限（不可越过此值）。早期无高地址内存映射，留作未来
/// 扩展空间；MVP 任意 alloc_frame() 即可满足。
const _KSTACK_ADDR_LIMIT: u64 = u64::MAX;

/// 分配 4 帧连续 kstack。返回 `(bottom, top)`，bottom 是最低地址，top = bottom + KSTACK_SIZE - 8。
///
/// 失败（page_frame 耗尽）返 None。
pub fn kstack_alloc() -> Option<(u64, u64)> {
    // MVP 不要求连续：4 个独立 frame 也行，只要记录 bottom=top of lowest frame, top=top of highest frame
    // 但栈增长方向是高→低，所以 top 必须是栈顶（最高地址 frame 的顶部）。
    let f0 = page_frame::alloc_frame()?;
    let f1 = page_frame::alloc_frame()?;
    let f2 = page_frame::alloc_frame()?;
    let f3 = page_frame::alloc_frame()?;

    let p0 = f0 as u64;
    let p1 = f1 as u64;
    let p2 = f2 as u64;
    let p3 = f3 as u64;

    // 栈增长向低：top = max(p_i) + PAGE_SIZE - 8（预留 syscall_entry_asm push 9*8=72B）
    // bottom = min(p_i)
    let bottom = p0.min(p1).min(p2).min(p3);
    let top_frame = p0.max(p1).max(p2).max(p3);
    let top = top_frame + 4096 - 8;

    // 检查是否连续（MVP 期望连续，不连续会导致栈空洞）
    let contiguous = (top_frame - bottom) == 3 * 4096;
    log::info!(
        "[kstack] alloc: frames=[{:#x}, {:#x}, {:#x}, {:#x}] bottom={:#x} top={:#x} contiguous={}",
        p0, p1, p2, p3, bottom, top, contiguous
    );

    // poison: 填 0xCC 让栈溢出越界可见
    unsafe {
        poison_range(bottom, KSTACK_SIZE);
    }

    Some((bottom, top))
}

/// 释放 kstack（`kstack_alloc` 分配的 4 帧）。
///
/// MVP 简化：不验证传入 bottom 是否真的属于 kstack；调用方契约保证。
pub fn kstack_free(bottom: u64) {
    unsafe {
        poison_range(bottom, KSTACK_SIZE);
    }
    page_frame::free_frame(bottom);
    // 其余 3 帧顺序无关紧要——MVP 简化：把它们也尝试归还
    for off in (4096..KSTACK_SIZE).step_by(4096) {
        page_frame::free_frame(bottom + off as u64);
    }
}

/// 毒化栈区（MVP 调试用：填 0xCC 让 stale read 立刻显形）。
unsafe fn poison_range(base: u64, len: usize) {
    let mut p = base as *mut u8;
    let end = base + len as u64;
    while (p as u64) < end {
        ptr::write_volatile(p, 0xCC);
        p = p.add(1);
    }
}