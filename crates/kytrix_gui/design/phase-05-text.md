# Phase 05 · 文本子系统 — 详细设计（v1）

> 关联：`custom-gui-engine-design.md` §4.5（Parley 布局/断行/缓存）、§4.6（D17 字体策略）、§4.7（行内流/选区）、
> §4.8（错误路径）、§5.6（图集）、§9.3（缓存预算）、§2.8（图集映射）；
> `design/phase-01-foundation.md` §4（模块根模板）、§4.5（公开路径）、附录 A.2（parley 事实）、A.6（etagere）；
> `design/phase-02-paint-foundation.md` §10（`AtlasTile`/`AtlasKind`/UV/代际）、§6.3（`GlyphRun`/`PositionedGlyph`）、§11（`Canvas`）；
> `design/phase-04-layout.md` §8（`LayoutBox`/尺寸预算）、§9（`MeasureFn`/`invalidate_measure`）、§4.3（dead_code 审计表）。
> 已落地代码：`crates/kytrix_gui/src/{app,window,error,lib}.rs`、`node/{tree,builder,dirty}.rs`、`layout/*`、`paint/*`。
>
> **本阶段只做设计 + API 核验 + 可编译 probe，不写生产代码**；所有签名/行号来自本机 registry 源码
> （`E:\.cargo\registry\src\rsproxy.cn-e3de039b2554c837\`）与 `%TEMP%` 下的 probe（§2.4，可复现），**未使用 web_fetch**。

---

## 0. 结论摘要

| # | 结论 | 依据 |
|---|---|---|
| 1 | Parley 0.12 **有** `parley::style::TextStyle`（26 字段），但它是 `StyleRunBuilder`/`TreeBuilder` 的「整段样式」入口，**`RangedBuilder` 不吃它**；P05 走 `RangedBuilder` + `StyleProperty`（26 变体），与 F-03 的裁决一致 | §2.1、§7.1 |
| 2 | `ranged_builder(fcx, text, scale, quantize)` 只有 **5 参**；`scale` 会**乘进所有坐标**（probe：同一文本 `scale=1→width 15.109`、`scale=2→30.219`），`quantize=true` **只对齐块轴（baseline）**，不量化 inline x | §2.1、§2.3① |
| 3 | 形状缓存键 = `(text hash, TextStyle hash, scale, font_stack_version)`；**不含** `atlas_generation`（主设计 §4.5 的键要拆成两级，F-53） | §7.2 |
| 4 | 宽度变化**不重建 `Layout`**：`break_all_lines(Some(w))` + `align(..)` 可重复调用（parley 文档原话 + probe 实测 5 行↔2 行）；文本/样式/scale 变化才重建 | §2.3②、§7.3 |
| 5 | 文本测量**不碰 `&mut` 引擎**：`App::refresh_text` 把 `Rc<ShapedText>`（内含 parley `Layout`）**重建进节点的 `measure` 闭包**；闭包只做 `layout.clone()` + `break_all_lines` + `align`。零 `RefCell`、零 `unsafe`，`MeasureFn` 与 P04 的 `compute_leaf` **一行都不用改** | §7.4、§13.3 |
| 6 | 空文本是 parley 0.12 的坑：`text=""` ⇒ **`lines()==1`、`width()==4.4453`、`height()==18`**（16 px Segoe UI 实测）。measure 必须把空文本特例化为 `Size::ZERO` | §2.3③、§7.5 |
| 7 | 光栅：skrifa 0.44 只出轮廓（**无光栅器**）→ swash 0.2.10 `Render::new(&[Source::Outline]).format(Format::Alpha)` 出 R8 alpha；`Placement{left,top,w,h}`，`top` 是**基线上方的像素数**，`left` 可为负（`j` = -2），`data.len()==w*h` | §2.2、§8 |
| 8 | 图集：etagere 0.3 单页、`allocate→None` 即满、**`deallocate` 对陈旧 id 会 panic**（probe 实测 `assertion failed: allocated`）、返回矩形会被**向上取整**（请求 8×11 得 8×16）⇒ 分配器必须自己记录 `AllocId` 做幂等释放 | §2.3④、§9 |
| 9 | 每个 glyph run 自带字体：`Run::font() -> &FontData { data: Blob<u8>, index: u32 }`（TTC 索引用 `index`）。probe：`A`→Segoe UI、`中`→msyh.ttc#1、`🌍`→Segoe UI Emoji、`한`→Malgun Gothic，**五个 run 五个字体文件** | §2.3⑤、§8.2 |
| 10 | D17 的 CJK 栈**不需要 `#[cfg]`**：`Query::set_families` 对不存在的家族名**静默跳过**（`query.rs:66-70`），因此一个跨平台有序列表即可；`fontique` 的 `Collection::new` 实测 **2.4 ms / 111 家族**，可进程级复用 | §2.2、§6.2 |
| 11 | `TextContext` **不公开**（crate 私有）：它必须持有 `parley::{FontContext, LayoutContext}`，公开即把第三方类型拉进公开面（F-35 同类）。公开替代 = `App::{set_font_stack, font_stack, font_stack_version, reload_system_fonts, refresh_text, text_selection_rects}` + `TextPassReport`（F-54） | §4、§6.1 |
| 12 | `Node` 尺寸：现状 **1016 B**（实测），`NodeKind` 48 B（= `Canvas`）。`TextStyle` 72 B + `SmolStr` 24 B ⇒ `TextContent` **96 B** ⇒ `NodeKind` **96 B**（载荷吃掉了 niche/tag，实测）⇒ `Node` **1088 B**（`text_bind` 是 `Option<Box<dyn Fn>>` = **16 B** + `text_measured_size` 8 B + 48 B 的 `NodeKind` 增量），仍在 P04 的 1152 B 上界内（S-53、§14） | §2.4④、§14 |
| 13 | 必须开 `parley` 的 **`complex-scripts` feature**：不开则 ICU4X 无 CJK 词典模型，每段中文都往 stderr 打 `ICU4X data error: No segmentation model for complex script: Chinese/Japanese`（probe 实测），断行退化为 UAX #29 WB999 | §2.2、§17 |
| 14 | 图集/光栅在 **P05 落地**并由文本 pass 消费（`App::refresh_text` → `ShapedLine` 的每个 glyph 带 `AtlasTile`），P06 只做 `ShapedLine → Scene` 的实例编码（S-54）；否则 `render/atlas.rs` 的 `pub(crate)` 无**非 test** 消费者，`-D warnings` 直接红 | §4.3、§9.6 |

---

## 1. 范围与非目标

### 1.1 本阶段交付（新增文件）

| 文件 | 内容 |
|---|---|
| `src/text.rs` | **公共模块根**：`TextStyle` / `FontStack` / `InlineBoxSpec` / `ShapedLine` / `ShapedTextLine` / `ShapedTextItem` / `ShapedGlyphRun` / `ShapedInlineBox` / `TextContent` / `TextPassReport` / `TextDiagnostic`（§5） |
| `src/text/context.rs` | `TextContext`（crate 私有）：`FontContext` + `LayoutContext<ColorBrush>` + `FontStack` + 家族解析缓存 + tofu 诊断 + `font_stack_version`（§6） |
| `src/text/shape.rs` | `shape`（唯一 `RangedBuilder` 调用点）、`ShapedText`、`rebreak`、内容宽度（§7） |
| `src/text/raster.rs` | skrifa 度量 + swash alpha 光栅，`GlyphRasterizer`（§8） |
| `src/text/atlas.rs` | 字形 `key → AtlasTile` 缓存 + `AtlasSet`（Glyph/Image 两页）的文本侧封装（§10） |
| `src/text/cache.rs` | 形状缓存（LRU + 容量/字节预算）与 `ShapeKey`（§11） |
| `src/text/inline_flow.rs` | `InlineBoxSpec → parley::InlineBox`、`(anchor, focus)` 字节偏移 → `Rect` 映射（§12） |
| `src/render.rs` | **crate 私有模块根**：`pub(crate) mod atlas;` + `pub(crate) use atlas::{AtlasAllocator, AtlasSet, DirtyRows, Allocation};`（P01 §4.1 模板） |
| `src/render/atlas.rs` | etagere 分配器、单页、1 纹素 padding、UV、脏行、`generation`、清空重建、`AtlasExhausted`（§9） |

### 1.2 本阶段修改

| 文件 | 修改 |
|---|---|
| `src/node/tree.rs` | `NodeKind::Text(TextContent)` + `is_leaf` + `Node.text_bind` + 手写 `Debug` 增字段 + `NodeTree.{text, text_slots, set_text_content, set_text_shaped, text_shaped, text_slot, invalidate_measure_for_text}`（§13） |
| `src/node/builder.rs` | `NodeBuilder::text(text, style)` / `NodeBuilder::bind_text(f)`（§13.2） |
| `src/app.rs` | `App.text: TextContext` + `App.atlas: AtlasSet` + `App.raster: GlyphRasterizer` + `refresh_styles` 求值 `bind_text` + 新 pass `App::refresh_text` + `set_font_stack` / `reload_system_fonts` / `text_selection_rects`（§13.3） |
| `src/window.rs` | `Window::set_scale` 记录 `scale` 的**文本影响**：`Window::scale()` 已是唯一来源；新增 `Window::scale_epoch()`（u32，`set_scale` 递增）供文本 pass 判定「图集要换代」；`compute_layout` 的 rustdoc 写明「文本节点必须在本帧先跑 `App::refresh_text`」（§13.4） |
| `src/error.rs` | 落地 `EngineError::TextShape { node, detail }`（P01 §6.2.2 #13，逐字）；`AtlasExhausted { kind }`（#12）随分配器落地（§9.5） |
| `src/lib.rs` | `pub mod text;` + `mod render;`（§3） |
| `crates/kytrix_gui/Cargo.toml` | **零新依赖**；`parley` 的 workspace 声明加 `features = ["complex-scripts"]`（§17） |

### 1.3 明确的非目标（交给后续 Phase）

| 不做 | 归属 |
|---|---|
| `ShapedLine → Scene` 的 `Primitive::GlyphRun` 实例编码、lane 分配、paint pass | P06 |
| GPU 纹理上传 / 脏行上传 / 采样器 / bindless | P07 / P09 |
| `ResourceCache` / 图片解码 / 图片图集填充 | P15 |
| 选区**交互**（鼠标拖拽、双击选词、剪贴板） | P10（本阶段只给 `(anchor, focus)` → `Rect` 的数据映射） |
| a11y 树、`SetTextSelection` action、IME | P14 / P12 |
| Markdown AST → `Vec<InlineFragment>` 的数据管线、链接下划线/`kbd`/行内图片的**渲染** | 拥有 `base_ui/markdown` 的 Phase（本阶段只定 `InlineBoxSpec` 与 range 机制，§12.4） |
| 字体子集化 / 内化字体文件 / 字形 hint 自定义（CFF 解释器调参） | 不做（D17） |
| 多页图集 / LRU 逐出 / 页扩容 | 不做（P02 §10.6 裁决；风险 V-14 / V-44） |

---

## 2. 事实基线（全部来自真实源码 + 本机 probe）

### 2.1 parley 0.12.0 逐字签名（`parley-0.12.0/src/`）

> **与 P01 附录 A.2 的两处修正**：①A.2 的行号是旧版，本节的 `layout/line.rs` / `layout/layout.rs` 行号以 0.12.0
> 实包为准；②`Layout::width()` 之外还有 `full_width()`，measure 必须区分（§7.5）。

```rust
// lib.rs:135-145 —— 唯一出口（无 pub use 之外的再导出层）
pub use builder::{RangedBuilder, StyleRunBuilder, TreeBuilder};
pub use context::LayoutContext;
pub use font::FontContext;
pub use inline_box::{InlineBox, InlineBoxKind};
pub use layout::Layout;                       // doc(inline)
pub use editing::*; pub use layout::*; pub use style::*;   // :143-145
pub use parlance::BaseDirection;              // :129

// font.rs:11-14 —— FontContext 只有两个 pub 字段 + new()
pub struct FontContext { pub collection: fontique::Collection, pub source_cache: fontique::SourceCache }
impl FontContext { pub fn new() -> Self }     // :19，doc: "discovering system fonts if available"

// context.rs:24 / :49 / :96 —— 5 参，第 4 = scale，第 5 = quantize
pub struct LayoutContext<B: Brush = [u8; 4]>;
impl<B: Brush> LayoutContext<B> {
    pub fn new() -> Self;                                                       // :49
    pub fn ranged_builder<'a>(&'a mut self, fcx: &'a mut FontContext,
                              text: &'a str, scale: f32, quantize: bool) -> RangedBuilder<'a, B>;  // :96
}

// builder.rs:51/59/71/82/103 —— RangedBuilder 的全部相关方法
pub fn push_default<'a>(&mut self, property: impl Into<StyleProperty<'a, B>>);                  // :51
pub fn push<'a>(&mut self, property: impl Into<StyleProperty<'a, B>>, range: impl RangeBounds<usize>); // :59（字节偏移）
pub fn push_inline_box(&mut self, inline_box: InlineBox);                                       // :71
pub fn set_base_direction(&mut self, base_direction: BaseDirection);                            // :82
pub fn build(self, text: impl AsRef<str>) -> Layout<B>;                                         // :103

// style/mod.rs:227 —— 26 变体（逐字见 §7.1 的映射表）
pub enum StyleProperty<'a, B: Brush> { FontFamily(..), FontSize(f32), FontWidth(..), FontStyle(..),
    FontWeight(..), FontVariations(..), FontFeatures(..), Locale(Option<Language>), Brush(B),
    Underline(bool), UnderlineOffset(Option<f32>), UnderlineSize(Option<f32>), UnderlineBrush(Option<B>),
    Strikethrough(bool), StrikethroughOffset(Option<f32>), StrikethroughSize(Option<f32>),
    StrikethroughBrush(Option<B>), LineHeight(LineHeight), VerticalAlign(VerticalAlign),
    WordSpacing(f32), LetterSpacing(f32), WordBreak(WordBreak), LineBreak(LineBreak),
    OverflowWrap(OverflowWrap), TextWrapMode(TextWrapMode), WhiteSpaceCollapse(WhiteSpaceCollapse) }  // :226-280

// style/mod.rs:49-61 —— LineHeight（NORMAL = MetricsRelative(1.0)）
pub enum LineHeight { MetricsRelative(f32), FontSizeRelative(f32), Absolute(f32) }

// inline_box.rs:6-27 —— InlineBox；index 是「文本字节偏移」，必须落在 char boundary
pub struct InlineBox { pub id: u64, pub kind: InlineBoxKind, pub index: usize,
                       pub width: f32, pub height: f32, pub baseline: Option<f32>,
                       pub vertical_align: VerticalAlign }
pub enum InlineBoxKind { InFlow, OutOfFlow, CustomOutOfFlow }        // :45-62
// push_inline_box 保留调用方的 index，并按 index 稳定排序（builder.rs:390-395）

// style/mod.rs:163-168 —— VerticalAlign 是 struct（alignment + shift），不是 enum
pub struct VerticalAlign { pub alignment: AlignmentBaseline, pub shift: BaselineShift }
impl VerticalAlign { pub const BASELINE/SUB/SUPER/TEXT_TOP/TEXT_BOTTOM/MIDDLE/TOP/BOTTOM: Self }

// parlance-0.1.1/font_family.rs:184-194 —— 字体栈的载体
pub enum FontFamily<'a> { Source(Cow<'a, str>), Single(FontFamilyName<'a>), List(Cow<'a, [FontFamilyName<'a>]>) }
pub enum FontFamilyName<'a> { Named(Cow<'a, str>), Generic(GenericFamily) }   // :80-86

// layout/layout.rs —— 只读输出与重排
pub fn width(&self) -> f32;            // :104 不含悬挂空白
pub fn full_width(&self) -> f32;       // :109 含尾部空白
pub fn first_baseline(&self) -> Option<f32>;   // :115
pub fn height(&self) -> f32;           // :135
pub fn calculate_content_widths(&self) -> ContentWidths;   // :130（每次重算）
pub fn lines(&self) -> impl ExactSizeIterator<Item = Line<'_, B>> + DoubleEndedIterator + Clone + '_; // :177
pub fn inline_boxes(&self) -> impl ExactSizeIterator<Item = &InlineBox> + '_;                 // :167
pub fn break_all_lines(&mut self, max_advance: Option<f32>);   // :210 可重复调用
pub fn align(&mut self, alignment: Alignment, options: AlignmentOptions);  // :226
pub fn text_len(&self) -> usize;       // :87（**Layout 不存文本**）
pub fn alignment(&self) -> Option<Alignment>;   // :94
// layout/mod.rs:93-100
pub struct ContentWidths { pub min: f32, pub max: f32 }

// layout/line.rs —— 行与项
pub fn metrics(&self) -> &LineMetrics;  // :26
pub fn text_range(&self) -> Range<usize>;   // :35
pub fn items(&self) -> impl Iterator<Item = PositionedLayoutItem<'a, B>> + Clone + '_;   // :142
pub struct LineMetrics { pub line_height: f32, pub baseline: f32, pub offset: f32, pub indent: f32,
    pub advance: f32, pub hanging_advance: f32, pub inline_min_coord: f32, pub inline_max_coord: f32,
    pub block_min_coord: f32, pub block_max_coord: f32, pub content_block_min_coord: f32,
    pub content_block_max_coord: f32 }   // :158-234
pub enum PositionedLayoutItem<'a, B: Brush> { GlyphRun(GlyphRun<'a, B>), InlineBox(PositionedInlineBox) }  // :261
pub struct PositionedInlineBox { pub x: f32, pub y: f32, pub width: f32, pub height: f32,
    pub baseline: Option<f32>, pub id: u64, pub kind: InlineBoxKind }   // :268
impl<'a, B: Brush> GlyphRun<'a, B> {
    pub fn run(&self) -> &Run<'a, B>;      // :298
    pub fn style(&self) -> &Style<B>;      // :305
    pub fn baseline(&self) -> f32;         // :317（布局块轴绝对 y）
    pub fn offset(&self) -> f32;           // :322（布局行内轴绝对 x）
    pub fn advance(&self) -> f32;          // :327
    pub fn glyphs(&'a self) -> impl Iterator<Item = Glyph> + Clone + 'a;            // :332（run 相对）
    pub fn positioned_glyphs(&'a self) -> impl Iterator<Item = Glyph> + Clone + 'a; // :337（x+=offset, y+=baseline）
}

// parley_engine-0.12.0/src/glyph.rs:4-12 —— 坐标单位是 f32 布局单位（= 逻辑 px × scale），x 行内、y 为基线
pub struct Glyph { pub id: u32, pub x: f32, pub y: f32, pub advance: f32 }

// layout/run.rs —— 每个 run 自带字体（TTC 索引用 index）
pub fn font(&self) -> &FontData;             // :86
pub fn font_size(&self) -> f32;              // :98
pub fn font_attrs(&self) -> &fontique::Attributes;   // :103
pub fn normalized_coords(&self) -> &[NormalizedCoord];  // :114
pub fn synthesis(&self) -> Synthesis;        // :108
pub fn text_range(&self) -> Range<usize>;    // :149
pub fn clusters(&self) -> impl Iterator<Item = Cluster<'a, B>> + Clone;   // :200
// linebender_resource_handle-0.1.1/font.rs:8-15（parley::FontData）
pub struct FontData { pub data: Blob<u8>, pub index: u32 }

// 选区：**不在 Layout 上**，在 editing 类型上（`&Layout` 即可，无需 &mut）
pub fn Cursor::from_byte_index<B: Brush>(layout: &Layout<B>, index: usize, affinity: Affinity) -> Self; // cursor.rs:17
pub fn Cursor::from_point<B: Brush>(layout: &Layout<B>, x: f32, y: f32) -> Self;    // :38
pub fn Cursor::index(&self) -> usize;      // :76
pub fn Cursor::geometry<B: Brush>(&self, layout: &Layout<B>, width: f32) -> BoundingBox;  // :279（f64 字段）
pub fn Selection::new(anchor: Cursor, focus: Cursor) -> Self;                       // selection.rs:31
pub fn Selection::from_byte_index<B: Brush>(layout: &Layout<B>, index: usize, affinity: Affinity) -> Self; // :42
pub fn Selection::text_range(&self) -> Range<usize>;                                // :159
pub fn Selection::geometry<B: Brush>(&self, layout: &Layout<B>) -> Vec<(BoundingBox, usize)>;  // :491
pub fn Selection::geometry_with<B: Brush>(&self, layout: &Layout<B>, f: impl FnMut(BoundingBox, usize)); // :503

// layout/alignment.rs:11/37/59/69
pub enum Alignment { Start, End, Left, Center, Right, Justify }     // :11
pub struct AlignmentOptions { pub align_when_overflowing: bool, pub last_line_alignment: Option<Alignment> } // :37
impl Default for AlignmentOptions { /* false, None */ }             // :59
// 语义：align() 每次从 offset = indent（LTR）/ -hanging_advance（RTL）重算，因此**反复 align 不累积**（alignment.rs:69-147）

// style/brush.rs:5-7 —— Brush 是标记 trait（ColorBrush 自动满足）
pub trait Brush: Clone + PartialEq + Default + core::fmt::Debug {}
impl<T: Clone + PartialEq + Default + core::fmt::Debug> Brush for T {}

// parlance-0.1.1/text.rs —— 换行/断行属性
pub enum WordBreak { Normal, BreakAll, KeepAll }               // :20
pub enum LineBreak { Loose, Normal, Strict, Anywhere }         // :34（默认 Strict）
pub enum OverflowWrap { Normal, Anywhere, BreakWord }          // :55
pub enum TextWrapMode { Wrap, NoWrap }                         // :69
```

