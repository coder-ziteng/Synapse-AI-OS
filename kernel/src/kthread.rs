//! P3-T4 内核侧线程基建：kthread 创建 + 独立栈 + `CpuContext`。
//!
//! ## 分层契约
//!
//! - **调度决策**在 `synapse-sched`（纯逻辑，宿主已测 56 tests）；本模块只做
//!   硬件不可避免的部分：栈内存（页帧分配器）、初始上下文构造、全局调度器持有。
//! - 全局 [`SCHED`]：`SpinLock<Option<Scheduler>>`（IRQ-safe，与 kstate/PAGE_FRAMES
//!   同一纪律）。boot 线程经 [`kthread_init`] 收编为 0 号内核线程。
//! - [`CpuContext`] 8 字布局 = sched `ContextSlot.words[0..8]` 的约定映射
//!   （P3-T5 `switch_to` 汇编按此 push/pop callee-saved 寄存器）。
//!
//! ## 栈布局与 guard page 策略
//!
//! 每 kthread 分配 5 个连续物理页帧（恒等映射：物理地址即虚拟地址）：
//!
//! ```text
//! 帧 0        帧 1 ──────────────── 帧 4
//! [guard 4KB][栈 16KB：低地址 → 高地址增长 ↑]
//! ```
//!
//! boot.S 页表用 **2MB 大页**恒等映射 0–4GB，无法单独 unmap 一个 4KB guard
//! 页触发 #PF。MVP 策略：guard 页填充毒化模式 `0x5A5A_…`，[`kthread_reap`]
//! 时校验——栈溢出事后必被检出（记录 + panic）；真正的 #PF guard 待页表
//! 4KB 化改造（Phase 4 用户地址空间引入时顺路）。
//!
//! ## 新线程首次切换约定（P3-T5 消费）
//!
//! `kthread_create` 预置：`ctx.rsp = 栈顶 - 8`，`[ctx.rsp] = kthread_trampoline`，
//! `ctx.rip = kthread_trampoline`，`ctx.rbx = entry`。`switch_to` 统一走
//! "恢复 callee-saved + ret" 路径即可启动新线程（无需 is_fresh 特判）：
//! `ret` 弹跳板地址 → 跳板 `call rbx`（rsp 16 对齐满足 SysV）→ entry 永不返回；
//! 若返回则落入 fallback panic。

use core::arch::{asm, global_asm};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use log::{info, warn};
use synapse_sched::{
    ContextSlot, Priority, Scheduler, SchedError, SwitchDecision, ThreadId, ThreadState,
    MAX_THREADS,
};

use crate::page_frame::{self, FRAME_SIZE};
use crate::sync::SpinLock;

/// 每 kthread 栈页数（16KB，不含 guard）。
pub const KSTACK_PAGES: usize = 4;
/// 每 kthread 总占用帧数（1 guard + [`KSTACK_PAGES`] 栈）。
pub const KSTACK_FRAMES: usize = KSTACK_PAGES + 1;
/// guard 页毒化模式（事后溢出检测用，见模块头）。
const GUARD_POISON: u64 = 0x5A5A_5A5A_5A5A_5A5A;

/// CPU 上下文（callee-saved，对照 SysV AMD64 ABI）。
///
/// 字段序 = P3-T5 `switch_to` 的 push/pop 序 = sched `ContextSlot.words[0..8]`：
/// `[rbx, rbp, r12, r13, r14, r15, rsp, rip]`。
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct CpuContext {
    /// rbx（新线程约定携带 entry 函数指针）。
    pub rbx: u64,
    /// rbp（新线程置 0 → backtrace 链自然终止）。
    pub rbp: u64,
    /// r12。
    pub r12: u64,
    /// r13。
    pub r13: u64,
    /// r14。
    pub r14: u64,
    /// r15。
    pub r15: u64,
    /// rsp（指向 `ret` 弹出 rip 的位置）。
    pub rsp: u64,
    /// rip（新线程 = [`kthread_trampoline`] 地址）。
    pub rip: u64,
}

impl CpuContext {
    /// 写入 sched 的不透明上下文槽（words[0..8]，其余保留字清零）。
    pub fn to_slot(self, slot: &mut ContextSlot) {
        slot.words = [0; synapse_sched::CTX_WORDS];
        slot.words[0] = self.rbx;
        slot.words[1] = self.rbp;
        slot.words[2] = self.r12;
        slot.words[3] = self.r13;
        slot.words[4] = self.r14;
        slot.words[5] = self.r15;
        slot.words[6] = self.rsp;
        slot.words[7] = self.rip;
    }

    /// 从 sched 上下文槽读回（P3-T5 切换路径 / 调试用）。
    pub fn from_slot(slot: &ContextSlot) -> Self {
        CpuContext {
            rbx: slot.words[0],
            rbp: slot.words[1],
            r12: slot.words[2],
            r13: slot.words[3],
            r14: slot.words[4],
            r15: slot.words[5],
            rsp: slot.words[6],
            rip: slot.words[7],
        }
    }
}

/// kthread 入口函数类型（永不返回；结束时调用 [`kthread_exit_running`] 路径，
/// T5/T6 接线前由 smoke 以显式 `kthread_exit(id)` 模拟）。
pub type KthreadEntry = extern "C" fn() -> !;

/// 内核侧线程元数据（sched TCB 不持有的硬件资源信息）。
#[derive(Clone, Copy, Debug)]
struct KthreadMeta {
    /// 对应 sched ThreadId。
    id: ThreadId,
    /// 栈区起始物理地址（= 虚拟地址，恒等映射；含 guard 帧）。0 = boot 线程（静态栈）。
    stack_base: u64,
    /// 占用帧数（含 guard）。0 = boot 线程。
    frames: usize,
    /// 入口函数地址（审计/调试）。
    entry: u64,
    /// 是否为 boot 线程（收编而来，无分配栈，不可 reap 释放）。
    is_boot: bool,
}

/// 全局调度器（IRQ-safe 锁；`kthread_init` 后置 Some）。
pub static SCHED: SpinLock<Option<Scheduler>> = SpinLock::new(None);

/// 内核线程元数据表（按 `ThreadId.index()` 索引，与 sched 槽位一一对应）。
static KTHREADS: SpinLock<[Option<KthreadMeta>; MAX_THREADS]> = SpinLock::new([None; MAX_THREADS]);

const UNINIT: &str = "SCHED accessed before kthread_init()";

extern "C" {
    /// 新线程首次进入的跳板（下方 global_asm 定义）。
    fn kthread_trampoline() -> !;
    /// 上下文切换（下方 global_asm 定义，详见 [`switch_to`] 安全包装）。
    ///
    /// **返回语义**：本线程被换出后，未来某次别的线程 `switch_to` 把它选为
    /// `next` 时，从当初的调用点"返回"（callee-saved + rsp/rip 从快照恢复）。
    /// 绝不能声明为 `-> !`：那会让 rustc 把调用点之后的代码全部当死代码删除，
    /// 切换回来的线程落进编译器填充的 int3/ud2（P3-T5 首跑 #DF 的根因之一）。
    fn _switch_to(
        prev: *mut synapse_sched::ContextSlot,
        next: *const synapse_sched::ContextSlot,
    );
}

