# 文本输入组件重构设计（Input / TextInput）

- 状态：**已实施**（P0–P4 全部提交并合入 `main`；实施结果与偏差登记见 §11）
- 范围：`crates/ui_kit/src/kit/input.rs` 组件 + `ui_core` 文本编辑能力 + Windows 平台输入修复 + 彩色 emoji 渲染通路
- 本轮约束：设计阶段**不修改代码**；本文档是实现阶段的唯一设计依据（§11 为实施后的回写）
- 相关现状文档：`.docs/gui-migration/spec-deltas/kit-text.md`（旧 input 迁移记录与已知遗留 §4.2；P5 回写见其 §10）
- 证据基准：文中 `file:line` 均为设计定稿时的源码；实现后的行号与 API 以 §11 为准

---

## 0. 决策记录（用户已裁决）

| # | 决策点 | 结论 |
|---|---|---|
| D1 | 架构分层 | **B：引擎级可编辑文本节点**。`ui_core` 拥有 buffer / layout / selection / compose / undo / 光标闪烁 / IME 定位；`ui_kit` 退化为外观、布局、校验与业务回调的薄壳 |
| D2 | 引擎状态与应用层状态的关系 | **B1：引擎权威 + `Signal<InputState>` 镜像**。引擎 → Signal 单向快照；应用层现有 14+ 处 `read(&state).text()` 保持不变；6 处直接 `app.write(&state, InputState::new(...))` 改为 `InputNode::update_value` |
| D3 | 彩色 emoji | **E1：独立 RGBA 彩色字形图集 + `KIND_COLOR_GLYPH` 渲染分支**；渲染系统字体原生彩色 emoji（Windows Segoe UI Emoji / macOS Apple Color Emoji / Linux Noto Color Emoji） |
| D4 | 多行高度语义 | **固定可视行数**：`max_lines` 决定可视高度，内容超出在 viewport 内垂直滚动；不自动增高 |
| D5 | 滚动条 | 仅内容溢出时出现；overlay 悬浮 + hover 淡入（复用 `kit::ScrollBar`），文本右侧预留 gutter，滚动条不参与换行宽度 |
| D6 | `max_length` / 掩码计数单位 | **字素簇（grapheme cluster）**；掩码每个字素簇一个 `•` |
| D7 | 撤销粒度 | **按编辑会话合并**（连续键入、连续删除、一次 IME 提交、一次粘贴各一步；导航/选区变化/换行/失焦打断），上限 100 步；快照改为区间增量 |
| D8 | 光标闪烁 | **530ms 半周期**（亮/灭各 530ms）；键入/移动/IME 输入时重置为亮；IME 组合中常亮不闪；失焦隐藏；**有选区时仍显示 caret**（在 focus 端） |
| D9 | 键鼠行为范围 | **全量补齐**：↑/↓（记忆目标列）、PageUp/PageDown、Ctrl/Alt+←/→ 词移动、Ctrl/Delete 词删除、Shift+Home/End、双击选词（Unicode 词边界）、三击选行、双击后拖拽按词/行扩展、Shift+点击扩展、拖出控件自动滚动、剪贴板、撤销/重做 |
| D10 | 架构实现修正（设计说明） | B 的"PlainEditor 内核"以**语义内核**落地：复用 parley `Cursor/Selection/Cluster` 编辑原语与 `PlainEditor` 的已验证语义，但**不把 `PlainEditor` 类型直接放进节点**。原因见 §3.2：掩码输入需要"真实文本 ↔ 掩码显示文本"双向映射；多行测量的断行重排必须在无 `FontContext` 的 measure 闭包内完成（与现有 `ShapedText` 测量机制一致）。这是一次有依据的实现细化，不改变 D1 的状态归属 |
| D11 | Windows `WM_CHAR` 控制字符 | 平台层统一过滤（对齐 macOS/Linux 现有行为），并在引擎编辑入口做第二道防御 |
| D12 | API 兼容 | 允许破坏性修改 `InputSpec/InputState/InputNode` 公开面并同步全部 18 个调用点（AGENTS §1.1：不保留兼容层） |

---

## 1. 摘要

当前输入框 `crates/ui_kit/src/kit/input.rs` 是一个"用文本节点 + Canvas 覆盖层 + 手写状态机"拼出来的组件：

- 文本排版和 caret/选区几何由 `ui_core` 的文本引擎提供，但**组件自己重新推导了 caret 位置、命中坐标、滚动补偿**，产生三套互不一致的坐标换算；
- 空文本的文本节点测量高度为 0，组件却假设文本节点盒就是行盒；
- placeholder 用绝对定位贴在内容盒顶部，与文本行盒无关；
- 输入事件、IME、pointer、键盘全部在组件层手工处理，且 Windows 平台把 Backspace/Enter/快捷键产生的 `WM_CHAR` 控制字符当正文插入；
- 彩色 emoji 被字形图集压成单色；字素簇（ZWJ emoji、组合字符）按 Unicode scalar 删除/移动；多行没有 viewport 概念，`max_lines` 只截断不滚动；没有光标闪烁。

本设计将文本编辑能力下沉到 `ui_core`：新增**引擎级可编辑文本节点**（`EditableText`）与对应的事件、布局、绘制、IME、闪烁能力；`ui_kit::input` 保留组件外观、尺寸/滚动条、清空按钮、校验与业务回调，并通过 `Signal<InputState>` 镜像把状态提供给应用层（B1）。四个实机 BUG 在架构上被根除，而不是逐点打补丁；同时一次性支持彩色 emoji、固定行数多行 + 滚动条、光标闪烁与完整成熟的键鼠行为。

---

## 2. 现状调查与根因（证据链）

### 2.1 当前组件结构

`InputNode` 由 5 个节点组成（`crates/ui_kit/src/kit/input.rs:980-1243`）：

```
root（border/背景/focus/error，flex_row + items_center，相对定位，overflow_hidden）
├─ viewport（仅单行：flex_row + items_center + height_full + flex_1 + overflow_hidden）
│   └─ text（文本节点；单行 flex_none，多行 flex_1）
├─ placeholder（absolute；top = padding_y；left/right = padding_x）
└─ caret（Canvas，absolute 全内容盒；sync 时用窗口坐标重填选区 quad + caret quad）
```

状态机 `InputState` 持有：`text: String`、`caret/anchor`（UTF-8 byte）、`composition`、`undo/redo`（100 步整串快照）、`dragging`、`error` 等（`input.rs:101-113`）。宿主每帧调用 `sync`（焦点镜像/IME/滚动/caret 画布），事件由宿主 `drain_events` 后逐条交给 `dispatch`（`input.rs:1643-1683`）。

### 2.2 BUG ①：placeholder 垂直位置不正确

**机制**：placeholder 是 root 的绝对定位子节点，`top = padding_y`（单行 `padding_y = 0`），贴内容盒顶部（`input.rs:997-1002,1173-1199`）；而文本节点在 viewport 中由 `items_center` 在固定内容高内垂直居中（`input.rs:1097-1121`）。两者只有 `content_h == line_h` 时才重合。

**量化**（Windows，Segoe UI 14px，行盒 19px；`TextStyle.line_height=None` ⇒ parley `MetricsRelative(1.0)` + quantize）：Medium 单行 root 高 30、border 1 → 内容高 28，行盒 top offset `(28−19)/2 = 4.5`；placeholder 在 0 处 → **偏上 4.5px**；Large 单行偏上 7px。多行空态更严重：root 只有 `min_height = 20`（Small）且 `align_self START`（`input.rs:1014-1017`），96px 的 gallery wrapper 完全没有传递进组件，placeholder/文本都缩在顶部 20px 内。

**结论**：结构性错误。placeholder 必须与文本**共享首行行盒**（成熟方案见 §4.1）。

### 2.3 BUG ②：空文本 caret 出现在下半截

**机制链**：

1. `ShapedText::measure` 对空文本直接返回 `LogicalSize::ZERO`（`crates/ui_core/src/text/shape.rs:278-285`，测试 `empty_text_measures_zero`）；
2. text pass 用该测量值设置节点测量尺寸（`crates/ui_core/src/app/text_pass.rs:491-506`）→ 空文本节点布局高度 = 0；
3. 单行 viewport `items_center` 把 0 高文本盒放在内容区正中（28/2 = 14）；
4. 但 parley 的空 layout 仍有一行带字体 strut 的空行，`cursor_geometry` 返回 `(0,0,1,行盒高)`（`crates/ui_core/src/text/inline_flow.rs:304-313` 测试；parley `cursor.rs:408-419` 的 `last_line_cursor_rect`）→ 引擎 caret 从文本盒 origin（= 垂直中点）开始向下画 19px，被 root clip → 只剩下半截。

输入文字后文本节点高 19、被正确居中，caret 才回到正常位置——与实机现象完全一致。

**结论**：空文本必须拥有确定的**空行盒（strut）**，且 caret/placeholder/滚动步长共用同一行盒口径。

### 2.4 BUG ③：拖动选字总是回到文本最前面

**机制链**：

- 点击路径：`ctx.hit()` 的 `HitResult.local_point` 已由命中测试扣除祖先滚动（`crates/ui_core/src/layout/hit_test.rs:328-426`），再换算到文本节点局部坐标（`input.rs:1770-1779`）→ 正确。
- 拖拽路径：`DragInfo.origin/current` 是**窗口坐标**（`crates/ui_core/src/app/input.rs:245-300`），组件用 `point - text_bounds.origin + 自身 scroll` 手工换算（`input.rs:1319-1330`），**漏掉祖先（gallery 页面）滚动**。
- gallery 第 10 节在 layout y ≈ 2284（常量推算），实机要滚动 P ≈ 1600–2300 才能看到；于是 `local_y ≈ −P`。`inline_flow::byte_index_at` 对 `y < 0` 直接 `return 0`（`crates/ui_core/src/text/inline_flow.rs:83-98`）→ 拖拽起点/每次移动都映射到 byte 0 ⇒ anchor 恒为 0，最终选区 = "文本开头 → 释放点"。
- 现有单测在无滚动 TestApp 根下运行（`input.rs:1955-1983`），三套换算恰好退化为恒等，所以全部通过。

**结论**：根因是"窗口坐标 / 未滚动布局坐标 / content-box 局部坐标"三种坐标空间混用 + 引擎缺少"窗口点 → 文本位置"的公共 API。

### 2.5 BUG ④：Backspace 无法删除 + 中文变豆腐块

**机制链**：

1. Windows 消息泵对每条消息执行 `TranslateMessage`（`crates/ui_core/src/platform/windows/event_loop/pump.rs:465-477`），因此 `WM_KEYDOWN VK_BACK` 会额外产生 `WM_CHAR(0x08)`；Enter → 0x0D、Esc → 0x1B、Ctrl+A/C/V/X/Z/Y → 0x01/0x03/0x16/0x18/0x1A/0x19。
2. 平台 `WM_CHAR` 分支只做 IME 补发抑制 + 代理对合并，**没有控制字符/修饰键过滤**（`crates/ui_core/src/platform/event.rs:158-184,237-249`）。
3. 事件进入组件：`InputEvent::Text` 直接 `insert()`（`input.rs:1653-1658`）；`clean_single_line` 只过滤 `\r \n \t`，多行完全不过滤（`input.rs:1786-1793`）。
4. 净效果：按一次 Backspace = KeyDown 删 1 个真字符 + `Text("\u{8}")` 插回 1 个 U+0008；U+0008 无字形 → `.notdef` 豆腐块（`shape.rs:1037-1064` 仍绘制 glyph 0）。Ctrl+V 会在粘贴后追加 0x16，多行 Enter 会变成 `\n\r`。
5. 平台对比：macOS 已有 `!c.is_control()` + Control/Meta 屏蔽（`crates/ui_core/src/platform/macos/view.rs:125-140`）；Linux 同样（`crates/ui_core/src/platform/linux/surface.rs:1129-1165`、`wayland/text_input.rs:271-280`）。**只有 Windows 缺这两层过滤**。

**结论**：平台层缺陷（Windows），同时组件入口缺少防御。修复应在平台 `translate` 层（所有平台对齐），并在引擎编辑入口做 `is_control` 过滤兜底。

### 2.6 其余确认到的缺陷（本轮一并解决）

| # | 缺陷 | 证据 | 处理 |
|---|---|---|---|
| P1 | `sync` 在引擎 text/layout pass **之前**执行，caret/选区永远用上一帧布局；文本刚变时 `text_caret_rect` 返回 Err → 旧画布保留，出现一帧滞后与"旧 caret 粘住" | 帧序 `platform/.../pump.rs:646-654` + `app/frame.rs:113-148`；守卫 `node/tree/content.rs:285-341`；`input.rs:1685-1738` | B 架构：caret/选区/IME 由引擎在 layout pass 之后计算并绘制，不存在组件侧 stale 问题 |
| P2 | 单行横向滚动里屏幕坐标与内容坐标混用（`window.text_caret_rect`−布局原点 再与 `next + viewport_width` 比较），长文本可能滚到 caret 不可见的位置；`line_height: px(1.0)` | `input.rs:1256,1626-1634` | 引擎接管 ensure-visible（使用统一坐标空间） |
| P3 | 选区画在文本**之上**（caret Canvas 在 text 节点之后），`Role::Selection = mix(brand, canvas, 0.62)` 不透明 → 选中文字被盖住 | `input.rs:1685-1738`；`theme/palette.rs` 的 Selection | 引擎在字形前画选区、字形后画 caret/组合下划线 |
| P4 | 彩色 emoji 被压成单色 | `text/raster.rs:169-195`（`bgra_to_coverage` 只取 alpha）；字形图集单通道 `text/atlas.rs:148-208` | E1 彩色字形图集 |
| P5 | 字素簇编辑缺失：Backspace/←/→ 按 `char`；ZWJ 家族、肤色修饰、国旗会被拆开；`max_length` 也按 char | `input.rs:220-258,284-305,1786-1793`；`kit-text.md §4.1.6/§4.2.5` | 车级编辑按 grapheme cluster（parley Cluster / ICU 分段） |
| P6 | 多行没有 viewport/滚动；root 只到 `min_height`，宿主高度传不进来；没有 ↑/↓、PageUp/Down；没有滚动条 | `input.rs:997-1120`；gallery `sized_box` 只给 wrapper 尺寸（`examples/gallery.rs:282-292`） | 固定可视行数 `max_lines` + viewport + ScrollBar |
| P7 | 没有光标闪烁 | 组件与引擎均无 | 引擎 caret clock + timer 调度 |
| P8 | 输入框聚焦时 ↑/↓ 同时滚动外层页面（引擎 `default_key_scroll` 在事件分发末尾无条件执行） | `app/input.rs:560-675` | 可编辑节点消费导航键时跳过引擎默认滚动 |
| P9 | `dispatch` 的"双读比较"写法与其它控件不一致，submit handler 同步写 state 时会丢弃 `next` | `input.rs:1676-1681`；对照 `interactive.rs:574-608` | B 架构下删除该路径 |
| P10 | IME 补发抑制存在竞争窗口：任何 `WM_KEYDOWN` 都会 `clear_pending`；不匹配字符清空队列并放行，中文提交可能重复 | `windows/ime.rs:138-183`；`event.rs:158-161` | 组合期文本走 IME 事件、普通字符消息不插入；补测试 |
| P11 | 控件高度 API 缺失：`InputSpec`/`build` 无法表达控件盒高度，gallery 单行 30px 输入框落在 40px 包装盒顶部 | `input.rs:1004-1020`；`examples/gallery.rs:1221-1232` | 新 `InputSpec.visible_lines`/高度语义，见 §5 |
| P12 | undo 快照整串克隆：每次 dispatch 克隆 `InputState`（含 100 步 String 历史），长文本 O(历史×正文) | `kit-text.md §4.2.16` | 区间增量 + `Arc<str>`，见 §3.8 |
| P13 | 掩码按 char 逐字符 `•`，emoji 会显示多个点；caret 映射同样按 char | `input.rs:140-149,781-783` | 字素簇掩码 + 映身表 |
| P14 | a11y 未暴露选区 | `A11yPatch.selection` 已存在（`a11y.rs`），组件未填 | 引擎自动填 value + selection |

---

## 3. 外部成熟方案调研结论

### 3.1 caret / placeholder / 行盒（工程共识）

| 框架 | caret 几何来源 | 空文本 | placeholder |
|---|---|---|---|
| Chromium/Blink | 非空：行内 fragment 的 block-size（=行盒高）；并 clamp 到行盒 | `LocalCaretRectForEmptyElement`：bar 高 = 字体度量 Height，在 `line-height` 空间内垂直居中 | 独立 block（UA shadow DOM），靠同一字体/行高对齐，不参与 caret 定位 |
| Qt 6 | `QTextLine::height() + 1`（`qwidgetlinecontrol.cpp:379-389`） | 行高 | `QLineEdit` 用 `QFontMetrics` 独立 drawText；`QTextEdit` 对齐首行 frame top |
| Flutter | `preferredLineHeight` / caret metrics | `preferredLineHeight`（不要求先 layout） | InputDecorator 与 input 同 baseline 布局；hint 是独立子节点 |
| egui | `cursor_rect(galley, cursor, row_height)`，空 galley 回退 `row_height` | `row_height` | 同一 layouter，显式 `atom_align` 对齐 |
| parley 0.12 | `Cursor::geometry` 取行 metrics `block_min/max_coord` | 空 layout 的 strut 行（但"只有空 inline box 的行"可 0 高，需要 UI 层保证行高） | 不提供 |

**对实现的约束**：caret/placeholder/行高/滚动步长/IME 候选框高度必须来自**同一个行盒度量**；空文本必须有确定行盒；placeholder 不参与文本布局。

