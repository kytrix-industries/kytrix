//! 进程级 GPU 单例：`MTLDevice` / 两个 `MTLLibrary` / PSO 缓存 / 采样器（R09-5 / R09-13）。
//!
//! **为什么是单例**：`MTLDevice`、`MTLLibrary`、`MTLRenderPipelineState`、`MTLSamplerState`
//! 都是**设备级**对象，同一台设备上没有理由每个窗口一份。旧实现每个窗口重跑
//! `MTLCreateSystemDefaultDevice` + 两次 metallib 解析 + 4 条同步 PSO 编译（首次编译要做后端
//! 机器码生成，数十毫秒），全部落在**主线程**的建窗路径上：表现为"启动白屏 / Cmd+N 全应用卡
//! 一下"，并且 N 个窗口放大成 N 份重复对象（R09-5）。
//!
//! 现在：
//! - 设备 + caps 校验：进程一次（[`GpuCore::shared`]）；
//! - metallib：首次需要 PSO 时加载一次（[`GpuCore::pipeline_set`]，发生在 `kytrix-metal`
//!   线程的首帧录制里，**不占主线程**）；
//! - PSO：按 [`PipelineKey`] 懒建 + 缓存，同一键全进程只编译一次。
//!
//! 严格 per-window 的只剩：drawable、persist、上传槽（背压需要）、display link、GPU 线程。
//!
//! 图集页仍是 per-window：共享一张图集纹理会引入跨窗口的 command buffer 重叠写/读竞争
//! （同一 `MTLCommandQueue` 只保证**开始**顺序，不保证执行隔离），需要 `MTLEvent` 或共享
//! 渲染线程才能安全落地 —— 属于 wave-5 的共享渲染线程，不在本次改动范围（详见报告）。
//!
//! # 设备恢复（C4）：注册表可替换
//!
//! 旧实现是 `OnceLock<Arc<GpuCore>>`：设备一旦初始化就永不替换，`DeviceLost` 只能让整个应用
//! 退出（audit §4）。现在进程级状态是 [`Registry`]（`Mutex` 保护）：`shared()` 返回**当前**
//! 实例，[`GpuCore::recover`] 在设备丢失后把注册表换成新建的 `GpuCore`（新 `MTLDevice` + 采样器
//! + 空 PSO 缓存），generation 递增。
//!
//! 恢复粒度是**整 renderer 重建**（对齐 Chrome 的粗粒度重建：Metal 后端在 GPU 进程中，
//! 设备丢失 = 进程级 GPU 状态重建）：设备级对象进新 core，窗口级对象（persist / 图集页 /
//! 上传槽 / display link / GPU 线程）由 `renderer::Renderer::new_with_layer` 在新 core 上重建，
//! 旧 `CAMetalLayer` 复用（C6 已让图层显式持有几何）。
//!
//! 两条恢复纪律：
//! - **限次**：[`RecoveryBudget`] 在 5 s 内最多允许 3 次设备重建；超限返回粘性
//!   `EngineError::DeviceLost`（应用退出），避免驱动持续失效时每帧重建的活锁。
//! - **多窗口只删建一次**：`recover(stale)` 看到注册表里的当前实例不是调用方的 `stale`（说明
//!   另一个窗口已经重建过）时直接复用，不再 `MTLCreateSystemDefaultDevice`（多窗口同时收到
//!   `DeviceLost` 时系统限流/反复删建设备是真实风险，audit §4.5）。
//!
//! 代价（登记为残余风险，不触发性能门禁）：恢复发生在 UI 泵内，**主线程阻塞上限 250 ms**
//! （旧 GPU 线程的 `join_with_deadline`）；可见空窗约 **1–3 帧**（旧帧保留 + 新 GPU 线程首帧
//! PSO 编译数十毫秒）。恢复成功后 `Window::force_full_republish()` 让 UI 整帧重发。

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use ahash::RandomState;
use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_metal::{
    MTLDevice, MTLPixelFormat, MTLRenderPipelineState, MTLSamplerAddressMode, MTLSamplerDescriptor,
    MTLSamplerMinMagFilter, MTLSamplerMipFilter, MTLSamplerState,
};
use smol_str::SmolStr;

