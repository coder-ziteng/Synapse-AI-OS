//! 内核 4KB 页表模块 + 地址空间抽象（P4-T2）。
//!
//! ## 设计
//!
//! boot.S 已用 2MB 大页恒等映射 0-4GB（supervisor-only，PD0=0-1GB / PD1=1-2GB…
//! 实际 PD0/PD1 各 1GB+1GB=0-2GB，PD1 覆盖 2-4GB 段在 PDPT[1]）。本模块在其上
//! 提供**每进程地址空间**：
//!
//! ```text
//! 新 PML4（每 AddressSpace 一帧）
//!  └─ [0] → 新 PDPT（每 AS 一帧）
//!      ├─ [0] → boot PD0（共享）：0-1GB 内核恒等映射，2MB 大页，supervisor-only
//!      │        → 用户页表中内核区不可访问（Doc 02 §3.2；无 US 位，首期不做 KPTI）
//!      └─ [1] → 新 user PD（每 AS 一帧，初始全零）：1-2GB 用户 VA 区
//!               └─ 按需挂 4KB PT（map_page 惰性分配）
//! ```
//!
//! **用户 VA 区 = [1GB, 2GB)**：基址决策见 task.json decision_log 2026-09-27
//! 改址条目（原 Doc 02 §3.1 PROPOSED 0x400000 与内核镜像 PA [0x200000,0x4cb000)
//! 同 VA 冲突不可调和；1GB 处 PA 无物理内存，PDPT entry 粒度整区独占零拆分）。
//!
//! **恒等映射假设**：内核 VA == PA（boot.S 建立），页表帧直接用物理地址解引用。
//! 内核高半迁移后此处需引入 offset 转换（Doc 02 §3.1 远期方向）。
//!
//! **TLB 纪律**：无 PCID 基线（Doc 02 §3.2）——`mov cr3` 全量刷（非 global 页），
//! unmap 单页走 `invlpg`。PCID/INVPCID 是 P4-T12 可选优化。
//!
//! **NX**：boot.S 只开了 EFER.LME 未开 NXE——NX 位（bit63）在 NXE=0 时是保留位，
//! 写入会导致页表加载 #GP。[`enable_nxe`] 幂等开启，smoke 首步调用。
//!
//! ## expected-fault 钩子（仅 smoke/测试用）
//!
//! #PF handler（idt.rs）默认 panic。smoke 需要"故意踩未映射页并活下来"：
//! [`probe_expect_pf`] 先武装 `PF_RESUME_RIP`（恢复点 = 探测指令下一条），
//! 再执行探测读；#PF 陷入后 `page_fault_inner` 经 [`take_expected_fault`]
//! 消费钩子返回恢复 RIP，idt.rs 的 trampoline 改写栈帧 RIP 后 iretq 续跑。
//! 未武装的 #PF 一律照旧 panic。

use alloc::vec::Vec;
use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};
use log::info;

use crate::page_frame::{alloc_frame, free_frame, with_page_frames, PhysicalAddr, FRAME_SIZE};

// ---------------------------------------------------------------------------
// 页表项位（AMD64 Manual Vol.2 §5 Paging）
// ---------------------------------------------------------------------------

/// P：present。
pub const PT_PRESENT: u64 = 1 << 0;
/// R/W：可写。
pub const PT_WRITABLE: u64 = 1 << 1;
/// U/S：用户可访问（各级 AND 语义：任一级 0 即 supervisor-only）。
pub const PT_USER: u64 = 1 << 2;
/// A：accessed（CPU 置位）。
pub const PT_ACCESSED: u64 = 1 << 5;
/// D：dirty（CPU 置位，仅叶级）。
pub const PT_DIRTY: u64 = 1 << 6;
/// PS：大页（PD 级 = 2MB，PDPT 级 = 1GB）。
pub const PT_HUGE: u64 = 1 << 7;
/// NX：不可执行（需 EFER.NXE=1，否则为保留位）。
pub const PT_NX: u64 = 1 << 63;
/// 页表项物理地址掩码（bits 12..=51）。
pub const PT_ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
/// 2MB 大页物理基址掩码（bits 21..=51）。
const HUGE2M_ADDR_MASK: u64 = 0x000F_FFFF_FFE0_0000;

