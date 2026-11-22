//! The widget registry: descriptors, mount factories, and lookup by persisted ID.
//!
//! Every widget is compiled into the application, so the complete catalogue is known at
//! build time and nothing registers widgets at runtime. This crate owns the one list of
//! them: the descriptor a selection surface lists and the erased mount a pane calls.
//!
//! # What a registration is, and what it is not
//!
//! A registration is *identity plus a monomorphized mount*. It is not a widget
//! instance: [`WidgetView`] is not object safe (its `Config` is an associated type and
//! its state belongs to its own type), so the registry keeps a function pointer built
//! once per registered type rather than a trait object.
//!
//! The registry no longer builds the state cell, the presentation shell, the recovery
//! control, or the header. Those are the framework's and they belong to one mount, in
//! `AnyWidget::mount`'s documented order: scope → `WidgetStatus` (an engine `Signal`,
//! not a toolkit entity) → surface shell → body cell and `ReloadHandle` → `W::new` →
//! `load` → frame route. The pane supplies what only it knows — the window, the parent
//! node, and the composition root's `Rc<WidgetServices>` — and [`WidgetRegistration`]
//! supplies the part that is the same for all 24 entries: decoding a stored config
//! into the widget's own type and erasing the result.
//!
//! # No process global
//!
//! The catalogue used to be installed as a toolkit global that panes looked up by
//! type. The engine has no global registry and no global lookup: the composition root
//! (the widget gallery, the application shell) builds [`crate::catalogue::registry`]
//! once and passes the `WidgetRegistry` to whatever mounts a pane, exactly as it passes
//! `Rc<WidgetServices>` — see `crates/widgets/AGENTS.md` §8.1.

use std::{
    collections::{BTreeMap, btree_map::Entry},
    fmt,
    rc::Rc,
};

use app_config::widget::{VersionedWidgetConfig, WidgetConfig, decode_or_default};
use smol_str::SmolStr;
use thiserror::Error;
use ui_core::{app::App, error::EngineError, node::id::NodeId, window::WindowId};
use widget::{
    any::AnyWidget,
    config::WidgetConfigChange,
    descriptor::WidgetDescriptor,
    identity::{WidgetId, WidgetInstanceId},
    services::WidgetServices,
    view::WidgetView,
};

/// The erased mount one registration performs.
///
/// [`WidgetView`] is not object safe, so the registry keeps a function pointer
/// monomorphized for the registered type instead of a trait object: the stored config
/// is decoded into that type inside the function, and what comes back is the erased
/// [`AnyWidget`] the pane hosts.
type MountFn = fn(
    &mut App,
    WindowId,
    NodeId,
    &Rc<WidgetServices>,
    WidgetInstanceId,
    Option<VersionedWidgetConfig>,
    WidgetConfigChange,
) -> Result<AnyWidget, EngineError>;

/// A descriptor paired with its statically linked mount.
#[derive(Clone)]
pub struct WidgetRegistration {
    descriptor: WidgetDescriptor,
    mount: MountFn,
}

impl WidgetRegistration {
    /// Registers one statically linked view type.
    ///
    /// The descriptor and the mounted widget's own descriptor come from the same
    /// declarations (`ID`, `DISPLAY_NAME`, `CATEGORY`, `DESCRIPTION`, `ICON`,
    /// `SELECTABLE`, `Config::SCHEMA_VERSION`), so a pane's widget and the registry's
    /// entry for it cannot disagree about any of them.
    pub fn for_view<W: WidgetView>() -> Self {
        Self {
            descriptor: descriptor_of::<W>(),
            mount: mount_of::<W>,
        }
    }

    /// Returns this registration's immutable descriptor.
    pub const fn descriptor(&self) -> &WidgetDescriptor {
        &self.descriptor
    }

    /// Mounts one instance of this widget under `parent`.
    ///
    /// `config` is the envelope the pane stored for this instance, or `None` when the
    /// pane has none; `config_change` is the pane's own report, so a widget mounted
    /// here writes its config back to the pane hosting it.
    ///
    /// Nothing here can fail on the config: a stored envelope this build cannot read —
    /// another schema version, a payload that no longer decodes — becomes the config
    /// type's default rather than an error, because a pane the application cannot
    /// rebuild is worse than a pane that lost its settings. The mount itself can fail
    /// on the engine (`EngineError`): the window or the parent node it names is gone.
    pub fn mount(
        &self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        services: &Rc<WidgetServices>,
        config: Option<VersionedWidgetConfig>,
        config_change: WidgetConfigChange,
    ) -> Result<AnyWidget, EngineError> {
        (self.mount)(
            app,
            window,
            parent,
            services,
            WidgetInstanceId::new(),
            config,
            config_change,
        )
    }
}

impl fmt::Debug for WidgetRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WidgetRegistration")
            .field("descriptor", &self.descriptor)
            .finish_non_exhaustive()
    }
}

