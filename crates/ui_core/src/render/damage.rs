//! 脏矩形合成（P02 §12 的规格落地）。
//!
//! 纯 CPU、纯函数式：给定「逻辑矩形 + 外扩量 + clip + 帧尺寸」，产出平台无关的**物理像素**脏矩形。
//! 不知道 GPU、不知道 `Present1`、不持有窗口。
//!
//! 关键契约：
//! - 输入一律**逻辑像素**，只在 `push` 里做一次 `to_physical_covering`（安全侧，P01 §7.1）；
//! - 外扩在**物理像素域** `ceil`（与 `params0.w` 同语义，P01 §7.2）；
//! - 合并判据是「面积不增」（相交/共边才合并，L 形拒绝）；
//! - 阈值：面积 `> 50%` 帧面积 → `Full`；输出矩形数硬上限 `MAX_DAMAGE_RECTS = 16`
//!   （超过 16 个先在帧末聚合，聚合后覆盖面积仍 > 50% ⇒ `Full`；累计超过
//!   [`MAX_ACCUMULATED_DAMAGE_RECTS`] 也直接 `Full`）；
//! - 输出形状 = [`DamageSummary`]（P06 F-61：不落 `DamageOutput`/`ScrollHint`，矩形留在累加器里，
//!   `reset` 因此能保留容量）。
//!
//! # X3：从「第 17 个矩形就整帧」到「累计 + 帧末聚合」
//!
//! 旧实现只要累计到第 17 个**不可合并**矩形就整帧：盘口 40–50 档 × 2–3 列每 tick 同时变化时，
//! 16 个矩形的预算必然被击穿，于是每个 tick 都退化成整帧，Windows `Present1` 的脏矩形与合成端
//! 局部拷贝在最热场景完全失效（review §3.1 P1-5）。Chrome 的 `cc::Display` damage 列表与 Qt 的
//! backing-store region 都是「先累计、再在提交前规范化/聚合」的模型，这里对齐它们：
//!
//! 1. [`DamageAccumulator::push`]（热路径，O(n) 扫描）只做**与顺序无关的包含吸收**：新矩形被
//!    已有矩形包含 ⇒ 丢弃；新矩形包含已有矩形 ⇒ 吃掉旧的。累计超过
//!    [`MAX_ACCUMULATED_DAMAGE_RECTS`] 才立刻 `Full`（防病态帧的 CPU/内存）；
//! 2. [`DamageAccumulator::finish`]（每帧一次）先按 `(y0, x0, y1, x1)` 排序，把「面积不增」的
//!    可合并对并到不动点（判据与旧实现相同），再对超过 16 个的集合做**行/列带聚类**：
//!    分别按 y、x 排序切成 16 段连续带、取面积和更小的那个轴，最后按 50% 面积阈值判决。
//!
//! 为什么面积合并放在 `finish` 而不是 `push`：旧的在线贪心（「找到第一个可合并的旧矩形就并」）
//! 的合并**身份**依赖插入顺序 —— 同一组输入换一个 push 顺序会得到不同的大矩形（例如三个互相
//! 共边的矩形，先并不同的两个会留下不同的划分）。归并判据本身是「面积不增」，只要在**排序后的
//! 规范输入**上做，输出就与 push 顺序无关（I8 的确定性前提）；代价只是小 damage 帧多一次
//! ≤16 元素的排序/扫描（纳秒级）。
//!
//! 聚合只保证「覆盖 ⊇ 本帧所有实际重绘区域」：多覆盖的像素在持久目标里本来就是上一帧的合法
//! 内容，重画一遍结果不变；漏覆盖才是残影。矩形之间的空隙/别的矩形不会被聚合删掉。

use crate::math::{PhysicalRect, Rect, Scale};

/// damage 矩形数上限（三平台 dirty-rect 列表长度 + 每个矩形重放整份 draw op 的预算；P02 §12.5）。
pub(crate) const MAX_DAMAGE_RECTS: usize = 16;

