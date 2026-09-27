//! spawn 初始 capability 授予（对齐 [Doc 02 §5.2](../../../docs/design/02-userspace-abi-and-process-model.md)）。
//!
//! `spawn(elf_ref, args, caps)` 的 `caps` 参数：父进程 CapTable 中的
//! CapRef 数组，内核将其**委托**给子进程（attenuation-only）。
//! 子进程 CapTable 初始状态：slot 0 = NULL trap（`CapTable::new` 已保证），
//! slot 1..N = 父进程授予的初始 caps。
//!
//! 转移机制复用 [`synapse_cap::transfer_caps`] 的 atomic all-or-nothing
//! 语义：任何一项失败（cap 不存在 / 缺 GRANT / 对象非 Live / 子表槽不足）
//! → 整批失败，父表零副作用，spawn 由集成层整体回滚（子进程不产生）。

use synapse_cap::{
    transfer_caps, CapError, CapRef, CapTable, ObjectTable, Rights, TransferItem, MAX_TRANSFER,
};

/// 单次 spawn 可授予的初始 cap 数上限（复用 IPC 转移上限，
/// Doc 03 §4 `cap_transfer_count` 同源约束）。
pub const MAX_INITIAL_CAPS: usize = MAX_TRANSFER;

/// 授予项：父表槽位 + 衰减掩码（子进程权限 = 父权限 ∩ mask）。
#[derive(Clone, Copy, Debug)]
pub struct GrantItem {
    /// 父进程 CapTable 中的槽位。
    pub cptr: CapRef,
    /// 衰减掩码。
    pub mask: Rights,
}

/// 将初始 caps 安装进子进程 CapTable（atomic：全成或全不成）。
///
/// `dst_parent = None`：子表中这些 cap 是根引用（其内核侧父子链
/// 通过父表对应槽维系——撤销级联时集成层沿父表 → 子表遍历，
/// Doc 01 §3.4 跨进程派生副本处理）。
///
/// 返回子表中安装好的槽位（与 `items` 顺序一致；按 `CapTable::new`
/// 的 LIFO 空闲栈，首个授予落在高槽位——集成层若要求 slot 1..N
/// 顺序授予，可在安装后重排或使用定制构造）。
pub fn install_initial_caps(
    parent_table: &CapTable,
    child_table: &mut CapTable,
    items: &[GrantItem],
    objects: &ObjectTable,
) -> Result<[CapRef; MAX_INITIAL_CAPS], CapError> {
    if items.len() > MAX_INITIAL_CAPS {
        return Err(CapError::InvalidCap);
    }
    let mut transfer_items = [TransferItem { cptr: 0, mask: Rights::EMPTY }; MAX_TRANSFER];
    for (i, g) in items.iter().enumerate() {
        transfer_items[i] = TransferItem { cptr: g.cptr, mask: g.mask };
    }
    transfer_caps(parent_table, child_table, &transfer_items[..items.len()], None, objects)
}
