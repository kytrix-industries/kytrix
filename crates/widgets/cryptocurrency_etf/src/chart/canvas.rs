//! The canvas one ETF chart is: its state, its viewport, and its pointer.
//!
//! A chart owns a canvas **node** rather than a value: the engine keeps the node tree,
//! and a canvas is replaced through [`Window::set_canvas`](ui_core::window::Window::set_canvas)
//! when what it should draw moved. Everything a frame reuses — the box it was last
//! laid out in, the shaped labels, the prepared bars and time ticks, the registered
//! mesh of the net-asset line — lives here, so a frame that changed nothing rebuilds
//! nothing (I3).
//!
//! # The pointer is followed one frame behind, on purpose
//!
//! A mouse crossing a plot produces one move event per pixel, and a chart that rebuilt
//! a canvas per pixel would spend the whole gesture drawing. A move records the
//! reading it resolved to, and the next `sync` — which the widget asks for when the
//! reading really moved — applies it. The reading is what the frame draws from, so two
//! positions inside one day's slot are one frame.
//!
//! # The box the chart is given
//!
//! The engine's canvas draws in **absolute window coordinates**, so the chart is laid
//! out from the layout box the node ended up in on this frame, and every box and label
//! it places is derived from that. That is also why the chart is not put inside a
//! scroll container: scrolling moves a node without invalidating the canvas that draws
//! in window coordinates, so the drawing would stay where the node used to be. A pane
//! narrower than [`content_min_size`] squeezes the plot instead of scrolling to it
//! (registered as a W3c difference).
//!
//! # Language, appearance and colour
//!
//! The canvas is not part of the engine's style pass: a canvas holds primitives with
//! colours baked in, so a language switch (which rewrites every label) or an appearance
//! switch (which changes every colour) is a *rebuild*. The node carries a `bind_style`
//! that reads the appearance signal and a `bind_a11y` that reads the language signal,
//! so both changes invalidate it and schedule the frame whose `sync` rebuilds the
//! canvas.

use std::sync::Arc;

use base_ui::{
    style_builder::StyleBuilder,
    theme::{appearance::Appearance, role::Role},
};
use i18n::t_smol;
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    error::EngineError,
    math::{Point, Rect},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{canvas::Canvas, geometry::GeometryId, style::StylePatch},
};
use widget::cx::WidgetCx;

use super::{
    axes::{ChartLayout, nearest_index},
    data::{Metrics, PreparedData},
    paint::{CacheKey, Hover, PreparedFrame, line_geometry, measure, paint, prepare},
    palette::LINE_WIDTH,
    text::ChartTextCache,
};
use crate::model::{
    CryptocurrencyEtfDataset, CryptocurrencyEtfFundSelection, CryptocurrencyEtfWindow,
};

/// Everything one chart reuses between frames.
#[derive(Default)]
struct ChartCache {
    /// The layout of the frame that was prepared from the current inputs.
    layout: Option<ChartLayout>,
    /// The prepared frame, and the inputs it was prepared from.
    prepared: Option<(CacheKey, PreparedFrame)>,
    /// What the canvas on screen was built from.
    painted: Option<PaintedKey>,
    /// The shaped labels, keyed by text and style.
    text: ChartTextCache,
    /// The registered mesh of the net-asset line, released when the line changes.
    line: Option<GeometryId>,
}

/// The inputs one canvas was built from.
///
/// A canvas has every colour baked into it, so the appearance is part of the key; the
/// hover is part of it because the crosshair and the tooltip are; the line's mesh is
/// part of it because a canvas names the geometry it draws.
#[derive(Clone, Copy, Debug, PartialEq)]
struct PaintedKey {
    cache: CacheKey,
    hover: Option<Hover>,
    line: Option<GeometryId>,
    appearance: Appearance,
}

/// One ETF chart: the view it resolved, the cache a frame reuses, and the node it
/// draws into.
pub(crate) struct CryptocurrencyEtfChart {
    data: PreparedData,
    /// Moves whenever `data` is rebuilt, which is how a prepared frame knows the
    /// window under it changed.
    revision: u64,
    cache: ChartCache,
    /// The reading the pointer is over, if it is over the plot.
    hover: Option<Hover>,
    /// The canvas node, once the chart has been built into the pane's body.
    node: Option<NodeId>,
}

impl CryptocurrencyEtfChart {
    /// Resolves one history into the view a chart draws.
    pub(crate) fn new(
        cx: &mut WidgetCx<'_>,
        dataset: Arc<CryptocurrencyEtfDataset>,
        selection: CryptocurrencyEtfFundSelection,
        window: CryptocurrencyEtfWindow,
    ) -> Self {
        let data = PreparedData::new(cx.translate(), dataset, selection, window);
        Self {
            data,
            revision: 1,
            cache: ChartCache::default(),
            hover: None,
            node: None,
        }
    }

