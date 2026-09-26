//! 玄武徽章 Logo 导出 —— 矢量重绘 boot-animation/index.html 的 `#xw-emblem`
//!
//! ```text
//! cargo run --release --example logo --features renderer -p synapse-display
//! cargo run --release --example logo --features renderer -p synapse-display -- --size 1024 --out-dir synapse-aios/logo
//! ```
//!
//! 输出 4 版 PNG：
//! 1. `synapse-xuanwu-color.png`          全彩 · 透明底
//! 2. `synapse-xuanwu-color-on-dark.png`  全彩 · 玄黑底 + 光晕
//! 3. `synapse-xuanwu-mono-dark.png`      单色亮（深色 UI 适用）· 透明底
//! 4. `synapse-xuanwu-mono-light.png`     单色暗（浅色 UI 适用）· 透明底

use tiny_skia::{
    Color, FillRule, GradientStop, LineCap, LineJoin, LinearGradient, Paint,
    Path, PathBuilder, Pixmap, Point, Shader, SpreadMode, Stroke, StrokeDash,
    Transform,
};

/// 配色模式
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Color,
    MonoDark,
    MonoLight,
}

/// 绘制上下文（viewBox 240 → 目标尺寸缩放）
struct Ctx {
    mode: Mode,
    s: f32,
}

impl Ctx {
    /// 取色：Color 模式原样；Mono 模式统一色相、alpha 可覆盖（填充降级为层次）
    fn c(&self, r: f32, g: f32, b: f32, a: f32, mono_a: Option<f32>) -> Color {
        // mono_a 仅 Mono 模式生效（填充降级为层次透明度）
        let a = match self.mode {
            Mode::Color => a,
            _ => mono_a.unwrap_or(a),
        };
        match self.mode {
            Mode::Color => Color::from_rgba(r, g, b, a),
            Mode::MonoDark => Color::from_rgba(0.902, 0.957, 0.969, a),
            Mode::MonoLight => Color::from_rgba(0.070, 0.070, 0.080, a),
        }
        .unwrap_or(Color::BLACK)
    }

    fn p(&self, v: f32) -> f32 {
        v * self.s
    }

    fn fill(&self, px: &mut Pixmap, path: &Path, col: Color) {
        let paint = Paint {
            shader: Shader::SolidColor(col),
            anti_alias: true,
            ..Default::default()
        };
        px.fill_path(path, &paint, FillRule::Winding, Transform::identity(), None);
    }

    fn stroke(
        &self,
        px: &mut Pixmap,
        path: &Path,
        col: Color,
        w: f32,
        dash: Option<(Vec<f32>, f32)>,
        round: bool,
    ) {
        let paint = Paint {
            shader: Shader::SolidColor(col),
            anti_alias: true,
            ..Default::default()
        };
        let stroke = Stroke {
            width: w * self.s,
            dash: dash.and_then(|(d, o)| {
                StrokeDash::new(d.into_iter().map(|v| v * self.s).collect(), o * self.s)
            }),
            line_cap: if round { LineCap::Round } else { LineCap::Butt },
            line_join: LineJoin::Round,
            ..Default::default()
        };
        px.stroke_path(path, &paint, &stroke, Transform::identity(), None);
    }

    fn poly(&self, pts: &[(f32, f32)]) -> Path {
        let mut pb = PathBuilder::new();
        for (i, (x, y)) in pts.iter().enumerate() {
            if i == 0 {
                pb.move_to(self.p(*x), self.p(*y));
            } else {
                pb.line_to(self.p(*x), self.p(*y));
            }
        }
        pb.close();
        pb.finish().expect("polygon path")
    }

    fn line(&self, x0: f32, y0: f32, x1: f32, y1: f32) -> Path {
        let mut pb = PathBuilder::new();
        pb.move_to(self.p(x0), self.p(y0));
        pb.line_to(self.p(x1), self.p(y1));
        pb.finish().expect("line path")
    }

    fn circle(&self, cx: f32, cy: f32, r: f32) -> Path {
        PathBuilder::from_circle(self.p(cx), self.p(cy), self.p(r)).expect("circle path")
    }
}

