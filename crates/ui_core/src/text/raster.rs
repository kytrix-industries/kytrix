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
        let mut scaler = ctx.builder(swash_font).size(ppem).hint(true).build();
        let image = Render::new(&[Source::Outline])
            .format(Format::Alpha)
            .render(&mut scaler, glyph as u16)?;
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
    use super::{GlyphRasterizer, probe_font};

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
            .rasterize(&font, u32::from(glyph), 16.0)
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
            .rasterize(&font, u32::from(glyph), 16.0)
            .expect("first");
        let second = raster
            .rasterize(&font, u32::from(glyph), 16.0)
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
            .rasterize(&font, u32::from(glyph), 13.0)
            .expect("13ppem");
        let large = raster
            .rasterize(&font, u32::from(glyph), 26.0)
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
            raster.rasterize(&font, 0, 0.0).is_none(),
            "ppem 0 is rejected"
        );
        let _ = raster.rasterize(&font, u32::from(u16::MAX), 16.0);
    }

    #[test]
    fn font_epoch_invalidates_the_pinned_fonts() {
        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut raster = GlyphRasterizer::new();
        let _ = raster.rasterize(&font, 1, 16.0);
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
