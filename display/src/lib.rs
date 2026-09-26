//! Synapse S6 显示栈 PoC（宿主 std 渲染管线）
//!
//! ## 范围
//!
//! 本 crate 是 [设计文档 07](../../docs/design/07-display-stack-and-spatial-shell.md) 中
//! 「S6.0 显示骨架 MVP」的 PoC 实现，目的是在宿主 std 环境验证：
//!
//! 1. **矢量场景图**（`scene` 模块）—— Panel / Avatar / SpatialAnchor 的节点模型
//! 2. **空间投影**（`camera` 模块）—— SpatialCamera 3D → 2D 投影公式
//! 3. **2D 虚拟人**（`avatar` 模块）—— SVG-like Path 描述的角色 + 表情状态机
//! 4. **渲染管线**（`renderer` 模块，feature `renderer`）—— tiny-skia 软件光栅化
//! 5. **混合模式视觉效果**（`poc` 模块，feature `renderer`）—— 导出 PNG
//!
//! ## 不在 PoC 范围
//!
//! - IPC 接口（[设计文档 07 §3]）—— 实际显示服务是用户态进程，PoC 单进程跑通
//! - DynamicUIGenerator / OntologyEngine 联动 —— PoC 用硬编码场景
//! - 动画曲线库 —— PoC 用常数动画或单帧静态
//! - 真实字体（PoC 用路径绘字或缺省字体回退）
//!
//! ## 模块结构
//!
//! ```text
//! types       基础类型（Vec2/Vec3/Color/Transform）
//! scene       场景图节点（SceneGraph, PanelNode, AvatarNode, Anchor）
//! camera      SpatialCamera 3D→2D 投影
//! avatar      2D 虚拟人（Live2D 风格简化，6 表情 + 待机动画）
//! renderer    Renderer trait + tiny-skia 实现（feature-gated）
//! poc         main 示例：跑通混合模式最小视觉，导出 PNG
//! ```

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod avatar;
pub mod camera;
pub mod scene;
pub mod types;

#[cfg(feature = "renderer")]
pub mod renderer;

#[cfg(feature = "renderer")]
pub mod poc;