/// 玄武徽章（240 viewBox 坐标，对齐 index.html `#xw-emblem`）
fn draw_emblem(px: &mut Pixmap, ctx: &Ctx) {
    let cyan = |a: f32| ctx.c(0.302, 0.816, 0.882, a, None);
    let violet = |a: f32| ctx.c(0.486, 0.302, 1.0, a, None);

    // 外圈刻度环 + 内环
    let dash_ring = ctx.circle(120.0, 120.0, 112.0);
    ctx.stroke(px, &dash_ring, cyan(0.22), 1.0, Some((vec![2.0, 7.0], 0.0)), false);
    let inner_ring = ctx.circle(120.0, 120.0, 104.0);
    ctx.stroke(px, &inner_ring, ctx.c(0.902, 0.957, 0.969, 0.07, None), 1.0, None, false);

    // 神经突触放射线 + 末端节点
    let rays: [((f32, f32), (f32, f32)); 6] = [
        ((120.0, 28.0), (120.0, 16.0)),
        ((199.7, 74.0), (210.1, 68.0)),
        ((199.7, 166.0), (210.1, 172.0)),
        ((120.0, 212.0), (120.0, 224.0)),
        ((40.3, 166.0), (29.9, 172.0)),
        ((40.3, 74.0), (29.9, 68.0)),
    ];
    for (a, b) in rays {
        let path = ctx.line(a.0, a.1, b.0, b.1);
        ctx.stroke(px, &path, cyan(0.5), 1.0, None, true);
    }
    let dots = [
        (120.0, 12.0),
        (213.6, 66.0),
        (213.6, 174.0),
        (120.0, 228.0),
        (26.4, 174.0),
        (26.4, 66.0),
    ];
    for (x, y) in dots {
        let d = ctx.circle(x, y, 2.0);
        ctx.fill(px, &d, cyan(1.0));
    }

    // 灵蛇轨道（-18° 倾斜椭圆弧，428/520 周长的开口环）
    let (cos_t, sin_t) = (0.951_06f32, -0.309_02f32);
    let span = core::f32::consts::TAU * (428.0 / 520.0);
    let mut pb = PathBuilder::new();
    let steps = 96;
    for i in 0..=steps {
        let t = span * i as f32 / steps as f32;
        let dx = 100.0 * t.cos();
        let dy = 60.0 * t.sin();
        let rx = 120.0 + dx * cos_t - dy * sin_t;
        let ry = 120.0 + dx * sin_t + dy * cos_t;
        if i == 0 {
            pb.move_to(ctx.p(rx), ctx.p(ry));
        } else {
            pb.line_to(ctx.p(rx), ctx.p(ry));
        }
    }
    let orbit = pb.finish().expect("orbit path");
    match ctx.mode {
        Mode::Color => {
            let shader = LinearGradient::new(
                Point::from_xy(0.0, ctx.p(240.0)),
                Point::from_xy(ctx.p(240.0), 0.0),
                vec![
                    GradientStop::new(0.0, cyan(0.15)),
                    GradientStop::new(0.3, cyan(1.0)),
                    GradientStop::new(0.7, violet(1.0)),
                    GradientStop::new(1.0, violet(0.15)),
                ],
                SpreadMode::Pad,
                Transform::identity(),
            )
            .expect("orbit gradient");
            let paint = Paint {
                shader,
                anti_alias: true,
                ..Default::default()
            };
            let stroke = Stroke {
                width: 3.0 * ctx.s,
                line_cap: LineCap::Round,
                ..Default::default()
            };
            px.stroke_path(&orbit, &paint, &stroke, Transform::identity(), None);
        }
        _ => ctx.stroke(px, &orbit, ctx.c(0.9, 0.95, 0.97, 0.8, None), 3.0, None, true),
    }
    let node = ctx.circle(211.6, 121.8, 2.6);
    ctx.fill(px, &node, ctx.c(0.604, 0.482, 1.0, 1.0, Some(0.9)));

    // 龟壳六边形 + 甲片明暗阶
    let shell = [
        (120.0, 32.0),
        (196.2, 76.0),
        (196.2, 164.0),
        (120.0, 208.0),
        (43.8, 164.0),
        (43.8, 76.0),
    ];
    let shell_path = ctx.poly(&shell);
    ctx.fill(px, &shell_path, ctx.c(0.039, 0.067, 0.125, 1.0, Some(0.10)));

    let facets: [([(f32, f32); 4], f32); 6] = [
        ([(120.0, 32.0), (196.2, 76.0), (159.8, 97.0), (120.0, 74.0)], 0.0),
        ([(196.2, 76.0), (196.2, 164.0), (159.8, 143.0), (159.8, 97.0)], 1.0),
        ([(196.2, 164.0), (120.0, 208.0), (120.0, 166.0), (159.8, 143.0)], 0.0),
        ([(120.0, 208.0), (43.8, 164.0), (80.2, 143.0), (120.0, 166.0)], 1.0),
        ([(43.8, 164.0), (43.8, 76.0), (80.2, 97.0), (80.2, 143.0)], 0.0),
        ([(43.8, 76.0), (120.0, 32.0), (120.0, 74.0), (80.2, 97.0)], 1.0),
    ];
    for (pts, alt) in facets {
        let path = ctx.poly(&pts);
        let fill = if alt < 0.5 {
            ctx.c(0.047, 0.082, 0.149, 1.0, Some(0.16))
        } else {
            ctx.c(0.059, 0.106, 0.188, 1.0, Some(0.22))
        };
        ctx.fill(px, &path, fill);
        ctx.stroke(px, &path, cyan(0.13), 1.0, None, false);
    }

    // 甲片辐条
    let spokes: [((f32, f32), (f32, f32)); 6] = [
        ((120.0, 74.0), (120.0, 32.0)),
        ((159.8, 97.0), (196.2, 76.0)),
        ((159.8, 143.0), (196.2, 164.0)),
        ((120.0, 166.0), (120.0, 208.0)),
        ((80.2, 143.0), (43.8, 164.0)),
        ((80.2, 97.0), (43.8, 76.0)),
    ];
    for (a, b) in spokes {
        let path = ctx.line(a.0, a.1, b.0, b.1);
        ctx.stroke(px, &path, cyan(0.2), 1.0, None, false);
    }

    // 内六边形 + 壳轮廓
    let inner_hex = ctx.poly(&[
        (120.0, 74.0),
        (159.8, 97.0),
        (159.8, 143.0),
        (120.0, 166.0),
        (80.2, 143.0),
        (80.2, 97.0),
    ]);
    ctx.fill(px, &inner_hex, ctx.c(0.067, 0.118, 0.212, 1.0, Some(0.28)));
    ctx.stroke(px, &inner_hex, cyan(0.35), 1.0, None, false);
    ctx.stroke(px, &shell_path, cyan(0.6), 1.6, None, false);

    // 中心能量核
    let core_ring = ctx.circle(120.0, 120.0, 21.0);
    ctx.stroke(px, &core_ring, cyan(0.5), 1.0, Some((vec![3.0, 9.0], 0.0)), false);
    let core = ctx.circle(120.0, 120.0, 12.5);
    ctx.fill(px, &core, ctx.c(0.024, 0.043, 0.078, 1.0, Some(0.15)));
    ctx.stroke(px, &core, cyan(1.0), 1.3, None, false);
    let core_dot = ctx.circle(120.0, 120.0, 4.6);
    ctx.fill(px, &core_dot, ctx.c(0.875, 0.965, 0.980, 1.0, Some(1.0)));
    let core_hole = ctx.circle(120.0, 120.0, 1.6);
    ctx.fill(px, &core_hole, ctx.c(0.039, 0.067, 0.125, 1.0, Some(0.35)));
}

