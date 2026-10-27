# Widget 开发规范

本文件适用于 `crates/widgets/**`。规则以当前代码实现为准：框架接口看 `crates/widget`，装配与展示规则看
`crates/widget/src/surface.rs`，参考实现看 `crates/widgets/force_orders`（表格与状态上报）、
`crates/widgets/order_book`（canvas 自绘、正文内动画、header 里的下拉控件）与
`crates/widgets/perpetual_statistics`（一个 crate 出多个 widget 的目录布局、轮询型 service、canvas 图表的
axes / palette / text / paint 分层）。
---

## 1. 框架 / Widget 职责划分

**框架拥有的**（widget 不得重写、不得绕过）：

| 关注点                                                  | 实现位置                                                         |
|---------------------------------------------------------|------------------------------------------------------------------|
| `WidgetState` 状态机、转移、去重、通知                  | `widget::status`                                                 |
| Banner / 占位的选择与绘制                               | `widget::surface`                                                |
| 正文槽位的底色 / 圆角 / 裁剪（`surface.sunken` + `rounded_sm`） | `widget::surface`                                                |
| 正文的缓存边界                                          | `widget::surface`                                                |
| 恢复控件的默认接线（`common.retry` → `load`）           | `widget_registry::registry`                                      |
| 持久化配置的编解码、写回时机、两条修复规则              | `app_config::widget` + `widget::config` + `kytrix::…::workspace` |
| Header 行的布局、拖动区、关闭/分割按钮                  | `kytrix::…::workspace::pane`                                     |
| pane 无人选择时的默认组件、选择列表的过滤               | `app_config::workspace::DEFAULT_WIDGET_ID` + descriptor `selectable` |
| 连接池、传输、重连、RPC / stream 生命周期               | `market_participants_connector` + `widget::{binding,runtime,stream,rpc}`     |

**Widget 拥有的**：协议、取数、数据模型、就绪态正文、header 控件、错误类型的本地化实现、子区域的局部状态、
**自己持久化的那部分配置**。

由此得到十条硬约束：

1. 不写 `ViewStatus` 一类的状态枚举，也不写 `render` 里的"该画 banner 还是占位"分支。状态只经
   `Entity<WidgetStatus>` 的 `report(WidgetState, cx)` 上报，位置由框架决定。
2. 正文（`Render`）只画"有内容"的样子。它只在 `has_content()` 为真时被嵌入。
3. 不给自己包 `.cached()`，不定正文的宽高。缓存样式是框架的
   `StyleRefinement::default().flex_1().min_w_0().min_h_0()`。
4. 正文不画自己的底色、圆角、裁剪：框架的正文槽位已经给了 `surface.sunken` 底色、`rounded_sm` 圆角和裁剪到圆角。
5. 失败与中性文案是两个出口：`WidgetState::failed` 与 `WidgetState::waiting`。不要用等待报错，也不要为"空数据"造一条
   文案 —— 见 §6。
6. **空快照不是空状态**：内容为空（空表、空列表、空图） **不是**要上报给框架的状态，但也 **不能什么都不画**——正文里要有
   `base_ui::empty_state::EmptyState` + `common.no_data` 的「暂无数据」。见 §6。
7. 被缓存区域里不放自由播放的动画；逐帧推进的状态必须由 view 自己请求帧、收敛即停，并尊重 reduce-motion（见 §7）。
8. 不自行创建上游连接、不自行 spawn 长期任务、不引入通用 DI / capability registry。依赖以具体类型经构造函数注入
   到 `service.rs`。
9. 持久化的选项由 widget 自己 **读回**（`config()`）并 **上报**（`config_change.publish(cx)`）；改了字段不上报，改动
   就只活在内存里。见 §4.5。
10. **应用设置**（语言、外观、数据时区、账户、代理、场所）只经 `ConfigManager` 的**按域事件**得知变化：不自造
   watch / 轮询，不把快照存进字段当"当时的配置"再定期比较，也不在回调里写配置。widget 能碰的只有时区与账户两域，
   而且两条接缝已经写好了（`widget::time_zone` / `widget::account`）。见 §4.6。

---

## 2. 目录布局

```text
crates/widgets/<widget_name>/
|-- Cargo.toml
`-- src/
    |-- lib.rs      # 只声明模块：mod model; mod service; pub mod view;
    |-- view.rs     # WidgetView + Render；UI 状态、交互、header、就绪态正文
    |-- model.rs    # 数据模型、合并/排序/裁剪等纯逻辑（含单测）
    `-- service.rs  # 协议、私有 Source trait、Service、生产实现与 fake
```

- `lib.rs` 只做模块声明，不写实现、不写测试。对外只 `pub` 注册要用的那个模块（单 widget 的 crate 是 `pub mod view`，
  一个 crate 出多个 widget 时是 `pub mod widget`，见下）：注册需要 `W::ID` 等常量，其余模块对外不可见。
- 协议数据类型与 `ForceOrdersSource` 这类接缝 trait 都写成 `pub(super)`（要跨同级子模块复用时写 `pub(crate)` / `pub`，
  例如 `etf` 的 `EtfSource` 与 `perpetual_statistics` / `volume_heatmap` 的 `service/stream.rs`）。`service` / `model`
  本身不是 `pub`，所以 crate 外的可见面只有注册要用的那些类型：单 widget 的 crate 是 `pub mod view` 里的 Widget，一个
  crate 出多个 widget 时还有 `pub mod widget` 的别名，以及别名写出来就必须可见的通用视图与 series 类型。
- 在职责独立且上述文件放不下时，才增加 `chart.rs`、`columns.rs` 等文件；不要把所有东西塞进 `view.rs`。
- 没有数据模型与取数工作的 widget（目前只有 `crates/widgets/empty`）只有 `lib.rs` 与 `view.rs`：不要为了对称而建空模块。
- **dialog 一律单独成文件 `dialog.rs`**：用 dialog 的 widget，其 dialog 的视图结构体与 `impl`、`Render`、它自己的临时态
  （输入、筛选、搜索答案、翻页/导航栈）、它拥有的子实体（列表、`NavStackState`、`InputState`）、它自己的
  header/footer/行渲染函数，以及它自己的测试，全部放 `src/dialog.rs`，`lib.rs` 加一行 `mod dialog;`。
  `view.rs` 只留 `WidgetView` 的实现（`load` / `has_content` / `header` / `config` / `instrument_link`）与状态上报——
  dialog 是职责独立的视图，这是上面那条规则的一个实例，不是例外。
  widget 与 dialog 之间 **只走弱引用**：widget 持 `WeakEntity<XxxDialog>`，dialog 持 `WeakEntity<XxxWidget>`；
  任何一侧持有对方的强引用都会让 dialog 关不掉、或让它的状态永不释放。参考实现：
  `x/src/dialog.rs`、`hyperliquid_wallet/src/dialog.rs`。
  一个 crate 出多个 widget 且各自有 dialog 时，放 `src/dialog/<widget>.rs`，`src/dialog/mod.rs` 只声明模块。
- 不使用 `#[path = "..."]`；按 Rust 标准模块布局。
- **一个 crate 出多个 widget 时**，把共用的那个视图放 `widget/display/pane.rs`（或同类共用文件），每个 widget 一个
  `widget/<name>.rs`，只放它的常量与 `pub type <Name>Widget = …`；`widget/mod.rs` 只声明模块。
- 行内模块（`mod x { … }`）被 workspace 的 `inline_modules = "forbid"` 挡下，`#[cfg(test)]` 的测试模块不在其列；
  `definition_in_module_root = "forbid"` 则挡下在 `mod.rs` 里写定义。测试因此照模块的形状放：只有一个文件的模块
  （`model.rs`、`view.rs`、`chart/paint.rs`）把 `#[cfg(test)] mod tests { … }` 写在同一文件里，仓库里既有 widget 都是
  那个形状；一旦模块是 **目录**（`model/`、`chart/axes/`），测试就放兄弟文件 `foo/bar.rs` + `foo/bar_tests.rs`，由
  **父模块** 写成 `#[cfg(test)] mod bar_tests;`（父模块是 `mod.rs`；声明不是定义，写在那里不触发
  `definition_in_module_root`）。
  多个模块共用的假实现按根 `AGENTS.md` §5.1 放 `foo/test_support.rs`，由 `foo/mod.rs` 写成
  `#[cfg(test)] pub(crate) mod test_support;`。
