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
  64-bit : 设 esp=0x60008 (避开 isa-bios ROM 区; retf 后入口 rsp%16==8) → retf _start64

低内存数据区约定:
  0x500 : 启动盘号 (stage 1 保存 BIOS DL)
  0x502 : 内核剩余扇区数 (word)
  0x504 : PM 复制目标物理地址 (dword, 初始 0x200000, 每块推进)
  0x50A : 当前块扇区数备份 (word, 防 BIOS 调用破坏 bx)
  0x6000: DAPS 磁盘地址包 (AH=42h, 缓冲固定 0x1000:0x0000 → 0x10000)
  0x10000: 读盘暂存区 (≤63.5KB; 加载完成后被页表复用)

调试检查点 (port 0x402, 配合 -device isa-debugcon):
  'A' 进入 stage2, 'a' A20 已开, 'b' EDD 可用, 'B' LGDT 完成,
  'e' 每块 int13h 读成功, 'f' 每块 PM 复制完成, 'c' 内核加载完成,
  'M' E820 start, 'p' E820 探测失败→硬编码 fallback,
  'N' E820 done + 1 字节条目数回显 (P6.0-T1 真实探测),
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
INITRD = sys.argv[3] if len(sys.argv) > 3 else None   # P4-T5: cpio newc initramfs
IMG_SECTORS = 32768          # 16 MB
KERNEL_LOAD = 0x200000       # 链接基址
# P6.0-T1: 0x21000。原 0x20100 会与真实 E820 探测结果冲突 —— 127 条上限时
# entries 区最多铺到 0x20004 + 127×24 = 0x20BE4，0x21000 在其后且仍远低于
# 内核加载基址 0x200000。stage2 写 {base u64, size u64}；内核 initrd.rs 消费。
INITRD_INFO_ADDR = 0x21000
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
# P4-T5 initramfs: cpio newc 归档追加到 kernel.bin 之后，stage2
# 连续加载 (kernel + initramfs) 到 0x200000+，并在 0x21000 写
# {base, size} 记录。initrd_base = 0x200000 + ksectors*512（构建期常量）。
# ============================================================
if INITRD:
    with open(INITRD, 'rb') as f:
        initrd = f.read()
    assert initrd[:6] == b'070701', f'initramfs {INITRD} not cpio newc'
else:
    initrd = b''
ird_sectors = (len(initrd) + 511) // 512
initrd_base = KERNEL_LOAD + ksectors * 512
initrd_size = len(initrd)
total_sectors = ksectors + ird_sectors
assert total_sectors < 0x10000, f'kernel+initrd {total_sectors} sectors overflow u16 counter'
print(f'initramfs: {initrd_size} bytes ({ird_sectors} sectors) -> base=0x{initrd_base:x}; '
      f'total load = {total_sectors} sectors')

# ============================================================
# STAGE 2 (@ 0x8000): 内核加载器 + 16→32→64 trampoline
# ============================================================
S2_BASE = 0x8000
s2 = bytearray()

def emit(data):
    s2.extend(data)

def debug16(ch):
    """16-bit 模式 debug 写 (mov dx, imm16 = 3 字节)"""
    emit(bytes([0xBA, 0x02, 0x04]))      # mov dx, 0x402 (isa-debugcon port)
    emit(bytes([0xB0, ch]))
    emit(bytes([0xEE]))

def debug32(ch):
    """32/64-bit 模式 debug 写 (mov edx, imm32 = 5 字节)"""
    emit(bytes([0xBA, 0x02, 0x04, 0x00, 0x00]))  # mov edx, 0x402
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

# 重置 DS=0 (EDD 检查可能破坏 DS)
emit(bytes([0x31, 0xC0]))          # xor ax, ax
emit(bytes([0x8E, 0xD8]))          # mov ds, ax

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
emit(bytes([0x31, 0xC0]))                # xor ax, ax        # 重置 DS=0
emit(bytes([0x8E, 0xD8]))                # mov ds, ax        # (BIOS 可能破坏 DS)
emit(bytes([0xBE, 0x00, 0x60]))          # mov si, 0x6000
emit(bytes([0xB4, 0x42]))                # mov ah, 0x42
emit(bytes([0x8A, 0x16, 0x00, 0x05]))    # mov dl, [0x500]
emit(bytes([0xCD, 0x13]))                # int 0x13
read_fail_off = len(s2)                  # jc read_fail (near16, patch)
emit(bytes([0x0F, 0x82, 0x00, 0x00]))
_rf_disp = read_fail - (read_fail_off + 6)
assert -32768 <= _rf_disp <= 32767, f'jc read_fail rel16 out of range: {_rf_disp}'
struct.pack_into('<h', s2, read_fail_off + 2, _rf_disp)
# 重置 DS=0 (BIOS int13h 可能破坏 DS)
emit(bytes([0x31, 0xC0]))                # xor ax, ax
emit(bytes([0x8E, 0xD8]))                # mov ds, ax
emit(bytes([0x8B, 0x1E, 0x0A, 0x05]))    # mov bx, [0x50A] (int13h 可能破坏 bx)
debug16(0x65)                            # 'e' 本块读盘成功

