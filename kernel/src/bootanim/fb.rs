//! 帧缓冲渲染核心：双静态缓冲 + 整数 alpha 混合 + 矢量图元。
//!
//! * `BG`：开场一次性烘焙的静态背景（极光 + 技术网格 + 暗角），之后每帧
//!   仅把脏矩形从 `BG` 拷回 `BACK`，避免每帧全屏重算渐变。
//! * `BACK`：逐帧合成缓冲，脏矩形呈现时拷入 LFB。
//!
//! 单线程 + 中断关闭环境下运行，`static mut` 缓冲无竞争。
//! 全部定点整数运算（8.8 / 0–255 alpha），不依赖 FPU 精度模式。

use super::vbe::FbInfo;

/// 屏幕宽（与 `vbe::W` 一致）。
pub const W: usize = 1024;
/// 屏幕高（与 `vbe::H` 一致）。
pub const H: usize = 768;

/// 逐帧合成缓冲（3 MiB BSS）。
pub static mut BACK: [u32; W * H] = [0; W * H];
/// 静态背景缓冲（3 MiB BSS）。
static mut BG: [u32; W * H] = [0; W * H];
/// 已凝结内容缓冲（3 MiB BSS）：高 8 位存源 alpha（0xAARRGGBB），
/// 供 Liquid Glass 卡片退场时整体 alpha 混合 blit。
pub static mut SETTLED: [u32; W * H] = [0; W * H];
/// 记录模式：图元写入 `SETTLED`（保留源 alpha）而非混合进 `BACK`。
static mut RECORD: bool = false;

/// 当前 LFB 描述符（`setup` 后有效）。
pub static mut FB: FbInfo = FbInfo {
    ptr: core::ptr::null_mut(),
    w: 0,
    h: 0,
    stride: 0,
};

/// 当前脏矩形裁剪区（图元写入越界部分被丢弃）。
static mut CLIP: (i32, i32, i32, i32) = (0, 0, W as i32, H as i32);

/// 绑定 LFB（动画开始时调用一次）。
pub fn setup(info: FbInfo) {
    unsafe { FB = info };
}

/// 设置后续图元写入的裁剪矩形。
pub fn set_clip(x0: i32, y0: i32, x1: i32, y1: i32) {
    unsafe { CLIP = (x0, y0, x1, y1) };
}

/// 全屏裁剪（收束/淡出阶段用）。
pub fn set_clip_full() {
    set_clip(0, 0, W as i32, H as i32);
}

/// 颜色打包（0x00RRGGBB）。
pub const fn rgb(r: u32, g: u32, b: u32) -> u32 {
    (r << 16) | (g << 8) | b
}

/// 设计令牌（与 synapse-aios 视觉系统同源）。
pub mod color {
    /// 玄黑底色。
    pub const BG: u32 = super::rgb(0x05, 0x07, 0x0e);
    /// 主光晕青。
    pub const CYAN: u32 = super::rgb(0x4d, 0xd0, 0xe1);
    /// 副光晕紫。
    pub const VIOLET: u32 = super::rgb(0x7c, 0x4d, 0xff);
    /// 浅紫（INFO 日志）。
    pub const VIOLET_LT: u32 = super::rgb(0xa7, 0x8b, 0xfa);
    /// 在线薄荷绿。
    pub const MINT: u32 = super::rgb(0x34, 0xd3, 0x99);
    /// 主文字。
    pub const TEXT: u32 = super::rgb(0xe6, 0xf4, 0xf7);
    /// 核心高光。
    pub const CORE: u32 = super::rgb(0xdf, 0xf6, 0xfa);
    /// 龟壳底。
    pub const SHELL: u32 = super::rgb(0x0a, 0x11, 0x20);
    /// 甲片暗阶。
    pub const FACET_A: u32 = super::rgb(0x0c, 0x15, 0x26);
    /// 甲片亮阶。
    pub const FACET_B: u32 = super::rgb(0x0f, 0x1b, 0x30);
    /// 内六边形。
    pub const FACET_IN: u32 = super::rgb(0x11, 0x1e, 0x36);
    /// 核环底。
    pub const CORE_BG: u32 = super::rgb(0x06, 0x0b, 0x14);
}

