use crate::math::{PhysicalRect, Rect, Scale};

pub(crate) const MAX_DAMAGE_RECTS: usize = 16;

pub(crate) const FULL_FRAME_AREA_RATIO: f32 = 0.5;

pub(crate) const MAX_ACCUMULATED_DAMAGE_RECTS: usize = 512;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DamageSummary {
    pub full: bool,
    pub rects: usize,
}

pub(crate) struct DamageAccumulator {
    scale: Scale,
    rects: Vec<PhysicalRect>,
    full: bool,
}

impl DamageAccumulator {
    pub(crate) fn new(scale: Scale) -> Self {
        Self {
            scale,
            rects: Vec::with_capacity(MAX_DAMAGE_RECTS),
            full: false,
        }
    }

    pub(crate) fn reset(&mut self, scale: Scale) {
        self.scale = scale;
        self.rects.clear();
        self.full = false;
    }

    pub(crate) fn push(&mut self, bounds: Rect, aa_padding: f32, clip: Option<PhysicalRect>) {
        if self.full {
            return;
        }
        let mut rect = bounds.to_physical_covering(self.scale);
        if rect.is_empty() {
            return;
        }
        let pad = if aa_padding.is_nan() {
            0
        } else {
            aa_padding.max(0.0).ceil() as i32
        };
        rect = PhysicalRect::new(
            rect.x0.saturating_sub(pad),
            rect.y0.saturating_sub(pad),
            rect.x1.saturating_add(pad),
            rect.y1.saturating_add(pad),
        );
        if let Some(clip) = clip {
            rect = rect.intersect(clip);
            if rect.is_empty() {
                return;
            }
        }
        let mut index = 0;
        while index < self.rects.len() {
            let existing = self.rects[index];
            if covers(existing, rect) {
                return;
            }
            if covers(rect, existing) {
                self.rects.swap_remove(index);
            } else {
                index += 1;
            }
        }
        self.rects.push(rect);
        if self.rects.len() > MAX_ACCUMULATED_DAMAGE_RECTS {
            self.become_full();
        }
    }

    pub(crate) fn push_full(&mut self) {
        self.become_full();
    }

    pub(crate) fn finish(&mut self, frame: PhysicalRect) -> DamageSummary {
        if self.full {
            return DamageSummary {
                full: true,
                rects: 0,
            };
        }
        if self.rects.is_empty() {
            return DamageSummary::default();
        }
        self.merge_to_fixpoint();
        let threshold = (frame.area() as f64) * f64::from(FULL_FRAME_AREA_RATIO);
        let mut total: i64 = self.rects.iter().map(|rect| rect.area()).sum();
        if total as f64 > threshold {
            return self.become_full();
        }
        if self.rects.len() > MAX_DAMAGE_RECTS {
            self.aggregate_to_budget();
            total = self.rects.iter().map(|rect| rect.area()).sum();
            if total as f64 > threshold {
                return self.become_full();
            }
        }
        debug_assert!(
            self.rects.len() <= MAX_DAMAGE_RECTS,
            "aggregation must respect the draw-pass budget"
        );
        DamageSummary {
            full: false,
            rects: self.rects.len(),
        }
    }

    pub(crate) fn rects(&self) -> &[PhysicalRect] {
        &self.rects
    }

    fn become_full(&mut self) -> DamageSummary {
        self.full = true;
        self.rects.clear();
        DamageSummary {
            full: true,
            rects: 0,
        }
    }

    fn merge_to_fixpoint(&mut self) {
        loop {
            self.sort_by_position();
            let mut merged = false;
            let mut i = 0;
            while i < self.rects.len() {
                let mut j = i + 1;
                while j < self.rects.len() && self.rects[j].y0 <= self.rects[i].y1 {
                    if mergeable(self.rects[i], self.rects[j]) {
                        self.rects[i] = self.rects[i].union(self.rects[j]);
                        self.rects.remove(j);
                        merged = true;
                    } else {
                        j += 1;
                    }
                }
                i += 1;
            }
            if !merged {
                break;
            }
        }
    }

