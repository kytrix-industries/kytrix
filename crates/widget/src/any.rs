//! The type-erased widget instance a pane hosts.

use std::rc::Rc;

use app_config::widget::VersionedWidgetConfig;
use gpui_kit::{AnyView, App, Entity, Window};

use super::{
    descriptor::WidgetDescriptor,
    identity::{WidgetId, WidgetInstanceId},
    instrument_link::InstrumentLink,
    surface::WidgetSurface,
    view::{WidgetHeader, WidgetView},
};

/// Asks a widget's own view whether it has content.
///
/// `WidgetView` is not object safe, so the questions the presentation must ask of
/// the concrete type are answered by type-erased closures built at registration.
pub type HasContent = Rc<dyn Fn(&AnyView, &App) -> bool>;

/// Erases [`WidgetView::has_content`] for one concrete widget type.
pub fn has_content_of<W: WidgetView>() -> HasContent {
    Rc::new(|body, cx| {
        body.clone()
            .downcast::<W>()
            .is_ok_and(|body| body.read(cx).has_content())
    })
}

/// Builds one widget's header groups for this frame.
///
/// It carries its own weak handle, so an erased widget never retains its own body.
pub type BuildHeader = Rc<dyn Fn(&mut Window, &mut App) -> WidgetHeader>;

/// Erases [`WidgetView::header`] for one concrete widget type.
pub fn build_header_of<W: WidgetView>(view: gpui_kit::WeakEntity<W>) -> BuildHeader {
    Rc::new(move |window, cx| {
        view.update(cx, |body, cx| W::header(body, window, cx))
            .unwrap_or_default()
    })
}

/// Asks a widget's own view for the config its pane should store.
///
/// The answer is computed on demand rather than kept from creation, because a
/// widget changes its own config while it runs: the envelope a pane was restored
/// from is a record of the past, and writing it back would persist the state the
/// user has already changed away from.
pub type PersistedConfig = Rc<dyn Fn(&AnyView, &App) -> Option<VersionedWidgetConfig>>;

/// Erases [`WidgetView::config`] for one concrete widget type.
pub fn persisted_config_of<W: WidgetView>() -> PersistedConfig {
    Rc::new(|body, cx| {
        let config = body
            .clone()
            .downcast::<W>()
            .ok()?
            .read(cx)
            .config()?
            .clone();

        match VersionedWidgetConfig::encode(&config) {
            Ok(envelope) => Some(envelope),
            // Every field of a widget config is a type serde can write, so this is
            // unreachable in practice. Storing nothing is the least wrong answer:
            // the pane keeps the rest of its layout, and the widget is rebuilt from
            // defaults next time instead of from a half-written config.
            Err(error) => {
                log::error!(
                    "failed to encode the config of `{}`: {error}",
                    W::ID.as_str()
                );

                None
            }
        }
    })
}

/// One widget instance, with its concrete view type erased.
///
/// This is a structural and lifecycle container: it holds the parts a pane needs
/// without knowing which widget it is hosting. It deliberately does not implement
/// `Render` — what a pane embeds is the [`WidgetSurface`], which owns the body, the
/// state the widget reports into, and the presentation that follows from it.
pub struct AnyWidget {
    descriptor: WidgetDescriptor,
    instance_id: WidgetInstanceId,
    surface: Entity<WidgetSurface>,
    build_header: BuildHeader,
    config: PersistedConfig,
    instrument_link: Option<InstrumentLink>,
}

impl AnyWidget {
    pub fn new(
        descriptor: WidgetDescriptor,
        instance_id: WidgetInstanceId,
        surface: Entity<WidgetSurface>,
        build_header: BuildHeader,
        config: PersistedConfig,
        instrument_link: Option<InstrumentLink>,
    ) -> Self {
        Self {
            descriptor,
            instance_id,
            surface,
            build_header,
            config,
            instrument_link,
        }
    }

    /// Returns this widget's immutable type descriptor.
    pub fn descriptor(&self) -> &WidgetDescriptor {
        &self.descriptor
    }

    /// Returns the stable widget type ID.
    pub fn widget_id(&self) -> &WidgetId {
        self.descriptor.id()
    }

    /// Returns this instance's process-local unique ID.
    pub const fn instance_id(&self) -> WidgetInstanceId {
        self.instance_id
    }

    /// Returns the one thing a pane embeds to present this widget.
    ///
    /// The body, the state the widget reports into, and the document a neutral
    /// message is drawn from all belong to it, so reading it every frame is what
    /// subscribes the pane to every change any of them makes.
    pub const fn surface(&self) -> &Entity<WidgetSurface> {
        &self.surface
    }

    /// Builds this frame's header groups.
    ///
    /// The pane calls this once per frame, so the header is always fresh and is
    /// never part of a cached subtree.
    pub fn header(&mut self, window: &mut Window, cx: &mut App) -> WidgetHeader {
        (self.build_header)(window, cx)
    }

    /// Returns the widget's instrument synchronization capability, if it has one.
    pub fn instrument_link(&self) -> Option<&InstrumentLink> {
        self.instrument_link.as_ref()
    }

    /// Returns the config envelope this instance should now be stored as.
    ///
    /// The widget answers with the config it currently holds, so this reflects
    /// every change the widget has reported. `None` means this widget persists no
    /// options, and the pane stores no config for it.
    pub fn config(&self, cx: &App) -> Option<VersionedWidgetConfig> {
        (self.config)(self.surface.read(cx).body(), cx)
    }
}

impl std::fmt::Debug for AnyWidget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AnyWidget")
            .field("descriptor", &self.descriptor)
            .field("instance_id", &self.instance_id)
            .field("surface", &self.surface.entity_id())
            .field("instrument_link", &self.instrument_link.is_some())
            .finish_non_exhaustive()
    }
}
