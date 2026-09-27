# 设计文档 08：AI 原生文件系统与语义索引 (AI-Native FS & Semantic Index)

> 状态：**DECIDED**（架构与 IPC 协议已敲死，剩余 TBD 为实现期局部决策）
> 关联需求：[需求目标 §三之六 Phase 6 存储栈](../需求目标.md)、[设计文档 06 §6.4 S4 记忆与自进化](06-system-services-roadmap.md)
> 关联里程碑：**P4.5 PCI 枚举 → P6 存储栈 → S4 语义记忆**
> 前置依赖：内核 P4（IPC + cap + 用户态进程）/ P4.5（PCI 用户态化）/ Doc 03（IPC 单拷贝）
> 最后更新：2026-09-27

---

## DECIDED 决策汇总（敲死表）

| 决策项 | DECIDED 方案 | 章节 |
|--------|-------------|------|
| **块设备接口粒度** | 512B sector（统一 trait，与 QEMU/virtio/AHCI/NVMe 一致） | §3 |
| **块设备驱动后端** | virtio-blk 优先（QEMU 调试） + AHCI/NVMe 真机（P6 落地） | §3.1 |
| **FS 元数据布局** | extent-based（类似 ext4）+ inode 扩展区 + WAL journal | §4 |
| **AI 钩子位置** | inode 末尾预留 **summary slot**（32B BLAKE3 hash + 8B 索引 cap 指针）；写文件时同步落盘 | §4.2 |
| **索引服务进程** | userspace 单例 `vectorfsd`（pid = 2，init = 1 已占用）；系统服务层 S1 启动 | §5 |
| **索引数据存储** | vectorfsd 自管的 HNSW + B+Tree，落地为普通文件；通过 inode 扩展区在同一事务里 atomic commit | §4.3 |
| **摘要计算时机** | **写时同步**（延迟换一致性）；读时不重算 | §5.3 |
| **向量模型位置** | **用户态**（vectorfsd 内置 bge-small 等），内核零 ML 依赖；模型可热替换 | §5.4 |
| **模型版本管理** | summary slot 不带模型版本；重新索引靠后台 `vectorfsd sweep` | §5.4 |
| **语义检索路径** | `vectorfsd` 暴露 Endpoint（cptr 由 init 铸造并 grant 给所有用户进程） | §5.5 |
| **权限模型** | vectorfsd 持有 `FileSystem (R|W)` cap 集（限定索引根）；调用方持 `Endpoint (SEND)` cap + badge=agent_id | §5.6 |
| **错误码** | 复用 Doc 02 §4.3；vectorfsd 专属新增 `E_VEC_DIM_MISMATCH (-16)` / `E_VEC_NOT_READY (-17)` | §5.7 |

> ✅ 上述决策经 2026-09-27 三轮对话收敛，本文档定位为**架构与协议契约**。实现期 TBD 见 §9。

---

## 0. 文档目的

Synapse 是 AI-Native OS（[项目定位](../项目定位.md)）。文件系统层必须回答两个传统 OS 不会问的问题：

1. **每个文件"是什么"** —— 不仅存字节，还存可被 AI 消费的语义摘要；
2. **怎么找到"相关的东西"** —— 不仅按路径查，还按语义相似度查。

本文档把这两条**架构与 IPC 协议**钉死，让 P4.5 PCI 落地 → P6 存储栈 → S4 语义记忆三层按同一蓝图推进，避免各自为政。代码层面的 trait/消息常量从本 ADR 派生，详见 [kernel/src/block.rs](../../kernel/src/block.rs) 与 §5 IPC 协议。

---

## 1. 范围与边界

### 1.1 内核契约：FS = 三类 capability

| 内核原语 | 作用 | Capability | 持有者 |
|---------|------|-----------|--------|
| **`block_alloc`** | 分配一个块设备（PCI 设备 cap + BAR MMIO） | `BlockDevice` | FS 服务（暂未启动；P4.5 PCI 落地） |
| **`file_open`** | 打开路径，返回 FileSystem cap | `FileSystem (R\|W\|X)` | 任意用户进程 |
| **`vec_index`** | 注册/查询向量索引端点 | `Endpoint (SEND\|RECV)` | vectorfsd（Server 端） + 调用方（Client 端） |

