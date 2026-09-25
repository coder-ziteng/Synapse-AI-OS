//! QEMU 启动参数固化。
//!
//! 所有本地 `xtask run` 与 CI `xtask ci` 共用同一份参数生成逻辑，
//! 确保"本地能跑 = CI 能跑"，参数只写一次。

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

/// workspace 根目录。
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask manifest has parent")
        .to_path_buf()
}

/// 构造 QEMU 命令行参数。
///
/// 关键参数说明：
///
/// | 参数                                | 用途                                                   |
/// | ----------------------------------- | ------------------------------------------------------ |
/// | `-drive file=kernel_hd.img,format=raw` | 启动盘（build_disk.py 产物）                         |
/// | `-display none`                     | 无头模式                                               |
/// | `-serial file:serial.log`           | COM1 (UART 16550 @ 0x3F8) 输出捕获到文件               |
/// | `-device isa-debugcon,iobase=0x402` | stage1/stage2 追踪字符（build_disk.py 写入）           |
/// | `-device isa-debugcon,iobase=0x501` | kernel `_start64` 入口追踪字符                         |
/// | `-no-reboot`                        | triple fault 时 QEMU 退出（exit != 0），而非重启       |
/// | `-monitor none`                     | 禁用 monitor（避免额外 socket 残留）                   |
/// | `-m 128M`                           | 内存；足够 4GB 恒等映射页表 + 内核                     |
/// | `-cpu qemu64`                       | 最小可用 x86_64 CPU（无特殊 feature 依赖）             |
pub fn build_args(serial_log: &Path, dc402: &Path, dc501: &Path) -> Vec<String> {
    let root = workspace_root();
    let img = root.join("kernel_hd.img");

    vec![
        "-drive".into(),
        format!("file={},format=raw", img.display()),
        "-display".into(),
        "none".into(),
        "-serial".into(),
        format!("file:{}", serial_log.display()),
        "-device".into(),
        "isa-debugcon,iobase=0x402,chardev=dc402".into(),
        "-chardev".into(),
        format!("file,id=dc402,path={}", dc402.display()),
        "-device".into(),
        "isa-debugcon,iobase=0x501,chardev=dc501".into(),
        "-chardev".into(),
        format!("file,id=dc501,path={}", dc501.display()),
        "-no-reboot".into(),
        "-monitor".into(),
        "none".into(),
        "-m".into(),
        "128M".into(),
        "-cpu".into(),
        "qemu64".into(),
    ]
}

/// 无头启动 QEMU 并**等待退出**。用于 `xtask run`（用户想看到完整日志后再返回 shell）。
pub fn run_headless() -> Result<(), String> {
    let root = workspace_root();
    let serial = root.join("serial.log");
    let dc402 = root.join("debugcon-stage12.log");
    let dc501 = root.join("debugcon-kernel.log");

    // 清理旧日志，避免误读上一轮残留
    let _ = std::fs::remove_file(&serial);
    let _ = std::fs::remove_file(&dc402);
    let _ = std::fs::remove_file(&dc501);

    let args = build_args(&serial, &dc402, &dc501);
    println!("[xtask] QEMU: qemu-system-x86_64 {}", args.join(" "));

    let status = Command::new("qemu-system-x86_64")
        .args(&args)
        .status()
        .map_err(|e| format!("qemu spawn failed: {e}"))?;

    if !status.success() {
        return Err(format!("QEMU exited with {status}"));
    }
    Ok(())
}

/// 启动 QEMU 并返回子进程句柄（不等待）。用于 `xtask ci`（需要轮询 serial log + 超时杀进程）。
pub fn spawn_headless() -> Result<Child, String> {
    let root = workspace_root();
    let serial = root.join("serial.log");
    let dc402 = root.join("debugcon-stage12.log");
    let dc501 = root.join("debugcon-kernel.log");

    let _ = std::fs::remove_file(&serial);
    let _ = std::fs::remove_file(&dc402);
    let _ = std::fs::remove_file(&dc501);

    let args = build_args(&serial, &dc402, &dc501);
    println!("[xtask] QEMU: qemu-system-x86_64 {}", args.join(" "));

    Command::new("qemu-system-x86_64")
        .args(&args)
        .spawn()
        .map_err(|e| format!("qemu spawn failed: {e}"))
}
