# Widget 开发规范（P3 契约）

本文件适用于 `crates/widgets/**`，以 **`crates/widget`（P3-1 冻结契约）** 与 **`crates/base_ui/src/kit`（引擎化组件）** 为准。
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
6. **空快照不是空状态**：拿到空表 / 空列表 / 空图时，正文里要画 `base_ui::kit::empty_state` + `common.no_data`（表格用
   kit `Table` 的空态能力），不能留一块白。
7. **不自行创建上游连接、不 spawn 长期任务、不引入 DI / capability registry**。依赖以具体类型经构造函数注入 `service.rs`。
8. **持久化选项自己读回（`config()`）并上报（`config_change.publish(app)`）**；改了字段不上报，改动只活在内存里。
9. **应用设置只经注入的服务读**：语言 / 外观 / 数据时区 / 账户 / 目录 / 服务连接全部在 `WidgetServices` 里，不读进程全局、不
   自造 watch / 轮询。
10. **动画由 widget 自己请求帧、收敛即停，并尊重 `reduce_motion`**（见 §8）。

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
- 失败后不自动重连、不重试、不退避；错误由 widget 自己映射成强类型 `TranslatedError`。

---

## 5. `WidgetCx`：一个上下文，一次借一个

`WidgetCx<'a>` 是 widget 唯一拿得到的宿主视图：`&mut App`、`WindowId`、`&WidgetServices`、自己的 `SignalScope`、`FrameRouter`。
节点树在 `Window` 里、`Window` 在 `App` 里，因此上下文按需借出，**同一表达式里不能同时持有两个**：

```rust
cx.app() / cx.app_mut()              // &App / &mut App
cx.tree() / cx.tree_mut()            // &NodeTree / &mut NodeTree
cx.window() / cx.window_mut()        // WindowId / &mut Window
cx.services() / cx.translate() / cx.language() / cx.theme()
cx.config_ref(|config| ...)          // 不克隆地读设置快照（read 不登记依赖）
cx.account() / cx.data_time_zone()   // 由设置快照派生
cx.scope() / cx.router()
cx.signal(v) / cx.transient(v)       // 铸进 widget scope
cx.read(&s) / cx.write(&s, v) / cx.update(&s, f)
cx.request_frame() / cx.request_node_repaint(node) / cx.set_a11y_patch(node, patch)
cx.handle() / cx.spawn(future) / cx.runtime()
```

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
  `base_ui::kit::markdown` 画，widget 不需要（也不能）自己标记。
- `Notice::MarketNotLoaded { venues, supported }`：`venues` 是自己在等的市场（只是状态身份，句子不读它），`supported` 是句子
  列出来的、本组件能读的全部市场（从静态定义表取，与用户选了什么无关）。句子 `common.market_not_loaded` 由 `widget::venue`
  供给；它**是富文本**（对齐段落 + `<kbd>` 图标 + 一行一个市场名和 logo），surface 用 `base_ui::kit::markdown` 画，
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
- 尺寸 / 字号取 `base_ui::theme` 的令牌或 kit 组件的尺寸档，不写字面 px。

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
| `runtime: tokio::runtime::Handle` | stream / RPC 的运行时 | 不自己建 runtime |

**数据时区与账户**：画的那一帧读 `cx.data_time_zone()` / `cx.account()`（由设置快照派生，不缓存、不格式化后存快照）；
设置写入会沿 `config` 信号失效读过它的节点。

### 8.2 binding：消息化，不回调

`WidgetBinding<P>` 把 tokio 侧结果投进 `WidgetInbox<P>` 并在交付时请求帧；widget 在 `sync` 里 `binding.take()` 消费：

