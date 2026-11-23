//! 图片节点（E7）的引擎侧解析：内在尺寸 / measure 安装 / 异步资源交付 / 失效。
//!
//! 消费者：`App::refresh_images`（布局之前）、`App::poll_resources`（异步解码交付）、
//! `App::{unload_resource, close_window}`（资源释放后的失效）。全部是**低频**路径
//! （挂载 / 交付 / DPI / 释放），所以这里做全树 DFS；帧内稳态没有这段代码。

use super::{NodeKind, NodeTree};
use crate::{
    math::Size,
    node::{
        id::NodeId,
        image::{ImageContent, ImageSource},
    },
    resources::{PendingResource, ResourceCache, ResourceId, ResourceImage},
};

/// 把 `intrinsic`（逻辑像素）写进图片载荷并安装 measure 闭包。
///
/// 只有「内在尺寸变了 / 还没有 measure」才真正写 + 失效；返回是否发生了变化。
/// 失效语义与文本快照一致（`set_text_shaped`）：清 `node..root` 的 taffy 缓存 +
/// `mark_layout`，否则 taffy 会命中旧的 measure 结果（缓存键不含闭包捕获的状态）。
pub(crate) fn set_intrinsic(tree: &mut NodeTree, node: NodeId, intrinsic: Size) -> bool {
    let changed = {
        let Some(target) = tree.arena.get_mut(node) else {
            return false;
        };
        let NodeKind::Image(content) = &mut target.kind else {
            return false;
        };
        let changed = content.intrinsic() != Some(intrinsic) || target.measure.is_none();
        if changed {
            content.set_intrinsic(intrinsic);
            target.measure = Some(Box::new(move |_known, _available| intrinsic));
        }
        changed
    };
    if changed {
        tree.invalidate_caches_upwards(node);
        tree.mark_layout(node);
        tree.mark_paint(node);
    }
    changed
}

/// 解析所有图片节点的内在尺寸（E7：挂载首帧 / DPI 变化后的布局前 pass）。
///
/// 只处理 `Ready` 且资源**存活**的节点；`Pending` / 已释放的节点保持原样（占位）。
/// 返回内在尺寸真的变了的节点数。
pub(crate) fn refresh_intrinsics(
    tree: &mut NodeTree,
    resources: &ResourceCache,
    scale: f32,
) -> usize {
    let scale = sane_scale(scale);
    let mut updates: Vec<(NodeId, Size)> = Vec::new();
    tree.for_each_dfs(|node| {
        if let Ok(target) = tree.get(node)
            && let Some(content) = target.kind.image()
            && let Some(id) = content.source().resource()
            && let Some(image) = resources.image(id)
        {
            updates.push((node, intrinsic_of(image, scale)));
        }
    });
    updates
        .into_iter()
        .filter(|(node, intrinsic)| set_intrinsic(tree, *node, *intrinsic))
        .count()
}

/// 把 `poll_resources` 交付的 `Ready` 结果写回持有对应 `PendingResource` 的图片节点。
///
/// 同一节点只改写一次；`Failed` 项不在 `resolved` 里 ⇒ 节点保持 `Pending`（占位），
/// 由应用决定重试或换节点。返回改写的节点数。
pub(crate) fn resolve_pending(
    tree: &mut NodeTree,
    resolved: &ahash::HashMap<PendingResource, ResourceImage>,
    scale: f32,
) -> usize {
    if resolved.is_empty() {
        return 0;
    }
    let scale = sane_scale(scale);
    let mut updates: Vec<(NodeId, ResourceImage)> = Vec::new();
    tree.for_each_dfs(|node| {
        if let Ok(target) = tree.get(node)
            && let Some(content) = target.kind.image()
            && let ImageSource::Pending(pending) = content.source()
            && let Some(image) = resolved.get(&pending)
        {
            updates.push((node, *image));
        }
    });
    let mut count = 0;
    for (node, image) in updates {
        if apply_ready(tree, node, image, scale) {
            count += 1;
        }
    }
    count
}

