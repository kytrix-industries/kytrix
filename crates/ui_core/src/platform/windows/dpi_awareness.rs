//! DPI 感知（awareness）的声明、查询与启动校验（R07-1）。
//!
//! 进程清单（`ui_core.manifest.xml`，由 `build.rs` 编成 `RT_MANIFEST` 链接进每个链接本 crate
//! 的可执行文件）是**声明**：它是否生效由 OS 与宿主进程决定 —— 宿主已经有自己的清单时本清单
//! 不会被当作进程清单，宿主已经设过 awareness 时清单也会被忽略。所以「DPI 感知正确」不能只
//! 靠清单：启动时必须查询实际值（[`current`]）并按引擎要求校验（[`install`]）。
//!
//! 为什么必须是 PerMonitorV2：`WM_DPICHANGED` 的 `lParam` 建议矩形、`GetDpiForWindow` 的每屏
//! DPI、非客户区按新 DPI 重绘，这三件事都只在 per-monitor v2 下成立。退回 system-aware 后窗口
//! 换屏不会重算 DPI，引擎仍按旧的 scale 排版 —— 这就是 R07-1 说的「静默退回 96 DPI」。

use std::fmt;

use windows::Win32::UI::HiDpi::{
    AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, DPI_AWARENESS_CONTEXT_SYSTEM_AWARE,
    DPI_AWARENESS_PER_MONITOR_AWARE, DPI_AWARENESS_SYSTEM_AWARE, DPI_AWARENESS_UNAWARE,
    GetAwarenessFromDpiAwarenessContext, GetThreadDpiAwarenessContext,
    SetProcessDpiAwarenessContext,
};

use crate::error::EngineError;

/// 引擎要求的 DPI awareness：per-monitor v2（启动校验的唯一判据）。
pub(crate) const REQUIRED: DpiAwareness = DpiAwareness::PerMonitorAwareV2;

/// 进程（线程）实际生效的 DPI awareness（`GetThreadDpiAwarenessContext` 的强类型结果）。
///
/// `PerMonitorAware` 与 `PerMonitorAwareV2` 必须分开：`GetAwarenessFromDpiAwarenessContext`
/// 对两者都返回 `DPI_AWARENESS_PER_MONITOR_AWARE`，只有比较上下文本身才能区分。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DpiAwareness {
    /// 96 dpi 逻辑坐标，系统对整窗做位图拉伸。
    Unaware,
    /// 按进程启动时的系统 DPI 排版；换显示器不重算。
    SystemAware,
    /// 每屏 DPI（v1）。
    PerMonitorAware,
    /// 每屏 DPI v2：`WM_DPICHANGED` 带建议矩形，非客户区也按新 DPI 重绘。
    PerMonitorAwareV2,
    /// 系统返回了引擎不认识的上下文：按「不满足要求」处理，绝不谎报成 aware。
    Unknown,
}

impl DpiAwareness {
    /// 稳定诊断名（`Display` 与日志 grep 用；改文案等于改日志契约）。
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Unaware => "unaware",
            Self::SystemAware => "system-aware",
            Self::PerMonitorAware => "per-monitor (v1)",
            Self::PerMonitorAwareV2 => "per-monitor v2",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for DpiAwareness {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 启动校验失败时的诊断（`Display` 给人读；`actual` / `required` 给测试与日志断言）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DpiAwarenessDiagnostic {
    actual: DpiAwareness,
    required: DpiAwareness,
}

impl DpiAwarenessDiagnostic {
    /// 判定（**纯函数**：输入即实际 awareness，不查 Win32 —— 测试不依赖真机 DPI 设置）。
    pub(crate) fn check(actual: DpiAwareness) -> Result<(), Self> {
        if actual == REQUIRED {
            return Ok(());
        }
        Err(Self {
            actual,
            required: REQUIRED,
        })
    }
}

impl fmt::Display for DpiAwarenessDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "DPI awareness is {}, but this engine requires {}: without it Windows keeps the \
             startup DPI for every window and per-monitor scaling silently degrades to 96 dpi \
             (R07-1)",
            self.actual, self.required
        )
    }
}

/// 消费者：`event_loop::pump::run`。
///
/// 声明 + **校验**进程 DPI awareness（R07-1）。
///
/// 清单已经声明 PerMonitorV2，这里保留 `SetProcessDpiAwarenessContext` 作为「宿主没有清单」
/// 的兜底；**它的返回值不是判据** —— 清单已生效时该调用会返回 `ERROR_ACCESS_DENIED`。真正的
/// 判据是调用之后查询到的实际值：不是 PerMonitorV2 就返回带诊断的 [`EngineError::AppInit`]
/// （7×24 场景里「按错误 scale 排版」比「启动失败」更贵，不静默降级）。
pub(crate) fn install() -> Result<(), EngineError> {
    // SAFETY: 无参数指针的进程级设置；必须在创建任何窗口之前调用（`run` 的第一句）。
    let set = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let actual = current();
    let diagnostic = match DpiAwarenessDiagnostic::check(actual) {
        Ok(()) => {
            // 可观测：与渲染层打印 `GpuCaps` 同口径的一行启动诊断；DPI 感知是速度慢、难复现的
            // 缺陷来源（只有跨屏拖动才现形），不能只能靠「看起来没问题」推断。
            eprintln!("kytrix: DPI awareness = {actual}");
            return Ok(());
        }
        Err(diagnostic) => diagnostic,
    };
    let detail = match set {
        Ok(()) => {
            smol_str::format_smolstr!("{diagnostic}; SetProcessDpiAwarenessContext succeeded")
        }
        Err(error) => smol_str::format_smolstr!(
            "{diagnostic}; SetProcessDpiAwarenessContext = 0x{:08X}",
            error.code().0 as u32
        ),
    };
    Err(EngineError::AppInit { detail })
}

