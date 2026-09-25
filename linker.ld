/* Synapse kernel linker script (synapse-kernel P1-T2)
 *
 * 必须满足：
 * 1. 单 PT_LOAD 段：text + rodata + data + bss 全部落在同一段，paddr=vaddr。
 * 2. .bss (含 page_tables, trampoline_stack) 由 boot.S 的 OFFSET 访问，必须落在同一段内。
 * 3. compiler_builtins 的 memset/memcpy/memcmp 由 core 调用，必须保留。
 */

PHDRS
{
    text PT_LOAD FLAGS(5);  /* R+X */
}

SECTIONS
{
    . = 0x200000;

    .text : {
        *(.text.first)
        *(.text .text.*)
        KEEP(*(.note.Xen))
        *(.rodata .rodata.*)
        *(.eh_frame)
        *(.gcc_except_table)
    } :text

    . = ALIGN(4K);
    .data : {
        *(.data .data.*)
        . = ALIGN(16);
        *(.init_array)
    } :text

    . = ALIGN(4K);
    .bss : {
        *(.bss .bss.*)
        *(.bss.boot)
        *(COMMON)
        . = ALIGN(4K);
    } :text

    /* compiler_builtins 提供 memset/memcpy/memcmp 等给 core 调用 */
    .text.builtins : {
        KEEP(*(.text .text.*))
        KEEP(*(.text.*))
    } :text

    /DISCARD/ : {
        *(.comment)
        *(.note.GNU-stack)
        *(.note.gnu.property)
    }
}

PROVIDE(_kernel_end = .);
