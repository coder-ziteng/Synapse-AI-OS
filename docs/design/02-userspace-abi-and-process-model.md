# 设计文档 02：用户态 ABI 与进程模型

> 状态：DRAFT / 设计细化（含监督树与重启策略）
> 关联需求：FR5 用户态加载、FR10 进程冻结与监督原语
> 关联里程碑：Phase 4（用户态与 IPC）；监督树由系统服务层 S3 落地
> 最后更新：2026-09-25

---

## 0. 文档目的

定义 Synapse 用户态程序的**编译目标、加载方式、syscall 约定、进程/线程模型、地址空间布局**。这是"能加载并运行第一个用户态进程"（FR5）的前置设计，Phase 4 开工前必须收敛。

> ⚠️ 本文档为骨架，各章节列出**待决策问题（TBD）**。

---

## 1. 用户态编译目标（Target）

### 1.1 自定义 target json
裸机用户态不能用 `x86_64-unknown-linux-gnu`（那假设了 Linux ABI）。需要自定义 target，例如 `x86_64-synapse-user.json`：

```jsonc
{
  "llvm-target": "x86_64-unknown-none",
  "data-layout": "…",                 // TBD：对齐内核 target
  "arch": "x86_64",
  "target-endian": "little",
  "pointer-width": "64",
  "os": "none",
  "panic-strategy": "abort",          // 用户态 panic 直接 abort
  "disable-redzone": true,            // 裸机惯例
  "features": "…",                    // TBD：与内核一致，避免 ABI 漂移
  "linker-flavor": "ld.lld",
  "relocation-model": "static"        // 首期静态，见 §3
}
```

### 1.2 待决策
- [ ] 用户态是否允许 `std`？（首期建议 `no_std` + 极小 runtime，避免引入 libc）
- [ ] 是否提供极简 libc shim（影响 Phase 5 llama.cpp 可行性，见需求"AI 原生收敛"）

---

## 2. ELF 加载器

### 2.1 范围
- 首期**只支持静态 ELF**（`ET_EXEC` 或 `ET_DYN` PIE），**砍掉动态链接器**（ld.so）。
- 解析 `PT_LOAD` 段，按 `p_vaddr` 映射到用户地址空间，设置权限（R/W/X）。
- 入口点 `e_entry` 作为初始 RIP。

### 2.2 待决策
- [ ] 是否支持 PIE + 重定位（`R_X86_64_RELATIVE`）？还是固定加载地址即可？
- [ ] ELF 从哪里来（initramfs，见 §6）？
- [ ] 加载失败 / 段越界的错误处理路径

---

## 3. 地址空间布局

### 3.1 用户地址空间（示意，TBD 具体数值）
```
0x0000_0000_0000_0000  ┌─────────────────┐
                       │  (保留 / 空指针陷阱)│
0x0000_0000_0040_0000  ├─────────────────┤  ← 默认代码加载基址
                       │  .text / .rodata │
                       ├─────────────────┤
                       │  .data / .bss    │
                       ├─────────────────┤
                       │  heap (向下? TBD) │
                       │        ↓         │
                       │        ↑         │
                       │  stack           │
0x0000_7FFF_FFFF_F000  ├─────────────────┤
                       │  (内核映射区，NX，│
                       │   Ring3 不可访问) │
0xFFFF_FFFF_FFFF_FFFF  └─────────────────┘
```

### 3.2 内核映射隔离
- 内核高半映射区在用户页表中**标记为不可访问（present=0 或 NX + supervisor）**。
- 首期**不做 KPTI**（Meltdown 缓解），但在威胁模型中记为已知风险。

### 3.3 用户内存区域管理（VMA-like）
按需分页需要一张"哪些区间合法、权限如何"的表：

```rust
pub struct UserMemoryRegion {
    pub start: VirtAddr,   // 页对齐
    pub end: VirtAddr,
    pub flags: RegionFlags, // READ | WRITE | EXEC | GROWABLE(stack/heap)
    pub kind: RegionKind,   // Code | Data | Stack | Heap | Mapped(device)
}
```
- 缺页处理：命中合法 region → 分配页帧并映射；未命中 → SIGSEGV 等价物（杀进程）。

---

## 4. 系统调用 ABI

### 4.1 调用约定（TBD，候选 x86_64 System V 风格）
| 寄存器 | 用途 |
|--------|------|
| `rax` | syscall 号 |
| `rdi, rsi, rdx, r10, r8, r9` | 参数 1~6 |
| `rax`（返回） | 返回值 / 负错误码 |

- 指令：`syscall`（需在 Phase 4 配置 `MSR_LSTAR` / `MSR_STAR` / `MSR_FMASK`）。
- 内核栈切换：通过 `MSR_GS_BASE` + TSS.RSP0 定位当前线程内核栈。

### 4.2 syscall 号分配表（草案）
| 号 | 名称 | 说明 |
|----|------|------|
| 0 | `ipc_send` | 见文档 03 |
| 1 | `ipc_recv` | |
| 2 | `ipc_reply` | |
| 3 | `cap_invoke` | 通用能力调用 |
| 4 | `yield` | 主动让出 CPU |
| 5 | `process_spawn` | 见 §5 |
| 6 | `gettime` | 时钟服务，见 §7 |
| 7 | `process_freeze` | *(FR10)* 冻结进程（持有 PROCESS_ADMIN capability 才能调用）|
| 8 | `process_thaw` | *(FR10)* 解冻进程 |
| … | TBD | |

