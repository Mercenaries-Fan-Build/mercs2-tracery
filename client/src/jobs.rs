//! Job zones: every handler in the Pimp job system's two per-CPU handler tables
//! (`mercs2_formats::profiler_sites::JobTables`) is replaced by a trampoline that runs the original inside a
//! Tracy zone named by its slot (`job 07`), with the jobtype hash and the handler's address as the zone
//! text. Every job then shows on the thread that ran it: the main thread for the game jobs, the worker for
//! the render jobs. A slot whose jobtype is routed ([`crate::route`]) hands its job to the router instead,
//! which queues it for an extra worker; the worker runs it inside the same zone ([`ZONES`]).
//!
//! The table is data, so this is a pointer swap, not a code patch: the job loops call
//! `[cpu*300 + table + type*8](arg)` as `cdecl` with one argument, and a trampoline is a `cdecl` function of
//! one argument. Jobtypes register during start-up and level loads (`count` grows), so [`track`] runs every
//! frame and wraps the slots registered since. The original is stored before the table entry is written,
//! so a job loop reading the entry at any moment calls either the original or a trampoline that finds it.
//! A wrapped slot that later holds anything but its trampoline is fatal: something re-registered it, which
//! the registrars (`i = count++`) never do. So is a slot whose two CPU copies differ: routed jobs run the
//! CPU 0 copy wherever they run.

use mercs2_formats::profiler_sites::{CellsChangeRing, JOB_SLOTS, JobTables};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

const CPUS: usize = 2;
pub const SLOTS: usize = JOB_SLOTS as usize;
/// Jobtype `0xECAED415`: the per-cell distance test the port replaces. Its trampoline reads the change
/// ring counter around the original to attach the change count to its Tracy zone.
pub const CELLS_JOBTYPE: u32 = 0xECAED415;

/// The original handler of each slot of each CPU's table (0 until wrapped).
static ORIGINAL: [[AtomicUsize; SLOTS]; CPUS] = [const { [const { AtomicUsize::new(0) }; SLOTS] }; CPUS];
/// The zone text of each slot: jobtype hash and original handler.
static TEXT: [OnceLock<String>; SLOTS] = [const { OnceLock::new() }; SLOTS];
/// Whether each slot's jobtype is routed.
static ROUTED: [AtomicBool; SLOTS] = [const { AtomicBool::new(false) }; SLOTS];
/// The router, `fn(slot, handler, arg) -> bool` (`true`: the job was taken), or 0 before one is set.
static ROUTER: AtomicUsize = AtomicUsize::new(0);
/// How many slots are wrapped (the same in both tables).
static WRAPPED: AtomicU32 = AtomicU32::new(0);
/// The slot [`CELLS_JOBTYPE`] registered at (`u32::MAX` until seen). Reads of the change ring counter
/// before and after the original handler attach the diff to that slot's zone.
static CELLS_SLOT: AtomicU32 = AtomicU32::new(u32::MAX);
/// The change ring, set once from the resolved [`CellsChangeRing`] site. The low 32 bits are the counter's
/// address, the high 32 the ring's; 0 until set.
static CELLS_RING: AtomicU64 = AtomicU64::new(0);
/// A session-wide count of changes the port's zone saw, so the overlay can show it.
static CELLS_CHANGES_SEEN: AtomicU64 = AtomicU64::new(0);
/// A session-wide count of job 17 invocations observed.
static CELLS_INVOCATIONS: AtomicU64 = AtomicU64::new(0);
/// Session max of the ring counter observed when a job 17 invocation ended.
static CELLS_MAX_AFTER: AtomicU32 = AtomicU32::new(0);
/// Session max of the per-invocation change diff.
static CELLS_MAX_DIFF: AtomicU32 = AtomicU32::new(0);
/// Session count of invocations with a non-zero diff.
static CELLS_NONZERO_INVOCATIONS: AtomicU64 = AtomicU64::new(0);

pub fn set_cells_change_ring(ring: CellsChangeRing) {
    CELLS_RING.store((ring.counter as u64) | ((ring.ring as u64) << 32), Ordering::Release);
}

/// Session stats for the cells jobtype: invocations, total changes, non-zero invocations, max diff, max after.
pub fn cells_totals() -> (u64, u64, u64, u32, u32) {
    (
        CELLS_INVOCATIONS.load(Ordering::Relaxed),
        CELLS_CHANGES_SEEN.load(Ordering::Relaxed),
        CELLS_NONZERO_INVOCATIONS.load(Ordering::Relaxed),
        CELLS_MAX_DIFF.load(Ordering::Relaxed),
        CELLS_MAX_AFTER.load(Ordering::Relaxed),
    )
}