// 跳板：switch_to `ret` 至此 → `sti` → `call rbx`（entry）。entry 声明为
// `-> !`，正常永不返回；返回即 bug → fallback panic 留证。
//
// **`sti` 必须在跳板里**（P3-T6 首跑挂死的根因）：checkpoint 在 `cli` 下执行
// `_switch_to`，新线程从跳板进入时 IF 继承自切换方（=0）；且它未来的
// yield/sleep 检查点以"入口 IF 快照"恢复中断——首跑 IF=0 会被逐次 perpetuate，
// 该线程永远收不到定时器中断（phase-B 等 tick 的自旋成为死循环，PIC 的
// IRQ0 挂起位永远无人消费）。线程上下文的标准语义是 IF=1，在唯一入口
// （跳板）处一次性建立。
global_asm!(
    ".globl kthread_trampoline",
    "kthread_trampoline:",
    "sti",
    "call rbx",
    "call {fallback}",
    "3: jmp 3b",
    fallback = sym kthread_fallback,
);

// ========================================================================
// P3-T5 `switch_to`：上下文切换汇编
// ========================================================================
//
// ## ABI 契约（对照 Intel SDM Vol.1 §3.7 + SysV AMD64 ABI §3.2.3）
//
// **SysV AMD64 调用约定**：
// - 调用方负责保存 caller-saved（rax/rcx/rdx/rsi/rdi/r8-r11）；本函数复用 rax
//   作为 rip 中转，不影响调用方对 caller-saved 的保存责任。
// - callee-saved 集合 = { rbx, rbp, r12, r13, r14, r15, rsp }。rip 不在此
//   集合内——但 `call` 指令将 rip 压栈，进入时 [rsp] = ret-addr = 调用点续址。
// - 进入本函数时 rsp ≡ caller_rsp − 8；退出/切换后调用点处 rsp ≡ caller_rsp
//   → SysV 要求调用函数前后 rsp 16 对齐的一致性自动满足（参见 SDM §3.7.2）。
//
// **ContextSlot 字段序**（sched `types.rs:171-176` 编译期断言）：
//   words[0..8] = [rbx, rbp, r12, r13, r14, r15, rsp, rip]，
//   8 字节步进。`CpuContext::to_slot` 与本汇编采用**同一套偏移**，所以可
//   直接对 `*mut ContextSlot` 操作（16-word 槽位的 words[8..] 暂未使用，保留
//   给未来 AVX-512 扩展位）。
//
// ## 寄存器策略
//
// 与传统 Linux `__switch_to` "push 到 prev 栈 → 换栈 → pop 从新栈" 不同，
// 本实现采用**直接读写 ctx**（不引入栈上的 callee-saved 影子帧），原因：
//
// 1. kthread_create 仅在 `[rsp]` 预置 1 个 ret-word（trampoline 地址）；
//    新线程栈不含 callee-saved 的影子帧，传统 push/pop 路径需要重新填栈
//    → 增加 bug 面积且与 P3-T4 已通过的 smoke 检查矛盾。
// 2. 直接读写 ctx 跨"创建/恢复"两条流：`switch_to(boot_ctx, t_ctx)` 中
//    t_ctx.words[0..8] 是创建期预设的 rbx=entry/rip=trampoline，加载后
//    `ret` 弹 trampoline → kthread_trampoline: `call rbx` → entry 永不返回。
//
// ## 对齐保证
//
// - 入口 rsp ≡ caller_rsp − 8 ⇒ rsp % 16 == 8（SysV 进栈约定）。
// - 加载 next.rsp 后 rsp 立即由 next 决定；新线程栈顶预置 rsp ≡ stack_top − 8
//   ⇒ ret 弹出后 rsp ≡ stack_top ≡ 16 对齐（kthread_create 保证 stack_top
//   4KB 对齐 ⇒ stack_top % 16 == 0）。
// - 跳板 `call rbx` 再入栈 8 ⇒ rsp ≡ stack_top − 8 ⇒ entry 函数进入时
//   rsp % 16 == 8（SysV 入栈约定达成）。
global_asm!(
    ".globl _switch_to",
    "_switch_to:",
    // ---- 保存 callee-saved 到 prev_ctx (rdi = prev) ----
    "  mov [rdi + 0x00], rbx",     // rbx
    "  mov [rdi + 0x08], rbp",     // rbp
    "  mov [rdi + 0x10], r12",     // r12
    "  mov [rdi + 0x18], r13",     // r13
    "  mov [rdi + 0x20], r14",     // r14
    "  mov [rdi + 0x28], r15",     // r15
    // ---- 保存 rsp（入口 rsp = caller_rsp - 8）与 rip（[rsp] = ret-addr）----
    "  mov [rdi + 0x30], rsp",     // rsp
    "  mov rax, [rsp]",
    "  mov [rdi + 0x38], rax",     // rip
    // ---- 加载 next 的 callee-saved 与 rsp (rsi = next) ----
    "  mov rbx, [rsi + 0x00]",
    "  mov rbp, [rsi + 0x08]",
    "  mov r12, [rsi + 0x10]",
    "  mov r13, [rsi + 0x18]",
    "  mov r14, [rsi + 0x20]",
    "  mov r15, [rsi + 0x28]",
    "  mov rsp, [rsi + 0x30]",
    // ---- ret 弹 [rsp] 即新 RIP ----
    "  ret",
);

/// 上下文切换：保存 `prev` 的 callee-saved + rsp + rip，加载 `next` 同名寄存器。
///
/// **返回时机**：`prev` 线程被换出后不立即返回；当它未来被另一个 `switch_to`
/// 选为 `next` 时，本调用"返回"，执行从调用点之后继续（callee-saved 与
/// rsp/rip 均从换出时的快照恢复——对调用方而言等价于一次普通的阻塞调用）。
///
/// # Safety
///
/// - `prev` 与 `next` 必须指向 sched `ContextSlot`（16-word，前 8 字布局 =
///   `[rbx, rbp, r12, r13, r14, r15, rsp, rip]`）。**典型用法**：从 `s.thread_mut(id)` /
///   `s.thread(id)` 取出引用后转 `*mut` / `*const ContextSlot`。
/// - `prev` 与 `next` **不得指向同一个槽位**：别名会使"保存"覆盖目标快照，
///   "恢复"读回调用者自己的状态，`ret` 落进调用方编译器视为不可达的区域
///   （int3/ud2 填充）→ #BP 风暴 → #DF。调用前必须先经 sched 状态机
///   （`commit_switch`）确定 prev/next 是两个不同线程。
/// - `next.rsp` 指向的栈内存必须合法（[rsp] 是恢复时的 RIP；新线程由
///   `kthread_create` 在栈顶预置 trampoline 地址）。
/// - 调用方在取指针与切换期间不持有 [`SCHED`] 自旋锁（单核 MVP：临界区内
///   取指针 → 出临界区 → 切换；切换后新线程的锁状态必须干净）。
pub unsafe fn switch_to(
    prev: *mut synapse_sched::ContextSlot,
    next: *const synapse_sched::ContextSlot,
) {
    unsafe { _switch_to(prev, next) }
}