内核**不**实现 FS / 索引逻辑，仅提供上述三类原语 + 持久化存储的物理后端（块设备驱动）。这与 [Doc 07 §1.1](07-display-stack-and-spatial-shell.md) 显示栈"framebuffer_alloc + gpu_irq_subscribe"的契约同构。

**架构性不变量**：
- 只有持有 `FileSystem (W)` 的进程能改文件字节；
- 只有持有 `FileSystem (W)` 的进程能写 summary slot（绕过此约束 = 绕过了索引一致性，违反本 ADR §4.2 原子提交保证）；
- vectorfsd 不持有任何外交工具 cap（[Doc 04 §3.2](04-diplomat-channel-architecture.md) 唯一网络出口不变量的对偶）——这里形成**唯一索引入口不变量**。

### 1.2 用户态分层

```
┌──────────────────────────────────────────────────────────────────┐
│  S4  Semantic Memory Layer（语义记忆，应用层）                    │
│   ├── EpisodicMemory / SemanticMemory / Reflector                │
│   └── 通过 vectorfsd IPC 检索（不直连 FS）                        │
│  ────────── 系统服务边界（IPC）───────────────────────────────  │
│  vectorfsd（用户态单例 system 服务，pid = 2）                      │
│   ├── HNSW 索引（ANN 检索，主内存 + 索引文件 mmap）                │
│   ├── 模型加载器（bge-small / 用户替换）                          │
│   ├── 摘要计算器（接收文件路径 → 读内容 → 算 embedding）          │
│   ├── Sweep Worker（crash 后重索引、模型升级全量重建）            │
│   └── IPC Server（Doc 03 同步 Endpoint）                          │
│  ────────── 内核边界（syscall + capability）─────────────────── │
│  Synapse Kernel Phase 4.5~6                                     │
│   ├── block（块设备驱动层，§3）                                   │
│   ├── fs（extent + summary slot + journal，§4）                   │
│   ├── cap（FileSystem capability）                                │
│   └── ipc（Endpoint / Notification）                              │
└──────────────────────────────────────────────────────────────────┘
```

### 1.3 进程拓扑

| 进程 | 数量 | 权限 | 备注 |
|------|------|------|------|
| **vectorfsd** | 单例 system 服务 | `FileSystem (R\|W)` 限定索引根 + `Endpoint (SEND\|RECV)` 服务端 | pid = 2 |
| **FS 服务（远期）** | 单例 | `BlockDevice (R\|W)` 持有块设备 | pid = 3 |
| **应用 / Agent** | N | `FileSystem` 限定其工作目录 + `Endpoint (SEND)` to vectorfsd | 受 Doc 01 L1~L5 权限约束 |

应用不直接调块设备、不直接读 summary slot——所有 FS 路径走 FS 服务或直接 syscall（FileSystem cap 即所需），向量检索一律经 vectorfsd。

---

## 2. 三层架构

```
┌───────────────────────────────┐
│  用户进程：write(path, data)  │  ← FS syscall (Doc 02 §4)
└─────────────┬─────────────────┘
              ▼
┌───────────────────────────────┐
│  FS 服务（远期 / MVP=内核）   │  ← extent-based + summary slot + WAL
│   - 写数据 blocks             │
│   - 算 BLAKE3(data) → 32B    │
│   - 写 summary slot (同事务)  │
└─────────────┬─────────────────┘
              ▼
┌───────────────────────────────┐
│  块设备驱动（kernel block.rs）│  ← virtio-blk / AHCI / NVMe
│   - 512B sector I/O           │
│   - BAR MMIO + DMA            │
└───────────────────────────────┘
```

**AI 检索的反向路径**：

```
用户进程：find_similar(query_vec, k)        ← vectorfsd IPC（§5）
   │
   ▼
vectorfsd：HNSW top-k ANN                   ← 全内存索引
   │
   ▼
vectorfsd：mint FileSystem cap(scope=match.path) → 转移给调用方
   │
   ▼
用户进程：read(cap) → 文件内容               ← FS syscall
```

---

## 3. 块设备驱动层（内核）

### 3.1 BlockDevice trait

定义在 [kernel/src/block.rs](../../kernel/src/block.rs)。统一接口：

