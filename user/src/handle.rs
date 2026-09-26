//! 用户态 handle 类型。
//!
//! 与 `synapse-cap` / `synapse-ipc` **同形**但**不可互通**：内核 handle
//! 携带表引用（`&CapTable`），用户态仅持有 `cptr` 索引（`u8`）。本模块
//! 用 newtype 把 `u8` 索引包成 [`CapRef`]，构造时即校验 0..=255，与
//! 内核 `CapTable` 槽位大小一致（Doc 01 §3）。

use core::fmt;

/// 用户态 capability 槽位索引（与内核 `CapTable` 容量 256 一致）。
///
/// 构造仅接受 0..=255；其它值返回 [`CapError::InvalidIndex`]。输入参数
/// 类型为 `u16` 而非 `u8`，便于从外部整数（解析 / 配置 / 内核返回值）
/// 直接构造而无需先做截断——截断会绕过校验，构成混淆面。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CapRef(u8);

impl CapRef {
    /// 新建槽位引用。
    ///
    /// # Errors
    ///
    /// `index > 255` 时返回 [`CapError::InvalidIndex`]——硬上限来自内核
    /// `CapTable<N=256>` 容量，禁止用户态伪造更大索引。
    pub const fn new(index: u16) -> Result<CapRef, CapError> {
        if index <= 255 {
            Ok(CapRef(index as u8))
        } else {
            Err(CapError::InvalidIndex)
        }
    }

    /// 槽位号（`0..=255`）。
    #[inline]
    pub const fn index(self) -> u8 {
        self.0
    }

    /// 作为 64-bit syscall 参数（高位补 0，类型显式转换）。
    #[inline]
    pub const fn as_u64(self) -> u64 {
        self.0 as u64
    }
}

impl fmt::Debug for CapRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CapRef({})", self.0)
    }
}

/// 用户态 handle 构造错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapError {
    /// 索引越界（> 255）。
    InvalidIndex,
}

impl fmt::Display for CapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CapError::InvalidIndex => f.write_str("capability index out of range (max 255)"),
        }
    }
}