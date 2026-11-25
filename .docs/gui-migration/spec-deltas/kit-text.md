# P2-3 规格差异：`base_ui::kit::markdown` / `base_ui::kit::input`

- 基：`gui-engine-migration@828bf53`（P2-1a 主题/StyleBuilder 已合入）；本批 worktree
  `.agents/worktrees/p2-3`、分支 `mig/p2-3`。P2-3 交付 `4b667e2`；**P2-3b 修复提交 `686d661` + `4b40f2e`**
  （两笔追加提交，未 amend/rebase）；**P2-3c 复核收尾提交见 §9**（同样为追加提交）。
- 规格：`crates/ui_core/design/custom-gui-engine-design.md` v2.2 §11.2（E2 富文本）、§11.5
  （E5 点→字符）、§4.5/§4.7（文本缓存与行内盒）、§8.3（IME/剪贴板）、§3.4（主题信号）、
  §4.9（D24 薄 StyleBuilder）、§6.10（测试等价性）。
- 评估依据：`.docs/gui-migration/assess/02-base-ui.md` §4/§5/§10.5（G1/G10/G13）、`D-03`
  （用户裁决：扩展 E2 富文本）、`D-06`（测试折算口径）。
- 本文只登记**实现与规格/旧实现的差异、裁定与遗留**，供 Lead 合并与 P1-DOC/后续批次回写；
  本批不改设计文档。

## 1. 交付面

| 路径 | 行数 | 职责 |
|---|---|---|
| `crates/base_ui/src/kit/markdown/document.rs` | ~440 | 只读 IR（块/行内片段/链接表/kbd/表格）+ `plain_text` |
| `crates/base_ui/src/kit/markdown/source.rs` | ~580 | `file:` 图片 URI、`type`/`size` 解析、固有尺寸、`MarkdownAssets` |
| `crates/base_ui/src/kit/markdown/parse.rs` | ~900 | comrak → IR（旧语义 + 新块类型；`<p align>`/`<kbd>` 状态机、空行、链接表） |
| `crates/base_ui/src/kit/markdown/inline_flow.rs` | ~665 | 行内片段 → `TextContent{runs, inline_boxes}` + 链接/kbd 字节区间 |
| `crates/base_ui/src/kit/markdown/view.rs` | ~1220（P2-3c 后） | IR → 节点树（文本/代码块/引用/列表/表格/分隔线/空行）+ 链接命中 |
| `crates/base_ui/src/kit/input.rs` | ~3750（P2-3c 后） | 编辑状态机 + 输入框节点树 + 指针/键盘/IME/剪贴板接线 + 水平滚动/undo/拖选/双击 |
| `crates/base_ui/src/kit/mod.rs`、`crates/base_ui/src/lib.rs` | 声明 | `kit` 模块树挂载 |
| `.docs/gui-migration/test-ledger/p2-3.md` | 新 | 174 条旧测试逐条处置（按批次独立文件；P2-3b 恢复，见 §7） |
| 本文件 | 新 | 差异与遗留 |

## 2. markdown → 引擎映射

### 2.1 数据形状

| 旧 IR | 新 IR | 理由 |
|---|---|---|
| `SharedString` | `SmolStr` | AGENTS §2.2；引擎 `TextContent.text` 本身就是 `SmolStr` |
| `Size<Pixels>`（图片尺寸） | `ui_core::math::Size`（f32 逻辑像素） | 引擎全部是逻辑像素；`Pixels` 是 gpui 类型 |
| `AssetSource`（gpui trait） | `MarkdownAssets { fn load(&self, path) -> Option<Cow<'static,[u8]>> }` | 解析期只需要"路径 → 字节"；生产实现直接复用 `crate::assets::Assets`（rust-embed）。**视图层的图片绘制**走引擎 `ResourceCache`（`ResourceImages`），与解析期解耦 |
| `MarkdownPiece::Text { strong, emphasis, link, kbd }` | 增加 `strike`、`code` | `~~x~~` 与 `` `x` `` 旧实现整棵丢弃；本批作为行内标记 |
| `MarkdownBlock::{Heading, Paragraph, BlankLine}` | 增加 `CodeBlock`、`Quote`、`List`、`ThematicBreak`、`Table` | 批任务要求"代码块/引用/列表/表格按节点树表达" |
| comrak 默认零扩展 | `extension.table` + `extension.strikethrough` | 表格与删除线需要 GFM 扩展（comrak 0.59：`Options { extension: Extension { table, strikethrough, .. } }`） |

> **语义扩展登记**：旧 parse 的 3 条"整棵丢弃"测试改写为"按新 IR 保留"
> （`dropped_block_nodes_leave_no_block_behind` → `formerly_dropped_block_nodes_are_now_drawn`、
> `block_constructs_inside_an_align_tag_pair_are_dropped` → `..._are_drawn`、
> `strikethrough_is_plain_text_without_the_extension` → `strikethrough_is_a_piece_mark`）。
> 被丢弃的类别仍然丢弃：`HtmlBlock`（除 `<p align>` 标签对外）、`Raw`、`Footnote*`、`Math`、
> `FrontMatter`、`DescriptionList*`、`MultilineBlockQuote`、`WikiLink`、`Superscript`/
> `Highlight`/`Insert`、`Escaped`。空行不折叠、`<p align>` 标签对、`<kbd>` 配对、深嵌套护栏
> （`MAX_INLINE_NESTING=64`）的旧语义逐条保留（t1 测试同名迁移）。

### 2.2 行内样式的引擎表达

| markdown 标记 | 引擎表达 | 备注 |
|---|---|---|
| strong | `TextRun.style.weight = 700` | 标题整块 bold（基样式 weight 700） |
| emphasis | `TextStyle.slant = Italic` | 引擎无 `with_italic` 构造器，字段直写 |
| link | run 颜色 = `Role::BrandBase` + `TextDecoration::underline(1.0)`（线色同链接色） | **常驻下划线** |
| strikethrough | `TextDecoration::strikethrough(1.0)` | 与链接互斥时链接优先（CommonMark 不允许嵌套链接） |
| inline code | `TextStyle.with_family(MONO_FAMILY)` + `Role::FgDefault` | 引擎 `TextStyle` 无背景通道，因此没有"代码底色" |
| `<kbd>` | 等宽 + 下划线（`modules` 内的 kbd 区间另外记录在 `Flow::kbd_spans`） | **降级**，见 §4.1 |
| 行内图片 | `InlineBoxSpec { index = 插入点字节, size = 解析期尺寸, align = Middle, paint = Image/Quad }` | 图片不占文本字节；`paint` 由视图层的 `ImagePaintSource` 决定 |
| 硬换行 | 文本里的 `'\n'` | parley 断行；行尾装饰裁剪由引擎 E2b 规则负责 |
| 段落对齐 | `TextStyle.alignment`（`TextAlign::{Left,Center,Right}`） | 表格单元格对齐同通道 |
| 标题字号 | 视图层把 `body_size × HeadingLevel::scale()` 传给 `FlowStyle` | 旧倍率 1.75/1.5/1.3/1.15/1.05/1.0 原样保留 |

链接命中：`MarkdownHandles::link_at(window, point)` = 节点几何 → 点转文本节点局部坐标 →
引擎 E5 `Window::text_byte_index_at`（簇边界由 parley 决定）→ `Flow::link_spans` 字节区间。
**不新增引擎 API**（与 E5 spec-delta §1 的应用层建议一致）。

