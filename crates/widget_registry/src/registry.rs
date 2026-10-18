//! The widget registry: descriptors, factories, and lookup by persisted ID.

use std::{
    collections::{BTreeMap, btree_map::Entry},
    error::Error as StdError,
    fmt::{self, Display},
};

use app_config::widget::{VersionedWidgetConfig, WidgetConfig, decode_or_default};
use gpui_kit::{AnyView, App, AppContext, Entity, Global};
use smol_str::SmolStr;
use thiserror::Error;
use widget::{
    any::{AnyWidget, has_content_of, persisted_config_of},
    config::WidgetConfigChange,
    descriptor::WidgetDescriptor,
    identity::{WidgetId, WidgetInstanceId},
    status::{WidgetAction, WidgetStatus},
    surface::WidgetSurface,
    view::WidgetView,
};

/// A descriptor paired with its statically linked view factory.
#[derive(Clone)]
pub struct WidgetRegistration {
    descriptor: WidgetDescriptor,
    factory: fn(Option<VersionedWidgetConfig>, WidgetConfigChange, &mut App) -> AnyWidget,
}

impl WidgetRegistration {
    /// Builds a registration whose config loading and type erasure are handled by
    /// the framework.
    pub fn for_view<W: WidgetView>() -> Self {
        Self {
            descriptor: descriptor_of::<W>(),
            factory: create_widget_view::<W>,
        }
    }

    /// Returns this registration's immutable descriptor.
    pub const fn descriptor(&self) -> &WidgetDescriptor {
        &self.descriptor
    }

    /// Instantiates the concrete view and erases it into an [`AnyWidget`].
    ///
    /// `config_change` is the pane's own report, so a widget created here writes
    /// its config back to the pane hosting it.
    pub fn create(
        &self,
        config: Option<VersionedWidgetConfig>,
        config_change: WidgetConfigChange,
        cx: &mut App,
    ) -> Entity<AnyWidget> {
        let widget = (self.factory)(config, config_change, cx);

        cx.new(|_| widget)
    }
}

/// The immutable metadata of one widget type, as every registration and every
/// created instance reports it.
///
/// One construction serves both, so a pane's widget and the registry's entry for
/// it can never disagree about the identity, the description, the icon, or
/// whether the widget may be offered by a selection surface.
fn descriptor_of<W: WidgetView>() -> WidgetDescriptor {
    let mut descriptor = WidgetDescriptor::new(
        W::ID,
        W::DISPLAY_NAME,
        W::CATEGORY,
        W::Config::SCHEMA_VERSION,
    )
    .with_description(W::DESCRIPTION)
    .with_selectable(W::SELECTABLE);
    if let Some(icon) = W::ICON {
        descriptor = descriptor.with_icon(icon);
    }

    descriptor
}

/// Creates one widget instance: config, state, recovery, then the first load.
///
/// The order matters. The state cell the widget reports into exists before the
/// widget does, and the weak handle the recovery runs through with it, so every
/// failure the widget is able to report already offers the user a way out. The
/// surface is built around the erased body before the first load, so a widget that
/// reports while loading is presented by the frame that follows. The first load runs
/// last, after construction, so a widget never has to report a failure from inside
/// its own constructor.
///
/// Nothing here can fail. A stored config that this build cannot read — another
/// schema version, a payload that no longer decodes — becomes the config type's
/// default rather than an error, because a pane the application cannot rebuild is
/// worse than a pane that lost its settings.
fn create_widget_view<W: WidgetView>(
    config: Option<VersionedWidgetConfig>,
    config_change: WidgetConfigChange,
    cx: &mut App,
) -> AnyWidget {
    let instance_id = WidgetInstanceId::new();
    let config = decode_or_default::<W::Config>(config);
    let status: Entity<WidgetStatus> = cx.new(|_| WidgetStatus::new());
    let view = cx.new({
        let status = status.clone();
        move |cx| {
            let weak: gpui_kit::WeakEntity<W> = cx.weak_entity();
            let reload = reload_action(weak.clone());
            // The framework's recovery is the same capability, labelled for the
            // user, so a failure the widget reports always offers a way out.
            let recovery = WidgetAction::new("common.retry", reload_action(weak));

            W::new(
                instance_id,
                config,
                status,
                reload,
                config_change,
                recovery,
                cx,
            )
        }
    });
    let instrument_link = view.read(cx).instrument_link();
    let body: AnyView = view.clone().into();
    let surface = cx.new(|cx| WidgetSurface::new(status, body, has_content_of::<W>(), cx));
    view.update(cx, |view, cx| view.load(cx));

    AnyWidget::new(
        descriptor_of::<W>(),
        instance_id,
        surface,
        widget::any::build_header_of(view.downgrade()),
        persisted_config_of::<W>(),
        instrument_link,
    )
}

