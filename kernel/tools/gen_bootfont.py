#!/usr/bin/env python3
"""gen_bootfont.py — 生成内核开机动画位图字体（kernel/src/bootanim/font_data.rs）。

字体源：Cascadia Mono（SIL Open Font License 1.1，允许嵌入二进制产物）。
渲染为 8-bit 灰度 alpha 表（抗锯齿边缘直接作为 blend 系数），等宽 cell。

用法：
    python kernel/tools/gen_bootfont.py [ttf 路径]

输出两档：
    FONT_S — 日志/标签/小字（em 16px）
    FONT_M — 时钟/百分比/品牌名（em 24px）
覆盖 ASCII 0x20..=0x7E。
"""
import sys
from PIL import Image, ImageDraw, ImageFont

FIRST, LAST = 0x20, 0x7E
GLYPHS = [chr(c) for c in range(FIRST, LAST + 1)]

SPECS = [
    ("S", 16),
    ("M", 24),
]


def render(ttf_path: str, em_size: int):
    font = ImageFont.truetype(ttf_path, em_size)
    adv = int(round(font.getlength("M")))
    ascent, descent = font.getmetrics()
    height = ascent + descent
    # 画布余量：抗锯齿边缘 + descender
    buf = {}
    for ch in GLYPHS:
        img = Image.new("L", (adv + 8, height + 8), 0)
        d = ImageDraw.Draw(img)
        d.text((4, 4), ch, fill=255, font=font)
        # 裁回 cell：左 4px 余量去掉，保留 adv 宽 × height 高（基线对齐 ascent+4）
        cell = img.crop((4, 4, 4 + adv, 4 + height))
        buf[ch] = list(cell.getdata())
    return adv, height, buf


def emit(name: str, adv: int, height: int, buf) -> str:
    lines = []
    lines.append(f"pub const FONT_{name}_ADV: u32 = {adv};")
    lines.append(f"pub const FONT_{name}_H: u32 = {height};")
    lines.append(f"/// ASCII 0x{FIRST:02X}..=0x{LAST:02X} 灰度 alpha 表，row-major [glyph][y][x]。")
    lines.append(f"pub const FONT_{name}_BITS: &[u8] = &[")
    flat = []
    for ch in GLYPHS:
        flat.extend(buf[ch])
    for i in range(0, len(flat), 24):
        chunk = flat[i:i + 24]
        lines.append("    " + " ".join(f"0x{b:02x}," for b in chunk))
    lines.append("];")
    return "\n".join(lines)


def main():
    ttf = sys.argv[1] if len(sys.argv) > 1 else r"C:\Windows\Fonts\CascadiaMono.ttf"
    out = []
    out.append("//! 开机动画位图字体数据（生成产物，勿手改）。")
    out.append("//!")
    out.append("//! 源字体：Cascadia Mono（SIL OFL 1.1）。")
    out.append("//! 生成：`python kernel/tools/gen_bootfont.py`。")
    out.append("")
    for name, em in SPECS:
        adv, height, buf = render(ttf, em)
        out.append(emit(name, adv, height, buf))
        out.append("")
        print(f"FONT_{name}: adv={adv} h={height} bytes={adv * height * len(GLYPHS)}")
    src = "\n".join(out)
    dst = r"d:\ai-os\kernel\src\bootanim\font_data.rs"
    import os
    os.makedirs(os.path.dirname(dst), exist_ok=True)
    with open(dst, "w", encoding="utf-8", newline="\n") as f:
        f.write(src)
    print("written", dst)


if __name__ == "__main__":
    main()