### 2.3 块 → 节点

| 块 | 节点结构 | 备注 |
|---|---|---|
| Heading / Paragraph | 每块一个 wrapper（`flex_col` + `width_full` + `margin_top`）+ 一个文本节点（`runs` + `inline_boxes`） | 块间距（旧 `MarkdownState::render` 规则）：**非空行相邻**的非首块 `+8px`；非首块标题再 `+8px`（合计 16）；**空行自己就是间距**，两侧不再叠 8px ⇒ 空行后段落 +0、标题 +8（P2-3c 复核修正） |
| BlankLine | wrapper + 一个 `height = body_size × 1.4` 的空盒子 | 与旧 `div().h(line_height)` 等价 |
| CodeBlock | `flex_col` 容器（背景 `SurfaceSunken`、圆角 4、padding 8）+ 等宽文本节点 | 语言标记暂不参与绘制（无高亮） |
| Quote | `flex_row`：左侧 3px 竖线（`LineBorder`）+ 内容列（递归块） | 旧实现整块丢弃 |
| List | `flex_col`：每项 `flex_row`（标记列宽 20 + 内容列递归块）；有序列表号 = `start + index` | 旧实现整块丢弃 |
| ThematicBreak | `height: 1px` + 背景 `LineDivider` | 旧实现整块丢弃 |
| Table | `flex_col` 行 × `flex_row` 单元格（`flex_1`、padding 4、按列对齐的文本节点），行间 1px 分隔线 | **不依赖 P2-2 的 `table` 组件**；无列宽测量/换行行高优化 |
| 文档根 | `MARKDOWN`（`flex_col` + `width_full` + `min_width_0`） | `Probe::Selector::Id("MARKDOWN")` 可直接命中 |

## 3. input 状态机 API 与 IME 语义

### 3.1 API（实现事实，与任务书签名等价）

```rust
pub struct InputSpec { name, placeholder, masked, multiline, readonly, disabled,
                       max_length: Option<usize>（char 计）, size: ControlSize,
                       submit: Option<Action> }         // 逐字段公开；name 同时是节点/动作前缀与 a11y 兜底标签
pub enum InputError { Empty, TooLong { max }, Invalid { i18n_key } }   // i18n_key() → "input.error.empty" / "input.error.tooLong" / 应用 key
pub enum CaretMotion { CharLeft, CharRight, Home, End, DocStart, DocEnd }  // Home/End 是行级（单行 = 文档首尾）
pub struct InputState { /* 私有字段 */ }        // Clone + PartialEq（Signal 要求）；含 100 步 undo/redo 快照
pub fn build(tree, parent, spec: Arc<InputSpec>, theme: &Theme, state: &Signal<InputState>,
             placeholder_text: &Signal<SmolStr>) -> InputNode
                                                 // placeholder_text = 占位符文本信号（update_placeholder 的写入口）
pub struct InputNode { root, viewport: Option<NodeId>, text, placeholder, caret,
                       state: Signal<InputState>, placeholder_state: Signal<SmolStr>, spec }
                                                 // viewport = 单行横向滚动视口（多行 = None）
impl InputNode {
    pub fn install(&self, app: &mut App);                    // 点击/拖选 Action 处理器 + 单行滚动容器登记（引擎 on_click/on_drag）
    pub fn sync(&self, app: &mut App) -> bool;               // 每轮同步：焦点镜像 + IME + reveal + caret/选区 Canvas
    pub fn dispatch(&self, app: &mut App, event: &Event) -> bool;  // 外壳 drain_events 后逐条投递
    pub fn update_value(&self, app: &mut App, value: &str) -> bool;      // 程序化写值（旧 set_value 契约；§4.2.13）
    pub fn update_placeholder(&self, app: &mut App, placeholder: impl Into<SmolStr>) -> bool;
}
```

`InputState` 的编辑 API（纯逻辑，除剪贴板外不需要 `App`）：`insert / delete_backward /
delete_forward / insert_newline / select_all / set_caret / move_caret / select_word / select_line /
begin_drag / drag_to / end_drag / ime_preedit / ime_commit / ime_cancel / copy / cut / paste /
undo / redo / validate / set_error / set_focused / set_caret_from_point / set_text / handle_key /
handle_ime / is_dragging`，加只读访问 `text / display_text(masked) / caret / selection /
selection_anchor / is_composing / is_focused / error / can_undo / can_redo`。坐标：`caret`/
`display_text` 用**未脱敏显示文本**的 UTF-8 字节（组合串内联，可直接喂引擎几何 API）；
`selection` 用**提交文本**的字节；组合期选区恒空、`caret` 落在组合串内（由 `preedit.caret`
的 UTF-16 码元换算）。

事件接线（引擎事件模型的边界）：
- 引擎只有一个 `App::on_frame`，且 `App::drain_events()` 是消费式队列 ⇒ **本模块不装全局钩子**；
  应用外壳负责每帧 `sync()` + 把 `drain_events()` 的结果逐条 `dispatch()`。这是不抢全局钩子的
  唯一接线方式（旧 `Entity`+`EventEmitter` 无对应物，G2）。
- 点击定位走引擎 Action（`Action::Custom`，key = `base_ui.input.{name}.click`，**`name` 在同一
  App 内必须唯一**）；处理器从 `ActionCtx::hit()` 的 border-box 局部点换算到文本节点局部坐标 →
  E5 → caret，并 `App::set_focus`；`disabled` 时不注册。
- 焦点：根节点 `tab_stop(true)` + `on_click(Action)`（引擎 `focusable = visible && interactive &&
  tab_stop && (accepts_action || explicit interactive)`），a11y 角色 `TextInput` /
  `MultilineTextInput`（`disabled` 透传）。
- 渲染：文本节点 `bind_text`（已提交 + 内联组合串；掩码逐字符 `•`）+ `bind_style`（禁用/错误
  边框色 `StatusDangerBorder`/焦点色）；占位符是**独立文本节点**（`opacity` 由空值切换）；
  caret/选区是 `NodeKind::Canvas` 子节点，`sync` 用 `Window::set_canvas` 按引擎几何
  （`Window::text_caret_rect` / `NodeTree::text_selection_rects`）重填。
- 键盘（`handle_key`）：`Ctrl/Cmd + A/C/X/V`（剪贴板走 `App::set_clipboard_text` /
  `clipboard_text`）、`Backspace`/`Delete`、`←/→`（可 Shift 扩选）、`Home/End`（行级）、
  `Ctrl+Home/Ctrl+End`（文档首尾）、`Enter`（多行插换行；单行派发 `spec.submit`）、`Escape`
  （先取消组合，再收缩选区）。`↑/↓` 不消费（见 §4.2）。
- `max_length` 按 `char` 计、插入/粘贴裁剪不拆 UTF-8；掩码输入禁止复制/剪切；只读允许选择/复制；
  禁用不可聚焦、不接收编辑、不登记 IME。

### 3.2 IME 语义（与 §8.3 / E5 的契约）

- `ImeEvent::{Preedit, Commit}` 的 `caret` / `clauses` / `replace` 都是 **UTF-16 码元**，
  而引擎其余文本面（`TextRun.range`、`InlineBoxSpec.index`、`text_byte_index_at`、
  `App::set_ime_cursor_area` 的 `byte_index`）都是 **UTF-8 字节**。本模块自带换算
  （`utf16_len` / `utf8_from_utf16_offset` / `utf8_from_utf16_units`）：按
  `char::len_utf16()` 累加，落在代理对内部时**向下吸附**到该 `char` 的起点；`clauses` 不参与
  渲染（引擎没有 IME 子句高亮通道，登记遗留）。
