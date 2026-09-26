//! 渲染管线 v2 —— Renderer trait + tiny-skia 实现（玄武视觉）
//!
//! [设计文档 07 §2.3] 规定的硬件加速迁移路径：
//! - 当前（PoC）：CPU 软件光栅化（tiny-skia）
//! - 未来（Phase 6.x）：GPU backend（virtio-gpu 硬件加速），`Renderer` trait 不变
//!
//! ## 视觉语言（对齐 synapse-aios 开机视觉）
//!
//! 1. 背景：玄黑 #05070e + 三团极光 radial blob + 64px 技术网格 + 暗角
//! 2. Bento Grid：7 卡网格（brand/status/clock/hero/log/vitals/progress）
//! 3. Liquid Glass：深色基底 + white 4.5% 填充 + 顶部 44% 高光渐变 + 1px 玻璃边 + 层叠深阴影
//! 4. 虚拟人：赛博朋克剪影（青/紫双色差描边 + 面罩 visor + 胸口六边形能量核 + 扫描线）
//!
//! 文字渲染 PoC 阶段用占位条（等 S6.0 接入 fontdue / ab_glyph）。

use tiny_skia::{
    Color as TsColor, FillRule, GradientStop, LinearGradient, Paint, Path,
    PathBuilder, Point, Rect, Shader, SpreadMode, Stroke, StrokeDash,
    Transform,
};

use crate::avatar::Avatar;
use crate::camera::SpatialCamera;
use crate::scene::{Panel, PanelKind, SceneNode, VitalHue};
use crate::types::Color;

/// 渲染器 trait（未来 GPU backend 实现同一 trait，SceneGraph 不变）
pub trait Renderer {
    /// 渲染一帧场景到 framebuffer
    fn render(
        &mut self,
        scene: &crate::scene::SceneGraph,
        camera: &SpatialCamera,
    );

    /// framebuffer 宽
    fn width(&self) -> u32;

    /// framebuffer 高
    fn height(&self) -> u32;

    /// 拷贝 framebuffer 数据（RGBA premultiplied，每像素 4 字节）
    fn pixels(&self) -> Vec<u8>;
}

fn ts(c: Color) -> TsColor {
    TsColor::from_rgba(c.r, c.g, c.b, c.a).unwrap_or(TsColor::BLACK)
}

fn lin_grad(
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    c0: Color,
    c1: Color,
) -> Shader<'static> {
    LinearGradient::new(
        Point::from_xy(x0, y0),
        Point::from_xy(x1, y1),
        vec![GradientStop::new(0.0, ts(c0)), GradientStop::new(1.0, ts(c1))],
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap_or_else(|| Shader::SolidColor(ts(c1)))
}

/// tiny-skia 软件光栅化实现（PoC）
pub struct TinySkiaRenderer {
    pixmap: tiny_skia::Pixmap,
}

impl TinySkiaRenderer {
    /// 构造
    pub fn new(width: u32, height: u32) -> Option<Self> {
        tiny_skia::Pixmap::new(width, height).map(|pixmap| Self { pixmap })
    }

    /// 构造圆角矩形 Path（cubic bezier 近似圆弧）
    fn rounded_rect(rect: Rect, r: f32) -> Path {
        let x = rect.x();
        let y = rect.y();
        let w = rect.width();
        let h = rect.height();
        let r = r.min(w * 0.5).min(h * 0.5);
        let k = r * 0.552_284_8;

        let mut pb = PathBuilder::new();
        pb.move_to(x + r, y);
        pb.line_to(x + w - r, y);
        pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
        pb.line_to(x + w, y + h - r);
        pb.cubic_to(
            x + w,
            y + h - r + k,
            x + w - r + k,
            y + h,
            x + w - r,
            y + h,
        );
        pb.line_to(x + r, y + h);
        pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
        pb.line_to(x, y + r);
        pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
        pb.close();
        pb.finish().expect("rounded_rect path")
    }

    /// 顶部圆角、底部直角的矩形（玻璃顶部高光区）
    fn top_rounded_rect(rect: Rect, r: f32, hh: f32) -> Path {
        let x = rect.x();
        let y = rect.y();
        let w = rect.width();
        let r = r.min(w * 0.5).min(hh);
        let k = r * 0.552_284_8;

        let mut pb = PathBuilder::new();
        pb.move_to(x + r, y);
        pb.line_to(x + w - r, y);
        pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
        pb.line_to(x + w, y + hh);
        pb.line_to(x, y + hh);
        pb.line_to(x, y + r);
        pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
        pb.close();
        pb.finish().expect("top_rounded_rect path")
    }