**`Style<B>` 的可读面很窄**（`layout/mod.rs:50-75`，无任何 `impl Style`）：只有 `pub brush: B` / `pub underline` /
`pub strikethrough` / `pub locale`；`font_size` 要从 `Run::font_size()` 读，`letter_spacing` 根本不可读 ⇒
**`ShapedLine` 不能靠回读 parley 判定样式，必须在 shape 时把输入 `TextStyle` 留在缓存条目里**（§11.2）。

### 2.2 fontique 0.12 / skrifa 0.44 / swash 0.2.10 / etagere 0.3.0（逐字，附行号）

```rust
// ── fontique 0.12.0 ──（src/collection/mod.rs 与 query.rs）
pub struct CollectionOptions { pub shared: bool, pub system_fonts: bool }   // :37（默认 false / true，:55）
impl Collection {
    pub fn new(options: CollectionOptions) -> Self;                   // :81（system 特性下构造即枚举系统字体）
    pub fn load_system_fonts(&mut self);                              // :95（幂等）
    pub fn family_names(&mut self) -> impl Iterator<Item = &str> + '_ + Clone;   // :111
    pub fn family_id(&mut self, name: &str) -> Option<FamilyId>;      // :124（大小写不敏感）
    pub fn family_name(&mut self, id: FamilyId) -> Option<&str>;      // :129
    pub fn family(&mut self, id: FamilyId) -> Option<FamilyInfo>;     // :134
    pub fn fallback_families(&mut self, key: impl Into<FallbackKey>) -> impl Iterator<Item = FamilyId> + '_ + Clone; // :173
    pub fn query<'a>(&'a mut self, source_cache: &'a mut SourceCache) -> Query<'a>;   // :200（唯一构造点）
}
impl<'a> Query<'a> {
    pub fn set_families<'f, I>(&mut self, families: I) where I: IntoIterator, I::Item: Into<QueryFamily<'f>>; // :56
    pub fn set_attributes(&mut self, attributes: Attributes);         // :83
    pub fn set_fallbacks(&mut self, key: impl Into<FallbackKey>);     // :96（**追加 Hani 兜底**，:105-112）
    pub fn matches_with<'q>(&'q mut self, f: impl FnMut(&'q QueryFont) -> QueryStatus);  // :122（没有 matches()）
    pub fn first_available_font(&mut self) -> Option<QueryFont>;      // :181
}
pub enum QueryFamily<'a> { Named(&'a str), Id(FamilyId), Generic(GenericFamily) }   // :213
pub struct QueryFont { pub family: (FamilyId, usize), pub blob: Blob<u8>, pub index: u32,
                       pub synthesis: Synthesis, pub charmap_index: CharmapIndex }  // :242
pub struct FallbackKey;  pub fn new(script: impl Into<Script>, locale: Option<&Language>) -> Self;  // fallback.rs:125
pub struct Attributes { pub width: FontWidth, pub style: FontStyle, pub weight: FontWeight }  // attributes.rs:17（**无 family**）
pub struct Script;  pub const fn from_bytes(raw: [u8; 4]) -> Self;   // parlance script.rs:36（无 Script::Latin/Han 常量）
pub struct FontWeight(f32);  pub const fn new(weight: f32) -> Self;   // parlance font.rs:56
pub enum FontStyle { Normal, Italic, Oblique(Option<f32>) }          // parlance font.rs:300
// SourceCache: new(SourceCacheOptions)（source_cache.rs:52）/ get(&SourceInfo)（:95）/ prune(max_age, prune_failed)（:150）

// ── skrifa 0.44.0 ──（read-fonts 0.41.0 提供 FontRef）
pub fn FontRef::new(data: &[u8]) -> Result<Self, ReadError>;                     // read-fonts lib.rs:349（单字体）
pub fn FontRef::from_index(data: &[u8], index: u32) -> Result<Self, ReadError>;   // :365（TTC，必须传 QueryFont.index）
pub fn charmap(&self) -> Charmap;  pub fn map(&self, ch: impl Into<u32>) -> Option<GlyphId>;   // skrifa charmap.rs:102
pub fn metrics(&self, size: Size, location: impl Into<LocationRef>) -> Metrics;   // provider.rs:37
pub struct Metrics { pub units_per_em: u16, pub ascent: f32, pub descent: f32, pub leading: f32,
    pub underline: Option<Decoration>, pub strikeout: Option<Decoration>, /* … */ }  // metrics.rs:72-103
pub struct Decoration { pub offset: f32, pub thickness: f32 }        // metrics.rs:48（**没有 underline_offset/size 字段名**）
pub fn glyph_metrics(&self, size: Size, location: impl Into<LocationRef>) -> GlyphMetrics;   // provider.rs:41
pub fn GlyphMetrics::advance_width(&self, gid: GlyphId) -> Option<f32>;   // metrics.rs:291（另有 left_side_bearing :320、bounds :352）
pub fn Size::new(ppem: f32) -> Self;  pub fn Size::unscaled() -> Self;    // instance.rs:23/29（px vs 字体单位）
pub fn outline_glyphs(&self) -> OutlineGlyphCollection;               // provider.rs:113
pub fn OutlineGlyphCollection::get(&self, gid: GlyphId) -> Option<OutlineGlyph<'a>>;  // outline/mod.rs:639
pub fn OutlineGlyph::draw<'s>(&self, settings: impl Into<DrawSettings<'a>>, pen: &mut impl OutlinePen)
    -> Result<AdjustedMetrics, DrawError>;                            // outline/mod.rs:353
pub struct DrawSettings<'a>;  pub fn unhinted(size: Size, location: impl Into<LocationRef<'a>>) -> Self;  // :189
pub trait OutlinePen { move_to/line_to/quad_to/curve_to/close }        // read-fonts model/pen.rs:8（`Vec<PathElement>` 已实现 :51）
// **skrifa 没有光栅器**（无 raster/rasterize 符号；bitmap.rs 只读嵌入 bitmap strike）

// ── swash 0.2.10 ──（src/scale/mod.rs，除注明）
pub struct ScaleContext;  pub fn new() -> Self;                        // :287/:304（每线程一个；内部 LRU 字体缓存默认 8）
pub fn builder<'a>(&'a mut self, font: impl Into<FontRef<'a>>) -> ScalerBuilder<'a>;   // :328（**没有 From<&[u8]>**）
pub fn ScalerBuilder::size(mut self, ppem: f32) -> Self;               // :394（0.0 = 字体单位）
pub fn ScalerBuilder::hint(mut self, yes: bool) -> Self;               // :400（hinting 是 **builder** 的属性）
pub fn ScalerBuilder::normalized_coords<I>(self, coords: I) -> Self;   // :431
pub fn ScalerBuilder::build(self) -> Scaler<'a>;                       // :446（Scaler 借 context，一次一个）
pub fn Render::new(sources: &'a [Source]) -> Self;                     // :789（**无 size 参数**）
pub fn Render::format(&mut self, format: Format) -> &mut Self;         // :803（Format 来自 `swash::zeno`）
pub fn Render::offset(&mut self, offset: Vector) -> &mut Self;         // :817（f32 像素，不是 26.6）
pub fn Render::render(&self, scaler: &mut Scaler, glyph_id: GlyphId) -> Option<Image>;   // :985
pub struct Image { pub source: Source, pub content: Content, pub placement: Placement, pub data: Vec<u8> }   // image.rs:28
pub enum Source { Outline, ColorOutline(u16), Bitmap(StrikeWith), ColorBitmap(StrikeWith) }   // :267
pub fn FontRef::from_index(data: &[u8], index: usize) -> Option<Self>;  // font.rs:172（**usize**，注意与 skrifa 的 u32 不同）
pub fn Charmap::map(&self, codepoint: impl Into<u32>) -> GlyphId;      // charmap.rs:55（**0 = 缺失**，不是 Option）
// zeno（swash 再导出 `swash::zeno`）
pub struct Placement { pub left: i32, pub top: i32, pub width: u32, pub height: u32 }   // zeno geometry.rs:459
pub enum Format { Alpha, Subpixel, CustomSubpixel([f32; 3]) }          // zeno mask.rs:15（Alpha = 1 B/px，stride = width）

// ── etagere 0.3.0 ──（src/allocator.rs、src/lib.rs）
pub fn AtlasAllocator::new(size: Size) -> Self;                        // allocator.rs:114（Size = euclid::Size2D<i32>，lib.rs:98）
pub fn with_options(size: Size, options: &AllocatorOptions) -> Self;   // :86（alignment 默认 (1,1)，:104-125）
pub fn allocate(&mut self, size: Size) -> Option<Allocation>;          // :188（None = 本页放不下，**永不扩容**）
pub fn deallocate(&mut self, id: AllocId);                            // :394（**陈旧 id 直接 panic**，:398-399）
pub fn clear(&mut self);                                              // :118
pub fn size(&self) -> Size;   pub fn allocated_space(&self) -> i32;   pub fn free_space(&self) -> i32;  pub fn is_empty(&self) -> bool;  // :179/:530/:535/:514
pub struct Allocation { pub id: AllocId, pub rectangle: Rectangle }    // lib.rs:137（Rectangle = Box2D<i32>）
pub use euclid::{point2, size2};                                      // lib.rs:95（只有这两个）
// 构造约束：宽高 > 0 且 ≤ u16::MAX，area 必须装进 i32（:123-133）；寻址内部是 u16
```

### 2.3 五个必须记住的语义事实（probe 实测）

① **`scale` 乘进所有坐标**；`quantize=true` 只对齐块轴。

```text
shape("Hi", 16 px)  scale=1.0 quantize=true  → width=15.109 first_baseline=14.000 glyph_x=[0.0, 11.5546875]
                    scale=1.0 quantize=false → width=15.109 first_baseline=14.746 glyph_x=[0.0, 11.5546875]
                    scale=2.0 quantize=true  → width=30.219 first_baseline=29.000 glyph_x=[0.0, 23.109375]
                    scale=1.25 quantize=true → width=18.887 first_baseline=18.000 glyph_x=[0.0, 14.443359]
                    scale=1.25 quantize=false→ width=18.887 first_baseline=18.433 glyph_x=[0.0, 14.443359]
```
⇒ `width` 按 `scale` 线性放大（16 px 字体在 scale=2.0 下是 32 物理 px）；`quantize` **只**把 baseline 取整，
inline x 仍是分数。**结论**：`scale` 取窗口 DPI，`ShapedLine` 的所有坐标 = parley 坐标 `/ scale` 转回逻辑 px；
x 的整像素化由光栅阶段按**物理像素**落点（§8.3）。

② **宽度变化只重排、不重建**：同一 `Layout` 上 `break_all_lines(120)`→2 行、`break_all_lines(60)`→5 行、
`break_all_lines(None)`→1 行；`content_widths = (min 43.578, max 259.625)`，与 `break(1.0).width()` / `break(None).width()`
逐一相等 ⇒ **MinContent/MaxContent 不必断行**，直接读 `calculate_content_widths()`。

③ **空文本不是「零」**：`text=""` → `lines()==1`、`width()==4.4453`、`full_width()==4.4453`、`height()==18.0`、
`first_baseline()==Some(14.0)`、`items()==0`。`" "` 同样 width 4.4453（真实空格）；`"x"` width 8.0。
⇒ 空文本必须由 measure 特例化（§7.5），否则空 label 会占 4.4 px 宽。

④ **etagere 的三个硬事实**：`allocate(8,11)` 在 256² 页上返回 `8×16`（高度向上取整到 16 的 shelf 档）；
同一页最多 399 个 `(10,14)` 字形块（`allocated_space==65536`）；**陈旧 `AllocId` 的 `deallocate` panic**
（`assertion failed: allocated`）⇒ 引擎必须自己记录 id 集合做幂等释放（P02 §10.4 已冻结该策略）。
`alloc(300,300)` 在 256² 页 → `None`（超页单块直接失败，对应 `AtlasExhausted`）。

⑤ **每个 run 一个字体**（同一行内可以横跨 4 个字体文件）：

```text
text = "A中🌍日한" (16 px, scale=1.0, quantize=true) → width=100.64 height=22.00 first_baseline=18.0
  run offset=0.00    blob=1045720B  ttc=0  advance=10.67  glyph id=36   x=0.00    adv=10.67  → placement(l=-1,t=12,w=12,h=12) nonzero=65
  run offset=10.67   blob=19704352B ttc=1  advance=16.00  glyph id=1048 x=10.67   adv=16.00  → placement(l=1,t=12,w=14,h=14)  nonzero=88
  inline box id=3 x=26.67 y=0.00 w=20.00 h=18.00
  run offset=46.67   blob=12934920B ttc=0  advance=21.97  glyph id=729  x=46.67   adv=21.97  → placement(l=2,t=15,w=18,h=18)  nonzero=213
  run offset=68.64   blob=19704352B ttc=1  advance=16.00  glyph id=1945 x=68.64   adv=16.00  → placement(l=2,t=12,w=12,h=14)  nonzero=104
  run offset=84.64   blob=13459196B ttc=0  advance=16.00  glyph id=2846 x=84.64   adv=16.00  → placement(l=1,t=13,w=15,h=14)  nonzero=89
glyphs rasterized=5 tofu(gid 0)=0 empty_alpha=0
```
⇒ ① `ShapedGlyphRun` 必须记录「哪个字体（blob+index）」否则 P06 无法解释 tile；但 tile 在 P05 已产出，
公开面只需要 tile（§5.4）。② tofu 判定 = **shape 后出现 `glyph.id == 0`**（不是查 `fallback_families` 是否为空 ——
probe 里 `Xsux`/`Deva` 的 fallback 家族数都是 1，空的 fallback 列表**测不出** tofu，§6.4）。

### 2.4 probe 记录（四段，全部在仓库外，可复现）

| probe | 位置 | 命令 | 结论 |
|---|---|---|---|
| ① 形状/断行/对齐/每-run 字体/选区 | `%TEMP%\kg-p05-probe` | `cargo run --offline --release` | §2.3①②③⑤、`Selection::geometry` 返回 `Vec<(BoundingBox,usize)>`（`[0,6)` 选中 → 1 个 rect `(0,0)-(43.56,18)`） |
| ② 字体枚举/回退/光栅/图集 | `%TEMP%\kg-p05-font-probe` | `cargo run --offline` | §2.2 行号复核；`Collection::new` 2.419 ms、111 家族 31.8 µs；`family_id` 命中：`Microsoft YaHei UI`/`Microsoft YaHei`/`SimSun`/`Segoe UI`/`Microsoft JhengHei UI`，**未装** `PingFang SC`/`Noto Sans CJK SC`/`Source Han Sans SC`；`j` 的 `placement.left == -2`、`É` 的 `top == 15`；`data.len() == w*h` 恒成立 |
| ③ 尺寸预算 | `%TEMP%\kg-p05-size-probe`（`kytrix_gui` path 依赖） | `cargo run --offline` | `Node` **1016 B**、`NodeKind` **48 B**、`Canvas` 48 B、`Color` 20 B、`SmolStr` 24 B、候选 `TextStyle` **72 B**、`TextContent` **96 B**、`Option<Box<TextContent>>` **8 B**、`PositionedGlyph` 40 B、`Layout<ColorBrush>` 376 B |
| ④ 文本 pass 成本 | `%TEMP%\kg-p05-probe`（`--release` 计时） | `cargo run --offline --release` | `clone()+break_all_lines+align`：24 字符 **1.68 µs**、3200 字符 **72.9 µs**、16000 字符 **288 µs**（每 20 次均值）；这是 measure 闭包的**唯一**开销（§7.4、V-42） |

probe ②的 `complex-scripts` 对照（同一份代码，仅改 feature）：**不开**时 stderr 每段中文打印
`ICU4X data error: No segmentation model for complex script: Chinese/Japanese`；**开启后消失**，断行按 CJK 词典边界。
证据链：`parley_engine-0.12.0/src/analysis.rs:110-116`（`#[cfg(feature = "complex-scripts")] WordSegmenter::new_dictionary`
vs `new_for_non_complex_scripts`）、`analysis.rs:843-855`（`has_word_dictionary` = `cfg!(feature = "complex-scripts")`）、
`icu_segmenter-2.4.0/src/complex/mod.rs:183`（`DataError::custom("No segmentation model for complex script")`）。

---

## 3. 文件树与模块形态

```text
crates/kytrix_gui/src/
├── lib.rs                     # + pub mod text;  + mod render;                （§13.5）
├── text.rs                    # 公共模块根：契约类型直接定义在这里（P01 §4.1「公共模块根」模板）
├── text/
│   ├── context.rs             # TextContext（pub(crate)）：字体库/布局 scratch/字体栈/诊断
│   ├── shape.rs               # shape()：RangedBuilder 唯一调用点；ShapedText（pub(crate)）
│   ├── raster.rs              # GlyphRasterizer：skrifa 度量 + swash alpha
│   ├── atlas.rs               # GlyphAtlas：key → AtlasTile（消费 render::atlas）
│   ├── cache.rs               # ShapeCache：LRU + 预算
│   └── inline_flow.rs         # InlineBoxSpec → InlineBox；selection_rects
├── render.rs                  # crate 私有模块根：pub(crate) mod atlas; + pub(crate) use atlas::{…};
└── render/
    └── atlas.rs               # AtlasAllocator / AtlasSet / DirtyRows / Allocation
```

**模块根形态判据**（P01 §4.1 两类模板，DEVIATIONS M-06/M-07）：
- `text.rs` 是**公共模块根**：`pub mod context;` 等子模块**不出现**在公共面（子模块全部 `pub(crate) mod`），
  契约类型（`TextStyle` 等 10 个）**定义在 `text.rs` 里**。这与 `paint.rs`/`node.rs` 的「子模块 `pub mod`」不同，
  理由是 P01 §4.5 的路径表把文本类型钉在 `text::*`（模块根），子模块是实现（`context`/`shape`/…），
  因此子模块必须 `pub(crate)`，否则 `kytrix_gui::text::shape::ShapedText` 会成为事实公开面。
- `render.rs` 是**crate 私有模块根**：`pub(crate) mod atlas;` + `pub(crate) use atlas::{…}`，**不定义任何类型**
  （P01 §4.1 的模板逐字照抄）。
- 无 `mod.rs`、无 `#[path]`、无 `pub use`、无 `#[allow]`/`#[expect]`/`unsafe`（门禁 §16 逐条 grep）。
- 每个类型恰好**一个** `impl` 块（`multiple_inherent_impl = "forbid"`）。

---

## 4. 公共 / 私有边界

### 4.1 公开路径表增量（P01 §4.5 / P02 §4.2 / P04 §4.1 的续）

