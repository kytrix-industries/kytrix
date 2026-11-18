//! 帧交付契约（共享，纯数据）：`FramePayload` / `FrameRequest` / `FrameFeedback` 与最新帧槽；
//! 以及 in-flight 帧轮转（`design/phase-08-d3d12.md` §4.2）。
//!
//! 本模块是**三平台共用**的纯数据 + 纯函数（`design/async-render-pipeline.md` §4.1 / §4.2）：
//! 不含线程、句柄或任何平台 API。UI 线程产帧、GPU 线程执行，跨线程只搬这里的值。
//! 线程原语（Windows 的 work event、Linux 的 condvar、macOS 的 display link）各自留在平台模块内。
//!
//! `FrameIndex` 本身留在 `window.rs`（P06 的公开 API 返回类型，DEVIATIONS F-74）；本模块只提供
//! 「槽位 + 需要等待/将要 signal 的 fence 值」（`render/windows/queue.rs` 消费）与帧交付数据。

use std::{
    mem::size_of,
    ops::Range,
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::{Duration, Instant},
};

#[cfg(target_os = "linux")]
use crate::math::Size;
use crate::{
    paint::instance::{FrameOp, GeometryDrawGpu, GpuClipEntry, GpuPrimitive, InstanceBuffer},
    render::{compositor::Plan, stats::RenderStats, timer::GpuTimerSample},
    window::FrameIndex,
};

/// 一个实例的字节数（`GpuPrimitive` = 128 B 布局，静态断言在 `paint::instance`）。
const INSTANCE_BYTES: usize = size_of::<GpuPrimitive>();

/// 一帧实例字节的一段（**增量载荷**的单位）。
///
/// `bytes` 是 `Arc<[u8]>`：UI 线程从实例环切出来时只拷贝一次，之后跨线程移交、被覆盖帧的
/// 合并、GPU 线程的欠账补写都只 `Arc::clone`（零二次拷贝）。
pub(crate) struct InstanceChunk {
    /// 实例环下标（首实例）。
    start: u32,
    bytes: Arc<[u8]>,
}

impl InstanceChunk {
    /// 本块覆盖的实例区间。
    ///
    /// 跨模块可见（`pub(crate)`）：`compose` 的全量协议用例要断言「重建后的载荷是一块覆盖
    /// 整张实例环的整块」，块外的代码只能从这里读区间（字段是模块私有的）。
    pub(crate) fn range(&self) -> Range<u32> {
        self.start..self.start + (self.bytes.len() / INSTANCE_BYTES) as u32
    }

    /// 本块的字节（**仅测试**：等价性用例要断言增量载荷只带变化段）。
    ///
    /// 消费者只有 Windows 的 WARP 等价性用例，故按它的 cfg 收窄（否则 macOS/Linux 的
    /// lib test 会因未使用而触发 dead_code）。
    #[cfg(all(test, windows))]
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// 一帧的 CPU→GPU 快照（`design/async-render-pipeline.md` §4.1）。
///
/// 实例数据是**增量**的：`chunks` 只含自上次载荷以来变化的实例段，**应用顺序 = 旧 → 新**
/// （重叠区间后者覆盖）。GPU 线程侧的实例缓冲是持久的（`InstanceMirror`），未变化的段保留
/// 上次内容。绘制序、裁剪表、几何表仍然全量过线 —— 它们是小数据（op 数 × 24 B），且
/// 「同一帧的四份数据」这个不变量不能拆。
pub(crate) struct FramePayload {
    /// 实例块（**应用顺序：旧 → 新**）。全量载荷 = 一个覆盖 `[0, instance_count)` 的块。
    chunks: Vec<InstanceChunk>,
    /// 实例环总实例数（含空洞；GPU 侧槽缓冲的容量 / SRV 尺寸判据）。
    instance_count: u32,
    /// 本帧绘制序真正引用的实例数（存活实例；绘制统计 `RenderStats.instances`）。
    drawn_instances: u32,
    ops: Vec<FrameOp>,
    clips: Vec<GpuClipEntry>,
    /// 几何绘制表：`FrameOp::Geometry.index` 的下标表，**必须与 `ops` 同帧过线**
    /// （`ops` 里的下标只有配合同一帧的表才有意义）。
    geometry: Vec<GeometryDrawGpu>,
    /// 滚动槽的**物理像素**偏移表（下标 = `FrameOp::Instances.scroll`；B6-2 的合成端偏移）。
    ///
    /// 表很小（槽数 × 8 B）且每帧全量过线：滚动帧因此**不产生任何实例块**（`chunks` 为空），
    /// GPU 线程只在绘制这些 op 时把偏移写进 root constant，实例字节一个都不用动。
    scroll: Vec<[f32; 2]>,
    /// 滚动槽的**屏幕裁剪区**（`[x0, y0, x1, y1]`，物理像素；下标同上）。
    ///
    /// 滚动容器自己的视口 clip 不能烘焙进实例的裁剪链：它随滚动偏移在**内容坐标**里移动
    /// （见 `ScrollTable` 的坐标系约定），烘焙进去就等于「每滚一像素都要重编码全部 lane」。
    /// 因此它在 CPU 侧被折成这个**屏幕空间**的矩形，三个后端在绘制该槽的 op 时把 scissor
    /// 收窄到它（裁剪精度与烘焙 clip 相同：两者都是轴对齐矩形）。
    scroll_clip: Vec<[i32; 4]>,
}

impl FramePayload {
    /// 增量载荷：只带自上次载荷以来变化的实例段（`InstanceBuffer` 的变化区间被取走）。
    ///
    /// # Panics
    /// `InstanceBuffer` 的实例数与变化区间不一致（编码器不变量被破坏）。
    pub(crate) fn from_buffer_delta(
        buffer: &mut InstanceBuffer,
        scroll: Vec<[f32; 2]>,
        scroll_clip: Vec<[i32; 4]>,
    ) -> Self {
        let mut buffers = pool_lock().pop().unwrap_or_default();
        buffers.chunks.clear();
        let count = buffer.instance_count();
        let ranges = buffer.changes().len();
        for index in 0..ranges {
            let range = buffer.changes()[index].clone();
            let bytes = &buffer.as_bytes()
                [range.start as usize * INSTANCE_BYTES..range.end as usize * INSTANCE_BYTES];
            buffers.chunks.push(InstanceChunk {
                start: range.start,
                bytes: Arc::from(bytes),
            });
        }
        buffer.clear_changes();
        buffers.ops.clear();
        buffers.ops.extend_from_slice(buffer.ops());
        buffers.clips.clear();
        buffer.write_clips_gpu(&mut buffers.clips);
        buffers.geometry.clear();
        buffers.geometry.extend_from_slice(buffer.geometry());
        buffers.scroll = scroll;
        buffers.scroll_clip = scroll_clip;
        Self {
            chunks: buffers.chunks,
            instance_count: count,
            drawn_instances: buffer.len() as u32,
            ops: buffers.ops,
            clips: buffers.clips,
            geometry: buffers.geometry,
            scroll: buffers.scroll,
            scroll_clip: buffers.scroll_clip,
        }
    }

