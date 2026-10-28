//! Shadow mode for jobtype `0xECAED415`: on every real invocation, walk the game's job struct from TLS to
//! find the cells, cfg and reference-point block the handler is about to process, then run the Rust port
//! against that snapshot into a thread-local sink. After the game handler finishes, we read the ring
//! counter's diff and compare it to the port's emitted-change count. Session totals are logged on STOP.
//!
//! This is purely diagnostic: nothing from the shadow run is written back to the game. The port runs
//! BEFORE the game handler so it reads the sub-slot states before they're mutated. With shadow mode off
//! the game handler does not run at all: the port is the handler ([`crate::live`]).
//!
//! # Why not use the game globals directly?
//!
//! `DAT_00F79524` (cfg_ptr), `DAT_00F7951C` (arr_ptr) are set by the handler at its entry. Before the
//! handler runs these globals hold values from the previous invocation. The port needs the invocation's
//! own values, so we walk the job struct ourselves. Having two independent derivations also doubles as a
//! sanity check: a `verify_tls_walk` pass compares what the walker produced to what the handler sets.

use crate::cells::{self, Cell, Change, Config, DistanceBlock, RecordingSink};
use std::cell::RefCell;
use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// The v1.1 globals the dispatcher `FUN_00735E80` publishes for the body — `_DAT_00F78524` (cfg
/// pointer), `_DAT_00F7851C` (point-block pointer), `_DAT_00F78520` (`f32` cell size) — read back
/// after the live run to check them against the walk. Armed only when the exe's sha256 is exactly
/// `v11-patched-securom`.
///
/// This block also carried a "game-on-copy" run for three sessions (`63ce1fec…`–`f591105c…`): the
/// game's own per-cell body called on byte copies of the cells before the live run. It agreed with
/// the port and disagreed with the live run in all 94 mismatches it saw, which is what it was for;
/// it was then removed, because two of the four runs that carried it crashed on the main thread
/// and none of the four before it had.
pub struct GameCopy {
    cfg_global: usize,
    arr_global: usize,
    size_global: usize,
}

static GAME_COPY: OnceLock<Option<GameCopy>> = OnceLock::new();

/// sha256 of `Mercenaries2.exe` for `v11-patched-securom` (`builds.tsv`).
const V11_PATCHED_SECUROM: [u8; 32] = [
    0x7a, 0x34, 0x88, 0x47, 0xe1, 0x03, 0xd7, 0x1e, 0x8c, 0x17, 0xe7, 0xa5, 0x1a, 0x0f, 0x3b, 0x4d, 0x44, 0x22, 0xe0, 0xc9, 0xcb, 0x46, 0xec,
    0x6a, 0xcc, 0x9f, 0xe5, 0xe4, 0xe6, 0xbe, 0x36, 0xb5,
];

/// Arm the dispatcher-globals check for this exe, or record why not. Returns the line to log.
pub fn arm_game_copy(exe_sha256: &[u8; 32]) -> String {
    let armed = (*exe_sha256 == V11_PATCHED_SECUROM).then_some(GameCopy { cfg_global: 0x00f7_8524, arr_global: 0x00f7_851c, size_global: 0x00f7_8520 });
    let line = match &armed {
        Some(g) => format!("[*] mercs2_profiler: cells dispatcher globals armed: {:#x} {:#x} {:#x}", g.cfg_global, g.arr_global, g.size_global),
        None => String::from("[*] mercs2_profiler: cells dispatcher globals not armed: the exe is not v11-patched-securom"),
    };
    let _ = GAME_COPY.set(armed);
    line
}

/// The x87 status and control words: `(status, control)`. `fnstsw`/`fnstcw` have no side effects
/// (unlike `fnstenv`, which masks exceptions). Status bits 11..13 are TOP — 0 on an empty stack —
/// and bit 6 is the stack-fault flag; the control word carries precision (bits 8..9) and rounding
/// (10..11). The x87 stack is per thread and persists across calls, and every compare in
/// `FUN_00735840` sends a NaN down its "outside"/"no band" branch, so a stack left non-empty by
/// earlier code on this thread makes the handler see every point as far away.
#[cfg(all(target_arch = "x86", target_os = "windows"))]
fn x87_words() -> (u16, u16) {
    let status: u16;
    let mut control: u16 = 0;
    unsafe { core::arch::asm!("fnstsw ax", "fnstcw word ptr [{c}]", out("ax") status, c = in(reg) &mut control, options(nostack)) };
    (status, control)
}

#[cfg(not(all(target_arch = "x86", target_os = "windows")))]
fn x87_words() -> (u16, u16) {
    (0, 0)
}

fn x87_text(w: (u16, u16)) -> String {
    format!("sw {:#06x} (TOP {} SF {}) cw {:#06x}", w.0, (w.0 >> 11) & 7, (w.0 >> 6) & 1, w.1)
}

/// The x87 status word alone, for bracketing a call.
pub fn x87_status() -> u16 {
    x87_words().0
}

/// How many job 17 invocations changed TOP by each amount (mod 8) across the game handler alone,
/// sampled by the trampoline immediately around the call. Index = `(top_after - top_before) & 7`.
static TOP_DELTA: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];

thread_local! {
    /// The status words the trampoline sampled around the last game handler call, and the entry tag.
    static LIVE_SW: std::cell::Cell<(u16, u16, u8)> = const { std::cell::Cell::new((0, 0, 0)) };
}

/// Called by the trampoline with the status words from just before and just after the game handler
/// and the abridged tag byte at entry.
pub fn note_live_x87(before: u16, after: u16, entry_tag: u8) {
    LIVE_SW.set((before, after, entry_tag));
    let d = ((after >> 11).wrapping_sub(before >> 11)) & 7;
    TOP_DELTA[usize::from(d)].fetch_add(1, Ordering::Relaxed);
}