| 类型 | 公开路径 | 定义文件 | 消费 Phase |
|---|---|---|---|
| `TextStyle` | `kytrix_gui::text::TextStyle` | `src/text.rs` | P08 `base_ui` 文本组件 / P15 Markdown |
| `FontStack` | `kytrix_gui::text::FontStack` | `src/text.rs` | P08（主题/语言切换）/ P12（系统字体变化） |
| `InlineBoxSpec` | `kytrix_gui::text::InlineBoxSpec` | `src/text.rs` | P15（Markdown 行内图片/`kbd`） |
| `ShapedLine` | `kytrix_gui::text::ShapedLine` | `src/text.rs` | P06（`GlyphRun` 编码）/ P08 L1 断言 / P14（a11y 文本范围） |
| `ShapedTextLine` | `kytrix_gui::text::ShapedTextLine` | `src/text.rs` | P06 / P10（行几何） |
| `ShapedTextItem` | `kytrix_gui::text::ShapedTextItem` | `src/text.rs` | P06（遍历项） |
| `ShapedGlyphRun` | `kytrix_gui::text::ShapedGlyphRun` | `src/text.rs` | P06 / P15 |
| `ShapedInlineBox` | `kytrix_gui::text::ShapedInlineBox` | `src/text.rs` | P06 / P15 |
| `TextContent` | `kytrix_gui::text::TextContent` | `src/text.rs` | `NodeKind::Text` 的载荷（P08/P15 构造） |
| `TextPassReport` | `kytrix_gui::text::TextPassReport` | `src/text.rs` | P06（帧记录）/ P08（测试） |
| `TextDiagnostic` | `kytrix_gui::text::TextDiagnostic` | `src/text.rs` | P08 诊断面板 / 日志 |
| **`TextContext`** | **crate 私有**（`pub(crate)`） | `src/text/context.rs` | 只有 `App`（F-54） |

### 4.2 §1.3 白名单的传递闭包扩充（S-16 格式）

| 新增公开类型 | 由谁带出 | 理由 |
|---|---|---|
| `ShapedTextLine` / `ShapedTextItem` / `ShapedGlyphRun` / `ShapedInlineBox` | `ShapedLine` 的 `lines()` / `line_items()` 返回类型 | P02 §6.3.2 的 `GlyphRun`/`PositionedGlyph` 已进白名单；`ShapedLine` 是它的上游，读面必须自洽 |
| `TextPassReport` | `App::refresh_text` 的返回类型 | 与 P03 `StylePassReport`、P04 `LayoutReport` 同构 |
| `TextDiagnostic` | `App::text_diagnostics()` 的元素类型 | D17「tofu 不静默替换」必须可观测（§6.4） |
| `TextContent` | `NodeKind::Text(TextContent)` 的字段类型 | 节点载荷是应用层构造文本的唯一入口 |

`ShapedLine` 内部复用 P02 的 `PositionedGlyph`（`tile + position + size`）作为字形记录，**不新增字形类型**
（一图元一种真相；`PositionedGlyph` 的 `position` 在 `ShapedLine` 里是**文本盒局部坐标**，P06 加节点原点后
才是绝对坐标 —— 见 S-54）。

### 4.3 每个 `pub(crate)` item 与它的**非 test 消费者**（dead_code 审计表）

> 规则（P02 §3.2 / P03 §4.3 / P04 §4.3）：`pub(crate)` item 必须在**非 test 构建**里被调用，否则 `-D warnings` 失败。

| 文件 | item | 非 test 消费者 |
|---|---|---|
| `render/atlas.rs` | `AtlasAllocator::{new, allocate, clear, mark_written, generation, dirty_span_count}` | `text/atlas.rs` 的 `GlyphAtlas::ensure`（**文本图集**）＋ `App::refresh_text`（`dirty_spans`） |
| `render/atlas.rs` | `AtlasAllocator::{deallocate, take_dirty_rows, for_test}` / `AtlasSet::{image_page_for_test, image_generation_for_test}` | **仅测试构建**（v1 无逐 tile 逐出；P09 落地上传时把 `take_dirty_rows` 恢复为非 test） |
| | `DirtyRows::{mark, spans, is_full, clear}` | `AtlasAllocator::{mark_written, dirty_span_count}`（`is_full` 是 `mark` 的 fast-path） |
| | `Allocation::{id, tile, content, padded}` | `GlyphAtlas::ensure`（`debug_assert` 校验 + `tile` 入缓存 + `id` 入反查表） |
| `text/context.rs` | `TextContext::{new, font_stack, font_stack_version, font_epoch, set_font_stack, reload_system_fonts, shape_cached, diagnostics, push_diagnostics}` | `App::{set_font_stack, reload_system_fonts, refresh_text}` |
| `text/shape.rs` | `ShapedText::{shape, measure, rasterize, text, style, layout, scale, font_stack_version, approx_bytes}` / `family_list` | `TextContext::{shape_cached, set_font_stack}`、`measure` 闭包、`App::refresh_text`、`NodeTree::text_selection_rects` |
| `text/raster.rs` | `GlyphRasterizer::{new, rasterize, font_epoch, set_font_epoch}` / `FontKey::of` | `App::refresh_text`（`set_font_epoch`）、`GlyphAtlas::ensure`（`rasterize`/`font_epoch`） |
| `text/atlas.rs` | `GlyphAtlas::{ensure, clear, take_stats}` | `App::{refresh_text, set_font_stack}`、`ShapedText::rasterize` |
| `text/cache.rs` | `ShapeCache::{get, insert, clear}` | `TextContext::{shape_cached, set_font_stack}` |
| `text/inline_flow.rs` | `to_inline_box` / `vertical_align` / `selection_rects` | `shape`（`to_inline_box`）、`NodeTree::text_selection_rects`（`selection_rects`） |
| `node/tree.rs` | `NodeTree::{text_content, set_text_content, take_bind_text, put_bind_text, set_text_shaped, set_text_line, text_line, text_measured_size, text_slot, invalidate_text_layout}` | `App::refresh_styles` / `App::refresh_text`（`debug_assert` 读 `text_line`） |
| `node/tree.rs` | `Node.text_bind` / `Node.text_measured_size` | `App::refresh_styles`（`take_bind_text`/`put_bind_text`）、`set_text_shaped`（写 size） |

> **只读统计量一律 `#[cfg(test)]`**：`ShapeCache::{len, bytes}`、`GlyphAtlas::len`、
> `GlyphRasterizer::cached_fonts`、`TextContext::{cached_shapes, cached_bytes, family_count}`、
> `AtlasAllocator::{deallocate, take_dirty_rows, for_test}` 只在用例里读（P02/P03 的 `for_test` 先例），
> **不**进入非 test 构建 ⇒ 不参与 dead_code 审计。
> 非 test 的统计出口只有 `GlyphAtlas::take_stats()`（喂 `TextPassReport`）。

**故意保持 `pub` 的项**（无 in-crate 非 test 调用者，但属公开面）：`App::{refresh_text, set_font_stack,
font_stack, font_stack_version, reload_system_fonts, text_diagnostics, text_selection_rects}`、
`NodeBuilder::{text, bind_text}`、`NodeTree::text_selection_rects`。这与 P03 的 `refresh_styles` /
P04 的 `invalidate_measure` 同构（**已落地的先例**：`pub` + P06 是首个 in-crate 调用者）。

### 4.4 公开类型必须有真实消费方（AGENTS §1.2 / S-23）

| 类型 | P05 内的读者 | 首个非 P05 读者 |
|---|---|---|
| `TextStyle` | `shape`（按字段 push `StyleProperty`）、`ShapeKey` | P08 主题 |
| `FontStack` | `TextContext::family_list` | P08/P12 |
| `InlineBoxSpec` | `inline_flow::to_inline_box` | P15 |
| `ShapedLine` 及 4 个子类型 | `App::refresh_text` 产出；`text/atlas.rs` 消费 `glyphs` | P06 |
| `TextContent` | `App::refresh_text`、`NodeBuilder::text` | P08 |
| `TextPassReport` | `App::refresh_text` 构造 | P06 帧记录 |
| `TextDiagnostic` | `TextContext` 产 `tofu` 诊断 | P08 诊断面板 |

---

## 5. `src/text.rs` —— 公共契约（逐字定义）

> 全部 `missing_docs` 友好（P02 S-22 的债务不再新增）；`pub` 字段的 rustdoc 写明单位与坐标空间。
> **不 `derive(Copy)`**：`TextStyle` 含 `SmolStr`；`ShapedLine` 含 `Vec`。

### 5.1 样式与输入

```rust
// src/text.rs
use core::ops::Range;

use smol_str::SmolStr;

use crate::math::{Point, Rect, Size};
use crate::paint::color::Color;
use crate::paint::primitive::PositionedGlyph;

/// 文本水平对齐（CSS `text-align` 的子集；映射到 parley `Alignment`，§7.1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TextAlign { #[default] Start, End, Left, Center, Right, Justify }

/// 文本倾斜（映射到 parley `FontStyle`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TextSlant { #[default] Normal, Italic, Oblique }

/// 软换行策略（映射到 `TextWrapMode` / `OverflowWrap` / `WordBreak`，§7.1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TextWrap {
    /// 按 Unicode 断行规则在软换行机会处换行（默认）。
    #[default]
    Wrap,
    /// 不换行（`TextWrapMode::NoWrap`）：超宽时按 overflow 处理。
    NoWrap,
    /// 任意字符间可断（`WordBreak::BreakAll` + `OverflowWrap::Anywhere`）：日志/哈希列。
    Anywhere,
}

/// 段落基准方向（映射到 `set_base_direction`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TextDirection { #[default] Auto, Ltr, Rtl }

/// 一段文本的样式（**值语义**，不是句柄）。`Clone` 而非 `Copy`（`SmolStr`）。
#[derive(Clone, Debug, PartialEq)]
pub struct TextStyle {
    /// 家族名覆盖；`None` = 用引擎 `FontStack`（D17 的 CJK 显式栈，§6.2）。
    pub family: Option<SmolStr>,
    /// 字号（**逻辑像素**；物理 ppem = `size * scale` 后取整，§8.3）。
    pub size: f32,
    /// CSS 字重 1..=1000；`400` 常规、`700` 粗体。
    pub weight: u16,
    /// 倾斜。
    pub slant: TextSlant,
    /// 行高倍数（CSS 无单位 `line-height`）；`None` = 字体度量（`LineHeight::NORMAL`）。
    pub line_height: Option<f32>,
    /// 字间距（逻辑像素；负值收缩）。
    pub letter_spacing: f32,
    /// 前景色（未预乘）。`ColorBrush` 在 shape 时构造（§7.1）。
    pub color: Color,
    /// 水平对齐。
    pub alignment: TextAlign,
    /// 软换行策略。
    pub wrap: TextWrap,
    /// 基准方向。
    pub direction: TextDirection,
}

impl Default for TextStyle { /* size: 14.0, weight: 400, letter_spacing: 0.0,
    color: Color::Srgb { r: 0.0, g: 0.0, b: 0.0, alpha: 1.0 },
    family: None, slant: Normal, line_height: None, alignment: Start, wrap: Wrap, direction: Auto */ }

impl TextStyle {
    /// 以字号建默认样式（其余取 `Default`）。
    #[must_use] pub const fn new(size: f32) -> Self;
    /// 链式：字号。
    #[must_use] pub const fn with_size(self, size: f32) -> Self;
    /// 链式：家族名覆盖（`SmolStr`，AGENTS §2.2）。
    #[must_use] pub fn with_family(self, family: impl Into<SmolStr>) -> Self;
    /// 链式：字重（不做范围校验 —— CSS 允许 1..=1000，越界由字体匹配钳制）。
    #[must_use] pub const fn with_weight(self, weight: u16) -> Self;
    /// 链式：颜色。
    #[must_use] pub const fn with_color(self, color: Color) -> Self;
    /// 链式：行高倍数。
    #[must_use] pub const fn with_line_height(self, factor: f32) -> Self;
    /// 链式：对齐。
    #[must_use] pub const fn with_alignment(self, alignment: TextAlign) -> Self;
    /// 链式：换行策略。
    #[must_use] pub const fn with_wrap(self, wrap: TextWrap) -> Self;
    /// 链式：基准方向。
    #[must_use] pub const fn with_direction(self, direction: TextDirection) -> Self;
}

/// 节点载荷：文本 + 样式（`NodeKind::Text` 的内容）。
#[derive(Clone, Debug, PartialEq)]
pub struct TextContent { pub text: SmolStr, pub style: TextStyle }
impl TextContent { #[must_use] pub fn new(text: impl Into<SmolStr>, style: TextStyle) -> Self; }

/// 行内盒说明（`<img>` / `kbd` / 行内图标）：**尺寸由应用给，位置由引擎算**。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InlineBoxSpec {
    /// 应用句柄（映射回子节点 / 资源）；原样出现在 `ShapedInlineBox::id`。
    pub id: u64,
    /// 文本**字节偏移**（必须落在 char boundary，§12.2）。
    pub index: usize,
    /// 盒尺寸（逻辑像素；`scale` 变化时引擎按新 scale 重新换算，§12.3）。
    pub size: Size,
    /// 盒内基线（相对盒顶，逻辑像素）；`None` = 底边对齐文本基线（parley 语义）。
    pub baseline: Option<f32>,
    /// 盒在行内的垂直对齐（映射到 parley `VerticalAlign`）。
    pub align: InlineAlign,
}

/// 行内盒的垂直对齐（parley `VerticalAlign` 的 8 个常量，线性表）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum InlineAlign {
    #[default] Baseline, Sub, Super, TextTop, TextBottom, Middle, Top, Bottom,
}
```

### 5.2 形状输出（只读）

```rust
/// 一段文本的**只读**布局结果（= 一个 parley `Layout` 的快照，含图集矩形）。
///
/// 坐标：**文本盒局部**逻辑像素（原点 = 内容盒左上角）。`positioned` 的 y 已含基线，
/// P06 加节点 `LayoutBox.bounds.origin` 后才是 `Primitive::GlyphRun` 要求的绝对坐标（S-54）。
#[derive(Clone, Debug, PartialEq)]
pub struct ShapedLine {
    /// 源文本（P10 的选区映射以此 `&str` 为字节基准）。
    pub text: SmolStr,
    /// 逐行结果（空文本 = 0 行，§7.5）。
    pub lines: Vec<ShapedTextLine>,
    /// 内容宽（= `max(line.advance)`，不含悬挂空白）。
    pub width: f32,
    /// 内容高（= `Σ line.line_height`）。
    pub height: f32,
    /// 首行基线 y（文本盒局部）；空文本 = `None`。
    pub first_baseline: Option<f32>,
    /// 生成时的图集代际：`AtlasTile::is_stale(generation)` 为真 ⇒ 本快照的字形矩形已失效。
    pub atlas_generation: u32,
    /// 生成时的字体栈版本（诊断 / 断言）。
    pub font_stack_version: u32,
    /// 生成时的设备 scale（诊断 / 断言：`scale * glyph.size` 必须是整数）。
    pub scale: f32,
}

/// 一行。
#[derive(Clone, Debug, PartialEq)]
pub struct ShapedTextLine {
    /// 该行覆盖的源文本字节区间。
    pub text_range: Range<usize>,
    /// 基线 y（文本盒局部）。
    pub baseline: f32,
    /// 行高（含 leading）。
    pub line_height: f32,
    /// 行内推进（含悬挂空白之前的全部 advance）。
    pub advance: f32,
    /// 行的项：字形 run 与行内盒**按绘制序**交替（顺序即语义，P06 顺序发射）。
    pub items: Vec<ShapedTextItem>,
}

/// 行内项。
#[derive(Clone, Debug, PartialEq)]
pub enum ShapedTextItem {
    GlyphRun(ShapedGlyphRun),
    InlineBox(ShapedInlineBox),
}

/// 一段同字体同色的字形 run。
#[derive(Clone, Debug, PartialEq)]
pub struct ShapedGlyphRun {
    /// 前景色（未预乘；来自 `TextStyle.color`）。
    pub color: Color,
    /// 该 run 覆盖的源文本字节区间（选区 / a11y 用）。
    pub text_range: Range<usize>,
    /// 已定位（含跑偏移与基线）且**已装箱**的字形；`position`/`size` 是文本盒局部逻辑像素。
    pub glyphs: Vec<PositionedGlyph>,
}

/// 一个行内盒的落点。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShapedInlineBox {
    /// `InlineBoxSpec::id`。
    pub id: u64,
    /// 文本盒局部矩形（`baseline` 为盒内基线，相对 `rect` 顶边）。
    pub rect: Rect,
    /// 盒内基线；`None` = 底边对齐（parley 语义）。
    pub baseline: Option<f32>,
}

/// 一次文本 pass 的统计（P06 帧记录 / L1 断言）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextPassReport {
    /// 本次真正跑 `RangedBuilder::build` 的次数（形状缓存 miss）。
    pub shaped: usize,
    /// 命中形状缓存、复用 `Rc<ShapedText>` 的次数。
    pub reused: usize,
    /// 真正光栅化的字形数（含重排后重光栅）。
    pub rasterized: usize,
    /// 命中字形 `tile` 缓存的字形数。
    pub tiles_reused: usize,
    /// 出现 `glyph.id == 0`（.notdef）的字形数（> 0 即 tofu，见 `TextDiagnostic`）。
    pub tofu: usize,
    /// 因图集满而走「清空 + 重建」的次数。
    pub atlas_rebuilds: usize,
    /// 本次累积的脏行段数（`AtlasAllocator::dirty_span_count` 的读数；真正的上传读者是 P07）。
    pub dirty_spans: usize,
}

/// 一条文本诊断（D17：**不静默替换**，缺失字形/字体必须可观测）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextDiagnostic {
    /// 文本里出现了 `font` 也覆盖不到的字形（`glyph.id == 0`）。
    MissingGlyph { font: SmolStr, glyph: u32, text_range: Range<usize> },
    /// 字体栈里**一个候选都没命中**，已退到 generic `sans-serif`。
    FontStackMiss { tried: SmolStr },
    /// 行内盒的 `index` 不在 char boundary（已按最近的合法边界插入）。
    InlineBoxOffBoundary { id: u64, index: usize },
    /// 行内盒比整行可用宽度还宽（已降级为独立行，§12.3）。
    InlineBoxOverflow { id: u64, width: f32, available: f32 },
}
```

`TextDiagnostic` 是**值**（可 `Eq`、可断言），由 `App::text_diagnostics()` 以 `&[TextDiagnostic]` 暴露；
`TextPassReport::tofu` 是它的计数影子（帧记录只需要数）。

---

## 6. `src/text/context.rs` —— `TextContext`（crate 私有）与 D17

### 6.1 所有权裁决：`App` 持有，`Window` **不**持有

| 方案 | 字体库/字体数据 | 形状缓存 | measure 可达性 | 结论 |
|---|---|---|---|---|
| **`App` 持有（采纳）** | 进程一份：`Collection::new` 实测 **2.4 ms / 111 家族**；`SourceCache` 的 `Blob`（msyh.ttc 19 MB、simsun 18 MB）只读一次 | 跨窗口共享（同一段文本在两个窗口只 shape 一次） | measure 闭包捕获 `Rc<ShapedText>`（§7.4），**不需要**回指 `App` | ✅ |
| `Window` 持有 | 每窗口一份 ⇒ N 个窗口 N 份 19 MB 字体 blob（`SourceCache` 是 per-`FontContext` 的） | 每窗口一份 | 直接 `&mut`，最省事 | ❌ 内存翻倍 |
| `NodeTree` 持有 | 同上；且 `NodeTree::new()` 被 **271 个既有用例**大量构造 ⇒ 每个测试都付字体枚举成本 | 同左 | 可直接 `&mut` | ❌ 测试成本 + 语义错位（树不该是字体库） |

因此：`App` 有 `text: TextContext`、`atlas: AtlasSet`、`raster: GlyphRasterizer` 三个字段（全部 `pub(crate)`，
`field_scoped_visibility_modifiers` 禁止字段级可见性 ⇒ 字段私有 + `pub(crate)` 方法，§4.3）；
`Window` 只新增 `scale_epoch: u32`（§13.4）。

**`TextContext` 不公开**：它必须持有 `parley::{FontContext, LayoutContext}`，公开即把 `parley` 拉进公开面
（F-35 的同类事件，且比 `taffy::Style` 更糟：`FontContext` 的 `Collection` 需要 `&mut`，公开字段等于把
「谁能 shape」交给应用层）。应用层的等价入口是 `App` 的 6 个方法（F-54）。

### 6.2 `FontStack`：CJK 显式栈 + 无 `#[cfg]` 探测

