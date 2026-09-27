# P6 03-planning — 任务切分（落入 task.json plan 数组）

> 完成时间：2026-09-27
> 输入：02-architecture 依赖图 + 分组
> 输出：P5-T1~T7 + P6-A1~L2 共 35 任务切分；待用户审视后落入 task.json

---

## 1. 切分总览

| 阶段 | sub-phase | 任务数 | 估计总窗口 |
|---|---|---|---|
| P5 外交工具与 Agent 雏形 | T1~T7 | 7 | 6~10 |
| P6-A 内存自适应（GAP-4） | A1~A4 | 4 | 3~5 |
| P6-B SMP | B1~B4 | 4 | 4~6 |
| P6-C 存储栈（Doc 08 DECIDED） | C1~C4 | 4 | 5~8 |
| P6-D IOMMU + DMA 安全 | D1~D2 | 2 | 2~3 |
| P6-E 显示驱动 | E1~E2 | 2 | 2~3 |
| P6-F USB 主机栈（GAP-3 上半） | F1~F3 | 3 | 4~6 |
| P6-G 蓝牙栈（GAP-3 下半） | G1~G2 | 2 | 3~5 |
| P6-H 音频栈（GAP-2） | H1~H2 | 2 | 2~3 |
| P6-I 本地推理运行时 | I1~I2 | 2 | 2~4 |
| P6-J 跨 OS 外交协议（Doc 05） | J1~J3 | 3 | 3~5 |
| P6-K 安全强化（分散） | K1~K4 | 4 | 3~5 |
| P6-L 用户会话与登录体系（GAP-1） | L1~L2 | 2 | 1~2 |
| **合计** | | **41** | **40~65 窗口** |

> 注：单 sub-phase 内任务串行；跨 sub-phase 按 02-architecture 批次并行；总日历时间 ≈ 串行关键路径（10~15 窗口）+ 并行批次合流时间。

## 2. P5 任务切分（外交工具与 Agent 雏形，需求目标 §三 Phase 5）

### P5-T1 用户态 virtio-net PCI 驱动（smoltcp 集成）
- 前置：P4.5 PCI BAR 映射 + Notification 中断路由
- 交付：user/diplomat-net（用户态 ELF，独立 crate）+ virtio-net PCI 驱动（MMIO 配置 + virtqueue 协商）+ smoltcp 集成（no_std 友好，已存在）+ 用户态 TCP/IP socket 接口
- 验证：QEMU 真机 + DHCP 拿 IP + ping 192.168.x.x

### P5-T2 外交工具根进程（唯一 NIC cap 持有者）
- 前置：P5-T1
- 交付：user/diplomat 进程（pid=2，init=1 已占）+ 持有唯一 BlockDevice(NIC) cap + 启动时拒绝其他进程的 NIC cap 申请 + 接收 Agent 网络请求 IPC
- 验证：构造尝试申请 NIC cap 的进程 → 期望 -1 E_INVALID_CAP；外交工具成功 ping 外网

### P5-T3 API Channel（最小 HTTPS 客户端 + 域名白名单）
- 前置：P5-T2
- 交付：Channel Registry（编译期静态）+ API Channel 最小子集（HTTPS 出站、固定域名白名单、请求/响应 schema、超时、限流、审计摘要）+ rustls 集成（no_std 友好，已存在）
- 验证：外交工具向 api.openai.com 发起 HTTPS 请求 → Agent 收到响应

### P5-T4 Security Gateway 最小闭环 + 审计摘要
- 前置：P5-T3
- 交付：出站脱敏规则集（内置）+ 大小限制 + 凭证脱敏 + 审计事件流（不经过内核）
- 验证：构造尝试发送含凭证的请求 → 期望脱敏后转发 + 审计事件含脱敏记录

### P5-T5 Agent → 外交工具 → 远端 API 端到端 demo
- 前置：P5-T4
- 交付：user/agent-demo 进程（Agent 雏形）+ 通过 IPC 向外交工具发网络请求 + 端到端流程演示（Agent → IPC → 外交工具 → smoltcp → virtio-net → 外部 API）
- 验证：QEMU 真机 + 真实外网调用（如 OpenAI/Anthropic）→ 收到响应

### P5-T6 唯一网络出口不变量验证（架构性保证）
- 前置：P5-T5
- 交付：smoke 测试构造尝试绕过外交工具（直接申请 NIC cap / 直接 DMA NIC BAR）→ 期望全部拒绝并审计
- 验证：违反尝试 100% 拒绝 + 审计事件计数 == 尝试次数

