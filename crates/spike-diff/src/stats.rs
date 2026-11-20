//! Frame timing and row counters for the Phase 0 measurement protocol.
//!
//! - Frame interval: time between successive paints. The view requests a new
//!   frame every frame, so at 60 Hz this sits near 16.7 ms and a dropped frame
//!   shows up as roughly 33 ms.
//! - Frame build: from the start of the root view's render to the end of
//!   paint; the CPU work for one frame, independent of vsync.

use std::time::{Duration, Instant};

pub const LOG_EVERY: Duration = Duration::from_secs(5);
const LONG_FRAME_MS: f32 = 50.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Summary {
    pub frames: usize,
    /// Frames per second over the window. Unlike the interval percentiles this
    /// does not suffer from paint-time jitter, so it is the steadier signal.
    pub fps: f32,
    pub interval_p95: f32,
    pub interval_max: f32,
    pub build_p95: f32,
    pub build_max: f32,
}

pub struct FrameStats {
    render_start: Option<Instant>,
    last_paint: Option<Instant>,
    intervals: Vec<f32>,
    builds: Vec<f32>,
    window_start: Instant,
    /// The last completed 5 s window, shown in the readout.
    pub last: Summary,
    /// Since start or the last reset.
    pub worst_interval: f32,
    pub long_frames: u64,
    pub blank_rows: u64,
    pub unstyled_rows: u64,
    pub first_paint: Option<Duration>,
}

impl FrameStats {
    pub fn new(now: Instant) -> Self {
        Self {
            render_start: None,
            last_paint: None,
            intervals: Vec::with_capacity(1024),
            builds: Vec::with_capacity(1024),
            window_start: now,
            last: Summary::default(),
            worst_interval: 0.0,
            long_frames: 0,
            blank_rows: 0,
            unstyled_rows: 0,
            first_paint: None,
        }
    }

    pub fn render_started(&mut self, now: Instant) {
        self.render_start = Some(now);
    }

    /// Records the end of a frame's paint. Returns a summary when a log window
    /// has just closed.
    pub fn painted(&mut self, now: Instant) -> Option<Summary> {
        if let Some(start) = self.render_start.take() {
            self.builds.push(ms(now - start));
        }
        if let Some(prev) = self.last_paint.replace(now) {
            let interval = ms(now - prev);
            self.intervals.push(interval);
            self.worst_interval = self.worst_interval.max(interval);
            if interval > LONG_FRAME_MS {
                self.long_frames += 1;
            }
        }
        if now - self.window_start < LOG_EVERY {
            return None;
        }
        let window = (now - self.window_start).as_secs_f32();
        self.window_start = now;
        self.last = Summary {
            frames: self.intervals.len(),
            fps: self.intervals.len() as f32 / window,
            interval_p95: p95(&mut self.intervals),
            interval_max: max(&self.intervals),
            build_p95: p95(&mut self.builds),
            build_max: max(&self.builds),
        };
        self.intervals.clear();
        self.builds.clear();
        Some(self.last)
    }

    /// Clears the since-reset counters (not the first-paint time).
    pub fn reset(&mut self, now: Instant) {
        let first_paint = self.first_paint;
        *self = Self::new(now);
        self.first_paint = first_paint;
    }
}

fn ms(duration: Duration) -> f32 {
    duration.as_secs_f32() * 1000.0
}

fn max(values: &[f32]) -> f32 {
    values.iter().copied().fold(0.0, f32::max)
}

/// Nearest-rank 95th percentile; 0 for no samples.
fn p95(values: &mut [f32]) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f32::total_cmp);
    let rank = (values.len() as f32 * 0.95).ceil() as usize;
    values[rank.clamp(1, values.len()) - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p95_is_nearest_rank() {
        let mut values: Vec<f32> = (1..=100).map(|v| v as f32).collect();
        assert_eq!(p95(&mut values), 95.0);
        assert_eq!(p95(&mut [7.0]), 7.0);
        assert_eq!(p95(&mut []), 0.0);
    }

    #[test]
    fn windows_close_every_five_seconds_and_count_long_frames() {
        let start = Instant::now();
        let mut stats = FrameStats::new(start);
        let mut summary = None;
        for frame in 0..=300u64 {
            let at = start + Duration::from_millis(frame * 17 + if frame >= 100 { 60 } else { 0 });
            stats.render_started(at - Duration::from_millis(4));
            summary = stats.painted(at).or(summary);
        }
        let summary = summary.expect("a window closed after 5 s");
        assert!(summary.frames > 250);
        assert!((55.0..=60.0).contains(&summary.fps), "fps {}", summary.fps);
        assert_eq!(stats.long_frames, 1);
        assert!(stats.worst_interval >= 77.0);
        assert!((summary.build_p95 - 4.0).abs() < 0.5);
    }
}
