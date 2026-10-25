//! Rust port of jobtype `0xECAED415` — a per-cell distance test the game posts about 625 times per frame
//! (jobtype hash `0xECAED415`, handler `FUN_00735C80` in v1.0, `FUN_00735E80` in v1.1).
//!
//! The handler reads the current job struct through the thread's TLS block, looks up a slot in the job's
//! dispatch table (three indices at `+0x0C`/`+0x10`/`+0x14` sum to pick it), pulls four values out of that
//! slot (a config pointer, an output buffer base, a viewpoint, a cell-size scale), and runs the per-cell
//! loop. In the game these four live in globals (`0xF79524`, `0xF7951C`, `0xF79520`, `0x1176260`), which is
//! why two copies of the job can't run at once: they'd clobber each other's globals.
//!
//! The port takes them as plain function arguments. Nothing in the port writes a global, so the port is
//! re-entrant from the start.
//!
//! # Verification
//!
//! The port is not swapped in until it matches the original. The profiler can be built in a mode that, for
//! every job 17 the game dispatches, snapshots the inputs, lets the original run, then re-runs the port
//! against a copy of the inputs into a shadow change-list and compares it to what the original wrote. A
//! mismatch is a fatal error naming the cell and sub-slot. See [`verify`].


/// What the per-cell loop writes: a cell slot's new LOD byte, from its old byte, together with the slot's
/// owning entity id. The game appends these to a critical-section-guarded list at `0xDD5770/0xDD5790`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    /// The entity id of the slot whose LOD changed, from `cell[-0xA2 ...]`.
    pub entity_id: u32,
    /// `[old << 8 | new]`.
    pub old_new: u16,
    /// Two words of auxiliary data (set by callers of the change-emitting helper).
    pub extra: [u32; 2],
}

/// The config block for one dispatch slot. All reads are by offset to keep the game's layout verbatim.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Config(pub [u8; 0x60]);

/// The named fields of a config block. Game offsets (v1.0) in comments.
impl Config {
    /// `cfg[0x04]`: shift. `2 << (shift - 1)` is the cell's world size (`DAT_00F79520`).
    pub fn shift(&self) -> u8 { self.0[0x04] }
    pub fn grid_dim(&self) -> i16 { self.i16(0x06) }
    /// `cfg[0x08]`: cells-per-row of the outer grid (`FUN_00735BD0` splits `cell_id` by this).
    pub fn outer_grid_width(&self) -> i16 { self.i16(0x08) }
    pub fn inner_count(&self) -> i16 { self.i16(0x0a) }
    pub fn origin_x(&self) -> f32 { self.f32(0x0c) }
    pub fn origin_y(&self) -> f32 { self.f32(0x10) }
    pub fn origin_z(&self) -> f32 { self.f32(0x14) }
    /// The byte stride a cell `grid_id` advances in world coordinates; stored as an `i32` which the game
    /// multiplies before adding to the origin.
    pub fn cell_step(&self) -> i32 { self.i32(0x24) }
    /// The sub-cell world step applied per sub-slot `(x, z)` byte coordinate.
    pub fn sub_step(&self) -> f32 { self.f32(0x28) }
    pub fn y_origin(&self) -> i16 { self.i16(0x30) }
    /// Scale factor applied to a sub-slot's packed 14-bit radius key before squaring.
    pub fn inner_scale(&self) -> f32 { self.f32(0x38) }
    /// Outer radius margin, added to the scaled inner radius to produce the outer threshold.
    pub fn margin(&self) -> f32 { self.f32(0x3c) }
    /// `inner_gates[k]` for a reference point index `k` (from `inner_indices`): if 0, that point is
    /// not valid for LOD updates.
    pub fn inner_gates(&self) -> &[u8; 16] { (&self.0[0x40..0x50]).try_into().expect("16 bytes") }
    /// The reference point index used for inner iteration `i` (`cfg[0x44 + i]`).
    pub fn inner_indices(&self) -> &[u8; 16] { (&self.0[0x44..0x54]).try_into().expect("16 bytes") }

    fn i16(&self, at: usize) -> i16 { i16::from_le_bytes(self.0[at..at + 2].try_into().unwrap()) }
    fn i32(&self, at: usize) -> i32 { i32::from_le_bytes(self.0[at..at + 4].try_into().unwrap()) }
    fn f32(&self, at: usize) -> f32 { f32::from_le_bytes(self.0[at..at + 4].try_into().unwrap()) }
}

/// A cell block the job walks. 0x1f0 bytes in the game. Field accessors by offset.
#[repr(C)]
pub struct Cell(pub [u8; 0x1f0]);

