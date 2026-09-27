//! ELF64 静态可执行文件（ET_EXEC）解析器 — 纯逻辑（P4-T5）。
//!
//! ## 范围（task.json P4-T5 deliverables / Doc 02 §2）
//!
//! - ELF64 little-endian 头校验：magic / class / e_type=ET_EXEC / machine=x86-64；
//! - program header 遍历：PT_LOAD 提取（vaddr/offset/filesz/memsz/flags）；
//! - 段合法性：文件越界、memsz<filesz、页对齐同余、W^X、不可读、
//!   用户基址区约束（[user_base, user_limit)）、段页区间重叠、entry 落点；
//! - **不做** PIE / 动态链接（见 PT_DYNAMIC 即拒绝）/ 重定位。
//!
//! ## 纪律
//!
//! 与 `cap`/`ipc`/`proc`/`sched`/`vma` 同源：零依赖纯 no_std、固定数组
//! （无 alloc）、[`#![deny(unsafe_code)]`](deny)、宿主可测。内核集成层
//! （`kernel/src/elfload.rs`）消费本 crate 的 [`ParsedElf`] 做实际映射。
//!
//! ## 与 xtask `user` 断言的关系
//!
//! `xtask/src/user.rs` 的 `verify_user_elf` 是**构建期**冒烟断言（宿主侧、
//! 面向单个已知产物）；本 crate 是**加载期**完整解析（内核侧、面向任意
//! initramfs 来路字节，畸形输入必须全部走 Err 而非 panic）。

#![no_std]
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod cpio;

// ===========================================================================
// 常量（ELF-64 spec: Table 1~6 + SysV x86-64 ABI）
// ===========================================================================

/// ELF magic：`\x7fELF`。
pub const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
/// `EI_CLASS` = ELFCLASS64。
const ELFCLASS64: u8 = 2;
/// `EI_DATA` = ELFDATA2LSB（little-endian）。
const ELFDATA2LSB: u8 = 1;
/// `e_type` = ET_EXEC（静态可执行，非 PIE）。
pub const ET_EXEC: u16 = 2;
/// `e_machine` = EM_X86_64。
pub const EM_X86_64: u16 = 0x3E;
/// program header 类型：PT_LOAD（可加载段）。
pub const PT_LOAD: u32 = 1;
/// program header 类型：PT_DYNAMIC（动态链接 — 静态 ELF 不应存在）。
pub const PT_DYNAMIC: u32 = 2;
/// 段标志：PF_X（可执行）。
pub const PF_X: u32 = 1;
/// 段标志：PF_W（可写）。
pub const PF_W: u32 = 2;
/// 段标志：PF_R（可读）。
pub const PF_R: u32 = 4;

/// ELF64 文件头长度（e_ident[16] + 固定字段 48）。
const EHDR_LEN: usize = 64;
/// ELF64 program header 单条长度。
const PHDR_LEN: usize = 56;
/// 页大小（段 vaddr/offset 同余校验与页区间计算）。
pub const PAGE_SIZE: u64 = 4096;

/// PT_LOAD 段数上限（典型静态 ELF 为 2~4；固定数组纪律，超出即拒绝）。
pub const MAX_LOAD_SEGMENTS: usize = 8;

// ===========================================================================
// 错误
// ===========================================================================