/// 跳板 fallback：entry 意外返回时 panic（走既有 backtrace + 355 出口）。
extern "C" fn kthread_fallback() -> ! {
    panic!("kthread entry returned (contract: fn() -> !)");
}

/// kthread 子系统错误。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KthreadError {
    /// 页帧不足（无法分配 [`KSTACK_FRAMES`] 连续帧）。
    NoMemory,
    /// 调度器错误透传。
    Sched(SchedError),
    /// 目标是当前运行线程：T4 阶段不支持"运行中退出"（需 T5/T6 先切走）。
    StillCurrent,
    /// 元数据缺失（ThreadId 有效但未经 kthread_create/init 注册——集成层 bug）。
    NoMeta,
}

impl From<SchedError> for KthreadError {
    fn from(e: SchedError) -> Self {
        KthreadError::Sched(e)
    }
}

/// 对 [`SCHED`] 的临界区访问（未初始化 panic）。
pub fn with_sched<R>(f: impl FnOnce(&mut Scheduler) -> R) -> R {
    let mut g = SCHED.lock();
    f(g.as_mut().expect(UNINIT))
}

/// 初始化 kthread 子系统并收编 boot 线程。
///
/// 收编 = spawn（最高优先级，owner_pid=0）+ 直接 `commit_switch(None, boot)`
/// 置为 Running——boot 线程不经过就绪队列等待，它本来就站在 CPU 上。
/// 记录当前 rsp 进上下文槽（rip 留 0：运行中线程被换出时才由 switch_to 填）。
/// 重复调用 panic。
pub fn kthread_init() -> ThreadId {
    {
        let mut g = SCHED.lock();
        if g.is_some() {
            panic!("kthread_init called twice");
        }
        *g = Some(Scheduler::new());
    }
    let boot = with_sched(|s| {
        let id = s
            .spawn(synapse_sched::Priority::HIGH.0, 0)
            .expect("boot thread spawn");
        // boot 线程即刻 Running：commit_switch 消费队首（此刻必为它）。
        s.commit_switch(None, id).expect("boot adoption");
        id
    });
    // 记录 boot 线程真实 rsp（供未来换出恢复）；栈是 boot.S 静态栈，无分配帧。
    let rsp: u64;
    unsafe {
        asm!("mov {}, rsp", out(reg) rsp, options(nostack, preserves_flags));
    }
    with_sched(|s| {
        let ctx = CpuContext { rsp, ..CpuContext::default() };
        ctx.to_slot(&mut s.thread_mut(boot).expect("boot tcb").ctx);
    });
    let mut t = KTHREADS.lock();
    t[boot.index()] = Some(KthreadMeta {
        id: boot,
        stack_base: 0,
        frames: 0,
        entry: 0,
        is_boot: true,
    });
    info!("[kthread] init: boot thread adopted id={:#x} rsp={:#x}", boot.0, rsp);
    boot
}

/// 创建内核线程：分配栈（1 guard + 16KB）→ 构造初始上下文 → sched 入队。
///
/// **只入队不切换**（切换是 P3-T5 switch_to + P3-T6 抢占接线的事）；
/// 新线程状态 Ready，等待未来的调度器真正启动它。
pub fn kthread_create(entry: KthreadEntry, priority: u8) -> Result<ThreadId, KthreadError> {
    // 1. 连续帧：guard(帧0) + 栈(帧1..=KSTACK_PAGES)
    let base = page_frame::with_page_frames(|a| a.alloc_contiguous_frames(KSTACK_FRAMES))
        .ok_or(KthreadError::NoMemory)?;
    let stack_top = base + (KSTACK_FRAMES * FRAME_SIZE) as u64;

    // 2. guard 毒化 + 栈清零（恒等映射，物理地址可直接写）
    unsafe {
        let guard = base as *mut u64;
        for i in 0..FRAME_SIZE / 8 {
            guard.add(i).write_volatile(GUARD_POISON);
        }
        let stack = (base + FRAME_SIZE as u64) as *mut u8;
        core::ptr::write_bytes(stack, 0, KSTACK_PAGES * FRAME_SIZE);
        // 3. 蹦床帧：[栈顶-8] = trampoline 地址（switch_to 的 ret 弹此）
        let rsp = stack_top - 8;
        (rsp as *mut u64).write_volatile(kthread_trampoline as *const () as u64);
    }

    // 4. sched 入队 + 写初始上下文
    let id = with_sched(|s| {
        let id = s.spawn(priority, 0)?;
        let ctx = CpuContext {
            rbx: entry as u64,
            rsp: stack_top - 8,
            rip: kthread_trampoline as *const () as u64,
            ..CpuContext::default()
        };
        ctx.to_slot(&mut s.thread_mut(id)?.ctx);
        // FR8 联动：内核线程栈记入 pid 0（内核自身）页账本
        s.ledger_mut().alloc(0, KSTACK_FRAMES as u64)?;
        Ok::<ThreadId, SchedError>(id)
    })?;

    let mut t = KTHREADS.lock();
    t[id.index()] = Some(KthreadMeta {
        id,
        stack_base: base,
        frames: KSTACK_FRAMES,
        entry: entry as u64,
        is_boot: false,
    });
    info!(
        "[kthread] created id={:#x} prio={} stack=[{:#x}..{:#x}) entry={:#x}",
        id.0, priority, base, stack_top, entry as u64
    );
    Ok(id)
}

/// 请求线程退出（非当前运行线程路径：Ready/Blocked/Sleeping → Exited）。
///
/// 当前运行线程的"运行中退出"需要先切到 reaper/idle（P3-T5/T6 接线），
/// T4 阶段返回 [`KthreadError::StillCurrent`]。
pub fn kthread_exit(id: ThreadId) -> Result<(), KthreadError> {
    with_sched(|s| {
        if s.current() == Some(id) {
            return Err(KthreadError::StillCurrent);
        }
        s.exit(id)?;
        Ok(())
    })
}

/// 回收已 Exited 线程：guard 校验 → 释放栈帧 → sched 槽位 reap → FR8 记账归还。
pub fn kthread_reap(id: ThreadId) -> Result<(), KthreadError> {
    // guard 毒化校验（栈溢出事后检测）
    let meta = {
        let t = KTHREADS.lock();
        *t.get(id.index()).ok_or(KthreadError::NoMeta)?.as_ref().ok_or(KthreadError::NoMeta)?
    };
    if !meta.is_boot {
        let corrupted = unsafe {
            let guard = meta.stack_base as *const u64;
            (0..FRAME_SIZE / 8).any(|i| guard.add(i).read_volatile() != GUARD_POISON)
        };
        if corrupted {
            panic!("[kthread] guard page corrupted for id={:#x} — stack overflow detected", id.0);
        }
    }

    with_sched(|s| {
        s.reap(id)?;
        if !meta.is_boot {
            s.ledger_mut().free(0, meta.frames as u64)?;
        }
        Ok::<(), SchedError>(())
    })?;

    if !meta.is_boot {
        page_frame::with_page_frames(|a| a.free_contiguous_frames(meta.stack_base, meta.frames));
    }
    let mut t = KTHREADS.lock();
    t[id.index()] = None;
    info!("[kthread] reaped id={:#x} (stack {} frames freed)", id.0, meta.frames);
    Ok(())
}