use crate::{
    error::EngineError,
    render::macos::{
        device::{D8_REQUIRED, GpuCaps, init_error},
        pipeline::{BlendMode, PipelineKey, PipelineSet, ShaderSet, build_pipeline},
    },
};

/// 设备恢复的时间窗（与 Windows `render/windows/device.rs` 的 `RECOVERY_WINDOW` 同口径）。
const RECOVERY_WINDOW: Duration = Duration::from_secs(5);

/// 一个时间窗内最多重建几次 `MTLDevice`（第 4 次请求 ⇒ 粘性 `DeviceLost`）。
const MAX_DEVICE_RECOVERIES: usize = 3;

/// 进程级设备注册表（见模块文档「设备恢复」）。
static REGISTRY: Mutex<Registry> = Mutex::new(Registry::new());

/// 设备注册表：当前实例 + 安装代际 + 重建限次。
///
/// 「当前实例」可替换是 C4 的核心改动：旧实现用 `OnceLock`，设备丢失后进程里没有任何合法
/// 路径拿到新设备，只能退出应用。
struct Registry {
    core: Option<Arc<GpuCore>>,
    /// 成功安装过几次 `GpuCore`（首次创建 = 1，每次恢复 +1）。诊断/测试用它数重建次数。
    generation: u64,
    budget: RecoveryBudget,
}

/// 设备重建的限次窗口（进程级）。
///
/// 只在**真正要删建 `MTLDevice`** 时扣一次；「复用其它窗口已重建的实例」不扣 —— 那条路径不
/// 触碰设备，没有限流风险。
struct RecoveryBudget {
    /// 时间窗内的尝试时刻（`None` = 空槽；满槽 = 超限）。
    attempts: [Option<Instant>; MAX_DEVICE_RECOVERIES],
}

impl RecoveryBudget {
    const fn new() -> Self {
        Self {
            attempts: [None; MAX_DEVICE_RECOVERIES],
        }
    }

    /// 过窗的尝试作废（窗口是滑动的：`5 s` 内的尝试才计数）。
    fn expire(&mut self, now: Instant) {
        for attempt in &mut self.attempts {
            if attempt.is_some_and(|at| now.saturating_duration_since(at) >= RECOVERY_WINDOW) {
                *attempt = None;
            }
        }
    }

    /// 申请一次设备重建额度。
    ///
    /// 失败以**粘性** `DeviceLost` 表达（与「设备真的没了」同一公开语义：调用方据此退出），
    /// `reason` 明确写出「恢复超限 + 下一次可重试的剩余时间」。
    fn charge(&mut self, now: Instant) -> Result<(), EngineError> {
        self.expire(now);
        if let Some(slot) = self.attempts.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(now);
            return Ok(());
        }
        let oldest = self.attempts.iter().flatten().min().copied().unwrap_or(now);
        let retry_in = RECOVERY_WINDOW.saturating_sub(now.saturating_duration_since(oldest));
        let reason = smol_str::format_smolstr!(
            "device recovery limit reached ({MAX_DEVICE_RECOVERIES} attempts within \
             {RECOVERY_WINDOW:?}); retry in {retry_in:?}"
        );
        Err(device_lost(reason))
    }
}

/// 构造 `EngineError::DeviceLost`：恢复相关失败也必须带已落盘的诊断文件（公开错误契约）。
fn device_lost(reason: SmolStr) -> EngineError {
    let log_path = super::device::write_diagnostics(&reason, 0);
    EngineError::DeviceLost { reason, log_path }
}

impl Registry {
    const fn new() -> Self {
        Self {
            core: None,
            generation: 0,
            budget: RecoveryBudget::new(),
        }
    }