- 新增依赖必须先加在 workspace 根 `Cargo.toml`，子 crate 用 `{ workspace = true }` / `.workspace = true` 引用。 **同时在根
  `Cargo.toml` 的 `members` 中登记。**

---

## 3. 每帧到底发生了什么

理解这一节才知道性能该往哪里花：

```
每帧（由最快通知者驱动：状态 notify / 输入 / 动画帧）
Root（未缓存）→ Stage（未缓存）→ Pane（未缓存）
  |-- header 钩子：pane 调一次 widget.header(window, cx)    <- 每帧重建，永不被缓存
  |-- banner（失败且有内容时，几个 div，廉价元素）           <- 每帧重建
  `-- 正文槽位（框架画的底色 / 圆角 / 裁剪）                  <- 每帧重建
        `-- 正文 view（框架无条件 .cached）                   <- 命中：整棵子树跳过
              |-- 正文内部：widget 自己的结构
              `-- 没有内容时换成占位（同样在这个槽位里）
```

三条结论：

- **元素树的每帧重建不是成本问题**，昂贵或高变化的部分才需要拆分。所以不要为了"少建几个 div"做优化。
- **无关帧（别的 pane 在更新）里，正文命中缓存，闲置 pane 近似零成本。** 这是框架给的，不需要 widget 做什么。
- **被缓存的正文不会因为别人重绘而顺带刷新。** 不 `notify` 就是可见的冻结——这是 widget 唯一新增的失效义务，
  见 §5。

---

## 4. `WidgetView` 契约

```rust
pub trait WidgetView: Render + Sized + 'static {
    type Config: WidgetConfig;

    const ID: WidgetId;                        // 持久化标识，不可依赖类型名
    const DISPLAY_NAME: &'static str;          // i18n **key**，不是文案
    const CATEGORY: WidgetCategory;            // 在 Widget Gallery 的哪个分类下被列出
    const DESCRIPTION: &'static str = "";
    const ICON: Option<&'static str> = None;
    const SELECTABLE: bool = true;             // false = 框架自建、选择列表不提供（如 `kytrix.empty`）

    fn new(
        instance_id: WidgetInstanceId,
        config: Self::Config,
        status: Entity<WidgetStatus>,          // 写状态的唯一入口；框架与 surface 共享同一个实体
        reload: impl Fn(&mut App) + 'static,
        config_change: WidgetConfigChange,     // 改了 config() 的字段就 publish(cx)
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self;

    fn load(&mut self, cx: &mut Context<Self>);
    fn has_content(&self) -> bool;
    fn config(&self) -> Option<&Self::Config> { None }   // 持久化的选项；None = 不持久化
    fn header(&mut self, window: &mut Window, cx: &mut Context<Self>) -> WidgetHeader { WidgetHeader::empty() }
    fn instrument_link(&self) -> Option<InstrumentLink> { None }
}
```

创建顺序由 `widget_registry::registry` 固定：解码 config（ **永不失败**，读不出来就是默认值）→ 建
`Entity<WidgetStatus>`（初始 `Connecting`）→ `cx.new(W::new)`（其中 `reload` 与 `recovery` 都走同一个 `WeakEntity`）→
读 `instrument_link()` → 建 `Entity<WidgetSurface>`（正文擦除为 `AnyView` 后交给它）→ `load()` 一次 → 擦除为
`AnyWidget`。surface 在擦除之前建好，所以 widget 在 `load` 里报的状态当帧就有地方画。

因此：

- **`new` 里不要开始干活**，也不要报失败。框架在构造之后才调 `load`。
- `instance_id` 由框架分配；`config` 已解码并校验过 schema，直接用。 **不要把 `instance_id` 拼进任何元素 id**（§7）：
  现在没有 widget 需要它，构造函数里收成 `_instance_id` 即可。反正它也不能拿来当持久化标识。
- `recovery` 存进字段，每个失败路径都用它。框架在实例化这个 widget 时（`create_widget_view` 建 `cx.new(W::new)` 的同一次
  构造中、`W::new` 之前）就把它建好了，所以任何早期失败都已有出路。
- `reload` 是同一个能力（重跑 `load`），只是不带面向用户的标签。多数 widget 只留 `recovery` 就够；只有当"重新加载"与
  "失败恢复"需要区分处理时才另外保存 `reload`。两者都忽略了也不会编译报错，所以更容易被忘掉。
- `status` 是 widget 唯一的写入点：把它存进字段，用 `report(WidgetState, cx)` 上报（§5.1）。框架与 surface 持有同一个
  `Entity`，所以写进去的状态当帧就会出现在 pane 上。
- 想测试就必须有 `new_with_source<S: …Source>(…)` 这样的泛型接缝，让 `new` 只负责从全局取生产实现：

```rust
fn new(_instance_id: WidgetInstanceId, config: Self::Config, status: Entity<WidgetStatus>,
       _reload: impl Fn(&mut App) + 'static, config_change: WidgetConfigChange,
       recovery: WidgetAction, cx: &mut Context<Self>) -> Self {
    let source = cx.global::<market_participants_connector::connector::MarketParticipantsConnector>().market();
    Self::new_with_source(config, source, status, config_change, recovery, cx)
}
```

### `load`

首次加载与用户恢复共用同一个 `load`，所以它必须能分辨这两种情况。参考实现：

```rust
fn load(&mut self, cx: &mut Context<Self>) {
    let routes = needed_routes(cx);
    // 要读的那组 route 变了也是"换一条流"，同样保留读者正在看的行。
    let retry = self.status.is_failed(cx)
        || (!self.routes.is_empty() && self.routes.as_ref() != routes.as_ref());
    if !retry {
        self.clear_orders(cx);          // 显式重载：丢掉不再可信的旧数据
    }
    self.pending_snapshot = None;
    self.ready_received = false;
    self.awaiting_snapshot = true;
    self.routes = routes.clone();
    self.status.report(WidgetState::Connecting, cx);
    let request = if retry {
        ForceOrdersRequest::Retry { routes }
    } else {
        ForceOrdersRequest::Load { routes }
    };
    self.binding.call(request, cx, Self::apply_rpc_result);
}
```

两条语义：

- **重试保留用户正在看的内容**（`retry == true` 时不清快照），否则会闪掉一屏数据。
- **显式重载从头开始**，因为 widget 无法再为旧数据担保。

### `has_content`

便宜谓词， **不许构建元素**：框架是"问它"而不是"渲染完再看有没有"。`false` 时框架放占位，正文 view 根本不被嵌入。

`force_orders` 的答案是 `!self.routes.is_empty() && self.snapshot.is_some()` —— 拿到过快照就是有内容， **空快照也算**；一个
route 都读不到的 pane 没有正文可画（见 §8）。

### `header`

每帧一次、必须便宜、永远新鲜、永不缓存。默认实现是空 header。

- 倒计时、刷新按钮、状态徽标放这里；`loading` / `live` 这类静态状态不占 header。
- 只对真实控件调 `occlude()`，中间留白要留给 pane 的拖动区。
- **前置检查没过就没有可用的 header 控件**：本组件需要的市场参与者元数据还没进内存时（正文里站着 `MarketNotLoaded` 的那
  个状态），header 里的每一个控件都必须是 disabled 的——控件自己的答案只有两个来源：**从已加载场所里取选项的**控件按
  「有没有东西可选」判定（`InstrumentButton` 自己读 `filters.offers_anything()`，没有就不挂 click），**作用于当前 scope
  的**控件读组件自己的等待状态（`awaiting_market`，以及由它派生的 `has_scope()` / `refresh_is_disabled()` /
  `venue_picker_is_disabled()`），同一状态的另一面就是正文里那条「去加载市场」的句子。没有与市场参与者无关的例外：`kytrix.empty`
  这类没有 header 的组件不在此列，`etf` / `indices` / `wallet_monitor` / `x` 读的是账户而不是市场参与者，
  它们的禁用条件仍由各自的前置条件决定。
- **禁用要真的禁用**：`Button` 的 `disabled` 在本仓库里只**画**成禁用，挂上去的 handler 照样会跑（`Dropdown` / `Select`
  自己会挡住打开动作），所以禁用的 Button 要么不挂 click（`.when(!disabled, |button| button.on_click(..))`），要么在动作
  入口再挡一次（`watch_list` 的 `add`：选择器的确认可能在 pane 开始等待之后才到）。
- **不要在这个方法里再 `update` 自己**：它已经在 pane 的 `widget.update(...)` 内运行，同一个实体二次 `update` 会
  panic。要触发工作就 `cx.notify()`、`cx.spawn(...)` 或 `binding.call(...)`，这些在 `&mut Context<Self>` 上都是正当的。

### `instrument_link`

只有需要与其它 pane 联动标的的图表类 widget 才实现。`InstrumentLink` 的 `apply_remote` 返回 `false` 表示拒绝联动。

### 4.5 配置：写回与两条修复规则

**持久化哪些选项由 widget 决定，`Config` 的每一项都必须有 `Default`。**

- 有选项要持久化 → 实现 `config()`，直接返回字段（和 `has_content()` 一样必须是便宜谓词，不要计算）。
- 没有选项要持久化 → **不要**实现 `config()`（默认 `None`）。布局文件里那个 pane 的 `config` 就是 `null`，
  框架不会为它存任何东西。`Config` 类型仍然要保留，用来承载 `SCHEMA_VERSION`。

**写回是两步，缺一不可：**

```rust
// 1. config() 让框架随时读到"现在"的值
fn config(&self) -> Option<&Self::Config> { Some(&self.config) }

