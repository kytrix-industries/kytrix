//! The contract a statically linked widget implements.
//!
//! A widget writes its business: the protocol it speaks, how it fetches data, the
//! model it keeps, what its ready state looks like, and which controls belong in
//! its header. Everything the fifteen hand-written widgets used to repeat — the
//! state machine, the spinner, where a notice is shown, the recovery control —
//! belongs to the framework.
//!
//! A widget is a view, which is what makes its body cacheable: GPUI can only skip
//! re-rendering a subtree it can identify, and only an entity-backed view has that
//! identity. `Render` is therefore the ready state, and the
//! [`Entity<WidgetStatus>`](WidgetStatus) the framework gave the widget says when
//! that state applies.
//!
//! # What the widget does not own
//!
//! It does not own the presentation, and it does not own the words. What it reports
//! is a state; what a user reads is resolved by the framework from the catalogue,
//! in the language showing on the frame that draws it — so a language change is not
//! something a widget has to answer, and no widget registers an observation of the
//! language.

use app_config::widget::WidgetConfig;
use gpui_kit::{AnyElement, App, Context, Entity, IntoElement, Render, Window};

use super::{
    category::WidgetCategory,
    config::WidgetConfigChange,
    identity::{WidgetId, WidgetInstanceId},
    instrument_link::InstrumentLink,
    status::{WidgetAction, WidgetStatus},
};

/// The two groups of controls a widget contributes to its pane's header row.
///
/// The pane fixes the row's layout — linkage slot, left group, drag region, right
/// group, window actions — so a widget only decides what goes in its two groups.
/// An absent group occupies nothing at all.
#[derive(Default)]
pub struct WidgetHeader {
    /// Controls shown before the draggable region.
    pub left: Option<AnyElement>,
    /// Controls shown after the draggable region.
    pub right: Option<AnyElement>,
}

impl WidgetHeader {
    /// Creates a header with both groups empty.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Sets the controls shown before the draggable region.
    pub fn left(mut self, left: impl IntoElement) -> Self {
        self.left = Some(left.into_any_element());
        self
    }

    /// Sets the controls shown after the draggable region.
    pub fn right(mut self, right: impl IntoElement) -> Self {
        self.right = Some(right.into_any_element());
        self
    }
}

/// A statically linked widget view.
///
/// Implementors are constructed by the registry, which owns everything that is
/// the same for every widget: decoding persisted config, creating the state cell,
/// wiring the default recovery, building the surface that presents the widget, and
/// starting the first load.
pub trait WidgetView: Render + Sized + 'static {
    type Config: WidgetConfig;

    const ID: WidgetId;
    /// Message key of the widget name, resolved through the global translator.
    const DISPLAY_NAME: &'static str;
    /// The group of the widget gallery this widget is listed under.
    const CATEGORY: WidgetCategory;
    const DESCRIPTION: &'static str = "";
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
    /// construction is complete.
    ///
    /// [`load`]: WidgetView::load
    fn new(
        instance_id: WidgetInstanceId,
        config: Self::Config,
        status: Entity<WidgetStatus>,
        reload: impl Fn(&mut App) + 'static,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self;

    /// Starts or restarts this widget's work.
    ///
    /// The framework calls this once after construction and once every time the
    /// user takes the recovery a failure offered.
    fn load(&mut self, cx: &mut Context<Self>);

    /// Reports whether this frame has a ready body to render.
    ///
    /// This is a cheap predicate rather than a rendering decision: the framework
    /// asks it *instead of* building the body, so answering it must not construct
    /// any elements. `false` makes the framework stand in a placeholder.
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

    /// Builds this frame's header groups.
    ///
    /// Called once per frame by the pane, so it must be cheap and always reflect
    /// current state. It is never cached: a header that skipped a frame would
    /// show a stale control.
    fn header(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> WidgetHeader {
        WidgetHeader::empty()
    }

    /// Exposes this widget's instrument synchronization capability, if it has one.
    fn instrument_link(&self) -> Option<InstrumentLink> {
        None
    }
}