```rust
// src/text.rs
/// 字体栈（D17）：有序候选名 + 版本号。**跨平台同一个列表**，缺失家族由 fontique 静默跳过。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontStack {
    families: SmallVec<[SmolStr; 8]>,
    version: u32,
}

impl FontStack {
    /// 平台默认栈：先 Latin/UI，再 CJK（Han 覆盖不到时 parley 会继续往后找），最后 generic 兜底。
    ///
    /// 实测：Windows 上 `Microsoft YaHei UI`/`Microsoft YaHei`/`SimSun` 命中，
    /// `PingFang SC`/`Noto Sans CJK SC`/`Source Han Sans SC` 不存在 → 由 `Query::set_families`
    /// 的 `family_id(name)?` 语义**跳过**（query.rs:66-70），不需要 `#[cfg(target_os)]`。
    #[must_use] pub fn system_ui() -> Self;   // 见下表
    /// 自定义栈（按序）；`version` 从 1 起。
    #[must_use] pub fn new(families: impl IntoIterator<Item = impl Into<SmolStr>>) -> Self;
    /// 候选名（按序）。
    pub fn families(&self) -> impl ExactSizeIterator<Item = &str> + Clone + '_;
    /// 当前版本（每次内容变更 +1；形状缓存键的一部分）。
    #[must_use] pub const fn version(&self) -> u32;
    /// 替换候选集：返回**新**版本号（内容相同则不变）。
    pub fn set_families(&mut self, families: impl IntoIterator<Item = impl Into<SmolStr>>) -> u32;
}
```

`FontStack::system_ui()` 的逐字候选表（顺序即优先级；最后一项是 generic）：

| # | 家族 | 平台 | 说明 |
|---|---|---|---|
| 1 | `Segoe UI Variable Text` | Win 11 | 现代 UI 首选 |
| 2 | `Segoe UI` | Win | 命中（probe） |
| 3 | `SF Pro Text` | macOS | 缺失则跳过 |
| 4 | `Helvetica Neue` | macOS | 缺失则跳过 |
| 5 | `Noto Sans` | Linux | 缺失则跳过 |
| 6 | `DejaVu Sans` | Linux | 缺失则跳过 |
| 7 | `Microsoft YaHei UI` | Win | **实测命中**（msyh.ttc#1，19.7 MB） |
| 8 | `Microsoft YaHei` | Win | 实测命中（msyh.ttc#0） |
| 9 | `PingFang SC` | macOS | 缺失则跳过 |
| 10 | `Noto Sans CJK SC` | Linux | 缺失则跳过 |
| 11 | `Source Han Sans SC` | Linux | 缺失则跳过 |
| 12 | `Noto Sans SC` | 跨平台 | 缺失则跳过 |
| 13 | `WenQuanYi Micro Hei` | Linux | 缺失则跳过 |
| 14 | `GenericFamily::SansSerif` | 全平台 | `FontFamilyName::Generic`，fontique 的 generic 映射兜底 |

**为什么 Latin 在前、CJK 在后也正确**：parley/fontique 的字体列表是「按家族尝试，缺字形进下一个家族」，
不是「按码点选家族」——probe ⑤ 里 `A` 用了 Segoe UI、`中`/`日` 用了 msyh、`🌍` 用了 Segoe UI Emoji、`한`
用了 Malgun Gothic，全部发生在**同一次 shape** 里；列表里没有的脚本继续走 fontique 的脚本回退（`set_fallbacks`）。

**为什么不需要 `Script::Hani` 显式回退**：`Query::set_fallbacks` 内部**总是追加** Hani 兜底家族列表
（`query.rs:105-112`，源码注释 `// HACK: always add a Han font ...`），parley 自己已经做了这一步；
我们在 `FontStack` 里显式列 CJK 家族是**更早、更可控**的一层（D17 的要求），两者并存不冲突。

`FontStack` → parley 的载体（`text/context.rs`，crate 私有）：

```rust
pub(crate) fn family_list(&mut self) -> parley::FontFamily<'static> {
    // FontFamily::List(Cow::Owned(vec![FontFamilyName::Named(..)…, FontFamilyName::Generic(SansSerif)]))
}
```
`family_list` 在 `set_font_stack` 时失效重算，`shape` 只 `clone()`（`Cow::Owned` 的 `Vec` 克隆 = 一次小分配，
每条文本一次，可接受；V-43 记「若 shape 频率高再改成 `&FontFamily` 借用」）。

### 6.3 `font_stack_version` 与失效

| 事件 | 动作 | 依据 |
|---|---|---|
| `App::set_font_stack(stack)` | `font_stack.set_families(..)` → 版本 +1；`font_epoch += 1`；**清空形状缓存 + 清字形图集 + 图集 `clear()`（generation +1）**；所有**文本节点**标 `LAYOUT | PAINT` + `invalidate_measure` | §4.6「语言/字体切换 → `font_stack_version += 1` → 所有文本节点 LAYOUT + PAINT」；字形 tile 与字体绑定（B2） |
| `App::reload_system_fonts()` | `collection.load_system_fonts()`（幂等）+ 家族 id 缓存清空 + 版本 +1 + `font_epoch += 1` + 同上失效 | D17「系统字体安装/卸载」；B2 |
| `Window::set_scale`（DPI） | **不动** `font_stack_version`；`scale_epoch += 1`；文本 pass 发现 `scale_epoch` 变化 ⇒ 图集 `clear_all()`（generation +1）+ 形状缓存**不失效**（键含 scale，新 scale 自然 miss） | §4.6 的 DPI 行未提版本；P02 §10.6「DPI 变化 → clear」 |

**「所有文本节点标 LAYOUT + PAINT」的实现**：`App::invalidate_all_text(window)` 遍历 `tree.order()`，
对 `NodeKind::Text` 调 `tree.invalidate_measure(node)` **再** `tree.mark_paint(node)`（P04 §9 的
`invalidate_measure` 只标 `LAYOUT`，不保证 `PAINT` —— S-47 的语义缺口，见 S-56）。

### 6.4 tofu 诊断（D17：不静默替换）

- **判定**：shape 完成后遍历 `positioned_glyphs()`，`id == 0` ⇒ 该字形是 `.notdef`。probe ⑤ 实测
  `tofu(gid 0)=0`（四国文字都有字体）；`id == 0` 是唯一可靠信号（字体覆盖表要按 `QueryFont::charmap()`
  逐字查，而 parley 内部已经做过这件事）。
- **记录**：`TextDiagnostic::MissingGlyph { font, glyph, text_range }`，`font` 取该 run 的
  `Run::font()`（blob 长度 + index 编成 `SmolStr`，形如 `"seguiemj.ttf#0"`），`text_range` 取 run 的
  `text_range()`。**不静默换字体**（D17）：引擎不因为缺字形去改 `FontStack`。
- **暴露**：`App::text_diagnostics() -> &[TextDiagnostic]`（环形缓冲上限 64；`TextDiagnostic` 可 `Eq` 以便测试）。
- **为什么不用 `fallback_families(...).next().is_none()`**：probe ② 实测 `Hani`/`Xsux`/`Deva` 三个脚本的
  fallback 家族数都是 1（Windows 的 `system.fallback` 永远给一个），该判据**恒假**（§2.3⑤）。

---

## 7. `src/text/shape.rs` —— 形状、断行、缓存

### 7.1 `TextStyle` → `StyleProperty` 的逐字段映射（`RangedBuilder` 唯一调用点）

```rust
pub(crate) fn shape(
    fcx: &mut parley::FontContext,
    lcx: &mut parley::LayoutContext<ColorBrush>,
    family: &parley::FontFamily<'static>,
    content: &TextContent,
    inline_boxes: &[InlineBoxSpec],
    scale: f32,
) -> parley::Layout<ColorBrush> {
    let s = &content.style;
    // 5 参：text / scale / quantize=true（§2.3①：只为 baseline 取整）
    let mut b = lcx.ranged_builder(fcx, &content.text, scale, true);
    b.set_base_direction(match s.direction {
        TextDirection::Auto => parley::BaseDirection::Auto,
        TextDirection::Ltr  => parley::BaseDirection::Ltr,
        TextDirection::Rtl  => parley::BaseDirection::Rtl,
    });
    b.push_default(StyleProperty::FontFamily(family.clone()));
    b.push_default(StyleProperty::FontSize(s.size));
    b.push_default(StyleProperty::FontWeight(FontWeight::new(f32::from(s.weight))));
    b.push_default(StyleProperty::FontStyle(match s.slant {
        TextSlant::Normal => parley::FontStyle::Normal,
        TextSlant::Italic => parley::FontStyle::Italic,
        TextSlant::Oblique => parley::FontStyle::Oblique(None),
    }));
    b.push_default(StyleProperty::LetterSpacing(s.letter_spacing));
    b.push_default(StyleProperty::LineHeight(match s.line_height {
        None => parley::LineHeight::NORMAL,                    // MetricsRelative(1.0)
        Some(f) => parley::LineHeight::FontSizeRelative(f),
    }));
    b.push_default(StyleProperty::Brush(ColorBrush::new(s.color)));
    b.push_default(StyleProperty::WhiteSpaceCollapse(parley::WhiteSpaceCollapse::Preserve));
    match s.wrap {
        TextWrap::Wrap => {}
        TextWrap::NoWrap => b.push_default(StyleProperty::TextWrapMode(parley::TextWrapMode::NoWrap)),
        TextWrap::Anywhere => {
            b.push_default(StyleProperty::WordBreak(parley::WordBreak::BreakAll));
            b.push_default(StyleProperty::OverflowWrap(parley::OverflowWrap::Anywhere));
        }
    }
    for spec in inline_boxes {
        b.push_inline_box(inline_flow::to_inline_box(spec));
    }
    b.build(&content.text)
}
```

| 决策 | 结论 | 理由 |
|---|---|---|
| 为什么 `WhiteSpaceCollapse::Preserve` | 必须 | `RangedBuilder` **不折叠空白**（builder.rs:40-42 的 doc），默认 `Preserve`（style/mod.rs:34-45）；引擎不做 HTML 式折叠，折行语义交给 `TextWrap` |
| 为什么 `quantize = true` | 必须 | probe ①：`quantize=false` 时 baseline = 18.433（1.25 scale），`true` 时 18.0；v1 要整像素基线 |
| 为什么不用 `parley::TextStyle` + `TreeBuilder` | 不选 | `TextStyle` 是「整段一次性样式」，`TreeBuilder` 还要自管文本缓冲与 style span 栈；P05 的样式是单段平铺，`RangedBuilder` 的 26 变体足够且与 F-03 的裁决一致（A.2 也这么写） |
| 为什么 `LetterSpacing` 无条件 push | 简化 | 0.0 与默认等价；避免两条分支 |
| 为什么不 push `Underline`/`Strikethrough` | 本阶段无读者 | 链接下划线属 Markdown 管线（§12.4）；机制已核验（`StyleProperty::Underline(bool)`），落地时不改本函数签名（`TextStyle` 加字段即可） |

### 7.2 缓存键（拆两级，F-53）

```rust
// text/cache.rs
/// 形状缓存键：**不含** atlas_generation（图集换代只影响 tile，不影响字形/位置）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ShapeKey {
    text_hash: u64,          // ahash of &str（含长度）
    text_len: u32,
    style_hash: u64,         // 见下：f32 全部按 to_bits 参与
    scale_bits: u32,         // f32::to_bits（位相等，不做容差）
    font_stack_version: u32,
}
```

`style_hash` 的字段集（顺序固定，写进 rustdoc，改动即缓存失效）：
`family`（**`Option<&str>`，`SmolStr::as_str()`**，N4）、`size.to_bits()`、`weight`、`slant as u8`、
`line_height.map(f32::to_bits)`、`letter_spacing.to_bits()`、`color`（5 个 `f32::to_bits` + variant tag）、
`alignment as u8`、`wrap as u8`、`direction as u8`。用 `ahash::AHasher::default()`（AGENTS §2.3：ahash 优先），
**不引第三方 hasher**。

命中校验：`ShapeKey` 相等**且** `entry.text == content.text && entry.style == content.style`（防哈希碰撞，
一次 `SmolStr` 比较 + 一次 `TextStyle` 比较；比 re-shape 便宜三个数量级）。

| 维度 | 形状缓存（`ShapeCache`） | 字形 tile 缓存（`GlyphAtlas`） | `ShapedLine` 快照 |
|---|---|---|---|
| 键 | `ShapeKey`（文本/样式/scale/栈版本） | `(font_key, glyph_id, ppem)` | 节点（每节点一份） |
| 含 `atlas_generation` | **否** | **是**（隐含：`clear()` 时整表丢弃） | 是（字段，供 `is_stale` 判定） |
| 失效者 | 文本/样式/scale/`font_stack_version` | 图集 `clear()` / 容量逃逸 | 形状或图集任一变化 |

**对主设计 §4.5 的修正（F-53）**：原文缓存键 `text + color + font_size + TextStyle + scale + font_stack_version
+ atlas_generation` 把两级缓存混成一级。若形状也含 `atlas_generation`，一次图集 `clear()`（DPI 变化、容量逃逸）
会让**所有**文本 re-shape（昂贵且语义上没必要：字形 id、advance、baseline 都与图集无关）。修正后：
`atlas_generation` 只进 tile 缓存与 `ShapedLine.atlas_generation`。

### 7.3 断行与对齐（宽度变化只重排）

```rust
/// 在既有 `Layout` 上按新宽度重排（**不重建**：probe ②）。
pub(crate) fn rebreak(layout: &mut parley::Layout<ColorBrush>, style: &TextStyle, width: Option<f32>) {
    layout.break_all_lines(width);
    layout.align(align_of(style.alignment), AlignmentOptions::default());
}

fn align_of(a: TextAlign) -> parley::Alignment {   // 1:1，无 `Justify` 特例
    match a { Start => Alignment::Start, End => Alignment::End, Left => Alignment::Left,
              Center => Alignment::Center, Right => Alignment::Right, Justify => Alignment::Justify }
}
```

| 规则 | 内容 | 证据 |
|---|---|---|
| 可重复调用 | `break_all_lines` 每次重置 `width/height/lines/line_items`，保留 shaped 数据 | `layout/line_break.rs:844-851`；probe ② |
| `align` 不累积 | 每次从 `offset = indent`（LTR）/`-hanging_advance`（RTL）重算 | `layout/alignment.rs:69-147` |
| `align` 必须在 `break_all_lines` 之后 | 否则 `layout.alignment() == None` | `layout/layout.rs:215-225` doc |
| `Justify` 会写进 run/cluster advance | 所以**先 align 再测量/遍历**（P05 的顺序就是 shape → break → align → 读数） | `run.rs:140-142` |
| 右对齐行尾空白 | 由 parley 的 `hanging_advance` + `offset` 处理；引擎**不**手工截断（§12.5） | `LineMetrics.hanging_advance`、`Layout::width` vs `full_width` |

### 7.4 measure：把 `Rc<ShapedText>` 冻结进闭包（不碰 `&mut` 引擎）

```rust
// text/shape.rs —— crate 私有，frozen 的「可重排快照」
pub(crate) struct ShapedText {
    text: SmolStr,
    style: TextStyle,
    scale: f32,
    font_stack_version: u32,
    layout: parley::Layout<ColorBrush>,   // 只读：measure 用 clone 后的副本重排
    widths: parley::ContentWidths,        // min/max 内容宽（probe ②：与 break 结果逐一相等）
}

impl ShapedText {
    /// 按 taffy 语义测量（纯函数：只读 self，clone 后重排）。
    pub(crate) fn measure(&self, known: taffy::Size<Option<f32>>,
                          available: taffy::Size<taffy::AvailableSpace>) -> Size;
}
```

**接线方式（关键裁决，§7.4 的「零 `RefCell`」）**：`App::refresh_text` 拿到 `Rc<ShapedText>` 后调
`NodeTree::set_text_shaped(node, rc)`，后者**重建**该节点的 `measure` 闭包：

```rust
// node/tree.rs（pub(crate)）
/// 冻结形状快照并重建 measure 闭包。
///
/// **无论尺寸是否变化都必须清 `node..root` 的 taffy 缓存**（B1）：taffy 的 cache 键不含
/// 「闭包捕获的状态」，snapshot 变化后旧的 measure 结果必须作废；`size_changed` 只决定
/// **是否额外**置 `LAYOUT`（尺寸没变时只 `PAINT`，仍满足 §4.5 的目标语义）。
pub(crate) fn set_text_shaped(&mut self, node: NodeId, shaped: Rc<ShapedText>,
                              measured: Size, size_changed: bool) -> bool {
    let Some(target) = self.arena.get_mut(node) else { return false };
    let rc = Rc::clone(&shaped);
    target.measure = Some(Box::new(move |known, available| rc.measure(known, available)));
    target.text_measured_size = measured;
    // ① 无条件清缓存（B1 的硬要求）：node..root 的 taffy cache 全部作废
    self.invalidate_caches_upwards(node);
    // ② 脏位：尺寸变了才 LAYOUT；否则只 PAINT
    if size_changed { self.mark_layout(node) } else { self.mark_paint(node) }
    true
}
```

| 为什么是「重建闭包」而不是共享槽 | 结论 | 理由 |
|---|---|---|
| `Rc<RefCell<Option<Rc<ShapedText>>>>` 槽 | 否 | P04 §9.1 的 `MeasureFn` 契约是「纯函数、不得写状态」；引入可变槽等于把唯一保障变成注释 |
| 闭包捕获 `Rc<ShapedText>`，shape 变化时重建 | **采纳** | 闭包捕获的是**不可变**快照：`Fn` 类型级纯函数；重建只在文本/样式/scale/栈版本变化时发生（每次 `Box` 一次分配） |
| 直接在 `compute_leaf` 里按 `NodeKind` 分派 | 否（本阶段） | 要向 `NodeTree` 注入 `TextContext`（字体库），带来 §6.1 的 N 份字体数据问题；且要改 P04 冻结的 `compute_leaf` 逐字实现 |

| 失效纪律（B1，**硬要求**） | 内容 |
|---|---|
| 触发条件 | **任何** shape 快照变化：`text` / `TextStyle` 任一字段 / `scale` / `font_stack_version` |
| 必做 | `invalidate_caches_upwards(node)`（清 `node..root` 的 taffy cache）。**不得**因为「测得尺寸没变」跳过 |
| 尺寸比较的作用 | 只决定**额外**置不置 `LAYOUT`：变了 → `mark_layout`（`LAYOUT|PAINT` + 冒泡）；没变 → `mark_paint`（只重画，不重排） |
| 反例（用例 `text_wrap_change_same_max_width_relayouts`） | `TextWrap::Wrap` → `TextWrap::Anywhere` 的 `widths.max` 相同（advance 不变），但同一 `Definite(40)` 下限的断行行数不同 ⇒ 若用「尺寸比较」当「清缓存」的判据，第二次布局会命中陈旧 taffy cache、高度不更新 |

`Rc` 不是 `Send`/`Sync`，但 `App`/`Window`/`NodeTree` 全部单线程（P01 §1.4），`MeasureFn` 也没有 `Send` 约束
（`layout.rs:39-40`）；`EngineError` 的 `Send + Sync` 要求不受影响（错误里只有 `SmolStr`/`NodeId`/`f32`）。

**成本**（probe ④，`--release`）：`clone + break_all_lines + align` = 24 字符 **1.68 µs**、3200 字符
**72.9 µs**、16000 字符 **288 µs**。taffy 每轮对同一叶子最多调 2–3 次 measure（P04 §9.3 已证 cache 命中后
零调用），因此 v1 可接受；大段落（>8k 字符）记 V-42。

### 7.5 measure 的返回规则（逐条）

```text
measure(known, available):
  0. text.is_empty()                       → Size::ZERO                    // §2.3③ 的坑
  1. available.width == Definite(w)        → break(Some(w)) → (width(), height())
  2. available.width == MinContent         → break(Some(widths.min)) → (widths.min, height())
  3. available.width == MaxContent         → break(None)     → (widths.max, height())
  4. known.width == Some(w)（且 available 不是三态） → 同 1
  5. 其余                                   → break(None)（保守：单行固有宽）
  返回值 = 内容盒尺寸（taffy 会加 padding/border，P04 §9.1）
```

