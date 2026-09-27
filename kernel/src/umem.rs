//! 用户内存映射：`mmap`(#40) / `munmap`(#41) 内核实现（P4-T6）。
//!
//! ## 设计（MVP）
//!
//! - **eager 映射**：mmap 成功路径立即逐页 alloc_frame → 清零（防物理页
//!   残留信息泄漏）→ map_page（叶权限 = prot，W^X 已在 prot 校验层强制）
//!   → `demand_register` 登记 VMA。demand paging 路径对这些页永不触发
//!   （已 present）；VMA 登记的意义在 munmap 之后——再访问即"no VMA"
//!   → kill 骨架，语义闭环。
//! - **bump 选址**：`addr == 0` 时从 [`MMAP_ARENA_BASE`]（1GB+16MB，避开
//!   ELF 装载区 1GB..+8MB 与用户栈 0x4080_0000）线性推进；页间不留 guard
//!   gap（MVP，碎片问题留给 T12/远期 buddy）。`addr != 0` 要求页对齐 +
//!   落在用户窗口 + 与既有 VMA 无重叠；与已映射页（ELF 段/栈）冲突由
//!   `map_page` 的 `AlreadyMapped` 兜底 → `E_INVALID_ADDR`。
//! - **munmap 精确匹配**：(addr, len) 必须命中一条 ACTIVE 区域整段；
//!   部分解除 / 未登记 → `E_NOT_FOUND`（防误拆 ELF 段，T9 进程表接入后
//!   再考虑 POSIX 语义拆分）。
//! - **不回收空洞**：munmap 后 bump 指针不回退（区域可能仍在中间）；
//!   `cleanup_all`（进程退出路径）重置整个 arena。
//!
//! ## 错误码（Doc 02 §4.3，经 synapse_abi 常量）
//!
//! | 场景 | 码 |
//! | --- | --- |
//! | len=0 / 未页对齐 / addr 未对齐 / 越窗 / 重叠 / 撞已映射页 | `E_INVALID_ADDR` |
//! | prot 含未知位 / 无 R / W+X 同求 | `E_PERMISSION` |
//! | flags 含未知位 | `E_NOT_IMPLEMENTED` |
//! | 帧耗尽 / arena 耗尽 | `E_NO_MEMORY` |
//! | munmap 未命中 ACTIVE 区域 | `E_NOT_FOUND` |
//!
//! ## 账目（FR8）
//!
//! 每页帧走 page_frame 分配器出账；munmap/cleanup_all 逐页 unmap_page →
//! free_frame 归还。`elf_continuation` 在 FR8 断言前调 [`cleanup_all`]
//! 兜底（hello 自律 munmap 时为 no-op）。

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use log::info;

use synapse_abi::{
    E_INVALID_ADDR, E_NO_MEMORY, E_NOT_FOUND, E_NOT_IMPLEMENTED, E_PERMISSION,
    E_QUOTA_EXCEEDED, MAP_GROWABLE, MAP_MASK, PROT_EXEC, PROT_MASK, PROT_READ, PROT_WRITE,
};
use synapse_cap::quota::Resource;
use synapse_proc::process::Pid;
use synapse_vma::{RegionFlags, RegionKind, UserMemoryRegion};

use crate::page_frame::{alloc_frame, free_frame, FRAME_SIZE};
use crate::paging::{
    demand_register, demand_unregister, demand_with_table, AddressSpace, MapError, PT_NX,
    PT_USER, PT_WRITABLE, USER_REGION_END, USER_REGION_START,
};
use crate::sync::SpinLock;

/// mmap 选址 arena 起点（1GB + 16MB；ELF 段在 1GB..、用户栈顶 0x4080_0000，
/// 均在下方，互不冲突）。
pub const MMAP_ARENA_BASE: u64 = 0x4100_0000;

const FRAME: u64 = FRAME_SIZE as u64;

/// 一条 ACTIVE mmap 区域（munmap 精确匹配 + cleanup 账目）。
#[derive(Clone, Copy)]
struct Region {
    start: u64,
    len: u64,
    #[allow(dead_code)] // prot 留存：T7 mprotect / 增长路径消费
    prot: u32,
    #[allow(dead_code)] // flags 留存：T7 GROWABLE 增长路径消费
    flags: u32,
}

struct UmemState {
    /// bump 选址指针（下一次 addr=0 的候选）。
    bump: u64,
    /// ACTIVE 区域表。
    active: Vec<Region>,
}

static STATE: SpinLock<UmemState> = SpinLock::new(UmemState {
    bump: MMAP_ARENA_BASE,
    active: Vec::new(),
});

