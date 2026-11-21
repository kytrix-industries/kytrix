//! 图片节点载荷（E7）：`ImageFit` / `ImageSource` / `ImageContent`。
//!
//! 语义（`spec-deltas/E7.md`）：
//! - 图片是**叶子**节点：尺寸来自 `Style`（显式尺寸优先）或资源内在尺寸（tile 逻辑尺寸）；
//! - [`ImageFit`] 决定绘制矩形：`Fill` 拉伸、`Contain` 等比内嵌居中、`Cover` 等比铺满并居中裁切；
//! - 资源未就绪（[`ImageSource::Pending`]）或已释放时**不画图元**，节点自身的背景/边框照常绘制
//!   （占位 = 节点背景 / 透明）；资源到达后由 `App::poll_resources` 自动失效重绘；
//! - 绘制产出仍是 `Primitive::Image`（I2：不新增图元变体）。`Cover` 不在 shader 里做裁切：
//!   引擎把**图集 tile 的 UV 收缩**到可见源区域（同 page / generation / 分配身份），
//!   图元 `bounds` 恒等于节点盒 —— 布局与绘制一致，且不需要 clip 栈。

use crate::{
    math::{Rect, Size},
    paint::{atlas::AtlasTile, color::Color},
    resources::{PendingResource, ResourceId},
};

/// 图片在节点盒内的填充方式（CSS `object-fit` 的最小集）。
///
/// 默认 `Fill`：与 `NodeBuilder::image(resource, fit)` 的显式参数一起使用时不会用到默认值，
/// 但 `ImageContent` 的 `Default` 需要一个确定语义（避免「未指定即意外裁切」）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ImageFit {
    /// 拉伸到节点盒（不保持宽高比）。
    #[default]
    Fill,
    /// 等比缩放内嵌居中：完整可见，多余空间留白（节点背景透出）。
    Contain,
    /// 等比缩放铺满节点盒并居中裁切：溢出部分不绘制。
    Cover,
}

/// 图片资源来源：已就绪句柄，或异步解码中的句柄。
///
/// `Pending` 由引擎在 `App::poll_resources` 交付 `Ready` 时自动改写为 `Ready`（同一节点，
/// 不重建、不丢布局位置）；`Failed` 的项保持 `Pending`（占位），由应用决定重试或换节点。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageSource {
    /// 已装箱进图片图集的资源（`ResourceImage.id`）。
    Ready(ResourceId),
    /// 异步解码中的资源（`ResourceCache::insert_async[_at_size]` 的返回值）。
    Pending(PendingResource),
}

impl ImageSource {
    /// 已就绪的资源句柄（`Pending` = `None`）。
    #[must_use]
    pub const fn resource(self) -> Option<ResourceId> {
        match self {
            Self::Ready(id) => Some(id),
            Self::Pending(_) => None,
        }
    }
}

/// 图片节点载荷（`NodeKind::Image`）。
///
/// **字段全部私有**：构造走 [`crate::node::builder::NodeBuilder::image`] /
/// `image_pending`，读取走访问器。`intrinsic` 是引擎解析出来的**逻辑像素**内在尺寸
/// （资源 tile 的物理像素 ÷ 窗口 scale）；`None` = 资源未就绪 / 尚未解析。
#[derive(Clone, Debug, PartialEq)]
pub struct ImageContent {
    source: ImageSource,
    fit: ImageFit,
    modulation: Color,
    intrinsic: Option<Size>,
}

impl ImageContent {
    /// 引擎内部构造（公开构造入口是 `NodeBuilder::image` / `image_pending`）。
    pub(crate) fn new(source: ImageSource, fit: ImageFit) -> Self {
        Self {
            source,
            fit,
            modulation: Color::WHITE,
            intrinsic: None,
        }
    }

    /// 资源来源（就绪句柄或异步句柄）。
    #[must_use]
    pub const fn source(&self) -> ImageSource {
        self.source
    }

    /// 填充方式。
    #[must_use]
    pub const fn fit(&self) -> ImageFit {
        self.fit
    }

    /// 颜色调制（`texel * modulation` 的预乘空间语义见 `paint::primitive::Image`）。
    #[must_use]
    pub const fn modulation(&self) -> Color {
        self.modulation
    }

    /// 已解析的内在尺寸（逻辑像素；`None` = 资源未就绪 / 尚未解析）。
    #[must_use]
    pub const fn intrinsic(&self) -> Option<Size> {
        self.intrinsic
    }

    /// 引擎内部：改写资源来源（异步交付 / 换句柄）。
    pub(crate) fn set_source(&mut self, source: ImageSource) {
        self.source = source;
    }

    /// 引擎内部：改写颜色调制（`NodeBuilder::image_modulation`）。
    pub(crate) fn set_modulation(&mut self, modulation: Color) {
        self.modulation = modulation;
    }

    /// 引擎内部：写入解析后的内在尺寸（`App::refresh_images` / 异步交付）。
    pub(crate) fn set_intrinsic(&mut self, intrinsic: Size) {
        self.intrinsic = Some(intrinsic);
    }
}