### P5-T7 L4 高风险串口确认通道（中间态）
- 前置：P5-T3
- 交付：L4 高风险操作走串口交互确认（默认拒绝 + 超时即拒绝）+ Doc 01 §5.1 实现
- 验证：构造 L4 操作请求 → 期望串口提示 + 默认拒绝 + 超时即拒绝

## 3. P6-A 任务切分（内存自适应，源自 GAP-4）

### P6-A1 E820 真实探测恢复（解 INT 15h 挂起）
- 前置：P2-T1 Memory Map 解析
- 交付：build_disk.py 移除 stage2 16-bit 实模式硬编码 QEMU 内存布局 → 恢复 BIOS INT 15h AX=E820h 调用（解 stage2 挂起根因，已识别）；保留 UEFI GetMemoryMap 路径为 fallback
- 验证：QEMU 真机 E820 entries 完整 + SeaBIOS 一致

### P6-A2 buddy 分配器替换位图（解除 128MB 上限）
- 前置：P6-A1
- 交付：kernel/src/buddy.rs（伙伴分配器，order=11 = 2^11 = 2048 页 = 8MB 最大块）+ 替换 page_frame.rs 位图分配器；迁移 alloc/free 接口保持语义
- 验证：宿主 20+ 测试 + QEMU 真机 256MB 内存全绿 + 原有 11 套 smoke 全绿

### P6-A3 高端内存映射（>4GiB 直通）
- 前置：P6-A2
- 交付：恒等映射扩展到 0-64GiB（覆盖 32G/64G 真机上限）+ 高端页表 per-CPU 临时映射 + Doc 02 §3.1 高端映射方案落地
- 验证：QEMU 真机 4GB+ 内存可用 + FR8 归零

### P6-A4 32G/64G 真机验收
- 前置：P6-A3
- 交付：真机测试报告（32G + 64G 各一份）+ QEMU -m 32G/64G 全套 smoke PASS
- 验证：所有原有 11 套 smoke 在 32G 配置下全绿

## 4. P6-B 任务切分（SMP）

### P6-B1 AP 启动（INIT-SIPI-SIPI）+ per-CPU 栈/GS base
- 前置：P6-A4
- 交付：AP 启动序列（INIT-SIPI-SIPI 协议）+ per-CPU 栈 + IA32_KERNEL_GS_BASE 配置 + BSP/AP 同步原语
- 验证：QEMU -smp 4 真机 + 4 CPU 同时打印标记

### P6-B2 per-CPU runqueue + IPI 调度
- 前置：P6-B1
- 交付：per-CPU runqueue + IPI 跨核调度（reschedule IPI + TLB shootdown IPI）+ 跨核迁移策略
- 验证：QEMU -smp 4 + 8 worker 真机交错

### P6-B3 自适应 SpinLock + 锁升级验证
- 前置：P6-B2
- 交付：自适应 SpinLock（短忙等 + 长 MCS queue lock）+ 锁顺序静态分析 + 升级验证（单核→多核回归）
- 验证：QEMU -smp 4 + 锁竞争真机（stress smoke）+ 全套原有 smoke 全绿

### P6-B4 跨核 IPC + 基准（p50/p95/p99）
- 前置：P6-B3
- 交付：跨核 IPC 真机 + NFR2 基准（p50/p95/p99，对照 P4-T14 单核基线）+ 报告落 logs/bench-{ts}.txt
- 验证：基准 < 2× 单核延迟（跨核开销预算）

## 5. P6-C 任务切分（存储栈，Doc 08 DECIDED）

### P6-C1 virtio-blk 用户态驱动
- 前置：P4.5 PCI BAR 映射 + Notification 中断路由
- 交付：user/storage-blk 用户态 ELF + virtio-blk PCI 驱动（MMIO 配置 + virtqueue）+ 512B sector 读写接口（Doc 08 §3）
- 验证：QEMU 真机 + virtio-blk-pci 设备 + 读写往返（loopback）

### P6-C2 FS 元数据：extent + summary slot + WAL journal
- 前置：P6-C1
- 交付：kernel/src/fs.rs（extent-based 类似 ext4 + inode 扩展区 + 32B BLAKE3 + 8B 索引 cap summary slot + WAL journal）+ Doc 08 §4 落地
- 验证：宿主 30+ 测试 + QEMU 真机 + FS 挂载 + 文件读写往返