/// 全帧退化的面积比例（超过半屏时全帧拷贝更便宜；P02 §12.5）。
pub(crate) const FULL_FRAME_AREA_RATIO: f32 = 0.5;

/// 累加器最多保留多少个未被包含关系吸收的矩形；超过即整帧。
///
/// 取值论证：一个 `PhysicalRect` 16 B，512 个 = 8 KiB/窗口。盘口 40–50 档 × 2–3 列的实测
/// damage 输入是 80–150 个（每档 1–2 个 lane 矩形），512 留了 3–4 倍余量；再往上时本帧的
/// 重绘区域已经散布到「16 个聚合矩形」与整帧拷贝成本相当，整帧是更便宜且更简单的选择。
/// 上限同时把 `finish` 的归并/聚合成本钉在常数级（512 个矩形排序 + 一次线性切带）。
pub(crate) const MAX_ACCUMULATED_DAMAGE_RECTS: usize = 512;

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
    /// 本帧累计的物理脏矩形（包含吸收之后、尚未做面积归并）。
    rects: Vec<PhysicalRect>,
    full: bool,
}

impl DamageAccumulator {
    /// 空累加器（消费者：`Window::new`）。
    pub(crate) fn new(scale: Scale) -> Self {
        Self {
            scale,
            // 绝大多数帧 ≤ `MAX_DAMAGE_RECTS` 个；先给一帧的预算，overflow 帧再增长。
            rects: Vec::with_capacity(MAX_DAMAGE_RECTS),
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
    ///
    /// 这里只做包含吸收（见模块文档）：它是**与 push 顺序无关**的规范化（输入多重集的最大元），
    /// 让后续 `finish` 的归并/聚合结果是输入的纯函数。
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
                // 已被覆盖：新矩形不带来任何新的重绘区域。
                return;
            }
            if covers(rect, existing) {
                // 新矩形覆盖旧矩形：并集不变，删掉旧矩形。
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

    /// 强制全帧（resize / DPI / 主题 / 字体切换；消费者：`Window::paint_frame` 的 `full_damage_pending`）。
    pub(crate) fn push_full(&mut self) {
        self.become_full();
    }

    /// 合并 + 聚合 + 阈值判定，返回本帧的汇总。`frame` 是 framebuffer 的物理矩形（面积阈值用）。
    ///
    /// 三步，都是确定性函数（同输入同输出，且与 `push` 顺序无关）：
    /// 1. 按 `(y0, x0, y1, x1)` 排序后把「面积不增」的可合并对并到不动点（旧判据/旧行为）；
    /// 2. 覆盖面积超 50% ⇒ `Full`（聚合只会把包围盒撑大，先判可以省掉聚合成本）；
    /// 3. 超过 [`MAX_DAMAGE_RECTS`] ⇒ 行/列带聚类到 16 个并再判一次面积阈值。
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

    /// 合并后的物理矩形（发布构建可读：P07 的 `Present1`/`incremental present` 翻译消费；
    /// `Window::commit_frame` 的 `debug_assert` 也读它）。
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

    /// 把「面积不增」的可合并对并到不动点。每轮按 `(y0, x0, y1, x1)` 排序 ⇒ 同输入同输出。
    fn merge_to_fixpoint(&mut self) {
        loop {
            self.sort_by_position();
            let mut merged = false;
            let mut i = 0;
            while i < self.rects.len() {
                let mut j = i + 1;
                // 已按 y0 升序：`y0 > 当前矩形 y1` 的对在 y 轴上必有间隙 ⇒ 不可合并，
                // 后面的 j 只会更远。这个 break 只跳过必然不可合并的对，不改变结果。
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

    /// 把超过预算的矩形集合聚类成至多 [`MAX_DAMAGE_RECTS`] 个覆盖矩形。
    ///
    /// 行/列带聚类（review §3.1 P1-5 的建议方向）：分别按 y、x 排序切成 16 段连续带，每段取
    /// 包围盒；取覆盖面积更小的那个轴。盘口这种「固定列宽、多行同时变」的场景选列带，每个
    /// 带只覆盖同一列里的连续几档，不会把整行盘口拉成一个横跨屏幕的大矩形。
    ///
    /// 切带后有可能出现互相重叠/包含的带（排序键不同的矩形共处多个带），再跑一次不动点归并：
    /// 合并只会用并集（面积不增），最终输出两两不可合并 ⇒ 两两不相交 ⇒ `rects()` 的面积和
    /// 等于真实覆盖面积，面积阈值语义保持「不相交矩形面积和」。
    fn aggregate_to_budget(&mut self) {
        self.sort_by_position();
        let rows = bands(&self.rects, MAX_DAMAGE_RECTS);
        self.rects
            .sort_unstable_by_key(|rect| (rect.x0, rect.y0, rect.x1, rect.y1));
        let columns = bands(&self.rects, MAX_DAMAGE_RECTS);
        let row_area: i64 = rows.iter().map(|rect| rect.area()).sum();
        let column_area: i64 = columns.iter().map(|rect| rect.area()).sum();
        // 并列时取行带（确定性；两种切法的覆盖都不小于输入）。
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

/// 把已按某个轴排序的矩形切成至多 `limit` 段连续带，每段取包围盒。
///
/// 分组按数量均分（`17 ⇒ 2 + 15×1`），确定性；组内成员在排序序上连续，因此带与带之间只
/// 共享排序轴上的空隙，不会跨到相隔很远的区域。
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

/// 合并判据（P02 §12.4）：`area(a ∪ b) <= area(a) + area(b)`。
fn mergeable(a: PhysicalRect, b: PhysicalRect) -> bool {
    a.union(b).area() <= a.area().saturating_add(b.area())
}

/// `outer` 是否覆盖（包含）`inner` 的全部像素。
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

    /// 输出矩形的覆盖面积（`finish` 后两两不可合并 ⇒ 两两不相交，面积和 = 真实覆盖面积）。
    fn coverage_area(acc: &DamageAccumulator) -> i64 {
        acc.rects().iter().map(|rect| rect.area()).sum()
    }

    /// `rect` 是否被 `outputs` 的并集完整覆盖（逐像素扫描；测试里的矩形都很小）。
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
        // 一个 900×900 的大矩形已经超过 50% 覆盖时也要判 Full。
        acc.push(rect(0.0, 0.0, 900.0, 900.0), 0.0, None);
        let (full, _) = finish(&mut acc);
        assert!(full, "> {FULL_FRAME_AREA_RATIO} 帧面积 ⇒ Full");
    }

    // ── X3：聚合测试 ────────────────────────────────────────────────────────

    /// 场景一：盘口「全档更新」。
    ///
    /// 1920×1080 帧、40 行 × 2 列同时变化；行间/列间逻辑间隙 3 px，AA 外扩 1 px 后物理间隙
    /// 1 px（不共享边 ⇒ 旧 greedy 判据（面积不增）一对也合并不了）。旧实现第 17 个矩形就整帧
    /// （`80 > 16` ⇒ `Full`）；新实现累计完 80 个后在帧末聚合成 16 个。确定性数字：
    /// - 旧判据合并后仍是 80 个 ⇒ `80 > MAX_DAMAGE_RECTS` ⇒ 旧实现 `Full`（见下面的基线断言）；
    /// - 新实现输出 16 个矩形，覆盖面积 16 × (152 × 94) = 228,608 px² = 帧面积 11.03%（< 15%）。
    ///
    /// 坐标/数字都是显式常量：任何让聚合法/轴选择变回整帧或覆盖变松的改动都会让本测试变红。
    #[test]
    fn orderbook_full_depth_update_stays_partial() {
        let scale = scale(1.0);
        let screen = PhysicalRect::new(0, 0, 1920, 1080);
        const ROWS: i32 = 40;
        const COLUMNS: i32 = 2;
        // 列：逻辑 150 px 宽、列间 3 px；AA 外扩 1 px ⇒ 物理 152×18，间隙 1 px。
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

        // 基线：旧实现「第一个可合并就合并、超 16 立即 Full」在这组输入上一对也合并不了，
        // 因此第 17 个矩形就整帧（review §3.1 P1-5 的退化场景）。
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

        // 新实现：80 个输入 → 帧末聚合。
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
        // 输出按 (y0, x0) 有序（I8 的确定性前提）。
        assert!(
            acc.rects().windows(2).all(|pair| {
                (pair[0].y0, pair[0].x0, pair[0].y1, pair[0].x1)
                    <= (pair[1].y0, pair[1].x0, pair[1].y1, pair[1].x1)
            }),
            "rects() 必须按 (y0, x0) 有序"
        );
        // 覆盖性：每个输入矩形都被输出并集覆盖（damage 是超集即可）。
        for physical in &expected {
            assert!(
                is_covered(*physical, acc.rects()),
                "输入矩形 {physical:?} 未被聚合结果覆盖：{:?}",
                acc.rects()
            );
        }
    }

    /// 覆盖性 property：确定性 LCG 生成 20 组随机矩形（相交/包含/负坐标/clip/外扩），断言
    /// 每个输入矩形都被输出并集覆盖；把 push 顺序伪随机打乱后，输出必须逐字节一致。
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
            // 8–27 个输入：既有小 damage 帧，也必然覆盖到 > 16 的聚合路径。
            let count = 8 + (next() % 20) as usize;
            let mut pushes = Vec::with_capacity(count);
            for _ in 0..count {
                // 逻辑坐标含负值（-20）和超出 screen 的部分：夹紧由帧/后端负责，覆盖性只要求
                // 帧内可见部分被覆盖。
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
            // 聚合路径：push 阶段还没归并，累计数 > 预算就说明本组会走行/列带聚合。
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
                    // 全帧：整个帧都会被重画 ⇒ 可见部分天然被覆盖。
                    continue;
                }
                assert!(
                    is_covered(visible, &first_rects),
                    "case {case}: 输入 {physical:?} 未被覆盖：{first_rects:?}"
                );
            }

            // 顺序打乱：输出必须与 push 顺序无关（同一输入多重集 ⇒ 同一结果）。
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

    /// 矩形数边界：恰好 16 个保持 Partial（不聚合、不整帧）。
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
        // 16 × 58 × 100 = 92,800
        assert_eq!(coverage_area(&acc), 92_800);
    }

    /// 矩形数边界：17 个必须聚合到 ≤16（不得直接整帧），且聚合后仍覆盖全部输入。
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
        // 前两段并成一个带：117 × 100；其余 15 个 58 × 100 ⇒ 11700 + 87000 = 98,700。
        assert_eq!(coverage_area(&acc), 98_700);
        for physical in &expected {
            assert!(
                is_covered(*physical, acc.rects()),
                "输入 {physical:?} 未被覆盖：{:?}",
                acc.rects()
            );
        }
    }

    /// 面积阈值边界（精确构造）：聚合后面积恰为 50% 帧面积 ⇒ Partial；超过 50% ⇒ Full。
    ///
    /// 同一组 17 个矩形聚合后覆盖面积恒为 98,700：
    /// - 1400×141 帧 = 197,400 ⇒ 恰好 50% ⇒ 保持 Partial；
    /// - 1400×140 帧 = 196,000 ⇒ 98,700 > 98,000 = 50% ⇒ Full。
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

    /// 退化：极端散乱（512 个互不相交的小矩形散布全帧）⇒ 行/列带会被撑到超过 50% 帧面积 ⇒ Full。
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

    /// 硬上限：累计超过 [`MAX_ACCUMULATED_DAMAGE_RECTS`] 直接 Full（不等帧末聚合）。
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
