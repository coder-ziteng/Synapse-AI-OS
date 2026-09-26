// Minimal boot sector test — just writes to debug port and hangs
// Assemble with: nasm or llvm-mc
.code16
.org 0

// BIOS loads us at 0x7C00
cli
// Write 'T' to debug port 0x501
mov dx, 0x501
mov al, 0x54   // 'T' for "Test"
out dx, al

// Write to isa-debug-exit (port 0xf4, value 0x33 → exit code = (0x33<<1)|1 = 0x67 = 103)
mov dx, 0xf4
mov al, 0x33
out dx, al

1: hlt
jmp 1b

// Pad to 510 bytes and add boot signature
.org 510
.short 0xAA55