### 3.2 命中测试与选区

- 最小单位是 **UAX #29 grapheme cluster**（Blink 有专门的 grapheme 状态机；Qt `cursorToX` 吸附 grapheme boundary；parley `Cluster` 定义就是 caret/selection/hit-testing 边界）。
- **affinity** 必须建模：软换行/双向文本处同一 byte 有两个视觉位置；Blink 用 `(Position, TextAffinity)`，parley 用 `Affinity::{Upstream,Downstream}` 并在 `Cursor::geometry` 中据此选左/右 cluster。
- **desired x（列记忆）**：Qt 有 `QTextCursor::setVerticalMovementX`；parley 用 `Selection.h_pos` 在 `move_lines` 间保持；Flutter 用持久的 `VerticalCaretMovementRun`；Blink 仅在单次命令内保留。**跨按键列记忆需要显式保存（parley Selection 自带）**。
- 选区高亮按行拆矩形；行尾换行符宽度 parley 用 `(ascent+descent)×0.25` 表示（`selection.rs:503-619`）。
- 拖出控件自动滚动是 UI 层职责（Blink `AutoscrollController`；parley 不提供）。
- `maxlength` 在 HTML 是 UTF-16 code unit，但成熟编辑器删除/计数用 grapheme（Flutter `LengthLimitingTextInputFormatter` 用 `characters`）；本设计按字素簇（D6）。

### 3.3 多行"限行"与滚动

- **限行 = 视口高度限制，不是内容限制**：Flutter `maxLines` 只决定 RenderEditable 的 preferred height（仅 `maxLines==1` 截断排版），内容仍完整排版并滚动；Qt/Chrome 用滚动容器 + 按需滚动条。本设计采用 D4 固定可视行数。
- 滚动条策略：Qt 默认 `ScrollBarAsNeeded`；Flutter 只多行挂 Scrollbar；滚动条是否占位影响换行宽度（HTML textarea effective width 计入 scrollbar）。本设计 D5：overlay + gutter，不参与换行宽度。
- ensure visible 带 padding：Flutter `scrollPadding` 默认 20px；Qt `ensureCursorVisible`；egui `scroll_to_rect`。
- IME/选区坐标随滚动重算：候选窗锚点使用 caret/组合区域。

### 3.4 IME

- 预编辑是**文本流内的 composing range**（Chrome `CompositionText`、Qt `QInputMethodEvent`、Flutter `TextEditingValue.composing`、parley `set_compose/clear_compose/finish_compose`）；渲染由 UI 层加下划线/clause；commit 替换该 range，cancel 删除。
- 候选窗锚点用 caret/composition rect，不是 preedit 内部光标；Windows `COMPOSITIONFORM`+`CANDIDATEFORM(CFS_EXCLUDE)`、macOS `firstRect`、Wayland `set_cursor_rectangle`。
- **WM_CHAR 必须过滤控制字符**：Qt `QInputControl::isAcceptableInput`（`isPrint()`，TextEdit 额外允许 Tab）；GPUI `!c.is_control()`；Chromium Textfield `IsValidCharToInsert` 排除 `<0x20` 与 `0x7f..=0x9f`；winit 把"没有对应 keydown 的 WM_CHAR"视为 IME 文本丢弃。
- Windows 组合结束顺序陷阱（Hangul 在 ENDCOMPOSITION 后仍可能收到 COMPOSITION）需要平台层兼容（现有 `windows/ime.rs` 已有部分处理，设计保留并补测试）。

### 3.5 光标闪烁

| 框架 | 半周期 | 输入重置 | 选区时 caret | 失焦 |
|---|---|---|---|---|
| Chrome/Blink | 500ms（`LayoutTheme`，Windows 用 `GetCaretBlinkTime`） | 网页不重启相位；Views Textfield 重启 | 非 caret 选区时无 caret | 隐藏 |
| Qt 6 | 500ms（`cursorFlashTime=1000`） | 重置（`updateCursorBlinking`） | 由 `SH_BlinkCursorWhenTextSelected` 决定 | 隐藏 |
| Flutter | 500ms | 重置 | `selection.isCollapsed` 才闪 | 隐藏 |
| egui | 0.5s/0.5s | 重置（`last_interaction_time`） | 不隐藏 | 仅 viewport 有焦点时绘制 |

按 D8：530ms 半周期、输入重置、组合中常亮、失焦隐藏、**有选区仍显示 caret**（focus 端）。调度用单一时钟源 + 只重绘 caret 所在脏区，不用连续动画帧；Windows 可后续读取 `GetCaretBlinkTime` 覆盖默认值（登记为后续增强）。

### 3.6 掩码 / a11y

- 掩码单位框架各不相同：Qt/Flutter 按 UTF-16 unit、egui 按 `char`、Chrome 用 `-webkit-text-security` 由绘制层替换。本设计按字素簇（D6），并在文档中声明"一个 ZWJ emoji 显示一个 `•`"。
- a11y：AccessKit 提供 `TextInput/MultilineTextInput/PasswordInput` 角色 + `TextSelection{anchor,focus}` + `SetTextSelection/ReplaceSelectedText`；没有 preedit/clause 字段。组合期间不暴露 preedit；密码不暴露明文。

### 3.7 parley 0.12 可复用能力（关键）

| 能力 | parley API | 本设计使用方式 |
|---|---|---|
| 字素簇边界 | `Cluster::from_byte_index/from_point/text_range` | 命中吸附、删除/移动边界 |
| 点命中 + affinity | `Cursor::from_point`（按 cluster 左右半区 + RTL + 软换行） | 指针定位/拖选/Shift+点击 |
| caret 几何 | `Cursor::geometry`（行 metrics + affinity） | caret 绘制/IME/ensure visible |
| 视觉/逻辑移动、词移动、行首尾 | `Cursor::next_visual/previous_visual/next_visual_word/…`、`Selection::{line_start,hard_line_start,line_end,hard_line_end}` | 完整键盘映射 |
| 上下行 + 列记忆 | `Selection::move_lines(delta, extend)`（内部 `h_pos`） | ↑/↓、PageUp/Down（自研行数换算） |
| 双击选词/三击选行/拖拽扩展 | `Selection::{word_from_point,line_from_point,hard_line_from_point,extend_to_point,shift_click_extension}` | pointer 行为 |
| 选区矩形 | `Selection::geometry_with`（跨行 + 换行符宽度） | 选区绘制 |
| compose 模型 | `PlainEditor::{set_compose,clear_compose,finish_compose}`（语义参考） | IME 状态机实现 |
| 行度量 | `LineMetrics{line_height,baseline,offset,advance,block_min/max_coord}` | 行盒/滚动/IME/placeholder |
| 空文本行 | 需要 UI 层保证 strut/line-height | 引擎 `EditableText` 显式空行盒 |

**注意**（实现前必须验证的两个 parley 行为）：

1. 空文本 `Layout` 的行高来源：parley 对空文本会生成 strut 行（`shape/mod.rs:58-62` 的假空格），但"只有空 inline box 的行"可为 0 高（`line_break.rs:918-926`）。引擎必须显式设置/校验 `EditableText` 的空行盒高度 > 0，不能假设。
2. `PlainEditor::backdelete` 只对 hard line break 与 `is_emoji()` 簇删整簇，其余只删一个 scalar（`editing/editor.rs:279-316`）。本设计按 `Cluster::text_range()` 删整簇（严格 UAX #29），与产品要求一致。

---

## 4. 目标架构（B：引擎级可编辑文本节点）

### 4.1 分层与职责

```
应用层（settings_ui / widgets / gallery …）
  │  读写 Signal<InputState>（镜像） + 业务回调（validate/submit/clear）
  ▼
ui_kit::kit::input（薄壳）
  │  · 外观（border/背景/focus ring/theme）
  │  · 布局容器：单行/多行 viewport、max_lines 高度、ScrollBar、clear 按钮
  │  · InputSpec → EditableTextSpec 映射；把引擎快照同步进 Signal<InputState>
  │  · 业务层：校验错误、submit-on-blur、placeholder 文本/颜色、a11y role/label
  ▼
ui_core（引擎：编辑内核 + 渲染）
  │  · EditableText 节点：buffer / display 映射 / parley Layout / Selection / compose / undo
  │  · 事件路由：pointer / Text / Key / IME 直达聚焦可编辑节点（不再经宿主 dispatch）
  │  · 布局与绘制：行盒测量、选区（字形下）、caret/组合下划线（字形上）、placeholder
  │  · IME cursor area、caret 闪烁时钟、ensure-visible、拖拽自动滚动
  │  · 彩色 emoji：RGBA 彩色字形图集 + KIND_COLOR_GLYPH
  ▼
平台层（windows/macos/linux）
     · WM_CHAR 控制字符过滤（平台修复）
     · IME 事件、候选窗定位（复用现有链路）
```

**与成熟方案的对应**：这是 Flutter（Widget 状态）与 Chrome contenteditable（引擎编辑内核）的折中；D1 选择"引擎持有编辑状态"，由引擎承担 Chrome 角色（DOM+Selection+LayoutNG+IME 控制器），`ui_kit` 承担 Flutter 的 InputDecorator 角色；应用层通过镜像 Signal 保持响应式读取（B1）。

### 4.2 引擎数据模型

新增 `ui_core::text_edit` 模块（不依赖 `ui_kit`）：

```rust
// 公开：文本光标（byte index + 视觉亲和性）
pub struct TextCursor { pub byte: usize, pub affinity: TextAffinity }
pub enum TextAffinity { Upstream, Downstream }

// 公开：不透明选区包装，保留 parley Selection 的 h_pos / anchor_base（词/行扩展粒度）
pub struct TextSelection(/* parley::editing::Selection */);
impl TextSelection {
    pub fn anchor(&self) -> TextCursor;
    pub fn focus(&self) -> TextCursor;
    pub fn is_collapsed(&self) -> bool;
    pub fn range(&self) -> Range<usize>;   // 显示文本 UTF-8 byte
}

// 公开：可编辑节点规格（挂载期常量；运行期可改的字段单独 setter）
pub struct EditableSpec {
    pub role: A11yRole,                 // TextInput / MultilineTextInput / PasswordInput
    pub label: SmolStr,
    pub placeholder: SmolStr,
    pub multiline: bool,
    pub masked: bool,
    pub max_length: Option<usize>,      // grapheme cluster 数
    pub submit: Option<Action>,         // 单行 Enter
    pub text_style: TextStyle,          // 字号/行高/换行/对齐（含空行盒行高）
    pub caret_color: Color,
    pub selection_color: Color,
    pub placeholder_color: Color,
    pub composition_color: Color,
}

// 公开：只读快照（供 kit 镜像 Signal）
pub struct TextEditSnapshot {
    pub generation: u64,                // 单调递增；文本/选区/组合/焦点变化时 +1
    pub text: Arc<str>,                 // 已提交真实文本（掩码时为真实文本，a11y 另行映射）
    pub selection: TextSelection,       // 显示文本坐标
    pub composing: bool,
    pub focused: bool,
    pub can_undo: bool,
    pub can_redo: bool,
}

// 内部：引擎编辑内核（每个可编辑节点一个，存于 NodeTree 的 side table）
struct EditableText {
    committed: String,                  // 真实已提交文本
    composition: Option<Composition>,   // 预编辑：start/length/utf16 caret/clauses
    display: DisplayText,               // 显示文本缓存 + 真实↔显示 grapheme 映射（掩码/组合）
    layout: EditableLayout,             // parley Layout + 宽度 + 脏标记 + 断行缓存
    selection: parley::editing::Selection, // h_pos / anchor_base 在这里
    scroll: ScrollAnchor,               // ensure-visible / 拖拽自动滚动状态
    undo: UndoStack,                    // 区间增量 + 合并策略
    blink: BlinkState,                  // last_activity / visible（引擎全局时钟驱动）
    spec: EditableSpec,
    last_snapshot: TextEditSnapshot,    // 只在 generation 变化时克隆 Arc
}
```

**节点表示**：不新增 `NodeKind` 变体，保持 `NodeKind::Text`（`crates/ui_core/src/node/tree.rs`），在 `NodeTree` 增加与 `text_slots` 同形的 `editable_slots: Vec<Option<Box<EditableText>>>`。理由：

- 文本 pass / paint pass / a11y / automation 的通用文本路径继续以 `kind.text()` 判断，减少 ~15 处穷举 match 改动；
- 可编辑能力由 side table 存在与否激活（`NodeTree::editable(node)`），与现有 `text_slots` 的 "文本槽 + 节点种类" 组合方式一致；
- TextContent 保持为"显示文本的镜像"（用于 a11y/automation/probe），由引擎在快照更新时写入，不作为编辑事实源。

**为什么不用 `PlainEditor` 类型直接做节点**（D10）：`PlainEditor` 把 `buffer + Layout + compose` 绑在一起，且 `refresh_layout` 需要 `&mut FontContext`；而掩码输入需要显示文本与真实文本分离，多行测量需要像 `ShapedText` 一样在 taffy measure 闭包内按可用宽度重排（此时拿不到 `FontContext`）。因此引擎自带 `EditableText`（状态机语义参照 `PlainEditor` 与其测试），排版复用 `parley::Layout`，编辑原语全部复用 `parley::editing`。

### 4.3 文本布局与行盒

- `EditableLayout` 持有 `parley::Layout<ColorBrush>`、当前断行宽度、`dirty` 标记、`measure_memo`。
- **整形（shape）**：当 `committed`/`composition`/`masked`/`spec.text_style`/字体栈变化时，用 `TextContext` 的 `FontContext/LayoutContext` 构造 display `TextContent`（含组合下划线 run，见 §4.6）并 build layout，样式与现有文本节点完全一致。
- **断行（break）**：`EditableLayout::break_at(width)` 只调用 `Layout::break_all_lines(Some(width))`（不需要 FontContext），可在 taffy measure 闭包内调用——与现有 `ShapedText::measure` 的 `RefCell<ParleySnapshot>` 模式一致（`text/shape.rs:286-316`）。
- **空行盒**：`EditableText` 的 `TextStyle` 强制带一个确定行高：`line_height` 取「显式 `TextStyle.line_height`（px）」否则「字体 metrics 的可量化行高」。引擎在首个 shape 后缓存 `line_metrics`；空文本的测量/断行结果按"至少一行"返回 `(width=0, height=line_height)`，并为 raster 产生一行空 line 记录（`ShapedTextLine` 的 `text_range=0..0`、`baseline/line_height` 来自 parley 空 layout 的 strut 行）。**这是 BUG ② 的引擎级修复，也是 placeholder/IME/滚动步长的统一来源。**
- 行度量 API（公开）：`App::text_edit_metrics(window,node) -> Option<TextEditMetrics>`（`line_height / first_baseline / line_count / content_height`）；kit 用 `max_lines × line_height + 2×(border+padding_y)` 计算可视高度。
- 多行 viewport：kit 建 `root(fixed height) → viewport(overflow_hidden + scroll) → editable text node`；文本节点高度 = 完整内容高（不被 `max_lines` 截断），滚动内容 = 文本节点高度。单行：view 宽度 = 内容宽（`NoWrap`），横向滚动复用现有机制。
- **段落内组合 vs 掩码**：display 文本构造规则：
  - 普通：`prefix + preedit + suffix`；
  - 掩码：`bullets(prefix graphemes) + bullets(preedit graphemes) + bullets(suffix graphemes)`，每字素簇一个 `•`；
  - display→raw 映射按 grapheme 前缀长度查表（`SmallVec<[u32; 8]>` 前缀表 + 二分，避免每次 O(n)）。

### 4.4 事件路由与编辑命令

**路由原则**：所有编辑事件由引擎在分发阶段直接交给聚焦的 `Edit` 节点，不再进入 `App::drain_events` 队形（避免宿主重复处理与双重插入）。

- `app/resources.rs::dispatch`：对 `Event::Input`/`Event::Ime`，先让 `app.try_dispatch_edit_event(window, &event)`；返回 `true` 表示被可编辑节点消费 → 不 push 到事件队列（`InputEvent::Key` 里未被编辑消费的键仍走原逻辑：先 focus 激活/绑定，再 default scroll，再入队）。
- `App::dispatch_input`（`app/input.rs`）：
  - 指针：`on_pointer_down` 命中可编辑节点 → 聚焦 + 按 `click_count` 语义设置选区（1：caret；2：词；3：硬行）+ 进入引擎内拖拽选择态（pointer capture）；`on_pointer_move` 扩展选区（`Selection::extend_to_point`，保留 anchor_base 粒度）；`on_pointer_up` 结束；拖出 viewport 时按距离/时间滚动最近祖先 scroll 容器并重新扩展（自动滚动）。
  - 键盘：在 `on_key` 的"焦点激活/绑定"之后、`default_key_scroll` 之前插入 `text_edit::handle_key`；返回 consumed 则不执行默认滚动且不将事件入队。
  - 文本：平台 `InputEvent::Text` 在 `dispatch_input` 中交给 `text_edit::handle_text`（过滤控制字符、单行 `\r\n\t` 剔除、grapheme max_length 裁剪）。
  - IME：`Event::Ime` 交给 `text_edit::handle_ime`（compose 模型：Start/Preedit 更新显示；Commit 替换组合；End 取消；失焦取消）。
- 焦点变化：引擎在 `set_focus`/失焦时结束组合、结束拖拽、重置闪烁状态、清除/重设 IME cursor area。
- disabled/readonly：disabled 节点不可聚焦、不进入编辑路由；readonly 允许选区/复制/导航，拒绝 insert/delete/paste/undo-redo 文本变更。

**键盘映射（D9，Windows/Linux 为 Ctrl，macOS 为 Cmd/Alt）**

