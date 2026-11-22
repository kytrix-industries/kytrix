//! The headless host the shell's tests open the application in.
//!
//! A test needs the same application the binary builds: the real composition root (in-memory
//! settings, the embedded catalogue, the market participants connector), the real shell over
//! a real engine window, and the same frame route. [`Harness`] assembles exactly that on
//! [`TestApp`] — the engine's headless window — so a test can press a control by its debug
//! id, run the frames a real pump would run, and assert what the window shows.
//!
//! The harness is `cfg(test)` only: it is not part of the shipped binary.

use std::{cell::RefCell, rc::Rc, time::Instant};

use app_config::settings::{Config, Language};
use app_config_provider::store::ConfigStore;
use i18n::{language::codes::LanguageCode, translate::Translate};
use smol_str::SmolStr;
use ui_core::{
    event::{InputEvent, Modifiers, PointerButton},
    math::{Point, Rect, Size},
    node::id::NodeId,
    test_support::app::TestApp,
    window::{WindowId, WindowOptions},
};

use crate::{composition::Composition, shell::root::Shell};

/// The translator over the product catalogue.
pub(crate) fn translate(language: LanguageCode) -> Translate {
    Translate::new(
        Rc::new(i18n::global::embedded::load().expect("the embedded catalogue loads")),
        language,
    )
}

/// The settings a fresh installation starts with, in English.
pub(crate) fn config() -> Config {
    let mut config = Config::default();
    config.language = Language::En;
    config
}

/// The application, its composition root and its shell, over one headless window.
pub(crate) struct Harness {
    /// The engine test host (window, frame pump, probe).
    pub app: TestApp,
    /// The application's one composition root.
    pub composition: Rc<Composition>,
    /// The shell, as the frame route holds it.
    pub shell: Rc<RefCell<Shell>>,
    /// The window the shell draws.
    pub window: WindowId,
}

impl Harness {
    /// Opens the application over a fresh installation.
    pub(crate) fn new() -> Self {
        Self::with_config(config())
    }

    /// Opens the application over `config`.
    pub(crate) fn with_config(config: Config) -> Self {
        let mut app = TestApp::with_options(WindowOptions {
            title: SmolStr::new_static("Kytrix test"),
            size: Size::new(1280.0, 960.0),
            ..WindowOptions::default()
        });
        let window = app.window_id();
        let runtime = app.app().runtime_handle().clone();
        let composition = Composition::with_store_for_test(
            app.app_mut(),
            runtime,
            ConfigStore::in_memory(config),
            None,
        )
        .expect("the composition root builds headlessly");
        let shell = Shell::open_in(app.app_mut(), Rc::clone(&composition), window)
            .expect("the shell opens in the test window");
        let shell = Shell::install(app.app_mut(), shell).expect("the frame route installs");
        let mut harness = Self {
            app,
            composition,
            shell,
            window,
        };
        // Two rounds: the first measures the window and lays the shell out, the second
        // applies the geometry that measurement produced.
        harness.tick();
        harness.tick();

        harness
    }

    /// Runs one production pump: the frame route, then the frame.
    pub(crate) fn tick(&mut self) {
        self.composition
            .frame_router
            .tick(self.app.app_mut())
            .expect("the shell's route runs");
        let _ = self.app.drain();
    }

    /// The window's node tree.
    pub(crate) fn tree(&self) -> &ui_core::node::tree::NodeTree {
        self.app
            .app()
            .window(self.window)
            .expect("the window is alive")
            .tree()
    }

    /// Every node of the window, depth first from the root.
    fn nodes(&self) -> Vec<NodeId> {
        let tree = self.tree();
        let mut nodes = Vec::new();
        let mut stack = vec![tree.root()];
        while let Some(node) = stack.pop() {
            nodes.push(node);
            if let Ok(entry) = tree.get(node) {
                stack.extend(entry.children().iter().rev().copied());
            }
        }
        nodes
    }

    /// The node with this debug id.
    pub(crate) fn node(&self, debug_id: &str) -> Option<NodeId> {
        let tree = self.tree();
        self.nodes()
            .into_iter()
            .find(|node| tree.debug_id(*node) == Some(debug_id))
    }

