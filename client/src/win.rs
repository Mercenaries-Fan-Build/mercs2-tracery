//! The Windows side: `DllMain`, start-up, and the detours.
//!
//! Start-up runs on a thread started from `DllMain`, never under the loader lock:
//! 1. `mercs2_profiler.log` (m2-sdk.dll's per-module log) and the m2-sdk.dll version check;
//! 2. `mercs2_profiler.ini` (written with its defaults when missing) and the thread that sends the
//!    session's requests to the capture service;
//! 3. the profiler sites, resolved in the mapped game image once SecuROM has decrypted `.text`, and the
//!    image's code ranges and the exe file's sha256 for the samples header;
//! 4. the Tracy client (on demand: it records only while a capture is connected);
//! 5. the glyph atlas the overlay draws from;
//! 6. the RunFrame detour: one `RunFrame` zone and one frame mark per frame, the frame time the overlay
//!    shows, and the hotkey (a down edge while the game has focus) that starts or stops a session. Its
//!    first call takes the main thread's id and checks the sampler can open it and find its stack. On the
//!    first frame the game's device exists (the RenderSystem's `IDirect3DDevice9`), it detours the Present
//!    in that device's vtable, which draws the overlay. Every frame it also wraps the Pimp jobtypes
//!    registered since the last frame in Tracy zones ([`crate::jobs`]), marking the routed ones.
//!
//! With `job_workers` above 0, start-up first starts the extra job workers and detours the game's job wait
//! ([`crate::pool`]); with 0 neither exists and the game's job system is untouched but for the zones.
//!
//! A session is a Tracy capture and a samples stream: START, then the stream opens (when it cannot, the
//! capture is stopped again); on the next press the stream ends, then STOP. The stream samples the main
//! thread and every thread that started in the game's image (the job worker, the timer, the audio mixer,
//! ...) as the session started; threads started later are not sampled.
//!
//! Any failure is fatal: logged, shown in a message box, and the process exits; the game never runs with
//! a profiler that is half there. That includes a device whose Present changes to another implementation
//! after the first was detoured. The game presents through the RenderSystem device's Present
//! (`FUN_00748DA0`, from BeginSubmit `FUN_0074A890` and the render command interpreter `FUN_007518B0`),
//! but not on every RunFrame: start-up runs hundreds of frames without one. The first Present the detour
//! sees is logged with its frame.

use crate::config::{self, Config};
use crate::cpu;
use crate::threads;
use crate::overlay::{self, Atlas, Overlay, PresentFn};
use crate::sampler::{self, Target};
use crate::session::{self, Session};
use core::ffi::c_void;
use mercs2_formats::exe_image::{Image, parse_headers};
use mercs2_formats::prof_samples;
use sha2::{Digest, Sha256};
use mercs2_formats::keys::{self, Mods, vk};
use mercs2_formats::mod_runtime::{self as rt, ffi};
use mercs2_formats::profd::{self, LINE_MAX, Reply, Request};
use crate::crash;
use crate::jobs;
use crate::pool;
use mercs2_formats::profiler_sites::{self, JobTables, ProfilerSites, RENDER_SYSTEM_DEVICE, Site};
use std::cell::RefCell;
use std::ffi::CString;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddrV4, TcpStream};
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::RGNDATA;
use windows::core::HRESULT;
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{DisableThreadLibraryCalls, GetModuleFileNameW, GetModuleHandleA};
use windows_sys::Win32::System::Performance::QueryPerformanceFrequency;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows_sys::Win32::System::Threading::{ExitProcess, GetCurrentProcessId, GetCurrentThreadId};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId, MB_ICONERROR, MB_OK, MessageBoxW};

/// The m2-sdk.dll release this ASI is built against (manifest.yaml requires `^0.4`).
const SDK_BUILT: &str = "0.4.0";
const TITLE: &str = "mercs2-profiler";
/// How long start-up waits for SecuROM to decrypt the code the sites are found in.
const SITES_WAIT: Duration = Duration::from_secs(30);
const SITES_RETRY: Duration = Duration::from_millis(50);
/// The headers fit in the first page of the image.
const HEADER_BYTES: usize = 0x1000;

