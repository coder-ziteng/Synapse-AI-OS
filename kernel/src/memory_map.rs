//! 物理内存映射表（BIOS INT 15h AX=E820h）。
//!
//! ## 数据来源
//!
//! `boot.S` Step 4.5 在 32-bit 保护模式（分页未启用）下循环调用 `INT 15h`
//! 把 E820 entries 写入固定物理地址 `E820_BUFFER_ADDR = 0x20004`，条目数写入
//! `[E820_COUNT_ADDR] = 0x20000`。`_start64` 入口由集成层调用
//! [`memory_map_init`] 解析 buffer → 全局 [`MEMORY_MAP`]。
//!
//! ## MVP 约束（与 kstate 同源）
//!
//! 解析为静态 `Option<MemoryMap>`；容量 256 条（远超典型 QEMU/物理机 E820
//! 数十条上限）。P2 页帧分配器（T2）落地后可直接消费 `MEMORY_MAP`，无需重解析。
//!
//! ## 锁语义
//!
//! 全局表由 `SpinLock<Option<MemoryMap>>` 保护（与 [`crate::kstate`] 同源）；
//! 解析只发生一次（`memory_map_init`），之后仅读路径访问。

use log::info;

use crate::sync::SpinLock;

/// E820 entry buffer 起始物理地址（`boot.S` 写入）。
pub const E820_BUFFER_ADDR: usize = 0x20004;
/// E820 entry count 物理地址（4 字节；boot.S 在解析完成后回填）。
pub const E820_COUNT_ADDR: usize = 0x20000;
/// 单条 E820 entry 字节数（v1：base u64 + size u64 + type u32）。
pub const E820_ENTRY_SIZE: usize = 24;
/// 容量上限（典型 E820 < 64 条；256 留余量）。
pub const MAX_E820_ENTRIES: usize = 256;

/// BIOS E820 memory type（spec + EDNS-7 扩展）。
///
/// 仅取与内核分配/回收相关的几个常用类型；其他 `>= 0x10000` 的 vendor-defined
/// 类型按 Reserved 处理（不参与可用内存统计）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum MemoryType {
    /// 可用物理内存（page allocator 可消费）。
    Usable = 1,
    /// 保留（BIOS / MMIO / 坏块等，不可分配）。
    Reserved = 2,
    /// ACPI reclaim（休眠后回收；MVP 当作 Reserved 不分配，避免误踩）。
    AcpiReclaim = 3,
    /// ACPI NVS（永不回收；Reserved）。
    AcpiNvs = 4,
    /// 坏内存。
    BadMemory = 5,
    /// vendor-defined / 未知类型（Reserved）。
    Unknown = 0xFFFF_FFFF,
}

impl MemoryType {
    /// 从 BIOS 返回的 u32 安全转换（未知值 → [`MemoryType::Unknown`]）。
    pub fn from_u32(v: u32) -> MemoryType {
        match v {
            1 => MemoryType::Usable,
            2 => MemoryType::Reserved,
            3 => MemoryType::AcpiReclaim,
            4 => MemoryType::AcpiNvs,
            5 => MemoryType::BadMemory,
            _ => MemoryType::Unknown,
        }
    }

    /// 是否可作为页帧分配器的候选（`Usable`）。
    pub fn is_usable(self) -> bool {
        matches!(self, MemoryType::Usable)
    }
}

/// 单条 BIOS E820 entry（24 字节，`repr(C)` 对齐 boot.S 写入布局）。
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct E820Entry {
    /// 物理基址（8 字节对齐最佳）。
    pub base: u64,
    /// 长度（字节）。
    pub size: u64,
    /// 内存类型。
    pub mem_type: MemoryType,
}

/// 解析后的内存映射（条目固定数组 + 汇总）。
pub struct MemoryMap {
    entries: [Option<E820Entry>; MAX_E820_ENTRIES],
    count: usize,
    /// 所有 Usable 区间总字节数（解析时一次性算出，热路径 O(1)）。
    total_usable: u64,
}