- 组合串渲染：**内联**在组合起点（`display_text` = 前缀 + preedit + 后缀），不弹候选窗；
  caret 用"未脱敏显示文本"的字节喂 `Window::text_caret_rect`，用 `display_caret_byte` 换算
  （掩码时按字符数映射）后喂 `App::set_ime_cursor_area(window, text_node, byte)`。
- `Preedit`：整串替换当前组合串（`replace` 区间先在组合串内按 UTF-16 折算成新的完整串）；
  没有进行中的组合时从 caret 处开始（先删掉选区，再记 `start`）。
- `Commit`：`text` 替换整个当前组合串；带 `replace` 时同样先折算成完整串再提交；提交前按
  `max_length` 裁剪。没有组合时等价于一次 [`InputState::insert`]。有组合串的提交之后引擎会补
  一条 `End`（本模块 `End` 幂等：已无组合串时什么都不做）。
- `End`（未收到 `Commit`）= **取消**组合串（丢弃 preedit，caret 回到组合起点）；失焦
  （`set_focused(false)`）同样丢弃未提交的组合。
- 组合期**不**把 `InputEvent::Text` 当正文插入（避免与 IME 双重输入，见 `dispatch`）；
  `Backspace`/`Delete` 在组合期先取消组合（不再向组合串内部退格——平台输入法自己处理组合串编辑）。
- 焦点变化：`sync` 检测 `Window::focus().is_focused(root)` 并镜像到 `InputState.focused`；
  失焦时 `set_ime_enabled(false)` + `clear_ime_cursor_area`，只读/禁用不登记 IME 区域。

## 4. 与旧实现的差异 / 遗留（需 Lead 裁决或后续批次）

### 4.1 markdown

1. **`<kbd>` 带内容的行内框不可表达**（E2 遗留 G10-3）：`InlineBoxSpec` 只画盒、不承载文字，
   `InlineBoxPaint::Quad` 也无法包住相邻 run。本批降级为等宽 + 下划线，kbd 区间保留在
   `Flow::kbd_spans` 里。若产品需要真 kbd 框，需要引擎"带内容的行内盒"（E2b 候选）。
2. **链接 hover 下划线不可表达**：`runs` 是静态数据，`bind_style` 只能改整节点前景色（连装饰
   线一起改），无法只给一条链接加/去线。本批链接**常驻下划线**，hover 反馈留给后续（应用层可
   用 `Signal<Option<LinkId>>` + 整块重挂载实现，代价是整段 reshape）。
3. **行内图片尺寸**：解析期尺寸规则完整移植（`size=` clamp 1..256、缺省读文件头固有尺寸、svg
   无固有尺寸 → 16）。**绘制**：`ResourceImages` 把字节喂 `ResourceCache::insert` 得
   `InlineBoxPaint::Image`；`svg` 需要构建期预栅格（P2-1b 资产批次），本批 svg 不画（返回
   `None`，引擎只算落点）；默认 `PlaceholderImages` 画占位 Quad（资源通路未接线时的兜底）。
4. **表格**：简单行列 flex，无列宽测量/列对齐线/单元格合并；行高由内容自然撑开。GFM 表格扩展
   之外的表格语法不识别。
5. **代码块**：等高亮、无水平滚动（长行按 `Wrap` 折行）、无语言标签渲染。
6. **emoji / 字素簇**：渲染完全交给 parley（引擎负责簇边界）；但**键盘**左右移动/退格按 char
   边界（不拆 UTF-8、ZWJ 簇会逐码点删），因为引擎未公开 grapheme 迭代 API。建议后续向引擎
   申请 `text_byte_index_at` 之外的簇步进 API（E2b 候选），本批按 D-06 折算并登记。
7. **`MarkdownDocument` 无状态实体**：旧 `MarkdownState`（source/document/hover/pressed）删除；
   调用方持 `Arc<MarkdownDocument>`，切换文档 = 重新 `MarkdownView::new` + 重挂载（旧
   `set_source` 的"同源不重解析"优化由调用方在重建前比较 `source` 实现；本批不提供缓存）。
8. **i18n 集成测试移交 P2-4**（alert 批次）：`view_tests` 里 2 条 alert/placeholder 消息测试
   依赖 `i18n` crate 与 alert 组件，不在本批写作用域。
9. **块容器递归上限（P2-3b，High）**：`MAX_BLOCK_NESTING = 64`。引用/列表的嵌套超过 64 层时，
   该容器子树**不再递归**，其可见文字（`Text`/`Code`/软硬换行）拍平成一个普通段落；容器本身
   保留一层。comrak 仍会解析整棵 AST（迭代器遍历，不递归），但 IR 深度、view 与 `plain_text`
   的递归都因此有界。1000 层 `>`（约 2 KB）从 `STATUS_STACK_OVERFLOW` abort 变为可见降级文字。
10. **空行/标题间距（P2-3b 修 M4，P2-3c 复核纠正过度校正）**：规则与旧 `MarkdownState::render` 一致——`gap = (非首块 && 上一块不是空行) ? BLOCK_GAP : 0`，
   非首块标题再 `+= HEADING_SPACE_ABOVE`（与空行无关）。即空行后**段落 0 / 标题 8**；无空行的
   相邻块**段落 8 / 标题 16**。P2-3b 曾删掉 `previous_blank` 抑制（空行后 8/16，系统性多 8px），
   P2-3c 已恢复。旧测试
   `a_heading_after_the_first_block_should_have_extra_space_above_it` 的等价断言由
   `a_heading_after_a_blank_line_keeps_the_extra_space_above_it` 覆盖（台账 §10/§12）。
11. **图片链接命中（P2-3b 登记，Medium）**：**不扩引擎**。行内图片不占文本字节，纯图片链接的
   `Flow::link_spans` 为空，`MarkdownHandles::link_at` 只能字节命中，因此图片链接不可点/不可
   hover。根因是 E2b「节点文本路径的 `InlineBox` rect 只读几何面」缺失；源测试
   `an_image_inside_a_link_should_be_hoverable_and_clickable` 在台账中登记为**删除/已接受差异**
   （不是等价覆盖）。后续若产品要求，应在引擎 E2b 增加行内盒命中查询，再由应用层接线。
12. **链接命中半开区间（L1）**：`link_at` 的 `span.range` 是 `[start, end)`，E5 点在最后一个字符
   右半边返回 `byte == span.end` ⇒ `None`。登记为 E5 边界语义，应用侧不按 caret 矩形细化。
13. **默认排版（L2）**：markdown 默认字号/行高固定为 `tokens.medium`（14 px / 1.4），不再继承
   祖先 `window.text_style()`（旧 gpui 16 px / 1.618）。`with_body_size` 已按 AGENTS 删除；
   调用方要别的字号请显式构造 `MarkdownStyle`（P2-5 接线时按产品裁决）。
14. **a11y（L3）**：标题未声明 `A11yRole::Heading` + level，链接/图片 alt 无额外语义；文本节点
   默认 `Label` 可读。登记，后续批次。
15. **任务列表（L10）**：`TaskItem` 被过滤、按普通 bullet 渲染，勾选状态丢失（旧实现整块丢弃，
   属部分新增能力）；登记。

### 4.2 input

