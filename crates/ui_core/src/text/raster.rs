//! 字形光栅：skrifa 取度量/轮廓，swash 出 R8 alpha（P05 §8）。
//!
//! skrifa 0.44 **没有光栅器**（只能用 `OutlineGlyph::draw` 到 `OutlinePen`），因此实际光栅由
//! swash 0.2.10 的 zeno 后端完成；两者共用同一份 `skrifa`（swash 的依赖区间 `<= 0.44`）。

use swash::{
    scale::{Render, ScaleContext, Source},
    zeno::{Format, Placement},
};

/// 字体身份：blob 的地址 + 长度 + TTC 索引 + 字体代际（B2）。
///
/// 地址只在 [`GlyphRasterizer`] 的 `fonts` 表**持有 `Blob` 的 clone** 时才稳定：parley 每次
/// `ranged_builder` 都会 `source_cache.prune(128, false)`，被释放的 blob 会让新 blob 复用同一地址。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FontKey {
    ptr: usize,
    len: usize,
    ttc_index: u32,
    font_epoch: u32,
}

impl FontKey {
    pub(crate) fn of(font: &parley::FontData, font_epoch: u32) -> Self {
        Self {
            ptr: font.data.data().as_ptr() as usize,
            len: font.data.data().len(),
            ttc_index: font.index,
            font_epoch,
        }
    }
}

/// 一个字体条目：**持有 `FontData` 的 clone（Blob 是 `Arc`）**，从而 pin 住 `FontKey::ptr`。
struct FontEntry {
    font: parley::FontData,
}

/// 一张光栅化后的字形位图（覆盖度，**不含**图集 padding）。
pub(crate) struct RasterGlyph {
    /// swash 的落点：`top` 是基线上方的像素数，`left` 可负。
    pub placement: Placement,
    /// R8 覆盖度，`len == width * height`，行 stride = width。
    pub data: Vec<u8>,
}

/// 一个 run 的字形变体参数：可变字体实例坐标 + 合成样式（faux bold）。
///
/// Parley 用这些参数算 advance，但**光栅必须收到同样的参数**，否则位图与排版对不上：
/// 可变字体（如 `Segoe UI Variable`）在 weight=700 时 parley 按加粗坐标排版，swash 若按
/// 默认轴（≈400）出位图，就是「粗体不变粗、字距不匹配」。
///
/// `hash` 参与 [`crate::text::atlas::GlyphKey`]：同一字形的不同实例必须各占一个 tile。
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct GlyphVariation {
    /// 归一化实例坐标（`-1..=1` 映射到 `i16::MIN..=i16::MAX`，即 `swash::NormalizedCoord`）。
    coords: Vec<i16>,
    /// faux bold 强度（像素；0 = 不合成）。
    embolden: f32,
    /// 参与图集缓存键的摘要。
    hash: u64,
}

impl GlyphVariation {
    /// 由 parley 的 run 参数构造（只处理可变字体实例与 faux bold；faux italic 需要 zeno
    /// `Transform`，当前不在依赖集内，未实现）。
    pub(crate) fn of(coords: &[i16], synthesis: fontique::Synthesis, ppem: f32) -> Self {
        use std::hash::{Hash, Hasher};
        let embolden = if synthesis.embolden() {
            // FreeType 的 faux bold 默认强度是 1/24 em。
            ppem / 24.0
        } else {
            0.0
        };
        let mut hasher = ahash::AHasher::default();
        coords.hash(&mut hasher);
        embolden.to_bits().hash(&mut hasher);
        Self {
            coords: coords.to_vec(),
            embolden,
            hash: hasher.finish(),
        }
    }

    /// 参与图集缓存键的摘要。
    pub(crate) fn hash(&self) -> u64 {
        self.hash
    }

    /// 全默认（无实例坐标、无合成）：光栅路径可以省掉额外的 scaler 配置。
    fn is_identity(&self) -> bool {
        self.coords.is_empty() && self.embolden <= 0.0
    }
}

