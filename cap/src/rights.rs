//! 权限位掩码（对齐 [Doc 01 §4](../../../docs/design/01-capability-agent-permission-model.md) `bitflags! { Rights }`）。
//!
//! 手写实现（零依赖），语义与 bitflags 宏一致。
//! 约束（Doc 02 §4.5 ABI 版本策略）：权限位**只允许追加**；
//! 删除或重解释权限位必须提升 ABI major 版本。bit 7..=31 保留。

/// Capability 权限位掩码。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rights(u32);

impl Rights {
    /// 向 endpoint 发送消息。
    pub const SEND: Rights = Rights(1 << 0);
    /// 从 endpoint 接收消息。
    pub const RECV: Rights = Rights(1 << 1);
    /// 对已接收消息回复。
    pub const REPLY: Rights = Rights(1 << 2);
    /// 读内存区域 / 对象。
    pub const READ: Rights = Rights(1 << 3);
    /// 写内存区域 / 对象。
    pub const WRITE: Rights = Rights(1 << 4);
    /// 执行（内存区域 EXEC / WasmRuntime）。
    pub const EXEC: Rights = Rights(1 << 5);
    /// 允许再委托（派生子 capability / cap transfer）。
    pub const GRANT: Rights = Rights(1 << 6);

    /// 空权限集。
    pub const EMPTY: Rights = Rights(0);
    /// 全部已定义权限位（不含保留位）。
    pub const ALL: Rights = Rights(0b111_1111);

    /// 从原始位构造（保留位会被掩掉，防 ABI 漂移）。
    pub const fn from_bits(bits: u32) -> Rights {
        Rights(bits & 0b111_1111)
    }

    /// 原始位值。
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// 是否包含 `other` 的全部权限位（`self ⊇ other`）。
    ///
    /// 校验路径核心：`required ⊆ held` 才放行。
    pub const fn contains(self, other: Rights) -> bool {
        (self.0 & other.0) == other.0
    }

    /// 权限交集（attenuation 用：派生权限 = 原权限 ∩ 掩码）。
    pub const fn intersection(self, other: Rights) -> Rights {
        Rights(self.0 & other.0)
    }

    /// 权限并集。
    pub const fn union(self, other: Rights) -> Rights {
        Rights(self.0 | other.0)
    }

    /// 是否为空集。
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl core::ops::BitOr for Rights {
    type Output = Rights;
    fn bitor(self, rhs: Rights) -> Rights {
        self.union(rhs)
    }
}

impl core::ops::BitAnd for Rights {
    type Output = Rights;
    fn bitand(self, rhs: Rights) -> Rights {
        self.intersection(rhs)
    }
}
