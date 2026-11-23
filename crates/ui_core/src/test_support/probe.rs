use crate::{
    layout::hit_test::HitResult,
    math::{LogicalPoint, LogicalRect},
    node::{
        id::NodeId,
        tree::{NodeKind, NodeTree},
    },
    paint::scene::LaneId,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Selector {
    Id(&'static str),
    Text(&'static str),
    Path(&'static [&'static str]),
}

pub struct Probe<'a> {
    tree: &'a NodeTree,
}

impl<'a> Probe<'a> {
    #[must_use]
    pub fn new(tree: &'a NodeTree) -> Self {
        Self { tree }
    }

    #[must_use]
    pub fn by_selector(&self, selector: Selector) -> Option<NodeId> {
        match selector {
            Selector::Id(id) => self
                .tree
                .dfs_ids()
                .into_iter()
                .find(|node| self.tree.debug_id(*node) == Some(id)),
            Selector::Text(text) => self.tree.dfs_ids().into_iter().find(|node| {
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

    #[must_use]
    pub fn exists(&self, selector: Selector) -> bool {
        self.by_selector(selector).is_some()
    }

    #[must_use]
    pub fn count(&self, selector: Selector) -> usize {
        match selector {
            Selector::Id(id) => self
                .tree
                .dfs_ids()
                .into_iter()
                .filter(|node| self.tree.debug_id(*node) == Some(id))
                .count(),
            Selector::Text(text) => self
                .tree
                .dfs_ids()
                .into_iter()
                .filter(|node| {
                    self.tree.get(*node).is_ok_and(|target| {
                        matches!(target.kind(), NodeKind::Text(content) if content.text == text)
                    })
                })
                .count(),
            Selector::Path(_) => usize::from(self.exists(selector)),
        }
    }

    #[must_use]
    pub fn bounds(&self, node: NodeId) -> Option<LogicalRect> {
        self.tree
            .get(node)
            .ok()
            .map(|target| target.layout().bounds)
    }

    #[must_use]
    pub fn hit(&self, point: LogicalPoint) -> Option<HitResult> {
        self.tree.hit_test(point)
    }

    #[must_use]
    pub fn lane_of(&self, node: NodeId) -> Option<LaneId> {
        self.tree.lane_of(node)
    }
}
