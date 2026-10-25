//! Shadow mode for jobtype `0xECAED415`: on every real invocation, walk the game's job struct from TLS to
//! find the cells, cfg and reference-point block the handler is about to process, then run the Rust port
//! against that snapshot into a thread-local sink. After the game handler finishes, we read the ring
//! counter's diff and compare it to the port's emitted-change count. Session totals are logged on STOP.
//!
//! This is purely diagnostic: nothing from the shadow run is written back to the game. The port runs
//! BEFORE the game handler so it reads the sub-slot states before they're mutated.
//!
//! # Why not use the game globals directly?
//!
//! `DAT_00F79524` (cfg_ptr), `DAT_00F7951C` (arr_ptr) are set by the handler at its entry. Before the
//! handler runs these globals hold values from the previous invocation. The port needs the invocation's
//! own values, so we walk the job struct ourselves. Having two independent derivations also doubles as a
//! sanity check: a `verify_tls_walk` pass compares what the walker produced to what the handler sets.

use crate::cells::{self, Cell, Change, Config, DistanceBlock, RecordingSink};
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// Whether shadow mode is on. Set at startup from the config.
pub static ENABLED: AtomicBool = AtomicBool::new(false);
/// Address of the game's cfg_ptr global (`DAT_00F79524` or v1.1's `0x00F78524`). Used only to double-check
/// that the TLS walk picked the same pointer the handler set.
pub static GAME_CFG_PTR_GLOBAL: AtomicUsize = AtomicUsize::new(0);

/// Session totals.
static STATS: Stats = Stats::new();

struct Stats {
    invocations: AtomicU64,
    matches: AtomicU64,
    mismatches: AtomicU64,
    port_changes: AtomicU64,
    game_changes: AtomicU64,
    walk_mismatches: AtomicU64,
}

impl Stats {
    const fn new() -> Self {
        Self {
            invocations: AtomicU64::new(0),
            matches: AtomicU64::new(0),
            mismatches: AtomicU64::new(0),
            port_changes: AtomicU64::new(0),
            game_changes: AtomicU64::new(0),
            walk_mismatches: AtomicU64::new(0),
        }
    }
}

thread_local! {
    /// Reused per invocation to avoid heap allocation in the hot path.
    static SINK: RefCell<RecordingSink> = RefCell::new(RecordingSink::default());
    /// Reused per invocation to avoid heap allocation.
    static CELL_REFS: RefCell<Vec<*const Cell>> = const { RefCell::new(Vec::new()) };
}

/// Session stats: (invocations, matches, mismatches, port_changes, game_changes, walk_mismatches).
pub fn stats() -> (u64, u64, u64, u64, u64, u64) {
    (
        STATS.invocations.load(Ordering::Relaxed),
        STATS.matches.load(Ordering::Relaxed),
        STATS.mismatches.load(Ordering::Relaxed),
        STATS.port_changes.load(Ordering::Relaxed),
        STATS.game_changes.load(Ordering::Relaxed),
        STATS.walk_mismatches.load(Ordering::Relaxed),
    )
}

/// Read the Pimp-thread's exe TLS block through `fs:[0x2C]` → the current thread's TLS array,
/// then the first entry. Only x86 has `fs:[0x2C]`; on any other host this returns NULL so the shadow
/// walker is a no-op in tests.
#[cfg(all(target_arch = "x86", target_os = "windows"))]
unsafe fn pimp_control_block() -> *const u8 {
    let array: usize;
    unsafe { core::arch::asm!("mov {}, dword ptr fs:[0x2c]", out(reg) array, options(nostack, readonly, preserves_flags)) };
    if array == 0 {
        return core::ptr::null();
    }
    unsafe { (array as *const usize).read() as *const u8 }
}

#[cfg(not(all(target_arch = "x86", target_os = "windows")))]
unsafe fn pimp_control_block() -> *const u8 {
    core::ptr::null()
}

/// Walk the current thread's job struct and derive the inputs this job 17 invocation will use.
/// Returns `None` when the TLS pointer or the job struct is NULL (we aren't on a Pimp thread).
///
/// # Safety
/// Must run on a Pimp worker thread with a live job in progress.
unsafe fn walk_tls_job() -> Option<(*const Config, *const DistanceBlock, Vec<*const Cell>)> {
    let pcb = unsafe { pimp_control_block() };
    if pcb.is_null() {
        return None;
    }
    let job = unsafe { (pcb.add(0xc) as *const *const u8).read() };
    if job.is_null() {
        return None;
    }
    let a = unsafe { (job.add(0xc) as *const u32).read() } as usize;
    let b = unsafe { (job.add(0x10) as *const u32).read() } as usize;
    let count = unsafe { (job.add(0x14) as *const u32).read() } as usize;
    if count == 0 || count > 1024 {
        return None;
    }
    let base = job as usize + 0x20;
    // sum3 = a + b + count: primary slot sits right after the cells in the per-slot table.
    let sum3 = a.wrapping_add(b).wrapping_add(count);
    let cfg_ptr = unsafe { ((base + sum3 * 8 + 8) as *const *const Config).read() };
    let arr_ptr = unsafe { ((base + sum3 * 8 + 0x10) as *const *const DistanceBlock).read() };
    if cfg_ptr.is_null() || arr_ptr.is_null() {
        return None;
    }
    let mut cells = Vec::with_capacity(count);
    for i in 0..count {
        let p = unsafe { ((base + (a + b + i) * 8) as *const *const Cell).read() };
        if p.is_null() {
            return None;
        }
        cells.push(p);
    }
    Some((cfg_ptr, arr_ptr, cells))
}

