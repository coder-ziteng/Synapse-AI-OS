#!/usr/bin/env python3
"""为内核 ELF 注入 PVH (Xen HVM) Note 段 + PT_NOTE 程序头，使 QEMU `-kernel` 可直接加载。

PVH Note 格式（Xen ABI）：
  namesz  = 4 ("Xen\0")
  descsz  = 4
  type    = 0x12 (XEN_ELFNOTE_PHYS32_ENTRY)
  desc    = u32 entry_phys (小端)
"""
import struct
import sys

XEN_NOTE_NAME = b"Xen\x00"
NOTE_TYPE_PHYS32_ENTRY = 0x12


def elf64_note(name: bytes, desc: bytes, ntype: int) -> bytes:
    """构造一个 Elf64_Nhdr + name + desc 的字节串（4 字节对齐）。"""
    namesz = len(name)
    descsz = len(desc)
    header = struct.Struct("<III").pack(namesz, descsz, ntype)
    # 4 字节对齐
    name_padded = name + b"\x00" * ((4 - namesz % 4) % 4)
    desc_padded = desc + b"\x00" * ((4 - descsz % 4) % 4)
    return header + name_padded + desc_padded


def parse_u16(buf: bytes, off: int) -> int:
    return struct.unpack_from("<H", buf, off)[0]


def parse_u32(buf: bytes, off: int) -> int:
    return struct.unpack_from("<I", buf, off)[0]


def parse_u64(buf: bytes, off: int) -> int:
    return struct.unpack_from("<Q", buf, off)[0]


def patch(path_in: str, path_out: str) -> None:
    with open(path_in, "rb") as f:
        data = bytearray(f.read())

    # 校验 ELF magic
    assert data[:4] == b"\x7fELF", "Not an ELF file"
    # 校验 64-bit
    assert data[4] == 2, "Not 64-bit ELF"
    # 校验 little-endian
    assert data[5] == 1, "Not little-endian ELF"

    e_entry = parse_u64(data, 0x18)
    e_phoff = parse_u64(data, 0x20)
    e_phentsize = parse_u16(data, 0x36)
    e_phnum = parse_u16(data, 0x38)

    # QEMU `-kernel` 加载 PIE ELF 时按 e_entry 当物理地址处理（与 vaddr 重合）
    entry_phys = e_entry
    note_body = elf64_note(XEN_NOTE_NAME, struct.pack("<I", entry_phys), NOTE_TYPE_PHYS32_ENTRY)

    # 对齐到 4 字节
    while len(data) % 4 != 0:
        data.append(0)
    note_offset = len(data)
    data.extend(note_body)

    # 在 Program Header 数组里追加 PT_NOTE（直接覆盖原 PT 表末尾后第一个 slot）
    PT_NOTE = 4
    PF_R = 0x4
    # Elf64_Phdr 布局（56 字节）：p_type(u32), p_flags(u32), p_offset(u64),
    #   p_vaddr(u64), p_paddr(u64), p_filesz(u64), p_memsz(u64), p_align(u64)
    new_ph = struct.Struct("<IIQQQQQQ").pack(
        PT_NOTE,          # p_type
        PF_R,             # p_flags
        note_offset,      # p_offset
        note_offset,      # p_vaddr
        note_offset,      # p_paddr
        len(note_body),   # p_filesz
        len(note_body),   # p_memsz
        4,                # p_align
    )

    # 写回 ELF Header 的 phnum
    struct.pack_into("<H", data, 0x38, e_phnum + 1)

    # 把新 Program Header 写回 Program Header 表末尾后第一个 slot
    # PT 表原长度 = phnum * phentsize，原表后是其它数据；直接覆盖 phnum*phentsize 偏移处 56 字节
    append_offset = e_phoff + e_phnum * e_phentsize
    data[append_offset:append_offset + len(new_ph)] = new_ph

    with open(path_out, "wb") as f:
        f.write(data)

    print(f"[add_pvh_note] e_entry=0x{e_entry:x} entry_phys=0x{entry_phys:x}")
    print(f"[add_pvh_note] note appended at file offset 0x{note_offset:x} ({len(note_body)} bytes)")
    print(f"[add_pvh_note] PT_NOTE added at phoff 0x{append_offset:x}, phnum {e_phnum} -> {e_phnum + 1}")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        print("Usage: add_pvh_note.py <input.elf> <output.elf>", file=sys.stderr)
        sys.exit(1)
    patch(sys.argv[1], sys.argv[2])