# 文本 / 资源 / 无障碍审查

## 总体判断

`crates/ui_core` 的文本、资源和无障碍子系统整体架构方向正确：Parley 调用集中在 `ShapedText`，宽度变化用 clone + `break_all_lines` 重排而不重建 shape；字形与图片分别走带代际的图集；AccessKit 侧采用增量 `TreeUpdate`、懒激活和主线程动作回投。没有发现 P0 级（崩溃/数据损坏/安全）问题。

但有两个 P1 级风险会直接影响产品主路径：**可变字体的字重/字形实例没有被光栅器使用**（默认字体栈第一个就是 `Segoe UI Variable Text`），以及**图片图集被第一张图锁定页档且不能扩容**。此外，数字表格对齐所需的 font features、文本统计口径、海量文本节点的每帧遍历，以及 a11y 的实时刷新、表格/虚拟列表语义和控件动作仍有明显缺口；这些在“高频行情 + 复杂表格”产品里会被放大。

以下 12 条按影响排序，证据均为当前 HEAD `a9a4220` 代码。

## 发现（P0-P3）

### P1-1 可变字体实例与合成样式未进入字形光栅，`GlyphKey` 也不含 variation 坐标

- **证据**：`crates/ui_core/src/text/shape.rs:242-262`；`crates/ui_core/src/text/raster.rs:85-97`；`crates/ui_core/src/text/atlas.rs:13-19`；`crates/ui_core/src/text.rs:335-337`。
- **问题**：`rasterize` 只把 `font`、`glyph.id`、`ppem` 传给 `GlyphAtlas`；`GlyphRasterizer::rasterize` 只构造 `FontRef::from_index(...).size(ppem).hint(true)`，没有 `Run::normalized_coords()`、`Run::synthesis()` 的 `variation_settings/embolden/skew`。`GlyphKey` 只有 `font/glyph/ppem`，同一可变字体文件的 regular 与 bold 会命中同一 tile。`shape.rs:247-251` 对 `synthesis().any()` 只记诊断，不应用合成。
- **影响**：默认字体栈第一项是 `Segoe UI Variable Text`；在 Windows 11 等可变字体环境下，`TextStyle.weight = 700` 的标题/数字可能仍按默认轴（约 400）光栅化，而 Parley 用 700 的字形 advance，表现为“粗体不变粗、字距不匹配”，斜体也可能完全丢失。这不是仅影响装饰字体，而是默认 UI 字体主路径。
- **成熟实现对照**：HarfBuzz 用 `hb_font_set_variations` / shape plan 的 variation coordinates；Skia `SkFontArguments::setVariationDesignPosition` 创建字体实例；DirectWrite 按 `DWRITE_FONT_AXIS_VALUE` 取实例；Flutter 将 `FontVariation` 传到引擎。
- **建议**：让 `rasterize` 接收 `&[NormalizedCoord]` 和 `fontique::Synthesis`，使用 `ScalerBuilder::normalized_coords(...)`，并对 faux bold/italic 使用 `Render::embolden/skew`（或显式不应用并让字重只走真实 face）；把 coordinates/synthesis 纳入 `GlyphKey` 的哈希；增加可变字体 regular/bold/italic 的像素回归。
- **置信度**：高。

### P1-2 图片图集页尺寸由第一张图锁定，且没有扩容或多页

- **证据**：`crates/ui_core/src/resources.rs:198-206`、`crates/ui_core/src/resources.rs:213-230`；`crates/ui_core/src/render/atlas.rs:42-49`。
- **问题**：`ResourceCache::insert` 第一次插入时用 `page_for_tile(width, height)` 选 512/1024/2048 页并在 `open_page` 后永久锁定（`debug_assert!(self.page.is_none())`）；之后 `AtlasAllocator::allocate` 放不下就只返回 `DecodeCause::Capacity`，没有 `grow`/多页路径。若启动时先插入一个 16×16 图标，页是 512²，随后任何大于约 510 px 的图片（截图、K 线图、用户头像）都会永久失败，`unload` 所有条目也不能换页。
- **影响**：加载顺序决定整个进程的图片上限；行情软件通常先注册小图标/头像，再加载大图，属于可复现的主路径故障。同时单页 2048 上限会把 4K 截图降到 2046 px，图集页本身也会一直常驻。
- **成熟实现对照**：Skia/Chrome 的图像/字形图集支持多页、按尺寸分页或动态扩容；Flutter/Impeller 的 `TextureAtlas` 支持多页；Qt RHI 的 texture atlas 也按页管理。单页锁死是少见做法。
- **建议**：至少按尺寸档维护多个图片页，把 `page` 标识纳入 `AtlasTile`/UV；或提供显式 `reserve_image_page` / 首图前设置页档的 API，并在文档中要求“先插入最大图”。若短期保留锁档（F-208 的既定取舍），应把首图顺序风险暴露为启动诊断，而不是等应用随机 `Capacity`。
- **置信度**：高。

