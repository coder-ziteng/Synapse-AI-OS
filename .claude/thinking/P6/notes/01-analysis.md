# P6 01-analysis — 范围扫描与子主题清单

> 完成时间：2026-09-27
> 输入：需求目标.md §三 Phase 6 + docs/design/04-08 + task.json requirement_gaps + Decision Log 已登记 GAP-1~4
> 输出：P6 子主题清单（12 个 sub-phase）与 P5 衔接定位

---

## 1. P6 范围来源对账

| 来源 | 项 | 归入 sub-phase |
|---|---|---|
| 需求目标 §三 Phase 6 | SMP（AP 启动 + per-CPU runqueue + 锁升级验证） | P6-B |
| 需求目标 §三 Phase 6 | 存储栈（virtio-blk 用户态驱动 + 持久化） | P6-C |
| 需求目标 §三 Phase 6 | IOMMU（约束用户态驱动 DMA） | P6-D |
| 需求目标 §三 Phase 6 | 本地推理运行时（Rust 推理栈 vs 用户态 libc shim） | P6-I |
| 需求目标 §三 Phase 6 | 跨 OS 外交协议（DID/VC/行为契约/信誉） | P6-J |
| 需求目标 §三 Phase 6 | 硬件信任根（TPM/TEE/后量子密码，Doc 06 §13） | P6-K4 |
| 需求目标 §三 Phase 6 | 同步 IPC 优先级继承（Doc 03 §7） | P6-K3 |
| 需求目标 §三 Phase 6 | KPTI / W^X 强化 / 栈保护 | P6-K1/K2 |
| 需求目标 §三 + GAP-1 | 用户会话与登录体系（capability 命名空间 + 会话 token 决策点） | P6-L |
| 需求目标 §三 + GAP-2 | 音频栈（virtio-snd/HDA 用户态驱动 + 输入采集） | P6-H |
| 需求目标 §三 + GAP-3 | USB 主机栈（xHCI + class 驱动） | P6-F |
| 需求目标 §三 + GAP-3 | 蓝牙栈（HCI 传输 + L2CAP + profiles） | P6-G |
| 需求目标 §三 + GAP-4 | 内存自适应三件套（E820 + buddy + 高端映射） | P6-A |
| Doc 04 §5 + Doc 05 | 外交工具 Channel 完整化（5 类 Channel + 跨 OS 协议） | P5 + P6-J |
| Doc 06 §13 | 系统服务层硬件信任根埋点 | P6-K4 |
| Doc 07 §6 + S6.3 | 显示栈真实后端（virtio-gpu） | P6-E |
| Doc 08（DECIDED） | AI 原生 FS + vectorfsd + summary slot + 模型加载 | P6-C |
| S7 候选 | 外设服务层（AudioService/BluetoothService/UsbDeviceManager） | 依赖 P6-F/G/H，文档化至 S7；不在 P6 任务本体 |

## 2. P6 子主题清单（按依赖分批）

### P6-A 内存自适应（真机部署硬前置，源自 GAP-4）
**问题**：当前 MVP 三处硬限制——① boot.S E820 硬编码（build_disk.py 临时方案）② page_frame.rs MAX_FRAMES=32768（128MB 位图上限）③ 恒等映射仅 0-4GiB。
**目标**：32G/64G 真机即插即用，无需改代码。
**任务**：A1 E820 真实探测 → A2 buddy 替换位图 → A3 高端映射 → A4 真机验收。
**已有埋点**：memory_map.rs 解析器（256 条目容量）+ page_frame.rs 注释明示"留给后续 buddy"。

### P6-B SMP（内存自适应之后，源自 NFR3 + 锁升级需求）
**问题**：单核 BSP only（需求目标 §三 + requirements-review §2 NFR3）。
**目标**：AP 启动 + per-CPU 数据 + 跨核调度 + 自适应锁。
**任务**：B1 AP 启动（INIT-SIPI-SIPI）+ per-CPU 栈/GS base → B2 per-CPU runqueue + IPI 调度 → B3 自适应 SpinLock + 锁升级验证 → B4 跨核 IPC + 基准。
**前置**：P6-A（高端映射）+ P3-T6/T7 已有抢占模型。
**约束**：所有共享数据结构从 P3 起就 SpinLock + 关中断临界区（需求目标 NFR6），避免多核化时重写——已满足。