/// Decodes one stored config into the widget's own type and mounts it.
///
/// The registry's whole share of a mount: the widget is constructed from the config its
/// type declares and the framework does the rest (see the module documentation).
fn mount_of<W: WidgetView>(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    services: &Rc<WidgetServices>,
    instance_id: WidgetInstanceId,
    config: Option<VersionedWidgetConfig>,
    config_change: WidgetConfigChange,
) -> Result<AnyWidget, EngineError> {
    AnyWidget::mount::<W>(
        app,
        window,
        parent,
        services,
        instance_id,
        decode_or_default::<W::Config>(config),
        config_change,
    )
}

/// The descriptor one registered type declares.
///
/// One construction serves both the registry's entry and the mounted widget, so the two
/// can never disagree about the identity, the description, the icon, or whether the
/// widget may be offered by a selection surface.
fn descriptor_of<W: WidgetView>() -> WidgetDescriptor {
    let mut descriptor = WidgetDescriptor::new(
        W::ID,
        W::DISPLAY_NAME,
        W::CATEGORY,
        W::Config::SCHEMA_VERSION,
    )
    .with_selectable(W::SELECTABLE);
    if let Some(description) = W::DESCRIPTION {
        descriptor = descriptor.with_description(description);
    }
    if let Some(icon) = W::ICON {
        descriptor = descriptor.with_icon(icon);
    }

    descriptor
}

/// Registry for statically linked widget factories.
///
/// A `BTreeMap` gives deterministic ID ordering and logarithmic lookup without
/// maintaining a second index for gallery presentation.
#[derive(Clone, Debug, Default)]
pub struct WidgetRegistry {
    registrations: BTreeMap<WidgetId, WidgetRegistration>,
}

impl WidgetRegistry {
    /// Registers a statically linked widget and rejects duplicate stable IDs.
    pub fn register(
        &mut self,
        registration: WidgetRegistration,
    ) -> Result<(), WidgetRegistryError> {
        let id = registration.descriptor.id().clone();
        match self.registrations.entry(id.clone()) {
            Entry::Vacant(entry) => {
                entry.insert(registration);
                Ok(())
            }
            Entry::Occupied(_) => Err(WidgetRegistryError::DuplicateId(id)),
        }
    }

    /// Looks up a registration by a persisted or static ID.
    pub fn get(&self, id: &str) -> Option<&WidgetRegistration> {
        self.registrations.get(id)
    }

    /// Mounts a widget by ID under `parent`, with the config its pane stored.
    ///
    /// This is what a pane does to fill itself: the ID it restored from the layout is
    /// looked up, and the registration mounts one instance with the pane's own config
    /// report. A pane whose stored ID this build does not know gets
    /// [`WidgetMountError::NotRegistered`] and shows the catalogue's fallback widget
    /// instead (`crates/widgets/AGENTS.md` §1, `app_config::workspace::DEFAULT_WIDGET_ID`).
    pub fn mount(
        &self,
        id: &str,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        services: &Rc<WidgetServices>,
        config: Option<VersionedWidgetConfig>,
        config_change: WidgetConfigChange,
    ) -> Result<AnyWidget, WidgetMountError> {
        let registration = self
            .get(id)
            .ok_or_else(|| WidgetMountError::NotRegistered(SmolStr::new(id)))?;

        Ok(registration.mount(app, window, parent, services, config, config_change)?)
    }

    /// Mounts a widget by ID with its config type's default value.
    pub fn mount_default(
        &self,
        id: &str,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        services: &Rc<WidgetServices>,
        config_change: WidgetConfigChange,
    ) -> Result<AnyWidget, WidgetMountError> {
        self.mount(id, app, window, parent, services, None, config_change)
    }

    /// Iterates registrations in stable widget ID order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &WidgetRegistration> {
        self.registrations.values()
    }
}

/// A widget registration failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum WidgetRegistryError {
    /// A second registration attempted to claim an existing stable ID.
    #[error("widget id `{0}` is already registered")]
    DuplicateId(WidgetId),
}

/// A widget lookup or mount failure.
#[derive(Debug, Error)]
pub enum WidgetMountError {
    /// No registration exists for the ID a pane or a selection surface asked for.
    #[error("widget id `{0}` is not registered")]
    NotRegistered(SmolStr),
    /// The engine refused the mount: the window or the parent node it names is gone.
    #[error(transparent)]
    Mount(#[from] EngineError),
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc, sync::Arc};

