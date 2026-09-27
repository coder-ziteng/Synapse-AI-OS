//! 时钟基准：TSC ↔ PIT 频率校准 + 单调时钟（P2-T7）。
//!
//! ## 目的
//!
//! Phase 2 出口的最后一块时间基础设施：为 Phase 3 时间片调度与
//! Phase 4 `gettime` syscall（设计文档 02 §7）提供**校准后的单调时钟基准**。
//!
//! ## 校准原理
//!
//! PIT Channel 0 以已知基准频率（1193182 Hz ÷ divisor）驱动 IRQ 0 tick。
//! 校准过程：
//!
//! 1. 采样点对 `(tsc, pit_counts)`，其中 `pit_counts` 是 PIT 自初始化以来
//!    累计走过的**输入时钟计数**：`ticks × divisor + (divisor − latched_counter)`。
//!    采样在 `without_interrupts` 临界区内完成，保证 latch counter 与
//!    tick_count 读取的一致性（否则 IRQ 0 可能在两次读取之间触发，
//!    引入整整一个 divisor 的误差 ≈ 10%）。
//! 2. 等待测量窗口走过 ≥ [`CALIB_TICKS`] 个 tick（100Hz 下 ≈ 100ms）。
//! 3. `tsc_hz = Δtsc × PIT_FREQUENCY ÷ Δpit_counts`。
//!
//! 精度分析：100ms 窗口下 Δpit_counts ≈ 119320，latch 读数误差 ≤ 2 counts
//! （Mode 3 计数器按 2 递减），相对误差 ~2e-5，远优于时间片调度需求。
//!
//! ## 为什么选 PIT 而不是其他源？
//!
//! - **PIT ch0 tick + latch**（本实现）：ch0 已在跑（P2-T6），无需重配
//!   中断源；latch 提供 tick 内的亚毫秒分数，窗口可以短（100ms）。
//! - PIT ch2 one-shot：需要额外操作 port 0x61 gate 位，且 ch2 不产生
//!   中断只能轮询——收益不明显，弃。
//! - APIC timer：Phase 2 尚未迁移 APIC（可选项，见需求目标 Phase 2），弃。
//! - RTC (CMOS)：1Hz 量级粒度太粗，弃。
//!
//! ## 单调时钟 API
//!
//! [`monotonic_ns`] / [`monotonic_us`] / [`monotonic_ms`] 基于 `rdtsc` +
//! 校准频率换算，起点为 CPU reset（TSC 从 0 开始计）。所有换算用
//! `(tsc/hz)×mult + (tsc%hz)×mult/hz` 拆分避免 u64 中间溢出。

use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::instructions::interrupts::without_interrupts;

use crate::pit;

/// 校准测量窗口：至少走过这么多 PIT tick（100Hz 下 ≈ 100ms）。
const CALIB_TICKS: u64 = 10;

/// 校准得到的 TSC 频率（Hz）。0 = 尚未校准。
static TSC_HZ: AtomicU64 = AtomicU64::new(0);

