#!/usr/bin/env python3
"""为内核 ELF 注入 Multiboot2 header + entry address tag。

Multiboot2 header (32 字节) 由三部分组成：
  ┌─ magic(4) ─┬─ arch(4) ─┬─ length(4) ─┬─ checksum(4) ─┐
  │ 0xE85250D6 │     0    │      32     │   -(sum)      │  = 16 字节
  ├─ tag0 ─────────────────────────────────────────────────┤
  │ type(u16)=3 │ flags(u16)=0 │ size(u32)=16 │ entry(u32) │  = 16 字节
  ├─ end tag ──────────────────────────────────────────────┤
  │ type(u16)=0 │ flags(u16)=0 │ size(u32)=8  │            │  =  8 字节
  └──────────────────────────────────────────────────────────┘
合计 40 字节。

布局：插在第一个 PT_LOAD 的偏移 0 处（ELF header 64 字节会向后平移）。
程序头表内容（program headers）向后平移 40 字节避免被覆盖。

QEMU `-kernel file` 检测到 Multiboot2 header 时：
- 按 ELF PT_LOAD 段加载到内存；
- 切到 32-bit 保护模式；
- 按 entry tag 给出的地址跳转。
"""
import struct
import sys

MB2_MAGIC       = 0xE85250D6
MB2_ARCH_I386   = 0
MB2_TAG_ENTRY   = 3     # Multiboot2 entry address tag
MB2_TAG_END     = 0
MB2_TAG_FLAG_OPTIONAL = 1
MB2_HDR_SIZE    = 16    # magic+arch+length+checksum

# 完整 3+tag 长度：entry tag = u16 type + u16 flags + u32 size + u32 entry = 12 字节
# 但 MB2 spec 要求 size 字段 8 字节对齐（包含 type/flags/size 自身）。
# 8 字节对齐：header_size (16) + entry_tag (12) + 4 字节填充 + end_tag (8) = 40
# 简化版：entry tag 把 base 字段补齐到 8 字节边界 → 实际用 16 字节（size=16，含 type/flags/size+8 字节 data）。
ENTRY_TAG_SIZE = 16
END_TAG_SIZE   = 8
MB2_TOTAL = MB2_HDR_SIZE + ENTRY_TAG_SIZE + END_TAG_SIZE  # = 40


def build_multiboot2_header(entry_phys: int) -> bytes:
    """构造 40 字节 Multiboot2 header（含 entry tag + end tag）。"""
    checksum = -(MB2_MAGIC + MB2_ARCH_I386 + MB2_TOTAL) & 0xFFFFFFFF
    header = struct.pack(
        "<IIII", MB2_MAGIC, MB2_ARCH_I386, MB2_TOTAL, checksum,
    )
    # entry tag: type=3, flags=0 (multiboot required tag), size=16 (8-aligned),
    #   entry = u32 (放在 dword 0)
    # spec: entry address tag layout:
    #   u32 entry_addr (only field). For 32-bit arch, single u32.
    #   但 size 必须是 8 字节边界，所以 size=8 + padding 到 16
    entry_tag = struct.pack("<HHII", MB2_TAG_ENTRY, 0, 8, entry_phys)
    assert len(entry_tag) == 12, "entry_tag layout wrong"
    entry_tag = entry_tag.ljust(ENTRY_TAG_SIZE, b"\x00")  # pad to 16
    # end tag: type=0, flags=0, size=8
    end_tag = struct.pack("<HHI", MB2_TAG_END, 0, END_TAG_SIZE)
    assert len(end_tag) == END_TAG_SIZE

    mb2 = header + entry_tag + end_tag
    assert len(mb2) == MB2_TOTAL, f"mb2 size wrong: {len(mb2)} != {MB2_TOTAL}"
    return mb2


def patch(path_in: str, path_out: str) -> None:
    with open(path_in, "rb") as f:
        data = bytearray(f.read())
    assert data[:4] == b"\x7fELF", "Not ELF"
    assert data[4] == 2, "Not 64-bit ELF"
    assert data[5] == 1, "Not little-endian"

    e_entry = struct.unpack_from("<Q", data, 0x18)[0]
    e_phoff = struct.unpack_from("<Q", data, 0x20)[0]
    e_phentsize = struct.unpack_from("<H", data, 0x36)[0]
    e_phnum = struct.unpack_from("<H", data, 0x38)[0]

    if not (0 <= e_entry < 0xFFFFFFFF):
        sys.exit(f"e_entry 0x{e_entry:x} doesn't fit in u32 (entry tag requirement)")
    mb2 = build_multiboot2_header(e_entry & 0xFFFFFFFF)

    # 插入位置：ELF header 之后（offset 64）。ELF header 第 1 PT_LOAD 通常从 0 开始。
    insert_off = 0x40
    assert e_phoff == 0x40, f"unexpected e_phoff 0x{e_phoff:x}, expected 0x40"

    # 1. 在 0x40 处插入 MB2 header (40 bytes)
    new_data = bytearray(data[:insert_off])
    new_data.extend(mb2)
    new_data.extend(data[insert_off:])

    # 2. e_phoff += 40；程序头表后移
    new_phoff = e_phoff + MB2_TOTAL
    struct.pack_into("<Q", new_data, 0x20, new_phoff)

    # 3. 每条 PT 的 p_offset / p_vaddr / p_paddr += 40；p_filesz += 40（首段首段把 MB2 覆盖到）
    for i in range(e_phnum):
        ph_off = new_phoff + i * e_phentsize
        p_type = struct.unpack_from("<I", new_data, ph_off)[0]
        if p_type != 1:  # PT_LOAD 之外先不动
            continue
        p_offset = struct.unpack_from("<Q", new_data, ph_off + 8)[0]
        p_vaddr = struct.unpack_from("<Q", new_data, ph_off + 16)[0]
        p_paddr = struct.unpack_from("<Q", new_data, ph_off + 24)[0]
        p_filesz = struct.unpack_from("<Q", new_data, ph_off + 32)[0]
        struct.pack_into("<Q", new_data, ph_off + 8, p_offset + MB2_TOTAL)
        struct.pack_into("<Q", new_data, ph_off + 16, p_vaddr + MB2_TOTAL)
        struct.pack_into("<Q", new_data, ph_off + 24, p_paddr + MB2_TOTAL)
        struct.pack_into(
            "<Q", new_data, ph_off + 32, p_filesz + MB2_TOTAL,
        )

    # 4. e_entry += 40
    struct.pack_into("<Q", new_data, 0x18, e_entry + MB2_TOTAL)

    # e_shoff 不变（段表在文件末尾，未被插入影响）

    with open(path_out, "wb") as f:
        f.write(new_data)

    print(f"[add_mb2] entry=0x{e_entry:x} -> 0x{e_entry+MB2_TOTAL:x}")
    print(f"[add_mb2] MB2 header at file offset 0x{insert_off:x}, {MB2_TOTAL} bytes")
    print(f"[add_mb2] e_phoff 0x{e_phoff:x} -> 0x{new_phoff:x}")
    print(f"[add_mb2] phnum unchanged: {e_phnum}")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        print("Usage: add_multiboot2_header.py <in.elf> <out.elf>", file=sys.stderr)
        sys.exit(1)
    patch(sys.argv[1], sys.argv[2])