```rust
pub trait BlockDevice: Send + Sync {
    fn read_sector(&self, lba: u64, buf: &mut [u8; 512]) -> Result<(), BlockError>;
    fn write_sector(&self, lba: u64, buf: &[u8; 512]) -> Result<(), BlockError>;
    fn capacity_sectors(&self) -> u64;
    fn name(&self) -> &str;  // null-terminated ≤16 字节，用于日志
}

pub enum BlockError {
    NotReady,    // 设备未就绪 / BAR 未映射
    OutOfRange,  // lba ≥ capacity_sectors
    IoError,     // virtio status != OK / ATA ERR 位
    RegistryFull,// 注册表已满（MAX_BLOCK_DEVS = 4）
}
```

**约束**：
- 所有实现必须 IRQ-safe（持锁期间可被中断嵌套）；
- `buf` 必须 8B 对齐（DMA 约束）；
- 调用方负责上层同步（page cache / fs journal）。

### 3.2 驱动后端矩阵

| 后端 | 优先级 | 落地时间 | 备注 |
|------|--------|---------|------|
| **virtio-blk (legacy MMIO)** | P0 | P4.5（PCI 用户态化后） | QEMU `-drive file=...` 默认即用；首选调试路径 |
| **virtio-blk (modern MMIO)** | P1 | P6 远期 | virtio spec 1.1+，需 cap structure 协商 |
| **AHCI (SATA)** | P1 | P6 真机 | 笔记本 / 服务器通用；port multiplier 暂不支持 |
| **NVMe** | P2 | P6 远期 | 高吞吐；需 MSI-X + 多队列（per-CPU kthread） |

MVP（P4.5）只起 **virtio-blk stub**：[kernel/src/block.rs](../../kernel/src/block.rs) 提供 trait + 占位结构，PCI 探测 + BAR 映射 + virtqueue 协商留 TODO，P4.5 PCI 子系统落地时填实。

### 3.3 注册表与生命周期

```rust
const MAX_BLOCK_DEVS: usize = 4;
static REGISTRY: SpinLock<[Option<DeviceEntry>; MAX_BLOCK_DEVS]> = ...;

pub fn register(dev: &'static dyn BlockDevice) -> Result<usize, BlockError>;
pub fn device(idx: usize) -> Option<&'static dyn BlockDevice>;
pub fn device_count() -> usize;
pub fn stats(idx: usize) -> Option<BlockStatsSnapshot>;
```

- 注册表用固定数组（`Option<DeviceEntry>`），**不 alloc**（与 [kernel/src/kstate.rs](../../kernel/src/kstate.rs) 静态表同构）；
- 同一设备重复注册 → 幂等返回旧 slot；
- 设备死亡（PCI 热拔）→ 当前 MVP 静默标记 NotReady；热拔通知留给 P6 + Doc 01 监督树。

### 3.4 不在本模块

- DMA buffer 池：复用 [kernel/src/page_frame.rs](../../kernel/src/page_frame.rs) 分配的 4KB 页；
- page cache：与 FS 服务同阶段，单独 `fs/cache`（不在 block.rs）；
- IO 调度器（noop/deadline）：MVP 走 noop，顺序提交；P6 加 deadline。

---

## 4. FS 元数据（用户态服务 / MVP 期内核桩）

### 4.1 布局总览

```
┌─────────────────────────────────────────────────────┐
│  Block 0: Superblock（魔数 + 元数据指针）            │
│  Block 1..N: Journal（WAL）                          │
│  Block N+1..M: Inode Bitmap                          │
│  Block M+1..P: Inode Table（每个 inode = 256B）      │
│  Block P+1..Q: Extent Map（每个文件 → [start, len]） │
│  Block Q+1..R: Data Blocks（4KB 对齐）              │
│  Block R+1..S: Summary Index（vectorfsd 索引文件）    │
└─────────────────────────────────────────────────────┘
```

### 4.2 Inode 布局（256B）