/// 替换图片节点的资源句柄（`NodeTree::set_image_resource` 的实现）。
///
/// 只改载荷：新资源的内在尺寸由**下一次** `App::refresh_images` 在布局之前解析（尺寸可能变）；
/// 这里先标 `LAYOUT | PAINT` 让该帧一定出现。返回 `false` = 节点不存在 / 不是图片节点。
pub(crate) fn set_resource(tree: &mut NodeTree, node: NodeId, resource: ResourceId) -> bool {
    {
        let Some(target) = tree.arena.get_mut(node) else {
            return false;
        };
        let NodeKind::Image(content) = &mut target.kind else {
            return false;
        };
        content.set_source(ImageSource::Ready(resource));
        // 内在尺寸**不在这里清**：新资源可能更大/更小，由下一次 refresh_intrinsics 重算
        // （资源缺失时保留旧内在尺寸 ⇒ 换图失败不会让布局跳动）。
    }
    tree.images_need_refresh = true;
    tree.invalidate_caches_upwards(node);
    tree.mark_layout(node);
    tree.mark_paint(node);
    true
}

/// 标记所有引用 `resource` 的图片节点重绘（`App::unload_resource` 的失效入口）。
///
/// **不动布局**：资源消失不改变节点尺寸（内在尺寸保留，避免卸载导致布局跳动）；
/// paint 跳过图元后节点背景/透明占位照旧。返回标脏的节点数。
pub(crate) fn invalidate_with_resource(tree: &mut NodeTree, resource: ResourceId) -> usize {
    let nodes = collect(tree, |content| {
        content.source().resource() == Some(resource)
    });
    nodes
        .into_iter()
        .filter(|node| tree.mark_paint(*node))
        .count()
}

/// 标记所有「引用的资源已经不在缓存里」的图片节点重绘（关窗 / 释放后的悬垂防护）。
///
/// 资源 tile 的生命周期是「恒有效直到 unload」（`§11.8 R-12`）：unload 之后继续复用
/// 上一帧 lane 会把已回收的 tile（可能已被别的图复用）画出来。关窗释放资源后调用本函数，
/// 让**其它窗口**里引用被释放资源的图片节点当帧失效。返回标脏的节点数。
pub(crate) fn invalidate_dead(tree: &mut NodeTree, resources: &ResourceCache) -> usize {
    let nodes = collect(tree, |content| {
        content
            .source()
            .resource()
            .is_some_and(|id| resources.image(id).is_none())
    });
    nodes
        .into_iter()
        .filter(|node| tree.mark_paint(*node))
        .count()
}

/// 收集满足 `predicate` 的图片节点（DFS 先序；先收集再修改，避免同时借 `arena`）。
fn collect(tree: &NodeTree, predicate: impl Fn(&ImageContent) -> bool) -> Vec<NodeId> {
    let mut nodes = Vec::new();
    tree.for_each_dfs(|node| {
        if let Ok(target) = tree.get(node)
            && let Some(content) = target.kind.image()
            && predicate(content)
        {
            nodes.push(node);
        }
    });
    nodes
}

/// 资源交付就绪：改写来源 + 内在尺寸 + measure，并让本帧重排重绘。
fn apply_ready(tree: &mut NodeTree, node: NodeId, image: ResourceImage, scale: f32) -> bool {
    let intrinsic = intrinsic_of(image, scale);
    {
        let Some(target) = tree.arena.get_mut(node) else {
            return false;
        };
        let NodeKind::Image(content) = &mut target.kind else {
            return false;
        };
        content.set_source(ImageSource::Ready(image.id));
        content.set_intrinsic(intrinsic);
        target.measure = Some(Box::new(move |_known, _available| intrinsic));
    }
    tree.invalidate_caches_upwards(node);
    tree.mark_layout(node);
    tree.mark_paint(node);
    true
}

/// 资源内在尺寸的**逻辑像素**口径：tile 物理像素 ÷ 窗口 scale。
///
/// 与 `ResourceCache::insert_at_size` / `insert_for_display` 的两级缓存口径一致：
/// 应用按逻辑尺寸 × scale 取变体，引擎除以同一个 scale 还原布局尺寸；`insert` 的原始尺寸
/// 变体在 2× 屏上因此按「2× 资产」解释（1× 资产的逻辑尺寸应显式给 `Style` 尺寸）。
fn intrinsic_of(image: ResourceImage, scale: f32) -> Size {
    Size::new(image.width as f32 / scale, image.height as f32 / scale)
}

fn sane_scale(scale: f32) -> f32 {
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    }
}
