//! Setup shared by the crate's unit-test modules.

use tokio::runtime::Handle;

/// The handle of the runtime every test in the process shares.
///
/// A runtime built inside one test would be shut down when that test ends, failing the
/// blocking work and the actor threads a still-running peer started; one runtime per
/// process is what makes the handle a stand-in for the application runtime rather than a
/// second one.
///
/// # Panics
///
/// Panics when the operating system cannot provide the runtime's worker threads.
pub(crate) fn test_handle() -> Handle {
    static SHARED: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();

    SHARED
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("a test runtime must build")
        })
        .handle()
        .clone()
}
