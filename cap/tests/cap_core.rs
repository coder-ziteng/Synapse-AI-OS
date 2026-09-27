//! synapse-cap 宿主单元测试（NFR4/NFR5：核心逻辑全部可脱离内核测试）。
//!
//! 覆盖矩阵（对齐设计评审测试矩阵行"Cap rights、generation、撤销、配额"）：
//! rights 语义 / CapTable 分配-委托-级联撤销 / ObjectTable generation 与状态机 /
//! atomic cap transfer / 配额 / errno 映射。

use synapse_cap::*;

// ---------- helpers ----------

fn root_cap(obj: ObjRef, rights: Rights) -> Capability {
    Capability::root(obj, rights)
}

// ---------- rights ----------

#[test]
fn rights_contains_is_subset_check() {
    let held = Rights::SEND | Rights::RECV | Rights::GRANT;
    assert!(held.contains(Rights::SEND));
    assert!(held.contains(Rights::SEND | Rights::GRANT));
    assert!(!held.contains(Rights::WRITE));
    assert!(held.contains(Rights::EMPTY));
    assert!(Rights::EMPTY.is_empty());
}

#[test]
fn rights_attenuation_only() {
    let held = Rights::SEND | Rights::RECV;
    let att = held.intersection(Rights::SEND | Rights::WRITE);
    assert_eq!(att, Rights::SEND); // WRITE 被衰减掉，不可放大
}

#[test]
fn rights_from_bits_masks_reserved() {
    // bit 7..=31 保留（Doc 02 §4.5：只允许追加），from_bits 必须掩掉
    let r = Rights::from_bits(0xFFFF_FFFF);
    assert_eq!(r, Rights::ALL);
    assert_eq!(r.bits(), 0b111_1111);
}

// ---------- CapTable ----------

#[test]
fn cap_table_slot0_is_null_trap() {
    let mut t = CapTable::new();
    let obj = ObjRef { index: 0, generation: 1 };
    for _ in 0..8 {
        let c = t.alloc(root_cap(obj, Rights::SEND)).unwrap();
        assert_ne!(c, 0, "alloc 永不返回 slot 0（Doc 02 §5.2 NULL trap）");
    }
    assert!(t.get(0).is_err());
    assert!(t.free(0).is_err());
}

#[test]
fn cap_table_full_returns_no_memory() {
    let mut t = CapTable::new();
    let obj = ObjRef { index: 0, generation: 1 };
    assert_eq!(t.remaining_free(), 255);
    for _ in 0..255 {
        t.alloc(root_cap(obj, Rights::SEND)).unwrap();
    }
    assert_eq!(t.remaining_free(), 0);
    assert_eq!(t.alloc(root_cap(obj, Rights::SEND)), Err(CapError::NoMemory));
}

#[test]
fn cap_table_free_then_reuse() {
    let mut t = CapTable::new();
    let obj = ObjRef { index: 0, generation: 1 };
    let c = t.alloc(root_cap(obj, Rights::SEND)).unwrap();
    t.free(c).unwrap();
    assert!(t.get(c).is_err()); // 空槽 → InvalidCap
    assert_eq!(t.get(200).err(), Some(CapError::InvalidCap));
    let c2 = t.alloc(root_cap(obj, Rights::RECV)).unwrap();
    assert_eq!(c2, c); // 空闲栈 LIFO 复用
    assert_eq!(t.get(c2).unwrap().rights, Rights::RECV);
}

// ---------- delegate ----------

#[test]
fn delegate_requires_grant_and_attenuates() {
    let mut t = CapTable::new();
    let obj = ObjRef { index: 3, generation: 1 };
    let src = t.alloc(root_cap(obj, Rights::SEND | Rights::RECV | Rights::GRANT)).unwrap();

    // 尝试放大权限：mask 含 WRITE，结果只能是交集
    let child = t.delegate(src, Rights::SEND | Rights::WRITE).unwrap();
    assert_eq!(t.get(child).unwrap().rights, Rights::SEND);
    assert_eq!(t.get(child).unwrap().parent, Some(src));

    // 无 GRANT 位的 cap 不可再委托
    let no_grant = t.alloc(root_cap(obj, Rights::SEND)).unwrap();
    assert_eq!(t.delegate(no_grant, Rights::SEND), Err(CapError::Permission));

    // 不存在的源 → InvalidCap
    assert_eq!(t.delegate(250, Rights::SEND), Err(CapError::InvalidCap));
}

