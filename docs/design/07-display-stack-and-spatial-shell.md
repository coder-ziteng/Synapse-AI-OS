# 设计文档 07：显示栈与空间外壳 (Display Stack & Spatial Shell)

> 状态：**DECIDED**（选型与边界已收敛，剩余细化为实现阶段 TBD）
> 关联需求：[需求目标 §三之二 S6](../需求目标.md)、原始构想「图形化界面 + AI 助理 + 类虚拟现实」「速度/稳定性/流畅度」
> 关联里程碑：**S6 本体模型与动态 UI**
> 前置依赖：内核 P6（virtio-gpu 用户态驱动 + framebuffer 输出原语）、S5（多 Agent + 五级权限 enforcement）
> 最后更新：2026-09-26

---

## DECIDED 决策汇总

| 决策项 | DECIDED 方案 | 章节 |
|--------|-------------|------|
| 渲染栈 | **tiny-skia**（Rust 纯 Rust 2D 矢量光栅化，no_std 友好）+ 自研矢量场景图 | §2 |
| 显示驱动 | **virtio-gpu** 用户态驱动（内核仅 MMIO/IRQ 转发，参考 P4.5 IRQ 用户态化） | §3 |
| 合成器 | **自研极简合成器**（分层 SceneGraph → framebuffer 的单遍绘制，无 GPU shader 依赖） | §3 |
| 虚拟人起步 | **2D 矢量动画**（Live2D 风格，关键帧 + 骨骼变形），后期可换 3D 角色 | §4 |
| 交互范式 | **混合模式**：空间外壳（3D 摄像机 + 锚点 + 虚拟人）+ 平面内容（动态卡片面板）| §4 |
| UI 生成 | **DynamicUIGenerator** 读取 OntologyEngine 状态 → 生成 Card/Panel JSON Schema → 合成器消费 | §5 |
| 输入融合 | **空间手势 / 视线 / 语音** 三模态事件流，统一进入 InputFusionBus | §6 |
| 性能预算 | **60 FPS @ 1080p**（16.6ms 帧时间）、合成延迟 <8ms、输入到首帧延迟 <100ms | §7 |
| 加速路径 | 首期 CPU 软件光栅化跑通 → 后期接 virtio-gpu 硬件加速（同一 SceneGraph 不变） | §2.3 |

> ✅ 显示栈核心选型与边界已收敛。本文档的 TBD 集中在实现细节（场景图节点类型、卡片 schema 字段、动画曲线库），不阻塞 S6 启动。

---

## 0. 文档目的

原始构想明确：

> "界面是图形化界面，不是控制台" + "AI 助理（虚拟人）+ 类虚拟现实的交互逻辑" + "核心是速度、稳定性、流畅度"。

同时 [需求目标 R11](../需求目标.md) 登记："原始构想明确是图形化界面，当前方案无显示栈（virtio-gpu / 合成器 / 渲染管线均缺）"。本文档把 S6 显示栈从「一行登记」收敛为可执行的边界、接口、组件与阶段。

文档回答五个问题：
1. 显示栈跑在哪一层、内核要不要碰图形？
2. 渲染引擎选什么、为什么不是 WebView？
3. "类虚拟现实"如何在不堆 3D 控件的前提下落地？
4. DynamicUIGenerator 与 OntologyEngine / 外交工具如何协作？
5. 怎么保证 60 FPS 的速度/稳定性/流畅度目标？

---

## 1. 范围与边界

### 1.1 内核契约：显示 = 一类 capability

内核**不实现任何图形逻辑**，仅提供与显示相关的两类基础原语（与 P4.5 阶段 IRQ 用户态化同构）：

| 内核原语 | 作用 | Capability |
|---------|------|-----------|
| **`framebuffer_alloc`** | 分配物理连续页 + 映射到进程虚拟地址 | `Framebuffer` |
| **`gpu_irq_subscribe`** | 订阅 virtio-gpu 的设备 IRQ Notification | `Device + IRQ_BIND` |

