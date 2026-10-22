# base_ui `color_picker` 设计与落地计划

> 版本：v1（已完成 3 轮对抗性审查；A / 快速 / B / 无工具四个审查均 PASS；0 BLOCKER / 0 MAJOR）
> 目标读者：直接实现该组件的 AI/工程师
> 本阶段约束：**只写文档，不修改任何代码**
> 工作区：`E:\kytrix_color_picker`
> 组件归属：`crates/base_ui/`

---

## 0. 结论先行

1. **Hue 色彩带可以通过 GPUI 直接绘制，但不能用一个 `linear_gradient` 画出来。** GPUI 0.3.8 的 `Background` 内部私有的 `colors: [LinearColorStop; 2]` 只有 **两个** color stop（`gpui-pre-0.3.8/src/color.rs:779-788`），公开构造器 `linear_gradient(angle, from, to)` 也只接收两个 stop。7 段色相环必须拆成多段，或改成逐段实色绘制。
2. 本设计选择 **`canvas` + `paint_quad` 逐条实色切片** 来绘制 Hue/Saturation/Lightness/Alpha 四条轨道，而不是 6 段 `linear_gradient`。原因有两个：
   - GPUI 自带的 gradient 只有 2 个 stop，无法表达完整的 hue 环；
   - GPUI 0.3.8 两个后端对 “sRGB gradient” 的解释不一致：Windows/DirectX 直接对 sRGB 分量插值，wgpu 后端却对本来就是 sRGB 的 stop 又做了一次 `linear_to_srgba`。用实色切片可以完全绕开后端差异，颜色精确、跨平台一致。
   - gpui-component 0.7.1 自己的色相条也是用 **96 个实色 div** 拼的，而不是 gradient；本设计只是把 96 个元素合并成 **1 个 canvas**，进一步降低 layout/element 开销。
3. 12×12 调色盘用 **一个 canvas 画 144 个圆角实色 quad**，选中态再画 1 个 ring quad。没有 144 个 div、没有 144 个 hover listener、没有每格一个 Entity。
4. 组件的业务状态是 `base_ui::color_picker::ColorPickerState`（Entity + `EventEmitter<ColorPickerEvent>`），对外事件为 `ColorPickerEvent::ColorChanged(Hsla)`；使用方式与 `input::InputState` 一致：调用方持有/订阅 state，视图只负责渲染。
5. 组件自身是一个 **自更新** 的整体：触发器是一个观察 state 的子 view，面板就是 `ColorPickerState` 的 `Render` 输出。颜色变化会原地重绘触发器和面板；`ColorPickerState::use_keyed` / `TriggerSlot` 用私有 slot 避免 `window.use_keyed_state` 额外注册的 observer 把每次 notify 再转发给父 view。**但不要把它宣传成“父页面完全不 render”**：GPUI 的 `mark_view_dirty` 会把祖先 view 标脏（`window.rs:2191-2204`），父 view 的 render 仍可能执行；本设计消除的是 `use_keyed_state` 额外的一次 `cx.notify(current_view)`，以及让触发器/面板成为可独立重绘的子 view。性能收益要按实际 draw 成本评估，不能承诺“父 render 不跑”。
6. 不新增依赖。12×12 调色盘数据用 `std::sync::LazyLock` 在首次打开时生成一次（144 次 HSL→RGB），之后每帧只读取。
7. 本次不引入 gpui-base / gpui-component 的现成 color picker，原因见 §1.5；但复用它们已经验证过的思路（hex 解析/格式化、`canvas`、`on_drag`、`canvas` prepaint 记录 bounds）与 base_ui 既有组件（`Button`、`Join`、`Overlay`、`Input`、`Separator`、主题 token）。

---

## 1. 事实调查

> 这一节列出的版本、符号和行号，是后续所有设计决策的依据。实现前不要凭记忆改 API。

### 1.1 依赖版本（以 `Cargo.lock` 为准）

| crate | 解析版本 | 说明 |
|---|---|---|
| `gpui-kit` | 0.7.1 | workspace 配置 `default-features = false`，因此 **没有** `gpui-component` 和 assets 层 |
| `gpui-base` | 0.7.1 | `gpui_kit::base` 就是它 |
| `gpui`（package `gpui-pre`） | **0.3.8** | 不是 0.3.9；registry 里 0.3.9 的源码不能当作事实依据 |
| `gpui-pre-windows` | 0.3.8 | Windows 桌面后端 |
| `gpui-pre-wgpu` | 0.3.8 | Linux/macOS 后端 |
| `gpui-component` | 0.7.1 | 只存在于 registry，**未启用**；仅作为实现参考 |

相关事实：

- `crates/base_ui/Cargo.toml` 只依赖 `gpui-kit`、`color`、`comrak`、`either`、`image`、`rust-embed`、`smallvec`、`smol_str`。
- `gpui-base 0.7.1` 的 `lib.rs` 里已经 `mod color_picker;`，并且 `pub use color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState, ColorSwatch, HslaSliders};`（`gpui-base-0.7.1/src/lib.rs:19,100`）。
- 同一个 `gpui-base` 还有 `pub mod slider;`（`lib.rs:62`）和 `pub use slider::{Slider, ...}`（`lib.rs:169`）。

### 1.2 base_ui 现有结构与约定

- `crates/base_ui/src/lib.rs` 只做模块声明，模块列表按字母序；新增 `pub mod color_picker;` 应放在 `button` 与 `deque_sized_list` 之间。
- 组件文件都是单文件模块（`alert.rs`、`table.rs`、`layer.rs` 等），单元测试内联在文件底部的 `#[cfg(test)] mod tests`。
- 现有可复用组件：
  - `Button`：`crates/base_ui/src/button.rs`，实现 `InteractiveElement`、`StatefulInteractiveElement`、`Selectable`、`Disableable`、`UnifiedSizable`、`TooltipExt`、`Styled`。`Button::new(id)` 默认 `UnifiedSize::Medium`。
  - `Join`：`crates/base_ui/src/join.rs`，`#[derive(Default, IntoElement)]`，只能 `.button(Button)` / `.select(Select)`；会给非首元素加 `rounded_l_none().border_l_0()`、给非尾元素加 `rounded_r_none()`。它自己实现 `Styled`，因此 `.bg()`/`.p()`/`.w_full()` 是对内部 flex row 生效。
  - `Overlay`：`crates/base_ui/src/overlay.rs`。`Overlay::new(id, open_state: Entity<bool>, trigger_builder, panel_builder)`，负责 Popup 定位、Presence 过渡、`on_mouse_down_out` 关闭、`deferred`/`Positioner`/`occlude`。`Dropdown` 就是它的直接使用范例（`dropdown.rs:173-214`）。
  - `Input` / `InputState`：`crates/base_ui/src/input.rs`。
    - `InputStateBuilder::new(id).default_value(...).placeholder(...).build(window, cx)` 必须在 `window.use_keyed_state(..., |window, cx| ...)` 的 `Context<InputState>` 里调用（见 `examples/input.rs:19-23`）。
    - `InputState` 发出 `InputStateEvent::{Change(SharedString), PressEnter(SharedString), Blur(SharedString)}`（`input.rs:139-146`）。
    - `Input::new(&Entity<InputState>)` 是展示层，支持 `.small()/.medium()/.large()`。
    - `InputState::engine_focus_handle(&self, cx) -> FocusHandle` 可以给测试/外部移焦使用（`input.rs:156-162`）。
  - `Separator::horizontal()`：`crates/base_ui/src/separator.rs`，颜色 `theme.roles.line.divider`，`h_px().w_full()`。
  - 主题：`crates/base_ui/src/style/`。
    - `theme.roles.surface.{canvas,raised,sunken,overlay,control}`
    - `theme.roles.line.{divider,border,strong,focus}`
    - `theme.roles.fg.{default,muted,subtle,disabled}`
    - `theme.components.input.background.{enabled,disabled}`、`.foreground.{enabled,disabled}`、`.border.{normal,focused,disabled}`、`.placeholder`、`.selection`（`components.rs:240-305`；`background/foreground/border` 是嵌套结构，不是单个 `ResolvedColor`）。
    - `theme.components.menu.background` / `.shadow` 是单个 `ResolvedColor`（`components.rs:307-314`）。
  - 测试模式：`#[gpui_kit::test]`、`TestAppContext`、`VisualTestContext`、`cx.debug_bounds("SELECTOR")`、`cx.simulate_click(...)`、`cx.simulate_mouse_down/up`、`cx.simulate_input(...)`、`cx.run_until_parked()`。示例见 `dropdown.rs:368-485`、`overlay.rs:199-355`。
- 目前 **没有** `base_ui::color_picker`，也 **没有** `base_ui` 的 slider 组件。
- 主窗口最小尺寸是 `1280×960`（`crates/kytrix/src/windows/root/window.rs:33-37`），所以 403px 高的 popup 不会触到窗口高度边界；面板不需要滚动。

### 1.3 GPUI 0.3.8 的绘制能力（重点）

以下 API 在 `gpui-pre 0.3.8` 中真实存在：

- `gpui::canvas(prepaint, paint)`：返回 `Canvas<T>`，`prepaint` 是 `FnOnce(Bounds<Pixels>, &mut Window, &mut App) -> T`，`paint` 是 `FnOnce(Bounds<Pixels>, T, &mut Window, &mut App)`，两者都必须 `'static`。`Canvas` 本身实现 `Styled`，可以用 `.absolute().size_full()` 填满父容器。
- `Window::paint_quad(PaintQuad)`、`fill(bounds, background)`、`quad(bounds, corner_radii, background, border_widths, border_color, border_style)`。
  - `PaintQuad { corner_radii: Corners<Pixels>, border_widths: Edges<Pixels>, border_color: Hsla, background: Background, ... }`。
  - 实色圆角：`fill(bounds, color).corner_radii(px(4.))`。
  - 边框/ring：`fill(bounds, hsla(0., 0., 0., 0.)).border_widths(px(2.)).border_color(ring).corner_radii(px(3.))`，或 `quad(...)`。
- `checkerboard(color, size) -> Background`：棋盘格背景；shader 中一半方格是该实色、另一半透明。用于 alpha 色块的底。
- **`Background` 内部私有的 `colors: [LinearColorStop; 2]` 只有两个 stop**（`gpui-pre-0.3.8/src/color.rs:779-788`）；`linear_gradient(angle, from, to)`（`:865`）与 `linear_color_stop(color, percentage)`（`:893`）也只接收两个 stop。没有 3+ stop 的公开 API，也不能 push stop。
- `ColorSpace` 默认是 `Srgb`（`color.rs:757-765`）；`Background::color_space` 可改成 `Oklab`，但仍然只有两个 stop。
- Windows/DirectX shader（`gpui-pre-windows-0.3.8/src/shaders.hlsl`）：
  - `prepare_gradient_color` 只在 `color_space == 1`（Oklab）时做转换；`color_space == 0` 直接使用 `hsla_to_rgba` 的返回值（`:317-334`）。
  - `gradient_color` 的 `case 0` 是 `lerp(color0, color1, t)`（`:384-387`），即在 sRGB 分量上插值。
- wgpu shader（`gpui-pre-wgpu-0.3.8/src/shaders.wgsl`）：
  - `prepare_gradient_color` 在 `color_space == 0` 时又对 `hsla_to_rgba` 的结果调用 `linear_to_srgba`（`:404-425`）。
  - 而 `hsla_to_rgba`（`:241-274`）直接按 HSL 公式返回 sRGB 值，没有做 linear 转换。
  - 结论：**不要依赖 GPUI 0.3.8 的 gradient 做跨平台色彩正确性**。Windows 上是对的，wgpu 上会偏亮；这是上游实现差异，不是本组件应该去修的。
- 没有圆角裁剪：`ContentMask` 只有矩形 bounds；`overflow_hidden` 也只能裁矩形。因此圆角只能由每个 quad 自己的 `corner_radii` 或逐列收缩的绘制来近似。
- 交互：
  - `Div::on_mouse_down(MouseButton, Fn(&MouseDownEvent, &mut Window, &mut App))`
  - `Div::on_drag(value, constructor)`：鼠标拖动开始后，`on_drag_move` 会收到 **元素外** 的移动事件（`div.rs:353-383`），这是滑杆拖动的正确机制。
  - `Div::on_drag_move::<T>(Fn(&DragMoveEvent<T>, ...))`，`DragMoveEvent` 有 `event: MouseMoveEvent`、`bounds: Bounds<Pixels>`、`drag(cx) -> &T`。
  - `Div` 没有 `on_prepaint`。base_ui 现有代码里的 `.on_prepaint` 来自 gpui-base 的 `ElementExt`（`gpui-base-0.7.1/src/element_ext.rs:16-28`），它会给元素注入一个 `canvas(...).absolute().size_full()` 子元素并在 prepaint 时回调 bounds。**本设计不用它**：palette/track 本来就有一个自己的 canvas，直接在 canvas 的 prepaint 回调里把 bounds 写进 `Rc<Cell<Bounds<Pixels>>>` 即可，避免额外元素。
  - `Div::on_mouse_move` 只在 hover 时触发，不能用于拖出元素后的移动；滑杆必须用 `on_drag`/`on_drag_move`。
- `Window::use_keyed_state<S>(key, cx, init)`：返回 `Entity<S>`，init 拿到 `&mut Context<S>`；创建时会给 **当前 view** 注册 observe，keyed state notify 时会 `cx.notify(current_view)`（`window.rs:4204-4222`、`:7201-7206`）。这既是缓存机制，也是一个容易多出一次 `cx.notify(current_view)` 的坑，见 §6.4。

### 1.4 base_ui / GPUI 的 colors / Hsla

- `gpui_kit::Hsla` 的字段是 `h, s, l, a: f32`，**h/s/l/a 都在 0..1**（gpui 的 `hsla()` 也是这个约定）。
- `Rgba::from(Hsla)` 与 `Hsla::from(Rgba)` 都在 gpui 内（`gpui-pre-0.3.8/src/color.rs:194,677`），可以直接用于 hex 与 HSL 的互转。
- `Rgba` 的分量是 0..1 的 sRGB（非线性）。本组件不做色彩管理，不引入 `color` crate 的 Oklch 转换；调色盘的 HSL 生成和 hex 量化都在 sRGB/HSL 内完成。
- `Hsla` 实现了 `Copy/Clone/Debug/PartialEq`（gpui-base 的 color picker 里直接 `self.preview == self.value`），可以按值比较。

### 1.5 已存在的 color picker：为什么不用

#### A. `gpui-base::ColorPickerState`（0.7.1）

事实：

- 它已经实现：`value/preview/open/active_tab/hex_input/4 个 SliderState`，事件 `ColorPickerEvent::Change(Option<Hsla>)`（`gpui-base-0.7.1/src/color_picker.rs:90-95,177-250`）。
- 4 个 slider 都是 `SliderState::new().min(0).max(1).step(0.01)`（`:109-127`）；**hue 的精度被固定成 1%（约 3.6°）**，没有 setter 能在创建后改成更细的步长（`SliderState::step(self, ...)` 是 consuming builder，`sliders()` 只暴露 `&Entity<SliderState>`）。
- 它的 `select_color` 会 `self.open = false`（`:352-355`）；调色盘选择会关闭 picker。
- 它的 hex input 是 gpui-base 的 `input::InputState`，不是 base_ui 的 `input::InputState`；base_ui 的 `Input` 不能直接渲染它，需要另写一套 input chrome / editor style。
- 它没有调色盘数据、没有 12×12、没有选中命中逻辑；palette 仍要应用自己写。
- 它的事件名是 `Change`，不是需求要求的 `ColorChanged`。