/// 用户 VA 区起点（含）：PDPT[0] entry 1 所辖 1GB 区。
pub const USER_REGION_START: u64 = 0x4000_0000;
/// 用户 VA 区终点（不含）。
pub const USER_REGION_END: u64 = 0x8000_0000;

/// map/unmap 错误码。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MapError {
    /// VA 不在用户区 [1GB, 2GB)（内核区共享只读，不允许经此接口映射）。
    OutOfUserRegion,
    /// VA 或 PA 未 4KB 对齐。
    Unaligned,
    /// 目标 VA 已有 present 映射。
    AlreadyMapped,
    /// 目标 VA 无 present 映射（unmap/translate 失败）。
    NotMapped,
    /// 页帧耗尽（中间级 PT 分配失败）。
    OutOfFrames,
}

// ---------------------------------------------------------------------------
// 底层原语（恒等映射：页表帧 PA 直接解引用）
// ---------------------------------------------------------------------------

/// 读页表帧第 `idx` 项（volatile：绕过 Rust 别名/优化假设）。
unsafe fn entry_read(table_phys: PhysicalAddr, idx: usize) -> u64 {
    (table_phys as *const u64).add(idx).read_volatile()
}

/// 写页表帧第 `idx` 项。
unsafe fn entry_write(table_phys: PhysicalAddr, idx: usize, val: u64) {
    (table_phys as *mut u64).add(idx).write_volatile(val);
}

/// 分配并清零一个 4KB 帧（页表用）。
fn zeroed_frame() -> Option<PhysicalAddr> {
    let f = alloc_frame()?;
    unsafe { core::ptr::write_bytes(f as *mut u8, 0, FRAME_SIZE) };
    Some(f)
}

