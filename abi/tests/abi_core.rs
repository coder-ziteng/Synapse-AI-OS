//! synapse-abi 宿主单元测试。
//!
//! 覆盖：号表与 Doc 02 §4.2 一致 / from_num 全集与空洞 / class 路由 /
//! decode 参数拆位与越界拒绝 / abi_query 版本打包 / SyscallFrame 布局。

use synapse_abi::*;

fn frame(num: u64, args: [u64; 6]) -> SyscallFrame {
    SyscallFrame { num, args }
}

// ---------- 版本 ----------

#[test]
fn abi_query_packs_major_minor() {
    assert_eq!(abi_query_value(), ((ABI_MAJOR as u64) << 16) | (ABI_MINOR as u64));
    assert_eq!(abi_query_value() >> 16, ABI_MAJOR as u64);
    assert_eq!(abi_query_value() & 0xFFFF, ABI_MINOR as u64);
}

#[test]
fn frame_layout_is_repr_c() {
    // num + 6 args = 56 字节（汇编 glue 依赖此布局）
    assert_eq!(core::mem::size_of::<SyscallFrame>(), 56);
}

// ---------- 号表 ----------

#[test]
fn syscall_numbers_match_doc02_table() {
    // Doc 02 §4.2 分配表：IPC 0~3 / Notif 4~5 / Cap 10~12 / ABI 18 /
    // Process 20~25 / Time 30 / Memory 40~41 —— 共 18 个
    let table = [
        (SyscallId::IpcSend, 0),
        (SyscallId::IpcRecv, 1),
        (SyscallId::IpcReply, 2),
        (SyscallId::IpcTrySend, 3),
        (SyscallId::NotificationSignal, 4),
        (SyscallId::NotificationWait, 5),
        (SyscallId::CapInvoke, 10),
        (SyscallId::CapDelegate, 11),
        (SyscallId::CapRevoke, 12),
        (SyscallId::AbiQuery, 18),
        (SyscallId::ProcessSpawn, 20),
        (SyscallId::ProcessExit, 21),
        (SyscallId::ProcessReap, 22),
        (SyscallId::ProcessFreeze, 23),
        (SyscallId::ProcessThaw, 24),
        (SyscallId::Yield, 25),
        (SyscallId::GetTime, 30),
        (SyscallId::Mmap, 40),
        (SyscallId::Munmap, 41),
    ];
    // 19 = §4.2 表内 18 个（IPC 4 + Notif 2 + Cap 3 + Process 6 + Time 1 + Mem 2）
    //    + §4.5 新增 abi_query（#18）
    assert_eq!(table.len(), 19);
    for (id, num) in table {
        assert_eq!(id.num(), num, "{id:?} 号与 Doc 02 §4.2 不符");
        assert_eq!(SyscallId::from_num(num), Some(id));
    }
}

#[test]
fn unknown_and_reserved_numbers_rejected() {
    for num in [6, 7, 8, 9, 13, 17, 19, 26, 29, 31, 39, 42, 100, u64::MAX] {
        assert_eq!(SyscallId::from_num(num), None, "号 {num} 应为空洞/预留");
        assert!(decode(&frame(num, [0; 6])).is_none());
    }
}

#[test]
fn class_routing() {
    assert_eq!(SyscallId::IpcSend.class(), SyscallClass::Ipc);
    assert_eq!(SyscallId::NotificationWait.class(), SyscallClass::Notification);
    assert_eq!(SyscallId::CapDelegate.class(), SyscallClass::Capability);
    assert_eq!(SyscallId::AbiQuery.class(), SyscallClass::Abi);
    assert_eq!(SyscallId::ProcessSpawn.class(), SyscallClass::Process);
    assert_eq!(SyscallId::Yield.class(), SyscallClass::Process);
    assert_eq!(SyscallId::GetTime.class(), SyscallClass::Time);
    assert_eq!(SyscallId::Munmap.class(), SyscallClass::Memory);
}

// ---------- decode ----------

#[test]
fn decode_ipc_send() {
    let f = frame(0, [3, 0x7000_0000, 128, 0x7000_1000, 2, 0]);
    assert_eq!(
        decode(&f),
        Some(Syscall::IpcSend {
            ep: 3,
            msg: 0x7000_0000,
            len: 128,
            caps: 0x7000_1000,
            n_caps: 2
        })
    );
}

