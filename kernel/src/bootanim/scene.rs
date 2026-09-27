//! 开机动画分镜（与 synapse-aios/boot-animation/index.html v2 时间轴同源）。
//!
//! 渲染架构（SETTLED + dirty-rect）：
//! * t=0 一次性把每张卡片的「玻璃 + 静态标签 + 静态徽记」记入 `SETTLED`
//!   （带源 alpha，0xAARRGGBB）。
//! * 每帧只恢复本帧脏矩形 → 用 `blit_settled` 整体平移/淡入退场 → 重绘小
//!   动态区（vitals 计数、clock 数字、prog 轨道头部、log 行 fade、pill 闪烁）。
//!
//! 时间轴（ms）：
//! 0–2300：Bento 卡片次第凝结；1550–4200：徽章描线；1950–6250：日志逐行；
//! 2400–4800：vitals 计数；5200–8400：进度条；8200：pill→SYSTEM ONLINE；
//! 8800：卡片错峰退场；9700：READY 收束；10900：黑淡出交还屏幕。

use super::fb;
use super::fb::color;
use super::font;

/// 总时长（ms）。
pub const END_MS: i64 = 11700;

// ---------- Bento 布局 ----------
const BRAND: Rect = Rect(24, 54, 526, 118);
const STATUS: Rect = Rect(542, 54, 763, 118);
const CLOCK: Rect = Rect(779, 54, 1000, 118);
const HERO: Rect = Rect(24, 134, 526, 590);
const LOG: Rect = Rect(542, 134, 1000, 590);
const VITALS: Rect = Rect(24, 606, 526, 714);
const PROG: Rect = Rect(542, 606, 1000, 714);

/// 矩形（x0, y0, x1, y1）。
#[derive(Clone, Copy)]
pub struct Rect(pub i32, pub i32, pub i32, pub i32);

// ---------- 缓动 / 淡入 ----------
fn clamp01(v: i64) -> i64 {
    v.clamp(0, 1000)
}
fn prog(t: i64, start: i64, dur: i64) -> i64 {
    clamp01((t - start) * 1000 / dur)
}
fn ease_out_cubic(p: i64) -> i64 {
    let q = 1000 - p;
    1000 - q * q * q / 1_000_000
}
fn ease_in_out_quad(p: i64) -> i64 {
    if p < 500 {
        2 * p * p / 1000
    } else {
        let q = -2 * p + 2000;
        1000 - q * q / 2000
    }
}
fn fade_in(t: i64, start: i64, dur: i64) -> u32 {
    (prog(t, start, dur) * 255 / 1000) as u32
}

// ---------- 卡片入场 / 退场 ----------
const CARD_IN: [(i64, Rect); 7] = [
    (500, BRAND),
    (650, STATUS),
    (800, CLOCK),
    (950, HERO),
    (1150, LOG),
    (1350, VITALS),
    (1550, PROG),
];
/// 退场错峰：vitals/prog 0，log/clock 100，hero/status 200，brand 300
const EXIT_DELAY: [i64; 7] = [300, 200, 100, 200, 100, 0, 0];

/// 卡片在时刻 t 的 (alpha 0–255, y 偏移)。
fn card_state(t: i64, idx: usize) -> (u32, i32) {
    let (start, _) = CARD_IN[idx];
    let p = ease_out_cubic(prog(t, start, 750));
    let mut a = p * 255 / 1000;
    let mut dy = 16 * (1000 - p) / 1000;
    if t >= 8800 {
        let q = prog(t, 8800 + EXIT_DELAY[idx], 850);
        a = a * (1000 - q) / 1000;
        dy -= 10 * q / 1000;
    }
    (a as u32, dy as i32)
}

