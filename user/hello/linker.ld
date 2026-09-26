/* Synapse 用户态链接脚本（P4-T1）
 *
 * 约定（Doc 02 §3.1 PROPOSED / task.json P4 决策）：
 * 1. 基址 0x400000，无 PIE（target json relocation-model=static）。
 * 2. NULL guard：0x0..0x1000 永不映射（内核 VMA 强制，P4-T3）；
 *    本脚本基址天然避开。
 * 3. 两个 PT_LOAD 段：text RX(5) + data RW(6)，W^X 分段
 *    （P4-T5 内核 ELF 加载器按 p_flags 设置页权限）。
 * 4. ENTRY(_start) + KEEP(.text._start)：--gc-sections 下保住入口。
 *
 * 路径注意：target json 的 `--script=user/linker.ld` 相对 cargo cwd 解析，
 * xtask user 子命令固定从仓库根运行 cargo（--manifest-path 指向 user/hello）。
 */

ENTRY(_start)

PHDRS
{
    text PT_LOAD FLAGS(5);  /* R+X */
    data PT_LOAD FLAGS(6);  /* R+W */
}

SECTIONS
{
    . = 0x400000;

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
