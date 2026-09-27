//! # synapse-vma — Synapse 用户内存区域表（VMA-like）纯逻辑（P4-T3）
//!
//! 对齐设计：[Doc 02 §3.3](../docs/design/02-userspace-abi-and-process-model.md)——
//! 按需分页需要一张"哪些区间合法、权限如何"的表，缺页命中合法区间 → 分配页帧并映射；
//! 未命中 → SIGSEGV 等价物（杀进程，T3 提供骨架）。
//!
//! ## 模块划分
//!
//! | 项 | 职责 |
//! |----|------|
//! | [`PAGE_SIZE`] | 4KB 页粒度常量（VMA 边界必须页对齐） |
//! | [`NULL_GUARD_END`] | NULL 页守卫上界：[0, 0x1000) 永不被映射（Doc 02 §3.1） |
//! | [`MAX_REGIONS`] | 单进程 VMA 数量上限（固定数组容量） |
//! | [`RegionFlags`] | `R/W/X/GROWABLE` 位组合（手动 bitflags，不引入依赖） |
//! | [`RegionKind`] | `Code`/`Data`/`Stack`/`Heap`/`Mapped` 分类 |
//! | [`UserMemoryRegion`] | 单条 VMA 记录：起止 + flags + kind；自带 `validate/contains/overlaps` |
//! | [`VmaError`] | `Unaligned/Empty/NullGuard/Overlap/Full/NotFound/NotGrowable/Shrink` |
//! | [`RegionTable`] | 固定容量表：`insert/lookup/remove/grow/clear/iter/len` |
//!
//! ## 工程约束（与 cap/ipc/proc/sched 同一纪律）
//!
//! - `no_std` + 零依赖 + 固定数组（不使用 alloc），宿主 `cargo test` 全量可测。
//! - 禁止 `unsafe`（crate 级 deny）；锁语义（SpinLock + 关中断）由内核集成层负责：
//!   缺页路径在 [`crate::paging`（内核）](https://docs.rs/synapse-kernel) 中
//!   包成 IRQ-safe 临界区，本 crate 只提供纯决策逻辑。
//!
//! ## 与内核分页的契约
//!
//! - VMA 边界必须 4KB 对齐 → 与 paging.rs `FRAME_SIZE` 一致；
//! - VMA 起点不得小于 `NULL_GUARD_END`（0x1000）→ NULL 守卫（Doc 02 §3.1）；
//! - 用户地址窗口（[1GB, 2GB) = `USER_REGION_START..USER_REGION_END`）由
//!   内核 paging 模块强制，本 crate 不耦合该常量。

#![no_std]
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod table;

pub use table::{RegionTable, VmaError};

/// 单页大小（4KB）。VMA 边界必须按此对齐。
pub const PAGE_SIZE: u64 = 4096;

/// NULL 页守卫上界（不含）：[0, `NULL_GUARD_END`) 永不被映射（Doc 02 §3.1）。
/// VMA 起点 < `NULL_GUARD_END` 即被 [`VmaError::NullGuard`] 拒绝。
pub const NULL_GUARD_END: u64 = 0x1000;

/// 单进程 VMA 数量上限（固定数组容量）。MVP：32 条——超出即
/// [`VmaError::Full`]。Phase 5+ 视需求换 BTreeMap / 段树。
pub const MAX_REGIONS: usize = 32;

// ===========================================================================
// RegionFlags
// ===========================================================================

/// VMA 权限与可增长标志位组合。
///
/// 位分配：
/// - bit0 = READ（用户可读）
/// - bit1 = WRITE（用户可写）
/// - bit2 = EXEC（用户可执行）
/// - bit3 = GROWABLE（堆/栈专用：允许通过 [`RegionTable::grow`] 延展边界）
///
/// [`RegionFlags::NONE`] 表示 PROT_NONE 守卫区（存在但不映射物理页）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RegionFlags(u8);

impl RegionFlags {
    /// 无任何标志（PROT_NONE 守卫）。
    pub const NONE: Self = Self(0);
    /// 仅可读。
    pub const READ: Self = Self(1 << 0);
    /// 仅可写（实际很少单独存在；通常配合 READ）。
    pub const WRITE: Self = Self(1 << 1);
    /// 仅可执行。
    pub const EXEC: Self = Self(1 << 2);
    /// 可增长（堆/栈）。
    pub const GROWABLE: Self = Self(1 << 3);

    /// 常用组合：可读 + 可写。
    pub const RW: Self = Self(Self::READ.0 | Self::WRITE.0);
    /// 常用组合：可读 + 可执行（代码段典型）。
    pub const RX: Self = Self(Self::READ.0 | Self::EXEC.0);

    /// 取底层位表示。
    pub const fn bits(self) -> u8 {
        self.0
    }
    /// 位并集：`a.union(b)` 表示"同时具备两者的所有标志"。
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
    /// 位子集判定：`a.contains(b)` 为 true 当且仅当 b 的每一位 a 都有
    ///（`NONE` 永远满足——用于"判断权限是否被完全满足"）。
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

impl core::ops::BitOr for RegionFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

// ===========================================================================
// RegionKind
// ===========================================================================

/// VMA 用途分类。
///
/// - `Code`：代码段（典型 RX）
/// - `Data`：数据段（典型 RW，非向上增长）
/// - `Stack`：栈（典型 RW，GROWABLE，向下增长）
/// - `Heap`：堆（典型 RW，GROWABLE，向上增长）
/// - `Mapped`：设备/文件映射（特殊用途，T7+ 接管）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RegionKind {
    /// 代码段。
    Code,
    /// 数据段。
    Data,
    /// 栈（GROWABLE，向下）。
    Stack,
    /// 堆（GROWABLE，向上）。
    Heap,
    /// 设备/文件映射（T7+ 接管）。
    Mapped,
}

