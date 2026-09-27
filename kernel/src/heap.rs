//! 内核堆分配器（基于物理页帧分配器）。
//!
//! ## 设计
//!
//! **First-fit 链表分配器**：维护空闲块链表，每个块头部记录大小。
//! 分配时从头遍历找到第一个足够大的块，拆分后返回。
//! 释放时将块插入链表（按地址排序，便于后续合并相邻块——MVP 暂不合并）。
//!
//! **数据结构**：
//! ```text
//! ┌─────────────┬─────────────┬───────────────────────────┐
//! │ size: usize │ next: *mut  │ payload (size - 16 bytes) │
//! │ (8 bytes)   │ Block (8B)  │ (aligned to 16 bytes)     │
//! └─────────────┴─────────────┴───────────────────────────┘
//! ```
//!
//! **容量**：初始从 `PAGE_FRAMES` 分配 1MB（256 页）作为堆池。
//! 用尽时尝试从 `PAGE_FRAMES` 再申请更多页（MVP 仅启动时一次性分配）。
//!
//! **锁**：`SpinLock<Heap>` 保护（IRQ-safe，与 kstate 同源）。
//!
//! **与 GlobalAlloc 接线**：实现 `#[global_allocator]` 使内核可使用 `Box`/`Vec`。

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;

use crate::page_frame;
use crate::sync::SpinLock;

/// 堆块头部（16 字节）。
#[repr(C)]
struct Block {
    /// 块大小（含头部 16 字节；payload = size - 16）。
    size: usize,
    /// 下一个空闲块指针（链表）。
    next: *mut Block,
}

/// 最小分配粒度（16 字节对齐）。
const MIN_ALIGN: usize = 16;
/// 最小块大小（头部 16 + payload 至少 16 = 32 字节）。
const MIN_BLOCK_SIZE: usize = 32;

/// 堆分配器状态。
pub struct Heap {
    /// 空闲块链表头。
    free_list: *mut Block,
    /// 堆池起始地址。
    pool_start: usize,
    /// 堆池结束地址。
    pool_end: usize,
    /// 已分配字节数（统计）。
    allocated: usize,
}

impl Heap {
    /// 构造空堆（后续由 `init` 初始化）。
    const fn new() -> Self {
        Heap {
            free_list: null_mut(),
            pool_start: 0,
            pool_end: 0,
            allocated: 0,
        }
    }

    /// 初始化堆：从 `PAGE_FRAMES` 分配 `pool_pages` 页作为堆池。
    ///
    /// 将整个池视为一个大的空闲块插入链表。
    fn init(&mut self, pool_pages: usize) {
        let pool_size = pool_pages * page_frame::FRAME_SIZE;

        // 分配连续帧
        let pool_start = page_frame::with_page_frames(|a| {
            a.alloc_contiguous_frames(pool_pages)
                .expect("heap init: out of contiguous physical frames")
        }) as usize;
        let pool_end = pool_start + pool_size;

        // 将整个池初始化为一个空闲块
        let block = pool_start as *mut Block;
        unsafe {
            (*block).size = pool_size;
            (*block).next = null_mut();
        }

        self.free_list = block;
        self.pool_start = pool_start;
        self.pool_end = pool_end;
        self.allocated = 0;

        log::info!(
            "[heap] initialized: pool=[{:#x}..{:#x}) ({} KB, {} pages)",
            pool_start, pool_end, pool_size / 1024, pool_pages
        );
    }