# 读 buffer 0x10000 前 4 字节并输出（诊断: 每块回读应与 kern[N*0xFE00] 一致）
# 注：旧版"移除此代码会导致启动失败"之谜已解 —— 是复制段 mov esi 缺 0x66
# 前缀, ESI 高位靠此处 66 BE 残留才凑巧正确; 前缀已修复, 本段仅留作诊断。
emit(bytes([0x31, 0xC0]))                # xor ax, ax
emit(bytes([0x8E, 0xD8]))                # mov ds, ax
emit(bytes([0xBA, 0x02, 0x04]))          # mov dx, 0x402
# 读 buffer[0..4] 两次 + buffer[0x7140..0x7144] 一次 (共 12 字节诊断输出)
# 两次读 [0..4] 若不一致 → 内存在变 (异步 DMA?); 一致 → buffer 内容即如此
for _esi in (0x10000, 0x10000, 0x17140):
    emit(bytes([0x66, 0xBE]) + struct.pack('<I', _esi))  # mov esi, _esi
    for _ in range(4):
        emit(bytes([0x67, 0xAC]))          # lodsb
        emit(bytes([0xEE]))                # out dx, al

# ==== 16-bit 实模式复制: 暂存区 0x10000 → [0x504] ====
# 用 32-bit 寻址 (rep movsd with addr32 前缀) + 段基址=0;
# 这样 ESI/EDI 就是线性地址, 直接覆盖 0x200000+。
# 这避开了 PM-exit/再入的脆弱时序。
emit(bytes([0x31, 0xC0]))                # xor ax, ax
emit(bytes([0x8E, 0xD8]))                # mov ds, ax   (段基=0, 线性=偏移)
emit(bytes([0x8E, 0xC0]))                # mov es, ax
# ⚠️ 必须带 0x66 前缀 (mov esi, imm32)！无前缀时 BE 00 00 只是 mov si,0，
# 且后续 01 00 被解码为 add [bx+si],ax (向 0x7F 加 0, 侥幸无害)；
# ESI 高 16 位只能靠前面诊断代码的 66 BE 残留 —— 这就是"删掉 buffer 检查
# 就无法启动"之谜的真相。
emit(bytes([0x66, 0xBE, 0x00, 0x00, 0x01, 0x00]))  # mov esi, 0x10000 (源)
emit(bytes([0x66, 0x8B, 0x3E, 0x04, 0x05]))      # mov edi, [0x0504] (当前 dst, 线性)
emit(bytes([0x0F, 0xB7, 0xCB]))          # movzx ecx, bx
emit(bytes([0xC1, 0xE1, 0x07]))          # shl ecx, 7  (dword 数 = 扇区×128)
emit(bytes([0xFC]))                      # cld
emit(bytes([0xF3, 0x66, 0x67, 0xA5]))      # addr32 rep movsd (32-bit data + 32-bit addr)
debug16(0x66)                            # 'f' 本块复制完成

# ---- lba/dst 推进 ----
# ⚠️ 严禁在 movzx eax,bx 与两条 add 之间插入 debug16！
# debug16 的 `mov al, imm8` 会污染 EAX 低字节。历史 bug（本镜像损坏之谜的
# 根因）：marker '3'(0x33) 使 LBA += 51 (应为 127)，marker '4'(0x34) 经
# shl 9 使 dst += 0x6800 (应为 0xFE00) → 各块以错误 LBA 读盘、以重叠错位
# 方式写入 RAM，最终镜像仅 ~14% 字节正确，内核在 0x207143 (#BP) 崩溃。
emit(bytes([0x8B, 0x1E, 0x0A, 0x05]))    # mov bx, [0x50A]
emit(bytes([0x66, 0x0F, 0xB7, 0xC3]))    # movzx eax, bx
emit(bytes([0x66, 0x01, 0x06, 0x08, 0x60]))  # add [0x6008], eax (lba += count)
emit(bytes([0x66, 0xC1, 0xE0, 0x09]))    # shl eax, 9
emit(bytes([0x66, 0x01, 0x06, 0x04, 0x05]))  # add [0x0504], eax  (dst += count×512)
emit(bytes([0x83, 0x3E, 0x02, 0x05, 0x00]))  # cmp word [0x502], 0
_jnz_off = len(s2)                       # jnz load_loop (near16, patch)
emit(bytes([0x0F, 0x85, 0x00, 0x00]))
struct.pack_into('<h', s2, _jnz_off + 2, load_loop - (_jnz_off + 6))
debug16(0x63)                            # 'c' 内核加载完成

