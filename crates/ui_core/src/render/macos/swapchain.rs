//! `CAMetalLayer` 的创建与**主线程独占**的属性写入口（`design/async-render-pipeline.md` §5.3）。
//!
//! drawable 由 `CAMetalDisplayLink` 的 update 提供，图层不再取 drawable（§5.7 删除
//! `nextDrawable` / `allowsNextDrawableTimeout`）。本模块只剩两件事：
//!
//! 1. 建图层并写下不随窗口变化的属性（设备 / 像素格式 / 3 张 drawable / 呈现语义）；
//! 2. 提供 `contentsScale` / `drawableSize` 的唯一写入口 —— 调用者必须是主线程。
//!
//! GPU 线程**不**碰这里的任何属性：它只读 drawable 纹理的实际尺寸（§5.4 的最后校验）。

use objc2::rc::Retained;
use objc2_foundation::NSSize;
use objc2_metal::MTLPixelFormat;
use objc2_quartz_core::CAMetalLayer;

use crate::{
    error::EngineError,
    render::macos::device::{Device, init_error},
};

/// drawable 池深度：`CAMetalLayer` 只接受 2 / 3；取 3 与三槽上传缓冲、三张在飞帧对齐。
const MAXIMUM_DRAWABLE_COUNT: usize = 3;

/// 建图层并绑定设备（`pixelFormat` 固定 `BGRA8Unorm_sRGB`，与 P07 契约的唯一编码点一致）。
///
/// 只在主线程调用（`CAMetalLayer` 属性的唯一写者，§5.3）。
pub(crate) fn new_layer(
    device: &Device,
    size: (u32, u32),
    scale: f64,
) -> Result<Retained<CAMetalLayer>, EngineError> {
    let layer = CAMetalLayer::new();
    layer.setDevice(Some(device.raw()));
    layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm_sRGB);
    layer.setOpaque(true);
    // 需要把 persist blit **进** drawable：`framebufferOnly` 只保证 render target 用法。
    layer.setFramebufferOnly(false);
    layer.setMaximumDrawableCount(MAXIMUM_DRAWABLE_COUNT);
    // 异步呈现：present 不等事务、由合成器按 vsync 呈现（两者都是默认值，显式写出契约）。
    layer.setPresentsWithTransaction(false);
    layer.setDisplaySyncEnabled(true);
    set_layer_size(&layer, size, scale);
    let actual = layer.device();
    if actual.as_deref() != Some(device.raw()) {
        return Err(init_error(
            "CAMetalLayer",
            "device was not retained by the layer",
        ));
    }
    Ok(layer)
}

/// 主线程写 `contentsScale` / `drawableSize`（§5.3 / §5.4）。
///
/// 只有窗口尺寸或 DPI 真的变化时才调用（`Renderer::set_layer_scale`）；GPU 线程读 drawable
/// 的实际尺寸做最后校验，因此这里的两帧滞后不会画错目标。
pub(crate) fn set_layer_size(layer: &Retained<CAMetalLayer>, size: (u32, u32), scale: f64) {
    let size = (size.0.max(1), size.1.max(1));
    layer.setContentsScale(scale);
    layer.setDrawableSize(NSSize::new(f64::from(size.0), f64::from(size.1)));
}