### P6-C 存储栈（接 P4.5 PCI 预研，Doc 08 已 DECIDED）
**问题**：virtio-blk stub 已在 P4.5 启动；FS/vectorfsd/AI Index 架构钉死（Doc 08 DECIDED）。
**目标**：用户态块设备驱动 + 持久化 FS + 语义索引可启动。
**任务**：C1 virtio-blk 用户态驱动 → C2 FS extent + summary slot + WAL journal → C3 vectorfsd 单例 + IPC 协议 → C4 FS syscall + FileSystem cap。
**前置**：P4.5 PCI BAR 映射 + 中断 Notification 路由（已就绪）+ P5 外交工具（间接，无外交工具则远端索引 sync 不可用，本地 OK）。
**里程碑**：S4 语义记忆依赖 P6-C 完成。

### P6-D IOMMU + DMA 安全（驱动前置）
**问题**：首期威胁模型声明"信任外交工具"——用户态驱动可经 DMA 改写任意内存。
**目标**：约束用户态驱动 DMA 范围，关闭这条攻击面。
**任务**：D1 IOMMU 抽象（Intel VT-d 探测 + 二级页表）→ D2 用户态 DMA 区域 cap。
**前置**：P6-B（SMP 后 IOMMU 中断路由多核化）。
**绑定**：P6-C1（virtio-blk DMA）/ P6-E1（virtio-gpu DMA）/ P6-F1（xHCI DMA）皆依赖 D2。

### P6-E 显示驱动（接 S6 显示栈 PoC）
**问题**：S6 PoC 用 CPU 软光栅（tiny-skia），1080p ~105ms，远超 16.6ms 预算。
**目标**：virtio-gpu 用户态驱动 + framebuffer cap，PoC 接入真后端。
**任务**：E1 virtio-gpu 用户态驱动 + framebuffer cap → E2 显示栈接入真后端（脏矩形 + 渐变 shader）。
**前置**：P6-B + P6-D（DMA 区域 cap）+ Doc 07 §5 显示栈架构。
**约束**：PoC 阶段（display crate 已有 19 宿主测全绿）已证明架构成立，E2 仅后端切换。

### P6-F USB 主机栈（源自 GAP-3 上半，工作量与内核本体相当）
**问题**：真机部署无 USB 主机栈，键鼠等物理外设不可接入。
**目标**：xHCI 用户态驱动 + USB class 驱动框架。
**任务**：F1 xHCI 驱动 → F2 USB HID class（键鼠）→ F3 USB mass storage class（辅助）。
**前置**：P6-D2（DMA 区域 cap，xHCI 需 DMA 描述符）。
**架构模式**：内核只做 MMIO/IRQ 转发 + capability 授权，驱动全在用户态（沿用项目惯例）。

### P6-G 蓝牙栈（源自 GAP-3 下半，依赖 P6-F）
**问题**：真机部署无蓝牙栈，BLE/A2DP/HID 设备不可接入。
**目标**：HCI 传输层 + L2CAP + profiles。
**任务**：G1 HCI 传输层（USB/UART）→ G2 L2CAP + profiles（A2DP/HID/GATT）。
**前置**：P6-F1（USB HCI 传输）。

### P6-H 音频栈（源自 GAP-2）
**问题**：S6.3 语音交互仅有融合层设计（VoiceEvent/STT 走外交工具 Realtime Channel），无 TTS 播放/系统声音/麦克风采集驱动；语音闭环（听+说）依赖此项。
**目标**：音频驱动 + AudioService。
**任务**：H1 音频设备驱动选型（virtio-snd 优先 / HDA fallback）→ H2 AudioService（播放/采集）+ 接入 S6.3 语音闭环。
**前置**：P6-D2（DMA 区域 cap）。
**决策点**：H1 设备驱动选型——virtio-snd QEMU 可调、HDA 真机主流——倾向先 virtio-snd PoC、再 HDA 真机。

### P6-I 本地推理运行时
**问题**：llama.cpp 依赖 libc/POSIX，本项目裸机用户态跑不起来（需求目标 §四 避坑指南 §5）。
**目标**：决策 + MVP 接入。
**任务**：I1 决策（Rust 推理栈 vs 用户态 libc shim）→ I2 MVP 推理接入外交工具（脱机能力）。
**前置**：I1 决策结果驱动 I2 路径。
**候选**：candle/burn no_std 裁剪版 / 极简 libc shim / 承认外交工具仅远程 AI API 代理。

### P6-J 跨 OS 外交协议（Doc 05 已 PROPOSED 完成）
**问题**：DID/VC/SD-JWT/信誉/行为契约/协议适配均为远期设计。
**目标**：跨 OS 身份 + 行为契约 + 外部协议适配。
**任务**：J1 DID/VC 身份协议实现 → J2 行为契约 + 信誉系统 → J3 外部 Agent 协议适配。
**前置**：P5 外交工具 Channel 完整化 + P6-K4 硬件信任根（VC 签名依赖 CryptoProvider）。