#[test]
fn decode_process_spawn_and_exit() {
    let f = frame(20, [5, 0x1000, 0x2000, 3, 7, 0]);
    assert_eq!(
        decode(&f),
        Some(Syscall::ProcessSpawn {
            elf: 5,
            args: 0x1000,
            caps: 0x2000,
            n_caps: 3,
            death_ep: 7
        })
    );
    // exit code 为 i32 补码：-1
    let f = frame(21, [0xFFFF_FFFF_FFFF_FFFF, 0, 0, 0, 0, 0]);
    assert_eq!(decode(&f), Some(Syscall::ProcessExit { code: -1 }));
}

#[test]
fn decode_zero_arg_syscalls() {
    assert_eq!(decode(&frame(18, [9; 6])), Some(Syscall::AbiQuery)); // 参数被忽略
    assert_eq!(decode(&frame(25, [9; 6])), Some(Syscall::Yield));
}

#[test]
fn decode_rejects_out_of_range_narrow_args() {
    // CapRef 参数 > 255 → None（参数越界 = IllegalSyscall，不截断）
    assert!(decode(&frame(12, [256, 0, 0, 0, 0, 0])).is_none()); // cap_revoke(256)
    assert!(decode(&frame(0, [3, 0, 0, 0, 0, 0])).is_some()); // 255 边界内
    assert!(decode(&frame(12, [255, 0, 0, 0, 0, 0])).is_some());
    // u32 参数 > u32::MAX → None
    assert!(decode(&frame(22, [1 << 32, 0, 0, 0, 0, 0])).is_none()); // reap(pid 越界)
    assert!(decode(&frame(4, [1, (1 << 32) + 1, 0, 0, 0, 0])).is_none()); // signal bits 越界
    // 指针/长度参数不校验（透传 u64，集成层负责地址校验）
    assert!(decode(&frame(1, [2, u64::MAX, u64::MAX, 0, 0, 0])).is_some());
}

// ---------- P4-T6：错误码 / Timespec / prot·flags 位 ----------
// ---------- P4-T7：扩展错误码 -11..-15 ----------

#[test]
fn error_codes_are_distinct_negative_range() {
    // Doc 02 §4.3 全集 = 15 个，值域 [-15, -1]，互不重复。
    // 前 10 个由 P4-T6 引入；-11..-15 由 P4-T7 补齐（与 cap crate CapError
    // / Doc 03 §5.1 对齐，详见 abi/src/lib.rs 错误码段注记）。
    let codes = [
        E_INVALID_CAP,
        E_INVALID_ADDR,
        E_NO_MEMORY,
        E_WOULD_BLOCK,
        E_NOT_FOUND,
        E_AGENT_ID_CONFLICT,
        E_PERMISSION,
        E_FROZEN,
        E_ZOMBIE,
        E_NOT_IMPLEMENTED,
        E_ABI_MISMATCH,
        E_OBJECT_RETIRED,
        E_QUOTA_EXCEEDED,
        E_PEER_DIED,
        E_TIMEOUT,
    ];
    assert_eq!(codes.len(), 15);
    for (i, c) in codes.iter().enumerate() {
        assert_eq!(*c, -(i as i64) - 1, "错误码应按文档顺序 -1..-15");
    }
}

#[test]
fn timespec_layout_is_repr_c_16_bytes() {
    assert_eq!(core::mem::size_of::<Timespec>(), 16);
    assert_eq!(core::mem::align_of::<Timespec>(), 8);
    // 字段序（sec 在前）由 repr(C) + 声明序保证；用 Default/Copy 语义自检
    let ts = Timespec { sec: 7, nsec: 123 };
    let copy = ts;
    assert_eq!(copy, ts);
    assert_eq!(Timespec::default(), Timespec { sec: 0, nsec: 0 });
}

#[test]
fn clock_ids_match_doc() {
    assert_eq!(CLOCK_MONOTONIC, 0);
    assert_eq!(CLOCK_WALL, 1);
}