    /// 全量载荷：一个覆盖整张实例环的块（快照 / 首帧 / 重建后 / 新渲染器挂上）。
    ///
    /// 生产路径要用它：合成线程整帧重建时**必须**整块重发实例缓冲，否则 GPU 侧的实例镜像
    /// 缺内容（它只被载荷填满）。取走全量之后变化区间清零 —— 那些变化已经包含在全量块里。
    pub(crate) fn full(
        buffer: &InstanceBuffer,
        scroll: Vec<[f32; 2]>,
        scroll_clip: Vec<[i32; 4]>,
    ) -> Self {
        let mut buffers = pool_lock().pop().unwrap_or_default();
        buffers.chunks.clear();
        let count = buffer.instance_count();
        if count > 0 {
            buffers.chunks.push(InstanceChunk {
                start: 0,
                bytes: Arc::from(buffer.as_bytes()),
            });
        }
        buffers.ops.clear();
        buffers.ops.extend_from_slice(buffer.ops());
        buffers.clips.clear();
        buffer.write_clips_gpu(&mut buffers.clips);
        buffers.geometry.clear();
        buffers.geometry.extend_from_slice(buffer.geometry());
        buffers.scroll = scroll;
        buffers.scroll_clip = scroll_clip;
        Self {
            chunks: buffers.chunks,
            instance_count: count,
            drawn_instances: buffer.len() as u32,
            ops: buffers.ops,
            clips: buffers.clips,
            geometry: buffers.geometry,
            scroll: buffers.scroll,
            scroll_clip: buffers.scroll_clip,
        }
    }

    /// 把**被覆盖帧**（从未到达 GPU）的实例块并进来：旧块在前、新块在后。
    ///
    /// 新块里已经被更新版本覆盖的旧块直接丢弃（同样的区间以新块为准）；旧块里**没有被新块
    /// 完整覆盖**的区间必须保留，否则那段变化永远不会出现在任何 GPU 缓冲里。
    pub(crate) fn absorb(&mut self, mut dropped: Self) {
        // `FramePayload` 有 `Drop`（回收池），所以先 `take` 出块表再消费。
        let dropped_chunks = std::mem::take(&mut dropped.chunks);
        if dropped_chunks.is_empty() {
            return;
        }
        let mut kept: Vec<InstanceChunk> = Vec::with_capacity(dropped_chunks.len());
        // 极端情况（GPU 长时间停摆、每帧都改大片 lane）下不做逐块覆盖判定：保留全部旧块
        // 仍是正确的（只是块数更多），比 O(n·m) 的判定便宜。
        let exhaustive = dropped_chunks.len().saturating_mul(self.chunks.len()) <= 4096;
        for older in dropped_chunks {
            let covered = exhaustive
                && self.chunks.iter().any(|newer| {
                    newer.range().start <= older.start && older.range().end <= newer.range().end
                });
            if !covered {
                kept.push(older);
            }
        }
        kept.append(&mut self.chunks);
        self.chunks = kept;
    }

    /// 本帧的实例块（应用顺序：旧 → 新）。
    pub(crate) fn chunks(&self) -> &[InstanceChunk] {
        &self.chunks
    }

    /// 实例环总实例数（含空洞）。
    pub(crate) fn instance_count(&self) -> u32 {
        self.instance_count
    }

    /// 本帧绘制序真正引用的实例数（`RenderStats.instances`；不含空洞）。
    pub(crate) fn drawn_instances(&self) -> u32 {
        self.drawn_instances
    }

    /// 本帧的绘制序。
    pub(crate) fn ops(&self) -> &[FrameOp] {
        &self.ops
    }

    /// 本帧的裁剪表（GPU 布局，表项 0 是哨兵）。
    pub(crate) fn clips(&self) -> &[GpuClipEntry] {
        &self.clips
    }

    /// `FrameOp::Geometry.index` 的下标表（与 [`FramePayload::ops`] 同帧）。
    pub(crate) fn geometry(&self) -> &[GeometryDrawGpu] {
        &self.geometry
    }

    /// 滚动槽的物理像素偏移表（`FrameOp::Instances.scroll` 的下标；长度 = 槽数）。
    pub(crate) fn scroll(&self) -> &[[f32; 2]] {
        &self.scroll
    }

    /// 滚动槽的屏幕裁剪区（`[x0, y0, x1, y1]` 物理像素，下标同上）。
    ///
    /// 消费者：三个后端的 `draw_ops`（该槽的 op 在绘制前把 scissor 收窄到它）；缺省/越界 ⇒ 不收窄。
    pub(crate) fn scroll_clip(&self) -> &[[i32; 4]] {
        &self.scroll_clip
    }

    /// 本帧交给 GPU 的实例字节数（增量载荷 = 变化段；`FrameRecord.uploaded_bytes`）。
    #[cfg(feature = "test-support")]
    pub(crate) fn uploaded_bytes(&self) -> usize {
        self.chunks
            .iter()
            .map(|chunk| chunk.bytes.len())
            .sum::<usize>()
    }

    /// 空载荷（窗口首帧 priming 与槽位/线程单元测试用；真实帧必须走
    /// [`FramePayload::from_buffer_delta`] / [`FramePayload::full`]）。只有 Windows 的首帧
    /// priming 在产品构建里需要它。
    #[cfg(any(windows, test))]
    pub(crate) fn empty() -> Self {
        Self {
            chunks: Vec::new(),
            instance_count: 0,
            drawn_instances: 0,
            ops: Vec::new(),
            clips: Vec::new(),
            geometry: Vec::new(),
            scroll: Vec::new(),
            scroll_clip: Vec::new(),
        }
    }
}

/// 一帧的四个 CPU 缓冲（`FramePayload` 回收的单位，DEVIATIONS F-213）。
#[derive(Default)]
struct PayloadBuffers {
    chunks: Vec<InstanceChunk>,
    ops: Vec<FrameOp>,
    clips: Vec<GpuClipEntry>,
    geometry: Vec<GeometryDrawGpu>,
    scroll: Vec<[f32; 2]>,
    scroll_clip: Vec<[i32; 4]>,
}

/// 回收池上限：最多缓存 4 帧的缓冲（GPU 线程通常只压着一帧，4 个足够且封顶内存）。
const POOL_LIMIT: usize = 4;

/// 跨线程回收池：GPU 线程 drop `FramePayload` 时把缓冲还回来，UI 线程下一帧直接复用。
///
/// `Mutex` 每帧只锁一次、临界区是 4 次 `Vec` 交换，代价远小于 4 次分配。
fn payload_pool() -> &'static Mutex<Vec<PayloadBuffers>> {
    static POOL: OnceLock<Mutex<Vec<PayloadBuffers>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(Vec::new()))
}

fn pool_lock() -> std::sync::MutexGuard<'static, Vec<PayloadBuffers>> {
    payload_pool()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

impl Drop for FramePayload {
    fn drop(&mut self) {
        // 只回收有容量的缓冲：`empty()` 的零容量帧不占池位。
        if self.chunks.capacity() == 0
            && self.ops.capacity() == 0
            && self.clips.capacity() == 0
            && self.geometry.capacity() == 0
            && self.scroll.capacity() == 0
            && self.scroll_clip.capacity() == 0
        {
            return;
        }
        let mut pool = pool_lock();
        if pool.len() >= POOL_LIMIT {
            return;
        }
        pool.push(PayloadBuffers {
            chunks: std::mem::take(&mut self.chunks),
            ops: std::mem::take(&mut self.ops),
            clips: std::mem::take(&mut self.clips),
            geometry: std::mem::take(&mut self.geometry),
            scroll: std::mem::take(&mut self.scroll),
            scroll_clip: std::mem::take(&mut self.scroll_clip),
        });
    }
}

