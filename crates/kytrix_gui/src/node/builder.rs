//! `NodeBuilder`：声明式建树的唯一入口。
//!
//! 设计见 `design/phase-03-node-state.md` §12 与 `design/phase-04-layout.md` §10。
//! 一个 builder 实例 = 一次挂载（不是可复用的模板）；`build` 时把 6 个部件装成 `NodeSpec` 交给
//! `NodeTree::insert_child`（`NodeSpec` 的字段私有 —— 见其 rustdoc）。

use smol_str::SmolStr;

use crate::{
    error::EngineError,
    layout::{LayoutOverride, MeasureFn},
    math::{Point, Rect},
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
};

/// 声明式建树的唯一入口。**只在挂载时使用一次**。
///
/// **不 `derive(Default)`**（B3）：默认值必须与 [`NodeBuilder::boxed`] 完全一致，手写 `impl Default`
/// 表达这一点，也避免 `NodeKind` 被迫实现 `Default`。
pub struct NodeBuilder {
    kind: NodeKind,
    style: Style,
    bind_style: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>>,
    measure: Option<MeasureFn>,
    hit_shape: Option<Box<dyn Fn(Point) -> bool>>,
    layout_override: Option<LayoutOverride>,
    bind_text: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> SmolStr>>,
    paint: Option<PaintSpec>,
    debug_id: Option<SmolStr>,
}

impl Default for NodeBuilder {
    fn default() -> Self {
        Self::boxed()
    }
}

