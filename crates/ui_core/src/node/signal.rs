use std::{any::Any, fmt, marker::PhantomData, num::NonZeroU32, ops::Deref, rc::Rc};

use smallvec::SmallVec;

use crate::node::{dependency::ReadCtx, id::next_generation};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SignalId {
    index: u32,
    generation: NonZeroU32,
}

impl SignalId {
    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }

    #[must_use]
    pub const fn generation(self) -> u32 {
        self.generation.get()
    }

    #[must_use]
    pub const fn to_bits(self) -> u64 {
        ((self.generation.get() as u64) << 32) | self.index as u64
    }

    fn from_parts(index: u32, generation: u32) -> Self {
        let Some(generation) = NonZeroU32::new(generation) else {
            panic!("signal generation 0 is reserved for retired slots (engine invariant)");
        };
        Self { index, generation }
    }

    #[cfg(test)]
    pub(crate) const fn for_test(index: u32) -> Self {
        Self {
            index,
            generation: NonZeroU32::MIN,
        }
    }
}

impl fmt::Debug for SignalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SignalId(#{}.{})", self.index, self.generation)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DerivedId(u32);

impl DerivedId {
    #[must_use]
    pub const fn index(self) -> u32 {
        self.0
    }

    #[must_use]
    pub const fn to_bits(self) -> u64 {
        self.0 as u64
    }

    #[cfg(test)]
    pub(crate) const fn for_test(index: u32) -> Self {
        Self(index)
    }
}

pub struct Signal<T> {
    id: SignalId,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Clone for Signal<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Signal<T> {}

impl<T> std::fmt::Debug for Signal<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Signal(#{}.{})", self.id.index(), self.id.generation())
    }
}

impl<T> PartialEq for Signal<T> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<T> Eq for Signal<T> {}

impl<T> Signal<T> {
    pub(crate) const fn new(id: SignalId) -> Self {
        Self {
            id,
            _marker: PhantomData,
        }
    }

    pub(crate) const fn id(self) -> SignalId {
        self.id
    }
}

pub struct Derived<T> {
    id: DerivedId,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Clone for Derived<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Derived<T> {}

impl<T> std::fmt::Debug for Derived<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Derived(#{})", self.id.index())
    }
}

impl<T> Derived<T> {
    pub(crate) const fn new(id: DerivedId) -> Self {
        Self {
            id,
            _marker: PhantomData,
        }
    }

    pub(crate) const fn id(self) -> DerivedId {
        self.id
    }
}

pub struct Transient<T> {
    id: SignalId,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Clone for Transient<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Transient<T> {}

impl<T> std::fmt::Debug for Transient<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Transient(#{}.{})",
            self.id.index(),
            self.id.generation()
        )
    }
}

impl<T> Transient<T> {
    pub(crate) const fn new(id: SignalId) -> Self {
        Self {
            id,
            _marker: PhantomData,
        }
    }

    pub(crate) const fn id(self) -> SignalId {
        self.id
    }
}

pub struct SignalRef<T> {
    value: Rc<T>,
}

impl<T: 'static> SignalRef<T> {
    pub(crate) fn from_any(value: Rc<dyn Any>, id: SignalId) -> Self {
        match value.downcast::<T>() {
            Ok(value) => Self { value },
            Err(_) => panic!(
                "signal #{}.{}: store value type mismatch — engine invariant broken \
                 (SignalId ↔ T is a constructive bijection)",
                id.index(),
                id.generation()
            ),
        }
    }
}

impl<T> AsRef<T> for SignalRef<T> {
    fn as_ref(&self) -> &T {
        &self.value
    }
}

impl<T> Deref for SignalRef<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T: fmt::Debug> fmt::Debug for SignalRef<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SignalRef").field(&*self.value).finish()
    }
}

impl<T: fmt::Display> fmt::Display for SignalRef<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&*self.value, f)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SignalScope {
    index: u32,
}

impl SignalScope {
    pub(crate) const ROOT: Self = Self { index: 0 };

    pub(crate) const fn from_index(index: u32) -> Self {
        Self { index }
    }

    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }
}

pub(crate) enum SignalValue {
    Owned(Rc<dyn Any>),
    Derived(DerivedId),
    Transient(Rc<dyn Any>),
}

pub(crate) struct SignalSlot {
    value: Option<SignalValue>,
    generation: u32,
}

impl SignalSlot {
    #[inline]
    pub(crate) fn live(&self, id: SignalId) -> &Self {
        if self.generation != id.generation() {
            stale_handle_panic(id, self);
        }
        self
    }

    #[inline]
    fn expect_value(&self) -> &SignalValue {
        self.value
            .as_ref()
            .expect("live handle resolved to a free signal slot — engine invariant broken")
    }