/// Cell layout, per sub-slot `i`:
/// - `cell[i*4..i*4+4]` entity_id (0..0x80, 32 slots)
/// - `cell[0x80 + i]` state byte (0x80..0xA0)
/// - `cell[0xA0 + i*4]` sub-slot record (4 bytes: x_byte, y_byte, key u16) (0xA0..0x120)
/// - `cell[0x120 + i]` y-contribution byte (0x120..0x140)
/// - `cell[0x140 + i*3]` three LOD-radius bytes (0x140..0x1A0)
/// - `cell[0x1A0 + i*2]` LOD base word (0x1A0..0x1E0)
/// - `cell[0x1E4]` sub_slot_count (i16); `cell[0x1E6]` cell_id (i16)
impl Cell {
    pub fn sub_slot_count(&self) -> i16 { self.i16(0x1e4) }
    pub fn cell_id(&self) -> i16 { self.i16(0x1e6) }
    pub fn entity_id(&self, i: usize) -> u32 { self.u32(i * 4) }
    pub fn state(&self, i: usize) -> u8 { self.0[0x80 + i] }
    /// Low byte of the sub-slot record (`cell[0xA0 + i*4]`): the sub-slot's X byte coordinate.
    pub fn sub_slot_x_byte(&self, i: usize) -> u8 { self.0[0xA0 + i * 4] }
    /// Second byte of the sub-slot record (`cell[0xA1 + i*4]`): the sub-slot's Y byte coordinate.
    pub fn sub_slot_y_byte(&self, i: usize) -> u8 { self.0[0xA1 + i * 4] }
    /// `cell[0xA2 + i*4]` as `u16`: packed distance key (low 14 bits) + 2 high bits (part of `y`).
    pub fn sub_slot_key(&self, i: usize) -> u16 { self.u16(0xA2 + i * 4) }
    /// `cell[0x120 + i]`: byte added to the Y computation.
    pub fn y_contrib(&self, i: usize) -> u8 { self.0[0x120 + i] }
    /// `cell[0x140 + i*3..][0..3]` as `(lo, mid, hi)`: the three LOD-radius deltas.
    pub fn lod_bytes(&self, i: usize) -> (u8, u8, u8) {
        let b = 0x140 + i * 3;
        (self.0[b], self.0[b + 1], self.0[b + 2])
    }
    /// `cell[0x1A0 + i*2]` as `u16`: the base LOD-radius word.
    pub fn lod_base(&self, i: usize) -> u16 { self.u16(0x1A0 + i * 2) }

    fn u16(&self, at: usize) -> u16 { u16::from_le_bytes(self.0[at..at + 2].try_into().unwrap()) }
    fn i16(&self, at: usize) -> i16 { i16::from_le_bytes(self.0[at..at + 2].try_into().unwrap()) }
    fn u32(&self, at: usize) -> u32 { u32::from_le_bytes(self.0[at..at + 4].try_into().unwrap()) }
}

/// The outcome of running the per-sub-slot test on one sub-slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotOutcome {
    /// The sub-slot was culled (distance key below the fade threshold); state becomes 0.
    Faded,
    /// LOD bits accumulated but no 0x10..0x40 band was set — the loop should `continue` without writing.
    NoBand,
    /// A new state byte: emit a change if it differs from the current state.
    Band(u8),
}

/// Pick the LOD band from the computed near distances and the sub-slot's radius tiers, with the game's
/// one-step upgrade hysteresis. Port of the "big if-else" in `FUN_00735640` (v1.0 `0x7358cf..0x735a9f`).
///
/// - `near_2d_sq`: smallest 2D-squared distance to any valid reference point.
/// - `near_3d_sq`: smallest 3D-squared distance to any valid reference point.
/// - `inner_sq`: `(sub_slot.inner_radius)^2`.
/// - `lod_bytes`: three per-sub-slot LOD-radius bytes `(lo, mid, hi)` (`cell[0x140 + i*3..][0..3]`).
/// - `base_u16`: the per-sub-slot base LOD-radius word (`cell[0x1a0 + i*2]`).
/// - `margin`: `cfg.margin`.
/// - `old_state`: `cell[0x80 + i]`.
///
/// Returns 0 if no band is picked (the game's "no high bits set" path, which skips the write).
pub fn select_lod_band(near_2d_sq: f32, near_3d_sq: f32, inner_sq: f32, lod_bytes: (u8, u8, u8), base_u16: u16, margin: f32, old_state: u8) -> u8 {
    // If the nearest 2D is at least the inner radius, keep the current band if we were ever in one.
    if inner_sq <= near_2d_sq {
        return if (old_state & 0xf0) != 0 { 0x10 } else { 0 };
    }
    // Inside the inner radius: pick the band by thresholds on 3D distance, with each hysteresis band
    // capping the natural tier at one step below when the old state hadn't reached that tier yet.
    let b1 = f32::from(base_u16) + f32::from(lod_bytes.0); // base + cell[0x140 + i*3]
    let r1 = margin + b1;
    if r1 * r1 <= near_3d_sq {
        return 0x10;
    }
    if b1 * b1 <= near_3d_sq {
        return if old_state < 0x20 { 0x10 } else { 0x20 };
    }
    let b2 = f32::from(base_u16) + f32::from(lod_bytes.1); // base + cell[0x141 + i*3]
    let r2 = margin + b2;
    if r2 * r2 <= near_3d_sq {
        return 0x20;
    }
    if b2 * b2 <= near_3d_sq {
        return if old_state < 0x30 { 0x20 } else { 0x30 };
    }
    let b3 = f32::from(base_u16) + f32::from(lod_bytes.2); // base + cell[0x142 + i*3]
    let r3 = margin + b3;
    if r3 * r3 <= near_3d_sq {
        return 0x30;
    }
    if b3 * b3 <= near_3d_sq {
        return if old_state < 0x40 { 0x30 } else { 0x40 };
    }
    // Below b3 — closest tier. No hysteresis: jumps straight to 0x40 from any state.
    0x40
}

