//! 物理页帧分配器（4KB 粒度位图分配器）。
//!
//! ## 设计
//!
//! 线性扫描位图：`bitmap[frame/64] & (1 << (frame%64))` 为 1 表示空闲。
//! 分配从上次成功位置继续（hint 指针），减少碎片化扫描成本。
//!
//! **容量上限**：`MAX_FRAMES = 32768`（128MB 内存上限）。bitmap 占 512 u64 = 1 页。
//! 超出 128MB 的内存不会被本分配器管理（留给后续 buddy 或更复杂分配器）。
//!
//! **保留帧**：
//! - 0..1MB：BIOS/EBDA/IVT 区（E820 已标 Reserved，不进入 bitmap）
//! - 0x200000..`_kernel_end`：内核自身（ELF PT_LOAD + BSS）
//! - Stage2 (0x8000..0x9000)、页表 (0x10000..0x16000)、E820 buffer (0x20000..0x21000)
//!   都在 Reserved 区间，不会进入 bitmap
//!
//! **与 [`crate::memory_map`] 的契约**：初始化时消费 `MEMORY_MAP` 全部 Usable
//! 区间；调用 [`init_page_frame_allocator`] 前必须先 [`memory_map_init`]。
//!
//! **锁**：`SpinLock<PageFrameAllocator>` 保护；与 kstate 同语义（IRQ-safe）。

use log::info;

use crate::memory_map::{with_memory_map, MemoryType};
use crate::sync::SpinLock;

/// 单帧大小（4KB）。
pub const FRAME_SIZE: usize = 4096;
/// 最大管理帧数（32768 = 128MB）。
pub const MAX_FRAMES: usize = 1 << 15;
/// bitmap 数组大小（512 u64 = 4096 字节 = 1 页）。
const BITMAP_WORDS: usize = MAX_FRAMES / 64;

/// 物理地址类型（u64，4KB 对齐）。
pub type PhysicalAddr = u64;

/// 物理页帧分配器。
pub struct PageFrameAllocator {
    bitmap: [u64; BITMAP_WORDS],
    /// 已分配帧数（非空闲）。
    used: usize,
    /// 总可分配帧数（初始化后确定）。
    total: usize,
    /// 上次成功分配的帧号（hint 指针，减少扫描）。
    hint: usize,
}

impl PageFrameAllocator {
    /// 构造空分配器（全 0 bitmap = 全空闲；后续由 `mark_range_usable` 填充）。
    const fn new() -> Self {
        PageFrameAllocator {
            bitmap: [0; BITMAP_WORDS],
            used: 0,
            total: 0,
            hint: 0,
        }
    }

    /// 标记 [base, base+size) 区间的帧为可用（置 1）。
    /// 帧号按 4KB 对齐；不足一帧的前后缀被忽略（向上取整 start，向下取整 end）。
    fn mark_range_usable(&mut self, base: u64, size: u64) {
        let start_frame = ((base as usize) + FRAME_SIZE - 1) / FRAME_SIZE;
        let end_frame = (base as usize + size as usize) / FRAME_SIZE;
        for f in start_frame..end_frame {
            if f >= MAX_FRAMES {
                break;
            }
            let idx = f / 64;
            let bit = f % 64;
            if self.bitmap[idx] & (1 << bit) == 0 {
                self.bitmap[idx] |= 1 << bit;
                self.total += 1;
            }
        }
    }

    /// 标记 [base, base+size) 区间的帧为已用（清 0）。
    /// 用于保留内核自身占用的帧。
    fn mark_range_used(&mut self, base: u64, size: u64) {
        let start_frame = (base as usize) / FRAME_SIZE;
        let end_frame = (base as usize + size as usize + FRAME_SIZE - 1) / FRAME_SIZE;
        for f in start_frame..end_frame {
            if f >= MAX_FRAMES {
                break;
            }
            let idx = f / 64;
            let bit = f % 64;
            if self.bitmap[idx] & (1 << bit) != 0 {
                self.bitmap[idx] &= !(1 << bit);
                self.total -= 1;
            }
        }
    }

    /// 分配一个物理页帧（4KB 对齐）。
    ///
    /// 返回 `None` 表示内存耗尽。MVP 不支持 NUMA / 高端内存分区。
    pub fn alloc_frame(&mut self) -> Option<PhysicalAddr> {
        if self.used >= self.total {
            return None;
        }
        let n = BITMAP_WORDS;
        let start_word = self.hint / 64;
        for i in 0..n {
            let idx = (start_word + i) % n;
            let word = self.bitmap[idx];
            if word != 0 {
                let bit = word.trailing_zeros() as usize;
                let frame = idx * 64 + bit;
                self.bitmap[idx] &= !(1u64 << bit);
                self.used += 1;
                self.hint = frame;
                return Some((frame * FRAME_SIZE) as u64);
            }
        }
        None
    }

