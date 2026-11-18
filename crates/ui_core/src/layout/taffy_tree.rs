//! 为 `NodeTree` 实现 taffy 0.15.0 的低级 trait（+ 布局运行入口）。
//!
//! 事实基线：`E:\.cargo\registry\...\taffy-0.15.0\src\`（`tree/traits.rs`、`compute/{mod,leaf,oof}.rs`）。
//! 设计见 `design/phase-04-layout.md` §5。
//!
//! **本文件的 impl 都是 trait impl**，不新增 inherent `impl NodeTree` 块
//! （`clippy::multiple_inherent_impl = "forbid"` + AGENTS §1.3「严禁拆分多个 impl 到不同文件」）；
//! 需要 `&mut NodeTree` 的算法逻辑一律写成**自由函数**（`run` / `dispatch_child_layout`）。

use smallvec::SmallVec;
use smol_str::SmolStr;
use taffy::{
    AvailableSpace, BlockContext, CacheTree, Display, Layout, LayoutBlockContainer,
    LayoutContainingBlock, LayoutFlexboxContainer, LayoutGridContainer, LayoutInput, LayoutOutput,
    LayoutPartialTree, NodeId as TaffyNode, Size as TaffySize, Style, TraversePartialTree,
    compute_block_layout, compute_cached_layout, compute_flexbox_layout, compute_grid_layout,
    compute_hidden_layout, compute_oof_layout, compute_root_layout,
};

use crate::{
    error::EngineError,
    layout::{LayoutReport, measure},
    node::{id::NodeId, tree::NodeTree},
};

/// 引擎 `NodeId` → taffy `NodeId`。
///
/// 编码复用 P03 的 `NodeId::to_bits()`：`generation << 32 | index`，双向无损
/// （`taffy::NodeId` 是 `u64` 的 newtype，`new`/`From<u64>`，见 `tree/node.rs:13-32`）。
fn to_taffy(node: NodeId) -> TaffyNode {
    TaffyNode::new(node.to_bits())
}

/// 迭代器元素版本（`ChildIter` 的元素类型需要 `fn(&NodeId) -> TaffyNode`）。
fn to_taffy_ref(node: &NodeId) -> TaffyNode {
    TaffyNode::new(node.to_bits())
}

/// taffy `NodeId` → 引擎 `NodeId`；非法编码回落根（`debug_assert`，release 安全）。
///
/// **必须补上树的窗口 tag**：taffy 只携带 64 位的 tree-local 编码（`(gen << 32) | index`），
/// 直接 `NodeId::from_bits` 得到的是"未绑定"句柄（tag 0）。未绑定句柄在 `arena.get` 里是通配的
/// （查得到），但作为 `HashMap` 的键与 arena 铸出的带 tag 句柄**不相等** —— `layout_cache` /
/// `hoisted` / `geometry_transforms` 的失效会因此漏清（wave-6 的实测回归：文本 measure 缓存
/// 清不掉，根 cache 命中 ⇒ 文本改动不重排）。
fn to_engine(tree: &NodeTree, node: TaffyNode) -> NodeId {
    let tag = tree.window_tag();
    NodeId::from_bits(u64::from(node))
        .map(|engine| engine.with_window_tag(tag))
        .unwrap_or_else(|| {
            debug_assert!(
                false,
                "taffy handed back a NodeId that the engine never issued"
            );
            tree.root()
        })
}

/// 一次布局运行：**不**做圆整（P01 §7.5 / I11；引擎永不调用 taffy 的圆整入口）。
pub(crate) fn run(
    tree: &mut NodeTree,
    available: taffy::Size<Option<f32>>,
) -> Result<LayoutReport, EngineError> {
    tree.begin_layout();
    let root = to_taffy(tree.root());
    let available_space = TaffySize {
        width: available
            .width
            .map_or(AvailableSpace::MaxContent, AvailableSpace::Definite),
        height: available
            .height
            .map_or(AvailableSpace::MaxContent, AvailableSpace::Definite),
    };

    // 低级入口：内部只写 `set_unrounded_layout`，不做任何圆整。
    compute_root_layout(tree, root, available_space);

    // R04-10：错误帧既**不推进绝对坐标**，也不消费 `LAYOUT`（见 `set_local_layout` 的同款守卫）。
    //
    // 旧实现在 `absolutize()` 之后才 `take_invalid_measure()`，此时整轮已经跑完、`LAYOUT` 已被
    // 清掉：应用修好 measure 闭包后没有任何失效源（`layout_if_dirty` 只看 `LAYOUT`），
    // 该子树永远停在错误帧的盒子上。这里把「出错节点 → 根」的 taffy 缓存清掉并重新标 `LAYOUT`，
    // 于是修复本身（闭包返回合法值）就足以让下一帧自愈，不需要外部 `mark_layout`。
    // R04-10：错误帧既**不推进绝对坐标**，也不消费 `LAYOUT`。旧实现在 `absolutize()` 之后才
    // `take_invalid_measure()`，此时整轮已跑完、`LAYOUT` 已被清掉：应用修好 measure 闭包后
    // 没有任何失效源（`layout_if_dirty` 只看 `LAYOUT`），该子树永远停在错误帧的盒子上。
    // 这里把「出错节点 → 根」的 taffy 缓存清掉并重新标 `LAYOUT`，于是修复本身（闭包返回合法值）
    // 就足以让下一帧自愈，不需要外部 `mark_layout`。
    if let Some(error) = tree.take_invalid_measure() {
        let node = match &error {
            EngineError::InvalidMeasure { node, .. } => *node,
            // `NodeTree::invalid_measure` 的唯一写者是 `note_invalid_measure`（只装这一种错误）。
            _ => tree.root(),
        };
        tree.invalidate_layout(node);
        return Err(error);
    }
    // 引擎 post-pass：父相对 → 窗口绝对（含出流/hoisted 子树）。
    tree.absolutize();

    Ok(tree.take_report())
}