/// 读 TSC（rdtsc 指令）。
#[inline]
pub fn rdtsc() -> u64 {
    // SAFETY: rdtsc 在 ring 0 恒可用（CR4.TSD 未置位），无内存副作用。
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// 校准得到的 TSC 频率（Hz）；未校准返回 0。
pub fn tsc_hz() -> u64 {
    TSC_HZ.load(Ordering::Relaxed)
}

/// 采样一次 `(tsc, pit 累计输入时钟计数)`。
///
/// 关中断临界区内完成 latch + tick 读取，保证二者一致（见模块头精度分析）。
fn sample(divisor: u64) -> (u64, u64) {
    without_interrupts(|| {
        let tsc = rdtsc();
        // SAFETY: 在 without_interrupts 临界区内独占访问 PIT；单核 boot 路径。
        let counter = unsafe { pit::read_counter() } as u64;
        let ticks = pit::tick_count();
        // counter 从 divisor 递减到 0 后重载并触发 tick；
        // 当前周期内已走计数 = divisor − counter（clamp 防御重载瞬间的边界值）。
        let frac = divisor.saturating_sub(counter.min(divisor));
        (tsc, ticks * divisor + frac)
    })
}

/// 执行 TSC ↔ PIT 交叉校准（boot 时调用一次；需中断已开启、PIT 已初始化）。
///
/// 阻塞约 [`CALIB_TICKS`] 个 tick 时长（100Hz 下 ≈ 100ms）。
/// 重复调用会重新校准并覆盖结果（幂等）。
///
/// # Panics
///
/// PIT 未初始化（frequency == 0）时 panic——调用顺序错误属于 boot 布线 bug。
pub fn calibrate() {
    // SAFETY: boot 路径单线程读取 PIT 配置；frequency() 只读 UnsafeCell 内字段。
    let freq = unsafe { pit::frequency() } as u64;
    assert!(freq > 0, "clock::calibrate requires pit::init first");

    // 与 pit.rs init 写入的分频值一致（整数除法）。
    let divisor = pit::PIT_FREQUENCY as u64 / freq;

    let window = CALIB_TICKS * divisor;

    // 累积式测量：逐段累加 (Δpit_counts, Δtsc)。
    //
    // 为什么不能只采两个端点相减：PIT counter 硬件回绕后 TICK_COUNT 要等
    // IRQ 0 handler 跑完才 +1（QEMU TCG 下中断投递可延迟到 TB 边界），
    // cli 临界区内 latch 到的 (counter, ticks) 可能处于 "counter 已重载、
    // ticks 未跟上" 的滞后态 → 累计计数瞬间**倒退** ~divisor。端点相减
    // (wrapping_sub) 会把倒退放大成 ~u64::MAX 的假窗口（曾导致
    // `dcnt * 1000` 乘法溢出 panic）。
    //
    // 累积策略：只有 cnt 前进的区间才被消费；遇到倒退（IRQ 滞后）就
    // 不更新基线，等 tick 补上后从旧基线重新量——tsc 单调不减，
    // Δtsc 与 Δpit_counts 始终覆盖同一段真实时间，无系统性偏差。
    let (mut tsc_base, mut cnt_base) = sample(divisor);
    let mut dcnt_acc: u64 = 0;
    let mut dtsc_acc: u64 = 0;

    while dcnt_acc < window {
        core::hint::spin_loop();
        let (tsc1, cnt1) = sample(divisor);
        if cnt1 >= cnt_base {
            dcnt_acc += cnt1 - cnt_base;
            dtsc_acc += tsc1.saturating_sub(tsc_base);
            tsc_base = tsc1;
            cnt_base = cnt1;
        }
        // cnt1 < cnt_base：IRQ 滞后瞬时态，跳过（不更新基线）
    }

    // Δtsc × 1.19e6：100ms @ 18GHz 上限 → 1.8e9 × 1.19e6 ≈ 2.2e15，u64 安全。
    let hz = dtsc_acc
        .checked_mul(pit::PIT_FREQUENCY as u64)
        .expect("TSC delta × PIT_FREQUENCY overflow (calibration window too long?)")
        / dcnt_acc;

    // 合理性防线：[1MHz, 18GHz]。上限 18GHz 同时保证 monotonic_ns 的
    // `(tsc % hz) × 1e9 < 1.8e19 < u64::MAX` 不溢出。
    assert!(
        hz >= 1_000_000 && hz <= 18_000_000_000,
        "calibrated TSC freq out of sane range [1MHz, 18GHz]: {} Hz (dtsc={}, dcnt={})",
        hz, dtsc_acc, dcnt_acc
    );

    TSC_HZ.store(hz, Ordering::Relaxed);
    log::info!(
        "[clock] TSC calibrated: {} Hz (~{} MHz), window={} PIT counts (~{} ms)",
        hz,
        hz / 1_000_000,
        dcnt_acc,
        dcnt_acc * 1000 / pit::PIT_FREQUENCY as u64
    );
}

/// 自 CPU reset 以来的单调纳秒数（未校准返回 0）。
pub fn monotonic_ns() -> u64 {
    let hz = tsc_hz();
    if hz == 0 {
        return 0;
    }
    let tsc = rdtsc();
    // 拆分避免溢出：(tsc/hz)×1e9 ≤ u64::MAX 对 ~584 年内的 tsc 成立；
    // (tsc%hz)×1e9 < hz×1e9 ≤ 2e10×1e9 = 2e19 —— hz > 2e10 时会溢出，
    // 但校准 assert 上限（smoke 验证 < 20GHz）保证不越界。
    (tsc / hz) * 1_000_000_000 + ((tsc % hz) * 1_000_000_000) / hz
}

/// 自 CPU reset 以来的单调微秒数（未校准返回 0）。
pub fn monotonic_us() -> u64 {
    monotonic_ns() / 1_000
}

/// 自 CPU reset 以来的单调毫秒数（未校准返回 0）。
pub fn monotonic_ms() -> u64 {
    monotonic_ns() / 1_000_000
}
