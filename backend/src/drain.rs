// A battery's time left from its recent draw: the mean draw (A or W) over the last few minutes against what's left in it
// (Ah or Wh). Nothing until it has seen a full minute of discharging, so a moment's spike or idle isn't a prediction.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// how far back the mean draw looks
const WINDOW: Duration = Duration::from_secs(5 * 60);
/// how much draw it needs before it predicts
const MIN_SPAN: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct Drain {
    /// (when, draw out of the battery), oldest first
    samples: VecDeque<(Instant, f64)>,
}

impl Drain {
    /// One reading of the draw (positive = discharging); a charge (≤ 0) starts over.
    pub fn add(&mut self, at: Instant, draw: f64) {
        if !(draw > 0.0) {
            self.samples.clear();
            return;
        }
        self.samples.push_back((at, draw));
        while self.samples.front().is_some_and(|(t, _)| at.duration_since(*t) > WINDOW) {
            self.samples.pop_front();
        }
    }

    /// Seconds until `remaining` (same unit as the draw, × hours) runs out at the mean draw; None under a minute of draw.
    pub fn seconds_left(&self, remaining: f64) -> Option<u32> {
        let (first, _) = self.samples.front()?;
        let (last, _) = self.samples.back()?;
        if last.duration_since(*first) < MIN_SPAN {
            return None;
        }
        let mean = self.samples.iter().map(|(_, d)| d).sum::<f64>() / self.samples.len() as f64;
        Some((remaining.max(0.0) / mean * 3600.0).round() as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_a_minute_then_uses_the_mean() {
        let t0 = Instant::now();
        let mut drain = Drain::default();
        drain.add(t0, 4.0);
        drain.add(t0 + Duration::from_secs(30), 8.0);
        assert_eq!(drain.seconds_left(6.0), None);
        drain.add(t0 + Duration::from_secs(60), 6.0);
        // 6 Ah at a mean 6 A: an hour
        assert_eq!(drain.seconds_left(6.0), Some(3600));
    }

    #[test]
    fn charging_starts_over_and_old_draw_ages_out() {
        let t0 = Instant::now();
        let mut drain = Drain::default();
        drain.add(t0, 5.0);
        drain.add(t0 + Duration::from_secs(90), 5.0);
        drain.add(t0 + Duration::from_secs(100), -2.0);
        assert_eq!(drain.seconds_left(5.0), None);
        drain.add(t0 + Duration::from_secs(200), 1.0);
        drain.add(t0 + Duration::from_secs(300), 1.0);
        drain.add(t0 + Duration::from_secs(600), 3.0);
        // the 200 s reading is over 5 min old: mean of 1 and 3
        assert_eq!(drain.seconds_left(2.0), Some(3600));
    }
}
