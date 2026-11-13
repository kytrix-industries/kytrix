# P16：IME 精确光标（caret）定位

> 背景：P15 的 demo 在实机上暴露了 IME UI 定位错误。Win32 只在 `WM_IME_STARTCOMPOSITION`
> 用「最后指针位置」调用 `ImmSetCompositionWindow(CFS_POINT)`；窗口首次启动、尚无指针事件时该值是
> `(0,0)`，且引擎从未调用 `ImmSetCandidateWindow`。macOS 的 `firstRectForCharacterRange` 固定返回
> 窗口原点 1×1，Linux 的 `set_cursor_rectangle` 固定 `(0,0,1,1)`。本文定义 P16 的修复基线：
> **由应用声明文本插入点（text node + UTF-8 byte index），引擎计算 caret 矩形并转给三平台 IME**。
> 实现与本文不一致处记入 `design/DEVIATIONS.md`。

---

## 0. 结论摘要

| 项 | 决定 |
|---|---|
| 范围 | `crates/kytrix_gui` 的 `Window` / `NodeTree` / 三平台 IME 通路 + `crates/kytrix_gui_demo` 的输入框接线 + 文档 |
| API | `App::set_ime_cursor_area(window, text_node, byte_index)` / `App::clear_ime_cursor_area(window)` / `App::ime_cursor_area(window)` |
| 几何 | `NodeTree::text_caret_rect` 用 parley `Cursor::geometry` 计算**文本盒局部**矩形（1 逻辑像素宽）；`Window::text_caret_rect` 加上节点绝对 bounds、content offset 与祖先滚动偏移 |
| 坐标 | 公开 API 全部是**窗口客户区逻辑坐标**、左上原点；平台层负责转物理/屏幕坐标 |
| Win32 | `ImmSetCompositionWindow(CFS_POINT, caret 底边)` + `ImmSetCandidateWindow(CFS_EXCLUDE, caret 区域)`；`WM_IME_STARTCOMPOSITION` 时重放；删除「最后指针位置」回退 |
| macOS | `GuiView` 保存 caret 区域，`firstRectForCharacterRange:actualRange:` 转成屏幕坐标返回；无 hint 时保留 1×1 回退 |
| Linux | `WaylandState` 保存 caret 区域；`set_ime_focus(true)` / 区域变化时 `set_cursor_rectangle` + `commit()`；无 hint 时 1×1 回退 |
| demo | 输入框文本子节点 + `ImeField::display(false).len()` 作为插入点；焦点在输入框时设置，离开时清除 |
| 测试 | caret 几何（空文本 / 末尾 / CJK / scale）、API 校验与清除、Win32 表单值（含真实隐藏窗口读回）、demo 输入框区域随文本移动 |

## 1. 现状与根因

### 1.1 代码事实

- `platform/windows/event_loop.rs` 的 `WndProcState.cursor` 初始化 `(0,0)`，只在收到 `PlatformEvent::Pointer`
  时更新；`WM_IME_STARTCOMPOSITION` 分支用 `state.cursor` 调 `ImmSetCompositionWindow(CFS_POINT)`。
- `platform/windows/ime.rs` 只有 `set_composition_point`：没有 `ImmSetCandidateWindow`，没有区域
  （`rcArea`），也没有在 `WM_IME_STARTCOMPOSITION` 之外预置位置。
- `platform/macos/view.rs` 的 `firstRectForCharacterRange` 固定 `(0,0,1,1)`；Linux
  `platform/linux/surface.rs::set_ime_focus` 固定 `set_cursor_rectangle(0,0,1,1)`。
- 引擎没有 caret / selection 公开几何入口；`NodeTree::text_selection_rects` 对 collapsed selection
  返回空 `Vec`，不能直接当 caret 用（`text/inline_flow.rs` §选区几何）。
- demo 的输入框是容器 + 隐藏文本子节点，视觉 `|` 只是 `display()` 末尾追加字符，不移动插入点。

### 1.2 失败链

1. 用户启动窗口后直接 Tab/键盘操作到输入框，未产生指针消息 ⇒ Win32 `WndProcState.cursor` 仍是 `(0,0)`。
2. 第一次组合开始 ⇒ 组合窗被放到客户区左上角；候选窗没有位置/排除区域。
3. macOS/Linux 同理只有固定 1×1，输入法 UI 无法贴近输入框。

## 2. 公开 API

### 2.1 `App::set_ime_cursor_area`

```rust
pub fn set_ime_cursor_area(
    &mut self,
    window: WindowId,
    node: NodeId,
    byte_index: usize,
) -> Result<(), EngineError>
```

