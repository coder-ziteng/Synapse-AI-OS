//! PIC (8259A) 中断控制器实现。
//!
//! x86 架构使用两个级联的 8259A PIC 芯片处理硬件中断：
//! - Master PIC: IRQ 0-7 (映射到 IDT 32-39)
//! - Slave PIC: IRQ 8-15 (映射到 IDT 40-47)
//!
//! PIC 偏移量配置：
//! - Master: offset = 0x20 (32)，避开 CPU 异常 (0-31)
//! - Slave: offset = 0x28 (40)
//!
//! 初始化序列 (ICW1-ICW4):
//! 1. ICW1: 初始化命令 (0x11)，级联模式，需要 ICW4
//! 2. ICW2: 中断向量偏移量 (master=0x20, slave=0x28)
//! 3. ICW3: 级联配置 (master: slave on IRQ2=0x04, slave: cascade ID=2)
//! 4. ICW4: 操作模式 (8086 mode=0x01)

use core::cell::UnsafeCell;

use x86_64::instructions::port::Port;

/// PIC I/O 端口
const PIC1_COMMAND: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_COMMAND: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

/// PIC 命令字
const PIC_EOI: u8 = 0x20; // End of interrupt
const ICW1_INIT: u8 = 0x11; // Initialization + ICW4 needed
const ICW4_8086: u8 = 0x01; // 8086/88 mode

/// IRQ 偏移量 (避开 CPU 异常 0-31)
pub const IRQ_OFFSET: u8 = 32;

/// PIC 控制器
pub struct Pic {
    command: Port<u8>,
    data: Port<u8>,
}

impl Pic {
    /// 检查是否处理该 IRQ
    fn handles_irq(&self, irq: u8) -> bool {
        irq < 8
    }

    /// 发送 EOI (End of Interrupt)
    unsafe fn send_eoi(&mut self, irq: u8) {
        if self.handles_irq(irq) {
            self.command.write(PIC_EOI);
        }
    }

    /// 初始化 PIC
    unsafe fn init(&mut self, offset: u8) {
        // ICW1: 初始化 + 需要 ICW4
        self.command.write(ICW1_INIT);

        // ICW2: 中断向量偏移量
        self.data.write(offset);

        // ICW3: 级联配置
        if offset == IRQ_OFFSET {
            // Master: slave 连接到 IRQ2
            self.data.write(0x04);
        } else {
            // Slave: 级联 ID = 2
            self.data.write(0x02);
        }

        // ICW4: 8086 模式
        self.data.write(ICW4_8086);
    }

    /// 设置中断屏蔽
    unsafe fn set_mask(&mut self, irq: u8, mask: bool) {
        let current = self.data.read();

        if mask {
            // 屏蔽：设置对应位为 1
            self.data.write(current | (1 << irq));
        } else {
            // 解除屏蔽：设置对应位为 0
            self.data.write(current & !(1 << irq));
        }
    }

    /// 读取当前中断服务寄存器 (ISR)
    unsafe fn get_isr(&mut self) -> u8 {
        self.command.write(0x0B); // OCW3: Read ISR
        self.command.read()
    }
}

/// 双 PIC 系统 (Master + Slave)
pub struct DualPic {
    master: Pic,
    slave: Pic,
}

impl DualPic {
    /// 创建双 PIC 实例
    pub const fn new() -> Self {
        DualPic {
            master: Pic {
                command: Port::new(PIC1_COMMAND),
                data: Port::new(PIC1_DATA),
            },
            slave: Pic {
                command: Port::new(PIC2_COMMAND),
                data: Port::new(PIC2_DATA),
            },
        }
    }

    /// 初始化双 PIC
    pub unsafe fn init(&mut self) {
        self.master.init(IRQ_OFFSET);
        self.slave.init(IRQ_OFFSET + 8);

        // 默认屏蔽所有 IRQ，由驱动按需解除
        self.master.data.write(0xFF);
        self.slave.data.write(0xFF);
    }

    /// 发送 EOI
    pub unsafe fn send_eoi(&mut self, irq: u8) {
        if irq >= 8 {
            // Slave IRQ: 两个 PIC 都需要 EOI
            self.slave.send_eoi(irq - 8);
            self.master.send_eoi(2); // Cascade IRQ
        } else {
            // Master IRQ: 只需 master EOI
            self.master.send_eoi(irq);
        }
    }

    /// 解除指定 IRQ 的屏蔽
    pub unsafe fn enable_irq(&mut self, irq: u8) {
        if irq < 8 {
            self.master.set_mask(irq, false);
        } else {
            // 同时解除 master 的 IRQ2 (cascade) 屏蔽
            self.master.set_mask(2, false);
            self.slave.set_mask(irq - 8, false);
        }
    }

    /// 屏蔽指定 IRQ
    pub unsafe fn disable_irq(&mut self, irq: u8) {
        if irq < 8 {
            self.master.set_mask(irq, true);
        } else {
            self.slave.set_mask(irq - 8, true);
        }
    }

    /// 获取当前中断服务寄存器
    pub unsafe fn get_isr(&mut self) -> u16 {
        let master_isr = self.master.get_isr() as u16;
        let slave_isr = self.slave.get_isr() as u16;
        (slave_isr << 8) | master_isr
    }
}

/// 全局 PIC 实例（`UnsafeCell` 包装，避免 `static mut` 引用导致的 UB 警告）。
///
/// 单核 MVP 下：`init()` 在开中断前调用一次；之后 `send_eoi`/`enable_irq`/`disable_irq`
/// 只在中断关闭或中断上下文（EOI）中调用，不存在真正的并发数据竞争。
struct PicCell(UnsafeCell<DualPic>);
unsafe impl Sync for PicCell {}

static PIC: PicCell = PicCell(UnsafeCell::new(DualPic::new()));

/// 初始化 PIC
pub unsafe fn init() {
    (*PIC.0.get()).init();
    log::info!("[pic] PIC initialized: master offset=0x{:02x}, slave offset=0x{:02x}",
               IRQ_OFFSET, IRQ_OFFSET + 8);
}

/// 发送 EOI
pub unsafe fn send_eoi(irq: u8) {
    (*PIC.0.get()).send_eoi(irq);
}

/// 解除 IRQ 屏蔽
pub unsafe fn enable_irq(irq: u8) {
    (*PIC.0.get()).enable_irq(irq);
}

/// 屏蔽 IRQ
pub unsafe fn disable_irq(irq: u8) {
    (*PIC.0.get()).disable_irq(irq);
}
