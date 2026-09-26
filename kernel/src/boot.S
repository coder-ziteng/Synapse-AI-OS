// Synapse kernel 启动 trampoline（32-bit 保护模式 → 64-bit 长模式切换）
//
// 入口：QEMU PVH 加载后跳 `_start`（CPU: 32-bit 保护模式，CR0.PE=1，分页未启用）
// 出口：长模式启用 + 远跳到 64-bit Rust 入口 `_start64`
//
// 编码约束说明（关键）：
// - LLVM MC 不支持 `.code32` 切换，所有 32-bit 指令须手编字节或避开冲突编码。
// - 32-bit CR 控制寄存器指令（mov eax, cr4 等）以及 RDMSR/WRMSR 必须用 .byte 手编。
// - 间接寻址 `[edi]` 在 MC 默认 64-bit 模式下被加 0x67 (地址大小前缀)，
//   此前缀在 32-bit 模式下反而切换到 16-bit 地址，会破坏寻址！必须手编 `.byte 89 07`。
// - 绝对 `[disp32]` 使用 SIB 形式（`89 04 25 [disp32]`、`0F 01 14 25 [disp32]`），
//   不使用 ModRM=0x15 形式（`0F 01 15 [disp32]`），后者在某些情况下 QEMU TCG
//   行为异常。
// - 64-bit `jmp m16:64 [rip+disp]` 用 `.byte 0x48, 0xFF, 0x2D, ...` 手编。
//
// 链接契约：
// - `_start`（32-bit）：QEMU PVH 入口，e_entry 指向此。
// - `_start64`（64-bit Rust）：见 main.rs。
// - `.long SYMBOL` / `.quad SYMBOL` 产生 R_X86_64_32 / R_X86_64_64 重定位（需 -C relocation-model=static）。

// ======================================================================
// Multiboot header — 让 QEMU / GRUB 识别并直接加载内核
// ======================================================================
// Multiboot 1 spec: header 必须在二进制前 8KB 内，32 位对齐。
// magic + flags + checksum 必须和为 0。
// flags=3: 请求 4K 对齐的模块 + 提供 memory map。

.section .text.first, "ax"

// ======================================================================
// Multiboot2 header — 让 QEMU / GRUB 识别并直接加载 64-bit 内核
// ======================================================================
// Multiboot2 spec: magic=0xE85250D6, architecture=0 (i386)。
// header_length = 6*4 = 24 字节（magic + arch + len + checksum + end_type + end_len）
// checksum = -(magic + arch + len) mod 2^32 = 0x17ADAF0E
.align 8
.long 0xE85250D6              // magic
.long 0x00000000              // architecture: 0 = i386 (32-bit protected mode)
.long 24                      // header length (6 fields × 4 bytes)
.long 0x17ADAF0E              // checksum: -(0xE85250D6 + 0 + 24) mod 2^32
// End tag (type=0, length=8)
.align 8
.long 0                       // type: end tag
.long 8                       // length: 8 bytes

.global _start
.type _start, @function

_start:
    // —— Step 1: 关闭中断 ——
    cli

    // —— Step 2: 加载 GDT（32-bit GDTR: 16-bit limit + 32-bit base）——
    //   使用 SIB 形式编码 `[disp32]`：
    //     0F 01 14 25 [disp32]
    //     ModRM = 0x14 = mod:00 reg:010 r/m:100  → SIB follows
    //     SIB   = 0x25 = scale:00 index:100(none) base:101 → [disp32]
    .byte 0x0F, 0x01, 0x14, 0x25
    .long trampoline_gdt_ptr

    // —— Step 3: 平坦段寄存器 ——
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov fs, ax
    mov gs, ax
    mov ss, ax

    // —— Step 4: 重设栈 ——
    mov esp, OFFSET trampoline_stack_top

    // —— Step 5: 清零 4 级页表（16 KB）——
    //   4 个 4KB 页表 × 1024 个 dword = 4096 dword；但当前仅使用 PML4 + PDPT + PD0 + PD1，
    //   共 4*1024 = 4096 dword。循环 1024*4 = 4096 次，每次清 4 字节。
    //   但 ECX 寄存器一次只能装 32 位，循环计数大，用两级循环或展开。
    //   简化：先只清需要的部分（PML4 + PDPT + PD0 + PD1 = 4 * 4KB = 16KB = 4096 dword）。
    //   用 ecx 计数需要循环 4096 次，但我们只用 10 次 × 4 字节的简化版仅清零 40 字节。
    //   TODO：改为完整的 4096 次循环（当前仅清零 PML4 第一项的 10 个 dword 作占位）。
    mov edi, OFFSET page_tables
    .byte 0xB9, 0x00, 0x10, 0x00, 0x00  // mov ecx, 0x1000  (4096 次)
    .byte 0x31, 0xC0                      // xor eax, eax

