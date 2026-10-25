//! `mercs2_profiler.ini`, beside the ASI: written with its defaults when missing, then read. The file is
//! read with `mercs2_formats::ini::parse` (section-blind, `;`/`#` comments, first `=` splits). A key the
//! profiler does not know, or a value it cannot use, is an error.

use crate::route;
use mercs2_formats::ini;
use mercs2_formats::keys::{KeySpec, resolve_key_spec};
use std::net::SocketAddrV4;
use std::path::Path;

pub const DEFAULT_INI: &str = "\
; mercs2-profiler
; The key that starts a capture session, and stops it on the next press: a key name with optional
; ctrl+/shift+/alt+ in front (F5, ctrl+F12, scrolllock).
hotkey = F5
; The capture service (tools/tracy/compose.yaml in the wad_simulator workspace) that writes each
; session to its own .tracy and .samples files.
capture_service = 127.0.0.1:8099
; How many times a second the game's threads are sampled while a session records (1 to 10000).
sample_rate_hz = 250
; Extra job worker threads (0 to 8), Pimp CPU indices 2 and up, which run the routed jobtypes.
job_workers = 1
; The jobtypes the extra workers run instead of the thread that dequeues them: jobtype hashes
; (0x...), comma-separated; empty routes none. Only jobtypes checked for routing are accepted.
route_jobtypes =
";

/// The range `sample_rate_hz` accepts.
pub const SAMPLE_RATE_HZ: core::ops::RangeInclusive<u32> = 1..=10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The spec as written, for display.
    pub hotkey_name: String,
    pub hotkey: KeySpec,
    pub capture_service: SocketAddrV4,
    pub sample_rate_hz: u32,
    pub job_workers: u32,
    pub route_jobtypes: Vec<u32>,
}

impl Default for Config {
    fn default() -> Self {
        parse(DEFAULT_INI.as_bytes()).expect("the default file parses")
    }
}

fn text(key: &str, value: &[u8]) -> Result<String, String> {
    String::from_utf8(value.to_vec()).map_err(|_| format!("mercs2_profiler.ini: {key} is not UTF-8"))
}

pub fn parse(bytes: &[u8]) -> Result<Config, String> {
    if bytes.len() > ini::MAX_FILE {
        return Err(format!("mercs2_profiler.ini: the file is {} bytes; the INI reader accepts at most {}", bytes.len(), ini::MAX_FILE));
    }
    let mut hotkey = None;
    let mut service = None;
    let mut rate = None;
    let mut workers = None;
    let mut routed = None;
    let mut err = None;
    ini::parse(bytes, |k, v| {
        if err.is_some() {
            return;
        }
        let r = (|| {
            if k.eq_ignore_ascii_case(b"hotkey") {
                let name = text("hotkey", v)?;
                let spec = resolve_key_spec(&name);
                if spec.vk.is_none() {
                    return Err(format!("mercs2_profiler.ini: hotkey = {name:?} names no key"));
                }
                hotkey = Some((name, spec));
            } else if k.eq_ignore_ascii_case(b"capture_service") {
                let s = text("capture_service", v)?;
                let addr = s.parse::<SocketAddrV4>().map_err(|e| format!("mercs2_profiler.ini: capture_service = {s:?}: {e}"))?;
                service = Some(addr);
            } else if k.eq_ignore_ascii_case(b"sample_rate_hz") {
                let s = text("sample_rate_hz", v)?;
                let hz = s
                    .parse::<u32>()
                    .ok()
                    .filter(|hz| SAMPLE_RATE_HZ.contains(hz))
                    .ok_or_else(|| format!("mercs2_profiler.ini: sample_rate_hz = {s:?} is not a whole number from 1 to 10000"))?;
                rate = Some(hz);
            } else if k.eq_ignore_ascii_case(b"job_workers") {
                let s = text("job_workers", v)?;
                let n = s
                    .parse::<u32>()
                    .ok()
                    .filter(|n| *n <= route::MAX_WORKERS)
                    .ok_or_else(|| format!("mercs2_profiler.ini: job_workers = {s:?} is not a whole number from 0 to {}", route::MAX_WORKERS))?;
                workers = Some(n);
            } else if k.eq_ignore_ascii_case(b"route_jobtypes") {
                let s = text("route_jobtypes", v)?;
                let hashes = s
                    .split(',')
                    .map(str::trim)
                    .filter(|h| !h.is_empty())
                    .map(|h| {
                        h.strip_prefix("0x")
                            .or_else(|| h.strip_prefix("0X"))
                            .and_then(|x| u32::from_str_radix(x, 16).ok())
                            .ok_or_else(|| format!("mercs2_profiler.ini: route_jobtypes: {h:?} is not a jobtype hash (0x followed by hex)"))
                    })
                    .collect::<Result<Vec<u32>, String>>()?;
                routed = Some(hashes);
            } else {
                return Err(format!("mercs2_profiler.ini: unknown key {:?}", String::from_utf8_lossy(k)));
            }
            Ok(())
        })();
        if let Err(e) = r {
            err = Some(e);
        }
    });
    if let Some(e) = err {
        return Err(e);
    }
    let (hotkey_name, hotkey) = hotkey.ok_or("mercs2_profiler.ini: hotkey is missing")?;
    let capture_service = service.ok_or("mercs2_profiler.ini: capture_service is missing")?;
    let sample_rate_hz = rate.ok_or("mercs2_profiler.ini: sample_rate_hz is missing")?;
    let job_workers = workers.ok_or("mercs2_profiler.ini: job_workers is missing")?;
    let route_jobtypes = routed.ok_or("mercs2_profiler.ini: route_jobtypes is missing")?;
    route::check(job_workers, &route_jobtypes).map_err(|e| format!("mercs2_profiler.ini: {e}"))?;
    Ok(Config { hotkey_name, hotkey, capture_service, sample_rate_hz, job_workers, route_jobtypes })
}