// 2. 每个改了 config() 字段的地方上报一次
self .config.timeframe = timeframe;
self .config_change.publish(cx);
```

框架 **不会**定期比较配置，也不会在 pane 快照时替你发现变化：`pane.snapshot()` 只问 `config()`，而它什么时候被调用
取决于有没有人告诉 stage"这个 pane 需要写回"。所以 **改了字段却不上报，改动就只活在内存里**，重启、切工作区、甚至拖
一次分隔条之后重新快照都会丢掉它。重复上报是安全的（内容没变时不会写盘），宁可多报。

`publish` 把报告交给 pane 建的那条 `WidgetConfigChange`，它经 `cx.defer` 落到 stage，所以从 `header()`、`Render`、
控件回调里调用都是安全的（那几处已经在一个 update 租约里，同步回调 stage 会 panic）。

**规则 1（读不出来就重建并回写，框架负责）**：`app_config::widget::VersionedWidgetConfig::decode` 永不失败。版本不
一致、`payload` 解析不了、类型不变量不满足，一律变成 `C::default()`；pane 比较存储的信封与 widget 现在报告的信封，
不同就记一条 `log::warn!` 并在启动时把工作区写回去。widget 不需要为此写任何代码，但要知道 **代价**：整份 config 回到
默认，不是只丢违规的那一项（超容量、重复项、route 与 instrument 不匹配都算读不出来）。

**规则 2（读得出来但不支持就重置该项，widget 负责）**：拿到的值可能在这一版程序里已经不成立了（市场参与者下架了标的、这个
route 不再发那个周期、`PRICE_MULTIPLIERS` 里没有存下来的倍率）。在把配置用于实际工作之前（也就是 `load` 里、订阅
之前）逐项校验一次，不支持的那一项回到 `Config::default()` 的对应值并上报：

```rust
fn resolve_scope(&mut self, cx: &mut Context<Self>) {
    let default = Self::Config::default();
    if !supported_timeframes(route).contains(&self.config.timeframe) {
        self.config.timeframe = default.timeframe;   // 回默认，不挑"就近"的那个
        log::warn!("…");
        self.config_change.publish(cx);
    }
}
```

校验要异步做（问 provider 这个标的还在不在）时， **订阅等它**：`self.status.report(WidgetState::Connecting, cx)` 同步
写、`cx.spawn` 发查询、答案回来后先比较"配置还是不是我当初问的那个"（不是就当作用户已经改过，直接丢弃，那次改动自己会
订阅），再发订阅请求。服务因此永远收不到一份还没修正的集合，代价是首帧晚一次网络往返——一个一直不回应的查询会让这个
pane 停在 connecting，不为它另加超时层。

四条边界：

- **只在"确定不支持"时重置。** 取数失败、未登录、没有账号、查询超时 → 保留原值不动（照常订阅），让订阅照旧报它自己的
  错，用户照旧有恢复按钮。
- **不挑选替代值。** 就近回退会让用户得到一个他没选过的值，看起来像他的决定。一律回默认。
- **重置必须上报**，否则每次启动都会再重置一次，用户看到的是"又变回去了"。
- 一个 widget 有多个查询（`watch_list` 按 route）时，答案 **凑齐后一次结算**：每个查询被问的都是同一个集合，逐次结算
  会让后面的答案看到已被前一个改过的集合。

纯逻辑（"哪些项不在支持集合里"）放 `model.rs` 并写单测；查询与状态迁移放 `view.rs`。

**版本**：所有 widget 的 `SCHEMA_VERSION` 都是 `V1`，由 `catalogue.rs` 的一条测试钉住。不做版本迁移、不写兼容层，
不兼容的变化直接删旧结构（根 `AGENTS.md` §1.1）。

### 4.6 应用设置的更新：按域事件，不是快照轮询

只管"设置变了我怎么知道"，不管 `config()` 写回（§4.5）。`ConfigManager`（`app_config_provider`，进程级 `Global`）持有当前
`Config` 快照与无状态的 `Entity<ConfigEvents>`；**一次 `write` / `update` 只算一份 diff**（`ConfigChangeSet`），再摊成按域
事件发出：

| 事件 | widget 怎么消费 |
|------|-----------------|
| `AppearanceChanged` / `LanguageChanged` | **什么都不做**：`base_ui` 换 theme、`kytrix::language` 写 `LanguageCode`，各自 `refresh_windows()`（§5.1） |
| `DataTimeZoneChanged` | `widget::time_zone`，见下 |
| `AccountChanged` | `widget::account::account_of` + 自己的 `reconcile_account`（账户与头像合并成一条） |
| `ProxyCatalogChanged` / `DefaultRouteChanged` / `ServiceRouteChanged` / `VenueChanged` | **不属于 widget**：只由进程 owner 经 `ConfigEventBridge` 消费 |

1. **只有时区与账户两域由 widget 直接订阅**，两者都已写成共享接缝，不要自己 `cx.subscribe(events, …)` 重造：
   - 时区：**画的那一帧读** `data_time_zone(cx)`（返回 `UtcOffset`，没有 manager 时是 UTC）。要跟随时用
     `on_data_time_zone_change(cx, |this, offset, cx| …)` 或 `notify_on_data_time_zone_change(cx)`，都返回
     `Option<Subscription>`（没有 manager 就是 `None`，离线 runtime 与单测就长这样）。**不缓存时区**，也不在数据到达时
     格式化时间——那会把一个时钟冻进快照。
   - 账户：`account_of(&cx.global::<ConfigManager>().read())` 是 scope，事件到了就重算并 `reconcile_account`；账户没变
     （只换了头像）必须原样返回，那一条事件是两者合并发的。
2. **订阅就是连接，存进字段。** `Subscription` 非 `Send`/`Sync`、`#[must_use]`，**丢掉就是取消**：写
   `_account: Subscription` / `_time_zone: Option<Subscription>`。`.detach()` 只给没有终点的跟随（`VenuesUpdated`）。
3. **`refresh_windows` 只管语言与外观。** 时区与账户事件只 notify 被通知到的实体，而正文是 `.cached()` 的，
   **父层 notify 不会重建被缓存的子实体**：有子实体在画时间戳或账户内容的，回调里要把它一起 notify
   （`this.list.update(cx, |_, cx| cx.notify())`；`hyperliquid_wallet` 连 `NavStack` 每一页）。
   只 notify 自己是"文字变了但屏幕没变"的常见成因。
4. **回调只读。** 在回调里同步写配置会重入 emitter；要回写就 `cx.defer`。widget 本来也没有写 `Config` 的入口
   （`write` / `update` 要 `&mut App`，`cx.global_mut::<ConfigManager>()` 编译不过），唯一该写的是自己的
   `WidgetConfig`（§4.5）。

**不碰的**：`watch` / `network_subscribe` / `cx.observe_global` / `while rx.changed()` 轮询都已被这条事件路径取代。
`ConfigNotification<P>` 是 owner 与它**后台消费者**之间的单消费者通道（一个 `wait()`），widget 不创建也不 `wait()`；
要按配置的路由发请求就读 connector 的共享投影 `current()`（`x` 的 `SharedTransport` 即此）。

