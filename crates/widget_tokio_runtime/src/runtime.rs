//! The one way to hand work to the application Tokio runtime.
//!
//! A Tokio runtime is two different things at once, and Tokio's own API spells
//! both of them `Handle`: the *capability* to put work on a runtime, and the
//! *fact* that the current thread is already inside one. `Handle::spawn` needs
//! only the capability, so it is safe from anywhere; `tokio::task::spawn_blocking`
//! and `Handle::current` need the fact, and panic when a thread that is not
//! running the runtime — a GPUI task, most of all — asks for them. Passing a
//! `Handle` around therefore proves nothing about where the work will run, which
//! is why every ambient use of those two APIs is a latent panic rather than a
//! typo.
//!
//! This type carries both, so a caller cannot have one without the other:
//! [`Runtime::blocking`] takes the capability and establishes the fact itself,
//! and [`Runtime::enter`] hands out a guard that proves the fact for as long as
//! it lives. The raw Tokio APIs are denied workspace-wide in `clippy.toml`, so
//! the only way to reach them is through this module:
//!
//! ```text
//! clippy.toml:
//!   disallowed-methods = [{ path = "tokio::task::spawn_blocking", … },
//!                         { path = "tokio::runtime::Handle::current", … }]
//! ```

use std::future::Future;

use tokio::{
    runtime::{Builder, Handle, Runtime as TokioRuntime},
    task::JoinHandle,
};

/// The application Tokio runtime, as the value every service receives.
///
/// It is built and installed by [`crate::global::install`]; services take it from
/// [`crate::global::handle`] or [`crate::global::try_handle`] and never build one
/// of their own.
#[derive(Clone)]
pub struct Runtime {
    handle: Handle,
}

impl Runtime {
    /// Creates the application runtime from a handle the composition root owns.
    ///
    /// The composition root is the one place that legitimately reaches for
    /// `Handle::try_current`: it runs while the runtime it is installing is
    /// entered, and every service afterwards reads the value from globals rather
    /// than asking the thread it happens to be on.
    pub fn from_handle(handle: Handle) -> Self {
        Self { handle }
    }

    /// Runs blocking work on the runtime's blocking pool.
    ///
    /// The runtime context the work needs is established here, so the caller's
    /// thread is irrelevant: a GPUI task, a Tokio worker and a plain
    /// `std::thread` all get a real blocking worker instead of a panic. The
    /// returned handle is a plain value, so it may be awaited on any executor or
    /// dropped to let the work finish detached.
    ///
    /// # Panics
    ///
    /// Panics when the runtime has already been shut down, which only happens
    /// while the application is being torn down.
    #[allow(
        clippy::disallowed_methods,
        reason = "this module is the one place that owns Tokio's context-sensitive APIs"
    )]
    pub fn blocking<T, F>(&self, work: F) -> JoinHandle<T>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        self.handle.spawn_blocking(work)
    }

    /// Runs asynchronous work on the runtime's worker threads.
    ///
    /// `Handle::spawn` needs no context of its own, so this is safe from any
    /// thread; it exists so that handing work to the runtime has one name rather
    /// than one per caller.
    pub fn run<T>(&self, work: impl Future<Output = T> + Send + 'static) -> JoinHandle<T>
    where
        T: Send + 'static,
    {
        self.handle.spawn(work)
    }

    /// Enters the runtime context for as long as the returned guard lives.
    ///
    /// This is the escape hatch for an operation that must itself observe the
    /// context — a third-party call that reaches for `Handle::current` on the
    /// caller's thread. The guard is neither `Send` nor usable across an await,
    /// so it cannot outlive the scope that established the context; prefer
    /// [`Runtime::blocking`] and [`Runtime::run`], which need no guard at all.
    pub fn enter(&self) -> tokio::runtime::EnterGuard<'_> {
        self.handle.enter()
    }

    /// The runtime handle, for a Tokio API that takes one as an argument.
    ///
    /// Taking the handle from here rather than from `Handle::current` is what
    /// keeps a caller from depending on a context it does not have.
    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    /// A runtime for a test that needs one to exist.
    ///
    /// The application has exactly one runtime and installs it, so this exists
    /// only for tests that drive a value whose contract is "hand me the
    /// application runtime". It is the same runtime [`test_handle`] hands out.
    pub fn for_test() -> Self {
        Self::from_handle(test_handle().clone())
    }
}