// ---------- revoke cascade ----------

#[test]
fn revoke_cascade_kills_derived_spares_unrelated() {
    let mut t = CapTable::new();
    let obj = ObjRef { index: 1, generation: 1 };
    let other = ObjRef { index: 2, generation: 1 };
    let full = Rights::ALL;

    let root = t.alloc(root_cap(obj, full)).unwrap();
    let child = t.delegate(root, Rights::SEND | Rights::GRANT).unwrap();
    let grandchild = t.delegate(child, Rights::SEND).unwrap();
    let unrelated = t.alloc(root_cap(other, full)).unwrap();
    let sibling = t.delegate(root, Rights::RECV).unwrap();

    // 撤销 child → child + grandchild 死；root / sibling / unrelated 活
    let n = t.revoke_cascade(child).unwrap();
    assert_eq!(n, 2);
    assert!(t.get(child).is_err());
    assert!(t.get(grandchild).is_err());
    assert!(t.get(root).is_ok());
    assert!(t.get(sibling).is_ok());
    assert!(t.get(unrelated).is_ok());

    // 撤销 root → sibling 也死（root 已无 parent 链上游）
    let n = t.revoke_cascade(root).unwrap();
    assert_eq!(n, 2); // root + sibling
    assert!(t.get(sibling).is_err());
    assert!(t.get(unrelated).is_ok());

    // 撤销不存在的槽 → InvalidCap，无副作用
    assert_eq!(t.revoke_cascade(250), Err(CapError::InvalidCap));
}

// ---------- ObjectTable: generation ----------

#[test]
fn generation_invalidates_stale_refs() {
    let mut ot = ObjectTable::new();
    let a = ot.alloc(ObjKind::Endpoint).unwrap();
    assert_eq!(ot.check_live(a), Ok(ObjKind::Endpoint));
    ot.free(a).unwrap();

    // 槽空 → 旧引用 ObjectRetired
    assert_eq!(ot.check_live(a), Err(CapError::ObjectRetired));

    // 复用同槽 → generation 递增，旧引用仍失效
    let b = ot.alloc(ObjKind::Endpoint).unwrap();
    assert_eq!(b.index, a.index);
    assert_ne!(b.generation, a.generation);
    assert_eq!(ot.check_live(a), Err(CapError::ObjectRetired));
    assert_eq!(ot.check_live(b), Ok(ObjKind::Endpoint));
}

#[test]
fn object_out_of_range_is_invalid_cap() {
    let ot = ObjectTable::new();
    let bogus = ObjRef { index: 10_000, generation: 1 };
    assert_eq!(ot.check_live(bogus), Err(CapError::InvalidCap));
    assert_eq!(ot.state_of(bogus), Err(CapError::InvalidCap));
}

#[test]
fn object_table_full_returns_no_memory() {
    let mut ot = ObjectTable::new();
    for _ in 0..1024 {
        ot.alloc(ObjKind::MemoryRegion).unwrap();
    }
    assert_eq!(ot.alloc(ObjKind::MemoryRegion), Err(CapError::NoMemory));
    assert_eq!(ot.live_count(), 1024);
}

// ---------- ObjectTable: 状态机 ----------

#[test]
fn lifecycle_state_machine() {
    let mut ot = ObjectTable::new();
    let o = ot.alloc(ObjKind::Endpoint).unwrap();
    assert_eq!(ot.state_of(o), Ok(ObjState::Live));

    ot.begin_revoke(o).unwrap();
    assert_eq!(ot.state_of(o), Ok(ObjState::Revoking));
    // Revoking 期间 check_live 必须拒绝（新 invoke 立即失败，不等待遍历）
    assert_eq!(ot.check_live(o), Err(CapError::ObjectRetired));
    // 重复 begin_revoke 非法
    assert_eq!(ot.begin_revoke(o), Err(CapError::ObjectRetired));

    ot.retire(o).unwrap();
    assert_eq!(ot.state_of(o), Ok(ObjState::Retired));
    assert_eq!(ot.retire(o), Err(CapError::ObjectRetired));

    ot.free(o).unwrap();
    // Freed 后槽空：state_of / free 均 ObjectRetired
    assert_eq!(ot.state_of(o), Err(CapError::ObjectRetired));
    assert_eq!(ot.free(o), Err(CapError::ObjectRetired));
    assert_eq!(ot.live_count(), 0);
}

