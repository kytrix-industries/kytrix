//! 稳定元素探针（L1）：基于**持久节点树**直接查布局盒，不依赖绘制动作。
//!
//! 设计见 `design/phase-06-paint-runtime.md` §10.2；`Selector` 只用强类型变体，不用字符串 CSS 选择器。
//! `Role(A11yRole)` 推迟 P14（F-63）。

use crate::{
    layout::hit_test::HitResult,
    math::{Point, Rect},
    node::{
        id::NodeId,
        tree::{NodeKind, NodeTree},
    },
    paint::scene::LaneId,
};

/// 元素选择器（强类型；同一选择器多个命中时取 `tree.order()` 的第一个）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Selector {
    /// 稳定调试 id（`NodeBuilder::id`）。
    Id(&'static str),
    /// 文本节点的**精确**源文本（`TextContent.text`）。
    Text(&'static str),
    /// 从窗口根向下的 id 路径（每段匹配该层的 `debug_id`）。
    Path(&'static [&'static str]),
}

/// 节点树的只读探针。
pub struct Probe<'a> {
    tree: &'a NodeTree,
}

impl<'a> Probe<'a> {
    /// 从窗口的 `tree()` 构造（消费者：`TestApp::probe`）。
    #[must_use]
    pub fn new(tree: &'a NodeTree) -> Self {
        Self { tree }
    }

    /// 解析选择器（歧义时取 DFS 先序的第一个）。
    #[must_use]
    pub fn by_selector(&self, selector: Selector) -> Option<NodeId> {
        match selector {
            Selector::Id(id) => self
                .tree
                .order()
                .iter()
                .copied()
                .find(|node| self.tree.debug_id(*node) == Some(id)),
            Selector::Text(text) => self.tree.order().iter().copied().find(|node| {
                self.tree
                    .get(*node)
                    .is_ok_and(|target| matches!(target.kind(), NodeKind::Text(content) if content.text == text))
            }),
            Selector::Path(segments) => {
                let mut current = self.tree.root();
                for segment in segments {
                    let children = self
                        .tree
                        .get(current)
                        .map(|target| target.children().to_vec())
                        .unwrap_or_default();
                    current = children
                        .into_iter()
                        .find(|child| self.tree.debug_id(*child) == Some(*segment))?;
                }
                Some(current)
            }
        }
    }

    /// 选择器是否存在。
    #[must_use]
    pub fn exists(&self, selector: Selector) -> bool {
        self.by_selector(selector).is_some()
    }

    /// 选择器的命中数。
    #[must_use]
    pub fn count(&self, selector: Selector) -> usize {
        match selector {
            Selector::Id(id) => self
                .tree
                .order()
                .iter()
                .filter(|node| self.tree.debug_id(**node) == Some(id))
                .count(),
            Selector::Text(text) => self
                .tree
                .order()
                .iter()
                .filter(|node| {
                    self.tree.get(**node).is_ok_and(|target| {
                        matches!(target.kind(), NodeKind::Text(content) if content.text == text)
                    })
                })
                .count(),
            Selector::Path(_) => usize::from(self.exists(selector)),
        }
    }

    /// 节点的逻辑像素布局盒（I11：**只有逻辑像素**）。
    #[must_use]
    pub fn bounds(&self, node: NodeId) -> Option<Rect> {
        self.tree
            .get(node)
            .ok()
            .map(|target| target.layout().bounds)
    }

    /// 命中测试（转调 `NodeTree::hit_test`）。
    #[must_use]
    pub fn hit(&self, point: Point) -> Option<HitResult> {
        self.tree.hit_test(point)
    }

    /// 节点的绘制桶。
    #[must_use]
    pub fn lane_of(&self, node: NodeId) -> Option<LaneId> {
        self.tree.lane_of(node)
    }
}
