//! 用户态构建 pipeline（P4-T1）：
//!
//! 1. `cargo build --manifest-path user/hello/Cargo.toml
//!        --target <root>/x86_64-synapse-user.json
//!        -Zjson-target-spec -Zbuild-std=core,alloc`
//!    → `user/hello/target/x86_64-synapse-user/{debug|release}/hello`
//!    （cwd 必须是仓库根：target json 的 `--script=user/linker.ld`
//!    由 ld.lld 相对 cargo cwd 解析。）
//! 2. 宿主侧 ELF 头断言（task.json P4-T1 verify）：
//!    * `e_type == ET_EXEC`（非 PIE，Doc 02 §2.1）
//!    * `e_entry` 落在 1GB 基址区 [0x4000_0000, 0x8000_0000)（Doc 02 §3.1 UPDATE(P4-T2)）
//!    * PT_LOAD flags：text=RX(5)、data=RW(6)，W^X 分段
//!    * 无 PT_DYNAMIC（砍动态链接）
//!
//! 解析器是纯字节切片逻辑 → 宿主单测用手工构造的 ELF 字节验证
//! （含畸形输入拒绝路径）。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// workspace 根目录（与 build.rs 同款；不共用是避免动 build.rs 引入并行窗口冲突面）。
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask manifest has parent")
        .to_path_buf()
}

/// 执行用户态构建 + ELF 校验。`release` 控制是否 `--release`。
pub fn run(release: bool) -> Result<(), String> {
    let root = workspace_root();
    let target_json = root.join("x86_64-synapse-user.json");
    let manifest = root.join("user").join("hello").join("Cargo.toml");
    let profile = if release { "release" } else { "debug" };

    let mut cmd = Command::new("cargo");
    cmd.current_dir(&root); // 关键：--script=user/linker.ld 相对 cwd 解析
    cmd.arg("build");
    cmd.arg("--manifest-path").arg(&manifest);
    cmd.arg("--target").arg(&target_json);
    cmd.arg("-Zjson-target-spec");
    // 用户态首期无 std（Doc 02 §1.2）：只建 core + alloc
    cmd.arg("-Zbuild-std=core,alloc");
    if release {
        cmd.arg("--release");
    }

    let status = cmd
        .status()
        .map_err(|e| format!("cargo build (user) spawn failed: {e}"))?;
    if !status.success() {
        return Err(format!("cargo build (user) failed: {status}"));
    }

    let elf = root
        .join("user")
        .join("hello")
        .join("target")
        .join("x86_64-synapse-user")
        .join(profile)
        .join("hello");
    let bytes =
        fs::read(&elf).map_err(|e| format!("read ELF {} failed: {e}", elf.display()))?;
    let summary = verify_user_elf(&bytes)?;

    println!("[xtask] user build OK -> {}", elf.display());
    println!("[xtask] ELF verify: {summary}");
    Ok(())
}

// ---------------------------------------------------------------------------
// ELF64 静态断言（宿主侧）
// ---------------------------------------------------------------------------

const ET_EXEC: u16 = 2;
const EM_X86_64: u16 = 0x3E;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;

const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;
/// 期望 text 段 flags：R+X。
const FLAGS_RX: u32 = PF_R | PF_X;
/// 期望 data 段 flags：R+W（W^X：可写段不得可执行）。
const FLAGS_RW: u32 = PF_R | PF_W;

/// 用户态基址（Doc 02 §3.1 UPDATE(P4-T2)：1GB。原 PROPOSED 0x400000 与
/// 内核 2MB 大页恒等映射的自身物理页 [0x200000,0x4cb000) 同 VA 冲突而废弃；
/// 1GB 处 PA 1-2GB 无物理内存，每地址空间独立 PD 与内核映射零重叠）。
pub const USER_BASE: u64 = 0x4000_0000;
/// 用户 text 基址区上限（断言 e_entry 落在 [USER_BASE, USER_ENTRY_LIMIT)，
/// 即 PDPT[0] entry 1 所辖的 1GB VA 区内）。
pub const USER_ENTRY_LIMIT: u64 = 0x8000_0000;

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u64le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes([
        b[o], b[o + 1], b[o + 2], b[o + 3], b[o + 4], b[o + 5], b[o + 6], b[o + 7],
    ])
}

