use swash::{
    scale::{Render, ScaleContext, Source, StrikeWith, image::Content},
    zeno::{Format, Placement, Vector},
};

fn bgra_to_coverage(data: &[u8]) -> Vec<u8> {
    let (pixels, remainder) = data.as_chunks::<4>();
    if !remainder.is_empty() {
        return data.to_vec();
    }
    pixels.iter().map(|pixel| pixel[3]).collect()
}

pub(crate) const SUBPIXEL_PHASES: u8 = 4;

pub(crate) const HINTING: bool = false;

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

#[derive(Clone, Copy)]
struct SwashFont {
    offset: u32,
    key: swash::CacheKey,
}

struct FontEntry {
    font: parley::FontData,
    swash: Option<SwashFont>,
}

impl FontEntry {
    fn swash_font(&self) -> Option<swash::FontRef<'_>> {
        let swash = self.swash?;
        Some(swash::FontRef {
            data: self.font.data.data(),
            offset: swash.offset,
            key: swash.key,
        })
    }
}

pub(crate) struct RasterGlyph {
    pub placement: Placement,
    pub data: Vec<u8>,
    pub color: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct GlyphVariation {
    coords: Vec<i16>,
    embolden: f32,
    hash: u64,
}

impl GlyphVariation {
    pub(crate) fn of(coords: &[i16], synthesis: fontique::Synthesis, ppem: f32) -> Self {
        use std::hash::{Hash, Hasher};
        let embolden = if synthesis.embolden() {
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

    pub(crate) fn hash(&self) -> u64 {
        self.hash
    }
}

pub(crate) fn split_subpixel_x(x: f32) -> (f32, u8) {
    if !x.is_finite() {
        return (0.0, 0);
    }
    let phase_count = f32::from(SUBPIXEL_PHASES);
    let mut base = x.floor();
    let phase = ((x - base) * phase_count).round();
    if phase >= phase_count {
        base += 1.0;
        (base, 0)
    } else {
        (base, phase as u8)
    }
}

pub(crate) struct GlyphRasterizer {
    ctx: ScaleContext,
    fonts: ahash::HashMap<FontKey, FontEntry>,
    font_epoch: u32,
}

impl GlyphRasterizer {
    pub(crate) fn new() -> Self {
        Self {
            ctx: ScaleContext::with_max_entries(16),
            fonts: ahash::HashMap::default(),
            font_epoch: 1,
        }
    }

    pub(crate) fn font_epoch(&self) -> u32 {
        self.font_epoch
    }

    pub(crate) fn set_font_epoch(&mut self, epoch: u32) {
        if self.font_epoch != epoch {
            self.font_epoch = epoch;
            self.fonts.clear();
        }
    }

    pub(crate) fn rasterize(
        &mut self,
        font: &parley::FontData,
        glyph: u32,
        ppem: f32,
        subpixel_x: u8,
        variation: &GlyphVariation,
    ) -> Option<RasterGlyph> {
        debug_assert!(
            subpixel_x < SUBPIXEL_PHASES,
            "subpixel bucket {subpixel_x} must be below SUBPIXEL_PHASES"
        );
        if ppem <= 0.0 || !ppem.is_finite() || glyph > u32::from(u16::MAX) {
            return None;
        }
        let key = FontKey::of(font, self.font_epoch);
        let Self { ctx, fonts, .. } = self;
        let entry = fonts.entry(key).or_insert_with(|| FontEntry {
            font: font.clone(),
            swash: swash::FontRef::from_index(font.data.data(), font.index as usize).map(|font| {
                SwashFont {
                    offset: font.offset,
                    key: font.key,
                }
            }),
        });
        let swash_font = entry.swash_font()?;
        let builder = ctx
            .builder(swash_font)
            .size(ppem)
            .hint(HINTING)
            .normalized_coords(variation.coords.iter());
        let mut scaler = builder.build();
        let mut render = Render::new(&[
            Source::ColorOutline(0),
            Source::ColorBitmap(StrikeWith::BestFit),
            Source::Outline,
        ]);
        render.format(Format::Alpha);
        if subpixel_x != 0 {
            render.offset(Vector::new(
                f32::from(subpixel_x) / f32::from(SUBPIXEL_PHASES),
                0.0,
            ));
        }
        if variation.embolden > 0.0 {
            render.embolden(variation.embolden);
        }
        let image = render.render(&mut scaler, glyph as u16)?;
        let color = image.content == Content::Color;
        let data = if color {
            bgra_to_coverage(&image.data)
        } else {
            image.data
        };
        Some(RasterGlyph {
            placement: image.placement,
            data,
            color,
        })
    }

    #[cfg(test)]
    pub(crate) fn cached_fonts(&self) -> usize {
        self.fonts.len()
    }

    #[cfg(test)]
    pub(crate) fn swash_cache_key(&self, font: &parley::FontData) -> Option<u64> {
        let key = FontKey::of(font, self.font_epoch);
        self.fonts
            .get(&key)
            .and_then(FontEntry::swash_font)
            .map(|font| font.key.value())
    }
}

impl Default for GlyphRasterizer {
    fn default() -> Self {
        Self::new()
    }
}

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
pub(crate) fn probe_variable_font() -> Option<parley::FontData> {
    use std::borrow::Cow;

    use crate::paint::color::ColorBrush;

    let mut font_cx = parley::FontContext::new();
    let mut layout_cx: parley::LayoutContext<ColorBrush> = parley::LayoutContext::new();
    let mut builder = layout_cx.ranged_builder(&mut font_cx, "A", 1.0, true);
    let families = [
        "Segoe UI Variable Text",
        "Segoe UI",
        "Noto Sans",
        "DejaVu Sans",
    ];
    let family_list: Vec<parley::FontFamilyName<'static>> = families
        .iter()
        .map(|name| parley::FontFamilyName::Named(Cow::Owned((*name).to_owned())))
        .chain([parley::FontFamilyName::Generic(
            parley::GenericFamily::SansSerif,
        )])
        .collect();
    builder.push_default(parley::StyleProperty::FontFamily(parley::FontFamily::List(
        Cow::Owned(family_list),
    )));
    builder.push_default(parley::StyleProperty::FontSize(16.0));
    let mut layout = builder.build("A");
    layout.break_all_lines(None);
    let font = layout.lines().next()?.items().find_map(|item| match item {
        parley::PositionedLayoutItem::GlyphRun(run) => Some(run.run().font().clone()),
        parley::PositionedLayoutItem::InlineBox(_) => None,
    })?;
    let variable = swash::FontRef::from_index(font.data.data(), font.index as usize)
        .is_some_and(|font| font.variations().next().is_some());
    variable.then_some(font)
}

#[cfg(test)]
mod tests {
    use super::{
        GlyphRasterizer, GlyphVariation, probe_font, probe_variable_font, split_subpixel_x,
    };

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
    fn regular_raster_after_bold_does_not_inherit_variation() {
        let Some(font) = probe_variable_font() else {
            eprintln!("skip: no variable system font available");
            return;
        };
        let Some(glyph) = first_glyph(&font, 'b') else {
            eprintln!("skip: no 'b' glyph in the first system font");
            return;
        };
        let bold = GlyphVariation::of(&[16_384], Default::default(), 17.5);
        let regular = GlyphVariation::default();
        let mut raster = GlyphRasterizer::new();
        let bold_image = raster
            .rasterize(&font, u32::from(glyph), 17.5, 0, &bold)
            .expect("bold glyph must rasterize");
        let after_bold = raster
            .rasterize(&font, u32::from(glyph), 17.5, 0, &regular)
            .expect("regular glyph after bold must rasterize");
        let mut fresh = GlyphRasterizer::new();
        let reference = fresh
            .rasterize(&font, u32::from(glyph), 17.5, 0, &regular)
            .expect("fresh regular glyph must rasterize");

        assert_eq!(
            (
                after_bold.placement.left,
                after_bold.placement.top,
                after_bold.placement.width,
                after_bold.placement.height
            ),
            (
                reference.placement.left,
                reference.placement.top,
                reference.placement.width,
                reference.placement.height
            ),
            "regular glyph must not inherit the previous bold instance"
        );
        assert_eq!(
            after_bold.data, reference.data,
            "regular glyph coverage must match a fresh rasterizer"
        );
        assert_ne!(
            bold_image.data, reference.data,
            "the bold instance must differ from regular coverage"
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
            .rasterize(&font, u32::from(glyph), 16.0, 0, &GlyphVariation::default())
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
    fn subpixel_phases_quantize_to_the_nearest_quarter_pixel() {
        assert_eq!(split_subpixel_x(10.0), (10.0, 0));
        assert_eq!(
            split_subpixel_x(10.1),
            (10.0, 0),
            "0.1 px 与整像素必须同档（相位误差 < 1/8 px）"
        );
        assert_eq!(split_subpixel_x(10.2), (10.0, 1));
        assert_eq!(
            split_subpixel_x(10.4),
            (10.0, 2),
            "0.4 px 必须换到 1/2 px 档"
        );
        assert_eq!(
            split_subpixel_x(10.9),
            (11.0, 0),
            "0.9 px 四舍五入时进位到下一个整数档（桶数恒为 4，不会出现第 5 档）"
        );
        assert_eq!(
            split_subpixel_x(-10.2),
            (-11.0, 3),
            "负坐标同样按 floor + 相位处理（小数部分 0.8 → 3/4 档）"
        );
        assert_ne!(
            split_subpixel_x(10.0).1,
            split_subpixel_x(10.4).1,
            "x=10.0 与 x=10.4 必须落在不同相位（图集键因此不同）"
        );
        assert_eq!(
            split_subpixel_x(10.0).1,
            split_subpixel_x(10.1).1,
            "x=10.0 与 x=10.1 必须落在同一相位（共享同一个 tile）"
        );
    }

    #[test]
    fn split_subpixel_x_is_bounded_for_non_finite_inputs() {
        for x in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let (base, phase) = split_subpixel_x(x);
            assert!(base.is_finite(), "{x}: base must stay finite, got {base}");
            assert!(
                phase < super::SUBPIXEL_PHASES,
                "{x}: phase {phase} out of range"
            );
            assert_eq!(
                (base, phase),
                (0.0, 0),
                "{x}: non-finite inputs fall back to the bounded integer origin"
            );
        }
        let (base, phase) = split_subpixel_x(f32::MAX);
        assert!(base.is_finite(), "f32::MAX must stay finite: {base}");
        assert!(phase < super::SUBPIXEL_PHASES);
    }

    #[test]
    fn raster_subpixel_phase_changes_the_bitmap() {
        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let Some(glyph) = first_glyph(&font, 'R') else {
            eprintln!("skip: no 'R' glyph in the first system font");
            return;
        };
        let mut raster = GlyphRasterizer::new();
        let variation = GlyphVariation::default();
        let integer = raster
            .rasterize(&font, u32::from(glyph), 16.0, 0, &variation)
            .expect("integer phase");
        let half = raster
            .rasterize(&font, u32::from(glyph), 16.0, 2, &variation)
            .expect("half pixel phase");

        assert_ne!(
            (
                integer.placement.left,
                integer.placement.width,
                &integer.data
            ),
            (half.placement.left, half.placement.width, &half.data),
            "1/2 px 相位必须改变覆盖度或位图落点"
        );
        assert_eq!(
            (integer.placement.top, integer.placement.height),
            (half.placement.top, half.placement.height),
            "水平相位不应改变竖直落点与高度"
        );
    }

    #[test]
    fn raster_without_hinting_keeps_a_valid_bitmap() {
        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let Some(glyph) = first_glyph(&font, 'A') else {
            eprintln!("skip: no 'A' glyph in the first system font");
            return;
        };
        let mut raster = GlyphRasterizer::new();
        for ppem in [11.0f32, 16.0, 32.0] {
            let image = raster
                .rasterize(&font, u32::from(glyph), ppem, 0, &GlyphVariation::default())
                .unwrap_or_else(|| panic!("{ppem} ppem must rasterize with hinting off"));
            assert!(
                image.placement.width > 0 && image.placement.height > 0,
                "{ppem} ppem: glyph bitmap must not be empty"
            );
            assert_eq!(
                image.data.len() as u32,
                image.placement.width * image.placement.height,
                "{ppem} ppem: R8 stride == width"
            );
            assert!(
                image.data.iter().any(|byte| *byte != 0),
                "{ppem} ppem: coverage must not be all zero"
            );
        }
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
            .rasterize(&font, u32::from(glyph), 16.0, 0, &GlyphVariation::default())
            .expect("first");
        let second = raster
            .rasterize(&font, u32::from(glyph), 16.0, 0, &GlyphVariation::default())
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
            .rasterize(&font, u32::from(glyph), 13.0, 0, &GlyphVariation::default())
            .expect("13ppem");
        let large = raster
            .rasterize(&font, u32::from(glyph), 26.0, 0, &GlyphVariation::default())
            .expect("26ppem");

        assert!(
            large.placement.width > small.placement.width,
            "26ppem must be wider: {} vs {}",
            large.placement.width,
            small.placement.width
        );
    }

    #[test]
    fn raster_fractional_ppem_is_not_rounded() {
        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let Some(glyph) = first_glyph(&font, 'A') else {
            eprintln!("skip: no 'A' glyph");
            return;
        };
        let mut raster = GlyphRasterizer::new();
        for ppem in [17.5f32, 13.25, 18.75] {
            let image = raster
                .rasterize(&font, u32::from(glyph), ppem, 0, &GlyphVariation::default())
                .unwrap_or_else(|| panic!("{ppem} fractional ppem must rasterize"));
            assert!(
                image.placement.width > 0 && image.placement.height > 0,
                "{ppem} ppem: glyph bitmap must not be empty"
            );
            assert_eq!(
                image.data.len() as u32,
                image.placement.width * image.placement.height,
                "{ppem} ppem: R8 stride == width"
            );
            assert!(
                image.data.iter().any(|byte| *byte != 0),
                "{ppem} ppem: coverage must not be all zero"
            );
        }

        let half = raster
            .rasterize(&font, u32::from(glyph), 17.5, 0, &GlyphVariation::default())
            .expect("17.5 ppem");
        let eighteen = raster
            .rasterize(&font, u32::from(glyph), 18.0, 0, &GlyphVariation::default())
            .expect("18 ppem");
        let placement_differs = (
            half.placement.left,
            half.placement.top,
            half.placement.width,
            half.placement.height,
        ) != (
            eighteen.placement.left,
            eighteen.placement.top,
            eighteen.placement.width,
            eighteen.placement.height,
        );
        assert!(
            placement_differs || half.data != eighteen.data,
            "17.5 ppem must raster as f32, not be rounded to 18 ppem"
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
                .rasterize(&font, 0, 0.0, 0, &GlyphVariation::default())
                .is_none(),
            "ppem 0 is rejected"
        );
        for ppem in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(
                raster
                    .rasterize(&font, 0, ppem, 0, &GlyphVariation::default())
                    .is_none(),
                "non-finite ppem {ppem} is rejected"
            );
        }
        let _ = raster.rasterize(
            &font,
            u32::from(u16::MAX),
            16.0,
            0,
            &GlyphVariation::default(),
        );
    }

    #[test]
    fn font_epoch_invalidates_the_pinned_fonts() {
        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut raster = GlyphRasterizer::new();
        let _ = raster.rasterize(&font, 1, 16.0, 0, &GlyphVariation::default());
        assert!(raster.cached_fonts() <= 1);

        raster.set_font_epoch(raster.font_epoch() + 1);
        assert_eq!(
            raster.cached_fonts(),
            0,
            "epoch bump drops all pinned fonts"
        );
    }

    #[test]
    fn font_ref_key_is_stable_across_rasters() {
        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let Some(glyph) = first_glyph(&font, 'A') else {
            eprintln!("skip: no 'A' glyph in the first system font");
            return;
        };
        let mut raster = GlyphRasterizer::new();
        let logical = GlyphVariation::default();
        raster
            .rasterize(&font, u32::from(glyph), 16.0, 0, &logical)
            .expect("first rasterize");
        let first = raster
            .swash_cache_key(&font)
            .expect("font must be pinned after rasterize");
        raster
            .rasterize(&font, u32::from(glyph), 18.0, 2, &logical)
            .expect("second rasterize");
        let second = raster
            .swash_cache_key(&font)
            .expect("font must still be pinned");

        assert_eq!(
            first, second,
            "同一字体的所有字形必须共享一个 swash CacheKey（否则 FontCache 恒 miss）"
        );
        let fresh = swash::FontRef::from_index(font.data.data(), font.index as usize)
            .expect("probe font must be a valid swash font")
            .key
            .value();
        assert_ne!(
            fresh, second,
            "from_index 每次生成新 key —— 这正是修复前每字形重建 ScalerProxy 的根因"
        );
    }

    #[test]
    fn font_epoch_changes_the_swash_key() {
        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let Some(glyph) = first_glyph(&font, 'A') else {
            eprintln!("skip: no 'A' glyph in the first system font");
            return;
        };
        let mut raster = GlyphRasterizer::new();
        raster
            .rasterize(&font, u32::from(glyph), 16.0, 0, &GlyphVariation::default())
            .expect("first rasterize");
        let before = raster.swash_cache_key(&font).expect("pinned");

        let next_epoch = raster.font_epoch() + 1;
        raster.set_font_epoch(next_epoch);
        raster
            .rasterize(&font, u32::from(glyph), 16.0, 0, &GlyphVariation::default())
            .expect("rasterize after epoch bump");
        let after = raster.swash_cache_key(&font).expect("pinned again");

        assert_ne!(
            before, after,
            "字体换代后必须使用新的 swash CacheKey，避免跨字体复用 ScalerProxy"
        );
    }

    fn first_glyph(font: &parley::FontData, ch: char) -> Option<u16> {
        let swash_font = swash::FontRef::from_index(font.data.data(), font.index as usize)?;
        let glyph = swash_font.charmap().map(ch);
        (glyph != 0).then_some(glyph)
    }
}
