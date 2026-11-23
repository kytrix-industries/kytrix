use std::{any::Any, rc::Rc};

use smallvec::SmallVec;

use crate::{
    error::EngineError,
    node::{
        id::NodeId,
        signal::{
            Derived, DerivedId, DerivedStore, Signal, SignalId, SignalRef, SignalSlot, Transient,
        },
    },
    window::WindowId,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Owner {
    Node { window: WindowId, node: NodeId },
    Derived(DerivedId),
}

pub(crate) type OwnerSet = SmallVec<[Owner; 2]>;

#[derive(Default)]
pub(crate) struct DependencyGraph {
    dependents: ahash::HashMap<SignalId, OwnerSet>,
}

impl DependencyGraph {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn dependents_mut(&mut self, id: SignalId) -> Option<&mut OwnerSet> {
        self.dependents.get_mut(&id)
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn dependents(&self) -> &ahash::HashMap<SignalId, OwnerSet> {
        &self.dependents
    }

    pub(crate) fn replace_deps(&mut self, owner: Owner, old: &[SignalId], new: &[SignalId]) {
        for id in old {
            if new.contains(id) {
                continue;
            }
            let empty = match self.dependents.get_mut(id) {
                Some(set) => {
                    set.retain(|existing| *existing != owner);
                    set.is_empty()
                }
                None => false,
            };
            if empty {
                self.dependents.remove(id);
            }
        }
        for id in new {
            if old.contains(id) {
                continue;
            }
            let set = self.dependents.entry(*id).or_default();
            if !set.contains(&owner) {
                set.push(owner);
            }
        }
    }

    pub(crate) fn detach(&mut self, id: SignalId, owner: Owner) {
        let empty = match self.dependents.get_mut(&id) {
            Some(set) => {
                set.retain(|existing| *existing != owner);
                set.is_empty()
            }
            None => false,
        };
        if empty {
            self.dependents.remove(&id);
        }
    }

    pub(crate) fn node_reader_outside(
        &self,
        signals: &[SignalId],
        removed: &ahash::HashSet<NodeId>,
    ) -> Option<NodeId> {
        for signal in signals {
            let Some(owners) = self.dependents.get(signal) else {
                continue;
            };
            for owner in owners {
                if let Owner::Node { node, .. } = owner
                    && !removed.contains(node)
                {
                    return Some(*node);
                }
            }
        }
        None
    }

    pub(crate) fn remove_signal(&mut self, id: SignalId) -> Option<OwnerSet> {
        self.dependents.remove(&id)
    }

    #[cfg(test)]
    pub(crate) fn signal_count(&self) -> usize {
        self.dependents.len()
    }
}

pub(crate) struct ReadTracker {
    deps: SmallVec<[SignalId; 8]>,
}

impl ReadTracker {
    pub(crate) fn new() -> Self {
        Self {
            deps: SmallVec::new(),
        }
    }

    pub(crate) fn record(&mut self, id: SignalId) {
        if !self.deps.contains(&id) {
            self.deps.push(id);
        }
    }

    pub(crate) fn as_slice(&self) -> &[SignalId] {
        &self.deps
    }

    pub(crate) fn into_inner(self) -> SmallVec<[SignalId; 8]> {
        self.deps
    }
}

pub(crate) struct EvalStack {
    stack: SmallVec<[DerivedId; 8]>,
}

impl EvalStack {
    pub(crate) fn new() -> Self {
        Self {
            stack: SmallVec::new(),
        }
    }

    pub(crate) fn contains(&self, id: DerivedId) -> bool {
        self.stack.contains(&id)
    }

    pub(crate) fn push(&mut self, id: DerivedId) {
        self.stack.push(id);
    }

    pub(crate) fn pop(&mut self) -> Option<DerivedId> {
        self.stack.pop()
    }

    pub(crate) fn clear(&mut self) {
        self.stack.clear();
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }
}

pub(crate) struct EvalGuard<'a> {
    stack: &'a mut EvalStack,
    id: DerivedId,
}

impl<'a> EvalGuard<'a> {
    pub(crate) fn enter(stack: &'a mut EvalStack, id: DerivedId) -> Self {
        stack.push(id);
        Self { stack, id }
    }

    pub(crate) fn stack_mut(&mut self) -> &mut EvalStack {
        self.stack
    }
}

impl Drop for EvalGuard<'_> {
    fn drop(&mut self) {
        let popped = self.stack.pop();
        debug_assert_eq!(popped, Some(self.id), "eval stack is not LIFO");
    }
}

