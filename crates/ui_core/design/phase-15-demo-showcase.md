# P15：demo 展示扩展（焦点 / CJK / 输入法 / 无障碍）

> **窗口/布局更新（当前实现）**：窗口改为**页面级 flex**（根列 → 标题列 / 内容列 → 三行），
> 背景节点绝对定位铺满整个窗口；窗口尺寸 = 内容 `960×740` + 平台顶部安全区
>（macOS 38、Linux 自绘标题栏 36、Windows 0），见 `window_size()`。卡片默认矩形仍与 §1 的
> 相对内容一致，但整体下移平台安全区；卡片内部继续用相对定位，`Canvas` 的绝对坐标由
> `RES_CARD.origin` 平移。Linux 的关闭/最小化/最大化按钮由 demo 自绘（`scene/chrome.rs`），
> Windows/macOS 使用原生标题栏。下文的“绝对定位卡片网格”是 P15 当时的历史记录。


> **P18 迁移**：demo 已并入 `..`，不再是独立 crate；当前运行/测试
> 命令见 `design/phase-18-demo-example.md`。本文保留 P15 的设计与实测记录，路径已同步为 example。
>
> 目标：把 demo 从一个「三色块 + 一行文本」的冒烟示例，扩成能**肉眼验证**引擎
> 四类能力的展示场景：焦点与键盘导航、CJK 排版、输入法（IME）组合与提交、无障碍（AccessKit）语义。
> 本文是本次改动的设计基线；实现与本文不一致处记入 `design/DEVIATIONS.md`。

---

## 0. 结论摘要

| 项     | 决定                                                                                                                                                                                         |
|--------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 范围   | `..` + 文档；引擎只做**被实测暴露的三处既有缺口**修复（F-151 勾选态通道 / F-152 回投任务唤醒 / F-153 文本变化重发 a11y）                                                                     |
| 窗口   | 内容逻辑 `960×740`（另加平台顶部安全区），`Scale::ONE`；DPI 由平台覆盖（本机 1.25 ⇒ 物理 1200×925）                                                                                                                    |
| 布局   | 页面级 flex：根列 → 标题列 / 内容列（2 列 × 3 行）；背景绝对定位铺满窗口                                                                                                                  |
| 焦点   | 7 个可聚焦节点：3 色块 → 单选 A → 单选 B → 缩放滑块 → 输入框；Tab / Shift+Tab 循环，焦点环 + 状态文字                                                                                        |
| CJK    | 4 条单行脚本样例（简 / 繁 / 日 / 韩）+ 1 段中英混排自动换行段落；断言无 `MissingGlyph`                                                                                                       |
| IME    | 自建 `ImeField` 单行模型（`Start/Preedit/Commit/End` + `Text` + Backspace + Enter），事件回显 + 内联组合串（Windows MSPY 组合期不给组合串，见 V-104；内联串由 `snapshot_demo` 种子状态演示） |
| 无障碍 | 色块 = `RadioButton`（`checked`）、单选 = `RadioButton`（`checked`）、滑块 = `Slider`（`value` + `SetValue`）、输入框 = `TextInput`（`value` + `SetValue`）；全部带可访问名                  |
| 测试   | 单元测试（IME 模型 / 焦点视图 / 滑块状态）+ 集成测试（建树、Tab 序、a11y 快照、CJK 无豆腐、选中态、方向键→a11y value）                                                                       |

### 0.1 `checked`（Toggled）的由来

最初的设计想用 `A11yPatch.selected` 表达单选/勾选的选中态，理由是「P11 只有 `selected` / `expanded`」。
UIA 客户端 dump 推翻了这个假设：`accesskit_windows` 对 `Role::RadioButton` 的 `SelectionItemPattern`
**只看 `toggled()`**（`accesskit_windows-0.35.2/src/node.rs:648-680`），只写 `selected` 的 RadioButton 在
UIA 里没有任何选中状态。

