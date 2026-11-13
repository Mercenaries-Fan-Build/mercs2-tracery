//! The overlay's frame rate: the interval between successive `IDirect3DDevice9::Present` calls. Unlike the
//! RunFrame time it includes every wait a frame pays outside RunFrame (vsync, the deferred flip), so it is
//! the rate the player sees. Plain logic, tested on the host.

/// Weight of the newest interval in the average.
pub const AVERAGE_WEIGHT: f64 = 0.05;

/// The Present calls seen so far, as the last interval and its running average.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PresentClock {
    last: Option<f64>,
    interval_ms: f64,
    average_ms: f64,
    intervals: u64,
}

impl PresentClock {
    pub const fn new() -> Self {
        PresentClock { last: None, interval_ms: 0.0, average_ms: 0.0, intervals: 0 }
    }

    /// One Present, at `now` seconds on a monotonic clock.
    pub fn present(&mut self, now: f64) {
        if let Some(last) = self.last {
            let ms = (now - last) * 1000.0;
            self.average_ms = if self.intervals == 0 { ms } else { self.average_ms * (1.0 - AVERAGE_WEIGHT) + ms * AVERAGE_WEIGHT };
            self.interval_ms = ms;
            self.intervals += 1;
        }
        self.last = Some(now);
    }

    /// The last interval and the average, in milliseconds; `None` before the second Present.
    pub fn intervals_ms(&self) -> Option<(f64, f64)> {
        (self.intervals > 0).then_some((self.interval_ms, self.average_ms))
    }

    /// The overlay line; `None` before the second Present.
    pub fn line(&self) -> Option<String> {
        let (ms, avg) = self.intervals_ms()?;
        Some(format!("present {:6.1} fps {:5.1} ms  avg {:6.1} fps {:5.1} ms", 1000.0 / ms, ms, 1000.0 / avg, avg))
    }
}

impl Default for PresentClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_line_before_the_second_present() {
        let mut c = PresentClock::new();
        assert_eq!(c.line(), None);
        c.present(1.0);
        assert_eq!(c.intervals_ms(), None);
        assert_eq!(c.line(), None);
    }

    #[test]
    fn the_first_interval_seeds_the_average() {
        let mut c = PresentClock::new();
        c.present(10.0);
        c.present(10.0 + 1.0 / 60.0);
        let (ms, avg) = c.intervals_ms().unwrap();
        assert!((ms - 16.666_666).abs() < 1e-3, "{ms}");
        assert_eq!(ms, avg);
        assert_eq!(c.line().unwrap(), "present   60.0 fps  16.7 ms  avg   60.0 fps  16.7 ms");
    }

    #[test]
    fn later_intervals_move_the_average_by_its_weight() {
        let mut c = PresentClock::new();
        c.present(0.0);
        c.present(0.010);
        c.present(0.030);
        let (ms, avg) = c.intervals_ms().unwrap();
        assert!((ms - 20.0).abs() < 1e-9, "{ms}");
        assert!((avg - (10.0 * (1.0 - AVERAGE_WEIGHT) + 20.0 * AVERAGE_WEIGHT)).abs() < 1e-9, "{avg}");
    }
}
