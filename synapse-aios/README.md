# Synapse AI-OS · 玄武 · 视觉识别系统

> 一个 AI 主导操作系统的品牌视觉与开机体验设计。
> **玄武**镇场，**神经突触**驱动内核。

---

## 📁 文件结构

```
synapse-aios/
├── logo/                                     # 品牌 Logo（4 版 PNG）
│   ├── synapse-xuanwu-color.png              # 主视觉版（多色 / 带光晕）
│   ├── synapse-xuanwu-color-on-dark.png      # 多色版（深底适用）
│   ├── synapse-xuanwu-mono-dark.png          # 单色版（深色图形，浅色 UI 适用）
│   └── synapse-xuanwu-mono-light.png         # 单色版（浅色图形，深色 UI 适用）
└── boot-animation/                           # 开机动画
    └── index.html                            # 单文件，开浏览器即可预览
```

---

## 🐢 设计语言

### Logo 核心元素

| 元素 | 象征意义 |
|------|---------|
| **玄武龟壳（六边形）** | 北方神兽 · 镇守 · 稳定 · 玄色主调 |
| **缠绕蛇身** | 灵动 · 阴阳合一 · 北之神 |
| **中心能量核** | AI 之心 · 系统核心 |
| **神经突触放射线** | Synapse（突触） · 信息互联 |
| **甲纹节点（发光点）** | 神经信号 · 数据流通 |

### 配色

| 色值 | 用途 |
|------|------|
| `#02050b` | 玄黑底色 |
| `#0d1421` / `#1f2a3d` | 龟壳主体渐变 |
| `#4dd0e1` | 主光晕色（青） |
| `#7c4dff` | 副光晕色（紫） |
| `#e0f7fa` | 高光 / 文字 |

---

## 🚀 使用方式

### Logo

直接嵌入 HTML：

```html
<img src="logo/synapse-xuanwu-color.png" alt="Synapse AI-OS" width="120">
```

或在 React 中：

```jsx
import logo from './synapse-xuanwu-color.png';
<img src={logo} className="w-32 h-32" alt="Synapse AI-OS" />
```

### 开机动画

```bash
# 直接双击打开
open boot-animation/index.html

# 或起本地服务（推荐，避免某些浏览器 file:// 限制）
cd boot-animation
python3 -m http.server 8000
# 然后访问 http://localhost:8000
```

动画时长：约 **9.5 秒**，结束后显示 REPLAY 按钮可重播。

---

## 🎬 开机动画分镜

| 时间 | 阶段 | 视觉 |
|------|------|------|
| 0–1.4s | 黑屏 | 启动数字 `[ BOOT 0% ]` 角落亮起 |
| 1.4–3.2s | Logo 觉醒 | 模糊 → 清晰，旋转 15° → 0° |
| 2.6–4.0s | 突触展开 | 6 条放射线依次描边 |
| 2.5–6.0s | 粒子汇聚 | Canvas 粒子从四周飞向中心 |
| 4.0–5.0s | 系统日志 | 左侧逐行打印启动日志 |
| 4.6–5.5s | 标题显现 | `SYNAPSE AI-OS` 逐字 fade-in |
| 5.0–8.0s | 进度条 | 底部进度条填充至 100% |
| 8.0–9.5s | 整体淡出 | 模糊 + 透明度 0 |
| 9.5s+ | 重播 | 右上角 `↻ REPLAY` 按钮 |

---

## 🔧 扩展建议

### 想换主色？

Logo 源文件（`display/examples/logo` 渲染管线导出）中替换 `4dd0e1` 和 `7c4dff` 后重新导出 PNG 即可。可以全局替换：
- 主色：`#4dd0e1` → 你的色值
- 副色：`#7c4dff` → 你的色值

### 开机动画时长调整？

修改 `index.html` 里所有 `animation-delay` 后的秒数即可。建议保持相对比例：
- logo 显现 1.4s 起步
- 标题 4.6s 起步
- 淡出 8s 起步

### 想做完整产品级开机体验？

下一步可加：
- **加载音效**：粒子汇聚 + logo 显现时配 low-frequency hum
- **第二阶段**：logo 缩小移至左上角，主体内容淡入（模拟桌面进入）
- **多分辨率 SVG**：导出 1x / 2x / 3x 适配高分屏
- **PWA / Electron splash**：把开机动画改造成 Electron 启动画面

---

## 📜 命名释义

- **Synapse** —— 神经突触，神经元之间信息传递的节点
- **Xuanwu (玄武)** —— 中国四象之一，镇北方，主水、主智、主冬
- **AI-OS** —— AI-Native Operating System，由 AI 主导驱动一切交互

**「北冥有鱼，其名为鲲... 玄武出，神经启。」**