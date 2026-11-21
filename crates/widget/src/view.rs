//! The contract a statically linked widget implements.
//!
//! A widget writes its business: the protocol it speaks, how it fetches data, the
//! model it keeps, what its ready state looks like, and which controls belong in
//! its header. Everything the twenty widgets used to repeat — the state machine, the
//! spinner, where a notice is shown, the recovery control — belongs to the
//! framework.
//!
//! # The shape is the engine's shape
//!
//! There is no `Render`, no `Entity`, no element tree, and no toolkit context here. A
//! widget builds **nodes** through the [`WidgetCx`] it is handed, and the engine
//! keeps them: the tree is persistent, so a body is built once and updated through
//! the dynamic channels (`bind_text` / `bind_style` / `bind_a11y`) rather than
//! rebuilt every frame.
//!
//! The lifecycle is the same convention [`base_ui::kit`] components follow, mapped
//! onto the widget:
//!
//! | kit phase | widget phase |
//! |---|---|
//! | `new(app, …)` | [`WidgetView::new`] — create signals and collaborators |
//! | `build(tree, parent)` | [`WidgetView::body`] and [`WidgetView::header`] — nodes only |
//! | `mount` / `attach` | framework: the surface builds the body slot and mounts the widget |
//! | `sync(app)` | [`WidgetView::sync`] — frame-driven state, called by the [`FrameRouter`] |
//! | `dispatch(app, &Event)` | [`WidgetView::dispatch`] — platform events, forwarded by the surface |
//! | `release(app)` | [`WidgetView::release`], then the framework removes the surface and releases the scope |
//!
//! # What the widget does not own
//!
//! It does not own the presentation, and it does not own the words. What it reports
//! is a state ([`WidgetState`]); what a user reads is resolved by the surface, from
//! the catalogue, in the language showing on the frame that draws it. A widget that
//! draws text of its own does so through the translation context the [`WidgetCx`]
//! hands out, and reads the language signal **inside** the `bind_text` closure that
//! draws it — that read is what registers the dependency that a language switch
//! invalidates. A widget never reports a language change itself.

use app_config::widget::WidgetConfig;
use ui_core::{error::EngineError, event::Event, node::id::NodeId};

use super::{
    category::WidgetCategory,
    config::WidgetConfigChange,
    identity::{WidgetId, WidgetInstanceId},
    instrument_link::InstrumentLink,
    status::{WidgetAction, WidgetStatus},
};
use crate::cx::WidgetCx;

/// Asks the framework to run [`WidgetView::load`] again.
///
/// This is what a widget's recovery action calls, and what the framework hands to
/// the widget so its own failure can offer a way out. It is an owned `Rc` closure
/// rather than a borrow because a recovery outlives the frame that drew it: the
/// framework's weak handle to the widget is what it captures, so a recovery that
/// fires after the widget was released is a no-op instead of a use-after-free.
#[derive(Clone)]
pub struct ReloadHandle(std::rc::Rc<dyn Fn(&mut ui_core::app::App)>);

impl ReloadHandle {
    /// Wraps `reload`, which the framework calls with the application context.
    pub fn new(reload: impl Fn(&mut ui_core::app::App) + 'static) -> Self {
        Self(std::rc::Rc::new(reload))
    }

    /// Runs the widget's load path again.
    pub fn reload(&self, app: &mut ui_core::app::App) {
        (self.0)(app);
    }
}

impl std::fmt::Debug for ReloadHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ReloadHandle")
    }
}

/// Which of a pane's two header groups a widget is asked to build into.
///
/// The pane fixes the row's layout — linkage slot, left group, drag region, right
/// group, window actions — so a widget only decides what goes in its two groups,
/// and an empty group occupies nothing. The old `WidgetHeader { left, right:
/// AnyElement }` shape is gone: a widget builds its controls directly into the
/// container the pane made for the group, which is the engine's shape for "here is
/// where it goes".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderGroup {
    /// Controls shown before the draggable region.
    Left,
    /// Controls shown after the draggable region.
    Right,
}

