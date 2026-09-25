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
// - 所有 `[reg+disp]` 或 `[OFFSET symbol+disp]` 的绝对寻址无此问题。
// - 64-bit `jmp m16:64 [rip+disp]` 用 `.byte 0x48, 0xFF, 0x2D, ...` 手编。
//
// 链接契约：
// - `_start`（32-bit）：QEMU PVH 入口，e_entry 指向此。
// - `_start64`（64-bit Rust）：见 main.rs。
// - `.long SYMBOL` / `.quad SYMBOL` 产生 R_X86_64_32 / R_X86_64_64 重定位（需 -C relocation-model=static）。

.section .text.first, "ax"
.global _start
.type _start, @function

_start:
    // Debug: 确认 trampoline 入口
    mov dx, 0x501
    mov al, 0x41
    out dx, al

    // Debug: after first debug write
    mov al, 0x42               // 'B' for "Before cli"
    out dx, al

    // —— Step 1: 关闭中断 ——
    cli

    // Debug: after cli
    mov al, 0x43               // 'C' for "After cli"
    out dx, al

    // —— Step 2: 加载 GDT（32-bit GDTR: 16-bit limit + 32-bit base）——
    //   编码：0F 01 15 [disp32]   （在 32-bit 模式下 r/m=101 mod=00 = [disp32]）
    .byte 0x0F, 0x01, 0x15
    .long trampoline_gdt_ptr

    // Debug: after lgdt
    mov al, 0x44               // 'D' for "After lgdt"
    out dx, al

    // —— Step 3: 平坦段寄存器 ——
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov fs, ax
    mov gs, ax
    mov ss, ax

    // Debug: after segment setup
    mov al, 0x45               // 'E' for "After segments"
    out dx, al

    // —— Step 4: 重设栈 ——
    mov esp, OFFSET trampoline_stack_top

    // Debug: after stack setup
    mov al, 0x46               // 'F' for "After stack"
    out dx, al

    // Debug probe: write to page_tables directly, no loop
    // If this crashes, the address itself is bad (not a loop issue).
    mov edi, OFFSET page_tables
    .byte 0x31, 0xC0  // xor eax, eax
    .byte 0x89, 0x07  // mov [edi], eax   (first 4 bytes of page_tables)
    // If we reach here, page_tables IS writable
    mov dx, 0x501
    mov al, 0x50           // 'P' for "Probe write SUCCEEDED"
    out dx, al

    // —— Step 5: 清零 4 级页表（16 KB）——
    mov edi, OFFSET page_tables
    .byte 0xB9, 0x0A, 0x00, 0x00, 0x00  // mov ecx, 10
    .byte 0x31, 0xC0  // xor eax, eax

    // Debug: about to write FIRST iteration
    mov dx, 0x501
    mov al, 0x49           // 'I' for "loop begin"
    out dx, al

.Lclear_loop:
    // Debug: just before first write
    .byte 0x50              // push eax
    mov dx, 0x501
    mov al, 0x48           // 'H' for "HERE: about to write [edi]"
    out dx, al
    .byte 0x58              // pop eax

    .byte 0x89, 0x07            // mov [edi], eax
    // Debug: after FIRST write
    .byte 0x50                  // push eax (hand-encoded)
    mov dx, 0x501
    mov al, 0x4A               // 'J' for "after first mov [edi],eax"
    out dx, al
    .byte 0x58                  // pop eax (hand-encoded)

    .byte 0x83, 0xC7, 0x04      // add edi, 4
    .byte 0x49                  // dec ecx
    jnz .Lclear_loop            // 让汇编器计算偏移

    // Debug: 循环完成
    mov al, 0x4F
    out dx, al

    // —— Step 6: PDPT[0] = PD0（vaddr 0..2G 走 PD0）——
    mov eax, OFFSET page_tables
    add eax, 0x1000
    or  eax, 0x3                // present + writable
    //   89 04 25 [disp32] = mov [disp32], eax  （SIB 编码，32/64-bit 均有效）
    .byte 0x89, 0x04, 0x25
    .long page_tables + 8

    // —— Step 7: PDPT[1] = PD1（vaddr 2G..4G 走 PD1）——
    mov eax, OFFSET page_tables
    add eax, 0x2000
    or  eax, 0x3
    .byte 0x89, 0x04, 0x25
    .long page_tables + 16

    // —— Step 8: 填 PD0：512 项 × 2MB 大页覆盖物理 0..2G ——
    mov edi, OFFSET page_tables
    add edi, 0x1000              // edi = &PD0
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
    cmp ecx, 512
    jb  .Lpd1_loop

    // —— Step 10: PML4[0] = PDPT ——
    mov eax, OFFSET page_tables
    or  eax, 0x3
    .byte 0x89, 0x04, 0x25
    .long page_tables

    // —— Step 11: CR4.PAE = 1（开 4 级页表）——
    //   mov eax, cr4 = 0F 20 E0
    .byte 0x0F, 0x20, 0xE0
    or  eax, 0x20
    //   mov cr4, eax = 0F 22 E0
    .byte 0x0F, 0x22, 0xE0

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
    //   mov eax, cr0 = 0F 20 C0
    .byte 0x0F, 0x20, 0xC0
    or  eax, 0x80000000
    //   mov cr0, eax = 0F 22 C0
    .byte 0x0F, 0x22, 0xC0

    // —— Debug: announce long-mode entry. 写 port 0x501 → QEMU exit code ((al<<1)|1)
    mov dx, 0x501
    mov al, 0x4C           // 'L' for "Long mode entered"
    out dx, al

    // —— Step 15: 上一句 `mov cr0, eax` 后 CPU 进入 64-bit 模式。
    //    64-bit 模式下 `ljmp ptr16:32`（0xEA）无效，必须用 `jmp m16:64 [rip+disp]`。
    //   编码：48 FF 2D [disp32]
    //   [rip+disp] 处为 16-bit selector + u64 offset（共 10 字节）。
    .byte 0x48, 0xFF, 0x2D
    .long  0                          // disp32 = 0；目标就在下一行
.Lm64_jmp_descriptor:
    .short 0x18                       // 64-bit ring-0 code segment (L=1)
    .quad  _start64                   // 64-bit RIP（R_X86_64_64）

    // —— Debug: should never reach here if far jump succeeded ——
    mov dx, 0x501
    mov al, 0x58           // 'X' for "eXception before far jump"

.size _start, . - _start

// ======================================================================
// 数据 / BSS
// ======================================================================

.align 16
.Lgdt:
    .quad 0                           // 0x00: NULL
    .quad 0x00CF9A000000FFFF         // 0x08: 32-bit ring-0 code (base=0, limit=4G)
    .quad 0x00CF92000000FFFF         // 0x10: 32-bit ring-0 data
    .quad 0x00209A0000000000         // 0x18: 64-bit ring-0 code (L=1, D=0)
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
