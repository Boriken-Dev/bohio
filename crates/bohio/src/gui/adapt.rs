//! What a watch costs the link: the ladder of quality, size and rate that its acknowledgements move along, and the byte
//! rate cap.
//!
//! The host sends at most `frames_in_flight` updates the client has not acknowledged, so a slow link already bounds the
//! delay. The ladder bounds the bytes. An acknowledgement's delay is the link's round trip, the time its own bytes take and
//! what waits ahead of them; only the last two are the watch's to lower, so each delay is judged by what it exceeds the least
//! delay of the last `adapt_window` acknowledgements by (the round trip and an update as small as the watch sends). Excess
//! beyond `adapt_late` requested frame intervals, several times in a row, steps down to a cheaper update (a lower JPEG quality,
//! then a smaller frame, then fewer updates a second); an excess within `adapt_early` steps back up.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::config::{GuiConfig, LadderStep};

/// What an update is made with at one level of the ladder.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Quality {
    /// The JPEG quality a rectangle is capped at; `None` at full quality.
    pub(crate) jpeg: Option<u8>,
    /// Frame pixels per display pixel.
    pub(crate) scale: f64,
    /// Updates a second at most.
    pub(crate) fps: f64,
}

/// The ladder and where a watch stands on it.
#[derive(Debug)]
pub(crate) struct Adapter {
    steps: Vec<LadderStep>,
    base_fps: f64,
    min_fps: f64,
    enabled: bool,
    down_after: u32,
    up_after: u32,
    late: f64,
    early: f64,
    level: u32,
    max_level: u32,
    late_run: u32,
    early_run: u32,
    average_ms: Option<f64>,
    /// The last acknowledgement delays; their least is what a delay is judged against.
    window: VecDeque<Duration>,
    window_len: usize,
}

/// How much of a new acknowledgement delay the average takes.
const WEIGHT: f64 = 0.3;

impl Adapter {
    /// A ladder over `config`'s settings for a watch asking `fps` updates a second. With `steps` false (a lossless watch)
    /// only the rate comes down: quality and size are what the client asked for.
    pub(crate) fn new(config: &GuiConfig, fps: u32, steps: bool) -> Self {
        let steps: Vec<LadderStep> = if steps {
            config
                .adapt_ladder
                .iter()
                .filter_map(|step| LadderStep::parse(step))
                .collect()
        } else {
            Vec::new()
        };
        let base_fps = f64::from(fps.max(1));
        let min_fps = f64::from(config.min_fps).min(base_fps);
        let mut halvings = 0_u32;
        while base_fps / f64::from(1_u32 << halvings.min(30)) > min_fps && halvings < 30 {
            halvings += 1;
        }
        Self {
            max_level: u32::try_from(steps.len()).unwrap_or(u32::MAX) + halvings,
            steps,
            base_fps,
            min_fps,
            enabled: config.adapt,
            down_after: config.adapt_down_after,
            up_after: config.adapt_up_after,
            late: config.adapt_late,
            early: config.adapt_early,
            level: 0,
            late_run: 0,
            early_run: 0,
            average_ms: None,
            window: VecDeque::new(),
            window_len: config.adapt_window.max(1),
        }
    }

    /// The step the watch is on: `0` is full quality at the requested rate.
    pub(crate) const fn level(&self) -> u32 {
        self.level
    }

    /// The average acknowledgement delay, in milliseconds, once there is one.
    pub(crate) fn ack_ms(&self) -> Option<u64> {
        self.average_ms.map(|ms| ms.round().max(0.0) as u64)
    }

    /// What updates are made with now.
    pub(crate) fn quality(&self) -> Quality {
        let in_steps = self.steps.len() as u32;
        let step =
            (self.level > 0 && self.level <= in_steps).then(|| self.steps[self.level as usize - 1]);
        let beyond = self.level.saturating_sub(in_steps);
        let last = self.steps.last();
        let (jpeg, scale) = match (step, last) {
            (Some(step), _) => (Some(step.quality), step.scale),
            // Past the steps the last one stays.
            (None, Some(last)) if self.level > in_steps => (Some(last.quality), last.scale),
            _ => (None, 1.0),
        };
        let fps = (self.base_fps / f64::from(1_u32 << beyond.min(30))).max(self.min_fps);
        Quality { jpeg, scale, fps }
    }

    /// Takes one acknowledgement `delay` after its update was sent. `true` when the level moved.
    pub(crate) fn observe(&mut self, delay: Duration) -> bool {
        let ms = delay.as_secs_f64() * 1000.0;
        self.average_ms = Some(self.average_ms.map_or(ms, |old| old + (ms - old) * WEIGHT));
        if !self.enabled {
            return false;
        }
        self.window.push_back(delay);
        if self.window.len() > self.window_len {
            self.window.pop_front();
        }
        let floor = self.window.iter().min().copied().unwrap_or(delay);
        let excess = delay.saturating_sub(floor);
        let interval = delay_per_frame(self.base_fps);
        if excess > interval.mul_f64(self.late) {
            self.late_run += 1;
            self.early_run = 0;
        } else if excess <= interval.mul_f64(self.early) {
            self.early_run += 1;
            self.late_run = 0;
        } else {
            self.late_run = 0;
            self.early_run = 0;
        }
        if self.late_run >= self.down_after && self.level < self.max_level {
            self.level += 1;
            self.late_run = 0;
            return true;
        }
        if self.early_run >= self.up_after && self.level > 0 {
            self.level -= 1;
            self.early_run = 0;
            return true;
        }
        if self.late_run >= self.down_after {
            self.late_run = 0;
        }
        false
    }
}

fn delay_per_frame(fps: f64) -> Duration {
    Duration::from_secs_f64(1.0 / fps)
}