### P2-1 `TextStyle` 没有 font features / font variations，无法做 `tnum` 数字对齐

- **证据**：`crates/ui_core/src/text.rs:92-113`（`TextStyle` 字段仅 family/size/weight/slant/line_height/letter_spacing/color/alignment/wrap/direction）；`crates/ui_core/src/text/shape.rs:63-78`（只 push `FontFamily/FontSize/FontWeight/FontStyle/LetterSpacing/LineHeight/Brush/WhiteSpaceCollapse`）。
- **问题**：Parley 0.12 支持 `StyleProperty::FontFeatures` 和 `FontVariations`，但引擎没有暴露对应样式，应用无法请求 `tnum`（tabular numbers）、`zero`、`ss01`、`cvXX` 等特性，也无法按列使用 condensed/width 轴。
- **影响**：行情价格、涨跌幅、成交量等数字默认使用比例数字，数字内容变化时列宽抖动；表格无法保证小数点/个位对齐，和“高频数字表格”这一核心需求冲突。即使应用想绕过，也只能换字体或拆字符，无法正确解决。
- **成熟实现对照**：CSS `font-feature-settings` / `font-variant-numeric: tabular-nums`；Skia `SkShaper` 的 feature 列表；Flutter `TextStyle.fontFeatures`；Qt `QFont::setFeature`。
- **建议**：在 `TextStyle` 增加 `font_features: SmallVec<[FontFeature; 2]>`（或 CSS 字符串 + 解析缓存）和 `font_variations`，纳入 `hash_into` 与缓存键；`shape` 中 push `StyleProperty::FontFeatures` / `FontVariations`；为交易所数字列提供 `tabular-nums` 便捷常量；增加“1111 与 8888 advance 相等”的回归用例。
- **置信度**：高。

### P2-2 整 ppem + `hint(true)` + 无亚像素定位，Dpi 非整数时字距/字面不稳定

- **证据**：`crates/ui_core/src/text/shape.rs:244`（`font_size().round().clamp(1.0, 512.0)`）、`crates/ui_core/src/text/shape.rs:267-268`（`glyph.x.round()` / `glyph.y.round()`）；`crates/ui_core/src/text/raster.rs:94-97`（`ScalerBuilder::size(ppem).hint(true)`、`Format::Alpha`，无 `Render::offset`）。
- **问题**：Parley 的布局坐标/advance 是按物理 font size（可为小数）算的，但光栅使用四舍五入后的整数 ppem，且所有字形落点强制 round；`hint(true)` 又按整数 ppem 重排笔画。Parley 的 `quantize=true` 只量化块轴（设计文档 §2.3① 已确认 inline x 仍是分数），所以“布局 advance 是小数、字形位图是整数 ppem”并不一致。
- **影响**：125%/150% 缩放下，14px 文本按 17.5→18 ppem 光栅，汉字/数字内部字面与外部 advance 有偏差；长数字列、滚动行情和缩放动画会出现字距跳动/个别字形重叠。CJK 笔画在全 hinting 下还可能被横向挤压。设计已把 V-40 登记为“v1 接受”，但对交易终端的大字号数字和高频刷新应重新评估。
- **成熟实现对照**：Skia 在 UI 文本常用 `SkFontHinting::kSlight`/`kNone`，并在高 DPI 使用 subpixel positioning；DirectWrite 支持 subpixel origin；HarfBuzz 的 ppem 与定位分离；Flutter 只在低 DPI 做整像素 snap。
- **建议**：按字号选择 hinting（例如 ≤16ppem 才 hint），高 DPI 使用 fractional ppem 或保持字体内 advance 与布局一致；为 x 方向增加 1/3 或 1/4 子像素桶（`Render::offset`）或至少把 `glyph.x` 的量化策略与 Parley 的 `quantize` 对齐；加 1.25/1.5 缩放下长数字串的像素/advance 回归。
- **置信度**：中高（实现事实明确；视觉严重度依赖字体与 DPI，需实测）。