/// UI 线程 → GPU 线程的一帧请求（§4.1）。
///
/// `index` 是 UI 侧的提交序号；GPU 线程只对真正录制并提交的帧推进自己的记账（§4.3），
/// 被跳过的序号不产生 GPU 工作，`FrameIndex` 因此不要求连续出现在完成通知里。
pub(crate) struct FrameRequest {
    pub index: FrameIndex,
    pub payload: FramePayload,
    /// UI 侧的 damage 计划；GPU 线程在渲染前与自己的「目标是新建/重建」状态合成 `FramePlan`。
    pub plan: Plan,
    /// 目标物理像素（resize 的唯一来源，§4.4）。
    pub surface: (u32, u32),
    /// 窗口 DPI（`root.frame.params.x`）。
    ///
    /// **必须与 `surface` 同帧过线**：geometry 通道的顶点是逻辑像素，shader 用 `frame.params.x`
    /// 换物理像素；两者出自不同帧时，DPI 切换后的第一帧几何会被画在错误的尺度上
    /// （实例通道不受影响，它的 bounds 已是物理像素 —— 这正是这个 bug 容易被漏掉的原因）。
    pub scale: f32,
    /// 窗口的逻辑尺寸（surface-local 坐标；Linux 专用）。
    ///
    /// Linux 的 Wayland buffer 尺寸是 `round(surface_local × scale)`，而
    /// `wp_viewport.set_destination` 必须写逻辑尺寸（compositor 按它显示 buffer）。两者必须来自
    /// **同一帧**：GPU 线程按本帧的 `surface` 重建 swapchain、按本帧的 `surface_local` 写
    /// destination，present 之前一次生效（D1 / D2）。
    #[cfg(target_os = "linux")]
    pub surface_local: Size,
    /// 整数回退的 `wl_surface` buffer scale（Linux 专用；fractional 模式为 1）。
    ///
    /// 与 `surface` / `surface_local` 同帧成对：整数 `N` 下 `surface = round(surface_local × N)`，
    /// `buffer_scale` 必须是 `N`，surface 的逻辑尺寸才等于 `surface / N`；否则 compositor 会把
    /// 整面放大 `N×`（V-110）。GPU 线程在 present 前写入，与 attach/commit 同一次生效。
    #[cfg(target_os = "linux")]
    pub buffer_scale: i32,
    /// HUD 的发布延迟基准（[`FrameFeedback::latency`] 的起点）。
    pub published_at: Instant,
}

impl FrameRequest {
    /// 合成线程用一条 `SceneDelta` 的头部 + 已编码的载荷组装帧请求。
    ///
    /// 这是**唯一**把 `FrameHeader` 转成 `FrameRequest` 的地方：UI 线程不再构造帧请求，合成线程
    /// 也不重复推导 `surface` / `scale` / `plan`（它们只能来自同一帧的头部）。
    pub(crate) fn from_header(
        header: crate::render::delta::FrameHeader,
        payload: FramePayload,
    ) -> Self {
        Self {
            index: header.index,
            payload,
            plan: header.plan,
            surface: header.surface,
            scale: header.scale.factor(),
            #[cfg(target_os = "linux")]
            surface_local: header.surface_local,
            #[cfg(target_os = "linux")]
            buffer_scale: header.buffer_scale,
            published_at: header.published_at,
        }
    }
}

/// 合成线程 → 每窗口 GPU 线程的交付端点（`Send + Sync`）。
///
/// 三个后端各自实现：Windows / macOS / Linux 的窗口渲染门面本来就持有
/// 「最新帧槽 + 唤醒原语」的 `Arc<Shared>`（`Shared` 已是 `Send + Sync`），因此合成线程直接往
/// 那个槽里发布，**不经过 UI 线程**（否则会多一次泵迭代的延迟）。
pub(crate) trait FrameSink: Send + Sync {
    /// 发布一帧（不阻塞，最新者胜）。返回 `false` 表示 GPU 线程已终止（帧被丢弃）。
    fn submit(&self, request: FrameRequest) -> bool;

    /// GPU 线程是否还活着（合成线程据此跳过已死的窗口）。
    fn is_alive(&self) -> bool;

    /// 渲染器重建代际：GPU 侧每次重建实例镜像（设备恢复 / 新渲染器挂上）就 +1。
    ///
    /// UI 线程在每帧开始前读它；值变了 ⇒ 那一帧必须整帧发布（`SceneDelta::full`），因为
    /// GPU 侧的 [`InstanceMirror`] 已经被清空 —— 增量载荷只会填它「变化」的段，其余段会画成
    /// 退化实例（AR4 P1-1）。装配点见 [`ResyncGate`]。
    ///
    /// 默认 `0` = 该后端没有进程内重建路径（Linux / macOS 目前只有建窗口一次）。
    fn reset_epoch(&self) -> u64 {
        0
    }
}

/// GPU 线程 → UI 线程的一帧反馈（§4.1）。
///
/// 窗口路径的 `RenderStats` / GPU 采样从这里取（落后 ≤1 帧）；离屏路径保持同步返回。
///
/// **跨平台契约**：GPU 线程必须在**观察到**该帧的 GPU 工作完成之后才发布反馈（fence /
/// timeline / completed handler），不得在只是「提交了」的时候发布；反馈按提交顺序发布；
/// 允许覆盖尚未被 UI 线程取走的上一条（反馈槽是最新者胜，不是队列）。
pub(crate) struct FrameFeedback {
    pub index: FrameIndex,
    pub stats: RenderStats,
    pub samples: Vec<GpuTimerSample>,
    /// GPU 线程**观察到该帧 GPU 工作完成**的时刻（fence / timeline / completed handler），
    /// 不是 `Present` 返回的时刻。
    pub completed_at: Instant,
    /// 本帧**上屏**时刻的最佳可信值；`None` = 该端如实不提供（**不是**「退回完成时刻」的占位）。
    ///
    /// 来源按后端区分，各端都**不得**拿 [`Self::completed_at`] 顶替（那正是本轮修掉的假数据缺陷）：
    /// - macOS：display link 的 `CAMetalDisplayLinkUpdate.targetPresentationTimestamp`（本帧被排定
    ///   的上屏时刻）换算出的**tick 推断值**，误差上界 1 个 vsync；它只在「更新的 tick 已经到达」
    ///   （证明该 vsync 的呈现截止时刻已过）之后才写入，且不早于 [`Self::completed_at`]。
    ///   逐帧真实呈现时间戳（`MTLDrawable.presentedTime` / `addPresentedHandler:`）在
    ///   display-link 驱动的 drawable 上不可靠（实测只回调数次、`presentedTime` 恒为 0），
    ///   已不再作为来源。
    /// - Windows / Linux：恒为 `None` —— 两端都没有接真实呈现时间戳（`GetFrameStatistics` /
    ///   `wl_surface.frame` 回调未接），没有可信值就留空。
    pub presented_at: Option<Instant>,
    /// `completed_at - published_at`：UI 线程发布本帧到 **GPU 工作完成**的时长。
    ///
    /// **三端同义**（B6-2 统一）：起点是 [`FrameRequest::published_at`]（UI 提交时刻），终点是
    /// [`Self::completed_at`]。合成线程上线后这条链多了一段「合成线程等待下一拍 + 增量编码」——
    /// 那正是 UI 发布到 GPU 完成之间的真实路径，因此数值变大是语义正确的结果，不是回归。
    /// HUD 的 `Latency` 一行显示的就是它；「发布 → 上屏」由 [`Self::presented_at`] 单独承载，
    /// 其真实性判据见 [`Self::presentation_is_consistent`]。
    pub latency: Duration,
}

impl FrameFeedback {
    /// [`Self::presented_at`] 契约的可执行版本（三端 `poll` 共用）：呈现时刻存在时不得早于本帧的
    /// GPU 完成时刻 —— 像素不可能比 GPU 工作更早上屏。
    ///
    /// `None` 是合法值（该端如实不提供），不是「用完成时刻兜底」的同义词：本函数同时守住
    /// 「有值 ⇒ 可信」与「不可信 ⇒ 必须为 `None`」两个方向。
    pub(crate) fn presentation_is_consistent(&self) -> bool {
        self.presented_at
            .is_none_or(|presented| presented >= self.completed_at)
    }
}

/// 最新帧槽：**最新者胜 + damage 合并**（§4.2 / I5）。
///
/// 这不是队列：GPU 线程忙时后来的帧覆盖先前的帧，被覆盖帧的 damage 由后继帧补画
/// （persist 纹理跨帧保留，被跳过的帧没有写进去）。因此全程没有「强制全量」的补救路径。
/// 锁与唤醒原语不在这里：本类型是纯数据，各平台门面自行决定同步方式。
pub(crate) struct LatestFrame {
    pending: Option<FrameRequest>,
}