结论：**不直接复用 state**。复用它意味着要包一层再同步“两份颜色/两个 tab/一个 open”，还要解决 hex input 的样式与精度问题；复杂度高于写一个 300 行左右的 base_ui state。

#### B. `gpui-component::ColorPicker`（0.7.1）

事实（`gpui-component-0.7.1/src/color_picker.rs`）：

- 它是在 gpui-base color picker 之上的 **有样式封装**，依赖 `rust_i18n`、自己的 `Theme`、`Popover`、`TabBar`、`Slider` 等；workspace 的 `gpui-kit` 明确 `default-features = false`，没有启用 `component`。
- 它的调色盘是 `DEFAULT_COLORS` 的 9 组 12 阶 Tailwind/Radix 风格色板（`color_palettes()`，`:25-51`），不是 12×12 HSL 调色盘。
- 它的 hue 条是 96 个 `div().flex_1().bg(color)`（`:255-273,421-433`），saturation/alpha 用 2-stop `linear_gradient`（`:435-447`）。
- 它把 hex input 放在 `hovered_color` 有值时才显示（`:191-207`），不符合我们“始终显示当前颜色 + hex input”的布局。

结论：**不引入依赖，仅作为实现参考**。它验证了两件事：hue 条必须拆段绘制；gpui-base 的 picker 需要应用自己提供 palette/popup。

### 1.6 需求与事实的映射

| 需求 | 落地方式 | 事实依据 |
|---|---|---|
| 1. 默认是颜色方块 dropdown Button，点开 popup panel | `ColorPicker` = `Overlay` + 子 view `ColorPickerTrigger`（`Button::tonal` + `ColorWell`） | `Overlay`/`Dropdown` 现有实现 |
| 2. 两个 tab，Join + Button | `Join::default().button(...).button(...)`，每个 Button `.ghost().small().flex_1()`；选中态用 `.bg(surface.overlay)` 覆盖 | `join.rs`、`button.rs` |
| 3. 12×12 HSL 调色盘，下方分割线，当前色块 + input，命中色块显示 border | 1 个 canvas 画 144 个 quad；独立 12×12 数据；下方 `Separator` + `ColorWell` + `Input`；选中 ring 用亮度选黑/白 | `canvas`/`paint_quad`；需求原文 |
| 4. 第二个 tab 是 HSLA，布局按图 1，Hue 色带要确认 | 4 行 `label + canvas track + numeric`；canvas 用 256 条实色切片绘制，含圆角帽 | §1.3；gpui-component 的 96 div 先例 |
| 5. 业务逻辑类似 input，订阅 `ColorPickerState` 的 `ColorChanged` | `ColorPickerState: Entity + EventEmitter<ColorPickerEvent>`；事件 `ColorChanged(Hsla)`；带 hex `InputState` 子实体 | `input.rs` 的 state/event 模式 |
| 6. 绘制性能优先 | 1 canvas/144 quad；4 canvas/~1030 quad；无每格元素/handler；触发器/面板是独立子 view；不承诺父 view 不 render | §6 |

---

## 2. 总体架构

### 2.1 文件与模块

```text
crates/base_ui/
├── src/
│   ├── lib.rs                     # 只加一行: pub mod color_picker;
│   └── color_picker.rs            # 新增，单文件模块（公共 API + 私有实现 + 单元测试）
└── examples/
    └── color_picker.rs            # 新增，手工验收 + 编译检查
```

为什么是单文件：

- workspace 有 `clippy::pub_use = "forbid"`（根 `Cargo.toml:154`），整个仓库没有任何 `pub use`。如果做成 `color_picker/mod.rs` + 子模块，公共类型只能放在子模块里，外部路径会变成 `base_ui::color_picker::state::ColorPickerState`，不可接受；而 `mod.rs` 又受 `clippy::definition_in_module_root = "forbid"` 限制，不能放定义。
- 现有复杂组件（`alert.rs` 32KB、`layer.rs` 59KB）也是单文件。本文件预计 850–1200 行（含测试），在项目可接受范围内。
- 如果未来超过约 1500 行，只能把 **私有辅助类型** 拆到 `color_picker/` 子目录，公共类型仍留在 `color_picker.rs`；本次不做。

### 2.2 类型关系

```text
ColorPicker (RenderOnce, 无状态视图元素)
├── open_state: Entity<bool>              // use_keyed_state((id,"open"))
├── trigger_slot: Entity<TriggerSlot>     // use_keyed_state(...)，只持有 trigger，不 notify
│   └── trigger: Entity<ColorPickerTrigger>  // 自更新 view，observe state/open
└── Overlay(panel = Entity<ColorPickerState>)
        └── ColorPickerState::render()     // 整个 Panel 的 Render 输出
```

- `ColorPickerState` 由 `ColorPickerState::use_keyed(id, default_color, window, cx)` 创建。该函数内部用一个 **私有的、不 notify 的** `ColorPickerStateSlot` 作为 `use_keyed_state` 的载体，state 本身由 `cx.new` 创建。这样 state 的 `cx.notify()` 不会被 `use_keyed_state` 的 observer 转发到父 view。
- `ColorPickerState` 既是业务 state，又是 Panel 的 view（对应 gpui-base 的 `ColorPickerState: Render` 只渲染 hex input 的做法，这里渲染整个 Panel）。
- `ColorPickerTrigger` 是独立子 view，观察 state 和 open，因此颜色/开关变化只重绘触发器；GPUI 仍会按 dirty 路径标脏祖先，但不会再多走一次 keyed observer 的 notify。
- `TriggerSlot` 只是让 trigger entity 在多次 `ColorPicker::render` 之间保持稳定；它 **不 notify**，因此不会像直接把 trigger 放到 `use_keyed_state` 里那样每次颜色变化都 `notify(current_view)`。

### 2.3 数据流

> 为什么不用 `Dropdown`：`Dropdown` 的 panel 内容是 `Menu`/`MenuItem`，只能承载菜单项；本组件的 panel 是任意 canvas + Input + tabs。`Dropdown` 自己也只是 `Overlay` 的一层壳（`dropdown.rs:173-214`），所以直接使用 `Overlay`，触发器仍然是一个 `Button`，行为上就是 dropdown。

```text
用户点击 palette / 拖动 slider
  -> ColorPickerState 内部 set_color(...)
     -> self.color = new
     -> self.hex_input.update(set_value(hex_string(new).as_str()))   // set_value 不触发 InputEvent::Change
     -> cx.emit(ColorPickerEvent::ColorChanged(new))
     -> cx.notify()
        -> ColorPickerState::render() 重绘 Panel（颜色、选中 ring、thumb、数字）
        -> ColorPickerTrigger 观察到 notify，重绘 trigger swatch
        -> 调用方如果订阅了 ColorChanged，自行处理业务
```

```text
用户在 hex input 输入
  -> InputState 发 InputStateEvent::Change(text)
     -> ColorPickerState 解析
        - 合法: 更新颜色 + emit ColorChanged，但不回写 input（避免光标跳动）
        - 非法: 什么都不做
  -> Enter / Blur
     -> 合法: 规范化回写 #RRGGBB(AA)
     -> 非法: 回退到当前颜色的 hex
```

### 2.4 私有类型与常量清单（实现时按此命名）

```rust
// Panel / layout
const PANEL_PADDING: Pixels = px(12.);
const PANEL_CONTENT_WIDTH: Pixels = px(284.);
const PANEL_SECTION_GAP: Pixels = px(12.);

// Palette
const PALETTE_COLS: usize = 12;
const PALETTE_ROWS: usize = 12;
const PALETTE_CELL: f32 = 20.0;
const PALETTE_GAP: f32 = 4.0;
const PALETTE_STEP: f32 = 24.0;
const PALETTE_SIZE: f32 =
    PALETTE_CELL * PALETTE_COLS as f32 + PALETTE_GAP * (PALETTE_COLS as f32 - 1.0); // 284
const PALETTE_SATURATION: f32 = 0.72;
const PALETTE_LIGHTNESS: [f32; 12] = [
    0.06, 0.12, 0.18, 0.26, 0.34, 0.42, 0.50, 0.60, 0.70, 0.80, 0.90, 0.96,
];

// HSLA sliders
const SLIDER_ROW_HEIGHT: Pixels = px(24.);
const SLIDER_ROW_GAP: Pixels = px(10.);
const SLIDER_LABEL_WIDTH: Pixels = px(72.);
const SLIDER_VALUE_WIDTH: Pixels = px(40.);
const SLIDER_ROW_INNER_GAP: Pixels = px(8.);
const TRACK_HEIGHT: f32 = 14.0;
const TRACK_RADIUS: f32 = 7.0;
const TRACK_SLICES: usize = 256;
const THUMB_DIAMETER: f32 = 12.0;

// 私有元素/视图
struct ColorPickerStateSlot { state: Entity<ColorPickerState> }
struct TriggerSlot { trigger: Entity<ColorPickerTrigger> }
struct ColorPickerTrigger { state: Entity<ColorPickerState>, open: Entity<bool>, size: UnifiedSize, _subscriptions: Vec<Subscription> }
struct ColorWell { color: Hsla, size: Pixels, radius: Pixels }
struct PaletteGrid { state: Entity<ColorPickerState>, color: Hsla, label: SharedString }
struct HslaSlider { state: Entity<ColorPickerState>, channel: HslaChannel, color: Hsla, label: SharedString }
enum HslaChannel { Hue, Saturation, Lightness, Alpha }
struct HslaDrag { picker: EntityId, channel: HslaChannel }
struct HslaDragPreview;
struct PaletteSwatch { hsla: Hsla, rgb: [u8; 3] }
```

- 这些常量必须在布局/绘制里**实际引用**（例如 `.w(px(PALETTE_SIZE))`、`.h(px(TRACK_HEIGHT))`、`gap(SLIDER_ROW_GAP)`），不能只声明；否则 `-D warnings` 的 `dead_code` 会报错。确实不用的常量直接删除。
- 所有私有元素（`ColorWell`/`PaletteGrid`/`HslaSlider`）加 `#[derive(IntoElement)]` 并实现 `RenderOnce`；`ColorPickerTrigger`/`ColorPickerState` 是子 view，实现 `Render`，不 derive `IntoElement`。
- `HslaChannel`/`HslaDrag` 加 `#[derive(Debug, Copy, Clone, Eq, PartialEq)]`，方便在 `on_drag` 回调里按值比较和传递。
- `ColorWell`/`PaletteGrid`/`HslaSlider` 不实现 `Styled`（没有对外样式需求），字段保持私有。
- `HslaChannel` 需要 `Debug` 以便调试 selector；不要用 `format!` 每帧生成 selector，用 4 个固定字符串。

**私有方法/关联函数签名（实现时按此写，避免“未定义就使用”）：**

```rust
// ColorPickerState（全部在唯一的 impl 块内）
fn set_user_color(&mut self, color: Hsla, sync_hex: bool, window: &mut Window, cx: &mut Context<Self>);
fn set_channel(&mut self, channel: HslaChannel, t: f32, window: &mut Window, cx: &mut Context<Self>);
fn select_palette(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>);
fn write_hex(&mut self, color: Hsla, window: &mut Window, cx: &mut Context<Self>);
fn on_hex_change(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>);
fn on_hex_commit(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>);
fn tab_button(&self, cx: &mut Context<Self>, state: Entity<Self>, tab: ColorPickerTab, active: ColorPickerTab, label: SharedString) -> Button;

// HslaChannel
fn id_prefix(self) -> &'static str;
fn debug_selector(self) -> &'static str;
fn label(self, labels: &ColorPickerLabels) -> SharedString;
fn value_text(self, color: Hsla) -> String;

// PaletteGrid / HslaSlider / ColorWell
PaletteGrid::new(state: Entity<ColorPickerState>, color: Hsla, label: SharedString) -> Self;
HslaSlider::new(state: Entity<ColorPickerState>, channel: HslaChannel, color: Hsla, label: SharedString) -> Self;
ColorWell::new(color: Hsla) -> Self; // .size(Pixels), .radius(Pixels)
```

### 2.5 推荐 imports（避免命名冲突）

```rust
use std::{
    cell::Cell,
    rc::Rc,
    sync::LazyLock,
};

use gpui_kit::{
    AnyElement, App, AppContext, Bounds, Context, ElementId, Empty, Entity, EntityId, EventEmitter,
    Hsla, InteractiveElement, IntoElement, MouseButton, Orientation, ParentElement, Pixels, Point,
    Render, RenderOnce, Rgba, Role, SharedString, Size, StatefulInteractiveElement, Styled,
    Subscription, TextAlign, Window, canvas, checkerboard, div, fill, hsla, point, px, rems, size,
    base::{
        Align, Disableable, Placement, Selectable, StyledExt, box_shadow,
    },
    prelude::FluentBuilder,
};

use smol_str::{SmolStr, format_smolstr};

use crate::{
    button::Button,
    input::{Input, InputState, InputStateBuilder, InputStateEvent},
    join::Join,
    overlay::Overlay,
    separator::Separator,
    size::{UnifiedSizable, UnifiedSize},
    style::theme::ThemeExt,
};
```

- `Canvas`/`Corners`/`Edges`/`BorderStyle` 不在推荐 import 里：用 `canvas(...)` 函数、`fill(...).corner_radii(...).border_widths(...)` 即可，不需要显式写这些类型。
- 本组件 v1 没有 hover 过渡，不需要 `Easing/Transition/transition`。
- 上面的 import 是展示路径与命名冲突；**最终以 `cargo +nightly clippy -- -D warnings` 为准删掉未使用的 import**。
- **绝对不要** `use gpui_kit::base::*;`：gpui-base 0.7.1 已经导出了同名的 `ColorPicker`/`ColorPickerState`/`ColorPickerEvent`/`ColorSwatch`。


```text
用户点击 palette / 拖动 slider
  -> ColorPickerState 内部 set_color(...)
     -> self.color = new
     -> self.hex_input.update(set_value(hex_string(new).as_str()))   // set_value 不触发 InputEvent::Change
     -> cx.emit(ColorPickerEvent::ColorChanged(new))
     -> cx.notify()
        -> ColorPickerState::render() 重绘 Panel（颜色、选中 ring、thumb、数字）
        -> ColorPickerTrigger 观察到 notify，重绘 trigger swatch
        -> 调用方如果订阅了 ColorChanged，自行处理业务
```

```text
用户在 hex input 输入
  -> InputState 发 InputStateEvent::Change(text)
     -> ColorPickerState 解析
        - 合法: 更新颜色 + emit ColorChanged，但不回写 input（避免光标跳动）
        - 非法: 什么都不做
  -> Enter / Blur
     -> 合法: 规范化回写 #RRGGBB(AA)
     -> 非法: 回退到当前颜色的 hex
```

---

## 3. 公共 API（实现契约）

