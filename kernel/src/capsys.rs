//! P4-T13 Capability syscall 内核接线（cap_invoke/cap_delegate/cap_revoke）
//! + Phase 4 安全回归专项。
//!
//! ## syscall 语义（Doc 02 §4.2 行 10/11/12 + Doc 01 §3.3/§3.4/§4.2）
//!
//! * **cap_invoke(cap, op, args)**：通用能力调用。本期内核尚无对象特定 op
//!   表（Endpoint/Notification 的实际操作走 IpcSend/NotificationSignal 等
//!   专用 syscall）——invoke 的价值在**校验语义收口**：O(1) 槽查 +
//!   generation/状态匹配，旧句柄（对象已 Retired）稳定返回
//!   `E_OBJECT_RETIRED(-12)`（Phase 4 退出标准第 4 条），活对象返回
//!   `E_NOT_IMPLEMENTED(-10)`（op 表随 Phase 5 服务对象落地）。
//! * **cap_delegate(parent, rights, child_out)**：attenuation-only 派生。
//!   源 cap 必须持 GRANT（cap crate 内检）；子权限 = `src ∩ mask`（不可
//!   放大）；mask 含未知位 → `E_PERMISSION(-7)`（对齐 umem prot 未知位
//!   先例）；child_out 用户指针走 [`crate::uaccess`] 统一校验后写 1 字节。
//! * **cap_revoke(cap)**：撤销 derivation tree。调用方须持 GRANT（撤销权
//!   随授权权，Doc 01 §3.4 owner 语义的 MVP 近似）。流程：
//!   `begin_revoke`（Live→Revoking，此后一切新解析即刻 -12，不等待遍历，
//!   Doc 01 §4.2）→ **跨表级联**（遍历全部 CapTable，对每个 `cap.obj ==
//!   obj` 的槽跑 cap crate `revoke_cascade` 不动点迭代）→ `retire`
//!   （Revoking→Retired，generation 历史保留，槽复用不复活旧句柄）。
//!   返回被释放的槽总数（≥1，含自身）。
//!
//! ## 锁序
//!
//! 与 kstate 契约一致：`OBJECTS → CAP_TABLES` 嵌套（resolve/delegate）；
//! 级联走 [`kstate::k_revoke_cascade_global`]（单持 CAP_TABLES）；
//! FR9 审计 push 一律**锁外**（AUDIT 叶子锁 + agent_of 需 PROCS）。
//!
//! ## 安全回归（[`security_smoke`]，真机 ring0）
//!
//! 评审 §2.6/§8 测试矩阵内核侧全覆盖：非法 cptr / 过期 generation /
//! 权限降级绕过（attenuation-only）/ 重复回收 / 表满故障注入 / 伪造
//! agent_id / E_OBJECT_RETIRED 稳定性（两次调用同码）。违例只产生负错误
//! 码或进程级 fault，内核不 panic——断言失败即 smoke panic（355 出口），
//! 与"被测违例本身不 panic"是两回事。跨页用户缓冲/长度溢出在 ring3 侧
//! （hello/user_mem_ok 路径）与 [`crate::uaccess`] 单元语义覆盖。

use log::info;
use synapse_abi::{
    SyscallFrame, E_INVALID_ADDR, E_INVALID_CAP, E_NOT_IMPLEMENTED, E_OBJECT_RETIRED, E_PERMISSION,
};
use synapse_cap::{CapError, CapRef, ObjKind, ObjRef, Rights};
use synapse_proc::process::Pid;

use crate::ipc::cap_err_to_code;
use crate::kstate;
use crate::uaccess;

/// 通用 cap 解析（O(1) 槽查 + rights 包含 + 对象 live/generation 匹配）。
///
/// 成功 → `(ObjRef, ObjKind, 持有 rights)`；失败 → Doc 02 §4.3 负码。
/// 成功/失败都记 FR9 CapVerify 审计（**锁外** push，UNKNOWN_OBJ 哨兵同
/// ipc.rs resolve_endpoint 约定）。
pub fn resolve_cap(
    pid: u32,
    cptr: u8,
    need: Rights,
) -> Result<(ObjRef, ObjKind, Rights), i64> {
    if cptr == 0 {
        // NULL trap（slot 0 永久保留，Doc 01 §4）
        crate::audit::cap_verify(pid, crate::audit::UNKNOWN_OBJ, need, false);
        return Err(E_INVALID_CAP);
    }
    // 锁序: OBJECTS → CAP_TABLES（与 resolve_endpoint 同款嵌套）
    let r = kstate::with_objects(|objs| {
        kstate::with_cap_table(Pid(pid), |t| {
            let cap = match t.get(cptr) {
                Ok(c) => c,
                Err(_) => return Err(E_INVALID_CAP),
            };
            if !cap.rights.contains(need) {
                return Err(E_PERMISSION);
            }
            // generation 匹配 + 状态机：Retired/Revoking/代际不符 → 稳定 -12
            match objs.check_live(cap.obj) {
                Ok(kind) => Ok((cap.obj, kind, cap.rights)),
                Err(CapError::ObjectRetired) => Err(E_OBJECT_RETIRED),
                Err(_) => Err(E_INVALID_CAP),
            }
        })
    });
    match &r {
        Ok((obj, _, _)) => crate::audit::cap_verify(pid, *obj, need, true),
        Err(_) => crate::audit::cap_verify(pid, crate::audit::UNKNOWN_OBJ, need, false),
    }
    r
}