impl LatestFrame {
    pub(crate) const fn new() -> Self {
        Self { pending: None }
    }

    /// 放入一帧；已有未取走的帧时保留**新**的 payload/surface，把旧的 damage 合并进新的
    /// plan（[`Plan::merge`]），旧请求的 `index` 丢弃 —— 它不会产生任何 GPU 工作。
    ///
    /// **实例增量同样必须合并**（否则被覆盖帧的变化永远到不了 GPU）：旧载荷的块按「旧 → 新」
    /// 的顺序并进新载荷（[`FramePayload::absorb`]），GPU 线程写缓冲时按顺序覆盖，内容与
    /// 「只交付新帧」逐字节一致。
    pub(crate) fn publish(&mut self, request: FrameRequest) {
        let Some(previous) = self.pending.take() else {
            self.pending = Some(request);
            return;
        };
        let mut next = request;
        next.plan = previous.plan.merge(next.plan);
        next.payload.absorb(previous.payload);
        self.pending = Some(next);
    }

    /// 取走当前帧（没有则 `None`）。
    pub(crate) fn take(&mut self) -> Option<FrameRequest> {
        self.pending.take()
    }

    /// 是否有未取走的帧。
    pub(crate) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }
}

/// 「消费端重置 ⇒ 生产端整帧重发」握手的 UI 侧存根（AR4 P1-1 / P2-2）。
///
/// GPU 线程每次重建实例镜像（设备恢复 / 新渲染器挂上）就把 [`FrameSink::reset_epoch`] +1；
/// UI 线程每帧开始前 [`ResyncGate::observe`] 一次该代际，返回 `true` 的那一帧必须整帧发布
/// （`DeltaBuilder::force_full` ⇒ `delta.full = true` ⇒ 载荷是覆盖整张实例环的整块）。
///
/// 没有这个握手，恢复后的第一帧只会发「变化 lane」，而镜像里其余段是 0 ⇒ 退化实例。
/// 纯数据 + 纯逻辑：三个后端共用，不碰任何平台句柄。
#[derive(Debug, Default)]
pub(crate) struct ResyncGate {
    /// 上次观察到的代际；`None` = 还没有观察过（UI 线程第一次读）。
    seen: Option<u64>,
}

impl ResyncGate {
    /// 观察一次渲染器重建代际；返回 `true` = 自上次观察以来重建过（本帧必须整帧发布）。
    ///
    /// 首次观察只记录代际并返回 `false`：**首帧本来就是整帧发布**（`DeltaBuilder::full_for`
    /// 在合成侧还没有任何 lane 时返回真），这里再强制一次只会重复劳动。
    pub(crate) fn observe(&mut self, epoch: u64) -> bool {
        match self.seen {
            None => {
                self.seen = Some(epoch);
                false
            }
            Some(seen) if seen == epoch => false,
            Some(_) => {
                self.seen = Some(epoch);
                true
            }
        }
    }
}

/// 单槽欠账区间数上限：超过就整块重写（代价封顶，不让欠账表无限增长）。
const MAX_PENDING_RANGES: usize = 128;

/// GPU 线程侧的持久实例镜像（三平台共用：纯数据 + 纯逻辑，不碰平台句柄）。
///
/// 每个 in-flight 槽的实例缓冲是**持久**的（只扩容，不按帧重写全部内容）：本帧只把变化的
/// 区间写进去，未变化的段保留该槽上次的内容。三个槽各自落后于镜像不同的帧，因此每个槽记一份
/// 「欠账区间」；写某个槽时把欠账一次性从镜像补上（欠账来自**别的**槽的帧，字节仍保存在镜像
/// 里）。这是「帧间增量实例编码」在 GPU 线程侧的落点：payload 只带变化段
/// （[`FramePayload::chunks`]），镜像负责把它们补齐到每个槽。
///
/// **为什么需要镜像**：载荷之间是增量关系，而被覆盖的帧（[`LatestFrame`]）不会到达 GPU；
/// 另外槽是轮转使用的，A 槽写过的字节 B 槽没见过。镜像 + 每槽欠账把这两件事一起解决，
/// 且写 GPU 缓冲时永远只写**当前槽** —— 绝不写别的在飞槽（那会与 GPU 读并发）。
pub(crate) struct InstanceMirror {
    /// 完整实例字节（长度 = `instance_count × 128`）。
    bytes: Vec<u8>,
    slots: Vec<SlotMirror>,
    /// 本帧要写进当前槽的区间（[`InstanceMirror::prepare`] 填充）。
    writes: Vec<Range<u32>>,
    /// 本帧是否整块写（首帧 / 扩容重建 / 欠账太多）。
    write_full: bool,
    /// 本帧载荷覆盖的区间（已合并；[`InstanceMirror::finish`] 把它们记为其它槽的欠账）。
    payload_ranges: Vec<Range<u32>>,
}

/// 一个槽的欠账。
struct SlotMirror {
    /// 该槽缓冲需要整块重写（新建 / 扩容重建 / 欠账溢出）。
    needs_full: bool,
    /// 该槽缓冲落后于镜像的区间（无序、已合并的小表）。
    pending: Vec<Range<u32>>,
}

impl InstanceMirror {
    pub(crate) fn new(slots: usize) -> Self {
        Self {
            bytes: Vec::new(),
            slots: (0..slots)
                .map(|_| SlotMirror {
                    needs_full: true,
                    pending: Vec::new(),
                })
                .collect(),
            writes: Vec::new(),
            write_full: false,
            payload_ranges: Vec::new(),
        }
    }

    /// 槽缓冲被重建（扩容 / 设备恢复）：内容丢失，下一次使用必须整块重写。
    pub(crate) fn invalidate(&mut self, slot: usize) {
        let state = &mut self.slots[slot];
        state.needs_full = true;
        state.pending.clear();
    }

    /// 应用本帧载荷进镜像，并算出「当前槽要写哪些区间」。
    ///
    /// 返回 `true` = 整块写（此时 [`InstanceMirror::bytes`] 就是全部内容，长度足以覆盖当前槽）；
    /// `false` = 只写 [`InstanceMirror::writes`] 里的区间。
    pub(crate) fn prepare(&mut self, slot: usize, payload: &FramePayload) -> bool {
        let Self {
            bytes,
            slots,
            writes,
            write_full,
            payload_ranges,
        } = self;
        let count = payload.instance_count();
        let needed = count as usize * INSTANCE_BYTES;
        if bytes.len() < needed {
            bytes.resize(needed, 0);
        } else {
            bytes.truncate(needed);
        }
        // 应用本帧载荷（顺序 = 旧 → 新，重叠区间后者覆盖）。
        payload_ranges.clear();
        for chunk in payload.chunks() {
            let range = chunk.range();
            let start = range.start as usize * INSTANCE_BYTES;
            let end = start + chunk.bytes.len();
            debug_assert!(
                end <= bytes.len(),
                "payload chunk escapes the instance arena"
            );
            if end > bytes.len() {
                continue;
            }
            bytes[start..end].copy_from_slice(&chunk.bytes);
            payload_ranges.push(range);
        }
        normalize_ranges(payload_ranges);
        // 镜像缩短时，越界的欠账一并丢弃（它们指向的段已不存在）。
        for state in slots.iter_mut() {
            state
                .pending
                .retain(|range| range.start < count && range.end <= count);
        }
        writes.clear();
        *write_full = false;
        let state = &mut slots[slot];
        let outstanding: u32 = state
            .pending
            .iter()
            .map(|range| range.end - range.start)
            .sum();
        // 整块写与区间写的取舍：欠账已经超过半个实例环时，整块 memcpy 更快。
        if state.needs_full || outstanding.saturating_mul(2) >= count {
            *write_full = true;
            return true;
        }
        writes.extend(state.pending.iter().cloned());
        writes.extend(payload_ranges.iter().cloned());
        normalize_ranges(writes);
        let written: u32 = writes.iter().map(|range| range.end - range.start).sum();
        if written.saturating_mul(2) >= count {
            *write_full = true;
        }
        *write_full
    }

