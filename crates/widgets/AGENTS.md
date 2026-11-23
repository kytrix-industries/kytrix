# Widget 开发规范（P3 契约）

本文件适用于 `crates/widgets/**`，以 **`crates/widget`（P3-1 冻结契约）** 与 **`crates/ui_kit/src/kit`（引擎化组件）** 为准。
框架接口看 `crates/widget/src/{view,cx,any,surface,status,frame,binding,services}.rs`；展示规则唯一实现在
`crates/widget/src/surface.rs`。参考实现随 W 批迁移逐步更新（`force_order` 的表格与状态上报、`news` 的 binding 与
header 声音按钮、`perpetual_statistics` 的一 crate 多 widget 布局）。

> **迁移现实**：`crates/widget` 已冻结，`crates/widgets/**` 的 20 个 crate 正在按 W 批迁移，**未迁移的 crate 不能编译是
> 预期状态**。新写或迁移代码一律按本文件，不按旧接口。

---

## 1. 框架 / Widget 职责划分

**框架拥有的**（widget 不得重写、不得绕过）：

| 关注点 | 实现位置 |
|---|---|
| `WidgetState` 五状态、去重写入、失效 | `widget::status`（`Signal<WidgetState>`） |
| banner / 占位 / 正文槽的展示选择与绘制 | `widget::surface` |
| 正文槽的底色 / 圆角 / 裁剪（`WIDGET_SLOT`） | `widget::surface` |
| 恢复控件的默认接线（`common.retry` → `load`） | `AnyWidget::mount` 建的 `WidgetAction` |
| 正文一次建树的时机、节点树身份、`bind_*` 依赖边 | 框架（`AnyWidget::sync` + `WidgetCx`） |
| 每帧钩子的单槽与扇出 | `widget::frame::FrameRouter` |
| 持久化配置的编解码与写回时机 | `app_config::widget` + `widget::config` + pane（P4） |
| Header 行布局、拖动区、关闭/分割按钮 | `kytrix` 外壳（P4） |
| 连接池 / RPC / stream 生命周期 | `market_participants_connector` + `widget::{binding,runtime,stream,rpc}` |

**Widget 拥有的**：协议、取数、数据模型、就绪态正文、header 控件、`TranslatedError` 实现、子区域局部状态、
自己持久化的那部分配置。

由此得到硬约束：

1. **不写状态枚举、不在正文里判断 banner / 占位**。状态只经 `WidgetStatus::report(app, WidgetState)` 上报，位置由框架决定。
2. **正文只画「有内容」的样子**：`has_content()` 为真时才建树 / 展示；为假时框架放占位。
3. **正文只建一次**：首次 `has_content() == true` 的帧由框架调用 `body`，之后靠 `bind_text` / `bind_style` / `bind_a11y` /
   `sync` 更新；不得每帧重建正文。
4. **正文不画自己的底色 / 圆角 / `overflow_hidden`**：`WIDGET_SLOT` 已经给了；正文根从 `flex_1` + `min_width_0` +
   `min_height_0` 起步。
5. **失败与中性文案是两个出口**：`WidgetState::failed`（终态 + 恢复控件）与 `WidgetState::waiting`（没有内容时的中性引导）。
   **不用等待报「错」**，也不为「空数据」造等待文案 —— 空快照是正文里的「暂无数据」。
6. **空快照不是空状态**：拿到空表 / 空列表 / 空图时，正文里要画 `ui_kit::kit::empty_state` + `common.no_data`（表格用
   kit `Table` 的空态能力），不能留一块白。
7. **不自行创建上游连接、不 spawn 长期任务、不引入 DI / capability registry**。依赖以具体类型经构造函数注入 `service.rs`。
8. **持久化选项自己读回（`config()`）并上报（`config_change.publish(app)`）**；改了字段不上报，改动只活在内存里。
9. **应用设置只经注入的服务读**：语言 / 外观 / 数据时区 / 账户 / 目录 / 服务连接全部在 `WidgetServices` 里，不读进程全局、不
   自造 watch / 轮询。
10. **动画由 widget 自己请求帧、收敛即停，并尊重 `reduce_motion`**（见 §8）。
11. **kit 组件的 `key` / action 名必须经 `cx.unique_key(后缀)` 生成，禁止常量 key**：kit 的 key 是**应用级** Action 身份
    （`"<key>.click"`，`App::on_action` 同 Action 后写覆盖）与调试 id 前缀。两个 pane 用同一常量 key 时，后建的覆盖先建的，
    先 release / 先重建的一方再把对方的 handler 摘掉（点击派发到错的 pane 或没有 handler；W2a 审查 F1 实测）。后缀在
    widget 内唯一标识控件位置；多行重复控件把行身份拼进后缀；同一控件跨帧重建必须用同一后缀。写法与释放配对见 §9。

---

## 2. 目录布局

```text
crates/widgets/<widget_name>/
|-- Cargo.toml
`-- src/
    |-- lib.rs      # 只声明模块：mod model; mod service; pub mod view;
    |-- view.rs     # WidgetView 实现：状态上报、header、就绪态正文、header 控件
    |-- model.rs    # 数据模型、合并/排序/裁剪等纯逻辑（含单测）
    `-- service.rs  # 协议、私有 Source trait、Service、生产实现与 fake
```

- `lib.rs` 只做模块声明，不写实现、不写测试。对外只 `pub` 注册要用的模块（单 widget 的 crate 是 `pub mod view`；一个
  crate 出多个 widget 时是 `pub mod widget`，每项一个文件、只放常量与 `pub type <Name>Widget = …`）。
- 协议数据类型与 `FooSource` 这类接缝 trait 写 `pub(super)` / `pub(crate)`（跨同级子模块复用时 `pub(crate)`）；
  `service` / `model` 本身不是 `pub`。
- 职责独立时增加 `chart.rs` / `columns.rs`；不要把所有东西塞 `view.rs`。
- **dialog 一律单独成文件 `dialog.rs`**：它的视图结构体、临时态（输入 / 筛选 / 翻页 / 导航栈）、它拥有的 kit 组件与它自己的
  测试都放那里；`view.rs` 只留 `WidgetView` 实现与状态上报。widget 与 dialog 之间只通过动作闭包 / 信号交互，**不互相持有
  生命周期句柄**（引擎没有实体身份，dialog 的 kit 组件由 dialog 自己 release）。
- 不使用 `#[path = "..."]`；按 Rust 标准模块布局。
- 行内模块被 workspace lint `inline_modules = "forbid"` 挡下；`#[cfg(test)]` 测试模块除外。模块是目录时测试放兄弟文件
  `foo/bar_tests.rs`，由父模块 `#[cfg(test)] mod bar_tests;` 声明。
- 新增依赖先加 workspace 根 `Cargo.toml` 的 `[workspace.dependencies]` 与 `members`，子 crate 用
  `{ workspace = true }` / `.workspace = true` 引用。

---

## 3. 每帧到底发生了什么

```text
平台泵每轮：
  App::run_frame_hook
    |-- FrameRouter::tick：按注册序跑每个 AnyWidget 的同步路由
    |      `-- AnyWidget::sync：采样 has_content（首次为真时建 body）
    |            |-- WidgetView::sync（widget 自己的模型 → 信号 / canvas 写回）
    |            `-- WidgetSurface::sync（读状态 → 写 presentation 信号、banner、占位）
    `-- 帧管线：styles（bind_style）→ text（bind_text）→ layout → paint → commit