### P2-3 形状缓存字节预算按“run 数”而非“字形数”计费，8 MiB 上限会严重失真

- **证据**：`crates/ui_core/src/text/shape.rs:154-160`（`let glyphs = self.layout.lines().map(|line| line.items().count() * 32).sum();`）。
- **问题**：`line.items()` 是行内 item（通常一个 run 一项），不是字形；一段 10k 字符的单 run 文本只按 32 B 计入预算，而真实 `Layout` 至少持有数万字节到数百 KB 的字形/簇数据。`ShapeCache` 的 `BYTE_BUDGET = 8 * 1024 * 1024`（`crates/ui_core/src/text/cache.rs:68`）因此基本失效。
- **影响**：高文本量页面（多行日志、长说明、批量表格单元格）的形状缓存实际占用可能是预算的 10–100 倍；在长会话/多窗口切换中形成不可控内存峰值。设计 §11.1 本身要求 `size_of::<ShapedText>() + Σ(每个字形 16 B) + text.len()`，当前实现与设计不一致。
- **成熟实现对照**：Chrome/Blink 的 `ShapeResult` 按字形数组/簇数组计费；Flutter `ParagraphCache` 按实际 paragraph 对象估算；Qt `QTextLayout` 也按 glyph run 存储计费。
- **建议**：遍历 `line.items()` 中 `PositionedLayoutItem::GlyphRun` 的 `positioned_glyphs().count()`（或从 `ShapedText` 内部保存的 run 数/簇数）累加；为长单 run 文本增加预算测试，逐出后再检查 `bytes()` 与 RSS/分配量。
- **置信度**：高。

### P2-4 `TextPassReport` 的 raster/tofu/atlas 统计从未填充，`dirty_spans` 也落后一帧

- **证据**：`crates/ui_core/src/text.rs:498-515`（`rasterized`、`tiles_reused`、`tofu`、`atlas_rebuilds` 字段及文档）；`crates/ui_core/src/app.rs:2927-2939`（`rasterize` 返回的 `_stats` 被忽略，未写入任何 report）；`crates/ui_core/src/app.rs:2868`（`report.dirty_spans = pending_dirty.spans().len()` 在 `layout_text` 合并当前 `atlas.take_dirty_rows()` 之前）。
- **问题**：`refresh_text` 只填 `shaped/reused/skipped`；`rasterized/tiles_reused/tofu/atlas_rebuilds` 永远是默认 0。`dirty_spans` 读到的是上一轮遗留的 `pending_dirty`，本帧 `layout_text` 新增的脏行不会计入；`TextLayoutReport` 也没有 raster 统计。`GlyphAtlasStats` 的 `rasters/hits/rebuilds` 被 `take_stats()` 清零后直接丢弃。
- **影响**：帧记录、L1 断言、调试 HUD 和性能回归会把“光栅了 0 个字形/没有 atlas 重建”当真；tofu、图集重建、tile 复用率等关键可观测性全部失真。对高频文本产品，这会让“缓存是否有效、是否在反复重建图集”无法诊断。
- **成熟实现对照**：Chrome tracing 的 `SkStrikeCache` 命中/未命中计数；Flutter `FrameTiming`/`Paragraph` 统计；Skia `SkStrikeCache::getCacheStats` 都按真实调用记录。
- **建议**：在 `layout_text` 聚合 `GlyphAtlasStats` 到 `TextLayoutReport`（或新增 `TextRasterReport`）；`tofu` 从 `MissingGlyph` 诊断/`glyph.id == 0` 计数；`dirty_spans` 在 `layout_text` 合并后写回；删除或重命名不再适用的旧字段，避免“文档说会统计、实际永远 0”。
- **置信度**：高。

