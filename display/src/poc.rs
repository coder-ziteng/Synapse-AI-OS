//! 混合模式最小视觉 POC
//!
//! 跑通最小化的"空间外壳 + 平面内容"混合模式（玄武视觉 v2）：
//! - Bento Grid 7 卡（brand/status/clock/hero/log/vitals/progress，Liquid Glass）
//! - 1 个赛博朋克虚拟人（hero 卡主舞台，6 表情状态机）
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
use crate::scene::{Panel, SceneGraph, SceneNode, Vital, VitalHue};
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

    // 1. 构造场景（Bento Grid 7 卡，对齐玄武开机视觉分镜）
    let mut scene = SceneGraph::new();

    scene.add(SceneNode::Panel(Panel::brand_card(0)));
    scene.add(SceneNode::Panel(Panel::status_card(1, "BOOTING")));
    scene.add(SceneNode::Panel(Panel::clock_card(2, "9.52")));
    scene.add(SceneNode::Panel(Panel::hero_card(3, "玄武 · XUANWU")));
    scene.add(SceneNode::Panel(Panel::log_card(
        4,
        vec![
            "stage2 EDD read ok, A20 gate opened".into(),
            "[ OK ] long mode entered, identity-map 0-4 GiB".into(),
            "[ OK ] kernel 5.1 MiB loaded @ 0x200000".into(),
            "[INFO] serial COM1 online, 115200 8N1".into(),
            "[ OK ] GDT / TSS loaded, IST1 double-fault armed".into(),
            "[ OK ] IDT 256 vectors, #PF #GP #DF handlers set".into(),
            "[ OK ] PIC remapped, PIT 100 Hz IRQ0 ticking".into(),
            "[ OK ] heap online, first-fit 4096 KiB pool".into(),
            "[INFO] TSC calibrated 3.199 GHz (PIT cross-check)".into(),
            "[ ** ] smoke: 57/57 checks passed".into(),
        ],
    )));
    scene.add(SceneNode::Panel(Panel::vitals_card(
        5,
        vec![
            Vital {
                name: "NEURAL LOAD".into(),
                value: "78".into(),
                ratio: 0.78,
                hue: VitalHue::Cyan,
            },
            Vital {
                name: "MEMORY".into(),
                value: "1.9 / 16 GB".into(),
                ratio: 0.12,
                hue: VitalHue::Violet,
            },
            Vital {
                name: "SYNAPSE SYNC".into(),
                value: "100".into(),
                ratio: 1.0,
                hue: VitalHue::Mint,
            },
        ],
    )));
    scene.add(SceneNode::Panel(Panel::progress_card(6, "SERVICES", 0.76)));

    // 虚拟人：hero 卡主舞台（z_order 14 叠在 hero 卡玻璃之上）
    let mut avatar = Avatar {
        position: Vec3::new(-262.0, 40.0, 0.0),
        body_radius_x: 100.0,
        body_radius_y: 140.0,
        head_radius: 58.0,
        z_order: 14,
        ..Default::default()
    };
    avatar.set_expression(options.expression);
    scene.add(SceneNode::Avatar(avatar));

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