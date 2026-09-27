//! 块设备驱动层 — BlockDevice trait + virtio-blk stub (P4.5 预研启动)
//!
//! ## 范围（DECIDED，见 `docs/design/08-fs-ai-index.md` §3）
//!
//! - 定义 [`BlockDevice`] trait：512B sector 接口 + 错误类型 + 设备名
//! - 内核内静态注册表 [`REGISTRY`]（容量 4，固定数组免 alloc）
//! - [`virtio`] 子模块：virtio-blk 桩结构 + PCI 位置类型；MMIO 读写留 TODO
//!   （P4.5 PCI 用户态化后实做 BAR 映射 + virtqueue 协商）
//! - [`init_block`]：boot 链路调一次；当前仅探测 PCI 找候选设备，
//!   真实注册等到 P4.5 virtqueue 落地后启用
//!
//! ## PCI 复用约定
//!
//! [`pci`] 子模块复制自 [`crate::bootanim::vbe`] 的 0xCF8/0xCFC walker
//! （约 30 行）。bootanim 窗口未公开 PCI API，本模块自维护最小拷贝
//! —— 后续 P4.5 抽出公共 [`crate::pci`] 后删除此副本（详见
//! `docs/design/rule.md` §10 防冲突协议：禁止修改其他窗口私产）。
//!
//! ## 不在本模块
//!
//! - FS 元数据（inode / extent / journal）：下一阶段单独 `fs/` crate
//! - AI 索引（vectorfsd）：用户态服务，IPC 协议见 Doc 08 §5
//! - AHCI / NVMe 真机驱动：P6 远期
//! - DMA buffer 池：复用 [`crate::page_frame`] 分配的 4KB 页
//! - IO 调度器：MVP 走 noop（顺序提交），P6 加 deadline

#![allow(dead_code)]

use core::sync::atomic::{AtomicU32, Ordering};

use log::{info, warn};

use crate::sync::SpinLock;

// ============================================================
// 错误类型
// ============================================================

/// 块设备操作错误（`Send + Copy`，可作 syscall 返回值打包）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockError {
    /// 设备未就绪（probe 未完成 / BAR 未映射 / virtqueue 未协商）
    NotReady,
    /// LBA 越界（>= `capacity_sectors()`）
    OutOfRange,
    /// IO 错误（virtio-blk status != OK / ATA ERR 位 / NVMe completion 失败）
    IoError,
    /// 注册表已满（`MAX_BLOCK_DEVS` = 4）
    RegistryFull,
}

// ============================================================
// 设备 trait
// ============================================================

/// 块设备统一接口（512B sector 粒度）。
///
/// **MVP 约束**：
/// - 所有实现必须 IRQ-safe（持锁期间可被中断嵌套）
/// - `buf` 必须 8B 对齐（DMA 约束；由调用方保证）
/// - 失败时 `buf` 内容**未定义**（调用方应按 0 处理或重试）
/// - 调用方（FS / page cache）负责上层同步
pub trait BlockDevice: Send + Sync {
    /// 读一个 sector（512B）到 `buf`。
    ///
    /// `lba` 必须 < `capacity_sectors()`，否则返 `OutOfRange`。
    fn read_sector(&self, lba: u64, buf: &mut [u8; 512]) -> Result<(), BlockError>;

    /// 写一个 sector（512B）到 `lba`。
    fn write_sector(&self, lba: u64, buf: &[u8; 512]) -> Result<(), BlockError>;

    /// 设备 sector 总数。
    fn capacity_sectors(&self) -> u64;

    /// 设备识别名（用于日志 / 调试），≤16 字节。
    fn name(&self) -> &str;
}

// ============================================================
// 注册表 + 统计
// ============================================================

/// 注册表容量（固定数组免 alloc，与 [`crate::kstate`] 静态表同构）。
const MAX_BLOCK_DEVS: usize = 4;

struct DeviceEntry {
    dev: &'static dyn BlockDevice,
    stats: BlockStats,
}

/// 每设备统计（IOPS / 错误率观测）。
#[derive(Default)]
pub struct BlockStats {
    /// 成功读取的 sector 数（自注册以来累加）
    pub reads: AtomicU32,
    /// 成功写入的 sector 数
    pub writes: AtomicU32,
    /// 失败 IO 数（read/write 返 Err 计数）
    pub errors: AtomicU32,
}

/// 统计快照（Copy，用于日志 / 监控导出）。
#[derive(Debug, Clone, Copy)]
pub struct BlockStatsSnapshot {
    /// `BlockStats::reads` 快照
    pub reads: u32,
    /// `BlockStats::writes` 快照
    pub writes: u32,
    /// `BlockStats::errors` 快照
    pub errors: u32,
}

