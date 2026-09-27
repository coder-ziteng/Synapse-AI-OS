/* Synapse 用户态链接脚本（P4-T7 Phase 2 ipc-pong 专用）。
 *
 * 与 hello/linker.ld 同构（基址 1GB / 无 PIE / W^X 分段），但独立副本避免
 * 跨 bin 共享 linker.ld 的隐式耦合。lld 的 `--script=` 相对**包根**解析，
 * 故脚本与本 Cargo.toml 同目录（task.json P4-T1 actual_approach 实测坑）。
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
