//! synapse-proc 宿主单元测试（NFR4/NFR5）。
//!
//! 覆盖：spawn 校验链 / 生命周期状态机 / death signal / 孤儿过继 /
//! reap 与配额归还 / freeze-thaw 行为围栏 / agent 注册表 / 初始 caps 授予。

use synapse_cap::{
    Capability, CapError, ObjectTable, ObjKind, Quota, Resource, Rights, DEFAULT_QUOTA,
};
use synapse_ipc::AgentId;
use synapse_proc::agent::AgentRegistry;
use synapse_proc::*;

fn params(agent: u32) -> SpawnParams {
    SpawnParams {
        agent: AgentId(agent),
        quota: DEFAULT_QUOTA,
        death_endpoint: 1,
    }
}

// ---------- 初始化 ----------

#[test]
fn init_process_is_installed_at_boot() {
    let t = ProcessTable::new(AgentId(1));
    let init = t.get(INIT_PID).unwrap();
    assert_eq!(init.state, ProcState::Running);
    assert_eq!(init.parent, Pid::NULL);
    assert_eq!(init.quota, INIT_QUOTA);
    assert_eq!(t.agents.lookup(AgentId(1)), Some(INIT_PID));
}

// ---------- spawn ----------

#[test]
fn spawn_by_init_creates_child_and_carves_quota() {
    let mut t = ProcessTable::new(AgentId(1));
    let child = t.spawn(INIT_PID, params(10)).unwrap();
    assert_ne!(child, INIT_PID);
    assert_eq!(t.get(child).unwrap().state, ProcState::Created);
    assert_eq!(t.get(child).unwrap().parent, INIT_PID);
    assert_eq!(t.get(child).unwrap().death_endpoint, 1);
    // 配额划拨：父账上按子上限预留
    assert_eq!(t.get(INIT_PID).unwrap().usage.used(Resource::Pages), DEFAULT_QUOTA.max_pages);
    assert_eq!(t.get(INIT_PID).unwrap().usage.used(Resource::Caps), DEFAULT_QUOTA.max_caps as u32);
    // agent 注册
    assert_eq!(t.agents.lookup(AgentId(10)), Some(child));
}

#[test]
fn spawn_only_allowed_for_init_in_phase4() {
    let mut t = ProcessTable::new(AgentId(1));
    let c1 = t.spawn(INIT_PID, params(10)).unwrap();
    t.set_running(c1).unwrap();
    // 非 init spawn → Permission（Doc 02 §5.2 首期限制）
    assert_eq!(t.spawn(c1, params(11)), Err(CapError::Permission));
}

#[test]
fn spawn_agent_conflict_and_quota_exceeded() {
    let mut t = ProcessTable::new(AgentId(1));
    t.spawn(INIT_PID, params(10)).unwrap();
    // agent_id 重复 → AgentIdConflict（-6）
    assert_eq!(t.spawn(INIT_PID, params(10)), Err(CapError::AgentIdConflict));

    // 子配额超父剩余 → QuotaExceeded（-13）
    let greedy = Quota { max_pages: INIT_QUOTA.max_pages, ..DEFAULT_QUOTA };
    assert_eq!(
        t.spawn(INIT_PID, SpawnParams { agent: AgentId(11), quota: greedy, death_endpoint: 1 }),
        Err(CapError::QuotaExceeded)
    );

    // 非法子配额（max_caps > 256）→ QuotaExceeded
    let bad = Quota { max_caps: 300, ..DEFAULT_QUOTA };
    assert_eq!(
        t.spawn(INIT_PID, SpawnParams { agent: AgentId(12), quota: bad, death_endpoint: 1 }),
        Err(CapError::QuotaExceeded)
    );
}

#[test]
fn spawn_rejects_dead_or_frozen_parent() {
    let mut t = ProcessTable::new(AgentId(1));
    t.freeze(INIT_PID).unwrap();
    assert_eq!(t.spawn(INIT_PID, params(10)), Err(CapError::Frozen));
    t.thaw(INIT_PID).unwrap();

    t.exit(INIT_PID, 0).unwrap();
    assert_eq!(t.spawn(INIT_PID, params(10)), Err(CapError::Zombie));
    assert_eq!(t.spawn(Pid(999), params(10)), Err(CapError::NotFound));
}

#[test]
fn spawn_pid_monotonic_no_reuse() {
    let mut t = ProcessTable::new(AgentId(1));
    let a = t.spawn(INIT_PID, params(10)).unwrap();
    let b = t.spawn(INIT_PID, params(11)).unwrap();
    assert_eq!(b.0, a.0 + 1);
    // reap 后 pid 不复用
    t.exit(a, 0).unwrap();
    t.release_resources(a).unwrap();
    t.reap(INIT_PID, a).unwrap();
    let c = t.spawn(INIT_PID, params(12)).unwrap();
    assert_ne!(c, a);
    assert!(t.get(a).is_none()); // 陈旧 pid → NotFound
}

// ---------- 状态机 ----------

