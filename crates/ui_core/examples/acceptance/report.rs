//! 验收报告的数据面：分位数、抖动、进程 CPU 采样与统一格式化。
//!
//! 只做统计与打印，不碰引擎状态（`headless` / `windowed` 两个模式共用）。
//!
//! 口径（写进报告，避免「数字对不上」）：
//! - 分位数用 nearest-rank（`ceil(p·n)-1`），小样本下不插值；
//! - 「帧间隔抖动」= 帧间隔相对**中位数**的 RMS 偏差（标准差口径），单帧尖刺会把它抬高；
//! - 进程 CPU 是**累计 CPU 时间**（用户 + 内核），百分比按「占单核」给出（`ps %cpu` 同口径），
//!   同时给出「占整机」的换算（除以逻辑核数），后者才是任务管理器口径。

use std::time::{Duration, Instant};

/// 一组时长样本（微秒）。容量预留在构造时给足，测量期不分配。
#[derive(Clone)]
pub struct Samples {
    label: String,
    values: Vec<f64>,
}

impl Samples {
    /// 建一个样本集（`capacity` 是预计帧数；避免测量期扩容干扰缓存）。
    #[must_use]
    pub fn new(label: impl Into<String>, capacity: usize) -> Self {
        Self {
            label: label.into(),
            values: Vec::with_capacity(capacity),
        }
    }

    /// 追加一次测量。
    pub fn push(&mut self, duration: Duration) {
        self.values.push(duration.as_secs_f64() * 1e6);
    }

    /// 样本数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// 原始样本（微秒）。
    #[must_use]
    pub fn values(&self) -> &[f64] {
        &self.values
    }

    /// 统计摘要；空样本返回 `None`。
    #[must_use]
    pub fn summary(&self) -> Option<Summary> {
        Summary::of(&self.values)
    }

    /// 一行报告（`label p50=… p95=… p99=… max=… mean=…`，单位毫秒）。
    #[must_use]
    pub fn line(&self) -> String {
        match self.summary() {
            Some(summary) => format!("{:<22} {}", self.label, summary.line()),
            None => format!("{:<22} (no samples)", self.label),
        }
    }
}

/// 一组微秒样本的摘要（毫秒显示，微秒精度保留在 `*_us` 字段）。
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
    /// 从微秒样本计算（nearest-rank 分位数；`stddev` 是总体标准差）。
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

    /// 一行摘要（毫秒显示，三位小数）。
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

/// nearest-rank 分位数（`sorted` 已升序；`p ∈ (0, 1]`）。
#[must_use]
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    debug_assert!(!sorted.is_empty());
    let rank = (p * sorted.len() as f64).ceil() as usize;
    let index = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[index]
}

/// 帧间隔分布（窗口模式）：间隔本身 + 抖动（相对中位数的 RMS 偏差）+ 等效 FPS。
pub struct IntervalReport {
    pub intervals: Samples,
    /// 相对中位数的 RMS 偏差（微秒）。
    pub jitter_us: f64,
    /// 中位间隔换算的等效 FPS。
    pub fps: f64,
    /// 间隔落在「中位数 ±1 ms」之外的帧数。
    pub outside_1ms: usize,
}

impl IntervalReport {
    /// 由帧间隔样本计算。
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

    /// 多行报告。
    #[must_use]
    pub fn lines(&self) -> String {
        let summary = self
            .intervals
            .summary()
            .unwrap_or(Summary {
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

/// 逻辑核数（「占整机 CPU」的换算分母）。
#[must_use]
pub fn cores() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
}

/// 一段空闲窗口的进程 CPU 采样结果。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IdleCpu {
    /// 采样窗口（秒）。
    pub wall_secs: f64,
    /// 窗口内消耗的 CPU 时间（秒，用户 + 内核，全部线程）。
    pub cpu_secs: f64,
}

impl IdleCpu {
    /// 占**单核**的百分比（`ps %cpu` 同口径）。
    #[must_use]
    pub fn percent_of_core(&self) -> f64 {
        if self.wall_secs > 0.0 {
            self.cpu_secs / self.wall_secs * 100.0
        } else {
            0.0
        }
    }

    /// 占**整机**的百分比（任务管理器口径；分母是逻辑核数）。
    #[must_use]
    pub fn percent_of_machine(&self) -> f64 {
        self.percent_of_core() / cores() as f64
    }

    /// 一行报告。
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

/// 让当前进程在 `duration` 内保持运行并测量累计 CPU 时间。
///
/// **阻塞**：调用方应放在后台线程里（窗口模式的空闲测量）。
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

/// 当前进程的累计 CPU 时间（秒，用户 + 内核，含全部线程）。
#[cfg(windows)]
#[must_use]
pub fn process_cpu_seconds() -> Option<f64> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: 四个输出指针都指向本栈帧上的 `FILETIME`；`GetCurrentProcess` 是伪句柄。
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

/// `FILETIME`（100 ns 为单位）→ 秒。
#[cfg(windows)]
fn filetime_seconds(time: &windows::Win32::Foundation::FILETIME) -> f64 {
    let ticks = (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    ticks as f64 / 1e7
}

/// 当前进程的累计 CPU 时间（秒）。
///
/// `/proc/self/stat` 的 `utime`/`stime` 以 `USER_HZ`（内核固定 100，x86_64 / aarch64 同值）为单位；
/// 这两个字段是 14/15 号字段，`comm` 里可能有空格与 `)`，因此从最后一个 `)` 之后开始数。
#[cfg(target_os = "linux")]
#[must_use]
pub fn process_cpu_seconds() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let tail = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = tail.split_whitespace().collect();
    // fields[0] = state（3 号字段）⇒ utime 是 14 号 = fields[11]，stime 是 15 号 = fields[12]。
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some((utime + stime) as f64 / 100.0)
}

/// 当前进程的累计 CPU 时间（秒）。
///
/// macOS 没有 `/proc`，也没有可用的 std 接口读 `getrusage`（引擎不引入 `libc`）：用 `ps -o time=`
/// 的累计时间列（格式 `MM:SS.ss` / `HH:MM:SS`）。只在空闲测量里调用（不在帧循环里）。
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

/// `ps -o time=` 的时间格式：`[HH:]MM:SS[.cc]`（也可能是 `MM:SS`）。
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
        // 小样本下 p99 必须仍落在最大值上，不能越界。
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
        // 中位数 16,667；偏差 = -667 / 0 / +333 / -167 / +1333。
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
