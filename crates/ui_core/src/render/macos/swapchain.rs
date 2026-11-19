//! `CAMetalLayer` 的创建与**主线程独占**的属性写入口。
//!
//! drawable 由 `CAMetalDisplayLink` 的 update 提供，图层不再取 drawable（§5.7 删除
//! `nextDrawable` / `allowsNextDrawableTimeout`）。本模块只剩三件事：
//!
//! 1. 建图层并写下不随窗口变化的属性（设备 / 像素格式 / 3 张 drawable / 呈现语义 / 背景色 /
//!    live-resize 的贴放位置）；
//! 2. **layer-hosting 的几何责任**（C6）：`frame`/`bounds`（逻辑点）恒等于 view bounds，
//!    `contentsScale` / `drawableSize`（物理像素）恒等于 backing；
//! 3. 提供 `contentsScale` / `drawableSize` / `backgroundColor` / 几何的唯一写入口 —— 调用者必须
//!    是主线程。
//!
//! GPU 线程**不**碰这里的任何属性：它只读 drawable 纹理的实际尺寸（§5.4 的最后校验）。

use objc2::rc::Retained;
use objc2_core_graphics::{CGColor, CGColorSpace, kCGColorSpaceSRGB};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use objc2_metal::MTLPixelFormat;
use objc2_quartz_core::{CAMetalLayer, CATransaction};

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
    // **保留图层自己的 vsync 对齐**（R09-6 / AR3 §6.1）：节奏由 `CAMetalDisplayLink` 独占 ——
    // delegate 给出 `targetPresentationTimestamp`，提交只做无时刻的 `presentDrawable:`
    // （**不能**用 `presentDrawable:atTime:`，见 `render/macos/present.rs`）。
    //
    // Apple 的《Frame Pacing》把节奏源与这个开关**成对绑定**：
    // - VSync-On ⇒ 用 `CAMetalDisplayLink` **且** `displaySyncEnabled = YES`；
    // - VSync-Off ⇒ 用 semaphore 定时循环 **且** `displaySyncEnabled = NO`。
    // 本后端是前者（display link 驱动），所以这里必须是 `true`。
    // `false` 的文档化代价是「present 更快，但**可能出现短暂视觉撕裂**」
    // （`CAMetalLayer.displaySyncEnabled`）。
    layer.setDisplaySyncEnabled(true);
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

/// 主线程写 layer 的**逻辑几何**（C6）：`frame.origin = (0,0)`、`size = points`。
///
/// 责任归属：view 以 layer-hosting 安装（`NSView::setLayer`，见 `platform/macos/window.rs`）。
/// Apple `NSView.wantsLayer` 文档规定 layer-hosting 时 App **自己**负责 layer（AppKit 不响应
/// view 变化）；`CAMetalLayer::new()` 的 frame/bounds 默认是 zero，不写就只能依赖未文档化的
/// AppKit 行为。
///
/// 真机定案（2026-11-19，macOS 15.8.1 @192.168.1.6，证据见 `P1C-MAC-NOTES.md`）：当前构造顺序
/// 下 AppKit 实测**也**会在 `setLayer:` 与 resize 后把 frame/bounds 同步成 view bounds（与
/// `wantsLayer` 文档的 "hands off" 语义相反，与 WWDC 2012 Session 217 的说法一致）。因此这里
/// 写入的值与 AppKit 的值相同：是**幂等防御**，不改变可见行为，也不与 AppKit 竞争 ——
/// `CATransaction` 关闭隐式动画，写入只在主线程（§5.3 的唯一写者纪律）。
///
/// `setFrame:` 会按当前 `anchorPoint` 同时算好 `bounds` 与 `position`；不直接写 `position`
/// 是为了不假设 anchorPoint 的值（AppKit 管理时是 (0.5, 0.5)，但那是它的实现细节）。
pub(crate) fn set_layer_geometry(layer: &Retained<CAMetalLayer>, points: (f64, f64)) {
    let width = points.0.max(0.0);
    let height = points.1.max(0.0);
    // 关隐式动画：resize 时 CA 不会把 frame/bounds 动画插值（否则旧 drawable 被拉伸/发虚）。
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    layer.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(width, height),
    ));
    CATransaction::commit();
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