- `node` 必须是该窗口内的**文本节点**（`NodeKind::Text`）；`byte_index` 是该文本内容中的 UTF-8 字节偏移。
- 允许在文本尚未 shape 时调用（首帧前布局未完成仍可先登记）；几何在读取时按当前文本布局解析。
  **`byte_index` 的越界/char-boundary 校验推迟到读取时**（信号写入到下一次 text pass 之间，文本内容可能仍是旧快照）。
- 只登记 IME hint，**不标脏、不请求重绘**；若请求变化则 `Dispatcher::wake()`，让平台泵有机会下发。
- 错误：`UnknownWindow`（窗口不存在）、`StaleNode`（节点不存在）、`TextShape`（非文本节点）；
  非法 byte index 由 `ime_cursor_area` / `text_caret_rect` 报 `TextShape`。
- 同一 `(node, byte_index)` 重复调用是 no-op；最后一次成功设置持续有效，直到 `clear_ime_cursor_area` 或再次设置。

### 2.2 `App::clear_ime_cursor_area`

```rust
pub fn clear_ime_cursor_area(&mut self, window: WindowId)
```

- 清除该窗口的 IME hint；不存在的窗口是 no-op。
- 平台层回到「无 hint」状态：Win32 不再重放组合窗位置；macOS/Linux 回到原有 1×1 回退。
- 不改变窗口焦点、不关闭 IME 上下文、不产生帧。
- hint 真的变化时必须 `Dispatcher::wake()`：平台泵每轮都要无条件调用 `sync_ime_cursor_area`，不能把它
  放在「本帧是否需要重绘」的门禁内（否则无帧的 clear 永远到不了平台）。

### 2.3 `App::ime_cursor_area`

```rust
pub fn ime_cursor_area(&self, window: WindowId) -> Result<Option<Rect>, EngineError>
```

- 返回当前 hint 对应的**窗口客户区逻辑矩形**。
- `Ok(None)`：没有设置 hint；`Err(TextShape)`：已设置但文本节点尚未 shape / 文本内容已变化导致暂不可解析；
  平台层应跳过本次更新而不是清除上一个有效位置。`Err(UnknownWindow)` / `StaleNode` 同上。
- 主要消费者：三平台泵的 `sync_ime_cursor_area`；也可用于应用诊断与测试。

### 2.4 `Window::text_caret_rect`

```rust
pub fn text_caret_rect(&self, node: NodeId, byte_index: usize) -> Result<Rect, EngineError>
```

`Window::ime_cursor_area` 是内部解析入口：`Ok(None)` / `Some(rect)` 与 `App::ime_cursor_area` 同义，
平台层通过 `App` 的公开包装读取。它不在 `Window` 上重复保存矩形，而是每次按当前文本布局解析。

- `NodeTree::text_caret_rect` 返回**文本盒局部**矩形（文本内容盒左上角为原点）。
- 本方法加上节点 `LayoutBox.bounds.origin`（窗口绝对坐标）与 `NodeTree::content_offset`（border + padding），
  再扣除祖先滚动偏移（`scroll::to_screen`）。
- 文本为空时 parley 的 cursor geometry 没有行高；用 `TextStyle.size × line_height.unwrap_or(1.0)` 作为高度回退
  （仍是没有字体 leading 的近似，记录为 V-107），宽度回退 1 逻辑像素。
- 非文本节点、未 shape、非法 byte index ⇒ `TextShape`；节点不存在 ⇒ `StaleNode`。

## 3. caret 几何

`NodeTree::text_caret_rect` 与现有 `text_selection_rects` 共用 `TextSlot`：

```text
node kind 是 Text
  → text_slot(node).broken()（parley Layout<ColorBrush>）
  → Cursor::from_byte_index(layout, byte_index, Affinity::Downstream)
  → cursor.geometry(layout, width = scale)（parley 第二参数是 caret 宽度，单位是物理布局像素）
  → to_rect(...) 除回 scale，得到 1 逻辑像素宽的 caret
```

- 宽度固定为 **1 逻辑像素**；高度取 parley 行 metrics。
- `byte_index` 必须 `<= text.len()` 且落在 char boundary；否则 `TextShape`（不做静默吸附）。
- **文本内容 / 样式 / 行内盒与 shape 快照必须一致**：`TextContent` 与 `ShapedText` 的 text、style、
  inline_boxes 任一不等时返回 `TextShape`；最终断行宽度（`laid_width_bits`）也必须是当前内容宽/
  max-content 宽，否则同样返回 `TextShape`。绝不返回旧布局的 caret（文本/宽度变化后、下一次
  `refresh_text` / `layout_text` 前的窗口期由平台继续沿用上一有效区域）。
