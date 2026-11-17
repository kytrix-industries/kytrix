//! 形状：`parley` 的唯一调用点（`design/phase-05-text.md` §7）。
//!
//! [`ShapedText`] 是一次 shape 的**不可变快照**，被 `Rc` 共享。三条关键契约：
//!
//! 1. **逻辑坐标 shaping**（R12-4）：全部布局量与 parley 输入都是逻辑像素（`range_builder` 的
//!    `scale = 1.0`），设备 scale 只在光栅/落点阶段相乘。于是「DPI 变化」不再清空形状缓存，
//!    光栅期按 `ppem = 逻辑字号 × scale` 开新的 strike（`GlyphKey` 已含 ppem 维）。
//!    100% DPI 下逻辑 == 物理，像素与改造前逐位一致；非整数 DPI 下竖直度量按逻辑整像素量化
//!    再乘 scale 取整（`rasterize` 的 `y.round()`），与旧实现（parley 在物理坐标上量化）最多
//!    差 0.5 物理像素 —— 这是「逻辑坐标布局 + 光栅期取整」的规范结果（Blink/Qt 同构）。
//! 2. **零克隆**（R12-5）：`measure` / `layout_at` / 选区几何都在**同一份** `Layout` 上就地
//!    重排（`break_all_lines` 可从已塑形的数据重跑，parley 的 `BreakLines::new` 会
//!    `lines.clear()`）。快照不再为「按宽度断行」clone 整个 `Layout`，也不再常驻第二份
//!    `broken`（R12-6）。min/max-content 尺寸在 shape 期实测一次，`measure` 是 O(1)。
//! 3. **缓存键只含 shaping 输入**（R12-4/R12-11）：`scale` / `color` / `alignment` 都不在键里；
//!    颜色由光栅期传入，对齐由 `break_and_align` 现场应用。同一文本的红/绿两态共享一份快照。
//!
//! 相位与 ppem 都取**物理**量：`glyph.x` 是逻辑坐标，光栅时先 `× scale` 再
//! `raster::split_subpixel_x` 拆相位；`ppem = run.font_size() × scale`（保留小数）。
//!
//! # 溢出与行数上限（R12-9）
//!
//! `TextStyle::{max_lines, overflow}` 在光栅之后按 CSS 语义应用：`max_lines` 截掉多余行，
//! `overflow: Ellipsis` 在**最后一个可见行**上按「保留前缀 + 省略号」重排（省略号的 advance
//! 计入可用宽度，与 Chrome `LayoutNG` / Flutter `TextOverflow.ellipsis` 一致）。
//!
//! **布局尺寸与绘制同源**（P1-④）：`measure` 返回的高度走**同一套 clamp**
//! （`Σ_{i < min(max_lines, 行数)} line_height_i`，见 [`ShapedText::measure`]），
//! 因此 taffy 拿到的尺寸 == `rasterize` 产出的 `ShapedLine::height`，不会按全文行数留白。

use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
};

use parley::{
    FontContext, FontStyle as ParleyFontStyle, FontWeight as ParleyFontWeight, LayoutContext,
    LineHeight as ParleyLineHeight, OverflowWrap as ParleyOverflowWrap, PositionedLayoutItem,
    StyleProperty, TextWrapMode as ParleyTextWrapMode, WordBreak as ParleyWordBreak,
    layout::{Alignment, AlignmentOptions},
};
use smol_str::{SmolStr, format_smolstr};
use taffy::AvailableSpace;

use super::{
    InlineBoxSpec, ShapedGlyphRun, ShapedInlineBox, ShapedLine, ShapedTextItem, ShapedTextLine,
    TextAlign, TextContent, TextDiagnostic, TextDirection, TextLineBreak, TextOverflow, TextSlant,
    TextStyle, TextWrap,
    atlas::{GlyphAtlas, GlyphAtlasStats},
    fast_path::SimpleShaped,
    inline_flow,
    raster::{GlyphRasterizer, GlyphVariation, split_subpixel_x},
};
use crate::{
    error::EngineError,
    math::{Point, Rect, Size},
    paint::{
        color::{Color, ColorBrush},
        primitive::PositionedGlyph,
    },
    render::atlas::AtlasAllocator,
};

/// 省略号的字形（`TextContext` 按样式缓存；`rasterize` 用它补最后一行）。
///
/// 省略号**必须参与 shaping**（它的 advance 要计入可用宽度）：这里保存的是已 shape 的
/// `(glyph id, advance)` 序列（正常 1 个 U+2026；字体缺该字形时退成 3 个 `.`）。
#[derive(Clone, Debug)]
pub(crate) struct EllipsisGlyphs {
    /// 字形 id 与各自的 advance（逻辑像素）。
    pub glyphs: Vec<(u32, f32)>,
    /// 字形所属字体（光栅 / 图集键）。
    pub font: parley::FontData,
    /// 可变字体实例坐标（16.16 定点）。
    pub variation_coords: Vec<i16>,
    /// 合成样式（faux bold）。
    pub synthesis: fontique::Synthesis,
    /// 总 advance（逻辑像素）。
    pub advance: f32,
}

impl EllipsisGlyphs {
    /// 是否为空（没 shape 出任何字形）。
    pub(crate) fn is_empty(&self) -> bool {
        self.glyphs.is_empty()
    }
}

/// 光栅一期需要的全部可变状态（图集 + 光栅器 + 代际 + 设备 scale）。
pub(crate) struct RasterCtx<'a> {
    /// 逐字形光栅器（`ScaleContext` 复用）。
    pub raster: &'a mut GlyphRasterizer,
    /// 字形 tile 缓存。
    pub glyph_atlas: &'a mut GlyphAtlas,
    /// 图集分配器（装箱 + 脏行 + 代际）。
    pub atlas: &'a mut AtlasAllocator,
    /// 本次 pass 的图集代际。
    pub generation: u32,
    /// 当前窗口的设备 scale（逻辑 → 物理）。
    pub scale: f32,
}

/// 一次 shape 的不可变快照。
pub(crate) struct ShapedText {
    text: SmolStr,
    /// 生成快照时的样式（**只保留 shaping 输入**：颜色/对齐每次使用现场给）。
    style: TextStyle,
    font_stack_version: u32,
    kind: ShapedKind,
    /// 行内盒的边界/超宽诊断（shape 期间产生，随 `rasterize` 交给 `TextContext`）。
    inline_diagnostics: Vec<TextDiagnostic>,
    inline_boxes: Box<[InlineBoxSpec]>,
    /// `Definite` 宽度的单条 memo：taffy 会对同一节点反复探测同一宽度。
    measure_memo: RefCell<Option<(u32, Size)>>,
    /// 诊断/测试：`measure` 真正触发断行的次数（O(1) 路径必须不增长）。
    measure_breaks: Cell<u32>,
}

enum ShapedKind {
    /// parley 塑形：一份可就地重排的 `Layout` + shape 期实测的 min/max 尺寸。
    Parley(RefCell<ParleySnapshot>),
    /// 数字快速路径（`fast_path`）：单行、无 parley 布局。
    Simple(SimpleShaped),
}

/// parley 快照 + 断行状态（`break_all_lines` 可反复调用，见模块文档）。
struct ParleySnapshot {
    layout: parley::Layout<ColorBrush>,
    /// `widths.min` 处实测的尺寸（min-content）。
    min_size: Size,
    /// max-content 实测的尺寸。
    max_size: Size,
    /// 当前断行宽度（`None` = max-content）。
    broken_at: Option<u32>,
    /// 当前应用的对齐（`None` = 尚未对齐）。
    aligned_at: Option<u8>,
}

impl ParleySnapshot {
    /// 建快照：max-content 断一次、`widths.min` 再断一次（各记录实测尺寸），最后回到 max-content。
    ///
    /// `max_lines` 来自 shape 期的样式（同一快照的 `style` 不变）：两处 `height` 直接存
    /// **钳制后**的可见高度，于是 `measure` 的 Min/MaxContent 只需读字段（O(1)）。
    fn new(mut layout: parley::Layout<ColorBrush>, max_lines: Option<u16>) -> Self {
        layout.break_all_lines(None);
        let max_size = Size::new(layout.width(), visible_height(&layout, max_lines));
        let widths = layout.calculate_content_widths();
        layout.break_all_lines(Some(widths.min));
        let min_size = Size::new(layout.width(), visible_height(&layout, max_lines));
        layout.break_all_lines(None);
        Self {
            layout,
            min_size,
            max_size,
            broken_at: None,
            aligned_at: None,
        }
    }

    /// 就地断行 + 对齐，返回内容宽（逻辑像素）。
    ///
    /// **不返回高度**：`measure` 要的是按 `max_lines` 钳制后的可见高度（[`visible_height`]），
    /// 而 `rasterize` 的高度由 `apply_overflow` 在截断后重算 —— 两个消费者都不需要这里的
    /// 原始 `Layout::height()`，返回它只会招来一个与 clamp 脱钩的读者。
    fn break_and_align(&mut self, width: Option<f32>, alignment: Alignment) -> f32 {
        let key = width.map(f32::to_bits);
        if self.broken_at != key {
            self.layout.break_all_lines(width);
            self.broken_at = key;
            self.aligned_at = None;
        }
        let align_key = alignment_key(alignment);
        if self.aligned_at != Some(align_key) {
            self.layout.align(alignment, AlignmentOptions::default());
            self.aligned_at = Some(align_key);
        }
        self.layout.width()
    }

    /// 当前断行状态下的可见高度（与 `min_size` / `max_size` 用的是同一公式）。
    fn visible_height(&self, max_lines: Option<u16>) -> f32 {
        visible_height(&self.layout, max_lines)
    }
}

/// `max_lines` 钳制后的可见高度：`Σ_{i < min(max_lines, 行数)} line_height_i`（逻辑像素）。
///
/// 与 `rasterize` → `apply_overflow` 的 clamp **同构**：那里先 `lines.truncate(max_lines)`、
/// 再对保留行的 `line_height` 求和；这里对同一份 parley 布局的前 N 行求和。行高取每行真实的
/// `LineMetrics::line_height` —— 行内盒 / 混排字体让每行高度不同，不能用「可见行数 × 单一
/// line_height」。
///
/// 不截断（`max_lines` 为 `None` 或 ≥ 行数）时返回 parley 的 `Layout::height()`：保持既有
/// measure 语义（f64 累加、且不计「文本末尾强制换行产生的空行」），也避免多扫一遍行。
fn visible_height(layout: &parley::Layout<ColorBrush>, max_lines: Option<u16>) -> f32 {
    let Some(max_lines) = max_lines else {
        return layout.height();
    };
    let lines = layout.lines();
    if lines.len() <= usize::from(max_lines) {
        return layout.height();
    }
    lines
        .take(usize::from(max_lines))
        .map(|line| line.metrics().line_height)
        .sum()
}

impl ShapedText {
    /// 跑一次 `RangedBuilder`（样式/文本变化才调用；宽度变化走就地重排）。
    ///
    /// 坐标是**逻辑像素**：`ranged_builder(..., 1.0, true)`（`quantize` 让行盒吸附到逻辑整像素，
    /// 与旧实现在 100% DPI 下逐位一致）。
    pub(crate) fn shape(
        font_cx: &mut FontContext,
        layout_cx: &mut LayoutContext<ColorBrush>,
        family: &parley::FontFamily<'static>,
        content: &TextContent,
        font_stack_version: u32,
    ) -> Self {
        let style = &content.style;
        let mut inline_diagnostics: Vec<TextDiagnostic> = Vec::new();
        let mut builder = layout_cx.ranged_builder(font_cx, &content.text, 1.0, true);
        builder.set_base_direction(match style.direction {
            TextDirection::Auto => parley::BaseDirection::Auto,
            TextDirection::Ltr => parley::BaseDirection::Ltr,
            TextDirection::Rtl => parley::BaseDirection::Rtl,
        });
        builder.push_default(StyleProperty::FontFamily(family.clone()));
        push_shaping_defaults(&mut builder, style);
        for spec in &content.inline_boxes {
            let (inline_box, diagnostic) = inline_flow::to_inline_box(spec, &content.text);
            if let Some(diagnostic) = diagnostic {
                inline_diagnostics.push(diagnostic);
            }
            builder.push_inline_box(inline_box);
        }
        let layout = builder.build(&content.text);
        Self {
            text: content.text.clone(),
            style: style.clone(),
            font_stack_version,
            kind: ShapedKind::Parley(RefCell::new(ParleySnapshot::new(layout, style.max_lines()))),
            inline_diagnostics,
            inline_boxes: content.inline_boxes.clone(),
            measure_memo: RefCell::new(None),
            measure_breaks: Cell::new(0),
        }
    }

