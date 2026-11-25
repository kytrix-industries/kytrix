use swash::{
    scale::{Render, ScaleContext, Source, StrikeWith, image::Content},
    zeno::{Format, Placement, Vector},
};

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
    color_glyphs: ColorGlyphCoverage,
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
    pub color_monochrome: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ColorGlyphCoverage {
    colr: Option<ColrCoverage>,
}

impl ColorGlyphCoverage {
    fn of(data: &[u8], font_offset: u32) -> Self {
        Self {
            colr: ColrCoverage::of(data, font_offset),
        }
    }

    fn declares_color(&self, glyph: u32) -> bool {
        self.colr.as_ref().is_some_and(|colr| colr.has_paint(glyph))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ColrCoverage {
    v0: Box<[u32]>,
    v1: Box<[u32]>,
}

impl ColrCoverage {
    fn of(data: &[u8], font_offset: u32) -> Option<Self> {
        let base = usize::try_from(font_offset).ok()?;
        let (table, table_len) = sfnt_table(data, base, *b"COLR")?;
        let header = slice(data, table, 0, 18)?;
        let version = u16::from_be_bytes([header[0], header[1]]);
        let v0_count = usize::from(u16::from_be_bytes([header[2], header[3]]));
        let v0_offset = usize::try_from(u32::from_be_bytes([
            header[4], header[5], header[6], header[7],
        ]))
        .ok()?;
        let v0 = (0..v0_count)
            .filter_map(|index| {
                let record = table.checked_add(v0_offset)?.checked_add(index * 6)?;
                let bytes = data.get(record..record.checked_add(2)?)?;
                Some(u32::from(u16::from_be_bytes([bytes[0], bytes[1]])))
            })
            .collect::<Vec<_>>();
        if version < 1 {
            return Some(Self {
                v0: v0.into_boxed_slice(),
                v1: Box::default(),
            });
        }
        let v1_offset = usize::try_from(u32::from_be_bytes([
            header[14], header[15], header[16], header[17],
        ]))
        .ok()?;
        let v1 = if v1_offset == 0 {
            Vec::new()
        } else {
            let records = v1_offset.checked_add(4)?;
            let declared = read_u32(data, table.checked_add(v1_offset)?).unwrap_or(0);
            let tabular = table_len.saturating_sub(records) / 6;
            let in_data = data.len().saturating_sub(table.checked_add(records)?) / 6;
            let count = usize::try_from(declared)
                .unwrap_or(usize::MAX)
                .min(tabular)
                .min(in_data);
            (0..count)
                .filter_map(|index| {
                    let record = table
                        .checked_add(records)?
                        .checked_add(index.checked_mul(6)?)?;
                    Some(u32::from(read_u16(data, record)?))
                })
                .collect::<Vec<_>>()
        };
        Some(Self {
            v0: v0.into_boxed_slice(),
            v1: v1.into_boxed_slice(),
        })
    }

    fn has_paint(&self, glyph: u32) -> bool {
        self.v0.binary_search(&glyph).is_ok() || self.v1.binary_search(&glyph).is_ok()
    }
}

fn slice(data: &[u8], base: usize, relative: usize, len: usize) -> Option<&[u8]> {
    let start = base.checked_add(relative)?;
    let end = start.checked_add(len)?;
    data.get(start..end)
}

fn read_u16(data: &[u8], offset: usize) -> Option<u16> {
    let bytes = data.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn sfnt_table(data: &[u8], font_offset: usize, tag: [u8; 4]) -> Option<(usize, usize)> {
    let directory = slice(data, font_offset, 0, 12)?;
    let num_tables = usize::from(u16::from_be_bytes([directory[4], directory[5]]));
    for index in 0..num_tables.min(256) {
        let record = slice(data, font_offset, 12 + index * 16, 16)?;
        if record[0..4] != tag {
            continue;
        }
        let offset = u32::from_be_bytes([record[8], record[9], record[10], record[11]]);
        let length = u32::from_be_bytes([record[12], record[13], record[14], record[15]]);
        return Some((usize::try_from(offset).ok()?, usize::try_from(length).ok()?));
    }
    None
}

fn color_pixels(data: &[u8], premultiplied: bool) -> Vec<u8> {
    let (pixels, remainder) = data.as_chunks::<4>();
    if !remainder.is_empty() {
        return data.to_vec();
    }
    if !premultiplied {
        return data.to_vec();
    }
    let mut out = Vec::with_capacity(data.len());
    for pixel in pixels {
        let alpha = u32::from(pixel[3]);
        if alpha == 0 {
            out.extend_from_slice(&[0, 0, 0, 0]);
            continue;
        }
        let unpremultiply = |channel: u8| -> u8 {
            let value = u32::from(channel) * 255;
            u8::try_from((value + alpha / 2) / alpha).unwrap_or(u8::MAX)
        };
        out.extend_from_slice(&[
            unpremultiply(pixel[0]),
            unpremultiply(pixel[1]),
            unpremultiply(pixel[2]),
            pixel[3],
        ]);
    }
    out
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
        let entry = fonts.entry(key).or_insert_with(|| {
            let swash =
                swash::FontRef::from_index(font.data.data(), font.index as usize).map(|font| {
                    SwashFont {
                        offset: font.offset,
                        key: font.key,
                    }
                });
            let color_glyphs = swash.map_or_else(ColorGlyphCoverage::default, |swash| {
                ColorGlyphCoverage::of(font.data.data(), swash.offset)
            });
            FontEntry {
                font: font.clone(),
                swash,
                color_glyphs,
            }
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
        let color_monochrome = !color && entry.color_glyphs.declares_color(glyph);
        let data = if color {
            let premultiplied = matches!(image.source, Source::ColorOutline(_));
            color_pixels(&image.data, premultiplied)
        } else {
            image.data
        };
        Some(RasterGlyph {
            placement: image.placement,
            data,
            color,
            color_monochrome,
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
pub(crate) fn probe_emoji_font() -> Option<parley::FontData> {
    use crate::paint::color::ColorBrush;

    let mut font_cx = parley::FontContext::new();
    let mut layout_cx: parley::LayoutContext<ColorBrush> = parley::LayoutContext::new();
    let mut builder = layout_cx.ranged_builder(&mut font_cx, "\u{1F30D}", 1.0, true);
    builder.push_default(parley::StyleProperty::FontSize(32.0));
    let mut layout = builder.build("\u{1F30D}");
    layout.break_all_lines(None);
    let font = layout.lines().next()?.items().find_map(|item| match item {
        parley::PositionedLayoutItem::GlyphRun(run) => Some(run.run().font().clone()),
        parley::PositionedLayoutItem::InlineBox(_) => None,
    })?;
    let mut raster = GlyphRasterizer::new();
    let glyph = u32::from(
        swash::FontRef::from_index(font.data.data(), font.index as usize)?
            .charmap()
            .map('\u{1F30D}'),
    );
    let image = raster.rasterize(&font, glyph, 32.0, 0, &GlyphVariation::default())?;
    image.color.then_some(font)
}

#[cfg(test)]
pub(crate) fn color_glyph_monochrome_fixture() -> Result<(parley::FontData, u32), String> {
    let (base, glyph) = single_face_font_base()?;
    let fabricated = append_colr_v1_paint(&base, glyph);
    Ok((
        parley::FontData::new(parley::fontique::Blob::from(fabricated), 0),
        u32::from(glyph),
    ))
}

#[cfg(test)]
fn single_face_font_base() -> Result<(Vec<u8>, u16), String> {
    let mut attempted = Vec::new();
    for path in single_face_font_candidates() {
        attempted.push(path.display().to_string());
        let Ok(data) = std::fs::read(&path) else {
            continue;
        };
        let Some(font) = swash::FontRef::from_index(&data, 0) else {
            continue;
        };
        if font.offset != 0 || sfnt_table(&data, 0, *b"COLR").is_some() {
            continue;
        }
        let glyph = font.charmap().map('A');
        if glyph != 0 {
            return Ok((data, glyph));
        }
    }
    Err(format!(
        "no single-face TTF/OTF font without a COLR table is available on this host; tried: [{}]",
        attempted.join(", ")
    ))
}

#[cfg(test)]
fn single_face_font_candidates() -> Vec<std::path::PathBuf> {
    use std::path::{Path, PathBuf};

    const SCAN_DEPTH: usize = 3;
    let mut candidates = Vec::new();
    if cfg!(windows) {
        candidates.extend(
            [
                "segoeui.ttf",
                "arial.ttf",
                "calibri.ttf",
                "tahoma.ttf",
                "verdana.ttf",
            ]
            .map(|name| Path::new(r"C:\Windows\Fonts").join(name)),
        );
    } else if cfg!(target_os = "macos") {
        candidates.extend(
            [
                "/System/Library/Fonts/Supplemental/Arial.ttf",
                "/System/Library/Fonts/Supplemental/Verdana.ttf",
                "/System/Library/Fonts/Helvetica.ttc",
                "/System/Library/Fonts/SFNSMono.ttf",
            ]
            .map(PathBuf::from),
        );
        collect_font_files(Path::new("/Library/Fonts"), 0, &mut candidates);
        collect_font_files(
            Path::new("/System/Library/Fonts"),
            SCAN_DEPTH,
            &mut candidates,
        );
    } else {
        candidates.extend(
            [
                "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
                "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
                "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
            ]
            .map(PathBuf::from),
        );
        collect_font_files(Path::new("/usr/share/fonts"), SCAN_DEPTH, &mut candidates);
    }
    candidates
}

#[cfg(test)]
fn collect_font_files(dir: &std::path::Path, depth: usize, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files = Vec::new();
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if depth > 0 {
                subdirs.push(path);
            }
        } else if matches!(
            path.extension().and_then(std::ffi::OsStr::to_str),
            Some("ttf" | "otf")
        ) {
            files.push(path);
        }
    }
    files.sort();
    subdirs.sort();
    out.extend(files);
    for subdir in &subdirs {
        collect_font_files(subdir, depth - 1, out);
    }
}

#[cfg(test)]
pub(crate) fn gpu_pixels_exempt() -> bool {
    std::env::var("KYTRIX_TEST_SKIP_GPU_PIXELS").is_ok_and(|value| value == "1")
}

#[cfg(test)]
pub(crate) fn require_color_glyph_monochrome_fixture() -> Option<(parley::FontData, u32)> {
    match color_glyph_monochrome_fixture() {
        Ok(fixture) => Some(fixture),
        Err(reason) => {
            assert!(
                gpu_pixels_exempt(),
                "无法构造 COLRv1-only 测试字体：{reason}；设置 KYTRIX_TEST_SKIP_GPU_PIXELS=1 可显式豁免"
            );
            eprintln!(
                "skip COLRv1-only fixture: {reason} (explicitly exempted by KYTRIX_TEST_SKIP_GPU_PIXELS=1)"
            );
            None
        }
    }
}

#[cfg(test)]
fn append_colr_v1_paint(data: &[u8], glyph: u16) -> Vec<u8> {
    let num_tables = usize::from(u16::from_be_bytes([data[4], data[5]]));
    let directory_end = 12 + num_tables * 16;
    let mut records: Vec<[u8; 16]> = (0..num_tables)
        .map(|index| {
            let mut record = [0u8; 16];
            record.copy_from_slice(&data[12 + index * 16..12 + index * 16 + 16]);
            let offset = u32::from_be_bytes([record[8], record[9], record[10], record[11]]) + 16;
            record[8..12].copy_from_slice(&offset.to_be_bytes());
            record
        })
        .collect();
    let mut colr = Vec::new();
    colr.extend_from_slice(&1u16.to_be_bytes());
    colr.extend_from_slice(&0u16.to_be_bytes());
    colr.extend_from_slice(&0u32.to_be_bytes());
    colr.extend_from_slice(&0u32.to_be_bytes());
    colr.extend_from_slice(&0u16.to_be_bytes());
    colr.extend_from_slice(&34u32.to_be_bytes());
    for _ in 0..4 {
        colr.extend_from_slice(&0u32.to_be_bytes());
    }
    colr.extend_from_slice(&1u32.to_be_bytes());
    colr.extend_from_slice(&glyph.to_be_bytes());
    colr.extend_from_slice(&0u32.to_be_bytes());
    let shifted_end = data.len() + 16;
    let colr_offset = (shifted_end + 3) & !3;
    let mut record = [0u8; 16];
    record[0..4].copy_from_slice(b"COLR");
    record[8..12].copy_from_slice(&(colr_offset as u32).to_be_bytes());
    record[12..16].copy_from_slice(&(colr.len() as u32).to_be_bytes());
    records.push(record);
    records.sort_by_key(|record| [record[0], record[1], record[2], record[3]]);

    let mut font = Vec::with_capacity(colr_offset + colr.len());
    font.extend_from_slice(&data[0..12]);
    for record in &records {
        font.extend_from_slice(record);
    }
    font.extend_from_slice(&data[directory_end..]);
    font.resize(colr_offset, 0);
    font.extend_from_slice(&colr);
    let new_count = (num_tables + 1) as u16;
    font[4..6].copy_from_slice(&new_count.to_be_bytes());
    let power = (new_count as usize).next_power_of_two() >> 1;
    let search_range = (power * 16) as u16;
    let entry_selector = power.trailing_zeros() as u16;
    let range_shift = (new_count * 16).saturating_sub(search_range);
    font[6..8].copy_from_slice(&search_range.to_be_bytes());
    font[8..10].copy_from_slice(&entry_selector.to_be_bytes());
    font[10..12].copy_from_slice(&range_shift.to_be_bytes());
    font
}

#[cfg(test)]
mod tests {
    use super::{
        ColorGlyphCoverage, ColrCoverage, GlyphRasterizer, GlyphVariation, color_pixels,
        probe_emoji_font, probe_font, probe_variable_font, require_color_glyph_monochrome_fixture,
        split_subpixel_x,
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

    #[test]
    fn color_pixels_unpremultiply_the_colr_output_to_straight_rgba() {
        let premultiplied = [0u8, 171, 132, 250, 88, 59, 11, 93, 0, 0, 0, 0];
        let straight = color_pixels(&premultiplied, true);
        assert_eq!(straight.len(), premultiplied.len());
        assert_eq!(
            &straight[0..4],
            &[0, 174, 135, 250],
            "COLR 输出是 sRGB 空间预乘，必须反预乘成 straight"
        );
        assert_eq!(&straight[4..8], &[241, 162, 30, 93]);
        assert_eq!(&straight[8..12], &[0, 0, 0, 0], "alpha=0 的像素不得除零");
        for pixel in straight.as_chunks::<4>().0 {
            assert!(pixel[3] == 0 || pixel[..3].iter().any(|channel| *channel != 0));
        }
    }

    #[test]
    fn color_pixels_pass_through_the_straight_bitmap_path() {
        let straight = [12u8, 34, 56, 200, 0, 0, 0, 255];
        assert_eq!(
            color_pixels(&straight, false),
            straight.to_vec(),
            "sbix/CBDT PNG 解码已是 straight RGBA，不得再次反预乘"
        );
        assert_eq!(
            color_pixels(&straight[..7], false).len(),
            7,
            "非 4 字节对齐的载荷原样返回，不构造半像素"
        );
    }

    #[test]
    fn colr_coverage_reads_v0_and_v1_base_glyph_lists() {
        let mut font = vec![0u8; 28];
        font[0..4].copy_from_slice(&[0x00, 0x01, 0x00, 0x00]);
        font[4..6].copy_from_slice(&1u16.to_be_bytes());
        font[12..16].copy_from_slice(b"COLR");
        font[20..24].copy_from_slice(&28u32.to_be_bytes());
        let colr_len = 34 + 2 * 6 + 4 + 2 * 6;
        font[24..28].copy_from_slice(&(colr_len as u32).to_be_bytes());
        let colr = font.len();
        font.extend_from_slice(&1u16.to_be_bytes());
        font.extend_from_slice(&2u16.to_be_bytes());
        font.extend_from_slice(&34u32.to_be_bytes());
        font.extend_from_slice(&0u32.to_be_bytes());
        font.extend_from_slice(&0u16.to_be_bytes());
        font.extend_from_slice(&46u32.to_be_bytes());
        font.extend_from_slice(&0u32.to_be_bytes());
        font.extend_from_slice(&0u32.to_be_bytes());
        font.extend_from_slice(&0u32.to_be_bytes());
        font.extend_from_slice(&0u32.to_be_bytes());
        assert_eq!(font.len() - colr, 34, "COLR v0 头正好 34 字节");
        for glyph in [10u16, 20] {
            font.extend_from_slice(&glyph.to_be_bytes());
            font.extend_from_slice(&0u16.to_be_bytes());
            font.extend_from_slice(&0u16.to_be_bytes());
        }
        assert_eq!(font.len() - colr, 46);
        font.extend_from_slice(&2u32.to_be_bytes());
        for glyph in [30u16, 41] {
            font.extend_from_slice(&glyph.to_be_bytes());
            font.extend_from_slice(&0u32.to_be_bytes());
        }
        assert_eq!(
            font.len() - colr,
            colr_len,
            "COLRv1 BaseGlyphPaintRecord 是 uint16 glyphID + Offset32 = 6 B/条"
        );

        let coverage = ColrCoverage::of(&font, 0).expect("COLR 表必须可解析");
        for glyph in [10, 20, 30, 41] {
            assert!(
                coverage.has_paint(glyph),
                "glyph {glyph} 必须声明 COLR 绘制"
            );
        }
        for glyph in [0, 11, 21, 31, 42, 0x002A_0000, 0x001E_0000, u32::MAX] {
            assert!(
                !coverage.has_paint(glyph),
                "glyph {glyph} 不得声明 COLR 绘制"
            );
        }
        assert!(
            ColrCoverage::of(&font[..20], 0).is_none(),
            "截断的字体不得产生半解析结果"
        );
    }

    #[test]
    fn colr_v1_records_are_six_bytes_not_eight() {
        let record = |glyph: u16| {
            let mut bytes = Vec::with_capacity(6);
            bytes.extend_from_slice(&glyph.to_be_bytes());
            bytes.extend_from_slice(&7u32.to_be_bytes());
            bytes
        };
        let mut colr = Vec::new();
        colr.extend_from_slice(&1u16.to_be_bytes());
        colr.extend_from_slice(&0u16.to_be_bytes());
        colr.extend_from_slice(&0u32.to_be_bytes());
        colr.extend_from_slice(&0u32.to_be_bytes());
        colr.extend_from_slice(&0u16.to_be_bytes());
        colr.extend_from_slice(&34u32.to_be_bytes());
        for _ in 0..4 {
            colr.extend_from_slice(&0u32.to_be_bytes());
        }
        colr.extend_from_slice(&2u32.to_be_bytes());
        colr.extend_from_slice(&record(42));
        colr.extend_from_slice(&record(300));

        let mut font = vec![0u8; 28];
        font[0..4].copy_from_slice(&[0x00, 0x01, 0x00, 0x00]);
        font[4..6].copy_from_slice(&1u16.to_be_bytes());
        font[12..16].copy_from_slice(b"COLR");
        font[20..24].copy_from_slice(&28u32.to_be_bytes());
        font[24..28].copy_from_slice(&(colr.len() as u32).to_be_bytes());
        font.extend_from_slice(&colr);

        let coverage = ColrCoverage::of(&font, 0).expect("COLR 表必须可解析");
        assert!(coverage.has_paint(42), "42 必须在 v1 base glyph list 中");
        assert!(coverage.has_paint(300), "300 必须在 v1 base glyph list 中");
        assert!(
            !coverage.has_paint(42 << 16),
            "按 8 B 步长误读会得到 glyphID|paintOffset 拼接的伪 ID {}, 规范布局必须读 uint16",
            42 << 16
        );
        assert!(!coverage.has_paint(300 << 16), "300 的伪拼接 ID 也不得命中");
    }

    #[test]
    fn colr_v1_base_glyph_records_are_clamped_to_the_table_length() {
        let mut colr = Vec::new();
        colr.extend_from_slice(&1u16.to_be_bytes());
        colr.extend_from_slice(&0u16.to_be_bytes());
        colr.extend_from_slice(&0u32.to_be_bytes());
        colr.extend_from_slice(&0u32.to_be_bytes());
        colr.extend_from_slice(&0u16.to_be_bytes());
        colr.extend_from_slice(&34u32.to_be_bytes());
        for _ in 0..4 {
            colr.extend_from_slice(&0u32.to_be_bytes());
        }
        colr.extend_from_slice(&65535u32.to_be_bytes());
        for glyph in [42u16, 300] {
            colr.extend_from_slice(&glyph.to_be_bytes());
            colr.extend_from_slice(&7u32.to_be_bytes());
        }
        let table_len = colr.len();
        let mut font = vec![0u8; 28];
        font[0..4].copy_from_slice(&[0x00, 0x01, 0x00, 0x00]);
        font[4..6].copy_from_slice(&1u16.to_be_bytes());
        font[12..16].copy_from_slice(b"COLR");
        font[20..24].copy_from_slice(&28u32.to_be_bytes());
        font[24..28].copy_from_slice(&(table_len as u32).to_be_bytes());
        font.extend_from_slice(&colr);
        for glyph in [777u16, 888] {
            font.extend_from_slice(&glyph.to_be_bytes());
            font.extend_from_slice(&9u32.to_be_bytes());
        }

        let coverage = ColrCoverage::of(&font, 0).expect("COLR 表必须可解析");
        assert!(coverage.has_paint(42), "表内记录 42 必须命中");
        assert!(coverage.has_paint(300), "表内记录 300 必须命中");
        assert!(
            !coverage.has_paint(777) && !coverage.has_paint(888),
            "声明计数 65535 超出表长可容纳的 2 条记录：表外数据不得被当成 BaseGlyphPaintRecord 解析"
        );
        assert_eq!(
            coverage.v1.len(),
            2,
            "BaseGlyphList 必须按 COLR 表目录声明的长度截断（表长 {table_len} B，记录区仅 2 条）"
        );
    }

    #[test]
    fn colr_v1_huge_base_glyph_count_is_bounded_by_the_table() {
        let mut colr = Vec::new();
        colr.extend_from_slice(&1u16.to_be_bytes());
        colr.extend_from_slice(&0u16.to_be_bytes());
        colr.extend_from_slice(&0u32.to_be_bytes());
        colr.extend_from_slice(&0u32.to_be_bytes());
        colr.extend_from_slice(&0u16.to_be_bytes());
        colr.extend_from_slice(&34u32.to_be_bytes());
        for _ in 0..4 {
            colr.extend_from_slice(&0u32.to_be_bytes());
        }
        colr.extend_from_slice(&u32::MAX.to_be_bytes());
        colr.push(0);
        let mut font = vec![0u8; 28];
        font[0..4].copy_from_slice(&[0x00, 0x01, 0x00, 0x00]);
        font[4..6].copy_from_slice(&1u16.to_be_bytes());
        font[12..16].copy_from_slice(b"COLR");
        font[20..24].copy_from_slice(&28u32.to_be_bytes());
        font[24..28].copy_from_slice(&(colr.len() as u32).to_be_bytes());
        font.extend_from_slice(&colr);

        let coverage = ColrCoverage::of(&font, 0).expect("COLR 表必须可解析");
        assert!(
            coverage.v1.is_empty(),
            "numBaseGlyphPaintRecords = u32::MAX 且表长只容纳 0 条记录时不得解析任何 base glyph（否则会迭代 43 亿次）"
        );
        assert!(!coverage.has_paint(42));
    }

    #[test]
    fn a_colr_v1_only_glyph_falls_back_to_monochrome_with_the_flag() {
        let Some((font, glyph)) = require_color_glyph_monochrome_fixture() else {
            return;
        };
        let mut raster = GlyphRasterizer::new();
        let image = raster
            .rasterize(&font, glyph, 32.0, 0, &GlyphVariation::default())
            .expect("fabricated COLRv1-only font must rasterize the outline fallback");
        assert!(
            !image.color,
            "COLRv1-only 字形必须回退单色（swash 0.2.10 无 v1 paint graph）"
        );
        assert!(
            image.color_monochrome,
            "COLRv1-only 字形必须标记为彩色字形单色回退（驱动 ColorGlyphMonochrome 诊断）"
        );
        let coverage = ColorGlyphCoverage::of(font.data.data(), 0);
        assert!(
            coverage.declares_color(glyph),
            "注入的 COLRv1 BaseGlyphPaintRecord 必须被声明为彩色字形"
        );
    }

    #[test]
    fn color_emoji_raster_keeps_straight_alpha_above_premultiplied_alpha() {
        let Some(font) = probe_emoji_font() else {
            eprintln!("skip: no color emoji font available on this device");
            return;
        };
        let font_ref = swash::FontRef::from_index(font.data.data(), font.index as usize)
            .expect("emoji font must be a valid swash font");
        let glyph = u32::from(font_ref.charmap().map('\u{1F30D}'));
        let mut raster = GlyphRasterizer::new();
        let image = raster
            .rasterize(&font, glyph, 32.0, 0, &GlyphVariation::default())
            .expect("emoji glyph must rasterize");
        let mut straight_channels = 0usize;
        for pixel in image.data.as_chunks::<4>().0 {
            let alpha = u32::from(pixel[3]);
            if (10..250).contains(&alpha) {
                let max = u32::from(pixel[..3].iter().copied().max().unwrap_or(0));
                if max > alpha {
                    straight_channels += 1;
                }
            }
        }
        assert!(
            straight_channels > 0,
            "彩色覆盖度必须是 straight alpha：半透明像素里必须出现 rgb > alpha（实测 {straight_channels}）"
        );
    }

    #[test]
    fn colr_coverage_ignores_fonts_without_a_colr_table() {
        let mut font = vec![0u8; 12];
        font[0..4].copy_from_slice(&[0x00, 0x01, 0x00, 0x00]);
        assert!(ColrCoverage::of(&font, 0).is_none());
        assert!(ColrCoverage::of(&[], 0).is_none());
    }

    #[test]
    fn a_color_emoji_rasterizes_to_straight_rgba_with_visible_color() {
        let Some(font) = probe_emoji_font() else {
            eprintln!("skip: no color emoji font available on this device");
            return;
        };
        let font_ref = swash::FontRef::from_index(font.data.data(), font.index as usize)
            .expect("emoji font must be a valid swash font");
        let glyph = u32::from(font_ref.charmap().map('\u{1F30D}'));
        assert_ne!(glyph, 0, "emoji font must map U+1F30D");
        let mut raster = GlyphRasterizer::new();
        let image = raster
            .rasterize(&font, glyph, 32.0, 0, &GlyphVariation::default())
            .expect("emoji glyph must rasterize");
        assert!(image.color, "系统 emoji 字体必须走彩色字形路径");
        assert_eq!(
            image.data.len() as u32,
            image.placement.width * image.placement.height * 4,
            "彩色字形载荷 = w × h × 4（RGBA8）"
        );
        let mut colored = 0usize;
        let mut transparent_only = true;
        for pixel in image.data.as_chunks::<4>().0 {
            if pixel[3] != 0 {
                transparent_only = false;
            }
            let max = pixel[..3].iter().copied().max().unwrap_or(0);
            let min = pixel[..3].iter().copied().min().unwrap_or(0);
            if pixel[3] > 200 && max - min > 60 {
                colored += 1;
            }
        }
        assert!(!transparent_only, "emoji 位图不得全透明");
        assert!(
            colored > 20,
            "彩色 emoji 必须产生足够的饱和像素（实测 {colored}）"
        );
    }

    fn first_glyph(font: &parley::FontData, ch: char) -> Option<u16> {
        let swash_font = swash::FontRef::from_index(font.data.data(), font.index as usize)?;
        let glyph = swash_font.charmap().map(ch);
        (glyph != 0).then_some(glyph)
    }
}