```

三条结论：

- **没有「每帧重建元素树」这回事**：节点树持久，只有 `bind_*` 失效的节点重算动态属性 + 局部位图；widget 不需要为性能做
  额外缓存构件，也不得每帧重建正文。
- **稳态零帧是目标（I3）**：`sync` 在没有变化时不得写信号、不得 `request_frame()`；只在状态/内容真的变了时写。
- **可见内容变化必须经过信号或 `sync` 的显式写**：没有写就没有失效边，就没有帧。`WidgetStatus::report` 是状态变化的唯一通道；
  正文里的动态值放进信号，用 `bind_text` / `bind_style` 读。
- **E10 一次性定时器在 `run_frame_hook` 的最前面派发**（早于 `FrameRouter::tick` 与帧管线）：倒计时回调里的写入在同一轮
  的路由与绘制里可见。测试装置 `WidgetTestApp::advance` 在**帧入口**派发到期的定时器（见 §11），绘制同轮可见，但路由晚
  一轮看到——不要在 widget 里依赖「同一轮 sync 已看到定时器写入」的写法。

---

## 4. `WidgetView` 契约（五段式）

```rust
pub trait WidgetView: Sized + 'static {
    type Config: WidgetConfig;

    const ID: WidgetId;                        // 持久化标识，不可依赖类型名
    const DISPLAY_NAME: &'static str;          // i18n **key**
    const CATEGORY: WidgetCategory;
    const DESCRIPTION: Option<&'static str> = None;  // i18n **key**
    const ICON: Option<&'static str> = None;
    const SELECTABLE: bool = true;             // false = 框架自建、选择列表不提供

    fn new(
        instance_id: WidgetInstanceId,
        config: Self::Config,
        status: WidgetStatus,                  // 状态唯一写入句柄（Copy）
        reload: ReloadHandle,                  // 框架的「重跑 load」能力
        config_change: WidgetConfigChange,     // 改了 config() 字段就 publish
        recovery: WidgetAction,                // 失败时给用户的恢复控件，默认 common.retry
        cx: &mut WidgetCx<'_>,
    ) -> Result<Self, EngineError>;

    fn load(&mut self, cx: &mut WidgetCx<'_>);                 // 启动/重试工作
    fn has_content(&self) -> bool;                             // 便宜谓词，不建节点
    fn config(&self) -> Option<&Self::Config> { None }          // 持久化选项；None = 不持久化
    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError>;  // 只建一次
    fn sync(&mut self, _cx: &mut WidgetCx<'_>) -> Result<(), EngineError> { Ok(()) }
    fn header(&mut self, _group: HeaderGroup, _parent: NodeId, _cx: &mut WidgetCx<'_>)
        -> Result<(), EngineError> { Ok(()) }
    fn dispatch(&mut self, _event: &Event, _cx: &mut WidgetCx<'_>) -> bool { false }
    fn release(&mut self, _cx: &mut WidgetCx<'_>) {}            // 释放 kit 组件 / 解绑 / 取消定时器
    fn instrument_link(&self) -> Option<InstrumentLink> { None }
}
```

与 kit 组件五段式的一一对应：

| kit 组件阶段 | widget 契约 | 谁调用 | 语义 |
|---|---|---|---|
| `new(app, …)` | `WidgetView::new` | `AnyWidget::mount` | 建信号与协作者；**不启动工作** |
| `build(tree, parent)` | `WidgetView::body` / `header` | 框架（body 首次有内容时）/ pane | 只建节点 |
| `mount` / `attach` | —（框架） | 框架 | 建 surface 壳，把正文槽交给 widget |
| `sync(app)` | `WidgetView::sync` + `WidgetSurface::sync` | `FrameRouter`（每帧） | 自绘重画 / 倒计时 / 行同步 + 展示规则 |
| `dispatch(app, &Event)` | `WidgetView::dispatch` + `WidgetSurface::dispatch` | pane 把事件转发进来 | 框架控件先消费 |
| `release(app)` | `WidgetView::release` | `AnyWidget::release` | 释放 kit 组件 / 解绑 / 取消定时器 |

**`AnyWidget::mount` 的不可交换顺序**（框架负责，widget 只需知道）：scope → `WidgetStatus` → surface 壳 → body 单元与
`ReloadHandle`（`Weak` 自引用）→ `W::new` → 存入 body → `load` → 注册帧路由。构造失败时框架回滚整个 mount（删 shell、
释放 scope），pane 不会拿到半成品。

**`AnyWidget::release` 的释放顺序（R-21 ↔ E16）**：摘帧路由 → `WidgetView::release`（**节点仍在**）→ surface 组件 release →
丢 widget（body 置空）→ `App::remove_node_with_scope(window, surface.root(), scope)`：**先删子树、后释放 scope**。
因此 widget 在 `release` 里可以放心摸自己的节点（解绑 key、停定时器、release kit 组件），但**不得**自己删 surface 的节点，
也不得假设 release 会在节点删除之后被调用。

### 4.1 `new` / `load` / `has_content` / `body` 的边界

- **`new` 不开始工作、不报失败**：框架在构造完成后才调 `load`。`new` 里只存字段、建信号、装配 kit 组件。
- `instance_id` 由框架分配，**不拼进节点 id / 选择器**；它只用于诊断与实例身份。
- `recovery` 存进字段，**每个失败路径**都用它（或换一个对用户更准确的 `WidgetAction` 标签 key）。`reload` 是同一能力
  的去标签版本，只有「重试」与「恢复」需要区分处理时才另存。
- **`load` 首次与恢复共用**，必须能分辨两种情况：恢复时保留用户正在看的内容，显式重载时清掉不再可信的旧数据。
- `has_content` 是便宜谓词，**不许建节点、不许计算重活**；`false` 时框架放占位，正文根本不建。
- `body` **只调一次**：之后内容变化靠 `bind_*` 与 `sync`。body 的 `parent` 是框架的正文槽（`WIDGET_CONTENT`），可以在
  槽里建一个根或多个根。
- `WidgetView::sync` 里才变成「有内容」的 body 会**晚一帧**出现（框架在 sync 前采样 `has_content`）：需要当帧显示就
  `cx.request_frame()`（状态 / banner / 占位不受影响，surface 在 sync 后重读状态）。

### 4.2 `header`

- 每次 pane 重建 header 行时调用一次，必须便宜；空 group 什么都不建。
- 只对真实控件设置可交互区域；中间留白是 pane 拖动区。
- **前置条件没满足时 header 不得留可用控件**：需要的目录 / 账户 / scope 没有时，控件必须在构造时判定为禁用（kit 控件的
  `disabled(true)` 只画状态，**不挡事件**：要么不挂 `on_click`，要么在动作入口再挡一次）。
- **不要在 `header` 里再 `update` / 重入同一个 widget**：写状态、`binding.call`、`cx.spawn`、`cx.request_frame` 都是正当的
  触发方式。
- 已 release 的 widget 被问 header 时框架返回 `Ok(())`（不建控件、不 panic），pane 可以在同一 turn 里重建 header 行。

### 4.3 `config` 与写回

- 有持久化选项 → 实现 `config()`，**直接返回字段**；没有 → 不实现（默认 `None`，布局里该 pane 的 config 为 `null`）。
  没有可持久化选项也要一个空的 `Config` 类型承载 `SCHEMA_VERSION`。
- **写回是两步**：`config()` 返回当前字段；每个改字段的地方调 `self.config_change.publish(cx.app_mut())`。重复上报安全。
- 读出来的配置可能在当前版本不成立（下架标的 / 不再支持的周期 / 不存在的倍率）：在 `load` 里、**订阅之前**逐项校验，
  不支持的那一项回 `Config::default()` 的对应值并上报；只在「确定不支持」时重置（取数失败 / 未登录 / 超时都不动）。
- 不做版本迁移、不写兼容层；不兼容变更直接删旧结构。

### 4.4 迟到的消息与终态

- `WidgetState::failed` 是**终态**：只有用户的恢复（重新 `load`）才离开。每个消息入口都要挡：`StreamKey` 是否仍属于当前
  scope、`status.is_failed(cx.app())`、等待目录时的 `awaiting_market` 等。
- **RPC 答复先按精确 `RequestId` 归因**（§8.2 的在飞命令表）：未知 id = 替换后迟到的答复，丢弃并计数，不判失败、不重试。
- 失败后不自动重连、不重试、不退避；错误由 widget 自己映射成强类型 `TranslatedError`。

---

## 5. `WidgetCx`：一个上下文，一次借一个

`WidgetCx<'a>` 是 widget 唯一拿得到的宿主视图：`&mut App`、`WindowId`、`&WidgetServices`（含组合根三单例）、自己的
`SignalScope`。节点树在 `Window` 里、`Window` 在 `App` 里，因此上下文按需借出，**同一表达式里不能同时持有两个**：

```rust
cx.app() / cx.app_mut()              // &App / &mut App
cx.tree() / cx.tree_mut()            // &NodeTree / &mut NodeTree
cx.window() / cx.window_mut()        // WindowId / &mut Window
cx.services() / cx.translate() / cx.language() / cx.theme()
cx.services_rc()                    // Rc<WidgetServices>：自建 selector host 等宿主时 clone 传入
cx.config_ref(|config| ...)          // 不克隆地读设置快照（read 不登记依赖）
cx.account() / cx.data_time_zone()   // 由设置快照派生
cx.scope() / cx.layer() / cx.frame_router() / cx.animation()
cx.unique_key("header.sound")       // kit 组件 key：`<后缀>@<本实例 scope>`，跨 pane 唯一（§9）
cx.signal(v) / cx.transient(v)       // 铸进 widget scope
cx.read(&s) / cx.write(&s, v) / cx.update(&s, f)
cx.request_frame() / cx.request_node_repaint(node) / cx.set_a11y_patch(node, patch)
cx.handle() / cx.spawn(future) / cx.runtime()
```

**组合根三单例（`cx.layer()` / `cx.frame_router()` / `cx.animation()`）**：`Layer`（浮层层序）、`FrameRouter`（`App::on_frame`
单槽唯一注册者）、`AnimationRouter`（`App::on_animation_complete` 单槽唯一注册者）都是**应用级唯一实例**，由组合根创建、
经 `WidgetServices` 注入。构建需要用它们的 kit 组件（`Overlay` / `Tooltip` / 对话框 / selector host）时 **clone 传入**
（三者都是 `Rc` 句柄，clone 共享同一实例）。**widget 不得自建**：第二个 `Layer` 会让两个浮层对谁在上意见不一；第二个
`AnimationRouter` 会静默覆盖引擎的完成槽，先前登记的淡出永远收不了尾；第二个 `FrameRouter` 装不上（单槽已被占用），
它上面的路由永远不会跑。

**借用通法**：先拿 `parent`（`NodeId` 是 `Copy`），再调组件的 `build(cx.app_mut(), window, parent)`；kit 组件的
`build` 需要 `&mut App` 时先 `let window = cx.window();`。`bind_*` 闭包里要 `Translate` / `ThemeHandle` 时先 `clone()`
（它们 `Rc` 共享）。

**自绘族**：`cx.window_mut()` 与 `cx.set_canvas(node, canvas)` 是直通口；canvas 节点必须是 `NodeBuilder::canvas(...)` 建的，
对普通节点 `set_canvas` 返回 `false`。指针位置用 `cx.app().pointer_position(cx.window())`。

---

## 6. 状态、展示与占位

### 6.1 状态写入（唯一入口）

| 写侧 | 语义 |
|---|---|
| `status.report(app, WidgetState::Connecting)` | 正在建立连接 |
| `status.report(app, WidgetState::Loading)` | 已在连接上取数 / 刷新 |
| `status.report(app, WidgetState::Ready)` | 工作完成，没有要报告的内容 |
| `status.report(app, WidgetState::waiting(notice))` | 没有内容时的中性引导（`Notice`，只用于「还没开始看」） |
| `status.report(app, WidgetState::failed(error, action))` | 失败（终态）+ 恢复控件 |

- **每次写入整体替换**；sum type 保证旧失败 / 旧文案不残留。
- **重复写入不通知**：`App::write` 等值短路（`Failed` 的相等 = 同一个错误句柄 + 同一个动作；`Waiting` 的相等含 `Notice` 内容），
  所以可以在每条消息上无条件上报。这不是优化：流式 widget 不这样做会让窗口按消息频率重绘。
- 状态写入沿依赖图失效并请求帧（surface 的根读状态信号）；**读侧**只有 `state` / `is_busy` / `is_failed`。
- 语言切换**不需要 widget 重新上报状态**：句子由 surface 在画的那一帧解析。

### 6.2 `Notice`：等待文案是目录引用，不是句子

- `Notice::message("widget.x.y")`：一条没有参数的目录文案。**它可能是富文本**：目录把若干 widget 自己的文案写成
  `<p align="center">…</p>`（`widget.news.logged_out`、`widget.empty.hint` 等），surface 会按内容判断并交给
  `ui_kit::kit::markdown` 画，widget 不需要（也不能）自己标记。
- `Notice::MarketNotLoaded { venues, supported }`：`venues` 是自己在等的市场（只是状态身份，句子不读它），`supported` 是句子
  列出来的、本组件能读的全部市场（从静态定义表取，与用户选了什么无关）。句子 `common.market_not_loaded` 由 `widget::venue`
  供给；它**是富文本**（对齐段落 + `<kbd>` 图标 + 一行一个市场名和 logo），surface 用 `ui_kit::kit::markdown` 画，
  widget 不拼文本、不插值、不自己渲染。
- **判断口径是「解析后的目录值」**：`Notice::is_markdown(&Translate)` 解析当前语言的文案，含 HTML 标签 / markdown 链接、
  图片 / `**强调**` / 行内代码就是富文本，否则是一句话。同一 key 在不同语言可以一边富文本、一边纯文本。widget 侧只报
  `Notice`，不预判、不缓存 `is_markdown`。
- 等待占位**没有动作按钮**，所以文案只能是一句话（和它指向的控件图标）；「未加载」是等待，不是失败。
- **行内图片是占位色块**：`common.market_not_loaded` 的市场 logo 与 `<kbd>` 图标在资产批接入前由
  `PlaceholderImagePaint` 画成灰块；文字、`<kbd>` 与粗体已经正确。**W1 真窗口验收不据此判缺陷**（资产批次事项，
  设计 §13.9 R-4）。

### 6.3 六行展示真值表（框架唯一实现）

| 有内容？ | 状态 | 结果 |
|---|---|---|
| 是 | `Failed` | 失败文案在正文上方的 **banner**（+ 恢复控件） |
| 是 | 其它 | 只有正文 |
| 否 | `Failed` | 失败占位 + 恢复控件 |
| 否 | `Waiting(Notice)` | notice 的句子 / 富文本 |
| 否 | `Connecting` | spinner + `common.connecting` |
| 否 | `Loading` / `Ready` | spinner + `common.loading` |

**给 widget 的推论**：

- 不要为「没有内容」造 `waiting` 文案——连接成功就显示内容；`waiting` 只给「请选择标的」这类中性引导。
- **空快照必须自己说空**：`has_content()` 为真、正文却一片空白时，用户分不清「没数据」和「坏了」。表格 / 列表用
  kit `Table` 的空态能力（列头保留、空态在表体），其它正文用 `EmptyState` + `common.no_data` 居中覆盖。
- 失败在两处（banner / 占位）显示**完全相同**的文案，不加前缀、不加包装；语气由错误类型自己的 i18n key 负责。

---

## 7. 语言、主题与 i18n

- **文案全是目录 key**：用户可见文案用 `t_smol!(translate, "key")`（带参数：`t_smol!(translate, "key", name = value)`）；
  `DISPLAY_NAME` / `DESCRIPTION` 是 key 不是文案。新增 key 必须同时加 `crates/i18n/locales/en.yaml` 与 `zh-CN.yaml`
  （缺 key 翻译成空串，是空白不是崩溃）。
- **`Translate` 是注入的纯状态**：从 `cx.translate()` 拿（`bind_*` 闭包里先 `clone()`）；**没有全局** `t!`。
- **语言信号只在 `bind_*` 闭包里读**：

  ```rust
  let language = cx.language();
  let translate = cx.translate().clone();
  NodeBuilder::text(SmolStr::default(), TextStyle::new(13.0))
      .bind_text(move |read| {
          let _language = read.read(&language);   // ← 读即登记依赖；语言写入会重算这一节点
          t_smol!(translate, "widget.my_widget.title")
      })
      .build(cx.tree_mut(), parent)?;
  ```

  在 `load` / `sync` 里读语言**不登记依赖**（那是对的：加载不画）。`Translate` 与 `Signal<LanguageCode>` 是两份状态，
  组合根按「先 `set_language`、后写信号」保持一致；widget 只读。
- **颜色必须经 `ThemeHandle` 在 `bind_style` 里读**（G3）：挂载期把 `theme.role(..)` 拷成常量，换外观后颜色会留在旧主题。
  `bind_style` 闭包里用 `read` 作为 `ThemeHandle::role` 的读上下文。整段 markdown / 逐 run 颜色的静态载荷由
  `MarkdownNode::sync` 按外观重建——widget 正文里的富文本用 `MarkdownNode` 而不是自己挂死颜色。
- 尺寸 / 字号取 `ui_kit::theme` 的令牌或 kit 组件的尺寸档，不写字面 px。

---

## 8. 服务、协议与生命周期

### 8.1 `WidgetServices`（唯一的协作者入口）

| 字段 | 用途 | 注意 |
|---|---|---|
| `translate` / `language` | 文案 + 语言依赖 | 见 §7 |
| `config: Signal<Config>` | 设置快照 | `bind_*` 里读才跟随；`sync`/`load` 里用 `cx.config_ref(..)`。**框架 surface 根也读它**：设置写入即便 widget 无正文也调度一帧（`sync` 能看到新账户 / 时区），widget 不要自己 `request_frame` |
| `theme: ThemeHandle` | 语义色 | 只在 `bind_style` 里读 |
| `market_participants: Option<MarketParticipantsConnector>` | 市场目录 | 应用根永远 `Some`；headless 测试为 `None`，widget 自己映射为业务 notice/failure。**P4 补缝后改必填** |
| `catalog_generation: Signal<u64>` | 目录已发布世代（组合根写、框架 surface 根读） | widget 在 `sync` 里比较它判断目录是否发布过新世代（§8.3）；`0` = 尚未发布；不在 `body` 里轮询 `catalog()` |
| `kytrix_service: Option<KytrixServiceConnector>` | Kytrix 服务连接（news / x / wallets…） | 同上，`None` → 自己的业务失败 |
| `open_url: Rc<dyn Fn(&str)>` | 打开外部链接 | 调用即可，不碰浏览器 / shell |
| `sound: Rc<dyn Fn(&'static str) -> Option<Bytes>>` | 应用层音效字节 | `None` = 本宿主无音频（测试通常如此），不是错误 |
| `reduce_motion: Signal<bool>` | 无障碍减弱动效 | 动画前 `cx.read(&services.reduce_motion)`，为真直接落终值 |
| `layer: Rc<Layer>` | 应用级唯一的浮层层序 | 经 `cx.layer()` 取；构建 `Overlay` / `Tooltip` / 对话框 / selector host 时 **clone 传入**。widget 不得自建（§5） |
| `frame_router: Rc<FrameRouter>` | 应用级唯一的每帧扇出 | 经 `cx.frame_router()` 取；框架的 widget 路由已在上面。widget 自建 router 装不上单槽、路由永不跑 |
| `animation_router: Rc<AnimationRouter>` | 应用级唯一的动画完成路由 | 经 `cx.animation()` 取；每个 `Overlay` / `Tooltip` 都用它。widget 自建会覆盖引擎单槽，先前淡出收不了尾 |
| `runtime: tokio::runtime::Handle` | stream / RPC 的运行时 | 不自己建 runtime |

**数据时区与账户**：画的那一帧读 `cx.data_time_zone()` / `cx.account()`（由设置快照派生，不缓存、不格式化后存快照）；
设置写入会沿 `config` 信号失效读过它的节点。

### 8.2 binding：消息化，不回调

`WidgetBinding<P>` 把 tokio 侧结果投进 `WidgetInbox<P>` 并在交付时请求帧；widget 在 `sync` 里 `binding.take()` 消费：

- `Inbound::{Reliable, Latest, Rpc}`。
- **`call` 返回 `RequestId`（P3-1i）**：这是这条请求的框架身份，回复（`Inbound::Rpc { request_id, .. }`）携带同一个值。
  `call` **替换并 abort** 上一次调用（abort 只挡尚未入箱的回复；已入箱的旧回复仍会到达）。
- **在飞命令表（P3-1i，冻结写法）**：`widget::binding::Pending<C>`，`C` 是 widget 自己的命令枚举。发令时
  `let id = binding.call(cx, request); self.pending.track(id, Command::X);`；收令时
  `match self.pending.take(request_id) { Some(Command::X) => ..., None => { /* 丢弃 */ } }`。**未知 id = 替换后迟到的答复，
  一律丢弃并计数（`pending.dropped()`），不得落到当前在飞命令上**——只有一个命令槽的 widget 也照此办理（W1 F11：
  不要用「整体 pending 标志」或「id 单调比较」归因）。主动放弃命令（签出 / 失败 / 回到等待目录）时 `pending.clear()`
  （不计数）；它之后的答复按迟到计。
- 一个 binding 一张表；两个 binding 的 widget（如 `prediction_market` 的正文与盘口）各持一张。
- 失败 / 取消后不再投递；widget 在 `sync` 里 drain 时先做 `is_failed` / `StreamKey` / `awaiting_market` 守卫——**id 归因与
  终态守卫是两道独立的门**，缺一不可。
- `Ready` 水位由**协议自己**定义：拿到完整快照不等于业务成功，widget 自己维护 `pending_snapshot` / `ready_received`
  （「refresh 已请求、轮次还没上报」这类**轮次状态**不是命令槽：它随流消息清，不随答复清）。
- 长任务由服务的流监督器持有；不 `detach` 无终点的任务，不写自动重连 / 退避。

### 8.3 目录：前置检查与世代变化

**前置检查（`load` 的第一件事）**：`WidgetView::load` 的**最前面**检查自己需要的市场是否已加载
（`services.market_participants()` 的 `LoadedVenues`）；不在就
`report(waiting(Notice::MarketNotLoaded { venues, supported }))` 并返回。widget **不加载目录、不触发发现、不重试**。

**目录变了怎么知道（唯一写法）**：组合根把目录的已发布世代写进注入信号
`services.catalog_generation: Signal<u64>`（值 = `CatalogGenerationId::get()`，`0` = 尚未发布；P4 组合根在
`VenuesUpdated` 到达后写）。框架的 surface 根在渲染时读这个信号，所以**一次写入恰好请求一帧**，并让每个已挂载
widget 的 `sync` 跑到——包括还没有正文、正卡在「等待市场」的 widget。widget 侧只做比较：

```rust
// new：记下构造时看到的世代（`cx.read` 不登记依赖——构造不画）。
let generation = cx.services().catalog_generation;
let seen = *cx.read(&generation);

// sync：世代动了就更新记录并重跑 load；重跑会重新做前置检查、重建 route / 订阅。
let generation = *cx.read(&self.generation);
if generation != self.seen {
    self.seen = generation;
    self.market_reloaded = true;
    self.load(cx);
}
```

- **不要**在 `body` / `sync` 里轮询 `catalog().generation_id()`（connector 属组合根，widget 不持有）、不要自建观察节点、
  不要订阅 connector 事件、不要缓存「当时的目录」。目录派生数据只经 `load` 重跑后由 `sync` 写回，再走 `bind_*` 重绘。
- `load` 重跑要分清「目录刷新」与「用户恢复」：前者保留用户正在看的内容（只重建 route / 订阅），后者清掉不再可信的旧数据。
- 稳态不出帧：`catalog_generation` 只在目录真的发布新世代时变；`sync` 里没变就什么都不写（I3）。
- 契约层保证：surface 根读该信号（`surface.rs` 的依赖锚点），因此「等待目录」的 widget 不需要正文就能被唤醒。

---

## 9. 正文、kit 组件与释放

- **正文一次建树**：`body` 里用 `NodeBuilder` 与 kit 组件建子树；之后更新走：
  - `bind_text`：文本（闭包里读语言 / 信号）；
  - `bind_style`：颜色 / 可见性（`StylePatch`；主题与 presentation）；
  - `bind_a11y`：a11y 动态补丁（role / label / hidden / checked…）；
  - `sync`：结构 / 画布 / 实测回写（列表行内容、canvas、行高）。
- **kit 组件也走五段式**：`new(app, …)` → `build(tree, parent)`（或 `build(app, window, parent)`）→ `mount`/`attach`（如列表）→
  `sync(app)` → `dispatch(app, &event)` → `release(app)`。控件的 `on_click` 回调是 `Rc` 闭包，捕获 widget 需要的信号与动作。
- **kit 组件的 `key` 只从 `cx.unique_key(后缀)` 取，禁止常量**（硬约束 11）。kit 的 key 是**应用级**动作身份
  （`Button::on_click` 由 key 派生 `Action::Custom("<key>.click")`，`App::on_action` 同 Action 后写覆盖）与调试 id 前缀。
  两个 pane 都建同一个 header 控件时，常量 key 让后建的覆盖先建的，先 release / 先重建的一方再把对方 handler 摘掉
  （点击派发到错 pane 或没有 handler；W2a 审查 F1 实测）。后缀是 **widget 内的控件位置名**：一个控件一个后缀；行内重复
  控件把行身份拼进后缀（`format_smolstr!("row.{index}.sound")`）。同一控件跨帧 / 语言切换重建必须用**同一后缀**
  （返回同一 key），重建与释放才配得上对。

  ```rust
  // body / header：key 由本实例身份派生（`<后缀>@<scope>`），两个 pane 不会撞。
  let key = cx.unique_key("market_trade.header.instrument");
  let theme = cx.theme().clone();
  let window = cx.window();
  let button = Button::new(cx.app_mut(), theme, key.clone())
      .label(label)
      .on_click(move |action_ctx| { /* 只写自己 pane 的模型 / 信号 */ });
  button.build(cx.app_mut(), window, parent)?;
  // 记住 key / action / 组件句柄（跨帧存活）；下面的重建与 release 用同一份。
  let action = button.action_identity().expect("on_click 注册了 <key>.click");
  self.header = Some(HeaderControl { button, key, action });

  // release / 重建前（存活守卫通过后）：先摘自己建的节点，再释放组件。
  if let Some(control) = self.header.take() {
      control.button.release(cx.app_mut()); // off_action(<key>.click) + release_scope
  }
  ```

  **只 `off_action` 不 `Button::release` 会漏组件作用域**（W2a 审查 F2：header 每次重建 +10 live 信号槽）；kit 组件的
  `release` 两件事都做，所以优先在字段里保留组件句柄。需要「只摘 handler、不交还组件」的接缝属于 selector 批（§15.5）。
- **需要组合根单例的 kit 组件用 `cx` 的访问器装配**：`Overlay` / `Tooltip` / 对话框 / selector host 的构造要
  `theme.clone()` + `layer.clone()` + `animation.clone()`（`cx.theme()` / `cx.layer()` / `cx.animation()`），`sync` 由
  `cx.frame_router()` 上的路由驱动（widget 自己的 `sync` 里调 `component.sync(cx.app_mut())` 即可，不要再注册第二个
  `App::on_frame`）。三个 `Rc` 句柄 clone 的是同一个组合根实例，widget 不新建（§5）。
- **需要整个服务聚合的宿主**（`SelectorHost` 一类拥有 `Rc<WidgetServices>` 的宿主）用 `Rc::clone(cx.services_rc())`
  取组合根那一个 `Rc`：不重建聚合、不逐字段复制，宿主与 widget 共用同一实例（P3-1f）。
- **表格 / 列表的行闭包没有 `App`**：行内容里的动态值只能来自挂在节点上的 `bind_*`（读主题 / 设置信号）或行模型字段；
  需要 App 的结构性更新放到 widget 的 `sync`（`set_rows` / `set_items` / `push`）。
- **释放顺序**：`WidgetView::release` 里 release 自己建的 kit 组件（节点仍在，可解绑 / 停动画），**不要自己删 surface 节点**；
  框架随后 `remove_node_with_scope` 先删子树、再释放 scope。自己持有、但不在 surface 子树里的节点 / 作用域，必须在
  `release` 里显式清理（`cx.app_mut().remove_node_with_scope(..)` 或 `release_scope`）。
- 被隐藏的 spinner 不空转：占位切到正文时框架释放占位组件，widget 不需要操心；**widget 自己动画**时只在有可见位移时
  `request_frame`，收敛立刻停，`reduce_motion` 为真直接落终值。
- 调试选择器：框架占用 `WIDGET_SURFACE` / `WIDGET_SLOT` / `WIDGET_CONTENT` / `WIDGET_PLACEHOLDER` / `WIDGET_BANNER` 与
  `widget.{placeholder,banner}.action.*`；富文本 notice 的根是 `MARKDOWN`；kit / ui_kit 组件有自己的选择器；widget 自媒体
  用 `TEST_*` 或自有前缀。

---

## 10. 错误

- 面向用户的错误必须**强类型**并实现 `widget::error::TranslatedError`
  （`fn translated_message(&self, translate: &Translate) -> SmolStr`）。
- **严禁** `Display` / `to_string()` / 字面量拼用户可见错误。常见错误（`MarketError`、`MarketParticipantError`、
  `RpcCallError<E>`、`TransportError`、`NetworkError`、`KytrixServiceError`、`XApiError`、`WalletApiError`…）已在
  `crates/widget/src/error.rs` 实现；widget 自己的错误类型在自己的模块里实现该 trait。
- `WidgetState::failed` 收的是 `Arc<dyn TranslatedError>`，框架不补前缀；文案必须自足（语气写在错误类型自己的 key 里）。
- 「变更型命令结果未知」（超时 / 取消 / 响应丢失）必须当未知处理，不自动重试。
- 内部错误继续用 `EngineError`（节点 / 窗口 / 树），不出现字符串错误。

---

## 11. 测试（`WidgetTestApp`）

- **unit test 与被测代码同模块**（`#[cfg(test)] mod tests`），可访问私有字段；不建集中式 `tests.rs`。
- 契约层装置是 `widget::test_support::WidgetTestApp`（`#[cfg(test)]` 或 feature `test-support`）：
  - 组合与生产相同的 `WidgetServices`（目录 / 服务为 `None`，`catalog_generation` = 0，`open_url` / `sound` 是 no-op，
    `reduce_motion` = false）；
  - **组合根三单例**（P3-1e）：宿主自建 `Layer` / `FrameRouter` / `AnimationRouter`，经 `WidgetServices::builder` 注入，
    并用自己的句柄暴露为 `layer()` / `router()` / `animation()`；`cx.layer()` / `cx.animation()` 打开的浮层可直接在这些
    宿主句柄上断言（注册进同一栈、装的是同一个完成路由）。测试不得另建一套单例；
  - `WidgetTestApp::new()`（fixture 目录）或 `with_translate(...)`（产品目录文案）；
  - **注入协作者**（P3-1d，不再在 widget crate 里加私有源缝）：`with_open_url(|url| ...)`、`with_sound(|name| ...)`、
    `with_market_participants(Option<MarketParticipantsConnector>)`、`with_kytrix_service(Option<KytrixServiceConnector>)`。
    headless 测试可以自建 `MarketParticipantsConnector`（`MarketParticipantsConnectorInput` + `Runtime::from_handle(测试 runtime)`）
    后交给 `mount` 驱动目录路径；`KytrixServiceConnector::install` 仍要工具包上下文，headless 只能传 `None`（设计 §13.9 R-1，
    P4 补缝）；
  - **runtime 驱动**（P3-1d）：`drain_runtime()` 推进 services 所在的 **current-thread** runtime 一个 turn；
    `run_until_idle()` 交替 runtime turn 与 `drain()` 直到 `frame_requests` / 任务数都不再变化（有界 `RUNTIME_TURNS`，返回
    turn 数）。**不 sleep、不推进时钟**：需要定时器 / 超时的测试仍用自己拥有的 `#[tokio::test(start_paused = true)]`
    runtime；`WidgetCx::spawn` / `AppHandle` 的任务走引擎 handle，不由这两个方法驱动；
  - `mount::<W>(config)` / `try_mount` / `try_mount_in_scope(parent, scope, config)`（拒绝挂载失败路径的探针）、
    `build_surface` / `sync_surface`、`report` / `switch_language`；
  - `drain()` = `router.tick` + `TestApp::drain`（生产泵序；`TestApp::drain` 本身不跑 `on_frame`）；
  - **时钟推进与 E10 定时器**（P3-1g）：`advance(by: Duration) -> bool` 推进**引擎时钟**并跑一轮
    （`router.tick` + 一帧），返回本轮是否提交像素。`App::after` / `after_scoped` 的 deadline 以引擎钟为基准，
    `advance` 是唯一推进源：**不读 wall clock、不真实 sleep、不等待平台定时器**（I3）。到期的定时器在帧**入口**派发，
    回调的写入参与同一帧绘制；自续期倒计时每 `advance` 触发**一次**（不是按经过的 delay 次数补触发）。未到 deadline 的
    `advance` 是空闲轮：`false`，`frame_index()` / `frame_requests()` 都不变。宿主窗口在第一次 `drain()` / `advance()`
    之前是「未提交」状态，测空闲语义前先 `drain()` 一次。widget 自己的「现在」（倒计时格式化的时间源）**不是**引擎钟：
    测试自己写那个状态；
  - **观测面**（P3-1g）：`frame_index()`（提交游标，`0` = 未提交）、`frame_requests()`（I3 断言面）、`rendered()`
    （最近一轮是否提交像素；`advance` 的返回值就是本轮这个事实）。
  - **值槽位统计**（P3-1i）：`signal_slot_stats() -> SignalSlotStats { slots, live, free }`（E21 透传）。释放断言的读点：
    widget / 组件释放后 `live` 应回到 mount 前，反复开关不应让 `slots` 增长；根作用域铸造的信号永不回收，是它要抓的泄漏。
  - 断言面：`visible` / `text_of` / `has_text` / `has_canvas` / `has_inline_boxes` / `node_count` / `find_by_debug_prefix` /
    `click_recovery` / `enable_a11y_capture` + `a11y_hidden`。
  - **canvas / 图元读点**（P3-1j）：自绘 widget 必须断言**画出来的图元**，不能只断言 `has_canvas`——删掉 `set_canvas`、
    去掉动画的 `request_repaint`、稳态短路失效，都曾在「只有 `has_canvas` 断言」的测试下存活（W4a/W4b/W5a 审查）。
    三个读点（E23）：
    - `paint_entries(node)`：节点**自有**绘制（背景 quad / 文本 / 图片 / 自绘节点那条不透明 `PrimitiveKind::Canvas`），
      用来断言「这个节点这一帧被画了」；
    - `canvas_primitives(node) -> Vec<CanvasPrimitive>`：画布**内容**，按 `Canvas::push_*` 顺序，一 entry 一次 push：
      quad（bounds / `corner_radius` / `color` / `border` / `gradient`）、path / geometry（`geometry` 是注册的
      `GeometryId`）、`ClipPush` / `ClipPop` 括号、每条文本 run 一个 `GlyphRun` entry（`glyphs` = 定位字形数）；
      颜色是 widget 写入的 `ui_core::paint::color::Color`（与 `theme.theme_for(appearance).role(role)` 直接比，不是
      `QuadRecord` 的解析后线性预乘）；
    - `canvas_handle(node) -> Option<Arc<Canvas>>`：节点树上的输入值，**不需要帧**；同一 `Arc` 跨稳态 sync 不变。

    读帧的两个方法读的是**最近一次 `drain()` 的那一帧**：空闲轮没有绘制（都为空），断言放在 `rendered()` 为真的那一轮
    之后；无 drain 记录时 panic（与 `rendered()` 同口径）。四种冻结写法（样板：`crates/widget/src/test_support.rs` 的
    `CanvasWidget` 与 `test_support::tests::a_canvas_*` / `a_settled_canvas_*` / `switching_the_appearance_*` /
    `releasing_a_canvas_*`）：

    ```rust
    // A 稳态：内容没变的 sync 不重写画布、不请求帧（I3）
    let before = harness.canvas_handle(canvas).expect("canvas node");
    let requests = harness.frame_requests();
    harness.with_cx(|cx| widget.sync(cx)).expect("sync");
    harness.drain();
    assert!(!harness.rendered(), "稳态不出帧");
    assert_eq!(harness.frame_requests(), requests);
    assert!(Arc::ptr_eq(&before, &harness.canvas_handle(canvas).expect("canvas node")),
        "稳态短路失效（每帧 set_canvas）会换掉这个 Arc");

    // B 主题：颜色跟着外观重解析（不是挂载期常量）
    theme.set_appearance(harness.app_mut(), Appearance::Dark);
    harness.drain();
    let primitives = harness.canvas_primitives(canvas);
    assert_eq!(primitives[1].color, Some(theme.theme_for(Appearance::Dark).role(Role::MarketUp)));

    // C 未写入：删掉 set_canvas 的变异在这条上红（节点仍是一条不透明 Canvas 图元）
    assert!(harness.canvas_primitives(canvas).is_empty(), "没有 set_canvas 就没有内容");

    // D 几何归还：画布里的几何 id 可读，释放后计数回基线
    let path = harness.canvas_primitives(canvas).into_iter()
        .find(|p| p.kind == PrimitiveKind::Path).expect("path");
    assert!(path.geometry.is_some());
    widget.release(harness.app_mut());
    assert_eq!(harness.app().geometry().len(), baseline, "释放归还几何");
    ```

    动画帧必须 `cx.request_frame()`：headless 的 `drain()` 每轮都跑 `sync`，只 `set_canvas` 不请求帧仍会提交，所以
    「动画会不会停」的断言面是 `frame_requests()` 随动画轮次增长，不是 `rendered()`。
- **倒计时 / 定时器测试写法**（P3-1g；取代 W2c 自建的 `ClockHost`，补 W2b 的倒计时 timer 缺口）：

  ```rust
  #[test]
  fn a_countdown_should_tick_once_per_advance() {
      let mut harness = WidgetTestApp::new();
      let widget = harness.mount::<MyWidget>(MyConfig::default());
      // 首帧：建 body / 求值 bind_text（首次读信号 → 登记依赖），之后写信号才会请求帧
      harness.drain();

      // 到点前：半程不触发，也不出帧
      assert!(!harness.advance(Duration::from_millis(125)));
      assert_eq!(harness.text_of(label), "10:00");

      // 到点：回调写倒计时信号 → 请求帧 → 同一轮绘制；回调的写入是本轮唯一的帧请求
      assert!(harness.advance(Duration::from_millis(125)));
      assert_eq!(harness.text_of(label), "09:59");

      // 再推进但没有变化：不出帧（I3；frame_requests 不增长）
      assert!(!harness.advance(Duration::from_millis(100)));
  }
  ```

  `label` 是 widget 自己的倒计时文本节点（自有 `TEST_*` / 选择器）；定时器由 widget 在 `load` / `sync` 里用
  `App::after_scoped`（scope = body root）自续期注册，回调只写倒计时信号，标签未变不 `request_repaint`。
  **两个前提**：① 信号必须先被节点读过（首帧 `bind_text` 求值）再写，否则写不请求帧；② 宿主一轮的顺序是
  `router.tick` → 帧（定时器在帧入口派发），生产是「定时器 → `FrameRouter::tick` → 帧」（`App::run_frame_hook`）。
  所以「`sync` 必须在本轮看到回调写入状态」的断言要多 `advance` 一次，或直接断言绘制结果——不要假设测试装置与
  生产泵逐拍同序。
- **RPC 答复归因测试写法**（P3-1i；取代「整体 pending 标志 / id 单调比较」的旧断言）：

  ```rust
  #[test]
  fn the_answer_to_a_replaced_command_should_be_dropped() {
      let mut harness = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
      let mut pane = pane(&mut harness);          // 测试自建 widget（bindings 的 id 从 1 起）
      load(&mut harness, &mut pane);              // 第 1 条命令：Load（id 1）
      replace_the_command(&mut harness, &mut pane); // 替换：第 2 条命令（id 2），旧 id 被遗忘

      // 旧命令的答复（id 1）此刻才到：必须被丢弃，不能落到 id 2 的命令上。
      harness.with_cx(|cx| {
          pane.widget.apply_rpc(
              RequestId::new(1),
              Err(RpcCallError::ServiceClosed { request_id: RequestId::new(1) }),
              cx,
          );
      });

      assert!(!pane.status.is_failed(harness.app()), "旧命令的失败不是新命令的");
      assert_eq!(pane.widget.pending.dropped(), 1, "迟到答复被丢弃并计数");
  }
  ```

  断言三件：**匹配 id 的答复被应用**（成功 / 失败落到对应命令）、**未知 id 被丢弃且 `dropped()` 增长**、**终态不被迟到
  答复重绘**。测试里手工构造 `Inbound::Rpc` 时，`request_id` 必须与 widget 实际发出的调用序列一致（每个 binding 的 id
  从 1 开始，`call` 每次 +1，被替换也占号）；不确定时读 `pending` 或用注入通道。
- **释放 / 泄漏断言写法**（P3-1i，W2c-R9）：

  ```rust
  #[test]
  fn releasing_the_pane_should_reclaim_every_value_slot() {
      let mut harness = WidgetTestApp::new();
      let baseline = harness.signal_slot_stats();

      let widget = harness.mount::<MyWidget>(MyConfig::default());
      assert!(harness.signal_slot_stats().live > baseline.live, "widget 铸了自己的值");
      widget.release(harness.app_mut());
      assert_eq!(harness.signal_slot_stats().live, baseline.live, "释放归还全部值槽");

      for _ in 0..8 {
          let widget = harness.mount::<MyWidget>(MyConfig::default());
          widget.release(harness.app_mut());
      }
      assert_eq!(harness.signal_slot_stats().slots, baseline.slots, "槽位复用，不增长");
  }
  ```

  两个口径：`live`（当前存活，应回到基线）与 `slots`（分配过的总数，反复开关不应增长——增长说明释放没归还或每次都
  新铸）。kit 组件的泄漏同样可测（`Button::release` 漏了组件作用域时 `live` 会涨），W2a 审查 F2 就是这么发现的。
- 测试属性：纯逻辑 `#[test]`；涉及 tokio 服务 `#[tokio::test]`（dev-deps 开 `tokio` `test-util`）。界面测试一律经
  `WidgetTestApp`，不依赖任何窗口工具包。
- 外部 crate 用测试装置：dev-dependencies 里 `widget = { workspace = true, features = ["test-support"] }`
  （workspace 单写者统一登记）。
- 覆盖建议：状态上报（含去重）、有 / 无内容两种失败出口、迟到消息与 `RequestId` 挡迟到、恢复动作、取消与替换、
  空快照的「暂无数据」、容器的能力裁剪、配置写回与规则 2 回默认、语言切换后的重绘、a11y hidden。
- **多 pane / 多实例**：kit key 与 `App` 级 action 是全局的，涉及它们的行为必须用**同一 `WidgetTestApp` 的两个 widget 实例**
  测：两个实例的 `cx.unique_key(同后缀)` 不同、两边的 handler 都在、release 一个之后另一个的 handler / 节点 / 帧路由仍在
  （P3-1h 的契约测试是 `cx::tests::releasing_one_pane_leaves_the_other_pane_s_action_handler` 与
  `test_support::tests::two_widget_instances_live_side_by_side`，widget crate 照此写自己的版本）。
- **会踩的坑**：
  1. 断言「写 → 出帧」用 `harness.frame_requests()`（`App::write` 只在有节点读过该信号时才请求帧）；
  2. `sync` 里才变「有内容」的正文晚一帧出现——断言前 `drain()` 或让 widget `request_frame`；
  3. 占位 / banner 的文案由 surface 解析：断言看到的是**渲染后的文本**（不应含 `<p` / `<kbd>` / `![](`）；
  4. a11y 断言要先 `enable_a11y_capture()`，且首帧是全树；
  5. 节点 id 是框架的选择器约定，widget 不要自己起 `WIDGET_*` 名字；
  6. 定时器 / 倒计时测试不要 `std::thread::sleep`、不要等 wall clock：先 `drain()` 一次建依赖，再用 `advance(delay)`
     （写法见本节的倒计时示例）。

---

## 12. 新 widget 检查清单

1. `WidgetId::new_static("kytrix.<名字>")`，`kytrix` 后一段、小写、点分段；持久化后不可改。
2. `Config` 实现 `WidgetConfig` + `SCHEMA_VERSION`（当前 `V1`）；每一项有 `Default`；没有可持久化选项也给空类型。
3. `DISPLAY_NAME` / `DESCRIPTION` key 同时加 `en.yaml` 与 `zh-CN.yaml`；`CATEGORY` 正确；`SELECTABLE` 只有框架自建组件为 `false`。
4. `model.rs` 纯逻辑 + 单测先行；`service.rs` 私有 Source trait + fake（正常 / 超时 / 失败 / 取消 / 替换）。
5. `view.rs`：`new` 不干活、`load` 只做前置检查 + 启动、`has_content` 便宜、`body` 只建一次、状态只经 `report`。
6. 每个改持久化字段的地方 `config_change.publish(cx.app_mut())`；规则 2 的校验在 `load` 里、订阅之前。
7. 消息入口全部挡 `StreamKey` / `is_failed` / `awaiting_market`；RPC 答复按 `RequestId` 精确归因（§8.2）；失败即终态。
8. 正文：`flex_1` + `min_*_0`，无自绘底色 / 圆角 / 裁剪；空快照画 `EmptyState` + `common.no_data`。
9. 语言文本在 `bind_text` 里读语言；颜色在 `bind_style` 里读主题；不把颜色 / 句子烤进挂载期常量。
10. `name-key` / 文案不自己拼；错误实现 `TranslatedError`。
11. release 释放自己建的 kit 组件与自定义作用域；不删 surface 节点；kit 组件的 key 全部经 `cx.unique_key(后缀)`（§9），
    release / 重建用与 build 相同的那个 key 或组件句柄。
12. 删掉重构后不再使用的代码、字段、i18n key 与依赖；生产代码不允许 `#[allow(dead_code)]` / `#[expect(...)]`。

**明确不做**：自动重连 / 退避；框架级 Ready 水位；widget 级缓存构件；共享恢复按钮包装；进程全局查服务；
在正文里判断 banner / 占位。

---

## 13. 骨架示例（单 widget）

```rust
use smol_str::SmolStr;
use ui_core::{error::EngineError, node::{builder::NodeBuilder, id::NodeId}, text::TextStyle};
use widget::{
    category::WidgetCategory, config::WidgetConfigChange, cx::WidgetCx,
    identity::{WidgetId, WidgetInstanceId},
    status::{WidgetAction, WidgetState, WidgetStatus},
    view::{HeaderGroup, ReloadHandle, WidgetView},
};

pub struct ExampleWidget { status: WidgetStatus, loaded: bool }

impl WidgetView for ExampleWidget {
    type Config = ();
    const ID: WidgetId = WidgetId::new_static("kytrix.example");
    const DISPLAY_NAME: &'static str = "widget.names.example";
    const CATEGORY: WidgetCategory = WidgetCategory::Market;

    fn new(_id: WidgetInstanceId, _config: (), status: WidgetStatus, _reload: ReloadHandle,
           _change: WidgetConfigChange, _recovery: WidgetAction, _cx: &mut WidgetCx<'_>)
           -> Result<Self, EngineError> {
        Ok(Self { status, loaded: false })   // 只存字段；不启动工作、不报失败
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        self.status.report(cx.app_mut(), WidgetState::Connecting);   // 之后启动 binding/RPC
    }

    fn has_content(&self) -> bool { self.loaded }

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let translate = cx.translate().clone();
        let language = cx.language();
        NodeBuilder::text(SmolStr::default(), TextStyle::new(13.0))
            .bind_text(move |read| { read.read(&language); i18n::t_smol!(translate, "widget.example.title") })
            .build(cx.tree_mut(), parent)?;
        Ok(())
    }

    fn sync(&mut self, _cx: &mut WidgetCx<'_>) -> Result<(), EngineError> { Ok(()) }
    fn header(&mut self, _group: HeaderGroup, _parent: NodeId, _cx: &mut WidgetCx<'_>)
        -> Result<(), EngineError> { Ok(()) }
    fn release(&mut self, _cx: &mut WidgetCx<'_>) {}   // release 自己建的 kit 组件
}
```

40 行；真实 widget 在此基础上加 `model.rs` / `service.rs`（§2）与 `binding`（§8.2）。

---

## 14. 验收

```powershell
# 单个 widget crate
cargo +nightly fmt -p <crate> -- --check
cargo +nightly clippy -p <crate> --all-targets -- -D warnings
cargo test -p <crate>
cargo check -p <crate> --all-targets

# 框架契约层自身
cargo +nightly fmt -p widget -- --check
cargo +nightly clippy -p widget --all-targets -- -D warnings
cargo test -p widget
cargo check -p widget --all-targets
```

测试二进制直接跑，**不需要任何环境变量**：链接期由 `ui_core/build.rs` 把本仓的 PerMonitorV2 进程清单
（R07-1）编进每个链接 `ui_core` 的 exe；全仓只有这一份清单来源（P5-3 起迁移期的清单过渡开关已删除）。

未执行的在线 RPC、真实窗口与 release profile 验证必须在交付说明里明确写「未执行」；静态检查通过不等于性能已测量。

---

## 15. 契约缺口登记（kit / theme 后续批处理）

W1（`news` / `force_order`）实迁与 P3-1f 契约补充暴露的 **kit 侧、theme 侧**缺口登记于此。**归属是 kit 后续批（组件）
与 theme 后续批（排版令牌），不是 widget 批**：widget 侧不得各自重建组件库、另造绑定面或自实现容器。

### 15.1 静态文案重建通法（kit 未补绑定面之前的唯一写法）

`Badge` / `Button`（`label` / `icon`）/ `EmptyState`（`text`）的文案是**构建期** `SmolStr` / `IconKind`，`kit::table` 的
`Column::header` / `empty_text` 同理——它们不带 `bind_text` 闭包；而 §7 要求文本在 `bind_*` 里读语言。两者相遇时的唯一
通法是**重建（rebuilt, not re-bound）**，每个静态块四步：

1. **记 key**：块构建时记下它按哪些值建的（`empty_language` / `(language, enabled, available)` / `rows_language`），
   不记节点 id；记在 widget 自己的字段里，跨帧存活。
2. **`sync` 比较**：在 widget 的 `sync` 里读现值（`*cx.read(&cx.language())`、`cx.config_ref`、宿主可用性），相等就直接
   返回——稳态不写信号、不请求帧（I3）。
3. **存活守卫摘节点 + release**：值变了，先摘自己建的节点、再释放组件作用域与 action handler。
   - **摘节点前查存活**：`if cx.tree().get(node).is_err() { return Ok(()); }`。header 行由 pane 重建，旧控件节点已随行
     消失；直接 `remove_node` 会返回 `StaleNode`，整次重建失败、控件再也没有（W1a2 F2 的实测缺陷）。
   - **父节点同样查存活**：pane 重建 header 行后旧 `parent` 也是陈旧句柄；`cx.tree().get(parent).is_err()` 时直接放弃
     重建（控件已随行消失，没有可重建的目标）。
   - **顺序 = 「节点先删 → 组件 `release`」**：与框架 E16 / `remove_node_with_scope` 的顺序一致；`Button::release` 释放
     组件作用域与 `"{key}.click"` action handler。
4. **按现值重建**：用同一 key 规则 / 同一父节点的现值重建组件，并把新的 key 值记回去（`sound_built = (language, enabled, available)`）。

**行内静态组件（表格 / 列表的行闭包）**没有 `App`，也不该在行闭包里做结构性更新：用容器的 `list::set_count` **重建可见
窗口**。`news` 在 `sync` 里比较 `rows_language`，语言变了就 `list.set_count(cx.app_mut(), count)`（保留滚动位置）并
`cx.request_frame()`（count 没变，别的路径不会为这次重建请求帧）；行里的动态文本仍各走 `bind_text`，只有分类徽标这种
静态组件随行重建。语言切换是低频事件，重新测量可见行的代价是当前 kit 面的真实成本（§15.2）。

三处参考实现（`crates/widgets/news/src/view.rs` 模块文档「Static copy is rebuilt, not re-bound」）：正文空态
`sync_empty_state`、header 声音控件 `sync_sound_button` + `release_sound_button`、行内徽标 `rebuild_rows_for_language`；
共用的摘节点助手是 `remove_own_node`。对应测试：`the_body_should_say_when_the_feed_is_empty`（空态）、
`the_rows_should_be_rebuilt_when_the_language_moves`（行 + header）、
`the_header_control_should_survive_the_pane_rebuilding_its_row`（存活守卫）。

**不要**把这四步抽成 widget 间共享封装（D-10：不抽跨 widget 抽象），也**不要**在 widget 里复制 kit 外观（W1a 手写的
badge pill 已按审查 F3 改为 `kit::badge::Badge`）。

### 15.2 kit 静态文案绑定面（kit 后续批）

缺绑定面的直接代价（登记，不在 widget 批解决）：

- 语言 / 选项 / 可用性变化必须**重建可见块**：`news` 切语言要重建可见行（重新测量），而不是一个 `bind_text` 重算文本；
- 重建逻辑每个 widget 各写一遍（§15.1 的三处示例 + 各自测试），`sync` 里多一份「上次按什么值建的」状态；
- 行内静态组件只能整窗重建（`set_count`）——重建次数与批量增长的合并写法相关（见 §15.3 的 `List::push`）。

**kit 后续批计划**：给 `Badge` / `Button`（`label` + `icon`）/ `EmptyState` / `kit::table::Column` 增 `bind_text` /
`*_key` 形式的绑定面（或等价的 `Signal` 文本槽）。补上之后，widget 的对应块改回 `bind_*`、删除 §15.1 的重建状态与它们的
测试。在此之前 W 批一律按 §15.1 写：**不改 kit 组件语义、不另造绑定面、不把重建逻辑下沉到共享封装**。

### 15.3 `kit::list` 无 separator、无像素 overscan（kit 后续批）

旧 `DequeUnsizedList::separator` 与 `LIST_OVERDRAW = 150 px` 没有对等能力。当前通法：行内容里自建 `kit::separator`
（`index > 0`）近似 separator；`ListOptions::variable(row_height)` + 默认 overscan **行数**近似像素缓冲（**已知差异**，
W1 真窗口验收不据此判缺陷）。**登记 kit 后续批**：`list` 增加 separator 钩子与像素级 overscan 估计；在此之前 widget
不自己实现虚拟列表 / 滚动容器，也不改 `kit::list` 的 `ListOptions`。

W1a2 追加登记的两处 `list` 代价：① **`List::push` 每条都重建可见窗口**（`list.rs` 自述的已知差异），双端列表 burst 时
应合并为一次 `set_items`（或等 kit 增 `push_many`），W 批不自己实现增量缓冲；② **行容器默认 padding**
（`padding_x=6 / padding_y=2`）叠加行内容 `padding(8.0)` → 水平 14 px、垂直 10 px，与旧 `px_2 py_2` 有视觉偏移，登记为
已知差异，等 kit 补 `ListOptions::padding_*`。

### 15.4 正文 / 次要文本排版令牌（theme 后续批）

`ui_kit::theme` 目前只有**控件尺寸阶梯**（`theme/components.rs` 的 `SizeTokens.font_size = 12/14`），**没有正文排版
令牌**；W1a 的正文因此写了字面 px（标题 / 内容 `13.0`、时间 `11.0`），既违反 §7「尺寸 / 字号取 `ui_kit::theme` 令牌或
kit 尺寸档，不写字面 px」，也与旧实现漂移（旧：标题 / 内容走主题正文档，时间 `text_xs()` = 12 px）。

W1a2 的当前写法：`UnifiedSize::Medium.text_size()`（14，标题 / 内容）/ `UnifiedSize::Small.text_size()`（12，时间）——
即**借用控件尺寸档**，不是正文档；时间回到旧 12 px，标题 / 内容比旧正文档大一档。**登记 theme 后续批**：`theme` 增
正文 / 次要文本 / 标题的排版令牌（body / secondary / caption 三级）；在此之前 W 批统一走 kit 尺寸档（或具名常量）并
登记与旧实现的偏差，**不写裸 px**。

### 15.5 已迁移 widget 的 kit key 唯一化（W 批跟进）

P3-1h 冻结 `cx.unique_key` 后，**已迁移**代码里两类写法要在各自 W 批改（本批不改 `crates/widgets/**`）：

- **常量 key**（W2a 审查 F1，必修）：`market_trade` 的 `HEADER_KEY = "MarketTradeInstrument"` 与表格 key
  `"MarketTradeTable"` 是同一个 App 内多个 pane 共用的动作身份。改为 `cx.unique_key("market_trade.header.instrument")` /
  `cx.unique_key("market_trade.table")`，并把生成出的 `Action` 存进 `HeaderControl` / 表格字段供 release 用；两个 pane
  的回归测试按 §11「多 pane」写。
- **按 owner 节点手拼的 key**：`news` 的 `format_smolstr!("news.sound.{}", parent.to_bits())`（`view.rs:884`）在同一实例的
  header 行重建后会换身份（parent 变了）；它今天正确只因 widget 把 `Button` 句柄存进字段、release 用的是组件自己的 key。
  改用 `cx.unique_key("news.sound")`（同一实例同后缀恒同 key，与 parent 无关），测试的 `find_by_debug_prefix(.., "news.sound")`
  前缀仍然成立。`force_order` / 其它已迁移 crate 里的 kit key 同样改。

**验收口径**：已迁移 widget 里不再出现字面量 kit key / 字面量 `Action::Custom`（测试里直接构造的组件除外）；kit key 只从
`cx.unique_key` 来。

**selector 批的释放面（W2a 审查 F2）**：`InstrumentButton::build` 消费掉 `Button` 只返回 `NodeId`，widget 只能
`off_action`、不能 `Button::release`（组件信号作用域泄漏；W2a 实测 `live 18→28→38`）。selector 批需让 `build` 交还可释放
句柄（或等价的 `release(&mut App)`）；本批只把契约写成「优先保留组件句柄，`release` 同时摘 handler 与释放 scope」。

### 15.6 `kit::tooltip` 内容没有宽度上限（kit 后续批）

旧 `ui_kit::tooltip` 的内容是调用方自己给的容器（W3a 的旧实现是 `div().max_w(px(320.0)).text_xs()`），所以长句在窄 pane 里按
320 px 换行。`kit::tooltip` 的 `TooltipContent` 只带文本，面板样式没有宽度选项（`kit/tooltip.rs` 的面板只有 padding / 背景 /
圆角），长句按 pane 宽度换行，换行位置与旧实现不同。**登记 kit 后续批**：`TooltipOptions` 增 `max_width`（或让
`TooltipContent` 带布局选项）；在此之前 W 批只登记偏差，不自己给浮层再包一层容器（W3a 的 `info_history` / `info_yearly`
两处 note 即此情形）。

