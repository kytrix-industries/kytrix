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

    /// 把另一组矩形并入本集合（并集）；容量不足返回 `false`（调用方必须降级成整面，
    /// **不得**丢弃矩形）。
    ///
    /// 完全相同的矩形只留一份：同一块 damage 会被「本帧提交 + 后续每帧的累积」反复并入，
    /// 不去重的话 3 张 image 的链上很快就会凑满容量并退化成整面拷贝。
    ///
    /// 消费方是 [`ImageCopyPlan`]（Linux 的差分拷贝）；Windows / macOS 各自已有等价机制
    /// （`copy_plan` / `DrawableSyncs`），因此非 Linux 构建只在测试里编译它。
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

/// 本次要把 persist 的哪一部分同步到一张 presentable image。
///
/// 与 [`ImageCopyPlan`] 同源：只有 Linux 的 Vulkan 后端消费（Windows / macOS 各自已有等价机制，
/// 见 `copy_plan` / `DrawableSyncs`），因此非 Linux 构建只在测试里编译它。
#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CopyPlan {
    /// 整面拷贝（该 image 的内容未知，或差集超出固定容量）。
    Full,
    /// 只拷贝这些矩形（已被 accept：拷贝多于此集合永远安全，拷贝少了不安全）。
    Regions(DamageRects),
}

#[cfg(any(target_os = "linux", test))]
impl CopyPlan {
    /// 本次的拷贝矩形；`None` = 整面（`VkImageCopy` 覆盖整张图）。
    ///
    /// 唯一消费者是 Linux 的 Vulkan 后端（`copy_to_swapchain`）。
    #[cfg(target_os = "linux")]
    pub(crate) fn regions(self) -> Option<DamageRects> {
        match self {
            Self::Full => None,
            Self::Regions(regions) => Some(regions),
        }
    }

    /// 本次是否整面拷贝（测试断言用）。
    #[cfg(test)]
    pub(crate) fn is_full(self) -> bool {
        matches!(self, Self::Full)
    }
}

/// 单张 presentable image 的同步状态（R11-12，与 D3D12 的 `copy_plan` / Metal 的
/// `DrawableSyncs` 同构）。
#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImageSync {
    /// 内容未知：新建 / 重建的 swapchain、present 失败之后的 image。本帧必须整面拷贝。
    Unknown,
    /// 相对 persist 还差这些矩形（空 = 已追平）。下一次用到这张 image 时只拷它们。
    Pending(DamageRects),
}

/// 每张 presentable image 的「相对 persist 还差哪些矩形」（R11-12）。
///
/// **为什么需要**：present 之后 swapchain image 的内容对应用不可信，因此后端必须自己保证
/// 「这张 image 上每个像素至少被正确写过一次，且写过之后每次变化都被再写一次」。旧实现每帧
/// 整面 `vkCmdCopyImage`（1080p@120Hz ≈ 1.0 GB/s，4K ≈ 4 GB/s 纯带宽），把 damage/scissor
/// 的收益全部抵消。
///
/// 状态机（合法的只有一种推进方式）：
/// - 新 / 重建的 swapchain：全部 `Unknown`（第一帧整面拷贝，建立基线）；
/// - 整面拷贝之后：本 image `Pending(空)`，其余 image 累积本帧的 damage；
/// - 局部拷贝：本 image 记为 `Pending(空)`，其余 image 把本帧 damage 并入自己的 `Pending`
///   （**必须累积**：中间被跳过的那几帧的 damage 只有落到对应 image 上，该 image 才追得上
///   persist —— 这正是旧实现「每帧整面」想掩盖的问题）；
/// - 任何累积超过 [`MAX_DAMAGE_RECTS`] 或整帧 damage：降级成 `Unknown`（整面拷贝），
///   而不是丢掉 damage（丢 damage 会让下次用到该 image 时少拷一块）。
///
/// **假设**：image 的像素内容在 present / acquire 之间保持不变（与 D3D12 的 flip 模型、
/// Metal 的 drawable 池相同的假设）。规范上这个假设不成立，因此这里是「相对 persist 的差集」，
/// 而不是「跳过未标脏像素」：一旦累积状态丢失，我们退回整面拷贝，屏幕内容始终正确。
#[cfg(any(target_os = "linux", test))]
pub(crate) struct ImageCopyPlan {
    images: Vec<ImageSync>,
}

#[cfg(any(target_os = "linux", test))]
impl ImageCopyPlan {
    /// 新链（`count` 张 image，全部 `Unknown`）。
    pub(crate) fn new(count: usize) -> Self {
        Self {
            images: vec![ImageSync::Unknown; count],
        }
    }

    /// swapchain 重建 / 尺寸变化：全部回到 `Unknown`（旧表按旧 image 索引，不得复用）。
    pub(crate) fn reset(&mut self, count: usize) {
        self.images.clear();
        self.images.resize(count, ImageSync::Unknown);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.images.len()
    }

    /// 本帧对 `image` 的拷贝计划。
    ///
    /// `written` 是本帧真正写进 persist 的矩形（`None` = 整面）。
    pub(crate) fn plan(&self, image: u32, written: Option<DamageRects>) -> CopyPlan {
        let Some(ImageSync::Pending(pending)) = self.images.get(image as usize) else {
            return CopyPlan::Full;
        };
        let Some(written) = written else {
            // 本帧整面重画了 persist ⇒ 整面拷贝（差集没有意义）。
            return CopyPlan::Full;
        };
        let mut regions = *pending;
        if !regions.extend(written) {
            return CopyPlan::Full;
        }
        regions.coalesce(MAX_DRAW_PASSES);
        // 空集是合法结果：本 image 与 persist 已经完全同步，这一帧没有像素需要搬。
        CopyPlan::Regions(regions)
    }