/// A cap on bytes a second: after an update of `n` bytes the next waits `n / rate`.
#[derive(Debug)]
pub(crate) struct Pace {
    rate: Option<u64>,
    ready_at: Instant,
}

impl Pace {
    pub(crate) fn new(rate: Option<u64>, now: Instant) -> Self {
        Self {
            rate,
            ready_at: now,
        }
    }

    /// When the next update may go.
    pub(crate) const fn ready_at(&self) -> Instant {
        self.ready_at
    }

    /// Records an update of `bytes` sent at `now`.
    pub(crate) fn spent(&mut self, bytes: u64, now: Instant) {
        if let Some(rate) = self.rate {
            let cost = Duration::from_secs_f64(bytes as f64 / rate as f64);
            self.ready_at = self.ready_at.max(now) + cost;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(fps: u32, steps: bool) -> Adapter {
        Adapter::new(&GuiConfig::default(), fps, steps)
    }

    /// The first acknowledgement, which sets the floor; later ones are judged by what they exceed it by.
    const FLOOR: Duration = Duration::from_millis(20);
    const LATE: Duration = Duration::from_millis(900);
    const EARLY: Duration = Duration::from_millis(30);

    /// A ladder that has seen one prompt acknowledgement.
    fn primed(fps: u32, steps: bool) -> Adapter {
        let mut ladder = adapter(fps, steps);
        assert!(!ladder.observe(FLOOR));
        ladder
    }

    #[test]
    fn three_late_acknowledgements_step_down_and_five_early_ones_step_up() {
        let mut ladder = primed(5, true);
        assert_eq!(ladder.quality().jpeg, None);
        assert!(!ladder.observe(LATE));
        assert!(!ladder.observe(LATE));
        assert!(ladder.observe(LATE));
        assert_eq!(ladder.level(), 1);
        assert_eq!(ladder.quality().jpeg, Some(85));
        for _ in 0..3 {
            ladder.observe(LATE);
        }
        assert_eq!(ladder.quality().jpeg, Some(70));
        for _ in 0..4 {
            assert!(!ladder.observe(EARLY));
        }
        assert!(ladder.observe(EARLY));
        assert_eq!(ladder.level(), 1);
    }

    #[test]
    fn an_acknowledgement_between_the_two_breaks_a_run() {
        let mut ladder = primed(5, true);
        ladder.observe(LATE);
        ladder.observe(LATE);
        ladder.observe(Duration::from_millis(220));
        assert!(!ladder.observe(LATE));
        assert_eq!(ladder.level(), 0);
    }

    #[test]
    fn the_ladder_goes_through_quality_and_size_to_the_lowest_rate() {
        let mut ladder = primed(8, true);
        let mut seen = Vec::new();
        for step in 0..60_u64 {
            // Each acknowledgement later than the one before, so the window least never catches up with them.
            ladder.observe(LATE + Duration::from_millis(50 * step));
            let quality = ladder.quality();
            seen.push((quality.jpeg, quality.scale, quality.fps));
        }
        assert!(seen.contains(&(Some(50), 0.75, 8.0)));
        assert!(seen.contains(&(Some(40), 0.5, 8.0)));
        assert_eq!(*seen.last().unwrap(), (Some(40), 0.5, 1.0));
        // The rate halves one step at a time: 8, 4, 2, 1.
        assert!(seen.contains(&(Some(40), 0.5, 4.0)));
        assert!(seen.contains(&(Some(40), 0.5, 2.0)));
        // And it stays at the lowest rung.
        assert_eq!(ladder.level(), 5 + 3);
    }

    #[test]
    fn a_lossless_watch_lowers_only_the_rate_and_a_disabled_ladder_not_at_all() {
        let mut lossless = primed(4, false);
        for _ in 0..3 {
            lossless.observe(LATE);
        }
        let quality = lossless.quality();
        assert_eq!((quality.jpeg, quality.scale, quality.fps), (None, 1.0, 2.0));

        let config = GuiConfig {
            adapt: false,
            ..GuiConfig::default()
        };
        let mut off = Adapter::new(&config, 5, true);
        off.observe(FLOOR);
        for _ in 0..30 {
            assert!(!off.observe(LATE));
        }
        assert_eq!(off.level(), 0);
        assert!(off.ack_ms().is_some());
    }

    #[test]
    fn a_round_trip_longer_than_the_frame_interval_is_not_lateness() {
        // Acknowledgements 300 ms after their updates, at 15 updates a second: all of that is the link round trip.
        let mut steady = Adapter::new(&GuiConfig::default(), 15, true);
        for _ in 0..40 {
            steady.observe(Duration::from_millis(300));
        }
        assert_eq!(steady.level(), 0);
        // The same delays after prompter ones are lateness: the window least is what they are judged against.
        let mut prompt = Adapter::new(&GuiConfig::default(), 15, true);
        for _ in 0..32 {
            prompt.observe(Duration::from_millis(100));
        }
        for _ in 0..3 {
            prompt.observe(Duration::from_millis(300));
        }
        assert_eq!(prompt.level(), 1);
    }

    #[test]
    fn a_pace_makes_the_next_update_wait_for_what_the_last_cost() {
        let start = Instant::now();
        let mut pace = Pace::new(Some(1000), start);
        pace.spent(500, start);
        assert_eq!(pace.ready_at(), start + Duration::from_millis(500));
        // Sent early, it still owes the first cost.
        pace.spent(500, start);
        assert_eq!(pace.ready_at(), start + Duration::from_secs(1));
        let mut free = Pace::new(None, start);
        free.spent(1_000_000, start);
        assert_eq!(free.ready_at(), start);
    }
}
