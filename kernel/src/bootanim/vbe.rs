//! VBE 线性帧缓冲初始化（bochs dispi 接口）。
//!
//! 开机动画的唯一显示后端：PCI 枚举找到显示控制器（class 0x03xx），
//! 取其 BAR0 作为 LFB 物理基址（QEMU pc/q35 上恒等映射覆盖 0–4GiB，
//! 故物理地址即虚拟地址），再用 bochs VBE dispi 寄存器切到 1024x768x32。
//!
//! 失败路径（无显示控制器 / dispi 不接受模式）返回 `None`，
//! 调用方静默跳过动画——保证无头环境与非 QEMU 环境不受影响。

/// 帧缓冲描述符。
#[derive(Clone, Copy)]
pub struct FbInfo {
    /// LFB 虚拟地址（恒等映射下 = 物理地址），按 32bpp XRGB 解释。
    pub ptr: *mut u32,
    /// 水平分辨率（像素）。
    pub w: u32,
    /// 垂直分辨率（像素）。
    pub h: u32,
    /// 每行像素数（stride，此处恒等于 `w`）。
    pub stride: u32,
}

/// PCI 配置空间访问（x86 经典 0xCF8/0xCFC 机制）。
mod pci {
    /// 读 32-bit 配置寄存器（bus/dev/fn 均 0 起始，reg 为字节偏移且 4 对齐）。
    pub fn read(bus: u32, dev: u32, func: u32, reg: u32) -> u32 {
        let addr = 0x8000_0000 | (bus << 16) | (dev << 11) | (func << 8) | (reg & 0xFC);
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

    /// 写 32-bit 配置寄存器。
    pub fn write(bus: u32, dev: u32, func: u32, reg: u32, val: u32) {
        let addr = 0x8000_0000 | (bus << 16) | (dev << 11) | (func << 8) | (reg & 0xFC);
        unsafe {
            core::arch::asm!(
                "out dx, eax",
                in("dx") 0xCF8u16,
                in("eax") addr,
                options(nostack, preserves_flags),
            );
            core::arch::asm!(
                "out dx, eax",
                in("dx") 0xCFCu16,
                in("eax") val,
                options(nostack, preserves_flags),
            );
        }
    }
}

/// bochs VBE dispi 寄存器索引。
mod dispi {
    pub const INDEX: u16 = 0x1CE;
    pub const DATA: u16 = 0x1CF;
    pub const IDX_ID: u16 = 0;
    pub const IDX_XRES: u16 = 1;
    pub const IDX_YRES: u16 = 2;
    pub const IDX_BPP: u16 = 3;
    pub const IDX_ENABLE: u16 = 4;
    pub const ID_V4: u16 = 0xB0C4;
    pub const ENABLED: u16 = 0x01;
    pub const LFB: u16 = 0x40;
    pub const NOCLEAR: u16 = 0x20;
    pub const DISABLED: u16 = 0x00;

    pub fn write(idx: u16, val: u16) {
        unsafe {
            core::arch::asm!(
                "out dx, ax",
                in("dx") INDEX,
                in("ax") idx,
                options(nostack, preserves_flags),
            );
            core::arch::asm!(
                "out dx, ax",
                in("dx") DATA,
                in("ax") val,
                options(nostack, preserves_flags),
            );
        }
    }

    pub fn read(idx: u16) -> u16 {
        unsafe {
            core::arch::asm!(
                "out dx, ax",
                in("dx") INDEX,
                in("ax") idx,
                options(nostack, preserves_flags),
            );
            let v: u16;
            core::arch::asm!(
                "in ax, dx",
                in("dx") DATA,
                out("ax") v,
                options(nostack, preserves_flags),
            );
            v
        }
    }
}

/// 目标模式（与动画布局常量匹配，见 `scene`）。
pub const W: u32 = 1024;
/// 目标模式高度。
pub const H: u32 = 768;

/// 枚举 PCI bus 0 找显示控制器并切换 VBE 模式。
///
/// 步骤：class code 0x03xx → BAR0（处理 64-bit BAR）→ 开 Memory Space Enable
/// → dispi 切 1024x768x32 → 回读校验。任一步不满足返回 `None`。
pub fn init() -> Option<FbInfo> {
    let (dev, func, bar0) = find_display()?;

    // 打开 Memory Space 解码（SeaBIOS 通常已开；幂等保险）。
    let cmd = pci::read(0, dev, func, 0x04);
    pci::write(0, dev, func, 0x04, cmd | 0x0002);

    dispi::write(dispi::IDX_ENABLE, dispi::DISABLED);
    dispi::write(dispi::IDX_ID, dispi::ID_V4);
    dispi::write(dispi::IDX_XRES, W as u16);
    dispi::write(dispi::IDX_YRES, H as u16);
    dispi::write(dispi::IDX_BPP, 32);
    dispi::write(dispi::IDX_ENABLE, dispi::ENABLED | dispi::LFB | dispi::NOCLEAR);

    if dispi::read(dispi::IDX_XRES) as u32 != W || dispi::read(dispi::IDX_BPP) as u32 != 32 {
        return None;
    }

    Some(FbInfo {
        ptr: bar0 as *mut u32,
        w: W,
        h: H,
        stride: W,
    })
}

/// 扫描 bus 0 的 32 个 slot，返回 (dev, func, BAR0 基址)。
fn find_display() -> Option<(u32, u32, u64)> {
    for dev in 0..32u32 {
        for func in 0..2u32 {
            let id = pci::read(0, dev, func, 0x00);
            if id == 0xFFFF || id & 0xFFFF == 0 {
                continue;
            }
            let class = pci::read(0, dev, func, 0x08) >> 16;
            if class >> 8 != 0x03 {
                continue;
            }
            if let Some(bar) = bar_mem_base(0, dev, func, 0x10) {
                return Some((dev, func, bar));
            }
        }
    }
    None
}

/// 解析 BAR：跳过 I/O BAR，处理 64-bit BAR 高位拼接。
fn bar_mem_base(bus: u32, dev: u32, func: u32, reg: u32) -> Option<u64> {
    let lo = pci::read(bus, dev, func, reg);
    if lo & 0x1 == 1 {
        return None; // I/O 空间 BAR
    }
    let base_lo = (lo & 0xFFFF_FFF0) as u64;
    if lo & 0x4 == 0x4 {
        let hi = pci::read(bus, dev, func, reg + 4) as u64;
        Some(base_lo | (hi << 32))
    } else {
        Some(base_lo)
    }
}