| 规则 | 说明 |
|---|---|
| 为什么 `MinContent` 要 break 一次 | 高度依赖行数，只有 `ContentWidths` 不够；probe ② 证明 `break(1.0).width() == widths.min`，所以按 `widths.min` 重排得到的就是 min-content 的行数 |
| 为什么 `MaxContent` 用 `widths.max` 而不是 `break(None).width()` | 两者相等（probe ②：259.625），但 `None` 分支更便宜（单行）且不会因 `line_max_advance` 边界误断 |
| 为什么宽高都从「重排后的 clone」读 | `Layout::width()` 不含悬挂空白（`layout.rs:104`），`full_width()` 含尾部空格；文本节点的内容宽应当**不含**尾随空格（否则右对齐/居中会多出空白） |
| `InvalidMeasure` 怎么产生 | 本函数不返回错误；异常值（NaN/负）由 P04 的 `compute_leaf` 检测并 `note_invalid_measure`（`layout/measure.rs` 逐字保留） |
| 空文本为什么是 0 而不是「一行高度」 | 空 label 不该占位；需要占位的应用显式给节点 `Style.size.height`。测试 `measure_empty_text_is_zero` 固化 |
| `text_measured_size` 的作用 | `App::refresh_text` 用它判「尺寸变了没有」：变了 → `invalidate_measure`（LAYOUT）；没变 → `mark_paint`（PAINT only，§13.3） |

---

## 8. `src/text/raster.rs` —— 光栅

### 8.1 管线

```text
parley Layout 的 GlyphRun
  → Run::font() = FontData { data: Blob<u8>, index: u32 }      // 每个 run 自己的字体（probe ⑤）
  → skrifa::FontRef::from_index(blob, index)                   // TTC 必须带 index
  → font.metrics(Size::new(ppem), LocationRef::default())      // ascent/descent/leading/underline
  → font.glyph_metrics(Size::new(ppem), ..).advance_width(gid) // 只做**校验**（advance 用 parley 的）
  → swash::FontRef::from_index(blob, index as usize)           // 注意 usize vs u32
  → ScaleContext::builder(font).size(ppem).hint(true).build()  // Scaler 复用（同 font+ppem）
  → Render::new(&[Source::Outline]).format(zeno::Format::Alpha).render(&mut scaler, gid)
  → Image { placement: {left, top, width, height}, data: Vec<u8> }
  → 写进图集：content = (w, h)，quad 物理落点 = (pen_phys + left, baseline_phys - top)
```

```rust
/// 逐字形的 R8 覆盖度（**不含 padding**；padding 由图集分配器加）。
pub(crate) struct RasterizedGlyph<'a> { pub width: u32, pub height: u32, pub data: &'a [u8] }

pub(crate) struct GlyphRasterizer {
    ctx: swash::scale::ScaleContext,                       // P02 建议：每线程一个
    fonts: ahash::HashMap<FontKey, FontEntry>,             // FontData → (swash::FontRef 需要的 bytes 快照)
    pub(crate) stats: RasterStats,
}
```

| 事实/决策 | 内容 | 依据 |
|---|---|---|
| skrifa 不提供光栅 | 只能用轮廓（`OutlineGlyph::draw` → `OutlinePen`）或嵌入 bitmap；**无 raster 模块** | §2.2 |
| 用 swash 而不是自写光栅 | swash 0.2.10 已有 zeno 光栅 + hinting + COLR/CBDT 源；自写要处理 hinting、gamma、overlap | AGENTS §1.4 |
| skrifa 仍然要用 | `Metrics` 的 `ascent/descent/leading/underline` 是**行高与下划线**的唯一来源（parley 的 `Run::font_metrics()` 也是它，但 P05 要对齐同一份数值做断言） | probe ②：`ascent=17.266 descent=-4.016 leading=0 underline=(-1.39, 0.93)` @16 px |
| `FontRef::from_index` 的 index 类型 | skrifa `u32`、swash `usize`：**两处都要传 `FontData.index`**（TTC），否则 msyh/simsun 全是 `.notdef` | probe ②（TTC 上 `FontRef::new` 失败 → 覆盖检查全 false） |
| 为什么 `hint(true)` | v1 整数 ppem（§8.3）；hinting 让 12–16 px 的 CJK/拉丁笔画落在整像素 | `ScalerBuilder::hint`（mod.rs:400） |
| 为什么 sources 只有 `Outline` | 颜色字形（COLR/CBDT）在 R8 字形图集里无法表达；v1 只取单色轮廓（probe ⑤：`🌍` 的 outline 也有 213 个非零像素） | V-41 |

### 8.2 字体身份（`FontKey`）—— **blob 必须被 pin 住**（B2）

```rust
/// 字体身份：blob 的**地址 + 长度** + TTC 索引 + 字体代际。
///
/// 地址只在**我们持有同一个 `Blob`** 时才稳定：`Rc<ShapedText>` 里的 `Layout` 只保证 shape 期间存活，
/// 而 `parley` 每次 `ranged_builder` 都会 `source_cache.prune(128, false)`（`context.rs:109`），
/// **可能释放别的 Blob 并让新 Blob 复用同一地址** —— 仅用 `(ptr, len, index)` 会把两个字体认成同一个（B2）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FontKey { ptr: usize, len: usize, ttc_index: u32, font_epoch: u32 }

/// 字体条目：**持有 blob 的 clone（pin 住 `Arc`）**，因此 `ptr` 在条目存活期间不会失效。
pub(crate) struct FontEntry { blob: parley::FontData }   // Blob<u8> + index，Clone 只是 Arc 计数 +1
```

| 规则 | 内容 |
|---|---|
| pin 语义 | `GlyphRasterizer::fonts: ahash::HashMap<FontKey, FontEntry>` 的**值**持有 `FontData`（`Blob` 是 `Arc`）；条目存在 ⇒ 地址稳定 ⇒ `ptr` 可作键 |
| 代际失效 | `font_epoch` 由 `TextContext` 维护：`set_font_stack` / `reload_system_fonts` 时 +1；`GlyphRasterizer::set_epoch(new)` 清空 `fonts`、`GlyphAtlas::clear()`、`AtlasSet::glyph_mut().clear()`（generation +1）。字号/字形缓存因此不会跨字体代际复用 |
| 为什么还需要 `len` | 防御性：同一地址不同长度的极端情形（不同 `Arc` 分配器复用），代价 8 B |
| 为什么不用 `Blob::id()` | `linebender_resource_handle` 的 `id` 只在 `from_raw_parts` 显式给出时才有意义，注册路径不保证唯一（`blob.rs:110`）；用「pin + 地址 + 代际」不依赖上游语义 |

### 8.3 字号 / scale 取整策略（v1 整数像素落点）

```text
ppem_phys   = round(style.size * scale).clamp(1.0, 512.0)     // 物理像素，整数
pen_phys_x  = round(glyph.x / scale * scale) = round(glyph.x) // parley 坐标已是物理 px（scale 乘过）
baseline_phys = round(glyph.y)                                // parley quantize=true 已取整（§2.3①）
quad_phys   = (pen_phys_x + placement.left, baseline_phys - placement.top)
quad_logical = quad_phys / scale                              // 回逻辑 px（f32，可为分数）
size_logical = (placement.width / scale, placement.height / scale)   // 契约：scale * size 是整数（P02 §6.3.2）
```

| 决策 | 结论 | 理由 |
|---|---|---|
| ppem 取整 | **必须** | `size=13.5, scale=1.25 → 16.875` 直接光栅会得到半像素模糊；取整 + hinting 才有稳定的 13 px 观感（v1 接受 ±0.5 px 字号偏差） |
| 字形落点 | 物理像素整数 | P02 §10.7「1:1 采样必须落在整数物理像素」；probe ⑤ 的 `placement` 本身就是整数 |
| `size_logical` 可为分数 | 是 | P02 的契约是 `scale * size` 为**整数**（= 物理尺寸），逻辑尺寸本来就可以是 `11/1.5 = 7.333` |
| subpixel x（1/3 px） | **不做** | 需要 3 套图集或 3× 页；v1 用整数落点 + 少量字距抖动（V-40） |
| gamma / 对比度 | 不做 | 覆盖率直出（`out = color * texel.r`，P02 §10.3）；gamma 校正属 P10 的渲染器策略 |

### 8.4 padding、空白与失败路径

| 情况 | 处理 |
|---|---|
| 字形有轮廓 | `allocate((w+2, h+2))`（1 纹素 padding 四周清 0），返回内容区 UV（P02 §10.2 的「用边」） |
| 空白字形（空格/U+00A0） | `Render::render` 返回 `None` 或无轮廓 ⇒ **不建 tile**，advance 仍然推进（不产生任何像素，符合空白语义） |
| `data.len() != w*h` | debug 断言失败；release 下丢弃该字形 + `TextDiagnostic`（不 panic） |
| 图集满（第一次） | `clear()` + `generation += 1` + 清 tile 缓存 + **重试一次**（P02 §10.6） |
| 图集满（第二次，单块 > 整页） | `EngineError::AtlasExhausted { kind: AtlasKind::Glyph }`（§9.5） |
| 字体数据坏（`swash::FontRef::from_index` 返回 `None`） | 该 run 的所有字形跳过 + `TextDiagnostic::MissingGlyph`；不静默替换字体 |
| 每个字形只光栅一次 | `GlyphAtlas` 的 `(FontKey, glyph, ppem)` 缓存；probe ⑤ 的 5 个字形各一次 |

---

## 9. `src/render.rs` + `src/render/atlas.rs` —— 图集分配器（P02 §10 冻结规格的落地）

### 9.1 `src/render.rs`（crate 私有模块根，逐字）

```rust
//! 渲染共享层（crate 私有；应用层不可见，I9）。
//!
//! 本文件只做模块声明与 crate 内出口（P01 §4.1「crate 私有模块根」模板）：**不定义任何类型**。
//! 落地范围：`atlas`（P05）；`damage` → P06、`windows`/`macos`/`linux`/`device`/`frame`… → P09+。

pub(crate) mod atlas;

pub(crate) use atlas::{Allocation, AtlasAllocator, AtlasSet, DirtyRows};
```

### 9.2 `AtlasAllocator`（逐字，含 P02 §10.4 的九个实现要点）

```rust
//! 单个 `AtlasKind` 的**单页**分配器（P02 §10.4）。容量逃逸 = `clear()` + `generation += 1`。

pub(crate) struct AtlasAllocator {
    kind: AtlasKind,
    page: (u32, u32),
    allocator: etagere::AtlasAllocator,
    tiles: Vec<Option<TileRecord>>,     // 下标 = 我们自己的槽位号（不是 AllocId 的内部值）
    free: Vec<usize>,                   // 槽位回收
    generation: u32,
    dirty: DirtyRows,
}

struct TileRecord { id: etagere::AllocId, tile: AtlasTile, content: PhysicalRect, padded: PhysicalRect }

#[derive(Clone, Copy, Debug)]
pub(crate) struct Allocation { pub id: etagere::AllocId, pub tile: AtlasTile,
                               pub content: PhysicalRect, pub padded: PhysicalRect }
```

```rust
impl AtlasAllocator {
    pub(crate) fn new(kind: AtlasKind) -> Self;                 // 页尺寸按 AtlasKind（Glyph 2048² / Image 2048²）
    pub(crate) fn kind(&self) -> AtlasKind;
    pub(crate) fn page(&self) -> (u32, u32);
    pub(crate) fn generation(&self) -> u32;
    pub(crate) fn allocate(&mut self, content: (u32, u32)) -> Option<Allocation>;
    /// 幂等释放：`id` 不在 `tiles` 里就**忽略**（etagere 对陈旧 id 会 panic，§2.3④）。
    pub(crate) fn deallocate(&mut self, id: etagere::AllocId);
    pub(crate) fn clear(&mut self);                             // 重建 etagere + tiles/free 清空 + generation += 1
    pub(crate) fn mark_written(&mut self, rect: PhysicalRect);
    #[cfg(test)]  // P05 只累积；P09 的增量上传把 cfg 去掉即可
    pub(crate) fn tile_of(&self, id: etagere::AllocId) -> Option<AtlasTile>;   // #[cfg(test)]：重绑定时取旧 tile
}
```

`allocate` 的逐字实现要点（P02 §10.4 的九步，**全部照抄 + 两条 probe 修正**）：

```text
1. 拒绝 content.0 == 0 || content.1 == 0
2. 拒绝 content.dim + 2 > 65535                              // etagere 的 Size 是 i32，内部寻址 u16（:123-133）
3. 拒绝 content.w + 2 > page.0
4. padded = (content.w + 2, content.h + 2)
5. let a = allocator.allocate(size2(padded.0, padded.1))?     // 返回矩形**可能更大**（probe：请求 8×11 得 8×16）
6. content_origin = (a.rectangle.min.x + 1, a.rectangle.min.y + 1)
7. uv = (x0/W, y0/H, (x0+w)/W, (y0+h)/H)                      // 用边、不用纹素中心（P02 §10.2）
8. 记录 TileRecord { id, tile: AtlasTile { atlas: kind, u0..v1, generation }, content, padded }
9. 返回 Allocation
```
> **修正 ①**：P02 §10.4 第 4 步只写了 `padded = content + 2`；probe 证明 etagere 会把高度**向上取整到 16 的档**
> （8×11 → 8×16），所以 `padded` 必须取 `a.rectangle` 的**实际**尺寸（引擎不假设它等于请求值）。
> **修正 ②**：`deallocate` 必须靠自己的 slot 表判重（etagere `assert!(allocated)` 会 panic，probe 实测）。

### 9.3 `AtlasSet`

```rust
/// 两类图集的集合（P02 §5.6：字形与图片分图集；v1 每类单页）。
pub(crate) struct AtlasSet { glyph: AtlasAllocator, image: AtlasAllocator }
impl AtlasSet {
    pub(crate) fn new() -> Self;
    pub(crate) fn glyph(&self) -> &AtlasAllocator;
    pub(crate) fn glyph_mut(&mut self) -> &mut AtlasAllocator;
    pub(crate) fn image(&mut self) -> &mut AtlasAllocator;      // P15 的首个真实读者（P05 只测）
    pub(crate) fn generation(&self) -> u32;                     // = glyph.generation()（P06 的 stale 判据）
    pub(crate) fn clear_all(&mut self);                         // 两页都 clear（DPI 变化 / 语言切换）
}
```
`glyph` 页 2048×2048 **R8 = 4 MiB**、`image` 页 2048×2048 **BGRA8 = 16 MiB**（P02 §10.3 冻结）；
页尺寸是常量 `GLYPH_PAGE: (u32, u32)` / `IMAGE_PAGE: (u32, u32)`，**不做**可配置（AGENTS §1.2）。

### 9.4 脏行（`DirtyRows`，P02 §10.5 逐字）

```rust
#[derive(Clone, Debug, Default)]
pub(crate) struct DirtyRows { spans: Vec<Range<u32>> }   // 升序、已合并（相邻也算合并）
impl DirtyRows {
    pub(crate) fn mark(&mut self, y0: u32, y1: u32);     // 非法（空/逆序）忽略；>8 段折叠为整页
    pub(crate) fn spans(&self) -> &[Range<u32>];
    pub(crate) fn is_full(&self) -> bool;
    pub(crate) fn clear(&mut self);
}
```
| 决策 | 结论 | 理由 |
|---|---|---|
| 粒度 | 整行（x 全宽） | 三平台上传都是「行跨度拷贝」（P02 §10.5） |
| `spans > 8` | 折叠 `[0, page_h)` | 8 段已意味着大半页在动 |
| P05 的消费者 | `AtlasAllocator::{mark_written, dirty_span_count}`（`App::refresh_text` 在光栅后 `mark_written`，报表读 `dirty_span_count`）| P07 才真正上传；P05 只累积（`take` 由 P07 调，故 P05 内 `take_dirty_rows` 的读者是 **P02 §14.9 的用例 + P07**，见 V-45） |

### 9.5 `generation` 与容量失败（`AtlasExhausted` **本阶段落地**）

| 时机 | `generation += 1` | 理由 |
|---|---|---|
| 单次 `allocate`/`deallocate` | 否 | 已有 tile 的 UV 不变 |
| `clear()`（DPI 变化 / 语言切换 / 字形换代 / 容量逃逸） | **是** | 所有 UV 失效（`AtlasTile::is_stale`） |
| 页尺寸变化 | 不实现 | v1 单页固定尺寸（V-44） |

```text
allocate(content) -> None
  → 第一次：atlas.clear()（generation += 1）→ 清 GlyphAtlas → retry allocate
  → 第二次仍 None：Err(EngineError::AtlasExhausted { kind: AtlasKind::Glyph })
```
P01 §6.2.2 #12 的 `AtlasExhausted { kind: AtlasKind }` 实现 Phase 由 P09 提前到 **P05**（F-51 的同类，
DEVIATIONS 新条目 F-55 记录），原因是它的唯一生产者（分配器）在本阶段落地；P09 只是消费者。

### 9.6 `shaped → tile` 的消费链（为什么 P05 必须同时落地光栅与图集）

```text
App::refresh_text(window)                       ← 公开入口（P06 是首个 in-crate 调用者，§4.3）
  ├─ TextContext::shape(..)            → Rc<ShapedText>（形状缓存）
  ├─ NodeTree::set_text_shaped(node, rc)          → 重建 measure 闭包（§7.4）
  ├─ ShapedText::rasterize(&mut atlas, &mut raster) → ShapedLine（每个字形带 AtlasTile）
  │     ├─ AtlasSet::glyph_mut()                  → render::atlas（非 test 消费 ✅）
  │     └─ GlyphRasterizer::rasterize              → swash
  └─ NodeTree::set_text_line(node, Rc<ShapedLine>) → P06 的输入
```
若把光栅/图集推到 P06（"本阶段不落地"），`render/atlas.rs` 的 11 个 `pub(crate)` item 就只有 test 消费者 ⇒
`dead_code` 直接违反 AGENTS §4 与 P02 §3.2 的硬规则。因此 **P05 落地「shape → raster → tile」的 CPU 全链**，
P06 只做「`ShapedLine` → `Scene` 的实例编码」（S-54）。

---

## 10. `src/text/atlas.rs` —— 字形 key → tile 缓存

```rust
/// 字形图集缓存：`(字体, 字形, 物理 ppem)` → `AtlasTile`。**逐字形只光栅一次**。
pub(crate) struct GlyphAtlas {
    tiles: ahash::HashMap<GlyphKey, AtlasTile>,
    /// 反查：tile 的 AllocId（图集重排时整表丢弃，不需要逐项 deallocate）。
    ids: ahash::HashMap<GlyphKey, etagere::AllocId>,
    stats: GlyphAtlasStats,        // 命中/未命中/重建计数（`TextPassReport` 的来源）
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct GlyphKey { font: FontKey, glyph: u32, ppem: u32 }
```

> `FontKey` 含 `font_epoch`（§8.2 的 B2 修复）：字体栈/系统字体换代时 `GlyphAtlas` 整表清空 +
> `AtlasAllocator::clear()`（generation +1），**字形 tile 不跨代际复用**。

```rust
impl GlyphAtlas {
    pub(crate) fn new() -> Self;
    /// 取 tile；未命中则光栅 + 分配。`Err(AtlasExhausted)` = 清空重建后仍放不下。
    ///
    /// **实现期修正**：`ensure` 不需要调用方传 `GlyphKey`/`ppem`，它由 `font`（+ `raster.font_epoch()`）、
    /// `glyph`、`ppem` 自行构造键；返回 `GlyphTile`（tile + swash 的 `left/top/width/height`），
    /// 因为落点必须用 `top/left` 才算得出（只给 `AtlasTile` 不够）。
    pub(crate) fn ensure(&mut self, raster: &mut GlyphRasterizer, atlas: &mut AtlasAllocator,
                         font: &parley::FontData, glyph: u32, ppem: f32)
        -> Result<Option<GlyphTile>, EngineError>;
    /// 与 `AtlasAllocator::clear()` 的同一事务里调用（两级缓存同步换代）。
    pub(crate) fn clear(&mut self);
    /// 取走统计（累加进 `TextPassReport` 后清零）。
    pub(crate) fn take_stats(&mut self) -> GlyphAtlasStats;
}
```