    #[inline]
    pub(crate) fn owned_ref<T: 'static>(&self) -> Option<&T> {
        match self.value.as_ref()? {
            SignalValue::Owned(value) => value.downcast_ref::<T>(),
            SignalValue::Derived(_) | SignalValue::Transient(_) => None,
        }
    }

    pub(crate) fn transient_ref<T: 'static>(&self) -> Option<&T> {
        match self.value.as_ref()? {
            SignalValue::Transient(value) => value.downcast_ref::<T>(),
            SignalValue::Owned(_) | SignalValue::Derived(_) => None,
        }
    }

    pub(crate) fn read_owned<T: 'static>(&self, id: SignalId) -> SignalRef<T> {
        match self.expect_value() {
            SignalValue::Owned(value) => SignalRef::from_any(Rc::clone(value), id),
            SignalValue::Derived(derived) => panic!(
                "signal #{}: slot is the handle of derived {derived:?}, not a plain signal",
                id.index()
            ),
            SignalValue::Transient(_) => {
                panic!("signal #{}: slot is a transient value", id.index())
            }
        }
    }

    pub(crate) fn read_transient<T: 'static>(&self, id: SignalId) -> SignalRef<T> {
        match self.expect_value() {
            SignalValue::Transient(value) => SignalRef::from_any(Rc::clone(value), id),
            SignalValue::Owned(_) | SignalValue::Derived(_) => {
                panic!("signal #{}: slot is not a transient value", id.index())
            }
        }
    }

    #[inline]
    fn owned_slot_mut(&mut self, id: SignalId, operation: &'static str) -> &mut Rc<dyn Any> {
        match self.value.as_mut() {
            Some(SignalValue::Owned(existing)) => existing,
            Some(SignalValue::Derived(_) | SignalValue::Transient(_)) => panic!(
                "signal #{}: {operation}() targets a transient/derived slot — use the matching \
                 entry point",
                id.index()
            ),
            None => panic!(
                "signal #{}: {operation}() targets a free slot — engine invariant broken",
                id.index()
            ),
        }
    }

    #[inline]
    pub(crate) fn write_owned<T: 'static>(&mut self, id: SignalId, value: T) {
        let existing = self.owned_slot_mut(id, "write");
        let guards = Rc::strong_count(existing) - 1;
        if guards > 0 {
            panic!(
                "signal #{}: write while {guards} SignalRef read guard(s) are alive — \
                 the engine does not copy values on read, so an in-place write would alias them. \
                 Drop the guard(s) first (e.g. `let value = (*cx.read(&s)).clone();`)",
                id.index()
            );
        }
        if let Some(current) = Rc::get_mut(existing).and_then(|any| any.downcast_mut::<T>()) {
            *current = value;
            return;
        }
        *existing = Rc::new(value);
    }

    pub(crate) fn update_owned<T: 'static, R>(
        &mut self,
        id: SignalId,
        update: impl FnOnce(&mut T) -> R,
    ) -> R {
        let existing = self.owned_slot_mut(id, "update");
        let guards = Rc::strong_count(existing) - 1;
        if guards > 0 {
            panic!(
                "signal #{}: update while {guards} SignalRef read guard(s) are alive — \
                 drop the guard(s) first",
                id.index()
            );
        }
        match Rc::get_mut(existing).and_then(|any| any.downcast_mut::<T>()) {
            Some(current) => update(current),
            None => panic!(
                "signal #{}: value type mismatch — engine invariant broken",
                id.index()
            ),
        }
    }

    pub(crate) fn write_transient<T: 'static>(&mut self, id: SignalId, value: T) {
        let existing = match self.value.as_mut() {
            Some(SignalValue::Transient(existing)) => existing,
            Some(SignalValue::Owned(_) | SignalValue::Derived(_)) => panic!(
                "signal #{}: write_transient() targets a plain/derived slot — use the matching \
                 entry point",
                id.index()
            ),
            None => panic!(
                "signal #{}: write_transient() targets a free slot — engine invariant broken",
                id.index()
            ),
        };
        let guards = Rc::strong_count(existing) - 1;
        if guards > 0 {
            panic!(
                "signal #{}: transient write while {guards} SignalRef read guard(s) are alive",
                id.index()
            );
        }
        if let Some(current) = Rc::get_mut(existing).and_then(|any| any.downcast_mut::<T>()) {
            *current = value;
            return;
        }
        *existing = Rc::new(value);
    }
}

#[cold]
fn stale_handle_panic(id: SignalId, slot: &SignalSlot) -> ! {
    let state = match (slot.generation, slot.value.is_some()) {
        (0, _) => "permanently retired (its generation space is exhausted)",
        (_, false) => "free (its value was recycled when the owning scope was released)",
        (_, true) => "occupied by a newer signal (the slot was recycled and reused)",
    };
    panic!(
        "signal #{}.{}: handle is stale — slot #{} is {state}, slot generation {}. \
         A handle must not be used after its scope is released: the value was dropped, so reads \
         fail deterministically instead of returning the new occupant's value. Plain-signal writes \
         are no-ops (`App::write` returns false, `App::update_with` returns None); transient writes \
         (`App::write_transient`) panic like reads.",
        id.index(),
        id.generation(),
        id.index(),
        slot.generation,
    )
}

pub(crate) struct DerivedSlot {
    handle: SignalId,
    compute: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> Box<dyn Any>>>,
    cache: Option<Rc<dyn Any>>,
    dirty: bool,
    deps: SmallVec<[SignalId; 8]>,
}

pub(crate) struct DerivedStore {
    slots: Vec<DerivedSlot>,
}

impl DerivedStore {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            slots: Vec::with_capacity(capacity),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    pub(crate) fn push(
        &mut self,
        handle: SignalId,
        compute: Box<dyn Fn(&mut ReadCtx<'_>) -> Box<dyn Any>>,
    ) {
        self.slots.push(DerivedSlot {
            handle,
            compute: Some(compute),
            cache: None,
            dirty: true,
            deps: SmallVec::new(),
        });
    }

    pub(crate) fn is_dirty(&self, id: DerivedId) -> bool {
        self.slot(id).dirty
    }

    pub(crate) fn handle(&self, id: DerivedId) -> SignalId {
        self.slot(id).handle
    }

    pub(crate) fn take_compute(
        &mut self,
        id: DerivedId,
    ) -> Option<Box<dyn Fn(&mut ReadCtx<'_>) -> Box<dyn Any>>> {
        self.slot_mut(id).compute.take()
    }

    pub(crate) fn put_compute(
        &mut self,
        id: DerivedId,
        compute: Box<dyn Fn(&mut ReadCtx<'_>) -> Box<dyn Any>>,
    ) {
        self.slot_mut(id).compute = Some(compute);
    }

    pub(crate) fn store_value(&mut self, id: DerivedId, value: Rc<dyn Any>) {
        let slot = self.slot_mut(id);
        slot.cache = Some(value);
        slot.dirty = false;
    }

    pub(crate) fn read_value<T: 'static>(&self, id: DerivedId) -> SignalRef<T> {
        let slot = self.slot(id);
        let cache = slot
            .cache
            .as_ref()
            .expect("derived value has not been evaluated");
        SignalRef::from_any(Rc::clone(cache), slot.handle)
    }

    pub(crate) fn deps(&self, id: DerivedId) -> &[SignalId] {
        &self.slot(id).deps
    }

    pub(crate) fn set_deps(&mut self, id: DerivedId, deps: SmallVec<[SignalId; 8]>) {
        self.slot_mut(id).deps = deps;
    }

    pub(crate) fn mark_dirty(&mut self, id: DerivedId) -> bool {
        let slot = self.slot_mut(id);
        if slot.dirty {
            false
        } else {
            slot.dirty = true;
            true
        }
    }

    fn slot(&self, id: DerivedId) -> &DerivedSlot {
        self.slots
            .get(id.index() as usize)
            .expect("derived id out of range: engine invariant broken")
    }

    fn slot_mut(&mut self, id: DerivedId) -> &mut DerivedSlot {
        self.slots
            .get_mut(id.index() as usize)
            .expect("derived id out of range: engine invariant broken")
    }
}

pub(crate) struct StateStore {
    values: Vec<SignalSlot>,
    free_slots: Vec<u32>,
    derived: DerivedStore,
    scopes: Vec<ScopeSlot>,
}

struct ScopeSlot {
    released: bool,
    signals: Vec<SignalId>,
}

impl StateStore {
    pub(crate) fn with_capacity(signals: usize, derived: usize) -> Self {
        Self {
            values: Vec::with_capacity(signals),
            free_slots: Vec::new(),
            derived: DerivedStore::with_capacity(derived),
            scopes: vec![ScopeSlot {
                released: false,
                signals: Vec::new(),
            }],
        }
    }