    /// Whether the window draws a node with this debug id.
    pub(crate) fn exists(&self, debug_id: &str) -> bool {
        self.node(debug_id).is_some()
    }

    /// The layout rectangle of the node with this debug id.
    pub(crate) fn bounds(&self, debug_id: &str) -> Option<Rect> {
        let node = self.node(debug_id)?;
        self.tree().get(node).ok().map(|node| node.layout().bounds)
    }

    /// Every layout rectangle the window gives a node with this debug id, in tree order.
    pub(crate) fn bounds_all(&self, debug_id: &str) -> Vec<Rect> {
        let tree = self.tree();
        self.nodes()
            .into_iter()
            .filter(|node| tree.debug_id(*node) == Some(debug_id))
            .filter_map(|node| tree.get(node).ok().map(|node| node.layout().bounds))
            .collect()
    }

    /// The first debug id in tree order that contains `needle`.
    ///
    /// A few components derive their debug ids from a per-instance prefix (the gallery
    /// page does), so a test names the part it knows rather than the whole id.
    pub(crate) fn debug_id_containing(&self, needle: &str) -> Option<SmolStr> {
        let tree = self.tree();
        self.nodes().into_iter().find_map(|node| {
            tree.debug_id(node)
                .filter(|id| id.contains(needle))
                .map(SmolStr::from)
        })
    }

    /// Every exact text the window draws, in tree order.
    pub(crate) fn texts(&self) -> Vec<SmolStr> {
        let tree = self.tree();
        self.nodes()
            .into_iter()
            .filter_map(|node| match tree.get(node).ok().map(|node| node.kind()) {
                Some(ui_core::node::tree::NodeKind::Text(content)) => Some(content.text.clone()),
                _ => None,
            })
            .collect()
    }

    /// Every debug id the window draws, in tree order.
    pub(crate) fn debug_ids(&self) -> Vec<SmolStr> {
        let tree = self.tree();
        self.nodes()
            .into_iter()
            .filter_map(|node| tree.debug_id(node).map(SmolStr::from))
            .collect()
    }

    /// How many nodes the window draws under this debug id.
    pub(crate) fn count(&self, debug_id: &str) -> usize {
        let tree = self.tree();
        self.nodes()
            .into_iter()
            .filter(|node| tree.debug_id(*node) == Some(debug_id))
            .count()
    }

    /// Presses and releases the primary button on the node with this debug id.
    ///
    /// A shell control is a wrapper (for its debug id) around the kit component that owns
    /// the action, so the press goes to the first interactive descendant of the wrapper:
    /// that is the component's own square, not the whole row it happens to fill.
    pub(crate) fn click(&mut self, debug_id: &str) {
        let wrapper = self
            .node(debug_id)
            .unwrap_or_else(|| panic!("the window should draw {debug_id}"));
        let target = self.interactive_descendant(wrapper);
        let bounds = self
            .tree()
            .get(target)
            .map(|node| node.layout().bounds)
            .unwrap_or_else(|_| panic!("{debug_id} should have a box"));
        let center = Point::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        let _ = self.app.inject(InputEvent::PointerDown {
            position: center,
            button: PointerButton::Primary,
            modifiers: Modifiers::empty(),
            timestamp: Instant::now(),
        });
        let _ = self.app.inject(InputEvent::PointerUp {
            position: center,
            button: PointerButton::Primary,
            modifiers: Modifiers::empty(),
            timestamp: Instant::now(),
        });
        self.tick();
    }

    /// The first node at or below `node` that takes pointer input.
    fn interactive_descendant(&self, node: NodeId) -> NodeId {
        let tree = self.tree();
        let mut stack = vec![node];
        while let Some(candidate) = stack.pop() {
            if tree
                .get(candidate)
                .is_ok_and(|entry| entry.style_patch().interactive == Some(true))
            {
                return candidate;
            }
            if let Ok(entry) = tree.get(candidate) {
                stack.extend(entry.children().iter().rev().copied());
            }
        }
        node
    }