    /// 分配 `count` 个连续物理页帧（4KB 对齐）。
    ///
    /// 返回起始帧的物理地址。`None` 表示无法找到足够的连续帧。
    /// MVP 用途：内核堆需要连续物理内存。
    pub fn alloc_contiguous_frames(&mut self, count: usize) -> Option<PhysicalAddr> {
        if count == 0 {
            return None;
        }
        if self.used + count > self.total {
            return None;
        }

        // 扫描 bitmap 寻找 count 个连续的 1（空闲帧）
        let mut start_frame = 0;
        let mut consecutive = 0;

        for frame in 0..MAX_FRAMES {
            let idx = frame / 64;
            let bit = frame % 64;
            if self.bitmap[idx] & (1u64 << bit) != 0 {
                // 当前帧空闲
                if consecutive == 0 {
                    start_frame = frame;
                }
                consecutive += 1;
                if consecutive == count {
                    // 找到足够的连续帧，标记为已用
                    for i in 0..count {
                        let f = start_frame + i;
                        let w = f / 64;
                        let b = f % 64;
                        self.bitmap[w] &= !(1u64 << b);
                    }
                    self.used += count;
                    self.hint = start_frame + count;
                    return Some((start_frame * FRAME_SIZE) as u64);
                }
            } else {
                // 当前帧已用，重置计数
                consecutive = 0;
            }
        }
        None
    }

    /// 释放 `count` 个连续物理页帧（从 `addr` 开始）。
    ///
    /// 必须与 `alloc_contiguous_frames` 配对使用。
    pub fn free_contiguous_frames(&mut self, addr: PhysicalAddr, count: usize) {
        let start_frame = addr as usize / FRAME_SIZE;
        for i in 0..count {
            self.free_frame((start_frame + i) as PhysicalAddr);
        }
    }

    /// 释放一个物理页帧。
    ///
    /// **双重释放是 no-op**（幂等），不会 panic。
    /// 地址越界（>= `MAX_FRAMES * FRAME_SIZE`）也是 no-op。
    pub fn free_frame(&mut self, addr: PhysicalAddr) {
        let frame = addr as usize / FRAME_SIZE;
        if frame >= MAX_FRAMES {
            return;
        }
        let idx = frame / 64;
        let bit = frame % 64;
        if self.bitmap[idx] & (1u64 << bit) != 0 {
            // 已经是空闲（双重释放）→ no-op
            return;
        }
        // 正常释放
        self.bitmap[idx] |= 1u64 << bit;
        self.used = self.used.saturating_sub(1);
    }

    /// 当前总可分配帧数（初始化后确定）。
    pub fn total_frames(&self) -> usize {
        self.total
    }

    /// 当前已分配帧数。
    pub fn used_frames(&self) -> usize {
        self.used
    }

    /// 当前空闲帧数。
    pub fn free_frames(&self) -> usize {
        self.total.saturating_sub(self.used)
    }
}

/// 全局页帧分配器（IRQ-safe 锁保护）。
pub static PAGE_FRAMES: SpinLock<Option<PageFrameAllocator>> = SpinLock::new(None);

const UNINIT: &str = "PAGE_FRAMES accessed before init_page_frame_allocator()";

// 链接器符号：内核结束地址（`linker.ld` 末尾 `PROVIDE(_kernel_end = .)`）。
extern "C" {
    static _kernel_end: u8;
}

/// 初始化全局页帧分配器。
///
/// 从 `MEMORY_MAP` 读取 Usable 区间填充 bitmap，再保留内核自身占用的帧
/// （0x200000..`_kernel_end`）。重复调用 panic。
pub fn init_page_frame_allocator() {
    let mut g = PAGE_FRAMES.lock();
    if g.is_some() {
        panic!("init_page_frame_allocator called twice");
    }

    let mut alloc = PageFrameAllocator::new();

    // 步骤 1: 从 MEMORY_MAP 填充 Usable 区间
    with_memory_map(|map| {
        for entry in map.iter() {
            if entry.mem_type == MemoryType::Usable {
                alloc.mark_range_usable(entry.base, entry.size);
            }
        }
    });

    // 步骤 2: 保留内核自身（0x200000.._kernel_end）
    // SAFETY: `_kernel_end` 由 linker.ld 提供，必在 0x200000+ 且 > _start64。
    let kernel_end = unsafe { &_kernel_end as *const u8 as u64 };
    let kernel_base: u64 = 0x200000;
    if kernel_end > kernel_base {
        alloc.mark_range_used(kernel_base, kernel_end - kernel_base);
    }

    info!(
        "[page_frame] initialized: total={} free={} ({} KB); kernel reserved [{:#x}..{:#x}]",
        alloc.total_frames(),
        alloc.free_frames(),
        alloc.free_frames() * FRAME_SIZE / 1024,
        kernel_base,
        kernel_end,
    );

    *g = Some(alloc);
}