# ==== BIOS INT 15h AX=E820h 真实探测 (P6.0-T1, 16-bit 实模式) ====
#   QEMU/SeaBIOS 在 16-bit 实模式下提供 E820；32-bit PM 没有 IDT 不能直接调 INT 15h。
#   物理 0x20000 是 P2 阶段预留给 E820 的 4KB 区（与内核加载 0x200000+ 不重叠）。
#   输出布局 (与 memory_map.rs Rust 端解析契约一致):
#     count(u32 LE) @ 0x20000..0x20004; entries @ 0x20004.. (24 字节/条步长)
#   探测循环契约 (ACPI spec INT 15h E820 + SeaBIOS rel-1.17 实测):
#     每轮 EAX=0xE820 / EDX='SMAP' / ECX=20 / EBX=continuation / ES:DI=输出槽。
#     终止双判据: CF=1（ACPI 规范, 本轮槽无效丢弃）或 EBX 回绕 0（本轮槽有效
#     计入后退出）。⚠️ 本机 SeaBIOS rel-1.17.0 实测**从不置 CF**, 仅在返回
#     最后一条后把 continuation 归零 —— 只认 CF 会无限循环（P2-T1 挂起真因,
#     2026-09-27 debugcon 逐轮取证）。BIOS 在 ES:DI 写 20 字节
#     [base u64 | size u64 | type u32], 正是内核 E820Entry 记录的前 20 字节
#     —— 槽位每轮 +24B 前进, 尾随 4 字节留洞（个别 BIOS 带 attrs 写满 24B 也
#     恰好落在洞内）, 无需任何转换。
#   历史避坑（P2-T1 挂起：'PC 跳回 stage2 起点', 2026-09-26 记录）:
#     1. EAX/EDX/EBX/EDI/EBP 凡跨 int 使用必须 66 前缀 32-bit 全宽清零 ——
#        加载循环 rep movsd 使高 32 位残留大值, 仅 mov ax/dx 会带上残值。
#        （首轮"127 槽全同"的根因是终止判据缺失, 见上; 全宽清零仍属必需。）
#     2. 探测循环内严禁 debug16 —— 其 `mov al,imm` 污染 AL（同 LBA 推进区教训）。
#     3. int 15h 只保证保留 SI/DI/BP; EAX/EBX/ECX/EDX/DS/ES 均视为可破坏 ——
#        写 count 前重建 ES=0x2000, tail 统一重建 DS=0（后续 V/W/X 诊断依赖）。
#     4. 条目数 ≥127 保险退出; 0 条目（首轮即 CF=1/不支持）→ 回退 P2-T1 硬编码表。
#   debugcon 标记: 'M' start; 'p' 探测失败→fallback; 'N' done + 1 字节条目数回显。
debug16(0x4D)                              # 'M' E820 start
# ⚠️ 寄存器全宽赋值：实模式 16-bit 指令 (mov di/bx,imm16 / xor bp,bx) 只动低
# 16 位，高 16 位残留加载循环 rep movsd 的 EDI/EBX 大值 → BIOS 把 continuation
# 读成巨值 / 把输出地址读错。凡跨 int 使用的指针/计数寄存器一律 66 前缀
# 32-bit 赋值（挂起真因另见上方终止双判据注释）。
emit(bytes([0x31, 0xC0]))                   # xor ax, ax
emit(bytes([0x8E, 0xD8]))                   # mov ds, ax (DS=0)
emit(bytes([0x66, 0xB8, 0x00, 0x20, 0x00, 0x00]))  # mov eax, 0x2000 (ES 段基全宽)
emit(bytes([0x8E, 0xC0]))                   # mov es, ax   (线性基 0x20000)
emit(bytes([0x66, 0xBF, 0x04, 0x00, 0x00, 0x00]))  # mov edi, 0x0004 (首槽全宽清零!)
emit(bytes([0x66, 0xBD, 0x00, 0x00, 0x00, 0x00]))  # mov ebp, 0 (条目计数, 全宽)
emit(bytes([0x66, 0xBB, 0x00, 0x00, 0x00, 0x00]))  # mov ebx, 0 (continuation, 全宽!)
e820_probe_loop = len(s2)
emit(bytes([0x66, 0xB8, 0x20, 0xE8, 0x00, 0x00]))  # mov eax, 0xE820 (高 32 位清零!)
emit(bytes([0x66, 0xBA, 0x50, 0x41, 0x4D, 0x53]))  # mov edx, 'SMAP' (高 32 位清零!)
emit(bytes([0x66, 0xB9, 0x14, 0x00, 0x00, 0x00]))  # mov ecx, 20 (全宽; 非 EDNS → 恰好 20B 输出)
emit(bytes([0xCD, 0x15]))                   # int 0x15
# 终止双判据（2026-09-27 debugcon 逐轮取证，SeaBIOS rel-1.17.0 实测）:
#   1. CF=1 → 经典 ACPI 尽头信号，本轮槽未写入有效数据 → 丢弃本轮;
#   2. EBX 回绕 0 → 本轮已写入最后一条后 continuation 归零 (本机实测:
#      此 SeaBIOS 从不置 CF, 7 条后 bl=0 → 从头循环 —— 这正是 P2-T1 当年
#      无限循环挂起的真正根因, 而非寄存器污染) → 本轮有效, 计入后退出。
# 先 pushf/pop si 快照 flags（SI 高 16 位残留无碍, 只用 bit0）。
emit(bytes([0x9C]))                         # pushf
emit(bytes([0x5E]))                         # pop si
emit(bytes([0xF7, 0xC6, 0x01, 0x00]))      # test si, 1
_e820_jnz = len(s2); emit(bytes([0x75, 0x00]))   # jnz e820_probe_end (CF=1, patch)
emit(bytes([0xFF, 0xC5]))                   # inc bp (本轮槽数据有效, 先计数)
emit(bytes([0x85, 0xDB]))                   # test ebx, ebx (continuation 回绕?)
_e820_jz2 = len(s2); emit(bytes([0x74, 0x00]))   # jz e820_probe_end (列表尽头, patch)
emit(bytes([0x83, 0xC7, 0x18]))            # add di, 24 (下一槽数据区)
emit(bytes([0x83, 0xFD, 0x7F]))            # cmp bp, 127
_e820_jb = len(s2); emit(bytes([0x72, 0x00]))   # jb e820_probe_loop (patch)
_e820_end = len(s2)                        # probe_end 起点（三处 rel8 目标）
for _off in (_e820_jnz, _e820_jz2):
    _rel = _e820_end - (_off + 2)
    assert 0 <= _rel <= 127
    s2[_off + 1] = _rel