### P6-C3 vectorfsd 单例 + IPC 协议 + 模型加载
- 前置：P6-C2
- 交付：user/vectorfsd（pid=2，init=1 + storage=3 已占）+ Doc 08 §5 IPC 协议（端点 cptr init 铸 + grant）+ HNSW 索引 + 模型加载（bge-small）+ 摘要计算（Doc 08 §5.3 写时同步）
- 验证：QEMU 真机 + 写文件触发摘要计算 + vectorfsd Endpoint 检索返回 top-K

### P6-C4 文件系统 syscall 接线 + FileSystem cap
- 前置：P6-C3
- 交付：abi 增 fs_open/fs_read/fs_write/fs_close 等 syscall（Doc 02 §4 补 fs_*）+ kstate 接 FileSystem cap 类型 + 应用走 syscall 直访（无需 FS 服务）
- 验证：QEMU 真机 + user/crasher 改造读写真实块设备

## 6. P6-D 任务切分（IOMMU + DMA 安全）

### P6-D1 IOMMU 抽象（Intel VT-d 探测 + 二级页表）
- 前置：P6-B3
- 交付：kernel/src/iommu.rs（Intel VT-d 探测 + DMAR 表解析 + 二级地址翻译）+ Doc 06 §13 硬件信任根埋点
- 验证：QEMU -device intel-iommu 真机 + DMAR 表解析 + 二级页表往返

### P6-D2 用户态驱动 DMA 授权 API（DMA 区域 cap）
- 前置：P6-D1
- 交付：abi 增 dma_region_alloc/release syscall + DmaRegion cap 类型（限定 VA 范围 + length）+ 驱动申请 cap → 内核配 IOMMU 二级页表 → 用户态 DMA 经此 cap 受限
- 验证：构造用户态驱动越界 DMA → IOMMU 拦截 + 审计事件

## 7. P6-E 任务切分（显示驱动）

### P6-E1 virtio-gpu 用户态驱动 + framebuffer cap
- 前置：P6-B3 + P6-D2
- 交付：user/display 用户态 ELF + virtio-gpu PCI 驱动（2D 模式 + resource create/attach）+ framebuffer cap 类型
- 验证：QEMU 真机 + virtio-gpu-pci 设备 + framebuffer 写入 → QMP screendump 看到画面

### P6-E2 显示栈 PoC 接入真后端（脏矩形 + 渐变 shader）
- 前置：P6-E1
- 交付：display crate 接入 virtio-gpu 后端（替换 tiny-skia 软光栅）+ 脏矩形 + 渐变 shader（Doc 07 §8）
- 验证：1080p 整帧 < 16.6ms（真机）+ S6 PoC 视觉回归

## 8. P6-F 任务切分（USB 主机栈，源自 GAP-3 上半）

### P6-F1 xHCI 驱动（USB host controller）
- 前置：P6-D2
- 交付：user/usb-xhci 用户态 ELF + xHCI 驱动（仅必要寄存器：USBCMD/USBSTS/Doorbell + 集合 TRB 环）+ 中断走 Notification
- 验证：QEMU 真机 + xHCI 设备识别 + 设备描述符读取

### P6-F2 USB HID class 驱动（键鼠）
- 前置：P6-F1
- 交付：user/usb-hid class 驱动（中断传输 + boot protocol）+ InputService syscall 接线
- 验证：QEMU 真机 + USB 键鼠虚拟设备 + 输入事件转发用户态进程

### P6-F3 USB mass storage class（辅助）
- 前置：P6-F1（可与 F2 并行）
- 交付：user/usb-storage class 驱动（bulk-only transport）+ SCSI 命令集
- 验证：QEMU 真机 + USB U 盘 + 文件读写

## 9. P6-G 任务切分（蓝牙栈，源自 GAP-3 下半）

### P6-G1 HCI 传输层（USB/UART）
- 前置：P6-F1
- 交付：user/bt-hci（HCI USB transport，UART transport 延后）+ HCI 命令/事件/数据 三通道
- 验证：QEMU 真机 + USB 蓝牙适配器虚拟设备 + HCI Reset 命令响应

### P6-G2 L2CAP + profiles（A2DP/HID/GATT）
- 前置：P6-G1
- 交付：user/bt-l2cap（L2CAP 信道）+ A2DP（音频）/ HID（输入）/ GATT（低功耗）profile
- 验证：QEMU 真机 + BLE 设备 + GATT 服务发现

## 10. P6-H 任务切分（音频栈，源自 GAP-2）

### P6-H1 音频设备驱动（virtio-snd 优先 / HDA fallback）
- 前置：P6-D2
- 交付：user/audio 用户态 ELF + virtio-snd PCI 驱动（PCM stream + control queue）+ HDA 真机 fallback 路径
- 验证：QEMU 真机 + virtio-snd-pci 设备 + PCM 播放/采集