/// One line: the TOP-delta histogram over the session.
pub fn x87_report() -> String {
    let h: Vec<String> = (0..8).map(|d| format!("{d}:{}", TOP_DELTA[d].load(Ordering::Relaxed))).collect();
    format!("[*] mercs2_profiler: cells x87 TOP delta across the game handler, per invocation [{}]", h.join(" "))
}

/// A 512-byte `fxsave` image: no side effects, holds FCW/FSW/abridged tag and the eight registers.
#[repr(C, align(16))]
struct FxArea([u8; 512]);

#[cfg(all(target_arch = "x86", target_os = "windows"))]
fn fxsave() -> FxArea {
    let mut a = FxArea([0; 512]);
    unsafe { core::arch::asm!("fxsave [{p}]", p = in(reg) a.0.as_mut_ptr(), options(nostack)) };
    a
}

#[cfg(not(all(target_arch = "x86", target_os = "windows")))]
fn fxsave() -> FxArea {
    FxArea([0; 512])
}

/// Decode an 80-bit extended value's bytes for a log line: raw hex plus a rough `f64`.
fn ext80(b: &[u8]) -> String {
    let mant = u64::from_le_bytes(b[0..8].try_into().expect("8"));
    let se = u16::from_le_bytes([b[8], b[9]]);
    let (sign, exp) = (se >> 15, se & 0x7fff);
    let value = if exp == 0 && mant == 0 {
        0.0
    } else if exp == 0x7fff {
        if mant << 1 == 0 { f64::INFINITY } else { f64::NAN }
    } else {
        (mant as f64) * 2f64.powi(i32::from(exp) - 16383 - 63)
    };
    format!("{}{:04x}:{:016x}={}", if sign == 1 { "-" } else { "" }, exp, mant, if sign == 1 { -value } else { value })
}

/// The registers the thread's x87 stack holds right now: TOP, the abridged tag (bit i set = physical
/// register i holds a value), and each held register decoded.
fn x87_registers() -> String {
    let a = fxsave();
    let fsw = u16::from_le_bytes([a.0[2], a.0[3]]);
    let top = usize::from((fsw >> 11) & 7);
    let ftw = a.0[4];
    let mut s = format!("TOP {top} tag {ftw:#04x}");
    for phys in 0..8 {
        if ftw & (1 << phys) != 0 {
            // Stack-relative index: ST(i) is physical register (TOP + i) mod 8.
            let st = (phys + 8 - top) & 7;
            s.push_str(&format!(" R{phys}=ST({st}) {}", ext80(&a.0[32 + 16 * st..32 + 16 * st + 10])));
        }
    }
    s
}

/// The abridged x87 tag byte right now (bit i set = physical register i holds a value) and TOP.
/// Sampled by the trampoline immediately before the game handler, so a mismatch can say whether
/// the handler started on an empty stack or inherited a held register.
pub fn x87_entry() -> (u8, u8) {
    let a = fxsave();
    let fsw = u16::from_le_bytes([a.0[2], a.0[3]]);
    (a.0[4], ((fsw >> 11) & 7) as u8)
}

/// Whether shadow mode is on. Set at startup from the config.
pub static ENABLED: AtomicBool = AtomicBool::new(false);
/// Address of the game's cfg_ptr global (`DAT_00F79524` or v1.1's `0x00F78524`). Used only to double-check
/// that the TLS walk picked the same pointer the handler set.
pub static GAME_CFG_PTR_GLOBAL: AtomicUsize = AtomicUsize::new(0);

/// Session totals.
static STATS: Stats = Stats::new();

/// Job 17 invocations currently inside the game handler. The game guards its ring write with a critical
/// section (`DAT_00DD4770`), so more than one thread does reach it; while this is above one, a global
/// before/after counter diff attributes another thread's writes to this invocation, which is why
/// `clean_*` is the only trustworthy comparison.
static IN_FLIGHT: AtomicU32 = AtomicU32::new(0);

struct Stats {
    invocations: AtomicU64,
    matches: AtomicU64,
    mismatches: AtomicU64,
    port_changes: AtomicU64,
    game_changes: AtomicU64,
    walk_mismatches: AtomicU64,
    /// Invocations that had the handler to themselves, and their verdicts. Only these are evidence.
    clean: AtomicU64,
    clean_matches: AtomicU64,
    /// Clean invocations where the port emitted more than the game, and fewer.
    clean_port_over: AtomicU64,
    clean_port_under: AtomicU64,
    /// Invocations that overlapped another in-flight invocation (counter diff is not attributable).
    overlapped: AtomicU64,
    /// Clean invocations where the port's change set differed from the ring's by content, not count.
    content_mismatches: AtomicU64,
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
            clean: AtomicU64::new(0),
            clean_matches: AtomicU64::new(0),
            clean_port_over: AtomicU64::new(0),
            clean_port_under: AtomicU64::new(0),
            overlapped: AtomicU64::new(0),
            content_mismatches: AtomicU64::new(0),
        }
    }
}

/// How many diff lines the session records before it stops formatting them. Bounded so the hot path
/// allocates a fixed number of times per session no matter how long the run is.
const DIFF_BUDGET: u32 = 64;
static DIFF_LEFT: AtomicU32 = AtomicU32::new(DIFF_BUDGET);
/// Recorded diff lines, drained by the frame hook so no logging happens inside a job handler.
static DIFFS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Take the diff lines recorded since the last call.
pub fn drain_diffs() -> Vec<String> {
    let mut g = DIFFS.lock().unwrap_or_else(|p| p.into_inner());
    std::mem::take(&mut *g)
}

/// Record a line for the frame hook to log. Returns false when the budget is spent.
fn record(line: String) -> bool {
    if let Ok(mut g) = DIFFS.lock() {
        g.push(line);
        return true;
    }
    false
}