1. **hover 态**：引擎无 per-node 可读 hover 信号（G3），输入框只表达 normal/focused/disabled/
   error 四态；旧 `.when(hovered)` 边框变体未迁移。
2. **`↑/↓` 视觉行移动**：引擎只公开"点 → 字节"（E5），不公开视觉行/`ShapedLine`（`ShapedLine`
   在 crate 内），因此 `↑/↓` 不消费（不伪造"段首/段尾"语义）；`←/→`/`Home`/`End`/
   `Ctrl+Home`/`Ctrl+End` 可用。**旧 gpui-base 在单行输入上本来就是 no-op**（`is_single_line`
   早退），当前产品无回归；多行批次需要引擎视觉行 API 再补。
3. **undo/redo**：P2-3b 已实现（用户裁决）：快照式（正文/选区）、有界历史 100 步、新编辑清空
   重做栈；Ctrl/Cmd+Z 撤销、Ctrl/Cmd+Shift+Z / Ctrl/Cmd+Y 重做。组合开始时的选区删除与随后的
   提交合并为**一步**；组合过程中的 preedit 不进入历史（未提交内容本来就不在正文里）。
4. **按住拖选**：P2-3b 已实现（用户裁决）：节点 `on_drag` 由引擎在移动越过阈值后取指针捕获，
   handler 用 `App::drag` 的 `origin/current` 窗口坐标换算到文本内容坐标；`Start` 把锚点放在
   按下点，`Move/End` 扩展选区，`Cancel` 结束；移出窗口由 `current` 坐标继续驱动，E5 自动
   clamp 到文本边界。
5. **簇边界**：同 §4.1.6（键盘/退格按 `char`，指针定位按 parley 簇；组合附加符号/ZWJ 会被拆成
   多个 `char`，但永不切断 UTF-8）。
6. **双击选词 / 三击选行**：P2-3b 已实现（用户裁决）。规则：字符分**词字符**（
   `char::is_alphanumeric()` 或 `_`，CJK 连续段属于这一类）、**空白**、**其它**三类；双击选中
   包含光标的同类极大连续段（光标在末尾时取前一个字符）；三击选中当前行（不含行尾 `\n`；
   单行输入 = 全文）。点击路径使用引擎 `ActionSource::Click { click_count }`。
7. **单行输入清洗**：P2-3b 起，`insert` / 粘贴 / IME `Commit` / 程序化 `set_text` 在
   `multiline == false` 时丢弃 `\r` / `\n` / `\t`；多行输入原样保留。旧实现只删 `\n`。
8. **自动重复**：P2-3b 起 `repeat == true` 的 `Key(Down)` 交给编辑/导航键处理（退格、删除、
   ←/→、Home/End），`Enter`/`NumpadEnter` 与命令快捷键（Ctrl/Cmd+…）只在首按处理；
   组合期仍只处理 `Escape`。
9. **单行水平滚动**：`InputNode` 为单行输入多挂一个 `{name}.viewport` 裁剪盒，`install` 把它
   登记成 `ScrollAxis::Horizontal` 滚动容器；`sync` 用文本自然宽 + caret 几何做 reveal
   （caret 在左边界外时回滚、越过右边界时前滚）。**caret/选区画布用屏幕绝对坐标**：
   `Window::text_caret_rect` 已含祖先滚动偏移，选区矩形补「窗口绝对 − 局部」原点差即得屏幕坐标；
   **不再减一次滚动偏移**（P2-3b 曾重复减，P2-3c 修正，见 §9）。点击/拖选用
   `HitResult.local_point`（hit test 已做滚动换算）→ E5，不需要额外偏移。
   文本未 shape 的那一帧不动滚动（下一轮重试，与 caret 画布同约定）。
10. **`sync`/`dispatch` 由外壳驱动**：见 §3.1。同窗多输入框的 IME 开关是**窗口级**状态
    （`App::set_ime_enabled`），本模块在"本输入框失焦"时关闭、"聚焦"时打开 ⇒ 外壳应只同步聚焦
    控件，或按"先失焦者、后得焦者"的顺序调用；登记为引擎缺口（per-node IME 无 API）。
11. **密码框平台 IME 语义**（Linux `set_ime_content_type(Password)`）属平台批次，未接线。
12. **图标/清空按钮/密码可见性切换**依赖"图标与资源"批次（P2-1b），本模块不引入。
13. **运行时更新 API（P2-3b 补，P2-3c 按旧契约裁定语义）**：
    `InputNode::update_value(app, &str)` = 旧 `set_value` 的**程序化契约**：单行清洗、`max_length`
    按字符裁剪并写 `TooLong` 错误、取消组合、光标/选区落在末尾、**绕过 `readonly` / `disabled`**
    （只读/禁用只拒绝用户编辑）、**清空撤销/重做历史**（程序化写值不可撤销）。
    `InputState::set_text` 则是**用户级**入口：同样的清洗/裁剪/光标语义，但 `readonly`/`disabled`
    返回 `false`，且这次替换进入撤销历史（一步）。两者差异的测试：
    `update_value_bypasses_readonly_and_disabled` / `update_value_clears_the_undo_history`。
    `InputNode::update_placeholder(app, SmolStr)` 写占位符信号并重绘。`InputSpec` 仍是挂载期常量
    （运行时改尺寸/掩码 = 重新挂载）。
14. **a11y（P2-3b 部分补齐）**：根节点 `A11yPatch` 已带 `read_only` / `invalid` / `placeholder`；
    标题/链接语义与错误文本节点（`error_message`）仍属后续批次。
15. **单行多行共用一条状态机**：多行 `Enter` 插换行、`multiline` 不剥换行/制表符、文本按宽度
    折行；单行 `Enter` 派发 `submit`。两者都没有视觉行级方向键（见第 2 条）。
16. **Info：undo 历史的内存/克隆成本**（P2-3c 登记）：`InputState` 内联 100 步 `EditSnapshot`
    （每步一份 `String` 正文），而 `Signal<InputState>` 要求 `Clone + PartialEq` ⇒ 每次
    `dispatch` 都克隆整份历史（`dispatch` 里 `before` / `next` 各一份）。单行短值可接受；
    多行长文本接线前应评估改为 `Arc`/共享快照（或惰性克隆），否则 O(历史×正文) 的拷贝进热路径。
17. **Info：readonly 与程序化 API 的统一口径**（P2-3c）：`readonly` / `disabled` 是**用户编辑**
    限制；`update_value`、公开的 `InputState::undo()` / `redo()` / `set_text`（调用方传自己的
    spec）都属程序化状态 API，不因只读而失效——与旧 `with_edits_allowed` 契约一致。因此应用代码
    对只读字段调用 `undo()` 仍可能改正文（`update_value` 已清空历史，常规程序化路径不会留下
    可撤销步）。当前生产没有 readonly 输入；P2-5 接线前决定是否给用户路径（`handle_key` 的
    Ctrl/Cmd+Z）加只读拦截。

## 5. 门禁与证据

head = `4b667e2`（`mig/p2-3`，工作树 `.agents/worktrees/p2-3`），全部实跑：

