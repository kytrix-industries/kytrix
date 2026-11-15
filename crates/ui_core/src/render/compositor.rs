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

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }
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
    /// `VK_KHR_incremental_present` 的 region hint；Metal 不消费（无对应扩展，F-121）。
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

    #[cfg(test)]
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
        Plan::Partial(rects)
    }
}

#[cfg(test)]
mod tests {
    use super::{DamageRects, FramePlan, Plan, plan};
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
        // 恰好装满不算超限。
        let exactly = partial_plan(&older[..MAX_DAMAGE_RECTS - 1]).merge(partial_plan(&newer));
        assert!(matches!(exactly, Plan::Partial(rects) if rects.len() == MAX_DAMAGE_RECTS));
    }
}
