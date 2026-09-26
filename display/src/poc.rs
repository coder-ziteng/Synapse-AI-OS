//! 混合模式最小视觉 POC
//!
//! 跑通最小化的"空间外壳 + 平面内容"混合模式：
//! - 1 个虚拟人（锚在屏幕左下）
//! - 2 个面板（任务卡片 + 通知卡片，锚定到虚拟人附近）
//! - 1 个锚点（FocusPoint 虚拟演示用）
//!
//! 渲染输出 PNG，文件名由调用方传入。
//!
//! ## 运行方式
//!
//! ```text
//! cargo run --example poc -- --output output/poc_mix_neutral.png --expr neutral
//! cargo run --example poc -- --output output/poc_mix_happy.png --expr happy
//! cargo run --example poc -- --output output/poc_mix_thinking.png --expr thinking
//! ```

use crate::avatar::{Avatar, Expression};
use crate::camera::{SpatialCamera, Viewport};
use crate::renderer::{save_png, Renderer, TinySkiaRenderer};
use crate::scene::{Panel, SceneGraph, SceneNode};
use crate::types::Vec3;

/// 渲染参数
#[derive(Debug, Clone)]
pub struct PocOptions {
    /// 输出 PNG 路径
    pub output: String,
    /// 视口
    pub viewport: Viewport,
    /// 虚拟人初始表情
    pub expression: Expression,
}

impl PocOptions {
    /// 默认 PoC 参数
    pub fn default_with_output(output: &str) -> Self {
        Self {
            output: output.into(),
            viewport: Viewport::FHD,
            expression: Expression::Neutral,
        }
    }
}

/// 跑通 PoC，返回渲染耗时（毫秒）
pub fn run(options: &PocOptions) -> Result<f64, String> {
    let start = std_time_now_ms();

    // 1. 构造场景
    let mut scene = SceneGraph::new();

    // 虚拟人：屏幕左下，主舞台
    let mut avatar = Avatar::default();
    avatar.position = Vec3::new(-280.0, 40.0, 0.0);
    avatar.set_expression(options.expression);
    scene.add(SceneNode::Avatar(avatar));

    // 任务卡片：锚定到虚拟人右侧，z=600（中等距离）
    scene.add(SceneNode::Panel(Panel::task_card(
        0,
        "会议纪要",
        "5 项任务待办,2 项需今天完成",
    )));

    // 通知卡片：屏幕左上（系统级），z=200（较近）
    scene.add(SceneNode::Panel(Panel::notice_card(
        1,
        "新消息:合同审批",
    )));

    // 2. 构造摄像机
    let camera = SpatialCamera::new(options.viewport);

    // 3. 渲染
    let mut renderer =
        TinySkiaRenderer::new(options.viewport.width, options.viewport.height)
            .ok_or_else(|| "Pixmap::new failed".to_string())?;
    renderer.render(&scene, &camera);

    // 4. 保存 PNG
    save_png(&renderer, &options.output)?;

    let elapsed = std_time_now_ms() - start;
    Ok(elapsed)
}

/// 简易时间戳（host std 环境，等价于 SystemTime::now）
fn std_time_now_ms() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poc_neutral_renders_to_png() {
        let tmp = std::env::temp_dir().join("synapse_poc_neutral.png");
        let opts = PocOptions::default_with_output(tmp.to_str().unwrap());
        let elapsed = run(&opts).expect("POC render failed");
        assert!(elapsed >= 0.0);
        assert!(tmp.exists(), "PNG should exist at {:?}", tmp);
        let meta = std::fs::metadata(&tmp).unwrap();
        assert!(meta.len() > 0, "PNG should have non-zero size");
    }

    #[test]
    fn poc_happy_renders_to_png() {
        let tmp = std::env::temp_dir().join("synapse_poc_happy.png");
        let mut opts = PocOptions::default_with_output(tmp.to_str().unwrap());
        opts.expression = Expression::Happy;
        run(&opts).expect("POC render failed");
        assert!(tmp.exists());
    }
}