    /// 首次创建（之后是纯读）：失败不缓存，下一个调用点会重试并再次拿到错误。
    fn shared(&mut self) -> Result<Arc<GpuCore>, EngineError> {
        if let Some(core) = &self.core {
            return Ok(Arc::clone(core));
        }
        self.install()
    }

    /// 设备丢失后的整设备重建（多窗口只删建一次，见模块文档）。
    ///
    /// `stale` 是调用方窗口正在用的实例：
    /// - 注册表当前实例**不是** `stale` ⇒ 另一个窗口已经重建过，直接返回当前实例（复用）；
    /// - 否则扣一次限次额度后新建 `GpuCore`（新 `MTLDevice` + caps 校验 + 采样器 + 空 PSO
    ///   缓存），generation +1。
    fn recover(&mut self, stale: &Arc<GpuCore>, now: Instant) -> Result<Arc<GpuCore>, EngineError> {
        if let Some(current) = &self.core
            && !Arc::ptr_eq(current, stale)
        {
            return Ok(Arc::clone(current));
        }
        self.budget.charge(now)?;
        self.install()
    }

    /// 新建一个 `GpuCore` 并把它装进注册表（`shared` / `recover` 的唯一安装点）。
    fn install(&mut self) -> Result<Arc<GpuCore>, EngineError> {
        let core = Arc::new(GpuCore::create()?);
        self.core = Some(Arc::clone(&core));
        self.generation += 1;
        Ok(core)
    }
}

/// 取注册表锁：锁只在整个创建/替换区间持有（恢复本来就是主线程串行的稀有事件）。
fn registry() -> MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 设备级 GPU 对象（进程唯一，可被 [`GpuCore::recover`] 替换）。
///
/// 线程归属：`kytrix-metal` 线程与 UI / 主线程都可以取用 —— Metal 的 device / library / PSO /
/// sampler 按 Apple 文档都是线程安全的，本结构里唯一可变的状态（PSO 缓存、shader 懒加载）
/// 由 `Mutex` / `OnceLock` 串行化。
pub(crate) struct GpuCore {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    /// 首次需要 PSO 时才解析 metallib（[`ShaderSet::load`]）：`Device::new()` 与 pure
    /// 离屏初始化不应该为此付代价，也不需要 `OUT_DIR` 里有真实 shader 产物。
    shaders: OnceLock<ShaderSet>,
    sampler: Retained<ProtocolObject<dyn MTLSamplerState>>,
    pipelines: Mutex<PipelineCache>,
    /// 各窗口 persist 纹理占用的字节数之和（[`MAX_PERSIST_BUDGET`] 是它的上限）。
    persist_bytes: AtomicU64,
}

// SAFETY: `MTLDevice` / `MTLSamplerState` / `MTLRenderPipelineState` / `MTLLibrary` 按 Apple
// 文档都是线程安全的；本类型的可变状态（`shaders` 的懒加载、`pipelines`）分别由
// `OnceLock` 与 `Mutex` 保护，构造后其余字段只读。
unsafe impl Send for GpuCore {}
unsafe impl Sync for GpuCore {}

impl GpuCore {
    /// 取**当前**设备实例（首次调用做设备创建 + D8 基线校验，之后是纯读注册表）。
    ///
    /// 返回 `Arc` 而不是 `&'static`：注册表可替换（C4），任何持有旧实例的窗口在旧设备上继续
    /// 工作是未定义行为；`DeviceLost` 之后活动窗口都必须经 `recover` 拿到新实例。
    pub(crate) fn shared() -> Result<Arc<Self>, EngineError> {
        registry().shared()
    }

    /// 设备丢失后的整设备重建；见 [`Registry::recover`] 的复用/限次规则。
    ///
    /// 返回的实例可能是新建的，也可能是**其它窗口已重建**的当前实例（多窗口只删建一次设备）。
    /// 旧 Arc 的持有者（例如尚未被重建的其它窗口）仍可安全读取旧对象，但不得再提交 GPU 工作。
    pub(crate) fn recover(stale: &Arc<Self>) -> Result<Arc<Self>, EngineError> {
        registry().recover(stale, Instant::now())
    }

