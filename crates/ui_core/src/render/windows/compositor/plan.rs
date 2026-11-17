//! 呈现目标状态机 + 本帧的绘制/拷贝计划（纯逻辑，可在 WARP 与纯 CPU 下测试）。
//!
//! 这一层回答三个问题，答案**只**来自 [`TargetState`] 与 `Plan`：
//! 1. 这一帧画进哪里（back buffer 直绘 / persist 整帧 / persist 局部）——
//!    [`frame_draw`]；
//! 2. 从 persist 往当前 back buffer 拷哪些矩形（R08-8 的按 buffer 记账）——
//!    [`BufferSyncs::take_copy`]；
//! 3. `Present1` 的 dirty 集（由 record 产出的**已夹紧**矩形，见 R08-10）。
//!
//! 三个「必须全量」的旧状态源（`Swapchain::first_present` / `GpuRenderer::fresh` /
//! `FramePlan::fresh`）在 R08-11 里合并成唯一一个 [`TargetState`]：
//!
//! ```text
//! 创建 / ResizeBuffers ──► Fresh ──present 成功──► PersistInvalid
//!                                                     │
//!            直绘整帧（Plan::Full）──► PersistInvalid  │ 整帧画进 persist
//!                                                     ▼
//!                                              PersistValid ──局部帧──►（保持）
//! ```
//!
//! 「必须全量」因此只有一个来源：`TargetState::Fresh` ⇒ 下一帧整帧绘制 + 整面 present；
//! 局部帧只在 `PersistValid` 时成立。

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

/// 呈现目标的内容有效性（R08-11 的唯一状态源）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TargetState {
    /// 交换链刚创建 / 刚 `ResizeBuffers`：back buffer 内容未定义，`Present1` 必须整面。
    Fresh,
    /// persist 的内容 = 上一个成功呈现的内容：增量帧可以在 persist 上局部绘制 + 逐矩形拷贝。
    PersistValid,
    /// persist 内容不可信（刚创建 / 刚被直绘帧跳过 / 刚重建）：下一帧必须整帧重画。
    PersistInvalid,
}

impl TargetState {
    pub(crate) fn is_fresh(self) -> bool {
        matches!(self, Self::Fresh)
    }

    pub(crate) fn is_persist_valid(self) -> bool {
        matches!(self, Self::PersistValid)
    }

    /// 交换链（重新）建好：back buffer 与 persist 都不可信。
    pub(crate) fn on_target_created(&mut self) {
        *self = Self::Fresh;
    }

    /// `Present1` 成功：不再是首帧；persist 的有效性由绘制路径决定（不变）。
    pub(crate) fn on_presented(&mut self) {
        if self.is_fresh() {
            *self = Self::PersistInvalid;
        }
    }

    /// `Present1` 失败：保守地要求下一帧整帧重画 + 整面 present（back buffer 内容不可信）。
    pub(crate) fn on_present_failed(&mut self) {
        *self = Self::Fresh;
    }

    /// 本帧把整帧直接画进了 back buffer（persist 没被碰 ⇒ 内容过期）。
    pub(crate) fn on_direct_draw(&mut self) {
        *self = Self::PersistInvalid;
    }

    /// 本帧把整帧画进了 persist（persist 与当前内容一致）。
    pub(crate) fn on_full_persist_draw(&mut self) {
        *self = Self::PersistValid;
    }
}

/// 本帧画进哪里（R08-8）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameDraw {
    /// 整帧直接画进**当前 back buffer**，整面 present：**零拷贝**。
    ///
    /// flip 模型存在的意义就是让 DWM 直接从 back buffer 合成；旧实现在全量帧上也先画进
    /// persist 再整块拷一次（4K@120 约 8 GB/s 的额外读写）。全量帧的每个像素都会被覆盖，只有
    /// 局部帧才需要 persist 的跨帧内容。
    Direct,
    /// 整帧画进 persist，再整块拷到 back buffer。
    ///
    /// 用于没有交换链的离屏目标（`render_snapshot` / 读回）—— 那里没有「直接画进 back
    /// buffer」这回事；也用于交换链路径上 persist 过期但本帧是局部帧的场景（离屏）。
    Full,
    /// 局部画进 persist + 按 buffer 记账只拷矩形（增量帧的稳态路径）。
    Incremental,
}