    fn aggregate_to_budget(&mut self) {
        self.sort_by_position();
        let rows = bands(&self.rects, MAX_DAMAGE_RECTS);
        self.rects
            .sort_unstable_by_key(|rect| (rect.x0, rect.y0, rect.x1, rect.y1));
        let columns = bands(&self.rects, MAX_DAMAGE_RECTS);
        let row_area: i64 = rows.iter().map(|rect| rect.area()).sum();
        let column_area: i64 = columns.iter().map(|rect| rect.area()).sum();
        self.rects = if column_area < row_area {
            columns
        } else {
            rows
        };
        self.merge_to_fixpoint();
    }

    fn sort_by_position(&mut self) {
        self.rects
            .sort_unstable_by_key(|rect| (rect.y0, rect.x0, rect.y1, rect.x1));
    }
}

fn bands(rects: &[PhysicalRect], limit: usize) -> Vec<PhysicalRect> {
    let groups = rects.len().min(limit);
    let mut bands = Vec::with_capacity(groups);
    let mut start = 0usize;
    for group in 0..groups {
        let end = start + (rects.len() - start).div_ceil(groups - group);
        let mut band = rects[start];
        for rect in &rects[start + 1..end] {
            band = band.union(*rect);
        }
        bands.push(band);
        start = end;
    }
    bands
}

fn mergeable(a: PhysicalRect, b: PhysicalRect) -> bool {
    a.union(b).area() <= a.area().saturating_add(b.area())
}

fn covers(outer: PhysicalRect, inner: PhysicalRect) -> bool {
    outer.x0 <= inner.x0 && outer.y0 <= inner.y0 && outer.x1 >= inner.x1 && outer.y1 >= inner.y1
}

#[cfg(test)]
mod tests {
    use super::{
        DamageAccumulator, FULL_FRAME_AREA_RATIO, MAX_ACCUMULATED_DAMAGE_RECTS, MAX_DAMAGE_RECTS,
        mergeable,
    };
    use crate::math::{PhysicalRect, Rect, Scale};

    fn scale(factor: f32) -> Scale {
        Scale::new(factor).expect("合法 scale")
    }

    fn frame() -> PhysicalRect {
        PhysicalRect::new(0, 0, 1000, 1000)
    }

