//! cpio newc（"070701"）归档解析器 — 纯逻辑（P4-T5）。
//!
//! ## 格式（Linux `Documentation/driver-api/early-userspace/buffer-format.rst`）
//!
//! ```text
//! 偏移  长度  字段（除 magic 外均为 8 位 ASCII 十六进制，大端语义数值）
//! 0     6     c_magic = "070701"
//! 6     8     c_ino
//! 14    8     c_mode   （低 12 位权限 + S_IF* 文件类型）
//! 22    8     c_uid
//! 30    8     c_gid
//! 38    8     c_nlink
//! 46    8     c_mtime
//! 54    8     c_filesize
//! 62    8     c_devmajor
//! 70    8     c_devminor
//! 78    8     c_rdevmajor
//! 86    8     c_rdevminor
//! 94    8     c_namesize （含结尾 NUL）
//! 102   8     c_check    （newc 恒 0）
//! 110   var   文件名（NUL 结尾）
//! ── 名字末尾补 0 至 (110 + namesize) 4 字节对齐 ──
//! var   c_filesize  文件数据
//! ── 数据末尾补 0 至 4 字节对齐 ──
//! ```
//!
//! 归档以名字为 `TRAILER!!!` 的条目结束（其后可有补齐字节，忽略）。
//!
//! ## 纪律
//!
//! 迭代器惰性解析（零拷贝借用输入切片）；畸形输入全部走 `Err`，不 panic。
//! 每个条目至少消耗 112 字节（header + 最短名 "."+NUL），迭代必然终止。

// ===========================================================================
// 常量与错误
// ===========================================================================

/// newc magic。
pub const NEWC_MAGIC: &[u8; 6] = b"070701";
/// 固定 header 长度。
pub const HEADER_LEN: usize = 110;
/// 结束条目名。
pub const TRAILER: &str = "TRAILER!!!";
/// 4 字节对齐掩码。
const ALIGN_MASK: usize = 3;

/// S_IFMT：mode 中的文件类型位。
pub const S_IFMT: u32 = 0o170000;
/// S_IFREG：普通文件。
pub const S_IFREG: u32 = 0o100000;
/// S_IFDIR：目录。
pub const S_IFDIR: u32 = 0o040000;
/// S_IFLNK：符号链接（initramfs 场景不消费，仅识别）。
pub const S_IFLNK: u32 = 0o120000;

/// cpio 解析错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpioError {
    /// magic 不是 "070701"（或位置越界 = 归档截断于 header 中间）。
    BadMagic,
    /// header 13 个十六进制字段含非法字符。
    BadHex,
    /// 名字区越出归档（namesize 撒谎）。
    TruncatedName,
    /// 名字未按规范以 NUL 结尾。
    NameMissingNul,
    /// 名字为 UTF-8 非法序列（内核侧按字节比较，str 化仅为便利）。
    BadNameUtf8,
    /// 数据区越出归档（filesize 撒谎）。
    TruncatedData,
}

impl core::fmt::Display for CpioError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{self:?}")
    }
}

// ===========================================================================
// Entry + 迭代器
// ===========================================================================

/// 单个归档条目（名字 + 数据零拷贝借用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry<'a> {
    /// 文件名（不含 NUL）。
    pub name: &'a str,
    /// 文件数据（长度 = c_filesize）。
    pub data: &'a [u8],
    /// c_mode 原始值。
    pub mode: u32,
    /// c_ino。
    pub ino: u32,
}

impl<'a> Entry<'a> {
    /// 是否普通文件（S_IFREG）。
    pub fn is_reg(&self) -> bool {
        self.mode & S_IFMT == S_IFREG
    }
    /// 权限位（mode 低 12 位）。
    pub fn perm(&self) -> u32 {
        self.mode & 0o7777
    }
}

/// 8 位 ASCII 十六进制字段解析。
fn hex8(b: &[u8], off: usize) -> Result<u32, CpioError> {
    let mut v: u32 = 0;
    for &c in &b[off..off + 8] {
        let d = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => return Err(CpioError::BadHex),
        };
        v = v * 16 + u32::from(d);
    }
    Ok(v)
}

fn align4(n: usize) -> usize {
    (n + ALIGN_MASK) & !ALIGN_MASK
}

/// 惰性条目迭代器（[`entries`] 创建）。
pub struct CpioIter<'a> {
    data: &'a [u8],
    pos: usize,
    done: bool,
}