    pub(crate) fn split(&mut self) -> (&[SignalSlot], &mut DerivedStore) {
        (&self.values, &mut self.derived)
    }

    pub(crate) fn new_scope(&mut self) -> SignalScope {
        let index = u32::try_from(self.scopes.len()).expect("信号作用域溢出 u32");
        self.scopes.push(ScopeSlot {
            released: false,
            signals: Vec::new(),
        });
        SignalScope::from_index(index)
    }

    pub(crate) fn scope_signals(&self, scope: SignalScope) -> &[SignalId] {
        self.scopes
            .get(scope.index() as usize)
            .filter(|slot| !slot.released)
            .map_or(&[], |slot| slot.signals.as_slice())
    }

    pub(crate) fn release_scope(&mut self, scope: SignalScope) -> Option<Vec<SignalId>> {
        if scope.index() == SignalScope::ROOT.index() {
            return None;
        }
        let scope_slot = self.scopes.get_mut(scope.index() as usize)?;
        if scope_slot.released {
            return Some(Vec::new());
        }
        scope_slot.released = true;
        let signals = std::mem::take(&mut scope_slot.signals);
        for id in &signals {
            let slot = self
                .values
                .get_mut(id.index() as usize)
                .expect("released signal id out of range: engine invariant broken");
            debug_assert_eq!(
                slot.generation,
                id.generation(),
                "released id must be the live occupant of its slot"
            );
            slot.value = None;
            match next_generation(slot.generation) {
                Some(next) => {
                    slot.generation = next;
                    self.free_slots.push(id.index());
                }
                None => slot.generation = 0,
            }
        }
        Some(signals)
    }

    pub(crate) fn push_owned<T: 'static>(&mut self, value: T) -> SignalId {
        self.push_owned_scoped(SignalScope::ROOT, value)
    }