- CJK / bidi / 连字的边界由 parley 负责，不自算字形索引。

## 4. 平台通路

### 4.1 Win32（IMM32）

`platform/windows/ime.rs::ImeState` 增加 `cursor_area: Option<PhysicalRect>`：

- `set_cursor_area(hwnd, area)`：保存并立刻调用：
  - `ImmSetCompositionWindow(COMPOSITIONFORM { dwStyle: CFS_POINT, ptCurrentPos: (x, y + height), rcArea: area })`
  - `ImmSetCandidateWindow(CANDIDATEFORM { dwIndex: 0, dwStyle: CFS_EXCLUDE, ptCurrentPos: (x, y), rcArea: area })`
- `WM_IME_STARTCOMPOSITION`：先重放 `cursor_area`，再发 `ImeEvent::Start`。
- `clear_cursor_area()`：清 `cursor_area`；后续 Start 不再重放（系统默认位置）。
- 删除 `WndProcState.cursor` 及 `PlatformEvent::Pointer` 更新它的代码；IME 不再读最后指针。
- `NativeWindow::sync_ime_cursor_area(app)` 在**每轮泵**（独立于 `take_frame_wake()` / 帧是否提交）调用：
  读取 `app.ime_cursor_area(id)`，与 `(逻辑矩形, 当前 Scale)` 缓存比较，变化才调用平台层；
  `Err(TextShape)` 时保留上一次有效区域。Windows 的平台 state 是**物理像素**，因此 DPI 变化即使
  逻辑矩形不变也必须重新下发。

`COMPOSITIONFORM.rcArea` 与 `CANDIDATEFORM.rcArea` 都用同一 caret 区域；`ptCurrentPos` 的
`y + height` 是组合窗基线，`CFS_EXCLUDE` 让候选窗避开文字。`CFS_POINT` 允许 IME 按自身策略微调；
本阶段不采用 `CFS_FORCE_POSITION`（它会阻止 IME 对边界/屏幕做合法调整，且各 IME 行为不一致）。

### 4.2 macOS（NSTextInputClient）

- `ViewIvars.ime_cursor: Cell<Option<Rect>>`（窗口逻辑坐标）。
- `GuiView::set_ime_cursor_area(Option<Rect>)` 仅保存；主线程由泵调用。
- `firstRectForCharacterRange:actualRange:`：有 hint ⇒ `NSRect`（view 坐标）→ `convertRect:toView:nil`
  → `window.convertRectToScreen`；无 hint ⇒ 原有 `(0,0,1,1)`。
- `NativeWindow::sync_ime_cursor_area(app)` 与 Win32 同序：每轮泵比较缓存并更新 view（不依赖是否有帧）。
- 不新增 macOS IME 启用/停用语义（P12 的 NSTextInputClient 生命周期不变）。

### 4.3 Linux（zwp_text_input_v3）

- `WaylandState.ime_cursor_area: Option<Rect>`（surface 逻辑坐标）。
- `set_ime_focus(true)` 的协议顺序必须是 **`enable()` → `set_cursor_rectangle(hint 或 1×1)` → `commit()`**：
  v3 的 `enable` 会重置此前 `set_cursor_rectangle` 状态（`text-input-unstable-v3.xml:102-113`）。
  `set_ime_focus(false)` 保持原有 `disable()`，并把“已下发区域”缓存清空。
- `set_ime_cursor_area(Some(area))` 在已 focused 时发 `set_cursor_rectangle` + `commit()`；`None` 时回到
  1×1 回退并 commit。
- 当前绑定是 v3 的 **version 1**（`surface.rs` 的 `1..=1`）：`text_input.commit()` 直接生效；若未来绑 v2+，
  `set_cursor_rectangle` 的提交还需要 surface 的 `wl_surface.commit()`（协议文档 `:286-293`）。
- `WaylandState::sync_ime_cursor_area(app, id)` 每轮泵调用（独立于帧门禁）；`Err(TextShape)` 保留旧值。

## 5. demo 接线

- `scene/ime.rs::build` 返回 `(field_node, text_node)`：`text_node` 是 `bound_hidden_label` 的返回值。
- `Scene` 增加 `ime_text: NodeId` 字段。
- `Scene::sync_focus`：
  - 输入框持有焦点 ⇒ `caret = app.read(&ime).display(false).len()`（不含视觉 `|`），
    调 `app.set_ime_cursor_area(window, ime_text, caret)`；
  - 否则 `app.clear_ime_cursor_area(window)`。