因此本阶段补上引擎的 `checked: Option<bool>` 通道（`A11yPatch` / `A11yNode` →
`set_toggled(Toggled::from(v))`；与 `selected` / `expanded` 同型，静态 `A11yOverride` 不承载），
demo 的色块与单选都改用它；`selected` 保留给
`ListBoxOption` / `Tab` / `TreeItem` 等真正按 `aria-selected` 暴露的角色。细节与回归见
`design/DEVIATIONS.md` F-151。

---

## 1. 场景布局

内容区 `960×740`（窗口高度再加平台顶部安全区）；外框 `#0B0E13`，卡片 `#141A22` + 1px `#222C38` 边框 + 14px 圆角。

| 卡片         | 逻辑矩形 `(x,y,w,h)` | 内容                                                                                                                                                 |
|--------------|----------------------|------------------------------------------------------------------------------------------------------------------------------------------------------|
| 标题         | `24,14,800,34`       | `Kytrix GUI · 焦点 / 中文 / 输入法 / 无障碍`（20px，700）                                                                                            |
| 副标题       | `24,50,912,20`       | `Tab / Shift+Tab 切换焦点 · Enter / Space 激活 · 点击色块 · 在输入框中输入中文 · F12 调试 HUD · Esc 退出`（12.5px；F12 段只在 `hud` feature 下出现） |
| ① 焦点与动作 | `24,80,580,176`      | 3 个色块 `150×88`（y=42，x=18/202/386）+ 选中说明（y=142）                                                                                           |
| ② 焦点状态   | `624,80,312,176`     | 当前焦点名、Tab 序 `i / 7`、操作提示（5 行）                                                                                                         |
| ③ 中文排版   | `24,270,580,200`     | 4 条 13.5px 脚本样例（y=40/66/92/118）+ 混排段落 `544×44`（y=142，13px，自动换行）                                                                   |
| ④ 无障碍     | `624,270,312,200`    | 单选 A（y=40）、单选 B（y=70）、滑块（y=98，42 高）+ 状态/提示文字                                                                                   |
| ⑤ 输入法     | `24,484,580,242`     | 输入框 `544×54`（y=40）、组合串行、最近 IME 事件、最近提交、2 行提示                                                                                 |
| ⑥ 图片与滚动 | `624,484,312,242`    | 2 个 16×16 PNG（1:1 / 2×）标签、20 行滚动列表 `276×126`（y=82）、提示                                                                                |

### 1.1 焦点序

DFS 先序 = 挂载顺序，因此**挂载顺序即 Tab 序**：

| #   | 节点             | a11y 角色     | 可访问名            |
|-----|------------------|---------------|---------------------|
| 1–3 | 色块（红/绿/蓝） | `RadioButton` | `色块 1（红）` …    |
| 4–5 | 单选 A / B       | `RadioButton` | `选项 A` / `选项 B` |
| 6   | 缩放滑块         | `Slider`      | `缩放比例`          |
| 7   | 输入框           | `TextInput`   | `输入法测试输入框`  |

挂载顺序因此是：背景 → 标题 → 焦点卡片（含色块）→ 无障碍卡片 → 输入法卡片 → 图片/滚动卡片 →
中文卡片 → 焦点状态卡片（后两者的文本没有可聚焦节点）。

---

## 2. 交互行为

### 2.1 焦点

- 启动时焦点给色块 1（焦点环可见）。
- `Tab` / `Shift+Tab` 走引擎默认绑定（`Action::{FocusNext, FocusPrev}`，末尾回绕）。
- 焦点状态文字由 `App::focused` 轮询 → `Signal<FocusView>` → `bind_text`；`FocusView` 记录
  `index`（`None` = 无焦点）、`total`、`label`。
- 色块 / 单选 / 滑块 / 输入框都带 `FocusRing::new(2.0, 2.0, #4C9AFF)`；鼠标点击容器内的文字子节点时，
  引擎按「命中节点 → 最近可聚焦祖先」落焦点。

### 2.2 色块与单选

- 点击色块：动作 `swatch.select` → 写 `active: Signal<u8>`（`u8::MAX` = 未选）+ 写剪贴板十六进制值；
  样式补丁换高亮色；`bind_a11y.checked` 同步（`RadioButton` 的选中态通道）。
