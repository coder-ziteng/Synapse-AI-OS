// fd_table: inline + overflow slab implementation (MVP)
//
// Strategy:
// - Per-process FdTable has an inline Vec<Option<FdEntry>> with INLINE_FDS entries
//   (initialized to None).
// - When inline is full and allocator is available, allocate an overflow Box<[Option<FdEntry>]> of
//   OVERFLOW_CHUNK entries and place extra descriptors there. This gives an easy upgrade path
//   from fixed-size inline to larger capacity without changing the external API.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use spin::Mutex;
use synapse_abi::{E_INVALID_CAP, E_NO_MEMORY};

use crate::elfload;

pub const MAX_PROCS: usize = 64; // slot count guidance (not enforced by structure here)
pub const INLINE_FDS: usize = 64; // per-process inline fd entries
pub const OVERFLOW_CHUNK: usize = 256; // when growing, allocate this many extra slots

#[derive(Clone)]
pub enum FdKind {
    Console,
    MemFile { ptr: *const u8, len: usize, pos: u64 },
}

#[derive(Clone)]
pub struct FdEntry {
    pub kind: FdKind,
}

struct FdTable {
    inline: Vec<Option<FdEntry>>,             // length = INLINE_FDS
    overflow: Option<Box<[Option<FdEntry>]>>, // optional overflow slab
}

impl FdTable {
    fn new() -> Self {
        let mut inline = Vec::with_capacity(INLINE_FDS);
        inline.resize(INLINE_FDS, None);
        Self { inline, overflow: None }
    }

    fn get(&self, fd: usize) -> Option<FdEntry> {
        if fd < self.inline.len() {
            return self.inline[fd].clone();
        }
        let off = fd - self.inline.len();
        match &self.overflow {
            Some(boxed) => {
                if off < boxed.len() { boxed[off].clone() } else { None }
            }
            None => None,
        }
    }

    fn set(&mut self, fd: usize, entry: Option<FdEntry>) -> Result<(), i64> {
        if fd < self.inline.len() {
            self.inline[fd] = entry;
            return Ok(());
        }
        let off = fd - self.inline.len();
        if let Some(boxed) = &mut self.overflow {
            if off < boxed.len() {
                boxed[off] = entry;
                return Ok(());
            } else {
                return Err(E_INVALID_CAP);
            }
        }
        Err(E_INVALID_CAP)
    }

    // find an empty slot; returns fd index
    fn alloc_slot_for(&mut self, entry: FdEntry) -> Result<usize, i64> {
        // search inline
        for (i, slot) in self.inline.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(entry);
                return Ok(i);
            }
        }
        // try overflow
        if let Some(boxed) = &mut self.overflow {
            for (i, slot) in boxed.iter_mut().enumerate() {
                if slot.is_none() {
                    *slot = Some(entry);
                    return Ok(self.inline.len() + i);
                }
            }
            // full
            return Err(E_NO_MEMORY);
        }
        // need to allocate overflow
        let mut v: Vec<Option<FdEntry>> = Vec::with_capacity(OVERFLOW_CHUNK);
        v.resize(OVERFLOW_CHUNK, None);
        let mut boxed = v.into_boxed_slice();
        // place first entry at offset 0
        boxed[0] = Some(entry);
        let fd_index = self.inline.len();
        self.overflow = Some(boxed);
        Ok(fd_index)
    }
}

// Global registry: pid -> FdTable
static FD_REGISTRY: Mutex<BTreeMap<u32, FdTable>> = Mutex::new(BTreeMap::new());
static PID_SEQ: AtomicU32 = AtomicU32::new(1);

pub fn alloc_pid() -> u32 {
    PID_SEQ.fetch_add(1, Ordering::SeqCst)
}

pub fn init_fd_table_for(pid: u32) {
    let mut reg = FD_REGISTRY.lock();
    let mut tbl = FdTable::new();
    // setup fd 0/1/2 -> Console
    tbl.inline[0] = Some(FdEntry { kind: FdKind::Console });
    tbl.inline[1] = Some(FdEntry { kind: FdKind::Console });
    tbl.inline[2] = Some(FdEntry { kind: FdKind::Console });
    reg.insert(pid, tbl);
}

pub fn get_fd_entry(pid: u32, fd: u32) -> Result<FdEntry, i64> {
    let reg = FD_REGISTRY.lock();
    let tbl = reg.get(&pid).ok_or(E_INVALID_CAP)?;
    let fd_usize = fd as usize;
    match tbl.get(fd_usize) {
        Some(e) => Ok(e),
        None => Err(E_INVALID_CAP),
    }
}

pub fn alloc_memfile(pid: u32, ptr: *const u8, len: usize) -> Result<u32, i64> {
    let mut reg = FD_REGISTRY.lock();
    let tbl = reg.get_mut(&pid).ok_or(E_INVALID_CAP)?;
    let entry = FdEntry { kind: FdKind::MemFile { ptr, len, pos: 0 } };
    match tbl.alloc_slot_for(entry) {
        Ok(idx) => Ok(idx as u32),
        Err(e) => Err(e),
    }
}

pub fn close_fd(pid: u32, fd: u32) -> Result<(), i64> {
    let mut reg = FD_REGISTRY.lock();
    let tbl = reg.get_mut(&pid).ok_or(E_INVALID_CAP)?;
    let idx = fd as usize;
    // verify in-bounds
    if idx < tbl.inline.len() {
        tbl.inline[idx] = None;
        return Ok(());
    }
    let off = idx - tbl.inline.len();
    match &mut tbl.overflow {
        Some(boxed) => {
            if off < boxed.len() {
                boxed[off] = None;
                return Ok(());
            }
            return Err(E_INVALID_CAP);
        }
        None => return Err(E_INVALID_CAP),
    }
}