| 键 | 行为 |
|---|---|
| ←/→ | 视觉上一个/下一个字素簇（`Cursor::previous_visual/next_visual`，含 affinity） |
| Ctrl/Alt + ←/→ | 按词移动（macOS Alt；其它平台 Ctrl） |
| ↑/↓ | 上下行，保持目标列（`Selection::move_lines` 内置 `h_pos`） |
| PageUp/PageDown | 按 viewport 可视行数换算为 `move_lines(±n)`；n 由最近祖先 scroll viewport 高度 / 行高得到 |
| Home/End | 软行首/行尾；有 wrap 时按视觉行（与现有一致） |
| Ctrl+Home/End | 文档首/尾 |
| Shift+方向/Home/End/Page | 把 focus 扩展（保留 anchor 与 anchor_base 粒度） |
| Backspace/Delete | 有选区删选区；否则删前/后一个字素簇（`Cluster::text_range()`）；Ctrl/Alt 变体删一个词 |
| Ctrl/Cmd+A/C/X/V/Z/Y、Shift+Z | 全选/复制/剪切/粘贴/撤销/重做；剪贴板走 `App::set_clipboard_text/clipboard_text`；掩码禁止复制/剪切 |
| Enter/NumpadEnter | 多行插入 `\n`；单行派发 `spec.submit`；auto-repeat 不重复派发 |
| Escape | 组合期取消组合；否则折叠选区到起点 |
| Tab | 不消费（交给引擎焦点导航；不插入） |
| 其它可打印字符 | 走 `InputEvent::Text`（平台过滤后），不按 KeyCode 映射 |

**undo 合并（D7）**：以 `EditKind::{Insert,Delete,Replace,CompositionCommit,Paste,Programmatic}` + 上次编辑时间 + 选区连续性为界；连续 Insert（且无导航/选区改变）合并、连续 Backspace 合并；一次 IME 提交、一次粘贴各一步；上限 100；新编辑清空 redo。步骤保存 `range + removed: Arc<str> + inserted: Arc<str> + selection_before/after`，`InputState` 的克隆不再携带整段历史（见 §4.8）。

**程序化 API**：`App::text_edit_set_value(window,node,&str)`（清洗/裁剪/清 undo/光标末尾，语义对齐旧 `update_value`）、`App::text_edit_value`、`undo/redo`、`set_placeholder/colors`、`metrics`、`snapshot`。kit 的 `InputNode::{update_value,clear,update_placeholder,set_clearable,set_submit_on_blur}` 改为委托这些引擎 API。

### 4.5 IME 与平台修复

- **Windows 平台**（`platform/event.rs`）：
  - `WM_CHAR` 路径增加与 macOS/Linux 一致的过滤：`c.is_control()` 拒绝（包含 0x7F、0x80–0x9F）；当 `modifiers` 含 Control/Alt/Meta 时拒绝文本——**唯一例外是 AltGr（`CONTROL|ALT` 同按）**：此时放行可打印字符（否则波兰语/德语等 AltGr 布局无法输入），`META` 与单按 Ctrl/Alt 仍拒绝（实现在 `platform/event.rs`，登记见 §11.2 ⑤；本条文案同步见 P4-16）；`is_composing()` 时普通字符不产生 `Text`（只信 IME 事件）。
  - 补单测：`WM_CHAR 0x08/0x0D/0x1B/0x09`、Ctrl+字母、代理对与 emoji、与 IME 补发的交互。
- **引擎编辑入口防御**：`text_edit::handle_text` 统一拒绝 `is_control()`（多行仅放行 `\n`，Tab 不作为正文插入），与平台过滤形成双保险；`App::insert_text`/paste 同样清洗。
- **IME 状态机**（引擎内）：
  - `Preedit{text,caret_utf16,clauses,replace}`：`replace` 语义沿用现有约定（相对当前组合串的 UTF-16 区间，Windows/Wayland 为 None；macOS 相对 marked text）；更新 display、选区折叠到组合内 caret；
  - `Commit{text,replace}`：替换组合区间；按 max_length 裁剪；记为一步 undo；
  - `End`：取消未提交组合（丢弃 preedit，caret 回组合起点）；已 commit 后幂等；
  - 失焦/Enter/Esc：取消或提交组合的边界行为与现有测试一致（`End` 取消、提交后引擎补 End 幂等）；
  - 组合期间**不插入**普通 `InputEvent::Text`；组合期 caret 常亮、不闪。
- **IME cursor area**：引擎在每帧 layout pass 之后为聚焦可编辑节点写 `window.set_ime_cursor_area_text(node, byte)`，byte = 组合内 caret（未组合时 = 提交 caret）的 display 文本 byte；平台链路复用 `App::set_ime_cursor_area`/`Window::ime_cursor_area`（现有实现正确）。
- **候选窗 / 组合下划线**：diagnostics 的 `ColorGlyphMonochrome` 等保持；组合下划线在文本绘制阶段按 clauses 画（见 §4.6）。

### 4.6 绘制（文本节点自带编辑装饰）

单行/多行 text 节点在 paint pass 中的绘制顺序（同一节点内，天然正确分层）：

1. **选区矩形**（`Selection::geometry_with` → Quad；半透明主题色，画在字形下方）；
2. **字形**（现有 glyph run 绘制；彩色 emoji 为 RGBA 字形）；
3. **组合下划线**（按 `clauses` 的 UTF-16 区间映射到 display byte，使用行 metrics 的 baseline：`baseline + 0.10×font_size`、厚度 `1.0~1.5pt`，clause 类型可区分粗细；Canvas 不支持 decoration，用 Quad）；
4. **placeholder**（仅 committed 文本为空且无 composition 时，按首行行盒 origin + 同 style 绘制，颜色来自 `EditableSpec.placeholder_color`）；
5. **caret**（focus 且闪烁可见时；位置 = `Cursor::geometry`，颜色 = `caret_color`；有选区时仍绘制在 focus 端（D8））。

选区矩形/下划线/caret 的坐标从"节点 content-box local"换算：`在 paint 阶段使用节点 layout bounds + content_offset，并按祖先 scroll slot 平移（现有 paint 管线已支持 scroll uniform，这里直接使用未滚动布局坐标即可，与字形一致）`。

### 4.7 caret 闪烁（引擎）

- `App` 持有全局 `caret_clock: { last_activity: Instant, visible: bool }`（单一时间源，不做每个节点独立 timer）。
- 帧管线在 layout pass 后调用 `text_edit::sync_blink(app, window)`：
  - 有聚焦可编辑节点：半周期 530ms；若 `now - last_activity >= 530ms` 且不在组合期 → 翻转 visible、请求帧；否则按剩余时间安排下一次唤醒（引擎内部用现有 `timer::after` 或扩展 `next_wake_deadline`，不引入连续动画帧）。
  - 无聚焦/失焦：visible 保持 true（下次聚焦直接亮）、取消唤醒。
- 活动重置：任何 edit/navigation/pointer composition/text 事件都设置 `last_activity = now; visible = true`；组合期强制 visible = true 且不闪。
- 只重绘 caret 所在节点（`mark_paint(node)`），不重绘整窗；与现有 I3「稳态零帧」不冲突：无焦点时无唤醒；有焦点空闲时每 530ms 一次翻转帧（与系统 caret 行为一致，gallery `--idle-check` 不聚焦输入框，仍然 PASS）。
- 可访问性/系统设置：后续可读 Windows `GetCaretBlinkTime()`（非官方文档承诺的 530 默认值，作为增强登记）。

### 4.8 状态镜像与组件 API（B1）

`ui_kit::InputState` 重新定义为**镜像 + 业务态**：

```rust
pub struct InputState {
    text: Arc<str>,                 // 引擎快照（掩码也是真实文本；a11y/validate 用）
    selection: TextSelection,       // 引擎快照（display 坐标）
    composing: bool,
    focused: bool,
    can_undo: bool,
    can_redo: bool,
    error: Option<InputError>,      // 组件/应用业务态
    generation: u64,                // 用于等值短路
}
```

- 引擎快照 → `InputState` 的同步在 `InputNode::sync` 中完成（每帧一次，generation 不变则零成本）：读 `app.text_edit_snapshot(window,node)`，变化时 `app.write(&state, ...)`。
- 应用层读取不变：`app.read(&state).text()` 继续返回 `&str`（`Arc<str>` 的 `Deref`）。
- 应用层写入：**删除 `InputState` 的所有编辑方法**；6 处 `cx.write(&state, InputState::new(text))` 改为 `node.update_value(app, text)`（调用点见 §6.2）。`InputState::new(text)` 仅用于信号初始化；`install` 时把初始 text 推入引擎。
- `InputSpec` 扩展：

```rust
pub struct InputSpec {
    pub name: SmolStr,
    pub placeholder: SmolStr,
    pub masked: bool,
    pub multiline: bool,
    pub readonly: bool,
    pub disabled: bool,
    pub max_length: Option<usize>,  // grapheme 数
    pub visible_lines: Option<u16>, // 多行固定可视行数（D4）；None = 由宿主高度决定
    pub size: ControlSize,
    pub submit: Option<Action>,
}
```

- `InputNode` 保留公开只读面（`root/viewport/scrollbar/clear/state/spec`），删除 `text/placeholder/caret` 节点字段（已由引擎绘制；gallery/color_picker 的 probe 引用改为 `root` + 引擎 API）。
- 组件 sync 负责：主题（边框/背景/focus/error）、`visible_lines` 高度（`text_edit_metrics.line_height × n + 2×(border+padding_y)`；首帧未就绪时用 min 高度，第二帧生效）、viewport/scrollbar 内容尺寸与 ScrollBar.sync、ensure-visible 由引擎负责、placeholder 文本/颜色与 caret/selection 颜色通过 `set_editable_*` 更新、清空按钮显示、submit-on-blur、`Signal<InputState>` 镜像、a11y label/placeholder。
- 组件 install 不再注册 click/drag handler（引擎原生处理）；`release` 只释放 clear 按钮与窗口级动作。

### 4.9 多行、`visible_lines` 与滚动条（D4/D5）

- 结构：`root(fixed height) → viewport(overflow_hidden + scroll) → editable text node(width_full)`；`ScrollBar::new(..., Vertical, ScrollbarOptions::default().hover())` + `attach(viewport)`。
- 高度：`visible_lines = Some(n)` → root 高度 = `n × line_height + 2×(border + padding_y)`；文本节点不截断，完整内容高；`n` 行以外的内容通过 viewport 滚动查看。`None` → root `height_full + min_height(line_height)` 由宿主决定；溢出时同样滚动。
- 单行高度仍由 `ControlSize` 决定（Small/Medium/Large = 20/30/40）；宿主包装盒应与控件高度一致或让控件在包装盒内居中（gallery 当前 40px 包装盒 + Medium 30px 输入框顶部对齐的问题在 gallery 侧修正，不为此增加 `InputSpec.height` 字段）。
- 滚动条：仅 `content_height > viewport_height` 时可见；overlay 在右侧内边缘（4px，hover 淡入），文本节点右侧留 6px gutter（不参与换行，滚动条绘制在 gutter 内，不遮挡字形）。
- ensure-visible：引擎在 layout pass 后对聚焦节点的 caret/选区做 reveal（上下各留 1 个行高 padding；单行横向留 4px）；拖拽自动滚动由引擎 pointer 逻辑负责。
- `TextStyle.max_lines` 继续只用于「不可滚动文本截断」（markdown 等），**不用于输入框限行**（避免 §2.6 P6 的测量截断/截断后仍全量光栅化问题）。

### 4.10 彩色 emoji（E1）

**现状**：swash 0.2.10 已能把系统 emoji 字体渲染成 `Content::Color` 的 BGRA 位图（`scale/mod.rs:518-548` 读取 COLR v0 图层；`ColorBitmap` 读 sbix/CBDT 位图）；当前 `text/raster.rs:169-195` 用 `bgra_to_coverage()` 只保留 alpha，`text/atlas.rs` 字形图集是 R8，所以显示为单色。

**设计**：

1. 新增 `AtlasKind::ColorGlyph`（索引 2）与独立 RGBA 图集（1 层、页尺寸按现有图集策略；仅存彩色字形，单色字形仍在 R8 图集）。
2. `GlyphAtlas` 拆为单色/彩色双缓存（同一 `GlyphKey` 命名空间可共享哈希，记录 `RasterGlyph.color`）。
3. `RasterGlyph` 保留 BGRA→RGBA8（预乘在 shader 内完成），尺寸/placement 语义不变。
4. 绘制：`Primitive::GlyphRun` 的每个 `PositionedGlyph` 已带 `AtlasTile{atlas,...}`；实例编码按 tile 选择 `KIND_GLYPH` 或 `KIND_COLOR_GLYPH`。新增 shader 分支：`color_glyph_atlas`（RGBA，sRGB）→ `linear premultiplied = vec4(texel.rgb * texel.a, texel.a)`；三端 shader（GLSL/HLSL/MSL）与 `contract.md` 同步（新增 binding、kind=8、保留位说明）。
5. 上传：新增 color-glyph 图集页上传路径（与 glyph 页一致的 dirty rect/增长/重建/驱逐，页格式 RGBA8；上传到图集 texture array 的新 binding）。
6. 三端 backend：新增 descriptor 绑定与 `KIND_COLOR_GLYPH` pipeline 变体（D3D12/Metal/Vulkan 各自 pipeline 创建点 + 验证测试）。
7. 缓存/驱逐：彩色字形 LRU 与单色共用 pass 计数与 eviction 策略；图集满的软失败/重试/`AtlasExhausted` 逻辑复用；彩色页内存预算（例如单页 1024² RGBA = 4 MiB）单独登记。
8. 降级：字体无彩色数据（Linux 无 Noto Color Emoji）→ 仍走单色路径；只有 COLRv1 且无 v0 回退记录的字体（当前 Windows/Noto/Apple 都不是）→ 单色并记 `ColorGlyphMonochrome` 诊断（已有），作为已登记风险。
9. 测试：单测（BGRA→RGBA、tile kind 编码、图集驱逐）；像素快照（Windows Segoe UI Emoji 的 `🌍`/`👨‍👩‍👧`/旗帜；macOS Apple Color Emoji；Linux 有 Noto 时，无则 skip 并记诊断）；gallery 新增 emoji 输入演示。

**已验证事实（本机）**：`C:\Windows\Fonts\seguiemj.ttf` 的 COLR 表版本为 1，但保留 3487 条 v0 base glyph / 55497 条 v0 layer 记录；swash 的 v0 读取不校验版本，因此当前 swash 路径可以渲染系统 Segoe UI Emoji 的彩色（v0 回退层）位图；无需引入 FreeType 或实现 COLRv1 paint graph。COLRv1 专属渐变/变换细节会缺失（v0 回退近似），列入已知差异。

### 4.11 掩码、`max_length`、字素簇（D6）

- 所有编辑命令（插入/删除/移动/选区/计数）以 display 文本的 `Cluster`（=grapheme）为边界；掩码 display 每 grapheme 一个 `•`，display cluster 与真实 grapheme 一一对应，因此 display 上的 cluster 移动可直接映射到真实文本 grapheme 边界。
- `max_length` 按真实文本 grapheme 数计；插入/粘贴在 grapheme 边界截断（不拆簇）；超长程序化写入经由 `update_value` 裁剪并写 `TooLong` 错误（保留现有语义），用户输入不报错。
- masked 仅支持单行（与现有产品用法一致）；`masked && multiline` 在 `InputSpec` 校验（debug assert / 文档约束），避免组合与掩码的显示歧义。密码框 a11y 使用 `A11yRole::PasswordInput`（AccessKit 有该角色），value 暴露掩码串或空，不暴露明文。
- IME 组合在密码框内同样掩码（每个 preedit grapheme 一个 `•`），组合下划线保留。

### 4.12 可访问性

- 引擎自动维护可编辑节点的 `A11yPatch`：`value`（普通为真实文本，密码为掩码串）、`text_selection`（display 坐标 → UTF-8 byte，AccessKit `TextSelection{anchor,focus}`）、`placeholder`、`read_only`、`disabled`、`invalid`（由 kit 业务态透传）、`role/label`（来自 `EditableSpec`）。
- `A11yRole` 缺 `PasswordInput`（当前 43 个变体，`crates/ui_core/src/a11y.rs`），本批补角色 + AccessKit 映射（`a11y/mapping.rs`）并更新 `A11yRole::ALL` 测试；密码输入使用该角色。
- 组合期间不暴露 preedit（与 Chrome/Flutter 一致）；a11y 的 `SetTextSelection`/`ReplaceSelectedText` action 由引擎处理（AccessKit 已有动作枚举）。
- kit 的 `bind_a11y` 保留：错误态/标签 i18n/placeholder 运行期更新仍在组件层；value/selection 由引擎在前，kit 只覆盖业务字段（避免双写冲突的字段职责表见 §5.3）。

### 4.13 错误处理（强类型）

- 引擎新增 `EngineError` 变体：`TextEditNotAvailable { node }`、`TextEditSpecInvalid { node, detail }`（掩码多行、行高非正等）、`TextEditLayout { node, detail }`（shape/break 失败）、`ColorGlyphAtlasExhausted`（复用 `AtlasExhausted { kind }`）。
- 编辑命令在节点不可用/规格非法时返回 `Result`/`false`，不 panic；平台事件过滤失败只产生诊断，不阻断帧。
- 组件层业务错误仍是 `InputError::{Empty,TooLong,Invalid}`，i18n key 保持稳定；`TooLong` 由 `InputNode` 按引擎文本（清洗后字素簇）与 `InputSpec.max_length` 派生，业务校验（必填/格式/重复）由应用层读镜像自行判定。

---

## 5. API 规格

### 5.1 `ui_core` 新增/修改

