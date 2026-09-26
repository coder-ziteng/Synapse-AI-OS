//! Synapse OS xtask — 构建 / 启动 / CI 工具。
//!
//! # 子命令
//!
//! * `xtask build [--release]` — `cargo build -p synapse-kernel --target x86_64-bootloader.json`
//!   然后 `python build_disk.py <elf> kernel_hd.img`。
//! * `xtask run   [--release]` — `build` + 启动 QEMU（headless，等待 QEMU 自然退出）。
//! * `xtask ci    [--timeout N]` — `build` + 启动 QEMU + 轮询 `serial.log`，
//!   在超时内断言出现 `EXPECTED_SERIAL`（默认 10 秒）。
//! * `xtask user  [--release]` — 构建用户态 bin（user/hello，
//!   `x86_64-synapse-user.json` + `user/linker.ld` 基址 0x400000）
//!   + 宿主侧 ELF 头断言（ET_EXEC / entry 基址区 / PT_LOAD RX·RW / 无 PT_DYNAMIC）。

mod build;
mod ci;
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
    eprintln!("Usage: xtask <build|run|ci|user> [options]");
    eprintln!("  build [--release]");
    eprintln!("  run   [--release]");
    eprintln!("  ci    [--timeout SECONDS]   (default 10)");
    eprintln!("  user  [--release]           构建 user/hello + ELF 头断言 (P4-T1)");
}