/// `LayoutPartialTree::compute_child_layout` + `LayoutBlockContainer::compute_block_child_layout`
/// 的统一实现（与 taffy `TaffyView::compute_child_layout` 的结构逐字对应）。
fn dispatch_child_layout(
    tree: &mut NodeTree,
    taffy_node: TaffyNode,
    inputs: LayoutInput,
    block_ctx: Option<&mut BlockContext<'_>>,
) -> LayoutOutput {
    // 祖先 `display:none` ⇒ 隐藏布局，不进 cache（taffy 的固定入口顺序）。
    if inputs.run_mode == taffy::RunMode::PerformHiddenLayout {
        return compute_hidden_layout(tree, taffy_node);
    }

    compute_cached_layout(tree, taffy_node, inputs, |tree, taffy_node, inputs| {
        let node = to_engine(tree, taffy_node);
        if inputs.run_mode == taffy::RunMode::PerformLayout {
            // 只有完整布局且 cache 未命中的节点计入 `laid_out`（「只重算脏子树」的度量）。
            tree.stats_mut().note_laid_out();
        }
        let display = tree.taffy_style(node).display;
        let has_children = !tree.taffy_children(node).is_empty();

        let mut output = if matches!(display, Display::None) {
            // N3：隐藏优先于 override —— 隐藏的覆盖节点不得有非零盒。
            compute_hidden_layout(tree, taffy_node)
        } else if let Some(overridden) = tree.layout_override_of(node) {
            // root 逃生口：尺寸由应用给定，位置由 post-pass 落（`NodeTree::absolutize`）。
            LayoutOutput::from_outer_size(TaffySize {
                width: overridden.rect.size.width,
                height: overridden.rect.size.height,
            })
        } else {
            match (display, has_children) {
                // N2：`FlowRoot` 不透传 `block_ctx`（与 taffy `TaffyView` 的
                // `(Display::FlowRoot, true) => compute_block_layout(.., None)` 一致）。
                (Display::Block, true) => compute_block_layout(tree, taffy_node, inputs, block_ctx),
                (Display::FlowRoot, true) => compute_block_layout(tree, taffy_node, inputs, None),
                (Display::Flex, true) => compute_flexbox_layout(tree, taffy_node, inputs),
                (Display::Grid, true) => compute_grid_layout(tree, taffy_node, inputs),
                // 仅 `Display::None` 能走到这里（上面已提前返回），保守按隐藏处理。
                (_, true) => compute_hidden_layout(tree, taffy_node),
                (_, false) => measure::compute_leaf(tree, node, inputs),
            }
        };

        // 出流盒的定位 pass 只在完整布局里跑（measure pass 写 hoisted 布局，而「最终布局 cache 命中」
        // 时不会重写它们 —— taffy `tree/taffy_tree.rs:337` 的同一理由）。
        if inputs.run_mode == taffy::RunMode::PerformLayout {
            compute_oof_layout(tree, taffy_node, &mut output);
        }
        output
    })
}

impl TraversePartialTree for NodeTree {
    type ChildIter<'a>
        = std::iter::Map<std::slice::Iter<'a, NodeId>, fn(&NodeId) -> TaffyNode>
    where
        Self: 'a;

    fn child_ids(&self, parent_node_id: TaffyNode) -> Self::ChildIter<'_> {
        self.taffy_children(to_engine(self, parent_node_id))
            .iter()
            .map(to_taffy_ref)
    }

    fn child_count(&self, parent_node_id: TaffyNode) -> usize {
        self.taffy_children(to_engine(self, parent_node_id)).len()
    }

    fn get_child_id(&self, parent_node_id: TaffyNode, child_index: usize) -> TaffyNode {
        let parent = to_engine(self, parent_node_id);
        match self.taffy_children(parent).get(child_index) {
            Some(child) => to_taffy(*child),
            None => {
                debug_assert!(false, "taffy asked for an out-of-range child index");
                to_taffy(self.root())
            }
        }
    }
}

impl LayoutPartialTree for NodeTree {
    type CoreContainerStyle<'a>
        = &'a Style<SmolStr>
    where
        Self: 'a;
    type CustomIdent = SmolStr;

    fn get_core_container_style(&self, node_id: TaffyNode) -> Self::CoreContainerStyle<'_> {
        self.taffy_style(to_engine(self, node_id))
    }

    fn set_unrounded_layout(&mut self, node_id: TaffyNode, layout: &Layout) {
        let node = to_engine(self, node_id);
        self.set_local_layout(node, layout);
    }

    fn compute_child_layout(&mut self, node_id: TaffyNode, inputs: LayoutInput) -> LayoutOutput {
        dispatch_child_layout(self, node_id, inputs, None)
    }
}

impl LayoutContainingBlock for NodeTree {
    type OofItemStyle<'a>
        = &'a Style<SmolStr>
    where
        Self: 'a;

    fn get_oof_item_style(&self, node_id: TaffyNode) -> Self::OofItemStyle<'_> {
        self.taffy_style(to_engine(self, node_id))
    }

    fn clear_hoisted_children(&mut self, node_id: TaffyNode) {
        let node = to_engine(self, node_id);
        self.hoisted_clear(node);
    }

    fn add_hoisted_children(&mut self, node_id: TaffyNode, hoisted: &[TaffyNode]) {
        let node = to_engine(self, node_id);
        let mapped: SmallVec<[NodeId; 2]> = hoisted.iter().map(|id| to_engine(self, *id)).collect();
        self.hoisted_push(node, &mapped);
    }
}

impl CacheTree for NodeTree {
    fn cache_get(&mut self, node_id: TaffyNode, input: &LayoutInput) -> Option<LayoutOutput> {
        let node = to_engine(self, node_id);
        let hit = self.taffy_cache(node).get(input);
        if hit.is_some() {
            self.stats_mut().note_cache_hit();
        } else {
            self.stats_mut().note_cache_miss();
        }
        hit
    }

    fn cache_store(
        &mut self,
        node_id: TaffyNode,
        input: &LayoutInput,
        layout_output: LayoutOutput,
    ) {
        let node = to_engine(self, node_id);
        self.taffy_cache(node).store(input, layout_output);
    }

    fn cache_clear(&mut self, node_id: TaffyNode) {
        let node = to_engine(self, node_id);
        self.taffy_cache(node).clear();
    }
}

impl LayoutFlexboxContainer for NodeTree {
    type FlexboxContainerStyle<'a>
        = &'a Style<SmolStr>
    where
        Self: 'a;
    type FlexboxItemStyle<'a>
        = &'a Style<SmolStr>
    where
        Self: 'a;

    fn get_flexbox_container_style(&self, node_id: TaffyNode) -> Self::FlexboxContainerStyle<'_> {
        self.taffy_style(to_engine(self, node_id))
    }

    fn get_flexbox_child_style(&self, child_node_id: TaffyNode) -> Self::FlexboxItemStyle<'_> {
        self.taffy_style(to_engine(self, child_node_id))
    }
}

impl LayoutGridContainer for NodeTree {
    type GridContainerStyle<'a>
        = &'a Style<SmolStr>
    where
        Self: 'a;
    type GridItemStyle<'a>
        = &'a Style<SmolStr>
    where
        Self: 'a;

    fn get_grid_container_style(&self, node_id: TaffyNode) -> Self::GridContainerStyle<'_> {
        self.taffy_style(to_engine(self, node_id))
    }

    fn get_grid_child_style(&self, child_node_id: TaffyNode) -> Self::GridItemStyle<'_> {
        self.taffy_style(to_engine(self, child_node_id))
    }
}

impl LayoutBlockContainer for NodeTree {
    type BlockContainerStyle<'a>
        = &'a Style<SmolStr>
    where
        Self: 'a;
    type BlockItemStyle<'a>
        = &'a Style<SmolStr>
    where
        Self: 'a;

