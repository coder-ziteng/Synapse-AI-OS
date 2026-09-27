//! synapse-user 宿主单元测试。
//!
//! 覆盖：CapRef 边界 / 所有 19 个 frame 编码器与 `abi::decode` 严格对称 /
//! `decode_abi_query_result` 正确解码 / 错误码负值映射。

use synapse_abi::{
    decode, Syscall, SyscallFrame, SyscallId, ABI_MAJOR, ABI_MINOR,
};
use synapse_user::{
    abi_query_value, build_args_abi_query, build_args_cap_delegate, build_args_cap_invoke,
    build_args_cap_revoke, build_args_exit, build_args_gettime, build_args_ipc_recv,
    build_args_ipc_reply, build_args_ipc_send, build_args_ipc_try_send, build_args_mem_mmap,
    build_args_mem_munmap, build_args_notify_signal, build_args_notify_wait,
    build_args_proc_exit, build_args_proc_freeze, build_args_proc_reap, build_args_proc_spawn,
    build_args_proc_thaw, build_args_proc_yield, decode_abi_query_result, AbiVersion, CapRef,
    SynapseError,
};

// ============================================================================
// CapRef 边界
// ============================================================================

#[test]
fn capref_accepts_zero_through_255() {
    for i in 0..=255u16 {
        assert_eq!(CapRef::new(i).unwrap().index(), i as u8);
        assert_eq!(CapRef::new(i).unwrap().as_u64(), i as u64);
    }
}

#[test]
fn capref_rejects_index_above_255() {
    assert!(CapRef::new(256).is_err());
    assert!(CapRef::new(u16::MAX).is_err());
}

// ============================================================================
// frame() 基础约定
// ============================================================================

#[test]
fn frame_uses_syscall_id_num() {
    let f = SyscallFrame { num: SyscallId::AbiQuery.num(), args: build_args_abi_query() };
    assert_eq!(f.num, 18);
    assert_eq!(f.args, [0u64; 6]);
}

// ============================================================================
// 19 个编码器 ↔ abi::decode 严格对称（核心测试）
// ============================================================================

#[test]
fn abi_query_roundtrip() {
    let args = build_args_abi_query();
    let f = SyscallFrame { num: SyscallId::AbiQuery.num(), args };
    assert_eq!(decode(&f), Some(Syscall::AbiQuery));
}

#[test]
fn ipc_send_roundtrip() {
    let ep = CapRef::new(7).unwrap();
    let args = build_args_ipc_send(ep, 0xDEAD_BEEF, 128, 0xCAFE_F00D, 3);
    let f = SyscallFrame { num: SyscallId::IpcSend.num(), args };
    assert_eq!(
        decode(&f),
        Some(Syscall::IpcSend {
            ep: 7, msg: 0xDEAD_BEEF, len: 128, caps: 0xCAFE_F00D, n_caps: 3,
        })
    );
}

#[test]
fn ipc_recv_roundtrip() {
    let args = build_args_ipc_recv(CapRef::new(2).unwrap(), 0x1000, 0x2000);
    let f = SyscallFrame { num: SyscallId::IpcRecv.num(), args };
    assert_eq!(decode(&f), Some(Syscall::IpcRecv { ep: 2, buf: 0x1000, cap_out: 0x2000 }));
}

#[test]
fn ipc_reply_roundtrip() {
    let args = build_args_ipc_reply(CapRef::new(3).unwrap(), 0x3000, 64);
    let f = SyscallFrame { num: SyscallId::IpcReply.num(), args };
    assert_eq!(decode(&f), Some(Syscall::IpcReply { ep: 3, msg: 0x3000, len: 64 }));
}

#[test]
fn ipc_try_send_roundtrip() {
    let args = build_args_ipc_try_send(CapRef::new(4).unwrap(), 0x4000, 32);
    let f = SyscallFrame { num: SyscallId::IpcTrySend.num(), args };
    assert_eq!(decode(&f), Some(Syscall::IpcTrySend { ep: 4, msg: 0x4000, len: 32 }));
}

#[test]
fn notify_signal_roundtrip() {
    let args = build_args_notify_signal(CapRef::new(5).unwrap(), 0xAA);
    let f = SyscallFrame { num: SyscallId::NotificationSignal.num(), args };
    assert_eq!(decode(&f), Some(Syscall::NotificationSignal { notif: 5, bits: 0xAA }));
}

