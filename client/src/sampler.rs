//! The stack sampler: while a session records, a thread samples each of the session's threads in turn every
//! sample period (one tick): it suspends the thread, reads its EIP and ESP, copies its stack from ESP
//! upward, and resumes it. The stack words that point into the image's code sections are streamed with the
//! EIP, the tick and the thread id to the capture service as `<session>.samples`
//! (`mercs2_formats::prof_samples`), so the records of one tick show what every sampled thread was doing at
//! nearly the same moment.
//!
//! Between SuspendThread and ResumeThread the sampler only calls GetExitCodeThread and GetThreadContext and
//! copies into a buffer allocated before the loop: the suspended thread may hold the heap lock or any other
//! lock, so nothing in that window may allocate or lock. Everything else (filtering, encoding, sending)
//! happens after the thread is resumed. A thread found exited while suspended is not read (its stack may be
//! freed); it gets one exited record and is not sampled again. The copy is made only when ESP lies inside
//! the thread's stack reservation (`AllocationBase` ..= stack base): the span from ESP to the base is then
//! committed (the stack grew through it), so it reads only mapped memory. The time each thread is held
//! suspended is measured (total and longest), so a session reports what the sampler cost the game.

use crate::samples_stream::Sender;
use crate::threads;
use core::ffi::c_void;
use mercs2_formats::prof_samples::{FLAG_EXITED, FLAG_SCAN_LIMIT, Header, Record};
use mercs2_formats::profd::{LINE_MAX, Reply};
use std::io::{BufRead, BufReader, Read};
use std::net::{Shutdown, SocketAddrV4, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, STILL_ACTIVE};
use windows_sys::Win32::System::Diagnostics::Debug::{CONTEXT, CONTEXT_CONTROL_X86, GetThreadContext};
use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
use windows_sys::Win32::System::Performance::QueryPerformanceCounter;
use windows_sys::Win32::System::Threading::{
    GetExitCodeThread, OpenThread, ResumeThread, SuspendThread, THREAD_GET_CONTEXT, THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME,
};

/// Bytes of stack copied per sample, from ESP upward.
const SCAN_BYTES: usize = 64 * 1024;
const CONNECT: Duration = Duration::from_secs(3);
/// How long the service may take to answer once the stream ends.
const ANSWER: Duration = Duration::from_secs(60);

/// A thread sampled.
pub struct Target {
    thread: HANDLE,
    thread_id: u32,
    stack_base: u32,
    /// The start of the stack's reservation: the lowest address ESP can hold.
    stack_floor: u32,
}

// SAFETY: a thread handle may be used from any thread.
unsafe impl Send for Target {}

/// What one sample of a thread found.
enum Sample {
    /// EIP, how many bytes from ESP were copied (`true` when the copy stopped at the buffer's length before
    /// the stack base), and how many QPC ticks the thread was held suspended.
    Got { eip: u32, len: usize, limited: bool, held: u64 },
    Exited,
}

