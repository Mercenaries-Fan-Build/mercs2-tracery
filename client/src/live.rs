//! The port of jobtype `0xECAED415` as its handler. With shadow mode off, the trampoline of the jobtype's
//! slot ([`crate::jobs`]) runs [`game::run`] in place of the game's `FUN_00735E80`: it walks the current
//! thread's Pimp job element for the cells, the config block and the reference-point block
//! ([`walk_tls_job`]), runs [`cells::job_17_run`], and commits what the game's handler commits — one
//! ring entry per changed sub-slot, written under the ring's critical section, then the slot's new state
//! byte (`pc-v11`: `FUN_00735840` `0x00735CE9..0x00735D39`, the fade batch `FUN_00518830`, the state
//! stores at `0x00735D39` and `0x0073597A`, both after the push and outside the lock).
//!
//! What the game's handler also does and this does not: publish the config, reference-block and
//! cell-size globals (`0x00F78524`, `0x00F7851C`, `0x00F78520`, `0x01175260`). Every reader of those is
//! inside the handler's own call chain, so with the port as the handler nothing reads them.
//!
//! A ring entry is 16 bytes: `entity_id`, `old | new << 8`, and eight bytes the game leaves as stack
//! residue (the high word of `+4`, `+8`, `+12`). The one consumer that copies those bytes
//! (`FUN_00518890`, into `0x019C50F8`) is never read back, so the port writes zeros there. The game checks
//! no capacity on the ring; the port pushes exactly the entries the game would.

use crate::cells::{Cell, Change, Config, DistanceBlock, Sink};
use mercs2_formats::profiler_sites::CellsChangeRing;

/// The ring's lock, held around each entry write and counter bump.
pub trait Lock {
    fn enter(&mut self);
    fn leave(&mut self);
}

/// The change ring as the sink writes it: the counter and the entries.
#[derive(Clone, Copy)]
pub struct Ring {
    pub counter: *mut u32,
    pub base: *mut u8,
}

/// The sink that commits to the game: a ring entry as each slot changes, the state bytes when its cell
/// is done.
pub struct LiveSink<'a, L: Lock> {
    cells: &'a [*const Cell],
    ring: Ring,
    lock: L,
    /// The slots of the cell being tested whose state byte changes, with the new byte.
    pending: Vec<(usize, u8)>,
}

impl<'a, L: Lock> LiveSink<'a, L> {
    /// # Safety
    /// `ring` must be the game's live ring and `cells` the live cell blocks of the job in progress, for
    /// as long as the sink is used; `lock` must be the lock the game's writers of that ring take.
    pub unsafe fn new(cells: &'a [*const Cell], ring: Ring, lock: L) -> Self {
        LiveSink { cells, ring, lock, pending: Vec::new() }
    }
}

impl<L: Lock> Sink for LiveSink<'_, L> {
    fn push(&mut self, slot: usize, c: Change) {
        self.lock.enter();
        // SAFETY: `new`'s contract; the lock serialises the counter and the entry with the game's writers.
        unsafe {
            let n = self.ring.counter.read_volatile();
            let entry = self.ring.base.add(n as usize * CellsChangeRing::ENTRY_BYTES) as *mut u32;
            entry.write_volatile(c.entity_id);
            entry.add(1).write_volatile(u32::from(c.old_new));
            entry.add(2).write_volatile(c.extra[0]);
            entry.add(3).write_volatile(c.extra[1]);
            self.ring.counter.write_volatile(n.wrapping_add(1));
        }
        self.lock.leave();
        self.pending.push((slot, (c.old_new >> 8) as u8));
    }

    fn cell_done(&mut self, cell: usize) {
        let states = self.cells[cell] as *mut u8;
        for (slot, new) in self.pending.drain(..) {
            // SAFETY: `new`'s contract; the job owns its cells while it runs, as it does in the game.
            unsafe { states.add(Cell::STATE_AT + slot).write_volatile(new) };
        }
    }
}

/// What [`walk_tls_job`] found: the job element and the invocation's config and reference-point block.
pub struct Walk {
    pub job: *const u8,
    pub cfg: *const Config,
    pub arr: *const DistanceBlock,
}

/// Read the Pimp-thread's exe TLS block through `fs:[0x2C]` → the current thread's TLS array,
/// then the first entry. Only x86 has `fs:[0x2C]`; on any other host this returns NULL so the walk
/// fails in tests.
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