/// Context the per-cell test needs: config, reference array, cell world coordinates, fade cutoff.
pub struct SlotContext<'a> {
    pub cfg: &'a Config,
    pub arr: &'a DistanceBlock,
    /// World X of this cell: `cfg.origin_x + (col * cfg.cell_step)` as f32.
    pub world_x: f32,
    /// World Z of this cell: `cfg.origin_z + (row * cfg.cell_step)` as f32.
    pub world_z: f32,
    /// A sub-slot with a packed-radius key below this is faded out.
    pub fade_threshold: i32,
}

/// The per-sub-slot computation inside `FUN_00735640`'s main loop. Does not touch global state; the caller
/// emits any change entries via a [`Sink`].
pub fn per_sub_slot(ctx: &SlotContext<'_>, cell: &Cell, i: usize) -> SlotOutcome {
    let key = cell.sub_slot_key(i);
    let old = cell.state(i);
    let packed = (key & 0x3fff) as i32;
    if packed < ctx.fade_threshold {
        return SlotOutcome::Faded;
    }

    let cfg = ctx.cfg;
    let sub_step = cfg.sub_step();
    let sub_x = f32::from(cell.sub_slot_x_byte(i)) * sub_step + ctx.world_x;
    let sub_z = f32::from(cell.sub_slot_y_byte(i)) * sub_step + ctx.world_z;
    // Sub-slot vertical: high 2 bits of `key` shifted into byte lane + y-contrib byte + y_origin.
    let sub_y_int = (i32::from(key >> 14) * 256) + i32::from(cell.y_contrib(i)) + i32::from(cfg.y_origin());
    let sub_y = sub_y_int as f32;

    let inner_radius = (packed as f32) * cfg.inner_scale();
    let outer_radius = inner_radius + cfg.margin();
    let outer_sq = outer_radius * outer_radius;
    let inner_sq = inner_radius * inner_radius;

    let mut acc: u8 = 0;
    let mut near_2d_sq = EMPTY;
    let mut near_3d_sq = EMPTY;
    let inner_count = cfg.inner_count().max(0) as usize;
    for j in 0..inner_count {
        let idx = cfg.inner_indices()[j];
        let p = &ctx.arr.points_a[idx as usize];
        let dx = sub_x - p.x;
        let dz = sub_z - p.z;
        let sq_2d = dx * dx + dz * dz;
        if sq_2d < outer_sq && (sq_2d < inner_sq || (old & 0xf0) != 0) {
            // The bit is set regardless of the gate; the gate only guards the distance-tracker update.
            acc |= 1u8.wrapping_shl(u32::from(idx) & 31);
            if cfg.inner_gates()[idx as usize] != 0 {
                if sq_2d < near_2d_sq {
                    near_2d_sq = sq_2d;
                }
                let dy = sub_y - p.y;
                let sq_3d = dy * dy + sq_2d;
                if sq_3d < near_3d_sq {
                    near_3d_sq = sq_3d;
                }
            }
        }
    }

    if near_2d_sq < outer_sq {
        acc |= select_lod_band(near_2d_sq, near_3d_sq, inner_sq, cell.lod_bytes(i), cell.lod_base(i), cfg.margin(), old);
    }

    if (acc & 0xf0) == 0 {
        return SlotOutcome::NoBand;
    }
    SlotOutcome::Band(acc)
}

/// `FUN_00735BD0` + `FUN_00735C80` port: iterate every cell this job invocation covers, compute each cell's
/// outer-grid AABB from `cell_id`, call [`cell_group_min`] to get the group-min threshold, then run
/// [`per_cell_test`] against that threshold. Pure: reads borrowed arguments, writes to `sink` only.
///
/// `cell_size` is `2 << (cfg.shift() - 1)` as `f32`, which the handler normally caches in a global
/// (`DAT_00F79520`); the port recomputes it from `cfg` so no global state is needed.
pub fn job_17_run<S: Sink>(cells: &[&Cell], cfg: &Config, arr: &DistanceBlock, sink: &mut S) {
    let outer = cfg.outer_grid_width();
    if outer <= 0 {
        return;
    }
    let shift = cfg.shift();
    // `2 << (shift - 1)` as `f32`. The game computes it with `shl edx, cl; fild [esp]` so the result is
    // integer-exact for any `shift <= 31`; saturating_sub guards the undefined `shl` by `-1` case.
    let cell_size = (2u32 << u32::from(shift).saturating_sub(1)) as f32;
    let origin_x = cfg.origin_x();
    let origin_z = cfg.origin_z();
    for cell in cells {
        let cell_id = cell.cell_id();
        let row = i32::from(cell_id) / i32::from(outer);
        let col = i32::from(cell_id) - row * i32::from(outer);
        let world_x = origin_x + (col as f32) * cell_size;
        let world_z = origin_z + (row as f32) * cell_size;
        let bb = Box2D { xmin: world_x, zmin: world_z, xmax: world_x + cell_size, zmax: world_z + cell_size };
        let mut out = [0.0f32; 4];
        let threshold = cell_group_min(&mut out, arr, &bb);
        per_cell_test(cell, cfg, arr, threshold, sink);
    }
}