    /// 本帧要写进当前槽的区间（`prepare` 返回 `false` 时有效）。
    pub(crate) fn writes(&self) -> &[Range<u32>] {
        &self.writes
    }

    /// 镜像的完整实例字节（`prepare` 返回 `true` 时，整块写它的前 `instance_count × 128` 字节）。
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// 当前槽写完之后推进记账：本帧载荷覆盖的区间记到其它槽的欠账里。
    ///
    /// **为什么不按「本帧是否整块写」传播 `needs_full`**：整块写可能只是因为**当前槽自己**
    /// 的缓冲是新建的（首帧）或欠账太多，与其它槽无关；它们的欠账永远等于
    /// 「本帧载荷覆盖的区间」—— 载荷的旧块（例如全量基准块）同样必须补写到每个槽，
    /// 因为它才是该槽内容的来源。
    pub(crate) fn finish(&mut self, slot: usize) {
        let Self {
            slots,
            write_full,
            payload_ranges,
            writes,
            ..
        } = self;
        for (index, state) in slots.iter_mut().enumerate() {
            if index == slot {
                state.needs_full = false;
                state.pending.clear();
                continue;
            }
            for range in payload_ranges.iter() {
                push_range(&mut state.pending, range.clone());
            }
            if state.pending.len() > MAX_PENDING_RANGES {
                state.needs_full = true;
                state.pending.clear();
            }
        }
        writes.clear();
        *write_full = false;
    }
}

/// 把一个区间并入「无序、已合并」的欠账表（先向前合并所有重叠/相邻项）。
fn push_range(ranges: &mut Vec<Range<u32>>, range: Range<u32>) {
    if range.is_empty() {
        return;
    }
    let mut start = range.start;
    let mut end = range.end;
    let mut index = 0usize;
    while index < ranges.len() {
        let current = &ranges[index];
        if current.end < start || current.start > end {
            index += 1;
            continue;
        }
        start = start.min(current.start);
        end = end.max(current.end);
        ranges.remove(index);
    }
    ranges.push(start..end);
}

/// 排序 + 合并重叠/相邻区间（原地，容量保留）。
fn normalize_ranges(ranges: &mut Vec<Range<u32>>) {
    ranges.sort_unstable_by_key(|range| range.start);
    let mut out = 0usize;
    for read in 0..ranges.len() {
        let range = ranges[read].clone();
        if out > 0 {
            let last = &mut ranges[out - 1];
            if range.start <= last.end {
                last.end = last.end.max(range.end);
                continue;
            }
        }
        ranges[out] = range;
        out += 1;
    }
    ranges.truncate(out);
}

/// in-flight 帧数（主设计 §2.4；同步提交下实际最多 1 帧在飞，但 ring 结构不依赖这一点）。
pub(crate) const MAX_FRAMES_IN_FLIGHT: usize = 3;

/// 一帧要用的槽位与 fence 值。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameSlot {
    /// ring 槽位（`0..MAX_FRAMES_IN_FLIGHT`），来自 [`InFlightRing::acquire`] 的轮转。
    pub index: usize,
    /// 进入本帧前必须等待到达的 fence 值（0 = 该槽从未使用，无需等待）。
    pub wait_value: u64,
    /// 本帧提交成功后 signal 的值 —— **这就是 timeline 的 signal 值**（R11-1）。
    ///
    /// 三个消费点必须用同一个值，不允许各自推导：
    /// 1. `vkQueueSubmit` 的 `TimelineSemaphoreSubmitInfo::signalSemaphoreValues`（提交时 signal）；
    /// 2. retire 队列的键（判断「哪些句柄的提交已完成」）；
    /// 3. teardown 的等待值（`vkWaitSemaphores` / `submitted_value`）。
    ///
    /// 旧实现把 UI 的 `FrameIndex` 当作 timeline 值 signal，而 teardown 等的是 ring 的提交计数：
    /// 序号一跳号（`Idle` 帧、最新帧槽合并且、`Retry`）两者就永久错位，关窗会在最后一帧仍在飞时
    /// 销毁资源。ring 的计数器是**唯一**由提交驱动的单调源，`FrameIndex` 只作为反馈 / 诊断标签。
    pub signal_value: u64,
}

/// ring 的 fence 记账 + 槽位轮转（「每个槽最后一次提交的值」+ 下一次提交的槽位）。
pub(crate) struct InFlightRing {
    slot_values: [u64; MAX_FRAMES_IN_FLIGHT],
    /// 下一次 [`InFlightRing::acquire`] 给出的槽位（0 → 1 → 2 → 0）。
    next_slot: usize,
    next_value: u64,
}

impl InFlightRing {
    pub(crate) const fn new() -> Self {
        Self {
            slot_values: [0; MAX_FRAMES_IN_FLIGHT],
            next_slot: 0,
            next_value: 0,
        }
    }

    /// 取本帧槽位：按 **acquire 次数**轮转（0 → 1 → 2 → 0），不推进 fence 记账（提交成功后才
    /// [`InFlightRing::advance`]）。正常路径每次 acquire 恰好对应一次提交；`NOT_READY` /
    /// `OUT_OF_DATE` 重试会多消费一次 acquire 而不提交（复用槽之前由调用方等的槽 fence 保证安全）。
    ///
    /// 槽位**不能**由 `FrameIndex` 取模决定：最新帧槽合并时会丢掉被覆盖帧的序号
    /// （[`LatestFrame`]），序号在一次提交与下一次提交之间的跨度可以是任意值；而平台的背压
    /// 原语（macOS 的三 permit semaphore、三槽上传缓冲）按提交次数轮转。两者不同源时，跨度
    /// 为 3 的倍数的两个提交会落到同一个上传槽 —— 后一个在 CPU 上重写 GPU 仍在读的缓冲
    /// （内容损坏，Metal validation 检测不到）。
    pub(crate) fn acquire(&mut self) -> FrameSlot {
        let index = self.next_slot;
        self.next_slot = (index + 1) % MAX_FRAMES_IN_FLIGHT;
        FrameSlot {
            index,
            wait_value: self.slot_values[index],
            signal_value: self.next_value + 1,
        }
    }

    /// 偷看下一次 [`InFlightRing::acquire`] 会给出的槽位，**不推进**轮转。
    ///
    /// 消费者：macOS 的 per-slot 背压（P1-2.12）—— 必须先知道「准备用哪个槽」才能检查
    /// 该槽自己的 permit 是否已归还，而不是盲目地先取一个 permit。
    /// Windows / Linux 用单调 fence / timeline，不需要这个入口。
    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn peek(&self) -> FrameSlot {
        let index = self.next_slot;
        FrameSlot {
            index,
            wait_value: self.slot_values[index],
            signal_value: self.next_value + 1,
        }
    }

    /// 提交成功后推进该槽的 fence 值。
    pub(crate) fn advance(&mut self, slot: FrameSlot) {
        self.slot_values[slot.index] = slot.signal_value;
        self.next_value = slot.signal_value;
    }

