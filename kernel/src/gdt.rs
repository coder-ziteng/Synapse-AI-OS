//! GDT/TSS 初始化（P2-T4）。
//!
//! ## 背景
//!
//! boot.S 的 trampoline GDT 只有 5 项（NULL + 32-bit code/data + 64-bit code/data），
//! 不含 TSS。进入长模式后，CPU 用 CS=0x18 跑 Rust 代码。
//!
//! 本模块在 `init_gdt_tss()` 建立**运行时 GDT**，替换 boot.S 的 trampoline GDT：
//!
//! - 为 P2-T5 IDT 异常处理做准备（TSS.IST1 → double fault 栈）
//! - 为 Phase 4 用户态做准备（TSS.RSP0 → syscall/sysret 内核栈）
//! - 预留 ring-3 描述符（Phase 4 启用）
//!
//! ## GDT 布局（共 8 项）
//!
//! | Index | Selector | 内容               | 说明                            |
//! |-------|----------|--------------------|---------------------------------|
//! | 0     | 0x00     | NULL               | 必选                            |
//! | 1     | 0x08     | 64-bit ring-0 code | 新规范 CS                       |
//! | 2     | 0x10     | 64-bit ring-0 data | DS/SS                           |
//! | 3     | 0x18     | 64-bit ring-0 code | **与 boot.S CS=0x18 兼容**      |
//! | 4     | 0x23     | 64-bit ring-3 data | 用户态 DS（Phase 4 启用；sel = index<<3\|3） |
//! | 5     | 0x2B     | 64-bit ring-3 code | 用户态 CS（Phase 4 启用；sel = index<<3\|3） |
//! | 6..7  | 0x30     | TSS (16 bytes)     | `ltr 0x30` 加载                 |
//!
//! **为何 index 3 必须放 64-bit ring-0 code？**
//!
//! `lgdt` 执行后到 `CS::set_reg(cs)` 之间的几条指令内，CPU 仍用旧 CS=0x18。
//! 若新 GDT[3] 不是有效 ring-0 code，NMI / #MC 等不可屏蔽中断会触发 #GP → #DF。
//! 把 64-bit ring-0 code 放 index 3 可让 CS=0x18 在切换期间始终合法；之后
//! `CS::set_reg(cs=0x08)` 将 CS 收敛到规范形式。
//!
//! ## 栈布局
//!
//! 两个 4KB 栈（静态分配）：
//!
//! - `KERNEL_STACK`：TSS.RSP0（Phase 4 P4-T4 syscall/sysret 切栈用；16KB）
//! - `DF_STACK`：TSS.IST1（double fault 异常专用栈；P2-T5 IDT 用；4KB）
//!
//! 栈顶 = 基址 + 4096（栈向低地址增长，RSP 初始指向栈顶）。

use core::ptr::addr_of;
use core::sync::atomic::{AtomicBool, Ordering};

use x86_64::instructions::segmentation::{Segment, CS, SS};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::PrivilegeLevel;
use x86_64::VirtAddr;

/// 栈大小。
/// - DF_STACK：4KB 足够（仅 #DF handler 临时使用）。
/// - KERNEL_STACK：16KB 容纳 syscall 入口保存 9 个 caller-saved + C 调用栈；
///   单核 MVP 下与 idle/boot 共用，Phase 5+ 接多线程后改为 per-thread。
const DF_STACK_SIZE: usize = 4096;
const KERNEL_STACK_SIZE: usize = 16384;

/// TSS — 用 `UnsafeCell` 包装 `static`，避免 `static mut` 的 2024 弃用警告。
///
/// SAFETY: 仅在 `init_gdt_tss` 中初始化一次，之后只读（lgdt 后 CPU 通过
/// 物理地址读 TSS 字段，Rust 侧仅 smoke 验证时读回初始值）。
struct TssCell(core::cell::UnsafeCell<TaskStateSegment>);
// SAFETY: TSS 仅在 boot 初始化 + 后续 CPU 读取；单核 MVP 下无并发。
unsafe impl Sync for TssCell {}