/// Write the default file at `path` when there is none; `true` when it was written.
pub fn ensure_default(path: &Path) -> Result<bool, String> {
    if path.exists() {
        return Ok(false);
    }
    std::fs::write(path, DEFAULT_INI).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(true)
}

pub fn load(path: &Path) -> Result<Config, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    parse(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mercs2_formats::keys::{Mods, vk};

    #[test]
    fn the_default_file_is_f5_and_the_local_service() {
        let c = Config::default();
        assert_eq!(c.hotkey, KeySpec { vk: Some(vk::F1 + 4), mods: Mods::NONE });
        assert_eq!(c.hotkey_name, "F5");
        assert_eq!(c.capture_service, "127.0.0.1:8099".parse().unwrap());
        assert_eq!(c.sample_rate_hz, 250);
        assert_eq!(c.job_workers, 1);
        assert!(c.route_jobtypes.is_empty());
        assert!(DEFAULT_INI.is_ascii());
    }

    #[test]
    fn keys_are_case_insensitive_and_modifiers_parse() {
        let c = parse(
            b"HotKey = ctrl+F12\r\nCAPTURE_SERVICE=10.0.0.2:9000\r\nSample_Rate_Hz = 1000\r\nJob_Workers = 0\r\nROUTE_JOBTYPES = \r\n",
        )
        .unwrap();
        assert_eq!(c.hotkey, KeySpec { vk: Some(vk::F1 + 11), mods: Mods::CTRL });
        assert_eq!(c.capture_service, "10.0.0.2:9000".parse().unwrap());
        assert_eq!(c.sample_rate_hz, 1000);
        assert_eq!(c.job_workers, 0);
    }

    #[test]
    fn every_problem_is_an_error_naming_the_key() {
        const JOBS: &str = "job_workers = 1\nroute_jobtypes =\n";
        const REST: &str = "capture_service = 127.0.0.1:1\nsample_rate_hz = 1000\njob_workers = 1\nroute_jobtypes =\n";
        let with = |s: &str| parse(format!("{s}{REST}").as_bytes());
        let base = |s: &str| parse(format!("hotkey = F5\ncapture_service = 127.0.0.1:1\nsample_rate_hz = 1\n{s}").as_bytes());
        assert!(with("hotkey = F99\n").unwrap_err().contains("names no key"));
        assert!(parse(format!("hotkey = F5\ncapture_service = localhost:1\nsample_rate_hz = 1\n{JOBS}").as_bytes()).unwrap_err().contains("capture_service"));
        assert!(with("hotkey = F5\nport = 3\n").unwrap_err().contains("unknown key \"port\""));
        assert!(parse(REST.as_bytes()).unwrap_err().contains("hotkey is missing"));
        assert!(parse(format!("hotkey = F5\nsample_rate_hz = 1000\n{JOBS}").as_bytes()).unwrap_err().contains("capture_service is missing"));
        assert!(parse(format!("hotkey = F5\ncapture_service = 127.0.0.1:1\n{JOBS}").as_bytes()).unwrap_err().contains("sample_rate_hz is missing"));
        for bad in ["0", "10001", "fast", "-5"] {
            let e = parse(format!("hotkey = F5\ncapture_service = 127.0.0.1:1\nsample_rate_hz = {bad}\n{JOBS}").as_bytes()).unwrap_err();
            assert!(e.contains("1 to 10000"), "{bad}: {e}");
        }
        assert!(base("route_jobtypes =\n").unwrap_err().contains("job_workers is missing"));
        assert!(base("job_workers = 1\n").unwrap_err().contains("route_jobtypes is missing"));
        assert!(base("job_workers = 9\nroute_jobtypes =\n").unwrap_err().contains("0 to 8"));
        assert!(base("job_workers = 1\nroute_jobtypes = ecaed415\n").unwrap_err().contains("not a jobtype hash"));
        assert!(base("job_workers = 1\nroute_jobtypes = 0xecaed415\n").unwrap_err().contains("not been checked"));
        assert!(base("job_workers = 0\nroute_jobtypes = 0xecaed415\n").unwrap_err().contains("no thread"));
    }

    #[test]
    fn ensure_default_writes_once_and_load_reads_it() {
        let dir = std::env::temp_dir().join(format!("mercs2_profiler_cfg_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mercs2_profiler.ini");
        let _ = std::fs::remove_file(&path);
        assert!(ensure_default(&path).unwrap());
        assert!(!ensure_default(&path).unwrap());
        assert_eq!(load(&path).unwrap(), Config::default());
        std::fs::remove_file(&path).unwrap();
    }
}