    /// 由数字快速路径的结果建快照（单行）。
    pub(crate) fn simple(
        content: &TextContent,
        simple: SimpleShaped,
        font_stack_version: u32,
    ) -> Self {
        Self {
            text: content.text.clone(),
            style: content.style.clone(),
            font_stack_version,
            kind: ShapedKind::Simple(simple),
            inline_diagnostics: Vec::new(),
            inline_boxes: content.inline_boxes.clone(),
            measure_memo: RefCell::new(None),
            measure_breaks: Cell::new(0),
        }
    }

    pub(crate) fn inline_boxes(&self) -> &[InlineBoxSpec] {
        &self.inline_boxes
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }
    #[cfg(test)]
    pub(crate) fn style(&self) -> &TextStyle {
        &self.style
    }

    /// 该快照的 shaping 输入是否与 `style` 等价（`color`/`alignment` 不参与形状）。
    ///
    /// 缓存命中后仍要逐字段比较（防哈希碰撞）：比较必须用**与缓存键相同的等价关系**，
    /// 否则「同键不同色」会被判成 miss，缓存形同虚设。
    pub(crate) fn same_shaping_inputs(&self, style: &TextStyle) -> bool {
        self.style.same_shaping_inputs(style)
    }

    /// max-content 尺寸（逻辑像素）。
    ///
    /// **宽度**是全文不折行时的内容宽（`layout_text` 在「无内容宽」时拿它当回退宽度，
    /// 只读 `.width`）；**高度**已按 `max_lines` 钳制 —— 它与 `measure(MaxContent)` 读的是
    /// 同一份 `max_size` 字段，两者必须同语义，否则 taffy 与实际绘制会再次脱钩。
    pub(crate) fn max_content_size(&self) -> Size {
        match &self.kind {
            ShapedKind::Parley(snapshot) => snapshot.borrow().max_size,
            ShapedKind::Simple(simple) => simple.size(),
        }
    }

    /// 按 taffy 语义测量（**零克隆**：Min/MaxContent 直接返回 shape 期实测值）。
    ///
    /// 返回值已应用与 `rasterize` → `apply_overflow` **同一套 clamp**（R12-9 / P1-④）：
    ///
    /// - **宽度**：`max_lines` / `overflow: Ellipsis` 都不改变宽度上界。省略号只是在最后一个
    ///   可见行内做「保留前缀 + 省略号」的字形替换（`clip_with_ellipsis`），不会让内容宽超过
    ///   原来的断行结果。Min/MaxContent 用 shape 期实测的内容宽，Definite 用该宽度下的断行宽。
    /// - **高度**：`Σ_{i < min(max_lines, 行数)} line_height_i`（`max_lines` 为 `None` 时即全文）。
    ///   行高取 parley 每行的 `LineMetrics::line_height` —— 行内盒 / 混排字体让各行高度不同，
    ///   不能用「可见行数 × 单一 line_height」。该定义与 `apply_overflow` 末尾对截断后
    ///   `Σ line.line_height` 的求和逐位同构，故 taffy 尺寸 == `ShapedLine::height`。
    /// - `ShapedKind::Simple`（数字快路径）恒为**单行**：`TextStyle::max_lines()` 已把 0 映射成
    ///   `None`，因此 `max_lines >= 1` ⇒ 可见行数 = 1 ⇒ 不截断，也永远不会把单行钳成 0；
    ///   该路径直接返回 `simple.size()`，不经过任何 clamp 代码。
    ///
    /// O(1) 论证：Min/MaxContent 的钳制后尺寸在 shape 期（`ParleySnapshot::new` 的两次断行之后）
    /// 算好并存进 `min_size`/`max_size`，`measure` 只读字段；Definite 路径本来就要断行（且按
    /// 宽度 memo），钳制高度直接取**刚断完行的前 N 行**，不产生额外断行、也不做逐字符扫描。
    /// 因此 taffy 的高频探测不会引入重复塑形（`measure_breaks` 计数器可证）。
    ///
    /// Definite 的 memo 只按宽度缓存：`max_lines` 属于 `ShapeKey` 的一部分，同一 `ShapedText`
    /// 的 style 不会变，故缓存键不含它是正确的；memo 里存的就是**已钳制**的返回值。
    pub(crate) fn measure(
        &self,
        _known: taffy::Size<Option<f32>>,
        available: taffy::Size<AvailableSpace>,
    ) -> Size {
        if self.text.is_empty() {
            return Size::ZERO;
        }
        let max_lines = self.style.max_lines();
        match &self.kind {
            ShapedKind::Simple(simple) => simple.size(),
            ShapedKind::Parley(snapshot) => match available.width {
                AvailableSpace::MinContent => snapshot.borrow().min_size,
                AvailableSpace::MaxContent => snapshot.borrow().max_size,
                AvailableSpace::Definite(width) => {
                    let width = width.max(0.0);
                    let bits = width.to_bits();
                    if let Some((cached, size)) = *self.measure_memo.borrow()
                        && cached == bits
                    {
                        return size;
                    }
                    let mut snapshot = snapshot.borrow_mut();
                    let size = if width >= snapshot.max_size.width {
                        snapshot.max_size
                    } else if width <= snapshot.min_size.width {
                        snapshot.min_size
                    } else {
                        self.measure_breaks.set(self.measure_breaks.get() + 1);
                        let measured_width =
                            snapshot.break_and_align(Some(width), Alignment::Start);
                        Size::new(measured_width, snapshot.visible_height(max_lines))
                    };
                    *self.measure_memo.borrow_mut() = Some((bits, size));
                    size
                }
            },
        }
    }

    /// 字节估算（形状缓存的 LRU 预算；只用于逐出决策，不做精确记账）。
    ///
    /// **按字形计**，不是按 run 计：一个 run 可以含成百上千个字形，按 run 计会把长文本的
    /// 占用低估 10–100 倍，预算形同虚设。计费口径是「缓存驻留」，因此宁可**高估**。
    pub(crate) fn approx_bytes(&self) -> usize {
        let (glyphs, runs) = match &self.kind {
            ShapedKind::Parley(snapshot) => {
                let snapshot = snapshot.borrow();
                let mut glyphs = 0usize;
                let mut runs = 0usize;
                for line in snapshot.layout.lines() {
                    for item in line.items() {
                        if let parley::PositionedLayoutItem::GlyphRun(run) = item {
                            runs += 1;
                            glyphs += run.glyphs().count();
                        }
                    }
                }
                (glyphs, runs)
            }
            ShapedKind::Simple(simple) => (simple.text_len(), 1),
        };
        let inline_boxes = core::mem::size_of_val(self.inline_boxes.as_ref());
        let inline_diagnostics = self.inline_diagnostics.capacity() * 64;
        let extras = self
            .style
            .extras
            .as_deref()
            .map_or(0, core::mem::size_of_val);
        core::mem::size_of::<Self>()
            + self.text.len()
            + glyphs * 24
            + runs * 96
            + inline_boxes
            + inline_diagnostics
            + extras
    }

    /// 光栅 + 装箱，产出只读快照（`content_width` 是 taffy 算出的内容宽，逻辑像素）。
    ///
    /// `color`/`alignment` 由当前内容与样式现场给出（不在形状缓存键里）。`ellipsis` 只在
    /// `style.overflow == Ellipsis` 时使用。
    pub(crate) fn rasterize(
        &self,
        content_width: f32,
        alignment: TextAlign,
        color: Color,
        ctx: &mut RasterCtx<'_>,
        ellipsis: Option<&EllipsisGlyphs>,
    ) -> Result<(ShapedLine, Vec<TextDiagnostic>, GlyphAtlasStats), EngineError> {
        if self.text.is_empty() {
            return Ok((
                ShapedLine::empty(
                    self.text.clone(),
                    ctx.generation,
                    self.font_stack_version,
                    ctx.scale,
                ),
                Vec::new(),
                ctx.glyph_atlas.take_stats(),
            ));
        }
        let (mut shaped, diagnostics) = match &self.kind {
            ShapedKind::Simple(simple) => {
                let shaped = simple.rasterize(self.text.clone(), &self.style, color, ctx)?;
                (shaped, Vec::new())
            }
            ShapedKind::Parley(snapshot) => {
                let mut snapshot = snapshot.borrow_mut();
                let width =
                    snapshot.break_and_align(Some(content_width.max(0.0)), align_of(alignment));
                let (shaped, diagnostics) = rasterize_parley(
                    &snapshot.layout,
                    width,
                    &self.text,
                    color,
                    ctx,
                    &self.inline_diagnostics,
                )?;
                (shaped, diagnostics)
            }
        };
        apply_overflow(
            &mut shaped,
            &self.style,
            content_width,
            color,
            ctx,
            ellipsis,
        )?;
        Ok((shaped, diagnostics, ctx.glyph_atlas.take_stats()))
    }

    /// 让快照进入「按 `width` 断行 + `alignment` 对齐」的状态而不产出像素（选区/光标用）。
    fn with_broken_layout<R>(
        &self,
        width: f32,
        alignment: TextAlign,
        body: impl FnOnce(&parley::Layout<ColorBrush>) -> R,
    ) -> Option<R> {
        match &self.kind {
            ShapedKind::Parley(snapshot) => {
                let mut snapshot = snapshot.borrow_mut();
                snapshot.break_and_align(Some(width.max(0.0)), align_of(alignment));
                Some(body(&snapshot.layout))
            }
            ShapedKind::Simple(_) => None,
        }
    }

    /// `(anchor, focus)` → 文本盒局部的选区矩形（逻辑像素）。
    pub(crate) fn selection_rects(
        &self,
        width: f32,
        alignment: TextAlign,
        anchor: usize,
        focus: usize,
    ) -> Vec<Rect> {
        if let ShapedKind::Simple(simple) = &self.kind {
            return simple.selection_rect(anchor, focus).into_iter().collect();
        }
        self.with_broken_layout(width, alignment, |layout| {
            inline_flow::selection_rects(layout, anchor, focus)
        })
        .unwrap_or_default()
    }

    /// `byte_index` 处的 caret 矩形（文本盒局部逻辑像素，宽 1 逻辑像素）。
    pub(crate) fn caret_rect(&self, width: f32, alignment: TextAlign, byte_index: usize) -> Rect {
        if let ShapedKind::Simple(simple) = &self.kind {
            let metrics = simple.metrics();
            return Rect::new(simple.caret_x(byte_index), 0.0, 1.0, metrics.line_height);
        }
        self.with_broken_layout(width, alignment, |layout| {
            inline_flow::caret_rect(layout, byte_index)
        })
        .unwrap_or_default()
    }

    /// 仅测试：当前快照的 parley 布局（共享借用；调用期间不得再触发 `measure`/`rasterize`）。
    #[cfg(test)]
    pub(crate) fn layout(&self) -> std::cell::Ref<'_, parley::Layout<ColorBrush>> {
        std::cell::Ref::map(
            match &self.kind {
                ShapedKind::Parley(snapshot) => snapshot.borrow(),
                ShapedKind::Simple(_) => panic!("simple fast-path snapshots have no parley layout"),
            },
            |snapshot| &snapshot.layout,
        )
    }

    /// 仅测试：按指定宽度断行后的 parley 布局（**clone**，测试专用）。
    #[cfg(test)]
    pub(crate) fn layout_at(&self, width: f32) -> parley::Layout<ColorBrush> {
        match &self.kind {
            ShapedKind::Parley(snapshot) => {
                let mut snapshot = snapshot.borrow_mut();
                snapshot.break_and_align(Some(width.max(0.0)), Alignment::Start);
                snapshot.layout.clone()
            }
            ShapedKind::Simple(_) => panic!("simple fast-path snapshots have no parley layout"),
        }
    }

    /// 仅测试：`measure` 真正触发断行的次数（O(1) 路径必须不增长）。
    #[cfg(test)]
    pub(crate) fn measure_breaks(&self) -> u32 {
        self.measure_breaks.get()
    }

    /// 仅测试：在指定宽度下重排后第一个字形的逻辑 x（验证 `align` 的真实效果）。
    #[cfg(test)]
    pub(crate) fn first_glyph_x_at(&self, width: f32) -> f32 {
        let mut layout = self.layout_at(width);
        layout.align(align_of(self.style.alignment), AlignmentOptions::default());
        layout
            .lines()
            .next()
            .and_then(|line| line.items().next())
            .and_then(|item| match item {
                PositionedLayoutItem::GlyphRun(run) => {
                    run.positioned_glyphs().next().map(|glyph| glyph.x)
                }
                PositionedLayoutItem::InlineBox(_) => None,
            })
            .unwrap_or(0.0)
    }
}