| API | 签名（语义） | 说明 |
|---|---|---|
| `NodeBuilder::editable_text` | `fn editable_text(spec: EditableSpec) -> Self` | 构建 `NodeKind::Text` + 注册 `editable_slots`；不设 `bind_text` |
| `App::text_edit_set_value` | `fn text_edit_set_value(&mut self, window, node, text: &str) -> bool` | 程序化写值；清洗/max_length/清 undo/光标末尾（对齐旧 `update_value`） |
| `App::text_edit_value` | `fn text_edit_value(&self, window, node) -> Option<SmolStr>` | 真实已提交文本 |
| `App::text_edit_snapshot` | `fn text_edit_snapshot(&self, window, node) -> Option<TextEditSnapshot>` | B1 镜像读取；generation 变化才克隆 |
| `App::text_edit_undo/redo` | `-> bool` | 程序化撤销/重做 |
| `App::text_edit_metrics` | `fn text_edit_metrics(&self, window, node) -> Result<TextEditMetrics, EngineError>` | `line_height/first_baseline/line_count/content_height`（空文本也有值）；**布局脏但未 `compute_layout` 时返回上一完整帧的自洽旧值**（P4-05 契约收窄，不加 `DirtyFlags::LAYOUT` 判据）；就绪判据含 `text_dirty_pending`（P4-24 补咬合测试） |
| `App::text_edit_set_placeholder` | `fn text_edit_set_placeholder(&mut self, window, node, placeholder: &str) -> Result<bool, EngineError>` | 运行期 placeholder **文本**；颜色另走下一行（原 `set_text_edit_placeholder(text, color)` 组合 setter 已拆分，P4-35） |
| `App::text_edit_set_placeholder_color` / `text_edit_set_{caret,selection,composition}_color` | `fn …(window, node, color: Color) -> Result<bool, EngineError>`；`App::text_edit_placeholder_color(window, node) -> Result<Color, EngineError>` 为只读 getter | 运行期逐色更新（主题切换）；**没有** `EditableColors` 打包类型（P4-35） |
| `App::set_text_edit_submit` | —（无此面） | submit 是挂载期常量（`EditableSpec.submit`），**没有**运行期 setter（§11.2 ⑰） |
| `App::text_edit_dispatch_hint` | — | 不需要；事件由引擎自动消费 |
| `Window::text_edit_*` | — | 仅供参考/测试的内部读面，公开面统一在 App |
| `TextSelection/TextCursor/TextAffinity/TextEditSnapshot/TextEditMetrics` | 公开类型 | 不暴露 parley 类型；`TextSelection` 内部包装并保留 h_pos/anchor_base |
| `EngineError` 变体 | 见 §4.13 | 强类型 |
| 平台修复 | `platform/event.rs` WM_CHAR 过滤 | 见 §4.5 |
| 渲染 | `AtlasKind::ColorGlyph`、`KIND_COLOR_GLYPH=8`、`color_glyph_atlas` binding、三端 pipeline | 见 §4.10；`render/shaders/contract.md` 同步 |

### 5.2 `ui_kit` 公开面修改

| 类型/方法 | 变化 |
|---|---|
| `InputSpec` | 新增 `visible_lines: Option<u16>`；`max_length` 单位改为 grapheme（文档） |
| `InputState` | 变为镜像 + 业务态；删除编辑方法（insert/delete/move/set_caret/select_word/undo/redo/…）；保留 `text/is_focused/is_composing/selection/caret/error/validate/set_error/can_undo/can_redo` 只读面 |
| `InputNode` | 新增 `scrollbar: Option<ScrollBar>`；删除 `text/placeholder/caret` 节点字段；`dispatch` 删除（宿主不再转发文本事件）；`install/sync/release/update_value/clear/update_placeholder/set_clearable/set_submit_on_blur` 保留（语义不变，内部委托引擎） |
| `input::build` | 签名保留（`tree,parent,spec,theme,&state,&placeholder`），内部构建新结构；`install` 时把 `state.text()` 推入引擎 |
| 删除 | `CaretMotion`（引擎内部化）、`InputState` 的 14 个编辑方法、组件内三套坐标换算/caret canvas/placeholder 节点/undo 快照、`kit-text.md §4.2` 已登记遗留项中可由引擎解决的条目 |

> **实际落地（P4-35 登记；P4-24 见 §5.1 metrics 行）**：本表与 §5.1 的**组合 setter 全部按单一职责拆分**——
> `InputSpec` 除 `visible_lines` 外另有 `label: Option<SmolStr>`（§11.2 ⑭）；`InputState` 不再有 `validate`
> （§11.2 ㉘）与编辑方法；`InputNode` 的运行期 setter 为 `update_value` / `update_label` / `update_placeholder` /
> `clear` / `set_clearable(&self, …)` / `set_submit_on_blur`，**没有**任何一次调用同时改文本与颜色/动作的组合面
> （`text_edit_set_placeholder` 只管文本、颜色走 `text_edit_set_placeholder_color`；submit 无运行期 setter，见 §11.2 ⑰）。
> 最终公开面以 §11.3 为准。

### 5.3 职责边界（防止双写）

| 数据 | 唯一写者 | 读法 |
|---|---|---|
| 真实文本 / display / selection / compose / undo / caret / 闪烁 / 滚动 | `ui_core::text_edit` | `App::text_edit_*`；kit 镜像到 `Signal<InputState>` |
| `InputState.error`、submit-on-blur 开关、clearable 按钮 | `ui_kit::input` | 应用层 `read(&state)` |
| border/背景/focus/error 边框、placeholder 文本/颜色、caret/selection 颜色、`visible_lines` 高度、ScrollBar | `ui_kit::input`（通过引擎 setter） | 组件 `sync` |
| a11y value/selection/placeholder/readonly/disabled | 引擎自动；kit 只写业务字段 | AccessKit 快照 |
| 业务校验（必填/格式/重复） | 应用层（读镜像） | 应用层自判 + `App::set_a11y_invalid` 回写 |

---

## 6. 实施计划（分阶段，含验收）

### P0 — 平台输入修复 + 引擎空行盒（先止血，独立可验收）

1. `platform/event.rs` 的 WM_CHAR 控制字符/修饰键过滤 + 单测矩阵。P0 与 P1 同批实施时，引擎编辑入口的 `is_control` 防御直接落在 P1 的 `text_edit` 里；旧组件的 `clean_single_line` 在 P2 被整体删除，不存在需要长期保留的兼容层。
2. 引擎空行盒：`TextStyle`/`EditableSpec` 行高确定化 + 空文本 measure/raster 行记录 + `text_edit_metrics`；单测：空文本 measure 高度 = 行盒、caret rect 在行盒内、`byte_index_at` 对空文本返回 0。
3. Windows 单测：`WM_CHAR 0x08/0x0D/0x1B/0x09/Ctrl+字母`、IME 补发与过滤交互。
4. 验收：`cargo test -p ui_core --lib` 新老用例全绿；`cargo test -p ui_kit --lib input::` 现有 69 条不回归（组件路径暂未改）；gallery Windows 实机手动验证 Backspace 能删除、Ctrl+V 不产生方块。

### P1 — 引擎可编辑文本节点内核

1. `ui_core::text_edit`：EditableText 数据模型、display 映射、parley layout/selection/compose、编辑命令、撤销栈、快照、metrics、错误类型。
2. 节点 side table、text pass/paint pass 分支、空行盒、选区/组合下划线/caret 绘制、placeholder 绘制。
3. 事件路由：pointer 点击/拖选/双击/三击/shift、keyboard 映射、Text/IME 消费；`default_key_scroll` 跳过可编辑节点。
4. IME cursor area、组合状态机、失焦/取消边界。
5. 单测：parley 语义等价（移动/词/行/上下行/h_pos/affinity/字素簇删除）、undo 分组、max_length 字素、掩码映射、空文本行为、事件消费。
6. 验收：引擎单测全绿；`ui_core` 编译/clippy/fmt；新 gallery 最小示例（引擎直连可编辑节点，无 kit）打字/IME/拖选可用。

### P2 — `ui_kit::input` 薄壳重写 + B1 镜像

1. `InputState` 镜像 + `InputSpec.visible_lines`；`InputNode` 新结构；`sync` 同步镜像/主题/高度/placeholder/颜色/清空按钮/submit-on-blur；`build/install/release`。
2. 迁移 18 个调用点（`InputSpec` 字段、6 处 `write(InputState::new)` → `update_value`、probe/节点字段引用）。
3. 删除旧状态机/坐标换算/caret canvas/placeholder 节点/`CaretMotion`。
4. 单测迁移：现有 69 条按新语义改写（编辑行为测试移入引擎层；组件层保留外观/镜像/清空/提交/主题/a11y）；新增 BUG 回归测试。
5. 验收：`cargo test -p ui_kit --lib` 全绿；`cargo test` 工作区；gallery 第 10 节四个输入框实机行为正确。

### P3 — 多行固定行数 + 滚动条 + 闪烁

1. `visible_lines` 高度、viewport/滚动内容/ScrollBar overlay、gutter、ensure-visible、拖拽自动滚动。
2. caret 闪烁时钟 + 唤醒调度 + 组合/失焦/选区规则。
3. 验收：多行 3 行可视 + 溢出滚动条；Flutter/gallery idle-check（无聚焦时 0 帧）PASS；闪烁实机验收（530ms、有选区仍闪、组合常亮）。

### P4 — 彩色 emoji（E1）

1. 彩色字形图集 + RGB(A) 上传 + 三端 pipeline/shader/contract + 实例编码。
2. 单测/像素快照（Windows 必测；mac/Linux 条件跑）；gallery emoji 演示。
3. 验收：三端实机彩色 emoji；无 emoji 字体环境降级单色；像素快照通过。

### P5 — 收尾

1. 删除全部旧代码/字段/依赖（AGENTS §1.1/§4）；更新 `.docs/gui-migration/spec-deltas/kit-text.md` 遗留清单状态。
2. 全量 `cargo test`、`cargo +nightly clippy -- -D warnings`、`cargo +nightly fmt`（AGENTS §8）；gallery 三端实机（Windows 必测，mac/Linux 按现有验收流程）。
3. 性能与不变量：idle 零帧、无额外连续帧、文本打字路径无 O(历史×正文) 克隆、shape cache 命中率不明显下降。

---

## 7. 测试计划

### 7.1 四个 BUG 的回归用例（必须红→绿可验证）

1. **placeholder 与文本行盒同 y**：构建输入框（空态、有字态）后取引擎行盒 metrics 与 placeholder/caret 矩形，断言 `placeholder.baseline == text.baseline`、垂直居中误差 ≤ 0.5px；覆盖 Small/Medium/Large 单行 + 多行。
2. **空文本 caret 在行盒内**：空文本、聚焦、无组合时 caret rect 高度 = 行盒高且中心 = 视口中心（单行）/首行（多行）；布局不可用时不得回退到节点盒（改为引擎保证）。
3. **滚动页面内拖选**：在带 ancestor scroll 的容器内挂输入框，滚动后从文本中部拖到末尾，断言 anchor = 起点 byte、focus = 终点、选区范围正确；再滚回 0 复测（防止再次坐标混用）。
4. **平台控制字符**：`WM_CHAR 0x08` 不产生 Text 事件；Backspace 只删 1 字素；`Ctrl+V` 后无 0x16；多行 Enter 只产生 `\n`；Ctrl+A 不插入 0x01；Esc 不插入 0x1B。

### 7.2 编辑内核用例（引擎层）

- 字素簇：`e + U+0301`、`👨‍👩‍👧`、`🇨🇳`、`👍🏽` 的 ←/→/Backspace/Delete/双击/选词/选区矩形/`max_length`。
- affinity：软换行处点击/拖选/选区几何；双向文本（RTL 段落/run）下的左右移动与 Home/End。
- 上下行：`h_pos` 跨多次按键保持；短行回到长行时列记忆；PageUp/Down 与 viewport 行数换算；Ctrl+Home/End。
- 双击/三击/Shift+点击/双击后拖拽按词/行扩展；拖出控件自动滚动。
- IME：Preedit/Commit/End/replace 子区间、组合中 caret、取消/失焦、Windows 补发 WM_CHAR 不重复、macOS replace 区间、Wayland 无 replace。
- undo 分组：连续输入合并、导航打断、IME 一次一步、粘贴一步、上限 100、redo 清空。
- 掩码：每字素一个 `•`、caret/选区映射、复制/剪切禁止、IME 掩码、a11y 不泄漏明文。
- 空文本：`byte_index_at`、句首插入、空行盒、placeholder 显示条件。
- 彩色 emoji：RGBA tile 编码、图集驱逐/重建、像素快照。

### 7.3 组件层用例（ui_kit）

- 现有 69 条按新架构改写后全绿；新增：镜像 signal 与引擎快照一致、`update_value` 清 undo、`update_placeholder`、清空按钮显示/隐藏、submit-on-blur 一次、主题切换重绘颜色（输入框/placeholder/caret/selection）、a11y value/selection/role、`visible_lines` 高度与滚动条显隐、focus ring。
- gallery 第 10 节：单行/掩码/禁用/多行（3 行）+ IME 演示；新增 `--probe` 打印 input/text/placeholder/caret 几何用于实机像素核对；`--idle-check` 保持 PASS。

### 7.4 不变量与回归门禁（AGENTS §8 + 现有 I3）

- 稳态（无聚焦、无动画）不得持续出帧（gallery `--idle-check` ≤2 帧/3s）。
- 有焦点空闲时闪烁最多每 530ms 一帧（不得变成连续动画帧）；组合期不闪。
- 旧测试中属于"旧架构行为"的用例（如 `dragging_*` 手工坐标、`caret_canvas_*`、`InputState` 编辑方法测试）按新架构迁移或删除，不保留兼容 shim。
- 变异测试（至少 4 项）：空行盒高度改为 0、WM_CHAR 过滤去掉、拖选坐标改回旧公式、闪烁不请求唤醒 —— 对应回归测试必须变红。

---

## 8. 迁移与删除清单

### 8.1 删除（不做兼容层）

- `InputState` 的编辑 API 与实现：`insert/delete_backward/delete_forward/insert_newline/select_all/set_caret/move_caret/ime_*/copy/cut/paste/undo/redo/select_word/select_line/begin_drag/end_drag/drag_to/set_caret_from_point/handle_key/handle_ime` 及 `CaretMotion`。
- 组件内：`Composition/EditSnapshot/EditUndo` 快照、三套坐标换算（`text_local_point/to_text_local`）、`caret_canvas/apply_canvas/caret_canvas 旧实现`、placeholder 节点、`clean_single_line`（迁入引擎）、`utf16` 辅助（迁入引擎）、`word_range_at/word_class`（由 parley word boundary 替代）、`masked_offset`（由引擎 display 映射替代）。
- `InputEvent::Text` 经宿主转发到 input 的路径（组件 `dispatch` 删除）；gallery/widget_gallery 的输入事件转发代码删除（继续保留其它控件 dispatch）。

### 8.2 调用点改造（18 处 `input::build` 调用）

| crate / 文件 | 改动 |
|---|---|
| `ui_kit/examples/gallery.rs` | 新结构、`visible_lines` 多行演示、emoji 演示、probe 节点名 |
| `ui_kit/src/kit/alert.rs` | `InputFactory` 类型不变；`InputSpec` 字段更新 |
| `ui_kit/src/kit/color_picker.rs` | HEX 输入 `submit` 不变；probe 字段引用 |
| `settings_ui/src/settings.rs`、`pages/proxy/editor.rs`、`pages/venues/credentials.rs`、`pages/translation/page.rs` | 搜索/凭据输入；`update_placeholder`/`set_clearable`/`submit_on_blur` 语义保持 |
| `instrument_selector_ui/src/{instrument_selector,prediction_selector}.rs` | 关键：`query` 读 `state.text()` 保持不变（B1）；`finish` 的 `release` 保留 |
| `kytrix/src/shell/dialogs.rs` | `InputSpec` 字段更新 |
| `widget_gallery_ui/src/gallery.rs` | 移除 input 事件转发；搜索框 `update_placeholder`、Enter 确认改走 `submit` Action |
| `widgets/{x,financial_index,ethereum_wallet,solana_wallet,hyperliquid_wallet,candlestick}/…` | 6 处 `cx.write(&state, InputState::new(text))` → `input.update_value(app, text)`；其余字段更新 |
| 各 crate 的 `InputSpec` 字面量 | 新增 `visible_lines: None` 等字段 |

### 8.3 文档更新

- `.docs/gui-migration/spec-deltas/kit-text.md §3/§4.2`：更新为"编辑内核已下沉 ui_core、§4.2 遗留已关闭/迁移"。
- `.docs/gui-migration/spec-deltas/kit-gallery.md`：输入框探针/主题检查条目。
- `render/shaders/contract.md`：彩色字形 kind/binding。
- 本文档作为实现阶段依据。

---

## 9. 风险与开放问题