### P6-K 安全强化（分散子任务）
**任务**：
- K1 KPTI（内核页表隔离）—— 消除 Meltdown 类侧信道
- K2 W^X 强化 + 栈保护（canary）—— 用户态栈溢出检测
- K3 同步 IPC 优先级继承（Doc 03 §7）—— 消除优先级反转
- K4 CryptoProvider 接入硬件信任根（TPM/TEE/后量子 ML-KEM/ML-DSA）
**前置**：K1/K2 需 P6-B（SMP 后 TLB 刷新策略多核化）；K3 需 P4-T7 IPC 已完成；K4 需 Doc 06 §13 CryptoProvider 埋点（Phase 5 起即做）。

### P6-L 用户会话与登录体系（源自 GAP-1 决策点）
**问题**：单用户 capability 模型无传统登录/账户/会话隔离/锁屏；多用户产品形态无着落。
**目标**：S6 桌面决策点收敛 + 多用户会话 + 锁屏。
**任务**：L1 决策点（capability 命名空间 vs 会话 token 候选方案）→ L2 多用户会话 + 锁屏设计。
**前置**：L1 在 S6 桌面（空间外壳）设计时决策，不阻塞 P6 其他 sub-phase；L2 需 P6-L1 决策结果。

## 3. P5 衔接（外交工具与 Agent 雏形，需求目标 §三 Phase 5）

P6 多数 sub-phase 依赖 P5（外交工具）。P5 必须先于 P6-C/J 推进：
- P5-T1 用户态 virtio-net PCI 驱动（smoltcp 集成）
- P5-T2 外交工具根进程（唯一 NIC cap 持有者）
- P5-T3 API Channel（最小 HTTPS 客户端 + 域名白名单）
- P5-T4 Security Gateway 最小闭环 + 审计摘要
- P5-T5 Agent → 外交工具 → 远端 API 端到端 demo
- P5-T6 唯一网络出口不变量验证（构造绕过尝试，期望拒绝并审计）
- P5-T7 L4 高风险串口确认通道（默认拒绝 + 超时即拒绝兜底）

## 4. 关键依赖图（顶层视图）

```text
P4（已完成 7/14，T7+T10~T14 pending）
  ↓
P4.5（PCI 预研启动，Doc 08 DECIDED）
  ↓
P5 ──→ P6-J（跨 OS 协议，硬依赖外交工具）
  ↓
P6-A 内存自适应（真机部署硬前置）
  ↓
P6-B SMP（依赖 P6-A 高端映射）
  ↓ ↓ ↓
P6-D P6-E P6-C1（virtio-blk）
  ↓ ↓ ↓ ↓
P6-C2/C3/C4 P6-F（USB）→ P6-G（蓝牙）→ P6-H（音频）
  ↓
P6-I（本地推理，可并行）
  ↓
P6-K（安全强化，分散）
  ↓
P6-L（会话决策，与 S6 桌面联动）
```

## 5. 风险点登记

| 风险 | 影响 | 缓解 |
|---|---|---|
| P6 工作量与内核本体相当 | 个人开发者排期失控 | 按 sub-phase 串行认领，每 sub-phase 1~3 窗口；并行认领需保证文件零冲突 |
| Doc 08 已有 DECIDED 但实现期 TBD 未收 | P6-C 实现返工 | 在 P6-C 启动前做一次实现期 TBD 评审 |
| IOMMU 硬件兼容性 | P6-D 推迟 | QEMU 仅支持部分模拟，真机验证推迟到 P6-B 后 |
| virtio-gpu QEMU 与真机差异 | P6-E 真机问题 | E1 走 QEMU 路径，E2 真机验证推迟到 P6-A 真机后 |
| xHCI 规范庞大（1TB+）| P6-F 工作量爆炸 | 仅实现必要寄存器 + 集合 TRB 环；HID class 优先，audio class 等 P6-G2 |
| 蓝牙栈 HCI 传输选型 | P6-G1 设计不确定 | 选 USB HCI 优先（与 P6-F 共栈），UART HCI 延后 |
| 本地推理栈决策 | P6-I 路径分叉 | I1 决策节点；推荐承认外交工具仅远程 AI API 代理（避坑指南 §5）|
| 用户会话决策影响 S6 桌面 | P6-L 与 S6 冲突 | L1 决策点不阻塞 S6 自身推进；L2 跟随 S6 决策 |
| 多窗口并行认领 task.json plan 字段冲突 | 切分粒度不足 | sub-phase 已细分到任务粒度，每任务独立 deps 字段；多窗口防冲突按 rule.md 第 10 条 |