图形合成、场景图、动画、文本排版、字体、光标、UI 控件——全部在用户态显示服务进程中完成。内核对「这块显存用来显示什么」零感知。

**架构性不变量**：只有持有 `Framebuffer + Device` capability 的进程能直接写显示；其他进程必须经 IPC 调用显示服务。外交工具不持有显示 capability（[Doc 04 §3.2](04-diplomat-channel-architecture.md) 唯一网络出口不变量）的对偶——这里是「唯一显示入口不变量」。

### 1.2 用户态分层

```
┌──────────────────────────────────────────────────────────────────┐
│  S6  Application Layer（应用层）                                  │
│   ├── DynamicUIGenerator（读 OntologyEngine → 生成 UI）          │
│   ├── AvatarController（虚拟人状态机 + 表情/动作）               │
│   └── InputFusionBus（输入融合 → 意图）                          │
│  ────────── S6 服务边界（IPC）────────────────────────────── │
│  S6  Display Service（显示服务，独立进程，system-wide 单例）     │
│   ├── SceneGraph（矢量场景图 + 空间外壳 + 平面内容树）          │
│   ├── SpatialCamera（虚拟摄像机，3D 坐标投影到 2D 视口）        │
│   ├── Compositor（场景图 → draw list → 光栅化）                 │
│   ├── TextShaper（字体 + 排版 + RTL/CTL）                       │
│   ├── AnimationEngine（属性动画 + 曲线 + 骨骼变形）             │
│   ├── tiny-skia Renderer（软件光栅化 / 后期 virtio-gpu 加速）  │
│   └── virtio-gpu Driver（PCI 设备驱动，复用外交工具同类）       │
│  ────────── 内核边界（IPC + syscall）────────────────────────── │
│  Synapse Kernel Phase 6（framebuffer_alloc / gpu_irq_subscribe）│
└──────────────────────────────────────────────────────────────────┘
```

### 1.3 进程拓扑

| 进程 | 数量 | 权限 |
|------|------|------|
| **DisplayService** | 单例 system 服务 | Framebuffer + Device (virtio-gpu) + 全局屏事件输入 capability |
| **InputService** | 单例 system 服务 | 输入设备 capability（键盘 / 鼠标 / 触控 / 麦克风 / 摄像头） |
| **OntologyEngine** | 单例（与 S4 共享） | GraphStore + 向量检索 capability |
| **AvatarService** | 可选独立 / 内嵌于 DisplayService | 2D 角色资源 + 骨骼动画数据 |
| **每个应用 / Agent UI** | N（沙箱内） | IPC 调 DisplayService（受限 UI 提交 API） |

应用不直接接触 framebuffer 或 virtio-gpu——与外交工具不让应用直接接触 NIC 是同一架构哲学。

---

## 2. 渲染栈选型

### 2.1 三候选详细对比

| 维度 | **A. wgpu / vulkan 原生** | **B. Skia C++ 绑定** | **C. tiny-skia + 自研场景图（DECIDED）** |
|------|---|---|---|
| **速度** | ★★★★★（GPU 原生） | ★★★★（Skia 经优化） | ★★★（CPU 光栅化，1080p 单帧 6~10ms） |
| **稳定性** | 驱动兼容性问题（vulkan/mesa 多版本） | Skia C++ 边界 FFI 安全风险 | 纯 Rust，无 FFI |
| **流畅度** | 60+ FPS 充裕 | 60 FPS 充裕 | 60 FPS 可达（中等场景）|
| **no_std / 裸机用户态** | 需 GPU 驱动栈 | 需 libc 适配 | **原生 no_std**，可在用户态直接编译 |
| **与外交工具同栈复用** | 否（驱动栈独立）| 否（C++ FFI 污染）| **是**（外交工具已用 Rust 用户态栈）|
| **AI 生成 UI 适配性** | 需自建场景图 | 需自建场景图 | 直接以矢量 Path 为节点即可 |
| **工作量** | 6 个月+（自研 + shader）| 8~12 周（绑定 + 合成器）| **4~6 周**（场景图 + 合成器 + UI 生成）|
| **个人可行性** | ❌ 不可行 | 风险高（FFI/依赖）| ✅ 可行 |

