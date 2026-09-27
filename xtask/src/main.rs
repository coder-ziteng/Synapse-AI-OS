//! Synapse OS xtask — 构建 / 启动 / CI 工具。
//!
//! # 子命令
//!
//! * `xtask build [--release]` — `cargo build -p synapse-kernel --target x86_64-bootloader.json`
//!   然后 `python build_disk.py <elf> kernel_hd.img`。
//! * `xtask run   [--release]` — `build` + 启动 QEMU（headless，等待 QEMU 自然退出）。
//! * `xtask gui   [--release]` — `build` + 启动 QEMU（gtk 窗口显示 VBE 帧缓冲）。
//! * `xtask ci    [--timeout N]` — `build` + 启动 QEMU + 轮询 `serial.log`，
//!   在超时内断言出现 `EXPECTED_SERIAL`（默认 10 秒）。

mod build;
mod ci;
mod qemu;

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
        "gui"   => build::run(release).and_then(|()| qemu::run_gui()),
        "ci"    => ci::run(timeout),
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
    eprintln!("Usage: xtask <build|run|gui|ci> [options]");
    eprintln!("  build [--release]");
    eprintln!("  run   [--release]              headless, 等待 QEMU 退出");
    eprintln!("  gui   [--release]              gtk 窗口显示 VBE 帧缓冲");
    eprintln!("  ci    [--timeout SECONDS]      (default 10)");
}