- `Inbound::{Reliable, Latest, Rpc}`；`Rpc` 带框架 `RequestId`，用于挡替换后的迟到响应。
- `call` **替换并 abort** 上一次调用；`cancel` / `shutdown` 显式收尾；`Drop` 只 abort 转发任务。
- 失败 / 取消后不再投递；widget 在 `sync` 里 drain 时先做 `is_failed` / `StreamKey` 守卫。
- `Ready` 水位由**协议自己**定义：拿到完整快照不等于业务成功，widget 自己维护 `pending_snapshot` / `ready_received`。
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
- **表格 / 列表的行闭包没有 `App`**：行内容里的动态值只能来自挂在节点上的 `bind_*`（读主题 / 设置信号）或行模型字段；
  需要 App 的结构性更新放到 widget 的 `sync`（`set_rows` / `set_items` / `push`）。
- **释放顺序**：`WidgetView::release` 里 release 自己建的 kit 组件（节点仍在，可解绑 / 停动画），**不要自己删 surface 节点**；
  框架随后 `remove_node_with_scope` 先删子树、再释放 scope。自己持有、但不在 surface 子树里的节点 / 作用域，必须在
  `release` 里显式清理（`cx.app_mut().remove_node_with_scope(..)` 或 `release_scope`）。
- 被隐藏的 spinner 不空转：占位切到正文时框架释放占位组件，widget 不需要操心；**widget 自己动画**时只在有可见位移时
  `request_frame`，收敛立刻停，`reduce_motion` 为真直接落终值。
- 调试选择器：框架占用 `WIDGET_SURFACE` / `WIDGET_SLOT` / `WIDGET_CONTENT` / `WIDGET_PLACEHOLDER` / `WIDGET_BANNER` 与
  `widget.{placeholder,banner}.action.*`；富文本 notice 的根是 `MARKDOWN`；kit / base_ui 组件有自己的选择器；widget 自媒体
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
  - 断言面：`visible` / `text_of` / `has_text` / `has_canvas` / `has_inline_boxes` / `node_count` / `find_by_debug_prefix` /
    `click_recovery` / `enable_a11y_capture` + `a11y_hidden`。
- 测试属性：纯逻辑 `#[test]`；涉及 tokio 服务 `#[tokio::test]`（dev-deps 开 `tokio` `test-util`）。界面测试一律经
  `WidgetTestApp`，不依赖任何窗口工具包。
- 外部 crate 用测试装置：dev-dependencies 里 `widget = { workspace = true, features = ["test-support"] }`
  （workspace 单写者统一登记）。
- 覆盖建议：状态上报（含去重）、有 / 无内容两种失败出口、迟到消息与 `RequestId` 挡迟到、恢复动作、取消与替换、
  空快照的「暂无数据」、容器的能力裁剪、配置写回与规则 2 回默认、语言切换后的重绘、a11y hidden。
- **会踩的坑**：
  1. 断言「写 → 出帧」用 `app.frame_requests()`（`App::write` 只在有节点读过该信号时才请求帧）；
  2. `sync` 里才变「有内容」的正文晚一帧出现——断言前 `drain()` 或让 widget `request_frame`；
  3. 占位 / banner 的文案由 surface 解析：断言看到的是**渲染后的文本**（不应含 `<p` / `<kbd>` / `![](`）；
  4. a11y 断言要先 `enable_a11y_capture()`，且首帧是全树；
  5. 节点 id 是框架的选择器约定，widget 不要自己起 `WIDGET_*` 名字。

---

## 12. 新 widget 检查清单