**否决 A 的原因**：wgpu/vulkan 在裸机用户态需自研 GPU 驱动栈（与外交工具用户态 virtio-net 驱动同等工作量），个人项目不可承受。

**否决 B 的原因**：Skia 是 C++，与本项目"内核安全核心 100% Safe Rust"+"外交工具用户态零 FFI 污染"的架构哲学冲突；移植工作量并不显著小于 C。

### 2.2 tiny-skia 核心约束

- **纯 Rust 2D 矢量光栅化库**，支持 Path / 渐变 / 阴影 / 文本（基础字体）。
- **CPU 路径**：`Pixmap::fill_path` / `draw_path` / `stroke_path`，单线程单遍。
- **API 形态**：`Pixmap`（目标缓冲）+ `Paint`（绘制描述）—— 易于自研合成器驱动。
- **风险点**：复杂 shader 效果（径向模糊、SVG filter）当前不支持，需手写软件实现或绕开。

### 2.3 硬件加速迁移路径（不破坏架构）

```
当前（Phase 6.0）                  未来（Phase 6.x 远期）
─────────────────                  ─────────────────
CPU 软件光栅化                     →   virtio-gpu 硬件加速
  ↓                                    ↓
SceneGraph ─┐                          SceneGraph ─┐
            ├─→ draw list → Pixmap               ├─→ draw list → GPU command buffer
            │                       →            │
  tiny-skia  Renderer                 wgpu / vulkan backend（替换 Renderer 实现）
```

**接口稳定**：`Renderer` trait 暴露 `render(scene: &SceneGraph, output: &mut Framebuffer)` 方法，SceneGraph 与合成器零修改。Phase 6.x 引入 GPU backend 时，仅替换 Renderer 实现，UI 生成器、虚拟人、动画引擎全部不动。

---

## 3. ABI：显示服务 IPC 接口

### 3.1 进程间通信总览

| 调用方 | → 被调方 | 接口 | 频率 |
|--------|---------|------|------|
| 应用 / Agent | → DisplayService | `SubmitPanel { schema }` | 用户操作触发 |
| 应用 / Agent | → DisplayService | `AnimateNode { id, props, duration }` | 高频（动画）|
| OntologyEngine 状态变化 | → DisplayService | `StateChanged { snapshot }` | 中频 |
| InputService | → DisplayService | `PointerEvent { x, y, kind }` | 高频 |
| InputService | → 应用 | `UserIntent { raw, fused }` | 低频 |
| DisplayService | → 内核 | `framebuffer_alloc` / `flip` | 每帧 1 次 |

### 3.2 SubmitPanel IPC 接口草案

```rust
// user/display_service/src/ipc.rs (概念设计)
pub enum DisplayRequest {
    /// 提交一个动态生成的 UI 面板（应用 → DisplayService）
    SubmitPanel {
        panel_id: PanelId,
        anchor: SpatialAnchor,          // 锚定到空间外壳的哪个锚点
        schema: PanelSchema,            // DynamicUIGenerator 输出
        z_order: i16,
        transition: Option<TransitionSpec>,
    },
    /// 更新已有面板（局部 diff，避免重建）
    UpdatePanel {
        panel_id: PanelId,
        diff: PanelDiff,
    },
    /// 移除面板（动画离场）
    RemovePanel {
        panel_id: PanelId,
        exit_transition: Option<TransitionSpec>,
    },
    /// 提交虚拟人动画触发请求（AvatarService → DisplayService）
    TriggerAvatarAnimation {
        avatar_id: AvatarId,
        clip: AnimationClip,
        loop_count: u8,
    },
}

pub struct PanelSchema {
    pub root: PanelNode,
    pub data_bindings: Vec<DataBinding>,  // 绑定 OntologyEngine 状态
}

pub enum PanelNode {
    Card { id: NodeId, layout: Layout, content: CardContent },
    Group { id: NodeId, layout: Layout, children: Vec<PanelNode> },
    Text { id: NodeId, layout: Layout, runs: Vec<TextRun> },
    Image { id: NodeId, layout: Layout, asset: ImageRef },
    Button { id: NodeId, layout: Layout, label: NodeId, on_tap: ActionRef },
    // ... 按需扩展
}
```