    fn get_block_container_style(&self, node_id: TaffyNode) -> Self::BlockContainerStyle<'_> {
        self.taffy_style(to_engine(self, node_id))
    }

    fn get_block_child_style(&self, child_node_id: TaffyNode) -> Self::BlockItemStyle<'_> {
        self.taffy_style(to_engine(self, child_node_id))
    }

    fn compute_block_child_layout(
        &mut self,
        node_id: TaffyNode,
        inputs: LayoutInput,
        block_ctx: Option<&mut BlockContext<'_>>,
    ) -> LayoutOutput {
        dispatch_child_layout(self, node_id, inputs, block_ctx)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use taffy::style::{Dimension, Display, Position};

    use crate::{
        layout::{LayoutOverride, LayoutReport},
        math::{Rect, Size},
        node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
        paint::{
            canvas::Canvas,
            style::{Clip, Style},
        },
    };

    fn style_with(width: Dimension, height: Dimension) -> Style {
        let mut inner = taffy::Style::<smol_str::SmolStr>::DEFAULT;
        inner.size = taffy::geometry::Size { width, height };
        Style {
            inner,
            clip: Clip::None,
        }
    }

    fn fixed(width: f32, height: f32) -> Style {
        style_with(Dimension::length(width), Dimension::length(height))
    }

    fn add(tree: &mut NodeTree, parent: NodeId, style: Style) -> NodeId {
        NodeBuilder::boxed()
            .style(style)
            .build(tree, parent)
            .expect("挂载成功")
    }

    fn layout(tree: &mut NodeTree, width: f32, height: f32) -> LayoutReport {
        tree.compute_layout(taffy::geometry::Size {
            width: Some(width),
            height: Some(height),
        })
        .expect("布局成功")
    }

    fn bounds(tree: &NodeTree, node: NodeId) -> Rect {
        tree.get(node).expect("存在").layout().bounds
    }

    #[test]
    fn flex_row_places_children_left_to_right() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let a = add(&mut tree, root, fixed(10.0, 10.0));
        let b = add(&mut tree, root, fixed(20.0, 10.0));
        let c = add(&mut tree, root, fixed(30.0, 10.0));

        let report = layout(&mut tree, 200.0, 100.0);

        assert_eq!(bounds(&tree, a), Rect::new(0.0, 0.0, 10.0, 10.0));
        assert_eq!(bounds(&tree, b), Rect::new(10.0, 0.0, 20.0, 10.0));
        assert_eq!(bounds(&tree, c), Rect::new(30.0, 0.0, 30.0, 10.0));
        assert!(report.laid_out >= 4, "{report:?}");
    }

    #[test]
    fn flex_column_stacks_children() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let row = add(
            &mut tree,
            root,
            style_with(Dimension::percent(1.0), Dimension::percent(1.0)),
        );
        {
            let mut style = fixed(10.0, 10.0);
            style.inner.flex_direction = taffy::style::FlexDirection::Column;
            style.inner.size = taffy::geometry::Size {
                width: Dimension::length(100.0),
                height: Dimension::length(100.0),
            };
            let column = add(&mut tree, row, style);
            let first = add(&mut tree, column, fixed(10.0, 10.0));
            let second = add(&mut tree, column, fixed(10.0, 20.0));

            let _ = layout(&mut tree, 100.0, 100.0);

            assert_eq!(bounds(&tree, first).origin.y, 0.0);
            assert_eq!(bounds(&tree, second).origin.y, 10.0);
        }
    }

    #[test]
    fn grid_two_columns_place_items() {
        use taffy::prelude::FromLength;

        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut container = taffy::Style::<smol_str::SmolStr>::DEFAULT;
        container.display = Display::Grid;
        container.size = taffy::geometry::Size {
            width: Dimension::length(100.0),
            height: Dimension::length(100.0),
        };
        container.grid_template_columns = vec![
            taffy::style::GridTemplateComponent::Single(
                taffy::style::TrackSizingFunction::from_length(30.0_f32),
            ),
            taffy::style::GridTemplateComponent::Single(
                taffy::style::TrackSizingFunction::from_length(70.0_f32),
            ),
        ];
        let grid = add(
            &mut tree,
            root,
            Style {
                inner: container,
                clip: Clip::None,
            },
        );
        let left = add(&mut tree, grid, fixed(5.0, 5.0));
        let right = add(&mut tree, grid, fixed(5.0, 5.0));

        let _ = layout(&mut tree, 200.0, 200.0);

        assert_eq!(bounds(&tree, left).origin.x, 0.0);
        assert_eq!(bounds(&tree, right).origin.x, 30.0, "第二列起点");
    }

    #[test]
    fn block_stacks_children() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut container = taffy::Style::<smol_str::SmolStr>::DEFAULT;
        container.display = Display::Block;
        container.size = taffy::geometry::Size {
            width: Dimension::length(100.0),
            height: Dimension::length(100.0),
        };
        let block = add(
            &mut tree,
            root,
            Style {
                inner: container,
                clip: Clip::None,
            },
        );
        let first = add(&mut tree, block, fixed(10.0, 10.0));
        let second = add(&mut tree, block, fixed(10.0, 20.0));

        let _ = layout(&mut tree, 200.0, 200.0);

        assert_eq!(bounds(&tree, first).origin.y, 0.0);
        assert_eq!(bounds(&tree, second).origin.y, 10.0);
    }

    #[test]
    fn fractional_sizes_are_not_rounded() {
        // P01 §7.5：不做圆整 ⇒ 10.5 / 3.25 必须原样保留。
        let mut tree = NodeTree::new();
        let root = tree.root();
        let a = add(&mut tree, root, fixed(10.5, 3.25));

        let _ = layout(&mut tree, 100.0, 100.0);

        assert_eq!(bounds(&tree, a), Rect::new(0.0, 0.0, 10.5, 3.25));
    }

    #[test]
    fn padding_offsets_child_origin() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut outer = taffy::Style::<smol_str::SmolStr>::DEFAULT;
        outer.size = taffy::geometry::Size {
            width: Dimension::length(100.0),
            height: Dimension::length(100.0),
        };
        outer.padding = taffy::geometry::Rect {
            left: taffy::style::LengthPercentage::length(4.0),
            right: taffy::style::LengthPercentage::length(4.0),
            top: taffy::style::LengthPercentage::length(4.0),
            bottom: taffy::style::LengthPercentage::length(4.0),
        };
        let outer = add(
            &mut tree,
            root,
            Style {
                inner: outer,
                clip: Clip::None,
            },
        );
        let inner = add(&mut tree, outer, fixed(10.0, 10.0));

        let _ = layout(&mut tree, 200.0, 200.0);

        assert_eq!(bounds(&tree, outer), Rect::new(0.0, 0.0, 100.0, 100.0));
        assert_eq!(bounds(&tree, inner), Rect::new(4.0, 4.0, 10.0, 10.0));
    }

    #[test]
    fn border_and_padding_offset_child_origin() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut outer = taffy::Style::<smol_str::SmolStr>::DEFAULT;
        outer.size = taffy::geometry::Size {
            width: Dimension::length(100.0),
            height: Dimension::length(100.0),
        };
        outer.padding = taffy::geometry::Rect {
            left: taffy::style::LengthPercentage::length(3.0),
            right: taffy::style::LengthPercentage::length(3.0),
            top: taffy::style::LengthPercentage::length(3.0),
            bottom: taffy::style::LengthPercentage::length(3.0),
        };
        outer.border = taffy::geometry::Rect {
            left: taffy::style::LengthPercentage::length(2.0),
            right: taffy::style::LengthPercentage::length(2.0),
            top: taffy::style::LengthPercentage::length(2.0),
            bottom: taffy::style::LengthPercentage::length(2.0),
        };
        let outer = add(
            &mut tree,
            root,
            Style {
                inner: outer,
                clip: Clip::None,
            },
        );
        let inner = add(&mut tree, outer, fixed(10.0, 10.0));

        let _ = layout(&mut tree, 200.0, 200.0);

        assert_eq!(
            bounds(&tree, inner).origin,
            crate::math::Point::new(5.0, 5.0)
        );
        assert_eq!(
            tree.get(outer).expect("存在").layout().border,
            crate::math::Edges::new(2.0, 2.0, 2.0, 2.0),
            "LayoutBox.border 来自 taffy Layout.border"
        );
        assert_eq!(
            tree.get(inner).expect("存在").layout().border,
            crate::math::Edges::ZERO
        );
    }

    #[test]
    fn min_max_size_clamp_measured_leaf() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut style = taffy::Style::<smol_str::SmolStr>::DEFAULT;
        style.max_size = taffy::geometry::Size {
            width: taffy::style::LengthPercentageAuto::length(50.0),
            height: taffy::style::LengthPercentageAuto::auto(),
        };
        let node = NodeBuilder::boxed()
            .style(Style {
                inner: style,
                clip: Clip::None,
            })
            .measure(Box::new(|_known, _available| Size::new(200.0, 10.0)))
            .build(&mut tree, root)
            .expect("挂载成功");

        let _ = layout(&mut tree, 100.0, 100.0);

        assert_eq!(
            bounds(&tree, node).size.width,
            50.0,
            "max_width 夹住 measure"
        );
    }

    #[test]
    fn percent_size_resolves_against_parent() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let parent = add(&mut tree, root, fixed(100.0, 100.0));
        let child = add(
            &mut tree,
            parent,
            style_with(Dimension::percent(0.5), Dimension::percent(0.5)),
        );

        let _ = layout(&mut tree, 200.0, 200.0);

        assert_eq!(bounds(&tree, child).size, Size::new(50.0, 50.0));
    }

    #[test]
    fn display_none_zeroes_subtree() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut hidden = taffy::Style::<smol_str::SmolStr>::DEFAULT;
        hidden.display = Display::None;
        hidden.size = taffy::geometry::Size {
            width: Dimension::length(50.0),
            height: Dimension::length(50.0),
        };
        let hidden = add(
            &mut tree,
            root,
            Style {
                inner: hidden,
                clip: Clip::None,
            },
        );
        let child = add(&mut tree, hidden, fixed(10.0, 10.0));

        let _ = layout(&mut tree, 100.0, 100.0);

        assert!(bounds(&tree, hidden).is_empty(), "display:none 的盒为空");
        assert!(bounds(&tree, child).is_empty(), "隐藏子树的盒也为空");
        assert_eq!(
            bounds(&tree, root).size,
            Size::new(100.0, 100.0),
            "根尺寸不受隐藏子树影响"
        );
    }

    #[test]
    fn root_fills_available_space() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let _ = layout(&mut tree, 200.0, 100.0);

        assert_eq!(bounds(&tree, root), Rect::new(0.0, 0.0, 200.0, 100.0));
    }

    #[test]
    fn layout_override_wins_over_flow_position() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let pane = add(&mut tree, root, fixed(100.0, 100.0));
        let canvas = NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 0.0, 1.0, 1.0)))
            .layout_override(Rect::new(8.0, 16.0, 40.0, 20.0))
            .build(&mut tree, pane)
            .expect("挂载成功");

        let _ = layout(&mut tree, 200.0, 200.0);

        assert_eq!(
            bounds(&tree, canvas),
            Rect::new(8.0, 16.0, 40.0, 20.0),
            "override 覆盖 taffy 的出流位置，相对父 border box"
        );
    }

    #[test]
    fn absolute_child_relative_to_positioned_ancestor() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut container = taffy::Style::<smol_str::SmolStr>::DEFAULT;
        container.position = Position::Relative;
        container.size = taffy::geometry::Size {
            width: Dimension::length(100.0),
            height: Dimension::length(100.0),
        };
        let parent = add(
            &mut tree,
            root,
            Style {
                inner: container,
                clip: Clip::None,
            },
        );
        let mut absolute = taffy::Style::<smol_str::SmolStr>::DEFAULT;
        absolute.position = Position::Absolute;
        absolute.inset = taffy::geometry::Rect {
            left: taffy::style::LengthPercentageAuto::length(12.0),
            right: taffy::style::LengthPercentageAuto::auto(),
            top: taffy::style::LengthPercentageAuto::length(8.0),
            bottom: taffy::style::LengthPercentageAuto::auto(),
        };
        absolute.size = taffy::geometry::Size {
            width: Dimension::length(10.0),
            height: Dimension::length(10.0),
        };
        let child = add(
            &mut tree,
            parent,
            Style {
                inner: absolute,
                clip: Clip::None,
            },
        );

        let _ = layout(&mut tree, 200.0, 200.0);

        assert_eq!(bounds(&tree, child), Rect::new(12.0, 8.0, 10.0, 10.0));
    }

    #[test]
    fn out_of_flow_hoisted_to_ancestor_uses_containing_block() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        // 定位祖先（relative, padding 10）
        let mut container_style = taffy::Style::<smol_str::SmolStr>::DEFAULT;
        container_style.position = Position::Relative;
        container_style.size = taffy::geometry::Size {
            width: Dimension::length(100.0),
            height: Dimension::length(100.0),
        };
        container_style.padding = taffy::geometry::Rect {
            left: taffy::style::LengthPercentage::length(10.0),
            right: taffy::style::LengthPercentage::length(10.0),
            top: taffy::style::LengthPercentage::length(10.0),
            bottom: taffy::style::LengthPercentage::length(10.0),
        };
        let container = add(
            &mut tree,
            root,
            Style {
                inner: container_style,
                clip: Clip::None,
            },
        );
        // 静态中间节点（不是包含块）
        let static_mid = add(&mut tree, container, fixed(50.0, 50.0));
        // 出流孙节点：包含块是 container，树父是 static_mid
        let mut abs_style = taffy::Style::<smol_str::SmolStr>::DEFAULT;
        abs_style.position = Position::Absolute;
        abs_style.inset = taffy::geometry::Rect {
            left: taffy::style::LengthPercentageAuto::length(5.0),
            right: taffy::style::LengthPercentageAuto::auto(),
            top: taffy::style::LengthPercentageAuto::length(7.0),
            bottom: taffy::style::LengthPercentageAuto::auto(),
        };
        abs_style.size = taffy::geometry::Size {
            width: Dimension::length(20.0),
            height: Dimension::length(20.0),
        };
        let absolute = add(
            &mut tree,
            static_mid,
            Style {
                inner: abs_style,
                clip: Clip::None,
            },
        );

        let _ = layout(&mut tree, 200.0, 200.0);

        // container 在 (0,0) 100×100；出流盒的 local 相对**包含块 border box**
        // （taffy `absolute_position_offset = border`，不含 padding，见
        //  `taffy-0.15.0/src/compute/flexbox.rs:511`），因此 inset 5/7 直接落到 (5,7)。
        assert_eq!(
            bounds(&tree, absolute),
            Rect::new(5.0, 7.0, 20.0, 20.0),
            "hoisted 出流盒的参照系是包含块的 border box，不是树父"
        );
    }

    #[test]
    fn second_run_without_changes_does_no_work() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let _a = add(&mut tree, root, fixed(10.0, 10.0));
        let _b = add(&mut tree, root, fixed(10.0, 10.0));
        let first = layout(&mut tree, 100.0, 100.0);
        assert!(first.laid_out >= 3, "{first:?}");

        let second = layout(&mut tree, 100.0, 100.0);

        assert_eq!(second.laid_out, 0, "没有任何变化 ⇒ 一个节点都不重算");
        assert_eq!(second.moved, 0);
        assert!(second.cache_hits >= 1, "命中根缓存即可短路整轮");
    }

    #[test]
    fn incremental_layout_hits_sibling_caches() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let a = add(&mut tree, root, fixed(10.0, 10.0));
        let b = add(&mut tree, root, fixed(10.0, 10.0));
        let c = add(&mut tree, root, fixed(10.0, 10.0));
        let _ = layout(&mut tree, 100.0, 100.0);

        // 只改 b 的宽度（生产路径：`NodeTree::set_style` → 标 LAYOUT + 清 node..root 的缓存）
        tree.set_style(b, fixed(40.0, 10.0)).expect("节点存活");
        let report = layout(&mut tree, 100.0, 100.0);

        assert_eq!(bounds(&tree, a), Rect::new(0.0, 0.0, 10.0, 10.0));
        assert_eq!(bounds(&tree, b), Rect::new(10.0, 0.0, 40.0, 10.0));
        assert_eq!(bounds(&tree, c), Rect::new(50.0, 0.0, 10.0, 10.0));
        assert!(report.cache_hits > 0, "兄弟子树必须命中缓存：{report:?}");
        assert!(
            report.laid_out < 4,
            "只应重排 root + b（及其祖先链）：{report:?}"
        );
    }

    #[test]
    fn change_below_fixed_size_ancestor_still_relayouts() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let fixed_parent = add(&mut tree, root, fixed(200.0, 200.0));
        let sibling = add(&mut tree, root, fixed(50.0, 50.0));
        let child = add(&mut tree, fixed_parent, fixed(10.0, 10.0));
        let _ = layout(&mut tree, 400.0, 400.0);
        let sibling_before = bounds(&tree, sibling);
        let parent_before = bounds(&tree, fixed_parent);

        tree.set_style(child, fixed(80.0, 10.0)).expect("节点存活");
        let report = layout(&mut tree, 400.0, 400.0);

        assert_eq!(
            bounds(&tree, child).size.width,
            80.0,
            "固定尺寸祖先下的子节点改动必须生效（清缓存不能早停）"
        );
        assert!(report.cache_hits > 0, "无关兄弟仍命中：{report:?}");
        assert_eq!(bounds(&tree, sibling), sibling_before, "未受影响子树不动");
        assert_eq!(bounds(&tree, fixed_parent), parent_before, "祖先盒不变");
    }

    #[test]
    fn invalid_measure_reports_node_axis_value() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed()
            .measure(Box::new(|_known, _available| Size::new(f32::NAN, 10.0)))
            .build(&mut tree, root)
            .expect("挂载成功");

        let error = tree
            .compute_layout(taffy::geometry::Size {
                width: Some(100.0),
                height: Some(100.0),
            })
            .expect_err("NaN 必须报错");

        assert!(matches!(
            error,
            crate::error::EngineError::InvalidMeasure { node: reported, axis: crate::layout::Axis::X, value }
                if reported == node && value.is_nan()
        ));
    }

    #[test]
    fn set_style_rejects_stale_handles() {
        let mut tree = NodeTree::new();
        let bogus = crate::node::id::NodeId::for_test(99, 1);

        assert!(matches!(
            tree.set_style(bogus, Style::default()),
            Err(crate::error::EngineError::StaleNode { node }) if node == bogus
        ));
        assert!(matches!(
            tree.set_layout_override(bogus, None),
            Err(crate::error::EngineError::StaleNode { .. })
        ));
        assert!(matches!(
            tree.invalidate_measure(bogus),
            Err(crate::error::EngineError::StaleNode { .. })
        ));
    }

    #[test]
    fn set_layout_override_updates_bounds_and_damage() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let canvas = NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 0.0, 1.0, 1.0)))
            .layout_override(Rect::new(0.0, 0.0, 10.0, 10.0))
            .build(&mut tree, root)
            .expect("挂载成功");
        let _ = layout(&mut tree, 100.0, 100.0);
        assert_eq!(bounds(&tree, canvas), Rect::new(0.0, 0.0, 10.0, 10.0));

        tree.set_layout_override(
            canvas,
            Some(LayoutOverride::new(Rect::new(20.0, 20.0, 30.0, 30.0))),
        )
        .expect("节点存活");
        let _ = layout(&mut tree, 100.0, 100.0);

        assert_eq!(bounds(&tree, canvas), Rect::new(20.0, 20.0, 30.0, 30.0));
        assert_eq!(
            tree.get(canvas).expect("存在").damage(),
            Rect::new(0.0, 0.0, 50.0, 50.0),
            "damage = 旧位置 ∪ 新位置"
        );
    }

    #[test]
    fn measure_leaf_is_cached_across_runs() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let calls = std::rc::Rc::new(Cell::new(0usize));
        let counter = calls.clone();
        let node = NodeBuilder::boxed()
            .measure(Box::new(move |_known, available| {
                counter.set(counter.get() + 1);
                let width = match available.width {
                    taffy::AvailableSpace::Definite(width) => width.min(80.0),
                    _ => 80.0,
                };
                Size::new(width, 10.0)
            }))
            .build(&mut tree, root)
            .expect("挂载成功");
        {
            let mut style = fixed(0.0, 0.0);
            style.inner.align_items = taffy::style::AlignItems::FLEX_START;
            style.inner.size = taffy::geometry::Size {
                width: Dimension::length(200.0),
                height: Dimension::length(100.0),
            };
            let _ = tree.set_style(root, style);
        }
        let _ = layout(&mut tree, 200.0, 100.0);
        let first_calls = calls.get();
        let first = bounds(&tree, node);

        let _ = layout(&mut tree, 200.0, 100.0);

        assert_eq!(bounds(&tree, node), first);
        assert_eq!(
            calls.get(),
            first_calls,
            "第二轮输入不变 ⇒ measure 不再调用（taffy cache 命中）"
        );
        assert_eq!(first.size, Size::new(80.0, 10.0));
    }

    #[test]
    fn layout_override_follows_tree_parent() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut root_style = fixed(200.0, 200.0);
        root_style.inner.flex_direction = taffy::style::FlexDirection::Column;
        tree.set_style(root, root_style).expect("root 存活");
        let spacer = add(&mut tree, root, fixed(200.0, 40.0));
        let pane = add(&mut tree, root, fixed(100.0, 100.0));
        let canvas = NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 0.0, 1.0, 1.0)))
            .layout_override(Rect::new(10.0, 20.0, 30.0, 30.0))
            .build(&mut tree, pane)
            .expect("挂载成功");

        let _ = layout(&mut tree, 200.0, 200.0);
        assert_eq!(
            bounds(&tree, pane).origin,
            crate::math::Point::new(0.0, 40.0)
        );
        assert_eq!(
            bounds(&tree, canvas),
            Rect::new(10.0, 60.0, 30.0, 30.0),
            "B1：override 相对树父 border box（父在 y=40）"
        );

        // 父被推开：override 必须跟随（不依赖任何 hoisted 的包含块 local）
        tree.set_style(spacer, fixed(200.0, 100.0)).expect("存活");
        let _ = layout(&mut tree, 200.0, 200.0);
        assert_eq!(
            bounds(&tree, pane).origin,
            crate::math::Point::new(0.0, 100.0)
        );
        assert_eq!(bounds(&tree, canvas), Rect::new(10.0, 120.0, 30.0, 30.0));
    }

    #[test]
    fn hidden_override_has_no_ghost_box() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut hidden = taffy::Style::<smol_str::SmolStr>::DEFAULT;
        hidden.display = Display::None;
        let pane = add(
            &mut tree,
            root,
            Style {
                inner: hidden,
                clip: Clip::None,
            },
        );
        let canvas = NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 0.0, 1.0, 1.0)))
            .layout_override(Rect::new(10.0, 20.0, 30.0, 30.0))
            .build(&mut tree, pane)
            .expect("挂载成功");

        let _ = layout(&mut tree, 200.0, 200.0);

        assert!(
            bounds(&tree, canvas).is_empty(),
            "N3：隐藏的 override 节点不得有非零盒（实际 {:?}）",
            bounds(&tree, canvas)
        );
    }

    #[test]
    fn out_of_flow_descendant_of_hidden_subtree_is_zeroed() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let pane = add(&mut tree, root, fixed(100.0, 100.0));
        let mut abs = taffy::Style::<smol_str::SmolStr>::DEFAULT;
        abs.position = Position::Absolute;
        abs.inset = taffy::geometry::Rect {
            left: taffy::style::LengthPercentageAuto::length(40.0),
            right: taffy::style::LengthPercentageAuto::auto(),
            top: taffy::style::LengthPercentageAuto::length(40.0),
            bottom: taffy::style::LengthPercentageAuto::auto(),
        };
        abs.size = taffy::geometry::Size {
            width: Dimension::length(20.0),
            height: Dimension::length(20.0),
        };
        let absolute = add(
            &mut tree,
            pane,
            Style {
                inner: abs,
                clip: Clip::None,
            },
        );
        let _ = layout(&mut tree, 200.0, 200.0);
        assert_eq!(
            bounds(&tree, absolute),
            Rect::new(40.0, 40.0, 20.0, 20.0),
            "可见时正常"
        );

        // 隐藏父：出流后代的幽灵盒必须清零（N1）
        let mut hidden = taffy::Style::<smol_str::SmolStr>::DEFAULT;
        hidden.display = Display::None;
        tree.set_style(
            pane,
            Style {
                inner: hidden,
                clip: Clip::None,
            },
        )
        .expect("存活");
        let _ = layout(&mut tree, 200.0, 200.0);

        assert!(
            bounds(&tree, absolute).is_empty(),
            "N1：隐藏子树里的出流后代必须零化（实际 {:?}）",
            bounds(&tree, absolute)
        );
    }

    #[test]
    fn flow_root_lays_out_children_like_block() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut container = taffy::Style::<smol_str::SmolStr>::DEFAULT;
        container.display = Display::FlowRoot;
        container.size = taffy::geometry::Size {
            width: Dimension::length(100.0),
            height: Dimension::length(100.0),
        };
        let flow = add(
            &mut tree,
            root,
            Style {
                inner: container,
                clip: Clip::None,
            },
        );
        let first = add(&mut tree, flow, fixed(10.0, 10.0));
        let second = add(&mut tree, flow, fixed(10.0, 20.0));

        let _ = layout(&mut tree, 200.0, 200.0);

        assert_eq!(
            bounds(&tree, first).origin.y,
            0.0,
            "N2：FlowRoot 走 block 排布"
        );
        assert_eq!(bounds(&tree, second).origin.y, 10.0);
    }

    // ── B9：布局后置遍历的增量性（不再全树重算子树包围盒）────────────────────

    /// 快照全树的子树包围盒（与 `recompute_subtree_bounds` 的全量结果做逐位对比）。
    fn snapshot_subtree_bounds(tree: &NodeTree) -> Vec<(NodeId, Rect)> {
        tree.dfs_ids()
            .into_iter()
            .map(|node| (node, tree.subtree_bounds(node)))
            .collect()
    }

    #[test]
    fn incremental_post_pass_touches_only_the_changed_path() {
        // 两级树：root → 100 行 × 100 列 = 10_001 个节点。
        //
        // 说明（为什么不是"root 直接挂 10k 叶子"）：taffy 每轮会把它**直接子节点**的布局写回
        // （`set_unrounded_layout`），于是这些子节点在 `place_subtree` 里必然被访问一次 ——
        // 这是布局算法本身的工作，不是 post-pass 的额外扫描。B9 要证明的是：**布局后置步骤**
        // 不再全树重算包络（旧实现固定 = 节点总数），而是与"被布局算法访问过的节点数"同阶。
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut root_style = fixed(1000.0, 1000.0);
        root_style.inner.flex_direction = taffy::style::FlexDirection::Column;
        tree.set_style(root, root_style).expect("root 存活");
        let mut rows = Vec::with_capacity(100);
        let mut leaves = Vec::with_capacity(100);
        for _ in 0..100 {
            let mut row_style = fixed(1000.0, 8.0);
            row_style.inner.flex_direction = taffy::style::FlexDirection::Row;
            let row = add(&mut tree, root, row_style);
            rows.push(row);
            for _ in 0..100 {
                leaves.push(add(&mut tree, row, fixed(8.0, 8.0)));
            }
        }

        let first = layout(&mut tree, 1000.0, 1000.0);
        assert_eq!(
            first.bounds_updated,
            tree.len(),
            "首轮布局每个节点都要写一次包围盒（{first:?}）"
        );

        // 改**最后一个**叶子：不推挤任何兄弟（位置全不变），只有它自己重排。
        let last = *leaves.last().expect("非空");
        tree.set_style(last, fixed(8.0, 9.0)).expect("节点存活");
        let second = layout(&mut tree, 1000.0, 1000.0);

        assert_eq!(second.moved, 1, "只有一个节点改变尺寸：{second:?}");
        assert!(
            second.bounds_updated <= 300,
            "增量 post-pass 只应写「被布局算法访问过的节点」的包络（≈ root + 100 行 + 变化行的 100 叶 = 201），\
             旧实现恒为全树 = {}；report={second:?}",
            tree.len()
        );
    }

    #[test]
    fn incremental_subtree_bounds_match_a_full_recompute() {
        // 覆盖：普通流、嵌套祖先链、出流盒（hoisted）、`LayoutOverride`。
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut root_style = fixed(200.0, 200.0);
        root_style.inner.flex_direction = taffy::style::FlexDirection::Column;
        tree.set_style(root, root_style).expect("root 存活");
        let spacer = add(&mut tree, root, fixed(200.0, 20.0));

        let mut container_style = fixed(100.0, 100.0);
        container_style.inner.position = taffy::style::Position::Relative;
        let container = add(&mut tree, root, container_style);
        let static_mid = add(&mut tree, container, fixed(50.0, 50.0));
        let mut abs = taffy::Style::<smol_str::SmolStr>::DEFAULT;
        abs.position = taffy::style::Position::Absolute;
        abs.inset = taffy::geometry::Rect {
            left: taffy::style::LengthPercentageAuto::length(5.0),
            right: taffy::style::LengthPercentageAuto::auto(),
            top: taffy::style::LengthPercentageAuto::length(7.0),
            bottom: taffy::style::LengthPercentageAuto::auto(),
        };
        abs.size = taffy::geometry::Size {
            width: taffy::Dimension::length(20.0),
            height: taffy::Dimension::length(20.0),
        };
        let out_of_flow = add(
            &mut tree,
            static_mid,
            Style {
                inner: abs,
                clip: Clip::None,
            },
        );
        let canvas = NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 0.0, 1.0, 1.0)))
            .layout_override(Rect::new(10.0, 10.0, 30.0, 30.0))
            .build(&mut tree, container)
            .expect("挂载成功");

        let verify = |tree: &mut NodeTree, label: &str| {
            let incremental = snapshot_subtree_bounds(tree);
            tree.recompute_subtree_bounds();
            let exact = snapshot_subtree_bounds(tree);
            assert_eq!(
                incremental, exact,
                "{label}：增量包络必须与全量重算逐位一致"
            );
        };

        let _ = layout(&mut tree, 200.0, 200.0);
        verify(&mut tree, "首轮");

        // 1) 推挤容器（出流盒随包含块移动 → 树祖先的包络需要只增修正）。
        tree.set_style(spacer, fixed(200.0, 60.0)).expect("存活");
        let _ = layout(&mut tree, 200.0, 200.0);
        verify(&mut tree, "祖先移动后");

        // 2) 覆盖节点换位置（扩大容器子树包络）。
        tree.set_layout_override(
            canvas,
            Some(LayoutOverride::new(Rect::new(150.0, 150.0, 40.0, 40.0))),
        )
        .expect("存活");
        let _ = layout(&mut tree, 200.0, 200.0);
        verify(&mut tree, "override 移动后");

        // 3) 出流盒改 inset（自己的 local 变化）。
        tree.set_absolute_inset(out_of_flow, crate::math::Point::new(40.0, 40.0));
        let _ = layout(&mut tree, 200.0, 200.0);
        verify(&mut tree, "出流盒移动后");

        // 4) 卸载整棵子树（包络必须收紧，不能留幽灵范围）。
        tree.remove(static_mid).expect("卸载成功");
        let _ = layout(&mut tree, 200.0, 200.0);
        verify(&mut tree, "卸载后");
    }

    /// wave-9 性能回归的确定性钉板：**大量出流兄弟**时，祖先包络修正必须去重（每个受波及祖先
    /// 只重算一次），不能是 `O(移动出流盒数 × 祖先数 × 直接子节点数)`。
    ///
    /// 计数用 `LayoutReport::bounds_updated`（`store_subtree_bounds` 的调用次数）：
    /// 200 个绝对定位子节点 ⇒ 首轮 = 根 + 200 个孩子 = 201 次放置写入 + **1** 次根包络修正；
    /// 未去重的实现会在每个移动子节点上重写根一次（+200），> 300 就会失败。
    #[test]
    fn many_out_of_flow_siblings_recompute_each_ancestor_once() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut children = Vec::new();
        for index in 0..200 {
            let child = add(&mut tree, root, fixed(10.0, 10.0));
            tree.set_absolute_inset(
                child,
                crate::math::Point::new((index % 20) as f32 * 10.0, (index / 20) as f32 * 10.0),
            );
            children.push(child);
        }
        let report = layout(&mut tree, 400.0, 400.0);
        assert!(
            report.bounds_updated <= 300,
            "首轮包络写入必须 ≈ 节点数（根 1 + 200），实际 {} —— 祖先修正没有去重",
            report.bounds_updated
        );
        // 第二次布局：结构没变 ⇒ 没有移动出流盒 ⇒ 只重贴根（以及被 taffy 写回的孩子）。
        let second = layout(&mut tree, 400.0, 400.0);
        assert!(
            second.bounds_updated <= 300,
            "稳定态包络写入必须 O(被布局节点数)，实际 {}",
            second.bounds_updated
        );
        // 正确性：增量包络与全量重算逐位一致（去重/自底向上顺序不能改变结果）。
        let incremental = children
            .iter()
            .map(|child| tree.subtree_bounds(*child))
            .collect::<Vec<_>>();
        tree.recompute_subtree_bounds();
        let exact = children
            .iter()
            .map(|child| tree.subtree_bounds(*child))
            .collect::<Vec<_>>();
        assert_eq!(incremental, exact, "增量包络必须与全量重算一致");
    }

    /// `LayoutOverride` 节点由树父就地放置（B1），包络天然精确 ⇒ **不得**进入跨界的祖先修正。
    ///
    /// 未修版本把 512 个 override 兄弟当"出流跨界"处理：每个都重扫根的 512 个孩子
    /// （实测 `layout_512` 1.5 ms，修复后 ≈ 180 µs）。计数断言把它变成确定性测试。
    #[test]
    fn layout_overrides_never_enter_the_ancestor_fixup() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut children = Vec::new();
        for index in 0..512 {
            let x = (index % 8) as f32 * 96.0;
            let y = (index / 8) as f32 * 32.0;
            let child = NodeBuilder::boxed()
                .layout_override(Rect::new(x, y, 90.0, 28.0))
                .build(&mut tree, root)
                .expect("挂载成功");
            children.push(child);
        }
        let report = layout(&mut tree, 800.0, 600.0);
        assert_eq!(
            report.bounds_updated, 513,
            "根 + 512 个 override 各写一次包络；override 不得触发祖先修正"
        );
        // 包络正确性：根 = 根自身 (800×600) ∪ 全部 override 盒（最后一格 y = 63 × 32 + 28）。
        assert_eq!(bounds(&tree, children[0]), Rect::new(0.0, 0.0, 90.0, 28.0));
        assert_eq!(
            bounds(&tree, children[511]),
            Rect::new(7.0 * 96.0, 63.0 * 32.0, 90.0, 28.0)
        );
        assert_eq!(
            tree.subtree_bounds(root),
            Rect::new(0.0, 0.0, 800.0, 2016.0 + 28.0)
        );
    }

    #[test]
    fn device_snapping_descends_when_only_the_subpixel_reference_moved() {
        // B7 的正确性边角：父/中间盒吸附后**没变**，但未圆整的位移让**孙子**的边界跨过半像素。
        //
        // 链：root → spacer + container → mid（margin 0.15）→ leaf（margin 0.3）。
        // spacer 10.0 → 10.2 让整条链的未圆整 y 各 +0.2：container 10.0→10.2（吸附 10/10）、
        // mid 10.15→10.35（吸附 10/10，**不变** ⇒ 会触发早期退出）、
        // leaf 10.45→10.65（吸附 10→**11**，必须重贴）。
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut root_style = fixed(200.0, 200.0);
        root_style.inner.flex_direction = taffy::style::FlexDirection::Column;
        tree.set_style(root, root_style).expect("root 存活");
        let spacer = add(&mut tree, root, fixed(200.0, 10.0));
        let container = add(&mut tree, root, fixed(100.0, 100.0));
        let mid = NodeBuilder::boxed()
            .style({
                let mut style = fixed(80.0, 80.0);
                style.inner.margin = taffy::geometry::Rect {
                    left: taffy::style::LengthPercentageAuto::length(0.0),
                    right: taffy::style::LengthPercentageAuto::length(0.0),
                    top: taffy::style::LengthPercentageAuto::length(0.15),
                    bottom: taffy::style::LengthPercentageAuto::length(0.0),
                };
                style
            })
            .build(&mut tree, container)
            .expect("挂载成功");
        let leaf = NodeBuilder::boxed()
            .style({
                let mut style = fixed(50.0, 10.0);
                style.inner.margin = taffy::geometry::Rect {
                    left: taffy::style::LengthPercentageAuto::length(0.0),
                    right: taffy::style::LengthPercentageAuto::length(0.0),
                    top: taffy::style::LengthPercentageAuto::length(0.3),
                    bottom: taffy::style::LengthPercentageAuto::length(0.0),
                };
                style
            })
            .build(&mut tree, mid)
            .expect("挂载成功");
        tree.set_device_scale(Some(1.0));
        let _ = layout(&mut tree, 200.0, 200.0);
        assert_eq!(bounds(&tree, container).origin.y, 10.0);
        assert_eq!(bounds(&tree, mid).origin.y, 10.0, "10.15 吸附到 10");
        assert_eq!(bounds(&tree, leaf).origin.y, 10.0, "10.45 吸附到 10");

        tree.set_style(spacer, fixed(200.0, 10.2)).expect("存活");
        let _ = layout(&mut tree, 200.0, 200.0);

        assert_eq!(
            bounds(&tree, container).origin.y,
            10.0,
            "container 的吸附盒没有变"
        );
        assert_eq!(
            bounds(&tree, mid).origin.y,
            10.0,
            "mid 的吸附盒也没有变（这正是早期退出的陷阱）"
        );
        assert_eq!(
            bounds(&tree, leaf).origin.y,
            11.0,
            "孙子的边界跨过半像素时必须重贴（否则提交盒错位 1px）"
        );
    }

    /// R04-10 验收：measure 违规帧**不得**消费 `LAYOUT`，也不得把 NaN 交给 taffy。
    ///
    /// 第一轮 measure 返回 NaN（`InvalidMeasure`），第二轮返回 10 —— 期间不调用任何
    /// `mark_layout` / `invalidate_measure`，节点必须自己回到 10×10。
    /// 旧实现：错误帧已把 `LAYOUT` 清掉（`set_local_layout` 无条件 `remove`）、且 taffy 缓存里
    /// 留着 NaN 布局 ⇒ 第二轮 cache 命中、第二/三次不再调 measure，节点永久停在坏盒子上。
    #[test]
    fn an_invalid_measure_is_recoverable_without_an_external_invalidation() {
        use std::rc::Rc;

        use crate::error::EngineError;

        let mut tree = NodeTree::new();
        let root = tree.root();
        let calls = Rc::new(Cell::new(0usize));
        let counter = Rc::clone(&calls);
        let node = NodeBuilder::boxed()
            .measure(Box::new(move |_known, _available| {
                let turn = counter.get();
                counter.set(turn + 1);
                if turn == 0 {
                    Size::new(f32::NAN, 10.0)
                } else {
                    Size::new(10.0, 10.0)
                }
            }))
            .build(&mut tree, root)
            .expect("挂载成功");

        let error = tree
            .compute_layout(taffy::geometry::Size {
                width: Some(100.0),
                height: Some(100.0),
            })
            .expect_err("NaN measure 必须报错");
        assert!(matches!(
            error,
            EngineError::InvalidMeasure {
                node: reported,
                axis: crate::layout::Axis::X,
                ..
            } if reported == node
        ));
        assert!(
            tree.has_layout_work(),
            "错误帧不得消费 LAYOUT：否则应用修好闭包后没有失效源能自愈"
        );

        let report = tree
            .compute_layout(taffy::geometry::Size {
                width: Some(100.0),
                height: Some(100.0),
            })
            .expect("第二轮必须自愈");
        let committed = bounds(&tree, node);
        // flex 行会沿交叉轴拉伸子节点高度，因此只断言 measure 决定的主轴宽度。
        assert_eq!(committed.size.width, 10.0, "第二轮必须重跑 measure");
        assert!(report.laid_out > 0, "{report:?}");
        assert!(calls.get() > 1, "measure 必须被重新调用：{}", calls.get());
        assert!(!tree.has_layout_work(), "布局成功后脏集合必须清空");
    }
}