/// 当前线程实际生效的 DPI awareness。
///
/// `GetAwarenessFromDpiAwarenessContext` 只能区分「unaware / system / per-monitor」，对 v1 与
/// v2 都返回 `DPI_AWARENESS_PER_MONITOR_AWARE`；本引擎的判据必须区分两者（[`REQUIRED`]），
/// 所以先用 [`AreDpiAwarenessContextsEqual`] 比较预定义上下文，再退回粗粒度枚举。
pub(crate) fn current() -> DpiAwareness {
    // SAFETY: 无参数查询；返回线程当前上下文（系统持有，不拥有、不释放）。
    let context = unsafe { GetThreadDpiAwarenessContext() };
    // SAFETY: 两个参数都是预定义上下文常量；比较函数只读它们。
    if unsafe { AreDpiAwarenessContextsEqual(context, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
        .as_bool()
    {
        return DpiAwareness::PerMonitorAwareV2;
    }
    // SAFETY: 同上。
    if unsafe { AreDpiAwarenessContextsEqual(context, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE) }
        .as_bool()
    {
        return DpiAwareness::PerMonitorAware;
    }
    // SAFETY: 同上。
    if unsafe { AreDpiAwarenessContextsEqual(context, DPI_AWARENESS_CONTEXT_SYSTEM_AWARE) }
        .as_bool()
    {
        return DpiAwareness::SystemAware;
    }
    // 其余上下文（unaware / unaware-gdiscaled / 非法 / 未来新增）：用粗粒度枚举兜底，
    // 让未知值落进 [`DpiAwareness::Unknown`] 而不是被当成满足要求。
    // SAFETY: 同上（只读查询）；非法上下文返回 `DPI_AWARENESS_INVALID`。
    match unsafe { GetAwarenessFromDpiAwarenessContext(context) } {
        DPI_AWARENESS_SYSTEM_AWARE => DpiAwareness::SystemAware,
        DPI_AWARENESS_PER_MONITOR_AWARE => DpiAwareness::PerMonitorAware,
        DPI_AWARENESS_UNAWARE => DpiAwareness::Unaware,
        _ => DpiAwareness::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::{DpiAwareness, DpiAwarenessDiagnostic, REQUIRED};

    /// R07-1：PerMonitorV2 是唯一通过启动校验的级别。
    #[test]
    fn per_monitor_v2_passes_the_startup_check() {
        assert_eq!(REQUIRED, DpiAwareness::PerMonitorAwareV2);
        assert!(DpiAwarenessDiagnostic::check(DpiAwareness::PerMonitorAwareV2).is_ok());
    }

    /// R07-1：其它任何级别都必须得到**字段被填满**的诊断，且 `Display` 同时点名实际值与要求值
    /// —— 只有一句「DPI 有问题」对真机排障没有价值。
    #[test]
    fn a_lesser_awareness_fills_the_diagnostic() {
        for awareness in [
            DpiAwareness::Unaware,
            DpiAwareness::SystemAware,
            DpiAwareness::PerMonitorAware,
            DpiAwareness::Unknown,
        ] {
            let diagnostic = DpiAwarenessDiagnostic::check(awareness)
                .expect_err("per-monitor v2 之外的级别都必须被拒绝");
            assert_eq!(diagnostic.actual, awareness);
            assert_eq!(diagnostic.required, DpiAwareness::PerMonitorAwareV2);

            let text = diagnostic.to_string();
            assert!(text.contains(awareness.as_str()), "{awareness:?}: {text}");
            assert!(text.contains("per-monitor v2"), "{awareness:?}: {text}");
            assert!(text.contains("R07-1"), "{awareness:?}: {text}");
        }
    }

    /// 诊断名是稳定契约：日志 / CI 用 grep 断言，不能随手改文案。
    #[test]
    fn awareness_display_names_are_stable() {
        assert_eq!(DpiAwareness::Unaware.to_string(), "unaware");
        assert_eq!(DpiAwareness::SystemAware.to_string(), "system-aware");
        assert_eq!(
            DpiAwareness::PerMonitorAware.to_string(),
            "per-monitor (v1)"
        );
        assert_eq!(
            DpiAwareness::PerMonitorAwareV2.to_string(),
            "per-monitor v2"
        );
        assert_eq!(DpiAwareness::Unknown.to_string(), "unknown");
    }
}
