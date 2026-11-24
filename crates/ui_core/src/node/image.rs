use crate::{
    math::{DevicePixelRatio, DeviceSize, LogicalRect, LogicalSize, dp},
    paint::{atlas::AtlasTile, color::Color},
    resources::{PendingResource, ResourceId},
};
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ImageFit {
    #[default]
    Fill,
    Contain,
    Cover,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageSource {
    Ready(ResourceId),
    Pending(PendingResource),
}

impl ImageSource {
    #[must_use]
    pub const fn resource(self) -> Option<ResourceId> {
        match self {
            Self::Ready(id) => Some(id),
            Self::Pending(_) => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImageContent {
    source: ImageSource,
    fit: ImageFit,
    modulation: Color,
    intrinsic: Option<LogicalSize>,
    prepared_target: Option<DeviceSize>,
}

impl ImageContent {
    pub(crate) fn new(source: ImageSource, fit: ImageFit) -> Self {
        Self {
            source,
            fit,
            modulation: Color::WHITE,
            intrinsic: None,
            prepared_target: None,
        }
    }

    #[must_use]
    pub const fn source(&self) -> ImageSource {
        self.source
    }

    #[must_use]
    pub const fn fit(&self) -> ImageFit {
        self.fit
    }

    #[must_use]
    pub const fn modulation(&self) -> Color {
        self.modulation
    }

    #[must_use]
    pub const fn intrinsic(&self) -> Option<LogicalSize> {
        self.intrinsic
    }

    #[must_use]
    pub const fn prepared_target(&self) -> Option<DeviceSize> {
        self.prepared_target
    }

    pub(crate) fn set_source(&mut self, source: ImageSource) {
        self.source = source;
        self.prepared_target = None;
    }

    pub(crate) fn set_modulation(&mut self, modulation: Color) {
        self.modulation = modulation;
    }

    pub(crate) fn set_intrinsic(&mut self, intrinsic: LogicalSize) {
        self.intrinsic = Some(intrinsic);
    }

    pub(crate) fn set_prepared_target(&mut self, target: DeviceSize) {
        self.prepared_target = Some(target);
    }
}

pub(crate) fn fit_bounds(
    bounds: LogicalRect,
    fit: ImageFit,
    image: LogicalSize,
) -> Option<LogicalRect> {
    if bounds.size.is_empty() || image.is_empty() {
        return None;
    }
    match fit {
        ImageFit::Fill | ImageFit::Cover => Some(bounds),
        ImageFit::Contain => {
            let scale = (bounds.size.width / image.width).min(bounds.size.height / image.height);
            let width = image.width * scale;
            let height = image.height * scale;
            Some(LogicalRect::new(
                bounds.origin.x + (bounds.size.width - width) * 0.5,
                bounds.origin.y + (bounds.size.height - height) * 0.5,
                width,
                height,
            ))
        }
    }
}

pub(crate) fn cover_tile(
    bounds: LogicalRect,
    image: LogicalSize,
    tile: AtlasTile,
) -> Option<AtlasTile> {
    if bounds.size.is_empty() || image.is_empty() {
        return None;
    }
    let scale = (bounds.size.width / image.width).max(bounds.size.height / image.height);
    let visible_u = (bounds.size.width / (image.width * scale)).clamp(0.0, 1.0);
    let visible_v = (bounds.size.height / (image.height * scale)).clamp(0.0, 1.0);
    let (u0, u1) = crop_axis(tile.u0, tile.u1, visible_u);
    let (v0, v1) = crop_axis(tile.v0, tile.v1, visible_v);
    Some(AtlasTile {
        u0,
        u1,
        v0,
        v1,
        ..tile
    })
}

fn crop_axis(start: f32, end: f32, fraction: f32) -> (f32, f32) {
    let span = (end - start) * fraction;
    let first = start + (end - start - span) * 0.5;
    (first, first + span)
}

pub(crate) fn variant_target(
    bounds: LogicalRect,
    fit: ImageFit,
    intrinsic: LogicalSize,
    dpr: DevicePixelRatio,
) -> Option<DeviceSize> {
    if bounds.size.is_empty() || intrinsic.is_empty() {
        return None;
    }
    let drawn = match fit {
        ImageFit::Fill => bounds.size,
        ImageFit::Contain => {
            let scale =
                (bounds.size.width / intrinsic.width).min(bounds.size.height / intrinsic.height);
            intrinsic * scale
        }
        ImageFit::Cover => {
            let scale =
                (bounds.size.width / intrinsic.width).max(bounds.size.height / intrinsic.height);
            intrinsic * scale
        }
    };
    let physical = dpr.to_physical_size(drawn);
    let width = physical.width.value();
    let height = physical.height.value();
    if !(width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0) {
        return None;
    }
    Some(DeviceSize::new(
        dp(width.ceil() as i32),
        dp(height.ceil() as i32),
    ))
}

#[cfg(test)]
mod tests {
    use super::{ImageFit, ImageSource, cover_tile, fit_bounds, variant_target};
    use crate::{
        math::{DevicePixelRatio, DeviceSize, LogicalRect, LogicalSize, dp, px},
        paint::atlas::{AtlasKind, AtlasTile},
        resources::{PendingResource, ResourceId},
    };

    fn tile() -> AtlasTile {
        AtlasTile {
            atlas: AtlasKind::Image,
            page: 2,
            u0: 0.25,
            v0: 0.5,
            u1: 0.75,
            v1: 0.75,
            generation: 7,
            slot: 11,
            slot_epoch: 3,
        }
    }

    #[test]
    fn fill_stretches_to_the_node_box() {
        let bounds = LogicalRect::new(px(10.0), px(20.0), px(100.0), px(50.0));
        assert_eq!(
            fit_bounds(bounds, ImageFit::Fill, LogicalSize::new(px(10.0), px(10.0))),
            Some(bounds),
            "Fill 不保持宽高比：直接等于节点盒"
        );
    }

    #[test]
    fn contain_centers_a_letterboxed_rect() {
        let bounds = LogicalRect::new(px(10.0), px(20.0), px(100.0), px(50.0));
        assert_eq!(
            fit_bounds(
                bounds,
                ImageFit::Contain,
                LogicalSize::new(px(10.0), px(10.0))
            ),
            Some(LogicalRect::new(px(35.0), px(20.0), px(50.0), px(50.0))),
            "Contain = 等比内嵌居中"
        );
    }

    #[test]
    fn cover_crops_the_source_uv_centered() {
        let cropped = cover_tile(
            LogicalRect::new(px(10.0), px(20.0), px(100.0), px(50.0)),
            LogicalSize::new(px(10.0), px(10.0)),
            tile(),
        )
        .expect("cover tile");
        assert_eq!(cropped.atlas, AtlasKind::Image);
        assert_eq!((cropped.page, cropped.generation), (2, 7), "页/代际不变");
        assert_eq!(
            (cropped.slot, cropped.slot_epoch),
            (11, 3),
            "分配身份沿用：tile 仍指向同一次分配"
        );
        assert!(
            (cropped.u0 - 0.25).abs() < 1e-6,
            "宽度方向不裁：{cropped:?}"
        );
        assert!(
            (cropped.u1 - 0.75).abs() < 1e-6,
            "宽度方向不裁：{cropped:?}"
        );
        assert!((cropped.v0 - 0.5625).abs() < 1e-6, "v0 = {cropped:?}");
        assert!((cropped.v1 - 0.6875).abs() < 1e-6, "v1 = {cropped:?}");
        assert_eq!(
            fit_bounds(
                LogicalRect::new(px(10.0), px(20.0), px(100.0), px(50.0)),
                ImageFit::Cover,
                LogicalSize::new(px(10.0), px(10.0))
            ),
            Some(LogicalRect::new(px(10.0), px(20.0), px(100.0), px(50.0))),
            "Cover 的图元 bounds 恒等于节点盒（源区域裁在 UV 上）"
        );
    }

    #[test]
    fn degenerate_sizes_produce_no_geometry() {
        let ok = LogicalRect::new(px(0.0), px(0.0), px(10.0), px(10.0));
        for bounds in [
            LogicalRect::new(px(0.0), px(0.0), px(0.0), px(10.0)),
            LogicalRect::new(px(0.0), px(0.0), px(10.0), px(f32::NAN)),
        ] {
            assert_eq!(
                fit_bounds(bounds, ImageFit::Fill, LogicalSize::new(px(1.0), px(1.0))),
                None
            );
            assert_eq!(
                cover_tile(bounds, LogicalSize::new(px(1.0), px(1.0)), tile()),
                None
            );
        }
        for image in [
            LogicalSize::ZERO,
            LogicalSize::new(px(f32::INFINITY), px(1.0)),
        ] {
            assert_eq!(fit_bounds(ok, ImageFit::Contain, image), None);
            assert_eq!(cover_tile(ok, image, tile()), None);
        }
    }

    #[test]
    fn image_source_ready_and_pending_are_distinct() {
        let ready = ImageSource::Ready(ResourceId::for_test(3, 1));
        let pending = ImageSource::Pending(PendingResource::for_test(4, 2));
        assert_eq!(ready.resource(), Some(ResourceId::for_test(3, 1)));
        assert_eq!(pending.resource(), None);
        assert_ne!(ready, pending);
    }

    #[test]
    fn variant_target_fills_the_box_at_the_window_dpr() {
        let bounds = LogicalRect::new(px(10.0), px(20.0), px(100.0), px(50.0));
        let intrinsic = LogicalSize::new(px(10.0), px(10.0));
        assert_eq!(
            variant_target(bounds, ImageFit::Fill, intrinsic, DevicePixelRatio::ONE),
            Some(DeviceSize::new(dp(100), dp(50))),
            "Fill 的栅格目标 = 节点盒"
        );
        assert_eq!(
            variant_target(
                bounds,
                ImageFit::Fill,
                intrinsic,
                DevicePixelRatio::new(1.5).expect("scale")
            ),
            Some(DeviceSize::new(dp(150), dp(75))),
            "目标物理尺寸 = 绘制盒 × DPR"
        );
        assert_eq!(
            variant_target(
                LogicalRect::new(px(0.0), px(0.0), px(10.2), px(10.2)),
                ImageFit::Fill,
                intrinsic,
                DevicePixelRatio::ONE
            ),
            Some(DeviceSize::new(dp(11), dp(11))),
            "小数绘制盒必须向上取整"
        );
    }

    #[test]
    fn variant_target_keeps_the_intrinsic_aspect_for_contain_and_cover() {
        let bounds = LogicalRect::new(px(0.0), px(0.0), px(100.0), px(50.0));
        let intrinsic = LogicalSize::new(px(10.0), px(10.0));
        assert_eq!(
            variant_target(bounds, ImageFit::Contain, intrinsic, DevicePixelRatio::ONE),
            Some(DeviceSize::new(dp(50), dp(50))),
            "Contain 的栅格目标 = 内嵌后的实际绘制尺寸"
        );
        assert_eq!(
            variant_target(bounds, ImageFit::Cover, intrinsic, DevicePixelRatio::ONE),
            Some(DeviceSize::new(dp(100), dp(100))),
            "Cover 按覆盖盒的比例栅格化，UV 裁切发生在绘制期"
        );
    }

    #[test]
    fn variant_target_rejects_degenerate_geometry() {
        let bounds = LogicalRect::new(px(0.0), px(0.0), px(10.0), px(10.0));
        let intrinsic = LogicalSize::new(px(10.0), px(10.0));
        for empty in [
            LogicalRect::new(px(0.0), px(0.0), px(0.0), px(10.0)),
            LogicalRect::new(px(0.0), px(0.0), px(10.0), px(f32::NAN)),
        ] {
            assert_eq!(
                variant_target(empty, ImageFit::Fill, intrinsic, DevicePixelRatio::ONE),
                None
            );
        }
        for image in [
            LogicalSize::ZERO,
            LogicalSize::new(px(f32::INFINITY), px(1.0)),
        ] {
            assert_eq!(
                variant_target(bounds, ImageFit::Contain, image, DevicePixelRatio::ONE),
                None
            );
        }
    }
}