/// The one thing every recovery this framework offers does: run `load` again.
///
/// It goes through a weak handle, so a widget that is gone is simply not
/// reloaded, and a widget never keeps itself alive through its own recovery.
fn reload_action<W: WidgetView>(view: gpui_kit::WeakEntity<W>) -> impl Fn(&mut App) + 'static {
    move |cx: &mut App| {
        let _ = view.update(cx, |widget, cx| widget.load(cx));
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

/// Registry for statically linked widget factories.
///
/// A `BTreeMap` gives deterministic ID ordering and logarithmic lookup without
/// maintaining a second index for gallery presentation.
#[derive(Clone, Debug, Default)]
pub struct WidgetRegistry {
    registrations: BTreeMap<WidgetId, WidgetRegistration>,
}

impl Global for WidgetRegistry {}

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

    /// Instantiates a widget by ID.
    pub fn create(
        &self,
        id: &str,
        config: Option<VersionedWidgetConfig>,
        config_change: WidgetConfigChange,
        cx: &mut App,
    ) -> Result<Entity<AnyWidget>, WidgetCreateError> {
        Ok(self
            .get(id)
            .ok_or_else(|| WidgetCreateError::NotRegistered(SmolStr::new(id)))?
            .create(config, config_change, cx))
    }

    /// Instantiates a widget using its config type's default value.
    pub fn create_default(
        &self,
        id: &str,
        config_change: WidgetConfigChange,
        cx: &mut App,
    ) -> Result<Entity<AnyWidget>, WidgetCreateError> {
        self.create(id, None, config_change, cx)
    }

    /// Instantiates a widget from the registry stored in GPUI global state.
    ///
    /// The registration is cloned before invoking the factory so the immutable
    /// global borrow does not overlap the factory's mutable `App` borrow.
    pub fn create_global(
        id: &str,
        config: Option<VersionedWidgetConfig>,
        config_change: WidgetConfigChange,
        cx: &mut App,
    ) -> Result<Entity<AnyWidget>, WidgetCreateError> {
        let registration = cx
            .global::<Self>()
            .get(id)
            .cloned()
            .ok_or_else(|| WidgetCreateError::NotRegistered(SmolStr::new(id)))?;

        Ok(registration.create(config, config_change, cx))
    }

    /// Instantiates a default-config widget from GPUI global state.
    pub fn create_default_global(
        id: &str,
        config_change: WidgetConfigChange,
        cx: &mut App,
    ) -> Result<Entity<AnyWidget>, WidgetCreateError> {
        Self::create_global(id, None, config_change, cx)
    }

    /// Iterates registrations in stable widget ID order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &WidgetRegistration> {
        self.registrations.values()
    }
}

/// A widget registration or lookup failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WidgetRegistryError {
    /// A second registration attempted to claim an existing stable ID.
    DuplicateId(WidgetId),
}

impl Display for WidgetRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateId(id) => write!(formatter, "widget id `{id}` is already registered"),
        }
    }
}

impl StdError for WidgetRegistryError {}

