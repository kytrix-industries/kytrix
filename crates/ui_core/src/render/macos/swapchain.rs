//! `CAMetalLayer` + `nextDrawable`（`design/phase-12-macos-linux.md` §5.3）。
//!
//! **无局部 present**：drawable 的 3 张纹理跨帧内容无保证 ⇒ 每帧整面 `blit`（结论 #4 / F-121）。

use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_foundation::NSSize;
use objc2_metal::MTLPixelFormat;
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};

use crate::{
    error::EngineError,
    render::macos::device::{Device, init_error},
};

/// 绘图图层（一个原生窗口一张）。
pub(crate) struct Swapchain {
    layer: Retained<CAMetalLayer>,
    size: (u32, u32),
    scale: f64,
    fresh: bool,
}

impl Swapchain {
    /// 建图层并绑定设备（`pixelFormat` 固定 `BGRA8Unorm_sRGB`，与 P07 契约的唯一编码点一致）。
    pub(crate) fn new(device: &Device, size: (u32, u32), scale: f64) -> Result<Self, EngineError> {
        let layer = CAMetalLayer::new();
        layer.setDevice(Some(device.raw()));
        let format = MTLPixelFormat::BGRA8Unorm_sRGB;
        layer.setPixelFormat(format);
        layer.setOpaque(true);
        // 需要把 persist blit **进** drawable：`framebufferOnly` 只保证 render target 用法。
        layer.setFramebufferOnly(false);
        // `nextDrawable` 现在只在 render 线程调用。仍保留超时：合成器暂停
        // （AX 查询 / 窗口被遮挡 / 屏幕录制）或关机时，worker 可以把 `None` 当作
        // 「本帧没拿到 drawable」重试，而不是永远卡在 `nextDrawable`。
        layer.setAllowsNextDrawableTimeout(true);
        layer.setMaximumDrawableCount(3);
        layer.setDisplaySyncEnabled(true);
        layer.setContentsScale(scale);
        let size = (size.0.max(1), size.1.max(1));
        layer.setDrawableSize(NSSize::new(f64::from(size.0), f64::from(size.1)));
        let actual = layer.device();
        if actual.as_deref() != Some(device.raw()) {
            return Err(init_error(
                "CAMetalLayer",
                "device was not retained by the layer",
            ));
        }
        Ok(Self {
            layer,
            size,
            scale,
            fresh: true,
        })
    }

    /// 交给平台层挂到 `NSView`（`setLayer`）。
    pub(crate) fn layer(&self) -> &CAMetalLayer {
        &self.layer
    }

    /// 下一次 present 是否必须全量（新建 / resize 之后为 `true`）。
    pub(crate) fn is_fresh(&self) -> bool {
        self.fresh
    }

    /// present 成功之后调用（下一次不再强制全量）。
    pub(crate) fn presented(&mut self) {
        self.fresh = false;
    }

    /// 取一个 drawable；`None` = 本帧没拿到（不报错，跳过本帧）。
    pub(crate) fn next_drawable(&self) -> Option<Retained<ProtocolObject<dyn CAMetalDrawable>>> {
        self.layer.nextDrawable()
    }

    /// resize：改 `contentsScale` 与 `drawableSize`（图层本身不重建）→ 下一次 present 全量。
    ///
    /// 调用方（render 线程）把 `NativeWindow` 读到的 scale 一起传进来；主线程不再写图层属性。
    pub(crate) fn resize(&mut self, size: (u32, u32), scale: f64) {
        let size = (size.0.max(1), size.1.max(1));
        self.scale = scale;
        self.layer.setContentsScale(scale);
        self.layer
            .setDrawableSize(NSSize::new(f64::from(size.0), f64::from(size.1)));
        self.size = size;
        self.fresh = true;
    }

    /// 只更新 DPI（显示模式切换时物理尺寸可能不变）；下一次仍按全量画。
    pub(crate) fn set_scale(&mut self, scale: f64) {
        if (scale - self.scale).abs() <= f64::EPSILON {
            return;
        }
        self.scale = scale;
        self.layer.setContentsScale(scale);
        self.fresh = true;
    }
}
