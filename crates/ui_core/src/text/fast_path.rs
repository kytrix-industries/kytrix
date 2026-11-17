//! 数字快速路径（`DigitFastPath`）：对齐 Blink `SimpleLineLayout` 的「字符 → glyph id → advance」
//! 直拼，跳过 parley 的 itemize / 分析 / 塑形全流程。
//!
//! # 为什么需要
//!
//! 交易终端的文本量集中在价格 / 数量 / 涨跌幅：字符集是
//! `[0-9 . , + - % / :]`（[`ALPHABET`]），字体固定、无 bidi、无复杂脚本、无 OpenType 特性。
//! 这类文本走通用 shaping 是纯开销：每次字符串变化都是一次 `RangedBuilder::build`（itemize +
//! Unicode 分析 + swash 塑形 + 行/项/簇构建）。Blink 的 `SimpleLineLayout`、Qt 的
//! `QRawFont`、Windows Terminal 都是「数字/ASCII 直拼」。
//!
//! # 正确性策略（**运行时自证**，不是「相信假设」）
//!
//! 快速路径**不依赖字体没有 kerning** 这类无法静态验证的假设。每个
//! `(字体栈身份, 全量 shaping 输入)` 第一次命中时做一次**验证性塑形**（等价关系见
//! [`FastKey`]：与形状缓存同一个 [`TextStyle::same_shaping_inputs`]）：
//!
//! 1. 对 [`ALPHABET`] 的每个字符单独用 parley 塑形一次，取 glyph id / advance（逻辑像素）/
//!    行度量（ascent/descent/line-height/baseline）/ 实例坐标（可变字体）/ 合成标志；
//! 2. 再塑形一次「所有有序字符对」（`AA`、`AB`…，800 字符）的探针串，逐字形断言
//!    `x == 前缀 advance 之和`（容差 [`ADVANCE_TOLERANCE`] = 0.01 px）且 glyph id 与第 1 步一致。
//!    这一步**同时**证明了「无 kerning」「无默认连字/替换」「advance 可加」，即快速路径
//!    与 parley 逐字形等价；
//! 3. 任一条不成立 ⇒ 该字体组合**永久**降级到 parley（`FastEntry::Unsupported`），并计入
//!    [`FastPathStats::rejected`]（可观测，不静默）。
//!
//! 于是「快速路径结果 == parley 结果」是该字体上被证明过的性质，而不是对宿主字体的假设；
//! 换字体 / 换任一 shaping 输入（字号、字重、行高、stretch、locale、可变字体轴…）都会重新
//! 验证。验证成本是 1 + 17 次短串塑形，每种组合只付一次。
//!
//! # 单行性（为什么不需要断行）
//!
//! 字符集里没有任何**软换行机会**（无空格；UAX#14 在 `NU`/`IS`/`PO`/`PR` 之间不换行），
//! 唯一可能产生换行机会的是 `-`（HY）与 `/`（SY）之后 —— 因此：
//!
//! - `TextWrap::NoWrap`：本来就不换行；
//! - 其它策略：只有文本**不含** `-` / `/` 时才走快速路径（`/-` 之后的断行规则需按 UAX#14 的
//!   `HY × NU` / `SY` 上下文判定，不值得为行情表引入）。
//!
//! 因此快速路径的布局恒为单行：`min-content == max-content == 总 advance`，
//! `measure` 是 O(1)、`layout` 不需要 `break_all_lines`。
//!
//! # 开关
//!
//! [`DigitFastPath::set_enabled`]（`App::set_digit_fast_path`）可整体回退到 parley 路径做对照；
//! 命中率由 [`FastPathStats`] 计入 `TextPassReport::{fast_path_shaped, fast_path_queries}`。

use std::{hash::Hasher, rc::Rc};

use parley::PositionedLayoutItem;
use smol_str::SmolStr;

use super::{
    ShapedGlyphRun, ShapedLine, ShapedTextItem, ShapedTextLine, TextStyle,
    raster::{GlyphVariation, split_subpixel_x},
};
use crate::{
    math::{Point, Rect, Size},
    paint::{color::Color, primitive::PositionedGlyph},
};

/// 快速路径接受的字符集（顺序即内部索引，改动即缓存失效）。
pub(crate) const ALPHABET: [char; 17] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', '.', ',', '+', '-', '%', '/', ':',
];

/// 快速路径与 parley 的 advance 允许误差（逻辑像素）。
pub(crate) const ADVANCE_TOLERANCE: f32 = 0.01;

/// 字符在 [`ALPHABET`] 里的下标（`None` = 不在快速路径字符集里）。
fn alphabet_index(character: char) -> Option<usize> {
    ALPHABET
        .iter()
        .position(|candidate| *candidate == character)
}

/// `-` / `/` 之后存在 UAX#14 的软换行机会：`Wrap`/`Anywhere` 下要按上下文判定，直接放弃。
fn has_hyphen_break_opportunity(text: &str) -> bool {
    text.as_bytes()
        .iter()
        .any(|byte| *byte == b'-' || *byte == b'/')
}

/// 一次验证 / 快速塑形用的字体级数据（按 `(字体栈身份, 全量样式)` 缓存）。
struct FastFont {
    /// parley 的字体身份（`Blob` + TTC 索引）：光栅与图集键都要它。
    font: parley::FontData,
    /// 可变字体实例坐标 / faux bold（光栅必须与排版同参数）。
    variation_coords: Vec<i16>,
    synthesis: fontique::Synthesis,
    /// 每个字符的 glyph id 与 advance（逻辑像素，已含实例坐标）。
    chars: [Option<FastChar>; ALPHABET.len()],
    /// 行度量（取 parley 的探针实测值，逻辑像素）。
    metrics: FastMetrics,
}

#[derive(Clone, Copy, Debug)]
struct FastChar {
    glyph: u32,
    advance: f32,
}

/// 单行文本的行度量（逻辑像素）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FastMetrics {
    /// 首行基线（文本盒局部）。
    pub baseline: f32,
    /// 行高（含 leading）。
    pub line_height: f32,
}

/// 一次快速路径的塑形结果：**单行**字形序列 + 行度量。
#[derive(Clone, Debug)]
pub(crate) struct SimpleShaped {
    glyphs: Vec<SimpleGlyph>,
    width: f32,
    metrics: FastMetrics,
    font: parley::FontData,
    variation_coords: Vec<i16>,
    synthesis: fontique::Synthesis,
    /// 源文本字节数（ASCII ⇒ 字节 == 字符数）。
    text_len: usize,
}

/// 一个已定位的简单字形（逻辑坐标）。
#[derive(Clone, Copy, Debug)]
pub(crate) struct SimpleGlyph {
    /// glyph id（`!= 0`；0 由调用方降级到 parley 以便出 tofu 诊断）。
    pub glyph: u32,
    /// 逻辑 x（文本盒局部）。
    pub x: f32,
}

