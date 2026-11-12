//! 命中测试：逆绘制序 + clip 链 + `hit_shape`。
//!
//! 坐标契约：`bounds` 是窗口绝对逻辑像素；`HitResult::local_point` 是**目标节点 border box 局部坐标**
//! （原点 = 节点左上角）。`clip_chain` 是沿途每个 `Clip::Bounds` **祖先**的 padding box（窗口绝对，祖先→子），
//! **不含目标自己的 clip**。
//!
//! 设计见 `design/phase-04-layout.md` §12。

use crate::{
    math::{Point, Rect},
    node::{id::NodeId, tree::NodeTree},
    paint::style::Clip,
};

/// clip 栈深度上限：**复用** `paint::scene::MAX_CLIP_DEPTH`（`u8`，值 8），不新增第四份常量。
const MAX_CLIP_DEPTH: usize = crate::paint::scene::MAX_CLIP_DEPTH as usize;

/// 命中路径上的 clip 矩形（定长、`Copy`、零分配；祖先 → 目标顺序）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipChain {
    len: u8,
    rects: [Rect; MAX_CLIP_DEPTH],
}

impl Default for ClipChain {
    fn default() -> Self {
        Self {
            len: 0,
            rects: [Rect::default(); MAX_CLIP_DEPTH],
        }
    }
}

impl ClipChain {
    /// 由祖先到目标顺序的 clip 矩形切片（窗口绝对）。
    #[must_use]
    pub fn as_slice(&self) -> &[Rect] {
        &self.rects[..usize::from(self.len)]
    }

    /// 已压入的 clip 数。
    #[must_use]
    pub fn len(&self) -> usize {
        usize::from(self.len)
    }

    /// 是否为空（未被任何祖先裁剪）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 压入一个 clip 矩形；超过深度上限则**丢弃**（不 panic —— 兄弟子树仍可能命中）。
    fn push(&mut self, rect: Rect) {
        if usize::from(self.len) < MAX_CLIP_DEPTH {
            self.rects[usize::from(self.len)] = rect;
            self.len += 1;
        }
    }

    /// 回到 `len` 深度（子树回溯时必须撤销，否则兄弟/自身的 chain 会带上别人的矩形）。
    fn truncate(&mut self, len: usize) {
        self.len = self.len.min(len as u8);
    }
}

/// 命中结果（设计 §4.4：`HitResult { node, local_point, clip_chain }`）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HitResult {
    /// 命中的节点（逆绘制序的第一个）。
    pub node: NodeId,
    /// 命中点在目标节点 border box 局部坐标下的位置。
    pub local_point: Point,
    /// 到达目标节点经过的 clip 矩形（窗口绝对；**不含**目标自己的 clip）。
    pub clip_chain: ClipChain,
}

/// 逆绘制序（`order` 是 DFS 先序 ⇒ 兄弟逆序 + 先子树后自身）+ 按布局盒与 clip 剪枝。
pub(crate) fn hit_test(tree: &NodeTree, point: Point) -> Option<HitResult> {
    let root = tree.root();
    if !tree.get(root).ok()?.layout().bounds.contains(point) {
        return None; // 窗口外（根 100% 撑满 ⇒ 等价于「点在窗口外」）
    }
    let mut chain = ClipChain::default();
    pick(tree, root, point, &mut chain)
}

