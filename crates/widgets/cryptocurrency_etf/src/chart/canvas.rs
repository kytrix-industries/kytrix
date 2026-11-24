use std::sync::Arc;

use i18n::t_smol;
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    error::EngineError,
    math::{LogicalPoint, LogicalRect, px},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{canvas::Canvas, geometry::GeometryId, style::StylePatch, style_builder::StyleBuilder},
};
use ui_kit::theme::{appearance::Appearance, role::Role};
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

#[derive(Default)]
struct ChartCache {
    layout: Option<ChartLayout>,
    prepared: Option<(CacheKey, PreparedFrame)>,
    painted: Option<PaintedKey>,
    text: ChartTextCache,
    line: Option<GeometryId>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PaintedKey {
    cache: CacheKey,
    hover: Option<Hover>,
    line: Option<GeometryId>,
    appearance: Appearance,
}

pub(crate) struct CryptocurrencyEtfChart {
    data: PreparedData,
    revision: u64,
    cache: ChartCache,
    hover: Option<Hover>,
    node: Option<NodeId>,
}

impl CryptocurrencyEtfChart {
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

    pub(crate) fn hover_index(&self) -> Option<usize> {
        self.hover.map(|hover| hover.index)
    }

    pub(crate) fn hover_at(&self, cursor: LogicalPoint) -> Option<Hover> {
        let layout = self.cache.layout?;
        if !layout.plot.contains(cursor) {
            return None;
        }
        nearest_index(layout.plot, self.data.points.len(), cursor.x).map(|index| Hover {
            index,
            cursor_y: px(cursor.y.value().round()),
        })
    }

    pub(crate) fn point_at(&mut self, cursor: LogicalPoint) -> bool {
        let next = self.hover_at(cursor);
        if self.hover == next {
            return false;
        }
        self.hover = next;
        true
    }

    pub(crate) fn clear_hover(&mut self) -> bool {
        self.hover.take().is_some()
    }

    pub(crate) fn metrics(&self) -> Option<Metrics> {
        self.data.metrics(self.hover_index())
    }

    pub(crate) fn build(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        let language = cx.language();
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();
        let label = t_smol!(translate, "widget.cryptocurrency_etf.chart");
        let node = NodeBuilder::canvas(Canvas::new(LogicalRect::default()))
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
            .bind_style(move |read| {
                let _appearance = theme.appearance(read);
                StylePatch::default()
            })
            .build(cx.tree_mut(), parent)?;
        self.node = Some(node);
        Ok(())
    }

    pub(crate) fn node(&self) -> Option<NodeId> {
        self.node
    }

    pub(crate) fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<bool, EngineError> {
        let Some(node) = self.node else {
            return Ok(false);
        };
        let Ok(target) = cx.tree().get(node) else {
            return Ok(false);
        };
        let bounds = target.layout().bounds;
        if bounds.size.width <= px(0.0) || bounds.size.height <= px(0.0) {
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
        if whole {
            self.cache.painted = Some(painted);
        }
        Ok(changed)
    }

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

    #[cfg(test)]
    pub(crate) fn layout_for_test(&self) -> Option<ChartLayout> {
        self.cache.layout
    }

    #[cfg(test)]
    pub(crate) fn hover_for_test(&self) -> Option<Hover> {
        self.hover
    }

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
