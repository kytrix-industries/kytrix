//! `NodeBuilder`：声明式建树的唯一入口。
//!
//! 设计见 `design/phase-03-node-state.md` §12。一个 builder 实例 = 一次挂载（不是可复用的模板）。

use crate::{
    error::EngineError,
    node::{
        dependency::ReadCtx,
        id::NodeId,
        tree::{NodeKind, NodeTree},
    },
    paint::{
        canvas::Canvas,
        geometry::GeometryId,
        style::{Style, StylePatch},
    },
};

/// 声明式建树的唯一入口。**只在挂载时使用一次**。
///
/// **不 `derive(Default)`**（B3）：默认值必须与 [`NodeBuilder::boxed`] 完全一致，手写 `impl Default`
/// 表达这一点，也避免 `NodeKind` 被迫实现 `Default`。
pub struct NodeBuilder {
    kind: NodeKind,
    style: Style,
    bind_style: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>>,
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
        }
    }

    /// 自绘节点：挂载时提交画布内容（P02 的 `Canvas`）。
    #[must_use]
    pub fn canvas(canvas: Canvas) -> Self {
        Self {
            kind: NodeKind::Canvas(canvas),
            style: Style::default(),
            bind_style: None,
        }
    }

    /// 静态几何节点（引用 P09 的 GPU 几何）。
    #[must_use]
    pub fn geometry(geometry: GeometryId) -> Self {
        Self {
            kind: NodeKind::Geometry(geometry),
            style: Style::default(),
            bind_style: None,
        }
    }

    /// 静态样式（覆盖 `Style::default()`）。
    #[must_use]
    pub fn style(self, style: Style) -> Self {
        Self { style, ..self }
    }

    /// 动态属性：闭包在每次 `refresh_styles` 求值时重算 `StylePatch`（值语义，不累积）。
    #[must_use]
    pub fn bind_style(self, bind_style: impl Fn(&mut ReadCtx<'_>) -> StylePatch + 'static) -> Self {
        Self {
            bind_style: Some(Box::new(bind_style)),
            ..self
        }
    }

    /// 挂载到 `parent` 末尾并返回新句柄。
    ///
    /// # Errors
    /// `EngineError::StaleNode { node: parent }`：父节点陈旧（从未存在/已卸载）。
    ///
    /// # Panics（仅 debug）
    /// 父节点是叶子（`NodeKind::Canvas` / `Geometry`）时 `debug_assert!` 失败。**release 下允许挂载，
    /// 但该子树永不产生布局盒、也永不产生任何像素**（P04 对叶子节点的子节点不做布局）——这是「已定义
    /// 但不可见」的行为，不是 UB，也不是静默降级（debug 构建必然失败）。
    pub fn build(self, tree: &mut NodeTree, parent: NodeId) -> Result<NodeId, EngineError> {
        let parent_node = tree.get(parent)?;
        debug_assert!(
            !parent_node.kind().is_leaf(),
            "leaf nodes cannot have children: canvas/geometry nodes do not lay out children, \
             so the subtree would never produce layout boxes or pixels"
        );
        tree.insert_child(parent, self.kind, self.style, self.bind_style)
    }
}

#[cfg(test)]
mod tests {
    use super::NodeBuilder;
    use crate::{
        error::EngineError,
        math::Rect,
        node::{
            dirty::DirtyFlags,
            tree::{NodeKind, NodeTree},
        },
        paint::{
            canvas::Canvas,
            geometry::GeometryId,
            style::{Style, StylePatch},
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
}