impl<'a> Iterator for CpioIter<'a> {
    type Item = Result<Entry<'a>, CpioError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let d = self.data;
        // header 边界 + magic
        if self.pos + HEADER_LEN > d.len() {
            self.done = true;
            return Some(Err(CpioError::BadMagic));
        }
        if &d[self.pos..self.pos + 6] != NEWC_MAGIC {
            self.done = true;
            return Some(Err(CpioError::BadMagic));
        }
        let h = self.pos;
        let r = (|| -> Result<Option<(Entry<'a>, usize)>, CpioError> {
            let ino = hex8(d, h + 6)?;
            let mode = hex8(d, h + 14)?;
            let filesize = hex8(d, h + 54)? as usize;
            let namesize = hex8(d, h + 94)? as usize;
            // 名字区（namesize 含 NUL，最短 "."+NUL = 2）
            if namesize < 2 {
                return Err(CpioError::NameMissingNul);
            }
            let name_start = h + HEADER_LEN;
            let name_end = name_start
                .checked_add(namesize)
                .ok_or(CpioError::TruncatedName)?;
            if name_end > d.len() {
                return Err(CpioError::TruncatedName);
            }
            if d[name_end - 1] != 0 {
                return Err(CpioError::NameMissingNul);
            }
            let name_bytes = &d[name_start..name_end - 1];
            let name = core::str::from_utf8(name_bytes).map_err(|_| CpioError::BadNameUtf8)?;
            // 数据区
            let data_start = align4(name_end);
            let data_end = data_start
                .checked_add(filesize)
                .ok_or(CpioError::TruncatedData)?;
            if data_end > d.len() {
                return Err(CpioError::TruncatedData);
            }
            let next_pos = align4(data_end);
            if name == TRAILER {
                return Ok(None);
            }
            Ok(Some((
                Entry {
                    name,
                    data: &d[data_start..data_end],
                    mode,
                    ino,
                },
                next_pos,
            )))
        })();
        match r {
            Ok(Some((entry, next_pos))) => {
                self.pos = next_pos;
                Some(Ok(entry))
            }
            Ok(None) => {
                self.done = true; // TRAILER
                None
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

/// 创建归档惰性迭代器（遇到 `TRAILER!!!` 停止；畸形条目产出 `Err` 后终止）。
pub fn entries(archive: &[u8]) -> CpioIter<'_> {
    CpioIter {
        data: archive,
        pos: 0,
        done: false,
    }
}

/// 按名字查找首个匹配条目的数据（`Ok(None)` = 归档合法但无此名）。
pub fn find<'a>(archive: &'a [u8], name: &str) -> Result<Option<&'a [u8]>, CpioError> {
    for item in entries(archive) {
        let e = item?;
        if e.name == name {
            return Ok(Some(e.data));
        }
    }
    Ok(None)
}

/// 校验归档头（首条目 magic）——内核 `initrd_init` 早期防御用。
pub fn validate_magic(archive: &[u8]) -> Result<(), CpioError> {
    if archive.len() < HEADER_LEN || &archive[0..6] != NEWC_MAGIC {
        return Err(CpioError::BadMagic);
    }
    Ok(())
}

// ===========================================================================
// 宿主单元测试（手工构造归档字节）
// ===========================================================================

#[cfg(test)]
mod tests {
    extern crate std;
    use std::string::String;
    use std::vec::Vec;

    use super::*;

    /// 构造一个 newc 条目字节（含补齐）。
    fn entry_bytes(name: &str, data: &[u8], mode: u32, ino: u32) -> Vec<u8> {
        let namesize = name.len() + 1; // 含 NUL
        let mut b = Vec::new();
        b.extend_from_slice(NEWC_MAGIC);
        let hex = |v: u32| -> [u8; 8] {
            let s: String = std::format!("{v:08X}");
            let mut a = [0u8; 8];
            a.copy_from_slice(s.as_bytes());
            a
        };
        b.extend_from_slice(&hex(ino)); // c_ino
        b.extend_from_slice(&hex(mode)); // c_mode
        b.extend_from_slice(&hex(0)); // uid
        b.extend_from_slice(&hex(0)); // gid
        b.extend_from_slice(&hex(1)); // nlink
        b.extend_from_slice(&hex(0)); // mtime
        b.extend_from_slice(&hex(data.len() as u32)); // filesize
        b.extend_from_slice(&hex(0)); // devmajor
        b.extend_from_slice(&hex(0)); // devminor
        b.extend_from_slice(&hex(0)); // rdevmajor
        b.extend_from_slice(&hex(0)); // rdevminor
        b.extend_from_slice(&hex(namesize as u32)); // namesize
        b.extend_from_slice(&hex(0)); // check
        assert_eq!(b.len(), HEADER_LEN);
        b.extend_from_slice(name.as_bytes());
        b.push(0);
        while b.len() % 4 != 0 {
            b.push(0);
        }
        b.extend_from_slice(data);
        while b.len() % 4 != 0 {
            b.push(0);
        }
        b
    }

