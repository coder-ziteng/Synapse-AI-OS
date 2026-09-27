//! per-process 内核扩展（P4-T9a）。
//!
//! `synapse-proc::ProcessTable` 是 `#![deny(unsafe_code)]` 纯逻辑 Pcb，
//! 不能直接放裸指针 / per-process kstack 元数据。本模块作为 Pcb 的 sibling，
//! 持有 Pcb 之外的"内核侧实际资源"（kstack 地址、VMA 表指针、AS 等）。
//!
//! ## PerCpu 与 PROC_EXT 关系
//!
//! - `PerCpu.current_pid` (gs:[0])：当前正在执行 / 即将 iretq 的用户进程 pid
//!   （IPC smoke 用合成 kthread ID 当 tag，不一定有 PROC_EXT 条目）
//! - `PerCpu.kstack_top` (gs:[8])：该 pid 的 kstack_top = `PROC_EXT[pid].kstack_top`
//!
//! 两个字段**解耦更新**：
//! - `set_current_pid(u32)` 只写 gs:[0]（context tag 用，无 kstack 校验）
//! - `switch_to_process(pid: Pid)` 同时写两个（要求 PROC_EXT 有条目，**真实**
//!   进程切换时由调用方负责）
//!
//! ## 锁顺序
//!
//! PROC_EXT 不与 kstate 任何锁嵌套（per-process 局部数据，调度路径仅在
//! 关中断临界区内访问）。

use core::cell::UnsafeCell;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::kstack;
use crate::sync::SpinLock;
use synapse_proc::process::{Pid, MAX_PROCS};

/// per-process 内核侧扩展条目。
#[derive(Clone, Copy)]
pub struct ProcExtEntry {
    /// kstack 最低物理地址
    pub kstack_bottom: u64,
    /// kstack 顶部地址（栈顶 push 起点，16 字节对齐）
    pub kstack_top: u64,
    /// 用户 AddressSpace 裸指针（`Box<AddressSpace>` 泄漏形态；0 = 未装载）。
    /// spawn 装载 ELF 时写入；terminate 路径读取并回收（P4-T9c）。
    pub user_as_ptr: u64,
}

impl ProcExtEntry {
    /// 空条目占位（`[const { None }; MAX_PROCS]` 不可直接 derive Default）。
    pub const fn empty() -> Self {
        Self { kstack_bottom: 0, kstack_top: 0, user_as_ptr: 0 }
    }
}

/// 全局 PROC_EXT 表（per-process kstack 元数据）。`None` = 该 pid 未分配 kstack。
static PROC_EXT: SpinLock<[Option<ProcExtEntry>; MAX_PROCS]> =
    SpinLock::new([const { None }; MAX_PROCS]);

// ---------------------------------------------------------------------------
// PerCpu 数据（IA32_KERNEL_GS_BASE → &PER_CPU）
//
// 布局（repr(C, align(16))）：
//   offset 0:  current_pid (u32)        — asm gs:[0]
//   offset 4:  _pad (u32)
//   offset 8:  kstack_top (u64)         — asm gs:[8]
// ---------------------------------------------------------------------------

#[repr(C, align(16))]
struct PerCpu {
    current_pid: u32,
    _pad: u32,
    kstack_top: u64,
}

struct PerCpuWrap(UnsafeCell<PerCpu>);
unsafe impl Sync for PerCpuWrap {}

static PER_CPU: PerCpuWrap = PerCpuWrap(UnsafeCell::new(PerCpu {
    current_pid: 0,
    _pad: 0,
    kstack_top: 0,
}));

fn per_cpu_addr() -> u64 {
    &PER_CPU.0 as *const UnsafeCell<PerCpu> as u64
}

static PROC_EXT_INIT_DONE: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// 启动初始化（boot 链路调一次，gdt 之后 + syscall MSR 之前）
// ---------------------------------------------------------------------------