impl SimpleShaped {
    /// 源文本字节数。
    pub(crate) fn text_len(&self) -> usize {
        self.text_len
    }

    /// 总 advance（逻辑像素）= 单行宽度。
    #[cfg(test)]
    pub(crate) fn width(&self) -> f32 {
        self.width
    }

    /// 内容尺寸（逻辑像素）：单行 ⇒ 高 = 行高。
    pub(crate) fn size(&self) -> Size {
        Size::new(self.width, self.metrics.line_height)
    }

    /// 行度量。
    pub(crate) fn metrics(&self) -> FastMetrics {
        self.metrics
    }

    /// 字符下标处的 glyph id（测试 / 诊断用）。
    #[cfg(test)]
    pub(crate) fn glyph_at(&self, index: usize) -> u32 {
        self.glyphs.get(index).map_or(0, |glyph| glyph.glyph)
    }

    /// 字符下标处的逻辑 x（`index >= 字符数` 时返回总宽）。
    pub(crate) fn x_at(&self, index: usize) -> f32 {
        self.glyphs.get(index).map_or(self.width, |glyph| glyph.x)
    }

    /// `byte_index` 处的 caret x（逻辑像素；ASCII ⇒ 字节下标 == 字符下标）。
    pub(crate) fn caret_x(&self, byte_index: usize) -> f32 {
        self.x_at(byte_index.min(self.text_len))
    }

    /// 单行选区矩形（逻辑像素，文本盒局部）。
    pub(crate) fn selection_rect(&self, anchor: usize, focus: usize) -> Option<Rect> {
        let (start, end) = if anchor <= focus {
            (anchor, focus)
        } else {
            (focus, anchor)
        };
        let x0 = self.caret_x(start);
        let x1 = self.caret_x(end);
        if (x1 - x0).abs() <= f32::EPSILON {
            return None;
        }
        Some(Rect::new(x0, 0.0, x1 - x0, self.metrics.line_height))
    }

    /// 光栅成 [`ShapedLine`]（单行；`color` 由当前内容决定，不参与形状缓存）。
    ///
    /// `ctx.scale` 是当前设备 scale：逻辑坐标 × scale = 物理落点，水平相位按物理坐标量化
    /// （`crate::text::raster::split_subpixel_x`），竖直按整物理像素对齐。
    pub(crate) fn rasterize(
        &self,
        text: SmolStr,
        style: &TextStyle,
        color: Color,
        ctx: &mut super::shape::RasterCtx<'_>,
    ) -> Result<ShapedLine, crate::error::EngineError> {
        let scale = ctx.scale;
        let ppem = (style.size * scale).clamp(1.0, 512.0);
        let variation = GlyphVariation::of(&self.variation_coords, self.synthesis, ppem);
        let baseline = self.metrics.baseline;
        let mut glyphs = Vec::with_capacity(self.glyphs.len());
        for glyph in &self.glyphs {
            let (base_x, subpixel_x) = split_subpixel_x(glyph.x * scale);
            let tile = ctx.glyph_atlas.ensure(
                ctx.raster,
                ctx.atlas,
                &self.font,
                glyph.glyph,
                ppem,
                subpixel_x,
                Some(&variation),
            )?;
            let Some(tile) = tile else {
                continue;
            };
            let origin_x = base_x + tile.left as f32;
            let origin_y = (baseline * scale).round() - tile.top as f32;
            glyphs.push(PositionedGlyph {
                tile: tile.tile,
                position: Point::new(origin_x / scale, origin_y / scale),
                size: Size::new(tile.width as f32 / scale, tile.height as f32 / scale),
            });
        }
        let line = ShapedTextLine {
            text_range: 0..text.len(),
            baseline,
            line_height: self.metrics.line_height,
            advance: self.width,
            items: vec![ShapedTextItem::GlyphRun(ShapedGlyphRun {
                color,
                text_range: 0..text.len(),
                glyphs,
            })],
        };
        Ok(ShapedLine {
            text,
            lines: vec![line],
            width: self.width,
            height: self.metrics.line_height,
            first_baseline: Some(baseline),
            atlas_generation: ctx.generation,
            font_stack_version: 0,
            scale,
        })
    }
}

/// 快速路径统计（喂 `TextPassReport`；命中率 = `hits / queries`）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FastPathStats {
    /// 进入快速路径判定的塑形次数（分母）。
    pub queries: usize,
    /// 真正走快速路径的次数（分子）。
    pub hits: usize,
    /// 走快速路径的字形总数（命中时 ≈ 字符数之和）。
    pub glyphs: usize,
    /// 首次验证失败的 `(字体栈身份, 全量样式)` 组合数（永久降级）。
    pub rejected: usize,
    /// 通过验证的字体组合数（缓存条目）。
    pub verified: usize,
}

/// 数字快速路径缓存 + 开关（进程级，由 `TextContext` 持有）。
pub(crate) struct DigitFastPath {
    enabled: bool,
    fonts: ahash::HashMap<FastKey, FastEntry>,
    stats: FastPathStats,
}

enum FastEntry {
    /// 已验证：`style` 是 `Rc<FastFont>` 被探针塑形时使用的**全量**样式。
    ///
    /// 命中时必须用 [`TextStyle::same_shaping_inputs`] 与请求样式复验：摘要只用于寻址，
    /// 摘要碰撞时两条不同样式会落在同一个键上，只按摘要命中就会把别的样式的行度量 /
    /// glyph id 交出去。
    Ready {
        font: Rc<FastFont>,
        style: TextStyle,
    },
    /// 验证失败：该组合永久走 parley（不静默：计入 `stats.rejected`）。
    Unsupported { style: TextStyle },
}

impl FastEntry {
    /// 该条目被验证时的全量样式（`Ready` 与 `Unsupported` 都必须能复验）。
    fn style(&self) -> &TextStyle {
        match self {
            Self::Ready { style, .. } | Self::Unsupported { style } => style,
        }
    }
}

