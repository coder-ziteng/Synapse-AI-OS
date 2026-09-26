//! 2D 虚拟人（Live2D 风格简化版）
//!
//! 与 [设计文档 07 §4.3] 对齐：起步阶段是 2D 矢量动画角色，由 SVG-like Path
//! 描述关键部件（眼/口/头/手），状态机驱动 6 种基础表情。
//!
//! PoC 简化：不做骨骼变形，关键部件用椭圆 + 矩形描述，状态机只影响部件位置/大小。
//! 真实落地时接入 Live2D SDK 或 VRoid（详见设计文档 §4.3 远期）。

use crate::types::{Color, Vec3};

/// 表情枚举（6 种基础 + 1 个空闲态）
///
/// 与设计文档 §4.3 一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Expression {
    /// 中性（默认）
    #[default]
    Neutral,
    /// 高兴
    Happy,
    /// 思考
    Thinking,
    /// 困惑
    Confused,
    /// 警觉
    Alert,
    /// 休眠
    Asleep,
}

/// 2D 虚拟人
#[derive(Debug, Clone)]
pub struct Avatar {
    /// 名称（用于 UI 显示）
    pub name: String,
    /// 世界坐标位置（虚拟人锚定在屏幕中心略下）
    pub position: Vec3,
    /// 角色底色
    pub base_color: Color,
    /// 当前表情
    pub expression: Expression,
    /// 身体椭圆宽度（屏幕像素，1.0 缩放下）
    pub body_radius_x: f32,
    /// 身体椭圆高度
    pub body_radius_y: f32,
    /// 头部半径
    pub head_radius: f32,
}

impl Default for Avatar {
    fn default() -> Self {
        Self {
            name: "Synapse".into(),
            // 屏幕中心略下方，z=0（最近景）
            position: Vec3::new(-120.0, -80.0, 0.0),
            base_color: Color::SYNAPSE_ACCENT,
            expression: Expression::Neutral,
            body_radius_x: 90.0,
            body_radius_y: 130.0,
            head_radius: 55.0,
        }
    }
}

impl Avatar {
    /// 设置表情
    pub fn set_expression(&mut self, expr: Expression) {
        self.expression = expr;
    }

    /// 当前表情下眼睛的"睁度"（影响 y 轴半径）
    /// Neutral/Happy: 1.0（睁眼）
    /// Thinking/Confused: 0.6（半眯）
    /// Alert: 1.2（瞪眼）
    /// Asleep: 0.05（闭眼）
    pub fn eye_open_ratio(&self) -> f32 {
        match self.expression {
            Expression::Neutral | Expression::Happy => 1.0,
            Expression::Thinking | Expression::Confused => 0.6,
            Expression::Alert => 1.2,
            Expression::Asleep => 0.05,
        }
    }

    /// 当前表情下嘴巴的"开度"
    /// Neutral: 0.0（闭嘴）
    /// Happy: 0.4（微笑）
    /// Thinking/Confused: 0.1（微张）
    /// Alert: 0.0
    /// Asleep: 0.0
    pub fn mouth_open_ratio(&self) -> f32 {
        match self.expression {
            Expression::Neutral | Expression::Alert | Expression::Asleep => 0.0,
            Expression::Happy => 0.4,
            Expression::Thinking | Expression::Confused => 0.1,
        }
    }

    /// 当前表情下眉毛倾斜角（弧度，正值 = 右眉上挑）
    pub fn brow_tilt(&self) -> f32 {
        match self.expression {
            Expression::Neutral => 0.0,
            Expression::Happy => 0.15,
            Expression::Thinking => -0.05,
            Expression::Confused => -0.2,
            Expression::Alert => 0.3,
            Expression::Asleep => 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_expression_is_neutral() {
        let a = Avatar::default();
        assert_eq!(a.expression, Expression::Neutral);
        assert_eq!(a.eye_open_ratio(), 1.0);
        assert_eq!(a.mouth_open_ratio(), 0.0);
    }

    #[test]
    fn asleep_eyes_closed_mouth_closed() {
        let mut a = Avatar::default();
        a.set_expression(Expression::Asleep);
        assert!(a.eye_open_ratio() < 0.1);
        assert_eq!(a.mouth_open_ratio(), 0.0);
    }

    #[test]
    fn happy_opens_mouth() {
        let mut a = Avatar::default();
        a.set_expression(Expression::Happy);
        assert!(a.mouth_open_ratio() > 0.0);
    }

    #[test]
    fn alert_widens_eyes_and_raises_brow() {
        let mut a = Avatar::default();
        a.set_expression(Expression::Alert);
        assert!(a.eye_open_ratio() > 1.0);
        assert!(a.brow_tilt() > 0.0);
    }

    #[test]
    fn brow_tilt_sign_distinguishes_confused_from_happy() {
        let mut a = Avatar::default();
        a.set_expression(Expression::Confused);
        let confused = a.brow_tilt();
        a.set_expression(Expression::Happy);
        let happy = a.brow_tilt();
        assert!(confused < 0.0);
        assert!(happy > 0.0);
    }
}