### P2-5 `refresh_text` 每帧 clone 全部 `TextContent` 并哈希全文，高频行情下是 O(节点数 × 文本长度)

- **证据**：`crates/ui_core/src/app.rs:2831-2843`（对 `tree.order()` 每个 text 节点 `tree.text_content(node).cloned()`，随后 `text.shape_cached(&content, scale)`），`crates/ui_core/src/text/context.rs:89-95`（每次 `ShapeKey::new` 都 `write(content.text.as_bytes())`）。
- **问题**：即使某个文本节点本帧没有变化，只要窗口需要出帧，`refresh_text` 仍会对所有文本节点克隆 `TextContent`（`SmolStr` 长文本会堆分配）、计算全文 hash，然后才可能命中缓存。行情 tick 时一个窗口有数百/数千个价格单元格，每帧都在做全量字符串读/哈希，而非只处理 dirty 节点。
- **影响**：高频刷新下 CPU 花在不变文本上；节点数量大时表现为掉帧、输入延迟和电量/风扇。设计 V-45 已登记“每帧遍历 tree.order()”但当前实现还额外做了 clone/hash，超出单纯遍历成本。
- **成熟实现对照**：Flutter `TextLayoutManager`/Chrome LayoutNG 只对 dirty text 节点重排；GPUI 文本系统用 element/text handle 复用；Qt 用 `QTextDocument` 的 layout dirty 标记。
- **建议**：给 `NodeTree` 文本槽增加 `TEXT`/`SHAPE` 脏位（P03 还有保留位），`refresh_text` 只遍历脏集合；至少先比较 `text_slot(node).key()` 与由轻量字段构造的候选 key，命中且 `Rc::ptr_eq` 时跳过 clone/全文 hash；长文本考虑 `Rc<str>`/借用而非每次 `SmolStr` clone。
- **置信度**：高。

### P2-6 A11y 实时数据没有 live region 与节流，每帧都可生成 `TreeUpdate` 并全树扫描

- **证据**：`crates/ui_core/src/app.rs:2476-2478`（文本内容变化 `tree.mark_a11y(node)`）；`crates/ui_core/src/a11y/adapter.rs:57-62`（`pending` 只看脏位/全量标志）、`crates/ui_core/src/a11y/adapter.rs:101-110`（每次 `take` 生成快照）；`crates/ui_core/src/a11y/adapter.rs:118-127`（增量仍扫描 `tree.order()` 找脏节点）；`crates/ui_core/src/a11y.rs:236-269` 的 `A11yNode` 没有 `live` 字段；`crates/ui_core/src/a11y/adapter.rs:241-291` 的 `to_accesskit` 从未 `set_live`。
- **问题**：高频行情下每个 tick 改价格文本就会标 A11y，平台泵只要 adapter 活跃就每帧调用 `build_update`；没有时间窗口合并、没有 `Live::Off/Polite/Assertive` 通道，也没有“只在焦点节点/显式状态区播报”的策略。增量收集还每次 O(N) 扫全树。屏幕阅读器/UIA 事件队列可能被大量 value 变化塞满，Narrator/VoiceOver 一直播报价格，用户无法听别的信息。
- **影响**：有辅助技术的用户无法使用实时行情页；AT 侧 CPU/事件积压，甚至拖慢主线程（Windows UIA 回调）。这是“实时数据 + 无障碍”必须专门处理的问题，而不是普通静态 UI 的默认行为。
- **成熟实现对照**：AccessKit 自身提供 `Live::{Off, Polite, Assertive}` 和 `set_live`；Chrome 对 AX events 做帧内合并并对不可见/非焦点节点抑制属性事件；Flutter semantics 每帧合并一次、支持 `SemanticsFlag.isLiveRegion`；ARIA `aria-live` 明确区分状态区与普通数据。
- **建议**：`A11yNode`/`A11yPatch` 增加 `live`，默认行情数据节点 `Off`，状态栏/告警用 `Polite/Assertive`；在 `A11yBridge` 增加最小间隔/帧合并（例如 volatile 文本 100–250ms 一次，或只在节点获得焦点时发 value 事件）；维护 dirty 节点集合，避免每次扫描 `tree.order()`；用 1k ticks/s 的 AT 场景做回归。
- **置信度**：中高（代码路径明确；AT 具体吞并行为需实机验证）。