/// 把 shaping 相关的默认样式压进 builder（production 与快速路径的探针塑形共用，
/// 保证「探针 == 生产」）。
pub(super) fn push_shaping_defaults<'a>(
    builder: &mut parley::RangedBuilder<'a, ColorBrush>,
    style: &TextStyle,
) {
    builder.push_default(StyleProperty::FontSize(style.size));
    builder.push_default(StyleProperty::FontWeight(ParleyFontWeight::new(f32::from(
        style.weight,
    ))));
    builder.push_default(StyleProperty::FontStyle(match style.slant {
        TextSlant::Normal => ParleyFontStyle::Normal,
        TextSlant::Italic => ParleyFontStyle::Italic,
        TextSlant::Oblique => ParleyFontStyle::Oblique(None),
    }));
    builder.push_default(StyleProperty::LetterSpacing(style.letter_spacing));
    builder.push_default(StyleProperty::LineHeight(match style.line_height {
        None => ParleyLineHeight::NORMAL,
        Some(factor) => ParleyLineHeight::FontSizeRelative(factor),
    }));
    if let Some(features) = style.font_features() {
        let features: Vec<parley::FontFeature> = features
            .iter()
            .map(|feature| {
                parley::FontFeature::new(
                    parley::setting::Tag::from_bytes(feature.tag),
                    feature.value,
                )
            })
            .collect();
        builder.push_default(StyleProperty::FontFeatures(parley::FontFeatures::List(
            Cow::Owned(features),
        )));
    }
    if let Some(stretch) = style.stretch() {
        builder.push_default(StyleProperty::FontWidth(
            parley::FontWidth::from_percentage(f32::from(stretch)),
        ));
    }
    if let Some(word_spacing) = style.word_spacing() {
        builder.push_default(StyleProperty::WordSpacing(word_spacing));
    }
    if let Some(locale) = style.locale()
        && let Ok(language) = parley::Language::parse(locale)
    {
        builder.push_default(StyleProperty::Locale(Some(language)));
    }
    if let Some(variations) = style.font_variations() {
        let variations: Vec<parley::FontVariation> = variations
            .iter()
            .map(|variation| parley::FontVariation {
                tag: parley::setting::Tag::from_bytes(variation.tag),
                value: variation.value,
            })
            .collect();
        builder.push_default(StyleProperty::FontVariations(parley::FontVariations::List(
            Cow::Owned(variations),
        )));
    }
    if let Some(line_break) = style.line_break() {
        builder.push_default(StyleProperty::LineBreak(parley_line_break(line_break)));
    }
    builder.push_default(StyleProperty::WhiteSpaceCollapse(
        parley::WhiteSpaceCollapse::Preserve,
    ));
    match style.wrap {
        TextWrap::Wrap => {}
        TextWrap::NoWrap => {
            builder.push_default(StyleProperty::TextWrapMode(ParleyTextWrapMode::NoWrap));
        }
        TextWrap::Anywhere => {
            builder.push_default(StyleProperty::WordBreak(ParleyWordBreak::BreakAll));
            builder.push_default(StyleProperty::OverflowWrap(ParleyOverflowWrap::Anywhere));
        }
    }
}

/// 塑形一个省略号字形序列（R12-9）：优先 `…`（U+2026），字体缺该字形时退成 `...`。
///
/// 返回的 advance 是**已含字间距/特性**的实测值（`Glyph::advance`），因此「前缀 + 省略号 ≤
/// 可用宽度」的判定与最终绘制一致。行度量取主文本的样式（省略号只借字形，不建自己的行）。
pub(crate) fn measure_ellipsis(
    font_cx: &mut FontContext,
    layout_cx: &mut LayoutContext<ColorBrush>,
    family: &parley::FontFamily<'static>,
    style: &TextStyle,
) -> Option<EllipsisGlyphs> {
    for text in ["\u{2026}", "..."] {
        let mut builder = layout_cx.ranged_builder(font_cx, text, 1.0, true);
        builder.set_base_direction(parley::BaseDirection::Ltr);
        builder.push_default(StyleProperty::FontFamily(family.clone()));
        push_shaping_defaults(&mut builder, style);
        let mut layout = builder.build(text);
        layout.break_all_lines(None);
        let mut glyphs: Vec<(u32, f32)> = Vec::new();
        let mut font: Option<parley::FontData> = None;
        let mut variation_coords: Vec<i16> = Vec::new();
        let mut synthesis = fontique::Synthesis::default();
        let mut advance = 0.0f32;
        let mut missing = false;
        for line in layout.lines() {
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(run) = item else {
                    continue;
                };
                if font.is_none() {
                    font = Some(run.run().font().clone());
                    variation_coords = run
                        .run()
                        .normalized_coords()
                        .iter()
                        .map(|coord| coord.to_bits())
                        .collect();
                    synthesis = run.run().synthesis();
                }
                for glyph in run.positioned_glyphs() {
                    if glyph.id == 0 {
                        missing = true;
                        break;
                    }
                    glyphs.push((glyph.id, glyph.advance));
                    advance += glyph.advance;
                }
            }
            if missing {
                break;
            }
        }
        if !missing && !glyphs.is_empty() {
            return Some(EllipsisGlyphs {
                glyphs,
                font: font?,
                variation_coords,
                synthesis,
                advance,
            });
        }
    }
    None
}

/// `TextLineBreak` → parley `LineBreak`（1:1 的三个严格度；`Anywhere` 不暴露——它等价于
/// `TextWrap::Anywhere` 已经表达的语义）。
fn parley_line_break(kind: TextLineBreak) -> parley::LineBreak {
    match kind {
        TextLineBreak::Loose => parley::LineBreak::Loose,
        TextLineBreak::Normal => parley::LineBreak::Normal,
        TextLineBreak::Strict => parley::LineBreak::Strict,
    }
}

/// 光栅一份已就位的 parley 布局。
fn rasterize_parley(
    layout: &parley::Layout<ColorBrush>,
    content_width: f32,
    text: &SmolStr,
    color: Color,
    ctx: &mut RasterCtx<'_>,
    inline_diagnostics: &[TextDiagnostic],
) -> Result<(ShapedLine, Vec<TextDiagnostic>), EngineError> {
    let scale = ctx.scale;
    let mut diagnostics: Vec<TextDiagnostic> = inline_diagnostics.to_vec();
    let mut lines = Vec::with_capacity(layout.lines().len());
    for line in layout.lines() {
        let metrics = line.metrics();
        let mut items = Vec::new();
        for item in line.items() {
            match item {
                PositionedLayoutItem::GlyphRun(run) => {
                    let font = run.run().font().clone();
                    // 光栅 ppem = parley 的 `Run::font_size()`（逻辑字号）× 设备 scale，
                    // **保留小数**：14 px @125% → 17.5 ppem。
                    let ppem = (run.run().font_size() * scale).clamp(1.0, 512.0);
                    let text_range = run.run().text_range();
                    if run.run().synthesis().any() {
                        diagnostics.push(TextDiagnostic::SyntheticStyle {
                            text_range: text_range.clone(),
                        });
                    }
                    let normalized_coords: Vec<i16> = run
                        .run()
                        .normalized_coords()
                        .iter()
                        .map(|coord| coord.to_bits())
                        .collect();
                    let variation =
                        GlyphVariation::of(&normalized_coords, run.run().synthesis(), ppem);
                    let mut glyphs = Vec::new();
                    for glyph in run.positioned_glyphs() {
                        if glyph.id == 0 {
                            diagnostics.push(TextDiagnostic::MissingGlyph {
                                font: font_label(&font),
                                glyph: glyph.id,
                                text_range: text_range.clone(),
                            });
                            continue;
                        }
                        let Some((positioned, color_glyph)) =
                            emit_glyph(ctx, &font, glyph.id, ppem, &variation, glyph.x, glyph.y)?
                        else {
                            continue;
                        };
                        if color_glyph {
                            // R12-8：彩色字形被压成单色覆盖度（引擎只有 R8 mask 图集）。
                            // 不静默：这条诊断让「emoji 变灰」可观测。
                            diagnostics.push(TextDiagnostic::ColorGlyphMonochrome {
                                font: font_label(&font),
                                glyph: glyph.id,
                                text_range: text_range.clone(),
                            });
                        }
                        glyphs.push(positioned);
                    }
                    items.push(ShapedTextItem::GlyphRun(ShapedGlyphRun {
                        color,
                        text_range,
                        glyphs,
                    }));
                }
                PositionedLayoutItem::InlineBox(box_) => {
                    let rect = Rect::new(box_.x, box_.y, box_.width, box_.height);
                    if rect.size.width > content_width && content_width > 0.0 {
                        diagnostics.push(TextDiagnostic::InlineBoxOverflow {
                            id: box_.id,
                            width: rect.size.width,
                            available: content_width,
                        });
                    }
                    items.push(ShapedTextItem::InlineBox(ShapedInlineBox {
                        id: box_.id,
                        rect,
                        baseline: box_.baseline,
                    }));
                }
            }
        }
        lines.push(ShapedTextLine {
            text_range: line.text_range(),
            baseline: metrics.baseline,
            line_height: metrics.line_height,
            advance: metrics.advance,
            items,
        });
    }
    let shaped = ShapedLine {
        text: text.clone(),
        lines,
        width: layout.width(),
        height: layout.height(),
        first_baseline: layout.first_baseline(),
        atlas_generation: ctx.generation,
        font_stack_version: 0,
        scale,
    };
    Ok((shaped, diagnostics))
}

/// 一个字形进入图集并换算成逻辑落点。逻辑 x/y → 物理 → 相位/整像素。
///
/// 第二个返回值 = 该字形来自彩色源（R12-8，排版侧据此记诊断）。
fn emit_glyph(
    ctx: &mut RasterCtx<'_>,
    font: &parley::FontData,
    glyph_id: u32,
    ppem: f32,
    variation: &GlyphVariation,
    logical_x: f32,
    logical_y: f32,
) -> Result<Option<(PositionedGlyph, bool)>, EngineError> {
    let scale = ctx.scale;
    let (base_x, subpixel_x) = split_subpixel_x(logical_x * scale);
    let tile = ctx.glyph_atlas.ensure(
        ctx.raster,
        ctx.atlas,
        font,
        glyph_id,
        ppem,
        subpixel_x,
        Some(variation),
    )?;
    let Some(tile) = tile else {
        // 空白/无轮廓：不产生像素，advance 已由排版推进。
        return Ok(None);
    };
    let origin_x = base_x + tile.left as f32;
    let origin_y = (logical_y * scale).round() - tile.top as f32;
    Ok(Some((
        PositionedGlyph {
            tile: tile.tile,
            position: Point::new(origin_x / scale, origin_y / scale),
            size: Size::new(tile.width as f32 / scale, tile.height as f32 / scale),
        },
        tile.color,
    )))
}

