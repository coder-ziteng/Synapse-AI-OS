/* Synapse 用户态链接脚本（init 进程，P4-T10）。
 *
 * 与 user/hello/linker.ld 同构：基址 0x4000_0000、PHDR text RX + data RW、
 * ENTRY(_start) + KEEP(.text._start) 保入口。两份脚本并存避免单文件改动
 * 触发并行窗口冲突（root Cargo.toml user/init 已 exclude）。
 */

ENTRY(_start)

PHDRS
{
    text PT_LOAD FLAGS(5);  /* R+X */
    data PT_LOAD FLAGS(6);  /* R+W */
}

SECTIONS
{
    . = 0x40000000;

    .text : {
        KEEP(*(.text._start))
        *(.text .text.*)
        *(.rodata .rodata.*)
    } :text

    . = ALIGN(4K);
    .data : {
        *(.data .data.*)
    } :data

    .bss (NOLOAD) : {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /DISCARD/ : {
        *(.eh_frame*)
        *(.note*)
        *(.comment*)
    }
}