// ===========================================================================
// UserMemoryRegion
// ===========================================================================

/// 单条用户内存区域记录：起止（页对齐，左闭右开）+ 权限 + 用途。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UserMemoryRegion {
    /// 起始虚拟地址（页对齐，含）。
    pub start: u64,
    /// 终止虚拟地址（页对齐，不含）。
    pub end: u64,
    /// 权限 + GROWABLE 标志。
    pub flags: RegionFlags,
    /// 用途分类。
    pub kind: RegionKind,
}

impl UserMemoryRegion {
    /// 新建（不做合法性校验，调用方负责传入合法值或经 [`Self::validate`]）。
    pub const fn new(start: u64, end: u64, flags: RegionFlags, kind: RegionKind) -> Self {
        Self { start, end, flags, kind }
    }

    /// 地址 `addr` 是否落在本区域（半开区间 `[start, end)`）。
    pub const fn contains(&self, addr: u64) -> bool {
        self.start <= addr && addr < self.end
    }

    /// 与另一区域是否存在区间交集（**端点相邻不算重叠**——end 是开区间）。
    pub const fn overlaps(&self, other: &Self) -> bool {
        self.start < other.end && other.start < self.end
    }

    /// 结构合法性校验：页对齐 + 非空 + 不穿过 NULL 守卫。
    ///
    /// 注意：**不**做"与既有 VMA 重叠"判定——那是 [`RegionTable::insert`] 的职责
    ///（纯逻辑与表操作分层：本方法只关心"自身是否合规"）。
    pub const fn validate(&self) -> Result<(), VmaError> {
        if self.start % PAGE_SIZE != 0 || self.end % PAGE_SIZE != 0 {
            return Err(VmaError::Unaligned);
        }
        if self.start >= self.end {
            return Err(VmaError::Empty);
        }
        if self.start < NULL_GUARD_END {
            return Err(VmaError::NullGuard);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_union_and_contains() {
        assert!(RegionFlags::RW.contains(RegionFlags::READ));
        assert!(RegionFlags::RW.contains(RegionFlags::WRITE));
        assert!(!RegionFlags::READ.contains(RegionFlags::WRITE));
        assert!(RegionFlags::NONE.contains(RegionFlags::NONE));
        assert!(!RegionFlags::NONE.contains(RegionFlags::READ));
        assert_eq!(RegionFlags::READ | RegionFlags::EXEC, RegionFlags::RX);
        assert_eq!(RegionFlags::RX.union(RegionFlags::GROWABLE).bits(), 0b1101);
    }

    #[test]
    fn region_contains_and_overlaps() {
        let r = UserMemoryRegion::new(0x4000_0000, 0x4000_8000, RegionFlags::RX, RegionKind::Code);
        assert!(r.contains(0x4000_0000));
        assert!(r.contains(0x4000_7FFF));
        assert!(!r.contains(0x4000_8000)); // 上界开区间
        assert!(!r.contains(0x3FFF_FFFF));
    }

    #[test]
    fn region_overlap_disjoint_and_adjacent() {
        let a = UserMemoryRegion::new(0x4000_0000, 0x4000_8000, RegionFlags::RX, RegionKind::Code);
        let b = UserMemoryRegion::new(0x4000_8000, 0x4000_A000, RegionFlags::RW, RegionKind::Data);
        let c = UserMemoryRegion::new(0x4000_7000, 0x4000_9000, RegionFlags::RW, RegionKind::Data);
        assert!(!a.overlaps(&b)); // 端点相邻 → 无交集
        assert!(a.overlaps(&c));
        assert!(b.overlaps(&c));
        // 与自身重叠
        assert!(a.overlaps(&a));
    }

    #[test]
    fn validate_unaligned() {
        let r = UserMemoryRegion::new(0x4000_0001, 0x4000_8000, RegionFlags::RX, RegionKind::Code);
        assert_eq!(r.validate(), Err(VmaError::Unaligned));
        let r = UserMemoryRegion::new(0x4000_0000, 0x4000_8001, RegionFlags::RX, RegionKind::Code);
        assert_eq!(r.validate(), Err(VmaError::Unaligned));
    }

    #[test]
    fn validate_empty() {
        let r = UserMemoryRegion::new(0x4000_8000, 0x4000_8000, RegionFlags::RX, RegionKind::Code);
        assert_eq!(r.validate(), Err(VmaError::Empty));
        // start > end 同样视为空
        let r = UserMemoryRegion::new(0x4000_8000, 0x4000_4000, RegionFlags::RX, RegionKind::Code);
        assert_eq!(r.validate(), Err(VmaError::Empty));
    }

    #[test]
    fn validate_null_guard() {
        // 完全在 NULL 守卫内
        let r = UserMemoryRegion::new(0, 0x1000, RegionFlags::RW, RegionKind::Data);
        assert_eq!(r.validate(), Err(VmaError::NullGuard));
        // 起点穿入 NULL 守卫
        let r = UserMemoryRegion::new(0, 0x2000, RegionFlags::RW, RegionKind::Data);
        assert_eq!(r.validate(), Err(VmaError::NullGuard));
        // 起点刚好等于 NULL_GUARD_END → 合规
        let r = UserMemoryRegion::new(NULL_GUARD_END, NULL_GUARD_END + PAGE_SIZE, RegionFlags::RW, RegionKind::Data);
        assert_eq!(r.validate(), Ok(()));
    }
}
