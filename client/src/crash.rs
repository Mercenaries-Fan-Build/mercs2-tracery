//! A vectored exception handler that writes a rich forensic report when the game takes an access
//! violation. The game's `pmc_bb` already has its own VEH that logs a short summary; this one runs
//! first (so its data is always written), captures more state (registers, stack, EBP chain,
//! instruction bytes, the `this` object for known Havok sites), returns `EXCEPTION_CONTINUE_SEARCH`,
//! and lets `pmc_bb` and the game's `UnhandledExceptionFilter` run after it.
//!
//! Writes one report per process life. The second-chance exception the OS raises before termination
//! would just repeat what we already have.

use core::ffi::c_void;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;
use windows_sys::Win32::Foundation::EXCEPTION_ACCESS_VIOLATION;
use windows_sys::Win32::System::Diagnostics::Debug::{AddVectoredExceptionHandler, CONTEXT, EXCEPTION_POINTERS};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

const EXCEPTION_CONTINUE_SEARCH: i32 = 0;

/// Directory to write reports into (the ASI's own directory).
static DIR: OnceLock<PathBuf> = OnceLock::new();
/// The ASI's load epoch, so the report says how long the game had been up.
static EPOCH: OnceLock<Instant> = OnceLock::new();
/// `pmc_bb`'s module base + size, so the report can resolve `module+offset` of return addresses.
static GAME_BASE: AtomicUsize = AtomicUsize::new(0);
static GAME_END: AtomicUsize = AtomicUsize::new(0);
/// Fire the full dump only once; a second-chance repeat adds nothing.
static ONCE: AtomicBool = AtomicBool::new(false);

/// Install the handler. `dir` is where reports go; `game_base`/`game_size` cover the game exe
/// (reported as `Mercenaries2.exe+N` in return-address lines).
pub fn install(dir: PathBuf, game_base: usize, game_size: usize) {
    let _ = DIR.set(dir);
    GAME_BASE.store(game_base, Ordering::Release);
    GAME_END.store(game_base.saturating_add(game_size), Ordering::Release);
    EPOCH.get_or_init(Instant::now);
    unsafe {
        AddVectoredExceptionHandler(1, Some(handler));
    }
}