| 决策 | 结论 | 理由 |
|---|---|---|
| 缓存键含 ppem 而非 scale | 是 | 同一 scale 下 13.5 与 13.6 取整后同 ppem（复用）；不同 DPI 自然分桶 |
| 命中时校验 `tile.is_stale(generation)` | 是 | 防御「图集被外部 clear 而缓存没清」；stale 即视为 miss |
| 容量上限 | 无独立上限（受图集页容量约束） | `etagere` 页即上限；LRU 逐出属 P02 §10.6 的「v1 不做」 |
| 缓存条目上限 | `tiles.len() <= 65536`（字形数上界，防御性） | 页 2048² / 最小 tile 3×3 ⇒ 物理上界约 16 万；断言只做 debug |
| 清空时机 | `AtlasAllocator::clear()` 之后由 `App::refresh_text` 调 `GlyphAtlas::clear()`（同一事务） | 两级缓存必须同步换代，否则 UV 指向已释放区域 |

---

## 11. `src/text/cache.rs` —— 形状缓存与预算

### 11.1 结构（LRU 用「插入序 + 命中提升」，不引第三方 crate）

```rust
/// 形状缓存：`Rc<ShapedText>` 的 LRU（容量 + 字节双上限）。
pub(crate) struct ShapeCache {
    entries: ahash::HashMap<ShapeKey, Entry>,
    /// 插入序（尾 = 最新）；`pop_front` 即最久未用。
    order: std::collections::VecDeque<ShapeKey>,
    bytes: usize,
    capacity: usize,        // 条目上限：256
    byte_budget: usize,     // 字节上限：8 MiB（§9.3 的「容量上限显式」）
}

struct Entry { shaped: Rc<ShapedText>, bytes: usize, text: SmolStr, style: TextStyle }
```

```rust
impl ShapeCache {
    pub(crate) fn new() -> Self;                       // 256 条 / 8 MiB
    pub(crate) fn get(&mut self, key: &ShapeKey) -> Option<Rc<ShapedText>>;   // 命中即提升顺序
    pub(crate) fn insert(&mut self, key: ShapeKey, entry: Rc<ShapedText>, text: SmolStr, style: TextStyle);
    pub(crate) fn clear(&mut self);
    pub(crate) fn len(&self) -> usize;
    pub(crate) fn bytes(&self) -> usize;
}
```

| 决策 | 结论 | 理由 |
|---|---|---|
| 键含 `Rc<ShapedText>` 还是 `ShapedLine` | `ShapedText`（含 parley `Layout`） | `ShapedLine` 带 `AtlasTile`，图集换代就会失效；形状本身不依赖图集（F-53） |
| 逐出会不会饿死正在显示的文本 | 不会 | 节点的 measure 闭包持有 `Rc` 克隆 ⇒ 即使缓存逐出，该文本的布局快照仍存活；逐出只影响后续复用 |
| 字节估算 | `size_of::<ShapedText>() + Σ(每个字形 16 B) + text.len()` | 只用于预算近似（不做精确 allocator 记账） |
| `VecDeque` 提升成本 | O(n)（n ≤ 256） | 命中率优先；如成为热点改侵入式链表（V-43） |
| 为什么不做并发安全 | 单线程（P01 §1.4） | `Rc` 已表达「非 `Send`」 |

### 11.2 缓存与「只读输出」的一致性

`ShapedText` 是**不可变**的：`layout` 字段对外只读，`measure` 用 `clone()` 重排（§7.4）。因此：
- 缓存命中 ⇒ 同一 `Rc` 被多个节点共享（同文本同样式同 scale）；
- `ShapedLine` 是**每节点**的快照（含 tile），不共享（相同文本的两个节点 tile 相同但 `position` 不同 ⇒ 实际上
  `ShapedLine` 也可按 `(ShapeKey, 原点)` 共享，但 v1 不做，V-43）。

---

## 12. `src/text/inline_flow.rs` —— 行内盒与选区映射

### 12.1 `InlineBoxSpec` → `parley::InlineBox`

```rust
pub(crate) fn to_inline_box(spec: &InlineBoxSpec) -> parley::InlineBox {
    parley::InlineBox {
        id: spec.id,
        kind: parley::InlineBoxKind::InFlow,
        index: spec.index,                       // 字节偏移，parley 原样保留（builder.rs:390-395 稳定排序）
        width: spec.size.width,
        height: spec.size.height,
        baseline: spec.baseline,
        vertical_align: match spec.align { /* 8 个常量，1:1 */ },
    }
}
```

| 字段 | 语义（parley 逐字） | P05 的约束 |
|---|---|---|
| `id` | 「用户自定义标识，用来把输出里的盒映射回输入」（inline_box.rs:9-10） | 应用给；引擎原样回传（P06 用它找子节点/资源） |
| `index` | 「文本字节偏移，**不得落在码点内部**」（:14-16） | 构造时 `debug_assert!(text.is_char_boundary(index))`；release 下吸附到最近的合法边界 + `TextDiagnostic::InlineBoxOffBoundary` |
| `width/height` | 逻辑像素 | `scale` 变化时应用无需改：逻辑尺寸稳定，物理光栅只在 P06/P15 |
| `baseline` | 「相对盒顶；`None` ⇒ 底边与文本基线对齐」（:21-24） | 行内图片给 `Some(h * 0.8)` 之类由应用决定；`kbd` 给 `None` |
| `vertical_align` | 相对**所在样式 span** 解析（`LayoutInlineBox.parent_style_index`） | P05 只有一个 style span ⇒ 等价于相对行 |

### 12.2 行内盒的输出与基线

probe ① 实测（16 px，盒 20×18）：

```text
baseline=None   → line_height=28.00  box_y=0.00   layout_h=28.00
baseline=Some(4) → line_height=34.00 box_y=10.00  layout_h=34.00
baseline=Some(20)→ line_height=24.00 box_y=0.00   layout_h=24.00
```
| 事实 | 结论 |
|---|---|
| 盒参与行高 | 是：`line_height` 随盒的基线位置变化（上沿撑高行） |
| `box.y` 是行块轴的**顶边**绝对坐标 | 是（`line.rs:439` = `line.inline_box_top`） |
| 盒内基线与文本基线的关系 | `baseline` 是**盒内**相对值，parley 负责换算；引擎只透传 |
| 行内盒宽度 > 可用行宽 | parley 会把它放到独立行并可能溢出；引擎在 shape 后检测 `PositionedInlineBox.width > line_metrics.advance` 并记 `TextDiagnostic::InlineBoxOverflow`（§4.8 表格要求「降级为独立行并记入 CPU 帧记录」）。**不**改写布局（改写要二次 shape） |

### 12.3 `scale` 与行内盒

行内盒尺寸是**逻辑像素**（`InlineBoxSpec.size`），parley 的布局坐标是物理像素 ⇒ shape 前必须把
`size`、`baseline` 乘 `scale`，输出时再除回（`ShapedInlineBox.rect`）。这是一处**必须**的换算：
`to_inline_box(spec, scale)` 而不是 `to_inline_box(spec)`（§7.1 的调用点补 `scale` 参数）。

### 12.4 Markdown 管线的边界（本阶段只定表示，不做渲染）

| 元素 | P05 的表示 | 落地 Phase |
|---|---|---|
| 链接下划线 | `StyleProperty::Underline(true)` + `UnderlineOffset/Size/Brush`，作用在**字节 range** 上（`RangedBuilder::push(.., range)` 已核验，builder.rs:59） | 拥有 markdown 的 Phase（`TextContent` 增 `spans: SmallVec<[TextSpan; 2]>`，additive） |
| `kbd` | `InlineBoxSpec { size, baseline: None, align: Baseline }` + 应用自绘背景/边框（P15 的 `Canvas` 或 P06 的 quad） | 同上 |
| 行内图片 | `InlineBoxSpec { size, baseline: Some(h*0.8), align: Baseline }` + 图片资源（P15 的 `Image` 图元） | P15 |
| 折行 | 由 parley 断行（`index` 决定位置，盒参与断行） | P05 ✅ |
| 右对齐行尾空白 | parley 的 `hanging_advance` 已处理（`LineMetrics.hanging_advance`，§7.3）；`ShapedTextLine.advance` 记**含**悬挂空白的推进，逐字形位置不受影响 | P05 ✅ |

**为什么本阶段不做 `Vec<InlineFragment>` 数据管线**：它的消费者（Markdown 渲染）不在 P05，而 `InlineBoxSpec`
把「盒的输入面」钉死后，Markdown 侧只需要做 AST → `(range, InlineBoxSpec)` 的投影，不需要引擎改动
（AGENTS §1.2：不做预防性抽象）。这条写进 §18 的裁决项 ④。

### 12.5 选区映射：`(anchor, focus)` 字节偏移 → `Rect`

```rust
/// `(anchor, focus)` 字节偏移 → 文本盒局部的选区矩形（每行一个）+ 行号。
pub(crate) fn selection_rects(
    layout: &parley::Layout<ColorBrush>, anchor: usize, focus: usize, scale: f32,
) -> Vec<(Rect, usize)> {
    use parley::editing::{Cursor, Selection};
    use parley::Affinity;
    let a = Cursor::from_byte_index(layout, anchor, Affinity::Downstream);
    let f = Cursor::from_byte_index(layout, focus, Affinity::Upstream);
    let sel = Selection::new(a, f);
    // BoundingBox 是 f64；除以 scale 回逻辑 px，并压成 `Rect`
    sel.geometry(layout).into_iter().map(|(b, line)| (to_rect(b, scale), line)).collect()
}
```

| 决策 | 结论 | 理由 |
|---|---|---|
| 用 parley 的 `Selection` 而不是自算 | **必须** | bidi、连字、簇边界只有 parley 的 cluster 数据知道；自算会错（probe ①：`[0,6)` → 1 个 rect `(0,0)-(43.56,18)`） |
| 为什么不放 `ShapedLine` 上 | `ShapedLine` 是快照（无 parley `Layout`），选区要活数据 | 映射挂在 `NodeTree::text_selection_rects(node, anchor, focus)`（`pub`，P10 消费），内部用 `text_slots[node].shaped.layout` |
| 空选区（`is_collapsed`） | 返回空 `Vec`（`Selection::geometry` 对 collapsed 返回空，selection.rs:509-511） | P10 想要光标矩形时用 `Cursor::geometry(layout, width)`（cursor.rs:279） |
| 坐标 | 文本盒局部逻辑 px；P10 加节点原点 | 与 `ShapedLine` 一致 |
| 归一化 | `anchor`/`focus` 顺序无关（`Selection::text_range` 取 min/max） | parley 已处理 |

`NodeTree::text_selection_rects` 的公开签名（P10 的入口）：

```rust
/// `(anchor, focus)` 字节偏移 → 该文本节点内的选区矩形（文本盒局部），`Err` = 句柄陈旧或不是文本节点。
pub fn text_selection_rects(&self, node: NodeId, anchor: usize, focus: usize) -> Result<SmallVec<[Rect; 4]>, EngineError>;
```
（`SmallVec` 出现在公开面会泄漏 `smallvec` 版本 —— 用 `Vec<Rect>` 更诚实；改为 `Result<Vec<Rect>, EngineError>`，S-55。）

---

## 13. 文本节点接入（`NodeKind::Text` / builder / app / window / error / lib）

### 13.1 `node/tree.rs`

```rust
/// 节点的种类（P03 的 3 变体 + P05 的 Text）。
#[derive(Clone, Debug, PartialEq)]
pub enum NodeKind {
    Box,
    Canvas(Canvas),
    Geometry(GeometryId),
    /// 文本叶子：`TextContent` 是载荷；布局由 `App::refresh_text` 注入的 measure 闭包完成。
    Text(TextContent),
}

impl NodeKind {
    pub(crate) fn is_leaf(&self) -> bool { matches!(self, Self::Canvas(_) | Self::Geometry(_) | Self::Text(_)) }
    /// P06 的 paint 分派用：取文本载荷。
    pub(crate) fn text(&self) -> Option<&TextContent> { match self { Self::Text(c) => Some(c), _ => None } }
}
```

`Node` 新增两个字段（**手写 `Debug` 必须同步**，P04 N4 的规则）：

| 字段 | 类型 | 大小 | 写者 | 非 test 读者 |
|---|---|---|---|---|
| `text_bind` | `Option<Box<dyn Fn(&mut ReadCtx<'_>) -> SmolStr>>` | 8 | `insert_child`（builder） | `App::refresh_styles`（`take_bind_text`/`put_bind_text`） |
| `text_measured_size` | `Size` | 8 | `set_text_shaped` | `App::refresh_text`（判「尺寸变了没有」） |

```rust
// NodeTree 的文本侧表（V-31 的解：**不**内联进 Node，仿 P04 的 layout_cache）
pub(crate) struct TextSlot { shaped: Rc<ShapedText>, line: Rc<ShapedLine> }
// NodeTree 新字段：
//   text_slots: Vec<Option<TextSlot>>,        // 按 arena 槽位下标（16 B/槽，仅文本节点非 None）

impl NodeTree {
    pub(crate) fn text_content(&self, node: NodeId) -> Option<&TextContent>;
    /// 写载荷（应用层经 `set_style` 之外的唯一文本入口；`App::refresh_styles` 求值 `bind_text` 后用）。
    pub(crate) fn set_text_content(&mut self, node: NodeId, content: TextContent) -> bool;
    pub(crate) fn take_bind_text(&mut self, node: NodeId) -> Option<Box<dyn Fn(&mut ReadCtx<'_>) -> SmolStr>>;
    pub(crate) fn put_bind_text(&mut self, node: NodeId, f: Box<dyn Fn(&mut ReadCtx<'_>) -> SmolStr>);
    /// 冻结形状 + 重建 measure 闭包（§7.4）。
    pub(crate) fn set_text_shaped(&mut self, node: NodeId, shaped: Rc<ShapedText>);
    /// 挂 `ShapedLine`（P06 的输入）。
    pub(crate) fn set_text_line(&mut self, node: NodeId, line: Rc<ShapedLine>);
    pub(crate) fn text_line(&self, node: NodeId) -> Option<&Rc<ShapedLine>>;
    pub(crate) fn text_slot(&self, node: NodeId) -> Option<&TextSlot>;
    pub(crate) fn text_measured_size(&self, node: NodeId) -> Option<Size>;
    /// 公开：选区映射（§12.5）。
    pub fn text_selection_rects(&self, node: NodeId, anchor: usize, focus: usize) -> Result<Vec<Rect>, EngineError>;
    /// 文本节点的 LAYOUT 失效（= P04 的 invalidate_measure + 补 PAINT，§6.3）。
    pub(crate) fn invalidate_text_layout(&mut self, node: NodeId) -> bool;
}
```

`remove(node)` 必须**同步清 `text_slots` 槽位**（否则 16 B/槽泄漏 + 陈旧 `Rc` 让已卸载文本的 `Layout` 常驻）：
`Rc` 随槽位置空而释放；同时 `DirtyFlags` 的既有语义不变（文本节点仍是叶子，`depends_on_children` 恒假）。

> **N5（槽位复用）**：`text_slots` 与 `layout_cache` 同生命周期 —— `insert_child` 在 `arena` 里拿到**复用槽位**时
> 必须 `resize_with(index+1, || None)` 后置 `None`，`remove` 对被卸载的整棵子树逐个置 `None`。
> 漏掉会让新节点继承上一代文本的 `Rc<ShapedText>`（`text_shaped` 命中陈旧快照 ⇒ 不 shape、显示旧文本）。
> 用例 `text_slot_cleared_on_remove_and_reuse`（§15.8 #58）固化。

### 13.2 `node/builder.rs`

```rust
impl NodeBuilder {
    /// 文本叶子（静态文本）。挂载后由 `App::refresh_text` 首次形状 —— 在此之前 measure 为 0×0。
    #[must_use] pub fn text(text: impl Into<SmolStr>, style: TextStyle) -> Self;
    /// 动态文本：每次 `refresh_styles` 求值（与 `bind_style` 同一机制、同一依赖图）。
    ///
    /// 返回值是 `SmolStr`（不是 `String`）：短字符串零分配（AGENTS §2.2；**对 brief 的修订**，S-57）。
    #[must_use] pub fn bind_text(self, f: impl Fn(&mut ReadCtx<'_>) -> SmolStr + 'static) -> Self;
}
```
| 规则 | 内容 |
|---|---|
| `text()` 是否设 measure | **不设**（`None` ⇒ P04 的 `compute_leaf` 返回 `Size::ZERO`）：避免在 builder 里伪造一个与真实形状不一致的闭包 |
| `bind_text()` 与 `text()` 可否同时 | 可以：`text()` 给初始值，`bind_text()` 在首个 `refresh_styles` 起覆盖它（语义 = 「初始值 + 绑定」） |
| 文本节点的子节点 | 不允许：`is_leaf()` 含 `Text` ⇒ `build` 的 debug 断言直接失败（P04 S-36 的既有语义） |
| `bind_text` 的依赖 | 与 `bind_style` **同一个** `ReadTracker`（一次 `replace_deps`），因此在 `refresh_styles` 的同一循环里求值 |

### 13.3 `app.rs`（三处修改 + 六个新方法）

```rust
pub struct App {
    // …既有字段…
    text: TextContext,          // 进程级：字体库 / 布局 scratch / 字体栈 / 形状缓存 / 诊断
    raster: GlyphRasterizer,     // 进程级：swash ScaleContext（每线程一个）
    atlas: AtlasSet,             // 进程级：Glyph 页 + Image 页（P09 的上传目标）
    glyph_atlas: GlyphAtlas,     // 进程级：字形 key → tile（与 atlas 同事务换代）
    scale_epochs: ahash::HashMap<WindowId, u32>,   // 每窗口「图集已同步到的 scale_epoch」（§13.3 步骤 1）
}
```
> **N3（命名统一）**：只有 `Window::scale_epoch()` 与 `App.scale_epochs` 两个名字；判定用
> `scale_epochs.entry(id).or_insert(0)` 读旧值，`!= window.scale_epoch()` 即换代。**不存在** `text_scale_epoch` 字段。

**(a) `refresh_styles` 增加 `bind_text` 求值**（逐字插入原循环，`evaluated` 语义不变）：

```rust
let old_text = tree.text_content(node).cloned();
let bind_text = tree.take_bind_text(node);
let text_value = bind_text.as_ref().map(|f| f(&mut ctx));      // 同一个 ReadCtx / 同一个 tracker
…
tree.put_bind_text(node, bind_text);
if let Some(text) = text_value {
    let style = old_text.as_ref().map_or_else(TextStyle::default, |c| c.style.clone());
    if old_text.as_ref().is_none_or(|c| c.text != text) {
        tree.set_text_content(node, TextContent::new(text, style));
        tree.mark_paint(node);                                  // 文本变了 → 至少要重画
        report.text_changed += 1;
    }
}
```
`StylePassReport` 新增 `pub text_evaluated: usize` / `pub text_changed: usize`（P06 帧记录要它们；
构造点只有 `app.rs` 一处 + 测试）。

**(b) 新 pass `App::refresh_text(window)`**：

```rust
/// 文本 pass：形状（缓存）→ 光栅 → 图集 → `ShapedLine`；尺寸变化时 `invalidate_measure`。
///
/// 必须在 `refresh_styles` 之后、`compute_layout` 之前调用（帧序：styles → text → layout）。
pub fn refresh_text(&mut self, window: WindowId) -> Option<Result<TextPassReport, EngineError>>;
```
逐字算法：

```text
1. 取 window；若 window.scale_epoch() != self.text_scale_epoch[window] → atlas.clear_all() + glyph_atlas.clear()
   + 记录新 epoch（DPI 变化 ⇒ 图集换代，P02 §10.6）
2. report = TextPassReport::default()
3. 遍历 tree.order()（DFS 先序，稳定）：
   a. 只看 NodeKind::Text；取 content（空文本也走完流程，产出空 ShapedLine）
   b. 形状缓存 key = ShapeKey::new(&content, scale, self.text.font_stack_version())
      - 命中 → report.reused += 1；Rc clone
      - 未命中 → self.text.shape(&content, inline_boxes, scale)?  （失败 → EngineError::TextShape）
                 → Rc::new(ShapedText) + cache.insert；report.shaped += 1
   c. 光栅/装箱：对每个 run 的每个字形调 glyph_atlas.ensure((FontKey, id, ppem), &mut raster, atlas.glyph_mut())
      - 命中 → tiles_reused += 1；未命中 → rasterized += 1
      - `Err(AtlasExhausted)` → return Err（调用方丢弃本帧，P04 §9.4 的同类处理）
      - id == 0 → tofu += 1 + diagnostics.push(MissingGlyph)
   d. 组装 ShapedLine（坐标除以 scale；tile.position 用物理/scale）
   e. 若该节点的 `text_slots` 已存在且 `tiles 未 stale && scale_epoch/shape_key 未变` → 跳过（report 不变）
   f. 计算 `measured = shaped.measure(MaxContent)`（**逻辑 px**）与 `size_changed = measured != tree.text_measured_size(node)`
      tree.set_text_shaped(node, shaped, measured, size_changed)   // **无条件清 node..root 缓存**（B1）
      tree.set_text_line(node, Rc::new(shaped_line))
      —— `size_changed` 只决定是否额外置 `LAYOUT`；缓存清理与它无关（§7.4 的失效纪律表）
4. Some(Ok(report))
```