| # | 门禁 | 结果 | 证据 |
|---|---|---|---|
| 1 | `cargo +nightly fmt -p base_ui -- --check` | PASS | `.review/mig-gates-1795228743-win.log` |
| 2 | `cargo +nightly clippy -p base_ui --all-targets -- -D warnings` | PASS | 同上（7.4s） |
| 3 | `cargo test -p base_ui --lib`（`KYTRIX_UI_CORE_MANIFEST=0`） | PASS **626**（另有 `cargo test -p base_ui` = **627 + doctests ok**） | `.review/mig-p2-3-gates-win.log`、`.review/mig-gates-1795228743-win.log` |
| 4 | `cargo check -p base_ui --all-targets` | PASS | `.review/mig-p2-3-gates-win.log` |
| 5 | `cargo xtask gates --platform all --step quick --scope base_ui`（head=4b667e2） | PASS（mac check 11.0s / linux check 8.0s） | `.review/mig-gates-1795228768-all.log` |
| 5b | `cargo xtask gates --platform win --step fast --scope base_ui`（head=4b667e2） | PASS（fmt/clippy/test 3 命令） | `.review/mig-gates-1795228743-win.log` |
| 6 | 变异自检（见 §6，全部红并逐字节还原） | PASS | `.review/mig-p2-3-mut-markdown.log`、`.review/mig-p2-3-mut-input.log` |

> 环境注记：Windows 首次两次 `--step fast` 因**共享 `CARGO_TARGET_DIR` 上并行批次同时重建
> `ui_core`**，doctest 阶段引用的 `libui_core-*.rlib` 在 rustdoc 运行前被替换 ⇒
> `extern location ... does not exist`（环境竞态，不是代码失败）。并行 cargo 空闲后重跑即 PASS；
> 另注意不要在 shell 里全局设 `KYTRIX_UI_CORE_MANIFEST=0` 再跑 xtask（会打乱 per-command 注入的
> 指纹），由 xtask 自己注入。
>
> P2-3b 的门禁在 §8（head `4b40f2e`）；P2-3c 收尾后的门禁在 §9.4（head `4dc8f65`）。

## 6. 测试与变异

- 旧测试折算：`test-ledger.md` §P2-3（174 条：迁移 107 / 重写 49 / 删除 16 / 移交 2；
  删除/重写理由逐条）。
- 本批新增：`kit::markdown` **163**（parse_tests 108 + source 20 + inline_flow 16 + view 14 +
  document 5）+ `kit::input` **39** = **202**；base_ui 全量 **627**（迁移前 425）。
- 变异自检（全部实跑；还原后按 SHA256 逐字节校验 `identical=True`）：
  1. **删除链接装饰**（`inline_flow::FlowBuilder::run_style` 的 `mark.link.is_some()` 分支改成
     `if false`）→ `kit::markdown::inline_flow::tests::links_carry_color_underline_and_byte_spans`
     红（15 passed / 1 failed）；
  2. **删除链接区间判定**（`MarkdownHandles::link_at` 的区间 `find` 恒 false）→
     `kit::markdown::view::tests::links_carry_underlines_and_are_hit_by_byte_index` 红
     （13 passed / 1 failed）；
  3. **IME UTF-16 换算错位**（`utf8_from_utf16_units` 的 `remaining -= ch.len_utf16()` 改成
     `remaining -= 1`）→ `kit::input::tests::{ime_caret_utf16_is_relative_to_the_preedit_not_the_document,
     utf16_helpers_count_surrogate_pairs_and_snap}` 红（36 / 2 failed）；
  4. **选区删除只删一半**（`delete_selection` 的 `start..end` 改成 `start..start+(end-start)/2`）→
     `kit::input::tests::ime_start_clears_the_selection` 红（37 / 1 failed）。

## 7. P2-3b（审查修复 + 用户裁决功能）补记

- 基：`4b667e2` 之后的追加提交（不 amend/rebase），工作树 `.agents/worktrees/p2-3`。
- 输入：`.review/mig-p2-3-review.md`（H1/M1–M7、L1–L10、I1–I3）+ 用户裁决（undo/redo、拖选、
  双击选词）。
- 处置摘要：
  - **H1**：块递归上限 64 + 超限拍平 + 5 条测试（深层引用/列表、边界 64、越界 65、兄弟保留）。
  - **M1**：单行横向滚动容器 + caret reveal + 点击换算 + 2 条测试。
  - **M2**：repeat 编辑/导航键 + Enter/命令只首按 + 2 条测试。
  - **M3**：单行 `\r`/`\n`/`\t` 清洗（insert/paste/IME/set_text）+ 2 条测试。
  - **M4**：空行后标题上间距 + 1 条测试；台账 3 条「重写不实」改判为删除/登记。
  - **M5**：登记 E2b 缺口（§4.1.11），不扩引擎。
  - **M6**：台账改为按批次独立文件 `.docs/gui-migration/test-ledger/p2-3.md`（174 条逐条重建；
    汇总文件由 Lead 合并）。
  - **M7**：`update_value` / `update_placeholder` + 3 条测试（值变更→信号失效/重绘；超长非法值
    → `TooLong`；readonly/disabled 拒绝）。
  - **用户裁决**：undo/redo（4 条测试）、拖选（4 条）、双击选词/三击选行（3 条）。
  - **L5/L6**：Esc 收缩选区（1 条）、IME `End` 派发级（1 条，变异复跑为红）。
  - **L8/L9**：`format!` → `format_smolstr!`/`SmolStr::new_static`；删除 `FlowStyle::base_style`、
    `MarkdownStyle::with_body_size`、view 的重复 `PlaceholderImages`。
  - **L4**：input a11y 补 `read_only`/`invalid`/`placeholder`。
  - **L1/L2/L3/L10/I1/I2/I3**：登记（见 §4.1/§4.2 与台账 §9/§10）。
- 测试与门禁结果见 §8（P2-3b 实跑）。

## 8. P2-3b 验证记录（实跑）

环境：`CARGO_TARGET_DIR=E:\kytrix\target`、`CARGO_BUILD_JOBS=6`；Windows 门禁在
`.agents/worktrees/p2-3` 跑；xtask 的 `--platform all` 走远端同步（见注记）。日志在
`.review/mig-p2-3b-*.log`。

| # | 门禁 | 结果 | 证据 |
|---|---|---|---|
| 1 | `cargo +nightly fmt -p base_ui -- --check` | **PASS**（exit 0） | `.review/mig-p2-3b-fmt.log` |
| 2 | `cargo +nightly clippy -p base_ui --all-targets -- -D warnings` | **PASS**（exit 0） | `.review/mig-p2-3b-clippy.log` |
| 3 | `cargo test -p base_ui --lib` | **PASS 655/655** | `.review/mig-p2-3b-test.log` |
| 4 | `cargo check -p base_ui --all-targets` | **PASS** | `.review/mig-p2-3b-check.log` |
| 5 | `cargo xtask gates --platform all --step quick --scope base_ui` | **PASS**（linux check 9.0 s；见注记） | `.review/mig-p2-3b-xtask.log` |

测试细分：`kit::markdown` **169** = parse_tests 113 + source 20 + inline_flow 16 + view 15 +
document 5；`kit::input` **61** = 原 39 + P2-3b 22。base_ui 全量从 627 → **655**。

变异自检（逐项实跑；每项还原后按 SHA256 逐字节校验 `RESTORE_IDENTICAL=True`）：