- `Scene::preview` 在播种 `ImeField::sample()` 后同样登记 caret，保证快照路径与真实路径使用同一个 hint。
- 不改变 demo 的可见布局；`|` 仍是视觉近似，但 IME 候选/组合窗由引擎用真实 caret 几何定位。

## 6. 测试计划

| 层 | 位置 | 断言 |
|---|---|---|
| 单元 | `src/text/inline_flow.rs` | `caret_rect`：末尾 x 为行宽、CJK 非零、空文本仍有行高、scale=2 时逻辑宽度仍 1 |
| 单元 | `src/node/tree.rs` / `src/app.rs` | `text_caret_rect` 的未 shape / 非文本 / 越界 / 非 char boundary / 文本内容与 shape 不一致错误；空文本回退高度 |
| 单元 | `src/app.rs` | `set_ime_cursor_area` 错误面；set/get/clear 状态；未 shape 时 `ime_cursor_area` 返回 `TextShape`；首次 `App::frame` 后解析；非 char boundary；带 padding/border 的节点绝对坐标；滚动容器内 caret 随 offset 移动 |
| 单元 | `src/platform/windows/ime.rs` | `caret_forms`：candidate 左上 + exclude 区域，composition 底边；scale=1.25 的物理表单值；真实隐藏 HWND 只 smoke-check `ImmGet*` 在可用时读回的 `rcArea`（`CFS_POINT` 允许 IME 调整，不断言 `ptCurrentPos` 完全相等；失败/无上下文打印原因并跳过） |
| 集成 | `crates/kytrix_gui_demo/tests/scene.rs` | 输入框聚焦后 hint 落在输入框内；空文本与提交「你好」后 x 增大；失焦后 `Ok(None)` |
| 静态 | 三平台 | Windows 实跑；macOS/Linux `cargo check` + 既有静态边界（V-82/V-84、V-107） |

## 7. 非目标

- 不做完整 `TextInputState` / 选区 / 插入点移动 / 多行编辑（P15 的非目标不变）。
- 不补 Windows TSF 文档管理器；内联组合串仍受 V-104 限制。
- 不保证 IME 候选窗一定出现在上方：`CFS_EXCLUDE` 只是「不要遮挡 caret 区域」的提示，最终布局由系统决定。
- macOS / Linux 仍只做静态交叉检查，不把本机 Windows 的 IME 行为外推。

## 8. 验证命令

```powershell
cargo test -p kytrix_gui
cargo test -p kytrix_gui --features test-support
cargo test -p kytrix_gui_demo
cargo +nightly clippy -p kytrix_gui -p kytrix_gui_demo --all-targets --all-features -- -D warnings
cargo +nightly fmt -p kytrix_gui -p kytrix_gui_demo -- --check
cargo doc -p kytrix_gui --no-deps
cargo check -p kytrix_gui --target x86_64-unknown-linux-gnu
cargo check -p kytrix_gui --target aarch64-apple-darwin
```

## 9. 验证记录（2026-11-13）

- `cargo test -p kytrix_gui`：666 passed / 1 ignored。
- `cargo test -p kytrix_gui --features test-support`：721 passed / 1 ignored；集成 `frame_record` 6、`layout` 4、`snapshot` 8。
- `cargo test -p kytrix_gui_demo --all-features`：23 单元 + 12 集成全过。
- `cargo +nightly clippy ... --all-targets --all-features -- -D warnings`、`cargo +nightly fmt -- --check`、`cargo doc -p kytrix_gui --no-deps` 全过。
- Linux/macOS 交叉 `cargo check`（`KYTRIX_GUI_SKIP_SHADERS=1`）全过。
- Win32 probe：隐藏 HWND 上 `ImmSetCompositionWindow`/`ImmSetCandidateWindow` 后 `ImmGetCompositionWindow`/`ImmGetCandidateWindow` 读回 `rcArea=(100,200,101,220)`，与输入 area 一致；`CFS_POINT` 的 `ptCurrentPos` 不强制逐像素相等。
- **真机回归**（Windows 11 / Microsoft Pinyin）：用 UIA `SetFocus`（**不经过鼠标**）聚焦「输入法测试输入框」→ `SendKeys("nihao")`；候选窗出现在输入框正文插入点正下方，截图见 [`demo-phase16-ime.png`](demo-phase16-ime.png)。这同时覆盖了原 bug 的「无指针事件首次进入输入框」路径。
