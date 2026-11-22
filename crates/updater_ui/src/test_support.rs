//! Test seams for the updater's own tests: the engine host, the product catalogue,
//! and a download manager that answers on demand.
//!
//! The harness is the same shape a non-widget application-layer crate uses
//! (`instrument_selector_ui/src/test_support.rs`): a headless `TestApp`, the real
//! `WidgetServices` aggregate assembled exactly as the composition root assembles it
//! — with its own `Layer` / `FrameRouter` / `AnimationRouter` singletons — and one
//! `drain` that runs the composition root's routes and then one engine frame. The
//! updater is not a widget, so the contract layer's `WidgetTestApp` does not fit it.
//!
//! The download manager is a fake: the real one is still a toolkit entity, and what
//! the updater depends on is the seam's contract — an added task, the events a
//! manager publishes, and the operations answering whether they took. A test drives
//! those events synchronously, so the download states are asserted without a
//! network, a file system race or a wall clock.

use std::{
    cell::RefCell,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use ahash::AHashMap;
use base_ui::{
    kit::{layer::Layer, overlay::AnimationRouter},
    theme::{appearance::Appearance, handle::ThemeHandle},
};
use download_provider::{
    request::DownloadRequest,
    task::{DownloadEvent, DownloadId, DownloadStatus, DownloadTask},
};
use i18n::{global::embedded, language::codes::LanguageCode, translate::Translate};
use ui_core::{
    math::Size,
    node::{id::NodeId, tree::NodeTree},
    test_support::app::TestApp,
    window::{WindowId, WindowOptions},
};
use url::Url;
use widget::{frame::FrameRouter, services::WidgetServices};

use crate::{
    download::{DownloadService, DownloadSubscription},
    indicator::UpdateIndicator,
    release::{PlatformKey, PlatformRelease, ReleaseFormat, ReleaseInfo, ReleaseNotes},
    updater::{Updater, UpdaterDeps},
};

/// The key prefix the harness's controls are built with.
pub(crate) const KEY: &str = "test.updater";

/// The release the tests offer.
pub(crate) fn release(version: &str) -> ReleaseInfo {
    ReleaseInfo {
        key: PlatformKey::current().expect("the tests run on a packaged platform"),
        version: semver::Version::parse(version).expect("the fixture version is a semver"),
        notes: Some(ReleaseNotes::for_test(&[(
            "en",
            "## What changed\n\n- everything",
        )])),
        pub_date: None,
        asset: PlatformRelease {
            url: Url::parse("https://example.invalid/kytrix-setup.exe")
                .expect("the fixture payload URL is a URL"),
            format: ReleaseFormat::Nsis,
        },
    }
}

/// The same release with only the notes a test publishes.
pub(crate) fn release_with_notes(notes: &[(&'static str, &'static str)]) -> ReleaseInfo {
    ReleaseInfo {
        notes: Some(ReleaseNotes::for_test(notes)),
        ..release("1.2.3")
    }
}

/// How long a test waits for work that really runs asynchronously.
const WAIT_LIMIT: Duration = Duration::from_secs(5);

/// The updater's test host.
pub(crate) struct Harness {
    pub app: TestApp,
    pub services: Rc<WidgetServices>,
    pub router: Rc<FrameRouter>,
    pub layer: Rc<Layer>,
    /// The runtime the updater's asynchronous work is spawned on.
    ///
    /// A current-thread runtime, driven explicitly by [`Harness::run_runtime`]: no
    /// test depends on a worker thread finishing on its own.
    runtime: tokio::runtime::Runtime,
    downloads: Rc<FakeDownloads>,
    updater: Option<Updater>,
    indicator: Option<UpdateIndicator>,
}

impl Harness {
    /// A 1280 × 900 host: the notes panel is 1040 px wide, so a smaller window would
    /// not hold it.
    pub(crate) fn new() -> Self {
        let mut app = TestApp::with_options(WindowOptions {
            size: Size::new(1280.0, 900.0),
            ..WindowOptions::default()
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("the test runtime builds");
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let layer = Rc::new(Layer::new());
        let router =
            Rc::new(FrameRouter::new(app.app_mut()).expect("the test host owns the frame hook"));
        let animation = Rc::new(AnimationRouter::new());
        let services = Rc::new(
            WidgetServices::builder(
                app.app_mut(),
                runtime.handle().clone(),
                translate(LanguageCode::En),
                theme,
                Rc::clone(&layer),
                Rc::clone(&router),
                Rc::clone(&animation),
            )
            .build(),
        );
        Self {
            app,
            services,
            router,
            layer,
            runtime,
            downloads: FakeDownloads::new(),
            updater: None,
            indicator: None,
        }
    }

    /// Installs the updater over the harness's services, reading metadata from
    /// `endpoint` when one is given.
    pub(crate) fn install_updater(&mut self, endpoint: Option<Url>) -> Updater {
        let window = self.window();
        let mut deps = UpdaterDeps::new(
            Arc::new(network::transport::hub::TransportHub::new(
                network::transport::hub::TransportPolicy {
                    allow_plaintext: true,
                    extra_root_certificates: Vec::new(),
                },
            )),
            self.downloads.clone(),
        );
        if let Some(endpoint) = endpoint {
            deps = deps.with_endpoint(endpoint);
        }
        let updater = Updater::install_with_key(
            self.app.app_mut(),
            window,
            Rc::clone(&self.services),
            deps,
            KEY,
        );
        self.updater = Some(updater.clone());
        updater
    }

    /// Mounts the indicator into the window's root and returns it.
    pub(crate) fn mount_indicator(&mut self) -> UpdateIndicator {
        let updater = self.updater.clone().expect("the updater is installed");
        let window = self.window();
        let parent = self.root();
        let indicator = UpdateIndicator::mount(
            self.app.app_mut(),
            window,
            parent,
            Rc::clone(&self.services),
            updater,
            KEY,
        );
        self.indicator = Some(indicator.clone());
        indicator
    }

    /// Runs the composition root's routes and then one engine frame.
    pub(crate) fn drain(&mut self) {
        if let Err(error) = self.router.tick(self.app.app_mut()) {
            panic!("a frame route failed: {error}");
        }
        let _ = self.app.drain().expect("the harness frame");
    }

    /// Runs `frames` production-shaped frames.
    pub(crate) fn settle(&mut self, frames: usize) {
        for _ in 0..frames {
            self.drain();
        }
    }

    /// Advances the engine clock by `by` and runs one frame (E10 timers are dispatched
    /// at frame entry, so a callback's writes are visible in this frame's paint).
    pub(crate) fn advance(&mut self, by: Duration) {
        self.app.advance(by);
        self.drain();
    }

    /// Drives the updater's runtime for `duration`, so spawned work makes progress.
    ///
    /// Deterministic tests never need this (they hand the updater its messages); it is
    /// for the paths that really spawn — reading a completed payload's file.
    pub(crate) fn run_runtime(&mut self, duration: Duration) {
        self.runtime.block_on(async {
            tokio::time::sleep(duration).await;
        });
    }

    /// Waits for `condition`, alternating runtime turns and frames.
    pub(crate) fn wait_until(
        &mut self,
        what: &str,
        mut condition: impl FnMut(&mut Harness) -> bool,
    ) {
        let deadline = Instant::now() + WAIT_LIMIT;
        loop {
            if condition(self) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            self.run_runtime(Duration::from_millis(5));
            self.drain();
        }
    }

    /// Switches the language the way the composition root does: translator first, then
    /// the signal, so a redraw can only draw the new language.
    pub(crate) fn switch_language(&mut self, language: LanguageCode) {
        self.services.translate.set_language(language);
        let signal = self.services.language;
        let _ = self.app.app_mut().write(&signal, language);
    }

    /// The engine's value-slot census (released assertions read `live`, reuse reads
    /// `slots`).
    pub(crate) fn signal_slot_stats(&self) -> ui_core::test_support::diagnostics::SignalSlotStats {
        ui_core::test_support::diagnostics::signal_slot_stats(self.app.app())
    }

    pub(crate) fn window(&self) -> WindowId {
        self.app.window_id()
    }

    pub(crate) fn root(&self) -> NodeId {
        self.app
            .app()
            .window(self.window())
            .expect("the harness window")
            .tree()
            .root()
    }

    pub(crate) fn tree(&self) -> &NodeTree {
        self.app
            .app()
            .window(self.window())
            .expect("the harness window")
            .tree()
    }

    pub(crate) fn updater(&self) -> &Updater {
        self.updater.as_ref().expect("the updater is installed")
    }

    pub(crate) fn downloads(&self) -> &FakeDownloads {
        &self.downloads
    }

    /// The text the subtree under `node` draws, in document order, one line per text
    /// node.
    pub(crate) fn text_of(&self, node: NodeId) -> String {
        let tree = self.tree();
        let mut texts = Vec::new();
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            let target = tree.get(current).expect("the node exists");
            if let ui_core::node::tree::NodeKind::Text(content) = target.kind() {
                texts.push(content.text.to_string());
            }
            stack.extend(target.children().iter().rev().copied());
        }
        texts.join("\n")
    }

    /// The first node under `parent` whose debug id starts with `prefix`.
    pub(crate) fn find_in(&self, parent: NodeId, prefix: &str) -> Option<NodeId> {
        let tree = self.tree();
        let mut stack = vec![parent];
        while let Some(current) = stack.pop() {
            if tree
                .debug_id(current)
                .is_some_and(|id| id.starts_with(prefix))
            {
                return Some(current);
            }
            stack.extend(
                tree.get(current)
                    .expect("node")
                    .children()
                    .iter()
                    .rev()
                    .copied(),
            );
        }
        None
    }

    /// The first node under the window's root whose debug id starts with `prefix`.
    pub(crate) fn find(&self, prefix: &str) -> Option<NodeId> {
        self.find_in(self.root(), prefix)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // A test may leave a dialog open, and one test closes the window; the app's
        // pump must not be stepped after that.
        if self.app.app().window(self.window()).is_none() {
            return;
        }
        if let Some(indicator) = self.indicator.take() {
            indicator.release(self.app.app_mut());
        }
        if let Some(updater) = self.updater.take() {
            updater.release(self.app.app_mut());
        }
        let _ = self.router.tick(self.app.app_mut());
        let _ = self.app.drain();
    }
}

/// The product catalogue's translation context in one language.
pub(crate) fn translate(language: LanguageCode) -> Translate {
    let catalogue = embedded::load().expect("the embedded locales load");
    Translate::new(Rc::new(catalogue), language)
}

/// A download manager that answers on demand.
pub(crate) struct FakeDownloads {
    state: RefCell<FakeState>,
    /// The one sink the updater installed, held apart from the task table so the
    /// subscription's drop can withdraw it through a plain `Rc`.
    sink: Rc<RefCell<Option<Arc<dyn Fn(DownloadEvent) + Send + Sync>>>>,
}

struct FakeState {
    tasks: AHashMap<DownloadId, DownloadTask>,
    requests: Vec<DownloadRequest>,
    deleted: Vec<DownloadId>,
    last: Option<DownloadId>,
}

impl FakeDownloads {
    pub(crate) fn new() -> Rc<Self> {
        Rc::new(Self {
            state: RefCell::new(FakeState {
                tasks: AHashMap::default(),
                requests: Vec::new(),
                deleted: Vec::new(),
                last: None,
            }),
            sink: Rc::new(RefCell::new(None)),
        })
    }

    /// The identity of the task that was added last.
    pub(crate) fn last_task(&self) -> Option<DownloadId> {
        self.state.borrow().last
    }

    /// Every request the updater added, in order.
    pub(crate) fn requests(&self) -> Vec<DownloadRequest> {
        self.state.borrow().requests.clone()
    }

    /// The tasks the manager was asked to delete, in order.
    pub(crate) fn deleted(&self) -> Vec<DownloadId> {
        self.state.borrow().deleted.clone()
    }

    /// How many subscriptions were installed.
    pub(crate) fn subscriptions(&self) -> usize {
        usize::from(self.sink.borrow().is_some())
    }

    /// Publishes one task update, as the manager does when a transfer reports.
    pub(crate) fn publish(&self, id: DownloadId, status: DownloadStatus) {
        let task = {
            let mut state = self.state.borrow_mut();
            let Some(task) = state.tasks.get_mut(&id) else {
                return;
            };
            task.status = status;
            task.clone()
        };
        self.event(DownloadEvent::TaskUpdated(task));
    }

    /// Completes a task: writes `bytes` to its destination and publishes the completion.
    pub(crate) fn complete(&self, id: DownloadId, bytes: &[u8]) {
        let destination = {
            let state = self.state.borrow();
            let Some(task) = state.tasks.get(&id) else {
                return;
            };
            task.destination.clone()
        };
        std::fs::write(&destination, bytes).expect("the payload file is writable");
        self.publish(
            id,
            DownloadStatus::Completed {
                downloaded: bytes.len() as u64,
                total: bytes.len() as u64,
            },
        );
    }

    /// Reports a failed transfer.
    pub(crate) fn fail(&self, id: DownloadId, failure: download_provider::error::DownloadFailure) {
        self.publish(id, DownloadStatus::Failed { failure });
    }

    fn event(&self, event: DownloadEvent) {
        let sink = self.sink.borrow().clone();
        if let Some(sink) = sink {
            sink(event);
        }
    }
}

impl DownloadService for FakeDownloads {
    fn add(&self, request: DownloadRequest) -> DownloadId {
        let id = DownloadId::new();
        let task = DownloadTask {
            id,
            url: request.url.clone(),
            destination: request.destination.clone(),
            status: DownloadStatus::Downloading {
                downloaded: 0,
                total: None,
                mb_per_second: 0.0,
            },
        };
        {
            let mut state = self.state.borrow_mut();
            state.requests.push(request);
            state.tasks.insert(id, task.clone());
            state.last = Some(id);
        }
        self.event(DownloadEvent::TaskUpdated(task));
        id
    }

    fn pause(&self, id: DownloadId) -> bool {
        let status = {
            let state = self.state.borrow();
            match state.tasks.get(&id).map(|task| &task.status) {
                Some(DownloadStatus::Downloading {
                    downloaded, total, ..
                }) => Some(DownloadStatus::Pausing {
                    downloaded: *downloaded,
                    total: *total,
                }),
                _ => None,
            }
        };
        match status {
            Some(status) => {
                self.publish(id, status);
                true
            }
            None => false,
        }
    }

    fn resume(&self, id: DownloadId) -> bool {
        let resumable = {
            let state = self.state.borrow();
            matches!(
                state.tasks.get(&id).map(|task| &task.status),
                Some(DownloadStatus::Paused { .. } | DownloadStatus::Failed { .. })
            )
        };
        if !resumable {
            return false;
        }
        self.publish(
            id,
            DownloadStatus::Downloading {
                downloaded: 0,
                total: None,
                mb_per_second: 0.0,
            },
        );
        true
    }

    fn delete(&self, id: DownloadId) -> bool {
        let removed = {
            let mut state = self.state.borrow_mut();
            state.deleted.push(id);
            let task = state.tasks.remove(&id);
            if let Some(task) = &task {
                let _ = std::fs::remove_file(&task.destination);
            }
            task.is_some()
        };
        if removed {
            self.event(DownloadEvent::TaskRemoved(id));
        }
        removed
    }

    fn task(&self, id: DownloadId) -> Option<DownloadTask> {
        self.state.borrow().tasks.get(&id).cloned()
    }

    fn subscribe(&self, sink: Arc<dyn Fn(DownloadEvent) + Send + Sync>) -> DownloadSubscription {
        *self.sink.borrow_mut() = Some(sink);
        let slot = Rc::clone(&self.sink);
        DownloadSubscription::new(move || {
            *slot.borrow_mut() = None;
        })
    }
}