/// 本帧是否必须按**整帧**绘制（R08-10 的升级规则）。
///
/// 两种来源：
/// 1. `FramePlan::is_full()`（显式全量帧，或 fresh）；
/// 2. 局部帧，但 damage 夹紧之后**一个可画矩形都不剩**（旧 surface 的矩形全部越界）——
///    这一帧的可见区域必然有变化，按整帧重画才是安全的（**不是错误**）。
pub(crate) fn requires_full_draw(frame: FramePlan, damage: Option<&DamageRects>) -> bool {
    frame.is_full() || (matches!(frame.plan(), Plan::Partial(_)) && damage.is_none())
}

/// 本帧的绘制模式。`has_swapchain = false` 是离屏 / 快照目标（没有 back buffer）。
///
/// 本帧的绘制模式（R08-8 的三档，按代价从低到高）。
///
/// 1. **整帧**（`Plan::is_full()` / fresh / 不可达的 Idle）：有 back buffer 就直绘它（零拷贝）；
///    没有（离屏 / 快照）就整帧画进 persist。
/// 2. **局部帧 + persist 有效**：局部画进 persist + 按 buffer 记账拷贝（稳态路径）。
/// 3. **局部帧 + persist 过期**（上一帧是直绘），这一帧把整帧画进 persist 并整块拷一次，
///    从下一帧起回到增量 —— 每个「全量帧」最多付一次重建，连续全量帧则全部零拷贝。
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

/// 一帧要往当前 back buffer 拷的范围（`BufferSyncs::take_copy` 的结果）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CopyExtent {
    /// 整个内容矩形：这张 back buffer 的内容无法证明与 persist 一致。
    Full,
    /// 只拷这些矩形：这张 back buffer 自上次同步以来 persist 变化过的区域并集。
    Rects(DamageRects),
}

/// 一张 back buffer 落后 persist 多少。
#[derive(Clone, Copy, Debug)]
struct BufferSyncState {
    /// 自上次同步以来错过的 damage（本帧 present 它时必须补拷）。
    pending: DamageRects,
    /// 自上次同步以来出现过整帧重画 / 内容从未同步过：整块不可信。
    full: bool,
}

impl BufferSyncState {
    const UNSYNCED: Self = Self {
        pending: DamageRects::new(),
        full: true,
    };
}

/// flip 模型的 back buffer 池深度。
///
/// **派生自** [`BUFFER_COUNT`]（不是重复的 `3`）：R08-14 若要改 buffer 数量，这里的槽表必须
/// 一起变，否则 `index % SYNC_SLOTS` 会静默把两张 buffer 映射到同一个槽（AR3 §4.2 缺口 2）。
const SYNC_SLOTS: usize = BUFFER_COUNT as usize;

/// 按 **back buffer 身份**追踪 persist 同步状态的固定容量表（照搬 macOS 的 `DrawableSyncs`）。
///
/// `BufferSyncs` 解决的是「当前要 present 的那张 back buffer 停在几帧前」：flip 模型下 in-flight
/// 帧最多 3 张，`FLIP_SEQUENTIAL` 的 present 目标跨帧内容在**规范上没有保证**（旧实现只拷本帧
/// damage、假定其余像素保留，这是未文档化行为，见审查报告 5.4-A）。每张 back buffer 记下自己
/// 错过的 damage，present 前一次补齐；**任何无法证明同步的状态**（新表项、期间出现过整帧、
/// 累计面积达到内容）都退化为整块拷贝 —— 带宽上界因此不超过改动前。
pub(crate) struct BufferSyncs {
    slots: [BufferSyncState; SYNC_SLOTS],
}

impl BufferSyncs {
    pub(crate) const fn new() -> Self {
        Self {
            slots: [BufferSyncState::UNSYNCED; SYNC_SLOTS],
        }
    }

    /// 交换链（重新）建好：全部 buffer 的内容都不可信。
    pub(crate) fn invalidate(&mut self) {
        self.slots = [BufferSyncState::UNSYNCED; SYNC_SLOTS];
    }