#[test]
fn retire_requires_revoking_state() {
    let mut ot = ObjectTable::new();
    let o = ot.alloc(ObjKind::Endpoint).unwrap();
    // Live → Retired 直跳非法
    assert_eq!(ot.retire(o), Err(CapError::ObjectRetired));
    assert_eq!(ot.state_of(o), Ok(ObjState::Live));
}

// ---------- transfer ----------

fn setup_pair() -> (ObjectTable, CapTable, CapTable, ObjRef) {
    let mut ot = ObjectTable::new();
    let obj = ot.alloc(ObjKind::Endpoint).unwrap();
    let src = CapTable::new();
    let dst = CapTable::new();
    (ot, src, dst, obj)
}

#[test]
fn transfer_success_attenuates_and_links_parent() {
    let (ot, mut src, mut dst, obj) = setup_pair();
    let s = src.alloc(root_cap(obj, Rights::SEND | Rights::RECV | Rights::GRANT)).unwrap();
    // 接收方持有的"来源 capability"（endpoint cap），作为 dst_parent
    let dst_src_ref = dst.alloc(root_cap(obj, Rights::SEND)).unwrap();

    let items = [TransferItem { cptr: s, mask: Rights::SEND | Rights::WRITE }];
    let out = transfer_caps(&src, &mut dst, &items, Some(dst_src_ref), &ot).unwrap();
    let got = dst.get(out[0]).unwrap();
    assert_eq!(got.rights, Rights::SEND); // WRITE 被衰减
    assert_eq!(got.parent, Some(dst_src_ref)); // 撤销链跨进程可追踪
    assert_eq!(got.obj, obj);
    // 源 cap 保持原位（transfer 是复制授予，不是移动）
    assert!(src.get(s).is_ok());
}

#[test]
fn transfer_requires_grant_and_is_atomic() {
    let (ot, mut src, mut dst, obj) = setup_pair();
    let good = src.alloc(root_cap(obj, Rights::SEND | Rights::GRANT)).unwrap();
    let no_grant = src.alloc(root_cap(obj, Rights::SEND)).unwrap();

    let before = dst.remaining_free();
    let items = [
        TransferItem { cptr: good, mask: Rights::SEND },
        TransferItem { cptr: no_grant, mask: Rights::SEND },
    ];
    // 第 2 项缺 GRANT → 整批失败，dst 零副作用
    assert_eq!(
        transfer_caps(&src, &mut dst, &items, None, &ot),
        Err(CapError::Permission)
    );
    assert_eq!(dst.remaining_free(), before);
}

#[test]
fn transfer_dst_full_atomic_rollback() {
    let (ot, mut src, mut dst, obj) = setup_pair();
    let s = src.alloc(root_cap(obj, Rights::SEND | Rights::GRANT)).unwrap();
    // 填满 dst
    let filler = root_cap(obj, Rights::EMPTY);
    while dst.remaining_free() > 0 {
        dst.alloc(filler.clone()).unwrap();
    }
    let items = [TransferItem { cptr: s, mask: Rights::SEND }];
    assert_eq!(
        transfer_caps(&src, &mut dst, &items, None, &ot),
        Err(CapError::NoMemory)
    );
    // 源保持原位
    assert!(src.get(s).is_ok());
}

#[test]
fn transfer_rejects_retired_object() {
    let (mut ot, mut src, mut dst, obj) = setup_pair();
    let s = src.alloc(root_cap(obj, Rights::SEND | Rights::GRANT)).unwrap();
    ot.begin_revoke(obj).unwrap();
    let items = [TransferItem { cptr: s, mask: Rights::SEND }];
    assert_eq!(
        transfer_caps(&src, &mut dst, &items, None, &ot),
        Err(CapError::ObjectRetired)
    );
    assert_eq!(dst.remaining_free(), 255);
}