/// The outer loop of `FUN_00735640`: computes the cell's world coordinates, derives the fade cutoff from
/// the group-min threshold, then walks each sub-slot and emits a [`Change`] when the state transitions.
///
/// This is a pure function: it reads `cell`, `cfg`, `arr`, writes to `sink`, and does not touch the game's
/// globals. The caller is responsible for committing new states back onto the cell (the game does this
/// inline inside the critical section; verification mode defers it until the records are compared).
pub fn per_cell_test<S: Sink>(cell: &Cell, cfg: &Config, arr: &DistanceBlock, threshold: f32, sink: &mut S) {
    let cell_id = cell.cell_id();
    let grid_dim = cfg.grid_dim();
    if grid_dim <= 0 {
        return;
    }
    let row = i32::from(cell_id) / i32::from(grid_dim);
    let col = i32::from(cell_id) - i32::from(grid_dim) * row;
    let step = cfg.cell_step();
    let world_x = cfg.origin_x() + (col * step) as f32;
    let world_z = cfg.origin_z() + (row * step) as f32;
    // Fade threshold: `(threshold - margin)` truncated toward zero (the game's `cvttsd2si`).
    let fade_threshold = (threshold - cfg.margin()) as i32;

    let ctx = SlotContext { cfg, arr, world_x, world_z, fade_threshold };
    let count = cell.sub_slot_count().max(0) as usize;
    for i in 0..count {
        let old = cell.state(i);
        match per_sub_slot(&ctx, cell, i) {
            SlotOutcome::Faded => {
                if old != 0 {
                    sink.push(Change { entity_id: cell.entity_id(i), old_new: u16::from(old), extra: [0, 0] });
                }
            }
            SlotOutcome::NoBand => {}
            SlotOutcome::Band(new) => {
                if old != new {
                    sink.push(Change { entity_id: cell.entity_id(i), old_new: u16::from(old) | (u16::from(new) << 8), extra: [0, 0] });
                }
            }
        }
    }
}

/// Where the port writes its results. In production this is the game's live change-list; in verification it
/// is a shadow buffer, which [`verify`] compares against the game's recorded one.
pub trait Sink {
    fn push(&mut self, c: Change);
}

/// The "infinity" sentinel the inner helpers use when a slot is empty (`DAT_00BEA9AC = 0x7F7FFFFF`, which is
/// bit-identical to `f32::MAX`). The game loads it as a float from `.rdata`.
pub const EMPTY: f32 = f32::MAX;

/// A reference point as the game stores it: a 12-byte `(x, y, z)` triple. The 2D helpers read only `x` and
/// `z`; the 3D path (`per_sub_slot`) reads `y` too.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// A 2D axis-aligned box as the game stores it, in `(xmin, zmin, xmax, zmax)` word order (`[0, 4, 8, 12]`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Box2D {
    pub xmin: f32,
    pub zmin: f32,
    pub xmax: f32,
    pub zmax: f32,
}

/// Chebyshev (L∞) distance from a point to a 2D box, port of `FUN_00735480` (v1.0).
///
/// Returns `max(dx, dz)` where `dx = max(0, max(xmin - x, x - xmax))` and likewise for `dz`. The game
/// expresses this as two chained compares per axis plus one `max`; the arithmetic is bit-identical in `f32`
/// because every intermediate is a difference of two `f32`s (which is representable in `f32`), the x87
/// 80-bit internals do not introduce extra precision.
pub fn chebyshev_box_distance(p: &Point, b: &Box2D) -> f32 {
    let dx = if p.x < b.xmin {
        b.xmin - p.x
    } else if p.x > b.xmax {
        p.x - b.xmax
    } else {
        0.0
    };
    let dz = if p.z < b.zmin {
        b.zmin - p.z
    } else if p.z > b.zmax {
        p.z - b.zmax
    } else {
        0.0
    };
    // `max` matches `FUN_004076B0`: `a if a > b else b` (NaN-preserving on x87).
    if dx > dz {
        dx
    } else {
        dz
    }
}