| 偏移 | 大小 | 字段 | 说明 |
|------|------|------|------|
| 0x00 | 2 | mode | 文件类型 + 权限位 |
| 0x02 | 2 | uid / gid | 受 Doc 01 Agent 权限模型约束 |
| 0x04 | 8 | size | 字节长度 |
| 0x0C | 8 | mtime | 修改时间（TSC ns） |
| 0x14 | 4 | nlinks | 硬链接计数 |
| 0x18 | 4 | extents | extent 数量（≤ 4 直存，溢出走间接 extent block） |
| 0x1C | 64 | extent[4] | `(lba: u48, len: u16)` packed |
| 0x5C | 4 | summary_len | summary slot 占用长度（0 = 未索引） |
| 0x60 | 32 | **summary_hash** | BLAKE3(data) |
| 0x80 | 8 | **summary_idx_cap** | 指向 vectorfsd 索引条目（pid + entry_id 编码） |
| 0x88 | 8 | flags | 压缩、加密、ACL 标志 |
| 0x90 | 96 | reserved | 留给 Doc 01 §5 ACL / quota |
| 0xFF | 1 | checksum | BLAKE3(inode[0..0xFF]) |

**summary slot 写入纪律**：
- `write(fd, data, len)` syscall 路径上：
  1. 算 `BLAKE3(data) = hash`（用户态或内核 fast-path）；
  2. **同一事务**（同一个 journal record）写 data blocks + 写 summary slot；
  3. 事务提交后**异步**通知 vectorfsd（Doc 03 Notification 机制）；
  4. vectorfsd 接收通知 → 读文件 → 算 embedding → 写自己管理的 HNSW。
- **原子性保证**：crash 后 summary slot 与 data blocks 同进退（journal 回滚）；
- **索引最终一致**：vectorfsd 异步索引可能落后（O(秒)），但绝不会"data 已落盘、hash 未落盘"——这是 §1.1 唯一索引入口不变量的体现。

### 4.3 索引存储（vectorfsd 索引文件）

vectorfsd 的 HNSW 索引**也是普通文件**，落地在 `Block R+1..S` 区间：

```
Summary Index File:
  ┌─ Magic (8B "VFSIDX01")
  ├─ Version (4B, 当前 1)
  ├─ Model Id Hash (32B, BLAKE3("bge-small-v1.5"))
  ├─ Vec Dim (4B)
  ├─ Entry Count (8B)
  ├─ HNSW Layer 0 (邻接表 + 向量)
  ├─ HNSW Layer 1..L (高层入口)
  └─ B+Tree (path → entry_id, 用于按路径删除/更新)
```

**原子更新**：vectorfsd 写索引文件走 **copy-on-write**——
1. 写新版本到临时 inode；
2. 写"指针切换"事务记录到 journal（指向新 inode）；
3. FS 服务 commit journal → 切指针；
4. 旧 inode 待引用计数归零后回收。

### 4.4 不在本模块

- 文件锁（fcntl 等）：MVP 不实现，P5 视需求补；
- ACL / 配额 enforcement：Doc 01 §5 接；
- 压缩 / 加密：P6 远期；
- 快照 / CoW 文件系统：P6 远期。

---

## 5. vectorfsd IPC 协议

### 5.1 服务身份

| 项 | 值 | 备注 |
|----|-----|------|
| **进程名** | `vectorfsd` | ELF 镜像名 |
| **预留 pid** | 2 | init = 1 已占用；FS 服务（远期）= 3 |
| **服务 Endpoint** | `vfs.endpoint` | init 启动时铸造 + grant；init 持有 `Endpoint (SEND\|RECV)` |
| **死亡 Notification** | 暂不提供 | vectorfsd 故障 → 由监督树（[Doc 02 §5.4](../设计文档/02-userspace-abi-and-process-model.md)）接管 |

### 5.2 Endpoint 铸造与分发

1. **init 启动顺序**（Phase 4.5+）：
   ```rust
   // init 进程（pid=1）启动逻辑
   let vfs_pid = spawn("vectorfsd.elf");
   let ep_obj = k_alloc_object(ObjKind::Endpoint)?;
   let vfs_cap = k_mint_root(vfs_pid, ep_obj, SEND|RECV)?;
   k_transfer(init_pid, user_pid, &[TransferItem { cptr: vfs_cap, mask: SEND }]);
   ```
2. **badge** = 调用方 `AgentId`（[Doc 01 §7](../设计文档/01-capability-agent-permission-model.md)），vectorfsd 通过 badge 反查权限；
3. **调用方持有的 rights**：`SEND`（不允许 RECV——单向 RPC 语义）。

### 5.3 消息格式（Doc 03 扩展）