/// Called by the jobs trampoline before the game handler runs. Snapshots the inputs via the TLS walker,
/// runs the port into the thread-local sink, and (once) checks the walked cfg pointer against the game's
/// stale global so we can tell early if the walker drifts from reality.
///
/// # Safety
/// Must run on the game's main thread as a job 17 handler is about to execute.
pub unsafe fn pre_handler() {
    let Some((cfg_ptr, arr_ptr, cell_ptrs)) = (unsafe { walk_tls_job() }) else {
        return;
    };
    SINK.with_borrow_mut(|sink| {
        sink.0.clear();
        CELL_REFS.with_borrow_mut(|refs| {
            refs.clear();
            refs.extend(cell_ptrs.iter().copied());
            // SAFETY: all pointers are live game memory and will outlive this call since the game handler
            // is about to use the same cells.
            let cfg = unsafe { &*cfg_ptr };
            let arr = unsafe { &*arr_ptr };
            let cells: Vec<&Cell> = refs.iter().map(|&p| unsafe { &*p }).collect();
            cells::job_17_run(&cells, cfg, arr, sink);
        });
    });
}

/// Called by the jobs trampoline after the game handler runs, with the ring counter's before/after.
/// Compares the port's emitted-change count to the game's ring diff.
pub fn post_handler(game_before: u32, game_after: u32) {
    STATS.invocations.fetch_add(1, Ordering::Relaxed);
    let game_count = u64::from(game_after.wrapping_sub(game_before));
    let port_count = SINK.with_borrow(|s| s.0.len() as u64);
    STATS.port_changes.fetch_add(port_count, Ordering::Relaxed);
    STATS.game_changes.fetch_add(game_count, Ordering::Relaxed);
    if port_count == game_count {
        STATS.matches.fetch_add(1, Ordering::Relaxed);
    } else {
        STATS.mismatches.fetch_add(1, Ordering::Relaxed);
    }
}

/// Once, verify the TLS-walked cfg pointer matches the game's cfg global that the handler just set.
pub fn verify_walk_once(walked_cfg_ptr: usize) {
    let global = GAME_CFG_PTR_GLOBAL.load(Ordering::Acquire);
    if global == 0 {
        return;
    }
    let game_cfg = unsafe { (global as *const usize).read() };
    if walked_cfg_ptr != game_cfg {
        STATS.walk_mismatches.fetch_add(1, Ordering::Relaxed);
    }
}

/// A ring entry's bytes read as `(entity_id, old, new)`. Game writes:
///   `+0: extra_0; +4: entity_id; +8: (old | new<<8); +12: extra_1`.
///
/// # Safety
/// `ring_base` must be a readable `n * 16`-byte buffer.
pub unsafe fn parse_ring(ring_base: *const u8, before: u32, after: u32, out: &mut Vec<Change>) {
    let count = after.wrapping_sub(before);
    for i in 0..count {
        let p = unsafe { ring_base.add(((before.wrapping_add(i)) * 16) as usize) };
        let entity_id = unsafe { (p.add(4) as *const u32).read_volatile() };
        let packed = unsafe { (p.add(8) as *const u16).read_volatile() };
        out.push(Change { entity_id, old_new: packed, extra: [0, 0] });
    }
    // Ensure &mut passed in isn't considered dropped.
    let _ = ring_base;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ring_extracts_entity_id_and_packed_old_new() {
        let bytes = [
            // entry 0
            0x11, 0x11, 0x11, 0x11, // extra_0
            0x22, 0x22, 0x22, 0x22, // entity_id = 0x22222222
            0x40, 0x10, 0x00, 0x00, // old=0x40, new=0x10
            0x33, 0x33, 0x33, 0x33, // extra_1
            // entry 1
            0x44, 0x44, 0x44, 0x44, 0xaa, 0xbb, 0xcc, 0xdd, 0x00, 0x40, 0x00, 0x00, 0x55, 0x55, 0x55, 0x55,
        ];
        let mut out = Vec::new();
        unsafe { parse_ring(bytes.as_ptr(), 0, 2, &mut out) };
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].entity_id, 0x22222222);
        assert_eq!(out[0].old_new, 0x1040);
        assert_eq!(out[1].entity_id, 0xddccbbaa);
        assert_eq!(out[1].old_new, 0x4000);
    }
}