1. `WidgetId::new_static("kytrix.<名字>")`，`kytrix` 后一段、小写、点分段；持久化后不可改。
2. `Config` 实现 `WidgetConfig` + `SCHEMA_VERSION`（当前 `V1`）；每一项有 `Default`；没有可持久化选项也给空类型。
3. `DISPLAY_NAME` / `DESCRIPTION` key 同时加 `en.yaml` 与 `zh-CN.yaml`；`CATEGORY` 正确；`SELECTABLE` 只有框架自建组件为 `false`。
4. `model.rs` 纯逻辑 + 单测先行；`service.rs` 私有 Source trait + fake（正常 / 超时 / 失败 / 取消 / 替换）。
5. `view.rs`：`new` 不干活、`load` 只做前置检查 + 启动、`has_content` 便宜、`body` 只建一次、状态只经 `report`。
6. 每个改持久化字段的地方 `config_change.publish(cx.app_mut())`；规则 2 的校验在 `load` 里、订阅之前。
7. 消息入口全部挡 `StreamKey` / `is_failed` / `awaiting_market`；失败即终态。
8. 正文：`flex_1` + `min_*_0`，无自绘底色 / 圆角 / 裁剪；空快照画 `EmptyState` + `common.no_data`。
9. 语言文本在 `bind_text` 里读语言；颜色在 `bind_style` 里读主题；不把颜色 / 句子烤进挂载期常量。
10. `name-key` / 文案不自己拼；错误实现 `TranslatedError`。
11. release 释放自己建的 kit 组件与自定义作用域；不删 surface 节点。
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
# 单个 widget crate（迁移期；链接前置见下）
$env:KYTRIX_UI_CORE_MANIFEST='0'
cargo +nightly fmt -p <crate> -- --check
cargo +nightly clippy -p <crate> --all-targets -- -D warnings
cargo test -p <crate>
cargo check -p <crate> --all-targets

# 框架契约层自身
cargo +nightly fmt -p widget -- --check
cargo +nightly clippy -p widget --all-targets -- -D warnings
$env:KYTRIX_UI_CORE_MANIFEST='0'; cargo test -p widget
cargo check -p widget --all-targets
Select-String -Path crates\widget\src\*.rs -Pattern 'gpui'   # 必须 0
```

**`KYTRIX_UI_CORE_MANIFEST=0` 是链接期开关**：`ui_core/build.rs` 的进程清单与 `gpui` 自带的 `RT_MANIFEST` 冲突，不带时
Windows 上稳定 `CVT1100 duplicate resource` + `LNK1123`。P5 删除 gpui 依赖后移除该开关。它是**测试二进制的复跑前提**，
不是测试失败，也不是可以长期保留的开关。

未执行的在线 RPC、真实窗口与 release profile 验证必须在交付说明里明确写「未执行」；静态检查通过不等于性能已测量。

---

## 15. 契约缺口登记（P3-1d 实测，kit 后续批处理）

W1（`news` / `force_order`）实迁暴露的两个 **kit 侧**缺口由 P3-1d 登记。**归属是 kit 后续批，不是 widget 批**：
widget 侧不得各自重建组件库、另造绑定面或自实现容器。

1. **kit 静态文案组件没有绑定面**：`Badge` / `Button`（`label` / `icon`）/ `EmptyState`（`text`）的文案是构建期
   `SmolStr` / `IconKind`；`kit::table` 的 `Column::header` / `empty_text` 同理——它们不带 `bind_text` 闭包。AGENTS §7
   要求语言在 `bind_*` 里读，因此当前唯一通法是 widget 在 `sync` 里检测语言 / 选项 / 可用性变化 →
   `remove_node` + 组件 `release` + 重建自己建的那一小块（surface 对静态 notice 已有先例；W1 的 header 声音按钮与空态、
   `force_order` 的表格都这么做，并各有测试）。**登记 kit 后续批补 `bind_text` / `*_key` 形式的绑定面**；补上之前 W 批
   按「重建自己建的节点」实现，不把这段重建逻辑抽成 widget 间共享封装、不改 kit 组件语义。
2. **`kit::list` 无 separator、无像素 overscan**：旧 `DequeUnsizedList::separator` 与 `LIST_OVERDRAW = 150 px` 没有
   对等能力。当前通法：行内容里自建 `kit::separator`（`index > 0`）近似 separator；`ListOptions::variable(row_height)`
   + 默认 overscan **行数**近似像素缓冲（**已知差异**，W1 真窗口验收不据此判缺陷）。**登记 kit 后续批**：`list` 增加
   separator 钩子与像素级 overscan 估计；在此之前 widget 不自己实现虚拟列表 / 滚动容器，也不改 `kit::list` 的
   `ListOptions`。
