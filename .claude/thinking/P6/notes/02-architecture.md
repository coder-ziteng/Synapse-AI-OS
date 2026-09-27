# P6 02-architecture — 依赖图与分组

> 完成时间：2026-09-27
> 输入：01-analysis 子主题清单
> 输出：sub-phase 间的依赖图 + 串/并行分组 + 串行关键路径

---

## 1. sub-phase 依赖图（细化）

```text
P5-T1 (virtio-net) ─→ P5-T2 (diplomat root) ─→ P5-T3 (API Channel) ─→ P5-T4 (Security Gateway)
                                                                          ↓
                                          P5-T5 (Agent E2E demo) ←────────┤
                                          P5-T6 (unique NIC exit) ←───────┤
                                          P5-T7 (L4 serial confirm) ←─────┘
                                                                          ↓
                                          P6-J (跨 OS 协议)  ←─────────────┘
                                          (依赖 P5-T3 + P6-K4 CryptoProvider)

P6-A1 (E820) ─→ P6-A2 (buddy) ─→ P6-A3 (highmem) ─→ P6-A4 (真机验收)
                                                          ↓
P6-B1 (AP 启动) ←────────────────────────────────────────┤
       ↓
P6-B2 (per-CPU runqueue + IPI) ←──────────────────────────┤
       ↓
P6-B3 (adaptive spinlock) ←───────────────────────────────┤
       ↓
P6-B4 (cross-core IPC bench) ←────────────────────────────┘

P6-D1 (IOMMU 抽象) ←── P6-B3
       ↓
P6-D2 (DMA cap) ←─────────────────────────────────────────┐
       ↓                                                   ↓
P6-C1 (virtio-blk) ←── P4.5 PCI ←── P6-D2 ←── P6-B2      P6-E1 (virtio-gpu)
       ↓                                                   ↓
P6-C2 (FS extent+summaryslot+journal)                      P6-E2 (display 接入)
       ↓
P6-C3 (vectorfsd IPC + 模型) ←── P5-T3 (外交工具，远端 sync 用)
       ↓
P6-C4 (FS syscall + FileSystem cap) ←── P5-T2 (cap 体系已建)

P6-F1 (xHCI) ←── P6-D2
       ↓
P6-F2 (USB HID) ←── P6-F1
       ↓
P6-F3 (USB mass storage) ←── P6-F1 （可与 F2 并行）

P6-G1 (HCI 传输 USB) ←── P6-F1
       ↓
P6-G2 (L2CAP + profiles) ←── P6-G1

P6-H1 (音频驱动 virtio-snd/HDA) ←── P6-D2
       ↓
P6-H2 (AudioService + 语音闭环) ←── P6-H1 + S6.3 (语音交互)

P6-I1 (推理决策) —— 独立 ——→ P6-I2 (MVP 接入)

P6-K1 (KPTI) ←── P6-B2 （TLB 刷新策略多核化）
P6-K2 (W^X + canary) ←── P6-B2 （独立任务，可与 K1 并行）
P6-K3 (IPC 优先级继承) ←── P4-T7 IPC
P6-K4 (CryptoProvider 硬件信任根) ←── Doc 06 §13 埋点（Phase 5 起做）

P6-L1 (会话决策) —— 独立 ——→ P6-L2 (多用户 + 锁屏)
```

## 2. 串行关键路径（最长链）

```text
P4.5 → P6-A1 → P6-A2 → P6-A3 → P6-A4
                            ↓
                          P6-B1 → P6-B2 → P6-B3
                                          ↓ ↓ ↓
                                   P6-D1 → P6-D2 → P6-C1 → P6-C2 → P6-C3 → P6-C4
                                                              ↓
                                                           P6-J ←── P5-T3
```

关键路径瓶颈：**P6-A 内存自适应**。所有真机部署 + 多核化 + 高端硬件依赖此。
故 P6 的"快速胜利"切入点：**P6-D IOMMU** 可在 P6-B 后立即并行（独立于 P6-A 之外，硬件兼容 OK 后），给同步 IPC 优先级继承（P6-K3，已独立）让路。

## 3. 并行分组（sub-phase 可同时推进的窗口集合）

| 批次 | sub-phase | 共同前置 | 可并行窗口数 |
|---|---|---|---|
| 批次 1 | P5-T1~T7 | P4.5 PCI | 1~2 |
| 批次 2 | P6-A1, P6-K3 | P4 (T7 IPC 已完成) | 1~2 |
| 批次 3 | P6-A2, P6-I1 | P6-A1 | 1~2 |
| 批次 4 | P6-A3, P6-A4 | P6-A2 | 1 |
| 批次 5 | P6-B1, P6-L1 | P6-A4 | 1~2 |
| 批次 6 | P6-B2, P6-D1, P6-K1/K2 | P6-B1 | 1~3 |
| 批次 7 | P6-B3, P6-B4 | P6-B2 | 1~2 |
| 批次 8 | P6-D2, P6-E1, P6-F1, P6-H1 | P6-B3 + P6-D1 | 2~4 |
| 批次 9 | P6-C1, P6-E2, P6-F2/F3, P6-H2, P6-G1 | 批次 8 | 2~4 |
| 批次 10 | P6-C2/C3/C4, P6-G2, P6-I2, P6-J1/J2/J3, P6-K4, P6-L2 | 批次 9 | 2~4 |
| 批次 11 | P6-I1 决策 → P6-I2 MVP 接入 | P6-I1 已决 | 1 |