### P6-H2 AudioService + 接入 S6.3 语音闭环
- 前置：P6-H1 + S6.3 语音交互设计
- 交付：user/audio-service（AudioService 系统服务）+ 播放（TTS 通道）+ 采集（麦克风 → 外交工具 Realtime Channel STT 输入）+ 接入 S6.3
- 验证：语音"听+说"闭环真机演示

## 11. P6-I 任务切分（本地推理运行时）

### P6-I1 决策（Rust 推理栈 vs 用户态 libc shim）
- 前置：独立任务（早期启动避免阻塞）
- 交付：决策报告（候选对比）+ 推荐方案（按避坑指南 §5，倾向承认外交工具仅远程 API 代理）
- 验证：用户决策通过

### P6-I2 MVP 推理接入外交工具（脱机能力）
- 前置：P6-I1 + P5-T3
- 交付：MVP 推理（按 I1 决策路径）+ 外交工具切换远端/本地 + 决策记录入 logs/
- 验证：QEMU 真机 + 远端 API 调用 vs 本地推理（按 I1 决策路径执行）

## 12. P6-J 任务切分（跨 OS 外交协议，Doc 05）

### P6-J1 DID/VC 身份协议实现
- 前置：P5-T3 + P6-K4
- 交付：user/diplomat-did DID method（W3C 标准）+ VC 颁发/验证流程 + 跨 OS 身份认证
- 验证：QEMU 真机 + 两个 Synapse 实例跨 OS DID 认证往返

### P6-J2 行为契约 + 信誉系统
- 前置：P6-J1
- 交付：行为契约 schema + 信誉评分（本地 + 跨 OS 同步）+ Doc 05 §4 落地
- 验证：跨 OS 信誉查询往返

### P6-J3 外部 Agent 协议适配（Doc 05 §6）
- 前置：P6-J2
- 交付：A2A / MCP 等候选协议适配器（按决策路径）+ 互操作测试
- 验证：与外部 Agent 协议真机互通

## 13. P6-K 任务切分（安全强化，分散）

### P6-K1 KPTI（内核页表隔离）
- 前置：P6-B2
- 交付：用户页表内核区取消（替代 supervisor-only）+ entry trampoline 切页表 + Meltdown 攻击防护测试
- 验证：QEMU + 公开 Meltdown PoC 测试集 PASS

### P6-K2 W^X 强化 + 栈保护（canary）
- 前置：P6-B2
- 交付：内核 .text 只读 + 用户栈 canary + PT_GNU_STACK 检查
- 验证：QEMU + 栈溢出攻击 PoC 测试集 PASS

### P6-K3 同步 IPC 优先级继承（Doc 03 §7）
- 前置：P4-T7 IPC
- 交付：sched 增 priority_inherit（持锁线程继承等待者最高优先级）+ IPC 优先级反转测试
- 验证：QEMU + 优先级反转 stress smoke 全绿

### P6-K4 CryptoProvider 接入硬件信任根（TPM/TEE/后量子）
- 前置：Doc 06 §13 埋点（Phase 5 起即做）
- 交付：CryptoProvider trait 实现（软件 fallback + 硬件后端 stub）+ TPM 2.0 stub + 后量子 ML-KEM/ML-DSA 软件实现
- 验证：QEMU + 加密单元测试 + 性能 baseline

## 14. P6-L 任务切分（用户会话与登录体系，源自 GAP-1）

### P6-L1 会话决策点（capability 命名空间 vs 会话 token）
- 前置：与 S6 桌面（空间外壳）设计同步
- 交付：决策报告 + 双轨机制选型（capability 命名空间做隔离 + 会话 token 做认证）
- 验证：S6 桌面设计决策通过

### P6-L2 多用户会话 + 锁屏设计
- 前置：P6-L1
- 交付：多用户 capability 命名空间 + 会话 token 颁发 + 锁屏 UI（Doc 07 §6 联动）
- 验证：QEMU 真机 + 多用户切换 + 锁屏恢复

## 15. 落入 task.json 的策略

- 在 task.json plan 数组末尾追加 P5 + P6 phase 节点（先登记 phase 框架 + tasks 数组）
- 每 task 字段：id / name / status: pending / deliverables（高层） / verify（高层） / deps / estimate / actual_approach（暂留空，04-codegen 认领时细化）
- 不覆盖 task.json 现有 P0~P4 内容；不动 decision_log 既有条目
- README §3 P5/P6 行延后更新（待 task.json 落地后单写者写入）