    /// Points the chart at a history, a window or a fund, reporting whether anything
    /// about it changed.
    ///
    /// The dataset is compared by pointer as well as by value: a refresh that returned
    /// the same days hands over the same allocation, and a chart that re-derived its
    /// labels from it would do work nobody can see. The locale is part of the
    /// comparison because the labels are: the same days in another language are
    /// another thing on screen, and this is the one place that decides whether to
    /// write them again.
    pub(crate) fn set_view(
        &mut self,
        cx: &mut WidgetCx<'_>,
        dataset: Arc<CryptocurrencyEtfDataset>,
        selection: CryptocurrencyEtfFundSelection,
        window: CryptocurrencyEtfWindow,
    ) -> bool {
        let locale = cx.translate().language();
        if self.data.locale == locale
            && Arc::ptr_eq(&self.data.source, &dataset)
            && self.data.selection == selection
            && self.data.window == window
        {
            return false;
        }
        self.data = PreparedData::new(cx.translate(), dataset, selection, window);
        self.revision = self.revision.wrapping_add(1).max(1);
        self.cache.prepared = None;
        if self
            .hover
            .is_some_and(|hover| hover.index >= self.data.points.len())
        {
            self.hover = None;
        }
        true
    }

    /// Re-resolves the labels when the language changed under a chart that is already
    /// on screen, reporting whether it did.
    ///
    /// The pane cannot wait for the next snapshot: an ETF answer arrives every eight
    /// hours, and a chart that kept the labels of the language the user just left would
    /// be wrong for most of that time.
    pub(crate) fn sync_locale(&mut self, cx: &mut WidgetCx<'_>) -> bool {
        if self.data.locale == cx.translate().language() {
            return false;
        }
        let (dataset, selection, window) = (
            self.data.source.clone(),
            self.data.selection.clone(),
            self.data.window,
        );
        self.set_view(cx, dataset, selection, window)
    }

    /// The point the pointer is over, if any.
    pub(crate) fn hover_index(&self) -> Option<usize> {
        self.hover.map(|hover| hover.index)
    }

    /// The reading a pointer position resolves to, if it is over the plot.
    ///
    /// A position resolves to the reading whose own x it is nearest to: the chart is
    /// about days, not about the pixel the pointer happens to be on.
    pub(crate) fn hover_at(&self, cursor: Point) -> Option<Hover> {
        let layout = self.cache.layout?;
        if !layout.plot.contains(cursor) {
            return None;
        }
        nearest_index(layout.plot, self.data.points.len(), cursor.x).map(|index| Hover {
            index,
            cursor_y: cursor.y.round(),
        })
    }

    /// Applies a pointer position, reporting whether the reading under it moved.
    ///
    /// Moving inside one day's slot is not a movement: the drawing is the same, so a
    /// frame would draw the same pixels.
    pub(crate) fn point_at(&mut self, cursor: Point) -> bool {
        let next = self.hover_at(cursor);
        if self.hover == next {
            return false;
        }
        self.hover = next;
        true
    }

    /// Records that the pointer left the plot, reporting whether it had been on it.
    pub(crate) fn clear_hover(&mut self) -> bool {
        self.hover.take().is_some()
    }

    /// The readings the pane's own row shows for the point under the pointer.
    pub(crate) fn metrics(&self) -> Option<Metrics> {
        self.data.metrics(self.hover_index())
    }

    /// Builds the chart's canvas node into `parent`.
    pub(crate) fn build(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        let language = cx.language();
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();
        let label = t_smol!(translate, "widget.cryptocurrency_etf.chart");
        let node = NodeBuilder::canvas(Canvas::new(Rect::default()))
            .id("TEST_ETF_CHART")
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .tab_stop(false)
            .a11y(A11yOverride {
                role: Some(A11yRole::Group),
                label: Some(label),
                ..A11yOverride::default()
            })
            .bind_a11y(move |read| {
                let _language = read.read(&language);
                A11yPatch {
                    label: Some(t_smol!(translate, "widget.cryptocurrency_etf.chart")),
                    ..A11yPatch::default()
                }
            })
            // A canvas bakes its colours in, so an appearance switch is a rebuild: the
            // read below is what invalidates the node and schedules the frame that
            // rebuilds it. The patch is empty on purpose — nothing about the node's own
            // style follows the theme.
            .bind_style(move |read| {
                let _appearance = theme.appearance(read);
                StylePatch::default()
            })
            .build(cx.tree_mut(), parent)?;
        self.node = Some(node);
        Ok(())
    }

    /// The node the chart draws into, once it has been built.
    pub(crate) fn node(&self) -> Option<NodeId> {
        self.node
    }