    /// Presses on the node with this debug id, drags it by `delta` and releases.
    ///
    /// The press, the move that crosses the drag threshold and the move that reports the
    /// full delta all go through the engine's own input dispatch (the same path a platform
    /// pump uses), so a drag target's `DragPhase::Start` / `Move` / `End` sequence is the
    /// production one. The frame that follows applies the intents the drag reported.
    pub(crate) fn drag(&mut self, debug_id: &str, delta: Point) {
        let node = self
            .node(debug_id)
            .unwrap_or_else(|| panic!("the window should draw {debug_id}"));
        let bounds = self
            .tree()
            .get(node)
            .map(|node| node.layout().bounds)
            .unwrap_or_else(|_| panic!("{debug_id} should have a box"));
        assert!(
            bounds.size.width > 0.0 && bounds.size.height > 0.0,
            "{debug_id} should have a hit area: {bounds:?}"
        );
        let start = Point::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        let move_to = |offset: Point| Point::new(start.x + offset.x, start.y + offset.y);
        let _ = self.app.inject(InputEvent::PointerDown {
            position: start,
            button: PointerButton::Primary,
            modifiers: Modifiers::empty(),
            timestamp: Instant::now(),
        });
        // The first move only has to cross the drag threshold; the moves after it report
        // the deltas the drag travels through. More than one matters: a drag that persisted
        // on every reported move would write several times, which a single move could not
        // tell apart from one write at the end.
        let _ = self
            .app
            .move_pointer(move_to(Point::new(delta.x * 0.5, delta.y * 0.5)));
        let _ = self
            .app
            .move_pointer(move_to(Point::new(delta.x * 0.75, delta.y * 0.75)));
        let _ = self.app.move_pointer(move_to(delta));
        let _ = self.app.inject(InputEvent::PointerUp {
            position: move_to(delta),
            button: PointerButton::Primary,
            modifiers: Modifiers::empty(),
            timestamp: Instant::now(),
        });
        self.tick();
    }

    /// The frame cursor the window has published.
    pub(crate) fn frame_index(&self) -> u32 {
        self.app
            .app()
            .window(self.window)
            .map_or(0, |window| window.frame_index().get())
    }

    /// The number of nodes in the window.
    pub(crate) fn node_count(&self) -> usize {
        self.tree().len()
    }