**测试**：`ConfigManager::in_memory(config, cx)` + `cx.set_global`，再用
`cx.update_global::<ConfigManager, _>(|manager, cx| manager.update(|config| …, cx))` 驱动**真实事件路径**再断言 pane 的反应；
直接调 `pane.reconcile_account(..)` 只证明分支在，不证明订阅接上了，两件事各写一条。

---

## 5. 状态与失效：widget 的两大义务

### 5.1 状态写入（`Entity<WidgetStatus>`）

状态是 `WidgetState` 的五个互斥取值，写入只有一个入口。`report` / `is_busy` / `is_failed` / `state` 定义在
`widget::status::WidgetStatusExt` 上、为 `Entity<WidgetStatus>` 实现，所以用到它们的文件要
`use widget::status::{WidgetState, WidgetStatus, WidgetStatusExt};`——漏了就是 E0599。

| 写侧                                             | 语义                       | 清掉的东西     |
|--------------------------------------------------|----------------------------|----------------|
| `report(WidgetState::Connecting, cx)`            | 正在建立连接               | 旧文案、旧失败 |
| `report(WidgetState::Loading, cx)`               | 已在连接上取数/刷新        | 旧文案、旧失败 |
| `report(WidgetState::Ready, cx)`                 | 工作完成，无可报告         | 旧文案、旧失败 |
| `report(WidgetState::waiting(notice), cx)`       | 没有内容时，用目录引用占位 | 旧失败         |
| `report(WidgetState::failed(error, action), cx)` | 失败 + 恢复控件            | 旧文案         |

`WidgetState::waiting` 收的是 `Notice`：`Notice::message("widget.x.y")` 交一条没有占位符的目录文案（key 是字面量），
`Notice::MarketNotLoaded { venues, supported }` 交两份场所——`venues` 是自己等的那几个（只有状态的身份，句子不读它），
`supported` 是这条句子列出来的、本组件能读的全部场所（见 §9）。句子与它的两个参数由 `widget::venue` 供给。
`error` 是 `Arc<dyn LocalizedError>`，`action` 是 `WidgetAction`（标签是目录 key）；`WidgetState::waiting` 与
`WidgetState::failed` 就是 `Waiting` / `Failed` 的构造器，直接写变体也一样。

每次写入都是 **整体替换**：sum type 保证旧的失败与旧的文案不可能残留。读侧只有 `state` / `is_busy` / `is_failed`。

四条必须记住的行为：

- **重复写入不通知。** `WidgetStatus::set` 先比较 `WidgetState` 的 `PartialEq`，相等就直接返回：不改、不通知。
  `Waiting` 的相等含 `Notice` 的内容，`Failed` 的相等是"同一个错误句柄 + 同一个动作"。widget 因此可以在每条消息上
  无条件上报，不必自己判断"是否变化"。这不是优化而是硬需求：一个 10Hz 的流不这样做会让整窗每秒重绘 10 次。
- **可以在 `Render` 里写状态。** 比较与写入都在 `Entity::update` 的同一次租约内完成；`notify` 只是把这个实体的 id
  交给 `App`，不会重入状态本身。
- **失败与等待的文案都不在写入时解析。** `Failed` 存的是类型化错误（`Arc<dyn LocalizedError>`），`Waiting` 存的是
  目录引用（`Notice`）；两者都由 surface 在 **画它的那一帧** 解析（§6 / §9）。
- **语言切换不需要 widget 做任何事，框架也不注册观察者。** 应用层的契约是
  `cx.set_global(LanguageCode)` + `cx.refresh_windows()`：那一帧整窗所有 `.cached()` 子树重建，surface 按新语言重新
  解析等待文案、失败文案与控件标签。widget 只负责报告状态，句子由画它的那一帧承接。

### 5.2 迟到的消息要自己挡

失败后框架 **不会**自动重试，也不会拦住迟到的 stream / RPC 结果。每个消息入口都要自己守卫：

```rust
fn apply_reliable(&mut self, key: ForceOrdersStreamKey, message: Arc<… >, context: &mut Context<Self>) {
    if key != ForceOrdersStreamKey::AllMarkets || self.status.is_failed(context) {
        return;
    }
    …
}
```

- `key` 校验是"这条消息还属于我当前作用域吗"，`is_failed` 是"我还处于终态吗"，两者都不能省。
- 失败即终态；只有用户触发恢复才重新开始（`load`）。不写自动重连、不写退避。

### 5.3 正文的唯一失效义务

正文被缓存，所以 **内容变了必须 notify**：

- 变化发生在 widget 自己身上 → `cx.notify()`（写状态时框架已经 `notify` 了状态 cell，pane 读它会重画；但如果只是
  绘制内容变了而状态没变，要自己 notify）。
- 变化发生在持有内容的子实体上 → notify 那个子实体，它会沿祖先链把正文标脏。
  `force_orders` 的 `sync_table` 就是这条：只在"被接受的快照发生变化"的那两处边界更新并 `notify` 表格实体。
- 表格 / 列表的滚动位置按 `ElementId` 存在 GPUI 的 state 里， **每帧必须用同一个 id**，否则滚动位置每帧归零。

---

## 6. 展示规则（框架唯一实现，widget 只需知道后果）

| `has_content()` | widget 状态                                              | 结果                                             |
|-----------------|----------------------------------------------------------|--------------------------------------------------|
| true            | `Failed`                                                 | **Banner**（失败文案 + 恢复控件）压在正文上方    |
| true            | 未失败（`Connecting` / `Loading` / `Ready` / `Waiting`） | 只有正文                                         |
| false           | `Failed`                                                 | 失败占位 + 恢复控件                              |
| false           | `Waiting(Notice)`                                        | 信息占位（由 markdown 画出，无 spinner、无动作） |
| false           | `Connecting` / `Loading`                                 | Spinner + `common.connecting` / `common.loading` |
| false           | `Ready`                                                  | Spinner + `common.loading`（兜底）               |

- **Banner 只属于失败**，而且需要有内容可压。它只在失败且有内容时才构造，不失败时根本不在元素树里、不占那一行。中性文案
  永远不会变成 banner。
- **占位只在没有内容时出现**，所以任何状态上报都不会抢走自己内容的空间。
- Banner 与占位互斥：同一条失败屏幕上只出现一次（旧代码里"正文占位 + banner"两份重复文案的缺陷因此消失）。
- 两个出口显示的是 **同一句话**：都由 `Failure::text(cx)` 直接渲染，谁都不加前缀、不加包装、不改写。所以同一个失败
  在"有内容"和"没有内容"时读起来完全一致，文案的完整表达由错误类型自己的 i18n key 负责（见 §9）。
- Banner 不固定高度，由文案与控件撑起；banner 里的按钮用 `UnifiedSize::Small`，占位里的按钮用默认尺寸——两处各自
  就地构造 `Button`，不共享包装。
- 恢复控件在 `action.enabled == false` 时不挂 click handler，两处一致。

**给 widget 的推论**：不要为"没有内容"造 `WidgetState::waiting` 文案——连接成功就显示内容。但 **空快照必须自己说它是
空的**：`has_content()` 为真、正文画出来却一片空白时，用户分不清"没有数据"和"出错了"，所以每个能拿到空快照的 widget
都要在正文里画「暂无数据」：

- 表格 / 列表：实现 `TableDataSet::empty`，返回
  `EmptyState::new("<元素名>Empty", t!(cx, "common.no_data"))`。 **列头保留**，空态画在表体里。
- 其它正文（列表、canvas 图、阶梯）：用 `EmptyState` 覆盖在绘图区上或居中放在正文里，`canvas` 不必移除。
- 文案一律用共用的 `common.no_data`，不要每个 widget 自己造 key。
- `WidgetState::waiting` 仍然只用于"没有 scope"这类 **中性引导**（"请选择标的"），它和空快照是两件事：前者是"还没开始
  看"，后者是"看过了，是空的"。
- 反例仍然是反例：`force_orders` 当年那个"等待强平订单" `waiting` 占位不该回来——本条要的是正文里的一块「暂无数据」，
  不是一条上报给框架的中性状态。`etf` 的空窗口在合法路径上不可达（解析层就把它判成 `EmptyAssetMap` 失败），所以它
  不需要空态。