    /// 正六边形 Path（玄武龟壳甲片语言）
    fn hex_path(cx: f32, cy: f32, r: f32) -> Path {
        let mut pb = PathBuilder::new();
        for i in 0..6 {
            let a = core::f32::consts::FRAC_PI_3 * i as f32 - core::f32::consts::FRAC_PI_2;
            let px = cx + r * a.cos();
            let py = cy + r * a.sin();
            if i == 0 {
                pb.move_to(px, py);
            } else {
                pb.line_to(px, py);
            }
        }
        pb.close();
        pb.finish().expect("hex path")
    }

    fn line_path(x0: f32, y0: f32, x1: f32, y1: f32) -> Path {
        let mut pb = PathBuilder::new();
        pb.move_to(x0, y0);
        pb.line_to(x1, y1);
        pb.finish().expect("line path")
    }

    fn fill(&mut self, path: &Path, c: Color) {
        let paint = Paint {
            shader: Shader::SolidColor(ts(c)),
            anti_alias: true,
            ..Default::default()
        };
        self.pixmap.fill_path(
            path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }

    fn fill_shader(&mut self, path: &Path, shader: Shader<'_>) {
        let paint = Paint {
            shader,
            anti_alias: true,
            ..Default::default()
        };
        self.pixmap.fill_path(
            path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }

    fn stroke_t(
        &mut self,
        path: &Path,
        c: Color,
        width: f32,
        dash: Option<StrokeDash>,
        t: Transform,
    ) {
        let paint = Paint {
            shader: Shader::SolidColor(ts(c)),
            anti_alias: true,
            ..Default::default()
        };
        let stroke = Stroke {
            width,
            dash,
            ..Default::default()
        };
        self.pixmap.stroke_path(path, &paint, &stroke, t, None);
    }

    fn stroke(&mut self, path: &Path, c: Color, width: f32) {
        self.stroke_t(path, c, width, None, Transform::identity());
    }

    /// 占位条（PoC 文字替身）
    fn bar(&mut self, x: f32, y: f32, w: f32, h: f32, c: Color) {
        if let Some(r) = Rect::from_xywh(x, y, w.max(1.0), h) {
            let path = PathBuilder::from_rect(r);
            self.fill(&path, c);
        }
    }

    /// 发光节点（三层同心圆近似 glow）
    fn glow_dot(&mut self, cx: f32, cy: f32, r: f32, c: Color) {
        for (mul, alpha) in [(2.4, 0.16), (1.6, 0.32), (1.0, 1.0)] {
            if let Some(p) = PathBuilder::from_circle(cx, cy, r * mul) {
                self.fill(&p, c.with_alpha(c.a * alpha));
            }
        }
    }

    /// 背景：逐像素 玄黑底 + 三团极光 + 64px 技术网格 + 暗角
    fn draw_background(&mut self) {
        let w = self.pixmap.width() as usize;
        let h = self.pixmap.height() as usize;
        let aspect = w as f32 / h as f32;

        // (中心 x, 中心 y, 半径, rgb, 强度) —— 对齐 index.html 三团 blob
        let blobs: [(f32, f32, f32, [f32; 3], f32); 3] = [
            (0.18, 0.10, 0.85, [77.0, 208.0, 225.0], 0.15),
            (0.85, 0.90, 0.80, [124.0, 77.0, 255.0], 0.13),
            (0.55, 0.45, 0.60, [30.0, 64.0, 120.0], 0.16),
        ];

        let data = self.pixmap.data_mut();
        for y in 0..h {
            let fy = (y as f32 + 0.5) / h as f32;
            for x in 0..w {
                let fx = (x as f32 + 0.5) / w as f32;
                let mut r = 5.0f32;
                let mut g = 7.0f32;
                let mut b = 14.0f32;

                for (bx, by, br, bc, bi) in blobs {
                    let dx = (fx - bx) * aspect;
                    let dy = fy - by;
                    let d = (dx * dx + dy * dy).sqrt();
                    let t = 1.0 - d / br;
                    if t > 0.0 {
                        let f = t * t * bi;
                        r += bc[0] * f;
                        g += bc[1] * f;
                        b += bc[2] * f;
                    }
                }

                // 技术网格（64px，径向 mask 中心 (0.5, 0.42)）
                if x % 64 == 0 || y % 64 == 0 {
                    let dx = (fx - 0.5) * aspect;
                    let dy = fy - 0.42;
                    let d = (dx * dx + dy * dy).sqrt();
                    let m = ((0.72 - d) / 0.42).clamp(0.0, 1.0);
                    let f = 0.035 * m;
                    r += 77.0 * f;
                    g += 208.0 * f;
                    b += 225.0 * f;
                }

                // 暗角
                let dx = (fx - 0.5) * aspect;
                let dy = fy - 0.40;
                let dv = (dx * dx + dy * dy).sqrt();
                let vt = ((dv - 0.52) / 0.48).clamp(0.0, 1.0);
                let vf = vt * vt * 0.78;
                r += (2.0 - r) * vf;
                g += (4.0 - g) * vf;
                b += (9.0 - b) * vf;

                let i = (y * w + x) * 4;
                data[i] = r.clamp(0.0, 255.0) as u8;
                data[i + 1] = g.clamp(0.0, 255.0) as u8;
                data[i + 2] = b.clamp(0.0, 255.0) as u8;
                data[i + 3] = 255;
            }
        }
    }

    /// Liquid Glass 卡基底：层叠深阴影 + 深色基底 + 玻璃填充 + 顶部高光 + 玻璃边
    fn draw_glass_card(&mut self, rect: Rect) {
        let r = 18.0;
        for (off, a) in [(16.0, 0.10), (11.0, 0.08), (7.0, 0.06), (3.0, 0.04)] {
            let sr = Rect::from_xywh(rect.x(), rect.y() + off, rect.width(), rect.height()).unwrap();
            let path = Self::rounded_rect(sr, r);
            self.fill(&path, Color::rgba(0.0, 0.0, 0.0, a));
        }

        let path = Self::rounded_rect(rect, r);
        self.fill(&path, Color::rgba(10.0 / 255.0, 16.0 / 255.0, 28.0 / 255.0, 0.60));
        self.fill(&path, Color::GLASS_FILL);

        let hi = Self::top_rounded_rect(rect, r, rect.height() * 0.44);
        let shader = lin_grad(
            rect.x(),
            rect.y(),
            rect.x(),
            rect.y() + rect.height() * 0.44,
            Color::rgba(1.0, 1.0, 1.0, 0.05),
            Color::TRANSPARENT,
        );
        self.fill_shader(&hi, shader);

        self.stroke(&path, Color::GLASS_EDGE, 1.0);
        self.bar(
            rect.x() + r,
            rect.y() + 1.0,
            rect.width() - 2.0 * r,
            1.0,
            Color::rgba(1.0, 1.0, 1.0, 0.06),
        );
    }

    fn hue_color(hue: VitalHue) -> (Color, Color) {
        match hue {
            VitalHue::Cyan => (Color::XUANWU_CYAN.with_alpha(0.55), Color::XUANWU_CYAN),
            VitalHue::Violet => (Color::XUANWU_VIOLET.with_alpha(0.5), Color::rgb8(0x9A, 0x7B, 0xFF)),
            VitalHue::Mint => (Color::XUANWU_MINT.with_alpha(0.45), Color::XUANWU_MINT),
        }
    }

    /// Bento 卡内容模板（占位条语言 + 品牌色）
    fn draw_panel_content(&mut self, panel: &Panel, rect: Rect) {
        let (x, y, w, h) = (rect.x(), rect.y(), rect.width(), rect.height());
        let faint = Color::XUANWU_TEXT.with_alpha(0.32);
        let dim = Color::XUANWU_TEXT.with_alpha(0.55);
        let text = Color::XUANWU_TEXT.with_alpha(0.90);
        let cyan = Color::XUANWU_CYAN;

        match panel.kind {
            PanelKind::Generic => {
                let accent = Color::SYNAPSE_ACCENT;
                self.bar(x + 16.0, y + 16.0, 6.0, 24.0, accent);
                let title_w = (panel.title.chars().count() as f32) * 10.0;
                self.bar(x + 30.0, y + 20.0, title_w.max(20.0), 6.0, text);
                let body = Color::XUANWU_TEXT.with_alpha(0.70);
                let line_count = panel.body.chars().count().min(3);
                for i in 0..line_count {
                    let line_w = (panel.width * (0.7 - 0.1 * i as f32)).min(w - 32.0);
                    self.bar(x + 16.0, y + 56.0 + i as f32 * 14.0, line_w.max(40.0), 4.0, body);
                }
            }
            PanelKind::Brand => {
                let cy = y + h * 0.5;
                let hx = x + 16.0 + 15.0;
                let hex = Self::hex_path(hx, cy, 14.0);
                self.stroke(&hex, cyan.with_alpha(0.8), 1.5);
                self.glow_dot(hx, cy, 3.5, cyan);
                self.bar(x + 46.0, cy - 6.0, 108.0, 11.0, text);
                self.bar(x + 158.0, cy - 6.0, 52.0, 11.0, cyan.with_alpha(0.9));
                let chip = Rect::from_xywh(x + 226.0, cy - 11.0, 92.0, 22.0).unwrap();
                let chip_path = Self::rounded_rect(chip, 11.0);
                self.fill(&chip_path, cyan.with_alpha(0.06));
                self.stroke(&chip_path, cyan.with_alpha(0.28), 1.0);
                self.bar(x + 238.0, cy - 3.0, 62.0, 6.0, cyan.with_alpha(0.85));
                self.bar(x + w - 16.0 - 96.0, cy - 3.0, 96.0, 6.0, faint);
            }
            PanelKind::Status => {
                self.bar(x + 20.0, y + 12.0, 64.0, 5.0, faint);
                let pill = Rect::from_xywh(x + 20.0, y + 28.0, 156.0, 26.0).unwrap();
                let pill_path = Self::rounded_rect(pill, 13.0);
                self.fill(&pill_path, cyan.with_alpha(0.05));
                self.stroke(&pill_path, cyan.with_alpha(0.25), 1.0);
                self.glow_dot(x + 35.0, y + 41.0, 3.0, cyan);
                let tw = (panel.body.chars().count() as f32 * 7.0).min(110.0);
                self.bar(x + 47.0, y + 38.0, tw.max(40.0), 6.0, cyan.with_alpha(0.9));
            }
            PanelKind::Clock => {
                self.bar(x + 20.0, y + 12.0, 44.0, 5.0, faint);
                let vw = (panel.body.chars().count() as f32 * 10.0).min(120.0);
                self.bar(x + 20.0, y + 28.0, vw.max(50.0), 14.0, text);
                self.bar(x + 26.0 + vw.max(50.0), y + 34.0, 12.0, 8.0, faint);
            }
            PanelKind::Hero => {
                // 角落刻度（仪表感）
                let t = 10.0;
                let c = cyan.with_alpha(0.4);
                let v = Color::XUANWU_VIOLET.with_alpha(0.4);
                self.bar(x + 12.0, y + 12.0, t, 1.0, c);
                self.bar(x + 12.0, y + 12.0, 1.0, t, c);
                self.bar(x + w - 12.0 - t, y + 12.0, t, 1.0, c);
                self.bar(x + w - 13.0, y + 12.0, 1.0, t, c);
                self.bar(x + 12.0, y + h - 13.0, t, 1.0, v);
                self.bar(x + 12.0, y + h - 12.0 - t, 1.0, t, v);
                self.bar(x + w - 12.0 - t, y + h - 13.0, t, 1.0, v);
                self.bar(x + w - 13.0, y + h - 12.0 - t, 1.0, t, v);

                // 一次性镜面扫光（静态帧取中段）
                let mut pb = PathBuilder::new();
                pb.move_to(x + w * 0.40, y);
                pb.line_to(x + w * 0.53, y);
                pb.line_to(x + w * 0.33, y + h);
                pb.line_to(x + w * 0.20, y + h);
                pb.close();
                if let Some(sheen) = pb.finish() {
                    self.fill(&sheen, Color::rgba(1.0, 1.0, 1.0, 0.028));
                }

                // caption：两侧渐隐线 + 居中文字条
                let cy = y + h - 30.0;
                let tw = (panel.title.chars().count() as f32 * 8.0).max(40.0);
                let cx = x + w * 0.5;
                self.bar(cx - tw * 0.5, cy, tw, 6.0, dim);
                self.bar(cx - tw * 0.5 - 26.0, cy + 2.0, 22.0, 1.0, cyan.with_alpha(0.5));
                self.bar(cx - tw * 0.5 - 48.0, cy + 2.0, 20.0, 1.0, cyan.with_alpha(0.15));
                self.bar(cx + tw * 0.5 + 4.0, cy + 2.0, 22.0, 1.0, Color::XUANWU_VIOLET.with_alpha(0.5));
                self.bar(cx + tw * 0.5 + 28.0, cy + 2.0, 20.0, 1.0, Color::XUANWU_VIOLET.with_alpha(0.15));
            }
            PanelKind::Log => {
                self.bar(x + 20.0, y + 16.0, 56.0, 5.0, faint);
                self.bar(x + w - 20.0 - 96.0, y + 16.0, 96.0, 5.0, Color::XUANWU_TEXT.with_alpha(0.22));
                self.bar(x + 20.0, y + 30.0, w - 40.0, 1.0, Color::XUANWU_TEXT.with_alpha(0.09));

                let lines: Vec<&String> = panel.lines.iter().rev().take(10).rev().collect();
                let n = lines.len() as f32;
                let start_y = y + h - 24.0 - n * 22.0;
                for (i, line) in lines.iter().enumerate() {
                    let ly = start_y + i as f32 * 22.0;
                    self.bar(x + 20.0, ly, 40.0, 5.0, Color::XUANWU_TEXT.with_alpha(0.22));
                    let tag = if line.contains("[ OK ]") {
                        cyan.with_alpha(0.95)
                    } else if line.contains("[INFO]") {
                        Color::rgb8(0xA7, 0x8B, 0xFA).with_alpha(0.95)
                    } else {
                        text
                    };
                    self.bar(x + 70.0, ly, 34.0, 5.0, tag);
                    let tw = ((line.len() as f32) * 3.2).min(w - 140.0);
                    self.bar(x + 112.0, ly, tw.max(60.0), 5.0, Color::XUANWU_TEXT.with_alpha(0.72));
                }
            }
            PanelKind::Vitals => {
                let colw = (w - 40.0) / 3.0;
                for (i, v) in panel.vitals.iter().take(3).enumerate() {
                    let cx0 = x + 20.0 + i as f32 * colw;
                    if i > 0 {
                        self.bar(cx0 - 12.0, y + 20.0, 1.0, h - 40.0, Color::XUANWU_TEXT.with_alpha(0.09));
                    }
                    let nw = (v.name.chars().count() as f32 * 6.0).min(colw - 30.0);
                    self.bar(cx0, y + 22.0, nw.max(30.0), 5.0, faint);
                    let vw = (v.value.chars().count() as f32 * 8.0).min(colw - 30.0);
                    self.bar(cx0, y + 38.0, vw.max(24.0), 12.0, text);

                    let tw = colw - 24.0;
                    let track = Rect::from_xywh(cx0, y + 64.0, tw, 3.0).unwrap();
                    self.fill(&Self::rounded_rect(track, 1.5), Color::XUANWU_TEXT.with_alpha(0.08));
                    let fw = tw * v.ratio.clamp(0.0, 1.0);
                    if fw > 1.0 {
                        let fill = Rect::from_xywh(cx0, y + 64.0, fw, 3.0).unwrap();
                        let (c0, c1) = Self::hue_color(v.hue);
                        let shader = lin_grad(cx0, y + 64.0, cx0 + tw, y + 64.0, c0, c1);
                        self.fill_shader(&Self::rounded_rect(fill, 1.5), shader);
                    }
                }
            }
            PanelKind::Progress => {
                let sw = (panel.title.chars().count() as f32 * 7.0).min(160.0);
                self.bar(x + 20.0, y + 22.0, sw.max(48.0), 6.0, cyan.with_alpha(0.75));
                self.bar(x + w - 20.0 - 56.0, y + 16.0, 40.0, 12.0, text);
                self.bar(x + w - 20.0 - 12.0, y + 22.0, 10.0, 6.0, faint);

                let tw = w - 40.0;
                self.bar(x + 20.0, y + 50.0, tw, 2.0, cyan.with_alpha(0.12));
                let fw = tw * panel.progress;
                if fw > 1.0 {
                    let fill = Rect::from_xywh(x + 20.0, y + 50.0, fw, 2.0).unwrap();
                    let shader = lin_grad(
                        x + 20.0,
                        y + 50.0,
                        x + 20.0 + tw,
                        y + 50.0,
                        cyan,
                        Color::XUANWU_VIOLET,
                    );
                    self.fill_shader(&PathBuilder::from_rect(fill), shader);
                }
                self.glow_dot(x + 20.0 + fw, y + 51.0, 3.0, Color::rgb8(0xC9, 0xF4, 0xFB));

                for i in 0..5 {
                    let sx = x + 20.0 + i as f32 * (tw - 28.0) / 4.0;
                    self.bar(sx, y + 66.0, 28.0, 4.0, Color::XUANWU_TEXT.with_alpha(0.22));
                }
            }
        }
    }

    /// 赛博朋克虚拟人：剪影 + 双色差描边 + visor 面罩 + 六边形能量核 + 扫描线
    fn draw_avatar(&mut self, avatar: &Avatar, camera: &SpatialCamera) {
        let proj = camera.project(avatar.position);
        if proj.opacity <= 0.0 {
            return;
        }

        let cx = proj.screen.x;
        let cy = proj.screen.y;
        let s = proj.scale;
        let bx = avatar.body_radius_x * s;
        let by = avatar.body_radius_y * s;
        let hr = avatar.head_radius * s;
        let hy = cy - by * 0.5 - hr * 0.3;

        let shell = Color::rgb8(0x0A, 0x11, 0x20);
        let shell_head = Color::rgb8(0x0C, 0x15, 0x26);
        let cyan = Color::XUANWU_CYAN;
        let violet = Color::XUANWU_VIOLET;

        // 光环虚线环（缓转刻度感）
        if let Some(halo) = PathBuilder::from_circle(cx, hy, hr * 2.1) {
            let dash = StrokeDash::new(vec![2.0, 7.0], 0.0);
            self.stroke_t(&halo, cyan.with_alpha(0.18), 1.0, dash, Transform::identity());
        }

        // 身体剪影
        let body = PathBuilder::from_oval(
            Rect::from_xywh(cx - bx, cy - by * 0.5, bx * 2.0, by).unwrap(),
        )
        .expect("body oval path");
        self.fill(&body, shell.with_alpha(0.92));
        // 色差描边（青左偏 / 紫右偏 / 中性收边）
        self.stroke_t(&body, cyan.with_alpha(0.45), 1.2, None, Transform::from_translate(-1.2, 0.0));
        self.stroke_t(&body, violet.with_alpha(0.40), 1.2, None, Transform::from_translate(1.2, 0.0));
        self.stroke(&body, cyan.with_alpha(0.22), 1.0);

        // 扫描线（椭圆弦宽逐行）
        let ry = by * 0.5;
        let mut dy = -ry + 2.0;
        while dy < ry {
            let k = 1.0 - (dy / ry) * (dy / ry);
            if k > 0.0 {
                let half = bx * k.sqrt();
                self.bar(cx - half, cy + dy, half * 2.0, 1.0, cyan.with_alpha(0.05));
            }
            dy += 4.0;
        }

        // 头部剪影 + 色差描边
        let head = PathBuilder::from_circle(cx, hy, hr).expect("head circle path");
        self.fill(&head, shell_head.with_alpha(0.95));
        self.stroke_t(&head, cyan.with_alpha(0.45), 1.0, None, Transform::from_translate(-1.0, 0.0));
        self.stroke_t(&head, violet.with_alpha(0.40), 1.0, None, Transform::from_translate(1.0, 0.0));

        // visor 面罩（表情调制高度；Asleep 收成一线）
        let eye_y = hy - hr * 0.10;
        let vw = hr * 0.62;
        let vh = (hr * 0.30 * avatar.eye_open_ratio()).max(1.5);
        let visor_glow = Rect::from_xywh(cx - vw - 4.0, eye_y - vh * 0.5 - 3.0, vw * 2.0 + 8.0, vh + 6.0).unwrap();
        self.fill(&Self::rounded_rect(visor_glow, (vh + 6.0) * 0.5), cyan.with_alpha(0.14));
        let visor = Rect::from_xywh(cx - vw, eye_y - vh * 0.5, vw * 2.0, vh).unwrap();
        let shader = lin_grad(cx - vw, eye_y, cx + vw, eye_y, cyan, violet);
        self.fill_shader(&Self::rounded_rect(visor, vh * 0.5), shader);

        // 嘴部发光条
        let mw = hr * 0.22;
        let mh = (hr * 0.10 * (1.0 + avatar.mouth_open_ratio())).max(1.2);
        let mouth = Rect::from_xywh(cx - mw, hy + hr * 0.45, mw * 2.0, mh).unwrap();
        self.fill(&Self::rounded_rect(mouth, mh * 0.5), cyan.with_alpha(0.55));

        // 胸口六边形能量核（玄武甲片语言）
        let core_r = hr * 0.55;
        let hex = Self::hex_path(cx, cy, core_r);
        self.stroke(&hex, cyan.with_alpha(0.6), 1.2);
        let hex_in = Self::hex_path(cx, cy, core_r * 0.55);
        self.stroke(&hex_in, violet.with_alpha(0.35), 1.0);
        if let Some(ring) = PathBuilder::from_circle(cx, cy, core_r * 1.35) {
            let dash = StrokeDash::new(vec![3.0, 9.0], 0.0);
            self.stroke_t(&ring, cyan.with_alpha(0.30), 1.0, dash, Transform::identity());
        }
        self.glow_dot(cx, cy, core_r * 0.26, Color::rgb8(0xDF, 0xF6, 0xFA));

        // 肩部电路走线 + 节点
        let l0 = Self::line_path(cx - bx * 0.72, cy - by * 0.12, cx - bx * 1.02, cy - by * 0.34);
        self.stroke(&l0, cyan.with_alpha(0.5), 1.0);
        self.glow_dot(cx - bx * 1.02, cy - by * 0.34, 2.0, cyan);
        let l1 = Self::line_path(cx + bx * 0.72, cy - by * 0.12, cx + bx * 1.02, cy - by * 0.34);
        self.stroke(&l1, violet.with_alpha(0.5), 1.0);
        self.glow_dot(cx + bx * 1.02, cy - by * 0.34, 2.0, violet);

        // 名称条 + 青色下划线光
        let lw = (avatar.name.chars().count() as f32) * 10.0;
        let ly = cy + by * 0.5 + 24.0 * s;
        self.bar(cx - lw * 0.5, ly, lw.max(20.0), 5.0, Color::XUANWU_TEXT.with_alpha(0.8));
        self.bar(cx - lw * 0.5, ly + 8.0 * s, lw.max(20.0), 1.0, cyan.with_alpha(0.5));
    }

    fn draw_panel(&mut self, panel: &Panel, camera: &SpatialCamera) {
        let world_pos = panel
            .anchor
            .map(|a| a.position)
            .unwrap_or(panel.world_pos);
        let proj = camera.project(world_pos);
        if proj.opacity <= 0.0 {
            return;
        }

        let base_x = proj.screen.x + panel.anchor_offset.x;
        let base_y = proj.screen.y + panel.anchor_offset.y;
        let s = proj.scale;
        let w = panel.width * s;
        let h = panel.height * s;
        let rect = Rect::from_xywh(base_x, base_y, w, h).unwrap();

        if panel.kind == PanelKind::Generic {
            let path = Self::rounded_rect(rect, 12.0);
            let bg_paint = Paint {
                shader: Shader::SolidColor(ts(panel.bg)),
                anti_alias: true,
                ..Default::default()
            };
            self.pixmap.fill_path(
                &path,
                &bg_paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
            if let Some(border_color) = panel.border {
                self.stroke(&path, border_color, 2.0);
            }
        } else {
            self.draw_glass_card(rect);
        }

        self.draw_panel_content(panel, rect);
    }
}

impl Renderer for TinySkiaRenderer {
    fn render(
        &mut self,
        scene: &crate::scene::SceneGraph,
        camera: &SpatialCamera,
    ) {
        self.draw_background();
        for node in scene.iter_draw_order() {
            match node {
                SceneNode::Avatar(a) => self.draw_avatar(a, camera),
                SceneNode::Panel(p) => self.draw_panel(p, camera),
            }
        }
    }

    fn width(&self) -> u32 {
        self.pixmap.width()
    }

    fn height(&self) -> u32 {
        self.pixmap.height()
    }

    fn pixels(&self) -> Vec<u8> {
        self.pixmap.data().to_vec()
    }
}

/// 保存当前帧为 PNG 文件（PoC 阶段辅助函数）
pub fn save_png(renderer: &dyn Renderer, path: &str) -> Result<(), String> {
    let img = image::RgbaImage::from_raw(
        renderer.width(),
        renderer.height(),
        renderer.pixels(),
    )
    .ok_or_else(|| "from_raw failed".to_string())?;
    img.save(path).map_err(|e| e.to_string())
}