/// 读当前 CR3 的 PML4 物理地址（掩掉标志位）。
pub fn cr3_read() -> PhysicalAddr {
    let v: u64;
    unsafe { asm!("mov {}, cr3", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v & PT_ADDR_MASK
}

/// 切 CR3（全量刷 TLB —— 无 PCID 正确性基线，Doc 02 §3.2）。
///
/// # Safety
/// 新 PML4 必须有效且**包含当前正在执行的内核代码/栈/数据的映射**
/// （本模块 `AddressSpace::new` 共享 boot PD0 即满足）。
pub unsafe fn cr3_write(pml4_phys: PhysicalAddr) {
    asm!("mov cr3, {}", in(reg) pml4_phys, options(nostack, preserves_flags));
}

/// 单页 TLB 失效。
unsafe fn invlpg(va: u64) {
    asm!("invlpg [{}]", in(reg) va, options(nostack, preserves_flags));
}

/// 幂等开启 EFER.NXE（NX 位生效前提）。返回 `true` = 本次调用前已开启。
///
/// # Safety
/// 写 MSR 需 CPL=0。开启后既有页表不受影响（boot 页表无 NX 位，开启合法）。
pub unsafe fn enable_nxe() -> bool {
    const MSR_EFER: u32 = 0xC000_0080;
    const NXE_BIT: u32 = 1 << 11;
    let lo: u32;
    let hi: u32;
    asm!("rdmsr", in("ecx") MSR_EFER, out("eax") lo, out("edx") hi, options(nomem, nostack));
    if lo & NXE_BIT != 0 {
        return true;
    }
    asm!("wrmsr", in("ecx") MSR_EFER, in("eax") lo | NXE_BIT, in("edx") hi, options(nostack));
    false
}

// ---------------------------------------------------------------------------
// expected-fault 钩子（#PF smoke 专用；消费方 = idt.rs page_fault_inner）
// ---------------------------------------------------------------------------

/// 武装中的恢复 RIP（0 = 未武装）。#PF handler swap 走即解除（单次有效）。
static PF_RESUME_RIP: AtomicU64 = AtomicU64::new(0);
/// handler 捕获的 CR2。
static PF_CAUGHT_ADDR: AtomicU64 = AtomicU64::new(0);
/// CR2 有效标志（区分 CR2==0 的 NULL 探测）。
static PF_CAUGHT_VALID: AtomicU64 = AtomicU64::new(0);

/// idt.rs #PF 路径回调：消费 expected-fault 钩子。
///
/// 返回非 0 = 期望内缺页，trampoline 据此改写栈帧 RIP 续跑；
/// 返回 0 = 非期望缺页（handler 走 panic）。同时记录 CR2 供断言。
pub(crate) fn take_expected_fault(cr2: u64) -> u64 {
    let resume = PF_RESUME_RIP.swap(0, Ordering::SeqCst);
    if resume != 0 {
        PF_CAUGHT_ADDR.store(cr2, Ordering::SeqCst);
        PF_CAUGHT_VALID.store(1, Ordering::SeqCst);
    }
    resume
}

/// 探测 `addr`：期望触发 #PF 并被钩子接住，返回捕获的 CR2。
///
/// 返回 `None` = 未发生缺页（addr 竟是映射好的——调用方断言失败）。
///
/// # Safety
/// * `addr` 必须是允许缺页的探测地址（smoke 场景：用户区未映射 VA）；
/// * 探测期间（武装→缺页→恢复）不得有其它路径触发 #PF——单核 + 内核区
///   全映射下成立；IRQ handler 自身缺页会误消费钩子（MVP 接受，记录在案）；
/// * 不得在持有自旋锁临界区内调用（handler 内 log 走串口）。
pub unsafe fn probe_expect_pf(addr: u64) -> Option<u64> {
    PF_CAUGHT_VALID.store(0, Ordering::SeqCst);
    PF_CAUGHT_ADDR.store(0, Ordering::SeqCst);
    // 序列：算恢复点（下一条指令地址）→ 武装 → 故意读 [addr]。
    // 缺页则 handler 把栈帧 RIP 改成恢复点续跑；不缺页则直接落到 2:。
    asm!(
        "lea rax, [rip + 2f]",
        "mov [{slot}], rax",
        "mov rax, [{a}]", // 探测读（结果弃用）
        "2:",
        slot = in(reg) &PF_RESUME_RIP as *const AtomicU64 as u64,
        a = in(reg) addr,
        out("rax") _,
        options(nostack),
    );
    let leftover = PF_RESUME_RIP.swap(0, Ordering::SeqCst);
    if leftover != 0 {
        // 钩子未被消费 = 没有发生缺页；顺手解除武装
        return None;
    }
    if PF_CAUGHT_VALID.load(Ordering::SeqCst) == 1 {
        Some(PF_CAUGHT_ADDR.load(Ordering::SeqCst))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// AddressSpace
// ---------------------------------------------------------------------------

/// 每进程地址空间：一个 PML4 + 私有 PDPT/user-PD，共享内核 0-1GB 恒等映射。
///
/// 所有页表帧由 page_frame 分配器出账（FR8 核算），Drop 时归还。
/// **不**拥有映射进去的用户数据帧——那是调用方（P4-T3 VMA / T5 ELF 加载器）
/// 的账目；`unmap_page` 返回 PA 由调用方决定 free。
pub struct AddressSpace {
    pml4: PhysicalAddr,
    pdpt: PhysicalAddr,
    user_pd: PhysicalAddr,
    /// map_page 惰性分配的 4KB PT 中间页（Drop 时全部归还）。
    owned_tables: Vec<PhysicalAddr>,
}

impl AddressSpace {
    /// 新建地址空间（3 帧固定开销 + 每 2MB 用户区 1 帧 PT 惰性开销）。
    ///
    /// 前置：boot 页表链有效（CR3 → PML4[0] → PDPT[0] → PD0 恒等映射 0-1GB）。
    /// 返回 `None` = 页帧耗尽。
    pub fn new() -> Option<Self> {
        let pml4 = zeroed_frame()?;
        let pdpt = zeroed_frame()?;
        let user_pd = zeroed_frame()?;

        // 从当前 CR3 链取 boot PD0 物理页（共享内核映射，不复制内容）
        let boot_pml4 = cr3_read();
        let boot_pdpt = unsafe { entry_read(boot_pml4, 0) } & PT_ADDR_MASK;
        let boot_pd0 = unsafe { entry_read(boot_pdpt, 0) } & PT_ADDR_MASK;

        unsafe {
            // PML4[0] → 私有 PDPT（supervisor：本 AS 全部低半映射经此）
            entry_write(pml4, 0, pdpt | PT_PRESENT | PT_WRITABLE);
            // PDPT[0] → boot PD0 共享：0-1GB 内核恒等映射，supervisor-only
            // （无 PT_USER → 用户态访问内核区必 #PF，Doc 02 §3.2 要求）
            entry_write(pdpt, 0, boot_pd0 | PT_PRESENT | PT_WRITABLE);
            // PDPT[1] → 私有 user PD：1-2GB 用户区（US 位下放到叶级控制）
            entry_write(pdpt, 1, user_pd | PT_PRESENT | PT_WRITABLE | PT_USER);
        }
        Some(Self { pml4, pdpt, user_pd, owned_tables: Vec::new() })
    }

    /// 本 AS 的 PML4 物理地址（= 激活时的 CR3 值）。
    pub fn pml4_phys(&self) -> PhysicalAddr {
        self.pml4
    }

    /// 激活本 AS（切 CR3，全量刷 TLB）。
    ///
    /// # Safety
    /// 调用后当前 CPU 立即用新页表——本 AS 必须共享内核映射（`new` 保证）。
    /// 中断/抢占开启时切换须由调度器串行化（P4-T4 接线前 smoke 场景单线程安全）。
    pub unsafe fn activate(&self) {
        cr3_write(self.pml4);
    }

    /// 映射用户页：`va`（必须在 [1GB, 2GB)）→ `pa`，叶级权限 = `flags | PT_PRESENT`。
    ///
    /// 中间级 PT 惰性分配（US|RW）。调用方负责 `flags` 含 PT_USER（用户可访问）
    /// 与 PT_NX 策略（数据页 NX、代码页无 NX——W^X 由 P4-T5 加载器按段强制）。
    pub fn map_page(
        &mut self,
        va: u64,
        pa: PhysicalAddr,
        flags: u64,
    ) -> Result<(), MapError> {
        if va % FRAME_SIZE as u64 != 0 || pa % FRAME_SIZE as u64 != 0 {
            return Err(MapError::Unaligned);
        }
        if !(USER_REGION_START..USER_REGION_END).contains(&va) {
            return Err(MapError::OutOfUserRegion);
        }
        let pd_idx = (((va - USER_REGION_START) >> 21) & 0x1FF) as usize;
        let pt_idx = ((va >> 12) & 0x1FF) as usize;

        let pt_phys = unsafe {
            let e = entry_read(self.user_pd, pd_idx);
            if e & PT_PRESENT != 0 {
                e & PT_ADDR_MASK
            } else {
                let pt = zeroed_frame().ok_or(MapError::OutOfFrames)?;
                entry_write(
                    self.user_pd,
                    pd_idx,
                    pt | PT_PRESENT | PT_WRITABLE | PT_USER,
                );
                self.owned_tables.push(pt);
                pt
            }
        };
        unsafe {
            if entry_read(pt_phys, pt_idx) & PT_PRESENT != 0 {
                return Err(MapError::AlreadyMapped);
            }
            entry_write(pt_phys, pt_idx, pa | flags | PT_PRESENT);
        }
        Ok(())
    }

    /// 解除用户页映射，返回原 PA（帧归调用方处置）。invlpg 即时生效。
    pub fn unmap_page(&mut self, va: u64) -> Result<PhysicalAddr, MapError> {
        if va % FRAME_SIZE as u64 != 0 {
            return Err(MapError::Unaligned);
        }
        if !(USER_REGION_START..USER_REGION_END).contains(&va) {
            return Err(MapError::OutOfUserRegion);
        }
        let pd_idx = (((va - USER_REGION_START) >> 21) & 0x1FF) as usize;
        let pt_idx = ((va >> 12) & 0x1FF) as usize;
        unsafe {
            let pd_e = entry_read(self.user_pd, pd_idx);
            if pd_e & PT_PRESENT == 0 {
                return Err(MapError::NotMapped);
            }
            let pt_phys = pd_e & PT_ADDR_MASK;
            let e = entry_read(pt_phys, pt_idx);
            if e & PT_PRESENT == 0 {
                return Err(MapError::NotMapped);
            }
            entry_write(pt_phys, pt_idx, 0);
            invlpg(va);
            Ok(e & PT_ADDR_MASK)
        }
    }

    /// VA → PA 翻译（4 级走查；PD 级支持 2MB 大页——内核共享区即大页）。
    /// 任一级 !present → `None`。不检查权限位（那是 CPU/#PF 的职责）。
    pub fn translate(&self, va: u64) -> Option<PhysicalAddr> {
        unsafe {
            let e0 = entry_read(self.pml4, ((va >> 39) & 0x1FF) as usize);
            if e0 & PT_PRESENT == 0 {
                return None;
            }
            let e1 = entry_read(e0 & PT_ADDR_MASK, ((va >> 30) & 0x1FF) as usize);
            if e1 & PT_PRESENT == 0 {
                return None;
            }
            let e2 = entry_read(e1 & PT_ADDR_MASK, ((va >> 21) & 0x1FF) as usize);
            if e2 & PT_PRESENT == 0 {
                return None;
            }
            if e2 & PT_HUGE != 0 {
                // 2MB 大页：物理基址 bits21..51 + 页内偏移 bits0..20
                return Some((e2 & HUGE2M_ADDR_MASK) | (va & 0x1F_FFFF));
            }
            let e3 = entry_read(e2 & PT_ADDR_MASK, ((va >> 12) & 0x1FF) as usize);
            if e3 & PT_PRESENT == 0 {
                return None;
            }
            Some((e3 & PT_ADDR_MASK) | (va & 0xFFF))
        }
    }
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        // 前提：本 AS 不是当前激活页表（调用方先切回旧 CR3）。
        debug_assert_ne!(cr3_read(), self.pml4, "drop active AddressSpace");
        for pt in self.owned_tables.drain(..) {
            free_frame(pt);
        }
        free_frame(self.user_pd);
        free_frame(self.pdpt);
        free_frame(self.pml4);
    }
}

// ---------------------------------------------------------------------------
// 真机 smoke（P4-T2 verify：新建 AS → CR3 切换 → 内核继续跑 → 用户页读写 →
// 未映射访问 #PF；+ FR8 页帧账本归零）
// ---------------------------------------------------------------------------

/// 宏：断言并计数（失败即 panic 走 backtrace + 355 出口）。
macro_rules! check {
    ($total:ident, $label:expr, $cond:expr) => {{
        if !($cond) {
            panic!("[paging-smoke] FAIL: {}", $label);
        }
        $total += 1;
        info!("[paging-smoke]   ok: {}", $label);
    }};
}

/// P4-T2 真机 smoke。在 `_start64` 的 kthread smoke 之后调用。
pub fn paging_smoke() {
    info!("[paging-smoke] start");
    let mut total: u32 = 0;

    let used_pre = with_page_frames(|a| a.used_frames());

    // 0. EFER.NXE：NX 位合法性前提（boot.S 只开了 LME）
    let nxe_was_on = unsafe { enable_nxe() };
    info!("[paging-smoke] EFER.NXE on (already was: {nxe_was_on})");

    // 1. 新建 AddressSpace（3 帧 + 惰性 PT）
    let mut as1 = AddressSpace::new().expect("[paging-smoke] frames for AddressSpace");
    check!(
        total,
        "AS frames 4K-aligned & distinct",
        [as1.pml4, as1.pdpt, as1.user_pd].iter().all(|&f| f % 4096 == 0)
            && as1.pml4 != as1.pdpt
            && as1.pdpt != as1.user_pd
            && as1.pml4 != as1.user_pd
    );

    // 2. 未激活即可翻译内核共享区（大页路径）：恒等映射
    check!(
        total,
        "kernel VA 0x200000 translates via shared PD0 (2MB huge)",
        as1.translate(0x20_0000) == Some(0x20_0000)
    );
    check!(
        total,
        "user region empty before map",
        as1.translate(USER_REGION_START).is_none()
    );

    // 3. 映射一个用户页（RW + US + NX：数据页 W^X）
    let f1 = alloc_frame().expect("[paging-smoke] frame f1");
    unsafe { core::ptr::write_bytes(f1 as *mut u8, 0, FRAME_SIZE) };
    as1.map_page(USER_REGION_START, f1, PT_WRITABLE | PT_USER | PT_NX)
        .expect("[paging-smoke] map_page");
    check!(
        total,
        "translate(user VA) = f1",
        as1.translate(USER_REGION_START) == Some(f1)
    );

    // 4. CR3 切换 → 内核继续运行（此后每条 log 本身就是证据：
    //    代码/栈/堆/log 静态区全在共享 0-1GB supervisor 映射里）
    let old_cr3 = cr3_read();
    unsafe { as1.activate() };
    info!(
        "[paging-smoke] CR3 {:#x} -> {:#x}; kernel alive after switch",
        old_cr3,
        as1.pml4_phys()
    );
    check!(total, "CR3 = new AS PML4", cr3_read() == as1.pml4_phys());

    // 5. 用户页读写（ring0 直访；U/S 位不限制 ring0，ring3 路径在 P4-T4）
    unsafe {
        let p = USER_REGION_START as *mut u64;
        p.write_volatile(0xDEAD_BEEF_CAFE_1234);
        check!(
            total,
            "user page write/read roundtrip",
            p.read_volatile() == 0xDEAD_BEEF_CAFE_1234
        );
    }

    // 6. 未映射用户 VA → #PF（expected-fault 钩子接住，CR2 断言）
    let unmapped = USER_REGION_START + 0x100_0000; // +16MB：user PD 范围内无 PT
    check!(
        total,
        "translate(unmapped) = None",
        as1.translate(unmapped).is_none()
    );
    let caught = unsafe { probe_expect_pf(unmapped) };
    check!(
        total,
        "#PF caught & CR2 = fault addr",
        caught == Some(unmapped)
    );

    // 7. unmap → invlpg 生效：再访问同 VA 又 #PF（TLB 无残留）
    let pa = as1.unmap_page(USER_REGION_START).expect("[paging-smoke] unmap");
    check!(total, "unmap returns f1", pa == f1);
    check!(
        total,
        "#PF again after unmap (invlpg flushed TLB)",
        unsafe { probe_expect_pf(USER_REGION_START) } == Some(USER_REGION_START)
    );

    // 8. 错误路径：内核区拒绝 / 重复映射拒绝 / 未对齐拒绝
    check!(
        total,
        "map into kernel region rejected (OutOfUserRegion)",
        as1.map_page(0x20_0000, f1, PT_WRITABLE) == Err(MapError::OutOfUserRegion)
    );
    as1.map_page(USER_REGION_START, f1, PT_WRITABLE | PT_USER)
        .expect("[paging-smoke] remap");
    check!(
        total,
        "double map rejected (AlreadyMapped)",
        as1.map_page(USER_REGION_START, f1, PT_WRITABLE | PT_USER)
            == Err(MapError::AlreadyMapped)
    );
    check!(
        total,
        "unaligned map rejected",
        as1.map_page(USER_REGION_START + 1, f1, PT_WRITABLE | PT_USER)
            == Err(MapError::Unaligned)
    );
    as1.unmap_page(USER_REGION_START).expect("[paging-smoke] unmap 2");

    // 9. 切回旧 CR3 + 清理 + FR8 页帧账本归零
    unsafe { cr3_write(old_cr3) };
    check!(total, "old CR3 restored", cr3_read() == old_cr3);
    drop(as1);
    free_frame(f1);
    let used_post = with_page_frames(|a| a.used_frames());
    check!(
        total,
        "FR8 ledger balanced (used frames pre == post)",
        used_post == used_pre
    );

    info!("[paging-smoke] {}/{} checks passed", total, total);
}
