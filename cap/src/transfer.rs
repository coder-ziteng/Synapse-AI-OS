//! 跨进程能力转移（对齐 [Doc 03 §5](../../../docs/design/03-ipc-message-and-single-copy-path.md)）。
//!
//! 语义：**atomic (all-or-nothing)**——任何一项预检查失败，整批转移
//! 失败，发送方 capability 全部保持原位，接收方不产生任何副作用。
//!
//! 权限衰减：接收方获得的权限 = 发送方持有权限 ∩ 掩码（不可放大）。
//! 父子链：转移成功的每个 capability 在接收方表中的 `parent` 指向
//! 接收方持有的"来源 capability"（`dst_parent`），保证撤销链路跨进程可追踪。

use crate::error::CapError;
use crate::object::ObjectTable;
use crate::rights::Rights;
use crate::table::CapTable;
use crate::types::{CapRef, Capability};

/// 单条 IPC 消息可携带的转移 capability 数上限（Doc 03 §4
/// `cap_transfer_count: u8` 的实际约束值；ABI 层先行拒绝超限消息头）。
pub const MAX_TRANSFER: usize = 8;

/// 单个转移项：源表槽位 + 衰减掩码。
#[derive(Clone, Copy, Debug)]
pub struct TransferItem {
    /// 发送方 CapTable 中的槽位。
    pub cptr: CapRef,
    /// 衰减掩码：接收方权限 = 源权限 ∩ mask（Doc 01 §3.3）。
    pub mask: Rights,
}

/// 原子转移一批 capability：`src` → `dst`。
///
/// 预检查顺序（全部通过才安装，任一失败零副作用）：
///
/// 1. `items.len() ≤ MAX_TRANSFER`，否则 [`CapError::InvalidCap`]；
/// 2. 每个源 cap 存在，否则 [`CapError::InvalidCap`]；
/// 3. 每个源 cap 持有 `GRANT` 位，否则 [`CapError::Permission`]（Doc 03 §5.1）；
/// 4. 每个目标内核对象为 `Live`，否则 [`CapError::ObjectRetired`]；
/// 5. `dst.remaining_free() ≥ N`，否则 [`CapError::NoMemory`]（Doc 03 §5.1）。
///
/// 返回接收方表中安装好的槽位（与 `items` 顺序一一对应）。
///
/// 锁语义：内核集成层负责在同一关中断临界区内持有 `src` / `dst` /
/// `objects` 三把锁后调用本函数（本 crate 无锁）。
pub fn transfer_caps(
    src: &CapTable,
    dst: &mut CapTable,
    items: &[TransferItem],
    dst_parent: Option<CapRef>,
    objects: &ObjectTable,
) -> Result<[CapRef; MAX_TRANSFER], CapError> {
    let n = items.len();
    if n > MAX_TRANSFER {
        return Err(CapError::InvalidCap);
    }

    // ---- 预检查阶段（只读，零副作用） ----
    let mut staged: [Option<Capability>; MAX_TRANSFER] = [const { None }; MAX_TRANSFER];
    for (i, item) in items.iter().enumerate() {
        let cap = src.get(item.cptr)?; // (2) 源存在
        if !cap.rights.contains(Rights::GRANT) {
            return Err(CapError::Permission); // (3) GRANT 位
        }
        objects.check_live(cap.obj)?; // (4) 对象 Live
        let mut child = cap.attenuated(item.mask);
        child.parent = dst_parent; // (父子链，Doc 03 §5)
        staged[i] = Some(child);
    }
    if dst.remaining_free() < n {
        return Err(CapError::NoMemory); // (5) atomic：槽位不足整批失败
    }

    // ---- 安装阶段（预检查已保证不会失败） ----
    let mut out = [0u8; MAX_TRANSFER];
    for i in 0..n {
        let cap = staged[i].take().expect("staged by precheck");
        out[i] = dst.alloc(cap)?;
    }
    Ok(out)
}