#[test]
fn transfer_rejects_missing_source_and_oversize_batch() {
    let (ot, src, mut dst, _obj) = setup_pair();
    // 源槽空 → InvalidCap
    let items = [TransferItem { cptr: 42, mask: Rights::SEND }];
    assert_eq!(
        transfer_caps(&src, &mut dst, &items, None, &ot),
        Err(CapError::InvalidCap)
    );
    // 超过 MAX_TRANSFER → InvalidCap（ABI 层应先行拒绝）
    let big = [TransferItem { cptr: 1, mask: Rights::EMPTY }; MAX_TRANSFER + 1];
    assert_eq!(
        transfer_caps(&src, &mut dst, &big, None, &ot),
        Err(CapError::InvalidCap)
    );
    // 空批次恒成功
    let out = transfer_caps(&src, &mut dst, &[], None, &ot).unwrap();
    assert_eq!(out[0], 0);
}

// ---------- quota ----------

#[test]
fn quota_charge_and_exceed() {
    let q = DEFAULT_QUOTA;
    q.validate().unwrap();
    let mut u = QuotaUsage::new();
    u.charge(&q, Resource::Caps, 64).unwrap(); // 恰好达上限
    assert_eq!(u.used(Resource::Caps), 64);
    assert_eq!(
        u.charge(&q, Resource::Caps, 1),
        Err(CapError::QuotaExceeded)
    );
    u.release(Resource::Caps, 10);
    assert_eq!(u.used(Resource::Caps), 54);
    u.charge(&q, Resource::Caps, 10).unwrap();
    // 饱和释放：release 多于 charge 归零不回绕
    u.release(Resource::Caps, 1000);
    assert_eq!(u.used(Resource::Caps), 0);
}

#[test]
fn quota_msg_size_and_validate() {
    let q = DEFAULT_QUOTA;
    let u = QuotaUsage::new();
    u.check_msg_size(&q, 4096).unwrap();
    assert_eq!(u.check_msg_size(&q, 4097), Err(CapError::QuotaExceeded));

    // max_caps > 256 非法
    let bad = Quota { max_caps: 257, ..DEFAULT_QUOTA };
    assert_eq!(bad.validate(), Err(CapError::QuotaExceeded));
    // max_msg_size > 4096 非法
    let bad2 = Quota { max_msg_size: 8192, ..DEFAULT_QUOTA };
    assert_eq!(bad2.validate(), Err(CapError::QuotaExceeded));
}

#[test]
fn quota_spawn_grant_from_parent_remaining() {
    let pq = DEFAULT_QUOTA;
    let mut pu = QuotaUsage::new();
    let child = Quota { max_caps: 32, max_pages: 1024, ..DEFAULT_QUOTA };
    synapse_cap::quota::check_spawn_grant(&pq, &pu, &child).unwrap();

    // 父剩余不足 → 拒绝 spawn 划拨
    pu.charge(&pq, Resource::Pages, 4000).unwrap();
    assert_eq!(
        synapse_cap::quota::check_spawn_grant(&pq, &pu, &child),
        Err(CapError::QuotaExceeded)
    );
}

// ---------- errno ----------

#[test]
fn errno_mapping_matches_doc02() {
    // Doc 02 §4.3（-1..-10）+ §4.5（-11..-15），一一对应且互不重复
    let pairs = [
        (CapError::InvalidCap, -1),
        (CapError::InvalidAddr, -2),
        (CapError::NoMemory, -3),
        (CapError::WouldBlock, -4),
        (CapError::NotFound, -5),
        (CapError::AgentIdConflict, -6),
        (CapError::Permission, -7),
        (CapError::Frozen, -8),
        (CapError::Zombie, -9),
        (CapError::NotImplemented, -10),
        (CapError::AbiMismatch, -11),
        (CapError::ObjectRetired, -12),
        (CapError::QuotaExceeded, -13),
        (CapError::PeerDied, -14),
        (CapError::Timeout, -15),
    ];
    for (e, expected) in pairs {
        assert_eq!(e.errno(), expected, "{e:?} errno 与 Doc 02 不符");
    }
}