/// The 4-point / 2-group block the game walks in `FUN_00735580`: four A-points, four A-gate bytes, four
/// B-points, four B-gate bytes, 0x68 = 104 bytes in all. The block lives in `DAT_00F7951C`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DistanceBlock {
    pub points_a: [Point; 4],
    pub gates_a: [u8; 4],
    pub points_b: [Point; 4],
    pub gates_b: [u8; 4],
}

/// For each of 4 slots, min of two Chebyshev distances (A- and B-point to the box, gated), then the min
/// across all 4; writes each slot's distance into `out[0..4]` and returns the overall min. Port of
/// `FUN_00735580` (v1.0).
///
/// Gating semantics (as the game writes them):
/// - `out[i]` starts at [`EMPTY`].
/// - If `gates_a[i] != 0`, `out[i] = chebyshev(points_a[i], box)`.
/// - If `gates_b[i] != 0`, `out[i] = min(chebyshev(points_b[i], box), out[i])`.
/// - The returned value is the min of `out[0..4]`.
pub fn cell_group_min(out: &mut [f32; 4], block: &DistanceBlock, b: &Box2D) -> f32 {
    let mut running = EMPTY;
    for i in 0..4 {
        let mut d = EMPTY;
        if block.gates_a[i] != 0 {
            d = chebyshev_box_distance(&block.points_a[i], b);
        }
        if block.gates_b[i] != 0 {
            let d_b = chebyshev_box_distance(&block.points_b[i], b);
            // `FUN_004076A0` is `min` (preserves NaN by only replacing when strictly less).
            if d_b < d {
                d = d_b;
            }
        }
        out[i] = d;
        if d < running {
            running = d;
        }
    }
    running
}

/// A sink that just records changes, for tests and the verification mode.
#[derive(Default, Debug)]
pub struct RecordingSink(pub Vec<Change>);

