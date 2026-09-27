//! 原生开机动画（Synapse AI-OS 「玄武 · 徽章」序列）。
//!
//! 视觉规格与时间轴源自 `synapse-aios/boot-animation/index.html` v2
//! （Bento Grid + Liquid Glass + FlatUI 几何徽章），在内核帧缓冲上原生重绘：
//! 无 DOM/无合成器，全部图元为整数定点光栅化。
//!
//! 运行窗口：长模式 + 0–4GiB 恒等映射 + 栈已就绪之后、cap/ipc bootstrap 之前
//! （中断保持关闭，计时走 rdtsc + PIT 通道 0 交叉校准）。
//! 不依赖分配器——全部静态缓冲。
//!
//! 无显示控制器 / VBE 模式不可用时静默跳过，不影响无头 CI 路径。

pub mod fb;
pub mod font;
mod font_data;
pub mod scene;
pub mod vbe;

use core::arch::x86_64::_rdtsc;

/// PIT 通道 0 分频值（1193180/11932 ≈ 100Hz，一个周期 10ms）。
const PIT_DIV: u16 = 11932;

/// 校准结果：每毫秒 TSC tick 数（默认 1GHz 兜底）。
static mut TSC_PER_MS: u64 = 1_000_000;
/// 动画起点 TSC。
static mut T0: u64 = 0;

#[inline]
fn out8(port: u16, v: u8) {
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") port,
            in("al") v,
            options(nostack, preserves_flags),
        );
    }
}

#[inline]
fn in8(port: u16) -> u8 {
    unsafe {
        let v: u8;
        core::arch::asm!(
            "in al, dx",
            in("dx") port,
            out("al") v,
            options(nostack, preserves_flags),
        );
        v
    }
}

/// 读 PIT 通道 0 当前计数（锁存后读 lsb/msb）。
fn pit_read_count() -> u16 {
    out8(0x43, 0x00);
    let lo = in8(0x40) as u16;
    let hi = in8(0x40) as u16;
    lo | (hi << 8)
}

/// rdtsc ↔ PIT 交叉校准：观测两次计数回绕（各 10ms）求 TSC 频率。
fn calibrate() {
    out8(0x43, 0x34); // ch0, lsb+msb, mode 2 (rate generator)
    out8(0x40, (PIT_DIV & 0xFF) as u8);
    out8(0x40, (PIT_DIV >> 8) as u8);

    let mut prev = pit_read_count();
    let mut wraps = 0u32;
    let mut mark = 0u64;
    let mut delta = 0u64;
    let mut spins = 0u32;
    while wraps < 2 && spins < 40_000_000 {
        let c = pit_read_count();
        if c > prev.wrapping_add(500) {
            // 回绕（mode2 递减计数跳回高值）
            let now = unsafe { _rdtsc() };
            if wraps == 0 {
                mark = now;
            } else {
                delta = now - mark;
            }
            wraps += 1;
        }
        prev = c;
        spins += 1;
    }
    if delta > 1000 {
        unsafe { TSC_PER_MS = delta / 10 };
    }
}

/// 动画已走过的毫秒数。
fn now_ms() -> i64 {
    let t = unsafe { _rdtsc() };
    let per = unsafe { TSC_PER_MS };
    ((t - unsafe { T0 }) / per) as i64
}

/// 播放完整开机序列（阻塞，≈11.7s；无显示设备时立即返回）。
pub fn run() {
    let info = match vbe::init() {
        Some(i) => i,
        None => {
            log::info!("[bootanim] no VBE display found — skipping boot sequence");
            return;
        }
    };
    log::info!(
        "[bootanim] framebuffer {}x{}x32, boot sequence starting",
        info.w,
        info.h
    );

    calibrate();
    fb::setup(info);
    fb::bake_background();

    unsafe { T0 = _rdtsc() };
    log::info!("[bootanim] TSC_PER_MS={}", unsafe { TSC_PER_MS });
    let mut last = -64i64;
    let mut frames = 0u32;
    loop {
        let t = now_ms();
        if t >= scene::END_MS {
            log::info!("[bootanim] END reached t={}ms", t);
            break;
        }
        if t - last >= 33 {
            scene::frame(t);
            last = t;
            frames += 1;
            if frames % 5 == 0 {
                log::info!("[bootanim] f={} t={}ms", frames, t);
            }
        }
    }
    fb::clear_black();
    log::info!("[bootanim] sequence complete ({} frames), handing over to kernel boot", frames);
}

/// GUI 演示模式（`gui_demo` feature / `xtask gui`）：无限循环播放开机序列。
///
/// 与 [`run`] 的区别：
/// * 时间轴走到 [`scene::END_MS`]（画面已淡出至全黑）后重置 `T0` 重播，永不返回；
/// * 调用点在 `kernel_bootstrap` 最前 —— 不进入 smoke、不触发 isa-debug-exit 关机，
///   QEMU 窗口保留到用户手动关闭。
///
/// 无 VBE 显示设备时退化为空闲自旋（同样不返回、不关机）。
pub fn run_forever() -> ! {
    let info = match vbe::init() {
        Some(i) => i,
        None => {
            log::info!("[bootanim] no VBE display found — gui-demo idle loop");
            loop {
                core::hint::spin_loop();
            }
        }
    };
    log::info!(
        "[bootanim] framebuffer {}x{}x32, gui-demo loop starting",
        info.w,
        info.h
    );

    calibrate();
    fb::setup(info);
    fb::bake_background();

    unsafe { T0 = _rdtsc() };
    log::info!("[bootanim] TSC_PER_MS={}", unsafe { TSC_PER_MS });
    let mut last = -64i64;
    let mut loops = 0u32;
    loop {
        let t = now_ms();
        if t >= scene::END_MS {
            // 时间轴尽头（已淡出全黑）——重置起点，开始下一轮
            unsafe { T0 = _rdtsc() };
            last = -64;
            loops += 1;
            log::info!("[bootanim] loop {} restart", loops);
            continue;
        }
        if t - last >= 33 {
            scene::frame(t);
            last = t;
        }
    }
}