/// Walk the current thread's job element the way `FUN_00735E80` does (`TLS[0] + 0xC` → the element;
/// `+0x0C`, `+0x10`, `+0x14` sum to the slot after the cells in its pointer table at `+0x20`) and derive
/// the inputs of this invocation: the cells into `cells`, the config and reference-point block in the
/// result. The game's handler does the same on every invocation; one that finds no job, no config or
/// no block would fault in the game, so here it is an error.
///
/// # Safety
/// Must run on a Pimp thread with a live job in progress.
pub unsafe fn walk_tls_job(cells: &mut Vec<*const Cell>) -> Result<Walk, String> {
    cells.clear();
    let pcb = unsafe { pimp_control_block() };
    if pcb.is_null() {
        return Err("this thread's TLS holds no Pimp control block".into());
    }
    let job = unsafe { (pcb.add(0xc) as *const *const u8).read() };
    if job.is_null() {
        return Err("the Pimp control block has no job in progress".into());
    }
    let a = unsafe { (job.add(0xc) as *const u32).read() } as usize;
    let b = unsafe { (job.add(0x10) as *const u32).read() } as usize;
    let count = unsafe { (job.add(0x14) as *const u32).read() } as usize;
    if count > 1024 {
        return Err(format!("the job element at {:#x} lists {count} cells", job as usize));
    }
    let base = job as usize + 0x20;
    // sum3 = a + b + count: primary slot sits right after the cells in the per-slot table.
    let sum3 = a.wrapping_add(b).wrapping_add(count);
    let cfg = unsafe { ((base + sum3 * 8 + 8) as *const *const Config).read() };
    let arr = unsafe { ((base + sum3 * 8 + 0x10) as *const *const DistanceBlock).read() };
    if cfg.is_null() || arr.is_null() {
        return Err(format!("the job element at {:#x} has config {:#x} and reference block {:#x}", job as usize, cfg as usize, arr as usize));
    }
    for i in 0..count {
        let p = unsafe { ((base + (a + b + i) * 8) as *const *const Cell).read() };
        if p.is_null() {
            return Err(format!("the job element at {:#x} has a null cell {i} of {count}", job as usize));
        }
        cells.push(p);
    }
    Ok(Walk { job, cfg, arr })
}

#[cfg(all(windows, target_arch = "x86"))]
pub mod game {
    use super::*;
    use std::cell::RefCell;
    use windows_sys::Win32::System::Threading::{CRITICAL_SECTION, EnterCriticalSection, LeaveCriticalSection};

    /// The ring's `CRITICAL_SECTION` in the game ([`CellsChangeRing::critical_section`]).
    pub struct CriticalSection(*mut CRITICAL_SECTION);

    impl Lock for CriticalSection {
        fn enter(&mut self) {
            // SAFETY: the game's initialised critical section, for the process's lifetime.
            unsafe { EnterCriticalSection(self.0) };
        }

        fn leave(&mut self) {
            // SAFETY: as above; entered by this thread in `enter`.
            unsafe { LeaveCriticalSection(self.0) };
        }
    }

    thread_local! {
        /// The cells of the invocation in progress on this thread.
        static CELLS: RefCell<Vec<*const Cell>> = const { RefCell::new(Vec::new()) };
    }

