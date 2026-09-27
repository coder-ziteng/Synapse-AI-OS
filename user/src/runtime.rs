//! 极简用户态运行时符号（P4-T1）。
//!
//! `compiler_builtins` 在自定义 `os=none` target 上是 "thin wrapper"，
//! **不**真提供 `memset`/`memcpy` 等 C 运行时符号——与内核 P1-T2-bis 踩过的
//! 坑同源（见 `kernel/src/main.rs` 的同名实现）。`core::fmt`、数组初始化等
//! 代码路径会引用它们，链接期必须由我们补齐。
//!
//! 本模块只在 `target_os = "none"`（即 x86_64-synapse-user target）下编译；
//! 宿主测试构建走 libc，不会（也不允许）与宿主符号冲突。
//!
//! 实现选择：普通字节循环而非 `rep movsb` 内联汇编——用户态首期无性能诉求，
//! LLVM 对小块拷贝有循环展开/向量化优化，且避免 asm 的 flags 约定负担。
//! P4-T12 收尾时若 profiling 显示热点，再换 `rep` 版本。

use core::ffi::{c_char, c_int, c_void};

/// C 运行时 `memset`：`s` 起 `n` 字节填充为 `c` 低 8 位，返回 `s`。
#[no_mangle]
pub unsafe extern "C" fn memset(s: *mut c_void, c: c_int, n: usize) -> *mut c_void {
    let p = s as *mut u8;
    let mut i = 0;
    while i < n {
        *p.add(i) = c as u8;
        i += 1;
    }
    s
}

/// C 运行时 `memcpy`：`s` → `d` 拷贝 `n` 字节（不允许重叠），返回 `d`。
#[no_mangle]
pub unsafe extern "C" fn memcpy(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void {
    let dst = d as *mut u8;
    let src = s as *const u8;
    let mut i = 0;
    while i < n {
        *dst.add(i) = *src.add(i);
        i += 1;
    }
    d
}

/// C 运行时 `memmove`：`s` → `d` 拷贝 `n` 字节（允许重叠），返回 `d`。
///
/// 重叠方向判定与内核实现同纪律：`d < s` 或 `d` 完全在 `s` 之后 → 正向；
/// 否则反向拷贝防止先写后读。
#[no_mangle]
pub unsafe extern "C" fn memmove(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void {
    let dst = d as *mut u8;
    let src = s as *const u8;
    if (d as usize) < (s as usize) || (d as usize) >= (s as usize).wrapping_add(n) {
        memcpy(d, s, n)
    } else {
        let mut i = n;
        while i > 0 {
            i -= 1;
            *dst.add(i) = *src.add(i);
        }
        d
    }
}

/// C 运行时 `memcmp`：相等返回 0，不等返回首个差异字节差值（C 语义，
/// 勿改成 `sete+neg`——会让 `PartialEq` 对相等内容判不等，内核 P1-T8 教训）。
#[no_mangle]
pub unsafe extern "C" fn memcmp(a: *const c_void, b: *const c_void, n: usize) -> c_int {
    let x = a as *const u8;
    let y = b as *const u8;
    let mut i = 0;
    while i < n {
        let av = *x.add(i) as c_int;
        let bv = *y.add(i) as c_int;
        if av != bv {
            return av - bv;
        }
        i += 1;
    }
    0
}

/// C 运行时 `strlen`：返回 NUL 结尾字符串长度（不含 NUL）。
#[no_mangle]
pub unsafe extern "C" fn strlen(s: *const c_char) -> usize {
    let mut len = 0;
    while *s.add(len) != 0 {
        len += 1;
    }
    len
}
