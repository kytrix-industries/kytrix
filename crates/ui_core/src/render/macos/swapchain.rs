//! `CAMetalLayer` 的创建与**主线程独占**的属性写入口（`design/async-render-pipeline.md` §5.3）。
//!
//! drawable 由 `CAMetalDisplayLink` 的 update 提供，图层不再取 drawable（§5.7 删除
//! `nextDrawable` / `allowsNextDrawableTimeout`）。本模块只剩两件事：
//!
//! 1. 建图层并写下不随窗口变化的属性（设备 / 像素格式 / 3 张 drawable / 呈现语义 / 背景色 /
//!    live-resize 的贴放位置）；
//! 2. 提供 `contentsScale` / `drawableSize` / `backgroundColor` 的唯一写入口 —— 调用者必须是主线程。
//!
//! GPU 线程**不**碰这里的任何属性：它只读 drawable 纹理的实际尺寸（§5.4 的最后校验）。

use objc2::rc::Retained;
use objc2_core_graphics::{CGColor, CGColorSpace, kCGColorSpaceSRGB};
use objc2_foundation::NSSize;
use objc2_metal::MTLPixelFormat;
use objc2_quartz_core::CAMetalLayer;

use crate::{
    error::EngineError,
    paint::color::Color,
    render::{
        color,
        macos::device::{Device, init_error},
    },
};

/// drawable 池深度：`CAMetalLayer` 只接受 2 / 3；取 3 与三槽上传缓冲、三张在飞帧对齐。
///
/// `compositor` 的 drawable 同步表也按这个深度分配容量（同时留下的纹理数与图层池相同）。
pub(crate) const MAXIMUM_DRAWABLE_COUNT: usize = 3;

/// 旧 drawable 在 live resize 期间的贴放位置：**不缩放**，固定视觉左上角。
///
/// `CAMetalLayer` 默认 `contentsGravity = resize`：AppKit 已把 layer bounds 同步放大而
/// `drawableSize` 还是旧值时，CA 会把旧 drawable 拉伸/发虚。固定到视觉左上角后旧内容保持
/// 1:1（屏幕上是旧内容 + `backgroundColor` 补齐的新条带），直到新尺寸的 drawable 到位。
///
/// **真机 P1 实测**：本视图（`GuiView.isFlipped = true`）下 CA 的 `bottomLeft` 才是视觉左上。
/// 用 `TopLeft` 会把旧内容贴到左下（条带在上/右）；`BottomLeft` 下旧内容贴左上、条带在右/下，
/// stale 帧的 card 位置与条带颜色（= 背景色）均实测正确。
///
/// SAFETY（每次读取这个 extern static）：CoreAnimation 的全局 `NSString` 常量，进程生命周期内
/// 有效。用别名而不是包装函数/常量：它必须在调用点以 `unsafe` 读取，但方向只在这一个名字上定义。
use objc2_quartz_core::kCAGravityBottomLeft as LIVE_RESIZE_GRAVITY;

/// 建图层并绑定设备（`pixelFormat` 固定 `BGRA8Unorm_sRGB`，与 P07 契约的唯一编码点一致）。
///
/// 只在主线程调用（`CAMetalLayer` 属性的唯一写者，§5.3）。`background` 同时写进
/// `backgroundColor`（sRGB `CGColor`）：live resize 期间旧 drawable 之外的新区域由它填充。
pub(crate) fn new_layer(
    device: &Device,
    size: (u32, u32),
    scale: f64,
    background: Color,
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
    // **关闭图层自己的 vsync 对齐**（R09-6）：节奏由 `CAMetalDisplayLink` 独占 —— delegate 给出
    // `targetPresentationTimestamp`、提交用 `presentDrawable:atTime:` 钉住那一帧。图层再对齐一次
    // 就是两道节流叠加：`displaySyncEnabled = true` 时 CA 会按自己的调度点再挪一帧，把已经钉好的
    // 呈现时刻又推后（且时序不可观测）。
    layer.setDisplaySyncEnabled(false);
    // SAFETY: 同别名上的说明：CoreAnimation 的全局字符串常量，进程生命周期内有效。
    layer.setContentsGravity(unsafe { LIVE_RESIZE_GRAVITY });
    // **显式声明图层的颜色空间为 sRGB**（P2-3.1）：`CAMetalLayer` 默认跟随窗口所在屏幕的
    // 颜色空间，在 P3 显示器上会变成 Display P3 —— 而我们写进去的内容是 sRGB 编码
    // （`pixelFormat = BGRA8Unorm_sRGB`）。不声明时，同一份内容在 P3 与 sRGB 屏之间
    // 颜色不一致（P3 上过饱和）。声明 sRGB 后由合成器负责到屏幕空间的转换，双屏一致。
    // R06-10：色彩空间的决定在共享层（`render::colorspace`），这里只做 CoreAnimation 翻译。
    if crate::render::colorspace::layer_uses_srgb()
        && let Some(space) = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))
    {
        layer.setColorspace(Some(&space));
    }
    set_layer_background(&layer, background);
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

/// 主线程写 layer 的背景色（旧 drawable 之外、AppKit 刚暴露的条带）。
///
/// 用 sRGB 颜色空间 + alpha 1 的 `CGColor`：与 `pixelFormat = BGRA8Unorm_sRGB` 的呈现一致。
/// 新 drawable **内部**的条带不走这里（由 persist 的 clear color 填，值是 linear）。
pub(crate) fn set_layer_background(layer: &Retained<CAMetalLayer>, background: Color) {
    let [red, green, blue, _] = color::background_srgb(background);
    let color = CGColor::new_srgb(f64::from(red), f64::from(green), f64::from(blue), 1.0);
    layer.setBackgroundColor(Some(&color));
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

#[cfg(test)]
mod tests {
    use super::LIVE_RESIZE_GRAVITY;

    /// live resize 的贴放位置钉在视觉左上（flipped NSView 下 CA 的 `bottomLeft`）。
    #[cfg(target_os = "macos")]
    #[test]
    fn the_live_resize_gravity_anchors_the_old_drawable_at_the_visual_top_left() {
        // SAFETY: CoreAnimation 的全局字符串常量，进程生命周期内有效。
        let gravity = unsafe { LIVE_RESIZE_GRAVITY };
        assert_eq!(gravity.to_string(), "bottomLeft");
    }
}