/// PROC_EXT + PerCpu 初始化。
///
/// # Safety
///
/// - 必须在 `init_gdt_tss` 之后调用（要 TSS.RSP0 = boot kstack top）；
/// - 必须在 `init_syscall` 之前调用（PerCpu 地址会被 wrmsr 写入 MSR）；
/// - 重复调用 panic。
pub unsafe fn init_proc_ext() {
    if PROC_EXT_INIT_DONE.swap(true, Ordering::SeqCst) {
        panic!("init_proc_ext called twice");
    }

    let ksp = crate::gdt::rsp0_stack_top();
    // 用 boot kstack 的 top - KSTACK_SIZE 推算 bottom（TSS.RSP0 = top - 8，
    // 所以 bottom = (top - 8) - KSTACK_SIZE + 8 = top - KSTACK_SIZE）
    let boot_kstack_bottom = ksp - kstack::KSTACK_SIZE as u64;

    // PROC_EXT[INIT_PID] = boot kstack
    {
        let mut t = PROC_EXT.lock();
        t[synapse_proc::process::INIT_PID.0 as usize] = Some(ProcExtEntry {
            kstack_bottom: boot_kstack_bottom,
            kstack_top: ksp,
            user_as_ptr: 0,
        });
    }

    // PerCpu = init
    unsafe {
        let pc = PER_CPU.0.get();
        ptr::write_volatile(core::ptr::addr_of_mut!((*pc).current_pid), synapse_proc::process::INIT_PID.0);
        ptr::write_volatile(core::ptr::addr_of_mut!((*pc)._pad), 0u32);
        ptr::write_volatile(core::ptr::addr_of_mut!((*pc).kstack_top), ksp);
    }

    log::info!(
        "[proc_ext] init: pid={} kstack=[{:#x}..{:#x}] per_cpu@{:#x}",
        synapse_proc::process::INIT_PID.0,
        boot_kstack_bottom,
        ksp,
        per_cpu_addr(),
    );
}

/// 把 PerCpu 地址交给 `init_syscall` 用于 wrmsr(IA32_KERNEL_GS_BASE)。
pub fn per_cpu_pointer() -> u64 {
    per_cpu_addr()
}

/// GS 账本归一化（P4-T7 Phase 2 修复）：确保 **IA32_KERNEL_GS_BASE == &PER_CPU**
/// （boot 约定，见 syscall.rs 模块头 swapgs 纪律）。
///
/// 不满足时（= GS.base 当前持有 &PER_CPU、KERNEL_GS_BASE 是用户值/垃圾）执行
/// 一次 `swapgs` 归位；已满足则 no-op —— **幂等**，任何上下文可安全调用。
///
/// 为什么不能盲目 swapgs：`handle_process_exit` 有两类调用方——
/// (1) syscall #21 路径：入口 swapgs 已把 GS.base 切到 &PER_CPU，需配平；
/// (2) #PF/#GP 异常 → `terminate_current` 路径：异常入口**不切 GS**，此时已是
///     boot 约定，再 swapgs 反而翻转账本（KERNEL_GS_BASE=用户值）。
/// P4-T5 的盲目配平在路径 (2) 下把账本搞反，下一个进 ring3 的进程首条
/// syscall 的入口 swapgs 装入垃圾 GS.base → `mov rsp, gs:[8]` 得到垃圾栈 →
/// 首条 push（syscall_entry_asm+0x15）fault → 异常递送再压同一垃圾栈 → #DF
/// （P4-T7 Phase 2 真机复现：crasher #PF 终止后 ipc-pong 子进程即 #DF）。
///
/// # Safety
/// 关中断上下文调用（swapgs 与后续读写非原子；timer IRQ handler 不碰 GS，
/// 实际窗口安全，但契约上要求调用方处于内核可信路径）。
pub unsafe fn ensure_kernel_gs_base() {
    const MSR_KERNEL_GS_BASE: u32 = 0xC000_0102;
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") MSR_KERNEL_GS_BASE,
            out("eax") lo,
            out("edx") hi,
            options(nostack, preserves_flags),
        );
    }
    let kgs = ((hi as u64) << 32) | lo as u64;
    if kgs != per_cpu_addr() {
        unsafe {
            core::arch::asm!("swapgs", options(nostack, preserves_flags));
        }
    }
}

// ---------------------------------------------------------------------------
// per-process kstack 分配 / 释放（spawn / reap 路径）
// ---------------------------------------------------------------------------

/// 给 pid 分配 per-process kstack 并安装到 PROC_EXT。
///
/// 失败（kstack_alloc 返 None）→ 不修改任何状态，返 `None`。
pub fn install_kstack(pid: Pid) -> Option<()> {
    let (bottom, top) = kstack::kstack_alloc()?;
    let mut t = PROC_EXT.lock();
    if t[pid.0 as usize].is_some() {
        // 已存在：调用方契约保证不会重复安装；防御性返回失败
        kstack::kstack_free(bottom);
        return None;
    }
    t[pid.0 as usize] = Some(ProcExtEntry {
        kstack_bottom: bottom,
        kstack_top: top,
        user_as_ptr: 0,
    });
    Some(())
}