/// 对 [`PageFrameAllocator`] 的临界区访问（未初始化 panic）。
pub fn with_page_frames<R>(f: impl FnOnce(&mut PageFrameAllocator) -> R) -> R {
    let mut g = PAGE_FRAMES.lock();
    f(g.as_mut().expect(UNINIT))
}

/// 分配一个物理页帧（`with_page_frames` 的快捷包装）。
pub fn alloc_frame() -> Option<PhysicalAddr> {
    with_page_frames(|a| a.alloc_frame())
}

/// 释放一个物理页帧（`with_page_frames` 的快捷包装）。
pub fn free_frame(addr: PhysicalAddr) {
    with_page_frames(|a| a.free_frame(addr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_allocator_alloc_returns_none() {
        let mut a = PageFrameAllocator::new();
        assert!(a.alloc_frame().is_none());
        assert_eq!(a.total_frames(), 0);
    }

    #[test]
    fn mark_usable_then_alloc() {
        let mut a = PageFrameAllocator::new();
        // 标记 0..64KB 为可用 = 16 帧
        a.mark_range_usable(0, 64 * 1024);
        assert_eq!(a.total_frames(), 16);

        let first = a.alloc_frame().unwrap();
        assert_eq!(first, 0);
        assert_eq!(a.used_frames(), 1);
        assert_eq!(a.free_frames(), 15);

        let second = a.alloc_frame().unwrap();
        assert_eq!(second, FRAME_SIZE as u64);
    }

    #[test]
    fn free_then_realloc() {
        let mut a = PageFrameAllocator::new();
        a.mark_range_usable(0, 16 * 1024); // 4 帧
        let f1 = a.alloc_frame().unwrap();
        let f2 = a.alloc_frame().unwrap();
        assert_eq!(a.free_frames(), 2);

        a.free_frame(f1);
        assert_eq!(a.free_frames(), 3);

        // 重分配应拿到刚释放的帧（或下一个空闲）
        let f3 = a.alloc_frame().unwrap();
        assert_eq!(a.free_frames(), 2);
        let _ = f2;
        let _ = f3;
    }

    #[test]
    fn mark_used_reduces_total() {
        let mut a = PageFrameAllocator::new();
        a.mark_range_usable(0, 64 * 1024); // 16 帧
        assert_eq!(a.total_frames(), 16);

        a.mark_range_used(0, 16 * 1024); // 保留前 4 帧
        assert_eq!(a.total_frames(), 12);
        assert_eq!(a.free_frames(), 12);

        // 第 4 帧起应可分配
        let f = a.alloc_frame().unwrap();
        assert_eq!(f, 4 * FRAME_SIZE as u64);
    }

    #[test]
    fn double_free_is_noop() {
        let mut a = PageFrameAllocator::new();
        a.mark_range_usable(0, 16 * 1024);
        let f = a.alloc_frame().unwrap();
        a.free_frame(f);
        assert_eq!(a.free_frames(), 4);
        a.free_frame(f); // 双重释放
        assert_eq!(a.free_frames(), 4); // 不变
    }

    #[test]
    fn mark_range_with_unaligned_base() {
        let mut a = PageFrameAllocator::new();
        // base=0x1000 (4KB), size=0x3000 (12KB) → 帧 1,2,3（帧 0 在 base 之前）
        a.mark_range_usable(0x1000, 0x3000);
        assert_eq!(a.total_frames(), 3);

        let f1 = a.alloc_frame().unwrap();
        assert_eq!(f1, 0x1000);
    }

    #[test]
    fn alloc_exhaustion_returns_none() {
        let mut a = PageFrameAllocator::new();
        a.mark_range_usable(0, 2 * FRAME_SIZE as u64); // 仅 2 帧
        assert!(a.alloc_frame().is_some());
        assert!(a.alloc_frame().is_some());
        assert!(a.alloc_frame().is_none());
    }
}
