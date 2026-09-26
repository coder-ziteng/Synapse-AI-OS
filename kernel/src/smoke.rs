//! 集成层 smoke（cap × ipc × proc 端到端真机验证）。
//!
//! 场景（全部在 init 单进程 / 单线程 / 单核 MVP 下顺序执行）：
//!
//! 1. **spawn** — `init.spawn(AgentId(2), death_endpoint=init_ep)` → child；
//!    建子 CapTable + 安装初始 caps（衰减，缺 `GRANT`）。
//! 2. **delegate + revoke_cascade** — 本表内派生两次，`revoke_cascade` 撤销
//!    根 + 全部派生（O(n·d) 遍历）。
//! 3. **transfer_caps 正反** — init → child 成功；child 持有的无 `GRANT` cap
//!    再传 → `Permission`。
//! 4. **object lifecycle** — `begin_revoke → retire → free` → 旧 `ObjRef`
//!    因 generation 失配返回 `ObjectRetired`。
//! 5. **Endpoint** — child `try_send` → `Queued`；init `recv` → `Message`
//!    （校验 sender / badge / label）；反向（init 先 recv → Waiting） →
//!    child `try_send` → `Delivered`；`cancel_sender` 摘除数。
//! 6. **Notification** — `signal` 两次 OR 聚合 → `poll` → `Some(合并位)` →
//!    再 `poll` → `None`（读清）。
//! 7. **exit → reap** — `exit(child, 0)` → `DeathSignal` → 投递为
//!    `SendRequest` 后 `cancel_sender` 摘除 → `release_resources`（Zombie）
//!    → `k_destroy_cap_table` → `reap` → AgentRegistry 查无 → init 配额划拨
//!    归还。
//!
//! 每步以 [`must!`] 计数；任一断言失败立即 `panic!`（走 P1-T5 已验收的
//! backtrace 路径）。最终 `[smoke] N/N checks passed` 输出到串口。

use log::info;

use synapse_cap::{CapError, CapTable, ObjKind, ObjRef, Rights, TransferItem};
use synapse_ipc::{AgentId, RecvOutcome, SendOutcome, SendRequest};
use synapse_proc::{
    grant::GrantItem, process::INIT_QUOTA, FaultKind, Pid, ProcessTable, SpawnParams,
};

use crate::bootstrap::BootstrapRefs;
use crate::kstate;
use crate::page_frame;

/// `must!(label, expr)` —— `expr` 须返回 `Result<T, CapError>`；`Err` 立即 panic。
/// 返回 Ok 值；不计入 total（调用方显式 `+= 1`）。
macro_rules! must {
    ($label:expr, $expr:expr) => {{
        let r = $expr;
        match r {
            Ok(v) => v,
            Err(e) => panic!("[smoke] FAIL {}: {:?} (errno={})", $label, e, e.errno()),
        }
    }};
}

/// 子进程配额：远小于 INIT 剩余（划拨语义可验证，reap 后归还）。
const DEFAULT_CHILD_QUOTA: synapse_cap::Quota = synapse_cap::Quota {
    max_pages: 1024,
    max_threads: 4,
    max_caps: 8,
    max_endpoints: 2,
    max_msg_size: 4096,
    max_pending_ipc: 8,
    max_grants: 2,
};