| 编号 | 变异 | 结果 | 证据 |
|---|---|---|---|
| H1 | 把 `MAX_BLOCK_NESTING` 判断改成 `if false`（去掉块递归上限） | **RED**：`deeply_nested_quotes...` 进程 `STATUS_STACK_OVERFLOW`（0xC00000FD） | `.review/mig-p2-3b-mut-h1.log` |
| M1 | reveal 目标强制 `next = 0.0`（去掉水平滚动） | **RED**：`long_single_line_scrolls_to_keep_the_caret_visible`（offset 恒 0） | `.review/mig-p2-3b-mut-m1.log` |
| M1-render | caret 画布的 `shift` 强制 `Point::ZERO`（不减去滚动偏移） | **RED**：同一测试的「caret 画布 x = 布局坐标 - 滚动偏移」断言（quad 793 vs 期望 154.4） | 同上（append） |
| M2 | `dispatch` 对 `repeat == true` 直接返回 `false`（恢复旧丢弃行为） | **RED**：`repeated_editing_and_navigation_keys_are_not_discarded`（文本仍是 `abc`） | `.review/mig-p2-3b-mut-m2.log` |
| undo-clear | `finish_edit` 不 `redo.clear()` | **RED**：`a_new_edit_clears_the_redo_stack` | `.review/mig-p2-3b-mut-undo.log` |
| undo-cap | `finish_edit` 不淘汰最旧快照 | **RED**：`undo_history_is_bounded_to_one_hundred_steps`（`!can_undo()` 失败） | 同上（append） |
| L6 | `ImeEvent::End` 分支恒 `false`（不取消组合） | **RED**：`ime_end_event_cancels_the_composition_through_dispatch` | `.review/mig-p2-3b-mut-ime-end.log` |

> xtask 注记：`cargo xtask gates` 从**已提交的 HEAD** 同步远端源码。P2-3b 的门禁 5 在最终提交
> `4b40f2e` 之后复跑，日志首行 `== head: 4b40f2e ... ==`，mac check 6.0 s / linux check 6.0 s，
> `RESULT: PASS`——覆盖本批改动。此前一次运行显示 head `4b667e2`（P2-3 基线，未含未提交改动），
> 不作为本批证据。

## 9. P2-3c 复核收尾（R2 剩余必修 ①–④）

- 提交：**`4dc8f65`**（`fix(base_ui): P2-3 复核回归（caret 屏幕坐标/空行间距/update_value 程序化契约）`，
  父 `4b40f2e`，追加提交未 amend/rebase；仅 2 文件 +288/−69）。
- 输入：`.review/mig-p2-3-review.md`「复核轮次 2」的剩余必修 ①–④（R2-F1/F2/F3/F4）。

### 9.1 处置

| 项 | 处置 |
|---|---|
| R2-F1 caret/选区画布重复减滚动偏移 | `caret_canvas` 删除 `shift`/`scroll_x`：caret 直接取 `Window::text_caret_rect`（引擎已含祖先滚动偏移，屏幕坐标），选区矩形 = `NodeTree::text_selection_rects`（局部）+ 「窗口绝对 − 局部」原点差。4b40f2e 的「quad = 布局 − offset」错误断言删除，改为 `quad == Window::text_caret_rect`；新增 `scrolled_caret_and_selection_canvases_use_screen_coordinates`（滚动后 caret + 选区 quad 都按屏幕坐标核对，且选区左缘在视口内）。 |
| R2-F2 M4 过度校正 | `build_blocks` 恢复 `previous_blank` 抑制 `BLOCK_GAP`；非首块标题只额外加 `HEADING_SPACE_ABOVE`。结果：空行后段落 **0** / 标题 **8**；无空行相邻块段落 8 / 标题 16（旧 `MarkdownState::render` 语义）。M4 测试改为正确期望并加「无空行相邻块」对照。 |
| R2-F3 M7 裁决（Lead） | `update_value` 按旧 `set_value` 程序化契约：**绕过 readonly/disabled**、**清空 undo/redo**、光标到末尾、单行清洗/`max_length`+`TooLong`/取消组合同 `set_text`；`set_text` 保持用户输入受限语义。旧源码核对（`gpui-base-0.9.1/src/input/base/state.rs`）：`set_value → replace_text`（`with_edits_allowed`，程序化绕过）、`undo_manager.clear()`、`reset_selection` 单行末尾/多行 `0..0`、`reset_scroll_to_start` 单行滚动复位开头。测试：删 `update_value_respects_readonly_and_disabled`，新增 `update_value_bypasses_readonly_and_disabled`、`update_value_clears_the_undo_history`。 |
| R2-F4 文档/台账 | 本文 §3.1（build 签名含 `placeholder_text`、`InputNode` 字段 `viewport`/`placeholder_state`、`InputState` 新方法表）、§2.3 与 §4.1.10（M4 正确语义）、§4.2.9（M1 屏幕坐标）、§4.2.13（M7 两入口差异）、§4.2.16/§4.2.17（Info 登记）；台账 `test-ledger/p2-3.md` §10.1（M1/M4/M7 行）与新增 §12。 |

### 9.2 与旧实现的两处已登记差异（R2-F3 核对结论）

1. 旧 `set_value` 对单行会 `reset_scroll_to_start`（长值显示**头部**）；新实现每帧 `sync` 都 reveal
   caret ⇒ 程序化写长值后显示**尾部**。当前没有需要「显示头部」的生产用法，登记为已接受差异。
2. 旧 `set_value` 多行把选区复位成 `0..0`；新 `replace_text` 统一落在**末尾**。当前没有多行
   生产输入，登记（多行批次再对齐）。

### 9.3 测试增减

| 项 | P2-3b 后 | P2-3c 后 |
|---|---|---|
| `kit::markdown` | 169 | **169**（不变；M4 测试断言改写 + 无空行对照） |
| `kit::input` | 61 | **63**（−1 删除错误语义测试，+3 新增：滚动 caret/选区、readonly/disabled 绕过、清 undo） |
| `base_ui` lib 全量 | 655 | **657**（`cargo test -p base_ui --lib` 实跑 657 passed） |

### 9.4 门禁（head `4dc8f65`，全部实跑）

| # | 门禁 | 结果 | 证据 |
|---|---|---|---|
| 1 | `cargo +nightly fmt -p base_ui -- --check` | **PASS**（exit 0） | `.review/mig-p2-3c-fmt.log` |
| 2 | `cargo +nightly clippy -p base_ui --all-targets -- -D warnings` | **PASS**（exit 0） | `.review/mig-p2-3c-clippy.log` |
| 3 | `cargo test -p base_ui --lib` | **PASS 657/657** | `.review/mig-p2-3c-test.log` |
| 4 | `cargo check -p base_ui --all-targets` | **PASS**（exit 0） | `.review/mig-p2-3c-check.log` |
| 5 | `cargo xtask gates --platform all --step quick --scope base_ui` | **PASS**：`== head: 4dc8f65`，mac check 16.0 s / linux check 10.0 s，`RESULT: PASS` | `.review/mig-p2-3c-xtask-quick.log`（xtask 自身日志 `.review/mig-gates-1795237143-all.log`） |

### 9.5 变异自检（实跑；每项用备份逐字节还原，`RESTORE_IDENTICAL=True`）