    pub(crate) fn push_owned_scoped<T: 'static>(
        &mut self,
        scope: SignalScope,
        value: T,
    ) -> SignalId {
        self.push_slot(SignalValue::Owned(Rc::new(value)), scope)
    }

    pub(crate) fn push_transient<T: 'static>(&mut self, value: T) -> SignalId {
        self.push_transient_scoped(SignalScope::ROOT, value)
    }

    pub(crate) fn push_transient_scoped<T: 'static>(
        &mut self,
        scope: SignalScope,
        value: T,
    ) -> SignalId {
        self.push_slot(SignalValue::Transient(Rc::new(value)), scope)
    }

    pub(crate) fn push_derived(
        &mut self,
        compute: Box<dyn Fn(&mut ReadCtx<'_>) -> Box<dyn Any>>,
    ) -> (DerivedId, SignalId) {
        let derived_id = DerivedId(u32::try_from(self.derived.len()).expect("派生槽位溢出 u32"));
        let handle = self.push_slot(SignalValue::Derived(derived_id), SignalScope::ROOT);
        self.derived.push(handle, compute);
        (derived_id, handle)
    }

    #[cfg(test)]
    pub(crate) fn set_owned<T: 'static>(&mut self, id: SignalId, value: T) {
        self.live_slot_mut(id).write_owned(id, value);
    }

    #[inline]
    pub(crate) fn write_owned<T: PartialEq + 'static>(
        &mut self,
        id: SignalId,
        value: T,
    ) -> Option<bool> {
        let slot = self
            .values
            .get_mut(id.index() as usize)
            .expect("signal id out of range: engine invariant broken");
        if slot.generation != id.generation() {
            return None;
        }
        if slot.owned_ref::<T>() == Some(&value) {
            return Some(false);
        }
        slot.write_owned(id, value);
        Some(true)
    }

    pub(crate) fn update_owned<T: 'static, R>(
        &mut self,
        id: SignalId,
        update: impl FnOnce(&mut T) -> R,
    ) -> Option<R> {
        let slot = self
            .values
            .get_mut(id.index() as usize)
            .expect("signal id out of range: engine invariant broken");
        if slot.generation != id.generation() {
            return None;
        }
        Some(slot.update_owned(id, update))
    }

    pub(crate) fn set_transient<T: 'static>(&mut self, id: SignalId, value: T) {
        self.live_slot_mut(id).write_transient(id, value);
    }

    pub(crate) fn read_owned<T: 'static>(&self, id: SignalId) -> SignalRef<T> {
        self.live_slot(id).read_owned(id)
    }

    pub(crate) fn read_transient<T: 'static>(&self, id: SignalId) -> SignalRef<T> {
        self.live_slot(id).read_transient(id)
    }

    #[cfg(test)]
    pub(crate) fn owned_ref<T: 'static>(&self, id: SignalId) -> Option<&T> {
        self.slot_if_live(id)?.owned_ref::<T>()
    }

    pub(crate) fn transient_ref<T: 'static>(&self, id: SignalId) -> Option<&T> {
        self.slot_if_live(id)?.transient_ref::<T>()
    }

    #[inline]
    pub(crate) fn is_live(&self, id: SignalId) -> bool {
        self.slot_if_live(id).is_some()
    }

    pub(crate) fn mark_derived_dirty(&mut self, id: DerivedId) -> bool {
        self.derived.mark_dirty(id)
    }

    pub(crate) fn derived_handle(&self, id: DerivedId) -> SignalId {
        self.derived.handle(id)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn slot_count_for_test(&self) -> usize {
        self.values.len()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn free_slot_count_for_test(&self) -> usize {
        self.free_slots.len()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn live_slot_count_for_test(&self) -> usize {
        self.values
            .iter()
            .filter(|slot| slot.value.is_some())
            .count()
    }

    #[cfg(test)]
    pub(crate) fn force_generation_for_test(&mut self, old: SignalId, generation: u32) -> SignalId {
        let slot = self
            .values
            .get_mut(old.index() as usize)
            .expect("test handle must be alive");
        slot.generation = generation;
        let fresh = SignalId::from_parts(old.index(), generation);
        for scope in &mut self.scopes {
            for id in &mut scope.signals {
                if *id == old {
                    *id = fresh;
                }
            }
        }
        fresh
    }

    fn slot_if_live(&self, id: SignalId) -> Option<&SignalSlot> {
        let slot = self.values.get(id.index() as usize)?;
        (slot.generation == id.generation()).then_some(slot)
    }

    fn live_slot(&self, id: SignalId) -> &SignalSlot {
        self.values
            .get(id.index() as usize)
            .expect("signal id out of range: engine invariant broken")
            .live(id)
    }

    fn live_slot_mut(&mut self, id: SignalId) -> &mut SignalSlot {
        let slot = self
            .values
            .get_mut(id.index() as usize)
            .expect("signal id out of range: engine invariant broken");
        if slot.generation != id.generation() {
            stale_handle_panic(id, slot);
        }
        slot
    }

    fn push_slot(&mut self, value: SignalValue, scope: SignalScope) -> SignalId {
        let scope_slot = self
            .scopes
            .get_mut(scope.index() as usize)
            .unwrap_or_else(|| {
                panic!(
                    "signal scope #{} does not exist — engine invariant broken",
                    scope.index()
                )
            });
        if scope_slot.released {
            panic!(
                "signal scope #{}: released scopes cannot mint new signals — its values were \
                 recycled by `release_scope`, so a new value here would leak forever",
                scope.index()
            );
        }
        let (index, generation) = match self.free_slots.pop() {
            Some(index) => {
                let generation = self.values[index as usize].generation;
                debug_assert!(generation > 0, "free list must never hold a retired slot");
                (index, generation)
            }
            None => {
                let index = u32::try_from(self.values.len()).expect("信号槽位溢出 u32");
                self.values.push(SignalSlot {
                    value: None,
                    generation: 1,
                });
                (index, 1)
            }
        };
        self.values[index as usize].value = Some(value);
        let id = SignalId::from_parts(index, generation);
        scope_slot.signals.push(id);
        id
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        marker::PhantomData,
        mem::size_of,
        rc::Rc,
    };

    use super::{Derived, Signal, SignalId, SignalRef, SignalScope, StateStore, Transient};
    use crate::{
        app::App,
        error::EngineError,
        node::{
            builder::NodeBuilder,
            dependency::{DependencyGraph, Owner, ReadCtx},
            dirty::DirtyFlags,
            id::NodeId,
        },
        paint::style::StylePatch,
        window::{WindowId, WindowOptions},
    };

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime")
    }

    fn new_app() -> (tokio::runtime::Runtime, App) {
        let rt = runtime();
        let app = App::new(rt.handle().clone());
        (rt, app)
    }

    fn build_node<F>(app: &mut App, window: WindowId, bind: F) -> NodeId
    where
        F: Fn(&mut ReadCtx<'_>) -> StylePatch + 'static,
    {
        let w = app.window_mut(window).expect("窗口存在");
        let root = w.tree().root();
        NodeBuilder::boxed()
            .bind_style(bind)
            .build(w.tree_mut(), root)
            .expect("挂载成功")
    }

    fn dirty(app: &App, window: WindowId, node: NodeId) -> DirtyFlags {
        app.window(window)
            .expect("窗口存在")
            .tree()
            .get(node)
            .expect("节点存在")
            .dirty()
    }

    fn deps(app: &App, window: WindowId, node: NodeId) -> Vec<SignalId> {
        app.window(window)
            .expect("窗口存在")
            .tree()
            .deps(node)
            .expect("节点存在")
            .to_vec()
    }

    #[test]
    fn signal_id_is_a_dense_slot_index_plus_generation() {
        let (_rt, mut app) = new_app();
        let a: Signal<u32> = app.signal(1);
        let b: Signal<u32> = app.signal(2);
        let c: Signal<u32> = app.signal(3);

        assert_eq!(
            (a.id().index(), b.id().index(), c.id().index()),
            (0, 1, 2),
            "SignalId 是稠密槽位下标（根作用域不回收 ⇒ 只增）"
        );
        assert_eq!(
            (
                a.id().generation(),
                b.id().generation(),
                c.id().generation()
            ),
            (1, 1, 1),
            "新槽位的代际从 1 开始（0 只属于退休槽位）"
        );
        assert_ne!(a.id(), b.id());
        assert_eq!(
            a.id().to_bits(),
            1u64 << 32,
            "to_bits = (代际 << 32) | 下标"
        );
        assert_eq!(c.id().to_bits(), (1u64 << 32) | 2);
        assert_eq!(size_of::<SignalId>(), 8, "槽位下标 + 代际");
        assert_eq!(
            size_of::<Option<SignalId>>(),
            8,
            "NonZeroU32 的 niche 让 Option 不额外膨胀"
        );
    }

    #[test]
    fn signal_handle_is_copy_without_t_bound() {
        struct NotCopy;

        let signal = Signal::<NotCopy> {
            id: SignalId::for_test(7),
            _marker: PhantomData,
        };
        let copy = signal;
        let _ = signal;
        assert_eq!(copy.id().index(), 7, "Copy 与 T: Copy 无关");
        assert_eq!(copy.id().generation(), 1);
    }

    #[test]
    fn rewriting_an_owned_value_reuses_its_allocation() {
        let mut store = StateStore::with_capacity(1, 0);
        let id = store.push_owned(String::from("first"));
        let before = store.owned_ref::<String>(id).expect("value") as *const String;
        store.set_owned(id, String::from("second-longer-than-inline"));
        let after = store.owned_ref::<String>(id).expect("value") as *const String;
        assert_eq!(before, after, "同类型写入必须复用已有分配");
        assert_eq!(
            store.owned_ref::<String>(id).expect("value"),
            "second-longer-than-inline"
        );
    }

    #[test]
    fn read_returns_shared_guard_without_copying() {
        let mut store = StateStore::with_capacity(1, 0);
        let id = store.push_owned(String::from("price"));

        let first = store.read_owned::<String>(id);
        let second = store.read_owned::<String>(id);

        assert!(
            std::ptr::eq(first.as_ref(), second.as_ref()),
            "读必须共享同一份值（零拷贝）"
        );
        assert_eq!(&*first, "price");
    }

    #[test]
    fn write_equal_value_short_circuits() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(7);
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        assert!(!dirty(&app, window, node).contains(DirtyFlags::BIND));

        assert!(!app.write(&signal, 7), "等值写入必须短路");
        assert!(!dirty(&app, window, node).contains(DirtyFlags::BIND));
        assert!(!app.take_wake_request(), "等值写入不得请求唤醒");
    }

    #[test]
    fn write_changed_value_invalidates() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(0);
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        assert_eq!(deps(&app, window, node), vec![signal.id()]);

        assert!(app.write(&signal, 5), "值变化必须返回 true");
        assert!(
            dirty(&app, window, node).contains(DirtyFlags::PAINT),
            "读者必须被标 PAINT"
        );
        assert!(app.take_wake_request(), "有节点被标脏 → 必须请求唤醒");
    }

    #[test]
    fn equal_but_interior_mutable_value_is_not_repainted() {
        let (_rt, mut app) = new_app();
        let shared = Rc::new(Cell::new(1u32));
        let signal: Signal<Rc<Cell<u32>>> = app.signal(Rc::clone(&shared));
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(cx.read(&signal).get() as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        app.window_mut(window)
            .expect("窗口存在")
            .tree_mut()
            .clear_all_dirty_for_test();
        let _ = app.take_wake_request();

        shared.set(2);
        assert!(
            !app.write(&signal, Rc::clone(&shared)),
            "PartialEq 相等 ⇒ 引擎无法感知内部变化（应用层必须用不可变值语义）"
        );
        assert!(
            !dirty(&app, window, node).contains(DirtyFlags::PAINT),
            "短路意味着**不重绘** —— 这就是 R9 的风险"
        );
        assert!(!app.take_wake_request(), "短路不得请求唤醒");
    }

    #[test]
    fn transient_write_never_invalidates() {
        let (_rt, mut app) = new_app();
        let transient: Transient<f32> = app.transient(0.0);
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read_transient(&transient)),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");

        let before = dirty(&app, window, node);
        app.write_transient(&transient, 1.0);
        assert_eq!(dirty(&app, window, node), before, "transient 写不得标脏");
        assert!(!app.take_wake_request(), "transient 写不得请求唤醒");
    }

    #[test]
    fn transient_read_is_not_tracked() {
        let (_rt, mut app) = new_app();
        let transient: Transient<u32> = app.transient(0);
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read_transient(&transient) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        assert!(
            deps(&app, window, node).is_empty(),
            "transient 读不得登记依赖"
        );
    }

    #[test]
    fn derived_is_lazy() {
        let calls = Rc::new(Cell::new(0u32));
        let counted = Rc::clone(&calls);
        let (_rt, mut app) = new_app();
        let derived: Derived<u32> = app.derived(move |_cx| {
            counted.set(counted.get() + 1);
            42
        });

        assert_eq!(calls.get(), 0, "注册时不得求值");
        assert_eq!(derived.id().index(), 0);
    }

    #[test]
    fn derived_caches_until_invalidated() {
        let calls = Rc::new(Cell::new(0u32));
        let counted = Rc::clone(&calls);
        let (_rt, mut app) = new_app();
        let source: Signal<u32> = app.signal(1);
        let derived: Derived<u32> = app.derived(move |cx| {
            counted.set(counted.get() + 1);
            *cx.read(&source) * 2
        });
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read_derived(&derived).expect("无环") as f32),
            ..StylePatch::default()
        });

        app.refresh_styles(window).expect("窗口存在");
        assert_eq!(calls.get(), 1, "首次读必须求值一次");
        app.refresh_styles(window).expect("窗口存在");
        assert_eq!(calls.get(), 1, "未失效时命中缓存");

        assert!(app.write(&source, 3));
        assert!(dirty(&app, window, node).contains(DirtyFlags::PAINT));
        app.refresh_styles(window).expect("窗口存在");
        assert_eq!(calls.get(), 2, "失效后必须重算");
    }

    #[test]
    fn derived_chain_propagates_invalidation() {
        let (_rt, mut app) = new_app();
        let source: Signal<u32> = app.signal(2);
        let inner: Derived<u32> = app.derived(move |cx| *cx.read(&source) + 1);
        let outer: Derived<u32> =
            app.derived(move |cx| *cx.read_derived(&inner).expect("无环") * 10);
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read_derived(&outer).expect("无环") as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");

        let node_deps = deps(&app, window, node);
        assert_eq!(node_deps.len(), 1);
        assert_ne!(node_deps[0], source.id(), "节点依赖的是派生的句柄 id");

        assert!(app.write(&source, 9));
        assert!(dirty(&app, window, node).contains(DirtyFlags::PAINT));
    }

    #[test]
    fn derived_cycle_is_reported_not_panicking() {
        let (_rt, mut app) = new_app();
        let unrelated: Signal<u32> = app.signal(7);
        let slot: Rc<RefCell<Option<Derived<u32>>>> = Rc::new(RefCell::new(None));
        let seen = Rc::new(Cell::new(false));

        let observed = Rc::clone(&seen);
        let alias = Rc::clone(&slot);
        let a: Derived<u32> = app.derived(move |cx| {
            let borrowed = alias.borrow();
            match borrowed.as_ref() {
                Some(derived) => match cx.read_derived(derived) {
                    Ok(value) => *value,
                    Err(EngineError::CyclicDerived { .. }) => {
                        observed.set(true);
                        0
                    }
                    Err(_) => 0,
                },
                None => 0,
            }
        });
        *slot.borrow_mut() = Some(a);

        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read_derived(&a).expect("外层不得 panic") as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");

        assert!(seen.get(), "自引必须被 EvalStack 检出并上报 CyclicDerived");
        let node_deps = deps(&app, window, node);
        assert_eq!(node_deps.len(), 1, "节点只依赖派生的**句柄** id");
        assert_ne!(
            node_deps[0],
            unrelated.id(),
            "节点没读过那个普通信号 ⇒ 它不能出现在依赖里（句柄 id 与信号 id 是同一 id 空间）"
        );
        assert!(!app.take_wake_request());
    }

    #[test]
    fn split_borrow_returns_disjoint_views() {
        let mut store = StateStore::with_capacity(2, 1);
        let id = store.push_owned(1u32);
        let (values, derived) = store.split();

        assert_eq!(values[id.index() as usize].owned_ref::<u32>(), Some(&1));
        assert_eq!(derived.len(), 0);
    }

    fn panic_payload_message(payload: Box<dyn std::any::Any + Send>) -> String {
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| {
                payload
                    .downcast_ref::<&'static str>()
                    .map(|text| (*text).to_owned())
            })
            .unwrap_or_default()
    }

    #[test]
    fn read_guard_shares_the_value_instead_of_copying() {
        let (_rt, mut app) = new_app();
        let signal: Signal<String> = app.signal(String::from("BTC"));

        let first = app.read(&signal);
        let second = app.read(&signal);

        assert!(
            std::ptr::eq(first.as_ref(), second.as_ref()),
            "两个读守卫必须指向同一份值（读路径零拷贝）"
        );
        assert_eq!(&*first, "BTC");
        drop((first, second));
        assert!(app.write(&signal, String::from("ETH")), "守卫释放后可写");
    }

    #[test]
    fn write_while_read_guard_is_alive_panics_with_diagnostic() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(1);

        let guard = app.read(&signal);
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            app.write(&signal, 2);
        }))
        .expect_err("读守卫存活期间写同一信号必须 panic");
        let message = panic_payload_message(payload);

        assert!(
            message.contains("SignalRef") || message.contains("read guard"),
            "panic 必须可诊断（指出读守卫借用冲突）：{message}"
        );
        assert!(
            message.contains(&format!("#{}", signal.id().index())),
            "panic 必须带 SignalId：{message}"
        );
        assert_eq!(*guard, 1, "冲突的写不得生效（值保持原样）");
        drop(guard);

        assert!(app.write(&signal, 3), "守卫释放后写必须成功");
        assert_eq!(*app.read(&signal), 3);
    }

    #[test]
    fn read_guard_does_not_block_other_signals() {
        let (_rt, mut app) = new_app();
        let read: Signal<u32> = app.signal(1);
        let written: Signal<u32> = app.signal(10);

        let guard = app.read(&read);
        assert!(app.write(&written, 20), "读 A 不得阻止写 B（粒度是单信号）");
        assert_eq!(*guard, 1);
        assert_eq!(*app.read(&written), 20);
    }

    #[test]
    fn write_after_guard_drop_succeeds_and_invalidates() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(0);
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        {
            let guard = app.read(&signal);
            assert_eq!(*guard, 0);
        }
        assert!(app.write(&signal, 7));
        assert!(dirty(&app, window, node).contains(DirtyFlags::PAINT));
    }

    #[test]
    fn transient_guard_blocks_transient_write() {
        let (_rt, mut app) = new_app();
        let transient: Transient<f32> = app.transient(0.5);
        let guard = app.read_transient(&transient);
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            app.write_transient(&transient, 1.0);
        }))
        .expect_err("瞬态值的读守卫同样阻止写");
        assert!(panic_payload_message(payload).contains("read guard"));
        assert!((*guard - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn update_mutates_in_place_without_equality_short_circuit() {
        let (_rt, mut app) = new_app();
        let signal: Signal<Vec<u32>> = app.signal(vec![1, 2, 3]);
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some((*cx.read(&signal)).len() as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        let before = app.read(&signal).as_ref() as *const Vec<u32> as usize;

        assert!(app.update(&signal, |values| values.push(4)));
        let after = app.read(&signal).as_ref() as *const Vec<u32> as usize;
        assert_eq!(before, after, "update 必须原地改（不换分配）");
        assert_eq!(*app.read(&signal), vec![1, 2, 3, 4]);
        assert!(
            dirty(&app, window, node).contains(DirtyFlags::PAINT),
            "update 无条件失效（无等值短路）"
        );
    }

    #[test]
    fn derived_read_is_a_snapshot_and_does_not_block_writes() {
        let (_rt, mut app) = new_app();
        let source: Signal<u32> = app.signal(1);
        let derived: Derived<u32> = app.derived(move |cx| *cx.read(&source) * 10);
        let captured: Rc<RefCell<Option<SignalRef<u32>>>> = Rc::new(RefCell::new(None));
        let store = Rc::clone(&captured);
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| {
            *store.borrow_mut() = Some(cx.read_derived(&derived).expect("无环"));
            StylePatch {
                opacity: Some(*cx.read(&source) as f32),
                ..StylePatch::default()
            }
        });
        app.refresh_styles(window).expect("窗口存在");
        let snapshot = captured.borrow_mut().take().expect("闭包存下了守卫");
        assert_eq!(*snapshot, 10, "派生值 = source × 10（首轮）");

        assert!(app.write(&source, 2), "派生读快照不阻止写上游");
        app.refresh_styles(window).expect("窗口存在");
        assert_eq!(
            *snapshot, 10,
            "旧快照保持旧值（快照语义，不失效、不 panic）"
        );
        assert!(
            !dirty(&app, window, node).is_empty(),
            "上游变化沿派生链失效读者"
        );
        drop(snapshot);

        let fresh = {
            app.write(&source, 3);
            app.refresh_styles(window).expect("窗口存在");
            captured.borrow_mut().take().expect("闭包再次存下守卫")
        };
        assert_eq!(*fresh, 30, "新快照反映新上游值");
    }

    #[test]
    fn batch_coalesces_n_writes_into_one_frame_request() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(0);
        let window = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        let _ = app.take_wake_request();

        let unbatched_start = app.frame_requests();
        for value in 1..=5 {
            assert!(app.write(&signal, value));
        }
        assert_eq!(
            app.frame_requests() - unbatched_start,
            5,
            "不批处理时 N 次写 ⇒ N 次帧请求"
        );
        let _ = app.take_wake_request();

        let batched_start = app.frame_requests();
        app.batch(|app| {
            for value in 6..=10 {
                assert!(app.write(&signal, value));
            }
            assert!(
                !app.take_wake_request(),
                "批内中间状态不得对外可见（迟迟不置唤醒位）"
            );
        });
        assert_eq!(
            app.frame_requests() - batched_start,
            1,
            "批内 N 次写 ⇒ 1 次帧请求"
        );
        assert!(app.take_wake_request(), "批结束必须请求一帧");
        assert!(!app.take_wake_request(), "且只请求一次");
        assert_eq!(*app.read(&signal), 10, "批内写法照常生效（最终状态可见）");
        assert!(
            dirty(&app, window, node).contains(DirtyFlags::BIND),
            "节点被标 BIND，下一帧样式求值会看到最终值"
        );
    }

    #[test]
    fn batch_does_not_evaluate_derived_values_inline() {
        let calls = Rc::new(Cell::new(0u32));
        let counted = Rc::clone(&calls);
        let (_rt, mut app) = new_app();
        let source: Signal<u32> = app.signal(1);
        let _derived: Derived<u32> = app.derived(move |cx| {
            counted.set(counted.get() + 1);
            *cx.read(&source) * 2
        });

        app.batch(|app| {
            app.write(&source, 2);
            app.write(&source, 3);
        });
        assert_eq!(calls.get(), 0, "写（含批内）不得重入派生求值");
    }

    #[test]
    fn nested_batches_settle_at_the_outermost_boundary() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(0);
        let window = app.create_window(WindowOptions::default());
        let _node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        let _ = app.take_wake_request();

        let start = app.frame_requests();
        app.batch(|app| {
            app.write(&signal, 1);
            app.batch(|app| {
                app.write(&signal, 2);
                assert!(app.frame_requests() == start, "内层批结束不得提前请求帧");
            });
            assert!(app.frame_requests() == start, "内层结束后仍在外层批里");
        });
        assert_eq!(app.frame_requests() - start, 1, "最外层结算一次");
    }

    #[test]
    fn batch_panic_unwinds_depth_and_still_requests_the_pending_frame() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(0);
        let window = app.create_window(WindowOptions::default());
        let _node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        let _ = app.take_wake_request();
        let start = app.frame_requests();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            app.batch(|app| {
                app.write(&signal, 5);
                panic!("user closure panicked");
            });
        }));
        assert!(result.is_err(), "闭包 panic 原样传播");
        assert!(!app.batching(), "panic 后批处理深度必须归零");
        assert_eq!(
            app.frame_requests() - start,
            1,
            "panic 展开也要把推迟的帧请求结算掉（否则引擎永不唤醒）"
        );
    }

    #[test]
    fn released_scope_handle_write_is_a_noop_and_read_fails_deterministically() {
        let (_rt, mut app) = new_app();
        let window = app.create_window(WindowOptions::default());
        let signal: Signal<u32> = app.window_signal(window, 1);
        let node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        assert!(!deps(&app, window, node).is_empty());

        assert!(app.close_window(window), "关窗成功");
        let _ = app.take_wake_request();
        let start = app.frame_requests();

        assert!(
            !app.write(&signal, 2),
            "已失效句柄的写必须 short-circuit 为 false"
        );
        assert!(
            app.update_with(&signal, |value| *value += 1).is_none(),
            "已失效句柄的 update_with 必须返回 None"
        );
        assert_eq!(app.frame_requests(), start, "不得请求帧（无僵尸唤醒）");
        assert!(!app.take_wake_request(), "不得置唤醒位");

        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = *app.read(&signal);
        }))
        .expect_err("读已失效句柄必须 panic");
        let message = panic_payload_message(payload);
        assert!(
            message.contains("stale"),
            "panic 必须可诊断（stale）：{message}"
        );
        assert!(
            message.contains(&format!(
                "#{}.{}",
                signal.id().index(),
                signal.id().generation()
            )),
            "panic 必须带槽位下标与代际：{message}"
        );
    }

    #[test]
    fn releasing_a_scope_drops_its_dependency_edges_immediately() {
        let (_rt, mut app) = new_app();
        let window = app.create_window(WindowOptions::default());
        let scope = app.signal_scope();
        let signal: Signal<u32> = app.signal_scoped(scope, 1);
        let _node = build_node(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(*cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        assert_eq!(
            app.dependency_edge_count_for_test(),
            1,
            "节点对该信号有一条依赖边"
        );

        assert_eq!(app.release_scope(scope), 1);
        assert_eq!(
            app.dependency_edge_count_for_test(),
            0,
            "作用域释放必须**立即**删除依赖边（不是等下次写时惰性清理）"
        );
    }

    #[test]
    fn releasing_a_scope_recycles_slots_and_bumps_generation() {
        let mut store = StateStore::with_capacity(4, 0);
        let scope = store.new_scope();
        let old = store.push_owned_scoped(scope, 7u32);
        assert_eq!((old.index(), old.generation()), (0, 1));
        assert_eq!(store.live_slot_count_for_test(), 1);

        assert_eq!(store.release_scope(scope).map(|ids| ids.len()), Some(1));
        assert_eq!(store.slot_count_for_test(), 1, "槽位数组不收缩（下标复用）");
        assert_eq!(store.free_slot_count_for_test(), 1, "槽位必须归还空闲栈");
        assert_eq!(
            store.live_slot_count_for_test(),
            0,
            "值必须被 drop（内存回收）"
        );
        assert!(!store.is_live(old), "释放后旧句柄立即失效");

        let scope2 = store.new_scope();
        let fresh = store.push_owned_scoped(scope2, 100u32);
        assert_eq!(fresh.index(), old.index(), "新信号必须优先复用空闲槽位");
        assert_eq!(
            fresh.generation(),
            old.generation() + 1,
            "复用必须推进槽位代际"
        );
        assert_ne!(fresh, old);
        assert_eq!(store.free_slot_count_for_test(), 0);
        assert_eq!(store.live_slot_count_for_test(), 1);
        assert!(store.is_live(fresh));
        assert_eq!(*store.read_owned::<u32>(fresh), 100);
    }

    #[test]
    fn stale_handle_read_is_a_deterministic_panic_with_diagnostic() {
        let mut store = StateStore::with_capacity(2, 0);
        let scope = store.new_scope();
        let old = store.push_owned_scoped(scope, 1u32);
        store.release_scope(scope);
        let scope2 = store.new_scope();
        let fresh = store.push_owned_scoped(scope2, 2u32);
        assert_eq!(fresh.index(), old.index());

        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = *store.read_owned::<u32>(old);
        }))
        .expect_err("旧句柄读必须 panic（绝不返回复用后的新值）");
        let message = panic_payload_message(payload);
        assert!(message.contains("stale"), "panic 必须可诊断：{message}");
        assert!(
            message.contains("newer signal") || message.contains("free"),
            "panic 必须说明槽位现状（空闲 / 已被新信号占用）：{message}"
        );
        assert!(
            message.contains("#0.1"),
            "panic 必须带旧句柄下标+代际：{message}"
        );
        assert_eq!(*store.read_owned::<u32>(fresh), 2, "新占用者的值不受影响");
    }

    #[test]
    fn stale_handle_write_is_a_noop_and_never_touches_the_reused_value() {
        let mut store = StateStore::with_capacity(2, 0);
        let scope = store.new_scope();
        let old = store.push_owned_scoped(scope, 1u32);
        store.release_scope(scope);
        let scope2 = store.new_scope();
        let fresh = store.push_owned_scoped(scope2, 2u32);
        assert_eq!(fresh.index(), old.index());

        assert_eq!(
            store.write_owned(old, 99u32),
            None,
            "失效句柄的写必须是 no-op（None = 不失效传播）"
        );
        assert_eq!(
            store.update_owned(old, |value| *value = 100),
            None,
            "失效句柄的 update 必须返回 None（闭包不执行）"
        );
        assert_eq!(
            *store.read_owned::<u32>(fresh),
            2,
            "复用后的新值绝不能被子虚乌有的旧写改掉"
        );
        assert_eq!(store.write_owned(fresh, 3u32), Some(true), "新句柄照常可写");
        assert_eq!(*store.read_owned::<u32>(fresh), 3);
    }

    #[test]
    fn read_guard_survives_the_release_as_a_value_snapshot() {
        let mut store = StateStore::with_capacity(1, 0);
        let scope = store.new_scope();
        let id = store.push_owned_scoped(scope, String::from("frozen"));

        let guard = store.read_owned::<String>(id);
        store.release_scope(scope);
        assert_eq!(&*guard, "frozen", "已发出的守卫持有旧值的 Rc 快照");
        assert_eq!(store.live_slot_count_for_test(), 0, "存储侧的值已 drop");
    }

    #[test]
    fn generation_exhaustion_retires_the_slot_instead_of_wrapping() {
        let mut store = StateStore::with_capacity(1, 0);
        let scope = store.new_scope();
        let id = store.push_owned_scoped(scope, 1u32);
        let almost = store.force_generation_for_test(id, u32::MAX);

        assert_eq!(store.release_scope(scope).map(|ids| ids.len()), Some(1));
        assert!(!store.is_live(almost));
        assert_eq!(store.free_slot_count_for_test(), 0, "退休槽位不进空闲栈");

        let scope2 = store.new_scope();
        let fresh = store.push_owned_scoped(scope2, 2u32);
        assert_ne!(fresh.index(), id.index(), "退休槽位不得被复用");
        assert_eq!(store.slot_count_for_test(), 2, "新信号只能用新槽位");
        assert!(!store.is_live(almost), "旧句柄不得复活");
    }

    #[test]
    fn thousand_window_cycles_recycle_slots_so_the_peak_does_not_grow() {
        const CYCLES: usize = 1000;
        const SIGNALS_PER_WINDOW: usize = 8;

        let mut store = StateStore::with_capacity(4, 0);
        let mut peak: Option<usize> = None;
        for cycle in 0..CYCLES {
            let scope = store.new_scope();
            for value in 0..SIGNALS_PER_WINDOW {
                let id = store.push_owned_scoped(scope, (value + cycle) as u32);
                assert!(store.is_live(id), "新句柄必须存活");
            }
            assert_eq!(
                store.release_scope(scope).map(|ids| ids.len()),
                Some(SIGNALS_PER_WINDOW)
            );
            assert_eq!(store.live_slot_count_for_test(), 0, "关窗后不得有存活槽位");
            let slots = store.slot_count_for_test();
            match peak {
                Some(expected) => assert_eq!(
                    slots, expected,
                    "第 {cycle} 轮后槽位峰值必须停在第 1 轮的 {expected} 个槽位（回收失效会线性增长）"
                ),
                None => peak = Some(slots),
            }
        }
        assert_eq!(
            store.free_slot_count_for_test(),
            SIGNALS_PER_WINDOW,
            "最后一轮的槽位必须全部回到空闲栈"
        );
    }

    #[test]
    fn recycling_a_slot_clears_its_dependency_entries() {
        let mut store = StateStore::with_capacity(2, 0);
        let mut graph = DependencyGraph::new();
        let scope = store.new_scope();
        let old = store.push_owned_scoped(scope, 1u32);
        let owner = Owner::Node {
            window: WindowId::for_test(1),
            node: NodeId::for_test(1, 1),
        };
        graph.replace_deps(owner, &[], &[old]);
        assert_eq!(graph.signal_count(), 1, "先建立一条依赖边");

        let released = store.release_scope(scope).expect("作用域存在");
        for signal in &released {
            graph.remove_signal(*signal);
        }
        assert_eq!(graph.signal_count(), 0, "释放后槽位级依赖边必须归零");

        let scope2 = store.new_scope();
        let fresh = store.push_owned_scoped(scope2, 2u32);
        assert_eq!(fresh.index(), old.index(), "槽位已复用");
        assert!(
            graph.dependents_mut(fresh).is_none(),
            "新占用者不得继承旧信号的 owner"
        );
        assert!(graph.dependents_mut(old).is_none(), "旧键必须干净");
    }

    #[test]
    fn minting_into_a_released_scope_is_a_diagnostic_panic() {
        let mut store = StateStore::with_capacity(1, 0);
        let scope = store.new_scope();
        store.push_owned_scoped(scope, 1u32);
        store.release_scope(scope);

        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            store.push_owned_scoped(scope, 2u32);
        }))
        .expect_err("已释放作用域不得再 mint 信号");
        let message = panic_payload_message(payload);
        assert!(
            message.contains("released scopes cannot mint"),
            "panic 必须可诊断：{message}"
        );
    }

    #[test]
    fn root_scope_is_never_released() {
        let mut store = StateStore::with_capacity(2, 0);
        let root_signal = store.push_owned(1u32);
        assert_eq!(
            store.release_scope(SignalScope::ROOT),
            None,
            "根作用域不可释放"
        );
        assert!(store.is_live(root_signal), "根作用域信号必须仍然存活");
        assert_eq!(*store.read_owned::<u32>(root_signal), 1);
    }

    #[test]
    fn window_handle_exhaustion_is_a_diagnostic_panic() {
        let (_rt, mut app) = new_app();
        app.set_next_window_id_for_test(u32::MAX - 1);
        let last = app.create_window(WindowOptions::default());
        assert_eq!(last.to_bits(), u32::MAX - 1, "最后一个可用句柄");

        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            app.create_window(WindowOptions::default());
        }))
        .expect_err("句柄用尽必须显式失败（不回绕到 0/1）");
        assert!(
            panic_payload_message(payload).contains("exhausted"),
            "panic 必须可诊断"
        );
    }

    #[test]
    fn explicit_scope_release_is_idempotent() {
        let (_rt, mut app) = new_app();
        let scope = app.signal_scope();
        let signal: Signal<u32> = app.signal_scoped(scope, 3);
        assert_eq!(app.release_scope(scope), 1);
        assert_eq!(app.release_scope(scope), 0, "重复释放返回 0");
        assert!(!app.write(&signal, 4), "失效句柄的写是 no-op");
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = *app.read(&signal);
        }))
        .expect_err("失效句柄的读必须 panic");
        assert!(panic_payload_message(payload).contains("stale"));
    }

    #[test]
    fn node_ids_from_different_windows_never_collide() {
        let (_rt, mut app) = new_app();
        let first = app.create_window(WindowOptions::default());
        let second = app.create_window(WindowOptions::default());
        let a = build_node(&mut app, first, |_cx| StylePatch::default());
        let b = build_node(&mut app, second, |_cx| StylePatch::default());

        assert_eq!(a.index(), b.index(), "两窗都是各自 arena 的第 1 个槽位");
        assert_eq!(a.generation(), b.generation());
        assert_ne!(a, b, "NodeId 必须携带窗口身份");

        let second_tree = app.window(second).expect("窗口存在").tree();
        assert_eq!(
            second_tree.window(),
            Some(second),
            "树必须记录自己的窗口归属"
        );
        assert!(
            matches!(
                second_tree.get(a),
                Err(crate::error::EngineError::StaleNode { node }) if node == a
            ),
            "跨窗口句柄必须**确定性**失败（旧实现会静默命中另一个节点）"
        );
        let first_tree = app.window(first).expect("窗口存在").tree();
        assert!(first_tree.get(a).is_ok());
        assert!(first_tree.get(b).is_err());
    }

    #[test]
    fn cross_window_node_handle_is_rejected_by_app_apis() {
        let (_rt, mut app) = new_app();
        let first = app.create_window(WindowOptions::default());
        let second = app.create_window(WindowOptions::default());
        let node = build_node(&mut app, first, |_cx| StylePatch::default());

        assert!(
            !app.set_focus(second, Some(node)),
            "跨窗口节点不得成为另一窗口的焦点"
        );
        assert!(
            !app.capture_pointer(second, node),
            "跨窗口节点不得成为另一窗口的指针捕获目标"
        );
        assert!(
            matches!(
                app.remove_node(second, node),
                Err(EngineError::StaleNode { node: reported }) if reported == node
            ),
            "跨窗口节点不得被另一窗口卸载（强类型 StaleNode）"
        );
        assert!(
            app.window(first)
                .expect("窗口存在")
                .tree()
                .get(node)
                .is_ok(),
            "误用不得影响真正的宿主窗口"
        );
    }
}