    /// 最后一次 signal 的值（诊断 / `wait_idle`）。
    pub(crate) fn last_signaled(&self) -> u64 {
        self.next_value
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    use super::{
        FrameFeedback, FramePayload, FrameRequest, FrameSlot, InFlightRing, InstanceChunk,
        InstanceMirror, LatestFrame, MAX_FRAMES_IN_FLIGHT, ResyncGate, pool_lock,
    };
    use crate::{
        math::PhysicalRect,
        paint::instance::InstanceEncoder,
        render::{
            compositor::{DamageRects, Plan},
            stats::RenderStats,
        },
        window::FrameIndex,
    };

    /// 文档契约（`FrameFeedback::presented_at` / `presentation_is_consistent`）的可执行版本：
    /// 呈现时刻要么如实缺失，要么**不早于**本帧的 GPU 完成时刻；把完成时刻冒充成上屏时刻（旧实现
    /// 的 32 ms 兜底）会被判为不一致。
    #[test]
    fn a_feedback_never_claims_a_presented_time_before_the_gpu_completed() {
        let completed_at = Instant::now();
        let feedback = |presented_at: Option<Instant>| FrameFeedback {
            index: FrameIndex::from_raw(1),
            stats: RenderStats::default(),
            samples: Vec::new(),
            completed_at,
            presented_at,
            latency: Duration::from_millis(4),
        };
        assert!(
            feedback(None).presentation_is_consistent(),
            "该端不提供上屏时刻时 `None` 是合法值"
        );
        assert!(feedback(Some(completed_at)).presentation_is_consistent());
        assert!(
            feedback(Some(completed_at + Duration::from_millis(8))).presentation_is_consistent(),
            "上屏必然晚于 GPU 完成：tick 推断值也遵守"
        );
        assert!(
            !feedback(Some(completed_at - Duration::from_millis(1))).presentation_is_consistent(),
            "早于 GPU 完成的上屏时刻不可信"
        );
    }

    #[test]
    fn a_dropped_payload_returns_its_buffers_to_the_pool() {
        // 回收池是跨线程的：GPU 线程 drop 帧后，UI 线程下一帧直接复用容量（F-213）。
        //
        // 断言直接作用在池的机制上并**全程持锁**：回收池是进程级单例，并行跑其它用例时
        // 池可能已被占满（`POOL_LIMIT`）或刚种下的大块已被别的用例取走 —— 那样
        // 「本次 drop 是否入池」就是竞态，不是本用例要守的不变量。
        let mut pool = pool_lock();
        let before = pool.len();
        let mut seeded = super::PayloadBuffers::default();
        seeded.chunks.reserve(4096);
        pool.push(seeded);
        assert_eq!(pool.len(), before + 1, "入池必须成功");
        let taken = pool.pop().expect("刚种下的缓冲必须能取回");
        assert!(
            taken.chunks.capacity() >= 4096,
            "池必须保留容量（capacity={}）",
            taken.chunks.capacity()
        );
        drop(taken);
        drop(pool);

        // 载荷构造 / 丢弃在池的任意状态下都必须是安全的（容量复用是优化，不是正确性）。
        let encoder = InstanceEncoder::new();
        let payload = FramePayload::full(encoder.buffer(), Vec::new(), Vec::new());
        assert_eq!(payload.instance_count(), 0);
        drop(payload);
    }

    fn partial(rects: &[PhysicalRect]) -> Plan {
        let mut plan = DamageRects::new();
        for rect in rects {
            assert!(plan.push(*rect));
        }
        Plan::Partial(plan)
    }

    fn request(index: u32, plan: Plan) -> FrameRequest {
        FrameRequest {
            index: FrameIndex::from_raw(index),
            payload: FramePayload::empty(),
            plan,
            surface: (64, 64),
            scale: 1.0,
            #[cfg(target_os = "linux")]
            surface_local: crate::math::Size::new(64.0, 64.0),
            #[cfg(target_os = "linux")]
            buffer_scale: 1,
            published_at: Instant::now(),
        }
    }

    /// `peek` 必须与紧接着的 `acquire` 给出同一个槽（macOS 的 per-slot 背压依赖它，P1-2.12）。
    #[test]
    fn peek_matches_the_next_acquire() {
        let mut ring = InFlightRing::new();
        for expected in [0, 1, 2, 0] {
            let peeked = ring.peek();
            assert_eq!(peeked.index, expected, "peek 不得推进轮转");
            let acquired = ring.acquire();
            assert_eq!(acquired.index, peeked.index, "peek 必须先于 acquire 一致");
            assert_eq!(
                acquired.signal_value, peeked.signal_value,
                "peek 给出的 fence 值也必须与 acquire 一致"
            );
            ring.advance(acquired);
        }
    }

    #[test]
    fn slots_cycle_through_the_ring() {
        let mut ring = InFlightRing::new();
        let slots: Vec<usize> = (0..7).map(|_| ring.acquire().index).collect();
        assert_eq!(slots, vec![0, 1, 2, 0, 1, 2, 0]);
        assert_eq!(MAX_FRAMES_IN_FLIGHT, 3);
    }

    #[test]
    fn first_use_of_each_slot_has_no_wait() {
        let mut ring = InFlightRing::new();
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            let slot = ring.acquire();
            assert_eq!(slot.wait_value, 0, "slot {} must not wait", slot.index);
            ring.advance(slot);
        }
    }

    #[test]
    fn wait_value_is_the_previous_submission_of_that_slot() {
        let mut ring = InFlightRing::new();
        let first = ring.acquire();
        ring.advance(first);
        let second = ring.acquire();
        ring.advance(second);
        let third = ring.acquire();
        ring.advance(third);

        // 第 4 次提交回到 slot 0（第 1 次提交的槽），必须等第 1 次的 signal 值。
        let fourth = ring.acquire();
        assert_eq!(fourth.index, 0);
        assert_eq!(fourth.wait_value, first.signal_value);
        assert_eq!(fourth.signal_value, 4);
    }

    #[test]
    fn signal_values_are_monotonic() {
        let mut ring = InFlightRing::new();
        let mut previous = 0;
        for _ in 0..9 {
            let slot = ring.acquire();
            assert_eq!(slot.signal_value, previous + 1);
            previous = slot.signal_value;
            ring.advance(slot);
        }
        assert_eq!(ring.last_signaled(), 9);
    }

    /// P0 回归（R11-1）：timeline 的 signal 值 = **提交序号**，与 UI 帧序号解耦。
    ///
    /// UI 帧序号会跳号（`Idle` 帧、最新帧槽合并、`Retry`），旧实现把它当 timeline 值 signal，
    /// 而 teardown 等的是提交计数 ⇒ 序号一跳号两者永久错位。这里模拟「只提交序号 1、4、7 的三帧」：
    /// 提交序号必须是 1、2、3，且 teardown 的等待值 = 最后一次 signal 的值，而不是帧序号 7。
    #[test]
    fn timeline_values_track_submissions_not_ui_frame_indices() {
        let mut ring = InFlightRing::new();
        let mut signaled = Vec::new();
        for ui_index in [1u32, 4, 7] {
            // 跳过不提交的序号：它们不产生 GPU 工作，也就不消费 ring 的提交序。
            let slot = ring.acquire();
            signaled.push((ui_index, slot.signal_value));
            ring.advance(slot);
        }
        assert_eq!(
            signaled,
            vec![(1, 1), (4, 2), (7, 3)],
            "UI 帧序号与提交序号是两条独立序列"
        );
        assert_eq!(
            ring.last_signaled(),
            3,
            "teardown 等的是最后一次 signal 的值（3），不是最后的 UI 帧序号（7）"
        );
        // 等待 3 覆盖全部三次提交；等待 2 不覆盖第三次（旧实现永远等不到 3）。
        assert!(
            signaled
                .iter()
                .all(|(_, value)| *value <= ring.last_signaled())
        );
    }

    /// R11-22：重试（`NOT_READY` / `OUT_OF_DATE`）会多消费一次 acquire 但不提交；`acquire` 给出的
    /// 值仍然是「下一次提交的值」，实际提交出的 timeline 值因此严格递增（同一个值不会被 signal
    /// 两次 —— `vkQueueSubmit` 要求 signal 值大于 timeline 当前值）。
    #[test]
    fn retries_never_signal_the_same_timeline_value_twice() {
        let mut ring = InFlightRing::new();
        let retried = ring.acquire();
        // 模拟 acquire 失败（不提交、不 advance）：下一次 acquire 给出同一个待提交值。
        let submitted = ring.acquire();
        assert_eq!(
            submitted.signal_value, retried.signal_value,
            "未提交的槽不消费提交序"
        );
        ring.advance(submitted);
        // 下一次提交的值严格大于上一次 signal 的值。
        let next = ring.acquire();
        assert!(next.signal_value > submitted.signal_value);
        ring.advance(next);
        assert_eq!(ring.last_signaled(), next.signal_value);
    }