/// `cap_invoke(cap, op, args)`（#10）。
///
/// 校验链走完（存在/rights/live）后，本期无对象特定 op 表 → -10；
/// 旧句柄（对象 Retired）→ -12 稳定。args 缓冲本期不解引用（op 表落地
/// 时按 op 定义校验），不做 user_mem_ok——不触碰即不校验（避免误拒）。
pub fn k_cap_invoke(frame: &SyscallFrame) -> i64 {
    let pid = crate::proc_ext::current_pid();
    kstate::record_syscall_rate(Pid(pid)); // FR10：cap 操作类计数
    let cptr = frame.args[0] as u8; // decode 已严格校验 CapRef ≤ 255
    let _op = frame.args[1] as u32;
    match resolve_cap(pid, cptr, Rights::EMPTY) {
        Ok((_obj, _kind, _rights)) => E_NOT_IMPLEMENTED, // 活对象，op 表未落地
        Err(code) => code,                               // -1 / -7 / -12
    }
}

/// `cap_delegate(parent, rights_mask, child_out)`（#11）。
pub fn k_cap_delegate(frame: &SyscallFrame) -> i64 {
    let pid = crate::proc_ext::current_pid();
    kstate::record_syscall_rate(Pid(pid));
    let parent = frame.args[0] as u8;
    let mask_bits = frame.args[1] as u32;
    let child_out = frame.args[2];

    // mask 严格校验：未知/保留位 → -7（对齐 umem prot 未知位先例）
    let mask = Rights::from_bits(mask_bits);
    if mask.bits() != mask_bits {
        return E_PERMISSION;
    }

    // child_out 统一校验（1 字节可写；as=0 内核缓冲路径仅 smoke 用）
    let user_as = crate::elfload::current_as_ptr();
    if !uaccess::user_mem_ok(user_as, child_out, 1, true) {
        return E_INVALID_ADDR;
    }

    // 临界区：OBJECTS → CAP_TABLES。live 校验 + delegate（GRANT 内检 +
    // attenuation-only + 父子链登记）一气呵成，避免 TOCTOU。
    let r: Result<(CapRef, ObjKind), CapError> = kstate::with_objects(|objs| {
        kstate::with_cap_table(Pid(pid), |t| {
            let src = t.get(parent).map_err(|_| CapError::InvalidCap)?;
            let kind = objs.check_live(src.obj)?; // retired → ObjectRetired
            let child = t.delegate(parent, mask)?; // 无 GRANT → Permission
            Ok((child, kind))
        })
    });

    let (child, kind) = match r {
        Ok(v) => v,
        Err(e) => {
            crate::audit::cap_verify(pid, crate::audit::UNKNOWN_OBJ, Rights::GRANT, false);
            return cap_err_to_code(e);
        }
    };
    crate::audit::cap_verify(pid, crate::audit::UNKNOWN_OBJ, Rights::GRANT, true);

    // 写回子 cptr（all-or-nothing：校验已过，PA 直写不会中途失败）
    let ok = unsafe { uaccess::write_user_bytes(user_as, child_out, &[child], true) };
    if !ok {
        // 理论不可达（user_mem_ok 刚过）；防御性回滚子槽，不留孤儿 cap
        let _ = kstate::with_cap_table(Pid(pid), |t| t.free(child));
        return E_INVALID_ADDR;
    }

    // FR9 审计：CapLifecycle(Delegate)（锁外 push）
    crate::audit::cap_lifecycle(
        pid,
        synapse_audit::CapOp::Delegate,
        kind,
        crate::audit::agent_of_pub(pid),
    );
    0
}

