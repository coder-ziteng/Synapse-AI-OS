//! 位图字体渲染（灰度 alpha 表 → `fb::px` 混合）。
//!
//! 数据见 `font_data`（Cascadia Mono, SIL OFL 1.1，构建期生成）。

use super::fb;
use super::font_data;

/// 一档位图字体。
pub struct Font {
    /// 字符步进宽（像素）。
    pub adv: u32,
    /// cell 高（像素）。
    pub h: u32,
    /// 灰度 alpha 表。
    pub bits: &'static [u8],
}

/// 小字档（日志 / 标签 / 副题）。
pub const S: Font = Font {
    adv: font_data::FONT_S_ADV,
    h: font_data::FONT_S_H,
    bits: font_data::FONT_S_BITS,
};

/// 大字档（时钟 / 百分比 / 品牌名 / READY 标题）。
pub const M: Font = Font {
    adv: font_data::FONT_M_ADV,
    h: font_data::FONT_M_H,
    bits: font_data::FONT_M_BITS,
};

/// 文本像素宽（含字距 `spacing`，`scale` 整数倍放大）。
pub fn text_width(text: &str, f: &Font, scale: u32, spacing: u32) -> i32 {
    let n = text.len() as u32;
    if n == 0 {
        return 0;
    }
    (n * (f.adv * scale + spacing) - spacing) as i32
}

/// 绘制文本（左上角 x,y；`alpha` 0–255；`scale` 最近邻整数倍）。
pub fn draw_text(
    x: i32,
    y: i32,
    text: &str,
    c: u32,
    alpha: u32,
    f: &Font,
    scale: u32,
    spacing: u32,
) {
    if alpha == 0 {
        return;
    }
    let adv = f.adv as i32;
    let h = f.h as i32;
    let mut cx = x;
    for ch in text.chars() {
        let g = if ('\x20'..='\x7e').contains(&ch) {
            ch as u32 - 0x20
        } else {
            0
        };
        let base = (g * f.h * f.adv) as usize;
        let mut yy = 0;
        while yy < h {
            let mut xx = 0;
            while xx < adv {
                let ga = f.bits[base + (yy as usize) * (adv as usize) + (xx as usize)] as u32;
                if ga != 0 {
                    let a = alpha * ga / 255;
                    if scale == 1 {
                        fb::px(cx + xx, y + yy, c, a);
                    } else {
                        let s = scale as i32;
                        fb::fill_rect(
                            cx + xx * s,
                            y + yy * s,
                            cx + xx * s + s,
                            y + yy * s + s,
                            c,
                            a,
                        );
                    }
                }
                xx += 1;
            }
            yy += 1;
        }
        cx += adv * scale as i32 + spacing as i32;
    }
}