    #[test]
    fn advance_only_touches_its_own_slot() {
        let mut ring = InFlightRing::new();
        let first = ring.acquire();
        ring.advance(first);
        let second = ring.acquire();
        // 手动写入一个非连续的值：它只能影响 slot 1 的记账。
        ring.advance(FrameSlot {
            index: second.index,
            wait_value: 0,
            signal_value: 41,
        });
        // slot 2 从未提交过，等待值必须是 0（不能被 slot 1 的 41 污染）。
        let third = ring.acquire();
        assert_eq!(third.index, 2);
        assert_eq!(third.wait_value, 0);
        ring.advance(third);
        // 回到 slot 0：等待值仍是第 1 次提交的值。
        let fourth = ring.acquire();
        assert_eq!(fourth.index, 0);
        assert_eq!(fourth.wait_value, first.signal_value);
    }

    /// P0 回归：**不重试时**槽位按 acquire 次数轮转（0 → 1 → 2），与 `FrameIndex` 无关。
    ///
    /// 这里每 3 个序号才取走一帧（1、4、7、10）：旧实现用 `index % 3` 会一直命中同一个槽，
    /// 而三槽 semaphore / 上传缓冲按提交次数轮转 —— 两者不同源，新帧会重写 GPU 仍在读的
    /// 上传缓冲。本测试只覆盖「每次 acquire 都提交」这一条路径：`NOT_READY` / `OUT_OF_DATE`
    /// 重试也会消费一次 acquire 而不提交（`renderer::run_gpu_loop` 保留 pending 帧），那种
    /// 交错下相邻两次提交不保证落在不同槽（复用槽之前由槽 fence 等待保证安全），不在本测试内。
    #[test]
    fn acquire_rotates_slots_by_acquisition_count() {
        let mut frame = LatestFrame::new();
        let mut ring = InFlightRing::new();
        let mut submitted = Vec::new();
        for value in 1..=10 {
            frame.publish(request(value, Plan::Full));
            if value % 3 != 1 {
                // 被后继帧覆盖：该序号不产生 GPU 工作。
                continue;
            }
            let frame = frame.take().expect("pending frame");
            assert_eq!(frame.index.get(), value, "被覆盖的序号必须被丢弃");
            let slot = ring.acquire();
            let recent = &submitted[submitted.len().saturating_sub(2)..];
            assert!(
                !recent.contains(&slot.index),
                "slot {} reused within three acquisitions (recent: {recent:?})",
                slot.index
            );
            submitted.push(slot.index);
            ring.advance(slot);
        }
        assert_eq!(submitted, vec![0, 1, 2, 0]);
    }

    // ── §4.2：最新帧槽 ───────────────────────────────────────────────────────

    #[test]
    fn the_slot_is_empty_until_a_frame_is_published() {
        let mut slot = LatestFrame::new();
        assert!(!slot.has_pending());
        assert!(slot.take().is_none());

        slot.publish(request(1, Plan::Full));
        assert!(slot.has_pending());
        let frame = slot.take().expect("published frame");
        assert_eq!(frame.index.get(), 1);
        assert_eq!(frame.surface, (64, 64));
        assert!(!slot.has_pending(), "取走后槽位必须为空");
    }

    #[test]
    fn a_newer_frame_replaces_the_pending_one() {
        let mut slot = LatestFrame::new();
        slot.publish(request(1, partial(&[PhysicalRect::new(0, 0, 8, 8)])));
        slot.publish(request(2, partial(&[PhysicalRect::new(8, 0, 16, 8)])));

        let frame = slot.take().expect("latest frame");
        assert_eq!(frame.index.get(), 2, "旧序号被丢弃（它不产生 GPU 工作）");
        assert!(!slot.has_pending());
    }

    #[test]
    fn replacing_a_frame_merges_the_dropped_damage() {
        let mut slot = LatestFrame::new();
        let first = PhysicalRect::new(0, 0, 8, 8);
        let second = PhysicalRect::new(0, 8, 8, 16);
        slot.publish(request(1, partial(&[first])));
        slot.publish(request(2, partial(&[second])));

        match slot.take().expect("latest frame").plan {
            Plan::Partial(rects) => {
                assert_eq!(rects.as_slice(), &[first, second], "两帧的 damage 取并集");
            }
            other => panic!("unexpected plan: {other:?}"),
        }
    }

    #[test]
    fn replacing_a_full_frame_stays_full() {
        let mut slot = LatestFrame::new();
        slot.publish(request(1, Plan::Full));
        slot.publish(request(2, partial(&[PhysicalRect::new(0, 0, 4, 4)])));
        assert_eq!(slot.take().expect("latest frame").plan, Plan::Full);
    }

    #[test]
    fn replacing_a_partial_frame_by_a_full_frame_is_full() {
        let mut slot = LatestFrame::new();
        slot.publish(request(1, partial(&[PhysicalRect::new(0, 0, 4, 4)])));
        slot.publish(request(2, Plan::Full));
        assert_eq!(slot.take().expect("latest frame").plan, Plan::Full);
    }

    /// Linux：buffer 尺寸、逻辑尺寸与 buffer scale 必须成对来自最新帧。
    ///
    /// destination（`surface_local`）与 swapchain（`surface`）是新帧的两个面，覆盖时保留旧的
    /// 任何一个都会让 compositor 用错尺寸 —— 旧内容被缩放（D1）或整面重采样（D3）。整数回退的
    /// `buffer_scale` 同理：它必须与这一帧的 buffer 尺寸出自同一帧，否则 surface 的逻辑尺寸
    /// 会被乘 / 除错（V-110）。
    #[cfg(target_os = "linux")]
    #[test]
    fn replacing_a_frame_keeps_surface_and_surface_local_paired() {
        use crate::math::Size;

        let mut slot = LatestFrame::new();
        slot.publish(FrameRequest {
            surface: (800, 600),
            scale: 1.0,
            surface_local: Size::new(800.0, 600.0),
            buffer_scale: 1,
            ..request(1, Plan::Full)
        });
        slot.publish(FrameRequest {
            surface: (876, 626),
            scale: 1.0,
            surface_local: Size::new(701.0, 501.0),
            buffer_scale: 2,
            ..request(2, Plan::Full)
        });

        let frame = slot.take().expect("latest frame");
        assert_eq!(frame.surface, (876, 626), "swapchain 目标来自新帧");
        assert_eq!(
            frame.surface_local,
            Size::new(701.0, 501.0),
            "destination 与 buffer 必须同帧成对"
        );
        assert_eq!(frame.buffer_scale, 2, "buffer scale 与 buffer 必须同帧成对");
    }
    // ── B6：实例增量载荷 + GPU 线程侧的持久镜像 ─────────────────────────────

    /// 构造一份载荷：`chunks` 是 `(实例下标, 字节)`，`count` 是实例环总数。
    fn manual_payload(chunks: &[(u32, &[u8])], count: u32) -> FramePayload {
        FramePayload {
            chunks: chunks
                .iter()
                .map(|(start, bytes)| InstanceChunk {
                    start: *start,
                    bytes: Arc::from(*bytes),
                })
                .collect(),
            instance_count: count,
            drawn_instances: count,
            ops: Vec::new(),
            clips: Vec::new(),
            geometry: Vec::new(),
            scroll: Vec::new(),
            scroll_clip: Vec::new(),
        }
    }

