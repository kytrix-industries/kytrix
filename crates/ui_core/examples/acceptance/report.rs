use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct Samples {
    label: String,
    values: Vec<f64>,
}

impl Samples {
    #[must_use]
    pub fn new(label: impl Into<String>, capacity: usize) -> Self {
        Self {
            label: label.into(),
            values: Vec::with_capacity(capacity),
        }
    }

    pub fn push(&mut self, duration: Duration) {
        self.values.push(duration.as_secs_f64() * 1e6);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    #[must_use]
    pub fn values(&self) -> &[f64] {
        &self.values
    }

    #[must_use]
    pub fn summary(&self) -> Option<Summary> {
        Summary::of(&self.values)
    }

    #[must_use]
    pub fn line(&self) -> String {
        match self.summary() {
            Some(summary) => format!("{:<22} {}", self.label, summary.line()),
            None => format!("{:<22} (no samples)", self.label),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Summary {
    pub count: usize,
    pub min_us: f64,
    pub p50_us: f64,
    pub p95_us: f64,
    pub p99_us: f64,
    pub max_us: f64,
    pub mean_us: f64,
    pub stddev_us: f64,
}

impl Summary {
    #[must_use]
    pub fn of(values: &[f64]) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        let mut sorted = values.to_vec();
        sorted.sort_by(f64::total_cmp);
        let count = sorted.len();
        let sum: f64 = sorted.iter().sum();
        let mean = sum / count as f64;
        let variance = sorted
            .iter()
            .map(|value| {
                let delta = value - mean;
                delta * delta
            })
            .sum::<f64>()
            / count as f64;
        Some(Self {
            count,
            min_us: sorted[0],
            p50_us: percentile(&sorted, 0.50),
            p95_us: percentile(&sorted, 0.95),
            p99_us: percentile(&sorted, 0.99),
            max_us: sorted[count - 1],
            mean_us: mean,
            stddev_us: variance.sqrt(),
        })
    }

    #[must_use]
    pub fn line(&self) -> String {
        format!(
            "n={:<4} p50={:>8.3}ms p95={:>8.3}ms p99={:>8.3}ms max={:>8.3}ms mean={:>8.3}ms sd={:>7.3}ms",
            self.count,
            self.p50_us / 1e3,
            self.p95_us / 1e3,
            self.p99_us / 1e3,
            self.max_us / 1e3,
            self.mean_us / 1e3,
            self.stddev_us / 1e3,
        )
    }
}

#[must_use]
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    debug_assert!(!sorted.is_empty());
    let rank = (p * sorted.len() as f64).ceil() as usize;
    let index = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[index]
}

pub struct IntervalReport {
    pub intervals: Samples,
    pub jitter_us: f64,
    pub fps: f64,
    pub outside_1ms: usize,
}

impl IntervalReport {
    #[must_use]
    pub fn new(intervals: &Samples) -> Option<Self> {
        let summary = intervals.summary()?;
        let median = summary.p50_us;
        let rms = (intervals
            .values()
            .iter()
            .map(|value| {
                let delta = value - median;
                delta * delta
            })
            .sum::<f64>()
            / intervals.len() as f64)
            .sqrt();
        let outside_1ms = intervals
            .values()
            .iter()
            .filter(|value| (**value - median).abs() > 1_000.0)
            .count();
        let fps = if median > 0.0 { 1e6 / median } else { 0.0 };
        Some(Self {
            intervals: intervals.clone(),
            jitter_us: rms,
            fps,
            outside_1ms,
        })
    }

    #[must_use]
    pub fn lines(&self) -> String {
        let summary = self.intervals.summary().unwrap_or(Summary {
            count: 0,
            min_us: 0.0,
            p50_us: 0.0,
            p95_us: 0.0,
            p99_us: 0.0,
            max_us: 0.0,
            mean_us: 0.0,
            stddev_us: 0.0,
        });
        format!(
            "frame interval      n={} p50={:.3}ms p95={:.3}ms p99={:.3}ms max={:.3}ms min={:.3}ms\n\
             frame rate          {:.2} fps (median interval)\n\
             jitter (RMS vs p50) {:.3} ms   outside p50±1ms: {} / {}\n\
             interval stddev     {:.3} ms",
            summary.count,
            summary.p50_us / 1e3,
            summary.p95_us / 1e3,
            summary.p99_us / 1e3,
            summary.max_us / 1e3,
            summary.min_us / 1e3,
            self.fps,
            self.jitter_us / 1e3,
            self.outside_1ms,
            self.intervals.len(),
            summary.stddev_us / 1e3,
        )
    }
}

#[must_use]
pub fn cores() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IdleCpu {
    pub wall_secs: f64,
    pub cpu_secs: f64,
}

impl IdleCpu {
    #[must_use]
    pub fn percent_of_core(&self) -> f64 {
        if self.wall_secs > 0.0 {
            self.cpu_secs / self.wall_secs * 100.0
        } else {
            0.0
        }
    }

    #[must_use]
    pub fn percent_of_machine(&self) -> f64 {
        self.percent_of_core() / cores() as f64
    }

    #[must_use]
    pub fn line(&self) -> String {
        format!(
            "idle cpu            {:.4}% of one core ({:.4}% of machine, {} logical core(s)) over {:.1}s: {:.4}s cpu",
            self.percent_of_core(),
            self.percent_of_machine(),
            cores(),
            self.wall_secs,
            self.cpu_secs,
        )
    }
}

#[must_use]
pub fn sample_idle_cpu(duration: Duration) -> Option<IdleCpu> {
    let before = process_cpu_seconds()?;
    let started = Instant::now();
    std::thread::sleep(duration);
    let wall = started.elapsed().as_secs_f64();
    let after = process_cpu_seconds()?;
    Some(IdleCpu {
        wall_secs: wall,
        cpu_secs: (after - before).max(0.0),
    })
}

#[cfg(windows)]
#[must_use]
pub fn process_cpu_seconds() -> Option<f64> {
    use windows::Win32::{
        Foundation::FILETIME,
        System::Threading::{GetCurrentProcess, GetProcessTimes},
    };

    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
        .ok()?;
    }
    Some(filetime_seconds(&kernel) + filetime_seconds(&user))
}

#[cfg(windows)]
fn filetime_seconds(time: &windows::Win32::Foundation::FILETIME) -> f64 {
    let ticks = (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    ticks as f64 / 1e7
}

#[cfg(target_os = "linux")]
#[must_use]
pub fn process_cpu_seconds() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let tail = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = tail.split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some((utime + stime) as f64 / 100.0)
}

#[cfg(target_os = "macos")]
#[must_use]
pub fn process_cpu_seconds() -> Option<f64> {
    let output = std::process::Command::new("ps")
        .args(["-o", "time=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    parse_ps_time(text.trim())
}

#[cfg(target_os = "macos")]
#[must_use]
fn parse_ps_time(text: &str) -> Option<f64> {
    let (hours, minutes, seconds) = match text.split(':').count() {
        3 => {
            let mut parts = text.split(':');
            let hours: f64 = parts.next()?.trim().parse().ok()?;
            let minutes: f64 = parts.next()?.trim().parse().ok()?;
            let seconds: f64 = parts.next()?.trim().parse().ok()?;
            (hours, minutes, seconds)
        }
        2 => {
            let mut parts = text.split(':');
            let minutes: f64 = parts.next()?.trim().parse().ok()?;
            let seconds: f64 = parts.next()?.trim().parse().ok()?;
            (0.0, minutes, seconds)
        }
        _ => return None,
    };
    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}

#[cfg(test)]
mod tests {
    use super::{IdleCpu, IntervalReport, Samples, parse_ps_time, percentile};

    #[test]
    fn percentile_is_nearest_rank() {
        let values = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(percentile(&values, 0.50), 2.0);
        assert_eq!(percentile(&values, 0.75), 3.0);
        assert_eq!(percentile(&values, 1.0), 4.0);
        assert_eq!(percentile(&values, 0.99), 4.0);
    }

    #[test]
    fn summary_keeps_extremes_and_stddev() {
        let mut samples = Samples::new("t", 4);
        for micros in [100.0, 200.0, 300.0, 400.0] {
            samples.push_us(micros);
        }
        let summary = samples.summary().expect("摘要");
        assert_eq!(summary.count, 4);
        assert_eq!(summary.min_us, 100.0);
        assert_eq!(summary.max_us, 400.0);
        assert_eq!(summary.p50_us, 200.0);
        assert!((summary.mean_us - 250.0).abs() < 1e-9);
        assert!((summary.stddev_us - 111.803_398_874_989_9).abs() < 1e-6);
        assert!(samples.line().contains("p50="));
        assert!(Samples::new("empty", 0).summary().is_none());
    }

    #[test]
    fn jitter_is_rms_around_the_median() {
        let mut intervals = Samples::new("interval", 5);
        for micros in [16_000.0, 16_667.0, 17_000.0, 16_500.0, 18_000.0] {
            intervals.push_us(micros);
        }
        let report = IntervalReport::new(intervals).expect("间隔报告");
        let expected = ((-667.0f64).powi(2)
            + 0.0
            + 333.0f64.powi(2)
            + (-167.0f64).powi(2)
            + 1333.0f64.powi(2))
        .sqrt()
            / 5.0f64.sqrt();
        assert!((report.jitter_us - expected).abs() < 1e-6);
        assert!((report.fps - 1e6 / 16_667.0).abs() < 1e-9);
        assert_eq!(report.outside_1ms, 1, "只有 18 ms 那帧越过 ±1 ms");
        assert!(report.lines().contains("fps"));
    }

    #[test]
    fn idle_cpu_reports_both_denominators() {
        let idle = IdleCpu {
            wall_secs: 10.0,
            cpu_secs: 0.01,
        };
        assert!((idle.percent_of_core() - 0.1).abs() < 1e-9);
        assert!(idle.percent_of_machine() <= idle.percent_of_core());
        assert!(idle.line().contains("of one core"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ps_time_formats_are_parsed() {
        assert!((parse_ps_time("0:01.50").expect("mm:ss") - 1.5).abs() < 1e-9);
        assert!((parse_ps_time("2:03.25").expect("hh:mm:ss") - 7383.25).abs() < 1e-9);
        assert!(parse_ps_time("bogus").is_none());
    }
}