/// ELF 解析错误（畸形输入全部走 Err，不 panic — 加载期面对不可信字节）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElfError {
    /// 文件长度不足 ELF64 头（64 字节）。
    TooShort,
    /// magic 不是 `\x7fELF`。
    BadMagic,
    /// `EI_CLASS` ≠ ELFCLASS64。
    NotElf64,
    /// `EI_DATA` ≠ ELFDATA2LSB。
    NotLittleEndian,
    /// `e_type` ≠ ET_EXEC（PIE/ET_DYN/ET_REL 一律拒绝）。
    NotStaticExec,
    /// `e_machine` ≠ EM_X86_64。
    BadMachine,
    /// `e_phentsize` ≠ 56。
    BadPhdrSize,
    /// program header 表越出文件边界。
    PhdrOutOfFile,
    /// 没有任何 PT_LOAD 段。
    NoLoadSegments,
    /// PT_LOAD 段数超过 [`MAX_LOAD_SEGMENTS`]。
    TooManySegments,
    /// 段的 `[p_offset, p_offset+p_filesz)` 越出文件边界。
    SegmentOutOfFile,
    /// `p_memsz < p_filesz` 或 `p_memsz == 0`。
    BadSegmentSize,
    /// `p_vaddr % 4096 != p_offset % 4096`（页对齐同余不成立）。
    SegmentNotCongruent,
    /// 段同时可写可执行（W^X 违例，Doc 02 §2.2）。
    WxSegment,
    /// 段不可读（PF_R 缺失）。
    UnreadableSegment,
    /// 段 flags 不在 {R, RX, RW} 集合内（如 X-only）。
    BadSegmentFlags,
    /// 段页区间低于 `user_base`（NULL guard / 内核区）。
    BelowUserBase,
    /// 段页区间越过 `user_limit`。
    AboveUserLimit,
    /// 两个 PT_LOAD 段的页区间重叠。
    OverlappingSegments,
    /// 存在 PT_DYNAMIC（应为静态 ELF）。
    DynamicPresent,
    /// `e_entry` 未落在任何可执行 PT_LOAD 段内。
    EntryNotExecutable,
}

impl core::fmt::Display for ElfError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{self:?}")
    }
}

// ===========================================================================
// LoadConfig / LoadSegment / ParsedElf
// ===========================================================================

/// 加载约束（内核集成层传入用户地址空间窗口，Doc 02 §3.1）。
#[derive(Clone, Copy, Debug)]
pub struct LoadConfig {
    /// 用户区下界（含；典型 1GB = 0x4000_0000）。
    pub user_base: u64,
    /// 用户区上界（不含；典型 2GB = 0x8000_0000）。
    pub user_limit: u64,
}

/// 单个 PT_LOAD 段的加载视图（file_data 借用输入字节，零拷贝）。
#[derive(Clone, Copy, Debug)]
pub struct LoadSegment<'a> {
    /// 段虚拟地址（原始值，可能页内非对齐）。
    pub vaddr: u64,
    /// 文件中段数据长度（映射时拷贝的部分）。
    pub filesz: u64,
    /// 内存中段总长度（> filesz 的部分为 bss，清零）。
    pub memsz: u64,
    /// 段数据在输入文件中的字节切片（长度 = filesz）。
    pub file_data: &'a [u8],
    /// p_flags 原始值（已通过 W^X / 可读校验）。
    pub flags: u32,
}

impl<'a> LoadSegment<'a> {
    /// 段占据的首页地址（含，页对齐）。
    pub fn page_start(&self) -> u64 {
        self.vaddr & !(PAGE_SIZE - 1)
    }

    /// 段占据的末页结束地址（不含，页对齐）。
    pub fn page_end(&self) -> u64 {
        (self.vaddr + self.memsz + (PAGE_SIZE - 1)) & !(PAGE_SIZE - 1)
    }

    /// 页数（≥ 1）。
    pub fn page_count(&self) -> u64 {
        (self.page_end() - self.page_start()) / PAGE_SIZE
    }

    /// 段是否可执行（PF_X）。
    pub fn exec(&self) -> bool {
        self.flags & PF_X != 0
    }

    /// 段是否可写（PF_W）。
    pub fn writable(&self) -> bool {
        self.flags & PF_W != 0
    }
}

/// 解析成功的静态 ELF（entry + 固定数组段表）。
#[derive(Debug)]
pub struct ParsedElf<'a> {
    /// 入口地址（e_entry；已验证落在可执行段内）。
    pub entry: u64,
    /// PT_LOAD 段（前 `segment_count` 个有效）。
    pub segments: [Option<LoadSegment<'a>>; MAX_LOAD_SEGMENTS],
    /// 有效段数。
    pub segment_count: usize,
}

impl<'a> ParsedElf<'a> {
    /// 迭代有效 PT_LOAD 段。
    pub fn loads(&self) -> impl Iterator<Item = &LoadSegment<'a>> {
        self.segments[..self.segment_count].iter().filter_map(|s| s.as_ref())
    }
}

// ===========================================================================
// 解析
// ===========================================================================

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