| 编号 | 变异 | 结果 | 证据 |
|---|---|---|---|
| R2-F1 | 恢复旧行为：`caret_canvas` 再加 `shift = (-scroll_x, 0)`（caret 与选区都减滚动偏移） | **RED**：`long_single_line_scrolls_to_keep_the_caret_visible`（quad 154.39 vs 屏幕 caret 793.0）+ `scrolled_caret_and_selection_canvases_use_screen_coordinates`（quad 286.99 vs 645.61） | `.review/mig-p2-3c-mut-1.log` |
| R2-F2 | 恢复过度校正：`block_index > 0 && !previous_blank` → `block_index > 0`（空行后也加 8px） | **RED**：`a_heading_after_a_blank_line_keeps_the_extra_space_above_it`（空行后标题 16 vs 期望 8） | `.review/mig-p2-3c-mut-2.log` |
| R2-F3 | `update_value` 走受限路径：readonly/disabled 早退 + `set_text` | **RED**：`update_value_bypasses_readonly_and_disabled`（`readonly.node.update_value(...)` 返回 false） | `.review/mig-p2-3c-mut-3.log` |

### 9.6 遗留

- 无阻塞项。Info：`InputState` 内联 100 步 undo 快照 ⇒ `dispatch` 每次克隆整份历史（§4.2.16）；
  `undo()`/`redo()` 属程序化状态 API，不受 readonly 限制（§4.2.17，与 `update_value` 同一口径）。
- 旧契约的「单行滚动复位开头 / 多行选区 0..0」按 §9.2 登记为已接受差异。

## 10. P5 回写：编辑内核已下沉 `ui_core`（输入组件重建）

- 背景：本文 §3/§4.2 记录的是 P2-3 时期的 `base_ui::kit::input`（组件自带编辑状态机、三套坐标换算、
  caret/选区 Canvas、整串 undo 快照）。该 crate 之后更名为 `ui_kit`；文本输入重构
  （`.docs/text-input-redesign-design.md`，P0–P4）把**编辑内核整体下沉到 `ui_core::text_edit`**，
  `ui_kit::kit::input` 退化为外观 / 布局 / 校验 / 业务回调的薄壳。本节取代 §3 的 API 事实与
  §4.2 的遗留清单，§1/§2/§4.1（markdown）与 §5–§9 的历史门禁记录保持原样（历史记录，不再更新）。
- 基线：`bbcb24cd`（本轮起点）→ `cb965b56`（P4 合入 `main`）。提交序列：
  `a3888798`（P0 平台 WM_CHAR 过滤 + AltGr）→ `1f52c099`（P1a-1 可编辑节点 + 空行盒）→
  `8a558517`（P1a-2 display 映射 / 掩码 / 快照）→ `434af283`（Linux/mac 验收预修）→
  `75e426a6`（P1b-1 编辑内核 + 键盘/Text 路由）→ `8df38db9`（P1b-2 IME / 组合 / a11y 角色）→
  `bb732834`（P1c-1 装饰绘制 + IME 定位）→ `7e2821fd`（P1c-2 指针路由 + 示例）→
  `feee4458`/`8de025b5`/`08b901f8`/`c287d649`/`af072f78`/`a6f80ea3`（小 API 与 Escape 修复）→
  `2b70fef2`（P1b-3 AccessKit TextRun）→ `dfbf4f9c`（P2 kit 薄壳 + 调用点迁移）→
  `589a462a`（P3 闪烁 / ensure-visible / 滚动条）→ `eee0f04e` + merge `cb965b56`（P4 彩色字形）。

### 10.1 职责迁移（旧组件 → 引擎）

| 旧（本文 §3.1 记的事实） | 新归属 |
|---|---|
| `InputState` 携带 `text / caret / anchor / composition / undo / redo / dragging / error` | `ui_core::text_edit::EditableText`（节点 side table，`NodeTree::editable*`）；`InputState` 变成**只读镜像**（`TextEditSnapshot` 的投影）+ `error` 业务态 |
| `InputState` 的 24 个编辑方法（insert/delete/move/select/ime_*/copy/cut/paste/undo/redo/…） | 全部删除；用户编辑走 `ui_core` 事件路由（pointer / Key / Text / Ime 直达聚焦可编辑节点），程序化走 `App::text_edit_*` |
| `CaretMotion` 枚举 | 删除；引擎内部 `TextMotion` |
| `handle_key` / `handle_ime` / `clean_single_line` / `utf16` 辅助 / `word_range_at` / `word_class` / `masked_offset` | 删除并下沉：`text_edit::handle_key` / `handle_ime` / `sanitize` / `utf16_*` / parley `word_from_point` / `MaskedDisplay` 双向映射 |
| 三套坐标换算（`text_local_point` / `to_text_local` / `caret_canvas` 的 shift） | 删除；引擎统一：命中走 `NodeTree::hit_test`（已扣祖先滚动）、拖拽超出节点时 `scroll::ancestor_offset`、绘制用节点 content-box 局部坐标 |
| `text` / `placeholder` / `caret` 三个子节点（placeholder 绝对定位、caret = `NodeKind::Canvas`） | 删除；可编辑文本节点自己绘制选区 / 字形 / 组合下划线 / placeholder / caret（paint pass 内按序在同一节点） |
| 组件 `dispatch(&Event)`（外壳 drain 后转发） | 删除；`Event::Input`/`Event::Ime` 由 `App` 在分发期先交给聚焦可编辑节点，被消费则不入事件队列 |
| `sync` 负责焦点镜像 + IME + reveal + 重填 caret Canvas | `sync` 只做：主题色、`visible_lines` 高度、viewport/`ScrollBar` 内容尺寸、placeholder 文本与颜色、a11y 业务字段、`Signal<InputState>` 镜像；闪烁/ensure-visible/IME area 由引擎在 layout pass 后处理 |
| 整串快照 undo（100 步 `String` 克隆） | `UndoStack` 区间增量（`range + Arc<str> removed/inserted + 前后选区`），按编辑会话合并，上限 100 |

### 10.2 删除清单（AGENTS §1.1：不留兼容层）

- 组件公开面：`InputState` 的编辑方法、`InputState::display_text`/`set_text`/`set_focused`、
  `CaretMotion`、`InputNode::{text, placeholder, caret}` 字段、`InputNode::dispatch`、
  `input::build` 之外的 `make_*` 辅助。
- 组件内部：`Composition` / `EditSnapshot` / `EditUndo` / `caret_canvas` / `apply_canvas` /
  `clean_single_line` / `text_local_point` / `to_text_local` / `utf16` 家族 / `word_range_at` /
  `word_class` / `masked_offset`。
- 事件接线：`widget_gallery_ui` 与 `ui_kit/examples/gallery.rs` 里把 `InputEvent::Text`/`Key`
  转发给 input 组件的代码（其它控件仍保留 dispatch）。
- 采集：全仓 `git grep` 这些符号 **0 命中**（`clean_single_line` / `CaretMotion` /
  `word_range_at` / `masked_offset` / `engine_text` 均无残留；`InputState::new` 仅剩信号初始化，
  见 §10.4）。

### 10.3 §4.2 遗留项处置（17 项逐条）