fn pick(tree: &NodeTree, node: NodeId, point: Point, chain: &mut ClipChain) -> Option<HitResult> {
    let slot = tree.get(node).ok()?;
    let bounds = slot.layout().bounds;
    // 本节点**未**入链前的深度：`clip_chain` 只含祖先的 clip（目标自己的 padding box 由应用自己算）。
    let depth = chain.len();

    // 不可见/隐藏的节点整体不参与命中（对目标自身也成立，与 P06 的「不绘制」一致）。
    if matches!(slot.style().inner.display, taffy::style::Display::None) {
        return None;
    }
    if slot.style_patch().visible == Some(false) {
        return None;
    }

    // 本节点的 clip **只作用于子树**（CSS：overflow 裁剪内容，不裁剪自身的 border/background）。
    // 点在 clip 外时仍可命中本节点，但不能下钻。
    let mut clipped = false;
    if slot.clip() == Clip::Bounds {
        let clip_rect = bounds.inset(slot.layout().border);
        if clip_rect.contains(point) {
            chain.push(clip_rect);
        } else {
            clipped = true;
        }
    }

    if !clipped {
        // 子节点逆序：最后一个子节点绘制在最上层，因此先测。
        for child in slot.children().iter().rev() {
            let Ok(child_node) = tree.get(*child) else {
                continue;
            };
            // B2：布局盒**不能**作为子树的剪枝依据（出流/溢出子节点可落在父盒之外）。
            // 唯一安全的子树剪枝是「本节点 `Clip::Bounds` 且点在 padding box 外」，由 `pick`
            // 内部的 `clipped` 处理（它仍然允许命中该节点自己的 border 带）。
            // 这里只跳过「叶子 + 盒外」这种零代价的明显不可能项。
            let leaf = child_node.children().is_empty();
            if leaf && !child_node.layout().bounds.contains(point) {
                continue;
            }
            if let Some(hit) = pick(tree, *child, point, chain) {
                chain.truncate(depth);
                return Some(hit);
            }
        }
    }

    // 自身：先看布局盒（允许溢出后代，但自身不声称盒外命中），再看 `hit_shape`（局部坐标）。
    if !bounds.contains(point) {
        chain.truncate(depth);
        return None;
    }
    let local_point = Point::new(point.x - bounds.origin.x, point.y - bounds.origin.y);
    if let Some(shape) = slot.hit_shape()
        && !shape(local_point)
    {
        chain.truncate(depth);
        return None;
    }
    let mut clip_chain = *chain;
    clip_chain.truncate(depth);
    chain.truncate(depth);
    Some(HitResult {
        node,
        local_point,
        clip_chain,
    })
}

#[cfg(test)]
mod tests {
    use crate::{
        layout::hit_test::{ClipChain, hit_test},
        math::{Point, Rect},
        node::{
            builder::NodeBuilder,
            id::NodeId,
            tree::{NodeKind, NodeTree},
        },
        paint::{
            canvas::Canvas,
            style::{Clip, Style, StylePatch},
        },
    };

