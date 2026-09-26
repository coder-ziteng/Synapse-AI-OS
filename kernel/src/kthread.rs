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

use log::{info, warn};
use synapse_sched::{ContextSlot, Scheduler, SchedError, SwitchDecision, ThreadId, MAX_THREADS};

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
}

// 跳板：switch_to `ret` 至此 → `call rbx`（entry）。entry 声明为 `-> !`，
// 正常永不返回；返回即 bug → fallback panic 留证。
global_asm!(
    ".globl kthread_trampoline",
    "kthread_trampoline:",
    "call rbx",
    "call {fallback}",
    "3: jmp 3b",
    fallback = sym kthread_fallback,
);

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

    // 收尾：t1 留队（P3-T5 switch_to 真机验证的第一个切换目标）。
    let _ = with_sched(|s| s.take_need_resched()); // 清 schedule() 残留标志
    if with_sched(|s| s.need_resched()) {
        warn!("[kthread-smoke] need_resched unexpectedly set");
    }
    info!("[kthread-smoke] {}/{} checks passed", total, total);
}
