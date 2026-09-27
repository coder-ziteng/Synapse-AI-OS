//! 基础几何与图形类型
//!
//! 与 [设计文档 07 §4.2 投影公式] 配合使用。
//! Vec2 / Vec3 是右手坐标系（x 右、y 上、z 朝观察者），与 OpenGL 一致。

use core::ops::{Add, Mul, Sub};

/// 2D 向量（屏幕坐标、UV、layout）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vec2 {
    /// x 分量
    pub x: f32,
    /// y 分量
    pub y: f32,
}

impl Vec2 {
    /// 零向量
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };

    /// 构造
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    /// 线性插值（动画引擎消费）
    pub fn lerp(a: Self, b: Self, t: f32) -> Self {
        Self {
            x: a.x + (b.x - a.x) * t,
            y: a.y + (b.y - a.y) * t,
        }
    }
}

impl Add for Vec2 {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self::new(self.x + rhs.x, self.y + rhs.y)
    }
}

impl Sub for Vec2 {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self::new(self.x - rhs.x, self.y - rhs.y)
    }
}

impl Mul<f32> for Vec2 {
    type Output = Self;
    fn mul(self, rhs: f32) -> Self {
        Self::new(self.x * rhs, self.y * rhs)
    }
}

/// 3D 向量（世界坐标、SpatialCamera 视锥内）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vec3 {
    /// x 分量
    pub x: f32,
    /// y 分量
    pub y: f32,
    /// z 分量
    pub z: f32,
}

impl Vec3 {
    /// 构造
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
}

/// RGBA 颜色（线性空间，0.0 ~ 1.0）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    /// 红
    pub r: f32,
    /// 绿
    pub g: f32,
    /// 蓝
    pub b: f32,
    /// alpha
    pub a: f32,
}

impl Color {
    /// 完全不透明黑色
    pub const BLACK: Self = Self::rgba(0.0, 0.0, 0.0, 1.0);
    /// 完全不透明白色
    pub const WHITE: Self = Self::rgba(1.0, 1.0, 1.0, 1.0);
    /// 透明（不绘制）
    pub const TRANSPARENT: Self = Self::rgba(0.0, 0.0, 0.0, 0.0);

    /// 0..=1 浮点构造
    pub const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    /// 0..=255 整数构造（设计 token 友好）
    pub const fn rgb8(r: u8, g: u8, b: u8) -> Self {
        Self::rgba(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0)
    }

    /// Synapse 主背景色（bg-1 = #121214）
    pub const SYNAPSE_BG_1: Self = Self::rgb8(0x12, 0x12, 0x14);
    /// Synapse 主背景色（bg-2 = #1C1D20，面板底色）
    pub const SYNAPSE_BG_2: Self = Self::rgb8(0x1C, 0x1D, 0x20);
    /// Synapse 强调色（color-yellow = #FAEA5F）
    pub const SYNAPSE_ACCENT: Self = Self::rgb8(0xFA, 0xEA, 0x5F);
    /// 文字主色（text-primary = rgba(255,255,255,0.90)）
    pub const SYNAPSE_TEXT_PRIMARY: Self =
        Self::rgba(1.0, 1.0, 1.0, 0.90);
    /// 文字次色
    pub const SYNAPSE_TEXT_SECONDARY: Self =
        Self::rgba(1.0, 1.0, 1.0, 0.75);

    /// 玄武玄黑底色（开机视觉 #05070e）
    pub const XUANWU_BG: Self = Self::rgb8(0x05, 0x07, 0x0E);
    /// 玄武主光晕青（#4dd0e1）
    pub const XUANWU_CYAN: Self = Self::rgb8(0x4D, 0xD0, 0xE1);
    /// 玄武副光晕紫（#7c4dff）
    pub const XUANWU_VIOLET: Self = Self::rgb8(0x7C, 0x4D, 0xFF);
    /// 玄武 mint（#34d399，online 态）
    pub const XUANWU_MINT: Self = Self::rgb8(0x34, 0xD3, 0x99);
    /// 玄武主文字（#e6f4f7）
    pub const XUANWU_TEXT: Self = Self::rgb8(0xE6, 0xF4, 0xF7);
    /// Liquid Glass 填充（white 4.5%）
    pub const GLASS_FILL: Self = Self::rgba(1.0, 1.0, 1.0, 0.045);
    /// Liquid Glass 玻璃边（white 8.5%）
    pub const GLASS_EDGE: Self = Self::rgba(1.0, 1.0, 1.0, 0.085);

    /// 替换 alpha 分量（玻璃层次叠色用）
    pub const fn with_alpha(self, a: f32) -> Self {
        Self {
            r: self.r,
            g: self.g,
            b: self.b,
            a,
        }
    }
}

/// 2D 仿射变换矩阵（a, b, c, d, e, f）：
/// | a c e |
/// | b d f |
/// | 0 0 1 |
/// 锚定面板的 billboard 旋转/缩放用
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform2D {
    /// a
    pub a: f32,
    /// b
    pub b: f32,
    /// c
    pub c: f32,
    /// d
    pub d: f32,
    /// e（x 平移）
    pub e: f32,
    /// f（y 平移）
    pub f: f32,
}

impl Transform2D {
    /// 单位变换
    pub const IDENTITY: Self = Self {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    /// 平移
    pub fn translate(x: f32, y: f32) -> Self {
        Self {
            e: x,
            f: y,
            ..Self::IDENTITY
        }
    }

    /// 缩放（均匀）
    pub fn scale(s: f32) -> Self {
        Self {
            a: s,
            d: s,
            ..Self::IDENTITY
        }
    }

    /// 应用变换到 2D 点
    pub fn apply(&self, p: Vec2) -> Vec2 {
        Vec2::new(
            self.a * p.x + self.c * p.y + self.e,
            self.b * p.x + self.d * p.y + self.f,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vec2_lerp_midpoint() {
        let a = Vec2::new(0.0, 0.0);
        let b = Vec2::new(10.0, 20.0);
        let m = Vec2::lerp(a, b, 0.5);
        assert_eq!(m, Vec2::new(5.0, 10.0));
    }

    #[test]
    fn vec2_ops() {
        let a = Vec2::new(1.0, 2.0);
        let b = Vec2::new(3.0, 5.0);
        assert_eq!(a + b, Vec2::new(4.0, 7.0));
        assert_eq!(b - a, Vec2::new(2.0, 3.0));
        assert_eq!(a * 2.0, Vec2::new(2.0, 4.0));
    }

    #[test]
    fn transform_translate_then_apply() {
        let t = Transform2D::translate(10.0, 20.0);
        assert_eq!(t.apply(Vec2::new(1.0, 2.0)), Vec2::new(11.0, 22.0));
    }

    #[test]
    fn transform_scale_then_apply() {
        let t = Transform2D::scale(2.0);
        assert_eq!(t.apply(Vec2::new(3.0, 4.0)), Vec2::new(6.0, 8.0));
    }

    #[test]
    fn color_constants_match_design_tokens() {
        // 与 docs/design/07 中"速度/稳定性/流畅度"小节色调一致
        assert_eq!(Color::SYNAPSE_BG_1.r, 0x12 as f32 / 255.0);
        assert_eq!(Color::SYNAPSE_ACCENT.r, 0xFA as f32 / 255.0);
    }
}