    /// present 成功之后推进状态。
    ///
    /// `written` 与 [`ImageCopyPlan::plan`] 的入参相同（用来判断「其余 image 是否需要降级成
    /// 整面」）。返回被降级成整面拷贝的 image 数（诊断）。
    pub(crate) fn commit(&mut self, image: u32, written: Option<DamageRects>) -> usize {
        let Some(current) = self.images.get_mut(image as usize) else {
            return 0;
        };
        // 本 image 已经被这一帧追平（整面拷贝也满足这一点）。
        *current = ImageSync::Pending(DamageRects::new());
        // 其余 image 累积本帧的 damage：它们的下一次 present 必须把这块补上。
        let mut degraded = 0usize;
        for (index, entry) in self.images.iter_mut().enumerate() {
            if index == image as usize {
                continue;
            }
            match entry {
                // 内容未知的 image 反正要整面拷贝，累积没有意义。
                ImageSync::Unknown => {}
                ImageSync::Pending(pending) => {
                    let Some(written) = written else {
                        // 本帧整面重写了 persist：其余 image 一定落后，下一帧整面拷贝。
                        *entry = ImageSync::Unknown;
                        degraded += 1;
                        continue;
                    };
                    if !pending.extend(written) {
                        // 累积超出固定容量：降级成整面拷贝（绝不丢 damage）。
                        *entry = ImageSync::Unknown;
                        degraded += 1;
                    }
                }
            }
        }
        degraded
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

    // ── R11-12：每 image 的差分拷贝计划 ──────────────────────────────────────

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

    /// 新链的每张 image 都是 `Unknown`：第一次用到它必须整面拷贝（建立基线）。
    #[test]
    fn a_new_swapchain_copies_every_image_in_full_once() {
        let mut plan = ImageCopyPlan::new(3);
        assert_eq!(plan.len(), 3);
        let damage = rects(&[PhysicalRect::new(0, 0, 8, 8)]);
        for image in 0..3 {
            assert!(plan.plan(image, Some(damage)).is_full(), "image {image}");
            plan.commit(image, Some(damage));
        }
        // 全部建立基线之后，同样的 damage 只需要拷那一块。
        assert_eq!(
            regions(plan.plan(0, Some(damage))),
            vec![PhysicalRect::new(0, 0, 8, 8)],
            "基线建立后只拷 damage"
        );
    }

    /// 其余 image 必须**累积**被跳过帧的 damage：否则那张 image 下次被用到时会少拷一块。
    #[test]
    fn skipped_frames_accumulate_damage_for_every_image() {
        let mut plan = ImageCopyPlan::new(2);
        let baseline = rects(&[PhysicalRect::new(0, 0, 8, 8)]);
        // 两张 image 各自建立基线（第一帧写到 image 0，第二帧写到 image 1）。
        plan.commit(0, Some(baseline));
        plan.commit(1, Some(baseline));
        // 第三帧只写 image 0：image 1 必须记住那一块还差着。
        let missed = rects(&[PhysicalRect::new(8, 0, 16, 8)]);
        plan.commit(0, Some(missed));
        let pending = regions(plan.plan(1, Some(rects(&[]))));
        assert!(
            pending.contains(&PhysicalRect::new(8, 0, 16, 8)),
            "被跳过帧的 damage 必须累积到未使用的 image 上：{pending:?}"
        );
        // 本帧自己的 damage 也在拷贝集合里（与累积的取并集）。
        let current = rects(&[PhysicalRect::new(0, 8, 8, 16)]);
        let pending = regions(plan.plan(1, Some(current)));
        assert!(pending.contains(&PhysicalRect::new(8, 0, 16, 8)));
        assert!(pending.contains(&PhysicalRect::new(0, 8, 8, 16)));
    }

    /// 完全同步的 image 不需要任何拷贝（这是差分拷贝的收益来源）。
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

    /// 整帧 damage（`written = None`）必须整面拷贝：这一帧 persist 被整体重写。
    #[test]
    fn a_full_frame_forces_a_full_copy() {
        let mut plan = ImageCopyPlan::new(2);
        let damage = rects(&[PhysicalRect::new(0, 0, 4, 4)]);
        plan.commit(0, Some(damage));
        assert!(plan.plan(0, None).is_full());
    }

    /// 累积超出固定容量时降级成整面拷贝，**绝不丢 damage**（降级后每张 image 都是 `Unknown`）。
    #[test]
    fn accumulating_beyond_capacity_degrades_to_full_copies() {
        let mut plan = ImageCopyPlan::new(2);
        let full = rects(&[PhysicalRect::new(0, 0, 4, 4)]);
        plan.commit(0, Some(full));
        plan.commit(1, Some(full));
        // 每帧给 image 0 一块互不相同的小矩形：image 1 的 pending 会不断累积直到溢出。
        for index in 0..(MAX_DAMAGE_RECTS as i32 + 2) {
            let damage = rects(&[PhysicalRect::new(index, 0, index + 1, 1)]);
            plan.commit(0, Some(damage));
        }
        assert!(
            plan.plan(1, Some(full)).is_full(),
            "累积溢出必须降级成整面拷贝（丢 damage 会少拷像素）"
        );
    }

    /// `reset`（swapchain 重建）清掉全部状态：新链的 image 索引与旧链无关。
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
}
