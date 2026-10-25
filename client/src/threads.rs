//! The game process's threads and modules: a snapshot of every thread for [`crate::cpu`] and the sampler
//! (thread id, start address from `NtQueryInformationThread` `ThreadQuerySetWin32StartAddress`, shown as
//! `module+offset`, and kernel plus user time from `GetThreadTimes`), a thread's stack base (its TEB's
//! `NT_TIB.StackBase`, from `ThreadBasicInformation`), and the loaded modules for the samples header. A
//! thread that exits between being listed and being opened is kept with no start and no CPU, so the report
//! counts it instead of leaving it out.

use crate::cpu::Thread;
use core::ffi::c_void;
use mercs2_formats::prof_samples::Module;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, HMODULE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, MODULEENTRY32W, Module32FirstW, Module32NextW, TH32CS_SNAPMODULE, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
    Thread32Next,
};
use windows_sys::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleFileNameW, GetModuleHandleA, GetModuleHandleExW,
    GetProcAddress,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcessId, GetThreadTimes, OpenThread, THREAD_QUERY_INFORMATION};

/// `THREADINFOCLASS::ThreadBasicInformation`.
const THREAD_BASIC_INFORMATION: i32 = 0;
/// `THREADINFOCLASS::ThreadQuerySetWin32StartAddress`.
const THREAD_QUERY_SET_WIN32_START_ADDRESS: i32 = 9;
type NtQueryInformationThread = unsafe extern "system" fn(HANDLE, i32, *mut c_void, u32, *mut u32) -> i32;

/// `THREAD_BASIC_INFORMATION` on x86.
#[repr(C)]
struct BasicInformation {
    exit_status: i32,
    teb: usize,
    client_id: [usize; 2],
    affinity_mask: usize,
    priority: i32,
    base_priority: i32,
}

fn nt_query() -> Result<NtQueryInformationThread, String> {
    static F: OnceLock<usize> = OnceLock::new();
    if let Some(&f) = F.get() {
        return Ok(unsafe { core::mem::transmute::<usize, NtQueryInformationThread>(f) });
    }
    let ntdll = unsafe { GetModuleHandleA(c"ntdll.dll".as_ptr().cast()) };
    if ntdll.is_null() {
        return Err("ntdll.dll is not loaded".into());
    }
    let f = unsafe { GetProcAddress(ntdll, c"NtQueryInformationThread".as_ptr().cast()) }.ok_or("ntdll.dll has no NtQueryInformationThread")?;
    let f = *F.get_or_init(|| f as usize);
    Ok(unsafe { core::mem::transmute::<usize, NtQueryInformationThread>(f) })
}

/// The stack base (`NT_TIB.StackBase`, the TEB's second word) of the thread `h` (opened with
/// `THREAD_QUERY_INFORMATION`).
pub fn stack_base(h: HANDLE) -> Result<u32, String> {
    let query = nt_query()?;
    let mut info: BasicInformation = unsafe { core::mem::zeroed() };
    let status = unsafe { query(h, THREAD_BASIC_INFORMATION, (&mut info as *mut BasicInformation).cast(), size_of::<BasicInformation>() as u32, core::ptr::null_mut()) };
    if status < 0 {
        return Err(format!("NtQueryInformationThread(ThreadBasicInformation): NTSTATUS {status:#x}"));
    }
    if info.teb == 0 {
        return Err("ThreadBasicInformation gives no TEB".into());
    }
    let base = unsafe { ((info.teb + 4) as *const u32).read_volatile() };
    if base == 0 {
        return Err(format!("the TEB at {:#x} has no stack base", info.teb));
    }
    Ok(base)
}

/// The module holding `addr`.
fn module_of(addr: usize) -> Option<HMODULE> {
    let mut m: HMODULE = core::ptr::null_mut();
    let found = unsafe {
        GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, addr as *const u16, &mut m)
    };
    (found != 0).then_some(m)
}

/// Whether `addr` lies in the game exe's image.
pub fn in_game_image(addr: u32) -> bool {
    module_of(addr as usize).is_some_and(|m| m == unsafe { GetModuleHandleA(core::ptr::null()) })
}