```rust
// crates/base_ui/src/color_picker.rs

use gpui_kit::{Hsla, ...};

/// 两个 panel tab。
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub enum ColorPickerTab {
    #[default]
    Palette,
    Hsla,
}

/// Color picker 对订阅者发出的事件。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ColorPickerEvent {
    /// 用户操作导致颜色改变。程序调用 `set_color` 不会发这个事件。
    ColorChanged(Hsla),
}

/// Panel 上所有可本地化的文案。默认英文。
#[derive(Debug, Clone, PartialEq)]
pub struct ColorPickerLabels {
    pub palette: SharedString,
    pub hsla: SharedString,
    pub hue: SharedString,
    pub saturation: SharedString,
    pub lightness: SharedString,
    pub alpha: SharedString,
}

impl Default for ColorPickerLabels { /* "Palette", "HSLA", "Hue", "Saturation", "Lightness", "Alpha" */ }

/// 颜色选择器的业务状态与 Panel view。
pub struct ColorPickerState {
    color: Hsla,
    active_tab: ColorPickerTab,
    labels: ColorPickerLabels,
    hex_input: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl ColorPickerState {
    /// **推荐的创建方式**。必须在有 Window 的渲染上下文里调用：
    ///
    /// ```ignore
    /// let state = ColorPickerState::use_keyed("accent", hsla(...), window, cx);
    /// ```
    ///
    /// 内部用不 notify 的 `ColorPickerStateSlot` 作为 keyed state 载体，
    /// state 本身由 `cx.new` 创建；因此 state 的 notify 不会触发调用方页面重绘。
    /// 同一个 `id` 在连续帧里复用同一个 state；首次创建后 `default_color` 不再生效。
    pub fn use_keyed(
        id: impl Into<ElementId>,
        default_color: Hsla,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self>;

    /// 低层创建方式：要求调用方在**不** use_keyed_state 的上下文里调用，
    /// 或者自行承担 `use_keyed_state` 的 observer 会额外 notify 父 view 的后果
    /// （注意：即使不用 keyed state，GPUI 的 dirty 路径仍可能让父 view render；
    ///  这里省掉的是多余的一次 notify，不是整个父 render）。
    /// 典型用法是测试里的 `cx.new(|cx| ColorPickerState::new(...))`。
    pub fn new(
        default_color: Hsla,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self;

    /// 更新 panel 文案。相等时 no-op，可以在 render 里每帧调用。
    pub fn set_labels(&mut self, labels: ColorPickerLabels, cx: &mut Context<Self>);

    pub fn color(&self) -> Hsla;
    pub fn active_tab(&self) -> ColorPickerTab;

    /// 程序设置颜色：不发 `ColorChanged`，但会同步 hex input、`cx.notify()`。
    /// 语义与 `InputState::update_value` 一致。
    pub fn set_color(
        &mut self,
        color: Hsla,
        window: &mut Window,
        cx: &mut Context<Self>,
    );

    pub fn set_active_tab(&mut self, tab: ColorPickerTab, cx: &mut Context<Self>);
}

impl EventEmitter<ColorPickerEvent> for ColorPickerState {}

impl Render for ColorPickerState {
    /// 返回整个 Panel（tabs + 当前 tab 内容 + 底部 hex row）。
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement;
}

/// 触发器和 popup 的视图元素。
#[derive(IntoElement)]
pub struct ColorPicker {
    state: Entity<ColorPickerState>,
    size: UnifiedSize,
}

impl ColorPicker {
    pub fn new(state: &Entity<ColorPickerState>) -> Self;
}

impl UnifiedSizable for ColorPicker { /* small/medium/large */ }
impl RenderOnce for ColorPicker { /* Overlay */ }
```

### 3.1 使用方式（调用方视角）
```rust
use base_ui::color_picker::{
    ColorPicker, ColorPickerEvent, ColorPickerState,
};
use gpui_kit::{Hsla, hsla};

struct SettingsView {
    accent: Hsla,
    picker: Option<Entity<ColorPickerState>>,
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 关键：ColorPickerState::new 需要 Window，因此不能在 view 的 new() 里 cx.new(...)。
        // 用 use_keyed 而不是 window.use_keyed_state 直接创建 state：
        // 后者的 observer 会在 state 每次 notify 时额外 notify SettingsView；
        // use_keyed 用不 notify 的私有 slot 省掉这次额外 notify。
        // 注意：GPUI 的 dirty 传播仍可能让 SettingsView render，不能承诺“父页面完全不 render”。
        let state = ColorPickerState::use_keyed(
            "accent-color-picker",
            self.accent,
            window,
            cx,
        );

        if self.picker.as_ref() != Some(&state) {
            self.picker = Some(state.clone());
            // 生产代码应把 Subscription 存进 self，避免 detach 泄漏；
            // 这里为了展示 API 用法。
            cx.subscribe(&state, |this, _, event: &ColorPickerEvent, cx| {
                let ColorPickerEvent::ColorChanged(color) = event;
                this.accent = *color;
                cx.notify();
            })
            .detach();
        }

        div().child(ColorPicker::new(&state))
    }
}
```

约束：

- 一个 `ColorPickerState` 只对应一个 `ColorPicker` 触发器。两个触发器共享一个 state 会共享 `open_state`，不建议。
- 调用方不需要在每次 `ColorChanged` 时手动刷新 picker 自身；触发器/Panel 已自更新。`cx.notify()` 只用于调用方自己的 UI。
- 调用方如果调用 `set_color`，不会收到 `ColorChanged`；这是有意的“程序设置静默”语义。

### 3.2 事件发射与 hex 回写契约（必须按此实现）

`ColorChanged` 只在**用户操作导致颜色改变**时发出；`set_color` 是程序设置，静默。所有用户路径都收敛到同一个内部函数 `set_user_color(color, sync_hex, window, cx)`：

| 路径 | 谁调用 | emit `ColorChanged` | 是否回写 hex input |
|---|---|---|---|
| 调用方 `set_color` | 程序 | ❌ | ✅（规范化） |
| 点击 palette | `select_palette` | ✅（仅颜色变化时） | ✅ |
| 点击/拖动 slider | `set_channel` | ✅（仅颜色变化时） | ✅ |
| hex input `Change`，合法 | `on_hex_change` | ✅（仅颜色变化时） | ❌（避免打断输入/光标跳动） |
| hex input `Change`，非法 | `on_hex_change` | ❌ | ❌ |
| hex input Enter/Blur，合法 | `on_hex_commit` | ✅（仅颜色变化时） | ✅（无论是否变化都规范化） |
| hex input Enter/Blur，非法 | `on_hex_commit` | ❌ | ✅（回退到当前颜色） |
| `set_labels` / `set_active_tab` | 程序 | ❌ | ❌ |

`set_user_color` 的内部顺序：

```rust
if color == self.color {
    return; // 不 emit、不 notify（除非 commit 需要规范化，由 on_hex_commit 单独 write_hex）
}
self.color = color;
if sync_hex {
    self.write_hex(color, window, cx);
}
cx.emit(ColorPickerEvent::ColorChanged(color));
cx.notify();
```

**防回环**（hex 双向同步的关键）：

- `write_hex` 调用 `InputState::update_value` → gpui-base `InputState::set_value`；`set_value` 内部把 `emit_events = false`，**不会**发出 `InputEvent::Change`（`gpui-base-0.7.1/src/input/base/state.rs:940-968`）。所以“程序写 hex → 触发 Change → 再写回”的环不存在。
- 用户输入的 `Change` 路径用 `sync_hex=false`，避免程序回写打断光标。
- Enter/Blur 的 `on_hex_commit` 先 `set_user_color`（emit 变化），再无条件 `write_hex` 规范化；不要在 `set_user_color` 的相等短路里依赖它完成规范化。
- `ColorChanged` 的 payload 是 `Hsla`（0..1），不是 hex 字符串；调用方需要 hex 时自己 `hex_string`/`Rgba` 转换。

**footer 的“色块”不是 trigger 的色块**：Panel 底部 `HexRow` 的第一个 child 是独立的 `ColorWell`（30×30，当前颜色预览，需求 3 明确要求），第二个是 `Input`。Trigger 的 `ColorWell` 在面板外，不能替代它。`HexRow` 的 wrapper 加 `.debug_selector(|| "TEST_COLOR_PICKER_HEX_INPUT".into())`，测试用这个 selector 找 footer 区域。

**open/close 的机制**：`open_state` 由 `ColorPicker::render` 用 `window.use_keyed_state((picker_id, "open"), ...)` 创建；`use_keyed_state` 会 observe 它，所以 trigger 写 `open` 时**父 view 会收到 notify 并重渲染 `ColorPicker`**，Overlay 在 `opened` 为 true 时挂载 panel。Panel 不是由 trigger 子 view 自己条件挂载，也不是由 `ColorPickerState` 空态自绘。Trigger 只负责写 open；Overlay/父 render 负责开关。

---

## 4. 视觉与布局规格

### 4.1 触发器

- `Button::new(("color-picker-trigger", state.entity_id()))`
  - `.tonal()`（与 dropdown trigger 一致）
  - `.with_unified_size(size)`
  - `.selected(opened)`
  - `.aria_label(hex_string(color).as_str())`
  - `.debug_selector(|| "TEST_COLOR_PICKER_TRIGGER".into())`
  - 子元素：`ColorWell::new(color).size(swatch_size).radius(px(4.))`
- swatch_size：
  - Small: `12px`
  - Medium: `18px`
  - Large: `20px`
- Button 的 `Interactive` 默认 `flex().items_center()`，`px` 来自 `UnifiedSizeStyle`（small 4 / medium 6 / large 10），所以按钮整体接近方形。
- 不在 trigger 里放文字/caret：需求是“内容是颜色方块”。
- `UnifiedSize` 只影响触发器的尺寸（以及内部 swatch 的大小），不影响 Panel 的固定布局（Panel 始终 284 内容宽 + 固定行高）。

### 4.2 Panel 外形

- **不要依赖 `w(308).p(12)` 的 box-sizing 语义**。GPUI 的 taffy 布局对 width/padding 的内外关系容易踩坑，且不同版本可能变化。实现为：
  - 外层 panel 只负责 `p(12)`、背景、边框、阴影；
  - 内层内容显式 `w(px(284.))`，所有 tab/内容/底部行都放在这个 284 宽的内层里；
  - 面板最终宽度由“内容 284 + 左右 padding 24”自动决定；Overlay 测量实际 bounds。
- 内容宽 `PANEL_CONTENT_WIDTH = 284px`，panel padding `PANEL_PADDING = 12px`。
- 背景 `theme.components.menu.background.hsla`；边框 `1px theme.roles.line.divider.hsla`；圆角 `rounded_sm()`；阴影与 Menu 相同：
  ```rust
  .shadow(vec![
      box_shadow(0., 1., 2., 0., shadow),
      box_shadow(0., 3., 6., -2., shadow),
  ])
  ```
  `shadow = theme.components.menu.shadow.hsla`。
- `.debug_selector(|| "TEST_COLOR_PICKER_PANEL".into())`
- Placement：`Overlay::new(...).placement(Placement::Bottom).align(Align::Start)`。
  - Panel 比 trigger 宽很多，Start 对齐左边缘；Positioner 会在右边空间不足时 clamp/flip。

### 4.3 Panel 内部尺寸

```text
┌──────────────────────────────────────────────┐ 外框约 310
│  padding 12                                  │
│  ┌────────────────────────────────────────┐  │
│  │ Join(track) 28px, p_1, bg=sunken        │  │
│  │  [ Palette ] [ HSLA ]                  │  │
│  └────────────────────────────────────────┘  │
│  gap 12                                       │
│  content: Palette 284 / HSLA 126             │
│  gap 12                                       │
│  ─────────────── Separator 1px ────────────   │
│  gap 12                                       │
│  [ColorWell 30] [ Input medium 30 ]          │
│  padding 12                                  │
└──────────────────────────────────────────────┘
```

- 结构只有 **一条** 分割线：位于内容与底部 hex row 之间。tabs 与内容之间不再放 Separator（参考图 1/2 都没有这条；R3 只要求调色盘下方有分割线）。
- Palette tab 内高：`28 + 12 + 284 + 12 + 1 + 12 + 30 = 379px`；加 padding 24 = **403px**；加 1px border 上下 = 外框约 **405px**。
- HSLA tab 内高：`28 + 12 + 126 + 12 + 1 + 12 + 30 = 221px`；加 padding 24 = **245px**；外框约 **247px**。
- 外框宽：内容 284 + padding 24 + border 2 = **约 310px**。上述“内容 284 / 内高 379”是布局用的确定值；border 的 2px 只影响外框，不影响测试可用性。
- 两个 tab 高度不同是允许的（图 1/图 2 本身高度也不同）；不强制固定高度。
- 主窗口最小 1280×960，405px 的 popup 不会超出高度。

### 4.4 Tabs（Join + Button）

```rust
let active = self.active_tab;
let state = cx.entity();

Join::default()
    .w_full()
    .p_1()
    .bg(theme.roles.surface.sunken.hsla)
    .rounded_sm()
    .button(self.tab_button(
        cx, state.clone(), ColorPickerTab::Palette, active, labels.palette.clone(),
    ))
    .button(self.tab_button(
        cx, state, ColorPickerTab::Hsla, active, labels.hsla.clone(),
    ))
```

`tab_button` 的实现要求：

```rust
Button::new(match tab {
        ColorPickerTab::Palette => ("color-picker-tab-palette", cx.entity_id()).into(),
        ColorPickerTab::Hsla => ("color-picker-tab-hsla", cx.entity_id()).into(),
    })
    .ghost()
    .small()
    .flex_1()
    .justify_center()
    .aria_selected(selected)
    .text_color(if selected { theme.roles.fg.default.hsla } else { theme.roles.fg.muted.hsla })
    .when(selected, |this| this.bg(theme.roles.surface.overlay.hsla))
    .debug_selector(move || match tab {
        ColorPickerTab::Palette => "TEST_COLOR_PICKER_TAB_PALETTE".into(),
        ColorPickerTab::Hsla => "TEST_COLOR_PICKER_TAB_HSLA".into(),
    })
    .child(label)
    .on_click(move |_, _, cx| {
        state.update(cx, |state, cx| state.set_active_tab(tab, cx));
    })