### 3.3 与外交工具的协同

外交工具出站请求若产生"文件落地"事件 → 通知 DisplayService → DynamicUIGenerator 读取 OntologyEngine 中 `File` 对象状态 → 生成"已下载文件"卡片面板 → 锚定到虚拟人旁。

入站消息（IM、邮件）走 Realtime Channel → 同上路径在空间生成通知卡片。

**关键约束**：DisplayService 不直接接收外交工具推送，而是订阅 OntologyEngine 的 `ObjectCreated` / `StateChanged` 事件——避免 DisplayService 与外交工具产生 IPC 强耦合。

---

## 4. 空间外壳与混合模式

### 4.1 三层结构

```
[ 空间外壳 (Spatial Shell) ]
├─ SpatialCamera     — 3D 摄像机（位置 / 朝向 / 焦距 / FOV）
├─ SpatialAnchors    — 空间中可挂载面板的"挂钩"（虚拟人旁、屏幕边缘、用户凝视点）
├─ Avatar            — 虚拟人（始终在场，2D 矢量动画起步）
└─ Background        — 程序化生成的动态背景（环境光 / 模糊层 / 焦点指示）

[ 平面内容 (Flat Content) ]
├─ Card              — 最小信息单元（纯 2D，无 3D 变换）
├─ Panel             — 卡片组合容器
├─ Dialog            — 模态对话框（用于 L4/L5 风险确认）
└─ Overlay           — 浮层（提示 / 菜单 / 通知）

[ 模式切换器 (Mode Switcher) ]
├─ 全空间模式（虚拟人 + 散落卡片）
├─ 全平面模式（虚拟人缩小至角落 / 隐藏，主视图变平面卡片墙）
└─ 混合模式（默认）— 虚拟人主舞台 + 卡片锚定到锚点
```

### 4.2 SpatialCamera 与 3D → 2D 投影

**关键设计原则**：3D 外壳极简，只提供空间框架感，不堆 3D 控件。

```
3D 世界坐标 (x, y, z)               2D 屏幕坐标 (sx, sy)
       │                                   ▲
       │  SpatialCamera 投影               │
       │  （透视投影 + 透视矩阵）            │
       ▼                                   │
3D 视口坐标 (vx, vy)  ──────→  2D 屏幕坐标 (sx, sy)
       │
       │  卡片在这里
       │  - 不参与 z 深度
       │  - 始终面向摄像机（billboard）
       │  - 仅用 z 决定遮挡顺序与缩放（远小近大）
```

**投影公式**（概念）：
```
sx = vx * focal / (vz + focal)
sy = vy * focal / (vz + focal)
scale = focal / (vz + focal)   // 远小近大
opacity = if vz > far_plane { 0 } else { 1 }  // 远平面外渐隐
```

### 4.3 虚拟人 (Avatar) 设计

**起步（Phase 6.0）**：

- 2D 矢量动画角色，由 SVG-like Path 描述关键部件（眼 / 口 / 头 / 手）。
- 骨骼动画由 AnimationClip 驱动（关键帧插值）。
- 状态：待机 / 思考（语音转文字时）/ 播报 / 确认（等待用户回复）。
- 表情：6 种基础表情（neutral / happy / thinking / confused / alert / asleep）。

**架构预留（Phase 6.x 远期）**：

- `AvatarBackend` trait，抽象渲染后端：`Live2DBackend` / `VRoidBackend` / `WebGLBackend`。
- 当前仅实现 `Live2DBackend`，后期可加 3D 角色不动 SceneGraph 接口。

### 4.4 锚点 (Spatial Anchor) 模型

