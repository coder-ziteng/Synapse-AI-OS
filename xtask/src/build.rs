//! 构建 pipeline：
//!
//! 1. `cargo build -p synapse-kernel --target <workspace_root>/x86_64-bootloader.json`
//!    → `target/x86_64-bootloader/{debug|release}/synapse-kernel`
//! 2. （P4-T5）用户态 ELF：复用 `xtask user` 管线构建 `user/hello`，
//!    打成 cpio newc initramfs → `target/initramfs.cpio`
//! 3. `python build_disk.py <elf> <workspace_root>/kernel_hd.img <initramfs>`
//!    → 16MB BIOS/MBR bootable 磁盘镜像（stage1 + stage2 + kernel.bin + initramfs）；
//!    stage2 把内核+initramfs 连续加载到 0x200000+，并在物理 0x20100 写
//!    {base u64, size u64} 记录（内核 `initrd.rs` 消费）。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::initramfs::{build_cpio, InitrdFile};

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

    // Step 2 (P4-T5): 构建用户态 ELF（复用 user 管线：build + ELF 头断言），
    // 打成 cpio newc initramfs，写 target/initramfs.cpio。
    crate::user::run(release)?;
    let hello = root
        .join("user")
        .join("hello")
        .join("target")
        .join("x86_64-synapse-user")
        .join(profile)
        .join("hello");
    let hello_bytes = fs::read(&hello)
        .map_err(|e| format!("read user ELF {} failed: {e}", hello.display()))?;
    let archive = build_cpio(&[InitrdFile {
        name: "hello",
        data: &hello_bytes,
    }]);
    let initramfs = root.join("target").join("initramfs.cpio");
    if let Some(parent) = initramfs.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir target failed: {e}"))?;
    }
    fs::write(&initramfs, &archive)
        .map_err(|e| format!("write {} failed: {e}", initramfs.display()))?;
    println!(
        "[xtask] initramfs OK -> {} ({} bytes, 1 file: hello={})",
        initramfs.display(),
        archive.len(),
        hello_bytes.len()
    );

    // Step 3: python build_disk.py <elf> <img> <initramfs>
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
        .arg(&initramfs)
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