static REGISTRY: SpinLock<[Option<DeviceEntry>; MAX_BLOCK_DEVS]> =
    SpinLock::new([const { None }; MAX_BLOCK_DEVS]);

/// 注册一个块设备（boot 链路调一次）。
///
/// 同一设备重复注册 → 幂等返回旧 slot。
/// 注册表满 → `RegistryFull`。
pub fn register(dev: &'static dyn BlockDevice) -> Result<usize, BlockError> {
    let mut g = REGISTRY.lock();
    for (i, slot) in g.iter_mut().enumerate() {
        if let Some(e) = slot.as_ref() {
            // fat pointer 相等即同一设备实例（含 data ptr + vtable ptr）
            if core::ptr::eq(e.dev, dev) {
                return Ok(i);
            }
        } else {
            *slot = Some(DeviceEntry {
                dev,
                stats: BlockStats::default(),
            });
            return Ok(i);
        }
    }
    Err(BlockError::RegistryFull)
}

/// 取第 idx 个设备（None = 未注册或 idx 越界）。
pub fn device(idx: usize) -> Option<&'static dyn BlockDevice> {
    REGISTRY
        .lock()
        .get(idx)
        .and_then(|s| s.as_ref())
        .map(|e| e.dev)
}

/// 已注册设备数。
pub fn device_count() -> usize {
    REGISTRY.lock().iter().filter(|s| s.is_some()).count()
}

/// 设备统计快照（用于日志 / 监控）。
pub fn stats(idx: usize) -> Option<BlockStatsSnapshot> {
    REGISTRY.lock().get(idx).and_then(|s| s.as_ref()).map(|e| BlockStatsSnapshot {
        reads: e.stats.reads.load(Ordering::Relaxed),
        writes: e.stats.writes.load(Ordering::Relaxed),
        errors: e.stats.errors.load(Ordering::Relaxed),
    })
}

// ============================================================
// PCI 配置空间 walker（0xCF8 / 0xCFC 机制）
// ============================================================
//
// 复制自 `kernel/src/bootanim/vbe.rs::pci`（约 30 行）。bootanim 窗口
// 未公开 PCI API（rule.md §10 防冲突），本模块自维护最小拷贝；P4.5
// 抽离公共 `kernel/src/pci.rs` 后删除此副本。
mod pci {
    /// 读 32-bit PCI 配置寄存器（bus/dev/fn 均 0 起始，reg 为字节偏移且 4 对齐）。
    pub fn read(bus: u32, dev: u32, func: u32, reg: u32) -> u32 {
        let addr = 0x8000_0000u32 | (bus << 16) | (dev << 11) | (func << 8) | (reg & 0xFC);
        unsafe {
            core::arch::asm!(
                "out dx, eax",
                in("dx") 0xCF8u16,
                in("eax") addr,
                options(nostack, preserves_flags),
            );
            let v: u32;
            core::arch::asm!(
                "in eax, dx",
                in("dx") 0xCFCu16,
                out("eax") v,
                options(nostack, preserves_flags),
            );
            v
        }
    }

    /// 解析 BAR（MEM BAR，处理 64-bit；IO BAR 返回 None）。
    pub fn bar_mem_base(bus: u32, dev: u32, func: u32, reg: u32) -> Option<u64> {
        let lo = read(bus, dev, func, reg);
        if lo & 0x1 == 1 {
            return None;
        }
        let base_lo = (lo & 0xFFFF_FFF0) as u64;
        if lo & 0x4 == 0x4 {
            let hi = read(bus, dev, func, reg + 4) as u64;
            Some(base_lo | (hi << 32))
        } else {
            Some(base_lo)
        }
    }
}

// ============================================================
// virtio-blk 子模块（stub）
// ============================================================

/// virtio-blk 设备桩。
///
/// **当前状态**：仅持有 PCI 位置 + 容量字段；`mmio_base = 0` 表示未映射，
/// 所有 I/O 返回 `NotReady`。P4.5 PCI + virtqueue 落地后实做：
/// 1. 写 virtio spec §3.1 要求的 magic value (`0x74726976`) / version (`1` = legacy)
/// 2. Device Status 寄存器走 reset → ACK → DRIVER → DRIVER_OK 序列
/// 3. 协商 virtqueue 0（queue_sel / queue_num / queue_desc|avail|used PA）
/// 4. 提交 `VIRTIO_BLK_T_IN` / `VIRTIO_BLK_T_OUT` 请求 + kick + poll used ring
pub mod virtio {
    use super::{BlockDevice, BlockError};