#[test]
fn lifecycle_full_path() {
    let mut t = ProcessTable::new(AgentId(1));
    let p = t.spawn(INIT_PID, params(10)).unwrap();
    assert_eq!(t.get(p).unwrap().state, ProcState::Created);

    t.set_running(p).unwrap();
    assert_eq!(t.get(p).unwrap().state, ProcState::Running);
    t.set_running(p).unwrap(); // 幂等
    t.set_blocked(p).unwrap();
    assert_eq!(t.get(p).unwrap().state, ProcState::Blocked);
    t.set_running(p).unwrap();

    // exit → Exited + death signal
    let sig = t.exit(p, 42).unwrap();
    assert_eq!(sig, DeathSignal { pid: p, exit_code: 42, fault_reason: None });
    assert_eq!(t.get(p).unwrap().state, ProcState::Exited);
    // 重复 exit → Zombie（-9 已终止需先 reap）
    assert_eq!(t.exit(p, 0), Err(CapError::Zombie));
    assert_eq!(t.set_running(p), Err(CapError::Zombie));

    // release_resources → Zombie
    t.release_resources(p).unwrap();
    assert_eq!(t.get(p).unwrap().state, ProcState::Zombie);
    t.release_resources(p).unwrap(); // 幂等
    // 活进程不可 release
    let q = t.spawn(INIT_PID, params(11)).unwrap();
    assert_eq!(t.release_resources(q), Err(CapError::Permission));
}

#[test]
fn fault_path_carries_reason() {
    let mut t = ProcessTable::new(AgentId(1));
    let p = t.spawn(INIT_PID, params(10)).unwrap();
    t.set_running(p).unwrap();
    let sig = t.fault(p, FaultKind::SegFault, -1).unwrap();
    assert_eq!(sig.fault_reason, Some(FaultKind::SegFault));
    assert_eq!(t.get(p).unwrap().state, ProcState::Faulted);
    t.release_resources(p).unwrap();
    assert_eq!(t.get(p).unwrap().state, ProcState::Zombie);
}

// ---------- reap ----------

#[test]
fn reap_frees_pid_agent_and_returns_quota() {
    let mut t = ProcessTable::new(AgentId(1));
    let p = t.spawn(INIT_PID, params(10)).unwrap();
    let reserved = t.get(INIT_PID).unwrap().usage.used(Resource::Pages);
    assert_eq!(reserved, DEFAULT_QUOTA.max_pages);

    // 活进程不可收尸
    assert_eq!(t.reap(INIT_PID, p), Err(CapError::Permission));
    t.exit(p, 0).unwrap();
    // Exited 尚未 release → Zombie 错误（需先释放资源）
    assert_eq!(t.reap(INIT_PID, p), Err(CapError::Zombie));
    t.release_resources(p).unwrap();

    // 非父进程收尸 → Permission（另建一进程模拟）
    let other = t.spawn(INIT_PID, params(11)).unwrap();
    t.set_running(other).unwrap();
    assert_eq!(t.reap(other, p), Err(CapError::Permission));

    // 父进程（init）收尸 → 成功
    t.reap(INIT_PID, p).unwrap();
    assert!(t.get(p).is_none());
    assert_eq!(t.agents.lookup(AgentId(10)), None); // agent_id 已释放
    // 配额预留已归还：init 账上只剩 other 的预留
    assert_eq!(t.get(INIT_PID).unwrap().usage.used(Resource::Pages), DEFAULT_QUOTA.max_pages);
    // agent 可被新进程复用
    let p2 = t.spawn(INIT_PID, params(10)).unwrap();
    assert_eq!(t.agents.lookup(AgentId(10)), Some(p2));
    // p2 预留叠加：other + p2 = 2 份
    assert_eq!(t.get(INIT_PID).unwrap().usage.used(Resource::Pages), 2 * DEFAULT_QUOTA.max_pages);
    // 双重 reap → NotFound
    assert_eq!(t.reap(INIT_PID, p), Err(CapError::NotFound));
}

// ---------- 孤儿过继 ----------

#[test]
fn orphan_reparented_to_init() {
    let mut t = ProcessTable::new(AgentId(1));
    let parent = t.spawn(INIT_PID, params(10)).unwrap();
    t.set_running(parent).unwrap();
    let child = t.spawn(INIT_PID, params(11)).unwrap();
    t.set_running(child).unwrap();
    // 模拟 Phase 5+ 多级进程树：child.parent = parent
    t.get_mut(child).unwrap().parent = parent;

    // parent 退出 → child 过继给 init（Doc 02 §5.3）
    t.exit(parent, 1).unwrap();
    assert_eq!(t.get(child).unwrap().parent, INIT_PID);

    // child 随后终止 → death signal 走它自己注册的 death_endpoint，
    // 收尸人是 init
    t.exit(child, 2).unwrap();
    t.release_resources(child).unwrap();
    t.reap(INIT_PID, child).unwrap();
    assert!(t.get(child).is_none());
}

// ---------- freeze / thaw ----------

