use smallvec::SmallVec;

use super::{NodeKind, NodeTree};
use crate::{
    math::{DevicePixelRatio, DeviceSize, LogicalSize},
    node::{
        id::NodeId,
        image::{ImageContent, ImageSource, variant_target},
    },
    resources::{
        DecodeCause, ImageResourceKind, PendingResource, ResourceCache, ResourceId, ResourceImage,
        VariantState,
    },
};

#[derive(Default)]
pub(crate) struct PrepareImageVariants {
    pub created: usize,
    pub failures: SmallVec<[(ResourceId, LogicalSize, DecodeCause); 4]>,
    pub recovered: SmallVec<[ResourceId; 4]>,
}

fn measure_for(intrinsic: LogicalSize) -> crate::layout::MeasureFn {
    Box::new(move |_known, _available| intrinsic)
}

pub(crate) fn set_intrinsic(tree: &mut NodeTree, node: NodeId, intrinsic: LogicalSize) -> bool {
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
            target.measure = Some(measure_for(intrinsic));
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
    dpr: DevicePixelRatio,
) -> usize {
    let nodes: SmallVec<[NodeId; 8]> = tree.image_nodes().iter().copied().collect();
    let mut updates: SmallVec<[(NodeId, LogicalSize); 8]> = SmallVec::new();
    for node in nodes {
        let Ok(target) = tree.get(node) else {
            continue;
        };
        let Some(content) = target.kind.image() else {
            continue;
        };
        let Some(id) = content.source().resource() else {
            continue;
        };
        let Some(image) = resources.image(id) else {
            continue;
        };
        updates.push((node, image.intrinsic(dpr)));
    }
    updates
        .into_iter()
        .filter(|(node, intrinsic)| set_intrinsic(tree, *node, *intrinsic))
        .count()
}

pub(crate) fn resolve_pending(
    tree: &mut NodeTree,
    resolved: &ahash::HashMap<PendingResource, ResourceImage>,
    dpr: DevicePixelRatio,
) -> usize {
    if resolved.is_empty() {
        return 0;
    }
    let nodes: SmallVec<[NodeId; 8]> = tree.image_nodes().iter().copied().collect();
    let mut updates: SmallVec<[(NodeId, ResourceImage); 8]> = SmallVec::new();
    for node in nodes {
        let Ok(target) = tree.get(node) else {
            continue;
        };
        let Some(content) = target.kind.image() else {
            continue;
        };
        let ImageSource::Pending(pending) = content.source() else {
            continue;
        };
        if let Some(image) = resolved.get(&pending) {
            updates.push((node, *image));
        }
    }
    let mut count = 0;
    for (node, image) in updates {
        if apply_ready(tree, node, image, dpr) {
            count += 1;
        }
    }
    count
}

pub(crate) fn prepare_variants(
    tree: &mut NodeTree,
    resources: &mut ResourceCache,
    dpr: DevicePixelRatio,
) -> PrepareImageVariants {
    let nodes: SmallVec<[NodeId; 8]> = tree.image_nodes().iter().copied().collect();
    let mut changed: SmallVec<[NodeId; 8]> = SmallVec::new();
    let mut report = PrepareImageVariants::default();
    let mut candidates: SmallVec<
        [(NodeId, ResourceId, ResourceImage, LogicalSize, DeviceSize); 8],
    > = SmallVec::new();
    for node in nodes {
        let Ok(target) = tree.get(node) else {
            continue;
        };
        let NodeKind::Image(content) = target.kind() else {
            continue;
        };
        let Some(id) = content.source().resource() else {
            continue;
        };
        let Some(image) = resources.image(id) else {
            continue;
        };
        let intrinsic = image.intrinsic(dpr);
        let Some(target_size) =
            variant_target(target.layout().bounds, content.fit(), intrinsic, dpr)
        else {
            continue;
        };
        candidates.push((node, id, image, intrinsic, target_size));
    }
    for (node, id, image, intrinsic, target_size) in candidates {
        match resources.ensure_variant(image, target_size) {
            Some(VariantState::Ready { .. }) => {
                if !report.recovered.contains(&id) {
                    report.recovered.push(id);
                }
            }
            Some(VariantState::Failed { cause })
                if !report
                    .failures
                    .iter()
                    .any(|(resource, _, _)| *resource == id) =>
            {
                report.failures.push((id, intrinsic, cause));
            }
            Some(VariantState::Failed { .. }) => {}
            None => {}
        }
        if let Some(target) = tree.arena.get_mut(node) {
            let NodeKind::Image(content) = &mut target.kind else {
                continue;
            };
            if content.prepared_target() != Some(target_size) {
                content.set_prepared_target(target_size);
                changed.push(node);
            }
        }
    }
    for node in changed {
        if tree.mark_paint(node) {
            report.created += 1;
        }
    }
    report
}

pub(crate) fn mark_bitmap_paint(tree: &mut NodeTree, resources: &ResourceCache) -> usize {
    let nodes: SmallVec<[NodeId; 8]> = tree.image_nodes().iter().copied().collect();
    let mut count = 0;
    for node in nodes {
        let Ok(target) = tree.get(node) else {
            continue;
        };
        let Some(content) = target.kind.image() else {
            continue;
        };
        let Some(id) = content.source().resource() else {
            continue;
        };
        let bitmap = matches!(
            resources.image(id).map(ResourceImage::kind),
            Some(ImageResourceKind::Bitmap { .. })
        );
        if bitmap && tree.mark_paint(node) {
            count += 1;
        }
    }
    count
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
    for node in tree.image_nodes().iter().copied() {
        if let Ok(target) = tree.get(node)
            && let Some(content) = target.kind.image()
            && predicate(content)
        {
            nodes.push(node);
        }
    }
    nodes
}

fn apply_ready(
    tree: &mut NodeTree,
    node: NodeId,
    image: ResourceImage,
    dpr: DevicePixelRatio,
) -> bool {
    let intrinsic = image.intrinsic(dpr);
    {
        let Some(target) = tree.arena.get_mut(node) else {
            return false;
        };
        let NodeKind::Image(content) = &mut target.kind else {
            return false;
        };
        content.set_source(ImageSource::Ready(image.id()));
        content.set_intrinsic(intrinsic);
        target.measure = Some(measure_for(intrinsic));
    }
    tree.invalidate_caches_upwards(node);
    tree.mark_layout(node);
    tree.mark_paint(node);
    true
}