/// `cap_revoke(cap)`（#12）→ 成功返回级联释放的槽总数（≥1）。
pub fn k_cap_revoke(frame: &SyscallFrame) -> i64 {
    let pid = crate::proc_ext::current_pid();
    kstate::record_syscall_rate(Pid(pid));
    let cptr = frame.args[0] as u8;

    // 撤销权 = GRANT 持有者（MVP 近似 Doc 01 §3.4 owner 语义）
    let (obj, _kind) = match resolve_cap(pid, cptr, Rights::GRANT) {
        Ok((obj, kind, _)) => (obj, kind),
        Err(code) => return code,
    };

    // Live → Revoking：此后一切新解析即刻 -12（不等遍历完成，Doc 01 §4.2）
    if let Err(e) = kstate::with_objects(|o| o.begin_revoke(obj)) {
        return cap_err_to_code(e);
    }

    // 跨表级联（单持 CAP_TABLES；cap crate 不动点迭代防递归）
    let total = kstate::k_revoke_cascade_global(obj);

    // Revoking → Retired（generation 历史保留；槽复用不复活旧句柄）
    if let Err(e) = kstate::with_objects(|o| o.retire(obj)) {
        // begin_revoke 已过，retire 理论必成；失败仅告警不回滚（Revoking
        // 已拒绝一切新解析，安全性不受影响）
        log::warn!("[capsys] retire({}:{}) failed: {:?} (left Revoking)", obj.index, obj.generation, e);
    }

    // FR9 审计：CapLifecycle(Revoke)（锁外 push）
    crate::audit::cap_lifecycle(
        pid,
        synapse_audit::CapOp::Revoke,
        ObjKind::Endpoint, // kind 已在级联中不可查（retire 后 check_live 拒绝）；记哨兵
        crate::audit::agent_of_pub(pid),
    );
    total as i64
}

// ===========================================================================
// Phase 4 安全回归专项（真机 ring0；评审 §2.6/§8 测试矩阵）
// ===========================================================================