    /// 进程成功安装过几个 `GpuCore`（首次创建 = 1，每次设备重建 +1）。
    pub(crate) fn generation() -> u64 {
        registry().generation
    }

    /// 设备创建 + caps 采集 + 基线校验（只有 [`Registry::install`] 会调用）。
    fn create() -> Result<Self, EngineError> {
        let device =
            objc2_metal::MTLCreateSystemDefaultDevice().ok_or(EngineError::UnsupportedGpu {
                os: "macos",
                required: D8_REQUIRED,
                actual: smol_str::SmolStr::new_static("no Metal device"),
            })?;
        let caps = GpuCaps::collect(&device);
        caps.check()?;
        // 诊断行只在启动失败时有消费者；这里断言它可格式化（`AppInit` 的 detail 会带上）。
        debug_assert!(!caps.describe().is_empty());
        let sampler = create_sampler(&device)?;
        Ok(Self {
            device,
            shaders: OnceLock::new(),
            sampler,
            pipelines: Mutex::new(PipelineCache::new()),
            persist_bytes: AtomicU64::new(0),
        })
    }

    /// 共享的 `MTLDevice`（只读借用：PSO / metallib 创建路径用）。
    pub(crate) fn device(&self) -> &ProtocolObject<dyn MTLDevice> {
        &self.device
    }

    /// 共享 `MTLDevice` 的一份强引用（窗口的 [`super::device::Device`] 句柄表以它为底层）。
    pub(crate) fn retain_device(&self) -> Retained<ProtocolObject<dyn MTLDevice>> {
        self.device.clone()
    }

    /// 采样器（`Linear/Linear/Clamp`、无 mip、`lodMaxClamp = 0`）：与 D3D12
    /// `MIN_MAG_MIP_LINEAR + MaxLOD 0`、Vulkan `LINEAR/LINEAR` 逐项等价。
    pub(crate) fn sampler(&self) -> &ProtocolObject<dyn MTLSamplerState> {
        &self.sampler
    }

    /// 取（必要时编译）本进程的一套 PSO：**每个键只编译一次**，之后全是缓存命中。
    ///
    /// 首次调用会解析两个 metallib 并同步编译 4 条 PSO（数十毫秒）—— 调用点是窗口 GPU 线程的
    /// 首帧录制，因此这段代价不再落在主线程（R09-5）。
    pub(crate) fn pipeline_set(
        &self,
        format: MTLPixelFormat,
        blend: BlendMode,
    ) -> Result<PipelineSet, EngineError> {
        let keys = PipelineSet::keys(format, blend);
        let mut cache = self
            .pipelines
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for key in keys {
            if cache.entries.contains_key(&key) {
                continue;
            }
            let state = self.build(key)?;
            cache.entries.insert(key, state);
        }
        let resolved = keys.map(|key| cache.entries[&key].clone());
        Ok(PipelineSet::from_resolved(resolved))
    }

    /// 编译一条 PSO：`PipelineCache` 已在锁内确认它缺失，这里才会走到 metallib 解析。
    fn build(
        &self,
        key: PipelineKey,
    ) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, EngineError> {
        let shaders = self.shaders()?;
        build_pipeline(self.device(), shaders, key)
    }

    /// 懒加载 metallib（进程一次）。
    fn shaders(&self) -> Result<&ShaderSet, EngineError> {
        if let Some(shaders) = self.shaders.get() {
            return Ok(shaders);
        }
        let loaded = ShaderSet::load(self.device())?;
        let _ = self.shaders.set(loaded);
        Ok(self.shaders.get().expect("the shader set was set above"))
    }