/// premultiplied → straight alpha（透明底 PNG 需要）
fn unpremultiply(data: &[u8]) -> Vec<u8> {
    let mut out = data.to_vec();
    for px in out.chunks_mut(4) {
        let a = px[3] as u32;
        if a == 0 {
            px[0] = 0;
            px[1] = 0;
            px[2] = 0;
        } else if a < 255 {
            for c in px[0..3].iter_mut() {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    out
}

fn render_variant(size: u32, mode: Mode, on_dark: bool) -> Vec<u8> {
    let mut px = Pixmap::new(size, size).expect("pixmap");
    let ctx = Ctx { mode, s: size as f32 / 240.0 };

    if on_dark {
        let bg = PathBuilder::from_rect(
            tiny_skia::Rect::from_xywh(0.0, 0.0, size as f32, size as f32).unwrap(),
        );
        ctx.fill(&mut px, &bg, Color::from_rgba8(0x05, 0x07, 0x0E, 255));
        // 光晕（三层同心圆近似 radial glow）
        for (r, a) in [(118.0, 0.04), (96.0, 0.05), (76.0, 0.07)] {
            let g = ctx.circle(120.0, 120.0, r);
            ctx.fill(&mut px, &g, Color::from_rgba(0.302, 0.816, 0.882, a).unwrap());
        }
        let vg = ctx.circle(150.0, 150.0, 60.0);
        ctx.fill(&mut px, &vg, Color::from_rgba(0.486, 0.302, 1.0, 0.05).unwrap());
    }

    draw_emblem(&mut px, &ctx);

    let data = if on_dark {
        px.data().to_vec()
    } else {
        unpremultiply(px.data())
    };
    data
}

fn save(size: u32, mode: Mode, on_dark: bool, dir: &str, name: &str) {
    let data = render_variant(size, mode, on_dark);
    let img = image::RgbaImage::from_raw(size, size, data).expect("rgba image");
    let path = format!("{}/{}", dir, name);
    img.save(&path).expect("save png");
    println!("[logo] {} ({}x{})", path, size, size);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut size = 512u32;
    let mut out_dir = "synapse-aios/logo".to_string();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--size" => {
                if i + 1 < args.len() {
                    size = args[i + 1].parse().unwrap_or(512);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--out-dir" => {
                if i + 1 < args.len() {
                    out_dir = args[i + 1].clone();
                    i += 2;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }

    std::fs::create_dir_all(&out_dir).ok();
    let d = out_dir.as_str();
    save(size, Mode::Color, false, d, "synapse-xuanwu-color.png");
    save(size, Mode::Color, true, d, "synapse-xuanwu-color-on-dark.png");
    save(size, Mode::MonoDark, false, d, "synapse-xuanwu-mono-dark.png");
    save(size, Mode::MonoLight, false, d, "synapse-xuanwu-mono-light.png");
}