// ---------- 玻璃 + 静态层（记入 SETTLED） ----------
/// Liquid Glass 卡片底（扁平化近似）：填充 + 描边 + 顶部高光。
/// alpha 在原版 HTML 是 0.045 / 0.085；但帧缓冲里太低以至于肉眼难辨。
/// 这里在 BG 上做稍亮的薄雾（fill alpha=22 约 9%，stroke=44 约 17%），
/// 既保留 Liquid Glass 质感又能在 dev/T CG 帧率下肉眼可读。
fn glass(r: Rect, a: u32) {
    if a == 0 {
        return;
    }
    fb::fill_round_rect(r.0, r.1, r.2, r.3, 18, 0xFFFFFF, 22 * a / 255);
    fb::stroke_round_rect(r.0, r.1, r.2, r.3, 18, 0xFFFFFF, 44 * a / 255);
    let hh = (r.3 - r.1) * 44 / 100;
    let mut i = 0;
    while i < hh {
        let la = 26 * a / 255 * (hh - i) as u32 / hh as u32;
        fb::hline(r.0 + 2, r.2 - 3, r.1 + 1 + i, 0xFFFFFF, la);
        i += 1;
    }
}

/// 文本助手。
fn label(x: i32, y: i32, txt: &str, a: u32) {
    font::draw_text(x, y, txt, color::TEXT, a * 32 / 100, &font::S, 1, 4);
}