s2[_e820_jb + 1] = (e820_probe_loop - (len(s2) + 2)) & 0xFF
assert e820_probe_loop - (len(s2) + 2) == int.from_bytes(bytes([s2[_e820_jb + 1]]), 'big', signed=True)
# ---- 探测收尾: count 写回 ----
emit(bytes([0x85, 0xED]))                   # test bp, bp
_e820_jz = len(s2); emit(bytes([0x74, 0x00]))   # jz e820_fallback (patch)
emit(bytes([0xB8, 0x00, 0x20]))            # mov ax, 0x2000 (防御性重建 ES, int15h 出口可能已破坏)
emit(bytes([0x8E, 0xC0]))                   # mov es, ax
emit(bytes([0x26, 0x89, 0x2E, 0x00, 0x00])) # mov [es:0x0000], bp (count 低 16 位)
emit(bytes([0x26, 0xC7, 0x06, 0x02, 0x00, 0x00, 0x00]))  # mov word [es:0x0002], 0 (高 16 位)
_e820_jmp = len(s2); emit(bytes([0xEB, 0x00]))   # jmp e820_tail (patch)
# ---- fallback: P2-T1 硬编码 QEMU -m 128M 表 (数据在 stage2 末尾) ----
e820_fallback = len(s2)
debug16(0x70)                               # 'p' E820 探测失败 → 硬编码 fallback
emit(bytes([0xB8, 0x00, 0x08]))            # mov ax, 0x800 (stage2 加载段)
emit(bytes([0x8E, 0xD8]))                   # mov ds, ax
data_si_patch = len(s2)
emit(bytes([0xBE]) + struct.pack('<H', 0))  # mov si, <data_off> (patch)
emit(bytes([0xB8, 0x00, 0x20]))            # mov ax, 0x2000 (目标段)
emit(bytes([0x8E, 0xC0]))                   # mov es, ax
emit(bytes([0xBF, 0x04, 0x00]))            # mov di, 0x0004 (目标段内偏移 = 线性 0x20004)
emit(bytes([0xB9]) + struct.pack('<H', 7 * 24))  # mov cx, 168 (7 entries × 24 bytes)
emit(bytes([0xFC]))                         # cld
emit(bytes([0xF3, 0xA4]))                   # rep movsb
emit(bytes([0x31, 0xC0]))                   # xor ax, ax (count 写回用)
emit(bytes([0x8E, 0xD8]))                   # mov ds, ax
emit(bytes([0xBF, 0x00, 0x00]))            # mov di, 0 (目标段内偏移 0 = 线性 0x20000)
emit(bytes([0xB8, 0x07, 0x00]))            # mov ax, 7 (count)
emit(bytes([0x26, 0x89, 0x05]))            # mov [es:di], ax (count 低 16 位)
emit(bytes([0xB8, 0x00, 0x00]))            # mov ax, 0
emit(bytes([0x26, 0x89, 0x45, 0x02]))      # mov [es:di+2], ax (count 高 16 位 = 0)
# ---- 两条路径汇合 ----
e820_tail = len(s2)
s2[_e820_jz + 1] = (e820_fallback - (_e820_jz + 2)) & 0xFF
s2[_e820_jmp + 1] = (e820_tail - (_e820_jmp + 2)) & 0xFF
assert e820_fallback - (_e820_jz + 2) == int.from_bytes(bytes([s2[_e820_jz + 1]]), 'big', signed=True)
assert e820_tail - (_e820_jmp + 2) == int.from_bytes(bytes([s2[_e820_jmp + 1]]), 'big', signed=True)
emit(bytes([0x31, 0xC0]))                   # xor ax, ax (统一恢复 DS=0 给后续 V/W/X)
emit(bytes([0x8E, 0xD8]))                   # mov ds, ax
emit(bytes([0xB8, 0x00, 0x20]))            # mov ax, 0x2000 (重建 ES 供 count 回显)
emit(bytes([0x8E, 0xC0]))                   # mov es, ax
debug16(0x4E)                               # 'N' E820 done
emit(bytes([0xBA, 0x02, 0x04]))            # mov dx, 0x402
emit(bytes([0x26, 0x8A, 0x06, 0x00, 0x00])) # mov al, [es:0x0000] (count 低字节 ≤127)
emit(bytes([0xEE]))                          # out dx, al (条目数回显)