    /// PCI 位置编码：`(bus << 16) | (dev << 8) | (fn)`
    pub type PciLocation = u32;

    /// virtio-blk PCI vendor ID（virtio spec §4.1.2）
    pub const VIRTIO_VENDOR_ID: u16 = 0x1AF4;
    /// virtio-blk PCI device ID（legacy transitional：subsystem vendor 已固定）
    pub const VIRTIO_BLK_DEVICE_ID: u16 = 0x1001;
    /// legacy MMIO BAR 长度（spec §4.2.2：20 字节寄存器窗口）
    pub const LEGACY_MMIO_LEN: usize = 0x20;

    /// virtio-blk 桩设备。
    pub struct VirtioBlk {
        /// PCI 位置编码
        pub pci_loc: PciLocation,
        /// 设备容量（sectors）；0 = 未读取 cfg[24..31]
        pub capacity_sectors: u64,
        /// legacy MMIO 基址（恒等映射下 = 物理地址）；0 = 未映射
        pub mmio_base: u64,
    }

    impl VirtioBlk {
        /// 创建桩（mmio_base = 0，capacity = 0，所有 IO 返 NotReady）。
        pub const fn stub(pci_loc: PciLocation) -> Self {
            Self {
                pci_loc,
                capacity_sectors: 0,
                mmio_base: 0,
            }
        }
    }

    impl BlockDevice for VirtioBlk {
        fn read_sector(&self, _lba: u64, _buf: &mut [u8; 512]) -> Result<(), BlockError> {
            Err(BlockError::NotReady)
        }
        fn write_sector(&self, _lba: u64, _buf: &[u8; 512]) -> Result<(), BlockError> {
            Err(BlockError::NotReady)
        }
        fn capacity_sectors(&self) -> u64 {
            self.capacity_sectors
        }
        fn name(&self) -> &str {
            "virtio-blk-stub"
        }
    }
}

// ============================================================
// boot 链路初始化
// ============================================================

/// 块设备子系统 init。boot 链路在 clock init 之后、smoke 之前调一次。
///
/// **当前 stub 行为**：
/// 1. 扫 PCI bus 0 找 vendor=0x1AF4 device=0x1001 的 virtio-blk 候选
/// 2. 发现候选 → log PCI 位置 + BAR5 + class（不注册，留 P4.5 实做）
/// 3. 未发现 → log warn + 继续（系统仍可从 initrd 启动）
pub fn init_block() {
    info!("[block] init: probing PCI bus 0 for virtio-blk (vendor=0x1AF4, device=0x1001)");

    let candidates = probe_virtio_blk();

    info!(
        "[block] init: {} candidate(s) found, {} device(s) registered",
        candidates,
        device_count()
    );
    if candidates == 0 {
        warn!("[block] no virtio-blk found — running off initrd only (P4.5 落地后挂盘)");
    }
}

/// 扫 PCI bus 0 找 virtio-blk 候选设备数量（不注册，留 P4.5 真实驱动接管）。
///
/// P4.5 替换计划（BAR 映射 + virtqueue + register）：
/// ```ignore
/// for dev in 0..32 {
///     let id = pci::read(0, dev, 0, 0x00);
///     if id == 0xFFFF { continue; }
///     if id & 0xFFFF != VIRTIO_BLK_DEVICE_ID as u32 { continue; }
///     let bar5 = pci::bar_mem_base(0, dev, 0, 0x24).unwrap_or(0);
///     let cap = pci::read(0, dev, 0, 0x14) as u64;  // BAR0 暂不用
///     // ... map BAR, init virtqueue ...
///     let blk = &'static_virtio_blk;  // 静态化
///     register(blk)?;
/// }
/// ```
fn probe_virtio_blk() -> usize {
    let mut found = 0usize;
    for dev in 0..32u32 {
        let id = pci::read(0, dev, 0, 0x00);
        if id == 0xFFFF || id == 0 {
            continue;
        }
        let vendor = (id & 0xFFFF) as u16;
        let device = ((id >> 16) & 0xFFFF) as u16;
        if vendor == virtio::VIRTIO_VENDOR_ID && device == virtio::VIRTIO_BLK_DEVICE_ID {
            let bar5 = pci::bar_mem_base(0, dev, 0, 0x24).unwrap_or(0);
            info!(
                "[block] candidate: virtio-blk @ PCI bus=0 dev={} bar5={:#x}",
                dev, bar5
            );
            found += 1;
        }
    }
    found
}