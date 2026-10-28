//! Bounded forwarding-delay histograms for taken-over devices.
use std::time::Duration;

const BUCKETS: usize = 256;

/// Delays in four sub-buckets per power of two of nanoseconds.
struct Histogram {
    counts: Box<[u64; BUCKETS]>,
    max: Duration,
}
impl Default for Histogram {
    fn default() -> Self {
        Self {
            counts: Box::new([0; BUCKETS]),
            max: Duration::ZERO,
        }
    }
}
impl Histogram {
    fn record(&mut self, delay: Duration) {
        let nanos = u64::try_from(delay.as_nanos()).unwrap_or(u64::MAX);
        self.counts[index(nanos)] += 1;
        self.max = self.max.max(delay);
    }
    fn count(&self) -> u64 {
        self.counts.iter().sum()
    }
    /// Upper bound of the bucket holding the quantile, at most the maximum.
    fn quantile(&self, quantile: f64) -> Duration {
        let rank = ((self.count() as f64 * quantile).ceil() as u64).max(1);
        let mut seen = 0;
        for (index, count) in self.counts.iter().enumerate() {
            seen += count;
            if seen >= rank {
                return Duration::from_nanos(upper(index)).min(self.max);
            }
        }
        self.max
    }
}
fn index(nanos: u64) -> usize {
    if nanos < 4 {
        return nanos as usize;
    }
    let exponent = 63 - nanos.leading_zeros() as usize;
    4 + (exponent - 2) * 4 + ((nanos >> (exponent - 2)) & 3) as usize
}
fn upper(index: usize) -> u64 {
    if index < 4 {
        return index as u64;
    }
    let exponent = (index - 4) / 4 + 2;
    let next = u128::from(5 + (index as u64 - 4) % 4) << (exponent - 2);
    u64::try_from(next - 1).unwrap_or(u64::MAX)
}
fn show(delay: Duration) -> String {
    if delay < Duration::from_millis(1) {
        format!("{} µs", delay.as_micros())
    } else if delay < Duration::from_secs(1) {
        format!("{:.1} ms", delay.as_secs_f64() * 1e3)
    } else {
        format!("{:.2} s", delay.as_secs_f64())
    }
}

/// Delays of frames forwarded from one device after its takeover cut.
pub(super) struct Forwarding {
    name: String,
    since: Duration,
    held: Histogram,
    overhead: Histogram,
}
impl Forwarding {
    pub(super) fn new(name: String, since: Duration) -> Self {
        Self {
            name,
            since,
            held: Histogram::default(),
            overhead: Histogram::default(),
        }
    }
    /// Records a frame read at `at`, scheduled for `due` and written at `written`.
    pub(super) fn record(&mut self, at: Duration, due: Duration, written: Duration) {
        if at < self.since {
            return;
        }
        if due > at {
            self.held.record(due - at);
        }
        self.overhead.record(written.saturating_sub(due.max(at)));
    }
    pub(super) fn summary(&self) -> String {
        let frames = self.overhead.count();
        if frames == 0 {
            return format!("routing worker: {} forwarded no frames", self.name);
        }
        let mut summary = format!(
            "routing worker: {} forwarded {frames} frames; Prek overhead median ≤ {}, p99 ≤ {}, max {}",
            self.name,
            show(self.overhead.quantile(0.5)),
            show(self.overhead.quantile(0.99)),
            show(self.overhead.max),
        );
        let held = self.held.count();
        if held > 0 {
            summary += &format!(
                "; {held} held for a decision, median ≤ {}, max {}",
                show(self.held.quantile(0.5)),
                show(self.held.max),
            );
        }
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn buckets_cover_every_delay_in_order_within_a_quarter_of_their_bound() {
        let mut last = 0;
        for nanos in (0..64).chain((6..64).map(|shift| (1u64 << shift) + 12_345)) {
            let index = index(nanos);
            assert!(index >= last && index < BUCKETS, "{nanos}");
            assert!(upper(index) >= nanos, "{nanos}");
            assert!(upper(index) as f64 <= nanos as f64 * 1.25 + 1.0, "{nanos}");
            last = index;
        }
        assert_eq!(index(u64::MAX), BUCKETS - 5);
        assert_eq!(upper(index(u64::MAX)), u64::MAX);
    }
    #[test]
    fn a_summary_separates_held_frames_from_overhead_and_skips_the_takeover_prefix() {
        let ms = Duration::from_millis;
        let mut forwarding = Forwarding::new("Touch".into(), ms(10));
        assert_eq!(
            forwarding.summary(),
            "routing worker: Touch forwarded no frames"
        );
        forwarding.record(ms(5), ms(5), ms(900));
        for at in 0..99 {
            forwarding.record(
                ms(10 + at),
                ms(10 + at),
                ms(10 + at) + Duration::from_micros(100),
            );
        }
        forwarding.record(ms(200), ms(240), ms(243));
        let summary = forwarding.summary();
        assert!(
            summary.starts_with("routing worker: Touch forwarded 100 frames;"),
            "{summary}"
        );
        assert!(summary.contains("overhead median ≤ 114 µs"), "{summary}");
        assert!(summary.contains("max 3.0 ms"), "{summary}");
        assert!(
            summary.contains("1 held for a decision, median ≤ 40.0 ms, max 40.0 ms"),
            "{summary}"
        );
    }
}