impl MemoryMap {
    /// 条目数。
    pub fn len(&self) -> usize {
        self.count
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// 迭代所有条目（管理 / 打印路径）。
    pub fn iter(&self) -> impl Iterator<Item = &E820Entry> {
        self.entries[..self.count].iter().filter_map(|e| e.as_ref())
    }

    /// 所有 Usable 区间总字节数。
    pub fn total_usable(&self) -> u64 {
        self.total_usable
    }

    /// 构造空表（host 测试构造）。
    pub fn empty() -> MemoryMap {
        MemoryMap {
            entries: [const { None }; MAX_E820_ENTRIES],
            count: 0,
            total_usable: 0,
        }
    }

    /// 插入一条 entry（host 测试 / 解析共用；返回是否成功）。
    pub fn push(&mut self, entry: E820Entry) -> Result<(), ()> {
        if self.count >= MAX_E820_ENTRIES {
            return Err(());
        }
        if entry.mem_type.is_usable() {
            self.total_usable = self.total_usable.saturating_add(entry.size);
        }
        self.entries[self.count] = Some(entry);
        self.count += 1;
        Ok(())
    }
}

/// 全局内存映射表（IRQ-safe 锁保护）。
pub static MEMORY_MAP: SpinLock<Option<MemoryMap>> = SpinLock::new(None);

const UNINIT: &str = "memory_map accessed before memory_map_init()";

/// 初始化全局内存映射表。重复调用 panic（与 `kstate_init` 同语义）。
///
/// 读取 `boot.S` 写入的 buffer：
/// * `[E820_COUNT_ADDR]`（u32）= 条目数；
/// * `[E820_BUFFER_ADDR + i*24]`（24 字节）= 第 i 条 entry。
pub fn memory_map_init() {
    let mut g = MEMORY_MAP.lock();
    if g.is_some() {
        panic!("memory_map_init called twice");
    }
    // SAFETY: E820_COUNT_ADDR / E820_BUFFER_ADDR 是 boot.S 与本模块的固定约定；
    // 在 _start64 入口阶段由 boot.S 写完，本函数首次调用必读到合法值。
    let raw_count =
        unsafe { (E820_COUNT_ADDR as *const u32).read_unaligned() };
    let count = (raw_count as usize).min(MAX_E820_ENTRIES);

    let mut map = MemoryMap::empty();
    for i in 0..count {
        // SAFETY: 0 ≤ i < count ≤ MAX_E820_ENTRIES；buffer 已被 boot.S 写入 24 字节 entry。
        // 用 read_unaligned 因为 E820_BUFFER_ADDR = 0x20004 仅 4 字节对齐, 而 E820Entry
        // (含 u64 字段) 需要 8 字节对齐, read_volatile 会触发 UB check panic。
        let raw = unsafe {
            ((E820_BUFFER_ADDR + i * E820_ENTRY_SIZE) as *const E820Entry).read_unaligned()
        };
        let entry = E820Entry {
            base: raw.base,
            size: raw.size,
            mem_type: MemoryType::from_u32(raw.mem_type as u32),
        };
        if map.push(entry).is_err() {
            break;
        }
    }
    info!(
        "[memory_map] parsed {} entries (raw count {}); usable = {:#x} bytes",
        map.len(),
        raw_count,
        map.total_usable(),
    );
    for (i, e) in map.iter().enumerate() {
        info!(
            "[memory_map]   [{:02}] base={:#018x} size={:#014x} type={:?}",
            i, e.base, e.size, e.mem_type,
        );
    }
    *g = Some(map);
}

/// 对 [`MemoryMap`] 的临界区只读访问（未初始化 panic）。
pub fn with_memory_map<R>(f: impl FnOnce(&MemoryMap) -> R) -> R {
    let mut g = MEMORY_MAP.lock();
    f(g.as_mut().expect(UNINIT))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_map() {
        let m = MemoryMap::empty();
        assert_eq!(m.len(), 0);
        assert_eq!(m.total_usable(), 0);
        assert!(m.is_empty());
    }

    #[test]
    fn push_usable_increments_total() {
        let mut m = MemoryMap::empty();
        m.push(E820Entry {
            base: 0x1000,
            size: 0x9000,
            mem_type: MemoryType::Usable,
        })
        .unwrap();
        m.push(E820Entry {
            base: 0x20000,
            size: 0x10000,
            mem_type: MemoryType::Usable,
        })
        .unwrap();
        assert_eq!(m.total_usable(), 0x19000);
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn push_reserved_does_not_increment_total() {
        let mut m = MemoryMap::empty();
        m.push(E820Entry {
            base: 0,
            size: 0x1000,
            mem_type: MemoryType::Reserved,
        })
        .unwrap();
        assert_eq!(m.total_usable(), 0);
    }

    #[test]
    fn from_u32_mapping() {
        assert_eq!(MemoryType::from_u32(1), MemoryType::Usable);
        assert_eq!(MemoryType::from_u32(2), MemoryType::Reserved);
        assert_eq!(MemoryType::from_u32(0xDEAD), MemoryType::Unknown);
    }

    #[test]
    fn push_overflow_returns_err() {
        let mut m = MemoryMap::empty();
        for i in 0..MAX_E820_ENTRIES {
            assert!(m
                .push(E820Entry {
                    base: i as u64 * 0x1000,
                    size: 0x1000,
                    mem_type: MemoryType::Usable,
                })
                .is_ok());
        }
        assert!(m
            .push(E820Entry {
                base: 0,
                size: 0,
                mem_type: MemoryType::Usable,
            })
            .is_err());
    }
}