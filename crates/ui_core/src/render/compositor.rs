//! damage → present/copy 计划（`design/phase-08-d3d12.md` §4.4 / §6.4）。
//!
//! 纯函数、零分配：输入的物理矩形来自 [`crate::render::damage::DamageAccumulator`]，
//! 输出是后端唯一认识的三种计划。**夹紧与去空在这里做**——`Present1` 对越界/空矩形
//! 返回 `DXGI_ERROR_INVALID_CALL`（DEVIATIONS F-72），不能留给后端。

use crate::{
    math::PhysicalRect,
    render::damage::{DamageSummary, MAX_DAMAGE_RECTS},
};

/// 最多 `MAX_DAMAGE_RECTS` 个物理矩形的固定容量集合（与 damage 的阈值常量同源）。
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

    /// 追加一个矩形；已满返回 `false`（调用方退化为全帧）。
    pub(crate) fn push(&mut self, rect: PhysicalRect) -> bool {
        if self.len == MAX_DAMAGE_RECTS {
            return false;
        }
        self.rects[self.len] = rect;
        self.len += 1;
        true
    }

    pub(crate) fn as_slice(&self) -> &[PhysicalRect] {
        &self.rects[..self.len]
    }

    /// 把矩形两两合并到最多 `max_passes` 个（P1-2.1）。
    ///
    /// **为什么必须做**：后端对每个 damage 矩形重放**整份** draw op 列表。16 个矩形就是 16 次
    /// 全量重放 —— 实测使增量帧（288 µs）比全量帧（222 µs）更慢，局部刷新反而更贵。
    /// 合并后重放次数有上界，代价是被并进来的中间像素多画一遍。
    ///
    /// 策略与 Chrome 的区域合并同源：每次合并「并集面积增长最小」的一对（贪心），因此优先
    /// 合并彼此靠近、形状相近的矩形，尽量少制造空白。合并是安全的：持久纹理里那些像素本来
    /// 就是上一帧的合法内容，多画一遍同样的图元得到同样的结果；`present`/`copy` 也用同一份
    /// 矩形，所以屏幕上不会多刷。
    pub(crate) fn coalesce(&mut self, max_passes: usize) {
        let max_passes = max_passes.clamp(1, MAX_DAMAGE_RECTS);
        while self.len > max_passes {
            // 找「并集面积 − 两者面积之和」最小的一对。
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
            // 用末尾元素填补被合并掉的槽位（顺序无所谓：damage 矩形之间没有先后语义）。
            self.rects[b] = self.rects[self.len - 1];
            self.len -= 1;
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// 局部刷新最多重放多少次 draw op 列表。
///
/// 4 是从「重放成本」与「多画的空白」之间取的折中：512 图元的 op 列表一次重放约 4 µs 级别的
/// 命令录制，4 次仍在预算内；再高就会让增量帧明显慢于全量帧（那正是要修的问题）。
pub(crate) const MAX_DRAW_PASSES: usize = 4;

/// 两个矩形的并集。
fn union_rect(a: PhysicalRect, b: PhysicalRect) -> PhysicalRect {
    PhysicalRect {
        x0: a.x0.min(b.x0),
        y0: a.y0.min(b.y0),
        x1: a.x1.max(b.x1),
        y1: a.y1.max(b.y1),
    }
}

/// 合并两个矩形带来的「多画面积」（并集面积 − 两者面积之和，已去重叠）。
fn union_growth(a: PhysicalRect, b: PhysicalRect) -> u64 {
    let union = union_rect(a, b);
    union_area(union).saturating_sub(union_area(a) + union_area(b))
}

/// 矩形面积（钳到非负，空矩形为 0）。
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

/// 一帧要提交的像素工作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    /// 没有像素要动。目标尺寸未变时 `commit_frame` 的第二段门禁把它拦下（不 publish、按
    /// `Idle` 结算）；目标尺寸变化时仍会发布，让 GPU 线程按新尺寸重建渲染目标。
    Idle,
    /// 整帧：全量 copy + 全量 present。
    Full,
    /// 局部：逐矩形 copy + scissor + `Present1(dirty)`。
    Partial(DamageRects),
}

impl Plan {
    /// 本次是否必须走全量 present（供 `swapchain` 的首帧规则与测试断言共用）。
    pub(crate) fn is_full(self) -> bool {
        matches!(self, Self::Full)
    }

    /// 两帧的 damage 合并（`design/async-render-pipeline.md` §4.2 / I5）。
    ///
    /// UI 线程的「最新帧槽」覆盖旧请求时，旧请求的像素工作没有发生（GPU 线程只会画最新的那份
    /// 快照），所以它的 damage 必须由后继帧补画：**并集**而不是交集。
    ///
    /// 规则（逐字对照 §4.2）：任一 `Full` ⇒ `Full`；否则矩形并集；并集超过
    /// `MAX_DAMAGE_RECTS` ⇒ `Full`（`Present1` / scissor 的固定容量不能溢出）。
    ///
    /// 拼接之后**按绘制预算合并**（P2-3.2）：GPU 忙时被覆盖的帧会把 damage 一路累积，
    /// 纯拼接很容易凑到 16 个矩形，而每个矩形都要重放一遍 draw op 列表 —— 合并到
    /// `MAX_DRAW_PASSES` 个正是为了给这个重放次数设上界（与 `plan()` 里同一套规则）。
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

/// 本帧的**唯一**提交状态：绘制范围 `plan` + 「swapchain 是否新建/重建」合并在一起。
///
/// 合并的原因（B3）：`fresh` 不是独立维度——它同时决定 ①必须全量拷贝（否则 back buffer
/// 上是上一帧的残留）与 ②`Present1` 必须 `DirtyRectsCount = 0`（F-72 ①）。把它和 `plan`
/// 放在同一个值里，就不可能出现「fresh 但走 Partial 拷贝」这种自相矛盾的组合；
/// `fresh` 只在 `present` **成功之后**才清除（失败时下一帧仍按首帧全量处理）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FramePlan {
    plan: Plan,
    fresh: bool,
}

impl FramePlan {
    pub(crate) fn new(plan: Plan, fresh: bool) -> Self {
        Self { plan, fresh }
    }

    /// 绘制/clear 的范围。
    pub(crate) fn plan(self) -> Plan {
        self.plan
    }

    /// 本帧的 dirty 集：`None` = 全量（D3D12 的 `DirtyRectsCount = 0`）。
    ///
    /// **消费方**：D3D12 用它同时派生 `Present1` 的 dirty rect 与 copy 范围（`fresh` ⇒ 全量
    /// copy 的规则在 Windows 后端的 `copy_plan` 里），Vulkan 用它生成
    /// `VK_KHR_incremental_present` 的 region hint，Metal 用它决定「这张 drawable 相对 persist
    /// 还差哪些矩形」（`render/macos/compositor.rs` 的 `DrawableSyncs`，P2-6）。
    /// 任何后端都**不得**把它当作「必须跳过的像素」：present 目标跨帧内容在 Metal/Vulkan 上
    /// 没有保证，draw/persist 侧仍按 `plan()` 的 scissor 处理。
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

    /// 本帧是否必须全量（绘制、copy、present 三者一致）。
    pub(crate) fn is_full(self) -> bool {
        self.fresh || self.plan.is_full()
    }

    /// `fresh` 是否仍然成立：`true` = 交换链刚建好，第一次 present 之前 back buffer 的内容未定义。
    ///
    /// 消费方：D3D12 后端的 `copy_to_back`（fresh ⇒ 整块 buffer，否则只拷内容子矩形）与
    /// `swapchain` 的首帧 present 规则；Metal 后端的 drawable 同步表（fresh ⇒ 强制整面拷贝）。
    #[cfg(any(windows, test))]
    pub(crate) fn is_fresh(self) -> bool {
        self.fresh
    }

    /// `Present1` 成功之后调用：从下一帧起不再需要全量。
    pub(crate) fn presented(&mut self) {
        self.fresh = false;
    }
}

/// 三条输入：本帧 damage 摘要、合并后的物理矩形、surface 的物理矩形。规则见 §4.4（逐字）。
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
        // P1-2.1：合并到至多 `MAX_DRAW_PASSES` 个，给「每个矩形重放整份 op 列表」设上界。
        rects.coalesce(MAX_DRAW_PASSES);
        Plan::Partial(rects)
    }
}