// 分发统计（elf_continuation 断言"hello 真实调用过 mmap/munmap"的内核侧证据）
static MMAP_OK: AtomicU64 = AtomicU64::new(0);
static MMAP_ERR: AtomicU64 = AtomicU64::new(0);
static MUNMAP_OK: AtomicU64 = AtomicU64::new(0);
static MUNMAP_ERR: AtomicU64 = AtomicU64::new(0);

/// (mmap 成功, mmap 失败, munmap 成功, munmap 失败) 计数快照。
pub fn stats() -> (u64, u64, u64, u64) {
    (
        MMAP_OK.load(Ordering::SeqCst),
        MMAP_ERR.load(Ordering::SeqCst),
        MUNMAP_OK.load(Ordering::SeqCst),
        MUNMAP_ERR.load(Ordering::SeqCst),
    )
}

/// 当前 ACTIVE 区域数（continuation 断言清理后 = 0）。
pub fn active_count() -> usize {
    STATE.lock().active.len()
}

/// prot(u32) → VMA `RegionFlags`（位值同构：R=bit0 W=bit1 X=bit2，见 abi crate）。
fn prot_to_flags(prot: u32, flags: u32) -> RegionFlags {
    let mut rf = RegionFlags::NONE;
    if prot & PROT_READ != 0 {
        rf = rf.union(RegionFlags::READ);
    }
    if prot & PROT_WRITE != 0 {
        rf = rf.union(RegionFlags::WRITE);
    }
    if prot & PROT_EXEC != 0 {
        rf = rf.union(RegionFlags::EXEC);
    }
    if flags & MAP_GROWABLE != 0 {
        rf = rf.union(RegionFlags::GROWABLE);
    }
    rf
}

/// prot(u32) → 页表叶 flags（W^X 已由校验层保证：X 时必无 W）。
fn prot_to_pt(prot: u32) -> u64 {
    let mut pt = PT_USER;
    if prot & PROT_WRITE != 0 {
        pt |= PT_WRITABLE;
    }
    if prot & PROT_EXEC == 0 {
        pt |= PT_NX;
    }
    pt
}

/// 校验参数合法性（公共段）。返回 `Err(错误码)` / `Ok(())`。
fn validate(prot: u32, flags: u32, len: u64) -> Result<(), i64> {
    // flags 未知位 → E_NOT_IMPLEMENTED（未实现语义，如 MAP_FIXED 类）
    if flags & !MAP_MASK != 0 {
        return Err(E_NOT_IMPLEMENTED);
    }
    // prot 未知位 / 无 R / W+X 同求（W^X 不变量）→ E_PERMISSION
    if prot & !PROT_MASK != 0 || prot & PROT_READ == 0 || prot & (PROT_WRITE | PROT_EXEC) == (PROT_WRITE | PROT_EXEC) {
        return Err(E_PERMISSION);
    }
    // len=0 或未页对齐 → E_INVALID_ADDR
    if len == 0 || len % FRAME != 0 {
        return Err(E_INVALID_ADDR);
    }
    Ok(())
}

/// 区间 [start, end) 是否与既有 VMA 重叠（单临界区读表）。
fn overlaps_vma(start: u64, end: u64) -> bool {
    demand_with_table(|t| t.iter().any(|r| r.start < end && start < r.end))
}

/// eager 映射 [start, start+len)：逐页 alloc+清零+map。
/// 失败回滚已映射页并返回错误码。
fn eager_map(as_user: &mut AddressSpace, start: u64, len: u64, prot: u32) -> Result<(), i64> {
    let pt = prot_to_pt(prot);
    let mut mapped = 0u64;
    for i in (0..len).step_by(FRAME as usize) {
        let va = start + i;
        let Some(frame) = alloc_frame() else {
            rollback(as_user, start, mapped);
            return Err(E_NO_MEMORY);
        };
        // SAFETY: frame 为刚分配的独占物理页（内核 VA==PA 恒等映射）。
        unsafe { core::ptr::write_bytes(frame as *mut u8, 0, FRAME_SIZE) };
        if let Err(e) = as_user.map_page(va, frame, pt) {
            free_frame(frame);
            rollback(as_user, start, mapped);
            // 撞已映射页（ELF 段/栈/重复 addr）→ 地址非法；PT 帧耗尽 → 内存不足
            return Err(match e {
                MapError::OutOfFrames => E_NO_MEMORY,
                _ => E_INVALID_ADDR,
            });
        }
        mapped += FRAME;
    }
    Ok(())
}

