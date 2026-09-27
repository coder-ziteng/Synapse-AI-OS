//! SpatialCamera —— 3D 世界 → 2D 屏幕投影
//!
//! [设计文档 07 §4.2] 规定的「极简空间外壳」投影公式。
//!
//! 设计原则（与设计文档严格对齐）：
//! 1. 3D 外壳极简——只提供空间框架感，不堆 3D 控件
//! 2. 所有内容是 2D 卡片面板，通过 z 决定遮挡与缩放（远小近大）
//! 3. 始终面向摄像机（billboard 行为）

use crate::types::{Vec2, Vec3};

/// 视口尺寸（屏幕像素）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    /// 宽（像素）
    pub width: u32,
    /// 高（像素）
    pub height: u32,
}

impl Viewport {
    /// 16:9 1080p（PoC 默认）
    pub const FHD: Self = Self {
        width: 1920,
        height: 1080,
    };

    /// 中心点（屏幕坐标系：原点左上、y 向下）
    pub fn center(&self) -> Vec2 {
        Vec2::new(self.width as f32 / 2.0, self.height as f32 / 2.0)
    }
}

/// 空间摄像机
///
/// 视点位于 (0, 0, -focal)，沿 +z 轴看向原点。3D 世界中的卡片 z 越大（离视点越远），
/// 屏幕投影越小；超出 `far_plane` 的卡片 opacity=0 不绘制。
///
/// 这是 PoC 简化版：真实引擎会有 roll/pitch/yaw 与投影矩阵，这里只保留 z 轴缩放，
/// 满足「虚拟人 + 锚点 + 卡片」三种核心元素的需要。
#[derive(Debug, Clone, Copy)]
pub struct SpatialCamera {
    /// 焦距（屏幕像素单位，越大 → 透视越弱/正交越强）
    pub focal: f32,
    /// 远裁面（卡片 z 大于此值完全不绘制）
    pub far_plane: f32,
    /// 视口
    pub viewport: Viewport,
}

impl Default for SpatialCamera {
    fn default() -> Self {
        Self {
            focal: 1200.0,
            far_plane: 4000.0,
            viewport: Viewport::FHD,
        }
    }
}

/// 3D → 2D 投影结果
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Projection {
    /// 屏幕坐标
    pub screen: Vec2,
    /// 缩放系数（1.0 = 原尺寸）
    pub scale: f32,
    /// 不透明度（0 = 远平面外不可见，1 = 完全可见）
    pub opacity: f32,
}

impl SpatialCamera {
    /// 构造一个默认摄像机
    pub fn new(viewport: Viewport) -> Self {
        Self {
            viewport,
            ..Self::default()
        }
    }

    /// 投影 3D 世界坐标到屏幕
    ///
    /// 公式（[设计文档 07 §4.2]）：
    /// ```text
    /// sx = vx * focal / (vz + focal)
    /// sy = vy * focal / (vz + focal)
    /// scale = focal / (vz + focal)
    /// opacity = if vz > far_plane { 0 } else { 1 }
    /// ```
    /// 屏幕坐标系：原点左上、x 右、y 下（与 SVG / framebuffer 一致）。
    pub fn project(&self, world: Vec3) -> Projection {
        let denom = world.z + self.focal;
        let k = self.focal / denom;

        // 视口中心为锚点，y 反转（世界 y 朝上、屏幕 y 朝下）
        let center = self.viewport.center();
        let screen = Vec2::new(
            center.x + world.x * k,
            center.y - world.y * k,
        );

        let opacity = if world.z > self.far_plane {
            0.0
        } else {
            1.0
        };

        Projection {
            screen,
            scale: k,
            opacity,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_origin_is_center() {
        let cam = SpatialCamera::new(Viewport::FHD);
        let p = cam.project(Vec3::new(0.0, 0.0, 0.0));
        assert_eq!(p.screen, Vec2::new(960.0, 540.0));
        assert!((p.scale - 1.0).abs() < 1e-5);
        assert_eq!(p.opacity, 1.0);
    }

    #[test]
    fn project_far_card_scales_down() {
        let cam = SpatialCamera::new(Viewport::FHD);
        let near = cam.project(Vec3::new(100.0, 0.0, 0.0));
        let far = cam.project(Vec3::new(100.0, 0.0, 2000.0));
        // 远点 scale 应小于近点（远小近大）
        assert!(far.scale < near.scale);
        // 远点 screen.x 更靠近中心
        assert!(far.screen.x.abs() < near.screen.x.abs());
    }

    #[test]
    fn project_beyond_far_plane_is_invisible() {
        let cam = SpatialCamera::new(Viewport::FHD);
        let p = cam.project(Vec3::new(0.0, 0.0, 5000.0));
        assert_eq!(p.opacity, 0.0);
    }

    #[test]
    fn viewport_center_is_correct() {
        assert_eq!(Viewport::FHD.center(), Vec2::new(960.0, 540.0));
    }

    #[test]
    fn y_axis_flips_world_to_screen() {
        // 世界 (0, 100, 0) 应当在屏幕 y 上更靠上（即 y 更小，原点左上）
        let cam = SpatialCamera::new(Viewport::FHD);
        let p = cam.project(Vec3::new(0.0, 100.0, 0.0));
        assert!(p.screen.y < 540.0, "world +y should project above center");
    }
}