---

## 7. 缓存与绘制约束

- **被缓存区域里不能有自由播放的动画。** 缓存命中时不会重新 render，所以 `with_animation(...).repeat()` 这类
  自走的动画会冻住。确实需要逐帧推进的状态（`order_book` 的深度条缓动、`indices` 单元格的报价闪烁）必须由 view 自己请求帧：
  在 `Render` 里推进状态后调 `window.request_animation_frame()`（在 view 内即"下一帧 notify 自己"，从而打破
  正文缓存），并且只在仍有可见位移时请求、位移收敛后立刻停止；同时尊重 `App::reduce_motion()`，为真时直接落到终值。
  Spinner 只由框架放在 **不缓存**的占位上，widget 不用操心。
- **正文永远是 `flex_1`**：不固定宽高、不 auto-size、不依赖内容测量。缓存命中时内容根本不被测量，所以盒子必须只从
  布局推得出来。用 `div().size_full().min_h_0()` 起步。
- **正文不画自己的底色 / 圆角 / `overflow_hidden`**：框架的正文槽位已经给了 `surface.sunken` 底色、`rounded_sm` 圆角，并裁到
  圆角里。正文自己再画一遍只是重复，而且内容画到边缘时会盖掉圆角。只有正文内部的子区域（表格、列表、canvas 的视口）
  才在自己的层级上决定底色与裁剪。
- **滚动 / 交互状态的 id 要稳定，且只带"兄弟之间"的区分符。** GPUI 的 `GlobalElementId` 是 **祖先中设过 id 的元素的路径 +
  自己**，
  而正文是框架 `.cached()` 出来的 `ViewElement`（`ElementId::View(正文实体 id)`）、header 挂在 `(pane_id, "PaneHeader")`
  之下：
  跨 pane、跨实例的区分已经由路径给出，widget 内部的 id 只需要解决 **同一个作用域下兄弟重名**。所以：
    - 作用域里只出现一次的元素 → `ElementId::Name(SharedString::new_static("Xxx"))`；
    - 同一作用域下有多个兄弟共用同一个名字时（表格 / 列表的行、网格的单元格、标签页、逐项的控件）才带区分符 →
      `ElementId::NamedInteger(SharedString::new_static("XxxRow"), index as u64)` 或 `(ElementId::Name(…), key)`；
    - **不要把 `WidgetInstanceId`（或由它异或 / 哈希出来的整数）拼进 id**：那是手工伪造的"全局唯一"，路径已经保证了
      跨 pane / 跨实例互不相干，拼进去只会让 id 与它实际的作用域不符。同理，单元格内的元素不必再拼 `(row, column)` ——
      承载它的单元格元素自己已经有 id。
    - id 一变，挂在它上面的滚动位置 / hover / 命中盒就换了一个元素，所以"稳定"仍然是第一位：只去掉多余的那部分。
- **不要自己给自己加 cache 构件。** 想缓存更小的区域，就把它做成独立实体；但先确认它真的昂贵——正文本身已经
  是缓存单元。
- 调试选择器（框架与 base_ui 组件已占用，widget 用 `TEST_*` 之类的自有名字）：
  `WIDGET_SURFACE` / `WIDGET_BANNER` / `WIDGET_BANNER_MESSAGE` / `WIDGET_BANNER_ACTION` /
  `WIDGET_PLACEHOLDER` / `WIDGET_PLACEHOLDER_MESSAGE` / `WIDGET_PLACEHOLDER_SPINNER` /
  `WIDGET_PLACEHOLDER_ACTION` / `PANE_HEADER` / `PANE_BODY`。
  中性占位文案由 `base_ui::markdown` 画，选择器是 `BASE_UI_MARKDOWN` / `BASE_UI_MARKDOWN_BLOCK_{n}`
  （`{n}` 数的是画出来的块；源码里的空行另有 `BASE_UI_MARKDOWN_BLANK_{n}`，不占块的下标）。
- 空态的调试选择器是 `base_ui::empty_state::EmptyState` 自带的 `EMPTY_STATE`（仓库里没有 `WIDGET_NO_DATA` 这个
  选择器，断言就写 `EMPTY_STATE`）：它由 widget 的正文画出来。 **不要**为了让选择器好看而自己重写一遍居中 div——
  文案共用 `common.no_data`，盒子共用 `EmptyState`。

---

## 8. 服务层、协议与生命周期

**数据通道分工**：可靠流传不能丢的状态与错误，最新流传可覆盖的快照。

- `WidgetProtocol::StreamKey` 必须包含所有会使旧数据失效的业务维度；handler 里再校验一次当前 key。
- 不假设两条流之间存在全局顺序：需要原子一致的字段放在同一条消息里。`force_orders` 的做法是 **Ready 严格晚于首个
  快照**，widget 用 `pending_snapshot` + `ready_received` 把"快照到了但还没 Ready"和"Ready 了但快照还没到"两种情况
  都接住。
- **Ready 水位自己管**。框架不提供这套机制：拿一个完整快照不等于业务成功，必须等到协议定义的边界。
- 容量上限（`MAX_FORCE_ORDERS`）在 model 与 service 两处 `clamp`，不靠调用方自觉。
- **终态失败不自重试**：发完 `Error` 状态后任务挂在 `publisher.cancelled().await` 上，等用户点恢复时由
  `replace_stream` 换掉。不要在 service 里写重连或退避。
- 每个 await / 背压等待都要能看到取消；长任务由 `ServiceContext` 的流监督器持有，不 detach、不无界。
- `WidgetBinding::call` 会取消上一个未完成的 UI 调用；需要拒绝被替换后的迟到响应时用 `call_with_id` 拿 `RequestId`。

**依赖注入的具体形状**（照抄参考实现）：

```rust
pub(super) trait ForceOrdersSource: Clone + Send + Sync + 'static {
    type Subscription: ForceOrderEventSubscription;

    fn perpetual_listings(&self, route: &VenueRoute)
                          -> impl Future<Output=Result<Vec<VenueListingKey>, MarketError>> + Send;
    fn subscribe_force_orders(&self, route: VenueRoute)
                              -> impl Future<Output=Result<Self::Subscription, MarketError>> + Send;
}

impl ForceOrdersSource for MarketCapability { … }          // 生产实现，同文件
pub(super) struct ForceOrdersService<S = MarketCapability> {
    source: S,
    …
}
```

- 用 RPITIT（`-> impl Future + Send`）而不是 `async fn`，两个 impl 都写 `async fn` 即可满足。
- 生产实现与测试 fake 实现同一个私有 trait；fake 放在 `#[cfg(test)] mod tests` 里，不进生产代码。
- 不引入 dyn 装箱、不做通用 capability registry。

**动态 scope 与目录：发现属于加载动作**：场所自己部署的分区（Hyperliquid 的 builder-deployed DEX）与每个 scope 的
目录，只在用户于标题栏「市场」菜单里点「加载 / 刷新」时由加载动作取回内存；`MarketParticipantsConnector::load_venue`
是唯一会联网并写内存的入口。因此 widget 侧没有发现接缝，也没有"读不到就顺手加载"：`WidgetView::load()` **只检查前置
条件，不发起任何加载**。

检查的是一次判断：自己需要的 route 是否在 `LoadedVenues` 里（`MarketParticipantsConnector::venues()`）。在场即全量就绪——一个场所
出现在那里，就意味着它的全部 scope 的目录与元数据都已提交，不存在"已注册但目录没到"的中间态，也没有第二张声明表可问；
不在场就 `self.status.report(WidgetState::waiting(Notice::MarketNotLoaded { venues, supported }), cx)` 报一句中性文案：
`venues` 是自己等的那几个场所，`supported` 是句子列出来的那组——**本组件所需 capability 的场所集合**，从静态定义表取
（`market_participants_connector::venue::definition::venues_supporting(CapabilityId::…)`），与加载状态、与用户当前选了什么标的都无关。
句子与它的两个实参由 `widget::venue` 在画它的那一帧供给（见 §9，指向左上角那个带图标的「市场」入口），然后立即返回。