    fn finish(acc: &mut DamageAccumulator) -> (bool, usize) {
        let summary = acc.finish(frame());
        (summary.full, summary.rects)
    }

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::new(x, y, w, h)
    }

    fn coverage_area(acc: &DamageAccumulator) -> i64 {
        acc.rects().iter().map(|rect| rect.area()).sum()
    }

    fn is_covered(rect: PhysicalRect, outputs: &[PhysicalRect]) -> bool {
        (rect.y0..rect.y1).all(|y| {
            (rect.x0..rect.x1).all(|x| {
                outputs
                    .iter()
                    .any(|out| out.x0 <= x && x < out.x1 && out.y0 <= y && y < out.y1)
            })
        })
    }

    #[test]
    fn push_uses_covering_not_round() {
        let mut acc = DamageAccumulator::new(scale(2.0));
        acc.push(rect(10.5, 20.25, 0.25, 0.25), 0.0, None);
        let (full, count) = finish(&mut acc);
        assert!(!full);
        assert_eq!(count, 1);
        #[cfg(feature = "test-support")]
        {
            let expected = PhysicalRect::new(21, 40, 22, 41);
            assert_eq!(acc.rects(), &[expected]);
        }
    }

    #[test]
    fn push_expands_by_physical_padding() {
        let mut acc = DamageAccumulator::new(scale(1.0));
        acc.push(rect(10.0, 10.0, 2.0, 2.0), 2.0, None);
        #[cfg(feature = "test-support")]
        assert_eq!(acc.rects(), &[PhysicalRect::new(8, 8, 14, 14)]);
        let _ = finish(&mut acc);
    }

    #[test]
    fn push_intersects_clip() {
        let mut acc = DamageAccumulator::new(scale(1.0));
        let clip = PhysicalRect::new(0, 0, 20, 20);
        acc.push(rect(10.0, 10.0, 40.0, 40.0), 0.0, Some(clip));
        #[cfg(feature = "test-support")]
        assert_eq!(acc.rects(), &[PhysicalRect::new(10, 10, 20, 20)]);
        let _ = finish(&mut acc);
    }

    #[test]
    fn push_rejects_non_finite_and_negative() {
        let mut acc = DamageAccumulator::new(scale(1.0));
        acc.push(rect(f32::NAN, 0.0, 1.0, 1.0), 0.0, None);
        acc.push(rect(0.0, 0.0, -4.0, 1.0), 0.0, None);
        acc.push(rect(f32::INFINITY, 0.0, 1.0, 1.0), 0.0, None);
        let (full, count) = finish(&mut acc);
        assert!(!full);
        assert_eq!(count, 0);
    }

    #[test]
    fn merge_overlapping() {
        let mut acc = DamageAccumulator::new(scale(1.0));
        acc.push(rect(0.0, 0.0, 10.0, 10.0), 0.0, None);
        acc.push(rect(2.0, 2.0, 3.0, 3.0), 0.0, None);
        assert_eq!(finish(&mut acc), (false, 1));
    }

    #[test]
    fn merge_adjacent() {
        let mut acc = DamageAccumulator::new(scale(1.0));
        acc.push(rect(0.0, 0.0, 10.0, 10.0), 0.0, None);
        acc.push(rect(10.0, 0.0, 10.0, 10.0), 0.0, None);
        assert_eq!(finish(&mut acc), (false, 1));
    }

    #[test]
    fn reject_l_shaped_merge() {
        let mut acc = DamageAccumulator::new(scale(1.0));
        acc.push(rect(0.0, 0.0, 10.0, 10.0), 0.0, None);
        acc.push(rect(10.0, 10.0, 10.0, 10.0), 0.0, None);
        assert_eq!(finish(&mut acc), (false, 2), "只在一个角相邻 ⇒ 不合并");
    }

    #[test]
    fn merge_is_order_independent() {
        let mut a = DamageAccumulator::new(scale(1.0));
        a.push(rect(0.0, 0.0, 10.0, 10.0), 0.0, None);
        a.push(rect(5.0, 5.0, 10.0, 10.0), 0.0, None);
        a.push(rect(100.0, 100.0, 5.0, 5.0), 0.0, None);

        let mut b = DamageAccumulator::new(scale(1.0));
        b.push(rect(100.0, 100.0, 5.0, 5.0), 0.0, None);
        b.push(rect(5.0, 5.0, 10.0, 10.0), 0.0, None);
        b.push(rect(0.0, 0.0, 10.0, 10.0), 0.0, None);

        assert_eq!(finish(&mut a), finish(&mut b));
        #[cfg(feature = "test-support")]
        assert_eq!(a.rects(), b.rects());
    }

    #[test]
    fn empty_stays_partial_empty() {
        let mut acc = DamageAccumulator::new(scale(1.0));
        assert_eq!(finish(&mut acc), (false, 0));
    }

    #[test]
    fn reset_reuses_capacity_and_is_deterministic() {
        let mut acc = DamageAccumulator::new(scale(1.0));
        for index in 0..4 {
            acc.push(rect((index as f32) * 100.0, 0.0, 5.0, 5.0), 0.0, None);
        }
        let first = acc.rects().to_vec();
        acc.reset(scale(1.0));
        assert!(acc.rects().is_empty());
        for index in 0..4 {
            acc.push(rect((index as f32) * 100.0, 0.0, 5.0, 5.0), 0.0, None);
        }
        assert_eq!(acc.rects(), first.as_slice());
        assert_eq!(finish(&mut acc), (false, 4));
    }

    #[test]
    fn push_after_full_is_ignored() {
        let mut acc = DamageAccumulator::new(scale(1.0));
        acc.push_full();
        acc.push(rect(0.0, 0.0, 1.0, 1.0), 0.0, None);
        assert_eq!(finish(&mut acc), (true, 0));
    }

    #[test]
    fn area_ratio_becomes_full() {
        let mut acc = DamageAccumulator::new(scale(1.0));
        acc.push(rect(0.0, 0.0, 900.0, 900.0), 0.0, None);
        let (full, _) = finish(&mut acc);
        assert!(full, "> {FULL_FRAME_AREA_RATIO} 帧面积 ⇒ Full");
    }

    #[test]
    fn orderbook_full_depth_update_stays_partial() {
        let scale = scale(1.0);
        let screen = PhysicalRect::new(0, 0, 1920, 1080);
        const ROWS: i32 = 40;
        const COLUMNS: i32 = 2;
        const CELL_WIDTH: f32 = 150.0;
        const CELL_HEIGHT: f32 = 16.0;
        const COLUMN_PITCH: f32 = 153.0;
        const ROW_PITCH: f32 = 19.0;
        const ORIGIN_X: f32 = 600.0;
        const ORIGIN_Y: f32 = 80.0;

        let mut pushes = Vec::new();
        for row in 0..ROWS {
            for column in 0..COLUMNS {
                pushes.push(rect(
                    ORIGIN_X + column as f32 * COLUMN_PITCH,
                    ORIGIN_Y + row as f32 * ROW_PITCH,
                    CELL_WIDTH,
                    CELL_HEIGHT,
                ));
            }
        }
        const PUSHED: usize = (ROWS as usize) * (COLUMNS as usize);
        assert_eq!(pushes.len(), PUSHED);

        let mut legacy: Vec<PhysicalRect> = Vec::new();
        for bounds in &pushes {
            let physical = bounds.to_physical_covering(scale);
            let expanded = PhysicalRect::new(
                physical.x0 - 1,
                physical.y0 - 1,
                physical.x1 + 1,
                physical.y1 + 1,
            );
            if let Some(existing) = legacy.iter_mut().find(|a| mergeable(**a, expanded)) {
                *existing = existing.union(expanded);
            } else {
                legacy.push(expanded);
            }
        }
        assert_eq!(
            legacy.len(),
            PUSHED,
            "旧 greedy 判据在这组 1 px 间隙的盘口矩形上一对都不该合并"
        );
        assert!(
            legacy.len() > MAX_DAMAGE_RECTS,
            "旧实现基线：{PUSHED} > {MAX_DAMAGE_RECTS} ⇒ 本帧必然退化为全帧"
        );

        let mut acc = DamageAccumulator::new(scale);
        let mut expected = Vec::with_capacity(PUSHED);
        for bounds in &pushes {
            acc.push(*bounds, 1.0, None);
            let physical = bounds.to_physical_covering(scale);
            expected.push(PhysicalRect::new(
                physical.x0 - 1,
                physical.y0 - 1,
                physical.x1 + 1,
                physical.y1 + 1,
            ));
        }
        assert_eq!(acc.rects().len(), PUSHED, "push 阶段不做面积归并");
        let summary = acc.finish(screen);
        assert!(!summary.full, "全档更新不得退化为整帧");
        assert_eq!(summary.rects, MAX_DAMAGE_RECTS, "聚合到绘制预算 16");
        assert_eq!(
            coverage_area(&acc),
            228_608,
            "确定性覆盖面积（列带 16×152×94）"
        );
        assert!(
            coverage_area(&acc) * 100 <= screen.area() * 15,
            "覆盖面积必须 ≤ 帧面积 15%：{} / {}",
            coverage_area(&acc),
            screen.area()
        );
        assert!(
            acc.rects().windows(2).all(|pair| {
                (pair[0].y0, pair[0].x0, pair[0].y1, pair[0].x1)
                    <= (pair[1].y0, pair[1].x0, pair[1].y1, pair[1].x1)
            }),
            "rects() 必须按 (y0, x0) 有序"
        );
        for physical in &expected {
            assert!(
                is_covered(*physical, acc.rects()),
                "输入矩形 {physical:?} 未被聚合结果覆盖：{:?}",
                acc.rects()
            );
        }
    }

    #[test]
    fn random_damage_is_covered_and_order_independent() {
        #[derive(Clone, Copy, Debug)]
        struct Push {
            bounds: Rect,
            padding: f32,
            clip: Option<PhysicalRect>,
        }

        fn expected(push: Push) -> Option<PhysicalRect> {
            let mut rect = push.bounds.to_physical_covering(scale(1.0));
            if rect.is_empty() {
                return None;
            }
            let pad = if push.padding.is_nan() {
                0
            } else {
                push.padding.max(0.0).ceil() as i32
            };
            rect = PhysicalRect::new(
                rect.x0.saturating_sub(pad),
                rect.y0.saturating_sub(pad),
                rect.x1.saturating_add(pad),
                rect.y1.saturating_add(pad),
            );
            if let Some(clip) = push.clip {
                rect = rect.intersect(clip);
                if rect.is_empty() {
                    return None;
                }
            }
            Some(rect)
        }

        let mut state = 0x5EED_1234_ABCD_9876u64;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 33) as u32).wrapping_add(1)
        };
        let screen = PhysicalRect::new(0, 0, 240, 240);
        let mut over_budget_frames = 0usize;
        for case in 0..20 {
            let count = 8 + (next() % 20) as usize;
            let mut pushes = Vec::with_capacity(count);
            for _ in 0..count {
                let x = (next() % 340) as f32 - 20.0;
                let y = (next() % 340) as f32 - 20.0;
                let width = 1.0 + (next() % 30) as f32;
                let height = 1.0 + (next() % 30) as f32;
                let padding = (next() % 3) as f32;
                let clip = if next() % 3 == 0 {
                    let x0 = (next() % 140) as i32 - 10;
                    let y0 = (next() % 140) as i32 - 10;
                    Some(PhysicalRect::new(
                        x0,
                        y0,
                        x0 + 1 + (next() % 170) as i32,
                        y0 + 1 + (next() % 170) as i32,
                    ))
                } else {
                    None
                };
                pushes.push(Push {
                    bounds: rect(x, y, width, height),
                    padding,
                    clip,
                });
            }
            let expected_rects: Vec<PhysicalRect> =
                pushes.iter().filter_map(|push| expected(*push)).collect();

            let mut first = DamageAccumulator::new(scale(1.0));
            for push in &pushes {
                first.push(push.bounds, push.padding, push.clip);
            }
            if first.rects().len() > MAX_DAMAGE_RECTS {
                over_budget_frames += 1;
            }
            let first_summary = first.finish(screen);
            let first_rects = first.rects().to_vec();
            assert_eq!(first_summary.rects, first_rects.len());
            assert!(
                first_rects.len() <= MAX_DAMAGE_RECTS,
                "case {case}: 输出超预算"
            );
            assert!(
                first_rects.windows(2).all(|pair| {
                    (pair[0].y0, pair[0].x0, pair[0].y1, pair[0].x1)
                        <= (pair[1].y0, pair[1].x0, pair[1].y1, pair[1].x1)
                }),
                "case {case}: rects() 必须有序"
            );
            for physical in &expected_rects {
                let visible = physical.intersect(screen);
                if visible.is_empty() {
                    continue;
                }
                if first_summary.full {
                    continue;
                }
                assert!(
                    is_covered(visible, &first_rects),
                    "case {case}: 输入 {physical:?} 未被覆盖：{first_rects:?}"
                );
            }

            let mut shuffled = pushes.clone();
            for index in (1..shuffled.len()).rev() {
                let swap = (next() as usize) % (index + 1);
                shuffled.swap(index, swap);
            }
            let mut second = DamageAccumulator::new(scale(1.0));
            for push in &shuffled {
                second.push(push.bounds, push.padding, push.clip);
            }
            let second_summary = second.finish(screen);
            assert_eq!(
                (first_summary.full, first_summary.rects),
                (second_summary.full, second_summary.rects),
                "case {case}: 打乱 push 顺序改变了 summary"
            );
            assert_eq!(
                first_rects,
                second.rects(),
                "case {case}: 打乱 push 顺序改变了输出矩形"
            );
        }
        assert!(
            over_budget_frames > 0,
            "属性测试必须覆盖到聚合路径（至少一帧 > {MAX_DAMAGE_RECTS} 个输入）"
        );
    }

    #[test]
    fn sixteen_rects_stay_partial() {
        let mut acc = DamageAccumulator::new(scale(1.0));
        for index in 0..MAX_DAMAGE_RECTS as i32 {
            acc.push(rect((index * 59) as f32, 20.0, 58.0, 100.0), 0.0, None);
        }
        let screen = PhysicalRect::new(0, 0, 1200, 1000);
        let summary = acc.finish(screen);
        assert!(!summary.full);
        assert_eq!(summary.rects, MAX_DAMAGE_RECTS);
        assert_eq!(coverage_area(&acc), 92_800);
    }

    #[test]
    fn seventeen_rects_aggregate_instead_of_full() {
        let mut acc = DamageAccumulator::new(scale(1.0));
        let mut expected = Vec::new();
        for index in 0..(MAX_DAMAGE_RECTS as i32 + 1) {
            acc.push(rect((index * 59) as f32, 20.0, 58.0, 100.0), 0.0, None);
            expected.push(PhysicalRect::new(index * 59, 20, index * 59 + 58, 120));
        }
        let screen = PhysicalRect::new(0, 0, 1200, 1000);
        let summary = acc.finish(screen);
        assert!(!summary.full, "17 个矩形必须先聚合，不得直接整帧");
        assert!(summary.rects <= MAX_DAMAGE_RECTS);
        assert_eq!(coverage_area(&acc), 98_700);
        for physical in &expected {
            assert!(
                is_covered(*physical, acc.rects()),
                "输入 {physical:?} 未被覆盖：{:?}",
                acc.rects()
            );
        }
    }

    #[test]
    fn area_threshold_boundary_is_exact() {
        fn run(width: i32, height: i32) -> (bool, i64) {
            let mut acc = DamageAccumulator::new(scale(1.0));
            for index in 0..(MAX_DAMAGE_RECTS as i32 + 1) {
                acc.push(rect((index * 59) as f32, 20.0, 58.0, 100.0), 0.0, None);
            }
            let summary = acc.finish(PhysicalRect::new(0, 0, width, height));
            (summary.full, coverage_area(&acc))
        }

        let (full, area) = run(1400, 141);
        assert!(!full, "恰好 50% 帧面积必须保持 Partial（阈值是严格大于）");
        assert_eq!(area, 98_700);

        let (full, _) = run(1400, 140);
        assert!(full, "> 50% 帧面积必须 Full");
    }

    #[test]
    fn scattered_rects_fall_back_to_full() {
        let mut state = 0x1234_5678_9ABC_DEF0u64;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as u32
        };
        let mut acc = DamageAccumulator::new(scale(1.0));
        for _ in 0..512 {
            let x = (next() % 1900) as f32;
            let y = (next() % 1060) as f32;
            acc.push(rect(x, y, 2.0, 2.0), 0.0, None);
        }
        let summary = acc.finish(PhysicalRect::new(0, 0, 1920, 1080));
        assert!(summary.full, "512 个散布全帧的小矩形聚合后必然超过 50%");
        assert_eq!(summary.rects, 0, "Full 时矩形必须清空");
        assert!(acc.rects().is_empty());
    }

    #[test]
    fn accumulation_cap_falls_back_to_full_immediately() {
        let mut acc = DamageAccumulator::new(scale(1.0));
        for index in 0..=(MAX_ACCUMULATED_DAMAGE_RECTS as i32) {
            let x = (index % 32) as f32 * 3.0;
            let y = (index / 32) as f32 * 3.0;
            acc.push(rect(x, y, 1.0, 1.0), 0.0, None);
        }
        assert_eq!(
            acc.rects().len(),
            0,
            "超过硬上限后矩形清空（Full 时 rects 为空）"
        );
        let summary = acc.finish(frame());
        assert!(summary.full);
        assert_eq!(summary.rects, 0);
    }
}