/// The handle of the runtime shared by every test in the process.
///
/// A widget's unit test drives a binding that spawns its service on a runtime,
/// and the runtime it hands over has to outlive the test: one built inside a test
/// would be shut down before the service it started had finished, and every spawn
/// would answer `JoinError::Cancelled`. It is built once per process and kept for
/// the whole run, which is what makes it a stand-in for the installed
/// application runtime rather than a second one.
///
/// It exists for tests. Production code reads the runtime the composition root
/// installed through [`crate::global::try_handle`].
///
/// # Panics
///
/// Panics when the operating system cannot provide the worker threads.
pub fn test_handle() -> &'static Handle {
    static SHARED: std::sync::OnceLock<TokioRuntime> = std::sync::OnceLock::new();
    SHARED
        .get_or_init(|| {
            Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("a test runtime must build")
        })
        .handle()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::Runtime;

    /// The whole reason this type exists: blocking work started from a thread
    /// that is not running the runtime must run on the runtime, not panic.
    #[test]
    fn blocking_work_runs_from_a_thread_without_a_runtime_context() {
        let application = Runtime::for_test();
        assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "the test thread must start outside every runtime context"
        );

        let worker = application
            .handle()
            .block_on(application.blocking(|| std::thread::current().id()))
            .expect("the blocking worker must not be lost");

        assert_ne!(
            worker,
            std::thread::current().id(),
            "blocking work must leave the caller's thread"
        );
    }

    /// A guard is the honest way to satisfy an API that reads the ambient
    /// context, and it is scoped: this is the fallback shape, not the default.
    #[test]
    fn enter_makes_the_context_observable_within_its_scope_only() {
        let application = Runtime::for_test();

        assert!(tokio::runtime::Handle::try_current().is_err());
        {
            let _entered = application.enter();
            assert!(tokio::runtime::Handle::try_current().is_ok());
        }
        assert!(tokio::runtime::Handle::try_current().is_err());
    }

    /// Work handed to the runtime completes whatever executor awaits it.
    #[test]
    fn run_completes_work_awaited_from_another_executor() {
        let application = Runtime::for_test();

        let answer = application.handle().block_on(application.run(async {
            tokio::time::sleep(Duration::from_millis(1)).await;
            42_u8
        }));

        assert_eq!(answer.expect("the task must complete"), 42);
    }

    /// Every crate the workspace builds, read from the manifests rather than
    /// listed here: a crate added tomorrow is walked without anyone remembering
    /// this test, and a crate removed tomorrow does not leave a stale name behind.
    fn workspace_crates(crates: &std::path::Path) -> Vec<String> {
        let mut found = Vec::new();
        let mut pending = vec![crates.to_path_buf()];
        while let Some(next) = pending.pop() {
            for entry in std::fs::read_dir(&next).expect("a crate directory is readable") {
                let path = entry.expect("a crate entry is readable").path();
                if !path.is_dir() {
                    continue;
                }
                let manifest = path.join("Cargo.toml");
                if manifest.is_file() {
                    found.push(package_name(&manifest));
                } else {
                    pending.push(path);
                }
            }
        }
        found.sort();
        found
    }

    /// The `name` of the `[package]` table, which is the first one in a manifest.
    fn package_name(manifest: &std::path::Path) -> String {
        let text = std::fs::read_to_string(manifest).expect("a manifest is readable");
        text.lines()
            .skip_while(|line| line.trim() != "[package]")
            .find_map(|line| line.trim().strip_prefix("name ="))
            .map(|name| name.trim().trim_matches('"').to_owned())
            .unwrap_or_else(|| panic!("{manifest:?} declares no package name"))
    }

    /// The one module allowed to name the context-sensitive Tokio APIs.
    const SANCTIONED: &str = "crates/widget_tokio_runtime/src/runtime.rs";

    /// The name every test-side escape must go through.
    const TEST_ESCAPE: &str = "fn current_test_handle()";

    /// A denial the whole workspace obeys is only worth what its escapes cost.
    ///
    /// `clippy.toml` denies `tokio::task::spawn_blocking` and
    /// `Handle::current` everywhere, and a crate that must reach them can silence
    /// the lint with one attribute. This walks the workspace and fails on any
    /// escape it cannot account for, which leaves exactly two shapes:
    ///
    /// * Production code names neither API at all.
    /// * A test that has to observe the runtime it runs on does it through one
    ///   named, reasoned helper — a test may not reach for the API at a call site,
    ///   because the reason would then live in an attribute instead of in a name
    ///   a reader can find.
    ///
    /// A denial inside a `#[allow(...)]` therefore has to be justified where it
    /// happens, and the walk is what keeps "justified" from meaning "convenient".
    #[test]
    fn only_the_runtime_module_names_tokios_context_sensitive_apis() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("the runtime crate lives two directories below the workspace root");
        let crates = root.join("crates");
        let mut offenders = Vec::new();
        let mut escapes = Vec::new();
        for source in rust_sources(&crates) {
            if source.ends_with(SANCTIONED) {
                continue;
            }
            let text = std::fs::read_to_string(&source).expect("a source file is readable");
            let lines = text.lines().collect::<Vec<_>>();
            for (index, line) in lines.iter().enumerate() {
                let code = line.split("//").next().unwrap_or_default();
                if !code.contains("task::spawn_blocking") && !code.contains("Handle::current()") {
                    continue;
                }
                // The escape is the function, not the line that calls the API: a
                // call site may not name either API unless the function around it
                // is the one whose name says why.
                let enclosing = lines[..=index]
                    .iter()
                    .rev()
                    .find(|line| line.trim_start().starts_with("fn "))
                    .copied()
                    .unwrap_or_default();
                if is_test_source(&source, &text) && enclosing.contains(TEST_ESCAPE) {
                    escapes.push(source.clone());
                } else {
                    offenders.push(format!("{source}:{}", index + 1));
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "these lines reach a context-sensitive Tokio API without the test escape: {offenders:?}"
        );
        assert!(
            escapes.iter().all(|source| {
                let text = std::fs::read_to_string(source).expect("a source file is readable");
                let lines = text.lines().collect::<Vec<_>>();
                lines.iter().enumerate().all(|(index, line)| {
                    let code = line.split("//").next().unwrap_or_default();
                    if !code.contains("task::spawn_blocking") && !code.contains("Handle::current()")
                    {
                        return true;
                    }
                    let above = lines[index.saturating_sub(6)..index].join("\n");
                    above.contains("clippy::disallowed_methods") && above.contains("reason =")
                })
            }),
            "every test escape must carry the lint exemption with a reason: {escapes:?}"
        );
        assert_eq!(
            workspace_crates(&crates).len(),
            36,
            "the walk must have found every workspace crate: {:?}",
            workspace_crates(&crates)
        );
    }

    /// A test that pauses the clock must drive its subject on its own runtime.
    ///
    /// `#[tokio::test(start_paused = true)]` pauses one runtime's clock and
    /// `tokio::time::advance` moves that one; a service started on a shared
    /// runtime keeps waiting on real time while the test advances a clock it never
    /// reads, and the test hangs instead of failing. Sharing a runtime across
    /// tests is fine everywhere else, which is exactly why this needs to be
    /// checked: the two are one keystroke apart and the symptom is a hang.
    #[test]
    fn tests_in_this_workspace_advance_their_own_clock() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("the runtime crate lives two directories below the workspace root");
        let mut offenders = Vec::new();
        for source in rust_sources(&root.join("crates")) {
            if source.ends_with(SANCTIONED) {
                continue;
            }
            let text = std::fs::read_to_string(&source).expect("a source file is readable");
            // The shared runtime is reached as `runtime::test_handle`; a file that
            // moves a paused clock must not name it. Nothing in production names it
            // at all, so the walk needs no test filter.
            let moves_the_clock = text.contains("start_paused") || text.contains("time::advance(");
            if moves_the_clock && text.contains("runtime::test_handle()") {
                offenders.push(source);
            }
        }

        assert!(
            offenders.is_empty(),
            "these tests move a paused clock while driving work on a shared runtime, \
             which waits on real time instead: {offenders:?}"
        );
    }

    /// Whether a source file is test-only, by the workspace's own layout.
    ///
    /// The workspace keeps tests either in a sibling `*_tests.rs`, in a
    /// `test_support.rs` a module declares only for tests, under a crate's
    /// `tests/` directory, or in an inline `#[cfg(test)] mod tests` at the bottom
    /// of the module it tests. All four are the same thing to this walk: code the
    /// product does not build.
    fn is_test_source(source: &str, text: &str) -> bool {
        source.contains("/tests/")
            || source.ends_with("_tests.rs")
            || source.ends_with("test_support.rs")
            || text.contains("#[cfg(test)]")
    }

    /// Every `.rs` file under `directory`, at any depth.
    fn rust_sources(directory: &std::path::Path) -> Vec<String> {
        let mut found = Vec::new();
        let mut pending = vec![directory.to_path_buf()];
        while let Some(next) = pending.pop() {
            for entry in std::fs::read_dir(&next).expect("a source directory is readable") {
                let path = entry.expect("a source entry is readable").path();
                if path.is_dir() {
                    pending.push(path);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    found.push(path.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        found.sort();
        found
    }
}