/// 查询线程元数据（smoke/调试观测用）。返回 (stack_base, frames, entry, is_boot)。
pub fn meta_of(id: ThreadId) -> Option<(u64, usize, u64, bool)> {
    let t = KTHREADS.lock();
    t.get(id.index())
        .and_then(|o| o.as_ref())
        .filter(|m| m.id == id)
        .map(|m| (m.stack_base, m.frames, m.entry, m.is_boot))
}

// ========================================================================
// P3-T4 真机 smoke：创建线程不切换仅入队 + 栈地址/对齐断言
// ========================================================================

/// smoke 用假入口（永不返回；T4 阶段实际不会被执行——无切换能力）。
extern "C" fn smoke_entry() -> ! {
    loop {
        x86_64::instructions::hlt();
    }
}

/// 宏：断言并计数（失败即 panic 走 backtrace + 355 出口）。
macro_rules! check {
    ($total:ident, $label:expr, $cond:expr) => {{
        if !($cond) {
            panic!("[kthread-smoke] FAIL: {}", $label);
        }
        $total += 1;
        info!("[kthread-smoke]   ok: {}", $label);
    }};
}

/// P3-T4 真机 smoke。在 `_start64` 集成 smoke 之后调用。
pub fn kthread_smoke() {
    info!("[kthread-smoke] start");
    let mut total: u32 = 0;

    // 1. 初始化 + boot 线程收编
    let boot = kthread_init();
    check!(total, "boot thread adopted as current", with_sched(|s| s.current() == Some(boot)));
    check!(
        total,
        "boot thread state = Running",
        with_sched(|s| s.state_of(boot) == Ok(synapse_sched::ThreadState::Running))
    );
    check!(total, "live_count = 1 after init", with_sched(|s| s.live_count() == 1));
    let boot_rsp = with_sched(|s| CpuContext::from_slot(&s.thread(boot).unwrap().ctx).rsp);
    check!(total, "boot ctx.rsp recorded (nonzero, 8-aligned)", boot_rsp != 0 && boot_rsp % 8 == 0);
    check!(total, "boot meta is_boot = true", meta_of(boot).is_some_and(|m| m.3));

    // 2. 创建两个 kthread：只入队，不切换
    let frames_pre = page_frame::with_page_frames(|a| a.used_frames());
    let t1 = kthread_create(smoke_entry, synapse_sched::Priority::DEFAULT.0).expect("create t1");
    let t2 = kthread_create(smoke_entry, synapse_sched::Priority::LOW.0).expect("create t2");
    check!(
        total,
        "t1/t2 state = Ready (enqueued, not switched)",
        with_sched(|s| {
            s.state_of(t1) == Ok(synapse_sched::ThreadState::Ready)
                && s.state_of(t2) == Ok(synapse_sched::ThreadState::Ready)
        })
    );
    check!(total, "current still boot after create", with_sched(|s| s.current() == Some(boot)));
    check!(total, "ready_count = 2", with_sched(|s| s.ready_count() == 2));
    check!(
        total,
        "stack frames charged (2 × 5 frames)",
        page_frame::with_page_frames(|a| a.used_frames() == frames_pre + 2 * KSTACK_FRAMES)
    );
    check!(
        total,
        "FR8 ledger: kernel pages = 10",
        with_sched(|s| s.ledger().pages_of(0) == (2 * KSTACK_FRAMES) as u64)
    );

    // 3. 栈地址/对齐断言（t1）
    let (base, frames, entry, _) = meta_of(t1).expect("t1 meta");
    check!(total, "stack base 4KB-aligned", base % FRAME_SIZE as u64 == 0);
    check!(total, "frames = KSTACK_FRAMES (5)", frames == KSTACK_FRAMES);
    check!(total, "entry recorded", entry == smoke_entry as *const () as u64);
    let ctx = with_sched(|s| CpuContext::from_slot(&s.thread(t1).unwrap().ctx));
    let stack_top = base + (KSTACK_FRAMES * FRAME_SIZE) as u64;
    check!(total, "ctx.rsp inside stack (top-8)", ctx.rsp == stack_top - 8);
    check!(total, "ctx.rsp % 16 == 8 (SysV after-ret alignment)", ctx.rsp % 16 == 8);
    let tramp = kthread_trampoline as *const () as u64;
    check!(total, "ctx.rip = kthread_trampoline", ctx.rip == tramp);
    check!(total, "ctx.rbx = entry", ctx.rbx == entry);
    let tramp_word = unsafe { (ctx.rsp as *const u64).read_volatile() };
    check!(total, "trampoline word on stack top", tramp_word == tramp);

    // 4. guard 页毒化完整（未溢出）
    let guard_ok = unsafe {
        let g = base as *const u64;
        (0..FRAME_SIZE / 8).all(|i| g.add(i).read_volatile() == GUARD_POISON)
    };
    check!(total, "guard page poison intact", guard_ok);

    // 5. 优先级传递 + 调度决策观测（不 commit——T4 无切换能力）
    let decision = with_sched(|s| s.schedule(0));
    check!(
        total,
        "schedule() picks t1 (DEFAULT < LOW), decision only",
        decision == SwitchDecision::Switch { prev: Some(boot), next: t1 }
    );
    check!(
        total,
        "no commit → current unchanged (still boot)",
        with_sched(|s| s.current() == Some(boot))
    );

    // 6. exit → reap：帧归还 + 账本归还 + 槽位失效
    kthread_exit(t2).expect("exit t2");
    check!(
        total,
        "t2 state = Exited",
        with_sched(|s| s.state_of(t2) == Ok(synapse_sched::ThreadState::Exited))
    );
    check!(total, "exit current boot rejected (StillCurrent)", kthread_exit(boot) == Err(KthreadError::StillCurrent));
    kthread_reap(t2).expect("reap t2");
    let used_post = page_frame::with_page_frames(|a| a.used_frames());
    info!("[kthread-smoke] used_frames: pre={} post={} expect={}", frames_pre, used_post, frames_pre + KSTACK_FRAMES);
    check!(total, "t2 frames freed", used_post == frames_pre + KSTACK_FRAMES);
    check!(
        total,
        "FR8 ledger after reap = 5",
        with_sched(|s| s.ledger().pages_of(0) == KSTACK_FRAMES as u64)
    );
    check!(
        total,
        "t2 id stale (NotFound) after reap",
        with_sched(|s| s.state_of(t2) == Err(SchedError::NotFound))
    );
    check!(total, "live_count = 2 (boot + t1)", with_sched(|s| s.live_count() == 2));

    // 收尾：回收 t1——它的 entry 是 hlt 死循环占位（T4 无切换能力时代的产物），
    // 留队会 ① 干扰 P3-T5 switch smoke 的 commit_switch "队首必须是 next" 断言
    // ② 一旦被误调度即永久挂死。exit + reap 归还帧与账本。
    kthread_exit(t1).expect("exit t1");
    kthread_reap(t1).expect("reap t1");
    let _ = with_sched(|s| s.take_need_resched()); // 清 schedule() 残留标志
    if with_sched(|s| s.need_resched()) {
        warn!("[kthread-smoke] need_resched unexpectedly set");
    }
    info!("[kthread-smoke] {}/{} checks passed", total, total);
}