impl Target {
    /// Thread `tid`: its handle (suspend, resume, context) and its stack (`NT_TIB.StackBase` from its TEB,
    /// and the reservation below it).
    pub fn open(tid: u32) -> Result<Self, String> {
        let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION, 0, tid) };
        if thread.is_null() {
            return Err(format!("OpenThread on thread {tid}: {}", std::io::Error::last_os_error()));
        }
        // Owned from here, so every error below closes the handle.
        let mut t = Target { thread, thread_id: tid, stack_base: 0, stack_floor: 0 };
        let stack_base = threads::stack_base(thread).map_err(|e| format!("thread {tid}: {e}"))?;
        let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { core::mem::zeroed() };
        let got = unsafe { VirtualQuery((stack_base - 1) as *const c_void, &mut mbi, size_of::<MEMORY_BASIC_INFORMATION>()) };
        if got == 0 {
            return Err(format!("VirtualQuery of thread {tid}'s stack at {:#x}: {}", stack_base - 1, std::io::Error::last_os_error()));
        }
        let stack_floor = mbi.AllocationBase as u32;
        if stack_floor >= stack_base {
            return Err(format!("thread {tid}'s stack reservation starts at {stack_floor:#x}, not below its base {stack_base:#x}"));
        }
        t.stack_base = stack_base;
        t.stack_floor = stack_floor;
        Ok(t)
    }

    /// `[floor, base)` of the stack, for the log.
    pub fn stack(&self) -> (u32, u32) {
        (self.stack_floor, self.stack_base)
    }

    pub fn thread_id(&self) -> u32 {
        self.thread_id
    }

    fn exited(&self) -> Result<bool, String> {
        let mut code = 0u32;
        if unsafe { GetExitCodeThread(self.thread, &mut code) } == 0 {
            return Err(format!("GetExitCodeThread on thread {}: {}", self.thread_id, std::io::Error::last_os_error()));
        }
        Ok(code != STILL_ACTIVE as u32)
    }

    unsafe fn sample(&self, buf: &mut [u8]) -> Result<Sample, String> {
        let tid = self.thread_id;
        let held_from = qpc();
        if unsafe { SuspendThread(self.thread) } == u32::MAX {
            let e = std::io::Error::last_os_error();
            return if self.exited()? { Ok(Sample::Exited) } else { Err(format!("SuspendThread on thread {tid}: {e}")) };
        }
        // Suspended: no allocation and no locks until ResumeThread.
        let mut code = 0u32;
        let queried = unsafe { GetExitCodeThread(self.thread, &mut code) };
        let query_error = if queried == 0 { unsafe { GetLastError() } } else { 0 };
        let gone = queried != 0 && code != STILL_ACTIVE as u32;
        let mut ctx: CONTEXT = unsafe { core::mem::zeroed() };
        ctx.ContextFlags = CONTEXT_CONTROL_X86;
        let got = if queried != 0 && !gone { unsafe { GetThreadContext(self.thread, &mut ctx) } } else { 0 };
        let context_error = if queried != 0 && !gone && got == 0 { unsafe { GetLastError() } } else { 0 };
        let (eip, esp) = (ctx.Eip, ctx.Esp);
        let in_stack = esp >= self.stack_floor && esp < self.stack_base;
        let avail = if in_stack { (self.stack_base - esp) as usize } else { 0 };
        let len = avail.min(buf.len());
        if got != 0 && in_stack {
            unsafe { core::ptr::copy_nonoverlapping(esp as *const u8, buf.as_mut_ptr(), len) };
        }
        let resumed = unsafe { ResumeThread(self.thread) };
        let held = qpc().saturating_sub(held_from);
        if resumed == u32::MAX {
            return Err(format!("ResumeThread on thread {tid}: {}", std::io::Error::last_os_error()));
        }
        if queried == 0 {
            return Err(format!("GetExitCodeThread on thread {tid}: {}", std::io::Error::from_raw_os_error(query_error as i32)));
        }
        if gone {
            return Ok(Sample::Exited);
        }
        if got == 0 {
            return Err(format!("GetThreadContext on thread {tid}: {}", std::io::Error::from_raw_os_error(context_error as i32)));
        }
        if !in_stack {
            return Err(format!("thread {tid}'s ESP {esp:#x} is outside its stack {:#x}..{:#x}", self.stack_floor, self.stack_base));
        }
        Ok(Sample::Got { eip, len, limited: avail > buf.len(), held })
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.thread) };
    }
}

fn qpc() -> u64 {
    let mut t = 0i64;
    unsafe { QueryPerformanceCounter(&mut t) };
    t as u64
}

/// A samples stream that is recording.
pub struct Running {
    stop: Arc<AtomicBool>,
    join: JoinHandle<Result<Finished, String>>,
}

/// A stream that ended: the service's answer with the sampler's cost, and one cost line per thread.
pub struct Finished {
    pub answer: String,
    pub threads: Vec<String>,
}

impl Running {
    /// Stop sampling, end the stream, and return the service's answer.
    pub fn finish(self) -> Result<Finished, String> {
        self.stop.store(true, Ordering::Release);
        self.join.join().map_err(|_| "the sampler thread panicked".to_string())?
    }
}

/// Open the samples stream of `session` at the capture service and start sampling `targets` (the threads
/// `header` lists, in that order) every `period`, tagging each sample with the frame count in `frames`.
pub fn start(
    addr: SocketAddrV4,
    session: &str,
    header: Header,
    targets: Vec<Target>,
    period: Duration,
    frames: &'static AtomicU32,
) -> Result<Running, String> {
    let listed: Vec<u32> = header.threads.iter().map(|t| t.tid).collect();
    let opened: Vec<u32> = targets.iter().map(Target::thread_id).collect();
    if listed != opened {
        return Err(format!("the samples header lists threads {listed:?} but the sampler holds {opened:?}"));
    }
    let encoded = header.encode()?;
    let stream = TcpStream::connect_timeout(&addr.into(), CONNECT)
        .map_err(|e| format!("the capture service at {addr} did not accept the samples stream: {e}"))?;
    let mut w = Sender::new(stream.try_clone().map_err(|e| format!("cloning the samples stream: {e}"))?);
    w.open(session, &encoded)?;
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let join = std::thread::Builder::new()
        .name("mercs2_profiler sampler".into())
        .spawn(move || run(stream, w, header, targets, period, frames, s))
        .map_err(|e| format!("starting the sampler thread: {e}"))?;
    Ok(Running { stop, join })
}