    fn fixed(width: f32, height: f32) -> Style {
        let mut inner = taffy::Style::<smol_str::SmolStr>::DEFAULT;
        inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::length(width),
            height: taffy::style::Dimension::length(height),
        };
        Style {
            inner,
            clip: Clip::None,
        }
    }

    fn absolute(size: f32) -> Style {
        let mut style = fixed(size, size);
        style.inner.position = taffy::style::Position::Absolute;
        style.inner.inset = taffy::geometry::Rect {
            left: taffy::style::LengthPercentageAuto::length(0.0),
            right: taffy::style::LengthPercentageAuto::auto(),
            top: taffy::style::LengthPercentageAuto::length(0.0),
            bottom: taffy::style::LengthPercentageAuto::auto(),
        };
        style
    }

    fn add(tree: &mut NodeTree, parent: NodeId, style: Style) -> NodeId {
        NodeBuilder::boxed()
            .style(style)
            .build(tree, parent)
            .expect("挂载成功")
    }

    fn layout(tree: &mut NodeTree) {
        let _ = tree
            .compute_layout(taffy::geometry::Size {
                width: Some(200.0),
                height: Some(200.0),
            })
            .expect("布局成功");
    }

    fn hit_node(tree: &NodeTree, x: f32, y: f32) -> Option<NodeId> {
        hit_test(tree, Point::new(x, y)).map(|hit| hit.node)
    }

    #[test]
    fn deepest_last_child_wins() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let container = add(&mut tree, root, fixed(100.0, 100.0));
        // 两个完全重叠的浮层（绝对定位，同位置同尺寸）⇒ 后创建的在最上层
        let lower = add(&mut tree, container, absolute(100.0));
        let upper = add(&mut tree, container, absolute(100.0));
        layout(&mut tree);

        let hit = hit_test(&tree, Point::new(50.0, 50.0)).expect("命中");
        assert_eq!(hit.node, upper, "后创建的子节点绘制在上层");
        assert_ne!(hit.node, lower);
        assert_eq!(hit.local_point, Point::new(50.0, 50.0));
        assert!(hit.clip_chain.is_empty());
    }

    #[test]
    fn child_beats_parent_background() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let parent = add(&mut tree, root, fixed(100.0, 100.0));
        let child = add(&mut tree, parent, fixed(20.0, 20.0));
        layout(&mut tree);

        assert_eq!(hit_node(&tree, 10.0, 10.0), Some(child), "子内容压过父背景");
        assert_eq!(hit_node(&tree, 50.0, 50.0), Some(parent), "父背景可命中");
    }

    #[test]
    fn clip_prunes_subtree_and_fills_chain() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut scroller_style = fixed(40.0, 40.0);
        scroller_style.clip = Clip::Bounds;
        scroller_style.inner.border = taffy::geometry::Rect {
            left: taffy::style::LengthPercentage::length(2.0),
            right: taffy::style::LengthPercentage::length(2.0),
            top: taffy::style::LengthPercentage::length(2.0),
            bottom: taffy::style::LengthPercentage::length(2.0),
        };
        let scroller = add(&mut tree, root, scroller_style);
        // 尺寸超出父、且被父裁剪（taffy 允许溢出，不做裁剪）
        let overflowing = add(&mut tree, scroller, fixed(100.0, 100.0));
        layout(&mut tree);

        let inside = hit_test(&tree, Point::new(10.0, 10.0)).expect("命中");
        assert_eq!(inside.node, overflowing);
        assert_eq!(inside.clip_chain.len(), 1, "裁剪祖先进链");
        assert_eq!(
            inside.clip_chain.as_slice()[0],
            Rect::new(2.0, 2.0, 36.0, 36.0),
            "padding box = border box 内缩 border"
        );

        // 点仍在 scroller 的 bounds 内但在 padding box 外 ⇒ 子节点被裁剪，命中父自身；
        // 父自己的 clip **不进** chain。
        let border_band = hit_test(&tree, Point::new(1.0, 1.0)).expect("命中父");
        assert_eq!(border_band.node, scroller, "被裁剪的子节点不可命中");
        assert!(
            border_band.clip_chain.is_empty(),
            "目标自身的 clip 不进 clip_chain"
        );

        // 父 bounds 之外 ⇒ 落到根背景
        assert_eq!(hit_node(&tree, 60.0, 60.0), Some(root));
    }

    #[test]
    fn nested_clips_accumulate_outer_to_inner() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut outer_style = fixed(100.0, 100.0);
        outer_style.clip = Clip::Bounds;
        let outer = add(&mut tree, root, outer_style);
        let mut inner_style = fixed(60.0, 60.0);
        inner_style.clip = Clip::Bounds;
        let inner = add(&mut tree, outer, inner_style);
        let leaf = add(&mut tree, inner, fixed(20.0, 20.0));
        layout(&mut tree);

        let hit = hit_test(&tree, Point::new(5.0, 5.0)).expect("命中");
        assert_eq!(hit.node, leaf);
        assert_eq!(hit.clip_chain.len(), 2, "两层 clip 都进链");
        assert_eq!(
            hit.clip_chain.as_slice()[0],
            Rect::new(0.0, 0.0, 100.0, 100.0),
            "外层在前"
        );
        assert_eq!(
            hit.clip_chain.as_slice()[1],
            Rect::new(0.0, 0.0, 60.0, 60.0),
            "内层在后"
        );
    }

    #[test]
    fn clip_chain_caps_at_eight() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut parent = root;
        for _ in 0..10 {
            let mut style = fixed(100.0, 100.0);
            style.clip = Clip::Bounds;
            parent = add(&mut tree, parent, style);
        }
        let leaf = add(&mut tree, parent, fixed(10.0, 10.0));
        layout(&mut tree);

        let hit = hit_test(&tree, Point::new(1.0, 1.0)).expect("命中");
        assert_eq!(hit.node, leaf);
        assert_eq!(
            hit.clip_chain.len(),
            crate::paint::scene::MAX_CLIP_DEPTH as usize,
            "超过深度上限时截断，不 panic"
        );
    }

    #[test]
    fn hit_shape_filters_local_points() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        // 自绘节点：只有左半边可命中（局部坐标 x < 20）
        let canvas = NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 0.0, 40.0, 20.0)))
            .style(fixed(40.0, 20.0))
            .hit_shape(|local: Point| local.x < 20.0)
            .build(&mut tree, root)
            .expect("挂载成功");
        layout(&mut tree);

        assert_eq!(hit_node(&tree, 10.0, 10.0), Some(canvas));
        assert_eq!(
            hit_node(&tree, 30.0, 10.0),
            Some(root),
            "hit_shape 拒绝 ⇒ 落到下层（根背景）"
        );
    }

    #[test]
    fn hit_shape_receives_node_local_coords() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let parent = add(&mut tree, root, fixed(100.0, 100.0));
        // 节点在父的 (10, 20) 处
        let node = NodeBuilder::boxed()
            .style({
                let mut style = fixed(30.0, 30.0);
                style.inner.margin = taffy::geometry::Rect {
                    left: taffy::style::LengthPercentageAuto::length(10.0),
                    right: taffy::style::LengthPercentageAuto::length(0.0),
                    top: taffy::style::LengthPercentageAuto::length(20.0),
                    bottom: taffy::style::LengthPercentageAuto::length(0.0),
                };
                style
            })
            .hit_shape(|local: Point| {
                // 局部坐标原点必须是节点自己的左上角
                local.x >= 0.0 && local.y >= 0.0 && local.x < 30.0 && local.y < 30.0
            })
            .build(&mut tree, parent)
            .expect("挂载成功");
        layout(&mut tree);

        let origin = tree.get(node).expect("存在").layout().bounds.origin;
        assert_eq!(origin, Point::new(10.0, 20.0), "margin 决定位置");
        let hit = hit_test(&tree, Point::new(15.0, 25.0)).expect("命中");
        assert_eq!(hit.node, node);
        assert_eq!(hit.local_point, Point::new(5.0, 5.0));
    }

    #[test]
    fn display_none_is_not_hittable() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut hidden = fixed(100.0, 100.0);
        hidden.inner.display = taffy::style::Display::None;
        let _hidden = add(&mut tree, root, hidden);
        layout(&mut tree);

        assert_eq!(hit_node(&tree, 10.0, 10.0), Some(root));
    }

    #[test]
    fn visible_false_skips_node() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed()
            .style(fixed(100.0, 100.0))
            .bind_style(|_cx| StylePatch {
                visible: Some(false),
                ..StylePatch::default()
            })
            .build(&mut tree, root)
            .expect("挂载成功");
        tree.set_style_patch(
            node,
            StylePatch {
                visible: Some(false),
                ..StylePatch::default()
            },
        );
        let _ = node;
        layout(&mut tree);

        assert_eq!(
            hit_node(&tree, 10.0, 10.0),
            Some(root),
            "visible=false 的节点不参与命中"
        );
    }

    #[test]
    fn points_outside_the_window_miss() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let _ = add(&mut tree, root, fixed(50.0, 50.0));
        layout(&mut tree);

        assert!(hit_test(&tree, Point::new(-1.0, 5.0)).is_none());
        assert!(hit_test(&tree, Point::new(500.0, 5.0)).is_none());
    }

    #[test]
    fn overlay_like_last_sibling_wins_regardless_of_size() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let base = add(&mut tree, root, fixed(100.0, 100.0));
        // 浮层：绝对定位在最后，尺寸小 ⇒ 必须 append 到 order 末尾（P13 的 Overlay 契约）
        let overlay = add(&mut tree, root, absolute(20.0));
        layout(&mut tree);

        assert_eq!(
            hit_node(&tree, 5.0, 5.0),
            Some(overlay),
            "绘制序靠后的浮层优先"
        );
        assert_eq!(hit_node(&tree, 50.0, 50.0), Some(base));
    }

    #[test]
    fn override_node_is_hittable_at_its_rect() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let pane = add(&mut tree, root, fixed(100.0, 100.0));
        let canvas = NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 0.0, 1.0, 1.0)))
            .layout_override(Rect::new(10.0, 10.0, 30.0, 30.0))
            .build(&mut tree, pane)
            .expect("挂载成功");
        layout(&mut tree);

        assert_eq!(hit_node(&tree, 15.0, 15.0), Some(canvas));
        assert_eq!(
            hit_node(&tree, 5.0, 5.0),
            Some(pane),
            "override 之外的区域仍是父节点"
        );
    }

    #[test]
    fn local_point_is_border_box_relative() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let parent = add(&mut tree, root, fixed(100.0, 100.0));
        let node = add(&mut tree, parent, {
            let mut style = fixed(30.0, 40.0);
            style.inner.margin = taffy::geometry::Rect {
                left: taffy::style::LengthPercentageAuto::length(10.0),
                right: taffy::style::LengthPercentageAuto::length(0.0),
                top: taffy::style::LengthPercentageAuto::length(20.0),
                bottom: taffy::style::LengthPercentageAuto::length(0.0),
            };
            style
        });
        layout(&mut tree);

        let hit = hit_test(&tree, Point::new(15.0, 25.0)).expect("命中");
        assert_eq!(hit.node, node);
        assert_eq!(hit.local_point, Point::new(5.0, 5.0));
    }

    #[test]
    fn overlapping_siblings_later_wins() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        // 完全重叠的两个兄弟（后创建的在上层）⇒ 后者胜出（N6）
        let first = add(&mut tree, root, absolute(100.0));
        let second = add(&mut tree, root, absolute(100.0));
        layout(&mut tree);

        assert_eq!(hit_node(&tree, 50.0, 50.0), Some(second));
        assert_ne!(hit_node(&tree, 50.0, 50.0), Some(first));
    }

    #[test]
    fn clip_chain_is_copy_and_default_is_empty() {
        let chain = ClipChain::default();
        assert!(chain.is_empty());
        assert_eq!(chain.len(), 0);
        assert!(chain.as_slice().is_empty());
        let copy = chain;
        assert_eq!(copy, chain);
    }

    #[test]
    fn kind_of_override_node_stays_canvas() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let canvas = NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 0.0, 1.0, 1.0)))
            .layout_override(Rect::new(0.0, 0.0, 10.0, 10.0))
            .build(&mut tree, root)
            .expect("挂载成功");

        assert!(matches!(
            tree.get(canvas).expect("存在").kind(),
            NodeKind::Canvas(_)
        ));
    }

    #[test]
    fn overflowing_child_hits_through_unclipped_parent() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let parent = add(&mut tree, root, fixed(100.0, 100.0));
        // 出流子节点落在父盒之外（父无 clip ⇒ 允许溢出命中，B2）
        let mut abs = taffy::Style::<smol_str::SmolStr>::DEFAULT;
        abs.position = taffy::style::Position::Absolute;
        abs.inset = taffy::geometry::Rect {
            left: taffy::style::LengthPercentageAuto::length(150.0),
            right: taffy::style::LengthPercentageAuto::auto(),
            top: taffy::style::LengthPercentageAuto::length(10.0),
            bottom: taffy::style::LengthPercentageAuto::auto(),
        };
        abs.size = taffy::geometry::Size {
            width: taffy::style::Dimension::length(30.0),
            height: taffy::style::Dimension::length(30.0),
        };
        let overflow = add(
            &mut tree,
            parent,
            Style {
                inner: abs,
                clip: Clip::None,
            },
        );
        layout(&mut tree);

        assert_eq!(
            tree.get(overflow).expect("存在").layout().bounds,
            Rect::new(150.0, 10.0, 30.0, 30.0)
        );
        assert_eq!(
            hit_node(&tree, 160.0, 25.0),
            Some(overflow),
            "无 clip 的父必须下钻，允许溢出子节点命中"
        );
    }

    #[test]
    fn clipped_parent_blocks_overflowing_child() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut parent_style = fixed(100.0, 100.0);
        parent_style.clip = Clip::Bounds;
        let parent = add(&mut tree, root, parent_style);
        let mut abs = taffy::Style::<smol_str::SmolStr>::DEFAULT;
        abs.position = taffy::style::Position::Absolute;
        abs.inset = taffy::geometry::Rect {
            left: taffy::style::LengthPercentageAuto::length(150.0),
            right: taffy::style::LengthPercentageAuto::auto(),
            top: taffy::style::LengthPercentageAuto::length(10.0),
            bottom: taffy::style::LengthPercentageAuto::auto(),
        };
        abs.size = taffy::geometry::Size {
            width: taffy::style::Dimension::length(30.0),
            height: taffy::style::Dimension::length(30.0),
        };
        let overflow = add(
            &mut tree,
            parent,
            Style {
                inner: abs,
                clip: Clip::None,
            },
        );
        layout(&mut tree);

        assert_eq!(
            tree.get(overflow).expect("存在").layout().bounds,
            Rect::new(150.0, 10.0, 30.0, 30.0),
            "布局盒照常存在（taffy 不裁剪）"
        );
        assert_eq!(
            hit_node(&tree, 160.0, 25.0),
            Some(root),
            "有 clip 的父把溢出子节点裁掉"
        );
    }

    #[test]
    fn override_is_hittable_under_offset_parent() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        // B1：override 相对**树父** border box，父被 spacer 推到 y=40
        let mut root_style = fixed(200.0, 200.0);
        root_style.inner.flex_direction = taffy::style::FlexDirection::Column;
        tree.set_style(root, root_style).expect("root 存活");
        let spacer = add(&mut tree, root, fixed(200.0, 40.0));
        let pane = add(&mut tree, root, fixed(100.0, 100.0));
        let canvas = NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 0.0, 1.0, 1.0)))
            .layout_override(Rect::new(10.0, 20.0, 30.0, 30.0))
            .build(&mut tree, pane)
            .expect("挂载成功");
        layout(&mut tree);

        assert_eq!(
            tree.get(canvas).expect("存在").layout().bounds,
            Rect::new(10.0, 60.0, 30.0, 30.0),
            "override 绝对坐标 = 树父 origin + rect（不是包含块）"
        );
        assert_eq!(
            hit_node(&tree, 15.0, 65.0),
            Some(canvas),
            "命中使用该绝对盒"
        );
        assert_eq!(
            hit_node(&tree, 15.0, 15.0),
            Some(spacer),
            "父盒之外命中 spacer"
        );
    }
}