- 点击单选 A/B：动作 `a11y.radio.a` / `a11y.radio.b` → 写 `A11yState.radio`；圆点可见性走
  `StylePatch.visible`（不重建节点、不改布局）。

### 2.3 缩放滑块

- 10 段方块（`24×16`，间距 3）表示 10%–100%；点亮段数 = `scale`。
- 点击滑块：动作 `a11y.slider.next` → `scale = scale % 10 + 1`（循环）。
- 获得焦点时 `←` / `→` 调整（`tick` 里读 `Event::Input(Key)`；引擎不内置箭头键语义）。
- AT 的 `SetValue("70")` 由 `App::on_a11y_action(A11yAction::SetValue)` 落到同一状态；同一个 handler 按
  `ctx.node()` 区分滑块与输入框（输入框的 `SetValue` 是替换整段文本）。

### 2.4 输入法

`ImeField`（纯值模型，`Signal<ImeField>`）：

| 输入                   | 行为                                                                                |
|------------------------|-------------------------------------------------------------------------------------|
| `ImeEvent::Start`      | `composing = true`；`last_event = "Start"`（系统可能不给组合串）                    |
| `ImeEvent::Preedit(s)` | `composing = true`；`preedit = s`；`last_event = "Preedit(\"s\")"`                  |
| `ImeEvent::Commit(s)`  | `composing = false`；`committed += s`；清 `preedit`；`last_event = "Commit(\"s\")"` |
| `ImeEvent::End`        | `composing = false`；清 `preedit`；`last_event = "End"`                             |
| `InputEvent::Text(s)`  | `committed += s`；`last_event = "Text(\"s\")"`                                      |
| `Backspace`            | 组合中（`composing`）⇒ 忽略（交给系统 IME）；否则删 `committed` 的最后一个 `char`   |
| `Enter`                | 组合中 ⇒ 忽略；否则把 `committed` 记进 `submitted` 并清空 `committed`               |
| AT `SetValue(s)`       | `committed = s`；清 `preedit`；`composing = false`（无选区模型，等价全选替换）      |

- 「是否在组合」由 **`Start` / `Preedit` / `Commit` / `End`** 驱动（`ImeField.composing`），**不依赖**
  `preedit` 是否为空：Windows 11 Microsoft Pinyin 组合期间连 `Start` 都不发；Linux / macOS 平台层从不发
  `Start`，靠 `Preedit` 进入组合态（`DEVIATIONS.md` V-104）。
- 键盘 / IME / `InputEvent::Text` 只在该输入框持有焦点时接受（`App::focused(window) == Some(field)`）；
  AT 的 `SetValue` 面向目标节点，不受焦点限制。
- 焦点离开输入框（含窗口失焦的 `Event::FocusChanged { focused: false }`）⇒ 收起组合态（否则
  `Esc` 会被一直误判成「组合中」）。
- 输入框显示 `committed + preedit + 焦点时的 "|"`；下方三行分别显示 `组合中：…`、`最近 IME 事件：…`、
  `最近提交：…`。`Start` 但无组合串时显示「组合中：（输入法处理中）」。
- `Esc`：`composing` 为真时不关窗（交给 IME 取消组合）；否则关窗退出。**平台边界**：本机 Windows 11
  MSPY 组合期间不发任何事件（Start/Commit/End 都在提交时到达，见 `DEVIATIONS.md` V-104），因此真实
  Windows 上应用无法感知组合中，**若 Esc 被 IME 放行到应用**就会走关窗路径；「组合中不关窗」只对按时
  发 `Start` 或 `Preedit` 的平台/IME 生效（Linux / macOS 经 `Preedit` 进入组合，从不发 `Start`）。