- 检查放在 `load()` 的 **最前面**，早于任何订阅与构建。scope 被剪除（场所再次加载后收缩）时 pane 因此回到中性提示，而不是
  走进既有的"能力不支持"分支——那条分支会清掉用户持久化的选择。
- **这个前置条件也管住"还没选标的"的 pane**：instrument 级 pane 的选择器画的是已加载的场所，一个场所都没加载时那个选择器
  是空的，所以"没有标可选"与"标的的市场没加载"是同一件事。`market_ready()` 因此先问"本组件所需 capability 里有没有一个
  场所已加载"（`venues_supporting` + `LoadedVenues::loaded`），再问选中标的的 scope 在不在——没有选中、但一个场所都没加载
  时返回 **假**，pane 报的是加载提示而不是 `widget.<名字>.select_instrument_prompt`；选中了而它的 scope 不在时照旧返回假；
  都没有问题时才轮到"请选择标的"那条（`watch_list` 的"一条都没添加"同理，它连选都没有）。
  同理，这类 pane 的 `on_venues_updated` 不能只问"自己要读的那组 market 变了没有"：它读的是**空集**，第一个场所加载时那个集合
  不会变，只比集合永远走不出提示——所以 `awaiting_market && market_ready(cx)` 时就重跑 `load()`。
- `WidgetState::waiting` 是中性引导（§5.1 / §6）：它的占位 **没有动作按钮**，所以提示只能是一句话（和一个指向控件的
  图标，见 §9）；"未加载"是等待而不是失败，不上报 `WidgetState::failed`。
- widget 不加载场所、不触发发现、不重试、不退避、不轮询、不做后台预热（根 `AGENTS.md` §1.2）。
- 需要"用户加载之后自动恢复"时，订阅 `LoadedVenues::events()` 的 `VenuesUpdated { venue, revision }`（事件实体由
  `MarketParticipantsConnector::install` 建一次），订阅随实体建立一次，不轮询。处理器叫 `on_venues_updated`，它问的是两件事：
  这次提交有没有让 **"自己要读的东西在不在内存里"** 这个答案变掉，以及这次提交有没有落在 **自己正在读的那个场所** 上：
    - 答案变得能读了（pane 正等着，`market_ready(cx)` 现在为真）→ 重跑 `load()`。等待中的 pane 等的是前置条件，
      场所加载后事件到达就重新开始（"加载后已打开的 pane 自动恢复"就是这样实现的）；读整个市场的 pane（`force_orders`、
      `volume_heatmap`）也靠这一条才能把后加载的场所读进来。
    - 答案变得读不了了（pane 正在读，`market_ready(cx)` 为假）→ `wait_for_market(cx)`：pane 读的那个 scope 被一次收缩提交剪除了。
    - **提交落在 pane 正在读的那个场所上**（用户刷新或重新加载了这个场所）→ pane **从头开始**：清掉手里那批它无法再担保的内容，
      并发出 **替换流的那个请求**。原因是流本身：每条流在打开时就把它要读的东西解析好并一直拿着（listing 的 rules / precision /
      units，以及它当时的成员与终态判断），而这次提交换掉的正是它当初解析的那一代目录；只有替换请求会让服务丢掉这条流、按现在的
      那一代重新解析（各协议的 `Retry`；`watch_list` 是 `MarketReloaded`，`hyperliquid_wallet` 是 `Reload`）。所以每个这样的 pane
      多一个布尔字段（`market_reloaded`），由 `on_venues_updated` 在判据成立时置上、由 `load()` 在 **前置检查之后** 取用——被打回等待时它要留着，市场回来后的那次加载
      同样必须重开流（那条流仍是在被换掉的那一代上解析的）。判据是"这个场所上有没有我正在读的东西"：选定 scope 的 pane 看
      `config.selection` 的 route 场所，`watch_list` 看 `config.entries`，`force_orders` 看自己在读的 `routes`，
      `hyperliquid_wallet` 看自己在读的 `markets`；`perpetual_statistics` 是例外，它的 `Load` 本身就走
      `replace_stream`，所以它只需要重跑 `load()`，不记这个字段。**两个请求状态不能混用**：`retry` 决定"保留用户正在看的内容"，
      替换请求决定"重开流"，重新加载是"清内容 + 替换请求"（`crates/widgets/*/src/view.rs` 里那些 `start` / `start_current` 的
      两个参数就是这两件事）。替换请求自己带着它要跟的那组 key：`watch_list` 的 `MarketReloaded` 与
      `hyperliquid_wallet` 的 `Reload` 都像 `Replace` / `Load` 一样按这份集合跟随（退掉离开的、跟上新出现的），
      并把其中每一条流重开一遍——取代掉一个还在飞的请求因此是无害的，不需要把重新加载推迟到那个回答到达之后。
      轮询型、每次取数都重读当前目录代的 pane（`volume_heatmap`）不在此列：它没有"打开时解析一次"的流。
    - 两个答案都没变（别的场所提交，而 pane 读的 scope 不在其中）→ 只重查前置条件， **不重跑 `load()`、不重启流**：
      否则每一次提交都会把用户正在看的内容清掉重来。所以 **选择器画什么不能取自那一次 `load()`**：凡是从已加载场所里取选项的
      pane，都必须在 **构建控件的那一刻** 读一次：`LoadedScope::all(cx.global::<MarketParticipantsConnector>().venues())`
      ——本文件里五个这样的 pane（`candlestick` / `watch_list` / `market_trades` / `order_book` /
      `perpetual_statistics`）现在都这样读——不把这组 scope 存进字段——存下来的那份只在 `load()` 时更新，
      而用户后来加载的场所按上面这条规则不会再触发 `load()`，那些场所于是既不出现在选择器的场所列里，它的目录也不出现在
      选择器读的目录里。
    读 **自己选定的那几个** scope 的 pane（`candlestick` / `watch_list` / `market_trades` / `order_book` /
    `perpetual_statistics`）就用它自己维护着的这两件事判断：`awaiting_market`（因为缺一个市场而没在读）与
    `market_ready(cx)`（现在要读的都在内存里）合起来就是"我要读的那组 market"，它们跟着选择一起变，所以不会过期。
    读的是"已加载场所里自己认下的那一部分"的 pane（`force_orders` 读所有已加载的强平 route，`volume_heatmap` 读它选中
    场所的那组 route，`hyperliquid_wallet` 读 Hyperliquid 的全部已加载 scope）判断只能靠它们已经在读的那组
    `routes`（`hyperliquid_wallet` 的那个字段叫 `markets`，`volume_heatmap` 记的是选中的场所连同它的 route）：
    与现在要读的不同就是集合变了；集合没变时还有上面那条"提交落在自己读的场所上"的规则，所以这个比较要在它之后问。
    - **`WidgetState::failed` 是终态（§5.2）**：已经失败的 pane 不因 `VenuesUpdated` 复活，事件到达时先
      `status.is_failed(cx)` 守卫、直接返回，只有用户触发恢复才重新开始 —— 这个守卫与"要不要重跑"的判断都在
      **事件处理器 `on_venues_updated`** 里，不在 `load()` 里；`load()` 只负责前置检查与开始工作，所以框架的恢复控件
      （它直接调 `load()`）永远不会被这段判断挡住。
- 等待中的 pane 仍然活着的流会迟到：`wait_for_market` 清空内存里的内容，但不清空流，所以 **每个消息入口除了 `StreamKey`
  与 `is_failed` 之外还要挡"我正在等市场"**（各 pane 的 `awaiting_market`）。否则一条在途快照会把 `has_content()` 变回
  真，框架于是画正文而不是那条提示。
- "没有所需 route 就画不出正文"的两个 pane（`force_orders`、`volume_heatmap`）必须同时让 `has_content()` 在所需 route
  缺失时返回 `false`：`has_content()` 为假时框架根本不调用正文 `Render`，提示只能由 `WidgetState::waiting` 的占位画出来。

---

## 9. 数据、i18n 与错误

**数据结构**

- 短字符串用 `SmolStr` / `SharedString`，不用 `String`。拼接用 `format_smolstr!`，不要 `format!` 再转。
  静态字符串用 `SmolStr::new_static` / `SharedString::new_static`。
- 集合用 `ahash::{HashMap, HashSet}`，并按已知规模给初始容量。
- 展示文本在 **失效边界**生成一次并缓存：`ForceOrderRow` 用 `OnceLock<ForceOrderDisplay>` 惰性初始化，行入表时不做
  任何格式化；快照用 `Arc<[Arc<ForceOrderRow>]>` 共享，`Clone` 只是加计数。