    /// 为一张 persist 纹理申请显存额度（R09-13 的"多窗口资源上限"）。
    ///
    /// 超预算时**明确失败**（`detail` 带「已用 / 请求 / 上限」三个数字），而不是让驱动在显存
    /// 压力下失败或换页。公开错误的分类由调用方的阶段决定（见 [`PersistPhase`]）。
    fn reserve_persist(&self, bytes: u64) -> Result<(), SmolStr> {
        let mut used = self.persist_bytes.load(Ordering::Acquire);
        loop {
            if !budget_allows(used, bytes) {
                return Err(smol_str::format_smolstr!(
                    "persist budget exceeded: {} MiB in use + {} MiB requested > {} MiB",
                    used / (1024 * 1024),
                    bytes / (1024 * 1024),
                    MAX_PERSIST_BUDGET / (1024 * 1024)
                ));
            }
            match self.persist_bytes.compare_exchange(
                used,
                used.saturating_add(bytes),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(current) => used = current,
            }
        }
    }

    fn release_persist(&self, bytes: u64) {
        self.persist_bytes.fetch_sub(bytes, Ordering::AcqRel);
    }
}

/// 进程级 persist 显存预算（R09-13）。
///
/// persist 严格 per-window（内容是这张窗口的合成结果，不能跨窗口共享），所以"多窗口资源上限"
/// 只能靠**总量预算**表达：每个窗口按自己的物理尺寸创建（Retina 5K 全屏 5120×2880×4 B ≈ 59 MB），
/// 窗口越多总量越大。512 MiB 允许 8 个 5K 窗口同时存在，超出就是明确的配置/尺寸异常，此时失败
/// 比让驱动在显存压力下失败（或换页抖动）更容易诊断。
pub(crate) const MAX_PERSIST_BUDGET: u64 = 512 * 1024 * 1024;

/// 一张 persist 纹理的字节数（`BGRA8Unorm_sRGB` = 4 B/像素）。
pub(crate) fn persist_bytes(size: (u32, u32)) -> u64 {
    u64::from(size.0.max(1)) * u64::from(size.1.max(1)) * 4
}

/// `used + requested` 是否仍在预算内（纯函数：便于单测边界，不碰设备）。
fn budget_allows(used: u64, requested: u64) -> bool {
    used.saturating_add(requested) <= MAX_PERSIST_BUDGET
}

/// 一次 persist 额度申请所处的阶段：决定超预算时归到哪个公开错误。
///
/// 与 `EngineError::Composition` 同一条纪律：`AppInit` **只给「窗口还建不出来」**的阶段；
/// 窗口已经存在、只是渲染目标重建失败，归 `Swapchain`（调用方据错误类型决定提示/上报策略）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PersistPhase {
    /// 建窗 / 建渲染器（`PersistLease::new`）：失败时窗口根本建不出来。
    Create,
    /// 窗口已存在的目标重建（`PersistLease::resize`）：GPU 线程据此停止出帧。
    Rebuild,
}

/// persist 显存额度不足的公开错误（`detail` 带「已用 / 请求 / 上限」三个数字）。
fn budget_error(detail: SmolStr, phase: PersistPhase) -> EngineError {
    match phase {
        PersistPhase::Create => EngineError::AppInit { detail },
        PersistPhase::Rebuild => EngineError::Swapchain { detail },
    }
}

/// 一次 persist 显存额度的 RAII 占用（`GpuRenderer` 持有，drop 时归还）。
///
/// resize 时先申请新额度、成功后才释放旧额度：申请失败时**旧额度与旧 persist 都保持原样**，
/// 不会进入「额度已释放、纹理没建」的不一致状态。
///
/// **失败的实际行为是「停」而不是「降级」**（AR3 §6.5 / P2-13）：`resize` 的 `Err` 冒泡到
/// GPU 循环后由 `publish_error` 变成粘性错误、GPU 线程退出、该窗口停止出帧。旧注释/报告说的
/// 「保留旧目标继续渲染」与代码相反，已按实现改正：
/// - 512 MiB 预算被打满属于「配置 / 尺寸异常」，静默按旧尺寸继续画会把真实问题藏起来；
/// - 新 drawable 里 persist 覆盖不到的区域**没有 clear 路径**，继续 present 会露出未初始化像素。
pub(crate) struct PersistLease {
    core: Arc<GpuCore>,
    bytes: u64,
}