// ========================================================================
// P3-T5 真机 smoke：switch_to 上下文切换的双向贯通
// ========================================================================
//
// 流程：commit(boot→t) → switch_to → t 跑 entry 自增计数器 → commit(t→boot)
//      → switch_to → boot 从调用点"返回" → 验证计数/状态/双方 ctx 快照。
//
// **先 commit 后切换**（P3-T5 首跑 #DF 的教训）：sched `current()` 是 entry
// 定位自身 prev ctx 的唯一依据；不 commit 则 entry 里 prev/next 别名同一个
// boot_ctx——保存阶段覆写 boot 快照，恢复阶段读回自己的状态，`ret` 落进
// 编译器视为不可达的 int3 填充区 → #BP 风暴 → 跑飞 #DF。
//
// entry 无法收参数（`KthreadEntry = fn() -> !`）：boot 的 ThreadId 经
// [`SWITCH_BOOT_ID`] 全局传入；entry 自身 id 用 `s.current()`（已 commit）。

/// 切换 smoke：boot 通过此 static 把 ThreadId 告诉 entry（`ThreadId.0` 是 u32）。
static SWITCH_BOOT_ID: AtomicU32 = AtomicU32::new(0);
/// 切换 smoke：entry 自增次数（验证 entry 真的被跑到）。
static SWITCH_TICK: AtomicU64 = AtomicU64::new(0);

/// 切换 smoke 用 entry：自增计数 + 切回 boot。
///
/// SAFETY: 调用本函数前必须已 `SWITCH_BOOT_ID.store(boot.0)`。
extern "C" fn switch_entry() -> ! {
    SWITCH_TICK.fetch_add(1, Ordering::Relaxed);
    let boot = ThreadId(SWITCH_BOOT_ID.load(Ordering::Relaxed));
    // 取 ctx 指针 + commit_switch（t → boot）：更新 scheduler.current = boot
    let (prev, next) = with_sched(|s| {
        let current = s.current().expect("current running");
        // commit_switch: current(t) → boot（boot 状态 Exited → Running）
        s.commit_switch(Some(current), boot).expect("commit_switch t→boot");
        let prev = &mut s.thread_mut(current).expect("current tcb").ctx as *mut _;
        let next = &s.thread(boot).expect("boot tcb").ctx as *const _;
        (prev, next)
    });
    // SAFETY: SCHED 锁外不重入；ctx 指针在数组中稳定；prev/next 经 commit_switch
    // 确定为两个不同线程；next.rsp 已由前次 switch_to 保存（boot 换出快照）。
    unsafe { _switch_to(prev, next) }
    // 正常路径下本线程被换出后不再回来（T5 smoke 结束时 t 已被 exit+reap）。
    // 若未来某次切换把 t 选为 next，会从这里的"调用点之后"恢复——落进 panic
    // 留证（smoke 语义：t 只应被调度一次）。
    panic!("switch_entry resumed after switch-away (unexpected in T5 smoke)");
}

/// P3-T5 真机 smoke：在 `_start64` 集成 smoke 之后调用。
pub fn kthread_switch_smoke() {
    info!("[kthread-switch-smoke] start");
    let mut total: u32 = 0;

    // 1. 准备：记录 boot id、清零计数器
    let boot = with_sched(|s| s.current().expect("current is boot"));
    SWITCH_BOOT_ID.store(boot.0, Ordering::Relaxed);
    SWITCH_TICK.store(0, Ordering::Relaxed);

    // 2. 创建切换线程。必须 HIGH 优先级：commit_switch 要求 next 在队首，
    //    而 boot（HIGH）被换出时也会回插 level-0 队尾——若 t 是 DEFAULT，
    //    队首会变成 boot 自己 → BadState。HIGH + FIFO（t 先入队）保证 peek()=t。
    let t = kthread_create(switch_entry, Priority::HIGH.0).expect("create switch thread");

    // 3. 切换前：boot 仍是 current、t 是 Ready
    check!(
        total,
        "boot still current before switch",
        with_sched(|s| s.current() == Some(boot))
    );
    check!(
        total,
        "switch thread Ready",
        with_sched(|s| s.state_of(t) == Ok(ThreadState::Ready))
    );

    // 4. 切换 boot → t（先 commit_switch 更新 scheduler.current = t）
    let (prev, next) = with_sched(|s| {
        // commit_switch: boot → t（更新 current 字段，t 状态 Ready → Running）
        s.commit_switch(Some(boot), t).expect("commit_switch boot→t");
        let prev = &mut s.thread_mut(boot).expect("boot tcb").ctx as *mut _;
        let next = &s.thread(t).expect("t tcb").ctx as *const _;
        (prev, next)
    });
    // SAFETY: prev=boot / next=t 是两个不同槽位（commit 已定序，无别名）；
    // t 的栈与蹦床帧由 kthread_create 预置；SCHED 锁已释放（t 的 entry 还要
    // 再进临界区）。本次调用在 t 切回 boot 后"返回"——控制流从下一行继续。
    unsafe { _switch_to(prev, next) };

    // 5. 验证：t 已执行（counter==1）→ 切回 boot → boot 继续
    check!(
        total,
        "switch_entry ran exactly once (counter = 1)",
        SWITCH_TICK.load(Ordering::Relaxed) == 1
    );
    check!(
        total,
        "current restored to boot",
        with_sched(|s| s.current() == Some(boot))
    );

    // 6. 验证：boot 的 ctx 在切换时已被 switch_to 保存（rsp/rip 非零、对齐）。
    //    SysV：call 前 rsp ≡ 0 (mod 16)，call 压 8 字节 → _switch_to 入口
    //    rsp ≡ 8 (mod 16)，保存的正是这个值。
    let boot_ctx = with_sched(|s| CpuContext::from_slot(&s.thread(boot).unwrap().ctx));
    check!(
        total,
        "boot ctx.rsp saved (nonzero, entry-aligned %16==8)",
        boot_ctx.rsp != 0 && boot_ctx.rsp % 16 == 8
    );
    check!(
        total,
        "boot ctx.rip saved (nonzero, in kernel text)",
        boot_ctx.rip != 0 && boot_ctx.rip > 0x20_0000 && boot_ctx.rip < 0x80_0000
    );

    // 7. 验证：t 的 ctx 也被 switch_entry 内第二次 _switch_to 保存。
    //    t_ctx.rsp = switch_entry 调 _switch_to 时的入口 rsp——位于 t 自己的
    //    栈区间内（guard 之上、栈顶之下），而不是创建期的 stack_top-8。
    let t_ctx = with_sched(|s| CpuContext::from_slot(&s.thread(t).unwrap().ctx));
    let (t_base, t_frames, t_entry, _) = meta_of(t).expect("t meta");
    let t_stack_top = t_base + (t_frames * FRAME_SIZE) as u64;
    check!(
        total,
        "t ctx.rsp inside own stack (switch_entry frame saved)",
        t_ctx.rsp > t_base + FRAME_SIZE as u64 && t_ctx.rsp < t_stack_top && t_ctx.rsp % 16 == 8
    );
    // rip = switch_entry 内第二次 _switch_to 调用点的续址（内核 text 段内）。
    // 不检查 rbx==entry：entry 真实运行后 rbx 是编译器自由使用的 callee-saved
    // 值（仅要求 switch_entry 自己的调用者视角守恒），==entry 只是 -O0 巧合。
    check!(
        total,
        "t ctx.rip = resume point inside entry (kernel text)",
        t_ctx.rip > 0x20_0000 && t_ctx.rip < 0x80_0000
    );
    let _ = t_entry; // meta 完整性由上方 kthread-smoke 的 entry 检查覆盖

    // 8. 收尾：exit + reap t（此刻 t 是 Ready——第二次 commit_switch 把它回插
    //    level-0 队列）。必须清掉：否则 t 留在队里，未来任何调度点都会把它
    //    选中 → 恢复进 switch_entry 尾部的 panic 留证路径。
    kthread_exit(t).expect("exit t");
    kthread_reap(t).expect("reap t");
    check!(
        total,
        "t cleaned up (exit + reap, queue empty)",
        with_sched(|s| s.state_of(t) == Err(SchedError::NotFound) && s.ready_count() == 0)
    );

    info!("[kthread-switch-smoke] {}/{} checks passed", total, total);
}

