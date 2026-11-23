use std::fmt;

use windows::Win32::UI::HiDpi::{
    AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, DPI_AWARENESS_CONTEXT_SYSTEM_AWARE,
    DPI_AWARENESS_PER_MONITOR_AWARE, DPI_AWARENESS_SYSTEM_AWARE, DPI_AWARENESS_UNAWARE,
    GetAwarenessFromDpiAwarenessContext, GetThreadDpiAwarenessContext,
    SetProcessDpiAwarenessContext,
};

use crate::error::EngineError;

pub(crate) const REQUIRED: DpiAwareness = DpiAwareness::PerMonitorAwareV2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DpiAwareness {
    Unaware,
    SystemAware,
    PerMonitorAware,
    PerMonitorAwareV2,
    Unknown,
}

impl DpiAwareness {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DpiAwarenessDiagnostic {
    actual: DpiAwareness,
    required: DpiAwareness,
}

impl DpiAwarenessDiagnostic {
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

pub(crate) fn install() -> Result<(), EngineError> {
    let set = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let actual = current();
    let diagnostic = match DpiAwarenessDiagnostic::check(actual) {
        Ok(()) => {
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

pub(crate) fn current() -> DpiAwareness {
    let context = unsafe { GetThreadDpiAwarenessContext() };
    if unsafe { AreDpiAwarenessContextsEqual(context, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
        .as_bool()
    {
        return DpiAwareness::PerMonitorAwareV2;
    }
    if unsafe { AreDpiAwarenessContextsEqual(context, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE) }
        .as_bool()
    {
        return DpiAwareness::PerMonitorAware;
    }
    if unsafe { AreDpiAwarenessContextsEqual(context, DPI_AWARENESS_CONTEXT_SYSTEM_AWARE) }
        .as_bool()
    {
        return DpiAwareness::SystemAware;
    }
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

    #[test]
    fn per_monitor_v2_passes_the_startup_check() {
        assert_eq!(REQUIRED, DpiAwareness::PerMonitorAwareV2);
        assert!(DpiAwarenessDiagnostic::check(DpiAwareness::PerMonitorAwareV2).is_ok());
    }

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