    /// Rebuilds the canvas the chart draws, reporting whether it changed.
    ///
    /// The work is split in two: a *prepared* frame is rebuilt only when the box or the
    /// resolved view moved, and the *canvas* is rebuilt only when what it would draw
    /// moved — the prepared frame, the hover, the appearance, the line's mesh, or the
    /// atlas lease of a label it was built from.
    pub(crate) fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<bool, EngineError> {
        let Some(node) = self.node else {
            return Ok(false);
        };
        let Ok(target) = cx.tree().get(node) else {
            return Ok(false);
        };
        let bounds = target.layout().bounds;
        if bounds.size.width <= 0.0 || bounds.size.height <= 0.0 {
            // The frame that laid the node out has not run yet; the next one will.
            return Ok(false);
        }

        let appearance = cx.theme().current(cx.app());
        let theme = cx.theme().clone();
        let colors = theme.theme_for(appearance);
        let key = CacheKey {
            bounds,
            revision: self.revision,
        };
        if self.cache.prepared.as_ref().map(|(cached, _)| *cached) != Some(key) {
            let measurements = measure(cx.app_mut(), &self.data, &mut self.cache.text, colors);
            let layout = ChartLayout::new(
                bounds,
                measurements.left_axis_width,
                measurements.right_axis_width,
            );
            let prepared = prepare(&self.data, layout);
            self.cache.layout = Some(layout);
            self.replace_line(cx, &prepared);
            self.cache.prepared = Some((key, prepared));
        }

        let painted = PaintedKey {
            cache: key,
            hover: self.hover,
            line: self.cache.line,
            appearance,
        };
        // The painted key names what the canvas was built from, but not what its labels
        // were *leased* from: a shaped handle points into a shared atlas, and an atlas
        // generation change or a per-tile eviction can take every lease the canvas holds
        // without moving anything in the key. The engine drops such runs at encode time,
        // so a frame that kept this canvas would show a chart with its axis, date and
        // tooltip labels missing — for good. A stale cache is a rebuild.
        if self.cache.painted == Some(painted) && !self.cache.text.has_stale() {
            return Ok(false);
        }

        let Some((_, prepared)) = self.cache.prepared.as_ref() else {
            return Ok(false);
        };
        let Some(layout) = self.cache.layout else {
            return Ok(false);
        };
        let line = self.cache.line.map(|id| (id, colors.role(Role::FgDefault)));
        let (canvas, whole) = paint(
            cx.app_mut(),
            &self.data,
            prepared,
            layout,
            self.hover,
            colors,
            &mut self.cache.text,
            line,
        );
        // Shaping one label can evict another label's tiles, so a single pass is not
        // always a whole frame: a refused run is shaped again and pushed by a second
        // pass. A second refusal is left to the next frame rather than retried here, so a
        // chart whose labels do not fit the atlas cannot spin this hook.
        let (canvas, whole) = if whole {
            (canvas, whole)
        } else {
            paint(
                cx.app_mut(),
                &self.data,
                prepared,
                layout,
                self.hover,
                colors,
                &mut self.cache.text,
                line,
            )
        };
        let changed = cx.set_canvas(node, canvas)?;
        // An incomplete canvas is not what the chart draws: leaving the key unrecorded is
        // what makes the next frame build it again (I3 holds — a whole frame is still a
        // short circuit).
        if whole {
            self.cache.painted = Some(painted);
        }
        Ok(changed)
    }

    /// Releases the mesh of the previous line and registers the one this frame draws.
    fn replace_line(&mut self, cx: &mut WidgetCx<'_>, prepared: &PreparedFrame) {
        if let Some(previous) = self.cache.line.take() {
            cx.app_mut().geometry_mut().release(previous);
        }
        let Some(points) = prepared.line.as_ref() else {
            return;
        };
        let Some(desc) = line_geometry(points, LINE_WIDTH) else {
            return;
        };
        match cx.app_mut().geometry_mut().insert(desc) {
            Ok(id) => self.cache.line = Some(id),
            Err(error) => {
                log::warn!("the ETF chart could not register its net-asset line: {error}");
            }
        }
    }

    /// Releases the geometry this chart registered and takes its canvas node down.
    ///
    /// The node is the chart's own: a chart that is dropped and re-created (the account
    /// behind the answer changed) removes the node it drew into, because the pane's body
    /// is built only once and the next chart is built into the same host. A node the pane
    /// already removed is not a failure — the framework tears the body down after this
    /// hook.
    pub(crate) fn release(&mut self, cx: &mut WidgetCx<'_>) {
        if let Some(line) = self.cache.line.take() {
            cx.app_mut().geometry_mut().release(line);
        }
        if let Some(node) = self.node.take()
            && cx.tree().get(node).is_ok()
        {
            let window = cx.window();
            let _ = cx.app_mut().remove_node(window, node);
        }
        self.cache.prepared = None;
        self.cache.painted = None;
        self.cache.layout = None;
        self.hover = None;
    }

    /// The layout of the last prepared frame, which is what a hover is resolved
    /// against.
    #[cfg(test)]
    pub(crate) fn layout_for_test(&self) -> Option<ChartLayout> {
        self.cache.layout
    }

    /// The pointer the chart is drawing from.
    #[cfg(test)]
    pub(crate) fn hover_for_test(&self) -> Option<Hover> {
        self.hover
    }

    /// The canvas node's current content.
    #[cfg(test)]
    pub(crate) fn canvas_for_test<'a>(
        &self,
        app: &'a ui_core::app::App,
        window: ui_core::window::WindowId,
    ) -> Option<&'a Canvas> {
        let node = self.node?;
        let target = app.window(window)?.tree().get(node).ok()?;
        match target.kind() {
            ui_core::node::tree::NodeKind::Canvas(canvas) => Some(canvas),
            _ => None,
        }
    }
}
