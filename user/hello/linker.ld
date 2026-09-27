/* Synapse 用户态链接脚本（P4-T1）
 *
 * 约定（Doc 02 §3.1 UPDATE(P4-T2) / task.json P4 decision_log 2026-09-27）：
 * 1. 基址 0x4000_0000（1GB，PDPT[0] entry 1 粒度），无 PIE
 *    （target json relocation-model=static）。
 *    ⚠️ 原 PROPOSED 0x400000 已废弃：内核以 2MB 大页恒等映射 0-4GB
 *    （boot.S），镜像占 PA [0x200000, 0x4cb000)——VA 0x400000 与内核自身
 *    物理页同 VA 冲突（同一 VA 不能既是内核代码又是用户 ELF）。1GB 处
 *    PA 1-2GB 无物理内存（RAM ~128MB），每地址空间给 PDPT[0] entry 1 挂
 *    独立清零 PD，与共享的内核 0-1GB 恒等映射（entry 0）零重叠。
 * 2. NULL guard：0x0..0x1000 永不映射（内核 VMA 强制，P4-T3）；
 *    本脚本基址天然避开。
 * 3. 两个 PT_LOAD 段：text RX(5) + data RW(6)，W^X 分段
 *    （P4-T5 内核 ELF 加载器按 p_flags 设置页权限）。
 * 4. ENTRY(_start) + KEEP(.text._start)：--gc-sections 下保住入口。
 *
 * 路径注意：lld 的 `--script=linker.ld` 相对**包根**（user/hello/）解析，
 * 故脚本与本 Cargo.toml 同目录（实测坑，见 task.json P4-T1 actual_approach）。
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
