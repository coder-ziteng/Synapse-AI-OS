//! 渲染管线 —— Renderer trait + tiny-skia 实现
//!
//! [设计文档 07 §2.3] 规定的硬件加速迁移路径：
//! - 当前（PoC）：CPU 软件光栅化（tiny-skia）
//! - 未来（Phase 6.x）：GPU backend（virtio-gpu 硬件加速），`Renderer` trait 不变
//!
//! ## PoC 渲染范围
//!
//! 1. 背景（程序化渐变 + 焦点指示）
//! 2. 虚拟人（椭圆身体 + 圆形头 + 椭圆眼 + 椭圆嘴 + 名称占位条）
//! 3. 面板（圆角矩形背景 + 边框 + 标题色块 + 内容线条）
//!
//! 文字渲染 PoC 阶段省略（等 S6.0 接入 fontdue / ab_glyph）。

use crate::avatar::Avatar;
use crate::camera::SpatialCamera;
use crate::scene::{Panel, SceneNode};
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

/// tiny-skia 软件光栅化实现（PoC）
pub struct TinySkiaRenderer {
    pixmap: tiny_skia::Pixmap,
}

impl TinySkiaRenderer {
    /// 构造
    pub fn new(width: u32, height: u32) -> Option<Self> {
        tiny_skia::Pixmap::new(width, height).map(|pixmap| Self { pixmap })
    }