# ==== 诊断: 加载完成后回读 RAM 关键 dword (定位损坏发生阶段) ====
# 'V' + [0x200000] 4B | 'W' + [0x207140] 4B | 'X' + [0x20FE00] 4B (block1 首)
for _marker, _addr in ((0x56, 0x200000), (0x57, 0x207140), (0x58, 0x20FE00)):
    emit(bytes([0x31, 0xC0]))            # xor ax, ax
    emit(bytes([0x8E, 0xD8]))            # mov ds, ax
    emit(bytes([0xBA, 0x02, 0x04]))      # mov dx, 0x402
    emit(bytes([0xB0, _marker]))         # mov al, marker
    emit(bytes([0xEE]))                  # out dx, al
    emit(bytes([0x66, 0xBE]) + struct.pack('<I', _addr))  # mov esi, addr
    for _ in range(4):
        emit(bytes([0x67, 0xAC]))        # lodsb
        emit(bytes([0xEE]))              # out dx, al

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
emit(bytes([0xBC, 0x08, 0x00, 0x06, 0x00]))  # mov esp, 0x60008
# 内核栈: 必须避开 0xF0000-0xFFFFF (QEMU isa-bios ROM 区, 写入被静默忽略 →
# push/retf 弹到 ROM 垃圾 → 三重故障)。0x60000 处于页表(0x16000 结束)与
# EBDA/VGA(0x9FC00) 之间的空闲 RAM。
# 对齐: 2×push(8B) + retf 弹 8B 后, _start64 入口 RSP=0x60008 (满足 Rust ABI rsp%16==8)

debug32(0x44)                            # 'D'