#[test]
fn notify_wait_roundtrip() {
    let args = build_args_notify_wait(CapRef::new(6).unwrap(), 0x55);
    let f = SyscallFrame { num: SyscallId::NotificationWait.num(), args };
    assert_eq!(decode(&f), Some(Syscall::NotificationWait { notif: 6, mask: 0x55 }));
}

#[test]
fn cap_invoke_roundtrip() {
    let args = build_args_cap_invoke(CapRef::new(10).unwrap(), 7, 0x8000);
    let f = SyscallFrame { num: SyscallId::CapInvoke.num(), args };
    assert_eq!(decode(&f), Some(Syscall::CapInvoke { cap: 10, op: 7, args: 0x8000 }));
}

#[test]
fn cap_delegate_roundtrip() {
    let args = build_args_cap_delegate(CapRef::new(11).unwrap(), 0b011, 0x9000);
    let f = SyscallFrame { num: SyscallId::CapDelegate.num(), args };
    assert_eq!(decode(&f), Some(Syscall::CapDelegate { parent: 11, rights: 0b011, child_out: 0x9000 }));
}

#[test]
fn cap_revoke_roundtrip() {
    let args = build_args_cap_revoke(CapRef::new(12).unwrap());
    let f = SyscallFrame { num: SyscallId::CapRevoke.num(), args };
    assert_eq!(decode(&f), Some(Syscall::CapRevoke { cap: 12 }));
}

#[test]
fn proc_spawn_roundtrip() {
    let args = build_args_proc_spawn(
        CapRef::new(20).unwrap(), 0xA000, 0xB000, 4, CapRef::new(21).unwrap(),
    );
    let f = SyscallFrame { num: SyscallId::ProcessSpawn.num(), args };
    assert_eq!(
        decode(&f),
        Some(Syscall::ProcessSpawn {
            elf: 20, args: 0xA000, caps: 0xB000, n_caps: 4, death_ep: 21,
        })
    );
}

#[test]
fn proc_exit_roundtrip_negative_code() {
    // -1i32 → u64 全 1 → 内核 `a[0] as i32` 回到 -1
    let args = build_args_proc_exit(-1);
    assert_eq!(args[0], 0xFFFF_FFFF_FFFF_FFFF);
    let f = SyscallFrame { num: SyscallId::ProcessExit.num(), args };
    assert_eq!(decode(&f), Some(Syscall::ProcessExit { code: -1 }));
}

#[test]
fn proc_exit_roundtrip_positive_code() {
    let args = build_args_proc_exit(42);
    assert_eq!(args[0], 42);
    let f = SyscallFrame { num: SyscallId::ProcessExit.num(), args };
    assert_eq!(decode(&f), Some(Syscall::ProcessExit { code: 42 }));
}

#[test]
fn proc_reap_roundtrip() {
    let args = build_args_proc_reap(0x1234_5678);
    let f = SyscallFrame { num: SyscallId::ProcessReap.num(), args };
    assert_eq!(decode(&f), Some(Syscall::ProcessReap { pid: 0x1234_5678 }));
}

#[test]
fn proc_freeze_roundtrip() {
    let args = build_args_proc_freeze(7);
    let f = SyscallFrame { num: SyscallId::ProcessFreeze.num(), args };
    assert_eq!(decode(&f), Some(Syscall::ProcessFreeze { pid: 7 }));
}

#[test]
fn proc_thaw_roundtrip() {
    let args = build_args_proc_thaw(8);
    let f = SyscallFrame { num: SyscallId::ProcessThaw.num(), args };
    assert_eq!(decode(&f), Some(Syscall::ProcessThaw { pid: 8 }));
}

#[test]
fn yield_roundtrip() {
    let args = build_args_proc_yield();
    let f = SyscallFrame { num: SyscallId::Yield.num(), args };
    assert_eq!(decode(&f), Some(Syscall::Yield));
}

#[test]
fn gettime_roundtrip() {
    let args = build_args_gettime(0, 0xC000);
    let f = SyscallFrame { num: SyscallId::GetTime.num(), args };
    assert_eq!(decode(&f), Some(Syscall::GetTime { clock_id: 0, ts_out: 0xC000 }));
}

#[test]
fn mmap_roundtrip() {
    let args = build_args_mem_mmap(0, 0x1000, 0b111, 0);
    let f = SyscallFrame { num: SyscallId::Mmap.num(), args };
    assert_eq!(decode(&f), Some(Syscall::Mmap { addr: 0, len: 0x1000, prot: 0b111, flags: 0 }));
}

