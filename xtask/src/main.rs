//! Synapse OS xtask — 构建 / 启动 / CI 工具。
//!
//! # 子命令
//!
//! * `xtask build [--release]` — `cargo build -p synapse-kernel --target x86_64-bootloader.json`
//!   然后 `python build_disk.py <elf> kernel_hd.img`。
//! * `xtask run   [--release]` — `build` + 启动 QEMU（headless，等待 QEMU 自然退出）。
//! * `xtask gui` — 以 `gui_demo` feature + release 构建 + 启动 QEMU（gtk 窗口）。
//!   内核完整播放一遍开机动画后定格、不关机 —— 窗口保留到用户手动关闭。
//!   强制 release：TCG 下 debug 构建帧耗时过长，动画会退化成幻灯片。
//! * `xtask ci    [--timeout N]` — `build` + 启动 QEMU + 轮询 `serial.log`，
//!   在超时内断言出现 `EXPECTED_SERIAL`（默认 10 秒）。
//! * `xtask user  [--release]` — 构建用户态 bin（user/hello，
//!   `x86_64-synapse-user.json` + `user/hello/linker.ld` 基址 1GB）
//!   + 宿主侧 ELF 头断言（ET_EXEC / entry 基址区 / PT_LOAD RX·RW / 无 PT_DYNAMIC）。

mod build;
mod ci;
mod initramfs;
mod qemu;
mod user;

use std::env;
use std::process;

fn main() {
    let mut args = env::args().skip(1);
    let cmd = match args.next() {
        Some(c) => c,
        None => {
            usage();
            process::exit(2);
        }
    };

    let rest: Vec<String> = args.collect();
    let release = rest.iter().any(|a| a == "--release");
    let timeout: u64 = rest
        .windows(2)
        .find(|w| w[0] == "--timeout")
        .and_then(|w| w[1].parse().ok())
        .unwrap_or(10);

    let result: Result<(), String> = match cmd.as_str() {
        "build" => build::run(release),
        "run"   => build::run(release).and_then(|()| qemu::run_headless()),
        // gui 固定 release + gui_demo：内核播放一遍开机动画后定格、不关机，
        // 窗口保留到用户手动关闭（debug 构建在 TCG 下帧太慢，动画会成幻灯片）。
        "gui"   => build::run_with_features(true, &["gui_demo"]).and_then(|()| qemu::run_gui()),
        "ci"    => ci::run(timeout),
        "user"  => user::run(release),
        other   => {
            usage();
            eprintln!("unknown command: {other}");
            process::exit(2);
        }
    };

    if let Err(e) = result {
        eprintln!("[xtask] {e}");
        process::exit(1);
    }
}

fn usage() {
    eprintln!("Usage: xtask <build|run|gui|ci|user> [options]");
    eprintln!("  build [--release]");
    eprintln!("  run   [--release]              headless, 等待 QEMU 退出");
    eprintln!("  gui                          gtk 窗口播放一遍开机动画后定格（固定 release+gui_demo，手动关窗退出）");
    eprintln!("  ci    [--timeout SECONDS]      (default 10)");
    eprintln!("  user  [--release]              构建 user/hello + ELF 头断言 (P4-T1)");
}