    /// Closes the application window the way the close button does.
    pub(crate) fn shutdown(&mut self) {
        self.shell.borrow_mut().close_for_test(self.app.app_mut());
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use app_config::{
        network::{RouteTarget, ServiceTarget},
        settings::{AccountConfig, AccountWalletKind},
        ui::SidebarMode,
        workspace::{WorkspaceConfig, WorkspaceLayout, WorkspacesConfig},
    };
    use download_provider::{request::DownloadRequest, task::DownloadId};
    use network::transport::http::HttpClientClass;

    use super::*;
    use crate::shell::metrics;

    fn two_workspaces() -> Config {
        let mut config = config();
        config.workspaces = WorkspacesConfig {
            entries: vec![
                WorkspaceConfig {
                    name: SmolStr::new_static("one"),
                    layout: WorkspaceLayout::single_pane(),
                },
                WorkspaceConfig {
                    name: SmolStr::new_static("two"),
                    layout: WorkspaceLayout::single_pane(),
                },
            ],
            active: Some(SmolStr::new_static("one")),
        };
        config
    }

    /// The harness opens the real shell: the title bar, the sidebar, the workspace and a
    /// mounted widget are all drawn.
    #[test]
    fn the_harness_should_open_the_real_shell() {
        let harness = Harness::new();
        for selector in [
            "TITLE_BAR",
            "SIDEBAR",
            "ROOT_PAGE",
            "WORKSPACE_STAGE",
            "PANE_BODY",
            "WIDGET_SURFACE",
        ] {
            assert!(
                harness.exists(selector),
                "the shell should draw {selector}; ids: {:?}",
                harness.debug_ids()
            );
        }
        assert!(harness.node_count() > 0);
    }

    /// The title bar is frameless chrome on Windows and publishes a drag strip between its
    /// controls (E6): the window can be moved by the bar without the system title bar.
    #[test]
    fn the_title_bar_should_publish_a_drag_region_between_its_controls() {
        let harness = Harness::new();
        let regions = harness.shell.borrow().title_bar().drag_regions().to_vec();
        assert_eq!(
            regions.len(),
            1,
            "one drag strip between the two control clusters"
        );
        let region = regions[0];
        assert!(region.size.width > 0.0 && region.size.height > 0.0);
        assert!(
            region.origin.x > 0.0,
            "the region leaves the left control cluster out"
        );
        if cfg!(target_os = "windows") {
            assert!(metrics::draws_system_buttons());
        }
    }

    /// The title bar's toggle walks the sidebar through its three states and persists each
    /// one: the next start reads the state back.
    #[test]
    fn the_toggle_should_walk_the_sidebar_and_persist_the_state() {
        let mut harness = Harness::new();
        assert_eq!(
            harness.composition.hub.read(|config| config.ui.sidebar),
            SidebarMode::Full
        );
        assert_eq!(
            harness.bounds("SIDEBAR").map(|bounds| bounds.size.width),
            Some(220.0)
        );

        harness.click("TITLE_BAR_SIDEBAR_TOGGLE");
        assert_eq!(
            harness.composition.hub.read(|config| config.ui.sidebar),
            SidebarMode::Hidden
        );
        assert!(!harness.exists("SIDEBAR_NEW_WORKSPACE"));

        harness.click("TITLE_BAR_SIDEBAR_TOGGLE");
        assert_eq!(
            harness.composition.hub.read(|config| config.ui.sidebar),
            SidebarMode::IconOnly
        );
        assert_eq!(
            harness.bounds("SIDEBAR").map(|bounds| bounds.size.width),
            Some(40.0)
        );
        assert!(!harness.exists("SIDEBAR_NEW_WORKSPACE_LABEL"));
    }

    /// The settings entry shows the settings page and hides the workspace; a workspace row
    /// gives the page area back.
    #[test]
    fn the_settings_entry_should_swap_the_page_area() {
        let mut harness = Harness::new();
        harness.click("SIDEBAR_FOOTER_SETTINGS");
        assert!(harness.exists("SETTINGS_PAGE"));
        assert!(matches!(
            harness.shell.borrow().pages().page(),
            crate::shell::pages::Page::Settings
        ));
        assert!(
            !stage_visible(&harness),
            "the workspace is hidden while the settings show"
        );

        harness.click("SIDEBAR_ITEM_Workspace 1");
        assert!(!harness.exists("SETTINGS_PAGE"));
        assert!(matches!(
            harness.shell.borrow().pages().page(),
            crate::shell::pages::Page::Workspace
        ));
        assert!(stage_visible(&harness));
    }

    /// A language write republishes the translator's language and rebuilds the shell's
    /// baked-in labels.
    #[test]
    fn a_language_write_should_rebuild_the_labels() {
        let mut harness = Harness::new();
        assert!(
            harness
                .texts()
                .iter()
                .any(|text| text.as_str() == "Settings")
        );

        harness
            .composition
            .hub
            .edit(harness.app.app_mut(), |config| {
                config.language = Language::ZhCn;
            })
            .expect("the in-memory store stores the language");
        harness.tick();

        assert!(
            harness.texts().iter().any(|text| text.as_str() == "设置"),
            "the settings label follows the language: {:?}",
            harness.texts()
        );
    }

    /// A workspace row switches the stage to that workspace's stored layout and persists
    /// which one is active.
    #[test]
    fn a_workspace_row_should_switch_the_stage() {
        let mut harness = Harness::with_config(two_workspaces());
        assert_eq!(
            harness.shell.borrow().workspaces().active_name().as_str(),
            "one"
        );

        harness.click("SIDEBAR_ITEM_two");

        assert_eq!(
            harness.shell.borrow().workspaces().active_name().as_str(),
            "two"
        );
        assert_eq!(
            harness
                .composition
                .hub
                .read(|config| config.workspaces.active.clone()),
            Some(SmolStr::new_static("two"))
        );
        assert!(harness.exists("WORKSPACE_STAGE"));
    }

    /// A widget is mounted in the pane's body and the pane header offers its controls.
    #[test]
    fn a_pane_should_mount_a_widget_and_its_header_controls() {
        let harness = Harness::new();
        for selector in ["PANE_BODY", "PANE_HEADER", "PANE_WIDGET_BUTTON"] {
            assert!(harness.exists(selector), "the pane should draw {selector}");
        }
        assert!(
            harness.exists("WIDGET_SURFACE"),
            "the default widget is mounted"
        );
    }

    /// Splitting adds a pane and its header; closing it removes it; the last pane stays.
    #[test]
    fn splitting_and_closing_should_change_the_pane_set() {
        let mut harness = Harness::new();
        assert_eq!(harness.shell.borrow().pages().stage().pane_count(), 1);

        harness.click("PANE_SPLIT_RIGHT");
        assert_eq!(harness.shell.borrow().pages().stage().pane_count(), 2);
        assert!(harness.exists("PANE_CLOSE_BUTTON"));
        assert_eq!(
            harness.count("WIDGET_SURFACE"),
            2,
            "the split mounts a second widget"
        );

        harness.click("PANE_CLOSE_BUTTON");
        assert_eq!(harness.shell.borrow().pages().stage().pane_count(), 1);
        assert!(!harness.exists("PANE_CLOSE_BUTTON"));
        assert_eq!(
            harness.count("WIDGET_SURFACE"),
            1,
            "the closed pane's widget is released, not left mounted"
        );
    }

    /// The gallery page opens over the workspace and leaves when dismissed.
    #[test]
    fn the_gallery_should_open_over_the_workspace() {
        let mut harness = Harness::new();
        assert!(!harness.shell.borrow().pages().stage().gallery_open());

        harness.click("PANE_WIDGET_BUTTON");
        assert!(harness.shell.borrow().pages().stage().gallery_open());
        assert!(harness.exists("WORKSPACE_PAGES"));
    }

    /// Closing the window releases the shell: its route is gone and its window is gone.
    #[test]
    fn closing_the_window_should_release_the_shell() {
        let mut harness = Harness::new();
        assert!(!harness.composition.frame_router.is_empty());

        harness.shutdown();

        assert_eq!(harness.composition.frame_router.len(), 0);
        assert!(harness.app.app().window(harness.window).is_none());
    }

    /// Whether the stage is visible in the harness's window (its root carries the visibility
    /// patch through a signal; the node's own patch is the observable).
    fn stage_visible(harness: &Harness) -> bool {
        let Some(node) = harness.node("WORKSPACE_STAGE") else {
            return false;
        };
        let tree = harness.tree();
        tree.get(node)
            .ok()
            .and_then(|node| node.style_patch().visible)
            .unwrap_or(true)
    }

    /// The harness reads the frame cursor it publishes.
    #[test]
    fn the_harness_should_report_the_frame_cursor() {
        let harness = Harness::new();
        let _ = harness.frame_index();
    }

    /// The title bar carries the account control, and the aggregate carries the service
    /// connection the control reads: the composition root wired both, not just one.
    #[test]
    fn the_title_bar_should_mount_the_account_control() {
        let harness = Harness::new();
        assert!(
            harness.composition.services().kytrix_service().is_some(),
            "the aggregate hands the service connection to widgets and the account control"
        );
        assert!(
            harness.shell.borrow().title_bar().profile().is_some(),
            "the title bar built the account control"
        );
        assert!(
            harness.exists("TEST_PROFILE_TRIGGER_AVATAR"),
            "the account trigger is drawn; ids: {:?}",
            harness.debug_ids()
        );
    }

    /// A settings write on the account record reaches the snapshot the account control
    /// reads: the aggregate's config signal and the commit hub's are one signal, so a
    /// sign-in redraws the trigger on the next frame instead of writing a snapshot nothing
    /// follows.
    #[test]
    fn an_account_write_should_reach_the_account_control_snapshot() {
        let mut harness = Harness::new();
        let host = harness.composition.profile_host();
        let account = account();
        harness
            .composition
            .hub
            .edit(harness.app.app_mut(), |config| {
                config.account = Some(account.clone());
            })
            .expect("the in-memory store stores the account");

        assert_eq!(
            host.snapshot_signal(harness.app.app()).account,
            Some(account),
            "the account control reads the published snapshot"
        );
    }

    /// The title bar installs the updater and mounts its indicator; closing the window
    /// releases both (indicator first, because it reads the updater's signal).
    #[test]
    fn the_title_bar_should_install_and_release_the_updater() {
        let mut harness = Harness::new();
        let (indicator, updater) = {
            let shell = harness.shell.borrow();
            let title_bar = shell.title_bar();
            assert!(title_bar.updater().is_some(), "the updater is installed");
            assert!(title_bar.indicator().is_some(), "the indicator is mounted");
            (
                title_bar.indicator().expect("mounted").clone(),
                title_bar.updater().expect("installed").clone(),
            )
        };
        assert!(!indicator.is_released());
        assert!(!updater.is_released());

        harness.shutdown();

        assert!(
            indicator.is_released(),
            "the indicator is released with the window"
        );
        assert!(
            updater.is_released(),
            "the updater is released with the window"
        );
        assert_eq!(
            harness.composition.frame_router.len(),
            0,
            "both of their routes are gone"
        );
    }

    /// The settings page and the updater download through one process manager: a task
    /// added through either seam is the same task, and the settings page's signal follows
    /// what the manager published on the next frame.
    #[test]
    fn the_downloads_seams_should_share_the_process_manager() {
        let mut harness = Harness::new();
        let request = || DownloadRequest {
            url: "http://127.0.0.1:9/model.bin"
                .parse()
                .expect("the request URL is a URL"),
            // No file name: the transfer would fail before it reaches the network, which
            // keeps this test about the wiring rather than about a transfer.
            destination: PathBuf::new(),
            target: RouteTarget::Service(ServiceTarget::TranslationModelDownload),
            class: HttpClientClass::ModelDownload,
            expected_size: None,
        };

        let from_updater = harness.composition.updater_deps().downloads.add(request());
        let from_settings = DownloadId::new();
        let settings_downloads = harness.composition.downloads().settings_service();
        (settings_downloads.add)(request(), from_settings);

        let provider = Rc::clone(harness.composition.downloads().provider());
        assert!(provider.task(from_updater).is_some());
        assert!(
            provider.task(from_settings).is_some(),
            "the settings page's identity is the manager's identity"
        );

        harness.tick();
        let tasks = harness.app.app().read_with(
            &harness.composition.downloads().tasks_signal(),
            Clone::clone,
        );
        assert!(
            tasks.iter().any(|task| task.id == from_updater)
                && tasks.iter().any(|task| task.id == from_settings),
            "the frame route mirrors both seams' tasks onto the settings signal"
        );
    }

    /// The account a test signs in as.
    fn account() -> AccountConfig {
        AccountConfig {
            user_id: "user-1".to_owned(),
            wallet_address: "0xabc".to_owned(),
            wallet_kind: AccountWalletKind::Ethereum,
        }
    }

    /// A language write reaches the frozen services signals and the widget mounted in the
    /// pane redraws in the new language: the P1-1 end-to-end probe.
    ///
    /// The pane's default widget draws the framework's hint, whose copy is resolved from
    /// `WidgetServices::translate` in the language the surface reads from
    /// `WidgetServices::language` — so the text changing is what proves the write reached
    /// the consumers.
    #[test]
    fn a_language_write_should_reach_the_mounted_widget() {
        let mut harness = Harness::new();
        assert!(
            harness
                .texts()
                .iter()
                .any(|text| text.contains("select a widget")),
            "the default widget starts in English: {:?}",
            harness.texts()
        );

        harness
            .composition
            .hub
            .edit(harness.app.app_mut(), |config| {
                config.language = Language::ZhCn;
            })
            .expect("the in-memory store stores the language");
        harness.tick();

        assert_eq!(
            *harness
                .app
                .app()
                .read(&harness.composition.services().language),
            LanguageCode::ZhCn,
            "the frozen language signal follows the write"
        );
        assert_eq!(
            harness
                .app
                .app()
                .read_with(&harness.composition.services().config, |config| {
                    config.language
                }),
            Language::ZhCn,
            "the frozen config snapshot follows the write"
        );
        assert!(
            harness
                .texts()
                .iter()
                .any(|text| text.contains("选择一个部件")),
            "the mounted widget redraws in the new language: {:?}",
            harness.texts()
        );
    }

    /// The settings page is built from `WidgetServices` too: a language write while it is
    /// open rebuilds its chrome (P1-1's settings-page half).
    #[test]
    fn a_language_write_should_rebuild_the_settings_page() {
        let mut harness = Harness::new();
        harness.click("SIDEBAR_FOOTER_SETTINGS");
        assert!(
            harness
                .texts()
                .iter()
                .any(|text| text.as_str() == "Appearance"),
            "the settings page starts in English: {:?}",
            harness.texts()
        );

        harness
            .composition
            .hub
            .edit(harness.app.app_mut(), |config| {
                config.language = Language::ZhCn;
            })
            .expect("the in-memory store stores the language");
        harness.tick();

        assert!(
            harness.texts().iter().any(|text| text.as_str() == "外观"),
            "the settings page follows the language signal: {:?}",
            harness.texts()
        );
    }

    /// Splitting lays both panes out: a new pane gets a real share of the stage instead of
    /// the zero rectangle its frame is built with (P1-2).
    #[test]
    fn splitting_should_lay_the_new_pane_out() {
        let mut harness = Harness::new();
        let stage = harness
            .bounds("WORKSPACE_STAGE")
            .expect("the stage is drawn");

        harness.click("PANE_SPLIT_RIGHT");

        let bodies = harness.bounds_all("PANE_BODY");
        assert_eq!(bodies.len(), 2, "the split mounts a second pane");
        for body in &bodies {
            assert!(
                body.size.width > 0.0 && body.size.height > 0.0,
                "every pane has a real rectangle: {bodies:?}"
            );
        }
        assert!(
            (bodies[0].size.width + bodies[1].size.width) <= stage.size.width,
            "the panes fit inside the stage: stage {stage:?}, panes {bodies:?}"
        );
        assert!(
            (bodies[0].size.width - bodies[1].size.width).abs() < 1.0,
            "an even split gives both panes the same width: {bodies:?}"
        );
    }

    /// Closing a pane gives the survivor the whole stage again (the topology changed, the
    /// window did not).
    #[test]
    fn closing_a_pane_should_give_the_survivor_the_whole_stage() {
        let mut harness = Harness::new();
        let stage = harness
            .bounds("WORKSPACE_STAGE")
            .expect("the stage is drawn");
        harness.click("PANE_SPLIT_RIGHT");

        harness.click("PANE_CLOSE_BUTTON");

        let bodies = harness.bounds_all("PANE_BODY");
        assert_eq!(bodies.len(), 1, "the closed pane is gone");
        let width_ratio = bodies[0].size.width / stage.size.width;
        assert!(
            width_ratio > 0.9,
            "the surviving pane fills the stage: stage {stage:?}, pane {:?}",
            bodies[0]
        );
    }

    /// Dragging the split bar moves the panes: the geometry follows the fraction (P1-2)
    /// while the stored layout is written once, when the drag ends (P2-③).
    #[test]
    fn dragging_the_split_bar_should_move_the_panes_and_persist_once() {
        let mut harness = Harness::new();
        harness.click("PANE_SPLIT_RIGHT");
        let bar = harness
            .debug_id_containing("SPLIT_BAR")
            .expect("the split draws a bar");
        let before = harness.bounds_all("PANE_BODY");
        let commits = harness.composition.hub.commits();

        harness.drag(&bar, Point::new(80.0, 0.0));

        let after = harness.bounds_all("PANE_BODY");
        assert!(
            after[0].size.width > before[0].size.width + 40.0,
            "the head pane follows the bar: before {before:?}, after {after:?}"
        );
        assert!(
            after[1].size.width > 0.0,
            "the tail pane keeps a real rectangle: {after:?}"
        );
        assert_eq!(
            harness.composition.hub.commits(),
            commits + 1,
            "a drag writes the layout once, on End, not once per pointer move"
        );
    }

    /// A rebuilt sidebar keeps its place in the window's child order, below the self-drawn
    /// title bar (P2-④): window children are painted in that order, and a panel rebuilt by
    /// appending would cover the chrome.
    #[test]
    fn a_rebuilt_sidebar_should_stay_below_the_title_bar() {
        let mut harness = Harness::new();
        let child_ids = |harness: &Harness| {
            let tree = harness.tree();
            let root = tree.root();
            tree.get(root)
                .expect("the window has a root")
                .children()
                .iter()
                .filter_map(|child| tree.debug_id(*child).map(SmolStr::from))
                .collect::<Vec<_>>()
        };
        let before = child_ids(&harness);

        // The toggle rebuilds the panel; so does a workspace-list change, but the toggle is
        // the one a test can drive without the add/rename dialogs.
        harness.click("TITLE_BAR_SIDEBAR_TOGGLE");
        harness.click("TITLE_BAR_SIDEBAR_TOGGLE");

        let after = child_ids(&harness);
        assert_eq!(
            before, after,
            "a rebuild does not append the panel to the window root"
        );
        let sidebar = after
            .iter()
            .position(|id| id.as_str() == "SIDEBAR")
            .expect("the window draws a sidebar");
        let title_bar = after
            .iter()
            .position(|id| id.as_str() == "TITLE_BAR")
            .expect("the window draws a title bar");
        assert!(
            sidebar < title_bar,
            "the sidebar is painted below the title bar: {after:?}"
        );
        assert_eq!(
            title_bar,
            after.len() - 1,
            "the title bar stays the window's last child: {after:?}"
        );
    }

    /// Confirming a choice in the gallery replaces the pane's widget: the old instance is
    /// released (its frame route and nodes go) and the new one mounts (P2-⑤).
    #[test]
    fn the_gallery_confirm_should_replace_the_pane_widget_and_release_the_old_one() {
        let mut harness = Harness::new();
        let routes = harness.composition.frame_router.len();

        harness.click("PANE_WIDGET_BUTTON");
        assert!(harness.shell.borrow().pages().stage().gallery_open());
        let row = harness
            .debug_id_containing(".row.kytrix.")
            .expect("the gallery lists a widget");
        let chosen = row
            .as_str()
            .rsplit_once(".row.")
            .expect("a row's id ends in the widget id")
            .1
            .to_owned();
        harness.click(&row);

        let confirm = harness
            .debug_id_containing(".confirm")
            .expect("the gallery footer draws its confirm control");
        harness.click(&confirm);

        assert!(
            !harness.shell.borrow().pages().stage().gallery_open(),
            "the page leaves once it reported"
        );
        assert_eq!(
            harness.composition.frame_router.len(),
            routes,
            "the old widget's route is released as the new widget's route registers"
        );
        let layout = harness.shell.borrow().pages().stage().snapshot();
        let WorkspaceLayout::Pane { widget } = layout else {
            panic!("a fresh workspace is one pane");
        };
        assert_eq!(
            widget.id.as_str(),
            chosen,
            "the pane shows the widget the gallery confirmed"
        );
        assert_eq!(harness.count("WIDGET_SURFACE"), 1, "one widget, not two");
    }

    /// Cancelling the gallery leaves the page, releases it and changes no pane (P2-⑤).
    #[test]
    fn the_gallery_cancel_should_release_the_page_and_keep_the_pane_widget() {
        let mut harness = Harness::new();
        let routes = harness.composition.frame_router.len();
        let before = harness.shell.borrow().pages().stage().snapshot();

        harness.click("PANE_WIDGET_BUTTON");
        let cancel = harness
            .debug_id_containing(".cancel")
            .expect("the gallery footer draws its cancel control");
        harness.click(&cancel);

        assert!(!harness.shell.borrow().pages().stage().gallery_open());
        assert_eq!(harness.composition.frame_router.len(), routes);
        assert_eq!(
            harness.shell.borrow().pages().stage().snapshot(),
            before,
            "a cancelled page chooses nothing"
        );
        assert_eq!(harness.count("WIDGET_SURFACE"), 1);
    }
}