/// `module+offset` of `addr`, or the bare address outside every module.
fn where_is(addr: usize) -> String {
    let Some(m) = module_of(addr) else {
        return format!("{addr:#x}");
    };
    let mut buf = vec![0u16; 512];
    let n = unsafe { GetModuleFileNameW(m, buf.as_mut_ptr(), buf.len() as u32) } as usize;
    let name = PathBuf::from(std::ffi::OsString::from_wide(&buf[..n]))
        .file_name()
        .map_or_else(|| "?".to_string(), |f| f.to_string_lossy().into_owned());
    format!("{name}+{:#x}", addr - m as usize)
}

fn filetime(f: FILETIME) -> u64 {
    (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime)
}

/// Every thread of this process now.
pub fn snapshot() -> Result<Vec<Thread>, String> {
    let query = nt_query()?;
    let pid = unsafe { GetCurrentProcessId() };
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return Err(format!("CreateToolhelp32Snapshot: {}", std::io::Error::last_os_error()));
    }
    let mut out = Vec::new();
    let mut te: THREADENTRY32 = unsafe { core::mem::zeroed() };
    te.dwSize = size_of::<THREADENTRY32>() as u32;
    let mut more = unsafe { Thread32First(snap, &mut te) } != 0;
    while more {
        if te.th32OwnerProcessID == pid {
            let tid = te.th32ThreadID;
            let h = unsafe { OpenThread(THREAD_QUERY_INFORMATION, 0, tid) };
            if h.is_null() {
                out.push(Thread { tid, start_va: None, start: "?".into(), cpu_100ns: None });
            } else {
                let mut start: usize = 0;
                let status = unsafe { query(h, THREAD_QUERY_SET_WIN32_START_ADDRESS, (&mut start as *mut usize).cast(), size_of::<usize>() as u32, core::ptr::null_mut()) };
                let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
                let (mut c, mut e, mut k, mut u) = (zero, zero, zero, zero);
                let timed = unsafe { GetThreadTimes(h, &mut c, &mut e, &mut k, &mut u) } != 0;
                unsafe { CloseHandle(h) };
                out.push(Thread {
                    tid,
                    start_va: (status >= 0).then_some(start as u32),
                    start: if status >= 0 { where_is(start) } else { format!("? (NTSTATUS {status:#x})") },
                    cpu_100ns: timed.then(|| filetime(k) + filetime(u)),
                });
            }
        }
        more = unsafe { Thread32Next(snap, &mut te) } != 0;
    }
    unsafe { CloseHandle(snap) };
    if out.is_empty() {
        return Err(format!("the thread snapshot lists no thread of process {pid}"));
    }
    Ok(out)
}

/// Every module loaded in this process now.
pub fn modules() -> Result<Vec<Module>, String> {
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return Err(format!("CreateToolhelp32Snapshot(modules): {}", std::io::Error::last_os_error()));
    }
    let mut out = Vec::new();
    let mut me: MODULEENTRY32W = unsafe { core::mem::zeroed() };
    me.dwSize = size_of::<MODULEENTRY32W>() as u32;
    let mut more = unsafe { Module32FirstW(snap, &mut me) } != 0;
    let mut bad = None;
    while more {
        let n = me.szModule.iter().position(|&c| c == 0).unwrap_or(me.szModule.len());
        match String::from_utf16(&me.szModule[..n]) {
            Ok(name) => out.push(Module { base: me.modBaseAddr as u32, size: me.modBaseSize, name }),
            Err(e) => {
                bad = Some(format!("the module at {:#x} has a name that is not UTF-16: {e}", me.modBaseAddr as u32));
                break;
            }
        }
        more = unsafe { Module32NextW(snap, &mut me) } != 0;
    }
    unsafe { CloseHandle(snap) };
    if let Some(e) = bad {
        return Err(e);
    }
    if out.is_empty() {
        return Err("the module snapshot lists no module".into());
    }
    Ok(out)
}
