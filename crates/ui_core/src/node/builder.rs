use std::sync::Arc;

use smol_str::SmolStr;

use crate::{
    error::EngineError,
    event::Action,
    layout::{LayoutOverride, MeasureFn, hit_test::HitShape},
    math::LogicalRect,
    node::{
        dependency::ReadCtx,
        id::NodeId,
        tree::{NodeKind, NodeSpec, NodeTree},
    },
    paint::{
        canvas::Canvas,
        geometry::GeometryId,
        style::{PaintSpec, Style, StylePatch},
    },
    text::{TextContent, TextStyle},
    text_edit::EditableSpec,
};

pub struct NodeBuilder {
    kind: NodeKind,
    style: Style,
    bind_style: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>>,
    measure: Option<MeasureFn>,
    hit_shape: Option<HitShape>,
    layout_override: Option<LayoutOverride>,
    bind_text: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> SmolStr>>,
    editable: Option<EditableSpec>,
    paint: Option<PaintSpec>,
    debug_id: Option<SmolStr>,
    action: Option<Action>,
    drag: Option<Action>,
    focus_ring: Option<crate::paint::style::FocusRing>,
    a11y: Option<crate::a11y::A11yOverride>,
    bind_a11y: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> crate::a11y::A11yPatch>>,
    tab_stop: bool,
    press_focuses_editable: bool,
}

impl Default for NodeBuilder {
    fn default() -> Self {
        Self::boxed()
    }
}

impl NodeBuilder {
    #[must_use]
    pub fn boxed() -> Self {
        Self {
            kind: NodeKind::Box,
            style: Style::default(),
            bind_style: None,
            measure: None,
            hit_shape: None,
            layout_override: None,
            bind_text: None,
            editable: None,
            paint: None,
            debug_id: None,
            action: None,
            drag: None,
            focus_ring: None,
            a11y: None,
            bind_a11y: None,
            tab_stop: true,
            press_focuses_editable: false,
        }
    }

    #[must_use]
    pub fn press_focuses_editable(mut self) -> Self {
        self.press_focuses_editable = true;
        self
    }

    #[must_use]
    pub fn a11y(mut self, a11y: crate::a11y::A11yOverride) -> Self {
        self.a11y = Some(a11y);
        self
    }