/// Dumped once, the first time the walk succeeds. Several port decisions were guesses about which of
/// these fields the game uses where (`grid_dim` vs `outer_grid_width`, `cell_step` vs `2^shift`,
/// whether `margin` is even non-zero); printing the live values settles them without another guess.
/// A cheap fingerprint of the config block, so a dump happens whenever the live values change rather
/// than only on the first invocation. `inner_count` and the gates were zero at the first invocation of
/// the last run, which made the whole band path inert and meant that single dump described nothing but
/// the load screen.
static CFG_FINGERPRINT: AtomicU64 = AtomicU64::new(u64::MAX);
static CFG_DUMPS_LEFT: AtomicU32 = AtomicU32::new(24);
/// The reference-point block, dumped on change under the same budget.
static BLOCK_FINGERPRINT: AtomicU64 = AtomicU64::new(u64::MAX);
static BLOCK_DUMPS_LEFT: AtomicU32 = AtomicU32::new(24);
/// Raw per-cell records, which are what a formula gets fitted against offline.
static RAW_LEFT: AtomicU32 = AtomicU32::new(400);

fn fnv(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// Spend one unit of a budget, returning whether it was available.
fn spend(b: &AtomicU32) -> bool {
    b.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| (v > 0).then(|| v - 1)).is_ok()
}

fn dump_block_on_change(arr: &DistanceBlock) {
    // SAFETY-free: DistanceBlock is plain data; hash its bytes to spot a change.
    let bytes = unsafe { core::slice::from_raw_parts((arr as *const DistanceBlock).cast::<u8>(), size_of::<DistanceBlock>()) };
    let h = fnv(bytes);
    if BLOCK_FINGERPRINT.swap(h, Ordering::Relaxed) == h || !spend(&BLOCK_DUMPS_LEFT) {
        return;
    }
    let p = |q: &cells::Point| format!("({:.3}, {:.3}, {:.3})", q.x, q.y, q.z);
    record(format!(
        "cells block: a=[{}, {}, {}, {}] gates_a={:?} b=[{}, {}, {}, {}] gates_b={:?}",
        p(&arr.points_a[0]),
        p(&arr.points_a[1]),
        p(&arr.points_a[2]),
        p(&arr.points_a[3]),
        arr.gates_a,
        p(&arr.points_b[0]),
        p(&arr.points_b[1]),
        p(&arr.points_b[2]),
        p(&arr.points_b[3]),
        arr.gates_b,
    ));
}

fn dump_cfg_once(cfg: &Config) {
    let h = fnv(&cfg.0);
    if CFG_FINGERPRINT.swap(h, Ordering::Relaxed) == h || !spend(&CFG_DUMPS_LEFT) {
        return;
    }
    let shift = cfg.shift();
    record(format!(
        "cells cfg: shift={shift} (2^shift={}) grid_dim={} outer_grid_width={} cell_step={} \
         inner_count={} sub_step={} inner_scale={} margin={} origin=({}, {}) y_origin={} gates={:?} indices={:?}",
        2u32 << u32::from(shift).saturating_sub(1),
        cfg.grid_dim(),
        cfg.outer_grid_width(),
        cfg.cell_step(),
        cfg.inner_count(),
        cfg.sub_step(),
        cfg.inner_scale(),
        cfg.margin(),
        cfg.origin_x(),
        cfg.origin_z(),
        cfg.y_origin(),
        cfg.inner_gates(),
        cfg.inner_indices(),
    ));
}

thread_local! {
    /// Reused per invocation to avoid heap allocation in the hot path.
    static SINK: RefCell<RecordingSink> = RefCell::new(RecordingSink::default());
    /// Reused per invocation to avoid heap allocation.
    static CELL_REFS: RefCell<Vec<*const Cell>> = const { RefCell::new(Vec::new()) };
    /// Whether this thread's current invocation had the handler to itself (set in `pre_handler`).
    static CLEAN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Whether the TLS walk succeeded, so `post_handler` knows the sink holds this invocation's run.
    static RAN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Reused buffer for the ring entries the game wrote during one invocation.
    static RING: RefCell<Vec<Change>> = const { RefCell::new(Vec::new()) };
    /// This invocation's cfg and reference-point block, so the content diff can re-derive a cell's
    /// fade threshold and report it next to the key the game faded on.
    static CFG_PTR: std::cell::Cell<*const Config> = const { std::cell::Cell::new(core::ptr::null()) };
    static ARR_PTR: std::cell::Cell<*const DistanceBlock> = const { std::cell::Cell::new(core::ptr::null()) };
    /// One copy of each cell's state bytes, taken before the handler runs. The handler zeroes the
    /// state of every slot it fades, so this is the only chance to see the pre-fade values; copying
    /// the fixed 32-byte block is far cheaper than snapshotting per-slot records.
    static STATES: RefCell<Vec<[u8; STATE_SNAP]>> = const { RefCell::new(Vec::new()) };
    /// One copy of each cell's sub-slot records (`0xA0..0x120`: `{x_byte, y_byte, key}` × 32), taken
    /// before the handler runs. The handler never writes these, so a record that differs afterwards
    /// was rewritten by another thread while the handler could have been reading it.
    static RECORDS: RefCell<Vec<[u8; RECORD_SNAP]>> = const { RefCell::new(Vec::new()) };
    /// Reused `(entity_id, old_state, packed)` scratch for one cell's interval measurement.
    static SLOTS: RefCell<Vec<(u32, u8, u16)>> = const { RefCell::new(Vec::new()) };
    /// The 96-byte job element (`TLS+0xC`) as the walk read it. The dispatcher re-reads `a`, `b`,
    /// `count` and the slot table from it on every cell, so if the element is overwritten under the
    /// handler — the worker refilling a ring slot — the live run's parameters diverge from the walk's.
    static JOB_PRE: RefCell<[u8; JOB_SNAP]> = const { RefCell::new([0; JOB_SNAP]) };
    static JOB_PTR: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// x87 words at pre-handler (what the live handler inherits) and at post-handler entry.
    static X87: std::cell::Cell<[(u16, u16); 2]> = const { std::cell::Cell::new([(0, 0); 2]) };
    /// The reference-point block as the port read it, before the handler. Compared against the live
    /// block after the handler returns, so a mismatch can be attributed to the block moving while the
    /// handler ran rather than to the port.
    static BLOCK_PRE: RefCell<DistanceBlock> = const { RefCell::new(DistanceBlock { points_a: [cells::Point { x: 0.0, y: 0.0, z: 0.0 }; 4], gates_a: [0; 4], points_b: [cells::Point { x: 0.0, y: 0.0, z: 0.0 }; 4], gates_b: [0; 4] }) };
}