#[inline]
fn blend_chan(dst: u32, src: u32, a: u32) -> u32 {
    (src * a + dst * (255 - a)) / 255
}

/// 单像素 alpha 混合写入 `BACK`（尊重裁剪）。
#[inline]
pub fn px(x: i32, y: i32, c: u32, a: u32) {
    if a == 0 {
        return;
    }
    let (cx0, cy0, cx1, cy1) = unsafe { CLIP };
    if x < cx0 || y < cy0 || x >= cx1 || y >= cy1 {
        return;
    }
    if x < 0 || y < 0 || x >= W as i32 || y >= H as i32 {
        return;
    }
    let idx = (y as usize) * W + (x as usize);
    unsafe {
        if RECORD {
            SETTLED[idx] = c | (a << 24);
            return;
        }
        let d = BACK[idx];
        if a >= 255 {
            BACK[idx] = c;
            return;
        }
        let dr = (d >> 16) & 0xFF;
        let dg = (d >> 8) & 0xFF;
        let db = d & 0xFF;
        let sr = (c >> 16) & 0xFF;
        let sg = (c >> 8) & 0xFF;
        let sb = c & 0xFF;
        BACK[idx] = rgb(blend_chan(dr, sr, a), blend_chan(dg, sg, a), blend_chan(db, sb, a));
    }
}

/// 两点 alpha 乘性叠加（用于光晕：只加不减，饱和截断）。
#[inline]
pub fn px_add(x: i32, y: i32, c: u32, a: u32) {
    if a == 0 {
        return;
    }
    if x < 0 || y < 0 || x >= W as i32 || y >= H as i32 {
        return;
    }
    let idx = (y as usize) * W + (x as usize);
    unsafe {
        let d = BG[idx];
        let dr = (d >> 16) & 0xFF;
        let dg = (d >> 8) & 0xFF;
        let db = d & 0xFF;
        let sr = ((c >> 16) & 0xFF) * a / 255;
        let sg = ((c >> 8) & 0xFF) * a / 255;
        let sb = (c & 0xFF) * a / 255;
        let or = (dr + sr).min(255);
        let og = (dg + sg).min(255);
        let ob = (db + sb).min(255);
        BG[idx] = rgb(or, og, ob);
    }
}

/// 实心矩形。
pub fn fill_rect(x0: i32, y0: i32, x1: i32, y1: i32, c: u32, a: u32) {
    let mut y = y0;
    while y < y1 {
        let mut x = x0;
        while x < x1 {
            px(x, y, c, a);
            x += 1;
        }
        y += 1;
    }
}

/// 1px 矩形描边。
pub fn stroke_rect(x0: i32, y0: i32, x1: i32, y1: i32, c: u32, a: u32) {
    hline(x0, x1 - 1, y0, c, a);
    hline(x0, x1 - 1, y1 - 1, c, a);
    vline(y0, y1 - 1, x0, c, a);
    vline(y0, y1 - 1, x1 - 1, c, a);
}

/// 水平线。
pub fn hline(x0: i32, x1: i32, y: i32, c: u32, a: u32) {
    let mut x = x0;
    while x <= x1 {
        px(x, y, c, a);
        x += 1;
    }
}

/// 垂直线。
pub fn vline(y0: i32, y1: i32, x: i32, c: u32, a: u32) {
    let mut y = y0;
    while y <= y1 {
        px(x, y, c, a);
        y += 1;
    }
}

/// 圆头线段（`th` 为直径，1 或 3）。
pub fn line(x0: i32, y0: i32, x1: i32, y1: i32, c: u32, a: u32, th: i32) {
    let dx = (x1 - x0).abs();
    let dy = (y1 - y0).abs();
    let steps = if dx > dy { dx } else { dy }.max(1);
    let r = (th - 1) / 2;
    let mut i = 0;
    while i <= steps {
        let x = x0 + (x1 - x0) * i / steps;
        let y = y0 + (y1 - y0) * i / steps;
        if r == 0 {
            px(x, y, c, a);
        } else {
            disk(x, y, r, c, a);
        }
        i += 1;
    }
}

/// 实心小圆盘。
pub fn disk(cx: i32, cy: i32, r: i32, c: u32, a: u32) {
    let mut y = -r;
    while y <= r {
        let half = isqrt(r * r - y * y);
        let mut x = -half;
        while x <= half {
            px(cx + x, cy + y, c, a);
            x += 1;
        }
        y += 1;
    }
}