    #[must_use]
    pub fn bind_a11y(
        self,
        bind_a11y: impl Fn(&mut ReadCtx<'_>) -> crate::a11y::A11yPatch + 'static,
    ) -> Self {
        Self {
            bind_a11y: Some(Box::new(bind_a11y)),
            ..self
        }
    }

    #[must_use]
    pub fn tab_stop(mut self, tab_stop: bool) -> Self {
        self.tab_stop = tab_stop;
        self
    }

    #[must_use]
    pub fn focus_ring(mut self, ring: crate::paint::style::FocusRing) -> Self {
        self.focus_ring = Some(ring);
        self
    }

    #[must_use]
    pub fn on_click(mut self, action: Action) -> Self {
        self.action = Some(action);
        self
    }

    #[must_use]
    pub fn on_action(mut self, action: Action) -> Self {
        self.action = Some(action);
        self
    }

    #[must_use]
    pub fn on_drag(mut self, action: Action) -> Self {
        self.drag = Some(action);
        self
    }

    #[must_use]
    pub fn canvas(canvas: Canvas) -> Self {
        Self {
            kind: NodeKind::Canvas(Arc::new(canvas)),
            ..Self::boxed()
        }
    }

    #[must_use]
    pub fn geometry(geometry: GeometryId) -> Self {
        Self {
            kind: NodeKind::Geometry(geometry),
            ..Self::boxed()
        }
    }

    #[must_use]
    pub fn image(
        resource: crate::resources::ResourceId,
        fit: crate::node::image::ImageFit,
    ) -> Self {
        Self {
            kind: NodeKind::Image(Box::new(crate::node::image::ImageContent::new(
                crate::node::image::ImageSource::Ready(resource),
                fit,
            ))),
            ..Self::boxed()
        }
    }

    #[must_use]
    pub fn image_pending(
        pending: crate::resources::PendingResource,
        fit: crate::node::image::ImageFit,
    ) -> Self {
        Self {
            kind: NodeKind::Image(Box::new(crate::node::image::ImageContent::new(
                crate::node::image::ImageSource::Pending(pending),
                fit,
            ))),
            ..Self::boxed()
        }
    }

    #[must_use]
    pub fn image_modulation(mut self, modulation: crate::paint::color::Color) -> Self {
        if let NodeKind::Image(content) = &mut self.kind {
            content.set_modulation(modulation);
        }
        self
    }

    #[must_use]
    pub fn text(text: impl Into<SmolStr>, style: TextStyle) -> Self {
        Self {
            kind: NodeKind::Text(Box::new(TextContent::new(text, style))),
            ..Self::boxed()
        }
    }

    #[must_use]
    pub fn editable_text(spec: EditableSpec) -> Self {
        let content = TextContent::new(SmolStr::new_static(""), spec.text_style.clone());
        Self {
            kind: NodeKind::Text(Box::new(content)),
            editable: Some(spec),
            ..Self::boxed()
        }
    }

    #[must_use]
    pub fn inline_boxes(
        mut self,
        boxes: impl IntoIterator<Item = crate::text::InlineBoxSpec>,
    ) -> Self {
        if let NodeKind::Text(content) = &mut self.kind {
            content.inline_boxes = boxes.into_iter().collect();
        }
        self
    }

    #[must_use]
    pub fn runs(mut self, runs: impl IntoIterator<Item = crate::text::TextRun>) -> Self {
        if let NodeKind::Text(content) = &mut self.kind {
            content.set_runs(runs);
        }
        self
    }

    #[must_use]
    pub fn bind_text(self, bind_text: impl Fn(&mut ReadCtx<'_>) -> SmolStr + 'static) -> Self {
        Self {
            bind_text: Some(Box::new(bind_text)),
            ..self
        }
    }

    #[must_use]
    pub fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    #[must_use]
    pub fn bind_style(self, bind_style: impl Fn(&mut ReadCtx<'_>) -> StylePatch + 'static) -> Self {
        Self {
            bind_style: Some(Box::new(bind_style)),
            ..self
        }
    }

    #[must_use]
    pub fn measure(mut self, measure: MeasureFn) -> Self {
        self.measure = Some(measure);
        self
    }

    #[must_use]
    pub fn hit_shape(mut self, hit_shape: HitShape) -> Self {
        self.hit_shape = Some(hit_shape);
        self
    }

    #[must_use]
    pub fn layout_override(mut self, rect: LogicalRect) -> Self {
        self.style.position(taffy::style::Position::Absolute);
        self.layout_override = Some(LayoutOverride::new(rect));
        self
    }

    #[must_use]
    pub fn paint(self, paint: PaintSpec) -> Self {
        Self {
            paint: Some(paint),
            ..self
        }
    }

    #[must_use]
    pub fn id(self, id: impl Into<SmolStr>) -> Self {
        Self {
            debug_id: Some(id.into()),
            ..self
        }
    }

    pub fn build(self, tree: &mut NodeTree, parent: NodeId) -> Result<NodeId, EngineError> {
        let parent_node = tree.get(parent)?;
        debug_assert!(
            !(parent_node.kind().is_leaf() || parent_node.layout_override().is_some()),
            "leaf nodes cannot have children: canvas/geometry/override nodes do not lay out \
             children, so the subtree would never produce layout boxes or pixels"
        );
        debug_assert!(
            self.editable.is_none() || self.bind_text.is_none(),
            "editable text nodes own their display mirror; bind_text would overwrite it"
        );
        let mut spec = NodeSpec::new(self.kind, self.style, self.bind_style);
        if let Some(measure) = self.measure {
            spec = spec.with_measure(measure);
        }
        if let Some(hit_shape) = self.hit_shape {
            spec = spec.with_hit_shape(hit_shape);
        }
        if let Some(layout_override) = self.layout_override {
            spec = spec.with_layout_override(layout_override);
        }
        if let Some(bind_text) = self.bind_text {
            spec = spec.with_bind_text(bind_text);
        }
        if let Some(editable) = self.editable {
            spec = spec.with_editable(editable);
        }
        if let Some(paint) = self.paint {
            spec = spec.with_paint(paint);
        }
        if let Some(debug_id) = self.debug_id {
            spec = spec.with_debug_id(debug_id);
        }
        if let Some(action) = self.action {
            spec = spec.with_action(action);
        }
        if let Some(drag) = self.drag {
            spec = spec.with_drag(drag);
        }
        if let Some(ring) = self.focus_ring {
            spec = spec.with_focus_ring(ring);
        }
        if let Some(a11y) = self.a11y {
            spec = spec.with_a11y(a11y);
        }
        if let Some(bind_a11y) = self.bind_a11y {
            spec = spec.with_bind_a11y(bind_a11y);
        }
        if !self.tab_stop {
            spec = spec.with_tab_stop(false);
        }
        if self.press_focuses_editable {
            spec = spec.with_press_focuses_editable();
        }
        tree.insert_child(parent, spec)
    }
}

#[cfg(test)]
mod tests {
    use super::NodeBuilder;
    use crate::{
        error::EngineError,
        layout::{LayoutOverride, hit_test::HitShape},
        math::{LogicalPoint, LogicalRect, px},
        node::{
            dirty::DirtyFlags,
            tree::{NodeKind, NodeTree},
        },
        paint::{
            canvas::Canvas,
            geometry::GeometryId,
            style::{Clip, Style, StylePatch},
        },
    };

    #[test]
    fn build_boxed_sets_kind_and_style() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let style = Style::default();

        let node = NodeBuilder::boxed()
            .style(style.clone())
            .build(&mut tree, root)
            .expect("挂载成功");

        assert_eq!(tree.get(node).expect("存在").kind(), &NodeKind::Box);
        assert_eq!(tree.get(node).expect("存在").style(), &style);
        assert_eq!(
            tree.get(node).expect("存在").style_patch(),
            &StylePatch::default()
        );
    }

    #[test]
    fn build_canvas_and_geometry_kinds() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let bounds = LogicalRect::new(px(1.0), px(2.0), px(30.0), px(40.0));

        let canvas = NodeBuilder::canvas(Canvas::new(bounds))
            .build(&mut tree, root)
            .expect("挂载成功");
        let geometry = NodeBuilder::geometry(GeometryId::for_test(2, 5))
            .build(&mut tree, root)
            .expect("挂载成功");

        match tree.get(canvas).expect("存在").kind() {
            NodeKind::Canvas(output) => assert_eq!(output.bounds(), bounds),
            other => panic!("expected Canvas, got {other:?}"),
        }
        match tree.get(geometry).expect("存在").kind() {
            NodeKind::Geometry(id) => assert_eq!(*id, GeometryId::for_test(2, 5)),
            other => panic!("expected Geometry, got {other:?}"),
        }
        assert!(tree.get(canvas).expect("存在").kind().is_leaf());
        assert!(tree.get(geometry).expect("存在").kind().is_leaf());
    }

    #[test]
    fn build_image_sets_kind_and_needs_measure() {
        use crate::{
            node::image::{ImageFit, ImageSource},
            resources::ResourceId,
        };

        let mut tree = NodeTree::new();
        let root = tree.root();
        let resource = ResourceId::for_test(3, 2);
        let node = NodeBuilder::image(resource, ImageFit::Cover)
            .image_modulation(crate::paint::color::Color::BLACK)
            .build(&mut tree, root)
            .expect("挂载成功");

        let target = tree.get(node).expect("存在");
        match target.kind() {
            NodeKind::Image(content) => {
                assert_eq!(content.source(), ImageSource::Ready(resource));
                assert_eq!(content.fit(), ImageFit::Cover);
                assert_eq!(content.modulation(), crate::paint::color::Color::BLACK);
                assert_eq!(
                    content.intrinsic(),
                    None,
                    "内在尺寸由 App::refresh_images 在布局前解析"
                );
            }
            other => panic!("expected Image, got {other:?}"),
        }
        assert!(target.kind().is_leaf(), "图片是叶子节点");
        assert!(
            tree.measure_of(node).is_none(),
            "builder 不伪造与资源不一致的尺寸"
        );
    }

    #[test]
    fn build_image_pending_stores_the_pending_handle() {
        use crate::{
            node::image::{ImageFit, ImageSource},
            resources::PendingResource,
        };

        let mut tree = NodeTree::new();
        let root = tree.root();
        let pending = PendingResource::for_test(7, 4);
        let node = NodeBuilder::image_pending(pending, ImageFit::Contain)
            .build(&mut tree, root)
            .expect("挂载成功");

        match tree.get(node).expect("存在").kind() {
            NodeKind::Image(content) => {
                assert_eq!(content.source(), ImageSource::Pending(pending));
                assert_eq!(content.fit(), ImageFit::Contain);
                assert_eq!(content.source().resource(), None, "Pending 没有就绪句柄");
            }
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[test]
    fn image_modulation_is_ignored_on_non_image_nodes() {
        use crate::paint::color::Color;

        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed()
            .image_modulation(Color::BLACK)
            .build(&mut tree, root)
            .expect("挂载成功");
        assert_eq!(tree.get(node).expect("存在").kind(), &NodeKind::Box);
    }

    #[test]
    fn build_stale_parent_is_error() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let doomed = NodeBuilder::boxed()
            .build(&mut tree, root)
            .expect("挂载成功");
        tree.remove(doomed).expect("卸载成功");
        let before = tree.len();

        let error = NodeBuilder::boxed()
            .build(&mut tree, doomed)
            .expect_err("陈旧父必须失败");

        assert!(matches!(
            error,
            EngineError::StaleNode { node } if node == doomed
        ));
        assert_eq!(tree.len(), before, "失败不得留下孤儿节点");
    }

    #[test]
    fn build_stores_bind_style() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed()
            .bind_style(|_cx| StylePatch {
                opacity: Some(0.5),
                ..StylePatch::default()
            })
            .build(&mut tree, root)
            .expect("挂载成功");

        assert!(
            tree.get(node)
                .expect("存在")
                .dirty()
                .contains(DirtyFlags::PAINT),
            "带 bind_style 的节点必须待求值"
        );
        assert!(
            tree.take_bind_style(node).is_some(),
            "闭包必须存进了节点（求值面见 app.rs 的 refresh_styles 用例）"
        );
        assert_eq!(
            tree.get(node).expect("存在").style_patch(),
            &StylePatch::default(),
            "build 不预求值"
        );
    }

    #[test]
    fn builder_default_is_box() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::default()
            .build(&mut tree, root)
            .expect("挂载成功");

        assert_eq!(tree.get(node).expect("存在").kind(), &NodeKind::Box);
        assert_eq!(
            tree.get(node).expect("存在").style(),
            &Style::default(),
            "default == boxed()"
        );
    }