pub struct ReadCtx<'a> {
    values: &'a [SignalSlot],
    derived: &'a mut DerivedStore,
    graph: &'a mut DependencyGraph,
    tracker: &'a mut ReadTracker,
    eval_stack: &'a mut EvalStack,
    owner: Owner,
}

impl<'a> ReadCtx<'a> {
    pub(crate) fn new(
        values: &'a [SignalSlot],
        derived: &'a mut DerivedStore,
        graph: &'a mut DependencyGraph,
        tracker: &'a mut ReadTracker,
        eval_stack: &'a mut EvalStack,
        owner: Owner,
    ) -> Self {
        Self {
            values,
            derived,
            graph,
            tracker,
            eval_stack,
            owner,
        }
    }

    pub(crate) fn owner(&self) -> Owner {
        self.owner
    }

    pub fn read<T: 'static>(&mut self, signal: &Signal<T>) -> SignalRef<T> {
        let id = signal.id();
        let slot = self
            .values
            .get(id.index() as usize)
            .expect("signal id out of range: engine invariant broken")
            .live(id);
        self.tracker.record(id);
        slot.read_owned(id)
    }

    pub fn read_transient<T: 'static>(&mut self, transient: &Transient<T>) -> SignalRef<T> {
        let id = transient.id();
        self.slot(id).read_transient(id)
    }

    pub fn read_derived<T: 'static>(
        &mut self,
        derived: &Derived<T>,
    ) -> Result<SignalRef<T>, EngineError> {
        let id = derived.id();
        let handle = self.derived.handle(id);
        self.tracker.record(handle);
        if self.derived.is_dirty(id) {
            self.eval_derived(id)?;
        }
        Ok(self.derived.read_value::<T>(id))
    }

    #[cfg(test)]
    #[must_use]
    pub fn peek<T: 'static>(&self, signal: &Signal<T>) -> SignalRef<T> {
        self.slot(signal.id()).read_owned(signal.id())
    }

    fn slot(&self, id: SignalId) -> &SignalSlot {
        self.values
            .get(id.index() as usize)
            .expect("signal id out of range: engine invariant broken")
            .live(id)
    }

    fn eval_derived(&mut self, id: DerivedId) -> Result<(), EngineError> {
        if self.eval_stack.contains(id) {
            return Err(EngineError::CyclicDerived { derived: id });
        }
        let mut guard = EvalGuard::enter(&mut *self.eval_stack, id);

        let Some(compute) = self.derived.take_compute(id) else {
            debug_assert!(
                false,
                "derived compute closure is missing outside evaluation"
            );
            return Err(EngineError::CyclicDerived { derived: id });
        };

        let mut nested = ReadTracker::new();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut nested_ctx = ReadCtx {
                values: self.values,
                derived: &mut *self.derived,
                graph: &mut *self.graph,
                tracker: &mut nested,
                eval_stack: guard.stack_mut(),
                owner: Owner::Derived(id),
            };
            compute(&mut nested_ctx)
        }));
        self.derived.put_compute(id, compute);
        let value: Box<dyn Any> = match outcome {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        };

        self.derived.store_value(id, Rc::from(value));

        let old = self.derived.deps(id);
        self.graph
            .replace_deps(Owner::Derived(id), old, nested.as_slice());
        self.derived.set_deps(id, nested.into_inner());

        drop(guard);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{DependencyGraph, EvalStack, Owner, OwnerSet, ReadCtx, ReadTracker};
    use crate::{
        node::{
            id::NodeId,
            signal::{DerivedId, SignalId},
        },
        window::WindowId,
    };

    fn node_owner(index: u32) -> Owner {
        Owner::Node {
            window: WindowId::for_test(1),
            node: NodeId::for_test(index, 1),
        }
    }

    fn signal(index: u32) -> SignalId {
        SignalId::for_test(index)
    }

    #[test]
    fn read_tracker_dedupes_and_keeps_first_order() {
        let mut tracker = ReadTracker::new();
        tracker.record(signal(3));
        tracker.record(signal(1));
        tracker.record(signal(3));

        assert_eq!(tracker.as_slice(), &[signal(3), signal(1)]);
        assert_eq!(tracker.into_inner().len(), 2);
    }

    #[test]
    fn replace_deps_adds_new_edges() {
        let mut graph = DependencyGraph::new();
        let owner = node_owner(0);

        graph.replace_deps(owner, &[], &[signal(0), signal(1)]);

        assert_eq!(
            graph.dependents_mut(signal(0)).map(|set| set.as_slice()),
            Some([owner].as_slice())
        );
        assert_eq!(
            graph.dependents_mut(signal(1)).map(|set| set.as_slice()),
            Some([owner].as_slice())
        );
    }

    #[test]
    fn replace_deps_removes_missing_edges() {
        let mut graph = DependencyGraph::new();
        let owner = node_owner(0);

        graph.replace_deps(owner, &[], &[signal(0), signal(1)]);
        graph.replace_deps(owner, &[signal(0), signal(1)], &[signal(1)]);

        assert!(
            graph.dependents_mut(signal(0)).is_none(),
            "空集合必须删除表项"
        );
        assert!(graph.dependents_mut(signal(1)).is_some());
    }

    #[test]
    fn replace_deps_is_idempotent() {
        let mut graph = DependencyGraph::new();
        let owner = node_owner(0);

        graph.replace_deps(owner, &[], &[signal(0)]);
        graph.replace_deps(owner, &[signal(0)], &[signal(0)]);

        let set = graph.dependents_mut(signal(0)).expect("表项存在");
        assert_eq!(set.len(), 1, "同一 owner 只能有一份");
    }

    #[test]
    fn replace_deps_replaces_not_accumulates() {
        let mut graph = DependencyGraph::new();
        let owner = node_owner(0);

        graph.replace_deps(owner, &[], &[signal(0)]);
        graph.replace_deps(owner, &[signal(0)], &[signal(1)]);

        assert!(graph.dependents_mut(signal(0)).is_none());
        assert!(graph.dependents_mut(signal(1)).is_some());
    }

    #[test]
    fn detach_removes_edge_and_entry() {
        let mut graph = DependencyGraph::new();
        let owner = node_owner(0);
        graph.replace_deps(owner, &[], &[signal(0)]);

        graph.detach(signal(0), owner);

        assert!(
            graph.dependents_mut(signal(0)).is_none(),
            "最后一条边删除后表项必须消失"
        );
        assert!(graph.remove_signal(signal(0)).is_none());
    }

    #[test]
    fn owner_set_inline_capacity() {
        let mut set = OwnerSet::new();
        set.push(node_owner(0));
        set.push(node_owner(1));
        assert!(!set.spilled(), "2 个 owner 必须内联");
        set.push(node_owner(2));
        assert!(set.spilled(), "第 3 个 owner 才溢出到堆");
    }

    #[test]
    fn eval_stack_push_pop_and_contains() {
        let mut stack = EvalStack::new();
        let id = DerivedId::for_test(0);

        assert!(stack.is_empty());
        stack.push(id);
        assert!(stack.contains(id));
        assert!(!stack.is_empty());
        assert_eq!(stack.pop(), Some(id));
        assert!(stack.is_empty());
    }

    #[test]
    fn peek_reads_without_tracking() {
        let mut store = crate::node::signal::StateStore::with_capacity(1, 0);
        let id = store.push_owned(7u32);
        let (values, derived) = store.split();
        let graph = &mut DependencyGraph::new();
        let mut tracker = ReadTracker::new();
        let mut eval_stack = EvalStack::new();
        let signal = crate::node::signal::Signal::<u32>::new(id);
        let ctx = ReadCtx::new(
            values,
            derived,
            graph,
            &mut tracker,
            &mut eval_stack,
            node_owner(0),
        );

        assert_eq!(*ctx.peek(&signal), 7);
        assert!(tracker.as_slice().is_empty(), "peek 不得登记依赖");
    }
}