/// 释放 pid 的 per-process kstack。
///
/// 调用方契约：pid 已被 `reap` 之前应先 `release_resources` 触发本调用；
/// reap 本函数内部不再校验 pid 状态机。
pub fn uninstall_kstack(pid: Pid) {
    let bottom = {
        let mut t = PROC_EXT.lock();
        match t[pid.0 as usize].take() {
            Some(e) => e.kstack_bottom,
            None => return, // 已释放或从未安装
        }
    };
    kstack::kstack_free(bottom);
}

/// 读 PROC_EXT[pid]（仅检查存在性 + 拿 kstack_top）。
///
/// 返 None = pid 无 PROC_EXT 条目（未 spawn 或已 reap）。
pub fn kstack_top_of(pid: Pid) -> Option<u64> {
    let t = PROC_EXT.lock();
    t[pid.0 as usize].map(|e| e.kstack_top)
}

// ---------------------------------------------------------------------------
// 当前进程切换（current_pid 与 kstack_top 解耦更新）
// ---------------------------------------------------------------------------

/// 写 gs:[0] = pid（只写 current_pid，不触发 kstack 校验）。
///
/// 用途：
/// - IPC smoke 用合成 kthread ID（0x90 / 0x91 等）当 context tag；
/// - 真实进程切换时由 [`switch_to_process`] 同时更新两个字段。
pub fn set_current_pid(pid: u32) {
    unsafe {
        let pc = PER_CPU.0.get();
        ptr::write_volatile(core::ptr::addr_of_mut!((*pc).current_pid), pid);
    }
}

/// 读当前 pid（asm gs:[0] 的镜像；用于 syscall dispatch 内 cap lookup 等）。
pub fn current_pid() -> u32 {
    unsafe {
        let pc = PER_CPU.0.get();
        ptr::read_volatile(core::ptr::addr_of!((*pc).current_pid))
    }
}

/// 把 PerCpu 切到 pid（更新 current_pid + kstack_top）。
///
/// 调用方契约：pid 必须有 PROC_EXT 条目（`install_kstack` 已装）；
/// 否则 panic（这是编程错误，不应静默）。
pub fn switch_to_process(pid: Pid) {
    let top = kstack_top_of(pid).expect("pid has no PROC_EXT entry");
    unsafe {
        let pc = PER_CPU.0.get();
        ptr::write_volatile(core::ptr::addr_of_mut!((*pc).current_pid), pid.0);
        ptr::write_volatile(core::ptr::addr_of_mut!((*pc).kstack_top), top);
    }
}

/// 读当前 kstack_top（诊断 / 测试用）。
pub fn current_kstack_top() -> u64 {
    unsafe {
        let pc = PER_CPU.0.get();
        ptr::read_volatile(core::ptr::addr_of!((*pc).kstack_top))
    }
}

/// 返回 PROC_EXT 槽位是否已安装 kstack（用于 reap 后断言）。
pub fn is_kstack_installed(pid: Pid) -> bool {
    let t = PROC_EXT.lock();
    t[pid.0 as usize].is_some()
}

// ---------------------------------------------------------------------------
// 用户 AddressSpace 句柄（P4-T9c）
//
// 装载形态：`Box<AddressSpace>::into_raw` 后的裸指针，由 proc_life 负责回收
// (`Box::from_raw` + drop)。slot 0 表示未装载。
// ---------------------------------------------------------------------------

/// 写入 pid 的 user AS 指针（spawn 装载 ELF 后调）。覆盖：未防御性检测旧值，
/// 调用方契约 = 该 pid 已 install_kstack 但尚未 set_user_as。
pub fn set_user_as(pid: Pid, as_ptr: u64) {
    let mut t = PROC_EXT.lock();
    if let Some(e) = t[pid.0 as usize].as_mut() {
        e.user_as_ptr = as_ptr;
    } else {
        panic!("set_user_as: pid {} has no PROC_EXT entry", pid.0);
    }
}

/// 取出 pid 的 user AS 指针（terminate 路径回收前调）；返回 0 = 未装载。
/// 调用后 slot 内 `user_as_ptr = 0`，避免双重 drop。
pub fn take_user_as(pid: Pid) -> u64 {
    let mut t = PROC_EXT.lock();
    if let Some(e) = t[pid.0 as usize].as_mut() {
        core::mem::replace(&mut e.user_as_ptr, 0)
    } else {
        0
    }
}

/// 只读查 pid 的 user AS 指针是否非零（proc_life 判断 spawned child 用）。
/// 不修改 PROC_EXT 状态。
pub fn peek_user_as(pid: Pid) -> bool {
    let t = PROC_EXT.lock();
    t[pid.0 as usize].map(|e| e.user_as_ptr != 0).unwrap_or(false)
}