vectorfsd 的所有消息复用 Doc 03 IPC 单拷贝路径（`IpcSend` syscall），payload ≤ `MAX_PAYLOAD = 4096`。
每条消息以一个 **VectorFsMessageHeader** 开头：

```rust
#[repr(C)]
pub struct VectorFsMessageHeader {
    /// 消息类型（见 §5.3.1）
    pub kind: u32,
    /// 请求 ID（用于异步 RPC 关联回复）
    pub request_id: u64,
    /// 调用方 badge（Doc 01 agent_id，vectorfsd 校验权限）
    pub caller_agent: u32,
    /// 保留字段
    pub _reserved: u32,
}

/// VectorFsMessageHeader.kind 枚举值
pub mod msg_kind {
    pub const INDEX_FILE:       u32 = 1;  // §5.3.2
    pub const FIND_SIMILAR:     u32 = 2;  // §5.3.3
    pub const UPDATE_SUMMARY:   u32 = 3;  // §5.3.4
    pub const REMOVE_FILE:      u32 = 4;  // §5.3.5
    pub const REINDEX_REQUEST:  u32 = 5;  // §5.3.6
}
```

#### 5.3.1 通用约定

- **payload 布局**：`<VectorFsMessageHeader><body...>`，body 长度由 `kind` 决定；
- **cap transfer**：调用方（client）→ vectorfsd（server）：仅 `IndexFile` / `ReindexRequest` 可携带 `FileSystem (R)` cap（vectorfsd 据此读文件）；server → client：`FindSimilar` 通过 cap-out 数组回传 `FileSystem (R)` cap 列表；
- **错误返回**：vectorfsd 单向 RPC，错误码在 payload 头部 `kind` 字段置 `0xFFFF_FFFF` + 跟随 `i32` 错误码（复用 Doc 02 §4.3 错误码 + §3.7 vectorfsd 专属）；
- **超长 payload**：向量维度 > 1024（float32 = 4096B 边界）→ 改走 Doc 03 大对象 grant 路径，header `kind` 仍相同。

#### 5.3.2 `INDEX_FILE` (kind=1)

写文件后异步触发索引更新。**调用方** 主动 push（不等 FS 通知），用于：
- 大文件分块写完后追加索引；
- 模型升级后手动重索引单文件。

```rust
#[repr(C)]
pub struct IndexFileBody {
    pub path_len: u16,           // UTF-8 字节数
    pub path_bytes: [u8; 256],   // 内联路径（MVP 不支持 >256 字符）
    pub summary_hash: [u8; 32],  // BLAKE3(data)
    pub force_recompute: bool,   // true = 忽略现有索引重算
}
```

vectorfsd 收到后：
1. 校验调用方 badge 是否有该路径的索引权限；
2. 读文件 → 算 embedding（调用本地模型）；
3. 写 HNSW + 写 B+Tree 路径项；
4. **同步回复**：payload = `[kind=0xFFFF_FFFF, err: i32, new_entry_id: u64]`。

#### 5.3.3 `FIND_SIMILAR` (kind=2)

最常用。请求格式：

```rust
#[repr(C)]
pub struct FindSimilarBody {
    pub query_vec_dim: u16,       // ≤ 1024
    pub k: u16,                    // top-k，≤ 64
    pub min_similarity: f32,       // 0.0..1.0，过滤阈值
    pub _pad: u32,
    pub query_vec: [f32; 1024],   // 内联；>1024 改 grant
}
```

vectorfsd 处理：
1. HNSW ANN top-k 召回（O(log N) 期望）；
2. 按 `min_similarity` 过滤；
3. **为每个命中 mint 一个 `FileSystem (R)` cap**，scope = 命中的文件路径；
4. 通过 IPC cap-out 数组回传（Doc 03 §3.1 cap transfer）；
5. payload 同步返回：`[kind=0xFFFF_FFFF, err: i32, count: u8, file_meta: [FileMeta; k]]`。

```rust
#[repr(C)]
pub struct FileMeta {
    pub path_len: u16,
    pub path_bytes: [u8; 256],
    pub similarity: f32,
    pub entry_id: u64,
}
```