> **N2（借用）**：`refresh_text` 用**索引循环**而不是 `for node in tree.order()`：
> `tree.order()` 持有 `&NodeTree`，而循环体要 `&mut tree`（`set_text_shaped`）⇒ E0502。
> 写成 `let mut index = 0; while index < tree.order().len() { let node = tree.order()[index]; index += 1; … }`
> （`NodeId: Copy`，借用在取下标后立即结束）。`App` 侧同理：先 `let Self { text, raster, atlas, glyph_atlas,
> windows, scale_epochs, .. } = self;` 把字段拆成互不重叠的可变借用，再取 `windows.get_mut(&id)?`。

**(c) 字体栈与诊断入口**：

```rust
pub fn font_stack(&self) -> &FontStack;
pub fn font_stack_version(&self) -> u32;
/// 换字体栈：版本 +1、清形状缓存、所有窗口的**文本节点** `LAYOUT | PAINT`。
pub fn set_font_stack(&mut self, stack: FontStack);
/// 系统字体安装/卸载/语言切换后调用：重载 + 版本 +1 + 同上失效。
pub fn reload_system_fonts(&mut self);
/// tofu / 边界 / 溢出诊断（D17 的可观测面）。
pub fn text_diagnostics(&self) -> &[TextDiagnostic];
```

| 决策 | 结论 | 理由 |
|---|---|---|
| 为什么 `refresh_text` 独立于 `refresh_styles` | 职责分离 | styles = 求值「应用状态 → 样式/文本」；text = 「文本 → 字形」；P06 的 paint 是第三个 pass。一个 pass 只做一件事 |
| 为什么返回 `Option<Result<..>>` | 窗口不存在 ≠ 文本错误 | 与 `refresh_styles` 的 `Option<StylePassReport>` 同构；错误是 `TextShape`/`AtlasExhausted` |
| 为什么遍历 `tree.order()` 而不是脏位 | 正确性优先 | 脏位只能表达「可能变」；文本的新旧尺寸比较需要「本帧一定访问过」。`order()` 的代价 O(n)，P06 再优化为脏位（V-45） |
| 空文本 | 产出 `ShapedLine { lines: [], width: 0, height: 0, .. }` + measure 0 | §7.5 |

### 13.4 `window.rs`

| 修改 | 内容 |
|---|---|
| 字段 | `scale_epoch: u32`（`Window::new` 起 0，`set_scale` 时 `+= 1`） |
| 方法 | `pub fn scale_epoch(&self) -> u32`（文本 pass 判定图集换代） |
| `set_scale` | 除既有的 `mark_layout(root)` 外，`self.scale_epoch = self.scale_epoch.wrapping_add(1)`（**不**在这里清图集：图集归 `App`，`App::refresh_text` 读 epoch 后清） |
| `compute_layout` 的 rustdoc | 增加一行：**「文本节点必须先在本帧调用 `App::refresh_text`；否则它们的 measure 是 0×0（`NodeBuilder::text` 不设 measure）」** |
| 为什么 `Window` 不持有 `TextContext` | §6.1（N 份 19 MB 字体 blob） |

### 13.5 `lib.rs` / `error.rs`

```rust
// src/lib.rs（只增两行，仍是纯模块声明）
pub mod text;
mod render;
```

```rust
// src/error.rs —— P01 §6.2.2 #13 / #12 逐字落地
/// 文本形状失败（字体回退全部失败 / 行内盒边界非法 / 超宽降级需上报）。
#[error("text shaping failed for {node:?}: {detail}")]
TextShape { node: NodeId, detail: SmolStr },

/// 图集无法容纳新图元（清空重建后仍失败）。
#[error("atlas {:?} is exhausted: no free tile fits", kind)]
AtlasExhausted { kind: AtlasKind },
```
`error.rs` 的模块级文档要同步删掉「`AtlasExhausted` → P09、`TextShape` → P06」这两句（它们已落地），
并保留其余未落地变体的说明。`all_variants()` 测试向量补这两项（`Display` 断言各一条）。

---

## 14. 尺寸预算与死代码

### 14.1 `Node` 尺寸（probe 实测 + 推算）

| 项 | 实测/推算 | 说明 |
|---|---|---|
| `Node`（现状） | **1016 B**（`cargo test node_and_handle_sizes`） | P04 的结论 |
| `NodeKind`（现状） | **48 B** = `Canvas` | `GeometryId` 8 B |
| `TextStyle` | **72 B** | `family: Option<SmolStr>` 24 + `Color` 20 + `Option<f32>` 8 + 4 个 `f32`/`u16`/4 个枚举 |
| `TextContent` | **96 B** | `SmolStr` 24 + `TextStyle` 72 |
| `NodeKind::Text` | **96 B**（实测；载荷 96 吃掉判别位） | `Canvas` 的 `Vec` 提供 niche，因此 `NodeKind` 只从 48 → 96（+48 B） |
| `Node`（P05） | **1088 B**（实测：1016 − 48 + 96 + `text_bind` **16** + `text_measured_size` 8） | `Option<Box<dyn Fn>>` 是**胖指针 16 B**（不是 8：`Box<dyn Fn>` = data + vtable） |
| 断言上界 | **1152 B**（P04 的 `node_and_handle_sizes` **已是** `<= 1152`，不含 1024） | 余量 64 B；**实现实测 1112 B**（S-59 的 `TextContent` 加 `Box<[InlineBoxSpec]>` 后），P06 再加 `paint`/`content_offset` 后为 1128 B（见 `design/phase-06-paint-runtime.md` §11.1） |
| 备选（若 P06/P07 再增字段顶破） | `NodeKind::Text(Box<TextContent>)` ⇒ `Node` 回 **1040 B** | `Option<Box<TextContent>>` 实测 **8 B**；代价 = 每个文本节点一次 96 B 分配 |

**结论（S-53）**：内联 `TextContent`（**实测 1088 B** ≤ 1152 B，余量 64 B），**不**用 `Box`。P04 的
`node_and_handle_sizes` 上界**已经是 1152**（`node/tree.rs:1310`：「过 1152 → 备选 `style: Box<Style>`」），
本阶段**不改**该断言，只**追加**两条打印/断言：`size_of::<TextContent>() == 96` 与 `size_of::<Node>() <= 1152`
（后者在 P05 实现首日复跑并把实测值回填本表）。`NodeKind` 的 `large_enum_variant` 已 `allow`（workspace lint），
不产生告警。

> **实测更正（2026-11-12，S-59 落地后）**：`TextContent` 因 `inline_boxes: Box<[InlineBoxSpec]>` 变为 **112 B**、
> `NodeKind` **112 B**，`Node` 实测 **1112 B**（不是 1088）。上表的 1088 是 N1（`inline_boxes`）落地前的推算，
> 以 **1112** 为准；P06 追加 `paint`(8)+`content_offset`(8) 后为 **1128 B**（余 24 B）。

### 14.2 `TextSlot` 的旁表成本

`Vec<Option<TextSlot>>`：16 B/节点（`Rc` ×2）。10k 节点 = 160 KB（vs 内联进 `Node` 的 1600 万 B 级别增长）。
槽位只在 `insert_child` 时 `push(None)`、`remove` 时置 `None` —— 与 `layout_cache: Vec<Option<taffy::Cache>>`
（408 B/槽，P04 §6.1）同构，但**只在文本节点上非空**。

### 14.3 死代码审计（落地判据）

- 每条 `pub(crate)` 必须在 §4.3 表里找到**非 test** 调用点；表外的项删除（AGENTS §4）。
- `render/atlas.rs` 的**生产** `pub(crate)` item 的消费者全部是 `text/atlas.rs` + `App::refresh_text`
  （**文本图集，非 test**）—— 这正是 S-54 把光栅/装箱放进 P05 的原因。
- **实现期修正（2026-11-12）**：v1 没有逐 tile 逐出，因此 `Allocator::{deallocate, take_dirty_rows}`
  与槽位表在**生产构建里没有读者** ⇒ 按 AGENTS §4 的「表外一律删除」把它们降为 `#[cfg(test)]`
  （`for_test`/`tile_of` 同类）。脏行的生产读面改为 `dirty_span_count()`（不消费），
  真正的 `take_dirty_rows` 消费者是 P09 的上传路径，落地时把该方法的 `cfg(test)` 去掉即可。
  `DirtyRows::is_full` 由 `mark` 的 fast-path 使用，`spans` 由 `dirty_span_count` 使用 ⇒ 两者都有非 test 读者。

---

## 15. 测试清单（计划 **62** 项 / 实测 **85** 项，全部内联 `#[cfg(test)] mod tests`）

> 断言要点一律写成「事实 + 数值/关系」；CJK/emoji 断言不依赖具体字体（换机器仍成立），
> 只断言**关系**（行数、单调性、非空 alpha、tile 变化），**不**断言具体 advance（D17：三平台不要求像素一致）。

### 15.1 `text/shape.rs`（12）

| # | 用例 | 断言要点 |
|---|---|---|
| 1 | `shape_latin_single_line` | `lines().len()==1`、`first_baseline>0`、`width>0`、`height>0` |
| 2 | `shape_cjk_breaks_between_chars` | 60 px 宽、`"世界世界世界"` ⇒ 行数 ≥ 3、每行 `text_range` 落在 char boundary |
| 3 | `shape_mixed_latin_cjk_emoji` | 混排不 panic；字形总数 ≥ 字符数 × 0.9；**tofu 断言必须是 `tofu == 0 \|\| diagnostics 含 FontStackMiss`**（N1）—— 宿主无 CJK/emoji 字体时形状仍合法，只是落到 `.notdef`；此时 `eprintln!` 一条 skip 说明并**不** panic（防 CI 假绿/假红） |
| 4 | `rebreak_changes_lines_not_layout` | 同 `Rc<ShapedText>` 上 `break(600)`→1 行、`break(80)`→>1 行；`layout.text_len()` 不变 |
| 5 | `align_center_shifts_first_glyph_x` | `Alignment::Center` 后首字形 x > `Start` 时的一半宽度；再 `Start` 复位（证明 align 不累积） |
| 6 | `content_widths_match_breaks` | `widths.min == break(1.0).width()`、`widths.max == break(None).width()`（probe ② 的回归） |
| 7 | `empty_text_is_zero_size` | `measure` 返回 `Size::ZERO`；`ShapedLine.lines.is_empty()`、`width==0`、`height==0` |
| 8 | `measure_definite_width_wraps` | `Definite(80)` 的 height > `Definite(600)` 的 height |
| 9 | `measure_min_content_is_longest_word` | `MinContent` 的 width == `widths.min`（±0.01） |
| 10 | `measure_max_content_is_single_line` | `MaxContent` 的 height == 单行高度（±0.01） |
| 11 | `shape_is_deterministic` | 同一输入两次 shape 的 `width/height/baseline` 位相等（f32 `to_bits`） |
| 12 | `inline_box_splits_run` | 带 `InlineBoxSpec{index:4}` 的文本：`items` 里 `InlineBox` 出现在第 0 个 run 之后；`ShapedInlineBox.rect.width == spec.size.width` |

### 15.2 `text/cache.rs`（6）

| # | 用例 | 断言要点 |
|---|---|---|
| 13 | `cache_hit_same_key` | 同 `TextContent`+scale 两次 `shape` ⇒ `shaped==1, reused==1`（`TextPassReport`） |
| 14 | `cache_miss_on_text_change` | 改一个字符 ⇒ `shaped==2` |
| 15 | `cache_miss_on_style_change` | 只改 `color` ⇒ 新 key（`style_hash` 变） |
| 16 | `cache_miss_on_scale_change` | `scale 1.0 → 1.25` ⇒ miss（`scale_bits` 参与键） |
| 17 | `cache_miss_on_font_stack_version` | `set_font_stack` 后 ⇒ miss |
| 18 | `cache_lru_evicts_and_keeps_node_alive` | 插入 300 条（>256）⇒ `len()==256`；被逐出节点的 `Rc::strong_count ≥ 1`（节点闭包仍持有） |

### 15.3 `text/raster.rs`（6）

| # | 用例 | 断言要点 |
|---|---|---|
| 19 | `raster_latin_glyph_has_alpha` | `A` 在 16 ppem 下 `data` 非空且存在 `>0` 字节；`data.len() == w*h` |
| 20 | `raster_placement_negative_left` | `j` 的 `placement.left < 0`（probe ②：-2）；quad x = pen + left 仍 ≥ 0-2 |
| 21 | `raster_ppem_rounds` | `size=13.5, scale=1.25` ⇒ 实际 ppem == 17（`round(16.875)`） |
| 22 | `raster_space_is_empty` | `' '` ⇒ 无图像（`None`），不产生 tile |
| 23 | `raster_ttc_index_is_used` | 用 msyh.ttc#0 与 #1 的同一字符光栅化 ⇒ 同一个非零 alpha（证明 index 生效；若文件不存在则 skip） |
| 24 | `raster_is_stable` | 同 `(font, glyph, ppem)` 两次光栅化字节相等 |

### 15.4 `render/atlas.rs`（10）

| # | 用例 | 断言要点 |
|---|---|---|
| 25 | `allocate_content_plus_padding` | `allocate((6,9))` ⇒ `content.w==6`、`content.h==9`、`padded` 的 `w≥8`、`h≥11`（etagere 取整，probe ④） |
| 26 | `uv_uses_edges_without_half_texel` | `u1-u0 == 6/2048`（±1e-6），且 `u0*2048` 是整数 |
| 27 | `padding_is_one_texel` | `content.min.x == padded.min.x + 1` |
| 28 | `deallocate_is_idempotent` | 连续两次 `deallocate(id)` 不 panic（etagere 会 panic，probe ④） |
| 29 | `deallocate_unknown_id_is_ignored` | 构造一个不存在的 `AllocId`（用第二次 `clear` 之后的旧 id）⇒ 忽略 |
| 30 | `clear_bumps_generation` | `clear()` 后 `generation == old+1`；旧 `AtlasTile::is_stale(new)` 为真 |
| 31 | `allocate_zero_or_oversize_returns_none` | `(0,10)` / `(2048,2048)` / `(3000,10)` ⇒ `None` |
| 32 | `exhaustion_then_clear_retry` | 填满页后 `allocate` 为 `None`；`clear()` 后同一请求成功 |
| 33 | `dirty_rows_merge_and_fold` | 标记 3 段相邻 ⇒ 1 段；标记 10 段 ⇒ `is_full()` |
| 34 | `atlas_set_two_kinds_independent` | `glyph` 与 `image` 各分配同尺寸块不互相挤占；`clear_all` 让两者 generation 都 +1 |

### 15.5 `text/atlas.rs`（5）

| # | 用例 | 断言要点 |
|---|---|---|
| 35 | `glyph_tile_is_cached` | 同一 `GlyphKey` 两次 `ensure` ⇒ `tiles_reused==1`、`rasterized==1` |
| 36 | `glyph_tile_differs_per_ppem` | 13 与 16 ppem ⇒ 两个不同 `AtlasTile`（UV 不同） |
| 37 | `glyph_tile_stale_after_clear` | `clear` + `generation` 变 ⇒ 旧 tile `is_stale`，`ensure` 重新分配 |
| 38 | `atlas_exhausted_after_retry` | 用 1×1 的假页把页填满后请求大块 ⇒ `Err(AtlasExhausted{kind: Glyph})`（`TextShape`/`AtlasExhausted` 的错误路径） |
| 39 | `glyph_atlas_key_includes_font` | 两个不同 `FontKey`（不同 blob）× 同 glyph ⇒ 两个 tile（不串字体） |

### 15.6 `text/inline_flow.rs`（8）

| # | 用例 | 断言要点 |
|---|---|---|
| 40 | `inline_box_index_maps_to_byte_offset` | `index:4` 的盒出现在第 4 字节的位置（x == 前 4 字节的 advance ±0.01） |
| 41 | `inline_box_baseline_affects_line_height` | `baseline=None` 与 `Some(4)` 的 `line_height` 不同（probe ①：28 vs 34） |
| 42 | `inline_box_not_on_char_boundary` | 中文文本 `index:1`（在多字节码点内）⇒ debug 断言 / release 吸附 + `TextDiagnostic::InlineBoxOffBoundary` |
| 43 | `inline_box_wider_than_line_reports_overflow` | 盒宽 > 行宽 ⇒ `TextDiagnostic::InlineBoxOverflow`，且布局不 panic |
| 44 | `selection_rects_cover_range` | `[0,6)` ⇒ 1 个 rect，`x0==0`、`x1 ≈ 前 6 字节 advance`（probe ①：43.56） |
| 45 | `selection_rects_multi_line` | 跨 2 行的选区 ⇒ 2 个 rect（行号 0/1） |
| 46 | `selection_collapsed_is_empty` | `anchor==focus` ⇒ 空 `Vec` |
| 47 | `selection_is_order_independent` | `(6,0)` 与 `(0,6)` 的 rect 集合相等 |

### 15.7 `text.rs`（6）

| # | 用例 | 断言要点 |
|---|---|---|
| 48 | `text_style_defaults` | `TextStyle::default()`：size 14.0、weight 400、`color` 不透明黑、`wrap == Wrap`、`family.is_none()` |
| 49 | `text_style_clone_is_value` | 改 clone 不影响原值（值语义） |
| 50 | `font_stack_system_ui_contains_cjk` | 候选表含 `Microsoft YaHei UI` 与 `PingFang SC`（**列表**含，不要求系统装着） |
| 51 | `font_stack_version_bumps_only_on_change` | 同内容 `set_families` ⇒ 版本不变；改一个名 ⇒ +1 |
| 52 | `text_content_is_comparable` | `TextContent` 的 `PartialEq` 按文本+样式（`bind_text` 的短路依据） |
| 53 | `shaped_line_size_budget` | `size_of::<TextContent>() == 96`、`size_of::<TextStyle>() == 72`（probe ③ 的回归；若改字段必须同步改断言） |

### 15.8 `node/tree.rs` + `builder.rs`（5）

| # | 用例 | 断言要点 |
|---|---|---|
| 54 | `text_node_is_leaf` | `NodeKind::Text` 的 `is_leaf()` 为真；给它 `build` 子节点 ⇒ debug 断言失败 |
| 55 | `bind_text_diff_marks_paint` | 改信号 ⇒ `refresh_styles` 后 `text_changed==1`、节点 `PAINT`、旧文本被替换 |
| 56 | `text_size_change_marks_layout` | `refresh_text` 后文本变长导致 size 变 ⇒ `LAYOUT` 位 + `invalidate_measure` 的 taffy 缓存清空 |
| 57 | `text_same_size_marks_paint_only` | 同宽同高的文本替换 ⇒ `LAYOUT` 位**不**置（§4.5 的目标语义） |
| 58 | `remove_drops_text_slot` | `remove` 后 `text_slot(node).is_none()`、`Rc::strong_count` 回落（无泄漏） |

### 15.9 `app.rs` / `window.rs` / `error.rs`（4）

| # | 用例 | 断言要点 |
|---|---|---|
| 59 | `refresh_text_requires_window` | 不存在的 `WindowId` ⇒ `None` |
| 60 | `font_stack_switch_relayouts_all_text` | `set_font_stack` 后所有文本节点 `LAYOUT|PAINT`、形状缓存 `len()==0` |
| 61 | `scale_epoch_clears_atlas` | `set_scale` 后 `refresh_text` ⇒ `atlas.generation()` +1、旧 `ShapedLine.atlas_generation` 变 stale |
| 62 | `text_shape_and_atlas_errors_display` | `TextShape`/`AtlasExhausted` 的 `Display` 含节点/种类；`EngineError: Send + Sync + 'static` 断言 |