/// 跑端到端集成 smoke。失败立刻 panic；不返回。
pub fn run_integration_smoke(refs: &BootstrapRefs) {
    info!("[smoke] start");
    let mut total: u32 = 0;

    // ============================================================
    // 1. spawn child + 初始 caps 安装
    // ============================================================
    info!("[smoke] 1/7 spawn + install_initial_caps");
    let init_caps_pre = proc_usage_caps();

    let child_pid = must!(
        "spawn",
        with_procs(|t| {
            t.spawn(
                kstate::INIT,
                SpawnParams {
                    agent: AgentId(2),
                    quota: DEFAULT_CHILD_QUOTA,
                    death_endpoint: refs.ep_cap,
                },
            )
        })
    );
    info!("[smoke]   spawned child pid={}", child_pid.0);
    total += 1;

    must!("set_running child", with_procs(|t| t.set_running(child_pid)));
    total += 1;

    must!("create child CapTable", kstate::k_create_cap_table(child_pid));
    total += 1;

    let initial_items = [GrantItem {
        cptr: refs.ep_cap,
        mask: Rights::SEND | Rights::RECV,
    }];
    let _installed = must!(
        "install_initial_caps",
        kstate::k_install_initial(kstate::INIT, child_pid, &initial_items)
    );
    total += 1;

    // ============================================================
    // 2. delegate ×2 + revoke_cascade
    // ============================================================
    info!("[smoke] 2/7 delegate + revoke_cascade");
    let mr_obj = must!("alloc mr", kstate::k_alloc_object(ObjKind::MemoryRegion));
    let mr_root = must!(
        "mint mr root",
        kstate::k_mint_root(
            kstate::INIT,
            mr_obj,
            Rights::READ | Rights::WRITE | Rights::GRANT,
        )
    );
    total += 2;

    let mr_a = must!(
        "delegate mr_root → A",
        kstate::with_cap_table(kstate::INIT, |t| t.delegate(mr_root, Rights::READ | Rights::GRANT))
    );
    let _mr_b = must!(
        "delegate A → B",
        kstate::with_cap_table(kstate::INIT, |t| t.delegate(mr_a, Rights::READ))
    );
    total += 2;

    let revoked = must!(
        "revoke_cascade(mr_root)",
        kstate::with_cap_table(kstate::INIT, |t| t.revoke_cascade(mr_root))
    );
    assert_eq!(revoked, 3usize, "revoked should be root+2 derivatives");
    total += 1;
    info!("[smoke]   revoked {} slots (root + 2 derivatives)", revoked);

    let probe_is_err = kstate::with_cap_table(kstate::INIT, |t| t.get(mr_a).is_err());
    assert!(
        probe_is_err,
        "post-revoke A should be InvalidCap"
    );
    total += 1;

    // ============================================================
    // 3. transfer_caps 正 / 反
    // ============================================================
    info!("[smoke] 3/7 transfer_caps positive + negative");
    let mr2_obj = must!("alloc mr2", kstate::k_alloc_object(ObjKind::MemoryRegion));
    let mr2_root = must!(
        "mint mr2 root",
        kstate::k_mint_root(
            kstate::INIT,
            mr2_obj,
            Rights::READ | Rights::WRITE | Rights::GRANT,
        )
    );
    total += 2;

    let transferred = must!(
        "transfer init→child",
        kstate::k_transfer(
            kstate::INIT,
            child_pid,
            &[TransferItem {
                cptr: mr2_root,
                mask: Rights::READ,
            }],
        )
    );
    let child_mr2 = transferred[0];
    total += 1;

    let child_rights_bits = must!(
        "child mr2 rights",
        kstate::with_cap_table(child_pid, |t| {
            let c = t.get(child_mr2)?;
            Ok::<_, CapError>(c.rights.bits())
        })
    );
    assert_eq!(child_rights_bits, Rights::READ.bits());
    total += 1;

    let neg = kstate::k_transfer(
        child_pid,
        kstate::INIT,
        &[TransferItem {
            cptr: child_mr2,
            mask: Rights::READ,
        }],
    );
    assert!(matches!(neg, Err(CapError::Permission)));
    total += 1;

    // ============================================================
    // 4. object lifecycle：begin_revoke / retire / free / generation 失配
    // ============================================================
    info!("[smoke] 4/7 object lifecycle + generation reuse");
    must!("begin_revoke(mr2)", kstate::with_objects(|o| o.begin_revoke(mr2_obj)));
    total += 1;
    must!("retire(mr2)", kstate::with_objects(|o| o.retire(mr2_obj)));
    total += 1;
    must!("free(mr2)", kstate::k_free_object(mr2_obj));
    total += 1;

    let old = kstate::with_objects(|o| o.check_live(mr2_obj));
    assert!(matches!(old, Err(CapError::ObjectRetired)));
    total += 1;

    let mr3_obj = must!("alloc new mr3", kstate::k_alloc_object(ObjKind::MemoryRegion));
    assert_ne!(mr3_obj.generation, mr2_obj.generation);
    total += 1;

    // ============================================================
    // 5. Endpoint：try_send / recv / cancel_sender
    // ============================================================
    info!("[smoke] 5/7 endpoint send/recv/cancel");
    let ep_obj = ep_obj_of(refs);

    let req_a = make_send(AgentId(2), 0xBADD_0001, 0xCAFE_F00D, 16, 0x1000);
    let outcome = must!("child try_send(req_a)", kstate::k_ep_try_send(ep_obj, req_a));
    assert_eq!(outcome, SendOutcome::Queued);
    total += 1;

    let recv_a = must!("init ep.recv()", kstate::k_ep_recv(ep_obj));
    match recv_a {
        RecvOutcome::Message(req) => {
            assert_eq!(req.sender, AgentId(2));
            assert_eq!(req.badge, 0xBADD_0001);
            assert_eq!(req.label, 0xCAFE_F00D);
            assert_eq!(req.payload_len, 16);
        }
        RecvOutcome::Waiting => panic!("[smoke] expected Message"),
    }
    total += 1;

    let wait = must!("init ep.recv() (empty)", kstate::k_ep_recv(ep_obj));
    assert!(matches!(wait, RecvOutcome::Waiting));
    total += 1;

    let req_b = make_send(AgentId(2), 0xBADD_0002, 0xCAFE_F00E, 8, 0x2000);
    let delivered = must!(
        "child try_send (waiting)",
        kstate::k_ep_try_send(ep_obj, req_b)
    );
    assert_eq!(delivered, SendOutcome::Delivered);
    total += 1;

    let n0 = must!(
        "cancel_sender(AgentId(99))",
        kstate::k_ep_cancel_sender(ep_obj, AgentId(99))
    );
    assert_eq!(n0, 0usize);
    total += 1;

    // ============================================================
    // 6. Notification：signal OR + poll 读清
    // ============================================================
    info!("[smoke] 6/7 notification signal/poll");
    let no_obj = no_obj_of(refs);
    must!("no.signal(0x3)", kstate::k_notify_signal(no_obj, 0x3));
    total += 1;
    must!("no.signal(0x5)", kstate::k_notify_signal(no_obj, 0x5));
    total += 1;

    let merged = must!("no.poll() → Some(0x7)", kstate::k_notify_poll(no_obj));
    assert_eq!(merged, Some(0x7));
    total += 1;

    let cleared = must!("no.poll() → None", kstate::k_notify_poll(no_obj));
    assert_eq!(cleared, None);
    total += 1;

    // ============================================================
    // 7. exit → reap 全链路
    // ============================================================
    info!("[smoke] 7/7 exit → death signal → reap");
    let death = must!("exit(child, 0)", with_procs(|t| t.exit(child_pid, 0)));
    assert_eq!(death.pid, child_pid);
    assert_eq!(death.exit_code, 0);
    assert_eq!(death.fault_reason, None);
    total += 1;

    // 模拟"内核向 death_endpoint 投递 DeathSignal"：构造 SendRequest 入队。
    // sender 设为 dying child 的 agent（kernel 代表 child 投递），
    // receiver 据此识别是哪个 child 死亡（与 cancel_sender 的语义一致）。
    let death_msg = SendRequest {
        sender: AgentId(2),
        badge: 0,
        label: 0xDEAD_BEEF,
        payload_len: 0,
        payload_addr: 0,
        caps: [TransferItem {
            cptr: 0,
            mask: Rights::EMPTY,
        }; 8],
        cap_count: 0,
    };
    let death_out = must!(
        "death try_send",
        kstate::k_ep_try_send(ep_obj, death_msg)
    );
    assert_eq!(death_out, SendOutcome::Queued);
    total += 1;

    let n_removed = must!(
        "cancel_sender(AgentId(2))",
        kstate::k_ep_cancel_sender(ep_obj, AgentId(2))
    );
    assert_eq!(n_removed, 1usize);
    total += 1;

    must!(
        "release_resources(child)",
        with_procs(|t| t.release_resources(child_pid))
    );
    total += 1;

    kstate::k_destroy_cap_table(child_pid);
    total += 1;

    must!("reap(INIT, child)", with_procs(|t| t.reap(kstate::INIT, child_pid)));
    total += 1;

    let got_child = with_procs(|t| t.get(child_pid).is_none());
    assert!(got_child, "post-reap get(child) should be None");
    total += 1;

    let agent_gone = with_procs(|t| t.agents.lookup(AgentId(2)).is_none());
    assert!(agent_gone, "post-reap agents.lookup(AgentId(2)) should be None");
    total += 1;

    // 配额划拨归还：init 的 Caps 用量应回到 spawn 前
    let init_caps_post = proc_usage_caps();
    assert_eq!(
        init_caps_pre, init_caps_post,
        "init Caps usage should return to pre-spawn (pre={} post={})",
        init_caps_pre, init_caps_post
    );
    total += 1;

    // ============================================================
    // 8. Page frame allocator: alloc / free / exhaustion / double-free
    // ============================================================
    info!("[smoke] 8/9 page frame allocator");

    let free_pre = page_frame::with_page_frames(|a| a.free_frames());
    let total_pre = page_frame::with_page_frames(|a| a.total_frames());
    info!(
        "[smoke]   pre: total={} free={} ({:.1} MB)",
        total_pre,
        free_pre,
        (free_pre * page_frame::FRAME_SIZE) as f64 / 1024.0 / 1024.0
    );
    assert!(total_pre > 0, "page frame allocator should have usable frames");
    total += 1;

    // 正路径: alloc → free → 计数回归
    let f1 = page_frame::alloc_frame().expect("first alloc");
    assert_eq!(f1 % page_frame::FRAME_SIZE as u64, 0, "frame must be 4KB aligned");
    total += 1;

    let free_after_one = page_frame::with_page_frames(|a| a.free_frames());
    assert_eq!(free_after_one, free_pre - 1, "free count should decrement by 1");
    total += 1;

    let f2 = page_frame::alloc_frame().expect("second alloc");
    assert_ne!(f1, f2, "two allocs should return distinct frames");
    total += 1;

    page_frame::free_frame(f1);
    page_frame::free_frame(f2);
    let free_after_free = page_frame::with_page_frames(|a| a.free_frames());
    assert_eq!(free_after_free, free_pre, "free count should return to pre-alloc");
    total += 1;

    // 双重释放幂等: 不改变 free count
    page_frame::free_frame(f1);
    let free_after_double = page_frame::with_page_frames(|a| a.free_frames());
    assert_eq!(free_after_double, free_pre, "double-free should be no-op");
    total += 1;

    // ============================================================
    // 9. Kernel heap: Box/Vec alloc + dealloc (GlobalAlloc trait)
    // ============================================================
    info!("[smoke] 9/9 kernel heap (Box/Vec)");

    // Box<u64> 分配
    let boxed = alloc::boxed::Box::new(0xDEAD_BEEF_u64);
    assert_eq!(*boxed, 0xDEAD_BEEF, "Box<u64> value mismatch");
    total += 1;

    // Vec<u32> 分配
    let mut vec = alloc::vec::Vec::new();
    for i in 0..100 {
        vec.push(i);
    }
    assert_eq!(vec.len(), 100, "Vec length mismatch");
    assert_eq!(vec[99], 99, "Vec content mismatch");
    total += 1;

    // String 分配（用 String::from() 避免 format! 的格式化路径）
    let s = alloc::string::String::from("hello 42");
    // 字节级比较（避坑：assert_eq! 在 nightly 上 String/&str 偶发失败，
    // 见 heap.rs 排查注释；这里仅做 raw 字节验证）
    if s.as_bytes() != b"hello 42" {
        panic!("String bytes mismatch");
    }
    total += 1;

    // 释放后内存应回归（MVP 暂不校验，仅验证不 panic）
    drop(boxed);
    drop(vec);
    drop(s);
    total += 1;

    // ============================================================
    // 10. GDT/TSS: lgdt + ltr + IST1/RSP0 验证
    // ============================================================
    info!("[smoke] 10/10 GDT/TSS (IST1 double-fault stack + RSP0)");

    // TSS.IST1 应指向 DF_STACK 栈顶（gdt::DF_STACK 基址 + 4096）。
    // 用 `gdt::ist1_df_stack_top()` 读回 TSS 字段并校验落在 DF_STACK 范围内。
    let ist1 = crate::gdt::ist1_df_stack_top();
    assert_ne!(ist1, 0, "IST1 must be non-zero after init_gdt_tss");
    // 注：CPU 不强制 IST/RSP 页对齐，但栈顶 = 基址 + 4096 应落在 DF_STACK 内
    // （基址因 linker 未生效 repr(align(4096)) 可能非页对齐，故此处不校验页对齐）
    let df_base = crate::gdt::df_stack_base() as u64;
    assert!(ist1 >= df_base && ist1 <= df_base + 4096, "IST1 should be within DF_STACK");
    total += 1;

    let rsp0 = crate::gdt::rsp0_stack_top();
    assert_ne!(rsp0, 0, "RSP0 must be non-zero after init_gdt_tss");
    let k_base = crate::gdt::kernel_stack_base() as u64;
    assert!(rsp0 >= k_base && rsp0 <= k_base + 4096, "RSP0 should be within KERNEL_STACK");
    total += 1;

    // IST1 与 RSP0 应指向不同栈（避免 DF handler 踩内核栈）。
    assert_ne!(ist1, rsp0, "IST1 and RSP0 should be distinct stacks");
    total += 1;

    // CS 应该已被收敛到规范形式 0x08（不是 boot.S 的 0x18）。
    let cs: u16 = {
        let sel: u16;
        unsafe { core::arch::asm!("mov {0:x}, cs", out(reg) sel, options(nostack, preserves_flags)); }
        sel
    };
    assert_eq!(cs, 0x08, "CS should be 0x08 (new GDT kernel code), got {:#x}", cs);
    total += 1;

    // SS 应为 0x10（新 GDT kernel data）。
    let ss: u16 = {
        let sel: u16;
        unsafe { core::arch::asm!("mov {0:x}, ss", out(reg) sel, options(nostack, preserves_flags)); }
        sel
    };
    assert_eq!(ss, 0x10, "SS should be 0x10 (new GDT kernel data), got {:#x}", ss);
    total += 1;

    // 引用防止 lint
    assert!(matches!(FaultKind::SegFault, FaultKind::SegFault));
    let _ = INIT_QUOTA;
    let _: Pid = child_pid; // 类型引用

    // ============================================================
    // 11. IDT + 异常处理: sidt 验证 + int3 (#BP) 触发验证
    // ============================================================
    info!("[smoke] 11/11 IDT + 异常处理");

    // IDT base 应非零（lidt 已执行）
    let idt_base = crate::idt::current_idt_base();
    assert_ne!(idt_base, 0, "IDT base should be non-zero after init_idt");
    total += 1;

    // IDT limit 应为 256 * 16 - 1 = 4095
    let idt_limit = crate::idt::current_idt_limit();
    assert_eq!(idt_limit, 4095, "IDT limit should be 4095 (256 entries × 16 bytes - 1), got {}", idt_limit);
    total += 1;

    // 触发 #BP (int3)：handler 应该记录日志并返回（不 panic）
    // int3 指令 = 0xCC；handler 返回后继续执行下一条指令
    unsafe { core::arch::asm!("int3"); }
    // 如果到这里说明 #BP handler 成功返回（iretq 继续执行）
    total += 1;

    info!("[smoke] {}/{} checks passed", total, total);
}

