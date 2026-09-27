//! P4-T13 用户指针统一校验路径（评审 §5 契约「地址」行）。
//!
//! ## 职责
//!
//! syscall 集成层对**一切用户态指针参数**的唯一校验入口，覆盖五类约束：
//!
//! 1. **长度**：`len == 0` 语义由调用方定（[`user_mem_ok`] 视为平凡合法，
//!    需要拒绝零长的调用方自行前置检查——gettime 的 16B 输出即如此）；
//! 2. **对齐**：[`user_mem_ok_align`] 附加 `va % align == 0` 检查；
//! 3. **溢出**：`va + len` 用 `checked_add`，回绕即拒绝；
//! 4. **区间重叠**：[`ranges_overlap`] 供双缓冲 syscall（IPC msg/caps、
//!    src/dst 拷贝）判定是否需 memmove 语义或拒绝；
//! 5. **跨页授权**：`[va, va+len)` 逐页走 [`AddressSpace::walk_flags`]，
//!    每页必须 present + `PT_USER`（写缓冲再加 `PT_WRITABLE`）——
//!    **拒绝而非 fault**：syscall 集成层不允许触发 #PF 路径（demand
//!    paging 只对 VMA 登记的懒映射区生效）。
//!
//! ## 统一前（P4-T13 之前）的重复实现
//!
//! * `syscall.rs::user_mem_ok(&AddressSpace, …)`（gettime/umem 用）；
//! * `ipc.rs::user_mem_ok(as_ptr: u64, …)`（IPC 缓冲用，`as_ptr==0` 表示
//!   内核缓冲 identity-mapped 直访）。
//!
//! 两者语义差一处：零长处理。本模块取 ipc 版语义（`len==0 → true`，
//! `as_ptr==0 → true` = 内核缓冲），syscall 侧包装函数保留自己的零长拒绝
//! 前置，行为不变。mmap（umem.rs）与 IPC 缓冲区校验自此共用同一 helper。
//!
//! ## 写入 helper
//!
//! [`write_user_bytes`]：校验通过后经恒等映射 PA 写入（walk_flags 返 PA，
//! `write_volatile` 逐字节），供 cap_delegate 的 child_out 等小输出参数用；
//! 大块拷贝仍走 ipc.rs 的 `ipc_copy`（memmove 重叠兜底）。

use crate::paging::{AddressSpace, PT_USER, PT_WRITABLE};

/// 用户缓冲合法性校验（统一路径；`as_ptr == 0` = 内核缓冲，恒真）。
///
/// `[va, va+len)` 每页 present + `PT_USER`（+ `PT_WRITABLE` 若 `need_write`）。
/// `va + len` 溢出 → false。`len == 0` → true（平凡合法；调用方需要拒绝
/// 零长时自行前置检查）。
pub fn user_mem_ok(as_ptr: u64, va: u64, len: u64, need_write: bool) -> bool {
    if as_ptr == 0 {
        return true; // 内核缓冲：identity mapping 下 VA = PA 恒可达
    }
    if len == 0 {
        return true;
    }
    let Some(end) = va.checked_add(len) else { return false };
    let as_ref = unsafe { &*(as_ptr as *const AddressSpace) };
    let mut page = va & !0xFFF;
    while page < end {
        match as_ref.walk_flags(page) {
            Some((_, flags)) => {
                if flags & PT_USER == 0 {
                    return false;
                }
                if need_write && flags & PT_WRITABLE == 0 {
                    return false;
                }
            }
            None => return false,
        }
        page += 0x1000;
    }
    true
}

/// [`user_mem_ok`] + 对齐检查（评审 §5「对齐」约束；align 必须是 2 的幂）。
pub fn user_mem_ok_align(as_ptr: u64, va: u64, len: u64, align: u64, need_write: bool) -> bool {
    debug_assert!(align.is_power_of_two());
    if va % align != 0 {
        return false;
    }
    user_mem_ok(as_ptr, va, len, need_write)
}

/// 两区间是否重叠（半开区间 `[a, a+alen)` × `[b, b+blen)`）。
///
/// 双缓冲 syscall（IPC msg/cap_out、拷贝 src/dst）用于判定 memmove 语义
/// 或直接拒绝。零长区间与任何区间不重叠。
pub fn ranges_overlap(a: u64, alen: u64, b: u64, blen: u64) -> bool {
    if alen == 0 || blen == 0 {
        return false;
    }
    // checked_add 防回绕假阳性
    let (Some(a_end), Some(b_end)) = (a.checked_add(alen), b.checked_add(blen)) else {
        return true; // 溢出即视为可疑 → 保守判重叠
    };
    a < b_end && b < a_end
}

/// 校验并写入用户缓冲（经恒等映射 PA，`write_volatile`）。
///
/// 成功 → true；校验失败（未映射/无权限/溢出）→ false，不写任何字节
/// （all-or-nothing：先全区间校验后写入）。
///
/// # Safety（调用契约）
///
/// * `as_ptr` 为 0（内核缓冲）或指向有效 [`AddressSpace`]；
/// * `data` 长度与用户缓冲声明长度一致（调用方已按 len 校验）。
pub unsafe fn write_user_bytes(as_ptr: u64, va: u64, data: &[u8], need_write: bool) -> bool {
    if !user_mem_ok(as_ptr, va, data.len() as u64, need_write) {
        return false;
    }
    if as_ptr == 0 {
        // 内核缓冲：identity mapping 直写
        core::ptr::copy_nonoverlapping(data.as_ptr(), va as *mut u8, data.len());
        return true;
    }
    let as_ref = &*(as_ptr as *const AddressSpace);
    let mut off = 0usize;
    while off < data.len() {
        let page_va = (va + off as u64) & !0xFFF;
        let Some((pa, _flags)) = as_ref.walk_flags(page_va) else { return false };
        let page_off = (va + off as u64) - page_va;
        let chunk = core::cmp::min(0x1000 - page_off as usize, data.len() - off);
        let dst = (pa + page_off) as *mut u8;
        for i in 0..chunk {
            dst.add(i).write_volatile(data[off + i]);
        }
        off += chunk;
    }
    true
}