/// A statically linked widget view.
///
/// Implementors are constructed by the registry, which owns everything that is
/// the same for every widget: decoding persisted config, creating the state cell,
/// wiring the default recovery, building the surface that presents the widget, and
/// starting the first load.
pub trait WidgetView: Sized + 'static {
    type Config: WidgetConfig;

    const ID: WidgetId;
    /// Message key of the widget name, resolved through the translation context.
    const DISPLAY_NAME: &'static str;
    /// The group of the widget gallery this widget is listed under.
    const CATEGORY: WidgetCategory;
    /// Optional message key of the widget description, resolved through the translation context.
    const DESCRIPTION: Option<&'static str> = None;
    const ICON: Option<&'static str> = None;
    /// Whether a selection surface may offer this widget.
    ///
    /// Every registered widget can be built by ID — that is what a pane restored
    /// from its layout does — but not every widget is one the user picks. A widget
    /// that answers `false` is built by the framework rather than offered by the
    /// gallery, so it stays out of the list of widgets a pane can be given while
    /// remaining a widget like any other: creatable, persistable, and restorable.
    const SELECTABLE: bool = true;

    /// Creates the widget's own state.
    ///
    /// The status cell is the widget's only channel for reporting state, and the
    /// framework keeps its own handle on it — what a pane draws is the surface built
    /// around this same cell. `reload` runs [`load`] again through the framework's
    /// own weak handle, and `recovery` is that same capability labelled for the
    /// user: the widget puts it on every failure it reports, so a failure always
    /// offers a way out. `config_change` is how the widget reports that it changed a
    /// field [`config`](WidgetView::config) hands back, which is what gets that
    /// change written out.
    ///
    /// The widget must not start its work here: the framework calls `load` once
    /// construction is complete. Failing to construct is reported as the engine's
    /// error type — a widget is created inside a live tree, so a failure means the
    /// tree or the window it names is gone.
    ///
    /// [`load`]: WidgetView::load
    #[allow(clippy::too_many_arguments)]
    fn new(
        instance_id: WidgetInstanceId,
        config: Self::Config,
        status: WidgetStatus,
        reload: ReloadHandle,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Result<Self, EngineError>;

    /// Starts or restarts this widget's work.
    ///
    /// The framework calls this once after construction and once every time the
    /// user takes the recovery a failure offered.
    fn load(&mut self, cx: &mut WidgetCx<'_>);

    /// Reports whether this frame has a ready body to render.
    ///
    /// This is a cheap predicate rather than a rendering decision: the framework
    /// asks it *instead of* building the body, so answering it must not construct
    /// any nodes. `false` makes the framework stand in a placeholder.
    fn has_content(&self) -> bool;

    /// Returns the options this widget persists, if it persists any.
    ///
    /// The framework asks for this to write the pane back, so it must be as cheap
    /// as [`has_content`](WidgetView::has_content): return a field, do not compute
    /// one. `None` — the default, and what a widget that persists nothing answers —
    /// means the pane stores no config at all.
    ///
    /// A widget that answers `Some` owns an obligation: every place it changes a
    /// field of that config reports the change through the
    /// [`WidgetConfigChange`] it was created with, or the change lives in memory
    /// only.
    fn config(&self) -> Option<&Self::Config> {
        None
    }

    /// Builds the widget's body into `parent`.
    ///
    /// Called **once** per body generation: the framework builds the body the first
    /// frame [`has_content`](WidgetView::has_content) answers `true` and keeps the
    /// subtree afterwards. Everything that changes afterwards goes through the
    /// engine's dynamic channels — `bind_text` / `bind_style` / `bind_a11y` reading
    /// signals — or through [`sync`](WidgetView::sync). Rebuilding the body per
    /// frame is not the engine's model and would destroy node identity and the
    /// dependency edges the style pass owns.
    ///
    /// `parent` is the framework's content slot; the widget may build a single root
    /// node into it or several.
    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError>;

    /// Frame-driven synchronization, called by the [`FrameRouter`] before every frame.
    ///
    /// This is the place for work that cannot be expressed as a dynamic channel:
    /// redrawing a canvas from the current model, measuring and writing back row
    /// heights, mirroring the pointer into a signal for the next frame. It must be
    /// cheap in the steady state and must not request a frame unless something
    /// changed — the engine's incremental invariant (I3) depends on it.
    fn sync(&mut self, _cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        Ok(())
    }

    /// Builds this widget's controls into one of the pane's header groups.
    ///
    /// Called once per group when the pane builds its header row, so it must be
    /// cheap and must not assume ownership of the nodes it creates. An empty group
    /// builds nothing.
    fn header(
        &mut self,
        _group: HeaderGroup,
        _parent: NodeId,
        _cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        Ok(())
    }

    /// Handles one platform event.
    ///
    /// The surface forwards the events its own controls did not take (a click that
    /// is not on the recovery button, a key press while a widget-owned input has
    /// focus). Returning `true` says the event was consumed. The default consumes
    /// nothing: a widget that builds only kit components which register engine
    /// actions does not need this hook.
    fn dispatch(&mut self, _event: &Event, _cx: &mut WidgetCx<'_>) -> bool {
        false
    }

    /// Releases widget-owned resources before the framework tears the surface down.
    ///
    /// Called once, after the last frame that showed the widget and **before** the
    /// surface's nodes are removed and the widget's signal scope is released. This
    /// is where kit components the widget holds are released
    /// (`Component::release(app)`), scoped key bindings are unbound, and timers are
    /// cancelled. The framework does the rest: `App::remove_node_with_scope` drops
    /// the subtree, the scoped keys/timers/animations under it, and the scope.
    fn release(&mut self, _cx: &mut WidgetCx<'_>) {}

    /// Exposes this widget's instrument synchronization capability, if it has one.
    fn instrument_link(&self) -> Option<InstrumentLink> {
        None
    }
}