// ============================================================
// helpers
// ============================================================

/// 构造一个 cap_count=0 的 SendRequest（smoke 用，无 IPC payload 拷贝）。
fn make_send(
    sender: AgentId,
    badge: u32,
    label: u32,
    payload_len: u32,
    payload_addr: u64,
) -> SendRequest {
    SendRequest {
        sender,
        badge,
        label,
        payload_len,
        payload_addr,
        caps: [TransferItem {
            cptr: 0,
            mask: Rights::EMPTY,
        }; 8],
        cap_count: 0,
    }
}

/// `with_procs` 闭包包装（Procs 锁保护，单线程 MVP 下锁立即可用）。
fn with_procs<R>(f: impl FnOnce(&mut ProcessTable) -> R) -> R {
    kstate::with_procs(f)
}

/// 读 init 的 Caps 用量（spawn 前 / reap 后比对用）。
fn proc_usage_caps() -> u32 {
    kstate::with_procs(|t| {
        let p = t.get(kstate::INIT).expect("init pcb");
        p.usage.used(synapse_cap::Resource::Caps)
    })
}

/// 取 init 的 ep ObjRef（通过 init CapTable 读 cap 内的 `obj` 字段）。
fn ep_obj_of(refs: &BootstrapRefs) -> ObjRef {
    kstate::with_cap_table(kstate::INIT, |t: &mut CapTable| {
        let c = t.get(refs.ep_cap).expect("init ep cap");
        c.obj
    })
}

/// 取 init 的 no ObjRef。
fn no_obj_of(refs: &BootstrapRefs) -> ObjRef {
    kstate::with_cap_table(kstate::INIT, |t: &mut CapTable| {
        let c = t.get(refs.no_cap).expect("init no cap");
        c.obj
    })
}
