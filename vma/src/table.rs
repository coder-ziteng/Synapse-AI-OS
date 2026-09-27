//! [`RegionTable`] 实现：固定容量 [`MAX_REGIONS`] 槽位的 VMA 表。
//!
//! 提供 insert / lookup / remove / grow / clear / iter / len。
//! 所有结构性校验（对齐、非空、NULL 守卫、重叠）由本模块负责；
//! 调用 [`UserMemoryRegion::validate`] + 槽位扫描即可。

use crate::{UserMemoryRegion, MAX_REGIONS};

/// VMA 操作错误。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VmaError {
    /// 起点或终点未按 [`PAGE_SIZE`] 对齐。
    Unaligned,
    /// 区间为空（`start >= end`）。
    Empty,
    /// 起点落在 [0, [`NULL_GUARD_END`]) NULL 守卫区（Doc 02 §3.1）。
    NullGuard,
    /// 与既有 VMA 区间重叠（端点相邻不算）。
    Overlap,
    /// 表已满（[`MAX_REGIONS`] 槽位用尽）。
    Full,
    /// `remove`/`grow` 时未找到指定起点。
    NotFound,
    /// `grow` 时该 VMA 未设置 [`crate::RegionFlags::GROWABLE`]。
    NotGrowable,
    /// `grow` 时新区间未延展（仅单调扩大；不允许缩小或位移）。
    Shrink,
}

/// 固定容量 VMA 表。
///
/// 设计选择：固定数组 + `len` 计数器——零分配、宿主可测；
/// Phase 5+ 如进程密度上量可换 BTreeMap 或区间树。
#[derive(Clone)]
pub struct RegionTable {
    slots: [Option<UserMemoryRegion>; MAX_REGIONS],
    len: usize,
}

impl RegionTable {
    /// 常量构造（供 `static` 初始化）。
    pub const fn new() -> Self {
        Self {
            slots: [None; MAX_REGIONS],
            len: 0,
        }
    }

    /// 当前 VMA 条目数。
    pub fn len(&self) -> usize {
        self.len
    }

    /// 表是否为空。
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 插入一条 VMA。先做结构校验，再线性扫描防重叠，最后填入第一个空槽。
    ///
    /// 返回插入位置的槽索引；失败返回对应 [`VmaError`]。
    pub fn insert(&mut self, region: UserMemoryRegion) -> Result<usize, VmaError> {
        region.validate()?;
        // 重叠检查（线性扫描：MVP 32 槽足够）
        for s in self.slots.iter().flatten() {
            if s.overlaps(&region) {
                return Err(VmaError::Overlap);
            }
        }
        // 找空槽
        let idx = self
            .slots
            .iter()
            .position(Option::is_none)
            .ok_or(VmaError::Full)?;
        self.slots[idx] = Some(region);
        self.len += 1;
        Ok(idx)
    }

    /// 按地址二分式线性查找包含 `addr` 的 VMA（`[start, end)` 半开区间）。
    pub fn lookup(&self, addr: u64) -> Option<&UserMemoryRegion> {
        self.slots
            .iter()
            .flatten()
            .find(|r| r.contains(addr))
    }

    /// 按起点地址删除一条 VMA（精确匹配 `start`）。
    pub fn remove(&mut self, start: u64) -> Option<UserMemoryRegion> {
        let idx = self
            .slots
            .iter()
            .position(|s| matches!(s, Some(r) if r.start == start))?;
        let r = self.slots[idx].take();
        self.len -= 1;
        r
    }

    /// 延展一条 GROWABLE VMA 的边界。
    ///
    /// 规则：
    /// - 必须找到起点为 `start` 的 VMA；否则 [`VmaError::NotFound`]；
    /// - 该 VMA 必须设置 [`crate::RegionFlags::GROWABLE`]；否则 [`VmaError::NotGrowable`]；
    /// - 新边界必须**严格单调延展**（`new_start <= old.start && new_end >= old.end`），
    ///   缩小或位移 → [`VmaError::Shrink`]；
    /// - 新区间与既有其他 VMA（除自身外）不能重叠 → [`VmaError::Overlap`]；
    /// - 新边界须满足 [`UserMemoryRegion::validate`]（对齐、非空、不穿 NULL 守卫）。
    pub fn grow(
        &mut self,
        start: u64,
        new_start: u64,
        new_end: u64,
    ) -> Result<(), VmaError> {
        let idx = self
            .slots
            .iter()
            .position(|s| matches!(s, Some(r) if r.start == start))
            .ok_or(VmaError::NotFound)?;
        let old = self.slots[idx].expect("idx points to Some");
        if !old.flags.contains(crate::RegionFlags::GROWABLE) {
            return Err(VmaError::NotGrowable);
        }
        if new_start > old.start || new_end < old.end {
            return Err(VmaError::Shrink);
        }
        // 构造候选 VMA 以复用结构校验（flags/kind 继承自旧记录）
        let candidate = UserMemoryRegion::new(new_start, new_end, old.flags, old.kind);
        candidate.validate()?;
        // 与其他槽位重叠检查（跳过自身 idx）
        for (i, s) in self.slots.iter().enumerate() {
            if i == idx {
                continue;
            }
            if let Some(other) = s {
                if candidate.overlaps(other) {
                    return Err(VmaError::Overlap);
                }
            }
        }
        self.slots[idx] = Some(candidate);
        Ok(())
    }