| 风险 | 影响 | 缓解 |
|---|---|---|
| B 架构改动面大（引擎新增编辑内核 + 事件路由 + 绘制 + 彩色 emoji） | 工期与回归风险 | 分 P0–P5 交付；每阶段可独立验收；先落地 P1 引擎能力再切组件 |
| 输入事件不再经宿主（`drain_events`） | 少数宿主/测试依赖事件队列观察文本输入 | 迁移点已列（widget_gallery/测试）；保留非编辑事件队形；`automation` 注入面同步更新 |
| 彩色 emoji 依赖 swash v0 回退层 | 未来 COLRv1-only 字体显示单色 | 已登记；后续可选 FreeType/COLRv1 路径；Windows 当前字体已实测有 v0 记录 |
| 掩码输入的 IME/词移动语义 | 与成熟框架不完全一致（Qt/Flutter 按 UTF-16） | 产品文档明确"按字素簇"；测试覆盖 emoji 掩码 |
| 多行 viewport 与引擎 ensure-visible 的滚动时序 | 滚动/候选窗一帧抖动 | 引擎在 layout pass 后统一 ensure-visible/IME 定位，同帧生效；gallery 实机验收 |
| 530ms 与系统 `GetCaretBlinkTime` 不一致 | 与系统设置不同步 | 后续增强读取系统值；当前默认值在文档登记 |
| PageUp/Down、双击拖拽扩展等新增行为改变现有测试 | 测试迁移成本 | 测试计划已列；按新语义重写而不是保留旧断言 |
| a11y 选择/组合暴露 | AccessKit 无 preedit 字段 | 组合期不暴露；password 不暴露明文 |

**开放问题（不阻塞实现，本设计已取默认值）**：

1. Windows 系统 caret 闪烁时间读取（`GetCaretBlinkTime`）是否在 P3 一并接入（默认 530ms）。
2. 彩色字形图集页尺寸/预算（初版 1024² RGBA 单页 + 驱逐，按实机 emoji 数量评估）。
3. Home/End 的 Chrome "缩进后二次 Home 到 0" 行为（外部调研未核实到源码，本设计不实现，登记可选）。
4. `visible_lines` 之外是否提供 `height` 覆盖（当前由宿主高度 + `visible_lines` 两者表达，暂不再加字段）。

---

## 10. 验收标准

1. 四个实机 BUG 全部消失（gallery Windows 实机 + 回归测试）：
   - placeholder 与文本首行行盒对齐（各尺寸误差 ≤0.5px）；
   - 空文本/清空后首次聚焦 caret 在行盒内（不再下半截）；
   - 任意滚动页面内从文本中部拖选锚点正确、方向正确；
   - Backspace 删除正常、中文不再出豆腐块、Ctrl/Enter/Esc 不污染文本。
2. 三项需求：
   - emoji 可输入/显示/整簇删除/整簇移动/整簇计数；彩色显示（三端系统 emoji 字体）；
   - 多行固定 `visible_lines` 行数，溢出出现 overlay 滚动条并可滚动；
   - caret 530ms 闪烁，输入重置、组合常亮、失焦隐藏、有选区仍显示。
3. 成熟行为（D9）全部可用并通过引擎测试；
4. 状态模型：应用层 `read(&state).text()` 行为不变；6 处直接写 state 全部迁移；
5. 质量门禁：`cargo test`、`cargo +nightly clippy -- -D warnings`、`cargo +nightly fmt` 全绿；gallery `--idle-check`/`--theme-check` 三端 PASS；无新增 `#[allow]`/注释/临时兼容层；
6. 性能：打字路径无整份历史克隆；稳态无额外连续帧；文本 shape 缓存命中率不下降（对比基线记录在验收日志）。

---

## 11. 实施结果与偏差登记（P5 回写）

- 状态：**已实施**。P0–P4 全部提交并合入 `main`；本轮基线 `bbcb24cd`（本轮起点）→
  `cb965b56`（P4 merge，`main` HEAD）。本节是唯一权威的「设计与实现不一致」清单，
  §3–§5 的 API 规格与 §6 的分阶段计划保持历史原样（作为依据留档）；例外是 §4.5（AltGr 例外，P4-16）与
  §5.1/§5.2（组合 setter 的实际拆分，P4-35）——这两处按实现回写。
- P4 遗留清零轮（基线 `ead8cdcb`）的处置、WONTFIX 21 条与 14 条裁决见 **§11.11**；偏差编号 ㉙–㊽ 为本轮新增。
- 证据：`.review/text-input/p5-*.log`（门禁/变异/性能/三端），gallery 截图
  `.review/text-input/p5-gallery-inputs-emoji.png`。

### 11.1 提交序列

| 阶段 | 提交 | 内容 |
|---|---|---|
| P0 | `a3888798` | Windows `WM_CHAR` 控制字符/修饰键过滤 + AltGr 例外 |
| P1a-1 | `1f52c099` | `EditableText` 节点 side table、空行盒布局、measure/命中基础 |
| P1a-2 | `8a558517` | display 映射（掩码/组合）、`TextSelection`、`TextEditSnapshot` |
| （预修） | `434af283` | Linux/macOS 验收预修（导入、cfg 门控、size 基线） |
| P1b-1 | `75e426a6` | 编辑内核（插入/删除/移动/undo）、键盘与 `InputEvent::Text` 路由、`App::text_edit_*` |
| P1b-2 | `8df38db9` | IME 组合状态机、组合 display、`A11yRole::PasswordInput` |
| P1c-1 | `bb732834` | 选区/组合下划线/caret/placeholder 绘制、IME cursor area |
| P1c-2 | `7e2821fd` | 指针路由（点击/拖选/双击/三击/自动滚动）、`examples/editable.rs` |
| 小 API | `feee4458` `8de025b5` `08b901f8` `c287d649` `af072f78` `a6f80ea3` | `focused_editable`、Escape 语义、setter 补齐等 |
| P1b-3 | `2b70fef2` | AccessKit `TextRun` 子节点模型、选区/动作、密码角色映射 |
| P2 | `dfbf4f9c` | `ui_kit::input` 薄壳重写 + 18 处调用点迁移；删除旧状态机 |
| P3 | `589a462a` | caret 闪烁时钟/唤醒、ensure-visible、多行 viewport + overlay ScrollBar |
| P4 | `eee0f04e` + merge `cb965b56` | 彩色字形 RGBA 图集、`KIND_COLOR_GLYPH`、三端 pipeline/shader/contract、彩色 emoji golden |

### 11.2 已登记的设计偏差（①–㊽）

| # | 偏差（相对本文 §3–§5） | 落地 |
|---|---|---|
| ① | `EditableText` **复用** `ShapedText`/`text_slots` 的同一布局源，不新建 §4.3 的独立 `EditableLayout`；断行/measure 走既有 `text_pass` 缓存（`shape_cached_editable` / `measure_editable`） | `1f52c099` `75e426a6` |
| ② | `App::text_edit_*` 返回 `Result<T, EngineError>`（§5.1 写的是 `Option`/`bool`）；错误语义按 §4.13 强类型化（`TextEditNotAvailable`/`TextEditSpecInvalid`/`TextEditLayout`） | `75e426a6` |
| ③ | 空文本**不**补 raster 的 `ShapedTextLine` 空行记录（§4.3 曾要求）：空文本行盒由 parley 的 strut 行 + `measure_editable` 在 measure 层给出，raster 侧没有额外合成行 | `1f52c099` |
| ④ | `TextSelection` **不内嵌** parley `Selection`：自有 `TextCursor{byte,affinity}` + 私有 `SelectionBase{Cluster,Word,Line}` 记录扩展粒度；parley 类型只在 crate 内瞬时构造 | `8a558517` `2b70fef2` |
| ⑤ | AltGr（`CONTROL|ALT` 同按）在 `WM_CHAR` 过滤里**例外放行**可打印字符（否则波兰语/德语等 AltGr 布局无法输入）；`META` 与单按 Ctrl/Alt 仍拒绝 | `a3888798` |
| ⑥ | 组合下划线**粗细按 clause 索引奇偶**交替（`ImeEvent` 只给 UTF-16 区间，没有 clause 类型，无法按主/子句区分） | `bb732834` |
| ⑦ | `auto` 宽度节点的 placeholder 宽度回退到**空文本的 max-content**（`TextStyle` 无可用宽度时无第二布局源） | `bb732834` |
| ⑧ | 窗口失焦后若仍出帧，可能重写一次 IME cursor area（没有 window-focus 门控；桌面 IME 不消费该矩形，无可观测影响） | `bb732834` |
| ⑨ | a11y 的**选区与文本动作**分工：`TextSelection` 属性挂在**输入节点**上，其 `TextPosition.node` 指向虚拟 `TextRun` 子节点（不是输入节点自引用）；`TextRun` 子节点承担文本内容/字符度量并宣告自己的 `SetTextSelection`；输入节点另外宣告 `SetTextSelection`（有 TextRun 子树时）与 `ReplaceSelectedText`（可编辑、非只读/非禁用），且可编辑节点**不**宣告 `SetValue` | `2b70fef2` |
| ⑩ | `readonly` 输入框**消费** IME 事件但不显示 preedit（不进入组合，避免出现可编辑假象） | `8df38db9` |
| ⑪ | `masked ⇔ A11yRole::PasswordInput` 是**单源硬约束**：`EditableSpec::validate` 拒绝 masked 非密码角色、密码角色非 masked、masked+multiline；不提供运行期切换 | `8df38db9` |
| ⑫ | preedit 更新的复杂度是 **O(committed + preedit)**（重建 display 串与映射表），但**不随帧重建**：只在 text/selection/composition/样式变化时重建 | `8df38db9` |
| ⑬ | 多行 placeholder 在父节点**没有 Clip** 时可以溢出（引擎不额外裁剪 placeholder 行） | `bb732834` |
| ⑭ | `InputSpec.label: Option<SmolStr>`：§4.8 的字段表漏了它，但 a11y/`InputNode::update_label` 需要运行期可改的标签 | `dfbf4f9c` |
| ⑮ | `App::text_edit_set_value` 是**程序化**入口，绕过 `readonly`/`disabled`（对齐旧 `set_value` 契约）并清空 undo；`readonly` 只拒绝用户编辑 | `dfbf4f9c` |
| ⑯ | `update_value` 触发 `InputError::TooLong` 时，长度按**清洗后**的字素簇计（单行先剥 `\r\n\t`，再与 `max_length` 比较） | `dfbf4f9c` |
| ⑰ | 单行 `Enter` 的 `submit` 动作**没有运行期 setter**（挂载期常量，写在 `EditableSpec.submit`）；`InputNode::set_submit_on_blur` 是组件层开关，不是引擎动作 | `dfbf4f9c` |
| ⑱ | 多行 overlay 滚动条的**鼠标命中区**：hover 时只有 4 px 宽轨道可命中，聚焦时轨道**半透明常驻**（不是悬停才出现）；滚轮滚动与 hover 淡入都是引擎/kit 的 overlay 语义，不占文本宽度 | `589a462a` |
| ⑲ | 单行水平 reveal 的可视区右边界需要 kit 侧**尾部 5 px inset**（给 caret 1 px + AA 留白），否则贴边字符的 caret 会被内容盒裁掉 | `589a462a` |
| ⑳ | 多窗口共享**单一全局** caret 闪烁时钟（`App::caret_blink`，owner 模型：持有 owner 的窗口独占翻转与唤醒，其它窗口活动不延迟 owner） | `589a462a` |
| ㉑ | 内容不可滚动时滚动条 **track 的 4 px 命中区仍然保留**（禁用态视觉隐藏但命中用于 hover 反馈），不改成 0 宽 | `589a462a` |
| ㉒ | 新增公开观测/校正面：`App::next_wake_deadline`（下一次唤醒时刻）、`App::text_edit_caret_visible`（闪烁当前相位）、`App::set_scroll_line_height`（滚动步长校正）、`App::focused_editable`（当前聚焦可编辑节点） | `589a462a` `af072f78` |
| ㉓ | 彩色字形**不乘** `color0`（保留 emoji 自身颜色）；单色字形仍 `color0 × texel.r`。语义写入 `render/shaders/contract.md` | `eee0f04e` |
| ㉔ | 像素 golden 依赖**宿主 emoji 字体版本**：Windows 基线来自 Segoe UI Emoji（`crates/ui_core/tests/golden/warp/color_emoji_1_light.png`）。Segoe UI Emoji 无国旗字形（`🇨🇳` 渲染成两个字母/单色），Linux 全局字体栈不含 emoji 家族时部分 emoji 走单色轮廓（实测见 §11.7） | `eee0f04e` |
| ㉕ | COLRv1 只有**单色回退**（swash 0.2.10 只读 COLR v0 记录）：Segoe UI Emoji 幸运地保留 3487 条 v0 base glyph，故 Windows 彩色可用；仅 v1 的字体退回单色并记 `ColorGlyphMonochrome` 诊断 | `eee0f04e` |
| ㉖ | **caret 矩形按行盒归一**：parley 的 `Cursor::geometry` 返回 `min/max(block, content)` 的并集（Linux 字体下可高于行盒：20 行盒 vs 22 caret），引擎在 `inline_flow::caret_rect` 里把 caret 吸附到**所在行盒**（top = 行顶、height = `LineMetrics.line_height`），保证「行盒 / caret / 占位符 / 滚动步长 / IME 候选框」同一口径（§3.1 约束），并让 ensure-visible 的「留 1 行」在任意字体度量下可达 | P5 收尾（本篇 §11.9） |
| ㉗ | 输入控件**整盒可点**：引擎新增 press 宿主代理（后续 ㊳ 收敛为 `NodeTree::set_press_focus_target`；press 落在该节点或其后代上时，若没有更近的焦点目标，则聚焦它承载的 editable；该节点不进入 Tab 序、不改变「点控件外失焦」语义）；`input.rs` 的 root 用它代理 padding/border，`kit::ScrollBar` 新增 `ScrollbarOptions::tab_stop(bool)`（输入框的 track/thumb 置 false，动作与拖拽不变）。恢复旧组件「整盒可点」行为，同时每个输入仍只有 editable 一个 Tab 停靠点。**P4-10 起该能力改为显式目标 `NodeTree::set_press_focus_target(host, target)`（建树后回填），见 ㊳** | 终审 round 1 修复（P3-1） |
| ㉘ | `InputState::validate` **删除**：字段错误只有单一来源——`InputNode` 镜像按引擎文本（清洗后字素簇）+ `InputSpec.max_length` 派生 `TooLong`，业务错误仍由应用层 `InputState::set_error` 注入；`validate` 在生产代码 0 消费者（§5.3 应用层校验未落地），删除避免「按镜像文本 / 按请求文本」双源语义 | 终审 round 1 修复（P4-16） |
| ㉙ | 彩色字形图集 tile 在上传时**复制 1 纹素边缘色**（padding 语义）：straight-alpha 纹素与 `FILTER = LINEAR` 在分数设备位置/缩放下不再出现「双线性 × straight alpha」导致的边缘偏暗；`render/shaders/contract.md` 增补彩色图集的 padding/纹素语义条目，Windows WARP 的 `golden/color_emoji_1_light.png` 随实现重出 | P4-01（§11.11.3 裁决 1） |
| ㉚ | 像素用例**缺 emoji 字体 / 无呈现后端默认硬失败**：`color_emoji_glyphs_render_in_color` 与 `golden_color_emoji` 不再「打印 skip 后 return」；只有显式置 `KYTRIX_TEST_SKIP_GPU_PIXELS=1` 才允许跳过，且必须打印可读 reason（用途、限制与更新流程见 `crates/ui_core/tests/golden/README.md`） | P4-02（裁决 2） |
| ㉛ | golden 维持**单份**彩色 emoji 基线（仅 Windows WARP 建 `color_emoji`；`lavapipe/` `metal-real/` 不建）：跨设备字体不同必然分叉，故不按「设备 + 字体版本」分目录；改为在 `tests/golden/README.md` 记录设备 / 驱动 / 字体名与版本 / 分辨率缩放，字体版本变化按「有意变更」走重基流程 | P4-03（裁决 3） |
| ㉜ | **readonly 单行输入 Enter / Ctrl+Enter 一律不提交**：单行 `Enter`/`NumpadEnter` 分支在 `readonly` 时既不发 `submit` 也不改文本（旧实现直通 `handle_enter` 且不看修饰键，Ctrl+Enter 同样提交）；readonly 仍允许选区 / 复制 / 导航，多行 Enter 在 readonly 下也不插入 `\n` | P4-04（裁决 4） |
| ㉝ | `App::text_edit_metrics` 契约**收窄**为「布局脏但未 `compute_layout` 时返回**上一完整帧**的自洽旧值」（该值此刻与 raster/几何一致）；不新增 `DirtyFlags::LAYOUT` 判据、不改实现，调用方（kit `visible_lines` 高度、首帧回退）不依赖「当帧精确」 | P4-05（裁决 5） |
| ㉞ | auto-width 输入的 placeholder「内容宽 0 → 自然宽」分支**删除**（判据 `width > 0` 在 auto-width 下恒假，实际总是走 `expected_layout_width` 的 max-content 回退；AGENTS §1.2）；placeholder 宽度仍按偏差 ⑦ 的「空文本 max-content」 | P4-06（裁决 6） |
| ㉟ | IME 光标区写入的**可见性口径统一**：聚焦 editable 必须同时满足 `opacity > 0`、非零尺寸、未被祖先 scroll/clip 完全裁剪，才写 `set_ime_cursor_area`；否则不写（候选窗不再定位到不可见位置）。该口径**只用于 IME 定位**，paint / hit-test / a11y 的既有可见性判定不变（那部分另立任务） | P4-07（裁决 7） |
| ㊱ | `stale_text` / `line_is_live` 修复路径加**去抖记账**：同一节点在同一帧内只记一次「需修复」，避免逐 tile LRU 淘汰导致每帧 `invalidate_text_line + request_frame` 的理论活锁 | P4-08（裁决 8） |
| ㊲ | `MissingLabel` 诊断**纳入「有 value 无 label」**：条件不再是 `label.is_none() && value.is_none()`（该条件因 `EditableText::a11y_patch` 恒填 value 而永不触发）；未加 label 的输入框现在会报该诊断 | P4-09（裁决 9） |
| ㊳ | press 宿主**显式目标 + 按点落 caret**：`NodeTree::set_press_focus_target(host, target)` 由宿主显式指定**唯一**目标 editable（builder 带参版本因 root 先于 editable 构建会成死 API，最终改为 build 后回填）（旧的「DFS 取子树第一个 editable」隐式解析删除），按下时聚焦 `target` 并按落点置 caret；契约约束「一个 press 宿主显式指定一个目标」，多个候选不再由遍历顺序决定（含 P4-11） | P4-10 / P4-11（裁决 10） |
| ㊴ | `.gitattributes` **不入库**：`.gitignore` 的 `.*` 规则继续生效，且**不**新增 `!.gitattributes` 豁免；EOL 规则改为「本机配置」写进 `CONTRIBUTING.md`（附可直接复制的文件内容与校验命令），worktree/远端不依赖该文件 | P4-12（裁决 11） |
| ㊵ | COLRv1 `numBaseGlyphPaintRecords` 读取后**按表长 clamp**（`min(count, 表中可用记录数)`）再迭代：损坏/恶意字体不再可能迭代 43 亿次，正常字体行为不变 | P4-13（裁决 12） |
| ㊶ | ScrollBar 极短 track（`visible_lines = 1` 的 16 px）时**最小 thumb 按 track 比例收缩**：`min_thumb` 不再被 `min(track_len)` 截断成 `usable = 0`，拖拽与轨道点击保持可动（滚轮路径不受影响） | P4-14（裁决 13） |
| ㊷ | **P4-76…P4-96 共 21 条全部 WONTFIX 登记不修**（逐条编号 + 一句话 + 理由分类见 §11.11.2）；本节不再为这些条目保留「待修」状态，代码不再为其加防御/测试 | P4-76…P4-96（裁决 14） |
| ㊸ | §4.5 的 `WM_CHAR` 规则补 **AltGr 例外**文案（`CONTROL|ALT` 同按放行可打印字符；`META` 与单按 Ctrl/Alt 仍拒绝），与 `platform/event.rs` 实现一致；P0 手动验收清单补 **AltGr 与 Alt+numpad** 两项，载体落在 `kit-gallery.md` §12.5（单按 Alt 的 Alt+numpad 仍按命令修饰键拒绝，属事件层采样的已登记口径，见 §11.11.2 P4-76） | P4-16 / P4-17 |
| ㊹ | **Shift+Escape 交还宿主**：Escape 的「交还」判据从 `plain` 改为 `modifiers.is_empty()`，Shift+Escape 不再被引擎消费（组合期取消组合的语义不变） | P4-21 |
| ㊺ | `#[cfg(test)] display_builds` **计数器口径登记**：仅 `install_committed` 与 `refresh_composition_display` 各 +1，语义是「display 串重建次数」而非「提交次数」；测试用它判「display 未变则跳过重建」 | P4-34 |
| ㊻ | **Shift+双击不选词**（刻意分层）：Shift 前缀一律走「按簇扩展 / `shift_click_extension`」，因此 Shift+双击等价于两次 Shift+点击，不触发双击选词；双击选词只在无 Shift 时生效 | P4-43 |
| ㊼ | a11y **action 优先级例外登记**：可编辑节点的 `SetTextSelection` / `ReplaceSelectedText` 由引擎**先于**应用 handler 接管（`editable_action` 在 `run_handler` 之前返回），其余 action 仍是 handler 优先；与 §4.12「a11y 文本动作由引擎处理」一致 | P4-59 |
| ㊽ | §5.1 的**组合 setter 拆分落地**：placeholder 文本与颜色拆成 `text_edit_set_placeholder` / `text_edit_set_placeholder_color`；**没有** `EditableColors` 打包类型（四个独立 `text_edit_set_*_color`）；submit 无运行期 setter（⑰）。§5.1 / §5.2 已按实际落地改写 | P4-35 |