### P2-7 A11y 表格/虚拟列表缺少行列位置和 Row/Cell 语义

- **证据**：`crates/ui_core/src/a11y.rs:51-78`（角色列表有 `Table`/`Grid`，但没有 `Row`/`Cell`/`ColumnHeader`）；`crates/ui_core/src/a11y/tree.rs:125-140`（`A11yNode` 没有 `pos_in_set`/`size_of_set`/行列索引）；`crates/ui_core/src/app.rs:1690-1699`（`virtual_row_index` 只作为 helper，注释说用于 a11y `pos_in_set`，实际未被 a11y 调用）；`crates/ui_core/src/interaction.rs:303`（`VirtualListState` 注释称供 `pos_in_set`/`set_size`）。
- **问题**：`role_by_kind` 对虚拟行只给 `ListBoxOption`，容器给 `List`，但 `to_accesskit` 不写 `set_pos_in_set`/`set_size_of_set`；没有 `Row`/`Cell` 角色，表格/网格里的文本单元格也无法形成行列关系。桌面读屏器只能读一串脱离上下文的 Label。
- **影响**：盘口、成交明细、自选列表、期权链等核心表格无法被屏幕阅读器按“第 3 行，共 200 行，买价列”导航；虚拟列表滚动后用户不知道当前位置。设计 V-80 将其记为低优先级，但对本产品领域应提升。
- **成熟实现对照**：AccessKit 的 `Node::set_pos_in_set`/`set_size_of_set` 与 `Role::Row`/`Role::Cell`；Flutter `Semantics` 的 `indexInParent`/`scrollIndex`；Qt `QAccessibleTableInterface`；Chrome AXTable 的行列索引。
- **建议**：A11yRole 增加 `Row`/`Cell`/`ColumnHeader`（或允许通过 `A11yOverride` 使用 AccessKit 原生 Role 的受控子集）；`A11yNode` 携带 `pos_in_set/size_of_set/row_index/column_index`，`virtual_row_index` 真正接入；补 100 行虚拟列表和 10×5 表格的 AT 快照断言。
- **置信度**：高。

### P2-8 A11y Slider/CheckBox/Switch/RadioButton 的默认动作和数值通道缺失

- **证据**：`crates/ui_core/src/a11y/mapping.rs:121-139`（`default_actions` 只覆盖 Button/TextInput/ScrollView/Dialog/Menu，`Slider`/`CheckBox`/`Switch`/`RadioButton` 没有默认动作）；`crates/ui_core/src/a11y/mapping.rs:97-105`（`Action::Increment`/`Decrement` 被映射为 `None`，只记 `UnhandledAction`）；`crates/ui_core/src/a11y/tree.rs:125-140`（只有 `checked/selected/expanded`，没有 numeric value/min/max）；`crates/ui_core/src/a11y/adapter.rs:266-280`（只 `set_value` 字符串，没有 `set_numeric_value`/`set_min_numeric_value`/`set_max_numeric_value`）。
- **问题**：`A11yRole::Slider`/`ProgressIndicator` 是公开角色，但 Slider 没有 `SetValue`/`Increment`/`Decrement`，数值控件也没有数值属性；CheckBox/Switch/RadioButton 若应用没有额外 `on_click`，默认动作表为空，AT 无法切换。`Increment`/`Decrement` 在 `action_from_accesskit` 被直接丢弃，连 handler 都无法接收。
- **影响**：屏幕阅读器用户无法用标准 RangeValue/Toggle 模式操作滑块、复选/开关/单选，输入控件实际不可无障碍使用；ProgressIndicator 仅能读字符串，读屏器不会按百分比播报。
- **成熟实现对照**：Windows UIA 的 `RangeValuePattern`（SetValue/Increment/Decrement）和 `TogglePattern`；AccessKit `Action::Increment/Decrement`、`Node::set_numeric_value`/`set_min_numeric_value`/`set_max_numeric_value`、`Toggled`；Qt accessible slider/checkbox interface。
- **建议**：`A11yAction` 增加 `Increment`/`Decrement`（并正确处理 `ActionData::NumericValue`），`default_actions` 给 Slider/CheckBox/Switch/RadioButton 合理默认（Click/SetValue）；`A11yNode` 增加 `numeric_value/min/max/step`，`to_accesskit` 写对应属性；补 UIA/AT 用例。
- **置信度**：高。