fn itoa(buf: &mut [u8; 24], mut v: u32) -> &str {
    let mut tmp = [0u8; 24];
    let mut n = 0;
    if v == 0 {
        tmp[0] = b'0';
        n = 1;
    }
    while v > 0 {
        tmp[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    for i in 0..n {
        buf[i] = tmp[n - 1 - i];
    }
    core::str::from_utf8(&buf[..n]).unwrap_or("")
}

fn lerp_color(a: u32, b: u32, k: i64) -> u32 {
    let ar = ((a >> 16) & 0xFF) as i64;
    let ag = ((a >> 8) & 0xFF) as i64;
    let ab = (a & 0xFF) as i64;
    let br = ((b >> 16) & 0xFF) as i64;
    let bg = ((b >> 8) & 0xFF) as i64;
    let bb = (b & 0xFF) as i64;
    fb::rgb(
        (ar + (br - ar) * k / 1000) as u32,
        (ag + (bg - ag) * k / 1000) as u32,
        (ab + (bb - ab) * k / 1000) as u32,
    )
}

/// 迷你徽记（六边形轮廓）。
fn brand_mark(mx: i32, my: i32, ma: u32) {
    if ma == 0 {
        return;
    }
    const HEXA: [(i32, i32); 6] = [
        (120, 32), (196, 76), (196, 164), (120, 208), (44, 164), (44, 76),
    ];
    let mut pts = [(0i32, 0i32); 6];
    for i in 0..6 {
        pts[i] = (
            mx + (HEXA[i].0 - 120) * 30 / 240,
            my + (HEXA[i].1 - 120) * 30 / 240,
        );
    }
    fb::poly_stroke(&pts, color::CYAN, ma, 1);
    fb::disk(mx, my, 2, color::CORE, ma);
}

// ---------- 徽章 ----------
/// 玄武徽章（去卡通化几何）。`size` = 外接直径 px，`am` = 卡片 alpha。
fn emblem(cx: i32, cy: i32, size: i32, t: i64, am: u32) {
    if am == 0 {
        return;
    }
    let sc = size * 1000 / 240;
    let hp = |x: i32, y: i32| (cx + (x - 120) * sc / 1000, cy + (y - 120) * sc / 1000);

    // 外刻度环（96s 缓转）+ 内环
    let rot = if t > 1550 { (t - 1550) * 360 / 96000 } else { 0 };
    let a_ring = fade_in(t, 1550, 900) * am / 255;
    if a_ring > 0 {
        let r = 112 * sc / 1000;
        let circ = 2 * 3 * r;
        let phase = (rot * circ as i64 / 360) as i32;
        fb::ring_dashed(cx, cy, r, 2 * sc / 1000, 7 * sc / 1000, phase, color::CYAN, 56 * a_ring / 255);
        fb::ring(cx, cy, 104 * sc / 1000, 1, color::TEXT, 18 * a_ring / 255);
    }

    // 龟壳六边形
    const HEXA: [(i32, i32); 6] = [(120, 32), (196, 76), (196, 164), (120, 208), (44, 164), (44, 76)];
    const HEXI: [(i32, i32); 6] = [(120, 74), (160, 97), (160, 143), (120, 166), (80, 143), (80, 97)];
    let shell_a = fade_in(t, 1500, 900) * am / 255;
    let mut outer = [(0, 0); 6];
    let mut inner = [(0, 0); 6];
    for i in 0..6 {
        outer[i] = hp(HEXA[i].0, HEXA[i].1);
        inner[i] = hp(HEXI[i].0, HEXI[i].1);
    }
    if shell_a > 0 {
        fb::poly_fill(&outer, color::SHELL, shell_a);
        for i in 0..6 {
            let fa = fade_in(t, 2450 + i as i64 * 100, 700) * am / 255;
            if fa == 0 {
                continue;
            }
            let j = (i + 1) % 6;
            let quad = [outer[i], outer[j], inner[j], inner[i]];
            let c = if i % 2 == 0 { color::FACET_A } else { color::FACET_B };
            fb::poly_fill(&quad, c, fa);
            fb::poly_stroke(&quad, color::CYAN, 33 * fa / 255, 1);
        }
        let ia = fade_in(t, 2550, 700) * am / 255;
        if ia > 0 {
            fb::poly_fill(&inner, color::FACET_IN, ia);
            fb::poly_stroke(&inner, color::CYAN, 89 * ia / 255, 1);
        }
        for i in 0..6 {
            let sp = prog(t, 2900 + i as i64 * 70, 600);
            if sp == 0 {
                continue;
            }
            let (x0, y0) = inner[i];
            let (x1, y1) = outer[i];
            let xe = x0 + ((x1 - x0) as i64 * sp / 1000) as i32;
            let ye = y0 + ((y1 - y0) as i64 * sp / 1000) as i32;
            fb::line(x0, y0, xe, ye, color::CYAN, 51 * am / 255, 1);
        }
        let hf = ease_out_cubic(prog(t, 1750, 1500));
        fb::poly_stroke_partial(&outer, hf as i32, color::CYAN, 153 * am / 255, 2);
    }

    // 灵蛇轨道
    let of = ease_out_cubic(prog(t, 2700, 1500));
    if of > 0 {
        let span = 4096 * 823 / 1000;
        let drawn = span as i64 * of / 1000;
        let mut u = 0i64;
        while u < drawn {
            let k = u * 1000 / span as i64;
            let c = lerp_color(color::CYAN, color::VIOLET, k);
            let ae = if k < 300 {
                150 + 850 * k / 300
            } else if k > 700 {
                150 + 850 * (1000 - k) / 300
            } else {
                1000
            };
            let a8 = (ae * am as i64 / 1000) as u32;
            fb::ellipse_arc_seg(
                cx,
                cy,
                100 * sc / 1000,
                60 * sc / 1000,
                -18,
                u as i32,
                (u + 12) as i32,
                c,
                a8,
            );
            u += 10;
        }
        let na = fade_in(t, 3900, 600) * am / 255;
        if na > 0 {
            let (nx, ny) = orbit_point(cx, cy, sc, drawn as i32);
            fb::disk(nx, ny, 3 * sc / 1000, color::VIOLET_LT, na);
        }
    }

    // 突触射线
    for i in 0..6 {
        let ang = 90 - i as i32 * 60;
        let rp = prog(t, 3150 + i as i64 * 70, 500);
        if rp > 0 {
            let (s, c) = fb::sincos_fixed(ang * 4096 / 360);
            let r0 = 92 * sc / 1000;
            let r1 = 104 * sc / 1000;
            let x0 = cx + r0 * c / 256;
            let y0 = cy - r0 * s / 256;
            let x1 = cx + r1 * c / 256;
            let y1 = cy - r1 * s / 256;
            let xe = x0 + ((x1 - x0) as i64 * rp / 1000) as i32;
            let ye = y0 + ((y1 - y0) as i64 * rp / 1000) as i32;
            fb::line(x0, y0, xe, ye, color::CYAN, 128 * am / 255, 1);
        }
        let da = fade_in(t, 3500 + i as i64 * 70, 500) * am / 255;
        if da > 0 {
            let (s, c) = fb::sincos_fixed(ang * 4096 / 360);
            let r = 108 * sc / 1000;
            fb::disk(cx + r * c / 256, cy - r * s / 256, 2 * sc / 1000, color::CYAN, da);
        }
    }

    // 中心能量核
    let ca = fade_in(t, 3350, 800) * am / 255;
    if ca > 0 {
        let mut scale = 400 + 600 * ease_out_cubic(prog(t, 3350, 800)) / 1000;
        if t > 4200 {
            let (s, _) = fb::sincos_fixed((((t - 4200) % 3600) * 4096 / 3600) as i32);
            scale = scale * (1000 + 55 * s as i64 / 256) / 1000;
        }
        let ra = fade_in(t, 3500, 600) * am / 255;
        if ra > 0 {
            let rot2 = -((t - 3500) * 360 / 16000);
            let r = 21 * sc / 1000;
            let circ = 2 * 3 * r;
            let phase = (rot2 * circ as i64 / 360) as i32;
            fb::ring_dashed(cx, cy, r, 3 * sc / 1000, 9 * sc / 1000, phase, color::CYAN, 128 * ra / 255);
        }
        let r_disk = (12 * sc / 1000) as i64 * scale / 1000;
        fb::disk(cx, cy, r_disk as i32, color::CORE_BG, ca);
        fb::ring(cx, cy, r_disk as i32, 1, color::CYAN, ca);
        fb::disk(cx, cy, (4 * sc / 1000) as i64 as i32 * scale as i32 / 1000, color::CORE, ca);
        fb::disk(cx, cy, 1, color::SHELL, ca);
    }
}

fn orbit_point(cx: i32, cy: i32, sc: i32, tpar: i32) -> (i32, i32) {
    let (s, c) = fb::sincos_fixed(tpar);
    let ex = 100 * sc / 1000 * c / 256;
    let ey = 60 * sc / 1000 * s / 256;
    let (rs, rc) = fb::sincos_fixed(-18 * 4096 / 360);
    (
        cx + (ex * rc - ey * rs) / 256,
        cy + (ex * rs + ey * rc) / 256,
    )
}

// ---------- 日志元数据 ----------
const LOGS: [(i64, &str, u8, &str); 11] = [
    (1950, "0.031", 0, "stage2: EDD ok, A20 open"),
    (2380, "0.058", 0, "long mode, identity-map 0-4 GiB"),
    (2810, "0.112", 0, "kernel 5.1 MiB @ 0x200000"),
    (3240, "0.190", 1, "COM1 online, 115200 8N1"),
    (3670, "0.244", 0, "GDT/TSS loaded, IST1 armed"),
    (4100, "0.301", 0, "IDT 256 vectors set"),
    (4530, "0.418", 0, "PIC remapped, PIT 100 Hz"),
    (4960, "0.527", 0, "heap online, first-fit 4 MiB"),
    (5390, "0.640", 1, "TSC 3.199 GHz calibrated"),
    (5820, "0.702", 2, "smoke: 57/57 checks passed"),
    (6250, "0.815", 0, "agent runtime ready"),
];

// ---------- 动态重绘（每帧 dirty 区） ----------

/// STATUS 卡片：动态内容 = pill 文本+闪烁点。
fn draw_status_dyn(r: Rect, t: i64, a: u32) {
    let pa = fade_in(t, 1300, 600) * a / 255;
    if pa == 0 {
        return;
    }
    let online = t >= 8200;
    let (cc, ca) = if online {
        (color::MINT, 242)
    } else {
        (color::CYAN, 229)
    };
    let txt = if online { "SYSTEM ONLINE" } else { "BOOTING" };
    let w = font::text_width(txt, &font::S, 1, 3);
    let x = r.0 + 20;
    let y = r.1 + 30;
    fb::fill_round_rect(x, y, x + w + 46, y + 24, 12, cc, 13 * pa / 255);
    fb::stroke_round_rect(x, y, x + w + 46, y + 24, 12, cc, 64 * pa / 255);
    let blink: u32 = if online {
        255
    } else {
        let (s, _) = fb::sincos_fixed(((t % 1400) * 4096 / 1400) as i32);
        (64 + 191 * (256 - s.abs()) / 256) as u32
    };
    fb::disk(x + 14, y + 12, 3, cc, pa * blink / 255);
    font::draw_text(x + 26, y + 6, txt, cc, pa * ca / 255, &font::S, 1, 3);
}

/// CLOCK 卡片：动态 = 数字（每帧重绘）。
fn draw_clock_dyn(r: Rect, t: i64, a: u32) {
    let va = fade_in(t, 1500, 600) * a / 255;
    if va == 0 {
        return;
    }
    let ms = (t - 1200).max(0);
    let mut buf = [0u8; 24];
    let sec = itoa(&mut buf, (ms / 1000) as u32);
    font::draw_text(r.0 + 20, r.1 + 28, sec, color::TEXT, va, &font::M, 1, 1);
    let w = font::text_width(sec, &font::M, 1, 1);
    let mut buf2 = [0u8; 24];
    let cent = (ms % 1000) / 10;
    let cs = itoa(&mut buf2, cent as u32);
    let dec = if cent < 10 { ".0" } else { "." };
    font::draw_text(r.0 + 20 + w, r.1 + 28, dec, color::TEXT, va, &font::M, 1, 1);
    let w2 = font::text_width(dec, &font::M, 1, 1);
    font::draw_text(r.0 + 20 + w + w2, r.1 + 28, cs, color::TEXT, va, &font::M, 1, 1);
    let w3 = font::text_width(cs, &font::M, 1, 1);
    font::draw_text(r.0 + 20 + w + w2 + w3 + 4, r.1 + 38, "s", color::TEXT, va * 32 / 100, &font::S, 1, 1);
}

/// HERO 动态区：徽章本身（动画很多，必须每帧）。
fn draw_hero_emblem_dyn(t: i64, a: u32) {
    let cx = (HERO.0 + HERO.2) / 2;
    let cy = (HERO.1 + HERO.3) / 2 - 20;
    emblem(cx, cy, 330, t, a);
}

/// LOG 卡片：动态 = 逐行 fade-in。
fn draw_log_dyn(r: Rect, t: i64, a: u32) {
    let la_outer = a;
    let bottom = r.3 - 16;
    let mut n = 0;
    for &(lt, _, _, _) in LOGS.iter() {
        if t >= lt {
            n += 1;
        }
    }
    for (i, &(lt, ts, kind, msg)) in LOGS.iter().enumerate() {
        if t < lt {
            continue;
        }
        let la = fade_in(t, lt, 450) * la_outer / 255;
        let rise = (6 * (1000 - ease_out_cubic(prog(t, lt, 450))) / 1000) as i32;
        let y = bottom - 22 * (n as i32 - i as i32) + rise;
        let x = r.0 + 20;
        let mut buf = [0u8; 24];
        buf[..ts.len()].copy_from_slice(ts.as_bytes());
        let tsx = core::str::from_utf8(&buf[..ts.len()]).unwrap_or("");
        font::draw_text(x, y, "[", color::TEXT, la * 32 / 100, &font::S, 1, 0);
        let w0 = font::text_width("[", &font::S, 1, 0);
        font::draw_text(x + w0, y, tsx, color::TEXT, la * 32 / 100, &font::S, 1, 0);
        let w1 = font::text_width(tsx, &font::S, 1, 0);
        font::draw_text(x + w0 + w1, y, "]", color::TEXT, la * 32 / 100, &font::S, 1, 0);
        let tagc = match kind {
            0 => color::CYAN,
            1 => color::VIOLET_LT,
            _ => color::TEXT,
        };
        let tag_s = match kind {
            0 => "[ OK ]",
            1 => "[INFO]",
            _ => "[ ** ]",
        };
        let x2 = x + 8 * 9 + 8;
        font::draw_text(x2, y, tag_s, tagc, la * 95 / 100, &font::S, 1, 0);
        let x3 = x2 + 7 * 9 + 6;
        font::draw_text(x3, y, msg, color::TEXT, la * 72 / 100, &font::S, 1, 0);
    }
}

/// VITALS 动态区：3 列数字 + 进度条填充（count-up）。
fn draw_vitals_dyn(r: Rect, t: i64, a: u32) {
    let e = ease_out_cubic(prog(t, 2400, 2400));
    let cols: [(&str, i64, u8, u32, u32); 3] = [
        ("NEURAL LOAD", 78 * e / 1000, 0, color::CYAN, color::CYAN),
        ("MEMORY", 19 * e / 1000, 1, color::VIOLET, color::VIOLET_LT),
        ("SYNAPSE SYNC", 100 * e / 1000, 0, color::MINT, color::MINT),
    ];
    let cw = (r.2 - r.0 - 40) / 3;
    for (i, &(_, val, dec, c0, c1)) in cols.iter().enumerate() {
        let va = fade_in(t, 1900 + i as i64 * 150, 700) * a / 255;
        if va == 0 {
            continue;
        }
        let x = r.0 + 20 + i as i32 * cw;
        let mut buf = [0u8; 24];
        let num = itoa(&mut buf, val as u32);
        font::draw_text(x, r.1 + 40, num, color::TEXT, va, &font::M, 1, 1);
        let mut w = font::text_width(num, &font::M, 1, 1);
        if dec == 1 {
            let mut buf2 = [0u8; 24];
            let frac = itoa(&mut buf2, (val * 10) as u32 % 10);
            font::draw_text(x + w, r.1 + 40, ".", color::TEXT, va, &font::M, 1, 1);
            w += font::text_width(".", &font::M, 1, 1);
            font::draw_text(x + w, r.1 + 40, frac, color::TEXT, va, &font::M, 1, 1);
            w += font::text_width(frac, &font::M, 1, 1);
        }
        let unit = if dec == 1 { "/ 16 GB" } else { "%" };
        font::draw_text(x + w + 4, r.1 + 50, unit, color::TEXT, va * 32 / 100, &font::S, 1, 1);
        // 进度条底 + 填充
        let bw = cw - 36;
        let by = r.1 + 76;
        fb::fill_round_rect(x, by, x + bw, by + 3, 1, color::TEXT, 20 * va / 255);
        let max = if dec == 1 { 19 } else { if i == 0 { 78 } else { 100 } };
        let fw = bw * val as i32 / max;
        if fw > 2 {
            let mut xx = 0;
            while xx < fw {
                let k = xx as i64 * 1000 / fw as i64;
                fb::vline(by, by + 2, x + xx, lerp_color(c0, c1, k / 2 + 500), va * 80 / 100);
                xx += 1;
            }
        }
        if i > 0 {
            fb::vline(r.1 + 20, r.3 - 20, x - 18, color::TEXT, a * 9 / 100);
        }
    }
}

/// PROG 动态区：阶段标签 + 百分比 + 轨道填充 + 头点。
fn draw_prog_dyn(r: Rect, t: i64, a: u32) {
    let p = ease_in_out_quad(prog(t, 5200, 3200));
    let pct = p;
    let stage = if pct >= 1000 {
        "READY"
    } else if pct >= 760 {
        "SERVICES"
    } else if pct >= 480 {
        "DRIVERS"
    } else if pct >= 180 {
        "KERNEL"
    } else {
        "LOADER"
    };
    font::draw_text(r.0 + 20, r.1 + 20, stage, color::CYAN, a * 75 / 100, &font::S, 1, 4);
    let mut buf = [0u8; 24];
    let num = itoa(&mut buf, (pct / 10) as u32);
    let nw = font::text_width(num, &font::M, 1, 1);
    font::draw_text(r.2 - 20 - nw - 12, r.1 + 14, num, color::TEXT, a, &font::M, 1, 1);
    font::draw_text(r.2 - 20 - 10, r.1 + 24, "%", color::TEXT, a * 32 / 100, &font::S, 1, 1);
    let ty = r.1 + 56;
    let tx0 = r.0 + 20;
    let tx1 = r.2 - 20;
    let fw = (tx1 - tx0) * pct as i32 / 1000;
    if fw > 2 {
        let mut xx = 0;
        while xx < fw {
            let k = xx as i64 * 1000 / fw as i64;
            fb::vline(ty, ty + 1, tx0 + xx, lerp_color(color::CYAN, color::VIOLET, k), a * 90 / 100);
            xx += 1;
        }
        fb::disk(tx0 + fw, ty + 1, 3, color::CORE, a);
    }
}

/// BRAND 动态区：迷你徽记 + SYNAPSE/AI-OS + chip + edition。
fn draw_brand_dyn(r: Rect, t: i64, a: u32) {
    let cy = (r.1 + r.3) / 2;
    let mx = r.0 + 20 + 15;
    let ma = fade_in(t, 1900, 600) * a / 255;
    brand_mark(mx, cy, ma);
    let na = fade_in(t, 1700, 800) * a / 255;
    if na > 0 {
        let x = r.0 + 64;
        font::draw_text(x, cy - 9, "SYNAPSE", color::TEXT, na, &font::S, 1, 5);
        let w = font::text_width("SYNAPSE", &font::S, 1, 5);
        font::draw_text(x + w + 6, cy - 9, "AI-OS", color::CYAN, na, &font::S, 1, 5);
    }
    let ca = fade_in(t, 2000, 800) * a / 255;
    if ca > 0 {
        let txt = "v0.1 PHASE-1";
        let w = font::text_width(txt, &font::S, 1, 2);
        let x = r.0 + 64 + font::text_width("SYNAPSE AI-OS", &font::S, 1, 5) + 24;
        fb::fill_round_rect(x, cy - 10, x + w + 20, cy + 10, 10, color::CYAN, 15 * ca / 255);
        fb::stroke_round_rect(x, cy - 10, x + w + 20, cy + 10, 10, color::CYAN, 71 * ca / 255);
        font::draw_text(x + 10, cy - 7, txt, color::CYAN, 217 * ca / 255, &font::S, 1, 2);
    }
    let sa = fade_in(t, 2200, 800) * a / 255;
    if sa > 0 {
        let txt = "XUANWU EDITION";
        let w = font::text_width(txt, &font::S, 1, 4);
        font::draw_text(r.2 - 20 - w, cy - 7, txt, color::TEXT, sa * 32 / 100, &font::S, 1, 4);
    }
}

/// READY 阶段整体（动态）。
fn draw_ready(t: i64) -> u32 {
    let a = fade_in(t, 9700, 1000);
    if a == 0 {
        return 0;
    }
    emblem(512, 320, 110, t, a);
    let txt = "SYNAPSE AI-OS";
    let w = font::text_width(txt, &font::M, 1, 8);
    font::draw_text(512 - w / 2, 400, txt, color::TEXT, a * 75 / 100, &font::M, 1, 8);
    let sub = "SYSTEM READY - XUANWU NEURAL CORE ONLINE";
    let sw = font::text_width(sub, &font::S, 1, 4);
    fb::disk(512 - sw / 2 - 14, 436, 2, color::MINT, a);
    font::draw_text(512 - sw / 2, 430, sub, color::TEXT, a * 32 / 100, &font::S, 1, 4);
    a
}

// ---------- 一次性烘焙（首帧调用一次） ----------
static mut SETTLE_DONE: bool = false;

/// 烘焙 `BG`（静态背景）+ `SETTLED`（每张卡片的玻璃 + 静态标签）。
/// 仅首帧执行一次；之后每帧只恢复 + blit + 动态层。
fn setup_once() {
    if unsafe { SETTLE_DONE } {
        return;
    }
    fb::bake_background();
    fb::settled_clear(0, 0, 1024, 768);
    fb::set_record(true);
    // 每张卡的玻璃（全 alpha）
    for &(_, r) in CARD_IN.iter() {
        glass(r, 255);
    }
    // HERO 静态：角刻 + 题注 + 边线
    let r = HERO;
    let l = 10;
    let m = 12;
    fb::hline(r.0 + m, r.0 + m + l, r.1 + m, color::CYAN, 102);
    fb::vline(r.1 + m, r.1 + m + l, r.0 + m, color::CYAN, 102);
    fb::hline(r.2 - m - l, r.2 - m, r.1 + m, color::CYAN, 102);
    fb::vline(r.1 + m, r.1 + m + l, r.2 - m, color::CYAN, 102);
    fb::hline(r.0 + m, r.0 + m + l, r.3 - m, color::VIOLET, 102);
    fb::vline(r.3 - m - l, r.3 - m, r.0 + m, color::VIOLET, 102);
    fb::hline(r.2 - m - l, r.2 - m, r.3 - m, color::VIOLET, 102);
    fb::vline(r.3 - m - l, r.3 - m, r.2 - m, color::VIOLET, 102);
    let txt = "XUANWU - NEURAL CORE";
    let tw = font::text_width(txt, &font::S, 1, 6);
    let ty = r.3 - 46;
    font::draw_text(512 - tw / 2, ty, txt, color::TEXT, 55, &font::S, 1, 6);
    fb::hline(512 - tw / 2 - 54, 512 - tw / 2 - 10, ty + 6, color::CYAN, 50);
    fb::hline(512 + tw / 2 + 10, 512 + tw / 2 + 54, ty + 6, color::VIOLET, 50);
    // STATUS 标签
    label(STATUS.0 + 20, STATUS.1 + 12, "KERNEL STATE", 255);
    // CLOCK 标签
    label(CLOCK.0 + 20, CLOCK.1 + 12, "UPTIME", 255);
    // LOG 标签 + COM1 标记 + 分隔线
    label(LOG.0 + 20, LOG.1 + 16, "BOOT LOG", 255);
    let dev = "COM1 115200 8N1";
    let dw = font::text_width(dev, &font::S, 1, 1);
    font::draw_text(LOG.2 - 20 - dw, LOG.1 + 18, dev, color::TEXT, 255 * 32 / 100, &font::S, 1, 1);
    fb::hline(LOG.0 + 20, LOG.2 - 21, LOG.1 + 36, color::TEXT, 255 * 9 / 100);
    // VITALS 标签
    let vcw = (VITALS.2 - VITALS.0 - 40) / 3;
    let vnames = ["NEURAL LOAD", "MEMORY", "SYNAPSE SYNC"];
    for i in 0..3 {
        let x = VITALS.0 + 20 + i as i32 * vcw;
        label(x, VITALS.1 + 18, vnames[i], 255);
    }
    // VITALS 分隔线
    for i in 1..3 {
        let x = VITALS.0 + 20 + i as i32 * vcw - 18;
        fb::vline(VITALS.1 + 20, VITALS.3 - 20, x, color::TEXT, 9 * 255 / 100);
    }
    // PROG 刻度标签 + 轨道底
    let pty = PROG.1 + 56;
    let ptx0 = PROG.0 + 20;
    let ptx1 = PROG.2 - 20;
    let pinner = ptx1 - ptx0;
    let marks = ["LOADER", "KERNEL", "DRIVERS", "SERVICES", "READY"];
    for i in 0..5 {
        let w = font::text_width(marks[i], &font::S, 1, 1);
        let x = if i == 0 {
            ptx0
        } else if i == 4 {
            ptx1 - w
        } else {
            ptx0 + pinner * i as i32 / 4 - w / 2
        };
        font::draw_text(x, pty + 14, marks[i], color::TEXT, 255 * 22 / 100, &font::S, 1, 1);
    }
    fb::fill_round_rect(ptx0, pty, ptx1, pty + 2, 1, color::CYAN, 31);
    fb::set_record(false);
    unsafe { SETTLE_DONE = true };
}

// ---------- 帧驱动 ----------
/// 渲染时刻 `t`（ms）的一帧。
pub fn frame(t: i64) {
    setup_once();
    fb::restore(0, 0, 1024, 768);
    fb::set_clip(0, 0, 1024, 768);

    if t < 9700 {
        for (idx, &(_, r)) in CARD_IN.iter().enumerate() {
            let (a, dy) = card_state(t, idx);
            if a == 0 { continue; }
            // SETTLED → BACK（带 alpha + dy）
            fb::blit_settled(r.0, r.1, r.2, r.3, a, dy);
            // 动态层（在卡片矩形内，乘 a）
            fb::set_clip(r.0, r.1 + dy.min(0), r.2, r.3 + dy.max(0));
            match idx {
                0 => draw_brand_dyn(r, t, a),
                1 => draw_status_dyn(r, t, a),
                2 => draw_clock_dyn(r, t, a),
                3 => draw_hero_emblem_dyn(t, a),
                4 => draw_log_dyn(r, t, a),
                5 => draw_vitals_dyn(r, t, a),
                _ => draw_prog_dyn(r, t, a),
            }
        }
    } else {
        let _a = draw_ready(t);
        if t >= 10900 {
            let fa = fade_in(t, 10900, 800);
            fb::fill_rect(0, 0, 1024, 768, 0x000000, fa);
        }
    }
    fb::set_clip(0, 0, 1024, 768);
    fb::present(0, 0, 1024, 768);
}