    /// 分配 `layout.size` 字节（`layout.align` 必须 ≤ `MIN_ALIGN`）。
    fn alloc(&mut self, layout: Layout) -> *mut u8 {
        let size = (layout.size() + MIN_ALIGN - 1) & !(MIN_ALIGN - 1); // 向上对齐
        let total = size + core::mem::size_of::<Block>(); // 含头部

        let mut prev: *mut Block = null_mut();
        let mut curr = self.free_list;

        while !curr.is_null() {
            let block = unsafe { &mut *curr };
            if block.size >= total {
                // 找到足够大的块
                let remainder = block.size - total;
                if remainder >= MIN_BLOCK_SIZE {
                    // 拆分：剩余部分作为新空闲块；**收缩已分配块到 `total`**
                    // （否则 free 时读到的 size 仍是原始值，会与已分配块之后的
                    // 新空闲块重叠，导致二次分配或 allocated 计数异常）
                    let next_block = (curr as usize + total) as *mut Block;
                    unsafe {
                        (*curr).size = total;          // ← 关键：收缩到实际分配大小
                        (*next_block).size = remainder;
                        (*next_block).next = block.next;
                    }
                    if prev.is_null() {
                        self.free_list = next_block;
                    } else {
                        unsafe { (*prev).next = next_block; }
                    }
                } else {
                    // 不拆分：整个块分配
                    if prev.is_null() {
                        self.free_list = block.next;
                    } else {
                        unsafe { (*prev).next = block.next; }
                    }
                }
                self.allocated += unsafe { (*curr).size };
                // 返回 payload 地址（跳过头部）
                return (curr as usize + core::mem::size_of::<Block>()) as *mut u8;
            }
            prev = curr;
            curr = unsafe { (*curr).next };
        }

        // 堆耗尽
        null_mut()
    }

    /// 释放 `ptr` 指向的块（`ptr` 必须是由 `alloc` 返回的地址）。
    fn free(&mut self, ptr: *mut u8) {
        if ptr.is_null() {
            return;
        }
        let block = (ptr as usize - core::mem::size_of::<Block>()) as *mut Block;
        // MVP: 不跟踪 allocated 计数器（Vec 重分配等场景下计数易出错）。

        // 插入链表（按地址排序，便于后续合并——MVP 暂不合并）
        let mut prev: *mut Block = null_mut();
        let mut curr = self.free_list;

        while !curr.is_null() && (curr as usize) < (block as usize) {
            prev = curr;
            curr = unsafe { (*curr).next };
        }

        unsafe {
            (*block).next = curr;
        }
        if prev.is_null() {
            self.free_list = block;
        } else {
            unsafe { (*prev).next = block; }
        }

        // TODO: 合并相邻块（MVP 暂不实现）
    }
}

// SAFETY: Heap 内部指针仅在持锁时访问，跨线程传递安全。
unsafe impl Send for Heap {}

/// 全局堆分配器（IRQ-safe 锁保护）。
static HEAP: SpinLock<Heap> = SpinLock::new(Heap::new());

/// 初始化内核堆（boot 时调用一次）。
///
/// 从 `PAGE_FRAMES` 分配 `pool_pages` 页（默认 256 页 = 1MB）作为堆池。
pub fn init_heap(pool_pages: usize) {
    let mut g = HEAP.lock();
    if g.pool_start != 0 {
        panic!("init_heap called twice");
    }
    g.init(pool_pages);
}

/// 堆池占用页数（FR8 页账本 boot 补记用；未初始化 = 0）。
pub fn pool_pages() -> usize {
    let g = HEAP.lock();
    (g.pool_end - g.pool_start) / page_frame::FRAME_SIZE
}

/// 内核全局分配器（实现 `GlobalAlloc` trait）。
pub struct KernelAllocator;

unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut g = HEAP.lock();
        g.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let mut g = HEAP.lock();
        g.free(ptr);
        let _ = layout; // MVP 暂不使用 layout.size
    }
}

/// 全局分配器实例（`#[global_allocator]`）。
#[global_allocator]
pub static ALLOCATOR: KernelAllocator = KernelAllocator;

#[cfg(test)]
mod tests {
    use super::*;

    // 宿主测试需要模拟 PAGE_FRAMES，暂用桩实现。
    // 真机测试在 smoke.rs 中验证。
}