/// Weight of the newest frame in the overlay's average frame time.
const AVERAGE_WEIGHT: f32 = 0.05;
/// How long the capture service may take to accept a connection.
const SERVICE_CONNECT: Duration = Duration::from_secs(3);
/// How long each request may take to be answered: longer than the service's own connect (15 s) and save
/// (120 s) timeouts, so its answer, not ours, says what went wrong.
const START_ANSWER: Duration = Duration::from_secs(30);
const STOP_ANSWER: Duration = Duration::from_secs(150);

static CONFIG: OnceLock<Config> = OnceLock::new();
/// The main thread's id, as the first RunFrame saw it.
static MAIN_THREAD: OnceLock<u32> = OnceLock::new();
/// What every samples file's header holds but its threads and modules: the exe's sha256, the image base,
/// the code ranges, the QPC frequency and the sample period.
static SAMPLES_HEADER: OnceLock<prof_samples::Header> = OnceLock::new();
static SESSION: Mutex<Session> = Mutex::new(Session::new());
static REQUESTS: OnceLock<Mutex<mpsc::Sender<Request>>> = OnceLock::new();
static EPOCH: OnceLock<Instant> = OnceLock::new();
/// Whether the hotkey was down on the previous frame.
static HOTKEY_DOWN: AtomicBool = AtomicBool::new(false);

static MODULE: AtomicUsize = AtomicUsize::new(0);
static TRACY: OnceLock<tracy_client::Client> = OnceLock::new();
/// RunFrame's and Present's trampolines, written by `m2_hook_attach` before the detours can first run.
static RUN_FRAME: AtomicUsize = AtomicUsize::new(0);
static PRESENT: AtomicUsize = AtomicUsize::new(0);
/// The address of the RenderSystem pointer global, from `profiler_sites`.
static RENDER_SYSTEM: AtomicUsize = AtomicUsize::new(0);
/// The Pimp job system's count global and handler tables, from `profiler_sites`.
static JOBS: OnceLock<JobTables> = OnceLock::new();
/// The Present detoured (0 until the game's device exists).
static PRESENT_TARGET: AtomicUsize = AtomicUsize::new(0);
static ATLAS: OnceLock<Atlas> = OnceLock::new();
thread_local! {
    /// The overlay's device resources, on the thread that calls Present.
    static OVERLAY: RefCell<Option<Overlay>> = const { RefCell::new(None) };
}
static FRAMES: AtomicU32 = AtomicU32::new(0);
static PRESENTS: AtomicU32 = AtomicU32::new(0);
/// The last and the average RunFrame time, in milliseconds, as `f32` bits.
static FRAME_MS: AtomicU32 = AtomicU32::new(0);
static AVERAGE_MS: AtomicU32 = AtomicU32::new(0);

fn module() -> HMODULE {
    MODULE.load(Ordering::Acquire) as HMODULE
}