/// Read the change ring counter if the site is known.
pub fn read_cells_counter() -> Option<u32> {
    let packed = CELLS_RING.load(Ordering::Acquire);
    let counter = packed as u32;
    (counter != 0).then(|| unsafe { (counter as usize as *const u32).read_volatile() })
}

/// A monotonic max update that returns true when `v` was larger than any value seen before.
fn fetch_max(a: &AtomicU32, v: u32) -> bool {
    let mut old = a.load(Ordering::Relaxed);
    while v > old {
        match a.compare_exchange_weak(old, v, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(prev) => old = prev,
        }
    }
    false
}

pub type Router = fn(usize, usize, usize) -> bool;

thread_local! {
    static NAMED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

type Handler = unsafe extern "C" fn(usize);

/// Set the router routed slots hand their jobs to.
pub fn set_router(r: Router) {
    ROUTER.store(r as usize, Ordering::Release);
}

/// The zone text of `slot`, once it is wrapped.
pub fn text(slot: usize) -> Option<&'static str> {
    TEXT[slot].get().map(String::as_str)
}

/// The handler a routed job of `slot` runs: CPU 0's original.
pub fn original(slot: usize) -> usize {
    ORIGINAL[0][slot].load(Ordering::Acquire)
}

/// Run slot `slot` of CPU `cpu`'s table: route it, or run it inside its zone.
#[inline(always)]
unsafe fn run(cpu: usize, slot: usize, arg: usize) {
    let f = ORIGINAL[cpu][slot].load(Ordering::Acquire);
    if ROUTED[slot].load(Ordering::Relaxed) {
        let r = ROUTER.load(Ordering::Acquire);
        // SAFETY: only `set_router` stores a non-zero value, a `Router`.
        if r != 0 && unsafe { core::mem::transmute::<usize, Router>(r) }(slot, f, arg) {
            return;
        }
    }
    if !NAMED.get() {
        NAMED.set(true);
        if let Some(c) = tracy_client::Client::running() {
            c.set_thread_name(if cpu == 0 { "main thread (Pimp cpu 0)" } else { "Pimp worker (cpu 1)" });
        }
    }
    let zone = ZONES[slot]();
    if let Some(t) = text(slot) {
        zone.emit_text(t);
    }
    let is_cells = slot as u32 == CELLS_SLOT.load(Ordering::Relaxed);
    let counter_before = if is_cells { read_cells_counter() } else { None };
    // SAFETY: a trampoline is in the table only after its original was stored (see `track`).
    let f: Handler = unsafe { core::mem::transmute::<usize, Handler>(f) };
    unsafe { f(arg) };
    if let Some(before) = counter_before {
        if let Some(after) = read_cells_counter() {
            let changes = after.wrapping_sub(before);
            CELLS_INVOCATIONS.fetch_add(1, Ordering::Relaxed);
            CELLS_CHANGES_SEEN.fetch_add(u64::from(changes), Ordering::Relaxed);
            fetch_max(&CELLS_MAX_AFTER, after);
            if changes > 0 && changes <= 1_000_000 {
                CELLS_NONZERO_INVOCATIONS.fetch_add(1, Ordering::Relaxed);
                fetch_max(&CELLS_MAX_DIFF, changes);
            }
            zone.emit_text(&format!("before={before} after={after} +{changes}"));
        }
    }
    drop(zone);
}

macro_rules! per_slot {
    ($($slot:literal)*) => {
        /// The zone of each slot, opened by the trampolines and by the workers that run routed jobs.
        pub static ZONES: [fn() -> tracy_client::Span; SLOTS] = [$(|| tracy_client::span!(concat!("job ", stringify!($slot)))),*];
        /// The trampoline of each slot of each CPU's table.
        static TRAMPOLINES: [[Handler; SLOTS]; CPUS] = [
            [$({ unsafe extern "C" fn t(arg: usize) { unsafe { run(0, $slot, arg) } } t as Handler }),*],
            [$({ unsafe extern "C" fn t(arg: usize) { unsafe { run(1, $slot, arg) } } t as Handler }),*],
        ];
    };
}

per_slot!(0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35);

/// Wrap the slots registered since the last call, marking those whose jobtype is in `routed`, and check
/// the wrapped ones still hold their trampolines. Returns one log line per slot wrapped.
///
/// # Safety
/// `t` must be the running game's job tables.
pub unsafe fn track(t: &JobTables, routed: &[u32]) -> Result<Vec<String>, String> {
    let tables = t.handlers.map(|a| a as usize as *mut usize);
    unsafe { track_at(t.count as usize as *const u32, tables, routed) }
}

