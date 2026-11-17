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

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};

use ahash::RandomState;
use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_metal::{
    MTLDevice, MTLPixelFormat, MTLRenderPipelineState, MTLSamplerAddressMode, MTLSamplerDescriptor,
    MTLSamplerMinMagFilter, MTLSamplerMipFilter, MTLSamplerState,
};

use crate::{
    error::EngineError,
    render::macos::{
        device::{D8_REQUIRED, GpuCaps, init_error},
        pipeline::{BlendMode, PipelineKey, PipelineSet, ShaderSet, build_pipeline},
    },
};

/// 进程级单例（首次成功初始化之后常驻；失败不缓存，下一个调用点会重试并再次拿到错误）。
static CORE: OnceLock<Arc<GpuCore>> = OnceLock::new();

/// 设备级 GPU 对象（进程唯一）。
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
    /// 取进程单例（首次调用做设备创建 + D8 基线校验，之后是纯读）。
    ///
    /// 并发首次调用最坏情况是"两份被创建、只有一份留下"：两份来自同一个
    /// `MTLCreateSystemDefaultDevice`，语义等价；真实调用点（主线程建窗 / 首个离屏渲染器）
    /// 都是串行的。
    pub(crate) fn shared() -> Result<&'static Arc<Self>, EngineError> {
        if let Some(core) = CORE.get() {
            return Ok(core);
        }
        let core = Arc::new(Self::create()?);
        let _ = CORE.set(core);
        Ok(CORE.get().expect("the singleton was initialized above"))
    }

    /// 设备创建 + caps 采集 + 基线校验（只有 `shared()` 会调用）。
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
    /// 超预算时**明确失败**（`AppInit` + 具体字节数），而不是让驱动在显存压力下失败或换页。
    fn reserve_persist(&self, bytes: u64) -> Result<(), EngineError> {
        let mut used = self.persist_bytes.load(Ordering::Acquire);
        loop {
            if !budget_allows(used, bytes) {
                return Err(EngineError::AppInit {
                    detail: smol_str::format_smolstr!(
                        "persist budget exceeded: {} MiB in use + {} MiB requested > {} MiB",
                        used / (1024 * 1024),
                        bytes / (1024 * 1024),
                        MAX_PERSIST_BUDGET / (1024 * 1024)
                    ),
                });
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

/// 一次 persist 显存额度的 RAII 占用（`GpuRenderer` 持有，drop 时归还）。
///
/// resize 时先申请新额度、成功后才释放旧额度：申请失败（超预算）时窗口保留原目标继续渲染，
/// 而不是进入"额度已释放、纹理没建"的不一致状态。
pub(crate) struct PersistLease {
    core: Arc<GpuCore>,
    bytes: u64,
}

impl PersistLease {
    /// 申请初始额度。
    pub(crate) fn new(core: Arc<GpuCore>, size: (u32, u32)) -> Result<Self, EngineError> {
        let bytes = persist_bytes(size);
        core.reserve_persist(bytes)?;
        Ok(Self { core, bytes })
    }

    /// 改成新尺寸的额度（窗口 resize / DPI 变化）。失败时保留原额度。
    pub(crate) fn resize(&mut self, size: (u32, u32)) -> Result<(), EngineError> {
        let bytes = persist_bytes(size);
        if bytes == self.bytes {
            return Ok(());
        }
        self.core.reserve_persist(bytes)?;
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
    use std::sync::PoisonError;

    use objc2_metal::{MTLDevice, MTLPixelFormat};

    use super::GpuCore;
    use crate::render::macos::pipeline::BlendMode;

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

    /// R09-5：第二个"窗口"（第二个渲染器）必须拿到同一个 `GpuCore`，而不是重建设备。
    #[test]
    fn the_core_is_a_process_singleton() {
        let first = GpuCore::shared().expect("metal device");
        let second = GpuCore::shared().expect("metal device");
        assert!(
            std::ptr::eq(first, second),
            "GpuCore 必须是进程单例（OnceLock）：否则每个窗口又各建一份 device"
        );
        assert!(!first.device().name().to_string().is_empty());
        assert!(
            std::ptr::eq(first.sampler(), second.sampler()),
            "采样器也只有一份"
        );
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
        let cached = cached_pipelines(core);
        assert_eq!(cached, 4, "一套 PSO 是 4 条（quad / ui / image / glyph）");
        assert!(first.distinct_states(), "4 条 PSO 不能是同一个对象");
        let second = core
            .pipeline_set(format, BlendMode::Premultiplied)
            .expect("pipeline set");
        assert_eq!(cached_pipelines(core), cached, "第二次请求不能新增缓存条目");
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