/// One line in `mercs2_profiler.log`.
fn log(line: &str) {
    let c = CString::new(line.replace('\0', " ")).expect("NULs replaced");
    unsafe { ffi::m2_logf_mod(module(), c"%s".as_ptr(), c.as_ptr()) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Log, show the reason, end the process.
fn fatal(msg: &str) -> ! {
    log(&format!("[!] mercs2_profiler: FATAL: {msg}"));
    let text = wide(&format!("{msg}\n\nThe game will now exit."));
    let title = wide(TITLE);
    unsafe {
        MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR);
        ExitProcess(1)
    }
}

/// The directory the ASI was loaded from.
fn module_dir() -> Result<PathBuf, String> {
    let mut buf = vec![0u16; 1024];
    loop {
        let n = unsafe { GetModuleFileNameW(module(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
        if n == 0 {
            return Err(format!("GetModuleFileNameW failed: {}", std::io::Error::last_os_error()));
        }
        if n < buf.len() {
            let path = PathBuf::from(std::ffi::OsString::from_wide(&buf[..n]));
            return path.parent().map(PathBuf::from).ok_or_else(|| format!("{} has no parent directory", path.display()));
        }
        buf.resize(buf.len() * 2, 0);
    }
}

fn mapped() -> Result<Image<'static>, String> {
    let base = unsafe { GetModuleHandleA(core::ptr::null()) } as *const u8;
    if base.is_null() {
        return Err("GetModuleHandleA(NULL) returned no module".into());
    }
    let header = unsafe { core::slice::from_raw_parts(base, HEADER_BYTES) };
    let (image_base, headers) = parse_headers(header).map_err(|e| format!("the game's headers: {e}"))?;
    Ok(unsafe { Image::from_mapped(base, image_base, headers) })
}

/// The sites, retried until SecuROM has decrypted `.text` or [`SITES_WAIT`] runs out.
fn resolve_sites() -> Result<ProfilerSites, String> {
    let start = Instant::now();
    loop {
        let last = match profiler_sites::resolve(&mapped()?) {
            Ok(s) => return Ok(s),
            Err(e) => e.to_string(),
        };
        if start.elapsed() >= SITES_WAIT {
            return Err(format!("after {} s: {last}", SITES_WAIT.as_secs()));
        }
        std::thread::sleep(SITES_RETRY);
    }
}

fn tracy() -> &'static tracy_client::Client {
    TRACY.get().expect("the detours are attached only after the Tracy client is started")
}

/// RunFrame (`void __cdecl (void)`): the frame inside a `RunFrame` zone, then the frame mark.
unsafe extern "C" fn run_frame() {
    let orig: unsafe extern "C" fn() = unsafe { core::mem::transmute(RUN_FRAME.load(Ordering::Acquire)) };
    if MAIN_THREAD.get().is_none() {
        let id = unsafe { GetCurrentThreadId() };
        match Target::open(id) {
            Ok(t) => {
                let (floor, base) = t.stack();
                log(&format!("[*] mercs2_profiler: main thread {id}; stack {floor:#x}..{base:#x}"));
                let _ = MAIN_THREAD.set(id);
            }
            Err(e) => fatal(&format!("opening the main thread for the sampler: {e}")),
        }
    }
    let began = Instant::now();
    {
        let _zone = tracy_client::span!("RunFrame");
        unsafe { orig() };
    }
    tracy().frame_mark();
    if let Some(counter) = jobs::read_cells_counter() {
        tracy_client::plot!("cells counter", f64::from(counter));
    }

    let ms = began.elapsed().as_secs_f32() * 1000.0;
    let frames = FRAMES.fetch_add(1, Ordering::Relaxed) + 1;
    let avg = if frames == 1 { ms } else { f32::from_bits(AVERAGE_MS.load(Ordering::Relaxed)) * (1.0 - AVERAGE_WEIGHT) + ms * AVERAGE_WEIGHT };
    FRAME_MS.store(ms.to_bits(), Ordering::Relaxed);
    AVERAGE_MS.store(avg.to_bits(), Ordering::Relaxed);
    if let Err(e) = unsafe { track_device(frames) } {
        fatal(&e);
    }
    match unsafe { jobs::track(JOBS.get().expect("set before the detours are attached"), &config().route_jobtypes) } {
        Ok(wrapped) => {
            for l in wrapped {
                log(&format!("[*] mercs2_profiler: frame {frames}: {l}"));
            }
        }
        Err(e) => fatal(&format!("job zones: {e}")),
    }
    poll_hotkey();
}

fn secs() -> f64 {
    EPOCH.get().expect("set before the detours are attached").elapsed().as_secs_f64()
}

fn config() -> &'static Config {
    CONFIG.get().expect("loaded before the detours are attached")
}

fn key_down(code: u8) -> bool {
    keys::async_state_is_down(unsafe { GetAsyncKeyState(i32::from(code)) })
}

fn game_focused() -> bool {
    let fg = unsafe { GetForegroundWindow() };
    let fg_pid = if fg.is_null() {
        None
    } else {
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(fg, &mut pid) };
        Some(pid)
    };
    keys::focus_matches(fg_pid, unsafe { GetCurrentProcessId() })
}