/// 验证缓存的键：`(字体栈身份, 全量 shaping 输入摘要)`。
///
/// 键的等价关系**就是** [`TextStyle::same_shaping_inputs`]：`style_digest` 由
/// [`TextStyle::hash_into`] 算出（与形状缓存 `text/cache.rs` 的 `ShapeKey` 同一套字段集与
/// 写法），因此 `line_height` / `stretch` / `locale` / `font_variations` / `word_spacing` /
/// OpenType 特性 / 断行 / 行数上限等**全部** shaping 输入都进键。这些字段确实被 `verify` 的
/// 探针塑形消费（`shape::push_shaping_defaults`）：
///
/// - `line_height` 决定探针实测的 `FastFont::metrics`（`baseline` / `line_height`）；
/// - `stretch` 改变字体选择（可能落到另一个 face）、`font_variations` 改变可变字体实例坐标
///   ⇒ **glyph id 与 advance 都会变**；
/// - `locale` 影响 `locl` 等语言相关的 GSUB 选择。
///
/// 旧键只有 `(font_identity, size, weight, slant)`：先塑形 `.with_line_height(2.0)` 的表头会把
/// 它的行度量缓存进表，随后默认行高的价格文本命中同一键、拿到表头的 `baseline` —— 后者是
/// 静默错误（P1-②）。`font_identity` 是 `TextContext` 的字体栈版本与家族覆盖的摘要：换栈 /
/// 重载系统字体会换新键，旧表自然失效（不保留 migration）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct FastKey {
    font_identity: u64,
    style_digest: u64,
}

impl FastKey {
    /// 由字体栈身份 + 全量样式构造（摘要写法与 `text/cache.rs` 的 `ShapeKey` 一致）。
    fn new(font_identity: u64, style: &TextStyle) -> Self {
        let mut hasher = ahash::AHasher::default();
        style.hash_into(&mut hasher);
        Self {
            font_identity,
            style_digest: hasher.finish(),
        }
    }
}

impl DigitFastPath {
    pub(crate) fn new() -> Self {
        Self {
            enabled: true,
            fonts: ahash::HashMap::default(),
            stats: FastPathStats::default(),
        }
    }

    /// 开关（回退验证用）：关闭后所有文本都走 parley。
    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// 当前开关状态。
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// 字体集合或样式相关配置变化后清空验证表与统计（字体栈/系统字体变了，旧的验证结论失效）。
    pub(crate) fn reset(&mut self) {
        self.fonts.clear();
        self.stats = FastPathStats::default();
    }

    /// 取走统计（累加进 `TextPassReport` 后清零）。
    pub(crate) fn take_stats(&mut self) -> FastPathStats {
        core::mem::take(&mut self.stats)
    }

    /// 尝试走快速路径。`None` = 不适用（调用方走 parley）。
    ///
    /// 命中判据是「键（字体栈身份 + 全量样式摘要）相同 **且**
    /// [`TextStyle::same_shaping_inputs`] 复验通过」：哈希只用来寻址，等价性逐字段比较，
    /// 因此与形状缓存（`ShapeKey` + `ShapedText::same_shaping_inputs`）是**同一个等价关系**，
    /// 不会出现「同键不同样式」的串味。复验失败（摘要碰撞）按 miss 重新验证并覆盖该条目。
    ///
    /// `font_key` 是**当前字体栈版本**的身份（字体变了必须重新验证）；`family` 是
    /// `TextContext` 解析出的 parley 家族列表（探针塑形必须与生产塑形用同一份）。
    pub(crate) fn try_shape(
        &mut self,
        font_cx: &mut parley::FontContext,
        layout_cx: &mut parley::LayoutContext<crate::paint::color::ColorBrush>,
        family: &parley::FontFamily<'static>,
        font_identity: u64,
        text: &str,
        style: &TextStyle,
    ) -> Option<SimpleShaped> {
        if !self.enabled || text.is_empty() {
            return None;
        }
        if !eligible(text, style) {
            return None;
        }
        self.stats.queries += 1;
        let key = FastKey::new(font_identity, style);
        // 命中：摘要相同（寻址）+ 全量 shaping 输入逐字段等价（复验，防 64 位摘要碰撞）。
        if let Some(entry) = self
            .fonts
            .get(&key)
            .filter(|entry| entry.style().same_shaping_inputs(style))
        {
            let FastEntry::Ready { font, .. } = entry else {
                // 该组合此前验证失败：永久走 parley（`rejected` 已在首次失败时计入）。
                return None;
            };
            let simple = build_simple(font, text)?;
            self.stats.hits += 1;
            self.stats.glyphs += simple.glyphs.len();
            return Some(simple);
        }
        // 首次 / 摘要碰撞：验证一次并覆盖该键（碰撞时结论替换旧条目，不静默错配）。
        let font = match verify(font_cx, layout_cx, family, style) {
            Some(font) => {
                self.stats.verified += 1;
                let font = Rc::new(font);
                self.fonts.insert(
                    key,
                    FastEntry::Ready {
                        font: Rc::clone(&font),
                        style: style.clone(),
                    },
                );
                font
            }
            None => {
                self.stats.rejected += 1;
                self.fonts.insert(
                    key,
                    FastEntry::Unsupported {
                        style: style.clone(),
                    },
                );
                return None;
            }
        };
        let simple = build_simple(&font, text)?;
        self.stats.hits += 1;
        self.stats.glyphs += simple.glyphs.len();
        Some(simple)
    }
}

impl Default for DigitFastPath {
    fn default() -> Self {
        Self::new()
    }
}

/// 快速路径的适用条件（除「验证通过」以外的静态条件）。
fn eligible(text: &str, style: &TextStyle) -> bool {
    if style.letter_spacing != 0.0 {
        return false;
    }
    // OpenType 特性（tnum 等）需要 GSUB 替换：快速路径不跑 shaping，直接放弃。
    if style.font_features().is_some() {
        return false;
    }
    // `Wrap`/`Anywhere` 下 `-`/`/` 之后有软换行机会（UAX#14），单行假设不成立。
    if style.wrap != super::TextWrap::NoWrap && has_hyphen_break_opportunity(text) {
        return false;
    }
    text.chars()
        .all(|character| alphabet_index(character).is_some())
}

/// 由验证过的字体表构造单行 glyph 序列（任一字形缺失 ⇒ `None`，退回 parley 出 tofu 诊断）。
fn build_simple(font: &FastFont, text: &str) -> Option<SimpleShaped> {
    let mut glyphs = Vec::with_capacity(text.len());
    let mut x = 0.0f32;
    for character in text.chars() {
        let slot = font.chars[alphabet_index(character)?];
        let fast_char = slot?;
        if fast_char.glyph == 0 {
            return None;
        }
        glyphs.push(SimpleGlyph {
            glyph: fast_char.glyph,
            x,
        });
        x += fast_char.advance;
    }
    Some(SimpleShaped {
        glyphs,
        width: x,
        metrics: font.metrics,
        font: font.font.clone(),
        variation_coords: font.variation_coords.clone(),
        synthesis: font.synthesis,
        text_len: text.len(),
    })
}

