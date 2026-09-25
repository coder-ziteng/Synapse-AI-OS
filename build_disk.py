"""Synapse AI-OS 可引导磁盘镜像生成器 (v2: 加载并跳转真实 Rust 内核).

布局 (16MB 硬盘镜像, BIOS/MBR 引导):
  sector 0        : stage 1 (boot sector @ 0x7C00) — EDD 扩展读加载 stage 2
  sector 1..N     : stage 2 (@ 0x8000) — 16→32→64 trampoline + EDD 加载内核
  sector N+1..    : kernel.bin (PT_LOAD 扁平化, 加载到 0x200000)

stage 2 流程:
  16-bit : 开 A20 (port 0x92) → EDD 检查 → LGDT →
           循环 {AH=42h 读 ≤127 扇区到暂存区 0x10000 → 进 32-bit PM
           → rep movsd 暂存区→[0x504] (平坦段, 直达 0x200000+) → 退回
           实模式 → lba/dst 推进} (DAPS seg:off 最大只能表达 ~1.06MB 无法
           直达 0x200000; SeaBIOS 1.17 的 INT 15h AH=87h 会三重故障, 弃用,
           改为每块自行进出保护模式复制)
           → CR0.PE → 远跳 32-bit
  32-bit : 段/栈 → 清零+填 4 级页表 (PML4@0x10000, PDPT@0x11000, PD0@0x12000,
           PD1@0x13000, 恒等映射 0-2GB) → CR4.PAE → CR3 → EFER.LME → CR0.PG
           → 远跳 64-bit
  64-bit : 设 rsp=0xEFFF8 (满足 Rust ABI: 入口 rsp%16==8) → jmp _start64

低内存数据区约定:
  0x500 : 启动盘号 (stage 1 保存 BIOS DL)
  0x502 : 内核剩余扇区数 (word)
  0x504 : PM 复制目标物理地址 (dword, 初始 0x200000, 每块推进)
  0x50A : 当前块扇区数备份 (word, 防 BIOS 调用破坏 bx)
  0x6000: DAPS 磁盘地址包 (AH=42h, 缓冲固定 0x1000:0x0000 → 0x10000)
  0x10000: 读盘暂存区 (≤63.5KB; 加载完成后被页表复用)

调试检查点 (port 0x501, 配合 -device isa-debugcon):
  'A' 进入 stage2, 'a' A20 已开, 'b' EDD 可用, 'B' LGDT 完成,
  'e' 每块 int13h 读成功, 'f' 每块 PM 复制完成, 'c' 内核加载完成,
  'C' 进入 32-bit, 'D' 段/栈就绪, 'E' 页表清零完,
  'F' 顶层页表项写完, 'G' PD 填充完, 'H' 分页+长模式已开,
  'K' 进入 64-bit, 'J' 即将跳内核, 'A'(内核) Rust _start64 已到达,
  'X' EDD 不支持, 'r' 读盘失败

用法: python build_disk.py [kernel.elf 路径] [输出镜像]
"""
import struct
import sys
import os

ELF = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
    'target', 'x86_64-bootloader', 'debug', 'synapse-kernel')
IMG = sys.argv[2] if len(sys.argv) > 2 else 'kernel_hd.img'
IMG_SECTORS = 32768          # 16 MB
KERNEL_LOAD = 0x200000       # 链接基址
EDD_MAX_SECTORS = 127        # 单次 AH=42h 读上限(保守值)

# ============================================================
# ELF64 解析: PT_LOAD 扁平化 + symtab 找 _start64
# ============================================================
with open(ELF, 'rb') as f:
    elf = f.read()

assert elf[:4] == b'\x7fELF' and elf[4] == 2 and elf[5] == 1, 'need ELF64 little-endian'
e_entry   = struct.unpack_from('<Q', elf, 0x18)[0]
e_phoff   = struct.unpack_from('<Q', elf, 0x20)[0]
e_shoff   = struct.unpack_from('<Q', elf, 0x28)[0]
e_phentsz = struct.unpack_from('<H', elf, 0x36)[0]
e_phnum   = struct.unpack_from('<H', elf, 0x38)[0]
e_shentsz = struct.unpack_from('<H', elf, 0x3A)[0]
e_shnum   = struct.unpack_from('<H', elf, 0x3C)[0]

