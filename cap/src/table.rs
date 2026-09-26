//! 每进程能力表（对齐 [Doc 01 §4](../../../docs/design/01-capability-agent-permission-model.md) `CapTable`）。
//!
//! 固定 256 槽 + 空闲槽位栈（O(1) 分配 / 释放）。
//! 锁语义按多核就绪标准编写（首期单核，本 crate 无锁——由内核集成层负责关中断临界区）。

use crate::error::CapError;
use crate::rights::Rights;
use crate::types::{Capability, CapRef, CAP_TABLE_SIZE};

/// 每进程一张能力表。
///
/// 固定 256 项 —— 避免动态扩容带来的锁竞争（Doc 01 §4 设计约束）。
pub struct CapTable {
    slots: [Option<Capability>; CAP_TABLE_SIZE],
    /// 空闲槽位栈（存放空闲 slot 索引）。
    free_list: [u8; CAP_TABLE_SIZE],
    /// 栈中空闲槽数量（0..=256；u8 无法表达 256，故用 u16）。
    free_top: u16,
}

impl Default for CapTable {
    fn default() -> Self {
        Self::new()
    }
}

impl CapTable {
    /// 创建空表：全部 256 槽空闲。
    ///
    /// 约定（Doc 02 §5.2）：slot 0 保留为 NULL trap——init 授予子进程的
    /// 初始 caps 从 slot 1 开始；本构造函数将 slot 0 预先占用，`alloc`
    /// 永不返回 0。
    pub fn new() -> CapTable {
        let mut free_list = [0u8; CAP_TABLE_SIZE];
        // 栈中依次压入 255..=1（slot 0 保留为 NULL trap，不入栈）
        for (i, slot) in free_list.iter_mut().enumerate() {
            *slot = (CAP_TABLE_SIZE - 1 - i) as u8;
        }
        CapTable {
            slots: [const { None }; CAP_TABLE_SIZE],
            free_list,
            // 255 个可分配槽（slot 0 保留）
            free_top: (CAP_TABLE_SIZE - 1) as u16,
        }
    }

    /// 剩余空闲槽数（cap transfer 的 atomic 预检查用，Doc 03 §5）。
    pub fn remaining_free(&self) -> usize {
        self.free_top as usize
    }

    /// 分配新槽位，返回 `CapRef`（8-bit 索引）。表满返回 [`CapError::NoMemory`]。
    pub fn alloc(&mut self, cap: Capability) -> Result<CapRef, CapError> {
        if self.free_top == 0 {
            return Err(CapError::NoMemory);
        }
        self.free_top -= 1;
        let slot = self.free_list[self.free_top as usize] as usize;
        debug_assert!(self.slots[slot].is_none());
        self.slots[slot] = Some(cap);
        Ok(slot as CapRef)
    }

    /// 释放槽位（对象销毁 / 进程退出回收路径）。空槽或 slot 0 为 no-op 错误。
    pub fn free(&mut self, cptr: CapRef) -> Result<(), CapError> {
        let idx = cptr as usize;
        if idx == 0 || idx >= CAP_TABLE_SIZE || self.slots[idx].is_none() {
            return Err(CapError::InvalidCap);
        }
        self.slots[idx] = None;
        self.free_list[self.free_top as usize] = cptr;
        self.free_top += 1;
        Ok(())
    }

    /// 按 `CapRef` 查找 capability（O(1) 数组索引，热路径）。
    pub fn get(&self, cptr: CapRef) -> Result<&Capability, CapError> {
        let idx = cptr as usize;
        if idx >= CAP_TABLE_SIZE {
            return Err(CapError::InvalidCap);
        }
        self.slots[idx].as_ref().ok_or(CapError::InvalidCap)
    }

    /// 可变借用（badge 修改 / 撤销标记等管理路径）。
    pub fn get_mut(&mut self, cptr: CapRef) -> Result<&mut Capability, CapError> {
        let idx = cptr as usize;
        if idx >= CAP_TABLE_SIZE {
            return Err(CapError::InvalidCap);
        }
        self.slots[idx].as_mut().ok_or(CapError::InvalidCap)
    }

    /// 在本表内派生子 capability（attenuation-only）并安装。
    ///
    /// - 权限 = `src.rights ∩ mask`（不可放大，Doc 01 §3.3）；
    /// - 新槽 `parent = Some(src_cptr)`（保留父子链，撤销级联前提）；
    /// - 源 capability 必须持有 `GRANT` 位（否则 [`CapError::Permission`]）。
    pub fn delegate(
        &mut self,
        src_cptr: CapRef,
        mask: Rights,
    ) -> Result<CapRef, CapError> {
        let src = self.get(src_cptr)?.clone();
        if !src.rights.contains(Rights::GRANT) {
            return Err(CapError::Permission);
        }
        let mut child = src.attenuated(mask);
        child.parent = Some(src_cptr);
        self.alloc(child)
    }

    /// 级联撤销（Doc 01 §3.4 derivation tree 遍历）：
    /// 撤销 `cptr` 及其全部派生副本（沿 `parent` 链，含跨代）。
    ///
    /// 复杂度 O(n·d)：n = 256 槽，d = 派生深度；不动点迭代避免递归
    /// （内核栈不深，禁止递归遍历）。返回被撤销的槽数（含自身）。
    ///
    /// 注意：本方法只清理**本表内**的 capability 槽。对象级状态
    /// （`Live → Revoking → Retired`）由 [`crate::object::ObjectTable`]
    /// 负责；跨进程派生副本由内核集成层遍历各进程 CapTable 调用本方法。
    pub fn revoke_cascade(&mut self, cptr: CapRef) -> Result<usize, CapError> {
        // 先校验目标存在（不存在 → InvalidCap，不产生副作用）
        self.get(cptr)?;

        let mut revoked = [false; CAP_TABLE_SIZE];
        revoked[cptr as usize] = true;
        let mut count = 1usize;

        // 不动点迭代：反复扫描，直到没有新的槽被标记
        loop {
            let mut changed = false;
            for idx in 1..CAP_TABLE_SIZE {
                if revoked[idx] {
                    continue;
                }
                if let Some(cap) = &self.slots[idx] {
                    if let Some(p) = cap.parent {
                        if revoked[p as usize] {
                            revoked[idx] = true;
                            changed = true;
                            count += 1;
                        }
                    }
                }
            }
            if !changed {
                break;
            }
        }

        // 统一释放被标记的槽（slot 0 永不在标记集中）
        for idx in 1..CAP_TABLE_SIZE {
            if revoked[idx] {
                self.slots[idx] = None;
                self.free_list[self.free_top as usize] = idx as u8;
                self.free_top += 1;
            }
        }
        Ok(count)
    }

    /// 遍历所有占用槽（审计 / 进程退出回收路径用）。
    pub fn iter(&self) -> impl Iterator<Item = (CapRef, &Capability)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(idx, slot)| slot.as_ref().map(|cap| (idx as CapRef, cap)))
    }
}
