use smallvec::SmallVec;

use crate::{
    error::EngineError,
    math::LogicalPoint,
    node::{id::NodeId, tree::NodeTree},
    popup::{
        FocusSnapshot, MAX_POPUP_DEPTH, MAX_POPUP_ENTRIES, PopupAnimationState, PopupDiagnostic,
        PopupDirty, PopupDismissHandler, PopupEntry, PopupError, PopupFocusState, PopupId,
        PopupStack, PopupState, PopupTier,
    },
    window::WindowId,
};

impl PopupStack {
    pub(crate) fn install(window: WindowId, root: NodeId) -> Self {
        Self {
            window,
            root,
            next_z: 1,
            entries: Vec::new(),
            generations: Vec::new(),
            free_slots: Vec::new(),
            order: Vec::new(),
            handlers: Vec::new(),
            dirty: PopupDirty::default(),
            viewport: crate::math::LogicalSize::ZERO,
            animations: Vec::new(),
            focus: PopupFocusState::default(),
            diagnostics: SmallVec::new(),
            max_sizes: SmallVec::new(),
        }
    }

    #[must_use]
    pub(crate) fn max_size_of(&self, id: PopupId) -> Option<crate::node::tree::MaxSize> {
        self.max_sizes
            .iter()
            .find(|(current, _)| *current == id)
            .map(|(_, max)| *max)
    }

    pub(crate) fn set_max_size(&mut self, id: PopupId, max: crate::node::tree::MaxSize) {
        if let Some(entry) = self
            .max_sizes
            .iter_mut()
            .find(|(current, _)| *current == id)
        {
            entry.1 = max;
        } else {
            self.max_sizes.push((id, max));
        }
    }

    pub(crate) fn forget_max_size(&mut self, id: PopupId) {
        self.max_sizes.retain(|(existing, _)| *existing != id);
    }

    #[must_use]
    pub fn viewport(&self) -> crate::math::LogicalSize {
        self.viewport
    }

    pub(crate) fn set_viewport(&mut self, viewport: crate::math::LogicalSize) -> bool {
        if self.viewport == viewport {
            return false;
        }
        self.viewport = viewport;
        self.mark(PopupDirty {
            geometry: true,
            order: false,
        });
        true
    }

    #[must_use]
    pub(crate) fn animations(&self) -> &[PopupAnimationState] {
        &self.animations
    }

    pub(crate) fn push_animation(&mut self, animation: PopupAnimationState) {
        self.animations
            .retain(|existing| existing.id != animation.id);
        self.animations.push(animation);
    }

    pub(crate) fn remove_animation(&mut self, id: PopupId) -> Option<PopupAnimationState> {
        let index = self.animations.iter().position(|state| state.id == id)?;
        Some(self.animations.remove(index))
    }

    #[must_use]
    pub(crate) fn animating(&self) -> bool {
        !self.animations.is_empty()
    }

    #[must_use]
    pub(crate) fn focus(&self) -> &PopupFocusState {
        &self.focus
    }

    pub(crate) fn focus_mut(&mut self) -> &mut PopupFocusState {
        &mut self.focus
    }

    #[must_use]
    pub(crate) fn diagnostics(&self) -> &[PopupDiagnostic] {
        &self.diagnostics
    }

    pub(crate) fn push_diagnostic(&mut self, diagnostic: PopupDiagnostic) {
        if !self.diagnostics.contains(&diagnostic) {
            self.diagnostics.push(diagnostic);
        }
    }

    pub(crate) fn clear_diagnostics(&mut self) {
        self.diagnostics.clear();
    }

    #[must_use]
    pub fn window(&self) -> WindowId {
        self.window
    }

    #[must_use]
    pub fn root(&self) -> NodeId {
        self.root
    }

    #[must_use]
    pub fn next_z(&self) -> u64 {
        self.next_z
    }