.Lclear_loop:
    .byte 0x89, 0x07            // mov [edi], eax
    .byte 0x83, 0xC7, 0x04      // add edi, 4
    .byte 0x49                  // dec ecx
    jnz .Lclear_loop            // 让汇编器计算偏移

    // —— Step 6: PDPT[0] = PD0（vaddr 0..2G 走 PD0）——
    mov eax, OFFSET page_tables
    add eax, 0x2000              // eax = &PD0（PDPT[0] 指向 PD0）
    or  eax, 0x3                // present + writable
    //   89 04 25 [disp32] = mov [disp32], eax  （SIB 编码，32/64-bit 均有效）
    .byte 0x89, 0x04, 0x25
    .long page_tables + 0x1008

    // —— Step 7: PDPT[1] = PD1（vaddr 2G..4G 走 PD1）——
    mov eax, OFFSET page_tables
    add eax, 0x3000
    or  eax, 0x3
    .byte 0x89, 0x04, 0x25
    .long page_tables + 0x1010

    // —— Step 8: 填 PD0：512 项 × 2MB 大页覆盖物理 0..2G ——
    mov edi, OFFSET page_tables
    add edi, 0x2000              // edi = &PD0
    xor ecx, ecx                // entry index
.Lpd0_loop:
    mov eax, ecx
    shl eax, 21                 // entry * 2MB
    or  eax, 0x83               // present + writable + huge (2MB)
    .byte 0x89, 0x07            // mov [edi], eax  （手编：无 0x67 前缀）
    add edi, 8
    inc ecx
    cmp ecx, 512
    jb  .Lpd0_loop

    // —— Step 9: 填 PD1：512 项 × 2MB 大页覆盖物理 2G..4G ——