/// [`track`] over the count global at `count` and the handler tables at `tables` (`{handler, hash}` pairs of
/// pointer-sized words).
unsafe fn track_at(count: *const u32, tables: [*mut usize; CPUS], routed: &[u32]) -> Result<Vec<String>, String> {
    let entry = |cpu: usize, slot: usize| unsafe { tables[cpu].add(2 * slot) };
    let count = unsafe { count.read_volatile() };
    if count > JOB_SLOTS {
        return Err(format!("{count} jobtypes are registered; a handler table holds {JOB_SLOTS}"));
    }
    let done = WRAPPED.load(Ordering::Relaxed) as usize;
    for cpu in 0..CPUS {
        for slot in 0..done {
            let now = unsafe { entry(cpu, slot).read_volatile() };
            if now != TRAMPOLINES[cpu][slot] as usize {
                return Err(format!(
                    "cpu {cpu}'s job slot {slot} now holds {now:#x}, not its trampoline; the original was {:#x}",
                    ORIGINAL[cpu][slot].load(Ordering::Relaxed)
                ));
            }
        }
    }
    let mut lines = Vec::new();
    for slot in done..count as usize {
        let [(h0, hash0), (h1, hash1)] =
            [0, 1].map(|cpu| unsafe { (entry(cpu, slot).read_volatile(), entry(cpu, slot).add(1).read_volatile() as u32) });
        if h0 == 0 {
            return Err(format!("job slot {slot} (jobtype {hash0:#010x}) is registered with no handler"));
        }
        if (h0, hash0) != (h1, hash1) {
            return Err(format!(
                "job slot {slot} differs between the CPU tables: cpu 0 has jobtype {hash0:#010x} handler {h0:#x}, cpu 1 jobtype {hash1:#010x} handler {h1:#x}"
            ));
        }
        let route = routed.contains(&hash0);
        let _ = TEXT[slot].set(format!("jobtype {hash0:#010x} handler {h0:#x}"));
        ROUTED[slot].store(route, Ordering::Relaxed);
        if hash0 == CELLS_JOBTYPE {
            CELLS_SLOT.store(slot as u32, Ordering::Release);
        }
        for cpu in 0..CPUS {
            ORIGINAL[cpu][slot].store(h0, Ordering::Release);
            unsafe { entry(cpu, slot).write_volatile(TRAMPOLINES[cpu][slot] as usize) };
        }
        lines.push(format!("job {slot:2}: jobtype {hash0:#010x} handler {h0:#x}{}", if route { " (routed)" } else { "" }));
    }
    WRAPPED.store(count, Ordering::Relaxed);
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn handler_a(_: usize) {}
    unsafe extern "C" fn handler_b(_: usize) {}

    #[test]
    fn registered_slots_are_wrapped_once_and_an_overwrite_is_fatal() {
        let mut m = Box::new(([[0usize; 2 * SLOTS]; CPUS], 0u32));
        let track = |m: &mut Box<([[usize; 2 * SLOTS]; CPUS], u32)>| {
            let tables = [m.0[0].as_mut_ptr(), m.0[1].as_mut_ptr()];
            unsafe { track_at(&raw const m.1, tables, &[0x1234_5678]) }
        };
        let (a, b) = (handler_a as Handler as usize, handler_b as Handler as usize);
        for cpu in 0..CPUS {
            m.0[cpu][0] = a;
            m.0[cpu][1] = 0xec7a_a7f0;
            m.0[cpu][2] = b;
            m.0[cpu][3] = 0x1234_5678;
            m.0[cpu][4] = a;
            m.0[cpu][5] = 0x9999;
        }
        m.1 = 1;
        let lines = track(&mut m).unwrap();
        assert_eq!(lines, ["job  0: jobtype 0xec7aa7f0 handler ".to_string() + &format!("{a:#x}")]);
        assert_eq!(m.0[0][0], TRAMPOLINES[0][0] as usize);
        assert_eq!(m.0[1][0], TRAMPOLINES[1][0] as usize);
        assert_eq!(original(0), a);
        assert!(!ROUTED[0].load(Ordering::Relaxed));
        assert_eq!(m.0[0][2], b, "slot 1 is not registered yet");
        m.1 = 2;
        let lines = track(&mut m).unwrap();
        assert!(lines[0].ends_with("(routed)"), "{lines:?}");
        assert!(ROUTED[1].load(Ordering::Relaxed));
        assert_eq!(track(&mut m).unwrap().len(), 0, "nothing new");
        m.0[1][4] = b;
        m.1 = 3;
        assert!(track(&mut m).unwrap_err().contains("differs between the CPU tables"));
        m.0[1][4] = a;
        m.0[1][2] = a;
        assert!(track(&mut m).unwrap_err().contains("cpu 1's job slot 1 now holds"));
        m.1 = JOB_SLOTS + 1;
        assert!(track(&mut m).unwrap_err().contains("a handler table holds 36"));
    }
}