impl PersistLease {
    /// 申请初始额度。
    pub(crate) fn new(core: Arc<GpuCore>, size: (u32, u32)) -> Result<Self, EngineError> {
        let bytes = persist_bytes(size);
        core.reserve_persist(bytes)
            .map_err(|detail| budget_error(detail, PersistPhase::Create))?;
        Ok(Self { core, bytes })
    }

    /// 改成新尺寸的额度（窗口 resize / DPI 变化）。失败时保留原额度（但调用方必须停止出帧，
    /// 见类型文档）。
    pub(crate) fn resize(&mut self, size: (u32, u32)) -> Result<(), EngineError> {
        let bytes = persist_bytes(size);
        if bytes == self.bytes {
            return Ok(());
        }
        self.core
            .reserve_persist(bytes)
            .map_err(|detail| budget_error(detail, PersistPhase::Rebuild))?;
        self.core.release_persist(self.bytes);
        self.bytes = bytes;
        Ok(())
    }
}

impl Drop for PersistLease {
    fn drop(&mut self) {
        self.core.release_persist(self.bytes);
    }
}

/// PSO 缓存（同一个键全进程只编译一次）。
struct PipelineCache {
    /// 4 条键起步：quad / geometry-ui / geometry-image / geometry-glyph。
    entries:
        HashMap<PipelineKey, Retained<ProtocolObject<dyn MTLRenderPipelineState>>, RandomState>,
}

impl PipelineCache {
    fn new() -> Self {
        Self {
            entries: HashMap::with_capacity_and_hasher(4, RandomState::new()),
        }
    }
}

