use super::{NodeKind, NodeTree};
use crate::{
    math::Size,
    node::{
        id::NodeId,
        image::{ImageContent, ImageSource},
    },
    resources::{PendingResource, ResourceCache, ResourceId, ResourceImage},
};

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

pub(crate) fn refresh_for_scale(
    tree: &mut NodeTree,
    resources: &mut ResourceCache,
    scale: f32,
) -> Vec<ResourceId> {
    let scale = sane_scale(scale);
    let mut candidates: Vec<(NodeId, ResourceId)> = Vec::new();
    tree.for_each_dfs(|node| {
        if let Ok(target) = tree.get(node)
            && let Some(content) = target.kind.image()
            && let Some(id) = content.source().resource()
        {
            candidates.push((node, id));
        }
    });
    let mut applied = Vec::new();
    for (node, id) in candidates {
        let Some(image) = resources.reselect(id, scale) else {
            continue;
        };
        if image.id == id {
            continue;
        }
        if apply_ready(tree, node, image, scale) {
            applied.push(image.id);
        }
    }
    applied
}

pub(crate) fn set_resource(tree: &mut NodeTree, node: NodeId, resource: ResourceId) -> bool {
    {
        let Some(target) = tree.arena.get_mut(node) else {
            return false;
        };
        let NodeKind::Image(content) = &mut target.kind else {
            return false;
        };
        content.set_source(ImageSource::Ready(resource));
    }
    tree.images_need_refresh = true;
    tree.invalidate_caches_upwards(node);
    tree.mark_layout(node);
    tree.mark_paint(node);
    true
}

pub(crate) fn invalidate_with_resource(tree: &mut NodeTree, resource: ResourceId) -> usize {
    let nodes = collect(tree, |content| {
        content.source().resource() == Some(resource)
    });
    nodes
        .into_iter()
        .filter(|node| tree.mark_paint(*node))
        .count()
}

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
