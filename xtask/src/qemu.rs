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

/// 日志目录（`<root>/logs`），不存在时创建。
///
/// 所有运行时日志（serial/debugcon）统一写入此目录，禁止散落到项目根目录。
fn logs_dir() -> PathBuf {
    let dir = workspace_root().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 内核成功路径的 isa-debug-exit 出口（对照 355=panic 出口）。机制详见 decision_log。
const KERNEL_SUCCESS_EXIT: i32 = 363;

/// QEMU 退出状态是否为"成功"：进程退出码 0，或内核成功出口 [`KERNEL_SUCCESS_EXIT`]。
pub(crate) fn is_success_status(status: &std::process::ExitStatus) -> bool {
    status.success() || status.code() == Some(KERNEL_SUCCESS_EXIT)
}

/// GUI 模式退出是否成功：除正常出口外，用户手动关闭 GTK 窗口时 QEMU
/// 在不同内存配置 / 平台下可能返回 -1（非 isa-debug-exit 触发），视为正常。
fn is_gui_success(status: &std::process::ExitStatus) -> bool {
    is_success_status(status) || status.code() == Some(-1)
}

/// 退出码的可读形式（被信号杀死时无退出码）。
fn exit_code_str(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(c) => c.to_string(),
        None => "unknown(signal)".into(),
    }
}

/// 打印 QEMU 退出码并判定成功/失败。startAIOS.ps1 靠解析 stdout 中的
/// `[xtask] QEMU exited with exit code: N` 行判定成败（363/355），不能只在错误路径输出。
fn report_exit(status: &std::process::ExitStatus) -> Result<(), String> {
    let code = exit_code_str(status);
    println!("[xtask] QEMU exited with exit code: {code}");
    if is_success_status(status) {
        Ok(())
    } else {
        Err(format!(
            "QEMU abnormal exit {code} (expected 0 or {KERNEL_SUCCESS_EXIT}=kernel success)"
        ))
    }
}

/// GUI 模式退出判定：接受 -1（用户关窗）。
fn report_gui_exit(status: &std::process::ExitStatus) -> Result<(), String> {
    let code = exit_code_str(status);
    println!("[xtask] QEMU exited with exit code: {code}");
    if is_gui_success(status) {
        Ok(())
    } else {
        Err(format!(
            "QEMU abnormal exit {code} (expected 0, {KERNEL_SUCCESS_EXIT}=kernel success, or -1=window closed)"
        ))
    }
}

/// 构造 QEMU 命令行参数。
///
/// 关键参数说明：
///
/// | 参数                                | 用途                                                   |
/// | ----------------------------------- | ------------------------------------------------------ |
/// | `-drive file=kernel_hd.img,format=raw` | 启动盘（build_disk.py 产物）                         |
/// | `-display none` / `-display gtk`    | 无头模式 / 有窗口模式                                  |
/// | `-serial file:serial.log`           | COM1 (UART 16550 @ 0x3F8) 输出捕获到文件               |
/// | `-device isa-debugcon,iobase=0x402` | stage1/stage2 追踪字符（build_disk.py 写入）           |
/// | `-device isa-debugcon,iobase=0x501` | kernel `_start64` 入口追踪字符                         |
/// | `-no-reboot`                        | triple fault 时 QEMU 退出（exit != 0），而非重启       |
/// | `-monitor none`                     | 禁用 monitor（避免额外 socket 残留）                   |
/// | `-m 1024M`                          | 内存；足够 4GB 恒等映射页表 + 内核                     |
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
        "-device".into(),
        "isa-debug-exit,iobase=0x502".into(),
        "-no-reboot".into(),
        "-monitor".into(),
        "none".into(),
        "-m".into(),
        "1024M".into(),
        "-cpu".into(),
        "qemu64".into(),
    ]
}

/// 有窗口模式：保留 serial 日志，但显示 VBE 帧缓冲。
pub fn build_args_gui(serial_log: &Path, dc402: &Path, dc501: &Path) -> Vec<String> {
    let mut args = build_args(serial_log, dc402, dc501);
    // 把 "-display" "none" 替换为 "-display" "gtk"
    for i in 0..args.len() {
        if args[i] == "none" && i > 0 && args[i - 1] == "-display" {
            args[i] = "gtk".into();
            break;
        }
    }
    args
}

/// 无头启动 QEMU 并**等待退出**。用于 `xtask run`（用户想看到完整日志后再返回 shell）。
pub fn run_headless() -> Result<(), String> {
    let logs = logs_dir();
    // 使用时间戳避免文件锁定冲突
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let serial = logs.join(format!("serial-{}.log", ts));
    let dc402 = logs.join(format!("debugcon-stage12-{}.log", ts));
    let dc501 = logs.join(format!("debugcon-kernel-{}.log", ts));

    let args = build_args(&serial, &dc402, &dc501);
    println!("[xtask] QEMU: qemu-system-x86_64 {}", args.join(" "));
    println!("[xtask] Logs: serial={}, dc402={}, dc501={}", serial.display(), dc402.display(), dc501.display());

    let status = Command::new("qemu-system-x86_64")
        .args(&args)
        .status()
        .map_err(|e| format!("qemu spawn failed: {e}"))?;

    report_exit(&status)
}

/// 启动 QEMU 并返回子进程句柄（不等待）。用于 `xtask ci`（需要轮询 serial log + 超时杀进程）。
pub fn spawn_headless() -> Result<Child, String> {
    let logs = logs_dir();
    let serial = logs.join("serial.log");
    let dc402 = logs.join("debugcon-stage12.log");
    let dc501 = logs.join("debugcon-kernel.log");

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

/// 有窗口启动 QEMU 并**等待退出**。
///
/// 与 `run_headless` 相比，`-display none` 替换为 `-display gtk`，
/// 保留 serial 日志文件，适合本地观察 VBE 开机动画。
pub fn run_gui() -> Result<(), String> {
    let logs = logs_dir();
    let serial = logs.join("serial.log");
    let dc402 = logs.join("debugcon-stage12.log");
    let dc501 = logs.join("debugcon-kernel.log");

    let _ = std::fs::remove_file(&serial);
    let _ = std::fs::remove_file(&dc402);
    let _ = std::fs::remove_file(&dc501);

    let args = build_args_gui(&serial, &dc402, &dc501);
    println!("[xtask] QEMU: qemu-system-x86_64 {}", args.join(" "));

    let status = Command::new("qemu-system-x86_64")
        .args(&args)
        .status()
        .map_err(|e| format!("qemu spawn failed: {e}"))?;

    report_gui_exit(&status)
}
