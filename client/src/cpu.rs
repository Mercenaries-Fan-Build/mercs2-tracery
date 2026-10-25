//! Per-thread CPU over a session: the difference between two snapshots of every thread of the game process
//! (taken at the session's start and stop), so a session shows whether the main thread, the one the
//! sampler samples, is where the CPU goes, and which other threads (job workers, the driver, Wine) burn
//! time. A host that reads every thread as 0 ms (Wine on macOS) is reported as not reporting per-thread
//! CPU, with the thread list alone.

/// One thread in a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thread {
    pub tid: u32,
    /// The address the thread started at; `None` when it could not be read.
    pub start_va: Option<u32>,
    /// Where the thread started, as `module+offset`, or the raw address outside any module.
    pub start: String,
    /// Kernel plus user time, in 100 ns units; `None` when the thread exited before it could be read.
    pub cpu_100ns: Option<u64>,
}

/// The report of a session: one line per thread, busiest first, and a one-line summary.
pub fn report(before: &[Thread], after: &[Thread], main: u32, wall_secs: f64) -> (Vec<String>, String) {
    let ms = |t: u64| t as f64 / 10_000.0;
    let wall_ms = (wall_secs * 1000.0).max(f64::EPSILON);
    let mut rows: Vec<(u64, String)> = Vec::new();
    let mut unread = 0usize;
    for a in after {
        let b = before.iter().find(|b| b.tid == a.tid);
        let (delta, note) = match (a.cpu_100ns, b.and_then(|b| b.cpu_100ns)) {
            (Some(now), Some(then)) => (now.saturating_sub(then), ""),
            (Some(now), None) if b.is_none() => (now, "  (started in the session)"),
            _ => {
                unread += 1;
                continue;
            }
        };
        let mark = if a.tid == main { "  main thread (sampled)" } else { "" };
        rows.push((delta, format!("tid {:>6}  {:<40} {:>10.1} ms  {:>5.1}% of a core{mark}{note}", a.tid, a.start, ms(delta), 100.0 * ms(delta) / wall_ms)));
    }
    let exited = before.iter().filter(|b| !after.iter().any(|a| a.tid == b.tid)).count();
    rows.sort_by(|x, y| y.0.cmp(&x.0));
    let total: u64 = rows.iter().map(|r| r.0).sum();
    if total == 0 && !rows.is_empty() {
        // Wine on macOS answers GetThreadTimes with 0 for every thread: the host does not report it.
        let mut lines: Vec<String> = after.iter().map(|a| format!("tid {:>6}  {}", a.tid, a.start)).collect();
        if exited > 0 {
            lines.push(format!("{exited} thread(s) exited during the session"));
        }
        let summary = format!("per-thread CPU is not reported by this host: all {} threads read 0 ms over {wall_secs:.1} s", rows.len());
        return (lines, summary);
    }
    let main_cpu = after
        .iter()
        .find(|a| a.tid == main)
        .and_then(|a| Some(a.cpu_100ns?.saturating_sub(before.iter().find(|b| b.tid == main)?.cpu_100ns?)));
    let mut lines: Vec<String> = rows.into_iter().map(|r| r.1).collect();
    if exited > 0 || unread > 0 {
        lines.push(format!("{exited} thread(s) exited during the session and {unread} could not be read; their CPU is not counted"));
    }
    let main_text = match main_cpu {
        Some(m) => format!("main thread {:.0} ms ({:.0}% of the process)", ms(m), 100.0 * m as f64 / total.max(1) as f64),
        None => "main thread not read".to_string(),
    };
    let summary = format!("process CPU {:.0} ms over {wall_secs:.1} s ({:.2} cores); {main_text}", ms(total), ms(total) / wall_ms);
    (lines, summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(tid: u32, start: &str, cpu: Option<u64>) -> Thread {
        Thread { tid, start_va: None, start: start.into(), cpu_100ns: cpu }
    }

    #[test]
    fn a_host_that_reads_every_thread_as_idle_says_so() {
        let before = [t(1, "Mercenaries2.exe+0x231590", Some(0)), t(2, "Mercenaries2.exe+0x476400", Some(0))];
        let after = [t(1, "Mercenaries2.exe+0x231590", Some(0)), t(2, "Mercenaries2.exe+0x476400", Some(0))];
        let (lines, summary) = report(&before, &after, 1, 120.0);
        assert_eq!(summary, "per-thread CPU is not reported by this host: all 2 threads read 0 ms over 120.0 s");
        assert_eq!(lines, ["tid      1  Mercenaries2.exe+0x231590", "tid      2  Mercenaries2.exe+0x476400"]);
    }

    #[test]
    fn a_session_reports_each_thread_busiest_first() {
        let before = [t(1, "Mercenaries2.exe+0x231590", Some(10_000_000)), t(2, "Mercenaries2.exe+0x476400", Some(0)), t(3, "x.dll+0x10", Some(5))];
        let after = [
            t(1, "Mercenaries2.exe+0x231590", Some(60_000_000)),
            t(2, "Mercenaries2.exe+0x476400", Some(80_000_000)),
            t(4, "d3d9.dll+0x99", Some(10_000_000)),
        ];
        let (lines, summary) = report(&before, &after, 1, 10.0);
        assert!(lines[0].contains("+0x476400") && lines[0].contains("8000.0 ms") && lines[0].contains("80.0% of a core"), "{lines:?}");
        assert!(lines[1].contains("5000.0 ms") && lines[1].contains("main thread (sampled)"), "{lines:?}");
        assert!(lines[2].contains("started in the session"), "{lines:?}");
        assert!(lines[3].contains("1 thread(s) exited"), "{lines:?}");
        assert_eq!(summary, "process CPU 14000 ms over 10.0 s (1.40 cores); main thread 5000 ms (36% of the process)");
    }

    #[test]
    fn an_unreadable_thread_is_counted_not_hidden() {
        let (lines, summary) = report(&[t(1, "a", Some(0))], &[t(1, "a", None), t(2, "b", Some(10_000))], 1, 1.0);
        assert!(lines.iter().any(|l| l.contains("1 could not be read")), "{lines:?}");
        assert!(summary.ends_with("main thread not read"), "{summary}");
    }
}