| 锚点 | 用途 | 位置策略 |
|------|------|---------|
| `AvatarRight` | 虚拟人右侧，常驻任务面板 | 跟随虚拟人位置 |
| `AvatarLeft` | 虚拟人左侧，临时通知卡片 | 飞入/飞出动画 |
| `ScreenTopRight` | 系统级通知 | 屏幕固定 |
| `FocusPoint` | 用户当前凝视点 | 视线输入驱动 |
| `Floating` | 自由摆放（用户拖拽） | 用户控制 |

**DynamicUIGenerator 决策锚点**：根据内容紧急度（`severity`）+ 类型（`notification` / `task` / `media`）+ 用户偏好档案（档案记忆）选择锚点。

---

## 5. DynamicUIGenerator

### 5.1 设计哲学

> 用户说"整理会议录音" → 不是打开一个录音 App，而是 AI 读取 OntologyEngine 中 `MeetingRecording` 对象 → 生成"录音列表卡片"+"进度面板"+"纪要预览卡片" → 锚定到 `AvatarRight` → 平滑飞入空间。

**UI 不是固定的窗口**，是 OntologyEngine 状态的**渲染产物**。

### 5.2 生成流程

```
┌────────────────────────────────────────────────────────────────┐
│  Trigger（用户意图 / 状态变化 / 定时 / 外交工具事件）             │
└─────────────────────────────────┬──────────────────────────────┘
                                  ▼
┌────────────────────────────────────────────────────────────────┐
│  Step 1: 意图解析                                                │
│   - Planner 拆解 → DAG 任务节点                                   │
│   - 调用远程 LLM API（走外交工具 API Channel）                    │
│   - 产出 UI 意图：{ kind, target_objects, severity }             │
└─────────────────────────────────┬──────────────────────────────┘
                                  ▼
┌────────────────────────────────────────────────────────────────┐
│  Step 2: 本体匹配                                                │
│   - 在 OntologyEngine 查询目标对象                              │
│   - 加载对象的 ontology 类型定义（字段、关系、可用动作）          │
└─────────────────────────────────┬──────────────────────────────┘
                                  ▼
┌────────────────────────────────────────────────────────────────┐
│  Step 3: UI Schema 生成                                          │
│   - 根据 ontology 类型 → 选择 UI 模板（Card / Panel / Dialog）   │
│   - 模板参数化（对象字段填入模板槽位）                           │
│   - 选择锚点 + 动画策略                                          │
│   - 输出 PanelSchema JSON                                        │
└─────────────────────────────────┬──────────────────────────────┘
                                  ▼
┌────────────────────────────────────────────────────────────────┐
│  Step 4: 提交 DisplayService                                     │
│   - SubmitPanel IPC                                              │
│   - DisplayService 校验 schema（防恶意生成）                    │
│   - 加入 SceneGraph                                              │
└────────────────────────────────────────────────────────────────┘
```

### 5.3 UI 模板库

| 模板 | 适用对象 | 关键组件 |
|------|---------|---------|
| `ObjectCard` | 任意 ontology 对象 | 标题 + 元数据网格 + 缩略图 + 动作按钮 |
| `ListPanel` | 集合对象（如录音列表） | 列表项 + 过滤栏 + 批量动作 |
| `FormPanel` | 待输入对象（如新建合同） | 字段输入 + 校验 + 提交按钮 |
| `ChartPanel` | 数据对象（统计 / 时间线） | 图表 + 时间范围 + 导出 |
| `ConfirmDialog` | L4/L5 操作确认 | 风险说明 + 二次确认 + 撤销时限 |
| `MediaCard` | 媒体对象（图片 / 视频） | 预览 + 元数据 + 关联对象链接 |

模板本身是 ontology 的一部分（应用开发者或系统本体定义），不是固定死的代码。Phase 6.0 首期内置上述 6 类模板 + 1 个 `CustomPanel` 兜底（接受任意 PanelSchema）。

### 5.4 数据绑定（活态 UI）