impl TssCell {
    const fn new() -> Self {
        TssCell(core::cell::UnsafeCell::new(TaskStateSegment::new()))
    }
    /// 初始化（仅 boot 时调用一次）。
    fn init(&self, rsp0: u64, ist1: u64) {
        // SAFETY: 单线程 boot 路径；init 前无读者。
        let tss = unsafe { &mut *self.0.get() };
        tss.privilege_stack_table[0] = VirtAddr::new(rsp0);
        tss.interrupt_stack_table[0] = VirtAddr::new(ist1);
    }
    /// 读回 TSS 指针（供 Descriptor::tss_segment_unchecked 使用）。
    fn as_ptr(&self) -> *const TaskStateSegment {
        self.0.get()
    }
    /// 读回 IST1 值（smoke 验证用）。
    fn ist1(&self) -> u64 {
        // SAFETY: smoke 在 init 之后调用；CPU 不修改 IST 字段（仅 #DF 时写入 RSP）。
        unsafe { (*self.0.get()).interrupt_stack_table[0].as_u64() }
    }
    /// 读回 RSP0 值（smoke 验证用）。
    fn rsp0(&self) -> u64 {
        unsafe { (*self.0.get()).privilege_stack_table[0].as_u64() }
    }
}

/// GDT 容器（同 TssCell 模式）。
struct GdtCell(core::cell::UnsafeCell<GlobalDescriptorTable<8>>);
unsafe impl Sync for GdtCell {}

impl GdtCell {
    const fn new() -> Self {
        GdtCell(core::cell::UnsafeCell::new(GlobalDescriptorTable::empty()))
    }
    /// 初始化（仅 boot 时调用一次）：填充描述符并 `lgdt`。
    fn init_and_load(&self, tss_ptr: *const TaskStateSegment) -> GdtSelectors {
        // SAFETY: 单线程 boot 路径；init 前无读者。
        let gdt = unsafe { &mut *self.0.get() };
        // 顺序严格对应模块头布局表。
        let cs1 = gdt.append(Descriptor::kernel_code_segment()); // [1] 0x08
        let ds2 = gdt.append(Descriptor::kernel_data_segment()); // [2] 0x10
        let _cs3 = gdt.append(Descriptor::kernel_code_segment()); // [3] 0x18 (兼容 boot.S CS)
        let ds4 = gdt.append(Descriptor::user_data_segment());   // [4] 0x20 ring-3
        let cs5 = gdt.append(Descriptor::user_code_segment());   // [5] 0x28 ring-3
        // SAFETY: tss_ptr 来自 `&'static` TSS 容器，生命周期满足。
        let tss_sel = unsafe { gdt.append(Descriptor::tss_segment_unchecked(tss_ptr)) }; // [6..7] 0x30

        debug_assert_eq!(cs1.0, 0x08, "cs64_kernel index");  // index 1, RPL=0
        debug_assert_eq!(ds2.0, 0x10, "ds64_kernel index");  // index 2, RPL=0
        debug_assert_eq!(ds4.0, 0x23, "ds64_user index");    // index 4, RPL=3 (0x20|3)
        debug_assert_eq!(cs5.0, 0x2B, "cs64_user index");    // index 5, RPL=3 (0x28|3)
        debug_assert_eq!(tss_sel.0, 0x30, "tss index");      // index 6, RPL=0

        // SAFETY: lgdt 后 GDT 必须常驻且不再修改；GdtCell 仅本函数写一次。
        unsafe {
            let gdt_ref: &'static GlobalDescriptorTable<8> = &*self.0.get();
            gdt_ref.load();
        }

        GdtSelectors {
            cs64_kernel: cs1.0,
            ds64_kernel: ds2.0,
            tss: tss_sel.0,
        }
    }
}

/// TSS 实例（`static` + `UnsafeCell`，避开 `static mut`）。
static TSS: TssCell = TssCell::new();

/// 运行时 GDT（`static` + `UnsafeCell`）。
static GDT: GdtCell = GdtCell::new();

/// 4KB 对齐封装（Rust `#[repr(align)]` 不能直接用于 static，需包一层）。
#[repr(align(4096))]
struct Aligned4K<T>(T);

/// 内核栈（TSS.RSP0；Phase 4 P4-T4 启用 syscall/sysret 时使用）。
///
/// Rust 侧只读（CPU 通过 TSS 中存的地址读写该区域）。
/// 16KB 对齐利于后续 IDT handler 用 guard page 检测栈溢出；
/// 也保证 `rsp % 16 == 8` 入口约定（push 任意奇数个寄存器后栈对齐）。
static KERNEL_STACK: Aligned4K<[u8; KERNEL_STACK_SIZE]> = Aligned4K([0; KERNEL_STACK_SIZE]);

/// Double fault 专用栈（TSS.IST1；P2-T5 IDT #DF handler 使用）。
static DF_STACK: Aligned4K<[u8; DF_STACK_SIZE]> = Aligned4K([0; DF_STACK_SIZE]);

/// 重入保护（防御性；`init_gdt_tss` 应该只调用一次）。
static INIT_DONE: AtomicBool = AtomicBool::new(false);

