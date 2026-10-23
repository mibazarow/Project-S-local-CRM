// The time-based half of run cancellation (SPEC 6.4 harness reliability): the
// run watchdog. Checked at step boundaries; progress is any LLM text delta or
// completed tool call. The flag-based half (the cancel flag) lives with the
// run state in step.rs; this module owns only the elapsed-time caps.
use super::events::RunLimits;
use std::time::{Duration, Instant};

pub(super) struct Watchdog {
    limits: RunLimits,
    // Elapsed time since run start; injectable so tests can fake the clock.
    now: Box<dyn Fn() -> Duration>,
    last_progress: Duration,
}

impl Watchdog {
    pub(super) fn system(limits: &RunLimits) -> Self {
        let started = Instant::now();
        Self::new(limits, Box::new(move || started.elapsed()))
    }

    pub(super) fn new(limits: &RunLimits, now: Box<dyn Fn() -> Duration>) -> Self {
        Watchdog {
            limits: limits.clone(),
            now,
            last_progress: Duration::ZERO,
        }
    }

    pub(super) fn note_progress(&mut self) {
        self.last_progress = (self.now)();
    }

    pub(super) fn check(&self) -> Option<&'static str> {
        let elapsed = (self.now)();
        if elapsed >= self.limits.max_duration {
            Some("timeout")
        } else if elapsed.saturating_sub(self.last_progress) >= self.limits.stall_timeout {
            Some("stalled")
        } else {
            None
        }
    }
}
