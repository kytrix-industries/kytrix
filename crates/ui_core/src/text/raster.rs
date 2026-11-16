//! 字形光栅：skrifa 取度量/轮廓，swash 出 R8 alpha（P05 §8）。
//!
//! skrifa 0.44 **没有光栅器**（只能用 `OutlineGlyph::draw` 到 `OutlinePen`），因此实际光栅由
//! swash 0.2.10 的 zeno 后端完成；两者共用同一份 `skrifa`（swash 的依赖区间 `<= 0.44`）。
//!
//! # 定位与 hinting 策略（Skia/Chrome 默认，复审 P2-2）
//!
//! - **水平亚像素定位开**：字形按 `1 / SUBPIXEL_PHASES` 物理像素的相位量化后光栅化
//!   （[`SUBPIXEL_PHASES`] + [`split_subpixel_x`]），同一字形的不同相位各占一个图集 tile。
//!   长数字列/滚动行情里每个字形不再被强制 `round` 到整像素，字距误差从「逐字形最多
//!   ±0.5 px」降到「相位量化后最多 1/8 px」。
//! - **hinting 关**（[`HINTING`]）：不再让 swash 按整数 ppem 重排笔画。Skia 的 UI 文本默认
//!   `kNone`（Windows 上 Chrome 同样只做亚像素定位）；hinting 打开时字形内部笔画与 parley
//!   用小数 advance 排出的位置不一致，CJK 还会被横向挤压。代价是小 ppem（≤ 16）下笔画略软。
//!
//! 回退：把 [`SUBPIXEL_PHASES`] 改成 `1`（整像素量化：`floor` 落点，**与旧实现的
//! `round` 不同**，会有最多 1px 的整体位移）或 [`HINTING`] 改成 `true`（旧的整 ppem

use swash::{
    scale::{Render, ScaleContext, Source},
    zeno::{Format, Placement, Vector},
};

/// 水平亚像素定位的相位档数（Skia/Chrome 默认 1/4 px）。
///
/// 每个字形按 `1 / SUBPIXEL_PHASES` 物理像素的相位量化后光栅化；图集为**每个相位各存一个
/// tile**（`GlyphKey::subpixel_x`）。改成 `1` = 关闭亚像素定位（落点吸附到整像素，图集占用
/// 回到旧实现）。
pub(crate) const SUBPIXEL_PHASES: u8 = 4;

/// 字形 hinting 开关（默认关闭，对齐 Skia/Chrome 的 UI 文本策略）。
///
/// 改成 `true` = 回退到旧的「整 ppem + 全 hinting」光栅。仅供视觉回退/对照：hinting 会让
/// 位图内部笔画与 parley 的小数 advance 不一致（见模块文档）。
pub(crate) const HINTING: bool = false;

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

/// 把字形的物理 x 拆成「整数基准 + 水平相位桶」。
///
/// 返回 `(base, phase)`：
/// - `base` 是 `f32` 整数值：绘制时把 tile 放到 `base + placement.left`；
/// - `phase` 是 `0..SUBPIXEL_PHASES` 的光栅相位桶，对应 `phase / SUBPIXEL_PHASES` 物理像素的
///   轮廓平移；`0` = 整像素对齐。
///
/// 与旧的 `x.round()` 的区别：`round` 把落点强制到整像素并丢弃小数；这里把落点吸附到最近的
/// `1 / SUBPIXEL_PHASES` 像素，余量交给光栅器（`Render::offset`），所以字形内部笔画与
/// parley 的小数 advance 保持一致。`frac * phase_count` 四舍五入到 `SUBPIXEL_PHASES` 时进位到
/// 下一个整数档（相位 0），保证桶数恒为 `SUBPIXEL_PHASES` 而不是 `SUBPIXEL_PHASES + 1`。
pub(crate) fn split_subpixel_x(x: f32) -> (f32, u8) {
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
    ///
    /// `subpixel_x` 是 `0..SUBPIXEL_PHASES` 的水平相位桶（见 [`split_subpixel_x`]）：轮廓先按
    /// `subpixel_x / SUBPIXEL_PHASES` 像素平移再光栅化，`placement.left` 已包含这段平移
    /// （zeno 按平移后的轮廓重新计算位图边界），绘制侧因此只需 `整数基准 + placement.left`。
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
        let entry = fonts
            .entry(key)
            .or_insert_with(|| FontEntry { font: font.clone() });
        // 借用 self.fonts 的条目（pin 住 blob）与 self.ctx 是互不重叠的字段借用。
        let bytes = entry.font.data.data();
        let index = entry.font.index as usize;
        let swash_font = swash::FontRef::from_index(bytes, index)?;
        // hinting 关闭（见模块文档）：Skia/Chrome 默认只用亚像素定位，不按整数 ppem 重排笔画。
        let mut builder = ctx.builder(swash_font).size(ppem).hint(HINTING);
        if !variation.is_identity() {
            // 可变字体实例坐标必须与 parley 排版用的坐标一致（见 `GlyphVariation`）。
            builder = builder.normalized_coords(variation.coords.iter());
        }
        let mut scaler = builder.build();
        let mut render = Render::new(&[Source::Outline]);
        render.format(Format::Alpha);
        if subpixel_x != 0 {
            // 只动 x：水平排版的落点需要亚像素相位，竖直方向保持整像素基线（与旧实现一致）。
            render.offset(Vector::new(
                f32::from(subpixel_x) / f32::from(SUBPIXEL_PHASES),
                0.0,
            ));
        }
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
    use super::{GlyphRasterizer, GlyphVariation, probe_font, split_subpixel_x};

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

    /// hinting 关闭（`HINTING == false`）后光栅仍产出非空的 R8 覆盖度（P2-2 方案 A）。
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

    /// P2-2：1/4 px 量化。整像素与 0.1 px 同档；0.4 px 必须换档，否则亚像素定位退化成旧的
    /// 逐字形 `round`。`10.9` 的进位断言同时钉住「桶数 = 4」（不会出现第 5 档）。
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

    /// P2-2：相位必须真的进入光栅。若 `Render::offset` 丢失，同一字形的不同相位会输出同一
    /// 位图，亚像素定位静默失效。
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

    /// P2-2：关闭 hinting 后光栅路径必须仍然产出完整位图（回归：改 `ScalerBuilder` 配置时
    /// 不能把位图变成空的/尺寸错的）。`HINTING == false` 是本方案的决策：回退成 `true` 属于
    /// 视觉决策变更，必须重跑 100%/125%/150% 目视回归并同步更新模块文档。
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

    fn first_glyph(font: &parley::FontData, ch: char) -> Option<u16> {
        let swash_font = swash::FontRef::from_index(font.data.data(), font.index as usize)?;
        let glyph = swash_font.charmap().map(ch);
        (glyph != 0).then_some(glyph)
    }
}