```rust
pub struct DataBinding {
    pub node_id: NodeId,
    pub source: BindingSource,         // 绑定到哪个 ontology 对象
    pub path: String,                  // 对象字段路径
    pub transform: BindingTransform,   // 类型转换 / 格式化
}
```

**ontology 状态变化 → 卡片自动更新**：用户编辑合同 → OntologyEngine `Contract.updated` → DisplayService 监听 → 重渲染相关卡片 → 平滑过渡动画。

---

## 6. 输入融合 (Input Fusion)

### 6.1 三模态输入

| 模态 | 设备 | 事件形态 |
|------|------|---------|
| **空间手势** | 摄像头 + 深度传感 / 触控板 / 鼠标 | `GestureEvent { kind, position, scale, rotation }` |
| **视线** | 摄像头（眼动追踪）或鼠标落点 | `GazeEvent { focus_point, dwell_time }` |
| **语音** | 麦克风（STT 走外交工具 Realtime Channel） | `VoiceEvent { text, intent_hint, confidence }` |

### 6.2 InputFusionBus

```rust
pub trait InputFusionBus {
    /// 三模态事件统一入口（InputService → Fusion）
    fn submit(&mut self, event: InputEvent);

    /// 输出融合后的用户意图（Fusion → 应用 / Planner）
    fn next_intent(&mut self) -> Option<UserIntent>;
}

pub enum InputEvent {
    Pointer(PointerEvent),
    Gesture(GestureEvent),
    Gaze(GazeEvent),
    Voice(VoiceEvent),
    Keyboard(KeyboardEvent),
}

pub struct UserIntent {
    pub raw: Vec<InputEvent>,            // 原始事件序列
    pub fused: FusedIntent,              // 融合后的意图
    pub confidence: f32,
}
```

### 6.3 融合策略

| 场景 | 主导模态 | 辅助模态 |
|------|---------|---------|
| **指向操作** | 视线（焦点）| 手势（确认点击）|
| **空间拖拽** | 手势（位置）| 视线（焦点验证）|
| **语音指令** | 语音（语义）| 手势（指向对象）|
| **文本输入** | 键盘 | — |
| **确认弹窗** | 语音（"确认"）或手势（点头）| — |

**冲突消解**：当多模态事件矛盾（如语音说"取消"但手势"确认"），按 L4/L5 风险等级处理：
- L1~L3 操作：取置信度高者 + 显示小提示"我理解为 X，确认？"
- L4~L5 操作：**强制模态确认**（语音 + 手势双确认，与 [Doc 01 §5.1](01-capability-agent-permission-model.md) 串口确认通道对接）

### 6.4 输入到首帧延迟预算

```
InputService 接收事件 ──┐
                       │ 0~2ms（IPC 接收）
FusionBus 融合 ─────────┤
                       │ 1~3ms（融合计算）
Planner 意图理解 ───────┤
                       │ 50~80ms（远程 LLM API）
DynamicUIGenerator ────┤
                       │ 10~20ms（schema 生成）
DisplayService 合成 ────┤
                       │ 8~12ms（场景图 + 光栅化）
首次呈现给用户 ─────────┘
                       总计：< 120ms（满足"流畅"感知）
```

---

## 7. 性能预算与流畅度保证

### 7.1 帧时间预算（60 FPS @ 1080p）

| 阶段 | 预算 | 实测目标 |
|------|------|---------|
| InputEvent → SceneGraph 更新 | 1ms | < 0.5ms |
| SceneGraph 重计算（layout / 动画推进） | 2ms | < 1ms |
| 合成器生成 draw list | 1ms | < 0.5ms |
| tiny-skia 光栅化（CPU 路径） | 10ms | 6~8ms（中等场景）|
| framebuffer flip | 0.5ms | < 0.3ms |
| **总帧时间** | **16.6ms** | **< 12ms**（留 4ms 缓冲）|

### 7.2 场景复杂度预算

| 项 | 上限 |
|----|------|
| 同时可见的 Panel 数 | ≤ 16 |
| 单 Panel 节点数 | ≤ 64 |
| 同时运行的动画数 | ≤ 32 |
| 文字排版字符数（单帧）| ≤ 8000 |
| 矢量 Path 复杂度（单帧）| ≤ 50k 段 |