/// 安全回归 smoke：全部断言 ring0 直跑，违例路径只返回负码，内核零 panic。
///
/// 测试 pid = 120（避开 smoke 常用 90/91；CapTable 槽 = 120 % MAX_PROCS）。
/// 对象一律 `MemoryRegion`（不占 Endpoint/Notification 实体槽——boot 至此
/// 实体 index 可能已逼近 64 上限，见 kstate 模块头 MVP 约束）。
pub fn security_smoke() {
    const TEST_PID: Pid = Pid(120);
    let mut total = 0usize;
    info!("[sec-smoke] P4-T13 安全回归 start (test pid=120)");

    kstate::k_create_cap_table(TEST_PID).expect("[sec-smoke] create cap table");

    // ---- 1. 非法 cptr：NULL trap + 空槽 ----
    assert_eq!(resolve_cap(120, 0, Rights::EMPTY).unwrap_err(), E_INVALID_CAP);
    assert_eq!(resolve_cap(120, 200, Rights::EMPTY).unwrap_err(), E_INVALID_CAP);
    total += 2;
    info!("[sec-smoke]   ok: 1. 非法 cptr（NULL trap + 空槽）→ -1 ×2");

    // ---- 2. delegate 无 GRANT → -7；attenuation-only 不可放大 ----
    let obj_a = kstate::k_alloc_object(ObjKind::MemoryRegion).expect("[sec-smoke] alloc A");
    let cap_a = kstate::k_mint_root(TEST_PID, obj_a, Rights::READ | Rights::WRITE)
        .expect("[sec-smoke] mint A");
    // 无 GRANT 的 cap 不能 delegate（直查表内 delegate，绕过 child_out 写回）
    let denied = kstate::with_cap_table(TEST_PID, |t| {
        t.delegate(cap_a, Rights::READ).unwrap_err()
    });
    assert!(matches!(denied, CapError::Permission));
    // GRANT 版：mask=ALL 派生 → 子权限 == 父权限 ∩ ALL（无放大）
    let obj_b = kstate::k_alloc_object(ObjKind::MemoryRegion).expect("[sec-smoke] alloc B");
    let cap_b = kstate::k_mint_root(
        TEST_PID,
        obj_b,
        Rights::READ | Rights::WRITE | Rights::GRANT,
    )
    .expect("[sec-smoke] mint B");
    let child = kstate::with_cap_table(TEST_PID, |t| {
        t.delegate(cap_b, Rights::ALL).expect("[sec-smoke] delegate ALL")
    });
    let (child_rights, child_parent) = kstate::with_cap_table(TEST_PID, |t| {
        let c = t.get(child).expect("child exists");
        (c.rights, c.parent)
    });
    assert_eq!(child_rights, Rights::READ | Rights::WRITE | Rights::GRANT);
    assert_eq!(child_parent, Some(cap_b));
    // mask 越权位（EXEC 父不持有）→ 子 = 交集，EXEC 被剥（降级不可绕过）
    let child2 = kstate::with_cap_table(TEST_PID, |t| {
        t.delegate(cap_b, Rights::ALL.union(Rights::EXEC)).expect("delegate ALL|EXEC")
    });
    let child2_rights = kstate::with_cap_table(TEST_PID, |t| t.get(child2).unwrap().rights);
    assert!(!child2_rights.contains(Rights::EXEC), "attenuation must strip EXEC");
    total += 4;
    info!("[sec-smoke]   ok: 2. delegate 无 GRANT→-7 + attenuation-only（ALL 无放大 / EXEC 被剥）");

    // ---- 3. revoke 级联：父槽 + 两派生子槽全释放；对象 Retired ----
    let revoked = kstate::with_cap_table(TEST_PID, |t| t.revoke_cascade(cap_b))
        .expect("[sec-smoke] revoke cascade");
    assert_eq!(revoked, 3, "cascade must free cap_b + child + child2");
    kstate::with_objects(|o| {
        o.begin_revoke(obj_b).expect("begin_revoke B");
        o.retire(obj_b).expect("retire B");
    });
    // 级联后旧 cptr 全变空槽 → -1（槽已释放）
    assert_eq!(resolve_cap(120, cap_b, Rights::EMPTY).unwrap_err(), E_INVALID_CAP);
    assert_eq!(resolve_cap(120, child, Rights::EMPTY).unwrap_err(), E_INVALID_CAP);
    total += 3;
    info!("[sec-smoke]   ok: 3. revoke 级联释放 3 槽 + 旧 cptr → -1");

    // ---- 4. E_OBJECT_RETIRED 稳定性（Phase 4 退出标准第 4 条）----
    // 场景：其他进程持有的副本未被级联触达（槽还在），对象已 Retired →
    // 解析必须**稳定**返回 -12（两次同码），而非 -1（槽仍占用）。
    let obj_c = kstate::k_alloc_object(ObjKind::MemoryRegion).expect("[sec-smoke] alloc C");
    let cap_c = kstate::k_mint_root(TEST_PID, obj_c, Rights::READ | Rights::GRANT)
        .expect("[sec-smoke] mint C");
    kstate::with_objects(|o| {
        o.begin_revoke(obj_c).expect("begin_revoke C");
        o.retire(obj_c).expect("retire C");
    });
    // 不跑级联——cap_c 槽仍在表中，指向 Retired 对象
    assert_eq!(resolve_cap(120, cap_c, Rights::EMPTY).unwrap_err(), E_OBJECT_RETIRED);
    assert_eq!(resolve_cap(120, cap_c, Rights::EMPTY).unwrap_err(), E_OBJECT_RETIRED);
    // Revoking 中间态同样拒绝（新对象走 begin_revoke 不 retire）
    let obj_d = kstate::k_alloc_object(ObjKind::MemoryRegion).expect("[sec-smoke] alloc D");
    let cap_d = kstate::k_mint_root(TEST_PID, obj_d, Rights::READ).expect("[sec-smoke] mint D");
    kstate::with_objects(|o| o.begin_revoke(obj_d).expect("begin_revoke D"));
    assert_eq!(resolve_cap(120, cap_d, Rights::EMPTY).unwrap_err(), E_OBJECT_RETIRED);
    total += 3;
    info!("[sec-smoke]   ok: 4. 旧句柄稳定 E_OBJECT_RETIRED（Retired ×2 + Revoking ×1）");

    // ---- 5. 过期 generation：同 index 复用后旧句柄不复活 ----
    // obj_c 已 Retired；free 后重分配大概率复用 index（generation +1）。
    kstate::k_free_object(obj_c).expect("[sec-smoke] free C");
    let obj_e = kstate::k_alloc_object(ObjKind::MemoryRegion).expect("[sec-smoke] alloc E");
    if obj_e.index == obj_c.index {
        // 旧 cap_c 持旧 generation → check_live 必须拒绝（-12 或 -1，不得 Ok）
        let stale = resolve_cap(120, cap_c, Rights::EMPTY);
        assert!(stale.is_err(), "stale generation must not resolve");
        assert_eq!(stale.unwrap_err(), E_OBJECT_RETIRED);
        total += 1;
        info!(
            "[sec-smoke]   ok: 5. generation 复用（index {} gen {}→{}）旧句柄拒绝",
            obj_c.index, obj_c.generation, obj_e.generation
        );
    } else {
        info!("[sec-smoke]   skip: 5. index 未复用（{}≠{}），generation 路径由宿主测试覆盖",
            obj_e.index, obj_c.index);
    }

    // ---- 6. cap 表满故障注入：255 槽填满后 alloc → 错误码，不 panic ----
    // （表 = 256 槽，slot 0 NULL trap 永久保留；对象复用 obj_e 免爆对象表）
    let mut filled = 0u32;
    let full_err = loop {
        match kstate::with_cap_table(TEST_PID, |t| {
            t.alloc(synapse_cap::Capability::root(obj_e, Rights::READ))
        }) {
            Ok(_) => filled += 1,
            Err(e) => break e,
        }
        if filled > 300 {
            panic!("[sec-smoke] cap table fill runaway");
        }
    };
    assert!(
        matches!(full_err, CapError::NoMemory),
        "table full must map to NoMemory, got {:?}",
        full_err
    );
    assert_eq!(cap_err_to_code(full_err), synapse_abi::E_NO_MEMORY);
    total += 2;
    info!("[sec-smoke]   ok: 6. 表满故障注入（filled={} 后 NoMemory → -3，零 panic）", filled);

    // ---- 7. 伪造 agent_id：spawn 复用已注册 agent → AgentIdConflict ----
    let dup = kstate::with_procs(|t| {
        t.spawn(
            synapse_proc::process::INIT_PID,
            synapse_proc::process::SpawnParams {
                agent: synapse_ipc::AgentId(1), // init 的 agent_id——伪造冲突
                quota: synapse_cap::DEFAULT_QUOTA,
                death_endpoint: 0,
            },
        )
    });
    assert!(
        matches!(dup, Err(CapError::AgentIdConflict)),
        "duplicate agent_id must be rejected, got {:?}",
        dup
    );
    total += 1;
    info!("[sec-smoke]   ok: 7. 伪造 agent_id(1) spawn → AgentIdConflict(-6)");

    // ---- 8. 重复回收：revoke 已 Retired 对象 / free 已 free 对象 ----
    // cap_c 槽指向已 free 的旧 generation → resolve -12/-1（第 5 步已证）；
    // 再次 begin_revoke obj_c（已 Freed）→ 必须错误而非 panic。
    let re_rev = kstate::with_objects(|o| o.begin_revoke(obj_c));
    assert!(re_rev.is_err(), "re-revoke freed object must fail cleanly");
    // 幂等 free：k_free_object 二次调用同样不得 panic
    let re_free = kstate::k_free_object(obj_c);
    assert!(re_free.is_err() || re_free.is_ok(), "re-free must not panic");
    total += 2;
    info!("[sec-smoke]   ok: 8. 重复回收（re-revoke → 错误码；re-free 零 panic）");

    // ---- 9. 跨进程级联：k_revoke_cascade_global 触达他表副本 ----
    // TEST_PID2 = 121 持 obj_e 副本（模拟 delegate 跨进程转移后的持有），
    // 从 120 侧发起全局级联 → 两表槽全清。
    const TEST_PID2: Pid = Pid(121);
    kstate::k_create_cap_table(TEST_PID2).expect("[sec-smoke] create table 121");
    let cap_e2 = kstate::k_mint_root(TEST_PID2, obj_e, Rights::READ | Rights::GRANT)
        .expect("[sec-smoke] mint E in 121");
    kstate::with_objects(|o| o.begin_revoke(obj_e).expect("begin_revoke E"));
    let swept = kstate::k_revoke_cascade_global(obj_e);
    assert!(swept >= 1, "global cascade must sweep 121's copy");
    // 121 侧槽已清 → -1；120 侧 obj_e 的填满槽也全部被清（同 obj）
    assert_eq!(resolve_cap(121, cap_e2, Rights::EMPTY).unwrap_err(), E_INVALID_CAP);
    kstate::with_objects(|o| o.retire(obj_e).expect("retire E"));
    total += 2;
    info!("[sec-smoke]   ok: 9. 跨进程级联（swept {} 槽，他表副本 → -1）", swept);

    // ---- 清理：obj_a/obj_d 释放 + 测试表销毁（FR8 无页帧参与，堆零分配）----
    kstate::with_objects(|o| {
        o.begin_revoke(obj_a).ok();
        o.retire(obj_a).ok();
        o.begin_revoke(obj_d).ok();
        o.retire(obj_d).ok();
    });
    kstate::k_free_object(obj_a).ok();
    kstate::k_free_object(obj_d).ok();
    kstate::k_destroy_cap_table(TEST_PID);
    kstate::k_destroy_cap_table(TEST_PID2);

    info!("[sec-smoke] PASS — {total}/{} 安全回归断言全绿", total);
}
