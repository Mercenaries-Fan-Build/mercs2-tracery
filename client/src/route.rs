//! Job routing: running chosen Pimp jobtypes on extra worker threads instead of on the thread that
//! dequeued them.
//!
//! The game's job loops (`0x00876400` and the wait `0x008765CF`) take a 96-byte job from a ring, point the
//! thread's TLS+0xC at it, call `handler(job[2])`, and only then read `job[1]`, the completion counter, and
//! add 1 to it when it is not null. A routed job's trampoline ([`forward`]) copies the 96 bytes, writes 0
//! over the ring copy's counter, and queues the copy: the game's loop then skips its add, and the worker
//! that runs the copy ([`run`]) points its own TLS+0xC at the copy, calls the handler, and makes the one
//! add. No transient value is ever written to a counter, so whatever reads them (the wait's
//! `fence[0] == fence[1]`, the job graph's releases) sees the same sequence as without routing, only later.
//!
//! A worker that waits on sub-jobs runs queued jobs until the wait is satisfied ([`wait_draining`]), as the
//! game's wait runs its own CPU's queues. Workers carry their own Pimp CPU index (2 and up), so every
//! per-CPU structure a routed jobtype touches must have that many slots: [`ROUTABLE`] lists the jobtypes
//! whose reads have been checked, with the highest CPU index each tolerates.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};

/// The bytes of a ring element: a job and its inline parameters.
pub const JOB_BYTES: usize = 0x60;
/// The completion counter's offset in a job.
pub const FENCE_AT: usize = 4;
/// The first Pimp CPU index of an extra worker: 0 is the main thread, 1 the game's worker.
pub const FIRST_WORKER_CPU: u32 = 2;
/// The most extra workers: CPU indices 2..=9.
pub const MAX_WORKERS: u32 = 8;

/// A jobtype whose code has been read for per-CPU and shared state, and the highest Pimp CPU index it
/// tolerates.
pub struct Routable {
    pub hash: u32,
    pub max_cpu: u32,
    /// Where the analysis is written down.
    pub evidence: &'static str,
}

/// The jobtypes that may be routed. A jobtype is listed only once its code is read.
pub const ROUTABLE: &[Routable] = &[];

/// Check a configuration: every routed jobtype is in [`ROUTABLE`] and tolerates the highest CPU index of
/// `workers` workers, and routing has workers to route to.
pub fn check(workers: u32, routed: &[u32]) -> Result<(), String> {
    if workers > MAX_WORKERS {
        return Err(format!("job_workers = {workers}; at most {MAX_WORKERS}"));
    }
    if workers == 0 && !routed.is_empty() {
        return Err("route_jobtypes names jobtypes but job_workers = 0 leaves no thread to run them".into());
    }
    let top = FIRST_WORKER_CPU + workers.saturating_sub(1);
    for &h in routed {
        let r = ROUTABLE.iter().find(|r| r.hash == h).ok_or_else(|| format!("jobtype {h:#010x} has not been checked for routing (route::ROUTABLE)"))?;
        if workers > 0 && top > r.max_cpu {
            return Err(format!(
                "jobtype {h:#010x} tolerates Pimp CPU indices up to {} ({}); job_workers = {workers} uses up to {top}",
                r.max_cpu, r.evidence
            ));
        }
    }
    Ok(())
}

/// A routed job: its slot (for its zone), handler, argument and a copy of its 96 bytes.
pub struct Job {
    pub slot: usize,
    pub handler: usize,
    pub arg: usize,
    pub bytes: [u8; JOB_BYTES],
}

impl Job {
    pub fn fence(&self) -> u32 {
        u32::from_le_bytes(self.bytes[FENCE_AT..FENCE_AT + 4].try_into().expect("4 bytes"))
    }
}

/// The routed jobs not yet taken by a worker.
pub struct Queue {
    jobs: Mutex<VecDeque<Job>>,
    ready: Condvar,
}

impl Queue {
    pub const fn new() -> Self {
        Queue { jobs: Mutex::new(VecDeque::new()), ready: Condvar::new() }
    }

    pub fn push(&self, j: Job) {
        self.jobs.lock().unwrap_or_else(|p| p.into_inner()).push_back(j);
        self.ready.notify_one();
    }

    pub fn try_pop(&self) -> Option<Job> {
        self.jobs.lock().unwrap_or_else(|p| p.into_inner()).pop_front()
    }

    /// The next job, waiting for one.
    pub fn pop(&self) -> Job {
        let mut q = self.jobs.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if let Some(j) = q.pop_front() {
                return j;
            }
            q = self.ready.wait(q).unwrap_or_else(|p| p.into_inner());
        }
    }

    pub fn len(&self) -> usize {
        self.jobs.lock().unwrap_or_else(|p| p.into_inner()).len()
    }
}

impl Default for Queue {
    fn default() -> Self {
        Self::new()
    }
}

/// What running a job needs from the thread: the game's per-thread "current job" (TLS+0xC), the call, and
/// the completion add.
pub trait Env {
    fn set_current_job(&self, job: *mut u8);
    /// # Safety
    /// `handler` must be a job handler.
    unsafe fn call(&self, handler: usize, arg: usize);
    fn complete(&self, fence: u32);
    fn yield_now(&self);
}

/// Queue the job at `slot_bytes` (the ring element the dequeuing loop points TLS+0xC at) for a worker,
/// and null the ring element's completion counter so the loop does not add to it.
///
/// # Safety
/// `slot_bytes` must point at a live ring element of [`JOB_BYTES`] bytes.
pub unsafe fn forward(q: &Queue, slot_bytes: *mut u8, slot: usize, handler: usize, arg: usize) {
    let mut bytes = [0u8; JOB_BYTES];
    unsafe {
        core::ptr::copy_nonoverlapping(slot_bytes, bytes.as_mut_ptr(), JOB_BYTES);
        slot_bytes.add(FENCE_AT).cast::<u32>().write_unaligned(0);
    }
    q.push(Job { slot, handler, arg, bytes });
}