/// 逐字形光栅器（每线程一个 `ScaleContext`，同 font+ppem 复用 `Scaler`）。
pub(crate) struct GlyphRasterizer {
    ctx: ScaleContext,
    fonts: ahash::HashMap<FontKey, FontEntry>,
    font_epoch: u32,
}

impl GlyphRasterizer {
    pub(crate) fn new() -> Self {
        Self {
            ctx: ScaleContext::new(),
            fonts: ahash::HashMap::default(),
            font_epoch: 1,
        }
    }

    pub(crate) fn font_epoch(&self) -> u32 {
        self.font_epoch
    }

    /// 字体换代：清空字体表（旧 `FontKey` 的 `ptr` 从此不再被 pin）。
    pub(crate) fn set_font_epoch(&mut self, epoch: u32) {
        if self.font_epoch != epoch {
            self.font_epoch = epoch;
            self.fonts.clear();
        }
    }

    /// 光栅化一个字形。`None` = 空字形（空格/无轮廓）或字体数据坏。
    pub(crate) fn rasterize(
        &mut self,
        font: &parley::FontData,
        glyph: u32,
        ppem: f32,
        variation: &GlyphVariation,
    ) -> Option<RasterGlyph> {
        if ppem <= 0.0 || !ppem.is_finite() || glyph > u32::from(u16::MAX) {
            return None;
        }
        let key = FontKey::of(font, self.font_epoch);
        let Self { ctx, fonts, .. } = self;
        let entry = fonts
            .entry(key)
            .or_insert_with(|| FontEntry { font: font.clone() });
        // 借用 self.fonts 的条目（pin 住 blob）与 self.ctx 是互不重叠的字段借用。
        let bytes = entry.font.data.data();
        let index = entry.font.index as usize;
        let swash_font = swash::FontRef::from_index(bytes, index)?;
        let mut builder = ctx.builder(swash_font).size(ppem).hint(true);
        if !variation.is_identity() {
            // 可变字体实例坐标必须与 parley 排版用的坐标一致（见 `GlyphVariation`）。
            builder = builder.normalized_coords(variation.coords.iter());
        }
        let mut scaler = builder.build();
        let mut render = Render::new(&[Source::Outline]);
        render.format(Format::Alpha);
        if variation.embolden > 0.0 {
            // faux bold 是 `Render` 的变换参数（不是 `Scaler` 的）。
            render.embolden(variation.embolden);
        }
        let image = render.render(&mut scaler, glyph as u16)?;
        Some(RasterGlyph {
            placement: image.placement,
            data: image.data,
        })
    }

    /// 仅测试：字体表里是否已经 pin 住该字体。
    #[cfg(test)]
    pub(crate) fn cached_fonts(&self) -> usize {
        self.fonts.len()
    }
}

impl Default for GlyphRasterizer {
    fn default() -> Self {
        Self::new()
    }
}

/// 测试与诊断用：从系统字体库取一个「一定能用」的字体数据（无系统字体时 `None`）。
#[cfg(test)]
pub(crate) fn probe_font() -> Option<parley::FontData> {
    use crate::paint::color::ColorBrush;

    let mut font_cx = parley::FontContext::new();
    let mut layout_cx: parley::LayoutContext<ColorBrush> = parley::LayoutContext::new();
    let mut builder = layout_cx.ranged_builder(&mut font_cx, "A", 1.0, true);
    builder.push_default(parley::StyleProperty::FontSize(16.0));
    let mut layout = builder.build("A");
    layout.break_all_lines(None);
    layout.lines().next()?.items().find_map(|item| match item {
        parley::PositionedLayoutItem::GlyphRun(run) => Some(run.run().font().clone()),
        parley::PositionedLayoutItem::InlineBox(_) => None,
    })
}

#[cfg(test)]
mod tests {
    use super::{GlyphRasterizer, GlyphVariation, probe_font};

