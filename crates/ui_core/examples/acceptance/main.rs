mod headless;
mod interaction;
mod report;
mod scene;
mod windowed;

use std::process::ExitCode;

pub struct Config {
    pub incremental_frames: usize,
    pub warmup_frames: usize,
    pub text_frames: usize,
    pub pan_frames: usize,
    pub scroll_frames: usize,
    pub idle_frames: usize,
    pub window_seconds: f64,
    pub hz: u32,
    pub window_warmup_seconds: f64,
    pub idle_seconds: u64,
    pub idle_warmup_seconds: u64,
    pub hud: bool,
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

const USAGE: &str = "\
usage: acceptance [cpu|breakdown|window|idle|interaction] [options]

modes:
  cpu                headless CPU 测量（默认）
  breakdown          1000 单元文本帧的 pass 归因（定位用，非判据）
  window             真实窗口：帧间隔 / 抖动 / UI 线程帧 CPU
  idle               真实窗口空闲 60 s 的进程 CPU
  interaction        GUI 端到端冒烟（输入 → 布局 → 绘制 → 呈现 → a11y；需要 test-support）

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
        "interaction" | "smoke" => interaction::run(&config),
        other => Err(failure(format!("unknown mode: {other}\n\n{USAGE}"))),
    }
}

fn failure(message: String) -> Box<dyn std::error::Error> {
    Box::<dyn std::error::Error>::from(message)
}