/// `Contain` / `Fill` 的绘制矩形（`Cover` 的**可见**矩形恒为节点盒，源区域由
/// [`cover_tile`] 裁）。
///
/// 返回 `None` = 节点盒或图片尺寸非正（不画）。`image` 是资源 tile 的像素尺寸（宽高比与
/// 源图一致：`ResourceCache` 按目标尺寸解码时等比 fit）。
pub(crate) fn fit_bounds(bounds: Rect, fit: ImageFit, image: Size) -> Option<Rect> {
    if bounds.size.is_empty() || image.is_empty() {
        return None;
    }
    match fit {
        ImageFit::Fill | ImageFit::Cover => Some(bounds),
        ImageFit::Contain => {
            let scale = (bounds.size.width / image.width).min(bounds.size.height / image.height);
            let width = image.width * scale;
            let height = image.height * scale;
            Some(Rect::new(
                bounds.origin.x + (bounds.size.width - width) * 0.5,
                bounds.origin.y + (bounds.size.height - height) * 0.5,
                width,
                height,
            ))
        }
    }
}

/// `Cover` 的源区域：把 tile 的 UV 收缩到居中可见的源矩形（同 page / generation / 分配身份）。
///
/// 图元 `bounds` 仍等于节点盒，shader 按收缩后的 UV 采样 ⇒ 等比铺满 + 居中裁切。
/// 返回 `None` = 节点盒或图片尺寸非正（不画）。
pub(crate) fn cover_tile(bounds: Rect, image: Size, tile: AtlasTile) -> Option<AtlasTile> {
    if bounds.size.is_empty() || image.is_empty() {
        return None;
    }
    let scale = (bounds.size.width / image.width).max(bounds.size.height / image.height);
    // 可见源区域占整张图的比例（≤ 1；节点盒比铺满后的图小，才需要裁）。
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

/// 在 `[start, end]` 上取居中的 `fraction` 子区间（fraction ∈ `0..=1`）。
fn crop_axis(start: f32, end: f32, fraction: f32) -> (f32, f32) {
    let span = (end - start) * fraction;
    let first = start + (end - start - span) * 0.5;
    (first, first + span)
}

#[cfg(test)]
mod tests {
    use super::{ImageFit, ImageSource, cover_tile, fit_bounds};
    use crate::{
        math::{Rect, Size},
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

    /// 内在尺寸 = tile 像素尺寸（`ResourceImage.width/height`）；三 fit 的几何事实。
    #[test]
    fn fill_stretches_to_the_node_box() {
        let bounds = Rect::new(10.0, 20.0, 100.0, 50.0);
        assert_eq!(
            fit_bounds(bounds, ImageFit::Fill, Size::new(10.0, 10.0)),
            Some(bounds),
            "Fill 不保持宽高比：直接等于节点盒"
        );
    }

    #[test]
    fn contain_centers_a_letterboxed_rect() {
        let bounds = Rect::new(10.0, 20.0, 100.0, 50.0);
        // 100×50 盒 + 10×10 图 ⇒ scale = 5 ⇒ 50×50 居中（x 偏移 25）。
        assert_eq!(
            fit_bounds(bounds, ImageFit::Contain, Size::new(10.0, 10.0)),
            Some(Rect::new(35.0, 20.0, 50.0, 50.0)),
            "Contain = 等比内嵌居中"
        );
    }

    #[test]
    fn cover_crops_the_source_uv_centered() {
        // 100×50 盒 + 方形图 ⇒ cover scale = 100/10 = 10 ⇒ 显示 100×100，
        // 水平方向完整、垂直方向居中裁到一半 ⇒ v 占原 UV 的中间 50%。
        let cropped = cover_tile(
            Rect::new(10.0, 20.0, 100.0, 50.0),
            Size::new(10.0, 10.0),
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
        // v ∈ [0.5, 0.75] 的中间一半 = [0.5625, 0.6875]。
        assert!((cropped.v0 - 0.5625).abs() < 1e-6, "v0 = {cropped:?}");
        assert!((cropped.v1 - 0.6875).abs() < 1e-6, "v1 = {cropped:?}");
        assert_eq!(
            fit_bounds(
                Rect::new(10.0, 20.0, 100.0, 50.0),
                ImageFit::Cover,
                Size::new(10.0, 10.0)
            ),
            Some(Rect::new(10.0, 20.0, 100.0, 50.0)),
            "Cover 的图元 bounds 恒等于节点盒（源区域裁在 UV 上）"
        );
    }

    #[test]
    fn degenerate_sizes_produce_no_geometry() {
        let ok = Rect::new(0.0, 0.0, 10.0, 10.0);
        for bounds in [
            Rect::new(0.0, 0.0, 0.0, 10.0),
            Rect::new(0.0, 0.0, 10.0, f32::NAN),
        ] {
            assert_eq!(
                fit_bounds(bounds, ImageFit::Fill, Size::new(1.0, 1.0)),
                None
            );
            assert_eq!(cover_tile(bounds, Size::new(1.0, 1.0), tile()), None);
        }
        for image in [Size::ZERO, Size::new(f32::INFINITY, 1.0)] {
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
}