- 纯逻辑（合并、排序、裁剪、格式化、max 项淘汰）全部放 `model.rs`，并在同文件写单测。

**i18n**

- 用户可见的文案一律 `t!(cx, "…")`；`DISPLAY_NAME` 是 **key**（如 `widget.names.force_orders`）。
- **新增 key 必须同时加到 `crates/i18n/locales/en.yaml` 与 `zh-CN.yaml`。** 缺失的 key 翻译成空串——界面上一块空白，
  不是崩溃，所以很容易漏掉。
- `common.*` 是 **跨 widget 共用**的：`connecting` / `loading` / `retry` 由框架在 surface 里解析，`no_data` 由 widget
  的空态用（见 §6）。widget 自己的文案仍然放自己的 `widget.<名字>.*`。
- **等待文案是一个 `Notice`——指向目录的引用，不是句子。** 调用点只给 key 与参数，句子由框架在 **画它的那一帧** 用
  那一帧的语言解析（§5.1 / §6）：
  - `Notice::message("widget.x.y")`：一条没有占位符的目录文案，key 是字面量。
  - `Notice::MarketNotLoaded { venues, supported }`：交两份场所，都是 `VenueId`。`venues` 是自己等的那几个（只有状态
    的身份：重复上报要能去重，句子不读它）；`supported` 是句子要列出来的那组——本组件所需 capability 的场所集合，从静态
    定义表取，与加载状态和用户选了什么标的都无关。句子是 `common.market_not_loaded`，它的两个实参由 `widget::venue`
    供给——`%{markets}` 把**每个场所写成一行**，一行是它的 logo（`VenueDisplay::asset_path` 那条 `brands/` 路径，写成
    `?size=16` 的行内图片）加它自己的名字（markdown 加粗），名字来自 `VenueDisplay::name_key`；
    `%{control}` 是「市场」控件自己的标签 key（`window.market_participants.title`）。"这条句子指向哪个控件"因此只落在一
    处，不跟着调用点复制。
    logo 的路径只有一处：`base_ui::brand` 里每个品牌一个常量，`Brand` 元素与这条句子读的是同一个，所以列表里的 logo
    不会和界面别处画的那张分叉。
  目录里那条文案自己写成 markdown：`<p align="center">…` 里先写句子（以冒号收尾），换一行再放 `%{markets}`，最后在
  同一行收 `</p>`；`<kbd>![](file://icons/phosphoricons/graph.svg) %{control}</kbd>` 把图标与控件名圈成一个不可拆开的
  整体，换行时一起走。
  **整对标签之间可以有空行**（`%{markets}` 的值里也一样）：空行照旧是空行，标签对跨过它闭合，整段文案与列表都画出来
  （空行那一条由 `base_ui` 的 `a_blank_line_in_a_tag_pairs_content_keeps_the_whole_message` 钉住；整句照旧画成一个居中
  段落由 `every_placeholder_message_should_draw_as_an_aligned_paragraph` 按参数填好之后再验）。
  widget 不自己拼文本、不自己插值，框架把中性文案交给 `base_ui::markdown` 画。
- **等待文案由 surface 交给 `base_ui::markdown`（`Markdown` / `MarkdownState`）画**，`MarkdownState` 只在原文变化时
  重新解析；带参数的 `Notice::MarketNotLoaded` 还多一步 memo——notice 与语言都没变就不再解析。正文里的 markdown 由
  widget 自己持有文档：`kytrix.empty` 的提示就是这样一条——`<kbd>![](file://icons/phosphoricons/puzzle-piece.svg)</kbd>`
  圈出右上角那个控件的图标；原文存进实体，语言变了再 `set_source`（参照 `crates/widgets/empty/src/view.rs`）。

**错误**

- 面向用户的错误必须是强类型并实现 `widget::error::LocalizedError`。
- **严禁**用 `Display` / `to_string()` / 字面量拼出用户可见的错误文本。
- 常见的错误类型（`MarketError`、`MarketParticipantError`、`RpcCallError<E>`、`TransportError`、`NetworkError`、
  `KytrixServiceError`、`XApiError`、`WalletApiError` 等）已经在 `crates/widget/src/error.rs` 里实现好了，
  直接用；widget 自己的错误类型放在自己的模块里实现该 trait。
- `WidgetState::failed` 里传的是 **不含任何前缀**的 detail，框架也不会替它加。失败在两个出口（banner / 占位）里显示的
  文字 **完全相同**，就是错误类型给出的那句。所以文案必须自足：需要"错误 / 失败"这类语气时，是 **错误类型自己的 i18n
  key** 负责写出来（现有 key 都是这个写法，如 `error.market.disconnected` = `The market stream disconnected: %{cause}`），
  不要指望展示层补前缀，也不要自己拼 `Error:`。
- 内部错误要区分"变更型命令的结果未知"（超时、取消、响应丢失）——它必须当作未知对待，不得自动重试。这类错误类型
  已经有本地化实现（`XMutationUnknown` / `WalletMutationUnknown` / `RpcCallError::ResponseDropped` 等）。

**主题与尺寸**

- 颜色取自 `cx.theme().roles.*`（语义角色，读取 `.hsla` 字段绘制）与 `cx.theme().components.*`（组件配方），必
  要时用 `cx.theme().appearance`；绘制期只读已解析的 `.hsla`，不做颜色空间转换。不得读取 `cx.theme().palette`
  （原始值只有 `Roles::resolve` 能读），也不硬编码 `rgb()`。需要 `use base_ui::style::theme::ThemeExt`。
- 控件尺寸用 `UnifiedSize`（`Small` / `Medium` / `Large`，各有 `h()` / `text_size()` / `gap()` / `px()`，图标用
  `icon_size()`），不写字面 px。

**UI 元素该放哪**

判据： **只有框架会决定它何时出现的元素属于 `widget::ui`；能被任意地方复用的原语留在 `base_ui`。**
所以 `Spinner`、`Button`、`Theme`、`Table`、`EmptyState` 在 `crates/base_ui`，`WidgetPlaceholder` 在
`crates/widget/src/ui`。业务 widget 两者都可以用，依赖方向是 `widget → base_ui`。

`EmptyState` 属于 `base_ui` 而不是 `widget::ui`，因为"这块地方什么都没有"不是 widget 独有的处境（`settings_ui` 的代理
表格先有同一种空态：`TableDataSet::empty` 在表体里说一句 `settings_ui.proxies.empty`），而"widget 什么时候没有正文、
要不要放占位"才是框架独有的判断。

---

## 10. 测试

- **unit test 与被测代码同模块**（`#[cfg(test)] mod tests`），能访问私有字段。不建集中式 `tests.rs`。
- `model.rs` 测纯逻辑；`service.rs` 用 fake 覆盖正常、超时、失败、取消、替换；`view.rs` 测状态上报、就绪态正文、
  恢复动作、迟到消息。
- 跨 crate 才用 `tests/` integration test。
- `src/lib.rs` 里不写测试。不为了测试私有实现而把 unit test 改成 integration test。

**widget 测试的最小装置**（照 `force_orders/src/view.rs` 的 `widget(cx)`）：

```rust
cx.executor().allow_parking();
cx.update( | cx| {
base_ui::style::theme::install(cx);
i18n::global::embedded::install(cx).expect("embedded locales should load");
global::install(cx).expect("test Tokio runtime should build");
…
});
```

- 状态断言走 `widget.status.state(cx)`；`AppContext` 上的读方法不需要 `&mut`。
- `dev-dependencies` 里需要 `gpui-kit` 的 `test-support`、`tokio` 的 `test-util`。
- 测试属性按层选：`model.rs` 用 `#[test]`，`service.rs` 用 `#[tokio::test]`，`view.rs` 用 `#[gpui_kit::test]`。
- 模块是 **目录**时（`model/`、`chart/axes/`）测试放兄弟文件 `foo/bar_tests.rs`，由父模块声明 —— 见 §2 的那条。
  多个测试模块共用的 fake 放 `foo/test_support.rs`，别在每个 `*_tests.rs` 里复制一份。

**五个会浪费大量时间的陷阱**