/// 解析并校验 ELF64 静态可执行文件。
///
/// `bytes` 为完整文件内容（内核侧 = initramfs 中提取的驻留内存切片）；
/// `cfg` 提供用户地址窗口约束。返回 [`ParsedElf`]（段数据零拷贝借用
/// `bytes`）或第一条违规 [`ElfError`]。
pub fn parse<'a>(bytes: &'a [u8], cfg: &LoadConfig) -> Result<ParsedElf<'a>, ElfError> {
    // ---- 文件头 ----
    if bytes.len() < EHDR_LEN {
        return Err(ElfError::TooShort);
    }
    if bytes[0..4] != ELF_MAGIC {
        return Err(ElfError::BadMagic);
    }
    if bytes[4] != ELFCLASS64 {
        return Err(ElfError::NotElf64);
    }
    if bytes[5] != ELFDATA2LSB {
        return Err(ElfError::NotLittleEndian);
    }
    let e_type = u16le(bytes, 16);
    if e_type != ET_EXEC {
        return Err(ElfError::NotStaticExec);
    }
    let e_machine = u16le(bytes, 18);
    if e_machine != EM_X86_64 {
        return Err(ElfError::BadMachine);
    }
    let e_entry = u64le(bytes, 24);
    let e_phoff = u64le(bytes, 32) as usize;
    let e_phentsize = u16le(bytes, 54) as usize;
    let e_phnum = u16le(bytes, 56) as usize;

    if e_phentsize != PHDR_LEN {
        return Err(ElfError::BadPhdrSize);
    }
    // phoff/phnum 乘法溢出与文件边界（e_phnum ≤ 65535、PHDR_LEN=56 →
    // 乘积 < 2^22，usize 不溢出；仍用 checked 保持纪律）。
    let ph_total = e_phnum
        .checked_mul(PHDR_LEN)
        .ok_or(ElfError::PhdrOutOfFile)?;
    if e_phoff.checked_add(ph_total).ok_or(ElfError::PhdrOutOfFile)? > bytes.len() {
        return Err(ElfError::PhdrOutOfFile);
    }

    // ---- program headers ----
    let mut segments: [Option<LoadSegment<'a>>; MAX_LOAD_SEGMENTS] = Default::default();
    let mut count = 0usize;
    for i in 0..e_phnum {
        let ph = e_phoff + i * PHDR_LEN;
        let p_type = u32le(bytes, ph);
        if p_type == PT_DYNAMIC {
            return Err(ElfError::DynamicPresent);
        }
        if p_type != PT_LOAD {
            continue; // PT_NOTE/PT_PHDR/PT_GNU_STACK 等忽略
        }
        if count >= MAX_LOAD_SEGMENTS {
            return Err(ElfError::TooManySegments);
        }
        let p_flags = u32le(bytes, ph + 4);
        let p_offset = u64le(bytes, ph + 8);
        let p_vaddr = u64le(bytes, ph + 16);
        let p_filesz = u64le(bytes, ph + 32);
        let p_memsz = u64le(bytes, ph + 40);

        // 尺寸与文件边界
        if p_memsz == 0 || p_memsz < p_filesz {
            return Err(ElfError::BadSegmentSize);
        }
        let data_end = p_offset
            .checked_add(p_filesz)
            .ok_or(ElfError::SegmentOutOfFile)?;
        if data_end > bytes.len() as u64 {
            return Err(ElfError::SegmentOutOfFile);
        }
        // 页对齐同余（p_vaddr ≡ p_offset (mod PAGE)，ELF spec 强制）
        if p_vaddr % PAGE_SIZE != p_offset % PAGE_SIZE {
            return Err(ElfError::SegmentNotCongruent);
        }
        // 权限（W^X + 可读 + 限定 {R, RX, RW}）
        if p_flags & PF_W != 0 && p_flags & PF_X != 0 {
            return Err(ElfError::WxSegment);
        }
        if p_flags & PF_R == 0 {
            return Err(ElfError::UnreadableSegment);
        }
        if p_flags != PF_R && p_flags != (PF_R | PF_X) && p_flags != (PF_R | PF_W) {
            return Err(ElfError::BadSegmentFlags);
        }
        // 用户地址窗口 + 溢出
        let seg = LoadSegment {
            vaddr: p_vaddr,
            filesz: p_filesz,
            memsz: p_memsz,
            file_data: &bytes[p_offset as usize..data_end as usize],
            flags: p_flags,
        };
        if seg.page_start() < cfg.user_base {
            return Err(ElfError::BelowUserBase);
        }
        if seg.page_end() > cfg.user_limit || seg.vaddr + seg.memsz > cfg.user_limit {
            return Err(ElfError::AboveUserLimit);
        }
        // 与已收录段的页区间重叠
        for prev in segments[..count].iter().flatten() {
            let disjoint = seg.page_end() <= prev.page_start() || prev.page_end() <= seg.page_start();
            if !disjoint {
                return Err(ElfError::OverlappingSegments);
            }
        }
        segments[count] = Some(seg);
        count += 1;
    }
    if count == 0 {
        return Err(ElfError::NoLoadSegments);
    }

    // ---- entry：必须落在某个 PF_X 段内 ----
    let entry_ok = segments[..count]
        .iter()
        .flatten()
        .any(|s| s.exec() && s.vaddr <= e_entry && e_entry < s.vaddr + s.memsz);
    if !entry_ok {
        return Err(ElfError::EntryNotExecutable);
    }

    Ok(ParsedElf {
        entry: e_entry,
        segments,
        segment_count: count,
    })
}

