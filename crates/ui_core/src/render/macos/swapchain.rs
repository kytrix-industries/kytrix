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

pub(crate) const MAXIMUM_DRAWABLE_COUNT: usize = 3;

use objc2_quartz_core::kCAGravityBottomLeft as LIVE_RESIZE_GRAVITY;

pub(crate) fn new_layer(
    device: &Device,
    size: (u32, u32),
    scale: f64,
    background: Color,
) -> Result<Retained<CAMetalLayer>, EngineError> {
    let layer = CAMetalLayer::new();
    configure_layer(&layer, device, size, scale, background)?;
    Ok(layer)
}

pub(crate) fn configure_layer(
    layer: &Retained<CAMetalLayer>,
    device: &Device,
    size: (u32, u32),
    scale: f64,
    background: Color,
) -> Result<(), EngineError> {
    layer.setDevice(Some(device.raw()));
    layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm_sRGB);
    layer.setOpaque(true);
    layer.setFramebufferOnly(false);
    layer.setMaximumDrawableCount(MAXIMUM_DRAWABLE_COUNT);
    layer.setPresentsWithTransaction(false);
    layer.setDisplaySyncEnabled(true);
    layer.setContentsGravity(unsafe { LIVE_RESIZE_GRAVITY });
    declare_srgb_colorspace(layer);
    let color = srgb_background(background);
    layer.setBackgroundColor(Some(&color));
    set_layer_size(layer, size, scale);
    verify_layer_device(layer, device)
}

pub(crate) fn reuse_layer(
    layer: &Retained<CAMetalLayer>,
    device: &Device,
    size: (u32, u32),
    scale: f64,
    background: Color,
) -> Result<(), EngineError> {
    layer.setDevice(Some(device.raw()));
    layer.setDisplaySyncEnabled(true);
    layer.setContentsGravity(unsafe { LIVE_RESIZE_GRAVITY });
    declare_srgb_colorspace(layer);
    let color = srgb_background(background);
    layer.setBackgroundColor(Some(&color));
    set_layer_size(layer, size, scale);
    verify_layer_device(layer, device)
}

fn declare_srgb_colorspace(layer: &Retained<CAMetalLayer>) {
    if crate::render::colorspace::layer_uses_srgb()
        && let Some(space) = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))
    {
        layer.setColorspace(Some(&space));
    }
}

fn srgb_background(background: Color) -> objc2_core_foundation::CFRetained<CGColor> {
    let [red, green, blue, _] = color::background_srgb(background);
    CGColor::new_srgb(f64::from(red), f64::from(green), f64::from(blue), 1.0)
}

fn verify_layer_device(layer: &Retained<CAMetalLayer>, device: &Device) -> Result<(), EngineError> {
    let actual = layer.device();
    if actual.as_deref() != Some(device.raw()) {
        return Err(init_error(
            "CAMetalLayer",
            "device was not retained by the layer",
        ));
    }
    Ok(())
}

pub(crate) fn set_layer_geometry(layer: &Retained<CAMetalLayer>, points: (f64, f64)) {
    let width = points.0.max(0.0);
    let height = points.1.max(0.0);
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    layer.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(width, height),
    ));
    CATransaction::commit();
}

pub(crate) fn set_layer_background(layer: &Retained<CAMetalLayer>, background: Color) {
    let color = srgb_background(background);
    layer.setBackgroundColor(Some(&color));
}

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

    #[cfg(target_os = "macos")]
    #[test]
    fn the_live_resize_gravity_anchors_the_old_drawable_at_the_visual_top_left() {
        let gravity = unsafe { LIVE_RESIZE_GRAVITY };
        assert_eq!(gravity.to_string(), "bottomLeft");
    }

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

    #[cfg(target_os = "macos")]
    #[test]
    fn a_new_layer_carries_view_geometry_in_points_and_backing_pixels() {
        use crate::{paint::color::Color, render::macos::device::Device};

        let device = Device::new().expect("metal device");
        let layer = super::new_layer(&device, (128, 96), 2.0, Color::default()).expect("layer");
        assert_layer_geometry(&layer, (64.0, 48.0), (128.0, 96.0), 2.0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_resize_keeps_layer_geometry_and_drawable_size_in_lockstep() {
        use crate::{paint::color::Color, render::macos::device::Device};

        let device = Device::new().expect("metal device");
        let layer = super::new_layer(&device, (128, 96), 2.0, Color::default()).expect("layer");
        super::set_layer_size(&layer, (200, 160), 2.0);
        assert_layer_geometry(&layer, (100.0, 80.0), (200.0, 160.0), 2.0);
        super::set_layer_size(&layer, (300, 240), 3.0);
        assert_layer_geometry(&layer, (100.0, 80.0), (300.0, 240.0), 3.0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn attaching_from_view_points_rewrites_the_exact_point_size() {
        use crate::{paint::color::Color, render::macos::device::Device};

        let device = Device::new().expect("metal device");
        let layer = super::new_layer(&device, (2563, 1441), 2.0, Color::default()).expect("layer");
        super::set_layer_geometry(&layer, (1281.25, 720.25));
        assert_layer_geometry(&layer, (1281.25, 720.25), (2563.0, 1441.0), 2.0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn reusing_a_layer_after_a_device_rebuild_rewrites_geometry_and_background() {
        use crate::{paint::color::Color, render::macos::device::Device};

        let device = Device::new().expect("metal device");
        let layer = super::new_layer(&device, (128, 96), 2.0, Color::default()).expect("layer");
        let device = Device::new().expect("metal device");
        let background = Color::from_rgba8([1, 2, 3, 255]);
        super::reuse_layer(&layer, &device, (300, 240), 3.0, background).expect("reuse layer");
        assert_layer_geometry(&layer, (100.0, 80.0), (300.0, 240.0), 3.0);
        assert!(
            layer.device().as_deref() == Some(device.raw()),
            "复用图层必须把 device 换成恢复后的当前实例"
        );
        assert!(
            layer.backgroundColor().is_some(),
            "背景色必须按当前窗口配置重写（旧 drawable 之外的条带由它填充）"
        );
    }

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