- 组合窗位置：P15 当时 Win32 按**最后指针位置**（`ImmSetCompositionWindow(CFS_POINT)`）；macOS / Linux
  用固定的窗口原点 1×1 矩形（`firstRectForCharacterRange` / `set_cursor_rectangle(0,0,1,1)`）。P16
  （`design/phase-16-ime-caret.md` / `DEVIATIONS.md` F-154）已改为 `App::set_ime_cursor_area` +
  `Window::text_caret_rect` 的精确 caret hint，关闭 V-106。

### 2.5 无障碍

- 每个交互节点都有静态 `A11yOverride.label`，因此 `App::a11y_diagnostics()` 为空
  （`MissingLabel` / `MissingAltText` 都不出现）。
- 两个 `Canvas` 图标带替代文本；否则引擎会标 `hidden` 并记 `MissingAltText`。
- `bind_a11y` 每帧给 `checked`（色块 / 单选，走 `set_toggled`）与 `value`（滑块 / 输入框）。
- `TextInput` 的 `SetValue` / `Slider` 的 `SetValue` 都由应用 handler 落地；`Click` 走 P10 的 `Action` 表
  （UIA 的 `SelectionItemPattern.Select()` 也是 `Action::Click`，因此单选切换只需要一条 handler）。
- 被装饰性文字子节点（单选/滑块的标签、输入框正文）标 `A11yOverride.hidden`，避免 AT 重复朗读。

---

## 3. 模块结构

```text
crates/ui_core/examples/demo/
├── main.rs           # example "demo"：真实窗口
├── snapshot.rs       # example "snapshot_demo"：--snapshot [path]
├── scene.rs          # 模块根：常量、`Scene`、build_into / install / preview / tick
└── scene/
    ├── card.rs       # 卡片/文字/绝对定位样式助手
    ├── swatch.rs     # ① 3 个色块
    ├── focus.rs      # ② 焦点视图 + 状态文字
    ├── cjk.rs        # ③ CJK 样例
    ├── a11y.rs       # ④ 单选 + 滑块（含 SetValue handler）
    ├── ime.rs        # ⑤ ImeField 模型 + 输入框
    ├── resources.rs  # ⑥ PNG 图标 + 滚动列表
    └── test_support.rs # 场景测试搭建（仅 test + test-support）
```

`Scene` 只暴露 `open` / `build_into` / `install` / `preview`；信号与节点句柄是私有字段（子模块可读）。
两个 example 源文件通过 `pub mod scene;` 共用同一份场景模块（P18）。

---

## 4. 测试计划

| 层       | 位置                   | 断言                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
|----------|------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 单元     | `scene/ime.rs`         | 组合 → 提交的完整状态机；Backspace 的 char 边界；Enter 提交；组合中忽略 Backspace/Enter                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| 单元     | `scene/focus.rs`       | `FocusView::new` 的命中/未命中/无焦点三种形态；`line()` / `position()` 文案                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| 单元     | `scene/a11y.rs`        | 滑块百分比文案、`SetValue` 解析（钳制 1..=10）、单选切换、截图种子状态                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| 单元     | `scene/swatch.rs`      | 选中说明文案与 `LABELS` / `HEX` 对齐                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| 单元     | `scene/cjk.rs`         | 四条样例覆盖简/繁/日/韩且都含 CJK；混排段落同时含 ASCII 与 CJK                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| 场景测试 | `examples/demo/scene*` | 首帧提交 + 图元数 + `Canvas` 绝对坐标；CJK 无 `MissingGlyph`；Tab 7 个不同节点后回绕；点击色块后 `checked == Some(true)`；a11y 快照含 `TextInput` / `Slider` / `RadioButton`、单选 `checked` 正确、诊断为空；`←/→` 改滑块后 a11y `value` 更新且**等宽状态行**也重发；IME 事件只在输入框聚焦时生效；`Start`→Esc 不关窗（覆盖按时发 `Start` 或 `Preedit` 的平台；Linux/macOS 经 `Preedit` 进入组合，本机 MSPY 组合期不发，见 V-104）、失焦收组合后 Esc 可关窗（节点失焦与窗口失焦各一例）；截图种子状态（色块 2 / 单选 B / 滑块 70% / 焦点在输入框） |