# ---- P4-T5: 写 initramfs 引导记录到物理 0x21000（32-bit PM 平坦段，分页未开；P6.0-T1 自 0x20100 后移至 127 条 E820 表之外）----
#   {base u64, size u64}；构建期常量（initrd_base = 0x200000 + ksectors*512）。
#   mov dword [abs], imm32 编码：C7 04 25 <disp32> <imm32>（SIB 无基址寄存器）。
def mov_mem_imm32(addr, imm):
    emit(bytes([0xC7, 0x04, 0x25]))
    emit(struct.pack('<I', addr))
    emit(struct.pack('<I', imm))

mov_mem_imm32(INITRD_INFO_ADDR + 0x0, initrd_base & 0xFFFFFFFF)
mov_mem_imm32(INITRD_INFO_ADDR + 0x4, (initrd_base >> 32) & 0xFFFFFFFF)
mov_mem_imm32(INITRD_INFO_ADDR + 0x8, initrd_size & 0xFFFFFFFF)
mov_mem_imm32(INITRD_INFO_ADDR + 0xC, (initrd_size >> 32) & 0xFFFFFFFF)

# ---- 清零页表区 24KB @ 0x10000 (PML4+PDPT+PD0+PD1+PD2+PD3) ----
emit(bytes([0xBF, 0x00, 0x00, 0x01, 0x00]))  # mov edi, 0x10000
emit(bytes([0xB9, 0x00, 0x18, 0x00, 0x00]))  # mov ecx, 0x1800 (6144 dwords = 24KB)
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
# PDPT[2] = PD2 @ 0x14000 | 3
emit(bytes([0xB8, 0x00, 0x40, 0x01, 0x00]))
emit(bytes([0x0D, 0x03, 0x00, 0x00, 0x00]))
emit(bytes([0x89, 0x04, 0x25]))
emit(struct.pack('<I', 0x11010))
# PDPT[3] = PD3 @ 0x15000 | 3
emit(bytes([0xB8, 0x00, 0x50, 0x01, 0x00]))
emit(bytes([0x0D, 0x03, 0x00, 0x00, 0x00]))
emit(bytes([0x89, 0x04, 0x25]))
emit(struct.pack('<I', 0x11018))

debug32(0x46)                            # 'F'

# ---- 填 PD0: 512 项 × 2MB 大页 = 恒等映射 0-1GB ----
emit(bytes([0x31, 0xED]))                # xor ebp, ebp (高 32 位=0)
emit(bytes([0xBF, 0x00, 0x20, 0x01, 0x00]))  # mov edi, 0x12000 (PD0)
emit(bytes([0x31, 0xC9]))                # xor ecx, ecx
pd_loop = len(s2)
emit(bytes([0x89, 0xC8]))                # mov eax, ecx
emit(bytes([0xC1, 0xE0, 0x15]))          # shl eax, 21
emit(bytes([0x0D, 0x83, 0x00, 0x00, 0x00]))  # or eax, 0x83 (P+W+PS)
emit(bytes([0x89, 0x07]))                # mov [edi], eax
emit(bytes([0x89, 0x6F, 0x04]))          # mov [edi+4], ebp
emit(bytes([0x83, 0xC7, 0x08]))          # add edi, 8
emit(bytes([0x41]))                      # inc ecx
emit(bytes([0x81, 0xF9, 0x00, 0x02, 0x00, 0x00]))  # cmp ecx, 512
emit(bytes([0x0F, 0x82]) + struct.pack('<i', pd_loop - (len(s2) + 6)))

# ---- 填 PD1: 512 项 × 2MB 大页 = 恒等映射 1-2GB ----
emit(bytes([0xBF, 0x00, 0x30, 0x01, 0x00]))  # mov edi, 0x13000 (PD1)
pd_loop2 = len(s2)
emit(bytes([0x89, 0xC8]))                # mov eax, ecx
emit(bytes([0xC1, 0xE0, 0x15]))          # shl eax, 21
emit(bytes([0x0D, 0x83, 0x00, 0x00, 0x00]))  # or eax, 0x83 (P+W+PS)
emit(bytes([0x89, 0x07]))                # mov [edi], eax
emit(bytes([0x89, 0x6F, 0x04]))          # mov [edi+4], ebp
emit(bytes([0x83, 0xC7, 0x08]))          # add edi, 8
emit(bytes([0x41]))                      # inc ecx
emit(bytes([0x81, 0xF9, 0x00, 0x04, 0x00, 0x00]))  # cmp ecx, 1024
emit(bytes([0x0F, 0x82]) + struct.pack('<i', pd_loop2 - (len(s2) + 6)))

