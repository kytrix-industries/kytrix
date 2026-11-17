//! 命中测试：逆绘制序 + clip 链 + per-draw 逆变换 + `hit_shape`。
//!
//! 坐标契约：`bounds` 是窗口绝对逻辑像素；`HitResult::local_point` 是**目标节点 border box 局部坐标**
//! （原点 = 节点左上角）。`clip_chain` 是沿途每个 `Clip::Bounds` **祖先**的 padding box（窗口绝对，祖先→子），
//! **不含目标自己的 clip**。
//!
//! # 命中语义（与绘制、焦点、a11y 对齐）
//!
//! - **可见性**：`display:none` / `visible:false` 的节点及其子树不参与命中（与焦点遍历共用
//!   `NodeTree::is_visible_locally`）。
//! - **禁用（`interactive: Some(false)`）**：整棵子树不命中（**祖先禁用 ⇒ 子树不命中**），
//!   与焦点遍历（`is_effectively_interactive`）和 a11y 的 `disabled` 属性同源。旧实现只让
//!   被标记的节点自己"透明"，子节点仍可命中（R04-9 的"点击穿透"）。
//! - **变换（C11）**：几何节点的 per-draw `Transform` 在命中时走**逆变换** —— 旋转/缩放后的
//!   内容按视觉范围命中，`local_point` 回到未变换的内容坐标。不可逆变换（det≈0 / NaN / ∞）
//!   跳过该节点并在 `NodeTree::hit_test_skips` 记账，**不 panic、不误判命中**。
//! - **数值防御（C13）**：非有限的点/布局盒/裁剪矩形/clip 偏移一律视为"不命中"并跳过该子树；
//!   NaN 永不返回 `true`。
//!
//! 设计见 `design/phase-04-layout.md` §12。