    /// P1：实例坐标必须参与缓存键（否则不同字重共用一个 tile）。
    ///
    /// faux bold 的 `Synthesis` 字段是私有的（只能由 parley 产出），因此这里只覆盖实例坐标；
    /// faux bold 的路径由 `is_identity` 分支保证（提交里另有说明）。
    #[test]
    fn glyph_variation_distinguishes_instances() {
        let plain = GlyphVariation::of(&[], Default::default(), 16.0);
        let heavy = GlyphVariation::of(&[16_384], Default::default(), 16.0);
        let heavier = GlyphVariation::of(&[-16_384], Default::default(), 16.0);
        assert_ne!(plain.hash(), heavy.hash(), "实例坐标必须换 tile");
        assert_ne!(heavy.hash(), heavier.hash(), "不同轴值必须换 tile");
        assert_eq!(
            plain.hash(),
            GlyphVariation::of(&[], Default::default(), 16.0).hash(),
            "相同参数必须命中同一 tile"
        );
    }

    #[test]
    fn raster_latin_glyph_produces_alpha_bytes() {
        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let Some(glyph) = first_glyph(&font, 'A') else {
            eprintln!("skip: no 'A' glyph in the first system font");
            return;
        };
        let mut raster = GlyphRasterizer::new();
        let image = raster
            .rasterize(&font, u32::from(glyph), 16.0, &GlyphVariation::default())
            .expect("'A' must rasterize");

        assert!(image.placement.width > 0 && image.placement.height > 0);
        assert_eq!(
            image.data.len() as u32,
            image.placement.width * image.placement.height,
            "R8 stride == width"
        );
        assert!(
            image.data.iter().any(|byte| *byte != 0),
            "glyph coverage must not be all zero"
        );
    }

    #[test]
    fn raster_is_deterministic_for_the_same_key() {
        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let Some(glyph) = first_glyph(&font, 'A') else {
            eprintln!("skip: no 'A' glyph");
            return;
        };
        let mut raster = GlyphRasterizer::new();
        let first = raster
            .rasterize(&font, u32::from(glyph), 16.0, &GlyphVariation::default())
            .expect("first");
        let second = raster
            .rasterize(&font, u32::from(glyph), 16.0, &GlyphVariation::default())
            .expect("second");

        assert_eq!(first.data, second.data);
        assert_eq!(
            (
                first.placement.left,
                first.placement.top,
                first.placement.width,
                first.placement.height
            ),
            (
                second.placement.left,
                second.placement.top,
                second.placement.width,
                second.placement.height
            )
        );
        assert_eq!(raster.cached_fonts(), 1, "the font blob is pinned once");
    }

    #[test]
    fn raster_ppem_changes_the_bitmap() {
        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let Some(glyph) = first_glyph(&font, 'A') else {
            eprintln!("skip: no 'A' glyph");
            return;
        };
        let mut raster = GlyphRasterizer::new();
        let small = raster
            .rasterize(&font, u32::from(glyph), 13.0, &GlyphVariation::default())
            .expect("13ppem");
        let large = raster
            .rasterize(&font, u32::from(glyph), 26.0, &GlyphVariation::default())
            .expect("26ppem");

        assert!(
            large.placement.width > small.placement.width,
            "26ppem must be wider: {} vs {}",
            large.placement.width,
            small.placement.width
        );
    }

    #[test]
    fn raster_rejects_absurd_inputs() {
        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut raster = GlyphRasterizer::new();

        assert!(
            raster
                .rasterize(&font, 0, 0.0, &GlyphVariation::default())
                .is_none(),
            "ppem 0 is rejected"
        );
        let _ = raster.rasterize(&font, u32::from(u16::MAX), 16.0, &GlyphVariation::default());
    }

    #[test]
    fn font_epoch_invalidates_the_pinned_fonts() {
        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut raster = GlyphRasterizer::new();
        let _ = raster.rasterize(&font, 1, 16.0, &GlyphVariation::default());
        assert!(raster.cached_fonts() <= 1);

        raster.set_font_epoch(raster.font_epoch() + 1);
        assert_eq!(
            raster.cached_fonts(),
            0,
            "epoch bump drops all pinned fonts"
        );
    }

    fn first_glyph(font: &parley::FontData, ch: char) -> Option<u16> {
        let swash_font = swash::FontRef::from_index(font.data.data(), font.index as usize)?;
        let glyph = swash_font.charmap().map(ch);
        (glyph != 0).then_some(glyph)
    }
}