### 11.3 最终公开 API 清单

**`ui_core`（编辑内核）**

```rust
// 构建
NodeBuilder::editable_text(spec: EditableSpec) -> Self
NodeTree::set_press_focus_target(host, target) -> Result<(), EngineError>  // press 宿主显式目标（§11.2 ㉗/㊳）
// 类型（不暴露 parley）
text_edit::{TextAffinity, TextCursor, TextSelection, TextEditSnapshot, TextEditMetrics,
            TextEditComposition, EditableSpec, sanitize_text}
// App 读面
App::text_edit_value(window, node) -> Result<SmolStr, EngineError>
App::text_edit_snapshot(window, node) -> Result<TextEditSnapshot, EngineError>
App::text_edit_composition(window, node) -> Result<Option<TextEditComposition>, EngineError>
App::text_edit_metrics(window, node) -> Result<TextEditMetrics, EngineError>
App::text_edit_placeholder_color(window, node) -> Result<Color, EngineError>
App::text_edit_caret_visible(window) -> Option<bool>
App::focused_editable(window) -> Option<NodeId>
App::next_wake_deadline() -> Option<Instant>
// App 写面（程序化）
App::text_edit_set_value(window, node, &str) -> Result<bool, EngineError>
App::text_edit_undo / text_edit_redo -> Result<bool, EngineError>
App::text_edit_set_placeholder(window, node, &str) -> Result<bool, EngineError>
App::text_edit_set_label(window, node, impl Into<SmolStr>) -> Result<bool, EngineError>
App::text_edit_set_a11y_invalid(window, node, bool) -> Result<bool, EngineError>
App::text_edit_set_{placeholder,caret,selection,composition}_color(window, node, Color) -> Result<bool, EngineError>
App::text_edit_set_text_style(window, node, TextStyle) -> Result<bool, EngineError>
App::set_scroll_line_height(handle, LogicalPx) -> Result<(), EngineError>
// 错误
EngineError::{TextEditNotAvailable{node}, TextEditSpecInvalid{node,detail}, TextEditLayout{node,detail},
              NodeSpecInvalid{parent,detail}, AtlasExhausted{kind}}
// 渲染
paint::atlas::AtlasKind::{Glyph=0, Image=1, ColorGlyph=2}、KIND_COLOR_GLYPH = 8
```

**`ui_kit`（薄壳）**：`InputSpec{name,label,placeholder,masked,multiline,readonly,disabled,max_length,
visible_lines,size,submit}`、`InputState`（镜像 + `error`，只读面 + `set_error`，无 `validate`、无编辑方法）、
`InputError{Empty,TooLong,Invalid}`、`InputNode{root,viewport,editable,scrollbar,placeholder_state,
state,spec}`、`input::build`、`InputNode::{install,release,sync,value,update_value,update_label,
update_placeholder,clear,set_clearable,set_submit_on_blur}`（`set_clearable(&self, …)`，与其它运行期
setter 一致）、`kit::ScrollBar` 的 `ScrollbarOptions::tab_stop(bool)`。

### 11.4 测试与门禁基线（Windows，HEAD `cb965b56` + P5 工作区改动）

| 门禁 | 结果 |
|---|---|
| `cargo test --workspace` | **8876 passed / 0 failed / 30 ignored**（60 测试二进制 + 41 doc-test = 101 个二进制） |
| `cargo test -p ui_core --lib` | 2280（2279 passed + 1 ignored） |
| `cargo test -p ui_core --lib --features test-support` | 2610（2609 passed + 1 ignored） |
| `cargo test -p ui_core --features test-support --tests` | frame_record **8** / layout **4** / snapshot **13** / gui_smoke 1 ignored / `demo` example 39 |
| `cargo test -p ui_kit --lib` | **748 passed** |
| `cargo +nightly clippy --workspace --all-targets -- -D warnings` | **PASS**（0 warning / 0 error） |
| `cargo +nightly fmt --check` | **PASS**（exit 0，无输出） |
| `cargo build -p ui_kit --example gallery` | PASS |
| `gallery --idle-check` | **PASS**：`baseline frame_index=2` → `3000ms 2 -> 2`（空闲 **0** 帧，门槛 ≤2） |
| `gallery --theme-check` | **PASS**：Dark 0 / Light 0 / Dark-again 0 failure |
| `gallery --probe --auto-close=18` | exit 0（输出见 §11.7 与 `kit-gallery.md` §12.2） |
| `gallery --auto-demo --auto-close=6` | exit 0：派发同帧 `2 -> 2`，600 ms 后 `frame_index=3` |

### 11.5 变异测试（11 项，先红后还原，SHA256 逐字节校验）

工作副本 `E:\kytrix-p5-mut`（源树拷贝）、独立 target `E:\kytrix-target-p5`；每项：锚点唯一 →
改文件 → 跑指定测试 → 从原始字节还原 → `restore identical=True`。日志
`.review/text-input/p5-mutations.log`、脚本 `.review/text-input/p5-mutate.py`。

| # | 变异 | 目标测试 | 结果 |
|---|---|---|---|
| M1 | 空行盒高度改 0（`measure_editable` 返回 `height=0`） | `ui_core::empty_editable_text_measures_a_positive_line_box` | **RED**（`ui_kit::empty_text_caret_sits_inside_the_line_box_of_every_size` 仍绿：该用例读 caret 几何而不是 measure，见 M1b） |
| M2 | `WM_CHAR` 控制字符过滤去掉 | `ui_core::control_char_messages_never_become_text` | **RED** |
| M1b | 空文本 caret 高度改 0（`inline_flow::caret_rect`） | `ui_core::empty_editable_caret_geometry_uses_the_line_box` + `ui_kit::empty_text_caret_sits_inside_the_line_box_of_every_size` | **RED 2/2** |
| M3 | 拖选坐标改回「窗口 − 布局 + 自身 scroll」 | `ui_core::ancestor_scroll_drag_keeps_the_anchor_and_focus_bytes` + `ui_kit::dragging_inside_a_scrolled_page_selects_the_expected_range` | **RED 2/2** |
| M4 | 闪烁不请求唤醒（`ensure_blink_timer` 不再 arm） | `ui_core::a_focused_caret_renders_one_frame_per_half_period` + `editing_resets_the_caret_to_visible_and_restarts_the_period` | **RED 2/2** |
| M5 | IME 组合期插入普通文本（`handle_text` 去掉 composing 判断） | `ui_core::ime_text_events_are_suppressed_during_composition` | **RED** |
| M6 | 彩色字形 kind 退回 `KIND_GLYPH` | `ui_core::glyph_run_kind_follows_the_tile_atlas` | **RED** |
| M7 | ensure-visible 每帧 reveal（去掉 reveal key 短路） | `ui_core::ensure_visible_does_not_steal_a_manual_scroll_back` | **RED** |
| M8 | masked 显示退化为明文（`MaskedDisplay::new` 推原文） | `ui_core::masked_display_replaces_each_grapheme_with_one_bullet` + `ui_kit::masked_input_mirrors_the_real_text_and_paints_bullets` | **RED 2/2** |
| M9 | masked **a11y value** 泄漏明文（`masked_text` 返回原文） | `ui_core::a_composing_password_input_keeps_the_a11y_value_masked`（P5 收尾新增）+ `a_password_input_only_exposes_the_masked_text_run` + `ui_kit::masked_input_mirrors_…` | **RED 1/3**：组合期用例变红（原两条仍绿 —— 它们走的是 `MaskedDisplay` 而不是 `build_a11y_value` 的 masked 分支） |
| M10 | placeholder 贴控件盒顶部（paint 起点 y−6 px，即 BUG① 的旧行为） | `ui_core::placeholder_paints_at_the_text_paint_position` + `ui_kit::placeholder_and_text_share_the_first_line_box_and_the_caret` | **RED**（引擎 paint 用例红；组件用例绿 —— 它只断言“placeholder 被画出 + caret 在行盒内”，不断言 placeholder 像素位置） |

四个实机 BUG 与「能变红的用例」对应关系（均在 debug 构建下实跑）：

| BUG | 回归用例（能变红的那条） | 变异 |
|---|---|---|
| ① placeholder 垂直对齐 | `ui_core::app::text_edit_tests::placeholder_paints_at_the_text_paint_position`（paint 层与同串文本字形位置一致）+ `placeholder_line_baseline_matches_the_first_text_baseline`（同一布局源） | M10 |
| ② 空文本 caret 在行盒内 | `ui_core::app::text_edit_tests::empty_editable_text_measures_a_positive_line_box`（measure）+ `empty_editable_caret_geometry_uses_the_line_box`、`ui_kit::empty_text_caret_sits_inside_the_line_box_of_every_size`（caret 几何） | M1 / M1b |
| ③ 滚动页内拖选锚点 | `ui_core::app::text_edit_pointer_tests::ancestor_scroll_drag_keeps_the_anchor_and_focus_bytes` + `ui_kit::dragging_inside_a_scrolled_page_selects_the_expected_range` | M3 |
| ④ 控制字符不插入 | `ui_core::platform::event::tests::control_char_messages_never_become_text`（平台层）+ `ui_kit::control_characters_never_reach_the_engine_text`（引擎入口兜底） | M2 |

### 11.6 性能与不变量（设计 §7.4 门槛对照）

| 门槛（§7.4 / §6 P5-3 / §10.6） | 证据 | 结果 |
|---|---|---|
| 稳态（无聚焦、无动画）不得持续出帧，gallery `--idle-check` ≤ 2 帧/3 s | Windows：`baseline 2 → 2`（**0 帧**）；Ubuntu：`baseline 1 → 2`（**1 帧**，Wayland）；基准 `idle_frame` **508 ns**/次（帧管线短路，非帧循环） | **PASS** |
| 有焦点空闲时闪烁最多每 530 ms 一帧，不得变成连续动画帧 | 引擎用例 `a_focused_caret_renders_one_frame_per_half_period`（半周期翻转一次、只 mark_paint caret 节点）+ `ensure_visible_does_not_steal_a_manual_scroll_back`；gallery `--probe` 的 `caret_visible` / `next_wake` 观测 | **PASS** |
| 打字路径无「整份历史克隆」（旧 `kit-text.md §4.2.16` 的 O(历史×正文)） | `undo_history_stores_incremental_ranges_not_whole_documents`（`UndoStep{range, removed/inserted: Arc<str>}`）；`InputState` 不再携带历史、组件 `dispatch` 已删除；快照只在 `generation` 变化时克隆 `Arc<str>`（`snapshots_share_the_text_allocation_until_the_content_changes`） | **PASS** |
| 文本 shape 缓存命中率不明显下降 | `measure_is_o1_for_min_max_content`（`measure_breaks` 只在**宽度变化**时 +1，min/max-content 走 memo）；可编辑节点走同一 `shape_cached_editable` 布局源（偏差①），不重建第二布局 | **PASS** |

关键基准（Windows 本机，`cargo bench -p ui_core`，criterion，`bench` profile = release + `lto=fat`；
子集 `text_update_320`/`text_shape_32`/`text_layout_32`/`idle_frame`）：

| 基准 | 含义 | `cb965b56`+P5（本工作区） | 基线 `bbcb24cd`（本轮起点） |
|---|---|---|---|
| `text_update_320` | 320 个文本节点、1 个信号改文本 → 整帧（打字路径） | 见 §11.6.1 | 见 §11.6.1 |
| `text_shape_32` | 32 个文本节点全部换文本 → text pass（shape） | 见 §11.6.1 | 见 §11.6.1 |
| `text_layout_32` | 32 个文本节点的 layout pass | 见 §11.6.1 | 见 §11.6.1 |
| `idle_frame` | 无脏区帧（空转） | 见 §11.6.1 | 见 §11.6.1 |

#### 11.6.1 基准数值（同机、同 nightly、同参数、空闲机器上先后连跑）

| 基准 | 含义 | 基线 `bbcb24cd`（本轮起点） | `cb965b56` + P5 工作区 | 差异 |
|---|---|---|---|---|
| `text_update_320` | 320 个文本节点、1 个信号改文本 → 整帧（打字路径） | **1.8904 ms** | **1.9423 ms** | **+2.7%** |
| `text_shape_32` | 32 个文本节点全部换文本 → text pass（shape） | **1.5750 ms** | **1.5684 ms** | **−0.4%** |
| `text_layout_32` | 32 个文本节点的 layout pass | **137.37 µs** | **139.98 µs** | **+1.9%** |
| `idle_frame` | 无脏区帧（空转） | **433.21 ns** | **432.22 ns** | **−0.2%** |

- 基线取 `E:\kytrix-p5-base`（临时 worktree，`bbcb24cd`）+ 独立 target `E:\kytrix-target-p5-base`；
  本工作区用默认 target。日志：`.review/text-input/p5-bench-base.log` / `p5-bench.log`。
- 结论：四条关键基准差异都在 **±3%**（criterion 中位数对比），打字路径 +2.7% 属噪声量级，
  与 P0–P4 新增的可编辑文本分支/装饰绘制成本相符，**没有** O(历史×正文) 级退化。
- **取数注意**：`cargo bench` 与编译/测试并行跑会把数值抬高一个量级（实测并发时
  `text_update_320` 8.02 ms、`text_layout_32` 665 µs、`idle_frame` 60 µs）；上面的数字是在
  `rustc` 进程为 0 时先后连跑得到的。
- `target/criterion` 里原有的 `base` 记录是 **2026-11-16..11-18** 的（本轮 P0–P4 之前、且早于
  ui_core 整改批次），criterion 打印的 `change:` 百分比对照的是那份陈旧基线，**不作为本轮
  A/B 证据**（例如 `idle_frame` 会显示 +71.8%，而绝对耗时只有 432 ns）。

