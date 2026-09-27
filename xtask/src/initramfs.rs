//! initramfs 打包（cpio newc 写器，P4-T5）。
//!
//! 把用户态产物（首期：`user/hello` 静态 ELF）打成 cpio newc 归档，
//! 由 `build_disk.py` 追加进磁盘镜像；stage2 把内核+initramfs 连续
//! 加载到 0x200000+，并在 0x20100 写 {base, size} 记录（内核
//! `initrd.rs` 消费）。
//!
//! 格式契约与纯逻辑解析器 [`synapse_elf::cpio`]（elf/ crate）严格对称：
//! header 110B ASCII-hex + 名字（含 NUL）4B 对齐补齐 + 数据 4B 对齐补齐
//! + `TRAILER!!!` 收尾。宿主测试直接走解析器闭环验证。
//!
//! ## cpio newc header（全部 8 位大写 ASCII hex）
//!
//! `c_magic="070701" | ino | mode | uid | gid | nlink | mtime | filesize |
//!  devmajor | devminor | rdevmajor | rdevminor | namesize | check`

/// S_IFREG | 0755：initramfs 内可执行文件的缺省 mode。
pub const MODE_REG_755: u32 = 0o100755;

/// 归档内的一个待打包文件（名字 + 数据）。
pub struct InitrdFile<'a> {
    /// 归档内名字（如 `hello`；内核 `cpio::find` 按此查找）。
    pub name: &'a str,
    /// 文件字节。
    pub data: &'a [u8],
}

/// 8 位大写 ASCII hex 字段。
fn hex8(v: u32) -> [u8; 8] {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut a = [0u8; 8];
    let mut x = v;
    for i in (0..8).rev() {
        a[i] = DIGITS[(x & 0xF) as usize];
        x >>= 4;
    }
    a
}

fn push_entry(out: &mut Vec<u8>, ino: u32, mode: u32, name: &str, data: &[u8]) {
    out.extend_from_slice(b"070701");
    out.extend_from_slice(&hex8(ino)); // c_ino
    out.extend_from_slice(&hex8(mode)); // c_mode
    out.extend_from_slice(&hex8(0)); // c_uid
    out.extend_from_slice(&hex8(0)); // c_gid
    out.extend_from_slice(&hex8(1)); // c_nlink
    out.extend_from_slice(&hex8(0)); // c_mtime（可重现构建：恒 0）
    out.extend_from_slice(&hex8(data.len() as u32)); // c_filesize
    out.extend_from_slice(&hex8(0)); // c_devmajor
    out.extend_from_slice(&hex8(0)); // c_devminor
    out.extend_from_slice(&hex8(0)); // c_rdevmajor
    out.extend_from_slice(&hex8(0)); // c_rdevminor
    out.extend_from_slice(&hex8(name.len() as u32 + 1)); // c_namesize（含 NUL）
    out.extend_from_slice(&hex8(0)); // c_check（newc 恒 0）
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out.extend_from_slice(data);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

/// 把文件列表打成 cpio newc 归档字节（含 `TRAILER!!!` 收尾）。
///
/// # Panics
/// 名字为空 / 含 NUL / 数据 > u32::MAX（构建期输入，违反即构建 bug）。
pub fn build_cpio(files: &[InitrdFile<'_>]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, f) in files.iter().enumerate() {
        assert!(!f.name.is_empty() && !f.name.contains('\0'), "bad initrd name");
        assert!(f.data.len() <= u32::MAX as usize, "initrd file too large");
        push_entry(&mut out, (i + 1) as u32, MODE_REG_755, f.name, f.data);
    }
    // TRAILER：ino=0, mode=0, 空数据（Linux buffer-format.rst 约定）
    push_entry(&mut out, 0, 0, "TRAILER!!!", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use synapse_elf::cpio;

    #[test]
    fn writer_parser_roundtrip() {
        let hello = [0x7fu8, b'E', b'L', b'F', 1, 2, 3];
        let note = b"some bytes here";
        let files = [
            InitrdFile { name: "hello", data: &hello },
            InitrdFile { name: "note.txt", data: note },
        ];
        let arc = build_cpio(&files);

        cpio::validate_magic(&arc).expect("magic");
        let entries: Vec<_> = cpio::entries(&arc).map(|r| r.unwrap()).collect();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "hello");
        assert_eq!(entries[0].data, hello);
        assert!(entries[0].is_reg());
        assert_eq!(entries[0].perm(), 0o755);
        assert_eq!(entries[1].name, "note.txt");
        assert_eq!(entries[1].data, note);

        assert_eq!(cpio::find(&arc, "hello"), Ok(Some(&hello[..])));
        assert_eq!(cpio::find(&arc, "note.txt").unwrap().unwrap(), note);
        assert_eq!(cpio::find(&arc, "missing"), Ok(None));
    }

    #[test]
    fn empty_archive_is_trailer_only() {
        let arc = build_cpio(&[]);
        assert_eq!(arc.len() % 4, 0);
        cpio::validate_magic(&arc).expect("magic");
        assert_eq!(cpio::entries(&arc).next(), None);
    }

    #[test]
    fn padding_alignment_holds_for_all_small_sizes() {
        for n in 0..17usize {
            let data = vec![0x5Au8; n];
            let arc = build_cpio(&[InitrdFile { name: "f", data: &data }]);
            assert_eq!(arc.len() % 4, 0, "archive not 4-aligned at n={n}");
            assert_eq!(cpio::find(&arc, "f").unwrap().unwrap(), data.as_slice());
        }
    }
}