impl Sink for RecordingSink {
    fn push(&mut self, c: Change) {
        self.0.push(c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recording_sink_collects_the_changes_in_order() {
        let mut s = RecordingSink::default();
        s.push(Change { entity_id: 0x1234_5678, old_new: 0x1020, extra: [1, 2] });
        s.push(Change { entity_id: 0x9abc_def0, old_new: 0x2030, extra: [3, 4] });
        assert_eq!(s.0.len(), 2);
        assert_eq!(s.0[0].entity_id, 0x1234_5678);
        assert_eq!(s.0[1].old_new, 0x2030);
    }

    const BOX: Box2D = Box2D { xmin: -10.0, zmin: -5.0, xmax: 20.0, zmax: 15.0 };

    fn pt(x: f32, z: f32) -> Point {
        Point { x, y: 0.0, z }
    }

    #[test]
    fn chebyshev_is_zero_inside_the_box() {
        assert_eq!(chebyshev_box_distance(&pt(0.0, 0.0), &BOX), 0.0);
        assert_eq!(chebyshev_box_distance(&pt(-10.0, -5.0), &BOX), 0.0);
        assert_eq!(chebyshev_box_distance(&pt(20.0, 15.0), &BOX), 0.0);
    }

    #[test]
    fn chebyshev_is_the_perpendicular_distance_outside_an_edge() {
        // left of box: dx = xmin - x = 5; dz = 0; max = 5
        assert_eq!(chebyshev_box_distance(&pt(-15.0, 0.0), &BOX), 5.0);
        // right of box: dx = x - xmax = 10; dz = 0; max = 10
        assert_eq!(chebyshev_box_distance(&pt(30.0, 0.0), &BOX), 10.0);
        // below box: dx = 0; dz = zmin - z = 3; max = 3
        assert_eq!(chebyshev_box_distance(&pt(0.0, -8.0), &BOX), 3.0);
        // above box: dx = 0; dz = z - zmax = 7; max = 7
        assert_eq!(chebyshev_box_distance(&pt(0.0, 22.0), &BOX), 7.0);
    }

    #[test]
    fn chebyshev_is_the_larger_of_the_two_distances_outside_a_corner() {
        // top-right corner, (30, 22): dx = 10, dz = 7, max = 10
        assert_eq!(chebyshev_box_distance(&pt(30.0, 22.0), &BOX), 10.0);
        // bottom-left corner, (-15, -8): dx = 5, dz = 3, max = 5
        assert_eq!(chebyshev_box_distance(&pt(-15.0, -8.0), &BOX), 5.0);
    }

    #[test]
    fn cell_group_min_gates_decide_which_points_contribute() {
        let block = DistanceBlock {
            points_a: [pt(30.0, 0.0), pt(0.0, 22.0), pt(0.0, 0.0), pt(-100.0, 0.0)],
            gates_a: [1, 1, 0, 1],
            points_b: [pt(-15.0, 0.0), pt(30.0, 22.0), pt(0.0, 0.0), pt(-100.0, 0.0)],
            gates_b: [1, 0, 1, 0],
        };
        let mut out = [0.0f32; 4];
        let min = cell_group_min(&mut out, &block, &BOX);
        // Slot 0: A=10, B=5 → min=5
        // Slot 1: A=7, B skipped → 7
        // Slot 2: A skipped, B=0 → 0
        // Slot 3: A=90, B skipped → 90
        assert_eq!(out, [5.0, 7.0, 0.0, 90.0]);
        assert_eq!(min, 0.0);
    }

    const B: f32 = 100.0; // base radius word
    const LODS: (u8, u8, u8) = (50, 30, 10); // the three LOD-radius bytes: b1=150, b2=130, b3=110
    const MARGIN: f32 = 5.0; // r_k = b_k + 5: r1=155, r2=135, r3=115

    #[test]
    fn select_lod_band_returns_zero_when_outside_inner_and_old_was_zero() {
        // 2D radius above inner: outer LOD only applies if we were already in a band.
        assert_eq!(select_lod_band(400.0, 400.0, 100.0, LODS, B as u16, MARGIN, 0x00), 0);
        assert_eq!(select_lod_band(400.0, 400.0, 100.0, LODS, B as u16, MARGIN, 0x10), 0x10);
        assert_eq!(select_lod_band(400.0, 400.0, 100.0, LODS, B as u16, MARGIN, 0x40), 0x10);
    }

    #[test]
    fn select_lod_band_covers_each_distance_tier() {
        // Pick near_3d_sq to fall in the stable region of each tier (not in the hysteresis bands); old_state
        // 0x40 so hysteresis never caps.
        // Tier thresholds (squared): r1=24025, b1=22500, r2=18225, b2=16900, r3=13225, b3=12100.
        let (inner_sq, old) = (10_000.0, 0x40);
        assert_eq!(select_lod_band(0.0, 30_000.0, inner_sq, LODS, B as u16, MARGIN, old), 0x10, "far side of r1");
        assert_eq!(select_lod_band(0.0, 20_000.0, inner_sq, LODS, B as u16, MARGIN, old), 0x20, "in b1..r2 (natural 0x20)");
        assert_eq!(select_lod_band(0.0, 14_000.0, inner_sq, LODS, B as u16, MARGIN, old), 0x30, "in b2..r3 (natural 0x30)");
        assert_eq!(select_lod_band(0.0, 5_000.0, inner_sq, LODS, B as u16, MARGIN, old), 0x40, "inside b3 (natural 0x40)");
    }

    #[test]
    fn select_lod_band_hysteresis_bands_drop_one_tier_when_old_hasnt_reached_the_natural_one() {
        let inner_sq = 10_000.0;
        // r3..b3 band: natural tier is 0x40. If old hadn't reached 0x40, drop to 0x30.
        for (old, want) in [(0x00, 0x30), (0x10, 0x30), (0x20, 0x30), (0x30, 0x30), (0x40, 0x40)] {
            assert_eq!(select_lod_band(0.0, 12_500.0, inner_sq, LODS, B as u16, MARGIN, old), want);
        }
        // r2..b2 band: natural tier is 0x30. If old < 0x30, drop to 0x20.
        for (old, want) in [(0x00, 0x20), (0x10, 0x20), (0x20, 0x20), (0x30, 0x30), (0x40, 0x30)] {
            assert_eq!(select_lod_band(0.0, 17_500.0, inner_sq, LODS, B as u16, MARGIN, old), want);
        }
        // r1..b1 band: natural tier is 0x20. If old < 0x20, drop to 0x10.
        for (old, want) in [(0x00, 0x10), (0x10, 0x10), (0x20, 0x20), (0x30, 0x20), (0x40, 0x20)] {
            assert_eq!(select_lod_band(0.0, 23_000.0, inner_sq, LODS, B as u16, MARGIN, old), want);
        }
    }

    #[test]
    fn select_lod_band_snaps_to_0x40_inside_the_closest_tier() {
        // Below b3 there is no hysteresis — a sub-slot that teleports in jumps straight to the top band.
        let inner_sq = 10_000.0;
        for old in [0x00, 0x10, 0x20, 0x30, 0x40] {
            assert_eq!(select_lod_band(0.0, 5_000.0, inner_sq, LODS, B as u16, MARGIN, old), 0x40);
        }
    }

    #[test]
    fn cell_group_min_falls_through_to_the_empty_sentinel_when_both_gates_are_zero() {
        let block = DistanceBlock {
            points_a: [pt(0.0, 0.0); 4],
            gates_a: [0; 4],
            points_b: [pt(0.0, 0.0); 4],
            gates_b: [0; 4],
        };
        let mut out = [0.0f32; 4];
        let min = cell_group_min(&mut out, &block, &BOX);
        assert_eq!(out, [EMPTY; 4]);
        assert_eq!(min, EMPTY);
    }

    /// Builder for test configs; only the fields a given test cares about are set.
    #[derive(Default)]
    struct CfgBuild {
        grid_dim: i16,
        inner_count: i16,
        origin_x: f32,
        origin_z: f32,
        cell_step: i32,
        sub_step: f32,
        inner_scale: f32,
        margin: f32,
        gates: [u8; 4],
        indices: [u8; 4],
    }

    impl CfgBuild {
        fn build(self) -> Config {
            let mut b = [0u8; 0x60];
            b[0x06..0x08].copy_from_slice(&self.grid_dim.to_le_bytes());
            b[0x0a..0x0c].copy_from_slice(&self.inner_count.to_le_bytes());
            b[0x0c..0x10].copy_from_slice(&self.origin_x.to_le_bytes());
            b[0x14..0x18].copy_from_slice(&self.origin_z.to_le_bytes());
            b[0x24..0x28].copy_from_slice(&self.cell_step.to_le_bytes());
            b[0x28..0x2c].copy_from_slice(&self.sub_step.to_le_bytes());
            b[0x38..0x3c].copy_from_slice(&self.inner_scale.to_le_bytes());
            b[0x3c..0x40].copy_from_slice(&self.margin.to_le_bytes());
            b[0x40..0x44].copy_from_slice(&self.gates);
            b[0x44..0x48].copy_from_slice(&self.indices);
            Config(b)
        }
    }

    /// A config with grid=1 cell_step=100 sub_step=1 inner_scale=10 margin=5, gate at idx 0.
    fn basic_cfg(grid_dim: i16, cell_step: i32, origin_x: f32, origin_z: f32) -> Config {
        CfgBuild { grid_dim, inner_count: 1, origin_x, origin_z, cell_step, sub_step: 1.0, inner_scale: 10.0, margin: 5.0, gates: [1, 0, 0, 0], ..Default::default() }.build()
    }

    /// `basic_cfg` with grid_dim and cell_step aligned to the outer grid (so `per_cell_test`'s world coords
    /// agree with `job_17_run`'s), plus shift (sets cell_size) and outer_grid_width.
    fn outer_cfg(outer: i16, shift: u8) -> Config {
        let cell_size = (2u32 << u32::from(shift).saturating_sub(1)) as i32;
        let mut c = basic_cfg(outer, cell_size, 0.0, 0.0);
        c.0[0x04] = shift;
        c.0[0x08..0x0a].copy_from_slice(&outer.to_le_bytes());
        c
    }

    fn mk_cell(cell_id: i16, sub_slot_count: i16) -> Box<Cell> {
        let mut c = Box::new(Cell([0; 0x1f0]));
        c.0[0x1e4..0x1e6].copy_from_slice(&sub_slot_count.to_le_bytes());
        c.0[0x1e6..0x1e8].copy_from_slice(&cell_id.to_le_bytes());
        c
    }

    #[derive(Default)]
    struct Slot {
        entity_id: u32,
        state: u8,
        x_byte: u8,
        y_byte: u8,
        key: u16,
        y_contrib: u8,
        lod_bytes: (u8, u8, u8),
        lod_base: u16,
    }

    impl Slot {
        fn write_to(self, cell: &mut Cell, i: usize) {
            cell.0[i * 4..i * 4 + 4].copy_from_slice(&self.entity_id.to_le_bytes());
            cell.0[0x80 + i] = self.state;
            cell.0[0xa0 + i * 4] = self.x_byte;
            cell.0[0xa1 + i * 4] = self.y_byte;
            cell.0[0xa2 + i * 4..0xa2 + i * 4 + 2].copy_from_slice(&self.key.to_le_bytes());
            cell.0[0x120 + i] = self.y_contrib;
            let base = 0x140 + i * 3;
            cell.0[base] = self.lod_bytes.0;
            cell.0[base + 1] = self.lod_bytes.1;
            cell.0[base + 2] = self.lod_bytes.2;
            cell.0[0x1a0 + i * 2..0x1a0 + i * 2 + 2].copy_from_slice(&self.lod_base.to_le_bytes());
        }
    }

    fn mk_arr(points_a: [Point; 4]) -> DistanceBlock {
        DistanceBlock { points_a, gates_a: [1; 4], points_b: [Point::default(); 4], gates_b: [0; 4] }
    }

    #[test]
    fn per_sub_slot_fades_when_the_packed_key_is_below_threshold() {
        let cfg = basic_cfg(1, 100, 0.0, 0.0);
        let mut cell = mk_cell(0, 1);
        Slot { entity_id: 0xdead_beef, state: 0x40, key: 5, ..Default::default() }.write_to(&mut cell, 0);
        let arr = mk_arr([Point::default(); 4]);
        let ctx = SlotContext { cfg: &cfg, arr: &arr, world_x: 0.0, world_z: 0.0, fade_threshold: 10 };
        assert_eq!(per_sub_slot(&ctx, &cell, 0), SlotOutcome::Faded);
    }

    #[test]
    fn per_sub_slot_matches_the_closest_tier_when_sub_slot_touches_a_reference() {
        // Inner scale=10, packed=100 → inner_radius=1000, margin=5 → outer_sq=1_010_025.
        // Reference point at the sub-slot's exact world position → distance=0 → inside b3 → 0x40,
        // and the inner-reference bit `1 << idx` (idx=0 → 0x01) is also set, so the new state is 0x41.
        let cfg = basic_cfg(1, 100, 0.0, 0.0);
        let mut cell = mk_cell(0, 1);
        Slot { entity_id: 0x1111, x_byte: 10, y_byte: 20, key: 100, lod_bytes: (1, 1, 1), ..Default::default() }
            .write_to(&mut cell, 0);
        let p = Point { x: 10.0, y: 0.0, z: 20.0 };
        let arr = mk_arr([p, Point::default(), Point::default(), Point::default()]);
        let ctx = SlotContext { cfg: &cfg, arr: &arr, world_x: 0.0, world_z: 0.0, fade_threshold: 0 };
        assert_eq!(per_sub_slot(&ctx, &cell, 0), SlotOutcome::Band(0x41));
    }

    #[test]
    fn per_cell_test_emits_a_change_for_each_state_transition() {
        let cfg = basic_cfg(1, 100, 0.0, 0.0);
        let mut cell = mk_cell(0, 2);
        // Slot 0: becomes 0x41 (was 0x00): band 0x40 + inner-reference bit 0x01.
        Slot { entity_id: 0x1111, x_byte: 10, y_byte: 20, key: 100, lod_bytes: (1, 1, 1), ..Default::default() }
            .write_to(&mut cell, 0);
        // Slot 1: fades from 0x40 to 0 (key below fade cutoff).
        Slot { entity_id: 0x2222, state: 0x40, key: 5, lod_bytes: (1, 1, 1), ..Default::default() }.write_to(&mut cell, 1);
        let p = Point { x: 10.0, y: 0.0, z: 20.0 };
        let arr = mk_arr([p, Point::default(), Point::default(), Point::default()]);
        let mut sink = RecordingSink::default();
        per_cell_test(&cell, &cfg, &arr, 15.0, &mut sink);
        assert_eq!(sink.0.len(), 2);
        assert_eq!(sink.0[0].entity_id, 0x1111);
        assert_eq!(sink.0[0].old_new, 0x4100); // new=0x41 << 8, old=0x00
        assert_eq!(sink.0[1].entity_id, 0x2222);
        assert_eq!(sink.0[1].old_new, 0x0040); // old=0x40, new=0 (fade)
    }

    #[test]
    fn job_17_run_does_nothing_when_the_outer_grid_width_is_zero() {
        let cfg = basic_cfg(1, 100, 0.0, 0.0); // outer_grid_width = 0 (default)
        let cell = mk_cell(0, 1);
        let arr = mk_arr([Point::default(); 4]);
        let mut sink = RecordingSink::default();
        job_17_run(&[&cell], &cfg, &arr, &mut sink);
        assert!(sink.0.is_empty());
    }

    #[test]
    fn job_17_run_processes_each_cell_in_order() {
        // Outer grid 2x2, cell_size = 2^3 = 8.
        let cfg = outer_cfg(2, 3);
        let p = Point::default();
        let arr = mk_arr([p; 4]);
        let mut c0 = mk_cell(0, 1); // row 0, col 0 — world box (0, 0, 8, 8)
        Slot { entity_id: 0x0001, state: 0x40, key: 5, ..Default::default() }.write_to(&mut c0, 0);
        let mut c1 = mk_cell(1, 1); // row 0, col 1 — world box (8, 0, 16, 8)
        Slot { entity_id: 0x0002, state: 0x40, key: 5, ..Default::default() }.write_to(&mut c1, 0);
        let mut sink = RecordingSink::default();
        job_17_run(&[&c0, &c1], &cfg, &arr, &mut sink);
        // Each cell fades its single sub-slot from 0x40 to 0; order matches cells slice.
        assert_eq!(sink.0.len(), 2);
        assert_eq!(sink.0[0].entity_id, 0x0001);
        assert_eq!(sink.0[1].entity_id, 0x0002);
    }

    #[test]
    fn per_cell_test_computes_the_cell_world_position_from_grid_coords() {
        // grid_dim=4, cell_step=100: cell_id=5 → row=1, col=1 → world (origin + 100, origin + 100).
        let cfg = basic_cfg(4, 100, 200.0, 300.0);
        let mut cell = mk_cell(5, 1);
        Slot { entity_id: 0x3333, key: 100, lod_bytes: (1, 1, 1), ..Default::default() }.write_to(&mut cell, 0);
        let p = Point { x: 300.0, y: 0.0, z: 400.0 };
        let arr = mk_arr([p, Point::default(), Point::default(), Point::default()]);
        let mut sink = RecordingSink::default();
        per_cell_test(&cell, &cfg, &arr, 15.0, &mut sink);
        assert_eq!(sink.0.len(), 1);
        assert_eq!(sink.0[0].old_new, 0x4100);
    }
}

/// The verification harness that will compare a recorded run of the game's job 17 against the port.
///
/// Not yet implemented. Lands when the arithmetic lands.
pub mod verify {
    /// A recorded case: the inputs the game saw and the changes it wrote, serialised to a `.jobcase` file by
    /// the profiler when it runs in record mode.
    #[derive(Debug)]
    pub struct Case {
        _todo: (),
    }
}