/// GDT 加载后各描述符选择子（供外部测试/诊断读回）。
#[derive(Clone, Copy, Debug)]
pub struct GdtSelectors {
    /// 64-bit ring-0 code（规范形式，新 GDT index 1）。
    pub cs64_kernel: u16,
    /// 64-bit ring-0 data（新 GDT index 2）。
    pub ds64_kernel: u16,
    /// TSS（新 GDT index 6，DPL=0 故 selector = 0x30）。
    pub tss: u16,
}

/// 建立并加载运行时 GDT + TSS（boot 时调用一次）。
///
/// # Safety
///
/// - 调用方必须在长模式、ring 0、关中断上下文中调用（boot 路径满足）。
/// - 重复调用会 panic（防御性检查）。
///
/// # 行为
///
/// 1. 配置 TSS.RSP0 = `KERNEL_STACK` 栈顶；TSS.IST1 = `DF_STACK` 栈顶。
/// 2. 重建 GDT（layout 见模块头注释）。
/// 3. `lgdt` 加载新 GDT。
/// 4. `CS::set_reg(0x08)` 远返回重载 CS 到规范形式。
/// 5. `SS::set_reg(0x10)` 重载 SS。
/// 6. `ltr(0x30)` 加载 TSS。
pub unsafe fn init_gdt_tss() -> GdtSelectors {
    if INIT_DONE.swap(true, Ordering::SeqCst) {
        panic!("init_gdt_tss called twice");
    }

    // ---- 1. 配置 TSS ----
    let kstack_top = addr_of!(KERNEL_STACK.0) as u64 + KERNEL_STACK_SIZE as u64;
    let dfstack_top = addr_of!(DF_STACK.0) as u64 + DF_STACK_SIZE as u64;
    TSS.init(kstack_top, dfstack_top);

    // ---- 2. 重建 GDT + lgdt ----
    let sels = GDT.init_and_load(TSS.as_ptr());

    // ---- 3. 重载 CS（far return） ----
    //    SAFETY: CS::set_reg 执行 push+retfq 重载 CS，新 GDT[1] 是合法 ring-0 code。
    unsafe {
        CS::set_reg(x86_64::structures::gdt::SegmentSelector::new(1, PrivilegeLevel::Ring0));
    }

    // ---- 4. 重载 SS ----
    unsafe {
        SS::set_reg(x86_64::structures::gdt::SegmentSelector::new(2, PrivilegeLevel::Ring0));
    }

    // ---- 5. 加载 TSS ----
    unsafe {
        load_tss(x86_64::structures::gdt::SegmentSelector::new(6, PrivilegeLevel::Ring0));
    }

    log::info!(
        "[gdt] GDT loaded: cs={:#x} ds={:#x} tss={:#x}",
        sels.cs64_kernel, sels.ds64_kernel, sels.tss
    );
    log::info!(
        "[gdt] TSS: RSP0={:#x} IST1(DF)={:#x}",
        kstack_top, dfstack_top
    );

    sels
}

/// 读回 TSS 当前 IST1 值（smoke 验证用）。
pub fn ist1_df_stack_top() -> u64 {
    TSS.ist1()
}

/// 读回 TSS 当前 RSP0 值（smoke 验证用）。
pub fn rsp0_stack_top() -> u64 {
    TSS.rsp0()
}

/// 设置 TSS.RSP0（per-thread 内核栈切换，P4-T4 进入用户态前 / 调度切换线程时调用）。
///
/// 写入值须为栈顶（最高地址），CPU 在 `syscall` / `int` 入口自动把用户 RSP 压入 [RSP0]。
///
/// # Safety
///
/// 调用方必须保证 `top` 指向一块足够大的（≥ 16KB）有效可写内核栈内存，
/// 且调用方独占该栈（单核 MVP 满足；Phase 5+ 接多线程后由调度器串行化）。
pub unsafe fn set_rsp0(top: u64) {
    // SAFETY: 单线程 boot/smoke 路径；TSS 字段被 CPU 与 Rust 共享访问，
    // 此处为唯一写入点（除 init_gdt_tss 之外）。
    unsafe { (*TSS.0.get()).privilege_stack_table[0] = VirtAddr::new(top) };
}

/// 读回 DF_STACK 基址（smoke 验证用）。
pub fn df_stack_base() -> usize {
    addr_of!(DF_STACK) as usize
}

/// 读回 KERNEL_STACK 基址（smoke 验证用）。
pub fn kernel_stack_base() -> usize {
    addr_of!(KERNEL_STACK) as usize
}