**调用方拿到 cap 后**：用 `FileSystem (R)` cap 调 `read(fd, buf, len)` syscall 读文件内容。
cap 在 IPC 单拷贝消息同步期内 transfer，调用方所有权**仅限本次会话**——vectorfsd 不持有对端 cap（单向 RPC 语义）。

#### 5.3.4 `UPDATE_SUMMARY` (kind=3)

文件覆盖写后调用：

```rust
#[repr(C)]
pub struct UpdateSummaryBody {
    pub path_len: u16,
    pub path_bytes: [u8; 256],
    pub new_summary_hash: [u8; 32],
}
```

vectorfsd 行为：找到旧 entry → 删除 → 触发后台 re-index（不等算完即返）。
回复：`[kind=0xFFFF_FFFF, err: i32]`。

#### 5.3.5 `REMOVE_FILE` (kind=4)

文件删除时调用（**仅** 当调用方持有 `FileSystem (W)` on the path）：

```rust
#[repr(C)]
pub struct RemoveFileBody {
    pub path_len: u16,
    pub path_bytes: [u8; 256],
}
```

vectorfsd：B+Tree 查 path → 删 entry → 写 HNSW tombstone。
回复：`[kind=0xFFFF_FFFF, err: i32]`。

#### 5.3.6 `REINDEX_REQUEST` (kind=5)

模型升级 / crash recovery / 手动批量重索引：

```rust
#[repr(C)]
pub struct ReindexBody {
    pub scope: u8,  // 0 = 全量, 1 = 单路径, 2 = 子树
    pub path_len: u16,
    pub path_bytes: [u8; 256],
    pub force_overwrite: bool,
}
```

vectorfsd：入队后台任务，**立即返**（不阻塞 RPC）：
回复：`[kind=0xFFFF_FFFF, err: i32, task_id: u64]`。
进度通过 `Notification` cap（Doc 03）异步通知（可选 cap transfer）。

### 5.4 模型版本与摘要时机

**摘要计算时机**：
- **写时同步**（DECIDED）—— `write()` syscall 提交 journal 前算 BLAKE3 hash 并写 summary slot；
- **embedding 异步** —— 通知 vectorfsd 后台算，不阻塞 syscall；
- **读时不重算** —— `read()` 不验证 hash（hash 是索引用，不做完整性校验；完整性靠 journal）；
- **崩溃恢复** —— vectorfsd 启动时扫 summary slot vs 索引文件 → 缺失项入队重索引（无需重启服务）。

**模型位置**（DECIDED）：
- 用户态（vectorfsd 进程内）；
- 默认 `bge-small-v1.5`（384 维，~33MB FP16）；
- 模型文件存 `/etc/vectorfsd/models/<hash>.bin`，hash 即 BLAKE3（model_bytes）；
- 热替换：vectorfsd 收到 `MODEL_SWAP`（kind=6，**未列入本文档**，留给 v2）信号 → 重新加载；
- summary slot **不带**模型版本字段——重新索引靠 `vectorfsd sweep`（后台 worker 周期扫 / 显式触发）。

### 5.5 完整调用时序（FIND_SIMILAR 端到端）

```
┌────────┐         ┌──────────┐         ┌────────────┐
│ Client │         │ Kernel   │         │ vectorfsd  │
└───┬────┘         └────┬─────┘         └──────┬─────┘
    │ IpcSend(payload)  │                       │
    ├──────────────────►│ resolve_endpoint      │
    │                   │ single-copy payload   │
    │                   ├──────────────────────►│
    │                   │                       │ HNSW top-k
    │                   │                       │ mint N × FileSystem(R)
    │                   │ ◄──────────────────────│ cap transfer + reply
    │ IpcRecv return    │                       │
    │ ◄─────────────────┤                       │
    │ read(cap)         │                       │
    ├──────────────────►│ walk + copy           │
    │                   │                       │
    │ syscall ret       │                       │
    │ ◄─────────────────┤                       │
```

### 5.6 Capability 传递规则（钉死）

| 场景 | cap 流向 | mask | 备注 |
|------|----------|------|------|
| Client → vectorfsd（INDEX_FILE / REINDEX） | Client → Server | `FileSystem (R)` | scope = 该 path 的父目录 |
| vectorfsd → Client（FIND_SIMILAR 命中） | Server → Client | `FileSystem (R)` | scope = 命中的文件路径（**仅**该文件） |
| Client → vectorfsd（UPDATE_SUMMARY / REMOVE_FILE） | 无 cap 传输 | — | 仅凭 badge + path；vectorfsd 用自持 `FileSystem (R\|W)` 验证 |
| vectorfsd 启动持有 | — | `FileSystem (R\|W)` on `/var/lib/vectorfsd/` | 索引文件落地处 |