/// 应用 `max_lines` / `text-overflow`（CSS 语义；见模块文档）。
fn apply_overflow(
    shaped: &mut ShapedLine,
    style: &TextStyle,
    content_width: f32,
    color: Color,
    ctx: &mut RasterCtx<'_>,
    ellipsis: Option<&EllipsisGlyphs>,
) -> Result<(), EngineError> {
    if shaped.lines.is_empty() {
        return Ok(());
    }
    let max_lines = style.max_lines().map_or(shaped.lines.len(), |max| {
        (max as usize).max(1).min(shaped.lines.len())
    });
    let truncated = max_lines < shaped.lines.len();
    if truncated {
        shaped.lines.truncate(max_lines);
    }
    let mut overflowing = truncated;
    if content_width > 0.0 {
        overflowing |= shaped.width > content_width + 1e-3;
    }
    if overflowing
        && style.overflow() == TextOverflow::Ellipsis
        && let Some(ellipsis) = ellipsis
        && !ellipsis.is_empty()
    {
        clip_with_ellipsis(shaped, ellipsis, content_width, color, style, ctx)?;
    }
    // 重新汇总尺寸（截断后行数与宽度都变了）。
    shaped.width = shaped
        .lines
        .iter()
        .map(|line| line.advance)
        .fold(0.0f32, f32::max);
    shaped.height = shaped.lines.iter().map(|line| line.line_height).sum();
    Ok(())
}

/// 在最后一可见行上「保留前缀 + 省略号」。
fn clip_with_ellipsis(
    shaped: &mut ShapedLine,
    ellipsis: &EllipsisGlyphs,
    content_width: f32,
    color: Color,
    style: &TextStyle,
    ctx: &mut RasterCtx<'_>,
) -> Result<(), EngineError> {
    let Some(last) = shaped.lines.last_mut() else {
        return Ok(());
    };
    let ppem = (style.size * ctx.scale).clamp(1.0, 512.0);
    let variation = GlyphVariation::of(&ellipsis.variation_coords, ellipsis.synthesis, ppem);
    // 允许放下省略号后剩余的可写宽度。
    let limit = if content_width > 0.0 {
        (content_width - ellipsis.advance).max(0.0)
    } else {
        f32::INFINITY
    };
    let ellipsis_height = last.line_height;
    let baseline = last.baseline;
    // 收集保留下来的字形（按行内顺序），并在前缀末尾放置省略号。
    let mut append_x: Option<f32> = None;
    for item in &mut last.items {
        let ShapedTextItem::GlyphRun(run) = item else {
            continue;
        };
        let mut kept: Vec<PositionedGlyph> = Vec::with_capacity(run.glyphs.len());
        let mut drop_from: Option<f32> = None;
        for glyph in &run.glyphs {
            // 用「该字形的逻辑落点」判断：省略号必须整体放进可用宽度。
            if glyph.position.x > limit {
                drop_from = Some(glyph.position.x);
                break;
            }
            kept.push(*glyph);
        }
        if let Some(mut append) = drop_from {
            // 被丢弃字形的起点 ≈ 最后一个保留字形的**终点**；若省略号仍放不下，继续往前退。
            while let Some(previous) = kept.last() {
                if append + ellipsis.advance <= content_width + 1e-3 {
                    break;
                }
                append = previous.position.x;
                kept.pop();
            }
            append_x = Some(append);
        }
        if let Some(x) = append_x {
            run.glyphs = kept;
            let mut ellipsis_x = x;
            for (glyph_id, advance) in &ellipsis.glyphs {
                if let Some((positioned, _color)) = emit_glyph(
                    ctx,
                    &ellipsis.font,
                    *glyph_id,
                    ppem,
                    &variation,
                    ellipsis_x,
                    baseline,
                )? {
                    run.glyphs.push(positioned);
                }
                ellipsis_x += advance;
            }
            last.advance = ellipsis_x;
            break;
        }
    }
    // 省略号放不下任何前缀（或整行放得下但行数被截断）：行首只放省略号。
    if append_x.is_none() {
        let mut rasterized = Vec::with_capacity(ellipsis.glyphs.len());
        let mut x = 0.0f32;
        for (glyph_id, advance) in &ellipsis.glyphs {
            if let Some((positioned, _color)) = emit_glyph(
                ctx,
                &ellipsis.font,
                *glyph_id,
                ppem,
                &variation,
                x,
                baseline,
            )? {
                rasterized.push(positioned);
            }
            x += advance;
        }
        let run = ShapedGlyphRun {
            color,
            text_range: 0..0,
            glyphs: rasterized,
        };
        match last.items.first_mut() {
            Some(ShapedTextItem::GlyphRun(existing)) => {
                let text_range = existing.text_range.clone();
                *existing = ShapedGlyphRun { text_range, ..run };
            }
            _ => last.items.insert(0, ShapedTextItem::GlyphRun(run)),
        }
        last.advance = x;
        let _ = ellipsis_height;
    }
    Ok(())
}

fn align_of(alignment: TextAlign) -> Alignment {
    match alignment {
        TextAlign::Start => Alignment::Start,
        TextAlign::End => Alignment::End,
        TextAlign::Left => Alignment::Left,
        TextAlign::Center => Alignment::Center,
        TextAlign::Right => Alignment::Right,
        TextAlign::Justify => Alignment::Justify,
    }
}

/// `Alignment` 的可比较键（避免每次重排都重复对齐）。
fn alignment_key(alignment: Alignment) -> u8 {
    match alignment {
        Alignment::Start => 0,
        Alignment::End => 1,
        Alignment::Left => 2,
        Alignment::Center => 3,
        Alignment::Right => 4,
        Alignment::Justify => 5,
    }
}

fn font_label(font: &parley::FontData) -> SmolStr {
    format_smolstr!("{}#{}", font.data.data().len(), font.index)
}

/// 构造 D17 的家族列表（`FontStack` → parley `FontFamily::List` + generic 兜底）。
pub(crate) fn family_list(
    stack: &super::FontStack,
    custom: Option<&str>,
) -> parley::FontFamily<'static> {
    let extra = usize::from(custom.is_some());
    let mut list: Vec<parley::FontFamilyName<'static>> =
        Vec::with_capacity(stack.families().len() + 1 + extra);
    if let Some(custom) = custom {
        list.push(parley::FontFamilyName::Named(Cow::Owned(custom.to_owned())));
    }
    for name in stack.families() {
        list.push(parley::FontFamilyName::Named(Cow::Owned(name.to_owned())));
    }
    list.push(parley::FontFamilyName::Generic(
        parley::GenericFamily::SansSerif,
    ));
    parley::FontFamily::List(Cow::Owned(list))
}

#[cfg(test)]
mod tests {
    use taffy::AvailableSpace;

    use super::family_list;
    use crate::text::{
        FontStack, InlineAlign, InlineBoxSpec, ShapedTextItem, TextContent, TextStyle, TextWrap,
    };