/// 验证性塑形：该 `(字体, 全量样式)` 是否满足快速路径的全部前提。
///
/// 返回 `None` = 不满足（permanent 降级）。成功时返回的 advance / 行度量**来自 parley 本身**，
/// 因此不存在「自己算一套度量」的漂移风险。
fn verify(
    font_cx: &mut parley::FontContext,
    layout_cx: &mut parley::LayoutContext<crate::paint::color::ColorBrush>,
    family: &parley::FontFamily<'static>,
    style: &TextStyle,
) -> Option<FastFont> {
    // 1. 逐字符单独塑形：glyph id / advance / 行度量 / 实例参数。
    let mut chars: [Option<FastChar>; ALPHABET.len()] = [None; ALPHABET.len()];
    let mut font: Option<parley::FontData> = None;
    let mut variation_coords: Vec<i16> = Vec::new();
    let mut synthesis = fontique::Synthesis::default();
    let mut metrics: Option<FastMetrics> = None;
    for (index, character) in ALPHABET.iter().copied().enumerate() {
        let buffer = [character as u8];
        // 安全：`character` 全部是 ASCII，单字节切片就是合法 UTF-8。
        let probe = core::str::from_utf8(&buffer).ok()?;
        let layout = probe_layout(font_cx, layout_cx, family, style, probe)?;
        let (id, advance, probe_font, coords, probe_synthesis) = single_glyph(&layout)?;
        if id == 0 {
            return None;
        }
        if let Some(existing) = &font {
            if existing != &probe_font {
                // 同一份字符集解析到了不同字体（fallback 链）：单行假设下的字形簇不再同质。
                return None;
            }
        } else {
            font = Some(probe_font);
            variation_coords = coords;
            synthesis = probe_synthesis;
        }
        let line = layout.lines().next()?;
        let line_metrics = line.metrics();
        let fast_metrics = FastMetrics {
            baseline: line_metrics.baseline,
            line_height: line_metrics.line_height,
        };
        match &metrics {
            Some(existing) if existing != &fast_metrics => return None,
            Some(_) => {}
            None => metrics = Some(fast_metrics),
        }
        chars[index] = Some(FastChar { glyph: id, advance });
    }
    let font = font?;
    let metrics = metrics?;
    let fast = FastFont {
        font,
        variation_coords,
        synthesis,
        chars,
        metrics,
    };

    // 2. 全部有序字符对：证明「无 kerning / 无默认替换 / advance 可加」。
    let mut pairs = String::with_capacity(ALPHABET.len() * ALPHABET.len() * 2);
    for first in ALPHABET {
        for second in ALPHABET {
            pairs.push(first);
            pairs.push(second);
        }
    }
    let layout = probe_layout(font_cx, layout_cx, family, style, &pairs)?;
    verify_pairs(&layout, &fast)?;
    Some(fast)
}

/// 用与生产路径**完全相同**的样式属性塑形一段探针文本（逻辑坐标：scale = 1.0）。
fn probe_layout(
    font_cx: &mut parley::FontContext,
    layout_cx: &mut parley::LayoutContext<crate::paint::color::ColorBrush>,
    family: &parley::FontFamily<'static>,
    style: &TextStyle,
    text: &str,
) -> Option<parley::Layout<crate::paint::color::ColorBrush>> {
    let mut builder = layout_cx.ranged_builder(font_cx, text, 1.0, true);
    builder.push_default(parley::StyleProperty::FontFamily(family.clone()));
    super::shape::push_shaping_defaults(&mut builder, style);
    let mut layout = builder.build(text);
    layout.break_all_lines(None);
    Some(layout)
}

/// 从单字符布局里取 `(glyph id, advance, 字体, 实例坐标, 合成标志)`。
fn single_glyph(
    layout: &parley::Layout<crate::paint::color::ColorBrush>,
) -> Option<(u32, f32, parley::FontData, Vec<i16>, fontique::Synthesis)> {
    let line = layout.lines().next()?;
    for item in line.items() {
        let PositionedLayoutItem::GlyphRun(run) = item else {
            continue;
        };
        let mut glyphs = run.positioned_glyphs();
        let glyph = glyphs.next()?;
        // 单字符必须恰好产生一个字形（连字/替换会得到 0 或 >1 个）。
        if glyphs.next().is_some() {
            return None;
        }
        let advance = run.run().advance();
        let coords: Vec<i16> = run
            .run()
            .normalized_coords()
            .iter()
            .map(|coord| coord.to_bits())
            .collect();
        return Some((
            glyph.id,
            advance,
            run.run().font().clone(),
            coords,
            run.run().synthesis(),
        ));
    }
    None
}