**badge 权限校验**（vectorfsd 内部）：
1. 解析 path → 检查是否在调用方 agent 的 Doc 01 §5 授权路径内；
2. INDEX_FILE / FIND_SIMILAR：允许访问 = agent 在该路径有 `R` 权限；
3. UPDATE_SUMMARY / REMOVE_FILE：需要 `W`；
4. REINDEX_REQUEST：仅 L4+ agent 允许（Doc 01 §5.1 高风险操作确认通道）。

### 5.7 错误码（vectorfsd 专属）

复用 [Doc 02 §4.3](../设计文档/02-userspace-abi-and-process-model.md) 已定义错误码；vectorfsd 新增：

| 错误码 | 值 | 触发条件 |
|--------|----|---------|
| `E_VEC_DIM_MISMATCH` | -16 | `query_vec_dim` 与索引 dim 不一致 |
| `E_VEC_NOT_READY` | -17 | 索引正在 rebuild / 模型未加载完成 |
| `E_VEC_QUOTA_EXCEEDED` | -18 | 索引条目数超过 L5 配额（默认 100k） |
| `E_VEC_MODEL_LOADING` | -19 | 模型热替换中（暂时拒服务） |

---

## 6. 阶段拆分

### 6.1 P4.5：PCI 用户态化 + block trait 落地

- [ ] `kernel/src/block.rs`：BlockDevice trait + virtio-blk stub + 注册表（**已起，2026-09-27**）
- [ ] PCI walker 抽离到 `kernel/src/pci.rs`（从 [bootanim/vbe.rs](../../kernel/src/bootanim/vbe.rs) 复制并暴露）
- [ ] virtio-blk 实做：BAR 映射 + 设备 reset + virtqueue 协商 + `read_sector`/`write_sector`
- [ ] QEMU 启动挂载磁盘 → 跑通 read/write 真机 smoke

### 6.2 P5：FS 服务骨架

- [ ] extent-based inode + extent map + journal（内核桩 → 用户态服务）
- [ ] summary slot 写入路径（write syscall 同事务）
- [ ] FileSystem cap（Doc 01 扩展）
- [ ] 原子提交测试（crash → recovery → hash 与 data 一致）

### 6.3 P5.5：vectorfsd v1

- [ ] 单例进程 + init 启动 + Endpoint 铸造 grant
- [ ] IPC 协议实现（§5.3 全部 kind）
- [ ] HNSW 索引文件（§4.3 copy-on-write）
- [ ] bge-small-v1.5 模型加载
- [ ] 真机 smoke：3 文件写入 → FIND_SIMILAR 召回 top-1 一致

### 6.4 S4：语义记忆接入

- [ ] EpisodicMemory / SemanticMemory 通过 vectorfsd 检索
- [ ] Reflector 用向量相似度发现重复经验
- [ ] 模型升级工具链（kind=6 REINDEX_REQUEST + 离线 sweep）

### 6.5 P6 远期