/// 圆环描边（`th` 环宽）。
pub fn ring(cx: i32, cy: i32, r: i32, th: i32, c: u32, a: u32) {
    let circ = 2 * 3 * r; // 周长近似（π≈3 足够描边采样）
    let steps = circ * 2;
    let mut i = 0;
    while i < steps {
        // 定点角度：i/steps * 2π，用查表式 sin/cos 近似（8 位定点）
        let (s, co) = sincos_fixed(i * 4096 / steps);
        let x = cx + (r * co) / 256;
        let y = cy + (r * s) / 256;
        if th <= 1 {
            px(x, y, c, a);
        } else {
            disk(x, y, (th - 1) / 2, c, a);
        }
        i += 1;
    }
}

/// 虚线圆环（`on`/`off` 为弧长像素）。
pub fn ring_dashed(cx: i32, cy: i32, r: i32, on: i32, off: i32, phase: i32, c: u32, a: u32) {
    let circ = 2 * 3 * r;
    let period = on + off;
    let mut arc = 0;
    while arc < circ {
        let seg = if (arc + phase).rem_euclid(period) < on { true } else { false };
        if seg {
            let (s, co) = sincos_fixed(arc * 4096 / circ);
            let x = cx + (r * co) / 256;
            let y = cy + (r * s) / 256;
            px(x, y, c, a);
            px(x + 1, y, c, a);
        }
        arc += 1;
    }
}

/// 椭圆弧描边（旋转角 `rot_deg`，参数区间 `t0..t1` ∈ [0,4096]，3 像素宽渐变由调用方逐段着色）。
pub fn ellipse_arc_seg(
    cx: i32,
    cy: i32,
    rx: i32,
    ry: i32,
    rot_deg: i32,
    t0: i32,
    t1: i32,
    c: u32,
    a: u32,
) {
    let (rs, rc) = sincos_fixed(rot_deg * 4096 / 360);
    let steps = ((t1 - t0).abs() / 4).max(2);
    let mut i = 0;
    while i <= steps {
        let t = t0 + (t1 - t0) * i / steps;
        let (s, co) = sincos_fixed(t);
        // 椭圆参数点 → 旋转
        let ex = (rx * co) / 256;
        let ey = (ry * s) / 256;
        let x = cx + (ex * rc - ey * rs) / 256;
        let y = cy + (ex * rs + ey * rc) / 256;
        disk(x, y, 1, c, a);
        i += 1;
    }
}

/// 凸多边形扫描线填充（点列任意序，内部做边表）。
pub fn poly_fill(pts: &[(i32, i32)], c: u32, a: u32) {
    let mut ymin = i32::MAX;
    let mut ymax = i32::MIN;
    for &(_, y) in pts {
        ymin = ymin.min(y);
        ymax = ymax.max(y);
    }
    let mut y = ymin;
    while y <= ymax {
        let mut xs = [i32::MAX; 8];
        let mut n = 0;
        for i in 0..pts.len() {
            let (x0, y0) = pts[i];
            let (x1, y1) = pts[(i + 1) % pts.len()];
            if (y0 <= y && y1 > y) || (y1 <= y && y0 > y) {
                let x = x0 + (y - y0) * (x1 - x0) / (y1 - y0);
                if n < 8 {
                    xs[n] = x;
                    n += 1;
                }
            }
        }
        // 插入排序
        for i in 1..n {
            let v = xs[i];
            let mut j = i;
            while j > 0 && xs[j - 1] > v {
                xs[j] = xs[j - 1];
                j -= 1;
            }
            xs[j] = v;
        }
        let mut k = 0;
        while k + 1 < n {
            let mut x = xs[k];
            while x <= xs[k + 1] {
                px(x, y, c, a);
                x += 1;
            }
            k += 2;
        }
        y += 1;
    }
}

/// 多边形描边。
pub fn poly_stroke(pts: &[(i32, i32)], c: u32, a: u32, th: i32) {
    for i in 0..pts.len() {
        let (x0, y0) = pts[i];
        let (x1, y1) = pts[(i + 1) % pts.len()];
        line(x0, y0, x1, y1, c, a, th);
    }
}