// ========================================================================
// P3-T6 抢占模型接线：IRQ0 → need_resched → 中断返回边界/阻塞点 checkpoint
// ========================================================================
//
// ## 抢占模型（sched crate 契约在此消费）
//
// - 定时器 ISR 核心只做记账：account_cpu(+1 tick) + `tick(now)`（唤醒到期
//   睡眠者、时间片到期置 need_resched），**不含切换逻辑**。
// - 真正的切换统一收敛到 [`checkpoint`]，两类调用点：
//   ① 中断返回边界（handler 尾部，EOI 之后、iret 之前）；
//   ② 阻塞点（[`kthread_yield`] / [`kthread_sleep_until`] /
//      [`kthread_block_current`] / [`kthread_exit_running`]）。
//
// ## 为什么 handler 尾部切换是安全的（xv6 同模型）
//
// 严格的"iret 之后才切换"需要汇编级全上下文钩子（保存/恢复全部
// caller-saved + 中断帧搬迁）。MVP 采用等价简化：handler 尾部（EOI 已发）
// 调 checkpoint——被切走线程的完整中断现场由**它自己栈上的硬件中断帧**
// (SS/RSP/RFLAGS/CS/RIP) 保留，`_switch_to` 只需另存 callee-saved；该线程
// 未来被选为 next 恢复时，沿自己的栈从 handler 返回路径 `iret` 弹出自己的
// 中断帧 → 完整还原被打断点（RFLAGS.IF 也随之恢复 1）。不变量：**被抢占
// 切走的线程，栈上恰好有一份冻结的中断帧 + handler 调用链**，恢复与 iret
// 一一配对，不嵌套、不跨线程。EOI 必须先于切换：否则切走后 PIC 仍屏蔽
// IRQ0，接管线程再也收不到定时器中断。
//
// ## IF（中断标志）纪律
//
// checkpoint 入口快照 IF → `cli` → 决策 + commit + `_switch_to`（全程 IF=0，
// 消除"决策后、切换前"定时器重入导致的双重 commit/双重切换窗口）；恢复后
// 按**本线程入口时的值**回置：
// - 线程上下文阻塞点（yield 等，入口 IF=1）→ 恢复 1；
// - 中断上下文（入口 IF=0，中断门自动清零）→ 保持 0，由 handler 返回路径
//   的 `iret` 恢复 1 —— 避免在 iret 之前嵌套进新定时器中断。
//
// ## PREEMPT 门
//
// T4/T5 smoke 在定时器中断已开启（marker X 早于线程 smoke）的环境下运行，
// 其占位 entry 是 hlt 死循环 / 一次性验证路径——被抢占切入即挂死或触发
// 留证 panic。故 [`on_timer_irq`] 以 [`PREEMPT`] 为门，仅 T6 smoke 内开启
// （smoke 结束回关，后续路径保持静默）。

/// 抢占门：true 时 [`on_timer_irq`] 才做记账 + 调度决策；false 时 IRQ 只计数 tick。
static PREEMPT: AtomicBool = AtomicBool::new(false);
/// IRQ 检查点实际执行 `_switch_to` 的累计次数（T6 smoke 观测：
/// 时间片到期自动切换的证据；线程恢复时自增，见 `on_timer_irq` 注释）。
static PREEMPT_SWITCHES: AtomicU64 = AtomicU64::new(0);

/// 开启抢占（T6 smoke 起用；幂等）。
pub fn enable_preemption() {
    PREEMPT.store(true, Ordering::Relaxed);
}

/// 关闭抢占（定时器 IRQ 退回纯 tick 计数；smoke 收尾/静默期用）。
pub fn disable_preemption() {
    PREEMPT.store(false, Ordering::Relaxed);
}

/// 抢占是否开启（观测用）。
pub fn preemption_enabled() -> bool {
    PREEMPT.load(Ordering::Relaxed)
}

/// IRQ 检查点在"物理运行线程已不可保存"时使用的 scratch 保存槽（P3-T6）。
///
/// 场景：最后一个活动线程走完 `kthread_exit_running`（schedule 返回 Idle →
/// 落入 enable_and_hlt 兜底循环）而全场只剩睡眠线程；tick 唤醒睡眠者后
/// schedule 返回 `Switch{prev:None}`——物理运行线程是 **Exited** 的兜底者，
/// 它的 ctx 槽在语义上已作废。此时把保存段写进本 scratch（只写不读：
/// Exited 线程永不再被选为 next，其冻结栈等待 reap 回收），切换照常进行。
///
/// 单核 + checkpoint 全程 IF=0 → 无并发写者；`UnsafeCell` 仅为绕开
/// `static mut` 引用告警。
struct IdleScratch(UnsafeCell<ContextSlot>);
// SAFETY: 见上——唯一写者是关中断下的 checkpoint 保存段，单核无并发；
// newtype + 直接 impl 而非依赖 `UnsafeCell<T: Send>` 的 blanket Sync（sched
// crate 未对 ContextSlot 声明 Send/Sync，静态项检查不过）。
unsafe impl Sync for IdleScratch {}

static IDLE_SCRATCH: IdleScratch =
    IdleScratch(UnsafeCell::new(ContextSlot { words: [0; synapse_sched::CTX_WORDS] }));

