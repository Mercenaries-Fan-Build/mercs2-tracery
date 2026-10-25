//! The extra job workers ([`crate::route`]) on Windows: threads with Pimp CPU indices 2 and up that run the
//! routed jobs, the router the job trampolines hand routed jobs to, and the detour of the game's job wait
//! (`mercs2_formats::profiler_sites::Site::JobWait`) that keeps a worker's nested waits off the game's
//! two-slot control blocks.
//!
//! The game keeps a thread's Pimp state in the exe's implicit TLS block (`[fs:0x2C]` → `[0]`): `+0x08` the
//! CPU index, `+0x0C` the current job. A worker sets its CPU index once and points `+0x0C` at the job copy
//! it runs. The game's wait reads the calling thread's control block at `CPU * 300`, which exists for CPUs 0
//! and 1 only; on a worker the detour waits by running queued routed jobs instead
//! ([`route::wait_draining`]), and on every other thread it calls the game's wait unchanged.

use crate::jobs;
use crate::route::{self, Env, Queue};
use std::cell::Cell;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, mpsc};

static QUEUE: Queue = Queue::new();
/// The workers: thread id and Pimp CPU index.
static THREADS: Mutex<Vec<(u32, u32)>> = Mutex::new(Vec::new());
/// The game's job wait, past the detour (written by `m2_hook_attach`).
pub static WAIT_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
static FATAL: OnceLock<fn(&str) -> !> = OnceLock::new();

thread_local! {
    /// This thread's Pimp CPU index when it is a worker, else 0.
    static WORKER_CPU: Cell<u32> = const { Cell::new(0) };
}

/// The exe's implicit TLS block of this thread, or null when the thread has none.
fn exe_tls() -> *mut u8 {
    let array: usize;
    unsafe { core::arch::asm!("mov {0}, dword ptr fs:[0x2c]", out(reg) array, options(nostack, readonly, preserves_flags)) };
    if array == 0 {
        return core::ptr::null_mut();
    }
    unsafe { (array as *const *mut u8).read() }
}

fn fatal(msg: &str) -> ! {
    (FATAL.get().expect("set before any job is routed"))(msg)
}

struct GameEnv(*mut u8);

impl Env for GameEnv {
    fn set_current_job(&self, job: *mut u8) {
        unsafe { self.0.add(0xc).cast::<*mut u8>().write_volatile(job) };
    }
    unsafe fn call(&self, handler: usize, arg: usize) {
        let f: unsafe extern "C" fn(usize) = unsafe { core::mem::transmute(handler) };
        unsafe { f(arg) };
    }
    fn complete(&self, fence: u32) {
        // SAFETY: a job's counter is the game's live `LONG` (the dequeuing loop would add to it).
        unsafe { AtomicI32::from_ptr(fence as usize as *mut i32) }.fetch_add(1, Ordering::SeqCst);
    }
    fn yield_now(&self) {
        std::thread::yield_now();
    }
}

/// Run one routed job on this worker, inside its slot's zone.
fn run_routed(env: &GameEnv, j: route::Job) {
    let zone = jobs::ZONES[j.slot]();
    if let Some(t) = jobs::text(j.slot) {
        zone.emit_text(t);
    }
    unsafe { route::run(env, j) };
    drop(zone);
}

/// The router the trampolines call: queue the current job for a worker. A worker that reaches a routed
/// trampoline (a job calling a handler directly) runs it itself.
fn router(slot: usize, handler: usize, arg: usize) -> bool {
    if WORKER_CPU.get() != 0 {
        return false;
    }
    let tls = exe_tls();
    if tls.is_null() {
        fatal(&format!("job slot {slot} is routed, but the thread that dequeued it has no game TLS block"));
    }
    let current = unsafe { tls.add(0xc).cast::<*mut u8>().read_volatile() };
    if current.is_null() {
        fatal(&format!("job slot {slot} is routed, but it was called with no current job (TLS+0xC), so its completion counter is unknown"));
    }
    unsafe { route::forward(&QUEUE, current, slot, handler, arg) };
    true
}

/// The detour of the game's job wait (`void __cdecl (int fence[2])`).
pub unsafe extern "C" fn job_wait(fence: *const i32) {
    if WORKER_CPU.get() == 0 {
        let orig: unsafe extern "C" fn(*const i32) = unsafe { core::mem::transmute(WAIT_ORIGINAL.load(Ordering::Acquire)) };
        return unsafe { orig(fence) };
    }
    let env = GameEnv(exe_tls());
    let done = || unsafe { fence.read_volatile() == fence.add(1).read_volatile() };
    unsafe { route::wait_draining(&env, &QUEUE, done) };
}

/// Start `n` workers, CPU indices 2..2+n, and set the router. Returns one log line per worker.
pub fn start(n: u32, on_fatal: fn(&str) -> !) -> Result<Vec<String>, String> {
    if FATAL.set(on_fatal).is_err() {
        return Err("the job workers were started twice".into());
    }
    let mut lines = Vec::new();
    for i in 0..n {
        let cpu = route::FIRST_WORKER_CPU + i;
        let (tx, rx) = mpsc::channel::<Result<u32, String>>();
        std::thread::Builder::new()
            .name(format!("mercs2_profiler job worker {cpu}"))
            .spawn(move || {
                let tls = exe_tls();
                if tls.is_null() {
                    let _ = tx.send(Err(format!("job worker {cpu} has no game TLS block")));
                    return;
                }
                unsafe { tls.add(8).cast::<u32>().write_volatile(cpu) };
                WORKER_CPU.set(cpu);
                let tid = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
                if let Some(c) = tracy_client::Client::running() {
                    c.set_thread_name(&format!("job worker (Pimp cpu {cpu})"));
                }
                THREADS.lock().unwrap_or_else(|p| p.into_inner()).push((tid, cpu));
                let _ = tx.send(Ok(tid));
                let env = GameEnv(tls);
                loop {
                    run_routed(&env, QUEUE.pop());
                }
            })
            .map_err(|e| format!("starting job worker {cpu}: {e}"))?;
        let tid = rx.recv().map_err(|_| format!("job worker {cpu} ended before it started"))??;
        lines.push(format!("job worker {cpu}: thread {tid}"));
    }
    jobs::set_router(router);
    Ok(lines)
}

/// The workers' thread ids.
pub fn thread_ids() -> Vec<u32> {
    THREADS.lock().unwrap_or_else(|p| p.into_inner()).iter().map(|t| t.0).collect()
}

/// The routed jobs waiting for a worker.
pub fn queued() -> usize {
    QUEUE.len()
}