/// 部分多边形描边（周长比例 `frac` ∈ [0,1]，用于描线动画）。
pub fn poly_stroke_partial(pts: &[(i32, i32)], frac: i32, c: u32, a: u32, th: i32) {
    if frac <= 0 {
        return;
    }
    let mut lens = [0i32; 8];
    let mut total = 0i32;
    for i in 0..pts.len() {
        let (x0, y0) = pts[i];
        let (x1, y1) = pts[(i + 1) % pts.len()];
        let l = ((x1 - x0).abs()).max((y1 - y0).abs());
        lens[i] = l;
        total += l;
    }
    let budget = total * frac.min(1000) / 1000;
    let mut used = 0;
    for i in 0..pts.len() {
        if used >= budget {
            break;
        }
        let remain = budget - used;
        let (x0, y0) = pts[i];
        let (x1, y1) = pts[(i + 1) % pts.len()];
        if remain >= lens[i] {
            line(x0, y0, x1, y1, c, a, th);
        } else {
            let f = if lens[i] == 0 { 0 } else { remain * 1000 / lens[i] };
            let xe = x0 + (x1 - x0) * f / 1000;
            let ye = y0 + (y1 - y0) * f / 1000;
            line(x0, y0, xe, ye, c, a, th);
        }
        used += lens[i];
    }
}

/// 圆角矩形填充（`r` 圆角半径）。
pub fn fill_round_rect(x0: i32, y0: i32, x1: i32, y1: i32, r: i32, c: u32, a: u32) {
    let mut y = y0;
    while y < y1 {
        // 当前行相对四角的内缩量
        let mut inset = 0;
        let dy_top = y0 + r - y;
        let dy_bot = y - (y1 - 1 - r);
        if y < y0 + r && dy_top > 0 {
            inset = r - isqrt((r * r - dy_top * dy_top).max(0));
        } else if y >= y1 - r && dy_bot > 0 {
            inset = r - isqrt((r * r - dy_bot * dy_bot).max(0));
        }
        hline(x0 + inset, x1 - 1 - inset, y, c, a);
        y += 1;
    }
}

/// 圆角矩形 1px 描边。
pub fn stroke_round_rect(x0: i32, y0: i32, x1: i32, y1: i32, r: i32, c: u32, a: u32) {
    hline(x0 + r, x1 - 1 - r, y0, c, a);
    hline(x0 + r, x1 - 1 - r, y1 - 1, c, a);
    vline(y0 + r, y1 - 1 - r, x0, c, a);
    vline(y0 + r, y1 - 1 - r, x1 - 1, c, a);
    corner_arc(x0 + r, y0 + r, r, 0, c, a);
    corner_arc(x1 - 1 - r, y0 + r, r, 1, c, a);
    corner_arc(x1 - 1 - r, y1 - 1 - r, r, 2, c, a);
    corner_arc(x0 + r, y1 - 1 - r, r, 3, c, a);
}

/// 四分之一圆弧（quadrant: 0=左上 1=右上 2=右下 3=左下）。
fn corner_arc(cx: i32, cy: i32, r: i32, quadrant: i32, c: u32, a: u32) {
    let steps = r * 2;
    let mut i = 0;
    while i <= steps {
        let ang = i * 1024 / steps; // 0..1024 = 90°
        let (s, co) = sincos_fixed(ang);
        let dx = (r * co) / 256;
        let dy = (r * s) / 256;
        let (px_, py_) = match quadrant {
            0 => (cx - dx, cy - dy),
            1 => (cx + dx, cy - dy),
            2 => (cx + dx, cy + dy),
            _ => (cx - dx, cy + dy),
        };
        px(px_, py_, c, a);
        i += 1;
    }
}

/// 把 `BG` 的矩形区域拷回 `BACK`（帧初恢复静态背景）。
pub fn restore(x0: i32, y0: i32, x1: i32, y1: i32) {
    let x0 = x0.clamp(0, W as i32);
    let x1 = x1.clamp(0, W as i32);
    let y0 = y0.clamp(0, H as i32);
    let y1 = y1.clamp(0, H as i32);
    unsafe {
        let bg = core::ptr::addr_of!(BG).cast::<u32>();
        let back = core::ptr::addr_of_mut!(BACK).cast::<u32>();
        let mut y = y0;
        while y < y1 {
            let off = (y as usize) * W + (x0 as usize);
            let len = (x1 - x0) as usize;
            core::ptr::copy_nonoverlapping(bg.add(off), back.add(off), len);
            y += 1;
        }
    }
}