#[derive(Debug, Error)]
pub enum WidgetCreateError {
    #[error("widget id `{0}` is not registered")]
    NotRegistered(SmolStr),
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use app_config::widget::{VersionedWidgetConfig, WidgetConfig, WidgetConfigSchemaVersion};
    use gpui_kit::{App, Context, Empty, IntoElement, Render, TestAppContext, Window};
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};
    use widget::{
        category::WidgetCategory,
        config::WidgetConfigChange,
        identity::{WidgetId, WidgetInstanceId},
        instrument_link::InstrumentLink,
        status::{WidgetAction, WidgetState, WidgetStatus, WidgetStatusExt},
        view::WidgetView,
    };

    use super::*;

    const ALPHA_ID: WidgetId = WidgetId::new_static("kytrix.test.alpha");
    const BETA_ID: WidgetId = WidgetId::new_static("kytrix.test.beta");
    const SCHEMA_V1: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;

    const MESSAGES: &str = r#"
common:
  retry: Retry
"#;

    /// Installs the translator and language the framework localizes a recovery
    /// label with.
    fn install(cx: &mut App) {
        let i18n = i18n::translator::types::I18n::builder()
            .locale_yaml(i18n::language::codes::LanguageCode::En, MESSAGES)
            .expect("test locale should parse")
            .fallback(i18n::language::codes::LanguageCode::En)
            .build()
            .expect("test translator should build");

        cx.set_global(i18n);
        cx.set_global(i18n::language::codes::LanguageCode::En);
    }

    /// A test app with the translator every creation path needs.
    fn app() -> TestAppContext {
        let cx = TestAppContext::single();
        cx.update(install);

        cx
    }

    #[derive(Clone, Default, Serialize, Deserialize)]
    struct TestConfig {
        seed: u64,
    }

    impl WidgetConfig for TestConfig {
        const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
    }

    /// Counts how many times the framework asks a widget to load.
    #[derive(Clone, Default)]
    struct Loads(Rc<Cell<usize>>);

    impl Loads {
        fn count(&self) -> usize {
            self.0.get()
        }
    }

    struct TestView {
        loads: Loads,
        config: TestConfig,
        recovery: WidgetAction,
        reload: Rc<dyn Fn(&mut App)>,
        config_change: WidgetConfigChange,
        /// The state this widget reports into.
        ///
        /// A widget keeps the cell it was handed rather than a copy of it: it is how
        /// what the widget is doing reaches the pane showing it.
        status: Entity<WidgetStatus>,
    }

    impl WidgetView for TestView {
        type Config = TestConfig;

        const ID: WidgetId = WidgetId::new_static("kytrix.test.typed");
        const DISPLAY_NAME: &'static str = "widget.names.candlestick";
        const CATEGORY: WidgetCategory = WidgetCategory::Market;

        fn new(
            _instance_id: WidgetInstanceId,
            config: Self::Config,
            status: Entity<WidgetStatus>,
            reload: impl Fn(&mut App) + 'static,
            config_change: WidgetConfigChange,
            recovery: WidgetAction,
            _cx: &mut Context<Self>,
        ) -> Self {
            let loads = Loads::default();
            Self {
                loads,
                config,
                recovery,
                reload: Rc::new(reload),
                config_change,
                status,
            }
        }

        fn load(&mut self, _cx: &mut Context<Self>) {
            self.loads.0.set(self.loads.0.get() + 1);
        }

        fn has_content(&self) -> bool {
            false
        }

        fn config(&self) -> Option<&Self::Config> {
            Some(&self.config)
        }
    }

    impl Render for TestView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            Empty
        }
    }

    /// A view that persists nothing, so the framework stores no config for it.
    struct StatelessView;

    impl WidgetView for StatelessView {
        type Config = TestConfig;

        const ID: WidgetId = WidgetId::new_static("kytrix.test.stateless");
        const DISPLAY_NAME: &'static str = "widget.names.news";
        const CATEGORY: WidgetCategory = WidgetCategory::Monitoring;

        fn new(
            _instance_id: WidgetInstanceId,
            _config: Self::Config,
            _status: Entity<WidgetStatus>,
            _reload: impl Fn(&mut App) + 'static,
            _config_change: WidgetConfigChange,
            _recovery: WidgetAction,
            _cx: &mut Context<Self>,
        ) -> Self {
            Self
        }

        fn load(&mut self, _cx: &mut Context<Self>) {}

        fn has_content(&self) -> bool {
            false
        }
    }

    impl Render for StatelessView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            Empty
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
            _status: Entity<WidgetStatus>,
            _reload: impl Fn(&mut App) + 'static,
            _config_change: WidgetConfigChange,
            _recovery: WidgetAction,
            _cx: &mut Context<Self>,
        ) -> Self {
            Self
        }

        fn load(&mut self, _cx: &mut Context<Self>) {}

        fn has_content(&self) -> bool {
            false
        }
    }

    impl Render for AlphaView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            Empty
        }
    }

    /// The same, under the beta ID.
    struct BetaView;

    impl WidgetView for BetaView {
        type Config = TestConfig;

        const ID: WidgetId = BETA_ID;
        const DISPLAY_NAME: &'static str = "widget.names.etf_flow";
        const CATEGORY: WidgetCategory = WidgetCategory::Statistics;

        fn new(
            _instance_id: WidgetInstanceId,
            _config: Self::Config,
            _status: Entity<WidgetStatus>,
            _reload: impl Fn(&mut App) + 'static,
            _config_change: WidgetConfigChange,
            _recovery: WidgetAction,
            _cx: &mut Context<Self>,
        ) -> Self {
            Self
        }

        fn load(&mut self, _cx: &mut Context<Self>) {}

        fn has_content(&self) -> bool {
            false
        }
    }

    impl Render for BetaView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            Empty
        }
    }

    /// The selection the linked fixture propagates, written as the payload a pane
    /// stores rather than built from the model types.
    ///
    /// The registry is the framework crate: what it has to carry through
    /// registration without translating is the link a view offers, and the widget
    /// API is the only listing shape it depends on. The payload is therefore
    /// decoded into whatever selection [`InstrumentLink::new`] takes — its field
    /// names are the ones the link's own wire shape declares — and every
    /// assertion below compares against the payload itself.
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
            _status: Entity<WidgetStatus>,
            _reload: impl Fn(&mut App) + 'static,
            _config_change: WidgetConfigChange,
            _recovery: WidgetAction,
            _cx: &mut Context<Self>,
        ) -> Self {
            Self
        }

        fn load(&mut self, _cx: &mut Context<Self>) {}

        fn has_content(&self) -> bool {
            false
        }

        fn instrument_link(&self) -> Option<InstrumentLink> {
            Some(InstrumentLink::new(
                serde_json::from_value(linked_selection())
                    .expect("the fixture selection is a canonical listing key"),
                |_, _| true,
            ))
        }
    }

    impl Render for LinkedView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            Empty
        }
    }

    /// A view the framework builds for itself rather than offering to the user:
    /// it carries a picture and a description like any other widget, and the only
    /// thing that keeps it out of a selection surface is its own answer.
    struct InternalView;

    impl WidgetView for InternalView {
        type Config = TestConfig;

        const ID: WidgetId = WidgetId::new_static("kytrix.test.internal");
        const DISPLAY_NAME: &'static str = "widget.names.empty";
        const CATEGORY: WidgetCategory = WidgetCategory::Market;
        const DESCRIPTION: &'static str = "built by the framework, never offered";
        const ICON: Option<&'static str> = Some("widgets/test.png");
        const SELECTABLE: bool = false;

        fn new(
            _instance_id: WidgetInstanceId,
            _config: Self::Config,
            _status: Entity<WidgetStatus>,
            _reload: impl Fn(&mut App) + 'static,
            _config_change: WidgetConfigChange,
            _recovery: WidgetAction,
            _cx: &mut Context<Self>,
        ) -> Self {
            Self
        }

        fn load(&mut self, _cx: &mut Context<Self>) {}

        fn has_content(&self) -> bool {
            false
        }
    }

    impl Render for InternalView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            Empty
        }
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
        assert_eq!(descriptor.config_schema_version(), SCHEMA_V1);
        assert_eq!(descriptor.description(), "");
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

    /// The metadata a created widget reports is the metadata it was registered
    /// with — identity, description, icon and selectability alike. One
    /// construction serves both, so a pane's widget and the registry's entry for
    /// it cannot disagree about any of them.
    #[test]
    fn a_created_widget_should_report_the_descriptor_it_was_registered_with() {
        let registration = WidgetRegistration::for_view::<InternalView>();
        let cx = app();

        cx.update(|cx| {
            let widget = registration.create(None, WidgetConfigChange::default(), cx);
            let descriptor = widget.read(cx).descriptor();

            assert_eq!(descriptor, registration.descriptor());
            assert!(!descriptor.selectable());
        });
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
    fn create_should_assign_unique_instance_ids() {
        let mut registry = WidgetRegistry::default();
        registry
            .register(WidgetRegistration::for_view::<TestView>())
            .expect("registration should succeed");
        let cx = app();

        cx.update(|cx| {
            let first = registry
                .create_default("kytrix.test.typed", WidgetConfigChange::default(), cx)
                .expect("registered widget should be created");
            let second = registry
                .create_default("kytrix.test.typed", WidgetConfigChange::default(), cx)
                .expect("registered widget should be created again");

            assert_ne!(first.read(cx).instance_id(), second.read(cx).instance_id());
        });
    }

    #[test]
    fn create_should_report_unknown_widget_id() {
        let registry = WidgetRegistry::default();
        let cx = app();

        cx.update(|cx| {
            let error = registry
                .create_default("kytrix.test.missing", WidgetConfigChange::default(), cx)
                .expect_err("unknown widget should fail");

            assert!(matches!(
                error,
                WidgetCreateError::NotRegistered(id) if id == "kytrix.test.missing"
            ));
        });
    }

    #[test]
    fn create_should_start_the_widget_once() {
        let registration = WidgetRegistration::for_view::<TestView>();
        let cx = app();

        cx.update(|cx| {
            let widget = registration.create(None, WidgetConfigChange::default(), cx);
            let body = widget.read(cx).surface().read(cx).body().clone();
            let loads = body
                .downcast::<TestView>()
                .expect("the body should be the widget view")
                .read(cx)
                .loads
                .count();

            assert_eq!(loads, 1, "the framework starts the widget's work");
        });
    }

    #[test]
    fn a_created_widget_should_keep_the_config_it_was_created_with() {
        let registration = WidgetRegistration::for_view::<TestView>();
        let cx = app();
        let config =
            VersionedWidgetConfig::encode(&TestConfig { seed: 7 }).expect("config should encode");

        cx.update(|cx| {
            let widget =
                registration.create(Some(config.clone()), WidgetConfigChange::default(), cx);

            assert_eq!(
                widget.read(cx).config(cx),
                Some(config),
                "the widget reports the config it holds, so the pane stores what it is showing"
            );
        });
    }

    #[test]
    fn a_widget_should_report_its_config_after_it_changes_it() {
        let registration = WidgetRegistration::for_view::<TestView>();
        let cx = app();

        cx.update(|cx| {
            let widget = registration.create(None, WidgetConfigChange::default(), cx);
            let body = widget
                .read(cx)
                .surface()
                .read(cx)
                .body()
                .clone()
                .downcast::<TestView>()
                .expect("the body should be the widget view");

            body.update(cx, |body, _| body.config.seed = 9);

            assert_eq!(
                widget.read(cx).config(cx),
                VersionedWidgetConfig::encode(&TestConfig { seed: 9 }).ok(),
                "the envelope is built from the config the widget holds now, not \
                 from the one it was created with"
            );
        });
    }

    #[test]
    fn a_widget_that_persists_nothing_should_report_no_config() {
        let registration = WidgetRegistration::for_view::<StatelessView>();
        let cx = app();
        let stored =
            VersionedWidgetConfig::encode(&TestConfig { seed: 7 }).expect("config should encode");

        cx.update(|cx| {
            let widget = registration.create(Some(stored), WidgetConfigChange::default(), cx);

            assert_eq!(
                widget.read(cx).config(cx),
                None,
                "a widget that persists no options stores no config, whatever its \
                 pane was restored from"
            );
        });
    }

    #[test]
    fn a_widget_created_without_config_should_get_the_defaults() {
        let registration = WidgetRegistration::for_view::<TestView>();
        let cx = app();

        cx.update(|cx| {
            let widget = registration.create(None, WidgetConfigChange::default(), cx);
            let body = widget
                .read(cx)
                .surface()
                .read(cx)
                .body()
                .clone()
                .downcast::<TestView>()
                .expect("the body should be the widget view");

            assert_eq!(body.read(cx).config.seed, 0);
        });
    }

    /// A stored config a build cannot read is a pane that comes back with default
    /// settings, not one the application refuses to rebuild.
    #[test]
    fn an_unreadable_config_should_become_the_defaults() {
        let registration = WidgetRegistration::for_view::<TestView>();
        let cx = app();
        let unreadable: VersionedWidgetConfig = serde_json::from_value(json!({
            "schema_version": SCHEMA_V1.get(),
            "payload": "{\"seed\":",
        }))
        .expect("the envelope itself is well formed");

        cx.update(|cx| {
            let widget =
                registration.create(Some(unreadable.clone()), WidgetConfigChange::default(), cx);
            let body = widget
                .read(cx)
                .surface()
                .read(cx)
                .body()
                .clone()
                .downcast::<TestView>()
                .expect("the body should be the widget view");

            assert_eq!(
                body.read(cx).config.seed,
                0,
                "a payload that does not decode is the default config"
            );
            assert_ne!(
                widget.read(cx).config(cx),
                Some(unreadable),
                "the pane then writes the rebuilt config back"
            );
        });
    }

    /// The status a created widget is given is the state its surface presents, and
    /// it starts out connecting: someone asked for this widget, so its first piece
    /// of work is the connection its data depends on.
    #[test]
    fn create_should_give_the_widget_a_status_that_starts_connecting() {
        let registration = WidgetRegistration::for_view::<TestView>();
        let cx = app();

        cx.update(|cx| {
            let widget = registration.create(None, WidgetConfigChange::default(), cx);
            let status = widget.read(cx).surface().read(cx).status().clone();

            assert_eq!(status.state(cx), WidgetState::Connecting);
        });
    }

    /// What the widget reports and what the pane presents are one state: the cell
    /// the widget was given is the one the surface the pane embeds reads.
    #[test]
    fn the_status_the_widget_was_given_should_be_the_one_the_pane_reads() {
        let registration = WidgetRegistration::for_view::<TestView>();
        let cx = app();

        cx.update(|cx| {
            let widget = registration.create(None, WidgetConfigChange::default(), cx);
            let body = widget
                .read(cx)
                .surface()
                .read(cx)
                .body()
                .clone()
                .downcast::<TestView>()
                .expect("the body should be the widget view");
            let status = widget.read(cx).surface().read(cx).status().clone();
            let reported = body.read(cx).status.clone();

            reported.report(WidgetState::Loading, cx);

            assert_eq!(
                status.state(cx),
                WidgetState::Loading,
                "the widget must publish into the state the pane reads"
            );
        });
    }

    #[test]
    fn the_default_recovery_should_reload_the_widget() {
        let registration = WidgetRegistration::for_view::<TestView>();
        let cx = app();

        cx.update(|cx| {
            let widget = registration.create(None, WidgetConfigChange::default(), cx);
            let body = widget
                .read(cx)
                .surface()
                .read(cx)
                .body()
                .clone()
                .downcast::<TestView>()
                .expect("the body should be the widget view");

            let recovery = body.read(cx).recovery.clone();
            assert_eq!(recovery.label(cx).as_ref(), "Retry");
            assert!(recovery.is_enabled());
            recovery.run(cx);

            assert_eq!(body.read(cx).loads.count(), 2);
        });
    }

    #[test]
    fn the_reload_the_widget_was_given_should_also_restart_it() {
        let registration = WidgetRegistration::for_view::<TestView>();
        let cx = app();

        cx.update(|cx| {
            let widget = registration.create(None, WidgetConfigChange::default(), cx);
            let body = widget
                .read(cx)
                .surface()
                .read(cx)
                .body()
                .clone()
                .downcast::<TestView>()
                .expect("the body should be the widget view");

            let reload = body.read(cx).reload.clone();
            reload(cx);

            assert_eq!(body.read(cx).loads.count(), 2);
        });
    }

    #[test]
    fn the_config_report_the_widget_was_given_should_reach_the_pane() {
        let registration = WidgetRegistration::for_view::<TestView>();
        let cx = app();
        let reports = Rc::new(Cell::new(0));
        let change = WidgetConfigChange::new({
            let reports = reports.clone();
            move |_| reports.set(reports.get() + 1)
        });

        cx.update(|cx| {
            let widget = registration.create(None, change, cx);
            let body = widget
                .read(cx)
                .surface()
                .read(cx)
                .body()
                .clone()
                .downcast::<TestView>()
                .expect("the body should be the widget view");

            let change = body.read(cx).config_change.clone();
            change.publish(cx);
        });

        assert_eq!(reports.get(), 1);
    }

    #[test]
    fn create_global_should_release_registry_borrow_before_the_factory_runs() {
        let mut registry = WidgetRegistry::default();
        registry
            .register(WidgetRegistration::for_view::<TestView>())
            .expect("registration should succeed");
        let cx = app();

        cx.update(|cx| {
            cx.set_global(registry);
            let widget = WidgetRegistry::create_default_global(
                "kytrix.test.typed",
                WidgetConfigChange::default(),
                cx,
            )
            .expect("global registration should create a widget");

            assert_eq!(widget.read(cx).widget_id().as_str(), "kytrix.test.typed");
        });
    }

    #[test]
    fn a_widget_should_be_able_to_override_its_header() {
        struct HeaderedView;

        impl WidgetView for HeaderedView {
            type Config = TestConfig;

            const ID: WidgetId = WidgetId::new_static("kytrix.test.headered");
            const DISPLAY_NAME: &'static str = "widget.names.candlestick";
            const CATEGORY: WidgetCategory = WidgetCategory::Monitoring;

            fn new(
                _instance_id: WidgetInstanceId,
                _config: Self::Config,
                _status: Entity<WidgetStatus>,
                _reload: impl Fn(&mut App) + 'static,
                _config_change: WidgetConfigChange,
                _recovery: WidgetAction,
                _cx: &mut Context<Self>,
            ) -> Self {
                Self
            }

            fn load(&mut self, _cx: &mut Context<Self>) {}

            fn has_content(&self) -> bool {
                true
            }
        }

        impl Render for HeaderedView {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                Empty
            }
        }

        let registration = WidgetRegistration::for_view::<HeaderedView>();
        let cx = app();

        cx.update(|cx| {
            let widget = registration.create(None, WidgetConfigChange::default(), cx);
            let body = widget
                .read(cx)
                .surface()
                .read(cx)
                .body()
                .clone()
                .downcast::<HeaderedView>()
                .expect("the body should be the widget view");

            assert!(body.read(cx).has_content());
            assert_eq!(
                registration.descriptor().id().as_str(),
                "kytrix.test.headered"
            );
        });
    }

    /// The link a pane receives is the exact one the view offered, listing key
    /// included: registration reads it once, at build time, and nothing on the way
    /// to the pane is allowed to translate it into another venue or another
    /// listing of the same canonical instrument.
    #[test]
    fn a_widget_should_hand_its_instrument_link_to_the_pane_unchanged() {
        let registration = WidgetRegistration::for_view::<LinkedView>();
        let cx = app();

        cx.update(|cx| {
            let widget = registration.create(None, WidgetConfigChange::default(), cx);
            let link = widget
                .read(cx)
                .instrument_link()
                .expect("the view offers a link, so the pane receives one")
                .clone();
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
        });
    }

    /// A view that offers no link gives the pane nothing to coordinate with, which
    /// is how a widget declares that it has no instrument of its own.
    #[test]
    fn a_widget_without_an_instrument_link_should_leave_the_pane_without_one() {
        let registration = WidgetRegistration::for_view::<StatelessView>();
        let cx = app();

        cx.update(|cx| {
            let widget = registration.create(None, WidgetConfigChange::default(), cx);

            assert!(widget.read(cx).instrument_link().is_none());
        });
    }
}