/// IRQ0 定时器中断的调度入口（由 `idt.rs` handler 在 **EOI 之后**调用）。
///
/// 三步：① current 记 1 tick CPU（FR8；TSC 精算归 P3-T8）；② `tick(now)`
/// 唤醒到期睡眠者 + 时间片到期置 need_resched；③ 中断返回边界 checkpoint。
///
/// 防御门：`PREEMPT` 关 / `SCHED` 未初始化（kthread_init 之前中断已活）
/// 直接返回——tick 计数（pit.rs）不受影响。
pub fn on_timer_irq() {
    if !PREEMPT.load(Ordering::Relaxed) {
        return;
    }
    if SCHED.lock().is_none() {
        return;
    }
    let me = with_sched(|s| {
        let cur = s.current();
        if let Some(c) = cur {
            let _ = s.account_cpu(c, 1); // 1 tick = 10ms @100Hz PIT
        }
        let mut woken = [ThreadId(0); 8];
        s.tick(crate::pit::tick_count(), &mut woken);
        cur
    });
    // checkpoint 返回 true = 本次 IRQ 真的切走了本线程；计数发生在恢复时
    // （切走 → 冻结 → 未来恢复 → +1）。smoke 检查时所有被切线程均已恢复
    // 或退出（退出前必先恢复），故终值完整。
    if checkpoint(me) {
        PREEMPT_SWITCHES.fetch_add(1, Ordering::Relaxed);
    }
}

/// 统一调度检查点：`schedule(now)` 决策 → `commit_switch` → `_switch_to`。
///
/// `dummy_prev` = 调用线程已把自己摘出 CPU 时（yield/sleep/block/exit 均置
/// current=None → schedule 返回 prev=None）的保存目标：自己的 ctx 槽位——
/// `_switch_to` 保存段写入的是本线程的活寄存器，未来被唤醒/回队再选为
/// next 时从"本调用之后"恢复，语义等价一次普通阻塞调用。
///
/// 返回是否真的执行了 `_switch_to`（自切换/KeepCurrent/Idle = false）。
fn checkpoint(dummy_prev: Option<ThreadId>) -> bool {
    use x86_64::registers::rflags::{read as read_rflags, RFlags};
    // 入口 IF 快照 + 关中断：决策 → 切换必须对定时器 IRQ 原子，否则窗口期
    // IRQ 重入会造成双重 commit / 双重切换（见模块头 IF 纪律）。
    let if_on = read_rflags().contains(RFlags::INTERRUPT_FLAG);
    x86_64::instructions::interrupts::disable();

    let action = with_sched(|s| {
        let now = crate::pit::tick_count();
        match s.schedule(now) {
            SwitchDecision::Switch { prev, next } => {
                let n = &s.thread(next).expect("next tcb").ctx as *const _;
                match prev.or(dummy_prev) {
                    // 无线程级保存目标：物理运行线程已 Exited（exit_running
                    // 兜底 hlt 循环中命中 IRQ）或处于"摘出→checkpoint"指令级
                    // 窗口。commit 照常（current=next），保存段写 scratch 废弃。
                    // 放弃本轮会让被唤醒线程永远无人切入（P3-T6 二跑挂死根因）。
                    None => {
                        s.commit_switch(prev, next).expect("checkpoint: commit_switch");
                        Some((IDLE_SCRATCH.0.get(), n))
                    }
                    Some(save_id) => {
                        s.commit_switch(prev, next).expect("checkpoint: commit_switch");
                        if save_id == next {
                            // 自切换（如 yield 后全场仅自己就绪）：commit 已把
                            // current 置回自己；绝不能对同一槽位跑 _switch_to
                            // （别名指针是 P3-T5 #DF 级陷阱，见 switch_to Safety）。
                            return None;
                        }
                        let p = &mut s.thread_mut(save_id).expect("prev tcb").ctx as *mut _;
                        Some((p, n))
                    }
                }
            }
            SwitchDecision::KeepCurrent | SwitchDecision::Idle => None,
        }
    });

    if let Some((prev, next)) = action {
        // SAFETY: prev/next 为不同槽位（上方已排除自切换）；SCHED 锁已释放
        // （恢复侧线程还要再进临界区）；全程 IF=0 无 IRQ 重入。本调用在被
        // 切走后"冻结"，未来被别的 checkpoint 选为 next 时从下一行恢复。
        unsafe { _switch_to(prev, next) };
    }
    if if_on {
        x86_64::instructions::interrupts::enable();
    }
    action.is_some()
}

/// 当前线程主动让出：Running → Ready（入队尾 RR），随即检查点切换。
pub fn kthread_yield() {
    let me = with_sched(|s| {
        let cur = s.current().expect("kthread_yield: no current thread");
        s.yield_current().expect("yield_current");
        cur
    });
    checkpoint(Some(me));
}

/// 当前线程睡到绝对 tick 时刻 `deadline`（tick = PIT 计数，10ms @100Hz）。
/// 到期唤醒由 [`on_timer_irq`] 的 `tick(now)` 完成，唤醒后按优先级入队。
pub fn kthread_sleep_until(deadline: u64) {
    let me = with_sched(|s| {
        let cur = s.current().expect("kthread_sleep_until: no current thread");
        s.sleep_current(deadline).expect("sleep_current");
        cur
    });
    checkpoint(Some(me));
}

/// 当前线程阻塞（P3-T7 Mutex / 未来 IPC 等待用；唤醒方 `unblock` 后
/// 由下一个检查点切入）。
pub fn kthread_block_current() {
    let me = with_sched(|s| {
        let cur = s.current().expect("kthread_block_current: no current thread");
        s.block_current().expect("block_current");
        cur
    });
    checkpoint(Some(me));
}

/// 当前运行线程退出：Running → Exited（摘出 CPU），随即切走，永不返回。
///
/// boot 线程不可退出（系统最后防线——违规 panic 留证）。退出线程的 ctx 槽
/// 仍作 `_switch_to` 保存目标（只写不读：Exited 是终态，永不会再被选为
/// next）；栈帧与 FR8 记账待 [`kthread_reap`] 回收。
pub fn kthread_exit_running() -> ! {
    let me = with_sched(|s| s.current().expect("kthread_exit_running: no current thread"));
    if meta_of(me).is_some_and(|m| m.3) {
        panic!("boot thread attempted kthread_exit_running (id={:#x})", me.0);
    }
    with_sched(|s| s.exit(me).expect("exit current"));
    checkpoint(Some(me));
    // 极端兜底：全场无可运行线程（Idle）→ 开中断停机等待（定时器仍在跳，
    // 未来若有线程被唤醒/创建，其 checkpoint 不会选中 Exited 的本线程——
    // 本循环即永久 idle。T6 smoke 场景不可达：boot 恒在）。
    loop {
        x86_64::instructions::interrupts::enable_and_hlt();
    }
}

// ========================================================================
// P3-T6 真机 smoke：3 内核线程并发计数/打印 + 时间片抢占 + 阻塞点轮转
// ========================================================================
//
// 拓扑：boot(HIGH) 睡眠轮询；3 个 DEFAULT worker 各自：
//   phase A — 计数到 W_LIMIT，每 W_YIELD_EVERY 主动 yield（阻塞点轮转 +
//             输出交错），每 W_PRINT_EVERY 打印进度；
//   phase B — 不让出空转 W_SPIN_TICKS（= 3 个时间片）→ 必然触发
//             "时间片到期 → need_resched → IRQ 检查点切走"（抢占证据）；
//   收尾   — 置 done → kthread_exit_running（运行中退出 + 自动切走）。
// boot 侧超时护栏：SMOKE_TIMEOUT_TICKS 未收敛即 panic（确定性 355 出口，
// 拒绝挂死 QEMU）。