/// 回滚 [start, start+mapped)：unmap + free。
fn rollback(as_user: &mut AddressSpace, start: u64, mapped: u64) {
    for i in (0..mapped).step_by(FRAME as usize) {
        if let Ok(pa) = as_user.unmap_page(start + i) {
            free_frame(pa);
        }
    }
}

/// `mmap(addr, len, prot, flags)` syscall 实现。返回映射 VA（>0）或负错误码。
///
/// # Safety
/// `as_ptr` 必须指向当前激活的用户 [`AddressSpace`]（syscall 分发层从
/// `elfload::current_as_ptr` 取得；单核 MVP 契约）。
pub unsafe fn sys_mmap(as_ptr: *mut AddressSpace, addr: u64, len: u64, prot: u32, flags: u32) -> i64 {
    if let Err(e) = validate(prot, flags, len) {
        MMAP_ERR.fetch_add(1, Ordering::SeqCst);
        log::warn!("[umem] mmap(addr={addr:#x}, len={len:#x}, prot={prot:#x}, flags={flags:#x}) -> {e}");
        return e;
    }

    // 选址：addr=0 → bump；addr!=0 → 对齐 + 窗口 + 重叠校验
    let mut st = STATE.lock();
    let start = if addr == 0 {
        let s = st.bump;
        match s.checked_add(len) {
            Some(e) if e <= USER_REGION_END => {
                st.bump = e;
                s
            }
            _ => {
                drop(st);
                MMAP_ERR.fetch_add(1, Ordering::SeqCst);
                return E_NO_MEMORY;
            }
        }
    } else {
        if addr % FRAME != 0
            || addr < USER_REGION_START
            || match addr.checked_add(len) {
                Some(e) if e <= USER_REGION_END => false,
                _ => true,
            }
        {
            drop(st);
            MMAP_ERR.fetch_add(1, Ordering::SeqCst);
            return E_INVALID_ADDR;
        }
        addr
    };
    if overlaps_vma(start, start + len) {
        // bump 指针不回退（空洞留给 cleanup_all 统一重置）
        drop(st);
        MMAP_ERR.fetch_add(1, Ordering::SeqCst);
        return E_INVALID_ADDR;
    }
    st.active.push(Region { start, len, prot, flags });
    drop(st);

    // P4-T9e quota 计费：在 ACTIVE 表登记后、eager_map 前 charge Pages
    // （Doc 02 §5.5）。失败 → 撤 ACTIVE 记录、释放页（尚未分配 = no-op）、
    // 返 E_QUOTA_EXCEEDED。注：cap lookup 表外（init / 没 PROC_EXT 条目）
    // 静默跳过 charge——集成 smoke 与 boot 路径契约。
    let pages = (len / FRAME) as u32;
    let pid_raw = crate::proc_ext::current_pid();
    let pid = Pid(pid_raw);
    let charged = if pid_raw != 0 {
        match crate::kstate::k_quota_charge(pid, Resource::Pages, pages) {
            Ok(()) => true,
            Err(_) => {
                crate::proc_life::note_quota_denied();
                STATE.lock().active.retain(|r| r.start != start);
                MMAP_ERR.fetch_add(1, Ordering::SeqCst);
                log::warn!(
                    "[umem] mmap quota exceeded: pid={} pages={} (region {:#x}..{:#x})",
                    pid_raw, pages, start, start + len
                );
                return E_QUOTA_EXCEEDED;
            }
        }
    } else {
        false
    };

    // SAFETY: 调用方契约——as_ptr 指向存活的激活 AS。
    let as_user = unsafe { &mut *as_ptr };
    if let Err(e) = eager_map(as_user, start, len, prot) {
        // 映射失败：撤 ACTIVE 记录 + 释放已 charge 的页配额（VMA 未登记无需撤）
        STATE.lock().active.retain(|r| r.start != start);
        if charged {
            crate::kstate::k_quota_release(pid, Resource::Pages, pages);
        }
        MMAP_ERR.fetch_add(1, Ordering::SeqCst);
        log::warn!("[umem] mmap eager_map failed at {start:#x} -> {e}");
        return e;
    }

    // VMA 登记（权限记录 + munmap 后 kill 闭环）。重叠已预检，insert 失败
    // 属竞争异常——单核 syscall 串行下不应发生，发生即回滚映射 + 释放配额
    // + 撤 ACTIVE 记录并报 -2。
    let region = UserMemoryRegion::new(start, start + len, prot_to_flags(prot, flags), RegionKind::Mapped);
    if let Err(e) = demand_register(region) {
        rollback(as_user, start, len);
        STATE.lock().active.retain(|r| r.start != start);
        if charged {
            crate::kstate::k_quota_release(pid, Resource::Pages, pages);
        }
        MMAP_ERR.fetch_add(1, Ordering::SeqCst);
        log::error!("[umem] VMA register failed: {e:?} at {start:#x}");
        return E_INVALID_ADDR;
    }

    MMAP_OK.fetch_add(1, Ordering::SeqCst);
    info!(
        "[umem] mmap [{start:#x}..{:#x}) prot={prot:#x} flags={flags:#x} -> eager-mapped {} pages",
        start + len,
        len / FRAME
    );
    start as i64
}