unsafe extern "system" fn handler(info: *mut EXCEPTION_POINTERS) -> i32 {
    if info.is_null() {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let rec = unsafe { (*info).ExceptionRecord };
    let ctx = unsafe { (*info).ContextRecord };
    if rec.is_null() || ctx.is_null() {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let code = unsafe { (*rec).ExceptionCode };
    if code != EXCEPTION_ACCESS_VIOLATION {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    if ONCE.swap(true, Ordering::SeqCst) {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    // Build the report; ignore any I/O error, we're on the way out anyway.
    let _ = unsafe { write_report(rec, ctx) };
    EXCEPTION_CONTINUE_SEARCH
}

/// Read `len` bytes starting at `va`, stopping early if the page is unreadable. Uses a cheap
/// try-read with `IsBadReadPtr` — not perfect, but it's good enough to avoid taking a nested AV
/// inside the handler.
unsafe fn try_read(va: usize, out: &mut [u8]) -> usize {
    use windows_sys::Win32::System::Memory::IsBadReadPtr;
    if va == 0 {
        return 0;
    }
    if unsafe { IsBadReadPtr(va as *const c_void, out.len()) } != 0 {
        return 0;
    }
    unsafe { core::ptr::copy_nonoverlapping(va as *const u8, out.as_mut_ptr(), out.len()) };
    out.len()
}

/// Resolve a code address to `Mercenaries2.exe+N` when it's in the game exe, else bare hex.
fn where_is(addr: usize) -> String {
    let base = GAME_BASE.load(Ordering::Acquire);
    let end = GAME_END.load(Ordering::Acquire);
    if base != 0 && addr >= base && addr < end {
        format!("Mercenaries2.exe+{:#x}", addr - base)
    } else {
        format!("{addr:#x}")
    }
}

/// A hex dump of `bytes` as "AA BB CC …" on one line.
fn hex_line(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&format!("{b:02X}"));
    }
    s
}

fn report_path() -> Option<PathBuf> {
    let dir = DIR.get()?;
    let mut t = unsafe { core::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    let t: windows_sys::Win32::Foundation::SYSTEMTIME = t;
    Some(dir.join(format!(
        "mercs2_profiler_crash_{:04}{:02}{:02}-{:02}{:02}{:02}.log",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )))
}

unsafe fn write_report(rec: *mut windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_RECORD, ctx: *mut CONTEXT) -> std::io::Result<()> {
    let Some(path) = report_path() else { return Ok(()) };
    let mut f = OpenOptions::new().write(true).create(true).truncate(true).open(&path)?;
    unsafe { write_body(&mut f, rec, ctx, &path) }?;
    f.flush()
}

unsafe fn write_body(
    f: &mut std::fs::File,
    rec: *mut windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_RECORD,
    ctx: *mut CONTEXT,
    path: &Path,
) -> std::io::Result<()> {
    let r = unsafe { &*rec };
    let c = unsafe { &*ctx };
    let elapsed = EPOCH.get().map(|e| e.elapsed().as_secs_f64()).unwrap_or(0.0);
    let fault_addr = r.ExceptionInformation.get(1).copied().unwrap_or(0);
    let fault_rw = r.ExceptionInformation.first().copied().unwrap_or(0);
    let fault_rw_str = match fault_rw {
        0 => "READ",
        1 => "WRITE",
        8 => "EXECUTE",
        _ => "?",
    };

    writeln!(f, "mercs2_profiler crash report ({})", path.display())?;
    writeln!(f, "{elapsed:.3} s after the profiler loaded")?;
    writeln!(f, "exception {:#x} AT EIP={} ACCESS {fault_rw_str} target={:#x}", r.ExceptionCode, where_is(c.Eip as usize), fault_addr)?;
    writeln!(f)?;

    writeln!(f, "== Registers ==")?;
    writeln!(f, "EAX={:08X}  ECX={:08X}  EDX={:08X}  EBX={:08X}", c.Eax, c.Ecx, c.Edx, c.Ebx)?;
    writeln!(f, "ESP={:08X}  EBP={:08X}  ESI={:08X}  EDI={:08X}", c.Esp, c.Ebp, c.Esi, c.Edi)?;
    writeln!(f, "EIP={:08X}  EFLAGS={:08X}", c.Eip, c.EFlags)?;
    writeln!(f, "CS={:04X} DS={:04X} ES={:04X} FS={:04X} GS={:04X} SS={:04X}", c.SegCs, c.SegDs, c.SegEs, c.SegFs, c.SegGs, c.SegSs)?;
    writeln!(f)?;

    writeln!(f, "== Instruction bytes (EIP..EIP+32) ==")?;
    let mut ib = [0u8; 32];
    let n = unsafe { try_read(c.Eip as usize, &mut ib) };
    writeln!(f, "{}", hex_line(&ib[..n]))?;
    if n < ib.len() {
        writeln!(f, "(only {n} bytes readable at EIP)")?;
    }
    writeln!(f)?;

    writeln!(f, "== Stack (ESP..ESP+0x200) ==")?;
    let mut stk = [0u8; 0x200];
    let n = unsafe { try_read(c.Esp as usize, &mut stk) };
    for off in (0..n).step_by(16) {
        let line = &stk[off..usize::min(off + 16, n)];
        writeln!(f, "esp+{off:03X}: {}", hex_line(line))?;
    }
    writeln!(f)?;

    writeln!(f, "== Stack as potential return addresses (ESP..ESP+0x200) ==")?;
    for off in (0..n).step_by(4) {
        if off + 4 > n {
            break;
        }
        let w = u32::from_le_bytes([stk[off], stk[off + 1], stk[off + 2], stk[off + 3]]) as usize;
        if is_code_in_game(w) {
            writeln!(f, "esp+{off:03X} = {}", where_is(w))?;
        }
    }
    writeln!(f)?;

    writeln!(f, "== EBP chain (max 32 frames) ==")?;
    let mut bp = c.Ebp as usize;
    for i in 0..32 {
        if bp == 0 || !is_stack_like(bp, c.Esp as usize) {
            writeln!(f, "frame {i:2}: stop ebp={bp:#x} (outside the stack window)")?;
            break;
        }
        let mut raw = [0u8; 8];
        let r = unsafe { try_read(bp, &mut raw) };
        if r != 8 {
            writeln!(f, "frame {i:2}: ebp={bp:#x} unreadable")?;
            break;
        }
        let next = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
        let ret = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]) as usize;
        writeln!(f, "frame {i:2}: ebp={bp:#x}  ret={}", where_is(ret))?;
        if next <= bp {
            break;
        }
        bp = next;
    }
    writeln!(f)?;

    // The ~1-minute Havok sweep crash: this is on the stack, caster at ESI/ECX.
    if (c.Eip as usize) & !0xf == 0x8bf470 || (c.Eip as usize) & !0xf == 0x8bf1b0 {
        writeln!(f, "== hkpWorldLinearCaster this (ECX={:#x}): 0x40 bytes ==", c.Ecx)?;
        let mut data = [0u8; 0x40];
        let r = unsafe { try_read(c.Ecx as usize, &mut data) };
        for off in (0..r).step_by(16) {
            let line = &data[off..usize::min(off + 16, r)];
            writeln!(f, "+{off:02X}: {}", hex_line(line))?;
        }
        writeln!(f)?;

        let obj = unsafe {
            // The failing dereference is `[esp+4]` → the first stack arg, which EAX holds now.
            // That value WAS loaded from the caller before the push; capture whichever is readable.
            let mut raw = [0u8; 4];
            try_read((c.Esp as usize).wrapping_add(0xc), &mut raw);
            u32::from_le_bytes(raw) as usize
        };
        writeln!(f, "== Fault object (first stack arg, [ESP+0xC]={obj:#x}) ==")?;
        if obj == 0 {
            writeln!(f, "NULL — the sweep handed the visitor a null object pointer")?;
        } else {
            let mut data = [0u8; 0x40];
            let r = unsafe { try_read(obj, &mut data) };
            if r == 0 {
                writeln!(f, "unreadable at {obj:#x}")?;
            } else {
                for off in (0..r).step_by(16) {
                    let line = &data[off..usize::min(off + 16, r)];
                    writeln!(f, "+{off:02X}: {}", hex_line(line))?;
                }
            }
        }
        writeln!(f)?;
    }

    writeln!(f, "== Note ==")?;
    writeln!(f, "This handler returned EXCEPTION_CONTINUE_SEARCH.")?;
    writeln!(f, "pmc_blackbox.log should also contain its own VEH summary written after this file.")?;
    Ok(())
}

/// Whether `addr` plausibly points inside the game exe's code.
fn is_code_in_game(addr: usize) -> bool {
    let base = GAME_BASE.load(Ordering::Acquire);
    let end = GAME_END.load(Ordering::Acquire);
    base != 0 && addr >= base && addr < end
}

/// A quick plausibility test for an EBP walking: the EBP should sit within ~1 MB of the thread's
/// current ESP (same stack).
fn is_stack_like(bp: usize, esp: usize) -> bool {
    let diff = if bp >= esp { bp - esp } else { esp - bp };
    bp != 0 && diff < 0x100_000
}