> 注：每批次同时推进窗口数受 rule.md "并发限 6"约束，但 P6 单 sub-phase 工作量常达 1~3 窗口，实际并行窗口数低于并发限。

## 4. 串/并行取舍理由

- **P6-A 串行**：内存三件套一气呵成，buddy 替换位图后所有上层（mmap/伙伴分配/回收）接口语义变化，硬性串行
- **P6-B 串行**：SMP 子任务依赖链紧（AP 启动 → runqueue → 自适应锁 → 跨核 IPC），锁升级验证是 B3 的核心交付物
- **P6-D 与 P6-B3 并行**：IOMMU 抽象可不依赖自适应锁（独立的硬件抽象层），但 D2（DMA cap）需 D1 + B3 双前置
- **P6-C 与 P6-E/F/H 并行**：存储/显示/USB/音频都是独立用户态驱动栈，virtio-blk/gpu/xHCI/HDA 共用 P6-D2 DMA cap 后即可各自推进
- **P6-G（蓝牙）串行**：HCI 传输选 USB 后可与 P6-F2/F3 共栈，节省工作量
- **P6-I（推理）独立**：与硬件栈解耦，但 MVP 接入需 P5 外交工具（远端推理切换脱机时）
- **P6-K 分散**：K1/K2/K3/K4 分别在不同批次推进，避免安全强化集中带来回归风险
- **P6-L（会话）与 S6 联动**：L1 在 S6 桌面设计时决策，不阻塞 P6 主体推进

## 5. 关键决策点（预先标识）

| 决策点 | 触发 sub-phase | 候选方案 | 推荐 |
|---|---|---|---|
| 内存分配器选型 | P6-A2 | buddy (Linux 风格) / slab / bitmap-buddy hybrid | buddy（Linux 经典，已广泛验证） |
| IOMMU 探测优先级 | P6-D1 | Intel VT-d 优先 / AMD-Vi 同步 | Intel VT-d 优先（QEMU 支持更稳） |
| 块设备驱动选型 | P6-C1 | virtio-blk / AHCI / NVMe | virtio-blk 优先（QEMU 调试）+ AHCI 真机 |
| 显示驱动后端 | P6-E1 | virtio-gpu / 纯 framebuffer mmap | virtio-gpu（已有 PoC 架构，Doc 07 DECIDED） |
| USB 主机栈策略 | P6-F1 | 仅必要寄存器 + 集合 TRB 环 / 完整 xHCI spec | 仅必要寄存器（控制/批量/中断传输）+ HID class 优先 |
| 音频驱动选型 | P6-H1 | virtio-snd / HDA | virtio-snd 优先（QEMU 可调）+ HDA 真机 fallback |
| 本地推理决策 | P6-I1 | Rust 推理栈 / libc shim + llama.cpp / 放弃本地推理 | 承认外交工具仅远程 AI API 代理（避坑指南 §5 推荐）|
| 跨 OS 身份协议 | P6-J1 | DID/VC + W3C 标准 / 自定义轻量协议 | DID/VC（W3C 标准，互操作性好） |
| 用户会话机制 | P6-L1 | capability 命名空间 / 会话 token / 双轨 | 双轨（capability 命名空间做隔离 + 会话 token 做认证）|
| KPTI 范围 | P6-K1 | 全量 / 仅关键路径 / 不做 | 仅关键路径（个人开发者资源有限，首期不全量做）|

## 6. 与现有设计的对齐

- **Doc 04 §5 Phase 5 MVP 范围约束**（仅 API Channel 最小子集）已纳入 P5-T3
- **Doc 05 跨 OS 协议**（远期骨架）已纳入 P6-J1/J2/J3
- **Doc 06 §13 CryptoProvider 埋点**已纳入 P6-K4
- **Doc 07 显示栈 + 空间外壳**（DECIDED）已纳入 P6-E 真后端 + S6 桌面
- **Doc 08 AI 原生 FS**（DECIDED）已纳入 P6-C
- **需求目标 §三 Phase 5 7 项**已纳入 P5-T1~T7
- **需求目标 §三 Phase 6 11 项**已纳入 P6-A~L 共 28 任务
- **task.json requirement_gaps GAP-1~4**已纳入 P6-L/A/F-H（2026-09-27 登记）
- **需求目标.md Risk Register R13~R16**已纳入 P6-L/R14/R15/R16

## 7. 下一阶段交付（03-planning）

- 把 P5-T1~T7 + P6-A1~L2 共 35 任务落入 task.json plan 数组（先登记 sub-phase ID/名称/deliverables/verify/deps/estimate 框架；actual_approach 在 04-codegen 阶段该任务被认领时细化）
- decision_log 条目登记 P6 设计启动 + 切分草案 + 关键决策点
- README §3 进度表 P5/P6 行更新（细化到 35 任务）；待用户审视 task.json 后落地