// ===========================================================================
// 宿主单元测试
// ===========================================================================

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec::Vec;

    use super::*;

    const CFG: LoadConfig = LoadConfig {
        user_base: 0x4000_0000,
        user_limit: 0x8000_0000,
    };
    const USER_BASE: u64 = 0x4000_0000;

    /// 构造最小合法静态 ELF：text RX @ base（含 entry）+ data RW @ base+0x2000，
    /// 段文件数据 = 0xAA/0xBB 填充。返回完整文件字节。
    ///
    /// 段文件偏移取页对齐（0x1000/0x2000）→ 与 vaddr 的页同余天然成立。
    fn synth(entry: u64) -> Vec<u8> {
        let phoff = 64usize;
        let text: Vec<u8> = std::vec![0xAA; 0x100];
        let data: Vec<u8> = std::vec![0xBB; 0x80];
        let off0 = 0x1000usize; // 页对齐 → ≡ USER_BASE (mod 4096)
        let off1 = 0x2000usize; // 页对齐 → ≡ USER_BASE+0x2000 (mod 4096)
        let mut b = std::vec![0u8; off1 + data.len()];

        b[0..4].copy_from_slice(&ELF_MAGIC);
        b[4] = ELFCLASS64;
        b[5] = ELFDATA2LSB;
        b[6] = 1; // e_version
        b[16..18].copy_from_slice(&ET_EXEC.to_le_bytes());
        b[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
        b[20..24].copy_from_slice(&1u32.to_le_bytes());
        b[24..32].copy_from_slice(&entry.to_le_bytes());
        b[32..40].copy_from_slice(&(phoff as u64).to_le_bytes());
        b[54..56].copy_from_slice(&(PHDR_LEN as u16).to_le_bytes());
        b[56..58].copy_from_slice(&2u16.to_le_bytes());

        // phdr 0: text RX
        let mut ph = |i: usize, t: u32, f: u32, off: u64, vaddr: u64, filesz: u64, memsz: u64| {
            let o = phoff + i * PHDR_LEN;
            b[o..o + 4].copy_from_slice(&t.to_le_bytes());
            b[o + 4..o + 8].copy_from_slice(&f.to_le_bytes());
            b[o + 8..o + 16].copy_from_slice(&off.to_le_bytes());
            b[o + 16..o + 24].copy_from_slice(&vaddr.to_le_bytes());
            b[o + 24..o + 32].copy_from_slice(&vaddr.to_le_bytes()); // p_paddr
            b[o + 32..o + 40].copy_from_slice(&filesz.to_le_bytes());
            b[o + 40..o + 48].copy_from_slice(&memsz.to_le_bytes());
            b[o + 48..o + 56].copy_from_slice(&0x1000u64.to_le_bytes()); // p_align
        };
        ph(0, PT_LOAD, PF_R | PF_X, off0 as u64, USER_BASE, text.len() as u64, text.len() as u64);
        // phdr 1: data RW（memsz > filesz → bss 0x800）
        ph(
            1,
            PT_LOAD,
            PF_R | PF_W,
            off1 as u64,
            USER_BASE + 0x2000,
            data.len() as u64,
            data.len() as u64 + 0x800,
        );

        b[off0..off0 + text.len()].copy_from_slice(&text);
        b[off1..off1 + data.len()].copy_from_slice(&data);
        b
    }

    #[test]
    fn parses_valid_static_elf() {
        let b = synth(USER_BASE + 0x40);
        let elf = parse(&b, &CFG).expect("valid ELF must parse");
        assert_eq!(elf.entry, USER_BASE + 0x40);
        assert_eq!(elf.segment_count, 2);
        let segs: Vec<_> = elf.loads().collect();
        assert_eq!(segs[0].vaddr, USER_BASE);
        assert!(segs[0].exec() && !segs[0].writable());
        assert_eq!(segs[0].file_data.len(), 0x100);
        assert_eq!(segs[0].file_data[0], 0xAA);
        assert_eq!(segs[0].page_start(), USER_BASE);
        assert_eq!(segs[0].page_end(), USER_BASE + 0x1000);
        assert_eq!(segs[1].vaddr, USER_BASE + 0x2000);
        assert!(!segs[1].exec() && segs[1].writable());
        assert_eq!(segs[1].page_count(), 1); // 0x80+0x800 = 0x880 → 1 页
        assert_eq!(segs[1].file_data[0], 0xBB);
    }

    #[test]
    fn rejects_header_problems() {
        assert_eq!(parse(&[], &CFG).unwrap_err(), ElfError::TooShort);
        assert_eq!(parse(&[0x7f, b'E', b'L', b'F'], &CFG).unwrap_err(), ElfError::TooShort);
        let mut b = synth(USER_BASE);
        b[1] = b'X';
        assert_eq!(parse(&b, &CFG).unwrap_err(), ElfError::BadMagic);
        let mut b = synth(USER_BASE);
        b[4] = 1; // ELFCLASS32
        assert_eq!(parse(&b, &CFG).unwrap_err(), ElfError::NotElf64);
        let mut b = synth(USER_BASE);
        b[5] = 2; // big-endian
        assert_eq!(parse(&b, &CFG).unwrap_err(), ElfError::NotLittleEndian);
    }

    #[test]
    fn rejects_non_exec_and_bad_machine() {
        let mut b = synth(USER_BASE);
        b[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN（PIE）
        assert_eq!(parse(&b, &CFG).unwrap_err(), ElfError::NotStaticExec);
        let mut b = synth(USER_BASE);
        b[18..20].copy_from_slice(&0xB7u16.to_le_bytes()); // aarch64
        assert_eq!(parse(&b, &CFG).unwrap_err(), ElfError::BadMachine);
    }

    #[test]
    fn rejects_phdr_problems() {
        let mut b = synth(USER_BASE);
        b[54..56].copy_from_slice(&48u16.to_le_bytes()); // e_phentsize != 56
        assert_eq!(parse(&b, &CFG).unwrap_err(), ElfError::BadPhdrSize);
        let mut b = synth(USER_BASE);
        b[56..58].copy_from_slice(&9999u16.to_le_bytes()); // e_phnum 越界
        assert_eq!(parse(&b, &CFG).unwrap_err(), ElfError::PhdrOutOfFile);
    }

    #[test]
    fn rejects_entry_not_in_rx_segment() {
        // entry 落在 RW data 段
        let b = synth(USER_BASE + 0x2000);
        assert_eq!(parse(&b, &CFG).unwrap_err(), ElfError::EntryNotExecutable);
        // entry 落在段外空洞
        let b = synth(USER_BASE + 0x1500);
        assert_eq!(parse(&b, &CFG).unwrap_err(), ElfError::EntryNotExecutable);
    }

    #[test]
    fn rejects_out_of_window() {
        // 段基址低于 user_base（NULL guard 区）
        let b = synth(USER_BASE);
        let mut b2 = b.clone();
        let ph0 = 64usize;
        b2[ph0 + 16..ph0 + 24].copy_from_slice(&0x1000u64.to_le_bytes());
        // entry 也随之移出段 → 先命中 BelowUserBase（段校验在 entry 之前）
        assert_eq!(parse(&b2, &CFG).unwrap_err(), ElfError::BelowUserBase);
        // 段越过 user_limit：memsz 巨大
        let mut b3 = b.clone();
        let ph1 = 64 + PHDR_LEN;
        b3[ph1 + 40..ph1 + 48].copy_from_slice(&0x4000_0000u64.to_le_bytes());
        assert_eq!(parse(&b3, &CFG).unwrap_err(), ElfError::AboveUserLimit);
    }

    #[test]
    fn rejects_wx_unreadable_and_dynamic() {
        let b = synth(USER_BASE);
        let ph0 = 64usize;
        // W+X
        let mut b2 = b.clone();
        b2[ph0 + 4..ph0 + 8].copy_from_slice(&(PF_R | PF_W | PF_X).to_le_bytes());
        assert_eq!(parse(&b2, &CFG).unwrap_err(), ElfError::WxSegment);
        // X-only（无 R）
        let mut b3 = b.clone();
        b3[ph0 + 4..ph0 + 8].copy_from_slice(&PF_X.to_le_bytes());
        assert_eq!(parse(&b3, &CFG).unwrap_err(), ElfError::UnreadableSegment);
        // 非常规 flags（R+其他位）
        let mut b4 = b.clone();
        b4[ph0 + 4..ph0 + 8].copy_from_slice(&(PF_R | 0x100).to_le_bytes());
        assert_eq!(parse(&b4, &CFG).unwrap_err(), ElfError::BadSegmentFlags);
        // PT_DYNAMIC：把 phdr1 的 type 改成 2
        let mut b5 = b.clone();
        let ph1 = 64 + PHDR_LEN;
        b5[ph1..ph1 + 4].copy_from_slice(&PT_DYNAMIC.to_le_bytes());
        assert_eq!(parse(&b5, &CFG).unwrap_err(), ElfError::DynamicPresent);
    }

    #[test]
    fn rejects_bad_sizes_and_bounds() {
        let b = synth(USER_BASE);
        let ph0 = 64usize;
        // memsz < filesz
        let mut b2 = b.clone();
        b2[ph0 + 40..ph0 + 48].copy_from_slice(&0x10u64.to_le_bytes());
        assert_eq!(parse(&b2, &CFG).unwrap_err(), ElfError::BadSegmentSize);
        // memsz == 0
        let mut b3 = b.clone();
        b3[ph0 + 40..ph0 + 48].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(parse(&b3, &CFG).unwrap_err(), ElfError::BadSegmentSize);
        // filesz 越出文件
        let mut b4 = b.clone();
        b4[ph0 + 32..ph0 + 40].copy_from_slice(&0xFFFF_FFFFu64.to_le_bytes());
        b4[ph0 + 40..ph0 + 48].copy_from_slice(&0xFFFF_FFFFu64.to_le_bytes());
        assert_eq!(parse(&b4, &CFG).unwrap_err(), ElfError::SegmentOutOfFile);
        // 同余破坏：vaddr+1（offset 不动）
        let mut b5 = b.clone();
        b5[ph0 + 16..ph0 + 24].copy_from_slice(&(USER_BASE + 1).to_le_bytes());
        assert_eq!(parse(&b5, &CFG).unwrap_err(), ElfError::SegmentNotCongruent);
    }

    #[test]
    fn rejects_overlapping_segments() {
        // 把 data 段改成与 text 段同页区间（vaddr=USER_BASE，offset=0 保持同余）
        let b = synth(USER_BASE);
        let mut b2 = b.clone();
        let ph1 = 64 + PHDR_LEN;
        b2[ph1 + 8..ph1 + 16].copy_from_slice(&0u64.to_le_bytes()); // p_offset = 0
        b2[ph1 + 16..ph1 + 24].copy_from_slice(&USER_BASE.to_le_bytes()); // p_vaddr
        b2[ph1 + 32..ph1 + 40].copy_from_slice(&0x40u64.to_le_bytes()); // p_filesz
        b2[ph1 + 40..ph1 + 48].copy_from_slice(&0x40u64.to_le_bytes()); // p_memsz
        assert_eq!(parse(&b2, &CFG).unwrap_err(), ElfError::OverlappingSegments);
    }

    #[test]
    fn rejects_no_load_segments() {
        // 两个 phdr 都改成 PT_NOTE
        let b = synth(USER_BASE);
        let mut b2 = b.clone();
        b2[64..68].copy_from_slice(&4u32.to_le_bytes());
        b2[64 + PHDR_LEN..64 + PHDR_LEN + 4].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(parse(&b2, &CFG).unwrap_err(), ElfError::NoLoadSegments);
    }
}
