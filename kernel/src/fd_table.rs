// Minimal per-process file descriptor table for MVP
use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU32, Ordering};
use spin::Mutex;
use synapse_abi::E_INVALID_CAP;

use crate::elfload;

#[derive(Clone)]
pub enum FdKind {
    Console,
    MemFile { ptr: *const u8, len: usize, pos: u64 },
}

#[derive(Clone)]
pub struct FdEntry { pub kind: FdKind }

// pid -> fd vec
static FD_TABLE: Mutex<BTreeMap<u32, Vec<Option<FdEntry>>>> = Mutex::new(BTreeMap::new());
static PID_SEQ: AtomicU32 = AtomicU32::new(1);

pub fn alloc_pid() -> u32 {
    PID_SEQ.fetch_add(1, Ordering::SeqCst)
}

pub fn init_fd_table_for(pid: u32) {
    let mut map = FD_TABLE.lock();
    let mut v: Vec<Option<FdEntry>> = Vec::new();
    // fd0 stdin -> Console (dummy)
    v.push(Some(FdEntry{ kind: FdKind::Console }));
    // fd1 stdout
    v.push(Some(FdEntry{ kind: FdKind::Console }));
    // fd2 stderr
    v.push(Some(FdEntry{ kind: FdKind::Console }));
    map.insert(pid, v);
}

pub fn get_fd_entry(pid: u32, fd: u32) -> Result<FdEntry, i64> {
    let map = FD_TABLE.lock();
    match map.get(&pid) {
        Some(vec) => {
            let idx = fd as usize;
            if idx >= vec.len() { return Err(E_INVALID_CAP); }
            match &vec[idx] {
                Some(e) => Ok(e.clone()),
                None => Err(E_INVALID_CAP),
            }
        }
        None => Err(E_INVALID_CAP),
    }
}

pub fn alloc_memfile(pid: u32, ptr: *const u8, len: usize) -> Result<u32, i64> {
    let mut map = FD_TABLE.lock();
    let vec = map.get_mut(&pid).ok_or(E_INVALID_CAP)?;
    // find free slot
    for (i, slot) in vec.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(FdEntry{ kind: FdKind::MemFile{ ptr, len, pos: 0 } });
            return Ok(i as u32);
        }
    }
    // push new
    vec.push(Some(FdEntry{ kind: FdKind::MemFile{ ptr, len, pos: 0 } }));
    Ok((vec.len()-1) as u32)
}

pub fn close_fd(pid: u32, fd: u32) -> Result<(), i64> {
    let mut map = FD_TABLE.lock();
    let vec = map.get_mut(&pid).ok_or(E_INVALID_CAP)?;
    let idx = fd as usize;
    if idx >= vec.len() { return Err(E_INVALID_CAP); }
    vec[idx] = None;
    Ok(())
}