| # | 旧遗留 | 处置 |
|---|---|---|
| 1 | hover 态（引擎无 per-node hover 信号） | **仍登记**（未变；引擎无 hover 读面） |
| 2 | `↑/↓` 视觉行移动不支持 | **关闭**：引擎 `Selection::move_lines` + `h_pos` 列记忆；PageUp/Down 按最近祖先 viewport 行数 |
| 3 | undo/redo 快照式 | **关闭**：区间增量 + 会话合并（D7） |
| 4 | 按住拖选用 `origin/current` 窗口坐标手工换算 | **关闭**：引擎 pointer 路由；超出节点按 `ancestor_offset` 换算，拖出 viewport 自动滚动 |
| 5 | 簇边界（按 `char`） | **关闭**：全部编辑命令按 parley `Cluster::text_range()` |
| 6 | 双击选词 / 三击选行 | **关闭**：引擎 `word_from_point` / `hard_line_from_point`，双击后拖拽按词/行扩展，Shift+点击扩展 |
| 7 | 单行清洗只在组件（`\r\n\t`） | **关闭**：平台 `WM_CHAR` 过滤 + 引擎 `sanitize`（多行仅放行 `\n`），双保险 |
| 8 | 自动重复键 | **关闭**：引擎 `handle_key` 支持 repeat 的编辑/导航键，Enter/命令键仅首按 |
| 9 | 单行水平滚动 + caret 屏幕坐标画布 | **关闭**：引擎 reveal（横向留 4px，kit 尾部再留 5px inset）+ 引擎绘制，Canvas 删除 |
| 10 | `sync`/`dispatch` 由外壳驱动、窗口级 IME 开关 | **关闭**：事件引擎内消费；IME cursor area 由引擎写（focus 变化时清/重设） |
| 11 | 密码框平台 IME 语义（`set_ime_content_type(Password)`） | **仍登记**（平台批次；`A11yRole::PasswordInput` 已接线，见 P1b-2） |
| 12 | 图标 / 清空按钮 / 密码可见性切换 | 清空按钮 **已实现**；图标化与密码可见性切换 **仍登记** |
| 13 | 运行时更新 API（`update_value`/`update_placeholder`） | **关闭**：`update_value`/`update_label`/`update_placeholder`/`clear`/`set_clearable`/`set_submit_on_blur` 全部委托 `App::text_edit_*` |
| 14 | a11y 部分缺失 | **关闭**：引擎维护 `value` / `selection`（虚拟 TextRun 子节点）/ `placeholder` / `read_only` / `invalid`；kit 只写业务字段 |
| 15 | 单行多行共用一条状态机但没有视觉行键 | **关闭**：同一引擎内核 + `EditableSpec.multiline` |
| 16 | undo 历史 O(历史×正文) 克隆进 `dispatch` 热路径 | **关闭**：`dispatch` 删除；快照只在 `generation` 变化时克隆 `Arc<str>`（`snapshots_share_the_text_allocation_until_the_content_changes`） |
| 17 | readonly 与程序化 API 口径 | **关闭并改写**：引擎里用户路径拒绝编辑（readonly 消费 IME/Text 但不改文本）；程序化 `App::text_edit_set_value` 绕过 readonly/disabled 并清 undo（对齐旧 `set_value` 契约）；`update_value` 的 `TooLong` 按**清洗后**字素计数 |

### 10.4 新公开面（`ui_kit` 侧）与镜像口径

```rust
pub struct InputSpec { name, label: Option<SmolStr>, placeholder, masked, multiline,
                       readonly, disabled, max_length: Option<usize>（grapheme）,
                       visible_lines: Option<u16>, size: ControlSize, submit: Option<Action> }
pub struct InputState { /* 私有：镜像快照 + error */ }   // Clone + PartialEq；无编辑方法
impl InputState { pub fn new(text: impl Into<String>) -> Self;      // 只用于信号初始化
                  pub fn text/selection/caret/is_composing/is_focused/error/can_undo/can_redo;
                  pub fn validate(&mut self, spec) -> Option<InputError>;
                  pub fn set_error(&mut self, Option<InputError>); }
pub struct InputNode { root, viewport, editable: NodeId, scrollbar: RefCell<Option<ScrollBar>>,
                       placeholder_state: Signal<SmolStr>, state: Signal<InputState>, spec: Arc<InputSpec> }
impl InputNode { install / release / sync / value / update_value / update_label /
                 update_placeholder / clear / set_clearable / set_submit_on_blur }
```

- `InputState::new(text)` 仍是唯一的信号初始化入口（22 个调用点全部是 `app.signal(_scoped)`），
  `install` 把初始文本推入引擎；此后单向镜像（引擎 → Signal）。
- `InputNode::value(app)` 读引擎（不是镜像），供 `InputState` 尚未镜像到最新一帧时使用。

### 10.5 门禁与测试基线（head `cb965b56` + P5 工作区改动，Windows）

| 门禁 | 结果 |
|---|---|
| `cargo test --workspace` | **8866 passed / 0 failed / 30 ignored**（60 测试二进制 + 41 doc-test 二进制 = 101） |
| `cargo test -p ui_core --lib` | 2280（2279 passed + 1 ignored） |
| `cargo test -p ui_core --lib --features test-support` | 2610（2609 passed + 1 ignored） |
| `cargo test -p ui_core --features test-support`（lib + integration） | frame_record 8 / layout 4 / snapshot 13 / gui_smoke 1 ignored / demo example 39 |
| `cargo test -p ui_kit --lib` | **748 passed** |
| `cargo +nightly clippy --workspace --all-targets -- -D warnings` | PASS（0 warning / 0 error） |
| `cargo +nightly fmt --check` | PASS（exit 0） |
| gallery `--idle-check` / `--theme-check` | PASS（空闲 0 帧 / 3 s；Dark→Light→Dark 0 failure） |

设计依据与验收标准见 `.docs/text-input-redesign-design.md` §10/§11。四个实机 BUG 的回归用例分两层：

| BUG | 引擎层（能变红的那条） | 组件层 |
|---|---|---|
| ① placeholder 对齐 | `app::text_edit_tests::placeholder_paints_at_the_text_paint_position`、`placeholder_line_baseline_matches_the_first_text_baseline` | `kit::input::tests::placeholder_and_text_share_the_first_line_box_and_the_caret` |
| ② 空文本 caret | `app::text_edit_tests::empty_editable_text_measures_a_positive_line_box`、`empty_editable_caret_geometry_uses_the_line_box` | `kit::input::tests::empty_text_caret_sits_inside_the_line_box_of_every_size` |
| ③ 滚动页内拖选 | `app::text_edit_pointer_tests::ancestor_scroll_drag_keeps_the_anchor_and_focus_bytes` | `kit::input::tests::dragging_inside_a_scrolled_page_selects_the_expected_range` |
| ④ 控制字符 | `platform::event::tests::control_char_messages_never_become_text` | `kit::input::tests::control_characters_never_reach_the_engine_text` |

变异证据（11 项，含上述四条各自能变红的变异）见设计文档 §11.5；三端复跑结果与平台差异
（Linux 字体行高导致的 4 条 P3 用例失败、macOS 无 Metal 会话导致的 2 条渲染类失败、远端新
nightly 的 2 条 clippy）见 §11.7/§11.8。

### 10.6 三端最终结果（P5 收尾第二轮，head cb965b56 + P5 工作区改动）

| 平台 | ui_core lib | ui_core test-support | ui_kit lib | clippy | fmt | gallery idle/theme |
|---|---|---|---|---|---|---|
| Windows | 2280（2279+1 ignored） | 2610（2609+1 ignored） | 748 | 0 | 0 | 0 帧 / 0 failure |
| Ubuntu | 2062 | 2386 | 748 | 0 | 0 | 1 帧 / 0 failure |
| macOS | 2027 | 2351 | 748 | 0 | 0 | 0 帧 / 0 failure |

Linux 的 4 条 nsure_visible_*、macOS 的 3 条失败均在 P5 收尾修掉（引擎口径修复 + 用例口径修复 +
2 条渲染类用例改为显式环境前置），详见设计文档 §11.8/§11.9。