**计数（实测 2026-11-12）**：新增 **85** 个用例（356 − 271）；`cargo test -p kytrix_gui` = **356 passed**、`--release` = **348 passed**（8 条 `#[cfg(debug_assertions)]` 不编译）。分布：`text/shape` 19、`text/context` 8、`text/cache` 2、`text/raster` 5、`text/atlas` 5、`text/inline_flow` 8、`text` 8、`render/atlas` 12、`node/tree` 10、`node/builder` 6、`app` 13、`window` 1、`error` 2（另有 5 条依赖宿主字体的用例在无字体时 `eprintln!` + skip，N1）。

---

## 16. 验收门禁

```powershell
# 1) 单元测试（debug + release）
cargo test  -p kytrix_gui
cargo test  -p kytrix_gui --release

# 2) clippy（workspace lint 由 [lints] workspace = true 继承）
cargo +nightly clippy -p kytrix_gui --all-targets -- -D warnings

# 3) 格式
cargo +nightly fmt -p kytrix_gui -- --check

# 4) 文档零警告
cargo doc -p kytrix_gui --no-deps

# 5) 三平台依赖解析（不链接）
cargo metadata --format-version 1 --filter-platform x86_64-pc-windows-msvc   | Out-Null
cargo metadata --format-version 1 --filter-platform aarch64-apple-darwin     | Out-Null
cargo metadata --format-version 1 --filter-platform x86_64-unknown-linux-gnu | Out-Null

# 6) 零 unsafe / 零 allow / 零 expect（bitflags 宏内除外）
Get-ChildItem crates/kytrix_gui/src -Recurse -Filter *.rs |
  Select-String -Pattern '\bunsafe\b|#\[allow|#\[expect' | Where-Object { $_.Line -notmatch 'bitflags' }
#   期望无输出

# 7) 模块形态：没有 mod.rs / #[path] / pub use
Get-ChildItem crates/kytrix_gui/src -Recurse -Filter *.rs | Where-Object { $_.Name -eq 'mod.rs' }
Get-ChildItem crates/kytrix_gui/src -Recurse -Filter *.rs | Select-String -Pattern '#\[path|^pub use '
#   期望无输出

# 8) 死代码审计：每条 pub(crate) 都能在 §4.3 找到非 test 消费者
Get-ChildItem crates/kytrix_gui/src -Recurse -Filter *.rs | Select-String -Pattern 'pub\(crate\) (fn|struct|enum|trait|type|const)'
Get-ChildItem crates/kytrix_gui/src -Recurse -Filter *.rs | Select-String -Pattern 'pub\(crate\) \w+:'
#   第 2 条必须无输出（field_scoped_visibility_modifiers = "forbid"）

# 9) 文件边界：只允许 P05 的新文件
Get-ChildItem crates/kytrix_gui/src -Recurse -Filter *.rs | Select-Object FullName
#   期望：既有 + src/text.rs + src/text/{context,shape,raster,atlas,cache,inline_flow}.rs
#         + src/render.rs + src/render/atlas.rs；**不得**出现 event.rs/a11y.rs/resources.rs/
#         test_support.rs/paint/instance.rs/render/damage.rs

# 10) 模块根形态
Get-Content crates\kytrix_gui\src\lib.rs        # 期望 9 行 pub mod + 1 行 mod render;
Select-String -Path crates\kytrix_gui\src\text.rs -Pattern '^impl ' | Measure-Object | % Count
#   期望每个契约类型恰好 1 个 impl 块（multiple_inherent_impl = "forbid"）

# 11) 依赖增量：只有 feature，没有新 crate
git diff --stat -- Cargo.toml crates/kytrix_gui/Cargo.toml Cargo.lock
#   期望只有 Cargo.toml 的 parley features 行；Cargo.lock 无变化（feature 不改变 lock）
cargo tree -p kytrix_gui -e normal --depth 1

# 12) parley 单版本 + complex-scripts 生效
cargo tree -p kytrix_gui -e features | Select-String 'parley feature "complex-scripts"'
#   期望恰有一行

# 13) Node 尺寸预算
cargo test -p kytrix_gui node_and_handle_sizes -- --nocapture
#   期望 size_of::<Node>() <= 1152（实测预计 1088）、size_of::<TextContent>() == 96
```

**「完成」判据（实测）**：1–13 全绿；`cargo test -p kytrix_gui` = **356**（271 + 85）；release = **348**；
`size_of::<Node>() <= 1152`；`cargo tree` 的 `parley` 恰好一个版本（0.12.0）。

---

## 17. Cargo 依赖

**零新依赖。** 只改一行 feature：

```toml
# Cargo.toml（workspace 根，[workspace.dependencies]）
parley = { version = "0.12.0", features = ["complex-scripts"] }
```

| 需要的依赖 | 现状 | 说明 |
|---|---|---|
| `parley` | 已声明（P01） | **必须**加 `complex-scripts`：否则 ICU4X 无 CJK 词典模型（probe ②：stderr 每段中文报 `No segmentation model`，断行退化为 WB999） |
| `fontique` / `skrifa` / `swash` / `etagere` | 已声明（P01） | `kytrix_gui/Cargo.toml` 已含；`swash` 依赖 `skrifa ">=0.31.1, <=0.44"` ⇒ 与 `skrifa 0.44.0` **共用一份**（dossier 实证），`cargo tree` 只应出现一个 `skrifa v0.44.0`。**N6**：`parley` 通过 `pub use fontique`（`lib.rs:109`）再导出**同一个** `fontique 0.12.0`，且 `parley 0.12.0` 的 `[dependencies.fontique]` 就是 `0.12.0`、`kytrix_gui/Cargo.toml` 直依赖的也是 `0.12.0` ⇒ 全仓只有一个 `fontique` 实例，`Collection` 与 `parley::FontContext.collection` 类型同一（不存在两个版本各持一份字体缓存的风险） |
| `ahash` / `smallvec` / `smol_str` | 已声明 | 缓存/字体栈/文本 |
| `peniko` | **不声明** | parley 的 brush 是我们的 `ColorBrush`；`peniko` 只是 parley 的 dev-dependency（F-04 保持） |
| `icu_*` / `harfrust` | **不声明** | `parley_engine` 的传递依赖（F-19 保持） |

`Cargo.lock` **不变**（feature 不改变依赖解析）。

---

## 18. 需裁决项（5 项，含建议）

| # | 事项 | 建议 | 备选 | 影响面 |
|---|---|---|---|---|
| 1 | **`TextContext` 是否公开** | **不公开**（crate 私有）+ `App` 的 6 个方法 + `TextPassReport`/`TextDiagnostic` 作公开面 | 公开（`pub struct TextContext { pub font_cx: parley::FontContext, .. }`，主设计 §4.5 的伪代码形态） | 公开则把 `parley` 拉进公开面（F-35 同类但更重），且应用层可绕过 `App` 直接 shape ⇒ 与 `refresh_text` 的所有权冲突；若裁决要公开，`TextContext` 需改成不透明句柄（字段全私有 + 只暴露 `font_stack*`） |
| 2 | **`bind_text` 的返回类型** | `SmolStr`（brief 写的是 `String`） | `String` | 用 `String` 会让短文本每次求值分配一次堆；`SmolStr` 对 ≤22 B 内联。若裁决要 `String`，在闭包出口 `SmolStr::new(&s)` 一次转换（多一次分配）即可，不改其它设计 |
| 3 | **纹理/图集归 `App` 还是 `Window`** | **`App`**（单 GPU 设备 ⇒ 单页纹理；probe ②：19 MB 字体 blob 只读一次） | `Window`（每窗口一套图集，DPI 变化只清自己） | 归 `Window` 时 `App::refresh_text` 需 `&mut Window` 的图集，且 `TextPassReport` 的 `atlas_rebuilds` 语义变成「每窗口」；P09 若要 per-window 纹理，本项要重开 |
| 4 | **本阶段是否实现 Markdown → `InlineFragment` 管线** | **不实现**（只落 `InlineBoxSpec` + 选区映射） | 实现（AST → 4 种 fragment → parley 属性） | 实现需要先有 `TextSpan`（range 级样式）与 `base_ui/markdown` 的 AST 类型；P05 无消费者（AGENTS §1.2），且会把 `TextContent` 撑大（Node 预算只剩 64 B） |
| 5 | **文本节点 measure 的注入方式** | **`App::refresh_text` 重建闭包**（`Rc<ShapedText>`，零 `RefCell`） | (a) `NodeTree` 持有 `TextContext` + `compute_leaf` 按 `NodeKind` 分派；(b) `Rc<RefCell<Option<Rc<ShapedText>>>>` 槽 | (a) 会让每个 `NodeTree::new()` 付字体枚举成本（271 个既有用例）且要改 P04 冻结的 `compute_leaf`；(b) 违反 `MeasureFn` 的纯函数契约。**若裁决选 (a)**，P04 的 measure 章节要重写 |

---

## 19. 风险与待复核项

| 编号 | 风险 | 等级 | 处置 |
|---|---|---|---|
| **V-39** | 长文本 measure 的 `clone + rebreak` 成本：16000 字符 **288 µs/次**（probe ④），×2–3 次/轮 ⇒ 近 1 ms | 中 | 先按 256 条缓存 + 文档写明「>8k 字符的段落建议拆节点」；若成为热点，给 `ShapeCache` 加「按宽度的测量记忆」（`(key, width_bits) → Size`，仍无 `RefCell`：记忆放在 `App` 侧，闭包只捕获 `Rc<MeasureMemo>`，见 V-42 的备选） |
| **V-40** | 整数像素落点让同一行内不同 ppem 的字形出现 ±0.5 px 基线抖动 | 低 | v1 接受；`quantize=true` 已把 baseline 取整（probe ①：18.0 vs 18.43），抖动只来自字形内部 `placement.top` |
| **V-41** | 彩色 emoji 在 R8 字形图集里只能是单色轮廓；CBDT-only 字体会渲染为空 | 中 | v1 用 `Source::Outline`（probe ⑤：`🌍` 有 213 个非零像素）；若出现空 emoji ⇒ 加 `AtlasKind::Color`（契约变更，P02 §10.3 的两种格式表要扩） |
| **V-42** | `MeasureFn` 的闭包持有 `Rc<ShapedText>` ⇒ 大段落的 `Layout` 在节点卸载前不释放 | 低 | `remove` 清 `text_slots`（§13.1）已释放；形状缓存逐出不影响节点持有的快照（这是**期望**行为：正在显示的文本不该因缓存逐出而重排） |
| **V-43** | 性能细项：`family_list()` 每次 `clone` 一次 `Vec`；`FontRef::from_index` 每次光栅化重解析表目录；`VecDeque` 的 LRU 提升 O(n) | 低 | 都在 µs 级；P18 基准若有热点再改（不改公开面） |
| **V-44** | 图集单页 2048²：字形页约 4 MiB，CJK + 4 个 DPI 档可能不够（P02 V-14） | 中 | 容量逃逸路径已实现（`clear + retry` → `AtlasExhausted`）；多页/扩容要 `AtlasTile.page` 字段（契约变更），P09 再评估 |
| **V-45** | 文本 pass 每帧遍历 `tree.order()`（O(n)）而不是脏位 | 低 | 先正确后优化：P06 的帧驱动可加 `TextDirty` 位（P03 的 `DirtyFlags` 还有 2 个保留位）；`take_dirty_rows` 的读者同理 |
| **V-46** | CI 的 Linux 容器可能没装 CJK/emoji 字体 ⇒ `tofu == 0` 的断言会假失败 | 中 | 断言写成「`tofu == 0` 或 `diagnostics` 非空」并单独断言诊断内容；Rustdoc 与测试注释写明「依赖宿主字体，D17 不要求跨平台像素一致」 |
| **V-47** | `parley` 的 `complex-scripts` 会拉 ICU 词典数据，二进制体积增加（未实测） | 低 | 中文断行的正确性优先；P18 的 release 体积预算若超，再评估「仅 CJK 平台开启」（会引入 `#[cfg]`，慎用） |

---

## 20. 本阶段追加到 `design/DEVIATIONS.md` 的条目（§十）

> 格式遵循 §九（`### F-53 …` / `### S-53 …` / `### 十·待复核项（P05 新增）` 表），**只追加**，不改历史条目。
> `V-38` 是 S-52 许诺但缺失的 P04 欠账，本阶段**补齐**（内容按 S-52 的 B2 行），P05 自己的待复核项从 **V-39** 起。

| ID | 标题 | 一句话 |
|---|---|---|
| F-53 | 形状缓存键**不含** `atlas_generation`（两级缓存） | 主设计 §4.5 的键混了两级；图集换代只影响 tile（S-11 的「generation 只在 clear 时 +1」） |
| F-54 | `parley::TextStyle` 存在但 P05 **不用**；`TextContext` 不公开 | 主设计 §4.5 的「Parley 0.12 无 TextStyle」是事实错误（F-03 已判），但 `RangedBuilder` 不吃它；`TextContext` 公开会把 parley 拉进公开面 |
| F-55 | `AtlasExhausted` 的落地 Phase 由 P09 改为 **P05** | 唯一生产者（分配器）本阶段落地；P09 只是消费者（与 F-51 同类） |
| F-56 | `invalidate_measure` **不**标 `PAINT`（S-47 的缺口），文本 pass 显式补 | 否则「字体栈切换 → 所有文本 LAYOUT + PAINT」里的 PAINT 会丢 |
| F-57 | `parley` 必须开 `complex-scripts` feature | 不开则 ICU4X 无 CJK 词典（stderr 报错 + 断行退化） |
| S-53 | `NodeKind::Text(TextContent)` **内联**，`Node` 1088 B | probe 实测；`Box` 备选留档 |
| S-54 | 光栅/图集在 P05 落地，`PositionedGlyph` 复用 P02 类型 | 否则 `render/atlas.rs` 无**非 test** 消费者；P06 只做 Scene 编码 |
| S-55 | `NodeTree::text_selection_rects` 返回 `Vec<Rect>`（不是 `SmallVec`） | 不把 `smallvec` 版本写进公开面（P04 §4.2 的同类理由） |
| S-56 | 空文本 = `Size::ZERO` + 0 行；parley 的 `""→4.4453×18` 是坑 | probe 实测；measure 特例化 + 用例固化 |
| S-57 | `bind_text` 返回 `SmolStr`（brief 写 `String`） | AGENTS §2.2 |

---

## 附录 A · 实现者检查单（按执行顺序）

1. `Cargo.toml` 加 `parley` 的 `complex-scripts`；`cargo tree -p kytrix_gui -e features | Select-String complex-scripts` 确认。
2. `error.rs`：加 `TextShape`/`AtlasExhausted` + `all_variants()` 两条 + Display 断言。**先落错误面**，后面才能返回 `Err`。
3. `render.rs` + `render/atlas.rs`：`AtlasAllocator`/`AtlasSet`/`DirtyRows`/`Allocation` + 10 个用例（§15.4）。**此步先让 `pub(crate)` 有消费者是下一步的事**，本步用 `#[cfg(test)]` 的用例跑通算法，`dead_code` 在 4 之后消失（若中途要过 clippy，先注释掉 `render` 的 `pub(crate) use`）。
4. `text.rs`：先定义 `TextStyle`/`TextContent`/`FontStack`/`InlineAlign`/`TextAlign`/`TextSlant`/`TextWrap`/`TextDirection` + `size_of` 断言（§15.7 的 6 条），再定义 `ShapedLine` 家族与 `TextPassReport`/`TextDiagnostic`。
5. `text/context.rs`：`FontStack::system_ui` + `family_list` + `font_stack_version` + 家族 id 缓存 + 诊断环形缓冲（不 shape）。
6. `text/shape.rs`：`shape` + `rebreak` + `ShapedText::measure` + 12 个用例（§15.1）。此时 `App` 还没接上，用例直接调 crate 私有函数。
7. `text/cache.rs`：`ShapeKey`/`ShapeCache` + 6 个用例（§15.2）。
8. `text/raster.rs`：`GlyphRasterizer` + 6 个用例（§15.3）。
9. `text/atlas.rs`：`GlyphAtlas` + 5 个用例（§15.5）。**到此 `render/atlas.rs` 的 `pub(crate)` 全部有非 test 消费者**。
10. `text/inline_flow.rs`：`to_inline_box` + `selection_rects` + 8 个用例（§15.6）。
11. `node/tree.rs`：`NodeKind::Text` + `Node.text_bind`/`text_measured_size` + `Debug` 同步 + `TextSlot` 旁表 + 5 个用例（§15.8）。
12. `node/builder.rs`：`text()`/`bind_text()` + 2 个用例。
13. `app.rs`：`refresh_styles` 的 `bind_text` + `refresh_text` + 6 个方法 + 4 个用例（§15.9）。
14. `window.rs`：`scale_epoch` + rustdoc。
15. 全量门禁（§16 的 13 条）+ 回填本文件的「实测」数字（测试数、`size_of`）。

## 附录 B · 与上游文档的交叉引用

| 主题 | 本文件 | 上游 |
|---|---|---|
| Parley 布局/断行/缓存 | §2.1、§7 | 主设计 §4.5；P01 A.2；DEVIATIONS F-03 |
| 字体策略 D17 | §6.2–§6.4 | 主设计 §4.6；F-18/F-19 |
| 行内流/选区 | §12 | 主设计 §4.7 |
| 错误路径 | §9.5、§13.5 | 主设计 §4.8；P01 §6.2.2 #12/#13 |
| 图集 | §9、§10 | 主设计 §2.8/§5.6；P02 §10；F-24/S-11 |
| `PositionedGlyph`/`GlyphRun` | §5.2、§9.6 | P02 §6.3.2；S-15/S-16/S-21 |
| measure | §7.4–§7.5 | P04 §9；F-50/S-47 |
| 公开路径 / 白名单 | §4 | P01 §4.5；P02 §4.2；P04 §4.1；M-02/M-07；S-16/S-23 |
| `Node` 尺寸 | §14.1 | P04 §8.3；V-30/V-31；S-51 |
| 模块根模板 | §3 | P01 §4.1；M-06 |
| 缓存预算 | §11 | 主设计 §9.3 |

---

## 21. 第二轮审查修订摘要（2026-11-12，S-59）

> 本节**覆盖**前文与之冲突的数字/流程（前文保留为设计轨迹）。

| 项 | 最终结论 |
|---|---|
| 帧序 | `refresh_styles` → `refresh_text`（shape + max-content 量尺寸）→ `Window::compute_layout` → **`App::layout_text`**（按内容宽 `break_all_lines` + `align` + 光栅/装箱 + 选区）→ paint |
| `refresh_text` | **不再**光栅化、**不再**产出 `ShapedLine`；快照未变（`ShapeKey` + `scale_epoch` + `Rc::ptr_eq`）⇒ 整节点跳过（`TextPassReport::skipped`） |
| `ShapedLine` 的断行 | 用 taffy 解析出的**内容宽**（`Layout.size − border − padding`，`set_local_layout` 存入 `Node.text_content_width`） |
| 选区 | 用**最终断行布局**（`TextSlot.broken`），不是 max-content 布局 |
| ppem | `parley::Run::font_size()` **已是物理值** ⇒ `.round().clamp(1,512)`，**不再**乘 `scale` |
| `TextStyle.family` | 前置进 `FontFamily::List`（`shape::family_list(stack, custom)`） |
| 行内盒 | `TextContent.inline_boxes: Box<[InlineBoxSpec]>` + `NodeBuilder::inline_boxes`；`ShapeKey` 哈希其字段 |
| 字形数据 | `GlyphRecord.data: Box<[u8]>`（R8 覆盖度，P09 上传源）；空白字形走 `missing` 负缓存 |
| B4 复位 | set_text_shaped 写新快照时清 line/broken/laid_* ⇒ layout_text 必重排（文本/style/family/inline_boxes 变更都走这条路）；efresh_styles 保留旧 inline_boxes |
| 图集代际 | `layout_text` 内不变；中途换代 ⇒ 整 pass 重跑一次（`TextLayoutReport::reruns`），再变则 `TextShape` |
| 字体栈状态 | `App::font_stack_status() -> FontStackStatus`；空栈/无匹配 ⇒ generic 兜底 + `FontStackMiss`（`ShapeFailure` 已删） |
| 尺寸（实测） | `TextContent` **112 B**、`NodeKind` **112 B**、`Node` **1112 B**（≤ 1152，余 40 B） |
| 用例/门禁 | `cargo test` **360**、`--release` **352**；clippy/fmt/doc 0 |
