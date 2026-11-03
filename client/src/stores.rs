//! The frame's render stores against their sizes (`mercs2_formats::profiler_sites::FrameStores`): the packet
//! ring, the display list, the constant sidecar and the world-matrix table. The game fills all four while it
//! builds a frame and zeroes them in the begin-frame, so the begin-frame's detour reads each one's total for
//! the finished frame. The packet producer drops packets once the ring is full; nothing that writes the other
//! three checks its size, so a frame past one of them wrote outside its store.

use mercs2_formats::frame_capture::{FrameCapture, MATRIX_BYTES, Section};
use mercs2_formats::profiler_sites::FrameStores;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// One store's use in a frame, in its own unit, and its size in that unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Use {
    pub used: u32,
    pub size: u32,
}

impl Use {
    /// Whether the frame filled the store: the packet ring at its size, the others past theirs.
    fn full(&self, store: Store) -> bool {
        match store {
            Store::Packets => self.used >= self.size,
            _ => self.used > self.size,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Store {
    Packets,
    List,
    Sidecar,
    Matrices,
}

impl Store {
    pub const ALL: [Store; 4] = [Store::Packets, Store::List, Store::Sidecar, Store::Matrices];

    pub fn name(self) -> &'static str {
        match self {
            Store::Packets => "packets",
            Store::List => "display list",
            Store::Sidecar => "constants",
            Store::Matrices => "matrices",
        }
    }
}

static STORES: OnceLock<FrameStores> = OnceLock::new();
static LAST: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];
static PEAK: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];
static FULL: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

pub fn set(s: FrameStores) -> Result<(), String> {
    STORES.set(s).map_err(|_| "the frame stores were set twice".to_string())
}

pub fn stores() -> Option<&'static FrameStores> {
    STORES.get()
}

/// Each store's use in the finished frame: bytes for the display list (its cursor counts dwords) and the
/// sidecar, entries for the packets and the matrices (a `u16` counter).
///
/// # Safety
/// The addresses in `s` must be the running game's globals.
pub unsafe fn read(s: &FrameStores) -> [Use; 4] {
    let u32_at = |a: u32| unsafe { (a as usize as *const u32).read_volatile() };
    let u16_at = |a: u32| unsafe { (a as usize as *const u16).read_volatile() };
    [
        Use { used: u32_at(s.packets), size: s.packet_cap },
        Use { used: u32_at(s.list_dwords).saturating_mul(4), size: s.list_bytes },
        Use { used: u32_at(s.sidecar_bytes_used), size: s.sidecar_bytes },
        Use { used: u32::from(u16_at(s.matrices)), size: s.matrix_cap },
    ]
}

/// Copy the finished frame's stores: its half of the packet array and the matrix table (the parity global still
/// names it at the begin-frame's entry), and its display list and sidecar from their base pointers. Each store
/// copies `min(used, size)` units; `frame` is [`read`]'s result for the same frame.
///
/// # Safety
/// The addresses in `s` must be the running game's globals, read before the begin-frame runs.
pub unsafe fn capture(s: &FrameStores, frame: &[Use; 4], number: u32) -> FrameCapture {
    let u32_at = |a: u32| unsafe { (a as usize as *const u32).read_volatile() };
    let copy = |address: u32, n: usize| unsafe { std::slice::from_raw_parts(address as usize as *const u8, n).to_vec() };
    let parity = u32_at(s.parity);
    let section = |k: usize, address: u32, unit: usize| {
        let u = frame[k];
        Section { used: u.used, size: u.size, address, bytes: copy(address, u.used.min(u.size) as usize * unit) }
    };
    let packets = s.packet_array + parity * s.packet_cap * s.packet_bytes;
    let matrices = s.matrix_table + parity * s.matrix_cap * MATRIX_BYTES as u32;
    FrameCapture {
        frame: number,
        parity,
        sections: [
            section(0, packets, s.packet_bytes as usize),
            section(1, u32_at(s.list_base), 1),
            section(2, u32_at(s.sidecar_base), 1),
            section(3, matrices, MATRIX_BYTES),
        ],
    }
}

/// Record one finished frame; returns the stores this frame filled for the first time in the session.
pub fn record(frame: &[Use; 4]) -> Vec<Store> {
    let mut first = Vec::new();
    for (k, store) in Store::ALL.into_iter().enumerate() {
        let u = frame[k];
        LAST[k].store(u.used, Ordering::Relaxed);
        PEAK[k].fetch_max(u.used, Ordering::Relaxed);
        if u.full(store) && FULL[k].fetch_add(1, Ordering::Relaxed) == 0 {
            first.push(store);
        }
    }
    first
}

/// `(last, peak, frames full)` of each store, in [`Store::ALL`] order.
pub fn totals() -> [(u32, u32, u64); 4] {
    std::array::from_fn(|k| (LAST[k].load(Ordering::Relaxed), PEAK[k].load(Ordering::Relaxed), FULL[k].load(Ordering::Relaxed)))
}

/// One line per store: last frame, peak and frames full, against its size.
pub fn lines() -> Vec<String> {
    let Some(s) = stores() else { return Vec::new() };
    let sizes = [s.packet_cap, s.list_bytes, s.sidecar_bytes, s.matrix_cap];
    Store::ALL
        .into_iter()
        .zip(totals())
        .zip(sizes)
        .map(|((store, (last, peak, full)), size)| {
            let pct = |n: u32| 100.0 * f64::from(n) / f64::from(size.max(1));
            format!("{} {last} / {size} ({:.0}%), peak {peak} ({:.0}%), {full} frame(s) full", store.name(), pct(last), pct(peak))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_frame_counts_and_the_first_is_named_once() {
        let at = |p: u32, l: u32, c: u32, m: u32| [Use { used: p, size: 8192 }, Use { used: l, size: 0x50000 }, Use { used: c, size: 0x20000 }, Use { used: m, size: 0x1400 }];
        assert!(record(&at(100, 4, 96, 3)).is_empty());
        assert_eq!(record(&at(8192, 0x50000, 0x20001, 0x1400)), vec![Store::Packets, Store::Sidecar]);
        assert!(record(&at(8192, 0x50004, 0x20001, 0x1401)).iter().eq([Store::List, Store::Matrices].iter()));
        assert!(record(&at(8192, 8, 8, 8)).is_empty());
        let t = totals();
        assert_eq!(t[0], (8192, 8192, 3));
        assert_eq!(t[1], (8, 0x50004, 1));
        assert_eq!(t[2], (8, 0x20001, 2));
        assert_eq!(t[3], (8, 0x1401, 1));
    }
}
