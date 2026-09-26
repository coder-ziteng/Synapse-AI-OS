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

/// 卡片内容种类（Bento Grid 格语义，对齐玄武开机视觉分镜）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelKind {
    /// 通用卡片（标题色块 + 正文行）
    Generic,
    /// 品牌条（徽章 + 名称 + 版本 chip）
    Brand,
    /// 内核状态（label + 状态 pill）
    Status,
    /// Uptime 时钟
    Clock,
    /// 主舞台（虚拟人 + 角落刻度 + caption）
    Hero,
    /// 启动日志（等宽行）
    Log,
    /// 系统体征（三列指标条）
    Vitals,
    /// 启动进度（渐变进度条 + 阶段刻度）
    Progress,
}

/// 体征指标色相
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VitalHue {
    /// 青（神经负载）
    Cyan,
    /// 紫（内存）
    Violet,
    /// 薄荷（同步率）
    Mint,
}

/// 体征指标（Vitals 卡单列）
#[derive(Debug, Clone)]
pub struct Vital {
    /// 指标名
    pub name: String,
    /// 数值文本
    pub value: String,
    /// 条形填充比 0.0~1.0
    pub ratio: f32,
    /// 色相
    pub hue: VitalHue,
}

/// 平面卡片面板
///
/// 局部坐标系：宽高定义在 (0,0) → (width, height) 矩形。
/// 真实屏幕位置 = `SpatialCamera.project(anchor.position)` + 面板 anchor_offset。
#[derive(Debug, Clone)]
pub struct Panel {
    /// 内容种类（决定 renderer 绘制模板）
    pub kind: PanelKind,
    /// 日志行（Log 卡）/ 通用正文行
    pub lines: Vec<String>,
    /// 体征指标（Vitals 卡）
    pub vitals: Vec<Vital>,
    /// 进度 0.0~1.0（Progress 卡）
    pub progress: f32,
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
            kind: PanelKind::Generic,
            lines: Vec::new(),
            vitals: Vec::new(),
            progress: 0.0,
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
            kind: PanelKind::Generic,
            lines: Vec::new(),
            vitals: Vec::new(),
            progress: 0.0,
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

    /// Bento Grid 单元构造（FHD 1080×660 居中盒，gap 16）
    ///
    /// 网格：4 列 (1.1/1.1/1/1) × 3 行 (64/1fr/108)，区域同玄武开机视觉：
    /// brand brand status clock / hero hero log log / vitals vitals prog prog
    fn bento(
        id: NodeId,
        kind: PanelKind,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        z: i16,
    ) -> Self {
        Self {
            kind,
            lines: Vec::new(),
            vitals: Vec::new(),
            progress: 0.0,
            id,
            title: String::new(),
            body: String::new(),
            width: w,
            height: h,
            anchor_offset: Vec2::ZERO,
            bg: Color::GLASS_FILL,
            border: None,
            anchor: None,
            // 屏幕左上角 → 世界坐标（y 轴翻转，z=0 保证 scale=1 平面布局）
            world_pos: Vec3::new(x - 960.0, 540.0 - y, 0.0),
            z_order: z,
        }
    }

    /// 品牌条（跨 2 列）
    pub fn brand_card(id: NodeId) -> Self {
        Self::bento(id, PanelKind::Brand, 420.0, 210.0, 556.0, 64.0, 10)
    }

    /// 内核状态 pill 卡
    pub fn status_card(id: NodeId, text: &str) -> Self {
        let mut p =
            Self::bento(id, PanelKind::Status, 992.0, 210.0, 246.0, 64.0, 11);
        p.body = text.into();
        p
    }

    /// Uptime 卡
    pub fn clock_card(id: NodeId, value: &str) -> Self {
        let mut p =
            Self::bento(id, PanelKind::Clock, 1254.0, 210.0, 246.0, 64.0, 12);
        p.body = value.into();
        p
    }

    /// 主舞台卡（虚拟人叠加其上，caption 由 title 承载）
    pub fn hero_card(id: NodeId, caption: &str) -> Self {
        let mut p =
            Self::bento(id, PanelKind::Hero, 420.0, 290.0, 556.0, 456.0, 13);
        p.title = caption.into();
        p
    }

    /// 启动日志卡
    pub fn log_card(id: NodeId, lines: Vec<String>) -> Self {
        let mut p =
            Self::bento(id, PanelKind::Log, 992.0, 290.0, 508.0, 456.0, 15);
        p.lines = lines;
        p
    }

    /// 系统体征卡
    pub fn vitals_card(id: NodeId, vitals: Vec<Vital>) -> Self {
        let mut p =
            Self::bento(id, PanelKind::Vitals, 420.0, 762.0, 556.0, 108.0, 16);
        p.vitals = vitals;
        p
    }

    /// 启动进度卡
    pub fn progress_card(id: NodeId, stage: &str, pct: f32) -> Self {
        let mut p =
            Self::bento(id, PanelKind::Progress, 992.0, 762.0, 508.0, 108.0, 17);
        p.title = stage.into();
        p.progress = pct.clamp(0.0, 1.0);
        p
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
        self.nodes.sort_by_key(z_order_of);
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
        SceneNode::Avatar(a) => a.z_order,
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