head `92b573cb`（P4 遗留清零轮实测 head；其后 `7f586728` 及之后仅文档改动）；判据为 0 failed、clippy/fmt exit 0、gallery 门槛。
head `92b573cb`（P4 遗留清零轮最终提交）；判据为 0 failed、clippy/fmt exit 0、gallery 门槛。
> **计数口径（P4-72）**：本节与 §11.9 的「测试通过数 / 二进制数」为 P4 遗留清零轮的最终实测值（head `92b573cb`，其后的文档修正不改变计数）；`cargo test --workspace` 的 8924 为本机 Windows 末次全量结果，llama_sys CUDA 冷构建可能使该命令在 CI/新环境长时间占用，属环境限制。三端 nightly 版本不同导致 clippy 命中项差异，已在 `CONTRIBUTING.md` 记录。
> **计数口径（P4-72）**：本节与 §11.9 的「测试通过数 / 二进制数」为 P4 遗留清零轮的最终实测值（head `92b573cb`）；`cargo test --workspace` 的 8924 为本机 Windows 末次全量结果，llama_sys CUDA 冷构建可能使该命令在 CI/新环境长时间占用，属环境限制。三端 nightly 版本不同导致 clippy 命中项差异，已在 `CONTRIBUTING.md` 记录。

**Windows（全工作区）**

| 门禁 | 结果 |
|---|---|
| `cargo test --workspace` | **8924 passed / 0 failed / 30 ignored**（102 个二进制） |
| `cargo test -p ui_core --lib` / `--features test-support` | **2315 passed + 1 ignored** / **2656 passed + 1 ignored** |
| `cargo test -p ui_core --features test-support --tests` | frame_record **8** / layout **4** / snapshot **14** / demo **39**（gui_smoke 1 ignored） |
| `cargo test -p ui_kit --lib` / `cargo test -p settings_ui --lib` | **758 passed / 0 failed**、**236 passed / 0 failed** |
| `cargo +nightly clippy --workspace --all-targets -- -D warnings` | **exit 0** |
| `cargo +nightly fmt --check` | **exit 0** |
| gallery `--idle-check` / `--theme-check` | PASS（**0 帧/3 s**；Dark→Light→Dark 0 failure） |

**Ubuntu（`my_ubuntu`，Wayland `wayland-0` + Vulkan）**

| 项 | 结果 |
|---|---|
| `cargo +nightly fmt --check` | **PASS**（exit 0） |
| `cargo test -p ui_core --lib` | **2098 passed / 0 failed** |
| `cargo test -p ui_core --lib --features test-support` | **2432 passed / 0 failed** |
| `cargo test -p ui_core --features test-support --tests` | lib 2432 / snapshot **14** 全 0 failed（frame_record/layout/demo 本机同套用例已绿，远端未单独复跑） |
| `cargo test -p ui_kit --lib` / `cargo test -p ui_kit` | **758 passed / 0 failed**（两者一致） |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings`（默认 + `--features test-support`） | **exit 0 / exit 0** |
| gallery 构建 + `--idle-check` / `--theme-check` | 构建 PASS；idle **PASS（2 帧/3 s，门槛 ≤2）**；theme **PASS（0 failure）** |
| 测后 `git checkout -- .` | 干净（`STATUS_AFTER=0`） |

**macOS（`my_macos`，ssh 无 GUI 会话 / 无 Metal 呈现）**

| 项 | 结果 |
|---|---|
| `cargo +nightly fmt --check` | **PASS** |
| `cargo test -p ui_core --lib` | **2062 passed / 0 failed** |
| `cargo test -p ui_core --lib --features test-support` | **2397 passed / 0 failed** |
| `cargo test -p ui_core --features test-support --tests` | lib 2397 / snapshot **14** 全 0 failed（frame_record/layout/demo 本机同套用例已绿，远端未单独复跑） |
| `cargo test -p ui_kit --lib` / `cargo test -p ui_kit` | **758 passed / 0 failed** |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings`（默认 + `--features test-support`） | **exit 0 / exit 0** |
| gallery 构建 + `--idle-check` / `--theme-check` | 构建 PASS；idle **PASS（1 帧/3 s，门槛 ≤2）**；theme **PASS（0 failure）** |

> 环境差异（已登记，不影响结论）：三台的 nightly 版本不同（Windows `1.99.0-nightly`、Ubuntu
> `1.102.0-nightly`、macOS `1.101.0-nightly`），P5 收尾已把两台远端新 nightly 报出的 2 条 clippy
> 修掉；Ubuntu 的 gallery 需要显式 `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR`；macOS 无 `timeout(1)`，
> 但 gallery 的 `--idle-check`/`--theme-check` 自带定时退出，可直接跑（实测均 PASS）。
>
> **idle 帧数口径**：`--idle-check` 先跑到稳定 baseline，再统计随后 3 s 内提交的帧数（门槛 ≤2）。
> 该数值包含 baseline 之后仍可能有的一次残留唤醒，run-to-run 在 **0–2 帧**之间波动，不是硬基线；
> 可复现的判据是「≤2 帧」。round 1 实测：Windows **0 帧/3 s**、Ubuntu **2 帧/3 s**、macOS **1 帧/3 s**
> （日志 `.review/text-input/final-gallery-idle.log` 与两台远端 `/tmp/fg-*.log`）。

### 11.8 P5 收尾发现与处置

| # | 发现 | 处置 |
|---|---|---|
| 1 | `build_a11y_value()` 的 masked 分支只在「masked + 组合中」可达，原用例不覆盖 → M9 存活 | **已修**：`refresh_composition_display` 同步刷新 `a11y_value`（新增 `a11y_value_for`，与 `install_committed` 共用），并新增 `a11y::contract_tests::a_composing_password_input_keeps_the_a11y_value_masked`；M9 复验 **RED** |
| 2 | Linux 4 条 `ensure_visible_*` 失败（真实跨平台缺陷：caret 行盒 22 ≠ `metrics.line_height` 20，且 reveal 目标被内容尺寸 clamp 后永不记录 key → 每帧抢滚动） | **已修**（§11.9 修复 A）：caret 吸附行盒 + reveal key 增 `limit` + 以 clamp 后的目标判定「已达」。Ubuntu `ui_core --lib --features test-support` **2386/0**，Windows 仍全绿 |
| 3 | 两台远端新 nightly 报 2 条 clippy（`input.rs:663` 新代码 + `tab_bar.rs:337` 既有代码） | **已修**：`map_or_else(|| x, f)` → `map_or(x, f)`；`close.clone()` → 移动 `close`。Ubuntu/macOS clippy **exit 0**，Windows 仍 0 |
| 4 | macOS `dragging_inside_a_scrolled_page_selects_the_expected_range` 失败（注入点 y = −1，落在窗口外：Windows 靠 caret 高 19 得 y=+0.5 的巧合通过） | **已修**：把前置留白 20 → 40 px 让字段完全可见（本轮滚动量仍 30，祖先滚动路径照旧被覆盖），并加**显式前置断言**（注入点必须在滚动容器可见区内，带可读信息）；macOS 变绿，M3 复验仍 **RED 2/2** |
| 5 | macOS 2 条渲染类用例（`a_selection_still_blinks_the_focus_end_caret`、`switching_appearance_repaints_every_input_color`）依赖真实提交帧，无 Metal 的 ssh 会话必红 | **已按环境前置处理**：检测 `TestApp::rendered()` 是否可用，缺失时 `eprintln!` 打印可读原因并跳过像素断言（引擎侧闪烁相位/主题文本色/边框色断言仍执行）；**已登记为环境限制**，有呈现后端时（Windows/Ubuntu）断言照常生效 |
| 6 | gallery demo 数据是本轮唯一的 `crates/**` 行为无关改动 | 见 `kit-gallery.md` §12.1 |

### 11.9 P5 收尾修复记录（第二轮，三端全绿）

**修复 A — caret 与行盒同一口径 + ensure-visible 可达性**（`text_edit.rs` / `text/inline_flow.rs`）

1. `inline_flow::caret_rect` 把 parley 的 `Cursor::geometry`（`min/max(block, content)` 并集，
   Linux 下可高于行盒）**吸附到光标所在行盒**：`top = 行顶`、`height = LineMetrics.line_height`。
   这是 §3.1「caret/行盒/滚动步长/IME 同一度量」的实现落地（偏差 ㉖）。
2. `RevealKey` 增加 `limit`（该滚动槽的 `max_offset`）：内容尺寸变化会改变 key ⇒ 允许「先被旧内容
   clamp、内容同步后再补一次 reveal」；稳定后 key 不再变化 ⇒ 不会每帧重复 reveal。
3. `ensure_visible` 以 `max(metrics.line_height, caret 高)` 作为垂直安全区高度，并**先把目标 clamp
   到可滚动范围**，`reached = state.offset == target || state.offset == slot.offset`：
   被 clamp 的目标也算「已达」并记录 key（修掉「不可达 ⇒ 永不记录 ⇒ 每帧抢回滚动」）。

**修复 B — 远端 clippy 2 条**：`ui_kit/src/kit/input.rs:663`（`map_or_else` → `map_or`）、
`ui_kit/src/kit/tab_bar.rs:337`（`close.clone()` → 移动 `close`）。

**修复 C — macOS 用例口径**：见 §11.8 #4/#5。

**修复 D — M9 回归用例**：见 §11.8 #1。

**变更后的语义**（同步登记）：caret 矩形在非 Windows 字体下由「内容并集高度」收紧为「行盒高度」；
ensure-visible 的「已达」判定改为「clamp 后目标」。二者都不改变 Windows 现有数值（19/19 与
目标可达），只把 Linux/macOS 的口径拉齐。

**复验**

| 项 | 结果 |
|---|---|
| 变异 11 项重跑（`.review/text-input/p5-mutations-r3.log`） | M1/M2/M1b/M3/M4/M5/M6/M7/M8/**M9**/M10 全部 **RED**（M9 = 新用例变红），每项 `restore identical=True`；M3 在新几何下仍 2/2 红 |
| Windows | 见 §11.7（8924 passed / clippy 0 / fmt 0 / gallery 0 帧 + 0 failure） |
| Ubuntu | 见 §11.7（ui_core 2098 & 2432 全绿、ui_kit 758、clippy 0、gallery idle 1 帧 + theme 0 failure） |
| macOS | 见 §11.7（ui_core 2062 & 2397 全绿、ui_kit 758、clippy 0、gallery idle 0 帧 + theme 0 failure） |

### 11.10 终审 round 1 修复记录（P3×3 + 廉价 P4）

- 基线：`b1e01c79`（P5）+ 本工作区改动；门禁在 Windows 实跑（stable `rustc 1.97.1` / nightly
  `1.99.0-nightly`），日志 `.review/text-input/fix-*.log`。

| # | 问题 | 修复 | 证据（测试） |
|---|---|---|---|
| P3-1 | 点击输入框 padding/border/滚动条 gutter 会清焦点（旧组件整盒可点；对 `submit_on_blur` 字段等同误触提交） | 引擎新增 press 宿主显式目标 `NodeTree::set_press_focus_target`（press 落在该节点或其非焦点后代上时，解析它承载的 editable；该节点不进入 Tab 序，点控件外仍失焦）→ kit root 声明代理；`kit::ScrollBar` 新增 `ScrollbarOptions::tab_stop(bool)`，输入框 track/thumb 置 `false`（动作/拖拽保留）→ 每个输入仍只有 editable 一个 Tab 停靠点 | `ui_kit::clicking_the_control_chrome_focuses_the_editable_without_blurring_it`（焦点/`submit_on_blur` 不触发/IME 不关不重开/无额外帧）、`clicking_the_multiline_padding_and_gutter_focuses_the_editable`、`the_input_contributes_exactly_one_tab_stop`（单行+多行 4 步 Tab 环）、`ui_core::interaction::focus::tests::a_press_host_*`（2 条）；变异 P3-1a/P3-1b 均 RED |
| P3-2 | 失焦不打断 undo 合并组（D7 明文要求） | `EditableText::set_focused` 在焦点变化时 `break_undo_group()` | `ui_core::blurring_breaks_the_undo_group_between_typing_sessions`、`blurred_masked_and_multiline_fields_start_a_new_undo_group`；既有 Enter/导航/粘贴打断与同聚焦会话合并用例不变；变异 RED |
| P3-3 | 空 label 的可编辑节点把内容文本当 a11y Name（组合期 Name 随 preedit 变化，违反 §4.12） | `effective_label` 只信显式 label：可编辑节点/文本输入角色不再回退到内容文本 | `ui_core::a11y::contract_tests::{an_unlabelled_editable_input_exposes_no_accessible_name, composition_never_changes_the_accessible_name}`（consumer `label()==None`、组合前后 name 恒定）；既有 TextRun/密码掩码/组合期不暴露 preedit 契约全绿；变异 RED |
| P4-1 | 本表 ⑨ 与实现相反 | 重写 ⑨（见上） | — |
| P4-2 | canvas 彩色图集耗尽时 `AtlasExhausted.kind` 恒为 `Glyph` | 按一次 pass 的 `skipped`/`skipped_color` 选择 kind | `ui_core::canvas_text_reports_color_atlas_exhaustion_with_the_real_kind`；变异 RED |
| P4-3 | `upload_color_glyph_page` 的 per-window 路径零覆盖 | 双窗口 + offscreen renderer（无需可见窗口）断言彩色页逐窗口入队 | `ui_core::the_color_glyph_page_uploads_to_every_window_with_a_renderer`（`cfg(windows)`） |
| P4-4 | ui_kit 死 API / 双源 / 签名不一致 | 删除 `App::drain_events_where`（含 `EventQueue::drain_where`，生产 0 消费者）；删除 `update_value` 里的 `last_placeholder` 潜伏写（宿主同帧直写 placeholder 信号不再被吞）；删除 `InputState::validate`（单一错误源，见 ㉘）；`InputNode::set_clearable(&self, …)` | `the_mirror_short_circuits_when_the_generation_does_not_change`（写计数探针）、`an_external_placeholder_write_survives_an_update_value_in_the_same_frame`、`error_states_expose_stable_i18n_keys`、`max_length_counts_grapheme_clusters_for_typed_and_programmatic_text` |
| P4-5 | §11.7 的 idle 帧数不可复现 | 改为「实测 0–2 帧/3 s（门槛 ≤2）」并注明测量口径（见 §11.7 注） | — |
| P4-6 | 镜像短路测试无判别力（审查变异 M09 存活） | `App::signal_write_calls()`（test-support 计数探针）+ 测试断言稳态 `sync` 不调用 `App::write` | `ui_kit::the_mirror_short_circuits_when_the_generation_does_not_change`；变异「恒 `mirror_dirty=true`」RED |

**复位验门禁（Windows，修复后）**

| 门禁 | 结果 |
|---|---|
| `cargo test --workspace` | **8876 passed / 0 failed / 30 ignored**（101 个二进制） |
| `cargo test -p ui_core --lib` | **2285 passed / 1 ignored** |
| `cargo test -p ui_core --lib --features test-support` | **2616 passed / 1 ignored** |
| `cargo test -p ui_kit --lib` | **752 passed** |
| `cargo +nightly clippy --workspace --all-targets -- -D warnings` | **exit 0** |
| `cargo +nightly fmt --check` | **exit 0** |
| `cargo build -p ui_kit --example gallery` | **exit 0** |
| gallery `--idle-check` | **PASS：baseline frame_index=2 → 2，0 帧/3 s（门槛 ≤2）** |
| gallery `--theme-check` | **PASS：Dark/Light/Dark 0 failure** |
| gallery `--probe --auto-close=18` | **exit 0**（输入几何与 P5 一致：input0 root 300×30 / editable 286×28；input3 滚动条 track 4 px） |

**变异自检（8/8 RED，先改后还原并 SHA256 逐字节校验）**：P3-1a（去 root press 代理）、P3-1b（滚动条
`tab_stop` 恢复）、P3-2（去 `break_undo_group`）、P3-3（恢复内容文本回退）、P4-2（kind 恒 Glyph）、
P4-3（彩色页逐窗口入队改成跳过有渲染器的窗口）、P4-4a（恢复 `last_placeholder` 潜伏写）、
P4-6（恒 `mirror_dirty=true`）→ 目标测试全部变红，每项 `restored_identical=True`。脚本/日志
`.review/text-input/fix-mutate.ps1`、`fix-mutations.log`。

### 11.11 P4 遗留清零轮（基线 `ead8cdcb`）

- 输入：`.review/text-input/p4-inventory.md` 的 **96 条**活跃 P4 记录（DECISION 14 / MECHANICAL 61 / WONTFIX-CANDIDATE 21；P0–P3 为 0）。
- 结论：**96 条全部处置完毕**——**75 条清零**（14 条 DECISION 按裁决落地 + 61 条机械项按批修完/登记），**21 条 WONTFIX 登记不修**（§11.11.2）。
- 口径：本节只记「处置 + 落点」，逐条原问题 / 证据 / `file:line` 保留在清单 §1–§3（清单行号为 `ead8cdcb` 时），不复述、不再引用漂移行号。

#### 11.11.1 已修 / 已按裁决定稿（75 条）

**A. DECISION 14 条（裁决原文见 §11.11.3）**

