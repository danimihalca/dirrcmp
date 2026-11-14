//! Smooth scrolling: a jump of N lines is spread over a short time with a sine
//! ease-out (fast start, gentle stop), like neoscroll.nvim's default `sine`.
//!
//! Pure logic with the clock passed in, so it is unit-tested without a terminal.
//! The owner asks `tick` for "how many lines to move right now" every frame.

use std::time::{Duration, Instant};

/// How long to wait between frames while animating (~60 fps).
pub const FRAME: Duration = Duration::from_millis(16);

const MIN_MS: f64 = 120.0;
const MAX_MS: f64 = 450.0;
const PER_LINE_MS: f64 = 5.0;

#[derive(Debug, Default)]
pub struct Scroller {
    /// Lines the current animation moves in total.
    total: isize,
    /// Lines of it already handed out.
    done: isize,
    start: Option<Instant>,
    ms: f64,
}

/// Ease-out sine: 0 -> 1, steepest at the start.
fn ease(x: f64) -> f64 {
    (x.clamp(0.0, 1.0) * std::f64::consts::FRAC_PI_2).sin()
}

impl Scroller {
    pub fn active(&self) -> bool {
        self.start.is_some()
    }

    /// Lines not yet handed out.
    pub fn remaining(&self) -> isize {
        self.total - self.done
    }

    /// Add a scroll of `lines` (negative = up). More in the same direction extends the
    /// animation from where it is; the opposite direction cancels what was left.
    pub fn push(&mut self, lines: isize, now: Instant) {
        if lines == 0 {
            return;
        }
        let left = self.remaining();
        let total = if left != 0 && left.signum() != lines.signum() { lines } else { left + lines };
        self.total = total;
        self.done = 0;
        self.start = Some(now);
        self.ms = (MIN_MS + PER_LINE_MS * total.unsigned_abs() as f64).min(MAX_MS);
    }

    /// Lines to move now. Becomes inactive once everything has been handed out.
    pub fn tick(&mut self, now: Instant) -> isize {
        let Some(start) = self.start else { return 0 };
        let t = now.saturating_duration_since(start).as_secs_f64() * 1000.0 / self.ms;
        let target = if t >= 1.0 {
            self.total
        } else {
            (self.total as f64 * ease(t)).round() as isize
        };
        let step = target - self.done;
        self.done = target;
        if self.done == self.total {
            self.start = None;
            self.total = 0;
            self.done = 0;
        }
        step
    }

    /// Everything that is left, at once (a different key arrived).
    pub fn flush(&mut self) -> isize {
        let left = self.remaining();
        *self = Scroller::default();
        left
    }

    /// Forget the animation (the view was replaced).
    pub fn cancel(&mut self) {
        *self = Scroller::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn spreads_the_distance_and_ends_exactly() {
        let b = Instant::now();
        let mut s = Scroller::default();
        assert!(!s.active());
        s.push(20, b);
        assert!(s.active());
        let mut sum = 0;
        let mut steps = Vec::new();
        for ms in (0..=700).step_by(16) {
            let d = s.tick(at(b, ms));
            steps.push(d);
            sum += d;
        }
        assert_eq!(sum, 20, "{steps:?}");
        assert!(!s.active());
        assert!(steps.iter().all(|d| *d >= 0), "never moves backwards: {steps:?}");
        assert!(steps.iter().filter(|d| **d > 0).count() > 3, "spread over several frames: {steps:?}");
        // ease-out: the first moving steps are larger than the last ones
        let moving: Vec<isize> = steps.into_iter().filter(|d| *d > 0).collect();
        assert!(moving[0] > *moving.last().unwrap(), "{moving:?}");
    }

    #[test]
    fn follows_a_sine_curve() {
        let b = Instant::now();
        let mut s = Scroller::default();
        s.push(100, b);
        let ms = (MIN_MS + PER_LINE_MS * 100.0).min(MAX_MS); // 450
        let half = s.tick(at(b, (ms / 2.0) as u64));
        // sin(pi/4) = 0.7071
        assert!((70..=71).contains(&half), "{half}");
        assert_eq!(half + s.tick(at(b, 10_000)), 100);
    }

    #[test]
    fn same_direction_extends_opposite_cancels() {
        let b = Instant::now();
        let mut s = Scroller::default();
        s.push(10, b);
        let a = s.tick(at(b, 40));
        s.push(10, at(b, 40));
        assert_eq!(s.remaining(), 20 - a);
        // reversing drops what was left and goes the other way
        s.push(-6, at(b, 60));
        assert_eq!(s.remaining(), -6);
        let mut sum = 0;
        for ms in (60..=1000).step_by(16) {
            sum += s.tick(at(b, ms));
        }
        assert_eq!(sum, -6);
        assert!(!s.active());
    }

    #[test]
    fn flush_gives_the_rest_and_resets() {
        let b = Instant::now();
        let mut s = Scroller::default();
        s.push(30, b);
        let a = s.tick(at(b, 50));
        assert_eq!(a + s.flush(), 30);
        assert!(!s.active());
        assert_eq!(s.tick(at(b, 500)), 0);
        assert_eq!(s.flush(), 0);
        s.push(5, b);
        s.cancel();
        assert!(!s.active() && s.remaining() == 0);
        s.push(0, b);
        assert!(!s.active());
    }

    #[test]
    fn duration_grows_with_distance_but_is_capped() {
        let b = Instant::now();
        let finish = |n: isize| {
            let mut s = Scroller::default();
            s.push(n, b);
            (0..2000u64).find(|ms| {
                s.tick(at(b, *ms));
                !s.active()
            })
        };
        let (small, big, huge) = (finish(3).unwrap(), finish(40).unwrap(), finish(10_000).unwrap());
        assert!(small < big && big <= huge);
        assert!(huge <= 451);
    }
}