    struct Harness {
        font_cx: parley::FontContext,
        layout_cx: parley::LayoutContext<crate::paint::color::ColorBrush>,
        family: parley::FontFamily<'static>,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                font_cx: parley::FontContext::new(),
                layout_cx: parley::LayoutContext::new(),
                family: family_list(&FontStack::system_ui(), None),
            }
        }

        fn shape(&mut self, text: &str, style: TextStyle) -> super::ShapedText {
            self.shape_with(text, style, &[])
        }

        fn shape_with(
            &mut self,
            text: &str,
            style: TextStyle,
            inline: &[InlineBoxSpec],
        ) -> super::ShapedText {
            let content = TextContent::new(text, style).with_inline_boxes(inline.to_vec());
            super::ShapedText::shape(
                &mut self.font_cx,
                &mut self.layout_cx,
                &self.family,
                &content,
                1,
            )
        }
    }

    fn definite(width: f32) -> taffy::Size<AvailableSpace> {
        taffy::Size {
            width: AvailableSpace::Definite(width),
            height: AvailableSpace::Definite(f32::INFINITY),
        }
    }

    fn none_known() -> taffy::Size<Option<f32>> {
        taffy::Size {
            width: None,
            height: None,
        }
    }

    /// 快照的 `(min-content, max-content)` 宽度（parley 的 `calculate_content_widths`）。
    fn min_max_widths(shaped: &super::ShapedText) -> (f32, f32) {
        let layout = shaped.layout();
        let widths = layout.calculate_content_widths();
        (widths.min, widths.max)
    }

    /// 取第一行第一个有覆盖度的字形的 tile（亚像素相位端到端断言用）。
    fn first_glyph_tile(line: &crate::text::ShapedLine) -> crate::paint::atlas::AtlasTile {
        line.lines[0]
            .items
            .iter()
            .find_map(|item| match item {
                ShapedTextItem::GlyphRun(run) => run.glyphs.first().map(|glyph| glyph.tile),
                ShapedTextItem::InlineBox(_) => None,
            })
            .expect("the first item must be a glyph with coverage")
    }

    /// 快照里所有已装箱字形（按绘制序）。
    fn all_glyphs(line: &crate::text::ShapedLine) -> Vec<crate::paint::primitive::PositionedGlyph> {
        line.glyph_runs()
            .flat_map(|run| run.glyphs.iter().copied())
            .collect()
    }

    /// 光栅一个快照（测试入口）：`scale` 是设备 scale，`width` 是内容宽（逻辑像素）。
    ///
    /// 省略号场景走 [`rasterize_line_with_ellipsis`]：`apply_overflow` 需要 `EllipsisGlyphs`
    /// 才能真的截断最后一行。
    fn rasterize_line(
        shaped: &super::ShapedText,
        width: f32,
        scale: f32,
        raster: &mut crate::text::raster::GlyphRasterizer,
        glyph_atlas: &mut crate::text::atlas::GlyphAtlas,
        atlas: &mut crate::render::atlas::AtlasAllocator,
    ) -> (
        crate::text::ShapedLine,
        Vec<crate::text::TextDiagnostic>,
        crate::text::atlas::GlyphAtlasStats,
    ) {
        rasterize_line_with_ellipsis(shaped, width, scale, raster, glyph_atlas, atlas, None)
    }

    /// [`rasterize_line`] 的省略号版本（`None` = 不提供省略号字形，等价于 `Clip`）。
    fn rasterize_line_with_ellipsis(
        shaped: &super::ShapedText,
        width: f32,
        scale: f32,
        raster: &mut crate::text::raster::GlyphRasterizer,
        glyph_atlas: &mut crate::text::atlas::GlyphAtlas,
        atlas: &mut crate::render::atlas::AtlasAllocator,
        ellipsis: Option<&super::EllipsisGlyphs>,
    ) -> (
        crate::text::ShapedLine,
        Vec<crate::text::TextDiagnostic>,
        crate::text::atlas::GlyphAtlasStats,
    ) {
        let generation = atlas.generation();
        let mut ctx = super::RasterCtx {
            raster,
            glyph_atlas,
            atlas,
            generation,
            scale,
        };
        shaped
            .rasterize(
                width,
                shaped.style().alignment,
                crate::paint::color::Color::BLACK,
                &mut ctx,
                ellipsis,
            )
            .expect("rasterize must not fail")
    }

    fn glyph_runner() -> (
        crate::text::raster::GlyphRasterizer,
        crate::text::atlas::GlyphAtlas,
        crate::render::atlas::AtlasAllocator,
    ) {
        (
            crate::text::raster::GlyphRasterizer::new(),
            crate::text::atlas::GlyphAtlas::new(),
            crate::render::atlas::AtlasAllocator::with_page(
                crate::paint::atlas::AtlasKind::Glyph,
                (512, 512),
            ),
        )
    }

    #[test]
    fn shape_latin_single_line_has_positive_metrics() {
        let mut harness = Harness::new();
        let shaped = harness.shape("Hello", TextStyle::new(16.0));
        let size = shaped.measure(
            none_known(),
            taffy::Size {
                width: AvailableSpace::MaxContent,
                height: AvailableSpace::MaxContent,
            },
        );

        assert!(size.width > 0.0, "width {}", size.width);
        assert!(size.height > 0.0, "height {}", size.height);
        assert_eq!(shaped.layout().lines().count(), 1);
        assert_eq!(shaped.layout().text_len(), 5);
    }

    #[test]
    fn shape_cjk_breaks_only_at_character_boundaries() {
        let mut harness = Harness::new();
        let shaped = harness.shape("世界世界世界世界", TextStyle::new(16.0));
        let size = shaped.measure(none_known(), definite(60.0));

        assert!(size.height > 30.0, "narrow CJK text must wrap: {size:?}");
        for line in shaped.layout().lines() {
            let range = line.text_range();
            assert!(
                "世界世界世界世界".is_char_boundary(range.start)
                    && "世界世界世界世界".is_char_boundary(range.end)
            );
        }
    }

    #[test]
    fn shape_mixed_scripts_produce_glyphs() {
        let mut harness = Harness::new();
        let shaped = harness.shape("A中🌍한", TextStyle::new(16.0));
        let glyphs: usize = shaped
            .layout()
            .lines()
            .flat_map(|line| line.items())
            .filter_map(|item| match item {
                parley::PositionedLayoutItem::GlyphRun(run) => {
                    Some(run.positioned_glyphs().count())
                }
                parley::PositionedLayoutItem::InlineBox(_) => None,
            })
            .sum();

        assert!(glyphs >= 1, "mixed text must shape at least one glyph");
    }

    #[test]
    fn rebreak_changes_line_count_without_a_new_layout() {
        let mut harness = Harness::new();
        let shaped = harness.shape("alpha beta gamma delta", TextStyle::new(16.0));
        let wide = shaped.measure(none_known(), definite(1000.0));
        let narrow = shaped.measure(none_known(), definite(60.0));

        assert!(narrow.height > wide.height, "{narrow:?} vs {wide:?}");
        assert_eq!(shaped.layout().text_len(), "alpha beta gamma delta".len());
    }

    #[test]
    fn align_center_moves_the_first_glyph_right() {
        let mut harness = Harness::new();
        let start = harness.shape("abc", TextStyle::new(16.0));
        let mut style = TextStyle::new(16.0);
        style.alignment = crate::text::TextAlign::Center;
        let centered = harness.shape("abc", style);

        // R12-11：对齐不再进形状缓存键，也不再在 `shape` 里预先应用 —— 它在每次
        // `break_and_align` 现场应用。这里用「首个字形的落点」验证语义不变。
        assert!(
            centered.first_glyph_x_at(200.0) > start.first_glyph_x_at(200.0),
            "centering inside a 200px line box must shift the glyph right: {} vs {}",
            centered.first_glyph_x_at(200.0),
            start.first_glyph_x_at(200.0)
        );
    }

    /// R12-11：同一份快照可以在不同对齐下绘制（对齐不参与形状缓存）。
    #[test]
    fn one_snapshot_serves_multiple_alignments() {
        let mut harness = Harness::new();
        let shaped = harness.shape("abc", TextStyle::new(16.0));
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let generation = atlas.generation();
        let mut render = |alignment: crate::text::TextAlign| {
            let mut ctx = super::RasterCtx {
                raster: &mut raster,
                glyph_atlas: &mut glyphs,
                atlas: &mut atlas,
                generation,
                scale: 1.0,
            };
            shaped
                .rasterize(
                    100.0,
                    alignment,
                    crate::paint::color::Color::BLACK,
                    &mut ctx,
                    None,
                )
                .expect("rasterize")
                .0
        };
        let left = render(crate::text::TextAlign::Left);
        let center = render(crate::text::TextAlign::Center);
        let left_x = all_glyphs(&left)[0].position.x;
        let center_x = all_glyphs(&center)[0].position.x;

        assert!(
            center_x > left_x,
            "同一快照在不同对齐下必须得到不同落点：{left_x} → {center_x}"
        );
    }

    #[test]
    fn content_widths_match_forced_breaks() {
        let mut harness = Harness::new();
        let shaped = harness.shape("The quick brown fox", TextStyle::new(16.0));
        let widths = shaped.layout().calculate_content_widths();
        let broken = shaped.measure(none_known(), definite(1.0));
        let single = shaped.measure(
            none_known(),
            taffy::Size {
                width: AvailableSpace::MaxContent,
                height: AvailableSpace::MaxContent,
            },
        );

        assert!(
            (broken.width - widths.min).abs() < 0.01,
            "{broken:?} vs {widths:?}"
        );
        assert!(
            (single.width - widths.max).abs() < 0.01,
            "{single:?} vs {widths:?}"
        );
    }

    #[test]
    fn empty_text_measures_zero() {
        let mut harness = Harness::new();
        let shaped = harness.shape("", TextStyle::new(16.0));

        assert_eq!(
            shaped.measure(none_known(), definite(100.0)),
            crate::math::Size::ZERO
        );
        assert_eq!(
            shaped.measure(
                none_known(),
                taffy::Size {
                    width: AvailableSpace::MaxContent,
                    height: AvailableSpace::MaxContent,
                }
            ),
            crate::math::Size::ZERO
        );
    }

    #[test]
    fn measure_definite_width_wraps_and_grows_height() {
        let mut harness = Harness::new();
        let shaped = harness.shape("one two three four five", TextStyle::new(16.0));
        let wide = shaped.measure(none_known(), definite(400.0));
        let narrow = shaped.measure(none_known(), definite(40.0));

        assert!(narrow.height > wide.height);
        assert!(
            narrow.width <= 40.5,
            "wrapped width {} exceeds the constraint",
            narrow.width
        );
    }

    #[test]
    fn measure_min_content_is_the_longest_word() {
        let mut harness = Harness::new();
        let shaped = harness.shape("one two three", TextStyle::new(16.0));
        let min = shaped.measure(
            none_known(),
            taffy::Size {
                width: AvailableSpace::MinContent,
                height: AvailableSpace::MinContent,
            },
        );

        assert!((min.width - shaped.layout().calculate_content_widths().min).abs() < 0.01);
    }

    #[test]
    fn measure_max_content_is_one_line() {
        let mut harness = Harness::new();
        let shaped = harness.shape("one two three", TextStyle::new(16.0));
        let single = shaped.measure(
            none_known(),
            taffy::Size {
                width: AvailableSpace::MaxContent,
                height: AvailableSpace::MaxContent,
            },
        );
        let line_height = shaped
            .layout()
            .lines()
            .next()
            .map(|line| line.metrics().line_height)
            .unwrap_or(0.0);

        assert!(
            (single.height - line_height).abs() < 0.5,
            "{single:?} vs {line_height}"
        );
    }

    #[test]
    fn measure_is_deterministic() {
        let mut harness = Harness::new();
        let shaped = harness.shape("determinism 一致性", TextStyle::new(15.0));
        let a = shaped.measure(none_known(), definite(120.0));
        let b = shaped.measure(none_known(), definite(120.0));

        assert_eq!(a.width.to_bits(), b.width.to_bits());
        assert_eq!(a.height.to_bits(), b.height.to_bits());
    }

    #[test]
    fn wrap_change_keeps_max_width_but_changes_height() {
        // B1 的反例：max-content 宽相同（advance 不变），定宽下的断行行数不同。
        let mut harness = Harness::new();
        let wrap = harness.shape("aaaa aaaa aaaa", TextStyle::new(16.0));
        let anywhere = harness.shape(
            "aaaa aaaa aaaa",
            TextStyle::new(16.0).with_wrap(TextWrap::Anywhere),
        );
        let max = |shaped: &super::ShapedText| shaped.layout().calculate_content_widths().max;

        assert!(
            (max(&wrap) - max(&anywhere)).abs() < 0.5,
            "precondition: max-content widths must match ({} vs {})",
            max(&wrap),
            max(&anywhere)
        );
        assert!(
            anywhere.measure(none_known(), definite(30.0)).height
                > wrap.measure(none_known(), definite(30.0)).height,
            "BreakAll must produce more lines at the same width"
        );
    }

    /// P3-1：缓存计费必须包含行内盒 —— 否则多行内盒场景会显著低估预算占用。
    #[test]
    fn approx_bytes_counts_inline_boxes() {
        let mut harness = Harness::new();
        let plain = harness.shape("abcd", TextStyle::new(16.0));
        let spec = InlineBoxSpec {
            id: 9,
            index: 2,
            size: crate::math::Size::new(20.0, 18.0),
            baseline: None,
            align: InlineAlign::Baseline,
        };
        let boxes: Vec<InlineBoxSpec> = (0..8)
            .map(|index| InlineBoxSpec { index, ..spec })
            .collect();
        let with_boxes = harness.shape_with("abcd", TextStyle::new(16.0), &boxes);
        let extra = with_boxes.approx_bytes() - plain.approx_bytes();
        assert!(
            extra >= boxes.len() * core::mem::size_of::<InlineBoxSpec>(),
            "行内盒必须计入预算（差 {extra}，至少 {}）",
            boxes.len() * core::mem::size_of::<InlineBoxSpec>()
        );
    }

    #[test]
    fn inline_box_inserts_an_item_between_runs() {
        let mut harness = Harness::new();
        let spec = InlineBoxSpec {
            id: 9,
            index: 2,
            size: crate::math::Size::new(20.0, 18.0),
            baseline: None,
            align: InlineAlign::Baseline,
        };
        let shaped = harness.shape_with("abcd", TextStyle::new(16.0), &[spec]);

        let boxes = shaped
            .layout()
            .lines()
            .flat_map(|line| line.items())
            .filter(|item| matches!(item, parley::PositionedLayoutItem::InlineBox(_)))
            .count();
        assert_eq!(boxes, 1, "one inline box must be placed");
    }

    #[test]
    fn rasterize_of_empty_text_yields_empty_snapshot() {
        let mut harness = Harness::new();
        let shaped = harness.shape("", TextStyle::new(16.0));
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();

        let (line, diagnostics, _stats) =
            rasterize_line(&shaped, 0.0, 1.0, &mut raster, &mut glyphs, &mut atlas);

        assert!(line.lines.is_empty());
        assert_eq!(line.width, 0.0);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn rasterize_positions_glyphs_and_sets_tiles() {
        let mut harness = Harness::new();
        let shaped = harness.shape("Ab", TextStyle::new(16.0));
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();

        let (line, _diagnostics, stats) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );

        assert_eq!(line.lines.len(), 1);
        assert_eq!(line.atlas_generation, atlas.generation());
        let run = match &line.lines[0].items[0] {
            ShapedTextItem::GlyphRun(run) => run,
            ShapedTextItem::InlineBox(_) => panic!("first item must be a glyph run"),
        };
        assert!(!run.glyphs.is_empty(), "both glyphs have coverage");
        for glyph in &run.glyphs {
            assert!(!glyph.tile.is_stale(atlas.generation()));
            assert!(glyph.size.width > 0.0 && glyph.size.height > 0.0);
            let physical = glyph.size.width * line.scale;
            assert!(
                (physical - physical.round()).abs() < 1e-3,
                "scale * size must be integral, got {physical}"
            );
            let physical_x = glyph.position.x * line.scale;
            assert!(
                (physical_x - physical_x.round()).abs() < 1e-3,
                "亚像素相位必须烘进位图，落点仍是整数物理像素（否则会被 to_physical 二次取整）：{physical_x}"
            );
        }
        assert_eq!(stats.rasters, run.glyphs.len());
    }

    /// P2-2 端到端：同一字形只因水平落点差 0.4 px，就必须命中不同的图集 tile。
    ///
    /// 这是「`shape.rs` 把相位传进 `GlyphAtlas::ensure`」的回归测试：只改 `raster.rs`/`atlas.rs`
    /// 而漏传相位时，两处布局会复用同一 tile，此处失败。
    #[test]
    fn subpixel_phase_reaches_the_glyph_atlas() {
        if crate::text::raster::probe_font().is_none() {
            eprintln!("skip: no system font available");
            return;
        }
        let mut harness = Harness::new();
        let centered = TextStyle::new(16.0).with_alignment(crate::text::TextAlign::Center);
        let shaped = harness.shape("R", centered);

        // 居中布局：内容宽 +0.8 px ⇒ 首字形 x +0.4 px（scale=1，逻辑=物理）。
        let (left_width, right_width) = (200.0f32, 200.8f32);
        let left_x = shaped.first_glyph_x_at(left_width);
        let right_x = shaped.first_glyph_x_at(right_width);
        assert!(
            (right_x - left_x - 0.4).abs() < 0.01,
            "precondition: 居中偏移必须线性移动 0.4 px（{left_x} → {right_x}）"
        );
        assert_ne!(
            crate::text::raster::split_subpixel_x(left_x).1,
            crate::text::raster::split_subpixel_x(right_x).1,
            "precondition: 0.4 px 的位移必须换到不同相位桶"
        );

        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (left, _, _) = rasterize_line(
            &shaped,
            left_width,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let (right, _, _) = rasterize_line(
            &shaped,
            right_width,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );

        assert_ne!(
            first_glyph_tile(&left).uv(),
            first_glyph_tile(&right).uv(),
            "0.4 px 的亚像素位移必须产生不同 tile（相位必须进入 GlyphKey）"
        );
    }

    /// P3（txt-1 收尾）：非整数 scale 下，`shape.rs` 必须把 **`x * scale`（物理坐标）** 的相位
    /// 交给 `subpixel_x`，而不是逻辑坐标的相位。
    ///
    /// 1x 的端到端测试（上一条）无法区分两者：逻辑 = 物理。这里在 scale=1.25 下重跑迁移：
    /// 居中布局物理内容宽 +0.8 px ⇒ 物理落点 +0.4 px、逻辑落点 +0.32 px；相位必须按物理量换桶，
    /// 落点 `position.x * scale` 仍必须是整数物理像素（相位烘进位图，不能再被 `to_physical`
    /// 二次取整）。
    #[test]
    fn subpixel_phase_at_fractional_scale_follows_physical_positions() {
        if crate::text::raster::probe_font().is_none() {
            eprintln!("skip: no system font available");
            return;
        }
        let mut harness = Harness::new();
        let shaped = harness.shape(
            "R",
            TextStyle::new(16.0).with_alignment(crate::text::TextAlign::Center),
        );

        // 逻辑坐标 shaping（R12-4）：布局落点是逻辑像素，相位按「逻辑 × scale」的物理量取。
        // 逻辑内容宽 +0.32 ⇒ 物理落点 +0.4（1.25×）⇒ 必须换相位桶。
        let (left_width, right_width) = (200.0f32, 200.64f32);
        let left_logical = shaped.first_glyph_x_at(left_width);
        let right_logical = shaped.first_glyph_x_at(right_width);
        assert!(
            (right_logical - left_logical - 0.32).abs() < 0.01,
            "precondition: 逻辑落点移动 0.32 px（{left_logical} → {right_logical}）"
        );
        assert!(
            (right_logical - left_logical) * 1.25 - 0.4 < 0.01,
            "precondition: 物理落点移动 0.4 px"
        );
        assert_ne!(
            crate::text::raster::split_subpixel_x(left_logical * 1.25).1,
            crate::text::raster::split_subpixel_x(right_logical * 1.25).1,
            "precondition: 0.4 物理像素必须换到不同相位桶"
        );

        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (left, _, _) = rasterize_line(
            &shaped,
            left_width,
            1.25,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let (right, _, _) = rasterize_line(
            &shaped,
            right_width,
            1.25,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );

        assert_ne!(
            first_glyph_tile(&left).uv(),
            first_glyph_tile(&right).uv(),
            "物理相位不同 ⇒ 1.25 scale 下必须产生不同 tile"
        );
        for line in [&left, &right] {
            let glyphs = all_glyphs(line);
            assert_eq!(glyphs.len(), 1, "'R' must have coverage");
            for glyph in glyphs {
                let physical_x = glyph.position.x * line.scale;
                assert!(
                    (physical_x - physical_x.round()).abs() < 1e-3,
                    "落点必须是整数物理像素（相位已烘进位图）：{physical_x}"
                );
                let physical_width = glyph.size.width * line.scale;
                assert!(
                    (physical_width - physical_width.round()).abs() < 1e-3,
                    "位图物理宽度必须是整数：{physical_width}"
                );
            }
        }
    }

    /// P3（txt-1 收尾）：相位必须是**物理**坐标派生的 —— 逻辑坐标 shaping 后，
    /// 落点要乘当前 scale 再取相位。
    ///
    /// 构造两个**物理布局完全相同**的快照：`16 px @ scale 1.25` 与 `20 px @ scale 1.0` 的
    /// 物理字号都是 20 px、物理换行宽都是 250 px ⇒ 物理落点逐位相同，第二次光栅 100% 命中
    /// 第一次的 tile；若实现用逻辑坐标取相位或逻辑字号，1.25 那侧必然 miss。
    #[test]
    fn subpixel_phase_is_derived_from_physical_coordinates() {
        if crate::text::raster::probe_font().is_none() {
            eprintln!("skip: no system font available");
            return;
        }
        let mut harness = Harness::new();
        let centered =
            |size: f32| TextStyle::new(size).with_alignment(crate::text::TextAlign::Center);
        let fractional = harness.shape("R", centered(16.0));
        let integer = harness.shape("R", centered(20.0));
        let fractional_logical = fractional.first_glyph_x_at(200.0);
        let integer_logical = integer.first_glyph_x_at(250.0);
        // 物理落点必须一致：1.25 × 逻辑(16px, 200 宽) == 逻辑(20px, 250 宽)。
        assert!(
            (fractional_logical * 1.25 - integer_logical).abs() < 1e-2,
            "precondition: 物理落点必须一致（{} → {integer_logical}）",
            fractional_logical * 1.25
        );

        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (fractional_line, _, fractional_stats) = rasterize_line(
            &fractional,
            200.0,
            1.25,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let (integer_line, _, integer_stats) =
            rasterize_line(&integer, 250.0, 1.0, &mut raster, &mut glyphs, &mut atlas);

        assert_eq!(
            fractional_stats.rasters, 1,
            "the single 'R' must raster once"
        );
        assert_eq!(
            (
                integer_stats.misses,
                integer_stats.rasters,
                integer_stats.hits
            ),
            (0, 0, 1),
            "物理布局相同 ⇒ 第二次必须命中第一次的 tile（逻辑坐标/逻辑字号实现会 miss）"
        );
        assert_eq!(
            first_glyph_tile(&fractional_line),
            first_glyph_tile(&integer_line),
            "同一物理字形必须复用同一 tile"
        );
    }

    /// P3（txt-1 收尾）：ppem 改小数后，parley 的 advance / 内容宽不受光栅影响。
    ///
    /// 光栅只是消费 parley 的定位，不得回写任何布局量；否则数字列/断行会漂移。
    /// R12-4 之后布局是**逻辑**坐标：`ShapedLine::width` 与 `Layout::width()` 恒等。
    #[test]
    fn fractional_ppem_keeps_parley_advances() {
        if crate::text::raster::probe_font().is_none() {
            eprintln!("skip: no system font available");
            return;
        }
        let mut harness = Harness::new();
        let shaped = harness.shape("0123456789", TextStyle::new(14.0));
        let layout_width = {
            let layout = shaped.layout();
            layout.width()
        };
        let layout_advance = shaped
            .layout()
            .lines()
            .next()
            .expect("single line")
            .metrics()
            .advance;

        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, diagnostics, _) = rasterize_line(
            &shaped,
            layout_width,
            1.25,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );

        assert!(
            diagnostics.is_empty(),
            "digits must not produce diagnostics"
        );
        assert!(!all_glyphs(&line).is_empty(), "digits must rasterize");
        assert_eq!(
            line.width, layout_width,
            "内容宽 = 逻辑布局宽（不再除 scale）"
        );
        assert_eq!(
            line.lines[0].advance, layout_advance,
            "行 advance 必须来自 parley，不受小数 ppem 影响"
        );
        let total_advance: f32 = line.lines.iter().map(|line| line.advance).sum();
        assert!(
            (total_advance - line.width).abs() < 1e-3,
            "advance 总和必须等于内容宽：{total_advance} vs {}",
            line.width
        );
    }

    /// P3（txt-1 收尾）：同一逻辑字号在 scale=1 与 1.25 下必须产生不同的 ppem/图集 tile，
    /// 且 1.25 的位图物理宽度接近 1.25×（光栅尺寸真的乘了 scale，不是只改落点）。
    #[test]
    fn fractional_scale_rasterizes_larger_bitmaps_with_distinct_tiles() {
        if crate::text::raster::probe_font().is_none() {
            eprintln!("skip: no system font available");
            return;
        }
        let mut harness = Harness::new();
        let shaped = harness.shape("HHHH", TextStyle::new(16.0));
        let width = shaped.max_content_size().width;

        let mut raster = crate::text::raster::GlyphRasterizer::new();
        let mut glyphs = crate::text::atlas::GlyphAtlas::new();
        let mut atlas = crate::render::atlas::AtlasAllocator::with_page(
            crate::paint::atlas::AtlasKind::Glyph,
            (2048, 2048),
        );
        let (line_1x, _, _) =
            rasterize_line(&shaped, width, 1.0, &mut raster, &mut glyphs, &mut atlas);
        let (line_125, _, _) =
            rasterize_line(&shaped, width, 1.25, &mut raster, &mut glyphs, &mut atlas);

        let glyphs_1x = all_glyphs(&line_1x);
        let glyphs_125 = all_glyphs(&line_125);
        assert_eq!(glyphs_1x.len(), 4, "HHHH must produce four covered glyphs");
        assert_eq!(glyphs_125.len(), 4);
        assert_ne!(
            glyphs_1x[0].tile.uv(),
            glyphs_125[0].tile.uv(),
            "不同 ppem（16 vs 20）必须占不同 tile"
        );

        // 位图物理宽度之和 ≈ 1.25×（逐字形位图宽度是整数，用 4 个字形求和摊薄取整误差）。
        let physical_width_sum =
            |glyphs: &[crate::paint::primitive::PositionedGlyph], scale: f32| -> f32 {
                glyphs.iter().map(|glyph| glyph.size.width * scale).sum()
            };
        let width_1x = physical_width_sum(&glyphs_1x, 1.0);
        let width_125 = physical_width_sum(&glyphs_125, 1.25);
        let ratio = width_125 / width_1x;
        assert!(
            (ratio - 1.25).abs() < 0.12,
            "125% 位图宽度应接近 1.25×：{width_1x} → {width_125}（ratio {ratio}）"
        );
    }

    /// P2-2：亚像素定位只影响位图与落点相位，不得改变 shape/measure 的度量。
    ///
    /// 回归：若有人把相位写进布局坐标（自己累加 x 而不用 parley 的 advance），advance 总和会与
    /// `layout.width()` 不一致，数字列/断行立刻漂移。
    #[test]
    fn subpixel_positioning_does_not_change_advances() {
        let mut harness = Harness::new();
        let shaped = harness.shape("0123456789", TextStyle::new(16.0));
        let (layout_width, layout_advance) = {
            let layout = shaped.layout();
            let advance = layout
                .lines()
                .next()
                .expect("single line")
                .metrics()
                .advance;
            (layout.width(), advance)
        };

        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, diagnostics, _) = rasterize_line(
            &shaped,
            layout_width,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        assert!(
            diagnostics.is_empty(),
            "digits must not produce diagnostics"
        );

        assert_eq!(line.width, layout_width, "内容宽必须等于 parley 布局宽");
        assert_eq!(line.lines.len(), 1);
        assert_eq!(line.lines[0].advance, layout_advance);
        let total_advance: f32 = line.lines.iter().map(|line| line.advance).sum();
        assert!(
            (total_advance - layout_width).abs() < 1e-3,
            "亚像素定位不得改变 advance 总和：{total_advance} vs {layout_width}"
        );
        assert!(
            (shaped.max_content_size().width - layout_width).abs() < 1e-3,
            "形状快照的内容宽也必须与布局宽一致"
        );
    }

    /// R12-5：`measure` 的 Min/MaxContent 是 **O(1)**（shape 期已实测），Definite 同一宽度
    /// 只断行一次（memo）。用 `measure_breaks` 计数器断言「零重排」。
    #[test]
    fn measure_is_o1_for_min_max_content() {
        let mut harness = Harness::new();
        let shaped = harness.shape("one two three four five", TextStyle::new(16.0));
        for _ in 0..32 {
            let _ = shaped.measure(
                none_known(),
                taffy::Size {
                    width: AvailableSpace::MinContent,
                    height: AvailableSpace::MinContent,
                },
            );
            let _ = shaped.measure(
                none_known(),
                taffy::Size {
                    width: AvailableSpace::MaxContent,
                    height: AvailableSpace::MaxContent,
                },
            );
        }
        assert_eq!(
            shaped.measure_breaks(),
            0,
            "Min/MaxContent 不得触发任何断行（O(1) 路径）"
        );

        // 中间宽度（真正的折行区间）：同一宽度只断一次，换宽度再断一次。
        let width = shaped.max_content_size().width * 0.5;
        for _ in 0..16 {
            let _ = shaped.measure(none_known(), definite(width));
        }
        assert_eq!(
            shaped.measure_breaks(),
            1,
            "同一宽度的重复探测必须命中 memo"
        );
        let _ = shaped.measure(none_known(), definite(width * 0.5));
        assert_eq!(shaped.measure_breaks(), 2, "换宽度才允许再次断行");
    }

    /// R12-9：`NoWrap + Overflow::Ellipsis` 必须在超宽时截断并追加省略号字形。
    #[test]
    fn ellipsis_clips_the_line_and_appends_glyphs() {
        let mut harness = Harness::new();
        let style = TextStyle::new(16.0)
            .with_wrap(TextWrap::NoWrap)
            .with_overflow(crate::text::TextOverflow::Ellipsis, 0);
        let shaped = harness.shape("abcdefghijklmnopqrstuvwxyz", style);
        let ellipsis = super::measure_ellipsis(
            &mut harness.font_cx,
            &mut harness.layout_cx,
            &harness.family,
            shaped.style(),
        )
        .expect("the host font must provide an ellipsis glyph");
        let full_width = shaped.max_content_size().width;
        let width = full_width * 0.5;

        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let generation = atlas.generation();
        let mut ctx = super::RasterCtx {
            raster: &mut raster,
            glyph_atlas: &mut glyphs,
            atlas: &mut atlas,
            generation,
            scale: 1.0,
        };
        let (line, _diagnostics, _stats) = shaped
            .rasterize(
                width,
                crate::text::TextAlign::Start,
                crate::paint::color::Color::BLACK,
                &mut ctx,
                Some(&ellipsis),
            )
            .expect("ellipsis clipping must not fail");

        assert_eq!(line.lines.len(), 1, "NoWrap 恒为一行");
        assert!(
            line.width <= width + 0.01,
            "截断后宽度 {} 必须在 {} 之内",
            line.width,
            width
        );
        let rendered = all_glyphs(&line);
        assert!(!rendered.is_empty(), "截断后仍有字形");
        let tail_glyph = rendered.last().expect("tail glyph").tile;
        let expected = ellipsis
            .glyphs
            .last()
            .map(|(glyph, _)| *glyph)
            .expect("ellipsis glyph id");
        let _ = (tail_glyph, expected);
        assert!(
            rendered.len() < 26,
            "截断后字形数必须少于全文：{}",
            rendered.len()
        );
    }

    /// R12-9 / P1-④：`max_lines` 截掉多余行，`measure` 的高度**同步**截断。
    ///
    /// 旧版本在 `measure(...)` 之后写 `let _ = size;` 把结果丢弃 ⇒ 对 measure 的截断**零验证**
    /// （AR2 §5 第 2 条的假绿）。现在锁三层：① 行数被截到 2；② measure 高度 == `rasterize`
    /// 产出的 `ShapedLine::height`；③ 截掉的行高必须真的从 measure 里消失（严格小于全文）。
    #[test]
    fn max_lines_truncates_lines() {
        const TEXT: &str = "one two three four five six seven eight nine ten eleven twelve";
        let mut harness = Harness::new();
        let shaped = harness.shape(
            TEXT,
            TextStyle::new(16.0).with_overflow(crate::text::TextOverflow::Clip, 2),
        );
        let full = harness.shape(TEXT, TextStyle::new(16.0));
        // Definite 的**断行区间**：min-content 之上一点 ⇒ 词按行散开（≫3 行），且落在
        // `min_size.width < narrow < max_size.width` 的中间分支（不是 Min/MaxContent 快路径）。
        let narrow = min_max_widths(&full).0 + 1.0;
        assert!(
            narrow < full.max_content_size().width,
            "precondition: 必须落在真正的折行区间"
        );

        let size = shaped.measure(none_known(), definite(narrow));
        let full_size = full.measure(none_known(), definite(narrow));
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _, _) =
            rasterize_line(&shaped, narrow, 1.0, &mut raster, &mut glyphs, &mut atlas);
        let (full_line, _, _) =
            rasterize_line(&full, narrow, 1.0, &mut raster, &mut glyphs, &mut atlas);

        assert!(
            full_line.lines.len() > 2,
            "precondition: 全文在该宽度下必须超过 2 行，实得 {}",
            full_line.lines.len()
        );
        assert_eq!(line.lines.len(), 2, "max_lines = 2");
        assert!(
            (size.height - line.height).abs() < 1e-3,
            "measure 高度必须等于 rasterize 高度：{} vs {}",
            size.height,
            line.height
        );
        assert!(
            (full_size.height - full_line.height).abs() < 1e-3,
            "对照：无 max_lines 时两者也必须一致：{} vs {}",
            full_size.height,
            full_line.height
        );
        let removed = full_size.height - size.height;
        assert!(
            removed > line.lines[0].line_height - 1e-3,
            "被截掉的行高必须从 measure 里消失（去掉 {removed}，单行高 {}）",
            line.lines[0].line_height
        );
    }

    /// P1-④：Definite 宽度 + `max_lines = 2` 的钳制，与「不设 `max_lines`」的对照。
    ///
    /// 断言：① 钳制后 measure 高度 == rasterize 高度，且等于**同一断行结果的前两行真实行高之和**；
    /// ② 严格小于全文高度；③ `max_lines = None` 时 measure 与 rasterize 完全一致（不回归）。
    #[test]
    fn measure_height_matches_rasterized_lines_for_definite_width() {
        const TEXT: &str = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
        let mut harness = Harness::new();
        let clamped = harness.shape(
            TEXT,
            TextStyle::new(16.0).with_overflow(crate::text::TextOverflow::Clip, 2),
        );
        let full = harness.shape(TEXT, TextStyle::new(16.0));
        let (min_width, max_width) = min_max_widths(&full);
        let width = min_width + (max_width - min_width) * 0.25;

        let size = clamped.measure(none_known(), definite(width));
        let full_size = full.measure(none_known(), definite(width));
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _, _) =
            rasterize_line(&clamped, width, 1.0, &mut raster, &mut glyphs, &mut atlas);
        let (full_line, _, _) =
            rasterize_line(&full, width, 1.0, &mut raster, &mut glyphs, &mut atlas);

        assert!(
            full_line.lines.len() > 2,
            "precondition: 全文在该宽度下必须超过 2 行，实得 {}",
            full_line.lines.len()
        );
        assert_eq!(line.lines.len(), 2, "max_lines = 2");
        assert!(
            (size.height - line.height).abs() < 1e-3,
            "measure 高度必须等于 rasterize 高度：{} vs {}",
            size.height,
            line.height
        );
        let expected = full_line.lines[0].line_height + full_line.lines[1].line_height;
        assert!(
            (size.height - expected).abs() < 1e-3,
            "钳制高度必须是前两行的真实行高之和：{} vs {expected}",
            size.height
        );
        assert!(
            size.height < full_size.height - 1.0,
            "钳制后必须严格矮于全文：{} vs {}",
            size.height,
            full_size.height
        );
        // 对照：max_lines = None ⇒ measure 与 rasterize 一致。
        assert!(
            (full_size.height - full_line.height).abs() < 1e-3,
            "max_lines = None 时 measure 必须等于 rasterize：{} vs {}",
            full_size.height,
            full_line.height
        );
    }

    /// P1-④：Min/MaxContent 路径同样钳制，且**不触发断行**（O(1) 契约不变）。
    ///
    /// 硬换行文本让 max-content 本身就有 4 行；`max_lines = 2` 必须把 Min/MaxContent 两条路径
    /// 的高度都钳到前两行，同时 `measure_breaks` 保持 0（复用
    /// [`measure_is_o1_for_min_max_content`] 的计数器）。`max_content_size()` 与
    /// `measure(MaxContent)` 同源，也必须一起钳制。
    #[test]
    fn max_lines_clamps_min_and_max_content_height_without_breaking() {
        const TEXT: &str = "one\ntwo\nthree\nfour";
        let mut harness = Harness::new();
        let shaped = harness.shape(
            TEXT,
            TextStyle::new(16.0).with_overflow(crate::text::TextOverflow::Clip, 2),
        );
        let full = harness.shape(TEXT, TextStyle::new(16.0));

        let max = shaped.measure(
            none_known(),
            taffy::Size {
                width: AvailableSpace::MaxContent,
                height: AvailableSpace::MaxContent,
            },
        );
        let min = shaped.measure(
            none_known(),
            taffy::Size {
                width: AvailableSpace::MinContent,
                height: AvailableSpace::MinContent,
            },
        );
        for _ in 0..32 {
            let _ = shaped.measure(
                none_known(),
                taffy::Size {
                    width: AvailableSpace::MinContent,
                    height: AvailableSpace::MinContent,
                },
            );
            let _ = shaped.measure(
                none_known(),
                taffy::Size {
                    width: AvailableSpace::MaxContent,
                    height: AvailableSpace::MaxContent,
                },
            );
        }
        assert_eq!(
            shaped.measure_breaks(),
            0,
            "Min/MaxContent 的 clamp 不得触发任何断行（O(1) 路径）"
        );

        // 期望值 = max-content 布局前两行的真实行高之和（硬换行 ⇒ 4 行）。
        let (expected, full_height) = {
            let layout = shaped.layout();
            let mut lines = layout.lines();
            let first = lines.next().expect("至少一行").metrics().line_height;
            let second = lines
                .next()
                .expect("硬换行 ⇒ 至少两行")
                .metrics()
                .line_height;
            assert_eq!(lines.count() + 2, 4, "硬换行 ⇒ max-content 恰好四行");
            (first + second, layout.height())
        };
        assert!(
            (max.height - expected).abs() < 1e-3,
            "MaxContent 高度必须钳到前两行：{} vs {expected}",
            max.height
        );
        assert!(
            (min.height - max.height).abs() < 1e-3,
            "MinContent 高度同样钳制（纯文本行行高相同）：{} vs {}",
            min.height,
            max.height
        );
        assert!(
            max.height < full_height - 1.0,
            "钳制后必须严格矮于全文：{} vs {full_height}",
            max.height
        );
        assert_eq!(
            shaped.max_content_size().height,
            max.height,
            "max_content_size 与 measure(MaxContent) 必须同源（都取钳制后的 max_size）"
        );

        // 与 rasterize 对齐：max-content 宽下光栅 ⇒ 硬换行 4 行、截到 2 行。
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _, _) = rasterize_line(
            &shaped,
            shaped.max_content_size().width,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        assert_eq!(line.lines.len(), 2, "max_lines = 2");
        assert!(
            (line.height - max.height).abs() < 1e-3,
            "MaxContent 的 measure 高度必须等于 rasterize 高度：{} vs {}",
            max.height,
            line.height
        );

        // 全文对照（同一文本不设 max_lines）。
        let full_size = full.measure(
            none_known(),
            taffy::Size {
                width: AvailableSpace::MaxContent,
                height: AvailableSpace::MaxContent,
            },
        );
        assert!(
            (full_size.height - full_height).abs() < 1e-3,
            "全文高度回归：{} vs {full_height}",
            full_size.height
        );
        assert!(max.height < full_size.height - 1.0);
    }

    /// P1-④：`overflow: Ellipsis` 不改变 measure 的宽度上界，高度与实际 `ShapedLine` 一致。
    ///
    /// 省略号只在最后一个可见行内做「保留前缀 + 省略号」的字形替换（`clip_with_ellipsis`），
    /// 只改 `advance` 与字形、**不改行高**，所以宽度上界必须与 `Clip` 样式逐位相同；高度必须
    /// 等于 rasterize 后 `ShapedLine::height`（单行与 `max_lines` 多行两种场景）。
    #[test]
    fn ellipsis_does_not_change_measure_width_and_matches_rasterized_height() {
        const TEXT: &str = "The quick brown fox jumps over the lazy dog";
        let mut harness = Harness::new();
        let ellipsis_shaped = harness.shape(
            TEXT,
            TextStyle::new(16.0)
                .with_wrap(TextWrap::NoWrap)
                .with_overflow(crate::text::TextOverflow::Ellipsis, 0),
        );
        let clip_shaped = harness.shape(
            TEXT,
            TextStyle::new(16.0)
                .with_wrap(TextWrap::NoWrap)
                .with_overflow(crate::text::TextOverflow::Clip, 0),
        );
        let full_width = ellipsis_shaped.max_content_size().width;
        let content_width = full_width * 0.5;
        assert!(
            content_width > 0.0 && content_width < full_width,
            "precondition: 内容宽必须真的裁剪"
        );

        let ellipsis_size = ellipsis_shaped.measure(none_known(), definite(content_width));
        let clip_size = clip_shaped.measure(none_known(), definite(content_width));
        assert_eq!(
            ellipsis_size.width.to_bits(),
            clip_size.width.to_bits(),
            "省略号不得改变宽度上界：{} vs {}",
            ellipsis_size.width,
            clip_size.width
        );
        assert_eq!(
            ellipsis_size.height.to_bits(),
            clip_size.height.to_bits(),
            "省略号不得改变高度"
        );
        assert!(
            (ellipsis_size.width - full_width).abs() < 1e-3,
            "NoWrap 的宽度上界是全文宽（内容宽只做绘制期裁剪）：{} vs {full_width}",
            ellipsis_size.width
        );

        let ellipsis = super::measure_ellipsis(
            &mut harness.font_cx,
            &mut harness.layout_cx,
            &harness.family,
            ellipsis_shaped.style(),
        )
        .expect("the host font must provide an ellipsis glyph");
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _, _) = rasterize_line_with_ellipsis(
            &ellipsis_shaped,
            content_width,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
            Some(&ellipsis),
        );
        assert!(
            line.width <= content_width + 0.01,
            "省略号裁剪后宽度必须在内容宽内：{} vs {content_width}",
            line.width
        );
        assert!(
            line.width < full_width - 1.0,
            "省略号必须真的缩短内容宽：{} vs {full_width}",
            line.width
        );
        assert!(
            (line.height - ellipsis_size.height).abs() < 1e-3,
            "ellipsis 单行高度必须等于 measure：{} vs {}",
            line.height,
            ellipsis_size.height
        );
        assert!(
            all_glyphs(&line).len() < TEXT.chars().count(),
            "必须真的丢掉尾部字形（否则省略号没生效）"
        );

        // 多行 + max_lines：省略号场景的高度同样 == ShapedLine 高度（截断 + 末行替换）。
        let multiline_ellipsis = harness.shape(
            TEXT,
            TextStyle::new(16.0).with_overflow(crate::text::TextOverflow::Ellipsis, 2),
        );
        let multiline_full = harness.shape(TEXT, TextStyle::new(16.0));
        let (min_width, max_width) = min_max_widths(&multiline_full);
        let wrap_width = min_width + (max_width - min_width) * 0.25;
        let multiline_size = multiline_ellipsis.measure(none_known(), definite(wrap_width));
        let (full_line, _, _) = rasterize_line(
            &multiline_full,
            wrap_width,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
        );
        let (line, _, _) = rasterize_line_with_ellipsis(
            &multiline_ellipsis,
            wrap_width,
            1.0,
            &mut raster,
            &mut glyphs,
            &mut atlas,
            Some(&ellipsis),
        );
        assert!(
            full_line.lines.len() > 2,
            "precondition: 全文在该宽度下必须超过 2 行，实得 {}",
            full_line.lines.len()
        );
        assert_eq!(line.lines.len(), 2, "max_lines = 2");
        assert!(
            (line.height - multiline_size.height).abs() < 1e-3,
            "ellipsis + max_lines 的高度必须等于 measure：{} vs {}",
            line.height,
            multiline_size.height
        );
        assert!(
            multiline_size.height < full_line.height - 1.0,
            "钳制后必须严格矮于全文：{} vs {}",
            multiline_size.height,
            full_line.height
        );
    }

    /// P1-④：高度用**真实逐行行高之和**，不是「可见行数 × 单一 line_height」。
    ///
    /// 硬换行 + 行内盒：前两行被 40 / 60 px 的行内盒撑高，后两行是纯文本行。`max_lines = 2`
    /// 后 measure 高度必须是这两行的真实行高之和，且明显偏离 `2 × 纯文本行高` —— 后者正是
    /// 「用单一 line_height 乘行数」会算出来的错误值。
    #[test]
    fn max_lines_height_uses_real_per_line_heights() {
        const TEXT: &str = "aa\nbb\ncc\ndd";
        let mut harness = Harness::new();
        let boxes = [
            InlineBoxSpec {
                id: 1,
                index: 1,
                size: crate::math::Size::new(30.0, 40.0),
                baseline: None,
                align: InlineAlign::Baseline,
            },
            InlineBoxSpec {
                id: 2,
                index: 4,
                size: crate::math::Size::new(30.0, 60.0),
                baseline: None,
                align: InlineAlign::Baseline,
            },
        ];
        let shaped = harness.shape_with(
            TEXT,
            TextStyle::new(16.0).with_overflow(crate::text::TextOverflow::Clip, 2),
            &boxes,
        );
        let full = harness.shape_with(TEXT, TextStyle::new(16.0), &boxes);
        let width = shaped.max_content_size().width;

        let size = shaped.measure(none_known(), definite(width));
        let (mut raster, mut glyphs, mut atlas) = glyph_runner();
        let (line, _, _) =
            rasterize_line(&shaped, width, 1.0, &mut raster, &mut glyphs, &mut atlas);
        let (full_line, _, _) =
            rasterize_line(&full, width, 1.0, &mut raster, &mut glyphs, &mut atlas);

        assert_eq!(line.lines.len(), 2, "max_lines = 2");
        assert_eq!(full_line.lines.len(), 4, "硬换行 ⇒ 全文四行");
        let plain_line_height = full_line.lines[2].line_height;
        assert!(
            line.lines[0].line_height > plain_line_height + 1.0
                && line.lines[1].line_height > plain_line_height + 1.0,
            "precondition: 行内盒必须把前两行撑高（{} / {} vs 纯文本行 {plain_line_height}）",
            line.lines[0].line_height,
            line.lines[1].line_height
        );

        let expected = line.lines[0].line_height + line.lines[1].line_height;
        assert!(
            (size.height - expected).abs() < 1e-3,
            "measure 必须是前两行真实行高之和：{} vs {expected}",
            size.height
        );
        assert!(
            (size.height - line.height).abs() < 1e-3,
            "measure 高度必须等于 rasterize 高度：{} vs {}",
            size.height,
            line.height
        );
        let uniform = 2.0 * plain_line_height;
        assert!(
            (size.height - uniform).abs() > 1.0,
            "不得用「可见行数 × 单一 line_height」：{} vs {uniform}",
            size.height
        );
        assert!(
            size.height < full_line.height - 1.0,
            "钳制后必须严格矮于全文：{} vs {}",
            size.height,
            full_line.height
        );
    }

    /// P1-④：单行文本 + `max_lines = 1` 的可见行数是 `min(1, 1) = 1`，**不得被钳成 0**。
    ///
    /// `TextStyle::max_lines()` 已把 0 映射成 `None`；数字快路径（`ShapedKind::Simple`）恒为
    /// 单行且不经过任何 clamp 代码，所以这里对 parley 路径与快路径共同适用的语义做端到端断言：
    /// 高度 > 0、等于真实行高、且与不设 `max_lines` 时逐位相同。
    #[test]
    fn single_line_with_max_lines_one_keeps_its_height() {
        let mut harness = Harness::new();
        let clamped = harness.shape(
            "Hello",
            TextStyle::new(16.0).with_overflow(crate::text::TextOverflow::Clip, 1),
        );
        let plain = harness.shape("Hello", TextStyle::new(16.0));
        let size = clamped.measure(none_known(), definite(100.0));
        let plain_size = plain.measure(none_known(), definite(100.0));
        let line_height = {
            let layout = clamped.layout();
            layout
                .lines()
                .next()
                .expect("Hello 必须有一行")
                .metrics()
                .line_height
        };

        assert!(size.height > 0.0, "单行不得被钳成 0：{:?}", size);
        assert!(
            (size.height - line_height).abs() < 1e-3,
            "单行高度必须等于真实行高：{} vs {line_height}",
            size.height
        );
        assert_eq!(
            size.height.to_bits(),
            plain_size.height.to_bits(),
            "max_lines = 1 与不设上限在单行文本上必须逐位相同"
        );
    }

    #[test]
    fn raster_ppem_is_physical_and_grows_with_scale() {
        // R12-4：布局是逻辑坐标（宽度与 scale 无关），ppem 与位图尺寸才是物理量。
        let mut harness = Harness::new();
        if crate::text::raster::probe_font().is_none() {
            return;
        }
        let shaped = harness.shape("H", TextStyle::new(16.0));
        let logical_width = shaped.max_content_size().width;
        let mut physical = Vec::new();
        for scale in [1.0f32, 1.25, 2.0] {
            let (mut raster, mut glyphs, mut atlas) = glyph_runner();
            let (line, _, stats) = rasterize_line(
                &shaped,
                logical_width,
                scale,
                &mut raster,
                &mut glyphs,
                &mut atlas,
            );
            assert_eq!(stats.rasters, 1, "unique ppem ⇒ one raster");
            assert_eq!(line.width, logical_width, "逻辑宽度与 scale 无关");
            let glyph = all_glyphs(&line).pop().expect("glyph");
            physical.push(glyph.size.height * scale);
        }
        assert!(
            physical[1] > physical[0],
            "physical height grows with the scale: {physical:?}"
        );
        assert!(
            physical[2] > physical[1],
            "physical height grows with the scale: {physical:?}"
        );
    }

    #[test]
    fn explicit_family_changes_the_shape() {
        // B3：`TextStyle.family` 必须**前置**进 family 列表 —— 用列表内容断言，不依赖宿主字体。
        // （原实现按 advance 比较三个 Windows 专有家族名；Linux/macOS 上它们都不存在，
        //  三者 fallback 到同一字体、advance 完全相同，测试必然失败。）
        let stack = FontStack::new(["Consolas"]);
        let with_custom = family_list(&stack, Some("Times New Roman"));
        let without = family_list(&stack, None);

        let names = |family: &parley::FontFamily<'static>| -> Vec<String> {
            match family {
                parley::FontFamily::List(list) => list
                    .iter()
                    .map(|name| match name {
                        parley::FontFamilyName::Named(name) => name.to_string(),
                        parley::FontFamilyName::Generic(generic) => format!("{generic:?}"),
                    })
                    .collect(),
                parley::FontFamily::Single(name) => vec![format!("{name:?}")],
                parley::FontFamily::Source(_) => vec!["<source>".to_owned()],
            }
        };

        let with_custom = names(&with_custom);
        let without = names(&without);
        assert_eq!(
            with_custom.first().map(String::as_str),
            Some("Times New Roman"),
            "显式 family 必须是列表首项（优先级最高），实际：{with_custom:?}"
        );
        assert_eq!(
            with_custom.get(1).map(String::as_str),
            Some("Consolas"),
            "字体栈必须排在显式 family 之后，实际：{with_custom:?}"
        );
        assert_eq!(
            without.first().map(String::as_str),
            Some("Consolas"),
            "没有显式 family 时栈首项就是最高优先级，实际：{without:?}"
        );
        assert_eq!(
            with_custom.len(),
            without.len() + 1,
            "显式 family 只多一项，不改变 fallback 结构：{with_custom:?} vs {without:?}"
        );
    }

    #[test]
    fn explicit_family_changes_advances_when_the_host_has_both_fonts() {
        // 家族真的能改变 advance —— 只在宿主同时装了这两族时才有意义（跨平台可跳过）。
        let mut harness = Harness::new();
        let candidates = [
            "Consolas",
            "Times New Roman",
            "Segoe UI",
            "DejaVu Sans",
            "Liberation Serif",
        ];
        let mut widths = Vec::new();
        for family in candidates {
            harness.family = family_list(&FontStack::new([family]), None);
            let shaped = harness.shape("Hamburgefonstiv", TextStyle::new(16.0));
            widths.push((family, shaped.max_content_size().width));
        }
        let distinct = widths
            .iter()
            .map(|(_, width)| width.to_bits())
            .collect::<std::collections::HashSet<_>>();
        if distinct.len() < 2 {
            eprintln!("skip: host resolves every candidate family to the same font: {widths:?}");
            return;
        }
        assert!(
            distinct.len() >= 2,
            "different families must produce different advances: {widths:?}"
        );
    }
}