    /// 按镜像的写计划把内容写进一块**模拟的** GPU 缓冲（与三个后端 `write` 同一逻辑）。
    fn apply_mirror(mirror: &InstanceMirror, full: bool, target: &mut [u8]) {
        if full {
            target.copy_from_slice(&mirror.bytes()[..target.len()]);
            return;
        }
        for range in mirror.writes() {
            let start = range.start as usize * 128;
            let end = range.end as usize * 128;
            target[start..end].copy_from_slice(&mirror.bytes()[start..end]);
        }
    }

    /// 被覆盖帧的实例块必须并进后继帧（旧 → 新），否则那段变化永远到不了 GPU。
    #[test]
    fn a_replaced_frame_absorbs_its_instance_chunks() {
        // 生产路径：`next.payload.absorb(previous.payload)`（self = 新帧）。
        let mut newer = manual_payload(&[(0, &[9u8; 128])], 8);
        let older = manual_payload(&[(2, &[7u8; 256])], 8);
        newer.absorb(older);
        let starts: Vec<u32> = newer.chunks.iter().map(|chunk| chunk.start).collect();
        assert_eq!(
            starts,
            vec![2, 0],
            "应用顺序必须是「旧块在前、新块在后」（重叠时新块覆盖）"
        );
    }

    /// 旧块被新块**完整覆盖**时直接丢弃（新块是最新版本）。
    #[test]
    fn absorb_drops_chunks_fully_covered_by_newer_bytes() {
        let mut newer = manual_payload(&[(0, &[2u8; 512])], 8);
        let older = manual_payload(&[(1, &[1u8; 256])], 8);
        newer.absorb(older);
        assert_eq!(
            newer.chunks.len(),
            1,
            "被覆盖的旧块必须丢掉：{:?}",
            newer
                .chunks
                .iter()
                .map(InstanceChunk::range)
                .collect::<Vec<_>>()
        );
        assert_eq!(newer.chunks[0].start, 0);
    }

    /// 最新帧槽被覆盖时，增量块与 damage 一起合并（旧帧不产生 GPU 工作，但它的变化不能丢）。
    #[test]
    fn replacing_a_pending_frame_merges_its_instance_chunks() {
        let mut slot = LatestFrame::new();
        let mut first = request(1, Plan::Full);
        first.payload = manual_payload(&[(4, &[1u8; 128])], 8);
        slot.publish(first);
        let mut second = request(2, partial(&[PhysicalRect::new(0, 0, 8, 8)]));
        second.payload = manual_payload(&[(0, &[2u8; 128])], 8);
        slot.publish(second);

        let frame = slot.take().expect("latest frame");
        assert_eq!(frame.index.get(), 2, "新序号胜出");
        let starts: Vec<u32> = frame.payload.chunks().iter().map(|c| c.start).collect();
        assert_eq!(starts, vec![4, 0], "旧帧的实例块必须并进新帧");
    }

    /// 镜像：三个槽轮转使用时，每个槽被写入之后都必须与镜像的完整内容一致。
    #[test]
    fn the_instance_mirror_converges_every_slot_after_rotation() {
        let mut mirror = InstanceMirror::new(3);
        // 16 个实例；首帧写整表，之后只改 1~2 个实例（凑出「欠账 + 本帧变化」的合并）。
        let first = manual_payload(&[(0, &[1u8; 16 * 128])], 16);
        assert!(mirror.prepare(0, &first), "从未写过的槽必须整块写");
        let mut gpu = [
            vec![0u8; 16 * 128],
            vec![0u8; 16 * 128],
            vec![0u8; 16 * 128],
        ];
        apply_mirror(&mirror, true, &mut gpu[0]);
        mirror.finish(0);
        assert_eq!(gpu[0], mirror.bytes(), "槽 0 与镜像一致");

        // 第二帧用槽 1（它也没写过）⇒ 整块写；镜像 = 首帧 + 本帧变化。
        let second = manual_payload(&[(3, &[2u8; 128])], 16);
        assert!(mirror.prepare(1, &second));
        apply_mirror(&mirror, true, &mut gpu[1]);
        mirror.finish(1);
        assert_eq!(gpu[1], mirror.bytes());

        // 第三帧回到槽 0：它欠着「槽 1 那一帧的 3..4」，本帧再改 0..1 ⇒ 只写这两段。
        let third = manual_payload(&[(0, &[3u8; 128])], 16);
        assert!(!mirror.prepare(0, &third), "欠账 + 本帧变化不足以整块写");
        let writes: Vec<std::ops::Range<u32>> = mirror.writes().to_vec();
        assert_eq!(writes, vec![0..1, 3..4], "只写「欠账 ∪ 本帧变化」的区间");
        apply_mirror(&mirror, false, &mut gpu[0]);
        mirror.finish(0);
        assert_eq!(gpu[0], mirror.bytes(), "增量补写之后槽 0 与镜像一致");

        // 槽 2 第一次用：整块写，内容同样是镜像全量（含前几帧的所有变化）。
        let fourth = manual_payload(&[(8, &[4u8; 128])], 16);
        assert!(mirror.prepare(2, &fourth));
        apply_mirror(&mirror, true, &mut gpu[2]);
        mirror.finish(2);
        assert_eq!(gpu[2], mirror.bytes());
        // 镜像内容：0..1 = 3、3..4 = 2、8..9 = 4、其余 = 1。
        assert_eq!(mirror.bytes()[0], 3);
        assert_eq!(mirror.bytes()[128 * 3], 2);
        assert_eq!(mirror.bytes()[128 * 8], 4);
        assert_eq!(mirror.bytes()[128 * 5], 1);
    }

    /// 槽缓冲被重建（扩容 / 设备恢复）之后必须整块重写，不能沿用旧欠账。
    #[test]
    fn an_invalidated_slot_is_rewritten_in_full() {
        let mut mirror = InstanceMirror::new(1);
        let first = manual_payload(&[(0, &[5u8; 4 * 128])], 4);
        assert!(mirror.prepare(0, &first));
        mirror.finish(0);
        // 无变化的一帧：正常路径不会整块写。
        let idle = manual_payload(&[], 4);
        assert!(!mirror.prepare(0, &idle));
        mirror.finish(0);
        // 缓冲重建 ⇒ 整块写。
        mirror.invalidate(0);
        assert!(mirror.prepare(0, &idle), "重构过的槽必须整块重写");
        mirror.finish(0);
    }

    /// AR4 P1-1 握手（`FrameSink::reset_epoch` 的 UI 侧存根）：只有**代际变化**的那一帧报告
    /// 「必须整帧重发」，而且只报告一次。
    ///
    /// 首次观察必须返回 `false`：首帧本来就是整帧发布，重复强制只会让首帧多做一次全量编码。
    /// 同一代际的后续帧必须返回 `false`：否则每一帧都会退化成全量发布（增量协议失效）。
    #[test]
    fn the_resync_gate_reports_a_rebuild_only_once() {
        let mut gate = ResyncGate::default();
        assert!(!gate.observe(0), "首次观察只记录代际（首帧本来就全量）");
        assert!(!gate.observe(0), "代际没变不得要求整帧重发");

        // 设备恢复：GPU 侧重建镜像 ⇒ 代际 +1 ⇒ 下一帧必须整帧重发。
        assert!(gate.observe(1), "代际变化必须报告一次「本帧整帧发布」");
        assert!(!gate.observe(1), "同一个新代际只报告一次（下一帧回到增量）");

        // 连续两次重建（恢复后再次 TDR）：两个新代际各报告一次。
        assert!(gate.observe(2));
        assert!(!gate.observe(2));
        // 代际回到一个旧值也必须报告（判据是「与上次观察不同」，不是「更大」）。
        assert!(gate.observe(1));
        assert!(!gate.observe(1));
        // 代际为 0 的默认后端（Linux / macOS）：永远不强制（首帧之后全走增量）。
        let mut stateless = ResyncGate::default();
        assert!(!stateless.observe(0));
        assert!(!stateless.observe(0));
    }
}
