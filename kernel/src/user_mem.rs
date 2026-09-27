// Minimal user memory helpers: copy_from_user / copy_to_user
// NOTE: This crate is part of kernel (no_std). Avoid heap allocation.
use core::ptr;
use synapse_abi::E_INVALID_ADDR;
use crate::paging::AddressSpace;

/// Copy bytes from user virtual address `user_va` (in the AddressSpace pointed by `as_ptr`)
/// into the kernel buffer `dst`. Returns Ok(()) or Err(errno).
pub fn copy_from_user(as_ptr: u64, user_va: u64, dst: &mut [u8]) -> Result<(), i64> {
    if as_ptr == 0 { return Err(E_INVALID_ADDR); }
    let as_user = unsafe { &*(as_ptr as *const AddressSpace) };
    if dst.len() == 0 { return Ok(()); }
    // validate each page
    let mut remaining = dst.len();
    let mut cur_va = user_va;
    let mut off = 0usize;
    while remaining > 0 {
        let page = cur_va & !0xfffu64;
        match as_user.walk_flags(page) {
            Some((pa, flags)) => {
                if flags & crate::paging::PT_USER == 0 { return Err(E_INVALID_ADDR); }
                let page_offset = (cur_va & 0xfff) as usize;
                let can = core::cmp::min(remaining, 4096 - page_offset);
                unsafe {
                    let src = (pa + page_offset as u64) as *const u8;
                    ptr::copy_nonoverlapping(src, dst[off..].as_mut_ptr(), can);
                }
                off += can;
                cur_va += can as u64;
                remaining -= can;
            }
            None => return Err(E_INVALID_ADDR),
        }
    }
    Ok(())
}

/// Copy bytes from kernel buffer `src` into user virtual address `user_va`.
pub fn copy_to_user(as_ptr: u64, user_va: u64, src: &[u8]) -> Result<(), i64> {
    if as_ptr == 0 { return Err(E_INVALID_ADDR); }
    let as_user = unsafe { &*(as_ptr as *const AddressSpace) };
    if src.len() == 0 { return Ok(()); }
    let mut remaining = src.len();
    let mut cur_va = user_va;
    let mut off = 0usize;
    while remaining > 0 {
        let page = cur_va & !0xfffu64;
        match as_user.walk_flags(page) {
            Some((pa, flags)) => {
                if flags & crate::paging::PT_USER == 0 { return Err(E_INVALID_ADDR); }
                // for writes, require writable
                if flags & crate::paging::PT_WRITABLE == 0 { return Err(E_INVALID_ADDR); }
                let page_offset = (cur_va & 0xfff) as usize;
                let can = core::cmp::min(remaining, 4096 - page_offset);
                unsafe {
                    let dst = (pa + page_offset as u64) as *mut u8;
                    // Use volatile write to avoid compiler reorder
                    for i in 0..can {
                        core::ptr::write_volatile(dst.add(i), src[off + i]);
                    }
                }
                off += can;
                cur_va += can as u64;
                remaining -= can;
            }
            None => return Err(E_INVALID_ADDR),
        }
    }
    Ok(())
}