#[cfg(test)]
mod tests {
    use super::{DamageRects, FramePlan, MAX_DRAW_PASSES, Plan, plan, union_growth};
    use crate::{
        math::PhysicalRect,
        render::damage::{DamageSummary, MAX_DAMAGE_RECTS},
    };

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
        // 绘制仍是局部，但 present 必须全量（back buffer 上是上一帧的残留）。
        assert!(matches!(frame.plan(), Plan::Partial(_)));
        assert_eq!(frame.present_dirty(), None);
        assert!(frame.is_full() && frame.is_fresh());

        // 只有 present 成功之后才清 fresh。
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
        // 右/下边界 + 1 的矩形：与 surface 的交集为空 ⇒ 丢弃（`Present1` 的 dirty rect 不得越界）。
        let outside = PhysicalRect::new(101, 51, 120, 70);
        assert_eq!(plan(partial(1), &[outside], surface()), Plan::Idle);

        // 跨边界：夹回 `surface.x1` / `surface.y1`。
        let straddling = PhysicalRect::new(90, 40, 101, 51);
        match plan(partial(1), &[straddling], surface()) {
            Plan::Partial(rects) => {
                assert_eq!(rects.as_slice(), &[PhysicalRect::new(90, 40, 100, 50)]);
            }
            other => panic!("unexpected plan: {other:?}"),
        }

        // 全量不受 surface 影响：仍然整帧（不经过矩形夹紧路径）。
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

    // ── §4.2：最新帧槽的 damage 合并 ─────────────────────────────────────────

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
                // 旧帧的 damage 在前、新帧的在后：补画顺序与产出顺序一致。
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
        // 恰好在容量内：不再退化成全量，而是**合并到绘制预算**（P2-3.2）。
        // 旧行为是原样保留 16 个矩形 —— 每个矩形都要重放一遍 draw op 列表。
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
        // 合并必须覆盖全部原始矩形（合并只是放大重绘范围，不能丢 damage）。
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
    /// P1-2.1：damage 必须合并到至多 `MAX_DRAW_PASSES` 个（后端按矩形数重放 op 列表）。
    #[test]
    fn damage_is_coalesced_to_the_pass_budget() {
        // 走真实入口 plan()（partial_plan 辅助函数直接构造 Plan::Partial，绕过合并）。
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

    /// 合并必须**覆盖**原始区域：合并后的矩形并集不得丢掉任何一个原始矩形。
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

    /// 少于预算时不得合并（合并会扩大重绘面积）。
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

    /// 贪心合并优先选「面积增长最小」的一对。
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
}
