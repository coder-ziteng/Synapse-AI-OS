//! 场景图（SceneGraph）
//!
//! 与 [设计文档 07 §1.2 / §4.1] 对齐：分层结构 = 空间外壳 + 平面内容 + 模式切换器。
//! PoC 阶段最小集：Panel / Avatar / SpatialAnchor 三种节点。
//!
//! ## 节点语义
//!
//! - [`SceneNode::Panel`]：2D 卡片面板，定义在屏幕空间的局部坐标系
//! - [`SceneNode::Avatar`]：2D 虚拟人（详见 `avatar` 模块）
//! - [`SceneNode::Anchor`]：空间锚点，挂载面板用，决定面板的 3D 位置

use crate::avatar::Avatar;
use crate::types::{Color, Vec2, Vec3};

/// 场景图节点 ID（PoC 用 usize，真实场景用 arena 索引）
pub type NodeId = usize;

/// 空间锚点
///
/// 面板挂载到锚点后，面板的 3D 位置由锚点决定，投影由 SpatialCamera 完成。
/// 详见 [设计文档 07 §4.4]。
#[derive(Debug, Clone, Copy)]
pub struct SpatialAnchor {
    /// 锚点 ID（用于绑定面板引用）
    pub id: u32,
    /// 锚点位置（世界坐标）
    pub position: Vec3,
    /// 锚点类型
    pub kind: AnchorKind,
}

/// 锚点类型
///
/// 与设计文档 §4.4 表格一致：AvatarRight / AvatarLeft / ScreenTopRight / FocusPoint / Floating
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorKind {
    /// 虚拟人右侧（主任务面板）
    AvatarRight,
    /// 虚拟人左侧（临时通知卡片）
    AvatarLeft,
    /// 屏幕右上角（系统级通知）
    ScreenTopRight,
    /// 焦点位置（用户凝视点驱动）
    FocusPoint,
    /// 自由漂浮（用户拖拽）
    Floating,
}

/// 平面卡片面板
///
/// 局部坐标系：宽高定义在 (0,0) → (width, height) 矩形。
/// 真实屏幕位置 = `SpatialCamera.project(anchor.position)` + 面板 anchor_offset。
#[derive(Debug, Clone)]
pub struct Panel {
    /// 节点 ID
    pub id: NodeId,
    /// 标题（PoC 简化：单行文本）
    pub title: String,
    /// 内容文本
    pub body: String,
    /// 面板像素宽度
    pub width: f32,
    /// 面板像素高度
    pub height: f32,
    /// 相对锚点的偏移（屏幕像素）
    pub anchor_offset: Vec2,
    /// 背景色
    pub bg: Color,
    /// 边框色（None = 不画边框）
    pub border: Option<Color>,
    /// 挂载的锚点（None = 自由摆放在 world_pos）
    pub anchor: Option<SpatialAnchor>,
    /// 自由位置（未挂锚点时使用，世界坐标）
    pub world_pos: Vec3,
    /// Z 顺序（小者先画）
    pub z_order: i16,
}

impl Panel {
    /// 构造一个标准任务面板
    pub fn task_card(id: NodeId, title: &str, body: &str) -> Self {
        Self {
            id,
            title: title.into(),
            body: body.into(),
            width: 320.0,
            height: 140.0,
            anchor_offset: Vec2::new(40.0, 0.0),
            bg: Color::SYNAPSE_BG_2,
            border: Some(Color::SYNAPSE_ACCENT),
            anchor: None,
            world_pos: Vec3::new(280.0, 0.0, 600.0),
            z_order: 10,
        }
    }

    /// 构造通知卡片（更小、左上）
    pub fn notice_card(id: NodeId, body: &str) -> Self {
        Self {
            id,
            title: "通知".into(),
            body: body.into(),
            width: 240.0,
            height: 80.0,
            anchor_offset: Vec2::new(-30.0, 60.0),
            bg: Color::SYNAPSE_BG_2,
            border: None,
            anchor: None,
            world_pos: Vec3::new(-300.0, 80.0, 200.0),
            z_order: 20,
        }
    }
}

/// 场景节点（变体）
#[derive(Debug, Clone)]
pub enum SceneNode {
    /// 平面卡片面板
    Panel(Panel),
    /// 2D 虚拟人
    Avatar(Avatar),
}

/// 场景图（按 z_order 排序的节点列表）
#[derive(Debug, Default)]
pub struct SceneGraph {
    /// 节点列表（按 z_order 升序：先画远处的）
    nodes: Vec<SceneNode>,
}

impl SceneGraph {
    /// 构造空场景
    pub fn new() -> Self {
        Self::default()
    }

    /// 添加节点（内部按 z_order 排序）
    pub fn add(&mut self, node: SceneNode) {
        self.nodes.push(node);
        // 稳定排序：相同 z_order 保持插入顺序（先入先画）
        self.nodes
            .sort_by_key(|n| z_order_of(n));
    }

    /// 节点数
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// 是否空
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// 按绘制顺序迭代（z_order 升序 = 远处先画）
    pub fn iter_draw_order(&self) -> impl Iterator<Item = &SceneNode> {
        self.nodes.iter()
    }
}

fn z_order_of(node: &SceneNode) -> i16 {
    match node {
        SceneNode::Panel(p) => p.z_order,
        // 虚拟人固定 z_order = 0（最远背景之前）
        SceneNode::Avatar(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_sorts_by_z_order() {
        let mut sg = SceneGraph::new();
        sg.add(SceneNode::Panel(Panel::task_card(0, "A", "a")));
        sg.add(SceneNode::Panel(Panel::notice_card(1, "n")));
        sg.add(SceneNode::Avatar(Avatar::default()));

        let orders: Vec<i16> = sg.iter_draw_order().map(z_order_of).collect();
        assert_eq!(orders, vec![0, 10, 20]);
    }

    #[test]
    fn scene_panel_factory_returns_expected_dims() {
        let p = Panel::task_card(0, "会议", "5 项任务");
        assert_eq!(p.title, "会议");
        assert_eq!(p.body, "5 项任务");
        assert_eq!(p.width, 320.0);
    }
}