/// `munmap(addr, len)` syscall 实现。成功返回 0，失败返回负错误码。
///
/// # Safety
/// 同 [`sys_mmap`]。
pub unsafe fn sys_munmap(as_ptr: *mut AddressSpace, addr: u64, len: u64) -> i64 {
    if len == 0 || len % FRAME != 0 || addr % FRAME != 0 {
        MUNMAP_ERR.fetch_add(1, Ordering::SeqCst);
        return E_INVALID_ADDR;
    }
    // 精确匹配 ACTIVE 区域（部分解除不支持 → E_NOT_FOUND）
    let mut st = STATE.lock();
    let hit = st
        .active
        .iter()
        .position(|r| r.start == addr && r.len == len);
    let Some(idx) = hit else {
        drop(st);
        MUNMAP_ERR.fetch_add(1, Ordering::SeqCst);
        log::warn!("[umem] munmap({addr:#x}, {len:#x}) -> {E_NOT_FOUND} (no exact ACTIVE region)");
        return E_NOT_FOUND;
    };
    st.active.remove(idx);
    drop(st);

    // 逐页 unmap + free（eager 映射 ⇒ 每页必 present）
    // SAFETY: 调用方契约——as_ptr 指向存活的激活 AS。
    let as_user = unsafe { &mut *as_ptr };
    for i in (0..len).step_by(FRAME as usize) {
        match as_user.unmap_page(addr + i) {
            Ok(pa) => free_frame(pa),
            Err(e) => log::error!("[umem] munmap unmap_page({:#x}) failed: {e:?}", addr + i),
        }
    }
    // VMA 注销（此后访问该区间 → "no VMA" → kill 骨架，语义闭环）
    demand_unregister(addr);

    // P4-T9e：释放已 charge 的 Pages 配额（仅当 caller 是带 PROC_EXT 的进程）
    let pid_raw = crate::proc_ext::current_pid();
    if pid_raw != 0 {
        crate::kstate::k_quota_release(
            Pid(pid_raw),
            Resource::Pages,
            (len / FRAME) as u32,
        );
    }

    MUNMAP_OK.fetch_add(1, Ordering::SeqCst);
    info!("[umem] munmap [{addr:#x}..{:#x}) -> freed {} pages", addr + len, len / FRAME);
    0
}

/// 进程清理路径：解除全部 ACTIVE 区域（unmap+free+VMA 注销）并重置 arena。
///
/// `elf_continuation` 在 FR8 断言前调用（hello 自律 munmap 时为 no-op 兜底）。
///
/// # Safety
/// 同 [`sys_mmap`]；调用时 CR3 可以已切回内核 AS（unmap 走页表帧 PA，
/// 与当前 CR3 无关）。
pub unsafe fn cleanup_all(as_ptr: *mut AddressSpace) {
    let regions: Vec<Region> = {
        let mut st = STATE.lock();
        st.bump = MMAP_ARENA_BASE;
        core::mem::take(&mut st.active)
    };
    if regions.is_empty() {
        return;
    }
    // SAFETY: 调用方契约。
    let as_user = unsafe { &mut *as_ptr };
    let mut freed = 0u64;
    let mut total_pages: u64 = 0;
    for r in &regions {
        for i in (0..r.len).step_by(FRAME as usize) {
            if let Ok(pa) = as_user.unmap_page(r.start + i) {
                free_frame(pa);
                freed += 1;
            }
        }
        demand_unregister(r.start);
        total_pages += r.len / FRAME;
    }
    // P4-T9e：批量释放总页配额（terminate_current 路径；caller = child pid，
    // PROC_EXT 仍有效，current_pid 在 cleanup 时尚未重置）。
    let pid_raw = crate::proc_ext::current_pid();
    if pid_raw != 0 && total_pages > 0 {
        crate::kstate::k_quota_release(
            Pid(pid_raw),
            Resource::Pages,
            total_pages as u32,
        );
    }
    info!(
        "[umem] cleanup_all: {} regions, {freed} pages unmapped+freed, released {total_pages} page quota",
        regions.len()
    );
}