loads = []
for i in range(e_phnum):
    off = e_phoff + i * e_phentsz
    p_type = struct.unpack_from('<I', elf, off)[0]
    if p_type == 1:  # PT_LOAD
        p_offset, p_vaddr, _paddr, p_filesz, p_memsz = struct.unpack_from('<QQQQQ', elf, off + 8)
        loads.append((p_vaddr, p_offset, p_filesz, p_memsz))
assert loads, 'no PT_LOAD segment'

base = min(l[0] for l in loads)
end  = max(l[0] + l[3] for l in loads)
assert base == KERNEL_LOAD, f'kernel base 0x{base:x} != 0x{KERNEL_LOAD:x}'
kernel = bytearray(end - base)  # bss 自动零填充 (memsz > filesz)
for vaddr, off, filesz, memsz in loads:
    kernel[vaddr - base: vaddr - base + filesz] = elf[off:off + filesz]

def find_symbol(name):
    needle = name.encode() + b'\0'
    for i in range(e_shnum):
        so = e_shoff + i * e_shentsz
        sh_type = struct.unpack_from('<I', elf, so + 4)[0]
        if sh_type != 2:  # SHT_SYMTAB
            continue
        sh_offset, sh_size = struct.unpack_from('<QQ', elf, so + 0x18)
        sh_link = struct.unpack_from('<I', elf, so + 0x28)[0]
        stro = e_shoff + sh_link * e_shentsz
        str_off = struct.unpack_from('<Q', elf, stro + 0x18)[0]
        for j in range(sh_size // 24):
            symoff = sh_offset + j * 24
            st_name = struct.unpack_from('<I', elf, symoff)[0]
            st_value = struct.unpack_from('<Q', elf, symoff + 8)[0]
            p = str_off + st_name
            if elf[p:p + len(needle)] == needle:
                return st_value
    return None

start64 = find_symbol('_start64')
assert start64, '_start64 symbol not found'
ksectors = (len(kernel) + 511) // 512
print(f'kernel: base=0x{base:x} size={len(kernel)} ({ksectors} sectors) '
      f'_start64=0x{start64:x} e_entry=0x{e_entry:x}')

# ============================================================
# STAGE 2 (@ 0x8000): 内核加载器 + 16→32→64 trampoline
# ============================================================
S2_BASE = 0x8000
s2 = bytearray()

def emit(data):
    s2.extend(data)

def debug16(ch):
    """16-bit 模式 debug 写 (mov dx, imm16 = 3 字节)"""
    emit(bytes([0xBA, 0x00, 0x05]))
    emit(bytes([0xB0, ch]))
    emit(bytes([0xEE]))

def debug32(ch):
    """32/64-bit 模式 debug 写 (mov edx, imm32 = 5 字节)"""
    emit(bytes([0xBA, 0x00, 0x05, 0x00, 0x00]))
    emit(bytes([0xB0, ch]))
    emit(bytes([0xEE]))

def fail16(ch):
    """16-bit 失败路径: debug 写 ch + hlt 循环"""
    debug16(ch)
    emit(bytes([0xF4]))            # hlt
    emit(bytes([0xEB, 0xFD]))      # jmp 回 hlt (disp=-3)

# ---- 16-bit 入口 (offset 0) ----
entry_16_off = len(s2)
debug16(0x41)                      # 'A' 进入 stage 2

emit(bytes([0xFA]))                # cli
emit(bytes([0x31, 0xC0]))          # xor ax, ax
emit(bytes([0x8E, 0xD8]))          # mov ds, ax
emit(bytes([0x8E, 0xC0]))          # mov es, ax
emit(bytes([0x8E, 0xD0]))          # mov ss, ax
emit(bytes([0xBC, 0x00, 0xF0]))    # mov sp, 0xF000

# ---- A20 (fast gate, port 0x92) ----
# 注意: 必须用立即数端口形式 E4/E6 (in al,imm8 / out imm8,al);
# ED/EE 是 DX 端口形式, 会把 DX(=0x501) 当端口 → A20 从未开启!
emit(bytes([0xE4, 0x92]))          # in al, 0x92
emit(bytes([0x0C, 0x02]))          # or al, 0x02  (bit1 = A20 enable)
emit(bytes([0x24, 0xFE]))          # and al, 0xFE (bit0 = system reset, 必须保持 0)
emit(bytes([0xE6, 0x92]))          # out 0x92, al
debug16(0x61)                      # 'a'

# ---- EDD 检查 (AH=41h) ----
emit(bytes([0xB4, 0x41]))          # mov ah, 0x41
emit(bytes([0xBB, 0xAA, 0x55]))    # mov bx, 0x55AA
emit(bytes([0x8A, 0x16, 0x00, 0x05]))  # mov dl, [0x500]
emit(bytes([0xCD, 0x13]))          # int 0x13
edd_jc_off = len(s2); emit(bytes([0x72, 0x00]))   # jc edd_fail (patch)
emit(bytes([0x81, 0xFB, 0x55, 0xAA]))  # cmp bx, 0xAA55
edd_jne_off = len(s2); emit(bytes([0x75, 0x00]))  # jne edd_fail (patch)
emit(bytes([0xEB, 0x00]))          # jmp 跳过失败桩 (patch)
edd_jmp_off = len(s2) - 1
edd_fail = len(s2)
fail16(0x58)                       # 'X' EDD 不支持
read_fail = len(s2)
fail16(0x72)                       # 'r' 读盘失败 (放在 EDD 桩之后,
                                  #  让 'c' 不再跌入)
edd_past = len(s2)
s2[edd_jc_off + 1]  = edd_fail - (edd_jc_off + 2)
s2[edd_jne_off + 1] = edd_fail - (edd_jne_off + 2)
assert 0 <= edd_past - (edd_jmp_off + 1) <= 127, 'EDD jump-over rel8 out of range'
s2[edd_jmp_off]     = edd_past - (edd_jmp_off + 1)
debug16(0x62)                      # 'b'

# ---- DAPS 初始化 (暂存区 0x10000; lba/总扇区数稍后 patch) ----
emit(bytes([0xC6, 0x06, 0x00, 0x60, 0x10]))          # mov byte [0x6000], 0x10
emit(bytes([0xC6, 0x06, 0x01, 0x60, 0x00]))          # mov byte [0x6001], 0
emit(bytes([0xC7, 0x06, 0x04, 0x60, 0x00, 0x00]))    # mov word [0x6004], 0    (buf off)
emit(bytes([0xC7, 0x06, 0x06, 0x60, 0x00, 0x10]))    # mov word [0x6006], 0x1000 (buf seg → 0x10000)
emit(bytes([0x66, 0xC7, 0x06, 0x08, 0x60]))          # mov dword [0x6008], lba
lba_patch = len(s2); emit(struct.pack('<I', 0))
emit(bytes([0xC7, 0x06, 0x0C, 0x60, 0x00, 0x00]))    # mov word [0x600C], 0    (lba 高位)
emit(bytes([0xC7, 0x06, 0x02, 0x05]))                # mov word [0x502], 总扇区数
total_patch = len(s2); emit(struct.pack('<H', 0))
emit(bytes([0x66, 0xC7, 0x06, 0x04, 0x05]))          # mov dword [0x504], 0x200000 (dst)
emit(struct.pack('<I', KERNEL_LOAD))

# ---- LGDT (循环前一次性加载; 每块 PM 复制与最终 trampoline 共用) ----
emit(bytes([0x66, 0x0F, 0x01, 0x16]))    # lgdt [disp16] (66 前缀, 16-bit 直接寻址)
gdt_addr_patch = len(s2)
emit(struct.pack('<H', 0))
debug16(0x42)                            # 'B' GDT 已加载

# ---- 加载循环: int13h → 0x10000 暂存 → 进 PM rep movsd → 0x200000+ → 退 PM ----
# (SeaBIOS 1.17 的 INT 15h AH=87h 内部三重故障, 改为每块自行进出保护模式复制;
#  int13h AH=42h 大扇区读已实测可用, PM 平坦段复制不依赖任何 BIOS)
load_loop = len(s2)
emit(bytes([0xA1, 0x02, 0x05]))          # mov ax, [0x502]
emit(bytes([0x3D, 0x7F, 0x00]))          # cmp ax, 127
emit(bytes([0x76, 0x03]))                # jbe have_count
emit(bytes([0xB8, 0x7F, 0x00]))          # mov ax, 127
# have_count:
emit(bytes([0x89, 0xC3]))                # mov bx, ax        (bx = 本次扇区数)
emit(bytes([0x89, 0x1E, 0x0A, 0x05]))    # mov [0x50A], bx   (备份, 防 BIOS 破坏)
emit(bytes([0xA3, 0x02, 0x60]))          # mov [0x6002], ax  (DAPS count)
emit(bytes([0x29, 0x1E, 0x02, 0x05]))    # sub [0x502], bx   (剩余 -=)
emit(bytes([0xBE, 0x00, 0x60]))          # mov si, 0x6000
emit(bytes([0xB4, 0x42]))                # mov ah, 0x42
emit(bytes([0x8A, 0x16, 0x00, 0x05]))    # mov dl, [0x500]
emit(bytes([0xCD, 0x13]))                # int 0x13
read_fail_off = len(s2)                  # jc read_fail (near16, patch)
emit(bytes([0x0F, 0x82, 0x00, 0x00]))
_rf_disp = read_fail - (read_fail_off + 6)
assert -32768 <= _rf_disp <= 32767, f'jc read_fail rel16 out of range: {_rf_disp}'
struct.pack_into('<h', s2, read_fail_off + 2, _rf_disp)
emit(bytes([0x8B, 0x1E, 0x0A, 0x05]))    # mov bx, [0x50A] (int13h 可能破坏 bx)
debug16(0x65)                            # 'e' 本块读盘成功

# ==== 16-bit 实模式复制: 暂存区 0x10000 → [0x504] ====
# 用 32-bit 寻址 (rep movsd with addr32 前缀) + 段基址=0;
# 这样 ESI/EDI 就是线性地址, 直接覆盖 0x200000+。
# 这避开了 PM-exit/再入的脆弱时序。
emit(bytes([0x31, 0xC0]))                # xor ax, ax
emit(bytes([0x8E, 0xD8]))                # mov ds, ax   (段基=0, 线性=偏移)
emit(bytes([0x8E, 0xC0]))                # mov es, ax
emit(bytes([0xBE, 0x00, 0x00, 0x01, 0x00]))      # mov esi, 0x10000 (源)
emit(bytes([0x66, 0x8B, 0x3E, 0x04, 0x05]))      # mov edi, [0x0504] (当前 dst, 线性)
emit(bytes([0x0F, 0xB7, 0xCB]))          # movzx ecx, bx
emit(bytes([0xC1, 0xE1, 0x07]))          # shl ecx, 7  (dword 数 = 扇区×128)
emit(bytes([0xFC]))                      # cld
emit(bytes([0xF3, 0x66, 0x67, 0xA5]))      # addr32 rep movsd (32-bit data + 32-bit addr)
debug16(0x66)                            # 'f' 本块复制完成

debug16(0x31)                            # '1' after rep movsd, before mov bx
emit(bytes([0x8B, 0x1E, 0x0A, 0x05]))    # mov bx, [0x50A]
debug16(0x32)                            # '2' after mov bx
emit(bytes([0x66, 0x0F, 0xB7, 0xC3]))    # movzx eax, bx
debug16(0x33)                            # '3' after movzx
emit(bytes([0x66, 0x01, 0x06, 0x08, 0x60]))  # add [0x6008], eax (lba += count)
debug16(0x34)                            # '4' after lba update
emit(bytes([0x66, 0xC1, 0xE0, 0x09]))    # shl eax, 9
emit(bytes([0x66, 0x01, 0x06, 0x04, 0x05]))  # add [0x0504], eax  (dst += count×512)
debug16(0x35)                            # '5' after dst update
emit(bytes([0x83, 0x3E, 0x02, 0x05, 0x00]))  # cmp word [0x502], 0
debug16(0x36)                            # '6' after cmp, before jnz
_jnz_off = len(s2)                       # jnz load_loop (near16, patch)
emit(bytes([0x0F, 0x85, 0x00, 0x00]))
struct.pack_into('<h', s2, _jnz_off + 2, load_loop - (_jnz_off + 6))
debug16(0x63)                            # 'c' 内核加载完成

# ---- 开保护模式 (最终 trampoline; LGDT 已在循环前完成) ----
emit(bytes([0x66, 0x0F, 0x20, 0xC0]))              # mov eax, cr0
emit(bytes([0x66, 0x0D, 0x01, 0x00, 0x00, 0x00]))  # or eax, 1
emit(bytes([0x66, 0x0F, 0x22, 0xC0]))              # mov cr0, eax

# ---- 远跳 32-bit ----
emit(bytes([0x66, 0xEA]))
pm_entry_placeholder = len(s2)
emit(struct.pack('<I', 0))
emit(struct.pack('<H', 0x08))

# ==== 32-bit PM 入口 ====
pm_entry_off = len(s2)
struct.pack_into('<I', s2, pm_entry_placeholder, S2_BASE + pm_entry_off)

debug32(0x43)                            # 'C'

emit(bytes([0xB8, 0x10, 0x00, 0x00, 0x00]))  # mov eax, 0x10
emit(bytes([0x8E, 0xD8]))                # mov ds, ax
emit(bytes([0x8E, 0xC0]))                # mov es, ax
emit(bytes([0x8E, 0xE0]))                # mov fs, ax
emit(bytes([0x8E, 0xE8]))                # mov gs, ax
emit(bytes([0x8E, 0xD0]))                # mov ss, ax
emit(bytes([0xBC, 0x00, 0x00, 0x0F, 0x00]))  # mov esp, 0xF0000

debug32(0x44)                            # 'D'

# ---- 清零页表区 16KB @ 0x10000 ----
emit(bytes([0xBF, 0x00, 0x00, 0x01, 0x00]))  # mov edi, 0x10000
emit(bytes([0xB9, 0x00, 0x10, 0x00, 0x00]))  # mov ecx, 0x1000
emit(bytes([0x31, 0xC0]))                # xor eax, eax
clear_loop = len(s2)
emit(bytes([0x89, 0x07]))                # mov [edi], eax
emit(bytes([0x83, 0xC7, 0x04]))          # add edi, 4
emit(bytes([0x49]))                      # dec ecx
emit(bytes([0x75, (clear_loop - (len(s2) + 2)) & 0xFF]))

debug32(0x45)                            # 'E'

# ---- 页表项 ----
# PML4[0] = PDPT @ 0x11000 | 3
emit(bytes([0xB8, 0x00, 0x10, 0x01, 0x00]))
emit(bytes([0x0D, 0x03, 0x00, 0x00, 0x00]))
emit(bytes([0x89, 0x04, 0x25]))
emit(struct.pack('<I', 0x10000))
# PDPT[0] = PD0 @ 0x12000 | 3
emit(bytes([0xB8, 0x00, 0x20, 0x01, 0x00]))
emit(bytes([0x0D, 0x03, 0x00, 0x00, 0x00]))
emit(bytes([0x89, 0x04, 0x25]))
emit(struct.pack('<I', 0x11000))
# PDPT[1] = PD1 @ 0x13000 | 3
emit(bytes([0xB8, 0x00, 0x30, 0x01, 0x00]))
emit(bytes([0x0D, 0x03, 0x00, 0x00, 0x00]))
emit(bytes([0x89, 0x04, 0x25]))
emit(struct.pack('<I', 0x11008))

debug32(0x46)                            # 'F'

# ---- 填 PD0+PD1: 1024 项 × 2MB 大页 = 恒等映射 0-2GB ----
emit(bytes([0x31, 0xED]))                # xor ebp, ebp (高 32 位=0)
emit(bytes([0xBF, 0x00, 0x20, 0x01, 0x00]))  # mov edi, 0x12000
emit(bytes([0x31, 0xC9]))                # xor ecx, ecx
pd_loop = len(s2)
emit(bytes([0x89, 0xC8]))                # mov eax, ecx
emit(bytes([0xC1, 0xE0, 0x15]))          # shl eax, 21
emit(bytes([0x0D, 0x83, 0x00, 0x00, 0x00]))  # or eax, 0x83 (P+W+PS)
emit(bytes([0x89, 0x07]))                # mov [edi], eax
emit(bytes([0x89, 0x6F, 0x04]))          # mov [edi+4], ebp
emit(bytes([0x83, 0xC7, 0x08]))          # add edi, 8
emit(bytes([0x41]))                      # inc ecx
emit(bytes([0x81, 0xF9, 0x00, 0x04, 0x00, 0x00]))  # cmp ecx, 1024
emit(bytes([0x0F, 0x82]) + struct.pack('<i', pd_loop - (len(s2) + 6)))

debug32(0x47)                            # 'G'

# ---- CR4.PAE ----
emit(bytes([0x0F, 0x20, 0xE0]))          # mov eax, cr4
emit(bytes([0x0D, 0x20, 0x00, 0x00, 0x00]))  # or eax, 0x20
emit(bytes([0x0F, 0x22, 0xE0]))          # mov cr4, eax
# ---- CR3 = PML4 ----
emit(bytes([0xB8, 0x00, 0x00, 0x01, 0x00]))  # mov eax, 0x10000
emit(bytes([0x0F, 0x22, 0xD8]))          # mov cr3, eax
# ---- EFER.LME ----
emit(bytes([0xB9, 0x80, 0x00, 0x00, 0xC0]))  # mov ecx, 0xC0000080
emit(bytes([0x0F, 0x32]))                # rdmsr
emit(bytes([0x0D, 0x00, 0x01, 0x00, 0x00]))  # or eax, 0x100
emit(bytes([0x0F, 0x30]))                # wrmsr
# ---- CR0.PG (进长模式) ----
emit(bytes([0x0F, 0x20, 0xC0]))          # mov eax, cr0
emit(bytes([0x05, 0x00, 0x00, 0x00, 0x80]))  # or eax, 0x80000000
emit(bytes([0x0F, 0x22, 0xC0]))          # mov cr0, eax

debug32(0x48)                            # 'H'

# ---- 远跳 64-bit ----
emit(bytes([0xEA]))
lm64_placeholder = len(s2)
emit(struct.pack('<I', 0))
emit(struct.pack('<H', 0x18))

# ==== 64-bit 入口 ====
lm64_off = len(s2)
struct.pack_into('<I', s2, lm64_placeholder, S2_BASE + lm64_off)

debug32(0x4B)                            # 'K'

# ---- 栈 + 跳入 Rust 内核 ----
emit(bytes([0x48, 0xBC]) + struct.pack('<Q', 0x15008))   # mov rsp, 0x15008 (16-byte 对齐)
debug32(0x4A)                            # 'J' 即将跳转
debug32(0x4C)                            # 'L' jmp rax 之前
emit(bytes([0x48, 0xB8]) + struct.pack('<Q', start64))   # mov rax, _start64
emit(bytes([0xFF, 0xE0]))                                # jmp rax
# 兜底
emit(bytes([0xF4]))                                      # hlt
emit(bytes([0xEB, 0xFD]))                                # jmp .

# ---- GDT (代码之后, 8 字节对齐) ----
while len(s2) % 8 != 0:
    emit(bytes([0x90]))

gdt_off = len(s2)
emit(struct.pack('<Q', 0))                       # NULL
emit(struct.pack('<Q', 0x00CF9A000000FFFF))      # 0x08: 32-bit code
emit(struct.pack('<Q', 0x00CF92000000FFFF))      # 0x10: 32-bit data
emit(struct.pack('<Q', 0x00AF9A000000FFFF))      # 0x18: 64-bit code (L=1, G=1, limit=0xFFFFF)
emit(struct.pack('<Q', 0x00AF92000000FFFF))      # 0x20: 64-bit data (DPL=0)

gdt_ptr_off = len(s2)
emit(struct.pack('<H', 39))
emit(struct.pack('<I', S2_BASE + gdt_off))

struct.pack_into('<H', s2, gdt_addr_patch, S2_BASE + gdt_ptr_off)

n_s2 = (len(s2) + 511) // 512
kernel_lba = 1 + n_s2
struct.pack_into('<I', s2, lba_patch, kernel_lba)
struct.pack_into('<H', s2, total_patch, ksectors)
assert n_s2 * 512 >= len(s2)
print(f'Stage 2: {len(s2)} bytes ({n_s2} sectors), kernel LBA={kernel_lba}')

# ============================================================
# STAGE 1 (boot sector @ 0x7C00): EDD 加载 stage 2 → 0x8000
# ============================================================
boot = bytearray(512)
bpos = 0

def bemit(data):
    global bpos
    boot[bpos:bpos + len(data)] = data
    bpos += len(data)

bemit(bytes([0xFA]))                     # cli
bemit(bytes([0xBA, 0x02, 0x04]))         # mov dx, 0x402
bemit(bytes([0xB0, 0x31]))               # mov al, '1' (entry)
bemit(bytes([0xEE]))                     # out dx, al
bemit(bytes([0x31, 0xC0]))               # xor ax, ax
bemit(bytes([0x8E, 0xD8]))               # mov ds, ax
bemit(bytes([0x8E, 0xC0]))               # mov es, ax
bemit(bytes([0x8E, 0xD0]))               # mov ss, ax
bemit(bytes([0xBC, 0x00, 0x7C]))         # mov sp, 0x7C00
bemit(bytes([0xB0, 0x80]))               # mov al, 0x80 (hd0 - SeaBIOS 跳过来时 DL 可能非标准)
bemit(bytes([0x88, 0x06, 0x00, 0x05]))   # mov [0x500], al (统一存为 0x80)

# 用 DL=0x80 (标准 hd0) 调 EDD AH=42h (SeaBIOS 报告有 2 个 HDD, DL=0x80..0x81)
bemit(bytes([0xBA, 0x02, 0x04]))         # mov dx, 0x402
bemit(bytes([0xB0, 0x32]))               # mov al, '2' (pre-read)
bemit(bytes([0xEE]))                     # out dx, al
bemit(bytes([0xB2, 0x80]))               # mov dl, 0x80 (hd0)
# AH=42h LBA 读
bemit(bytes([0xBE, 0x00, 0x60]))         # mov si, 0x6000 (DAPS)
bemit(bytes([0xB4, 0x42]))               # mov ah, 0x42
bemit(bytes([0xCD, 0x13]))               # int 0x13
bemit(bytes([0xBA, 0x02, 0x04]))         # mov dx, 0x402
bemit(bytes([0xB0, 0x43]))               # mov al, 'C'
bemit(bytes([0xEE]))                     # out dx, al
bemit(bytes([0x9C]))                     # pushf
bemit(bytes([0x58]))                     # pop ax (flags)
bemit(bytes([0x24, 0x01]))               # and al, 0x01 (CF)
bemit(bytes([0x04, 0x30]))               # add al, '0'
bemit(bytes([0xEE]))                     # out dx, al
bemit(bytes([0xB0, 0x41]))               # mov al, 'A'
bemit(bytes([0xEE]))                     # out dx, al
bemit(bytes([0xB0, 0x30]))               # mov al, '0'
bemit(bytes([0x8A, 0xC4]))               # mov al, ah
bemit(bytes([0x04, 0x30]))               # add al, '0'
bemit(bytes([0xEE]))                     # out dx, al
bemit(bytes([0xB0, 0x0A]))               # mov al, '\n'
bemit(bytes([0xEE]))                     # out dx, al
bf3 = bpos; bemit(bytes([0x72, 0x00]))   # jc fail

# DAPS @ 0x6000: 读 stage 2 (lba=1, buf=0x0800:0x0000 → 0x8000)
bemit(bytes([0xC6, 0x06, 0x00, 0x60, 0x10]))        # size=0x10
bemit(bytes([0xC6, 0x06, 0x01, 0x60, 0x00]))        # reserved
bemit(bytes([0xC7, 0x06, 0x02, 0x60]) + struct.pack('<H', n_s2))  # count
bemit(bytes([0xC7, 0x06, 0x04, 0x60, 0x00, 0x00]))  # off=0
bemit(bytes([0xC7, 0x06, 0x06, 0x60, 0x00, 0x08]))  # seg=0x0800
bemit(bytes([0x66, 0xC7, 0x06, 0x08, 0x60, 0x01, 0x00, 0x00, 0x00]))  # lba=1
bemit(bytes([0xC7, 0x06, 0x0C, 0x60, 0x00, 0x00]))  # lba 高位

bemit(bytes([0xBE, 0x00, 0x60]))         # mov si, 0x6000
bemit(bytes([0xB4, 0x42]))               # mov ah, 0x42
bemit(bytes([0xB2, 0x80]))               # mov dl, 0x80 (hd0)
bemit(bytes([0xCD, 0x13]))               # int 0x13
bemit(bytes([0xBA, 0x02, 0x04]))         # mov dx, 0x402
bemit(bytes([0xB0, 0x44]))               # mov al, 'D' (2nd EDD)
bemit(bytes([0xEE]))                     # out dx, al
bemit(bytes([0x9C]))                     # pushf
bemit(bytes([0x58]))                     # pop ax
bemit(bytes([0x24, 0x01]))               # and al, 0x01
bemit(bytes([0x04, 0x30]))               # add al, '0'
bemit(bytes([0xEE]))                     # out dx, al
bemit(bytes([0xB0, 0x0A]))               # mov al, '\n'
bemit(bytes([0xEE]))                     # out dx, al
bf3 = bpos; bemit(bytes([0x72, 0x00]))   # jc fail

bemit(bytes([0xEA, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00]))  # jmp 0000:8000

fail_off = bpos
bemit(bytes([0xBA, 0x02, 0x04]))         # mov dx, 0x402
bemit(bytes([0xB0, 0x5A]))               # mov al, 0x5A
bemit(bytes([0xEE]))                     # out dx, al
bemit(bytes([0xF4]))                     # hlt
bemit(bytes([0xEB, 0xFD]))               # jmp hlt

boot[bf3 + 1] = fail_off - (bf3 + 2)

assert bpos <= 510, f'boot sector overflow: {bpos}'
while bpos < 510:
    boot[bpos] = 0x90
    bpos += 1
boot[510] = 0x55
boot[511] = 0xAA

# ============================================================
# 镜像组装: stage1 + stage2 + kernel.bin, 16MB
# ============================================================
s2_padded = s2 + bytearray(n_s2 * 512 - len(s2))
kernel_padded = kernel + bytearray(ksectors * 512 - len(kernel))

img = boot + s2_padded + kernel_padded
total_used = len(img) // 512
assert total_used <= IMG_SECTORS, f'image overflow: {total_used} > {IMG_SECTORS}'
img += bytearray(IMG_SECTORS * 512 - len(img))

with open(IMG, 'wb') as f:
    f.write(img)
print(f'{IMG}: {len(img)} bytes, used {total_used} sectors '
      f'(stage1=1, stage2={n_s2}@lba1, kernel={ksectors}@lba{kernel_lba})')