`snapshot_demo` 路径额外调用 `Scene::preview`：把 `active=1`、`radio=B`、`scale=70%`、输入框种子为
`committed="你好，Kytrix"` + `preedit="shi li"`，并把焦点给输入框，保证离屏截图能同时看到四类能力。
`preview` 之前先跑一帧：输入框靠 `bind_style` 的 `interactive: Some(true)` 才能被 `set_focus` 接受。

---

## 5. 非目标

- 不做文本选区 / 光标移动 / 多行编辑（引擎 v1 无编辑模型）。
- 不做 caret 定位的 IME 候选窗（P15 当时平台层按指针位置，见 §2.4；P16 已补精确 caret hint，非本阶段范围）。
- 不做真实 macOS/Linux 窗口验收（沿用 P12 的静态检查结论）。
- 不为 Windows 补 TSF 文档管理器（内联组合串的平台限制见 §6.3 / `DEVIATIONS.md` V-104）。

---

## 6. 实测记录（本机，2026-11-13）

### 6.1 离屏快照

`cargo run -p ui_core --example snapshot_demo --features test-support -- --snapshot out\demo-showcase.png` →
`960×740` WARP 读回；种子状态下可见：色块 2 选中、单选 B、滑块 70%、输入框「你好，Kytrixshi li|」、
焦点环在输入框、CJK 四行 + 混排两行、图标与滚动列表（内容 20 行，视口约 5 行）。

### 6.2 UIA 客户端 dump（本机 UIAutomationClient 摘录）

```text
root                Window        Kytrix GUI · 焦点 / 中文 / 输入法 / 无障碍
RadioButton         色块 1（红）  SelectionItem(selected=False)
RadioButton         选项 A        SelectionItem(selected=True)
RadioButton         选项 B        SelectionItem(selected=False)
Slider              缩放比例      Value(60),Invoke
Edit                输入法测试输入框 Value()
Image               Kytrix 图标（2× 放大）
Text                한국어: 포커스, 입력기, 접근성을 하나의 트리에서 다룹니다.
```

`SelectionItemPattern` 在 F-151 之前是**缺失**的；`ValuePattern` 暴露滑块/输入框的 `value`，
`Invoke` 来自 `Click` 动作。UIA 动作端的实测：

```text
slider-before|60
slider-after|80                # ValuePattern.SetValue('80') → 应用 handler → 下一笔 delivery
radioA-after|False             # 对「选项 B」调用 SelectionItemPattern.Select()（→ Action::Click）后 A=False
radioB-after|True              # B=True
status|最近 AT 动作：Click → 选项 B
edit-before|                   # 输入框的 ValuePattern.SetValue('无障碍输入')
edit-after|无障碍输入
slider-unchanged|80            # 同一个 SetValue handler 按 node 路由，不会串到滑块
```

截图：`design/demo-phase15-default.png`（默认 + 焦点环）、`design/demo-phase15-uia.png`
（AT 动作后：滑块 80%、单选 B、焦点环在单选行）、`design/demo-phase15-ime.png`（真机 IME 上屏
「你好」）、`design/demo-phase15-snapshot.png`（离屏种子状态，含内联组合串）。

### 6.3 真机输入法

点击输入框 → 键入 `nihao` → 空格上屏：正文出现「你好」，事件计数「共 3 次」（Start / Commit / End）。
组合期间没有 `Preedit`（MSPY 的 TSF 组合 UI 只在自己的窗口里显示），**也没有准时的 `Start`**（本机实测
Start/Commit/End 三条都在提交时到达），所以应用侧无法感知真实组合；`ImeField.composing` 由
`Start`/`Preedit`/`Commit`/`End` 驱动（Linux/macOS 平台层不发 `Start`，经 `Preedit` 进入组合；本机 MSPY
三条都在提交时到达），`Start` 无组合串时状态行显示「组合中：（输入法处理中）」。原因与边界见
`DEVIATIONS.md` V-104；`Scene::preview` 与 `scene::ime` 的单测覆盖 `Preedit` 的渲染与状态机。