#[test]
fn freeze_thaw_behavior_fence() {
    let mut t = ProcessTable::new(AgentId(1));
    let p = t.spawn(INIT_PID, params(10)).unwrap();
    t.set_running(p).unwrap();
    t.ensure_accepts_ipc(p).unwrap();

    t.freeze(p).unwrap();
    assert!(t.get(p).unwrap().frozen);
    // 冻结进程拒收 IPC → Frozen（-8）
    assert_eq!(t.ensure_accepts_ipc(p), Err(CapError::Frozen));
    t.thaw(p).unwrap();
    t.ensure_accepts_ipc(p).unwrap();

    // 终止进程：freeze → Zombie 错误；终止自动解冻
    t.freeze(p).unwrap();
    t.exit(p, 0).unwrap();
    assert!(!t.get(p).unwrap().frozen);
    assert_eq!(t.freeze(p), Err(CapError::Zombie));
    assert_eq!(t.ensure_accepts_ipc(p), Err(CapError::Zombie));
}

// ---------- agent registry ----------

#[test]
fn agent_registry_uniqueness_and_release() {
    let mut r = AgentRegistry::new();
    assert!(r.is_empty());
    r.register(AgentId(7), Pid(2)).unwrap();
    assert_eq!(r.lookup(AgentId(7)), Some(Pid(2)));
    assert_eq!(r.find_by_owner(Pid(2)), Some(AgentId(7)));
    // 重复注册 → AgentIdConflict
    assert_eq!(r.register(AgentId(7), Pid(3)), Err(CapError::AgentIdConflict));
    // 未盖章哨兵不可注册
    assert_eq!(r.register(AgentId::UNSTAMPED, Pid(3)), Err(CapError::InvalidCap));
    r.release(AgentId(7));
    assert_eq!(r.lookup(AgentId(7)), None);
    r.release(AgentId(7)); // no-op
    assert_eq!(r.len(), 0);
}

// ---------- 初始 caps 授予 ----------

#[test]
fn initial_caps_installed_atomically() {
    let mut objects = ObjectTable::new();
    let ep = objects.alloc(ObjKind::Endpoint).unwrap();
    let mem = objects.alloc(ObjKind::MemoryRegion).unwrap();

    let mut parent = synapse_cap::CapTable::new();
    let c_ep = parent
        .alloc(Capability::root(ep, Rights::SEND | Rights::RECV | Rights::GRANT))
        .unwrap();
    let c_mem = parent
        .alloc(Capability::root(mem, Rights::READ | Rights::WRITE | Rights::GRANT))
        .unwrap();
    let mut child = synapse_cap::CapTable::new();

    let items = [
        GrantItem { cptr: c_ep, mask: Rights::SEND },          // 衰减：只给 SEND
        GrantItem { cptr: c_mem, mask: Rights::READ | Rights::WRITE },
    ];
    let out = install_initial_caps(&parent, &mut child, &items, &objects).unwrap();
    assert_eq!(child.get(out[0]).unwrap().rights, Rights::SEND);
    assert_eq!(child.get(out[1]).unwrap().rights, Rights::READ | Rights::WRITE);
    assert_eq!(child.get(out[0]).unwrap().obj, ep);
    // 父表零副作用（cap 仍在）
    assert!(parent.get(c_ep).is_ok());
    // slot 0 仍是 NULL trap
    assert!(child.get(0).is_err());

    // 缺 GRANT 的 cap 不可授予 → 整批失败，子表零副作用
    let c_nogrant = parent.alloc(Capability::root(ep, Rights::SEND)).unwrap();
    let before = child.remaining_free();
    let bad = [
        GrantItem { cptr: c_ep, mask: Rights::SEND },
        GrantItem { cptr: c_nogrant, mask: Rights::SEND },
    ];
    assert_eq!(install_initial_caps(&parent, &mut child, &bad, &objects), Err(CapError::Permission));
    assert_eq!(child.remaining_free(), before);

    // 超过上限 → InvalidCap
    let too_many = [GrantItem { cptr: c_ep, mask: Rights::SEND }; MAX_INITIAL_CAPS + 1];
    assert_eq!(
        install_initial_caps(&parent, &mut child, &too_many, &objects),
        Err(CapError::InvalidCap)
    );
}

#[test]
fn initial_caps_rejected_when_object_retired() {
    let mut objects = ObjectTable::new();
    let ep = objects.alloc(ObjKind::Endpoint).unwrap();
    let mut parent = synapse_cap::CapTable::new();
    let c = parent.alloc(Capability::root(ep, Rights::SEND | Rights::GRANT)).unwrap();
    let mut child = synapse_cap::CapTable::new();
    objects.begin_revoke(ep).unwrap(); // 对象进入 Revoking
    let items = [GrantItem { cptr: c, mask: Rights::SEND }];
    assert_eq!(
        install_initial_caps(&parent, &mut child, &items, &objects),
        Err(CapError::ObjectRetired)
    );
    assert_eq!(child.remaining_free(), 255); // 子表零副作用
}