### 4.3 待决策
- [ ] 错误码约定（复用 Linux errno 数值？还是自定义）
- [ ] syscall 是否可重启（被信号/抢占打断后）

---

## 5. 进程 / 线程模型

### 5.1 定义
- **进程（Process / AddressSpace）** = 一套用户页表 + CapTable + `agent_id`。
- **线程（Thread）** = 调度实体，隶属于某进程，共享地址空间。
- 首期：**单进程内可多线程**，进程间强隔离。

### 5.2 spawn 语义
- 无 `fork`（不复制地址空间）。只有 `spawn(elf, args, caps_to_grant)`：从 ELF 新建进程。
- **TBD**：spawn 时父进程如何向子进程授予初始能力集？

### 5.3 进程生命周期与"收尸"
```
Created → Running → (Blocked) → Exited/Faulted → Reaped
```
- 进程崩溃（段错误 / panic / 非法 syscall）→ 内核标记 Faulted。
- 父进程通过 **death notification**（类似 `waitpid` 或向父的 endpoint 发一条信号）得知子进程终止。
- **TBD**：僵尸进程回收时机、资源（页帧 / cap）释放顺序。

### 5.4 监督树与重启策略 *(原始构想新增)*

原始构想：L3 治理层"自愈机制"——一个 Agent 崩溃不影响整体。

**模型**：用户态监督树（系统服务层 S3 落地），**内核只提供原语**：

| 内核原语 | 用途 |
|---------|------|
| `death_notification`（§5.3） | 子进程终止事件通知父进程 |
| `process_freeze/thaw` syscall（§4.2）| 行为围栏冻结 / 解冻进程 |
| 资源核算（FR8） | 监督器判定"该 Agent 是否滥用资源" |
| 审计事件（FR9） | 监督决策的输入数据 |

**监督器（用户态进程）**的职责：

1. 启动子进程时声明 `RestartPolicy`：
   ```rust
   pub struct RestartPolicy {
       pub max_restarts: u32,           // 在 window_secs 内最多重启次数
       pub window_secs: u32,
       pub backoff: BackoffStrategy,    // Linear | Exponential
       pub on_exhausted: ExhaustedAction, // Degrade | Freeze | AlertUser
       pub preserve_state: bool,        // 是否保留状态快照（自进化先决条件）
   }
   ```
2. 收到 death notification → 按策略决定：重启 / 降级 / 冻结 / 通知用户。
3. 重启次数超限 → `ExhaustedAction` 触发，避免"反复崩溃风暴"。

**冷启动默认策略**（`init` 进程对每个必需服务声明）：
- 外交工具：`max_restarts=3 / 10s`，exhausted → 通知用户（核心服务）；
- 用户态 Agent：`max_restarts=10 / 60s`，exhausted → 冻结 + 通知主 Agent（业务降级）。

**重要约束**：
- 监督树是**用户态**模型，内核不强制（避免内核被绑死策略）；但内核保证原语可用。
- 自进化系统（系统服务层 S4）复用此监督树 + `preserve_state` 机制作为"策略回滚"的载体。
- **TBD**：监督器自身崩溃如何处理？候选：每个监督器被更高层监督器看管（递归）；或 kernel watchdog 监督 init。

---

## 6. initramfs 与启动流程

### 6.1 initramfs 格式
- **TBD**：cpio（newc）vs ustar tar vs 自定义扁平格式。
- 由 bootloader 或内核早期加载到内存，作为一个 MemoryRegion 暴露。
- 内核启动 → 挂载 initramfs → 加载 `/init`（外交工具根进程）→ 移交控制权。

### 6.2 启动链
```
bootloader → kernel_main → mm/sched/ipc init
          → 解析 initramfs → spawn init(Root Agent)
          → init 通过 IPC 拉起外交工具 / 其他 Agent
```

---

## 7. 时钟与时间服务

- `gettime` syscall：暴露单调时钟（TSC 校准后）+ 墙钟（RTC，若可用）。
- **TBD**：TSC 校准方式（PIT / APIC timer 反推）、时间精度、是否暴露给用户态直接读 TSC（`rdtsc` 权限）。

---

## 8. 待决策清单（Phase 4 前必须收敛）

- [ ] 用户态 target json 完整字段（与内核 data-layout / features 对齐）
- [ ] 是否提供 `std` / libc shim
- [ ] 静态 ELF 加载基址与是否支持 PIE
- [ ] syscall 号表最终版 + 错误码约定
- [ ] initramfs 打包格式
- [ ] spawn 时的初始能力授予方式
- [ ] 进程崩溃的 death notification 机制
- [ ] gettime 的时钟源与校准
- [ ] 监督器自身崩溃的兜底（kernel watchdog 还是递归监督？）
- [ ] `RestartPolicy` 是否在 spawn syscall 内核入参暴露给进程，还是完全由用户态监督器解析配置
- [ ] 进程冻结时其持有的 capability 是否同步冻结（防止持有者调用已冻结对象）

---

## 9. 参考资料

- Philipp Oppermann《Writing an OS in Rust》用户态章节
- x86_64 `syscall`/`sysret` ABI（AMD64 Architecture Programmer's Manual Vol.2）
- ELF Specification（System V ABI）
- seL4 / Redox 进程模型
