//! 构建 pipeline：
//!
//! 1. `cargo build -p synapse-kernel --target <workspace_root>/x86_64-bootloader.json`
//!    → `target/x86_64-bootloader/{debug|release}/synapse-kernel`
//! 2. `python build_disk.py <elf> <workspace_root>/kernel_hd.img`
//!    → 16MB BIOS/MBR bootable 磁盘镜像（stage1 + stage2 + kernel.bin）

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// workspace 根目录（xtask crate 在 `<root>/xtask/`，祖父即 root）。
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask manifest has parent")
        .to_path_buf()
}

/// 执行构建 pipeline。`release` 控制是否 `--release`。
pub fn run(release: bool) -> Result<(), String> {
    let root = workspace_root();
    let target_json = root.join("x86_64-bootloader.json");
    let profile = if release { "release" } else { "debug" };

    // Step 1: cargo build
    let mut cmd = Command::new("cargo");
    cmd.current_dir(&root);
    cmd.arg("build");
    cmd.arg("-p").arg("synapse-kernel");
    cmd.arg("--target").arg(&target_json);
    // 近期 nightly cargo 要求 JSON target spec 必须显式开启
    cmd.arg("-Zjson-target-spec");
    // 自定义 bare-metal target 没有预编译 core；用 rust-src 现场构建
    // 只构建 core + compiler_builtins + alloc（std 无法为 os=none 构建）
    // P2-T3 起内核启用了 `extern crate alloc`，必须一并构建 alloc
    cmd.arg("-Zbuild-std=core,compiler_builtins,alloc");
    if release {
        cmd.arg("--release");
    }

    let status = cmd.status().map_err(|e| format!("cargo build spawn failed: {e}"))?;
    if !status.success() {
        return Err(format!("cargo build failed: {status}"));
    }

    // Step 2: python build_disk.py <elf> <img>
    let elf = root
        .join("target")
        .join("x86_64-bootloader")
        .join(profile)
        .join("synapse-kernel");
    let img = root.join("kernel_hd.img");

    // Windows 上 Python 可执行文件名是 `python`；POSIX 上是 `python3`。
    let py = if cfg!(windows) { "python" } else { "python3" };

    let status = Command::new(py)
        .current_dir(&root)
        .arg("build_disk.py")
        .arg(&elf)
        .arg(&img)
        .status()
        .map_err(|e| format!("{py} spawn failed: {e}"))?;
    if !status.success() {
        return Err(format!("build_disk.py failed: {status}"));
    }

    println!("[xtask] build OK -> {}", img.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_root_points_to_repo_root() {
        let root = workspace_root();
        // 根目录必须有 `Cargo.toml` 且声明 `kernel` 在 members 里
        assert!(root.join("Cargo.toml").exists(), "root missing Cargo.toml");
        let content = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
        assert!(content.contains("\"kernel\""), "workspace should list kernel member");
    }
}
