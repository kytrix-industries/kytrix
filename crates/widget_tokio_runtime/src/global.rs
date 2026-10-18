//! The application Tokio runtime installed in GPUI global state.
//!
//! GPUI drives the platform event loop on its own executor, so asynchronous
//! services need a separate Tokio runtime for network IO, timers, and blocking
//! work. The composition root installs exactly one runtime and every consumer
//! only reads it, which keeps the number of executor threads independent of how
//! many services are running.
//!
//! What a consumer reads is [`Runtime`], not a bare `Handle`: a `Handle` is only
//! the capability to put work on the runtime, while Tokio's context-sensitive
//! APIs need the fact that the caller is already inside it. See
//! [`crate::runtime`] for why the two are one value here.

use gpui_kit::{App, Global};
use thiserror::Error;
use tokio::runtime::{Builder, Runtime as TokioRuntime};

use crate::runtime::Runtime;

/// Installs the application Tokio runtime in GPUI global state.
///
/// # Errors
///
/// Fails when the operating system cannot provide the runtime's worker threads.
pub fn install(cx: &mut App) -> Result<(), TokioRuntimeError> {
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(TokioRuntimeError::Build)?;

    cx.set_global(GlobalTokioRuntime::new(runtime));
    Ok(())
}

/// Returns the application runtime every service is spawned on.
///
/// # Panics
///
/// Panics when the composition root did not call [`install`] before the first
/// service was constructed. A missing runtime is a startup defect rather than a
/// recoverable runtime condition, so it fails loudly instead of silently
/// disabling every background service.
pub fn handle(cx: &App) -> Runtime {
    try_handle(cx).expect("the application Tokio runtime must be installed before it is used")
}

/// Returns the application runtime, or `None` when none was installed.
pub fn try_handle(cx: &App) -> Option<Runtime> {
    cx.try_global::<GlobalTokioRuntime>()
        .map(|runtime| runtime.application.clone())
}

/// The process-wide runtime installed in GPUI global state.
struct GlobalTokioRuntime {
    // Declared before the application runtime so the handle is dropped first.
    runtime: Option<TokioRuntime>,
    application: Runtime,
}

impl GlobalTokioRuntime {
    fn new(runtime: TokioRuntime) -> Self {
        let application = Runtime::from_handle(runtime.handle().clone());
        Self {
            runtime: Some(runtime),
            application,
        }
    }
}

impl Global for GlobalTokioRuntime {}

impl Drop for GlobalTokioRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            // `Runtime::drop` blocks until every blocking task finishes, which
            // must not stall the platform teardown of the application.
            runtime.shutdown_background();
        }
    }
}

/// Failure to build the application Tokio runtime.
#[derive(Debug, Error)]
pub enum TokioRuntimeError {
    #[error("failed to create the application Tokio runtime: {0}")]
    Build(#[source] std::io::Error),
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gpui_kit::TestAppContext;

    use super::{TokioRuntimeError, handle, install};

    #[gpui_kit::test]
    fn handle_should_drive_background_futures(cx: &mut TestAppContext) {
        cx.update(|cx| install(cx).expect("test runtime should build"));
        let runtime = cx.update(|cx| handle(cx));

        let result = runtime.handle().block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async { 7_u64 })
                .await
                .expect("the installed runtime should drive timers")
        });

        assert_eq!(result, 7);
    }

    #[gpui_kit::test]
    fn installed_runtime_should_run_blocking_work(cx: &mut TestAppContext) {
        cx.update(|cx| install(cx).expect("test runtime should build"));
        let runtime = cx.update(|cx| handle(cx));

        let answer = runtime
            .handle()
            .block_on(runtime.blocking(|| 3_u64))
            .expect("the blocking worker must not be lost");

        assert_eq!(answer, 3);
    }

    #[test]
    fn runtime_build_failure_is_a_typed_error() {
        let error = TokioRuntimeError::Build(std::io::Error::other("no threads"));

        assert!(error.to_string().contains("no threads"));
    }
}