#[test]
fn munmap_roundtrip() {
    let args = build_args_mem_munmap(0xD000, 0x1000);
    let f = SyscallFrame { num: SyscallId::Munmap.num(), args };
    assert_eq!(decode(&f), Some(Syscall::Munmap { addr: 0xD000, len: 0x1000 }));
}

#[test]
fn exit_alias_matches_proc_exit() {
    let code: i32 = 7;
    assert_eq!(build_args_exit(code), build_args_proc_exit(code));
}

// ============================================================================
// abi_query 解码
// ============================================================================

#[test]
fn abi_query_value_matches_constants() {
    assert_eq!(abi_query_value(), ((ABI_MAJOR as u64) << 16) | (ABI_MINOR as u64));
    assert_eq!(ABI_MAJOR, 0);
    // P4-T6：1→2（错误码/Timespec/prot 位入 crate）；P4-T7：2→3（-11..-15）。
    assert_eq!(ABI_MINOR, 3);
}

#[test]
fn decode_abi_query_result_handles_kernel_value() {
    // 内核返回的 (major << 16) | minor → 解码为 AbiVersion（从常量推导，不硬编码）
    assert_eq!(
        decode_abi_query_result(abi_query_value() as i64),
        Ok(AbiVersion { major: ABI_MAJOR, minor: ABI_MINOR })
    );
}

#[test]
fn decode_abi_query_result_handles_negative_error() {
    // -1 = InvalidCap 等错误码 → 包装为 SynapseError
    assert_eq!(decode_abi_query_result(-1), Err(SynapseError(-1)));
    assert_eq!(decode_abi_query_result(-127), Err(SynapseError(-127)));
}

// ============================================================================
// 严格 decode：未知号 / 窄参数越界（abi crate 行为，不重复测）
// ============================================================================

#[test]
fn unknown_syscall_num_is_none() {
    let f = SyscallFrame { num: 99, args: [0; 6] };
    assert_eq!(decode(&f), None); // 预留空洞
}

#[test]
fn narrow_cap_param_overflow_is_none() {
    // CapRef u8：传 256 (= 0x100) → u8 越界 → None = IllegalSyscall
    let f = SyscallFrame { num: SyscallId::CapRevoke.num(), args: [256, 0, 0, 0, 0, 0] };
    assert_eq!(decode(&f), None);
}

// ============================================================================
// 号段完整性（Doc 02 §4.2 分配表快照）
// ============================================================================

#[test]
fn syscall_id_numbers_match_doc_02_table() {
    // Doc 02 §4.2 + §4.5 号表快照：防止号段被无意改写
    assert_eq!(SyscallId::IpcSend.num(), 0);
    assert_eq!(SyscallId::IpcRecv.num(), 1);
    assert_eq!(SyscallId::IpcReply.num(), 2);
    assert_eq!(SyscallId::IpcTrySend.num(), 3);
    assert_eq!(SyscallId::NotificationSignal.num(), 4);
    assert_eq!(SyscallId::NotificationWait.num(), 5);
    assert_eq!(SyscallId::CapInvoke.num(), 10);
    assert_eq!(SyscallId::CapDelegate.num(), 11);
    assert_eq!(SyscallId::CapRevoke.num(), 12);
    assert_eq!(SyscallId::AbiQuery.num(), 18);
    assert_eq!(SyscallId::ProcessSpawn.num(), 20);
    assert_eq!(SyscallId::ProcessExit.num(), 21);
    assert_eq!(SyscallId::ProcessReap.num(), 22);
    assert_eq!(SyscallId::ProcessFreeze.num(), 23);
    assert_eq!(SyscallId::ProcessThaw.num(), 24);
    assert_eq!(SyscallId::Yield.num(), 25);
    assert_eq!(SyscallId::GetTime.num(), 30);
    assert_eq!(SyscallId::Mmap.num(), 40);
    assert_eq!(SyscallId::Munmap.num(), 41);
    assert_eq!(19, [
        SyscallId::IpcSend, SyscallId::IpcRecv, SyscallId::IpcReply, SyscallId::IpcTrySend,
        SyscallId::NotificationSignal, SyscallId::NotificationWait,
        SyscallId::CapInvoke, SyscallId::CapDelegate, SyscallId::CapRevoke,
        SyscallId::AbiQuery,
        SyscallId::ProcessSpawn, SyscallId::ProcessExit, SyscallId::ProcessReap,
        SyscallId::ProcessFreeze, SyscallId::ProcessThaw, SyscallId::Yield,
        SyscallId::GetTime, SyscallId::Mmap, SyscallId::Munmap,
    ].len());
}