/// Where `entity` sits in this invocation's cells, with the state byte the port read before the
/// handler and the byte there now, plus the slot's packed key. The two bytes differing is direct
/// evidence the cell changed under the handler; equal bytes with a mismatch point at the port.
fn locate(entity: u32) -> String {
    CELL_REFS.with_borrow(|refs| {
        STATES.with_borrow(|snaps| {
            for (k, &p) in refs.iter().enumerate() {
                // SAFETY: the same live cell pointers the handler just used.
                let cell = unsafe { &*p };
                let count = (cell.sub_slot_count().max(0) as usize).min(STATE_SNAP);
                for i in 0..count {
                    if cell.entity_id(i) == entity {
                        let pre = snaps.get(k).map_or(0xff, |s| s[i]);
                        let record_pre = RECORDS.with_borrow(|recs| recs.get(k).map(|r| [r[i * 4], r[i * 4 + 1], r[i * 4 + 2], r[i * 4 + 3]]));
                        let record_now = [cell.0[0xa0 + i * 4], cell.0[0xa1 + i * 4], cell.0[0xa2 + i * 4], cell.0[0xa3 + i * 4]];
                        let record = match record_pre {
                            Some(r) if r == record_now => format!("record {:02x?} unchanged", record_now),
                            Some(r) => format!("RECORD CHANGED pre {r:02x?} now {record_now:02x?}"),
                            None => String::from("record pre missing"),
                        };
                        // Re-run the port's slot math on the live inputs now, so the line carries
                        // every number the outcome depends on. The state byte it reads is the one
                        // the handler stored, so `old`-dependent terms are evaluated against `now`,
                        // which the line also prints.
                        let (cfg_ptr, arr_ptr) = (CFG_PTR.get(), ARR_PTR.get());
                        let math = if cfg_ptr.is_null() || arr_ptr.is_null() {
                            String::from("no cfg/arr")
                        } else {
                            // SAFETY: walked for this invocation; the handler has only just returned.
                            let (cfg, arr) = unsafe { (&*cfg_ptr, &*arr_ptr) };
                            match cells::cell_threshold(cell, cfg, arr).and_then(|t| cells::slot_context(cell, cfg, arr, t).map(|c| (t, c))) {
                                Some((t, ctx)) => {
                                    let p = &arr.points_a[usize::from(cfg.inner_indices()[0])];
                                    format!(
                                        "groupmin {t:.1} cutoff {} lod {:?} base {} inner_count {} gate0 {} p0 ({:.2},{:.2},{:.2}) :: {}",
                                        ctx.fade_threshold,
                                        cell.lod_bytes(i),
                                        cell.lod_base(i),
                                        cfg.inner_count(),
                                        cfg.inner_gates()[usize::from(cfg.inner_indices()[0])],
                                        p.x,
                                        p.y,
                                        p.z,
                                        cells::slot_math(&ctx, cell, i).describe()
                                    )
                                }
                                None => String::from("no grid"),
                            }
                        };
                        return format!(
                            "[cell {} slot {i} pre {pre:#04x} now {:#04x} key {} {record} | {math}]",
                            cell.cell_id(),
                            cell.state(i),
                            cell.sub_slot_key(i) & 0x3fff
                        );
                    }
                }
            }
            String::from("[entity not in this invocation's cells]")
        })
    })
}

/// How many (cell, slot) pairs of this invocation carry `entity`. More than one means a change
/// attributed to one slot may have been written at another.
fn occurrences(entity: u32) -> usize {
    CELL_REFS.with_borrow(|refs| {
        refs.iter()
            .map(|&p| {
                // SAFETY: the same live cell pointers the handler just used.
                let cell = unsafe { &*p };
                let count = (cell.sub_slot_count().max(0) as usize).min(STATE_SNAP);
                (0..count).filter(|&i| cell.entity_id(i) == entity).count()
            })
            .sum()
    })
}

/// One line when the reference block the handler read differs from the one the port read.
fn block_drift() -> Option<String> {
    let arr_ptr = ARR_PTR.get();
    if arr_ptr.is_null() {
        return None;
    }
    // SAFETY: walked for this invocation; the handler has only just returned.
    let now = unsafe { &*arr_ptr };
    BLOCK_PRE.with_borrow(|pre| {
        if pre == now {
            return None;
        }
        let p = |q: &cells::Point| format!("({:.2},{:.2},{:.2})", q.x, q.y, q.z);
        Some(format!(
            "cells block moved under the handler: A0 {} -> {} gates_a {:?} -> {:?}; B0 {} -> {} gates_b {:?} -> {:?}",
            p(&pre.points_a[0]),
            p(&now.points_a[0]),
            pre.gates_a,
            now.gates_a,
            p(&pre.points_b[0]),
            p(&now.points_b[0]),
            pre.gates_b,
            now.gates_b
        ))
    })
}

/// The cell's state array spans `0x80..0xA0`, so a cell can hold at most this many sub-slots.
const STATE_SNAP: usize = 0x20;
/// The cell's sub-slot records span `0xA0..0x120`.
const RECORD_SNAP: usize = 0x80;
/// A Pimp job element: `route::JOB_BYTES`.
const JOB_SNAP: usize = crate::route::JOB_BYTES;

