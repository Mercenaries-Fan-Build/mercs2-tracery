//! A diagnostic that drops one draw family before the game queues it: every render packet whose program (and,
//! when given, class) matches is never produced, so nothing of that family is drawn while it is set.
//!
//! Set by [`FILE`] next to the ASI, read every [`POLL_FRAMES`] frames. Missing or empty is off; otherwise
//! `<program>` or `<program> <class>`, each decimal or `0x` hex. The program and class are the ones the draw asks
//! for (the globals the packet producer copies into packet `+0x40` and `+0x44`); the producer then rewrites some
//! classes, so a frame capture can show another class for the same draw.

/// The control file, in the ASI's directory.
pub const FILE: &str = "mercs2_profiler_drop.txt";
/// How often, in frames, the control file is read.
pub const POLL_FRAMES: u32 = 30;

/// The family to drop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Filter {
    pub program: u16,
    /// `None` drops every class of the program.
    pub class: Option<u8>,
}

impl Filter {
    pub fn describe(&self) -> String {
        match self.class {
            Some(c) => format!("program {:#x} class {c}", self.program),
            None => format!("program {:#x}, every class", self.program),
        }
    }
}

fn number(word: &str, max: u32, what: &str) -> Result<u32, String> {
    let v = match word.strip_prefix("0x").or_else(|| word.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => word.parse::<u32>(),
    }
    .map_err(|_| format!("{FILE}: the {what} {word:?} is not a decimal or 0x hex number"))?;
    if v > max {
        return Err(format!("{FILE}: the {what} {word} is above {max:#x}"));
    }
    Ok(v)
}

/// The filter the control file's text names; `None` when it names none.
pub fn parse(text: &str) -> Result<Option<Filter>, String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    match words[..] {
        [] => Ok(None),
        [p] => Ok(Some(Filter { program: number(p, 0xffff, "program")? as u16, class: None })),
        [p, c] => Ok(Some(Filter { program: number(p, 0xffff, "program")? as u16, class: Some(number(c, 0xff, "class")? as u8) })),
        _ => Err(format!("{FILE}: expected `<program>` or `<program> <class>`, got {text:?}")),
    }
}

/// The packet producer's detour and the state it reads.
#[cfg(all(windows, target_arch = "x86"))]
pub mod hook {
    use super::Filter;
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

    /// The program to drop, or `u32::MAX` for none; the class, or `u32::MAX` for every class.
    pub static PROGRAM: AtomicU32 = AtomicU32::new(u32::MAX);
    pub static CLASS: AtomicU32 = AtomicU32::new(u32::MAX);
    /// The addresses of the producer's program (`u16`) and class (`u8`) globals, from `profiler_sites`.
    pub static PROGRAM_GLOBAL: AtomicUsize = AtomicUsize::new(0);
    pub static CLASS_GLOBAL: AtomicUsize = AtomicUsize::new(0);
    /// The producer's trampoline, written by `m2_hook_attach` before the detour can first run.
    pub static ORIGINAL: AtomicUsize = AtomicUsize::new(0);
    /// Packets dropped since the last begin-frame.
    pub static DROPPED: AtomicU32 = AtomicU32::new(0);

    /// Sets the filter the detour applies; `None` turns it off.
    pub fn set(filter: Option<Filter>) {
        match filter {
            Some(f) => {
                // The class first, so the detour never pairs the new program with the old class.
                CLASS.store(f.class.map_or(u32::MAX, u32::from), Ordering::Release);
                PROGRAM.store(u32::from(f.program), Ordering::Release);
            }
            None => {
                PROGRAM.store(u32::MAX, Ordering::Release);
                CLASS.store(u32::MAX, Ordering::Release);
            }
        }
    }

    /// The packet producer's detour. The producer takes register arguments (`EAX`, `ECX`, `EDX`) and stack
    /// arguments its callers pop (`add esp, 0x18` after each call), returns nothing, and reads no flags at entry,
    /// so the detour keeps `EAX`, `ECX` and `EDX`, and either jumps to the original or returns with `ret`.
    #[unsafe(naked)]
    pub unsafe extern "C" fn producer() {
        core::arch::naked_asm!(
            "push eax",
            "mov eax, dword ptr [{program}]",
            "cmp eax, -1",
            "je 2f",
            "push ecx",
            "mov ecx, dword ptr [{program_global}]",
            "movzx ecx, word ptr [ecx]",
            "cmp ecx, eax",
            "jne 3f",
            "mov eax, dword ptr [{class}]",
            "cmp eax, -1",
            "je 4f",
            "mov ecx, dword ptr [{class_global}]",
            "movzx ecx, byte ptr [ecx]",
            "cmp ecx, eax",
            "jne 3f",
            "4:",
            "lock inc dword ptr [{dropped}]",
            "pop ecx",
            "pop eax",
            "ret",
            "3:",
            "pop ecx",
            "2:",
            "pop eax",
            "jmp dword ptr [{original}]",
            program = sym PROGRAM,
            class = sym CLASS,
            program_global = sym PROGRAM_GLOBAL,
            class_global = sym CLASS_GLOBAL,
            dropped = sym DROPPED,
            original = sym ORIGINAL,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_control_file_names_a_program_and_optionally_a_class() {
        assert_eq!(parse(""), Ok(None));
        assert_eq!(parse("  \n"), Ok(None));
        assert_eq!(parse("0x4a"), Ok(Some(Filter { program: 0x4a, class: None })));
        assert_eq!(parse("74 1\n"), Ok(Some(Filter { program: 74, class: Some(1) })));
        assert_eq!(parse("0x4A 0x1"), Ok(Some(Filter { program: 0x4a, class: Some(1) })));
        assert_eq!(Filter { program: 0x4a, class: None }.describe(), "program 0x4a, every class");
        assert_eq!(Filter { program: 0x4a, class: Some(1) }.describe(), "program 0x4a class 1");
    }

    #[test]
    fn a_malformed_control_file_is_refused() {
        assert!(parse("4a").unwrap_err().contains("not a decimal or 0x hex number"));
        assert!(parse("0x10000").unwrap_err().contains("above 0xffff"));
        assert!(parse("0x4a 256").unwrap_err().contains("above 0xff"));
        assert!(parse("1 2 3").unwrap_err().contains("expected"));
    }
}