/// 进入/退出记录模式（凝结静态内容到 `SETTLED`）。
pub fn set_record(on: bool) {
    unsafe { RECORD = on };
}

/// 清空 `SETTLED` 矩形（源 alpha 归零）。
pub fn settled_clear(x0: i32, y0: i32, x1: i32, y1: i32) {
    unsafe {
        let s = core::ptr::addr_of_mut!(SETTLED).cast::<u32>();
        let mut y = y0.max(0);
        while y < y1.min(H as i32) {
            let off = (y as usize) * W + x0.max(0) as usize;
            let len = (x1.min(W as i32) - x0.max(0)) as usize;
            core::ptr::write_bytes(s.add(off), 0, len);
            y += 1;
        }
    }
}

/// 把 `SETTLED` 矩形以整体 alpha（0–255）+ y 偏移混合 blit 进 `BACK`。
pub fn blit_settled(x0: i32, y0: i32, x1: i32, y1: i32, alpha: u32, dy: i32) {
    if alpha == 0 {
        return;
    }
    unsafe {
        let s = core::ptr::addr_of!(SETTLED).cast::<u32>();
        let back = core::ptr::addr_of_mut!(BACK).cast::<u32>();
        let mut y = y0.max(0);
        while y < y1.min(H as i32) {
            let ty = y + dy;
            if ty < 0 || ty >= H as i32 {
                y += 1;
                continue;
            }
            let off = (y as usize) * W;
            let doff = (ty as usize) * W;
            let mut x = x0.max(0);
            while x < x1.min(W as i32) {
                let v = *s.add(off + x as usize);
                let sa = (v >> 24) * alpha / 255;
                if sa > 0 {
                    let c = v & 0x00FF_FFFF;
                    let di = doff + x as usize;
                    if sa >= 255 {
                        *back.add(di) = c;
                    } else {
                        let d = *back.add(di);
                        let dr = (d >> 16) & 0xFF;
                        let dg = (d >> 8) & 0xFF;
                        let db = d & 0xFF;
                        let sr = (c >> 16) & 0xFF;
                        let sg = (c >> 8) & 0xFF;
                        let sb = c & 0xFF;
                        *back.add(di) = rgb(
                            blend_chan(dr, sr, sa),
                            blend_chan(dg, sg, sa),
                            blend_chan(db, sb, sa),
                        );
                    }
                }
                x += 1;
            }
            y += 1;
        }
    }
}

/// 把 `BACK` 的矩形区域呈现到 LFB。
pub fn present(x0: i32, y0: i32, x1: i32, y1: i32) {
    let fb: FbInfo = unsafe { core::ptr::addr_of!(FB).read() };
    if fb.ptr.is_null() {
        return;
    }
    let x0 = x0.clamp(0, W as i32);
    let x1 = x1.clamp(0, W as i32);
    let y0 = y0.clamp(0, H as i32);
    let y1 = y1.clamp(0, H as i32);
    unsafe {
        let back = core::ptr::addr_of!(BACK).cast::<u32>();
        let mut y = y0;
        while y < y1 {
            let off = (y as usize) * W + (x0 as usize);
            let len = (x1 - x0) as usize;
            core::ptr::copy_nonoverlapping(back.add(off), fb.ptr.add(off), len);
            y += 1;
        }
    }
}

/// 全屏清黑并呈现（动画结束交还屏幕）。
pub fn clear_black() {
    unsafe {
        core::ptr::write_bytes(core::ptr::addr_of_mut!(BACK).cast::<u8>(), 0, W * H * 4);
    }
    present(0, 0, W as i32, H as i32);
}

