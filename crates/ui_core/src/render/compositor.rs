use crate::{
    math::PhysicalRect,
    render::damage::{DamageSummary, MAX_DAMAGE_RECTS},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DamageRects {
    rects: [PhysicalRect; MAX_DAMAGE_RECTS],
    len: usize,
}

impl DamageRects {
    pub(crate) const fn new() -> Self {
        Self {
            rects: [PhysicalRect {
                x0: 0,
                y0: 0,
                x1: 0,
                y1: 0,
            }; MAX_DAMAGE_RECTS],
            len: 0,
        }
    }

    pub(crate) fn push(&mut self, rect: PhysicalRect) -> bool {
        if self.len == MAX_DAMAGE_RECTS {
            return false;
        }
        self.rects[self.len] = rect;
        self.len += 1;
        true
    }

    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn extend(&mut self, other: DamageRects) -> bool {
        for rect in other.as_slice() {
            if self.as_slice().contains(rect) {
                continue;
            }
            if !self.push(*rect) {
                return false;
            }
        }
        true
    }

    pub(crate) fn as_slice(&self) -> &[PhysicalRect] {
        &self.rects[..self.len]
    }

    pub(crate) fn coalesce(&mut self, max_passes: usize) {
        let max_passes = max_passes.clamp(1, MAX_DAMAGE_RECTS);
        while self.len > max_passes {
            let mut best = (0usize, 1usize);
            let mut best_growth = u64::MAX;
            for a in 0..self.len {
                for b in (a + 1)..self.len {
                    let growth = union_growth(self.rects[a], self.rects[b]);
                    if growth < best_growth {
                        best_growth = growth;
                        best = (a, b);
                    }
                }
            }
            let (a, b) = best;
            let union = union_rect(self.rects[a], self.rects[b]);
            self.rects[a] = union;
            self.rects[b] = self.rects[self.len - 1];
            self.len -= 1;
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn clamped(self, width: u32, height: u32) -> DamageRects {
        let width = width.min(i32::MAX as u32) as i32;
        let height = height.min(i32::MAX as u32) as i32;
        let mut clamped = DamageRects::new();
        for rect in self.as_slice() {
            let x0 = rect.x0.clamp(0, width);
            let y0 = rect.y0.clamp(0, height);
            let x1 = rect.x1.clamp(x0, width);
            let y1 = rect.y1.clamp(y0, height);
            if x1 == x0 || y1 == y0 {
                continue;
            }
            let kept = clamped.push(PhysicalRect::new(x0, y0, x1, y1));
            debug_assert!(kept, "clamping never grows the set");
        }
        clamped
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }
}

pub(crate) const MAX_DRAW_PASSES: usize = 4;

fn union_rect(a: PhysicalRect, b: PhysicalRect) -> PhysicalRect {
    PhysicalRect {
        x0: a.x0.min(b.x0),
        y0: a.y0.min(b.y0),
        x1: a.x1.max(b.x1),
        y1: a.y1.max(b.y1),
    }
}

fn union_growth(a: PhysicalRect, b: PhysicalRect) -> u64 {
    let union = union_rect(a, b);
    union_area(union).saturating_sub(union_area(a) + union_area(b))
}

fn union_area(rect: PhysicalRect) -> u64 {
    let width = (rect.x1 - rect.x0).max(0) as u64;
    let height = (rect.y1 - rect.y0).max(0) as u64;
    width.saturating_mul(height)
}

impl Default for DamageRects {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CopyPlan {
    Full,
    Regions(DamageRects),
}

#[cfg(any(target_os = "linux", test))]
impl CopyPlan {
    #[cfg(target_os = "linux")]
    pub(crate) fn regions(self) -> Option<DamageRects> {
        match self {
            Self::Full => None,
            Self::Regions(regions) => Some(regions),
        }
    }

    #[cfg(test)]
    pub(crate) fn is_full(self) -> bool {
        matches!(self, Self::Full)
    }
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImageSync {
    Unknown,
    Pending(DamageRects),
}

#[cfg(any(target_os = "linux", test))]
pub(crate) struct ImageCopyPlan {
    images: Vec<ImageSync>,
}

#[cfg(any(target_os = "linux", test))]
impl ImageCopyPlan {
    pub(crate) fn new(count: usize) -> Self {
        Self {
            images: vec![ImageSync::Unknown; count],
        }
    }

    pub(crate) fn reset(&mut self, count: usize) {
        self.images.clear();
        self.images.resize(count, ImageSync::Unknown);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.images.len()
    }

    pub(crate) fn plan(&self, image: u32, written: Option<DamageRects>) -> CopyPlan {
        let Some(ImageSync::Pending(pending)) = self.images.get(image as usize) else {
            return CopyPlan::Full;
        };
        let Some(written) = written else {
            return CopyPlan::Full;
        };
        let mut regions = *pending;
        if !regions.extend(written) {
            return CopyPlan::Full;
        }
        regions.coalesce(MAX_DRAW_PASSES);
        CopyPlan::Regions(regions)
    }

    pub(crate) fn commit_without_copy(
        &mut self,
        image: u32,
        written: Option<DamageRects>,
    ) -> usize {
        let index = image as usize;
        let carried = self.images.get(index).copied();
        let degraded = self.commit(image, written);
        let Some(ImageSync::Pending(pending)) = carried else {
            if carried.is_some() {
                self.images[index] = ImageSync::Unknown;
            }
            return degraded;
        };
        let Some(written) = written else {
            self.images[index] = ImageSync::Unknown;
            return degraded + 1;
        };
        let mut merged = pending;
        if !merged.extend(written) {
            self.images[index] = ImageSync::Unknown;
            return degraded + 1;
        }
        self.images[index] = ImageSync::Pending(merged);
        degraded
    }

    pub(crate) fn commit(&mut self, image: u32, written: Option<DamageRects>) -> usize {
        let Some(current) = self.images.get_mut(image as usize) else {
            return 0;
        };
        *current = ImageSync::Pending(DamageRects::new());
        let mut degraded = 0usize;
        for (index, entry) in self.images.iter_mut().enumerate() {
            if index == image as usize {
                continue;
            }
            match entry {
                ImageSync::Unknown => {}
                ImageSync::Pending(pending) => {
                    let Some(written) = written else {
                        *entry = ImageSync::Unknown;
                        degraded += 1;
                        continue;
                    };
                    if !pending.extend(written) {
                        *entry = ImageSync::Unknown;
                        degraded += 1;
                    }
                }
            }
        }
        degraded
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    Idle,
    Full,
    Partial(DamageRects),
}

impl Plan {
    pub(crate) fn is_full(self) -> bool {
        matches!(self, Self::Full)
    }

    pub(crate) fn merge(self, next: Self) -> Self {
        match (self, next) {
            (Self::Full, _) | (_, Self::Full) => Self::Full,
            (Self::Idle, other) | (other, Self::Idle) => other,
            (Self::Partial(older), Self::Partial(newer)) => {
                let mut merged = DamageRects::new();
                for rect in older.as_slice().iter().chain(newer.as_slice()) {
                    if !merged.push(*rect) {
                        return Self::Full;
                    }
                }
                merged.coalesce(MAX_DRAW_PASSES);
                Self::Partial(merged)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FramePlan {
    plan: Plan,
    fresh: bool,
}

impl FramePlan {
    pub(crate) fn new(plan: Plan, fresh: bool) -> Self {
        Self { plan, fresh }
    }

    pub(crate) fn plan(self) -> Plan {
        self.plan
    }

    #[cfg(any(windows, target_os = "macos", test))]
    pub(crate) fn present_dirty(self) -> Option<DamageRects> {
        if self.fresh {
            return None;
        }
        match self.plan {
            Plan::Partial(damage) => Some(damage),
            Plan::Idle | Plan::Full => None,
        }
    }

    pub(crate) fn is_idle(self) -> bool {
        matches!(self.plan, Plan::Idle) && !self.fresh
    }

    pub(crate) fn is_full(self) -> bool {
        self.fresh || self.plan.is_full()
    }

    #[cfg(any(windows, test))]
    pub(crate) fn is_fresh(self) -> bool {
        self.fresh
    }

    pub(crate) fn presented(&mut self) {
        self.fresh = false;
    }
}

pub(crate) fn plan(damage: DamageSummary, merged: &[PhysicalRect], surface: PhysicalRect) -> Plan {
    if damage.full {
        return Plan::Full;
    }
    let mut rects = DamageRects::new();
    for rect in merged {
        let clipped = rect.intersect(surface);
        if clipped.is_empty() {
            continue;
        }
        if !rects.push(clipped) {
            return Plan::Full;
        }
    }
    if rects.is_empty() {
        Plan::Idle
    } else {
        rects.coalesce(MAX_DRAW_PASSES);
        Plan::Partial(rects)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CopyPlan, DamageRects, FramePlan, ImageCopyPlan, MAX_DAMAGE_RECTS, MAX_DRAW_PASSES, Plan,
        plan, union_growth,
    };
    use crate::{math::PhysicalRect, render::damage::DamageSummary};

    fn surface() -> PhysicalRect {
        PhysicalRect::new(0, 0, 100, 50)
    }

    fn partial(rects: usize) -> DamageSummary {
        DamageSummary { full: false, rects }
    }

    #[test]
    fn full_damage_is_a_full_plan() {
        let plan = plan(
            DamageSummary {
                full: true,
                rects: 0,
            },
            &[],
            surface(),
        );
        assert_eq!(plan, Plan::Full);
        assert!(plan.is_full());
    }

    #[test]
    fn a_fresh_swapchain_presents_without_dirty_rects_until_presented() {
        let rect = PhysicalRect::new(0, 0, 10, 10);
        let mut frame = FramePlan::new(plan(partial(1), &[rect], surface()), true);
        assert!(matches!(frame.plan(), Plan::Partial(_)));
        assert_eq!(frame.present_dirty(), None);
        assert!(frame.is_full() && frame.is_fresh());

        frame.presented();
        assert!(!frame.is_fresh());
        assert!(frame.present_dirty().is_some());
    }

    #[test]
    fn a_full_plan_presents_without_dirty_rects() {
        let frame = FramePlan::new(Plan::Full, false);
        assert_eq!(frame.present_dirty(), None);
    }

    #[test]
    fn an_idle_plan_stays_idle() {
        let frame = FramePlan::new(Plan::Idle, false);
        assert!(frame.is_idle());
        assert_eq!(frame.present_dirty(), None);
    }

    #[test]
    fn empty_rects_without_full_degrade_to_idle() {
        assert_eq!(plan(partial(0), &[], surface()), Plan::Idle);
    }

    #[test]
    fn rects_are_clipped_to_the_surface() {
        let rect = PhysicalRect::new(-5, -5, 10, 10);
        match plan(partial(1), &[rect], surface()) {
            Plan::Partial(rects) => {
                assert_eq!(rects.as_slice(), &[PhysicalRect::new(0, 0, 10, 10)])
            }
            other => panic!("unexpected plan: {other:?}"),
        }
    }

    #[test]
    fn rects_beyond_the_right_and_bottom_edges_are_clipped() {
        let outside = PhysicalRect::new(101, 51, 120, 70);
        assert_eq!(plan(partial(1), &[outside], surface()), Plan::Idle);

        let straddling = PhysicalRect::new(90, 40, 101, 51);
        match plan(partial(1), &[straddling], surface()) {
            Plan::Partial(rects) => {
                assert_eq!(rects.as_slice(), &[PhysicalRect::new(90, 40, 100, 50)]);
            }
            other => panic!("unexpected plan: {other:?}"),
        }

        assert_eq!(
            plan(
                DamageSummary {
                    full: true,
                    rects: 1,
                },
                &[outside],
                surface()
            ),
            Plan::Full
        );
    }

    #[test]
    fn fully_outside_rects_are_dropped() {
        let rect = PhysicalRect::new(120, 60, 140, 80);
        assert_eq!(plan(partial(1), &[rect], surface()), Plan::Idle);
    }

    #[test]
    fn too_many_rects_degrade_to_full() {
        let rect = PhysicalRect::new(0, 0, 1, 1);
        let many = vec![rect; MAX_DAMAGE_RECTS + 1];
        assert_eq!(plan(partial(many.len()), &many, surface()), Plan::Full);
    }

    #[test]
    fn rects_beyond_capacity_report_failure() {
        let mut rects = DamageRects::new();
        let rect = PhysicalRect::new(0, 0, 1, 1);
        for _ in 0..MAX_DAMAGE_RECTS {
            assert!(rects.push(rect));
        }
        assert_eq!(rects.len(), MAX_DAMAGE_RECTS);
        assert!(!rects.push(rect));
        assert!(!rects.is_empty());
    }

    fn partial_plan(rects: &[PhysicalRect]) -> Plan {
        let mut plan = DamageRects::new();
        for rect in rects {
            assert!(plan.push(*rect));
        }
        Plan::Partial(plan)
    }

    #[test]
    fn merging_with_full_is_full_in_both_orders() {
        let partial = partial_plan(&[PhysicalRect::new(0, 0, 4, 4)]);
        assert_eq!(partial.merge(Plan::Full), Plan::Full);
        assert_eq!(Plan::Full.merge(partial), Plan::Full);
        assert_eq!(Plan::Idle.merge(Plan::Full), Plan::Full);
        assert_eq!(Plan::Full.merge(Plan::Full), Plan::Full);
    }

    #[test]
    fn merging_with_idle_keeps_the_other_plan() {
        let partial = partial_plan(&[PhysicalRect::new(1, 2, 3, 4)]);
        assert_eq!(partial.merge(Plan::Idle), partial);
        assert_eq!(Plan::Idle.merge(partial), partial);
        assert_eq!(Plan::Idle.merge(Plan::Idle), Plan::Idle);
    }

    #[test]
    fn merging_two_partials_unions_the_rectangles() {
        let older = [
            PhysicalRect::new(0, 0, 8, 8),
            PhysicalRect::new(16, 0, 24, 8),
        ];
        let newer = [PhysicalRect::new(0, 16, 8, 24)];
        let merged = partial_plan(&older).merge(partial_plan(&newer));
        match merged {
            Plan::Partial(rects) => {
                let expected: Vec<PhysicalRect> =
                    older.iter().chain(newer.iter()).copied().collect();
                assert_eq!(rects.as_slice(), expected.as_slice());
            }
            other => panic!("unexpected plan: {other:?}"),
        }
    }

    #[test]
    fn merged_rects_beyond_capacity_degrade_to_full() {
        let older: Vec<PhysicalRect> = (0..MAX_DAMAGE_RECTS)
            .map(|index| PhysicalRect::new(index as i32, 0, index as i32 + 1, 1))
            .collect();
        let newer = [PhysicalRect::new(0, 8, 1, 9)];
        assert_eq!(
            partial_plan(&older).merge(partial_plan(&newer)),
            Plan::Full,
            "17 个矩形超出固定容量 ⇒ 全量"
        );
        let exactly = partial_plan(&older[..MAX_DAMAGE_RECTS - 1]).merge(partial_plan(&newer));
        let Plan::Partial(rects) = exactly else {
            panic!("capacity-fitting merge must stay partial, got {exactly:?}");
        };
        assert_eq!(
            rects.len(),
            MAX_DRAW_PASSES,
            "合并结果必须收敛到绘制预算，而不是原样保留 {} 个矩形",
            MAX_DAMAGE_RECTS
        );
        for original in older[..MAX_DAMAGE_RECTS - 1].iter().chain(newer.iter()) {
            assert!(
                rects
                    .as_slice()
                    .iter()
                    .any(|merged| merged.x0 <= original.x0
                        && merged.y0 <= original.y0
                        && merged.x1 >= original.x1
                        && merged.y1 >= original.y1),
                "原始矩形 {original:?} 必须被覆盖：{:?}",
                rects.as_slice()
            );
        }
    }
    #[test]
    fn damage_is_coalesced_to_the_pass_budget() {
        let rects: Vec<PhysicalRect> = (0..12)
            .map(|index| PhysicalRect::new(index * 4, 0, index * 4 + 2, 2))
            .collect();
        let coalesced = plan(
            partial(rects.len()),
            &rects,
            PhysicalRect::new(0, 0, 200, 50),
        );
        let Plan::Partial(rects) = coalesced else {
            panic!("expected partial plan, got {coalesced:?}");
        };
        assert_eq!(
            rects.len(),
            MAX_DRAW_PASSES,
            "12 个矩形必须被合并到 {MAX_DRAW_PASSES} 个以内"
        );
    }

    #[test]
    fn coalescing_covers_every_original_rect() {
        let originals: Vec<PhysicalRect> = (0..10)
            .map(|index| PhysicalRect::new(index * 7, index * 3, index * 7 + 5, index * 3 + 4))
            .collect();
        let coalesced = plan(
            partial(originals.len()),
            &originals,
            PhysicalRect::new(0, 0, 200, 50),
        );
        let Plan::Partial(rects) = coalesced else {
            panic!("expected partial plan");
        };
        for original in &originals {
            let covered = rects.as_slice().iter().any(|merged| {
                merged.x0 <= original.x0
                    && merged.y0 <= original.y0
                    && merged.x1 >= original.x1
                    && merged.y1 >= original.y1
            });
            assert!(
                covered,
                "原始矩形 {original:?} 必须被某个合并结果覆盖：{rects:?}"
            );
        }
    }

    #[test]
    fn small_damage_sets_are_left_alone() {
        let originals = [
            PhysicalRect::new(0, 0, 4, 4),
            PhysicalRect::new(50, 50, 54, 54),
        ];
        let coalesced = partial_plan(&originals);
        let Plan::Partial(rects) = coalesced else {
            panic!("expected partial plan");
        };
        assert_eq!(rects.as_slice(), &originals, "两个矩形不应被合并");
    }

    #[test]
    fn coalescing_prefers_the_tightest_pair() {
        let near_a = PhysicalRect::new(0, 0, 4, 4);
        let near_b = PhysicalRect::new(0, 4, 4, 8);
        let far = PhysicalRect::new(100, 100, 104, 104);
        assert!(
            union_growth(near_a, near_b) < union_growth(near_a, far),
            "相邻矩形的并集增长必须小于远离的矩形"
        );
        assert_eq!(union_growth(near_a, near_b), 0, "紧贴的矩形并集不增加面积");
    }

    fn rects(list: &[PhysicalRect]) -> DamageRects {
        let mut rects = DamageRects::new();
        for rect in list {
            assert!(rects.push(*rect));
        }
        rects
    }

    fn regions(plan: CopyPlan) -> Vec<PhysicalRect> {
        match plan {
            CopyPlan::Regions(rects) => rects.as_slice().to_vec(),
            CopyPlan::Full => panic!("expected a region copy, got full"),
        }
    }

    #[test]
    fn a_new_swapchain_copies_every_image_in_full_once() {
        let mut plan = ImageCopyPlan::new(3);
        assert_eq!(plan.len(), 3);
        let damage = rects(&[PhysicalRect::new(0, 0, 8, 8)]);
        for image in 0..3 {
            assert!(plan.plan(image, Some(damage)).is_full(), "image {image}");
            plan.commit(image, Some(damage));
        }
        assert_eq!(
            regions(plan.plan(0, Some(damage))),
            vec![PhysicalRect::new(0, 0, 8, 8)],
            "基线建立后只拷 damage"
        );
    }

    #[test]
    fn skipped_frames_accumulate_damage_for_every_image() {
        let mut plan = ImageCopyPlan::new(2);
        let baseline = rects(&[PhysicalRect::new(0, 0, 8, 8)]);
        plan.commit(0, Some(baseline));
        plan.commit(1, Some(baseline));
        let missed = rects(&[PhysicalRect::new(8, 0, 16, 8)]);
        plan.commit(0, Some(missed));
        let pending = regions(plan.plan(1, Some(rects(&[]))));
        assert!(
            pending.contains(&PhysicalRect::new(8, 0, 16, 8)),
            "被跳过帧的 damage 必须累积到未使用的 image 上：{pending:?}"
        );
        let current = rects(&[PhysicalRect::new(0, 8, 8, 16)]);
        let pending = regions(plan.plan(1, Some(current)));
        assert!(pending.contains(&PhysicalRect::new(8, 0, 16, 8)));
        assert!(pending.contains(&PhysicalRect::new(0, 8, 8, 16)));
    }

    #[test]
    fn a_fully_synced_image_needs_no_copy() {
        let mut plan = ImageCopyPlan::new(1);
        let damage = rects(&[PhysicalRect::new(0, 0, 8, 8)]);
        plan.commit(0, Some(damage));
        assert_eq!(
            regions(plan.plan(0, Some(damage))),
            vec![PhysicalRect::new(0, 0, 8, 8)]
        );
        plan.commit(0, Some(damage));
        let empty = DamageRects::new();
        assert_eq!(
            regions(plan.plan(0, Some(empty))),
            Vec::new(),
            "与本帧 damage 无关时不需要搬任何像素"
        );
    }

    #[test]
    fn a_present_without_any_copy_keeps_the_pending_rects() {
        let mut plan = ImageCopyPlan::new(2);
        let baseline = rects(&[PhysicalRect::new(0, 0, 8, 8)]);
        plan.commit(0, Some(baseline));
        plan.commit(1, Some(baseline));
        let missed = rects(&[PhysicalRect::new(90, 90, 100, 100)]);
        plan.commit(0, Some(missed));
        assert_eq!(
            regions(plan.plan(1, Some(rects(&[])))),
            vec![PhysicalRect::new(90, 90, 100, 100)]
        );

        let also_outside = rects(&[PhysicalRect::new(90, 0, 100, 10)]);
        plan.commit_without_copy(1, Some(also_outside));
        let mut carried = regions(plan.plan(1, Some(rects(&[]))));
        carried.sort_by_key(|rect| (rect.x0, rect.y0));
        assert_eq!(
            carried,
            vec![
                PhysicalRect::new(90, 0, 100, 10),
                PhysicalRect::new(90, 90, 100, 100),
            ],
            "没搬像素的 present 不得把欠账清成空集（那会让可见区域变大后少拷一块）"
        );
        assert_eq!(
            regions(plan.plan(0, Some(rects(&[])))),
            vec![PhysicalRect::new(90, 0, 100, 10)],
            "空集 early-return 不得跳过其余 image 的累积"
        );
    }

    #[test]
    fn a_full_frame_forces_a_full_copy() {
        let mut plan = ImageCopyPlan::new(2);
        let damage = rects(&[PhysicalRect::new(0, 0, 4, 4)]);
        plan.commit(0, Some(damage));
        assert!(plan.plan(0, None).is_full());
    }

    #[test]
    fn accumulating_beyond_capacity_degrades_to_full_copies() {
        let mut plan = ImageCopyPlan::new(2);
        let full = rects(&[PhysicalRect::new(0, 0, 4, 4)]);
        plan.commit(0, Some(full));
        plan.commit(1, Some(full));
        for index in 0..(MAX_DAMAGE_RECTS as i32 + 2) {
            let damage = rects(&[PhysicalRect::new(index, 0, index + 1, 1)]);
            plan.commit(0, Some(damage));
        }
        assert!(
            plan.plan(1, Some(full)).is_full(),
            "累积溢出必须降级成整面拷贝（丢 damage 会少拷像素）"
        );
    }

    #[test]
    fn resetting_forgets_every_image() {
        let mut plan = ImageCopyPlan::new(2);
        let damage = rects(&[PhysicalRect::new(0, 0, 4, 4)]);
        plan.commit(0, Some(damage));
        plan.reset(3);
        assert_eq!(plan.len(), 3);
        for image in 0..3 {
            assert!(plan.plan(image, Some(damage)).is_full(), "image {image}");
        }
    }

    #[test]
    fn clamping_a_rect_set_keeps_only_the_part_inside_the_image() {
        let damage = rects(&[
            PhysicalRect::new(4, 8, 12, 16),
            PhysicalRect::new(-3, -2, 1, 1),
            PhysicalRect::new(58, 58, 100, 100),
            PhysicalRect::new(80, 80, 90, 90),
            PhysicalRect::new(64, 0, 64, 10),
        ]);
        let clamped = damage.clamped(64, 64);
        assert_eq!(
            clamped.as_slice(),
            &[
                PhysicalRect::new(4, 8, 12, 16),
                PhysicalRect::new(0, 0, 1, 1),
                PhysicalRect::new(58, 58, 64, 64),
            ],
            "夹紧后的集合只保留落在 image 内的部分，顺序与输入一致"
        );
        assert_eq!(clamped.clamped(64, 64).as_slice(), clamped.as_slice());
        assert!(DamageRects::new().clamped(64, 64).is_empty());
    }
}
