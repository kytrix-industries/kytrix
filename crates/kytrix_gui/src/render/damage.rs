//! 脏矩形合成（`design/phase-06-paint-runtime.md` §8；P02 §12 的规格落地）。
//!
//! 纯 CPU、纯函数式：给定「逻辑矩形 + 外扩量 + clip + 帧尺寸」，产出平台无关的**物理像素**脏矩形。
//! 不知道 GPU、不知道 `Present1`、不持有窗口。
//!
//! 关键契约：
//! - 输入一律**逻辑像素**，只在 `push` 里做一次 `to_physical_covering`（安全侧，P01 §7.1）；
//! - 外扩在**物理像素域** `ceil`（与 `params0.w` 同语义，P01 §7.2）；
//! - 合并判据是「面积不增」（相交/共边才合并，L 形拒绝）；
//! - 阈值：`> 16` 个矩形或面积 `> 50%` 帧面积 → `Full`；
//! - 输出形状 = [`DamageSummary`]（P06 F-61：不落 `DamageOutput`/`ScrollHint`，矩形留在累加器里，
//!   `reset` 因此能保留容量）。

use crate::math::{PhysicalRect, Rect, Scale};

/// damage 矩形数上限（三平台 dirty-rect 列表长度的安全值；P02 §12.5）。
pub(crate) const MAX_DAMAGE_RECTS: usize = 16;

/// 全帧退化的面积比例（超过半屏时全帧拷贝更便宜；P02 §12.5）。
pub(crate) const FULL_FRAME_AREA_RATIO: f32 = 0.5;

/// 一次帧末的 damage 汇总。
///
/// `full == true` 时 `rects == 0`（矩形已清空，调用方应以整帧代替）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DamageSummary {
    /// 是否退化为全帧（resize / DPI / 主题 / 阈值逃逸）。
    pub full: bool,
    /// 合并后的脏矩形数。
    pub rects: usize,
}

/// 一次帧内的 damage 累加器（`Window` 持有，跨帧复用容量）。
pub(crate) struct DamageAccumulator {
    scale: Scale,
    rects: Vec<PhysicalRect>,
    full: bool,
}

impl DamageAccumulator {
    /// 空累加器（消费者：`Window::new`）。
    pub(crate) fn new(scale: Scale) -> Self {
        Self {
            scale,
            rects: Vec::new(),
            full: false,
        }
    }

    /// 帧初重置（保留 `Vec` 容量；消费者：`Window::paint_frame`）。
    pub(crate) fn reset(&mut self, scale: Scale) {
        self.scale = scale;
        self.rects.clear();
        self.full = false;
    }

    /// 累加一个**逻辑**矩形。`aa_padding` 是**物理像素**外扩量（§6.6 的取值表）。
    ///
    /// 不可换算（非有限 / 负尺寸）→ 直接丢弃；与 `clip` 求交为空 → 丢弃。
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
        if let Some(existing) = self.rects.iter_mut().find(|a| mergeable(**a, rect)) {
            *existing = existing.union(rect);
        } else {
            self.rects.push(rect);
        }
        if self.rects.len() > MAX_DAMAGE_RECTS {
            self.full = true;
            self.rects.clear();
        }
    }

    /// 强制全帧（resize / DPI / 主题 / 字体切换；消费者：`Window::paint_frame` 的 `full_damage_pending`）。
    pub(crate) fn push_full(&mut self) {
        self.full = true;
        self.rects.clear();
    }

    /// 合并 + 阈值判定，返回本帧的汇总。`frame` 是 framebuffer 的物理矩形（面积阈值用）。
    ///
    /// 合并循环到不动点（每轮按 `(y0, x0)` 排序 ⇒ 同输入同输出，I8 的前提）。
    pub(crate) fn finish(&mut self, frame: PhysicalRect) -> DamageSummary {
        if self.full {
            return DamageSummary {
                full: true,
                rects: 0,
            };
        }
        loop {
            self.rects.sort_by_key(|rect| (rect.y0, rect.x0));
            let mut merged = false;
            let mut i = 0;
            while i < self.rects.len() {
                let mut j = i + 1;
                while j < self.rects.len() {
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
        if self.rects.len() > MAX_DAMAGE_RECTS {
            self.full = true;
            self.rects.clear();
        } else {
            let total: i64 = self.rects.iter().map(|rect| rect.area()).sum();
            let threshold = (frame.area() as f64) * f64::from(FULL_FRAME_AREA_RATIO);
            if total as f64 > threshold {
                self.full = true;
                self.rects.clear();
            }
        }
        DamageSummary {
            full: self.full,
            rects: self.rects.len(),
        }
    }

    /// 合并后的物理矩形（发布构建可读：P07 的 `Present1`/`incremental present` 翻译消费；
    /// `Window::commit_frame` 的 `debug_assert` 也读它）。
    pub(crate) fn rects(&self) -> &[PhysicalRect] {
        &self.rects
    }
}

/// 合并判据（P02 §12.4）：`area(a ∪ b) <= area(a) + area(b)`。
fn mergeable(a: PhysicalRect, b: PhysicalRect) -> bool {
    a.union(b).area() <= a.area().saturating_add(b.area())
}

#[cfg(test)]
mod tests {
    use super::{DamageAccumulator, FULL_FRAME_AREA_RATIO, MAX_DAMAGE_RECTS};
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
        // 完全包含的矩形：`area(union) < area(a) + area(b)` ⇒ 合并（P02 §12.4）。
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
    fn too_many_rects_becomes_full() {
        let mut acc = DamageAccumulator::new(scale(1.0));
        for index in 0..(MAX_DAMAGE_RECTS + 1) {
            let x = (index as f32) * 20.0;
            acc.push(rect(x, 0.0, 5.0, 5.0), 0.0, None);
        }
        let (full, count) = finish(&mut acc);
        assert!(full);
        assert_eq!(count, 0);
    }

    #[test]
    fn area_ratio_becomes_full() {
        let mut acc = DamageAccumulator::new(scale(1.0));
        // 15 个互不相交的小矩形已经超过 50% 覆盖时也要判 Full；这里用一个 900×900 的大矩形。
        acc.push(rect(0.0, 0.0, 900.0, 900.0), 0.0, None);
        let (full, _) = finish(&mut acc);
        assert!(full, "> {FULL_FRAME_AREA_RATIO} 帧面积 ⇒ Full");
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
}