/// What sampling one thread cost it.
#[derive(Default)]
struct Held {
    samples: u64,
    total: u64,
    longest: u64,
    exited_at: Option<u32>,
}

fn run(
    stream: TcpStream,
    mut w: Sender<TcpStream>,
    header: Header,
    targets: Vec<Target>,
    period: Duration,
    frames: &AtomicU32,
    stop: Arc<AtomicBool>,
) -> Result<Finished, String> {
    let began = qpc();
    let mut held: Vec<Held> = targets.iter().map(|_| Held::default()).collect();
    let mut tick = 0u32;
    let sampled = (|| {
        let mut stack = vec![0u8; SCAN_BYTES];
        let mut out = Vec::with_capacity(64 * 1024);
        let mut next = Instant::now();
        while !stop.load(Ordering::Acquire) {
            for (t, h) in targets.iter().zip(held.iter_mut()) {
                if h.exited_at.is_some() {
                    continue;
                }
                let frame = frames.load(Ordering::Relaxed);
                let rec = match unsafe { t.sample(&mut stack)? } {
                    Sample::Exited => {
                        h.exited_at = Some(tick);
                        Record { qpc: qpc(), tick, frame, tid: t.thread_id, eip: 0, flags: FLAG_EXITED, words: Vec::new() }
                    }
                    Sample::Got { eip, len, limited, held: ticks } => {
                        h.samples += 1;
                        h.total += ticks;
                        h.longest = h.longest.max(ticks);
                        let words = stack[..len & !3]
                            .chunks_exact(4)
                            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                            .filter(|&v| header.in_code(v))
                            .collect();
                        Record { qpc: qpc(), tick, frame, tid: t.thread_id, eip, flags: if limited { FLAG_SCAN_LIMIT } else { 0 }, words }
                    }
                };
                rec.encode_into(&mut out);
            }
            w.tick(&out)?;
            out.clear();
            tick = tick.wrapping_add(1);
            next += period;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else {
                // Behind (the host could not sleep that finely): sample now and resynchronise.
                next = now;
            }
        }
        Ok(())
    })();
    if let Err(e) = sampled {
        // End the stream at once, so the service closes the file and accepts STOP.
        let _ = stream.shutdown(Shutdown::Both);
        return Err(e);
    }
    let freq = header.qpc_frequency.max(1) as f64;
    let span = (qpc().saturating_sub(began) as f64 / freq).max(f64::EPSILON);
    let share = |h: &Held| 100.0 * h.total as f64 / freq / span;
    let us = |ticks: u64| ticks as f64 * 1e6 / freq;
    let lines: Vec<String> = targets
        .iter()
        .zip(&held)
        .map(|(t, h)| {
            let main = if t.thread_id == header.main_thread { "  main thread" } else { "" };
            let exited = h.exited_at.map_or(String::new(), |k| format!("  exited by tick {k}"));
            format!("tid {:>6}  held {:5.2}% of {span:.1} s  ({} samples, longest {:.0} µs){main}{exited}", t.thread_id, share(h), h.samples, us(h.longest))
        })
        .collect();
    let main = targets.iter().position(|t| t.thread_id == header.main_thread).map(|i| &held[i]);
    let worst = held.iter().map(share).fold(0.0f64, f64::max);
    let longest = held.iter().map(|h| h.longest).max().unwrap_or(0);
    let exited = held.iter().filter(|h| h.exited_at.is_some()).count();
    let cost = format!(
        "sampler: {} threads, {tick} ticks over {span:.1} s; held the main thread {:.2}%, any thread at most {worst:.2}%, longest {:.0} µs{}",
        targets.len(),
        main.map_or(0.0, share),
        us(longest),
        if exited > 0 { format!("; {exited} thread(s) exited during the session") } else { String::new() }
    );
    drop(w);
    stream.shutdown(Shutdown::Write).map_err(|e| format!("ending the samples stream: {e}"))?;
    stream.set_read_timeout(Some(ANSWER)).map_err(|e| format!("setting a read timeout: {e}"))?;
    let mut line = String::new();
    BufReader::new((&stream).take(LINE_MAX as u64 + 2)).read_line(&mut line).map_err(|e| format!("reading the samples answer: {e}"))?;
    if line.is_empty() {
        return Err("the capture service closed the samples stream without answering".into());
    }
    match Reply::parse(&line)? {
        Reply::Ok(m) => Ok(Finished { answer: format!("{m}; {cost}"), threads: lines }),
        Reply::Err(e) => Err(format!("{e}; {cost}")),
    }
}