impl NodeBuilder {
    /// 普通容器/叶子（`NodeKind::Box`，默认 `Style`）。
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
            paint: None,
            debug_id: None,
        }
    }

    /// 自绘节点：挂载时提交画布内容（P02 的 `Canvas`）。
    #[must_use]
    pub fn canvas(canvas: Canvas) -> Self {
        Self {
            kind: NodeKind::Canvas(canvas),
            ..Self::boxed()
        }
    }

    /// 静态几何节点（引用 P09 的 GPU 几何）。
    #[must_use]
    pub fn geometry(geometry: GeometryId) -> Self {
        Self {
            kind: NodeKind::Geometry(geometry),
            ..Self::boxed()
        }
    }

    /// 文本叶子（静态文本）。
    ///
    /// 挂载后由 `App::refresh_text` 首次形状 —— **在此之前 measure 是 0×0**
    /// （`NodeBuilder::text` 不伪造与真实形状不一致的闭包）。
    #[must_use]
    pub fn text(text: impl Into<SmolStr>, style: TextStyle) -> Self {
        Self {
            kind: NodeKind::Text(TextContent::new(text, style)),
            ..Self::boxed()
        }
    }

    /// 行内盒（`<img>` / `kbd`）：参与断行与行高，位置由引擎算（§12）。
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

    /// 动态文本：每次 `refresh_styles` 求值（与 `bind_style` 同一机制、同一依赖图）。
    ///
    /// 返回值是 `SmolStr`（不是 `String`）：短字符串零分配（AGENTS §2.2）。
    #[must_use]
    pub fn bind_text(self, bind_text: impl Fn(&mut ReadCtx<'_>) -> SmolStr + 'static) -> Self {
        Self {
            bind_text: Some(Box::new(bind_text)),
            ..self
        }
    }

    /// 静态样式（覆盖 `Style::default()`）。
    #[must_use]
    pub fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    /// 动态属性：闭包在每次 `refresh_styles` 求值时重算 `StylePatch`（值语义，不累积）。
    #[must_use]
    pub fn bind_style(self, bind_style: impl Fn(&mut ReadCtx<'_>) -> StylePatch + 'static) -> Self {
        Self {
            bind_style: Some(Box::new(bind_style)),
            ..self
        }
    }

    /// 自定义叶子测量（P05 的文本 / 坐标标签入口）。
    ///
    /// 闭包必须是**纯函数**：不读 `Signal`、不写状态（见 `crate::layout::MeasureFn` 的契约）。
    #[must_use]
    pub fn measure(mut self, measure: MeasureFn) -> Self {
        self.measure = Some(measure);
        self
    }

    /// 自定义命中形状：`point` 是节点 **border box 局部坐标**，返回 `false` = 该点穿透到下层。
    ///
    /// 纯函数契约同 [`NodeBuilder::measure`]；默认（不设置）用布局盒矩形判定。
    #[must_use]
    pub fn hit_shape(mut self, hit_shape: impl Fn(Point) -> bool + 'static) -> Self {
        self.hit_shape = Some(Box::new(hit_shape));
        self
    }

    /// 布局覆盖（root 逃生口，P04 §10）：`rect` 相对**父节点 border box**，且就是该节点的 border box。
    ///
    /// **副作用**：同时把 `style.inner.position` 置为 `Position::Absolute` —— 出流是「位置与尺寸都由应用定」
    /// 的必要条件（父不该为它保留流内空间）。
    ///
    /// 该节点因此是**布局叶子**：挂在它下面的子节点不会被布局（`build` 有 debug 断言）。
    /// 若容器还需要子布局，请用普通容器 + `Style` 的尺寸，而不是 Override。
    #[must_use]
    pub fn layout_override(mut self, rect: Rect) -> Self {
        self.style.inner.position = taffy::style::Position::Absolute;
        self.layout_override = Some(LayoutOverride::new(rect));
        self
    }

    /// 静态绘制属性（P06）：背景 / 边框 / 圆角 / 阴影 / 渐变。动态覆盖请用 [`NodeBuilder::bind_style`]。
    #[must_use]
    pub fn paint(self, paint: PaintSpec) -> Self {
        Self {
            paint: Some(paint),
            ..self
        }
    }

    /// 稳定调试 id（P06）：`Probe::by_selector(Selector::Id(..))` 与诊断日志的锚点。
    ///
    /// 同一棵树内**允许重复**（多个命中取 `order()` 的第一个）；引擎不做唯一性检查。
    #[must_use]
    pub fn id(self, id: impl Into<SmolStr>) -> Self {
        Self {
            debug_id: Some(id.into()),
            ..self
        }
    }

    /// 挂载到 `parent` 末尾并返回新句柄。
    ///
    /// # Errors
    /// `EngineError::StaleNode { node: parent }`：父节点陈旧（从未存在/已卸载）。
    ///
    /// # Panics（仅 debug）
    /// 父节点是**布局叶子**（`NodeKind::Canvas` / `Geometry`，或带 `LayoutOverride`）时 `debug_assert!` 失败。
    /// **release 下允许挂载，但该子树永不产生布局盒、也永不产生任何像素**（P04 对叶子节点的子节点不做布局）
    /// ——这是「已定义但不可见」的行为，不是 UB，也不是静默降级（debug 构建必然失败）。
    pub fn build(self, tree: &mut NodeTree, parent: NodeId) -> Result<NodeId, EngineError> {
        let parent_node = tree.get(parent)?;
        debug_assert!(
            !(parent_node.kind().is_leaf() || parent_node.layout_override().is_some()),
            "leaf nodes cannot have children: canvas/geometry/override nodes do not lay out \
             children, so the subtree would never produce layout boxes or pixels"
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
        if let Some(paint) = self.paint {
            spec = spec.with_paint(paint);
        }
        if let Some(debug_id) = self.debug_id {
            spec = spec.with_debug_id(debug_id);
        }
        tree.insert_child(parent, spec)
    }
}

#[cfg(test)]
mod tests {
    use super::NodeBuilder;
    use crate::{
        error::EngineError,
        layout::LayoutOverride,
        math::{Point, Rect},
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
        let bounds = Rect::new(1.0, 2.0, 30.0, 40.0);

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
    fn build_stale_parent_is_error() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let doomed = NodeBuilder::boxed()
            .build(&mut tree, root)
            .expect("挂载成功");
        tree.remove(doomed).expect("卸载成功");
        let before = tree.order().len();

        let error = NodeBuilder::boxed()
            .build(&mut tree, doomed)
            .expect_err("陈旧父必须失败");

        assert!(matches!(
            error,
            EngineError::StaleNode { node } if node == doomed
        ));
        assert_eq!(tree.order().len(), before, "失败不得留下孤儿节点");
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
        let rect = Rect::new(1.0, 2.0, 3.0, 4.0);

        let node = NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 0.0, 1.0, 1.0)))
            .layout_override(rect)
            .build(&mut tree, root)
            .expect("挂载成功");

        let node = tree.get(node).expect("存在");
        assert_eq!(
            node.style().inner.position,
            taffy::style::Position::Absolute,
            "override 语义要求出流"
        );
        assert_eq!(node.layout_override(), Some(LayoutOverride::new(rect)));
        assert_eq!(node.style().clip, Clip::None);
        let _ = Point::ZERO;
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
                crate::math::Size::new(10.0, 10.0)
            }))
            .hit_shape(|point: Point| point.x < 5.0)
            .build(&mut tree, root)
            .expect("挂载成功");

        assert!(tree.measure_of(node).is_some(), "measure 必须存进节点");
        let shape = tree
            .get(node)
            .expect("存在")
            .hit_shape()
            .expect("hit_shape 必须存进节点");
        assert!(shape(Point::new(1.0, 1.0)));
        assert!(!shape(Point::new(6.0, 1.0)));
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "leaf nodes cannot have children")]
    fn leaf_parent_is_debug_assert() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let canvas = NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 0.0, 4.0, 4.0)))
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
            .layout_override(Rect::new(0.0, 0.0, 10.0, 10.0))
            .build(&mut tree, root)
            .expect("挂载成功");

        let _ = NodeBuilder::boxed().build(&mut tree, pane);
    }

    // ── P05：文本 ────────────────────────────────────────────────────────────

    #[test]
    fn text_builder_sets_kind_content_and_no_measure() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::text("label", crate::text::TextStyle::new(13.0))
            .build(&mut tree, root)
            .expect("挂载成功");

        match tree.get(node).expect("存在").kind() {
            NodeKind::Text(content) => {
                assert_eq!(content.text, "label");
                assert_eq!(content.style.size, 13.0);
            }
            other => panic!("expected Text, got {other:?}"),
        }
        assert!(
            tree.measure_of(node).is_none(),
            "text() 不伪造 measure：首次形状由 App::refresh_text 注入"
        );
        assert_eq!(
            tree.text_measured_size(node),
            Some(crate::math::Size::ZERO),
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