- [ ] AHCI / NVMe 驱动（[§3.2](#32-驱动后端矩阵) P1/P2）
- [ ] 大对象 grant 路径（>4KB 向量）
- [ ] Snapshot / CoW 文件系统
- [ ] 分布式向量索引（[Doc 05 §4](../设计文档/05-inter-os-diplomacy-protocol.md) 跨 OS 共享）

---

## 7. 风险与缓解

| # | 风险 | 影响 | 缓解 |
|---|------|------|------|
| **R1** | summary slot 写时同步 vs 异步性能矛盾 | 写延迟增加 | BLAKE3 ~1GB/s，4KB 文件 < 4μs；可接受；P6 接 AVX512 加速 |
| **R2** | vectorfsd 单点故障 | 全系统无法检索 | 监督树策略（[Doc 02 §5.4](../设计文档/02-userspace-abi-and-process-model.md)）：崩溃自动重启 + 启动时 sweep 重建索引 |
| **R3** | embedding 模型内存占用大（bge-small ~33MB） | 进程内存预算 | 模型文件 mmap，按需换页；多个 agent 共享同一 vectorfsd 实例 |
| **R4** | 索引文件与数据文件不一致（crash） | 检索结果错误 | §4.2 journal 原子提交 + §5.4 启动 sweep 双保险 |
| **R5** | HNSW 在 N > 1M 后内存爆炸 | 无法扩展 | 分层 HNSW + 磁盘 spill（PQ 量化）；MVP 仅支持 N ≤ 100k |
| **R6** | 路径含恶意字符 / 越界访问 | 越权读 | vectorfsd badge 校验 + path canonicalize（拒绝 `..`、空字节、非 UTF-8） |
| **R7** | 多个 Agent 同时 INDEX 同一文件 | 写竞争 | vectorfsd 内部加锁 + 写时复制；后续不冲突则合并 |

---

## 8. 与其他文档的对齐

| 对齐项 | 本文档位置 | 对应文档 |
|--------|----------|----------|
| `BlockDevice` / `FileSystem` capability | §1.1 | [Doc 01 §3 Capability 模型](01-capability-agent-permission-model.md) |
| vectorfsd 持有 `Endpoint (SEND\|RECV)` | §5.1 | [Doc 01 §6 服务进程 cap 模板] |
| IPC 消息格式 + 单拷贝 | §5.3 | [Doc 03 §3 IPC 消息格式](03-ipc-message-and-single-copy-path.md) |
| cap transfer 规则（FileSystem scope） | §5.6 | [Doc 01 §4 Capability 生命周期](01-capability-agent-permission-model.md) |
| 唯一索引入口不变量 | §1.1 | [Doc 04 §3.2](04-diplomat-channel-architecture.md) 唯一网络出口不变量的对偶 |
| Agent L4/L5 高风险操作确认 | §5.6 | [Doc 01 §5.1](01-capability-agent-permission-model.md) |
| 监督树 + freeze/thaw | §7 R2 | [Doc 02 §5.4](../设计文档/02-userspace-abi-and-process-model.md) |
| 服务层 S4 语义记忆 | §6.4 | [Doc 06 §6.6 S4 记忆与自进化](06-system-services-roadmap.md) |
| 持久化存储（Phase 6 存储栈） | §3~4 | [需求目标 Phase 6](../需求目标.md) |
| 跨 OS 向量索引共享（远期） | §6.5 | [Doc 05 §4](../设计文档/05-inter-os-diplomacy-protocol.md) |

---

## 9. 开放 TBD（不阻塞 P4.5 启动）

| TBD | 决策点 | 何时收敛 |
|-----|--------|----------|
| vectorfsd pid 分配策略 | 硬编码 2 / 由 init 服务注册表分配 | P5 启动前 |
| 默认 embedding 模型 | bge-small / mxbai / 自研 | P5.5 模型评测 |
| 索引文件压缩 | LZ4 / ZSTD / 不压缩 | P6 性能基线 |
| HNSW 参数 (M, ef_construction) | hnswlib 默认 / 自研调优 | P5.5 召回率评测 |
| 多语言支持 | 当前仅 ASCII path / 加 UTF-8 校验 | P5.5 |
| 索引加密 | 与 Doc 01 §5 L5 数据加密集成 | P6 |

---

## 10. 参考 (References)

- ext4 on-disk layout (extent tree + journal) — 参考 extent 编码与 journal group commit
- BLAKE3 hash function — [https://github.com/BLAKE3-team/BLAKE3](https://github.com/BLAKE3-team/BLAKE3)
- HNSW (Hierarchical Navigable Small World) — Malkov & Yashunin 2018
- bge-small-en-v1.5 — BAAI 嵌入模型，384 维
- virtio spec v1.1 — [https://docs.oasis-open.org/virtio/virtio/v1.1/](https://docs.oasis-open.org/virtio/virtio/v1.1/)
- [kernel/src/block.rs](../../kernel/src/block.rs) — BlockDevice trait 实现
- [kernel/src/bootanim/vbe.rs](../../kernel/src/bootanim/vbe.rs) — 现有 PCI walker（block.rs 复用）
- [设计文档 07](07-display-stack-and-spatial-shell.md) — 显示栈契约（与本文 §1.1 同构）