    /// 构造圆角矩形 Path（cubic bezier 近似圆弧，PoC 阶段不用抗锯齿抖动）
    fn rounded_rect(rect: tiny_skia::Rect, r: f32) -> tiny_skia::Path {
        let mut pb = tiny_skia::PathBuilder::new();
        let x = rect.x();
        let y = rect.y();
        let w = rect.width();
        let h = rect.height();
        let r = r.min(w * 0.5).min(h * 0.5);
        let k = r * 0.552_284_8;

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

    fn draw_background(&mut self) {
        let w = self.pixmap.width() as f32;
        let h = self.pixmap.height() as f32;
        for y in (0..self.pixmap.height() as i32).step_by(4) {
            for x in (0..self.pixmap.width() as i32).step_by(4) {
                let fx = x as f32 / w;
                let fy = y as f32 / h;
                let r = 0x12 + ((0x1C - 0x12) as f32 * fx) as u8;
                let g = 0x12 + ((0x1D - 0x12) as f32 * fy) as u8;
                let b = 0x14 + ((0x30 - 0x14) as f32 * (fx + fy) / 2.0) as u8;
                let paint = tiny_skia::Paint {
                    shader: tiny_skia::Shader::SolidColor(
                        tiny_skia::Color::from_rgba8(r, g, b, 255),
                    ),
                    anti_alias: false,
                    ..Default::default()
                };
                if let Some(r) =
                    tiny_skia::Rect::from_xywh(x as f32, y as f32, 4.0, 4.0)
                {
                    let path = tiny_skia::PathBuilder::from_rect(r);
                    self.pixmap.fill_path(
                        &path,
                        &paint,
                        tiny_skia::FillRule::Winding,
                        tiny_skia::Transform::identity(),
                        None,
                    );
                }
            }
        }

        // 焦点指示圆（虚拟人位置周围的光圈）
        let center = tiny_skia::PathBuilder::from_circle(
            w * 0.32,
            h * 0.62,
            180.0,
        )
        .expect("focus circle path");
        let focus_paint = tiny_skia::Paint {
            shader: tiny_skia::Shader::SolidColor(
                tiny_skia::Color::from_rgba8(0xFA, 0xEA, 0x5F, 32),
            ),
            anti_alias: true,
            ..Default::default()
        };
        self.pixmap.fill_path(
            &center,
            &focus_paint,
            tiny_skia::FillRule::Winding,
            tiny_skia::Transform::identity(),
            None,
        );
    }

    fn draw_avatar(&mut self, avatar: &Avatar, camera: &SpatialCamera) {
        let proj = camera.project(avatar.position);
        if proj.opacity <= 0.0 {
            return;
        }

        let cx = proj.screen.x;
        let cy = proj.screen.y;
        let s = proj.scale;

        // 身体椭圆
        let body = tiny_skia::PathBuilder::from_oval(
            tiny_skia::Rect::from_xywh(
                cx - avatar.body_radius_x * s,
                cy - avatar.body_radius_y * s * 0.5,
                avatar.body_radius_x * 2.0 * s,
                avatar.body_radius_y * s,
            )
            .unwrap(),
        )
        .expect("body oval path");
        let body_color = tiny_skia::Color::from_rgba8(
            (avatar.base_color.r * 255.0) as u8,
            (avatar.base_color.g * 255.0) as u8,
            (avatar.base_color.b * 255.0) as u8,
            220,
        );
        let body_paint = tiny_skia::Paint {
            shader: tiny_skia::Shader::SolidColor(body_color),
            anti_alias: true,
            ..Default::default()
        };
        self.pixmap.fill_path(
            &body,
            &body_paint,
            tiny_skia::FillRule::Winding,
            tiny_skia::Transform::identity(),
            None,
        );

        // 头部圆
        let head = tiny_skia::PathBuilder::from_circle(
            cx,
            cy - avatar.body_radius_y * s * 0.5 - avatar.head_radius * s * 0.3,
            avatar.head_radius * s,
        )
        .expect("head circle path");
        let head_color = tiny_skia::Color::from_rgba8(
            (avatar.base_color.r * 255.0) as u8,
            (avatar.base_color.g * 255.0) as u8,
            (avatar.base_color.b * 255.0) as u8,
            255,
        );
        let head_paint = tiny_skia::Paint {
            shader: tiny_skia::Shader::SolidColor(head_color),
            anti_alias: true,
            ..Default::default()
        };
        self.pixmap.fill_path(
            &head,
            &head_paint,
            tiny_skia::FillRule::Winding,
            tiny_skia::Transform::identity(),
            None,
        );

        // 眼睛（受表情影响）
        let eye_y = cy
            - avatar.body_radius_y * s * 0.5
            - avatar.head_radius * s * 0.3
            - avatar.head_radius * s * 0.15;
        let eye_radius_x = avatar.head_radius * s * 0.18;
        let eye_radius_y =
            avatar.head_radius * s * 0.22 * avatar.eye_open_ratio();

        let left_eye = tiny_skia::PathBuilder::from_oval(
            tiny_skia::Rect::from_xywh(
                cx - avatar.head_radius * s * 0.35 - eye_radius_x,
                eye_y - eye_radius_y,
                eye_radius_x * 2.0,
                eye_radius_y * 2.0,
            )
            .unwrap(),
        )
        .expect("left eye path");
        let eye_paint = tiny_skia::Paint {
            shader: tiny_skia::Shader::SolidColor(tiny_skia::Color::from_rgba8(
                18, 18, 20, 255,
            )),
            anti_alias: true,
            ..Default::default()
        };
        self.pixmap.fill_path(
            &left_eye,
            &eye_paint,
            tiny_skia::FillRule::Winding,
            tiny_skia::Transform::identity(),
            None,
        );

        let right_eye = tiny_skia::PathBuilder::from_oval(
            tiny_skia::Rect::from_xywh(
                cx + avatar.head_radius * s * 0.35 - eye_radius_x,
                eye_y - eye_radius_y,
                eye_radius_x * 2.0,
                eye_radius_y * 2.0,
            )
            .unwrap(),
        )
        .expect("right eye path");
        self.pixmap.fill_path(
            &right_eye,
            &eye_paint,
            tiny_skia::FillRule::Winding,
            tiny_skia::Transform::identity(),
            None,
        );

        // 嘴巴
        let mouth_y = cy
            - avatar.body_radius_y * s * 0.5
            - avatar.head_radius * s * 0.3
            + avatar.head_radius * s * 0.25;
        let mouth_w = avatar.head_radius * s * 0.25;
        let mouth_h =
            avatar.head_radius * s * 0.12 * (1.0 + avatar.mouth_open_ratio());
        let mouth = tiny_skia::PathBuilder::from_oval(
            tiny_skia::Rect::from_xywh(
                cx - mouth_w,
                mouth_y - mouth_h,
                mouth_w * 2.0,
                mouth_h * 2.0,
            )
            .unwrap(),
        )
        .expect("mouth path");
        let mouth_paint = tiny_skia::Paint {
            shader: tiny_skia::Shader::SolidColor(tiny_skia::Color::from_rgba8(
                18, 18, 20, 255,
            )),
            anti_alias: true,
            ..Default::default()
        };
        self.pixmap.fill_path(
            &mouth,
            &mouth_paint,
            tiny_skia::FillRule::Winding,
            tiny_skia::Transform::identity(),
            None,
        );

        // 名称占位条
        let label_paint = tiny_skia::Paint {
            shader: tiny_skia::Shader::SolidColor(tiny_skia::Color::from_rgba8(
                0xFF, 0xFF, 0xFF, 230,
            )),
            anti_alias: true,
            ..Default::default()
        };
        let label_y = cy + avatar.body_radius_y * s * 0.5 + 30.0 * s;
        let label_w = avatar.name.chars().count() as f32 * 12.0;
        let label = tiny_skia::PathBuilder::from_rect(
            tiny_skia::Rect::from_xywh(
                cx - label_w / 2.0,
                label_y,
                label_w.max(20.0),
                4.0,
            )
            .unwrap(),
        );
        self.pixmap.fill_path(
            &label,
            &label_paint,
            tiny_skia::FillRule::Winding,
            tiny_skia::Transform::identity(),
            None,
        );
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

        let r = 12.0;
        let rect = tiny_skia::Rect::from_xywh(base_x, base_y, w, h).unwrap();
        let path = Self::rounded_rect(rect, r);

        // 阴影
        for i in 1..=3 {
            let shadow_offset = (i as f32) * 2.0;
            let shadow_alpha = 60u8 / i as u8;
            let shadow_rect = tiny_skia::Rect::from_xywh(
                base_x,
                base_y + shadow_offset,
                w,
                h,
            )
            .unwrap();
            let shadow_path = Self::rounded_rect(shadow_rect, r);
            let shadow_paint = tiny_skia::Paint {
                shader: tiny_skia::Shader::SolidColor(
                    tiny_skia::Color::from_rgba8(0, 0, 0, shadow_alpha),
                ),
                anti_alias: true,
                ..Default::default()
            };
            self.pixmap.fill_path(
                &shadow_path,
                &shadow_paint,
                tiny_skia::FillRule::Winding,
                tiny_skia::Transform::identity(),
                None,
            );
        }

        // 背景
        let bg_paint = tiny_skia::Paint {
            shader: tiny_skia::Shader::SolidColor(tiny_skia::Color::from_rgba8(
                (panel.bg.r * 255.0) as u8,
                (panel.bg.g * 255.0) as u8,
                (panel.bg.b * 255.0) as u8,
                (panel.bg.a * 255.0) as u8,
            )),
            anti_alias: true,
            ..Default::default()
        };
        self.pixmap.fill_path(
            &path,
            &bg_paint,
            tiny_skia::FillRule::Winding,
            tiny_skia::Transform::identity(),
            None,
        );

        // 边框
        if let Some(border_color) = panel.border {
            let stroke = tiny_skia::Stroke {
                width: 2.0,
                ..Default::default()
            };
            let border_paint = tiny_skia::Paint {
                shader: tiny_skia::Shader::SolidColor(tiny_skia::Color::from_rgba8(
                    (border_color.r * 255.0) as u8,
                    (border_color.g * 255.0) as u8,
                    (border_color.b * 255.0) as u8,
                    (border_color.a * 255.0) as u8,
                )),
                anti_alias: true,
                ..Default::default()
            };
            self.pixmap.stroke_path(
                &path,
                &border_paint,
                &stroke,
                tiny_skia::Transform::identity(),
                None,
            );
        }

        // 标题色块
        let accent = Color::SYNAPSE_ACCENT;
        let accent_rect = tiny_skia::Rect::from_xywh(
            base_x + 16.0,
            base_y + 16.0,
            6.0,
            24.0,
        )
        .unwrap();
        let accent_path = tiny_skia::PathBuilder::from_rect(accent_rect);
        let accent_paint = tiny_skia::Paint {
            shader: tiny_skia::Shader::SolidColor(tiny_skia::Color::from_rgba8(
                (accent.r * 255.0) as u8,
                (accent.g * 255.0) as u8,
                (accent.b * 255.0) as u8,
                255,
            )),
            anti_alias: true,
            ..Default::default()
        };
        self.pixmap.fill_path(
            &accent_path,
            &accent_paint,
            tiny_skia::FillRule::Winding,
            tiny_skia::Transform::identity(),
            None,
        );

        // 标题占位条
        let title_w = (panel.title.chars().count() as f32) * 10.0;
        let title_paint = tiny_skia::Paint {
            shader: tiny_skia::Shader::SolidColor(tiny_skia::Color::from_rgba8(
                255, 255, 255, 230,
            )),
            anti_alias: true,
            ..Default::default()
        };
        let title_rect = tiny_skia::Rect::from_xywh(
            base_x + 30.0,
            base_y + 20.0,
            title_w.max(20.0),
            6.0,
        )
        .unwrap();
        let title_path = tiny_skia::PathBuilder::from_rect(title_rect);
        self.pixmap.fill_path(
            &title_path,
            &title_paint,
            tiny_skia::FillRule::Winding,
            tiny_skia::Transform::identity(),
            None,
        );

        // 正文行
        let body_paint = tiny_skia::Paint {
            shader: tiny_skia::Shader::SolidColor(tiny_skia::Color::from_rgba8(
                255, 255, 255, 180,
            )),
            anti_alias: true,
            ..Default::default()
        };
        let line_count = panel.body.chars().count().min(3);
        for i in 0..line_count {
            let line_w = (panel.width * (0.7 - 0.1 * i as f32) * s)
                .min(w - 32.0);
            let line_rect = tiny_skia::Rect::from_xywh(
                base_x + 16.0,
                base_y + 56.0 + i as f32 * 14.0,
                line_w.max(40.0),
                4.0,
            )
            .unwrap();
            let line_path = tiny_skia::PathBuilder::from_rect(line_rect);
            self.pixmap.fill_path(
                &line_path,
                &body_paint,
                tiny_skia::FillRule::Winding,
                tiny_skia::Transform::identity(),
                None,
            );
        }
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