/// What the dispatcher left in the three globals after the live run, against what the walk read,
/// and whether the job element changed under the handler. Empty when nothing differs.
fn parameter_drift() -> String {
    let mut s = String::new();
    if let Some(Some(g)) = GAME_COPY.get() {
        // SAFETY: the globals are plain words in the game's data section.
        let (cfg, arr, size) = unsafe {
            (
                (g.cfg_global as *const usize).read_volatile(),
                (g.arr_global as *const usize).read_volatile(),
                (g.size_global as *const f32).read_volatile(),
            )
        };
        if cfg != CFG_PTR.get() as usize || arr != ARR_PTR.get() as usize {
            s.push_str(&format!(
                " GLOBALS DIFFER: dispatcher cfg {cfg:#x} arr {arr:#x} size {size} vs walked cfg {:#x} arr {:#x}",
                CFG_PTR.get() as usize,
                ARR_PTR.get() as usize
            ));
        }
    }
    let job = JOB_PTR.get();
    if job != 0 {
        // SAFETY: the ring element the walk read; still mapped (the ring is static).
        let now: [u8; JOB_SNAP] = unsafe { core::ptr::read_volatile(job as *const [u8; JOB_SNAP]) };
        JOB_PRE.with_borrow(|pre| {
            if *pre != now {
                let changed: Vec<String> = (0..JOB_SNAP / 4)
                    .filter(|&w| pre[w * 4..w * 4 + 4] != now[w * 4..w * 4 + 4])
                    .map(|w| {
                        format!(
                            "+{:#x}:{:#x}->{:#x}",
                            w * 4,
                            u32::from_le_bytes(pre[w * 4..w * 4 + 4].try_into().expect("4")),
                            u32::from_le_bytes(now[w * 4..w * 4 + 4].try_into().expect("4"))
                        )
                    })
                    .collect();
                s.push_str(&format!(" JOB ELEMENT CHANGED at {job:#x}: {}", changed.join(" ")));
            }
        });
    }
    s
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

/// The attributable half of the comparison, which is the only half that is evidence:
/// (clean, clean_matches, port_over, port_under, overlapped, content_mismatches).
pub fn clean_stats() -> (u64, u64, u64, u64, u64, u64) {
    (
        STATS.clean.load(Ordering::Relaxed),
        STATS.clean_matches.load(Ordering::Relaxed),
        STATS.clean_port_over.load(Ordering::Relaxed),
        STATS.clean_port_under.load(Ordering::Relaxed),
        STATS.overlapped.load(Ordering::Relaxed),
        STATS.content_mismatches.load(Ordering::Relaxed),
    )
}

/// Called by the jobs trampoline before the game handler runs. Snapshots the inputs via the TLS walker,
/// runs the port into the thread-local sink, and (once) checks the walked cfg pointer against the game's
/// stale global so we can tell early if the walker drifts from reality.
///
/// # Safety
/// Must run on the game's main thread as a job 17 handler is about to execute.
pub unsafe fn pre_handler() {
    // Claim the in-flight slot first, so an invocation that bails on the walk still accounts for the
    // overlap it caused for anyone else.
    let was = IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
    CLEAN.set(was == 0);
    RAN.set(false);
    // An invocation the walk cannot read, or one with no cells, is not compared; the game's handler
    // runs either way.
    let Ok(walk) = CELL_REFS.with_borrow_mut(|refs| unsafe { crate::live::walk_tls_job(refs) }) else {
        return;
    };
    if CELL_REFS.with_borrow(Vec::is_empty) {
        return;
    }
    JOB_PTR.set(walk.job as usize);
    // SAFETY: a live ring element of `JOB_BYTES` bytes.
    JOB_PRE.with_borrow_mut(|pre| *pre = unsafe { core::ptr::read_volatile(walk.job as *const [u8; JOB_SNAP]) });
    RAN.set(true);
    let (cfg_ptr, arr_ptr) = (walk.cfg, walk.arr);
    CFG_PTR.set(cfg_ptr);
    ARR_PTR.set(arr_ptr);
    // SAFETY: live game memory for the duration of this call.
    dump_cfg_once(unsafe { &*cfg_ptr });
    dump_block_on_change(unsafe { &*arr_ptr });
    BLOCK_PRE.with_borrow_mut(|b| *b = unsafe { *arr_ptr });
    SINK.with_borrow_mut(|sink| {
        sink.0.clear();
        CELL_REFS.with_borrow(|refs| {
            STATES.with_borrow_mut(|snaps| {
                snaps.clear();
                for &p in refs.iter() {
                    // SAFETY: live game memory; `Cell` is a plain 0x1f0-byte block.
                    let states = unsafe { &(&(*p).0)[0x80..0x80 + STATE_SNAP] };
                    snaps.push(states.try_into().expect("STATE_SNAP bytes"));
                }
            });
            RECORDS.with_borrow_mut(|recs| {
                recs.clear();
                for &p in refs.iter() {
                    // SAFETY: as above.
                    let records = unsafe { &(&(*p).0)[0xa0..0xa0 + RECORD_SNAP] };
                    recs.push(records.try_into().expect("RECORD_SNAP bytes"));
                }
            });
            X87.set([x87_words(), (0, 0)]);
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
    // Release the in-flight slot before anything that can early-return.
    IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
    if !RAN.get() {
        return;
    }
    let mut x87 = X87.get();
    x87[1] = x87_words();
    X87.set(x87);
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

    // Only an invocation that had the handler to itself can be attributed: the ring counter is global
    // and the game writes it under a shared critical section.
    if !CLEAN.get() {
        STATS.overlapped.fetch_add(1, Ordering::Relaxed);
        return;
    }
    STATS.clean.fetch_add(1, Ordering::Relaxed);
    if port_count == game_count {
        STATS.clean_matches.fetch_add(1, Ordering::Relaxed);
    } else if port_count > game_count {
        STATS.clean_port_over.fetch_add(1, Ordering::Relaxed);
    } else {
        STATS.clean_port_under.fetch_add(1, Ordering::Relaxed);
    }
    compare_content(game_before, game_after);
}

/// The candidate fade-cutoff formulas under test. Several port decisions were guesses about which
/// config fields the game uses to build the cell box the group-min runs over; rather than guess
/// again, every candidate is scored against the cutoff measured from the game's own behaviour.
const CANDIDATES: [&str; 6] = [
    "groupmin(grid_dim, cell_step)",
    "groupmin(outer_grid_width, 2^shift)", // what the port uses today
    "groupmin(grid_dim, 2^shift)",
    "groupmin(outer_grid_width, cell_step)",
    "2^shift",
    "cell_step",
];

/// Per-candidate tallies: cells whose measured cutoff interval contains the candidate, and cells
/// where it does not.
static CAND_OK: [AtomicU64; CANDIDATES.len()] = [const { AtomicU64::new(0) }; CANDIDATES.len()];
static CAND_BAD: [AtomicU64; CANDIDATES.len()] = [const { AtomicU64::new(0) }; CANDIDATES.len()];
/// Cells that yielded a two-sided interval at all.
static MEASURED_CELLS: AtomicU64 = AtomicU64::new(0);

/// One line per candidate: name, consistent, inconsistent.
pub fn candidate_report() -> Vec<String> {
    let measured = MEASURED_CELLS.load(Ordering::Relaxed);
    let mut v = vec![format!("cells cutoff: {measured} cells gave a two-sided interval")];
    for (i, name) in CANDIDATES.iter().enumerate() {
        let ok = CAND_OK[i].load(Ordering::Relaxed);
        let bad = CAND_BAD[i].load(Ordering::Relaxed);
        let total = ok + bad;
        v.push(format!(
            "cells cutoff candidate {i} {name}: {ok} consistent, {bad} inconsistent ({:.3}%)",
            if total == 0 { 0.0 } else { 100.0 * ok as f64 / total as f64 }
        ));
    }
    v
}

/// The measured two-sided interval `(lo, hi]` for one cell's fade cutoff `T`, or `None` when this
/// cell's slots do not constrain it from both sides.
///
/// `slots` is `(entity_id, old_state, packed)` per sub-slot, `old_state` taken before the handler
/// ran. The game fades exactly when `packed < T` **and** the old state is non-zero, so:
///
/// * a slot the game faded proves `T > packed`,
/// * a slot with a non-zero old state the game did **not** fade proves `T <= packed`,
/// * a slot whose old state is zero emits nothing either way and carries no information.
///
/// Pure, so the arithmetic that the whole experiment rests on is testable on the host.
fn cutoff_interval(slots: &[(u32, u8, u16)], faded: &[u32]) -> Option<(i64, i64)> {
    let mut lo = -1i64;
    let mut hi = i64::MAX;
    let (mut saw_fade, mut saw_held) = (false, false);
    for &(entity, state, packed) in slots {
        let p = i64::from(packed);
        if faded.contains(&entity) {
            lo = lo.max(p);
            saw_fade = true;
        } else if state != 0 {
            hi = hi.min(p);
            saw_held = true;
        }
    }
    (saw_fade && saw_held && lo < hi).then_some((lo, hi))
}

/// Whether a candidate cutoff value is consistent with a measured interval.
fn consistent(t: i64, lo: i64, hi: i64) -> bool {
    t > lo && t <= hi
}

fn cell_box(cfg: &Config, cell_id: i16, grid: i16, step: f32) -> cells::Box2D {
    let row = if grid > 0 { i32::from(cell_id) / i32::from(grid) } else { 0 };
    let col = i32::from(cell_id) - row * i32::from(grid.max(1));
    let wx = cfg.origin_x() + (col as f32) * step;
    let wz = cfg.origin_z() + (row as f32) * step;
    cells::Box2D { xmin: wx, zmin: wz, xmax: wx + step, zmax: wz + step }
}

/// Every candidate cutoff for one cell, in [`CANDIDATES`] order.
fn candidate_values(cfg: &Config, arr: &DistanceBlock, cell_id: i16) -> [f32; CANDIDATES.len()] {
    let size = (2u32 << u32::from(cfg.shift()).saturating_sub(1)) as f32;
    let step = cfg.cell_step() as f32;
    let gd = cfg.grid_dim();
    let og = cfg.outer_grid_width();
    let mut out = [0.0f32; 4];
    [
        cells::cell_group_min(&mut out, arr, &cell_box(cfg, cell_id, gd, step)),
        cells::cell_group_min(&mut out, arr, &cell_box(cfg, cell_id, og, size)),
        cells::cell_group_min(&mut out, arr, &cell_box(cfg, cell_id, gd, size)),
        cells::cell_group_min(&mut out, arr, &cell_box(cfg, cell_id, og, step)),
        size,
        step,
    ]
}

/// Measure the game's own fade cutoff for each cell of this invocation and score every candidate
/// against it.
///
/// The game fades a sub-slot exactly when `packed < T` and its state is non-zero. So for one cell,
/// given which slots the game actually faded (the ring entries whose new state is `0x00`):
///
/// * every faded slot proves `T > packed`,
/// * every slot with a non-zero old state that was **not** faded proves `T <= packed`.
///
/// That is a two-sided interval `T in (lo, hi]` derived entirely from observed behaviour, with no
/// formula assumed. A candidate is consistent when its integer value lands inside. A candidate that
/// is consistent for every measured cell is the formula; if none is, the cutoff is not a group-min of
/// a cell box at all and that whole line of reasoning is dead.
fn score_candidates(faded: &[u32]) {
    let (cfg_ptr, arr_ptr) = (CFG_PTR.get(), ARR_PTR.get());
    if cfg_ptr.is_null() || arr_ptr.is_null() {
        return;
    }
    // SAFETY: walked for this invocation; the handler has only just returned.
    let (cfg, arr) = unsafe { (&*cfg_ptr, &*arr_ptr) };
    CELL_REFS.with_borrow(|refs| {
        STATES.with_borrow(|snaps| {
            for (k, &p) in refs.iter().enumerate() {
                let Some(snap) = snaps.get(k) else { continue };
                // SAFETY: the same live cell pointer the handler just used. entity ids and keys are
                // not written by the handler, so reading them after it returns is sound; the state
                // bytes it does write were snapshotted before the call.
                let cell = unsafe { &*p };
                let count = (cell.sub_slot_count().max(0) as usize).min(STATE_SNAP);
                SLOTS.with_borrow_mut(|slots| {
                    slots.clear();
                    for i in 0..count {
                        slots.push((cell.entity_id(i), snap[i], cell.sub_slot_key(i) & 0x3fff));
                    }
                });
                let Some((lo, hi)) = SLOTS.with_borrow(|s| cutoff_interval(s, faded)) else {
                    continue;
                };
                MEASURED_CELLS.fetch_add(1, Ordering::Relaxed);
                let vals = candidate_values(cfg, arr, cell.cell_id());
                for (ci, v) in vals.iter().enumerate() {
                    if consistent(i64::from(*v as i32), lo, hi) {
                        CAND_OK[ci].fetch_add(1, Ordering::Relaxed);
                    } else {
                        CAND_BAD[ci].fetch_add(1, Ordering::Relaxed);
                    }
                }
                // The raw inputs plus the measured constraint. `hi` proved uninformative last run
                // (always 12000, an always-visible sentinel), so `lo` -- the largest radius the game
                // actually faded -- is the signal, and it needs the cell's own geometry beside it to
                // be worth anything. Everything here is measured; no formula is assumed.
                if spend(&RAW_LEFT) {
                    let bx = cell_box(cfg, cell.cell_id(), cfg.grid_dim(), cfg.cell_step() as f32);
                    let faded_packed = SLOTS.with_borrow(|s| {
                        let mut v: Vec<u16> = s.iter().filter(|(e, _, _)| faded.contains(e)).map(|(_, _, p)| *p).collect();
                        v.sort_unstable();
                        v
                    });
                    record(format!(
                        "cells raw: cell={} box=({:.2},{:.2},{:.2},{:.2}) lo={lo} hi={hi} cheb={:?} faded_packed={faded_packed:?}",
                        cell.cell_id(),
                        bx.xmin,
                        bx.zmin,
                        bx.xmax,
                        bx.zmax,
                        vals.map(|v| v as i32),
                    ));
                }
            }
        });
    });
}

/// For a clean invocation, read back the ring entries the game just wrote and compare them to the
/// port's own change list as a multiset of `(entity_id, old, new)`. Records up to [`DIFF_BUDGET`]
/// human-readable differences for the frame hook to log.
fn compare_content(game_before: u32, game_after: u32) {
    let Some(base) = crate::jobs::cells_ring_base() else {
        return;
    };
    RING.with_borrow_mut(|ring| {
        ring.clear();
        // SAFETY: `base` is the resolved ring site and the window [before, after) was written by the
        // handler that just returned; a clean invocation means no other thread advanced the counter.
        unsafe { parse_ring(base as *const u8, game_before, game_after, ring) };
        // A fade is the only transition whose new state is zero: the band path returns early rather
        // than store a stateless tier, so `new == 0` identifies the game's fades exactly.
        if !ring.is_empty() {
            let faded: Vec<u32> = ring.iter().filter(|c| c.old_new >> 8 == 0).map(|c| c.entity_id).collect();
            if !faded.is_empty() {
                score_candidates(&faded);
            }
        }
        SINK.with_borrow(|sink| {
            let port = &sink.0;
            let mut missing: Vec<&Change> = Vec::new();
            let mut taken = vec![false; port.len()];
            for g in ring.iter() {
                match port
                    .iter()
                    .enumerate()
                    .position(|(i, p)| !taken[i] && p.entity_id == g.entity_id && p.old_new == g.old_new)
                {
                    Some(i) => taken[i] = true,
                    None => missing.push(g),
                }
            }
            let extra: Vec<&Change> = port.iter().zip(&taken).filter(|(_, t)| !**t).map(|(p, _)| p).collect();
            if missing.is_empty() && extra.is_empty() {
                return;
            }
            STATS.content_mismatches.fetch_add(1, Ordering::Relaxed);
            if DIFF_LEFT.fetch_sub(1, Ordering::Relaxed) == 0 {
                DIFF_LEFT.store(0, Ordering::Relaxed);
                return;
            }
            let mut line = String::from("cells diff:");
            for g in missing.iter().take(3) {
                line.push_str(&format!(
                    " game-only entity {:#010x} {:#04x}->{:#04x} residue {:#010x} {:#010x} occurs {}x {};",
                    g.entity_id,
                    g.old_new & 0xff,
                    g.old_new >> 8,
                    g.extra[0],
                    g.extra[1],
                    occurrences(g.entity_id),
                    locate(g.entity_id)
                ));
            }
            for p in extra.iter().take(3) {
                line.push_str(&format!(
                    " port-only entity {:#010x} {:#04x}->{:#04x} {};",
                    p.entity_id,
                    p.old_new & 0xff,
                    p.old_new >> 8,
                    locate(p.entity_id)
                ));
            }
            line.push_str(&format!(" ({} game-only, {} port-only)", missing.len(), extra.len()));
            let x = X87.get();
            let live = LIVE_SW.get();
            line.push_str(&format!(
                " x87 pre-handler [{}] around-live [entry tag {:#04x} sw {:#06x} TOP {} -> sw {:#06x} TOP {}] post-handler [{}] now [{}]",
                x87_text(x[0]),
                live.2,
                live.0,
                (live.0 >> 11) & 7,
                live.1,
                (live.1 >> 11) & 7,
                x87_text(x[1]),
                x87_registers()
            ));
            if let Some(drift) = block_drift() {
                line.push(' ');
                line.push_str(&drift);
            }
            line.push_str(&parameter_drift());
            record(line);
        });
    });
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

/// A ring entry's bytes read as `(entity_id, old, new)`. Read off `FUN_00735840`'s own write:
///
/// ```text
/// *(u32 *)(&DAT_00dd4790 + n*0x10) = entity_id;                  // +0
/// *(u32 *)(&DAT_00dd4794 + n*0x10) = CONCAT11(new, old);         // +4  = old | new << 8
/// *(u32 *)(&DAT_00dd4798 + n*0x10) = uninit;                     // +8
/// *(u32 *)(&DAT_00dd479c + n*0x10) = uninit;                     // +12
/// ```
///
/// The fade branch reaches the same ring through the batch push `FUN_00518830`, which writes the two
/// qwords of a `{entity_id, old | 0 << 8}` record, so a fade appears as `old -> 0`. `+8` and `+12` are
/// whatever was on the handler's stack and carry no meaning.
///
/// # Safety
/// `ring_base` must be a readable `n * 16`-byte buffer.
pub unsafe fn parse_ring(ring_base: *const u8, before: u32, after: u32, out: &mut Vec<Change>) {
    let count = after.wrapping_sub(before);
    for i in 0..count {
        let p = unsafe { ring_base.add(((before.wrapping_add(i)) * 16) as usize) };
        let entity_id = unsafe { (p as *const u32).read_volatile() };
        let packed = unsafe { (p.add(4) as *const u16).read_volatile() };
        // The residue words fingerprint the writer's stack: `0x0073580A` at +12 is the return address
        // the normal path of `FUN_00735840` leaves behind (seen in the dump); the batch push leaves
        // its own. A mismatched entry with a different fingerprint was not written by the handler.
        let extra = unsafe { [(p.add(8) as *const u32).read_volatile(), (p.add(12) as *const u32).read_volatile()] };
        out.push(Change { entity_id, old_new: packed, extra });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ring_extracts_entity_id_and_packed_old_new() {
        let bytes = [
            // entry 0: entity at +0, packed old|new<<8 at +4, +8 and +12 are stack residue
            0x22, 0x22, 0x22, 0x22, // entity_id = 0x22222222
            0x40, 0x10, 0x00, 0x00, // old=0x40, new=0x10
            0x11, 0x11, 0x11, 0x11, // residue
            0x33, 0x33, 0x33, 0x33, // residue
            // entry 1: a fade, old=0x40 -> new=0
            0xaa, 0xbb, 0xcc, 0xdd, 0x40, 0x00, 0x00, 0x00, 0x44, 0x44, 0x44, 0x44, 0x55, 0x55, 0x55, 0x55,
        ];
        let mut out = Vec::new();
        unsafe { parse_ring(bytes.as_ptr(), 0, 2, &mut out) };
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].entity_id, 0x22222222);
        assert_eq!(out[0].old_new, 0x1040);
        assert_eq!(out[1].entity_id, 0xddccbbaa);
        assert_eq!(out[1].old_new, 0x0040);
    }

    #[test]
    fn a_cutoff_interval_is_bounded_by_the_faded_below_and_the_held_above() {
        // faded at 10 and 7 -> T > 10; held (state != 0) at 20 and 40 -> T <= 20.
        let slots = [(1u32, 0x11u8, 10u16), (2, 0x41, 7), (3, 0x11, 20), (4, 0x21, 40)];
        assert_eq!(cutoff_interval(&slots, &[1, 2]), Some((10, 20)));
    }

    /// A slot whose old state is already zero emits nothing whichever side of the cutoff it is on,
    /// so letting it set the upper bound would invent a constraint the game never demonstrated.
    #[test]
    fn a_zero_state_slot_constrains_nothing() {
        let slots = [(1u32, 0x11u8, 10u16), (2, 0x00, 12), (3, 0x11, 30)];
        assert_eq!(cutoff_interval(&slots, &[1]), Some((10, 30)), "the state-0 slot at 12 must not cap hi");
    }

    #[test]
    fn a_cell_constrained_on_only_one_side_is_not_measured() {
        let only_fades = [(1u32, 0x11u8, 10u16), (2, 0x41, 7)];
        assert_eq!(cutoff_interval(&only_fades, &[1, 2]), None);
        let only_held = [(3u32, 0x11u8, 20u16)];
        assert_eq!(cutoff_interval(&only_held, &[]), None);
    }

    /// If a faded key is at or above a held key the two constraints contradict, which means an
    /// assumption is wrong. Such a cell must be dropped rather than scored.
    #[test]
    fn a_contradictory_cell_is_dropped() {
        let slots = [(1u32, 0x11u8, 30u16), (2, 0x11, 20)];
        assert_eq!(cutoff_interval(&slots, &[1]), None);
    }

    #[test]
    fn consistency_is_half_open_matching_the_games_strict_less_than() {
        // T in (10, 20]: the game fades when packed < T, so T == 20 still fades everything below 20.
        assert!(!consistent(10, 10, 20), "lo is exclusive");
        assert!(consistent(11, 10, 20));
        assert!(consistent(20, 10, 20), "hi is inclusive");
        assert!(!consistent(21, 10, 20));
    }

    #[test]
    fn the_dispatcher_globals_check_arms_only_for_the_v11_patched_securom_exe() {
        assert!(arm_game_copy(&[0u8; 32]).contains("not armed"));
        // The OnceLock keeps the first answer; the sha still decides the line's text.
        assert!(arm_game_copy(&V11_PATCHED_SECUROM).contains("globals armed: 0xf78524"));
    }

    #[test]
    fn parse_ring_reads_from_the_before_index_not_zero() {
        let mut bytes = [0u8; 48];
        bytes[32..36].copy_from_slice(&0xfeed_face_u32.to_le_bytes());
        bytes[36..38].copy_from_slice(&0x3020_u16.to_le_bytes());
        let mut out = Vec::new();
        unsafe { parse_ring(bytes.as_ptr(), 2, 3, &mut out) };
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].entity_id, 0xfeed_face);
        assert_eq!(out[0].old_new, 0x3020);
    }
}
