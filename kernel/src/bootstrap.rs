//! 内核启动引导（cap/ipc/proc 集成层落地）。
//!
//! 流程：
//! 1. [`kstate::kstate_init`]——5 张表就地构造（`ProcessTable::new` 自带
//!    init 进程 pid=1, Running, `INIT_QUOTA`，agent_id = 1）。
//! 2. 为 init 建 [`CapTable`]（slot 0 NULL trap 由 crate 内建）。
//! 3. 分配两个内核对象（Endpoint + Notification）并铸造根 capability：
//!    * Endpoint：`SEND | RECV | REPLY | GRANT`（`GRANT` 供 spawn 授予，
//!      `REPLY` 留给 Phase 2 RPC 链路）；
//!    * Notification：`SEND | RECV`（同步收发均可）。
//!
//! 每步 `log::info!` 输出 `ObjRef` / `CapRef`，定位集成层任意一步失败点。
//!
//! 返回的 [`BootstrapRefs`] 含 init 持有的根 cap 槽位（`smoke` 用作
//! `death_endpoint` 与通知位图测试）。

use log::info;

use synapse_cap::{CapError, CapRef, Rights};

use crate::kstate;

// 开机视觉序列（原生帧缓冲动画，规格见 synapse-aios/boot-animation）。
// 模块挂在 bootstrap 下而非 main.rs：main.rs 正被他窗并行修改（P3 线程基建），
// 钩子放此干净文件以避免跨窗口合并冲突；播放时机 = cap/ipc bootstrap 之前。
#[path = "bootanim/mod.rs"]
pub mod bootanim;

/// bootstrap 产物：init 进程持有的根 capability 槽位。
pub struct BootstrapRefs {
    /// init 的 IPC endpoint 根 cap（用于 `SpawnParams::death_endpoint`）。
    pub ep_cap: CapRef,
    /// init 的 Notification 根 cap（位图语义通知）。
    pub no_cap: CapRef,
}

/// 内核对象引导：初始化表 + 铸造 init 的根 caps。
///
/// 重复调用 panic（[`kstate::kstate_init`] 自身有 double-init guard）。
// gui_demo 构建下 `run_forever()` 发散（动画无限循环），其后代码不可达属预期。
#[allow(unreachable_code)]
pub fn kernel_bootstrap() -> BootstrapRefs {
    // GUI 演示模式（cargo feature `gui_demo`，由 `xtask gui` 启用）：
    // 无限循环播放开机动画，永不返回——不进 smoke、不触发 isa-debug-exit 关机，
    // QEMU 窗口保留到用户手动关闭。无头构建（run/ci）不带此 feature，行为不变。
    #[cfg(feature = "gui_demo")]
    bootanim::run_forever();

    // 开机动画（正常启动路径）：P3 线程基建期间临时禁用——bootanim 模块把内核
    // 膨胀到 ~12MB，4MB E820 usable 区间只剩 ~2MB 给栈/堆，kthread 栈被分配到
    // 低 640K 触发 #DF/#UD（P3-T7 mutex smoke 调试发现）。S6 显示 preresearch
    // 完成后再开。
    // bootanim::run();

    info!("[bootstrap] step 1/3: kstate_init");
    kstate::kstate_init();

    info!("[bootstrap] step 2/3: create init CapTable");
    kstate::k_create_cap_table(kstate::INIT)
        .expect("init CapTable slot already occupied");

    info!("[bootstrap] step 3/3: mint root capabilities");
    let ep_obj = kstate::k_alloc_object(synapse_cap::ObjKind::Endpoint)
        .expect("alloc endpoint obj");
    let ep_cap = kstate::k_mint_root(
        kstate::INIT,
        ep_obj,
        Rights::SEND | Rights::RECV | Rights::REPLY | Rights::GRANT,
    )
    .expect("mint endpoint root cap");
    info!(
        "[bootstrap]   ep  obj={}:{} cap={}",
        ep_obj.index, ep_obj.generation, ep_cap
    );

    let no_obj = kstate::k_alloc_object(synapse_cap::ObjKind::Notification)
        .expect("alloc notification obj");
    let no_cap = kstate::k_mint_root(
        kstate::INIT,
        no_obj,
        Rights::SEND | Rights::RECV,
    )
    .expect("mint notification root cap");
    info!(
        "[bootstrap]   no  obj={}:{} cap={}",
        no_obj.index, no_obj.generation, no_cap
    );

    info!("[bootstrap] done");
    BootstrapRefs { ep_cap, no_cap }
}

/// 编译期保证 [`CapError`] 错误类型被使用（kstate helpers 返回）。
#[allow(dead_code)]
const _: fn() -> Result<(), CapError> = || Ok(());
