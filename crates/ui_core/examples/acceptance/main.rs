//! 最终合成负载验收 harness：命令行入口（只做参数解析与模式分发）。
//!
//! 三个模式（三平台同一份二进制）：
//!
//! ```text
//! cargo run --release -p ui_core --example acceptance -- cpu     # 无窗口 CPU 测量（生产特性）
//! cargo run --release -p ui_core --example acceptance -- window  # 真实窗口帧率/抖动（+UI 线程 CPU）
//! cargo run --release -p ui_core --example acceptance -- idle    # 真实窗口空闲 60 s 的进程 CPU
//! ```
//!
//! 口径边界（写进报告）：
//! - **CPU 测量必须用默认特性构建**：`--features test-support` 会在每帧采集 `FrameRecord`
//!   （遍历 scene/tree），本 harness 检测到该特性时会打印警告，且只把 `FrameRecord` 计数当证据；
//! - `cpu` 模式是 headless（`renderer = None`）：编码在 UI 线程内联完成，因此测的是
//!   「样式 + 文本 + 布局 + 绘制 + 实例编码」的**完整 CPU 成本**，不受窗口前台化 / 组合器影响；
//! - `window` / `idle` 模式必须真机真窗口：帧间隔由平台帧时钟给出（vsync 节拍）。

mod headless;
mod report;
mod scene;
mod windowed;

use std::process::ExitCode;

/// 命令行参数（默认值 = 报告里使用的那一套）。
pub struct Config {
    /// 增量帧测量帧数（默认 250，对应「300 帧里 250 帧增量」）。
    pub incremental_frames: usize,
    /// 预热帧数（增量帧，不计入统计；默认 50）。
    pub warmup_frames: usize,
    /// 「1000 单元文本同时更新」帧数（默认 30）。
    pub text_frames: usize,
    /// 只平移折线窗口的帧数（默认 30）。
    pub pan_frames: usize,
    /// 只滚动成交列表的帧数（默认 30）。
    pub scroll_frames: usize,
    /// 空闲帧数（默认 10；必须全部退化为 `Idle`）。
    pub idle_frames: usize,
    /// 真实窗口模式的测量时长（秒，默认 20）。
    pub window_seconds: f64,
    /// 真实窗口模式的数据泵目标频率（Hz，默认 60；`120` 用来验证 120 Hz 节拍能力）。
    pub hz: u32,
    /// 真实窗口模式的预热时长（秒，默认 3）。
    pub window_warmup_seconds: f64,
    /// 空闲 CPU 的采样时长（秒，默认 60）。
    pub idle_seconds: u64,
    /// 空闲 CPU 的预热时长（秒，默认 3；首帧的 a11y 补发等收尾工作不算空闲）。
    pub idle_warmup_seconds: u64,
    /// 打开引擎内置 Debug HUD（仅 `--features hud` 构建有效）。
    pub hud: bool,
    /// 打印逐帧原始样本（`raw,<scenario>,<micros>`；供外部复核分位数）。
    pub raw: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            incremental_frames: 250,
            warmup_frames: 50,
            text_frames: 30,
            pan_frames: 30,
            scroll_frames: 30,
            idle_frames: 10,
            window_seconds: 20.0,
            hz: 60,
            window_warmup_seconds: 3.0,
            idle_seconds: 60,
            idle_warmup_seconds: 3,
            hud: false,
            raw: false,
        }
    }
}

/// 用法。
const USAGE: &str = "\
usage: acceptance [cpu|window|idle] [options]

modes:
  cpu                headless CPU 测量（默认）
  breakdown          1000 单元文本帧的 pass 归因（定位用，非判据）
  window             真实窗口：帧间隔 / 抖动 / UI 线程帧 CPU
  idle               真实窗口空闲 60 s 的进程 CPU

options:
  --frames N         增量帧测量帧数（默认 250）
  --warmup N         增量帧预热帧数（默认 50）
  --text-frames N    1000 单元文本帧数（默认 30）
  --pan-frames N     只平移折线的帧数（默认 30）
  --scroll-frames N  只滚动成交列表的帧数（默认 30）
  --idle-frames N    空闲帧数（默认 10）
  --seconds F        窗口模式测量秒数（默认 20）
  --hz N             窗口模式数据泵目标频率（默认 60；120 = 验证 120 Hz 节拍）
  --window-warmup F  窗口模式预热秒数（默认 3）
  --idle-seconds N   空闲 CPU 采样秒数（默认 60）
  --idle-warmup N    空闲 CPU 预热秒数（默认 3）
  --hud              打开 Debug HUD（需 --features hud 构建）
  --raw              打印逐帧原始样本（供外部复核）
";

fn main() -> ExitCode {
    match run() {
        Ok(passed) => {
            if passed {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(error) => {
            eprintln!("acceptance: {error}");
            ExitCode::from(2)
        }
    }
}

/// 解析参数并跑对应模式；返回 `true` = 全部指标达标。
fn run() -> Result<bool, Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut mode = String::from("cpu");
    let mut config = Config::default();
    let mut positional = 0usize;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(true);
            }
            "--hud" => config.hud = true,
            "--raw" => config.raw = true,
            flag if flag.starts_with("--") => {
                let raw = args
                    .next()
                    .ok_or_else(|| failure(format!("{flag} needs a value")))?;
                let parsed: f64 = raw
                    .parse()
                    .map_err(|error| failure(format!("{flag}: {error}")))?;
                if !parsed.is_finite() || parsed < 0.0 {
                    return Err(failure(format!("{flag}: value out of range")));
                }
                match flag {
                    "--frames" => config.incremental_frames = parsed as usize,
                    "--warmup" => config.warmup_frames = parsed as usize,
                    "--text-frames" => config.text_frames = parsed as usize,
                    "--pan-frames" => config.pan_frames = parsed as usize,
                    "--scroll-frames" => config.scroll_frames = parsed as usize,
                    "--idle-frames" => config.idle_frames = parsed as usize,
                    "--seconds" => config.window_seconds = parsed,
                    "--hz" => config.hz = parsed as u32,
                    "--window-warmup" => config.window_warmup_seconds = parsed,
                    "--idle-seconds" => config.idle_seconds = parsed as u64,
                    "--idle-warmup" => config.idle_warmup_seconds = parsed as u64,
                    _ => return Err(failure(format!("unknown option: {flag}\n\n{USAGE}"))),
                }
            }
            other if other.starts_with('-') => {
                return Err(failure(format!("unknown option: {other}\n\n{USAGE}")));
            }
            other => {
                if positional > 0 {
                    return Err(failure(format!("unexpected argument: {other}\n\n{USAGE}")));
                }
                mode = other.to_owned();
                positional += 1;
            }
        }
    }

    match mode.as_str() {
        "cpu" | "headless" => headless::run(&config),
        "breakdown" => headless::run_breakdown(&config),
        "window" | "fps" => windowed::run_window(&config),
        "idle" => windowed::run_idle(&config),
        other => Err(failure(format!("unknown mode: {other}\n\n{USAGE}"))),
    }
}

/// 字符串错误 → `Box<dyn Error>`（显式类型，避免 `.into()` 的多候选歧义）。
fn failure(message: String) -> Box<dyn std::error::Error> {
    Box::<dyn std::error::Error>::from(message)
}