/// 主线程写逻辑几何（点）+ `contentsScale` / `drawableSize`（物理像素）（§5.3 / §5.4 / C6）。
///
/// 只有窗口尺寸或 DPI 真的变化时才调用（`Renderer::set_layer_scale`）；GPU 线程读 drawable
/// 的实际尺寸做最后校验，因此这里的两帧滞后不会画错目标。
///
/// 逻辑尺寸由 `size / scale` 得出：`size` 是 AppKit 的 backing 像素（`convertSizeToBacking`
/// 已按 scale 取整），除回点后与 view bounds 最多差 `0.5 / scale` pt；`attach_layer` 另用
/// view bounds 的精确点写一次（两者在真机日志里一致，见 [`set_layer_geometry`]）。
pub(crate) fn set_layer_size(layer: &Retained<CAMetalLayer>, size: (u32, u32), scale: f64) {
    let size = (size.0.max(1), size.1.max(1));
    set_layer_geometry(
        layer,
        (f64::from(size.0) / scale, f64::from(size.1) / scale),
    );
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

    /// AR3 §6.1（P1-5）：本后端是 VSync-On（`CAMetalDisplayLink` + 无时刻的 `presentDrawable:`），
    /// 图层必须保留 vsync 对齐（`displaySyncEnabled = true`）。
    ///
    /// 失败面：`false` 会把「尽快呈现」变成无 vsync 的 ASAP 上屏。Apple《Frame Pacing》把
    /// `displaySyncEnabled = NO` 与 VSync-Off 的 semaphore 定时循环成对绑定；
    /// `CAMetalLayer.displaySyncEnabled` 文档写明 `false` 可能产生撕裂。
    /// 这条断言直接读回图层属性，是这次 `true → false` 回归的单一守卫。
    #[cfg(target_os = "macos")]
    #[test]
    fn the_layer_keeps_display_sync_enabled_for_the_display_link_path() {
        use crate::{paint::color::Color, render::macos::device::Device};

        let device = Device::new().expect("metal device");
        let layer = super::new_layer(&device, (64, 48), 1.0, Color::default()).expect("layer");
        assert!(
            layer.displaySyncEnabled(),
            "VSync-On + CAMetalDisplayLink 必须 displaySyncEnabled = true（否则有撕裂风险）"
        );
    }

    /// C6：layer-hosting 的几何责任在 App 侧 —— 建层之后 frame/bounds 就必须是 view 的**逻辑
    /// 尺寸**（`frame.origin = (0,0)`），而不是 `CAMetalLayer::new()` 的默认 zero；`drawableSize`
    /// 仍是 backing 物理像素 = 点尺寸 × scale。
    ///
    /// 这条用例断言的是图层自己的几何，不需要 NSWindow / view（audit §6.4-3 指出可脱离窗口建层）。
    #[cfg(target_os = "macos")]
    #[test]
    fn a_new_layer_carries_view_geometry_in_points_and_backing_pixels() {
        use crate::{paint::color::Color, render::macos::device::Device};

        let device = Device::new().expect("metal device");
        let layer = super::new_layer(&device, (128, 96), 2.0, Color::default()).expect("layer");
        assert_layer_geometry(&layer, (64.0, 48.0), (128.0, 96.0), 2.0);
    }

    /// C6：resize 的唯一写入口（`set_layer_size`，由主线程 `Renderer::set_layer_scale` 调用）
    /// 必须让逻辑几何与物理像素**同步**更新：尺寸变化与 scale 变化各测一次。
    #[cfg(target_os = "macos")]
    #[test]
    fn a_resize_keeps_layer_geometry_and_drawable_size_in_lockstep() {
        use crate::{paint::color::Color, render::macos::device::Device};

        let device = Device::new().expect("metal device");
        let layer = super::new_layer(&device, (128, 96), 2.0, Color::default()).expect("layer");
        // 尺寸变化（scale 不变）：点 100×80，物理 200×160。
        super::set_layer_size(&layer, (200, 160), 2.0);
        assert_layer_geometry(&layer, (100.0, 80.0), (200.0, 160.0), 2.0);
        // scale 变化（逻辑尺寸不变）：点 100×80，物理 300×240。
        super::set_layer_size(&layer, (300, 240), 3.0);
        assert_layer_geometry(&layer, (100.0, 80.0), (300.0, 240.0), 3.0);
    }

    /// C6：`attach_layer` 用 view bounds 的**精确点**写几何。
    ///
    /// `new_layer` 只有 backing / scale，半像素对齐时点尺寸会差 `0.5/scale` pt
    /// （`round(1281.25 × 2) / 2 = 1281.5` ≠ 1281.25）；attach 拿到 view 真值后必须覆盖成精确点，
    /// 且不改变已经写好的 backing 物理像素。
    #[cfg(target_os = "macos")]
    #[test]
    fn attaching_from_view_points_rewrites_the_exact_point_size() {
        use crate::{paint::color::Color, render::macos::device::Device};

        let device = Device::new().expect("metal device");
        // backing 2563 px @ scale 2 = 1281.5 pt（不是 view 的 1281.25 pt）。
        let layer = super::new_layer(&device, (2563, 1441), 2.0, Color::default()).expect("layer");
        super::set_layer_geometry(&layer, (1281.25, 720.25));
        assert_layer_geometry(&layer, (1281.25, 720.25), (2563.0, 1441.0), 2.0);
    }

    /// 几何断言：`frame.origin = (0,0)`、`frame.size == bounds.size == 点尺寸`、
    /// `drawableSize = 物理像素`、`contentsScale = scale`。
    #[cfg(target_os = "macos")]
    fn assert_layer_geometry(
        layer: &objc2::rc::Retained<objc2_quartz_core::CAMetalLayer>,
        points: (f64, f64),
        pixels: (f64, f64),
        scale: f64,
    ) {
        fn assert_close(actual: f64, expected: f64, what: &str) {
            assert!(
                (actual - expected).abs() <= 1e-9,
                "{what}: expected {expected}, got {actual}"
            );
        }

        let frame = layer.frame();
        let bounds = layer.bounds();
        assert_close(frame.origin.x, 0.0, "frame.origin.x");
        assert_close(frame.origin.y, 0.0, "frame.origin.y");
        assert_close(frame.size.width, points.0, "frame.size.width");
        assert_close(frame.size.height, points.1, "frame.size.height");
        assert_close(bounds.origin.x, 0.0, "bounds.origin.x");
        assert_close(bounds.origin.y, 0.0, "bounds.origin.y");
        assert_close(bounds.size.width, points.0, "bounds.size.width");
        assert_close(bounds.size.height, points.1, "bounds.size.height");
        let drawable = layer.drawableSize();
        assert_close(drawable.width, pixels.0, "drawableSize.width");
        assert_close(drawable.height, pixels.1, "drawableSize.height");
        assert_close(layer.contentsScale(), scale, "contentsScale");
    }
}
