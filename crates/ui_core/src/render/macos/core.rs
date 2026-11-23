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

const RECOVERY_WINDOW: Duration = Duration::from_secs(5);

const MAX_DEVICE_RECOVERIES: usize = 3;

static REGISTRY: Mutex<Registry> = Mutex::new(Registry::new());

struct Registry {
    core: Option<Arc<GpuCore>>,
    generation: u64,
    budget: RecoveryBudget,
}

struct RecoveryBudget {
    attempts: [Option<Instant>; MAX_DEVICE_RECOVERIES],
}

impl RecoveryBudget {
    const fn new() -> Self {
        Self {
            attempts: [None; MAX_DEVICE_RECOVERIES],
        }
    }

    fn expire(&mut self, now: Instant) {
        for attempt in &mut self.attempts {
            if attempt.is_some_and(|at| now.saturating_duration_since(at) >= RECOVERY_WINDOW) {
                *attempt = None;
            }
        }
    }

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

    fn shared(&mut self) -> Result<Arc<GpuCore>, EngineError> {
        if let Some(core) = &self.core {
            return Ok(Arc::clone(core));
        }
        self.install()
    }

    fn recover(&mut self, stale: &Arc<GpuCore>, now: Instant) -> Result<Arc<GpuCore>, EngineError> {
        if let Some(current) = &self.core
            && !Arc::ptr_eq(current, stale)
        {
            return Ok(Arc::clone(current));
        }
        self.budget.charge(now)?;
        self.install()
    }

    fn install(&mut self) -> Result<Arc<GpuCore>, EngineError> {
        let core = Arc::new(GpuCore::create()?);
        self.core = Some(Arc::clone(&core));
        self.generation += 1;
        Ok(core)
    }
}

fn registry() -> MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) struct GpuCore {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    shaders: OnceLock<ShaderSet>,
    sampler: Retained<ProtocolObject<dyn MTLSamplerState>>,
    pipelines: Mutex<PipelineCache>,
    persist_bytes: AtomicU64,
}

unsafe impl Send for GpuCore {}
unsafe impl Sync for GpuCore {}

impl GpuCore {
    pub(crate) fn shared() -> Result<Arc<Self>, EngineError> {
        registry().shared()
    }

    pub(crate) fn recover(stale: &Arc<Self>) -> Result<Arc<Self>, EngineError> {
        registry().recover(stale, Instant::now())
    }

    pub(crate) fn generation() -> u64 {
        registry().generation
    }

    fn create() -> Result<Self, EngineError> {
        let device =
            objc2_metal::MTLCreateSystemDefaultDevice().ok_or(EngineError::UnsupportedGpu {
                os: "macos",
                required: D8_REQUIRED,
                actual: smol_str::SmolStr::new_static("no Metal device"),
            })?;
        let caps = GpuCaps::collect(&device);
        caps.check()?;
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

    pub(crate) fn device(&self) -> &ProtocolObject<dyn MTLDevice> {
        &self.device
    }

    pub(crate) fn retain_device(&self) -> Retained<ProtocolObject<dyn MTLDevice>> {
        self.device.clone()
    }

    pub(crate) fn sampler(&self) -> &ProtocolObject<dyn MTLSamplerState> {
        &self.sampler
    }

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

    fn build(
        &self,
        key: PipelineKey,
    ) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, EngineError> {
        let shaders = self.shaders()?;
        build_pipeline(self.device(), shaders, key)
    }

    fn shaders(&self) -> Result<&ShaderSet, EngineError> {
        if let Some(shaders) = self.shaders.get() {
            return Ok(shaders);
        }
        let loaded = ShaderSet::load(self.device())?;
        let _ = self.shaders.set(loaded);
        Ok(self.shaders.get().expect("the shader set was set above"))
    }

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

pub(crate) const MAX_PERSIST_BUDGET: u64 = 512 * 1024 * 1024;

pub(crate) fn persist_bytes(size: (u32, u32)) -> u64 {
    u64::from(size.0.max(1)) * u64::from(size.1.max(1)) * 4
}

fn budget_allows(used: u64, requested: u64) -> bool {
    used.saturating_add(requested) <= MAX_PERSIST_BUDGET
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PersistPhase {
    Create,
    Rebuild,
}

fn budget_error(detail: SmolStr, phase: PersistPhase) -> EngineError {
    match phase {
        PersistPhase::Create => EngineError::AppInit { detail },
        PersistPhase::Rebuild => EngineError::Swapchain { detail },
    }
}

pub(crate) struct PersistLease {
    core: Arc<GpuCore>,
    bytes: u64,
}

impl PersistLease {
    pub(crate) fn new(core: Arc<GpuCore>, size: (u32, u32)) -> Result<Self, EngineError> {
        let bytes = persist_bytes(size);
        core.reserve_persist(bytes)
            .map_err(|detail| budget_error(detail, PersistPhase::Create))?;
        Ok(Self { core, bytes })
    }

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

struct PipelineCache {
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

    fn cached_pipelines(core: &GpuCore) -> usize {
        core.pipelines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .len()
    }

    #[test]
    fn persist_bytes_counts_bgra_pixels() {
        assert_eq!(super::persist_bytes((1, 1)), 4);
        assert_eq!(super::persist_bytes((2800, 1800)), 20_160_000);
        assert_eq!(super::persist_bytes((0, 0)), 4);
    }

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
        assert!(rebuild.to_string().contains("persist budget exceeded"));
    }

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

        let reused = registry.recover(&first, now).expect("reuse rebuilt device");
        assert!(
            Arc::ptr_eq(&recovered, &reused),
            "其它窗口已重建 ⇒ 直接复用，避免多窗口重复删建 MTLDevice"
        );
        assert_eq!(registry.generation, 2, "复用不产生新的设备");
    }

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
        budget
            .charge(now + RECOVERY_WINDOW)
            .expect("过窗后额度必须复位");
    }

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