### P2-9 A11y 文本选区没有 `TextRun` / `character_lengths`，grapheme 索引与字节偏移混用

- **证据**：`crates/ui_core/src/a11y.rs:23-78`（角色子集无 `TextRun`；文本节点固定 `Role::Label`）；`crates/ui_core/src/a11y/adapter.rs:241-291`（`to_accesskit` 不设置 `character_lengths`/`character_positions`/`character_widths`）；`crates/ui_core/src/a11y/action.rs:104-111`（把 AccessKit `TextPosition.character_index` 原样放进 `A11yActionData::Selection`）；`crates/ui_core/src/text/inline_flow.rs:69-89`（`selection_rects`/`caret_rect` 接受的是 UTF-8 字节偏移）。
- **问题**：AccessKit 的 `TextPosition.character_index` 是 `Node::character_lengths` 的索引，按“可选中字符（grapheme cluster）”计数，不是 UTF-8 字节偏移；而引擎没有创建 `TextRun` 节点/字符度量，导致读屏器无法查询文本位置，`SetTextSelection` 的索引直接交给字节偏移 API 时，CJK/emoji/组合字符会错位。设计 F-106 选择 `Label + value` 本身没问题，但 `SetTextSelection` 仍被声明为支持动作。
- **影响**：文本输入框/多行文本的屏幕阅读器选区、光标和“从光标处朗读”不可用；应用 handler 若按文档把 `anchor/focus` 当字节用会截断多字节字符。对中文行情搜索框、备注输入是实际缺陷。
- **成熟实现对照**：AccessKit `Role::TextRun` + `character_lengths`/`character_positions`/`character_widths`；Chrome accessibility text runs；Flutter `TextInput` semantics 的文本选择和字符边界；Qt `QAccessibleTextInterface`。
- **建议**：要么明确 v1 不支持 `SetTextSelection` 并从 `default_actions` 移除；要么为每个文本节点/run 建 `TextRun` 子节点，写 grapheme→UTF-8 byte 的 `character_lengths` 和位置/宽度，并在 `A11yActionData::Selection` 中携带转换后的字节偏移（或提供 `CharacterIndex→ByteIndex` 的公共 API）；补中文/emoji 选区测试。
- **置信度**：高。

### P3-1 空文本节点的 inline box 被 `measure` / `rasterize` 直接丢弃

- **证据**：`crates/ui_core/src/text/shape.rs:191-193`（`if self.text.is_empty() { return Size::ZERO; }`）、`crates/ui_core/src/text/shape.rs:221-232`（`rasterize` 同样对空文本直接返回 `ShapedLine::empty`）；`crates/ui_core/src/text.rs:247-274` 和 `crates/ui_core/src/text/inline_flow.rs:18-44` 允许 `TextContent` 携带 `inline_boxes`。
- **问题**：`TextContent::new("", style).with_inline_boxes([...])` 是合法 API，Parley 也支持空文本 + inline box；但代码把“文本为空”当成“没有内容”，不 measure、不 rasterize、不产出 `ShapedInlineBox`。纯图标/徽标的行内内容会消失，且没有任何诊断。
- **影响**：Markdown/工具栏中的行内图标、`<kbd>` 图标、emoji 替代图在空 label 下不可见；布局尺寸为 0，可能造成整行高度塌陷。
- **成熟实现对照**：CSS inline replaced element 可以在空 inline 上下文中存在（如 `<img>`）；Flutter `WidgetSpan`、Qt `QTextImageFormat` 都能在无文字时贡献尺寸。
- **建议**：将空文本特例条件改为 `self.text.is_empty() && self.inline_boxes.is_empty()`；`rasterize` 对仅有 inline box 的 `Layout` 正常遍历，`measure` 从 `layout` 读取 inline box 尺寸；增加“空文本 + 一个 20×10 inline box”的回归。
- **置信度**：高。

## 做得好的地方

