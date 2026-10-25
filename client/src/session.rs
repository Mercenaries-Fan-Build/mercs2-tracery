//! The capture session the hotkey drives: idle → starting → recording → saving → idle, one request to
//! the capture service per transition (`mercs2_formats::profd`), and the overlay lines that show it.
//! The service's answer, or the reason it could not be reached, decides each step; the outcome of the
//! last session stays on the overlay until the next one starts.

use mercs2_formats::profd::{Reply, Request};

pub const WHITE: u32 = 0xFFFF_FFFF;
pub const GREY: u32 = 0xFFA0_A0A0;
pub const RED: u32 = 0xFFFF_4040;
pub const YELLOW: u32 = 0xFFFF_D040;
pub const GREEN: u32 = 0xFF60_E060;
/// Overlay lines wrap at this many characters.
pub const WRAP: usize = 72;

#[derive(Debug, Clone, PartialEq)]
enum Phase {
    Idle,
    Starting { name: String },
    Recording { name: String, frame: u32, secs: f64 },
    Saving { name: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    phase: Phase,
    /// What the last session came to: the service's text, or why it failed.
    last: Option<Result<String, String>>,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    pub const fn new() -> Self {
        Session { phase: Phase::Idle, last: None }
    }

    /// The hotkey was pressed: the request to send, or `None` while a request is already out.
    /// `name` is the session a start would record.
    pub fn hotkey(&mut self, name: String) -> Option<Request> {
        match &self.phase {
            Phase::Idle => {
                self.phase = Phase::Starting { name: name.clone() };
                Some(Request::Start { session: name })
            }
            Phase::Recording { name, .. } => {
                self.phase = Phase::Saving { name: name.clone() };
                Some(Request::Stop)
            }
            Phase::Starting { .. } | Phase::Saving { .. } => None,
        }
    }

    /// The service answered the request sent from [`Session::hotkey`], or could not be reached (`Err`), at
    /// frame `frame` and monotonic time `secs`.
    pub fn answered(&mut self, answer: Result<Reply, String>, frame: u32, secs: f64) {
        let phase = std::mem::replace(&mut self.phase, Phase::Idle);
        match (phase, answer) {
            (Phase::Starting { name }, Ok(Reply::Ok(_))) => {
                self.last = None;
                self.phase = Phase::Recording { name, frame, secs };
            }
            (Phase::Starting { name }, Ok(Reply::Err(e)) | Err(e)) => {
                self.last = Some(Err(format!("{name} did not start: {e}")));
            }
            (Phase::Saving { .. }, Ok(Reply::Ok(m))) => self.last = Some(Ok(m)),
            (Phase::Saving { name }, Ok(Reply::Err(e)) | Err(e)) => {
                self.last = Some(Err(format!("{name} was not saved: {e}")));
            }
            (phase @ (Phase::Idle | Phase::Recording { .. }), _) => {
                unreachable!("an answer arrives only for a request sent from Starting or Saving, not {phase:?}")
            }
        }
    }

    /// Whether a capture is connected (frames recorded since `Recording`).
    pub fn recording(&self) -> bool {
        matches!(self.phase, Phase::Recording { .. })
    }

    /// The overlay's session lines at frame `frame`, time `secs`, with `hotkey` as written in the config.
    pub fn lines(&self, frame: u32, secs: f64, hotkey: &str) -> Vec<(String, u32)> {
        let (text, color) = match (&self.phase, &self.last) {
            (Phase::Idle, None) => (format!("idle ({hotkey})"), GREY),
            (Phase::Idle, Some(Ok(m))) => (format!("idle ({hotkey})  {m}"), GREEN),
            (Phase::Idle, Some(Err(e))) => (format!("ERR {e}"), RED),
            (Phase::Starting { name }, _) => (format!("starting {name}..."), YELLOW),
            (Phase::Recording { name, frame: f0, secs: s0 }, _) => {
                let t = (secs - s0).max(0.0) as u64;
                (format!("REC {name}  {:02}:{:02}  {} frames", t / 60, t % 60, frame.saturating_sub(*f0)), RED)
            }
            (Phase::Saving { name }, _) => (format!("saving {name}..."), YELLOW),
        };
        wrap(&text, WRAP).into_iter().map(|l| (l, color)).collect()
    }
}

/// `text` broken into lines of at most `width` characters, at spaces where there is one.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text.trim();
    while rest.chars().count() > width {
        let cut = rest.char_indices().nth(width).map_or(rest.len(), |(i, _)| i);
        // A space right after the first `width` characters: they fit whole.
        let at = if rest[cut..].starts_with(' ') { cut } else { rest[..cut].rfind(' ').filter(|&i| i > 0).unwrap_or(cut) };
        out.push(rest[..at].trim_end().to_owned());
        rest = rest[at..].trim_start();
    }
    out.push(rest.to_owned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(s: &Session, frame: u32, secs: f64) -> (String, u32) {
        let l = s.lines(frame, secs, "F5");
        assert_eq!(l.len(), 1, "{l:?}");
        l[0].clone()
    }

    #[test]
    fn a_session_starts_records_and_saves() {
        let mut s = Session::default();
        assert_eq!(line(&s, 0, 0.0), ("idle (F5)".into(), GREY));
        assert_eq!(s.hotkey("20261022-031650".into()), Some(Request::Start { session: "20261022-031650".into() }));
        assert_eq!(line(&s, 1, 0.0), ("starting 20261022-031650...".into(), YELLOW));
        assert_eq!(s.hotkey("ignored".into()), None, "a press while the start is out sends nothing");
        s.answered(Ok(Reply::Ok("recording 20261022-031650.tracy".into())), 10, 100.0);
        assert!(s.recording());
        assert_eq!(line(&s, 750, 172.4), ("REC 20261022-031650  01:12  740 frames".into(), RED));
        assert_eq!(s.hotkey("unused".into()), Some(Request::Stop));
        assert_eq!(line(&s, 751, 173.0), ("saving 20261022-031650...".into(), YELLOW));
        s.answered(Ok(Reply::Ok("saved 20261022-031650.tracy (9797 bytes, frames: 740)".into())), 760, 174.0);
        assert_eq!(line(&s, 761, 175.0), ("idle (F5)  saved 20261022-031650.tracy (9797 bytes, frames: 740)".into(), GREEN));
    }

    #[test]
    fn a_refused_or_unreachable_start_shows_why_and_returns_to_idle() {
        let mut s = Session::default();
        s.hotkey("a".into());
        s.answered(Ok(Reply::Err("no game answered".into())), 1, 1.0);
        assert_eq!(line(&s, 2, 2.0), ("ERR a did not start: no game answered".into(), RED));
        assert!(s.hotkey("b".into()).is_some(), "the next press starts again");
        s.answered(Err("connecting to 127.0.0.1:8099: refused".into()), 3, 3.0);
        assert_eq!(line(&s, 4, 4.0).0, "ERR b did not start: connecting to 127.0.0.1:8099: refused");
    }

    #[test]
    fn a_failed_save_shows_why() {
        let mut s = Session::default();
        s.hotkey("a".into());
        s.answered(Ok(Reply::Ok(String::new())), 1, 1.0);
        s.hotkey("x".into());
        s.answered(Ok(Reply::Err("no capture is running".into())), 2, 2.0);
        assert_eq!(line(&s, 3, 3.0), ("ERR a was not saved: no capture is running".into(), RED));
    }

    #[test]
    fn long_lines_wrap_at_spaces() {
        assert_eq!(wrap("short", 72), ["short"]);
        assert_eq!(wrap("aaa bbb ccc", 7), ["aaa bbb", "ccc"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"], "no space: cut at the width");
        let mut s = Session::default();
        s.hotkey("a".into());
        s.answered(Err("x ".repeat(60)), 1, 1.0);
        let lines = s.lines(2, 2.0, "F5");
        assert!(lines.len() > 1 && lines.iter().all(|(l, c)| l.len() <= WRAP && *c == RED), "{lines:?}");
    }
}