# ---- 填 PD2: 512 项 × 2MB 大页 = 恒等映射 2-3GB ----
emit(bytes([0xBF, 0x00, 0x40, 0x01, 0x00]))  # mov edi, 0x14000 (PD2)
pd_loop3 = len(s2)
emit(bytes([0x89, 0xC8]))                # mov eax, ecx
emit(bytes([0xC1, 0xE0, 0x15]))          # shl eax, 21
emit(bytes([0x0D, 0x83, 0x00, 0x00, 0x00]))  # or eax, 0x83 (P+W+PS)
emit(bytes([0x89, 0x07]))                # mov [edi], eax
emit(bytes([0x89, 0x6F, 0x04]))          # mov [edi+4], ebp
emit(bytes([0x83, 0xC7, 0x08]))          # add edi, 8
emit(bytes([0x41]))                      # inc ecx
emit(bytes([0x81, 0xF9, 0x00, 0x06, 0x00, 0x00]))  # cmp ecx, 1536
emit(bytes([0x0F, 0x82]) + struct.pack('<i', pd_loop3 - (len(s2) + 6)))

# ---- 填 PD3: 512 项 × 2MB 大页 = 恒等映射 3-4GB ----
emit(bytes([0xBF, 0x00, 0x50, 0x01, 0x00]))  # mov edi, 0x15000 (PD3)
pd_loop4 = len(s2)
emit(bytes([0x89, 0xC8]))                # mov eax, ecx
emit(bytes([0xC1, 0xE0, 0x15]))          # shl eax, 21
emit(bytes([0x0D, 0x83, 0x00, 0x00, 0x00]))  # or eax, 0x83 (P+W+PS)
emit(bytes([0x89, 0x07]))                # mov [edi], eax
emit(bytes([0x89, 0x6F, 0x04]))          # mov [edi+4], ebp
emit(bytes([0x83, 0xC7, 0x08]))          # add edi, 8
emit(bytes([0x41]))                      # inc ecx
emit(bytes([0x81, 0xF9, 0x00, 0x08, 0x00, 0x00]))  # cmp ecx, 2048
emit(bytes([0x0F, 0x82]) + struct.pack('<i', pd_loop4 - (len(s2) + 6)))

debug32(0x47)                            # 'G'

debug32(0x61)                            # debug: before CR4.PAE
# ---- CR4: PAE + OSFXSR + OSXMMEXCPT ----
# PAE(bit5)=1: 4级页表
# OSFXSR(bit9)=1: 启用 SSE/SSE2
# OSXMMEXCPT(bit10)=1: 启用 SSE 异常处理
emit(bytes([0x0F, 0x20, 0xE0]))          # mov eax, cr4
emit(bytes([0x0D, 0x20, 0x06, 0x00, 0x00]))  # or eax, 0x620 (PAE+OSFXSR+OSXMMEXCPT)
debug32(0x62)                            # debug: before write cr4
emit(bytes([0x0F, 0x22, 0xE0]))          # mov cr4, eax
debug32(0x63)                            # debug: CR4 written
# ---- CR3 = PML4 ----
emit(bytes([0xB8, 0x00, 0x00, 0x01, 0x00]))  # mov eax, 0x10000
emit(bytes([0x0F, 0x22, 0xD8]))          # mov cr3, eax
debug32(0x64)                            # debug: CR3 written
# ---- EFER.LME ----
emit(bytes([0xB9, 0x80, 0x00, 0x00, 0xC0]))  # mov ecx, 0xC0000080
emit(bytes([0x0F, 0x32]))                # rdmsr
debug32(0x65)                            # debug: rdmsr done
emit(bytes([0x0D, 0x00, 0x01, 0x00, 0x00]))  # or eax, 0x100
emit(bytes([0x0F, 0x30]))                # wrmsr
debug32(0x66)                            # debug: EFER written
# ---- CR0.PG (进长模式) ----
emit(bytes([0x0F, 0x20, 0xC0]))          # mov eax, cr0
emit(bytes([0x05, 0x00, 0x00, 0x00, 0x80]))  # or eax, 0x80000000
debug32(0x67)                            # debug: before write cr0
emit(bytes([0x0F, 0x22, 0xC0]))          # mov cr0, eax
debug32(0x68)                            # debug: CR0 written (paging on)

# ---- CR0: 清 EM(bit2) + TS(bit3) ----
# 不清 EM → Rust 的 fninit 会 #UD（EM=1 时所有 x87/SSE 指令非法）
emit(bytes([0x0F, 0x20, 0xC0]))          # mov eax, cr0
emit(bytes([0x25, 0xF3, 0xFF, 0xFF, 0xFF]))  # and eax, 0xFFFFFFF3
emit(bytes([0x0F, 0x22, 0xC0]))          # mov cr0, eax