/// The session name for a capture started now, in local time.
fn session_now() -> String {
    let mut t = unsafe { core::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    profd::session_at(t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

/// The hotkey's down edge, with its modifiers held, while the game has focus: the session's next request.
fn poll_hotkey() {
    let spec = config().hotkey;
    let base = spec.vk.expect("config::parse refuses a hotkey that names no key");
    let down = key_down(base);
    let was_down = HOTKEY_DOWN.swap(down, Ordering::Relaxed);
    if !down || was_down || !game_focused() {
        return;
    }
    let mut held = Mods::NONE;
    for (code, m) in [(vk::CONTROL, Mods::CTRL), (vk::SHIFT, Mods::SHIFT), (vk::MENU, Mods::ALT)] {
        if key_down(code) {
            held = held.union(m);
        }
    }
    if !held.contains(spec.mods) {
        return;
    }
    let req = SESSION.lock().unwrap_or_else(|p| p.into_inner()).hotkey(session_now());
    if let Some(req) = req {
        log(&format!("[*] mercs2_profiler: {} -> {}", config().hotkey_name, req.line().trim_end()));
        let tx = REQUESTS.get().expect("started before the detours are attached").lock().unwrap_or_else(|p| p.into_inner());
        if tx.send(req).is_err() {
            fatal("the capture-service request thread has stopped");
        }
    }
}

/// Send `req` to the capture service at `addr` and read its answer.
fn request(addr: SocketAddrV4, req: &Request, answer: Duration) -> Result<Reply, String> {
    let mut s = TcpStream::connect_timeout(&addr.into(), SERVICE_CONNECT)
        .map_err(|e| format!("the capture service at {addr} did not accept a connection: {e}"))?;
    s.set_write_timeout(Some(SERVICE_CONNECT)).map_err(|e| format!("setting a write timeout: {e}"))?;
    s.write_all(req.line().as_bytes()).map_err(|e| format!("sending to the capture service: {e}"))?;
    s.set_read_timeout(Some(answer)).map_err(|e| format!("setting a read timeout: {e}"))?;
    let mut line = String::new();
    BufReader::new((&s).take(LINE_MAX as u64 + 2))
        .read_line(&mut line)
        .map_err(|e| format!("reading the capture service's answer: {e}"))?;
    if line.is_empty() {
        return Err("the capture service closed the connection without answering".into());
    }
    Reply::parse(&line)
}

/// `req` to the service, logged.
fn logged_request(addr: SocketAddrV4, req: &Request, answer: Duration) -> Result<Reply, String> {
    let got = request(addr, req, answer);
    match &got {
        Ok(r) => log(&format!("[*] mercs2_profiler: {} <- {}", req.line().trim_end(), r.line().trim_end())),
        Err(e) => log(&format!("[!] mercs2_profiler: {} failed: {e}", req.line().trim_end())),
    }
    got
}

fn describe(r: &Result<Reply, String>) -> String {
    match r {
        Ok(Reply::Ok(m)) => m.clone(),
        Ok(Reply::Err(e)) | Err(e) => e.clone(),
    }
}

/// The samples stream of `session`, sampling the main thread and every thread of `snapshot` that started in
/// the game's image.
fn start_samples(addr: SocketAddrV4, session: &str, snapshot: &[cpu::Thread]) -> Result<sampler::Running, String> {
    let main = *MAIN_THREAD.get().ok_or("the main thread has not run a frame yet")?;
    let main_start = snapshot
        .iter()
        .find(|t| t.tid == main)
        .ok_or_else(|| format!("the main thread {main} is not in the thread snapshot"))?
        .start_va
        .ok_or_else(|| format!("the main thread {main}'s start address could not be read"))?;
    let workers = pool::thread_ids();
    let mut sampled = vec![prof_samples::Thread { tid: main, start: main_start }];
    sampled.extend(snapshot.iter().filter(|t| t.tid != main).filter_map(|t| {
        t.start_va
            .filter(|&va| threads::in_game_image(va) || workers.contains(&t.tid))
            .map(|start| prof_samples::Thread { tid: t.tid, start })
    }));
    if let Some(w) = workers.iter().find(|w| !sampled.iter().any(|s| s.tid == **w)) {
        return Err(format!("job worker thread {w} is not in the thread snapshot or its start address could not be read"));
    }
    let targets = sampled.iter().map(|t| Target::open(t.tid)).collect::<Result<Vec<_>, _>>()?;
    for (t, s) in targets.iter().zip(&sampled) {
        let (floor, base) = t.stack();
        let at = snapshot.iter().find(|x| x.tid == s.tid).map_or("?", |x| x.start.as_str());
        log(&format!("[*] mercs2_profiler: sampling thread {} ({at}); stack {floor:#x}..{base:#x}", s.tid));
    }
    let mut header = SAMPLES_HEADER.get().expect("built at start-up").clone();
    header.main_thread = main;
    header.threads = sampled;
    header.modules = threads::modules()?;
    let period = Duration::from_micros(u64::from(header.period_us));
    sampler::start(addr, session, header, targets, period, &FRAMES)
}

/// The thread that sends the session's requests, one at a time, and hands each answer to the session. A
/// start snapshots every thread once the capture runs and opens the samples stream on the game's threads;
/// a stop snapshots again, ends the stream, then the capture.
fn serve_requests(rx: mpsc::Receiver<Request>, addr: SocketAddrV4) {
    let mut samples: Option<sampler::Running> = None;
    let mut threads_at_start: Option<(Vec<cpu::Thread>, Instant)> = None;
    for req in rx {
        let answer = match &req {
            Request::Start { session } => {
                let started = logged_request(addr, &req, START_ANSWER);
                match started {
                    Ok(Reply::Ok(m)) => match threads::snapshot()
                        .map_err(|e| format!("the thread snapshot failed: {e}"))
                        .and_then(|t| Ok((start_samples(addr, session, &t)?, t)))
                    {
                        Ok((r, t)) => {
                            log(&format!("[*] mercs2_profiler: samples stream of {session} open; {} threads in the process", t.len()));
                            samples = Some(r);
                            threads_at_start = Some((t, Instant::now()));
                            Ok(Reply::Ok(m))
                        }
                        Err(e) => {
                            log(&format!("[!] mercs2_profiler: {session} did not start: {e}"));
                            let stopped = logged_request(addr, &Request::Stop, STOP_ANSWER);
                            Ok(Reply::Err(format!("{e}; the capture was stopped: {}", describe(&stopped))))
                        }
                    },
                    other => other,
                }
            }
            Request::Stop => {
                let threads_line = match (threads_at_start.take(), threads::snapshot()) {
                    (Some((before, at)), Ok(after)) => {
                        let main = MAIN_THREAD.get().copied().unwrap_or(0);
                        let (lines, summary) = cpu::report(&before, &after, main, at.elapsed().as_secs_f64());
                        log("[*] mercs2_profiler: thread CPU over the session:");
                        for l in &lines {
                            log(&format!("[*]   {l}"));
                        }
                        Ok(summary)
                    }
                    (None, _) => Err("no thread snapshot was taken at the start".to_string()),
                    (_, Err(e)) => Err(format!("the thread snapshot at the stop failed: {e}")),
                };
                let (inv, chg, nonzero, max_diff, max_after) = jobs::cells_totals();
                log(&format!(
                    "[*] mercs2_profiler: job 17 totals: {inv} invocations, {chg} changes ({:.3}/inv), {nonzero} non-zero invocations, max {max_diff} changes/inv, counter max {max_after}",
                    chg as f64 / inv.max(1) as f64
                ));
                let (sh_inv, sh_match, sh_mis, sh_port, sh_game, _sh_walk) = crate::shadow::stats();
                if sh_inv > 0 {
                    log(&format!(
                        "[*] mercs2_profiler: cells port shadow: {sh_inv} invocations, {sh_match} count-matches ({:.3}% match), {sh_mis} mismatches; port emitted {sh_port} changes vs game {sh_game}",
                        100.0 * sh_match as f64 / sh_inv as f64
                    ));
                }
                let ended = samples.take().map(|r| {
                    r.finish().map(|f| {
                        log("[*] mercs2_profiler: what sampling cost each thread:");
                        for l in &f.threads {
                            log(&format!("[*]   {l}"));
                        }
                        f.answer
                    })
                });
                match &ended {
                    Some(Ok(m)) => log(&format!("[*] mercs2_profiler: samples stream <- {m}")),
                    Some(Err(e)) => log(&format!("[!] mercs2_profiler: samples stream: {e}")),
                    None => log("[!] mercs2_profiler: STOP with no samples stream open"),
                }
                let stopped = logged_request(addr, &req, STOP_ANSWER);
                match (ended, stopped, threads_line) {
                    (Some(Ok(s)), Ok(Reply::Ok(m)), Ok(t)) => Ok(Reply::Ok(format!("{m}; {s}; {t}"))),
                    (Some(Ok(s)), Ok(Reply::Ok(m)), Err(e)) => Ok(Reply::Err(format!("{e}; {m}; {s}"))),
                    (Some(Ok(_)), other, _) => other,
                    (Some(Err(e)), stopped, _) => Ok(Reply::Err(format!("samples: {e}; capture: {}", describe(&stopped)))),
                    (None, stopped, _) => Ok(Reply::Err(format!("no samples stream was open; capture: {}", describe(&stopped)))),
                }
            }
            Request::Samples { .. } | Request::Status => logged_request(addr, &req, STOP_ANSWER),
        };
        SESSION.lock().unwrap_or_else(|p| p.into_inner()).answered(answer, FRAMES.load(Ordering::Relaxed), secs());
    }
}

/// The sha256 of the file the process was started from.
fn exe_sha256() -> Result<[u8; 32], String> {
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetModuleFileNameW(core::ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n == buf.len() {
        return Err(format!("GetModuleFileNameW(NULL) failed: {}", std::io::Error::last_os_error()));
    }
    let path = PathBuf::from(std::ffi::OsString::from_wide(&buf[..n]));
    let bytes = std::fs::read(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    Ok(Sha256::digest(&bytes).into())
}

/// The game's `IDirect3DDevice9*`, once the RenderSystem and its device exist.
unsafe fn game_device() -> Option<*mut c_void> {
    let global = RENDER_SYSTEM.load(Ordering::Relaxed) as *const usize;
    let rs = unsafe { global.read_volatile() };
    if rs == 0 {
        return None;
    }
    let device = unsafe { ((rs + RENDER_SYSTEM_DEVICE as usize) as *const usize).read_volatile() };
    (device != 0).then_some(device as *mut c_void)
}

/// Detour the game device's Present on the first frame the device exists; afterwards, check that the device
/// still presents through it.
unsafe fn track_device(frames: u32) -> Result<(), String> {
    let Some(device) = (unsafe { game_device() }) else {
        return Ok(());
    };
    let current = unsafe { overlay::present_of(device)? } as usize;
    let target = PRESENT_TARGET.load(Ordering::Relaxed);
    if target == 0 {
        unsafe { rt::hook_attach(current as *mut c_void, present as *mut c_void, Some(&PRESENT), "IDirect3DDevice9::Present")? };
        PRESENT_TARGET.store(current, Ordering::Relaxed);
        log(&format!("[*] mercs2_profiler: game device {device:p}; its IDirect3DDevice9::Present {current:#x} detoured at frame {frames}"));
        return Ok(());
    }
    if current != target {
        return Err(format!(
            "the game's device {device:p} now presents through {current:#x}, not the detoured IDirect3DDevice9::Present {target:#x}"
        ));
    }
    Ok(())
}

fn overlay_lines() -> Vec<(String, u32)> {
    let mut lines = vec![
        (format!("mercs2-profiler {}", env!("CARGO_PKG_VERSION")), session::WHITE),
        (
            format!(
                "frame {:5.1} ms  avg {:5.1} ms",
                f32::from_bits(FRAME_MS.load(Ordering::Relaxed)),
                f32::from_bits(AVERAGE_MS.load(Ordering::Relaxed))
            ),
            session::WHITE,
        ),
    ];
    let cfg = config();
    if cfg.job_workers > 0 {
        lines.push((
            format!(
                "job workers {} (cpu 2..{}), {} jobtype(s) routed, {} queued",
                cfg.job_workers,
                1 + cfg.job_workers,
                cfg.route_jobtypes.len(),
                pool::queued()
            ),
            session::WHITE,
        ));
    }
    let (invocations, changes, nonzero, max_diff, max_after) = jobs::cells_totals();
    if invocations > 0 {
        let avg = changes as f64 / invocations as f64;
        lines.push((
            format!("job 17: {invocations} inv, {changes} changes ({avg:.2}/inv), {nonzero} nonzero, max {max_diff}/inv, counter max {max_after}"),
            session::WHITE,
        ));
    }
    let s = SESSION.lock().unwrap_or_else(|p| p.into_inner());
    lines.extend(s.lines(FRAMES.load(Ordering::Relaxed), secs(), &cfg.hotkey_name));
    lines
}

/// `IDirect3DDevice9::Present`: the overlay into the back buffer, then the game's Present.
unsafe extern "system" fn present(this: *mut c_void, src: *const RECT, dst: *const RECT, wnd: HWND, dirty: *const RGNDATA) -> HRESULT {
    let orig: PresentFn = unsafe { core::mem::transmute(PRESENT.load(Ordering::Acquire)) };
    if PRESENTS.fetch_add(1, Ordering::Relaxed) == 0 {
        log(&format!("[*] mercs2_profiler: first IDirect3DDevice9::Present at frame {}; drawing the overlay", FRAMES.load(Ordering::Relaxed)));
    }
    {
        let _zone = tracy_client::span!("Overlay");
        let drawn = OVERLAY.with_borrow_mut(|overlay| {
            let atlas = ATLAS.get().expect("Present is detoured only after the atlas is built");
            unsafe { overlay.get_or_insert_with(|| Overlay::new(atlas)).draw(this, &overlay_lines()) }
        });
        if let Err(e) = drawn {
            fatal(&format!("drawing the overlay: {e}"));
        }
    }
    unsafe { orig(this, src, dst, wnd, dirty) }
}

fn start() -> Result<(), String> {
    unsafe { ffi::m2_log_init(module()) };
    log("==========================================");
    log(&format!("[*] mercs2_profiler {} loading", env!("CARGO_PKG_VERSION")));
    log("==========================================");
    rt::compatible(rt::loaded_version(), SDK_BUILT)?;
    EPOCH.get_or_init(Instant::now);

    let dir = module_dir()?;
    let ini = dir.join("mercs2_profiler.ini");
    if config::ensure_default(&ini)? {
        log(&format!("[*] mercs2_profiler: wrote the default {}", ini.display()));
    }
    // Install the crash dumper before anything else that could fault. Game exe's base and image size
    // give us Mercenaries2.exe+N resolution for return addresses.
    let game_base = unsafe { GetModuleHandleA(core::ptr::null()) } as usize;
    let game_image = mapped()?;
    let game_size = game_image.sections.iter().map(|s| (s.va - game_base as u32 + s.header.virtual_size) as usize).max().unwrap_or(0);
    crash::install(dir, game_base, game_size);
    log(&format!("[*] mercs2_profiler: crash dumper installed; reports go to {}", module_dir()?.display()));
    let cfg = config::load(&ini)?;
    log(&format!(
        "[*] mercs2_profiler: hotkey {}; capture service {}; job workers {}; routed jobtypes [{}]",
        cfg.hotkey_name,
        cfg.capture_service,
        cfg.job_workers,
        cfg.route_jobtypes.iter().map(|h| format!("{h:#010x}")).collect::<Vec<_>>().join(", ")
    ));
    let addr = cfg.capture_service;
    if CONFIG.set(cfg).is_err() {
        return Err("the config was loaded twice".into());
    }
    let (tx, rx) = mpsc::channel();
    if REQUESTS.set(Mutex::new(tx)).is_err() {
        return Err("the request thread was started twice".into());
    }
    std::thread::Builder::new()
        .name("mercs2_profiler capture service".into())
        .spawn(move || serve_requests(rx, addr))
        .map_err(|e| format!("starting the capture-service request thread: {e}"))?;

    let sites = resolve_sites()?;
    for site in Site::ALL {
        log(&format!("[*] mercs2_profiler: site {} -> {:#x}", site.name(), sites.va(site)));
    }
    log(&format!("[*] mercs2_profiler: RenderSystem global {:#x}", sites.render_system()));
    RENDER_SYSTEM.store(sites.render_system() as usize, Ordering::Relaxed);
    let job_tables = sites.jobs();
    log(&format!(
        "[*] mercs2_profiler: jobtype count {:#x}; handler tables {:#x} (cpu 0) {:#x} (cpu 1)",
        job_tables.count, job_tables.handlers[0], job_tables.handlers[1]
    ));
    if JOBS.set(job_tables).is_err() {
        return Err("the job tables were set twice".into());
    }
    let cells_ring = sites.cells_change();
    jobs::set_cells_change_ring(cells_ring);
    let startup_counter = jobs::read_cells_counter().unwrap_or(0);
    let startup_entry0 = unsafe { (cells_ring.ring as usize as *const u32).read_volatile() };
    log(&format!(
        "[*] mercs2_profiler: jobtype {:#010x} change ring: counter {:#x} (value {startup_counter}), ring {:#x} (entry 0 word 0 = {startup_entry0:#x})",
        jobs::CELLS_JOBTYPE,
        cells_ring.counter,
        cells_ring.ring
    ));
    // Shadow mode: run the Rust port against the game's actual job 17 inputs each invocation and
    // compare its change count to the game's ring diff. Enabled at startup; stats are logged on STOP.
    crate::shadow::ENABLED.store(true, Ordering::Release);
    log("[*] mercs2_profiler: cells port shadow mode on");

    let img = mapped()?;
    let code = img.code_ranges();
    let sha = exe_sha256()?;
    let sha_hex: String = sha.iter().map(|b| format!("{b:02x}")).collect();
    log(&format!(
        "[*] mercs2_profiler: exe sha256 {sha_hex}; image base {:#x}; code {}",
        img.sections.first().map_or(0, |s| s.va - s.header.virtual_address),
        code.iter().map(|(s, e)| format!("{s:#x}..{e:#x}")).collect::<Vec<_>>().join(" ")
    ));
    let mut freq = 0i64;
    if unsafe { QueryPerformanceFrequency(&mut freq) } == 0 || freq <= 0 {
        return Err(format!("QueryPerformanceFrequency failed: {}", std::io::Error::last_os_error()));
    }
    let header = prof_samples::Header {
        exe_sha256: sha,
        image_base: unsafe { GetModuleHandleA(core::ptr::null()) } as u32,
        qpc_frequency: freq as u64,
        period_us: 1_000_000 / config().sample_rate_hz,
        main_thread: 0,
        threads: Vec::new(),
        modules: Vec::new(),
        code,
    };
    if SAMPLES_HEADER.set(header).is_err() {
        return Err("the samples header was built twice".into());
    }

    if TRACY.set(tracy_client::Client::start()).is_err() {
        return Err("the Tracy client was started twice".into());
    }
    log("[*] mercs2_profiler: Tracy client started (on demand, port 8086)");

    let atlas = Atlas::rasterize()?;
    log(&format!(
        "[*] mercs2_profiler: glyph atlas {}x{} ({}x{} cells)",
        atlas.layout.tex_w, atlas.layout.tex_h, atlas.layout.cell_w, atlas.layout.cell_h
    ));
    if ATLAS.set(atlas).is_err() {
        return Err("the glyph atlas was built twice".into());
    }

    if unsafe { ffi::m2_hook_init() } != 1 {
        return Err("m2_hook_init failed; m2-sdk.log has the reason".into());
    }
    let workers = config().job_workers;
    if workers > 0 {
        for l in pool::start(workers, fatal)? {
            log(&format!("[*] mercs2_profiler: {l}"));
        }
        unsafe {
            rt::hook_attach(sites.va(Site::JobWait) as *mut c_void, pool::job_wait as *mut c_void, Some(&pool::WAIT_ORIGINAL), "JobWait")?;
        }
        log("[*] mercs2_profiler: JobWait detoured: a job worker's nested wait runs routed jobs");
    }
    unsafe {
        rt::hook_attach(sites.va(Site::RunFrame) as *mut c_void, run_frame as *mut c_void, Some(&RUN_FRAME), "RunFrame")?;
    }
    log("[*] mercs2_profiler: RunFrame detoured");
    log("[*] mercs2_profiler: ready");
    Ok(())
}

#[unsafe(no_mangle)]
pub extern "system" fn DllMain(h: HMODULE, reason: u32, _reserved: *mut c_void) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        MODULE.store(h as usize, Ordering::Release);
        unsafe { DisableThreadLibraryCalls(h) };
        if let Err(e) = std::thread::Builder::new().name("mercs2_profiler start".into()).spawn(|| {
            if let Err(e) = start() {
                fatal(&e);
            }
        }) {
            fatal(&format!("could not start the start-up thread: {e}"));
        }
    }
    1
}