    /// 清空全部 VMA（保留数组容量，便于复用）。
    pub fn clear(&mut self) {
        self.slots = [None; MAX_REGIONS];
        self.len = 0;
    }

    /// 按槽位顺序迭代（未占用的槽位跳过）。
    pub fn iter(&self) -> impl Iterator<Item = &UserMemoryRegion> {
        self.slots.iter().flatten()
    }
}

impl Default for RegionTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NULL_GUARD_END, PAGE_SIZE, RegionFlags, RegionKind};

    fn reg(start: u64, end: u64, flags: RegionFlags, kind: RegionKind) -> UserMemoryRegion {
        UserMemoryRegion::new(start, end, flags, kind)
    }

    fn code(start: u64, end: u64) -> UserMemoryRegion {
        reg(start, end, RegionFlags::RX, RegionKind::Code)
    }

    fn data(start: u64, end: u64) -> UserMemoryRegion {
        reg(start, end, RegionFlags::RW, RegionKind::Data)
    }

    fn heap(start: u64, end: u64) -> UserMemoryRegion {
        reg(start, end, RegionFlags::RW | RegionFlags::GROWABLE, RegionKind::Heap)
    }

    #[test]
    fn new_is_empty() {
        let t = RegionTable::new();
        assert_eq!(t.len(), 0);
        assert!(t.is_empty());
        assert!(t.lookup(0x4000_0000).is_none());
    }

    #[test]
    fn insert_and_lookup() {
        let mut t = RegionTable::new();
        t.insert(code(0x4000_0000, 0x4000_8000)).unwrap();
        t.insert(data(0x4010_0000, 0x4010_4000)).unwrap();
        assert_eq!(t.len(), 2);
        assert!(t.lookup(0x4000_4000).unwrap().flags.contains(RegionFlags::EXEC));
        assert!(t.lookup(0x4010_1000).unwrap().kind == RegionKind::Data);
        // 区间外
        assert!(t.lookup(0x4000_8000).is_none());
        assert!(t.lookup(0x3FFF_FFFF).is_none());
    }

    #[test]
    fn overlap_rejected_all_kinds() {
        let mut t = RegionTable::new();
        t.insert(code(0x4000_0000, 0x4000_8000)).unwrap();
        // 完全包含
        assert_eq!(
            t.insert(code(0x4000_1000, 0x4000_2000)).unwrap_err(),
            VmaError::Overlap
        );
        // 左搭接
        assert_eq!(
            t.insert(code(0x3FFF_8000, 0x4000_1000)).unwrap_err(),
            VmaError::Overlap
        );
        // 右搭接
        assert_eq!(
            t.insert(code(0x4000_7000, 0x4000_9000)).unwrap_err(),
            VmaError::Overlap
        );
        // 端点相邻 → 允许
        t.insert(code(0x4000_8000, 0x4000_A000)).unwrap();
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn insert_delegates_validate() {
        let mut t = RegionTable::new();
        // 注入未对齐 / 空 / NULL 守卫三种 → 期待表自身不重复实现校验
        assert_eq!(
            t.insert(reg(0x4000_0001, 0x4000_8000, RegionFlags::RX, RegionKind::Code))
                .unwrap_err(),
            VmaError::Unaligned
        );
        assert_eq!(
            t.insert(reg(0x4000_8000, 0x4000_8000, RegionFlags::RX, RegionKind::Code))
                .unwrap_err(),
            VmaError::Empty
        );
        assert_eq!(
            t.insert(reg(0, 0x2000, RegionFlags::RW, RegionKind::Data))
                .unwrap_err(),
            VmaError::NullGuard
        );
    }

    #[test]
    fn null_guard_boundary_accepted() {
        // 起点刚好 == NULL_GUARD_END → 合规
        let mut t = RegionTable::new();
        t.insert(reg(NULL_GUARD_END, NULL_GUARD_END + PAGE_SIZE, RegionFlags::RW, RegionKind::Data))
            .unwrap();
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn full_rejected_at_max() {
        let mut t = RegionTable::new();
        // 用 32 个互不重叠的 4KB 区间填满表（每条 2MB 间隔避 PD entry 共享——其实不影响本测试）
        for i in 0..MAX_REGIONS {
            let s = 0x1_0000_0000 + (i as u64) * 0x20_0000; // 起始于 4GB+，纯逻辑不关心用户窗口
            t.insert(code(s, s + PAGE_SIZE)).unwrap();
        }
        assert_eq!(t.len(), MAX_REGIONS);
        // 第 33 条 → Full
        let extra = 0x1_0000_0000 + MAX_REGIONS as u64 * 0x20_0000;
        assert_eq!(
            t.insert(code(extra, extra + PAGE_SIZE)).unwrap_err(),
            VmaError::Full
        );
    }

    #[test]
    fn remove_by_start() {
        let mut t = RegionTable::new();
        t.insert(code(0x4000_0000, 0x4000_8000)).unwrap();
        t.insert(data(0x4010_0000, 0x4010_4000)).unwrap();
        assert_eq!(t.len(), 2);

        let r = t.remove(0x4000_0000);
        assert!(r.is_some());
        assert_eq!(t.len(), 1);
        // 起点不存在
        assert!(t.remove(0x4020_0000).is_none());
        // 删后再插入同区间 → OK（无重叠）
        t.insert(code(0x4000_0000, 0x4000_8000)).unwrap();
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn grow_heap_up() {
        let mut t = RegionTable::new();
        t.insert(heap(0x4000_0000, 0x4000_8000)).unwrap();
        // 向上延展：起点不动，终点拉远
        t.grow(0x4000_0000, 0x4000_0000, 0x4010_0000).unwrap();
        assert_eq!(t.lookup(0x4000_9000).unwrap().kind, RegionKind::Heap);
        // 缩小 → Shrink
        assert_eq!(
            t.grow(0x4000_0000, 0x4000_0000, 0x4000_4000).unwrap_err(),
            VmaError::Shrink
        );
        // 起点右移 → Shrink（不允许位移；堆应向上长而非起点平移）
        assert_eq!(
            t.grow(0x4000_0000, 0x4000_1000, 0x4010_0000).unwrap_err(),
            VmaError::Shrink
        );
    }

    #[test]
    fn grow_stack_down() {
        let mut t = RegionTable::new();
        t.insert(reg(
            0x4000_0000,
            0x4000_8000,
            RegionFlags::RW | RegionFlags::GROWABLE,
            RegionKind::Stack,
        ))
        .unwrap();
        // 向下延展：终点不动，起点拉低
        t.grow(0x4000_0000, 0x3FFF_0000, 0x4000_8000).unwrap();
        assert_eq!(t.lookup(0x3FFF_8000).unwrap().kind, RegionKind::Stack);
    }

    #[test]
    fn grow_rejects_not_growable() {
        let mut t = RegionTable::new();
        t.insert(data(0x4000_0000, 0x4000_8000)).unwrap(); // 无 GROWABLE
        assert_eq!(
            t.grow(0x4000_0000, 0x4000_0000, 0x4010_0000).unwrap_err(),
            VmaError::NotGrowable
        );
    }

    #[test]
    fn grow_rejects_overlap_with_other() {
        let mut t = RegionTable::new();
        t.insert(heap(0x4000_0000, 0x4000_8000)).unwrap();
        t.insert(code(0x4000_8000, 0x4000_A000)).unwrap(); // 相邻
        // 把 heap 上延展到覆盖 code → Overlap
        assert_eq!(
            t.grow(0x4000_0000, 0x4000_0000, 0x4000_9000).unwrap_err(),
            VmaError::Overlap
        );
    }

    #[test]
    fn grow_not_found() {
        let mut t = RegionTable::new();
        assert_eq!(
            t.grow(0x4000_0000, 0x4000_0000, 0x4000_8000).unwrap_err(),
            VmaError::NotFound
        );
    }

    #[test]
    fn iter_skips_empty_slots() {
        let mut t = RegionTable::new();
        t.insert(code(0x4000_0000, 0x4000_8000)).unwrap();
        t.insert(data(0x4010_0000, 0x4010_4000)).unwrap();
        let mut kinds: [Option<RegionKind>; 2] = [None, None];
        let mut n = 0;
        for r in t.iter() {
            kinds[n] = Some(r.kind);
            n += 1;
        }
        assert_eq!(n, 2);
        assert!(kinds.contains(&Some(RegionKind::Code)));
        assert!(kinds.contains(&Some(RegionKind::Data)));
    }

    #[test]
    fn clear_empties_table() {
        let mut t = RegionTable::new();
        t.insert(code(0x4000_0000, 0x4000_8000)).unwrap();
        t.insert(data(0x4010_0000, 0x4010_4000)).unwrap();
        t.clear();
        assert_eq!(t.len(), 0);
        assert!(t.is_empty());
        // 复用容量：插入第 33 条仍能成功
        for i in 0..MAX_REGIONS {
            t.insert(code(
                0x1_0000_0000 + (i as u64) * 0x20_0000,
                0x1_0000_0000 + (i as u64) * 0x20_0000 + PAGE_SIZE,
            ))
            .unwrap();
        }
        assert_eq!(t.len(), MAX_REGIONS);
    }
}