1. **`debug_bounds` 由绘制动作填充。** 缓存命中会复用上一帧的 paint range、不再绘制，所以被缓存元素的选择器在命中
   那一帧答出的仍是 **上一帧的 bounds**（旧值照旧可查，看不出"这一帧没有重画"）。想断言"正文没重建"要用实体
   `Render` 计数器，不是 `debug_bounds`。
2. **`window.refresh()` 会把 `window.refreshing` 置真，强制所有缓存未命中**，所以它正好是"无关帧"的反面。要造无关帧，
   就 notify 一个 harness 另外渲染的实体（`crates/widget/src/surface.rs` 的测试里那个 `Marker`）。
3. **要按次数数通知，就把每次写入放进各自的 `cx.update`。** GPUI 会合并同一次 update 内对同一实体的 notify：在同一个
   `cx.update` 里连写两次状态只会数到一次（`crates/widget/src/status.rs` 的测试因此每次 `report` 各占一个
   `cx.update`）。`observe` 放在同一次 update 里并不丢计数，只要注册排在写入之前——订阅的激活也延后到那次 update
   结束，排在写入之后就数不到。
4. **不要手推像素偏移**。`m_1` 与容器内边距的组合不由断言算出来，只保留相对断言（`pane.rs` 的
   `placeholder.bottom() <= body.bottom()`、`header.bottom() <= body.top()`、`banner.bottom() <= body.top()`）。
5. **正文区域的 `debug_bounds` 读点必须先强制一次完整重画**：正文槽位是唯一被 `.cached()` 的层级，读之前先
   `window.refresh()` + `run_until_parked()`（即各 pane 的 `repaint` 范式），presence 与 absence 一视同仁——命中缓存的
   那一帧会把上一帧的 paint range 连同区间里记下的 bounds 一起搬进新帧（`Window::reuse_paint`），所以那一帧的读数说的是
   "上一帧画过什么"，不是"这一帧的树里有什么"。header / dialog / overlay / placeholder 不在缓存内，不需要这一步。
   代价要记住：强制重画会让这些读点不再顺带检出"内容变了却没 notify"的回归（陈旧帧本该让 presence 断言失败），
   所以"正文是否重建 / 是否命中缓存"一类主张 **只能**用实体 `Render` 计数（框架侧见 `widget::surface` 的 Counter），
   绝不用 `debug_bounds`。

---

## 11. 新 Widget 检查清单

1. `WidgetId` 用 `WidgetId::new_static("kytrix.<名字>")`，`kytrix` 之后只有一段、小写、点分段；持久化后不可改。
2. Config 必须实现 `WidgetConfig` 并给 `SCHEMA_VERSION`，当前一律 `WidgetConfigSchemaVersion::V1`。没有可持久化选项也要
   一个空的 config 类型，不要用 `()` 把 schema 版本糊过去。不兼容变更就直接删旧结构， **不写 migration、不写兼容层、
   不写 fallback**。每一项都要有 `Default`。
3. `DISPLAY_NAME` 等文案 key 同时加到两个 locale；`CATEGORY` 声明它在 Widget Gallery 的哪个分类下被列出。
   `SELECTABLE` 只对框架自建的组件（目前只有 `kytrix.empty`）才是 `false`：用户能选的一律留默认 `true`。
4. `model.rs` 先写纯逻辑 + 单测，再做 UI。
5. `service.rs` 定义协议与私有 Source trait，fake 覆盖正常/超时/失败/取消/替换。
6. `view.rs` 只写状态上报（`report(WidgetState, cx)`，见 §5.1）与就绪态正文；`has_content()` 是便宜谓词。
7. 有持久化选项就实现 `config()`，并在 **每个**改配置的地方 `config_change.publish(cx)`；没有就什么都不写（`config()`
   留默认 `None`）。再把规则 2 的校验放进 `load`， **订阅发生在校验之后**（见 §4.5）。
8. 在 `crates/widget_registry/src/catalogue.rs` 里 `WidgetRegistration::for_view::<XxxWidget>()` 注册；忘记注册就等于
   widget 不存在。
9. `lib.rs` 只声明模块。
10. 删掉重构后不再使用的代码、字段、i18n key 和依赖。生产代码里不允许 `#[allow(dead_code)]` / `#[expect(...)]`。

**明确不做**：自动重连与退避；框架级 Ready 水位；子区域的嵌套状态句柄（用 `base_ui` 原语 + 各自局部状态）；错误按来源分
通道（失败只有 `WidgetState::failed` 一个出口，控件标签只有框架的 `common.retry` 与 widget 自己的 key 两种来源）；
widget 级的缓存构件；共享的恢复按钮包装。

---

## 12. 验收

```powershell
cargo +nightly fmt --all -- --check
cargo +nightly clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -p kytrix
```

新增或迁移 Widget 时逐条自查：

- [ ] `Render` 里没有状态分支，没有 banner / 占位，没有 `.cached()`。
- [ ] 没有自造状态枚举；状态写入走 `report(WidgetState, cx)`；`WidgetState::waiting` 只用于中性引导文案，没有为"空数据"
  造等待文案；等待文案用 `Notice`（key 是字面量，`Notice::MarketNotLoaded` 交 `venues` 与 `supported`），用到的 key 必须
  同时存在于 en 与 zh-CN；能拿到空快照的地方都画了 `EmptyState` + `common.no_data`（表格走 `TableDataSet::empty`），
  没有留一块空白正文。
- [ ] `has_content()` 不构建元素；正文是 `size_full().min_h_0()` 的 flex 列，`Render` 里没有 `.cached()`，根元素上也没有
  自己的底色 / 圆角 / `overflow_hidden`。
- [ ] 内容变化时 notify 了自己或持有内容的子实体；滚动 / 交互的 `ElementId` 稳定。
- [ ] 时区 / 账户只经 §4.6 的两条接缝：`data_time_zone(cx)` + `widget::time_zone`（`_time_zone: Option<Subscription>`）、
  `widget::account::account_of` + `reconcile_account`；没有 watch / observe_global / 轮询，子实体被一起 notify。
- [ ] 每个消息入口都校验 `StreamKey` 且检查 `is_failed`。
- [ ] 前置检查没过时 header 里没有可用的控件：从已加载场所取选项的控件按「有没有东西可选」判定（`InstrumentButton` 读
  `filters.offers_anything()`），作用于当前 scope 的控件读 `awaiting_market`（或由它派生的 `has_scope()` /
  `refresh_is_disabled()` / `venue_picker_is_disabled()`）；禁用的 `Button` 不挂 click，动作入口再挡一次。
- [ ] 每个失败路径都附上框架给的 `recovery`（或明确换一个 `WidgetAction` 的标签 key，例如 `widget.news.view_plans`）。
- [ ] 用户可见错误是 `LocalizedError`，不是 `Display` 拼接。
- [ ] 新 key 已加到 en 与 zh-CN。
- [ ] 依赖声明在 workspace 根，crate 已登记进 `members`。
- [ ] 有持久化选项：`config()` 返回字段， **每个**改配置的地方都 `config_change.publish(cx)`，规则 2 的校验在 `load`
  里且 **订阅发生在校验之后**（取数失败时不动），`SCHEMA_VERSION` 是 `V1`，每一项都有 `Default`。
- [ ] 没有持久化选项：不实现 `config()`；布局里那个 pane 的 `config` 是 `null`。
- [ ] 测试覆盖：状态上报、有/无内容两种失败出口、迟到消息、恢复动作、容量与裁剪、取消与替换；有配置的还要覆盖
  「改动会写回」「不支持的项回默认并上报」「取不到数时不动」「订阅的是修正后的配置」。
- [ ] 有 `Notice` 调用点（`Notice::message(..)` / `Notice::MarketNotLoaded { .. }`）的 widget，把它那个生产源文件加进
  `crates/widget/src/error.rs` 的 `WIDGET_NOTICE_SOURCES`：这份清单是手写的（13 条），`include_str!` 只吃字面量，路径
  写错就是编译错误；覆盖率守卫按 `production_source` 递归扫 `crates/widgets` 与清单双向核对（漏登与多登都会失败），
  key 覆盖断言则要求扫出来的每个 key 在每种 locale 都定义且非空。

文档或静态检查通过不等于性能已测量。未执行的在线、运行时 UI 与 release profile 验证必须明确说明。