/// 校验「有序字符对」探针：每个字形的位置必须等于前缀 advance 之和。
fn verify_pairs(
    layout: &parley::Layout<crate::paint::color::ColorBrush>,
    font: &FastFont,
) -> Option<()> {
    let mut glyph_x: Vec<f32> = Vec::with_capacity(ALPHABET.len() * ALPHABET.len() * 2);
    let mut glyph_ids: Vec<u32> = Vec::with_capacity(glyph_x.capacity());
    for line in layout.lines() {
        for item in line.items() {
            let PositionedLayoutItem::GlyphRun(run) = item else {
                continue;
            };
            for glyph in run.positioned_glyphs() {
                glyph_x.push(glyph.x);
                glyph_ids.push(glyph.id);
            }
        }
    }
    let expected = ALPHABET.len() * ALPHABET.len() * 2;
    if glyph_x.len() != expected {
        // 连字/替换把字符数压缩了：不满足「一字符一字形」。
        return None;
    }
    let mut position = 0usize;
    let mut x = 0.0f32;
    for first in ALPHABET {
        for second in ALPHABET {
            for character in [first, second] {
                let fast_char = font.chars[alphabet_index(character)?]?;
                if (glyph_x[position] - x).abs() > ADVANCE_TOLERANCE {
                    return None;
                }
                if glyph_ids[position] != fast_char.glyph {
                    return None;
                }
                x += fast_char.advance;
                position += 1;
            }
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::{
        ADVANCE_TOLERANCE, ALPHABET, DigitFastPath, FastEntry, FastFont, SimpleShaped,
        alphabet_index, eligible, probe_layout,
    };
    use crate::{
        paint::color::ColorBrush,
        text::{
            FontStack, TextAlign, TextContent, TextDirection, TextFontVariation, TextStyle,
            TextWrap,
            context::TextContext,
            shape::{self, family_list},
        },
    };

    fn harness() -> (
        parley::FontContext,
        parley::LayoutContext<ColorBrush>,
        parley::FontFamily<'static>,
    ) {
        (
            parley::FontContext::new(),
            parley::LayoutContext::new(),
            family_list(&FontStack::system_ui(), None),
        )
    }

    fn has_system_fonts(font_cx: &mut parley::FontContext) -> bool {
        font_cx.collection.family_names().next().is_some()
    }

    /// parley 全量参考塑形的结果（与 `SimpleShaped` 同口径）。
    struct ParleyReference {
        /// 逐字形 `(glyph id, x)`。
        glyphs: Vec<(u32, f32)>,
        /// 总 advance（逻辑像素）。
        width: f32,
        /// 首行基线。
        baseline: f32,
        /// 首行行高。
        line_height: f32,
    }

    /// parley 的全量参考塑形：**直接复用生产探针的 builder 配置**。
    ///
    /// `probe_layout` = `FontFamily` + `shape::push_shaping_defaults`，与 `verify` 的探针是同一套
    /// 输入（size / weight / slant / letter-spacing / line-height / features / stretch /
    /// word-spacing / locale / variations / line-break / `WhiteSpaceCollapse` / wrap）。旧实现
    /// 自己拼 `push_default` 且只压 5 项，与生产探针**不等价**：`line_height` / `stretch` /
    /// `locale` / `font_variations` 的差异在测试里天然暴露不出来（P1-② 因此被假绿掩盖）。
    /// 复用之后「参考 == 生产探针的输入」是构造上的事实，不存在第二套样式定义漂移。
    fn parley_reference(
        font_cx: &mut parley::FontContext,
        layout_cx: &mut parley::LayoutContext<ColorBrush>,
        family: &parley::FontFamily<'static>,
        text: &str,
        style: &TextStyle,
    ) -> ParleyReference {
        // 参考里没有等价入口的字段必须**显式断言**，不能静默给出一个不等价的参考：
        // - `direction`：生产在 `ShapedText::shape` 里调 `set_base_direction`，而 `probe_layout`
        //   用 builder 默认值；parley 的默认是 `BaseDirection::Auto`（`RangedBuilder::new`），
        //   只有 `Auto` 与生产逐字一致；
        // - `color` / `alignment` 不是 shaping 输入（R12-11）；
        // - `overflow` / `max_lines` 在 shape 之后由 `apply_overflow` 应用，同样不改
        //   glyph id / advance / 行度量。
        assert_eq!(
            style.direction,
            TextDirection::Auto,
            "parley 参考只与 Auto 基准方向逐字等价"
        );
        let layout =
            probe_layout(font_cx, layout_cx, family, style, text).expect("探针布局必须能建出来");
        let mut glyphs = Vec::new();
        for line in layout.lines() {
            for item in line.items() {
                if let parley::PositionedLayoutItem::GlyphRun(run) = item {
                    for glyph in run.positioned_glyphs() {
                        glyphs.push((glyph.id, glyph.x));
                    }
                }
            }
        }
        let (baseline, line_height) = layout.lines().next().map_or((0.0, 0.0), |line| {
            let metrics = line.metrics();
            (metrics.baseline, metrics.line_height)
        });
        ParleyReference {
            glyphs,
            width: layout.width(),
            baseline,
            line_height,
        }
    }

    /// 断言快速路径结果与「该样式自己的」parley 全量参考一致。
    fn assert_matches_reference(simple: &SimpleShaped, reference: &ParleyReference, text: &str) {
        assert_eq!(
            reference.glyphs.len(),
            simple.text_len(),
            "{text}: 一字符一字形"
        );
        for (index, (glyph_id, x)) in reference.glyphs.iter().enumerate() {
            assert_eq!(
                *glyph_id,
                simple.glyph_at(index),
                "{text}: 第 {index} 个字形 id"
            );
            assert!(
                (x - simple.x_at(index)).abs() <= ADVANCE_TOLERANCE,
                "{text}: 第 {index} 个字形 x {x} vs 快速路径 {}",
                simple.x_at(index)
            );
        }
        assert!(
            (reference.width - simple.width()).abs() <= ADVANCE_TOLERANCE,
            "{text}: 宽度 {} vs 快速路径 {}",
            reference.width,
            simple.width()
        );
        assert!(
            (reference.baseline - simple.metrics().baseline).abs() <= ADVANCE_TOLERANCE,
            "{text}: baseline {} vs 快速路径 {}",
            reference.baseline,
            simple.metrics().baseline
        );
        assert!(
            (reference.line_height - simple.metrics().line_height).abs() <= ADVANCE_TOLERANCE,
            "{text}: line_height {} vs 快速路径 {}",
            reference.line_height,
            simple.metrics().line_height
        );
    }

    /// 快速路径表里 `Ready` 条目的 `Rc<FastFont>`（`Unsupported` 不计）。
    fn ready_fonts(fast: &DigitFastPath) -> Vec<Rc<FastFont>> {
        fast.fonts
            .values()
            .filter_map(|entry| match entry {
                FastEntry::Ready { font, .. } => Some(Rc::clone(font)),
                FastEntry::Unsupported { .. } => None,
            })
            .collect()
    }

    /// 测试专用：从 `ShapedText` 的 parley 快照里取逐字形 `(id, x, advance)` + 首行度量。
    ///
    /// 只对 `ShapedKind::Parley` 有效：关闭快速路径后的生产快照就走这条。
    fn parley_snapshot_glyphs(shaped: &shape::ShapedText) -> (Vec<(u32, f32, f32)>, f32, f32) {
        let layout = shaped.layout();
        let mut glyphs = Vec::new();
        for line in layout.lines() {
            for item in line.items() {
                if let parley::PositionedLayoutItem::GlyphRun(run) = item {
                    for glyph in run.positioned_glyphs() {
                        glyphs.push((glyph.id, glyph.x, glyph.advance));
                    }
                }
            }
        }
        let (baseline, line_height) = layout.lines().next().map_or((0.0, 0.0), |line| {
            let metrics = line.metrics();
            (metrics.baseline, metrics.line_height)
        });
        (glyphs, baseline, line_height)
    }

    /// 走一次快速路径；被拒绝 ⇒ 显式失败。
    ///
    /// 拒绝意味着宿主字体对字符集有 kerning/连字，或探针建不出来：这是必须让人看见的失败，
    /// 不能像旧测试那样 `continue` / `eprintln!("skip")`。
    fn shape_fast(
        fast: &mut DigitFastPath,
        font_cx: &mut parley::FontContext,
        layout_cx: &mut parley::LayoutContext<ColorBrush>,
        family: &parley::FontFamily<'static>,
        text: &str,
        style: &TextStyle,
    ) -> SimpleShaped {
        fast.try_shape(font_cx, layout_cx, family, 7, text, style)
            .unwrap_or_else(|| panic!("快速路径拒绝了文本 '{text}'：必须显式失败而不是跳过"))
    }

    #[test]
    fn alphabet_indices_are_stable() {
        assert_eq!(ALPHABET.len(), 17);
        assert_eq!(alphabet_index('0'), Some(0));
        assert_eq!(alphabet_index('9'), Some(9));
        assert_eq!(alphabet_index('.'), Some(10));
        assert_eq!(alphabet_index(':'), Some(16));
        assert_eq!(alphabet_index('a'), None);
        assert_eq!(alphabet_index(' '), None);
        assert_eq!(alphabet_index('中'), None);
    }

    #[test]
    fn eligibility_requires_the_fast_path_alphabet() {
        let style = TextStyle::new(12.0);
        assert!(eligible("1,234.56", &style));
        assert!(
            !eligible("-1.50%", &style),
            "默认 Wrap 下 `-` 之后有软换行机会（UAX#14 HY）⇒ 单行假设不成立"
        );
        assert!(!eligible("1 234", &style), "空格不在字符集里");
        assert!(!eligible("abc", &style));
        assert!(
            eligible("-1.50%", &style.clone().with_wrap(TextWrap::NoWrap)),
            "NoWrap 不受 `-` 影响"
        );
        assert!(
            !eligible("1,234.56", &style.clone().with_letter_spacing(0.5)),
            "letter-spacing 不在快速路径里"
        );
        assert!(
            !eligible(
                "1,234.56",
                &style.with_font_features([crate::text::TextFeature::TABULAR_NUMBERS])
            ),
            "OpenType 特性需要 shaping"
        );
    }

    /// 核心正确性：快速路径的 glyph id / advance / 位置 / 行度量必须与**全量样式**的 parley
    /// 参考一致（≤0.01 px）。
    ///
    /// 旧版本有两个逃逸口：参考只压 size/weight/letter_spacing（与生产探针不等价），验证被
    /// 拒绝时 `continue` 跳过。两者都已删除 —— 拒绝在这里就是失败。
    #[test]
    fn fast_path_matches_parley_advances_and_positions() {
        let (mut font_cx, mut layout_cx, family) = harness();
        if !has_system_fonts(&mut font_cx) {
            return;
        }
        let style = TextStyle::new(12.0).with_wrap(TextWrap::NoWrap);
        let mut fast = DigitFastPath::new();
        let probes = [
            "0",
            "1,234.56",
            "9999999",
            "-1.50%",
            "12/34",
            "1:2",
            "0.00000001",
            "+42,000",
        ];
        for text in probes {
            let simple = shape_fast(
                &mut fast,
                &mut font_cx,
                &mut layout_cx,
                &family,
                text,
                &style,
            );
            let reference = parley_reference(&mut font_cx, &mut layout_cx, &family, text, &style);
            assert_matches_reference(&simple, &reference, text);
        }
        let stats = fast.take_stats();
        assert_eq!(stats.queries, probes.len());
        assert_eq!(stats.hits, probes.len(), "所有探针都必须走快速路径");
        assert_eq!(stats.verified, 1, "同一 (字体, 全量样式) 只验证一次");
        assert_eq!(stats.rejected, 0);
    }

    /// 开关：关闭后 `try_shape` 立即返回 `None`（不计查询），**且端到端逐位回到 parley**。
    ///
    /// 旧版本只断言 `try_shape` 返回 `None`：11-d 的「关闭开关后与 parley 逐位相同」没有任何
    /// 证据。现在锁三层：
    /// 1. 关闭后快速路径不查询 / 不命中 / 不验证（统计全零）；
    /// 2. 关闭后生产入口（`TextContext::shape_cached`）产出的就是 parley 快照，其逐字形
    ///    id / x / advance 与「直接用全量样式塑形」**逐位相等**（同一个 builder，不需要容差）；
    /// 3. 开启时的快速路径结果与关闭后的 parley 结果逐字形一致（id 相同、x ≤ 0.01 px），
    ///    端到端尺寸与每个字节位置的 caret x 也一致 —— 开关只切实现，不切语义。
    #[test]
    fn disabling_the_fast_path_returns_none() {
        let (mut font_cx, mut layout_cx, family) = harness();
        if !has_system_fonts(&mut font_cx) {
            return;
        }
        let style = TextStyle::new(12.0).with_wrap(TextWrap::NoWrap);
        let text = "1,234.56";
        let content = TextContent::new(text, style.clone());

        // 1) 开启：生产入口走快速路径（一次验证 + 一次命中）。
        let mut enabled = TextContext::new();
        let fast_shaped = enabled.shape_cached(&content).0;
        let stats = enabled.take_fast_path_stats();
        assert_eq!((stats.queries, stats.hits), (1, 1), "开启时必须走快速路径");
        assert_eq!((stats.verified, stats.rejected), (1, 0));

        // 2) 关闭：`try_shape` 语义 + 生产入口回到 parley。
        let mut disabled = TextContext::new();
        disabled.set_digit_fast_path(false);
        assert!(!disabled.digit_fast_path_enabled());
        let parley_shaped = disabled.shape_cached(&content).0;
        let stats = disabled.take_fast_path_stats();
        assert_eq!(
            (stats.queries, stats.hits, stats.verified, stats.rejected),
            (0, 0, 0, 0),
            "关闭后不得查询 / 命中 / 验证"
        );

        // 2a) 关闭后的生产快照 == 直接用全量样式塑形的参考（逐位相等）。
        let reference = parley_reference(&mut font_cx, &mut layout_cx, &family, text, &style);
        let (parley_glyphs, baseline, line_height) = parley_snapshot_glyphs(&parley_shaped);
        assert_eq!(parley_glyphs.len(), reference.glyphs.len(), "字形数");
        for (index, ((id, x, _advance), (reference_id, reference_x))) in parley_glyphs
            .iter()
            .zip(reference.glyphs.iter())
            .enumerate()
        {
            assert_eq!(
                id, reference_id,
                "关闭后第 {index} 个 glyph id 必须与参考相同"
            );
            assert_eq!(
                x.to_bits(),
                reference_x.to_bits(),
                "关闭后第 {index} 个字形 x 必须与参考逐位相等：{x} vs {reference_x}"
            );
        }
        assert_eq!(
            baseline.to_bits(),
            reference.baseline.to_bits(),
            "baseline 逐位相等"
        );
        assert_eq!(
            line_height.to_bits(),
            reference.line_height.to_bits(),
            "行高逐位相等"
        );
        assert_eq!(
            parley_shaped.max_content_size().width.to_bits(),
            reference.width.to_bits(),
            "max-content 宽度逐位相等"
        );

        // 3) 开启的快速路径 vs 关闭后的 parley：逐字形 + 端到端位置一致。
        let mut fast = DigitFastPath::new();
        let simple = shape_fast(
            &mut fast,
            &mut font_cx,
            &mut layout_cx,
            &family,
            text,
            &style,
        );
        assert_eq!(simple.text_len(), parley_glyphs.len());
        for (index, (id, x, _advance)) in parley_glyphs.iter().enumerate() {
            assert_eq!(
                simple.glyph_at(index),
                *id,
                "第 {index} 个字形 id 必须与 parley 相同"
            );
            assert!(
                (simple.x_at(index) - x).abs() <= ADVANCE_TOLERANCE,
                "第 {index} 个字形 x {} vs parley {x}",
                simple.x_at(index)
            );
        }
        let width = parley_shaped.max_content_size().width;
        assert!(
            (fast_shaped.max_content_size().width - width).abs() <= ADVANCE_TOLERANCE,
            "端到端 max-content 宽度必须一致"
        );
        for byte_index in 0..=text.len() {
            let fast_caret = fast_shaped.caret_rect(width, TextAlign::Start, byte_index);
            let parley_caret = parley_shaped.caret_rect(width, TextAlign::Start, byte_index);
            assert!(
                (fast_caret.origin.x - parley_caret.origin.x).abs() <= ADVANCE_TOLERANCE,
                "byte {byte_index}: caret x {} vs {}",
                fast_caret.origin.x,
                parley_caret.origin.x
            );
            assert!(
                (fast_caret.size.height - parley_caret.size.height).abs() <= ADVANCE_TOLERANCE,
                "byte {byte_index}: 行高 {} vs {}",
                fast_caret.size.height,
                parley_caret.size.height
            );
        }
    }

    /// 非 ASCII / 含空格 / 超字符集文本必须原样落回 parley。
    #[test]
    fn ineligible_text_falls_back() {
        let (mut font_cx, mut layout_cx, family) = harness();
        if !has_system_fonts(&mut font_cx) {
            return;
        }
        let style = TextStyle::new(12.0);
        let mut fast = DigitFastPath::new();
        for text in ["1 234", "abc", "数字", "1.5 USDT"] {
            assert!(
                fast.try_shape(&mut font_cx, &mut layout_cx, &family, 1, text, &style)
                    .is_none(),
                "{text} must not use the fast path"
            );
        }
    }

    /// 缓存键含字号：不同字号各自验证，互不干扰。
    #[test]
    fn font_tables_are_keyed_by_size() {
        let (mut font_cx, mut layout_cx, family) = harness();
        if !has_system_fonts(&mut font_cx) {
            return;
        }
        let mut fast = DigitFastPath::new();
        let small = TextStyle::new(10.0);
        let large = TextStyle::new(22.0);
        let first = shape_fast(
            &mut fast,
            &mut font_cx,
            &mut layout_cx,
            &family,
            "123",
            &small,
        );
        let second = shape_fast(
            &mut fast,
            &mut font_cx,
            &mut layout_cx,
            &family,
            "123",
            &large,
        );
        assert!(
            second.width() > first.width(),
            "larger font must have larger advances"
        );
        let stats = fast.take_stats();
        assert_eq!(
            (stats.queries, stats.verified),
            (2, 2),
            "不同字号各自验证一次"
        );
        assert_eq!(fast.fonts.len(), 2);
    }

    /// caret / 选区几何：单行、ASCII ⇒ 前缀 advance 与总宽。
    #[test]
    fn caret_and_selection_use_prefix_advances() {
        let (mut font_cx, mut layout_cx, family) = harness();
        if !has_system_fonts(&mut font_cx) {
            return;
        }
        let style = TextStyle::new(12.0).with_wrap(TextWrap::NoWrap);
        let mut fast = DigitFastPath::new();
        let simple = shape_fast(
            &mut fast,
            &mut font_cx,
            &mut layout_cx,
            &family,
            "12345",
            &style,
        );
        assert_eq!(simple.caret_x(0), 0.0);
        assert!((simple.caret_x(5) - simple.width()).abs() <= 1e-4);
        assert!(simple.caret_x(2) > simple.caret_x(1));
        let rect = simple.selection_rect(1, 3).expect("non-empty selection");
        assert!((rect.origin.x - simple.caret_x(1)).abs() <= 1e-4);
        assert!((rect.size.width - (simple.caret_x(3) - simple.caret_x(1))).abs() <= 1e-4);
        assert!(simple.selection_rect(2, 2).is_none(), "collapsed selection");
    }

    /// P1-② 回归：只有 `line_height` 不同时**不得**命中同一条 `FastFont`。
    ///
    /// 原缺陷：旧 `FastKey` 只有 `(font_identity, size, weight, slant)`，`.with_line_height(2.0)`
    /// 的表头先塑形会把 `FastFont{metrics.line_height = 24}` 写进表，随后默认行高的价格文本
    /// 命中同一键、`rasterize` 用 `self.metrics` 画出**表头的 baseline / 行高**。
    #[test]
    fn different_line_heights_never_share_a_font_entry() {
        let (mut font_cx, mut layout_cx, family) = harness();
        if !has_system_fonts(&mut font_cx) {
            return;
        }
        let base = TextStyle::new(12.0).with_wrap(TextWrap::NoWrap);
        let doubled = base.clone().with_line_height(2.0);
        let text = "1,234.56";
        let mut fast = DigitFastPath::new();
        let normal = shape_fast(
            &mut fast,
            &mut font_cx,
            &mut layout_cx,
            &family,
            text,
            &base,
        );
        let tall = shape_fast(
            &mut fast,
            &mut font_cx,
            &mut layout_cx,
            &family,
            text,
            &doubled,
        );
        let stats = fast.take_stats();
        assert_eq!(stats.queries, 2);
        assert_eq!(stats.hits, 2);
        assert_eq!(
            stats.verified, 2,
            "两种行高必须各自验证一次（旧键在这里只会验证 1 次）"
        );
        assert_eq!(stats.rejected, 0);
        assert_eq!(fast.fonts.len(), 2, "两种行高必须是两条验证表项");
        // 行高来自各自的探针塑形，而不是字体的固定值：2 倍行高 == 2 × 字号。
        assert!(
            (tall.metrics().line_height - 2.0 * base.size).abs() <= ADVANCE_TOLERANCE,
            "line_height 必须由 parley 探针实测：{}",
            tall.metrics().line_height
        );
        assert!(
            (normal.metrics().line_height - tall.metrics().line_height).abs() > 1.0,
            "默认行高与 2 倍行高必须不同：{} vs {}",
            normal.metrics().line_height,
            tall.metrics().line_height
        );
        let fonts = ready_fonts(&fast);
        assert_eq!(fonts.len(), 2);
        assert!(
            !Rc::ptr_eq(&fonts[0], &fonts[1]),
            "不同 line_height 不得共享 FastFont（metrics 会串味）"
        );
        for (simple, style) in [(&normal, &base), (&tall, &doubled)] {
            let reference = parley_reference(&mut font_cx, &mut layout_cx, &family, text, style);
            assert_matches_reference(simple, &reference, text);
        }
    }

    /// P1-② 回归：只有 `font-stretch` 不同时不得共享条目。
    ///
    /// `stretch` 通过 `StyleProperty::FontWidth` 参与**字体选择**，可能落到另一个 face ⇒
    /// glyph id 与 advance 都会变（比行高更重的「拿错字体」）。
    ///
    /// 宿主字体不一定有该宽度面：`verify` 的探针拒绝是合法结果（`Unsupported`）。拒绝路径
    /// **不能**更强地断言「advance 不同」—— 拒绝时根本没有 `FastFont` 可比较；能锁住的是
    /// 「两条样式各自成条（不共享）+ 拒绝可观测 + 另一条样式不受影响」。
    #[test]
    fn different_stretches_never_share_a_font_entry() {
        let (mut font_cx, mut layout_cx, family) = harness();
        if !has_system_fonts(&mut font_cx) {
            return;
        }
        let base = TextStyle::new(12.0).with_wrap(TextWrap::NoWrap);
        let condensed = base.clone().with_stretch(75);
        let text = "1,234.56";
        let mut fast = DigitFastPath::new();
        let normal = shape_fast(
            &mut fast,
            &mut font_cx,
            &mut layout_cx,
            &family,
            text,
            &base,
        );
        let stretched = fast.try_shape(&mut font_cx, &mut layout_cx, &family, 7, text, &condensed);
        let stats = fast.take_stats();
        assert_eq!(stats.queries, 2);
        assert_eq!(
            fast.fonts.len(),
            2,
            "两种 stretch 必须是两条验证表项（旧键会串成一条）"
        );
        let base_reference = parley_reference(&mut font_cx, &mut layout_cx, &family, text, &base);
        assert_matches_reference(&normal, &base_reference, text);
        match stretched {
            Some(stretched) => {
                // 宿主字体能提供该宽度面：两条都验证通过，各自与自己的全量参考一致。
                assert_eq!((stats.hits, stats.verified, stats.rejected), (2, 2, 0));
                let fonts = ready_fonts(&fast);
                assert_eq!(fonts.len(), 2);
                assert!(
                    !Rc::ptr_eq(&fonts[0], &fonts[1]),
                    "不同 stretch 不得共享 FastFont"
                );
                let reference =
                    parley_reference(&mut font_cx, &mut layout_cx, &family, text, &condensed);
                assert_matches_reference(&stretched, &reference, text);
            }
            None => {
                // 宿主字体没有该宽度面 ⇒ 探针拒绝、该样式永久降级 parley（可观测）。
                assert_eq!(
                    (stats.hits, stats.verified, stats.rejected),
                    (1, 1, 1),
                    "拒绝必须计入 rejected，且不得影响另一条样式"
                );
            }
        }
    }

    /// P1-② 回归：只有 `font_variations` 不同时不得共享条目。
    ///
    /// 可变字体轴值改变**实例坐标**（`FastFont::variation_coords` 直接喂给光栅），也可能改变
    /// glyph id / advance；共享条目会让价格文本拿到另一个实例的字形与度量。
    ///
    /// 宿主字体不一定是可变字体：探针拒绝（`Unsupported`）是合法结果。拒绝路径不能断言
    /// 「advance 不同」（没有 `FastFont`），只能锁「各自成条 + 拒绝可观测」。
    #[test]
    fn different_font_variations_never_share_a_font_entry() {
        let (mut font_cx, mut layout_cx, family) = harness();
        if !has_system_fonts(&mut font_cx) {
            return;
        }
        let base = TextStyle::new(12.0).with_wrap(TextWrap::NoWrap);
        let heavy = base
            .clone()
            .with_font_variations([TextFontVariation::new(*b"wght", 700.0)]);
        let text = "1,234.56";
        let mut fast = DigitFastPath::new();
        let normal = shape_fast(
            &mut fast,
            &mut font_cx,
            &mut layout_cx,
            &family,
            text,
            &base,
        );
        let varied = fast.try_shape(&mut font_cx, &mut layout_cx, &family, 7, text, &heavy);
        let stats = fast.take_stats();
        assert_eq!(stats.queries, 2);
        assert_eq!(
            fast.fonts.len(),
            2,
            "两种 font_variations 必须是两条验证表项（旧键会串成一条）"
        );
        let base_reference = parley_reference(&mut font_cx, &mut layout_cx, &family, text, &base);
        assert_matches_reference(&normal, &base_reference, text);
        match varied {
            Some(varied) => {
                assert_eq!((stats.hits, stats.verified, stats.rejected), (2, 2, 0));
                let fonts = ready_fonts(&fast);
                assert_eq!(fonts.len(), 2);
                assert!(
                    !Rc::ptr_eq(&fonts[0], &fonts[1]),
                    "不同 font_variations 不得共享 FastFont"
                );
                let reference =
                    parley_reference(&mut font_cx, &mut layout_cx, &family, text, &heavy);
                assert_matches_reference(&varied, &reference, text);
            }
            None => {
                assert_eq!(
                    (stats.hits, stats.verified, stats.rejected),
                    (1, 1, 1),
                    "拒绝必须计入 rejected，且不得影响另一条样式"
                );
            }
        }
    }

    /// P1-② 回归：只有 `locale` 不同时不得共享条目。
    ///
    /// `locale` 影响 `locl` 等语言相关的 GSUB 选择（CJK 字形变体 / 标点宽度）。数字是否真的
    /// 随 locale 变取决于宿主字体，因此这里锁的是「各自成条 + 各自与自己的全量参考一致」，
    /// 而不是「两条输出必须不同」。
    #[test]
    fn different_locales_never_share_a_font_entry() {
        let (mut font_cx, mut layout_cx, family) = harness();
        if !has_system_fonts(&mut font_cx) {
            return;
        }
        let base = TextStyle::new(12.0).with_wrap(TextWrap::NoWrap);
        let localized = base.clone().with_locale("zh-Hans");
        let text = "1,234.56";
        let mut fast = DigitFastPath::new();
        let plain = shape_fast(
            &mut fast,
            &mut font_cx,
            &mut layout_cx,
            &family,
            text,
            &base,
        );
        let zh = shape_fast(
            &mut fast,
            &mut font_cx,
            &mut layout_cx,
            &family,
            text,
            &localized,
        );
        let stats = fast.take_stats();
        assert_eq!(
            (stats.queries, stats.hits, stats.verified, stats.rejected),
            (2, 2, 2, 0),
            "两种 locale 必须各自验证一次并各自命中"
        );
        assert_eq!(fast.fonts.len(), 2, "locale 必须参与 FastKey");
        let fonts = ready_fonts(&fast);
        assert_eq!(fonts.len(), 2);
        assert!(
            !Rc::ptr_eq(&fonts[0], &fonts[1]),
            "不同 locale 不得共享 FastFont"
        );
        for (simple, style) in [(&plain, &base), (&zh, &localized)] {
            let reference = parley_reference(&mut font_cx, &mut layout_cx, &family, text, style);
            assert_matches_reference(simple, &reference, text);
        }
    }
}