超出预算触发 **SceneGraphBudgetEnforcer**：远处 Panel 降低刷新率（30 FPS）/ 远处动画降低插值密度 / 非焦点 Panel 渲染到 0.5x 缩略图。

### 7.3 稳定性保证

| 机制 | 作用 |
|------|------|
| **脏矩形局部重绘** | 仅重绘状态变化区域，CPU 路径下帧时间可降至 2~4ms |
| **动画离线推进** | 动画状态机独立于渲染线程，避免动画卡顿 |
| **场景图版本号** | 每次提交 Panel 生成新版本号，回滚 / 重放支持 |
| **Panic 隔离** | DisplayService panic 时显示"系统恢复中"兜底画面，不影响内核 |

### 7.4 流畅度可观测性

DisplayService 暴露 Prometheus-like 指标：
- `display.frame_time_ms`（直方图）
- `display.panel_count`（仪表）
- `display.cache_hit_ratio`（仪表）
- `display.input_to_first_frame_ms`（直方图）
- `display.skia_render_time_ms`（直方图）

通过外交工具可对外暴露 → 用户远程可观测。

---

## 8. 阶段拆分（与 S6 路线图对齐）

### 8.1 S6.0：显示骨架（MVP）

- [ ] 内核 P6：`framebuffer_alloc` + `gpu_irq_subscribe` capability
- [ ] DisplayService 进程骨架（IPC 服务注册）
- [ ] virtio-gpu 用户态驱动（PCI 设备，2D 模式 `VIRTIO_GPU_CMD_RESOURCE_2D`）
- [ ] SceneGraph 极简实现（无空间外壳，仅一个根 Panel）
- [ ] tiny-skia 软件光栅化 path
- [ ] InputService：键盘 + 鼠标事件 → PointerEvent
- [ ] 退出标准：QEMU 内能看到一张纯色背景 + 一个静态按钮 + 点击反馈

### 8.2 S6.1：空间外壳

- [ ] SpatialCamera 实现（透视投影）
- [ ] 锚点系统（5 类锚点）
- [ ] 虚拟人 2D 起步（Live2D 风格，单一角色 6 表情）
- [ ] Panel billboard 行为（始终面向摄像机）
- [ ] 退出标准：虚拟人 + 卡片锚定到 `AvatarRight`，可拖动相机

### 8.3 S6.2：DynamicUIGenerator

- [ ] 6 类 UI 模板（ObjectCard / ListPanel / FormPanel / ChartPanel / ConfirmDialog / MediaCard）
- [ ] DataBinding 与 OntologyEngine 状态订阅
- [ ] Planner 输出 UI 意图 → 模板选择 → PanelSchema 生成
- [ ] 退出标准：用户语音"整理录音" → 录音列表卡片自动出现在 `AvatarRight`

### 8.4 S6.3：输入融合

- [ ] InputFusionBus 实现
- [ ] 语音接入（STT 走外交工具 Realtime Channel）
- [ ] 视线 / 手势模态接入（QEMU 内可降级为鼠标 + 键盘模拟）
- [ ] 退出标准：三模态事件融合生成 UserIntent 可观察

### 8.5 S6.4：性能与稳定性

- [ ] SceneGraphBudgetEnforcer
- [ ] 脏矩形局部重绘
- [ ] 性能指标导出（外交工具可访问）
- [ ] 长稳测试（24h 持续运行无内存泄漏 / 帧率不下降）
- [ ] 退出标准：1080p 中等场景稳定 60 FPS

### 8.6 S6.5（远期）：硬件加速 + 3D 虚拟人

- [ ] Renderer trait GPU backend 实现（virtio-gpu `VIRTIO_GPU_CMD_CTX_CMD` 3D 模式）
- [ ] AvatarBackend 3D 实现（VRoid / 自研）
- [ ] 退出标准：4K 分辨率稳定 60 FPS + 复杂 3D 虚拟人无掉帧

