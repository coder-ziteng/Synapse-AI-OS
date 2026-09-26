//! 内核对象表：生命周期状态机 + generation 校验
//! （对齐 [Doc 01 §4.2](../../../docs/design/01-capability-agent-permission-model.md)）。
//!
//! 状态机：`Live → Revoking → Retired → Freed`。
//! slot 复用时 generation 递增，旧 [`ObjRef`] 校验必然失败
//! （generation 历史独立于槽位存储，槽清空不丢代际）。

use crate::error::CapError;
use crate::types::{ObjKind, ObjRef, ObjState};

/// 内核对象表容量上限（首期固定；进程级配额见 [`crate::quota`]）。
pub const MAX_OBJECTS: usize = 1024;

/// 单个对象槽。
#[derive(Clone, Copy, Debug)]
struct ObjSlot {
    state: ObjState,
    generation: u32,
    kind: ObjKind,
}

/// 全局内核对象表（内核集成层持有一份，Spinlock + 关中断保护）。
pub struct ObjectTable {
    slots: [Option<ObjSlot>; MAX_OBJECTS],
    /// generation 历史：独立于槽位保存，槽释放后仍保留，
    /// 下次复用该槽时递增——旧 ObjRef 因 generation 不匹配必然失效。
    generations: [u32; MAX_OBJECTS],
    /// 已分配（未 Freed）对象数。
    live_count: usize,
}

impl Default for ObjectTable {
    fn default() -> Self {
        Self::new()
    }
}

impl ObjectTable {
    /// 创建空对象表。
    pub fn new() -> ObjectTable {
        ObjectTable {
            slots: [const { None }; MAX_OBJECTS],
            generations: [0; MAX_OBJECTS],
            live_count: 0,
        }
    }

    /// 当前存活对象数。
    pub fn live_count(&self) -> usize {
        self.live_count
    }

    /// 分配新对象，返回带 generation 的 [`ObjRef`]。
    ///
    /// slot 复用时 generation 递增（不复位），保证旧引用失效。
    /// 表满返回 [`CapError::NoMemory`]。
    pub fn alloc(&mut self, kind: ObjKind) -> Result<ObjRef, CapError> {
        // 线性扫描找空槽（MAX_OBJECTS=1024，冷路径可接受；
        // 热路径优化留待性能基准后决策，见 Doc 01 §3.2 TBD）
        for idx in 0..MAX_OBJECTS {
            if self.slots[idx].is_none() {
                self.generations[idx] = self.generations[idx].wrapping_add(1);
                let generation = self.generations[idx];
                self.slots[idx] =
                    Some(ObjSlot { state: ObjState::Live, generation, kind });
                self.live_count += 1;
                return Ok(ObjRef { index: idx as u32, generation });
            }
        }
        Err(CapError::NoMemory)
    }

    /// 释放对象槽（`Retired` 之后由回收路径调用）。
    ///
    /// generation 历史保留；下次 `alloc` 复用该槽时递增。
    pub fn free(&mut self, obj: ObjRef) -> Result<(), CapError> {
        let slot = self.slot_checked(obj)?;
        if slot.state == ObjState::Freed {
            return Err(CapError::ObjectRetired);
        }
        self.slots[obj.index as usize] = None;
        self.live_count -= 1;
        Ok(())
    }

    /// 取槽（可变）并校验 generation；越界 → InvalidCap，
    /// 槽空 / generation 不匹配 → ObjectRetired。
    fn slot_checked(&mut self, obj: ObjRef) -> Result<&mut ObjSlot, CapError> {
        let idx = obj.index as usize;
        if idx >= MAX_OBJECTS {
            return Err(CapError::InvalidCap);
        }
        let slot = self.slots[idx].as_mut().ok_or(CapError::ObjectRetired)?;
        if slot.generation != obj.generation {
            return Err(CapError::ObjectRetired);
        }
        Ok(slot)
    }

    /// 校验对象引用有效性 + 状态为 `Live`（invoke 热路径，O(1)）。
    ///
    /// - 索引越界 → [`CapError::InvalidCap`]；
    /// - 槽空 / generation 不匹配 / 状态非 `Live` → [`CapError::ObjectRetired`]。
    ///
    /// 设计约束（Doc 01 §4.2）：generation 校验是 O(1) 比较，不影响 NFR2。
    pub fn check_live(&self, obj: ObjRef) -> Result<ObjKind, CapError> {
        let idx = obj.index as usize;
        if idx >= MAX_OBJECTS {
            return Err(CapError::InvalidCap);
        }
        let slot = self.slots[idx].as_ref().ok_or(CapError::ObjectRetired)?;
        if slot.generation != obj.generation || slot.state != ObjState::Live {
            return Err(CapError::ObjectRetired);
        }
        Ok(slot.kind)
    }

    /// 查询对象当前状态（管理路径）。
    pub fn state_of(&self, obj: ObjRef) -> Result<ObjState, CapError> {
        let idx = obj.index as usize;
        if idx >= MAX_OBJECTS {
            return Err(CapError::InvalidCap);
        }
        let slot = self.slots[idx].as_ref().ok_or(CapError::ObjectRetired)?;
        if slot.generation != obj.generation {
            return Err(CapError::ObjectRetired);
        }
        Ok(slot.state)
    }

    /// 状态迁移：`Live → Revoking`（开始撤销 derivation tree）。
    ///
    /// 进入 `Revoking` 后所有新 invoke 立即返回错误，不等待遍历完成
    /// （Doc 01 §4.2：避免阻塞热路径）。
    pub fn begin_revoke(&mut self, obj: ObjRef) -> Result<(), CapError> {
        let slot = self.slot_checked(obj)?;
        match slot.state {
            ObjState::Live => {
                slot.state = ObjState::Revoking;
                Ok(())
            }
            ObjState::Revoking | ObjState::Retired | ObjState::Freed => {
                Err(CapError::ObjectRetired)
            }
        }
    }

    /// 状态迁移：`Revoking → Retired`（derivation tree 遍历完成后）。
    pub fn retire(&mut self, obj: ObjRef) -> Result<(), CapError> {
        let slot = self.slot_checked(obj)?;
        match slot.state {
            ObjState::Revoking => {
                slot.state = ObjState::Retired;
                Ok(())
            }
            ObjState::Live | ObjState::Retired | ObjState::Freed => {
                Err(CapError::ObjectRetired)
            }
        }
    }
}