    use app_config::widget::{VersionedWidgetConfig, WidgetConfig, WidgetConfigSchemaVersion};
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};
    use smol_str::SmolStr;
    use ui_core::{error::EngineError, node::builder::NodeBuilder, text::TextStyle};
    use widget::{
        category::WidgetCategory,
        config::WidgetConfigChange,
        cx::WidgetCx,
        identity::{WidgetId, WidgetInstanceId},
        instrument_link::InstrumentLink,
        status::{Notice, WidgetAction, WidgetState, WidgetStatus},
        test_support::{FixtureFailure, WidgetTestApp},
        view::{HeaderGroup, ReloadHandle, WidgetView},
    };

    use super::*;

    const ALPHA_ID: WidgetId = WidgetId::new_static("kytrix.test.alpha");
    const BETA_ID: WidgetId = WidgetId::new_static("kytrix.test.beta");

    /// The notice the fixture widgets report while they have nothing to show.
    ///
    /// It is a key of the test host's fixture catalogue (`test.waiting`), which is the
    /// one `WidgetTestApp::new` renders.
    const TEST_WAITING: &str = "test.waiting";

    thread_local! {
        /// How many times the fixture's `load` ran on this test's thread.
        ///
        /// The framework calls `load` with no handle the registry could keep, so the
        /// fixture counts its own calls. Each test runs on its own thread, so the count
        /// does not leak from one test to the next.
        static LOADS: Cell<usize> = const { Cell::new(0) };
    }

    /// How many times a fixture widget was loaded on this test's thread.
    fn loads() -> usize {
        LOADS.with(Cell::get)
    }

    #[derive(Clone, Default, Serialize, Deserialize)]
    struct TestConfig {
        seed: u64,
    }

    impl WidgetConfig for TestConfig {
        const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
    }
    /// A widget that persists one option and reports a waiting notice while it loads.
    ///
    /// Its own state cell is the one the framework minted; a widget keeps the handle it
    /// was given rather than a copy of the state, which is how what it is doing reaches
    /// the pane showing it.
    struct TestView {
        status: WidgetStatus,
        config: TestConfig,
    }

    impl WidgetView for TestView {
        type Config = TestConfig;

        const ID: WidgetId = WidgetId::new_static("kytrix.test.typed");
        const DISPLAY_NAME: &'static str = "widget.names.candlestick";
        const CATEGORY: WidgetCategory = WidgetCategory::Market;

        fn new(
            _instance_id: WidgetInstanceId,
            config: Self::Config,
            status: WidgetStatus,
            _reload: ReloadHandle,
            _config_change: WidgetConfigChange,
            _recovery: WidgetAction,
            _cx: &mut WidgetCx<'_>,
        ) -> Result<Self, EngineError> {
            Ok(Self { status, config })
        }

        fn load(&mut self, cx: &mut WidgetCx<'_>) {
            LOADS.with(|loads| loads.set(loads.get() + 1));
            self.status.report(
                cx.app_mut(),
                WidgetState::waiting(Notice::message(TEST_WAITING)),
            );
        }

        fn has_content(&self) -> bool {
            false
        }

        fn config(&self) -> Option<&Self::Config> {
            Some(&self.config)
        }

        fn body(&mut self, _parent: NodeId, _cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
            Ok(())
        }
    }

    /// A widget that changes its persisted option while it loads and reports the change.
    ///
    /// The envelope a pane stores is built from the config the widget holds **now**, and
    /// the report is what gets that change written out.
    struct MutatingView {
        config: TestConfig,
        config_change: WidgetConfigChange,
    }

    impl WidgetView for MutatingView {
        type Config = TestConfig;

        const ID: WidgetId = WidgetId::new_static("kytrix.test.mutating");
        const DISPLAY_NAME: &'static str = "widget.names.news";
        const CATEGORY: WidgetCategory = WidgetCategory::Monitoring;

        fn new(
            _instance_id: WidgetInstanceId,
            config: Self::Config,
            _status: WidgetStatus,
            _reload: ReloadHandle,
            config_change: WidgetConfigChange,
            _recovery: WidgetAction,
            _cx: &mut WidgetCx<'_>,
        ) -> Result<Self, EngineError> {
            Ok(Self {
                config,
                config_change,
            })
        }

        fn load(&mut self, cx: &mut WidgetCx<'_>) {
            self.config.seed = 9;
            self.config_change.publish(cx.app_mut());
        }

        fn has_content(&self) -> bool {
            false
        }

        fn config(&self) -> Option<&Self::Config> {
            Some(&self.config)
        }

        fn body(&mut self, _parent: NodeId, _cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
            Ok(())
        }
    }

    /// A view that persists nothing, so the registry stores no config for it.
    struct StatelessView;

    impl WidgetView for StatelessView {
        type Config = TestConfig;

        const ID: WidgetId = WidgetId::new_static("kytrix.test.stateless");
        const DISPLAY_NAME: &'static str = "widget.names.news";
        const CATEGORY: WidgetCategory = WidgetCategory::Monitoring;

        fn new(
            _instance_id: WidgetInstanceId,
            _config: Self::Config,
            _status: WidgetStatus,
            _reload: ReloadHandle,
            _config_change: WidgetConfigChange,
            _recovery: WidgetAction,
            _cx: &mut WidgetCx<'_>,
        ) -> Result<Self, EngineError> {
            Ok(Self)
        }

        fn load(&mut self, _cx: &mut WidgetCx<'_>) {}

        fn has_content(&self) -> bool {
            false
        }

        fn body(&mut self, _parent: NodeId, _cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
            Ok(())
        }
    }

    /// A view with the alpha ID, so ordering can be asserted across two types.
    struct AlphaView;

    impl WidgetView for AlphaView {
        type Config = TestConfig;

        const ID: WidgetId = ALPHA_ID;
        const DISPLAY_NAME: &'static str = "widget.names.candlestick";
        const CATEGORY: WidgetCategory = WidgetCategory::Market;

        fn new(
            _instance_id: WidgetInstanceId,
            _config: Self::Config,
            _status: WidgetStatus,
            _reload: ReloadHandle,
            _config_change: WidgetConfigChange,
            _recovery: WidgetAction,
            _cx: &mut WidgetCx<'_>,
        ) -> Result<Self, EngineError> {
            Ok(Self)
        }

        fn load(&mut self, _cx: &mut WidgetCx<'_>) {}

        fn has_content(&self) -> bool {
            false
        }

        fn body(&mut self, _parent: NodeId, _cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
            Ok(())
        }
    }

    /// The same, under the beta ID.
    struct BetaView;

    impl WidgetView for BetaView {
        type Config = TestConfig;

        const ID: WidgetId = BETA_ID;
        const DISPLAY_NAME: &'static str = "widget.names.cryptocurrency_etf";
        const CATEGORY: WidgetCategory = WidgetCategory::Statistics;

        fn new(
            _instance_id: WidgetInstanceId,
            _config: Self::Config,
            _status: WidgetStatus,
            _reload: ReloadHandle,
            _config_change: WidgetConfigChange,
            _recovery: WidgetAction,
            _cx: &mut WidgetCx<'_>,
        ) -> Result<Self, EngineError> {
            Ok(Self)
        }

        fn load(&mut self, _cx: &mut WidgetCx<'_>) {}

        fn has_content(&self) -> bool {
            false
        }

        fn body(&mut self, _parent: NodeId, _cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
            Ok(())
        }
    }
    /// The selection the linked fixture propagates, written as the payload a pane
    /// stores rather than built from the model types.
    ///
    /// The registry is the framework crate: what it has to carry through a mount without
    /// translating is the link a view offers, and the widget API is the only listing
    /// shape it depends on. The payload is therefore decoded into whatever selection
    /// [`InstrumentLink::new`] takes — its field names are the ones the link's own wire
    /// shape declares — and every assertion below compares against the payload itself.
    const LINKED_SELECTION: &str = r#"{"listing":{"route":{"venue":"binance","scope":"spot"},"listing_id":"listing-id/v1/9999999999999999999999999999999999999999999999999999999999999999"}}"#;

    /// The payload above, as the JSON both the fixture and its assertions read.
    fn linked_selection() -> Value {
        serde_json::from_str(LINKED_SELECTION).expect("the fixture selection is JSON")
    }

    /// A chart-like view, the only kind that offers an instrument link.
    struct LinkedView;

    impl WidgetView for LinkedView {
        type Config = TestConfig;

        const ID: WidgetId = WidgetId::new_static("kytrix.test.linked");
        const DISPLAY_NAME: &'static str = "widget.names.candlestick";
        const CATEGORY: WidgetCategory = WidgetCategory::Market;

        fn new(
            _instance_id: WidgetInstanceId,
            _config: Self::Config,
            _status: WidgetStatus,
            _reload: ReloadHandle,
            _config_change: WidgetConfigChange,
            _recovery: WidgetAction,
            _cx: &mut WidgetCx<'_>,
        ) -> Result<Self, EngineError> {
            Ok(Self)
        }

        fn load(&mut self, _cx: &mut WidgetCx<'_>) {}

        fn has_content(&self) -> bool {
            false
        }

        fn body(&mut self, _parent: NodeId, _cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
            Ok(())
        }

        fn instrument_link(&self) -> Option<InstrumentLink> {
            Some(InstrumentLink::new(
                serde_json::from_value(linked_selection())
                    .expect("the fixture selection is a canonical listing key"),
                |_, _| true,
            ))
        }
    }

    /// A view the framework builds for itself rather than offering to the user: it
    /// carries a picture and a description like any other widget, and the only thing that
    /// keeps it out of a selection surface is its own answer.
    struct InternalView;

    impl WidgetView for InternalView {
        type Config = TestConfig;

        const ID: WidgetId = WidgetId::new_static("kytrix.test.internal");
        const DISPLAY_NAME: &'static str = "widget.names.empty";
        const CATEGORY: WidgetCategory = WidgetCategory::Market;
        const DESCRIPTION: Option<&'static str> = Some("test.descriptions.internal");
        const ICON: Option<&'static str> = Some("widgets/test.png");
        const SELECTABLE: bool = false;

        fn new(
            _instance_id: WidgetInstanceId,
            _config: Self::Config,
            _status: WidgetStatus,
            _reload: ReloadHandle,
            _config_change: WidgetConfigChange,
            _recovery: WidgetAction,
            _cx: &mut WidgetCx<'_>,
        ) -> Result<Self, EngineError> {
            Ok(Self)
        }

        fn load(&mut self, _cx: &mut WidgetCx<'_>) {}

        fn has_content(&self) -> bool {
            false
        }

        fn body(&mut self, _parent: NodeId, _cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
            Ok(())
        }
    }

    /// A view that builds one control into the header group it is asked for.
    ///
    /// The pane owns the header row's layout and hands the widget the container of one
    /// group, which is the whole of the old `WidgetHeader { left, right }` shape.
    struct HeaderedView;

    impl WidgetView for HeaderedView {
        type Config = TestConfig;

        const ID: WidgetId = WidgetId::new_static("kytrix.test.headered");
        const DISPLAY_NAME: &'static str = "widget.names.candlestick";
        const CATEGORY: WidgetCategory = WidgetCategory::Monitoring;

        fn new(
            _instance_id: WidgetInstanceId,
            _config: Self::Config,
            _status: WidgetStatus,
            _reload: ReloadHandle,
            _config_change: WidgetConfigChange,
            _recovery: WidgetAction,
            _cx: &mut WidgetCx<'_>,
        ) -> Result<Self, EngineError> {
            Ok(Self)
        }

        fn load(&mut self, _cx: &mut WidgetCx<'_>) {}

        fn has_content(&self) -> bool {
            false
        }

        fn body(&mut self, _parent: NodeId, _cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
            Ok(())
        }

        fn header(
            &mut self,
            group: HeaderGroup,
            parent: NodeId,
            cx: &mut WidgetCx<'_>,
        ) -> Result<(), EngineError> {
            if group == HeaderGroup::Right {
                return Ok(());
            }
            NodeBuilder::text(SmolStr::new_static("sound"), TextStyle::new(13.0))
                .id("TEST_HEADER_CONTROL")
                .build(cx.tree_mut(), parent)?;
            Ok(())
        }
    }

    /// A view whose first load fails and whose recovery's load settles: the state it
    /// reports is how a test sees which path ran.
    struct RecoveringView {
        status: WidgetStatus,
        recovery: WidgetAction,
        failed: bool,
    }

    impl WidgetView for RecoveringView {
        type Config = TestConfig;

        const ID: WidgetId = WidgetId::new_static("kytrix.test.recovering");
        const DISPLAY_NAME: &'static str = "widget.names.candlestick";
        const CATEGORY: WidgetCategory = WidgetCategory::Market;

        fn new(
            _instance_id: WidgetInstanceId,
            _config: Self::Config,
            status: WidgetStatus,
            _reload: ReloadHandle,
            _config_change: WidgetConfigChange,
            recovery: WidgetAction,
            _cx: &mut WidgetCx<'_>,
        ) -> Result<Self, EngineError> {
            Ok(Self {
                status,
                recovery,
                failed: false,
            })
        }

        fn load(&mut self, cx: &mut WidgetCx<'_>) {
            if self.failed {
                self.status.report(
                    cx.app_mut(),
                    WidgetState::waiting(Notice::message(TEST_WAITING)),
                );
                return;
            }
            self.failed = true;
            self.status.report(
                cx.app_mut(),
                WidgetState::failed(Arc::new(FixtureFailure), self.recovery.clone()),
            );
        }

        fn has_content(&self) -> bool {
            false
        }

        fn body(&mut self, _parent: NodeId, _cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
            Ok(())
        }
    }
    /// A registry holding one fixture registration, which is what a test mounts by ID.
    fn fixture_registry<W: WidgetView>() -> WidgetRegistry {
        let mut registry = WidgetRegistry::default();
        registry
            .register(WidgetRegistration::for_view::<W>())
            .expect("one registration per fixture");

        registry
    }

    /// Mounts one widget by ID the way a pane does: through the registry, with the
    /// composition root's services and the pane's own config report.
    fn mount_by_id(
        registry: &WidgetRegistry,
        harness: &mut WidgetTestApp,
        id: &str,
        config: Option<VersionedWidgetConfig>,
    ) -> AnyWidget {
        mount_with_change(registry, harness, id, config, WidgetConfigChange::default())
    }

    /// The same, over an explicit config report.
    fn mount_with_change(
        registry: &WidgetRegistry,
        harness: &mut WidgetTestApp,
        id: &str,
        config: Option<VersionedWidgetConfig>,
        config_change: WidgetConfigChange,
    ) -> AnyWidget {
        let services = Rc::clone(harness.services_rc());
        let window = harness.window();
        let parent = harness.root();
        registry
            .mount(
                id,
                harness.app_mut(),
                window,
                parent,
                &services,
                config,
                config_change,
            )
            .unwrap_or_else(|error| panic!("{id} should mount: {error}"))
    }

    #[test]
    fn register_should_reject_duplicate_widget_id() {
        let mut registry = WidgetRegistry::default();
        registry
            .register(WidgetRegistration::for_view::<TestView>())
            .expect("first registration should succeed");

        let error = registry
            .register(WidgetRegistration::for_view::<TestView>())
            .expect_err("duplicate registration should fail");

        assert_eq!(error, WidgetRegistryError::DuplicateId(TestView::ID));
    }

    #[test]
    fn registration_should_describe_the_view_type() {
        let registration = WidgetRegistration::for_view::<TestView>();
        let descriptor = registration.descriptor();

        assert_eq!(descriptor.id(), &TestView::ID);
        assert_eq!(descriptor.display_name(), TestView::DISPLAY_NAME);
        assert_eq!(descriptor.category(), TestView::CATEGORY);
        assert_eq!(
            descriptor.config_schema_version(),
            WidgetConfigSchemaVersion::V1
        );
        assert_eq!(descriptor.description(), None);
        assert_eq!(descriptor.icon(), None);
        assert!(
            descriptor.selectable(),
            "a widget is offered unless it says otherwise"
        );
    }

    #[test]
    fn a_view_should_register_the_selectability_it_declares() {
        let registration = WidgetRegistration::for_view::<InternalView>();
        let descriptor = registration.descriptor();

        assert_eq!(descriptor.selectable(), InternalView::SELECTABLE);
        assert_eq!(descriptor.icon(), InternalView::ICON);
    }

    /// The metadata a mounted widget reports is the metadata it was registered with —
    /// identity, description, icon and selectability alike. One construction serves both,
    /// so a pane's widget and the registry's entry for it cannot disagree about any of
    /// them.
    #[test]
    fn a_mounted_widget_should_report_the_descriptor_it_was_registered_with() {
        let registry = fixture_registry::<InternalView>();
        let mut harness = WidgetTestApp::new();
        let widget = mount_by_id(&registry, &mut harness, InternalView::ID.as_str(), None);

        let descriptor = widget.descriptor();
        assert_eq!(
            &descriptor,
            registry
                .get(InternalView::ID.as_str())
                .expect("the fixture is registered")
                .descriptor()
        );
        assert_eq!(descriptor.description(), InternalView::DESCRIPTION);
        assert!(
            !descriptor.selectable(),
            "the widget keeps itself out of a selection surface"
        );
    }

    #[test]
    fn iter_should_return_registrations_in_widget_id_order() {
        let mut registry = WidgetRegistry::default();
        registry
            .register(WidgetRegistration::for_view::<BetaView>())
            .expect("beta registration should succeed");
        registry
            .register(WidgetRegistration::for_view::<AlphaView>())
            .expect("alpha registration should succeed");

        let ids = registry
            .iter()
            .map(|registration| registration.descriptor().id().as_str())
            .collect::<Vec<_>>();

        assert_eq!(ids, ["kytrix.test.alpha", "kytrix.test.beta"]);
    }

    #[test]
    fn mount_should_assign_unique_instance_ids() {
        let registry = fixture_registry::<TestView>();
        let mut harness = WidgetTestApp::new();

        let first = mount_by_id(&registry, &mut harness, TestView::ID.as_str(), None);
        let second = mount_by_id(&registry, &mut harness, TestView::ID.as_str(), None);

        assert_ne!(first.instance_id(), second.instance_id());
        assert_eq!(first.widget_id(), TestView::ID);
    }

    #[test]
    fn mount_should_report_unknown_widget_id() {
        let registry = fixture_registry::<TestView>();
        let mut harness = WidgetTestApp::new();
        let services = Rc::clone(harness.services_rc());
        let window = harness.window();
        let parent = harness.root();

        let error = registry
            .mount_default(
                "kytrix.test.missing",
                harness.app_mut(),
                window,
                parent,
                &services,
                WidgetConfigChange::default(),
            )
            .expect_err("unknown widget should fail");

        assert!(matches!(
            error,
            WidgetMountError::NotRegistered(id) if id == "kytrix.test.missing"
        ));
    }

    #[test]
    fn mount_should_start_the_widget_once() {
        LOADS.with(|loads| loads.set(0));
        let registry = fixture_registry::<TestView>();
        let mut harness = WidgetTestApp::new();

        let widget = mount_by_id(&registry, &mut harness, TestView::ID.as_str(), None);

        assert_eq!(loads(), 1, "the framework starts the widget's work");
        assert_eq!(
            widget.status().state(harness.app()),
            WidgetState::waiting(Notice::message(TEST_WAITING)),
            "what the first load reported is the state the pane reads"
        );
        assert_eq!(harness.router().len(), 1, "the mount routes one frame");
    }

    #[test]
    fn a_mounted_widget_should_keep_the_config_it_was_mounted_with() {
        let registry = fixture_registry::<TestView>();
        let mut harness = WidgetTestApp::new();
        let config =
            VersionedWidgetConfig::encode(&TestConfig { seed: 7 }).expect("config should encode");
        let widget = mount_by_id(
            &registry,
            &mut harness,
            TestView::ID.as_str(),
            Some(config.clone()),
        );

        assert_eq!(
            widget.config(),
            Some(config),
            "the widget reports the config it holds, so the pane stores what it is showing"
        );
    }

    #[test]
    fn a_widget_should_report_the_config_it_holds_after_it_changes_it() {
        let registry = fixture_registry::<MutatingView>();
        let mut harness = WidgetTestApp::new();
        let reports = Rc::new(Cell::new(0));
        let change = WidgetConfigChange::new({
            let reports = reports.clone();
            move |_| reports.set(reports.get() + 1)
        });
        let config =
            VersionedWidgetConfig::encode(&TestConfig { seed: 7 }).expect("config should encode");

        let widget = mount_with_change(
            &registry,
            &mut harness,
            MutatingView::ID.as_str(),
            Some(config),
            change,
        );

        assert_eq!(
            widget
                .config()
                .expect("the fixture persists its config")
                .decode::<TestConfig>()
                .seed,
            9,
            "the envelope is built from the config the widget holds now, not from the one it \
             was mounted with"
        );
        assert_eq!(
            reports.get(),
            1,
            "the report the pane handed the widget is the one that reaches the pane"
        );
    }

    #[test]
    fn a_widget_that_persists_nothing_should_report_no_config() {
        let registry = fixture_registry::<StatelessView>();
        let mut harness = WidgetTestApp::new();
        let stored =
            VersionedWidgetConfig::encode(&TestConfig { seed: 7 }).expect("config should encode");
        let widget = mount_by_id(
            &registry,
            &mut harness,
            StatelessView::ID.as_str(),
            Some(stored),
        );

        assert_eq!(
            widget.config(),
            None,
            "a widget that persists no options stores no config, whatever its pane was \
             restored from"
        );
    }

    #[test]
    fn a_widget_mounted_without_config_should_get_the_defaults() {
        let registry = fixture_registry::<TestView>();
        let mut harness = WidgetTestApp::new();
        let widget = mount_by_id(&registry, &mut harness, TestView::ID.as_str(), None);

        assert_eq!(
            widget
                .config()
                .expect("the fixture persists its config")
                .decode::<TestConfig>()
                .seed,
            0
        );
    }

    /// A stored config a build cannot read is a pane that comes back with default
    /// settings, not one the application refuses to rebuild.
    #[test]
    fn an_unreadable_config_should_become_the_defaults() {
        let registry = fixture_registry::<TestView>();
        let mut harness = WidgetTestApp::new();
        let unreadable: VersionedWidgetConfig = serde_json::from_value(json!({
            "schema_version": WidgetConfigSchemaVersion::V1.get(),
            "payload": "{\"seed\":",
        }))
        .expect("the envelope itself is well formed");
        let widget = mount_by_id(
            &registry,
            &mut harness,
            TestView::ID.as_str(),
            Some(unreadable.clone()),
        );

        assert_eq!(
            widget
                .config()
                .expect("the fixture persists its config")
                .decode::<TestConfig>()
                .seed,
            0,
            "a payload that does not decode is the default config"
        );
        assert_ne!(
            widget.config(),
            Some(unreadable),
            "the pane then writes the rebuilt config back"
        );
    }
    /// The recovery control a mounted widget offers is the framework's: the registry
    /// hands the mount to `AnyWidget::mount`, a failure the widget reports carries that
    /// action, a click goes through the engine's real action dispatch, and the action
    /// runs the widget's `load` again.
    #[test]
    fn a_failed_first_load_should_offer_the_frameworks_recovery() {
        let registry = fixture_registry::<RecoveringView>();
        let mut harness = WidgetTestApp::new();
        let widget = mount_by_id(&registry, &mut harness, RecoveringView::ID.as_str(), None);
        assert!(widget.status().is_failed(harness.app()));
        harness.drain();

        let placeholder = harness
            .find_by_debug_prefix(widget.surface(), "WIDGET_PLACEHOLDER")
            .expect("the failure stands in for the missing content");
        harness.click_recovery(placeholder);

        assert_eq!(
            widget.status().state(harness.app()),
            WidgetState::waiting(Notice::message(TEST_WAITING)),
            "the recovery ran the widget's load again"
        );
    }

    /// The link a pane receives is the exact one the view offered, listing key included:
    /// the mount reads it once and nothing on the way to the pane is allowed to translate
    /// it into another venue or another listing of the same canonical instrument.
    #[test]
    fn a_widget_should_hand_its_instrument_link_to_the_pane_unchanged() {
        let registry = fixture_registry::<LinkedView>();
        let mut harness = WidgetTestApp::new();
        let widget = mount_by_id(&registry, &mut harness, LinkedView::ID.as_str(), None);
        let link = widget
            .instrument_link()
            .expect("the view offers a link, so the pane receives one");
        let offered = linked_selection();

        assert_eq!(
            serde_json::to_value(link.selection()).expect("a selection serializes"),
            offered,
            "the pane receives the selection the view offered, unchanged"
        );
        assert_eq!(
            serde_json::to_value(link.listing()).expect("a listing key serializes"),
            offered["listing"],
            "the listing key is carried through as it was offered"
        );
        assert_eq!(
            serde_json::to_value(link.selection().listing.route).expect("a route serializes"),
            offered["listing"]["route"],
            "the route is carried through rather than translated to another venue"
        );
    }

    /// A view that offers no link gives the pane nothing to coordinate with, which is how
    /// a widget declares that it has no instrument of its own.
    #[test]
    fn a_widget_without_an_instrument_link_should_leave_the_pane_without_one() {
        let registry = fixture_registry::<StatelessView>();
        let mut harness = WidgetTestApp::new();
        let widget = mount_by_id(&registry, &mut harness, StatelessView::ID.as_str(), None);

        assert!(widget.instrument_link().is_none());
    }

    /// The pane builds a header row and hands the widget the container of one group; the
    /// widget builds into that container and an empty group occupies nothing.
    #[test]
    fn a_widget_should_build_its_header_into_the_group_it_is_asked_for() {
        let registry = fixture_registry::<HeaderedView>();
        let mut harness = WidgetTestApp::new();
        let widget = mount_by_id(&registry, &mut harness, HeaderedView::ID.as_str(), None);
        let parent = harness.root();
        let nodes = harness.node_count();

        widget
            .header(harness.app_mut(), HeaderGroup::Right, parent)
            .expect("the group the fixture leaves empty builds nothing");
        assert_eq!(harness.node_count(), nodes);

        widget
            .header(harness.app_mut(), HeaderGroup::Left, parent)
            .expect("the widget builds its control into the group container");
        let control = harness
            .find_by_debug_prefix(parent, "TEST_HEADER_CONTROL")
            .expect("the widget's header control exists");
        assert_eq!(
            harness.tree().debug_id(control),
            Some("TEST_HEADER_CONTROL")
        );
    }

    /// A parent node the pane removed is the engine's refusal, not the registry's: the
    /// mount reports it as its own error and rolls the shell back, so a pane is handed no
    /// half-built widget.
    #[test]
    fn mounting_into_a_stale_parent_should_report_the_engine_error() {
        let registry = fixture_registry::<TestView>();
        let mut harness = WidgetTestApp::new();
        let parent = harness.placeholder_parent();
        let window = harness.window();
        harness
            .app_mut()
            .remove_node(window, parent)
            .expect("the placeholder host is removable");
        let services = Rc::clone(harness.services_rc());
        let nodes = harness.node_count();

        let error = registry
            .mount_default(
                TestView::ID.as_str(),
                harness.app_mut(),
                window,
                parent,
                &services,
                WidgetConfigChange::default(),
            )
            .expect_err("a stale parent refuses the surface");

        assert!(
            matches!(
                error,
                WidgetMountError::Mount(EngineError::StaleNode { .. })
            ),
            "the engine's refusal is carried out as its own error: {error:?}"
        );
        assert_eq!(
            harness.node_count(),
            nodes,
            "the refused mount removes the shell it built"
        );
        assert!(
            harness.router().is_empty(),
            "no frame route is registered for a widget that does not exist"
        );
    }

    /// Two panes on one application: each mounts the same registration, so each has its
    /// own surface, instance identity, state and frame route, and releasing one leaves the
    /// other alone.
    #[test]
    fn two_panes_should_mount_the_same_registration_side_by_side() {
        let registry = fixture_registry::<TestView>();
        let mut harness = WidgetTestApp::new();
        let first = mount_by_id(&registry, &mut harness, TestView::ID.as_str(), None);
        let second = mount_by_id(&registry, &mut harness, TestView::ID.as_str(), None);

        assert_ne!(first.surface(), second.surface());
        assert_ne!(first.instance_id(), second.instance_id());
        assert_eq!(first.widget_id(), second.widget_id());
        assert_eq!(harness.router().len(), 2, "both panes route their frames");

        assert!(first.release(harness.app_mut()) > 0);
        assert_eq!(
            harness.router().len(),
            1,
            "only the released pane's route is gone"
        );
        assert!(harness.tree().get(first.surface()).is_err());
        assert!(harness.tree().get(second.surface()).is_ok());
        assert_eq!(
            second.status().state(harness.app()),
            WidgetState::waiting(Notice::message(TEST_WAITING))
        );
    }

    /// Releasing a pane gives back everything the mount took: its nodes, its frame route
    /// and its value slots. Release is idempotent, and mounting again reuses the slots
    /// instead of growing the table (a leaked pane shows up in `live`, a table that only
    /// ever appends in `slots`).
    #[test]
    fn releasing_a_mounted_widget_should_leave_nothing_behind() {
        let registry = fixture_registry::<TestView>();
        let mut harness = WidgetTestApp::new();
        let baseline_nodes = harness.node_count();
        let baseline = harness.signal_slot_stats();

        let widget = mount_by_id(&registry, &mut harness, TestView::ID.as_str(), None);
        harness.drain();
        assert_eq!(harness.router().len(), 1);
        assert!(
            harness.signal_slot_stats().live > baseline.live,
            "the mounted pane minted its own values"
        );

        assert!(
            widget.release(harness.app_mut()) > 0,
            "the scope owned the pane's signals"
        );
        assert!(widget.is_released());
        assert_eq!(harness.router().len(), 0, "the frame route is unregistered");
        assert!(
            harness.tree().get(widget.surface()).is_err(),
            "the surface's nodes are gone"
        );
        assert_eq!(harness.node_count(), baseline_nodes);
        assert_eq!(
            harness.signal_slot_stats().live,
            baseline.live,
            "release hands every value slot back"
        );
        assert_eq!(
            widget.release(harness.app_mut()),
            0,
            "a second release is a no-op"
        );

        // The first mount grew the table to the pane's peak; every later round has to
        // reuse those slots instead of growing it further.
        let released = harness.signal_slot_stats();
        for _ in 0..8 {
            let widget = mount_by_id(&registry, &mut harness, TestView::ID.as_str(), None);
            widget.release(harness.app_mut());
        }
        assert_eq!(
            harness.signal_slot_stats(),
            released,
            "eight more mount/release rounds reuse the slots instead of growing the table"
        );
        assert_eq!(harness.node_count(), baseline_nodes);
    }
}
