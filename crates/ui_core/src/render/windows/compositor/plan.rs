use windows::Win32::Graphics::Direct3D12::*;

use super::Persist;
use crate::{
    error::EngineError,
    math::PhysicalRect,
    render::{
        compositor::{DamageRects, FramePlan, MAX_DRAW_PASSES, Plan},
        damage::MAX_DAMAGE_RECTS,
        windows::swapchain::BUFFER_COUNT,
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TargetState {
    Fresh,
    PersistValid,
    PersistInvalid,
}

impl TargetState {
    pub(crate) fn is_fresh(self) -> bool {
        matches!(self, Self::Fresh)
    }

    pub(crate) fn is_persist_valid(self) -> bool {
        matches!(self, Self::PersistValid)
    }

    pub(crate) fn on_target_created(&mut self) {
        *self = Self::Fresh;
    }

    pub(crate) fn on_presented(&mut self) {
        if self.is_fresh() {
            *self = Self::PersistInvalid;
        }
    }

    pub(crate) fn on_present_failed(&mut self) {
        *self = Self::Fresh;
    }

    pub(crate) fn on_direct_draw(&mut self) {
        *self = Self::PersistInvalid;
    }

    pub(crate) fn on_full_persist_draw(&mut self) {
        *self = Self::PersistValid;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameDraw {
    Direct,
    Full,
    Incremental,
}

pub(crate) fn requires_full_draw(frame: FramePlan, damage: Option<&DamageRects>) -> bool {
    frame.is_full() || (matches!(frame.plan(), Plan::Partial(_)) && damage.is_none())
}

pub(crate) fn frame_draw(state: TargetState, plan: Plan, has_swapchain: bool) -> FrameDraw {
    let full_draw = plan.is_full() || matches!(plan, Plan::Idle) || state.is_fresh();
    if full_draw {
        return if has_swapchain {
            FrameDraw::Direct
        } else {
            FrameDraw::Full
        };
    }
    if state.is_persist_valid() {
        FrameDraw::Incremental
    } else {
        FrameDraw::Full
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CopyExtent {
    Full,
    Rects(DamageRects),
}

#[derive(Clone, Copy, Debug)]
struct BufferSyncState {
    pending: DamageRects,
    full: bool,
}

impl BufferSyncState {
    const UNSYNCED: Self = Self {
        pending: DamageRects::new(),
        full: true,
    };
}

const SYNC_SLOTS: usize = BUFFER_COUNT as usize;

pub(crate) struct BufferSyncs {
    slots: [BufferSyncState; SYNC_SLOTS],
}

impl BufferSyncs {
    pub(crate) const fn new() -> Self {
        Self {
            slots: [BufferSyncState::UNSYNCED; SYNC_SLOTS],
        }
    }

    pub(crate) fn invalidate(&mut self) {
        self.slots = [BufferSyncState::UNSYNCED; SYNC_SLOTS];
    }

    pub(crate) fn take_copy(
        &mut self,
        index: u32,
        damage: Option<&DamageRects>,
        content: (u32, u32),
    ) -> CopyExtent {
        let slot = &mut self.slots[index as usize % SYNC_SLOTS];
        let copy = match damage {
            None => CopyExtent::Full,
            Some(_) if slot.full => CopyExtent::Full,
            Some(damage) => {
                let mut rects = slot.pending;
                merge_damage(&mut rects, damage);
                if copy_area(&rects, content) >= u64::from(content.0) * u64::from(content.1) {
                    CopyExtent::Full
                } else {
                    CopyExtent::Rects(rects)
                }
            }
        };
        *slot = BufferSyncState {
            pending: DamageRects::new(),
            full: false,
        };
        copy
    }

    pub(crate) fn record_missed(&mut self, index: u32, damage: Option<&DamageRects>) {
        let slot = &mut self.slots[index as usize % SYNC_SLOTS];
        match damage {
            None => {
                slot.full = true;
                slot.pending = DamageRects::new();
            }
            Some(damage) if !slot.full => merge_damage(&mut slot.pending, damage),
            Some(_) => {}
        }
    }

    pub(crate) fn record_missed_others(&mut self, presented: u32, damage: Option<&DamageRects>) {
        for index in 0..SYNC_SLOTS as u32 {
            if index % SYNC_SLOTS as u32 != presented % SYNC_SLOTS as u32 {
                self.record_missed(index, damage);
            }
        }
    }
}

impl Default for BufferSyncs {
    fn default() -> Self {
        Self::new()
    }
}

fn merge_damage(pending: &mut DamageRects, damage: &DamageRects) {
    for rect in damage.as_slice() {
        if pending.push(*rect) {
            continue;
        }
        pending.coalesce(MAX_DAMAGE_RECTS - MAX_DRAW_PASSES);
        debug_assert!(
            pending.push(*rect),
            "a frame's damage set exceeds the draw-pass budget"
        );
    }
}

fn copy_area(rects: &DamageRects, content: (u32, u32)) -> u64 {
    let surface = PhysicalRect::new(0, 0, content.0 as i32, content.1 as i32);
    rects
        .as_slice()
        .iter()
        .map(|rect| rect.intersect(surface))
        .filter(|rect| !rect.is_empty())
        .map(|rect| u64::from(rect.width() as u32) * u64::from(rect.height() as u32))
        .sum()
}

pub(crate) fn clamp_damage(
    damage: Option<&DamageRects>,
    content: (u32, u32),
) -> Option<DamageRects> {
    let surface = PhysicalRect::new(0, 0, content.0 as i32, content.1 as i32);
    let damage = damage?;
    let mut clamped = DamageRects::new();
    let mut outside = 0usize;
    for rect in damage.as_slice() {
        let rect = rect.intersect(surface);
        if rect.is_empty() {
            outside += 1;
            continue;
        }
        if !clamped.push(rect) {
            return None;
        }
    }
    if outside > 0 && clamped.is_empty() {
        return None;
    }
    clamped.coalesce(MAX_DRAW_PASSES);
    Some(clamped)
}

pub(crate) fn copy_to_back(
    list: &ID3D12GraphicsCommandList,
    persist: &Persist,
    back: &ID3D12Resource,
    extent: CopyExtent,
) -> Result<(), EngineError> {
    transition(
        list,
        &persist.texture,
        D3D12_RESOURCE_STATE_RENDER_TARGET,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
    );
    transition(
        list,
        back,
        D3D12_RESOURCE_STATE_PRESENT,
        D3D12_RESOURCE_STATE_COPY_DEST,
    );
    match extent {
        CopyExtent::Full => {
            let whole = PhysicalRect::new(0, 0, persist.size.0 as i32, persist.size.1 as i32);
            copy_rect(list, &persist.texture, back, &whole);
        }
        CopyExtent::Rects(rects) => {
            for rect in rects.as_slice() {
                copy_rect(list, &persist.texture, back, rect);
            }
        }
    }
    transition(
        list,
        &persist.texture,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_RENDER_TARGET,
    );
    transition(
        list,
        back,
        D3D12_RESOURCE_STATE_COPY_DEST,
        D3D12_RESOURCE_STATE_PRESENT,
    );
    Ok(())
}

pub(crate) fn transition(
    list: &ID3D12GraphicsCommandList,
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) {
    let mut barrier = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: core::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: core::mem::ManuallyDrop::new(Some(resource.clone())),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    };
    unsafe { list.ResourceBarrier(core::slice::from_ref(&barrier)) };
    unsafe {
        let transition: &mut D3D12_RESOURCE_TRANSITION_BARRIER = &mut barrier.Anonymous.Transition;
        core::mem::ManuallyDrop::drop(&mut transition.pResource);
    }
}

pub(crate) fn copy_rect(
    list: &ID3D12GraphicsCommandList,
    source: &ID3D12Resource,
    destination: &ID3D12Resource,
    rect: &PhysicalRect,
) {
    let mut destination_location = D3D12_TEXTURE_COPY_LOCATION {
        pResource: core::mem::ManuallyDrop::new(Some(destination.clone())),
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: 0,
        },
    };
    let mut source_location = D3D12_TEXTURE_COPY_LOCATION {
        pResource: core::mem::ManuallyDrop::new(Some(source.clone())),
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: 0,
        },
    };
    let region = D3D12_BOX {
        left: rect.x0 as u32,
        top: rect.y0 as u32,
        front: 0,
        right: rect.x1 as u32,
        bottom: rect.y1 as u32,
        back: 1,
    };
    unsafe {
        list.CopyTextureRegion(
            &destination_location,
            rect.x0 as u32,
            rect.y0 as u32,
            0,
            &source_location,
            Some(&region),
        )
    };
    unsafe {
        core::mem::ManuallyDrop::drop(&mut destination_location.pResource);
        core::mem::ManuallyDrop::drop(&mut source_location.pResource);
    }
}

#[cfg(test)]
mod tests {
    use super::{BufferSyncs, CopyExtent, FrameDraw, TargetState, clamp_damage, frame_draw};
    use crate::{
        math::PhysicalRect,
        render::{
            compositor::{DamageRects, FramePlan, MAX_DRAW_PASSES, Plan},
            damage::MAX_DAMAGE_RECTS,
        },
    };

    fn damage(rects: &[PhysicalRect]) -> DamageRects {
        let mut damage = DamageRects::new();
        for rect in rects {
            assert!(damage.push(*rect));
        }
        damage
    }

    fn partial(rects: &[PhysicalRect]) -> Plan {
        Plan::Partial(damage(rects))
    }

    #[test]
    fn the_target_state_has_one_transition_table() {
        let mut state = TargetState::Fresh;
        assert!(state.is_fresh());
        state.on_presented();
        assert_eq!(state, TargetState::PersistInvalid, "present 之后不再 fresh");

        state = TargetState::PersistValid;
        state.on_presented();
        assert_eq!(state, TargetState::PersistValid, "局部帧保持有效");

        state.on_direct_draw();
        assert_eq!(state, TargetState::PersistInvalid, "直绘之后 persist 过期");
        state.on_full_persist_draw();
        assert_eq!(state, TargetState::PersistValid);

        state.on_present_failed();
        assert_eq!(state, TargetState::Fresh, "失败 ⇒ 下一帧必须整帧重来");
        state.on_target_created();
        assert_eq!(state, TargetState::Fresh, "重建目标 ⇒ Fresh");
    }

    #[test]
    fn full_frames_draw_directly_into_the_back_buffer() {
        assert_eq!(
            frame_draw(TargetState::PersistValid, Plan::Full, true),
            FrameDraw::Direct
        );
        assert_eq!(
            frame_draw(TargetState::Fresh, Plan::Full, true),
            FrameDraw::Direct
        );
        assert_eq!(
            frame_draw(
                TargetState::Fresh,
                partial(&[PhysicalRect::new(0, 0, 8, 8)]),
                true
            ),
            FrameDraw::Direct
        );
        assert_eq!(
            frame_draw(
                TargetState::PersistInvalid,
                partial(&[PhysicalRect::new(0, 0, 8, 8)]),
                true
            ),
            FrameDraw::Full
        );
        assert_eq!(
            frame_draw(
                TargetState::PersistValid,
                partial(&[PhysicalRect::new(0, 0, 8, 8)]),
                true
            ),
            FrameDraw::Incremental
        );
        assert_eq!(
            frame_draw(TargetState::PersistInvalid, Plan::Full, false),
            FrameDraw::Full
        );
        assert_eq!(
            frame_draw(
                TargetState::PersistInvalid,
                partial(&[PhysicalRect::new(0, 0, 8, 8)]),
                false
            ),
            FrameDraw::Full
        );
        assert_eq!(
            frame_draw(
                TargetState::PersistValid,
                partial(&[PhysicalRect::new(0, 0, 8, 8)]),
                false
            ),
            FrameDraw::Incremental
        );
    }

    #[test]
    fn the_backend_clamps_damage_to_the_current_content() {
        let content = (800, 600);
        let dirty = damage(&[PhysicalRect::new(700, 500, 1200, 900)]);
        let clamped = clamp_damage(Some(&dirty), content).expect("inside");
        assert_eq!(clamped.as_slice(), &[PhysicalRect::new(700, 500, 800, 600)]);

        let outside = damage(&[PhysicalRect::new(810, 10, 900, 40)]);
        assert_eq!(clamp_damage(Some(&outside), content), None);
        assert_eq!(clamp_damage(None, content), None);
        let mut many = DamageRects::new();
        for index in 0..MAX_DAMAGE_RECTS as i32 {
            assert!(many.push(PhysicalRect::new(index, 0, index + 1, 1)));
        }
        let clamped = clamp_damage(Some(&many), content).expect("capacity fits");
        assert!(clamped.as_slice().len() <= MAX_DRAW_PASSES);
        let small = damage(&[PhysicalRect::new(0, 0, 4, 4)]);
        let clamped = clamp_damage(Some(&small), content).expect("inside");
        assert_eq!(clamped.as_slice(), small.as_slice());
        let _ = FramePlan::new(Plan::Full, false);
    }

    #[test]
    fn buffer_syncs_copy_only_what_each_buffer_missed() {
        let content = (200, 200);
        let mut syncs = BufferSyncs::new();
        assert_eq!(syncs.take_copy(0, None, content), CopyExtent::Full);

        let frame_a = damage(&[PhysicalRect::new(0, 0, 20, 20)]);
        let frame_b = damage(&[PhysicalRect::new(100, 100, 120, 120)]);
        assert_eq!(syncs.take_copy(1, None, content), CopyExtent::Full);
        assert_eq!(syncs.take_copy(2, None, content), CopyExtent::Full);
        syncs.record_missed(0, Some(&frame_a));
        syncs.record_missed(2, Some(&frame_a));
        syncs.record_missed(0, Some(&frame_b));
        syncs.record_missed(2, Some(&frame_b));

        let copy = syncs.take_copy(2, Some(&frame_b), content);
        let CopyExtent::Rects(rects) = copy else {
            panic!("expected rect copy, got {copy:?}");
        };
        for expected in [
            PhysicalRect::new(0, 0, 20, 20),
            PhysicalRect::new(100, 100, 120, 120),
        ] {
            assert!(
                rects.as_slice().iter().any(|rect| rect.x0 <= expected.x0
                    && rect.y0 <= expected.y0
                    && rect.x1 >= expected.x1
                    && rect.y1 >= expected.y1),
                "missing {expected:?} in {:?}",
                rects.as_slice()
            );
        }
        let copy = syncs.take_copy(2, Some(&frame_b), content);
        let CopyExtent::Rects(rects) = copy else {
            panic!("expected rect copy after syncing");
        };
        assert_eq!(rects.as_slice(), frame_b.as_slice());
    }

    #[test]
    fn a_full_frame_invalidates_every_buffer_sync() {
        let content = (200, 200);
        let mut syncs = BufferSyncs::new();
        assert_eq!(syncs.take_copy(0, None, content), CopyExtent::Full);
        syncs.record_missed_others(0, None);
        assert_eq!(
            syncs.take_copy(1, Some(&damage(&[PhysicalRect::new(0, 0, 4, 4)])), content),
            CopyExtent::Full
        );
        syncs.invalidate();
        assert_eq!(
            syncs.take_copy(2, Some(&damage(&[PhysicalRect::new(0, 0, 4, 4)])), content),
            CopyExtent::Full
        );
    }

    #[test]
    fn the_copy_degrades_to_full_when_the_area_reaches_the_content() {
        let content = (100, 100);
        let mut syncs = BufferSyncs::new();
        assert_eq!(syncs.take_copy(0, None, content), CopyExtent::Full);
        let mut huge = DamageRects::new();
        for index in 0..MAX_DRAW_PASSES as i32 {
            assert!(huge.push(PhysicalRect::new(0, index * 25, 100, index * 25 + 25)));
        }
        assert_eq!(syncs.take_copy(0, Some(&huge), content), CopyExtent::Full);
    }

    #[test]
    fn a_partial_frame_without_a_visible_rect_upgrades_to_a_full_draw() {
        let content = (800, 600);
        let outside = damage(&[PhysicalRect::new(810, 10, 900, 40)]);
        let frame = FramePlan::new(Plan::Partial(outside), false);
        let clamped = clamp_damage(frame.present_dirty().as_ref(), content);
        assert_eq!(clamped, None);
        assert!(super::requires_full_draw(frame, clamped.as_ref()));
        let inside = damage(&[PhysicalRect::new(0, 0, 8, 8)]);
        let frame = FramePlan::new(Plan::Partial(inside), false);
        let clamped = clamp_damage(frame.present_dirty().as_ref(), content);
        assert!(!super::requires_full_draw(frame, clamped.as_ref()));
        assert!(super::requires_full_draw(
            FramePlan::new(Plan::Full, false),
            None
        ));
    }

    #[test]
    fn the_frame_plan_fresh_flag_is_derived_from_the_target_state() {
        assert!(FramePlan::new(Plan::Full, TargetState::Fresh.is_fresh()).is_fresh());
        assert!(!FramePlan::new(Plan::Full, TargetState::PersistValid.is_fresh()).is_fresh());
    }
}