    #[test]
    fn layout_override_forces_absolute_and_stores_rect() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let rect = LogicalRect::new(px(1.0), px(2.0), px(3.0), px(4.0));

        let node = NodeBuilder::canvas(Canvas::new(LogicalRect::new(
            px(0.0),
            px(0.0),
            px(1.0),
            px(1.0),
        )))
        .layout_override(rect)
        .build(&mut tree, root)
        .expect("挂载成功");

        let node = tree.get(node).expect("存在");
        assert_eq!(
            node.style().inner().position,
            taffy::style::Position::Absolute,
            "override 语义要求出流"
        );
        assert_eq!(node.layout_override(), Some(LayoutOverride::new(rect)));
        assert_eq!(node.style().clip, Clip::None);
        let _ = LogicalPoint::ZERO;
    }

    #[test]
    fn measure_and_hit_shape_are_stored() {
        use std::cell::Cell;

        let mut tree = NodeTree::new();
        let root = tree.root();
        let calls = std::rc::Rc::new(Cell::new(0usize));
        let counter = calls;

        let node = NodeBuilder::boxed()
            .measure(Box::new(move |_known, _available| {
                counter.set(counter.get() + 1);
                crate::math::LogicalSize::new(px(10.0), px(10.0))
            }))
            .hit_shape(HitShape::custom(|point: LogicalPoint| point.x < px(5.0)))
            .build(&mut tree, root)
            .expect("挂载成功");

        assert!(tree.measure_of(node).is_some(), "measure 必须存进节点");
        let shape = tree
            .get(node)
            .expect("存在")
            .hit_shape()
            .expect("hit_shape 必须存进节点");
        assert!(shape.contains(LogicalPoint::new(px(1.0), px(1.0))));
        assert!(!shape.contains(LogicalPoint::new(px(6.0), px(1.0))));
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "leaf nodes cannot have children")]
    fn leaf_parent_is_debug_assert() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let canvas = NodeBuilder::canvas(Canvas::new(LogicalRect::new(
            px(0.0),
            px(0.0),
            px(4.0),
            px(4.0),
        )))
        .build(&mut tree, root)
        .expect("挂载成功");

        let _ = NodeBuilder::boxed().build(&mut tree, canvas);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "leaf nodes cannot have children")]
    fn override_parent_is_debug_assert() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let pane = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(0.0), px(0.0), px(10.0), px(10.0)))
            .build(&mut tree, root)
            .expect("挂载成功");

        let _ = NodeBuilder::boxed().build(&mut tree, pane);
    }

    #[test]
    fn text_builder_sets_kind_content_and_no_measure() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::text("label", crate::text::TextStyle::new(px(13.0)))
            .build(&mut tree, root)
            .expect("挂载成功");

        match tree.get(node).expect("存在").kind() {
            NodeKind::Text(content) => {
                assert_eq!(content.text, "label");
                assert_eq!(content.style.size, px(13.0));
            }
            other => panic!("expected Text, got {other:?}"),
        }
        assert!(
            tree.measure_of(node).is_none(),
            "text() 不伪造 measure：首次形状由 App::refresh_text 注入"
        );
        assert_eq!(
            tree.text_measured_size(node),
            Some(crate::math::LogicalSize::ZERO),
            "未 shape 前的尺寸记录是 0"
        );
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "leaf nodes cannot have children")]
    fn text_parent_is_debug_assert() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let text = NodeBuilder::text("x", crate::text::TextStyle::default())
            .build(&mut tree, root)
            .expect("挂载成功");

        let _ = NodeBuilder::boxed().build(&mut tree, text);
    }

    #[test]
    fn bind_text_keeps_the_text_kind_and_marks_paint() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::text("", crate::text::TextStyle::default())
            .bind_text(|_cx| smol_str::SmolStr::new_static("dynamic"))
            .build(&mut tree, root)
            .expect("挂载成功");

        assert!(tree.get(node).expect("存在").kind().is_leaf());
        assert!(
            tree.take_bind_text(node).is_some(),
            "bind_text 必须存进节点"
        );
        assert!(
            tree.get(node)
                .expect("存在")
                .dirty()
                .contains(DirtyFlags::PAINT),
            "带 bind_text 的节点必须待求值"
        );
    }
}