/// 采样器（与 D3D12/Vulkan 对齐的唯一来源）。
fn create_sampler(
    device: &ProtocolObject<dyn MTLDevice>,
) -> Result<Retained<ProtocolObject<dyn MTLSamplerState>>, EngineError> {
    let descriptor = MTLSamplerDescriptor::new();
    descriptor.setMinFilter(MTLSamplerMinMagFilter::Linear);
    descriptor.setMagFilter(MTLSamplerMinMagFilter::Linear);
    descriptor.setSAddressMode(MTLSamplerAddressMode::ClampToEdge);
    descriptor.setTAddressMode(MTLSamplerAddressMode::ClampToEdge);
    descriptor.setRAddressMode(MTLSamplerAddressMode::ClampToEdge);
    descriptor.setMipFilter(MTLSamplerMipFilter::NotMipmapped);
    descriptor.setLodMaxClamp(0.0);
    device
        .newSamplerStateWithDescriptor(&descriptor)
        .ok_or_else(|| init_error("newSamplerState", "returned nil"))
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, PoisonError},
        time::{Duration, Instant},
    };

    use objc2_metal::{MTLDevice, MTLPixelFormat};

    use super::{GpuCore, RECOVERY_WINDOW, RecoveryBudget, Registry};
    use crate::{error::EngineError, render::macos::pipeline::BlendMode};

    /// 缓存条目数（同模块内可直接读私有字段）。
    fn cached_pipelines(core: &GpuCore) -> usize {
        core.pipelines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .len()
    }

    /// persist 的字节数口径（`BGRA8Unorm_sRGB`）：预算是按它累计的。
    #[test]
    fn persist_bytes_counts_bgra_pixels() {
        assert_eq!(super::persist_bytes((1, 1)), 4);
        assert_eq!(super::persist_bytes((2800, 1800)), 20_160_000);
        // 0 尺寸被夹到 1（与 `create_target` 的 `max(1)` 一致）。
        assert_eq!(super::persist_bytes((0, 0)), 4);
    }

    /// 预算边界：等于上限允许，多 1 字节就拒绝。
    #[test]
    fn the_persist_budget_is_inclusive_and_saturating() {
        let max = super::MAX_PERSIST_BUDGET;
        assert!(super::budget_allows(0, max));
        assert!(super::budget_allows(max, 0));
        assert!(super::budget_allows(max / 2, max / 2));
        assert!(!super::budget_allows(max, 1));
        assert!(!super::budget_allows(max - 1, 2));
        assert!(!super::budget_allows(u64::MAX, u64::MAX), "不能溢出成 0");
    }

    /// AR3 §6.5 / P2-13：超预算的错误分类必须按**阶段**区分（`AppInit` 只给「窗口还建不出来」），
    /// 并如实表达「重建失败」——而不是旧注释承诺的「保留旧目标继续渲染」。
    #[test]
    fn persist_budget_failures_are_classified_by_phase() {
        use smol_str::SmolStr;

        use super::{PersistPhase, budget_error};
        use crate::error::EngineError;

        let detail = SmolStr::new_static("persist budget exceeded: 1 MiB");
        assert!(matches!(
            budget_error(detail.clone(), PersistPhase::Create),
            EngineError::AppInit { .. }
        ));
        let rebuild = budget_error(detail, PersistPhase::Rebuild);
        assert!(
            matches!(rebuild, EngineError::Swapchain { .. }),
            "窗口已存在时的目标重建失败必须归 Swapchain（不是 AppInit）：{rebuild:?}"
        );
        // 错误文案带具体数字，调用方不需要再拼诊断。
        assert!(rebuild.to_string().contains("persist budget exceeded"));
    }

    /// R09-5：第二个"窗口"（第二个渲染器）必须拿到同一个 `GpuCore`，而不是重建设备。
    #[test]
    fn the_core_is_a_process_singleton_until_a_device_loss_replaces_it() {
        let first = GpuCore::shared().expect("metal device");
        let second = GpuCore::shared().expect("metal device");
        assert!(
            Arc::ptr_eq(&first, &second),
            "稳态下 GpuCore 必须是进程单例：否则每个窗口又各建一份 device"
        );
        assert!(!first.device().name().to_string().is_empty());
        assert!(
            std::ptr::eq(first.sampler(), second.sampler()),
            "采样器也只有一份"
        );
    }

    /// **C4 核心不变量**：`recover` 之后注册表换成新实例（generation +1），旧 Arc 仍有效；
    /// 另一个窗口（还持有旧实例）来恢复时**复用**已重建的实例，不再删建 `MTLDevice`。
    #[test]
    fn a_lost_device_is_replaced_once_and_then_reused_by_the_other_windows() {
        let mut registry = Registry::new();
        let first = registry.shared().expect("metal device");
        assert_eq!(registry.generation, 1, "首次创建算第一次安装");
        assert!(Arc::ptr_eq(
            &first,
            &registry.shared().expect("metal device")
        ));

        let now = Instant::now();
        let recovered = registry.recover(&first, now).expect("rebuild device");
        assert!(
            !Arc::ptr_eq(&first, &recovered),
            "恢复必须换成新实例（新 MTLDevice + 空 PSO 缓存）"
        );
        assert_eq!(registry.generation, 2, "一次恢复 = generation +1");
        assert!(
            !first.device().name().to_string().is_empty(),
            "旧 Arc 仍被本窗口/旧 lease 持有，读取必须仍然安全"
        );
        assert!(
            Arc::ptr_eq(&recovered, &registry.shared().expect("metal device")),
            "shared() 必须返回恢复后的当前实例"
        );

        // 第二个窗口晚一拍才发现设备丢失（它手里的 stale 仍是 first）：复用，不重建。
        let reused = registry.recover(&first, now).expect("reuse rebuilt device");
        assert!(
            Arc::ptr_eq(&recovered, &reused),
            "其它窗口已重建 ⇒ 直接复用，避免多窗口重复删建 MTLDevice"
        );
        assert_eq!(registry.generation, 2, "复用不产生新的设备");
    }

    /// 恢复限次：5 s 内最多 3 次（第 4 次拒绝），过窗后额度复位。
    #[test]
    fn the_recovery_budget_allows_three_attempts_per_five_seconds() {
        let mut budget = RecoveryBudget::new();
        let now = Instant::now();
        for attempt in 0..3u32 {
            budget
                .charge(now + Duration::from_millis(u64::from(attempt)))
                .unwrap_or_else(|error| panic!("attempt {attempt} must be allowed: {error:?}"));
        }
        let exhausted = budget
            .charge(now + Duration::from_millis(3))
            .expect_err("第 4 次恢复必须被拒绝");
        match exhausted {
            EngineError::DeviceLost { reason, log_path } => {
                assert!(
                    reason.contains("recovery limit reached"),
                    "超限理由必须可诊断：{reason}"
                );
                assert!(
                    log_path.exists(),
                    "DeviceLost 契约要求诊断文件已落盘：{log_path:?}"
                );
            }
            other => panic!("超限必须是粘性 DeviceLost：{other:?}"),
        }
        // 过窗复位：最早一次尝试失效之后又能重建（滑动窗口，不是永久熔断）。
        budget
            .charge(now + RECOVERY_WINDOW)
            .expect("过窗后额度必须复位");
    }

    /// 限次耗尽后的 `recover` 必须把超限作为 `DeviceLost` 报给调用方（泵据此走既有退出路径）。
    #[test]
    fn an_exhausted_registry_reports_device_lost_instead_of_rebuilding() {
        let mut registry = Registry::new();
        let now = Instant::now();
        let mut stale = registry.shared().expect("metal device");
        for _ in 0..3 {
            stale = registry
                .recover(&stale, now)
                .expect("attempt within budget");
        }
        let error = match registry.recover(&stale, now) {
            Ok(_) => panic!("第 4 次设备重建必须被拒绝"),
            Err(error) => error,
        };
        assert!(
            matches!(error, EngineError::DeviceLost { .. }),
            "超限必须上报粘性 DeviceLost：{error:?}"
        );
        assert_eq!(registry.generation, 4, "1 次创建 + 3 次重建");
    }

    /// PSO 懒建 + 缓存：同一键第二次请求必须命中缓存（同一对象地址），
    /// 且 4 条键各自独立（不能把不同布局折叠成同一个 PSO）。
    #[test]
    fn the_pipeline_cache_builds_each_key_once() {
        use crate::paint::{atlas::AtlasKind, geometry::GeometryPipeline};

        let core = GpuCore::shared().expect("metal device");
        let format = MTLPixelFormat::BGRA8Unorm_sRGB;
        let first = core
            .pipeline_set(format, BlendMode::Premultiplied)
            .expect("pipeline set");
        let cached = cached_pipelines(&core);
        assert_eq!(cached, 4, "一套 PSO 是 4 条（quad / ui / image / glyph）");
        assert!(first.distinct_states(), "4 条 PSO 不能是同一个对象");
        let second = core
            .pipeline_set(format, BlendMode::Premultiplied)
            .expect("pipeline set");
        assert_eq!(
            cached_pipelines(&core),
            cached,
            "第二次请求不能新增缓存条目"
        );
        // 同一键返回同一个对象（地址相等）。`&ProtocolObject` 的地址即 ObjC 对象地址。
        assert!(std::ptr::eq(first.quad(), second.quad()), "quad 命中缓存");
        for pipeline in [
            GeometryPipeline::Ui,
            GeometryPipeline::Textured {
                atlas: AtlasKind::Image,
            },
            GeometryPipeline::Textured {
                atlas: AtlasKind::Glyph,
            },
        ] {
            assert!(
                std::ptr::eq(first.geometry(pipeline), second.geometry(pipeline)),
                "{pipeline:?} 命中缓存"
            );
        }
    }
}