| 编号 | 处置（一句话） | 落点 |
|---|---|---|
| P4-01 | 彩色图集 tile 复制边缘色（straight-alpha 的 padding 语义进 `contract.md`）→ Windows golden 重出 | `text/raster.rs`、`app/text_pass.rs`、`render/shaders/contract.md`、`tests/golden/warp/color_emoji_1_light.png`；偏差 ㉙ |
| P4-02 | 缺 emoji 字体 / 无后端**默认硬失败**；仅 `KYTRIX_TEST_SKIP_GPU_PIXELS=1` 显式豁免并打印 reason | `tests/snapshot.rs`、`tests/golden/README.md`；㉚ |
| P4-03 | 维持**单份** golden（仅 Windows WARP）+ README 记录设备/驱动/字体名与版本/缩放 | `tests/golden/README.md`；㉛ |
| P4-04 | 单行 readonly 的 Enter / Ctrl+Enter **一律不提交**（readonly 下无任何提交面） | `text_edit.rs`；㉜ |
| P4-05 | `text_edit_metrics` 契约**收窄**为「布局脏帧返回上一完整帧旧值」，不改实现 | §5.1 metrics 行 + 本节；㉝ |
| P4-06 | 删除 auto-width placeholder 的「内容宽 0 → 自然宽」死分支 | `app/text_pass.rs`；㉞ |
| P4-07 | IME 光标区可见性口径统一：`opacity > 0` + 非零尺寸 + 未被裁剪 | `text_edit.rs`（`sync_ime_cursor_area`）；㉟ |
| P4-08 | `stale_text` / `line_is_live` 修复路径加去抖记账（一节点一帧一次） | `paint/pass.rs`、`app/frame.rs`；㊱ |
| P4-09 | `MissingLabel` 诊断纳入「有 value 无 label」的可编辑节点 | `a11y/tree.rs`；㊲ |
| P4-10 / P4-11 | `NodeTree::set_press_focus_target(host, target)`：宿主**显式指定唯一目标** + 按点落 caret；「一个 press 宿主一个目标」成文 | `node/tree.rs`、`interaction/focus.rs`、`app/input.rs`、`ui_kit/src/kit/input.rs`、引擎设计 §1.3；㊳ |
| P4-12 | `.gitattributes` **不跟踪**（不加 `!.gitattributes`），规则改写入 `CONTRIBUTING.md` | `CONTRIBUTING.md` §1；㊴ |
| P4-13 | COLRv1 `numBaseGlyphPaintRecords` **按表长 clamp** 后再迭代 | `text/raster.rs`；㊵ |
| P4-14 | ScrollBar 极短 track 的**最小 thumb 按 track 比例收缩** | `ui_kit/src/kit/scroll_bar.rs`；㊶ |

**B. MECHANICAL 61 条（按清单 §2 分组；逐条问题见清单，本节只给处置类别与落点）**

| 组（清单节） | 编号 | 处置要点 | 落点 |
|---|---|---|---|
| 平台/事件（§2.1） | P4-15、16、17 | WM_CHAR 测试 helper 保真度（scancode/extended/lParam 与真实消息一致）；§4.5 补 AltGr 例外文案；gallery 手动验收补 AltGr / Alt+numpad | `platform/event.rs`、本文 §4.5、`ui_kit/examples/gallery.rs`；㊸ |
| 编辑内核（§2.2，31 条） | P4-18…P4-48 | undo 合并串改**增量拼接**（18）；就绪判据 / 冗余回退 / 不可达分支收敛（24、28、29、38）；builder 冲突改强类型错误（26）；闪烁 / 指针 / reveal / 阈值 / 性能预算断言补齐（19、20、22、23、27、30、32、33、37、39–42、44、46、47）；`display_builds` 口径（34，见 ㊺）、Shift+Escape（21，㊹）、Shift+双击（43，㊻）、组合 setter 拆分（35，㊽）、`MaskedDisplay` 入参（31）、frame 标志一致（48）、`set_scroll_line_height` 校验与返回面（44、45） | `text_edit.rs`、`app/text_pass.rs`、`node/builder.rs`、`app/text_edit_tests.rs`、`app/text_edit_p3_tests.rs`、`app/text_edit_pointer_tests.rs`、`app/input.rs`、`app/focus.rs` |
| 渲染 / 彩色 / 图集（§2.3） | P4-49…P4-55 | mac/Linux 的 `flag_red` 改**带内**计数；彩色 LRU 驱逐用例改大页（不再空转）；`build.rs` 锚三端 descriptor/采样器常量；COLRv1-only 诊断测试去自掩蔽；单色代际隔离；删微小包装函数（§6）；`CanvasTextKey` 夹具代际 | `tests/snapshot.rs`、`text/atlas.rs`、`ui_core/build.rs`、`text/raster.rs`、`text/shape.rs`、`paint/pass.rs` |
| a11y（§2.4） | P4-56…P4-64 | `node_for` 回落 focus 字段；`focus_ancestors` 补真实 consumer 用例；角色判定去重；`editable_action` 优先级例外成文（59，见 ㊼）；Replace 的 `?` 传播判别测试；密码扫描覆盖 custom action；删 `apply_caret` 冗余守卫与不可达 run 分支；组合期 cancel 排序判别 | `a11y.rs`、`a11y/adapter.rs`、`a11y/action.rs`、`a11y/mapping.rs`、`a11y/tree.rs`、`a11y/contract_tests.rs`、`app/text_edit_tests.rs` |
| ui_kit（§2.5） | P4-65…P4-70 | 非法 style 不改状态；IME 回归区分 `focused_editable` 与 `focused`；`visible_lines_height` 的 `padding_y` 形参修正；placeholder 颜色断言走引擎；`update_value` 的 error 变化也 `request_repaint` | `ui_kit/src/kit/input.rs`、`app/text_edit_tests.rs` |
| settings_ui（§2.6） | P4-71 | 失焦提交帧的下拉/表格消费点改用**新目录**（不再用提交前快照） | `settings_ui/src/pages/translation/page.rs` |
| 文档 / 设计基线（§2.7） | P4-72、73、74、75 | 三端计数按实测回填（72：Windows 8924/0/30、Ubuntu 2098+2432、macOS 2062+2397）；`drain_events_where` 残留注记（73，见引擎设计 §1.3）；Linux 尺寸基线机理（75）与 `render/linux.rs` cfg 口径（74）写入 `CONTRIBUTING.md` | 本文 §11.7 / §11.9、`crates/ui_core/design/custom-gui-engine-design.md` §1.3、`CONTRIBUTING.md` §2/§3 |

> 批次划分（清单 §6）：1A 文档与登记 / 1B `ui_core` 测试 / 1C `ui_kit` 测试 / 2A 引擎微调 / 2B kit+settings_ui /
> 3 渲染与彩色字形（含 golden 重建）/ 4 a11y 读面。**说明**：机械项以清单分组归纳，逐条内容不在此复制。
>
> **偏差表复核（⑨）**：§11.2 ⑨（上一轮改写版）已对照
> `a11y::contract_tests::text_selection_and_replace_actions_are_advertised` 逐条核对——输入节点广告
> `SetTextSelection` + `ReplaceSelectedText` 且**不**广告 `SetValue`、`TextRun` 子节点广告 `SetTextSelection`、
> `text_selection.focus.node` 指向 `TextRun`：**与实现一致，本轮不改**。

#### 11.11.2 已登记不修（21 条，P4-76…P4-96）

| 编号 | 一句话 | 理由分类 |
|---|---|---|
| P4-76 | `WM_CHAR` 投递时刻采样修饰键存在理论漏窗（阻塞探针未复现，只会 fail-open，Chromium/Qt 同样在事件层采样） | 模型固有 |
| P4-77 | 过滤丢弃时保留 pending 高代理（真实 Windows 不产生孤立低代理，路径不可达） | 环境限制 |
| P4-78 | 被过滤字符仍先 `decode_char` 再判控制字符 | 微优化 |
| P4-79 | 每次编辑 `write_committed` 重建整篇 `String`（`Arc<str>` 模型固有，§4.2 同样 O(n)） | 模型固有 |
| P4-80 | `metrics.line_count` O(行数)、`value()` O(n) 克隆（已书面接受） | 模型固有 |
| P4-81 | >4 GiB 文本 u32 静默饱和（UI 文本不可达） | 环境限制 |
| P4-82 | `ready_layout_width` 的 `text_line` 存在性检查在现不变量下冗余（保留一致性） | 微优化 |
| P4-83 | `A11yIds::by_key` 容量对存活 `TextRun` 键豁免（上限 = 可编辑节点数，有界） | 模型固有 |
| P4-84 | 虚拟 `TextRun` 记录不回填 `disabled` / `read_only`（语义上继承父节点） | 测试读面 |
| P4-85 | `is_virtual_text_run` 每次 O(n) 扫描（仅测试/工具读面调用） | 测试读面 |
| P4-86 | a11y 批量预算按「父记录 + 每个可编辑节点 +1」计（accesskit 三端无每笔节点数硬上限） | 测试读面 |
| P4-87 | `crates/ui_core/Cargo.toml` 行内注释（与既有 `# P13/# P14` 同风格，非代码注释） | 微优化 |
| P4-88 | 内部 `editable/editable_mut` 纵深防御无实现者测试（公共入口已校验，路径不可达） | 测试读面 |
| P4-89 | `Some(*child) == text_run` 冗余纵深防御（关键分支另有 m1b 覆盖） | 微优化 |
| P4-90 | 单字素 >255 字节时 `editable_lengths` 静默降级（与既有 u8 上限同源，理论边界） | 环境限制 |
| P4-91 | reveal 的帧内 `request_frame` 造成一次额外补帧（次数有界、行为无错误） | 微优化 |
| P4-92 | `placeholder_line_fresh` 用 f32 bits 精确比较宽度（同路径重复计算位级稳定，抖动只多一次栅格化） | 微优化 |
| P4-93 | `on_pointer_leave` 的 cancel 重构在非 capture 路径行为等价（统一清理路径的等价重构） | 微优化 |
| P4-94 | 审查工件 / 实现报告里的计数与措辞不精确（批量；结论已被后续轮次覆盖） | 工件/流程（批量） |
| P4-95 | task-12 过程类观察（批量：实现者报告缺失、补丁元数据陈旧、CRLF 下 `fmt --check` 口径等） | 工件/流程（批量） |
| P4-96 | 环境限制批量（8 项：macOS 无呈现后端、Metal 计时 flake、交叉 check 缺 C 编译器、未跑 release bench、`llama_sys` 首次构建、Linux 全局字体栈无 emoji 家族、无真实 AT 客户端、远端磁盘限额） | 环境限制（批量） |

> 分类口径：**环境限制** = 本机/CI 环境不可达或不可复现；**模型固有** = 当前数据模型与事件层采样决定的边界；
> **测试读面** = 只影响测试 / 工具观测面；**微优化** = 量级可忽略或作为一致性防御保留。
> P4-94 / P4-95 是审查工件与流程记录（non-code），不属上述四类工程理由，单列「工件/流程」。

#### 11.11.3 决策记录（14 条裁决）

| # | 条目 | 裁决 | 落点 |
|---|---|---|---|
| 1 | P4-01 彩色字形偏暗 | 图集 tile **复制边缘色**（引擎实现，golden 重出） | ㉙ |
| 2 | P4-02 像素用例缺字体/无后端 | **默认硬失败** + `KYTRIX_TEST_SKIP_GPU_PIXELS=1` 显式豁免 | ㉚、`tests/golden/README.md` |
| 3 | P4-03 golden 依赖宿主字体 | **维持单 golden** + README 记录字体版本 | ㉛ |
| 4 | P4-04 readonly 单行 Enter | **一律不提交**（Ctrl+Enter 同） | ㉜ |
| 5 | P4-05 metrics 布局脏帧 | **收窄文档契约**（返回上一完整帧旧值），不改实现 | ㉝ |
| 6 | P4-06 auto-width placeholder 死分支 | **删除** | ㉞ |
| 7 | P4-07 IME 光标区可见性 | **统一口径**：`opacity > 0` + 非零尺寸 + 未被裁剪 | ㉟ |
| 8 | P4-08 stale_text 逐 tile LRU 抖动 | **加去抖记账** | ㊱ |
| 9 | P4-09 `MissingLabel` | 诊断纳入「**有 value 无 label**」 | ㊲ |
| 10 | P4-10 press 宿主（含 P4-11） | 实现**按点落 caret** + **显式指定目标 editable**（新引擎 API），并约束「一个 press 宿主显式指定一个目标」 | ㊳、引擎设计 §1.3 |
| 11 | P4-12 `.gitattributes` | **不跟踪**，改为文档记录（`CONTRIBUTING.md` 的「本机配置」） | ㊴ |
| 12 | P4-13 COLRv1 记录数 | **按表长 clamp** | ㊵ |
| 13 | P4-14 ScrollBar 极短 track | **最小 thumb 按 track 比例收缩** | ㊶ |
| 14 | P4-76…P4-96（21 条） | **全部登记不修** | §11.11.2、㊷ |

#### 11.11.4 本轮占位与遗留确认（全部已回填/已定案）

| 项 | 状态 |
|---|---|
| §11.7 / §11.9 三端测试计数（P4-72） | 已按实测回填（Windows workspace 8924/0/30、lib 2315、TS 2656；Ubuntu lib 2098、TS 2432；macOS lib 2062、TS 2397） |
| 旧计数口径 | 各轮历史记录保留原值供追溯；当前口径只认 §11.7 的实测值，不得引用更早的 `2062/2386/748`、`2027/2351/748`、`2067/2392/752`、`2032/2357/752` |
| 旧计数 `2062/2386/748`、`2027/2351/748` | 已在 §11.7 注明**过期**（round 2 为 `2067/2392/752`、`2032/2357/752`），不得引用 |

## 附录 A：关键证据索引

- 组件现状：`crates/ui_kit/src/kit/input.rs:45-56,86-113,980-1243,1246-1405,1581-1641,1643-1683,1685-1759,1762-1793`
- 空文本测量：`crates/ui_core/src/text/shape.rs:278-285,365-384`；`text/inline_flow.rs:304-313`；`app/text_pass.rs:491-506`
- 行高/metrics：`crates/ui_core/src/text/shape.rs:86-132,286-316,1090-1110`；`text.rs:745-801`；parley `layout/line.rs:159-241`
- 选区/命中/坐标：`node/tree/content.rs:256-394`；`window/focus.rs:17-49`；`interaction/scroll.rs:35-71`；`layout/hit_test.rs:328-426`；`text/inline_flow.rs:60-107`
- 帧序/稳态：`app/frame.rs:85-154`；`platform/windows/event_loop/pump.rs:646-654`；`app/timer.rs`
- IME/平台：`event.rs:158-184,237-249`；`platform/windows/ime.rs:85-183,290-352`；`platform/macos/view.rs:100-160,429-482`；`platform/linux/surface.rs:1129-1165`；`platform/wayland/text_input.rs:271-280`
- 渲染/图集：`text/raster.rs:6-12,169-195`；`text/atlas.rs:57-260`；`paint/atlas.rs`；`paint/instance.rs:1540-1600`；`render/shaders/contract.md`
- 滚动/滚动条：`app/scroll.rs:22-214`；`interaction/scroll.rs`；`kit/scroll_bar.rs:29-260,303-384`
- 定时器/动画：`app/timer.rs:14-85`；`app/animation.rs`；`examples/gallery.rs:1750,1908-1962`
- 依赖：`parley 0.12.0`（`Cargo.lock`）、`fontique/swash/skrifa`、`unicode-segmentation`（workspace，ui_core a11y 使用）、`accesskit 0.25.2`
- 本机字体事实：`C:\Windows\Fonts\seguiemj.ttf` COLR v1 + 3487 v0 base glyph / 55497 v0 layers；swash `scale/color.rs` 按 v0 布局读取（不校验版本）

## 附录 B：外部参考

1. Chromium Blink：`caret_rect.cc`（caret 行盒高度/空元素 caret）、`layout_box_model_object.cc`（空元素 caret 垂直居中）、`text_field_input_type.cc`（placeholder 独立 block）、`selection_modifier.cc`（affinity/desired x/Page 移动）、`autoscroll_controller.cc`（拖选自动滚动）、`editing_behavior.cc`（键位表）、`ui/views/controls/textfield/textfield.cc`（控制字符过滤）、`ui/base/ime/composition_text.h`、`third_party/blink/renderer/core/editing/ime/input_method_controller.cc`
2. Qt 6：`qtextlayout.cpp`（cursorToX/gra­pheme 吸附）、`qwidgetlinecontrol.cpp`（caret 高度/闪烁/掩码/preedit attributes）、`qlineedit.cpp`（placeholder 绘制/按键重置闪烁）、`qtextedit.cpp`（ensureCursorVisible/placeholder）、`qtextcursor.cpp`（verticalMovementX）、`qwindowsinputcontext.cpp`（COMPOSITIONFORM/CANDIDATEFORM/GCS_*）
3. Flutter：`text_painter.dart`（preferredLineHeight/getOffsetForCaret/getFullHeightForCaret）、`editable.dart`（maxLines/preferredHeight/掩码）、`editable_text.dart`（composing underline/_showCaretOnScreen/闪烁/VerticalCaretMovementRun）、`input_decorator.dart`（hint 同 baseline）、`text_formatter.dart`（grapheme 长度限制）
4. egui 0.37：`text_selection/text_cursor_state.rs`（cursor_rect 空 galley 回退）、`text_edit/builder.rs`（行高/hint 对齐/闪烁/scroll_to_rect/掩码）、`text_selection/visuals.rs`（闪烁时间函数）
5. parley 0.12：`editing/cursor.rs`、`editing/selection.rs`、`editing/editor.rs`、`layout/line.rs`、`layout/cluster.rs`、`layout/line_break.rs`、`style/mod.rs`；`swash 0.2.10 scale/color.rs`、`scale/mod.rs`
6. Unicode：UAX #29（grapheme clusters，GB9/GB11/GB12/GB13）、UTS #51（emoji sequences）
7. Windows：`ImmGetCompositionStringW`、`WM_IME_COMPOSITION`、`WM_CHAR`、`COMPOSITIONFORM`、`CANDIDATEFORM`、`GetCaretBlinkTime` 官方文档
8. 其它：winit 0.30.13 `platform_impl/windows/{event_loop.rs,keyboard.rs,ime.rs}`（IME/控制字符旁证）、GPUI `events.rs`（`!c.is_control()` 过滤）、AccessKit 0.25.2（TextInput/PasswordInput/TextSelection/SetTextSelection）、iced `text::editor`（编辑器模型分离参考）