/// Run a routed job on this thread: the job's copy is the current job while its handler runs, then its
/// counter gets the add.
///
/// # Safety
/// `j.handler` must be a job handler.
pub unsafe fn run(env: &impl Env, mut j: Job) {
    env.set_current_job(j.bytes.as_mut_ptr());
    unsafe { env.call(j.handler, j.arg) };
    env.set_current_job(core::ptr::null_mut());
    let fence = j.fence();
    if fence != 0 {
        env.complete(fence);
    }
}

/// Wait on a worker until `done()`, running queued jobs meanwhile.
///
/// # Safety
/// Every queued job's handler must be a job handler.
pub unsafe fn wait_draining(env: &impl Env, q: &Queue, done: impl Fn() -> bool) {
    while !done() {
        match q.try_pop() {
            Some(j) => unsafe { run(env, j) },
            None => env.yield_now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn a_configuration_routes_only_checked_jobtypes_within_their_cpu_range() {
        assert!(check(1, &[]).is_ok());
        assert!(check(0, &[]).is_ok());
        assert!(check(9, &[]).unwrap_err().contains("at most 8"));
        assert!(check(0, &[0x1234]).unwrap_err().contains("no thread"));
        assert!(check(1, &[0xecae_d415]).unwrap_err().contains("not been checked"));
    }

    /// The model's handlers: each job records how many times it ran.
    static RAN: [AtomicU32; 64] = [const { AtomicU32::new(0) }; 64];
    unsafe extern "C" fn handler(arg: usize) {
        RAN[arg].fetch_add(1, Ordering::SeqCst);
        if arg % 3 == 0 {
            std::thread::yield_now();
        }
    }

    /// Fences addressed by id (the 32-bit game stores a pointer; ids keep the model 64-bit clean):
    /// `[done, expected]`, and every value `done` ever took.
    struct Fences {
        pairs: Vec<[AtomicU32; 2]>,
        seen: Mutex<Vec<Vec<u32>>>,
    }

    struct ModelEnv(Arc<Fences>);
    impl Env for ModelEnv {
        fn set_current_job(&self, _: *mut u8) {}
        unsafe fn call(&self, h: usize, arg: usize) {
            let f: unsafe extern "C" fn(usize) = unsafe { core::mem::transmute(h) };
            unsafe { f(arg) }
        }
        fn complete(&self, fence: u32) {
            let id = fence as usize - 1;
            let now = self.0.pairs[id][0].fetch_add(1, Ordering::SeqCst) + 1;
            self.0.seen.lock().unwrap()[id].push(now);
        }
        fn yield_now(&self) {
            std::thread::yield_now();
        }
    }

    /// The game's dispatch, modelled: point at the slot, call the table entry (here: route every second job,
    /// run the rest), then read the slot's counter and add when it is not null.
    #[test]
    fn routed_jobs_complete_their_fence_exactly_once() {
        const JOBS: usize = 60;
        const GROUPS: usize = 6;
        let fences = Arc::new(Fences {
            pairs: (0..GROUPS).map(|_| [AtomicU32::new(0), AtomicU32::new((JOBS / GROUPS) as u32)]).collect(),
            seen: Mutex::new(vec![Vec::new(); GROUPS]),
        });
        let q = Arc::new(Queue::new());
        let workers: Vec<_> = (0..3)
            .map(|_| {
                let (q, f) = (q.clone(), fences.clone());
                std::thread::spawn(move || {
                    let env = ModelEnv(f.clone());
                    // Each worker waits for every group, draining the queue, as a nested wait would.
                    for g in 0..GROUPS {
                        unsafe {
                            wait_draining(&env, &q, || f.pairs[g][0].load(Ordering::SeqCst) == f.pairs[g][1].load(Ordering::SeqCst))
                        };
                    }
                })
            })
            .collect();
        let env = ModelEnv(fences.clone());
        for job in 0..JOBS {
            let mut ring = [0u8; JOB_BYTES];
            let fence = (job % GROUPS + 1) as u32;
            ring[FENCE_AT..FENCE_AT + 4].copy_from_slice(&fence.to_le_bytes());
            ring[8..12].copy_from_slice(&(job as u32).to_le_bytes());
            if job % 2 == 0 {
                unsafe { forward(&q, ring.as_mut_ptr(), 0, handler as unsafe extern "C" fn(usize) as usize, job) };
            } else {
                unsafe { handler(job) };
            }
            let after = u32::from_le_bytes(ring[FENCE_AT..FENCE_AT + 4].try_into().unwrap());
            if after != 0 {
                env.complete(after);
            }
        }
        for w in workers {
            w.join().unwrap();
        }
        for (j, r) in RAN.iter().take(JOBS).enumerate() {
            assert_eq!(r.load(Ordering::SeqCst), 1, "job {j} ran once");
        }
        let seen = fences.seen.lock().unwrap();
        for (g, s) in seen.iter().enumerate() {
            let expected = (JOBS / GROUPS) as u32;
            assert_eq!(s.len() as u32, expected, "group {g}: one add per job");
            assert!(s.iter().all(|&v| v <= expected), "group {g} never passes expected: {s:?}");
            assert_eq!(*s.iter().max().unwrap(), expected, "group {g} reaches expected");
        }
        assert_eq!(q.len(), 0);
    }
}