```

- `Join` 会给第二个按钮去掉左侧圆角，这是 Join 的定义；不要为了视觉去改 Join。
- 用显式 `.bg()` 覆盖 `Interactive` 的 ghost 选中态（ghost 的 active 只是 `veil.active` alpha，太弱，不能表达“白色选中块”）。
- `.justify_center()` 是 Styled 方法，Button 支持。
- **接受的视觉差异**：R2 强制 Join + Button，所以两个 tab 共享边是直角，选中块是 28px 轨道内的 20px 小块；参考图是独立白色 pill。这个差异是 R2 的实现约束带来的，不是遗漏。若未来要完全复刻参考图，需要换成非 Join 的自定义 segmented control，但那违反 R2，故不做。

### 4.5 Palette tab（12×12）

- grid 尺寸：`12*20 + 11*4 = 284px` 见方。
- **不是把 284 等分成 12 列**：每格 20px、每两个格子之间 4px gap，pitch 24px；`284/12≈23.67` 只用于说明“不能拿总宽除以 12 当 pitch”。绘制用 `PALETTE_STEP=24`/`PALETTE_CELL=20`；命中测试按 `scale = bounds.width / PALETTE_SIZE` 缩放，最后一列右缘仍精确落在 284。
- **12 列几何自检（避免 12×24=288 的误算）**：第 0..11 列的起点是 `0, 24, ..., 264`（共 11 个 gap，不是 12 个），最后一格右缘是 `264 + 20 = 284`。`12*24=288` 会多算一个 trailing gap；命中测试对 `x ∈ [0, 284)` 做 `floor(x/24)`，`col` 最大 11、`within` 最大 19.999，不会产生第 12 列或 288 的坐标。
- 构造：`PaletteGrid::new(state.clone(), color, labels.palette.clone())`；`color` / `label` 是字段，`render` 里不 `state.read(cx)`。
- 容器：`div().relative().w(px(PALETTE_SIZE)).h(px(PALETTE_SIZE))`，`.id(("color-picker-palette", state.entity_id()))`，`.role(Role::Group)`，`.aria_label(label)`，`.cursor_pointer()`。
  - 注意 `(EntityId, _)` **不能**转 `ElementId`；必须用 `(&'static str, EntityId)`（`("color-picker-palette", state.entity_id())`）或先 `let picker_id: ElementId = ("color-picker", state.entity_id()).into();`。
- 子元素：`canvas(prepaint, paint).absolute().size_full()`。`prepaint` 里 `bounds_state.set(bounds)`（`Rc<Cell<Bounds<Pixels>>>`，本身就是 canvas 的 bounds，不需要额外 `on_prepaint`/`ElementExt`）；`paint` 里画格子。
- 绘制：
  - 遍历 `row in 0..12`, `col in 0..12`：
    - `origin = point(bounds.left() + px(PALETTE_STEP * col as f32), bounds.top() + px(PALETTE_STEP * row as f32))`
    - `fill(Bounds::new(origin, size(px(PALETTE_CELL), px(PALETTE_CELL))), swatch.hsla).corner_radii(px(4.))`
  - 选中环：如果当前颜色命中 `PALETTE` 的某个 swatch，在对应 cell 的 bounds 内缩 1px 画 2px ring：
    ```rust
    let ring = selection_ring_color(cell.hsla);
    let inner = Bounds::new(
        point(origin.x + px(1.), origin.y + px(1.)),
        size(px(PALETTE_CELL - 2.0), px(PALETTE_CELL - 2.0)),
    );
    window.paint_quad(
        fill(inner, hsla(0., 0., 0., 0.))
            .border_widths(px(2.))
            .border_color(ring)
            .corner_radii(px(3.)),
    );
    ```
- 命中测试（鼠标 down）：
  ```rust
  fn palette_index_at(bounds: Bounds<Pixels>, position: Point<Pixels>) -> Option<usize> {
      let width = bounds.size.width.as_f32();
      let height = bounds.size.height.as_f32();
      let x: f32 = (position.x - bounds.left()).into();
      let y: f32 = (position.y - bounds.top()).into();
      if !(0.0..width).contains(&x) || !(0.0..height).contains(&y) {
          return None;
      }
      // 12 个 cell + 11 个 gap 的整体缩放到 bounds；不能直接用 width/12 当 pitch
      // （PALETTE_SIZE = 12*20 + 11*4，pitch 是 24，不是 284/12≈23.67）。
      let scale_x = width / PALETTE_SIZE;
      let scale_y = height / PALETTE_SIZE;
      let step_x = PALETTE_STEP * scale_x;
      let step_y = PALETTE_STEP * scale_y;
      let cell_x = PALETTE_CELL * scale_x;
      let cell_y = PALETTE_CELL * scale_y;
      let col = (x / step_x).floor() as usize;
      let row = (y / step_y).floor() as usize;
      if col >= PALETTE_COLS || row >= PALETTE_ROWS {
          return None;
      }
      let within_x = x - col as f32 * step_x;
      let within_y = y - row as f32 * step_y;
      if within_x >= cell_x || within_y >= cell_y {
          return None; // 落在 gap 上
      }
      Some(row * PALETTE_COLS + col)
  }
  ```
  - `.on_mouse_down(MouseButton::Left, ...)`：命中则 `state.update(cx, |state, cx| state.select_palette(index, window, cx))`（`Entity::update` 第一个参数是 `cx`）。
  - 不实现 palette 的 hover 高亮/拖动选色：需求没有要求，且会增加每帧 hover 状态；保持 1 个 canvas 的简洁。
  - 光标：`cursor_pointer()` 即可。
- 选中 ring 的颜色规则（保证黑白格都可见）：
  ```rust
  fn selection_ring_color(color: Hsla) -> Hsla {
      let [r, g, b, _] = rgba8(color);
      let luma = 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32;
      if luma > 140.0 {
          hsla(0., 0., 0., 0.85) // 亮色格：深色环
      } else {
          hsla(0., 0., 1., 0.95) // 暗色格：浅色环
      }
  }
  ```
- 没有每格 border：144 个额外 quad 会翻倍绘制量，且参考图本身没有格线。选中态已经能表达位置。

### 4.6 HSLA tab（图 1 布局）

行结构（4 行）：

```text
Hue        [════════ gradient track ════════]   188
Saturation [════════ gradient track ════════]    86
Lightness  [════════ gradient track ════════]    53
Alpha      [════════ gradient track ════════]   100
```

- 行：`div().flex().flex_row().items_center().gap(SLIDER_ROW_INNER_GAP).h(SLIDER_ROW_HEIGHT)`。
- 行间距：`.gap(SLIDER_ROW_GAP)`（4 行内容总高 126）。
- 标签：`div().w(SLIDER_LABEL_WIDTH).text_size(rems(0.75)).text_color(fg.muted.hsla).child(label)`。
- 数值：`div().w(SLIDER_VALUE_WIDTH).text_size(rems(0.75)).text_color(fg.muted.hsla).text_align(TextAlign::Right).child(value)`。
  - 用 `rems(0.75)` 而不是 `px(12.)`，与 `UnifiedSize::text_size` 的单位一致，避免 `AbsoluteLength` 转换差异。
  - Hue：`(h * 360.0).round() as i32`（允许 360）。
  - S/L/A：`(v * 100.0).round() as i32`。
  - 用 `format!("{value}")` 生成 `SharedString`；每帧最多 4 个短字符串，不影响性能。
- 轨道：`div().relative().flex_1().h(px(TRACK_HEIGHT))`，`.id((channel.id_prefix(), state.entity_id()))`。
  - `channel.id_prefix()` 返回 4 个固定 `&'static str`：`"color-picker-hue-track"` / `"color-picker-saturation-track"` / `"color-picker-lightness-track"` / `"color-picker-alpha-track"`。不要用 `(EntityId, _)`，也不要用 `format!`。
  - `.role(Role::Slider)`
  - `.aria_label(row label)`
  - `.aria_numeric_value(display_value as f64)`、`.aria_min_numeric_value(0.)`、`.aria_max_numeric_value(max)`、`.aria_orientation(Orientation::Horizontal)`
  - `.debug_selector(move || channel.debug_selector().into())`，其中 `debug_selector()` 返回 `"TEST_COLOR_PICKER_HUE_TRACK"` / `"TEST_COLOR_PICKER_SATURATION_TRACK"` / `"TEST_COLOR_PICKER_LIGHTNESS_TRACK"` / `"TEST_COLOR_PICKER_ALPHA_TRACK"`。`debug_selector` 的签名是 `FnOnce() -> String`，所以 `.into()` 是 `String`；闭包在元素构造时调用一次，不是每帧。
  - 子元素：`canvas(prepaint, paint).absolute().size_full()`。`prepaint` 里把 canvas 的 bounds 写进 `Rc<Cell<Bounds<Pixels>>>`（供 `on_mouse_down` 用），`paint` 里画切片。
- 轨道宽：`284 - 72 - 8 - 40 - 8 = 156px`。`TRACK_HEIGHT = 14`，`TRACK_RADIUS = 7`。
- **行宽算术**：`72（label） + 8（gap） + 156（track flex） + 8（gap） + 40（value） = 284`；两个 gap 都是 `SLIDER_ROW_INNER_GAP = 8`，不是 12。若把 gap 误写成 12，会变成 292 并溢出内容宽。
- 轨道绘制：**256 条实色切片 + 圆角帽 + 1 个 thumb**（详见 §6.2）。256 条 × 0.61px，端部近似更平滑。
- Alpha 轨道**不画 checkerboard**：切片直接叠在 panel 背景上，浅色主题读作“panel 白 → 当前色”，深色主题读作“panel 深 → 当前色”，与参考图的色带一致。透明度的棋盘格只在 `ColorWell`（触发器和底部色块）里画，因为那里需要表达“这个颜色本身是半透明的”。这是刻意选择，不是遗漏。
- 数值文本与图 1 一致：右对齐，字号 12，muted 色。

### 4.7 底部 hex row

```rust
div()
    .flex()
    .flex_row()
    .items_center()
    .gap_2()
    .child(ColorWell::new(color).size(px(30.)).radius(px(6.)))
    .child(div().flex_1().child(Input::new(&self.hex_input).medium()))
```

- `ColorWell` 30×30，圆角 6；画 checkerboard（仅当 `a < 1`）、颜色、1px `line.divider` 边。
- **它是需求 3 的“当前选择的颜色方块”**，是独立实例，不是 trigger 里的那个 18px ColorWell；两者都从同一个 `color` 绘制。
- `Input` 用 base_ui 的 `input::Input`，`medium()`（高 30），占满剩余 246px。
- Input 的初始值是 `hex_string(default_color).as_str()`，placeholder `#RRGGBB`。
- Input 的边框/焦点色由 base_ui 的 `Input` 自己处理；我们只负责给宽度和 `hex_input`。
- 整个 hex row 的 wrapper `.debug_selector(|| "TEST_COLOR_PICKER_HEX_INPUT".into())`；测试用它确认 footer 存在，用 `state.hex_input` 读值/聚焦。
- 行宽：`30 + 8 + 246 = 284`。

---

## 5. 颜色模型与数据

### 5.1 单位约定

- 内部统一使用 `gpui_kit::Hsla`，`h/s/l/a ∈ 0..=1`。
- 任何显示为角度的值乘 360；显示为百分比的值乘 100。
- 不要把 `h` 存成 0..360；不要混用 `color::Hsl`（单位是 0..360/0..100）和 `gpui::Hsla`。

### 5.2 hex 解析

```rust
fn parse_hex(text: &str) -> Option<Hsla> {
    let text = text.trim();
    let text = text.strip_prefix('#').unwrap_or(text);
    if !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let (width, has_alpha) = match text.len() {
        3 => (1, false),
        4 => (1, true),
        6 => (2, false),
        8 => (2, true),
        _ => return None,
    };
    let component = |i: usize| -> Option<f32> {
        let start = i * width;
        let raw = u8::from_str_radix(&text[start..start + width], 16).ok()?;
        let raw = if width == 1 { raw * 0x11 } else { raw };
        Some(raw as f32 / 255.0)
    };
    Some(Hsla::from(Rgba {
        r: component(0)?,
        g: component(1)?,
        b: component(2)?,
        a: if has_alpha { component(3)? } else { 1.0 },
    }))
}
```

规则：

- 接受 `#rgb` / `#rgba` / `#rrggbb` / `#rrggbbaa`，`#` 可省略，大小写不限，前后空白 trim。
- 短格式按 nibble 复制（`#f00` → `#ff0000`），不是缩放。
- 拒绝空串、长度不对、含非 hex 字符、`+`/`-`、`0x` 等。
- 只支持 hex；不支持 `rgb()`/`hsl()`/颜色名。Input placeholder 明确写 `#RRGGBB`，不制造额外承诺。

### 5.3 hex 格式化

```rust
use smol_str::{SmolStr, format_smolstr};

fn rgba8(color: Hsla) -> [u8; 4] {
    let rgba = Rgba::from(color);
    let q = |v: f32| (v.clamp(0., 1.) * 255.0).round() as u8;
    [q(rgba.r), q(rgba.g), q(rgba.b), q(rgba.a)]
}

fn hex_string(color: Hsla) -> SmolStr {
    let [r, g, b, a] = rgba8(color);
    if a < 255 {
        format_smolstr!("#{r:02X}{g:02X}{b:02X}{a:02X}")
    } else {
        format_smolstr!("#{r:02X}{g:02X}{b:02X}")
    }
}
```

- 用 `round()` 而不是 `as u8` 截断：截断会把 0.999 变成 254，产生差一错误。
- alpha 只在量化后 `< 255` 时输出；这样 `#RRGGBB` 的显示与“八位量化后不透明”一致。
- 所有 hex 显示大写（参考图 `#21D3ED`、`#C084FB`）。
- 返回 `SmolStr`（AGENTS §2.2：短字符串优先 `SmolStr`，格式化用 `format_smolstr!`）。调用方：
  - `InputStateBuilder::default_value(hex_string(default).as_str())`
  - `InputState::update_value(hex.as_str(), window, cx)`
  - `Button::aria_label(hex.as_str())`
  - 测试里比较 `hex.as_str()`。
- 不要先 `format!` 成 `String` 再转 `SmolStr`；`format_smolstr!` 本身在 7-9 字符时通常不分配。

### 5.4 12×12 调色盘

**行 = 色相家族**（12 行），**列 = 明度阶**（12 列），生成 144 个 HSL 颜色：

```rust
const PALETTE_COLS: usize = 12;
const PALETTE_ROWS: usize = 12;
const PALETTE_SATURATION: f32 = 0.72;

/// 12 个明度阶：暗 -> 亮，故意在暗端更密（与参考图的节奏一致）。
const PALETTE_LIGHTNESS: [f32; 12] = [
    0.06, 0.12, 0.18, 0.26, 0.34, 0.42,
    0.50, 0.60, 0.70, 0.80, 0.90, 0.96,
];

/// row 0 = 中性灰阶；row 1..12 = 11 个均匀分布的色相（0..1）。
fn palette_hue(row: usize) -> Option<f32> {
    (row > 0).then(|| (row - 1) as f32 / (PALETTE_ROWS as f32 - 1.0))
}
```

- row 0 的 12 个格子是 `s = 0` 的灰阶，从近黑到近白。
- row 1..11 是 11 个色相：0°、32.7°、65.5°、98.2°、130.9°、163.6°、196.4°、229.1°、261.8°、294.5°、327.3°（即 `(row-1)/11`）。这样 11 个色相均匀覆盖整个色环，没有 330° 的缺口，同时保留了中性行。
- 每个格子：`hsla(hue, PALETTE_SATURATION, lightness, 1.0)`；中性行 `s = 0.0`。
- 行/列方向与参考图一致（行 = 色相家族，列 = 明度）；但 **行数是刻意扩展的，不是参考图的 9 行**，见下面的“与参考图的差异”。

**与参考图的差异（刻意，不是笔误）：**

- 参考图 2 的网格是 **9 行**（1 行中性 + 8 个色相家族）× **12 列**明度；需求原文要求 **12×12**。两者不能同时字面满足。
- 本设计的默认选择：**12 行 = 1 行中性 + 11 个均匀分布的色相**，既保留参考图的“中性行 + 色相行 + 明度列”的结构，又严格满足 12×12。它比参考图多 3 个色相家族，不是逐格复刻。
- 如果产品要求严格复刻参考图的 9 行，则把 `PALETTE_ROWS` 改为 9、`palette_hue` 改为 `(row-1)/8`，并重算网格高度（9 行 × 20 + 8 × 4 = 212）与面板高度；但那样就不满足用户明确的“12×12”，所以 **不是默认**。
- 这是一个需要产品确认的取舍；在确认之前，按 12×12 默认实现。文档不再声称“与参考图完全一致”。

数据结构：

```rust
struct PaletteSwatch {
    hsla: Hsla,
    rgb: [u8; 3], // 用来做 8-bit 精确命中
}

static PALETTE: LazyLock<[PaletteSwatch; 144]> = LazyLock::new(|| {
    std::array::from_fn(|index| {
        let row = index / PALETTE_COLS;
        let col = index % PALETTE_COLS;
        let hsla = match palette_hue(row) {
            Some(hue) => hsla(hue, PALETTE_SATURATION, PALETTE_LIGHTNESS[col], 1.0),
            None => hsla(0., 0., PALETTE_LIGHTNESS[col], 1.0),
        };
        let [r, g, b, _] = rgba8(hsla);
        PaletteSwatch { hsla, rgb: [r, g, b] }
    })
});
```

- `LazyLock` 只在第一次渲染 Palette tab 时计算 144 次 HSL→RGB，之后是静态读取。
- 不要在 `render` 里重新生成调色盘，也不要把 144 个颜色存进每个 `ColorPickerState`（多个 picker 会重复 3KB+ 的初始化）。

**命中规则**：

```rust
fn palette_index(color: Hsla) -> Option<usize> {
    let [r, g, b, a] = rgba8(color);
    if a != 255 {
        return None;
    }
    PALETTE.iter().position(|swatch| swatch.rgb == [r, g, b])
}
```

- 用 8-bit 量化后的 RGB 比较，而不是 `Hsla == Hsla`：
  - 用户从 hex 输入/外部设置得到的颜色，天然是 8-bit；
  - 点击调色盘时，写入的是 `PALETTE[index].hsla`，其 `rgba8` 必然等于 `PALETTE[index].rgb`（同一函数生成），所以“点击后必然命中”。
- 命中要求 **量化后的 alpha == 255**（函数里判断 `rgba8(color)[3] != 255` 就返回 None），而不是直接比较 `color.a < 1.0`。这样 `a = 0.999` 量化成 255 时按“显示为不透明”处理，和 `hex_string` 去掉 AA 的行为一致。
- alpha 明显小于 1（例如 0.5）的颜色不命中任何调色盘格（调色盘全是 opaque），这符合“当前输入颜色命中某个方块”的语义。

**点击调色盘**：

```rust
state.update(cx, |state, cx| {
    state.set_user_color(PALETTE[index].hsla, true, window, cx);
});
```

- 不关闭 Panel。
- 若颜色已经是该格，仍会走一次 `set_user_color`；内部比较相等后应 no-op（不 emit、不 notify），避免重复事件。

### 5.5 HSLA 滑杆映射

- 滑杆的值直接是 0..1 的 `t`；不做 step 量化。拖动是连续 f32。
- Hue 轨道显示的是 `h*360`，但内部仍是 `h`。
- 点击/拖动映射：`t = ((x - track.left()) / track.width()).clamp(0., 1.)`。
- 颜色由 state 的当前 `Hsla` 派生：
  - Hue → `color.h = t`
  - Saturation → `color.s = t`
  - Lightness → `color.l = t`
  - Alpha → `color.a = t`
  - 其余分量不变。
- 拖动过程中实时 emit `ColorChanged`；这是“颜色连续变化”的语义，和 Input 的 `Change` 一致。

---

## 6. 渲染与性能设计

### 6.1 元素树

**Palette tab：**

```text
Panel (div, 1 个元素)
├── Join (1)
│   ├── Button (1)
│   └── Button (1)
├── PaletteGrid (div + canvas, 2 个元素)
├── Separator (1)
└── HexRow (div + ColorWell(div+canvas) + Input(div + editor), ~6)
```

**HSLA tab：**

```text
Panel (div)
├── Join + 2 Button
├── 4 × SliderRow (div + label + Track(div+canvas) + value, ~4 个元素/行)
├── Separator
└── HexRow
```

没有 144 个格子元素，也没有 12×12 的 flex/grid 布局计算。Palette tab 的 layout 只需要算一次固定尺寸。

### 6.2 轨道绘制算法（核心）

每条轨道用一个 `canvas`，在一个 paint pass 里画：

1. `TRACK_SLICES = 256` 条实色切片；
2. 一个 thumb（1 个 quad）。

```rust
// TRACK_SLICES 来自 §2.4，不要在这里再定义一份 SLICES
let left = bounds.left().as_f32();
let top = bounds.top().as_f32();
let width = bounds.size.width.as_f32();
let height = bounds.size.height.as_f32();   // TRACK_HEIGHT
let radius = TRACK_RADIUS;
if width <= 0.0 || height <= 0.0 {
    return;
}

for i in 0..TRACK_SLICES {
    let t0 = i as f32 / TRACK_SLICES as f32;
    let t1 = (i + 1) as f32 / TRACK_SLICES as f32;
    let t = (t0 + t1) / 2.0;                // 取切片中点，避免端点偏色

    let color = match channel {
        Hue        => hsla(t, 1.0, 0.5, 1.0),
        Saturation => hsla(base.h, t, base.l, 1.0),
        Lightness  => hsla(base.h, base.s, t, 1.0),
        Alpha      => hsla(base.h, base.s, base.l, t),
    };

    let x0 = left + width * t0;
    let x1 = left + width * t1;
    let center = (x0 + x1) / 2.0;
    let d = (center - left).min((left + width) - center);
    let inset = if d < radius {
        // 圆角帽：在距离端点 d 处，上下各收缩 inset
        radius - (radius * radius - (radius - d) * (radius - d)).sqrt()
    } else {
        0.0
    };

    let slice_height = height - inset * 2.0;
    if slice_height <= 0.0 {
        continue;
    }

    window.paint_quad(fill(
        Bounds::new(
            point(px(x0), px(top + inset)),
            size(px(x1 - x0), px(slice_height)),
        ),
        color,
    ));
}
```

要点：

- 切片是 **实色**，不经过 gradient shader，因此 Windows/wgpu 颜色一致。
- **Lightness 轨道用当前 `base.s`**（不是固定 1.0）：轨道展示的是“当前颜色家族”的明度变化，thumb 所在的 `l=t` 处应与 `hsla(h, s, l)` 同族；Saturation 轨道同理使用当前 `base.l`。Hue 轨道是唯一使用固定 `s=1,l=0.5` 的轨道（色相条本身要展示纯色相）。注意 thumb 本身是固定的 `surface.overlay` 白块，不参与颜色插值；这里讲的是轨道颜色，不是 thumb 填充色。
- 圆角帽通过 `inset` 近似：每个 0.61px 宽的切片上下收缩，形成视觉上的 pill 端帽；不需要 `overflow_hidden` 的圆角裁剪（GPUI 也没有）。最左/最右切片的 `height - inset*2` 可能 ≤ 0，直接 `continue` 跳过。
- 256 条切片在 156px 宽的轨道上约 0.61px/条；端部仍是 1px 级近似（不是 path 的像素级平滑），在 14px 高的轨道上视觉可接受；如果产品要求绝对平滑，后续可以把两端 7px 改成两块 2-stop 端帽渐变，但不要把切片提高到 1024。
- 每帧总 quad 数：
  - Hue 256 + Saturation 256 + Lightness 256 + Alpha 256 = 1024
  - 4 个 thumb = 4
  - Palette tab 不进树，所以 HSLA tab 约 1028 quad
- 这不是瓶颈：GPUI 的 quad 是批处理的 GPU instance；1024 个 `paint_quad` 调用的 CPU 成本远小于 1024 个元素的 layout/prepaint，更远小于 144 个带 hover 的 div。作为对比，gpui-component 的 hue 条用 96 个 `div`，每个 div 都要走一次 layout + paint。

**Thumb：**

```rust
// render 里先取 Copy 值，paint 闭包内不要捕获 &Theme
let thumb_fill = cx.theme().roles.surface.overlay.hsla;
let thumb_border = hsla(0., 0., 0., 0.35);

let left = bounds.left().as_f32();
let top = bounds.top().as_f32();
let width = bounds.size.width.as_f32();
let height = bounds.size.height.as_f32();
let x = (left + width * value_t).clamp(left + THUMB_DIAMETER / 2.0, left + width - THUMB_DIAMETER / 2.0);
let y = top + height / 2.0;
window.paint_quad(
    fill(
        Bounds::new(
            point(px(x - THUMB_DIAMETER / 2.0), px(y - THUMB_DIAMETER / 2.0)),
            size(px(THUMB_DIAMETER), px(THUMB_DIAMETER)),
        ),
        thumb_fill,
    )
    .border_widths(px(1.))
    .border_color(thumb_border)
    .corner_radii(px(THUMB_DIAMETER / 2.0)),
);
```

- 白色/深色 thumb 在轨道两端都能被 1px 半透明黑边区分。
- thumb 被 clamp 在轨道内，避免盖到 label/value。
- 轨道颜色随当前颜色变化（Saturation 用当前 l，Lightness 用当前 s，Alpha 用当前 h/s/l），这些都是直接构造 `Hsla`，没有颜色空间转换。

**为什么不用 `linear_gradient`？**

- 只支持 2 stop，hue 需要 7 个端点；可以拆成 6 段 2-stop，但：
  - Windows 上 6 段是精确的（HSL 每个 60° 段在 sRGB 分量上是线性的）；
  - wgpu 上 0.3.8 的 shader 会多做一次 gamma，颜色偏亮；
  - 6 段需要为第一/最后一段单独处理圆角，代码分支更多；
  - 256 条实色切片的成本已经足够低，且跨后端一致。
- 因此，**Hue 色带“能画”，但用 canvas 逐段实色画，而不是用 GPUI 的 gradient 画。**

### 6.3 ColorWell（颜色方块）

```text
canvas(bounds):
  1. 底：fill(bounds, surface.overlay).corner_radii(radius)
  2. 若 color.a < 1:
       fill(bounds, checkerboard(surface.sunken, 4.0)).corner_radii(radius)
  3. fill(bounds, color).corner_radii(radius)
  4. fill(bounds, hsla(0., 0., 0., 0.)).border_widths(px(1.)).border_color(line.divider).corner_radii(radius)
```

- `checkerboard` 的 size=4px：30px 的 swatch 是 7.5 格，16px 是 4 格，视觉正常。
- 第 4 步的 1px 边让白色/近白色块在白色 panel 上也有边界。
- `ColorWell` 只有 `color/size/radius` 三个字段；`Render` 里从 `cx.theme()` 读 `base = surface.overlay`、`checker = surface.sunken`、`border = line.divider`，拷贝成 `Hsla` 后交给 paint 闭包。paint 闭包只捕获这些 Copy 值，满足 `'static`。
- 触发器和底部各一个，共 ~6-8 个 quad。

### 6.4 自更新边界（减少重复 notify；不承诺父 render 不跑）

这是本设计最容易被实现错的地方，而且有两个独立的坑。

**坑 1：把 state 本身放进 `window.use_keyed_state`。**

```rust
// 错误：state 的每次 notify 都会被 use_keyed_state 的 observer 转发给当前 view
let state = window.use_keyed_state("picker", cx, |window, cx| {
    ColorPickerState::new(color, window, cx)
});
```

`use_keyed_state` 会给当前 view 注册 observe（`window.rs:4216-4217,7201-7206`）。state 在拖动 slider 时每帧 `cx.notify()`，于是调用方整个页面每帧重绘。这正是 `ColorPickerState::use_keyed` 存在的原因：

```rust
struct ColorPickerStateSlot {
    state: Entity<ColorPickerState>,
}

// 下面的函数体放进**唯一**的 `impl ColorPickerState` 块（不要新建第二个 inherent impl，
// workspace 的 clippy::multiple_inherent_impl = "forbid" 会拒绝）。
pub fn use_keyed(id: impl Into<ElementId>, default_color: Hsla, window: &mut Window, cx: &mut App) -> Entity<Self> {
    let key = (id.into(), "color-picker-state-slot");
    let slot = window.use_keyed_state(key, cx, |window, cx| {
        ColorPickerStateSlot {
            state: cx.new(|cx| ColorPickerState::new(default_color, window, cx)),
        }
    });
    slot.read(cx).state.clone()
}
```

- `ColorPickerStateSlot` 自己从不 `cx.notify()`，所以 `use_keyed_state` 注册在父 view 上的 observe 不会触发。
- state 由 `cx.new` 创建，不是 keyed state；state 的 `cx.notify()` 仍会按 GPUI 的 dirty 传播标脏它自己和祖先（`window.rs:2191-2204`），但不会再额外多出 keyed observer 的那一次 `cx.notify(current_view)`。
- 调用方拿到的 `Entity<ColorPickerState>` 由 slot 持有，生命周期与 keyed state 相同。

**坑 2：把 trigger 直接放进 `use_keyed_state`。**

```rust
// 错误：trigger 每次因为颜色变化 notify，都会额外 notify 当前 view
let trigger = window.use_keyed_state((id, "trigger"), cx, |window, cx| {
    ColorPickerTrigger::new(...)
});
```

`use_keyed_state` 会给当前 view 注册 observe（`window.rs:4216-4217,7201-7206`）。trigger 每次因为颜色变化 `cx.notify()`，都会多一次 `cx.notify(current_view)`；这是**额外的 notify**，不是父 render 的唯一来源（GPUI 的 dirty 传播本身也会把祖先标脏）。拖动时多一次 notify 仍然浪费，所以避开它。

**本设计做法（trigger 部分）：**

```rust
struct TriggerSlot {
    trigger: Entity<ColorPickerTrigger>,
}

let slot = window.use_keyed_state((picker_id.clone(), "trigger-slot"), cx, |_, cx| {
    TriggerSlot {
        trigger: cx.new(|cx| ColorPickerTrigger::new(state.clone(), open.clone(), size, cx)),
    }
});
let trigger = slot.read(cx).trigger.clone();
```

- `TriggerSlot` 自己从不 `cx.notify()`，所以 `use_keyed_state` 注册在父 view 上的 observe 不会触发。
- `ColorPickerTrigger` 观察 `state`/`open`，并只 `cx.notify()` 自己；它作为子 view 在元素树中重绘。
- Panel 是 `ColorPickerState` 的 `Render` 输出，state notify 时它自己重绘。
- **不要承诺父 view 完全不 render**：GPUI 的 `mark_view_dirty` 会把祖先标脏，`ViewElement::request_layout` 对非 `.cached()` view 会执行 render；父 render 是否跑由 GPUI 决定。本设计保证的是：
  1. 组件不额外 `notify` 父 view（省掉 keyed observer 的重复通知）；
  2. Panel/Trigger 的更新不依赖调用方在 `ColorChanged` 里手动 `notify`；
  3. 144 个色块只在 Palette tab 挂载时绘制，HSLA 拖动时不构建 PaletteGrid。
- 调用方订阅 `ColorChanged` 后如果主动 `cx.notify()` 刷新自己的预览/表单，那是调用方自己的 render 成本，不属于组件内部。

**notify/重绘矩阵（“父 view”列表示组件是否主动 notify 父 view，不是 GPUI 是否 render）：**

| 事件 | ColorPickerState | ColorPickerTrigger | 父 view（组件主动 notify） |
|---|---|---|---|
| 拖动 slider / 点 palette | ✅（Panel） | ✅（swatch） | ❌（GPUI dirty 传播仍可能 render；调用方主动 notify 除外） |
| hex input 输入 | ✅（Panel） | ✅（swatch） | ❌（同上） |
| 打开/关闭 popup | ❌ | ✅（pressed 态） | ✅（open_state keyed；低频） |
| 切换 tab | ✅（Panel） | ❌ | ❌ |
| 外部 `set_color` | ✅（Panel） | ✅（swatch） | ❌ |
| 主题切换 | ✅（下次 render 读 theme） | ✅ | 由全局 `refresh_windows` 触发 |

- 主题切换时 `style::theme::apply` 会 `cx.refresh_windows()`，所有窗口重绘；Panel/Trigger 在 render 里读 `cx.theme()`，所以自动生效，不需要缓存颜色。
- 不要在 `ColorPickerState` 里缓存 `ResolvedColor/Hsla` 形式的主题色；每帧从 `cx.theme()` 读 `hsla` 字段。主题切换时缓存会失效。

### 6.5 性能预算与禁止项

- Palette tab 每帧 ≤ 145 个 quad；HSLA tab 每帧 ≤ ~1030 个 quad。
- 每帧不得：
  - 重新生成 144 个调色盘颜色；
  - 为每个格子创建 Element/Entity/订阅；
  - 做 HSL→RGB 以外的颜色空间转换（Oklch/Oklab 不进入本组件）；
  - 为每个格子设置 `.debug_selector` 或 `.id`；
  - 在 `paint` 闭包里分配 `Vec`/`String`（paint 闭包只读 `'static` 数据与 Copy 值）。
- 子元素（`PaletteGrid`/`HslaSlider`/`ColorWell`）的 `RenderOnce` **不得**在 render 里 `state.read(cx)`。颜色/通道等显示数据由 `ColorPickerState::render` 作为字段传入；state 只用于事件回调里的 `state.update`。这是职责/依赖清晰的要求：子元素的 render 只依赖传入数据，不反向读父 state；不要把它描述成运行期 panic 风险（GPUI 的 RenderOnce render 在 request_layout 阶段执行，父 Entity 的 render 租约已结束）。
- `ColorPickerState::render` 每次重绘会重新构造 4 个 SliderRow 的元素；这是 4 行，不是 144 行，成本可接受。
- 调色盘的 144 个颜色是 `LazyLock` 静态数据，多个 picker 共享。
- 不在 HSLA tab 挂载 PaletteGrid：`match active_tab` 只构建当前 tab 的内容。切换 tab 时只重建当前 tab，避免 144 个 quad 在 HSLA 拖动时被无意义地重画。

---

## 7. 交互细节

### 7.1 打开/关闭

- Trigger 是 `Overlay` 的 trigger；`Overlay` 负责：
  - 点击 trigger 之外 → `on_mouse_down_out` 关闭；
  - 点击 panel 内部 → 不关闭；
  - Presence 淡入淡出（`ANIMATION_TICK`）；
  - `Positioner` 在下方空间不足时翻到上方。
- 关闭只有两种途径：点击 panel 外、再次点击 trigger（trigger 在 panel 外，属于 outside press）。
- **不实现 Escape 关闭**：`Overlay`/`Dropdown` 当前也没有 Escape 处理；本次不引入 window-level key handler。若后续产品要求，可以在 trigger/panel 上绑定焦点和 key context，属于独立改动。
- 选中 palette / 拖动 slider **不关闭** panel：需求 3 要求选中色块显示 border，关闭会让这个反馈失去意义。

### 7.2 Palette 点击

- 只处理 `MouseButton::Left` 的 `on_mouse_down`。
- 不做拖动选色（需求未要求）；指针在格子上按下即选中。
- 命中 gap 不选中。
- 点击当前已选中的格子：`set_user_color` 内部判断颜色相等 → 不 emit、不 notify。

### 7.3 Slider 点击与拖动

- 点击：`on_mouse_down` 立即按位置更新值（和 gpui-base Slider 一致）。
- 拖动：同一元素上 `on_drag` + `on_drag_move`。
  - `on_drag` 的 payload：`HslaDrag { picker: EntityId, channel: HslaChannel }`。
  - `on_drag` 的预览 view：`HslaDragPreview`（unit struct，`Render` 返回 `Empty`），避免出现拖拽 ghost。
  - `on_drag_move` 里用 `event.bounds`（元素 bounds）和 `event.event.position`（window 坐标）重新映射 `t`；即使用户把鼠标拖出轨道，也能继续更新。
  - 用 payload 的 `picker` + `channel` 过滤，避免多个 picker/slider 互相干扰。
- `on_drag` 只在鼠标拖动超过阈值后进入拖动；但 `on_mouse_down` 已经更新了第一次位置，所以短点击也正确。
- 轨道容器需要 `.id(...)`（GPUI 的 hitbox 需要稳定 id）和 `.cursor_pointer()`。
- **不做 touch 拖动**：当前组件只面向桌面鼠标；gpui-base 的 slider 有 touch 处理，本组件 v1 不引入。这是明确的非目标，不是遗漏。

### 7.4 Hex input

- Input 是 base_ui 的 `InputState`，由 `ColorPickerState` 自己创建并持有：
  ```rust
  let input_id: ElementId = ("color-picker-hex", cx.entity_id()).into();
  let hex_input = cx.new(|cx| {
      InputStateBuilder::new(input_id)
          .default_value(hex_string(default_color).as_str())
          .placeholder("#RRGGBB")
          .build(window, cx)
  });
  ```
  - 这里 `cx` 是 `Context<ColorPickerState>`，可以 `cx.new` 创建 `Entity<InputState>`（`InputState` 来自 `crate::input`；gpui-base 的 ColorPickerState 也是这么做的）。
- 订阅：
  ```rust
  let subscription = cx.subscribe_in(
      &hex_input,
      window,
      |this, _, event: &InputStateEvent, window, cx| match event {
          InputStateEvent::Change(text) => this.on_hex_change(text, window, cx),
          InputStateEvent::PressEnter(text) => this.on_hex_commit(text, window, cx),
          InputStateEvent::Blur(text) => this.on_hex_commit(text, window, cx),
      },
  );
  ```
  - 必须把 `Subscription` 存进 `_subscriptions`，否则订阅立刻 drop，输入框永远不会驱动颜色。
- `on_hex_change`：
  - `parse_hex(text)` 成功 → `set_user_color(color, sync_hex=false, window, cx)`；即更新颜色并 emit，但 **不回写 input**，避免光标跳到末尾、避免打断输入。
  - 失败 → 什么都不做。
- `on_hex_commit`（Enter/Blur）：
  - 成功 → 先 `set_user_color(color, sync_hex=false, ...)`（若颜色变化则更新+emit），然后 **无论颜色是否变化都** `write_hex(self.color)` 写回规范化 hex（大写、按需带 alpha）。不要用 `sync_hex=true` 依赖 `set_user_color` 的内部相等短路：那样输入 `#fff` 而当前已经是白色时不会把文本规范成 `#FFFFFF`。
  - 失败 → `write_hex(self.color)`，把输入框恢复成当前颜色。
- `write_hex` 使用 `self.hex_input.update(cx, |input, cx| input.update_value(text, window, cx))`。
  - `base_ui::InputState::update_value` → gpui-base `InputState::set_value`；**`set_value` 明确不 emit `InputEvent::Change`**（`gpui-base-0.7.1/src/input/base/state.rs:940-968`），所以不会形成“写回 → 解析 → 再写回”的循环。
  - 即便如此，`sync_hex=false` 仍然是必要的：它保证用户正在输入时不会被程序写回打断。
- 不在 input 上做 pattern 限制（例如只允许 hex 字符）：需求没有要求，且限制输入会让“非法输入回退”路径无法被测试/使用；用解析结果决定是否更新颜色即可。

### 7.5 Tab 切换

- 点击 tab Button → `state.set_active_tab(tab, cx)` → `cx.notify()` → Panel 重绘。
- 不改变 `open` 状态。
- Palette/HSLA 的内容是条件渲染；切换时旧的 canvas 被 drop，新的立即测量绘制。

### 7.6 可访问性范围

- Tab Button：`.aria_selected(selected)`。
- Slider track：`Role::Slider` + `aria_label` + numeric min/max/value + horizontal orientation。
- Palette grid：`Role::Group` + `aria_label(labels.palette)`。
- Trigger：`aria_label(hex_string(color).as_str())`。
- 非目标：144 个色块的逐格 a11y（需要 144 个 a11y node，和性能目标冲突）、canvas 上的键盘选色、Escape 关闭。实现时不要为了“看起来完整”塞一堆隐藏按钮，这会直接破坏性能目标。

---

## 8. 实现步骤（按顺序执行）

### Step 1：注册模块

修改 `crates/base_ui/src/lib.rs`，在 `pub mod button;` 后新增：

```rust
pub mod color_picker;
```

验收：`cargo check -p base_ui` 通过（此时还没有新文件，先跳过；Step 2 后再跑）。

### Step 2：骨架与颜色工具

在 `crates/base_ui/src/color_picker.rs` 建立：

1. 模块 doc comment（英文，说明组件职责、性能策略、与 gpui-base/gpui-component 的关系）。
2. imports（显式导入，**禁止** `use gpui_kit::base::*`，避免和 gpui-base 的 `ColorPickerState`/`ColorPicker`/`ColorSwatch` 冲突）。
3. `rgba8`、`hex_string`、`parse_hex`、`palette_index`、`selection_ring_color`。
4. `PALETTE_LIGHTNESS`、`PALETTE_SATURATION`、`PaletteSwatch`、`PALETTE`。
5. `ColorPickerTab`、`ColorPickerEvent`、`ColorPickerLabels` 及 `Default`。

验收：

- `cargo test -p base_ui color_picker` 里率先通过 hex/palette 单元测试（Step 9 写测试）。
- 不引入新依赖。

### Step 3：ColorPickerState 数据与事件

实现：

- `ColorPickerState` 字段：`color`、`active_tab`、`labels`、`hex_input`、`_subscriptions`。
- 私有 `ColorPickerStateSlot { state: Entity<ColorPickerState> }`。
- `use_keyed(id, default_color, window, cx)`：先 `let id = id.into();`，再用 `window.use_keyed_state((id, "color-picker-state-slot"), ...)` 持有 `ColorPickerStateSlot`，内部 `cx.new(|cx| ColorPickerState::new(...))`，返回 `Entity<ColorPickerState>`。
- `new(default_color, window, cx)`：
  - 创建 hex `InputState`（id 用 `("color-picker-hex", cx.entity_id())`）；
  - `cx.subscribe_in(...)`；
  - `_subscriptions: vec![subscription]`；
  - `color` / `labels` 默认。
- `color()`、`active_tab()`、`set_labels()`、`set_color()`、`set_active_tab()`。
- 内部方法（全部写在**同一个 `impl ColorPickerState` 块**里，满足 `multiple_inherent_impl = "forbid"`）：
  - `set_user_color(color, sync_hex, window, cx)`
  - `set_channel(channel, t, window, cx)`
  - `select_palette(index, window, cx)`
  - `write_hex(color, window, cx)`
  - `on_hex_change(text, window, cx)`
  - `on_hex_commit(text, window, cx)`
  - `tab_button(cx, state, tab, active, label) -> Button`（§4.4 的 tab 按钮）
- `impl EventEmitter<ColorPickerEvent>`。
- `impl Render for ColorPickerState`：先返回一个最小 panel 占位（只验证 state/视图生命周期），Step 5 再补全。

验收：

- 状态可创建；
- 程序 `set_color` 不发事件；
- hex input 的 Change 能驱动颜色（用 Step 9 的测试验证）。

### Step 4：ColorWell + PaletteGrid

实现私有 `ColorWell` 与 `PaletteGrid`：

- `ColorWell`：
  - `new(color)`, `.size(Pixels)`, `.radius(Pixels)`；
  - Render：`div().relative().size(...).child(canvas(...).absolute().size_full())`；
  - paint：底、checkerboard（按需）、颜色、1px 边框。
- `PaletteGrid`：
  - `new(state: Entity<ColorPickerState>, color: Hsla, label: SharedString)`；
  - Render：固定 284×284 的 relative div + canvas；
  - `Rc<Cell<Bounds<Pixels>>>` + canvas prepaint 记录 bounds（不用 `on_prepaint`/`ElementExt`）+ `on_mouse_down`；
  - paint：144 个 cell + 选中 ring（用 `palette_index(color)`）；
  - `.debug_selector(|| "TEST_COLOR_PICKER_GRID".into())` 放在容器 div 上（`debug_selector` 的签名是 `FnOnce() -> String`）。
  - **`RenderOnce` 里不要 `state.read(cx)`**：`color` 由父级 `ColorPickerState::render` 作为字段传进来；`state` 只在 `on_mouse_down` 回调里 `update`。这是依赖方向的要求（子元素只依赖传入数据），不是运行期 panic 风险。

验收：单独渲染 PaletteGrid 能看到 12×12 色块；点任意 cell 更新 state。

### Step 5：Panel 布局（tabs + palette + HSLA + hex row）

在 `ColorPickerState::render` 中：

1. 读 theme 的 Copy 值：
   ```rust
   let theme = cx.theme();
   let panel_bg = theme.components.menu.background.hsla;
   let panel_border = theme.roles.line.divider.hsla;
   let panel_shadow = theme.components.menu.shadow.hsla;
   let tab_track = theme.roles.surface.sunken.hsla;
   let tab_selected = theme.roles.surface.overlay.hsla;
   let fg_default = theme.roles.fg.default.hsla;
   let fg_muted = theme.roles.fg.muted.hsla;
   ```
   之后不要再持有 `&Theme`，避免和 `cx.entity()`/`self.hex_input` 的借用冲突。
2. 组装 Panel：外层 `div().p(PANEL_PADDING).bg(...).border_1().border_color(...).shadow(...).debug_selector(|| "TEST_COLOR_PICKER_PANEL".into())`；内层 `div().w(PANEL_CONTENT_WIDTH).flex().flex_col().gap(PANEL_SECTION_GAP)`。不要在外层写死 `w(308)`，让内容宽 284 决定。
3. Tabs（§4.4）。
4. `match self.active_tab`：
   - Palette → `PaletteGrid::new(cx.entity(), self.color, self.labels.palette.clone())`；
   - Hsla → 4 个 `HslaSlider::new(cx.entity(), channel, self.color, label)`。
5. `Separator::horizontal()`。
6. 底部 hex row（§4.7）。

验收：两个 tab 都能完整显示；panel 打开时无 panic。

### Step 6：HslaSlider（canvas 轨道 + 拖动）

实现私有：

- `enum HslaChannel { Hue, Saturation, Lightness, Alpha }`（`Copy/Clone/Eq/PartialEq`，实现 `id_prefix() -> &'static str`、`debug_selector() -> &'static str`、`label(&ColorPickerLabels) -> SharedString`、`value_text(Hsla) -> String`；全部在同一个 `impl HslaChannel` 块）。
- `struct HslaDrag { picker: EntityId, channel: HslaChannel }`（Copy）。
- `struct HslaDragPreview;` + `impl Render` 返回 `Empty`。
- `struct HslaSlider { state: Entity<ColorPickerState>, channel: HslaChannel, color: Hsla, label: SharedString, ... }`。
- Render：
  - Row（§4.6）；
  - Track 容器（id 用 `(channel.id_prefix(), state.entity_id())`、role、aria、debug selector、cursor、`on_mouse_down`、`on_drag`、`on_drag_move`）；
  - canvas 的 `prepaint` 把 canvas bounds 写进 `Rc<Cell<Bounds<Pixels>>>`，供 `on_mouse_down` 使用；`paint` 画 256 条切片 + thumb（§6.2）；
  - thumb 的 `thumb_fill`/`thumb_border` 在 render 里取成 `Hsla` Copy 值，paint 闭包不捕获 `&Theme`。
- 所有 `HslaSlider` 的 inherent impl 放在同一个 `impl` 块。

验收：点击/拖动 4 条轨道都能改变对应分量；颜色变化连续；拖出轨道仍能更新。

### Step 7：ColorPickerTrigger + TriggerSlot + Overlay

实现：

- `struct TriggerSlot { trigger: Entity<ColorPickerTrigger> }`。
- `ColorPickerTrigger`：
  - 字段：`state`、`open`、`size`、`_subscriptions`；
  - `new(state, open, size, cx)`：`cx.observe(&state, ...)`、`cx.observe(&open, ...)`，存订阅；
  - `Render`：`Button::tonal()` + `ColorWell` + `debug_selector` + `aria_label`；`opened` 时 `.selected(true)` 且**不挂**打开用的 `on_click`；关闭时挂 `on_click` 写 `open = true`。
- `ColorPicker`：
  - 字段：`state`、`size`；
  - `new(state)`、`UnifiedSizable`；
  - `RenderOnce`：
    - `picker_id = ("color-picker", state.entity_id()).into()`；
    - `open_state = window.use_keyed_state((picker_id.clone(), "open"), cx, |_, _| false)`；
    - `size_key = match size { Small => "trigger-small", Medium => "trigger-medium", Large => "trigger-large" }`；
    - `slot = window.use_keyed_state((picker_id.clone(), size_key), cx, |_, cx| TriggerSlot { trigger: cx.new(|cx| ColorPickerTrigger::new(...)) })`；
      - 注意：`ElementId` 只支持二元 tuple，不支持 `(id, "trigger-slot", size)` 三元组；`size_key` 直接编码 size，避免嵌套 tuple。
    - `Overlay::new((picker_id, "overlay"), open_state, ...)`；
    - `.placement(Placement::Bottom).align(Align::Start)`。

验收：点击 trigger 打开/关闭 panel；颜色改变时 trigger swatch 更新，且组件不额外 `notify` 父 view（调用方在 `ColorChanged` 里主动 notify 除外；GPUI 的 dirty 传播仍可能让父 render 执行）。

### Step 8：示例

新增 `crates/base_ui/examples/color_picker.rs`：

- `mod support;`
- `support::init::init(cx)`；
- 720×720 窗口；
- view 持有 `accent: Hsla` 和 `state: Option<Entity<ColorPickerState>>`；
- render 里用 `ColorPickerState::use_keyed("color-picker-example", self.accent, window, cx)` 创建/复用 state；
- 订阅 `ColorPickerEvent::ColorChanged` 更新 `accent`（示例用 `.detach()`，生产代码存 `Subscription`）；
- 可选：`state.update(cx, |s, cx| s.set_labels(labels.clone(), cx))` 设置本地化文案；`set_labels` 相等时 no-op；
- 渲染：`ColorPicker::new(&state)` + 一个 160×160 的预览块 + hex 文本。

验收：`cargo run -p base_ui --example color_picker` 手动可见、可交互。

### Step 9：测试

在 `color_picker.rs` 底部写 `#[cfg(test)] mod tests`，至少覆盖 §9 的用例。

### Step 10：格式化与静态检查

```powershell
cargo +nightly fmt
cargo test -p base_ui
cargo +nightly clippy -p base_ui --all-targets -- -D warnings
cargo check -p base_ui --examples
```

全部通过才算完成。

---

## 9. 测试计划

### 9.1 纯函数单元测试

| 测试 | 断言 |
|---|---|
| `parse_hex` 支持所有长度 | `#fff`、`#ffff`、`#ffffff`、`#ffffffff`、无 `#`、大小写混合都能解析；`#f00` 等于 `#ff0000` |
| `parse_hex` 拒绝非法 | `""`、`"#"`、`"#12"`、`"#12345"`、`"#1234567"`、`"#ggg"`、`"#+f0000"`、`"#-fffff"` |
| `hex_string` | 返回 `SmolStr`；整数/大写；α=255 时无 AA；α<255 时有 AA；round 而非 truncate |
| hex round-trip | 对若干颜色 `parse_hex(hex_string(c).as_str())` 的 8-bit 量化结果等于 `rgba8(c)` |
| 调色盘数量/结构 | `PALETTE.len() == 144`；第 0 行 12 个 `s == 0`；第 1..11 行 hue 分别是 `(row-1)/11`；每行 12 个 |
| `palette_index` 命中 | 对每个 swatch，`palette_index(swatch.hsla) == Some(index)` |
| `palette_index` 不命中 | 非调色盘颜色 → None；`a=0.5` 的调色盘颜色 → None（量化 alpha 不是 255） |
| `selection_ring_color` | 亮色（白/浅黄）返回深色；暗色（黑/深蓝）返回浅色 |

### 9.2 状态测试（`#[gpui_kit::test]`）

Harness（这是实际可编译的形状，不要只写字段不写创建）：

```rust
use std::{
    cell::RefCell,
    rc::Rc,
};

use crate::{
    animation::ANIMATION_TICK,
    color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState},
    style::theme,
};
use gpui_kit::{
    Context, Entity, Hsla, IntoElement, Modifiers, MouseButton, ParentElement, Render,
    Subscription, TestAppContext, VisualTestContext, Window, div, hsla, point, px, size,
};

const DEFAULT_COLOR: Hsla = hsla(0.55, 0.8, 0.53, 1.0);

struct Harness {
    state: Entity<ColorPickerState>,
    changes: Rc<RefCell<Vec<Hsla>>>,
    _subscription: Subscription,
}

impl Harness {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // 测试里没有 keyed 父页面，直接 cx.new 即可；use_keyed 的复用行为单独测。
        let state = cx.new(|cx| ColorPickerState::new(DEFAULT_COLOR, window, cx));
        let changes = Rc::new(RefCell::new(Vec::new()));
        let sink = changes.clone();
        let subscription = cx.subscribe(&state, move |_, _, event: &ColorPickerEvent, _| {
            let ColorPickerEvent::ColorChanged(color) = event;
            sink.borrow_mut().push(*color);
        });
        Self {
            state,
            changes,
            _subscription: subscription,
        }
    }
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(ColorPicker::new(&self.state))
    }
}

fn harness(cx: &mut TestAppContext) -> (Entity<Harness>, &mut VisualTestContext) {
    cx.update(theme::install);                 // 必须：render 里读 cx.theme()
    let (view, cx) = cx.add_window_view(|window, cx| Harness::new(window, cx));
    cx.simulate_resize(size(px(900.), px(800.))); // 405px 的 panel 不被裁
    cx.run_until_parked();
    (view, cx)
}

/// 真实的“按下”需要 down → frame → up；否则 trigger 的关闭会被同一帧的 mouse-up 重新打开。
fn press(cx: &mut VisualTestContext, selector: &'static str) {
    let target = cx.debug_bounds(selector).expect("selector exists").center();
    cx.simulate_mouse_down(target, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    cx.simulate_mouse_up(target, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
}

/// Overlay 的关闭有 Presence 淡出；断言 panel 消失前必须推进时钟。
fn settle(cx: &mut VisualTestContext) {
    cx.executor().advance_clock(ANIMATION_TICK * 2);
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
}
```

窗口尺寸设为 `900×800`（`cx.simulate_resize`），保证 405px 的 panel 不会被裁。

| 用例 | 步骤/断言 |
|---|---|
| 初始状态 | `state.color() == DEFAULT_COLOR`；`state.read_with(cx, \|s, cx\| s.hex_input.read(cx).value(cx))` 等于 `hex_string(DEFAULT_COLOR).as_str()`；`active_tab == Palette` |
| 打开 panel | `press(cx, "TEST_COLOR_PICKER_TRIGGER")`；`debug_bounds("TEST_COLOR_PICKER_PANEL").is_some()` |
| 关闭 panel | 打开后，在 `point(px(850.), px(750.))` 做 `simulate_mouse_down(..., MouseButton::Left, Modifiers::none())` + `simulate_mouse_up(...)`，再 `settle(cx)`；`debug_bounds("TEST_COLOR_PICKER_PANEL").is_none()` |
| 再次点击 trigger 关闭 | `press(trigger)` 打开，再 `press(trigger)`，再 `settle(cx)`；panel 消失，且没有被 mouse-up 重新打开 |
| 切 tab | `simulate_click(cx.debug_bounds("TEST_COLOR_PICKER_TAB_HSLA").unwrap().center(), Modifiers::none())`；`GRID` 不存在、`TEST_COLOR_PICKER_HUE_TRACK` 存在；再切回 |
| 点击 palette | 打开 panel，取 `TEST_COLOR_PICKER_GRID` bounds，`simulate_click(point(bounds.origin.x + px(4.*24.+10.), bounds.origin.y + px(2.*24.+10.)), Modifiers::none())`；`state.color() == PALETTE[2*12+4].hsla`；`palette_index(state.color()) == Some(28)`；`changes.len() == 1`；**panel 仍然存在**（`debug_bounds(PANEL).is_some()`） |
| 点击同一格 | 再点一次；`changes.len()` 仍为 1 |
| 点 gap | 点在 `col*24+22`（gap）位置；颜色/事件不变 |
| 点击 hue 轨道 | 打开 HSLA tab，取 `TEST_COLOR_PICKER_HUE_TRACK` bounds，`simulate_click` 在 25% 处；`color.h ≈ 0.25`；`changes.len() == 1` |
| 拖动 saturation | `simulate_mouse_down` 在 20% → `simulate_mouse_move` 到 30%（**必须的第二次 move，用于启动 drag**）→ `simulate_mouse_move` 到 80% → `simulate_mouse_up`；`color.s ≈ 0.8`；事件 ≥ 2 |
| 拖出轨道 | down 在 20% → move 到 30%（启动）→ move 到 track 右边界外 50px → up；`color.s` clamp 到 1.0，不 panic |
| hex 输入合法 | 打开 panel；`clear_and_focus_hex(cx, &state)` 清空并聚焦；`cx.simulate_input("#FF0000")`；`hex_string(state.color()).as_str() == "#FF0000"`，`changes` > 0；input 文本仍是用户输入（`#FF0000`，不被程序重写） |
| hex 输入非法 | 清空后输入 `#zz`；颜色不变、无事件；Enter 后 input 恢复当前颜色的 hex |
| Enter 规范化 | 清空后输入 `#fff`，Enter；颜色为白，input 文本变 `#FFFFFF` |
| 程序 set_color 静默 | 清空 changes，`set_color(blue)`；无事件，input 文本为蓝色 hex |
| 主题切换 | `cx.update(\|_, cx\| theme::apply(Appearance::Dark, cx))` 后 `settle`/重绘；panel/trigger 不 panic；颜色状态不变（`Appearance` 从 `crate::style::palette` 导入） |
| use_keyed 复用 | 一个测试 view 在 render 里用不同 `default` 调 `use_keyed("same")`；两次得到同一 `EntityId`，颜色仍是首次 default（新 default 不覆盖已有 state） |

拖动/点击的模拟调用形状（不要漏掉 `pressed_button` 和第二帧）：

```rust
// 点击：down + up 即可（on_mouse_down 里已更新）
cx.simulate_click(track_point(0.25), Modifiers::none());

// 拖动：down → move(启动) → move(目标) → up
let start = track_point(0.20);
let launch = track_point(0.30);
let target = track_point(0.80);
cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
cx.simulate_mouse_move(launch, Some(MouseButton::Left), Modifiers::none());
cx.simulate_mouse_move(target, Some(MouseButton::Left), Modifiers::none());
cx.simulate_mouse_up(target, MouseButton::Left, Modifiers::none());
cx.run_until_parked();

/// 清空 hex input 并聚焦；不要直接把 `window` 写进表格里的伪代码。
fn clear_and_focus_hex(cx: &mut VisualTestContext, state: &Entity<ColorPickerState>) {
    cx.update(|window, cx| {
        state.update(cx, |state, cx| {
            state.hex_input.update(cx, |input, cx| {
                input.update_value("", window, cx);
            });
        });
        let focus = state.read(cx).hex_input.read(cx).engine_focus_handle(cx);
        focus.focus(window, cx);
        window.draw(cx).clear(cx);
    });
    cx.run_until_parked();
}
```

- `hex_input` 是私有字段；这段 helper 放在 `color_picker.rs` 的 `#[cfg(test)] mod tests` 里（同模块可访问私有字段），不要放到 example 或外部集成测试。
- 主题切换的 `Appearance` 从 `crate::style::palette::Appearance` 导入。

### 9.3 手动验收（example）

- 打开 `examples/color_picker.rs`；
- 切换 tab 两次；
- 在 palette 点几个格子，确认 ring 跟随；
- 拖动 4 条滑杆到两端，确认颜色/数值/thumb 同步；
- 在 hex 输入 `#C084FB`、非法串，确认回退；
- 观测：拖动时 panel/trigger 更新；不要承诺“父页面完全不 render”（GPUI 的 dirty 传播仍可能让父 render 执行），只确认没有 144 色块重新生成、没有每格元素创建、没有颜色转换热点。

### 9.4 命令

```powershell
cargo test -p base_ui
cargo +nightly clippy -p base_ui --all-targets -- -D warnings
cargo +nightly fmt --check
cargo check -p base_ui --examples
```

---

## 10. 边界、非目标与风险

| 项 | 处理 |
|---|---|
| 面板高度 405px | 主窗口最小 1280×960，不会超出；不做滚动 |
| 无键盘选色/无逐格 a11y | 明确非目标；canvas 内部没有独立可聚焦节点。容器提供 Group/Slider 语义 |
| 无 touch 拖动 | 桌面鼠标优先；`on_drag` 是 mouse-only。若未来需要触屏，沿用 gpui-base slider 的 `TouchDragEvent` + `window.on_mouse_event` 模式另做 |
| Escape 不关闭 | 与现有 Dropdown 一致；后续可加 key context |
| wgpu gradient 色差 | 本设计不使用 gradient，规避 |
| `Hsla` 精度 vs hex 8-bit | 内部保留 f32；只有显示/输入是 8-bit。palette 命中用 8-bit 是为了和 hex 输入一致 |
| 主题切换 | 每帧从 `cx.theme()` 取色，不缓存 `ResolvedColor` |
| 多个 picker 共享 state | 不支持；一个 state 对应一个 `ColorPicker` |
| 面板宽度约 310 与 trigger 宽度差异 | `Align::Start` + Positioner clamp/flip；不在组件内做动态宽度 |
| 将 gpui-base 的 `ColorPickerState` 误导入 | 本文件禁止 `use gpui_kit::base::*`；用显式导入。消费方若同时 glob 导入两边，需要显式指定 `base_ui::color_picker::ColorPickerState` |

---

## 11. 关键风险的自查清单（实现前逐条确认）

- [ ] `gpui-pre` 解析版本是 **0.3.8**，不是 0.3.9。
- [ ] 本文件没有 `use gpui_kit::base::*;`，没有 `ColorSwatch` 命名冲突。
- [ ] 没有 `pub use`，没有 `#[allow]`/`#[expect]`，没有 `#[path]`。
- [ ] `ColorPickerState` 的所有 inherent impl 在同一个 `impl` 块。
- [ ] `ColorPickerState` 是通过 `ColorPickerState::use_keyed(...)` 创建的；`new` 只由 `use_keyed` 内部或测试里的 `cx.new` 调用，没有在 view `new()` 里直接 `cx.new`。
- [ ] `_subscriptions` 字段真的被保存，而不是 `.detach()` 后丢掉。
- [ ] `ColorPickerState::use_keyed` 使用不 notify 的 `ColorPickerStateSlot`；没有把 `ColorPickerState` 本身直接放进 `window.use_keyed_state`。
- [ ] `TriggerSlot` 自身不 notify；trigger 不直接放在 `use_keyed_state` 里。
- [ ] `ColorPickerState::render` 里 `cx.theme()` 的借用不跨越 `cx.entity()`/`self.hex_input` 的可变借用。
- [ ] `canvas` 的 paint 闭包只捕获 `'static` 数据和 Copy 值，不捕获 `&Theme`/`&self`。
- [ ] `paint_quad` 的每个 quad 都有有限的 bounds；track 宽度为 0 时提前 return。
- [ ] slider 的鼠标坐标用 canvas `prepaint` 记录的 bounds（不是 `Div::on_prepaint`，GPUI 0.3.8 没有这个方法）；拖动用 `DragMoveEvent.bounds`。
- [ ] hex 回写用 `update_value`（不触发 Change），且用户输入时 `sync_hex=false`。
- [ ] palette 的 144 个格子没有 Element/Entity/订阅。
- [ ] 主题色每帧读取，不缓存。
- [ ] 所有 hex 显示大写、round 而非 truncate。
- [ ] 测试窗口 ≥ 900×800，避免 panel 被 viewport 裁掉。
- [ ] 所有 `ElementId` 都是 `(&'static str, EntityId)` / `(ElementId, &'static str)` 等真实支持的形状；没有 `(EntityId, _)`。
- [ ] 所有六位 hex 显示都用 `hex_string(...).as_str()`，函数返回 `SmolStr`；没有无谓的 `String` 中转。
- [ ] 12×12 与参考图 9 行的差异已按 §5.4 记录；默认采用 1 中性 + 11 hue，且没有声称“完全复刻参考图”。
- [ ] 拖动测试至少两次 `simulate_mouse_move`（第一次启动 drag，第二次才产生 `DragMoveEvent`）。
- [ ] 没有承诺“父 view 完全不 render”；`use_keyed`/TriggerSlot 只负责省掉 keyed observer 的额外 notify，测试里没有无效的 `renders` counter 断言。
- [ ] §2.4 的常量都被布局/绘制实际引用，未使用的已删除（避免 `dead_code` 在 `-D warnings` 下报错）。
- [ ] `cargo test`、`clippy -D warnings`、`fmt` 全绿。

---

## 12. 对抗性审查记录

> 每轮记录：审查者、发现、修复、复审结论。审查只读文档/源码，不修改文件。

### Round 1（A 角：API 可编译性）

发现并修复：

1. **[BLOCKER] `(EntityId, _)` 不能转 `ElementId`**：`(state.entity_id(), "palette-grid")` / `(state.entity_id(), channel as usize)` 编译失败。→ 全部改为 `(&'static str, EntityId)`：`("color-picker-palette", state.entity_id())`、`(channel.id_prefix(), state.entity_id())`；文档新增说明：“ElementId tuple 的元素类型也必须精确匹配”。
2. **[MAJOR] `Div::on_prepaint` 不存在**：它是 gpui-base 的 `ElementExt::on_prepaint`（注入一个 canvas），显式 import 列表里也没有它。→ 设计改为复用 palette/track 已有 canvas 的 prepaint 回调记录 `Rc<Cell<Bounds<Pixels>>>`；删除对 `on_prepaint`/`ElementExt` 的依赖。
3. **[MAJOR] 拖动测试只 move 一次不会产生 `DragMoveEvent`**：`DRAG_THRESHOLD = 2.0`，第一次 move 在 Capture 阶段 `active_drag` 仍为 None，Bubble 阶段才启动 drag；第二次 move 才更新。→ 测试计划改为 down → move(启动) → move(目标) → up。
4. **[MINOR] `Background::LinearGradient` 符号不存在**：它是私有的 `BackgroundTag` 变体；`colors: [LinearColorStop; 2]` 是私有字段。→ 措辞改为“`Background` 内部私有的 `colors` 只有 2 个 stop”。
5. **[MINOR] `theme.components.input.background.hsla` 不成立**：`background/foreground/border` 是嵌套结构。→ 改为 `.background.{enabled,disabled}` / `.foreground.{enabled,disabled}` / `.border.{normal,focused,disabled}`。
6. **[MINOR] `debug_selector` 的 selector 名/类型**：`format!` 产生 `TEST_COLOR_PICKER_Hue_TRACK`，与测试的 `..._HUE_TRACK` 对不上。→ 改为 `HslaChannel::debug_selector()` 返回 4 个固定字符串，闭包 `.into()`。
7. **[MINOR] `state.update(|...|)` 少第一个 `cx`**：→ 改为 `state.update(cx, |state, cx| ...)`。
8. **[MINOR] §6.4 展示了第二个 inherent `impl`**：→ 改成“函数体放进唯一 impl 块”的片段。
9. **[MINOR] `observe_keyed_state` 行号过期**：`7339-7344` → `7201-7206`。
10. **[MINOR] `hex_string` 返回 `String` 不符合 AGENTS §2.2**：→ 返回 `SmolStr`，使用 `format_smolstr!`，调用方 `.as_str()`。

A 角判定：FAIL（1 BLOCKER + 2 MAJOR）。

### Round 1（B 角：需求与逻辑）

发现并修复：

1. **[BLOCKER] 12×12 与参考图 9 行的冲突未处理**：→ §5.4 新增“与参考图的差异（刻意）”：默认 12 行 = 1 中性 + 11 hue，明确不是逐格复刻；给出 9 行替代方案的常量改法与代价；删除“与参考图一致”的旧表述。
2. **[BLOCKER] §11 自查清单仍推荐 `new` + `use_keyed_state`**：与 §6.4 的性能坑冲突。→ 清单改为必须走 `ColorPickerState::use_keyed` / `ColorPickerStateSlot`，禁止把 state 直接放进 `use_keyed_state`。
3. **[MAJOR] `PaletteGrid` 缺 `color`/`label` 字段**：→ `PaletteGrid { state, color: Hsla, label: SharedString }`，构造签名同步。
4. **[MAJOR] 测试 selector 对不上**：→ 已由 A 角第 6 条一起修复为 4 个固定字符串。
5. **[MAJOR] Harness 不完整**：→ §9.2 给出完整 Harness（install 主题、`ColorPickerState::new`、`_subscription` 填充 `changes`、`press`/`settle` helper）。
6. **[MAJOR] hex 测试不可执行**：→ 默认值改为 `hex_string(DEFAULT_COLOR).as_str()` + `value(cx)`；输入前先 `update_value("")` 清空；focus 后再 `simulate_input`。
7. **[MAJOR] 关闭测试忽略 Presence 淡出**：→ 增加 `settle(cx)`（`advance_clock(ANIMATION_TICK * 2)` + `window.refresh()` + `run_until_parked`）；打开/关闭用 `press` helper（down → frame → up → refresh）。
8. **[MAJOR] 缺少关键回归**：→ 补“palette 点击后面板仍开 + `palette_index` 命中”、“拖出轨道仍更新”、“use_keyed 复用/新 default 不覆盖”。（Round 2 已删除无效的 render-counter 断言，见下。）
9. **[MAJOR] Lightness 轨道固定 `s = 1.0` 与 thumb 不一致**：→ 改为 `hsla(base.h, base.s, t, 1.0)`；只有 Hue 用固定 s=1/l=0.5。
10. **[MAJOR] thumb 在 paint 闭包里用 `theme`（`'static` 不成立）**：→ render 里先取 `thumb_fill`/`thumb_border` 两个 `Hsla` Copy 值，paint 只用 Capture 值。
11. **[MINOR] 面板 1px border 未计入**：→ 明确 content 284、外框约 310；高度 403/245（+border 约 405/247）。
12. **[MINOR] tabs 下多余 Separator**：→ 删除；只保留内容与 hex row 之间的一条。
13. **[MINOR] Alpha 轨道无 checkerboard 的意图未写明**：→ §4.6 明确“不画 checkerboard”是刻意选择，棋盘格只在 `ColorWell`。
14. **[MINOR] 圆角帽 5–6 级阶梯**：→ `TRACK_SLICES` 从 128 提到 256，并把“肉眼看不出台阶”改为“1px 级近似，可接受；要绝对平滑用端帽渐变”。
15. **[MINOR] 命中测试硬编码 284/24**：→ 用 `bounds.size.width/height` 推导 `step_x/step_y/cell_x/cell_y`。
16. **[MINOR] Join 与参考图独立 pill 的差异未说明**：→ §4.4 写明 R2 强制 Join 带来的直角共享边/20px 选中块是接受的视觉差异。

B 角判定：FAIL（2 BLOCKER + 多项 MAJOR）。

### Round 2（修复后复审）

**A 角（API 可编译性）：PASS**（0 BLOCKER / 0 MAJOR）。核对通过：ElementId 全部为 `(&'static str, EntityId)`；canvas prepaint/paint 签名；`use_keyed` 编译链；Harness 的 `theme::install`/`add_window_view`/`simulate_*`/`executor().advance_clock`；`format_smolstr!`；`hex_string` 调用点；`state.update(cx, ...)`；`set_labels` 签名。剩余 MINOR 三条（`debug_selector` 调用形式、§6.2 Lightness 摘要残留、§9.2 表格缺 `window`/`Appearance`）已在本轮修掉。

**B 角（需求与逻辑）：FAIL（0 BLOCKER / 4 MAJOR / 4 MINOR）**，修复如下：

1. **[MAJOR] §6.1 元素树仍有两条 Separator**（与 §4.3 单条冲突）→ 两条树都改为“tabs → content → Separator → HexRow”，与 403/405 高度一致。
2. **[MAJOR] §6.2 摘要仍写“Lightness 固定 s=1”**（与算法 `base.s` 矛盾）→ 改为“Lightness 用当前 s”；thumb 固定白块的说明同步修正。
3. **[MAJOR] §4.5 命中测试用 `width/12≈23.67` 当 step，绘制 pitch 是 24，最后一列右侧约 4px 落不进 cell** → 改为 `scale = bounds.width / PALETTE_SIZE`，`step = PALETTE_STEP * scale`，`cell = PALETTE_CELL * scale`；12 cell + 11 gap 的几何精确匹配。
4. **[MAJOR] §9.2 hex 用例的 `window` 未绑定、`Appearance` 未导入** → 增加 `clear_and_focus_hex(cx, &state)` helper（`cx.update(|window, cx| ...)` 内清空 + focus + draw）；主题切换写成 `cx.update(|_, cx| theme::apply(Appearance::Dark, cx))`，并注明 `Appearance` 从 `crate::style::palette` 导入。
5. **[MINOR] thumb 颜色理由与固定白块实现不符** → 改为“轨道自身同族；thumb 是固定 `surface.overlay` 白块，不参与颜色插值”。
6. **[MINOR] `TRACK_SLICES` 与局部 `SLICES` 重复；`PALETTE_SIZE` 未使用** → 统一用 `TRACK_SLICES`；`PALETTE_SIZE` 改为由 `PALETTE_CELL/GAP/COLS` 计算，并用于 grid 宽高、命中测试。
7. **[MINOR] `Entity<input::InputState>` 路径与 import 不符** → 改为 `Entity<InputState>`（crate::input 已导入）。
8. **[MINOR] `tab_button`/`channel.id_prefix()/debug_selector()` 未进 Step 3/6 清单** → 已加入 §2.4 的私有方法清单；Step 3/6 的说明同步引用。

**Round 2 额外（快速 Reviewer）**：**[MAJOR] “父 view 不重绘”不成立**：`mark_view_dirty` 会标脏祖先，非 `.cached()` 的 `ViewElement::request_layout` 会 render，root 每帧重跑 layout；`renders` 计数断言会失败。→ 已删除“父页面完全不 render”的承诺和 `renders` counter 测试，改为“省掉 keyed observer 的额外 notify；父 render 是否跑由 GPUI dirty 传播决定”；§0/§2.2/§3.1/§6.4/§9.2/§9.3 同步改写。
**[MAJOR] `Point + Point` 不成立**（`origin + point(...)` / `bounds.origin + point(...)`）→ 改为 `point(origin.x + px(1.), origin.y + px(1.))` / `point(bounds.origin.x + ..., bounds.origin.y + ...)`。
**[MINOR] 子元素 `state.read(cx)` 的 panic 理由不成立** → 改为“依赖方向/职责清晰”，删除 panic 说法。
**[MINOR] 未使用常量 dead_code** → §2.4 增加“必须实际引用，否则删除”的说明，并把 SLIDER/TRACK/THUMB 常量写进布局/绘制片段。

### Round 2 修复后复审（最终）

- **A 角：PASS**（0 BLOCKER / 0 MAJOR；3 条 MINOR 已修：`debug_selector` 调用形式、§6.2 Lightness 摘要、§9.2 hex `window`/`Appearance`）。
- **快速 Reviewer：PASS**（0 BLOCKER / 0 MAJOR；父 render 承诺、Point+Point、panic 理由、`transparent` 占位符、常量 dead_code 均已修）。
- **B 角：PASS**（0 BLOCKER / 0 MAJOR；Round 2 的 4 个 MAJOR 逐条确认已解决：§6.1 单 Separator、§6.2 Lightness 摘要、§4.5 命中测试 scale、§9.2 hex helper/Appearance）。
- 最终结论：**0 BLOCKER / 0 MAJOR，文档可以作为实现依据**。
- 唯一需要产品确认的取舍：§5.4 的“12×12（默认 1 中性 + 11 hue）vs 参考图 9 行”。默认按用户明确的 12×12 实现；若要求严格复刻参考图，按 §5.4 的替代常量修改（会不满足 12×12）。

### Round 3（无工具一致性审）

审查者只基于设计片段审需求/一致性，提出 2 个 MAJOR；均已处理：

1. **[MAJOR] 事件契约不自洽**：`set_color` 静默、palette/slider/hex 的 emit 路径未集中定义，hex 程序回写是否回环未说明。→ 新增 **§3.2** 表：`set_color` 静默；palette/`set_channel`/合法 hex Change/Enter 走 `set_user_color` emit（仅变化时）；hex Change 不回写；Enter 无条件规范化；`set_value` 不触发 `InputEvent::Change`；`sync_hex` 防回环。§5.5/§7.4/§9.2 同步。
2. **[MAJOR] footer“色块”漏项**：审查者基于摘要认为 footer 只有 Input；实际 §4.7 一直有 `ColorWell`，但表述不够突出。→ §4.7 明确“第一个 child 是独立 30×30 `ColorWell`（需求 3 的当前色块），第二个是 Input”，加 `TEST_COLOR_PICKER_HEX_INPUT` selector，并说明它不是 trigger 的 ColorWell。
3. 附带澄清：open 由 `open_state` keyed state 驱动、toggle 时 notify 父 view、Overlay 由父 render 挂载（不是 trigger 条件挂载）；§4.6 行宽 `72+8+156+8+40=284`（gap 8+8，不是 12）；§4.5 的 284 不是 12 等分（cell 20/gap 4/pitch 24），命中测试按 `PALETTE_SIZE` 缩放；§9.2 hex 用例断言 `hex_string(state.color())`。
4. 其余（Alpha 无 checkerboard、无 HEX_INPUT selector）已在 §4.6/§4.7 明确；Alpha 与图 1 的白→色一致，ColorWell 才画棋盘格。
5. 该审查者的 MAJOR B 是摘要省略造成的误报；§4.7 的代码块本身一直包含 `ColorWell`，本轮只是把文字写死、避免歧义。
6. **[MAJOR] 12×24=288 vs 284 的误算**：第 0..11 列只有 11 个 gap，最后一格右缘是 `11*24+20=284`；`12*24=288` 多算了一个 trailing gap。→ §4.5 增加“12 列几何自检”段落，命中测试 `floor(x/24)` 的 col 最大 11，明确不会产生 288。

**最终状态：A / 快速 / B / 无工具 四个审查均 PASS；无工具审查最后的 12×24=288 是几何误算（11 个 gap，最后一列 264..284），已在 §4.5 写死澄清并获其确认。0 BLOCKER / 0 MAJOR。**