.Lpd1_loop:
    mov eax, ecx
    shl eax, 21
    add eax, 0x200000           // 物理基 = ecx*2MB + 2GB
    or  eax, 0x83
    .byte 0x89, 0x07            // mov [edi], eax  （手编）
    add edi, 8
    inc ecx
    cmp ecx, 1024
    jb  .Lpd1_loop

    // —— Step 10: PML4[0] = PDPT ——
    //   PML4 起始于 page_tables；PML4[0] 在偏移 +8 处（PML4[0] 位于 page_tables+0）。
    //   注意：前一行 `.byte 0x89, 0x04, 0x25` 搭配 `.long <addr>` 生成 SIB 形式
    //   `mov [<addr>], eax`。此处 addr 必须为 page_tables + 8 才能写入 PML4[0]。
    mov eax, OFFSET page_tables
    or  eax, 0x3
    .byte 0x89, 0x04, 0x25
    .long page_tables + 8

    // —— Step 11: CR4.PAE = 1（开 4 级页表）——
    .byte 0x0F, 0x20, 0xE0          // mov eax, cr4
    or  eax, 0x20                    // set PAE bit
    .byte 0x0F, 0x22, 0xE0          // mov cr4, eax

    // —— Step 12: CR3 = PML4 物理地址 ——
    mov eax, OFFSET page_tables
    //   mov cr3, eax = 0F 22 D8
    .byte 0x0F, 0x22, 0xD8

    // —— Step 13: EFER.LME = 1（开长模式）——
    mov ecx, 0xC0000080
    //   rdmsr = 0F 32
    .byte 0x0F, 0x32
    or  eax, 0x100
    //   wrmsr = 0F 30
    .byte 0x0F, 0x30

    // —— Step 14: CR0.PG = 1（开分页；与 PAE+LME 同时生效 → 切到 long mode）——
    .byte 0x0F, 0x20, 0xC0          // mov eax, cr0
    or  eax, 0x80000000
    .byte 0x0F, 0x22, 0xC0          // mov cr0, eax

    // —— Step 14b: 清 CR0.EM(bit2) + CR0.TS(bit3) ——
    //   不清 EM → Rust 的 fninit 会 #UD（EM=1 时所有 x87/SSE 指令非法）。
    .byte 0x0F, 0x20, 0xC0          // mov eax, cr0
    and eax, 0xFFFFFFF3             // clear EM(bit2) + TS(bit3)
    .byte 0x0F, 0x22, 0xC0          // mov cr0, eax

    // —— Step 15: 上一句 `mov cr0, eax` 后 CPU 进入 64-bit 模式。
    //    64-bit 模式下 `ljmp ptr16:32`（0xEA）无效，必须用 `jmp m16:64 [rip+disp]`。
    //   编码：48 FF 2D [disp32]
    //   [rip+disp] 处为 16-bit selector + u64 offset（共 10 字节）。
    .byte 0x48, 0xFF, 0x2D
    .long  0                          // disp32 = 0；目标就在下一行
.Lm64_jmp_descriptor:
    .short 0x18                       // 64-bit ring-0 code segment (L=1)
    .quad  _start64                   // 64-bit RIP（R_X86_64_64）

.size _start, . - _start

// ======================================================================
// 数据 / BSS
// ======================================================================

.align 16
.Lgdt:
    .quad 0                           // 0x00: NULL
    .quad 0x00CF9A000000FFFF         // 0x08: 32-bit ring-0 code (base=0, limit=4G)
    .quad 0x00CF92000000FFFF         // 0x10: 32-bit ring-0 data
    .quad 0x00AF9A0000000000         // 0x18: 64-bit ring-0 code (L=1, D=0, G=1)
    .quad 0x0000920000000000         // 0x20: 64-bit ring-0 data
.Lgdt_end:

.align 4
trampoline_gdt_ptr:
    .word  .Lgdt_end - .Lgdt - 1      // 16-bit limit
    .long  .Lgdt                      // 32-bit base
    .long  0                          // padding（64-bit GDTR 高位；用 32-bit GDTR 不读此字段）

// 4 级页表 + 临时栈（DATA section — 必须落在文件内以便 QEMU PVH 实际加载）
// PVH loader 可能不会 zero-fill memsz 超过 filesz 的部分，所以把 boot-time 数据
// 放到 .data（PROGBITS，文件中有数据）而不是 .bss（NOBITS，文件中无数据）。
.section .data.boot, "aw", @progbits
.align 4096
page_tables:
    .skip 4096      // PML4
    .skip 4096      // PDPT
    .skip 4096      // PD0  (覆盖 0..2G)
    .skip 4096      // PD1  (覆盖 2G..4G)
page_tables_end:

.align 16
trampoline_stack_bottom:
    .skip 4096
trampoline_stack_top:

// ======================================================================
// PVH ELF note — 告诉 QEMU 这是 PVH 可引导内核，入口为 32-bit _start
// ======================================================================
// Xen PVH ABI: ELF note name="Xen", type=18 (PHYS32_ENTRY),
//              desc = 32-bit 物理入口地址
.section .note.Xen, "a", @note
.balign 4
.long 4                       // namesz: strlen("Xen") + 1 = 4
.long 4                       // descsz: 4 bytes (32-bit address)
.long 18                      // type: XEN_ELFNOTE_PHYS32_ENTRY
.ascii "Xen\0"                // name (4 bytes, padded to 4)
.long _start                  // descriptor: entry point address