    fn trailer() -> Vec<u8> {
        entry_bytes(TRAILER, &[], 0, 0)
    }

    fn synth_archive() -> Vec<u8> {
        let mut a = Vec::new();
        a.extend(entry_bytes("hello", &[1, 2, 3, 4, 5], S_IFREG | 0o755, 1));
        a.extend(entry_bytes("dir", &[], S_IFDIR | 0o755, 2));
        a.extend(entry_bytes("notes.txt", b"hi there", S_IFREG | 0o644, 3));
        a.extend(trailer());
        a
    }

    #[test]
    fn iterates_all_entries_and_stops_at_trailer() {
        let a = synth_archive();
        let got: Vec<_> = entries(&a).map(|r| r.unwrap()).collect();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].name, "hello");
        assert_eq!(got[0].data, &[1, 2, 3, 4, 5]);
        assert!(got[0].is_reg());
        assert_eq!(got[0].perm(), 0o755);
        assert_eq!(got[1].name, "dir");
        assert!(!got[1].is_reg());
        assert!(got[1].mode & S_IFMT == S_IFDIR);
        assert_eq!(got[2].name, "notes.txt");
        assert_eq!(got[2].data, b"hi there");
    }

    #[test]
    fn find_by_name() {
        let a = synth_archive();
        assert_eq!(find(&a, "hello"), Ok(Some(&[1u8, 2, 3, 4, 5][..])));
        assert_eq!(find(&a, "notes.txt").unwrap().unwrap(), b"hi there");
        assert_eq!(find(&a, "nope"), Ok(None));
    }

    #[test]
    fn alignment_padding_respected() {
        // 名字/数据长度组合覆盖 4 字节对齐补齐（1..8 字节数据）
        for n in 1..9usize {
            let mut a = Vec::new();
            a.extend(entry_bytes("f", &std::vec![0x5A; n], S_IFREG | 0o644, 1));
            a.extend(trailer());
            let e = entries(&a).next().unwrap().unwrap();
            assert_eq!(e.data.len(), n);
            assert!(e.data.iter().all(|&b| b == 0x5A));
            assert_eq!(find(&a, "f").unwrap().unwrap().len(), n);
        }
    }

    #[test]
    fn empty_archive_is_error() {
        assert_eq!(validate_magic(&[]), Err(CpioError::BadMagic));
        let e = entries(&[]).next();
        assert_eq!(e, Some(Err(CpioError::BadMagic)));
    }

    #[test]
    fn rejects_bad_magic_and_hex() {
        let mut a = synth_archive();
        a[2] = b'9'; // magic 破坏
        assert_eq!(validate_magic(&a), Err(CpioError::BadMagic));
        assert_eq!(entries(&a).next(), Some(Err(CpioError::BadMagic)));

        let mut a = synth_archive();
        a[6] = b'Z'; // c_ino 首字符非法 hex
        assert_eq!(entries(&a).next(), Some(Err(CpioError::BadHex)));
    }

    #[test]
    fn rejects_truncated_header_name_data() {
        let a = synth_archive();
        // header 截断（< 110 字节）
        assert_eq!(entries(&a[..100]).next(), Some(Err(CpioError::BadMagic)));
        // 名字截断：截到 header+3（namesize=6 > 剩余）
        assert_eq!(
            entries(&a[..HEADER_LEN + 3]).next(),
            Some(Err(CpioError::TruncatedName))
        );
        // 数据截断：第一条目数据 5 字节 + 补齐；截掉尾部
        let first_end = align4(HEADER_LEN + 6) + 5;
        assert_eq!(
            entries(&a[..first_end - 2]).next(),
            Some(Err(CpioError::TruncatedData))
        );
    }

    #[test]
    fn rejects_missing_name_nul() {
        let mut a = synth_archive();
        // 首条目名 "hello"（namesize=6）：把 NUL 改成 'x'
        a[HEADER_LEN + 5] = b'x';
        assert_eq!(entries(&a).next(), Some(Err(CpioError::NameMissingNul)));
        // namesize=0 → NameMissingNul
        let mut b = synth_archive();
        b[94..102].copy_from_slice(b"00000000");
        assert_eq!(entries(&b).next(), Some(Err(CpioError::NameMissingNul)));
    }

    #[test]
    fn trailer_only_archive_yields_nothing() {
        let a = trailer();
        assert!(validate_magic(&a).is_ok());
        assert_eq!(entries(&a).next(), None);
        assert_eq!(find(&a, "hello"), Ok(None));
    }
}