---

## 9. 风险与缓解

| # | 风险 | 影响 | 缓解 |
|---|------|------|------|
| **R1** | tiny-skia 复杂效果（模糊 / SVG filter）不支持 | 功能缺失 | 复杂效果手写软件实现 / 降级到无效果版本 / 后期 GPU 加速补 |
| **R2** | DynamicUIGenerator LLM 生成 UI 不稳定 / 卡顿 | 体验差 | UI 模板约束生成空间（不让 LLM 自由发挥）+ 本地缓存模板结果 + 降级到本地规则生成 |
| **R3** | 软件光栅化 1080p 单帧超过 12ms | 掉帧 | SceneGraphBudgetEnforcer 自动降级 + 脏矩形局部重绘 |
| **R4** | 虚拟人 2D 起步体验不够"AI 助理感" | 与原始构想偏差 | 重点打磨状态机与微交互（注视跟随 / 表情跟随语义）|
| **R5** | 三模态输入融合的歧义场景 | 误操作 | L4/L5 操作强制双模态确认 + 显示意图提示 + 撤销时限 |
| **R6** | 显示服务单点故障 | 全屏黑屏 | DisplayService 监督树策略（消费 P4 freeze/thaw + S3 监督树）：崩溃自动重启 + 兜底画面 |
| **R7** | 个人开发者时间线过长 | 进度失控 | S6.0 优先跑通"能显示一张图"，后续阶段允许顺延但不允许砍验证项 |

---

## 10. 与其他文档的对齐

| 对齐项 | 本文档位置 | 对应文档 |
|--------|----------|---------|
| Framebuffer + Device capability | §1.1 | [Doc 01 §3 Capability 模型](01-capability-agent-permission-model.md) |
| 显示服务 IPC 接口（SubmitPanel） | §3 | [Doc 03 §3 IPC 消息格式](03-ipc-message-and-single-copy-path.md) |
| 外交工具 / 显示服务解耦（订阅 ontology 而非直连）| §3.3 | [Doc 04 §6 安全网关](04-diplomat-channel-architecture.md) |
| 五级权限 + L4/L5 确认弹窗 | §5.3 + §6.3 | [Doc 01 §5.1 风险操作确认](01-capability-agent-permission-model.md) |
| virtio-gpu 用户态驱动 | §3 | [需求目标 §Phase 6 显示驱动](../需求目标.md) |
| S6 阶段拆分 | §8 | [Doc 06 §8 S6 范围](06-system-services-roadmap.md) |
| 监督树 + freeze 原语 | §9 R6 | [Doc 02 §5.4 监督树](02-userspace-abi-and-process-model.md) |

---

## 11. 开放 TBD（不阻塞 S6 启动）

| TBD | 决策点 | 何时收敛 |
|-----|--------|---------|
| 字体源（首期）| 系统内置等宽 + sans-serif 各 1 款 / 接受用户导入 | S6.0 启动前 |
| 默认虚拟人形象 | 极简几何人形 vs 抽象符号（AI 助理标识）| S6.1 启动前 |
| 动画曲线库 | 自研极简 / 集成现有曲线库 | S6.1 启动前 |
| 直角 vs 圆角设计语言 | 设计 token 决策（参考行业规范）| S6.0 启动前 |
| 国际化（多语言）首期范围 | 仅中文 / 仅英文 / 中英文 | S6.2 启动前 |

---

## 12. 参考 (References)

- 原始构想对话 [`../原始思路.md`](../原始思路.md)（"图形化界面 + AI 助理 + 类虚拟现实"）
- 鸿蒙 / visionOS 空间外壳设计（参考思路，**不模仿交互**）
- tiny-skia（[https://github.com/RazrFalcon/tiny-skia](https://github.com/RazrFalcon/tiny-skia)）
- Live2D 动画原理（关键帧 + 骨骼变形）
- Tauri / Iced / Druid 等 Rust GUI 框架的合成器参考
- Doc 06 §8.2 此前登记的"GUI 显示栈选型 TBD"