    #[must_use]
    pub fn order(&self) -> &[PopupId] {
        &self.order
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.order.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    #[must_use]
    pub fn dirty(&self) -> PopupDirty {
        self.dirty
    }

    pub fn entry(&self, id: PopupId) -> Result<&PopupEntry, PopupError> {
        let slot = self.resolve(id)?;
        self.entries[slot]
            .as_ref()
            .ok_or(PopupError::UnknownPopup { id })
    }

    pub(crate) fn entry_mut(&mut self, id: PopupId) -> Result<&mut PopupEntry, PopupError> {
        let slot = self.resolve(id)?;
        self.entries[slot]
            .as_mut()
            .ok_or(PopupError::UnknownPopup { id })
    }

    pub(crate) fn set_state(&mut self, id: PopupId, state: PopupState) -> Result<(), PopupError> {
        let entry = self.entry_mut(id)?;
        if entry.state == state {
            return Ok(());
        }
        entry.state = state;
        self.mark_dirty();
        Ok(())
    }

    pub(crate) fn mark(&mut self, dirty: PopupDirty) {
        self.dirty.geometry |= dirty.geometry;

        self.dirty.order |= dirty.order;
    }

    pub(crate) fn mark_dirty(&mut self) {
        self.mark(PopupDirty {
            geometry: true,
            order: true,
        });
    }

    pub(crate) fn take_dirty(&mut self) -> PopupDirty {
        std::mem::take(&mut self.dirty)
    }

    pub(crate) fn reserve_z(&mut self) -> u64 {
        let z = self.next_z;
        self.next_z = self.next_z.saturating_add(1);
        z
    }

    pub(crate) fn reserve_z_pair(&mut self) -> (u64, u64) {
        let scrim = self.next_z;
        let modal = self.next_z.saturating_add(1);
        self.next_z = modal.saturating_add(1);
        (scrim, modal)
    }

    pub(crate) fn raise(&mut self, id: PopupId) -> Result<u64, PopupError> {
        let entry = self.entry(id)?;
        let modal = entry.is_modal();
        let subtree = self.subtree(id)?;
        let mut group: SmallVec<[PopupId; 8]> = SmallVec::with_capacity(subtree.len() + 2);
        if modal && let Some(scrim) = self.scrim_of(id) {
            group.push(scrim);
        }
        group.extend(subtree.iter().copied());
        if modal {
            for descendant in self.owner_modal_descendants(id) {
                if let Some(scrim) = self.scrim_of(descendant)
                    && !group.contains(&scrim)
                {
                    group.push(scrim);
                }
                if !group.contains(&descendant) {
                    group.push(descendant);
                }
            }
        }
        let ordered: SmallVec<[PopupId; 8]> = self
            .order
            .iter()
            .copied()
            .filter(|current| group.contains(current))
            .collect();
        let base = self.next_z;
        let mut cursor = base;
        for current in ordered.iter().copied() {
            self.entry_mut(current)?.z = cursor;
            cursor = cursor.saturating_add(1);
        }
        self.next_z = cursor.max(base);
        self.sort_order();
        self.mark(PopupDirty {
            geometry: true,
            order: true,
        });
        Ok(base)
    }

    pub(crate) fn insert(
        &mut self,
        build: impl FnOnce(PopupId) -> PopupEntry,
    ) -> Result<PopupId, PopupError> {
        if self.len() >= MAX_POPUP_ENTRIES {
            return Err(PopupError::LimitReached {
                max: MAX_POPUP_ENTRIES,
            });
        }
        let slot = match self.free_slots.pop() {
            Some(slot) => slot,
            None => {
                if self.entries.len() >= MAX_POPUP_ENTRIES {
                    return Err(PopupError::LimitReached {
                        max: MAX_POPUP_ENTRIES,
                    });
                }
                self.entries.push(None);
                self.generations.push(0);
                self.handlers.push(None);
                u32::try_from(self.entries.len() - 1).map_err(|_| PopupError::LimitReached {
                    max: MAX_POPUP_ENTRIES,
                })?
            }
        };
        let generation = self.generations[slot as usize].wrapping_add(1);
        self.generations[slot as usize] = generation;
        let id = PopupId::new(self.window, slot, generation);
        let entry = build(id);
        self.next_z = self.next_z.max(entry.z.saturating_add(1));
        self.entries[slot as usize] = Some(entry);
        self.order.push(id);
        self.sort_order();
        self.mark_dirty();
        Ok(id)
    }

    pub(crate) fn remove(&mut self, id: PopupId) -> Result<PopupEntry, PopupError> {
        let slot = self.resolve(id)?;
        let entry = self.entries[slot]
            .take()
            .ok_or(PopupError::UnknownPopup { id })?;
        self.handlers[slot] = None;
        self.free_slots.push(id.slot());
        self.order.retain(|existing| *existing != id);
        self.animations.retain(|animation| animation.id != id);
        self.focus.records.retain(|(existing, _)| *existing != id);
        if self.focus.owner.is_some_and(|(owner, _)| owner == id) {
            self.focus.owner = None;
        }
        self.max_sizes.retain(|(existing, _)| *existing != id);
        for slot in self.entries.iter_mut().flatten() {
            if slot.parent == Some(id) {
                slot.parent = None;
                slot.owner_modal = None;
            }
            if slot.child == Some(id) {
                slot.child = None;
            }
            if slot.scrim_for == Some(id) {
                slot.scrim_for = None;
            }
        }
        self.mark_dirty();
        Ok(entry)
    }

    pub(crate) fn sort_order(&mut self) {
        let entries = &self.entries;
        self.order.sort_by_key(|id| {
            entries
                .get(id.slot() as usize)
                .and_then(|slot| slot.as_ref())
                .map_or(0, |entry| entry.z)
        });
    }

    pub(crate) fn sync_order(&mut self, tree: &mut NodeTree) -> Result<(), EngineError> {
        self.prune_missing(tree);
        let nodes: SmallVec<[NodeId; 8]> = self
            .order
            .iter()
            .filter_map(|id| self.entry(*id).ok().map(|entry| entry.node))
            .collect();
        tree.reorder_children(self.root, &nodes)
    }

    pub(crate) fn prune_missing(&mut self, tree: &NodeTree) {
        let missing: SmallVec<[PopupId; 4]> = self
            .order
            .iter()
            .copied()
            .filter(|id| {
                self.entry(*id).is_ok_and(|entry| {
                    entry.state != PopupState::Closed && tree.get(entry.node).is_err()
                })
            })
            .collect();
        for id in missing {
            let _ = self.remove(id);
        }
    }

    pub(crate) fn sync_rects(&mut self, tree: &NodeTree) {
        for slot in self.entries.iter_mut().flatten() {
            let Ok(target) = tree.get(slot.node) else {
                continue;
            };
            let bounds = target.layout().bounds;
            let painted = tree
                .painted_extent(slot.node)
                .map(|extent| extent.layout_bounds())
                .unwrap_or(bounds);
            slot.hit_rect = bounds;
            slot.paint_rect = painted.union(bounds);
            slot.a11y_rect = bounds;
        }
    }

    pub(crate) fn set_child(
        &mut self,
        parent: PopupId,
        child: Option<PopupId>,
    ) -> Result<(), PopupError> {
        self.entry_mut(parent)?.child = child;
        Ok(())
    }

    pub(crate) fn children(&self, parent: PopupId) -> SmallVec<[PopupId; 4]> {
        let mut children: SmallVec<[PopupId; 4]> = SmallVec::new();
        if let Some(entry) = self
            .entries
            .get(parent.slot() as usize)
            .and_then(Option::as_ref)
        {
            let mut child = entry.child;
            while let Some(current) = child {
                children.push(current);
                child = self.entry(current).ok().and_then(|entry| entry.child);
            }
        }
        children
    }

    pub(crate) fn chain(&self, id: PopupId) -> Result<SmallVec<[PopupId; 8]>, PopupError> {
        let mut chain: SmallVec<[PopupId; 8]> = SmallVec::new();
        let mut cursor = Some(id);
        while let Some(current) = cursor {
            if chain.contains(&current) {
                return Err(PopupError::Cycle { id: current });
            }
            if chain.len() >= MAX_POPUP_DEPTH {
                return Err(PopupError::DepthExceeded {
                    depth: chain.len() + 1,
                });
            }
            chain.push(current);
            cursor = self.entry(current)?.parent;
        }
        chain.reverse();
        Ok(chain)
    }

    pub(crate) fn subtree(&self, id: PopupId) -> Result<SmallVec<[PopupId; 8]>, PopupError> {
        let mut subtree: SmallVec<[PopupId; 8]> = SmallVec::new();
        subtree.push(id);
        let mut index = 0;
        while index < subtree.len() {
            let parent = subtree[index];
            for child in self.children(parent) {
                if !subtree.contains(&child) {
                    subtree.push(child);
                }
            }
            index += 1;
        }
        Ok(subtree)
    }

    pub(crate) fn nearest_modal(&self, parent: Option<PopupId>) -> Option<PopupId> {
        parent.and_then(|id| {
            self.entry(id)
                .ok()
                .and_then(|entry| entry.owner_modal.or(entry.is_modal().then_some(id)))
        })
    }

    pub(crate) fn enclosing(&self, tree: &NodeTree, node: NodeId) -> Option<PopupId> {
        let mut cursor = Some(node);
        while let Some(current) = cursor {
            if let Some(id) = self
                .order
                .iter()
                .copied()
                .find(|id| self.entry(*id).is_ok_and(|entry| entry.node == current))
            {
                return Some(id);
            }
            cursor = tree.get(current).ok().and_then(|target| target.parent());
        }
        None
    }

    pub(crate) fn scrim_of(&self, modal: PopupId) -> Option<PopupId> {
        self.order.iter().copied().find(|id| {
            self.entry(*id)
                .is_ok_and(|entry| entry.tier == PopupTier::Scrim && entry.scrim_for == Some(modal))
        })
    }

    pub(crate) fn topmost_dismissable(&self) -> Option<PopupId> {
        self.order.iter().rev().copied().find(|id| {
            self.entry(*id)
                .is_ok_and(|entry| entry.tier != PopupTier::Scrim && entry.state.is_dismissable())
        })
    }

    pub(crate) fn take_handler(&mut self, id: PopupId) -> Option<PopupDismissHandler> {
        let slot = self.resolve(id).ok()?;
        self.handlers.get_mut(slot)?.take()
    }

    pub(crate) fn restore_handler(&mut self, id: PopupId, handler: PopupDismissHandler) {
        if let Ok(slot) = self.resolve(id)
            && let Some(slot) = self.handlers.get_mut(slot)
        {
            *slot = Some(handler);
        }
    }

    pub(crate) fn set_handler(&mut self, id: PopupId, handler: PopupDismissHandler) {
        if let Ok(slot) = self.resolve(id)
            && let Some(slot) = self.handlers.get_mut(slot)
        {
            *slot = Some(handler);
        }
    }

    fn resolve(&self, id: PopupId) -> Result<usize, PopupError> {
        if id.window() != self.window {
            return Err(PopupError::UnknownPopup { id });
        }
        let slot = id.slot() as usize;
        if slot >= self.entries.len() {
            return Err(PopupError::UnknownPopup { id });
        }
        if self.generations[slot] != id.generation() {
            return Err(PopupError::StalePopup { id });
        }
        if self.entries[slot].is_none() {
            return Err(PopupError::UnknownPopup { id });
        }
        Ok(slot)
    }

    #[must_use]
    pub(crate) fn focus_owner(&self) -> Option<(PopupId, Option<NodeId>)> {
        self.focus().owner
    }

    pub(crate) fn set_focus_owner(&mut self, owner: Option<(PopupId, Option<NodeId>)>) {
        self.focus_mut().owner = owner;
    }

    pub(crate) fn set_owner_node(&mut self, id: PopupId, node: Option<NodeId>) {
        let focus = self.focus_mut();
        if let Some((owner, current)) = focus.owner.as_mut()
            && *owner == id
        {
            *current = node;
        }
    }

    pub(crate) fn push_focus_record(&mut self, id: PopupId, snapshot: FocusSnapshot) {
        let focus = self.focus_mut();
        focus.records.retain(|(existing, _)| *existing != id);
        focus.records.push((id, snapshot));
    }

    pub(crate) fn take_focus_record(&mut self, id: PopupId) -> Option<FocusSnapshot> {
        let focus = self.focus_mut();
        let index = focus
            .records
            .iter()
            .position(|(existing, _)| *existing == id)?;
        Some(focus.records.remove(index).1)
    }

    #[must_use]
    pub(crate) fn focus_record(&self, id: PopupId) -> Option<&FocusSnapshot> {
        self.focus()
            .records
            .iter()
            .find(|(existing, _)| *existing == id)
            .map(|(_, snapshot)| snapshot)
    }

    #[must_use]
    pub(crate) fn focus_record_count(&self) -> usize {
        self.focus().records.len()
    }

    #[must_use]
    pub(crate) fn topmost_modal(&self) -> Option<PopupId> {
        self.order().iter().rev().copied().find(|id| {
            self.entry(*id)
                .is_ok_and(|entry| entry.is_modal() && entry.state.is_visible())
        })
    }

    #[must_use]
    pub(crate) fn node_in_popup_scope(
        &self,
        tree: &NodeTree,
        node: NodeId,
        modal: PopupId,
    ) -> bool {
        let Some(enclosing) = self.enclosing(tree, node) else {
            return false;
        };
        let mut cursor = Some(enclosing);
        while let Some(id) = cursor {
            let Ok(entry) = self.entry(id) else {
                return false;
            };
            if id == modal || entry.owner_modal == Some(modal) {
                return true;
            }
            cursor = entry.parent;
        }
        false
    }

    #[must_use]
    pub(crate) fn node_in_focus_scope(
        &self,
        tree: &NodeTree,
        node: NodeId,
        owner: PopupId,
    ) -> bool {
        if self.entry(owner).is_err() {
            return false;
        }
        let Some(enclosing) = self.enclosing(tree, node) else {
            return false;
        };
        let mut cursor = Some(enclosing);
        while let Some(id) = cursor {
            if id == owner {
                return true;
            }
            let Ok(entry) = self.entry(id) else {
                return false;
            };
            cursor = entry.parent;
        }
        false
    }

    #[must_use]
    pub(crate) fn depth_ordered(&self) -> SmallVec<[PopupId; 8]> {
        let mut indexed: SmallVec<[(usize, PopupId); 8]> =
            SmallVec::with_capacity(self.order().len());
        for id in self.order().iter().copied() {
            let depth = self.chain(id).map_or(usize::MAX, |chain| chain.len());
            indexed.push((depth, id));
        }
        indexed.sort_by_key(|(depth, _)| *depth);
        indexed.into_iter().map(|(_, id)| id).collect()
    }

    #[must_use]
    pub(crate) fn popup_at(&self, point: LogicalPoint) -> Option<PopupId> {
        self.order().iter().rev().copied().find(|id| {
            self.entry(*id).is_ok_and(|entry| {
                entry.tier != PopupTier::Scrim
                    && entry.state.is_dismissable()
                    && entry.hit_rect.translate(entry.paint_offset).contains(point)
            })
        })
    }

    #[must_use]
    pub(crate) fn subtree_hit_contains(&self, id: PopupId, point: LogicalPoint) -> bool {
        self.subtree(id).is_ok_and(|subtree| {
            subtree.iter().any(|current| {
                self.entry(*current).is_ok_and(|entry| {
                    entry.tier != PopupTier::Scrim
                        && entry.hit_rect.translate(entry.paint_offset).contains(point)
                })
            })
        })
    }

    #[must_use]
    pub(crate) fn chain_root(&self, id: PopupId) -> Option<PopupId> {
        self.chain(id).ok().and_then(|chain| chain.first().copied())
    }

    #[must_use]
    pub(crate) fn auto_roots(&self, scope: Option<PopupId>) -> SmallVec<[PopupId; 4]> {
        self.order()
            .iter()
            .copied()
            .filter(|id| {
                self.entry(*id).is_ok_and(|entry| {
                    entry.parent.is_none()
                        && entry.tier == PopupTier::Auto
                        && entry.state.is_dismissable()
                        && entry.owner_modal == scope
                })
            })
            .collect()
    }

    #[must_use]
    pub(crate) fn opener_chain(&self, tree: &NodeTree, node: NodeId) -> Option<PopupId> {
        let mut cursor = Some(node);
        while let Some(current) = cursor {
            if let Some(id) = self.order().iter().rev().copied().find(|id| {
                self.entry(*id).is_ok_and(|entry| {
                    entry.parent.is_none()
                        && entry.tier == PopupTier::Auto
                        && entry.state.is_dismissable()
                        && entry.opener == Some(current)
                })
            }) {
                return Some(id);
            }
            cursor = tree.get(current).ok().and_then(|target| target.parent());
        }
        None
    }

    #[must_use]
    pub(crate) fn opener_chain_closing(&self, tree: &NodeTree, node: NodeId) -> Option<PopupId> {
        let mut cursor = Some(node);
        while let Some(current) = cursor {
            for id in self.order().iter().rev().copied() {
                let Ok(entry) = self.entry(id) else {
                    continue;
                };
                if entry.opener != Some(current) || entry.tier != PopupTier::Auto {
                    continue;
                }
                if matches!(
                    entry.state,
                    PopupState::Closing | PopupState::DismissPending
                ) {
                    return Some(id);
                }
            }
            cursor = tree.get(current).ok().and_then(|slot| slot.parent());
        }
        None
    }

    pub(crate) fn hint_topmost(&self) -> Option<PopupId> {
        self.order().iter().rev().copied().find(|id| {
            self.entry(*id)
                .is_ok_and(|entry| entry.tier == PopupTier::Hint && entry.state.is_dismissable())
        })
    }

    #[must_use]
    pub(crate) fn topmost_dismissable_non_scrim(&self) -> Option<PopupId> {
        self.order().iter().rev().copied().find(|id| {
            self.entry(*id)
                .is_ok_and(|entry| entry.tier != PopupTier::Scrim && entry.state.is_dismissable())
        })
    }

    #[must_use]
    pub(crate) fn owner_modal_descendants(&self, modal: PopupId) -> SmallVec<[PopupId; 8]> {
        let mut descendants: SmallVec<[PopupId; 8]> = SmallVec::new();
        let mut frontier: SmallVec<[PopupId; 8]> = SmallVec::new();
        frontier.push(modal);
        while let Some(parent) = frontier.pop() {
            for id in self.order().iter().copied() {
                let matches = self.entry(id).is_ok_and(|entry| {
                    entry.owner_modal == Some(parent) && entry.tier != PopupTier::Scrim
                });
                if matches && !descendants.contains(&id) && id != modal {
                    descendants.push(id);
                    frontier.push(id);
                }
            }
        }
        descendants
    }

    #[must_use]
    pub(crate) fn dismissable_entries(&self) -> SmallVec<[PopupId; 8]> {
        self.order()
            .iter()
            .copied()
            .filter(|id| {
                self.entry(*id)
                    .is_ok_and(|entry| entry.state.is_dismissable())
            })
            .collect()
    }
}