/// worker 数量。
const WORKERS: usize = 3;
/// 每 worker phase A 计数上限。
const W_LIMIT: u64 = 30_000;
/// phase A 每计数多少主动 yield 一次。
const W_YIELD_EVERY: u64 = 1_000;
/// phase A 每计数多少打印一次进度。
const W_PRINT_EVERY: u64 = 10_000;
/// phase B 无让出空转的 tick 跨度（6 tick = 60ms = 3 个时间片）。
const W_SPIN_TICKS: u64 = 6;
/// boot 轮询睡眠间隔（tick）。
const BOOT_POLL_TICKS: u64 = 3;
/// 全 smoke 超时（tick；1500 = 15s @100Hz）。
const SMOKE_TIMEOUT_TICKS: u64 = 1_500;

/// 各 worker 进度（done 的 Release/Acquire 建立 happens-before，Relaxed 足够）。
static W_PROGRESS: [AtomicU64; WORKERS] = [const { AtomicU64::new(0) }; WORKERS];
/// 各 worker 完成标志（置于 exit_running 之前——exit 永不返回，无后置机会）。
static W_DONE: [AtomicBool; WORKERS] = [const { AtomicBool::new(false) }; WORKERS];

/// worker 主体（entry 无法收参数，经宏生成的 3 个 extern fn 以索引进入）。
fn worker_body(idx: usize) -> ! {
    for i in 1..=W_LIMIT {
        W_PROGRESS[idx].store(i, Ordering::Relaxed);
        if i % W_PRINT_EVERY == 0 {
            info!("[preempt-smoke] worker {} progress {}", idx, i);
        }
        if i % W_YIELD_EVERY == 0 {
            kthread_yield();
        }
    }
    // phase B：抱着 CPU 空转跨多个时间片 → tick 到期置 need_resched →
    // 下一次 IRQ0 检查点把本 worker 切走（其他 worker / 唤醒的 boot 接管）。
    let start = crate::pit::tick_count();
    while crate::pit::tick_count() < start + W_SPIN_TICKS {
        core::hint::spin_loop();
    }
    W_DONE[idx].store(true, Ordering::Release);
    info!("[preempt-smoke] worker {} finished (progress={})", idx, W_LIMIT);
    kthread_exit_running()
}

macro_rules! preempt_worker_entry {
    ($name:ident, $idx:expr) => {
        extern "C" fn $name() -> ! {
            worker_body($idx)
        }
    };
}
preempt_worker_entry!(preempt_worker_0, 0);
preempt_worker_entry!(preempt_worker_1, 1);
preempt_worker_entry!(preempt_worker_2, 2);

/// P3-T6 真机 smoke：抢占 + 阻塞点 + RR 轮转 + 运行中退出 + 回收。
pub fn kthread_preempt_smoke() {
    info!("[preempt-smoke] start");
    let mut total: u32 = 0;
    let boot = with_sched(|s| s.current().expect("current is boot"));

    for i in 0..WORKERS {
        W_PROGRESS[i].store(0, Ordering::Relaxed);
        W_DONE[i].store(false, Ordering::Relaxed);
    }
    PREEMPT_SWITCHES.store(0, Ordering::Relaxed);

    let frames_pre = page_frame::with_page_frames(|a| a.used_frames());
    let switch_pre = with_sched(|s| s.switch_count());

    let ws = [
        kthread_create(preempt_worker_0, Priority::DEFAULT.0).expect("create w0"),
        kthread_create(preempt_worker_1, Priority::DEFAULT.0).expect("create w1"),
        kthread_create(preempt_worker_2, Priority::DEFAULT.0).expect("create w2"),
    ];
    check!(
        total,
        "3 workers Ready, boot still current (preemption off during create)",
        with_sched(|s| s.ready_count() == 3 && s.current() == Some(boot))
    );

    // 开启抢占 → boot 睡眠轮询：每次 tick 唤醒（HIGH）即抢占 DEFAULT worker；
    // worker 之间由 yield + 时间片 RR 轮转；输出在串口上交错。
    enable_preemption();
    let t0 = crate::pit::tick_count();
    loop {
        if (0..WORKERS).all(|i| W_DONE[i].load(Ordering::Acquire)) {
            break;
        }
        let elapsed = crate::pit::tick_count().saturating_sub(t0);
        if elapsed > SMOKE_TIMEOUT_TICKS {
            panic!(
                "[preempt-smoke] timeout after {} ticks: progress = [{}, {}, {}]",
                elapsed,
                W_PROGRESS[0].load(Ordering::Relaxed),
                W_PROGRESS[1].load(Ordering::Relaxed),
                W_PROGRESS[2].load(Ordering::Relaxed),
            );
        }
        kthread_sleep_until(crate::pit::tick_count() + BOOT_POLL_TICKS);
    }

    // done 置位 → exit_running 之间有指令级窗口；reap 有界重试（睡等其退出）。
    for (n, &w) in ws.iter().enumerate() {
        let mut tries = 0u32;
        loop {
            match kthread_reap(w) {
                Ok(()) => break,
                Err(_) => {
                    tries += 1;
                    if tries > 100 {
                        panic!("[preempt-smoke] worker {} stuck (not exiting)", n);
                    }
                    kthread_sleep_until(crate::pit::tick_count() + 2);
                }
            }
        }
    }
    disable_preemption(); // 后续路径回到静默（定时器 IRQ 只剩 tick 计数）

    // ---- 验证 ----
    check!(
        total,
        "all workers reached LIMIT (progress > 0 each)",
        (0..WORKERS).all(|i| W_PROGRESS[i].load(Ordering::Relaxed) == W_LIMIT)
    );
    check!(
        total,
        "all workers exited + reaped (state NotFound x3)",
        ws.iter().all(|&w| with_sched(|s| s.state_of(w) == Err(SchedError::NotFound)))
    );
    let switch_grew = with_sched(|s| s.switch_count()) - switch_pre;
    info!(
        "[preempt-smoke] switches during smoke = {}, irq-preempt switches = {}",
        switch_grew,
        PREEMPT_SWITCHES.load(Ordering::Relaxed)
    );
    check!(
        total,
        "switch_count grew >= 30 (yield + RR + preemption)",
        switch_grew >= 30
    );
    check!(
        total,
        "time-slice expiry auto-switch (>= 1 IRQ-driven switch)",
        PREEMPT_SWITCHES.load(Ordering::Relaxed) >= 1
    );
    check!(
        total,
        "FR8: total_cpu > 0 (tick accounting live)",
        with_sched(|s| s.total_cpu() > 0)
    );
    check!(
        total,
        "frames restored after reap (3 x 5 freed)",
        page_frame::with_page_frames(|a| a.used_frames()) == frames_pre
    );
    check!(total, "current still boot", with_sched(|s| s.current() == Some(boot)));
    check!(
        total,
        "run/sleep queues quiescent (ready=0, sleeping=0)",
        with_sched(|s| s.ready_count() == 0 && s.sleeping_count() == 0)
    );

    info!("[preempt-smoke] {}/{} checks passed", total, total);
}