debug32(0x48)                            # 'H'

# ---- 远跳 64-bit (push + retf) ----
# 32-bit 兼容模式下 retf 弹 8 字节：EIP(32) + CS(32-slot, 低 16 有效)
# 栈布局：[ESP+0]=start64(EIP) [ESP+4]=0x18(CS)
# 注意：不能 push 多余的 0（那是 64-bit retf 才需要的高 32 位）

emit(bytes([0x68]) + struct.pack('<I', 0x18))      # push 0x18 (64-bit code selector)
emit(bytes([0x68]) + struct.pack('<I', start64))    # push start64 (EIP 32-bit)
debug32(0x69)                                       # 'i' = about to retf
emit(bytes([0xCB]))                                 # retf → 64-bit!

# 兜底（正常不会到这里）
emit(bytes([0xF4]))                                 # hlt
emit(bytes([0xEB, 0xFD]))                           # jmp .


# ---- GDT (代码之后, 8 字节对齐) ----
while len(s2) % 8 != 0:
    emit(bytes([0x90]))

gdt_off = len(s2)
emit(struct.pack('<Q', 0))                       # NULL
emit(struct.pack('<Q', 0x00CF9A000000FFFF))      # 0x08: 32-bit code
emit(struct.pack('<Q', 0x00CF92000000FFFF))      # 0x10: 32-bit data
emit(struct.pack('<Q', 0x00AF9A0000000000))      # 0x18: 64-bit code (L=1, D=0, G=1)
emit(struct.pack('<Q', 0x0000920000000000))      # 0x20: 64-bit data (DPL=0)

gdt_ptr_off = len(s2)
emit(struct.pack('<H', 39))
emit(struct.pack('<I', S2_BASE + gdt_off))

struct.pack_into('<H', s2, gdt_addr_patch, S2_BASE + gdt_ptr_off)

# ============================================================
# P2-T1 E820 硬编码数据 (QEMU -m 128M 已知布局)
# 附在 stage2 二进制末尾; 代码端 SI 指向此处
# 7 条 entries × 24 字节 = 168 字节; 与代码端 rep movsb 长度一致
# ============================================================
E820_QEMU_M128 = [
    (0x0000000000000000, 0x000000000009FC00, 1),  # Usable low 640K
    (0x000000000009FC00, 0x0000000000000400, 2),  # Reserved (EBDA)
    (0x00000000000F0000, 0x0000000000010000, 2),  # BIOS ROM
    (0x0000000000100000, 0x00000000006EE000, 1),  # Usable ~110MB
    (0x00000000007FE0000, 0x0000000000020000, 2),  # Reserved
    (0x00000000FFFC0000, 0x0000000000040000, 2),  # High BIOS ROM
    (0x000000FD00000000, 0x0000000300000000, 2),  # MMIO hole
]
data_off = len(s2)
for base, size, t in E820_QEMU_M128:
    emit(struct.pack('<QQII', base, size, t, 0))  # 末 4 字节 ACPI ext attrs (0)
struct.pack_into('<H', s2, data_si_patch + 1, data_off & 0xFFFF)
assert len(s2) - data_off == 7 * 24, f'E820 data size mismatch: {len(s2) - data_off} != 168'

n_s2 = (len(s2) + 511) // 512
kernel_lba = 1 + n_s2
struct.pack_into('<I', s2, lba_patch, kernel_lba)
# P4-T5: 连续加载 kernel + initramfs（total_sectors 扇区），而非仅 ksectors
struct.pack_into('<H', s2, total_patch, total_sectors)
assert n_s2 * 512 >= len(s2)
print(f'Stage 2: {len(s2)} bytes ({n_s2} sectors), kernel LBA={kernel_lba}, '
      f'load {total_sectors} sectors (kernel+initrd)')

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
initrd_padded = initrd + bytearray(ird_sectors * 512 - len(initrd))

img = boot + s2_padded + kernel_padded + initrd_padded
total_used = len(img) // 512
assert total_used <= IMG_SECTORS, f'image overflow: {total_used} > {IMG_SECTORS}'
img += bytearray(IMG_SECTORS * 512 - len(img))

with open(IMG, 'wb') as f:
    f.write(img)
print(f'{IMG}: {len(img)} bytes, used {total_used} sectors '
      f'(stage1=1, stage2={n_s2}@lba1, kernel={ksectors}@lba{kernel_lba}, '
      f'initrd={ird_sectors}@lba{kernel_lba + ksectors})')
