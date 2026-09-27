// Minimal 32-bit multiboot test kernel
.section .multiboot, "a"
.align 4
.long 0x1BADB002
.long 0x00000003
.long -(0x1BADB002 + 0x00000003)

.section .text
.global _start
_start:
    cli
    // Write 'X' to debug-exit port
    mov dx, 0x501
    mov al, 0x58  // 'X'
    out dx, al
    // Write to isa-debug-exit
    mov dx, 0xf4
    mov al, 0x33
    out dx, al
1:  hlt
    jmp 1b