use crate::{
    math::{Point, Rect, Transform},
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

    /// 压入一个 clip 矩形；达到深度上限时**折叠**为与栈顶求交（P2-7）。
    ///
    /// 与 `PaintPass` 的 `ClipContext::push` 同一策略：轴对齐矩形求交无损，因此深层嵌套
    /// 不会让命中路径丢矩形（旧实现直接丢弃，超深子树的命中判定会比绘制宽松）。
    fn push(&mut self, rect: Rect) {
        if usize::from(self.len) >= MAX_CLIP_DEPTH {
            let last = &mut self.rects[usize::from(self.len) - 1];
            *last = last.intersect(rect);
            return;
        }
        self.rects[usize::from(self.len)] = rect;
        self.len += 1;
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

/// 取负（`Point` 没有 `Neg`；屏幕 = 内容 − 偏移）。
fn negate(offset: Point) -> Point {
    Point::new(-offset.x, -offset.y)
}

/// 几何节点的**可见范围**（窗口绝对，AABB）：`bounds.origin + transform(node-local box)`。
///
/// 与绘制侧 `paint::pass::geometry_damage` 用同一算法（四角变换取极值）；非单位变换时内容
/// 可以落在节点布局盒之外，子树剪枝与命中都必须用这个范围（否则"看得见点不到"）。
fn transformed_box(bounds: Rect, transform: Transform) -> Rect {
    let local = Rect::new(0.0, 0.0, bounds.size.width, bounds.size.height);
    let visual = transform.transform_rect(local);
    Rect::from_origin_size(bounds.origin + visual.origin, visual.size)
}

/// 逆绘制序（`children` 的 DFS 先序 ⇒ 兄弟逆序 + 先子树后自身）+ 按布局盒与 clip 剪枝。
///
/// **屏幕坐标**：`point` 是窗口逻辑像素；布局盒是**内容坐标**，因此进入滚动容器的子树时按它的 `offset`
/// 把点换算到内容空间（`design/phase-10-interaction.md` §7.3）。根不是滚动容器 ⇒ 顶层偏移恒为零。
pub(crate) fn hit_test(tree: &NodeTree, point: Point) -> Option<HitResult> {
    // C13：非有限的指针坐标永不命中（NaN 参与任何比较都是 false，但显式短路让诊断确定）。
    if !point.is_finite() {
        tree.note_hit_test_skip();
        return None;
    }
    let root = tree.root();
    let root_bounds = tree.get(root).ok()?.layout().bounds;
    if !root_bounds.is_finite() || !root_bounds.contains(point) {
        return None; // 窗口外（根 100% 撑满 ⇒ 等价于「点在窗口外」）
    }
    // 绝大多数窗口没有任何 per-draw 变换：一次 O(1) 判定后，热路径不再为几何节点做额外检查。
    let transforms = tree.has_geometry_transforms();
    pick(
        tree,
        root,
        point,
        Point::ZERO,
        ClipChain::default(),
        false,
        transforms,
    )
}

fn pick(
    tree: &NodeTree,
    node: NodeId,
    point: Point,
    offset: Point,
    mut chain: ClipChain,
    inherited_disabled: bool,
    transforms: bool,
) -> Option<HitResult> {
    let slot = tree.get(node).ok()?;
    // 本节点的**屏幕**布局盒（内容坐标减去祖先滚动偏移）。
    let bounds = slot.layout().bounds.translate(negate(offset));
    // 本节点**未**入链前的链：`clip_chain` 只含祖先的 clip（目标自己的 padding box 由应用自己算）。
    // `ClipChain` 是 `Copy`，按值传递即可，不需要「截断回某深度」这种容易出错的手工回溯。
    let ancestors = chain;

    // 不可见的节点整体不参与命中（对目标自身也成立，与 P06 的「不绘制」一致）。
    // 规则与焦点遍历共用 `NodeTree::is_visible_locally` —— 两处各写一份是本项目历史上的
    // 分歧来源（Tab 能落到不可见控件上）。
    if !tree.is_visible_locally(node) {
        return None;
    }

    // C12：`interactive: Some(false)` 的**整棵子树**不命中（祖先禁用 ⇒ 子树禁用），与焦点遍历
    // 的 `is_effectively_interactive` 同一条规则；旧实现只让本节点透传，子节点仍可命中。
    // 用已取到的 `slot` 判定（不再二次 `arena.get`）：命中测试是逐指针事件的热路径。
    let disabled = inherited_disabled || slot.style_patch().interactive == Some(false);

    // 本节点的 clip **只作用于子树**（CSS：overflow 裁剪内容，不裁剪自身的 border/background）。
    // 点在 clip 外时仍可命中本节点，但不能下钻。
    let mut clipped = false;
    if slot.clip() == Clip::Bounds {
        let clip_rect = bounds.inset(slot.layout().border);
        if !clip_rect.is_finite() {
            tree.note_hit_test_skip();
            return None;
        }
        if clip_rect.contains(point) {
            chain.push(clip_rect);
        } else {
            clipped = true;
        }
    }

    if !clipped {
        // 子节点的参照系：容器挂了滚动槽时，内容整体按 `offset` 上移（屏幕 = 内容 − 累计偏移）。
        let child_offset = offset
            + tree
                .scroll_slot(node)
                .map_or(Point::ZERO, |slot| slot.offset);
        if !child_offset.is_finite() {
            tree.note_hit_test_skip();
            return None;
        }
        // 子节点逆序：最后一个子节点绘制在最上层，因此先测。
        for child in slot.children().iter().rev() {
            let Ok(child_slot) = tree.get(*child) else {
                continue;
            };
            // 子树剪枝（P2-2）：用**子树包围盒**（自身 + 全部后代布局盒的并集）判断整棵
            // 子树是否可能被命中。只看节点自己的布局盒不行 —— 出流 / 溢出子节点可以落在
            // 父盒之外（B2），按父盒剪枝会漏命中；并集只会偏大，绝不偏小。
            //
            // C13：非有限包络不需要单独判定 —— `Rect::contains` 对非有限矩形恒 `false`
            // （`is_empty` 已含非有限判定），因此这里直接 `continue`，热路径不为诊断多付一次检查。
            let mut subtree = tree.subtree_bounds(*child).translate(negate(child_offset));
            // C11：几何节点的 per-draw 变换可能把内容缩放到布局盒之外（可见范围更大），
            // 剪枝必须用**变换后的**可见范围，否则缩放后的图表"看得见点不到"。
            if transforms && child_slot.kind().is_geometry() {
                let transform = tree.geometry_transform(*child);
                if transform != Transform::IDENTITY {
                    let child_bounds = child_slot.layout().bounds.translate(negate(child_offset));
                    if child_bounds.is_finite() {
                        subtree = subtree.union(transformed_box(child_bounds, transform));
                    }
                }
            }
            if !subtree.contains(point) {
                continue;
            }
            if let Some(hit) = pick(
                tree,
                *child,
                point,
                child_offset,
                chain,
                disabled,
                transforms,
            ) {
                return Some(hit);
            }
        }
    }

    // 自身：先看布局盒（允许溢出后代，但自身不声称盒外命中），再看 `hit_shape`（局部坐标）。
    if disabled {
        // 禁用的节点既不是命中目标，也不再把命中透给它的祖先（CSS `pointer-events:none`
        // 的子树语义：事件穿过**整棵**子树，而不是落到父背景上）。
        return None;
    }
    let local_point = Point::new(point.x - bounds.origin.x, point.y - bounds.origin.y);
    // C13：布局盒/滚动偏移出现 NaN/∞ 时"本节点不命中 + 诊断"，**不 panic、不误判**。
    // 放在自身判定前（而不是入口）：被剪枝的子树根本走不到这里，热路径不为诊断付每次访问的成本；
    // NaN 盒的 `contains` 本来就恒 false（`Rect::is_empty` 含非有限判定），因此正确性不依赖这条检查。
    if !bounds.is_finite() {
        tree.note_hit_test_skip();
        return None;
    }
    // C11：几何节点的 per-draw 变换在命中时走逆变换（命中范围 = `transform(节点盒)` 的 AABB，
    // `local_point` 回到未变换的内容坐标）。绝大多数窗口没有任何变换：`transforms` 是整次
    // 命中测试前的 O(1) 判定，因此热路径只有一次可预测的分支；真正的逆变换逻辑在
    // `#[inline(never)]` 的冷函数里，不会把 `pick` 的指令缓存撑大。
    if transforms && slot.kind().is_geometry() {
        let transform = tree.geometry_transform(node);
        if transform != Transform::IDENTITY {
            return hit_transformed(
                tree,
                node,
                slot,
                point,
                bounds,
                transform,
                local_point,
                ancestors,
            );
        }
    }
    if !bounds.contains(point) {
        return None;
    }
    if let Some(shape) = slot.hit_shape()
        && !shape(local_point)
    {
        return None;
    }
    Some(HitResult {
        node,
        local_point,
        clip_chain: ancestors,
    })
}

/// 带 per-draw 变换的几何节点命中（冷路径）。
///
/// - 命中范围 = `transform(节点盒)` 的 AABB（旋转/缩放的**视觉**范围，可以超出布局盒）；
/// - `local_point` 回到**未变换**的内容坐标（与 `hit_shape`、应用拿到的局部坐标同一空间）；
/// - 不可逆变换 ⇒ 跳过并记账（不 panic、不当作命中）。
#[inline(never)]
fn hit_transformed(
    tree: &NodeTree,
    node: NodeId,
    slot: &crate::node::tree::Node,
    point: Point,
    bounds: Rect,
    transform: Transform,
    local_point: Point,
    ancestors: ClipChain,
) -> Option<HitResult> {
    let Some(inverse) = transform.inverse() else {
        tree.note_hit_test_skip();
        return None;
    };
    let local = inverse.apply(local_point);
    if !local.is_finite() || !transformed_box(bounds, transform).contains(point) {
        return None;
    }
    let local_box = Rect::new(0.0, 0.0, bounds.size.width, bounds.size.height);
    if !local_box.contains(local) {
        return None;
    }
    if let Some(shape) = slot.hit_shape()
        && !shape(local)
    {
        return None;
    }
    Some(HitResult {
        node,
        local_point: local,
        clip_chain: ancestors,
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

    /// P2-7：超过 `MAX_CLIP_DEPTH` 后，clip 折叠为求交，且**回到兄弟子树时必须还原**。
    ///
    /// 折叠把新 clip 求交进栈顶矩形（轴对齐矩形无损）。若不还原，父层的 clip 会被子层的
    /// 矩形永久收窄，兄弟子树就会被过度裁剪、命中丢失。
    #[test]
    fn clip_folding_does_not_leak_into_sibling_subtrees() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        // 先叠满 `MAX_CLIP_DEPTH` 层（外加一层触发折叠）。
        let mut parent = root;
        for _ in 0..=super::MAX_CLIP_DEPTH {
            let mut style = fixed(100.0, 100.0);
            style.clip = Clip::Bounds;
            parent = add(&mut tree, parent, style);
        }
        // 折叠层下面挂两个兄弟：左窄右宽，右边那个落在左边子树的盒子外。
        let mut narrow = fixed(10.0, 10.0);
        narrow.clip = Clip::Bounds;
        let left = add(&mut tree, parent, narrow);
        let right = add(&mut tree, parent, fixed(80.0, 80.0));
        layout(&mut tree);

        // 访问左兄弟会让折叠发生（深度已满）；随后访问右兄弟时必须是**未收窄**的祖先链。
        let _ = hit_node(&tree, 5.0, 5.0);
        assert_eq!(
            hit_node(&tree, 40.0, 40.0),
            Some(right),
            "右兄弟的命中不得被左兄弟的折叠 clip 影响"
        );
        let _ = left;
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
        // 浮层：绝对定位在最后，尺寸小 ⇒ 必须 append 到父的 children 末尾（P10 的 Overlay 契约）
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

    /// P2-2：子树包围盒必须包含**溢出**后代（命中测试的唯一安全剪枝依据）。
    ///
    /// 只看节点自己的布局盒会漏命中出流 / 溢出子节点（B2）；并集偏大但绝不偏小。
    #[test]
    fn subtree_bounds_cover_overflowing_descendants() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let parent = add(&mut tree, root, fixed(100.0, 100.0));
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

        let bounds = tree.subtree_bounds(parent);
        assert!(
            bounds.contains(Point::new(160.0, 25.0)),
            "子树包围盒必须覆盖父盒之外的溢出子节点：{bounds:?}"
        );
        assert!(
            bounds.contains(tree.get(parent).expect("存在").layout().bounds.origin),
            "也必须覆盖自身布局盒：{bounds:?}"
        );
        assert_eq!(
            tree.subtree_bounds(overflow),
            tree.get(overflow).expect("存在").layout().bounds,
            "叶子节点的子树包围盒就是它自己的布局盒"
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

    // ── C11：per-draw 变换的逆变换命中 ─────────────────────────────────────

    fn geometry_node(tree: &mut NodeTree, parent: NodeId, rect: Rect) -> NodeId {
        NodeBuilder::geometry(crate::paint::geometry::GeometryId::for_test(0, 1))
            .layout_override(rect)
            .build(tree, parent)
            .expect("几何节点挂载")
    }

    fn with_transform(tree: &mut NodeTree, node: NodeId, transform: crate::math::Transform) {
        tree.set_geometry_transform(node, transform, Rect::default())
            .expect("几何节点可设置变换");
    }

    #[test]
    fn rotated_geometry_hits_through_inverse_transform() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = geometry_node(&mut tree, root, Rect::new(100.0, 50.0, 40.0, 20.0));
        // 90°：局部 (x, y) → (−y, x)；局部盒 (0,0,40,20) 的 AABB = (−20, 0, 20, 40)。
        with_transform(
            &mut tree,
            node,
            crate::math::Transform::rotate(std::f32::consts::FRAC_PI_2),
        );
        layout(&mut tree);

        // 屏幕 (90, 60) 在变换后的 AABB 内（80..120 × 50..90），逆变换后 = 局部 (10, 10)。
        let hit = hit_test(&tree, Point::new(90.0, 60.0)).expect("旋转后可命中");
        assert_eq!(hit.node, node);
        assert!(
            (hit.local_point.x - 10.0).abs() < 1e-3 && (hit.local_point.y - 10.0).abs() < 1e-3,
            "local_point 必须回到**未变换**的内容坐标：{hit:?}"
        );

        // 未变换时能被命中的 (140, 60)（局部 (40,10) 恰在边界外/旋转后被移走）现在应为
        // 变换后的形状之外 —— 逆变换把点映射到局部 (10, -40)，y 越界。
        assert_eq!(
            hit_node(&tree, 140.0, 60.0),
            Some(root),
            "旋转后的形状不再覆盖原布局盒的右侧"
        );
    }

    #[test]
    fn scaled_geometry_is_hittable_outside_its_layout_box() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = geometry_node(&mut tree, root, Rect::new(0.0, 0.0, 20.0, 10.0));
        with_transform(&mut tree, node, crate::math::Transform::scale(3.0, 3.0));
        layout(&mut tree);

        // 布局盒 20×10，视觉盒 60×30：点 (50, 25) 在布局盒外、视觉盒内。
        assert_eq!(
            hit_node(&tree, 50.0, 25.0),
            Some(node),
            "缩放后的内容必须在视觉范围内可命中（子树剪枝用变换后的包络）"
        );
        // 局部坐标 = 屏幕 / 3。
        let hit = hit_test(&tree, Point::new(50.0, 25.0)).expect("命中");
        assert!((hit.local_point.x - 50.0 / 3.0).abs() < 1e-3);
        // 视觉盒之外不命中。
        assert_eq!(hit_node(&tree, 70.0, 25.0), Some(root));
    }

    #[test]
    fn non_invertible_transform_skips_node_and_is_counted() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = geometry_node(&mut tree, root, Rect::new(0.0, 0.0, 40.0, 40.0));
        with_transform(&mut tree, node, crate::math::Transform::scale(0.0, 1.0));
        layout(&mut tree);

        let before = tree.hit_test_skips();
        assert_eq!(
            hit_node(&tree, 20.0, 20.0),
            Some(root),
            "不可逆变换的节点不得被当作命中（跳过而不是 panic）"
        );
        assert!(
            tree.hit_test_skips() > before,
            "跳过必须记账（C11 的诊断面）"
        );
    }

    // ── C12：interactive/disabled 在命中与焦点上同源 ───────────────────────

    #[test]
    fn disabled_ancestor_makes_its_whole_subtree_unhittable() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let container = add(&mut tree, root, fixed(100.0, 100.0));
        // 子节点**声明动作**：如果没有继承禁用规则，它会是一个合法的命中目标与可聚焦节点。
        let child = NodeBuilder::boxed()
            .style(fixed(50.0, 50.0))
            .on_click(crate::event::Action::custom("press"))
            .build(&mut tree, container)
            .expect("挂载成功");
        // 对照：同样的子节点挂在未禁用的容器下 ⇒ 既可命中也可聚焦（证明下面的 false 来自继承）。
        let active_container = add(&mut tree, root, fixed(100.0, 100.0));
        let active_child = NodeBuilder::boxed()
            .style(fixed(50.0, 50.0))
            .on_click(crate::event::Action::custom("press"))
            .build(&mut tree, active_container)
            .expect("挂载成功");
        tree.set_style_patch_for_test(
            container,
            StylePatch {
                interactive: Some(false),
                ..StylePatch::default()
            },
        );
        layout(&mut tree);

        assert_eq!(
            hit_node(&tree, 10.0, 10.0),
            Some(root),
            "祖先禁用 ⇒ 子树不命中（旧实现会落到子节点上）"
        );
        assert!(
            !crate::interaction::focus::focusable(&tree, child),
            "焦点遍历必须与命中同源（禁用子树的节点不可聚焦）"
        );
        assert_eq!(
            hit_node(&tree, 80.0, 80.0),
            Some(root),
            "禁用容器自己的盒也不命中"
        );
        assert_eq!(
            hit_node(&tree, 110.0, 10.0),
            Some(active_child),
            "对照子树必须可命中（证明上面不是「没有被命中目标」的巧合）"
        );
        assert!(
            crate::interaction::focus::focusable(&tree, active_child),
            "对照子节点必须可聚焦（证明继承规则只影响禁用子树）"
        );
    }

    #[test]
    fn disabled_sibling_does_not_block_other_subtrees() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let disabled = add(&mut tree, root, fixed(100.0, 100.0));
        let sibling = add(&mut tree, root, fixed(100.0, 100.0));
        tree.set_style_patch_for_test(
            disabled,
            StylePatch {
                interactive: Some(false),
                ..StylePatch::default()
            },
        );
        // 两个 100×100 兄弟在 flex 行里并排；各自命中自己的区域。
        layout(&mut tree);

        assert_eq!(hit_node(&tree, 10.0, 10.0), Some(root), "禁用盒不命中");
        assert_eq!(hit_node(&tree, 150.0, 10.0), Some(sibling), "兄弟不受影响");
    }

    /// C12 的三处一致性：命中/焦点用的继承谓词与 a11y 的 `disabled` 判据在**同一节点**上一致。
    ///
    /// a11y 的 `disabled` 属性是每节点值（`a11y::mapping::is_disabled`），祖先禁用由 AT 的
    /// 父节点语义继承；引擎侧命中/焦点的继承规则由 `is_effectively_interactive` 表达。
    #[test]
    fn hit_focus_and_a11y_agree_on_the_interactive_flag() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let plain = add(&mut tree, root, fixed(10.0, 10.0));
        let flagged = add(&mut tree, root, fixed(10.0, 10.0));
        tree.set_style_patch_for_test(
            flagged,
            StylePatch {
                interactive: Some(false),
                ..StylePatch::default()
            },
        );
        layout(&mut tree);

        let target = tree.get(flagged).expect("存在");
        let a11y_disabled = crate::a11y::mapping::is_disabled(target, None, None);
        assert!(a11y_disabled, "a11y 侧必须把它标成 disabled");
        assert!(
            !tree.is_effectively_interactive(flagged),
            "命中/焦点侧必须同样禁用"
        );
        assert_eq!(hit_node(&tree, 15.0, 5.0), Some(root), "命中侧跳过它");

        let plain_target = tree.get(plain).expect("存在");
        assert!(
            !crate::a11y::mapping::is_disabled(plain_target, None, None),
            "未标记的节点在 a11y 侧不是 disabled"
        );
        assert!(tree.is_effectively_interactive(plain));
    }

    // ── C13：NaN / ∞ 防御 ────────────────────────────────────────────────

    #[test]
    fn non_finite_points_never_hit() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let _node = add(&mut tree, root, fixed(100.0, 100.0));
        layout(&mut tree);

        for point in [
            Point::new(f32::NAN, 10.0),
            Point::new(10.0, f32::NAN),
            Point::new(f32::INFINITY, 10.0),
            Point::new(10.0, f32::NEG_INFINITY),
        ] {
            let before = tree.hit_test_skips();
            assert_eq!(hit_test(&tree, point), None, "{point:?} 不得命中");
            assert!(tree.hit_test_skips() > before, "非有限点必须记账诊断");
        }
    }

    #[test]
    fn non_finite_layout_box_is_skipped_not_matched() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = add(&mut tree, root, fixed(100.0, 100.0));
        layout(&mut tree);
        // 造一个 NaN 布局盒（模拟 measure 产出坏值后残留的状态）。
        tree.set_layout_for_test(
            node,
            crate::node::tree::LayoutBox {
                bounds: Rect::new(f32::NAN, f32::NAN, 10.0, 10.0),
                border: crate::math::Edges::ZERO,
            },
        );
        // 根盒仍有效 ⇒ 命中落到根；坏盒子树被跳过且记账。
        let before = tree.hit_test_skips();
        assert_eq!(hit_node(&tree, 10.0, 10.0), Some(root));
        assert!(tree.hit_test_skips() > before, "非有限布局盒必须记账");
    }
}