    /// 本帧 present 这张 back buffer：返回要补拷的范围，并把状态推进到「内容 = persist」。
    pub(crate) fn take_copy(
        &mut self,
        index: u32,
        damage: Option<&DamageRects>,
        content: (u32, u32),
    ) -> CopyExtent {
        let slot = &mut self.slots[index as usize % SYNC_SLOTS];
        let copy = match damage {
            // 整帧：整面都变过，逐矩形没有意义。
            None => CopyExtent::Full,
            // 这张 buffer 期间错过过整帧重画：累计矩形覆盖不了整面。
            Some(_) if slot.full => CopyExtent::Full,
            Some(damage) => {
                let mut rects = slot.pending;
                merge_damage(&mut rects, damage);
                if copy_area(&rects, content) >= u64::from(content.0) * u64::from(content.1) {
                    // 矩形之间可能重叠：一次整块拷反而更便宜。
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

    /// 本帧没有 present 这张 back buffer：把它又错过的改动记下来。
    pub(crate) fn record_missed(&mut self, index: u32, damage: Option<&DamageRects>) {
        let slot = &mut self.slots[index as usize % SYNC_SLOTS];
        match damage {
            None => {
                slot.full = true;
                slot.pending = DamageRects::new();
            }
            // 已经整块不可信时累计矩形没有意义（下次反正整块拷）。
            Some(damage) if !slot.full => merge_damage(&mut slot.pending, damage),
            Some(_) => {}
        }
    }

    /// 除了 `presented` 之外的每张 buffer 都记一次「错过了本帧」。
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

/// 把 `damage` 并入累计集；容量不够时先按绘制预算合并腾空位（与 `Plan::merge` 同一套规则）。
fn merge_damage(pending: &mut DamageRects, damage: &DamageRects) {
    for rect in damage.as_slice() {
        if pending.push(*rect) {
            continue;
        }
        // 单帧 damage 至多 `MAX_DRAW_PASSES` 个（`plan` / `Plan::merge` 都合并到这个上限），
        // 合并到该值以下必然腾出足够空位放下本帧剩余的矩形。
        pending.coalesce(MAX_DAMAGE_RECTS - MAX_DRAW_PASSES);
        debug_assert!(
            pending.push(*rect),
            "a frame's damage set exceeds the draw-pass budget"
        );
    }
}

/// 裁剪到内容矩形后各矩形面积之和（重叠部分重复计）：这次拷贝实际要付的像素带宽。
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

/// 把 `FramePlan` 的 dirty 集**夹紧**到当前内容矩形（R08-10 的后端二次防御）。
///
/// `plan()` 已经在 UI 线程按**当帧** surface 夹过一次，但 `LatestFrame` 合并时「矩形来自旧帧、
/// surface 来自新帧」（`src/render/frame.rs`）：旧帧的矩形可能越界。越界 dirty rect 是
/// `Present1` 的 `DXGI_ERROR_INVALID_CALL`、`CopyTextureRegion` 的 `D3D12_BOX` 越界（调试层报错
/// / 驱动未定义行为）—— 不能把 API 合法性交给一条没有类型的跨模块约定。
///
/// 返回 `None` 表示「退化为整帧」：有矩形整块落在内容之外（旧 surface 更大）、或数量超出绘制
/// 预算。空集（所有矩形都在内容之外）也按整帧处理：那一帧的可见区域必然有变化。
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
        // 全部矩形都在内容之外：旧帧的 damage 对新 surface 没有意义，整帧更安全。
        return None;
    }
    clamped.coalesce(MAX_DRAW_PASSES);
    Some(clamped)
}

/// 消费者：`record` 的拷贝步骤。
/// 把 persist 的指定范围拷进当前 back buffer（状态过渡在两端成对完成）。
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
        // 全帧也用 `CopyTextureRegion`（与局部同一路径）：swapchain back buffer 上
        // `CopyResource` 在 flip model 下会被拒绝（本机实测 DEVICE_REMOVED）。
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

/// 一次资源状态过渡（`RENDER_TARGET` ↔ `COPY_*` ↔ `PRESENT`）。
///
/// 消费者：本模块与 `record` / `persist`。
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
    // SAFETY: `barrier` 在本次调用期间独占；`slice::from_ref` 不转移所有权。
    unsafe { list.ResourceBarrier(core::slice::from_ref(&barrier)) };
    // SAFETY: union 载荷与 `pResource` 都是 `ManuallyDrop`，结构体 drop **不会**释放 COM 引用；
    // 被 barrier 过的资源（尤其是 swapchain back buffer）会被永久钉住（DEVIATIONS F-86）。
    // 记录已经提交，这里显式释放。
    unsafe {
        let transition: &mut D3D12_RESOURCE_TRANSITION_BARRIER = &mut barrier.Anonymous.Transition;
        core::mem::ManuallyDrop::drop(&mut transition.pResource);
    }
}

/// 一次纹理区域拷贝（半开区间 ↔ `D3D12_BOX` 的闭区间）。
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
    // SAFETY: 两个 location 在本次调用期间有效，`region` 由调用方夹紧到内容矩形内。
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
    // SAFETY: `pResource` 是 `ManuallyDrop`，结构体 drop 不释放 COM 引用；不显式释放会让
    // swapchain back buffer 永远存活（DEVIATIONS F-86）。
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

    /// R08-11：唯一状态源的转移表（present 成功 / 失败、直绘、整帧重画）。
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

    /// R08-8：全量帧直绘 back buffer（零拷贝），局部帧才走 persist。
    #[test]
    fn full_frames_draw_directly_into_the_back_buffer() {
        // 窗口路径：全量帧与 fresh 帧都直绘。
        assert_eq!(
            frame_draw(TargetState::PersistValid, Plan::Full, true),
            FrameDraw::Direct
        );
        assert_eq!(
            frame_draw(TargetState::Fresh, Plan::Full, true),
            FrameDraw::Direct
        );
        // fresh 的局部帧也必须整帧（back buffer 内容未定义）。
        assert_eq!(
            frame_draw(
                TargetState::Fresh,
                partial(&[PhysicalRect::new(0, 0, 8, 8)]),
                true
            ),
            FrameDraw::Direct
        );
        // persist 过期 + 局部帧：这一帧整帧重建 persist（下一帧起回到增量）。
        assert_eq!(
            frame_draw(
                TargetState::PersistInvalid,
                partial(&[PhysicalRect::new(0, 0, 8, 8)]),
                true
            ),
            FrameDraw::Full
        );
        // 稳态的局部帧：唯一走 persist 的路径。
        assert_eq!(
            frame_draw(
                TargetState::PersistValid,
                partial(&[PhysicalRect::new(0, 0, 8, 8)]),
                true
            ),
            FrameDraw::Incremental
        );
        // 离屏目标没有 back buffer：只能画进 persist。
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

    /// R08-10：后端二次防御 —— 越界矩形被夹紧，全部越界 / 超预算退化为整帧。
    #[test]
    fn the_backend_clamps_damage_to_the_current_content() {
        let content = (800, 600);
        // 旧 surface 的矩形（1200×900 时代的 damage）在当前 surface 内被夹到右下角。
        let dirty = damage(&[PhysicalRect::new(700, 500, 1200, 900)]);
        let clamped = clamp_damage(Some(&dirty), content).expect("inside");
        assert_eq!(clamped.as_slice(), &[PhysicalRect::new(700, 500, 800, 600)]);

        // 完全在内容之外 ⇒ 整帧（旧帧的 damage 对新 surface 没有意义）。
        let outside = damage(&[PhysicalRect::new(810, 10, 900, 40)]);
        assert_eq!(clamp_damage(Some(&outside), content), None);
        // 没有 dirty 集（整帧）本来就是整帧。
        assert_eq!(clamp_damage(None, content), None);
        // 满容量（`MAX_DAMAGE_RECTS` 个）的 dirty 集：夹紧之后必须收敛到绘制预算，而不是整帧。
        let mut many = DamageRects::new();
        for index in 0..MAX_DAMAGE_RECTS as i32 {
            assert!(many.push(PhysicalRect::new(index, 0, index + 1, 1)));
        }
        let clamped = clamp_damage(Some(&many), content).expect("capacity fits");
        assert!(clamped.as_slice().len() <= MAX_DRAW_PASSES);
        // 预算内的矩形保持原样（夹紧是幂等的）。
        let small = damage(&[PhysicalRect::new(0, 0, 4, 4)]);
        let clamped = clamp_damage(Some(&small), content).expect("inside");
        assert_eq!(clamped.as_slice(), small.as_slice());
        let _ = FramePlan::new(Plan::Full, false);
    }

    /// R08-8：按 buffer 记账 —— 每张 back buffer 只补拷它错过的 damage。
    #[test]
    fn buffer_syncs_copy_only_what_each_buffer_missed() {
        let content = (200, 200);
        let mut syncs = BufferSyncs::new();
        // 新表项：内容从未同步过 ⇒ 整块。
        assert_eq!(syncs.take_copy(0, None, content), CopyExtent::Full);

        let frame_a = damage(&[PhysicalRect::new(0, 0, 20, 20)]);
        let frame_b = damage(&[PhysicalRect::new(100, 100, 120, 120)]);
        // buffer 1 首发（整块），此后 buffer 0/2 各错过一帧，再轮到它们时只拷错过的部分。
        assert_eq!(syncs.take_copy(1, None, content), CopyExtent::Full);
        // 表项第一次被 present 之前一定整块（内容无法证明与 persist 一致）。
        assert_eq!(syncs.take_copy(2, None, content), CopyExtent::Full);
        syncs.record_missed(0, Some(&frame_a));
        syncs.record_missed(2, Some(&frame_a));
        syncs.record_missed(0, Some(&frame_b));
        syncs.record_missed(2, Some(&frame_b));

        let copy = syncs.take_copy(2, Some(&frame_b), content);
        let CopyExtent::Rects(rects) = copy else {
            panic!("expected rect copy, got {copy:?}");
        };
        // 并集必须覆盖它错过的两帧（frame_a 与 frame_b），且面积远小于整面。
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
        // 同步之后立刻再做一次：只剩本帧的 damage。
        let copy = syncs.take_copy(2, Some(&frame_b), content);
        let CopyExtent::Rects(rects) = copy else {
            panic!("expected rect copy after syncing");
        };
        assert_eq!(rects.as_slice(), frame_b.as_slice());
    }

    /// 期间出现过整帧重画 ⇒ 该 buffer 的累计矩形不可信，退化为整块拷贝（带宽上界 = 改动前）。
    #[test]
    fn a_full_frame_invalidates_every_buffer_sync() {
        let content = (200, 200);
        let mut syncs = BufferSyncs::new();
        assert_eq!(syncs.take_copy(0, None, content), CopyExtent::Full);
        syncs.record_missed_others(0, None);
        // buffer 1 下一次出现：整块（它错过了整帧）。
        assert_eq!(
            syncs.take_copy(1, Some(&damage(&[PhysicalRect::new(0, 0, 4, 4)])), content),
            CopyExtent::Full
        );
        // swapchain 重建之后所有 buffer 都必须整块。
        syncs.invalidate();
        assert_eq!(
            syncs.take_copy(2, Some(&damage(&[PhysicalRect::new(0, 0, 4, 4)])), content),
            CopyExtent::Full
        );
    }

    /// 累计面积达到整面时退化为整块（矩形重叠会让逐矩形拷贝更贵）。
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

    /// R08-10：夹紧之后「没有可画矩形」的局部帧必须升级为整帧，而不是留下一个空绘制。
    #[test]
    fn a_partial_frame_without_a_visible_rect_upgrades_to_a_full_draw() {
        let content = (800, 600);
        let outside = damage(&[PhysicalRect::new(810, 10, 900, 40)]);
        let frame = FramePlan::new(Plan::Partial(outside), false);
        let clamped = clamp_damage(frame.present_dirty().as_ref(), content);
        assert_eq!(clamped, None);
        assert!(super::requires_full_draw(frame, clamped.as_ref()));
        // 正常的局部帧不升级。
        let inside = damage(&[PhysicalRect::new(0, 0, 8, 8)]);
        let frame = FramePlan::new(Plan::Partial(inside), false);
        let clamped = clamp_damage(frame.present_dirty().as_ref(), content);
        assert!(!super::requires_full_draw(frame, clamped.as_ref()));
        // 全量帧永远整帧。
        assert!(super::requires_full_draw(
            FramePlan::new(Plan::Full, false),
            None
        ));
    }

    /// `FramePlan::fresh` 与 `TargetState` 的语义必须一致（迁移期的防回归）。
    #[test]
    fn the_frame_plan_fresh_flag_is_derived_from_the_target_state() {
        assert!(FramePlan::new(Plan::Full, TargetState::Fresh.is_fresh()).is_fresh());
        assert!(!FramePlan::new(Plan::Full, TargetState::PersistValid.is_fresh()).is_fresh());
    }
}