#[test]
fn prot_and_flags_bits() {
    // 与 vma RegionFlags 低位约定对齐：R=bit0 W=bit1 X=bit2 GROWABLE=bit3
    assert_eq!(PROT_READ, 0b0001);
    assert_eq!(PROT_WRITE, 0b0010);
    assert_eq!(PROT_EXEC, 0b0100);
    assert_eq!(PROT_MASK, 0b0111);
    assert_eq!(MAP_GROWABLE, 0b1000);
    assert_eq!(MAP_MASK, MAP_GROWABLE);
    assert_eq!(PROT_MASK & MAP_MASK, 0, "prot 与 flags 位段不得重叠");
}

#[test]
fn decode_gettime_and_mmap_args() {
    let f = frame(30, [CLOCK_MONOTONIC as u64, 0x4100_0000, 0, 0, 0, 0]);
    assert_eq!(
        decode(&f),
        Some(Syscall::GetTime { clock_id: 0, ts_out: 0x4100_0000 })
    );
    let f = frame(
        40,
        [0, 0x2000, (PROT_READ | PROT_WRITE) as u64, MAP_GROWABLE as u64, 0, 0],
    );
    assert_eq!(
        decode(&f),
        Some(Syscall::Mmap { addr: 0, len: 0x2000, prot: 0b0011, flags: 0b1000 })
    );
    let f = frame(41, [0x4100_0000, 0x1000, 0, 0, 0, 0]);
    assert_eq!(
        decode(&f),
        Some(Syscall::Munmap { addr: 0x4100_0000, len: 0x1000 })
    );
}

#[test]
fn abi_minor_bumped_to_4() {
    // P4-T6：1→2（错误码/Timespec/prot 位入 crate）；P4-T7：2→3（补 -11..-15）；
    // P4-T9c：3→4（DeathMsg / DEATH_LABEL / FAULT_* 编码）。
    // minor 增量 = 向后兼容新增；旧编号全部不变。
    assert_eq!(ABI_MINOR, 4);
    assert_eq!(abi_query_value(), 0x4);
}

// ---------- death notification（P4-T9c，Doc 02 §5.3） ----------

#[test]
#[allow(unsafe_code)] // 布局断言需要按字节视角读 repr(C) 结构
fn death_msg_layout_is_repr_c_16b() {
    // 内核投递 / 用户态 recv 读取共享此布局：4×u32 = 16 字节，无填充。
    assert_eq!(core::mem::size_of::<DeathMsg>(), 16);
    let m = DeathMsg { pid: 7, exit_code: -1, fault: FAULT_SEGFAULT, rsv: 0 };
    assert_eq!(m.pid, 7);
    assert_eq!(m.fault, FAULT_SEGFAULT);
    // 字段偏移（little-endian 字节序契约）
    let bytes = unsafe {
        core::slice::from_raw_parts(&m as *const DeathMsg as *const u8, 16)
    };
    assert_eq!(&bytes[0..4], &7u32.to_le_bytes());
    assert_eq!(&bytes[4..8], &(-1i32).to_le_bytes());
    assert_eq!(&bytes[8..12], &FAULT_SEGFAULT.to_le_bytes());
}

#[test]
fn fault_codes_are_distinct_and_none_is_zero() {
    // FAULT_NONE=0 是"正常退出"哨兵，必须与全部崩溃码互异。
    let codes = [
        FAULT_NONE, FAULT_SEGFAULT, FAULT_PANIC, FAULT_ILLEGAL_SYSCALL,
        FAULT_ILLEGAL_INSTRUCTION, FAULT_GENERAL_PROTECTION,
    ];
    assert_eq!(FAULT_NONE, 0);
    for (i, a) in codes.iter().enumerate() {
        for (j, b) in codes.iter().enumerate() {
            if i != j {
                assert_ne!(a, b, "fault codes must be distinct");
            }
        }
    }
    assert_eq!(DEATH_LABEL, 0x4445_4144);
}

#[test]
fn syscall_id_roundtrip() {
    // decode(...).id() 与号一致（分发表自检；全零参数对全部 19 个号均合法）
    for num in [0u64, 1, 2, 3, 4, 5, 10, 11, 12, 18, 20, 21, 22, 23, 24, 25, 30, 40, 41] {
        let s = decode(&frame(num, [0; 6])).expect("valid syscall number");
        assert_eq!(s.id().num(), num);
        assert_eq!(SyscallId::from_num(num), Some(s.id()));
    }
}