/// 烘焙静态背景到 `BG`：玄黑底 + 三团极光 + 64px 技术网格 + 径向暗角。
pub fn bake_background() {
    unsafe {
        let bg = core::ptr::addr_of_mut!(BG).cast::<u32>();
        let mut i = 0;
        while i < W * H {
            *bg.add(i) = color::BG;
            i += 1;
        }
    }
    // 极光（加性径向衰减）
    blob(170, 60, 520, color::CYAN, 34);
    blob(920, 730, 480, color::VIOLET, 30);
    blob(640, 400, 380, rgb(0x1e, 0x40, 0x78), 40);
    // 技术网格（64px，中心径向掩膜）
    let mut y = 0;
    while y < H as i32 {
        let mut x = 0;
        while x < W as i32 {
            let on_h = (y % 64) == 0;
            let on_v = (x % 64) == 0;
            if on_h || on_v {
                let dx = x - 512;
                let dy = y - 322;
                let d2 = dx * dx + dy * dy;
                // 掩膜：30% 内全亮，72% 外消失（平方距离比较，免 sqrt）
                let r_in = 300i32 * 300;
                let r_out = 720i32 * 720;
                if d2 < r_out {
                    let a = if d2 < r_in {
                        8
                    } else {
                        (8 * (r_out - d2) / (r_out - r_in)) as u32
                    };
                    px_add(x, y, color::CYAN, a);
                }
            }
            x += 1;
        }
        y += 1;
    }
    // 暗角（向 #020409 收敛）
    let mut y = 0;
    while y < H as i32 {
        let mut x = 0;
        while x < W as i32 {
            let dx = (x - 512) * 100 / 130; // 130% 120% 椭圆归一
            let dy = (y - 307) * 100 / 120;
            let d2 = dx * dx + dy * dy;
            let r0 = 520i32 * 520 / 100;
            if d2 > r0 {
                let t = ((d2 - r0) * 100 / (760i32 * 760 / 100 - r0)).min(100);
                let a = (t * 78 / 100) as u32;
                let idx = (y as usize) * W + (x as usize);
                unsafe {
                    let d = BG[idx];
                    let dr = (d >> 16) & 0xFF;
                    let dg = (d >> 8) & 0xFF;
                    let db = d & 0xFF;
                    BG[idx] = rgb(
                        blend_chan(dr, 0x02, a),
                        blend_chan(dg, 0x04, a),
                        blend_chan(db, 0x09, a),
                    );
                }
            }
            x += 1;
        }
        y += 1;
    }
}

/// 加性径向光团（写入 BG）。
fn blob(cx: i32, cy: i32, r: i32, c: u32, peak: u32) {
    let mut y = (cy - r).max(0);
    while y < (cy + r).min(H as i32) {
        let mut x = (cx - r).max(0);
        while x < (cx + r).min(W as i32) {
            let dx = x - cx;
            let dy = y - cy;
            let d2 = dx * dx + dy * dy;
            let r2 = r * r;
            if d2 < r2 {
                // closest-side 线性衰减 × 70% 截止
                let t = 100 - (d2 * 100 / r2);
                if t > 30 {
                    let a = (peak as i32 * (t - 30) / 70) as u32;
                    px_add(x, y, c, a);
                }
            }
            x += 1;
        }
        y += 1;
    }
}

/// 整数平方根（牛顿迭代，输入 ≥ 0）。
pub fn isqrt(n: i32) -> i32 {
    if n < 1 {
        return 0;
    }
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

/// 定点 sin/cos：输入 0..4096 = 0..360°，输出 ±256 定点（8 位小数）。
///
/// 33 采样点（11.25° 步长）线性插值：r=150 圆上弦高误差 < 0.2px。
pub fn sincos_fixed(t: i32) -> (i32, i32) {
    let t = t.rem_euclid(4096);
    const TAB: [i32; 33] = [
        0, 50, 98, 142, 181, 213, 236, 251, 256, 251, 236, 213, 181, 142, 98, 50, 0, -50, -98,
        -142, -181, -213, -236, -251, -256, -251, -236, -213, -181, -142, -98, -50, 0,
    ];
    let idx = (t / 128) as usize;
    let frac = t % 128;
    let s0 = TAB[idx];
    let s1 = TAB[idx + 1];
    let s = s0 + (s1 - s0) * frac / 128;
    // cos(t) = sin(t + 90°) = sin(t + 1024)
    let t2 = (t + 1024) % 4096;
    let idx2 = (t2 / 128) as usize;
    let frac2 = t2 % 128;
    let c0 = TAB[idx2];
    let c1 = TAB[idx2 + 1];
    let c = c0 + (c1 - c0) * frac2 / 128;
    (s, c)
}
