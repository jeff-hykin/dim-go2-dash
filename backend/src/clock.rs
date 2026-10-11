// The robot's time on this computer's clock. Each robot stream carries the robot's own time (a stamp from its clock,
// or a tick/RTP counter); this computer's clock is the one every stream must share. The offset between them is the
// smallest (arrival − robot time) seen lately: the message that crossed the network fastest, so a wrong robot clock
// doesn't matter and the network's queueing doesn't either. "Lately" is two 10 s windows, so if one clock runs faster
// than the other the offset follows it, both ways. A robot time that doesn't advance (Unitree firmware sometimes
// freezes or rewinds a stamp) or is missing falls back to the arrival time.

const WINDOW_NS: u64 = 10_000_000_000;

#[derive(Default)]
pub struct RobotClock {
    /// smallest arrival − robot time (ns) in the current window and the one before it
    current: Option<i64>,
    previous: Option<i64>,
    window_started_ns: u64,
    last_robot_ns: Option<i64>,
}

impl RobotClock {
    /// When the message stamped `robot_ns` (the robot's clock, any epoch) happened, on this computer's clock; it
    /// arrived at `arrived_ns`.
    pub fn place(&mut self, robot_ns: Option<i64>, arrived_ns: u64) -> u64 {
        let Some(robot_ns) = robot_ns else { return arrived_ns };
        if self.last_robot_ns.is_some_and(|last| robot_ns <= last) {
            return arrived_ns;
        }
        self.last_robot_ns = Some(robot_ns);
        self.observe(robot_ns, arrived_ns);
        self.local(robot_ns).map_or(arrived_ns, |at| at.min(arrived_ns))
    }

    /// Something stamped `robot_ns` arrived at `arrived_ns`: it narrows the offset.
    pub fn observe(&mut self, robot_ns: i64, arrived_ns: u64) {
        if arrived_ns.saturating_sub(self.window_started_ns) >= WINDOW_NS {
            self.previous = self.current.take();
            self.window_started_ns = arrived_ns;
        }
        let offset = arrived_ns as i64 - robot_ns;
        self.current = Some(self.current.map_or(offset, |o| o.min(offset)));
    }

    /// `robot_ns` on this computer's clock (None before anything was observed).
    pub fn local(&self, robot_ns: i64) -> Option<u64> {
        let offset = match (self.current, self.previous) {
            (Some(c), Some(p)) => c.min(p),
            (c, p) => c.or(p)?,
        };
        Some((robot_ns + offset).max(0) as u64)
    }

    /// Forget the robot's clock (a new connection, or a camera resync: its RTP clock may restart).
    pub fn reset(&mut self) {
        *self = RobotClock::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    #[test]
    fn a_wrong_robot_clock_lands_on_ours_by_the_fastest_arrival() {
        let mut clock = RobotClock::default();
        // the robot's clock is 1000 s behind; messages every 100 ms take 5 ms to arrive, one of them 2 ms
        let ours = 2_000_000 * 1_000 * MS;
        let robot = |i: u64| (ours - 1_000_000 * MS + i * 100 * MS) as i64;
        // the first can only be placed by its own arrival
        assert_eq!(clock.place(Some(robot(0)), ours + 5 * MS), ours + 5 * MS);
        clock.place(Some(robot(1)), ours + 100 * MS + 2 * MS);
        // later messages are placed by the 2 ms one: capture + 2 ms
        assert_eq!(clock.place(Some(robot(2)), ours + 200 * MS + 9 * MS), ours + 200 * MS + 2 * MS);
    }

    #[test]
    fn a_stamp_that_freezes_or_rewinds_falls_back_to_arrival() {
        let mut clock = RobotClock::default();
        clock.place(Some(1_000), 10 * MS);
        assert_eq!(clock.place(Some(1_000), 20 * MS), 20 * MS);
        assert_eq!(clock.place(Some(500), 30 * MS), 30 * MS);
        assert_eq!(clock.place(None, 40 * MS), 40 * MS);
    }

    #[test]
    fn the_offset_follows_a_robot_clock_running_slow() {
        let mut clock = RobotClock::default();
        // the robot's clock loses 1 ms every second; messages arrive 3 ms after they happen
        let mut last = 0;
        for second in 0..60u64 {
            let ours = (second + 1) * 1_000 * MS;
            let robot = (ours - second * MS) as i64;
            last = clock.place(Some(robot), ours + 3 * MS);
        }
        // the old fixed minimum would be 57 ms early by now; the windows keep it within the last ~20 s of drift
        let truth = 60 * 1_000 * MS;
        assert!(truth.abs_diff(last) <= 25 * MS, "{} ms off", truth.abs_diff(last) / MS);
    }
}