    /// The handler: this thread's job, run by the port and committed to the game.
    ///
    /// # Safety
    /// Must be called as the jobtype's handler: on a Pimp thread, with the job element live, in place
    /// of the game's handler and never beside it.
    pub unsafe fn run() {
        let Some(ring) = crate::jobs::cells_change_ring() else {
            crate::win::fatal("jobtype 0xECAED415 ran as the port before its change ring was resolved");
        };
        CELLS.with_borrow_mut(|cells| {
            let walk = match unsafe { walk_tls_job(cells) } {
                Ok(w) => w,
                Err(e) => crate::win::fatal(&format!("jobtype 0xECAED415 as the port: {e}")),
            };
            let lock = CriticalSection(ring.critical_section() as usize as *mut CRITICAL_SECTION);
            let live = Ring { counter: ring.counter as usize as *mut u32, base: ring.ring as usize as *mut u8 };
            // SAFETY: the resolved ring site and the cells of the job in progress.
            let mut sink = unsafe { LiveSink::new(cells, live, lock) };
            // SAFETY: live game memory for the duration of the job, as for the game's handler.
            let refs: Vec<&Cell> = cells.iter().map(|&p| unsafe { &*p }).collect();
            crate::cells::job_17_run(&refs, unsafe { &*walk.cfg }, unsafe { &*walk.arr }, &mut sink);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A lock that logs, at each enter and leave, what the ring holds — so the test can tell what was
    /// written inside the lock from what was written outside it.
    struct Logging {
        ring: Ring,
        log: Rc<RefCell<Vec<String>>>,
    }

    impl Logging {
        fn snapshot(&self, at: &str) -> String {
            // SAFETY: the test's own buffers.
            let n = unsafe { self.ring.counter.read_volatile() };
            let entries: Vec<String> = (0..n)
                .map(|i| {
                    let e = unsafe { self.ring.base.add(i as usize * 16) as *const u32 };
                    format!("[{:#x} {:#x} {} {}]", unsafe { e.read() }, unsafe { e.add(1).read() }, unsafe { e.add(2).read() }, unsafe { e.add(3).read() })
                })
                .collect();
            format!("{at}: counter {n} {}", entries.join(" "))
        }
    }

    impl Lock for Logging {
        fn enter(&mut self) {
            let s = self.snapshot("enter");
            self.log.borrow_mut().push(s);
        }

        fn leave(&mut self) {
            let s = self.snapshot("leave");
            self.log.borrow_mut().push(s);
        }
    }

    fn cell_with(states: &[u8]) -> Box<Cell> {
        let mut c = Box::new(Cell([0; 0x1f0]));
        c.0[Cell::STATE_AT..Cell::STATE_AT + states.len()].copy_from_slice(states);
        c
    }

    #[test]
    fn a_push_writes_the_entry_and_bumps_the_counter_inside_the_lock_and_stores_the_state_when_the_cell_is_done() {
        let mut counter = 3u32;
        let mut buffer = vec![0xAAu8; 16 * 8];
        let ring = Ring { counter: &mut counter, base: buffer.as_mut_ptr() };
        let log = Rc::new(RefCell::new(Vec::new()));
        let c0 = cell_with(&[0x11, 0x40, 0x00]);
        let c1 = cell_with(&[0x21]);
        let cells = [&*c0 as *const Cell, &*c1 as *const Cell];
        let mut sink = unsafe { LiveSink::new(&cells, ring, Logging { ring, log: log.clone() }) };
        sink.push(0, Change { entity_id: 0x1234, old_new: 0x0011, extra: [0, 0] });
        sink.push(2, Change { entity_id: 0x5678, old_new: 0x4100, extra: [0, 0] });
        assert_eq!(c0.0[Cell::STATE_AT..Cell::STATE_AT + 3], [0x11, 0x40, 0x00], "no state is stored before the cell is done");
        sink.cell_done(0);
        assert_eq!(c0.0[Cell::STATE_AT..Cell::STATE_AT + 3], [0x00, 0x40, 0x41]);
        sink.push(0, Change { entity_id: 0x9abc, old_new: 0x3121, extra: [0, 0] });
        sink.cell_done(1);
        assert_eq!(c1.0[Cell::STATE_AT], 0x31);
        assert_eq!(c0.0[Cell::STATE_AT..Cell::STATE_AT + 3], [0x00, 0x40, 0x41], "cell 0 is left alone");
        assert_eq!(
            *log.borrow(),
            [
                "enter: counter 3 [0xaaaaaaaa 0xaaaaaaaa 2863311530 2863311530] [0xaaaaaaaa 0xaaaaaaaa 2863311530 2863311530] [0xaaaaaaaa 0xaaaaaaaa 2863311530 2863311530]",
                "leave: counter 4 [0xaaaaaaaa 0xaaaaaaaa 2863311530 2863311530] [0xaaaaaaaa 0xaaaaaaaa 2863311530 2863311530] [0xaaaaaaaa 0xaaaaaaaa 2863311530 2863311530] [0x1234 0x11 0 0]",
                "enter: counter 4 [0xaaaaaaaa 0xaaaaaaaa 2863311530 2863311530] [0xaaaaaaaa 0xaaaaaaaa 2863311530 2863311530] [0xaaaaaaaa 0xaaaaaaaa 2863311530 2863311530] [0x1234 0x11 0 0]",
                "leave: counter 5 [0xaaaaaaaa 0xaaaaaaaa 2863311530 2863311530] [0xaaaaaaaa 0xaaaaaaaa 2863311530 2863311530] [0xaaaaaaaa 0xaaaaaaaa 2863311530 2863311530] [0x1234 0x11 0 0] [0x5678 0x4100 0 0]",
                "enter: counter 5 [0xaaaaaaaa 0xaaaaaaaa 2863311530 2863311530] [0xaaaaaaaa 0xaaaaaaaa 2863311530 2863311530] [0xaaaaaaaa 0xaaaaaaaa 2863311530 2863311530] [0x1234 0x11 0 0] [0x5678 0x4100 0 0]",
                "leave: counter 6 [0xaaaaaaaa 0xaaaaaaaa 2863311530 2863311530] [0xaaaaaaaa 0xaaaaaaaa 2863311530 2863311530] [0xaaaaaaaa 0xaaaaaaaa 2863311530 2863311530] [0x1234 0x11 0 0] [0x5678 0x4100 0 0] [0x9abc 0x3121 0 0]",
            ]
        );
        assert_eq!(counter, 6);
        assert_eq!(&buffer[16 * 6..], &[0xAAu8; 16 * 2][..], "entries past the counter are untouched");
    }

    #[test]
    fn the_walk_fails_off_a_pimp_thread() {
        let mut cells = vec![core::ptr::null()];
        let e = unsafe { walk_tls_job(&mut cells) }.err().expect("no fs:[0x2c] on the host");
        assert!(e.contains("no Pimp control block"), "{e}");
        assert!(cells.is_empty(), "the walk clears the cells first");
    }
}