/// 校验用户态 ELF：返回摘要字符串或第一条违规原因。
///
/// 只做 P4-T1 需要的最小断言（见模块头）；完整加载语义在 P4-T5 内核侧
/// ELF 解析器 crate 实现（那边还有畸形输入的宿主测试）。
fn verify_user_elf(b: &[u8]) -> Result<String, String> {
    if b.len() < 64 {
        return Err(format!("file too small for ELF64 header ({} bytes)", b.len()));
    }
    if b[0..4] != [0x7f, b'E', b'L', b'F'] {
        return Err("bad ELF magic".into());
    }
    if b[4] != 2 {
        return Err(format!("not ELFCLASS64 (ei_class={})", b[4]));
    }
    if b[5] != 1 {
        return Err(format!("not little-endian (ei_data={})", b[5]));
    }

    let e_type = u16le(b, 16);
    let e_machine = u16le(b, 18);
    let e_entry = u64le(b, 24);
    let e_phoff = u64le(b, 32) as usize;
    let e_phentsize = u16le(b, 54) as usize;
    let e_phnum = u16le(b, 56) as usize;

    if e_type != ET_EXEC {
        return Err(format!("e_type={e_type}, expected ET_EXEC(2) (无 PIE)"));
    }
    if e_machine != EM_X86_64 {
        return Err(format!("e_machine=0x{e_machine:x}, expected x86-64(0x3e)"));
    }
    if !(USER_BASE..USER_ENTRY_LIMIT).contains(&e_entry) {
        return Err(format!(
            "e_entry=0x{e_entry:x} outside base region [0x{USER_BASE:x}, 0x{USER_ENTRY_LIMIT:x})"
        ));
    }
    if e_phentsize != 56 {
        return Err(format!("e_phentsize={e_phentsize}, expected 56 (ELF64)"));
    }
    if e_phoff + e_phnum * 56 > b.len() {
        return Err("program headers out of file bounds".into());
    }

    let mut loads: Vec<(u32, u64, u64)> = Vec::new(); // (flags, vaddr, memsz)
    let mut entry_covered_by_rx = false;
    for i in 0..e_phnum {
        let ph = e_phoff + i * 56;
        let p_type = u32le(b, ph);
        let p_flags = u32le(b, ph + 4);
        let p_vaddr = u64le(b, ph + 16);
        let p_memsz = u64le(b, ph + 40);

        if p_type == PT_DYNAMIC {
            return Err("PT_DYNAMIC present (应为静态 ELF，砍动态链接)".into());
        }
        if p_type != PT_LOAD {
            continue;
        }
        if p_vaddr < USER_BASE {
            return Err(format!(
                "PT_LOAD vaddr=0x{p_vaddr:x} below user base 0x{USER_BASE:x} (NULL guard 区)"
            ));
        }
        if p_vaddr <= e_entry && e_entry < p_vaddr + p_memsz {
            entry_covered_by_rx = p_flags & PF_X != 0;
        }
        loads.push((p_flags, p_vaddr, p_memsz));
    }

    if loads.is_empty() {
        return Err("no PT_LOAD segments".into());
    }
    if !entry_covered_by_rx {
        return Err(format!("e_entry=0x{e_entry:x} not inside an executable PT_LOAD"));
    }
    for &(flags, vaddr, _) in &loads {
        // W^X：不允许同时可写可执行；也不允许不可读
        if flags & PF_W != 0 && flags & PF_X != 0 {
            return Err(format!("PT_LOAD@0x{vaddr:x} is W+X (flags=0x{flags:x})"));
        }
        if flags & PF_R == 0 {
            return Err(format!("PT_LOAD@0x{vaddr:x} not readable (flags=0x{flags:x})"));
        }
        if flags != FLAGS_RX && flags != FLAGS_RW {
            return Err(format!(
                "PT_LOAD@0x{vaddr:x} flags=0x{flags:x}, expected RX(0x5) or RW(0x6)"
            ));
        }
    }

    let summary = loads
        .iter()
        .map(|&(f, v, m)| format!("[{v:#x} memsz={m:#x} flags={f:#x}]"))
        .collect::<Vec<_>>()
        .join(" ");
    Ok(format!(
        "ET_EXEC entry=0x{e_entry:x} {} PT_LOAD: {summary}",
        loads.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手工构造最小合法用户态 ELF 字节（header + text RX + data RW）。
    fn synth_elf(e_type: u16, e_entry: u64, extra_ph: Option<(u32, u32, u64)>) -> Vec<u8> {
        let phnum: u16 = if extra_ph.is_some() { 3 } else { 2 };
        let mut b = vec![0u8; 64 + 56 * phnum as usize];
        b[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        b[4] = 2; // ELFCLASS64
        b[5] = 1; // ELFDATA2LSB
        b[6] = 1; // EV_CURRENT
        b[16..18].copy_from_slice(&e_type.to_le_bytes());
        b[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
        b[20..24].copy_from_slice(&1u32.to_le_bytes());
        b[24..32].copy_from_slice(&e_entry.to_le_bytes());
        b[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
        b[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
        b[56..58].copy_from_slice(&phnum.to_le_bytes()); // e_phnum

        let mut ph = |i: usize, p_type: u32, p_flags: u32, p_vaddr: u64, p_memsz: u64| {
            let o = 64 + i * 56;
            b[o..o + 4].copy_from_slice(&p_type.to_le_bytes());
            b[o + 4..o + 8].copy_from_slice(&p_flags.to_le_bytes());
            b[o + 16..o + 24].copy_from_slice(&p_vaddr.to_le_bytes());
            b[o + 40..o + 48].copy_from_slice(&p_memsz.to_le_bytes());
        };
        ph(0, PT_LOAD, FLAGS_RX, USER_BASE, 0x2000);
        ph(1, PT_LOAD, FLAGS_RW, USER_BASE + 0x2000, 0x1000);
        if let Some((t, f, v)) = extra_ph {
            ph(2, t, f, v, 0x100);
        }
        b
    }

    #[test]
    fn valid_static_user_elf_passes() {
        let b = synth_elf(ET_EXEC, USER_BASE + 0x78, None);
        let s = verify_user_elf(&b).expect("should pass");
        assert!(s.contains("ET_EXEC") && s.contains("2 PT_LOAD"), "{s}");
    }

    #[test]
    fn rejects_non_exec_and_bad_machine() {
        let b = synth_elf(3 /*ET_DYN*/, USER_BASE, None);
        assert!(verify_user_elf(&b).unwrap_err().contains("ET_EXEC"));
        let mut b = synth_elf(ET_EXEC, USER_BASE, None);
        b[18..20].copy_from_slice(&0xB7u16.to_le_bytes()); // aarch64，非 x86-64
        assert!(verify_user_elf(&b).unwrap_err().contains("machine"));
    }

    #[test]
    fn rejects_entry_outside_base_region() {
        // 内核基址区（0x200000）与 NULL 区都不行
        let b = synth_elf(ET_EXEC, 0x20_0000, None);
        assert!(verify_user_elf(&b).unwrap_err().contains("base region"));
        let b = synth_elf(ET_EXEC, 0x0, None);
        assert!(verify_user_elf(&b).unwrap_err().contains("base region"));
    }

    #[test]
    fn rejects_pt_dynamic() {
        let b = synth_elf(ET_EXEC, USER_BASE + 0x78, Some((PT_DYNAMIC, FLAGS_RW, USER_BASE + 0x4000)));
        assert!(verify_user_elf(&b).unwrap_err().contains("PT_DYNAMIC"));
    }

    #[test]
    fn rejects_wx_and_low_vaddr() {
        // W+X 段
        let b = synth_elf(ET_EXEC, USER_BASE + 0x78, Some((PT_LOAD, PF_R | PF_W | PF_X, USER_BASE + 0x4000)));
        assert!(verify_user_elf(&b).unwrap_err().contains("W+X"));
        // vaddr 低于基址（NULL guard 区）
        let b = synth_elf(ET_EXEC, USER_BASE + 0x78, Some((PT_LOAD, FLAGS_RW, 0x1000)));
        assert!(verify_user_elf(&b).unwrap_err().contains("below user base"));
    }

    #[test]
    fn rejects_truncated_input() {
        assert!(verify_user_elf(&[]).is_err());
        assert!(verify_user_elf(&[0x7f, b'E', b'L', b'F']).is_err());
        // magic 错
        let mut b = synth_elf(ET_EXEC, USER_BASE, None);
        b[1] = b'X';
        assert!(verify_user_elf(&b).unwrap_err().contains("magic"));
    }

    #[test]
    fn rejects_entry_not_in_rx_segment() {
        // entry 落进 RW data 段：非可执行 PT_LOAD
        let b = synth_elf(ET_EXEC, USER_BASE + 0x2000, None);
        assert!(verify_user_elf(&b).unwrap_err().contains("executable PT_LOAD"));
    }
}