- **Parley 集成边界清晰**：`shape.rs` 是唯一 `RangedBuilder` 调用点，文本/样式/scale/字体栈版本变化才重建 `ShapedText`；宽度变化通过 clone + `break_all_lines` 重排，避免重复 shape。
- **形状缓存键较完整**：`ShapeKey` 覆盖文本哈希、样式（含 f32 位模式）、inline box 字段、scale、字体栈版本；命中再做文本/样式/inline box 全量校验，防哈希碰撞。
- **CJK/复杂脚本基础正确**：workspace 启用了 Parley `complex-scripts`（CJK 词典断行），默认字体栈显式列出 Latin → CJK → 其他候选；inline box 的非法字节偏移会吸附到前一个 char boundary 并记诊断，而不是 panic。
- **字形图集关键不变量落实**：`FontKey` 含 blob 地址/长度/TTC index/font_epoch 且通过持有 `FontData` clone pin 住 blob；空白字形有负缓存；`GlyphAtlas::clear` 与 `AtlasAllocator` 换代同步，上传侧用脏行/整页重建；`write_rows` 避免整页拷贝。
- **图片颜色流水线正确**：线性空间预乘 → sRGB 重编码的 64 KiB LUT，`alpha == 255/0` 快速路径逐字节稳定；`ResourceId` 带代际、`unload` 幂等、失败插入不落条目；`DecodeCause` 强类型分类。
- **A11y 基础机制扎实**：角色/动作映射穷举无 `_` 分支；文本 `Label` 走 `set_value` 的 AccessKit 约定已核对；缺少 `Canvas` 替代文本时 `hidden` + 诊断而非静默；焦点陈旧回退根、增量必带祖先链；`update_if_active` 懒激活，ActionHandler 只 `Dispatcher::push` 回主线程，避免跨线程直接改 UI。

## 存疑 / 需实测

- **CJK/双向文本测试偏表面**：`crates/ui_core/src/text/shape.rs:456-469` 的 CJK 用例只检查 `shaped.layout()`（`break_all_lines(None)` 的 max-content 布局），没有检查 `measure(Definite)` 后的实际换行结果；RTL/双向文本没有专门用例。建议用最终 `layout_at` 的行 `text_range` 与字形顺序做 golden。
- **彩色 emoji**：`raster.rs` 只使用 `Source::Outline`（设计 V-41 已登记），而字体栈没有 emoji 家族；`🌍` 在 COLR 字体上可能只出单色/空白，`glyph.id == 0` 之外也没有诊断。建议明确是否 v1 支持 color emoji，并用 `Segoe UI Emoji`/`Noto Color Emoji` 实测 `rasterize` 返回值。
- **ICC / EXIF 方向**：`resources/image.rs:5-8` 明确“资产必须已是 sRGB”，`image::DynamicImage` 路径没有调用 `ImageDecoder::orientation()`；Display P3 截图/相机 JPEG 会出现偏色或旋转。作为合同限制可接受，但应在应用侧 API 文档和资产校验里明确。
- **caret 亲和性**：`crates/ui_core/src/text/inline_flow.rs:92-100` 固定 `Affinity::Downstream`；软换行行尾、RTL 行首的 caret 可能画到下一行/错误一侧，建议增加 `Affinity` 参数或从上一光标位置推导，并用 `measure` 后的布局实测。
- **`TextContext.family_list` 缓存字段**：`crates/ui_core/src/text/context.rs:103-106` 构建了默认列表但随后每 shape miss 都重新 `family_list(...)`，字段实际只用作 `is_none` 布尔；属于 V-43 的“每次 clone Vec”问题，建议要么真正复用，要么删除字段。
- **`TextContext::set_font_stack` 同内容也 bump**：`crates/ui_core/src/text/context.rs:67-74` 无条件递增版本并清缓存；`FontStack::set_families` 已有“内容相同不 bump”语义（`text.rs:318-325`），主题重放同一栈时会造成不必要的全量重排。
- **`ShapedLine.first_baseline`**：`crates/ui_core/src/text/shape.rs:319-322` 从 `self.layout`（max-content 快照）取 first baseline，而不是传入的最终 `layout`；窄宽/行内盒场景可能不同，建议改为 `layout.first_baseline()` 并补测。

