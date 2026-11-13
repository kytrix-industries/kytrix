# Phase 03 · 节点状态核心 — 详细设计（v1.1，裁决响应版）

> **文档类型**：实施阶段详细设计（Phase 03 的唯一权威输入）
> **上游**：`custom-gui-engine-design.md`（D16 §3 全章、§1.3/§1.4、§4.4、§4.9、§6.2、§7.8、§8.4、§9.1/§9.2、附录 A）、
> `design/phase-01-foundation.md`（v2）、`design/phase-02-paint-foundation.md`（v2）、`design/DEVIATIONS.md`（全量）、`AGENTS.md`
> **版本**：**v1.1**（2026-11-12 裁决响应版）。v1 的 5 项提案被**全部采纳**，另按 9 条细化修订；
> 全部修订点见 **§23 裁决记录**（v1 的任何表述以 §23 为准）。编号接续 `F-34` / `S-23` / `V-23`（历史条目不改写，见 §22）。
> **本阶段代码状态**：**只做详细设计，不写生产代码**。落地文件见 §3.1；对既有文件的改动仅列在 §3.4（`lib.rs` / `paint.rs` /
> `paint/scene.rs` / `error.rs` / `Cargo.toml`）。
> **事实基线**：Rust `1.97.1` stable + `nightly 1.99.0`（clippy `0.1.99`）；
> 全部第三方 API 结论以 `E:\.cargo\registry\src\rsproxy.cn-e3de039b2554c837\` 下的**真实源码**为准（行号相对该 crate 的 `src/`），
> **未使用任何 web 检索**。

---

## 0. 结论摘要

1. **范围**：节点树（`Node`/`Arena`/`NodeId`）、状态（`Signal`/`Derived`/`Transient`/`StateStore`）、
   依赖图（`DependencyGraph`/`ReadCtx`/`ReadTracker`）、脏标记与传播（`DirtyFlags` + 冒泡终止）、
   样式契约（`Style`/`StylePatch`/`Cursor`）、Scene 的 lane 分配与绘制序（`Scene`/`LaneId`）、headless 运行时
   （`App`/`Window`/`WindowId`）。**不写**布局算法、文本、paint pass、实例编码、damage 物理化、渲染、平台事件、a11y、Action。
2. **5 项裁决已全部采纳**（提案见 §20，裁决记录见 §23）：
   ① `Application` / `AppHandle` **推迟 P08/P09**；`App` 侧命名为 `runtime_handle(&self) -> &Handle`（`handle()` 留给 P09）；
   ② `versions: Vec<u64>` **删除**（含每槽位 `version`），`App::write<T: Clone + PartialEq + 'static>` + 等值短路；
   ③ `Scene` 的写入面在 P03 是 **`pub`** 且 doc 首行标注 engine-internal；`Scene` 不 `Clone`/`Default`、无公开构造器
      （`pub(crate) fn empty()`），只读面用 `lane_primitives -> &[Primitive]`（不暴露 `Lane`）；
   ④ `Owner` 携带 `WindowId`（`Owner::Node { window, node }`），`OwnerSet = SmallVec<[Owner; 2]>`；
   ⑤ 派生环检测在**运行时求值栈**（`EvalGuard` 的 `Drop` 保证 panic 安全），`read_derived` 返回 `Result`。
   **另有两条不入裁决的落地决策**：`App::refresh_styles(window) -> Option<StylePassReport>` 作为 P03 的「样式求值 pass」
   （S-34/F-40：它是让 `ReadCtx`/依赖图/`Derived`/`affects_layout`/`bind_style` 这 10 余项能力获得**非 test 消费者**的唯一方式，
   P06 的 paint 在重建 lane 前调用同一实现）；`Border` 复用 `paint::primitive::Border`（字段核对见 §10.2）。
3. **13 个 `pub(crate)` 模块的最后形态**见 §3.1；每个 `pub(crate)` item 的非 test 消费者见 §4.3（**逐条**，无遗漏）。
4. **测试**：**82** 个内联单测（§17），覆盖 §17 的各表；门禁见 §18（含 dead_code 审计与文件边界）。
5. **依赖增量**：`ahash`（normal，已在 dev）、`smallvec`、`tokio`（`rt` 面）；`bitflags`/`bytes`/`bytemuck` 已在 P02 声明。
   不新增 `slab`/`indexmap`（§19）。
6. **无阻塞性问题**。需上级裁决的 5 项见 §20，风险见 §21。

---

## 1. 范围与非目标

### 1.1 本阶段交付

| 交付 | 内容 | 对应设计 |
|---|---|---|
| `src/node/id.rs` | `NodeId`、`Arena<T, K>`、`ArenaKey`、`next_generation` | §3.2 数据模型 |
| `src/node/signal.rs` | `SignalId`/`Signal<T>`、`DerivedId`/`Derived<T>`、`Transient<T>`、`StateStore`（`SignalValue`/`SignalSlot`/`DerivedSlot`） | §3.2、§3.3、§3.7、§3.9 |
| `src/node/dependency.rs` | `Owner`/`OwnerSet`、`DependencyGraph`、`ReadTracker`、`ReadCtx`、`EvalStack` | §3.3、§3.8 |
| `src/node/dirty.rs` | `DirtyFlags`、`depends_on_children`（冒泡终止判定） | §3.4、§4.4 |
| `src/node/tree.rs` | `Node`、`NodeKind`、`LayoutBox`、`NodeTree`（挂载/卸载/绘制序/标记） | §3.2、§3.4、§4.4 |
| `src/node/builder.rs` | `NodeBuilder` | §3.10、附录 A |
| `src/paint/style.rs` | `Style`、`StylePatch`、`Cursor` | §4.9.2/§4.9.3 |
| `src/paint/scene.rs`（追加） | `Scene`、`LaneId`、`Lane::clear` | §5.1、§5.3；P02 §7.2 冻结形状 |
| `src/app.rs` | `App`（`StateStore` + `DependencyGraph` + `Handle` + `windows`）、写路径、`refresh_styles`、窗口生命周期、`StylePassReport` | §1.4、§3.5、§3.6、§9.2 |
| `src/window.rs` | `WindowId`、`WindowOptions`、`Window`（`NodeTree` + `Scene` + `Scale`） | §1.4、§8.2 |
| `src/error.rs`（追加） | `EngineError::{StaleNode, CyclicDerived}` | P01 §6.2.2 冻结表 #7/#8 |
| `src/lib.rs` / `src/paint.rs`（追加） | `pub mod app; pub mod node; pub mod window;` / `pub(crate) mod scene;` → `pub mod scene;` | P01 §6.1、P02 §3.1 |
| `crates/kytrix_gui/Cargo.toml` | `ahash`（移到 normal）、`smallvec`、`tokio` | §19 |

### 1.2 明确非目标

| 不做 | 归属 |
|---|---|
| 布局计算、`LayoutPartialTree` 实现、taffy `compute_*` 调用、`set_layout` 写回 | P04 |
| `hit_test`、`LayoutOverride`、`MeasureFn` | P04 |
| 文本（`bind_text`、`ShapedLine`、文本 measure） | P05 |
| paint pass（lane 重建、`Primitive` 生产、style→`Quad`/`Shadow` 拆分） | P06 |
| damage 物理化（`PhysicalRect`、合并、阈值） | P06 |
| `GpuPrimitive`/`InstanceBuffer`/编码 | P07 |
| `render/*`、`platform/*`、事件循环、`Event`/`InputEvent`/`Action`/`HitResult`/`KeyBinding` | P08–P17 |
| a11y（`A11yNodeId`、`A11yRole`/`A11yPatch`、`bind_a11y`） | P11 |
| `FocusScope`/`ScrollHandle`/`VirtualList`/`Overlay`/`DragState`/`AnimationId` | P13 |
| `Application`/`AppHandle`/`PlatformDispatcher`/`FrameScheduler`/帧状态机（§3.5 的 `Idle→…→WaitGpu`） | P08/P09（裁决 ①） |
| `Children`/`Slot`/`NodeHandle` | P08（§12.3） |
| `Style` 的圆角/clip/阴影/渐变/字体族字段 | P06（§10.1，无消费者） |
| `Scene` 的 clip 表（`clip` 栈下沉到 lane 内） | P06/P07 |

### 1.3 不变量落地映射

| 不变量 | 本阶段机制 | P03 的验证点（可执行） |
|---|---|---|
| **I3** 空闲不提交帧 | `App` 只在「有节点被标脏」时置 wake 标志；`take_wake_request()` 取走即清零；仅派生失效不置位 | §17 `app.rs` 的 `write_only_derived_does_not_wake` |
| **I5** 节点树即布局树 | P03 只建**唯一**的节点存储（`Arena<Node>` + `children`/`parent`），**不建**第二棵树；P04 的 `LayoutPartialTree` 直接读写它 | 无第二容器（§4.3 的 item 表即证明）；`NodeTree` 无 `layout_*` 影子表 |
| **I6** 状态只能经 `Signal` 写入 | `StateStore` 字段全私有；`Signal<T>` 只带 id；读只有 `ReadCtx` 一条路径；`Node` 字段全私有（只读访问器） | §17.2-1（`SignalId` 稠密且无 remove API）、§17.9-2（写只标读者，未读者保持干净）、`Scene`/`Node` 无 `&mut` 公开出口（编译期） |
| **I13** 单一 tokio runtime | `App` 只**持有** `tokio::runtime::Handle`；P03 不 spawn、不 `Handle::current()`（`clippy.toml` 已 deny） | §18 门禁 7 的 `Handle::current` 扫描 |
| **I17** 静态结构/动态属性分离 | `Style`（挂载时一次写入）+ `StylePatch`（`bind_style` 经依赖追踪重算）；`StylePatch.affects_layout` 决定是否升级为 `LAYOUT` | §17.9-3（`write_marks_layout_when_patch_affects_layout`）、§17.9-5（`refresh_styles_escalates_layout`） |

---

## 2. 事实基线与核验记录

> 全部条目来自 registry 源码；**P03 不得基于猜测**（AGENTS §1.4）。

### 2.1 taffy 0.15.0（`Style`/`Dimension`/`Layout`/`Cache`）

```rust
// style/mod.rs:709 —— 注意：不是 Default 派生，也没有 Copy/Eq
#[derive(Clone, PartialEq, Debug)]
pub struct Style<S: CheapCloneStr = DefaultCheapStr> { /* 44 个 pub 字段 */ }
// style/mod.rs:876 / :953
impl<S: CheapCloneStr> Style<S> { pub const DEFAULT: Style<S> = … }
impl<S: CheapCloneStr> Default for Style<S> { fn default() -> Self { Style::DEFAULT } }

// style/mod.rs:63 —— CheapCloneStr 的 blanket impl
pub trait CheapCloneStr:
    AsRef<str> + for<'a> From<&'a str> + From<String> + PartialEq + Eq + Clone + Default + Debug + 'static {}
impl<T> CheapCloneStr for T where T: /* 同上 */ {}
```

| 结论 | 证据 |
|---|---|
| `Style<SmolStr>` **成立**（`SmolStr` 满足 blanket impl 全部约束） | `style/mod.rs:63`；P01 附录 A.1 已记录 |
| **字段受 feature 门控**：`float`/`clear`（`float_layout` 关闭）、`flex_line_count`（`flexbox_balance` 关闭）在引擎的 feature 集下**不存在**；`grid_*` 存在 | `style/mod.rs:737-742`、`:820`、`:840-871`；workspace 的 taffy features 见 P01 §5.1 |
| `Style` **不是 `Copy`**，`Clone`/`PartialEq`/`Debug` 有 | `style/mod.rs:706` |
| `Display`：`Block`/`FlowRoot`/`Flex`/`Grid`/`None`；`Display::DEFAULT == Flex` | `style/mod.rs:251`、`:270` |
| `Position`：`Static`/`Relative`/`Absolute`/`Fixed`；默认 `Static` | `style/mod.rs:354` |
| `Dimension` 是 `CompactLength` 的**新类型**（不是枚举）；判定用 `is_auto()`/`is_sizing_keyword()`/`is_stretch()`/`expand()` | `style/dimension.rs:329`、`:522`、`:528`、`:534`、`:559` |
| `ExpandedDimension` 变体：`Length(f32)`/`Percent(f32)`/`Auto`/`MinContent`/`MaxContent`/`FitContentPx(f32)`/`FitContentPercent(f32)`/`FitContent`/`Stretch`/`Content`/`Calc(*const ())` | `style/dimension.rs:585-613` |
| `Layout` 是 `Copy`，字段 `order`/`location`/`size`/`scrollable_overflow_rect`（`content_size` feature）/`scrollbar_size`/`border`/`padding`/`margin` | `tree/layout.rs:579-612` |
| `Cache`：`pub struct Cache`（`Default`），`new`/`get`/`store`/`clear`/`is_empty`；**未实现 `CacheTree`**（P04 自己转发） | `tree/cache.rs:214`、`:237-360`；P01 附录 A.1 |
| `Size<T>`/`Point<T>`/`Rect<T>` 是泛型结构体，`Size::<f32>::ZERO`、`Size::<f32>::new(w,h)` 存在 | `geometry.rs:373`、`:575`、`:602`、`:672` |

**对 P03 的直接后果**：`Style.inner` 的**类型必须写 `taffy::Style<SmolStr>`**（不能写 `taffy::Style`，默认参数是
`DefaultCheapStr`，与 `SmolStr` 不是同一类型）；冒泡终止判定必须走 `Dimension::expand()`（`Dimension` 不是枚举）。

### 2.2 tokio 1.54.1（`Handle`）

| 结论 | 证据 |
|---|---|
| `pub struct Handle`，`Clone`、`Send + Sync`、`'static` | `runtime/handle.rs:13` |
| `Handle::spawn<F>(&self, future: F) -> JoinHandle<F::Output> where F: Future + Send + 'static, F::Output: Send + 'static` | `runtime/handle.rs:197` |
| `Handle::current()` 在 runtime 上下文外 **panic**，且被 `clippy.toml` workspace 级 `disallowed-methods` 禁止 | `runtime/handle.rs:144`；`clippy.toml:32` |
| `Handle` 由 `rt` feature 提供（`runtime/mod.rs:620-621` `pub use handle::{EnterGuard, Handle, TryCurrentError}`） | `runtime/mod.rs:620` |
| workspace 的 `tokio` 为 `features = ["full"]`，**成员无法收窄**（features 是并集） | 根 `Cargo.toml:235` |

**对 P03 的直接后果**：`App::new(runtime: tokio::runtime::Handle)` 只**保存**句柄（字段名 `runtime`）；P03 内**不得**出现
`Handle::current()` / `spawn_blocking`（后者同样在 deny 列表）。单测用 `tokio::runtime::Builder::new_current_thread().build()`
取句柄（`rt` 能力已由 `full` 提供）。

### 2.3 容器与位标志

| crate | 结论 | 证据 |
|---|---|---|
| `ahash 0.8.12` | `pub type HashMap<K, V> = std::collections::HashMap<K, V, ahash::RandomState>`；`HashSet` 同理；`RandomState` 可直接用 | `lib.rs:132`、`:135`、`:145` |
| `smallvec 1.16.5` | `SmallVec<[T; N]>`；`new`/`with_capacity`/`push`/`pop`/`insert`/`remove`/`retain(FnMut(&mut T))`/`truncate`/`clear`/`drain`/`as_slice` | `lib.rs:850`、`:1489`、`:1472`、`:1654`、`:1422`、`:1464`、`:1099`、`:1437` |
| `bitflags 2.13.2` | `bitflags!` 生成的**公开**类型只挂 inherent 方法（`empty`/`all`/`bits`/`from_bits*`/`is_empty`/`intersects`/`contains`/`insert`/`remove`/`toggle`/`set`/`union`/`difference`/`intersection`/`symmetric_difference`/`complement`），**不自动派生任何 std trait**；`Internal` 类型有 `Default`/`Debug`/`Display`/`PartialOrd`/`Ord` | `lib.rs:478-578`（宏）、`internal.rs:47-56`（Default/Debug）、`public.rs`（inherent 方法） |

**对 P03 的直接后果**：
1. `DirtyFlags` 必须在 `bitflags!` 里显式写 `#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]`（`Debug` 走
   `Internal` 的 `impl Debug`，`Default` 只有在需要时才派生）；
2. `bitflags!` 展开体内含 `#[allow(...)]`（`lib.rs:500-535`）。这是**宏定义**里的属性，不是本仓库的生产代码；
   AGENTS §4 的禁令针对我们写的代码，§18 门禁 7 的 grep 需带 `--exclude` 语义说明（§18 已注明）。

### 2.4 本地 probe（`%TEMP%\kg-p03-priv-probe`，不在仓库内）

P03 只做设计，不写生产代码；唯一需要的**语言行为**核验是「公开签名里出现 `pub(crate)` 类型」的编译结果
（它直接决定 F-38 的形态）。`rustc 1.97.1`，`--edition 2024 --crate-type lib -D warnings`：

```text
error: type `Lane` is more private than the item `Scene::lane`
 --> probe.rs:6:5
  |
6 |     pub fn lane(&self, _id: LaneId) -> Option<&Lane> { self.lanes.first() }
  |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ method `Scene::lane` is reachable at visibility `pub`
  |
note: but type `Lane` is only usable at visibility `pub(crate)`
  = note: `-D private-interfaces` implied by `-D warnings`
```

→ **结论**：`Scene::lane -> Option<&Lane>` 在 `-D warnings` 下**无法编译**（lint `private_interfaces` 是
warn-by-default，`-D warnings` 提升为 error；唯一的规避是 `#[allow]`，AGENTS §4 禁止）。这直接推出 F-38：
`lane` 返回 `Option<&[Primitive]>`、`lane_mut` 改为 `push`。

### 2.5 本章未验证项（显式登记）

| 项 | 处理 |
|---|---|
| `size_of::<taffy::Style<SmolStr>>()` 的**实测值** | 本阶段不跑 probe（禁写代码）；以 `assert!(size_of::<Node>() <= 1024)` 的**上限断言**兜底（§17 `tree.rs`），P04 落地时补实测并在 §11.1 回填 |
| `Handle` 在 `Runtime` drop 后 `spawn` 的行为 | P03 不 spawn，无结论需求 |

---

## 3. 文件树与落地顺序

### 3.1 文件树（P03 落地 / 推迟）

```text
crates/kytrix_gui/src/
├── lib.rs                      P01 → P03 追加 3 行：pub mod app; pub mod node; pub mod window;
├── error.rs                    P01 → P03 追加 2 个变体（StaleNode / CyclicDerived）
├── math.rs                     P01（不改）
├── app.rs                      P03 ✅ 公共模块根：App（StateStore + DependencyGraph + Handle + windows）
├── window.rs                   P03 ✅ 公共模块根：WindowId / WindowOptions / Window
├── node.rs                     P03 ✅ 公共模块根：只有 pub mod 行（无 item）
│   ├── id.rs                   P03 ✅ NodeId / Arena<T, K> / ArenaKey / next_generation
│   ├── signal.rs               P03 ✅ SignalId / Signal / DerivedId / Derived / Transient / StateStore
│   ├── dependency.rs           P03 ✅ Owner / OwnerSet / DependencyGraph / ReadTracker / ReadCtx / EvalStack
│   ├── dirty.rs                P03 ✅ DirtyFlags / depends_on_children
│   ├── tree.rs                 P03 ✅ Node / NodeKind / LayoutBox / NodeTree
│   └── builder.rs              P03 ✅ NodeBuilder
├── paint.rs                    P02 → P03 一行改动：pub(crate) mod scene; → pub mod scene;
│   ├── color.rs / primitive.rs / canvas.rs / geometry.rs / atlas.rs   P02（不改）
│   └── scene.rs                P02 → P03 追加 Scene / LaneId（+ Lane::clear）
├── layout.rs                   P04 ⏳
├── text.rs                     P05 ⏳
├── event.rs                    P08 ⏳
├── interaction.rs              P13 ⏳
├── a11y.rs                     P14 ⏳
├── resources.rs                P15 ⏳
├── render.rs / render/         P05+ ⏳
├── platform.rs / platform/     P12+ ⏳
└── test_support.rs             P08 ⏳
```

`node.rs` 的 P03 内容（**只有模块清单**，无 item、无 re-export、无 `pub(crate) use`）：

```rust
//! 节点树与状态：D16 的持久节点 + 细粒度依赖追踪（P03 落地）。
//!
//! 公共契约按文件归属（P01 §4.5 的「公开路径 = 定义所在文件」）：
//! `node::id::*`、`node::tree::*`、`node::signal::*`、`node::dependency::ReadCtx`、
//! `node::dirty::DirtyFlags`、`node::builder::NodeBuilder`。

pub mod builder;
pub mod dependency;
pub mod dirty;
pub mod id;
pub mod signal;
pub mod tree;
```

### 3.2 落地顺序（按「非 test 消费者」排序）

| # | 文件 | 依赖 | 首个非 test 消费者 | 为何不能更早/更晚 |
|---|---|---|---|---|
| 1 | `paint/style.rs` | `paint::primitive::Border`、`paint::color::Color` | `node::tree::Node`（字段） | 无依赖，最底层数据契约 |
| 2 | `node/id.rs` | `math`（无） | `node::tree`、`paint::scene` | `Arena`/`NodeId` 是节点与 lane 的共同底座 |
| 3 | `paint/scene.rs`（追加） | `node::id::{Arena, next_generation}`、`paint::primitive::Primitive` | `window::Window`（字段） | 依赖 2 |
| 4 | `node/signal.rs` | `error` | `node::dependency`、`app::App` | 状态存储先于依赖图 |
| 5 | `node/dependency.rs` | `node/id`、`node/signal`、`window::WindowId` | `app::App`、`node::tree`（dirty/deps 写入） | 依赖 2/4 |
| 6 | `node/dirty.rs` | `node/tree::Node`、`paint::style::Style`、taffy | `node::tree::NodeTree::mark_layout` | 需要 `Node` 的形状（§11） |
| 7 | `node/tree.rs` | 1/2/3/5/6 | `node::builder`、`app::App` | 最重的聚合点 |
| 8 | `node/builder.rs` | `node/tree`、`paint/style` | `app` 的测试与 P08 | 依赖 7 |
| 9 | `window.rs` | `node/tree`、`paint/scene`、`math::Scale` | `app::App`（`windows` 字段） | 依赖 3/7 |
| 10 | `app.rs` | 全部 | 自身即消费者（`write`/`refresh_styles`） | 最后 |
| 11 | `error.rs`（改） / `lib.rs`（改） | 2/4 | 编译期 | 变体 payload 依赖 `NodeId`/`DerivedId` |

### 3.3 模块形态与可见性硬约束（与 P01/P02 一致）

| 规则 | 依据 |
|---|---|
| 公共模块根（`app.rs`/`window.rs`/`node.rs`）用 `pub mod <child>;`，**不写** `pub use` / `pub(crate) use` / `mod.rs` / `#[path]` | P01 §4.1、M-06/M-07、lint `pub_use = "forbid"` |
| `node.rs` / `paint.rs` 不定义 item（含 `const _: () = assert!(…)`；静态断言放叶子文件） | P02 §3.3、M-03 |
| 每个类型只允许**一个** inherent `impl` 块 | lint `multiple_inherent_impl = "forbid"` |
| 字段可见性只有「全 pub」或「全私有」 | lint `field_scoped_visibility_modifiers = "forbid"`（S-20） |
| 内联单测在被测文件末尾（`#[cfg(test)] mod tests`） | AGENTS §5.1、P01 §4.3c |
| 短字符串 `SmolStr`、字节 `Bytes`、哈希容器 `ahash`、`SmallVec` 用于小集合 | AGENTS §2.2/§2.3/§2.4 |
| 零 `unsafe` / 零 `#[allow]` / 零 `#[expect]` / 零 `serde_json` | AGENTS §4、§1、§2.1 |
| 文件行数不设上限 | A-01 |

### 3.4 对既有文件的精确改动（P03 唯一允许的「改旧」范围）

| 文件 | 改动 | 理由 |
|---|---|---|
| `src/lib.rs` | 追加 `pub mod app;` `pub mod node;` `pub mod window;`（保持字典序：app, error, math, node, paint, window） | P01 §6.1 的「只追加行」 |
| `src/paint.rs` | `pub(crate) mod scene;` → `pub mod scene;`；模块文档的「crate 私有」表述同步 | P02 §19 明确「P03 增加 `Scene`/`LaneId`」 |
| `src/paint/scene.rs` | 追加 `LaneId`/`Scene`；`Lane` 追加 `pub(crate) fn clear(&mut self)` | `Scene::clear_frame` 需要 `Lane::clear`（保 `Vec` 容量） |
| `src/error.rs` | 追加 `StaleNode`/`CyclicDerived` 两个变体 + `use` | P01 §6.2.2 #7/#8 |
| `Cargo.toml`（crate） | `ahash` 从 `[dev-dependencies]` 移到 `[dependencies]`；新增 `smallvec`、`tokio` | §19 |
| `src/paint/canvas.rs` | **不改** | — |

---

## 4. 公共/私有边界与公开路径表增量

### 4.1 公开路径表增量（应用层迁移的唯一契约）

> 规则同 P01 §4.5：**公开路径 = 类型实际定义所在的文件路径**；不得为缩短路径而移动定义。

| 类型 | 公开路径 | 定义文件 | 消费 Phase |
|---|---|---|---|
| `App` | `kytrix_gui::app::App` | `src/app.rs` | P08（应用启动）/ P09（帧驱动） |
| `StylePassReport` | `kytrix_gui::app::StylePassReport` | `src/app.rs` | P06（`FrameRecord` 的样式 pass 统计）/ P08 |
| `WindowId` `WindowOptions` `Window` | `kytrix_gui::window::*` | `src/window.rs` | P08 / P12 |
| `NodeId` | `kytrix_gui::node::id::NodeId` | `src/node/id.rs` | P04（hit_test）/ P06 / P08（Probe） |
| `NodeTree` `Node` `NodeKind` `LayoutBox` | `kytrix_gui::node::tree::*` | `src/node/tree.rs` | P04 / P06 / P08（Probe 直查 `LayoutBox`/`Style`/`lanes`，§6.2） |
| `Signal<T>` `SignalId` `Derived<T>` `DerivedId` `Transient<T>` | `kytrix_gui::node::signal::*` | `src/node/signal.rs` | P08（应用状态）/ P13（动画） |
| `ReadCtx` | `kytrix_gui::node::dependency::ReadCtx` | `src/node/dependency.rs` | P08（`bind_*` 闭包） |
| `DirtyFlags` | `kytrix_gui::node::dirty::DirtyFlags` | `src/node/dirty.rs` | P06（帧记录）/ P08（Probe） |
| `NodeBuilder` | **`kytrix_gui::node::builder::NodeBuilder`** | `src/node/builder.rs` | P08（`base_ui` 建树） |
| `Style` `StylePatch` `Cursor` | `kytrix_gui::paint::style::*` | `src/paint/style.rs` | P06（style→paint）/ P08（`StyleBuilder` 返回 `Style`）/ P12（光标） |
| `Scene` `LaneId` | `kytrix_gui::paint::scene::*` | `src/paint/scene.rs` | P06（`reused_lanes`）/ P07 / P08（只读句柄） |

> **对 P01 §4.5 的修订（→ F-34）**：`NodeBuilder` 的路径从 `kytrix_gui::node::*`（模块根）改为
> **`kytrix_gui::node::builder::NodeBuilder`**。原因：本阶段的文件树把 `NodeBuilder` 放在 `node/builder.rs`（brief 明确规定
> `node.rs` 只做 `pub mod` 声明），而 P02 §3.3 已固化「模块根不定义 item」。两处约束冲突时以「路径 = 定义文件」为准。

### 4.2 §1.3 公开白名单的传递闭包扩充

§1.3 已列：`App`、`WindowOptions`、`WindowId`、`NodeId`、`NodeBuilder`、`Signal<T>`、`SignalId`、`Derived<T>`、
`Transient<T>`、`Style`、`StylePatch`、`Scene`、`Rect`/`Size`/`Point`。下表登记**由这些条目传递带出**、或本阶段新增必须公开的类型
（只扩充「已被现有条目引用」的类型，不新增能力）：

| 传递新增的公开类型 | 由谁带出 | 消费 Phase |
|---|---|---|
| `NodeTree` `Node` `NodeKind` `LayoutBox` | `NodeBuilder::build(&mut NodeTree, …)` 的参数；`Probe` 直查 `LayoutBox`（§6.2）；`NodeBuilder::canvas/geometry` 的载荷 | P04/P06/P08 |
| `ReadCtx` | `bind_style` 闭包参数（§4.9.3）；§1.3「读取必须经 `FrameCtx::read()`」的消息对应物（P03 叫 `ReadCtx`） | P06/P08 |
| `DirtyFlags` | `Node::dirty()` 的返回类型（帧记录/探针断言「只脏了一个节点」） | P06/P08 |
| `DerivedId` | `EngineError::CyclicDerived` 的 payload（P01 §6.2.2 #8）；`Derived<T>` 的 id | P05+ |
| `LaneId` | `Node::lanes()` 的元素类型；`Scene::order()`；P02 §4.2 已登记 | P06/P07 |
| `Cursor` | `StylePatch::cursor` 的字段类型（P01 §4.5 已登记） | P12 |
| `StylePassReport` | `App::refresh_styles` 的返回类型 | P06/P08 |
| **`taffy::Style<SmolStr>`** | `Style::inner` 的**公开字段类型**（F-35） | P05/P06（布局）/ P08（`base_ui::StyleBuilder`） |

**P03 落地时新增/确认的公开面增量（审查 A-N8）**

| 增量 | 位置 | 理由 / 消费者 |
|---|---|---|
| `Node::{id, parent, children, layout, prev_layout, damage, lanes}` | `node/tree.rs` | P08 的 `Probe` 必须直接查布局盒/damage/lane（§5.3 的只读面要求）；`App::write` 用 `style_patch()`。**均为只读**，`Node` 字段仍全私有（I6） |
| `App::windows() -> impl Iterator<Item = (WindowId, &Window)>` | `app.rs` | 帧是**按窗口**推进的（`refresh_styles(window)`/P06 的 paint），只给 `&Window` 拿不到窗口 id |
| `WindowId::to_bits() -> u32` | `window.rs` | 结构化日志/诊断需要单值投影（`WindowId` 从 1 起、0 非法） |
| `Signal<T>: PartialEq + Eq`（按 `SignalId` 相等） | `node/signal.rs` | 句柄是身份而非值：让「同一个信号」可比较（测试/`HashMap` 键）；**不**比较 `T`，因此不引入 `T: PartialEq` 约束 |
| `LaneId::{index, generation}` | `paint/scene.rs` | **仅测试**（`#[cfg(test)]`）：公开面不暴露内部槽位布局 |
| `GeometryId` **不可**从公开 API 构造 | `paint/geometry.rs`（P02） | `GeometryStore` 属 P09 ⇒ P03 的 `NodeBuilder::geometry` 只能由 crate 内测试句柄驱动；应用层在 P09 之前拿不到 `GeometryId`（见 §19 注记） |
**明确不扩充**：`Arena`/`ArenaKey`/`next_generation`、`StateStore`/`SignalSlot`/`SignalValue`/`DerivedSlot`/`DerivedStore`、
`Owner`/`OwnerSet`/`DependencyGraph`/`ReadTracker`/`EvalStack`/`EvalGuard`、`Lane`/`MAX_CLIP_DEPTH`（后两者 P02 已定 `pub(crate)`）。

### 4.3 每个 `pub(crate)` item 与其**非 test 消费者**（dead_code 审计表）

> 规则：`pub(crate)` item 必须在**非 test 构建**里被调用，否则 `cargo clippy --all-targets -- -D warnings` 失败（P02 §3.2 的 C1/C2）。
> 没有消费者的能力一律**不写**；表内「消费者」列必须能在 P03 的代码里找到调用点。

| 文件 | item | 可见性 | 非 test 消费者 |
|---|---|---|---|
| `node/id.rs` | `Arena<T, K>` | `pub(crate)` | `node::tree::NodeTree`（`arena` 字段）、`paint::scene::Scene`（`lanes` 字段） |
| | `ArenaKey`（trait） | `pub(crate)` | `Arena` 的 bound；`impl ArenaKey for NodeId`（本文件）、`for LaneId`（scene.rs） |
| | `Arena::{new, insert_with, remove, get, get_mut, values_mut, len}` | `pub(crate)` | 见 §6.2 的逐个消费者列 |
| | `next_generation` | `pub(crate)` | `Arena::insert_with/remove`、`Scene::new_lane/release_lane` |
| `node/signal.rs` | `StateStore` 及其方法 | `pub(crate)` | `App`（字段 + `signal`/`derived`/`transient`/`write`）、`ReadCtx`（读） |
| | `SignalValue` / `SignalSlot` / `DerivedSlot` | `pub(crate)` | `StateStore` 的方法体 |
| `node/dependency.rs` | `Owner` / `OwnerSet` | `pub(crate)` | `DependencyGraph`、`ReadCtx`（`owner` 字段）、`App::refresh_styles` |
| | `DependencyGraph::{new, dependents_mut, replace_deps, detach, remove_signal}` | `pub(crate)` | `App::{write, close_window}`、`eval_derived`/`refresh_styles`（`replace_deps`） |
| | `ReadTracker` | `pub(crate)` | `ReadCtx::read`（`record`）、`eval_derived`/`refresh_styles`（取 deps） |
| | `EvalStack` | `pub(crate)` | `ReadCtx::read_derived`（环检测）、`EvalGuard`（`Drop` 出栈） |
| | `EvalGuard`（`Drop` guard） | `pub(crate)` | `ReadCtx::eval_derived`（panic 安全的入栈/出栈，§8.5） |
| | `NodeKind::is_leaf` | `pub(crate)` | `dirty::depends_on_children`、`NodeBuilder::build` 的 debug 断言 |
| `node/dirty.rs` | `depends_on_children` / `axis_depends_on_children` | `pub(crate)` | `NodeTree::mark_layout` |
| `node/tree.rs` | `NodeTree::{new, root, get, order, append_child, remove}` | **`pub`** | 应用层/P04/P08（`build` 前后） |
| | `NodeTree::{insert_child, mark_paint, mark_layout, mark_a11y, mark_structure, clear_paint, set_style_patch, set_deps, take_bind_style, put_bind_style, deps, any_node}` | `pub(crate)` | `NodeBuilder::build`、`App::write`、`App::refresh_styles`（含 `any_node` 汇总报告）、`NodeTree::append_child/remove` 内部、`Window::set_scale`、`App::close_window`（读 `deps`） |
| | `Node` 的只读访问器 | **`pub`** | `Probe`（P08）、`App::write`（`style_patch().affects_layout`）、`depends_on_children`（`style()`/`kind()`） |
| `paint/scene.rs` | `Lane::clear` | `pub(crate)` | `Scene::clear_frame` |
| | `Scene::empty` | `pub(crate)` | `Window::new`（唯一的 `Scene` 构造点；应用层不可构造，见 §13.2） |
| | `Scene` 的读/写方法 | **`pub`** | `Window::scene()`（读）、P06/P07（写）；P03 无 crate 内写调用者 → 依裁决 ③（§23） |
| `window.rs` | `Window::new` | `pub(crate)` | `App::create_window` |
| `app.rs` | `App` 私有字段 | — | — |
| | `StylePassReport` | **`pub`** | `App::refresh_styles` 的返回类型（读它的字段的是应用层/P06） |
| `Cargo.toml` | — | — | — |

**结论**：表内每个 `pub(crate)` item 都有至少一个非 test 调用点；`Scene` 的写入面是**唯一**的 `pub` 例外（§20 裁决 ③）。

### 4.4 推迟项与接手 Phase

| 推迟项 | 现状 | 接手 Phase | 接手判据 |
|---|---|---|---|
| `Application` / `AppHandle` / `PlatformDispatcher` / `FrameScheduler` | 不落地（无消费者） | **P08/P09** | 事件循环存在；`PlatformDispatcher::wake` 是「唤醒」的唯一实现。**P09 追加 `App::handle() -> AppHandle`（纯新增，`runtime_handle` 保名不动）** |
| 帧状态机（`Idle→Pending→Drain→…→WaitGpu`）、`begin_frame`/`end_frame` | 不落地 | P06 | paint pass 存在；`REUSABLE` 的帧初重置与 `damage` 清零在那里做 |
| `Children` / `Slot` / `NodeHandle` | 不落地 | P08 | `base_ui` 的声明式建树需要它们 |
| `NodeKind::{Text, Overlay, VirtualList}`、`bind_text`、`bind_a11y` | 不落地 | P05/P11/P13 | 对应载荷类型（`TextStyle`/`A11yPatch`/`VirtualList`）落地 |
| `Node::{a11y, clip}` 字段、`ClipId`、`A11yNodeId` | 不落地 | P11/P06 | 类型存在且有写者 |
| `Style` 的 `corner_radius`/`clip`/`shadow`/`font_family` | 不落地 | P06 | style→paint 的消费者出现（§10.1） |
| `set_layout`（布局写回 + 新 AABB 并入 damage）、`mark_paint_subtree` | 不落地 | P04 | taffy 计算完成，能对比新旧 rect |
| `Scene::lane_mut`（直接拿 `&mut Lane`） | 不落地（`Scene::push` 覆盖 P03 需求） | P06/P07 | 编码器需要 `clip_depth` 等 lane 内部状态 |
| `NodeTree::begin_frame`（REUSABLE 重置 + damage 清零） | 不落地 | P06 | 帧存在 |
| `a11y`/`Geometry` 的 `GEOMETRY` 脏位写入 | 位已定义，无写者 | P08 | `set_geometry_transform` 存在 |

---

## 5. 所有权与公开面（`App` / `Window` / `NodeTree` / `Node`）

### 5.1 `App`（`src/app.rs`）

**结论**：`App` 持有状态存储、依赖图、tokio 句柄与全部窗口；**`Application`/`AppHandle` 本阶段不落地**（裁决 ①，→ S-24）。

```rust
/// 引擎的应用句柄：唯一持有 UI 可变状态的对象（§1.4「所有可变 UI 状态只在主线程」）。
///
/// `App` 不是 `Send`：`StateStore` 的 `Box<dyn Any>` 与 `Window` 内的 `NodeTree` 都含非 `Send` 数据，
/// 编译器因此阻止跨线程修改（§1.4）。**不得**把 `Box<dyn Any>` 改成 `Box<dyn Any + Send>`。
pub struct App {
    /// 信号 / 派生 / 瞬态值的唯一存储。
    store: StateStore,
    /// `SignalId → OwnerSet` 的反查表：写入时只失效真正的读者（§3.4 的最小失效）。
    graph: DependencyGraph,
    /// 单一 tokio runtime 的显式句柄（D21 / I13）。主线程**只经它**提交任务（§9.2）。
    ///
    /// 字段名 `runtime`（不是 `handle`）：为 P09 的 `App::handle() -> AppHandle` 留出名字（裁决 ①）。
    runtime: tokio::runtime::Handle,
    /// 窗口表：键是引擎铸出的 `WindowId`（单调递增、永不复用）。
    windows: ahash::HashMap<WindowId, Window>,
    /// 下一个 `WindowId`（u32 空间，40 亿窗口足够；不复用 → 陈旧句柄天然返回 `None`）。
    next_window_id: u32,
    /// 派生求值栈（环检测，§8.6）。只在 `ReadCtx` 求值期非空。
    eval_stack: EvalStack,
    /// 「需要唤醒帧」请求（§3.5 的 `Invalidate → wake`）。P08 由 `PlatformDispatcher::wake` 取代。
    wake_requested: bool,
}

impl App {
    /// `runtime` 由 composition root 传入（§9.2：`main` 建 runtime 后 `runtime.handle().clone()`）。
    ///
    /// # Panics: none
    #[must_use]
    pub fn new(runtime: tokio::runtime::Handle) -> Self;

    /// 注册一个普通信号（进入依赖图）。
    pub fn signal<T: Clone + 'static>(&mut self, init: T) -> Signal<T>;

    /// 注册一个派生值（**惰性**求值，不进依赖图直到被读）。
    pub fn derived<T: Clone + 'static>(
        &mut self,
        compute: impl Fn(&mut ReadCtx<'_>) -> T + 'static,
    ) -> Derived<T>;

    /// 注册一个瞬态值（**不登记依赖、不失效任何节点**，§3.7）。
    pub fn transient<T: Clone + 'static>(&mut self, init: T) -> Transient<T>;

    /// 写普通信号。返回 `value` 是否与旧值不同（**相等不失效**，§7.5）。
    ///
    /// `Clone` 与 `PartialEq` 的约束与 `App::signal` 保持一致：同一个 `T` 在所有入口的 bound 相同
    /// （不产生「能读不能写」的类型）。**短路只比较、不克隆**（`owned_ref`）。
    pub fn write<T: Clone + PartialEq + 'static>(&mut self, signal: &Signal<T>, value: T) -> bool;

    /// 写瞬态值。**永不**触发失效或唤醒。
    pub fn write_transient<T: 'static>(&mut self, transient: &Transient<T>, value: T);

    /// 取走并清零「需要唤醒帧」请求（§14.3）。P08/P09 由 `PlatformDispatcher::wake` 取代。
    pub fn take_wake_request(&mut self) -> bool;

    /// P03 的**样式求值 pass**（§14.4）：只重算该窗口内被标 `PAINT` 的 `bind_style`，
    /// 更新 `style_patch` / `deps` / `dirty` / damage AABB，**不产生任何 Scene 图元**。
    ///
    /// `None` = 没有这个 `WindowId`（P03 没有「未知窗口」的错误变体，见 §16）。
    pub fn refresh_styles(&mut self, window: WindowId) -> Option<StylePassReport>;

    /// headless 建窗。P03 不创建任何平台资源。
    pub fn create_window(&mut self, options: WindowOptions) -> WindowId;

    /// # Panics: none
    #[must_use]
    pub fn window(&self, id: WindowId) -> Option<&Window>;

    /// # Panics: none
    #[must_use]
    pub fn window_mut(&mut self, id: WindowId) -> Option<&mut Window>;

    /// 关窗：移除窗口、清理依赖图中属于该窗口的 `Owner`（§14.5）。返回窗口是否存在。
    pub fn close_window(&mut self, id: WindowId) -> bool;

    /// 单一 runtime 句柄的**借用**（不 clone）。应用层据此提交异步任务（§9.2）。
    ///
    /// **命名**：叫 `runtime_handle` 而不是 `handle`，把 `handle()` 留给 P09 的
    /// `App::handle() -> AppHandle`（跨线程回投句柄）—— 那时是纯新增，零破坏（裁决 ①）。
    #[must_use]
    pub fn runtime_handle(&self) -> &tokio::runtime::Handle;

    /// `windows` 的只读迭代（P08/P09 帧驱动需要按窗口遍历）。
    pub fn windows(&self) -> impl Iterator<Item = (WindowId, &Window)> + '_;
}

/// `App::refresh_styles` 的报告（§14.4）。`Copy`：P06 的 `FrameRecord` 可直接存。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StylePassReport {
    /// 实际求值的 `bind_style` 闭包个数（不区分「补丁是否变化」）。
    pub evaluated: usize,
    /// pass 结束后该窗口内是否仍有节点带 `LAYOUT`（含冒泡）。
    pub layout_dirty: bool,
    /// pass 结束后该窗口内是否仍有节点 `!REUSABLE`（= P06 需要重建 lane）。
    ///
    /// **P03 恒为 `true`**（N1）：`REUSABLE` 在 P03 **只有清除者**（`mark_*`），没有设置者——它的帧初重置
    /// 属 P06 的 `NodeTree::begin_frame`。因此 P03 里除空窗口外，该字段恒 `true`；断言它等于 `false`
    /// 只可能出现在「还没有任何节点」的窗口上。P06 接手后语义才有区分度。
    pub paint_dirty: bool,
    /// pass 结束后 `App::take_wake_request()` 会返回的值（**本报告不消费它**）。
    pub wake_requested: bool,
}
```

| 决策 | 结论 | 理由 |
|---|---|---|
| `Application` / `AppHandle` | **推迟 P08/P09** | `Application::run` 需要平台事件循环（P09/P12），`AppHandle` 需要 `PlatformDispatcher`（P09）。P03 无消费者 → 落地即 `dead_code`（→ S-24） |
| runtime 句柄的 API 名 | **`runtime_handle(&self) -> &tokio::runtime::Handle`**（借用，不 clone） | 不占用 `handle()` 这个名字：P09 加 `handle() -> AppHandle` 时是**纯新增**，零破坏（裁决 ①）。字段名同步为 `runtime` |
| `App::write` 的 `Clone + PartialEq` 约束 | 只在 `write` 上；`Signal<T>`/`ReadCtx::read` 只要 `Clone` | `PartialEq` 服务等值短路（§7.5）；`Clone` 让「同一 `T` 在所有入口的 bound 相同」（`signal`/`read` 都要 `Clone`）。`Signal<T>` 本身**无约束** → `T: !PartialEq` 的值也能持有（只是不能用 `write` 写） |
| `App::windows()` | 提供 | 否则 `windows` 字段在 crate 外无法遍历，而 `Probe`/`FrameRecord`（P08）只经 `Window` 拿不到全量窗口；P09 帧驱动同样需要 |
| `refresh_styles` 按窗口 | `(&mut self, window: WindowId) -> Option<StylePassReport>` | ①帧是**按窗口**推进的（P06/P09），一次给全量窗口会强迫调用方忽略「这一帧只画哪个窗口」；②P03 没有「未知 `WindowId`」的错误变体（冻结表不含），`Option` 是唯一诚实的表达（`None` = 窗口不存在） |
| `StylePassReport` 为什么是公开类型 | 它是 `refresh_styles` 的返回值 + P06 `FrameRecord` 的字段 | 在 §1.3 白名单的传递闭包内（`App`/帧记录带出），登记在 §4.2 |

### 5.2 `Window`（`src/window.rs`）

```rust
/// 窗口句柄：从 **1** 起单调递增、**永不复用**（陈旧 id 落回 `None`，不需要代际；`0` 保留为非法句柄，因此 `Option<WindowId>` 不会与「不存在」混淆）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WindowId(u32);

/// 建窗参数。P03 只保存，不驱动平台（P12 才消费）。
#[derive(Clone, Debug, PartialEq)]
pub struct WindowOptions {
    /// 标题（`SmolStr`，AGENTS §2.2）。
    pub title: SmolStr,
    /// 初始逻辑尺寸。
    pub size: Size,
    /// 初始 DPI 比例（由平台在 P12 传入；headless 默认 `Scale::ONE`）。
    pub scale: Scale,
}

impl Default for WindowOptions {
    fn default() -> Self;   // title: ""、size: 800×600、scale: Scale::ONE
}

/// 一个窗口的全部状态：节点树 + 绘制场景 + DPI + 参数。
pub struct Window {
    id: WindowId,
    options: WindowOptions,
    tree: NodeTree,
    scene: Scene,
    scale: Scale,
}

impl Window {
    /// 消费者：`App::create_window`。
    pub(crate) fn new(id: WindowId, options: WindowOptions) -> Self;

    #[must_use] pub fn id(&self) -> WindowId;
    #[must_use] pub fn tree(&self) -> &NodeTree;
    /// 应用层建树的唯一入口（`NodeBuilder::build` 的 `&mut NodeTree` 来自这里）。
    pub fn tree_mut(&mut self) -> &mut NodeTree;
    /// 只读句柄（S-10 / S-33：应用层**拿不到** `&mut Scene`；`Scene` 也**没有**公开构造器）。
    #[must_use] pub fn scene(&self) -> &Scene;
    #[must_use] pub fn options(&self) -> &WindowOptions;
    #[must_use] pub fn scale(&self) -> Scale;

    /// DPI 变化（P12 的事件处理调用它）：更新比例并标根 `LAYOUT|PAINT`（§3.4「窗口尺寸/DPI → 根 LAYOUT」）。
    ///
    /// **不**置 `App` 的唤醒请求：DPI 事件本身来自平台，平台总是会调度一帧（P12）。
    pub fn set_scale(&mut self, scale: Scale);
}
```

| 决策 | 结论 | 理由 |
|---|---|---|
| `Window.scale` 与 `WindowOptions.scale` 重复？ | 保留两份，`Window::new` 用 options 初始化 | `options` 是「创建参数快照」（P12 重建窗口时复读），`scale` 是当前生效值（DPI 可变）。`Window::set_scale` 只改后者 |
| `set_scale` 是否校验 | 不校验（`Scale` 构造即校验，P01 §6.3.2） | `Scale::new` 已返回 `Option`，平台层负责转 `InvalidDpiScale` |
| `Window::tree_mut` 交出 `&mut NodeTree` 的安全性 | 允许 | 应用层只能改结构与样式（`Style` 是值、`build` 只追加）；`Node` 字段全私有，`Signal` 值只能经 `App::write`（I6） |
| `Window` 是否 `Drop` 时清理依赖图 | 否 | `Drop` 拿不到 `&mut DependencyGraph`；清理在 `App::close_window`（唯一入口） |
| `Scene` 的可构造性 | **不可**（`pub(crate) fn empty()`，见 §13.2） | 裁决 ③ 的约束：`Scene` 不实现 `Clone`/`Default`、无公开构造器 → 应用层既不能克隆也不能伪造场景，只能读 `Window::scene()` |

### 5.3 `NodeTree` / `Node` 公开到什么程度

`Probe`（P08，§6.2）必须能**直接**查 `LayoutBox`/`Style`/`lanes`，因此 `NodeTree` 给**只读**访问器；写入全部留在
`pub(crate)`（除结构操作 `append_child`/`remove`，那是应用层声明式建树的一部分）。

```rust
impl NodeTree {
    /// 建树：创建唯一的 root 节点（`NodeKind::Box`，`Style::default()`）。
    #[must_use] pub fn new() -> Self;
    #[must_use] pub fn root(&self) -> NodeId;
    /// 节点只读查询；陈旧句柄 → `Err(EngineError::StaleNode)`。
    pub fn get(&self, node: NodeId) -> Result<&Node, EngineError>;
    /// 稳定绘制序（DFS 前序，§11.5）。含 root，长度 = 存活节点数。
    #[must_use] pub fn order(&self) -> &[NodeId];
    /// 把已存在的节点挂到 `parent` 末尾（若它已有父节点则**移动**，§11.4）。
    pub fn append_child(&mut self, parent: NodeId, child: NodeId) -> Result<(), EngineError>;
    /// 卸载节点及其子树（不能是 root；root → `Err(StaleNode)`）。
    pub fn remove(&mut self, node: NodeId) -> Result<(), EngineError>;
}

/// `NodeTree::new()` 无参数 → 必须实现 `Default`（clippy `new_without_default`；`#[allow]` 被 AGENTS §4 禁止）。
impl Default for NodeTree {
    fn default() -> Self { Self::new() }
}

impl Node {
    // 全部字段私有；只读访问器（`Probe`/帧记录/探针断言）    #[must_use] pub fn id(&self) -> NodeId;
    #[must_use] pub fn parent(&self) -> Option<NodeId>;
    #[must_use] pub fn children(&self) -> &[NodeId];
    #[must_use] pub fn kind(&self) -> &NodeKind;
    #[must_use] pub fn style(&self) -> &Style;
    #[must_use] pub fn style_patch(&self) -> &StylePatch;
    #[must_use] pub fn layout(&self) -> LayoutBox;
    #[must_use] pub fn prev_layout(&self) -> LayoutBox;
    /// 本帧累积的逻辑像素 damage AABB（§9.4；P06 才转物理矩形）。
    #[must_use] pub fn damage(&self) -> Rect;
    #[must_use] pub fn lanes(&self) -> &[LaneId];
    #[must_use] pub fn dirty(&self) -> DirtyFlags;
}
```

| 决策 | 结论 | 理由 |
|---|---|---|
| `Node` 字段为何全私有 | I6：应用层「不得持有 `&mut Node`」；只读访问器已覆盖 `Probe` 的全部需求（§6.2 的 `bounds`） | 字段私有 + 只读访问器 = 编译期无法改节点 |
| 为什么给 `NodeTree::get` 而不是 `get_mut` | `get_mut` 会让 `deps`/`dirty`/`lanes` 可被应用层篡改 | 结构变更已有 `append_child`/`remove` 两个受控入口 |
| `append_child`/`remove` 为什么是 `pub` | 应用层建/删子树是声明式 API 的一部分（§1.3 的 `Children` 语义），且 `NodeBuilder::build` 内部复用 `append_child` | §4.3 已列其消费者（`insert_child`） |

### 5.4 线程模型

| 规则 | 实现 |
|---|---|
| `App`/`Window`/`NodeTree`/`StateStore`/`Scene` 非 `Send`（编译期阻止跨线程改 UI） | `StateStore` 的 `Box<dyn Any>` 与 `ReadCtx` 的 `&mut` 已使 `App: !Send`；`Node` 内 `Box<dyn Fn>` 无 `+ Send` |
| 主线程不得 `Handle::current()` / `spawn_blocking` | `clippy.toml` 的 `disallowed-methods`（§2.2）；P03 代码零 `tokio` 调用，只有类型 |
| 单一 runtime | `App::runtime_handle()` 是唯一句柄来源（§2.2、S-37） |

---

## 6. `node/id.rs`

### 6.1 `NodeId`

```rust
/// 带代际的节点句柄（§3.2）。`index` 是 arena 槽位，`generation` 防悬垂。
///
/// 尺寸 8 B；`Copy + Eq + Hash`（可做 map/set 键）。字段私有：句柄只能由 `Arena` 铸出，
/// 应用层无法用结构体字面量伪造（S-20 的同类理由）。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId {
    index: u32,
    generation: u32,
}

impl NodeId {
    #[must_use] pub const fn index(self) -> u32;
    #[must_use] pub const fn generation(self) -> u32;
    /// `(generation as u64) << 32 | index as u64`。
    ///
    /// 消费者：§9.1 的结构化日志字段（`FrameIndex` + `NodeId` + `SignalId` 需要单值可打印的标识）；
    /// P06 帧记录与 P08 `Probe` 的键。`index` 在低 32 位 → 同一节点的多次装卸只差高位。
    #[must_use] pub const fn to_bits(self) -> u64;

    /// 仅测试构造（S-20 的形态：不扩大公开面）。
    #[cfg(test)]
    pub(crate) const fn for_test(index: u32, generation: u32) -> Self;
}

impl std::fmt::Debug for NodeId { /* `NodeId(n{index}.{generation})` */ }
```

| 决策 | 结论 | 理由 |
|---|---|---|
| 是否加 `Ord` | **不加** | 无消费者（不用它做排序键/`BTreeMap`）；`order: Vec<NodeId>` 的顺序是绘制序，不是 id 序 |
| `Debug` 手写 | 手写 `n{index}.{generation}` | 诊断可读性；`derive(Debug)` 打印 `NodeId { index: 3, generation: 1 }` 太长，帧记录会刷屏 |
| `to_bits` 的顺序 | `generation` **高**、`index` **低** | 同一节点的历次代际在数值上连续递增（诊断/排序都自然） |

### 6.2 `Arena<T, K>`（`pub(crate)`）

```rust
/// arena 键：`NodeId` 与 `LaneId` 共用同一套槽位/自由表/代际规则（两处实例化，非预防性抽象）。
pub(crate) trait ArenaKey: Copy {
    fn from_parts(index: u32, generation: u32) -> Self;
    fn parts(self) -> (u32, u32);
}

/// 代际固定为 `1..=u32::MAX`：**0 永不是合法代际**，因此 `remove` 后回绕也不会复活旧句柄。
pub(crate) fn next_generation(generation: u32) -> u32 {
    if generation == u32::MAX { 1 } else { generation + 1 }
}

/// 槽位 + 自由表的对象池。`get`/`get_mut` 校验代际 → 陈旧句柄一律 `None`（→ `StaleNode`）。
///
/// `Debug`（N5）：`Scene` 要 `derive(Debug)`，因此 `Arena<Lane, LaneId>` 与 `Slot<Lane>` 必须可 `Debug`；
/// `Arena<Node, NodeId>` 复用同一 derive（`Node` 手写 `Debug`，见 §11.1）。
#[derive(Debug)]
pub(crate) struct Arena<T, K: ArenaKey = NodeId> {
    slots: Vec<Slot<T>>,
    free: Vec<u32>,
}

#[derive(Debug)]
struct Slot<T> {
    generation: u32,          // 1..=u32::MAX
    value: Option<T>,         // None = 已释放，槽位在 free 里
}

impl<T, K: ArenaKey> Arena<T, K> {
    pub(crate) fn new() -> Self;
    /// 插入并由 arena 铸出 id（节点需要把自己 id 存进字段，故用回调形式，避免「先拿到 id 再回填」的中间态）。
    pub(crate) fn insert_with(&mut self, make: impl FnOnce(K) -> T) -> K;
    /// 释放并返回旧值；陈旧句柄 → `None`（幂等）。
    pub(crate) fn remove(&mut self, key: K) -> Option<T>;
    pub(crate) fn get(&self, key: K) -> Option<&T>;
    pub(crate) fn get_mut(&mut self, key: K) -> Option<&mut T>;
    /// 遍历所有存活值（消费者：`Scene::clear_frame`）。
    pub(crate) fn values_mut(&mut self) -> impl Iterator<Item = &mut T>;
    /// 存活数（消费者：`Scene::lane_count`）。
    pub(crate) fn len(&self) -> usize;
}
```

**逐方法消费者（非 test）**

| 方法 | `Arena<Node, NodeId>` 的消费者 | `Arena<Lane, LaneId>` 的消费者 |
|---|---|---|
| `new` | `NodeTree::new` | `Scene::empty`（`Window::new` 的唯一构造点） |
| `insert_with` | `NodeTree::insert_child` | `Scene::new_lane` |
| `remove` | `NodeTree::remove`（递归子树卸载） | `Scene::release_lane` |
| `get` | `NodeTree::get`/`order` 校验、`mark_*` 的前置校验 | `Scene::lane_primitives`/`Scene::push` |
| `get_mut` | `mark_paint`/`mark_layout`/`mark_a11y`/`mark_structure`/`clear_paint`/`set_style_patch`/`set_deps`/`take_bind_style` | `Scene::push` |
| `values_mut` | — | `Scene::clear_frame` |
| `len` | — | `Scene::lane_count` |

> `Arena::len` 与 clippy `len_without_is_empty`：该 lint 只对**导出**类型生效，`Arena` 是 `pub(crate)` → 不触发。
> 若实现时仍报（clippy 版本差异），把方法改名为 `live_len()`（不引入无消费者的 `is_empty`）。

**不变量（必须由实现守住，并被单测覆盖）**

| # | 不变量 | 强制点 |
|---|---|---|
| A1 | 同一时刻 `free` 内无重复 index | `remove` 只在「代际匹配且值存在」时 push；`insert_with` 只 `pop` |
| A2 | 存活槽位的 `generation` ∈ `1..=u32::MAX`；释放后立即 `next_generation` | `remove`；`next_generation` |
| A3 | `free` 里的 index 对应 `value == None` | `remove` 先 `take` 再 push |
| A4 | 陈旧句柄的 `get/get_mut/remove` 全部返回 `None`，**不得**命中复用后的新值 | `get` 的 `generation == slot.generation && value.is_some()` 双重校验 |
| A5 | `len()` = `Some` 槽位数 | `insert_with`/`remove` 维护（`saturating_sub` 防下溢） |

### 6.3 卸载后访问 `StaleNode` 的路径

```text
NodeTree::get(id) → arena.get(id):
    None  → Err(EngineError::StaleNode { node: id })     // 唯一映射点（tree.rs）
    Some  → Ok(&Node)

App::write → graph.dependents(signal) → Owner::Node{window, node}:
    window 不存在          → 该 owner 已死 → 从集合中 retain 掉
    tree.mark_paint(node) 返回 false（代际不匹配或槽位空）→ 同上
```

**关键**：`StaleNode` 的**唯一构造点**是 `NodeTree::get`（与 `NodeTree::remove`/`append_child`/`insert_child` 的参数校验，它们同样走 `self.get`）。
`App::write` 的失效路径**不**构造错误（写路径不能因为陈旧 owner 失败）：它把死 owner 当作「已无读者」直接剔除（§8.2 的 `retain`）。

---

## 7. `node/signal.rs`（状态模型）

### 7.1 id 设计：稠密、无代际、永不复用

```rust
/// 信号 / 瞬态值的稠密索引（`values: Vec<SignalSlot>` 的下标）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SignalId(u32);

/// 派生值的稠密索引（`derived: Vec<DerivedSlot>` 的下标）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DerivedId(u32);

impl SignalId  { #[must_use] pub const fn index(self) -> u32; #[must_use] pub const fn to_bits(self) -> u64; }
impl DerivedId { #[must_use] pub const fn index(self) -> u32; #[must_use] pub const fn to_bits(self) -> u64; }
```

| 决策 | 结论 | 理由 |
|---|---|---|
| `SignalId` 是否带代际 | **不带**（纯 `u32`） | P03 的 `StateStore` **永不删除单个条目**（值的生命周期 = `App` 的生命周期，§7.6 的裁决）。没有删除就没有「槽位复用」，代际无用（AGENTS §1.2）。设计 §3.9 的「引用计数 / 显式 drop」在 `Signal<T>: Copy` 下**不可实现**（Copy 句柄无法计数）→ 记录为 S-25 |
| `values` 用 `Vec` 还是 `HashMap` | **`Vec<SignalSlot>`** | 稠密索引 + 无删除 → 下标即 id；`Vec` 的局部性远好于 `HashMap`（§3.2 草图的 `HashMap<SignalId, Box<dyn Any>>` 被本决策取代） |
| `Transient<T>` 与 `Signal<T>` 是否共用一个 id 空间 | **共用**（都进 `values`，靠 `SignalValue` 变体区分） | 设计 §3.2 的 `SignalValue::{Owned,Derived,Transient}` 正是这个形态；共用后 `versions`/失效代码只有一套 |
| `Derived` 是否也是 `SignalValue` 变体 | **是**（`SignalValue::Derived(DerivedId)`） | 派生值需要一个**可被依赖的句柄 id**：节点读派生时登记 `dependents[handle_id]`，失效级联才能沿 `SignalId` 统一传播（§8.2）。派生值的**缓存**另存 `derived: Vec<DerivedSlot>`（`values` 里只放 `DerivedId` 指针） |

### 7.2 句柄

```rust
/// 普通信号句柄：只有 id，值在 `StateStore`（§3.2「节点不持有值」）。
///
/// `Copy` 无条件成立：`PhantomData<fn() -> T>` 不要求 `T: Copy`，也不携带 drop-check/auto-trait 负担
/// （`fn() -> T` 恒 `Copy + Send + Sync`）。
pub struct Signal<T> { id: SignalId, _marker: PhantomData<fn() -> T> }
impl<T> Clone for Signal<T> { fn clone(&self) -> Self { *self } }
impl<T> Copy for Signal<T> {}

/// 派生值句柄（惰性求值，§7.4）。
pub struct Derived<T> { id: DerivedId, _marker: PhantomData<fn() -> T> }
impl<T> Clone for Derived<T> { … }  impl<T> Copy for Derived<T> {}

/// 瞬态值句柄（滚动偏移/悬浮态，§3.7）。
pub struct Transient<T> { id: SignalId, _marker: PhantomData<fn() -> T> }
impl<T> Clone for Transient<T> { … }  impl<T> Copy for Transient<T> {}

// 三个句柄都提供 crate 内 id 访问器（字段私有）：
//   消费者：`App::{write, write_transient}`、`ReadCtx::{read, read_transient, read_derived}`、`App::derived`
impl<T> Signal<T>    { pub(crate) const fn id(self) -> SignalId; }
impl<T> Derived<T>   { pub(crate) const fn id(self) -> DerivedId; }
impl<T> Transient<T> { pub(crate) const fn id(self) -> SignalId; }
```

| 决策 | 结论 | 理由 |
|---|---|---|
| `Copy` 用 `derive` 还是手写 | **手写**（`PhantomData<fn() -> T>`） | `#[derive(Copy)]` 会追加 `T: Copy` 约束（derive 的经典坑），导致 `Signal<String>` 不是 `Copy` |
| 句柄是否带类型标识（`TypeId`） | **不带** | 类型由泛型参数静态保证；`TypeId` 只在 downcast 时由 `T` 静态求出（§7.3） |
| `Debug` | 手写（`Signal(#3)`） | 不打印 `PhantomData`；诊断需要 |
| 句柄上是否提供 `Signal::set(&self, v)` 这类写方法 | **不提供**（P03 唯一的写入口是 `App::write`） | I6「状态只能经 `Signal` 写入」的落点是 `App::write`（它需要 `&mut App` 才能做失效）；句柄级写方法拿不到依赖图。若将来追加，其约束**必须**是 `T: Clone + PartialEq + 'static`（与 `App::write` 一致，见裁决 ②） |

### 7.3 `StateStore` 与类型擦除

```rust
pub(crate) struct StateStore {
    values: Vec<SignalSlot>,
    derived: DerivedStore,
}

pub(crate) struct SignalSlot {
    value: SignalValue,
}

pub(crate) enum SignalValue {
    /// 普通信号：类型擦除的值。
    Owned(Box<dyn Any>),
    /// 派生值的**句柄槽**：值在 `DerivedSlot.cache`。
    Derived(DerivedId),
    /// 瞬态值：类型擦除的值；读不登记、写不失效。
    Transient(Box<dyn Any>),
}

pub(crate) struct DerivedSlot {
    /// 该派生值在信号表里的**句柄槽** id（`SignalValue::Derived(did)` 那一格）。
    ///
    /// 依赖图的键只有 `SignalId` 一种：节点/派生读派生时登记的是**它**（§8.2 的「句柄 id」），
    /// 失效级联也沿它传播。`push_derived` 一次写入，之后只读（`StateStore::derived_handle` 读它）。
    handle: SignalId,
    /// 计算闭包。求值期 `take()` 出去（避免与 `&mut self` 别名），求值后放回。
    compute: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> Box<dyn Any>>>,
    /// 缓存值（`Some` 且 `dirty == false` 时有效）。
    cache: Option<Box<dyn Any>>,
    /// 失效标志（push 模型，§7.5）。
    dirty: bool,
    /// 上次求值登记的依赖（差分清理用，§8.3）。与 `ReadTracker` 同容量（**B1**）。
    deps: SmallVec<[SignalId; 8]>,
}
```

**`DerivedStore` 的数据模型与复杂度（B2）**

| 项 | 内容 |
|---|---|
| 存储 | `slots: Vec<DerivedSlot>`；`DerivedId(u32)` 是**稠密下标**（无删除、无自由表） |
| `DerivedId ↔ 句柄` | 双向：`DerivedStore::handle(did)` 读 `DerivedSlot.handle`；反向（`SignalId → DerivedId`）由信号槽的 `SignalValue::Derived(did)` 提供。两条边在 `push_derived` 里**一次写入**，之后不变 |
| 复杂度 | `is_dirty`/`handle`/`deps` = O(1)；`take_compute`/`put_compute`/`store_value`/`set_deps` = O(1)（`Option::take` / 搬 `Box`）；`value::<T>` = O(1) + 一次 `Clone`（值本体）；`with_capacity` = O(n) 一次 |
| 为什么 `deps` 是 `SmallVec<[SignalId; 8]>` | 与 `ReadTracker` 的容量一致，`set_deps` 直接收 `SmallVec`（**不做 `from_slice` 拷贝**）；`SignalId` 4 B → 8 内联 = 32 B，典型派生读 1–5 个信号 |

**拆分借用与访问器（`split` / `DerivedStore`）**

`App::refresh_styles` 要同时「读信号表」与「写派生缓存」。`StateStore` 的字段必须私有（lint
`field_scoped_visibility_modifiers`），因此提供**唯一的拆分入口**与两个包装类型：

```rust
impl StateStore {
    /// 初始容量（不是契约；`App::new` 的消费者）。字段私有 → 只能经它构造。
    pub(crate) fn with_capacity(signals: usize, derived: usize) -> Self;
    /// 拆分借用：信号表只读 + 派生表可变（同一结构体的不交叠字段，Rust 允许）。
    /// 消费者：`App::refresh_styles`（构造 `ReadCtx`）。
    pub(crate) fn split(&mut self) -> (&[SignalSlot], &mut DerivedStore);

    // 写路径（消费者：`App::write` / `App` 的注册方法）
    pub(crate) fn push_owned<T: 'static>(&mut self, value: T) -> SignalId;
    pub(crate) fn push_transient<T: 'static>(&mut self, value: T) -> SignalId;
    pub(crate) fn push_derived(
        &mut self,
        compute: Box<dyn Fn(&mut ReadCtx<'_>) -> Box<dyn Any>>,
    ) -> (DerivedId, SignalId);
    pub(crate) fn set_owned<T: PartialEq + 'static>(&mut self, id: SignalId, value: T) -> bool;
    pub(crate) fn set_transient<T: 'static>(&mut self, id: SignalId, value: T);
    /// true = 本次由 clean 变 dirty（级联只传播一次）。
    pub(crate) fn mark_derived_dirty(&mut self, id: DerivedId) -> bool;
    pub(crate) fn derived_handle(&self, id: DerivedId) -> SignalId;
}

/// 派生表的唯一持有者（`StateStore::derived`）：封装 id→槽位映射与 `Option` 不变量。
pub(crate) struct DerivedStore { slots: Vec<DerivedSlot> }

impl DerivedStore {
    pub(crate) fn with_capacity(capacity: usize) -> Self;
    pub(crate) fn is_dirty(&self, id: DerivedId) -> bool;
    /// 求值期取走闭包（避免与 `&mut self` 别名）；`None` 只可能是求值重入（不变量破坏）。
    pub(crate) fn take_compute(&mut self, id: DerivedId)
        -> Option<Box<dyn Fn(&mut ReadCtx<'_>) -> Box<dyn Any>>>;
    pub(crate) fn put_compute(
        &mut self,
        id: DerivedId,
        f: Box<dyn Fn(&mut ReadCtx<'_>) -> Box<dyn Any>>,
    );
    /// `cache = Some(value)`、`dirty = false`。
    pub(crate) fn store_value(&mut self, id: DerivedId, value: Box<dyn Any>);
    /// 读缓存（downcast + clone）；`# Panics` 同 §7.3 的不变量断言。
    pub(crate) fn value<T: Clone + 'static>(&self, id: DerivedId) -> T;
    pub(crate) fn deps(&self, id: DerivedId) -> &[SignalId];
    pub(crate) fn set_deps(&mut self, id: DerivedId, deps: SmallVec<[SignalId; 8]>);
}
```

`SignalSlot` 的读访问器（消费者：`ReadCtx::read` / `read_transient`）：

```rust
impl SignalSlot {
    /// `SignalValue::Owned` 的 downcast + clone；类型不符 → panic（§7.3 的不变量断言）。
    pub(crate) fn owned<T: Clone + 'static>(&self) -> T;
    /// `SignalValue::Transient` 的 downcast + clone。
    pub(crate) fn transient<T: Clone + 'static>(&self) -> T;
}
```

> **为什么需要 `DerivedStore`**：`ReadCtx` 只持 `&mut DerivedStore`（而不是整个 `StateStore`），才可能同时持
> `&[SignalSlot]`（读信号）与 `&mut`（写派生缓存）。把「id → 槽位」的映射与 `Option::take` 的不变量收在一个类型里，
> 比在 `ReadCtx` 里裸索引 `Vec` 更安全，也让访问器有明确归属。

**类型擦除与 downcast 失败（结论）**

```rust
impl StateStore {
    /// 相等比较用的**借用**视图（不克隆）。消费者：`App::write` 的短路判断（§7.5）。
    ///
    /// 返回值 `None` 表示「不是 `Owned` 槽」或「类型不符」—— 前者是调用方错误（writer 必须确认是普通信号），
    /// 后者是不变量破坏；两者都映射为 panic（见下表）。
    pub(crate) fn owned_ref<T: 'static>(&self, id: SignalId) -> Option<&T>;
}
```

| 决策 | 结论 | 理由 |
|---|---|---|
| downcast 失败返回 `EngineError`？ | **不**。失败是**不变量破坏**（panic，且 `# Panics` 写进文档） | ①`Signal<T>` 只能由 `App::signal::<T>` 铸出，`SignalId` 永不复用 ⇒ id↔类型是**构造性双射**，失败只可能来自应用层 `unsafe` 伪造句柄；②返回 `Result` 会把 `?` 传染到每个 `bind_style` 闭包与 `ReadCtx::read`（§3.10 的签名是 `cx.read(&price)` 直接返回值，无 `?`）；③新增 `EngineError::ExpectedType` 会破坏 P01 §6.2.2 的 17 变体冻结表，且构造不出触发者（AGENTS §1.2 禁止无消费者能力） |
| 为什么 `expect` 而不是 `unreachable!` | `expect` 带固定文案 | `#[allow]` 禁止、panic 消息需可诊断；文案不含用户数据（AGENTS §3 的强类型错误要求针对**返回值**，此处是不变量断言） |
| 相等比较为何用 `owned_ref` 而非「读+克隆」 | 避免每次写都克隆一次值 | 写路径是高频（行情）；`owned_ref` 只借用，比较后立即释放 |
| 是否保留每槽位的 `version: u64` | **删除**（`SignalSlot` 只有 `value`） | 裁决 ② 的硬要求：「若没有读者则删除该字段，不要为测试保留」。P03 内没有任何**非 test** 读者：等值短路由值比较完成（不需要计数），`refresh_styles` 的报告只统计 `evaluated`/`layout`/`paint`/`wake`，帧记录属 P06。→ S-27 |

### 7.4 `Derived<T>` 的 API 与求值模型

```rust
impl App {
    pub fn derived<T: Clone + 'static>(
        &mut self,
        compute: impl Fn(&mut ReadCtx<'_>) -> T + 'static,
    ) -> Derived<T>;   // 只登记闭包与 id，**不求值**
}

impl ReadCtx<'_> {
    /// 读派生值：登记依赖 + 若脏则求值 + 返回缓存克隆。
    ///
    /// 唯一失败是环（`EngineError::CyclicDerived`）。
    pub fn read_derived<T: Clone + 'static>(&mut self, derived: &Derived<T>)
        -> Result<T, EngineError>;
}
```

**求值模型（惰性 + push 失效 + panic 安全的求值栈）**

```text
App::derived(compute):
    did    = 下一个 DerivedId
    handle = 分配 SignalSlot { value: Derived(did) }        // 句柄槽（供依赖图引用）
    derived.push(DerivedSlot {
        handle,                                            // B2：句柄 id 写进槽位（derived_handle 读它）
        compute: Some(box |cx| Box::new(compute(cx))),      // 类型擦除：T → Box<dyn Any>
        cache: None, dirty: true, deps: [],
    })
    Derived { id: did, _marker }

ReadCtx::read_derived(d) -> Result<T, EngineError>:              // 完整签名见下
    self.tracker.record(self.derived.handle(d))             // 下游（节点或另一个派生）依赖"句柄"
    if self.derived.is_dirty(d) {
        self.eval_derived(d)?                              // 惰性；失败（环）原样上抛，**不写缓存**
    }
    Ok(self.derived.value::<T>(d))                          // downcast + clone（不变量断言）

ReadCtx::eval_derived(d) -> Result<(), EngineError>:
    if self.eval_stack.contains(d) { return Err(CyclicDerived { derived: d }) }   // ① 环检测：**先于**取闭包
    let mut guard = EvalGuard::enter(&mut *self.eval_stack, d);   // ② 入栈 + Drop 出栈（panic 安全）
    let Some(compute) = self.derived.take_compute(d) else {
        debug_assert!(false, "派生值正在求值（环检测被绕过）");
        return Err(CyclicDerived { derived: d });           // 二道防线（不 panic，仍返回强类型错误）
    };
    let mut nested = ReadTracker::new();                    // ③ 本派生自己的读取集合
    let mut nested_ctx = ReadCtx {
        values: self.values, derived: self.derived, graph: self.graph,
        tracker: &mut nested, eval_stack: guard.stack_mut(), // 栈经 guard 转借（guard 仍负责出栈）
        owner: Owner::Derived(d),
    };
    let value: Box<dyn Any> = compute(&mut nested_ctx);      // ④ 求值（可递归 read / read_derived）
    drop(nested_ctx);                                        // ⑤ 先释放借用，再写缓存
    self.derived.put_compute(d, compute);
    self.derived.store_value(d, value);                      // ⑥ 只有成功返回才写 cache + dirty=false
    let old = self.derived.deps(d);                          // ⑦ 差分替换依赖（§8.3）
    self.graph.replace_deps(Owner::Derived(d), old, nested.as_slice());
    self.derived.set_deps(d, nested.into_inner());
    drop(guard);                                             // ⑧ 出栈（正常路径；panic 时由 Drop 完成）
    Ok(())
```

> **N9**：伪代码里的栈字段统一叫 `eval_stack`（与 `ReadCtx` 的字段名一致）；入栈时显式写 `&mut *self.eval_stack`
> （先重借用再交给 guard，避免把 `self` 的整段借用挪走）。

**`read_derived` 的完整签名与契约**

```rust
impl ReadCtx<'_> {
    /// 读一个派生值（§7.4）。
    ///
    /// 1. **登记依赖**：下游 owner 依赖该派生的**句柄槽** `SignalId`（不是它读过的信号）；
    /// 2. **惰性求值**：仅当 `dirty` 时求值；求值期间该派生在 `EvalStack` 上（环检测）；
    /// 3. 返回缓存值的克隆。
    ///
    /// # Errors
    /// - `EngineError::CyclicDerived { derived }`：两种情况都返回它 ——
    ///   ①**真环**：求值栈上再次遇到同一个 `DerivedId`；
    ///   ②**闭包 panic 后的残留**：该派生的 `compute` 已被 `take` 走（panic 展开时未放回），后续读命中
    ///   `take_compute == None` 的二道防线。
    ///   **因此 `CyclicDerived` 不能单独作为「存在环」的判据**（N8）：判环要看「同一 `App` 内的第一个错误是否
    ///   由 `eval_stack.contains` 产生」。两种情况都**不写缓存、不清 `dirty`**。
    ///
    /// # Panics
    /// - 缓存/闭包的类型与 `T` 不符（不变量破坏，见 §7.3）；
    /// - 派生闭包自身 panic（原样向上传播，见下表的「panic 安全」行）。
    ///
    /// # 部分状态（panic 之后，B-N8 固化）
    /// 派生闭包 panic 会让引擎进入一个**已定义但不完整**的状态，应用层必须知道：
    /// 1. 该派生的 `compute` 已被 `take` 走且**不会放回** ⇒ 之后每次读都返回 `CyclicDerived`（**永久**，
    ///    直到 `App` 重建）；
    /// 2. 那次求值在 `replace_deps` **之前**中断 ⇒ 该派生的依赖边仍是上一次成功求值的（对派生而言是
    ///    「陈旧但自洽」的：上游变了仍会把 `dirty` 置回，只是再也算不出来）；
    /// 3. 如果是**节点**的 `bind_style` panic，节点的 `deps` 同样停在旧值 ⇒ 下一次 pass 会走
    ///    「闭包已丢」分支显式退订（`App::refresh_styles`，§14.4），节点退化为静态节点；
    /// 4. 求值栈由 `EvalGuard::drop` 复位，不会污染后续求值。
    pub fn read_derived<T: Clone + 'static>(
        &mut self,
        derived: &Derived<T>,
    ) -> Result<T, EngineError>;
}
```

| 决策 | 结论 | 理由 |
|---|---|---|
| 环检测时机 | **运行时求值栈**（`EvalStack: SmallVec<[DerivedId; 8]>`） | 注册期 DFS **不可行**：闭包的依赖在闭包运行前不可知，且注册时新派生还不存在于任何闭包里。→ **S-26**（取代设计 §3.9 的「注册时 DFS 拒绝注册」） |
| 环的返回值 | `Err(EngineError::CyclicDerived { derived })`（**不是** panic） | 环是应用层可触发的真实错误（两个派生经 `Rc<RefCell<Option<Derived<_>>>>` 互引）；§2.1 I10「不静默降级」要求上报；AGENTS §3 要求强类型错误。`CyclicDerived` 因此有真实生产者 |
| 环检测与「取闭包」的顺序 | **先检测环，再 `take_compute`** | 环命中时闭包仍在槽里、缓存与 `dirty` 都不变 → 「求值失败不污染缓存」由顺序保证，不靠回滚 |
| **panic 安全（`Drop` guard）** | `EvalGuard { stack: &'a mut EvalStack, id }`，`impl Drop { self.stack.pop(); }`；`eval_derived` 内**先**建 guard，**再** `take_compute` | ①闭包 panic 时展开栈仍执行 `guard` 的 `Drop` → 求值栈不会残留脏深度（否则下一次求值会误判「正在求值」）；②`nested_ctx` 借 `guard.stack_mut()` 且声明在 guard **之后** → 展开顺序是 `nested_ctx` 先 drop、`guard` 后 drop（借用合法） |
| 求值失败（环）不污染缓存 | ①环：缓存与 `dirty` 不动；②闭包 panic：`compute` 已被 `take`，展开时不会执行到 `put_compute` → 该派生的 `compute` 为 `None`，后续读命中 `take_compute == None` 分支并返回 `CyclicDerived`（**不 UB、不 panic**），`dirty` 保持 true | 用「二道防线返回强类型错误」代替 `expect`：panic 之后引擎仍可用（该派生永久报错，直到 `App` 重建），符合 AGENTS §3 的强类型错误面 |
| 帧边界兜底 | `App::refresh_styles` 在进入时 `debug_assert!(self.eval_stack.is_empty())` 并 `self.eval_stack.clear()`（防御：若上一帧有 panic 被上层 `catch_unwind` 住） | 次生损害最小化；正常路径上 guard 已保证空 |
| 闭包签名为何返回 `T` 而非 `Result<T, _>` | `T` | 绝大多数派生只读信号（不可能失败）；返回 `Result` 会把 `?` 传染给每个派生闭包。派生读派生时由调用方显式处理 `Result`（可 `expect`/`unwrap_or_default`，见 §17 的环用例） |
| 是否做值相等缓存（`Derived` 重算后值相同则不下发失效） | **不做** | 无消费者：`Derived` 的失效只置 `dirty`，下游节点的重算由 `App::write` 的 push 决定；值相等短路需要把「派生值版本」也纳入 `SignalId` 传播（两套机制） |
| `Derived` 依赖 `Transient` | **禁止但不可强制**：文档写明「派生闭包不得读 `Transient`」 | `Transient` 写不置 `dirty` ⇒ 派生永久是旧值（stale）。P03 无强制手段（不加运行时检查：无消费者、影响热路径）→ 记为 V-24 |

### 7.5 `dirty` 模型与「写等值短路」的裁决（取代设计 §3.2 的 `versions: Vec<u64>`）

| 问题 | 结论 | 理由 |
|---|---|---|
| 是否需要 `versions: Vec<u64>` | **完全删除**（既无独立向量，也无每槽位 `version`） | 本设计的失效是 **push**（`App::write` 直接标脏），staleness 由 `DerivedSlot.dirty` / `Node.dirty` 表达，**没有任何代码做版本比较**。POS：`dead_code` 在非 test 构建必然失败；裁决 ② 明确「没有读者就删掉，不为测试保留」。→ S-27 |
| 扩容/索引语义 | `values: Vec<SignalSlot>`、`derived: Vec<DerivedSlot>`，`push` 即扩容，**下标即 id** | 无删除 → 不需要自由表/代际（§7.1） |
| **写相等值时是否失效** | **不失效**：`App::write` 先 `owned_ref::<T>` 比较，`==` 则直接返回 `false`（不标脏、不置唤醒） | ①高频行情下「同一报价重复推送」极常见，短路可省掉整条失效链路（I3「空闲零帧」也依赖它）；②代价是一次 `O(size_of(T))` 的 `PartialEq`（不是深比较，除非 `T` 自己实现成深比较）；③`f64::NAN != NAN` ⇒ 含 NaN 的值每次写都失效，这是**正确**方向（NaN 不可比较）；④`String`/`Vec` 的值比较是 O(n)：写频率远低于渲染，记 V-25 观察 |
| **等值短路的语义与风险（必须写进应用层契约）** | 引擎按 `PartialEq` 判定「值没变 ⇒ 无需重绘」。因此：**`T` 必须是不可变值语义**——若 `T` 内部有可变性（`Rc<RefCell<…>>`、`Cell`、`Arc<Mutex<…>>`）并且 `PartialEq` 只比较外层指针/句柄，则「内部状态变了但 `PartialEq` 相等」**不会**触发重绘（stale drawing）。应用层若要传递可变句柄，必须自己保证「句柄变化时才写」或把可变数据放进新值 | 这是短路的必然代价：引擎无法区分「值相等」与「语义相等」。写进 §4.9.2 的应用层契约与 `base_ui` 的 code review 约定；测试 §17.2-4 用 `Rc<Cell>` 型 `T` 的反例固化该风险 |
| 是否给 `write` 加重载（`write_always`） | **不** | 无消费者；真有需求时 additive 加一个方法即可（届时同样要写 `Clone + PartialEq` 之外的约束说明） |

### 7.6 值的生命周期（取代设计 §3.9 的「引用计数」）

| 问题 | 结论 |
|---|---|
| 单个信号的回收 | **不回收**（`App` 生命周期内一直存活） |
| 理由 | `Signal<T>` 是 `Copy` 句柄（§7.2），**无法**做引用计数；设计 §3.9 的「显式 `StateStore::drop(signal)` + 无引用即释放」在 Copy 句柄下不可实现。真正的回收意义也有限：应用的状态集是**有界**的（一个交易终端几百到几千个信号），而释放后再写入需要代际（§7.1 又回到代际问题）→ **S-25** |
| 内存上界 | `SignalSlot` = `Box<dyn Any>`(16) + 变体判别（`enum` 取最宽载荷）= **24 B**；1 万个信号 ≈ 240 KB（不含值本体） |
| 派生闭包的内存 | `Box<dyn Fn>` 16 B/个；同样与 `App` 同寿 |

### 7.7 `Transient<T>`

| 语义 | 结论 |
|---|---|
| 读 | `ReadCtx::read_transient(&Transient<T>) -> T`：**不调用** `tracker.record` |
| 写 | `App::write_transient`：只替换值，**不查 `dependents`**、不标脏、不置唤醒 |
| 存储 | 与 `Signal` 同表（`SignalValue::Transient`）；`SignalId` 空间共用 |
| 误用防护 | `App::write` 对 `SignalValue::Transient` 槽做 `debug_assert!(false, "transient 必须用 write_transient")`；release 下按普通信号处理（不会 UB，只是白标脏） |
| 典型消费者 | P13 的滚动偏移 / 悬浮位置 / 拖拽位移（§3.7、§8.4） |

---

## 8. `node/dependency.rs`（依赖追踪）

### 8.1 `Owner` / `OwnerSet`

```rust
/// 依赖边的**下游**：节点（带窗口，NodeId 是窗口内 id，必须消歧）或另一个派生值。
///
/// `Copy + Eq + Hash`：可作集合元素、可 `retain`/`contains`/复制出借用。
/// **不实现 `Ord`**（无排序消费者；`NodeId` 也不实现 `Ord`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Owner {
    Node { window: WindowId, node: NodeId },
    Derived(DerivedId),
}

/// 一个信号的读者集合。`SmallVec`（内联 2 个）而非 `HashSet`/位图。
pub(crate) type OwnerSet = SmallVec<[Owner; 2]>;
```

**`Owner` 的相等/哈希/去重/序（冻结规则）**

| 规则 | 内容 | 为什么 |
|---|---|---|
| 相等 | 字段逐一相等（`Node { window, node }` 含 `window`；`Derived(id)` 按 id） | `WindowId` 是 `u32` 单调 id、`NodeId` 是 `(index, generation)` → **同一个**节点的句柄只有一个表示形式 |
| 哈希 | `derive(Hash)`，与 `Eq` 一致 | `HashSet` 化的退路（V-26）要求 `Eq + Hash` 同源 |
| 去重 | `replace_deps` 插入前 `!set.contains(&owner)`；`detach` 用 `retain`（清掉历史重复，§8.3 D1） | 一 (信号, owner) 只留一份 |
| **序** | 集合是**插入序**（`SmallVec` push）。插入序 = 该 owner **首次**登记该信号的时机；`replace_deps` **不重排**已存在的元素 | 确定性来自「`refresh_styles` 按 `NodeTree::order()` 遍历 + `bind_style` 求值序固定」；**不**按 id 排序（排序每次 O(k log k)，且无消费者） |
| 集合语义与输入顺序 | `replace_deps(old, new)` 的结果**集合**与遍历顺序无关；若 `new` 的内部顺序变化，集合的**迭代序**可能变化 | 失效只做「标脏 + damage 并集」（都与顺序无关）；测试断言集合而非序列（§17.3）。诊断需要稳定序列时由调用方排序（P06 `FrameRecord`） |

| 选择 | 结论 | 理由 |
|---|---|---|
| 为什么 `Owner::Node` 带 `WindowId` | **必须**：`NodeId` 是**窗口内**的 arena 索引，两个窗口可以各自有 `n0.1`。不带 `WindowId` 时 `App::write` 会误标另一个窗口的同号节点（stale 之外的第二类错误） | §5.1 的多窗口模型（§1.4 的 `windows: HashMap`）。带窗口后 `App::write` 是 O(1) 定位：`windows.get_mut(&window)` |
| `OwnerSet` 用 `ahash::HashSet`？ | **不** | 稳态下每个信号的读者是 1–3 个（一个报价只被读它的面板读）；`SmallVec` 内联零分配、迭代序**确定**（测试可断言）、差分是两条小切片的 O(n·m) 比较（n,m ≤ 8） |
| 用位图？ | **不** | 位图键是「稠密 index」。`NodeId` 的 index 会被**复用**（装卸），代际不在位图里 ⇒ 陈旧句柄的位会命中复用后的新节点 —— 正是 §6.2 A4 要防的错误 |
| 上界与退化 | 主题/字体类信号在 mount 期可能聚到上千读者，插入是 O(k) 线性查重 ⇒ 一次性 O(k²)（5000 节点 ≈ 12.5M 次 12 B 比较，量级 ms，只在首次 mount）。记 **V-26**：P18 基准若显示 >1 ms，改 `ahash::HashSet`（additive，接口不变） | — |
| 读者集合的生命周期 | 每个 (信号, owner) 只在 **owner 的读取集合变化时**增删（`replace_deps` 差分）；稳态写入不触碰集合 | §8.3 |

### 8.2 `DependencyGraph`（`pub(crate)`）

```rust
pub(crate) struct DependencyGraph {
    /// `SignalId → 读者集合`。空集合立即删除表项（§3.9「避免表膨胀」）。
    dependents: ahash::HashMap<SignalId, OwnerSet>,
}

impl DependencyGraph {
    pub(crate) fn new() -> Self;

    /// 可变反查（`App::write` 的第一跳 + **死 owner 剔除**：`retain` 掉已卸载的节点/已不存在的窗口）。
    ///
    /// **没有只读版本**（B4）：唯一的读者 `App::write` 紧接着就要 `retain`，只读版无消费者 → 不写。
    pub(crate) fn dependents_mut(&mut self, id: SignalId) -> Option<&mut OwnerSet>;

    /// **每帧替换**依赖（差分清理，§8.3）。
    pub(crate) fn replace_deps(&mut self, owner: Owner, old: &[SignalId], new: &[SignalId]);
    /// 删一条边（`App::close_window` 的逐节点清理）。
    pub(crate) fn detach(&mut self, id: SignalId, owner: Owner);
    /// 删除整条表项（`App::write` 的 `retain` 后清理空集合，§8.3 D2）。
    pub(crate) fn remove_signal(&mut self, id: SignalId) -> Option<OwnerSet>;
}
```

**复杂度保证**

| 方法 | 复杂度 | 说明 |
|---|---|---|
| `dependents_mut` | O(1) 平均 | `ahash` 表 |
| `replace_deps` | O(n·m)（n = 旧集合长度，m = 新集合长度；typically ≤ 8×8） | 两条小切片的集合差；**不**遍历整树 |
| `detach` | O(k) | 线性查找（同时 `retain` 掉重复项，见 §8.3 的「允许重复但查重」） |
| `remove_signal` | O(1) | 只删表项 |
| 失效一跳（`App::write`） | O(#dependents) + O(级联派生数) | **不**遍历整树（I3/§3.4 的前提） |

### 8.3 `replace_deps` 的差分算法（逐字）

```text
replace_deps(owner, old, new):
    for id in old:
        if !new.contains(id):                     # 旧有新无 → 退订
            if let Some(set) = dependents.get_mut(id):
                set.retain(|o| *o != owner)        # 去掉该 owner 的**全部**出现（含历史重复）
                if set.is_empty(): dependents.remove(id)     # 空集合立即删条目
    for id in new:
        if !old.contains(id):                     # 新有旧无 → 订阅
            let set = dependents.entry(id).or_default()
            if !set.contains(&owner): set.push(owner)        # 查重：同一 owner 在同一信号上只留一份
```

| 不变量 | 内容 | 违反后果 |
|---|---|---|
| D1 | 每个 (信号, owner) 在集合里**至多一次** | 重复标记（幂等，无害）但会让 `detach` 漏删 → 潜在泄漏 |
| D2 | 集合为空时表项**必须**删除 | 表膨胀（§3.9） |
| D3 | `dependents` 的键集合 ⊆ 存活信号的 id | 写路径反查不可能命中死信号（无删除 ⇒ 恒成立） |
| D4 | `replace_deps` 是**替换**不是累加 | §3.8「条件分支留下幽灵依赖」的根因对策 |
| D5 | 不存在的 owner（已卸载节点）允许残留在集合里，由 `App::write` 的 `retain` 惰性剔除 | 删除时无法 O(1) 知道该节点的全部信号（除非读 `node.deps`，见 §14.5） |

### 8.4 `ReadTracker` 与 `ReadCtx` 的借用编排

```rust
/// 一次 owner 求值内的「本帧读取集合」。内联 8 个；查重是线性（典型 1–5 个）。
pub(crate) struct ReadTracker {
    deps: SmallVec<[SignalId; 8]>,
}

impl ReadTracker {
    pub(crate) fn new() -> Self;
    /// 去重登记；消费者：`ReadCtx::read`/`read_derived`。
    pub(crate) fn record(&mut self, id: SignalId);
    pub(crate) fn as_slice(&self) -> &[SignalId];
    pub(crate) fn into_inner(self) -> SmallVec<[SignalId; 8]>;
}

/// 显式读取上下文（§3.3）：**唯一**的读取入口，编译器保证「绕过登记」不可能。
///
/// 全部字段私有；只有引擎构造（`App::refresh_styles` 与嵌套求值）。
pub struct ReadCtx<'a> {
    /// 信号/瞬态值的只读视图（split borrow 的一半）。
    values: &'a [SignalSlot],
    /// 派生槽的可变视图（求值要写缓存/取闭包）。
    derived: &'a mut DerivedStore,
    graph: &'a mut DependencyGraph,
    /// 本 owner 的读取集合。**是借用不是自持有**：嵌套求值时父 ctx 换一个局部 tracker 即可（§7.4）。
    tracker: &'a mut ReadTracker,
    eval_stack: &'a mut EvalStack,
    owner: Owner,
}

impl<'a> ReadCtx<'a> {
    /// 引擎内部构造（消费者：`App::refresh_styles`；嵌套求值另建一个同形 ctx）。
    pub(crate) fn new(
        values: &'a [SignalSlot],
        derived: &'a mut DerivedStore,
        graph: &'a mut DependencyGraph,
        tracker: &'a mut ReadTracker,
        eval_stack: &'a mut EvalStack,
        owner: Owner,
    ) -> Self;

    /// 读普通信号：登记依赖 + 克隆值。
    ///
    /// # Panics: `SignalSlot::owned` 的不变量断言（§7.3）
    pub fn read<T: Clone + 'static>(&mut self, signal: &Signal<T>) -> T;

    /// 读瞬态值：**不登记**依赖（§7.7）。
    pub fn read_transient<T: Clone + 'static>(&mut self, transient: &Transient<T>) -> T;

    /// 读派生值：登记依赖 + 惰性求值（§7.4 有完整契约）。环 → `Err(CyclicDerived)`。
    pub fn read_derived<T: Clone + 'static>(&mut self, derived: &Derived<T>)
        -> Result<T, EngineError>;

    /// 诊断/测试专用：读值但**不登记**依赖（§3.8「`peek()` 仅测试可用」）。
    #[cfg(test)]
    #[must_use] pub fn peek<T: Clone + 'static>(&self, signal: &Signal<T>) -> T;

    /// 本 owner 的 `Owner`（`replace_deps` 的键；消费者：`App::refresh_styles`）。
    pub(crate) fn owner(&self) -> Owner;
}
```

**借用的三层切分（为什么 `ReadCtx` 的字段全是 `&`/`&mut`）**

| 问题 | 处理 |
|---|---|
| 求值期间要读信号（不可变）又要写派生缓存（可变） | `StateStore::split()` 把 `values`（`&[SignalSlot]`）与 `derived`（`&mut DerivedStore`）拆成两个不交叠借用 |
| 派生闭包存在 `derived` 里，调用时需要 `&mut ReadCtx` | `DerivedSlot::compute` 是 `Option`，求值前 `take()` 出来（借用立即结束），求值后放回（§7.4） |
| 嵌套求值要换「读取集合」 | `tracker` 是 `&'a mut ReadTracker`（**借用**，不是自持有）→ 嵌套求值直接用一个**局部** `ReadTracker` 构造子 ctx，父集合不受影响、无需 `mem::swap`（§7.4 的第 ④ 步） |
| 求值栈如何在嵌套与 panic 之间保持一致 | 栈指针经 `EvalGuard` 转借：guard 拥有 `&mut EvalStack`，`Drop` 里 `pop`；子 ctx 拿 `guard.stack_mut()`（§8.5） |
| `peek` 为何 `#[cfg(test)]` | §3.8 明确「`peek()` 仅测试可用并记日志」；发布物里不存在这个绕过登记的入口 → 也不会 `dead_code`（只在 test 构建编译） |
| `tracker.record` 与图的关系 | **热路径只写 `tracker`**（O(1) push，去重），owner 的读取集合在求值结束时**一次性** `replace_deps`（§8.3）。图不做「逐次登记」 |

> **裁决**：`DependencyGraph::record`（单条边登记）**不落地** —— P03 没有任何「不走 tracker 的单条登记」调用点
> （节点与派生的登记都经 `replace_deps` 一次性提交）。§8.2 的签名表保留为接口说明；实现清单只含
> `new`/`dependents`/`dependents_mut`/`replace_deps`/`detach`/`remove_signal`（§4.3 已按此列出）。

### 8.5 `EvalStack` 与 `EvalGuard`（panic 安全的求值栈）

```rust
pub(crate) struct EvalStack {
    stack: SmallVec<[DerivedId; 8]>,
}

impl EvalStack {
    pub(crate) fn new() -> Self;
    pub(crate) fn contains(&self, id: DerivedId) -> bool;   // 环检测
    pub(crate) fn push(&mut self, id: DerivedId);
    pub(crate) fn pop(&mut self) -> Option<DerivedId>;
    pub(crate) fn clear(&mut self);                        // 帧边界兜底（防上一帧 panic 残留）
    /// 调试断言：求值结束后必须为空（消费者：`App::refresh_styles`）。
    pub(crate) fn is_empty(&self) -> bool;
}

/// 入栈 + **Drop 出栈**：派生闭包 panic 时求值栈不会残留脏深度（§7.4）。
///
/// 拥有 `&mut EvalStack`；子 ctx 通过 `stack_mut()` 转借，因此借用检查也能保证
/// 「guard 比子 ctx 活得久」（子 ctx 先 drop，guard 后 drop → `pop` 在其后执行）。
pub(crate) struct EvalGuard<'a> {
    stack: &'a mut EvalStack,
    id: DerivedId,
}

impl<'a> EvalGuard<'a> {
    /// 入栈并返回 guard（消费者：`ReadCtx::eval_derived`）。
    pub(crate) fn enter(stack: &'a mut EvalStack, id: DerivedId) -> Self;
    /// 把栈**转借**给子 ctx（消费者：`ReadCtx::eval_derived` 构造 nested ctx）。
    pub(crate) fn stack_mut(&mut self) -> &mut EvalStack;
}

impl Drop for EvalGuard<'_> {
    fn drop(&mut self) {
        // 必须弹掉的正是自己入栈的那个 id；用 debug_assert 固化 LIFO 不变量。
        debug_assert_eq!(self.stack.pop(), Some(self.id), "eval stack is not LIFO");
    }
}
```

**不变量**：`EvalStack` 在 `App` 里常驻、只在求值期非空；**入栈必有出栈**由 `EvalGuard::drop` 保证（含 `?` 短路与 panic 展开）。
`App::refresh_styles` 进入时 `debug_assert!(self.eval_stack.is_empty())` 并 `clear()`（防御性兜底：正常路径上不应残留）。
`App::refresh_styles` 在每个节点求值后 `debug_assert!(self.eval_stack.is_empty())`（漏 `pop` 会在 debug 下立刻暴露）。

---

---

## 9. `node/dirty.rs`（`DirtyFlags` 与失效传播）

### 9.1 位定义

```rust
bitflags! {
    /// 节点的失效集合（§3.4）。`u8`：6 个已用位 + 2 个保留位。
    ///
    /// `Debug` 走 bitflags 的 `Internal` 实现（`internal.rs:56`）；`Default` **不**派生 ——
    /// 节点的初始值是显式的 `LAYOUT | PAINT | A11Y`（§11.4），没有「默认空集」的语义。
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct DirtyFlags: u8 {
        /// 需要重新测量/布局。
        const LAYOUT   = 0b0000_0001;
        /// 需要重算动态属性（`bind_style`）并在 P06 重建 lane。
        const PAINT    = 0b0000_0010;
        /// `Geometry` 的 per-draw uniform（`Transform`）需要更新；顶点不动。
        const GEOMETRY = 0b0000_0100;
        /// 无障碍节点需要重发（P11）。
        const A11Y     = 0b0000_1000;
        /// 绘制序变化（浮层 / 虚拟列表 / 子节点增删）。
        const ORDER    = 0b0001_0000;
        /// lane 可整桶复用（P06 的 paint pass 读它；被失效的节点清掉该位）。
        const REUSABLE = 0b0010_0000;
    }
}
```

| 位 | P03 的写者（非 test） | P03 的读者 | 后续消费者 |
|---|---|---|---|
| `LAYOUT` | `mark_layout`、`mark_structure`、`refresh_styles`（`affects_layout` 升级） | `mark_layout` 的冒泡早停（`contains`） | P04 布局 |
| `PAINT` | `mark_paint`、`mark_layout`、`mark_structure`、节点创建 | `refresh_styles`（选谁求值）、`clear_paint` | P06 lane 重建 |
| `GEOMETRY` | — | — | P08 `set_geometry_transform` |
| `A11Y` | `mark_structure`、`refresh_styles`（`interactive`/`visible` 变化）、节点创建 | — | P11 |
| `ORDER` | `mark_structure` | — | P06/P13 绘制序 |
| `REUSABLE` | `mark_*` 的 `remove` | — | P06 |

> `GEOMETRY`/`REUSABLE` 的「无 P03 读者」是**设计的预期**：位是 `pub const`（不触发 `dead_code`），
> 而 `remove(REUSABLE)` 真实读过该常量。→ §22 S-28 记录。

### 9.2 标记与冒泡（`NodeTree` 的 `pub(crate)` 方法）

```rust
impl NodeTree {
    /// 颜色/透明度/文本（尺寸不变）/光标等：只标本节点，**不冒泡**（§3.4 行 1–2）。
    /// 返回节点是否存在（false = 陈旧句柄 → `App::write` 剔除该 owner）。
    pub(crate) fn mark_paint(&mut self, node: NodeId) -> bool;

    /// 尺寸可能变化：`LAYOUT|PAINT` + 按 §9.3 向上冒泡（§3.4 行 3–4）。返回节点是否存在。
    pub(crate) fn mark_layout(&mut self, node: NodeId) -> bool;

    /// 无障碍语义变化：只标 `A11Y`（不产生视觉 damage，§3.4 行 9）。
    pub(crate) fn mark_a11y(&mut self, node: NodeId) -> bool;

    /// 结构变化（子节点增删/顺序）：`LAYOUT|PAINT|ORDER|A11Y` + 冒泡（§3.4 行 5）。
    pub(crate) fn mark_structure(&mut self, node: NodeId) -> bool;

    /// `refresh_styles` 消费掉 `PAINT`（样式求值完成；lane 重建由 `!REUSABLE` 决定）。
    pub(crate) fn clear_paint(&mut self, node: NodeId) -> bool;
}
```

**标记的公共尾段（三个方法共用，写成 `fn touch(&mut self, id, flags) -> bool` 私有辅助）**

```text
touch(node, flags):
    Some(n) = arena.get_mut(node) else return false
    n.dirty.insert(flags)
    if flags.contains(PAINT) || flags.contains(LAYOUT):      # lane 必须重建
        n.dirty.remove(REUSABLE)
    n.damage = n.damage.union(n.prev_layout.bounds).union(n.layout.bounds)   # §9.4
    return true
```

`mark_layout` 的冒泡（**逐字**）：

```text
fn mark_layout(node) -> bool:
    if !touch(node, LAYOUT | PAINT) { return false }
    let mut child = node
    loop:
        if !affects_parent(child) { break }                 # child 出流/隐藏 → 父不需重排
        let Some(p) = parent_of(child) else { break }        # 到根
        if arena.get(p).dirty.contains(LAYOUT)
           && depends_on_children(p) { break }              # 早停：N7 —— 只有「p 依赖子内容」时，
                                                            # 既有的 LAYOUT 才保证 p 的祖先链已被同一规则处理过
        if !touch(p, LAYOUT) { break }                       # 幂等（重复标记只并 damage）
        if !depends_on_children(p) { break }                 # p 尺寸不依赖子 → p 的祖先不需重排
        child = p
```

> **N7 的理由**：早停是纯优化，但它隐含「上一次标记已经从 p 继续向上」的前提 —— 这个前提只在 p **依赖子内容**
> （即上一次冒泡会越过 p）时成立。当 p 是固定尺寸节点时，`touch` 是幂等的（位并集 + damage 并集），多走一次没有代价，
> 而提前 break 会让「固定尺寸节点自身因 `affects_layout` 升级而要求祖先重排」的路径漏标记。所以早停条件里显式加
> `depends_on_children(p)`。用例见 §17.4-12。

`mark_structure(parent)` = `touch(parent, LAYOUT|PAINT|ORDER|A11Y)` + 同一冒泡循环（起点 `child = parent`）。

### 9.3 冒泡终止判定表（§3.4 逐行 + taffy 字段）

> **补记（审查 B-N4 / A-N3）**：
> 1. `append_child` 在**重挂**（`child` 本来有父）时，除 `mark_structure(old_parent)`/`mark_structure(parent)` 之外，
>    还对**被移动的整棵子树**调用 `mark_subtree_paint(child)` —— 逐节点 `touch(PAINT)`（清 `REUSABLE` + 并 damage）。
>    理由：拼接后该子树在 `order` 里的相对位置变了，P06 的 lane 顺序随之改变 ⇒ 整棵子树都要重画。
> 2. `touch` 只在 flags 含 `LAYOUT`/`PAINT` 时才 `remove(REUSABLE)` + `accumulate_damage()`；`A11Y`/`ORDER`/`GEOMETRY`
>    单独标记**不产生 damage、不清 `REUSABLE`**（像素没变）。

**两个谓词**（都在 `dirty.rs`，都读 `Node.style().inner`）：

```rust
/// N 的布局尺寸**是否可能**随子内容变化（保守：任一轴依赖即 true）。
///
/// 读的 taffy 字段：`Style::size: Size<Dimension>`（`style/mod.rs:754`）+ `Dimension::expand()`
/// （`style/dimension.rs:559`）。`Dimension` 不是枚举，必须 `expand()`。
pub(crate) fn depends_on_children(node: &Node) -> bool {
    if node.kind().is_leaf() { return false; }                 // Canvas/Geometry：叶子不自布局
    let size = node.style().inner.size;
    axis_depends_on_children(size.width) || axis_depends_on_children(size.height)
}

fn axis_depends_on_children(d: Dimension) -> bool {
    use taffy::style::ExpandedDimension as E;
    match d.expand() {
        // 固定/父定尺寸：与子内容无关
        E::Length(_) | E::Percent(_) | E::Stretch => false,
        // calc() 按包含块求值（N3：taffy 的 calc feature 在本引擎是**开启**的，必须显式列出）
        #[cfg(feature = "calc")]
        E::Calc(_) => false,
        // Auto / MinContent / MaxContent / FitContent / FitContentPx / FitContentPercent / Content
        // （以及 calc 关闭时的穷尽性兜底）→ 可能由内容决定，保守为 true
        _ => true,
    }
}

/// N 出流或隐藏时，它的尺寸/位置变化**不影响**父节点的布局。
pub(crate) fn affects_parent(node: &Node) -> bool {
    use taffy::style::{Display, Position};
    !matches!(node.style().inner.display, Display::None)
        && !matches!(node.style().inner.position, Position::Absolute | Position::Fixed)
}
```

| §3.4 的行 | 写入类型 | 直接后果 | 是否冒泡 | P03 的判定/实现 | damage 来源 |
|---|---|---|---|---|---|
| 1 | 颜色/透明度/阴影参数 | `PAINT` | ❌ | `mark_paint` | 节点自身 `layout.bounds` |
| 2 | 文本内容（尺寸不变） | `PAINT` | ❌ | `mark_paint`（`bind_text` 属 P05） | 文本 AABB |
| 3 | 文本内容（measure 变化） | `LAYOUT+PAINT+A11Y` | ✅ 到「第一个不受影响的祖先」 | `refresh_styles` 按 `StylePatch.affects_layout` 升级为 `mark_layout` | 新旧 AABB 并集 |
| 4 | 字体/字号/主题色/语言 | `LAYOUT+PAINT+A11Y` | ✅ | 同上（主题信号写入 → 读者逐个走 1 或 3） | 批量，超阈值全帧（P06） |
| 5 | 子节点增删/顺序变化 | `LAYOUT+ORDER+PAINT+A11Y` | ✅ | `mark_structure(parent)`（`append_child`/`remove` 内部调用） | 子树包围盒 |
| 6 | 窗口尺寸/DPI | 根 `LAYOUT` | 根向下 | `Window::set_scale` → `mark_layout(root)`；尺寸变化在 P12 | 全帧 |
| 7 | 滚动偏移（`Transient`） | 只改 uniform/clip | ❌ | **P03 不产生任何脏位**（§7.7） | scroll rect（P12） |
| 8 | `Geometry` uniform | `GEOMETRY` | ❌ | 位已定义；写者 P08 | geometry 包围盒 |
| 9 | 无障碍语义 | `A11Y` | ❌ | `mark_a11y`；`refresh_styles` 在 `interactive`/`visible` 变化时调用。**不产生 damage、不清 `REUSABLE`**（像素没变；damage 只在 `LAYOUT`/`PAINT` 时累积） | 无 |

**终止条件判定表（P03 可单测）**

| 节点 N 的形态 | `affects_parent(N)` | `depends_on_children(N)` | 冒泡在 N 处 | 判定依据 |
|---|---|---|---|---|
| 普通 flex 容器，`size` 全 `auto` | true | true | **继续** | `Dimension::Auto` ∈ 依赖集 |
| 固定宽高（`Length`/`Percent` 双轴） | true | **false** | **在 N 停止** | `expand()` 为 `Length`/`Percent` |
| `stretch` 双轴 | true | **false** | 在 N 停止 | `ExpandedDimension::Stretch`（父定尺寸） |
| `min-content`/`fit-content` 任一轴 | true | true | 继续 | 尺寸由内容决定（保守） |
| `position: absolute` / `fixed` | **false** | —（不参与判定） | **在 N 之前停止**（父不需要重排） | `Style::position` |
| `display: none` | **false** | — | 在 N 之前停止 | `Style::display == None` |
| `NodeKind::Canvas(_)` / `Geometry(_)` | true | **false** | 在 N 停止 | 叶子节点不自布局（§4.4「`Canvas`/`Geometry` 节点不参与子布局」） |
| 有 `LayoutOverride`（P04） | true | **false** | 在 N 停止 | P04 会在 `Node` 上加 `layout_override: Option<…>` 并在本函数里短路（§11.1 的扩展点） |

> **保守性的代价与收敛**：`Auto` 被当作「依赖子内容」→ 一个 `auto` 高度的祖先会被逐级标 `LAYOUT` 直到根。
> 这是**正确侧**的保守（多算不漏算）。§4.4 给出的精确终止是「measure 输出与上一帧相同则不冒泡」，它需要布局结果 →
> **P04 的 `set_layout` 在写回后比较新旧 `size`，相同则不再向上**（本表因此是 P04 的上界，不是替代）。→ §4.4 / V-27。

### 9.4 damage 累积（逻辑 AABB）

| 规则 | 内容 |
|---|---|
| 存储 | `Node.damage: Rect`（`Rect::default()` = 空 = 并集单位元，P01 §6.3.3 已冻结 `∅ ∪ R = R`） |
| 累积点 | 每次 `touch`（= 任一 `mark_*`）：`damage ∪= prev_layout.bounds ∪ layout.bounds` |
| 为什么是并集 | 节点移动/缩放时，「旧位置」和「新位置」都必须重绘；只算新位置会留下残影（stale drawing） |
| P03 的局限 | `layout`/`prev_layout` 在 P04 之前恒为 `LayoutBox::default()` → damage 恒为空。写回路径已就位，**P04 的 `set_layout` 必须再并一次新 box**（否则首帧的「新位置」不入 damage）→ §11.1 的 `set_layout` 交接 |
| 物理化 | P06 的 `render/damage.rs` 用 `Rect::to_physical_covering` + `aa_padding`（P01 §7.2） |
| 清零 | P06 的帧初（`NodeTree::begin_frame`） |
| 读取 | `Node::damage()`（`Probe`/P06 帧记录） |

### 9.5 `StylePatch.affects_layout` 的读取时机（结论）

| 时机 | 读什么 | 用途 |
|---|---|---|
| `App::write` 失效一跳 | **节点上次求值的 patch**（`node.style_patch().affects_layout`） | 决定 `mark_paint`（false）还是 `mark_layout`（true）。这是唯一能在「写发生时」拿到的信息 |
| `App::refresh_styles` 求值后 | **新 patch 与旧 patch 的 `affects_layout`**（`new \|\| old`） | 升级：旧值 true（上帧尺寸会变）或新值 true（本帧尺寸会变）都必须走 `LAYOUT` |
| 为什么要 `new \|\| old` | 旧 true、新 false：布局输入**曾经**变过，本帧需要一次布局把尺寸收回来；旧 false、新 true：本帧开始变 | 两个方向都不能漏 |
| 不读 `affects_layout` 的地方 | P06 的 lane 重建（只读 `REUSABLE`） | — |

---

## 10. `paint/style.rs`（`Style` / `StylePatch` / `Cursor`）

### 10.1 `Style`：是否暴露 `taffy::Style<SmolStr>`

**结论：暴露 `pub inner: taffy::Style<SmolStr>`。**

> **线程约束（审查 B-N9）**：`taffy::Style<SmolStr>` 在开启 `calc` feature 时含 `Calc` 的**指针**（指向 taffy 内部
> calc 运算的 arena），因此 `Style` 是 **`!Send + !Sync`**。由此：`Node`/`NodeTree`/`Window`/`App` 全都不是 `Send`，
> **不得跨线程搬运**（`App` 只能在创建它的线程上驱动；跨线程回投必须走 P09 的 `AppHandle` + `PlatformDispatcher`，
> 那时也只搬运「消息」而不是 `App` 本身）。这也解释了为什么 `App::runtime_handle()` 只借出一个 `Handle`：
> 句柄是 `Send` 的，`App` 不是。

```rust
/// 静态样式结构（I17）：挂载时一次写入；运行时只经 `StylePatch` 变化。
///
/// `inner` 是 taffy 的样式结构（布局属性）。**引擎不提供任何布局属性的访问器/Builder**：
/// 应用层的 `base_ui::StyleBuilder`（D24）直接构造 `taffy::Style<SmolStr>` 再包一层即可。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Style {
    /// 布局属性（taffy 0.15.0，`CustomIdent = SmolStr`）。
    pub inner: taffy::Style<SmolStr>,
}
```

| 决策 | 结论 | 理由 |
|---|---|---|
| 暴露 taffy 类型？ | **是** | ①设计 §4.9.3 的草图即 `pub inner: taffy::Style`；②不暴露就必须为 40+ 个布局属性提供引擎自有访问器 —— 那正是 D24 明令只允许存在于应用层的「builder/DSL」；③`taffy` 已是引擎的硬依赖且 workspace 用 `=0.15.0` 精确钉死，版本不会漂移 |
| 泄漏第三方类型的代价 | 应用层（`base_ui`）必须自己声明 `taffy = { workspace = true }` 才能写 `Style { inner: taffy::Style { .. } }` | 这是**刻意**的：`base_ui` 的 `StyleBuilder` 返回值就是引擎 `Style`，它本来就要碰 taffy 字段。→ F-35（首次公开第三方 crate 类型） |
| 为什么不用 `pub type Style = taffy::Style<SmolStr>` | 那会让 P06 无法 additive 加字段（圆角/clip/阴影），且 `Style` 会失去「引擎契约类型」的身份（§1.3 白名单列的是 `Style`） | 单字段包装不是预防性抽象：它是**唯一**的扩展点 |
| 类型参数必须写全 `Style<SmolStr>` | `taffy::Style` 的默认参数是 `DefaultCheapStr`，与 `SmolStr` **不是**同一类型（§2.1） | 写错会得到 `Style<String>`，`SmolStr` 的字段赋值编译失败 |
| `Default` | `= Style { inner: taffy::Style::<SmolStr>::DEFAULT }`（derive 转调 taffy 的 `Default`） | taffy 自带 `Style::DEFAULT`；`derive(Default)` 直接可用（`PhantomData<S>: Default`） |
| `PartialEq` | 需要（`refresh_styles` 比较新旧 patch 的**前提**是 patch 可比；`Style` 本身由 `Node` 持有，测试要比） | taffy `Style: PartialEq`（§2.1） |
| P06 才加的字段 | `corner_radius: [f32; 4]`、`clip: ClipPolicy`、`shadow: Option<ShadowSpec>`、`font_family: FontStack` | **P03 不加**：无读者（AGENTS §2.1「不得定义未使用的字段」；P06 的 style→paint 才是首个消费者）。additive 扩展，不改 `inner` |

### 10.2 `StylePatch`

```rust
/// 动态属性的增量补丁（§4.9.2 的「动态属性」列）：由 `bind_style` 每次求值**重算**，值语义。
///
/// 全部字段 `Option`：`None` = 本帧不覆盖（沿用 P06 的静态兜底）。`Default` 是「空补丁」。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StylePatch {
    /// 背景填充色（→ `Quad::paint.fill`）。
    pub background: Option<Color>,
    /// 前景色（文本/图标着色，P05 的 `GlyphRun::color`）。
    pub foreground: Option<Color>,
    /// 边框（复用 `paint::primitive::Border`：宽度 + 颜色；**不**重复定义，→ F-36）。
    pub border: Option<Border>,
    /// 整体不透明度（在**预乘空间**缩放，P02 §5.2 的 `Mul<f32>`）。
    pub opacity: Option<f32>,
    /// 可见性（`false` 时不绘制、不参与命中，P06/P04）。
    pub visible: Option<bool>,
    /// 命中/焦点/Action 开关（`false` = 不可交互，P10/P11）。
    pub interactive: Option<bool>,
    /// 光标形状（P12 设置平台光标）。
    pub cursor: Option<Cursor>,
    /// **尺寸会变时必须置 true** → `App` 追加 `LAYOUT` 并向祖先冒泡（§4.9.4 边界情况表）。
    pub affects_layout: bool,
}
```

| 字段 | P03 的写者/读者（非 test） | 后续消费者 |
|---|---|---|
| `affects_layout` | `App::write`（读）、`refresh_styles`（读+写 `LAYOUT`） | — |
| `interactive`/`visible` | `refresh_styles`（比较新旧，写 `A11Y`） | P10/P11/P06 |
| `background`/`foreground`/`border`/`opacity`/`cursor` | 只被存储与比较 | P06（→ `Quad`/`GlyphRun`）、P12（光标） |

**为什么 P03 就落地全部 8 个字段**：`StylePatch` 是 §4.9.3 逐字冻结的**数据契约**（不是抽象），
`PartialEq` 让 `refresh_styles` 能判「补丁是否真的变了」；字段本身被 `refresh_styles` 的 `!=` 比较**读取**，
因此不违反「不得定义未使用字段」。

**`Border` 的字段核对（裁决 7：不偷偷改 P02 的类型）**

| 检查项 | 结论 | 证据 |
|---|---|---|
| P02 `Border` 的字段 | `pub struct Border { pub width: f32, pub color: Color }`（`Copy`） | `crates/kytrix_gui/src/paint/primitive.rs`（P02 落地）；P02 §6.3.1 的字段表 |
| `StylePatch` 需要什么 | 宽度 + 颜色（v1 的单一边框，所有边同宽同色）、**不需要**半径 | 设计 §4.9.2「边框宽度」属静态结构；半径属圆角（P06 的 `Style` 字段）；v1 无 per-edge 边框 |
| 是否满足 | **满足**，直接用 `paint::primitive::Border` | `Border::new(width, color)` 就是 `StylePatch.border` 需要的全部信息 |
| 半径/逐边怎么办 | **不改 P02**：P06 的 `Style.corner_radius` 提供半径；逐边边框是 v1 明确不做（§4.9.2 未列），需要时 additive 加 `paint::style::StyleBorder`（那时才定义，并记偏差） | 不许「偷偷扩 P02 的 `Border` 字段」——它会牵动 `QuadPaint::Solid` 与 P02 的 60 个用例 |

### 10.3 `Cursor`

```rust
/// 光标形状（P12 映射到平台光标）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Cursor {
    /// 平台默认箭头。
    #[default]
    Default,
    /// 可点击（按钮/链接）。
    Pointer,
    /// 文本输入（I 形）。
    Text,
    /// 不允许（禁用态，§4.9.4 的例子）。
    NotAllowed,
    /// 可拖拽（分栏/画布平移）。
    Grab,
    /// 拖拽中。
    Grabbing,
    /// 水平调整大小（分栏）。
    ResizeHorizontal,
    /// 垂直调整大小。
    ResizeVertical,
}
```

| 决策 | 结论 | 理由 |
|---|---|---|
| 变体集 | 上表 8 个 | 每个都有命名的消费者：`Pointer`/`NotAllowed`（§4.9.4 的 `disabled` 例子）、`Text`（输入框，`base_ui`）、`Grab`/`Grabbing`/`ResizeHorizontal`/`ResizeVertical`（§8.4 的拖拽分栏）、`Default` |
| 是否带 `Hide`/`Wait`/`Crosshair` | **不加** | 无消费者（预防性枚举成员） |

### 10.4 `bind_style` 的闭包类型与存储

```rust
// node/builder.rs
impl NodeBuilder {
    /// 动态属性绑定（§4.9.3）。闭包捕获 `Signal<T>` 句柄（`Copy`），每次求值经 `ReadCtx::read` 登记依赖。
    pub fn bind_style(self, f: impl Fn(&mut ReadCtx<'_>) -> StylePatch + 'static) -> Self;
}

// node/tree.rs（字段，私有）
struct Node {
    // …
    /// `None` = 静态样式节点（没有动态属性）。求值前 `take()`、求值后放回（§8.4）。
    bind_style: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>>,
    /// **上次求值**的补丁（`App::write` 读 `affects_layout`、`refresh_styles` 比较新旧）。
    style_patch: StylePatch,
}
```

| 决策 | 结论 | 理由 |
|---|---|---|
| 闭包用 `Fn` 还是 `FnMut` | `Fn` | 求值只读状态（`ReadCtx` 已 `&mut`）；`Fn` 允许同一闭包被反复调用，避免 `&mut` 带来的借用约束（`Primitive`/`Node` 的 `&mut` 别名） |
| 为什么用 `Box<dyn Fn>` 而不是泛型参数 | 节点是同质的（`Arena<Node>`），每个节点的闭包类型不同 → 必须擦除 | 与设计 §3.10 的 `move` 闭包一致 |
| `'static` 约束 | 必须 | 闭包随 `Node` 存活到卸载；捕获的只有 `Copy` 句柄与 `Rc`/`Arc` |
| 求值期 `take()` 的 panic 安全 | 闭包 panic ⇒ 该节点的 `bind_style` 丢失（后续不再重算，样式停在旧值） | 不做 `catch_unwind`（AGENTS §1.2）；文档写明「`bind_style` 闭包不得 panic」 |
| 是否允许同一节点多个 `bind_*` | `bind_style` 只有一个字段 → 后一次调用覆盖前一次 | 设计只有 `StylePatch` 一条动态通道（`bind_text`/`bind_a11y` 是 P05/P11 的独立字段） |

### 10.5 边界情况表（§4.9.4 逐行）

| 情况 | 处理 | 在 P03 的落点 |
|---|---|---|
| `disabled` 改变尺寸（隐藏图标、文案变长） | `affects_layout = true` → 追加 `LAYOUT` 并冒泡；damage 取新旧 AABB 并集 | `App::write`（读旧 patch）→ `mark_layout`；`refresh_styles`（新 patch）→ `mark_layout` |
| `disabled` 只改颜色 | `affects_layout = false` → 只 `PAINT` | `mark_paint` |
| hover/焦点/滚动/拖拽/动画 | `Transient`，不进依赖图 | `read_transient` / `write_transient`（§7.7） |
| 高频变化（报价涨跌色） | 普通 `Signal`，按字段粒度拆分 | 依赖图天然支持：`Signal<f64>` 只失效读它的节点 |
| 主题/字体栈/语言切换 | 批量语义信号；超阈值 damage 退化全帧 | P03：逐读者 `mark_paint`/`mark_layout`；阈值在 P06 |
| 窗口尺寸/DPI | 根 `LAYOUT`，VBO 不重建 | `Window::set_scale` |
| 结构变化（列表增删） | `ORDER + 子树 LAYOUT` | `mark_structure` |
| 补丁值**未变** | `refresh_styles` 里 `new == old` → 不追加任何新脏位（`PAINT` 仍被消费掉） | 幂等（同一帧多次调用不会放大失效） |
| 节点没有 `bind_style` | `refresh_styles` 只 `clear_paint`，不求值 | 静态样式节点不参与动态通道 |
| `opacity` 变化 | `PAINT`（预乘空间缩放，P02 §5.2） | `mark_paint` |

---

## 11. `node/tree.rs`（`Node` / `NodeKind` / `LayoutBox` / `NodeTree`）

### 11.1 `Node` 字段表（P03 最终形态）

```rust
/// 一个持久节点（跨帧存活）。**字段全私有**（I6：应用层不得持 `&mut Node`）。
pub struct Node {
    id: NodeId,
    parent: Option<NodeId>,
    children: SmallVec<[NodeId; 4]>,
    kind: NodeKind,
    /// 静态结构（挂载时一次写入，之后只有结构变更才重写）。
    style: Style,
    /// 动态属性补丁（`bind_style` 的**上次求值**结果）。
    style_patch: StylePatch,
    /// 动态属性闭包（`None` = 静态节点）。
    bind_style: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>>,
    /// 当前布局盒（逻辑像素；P04 写）。
    layout: LayoutBox,
    /// 上一次布局盒（damage 的「旧」一侧）。
    prev_layout: LayoutBox,
    /// 本帧累积的逻辑 damage AABB（§9.4）。
    damage: Rect,
    /// 该节点的绘制输出桶（按绘制序；P06 的 paint 分配，P02/P03 只声明）。
    lanes: SmallVec<[LaneId; 2]>,
    /// 上次求值实际读过的信号（差分清理用；`replace_deps` 的 `old`）。
    deps: SmallVec<[SignalId; 8]>,
    dirty: DirtyFlags,
}
```

| 字段 | 为何是它（不是别的） | `SmallVec` 容量理由 |
|---|---|---|
| `id` | 由 `Arena::insert_with` 铸入；`Node::id()` 供 Probe/诊断 | — |
| `parent`/`children` | taffy 的 `TraversePartialTree` 需要 `child_ids`（O(1) 取子）；`I5` 不允许第二棵树。**警告（A-N12）**：`NodeId` 是**窗口内** id，`NodeTree` 的公开方法**不做**跨树归属校验 —— 把 A 窗口的 `NodeId` 传给 B 窗口的 `tree.get()`，只要槽位/代际恰好相同就会命中 B 的另一个节点（不会 UB，但会静默写错对象）。P03 的正确用法是「一个 `Window` 的 id 只在它自己的 `tree` 上用」；`Owner::Node` 因此必须带 `window`（§8.1）。真正的归属校验需要给 `NodeId` 加 `tree_id`，P04/P08 若出现真实误用再 additive 加 | `[NodeId; 4]`：容器的典型子数 ≤ 4（`LaneId` 8 B、`NodeId` 8 B） |
| `style` / `style_patch` | I17 的两条通道 | 内联 `Style`（设计 §3.2 的 `style: Style`；`Node` 尺寸上限由 §17 的 `size_of::<Node>() <= 1024` 断言守住） |
| `bind_style` | 动态通道的闭包存储（§10.4） | — |
| `layout` / `prev_layout` | damage 的新旧 AABB（§9.4）；P04 的唯一写点 | — |
| `damage` | §9.4 | — |
| `lanes` | §5.1 的 `Node.lanes`；P06 填充 | `[LaneId; 2]`：绝大多数节点 1 条 lane（背景/边框在一条 lane 里），自绘节点 2 条。设计草图的 4 是过度预留 |
| `deps` | 差分清理的 `old`（§8.3）；`App::close_window` 的逐节点退订（§14.5） | `[SignalId; 8]`：一个 `bind_style` 读 1–5 个信号；8 覆盖绝大多数且只有 40 B |
| `dirty` | §9 | — |

**`Debug` 手写（N4）**：`Node` **不 `derive(Debug)`**（`bind_style: Box<dyn Fn>` 不实现 `Debug`）。手写 `impl fmt::Debug`
打印除闭包外的全部字段，闭包位固定写 `<closure>`（`Some(<closure>)` / `None`）。消费者：测试失败信息、P08 `Probe` 诊断。

**`taffy::Cache` 不内联进 `Node`（N6）**：布局缓存不放节点里 —— ①它是 taffy 的**算法状态**（`Cache` 未实现 `CacheTree`，
P01 附录 A.1 已记录），混进引擎节点会让 `Node` 的语义跨层；②它的容量（`Cache` 内部数组）远大于 `bounds`，内联会
直接顶破 `size_of::<Node>() <= 1024` 的预算。**决策**：P04 在 `NodeTree` 旁建一份**旁表**（`Vec<taffy::Cache>` 或
`HashMap<NodeId, Cache>`，按 P04 的访问模式二选一），`Node` 只保留逻辑像素的 `LayoutBox`。
`size_of::<Node>() <= 1024` 的断言仍保留（§17.5-8）；若实测超预算，备选是 `style: Box<Style>`（不改公开面，V-30）。

**P03 明确不加的字段（→ §4.4）**

| 设计 §3.2 的字段 | 为什么 P03 不加 |
|---|---|
| `a11y: Option<A11yNodeId>` | 类型是 P11 的 `A11yNodeId`；无写者/读者 → 会触发 `dead_code` |
| `clip: Option<ClipId>` | P06 才有 clip 表；`ClipId` 类型不存在 |
| `id` 之外的 `text`/`canvas` 内容 | 由 `NodeKind` 承载（§11.2） |

### 11.2 `NodeKind`

```rust
/// 节点的绘制/布局类别。P03 只落三种；`Text`/`Overlay`/`VirtualList` 由后续 Phase **additive** 加变体。
#[derive(Clone, Debug, PartialEq)]
pub enum NodeKind {
    /// 普通容器/叶子：背景 + 边框（P06 由 `StylePatch` 产出 `Quad`）。
    Box,
    /// 自绘节点：应用层提供的 `Canvas`（P02 的图元容器）是它的绘制内容。
    Canvas(Canvas),
    /// 静态几何节点：只持 `GeometryId`，每帧 uniform 变换（P08 的 `set_geometry_transform`）。
    Geometry(GeometryId),
}
```

| 决策 | 结论 | 理由 |
|---|---|---|
| `Text` 推迟到 P05 | 不加变体 | 载荷需要 `TextStyle`/`ShapedLine`（P05）；加了会是「无载荷的空变体」+ 无写者 |
| `Overlay`/`VirtualList` 推迟 P13 | 不加 | 同上是 P13 的交互机制，且 `Overlay` 的语义（层级/点击外部关闭）属 P13 |
| additive 扩展方式 | 加变体 + 在 `NodeBuilder` 加对应构造器；**不改**已有变体 | `publish = false`（P01 §6.2.1），破坏性变更允许；`NodeKind` 不加 `#[non_exhaustive]`（与 `EngineError` 同理：穷尽 match 更有价值） |
| `Canvas` 载荷放值还是 `Box` | **值**（`Canvas` 48 B，P02 实测） | `NodeKind` 已是 `Box(..)`/`Canvas(Canvas)` 的 56 B 级别；`Node` 本体 ~800 B，`Box` 只会多一次间接 |
| `Geometry` 载荷 | `GeometryId`（8 B，`Copy`） | 顶点在 P09 的 VBO；节点只持句柄（设计 §5.5） |
| 叶子判定 | `NodeKind::is_leaf(&self) -> bool`（`pub(crate)`） | 消费者：`dirty::depends_on_children`（叶子不自布局）、`NodeBuilder::build` 的 debug 断言（叶子不能有子）。**不公开**（应用层用 `matches!` 即可，且白名单未列） |

```rust
impl NodeKind {
    /// `Canvas`/`Geometry` 是**叶子**：不布局子节点、尺寸不受子内容影响（§9.3 的终止条件之一）。
    pub(crate) fn is_leaf(&self) -> bool;
}
```

### 11.3 `LayoutBox`

```rust
/// 布局结果（逻辑像素，I11：**只有逻辑像素**，物理化在 paint/present 边界）。
///
/// P03 只有 `bounds`：`baseline` 属 P05 的文本行内流（无消费者 → 不加，additive 扩展）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LayoutBox {
    /// 节点在**窗口逻辑坐标**下的 AABB（半开区间，P01 §6.3.2 的 `Rect`）。
    pub bounds: Rect,
}
```

| 决策 | 结论 | 理由 |
|---|---|---|
| 直接复用 `taffy::Layout`？ | **不** | `taffy::Layout` 带 `order`/`border`/`padding`/`margin`/`scrollable_overflow_rect`（§2.1），是 taffy 的内部结果；公开面会泄漏 taffy 的布局语义，且 `Node` 的 `layout` 是**引擎**的事实（I2「一套数据契约」） |
| 只有 `bounds` 够不够 P04/P06 | 够：P06 的 damage 用 `bounds`；P04 的 hit_test 用 `bounds`；P05 的基线由 `inline_flow` 自己的行盒承载 | — |
| 为什么不叫 `Layout` | 与 `taffy::Layout` 及 `layout_collections` 语义撞名 | — |

### 11.4 挂载 / 卸载 / 顺序

```rust
impl NodeTree {
    /// 引擎内部：校验 parent 存在 → 插入 arena → 挂到 parent 末尾（`NodeBuilder::build` 的消费者）。
    pub(crate) fn insert_child(&mut self, parent: NodeId, node: Node) -> Result<NodeId, EngineError>;

    /// 挂到末尾。child 已有父节点时**移动**（先从旧父摘下，再挂）。
    pub fn append_child(&mut self, parent: NodeId, child: NodeId) -> Result<(), EngineError>;

    /// 卸载 `node` 及其整棵子树（root 不可移除）。
    pub fn remove(&mut self, node: NodeId) -> Result<(), EngineError>;
}
```

**挂载流程（逐字）**

```text
NodeBuilder::build(tree, parent):
    tree.get(parent)?                        # 陈旧父 → Err(StaleNode { node: parent })
    id = tree.insert_child(parent, node)?    # arena.insert_with(|id| Node { id, .. }) + append 段 + mark_structure(parent)
    Ok(id)

insert_child(parent, node):
    self.get(parent)?                                     # 二次校验（append_child 也要它）
    id = arena.insert_with(|id| Node { id, parent: None, children: [], ..node })
    self.order_insert(parent, id)                         # §11.5；parent 已校验 ⇒ 不会失败
    node.parent = Some(parent); parent.children.push(id)
    mark_structure(parent)                                # LAYOUT|PAINT|ORDER|A11Y + 冒泡
    Ok(id)

remove(node):
    if node == self.root { return Err(StaleNode { node }) }     # root 不可移除
    self.get(node)?                                              # 陈旧 → Err(StaleNode)
    let parent = node.parent().expect("非 root 必有父")
    order_remove_subtree(node)                                   # 从绘制序里摘掉整段连续块
    parent.children.retain(|c| *c != node)
    for each id in subtree_dfs(node) reversed:                   # 后序：先子后父
        arena.remove(id)                                        # 代际 +1 → 旧句柄变陈旧
    mark_structure(parent)                                      # 父的结构发生变化
    Ok(())
```

| 决策 | 结论 | 理由 |
|---|---|---|
| `append_child` 的「移动」语义 | 有父则先摘再挂 | 这是 `append_child` 唯一自洽的语义（应用层可能把节点在容器间搬）；替代方案（要求 child 无父）会让 API 无法表达「移动」，且应用层拿不到「新建但未挂」的节点（`create` 是 `pub(crate)`） |
| 结构环（把祖先挂到自己的后代下） | `debug_assert!` + **提前返回 `Ok(())`**（release 下不改结构） | 应用层经 `NodeBuilder` 无法构造环（只追加新节点）；直接 `append_child` 可构造 → 视为内部不变量违反，按 P02 的「debug 断言 + 不 panic」风格处理（S-18 的同类决定）。**不加** `EngineError` 变体（冻结表不允许） |
| root 的移除 | `Err(StaleNode { node: root })` | 没有专用变体；`StaleNode` 是「这个句柄不能用于该操作」的最接近语义（并在 `Display` 里区分？不做，见 §16） |
| 卸载时 `lanes`/`deps` 的清理 | `lanes`：P03 无 lane（P06 的帧收尾归还）；`deps`：由 `App` 的惰性剔除 + 关窗精确清理（§14.5） | `NodeTree` 不持有 `Scene`/`DependencyGraph`（所有权在 `Window`/`App`） |
| `remove` 的复杂度 | O(子树节点数 + `order.len()` 的定位扫描) | §11.5 |
| 子节点容量建议 | `SmallVec<[NodeId; 4]>` | 见 §11.1 |

### 11.5 `order: Vec<NodeId>` 的稳定绘制序与局部重排

**定义**：`order` 是树的 **DFS 前序**（子节点按 `children` 顺序），含 root。性质：

| # | 性质 | 用途 |
|---|---|---|
| O1 | 任一根子树在 `order` 中占**连续一段** `[pos, pos+len)` | 局部重排只需 splice，不必重建全表 |
| O2 | 前序 ⇒ 父在子之前 | P06 的 lane 渲染顺序天然满足「父背景先于子内容」 |
| O3 | 顺序只由 `children` 顺序决定，**与 NodeId 无关** | 稳定：卸载/重挂不会打乱兄弟顺序 |

**最简可测实现（P03）**

```text
order_pos(node) -> usize            # O(n) 线性扫描（P03 的最简版）
subtree_len(node) -> usize          # O(子树) DFS 计数

order_insert(parent, child):        # child 是刚插入 arena 的新节点（无子）
    let at = order_pos(parent) + subtree_len(parent)     # parent 块的末尾
    order.insert(at, child)                              # Vec::insert，O(n) memmove
    debug_assert_eq!(order, dfs_preorder(root))           # 全序不变量（debug-only）

order_remove_subtree(node):
    let at = order_pos(node); let len = subtree_len(node)
    order.drain(at..at + len);
    debug_assert_eq!(order, dfs_preorder(root))

append_child（移动）: order_remove_subtree(child) → 摘下 children → order_insert(new_parent, child)
```

| 决策 | 结论 | 理由 |
|---|---|---|
| 是否维护 `Node.order_index` 加速 `order_pos` | **不**（P03） | 每个结构操作多一处不变量要维护（插入/移动/删除都要改全表后续索引）；P03 的建树是「一次性 mount + 少量结构变更」，O(n) 扫描足够。**V-28**：P13 的虚拟列表若出现每帧上千次结构变更，再加 `order_index`（additive 字段） |
| 为什么用 `Vec::insert`/`drain` 而不是重建 | 保持 O1 的「局部位移」；重建是 O(n) 全表 + 大量 drop/move | `insert`/`drain` 的 memmove 只动后缀 |
| `debug_assert_eq!(order, dfs_preorder(root))` | 全序不变量，只在 debug 构建跑 | 它是 O(n) 的；release 下由 O1–O3 的构造性保证 |
| 初始值 | `vec![root]` | — |
| `order` 与 `Scene::order()` 的区别 | 前者是**节点**绘制序（P03），后者是**lane** 绘制序（P06 每帧由前者展开，§13.3） | 两者不能混（比例尺不同：一个节点 0..n 条 lane） |

---

## 12. `node/builder.rs`（`NodeBuilder`）

### 12.1 签名

```rust
/// 声明式建树的唯一入口（§3.10 / §4.9.3）。**只在挂载时使用一次**。
///
/// **不 `derive(Default)`**（B3）：派生会给每个字段要 `Default`，而 `kind`/`style` 的默认值必须与
/// `boxed()` 完全一致；手写 `impl Default` 表达「默认 = `boxed()`」这一点，也避免 `NodeKind` 被迫实现 `Default`。
pub struct NodeBuilder {
    kind: NodeKind,             // 默认 `NodeKind::Box`
    style: Style,               // 默认 `Style::default()`
    bind_style: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>>,
}

impl Default for NodeBuilder {
    fn default() -> Self { Self::boxed() }
}

impl NodeBuilder {
    /// 普通容器/叶子。
    #[must_use] pub fn boxed() -> Self;
    /// 自绘节点：挂载时提交画布内容（P02 的 `Canvas`）。
    #[must_use] pub fn canvas(canvas: Canvas) -> Self;
    /// 静态几何节点。
    #[must_use] pub fn geometry(geometry: GeometryId) -> Self;
    /// 静态样式（覆盖 `Style::default()`）。
    #[must_use] pub fn style(self, style: Style) -> Self;
    /// 动态属性（§10.4）。
    #[must_use] pub fn bind_style(
        self,
        f: impl Fn(&mut ReadCtx<'_>) -> StylePatch + 'static,
    ) -> Self;

    /// 挂到 `parent` 末尾并返回新节点 id。
    ///
    /// # Errors
    /// `EngineError::StaleNode { node: parent }`：父节点不存在或已卸载（代际不匹配）。
    /// **这是唯一的 `Err`**（错误语义冻结见下表）。
    ///
    /// # Panics（仅 debug）
    /// 父节点是叶子（`NodeKind::Canvas`/`Geometry`）时 `debug_assert!` 失败；**release 下允许挂载，但该子树
    /// 永不产生布局盒、也永不产生任何像素**（P04 对 `Canvas`/`Geometry` 的子节点不做布局，§4.4）——
    /// 这是「已定义但不可见」的行为，不是 UB，也不是静默降级（debug 构建必然失败）。
    pub fn build(self, tree: &mut NodeTree, parent: NodeId) -> Result<NodeId, EngineError>;
}
```

**错误语义冻结（裁决 8：`StaleNode` 之外没有新的 `Err`）**

| 情形 | `build` 的行为 | 为什么不用 `Err` |
|---|---|---|
| 父节点不存在 / 已卸载 | `Err(StaleNode { node: parent })` | 应用层可触发，必须可诊断（I10） |
| 父节点是 `Canvas`/`Geometry`（叶子） | `debug_assert!(false, "leaf nodes cannot have children")`；release 下**挂载成功**，子树得到 `LAYOUT` 标记但 P04 不为其布局 | 冻结表没有「父不可容子」的变体；新增变体会破坏 P01 §6.2.2。且叶子节点带子是一种应用层建模错误，debug 构建立刻暴露 |
| 结构环（把祖先挂到自己的后代下） | 仅 `append_child` 可能（`build` 只追加新节点，不可能成环）：`debug_assert!` + 不改结构 + `Ok(())` | 同上；且这是内部不变量违反（§11.4） |
| `Style` 非法（负尺寸等） | `Ok`（照常挂载） | `Style` 是纯数据；非法值由 P04 的 taffy 布局与后置校验（`LayoutInvalid`，P05）负责 |
| 内存不足 / 分配失败 | 不处理（`Vec`/`Box` 的分配失败 = abort） | Rust 默认；引擎不引入 `try_reserve` 的错误面 |

| 决策 | 结论 | 理由 |
|---|---|---|
| `build` 返回 `Result<NodeId, EngineError>`（设计草图是 `NodeId`） | **Result** | 陈旧父 id 是应用层可触发的真实错误（挂到已被卸载的容器）；静默丢子树会变成 stale drawing。这同时给 `StaleNode` 一个真实生产者（P01 §6.2.2 #7）→ F-37 |
| `canvas()` 是否带参 | **带**（`canvas(canvas: Canvas)`） | 设计草图是无参 + 之后写入，但 `NodeKind::Canvas` 必须存内容；无参版本会要求 `NodeTree::canvas_mut`（P03 无消费者）。带参 = 挂载即定内容，P08 需要更新时再加 `set_canvas`（additive） |
| `.style()` 是否校验 `Style` | 不校验 | `Style` 是纯数据；非法值由 P04 的 taffy 布局暴露 |
| 默认 `kind` | `Box`（`NodeBuilder::default() == boxed()`） | **不 `derive(Default)`**（B3）：手写 `impl Default` 表达「默认 = `boxed()`」，避免 `NodeKind` 被迫实现 `Default` |
| 是否提供 `.a11y()`/`.bind_a11y()`/`.bind_text()` | **不** | P05/P11（§4.4） |

### 12.2 `build` 流程（逐字，含失败点）

```text
build(self, tree, parent):
    let p = tree.get(parent)?                          # ① 唯一失败点：StaleNode
    debug_assert!(!p.kind().is_leaf(), "leaf nodes cannot have children");   # ② 叶子父：debug 暴露，release 放行
    let node = Node {
        id: <由 arena 铸出>,
        parent: None,
        children: SmallVec::new(),
        kind: self.kind,
        style: self.style,
        style_patch: StylePatch::default(),
        bind_style: self.bind_style,
        layout: LayoutBox::default(),
        prev_layout: LayoutBox::default(),
        damage: Rect::default(),
        lanes: SmallVec::new(),
        deps: SmallVec::new(),
        dirty: LAYOUT | PAINT | A11Y,                  # 新节点必须布局、必画、必发 a11y
    };
    tree.insert_child(parent, node)                    # 内部再校验一次 parent（幂等）
```

### 12.3 `Children` / `Slot` / `NodeHandle` 的裁决

| 类型 | 结论 | 理由 |
|---|---|---|
| `Children`（子节点集合） | **推迟 P08** | 它属于「声明式建树的语法」，消费者是 `base_ui` 的 builder；P03 的等价能力是 `NodeBuilder::build(tree, parent)` + `NodeTree::append_child` |
| `Slot`（命名插槽） | **推迟 P08** | 无消费者（P08 的 `base_ui` 组合式组件才需要） |
| `NodeHandle`（可保存的节点句柄） | **推迟 P08** | P03 的 `NodeId` 已是完整句柄（`Copy` + 代际）；`NodeHandle` 的额外语义（跨窗口/自动失效）在 P08 才有需求 |
| 是否在 §1.3 白名单里 | 是（`NodeHandle`/`Slot`/`Children` 已列） | 白名单允许**将来**落地，不要求本阶段落地 → S-29 |

---

## 13. `paint/scene.rs`（`Scene` / `LaneId`）

### 13.1 槽位、代际与分配策略

```rust
/// lane 句柄（P02 §7.2 冻结形状：私有字段 + 代际）。8 B，`Copy + Eq + Hash`。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct LaneId {
    index: u32,
    generation: u32,
}

impl ArenaKey for LaneId { /* from_parts / parts */ }

/// 一帧的绘制指令（I2）。**持久**：lane id 跨帧稳定（`Node.lanes` 持有）。
///
/// **派生约束（裁决 ③）**：只 `derive(Debug)`。**不** `Clone`（lane 复用是搬不是拷，S-15）、
/// **不** `Default`、**无公开构造器**（`empty` 是 `pub(crate)`）→ 应用层无法伪造/复制场景，
/// 只能读 `Window::scene()`。
#[derive(Debug)]
pub struct Scene {
    lanes: Arena<Lane, LaneId>,
    /// 本帧的 lane 绘制序（P06 的 paint 阶段用 `set_order` 写入）。
    order: Vec<LaneId>,
}
```

| 项 | 结论 | 理由 |
|---|---|---|
| 槽位复用 | 是（`Arena` 的自由表） | 虚拟列表滚动时 lane 会反复分配/释放；不复用会持续涨 |
| 代际 | 是（`next_generation`，0 永不合法） | 释放后旧 `LaneId` 必须失效，否则会往**别人的** lane 里写图元（画错位置） |
| `lane_mut` 是否公开 | **否**（`Lane` 是 `pub(crate)`；`pub fn lane_mut -> &mut Lane` 会触发 `private_interfaces` 并让应用层看到 `Lane`） | 用 `Scene::push(lane, primitive)` 代替（唯一的写入点，且 `Lane::push` 的 clip 断言继续生效） |
| `release_lane` 是否保留容量 | 否（`Arena::remove` 会 drop 该 `Lane` 的 `Vec`） | `release_lane` 只在节点卸载时调用（低频）；**帧级复用**走 `clear_frame`（保容量），两条路径职责不同 |
| `MAX_CLIP_DEPTH` | 仍 `pub(crate) u8 = 8`；由 `Scene::push → Lane::push` 的 debug 断言真实读取 | P02 §7.1 的 C3 |

### 13.2 签名（P03 的完整公开面）

```rust
impl Scene {
    /// **唯一的构造点**（`pub(crate)`）：消费者 `Window::new`。应用层不可构造（裁决 ③）。
    pub(crate) fn empty() -> Self;

    // ── 写入面（**engine-internal，应用层不得调用**；仍为 `pub` 仅为满足 dead_code，见 S-33）──
    /// engine-internal, 应用层不得调用 —— 分配一条空 lane。
    pub fn new_lane(&mut self) -> LaneId;
    /// engine-internal, 应用层不得调用 —— 释放 lane（代际 +1，旧句柄立即失效）；陈旧句柄 → 无操作。
    pub fn release_lane(&mut self, id: LaneId);
    /// engine-internal, 应用层不得调用 —— 向 lane 追加一条图元（**唯一**写入点）。`false` = lane 句柄陈旧。
    ///
    /// 内部转调 `Lane::push`（更新 `clip_depth` 并做 C2/C3 的 debug 断言）。
    #[must_use] pub fn push(&mut self, lane: LaneId, primitive: Primitive) -> bool;
    /// engine-internal, 应用层不得调用 —— 清空**所有** lane 的内容但保留分配与容量（每帧 paint 的起点）。
    pub fn clear_frame(&mut self);
    /// engine-internal, 应用层不得调用 —— 写入本帧绘制序（P06 按「节点绘制序 × 该节点 lanes」展开后调用）。
    /// **契约**：`order` 由 P06 保证**无重复**（本方法不查重：那是 O(n) 热路径）。
    pub fn set_order(&mut self, order: &[LaneId]);
    /// engine-internal, 应用层不得调用 —— 撤销绘制序（`order.clear()`）。
    pub fn clear_order(&mut self);

    // ── 只读面（P02 §13.2 冻结；应用层唯一可达的部分）──
    /// 一条 lane 的图元切片（**不**返回 `&Lane`：`Lane` 是 `pub(crate)`，F-38/§2.4）。
    #[must_use] pub fn lane_primitives(&self, id: LaneId) -> Option<&[Primitive]>;
    #[must_use] pub fn lane_count(&self) -> usize;
    #[must_use] pub fn order(&self) -> &[LaneId];
    /// 按 `order` 展平（陈旧 id 静默跳过）。
    pub fn iter(&self) -> impl Iterator<Item = &Primitive> + Clone + '_;
    #[must_use] pub fn primitive_count(&self) -> usize;
    #[must_use] pub fn is_empty(&self) -> bool;
}
```

**与 P02 §7.2 冻结形状的差异（→ F-38 / F-39）**

| P02 冻结 | P03 落地 | 原因 |
|---|---|---|
| `pub fn lane(&self, id) -> Option<&Lane>` | **`pub fn lane_primitives(&self, id) -> Option<&[Primitive]>`**（去掉返回 `&Lane` 的访问器） | ①`Lane` 是 `pub(crate)`，公开签名含它会触发 `private_interfaces`（**§2.4 的 probe 实测 `-D warnings` 编译失败**）；②裁决 ③ 选了「方案②」：只给切片投影，公开面更小（`Lane` 的任何内部状态——`clip_depth`——都不泄漏，P06/P07 在 crate 内另有 `pub(crate)` 通道） |
| `pub(crate) fn lane_mut(&mut self, id) -> Option<&mut Lane>` | 不落地；改为 `pub fn push(&mut self, lane, primitive) -> bool` | 同上的可见性理由；`push` 覆盖 P03/P06 的全部写入需求，并保留 `Lane::push` 的 clip 断言 |
| `pub(crate) fn new_lane/release_lane/clear_frame/set_order` | `pub`（doc 首行标注 engine-internal） | P03 无 crate 内调用者 → `pub(crate)` 触发 `dead_code`（S-33，裁决 ③ 接受） |
| `pub fn new()` + `#[derive(Debug, Default)]` | **`pub(crate) fn empty()` + 只 `derive(Debug)`** | 裁决 ③：`Scene` 不实现 `Clone`/`Default`。无公开构造函数后 `Scene::new` 会触发 clippy `new_without_default`（`#[allow]` 被 AGENTS §4 禁止）→ 改名 `empty` 并降为 `pub(crate)`（`Arena` 也不再需要 `Default`）→ F-39 |

### 13.3 绘制序与只读面

| 需求 | 访问器 | 消费者 |
|---|---|---|
| lane → 图元 | `Scene::lane_primitives(id)` | P06 帧记录、P07 编码 |
| 本帧绘制序 | `Scene::order()` | P07 展开、P06 断言「顺序没变」 |
| 展平遍历 | `Scene::iter()` | 编码器 / 帧记录 / full-refresh oracle 的单一入口 |
| 图元总数（**按绘制序统计**） | `Scene::primitive_count()` | `is_empty` + P06 断言；未进 `order` 的 lane 不计，回答的是「这一帧要画多少」 |
| lane 数 | `Scene::lane_count()` | P06 的 `reused_lanes` |

`iter()` 的精确实现：

```rust
pub fn iter(&self) -> impl Iterator<Item = &Primitive> + Clone + '_ {
    self.order.iter().filter_map(|id| self.lane_primitives(*id)).flatten()
}
```

> `filter_map` 让陈旧 `LaneId`（被 `release_lane` 后又留在 `order` 里）**静默跳过**而不是 panic：
> `set_order` 不做 O(n) 校验（每帧调用的热路径），`iter` 容错是更便宜的安全侧。

### 13.4 `Scene` 对 `test_support`（P06）的只读面

| P06 需要 | P03 已提供 | 缺口 |
|---|---|---|
| `RenderedFrame.reused_lanes` | `Scene::{order, lane, lane_count, primitive_count, is_empty, iter}` | 无（`reused_lanes` 由 P06 用两帧的 `LaneId` 集合差算出） |
| lane 内 clip 平衡断言（C1） | — | `Lane::clip_depth()` 是 `pub(crate)`，P06 同 crate 可用（无需公开） |
| `Scene` 值可比较（oracle） | `Primitive: PartialEq` + `Scene::iter()` 的顺序比较 | `Scene` 本身**不**实现 `PartialEq`（P02 冻结：`Scene` 不 `Clone`；`iter()` 已足够） |

---

---

## 14. `app.rs`（`App` 的写路径与样式求值 pass）

### 14.1 构造与字段初始化

```text
App::new(runtime):
    App {
        store: StateStore::with_capacity(64, 16),
        graph: DependencyGraph::new(),
        runtime,
        windows: ahash::HashMap::default(),
        next_window_id: 1,          # 0 保留（永不是合法 WindowId，便于诊断）
        eval_stack: EvalStack::new(),
        wake_requested: false,
    }
```

> `with_capacity` 的初值：一个交易终端的信号量在数百级（AGENTS §2.3「尽可能提供合理的初始容量」）；
> 数值不是契约，测试**不**断言容量。

### 14.2 `App::write` 的完整流程（逐字）

```text
write(&mut self, signal: &Signal<T>, value: T) -> bool:
    let id = signal.id
    # 1) 相等短路（§7.5）；owned_ref 只借用，不克隆
    if store.owned_ref::<T>(id) == Some(&value) { return false }
    # 2) 写值（无版本计数：S-27）
    store.set_owned(id, value);
    # 3) 一跳反查 + 级联派生失效（worklist，非递归）
    let App { graph, windows, store, .. } = self;            # 字段拆分借用（循环外一次）
    let mut worklist: SmallVec<[SignalId; 8]> = smallvec![id];
    let mut marked_nodes = 0usize;
    while let Some(sid) = worklist.pop():
        let Some(owners) = graph.dependents_mut(sid) else { continue };
        # 就地剔除死 owner（窗口已关/节点已卸载）；D5 的惰性清理点
        owners.retain(|owner| match owner {
            Owner::Node { window, node } => windows.get(window)
                .is_some_and(|w| w.tree.get(*node).is_ok()),
            Owner::Derived(_) => true,                        # 派生永不删除 → 恒存活（§7.6）
        });
        if owners.is_empty() { graph.remove_signal(sid); continue }
        for owner in owners.iter().copied():
            match owner:
                Owner::Node { window, node } =>
                    let Some(w) = windows.get_mut(&window) else { continue };
                    # 4) 用节点**上次求值**的 patch 决定 PAINT 还是 LAYOUT（§9.5）
                    let layout = w.tree.get(node)
                        .map(|n| n.style_patch().affects_layout)
                        .unwrap_or(false);
                    let alive = if layout { w.tree.mark_layout(node) } else { w.tree.mark_paint(node) };
                    if alive { marked_nodes += 1 }            # false → 下轮 retain 剔除
                Owner::Derived(d) =>
                    # 5) 派生失效：置 dirty，并把它的句柄 id 压入 worklist（级联）
                    if store.mark_derived_dirty(d) { worklist.push(store.derived_handle(d)) }
    # 6) 唤醒请求（只由「有节点被标脏」触发，§14.3）
    if marked_nodes > 0 { self.wake_requested = true }
    true
```

| 决策 | 结论 | 理由 |
|---|---|---|
| 为什么用 worklist 而不是递归 | 派生链可以很深（指标派生自指标）；worklist 把栈深度变成堆（`SmallVec` 内联 8） | 递归在深链上会爆栈；worklist 天然支持环（`dirty` 已是 true 就不再入队 → 有环也终止） |
| 级联的终止条件 | `mark_derived_dirty` 返回「本次由 clean→dirty」才入队 | 同一派生被多次失效只传播一次（幂等 + 有环安全） |
| 死 owner 的剔除时机 | 在同一跳内 `retain` | 写路径是唯一的「必然遍历该集合」的地方 → 零额外成本（§8.3 D5） |
| 陈旧 `NodeId` 的处理 | `mark_*` 返回 `false` → 不计数；下一轮 `retain` 剔除 | 不构造 `EngineError`：写路径不能失败（§6.3） |
| 写的返回值 | `bool` = **值是否变了**（不是「有节点失效」） | 应用层用它做「是否需要滚动到底/记日志」；「有没有帧」由 `take_wake_request` 回答 |
| 写操作是否加锁/重入保护 | 无 | `&mut self` 已在编译期串行化；P06 的「paint 回调内禁止写」见 §14.6 |

### 14.3 「需要唤醒帧」在 P03 的表达

| 项 | 结论 |
|---|---|
| API | `pub fn take_wake_request(&mut self) -> bool`（取走即清零） |
| 置位条件 | 至少**一个节点**被 `mark_paint`/`mark_layout` 成功（`marked_nodes > 0`）；**只**有派生失效时**不**置位 |
| 为什么只有派生失效不置位 | 派生是无副作用的缓存；没有节点变脏 ⇒ 没有任何像素要重画（I3「空闲不提交帧」）。派生会在下次被读时自行惰性重算 |
| 为什么不用 `PlatformDispatcher` | P03 没有平台层/事件循环（§4.4）；`PlatformDispatcher::wake(FrameWake)` 在 P09 落地后取代本方法的**同一处调用点**（§3.5 的 `Invalidate → wake` 边）。这不是过渡层：`wake_requested` 是帧调度器（P06/P09）本来就需要的状态位 |
| 测试可断言性 | `write` 后 `assert!(app.take_wake_request())`；第二次 `assert!(!app.take_wake_request())`（取走即清）；`StylePassReport.wake_requested` 是**不消费**的镜像值（§14.4） |

### 14.4 `App::refresh_styles`（P03 的样式求值 pass）

```text
refresh_styles(&mut self, window: WindowId) -> Option<StylePassReport>:
    let App { store, graph, windows, eval_stack, wake_requested, .. } = self;
    let Some(w) = windows.get_mut(&window) else { return None };      # 未知窗口 → None
    debug_assert!(eval_stack.is_empty(), "上一帧的派生求值栈泄漏");
    eval_stack.clear();                                              # 防御性兜底（§8.5）
    let mut evaluated = 0usize;
    let mut i = 0;
    while i < w.tree.order().len():
        let node = w.tree.order()[i];  i += 1;                        # 拷贝 id，借用立即结束
        if !w.tree.get(node).map(|n| n.dirty().contains(PAINT)).unwrap_or(false) { continue }
        let old = w.tree.get(node).map(|n| n.style_patch().clone()).unwrap_or_default();
        let Some(compute) = w.tree.take_bind_style(node) else {
            w.tree.clear_paint(node);  continue                          # 静态节点：只消费 PAINT
        };
        let (values, derived) = store.split();                       # 拆分借用（§7.3）
        let mut tracker = ReadTracker::new();                        # 本节点的读取集合
        let mut ctx = ReadCtx::new(values, derived, graph, &mut tracker, &mut *eval_stack,
                                  Owner::Node { window, node });     # B4：owner 由 ctx 提供
        let patch = compute(&mut ctx);                               # 求值（可读信号/派生）
        let owner = ctx.owner();                                     # B4：`ReadCtx::owner()` 的唯一消费者
        drop(ctx);                                                   # 释放借用（tracker 仍归本帧所有）
        evaluated += 1;
        # ① 依赖替换（差分），old = 节点上次的 deps（§8.3）
        let old_deps = w.tree.deps(node).unwrap_or(&[]);
        graph.replace_deps(owner, old_deps, tracker.as_slice());
        w.tree.set_deps(node, tracker.into_inner());
        # ② 写回 patch（先算升级标志，再写）
        let layout = patch.affects_layout || old.affects_layout;
        let a11y = patch.interactive != old.interactive || patch.visible != old.visible;
        w.tree.set_style_patch(node, patch);
        w.tree.put_bind_style(node, compute);
        # ③ 升级 + 消费 PAINT（clear_paint 必须最后：mark_layout 会重新置 PAINT）
        if layout { w.tree.mark_layout(node); }
        if a11y { w.tree.mark_a11y(node); }
        w.tree.clear_paint(node);
    # ④ 汇总报告（只统计本窗口）
    Some(StylePassReport {
        evaluated,
        layout_dirty: w.tree.any_node(|n| n.dirty().contains(LAYOUT)),
        paint_dirty:  w.tree.any_node(|n| !n.dirty().contains(REUSABLE)),
        wake_requested: *wake_requested,
    })
```

> `any_node` 是 `NodeTree` 的 `pub(crate)` 只读遍历（按 `order()` 短路返回第一个满足者）；两个字段共用它，
> 因此它有非 test 消费者。报告**不消费**唤醒标志（`take_wake_request` 仍是唯一的消费点，§14.3）。

| 决策 | 结论 | 理由 |
|---|---|---|
| 为什么这个 pass 必须在 P03 落地 | 它同时是 `ReadCtx`/`ReadTracker`/`DependencyGraph::replace_deps`/`Derived` 求值/`EvalStack`/`StylePatch` 比较/`affects_layout`/`Node.deps`/`bind_style` 字段的**唯一非 test 消费者**。没有它，这 10 项全部 `dead_code` 或只能靠测试 → 违反 P02 §3.2 的 C1/C2 | §4.3 的审计表 |
| **它不产生 Scene 图元** | 只更新 `style_patch` / `deps` / `dirty` / damage AABB；`Scene` 一个字节都不写 | 绘制指令的生产属 P06（裁决 6）。P03 的 `Scene` 只由测试/未来 pass 写入 |
| 它在帧里的位置 | `drain（写）→ refresh_styles → layout（P04）→ paint lane（P06）→ damage（P06）` | `affects_layout` 的补丁必须在**布局之前**求值，否则布局用的是旧输入 |
| 为什么按窗口 | 帧是按窗口推进的；报告也只统计这个窗口 | 多窗口下「另一个窗口的脏位」不该影响本次 pass 的判定（§5.1 的 `windows` 表） |
| 返回 `Option<StylePassReport>` 而非 `StylePassReport` | `None` = 该 `WindowId` 不存在 | P03 没有「未知窗口」的错误变体（P01 §6.2.2 冻结表）；`Option` 是唯一诚实的表达，调用方（应用层/P06 帧）持有效 id 时永不命中 `None` |
| 为什么遍历 `order` 而不是 arena 顺序 | `order` 是稳定的绘制序 ⇒ 求值顺序确定 ⇒ 依赖集合的插入序确定（§8.1 的「序」规则） | 也避免「arena 槽位顺序 = 历史分配顺序」的不确定 |
| 为什么按索引 `while` 循环 | `order()` 的借用与 `get_mut` 冲突；索引拷贝后借用结束（§11.5） | 零分配（不 clone 整个 order） |
| `set_style_patch` 之后再 `clear_paint` | 顺序无关（两个不同位），但 `mark_layout` 会重新 `insert(PAINT)` ⇒ `clear_paint` **必须最后** | 否则 `PAINT` 残留，下一帧重复求值 |
| 幂等性 | 同一窗口再调一次 → 所有节点已无 `PAINT` → `evaluated == 0` | 测试可断言 |
| 与 P06 的关系 | P06 的 paint 阶段在重建 lane **之前**调用同一实现（同一函数，不是副本），然后按 `!REUSABLE` 重建 lane | 唯一实现的代码路径 → 不出现「两套样式求值」 |

### 14.5 窗口生命周期与依赖图清理

```text
create_window(options):
    let id = WindowId(self.next_window_id); self.next_window_id += 1;
    self.windows.insert(id, Window::new(id, options));
    id

close_window(id):
    let Some(window) = self.windows.remove(&id) else { return false };
    # 精确退订：每个节点的 deps 是已知的（O(节点数 × 平均 deps)）
    for node in window.tree.order():
        for signal in window.tree.deps(*node).unwrap_or(&[]):
            self.graph.detach(*signal, Owner::Node { window: id, node: *node });
    true
```

| 决策 | 结论 | 理由 |
|---|---|---|
| 为什么关窗要精确退订 | 不断言则 `dependents` 里留下整棵死树（内存泄漏 + 每次写都走 `retain` 的无谓扫描） | §3.9「空集合立即移除，避免表膨胀」 |
| 为什么单个节点卸载（`NodeTree::remove`）不精确退订 | `NodeTree` 不持有 `DependencyGraph`（所有权在 `App`），应用层拿 `&mut NodeTree` 时无法回调 `App` | 折中：`App::write` 的 `retain` 惰性剔除（O(#dependents)，写路径本来就要遍历）+ 关窗时精确清理。**已知残留**：长期不写的信号可能残留少量死 owner（上界 = 历史 owner 数）→ V-29 |
| `WindowId` 是否复用 | **不**（单调 +1，0 保留） | 陈旧句柄落回 `None`，无需代际（§5.2） |
| `next_window_id` 溢出 | `u32` 用尽（40 亿）后 `wrapping_add` 会复用 → 文档写明「不可达」，不做保护 | 预防性检查无消费者（AGENTS §1.2） |
| P04 交接 | `NodeTree::remove` 卸载子树后，P04 应调用 `App::purge_owners(window, subtree_ids)`？**不需要**：`retain` 已覆盖。若 P08 要严格的即时回收，additive 加 `App::remove_node(window, node)` | — |

### 14.6 「paint 回调中禁止写」的接口预留

| 项 | P03 | P06 |
|---|---|---|
| 现状 | `App::write` 的 `&mut self` 已经与 `refresh_styles` 的 `&mut self` 互斥；但 `bind_style` 闭包拿到的是 `&mut ReadCtx`（不带 `&mut App`）⇒ **闭包内物理上无法写** | 同 |
| P06 的需求 | paint 回调（`Canvas` 的生产者）若持 `&mut App` 就能在绘制期写状态 → 必须禁止 / 延后到下一帧 drain | 设计 §3.10 的禁令 |
| P03 的预留 | `ReadCtx` **不**暴露 `App`/`StateStore` 的可变引用（只有 `read*`）⇒ 「读取者不能变成写入者」由类型保证，不需要运行时断言 | 若 P06 引入 `paint(app: &mut App)` 形态，则在其入口加 `debug_assert!(!self.in_frame)` 或改为 `&App`（那时才定） |

---

## 15. `window.rs` 的交接要点

| 项 | P03 | 接手 |
|---|---|---|
| `Window` 持有 `NodeTree` + `Scene` + `Scale` + `WindowOptions` | ✅ | — |
| 平台窗口句柄、DPI 事件、标题栏 | ❌ | P12（`platform/windows/window.rs` 等） |
| 多个窗口的帧循环 | ❌ | P09（`Window` 加 `FrameScheduler` 字段） |
| `Window::set_scale` 的完整语义（重建 offscreen、scissor 更新） | 只标根 `LAYOUT` | P06/P12 |
| `Window::scene()` 的只读面 | ✅ | P06 加 `reused_lanes` 计算 |
| `Window::scene_mut()` | ❌（**不**提前加：P03 无调用者） | P06 的 paint 阶段需要写 `Scene`（`new_lane`/`push`/`clear_frame`/`set_order`）；届时**additive** 加 `pub(crate) fn scene_mut(&mut self) -> &mut Scene`，公开面不变（应用层仍只有 `scene()`） |
| `NodeTree::mark_structure` 的子树语义 | 只标**该节点**的 `LAYOUT\|PAINT\|ORDER\|A11Y` 并向祖先冒泡；子树的 `LAYOUT`/`PAINT` 由 P04 的布局遍历按需展开 | P04 的 `set_layout` 落地时若发现「结构变更必须立刻标整棵子树」，应改为在 `append_child`/`remove` 里对受影响子树调用 `mark_subtree_paint`（P03 只在**重挂**时这样做，见 §9.3） |

---

## 16. 错误面（`src/error.rs` 追加）

```rust
use crate::node::id::NodeId;
use crate::node::signal::DerivedId;

// 追加到 enum EngineError（位置按 P01 §6.2.2 的编号顺序：放在 InvalidDpiScale 之后）

/// 节点句柄已陈旧：节点被卸载（代际 +1）或从未存在。
#[error(
    "stale node handle {node:?}: the node was unmounted or never existed in this window"
)]
StaleNode { node: NodeId },

/// 派生值形成环：求值栈上再次遇到同一个 `DerivedId`。
#[error(
    "cyclic derived value {derived:?}: its compute closure depends on itself (directly or \
     through another derived value)"
)]
CyclicDerived { derived: DerivedId },
```

| 项 | 结论 | 依据 |
|---|---|---|
| 变体名 / payload | 与 P01 §6.2.2 #7/#8 **逐字一致** | 冻结表 |
| 实现 Phase | 表内写 P04 → **改为 P03** | 本阶段落地（→ S-30） |
| `source()` | `None`（不包装外部错误） | P01 §6.2.2 的全局约定；`error.rs` 的 `source_is_none` 测试需把两个新变体加入 `all_variants()` |
| `Display` 语言 | 英文诊断文本 | P01 §6.2.3 / F-07 |
| `#[non_exhaustive]` | 不加 | P01 §6.2.1 |
| `StaleNode` 的 payload 语义 | 「这个句柄不可用」的**唯一**节点错误：①`get` 查不到（从未存在/已卸载）；②`build`/`append_child` 的父陈旧；③**`remove(root)`** —— 语义是「**不是可移除的子节点**」（root 没有父，不能按「卸载子树」处理）。**见 N11** | 冻结表不允许新增变体；`Display` 文案「unmounted or never existed」对 ③ 的表述偏弱，故在 `remove` 的 rustdoc 里写明「root 不是可移除的子节点 → `StaleNode`」，测试 §17.5-4 固化 |
| `InvalidMeasure` | **不在本阶段** | 冻结表写 P05（measure 闭包属 P05） |

---

## 17. 测试清单（计划 **82** 个 / 实测 **97** 个，全部内联 `#[cfg(test)] mod tests`）

> 组织：每个文件的用例放在该文件末尾（AGENTS §5.1）。`App` 相关用例需要 tokio 句柄：
> `fn handle() -> tokio::runtime::Handle { tokio::runtime::Builder::new_current_thread().build().unwrap().handle().clone() }`
> （`rt` 能力由 workspace 的 `full` 提供；不 spawn，故不需要 `enable_all`）。

### 17.1 `node/id.rs`（**7**）

| # | 用例 | 断言要点 |
|---|---|---|
| 1 | `node_id_is_8_bytes` | `size_of::<NodeId>() == 8`（设计 §3.2 的 8 B） |
| 2 | `node_id_to_bits_layout` | `for_test(3, 7).to_bits() == (7 << 32) \| 3`；`index()/generation()` 与 `for_test` 逆运算一致 |
| 3 | `arena_insert_mints_generation_one` | 首次 `insert_with` → `generation() == 1`、`index()` 递增、`len()` 增长 |
| 4 | `arena_get_rejects_stale_after_remove` | `remove(id)` → `get(id) == None`、`get_mut == None`、`remove(id)` 再调 == `None`（幂等） |
| 5 | `arena_reuses_index_with_bumped_generation` | remove 后再 insert → 同 `index`、`generation == 旧 + 1`；旧 id 仍 `None` |
| 6 | `arena_generation_skips_zero_on_wrap` | `for_test(0, u32::MAX)` 槽位 remove 后复用 → `generation == 1`（`next_generation` 直接测：`next_generation(u32::MAX) == 1`、`next_generation(1) == 2`） |
| 7 | `arena_values_mut_covers_only_live` | 插入 3 个、移除中间一个 → `values_mut()` 恰好 2 项，`len() == 2` |

### 17.2 `node/signal.rs`（**12**）

| # | 用例 | 断言要点 |
|---|---|---|
| 1 | `signal_id_is_dense_and_never_reused` | 连续 `signal()` 的 `id.index()` 为 0,1,2（稠密）；**没有** remove API（编译期证据） |
| 2 | `signal_handle_is_copy_without_t_bound` | `struct NotCopy; let s: Signal<NotCopy> = …; let t = s; let _ = s;`（Copy 与 `T: Copy` 无关） |
| 3 | `read_clones_value` | `read::<String>` 返回克隆；改返回值不影响存储 |
| 4 | `write_equal_value_short_circuits` | 写同值 → 返回 `false`、节点 `dirty` 不变（无 `PAINT`/`LAYOUT` 变化）、`take_wake_request() == false` |
| 5 | `write_changed_value_invalidates` | 写不同值 → 返回 `true`、读者节点被标 `PAINT`、`take_wake_request() == true` |
| 6 | `equal_but_interior_mutable_value_is_not_repainted` | **等值短路的风险固化**：`T = Rc<Cell<u32>>`（`PartialEq` 比指针）→ 原地改 `Cell` 后 `write` 同一个 `Rc` → 返回 `false`、节点不脏（应用层必须用不可变值语义，§7.5） |
| 7 | `transient_write_never_invalidates` | 节点读 transient → `write_transient` → 节点的 `dirty` 不变、`take_wake_request() == false` |
| 8 | `transient_read_is_not_tracked` | 在 `bind_style` 里只读 transient → `node.deps()` 为空 |
| 9 | `derived_is_lazy` | 注册后未被求值（`Rc<Cell<u32>>` 计数 == 0）；首次 `read_derived` 后 == 1 |
| 10 | `derived_caches_until_invalidated` | 连续两次 `read_derived` → 计数 == 1；写依赖信号 → 第三次读 → 计数 == 2 |
| 11 | `derived_chain_propagates_invalidation` | `a = derived(read(s))`、`b = derived(read_derived(a))`：写 `s` → 读 `b` 得到新值；`b` 的 `deps` 是 `a` 的**句柄** id（不是 `s`） |
| 12 | `derived_cycle_is_reported_not_panicking` | 互引的两个派生（`Rc<RefCell<Option<_>>>`）：内层闭包把 `Err(CyclicDerived)` 记进 `Rc<Cell<bool>>` 并返回默认值 → 断言 `seen == true` 且 `read_derived` 的外层不 panic |

### 17.2b `node/signal.rs` + `node/dependency.rs` 的 panic 安全（**1**，放 `dependency.rs` 的 `mod tests`）

| # | 用例 | 断言要点 |
|---|---|---|
| 1 | `eval_guard_pops_on_panic_and_cache_survives` | `std::panic::catch_unwind(AssertUnwindSafe(\|\| 读 a))`，其中 `a` 的闭包 panic：①求值栈事后**为空**（`EvalGuard::drop` 生效）；②`a` 的 `dirty` 仍为 `true`、缓存仍为空（未污染）；③再次读取 `a` 得到 `Err(CyclicDerived)` 而不是 panic（`take_compute == None` 的二道防线；N8：**不得**把该错误当作「存在环」的唯一判据）；④同一 `App` 上其它派生仍可正常求值 |

### 17.3 `node/dependency.rs`（**8**）

| # | 用例 | 断言要点 |
|---|---|---|
| 1 | `read_tracker_dedupes` | 同一 `SignalId` 记 3 次 → `as_slice().len() == 1`；顺序 = 首次出现序 |
| 2 | `replace_deps_adds_new_edges` | old=[] new=[a,b] → `dependents(a)`/`dependents(b)` 各含该 owner |
| 3 | `replace_deps_removes_missing_edges` | old=[a,b] new=[b] → `dependents(a) == None`（空集合删条目）、`dependents(b)` 仍含 owner |
| 4 | `replace_deps_is_idempotent` | 连续两次 `replace_deps(old=[a], new=[a])` → 集合里 owner **只一份**（D1 查重） |
| 5 | `replace_deps_replaces_not_accumulates` | 条件分支：第一次读 `a`、第二次读 `b` → 最终只依赖 `b`（D4，幽灵依赖用例） |
| 6 | `detach_removes_edge_and_entry` | `detach(a, owner)` → owner 消失；集合空 → 表项删除 |
| 7 | `owner_set_inline_capacity` | 2 个 owner 不分配（`OwnerSet::spilled() == false`，smallvec 有该断言 API）；第 3 个才 spill |
| 8 | `eval_stack_push_pop_and_contains` | push 后 `contains == true`、`is_empty == false`；pop 后归零；重复 push 同一 id 仍只算「栈上」（环检测语义） |

### 17.4 `node/dirty.rs`（**12**）

| # | 用例 | 断言要点 |
|---|---|---|
| 1 | `dirty_flag_bit_values` | 六个位的 `bits()` 恰为 `1,2,4,8,16,32`（冻结位定义） |
| 2 | `new_node_is_layout_paint_a11y` | `NodeBuilder::boxed().build()` → `dirty == LAYOUT \| PAINT \| A11Y`，`REUSABLE` 未置 |
| 3 | `mark_paint_does_not_bubble` | 孙子 `mark_paint` → 父/祖父 `dirty` **不含** PAINT/LAYOUT；孙子含 PAINT |
| 4 | `mark_paint_clears_reusable` | 先手动给 `REUSABLE`（同模块测试可写）→ `mark_paint` 后不含 `REUSABLE` |
| 5 | `mark_layout_bubbles_to_root` | 全 `auto` 尺寸链：叶子 `mark_layout` → 所有祖先含 LAYOUT（含 root） |
| 6 | `bubble_stops_at_fixed_size` | 父 `size = (percent(0.5), percent(0.5))` → 叶子 `mark_layout` 后父含 LAYOUT、**祖父不含** |
| 7 | `bubble_stops_at_stretch` | 父 `size = (stretch, stretch)` → 同上（`ExpandedDimension::Stretch` 不依赖子） |
| 8 | `bubble_skips_absolute_child` | 子 `position = Absolute` → 子 `mark_layout` 后**父不含 LAYOUT**（`affects_parent == false`） |
| 9 | `bubble_skips_hidden_child` | 子 `display = None` → 父不含 LAYOUT |
| 10 | `canvas_and_geometry_nodes_stop_bubbling` | `NodeKind::Canvas`/`Geometry` 节点自身 `mark_layout` → 父不含 LAYOUT（叶子不自布局） |
| 11 | `repeated_mark_layout_is_cheap_and_idempotent` | 两次 `mark_layout(同一叶子)` → 祖先仍只是 LAYOUT（无重复副作用）；第二次在父已 LAYOUT 处早停（断言只走一层 → 用「祖父的 `damage` 未变」间接验证） |
| 12 | `layout_escalation_marks_fixed_size_node_ancestors` | **N7**：叶子 `size = (Length, Length)`（固定尺寸）且其 patch 升级为 `affects_layout` → `mark_layout(叶子)` 后父**必须**含 `LAYOUT`；父是 `auto` 链时祖父也含 `LAYOUT`（证明早停条件不会把「固定尺寸节点触发的升级」挡掉） |

### 17.5 `node/tree.rs`（**9**）

| # | 用例 | 断言要点 |
|---|---|---|
| 1 | `new_tree_has_single_root` | `order() == [root()]`、`get(root)` 成功、root 的 `parent() == None` |
| 2 | `append_child_moves_node` | 子挂到 A 再挂到 B → A 的 `children` 不含它、B 含、`parent() == B`；`order` 仍满足前序 |
| 3 | `remove_subtree_frees_all_slots` | 父 + 2 子：`remove(parent)` → 三者 `get` 全部 `Err(StaleNode)`；`order().len()` 回到 1 |
| 4 | `remove_root_is_stale_error` | `remove(root)` → `Err(StaleNode { node: root })`，树不变 |
| 5 | `remove_marks_parent_structure` | `remove(子)` → 父含 `ORDER \| LAYOUT \| PAINT \| A11Y` |
| 6 | `order_is_dfs_preorder` | 手工建 A(B(C,D),E) → `order == [root, A, B, C, D, E]` |
| 7 | `order_survives_append_in_middle` | 在已有 B、C 的情况下再挂 D → `order == [root, A, B, C, D]`（顺序 = children 顺序，不按 id） |
| 8 | `node_size_budget` | `size_of::<Node>() <= 1024`（§2.5 的兜底；`Style` 实测值待 P04 回填） |
| 9 | `damage_unions_prev_and_current` | 同模块测试直接设 `prev_layout`/`layout` → `mark_paint` 后 `damage() == 两者并集`；再 `mark_layout` 不重复放大 |

### 17.6 `node/builder.rs`（**6**）

| # | 用例 | 断言要点 |
|---|---|---|
| 1 | `build_boxed_sets_kind_and_style` | `kind() == &NodeKind::Box`；`style()` 与传入相等；`style_patch() == StylePatch::default()` |
| 2 | `build_canvas_and_geometry_kinds` | `NodeKind::Canvas(c)` 的 `c.bounds()` 与传入一致；`NodeKind::Geometry(id)` 的 id 相等；两者 `is_leaf() == true` |
| 3 | `build_stale_parent_is_error` | 对已卸载的 parent `build` → `Err(StaleNode { node: parent })`；树节点数不变（无孤儿） |
| 4 | `build_stores_bind_style` | 带 `bind_style` 的节点：`refresh_styles` 后 `style_patch()` 是闭包算出的值（证明闭包真的存进去了） |
| 5 | `builder_default_is_box` | `NodeBuilder::default()` 等价 `boxed()`（kind 为 `Box`、`Style` 为默认） |
| 6 | `leaf_parent_is_debug_assert` | 父是 `Canvas` 节点时 `build` → debug 下 panic（`#[should_panic(expected = "leaf nodes cannot have children")]`，`#[cfg(debug_assertions)]`）；release 下 `Ok`（§12.1 的错误语义表） |

### 17.7 `paint/style.rs`（**3**）

| # | 用例 | 断言要点 |
|---|---|---|
| 1 | `style_default_is_taffy_default` | `Style::default().inner == taffy::Style::<SmolStr>::DEFAULT`；`inner.display == Display::Flex`（§2.1 的 DEFAULT） |
| 2 | `style_patch_default_is_empty` | 全部 `Option` 为 `None`、`affects_layout == false`；`StylePatch::default() == StylePatch::default()` |
| 3 | `cursor_default_is_default` | `Cursor::default() == Cursor::Default`；`NotAllowed != Pointer` |

### 17.8 `paint/scene.rs`（**9**，追加到 P02 已有的 6 个 Lane 用例之后）

| # | 用例 | 断言要点 |
|---|---|---|
| 1 | `scene_empty_is_empty` | `Scene::empty()` → `lane_count() == 0`、`order()` 空、`is_empty()`；**没有** `Default`/`Clone`（编译期证据：§13.2 的派生约束） |
| 2 | `new_lane_ids_are_distinct_and_dense` | 3 条 lane 的 `index()` 为 0,1,2、`generation == 1` |
| 3 | `release_then_reuse_bumps_generation` | 释放后再 `new_lane` → 同 index、`generation == 2`；旧 id 的 `lane_primitives()` 为 `None`、`push()` 返回 `false` |
| 4 | `push_appends_in_order_to_one_lane` | 同 lane 连续 3 个图元 → `lane_primitives(id)` 顺序一致；别的 lane 不受影响 |
| 5 | `push_tracks_clip_depth_via_lane` | `push(Clip(Push))` → `Scene::iter()` 输出含 clip；`push` 超过 `MAX_CLIP_DEPTH` 在 debug 下 panic（`#[should_panic]` **+ `#[cfg(debug_assertions)]`**，与 P02 的 `Lane::push` 断言一致；release 下该用例**不编译**，N2） |
| 6 | `set_order_controls_iter` | 两条 lane 各自 2 个图元；`set_order([b, a])` → `iter()` 先 b 后 a；`clear_order()` → `iter()` 空 |
| 7 | `iter_skips_stale_lane_ids` | `set_order([a, 释放后的 a])` → `iter()` 只输出存活 lane 的图元（不 panic） |
| 8 | `clear_frame_keeps_lanes_and_capacity` | 写入 3 个图元后 `clear_frame()` → `is_empty()`、`lane_count()` 不变、`lane_primitives(a)` 为空切片；再次 `push` 成功 |
| 9 | `clear_frame_resets_clip_depth` | `push(Clip(Push))` 后 `clear_frame()` → 该 lane 的 `clip_depth` 回到 0（`Lane::clear` 的回归）；随后可再次 `push(Clip(Pop))` 而不触发 debug 断言 |

### 17.9 `app.rs`（**11**）

| # | 用例 | 断言要点 |
|---|---|---|
| 1 | `new_app_has_no_windows` | `windows().count() == 0`；`take_wake_request() == false`；`runtime_handle()` 返回的句柄可用（`Handle::spawn` 不 panic，任务立即完成） |
| 2 | `write_marks_only_readers` | 两个节点各读一个信号：写 A 的信号 → 只有读它的节点变脏（`dirty().contains(PAINT)`），另一个保持 `REUSABLE`/无 PAINT |
| 3 | `write_marks_layout_when_patch_affects_layout` | 节点上次求值的 patch `affects_layout = true` → 写信号后该节点含 `LAYOUT` 且祖先含 `LAYOUT` |
| 4 | `refresh_styles_evaluates_only_dirty` | 3 个节点（1 个脏）→ `refresh_styles(w) == Some(report)` 且 `report.evaluated == 1`；再调一次 `evaluated == 0`（幂等） |
| 5 | `refresh_styles_escalates_layout` | 闭包返回 `affects_layout = true` 的 patch → 节点含 `LAYOUT`、祖先含 `LAYOUT`、`PAINT` 已清除，且 `report.layout_dirty == true` |
| 6 | `refresh_styles_replaces_deps` | 闭包里 `if flag { read(a) } else { read(b) }`：先 true 后 false → 写 `a` 不再标脏，写 `b` 标脏（D4 的端到端验证） |
| 7 | `refresh_styles_unknown_window_is_none` | `refresh_styles(未创建的 WindowId) == None`（不 panic、不动任何状态） |
| 8 | `write_only_derived_does_not_wake` | 只有派生直接读某信号（无节点）：写该信号 → 派生 dirty 但 `take_wake_request() == false`，`report.wake_requested == false` |
| 9 | `write_marks_across_windows` | 两个窗口各有一个节点读同一信号 → 写一次 → **两个**窗口的节点都变脏（`Owner::Node` 带 window 的意义） |
| 10 | `close_window_purges_dependents` | 关窗后写该信号 → 无节点变脏（不 panic）、`take_wake_request() == false`；`window(id) == None` |
| 11 | `refresh_styles_marks_a11y_on_interactive_change` | patch 的 `interactive` 由 `Some(true)` 变 `Some(false)` → 节点含 `A11Y`、不含额外 `LAYOUT`；`report.paint_dirty == true`（lane 仍要重建） |

### 17.10 `window.rs`（**4**）

| # | 用例 | 断言要点 |
|---|---|---|
| 1 | `create_window_ids_are_unique_and_monotonic` | 两次建窗 → id 递增且不等；`window(id)` 都存在 |
| 2 | `close_window_returns_false_for_unknown` | 未知 id → `false`；关两次 → 第二次 `false` |
| 3 | `window_options_default` | `title` 为空、`size == 800×600`、`scale == Scale::ONE` |
| 4 | `set_scale_marks_root_layout` | `set_scale(1.5)` → `scale() == 1.5`、root 含 `LAYOUT`（`mark_layout(root)` 的可观测后果） |

**合计**：7 + 12 + 1 + 8 + 12 + 9 + 6 + 3 + 9 + 11 + 4 = **82 个**（`id` 7、`signal` 12、`signal/dependency` 的 panic 安全 1、`dependency` 8、`dirty` 12、`tree` 9、`builder` 6、`style` 3、`scene` 9、`app` 11、`window` 4）。

> **落地实测（2026-11-12，实现 + 代码审查修复后回填）**：实际交付 **97** 个 P03 用例（`id` 7、`signal` 13、
> `dependency` 9、`dirty` 14、`tree` 12、`builder` 6、`style` 3、`scene` 9（另加 P02 的 6 个 `Lane` 用例）、`app` 18、
> `window` 4、`error` 2）；`cargo test -p kytrix_gui` = **200 passed**，`--release` = **194**（6 个
> `#[cfg(debug_assertions)]` 用例不编译）。三处断言的改写原因见 `DEVIATIONS.md` **S-38**（另有 S-39 登记审查期新增/替换的用例）：
> ①`dirty` 的「叶子父」场景不可达（`build` 禁止给叶子挂子节点）→ 改为谓词级断言；
> ②`scene` 的 clip 上限断言已由 P02 的 `depth_cap_is_debug_assert` 覆盖 → P03 只断言转发与计数；
> ③`builder` 的「`refresh_styles` 后 patch 是闭包算出的值」属 `App` 级行为 → 下沉到 `app.rs`。

> **用例数裁决（→ S-31）**：brief 估算 40–60，本设计列出 **82**（裁决后新增 6 条：等值短路的风险固化、panic 安全、
> `refresh_styles` 未知窗口、`clear_frame` 的 clip 复位、叶子父的 debug 断言、N7 的升级冒泡），决定**全部交付**。理由：
> ①每条用例对应一条独立的不变量、终止条件或端到端断言（§6.2 A1–A5、§8.3 D1–D5、§9.3 的 8 行、§17.9 的跨窗口/唤醒/`A11Y`），
> 删任何一条都会留下一个「只能靠人读代码」的回归面；②P02 交付 60 个用例，而 P03 的语义面（状态 + 依赖 + 派生 + 脏传播 +
> Scene + 窗口）明显更宽；③平均每例 ~8 行，增量成本可接受。
> **优先级**：若排期紧张，按「每张表从上到下」的顺序交付到 52 个即覆盖全部**机制性**断言（表的尾部多为「幂等/边界」补充）。

---

## 18. 验收门禁

```powershell
# ── 1) 单元测试（debug + release） ──
cargo test  -p kytrix_gui
cargo test  -p kytrix_gui --release

# ── 2) clippy（workspace lint 由 [lints] workspace = true 继承） ──
cargo +nightly clippy -p kytrix_gui --all-targets -- -D warnings

# ── 3) 格式 ──
cargo +nightly fmt -p kytrix_gui -- --check

# ── 4) 文档零警告（P03 的公开面比 P02 大，doc 必须干净） ──
cargo doc -p kytrix_gui --no-deps
#   期望无 warning；若出现 broken_intra_doc_links，改注释而不是 allow

# ── 5) 三平台依赖解析（不链接） ──
cargo fetch
cargo metadata --format-version 1 --filter-platform x86_64-pc-windows-msvc   | Out-Null
cargo metadata --format-version 1 --filter-platform aarch64-apple-darwin     | Out-Null
cargo metadata --format-version 1 --filter-platform x86_64-unknown-linux-gnu | Out-Null

# ── 6) 零 unsafe / 零 allow / 零 expect（bitflags 宏内的 allow 不算，见 §2.3） ──
Get-ChildItem crates/kytrix_gui/src -Recurse -Filter *.rs |
  Select-String -Pattern '\bunsafe\b|#\[allow|#\[expect' |
  Where-Object { $_.Line -notmatch 'bitflags' }
#   期望无输出

# ── 7) tokio 纪律：不得出现 Handle::current / spawn_blocking（clippy.toml 已 deny） ──
Get-ChildItem crates/kytrix_gui/src -Recurse -Filter *.rs |
  Select-String -Pattern 'Handle::current|spawn_blocking' |
  Where-Object { $_.Line -notmatch '^\s*(//|///|//!)' }
#   期望无输出。**排除注释行**（B-N5）：文档与 rustdoc 里会「提到」这两个名字来解释为什么禁用，那不是调用。

# ── 8) dead_code 审计：三条 grep 必须逐项与 §4.3 表对照 ──
Get-ChildItem crates/kytrix_gui/src -Recurse -Filter *.rs |
  Select-String -Pattern 'pub\(crate\) (fn|struct|enum|trait|type|const)'
Get-ChildItem crates/kytrix_gui/src -Recurse -Filter *.rs |
  Select-String -Pattern 'pub\(crate\) \w+:'
Get-ChildItem crates/kytrix_gui/src -Recurse -Filter *.rs |
  Select-String -Pattern 'pub\(crate\) use'
#   期望：每条命中都能在 §4.3 的表里找到行；表外的项一律删除或提升为 pub。
#   第 2 条 grep（`pub(crate) <field>:`）在当前 workspace lint 下**应当无输出**（`field_scoped_visibility_modifiers = "forbid"`），
#   它是「防止有人绕开 lint」的守卫；第 3 条（`pub(crate) use`）在 `node/*` / `paint/scene.rs` 里**应当无输出**
#   （公共模块根不做扁平出口，M-07）；`render/`/`platform/` 尚未创建。

# ── 9) 文件边界（不得创建 P04+ 的文件） ──
Get-ChildItem crates/kytrix_gui/src -Recurse -Filter *.rs | Select-Object FullName
#   期望只有：lib.rs error.rs math.rs app.rs window.rs node.rs paint.rs
#             node/{id,signal,dependency,dirty,tree,builder}.rs
#             paint/{color,primitive,canvas,geometry,atlas,scene,style}.rs
Get-ChildItem crates/kytrix_gui/src -Recurse -Filter *.rs |
  Where-Object { $_.Name -in @('mod.rs','layout.rs','text.rs','event.rs','interaction.rs','a11y.rs',
                               'resources.rs','test_support.rs','render.rs','platform.rs',
                               'instance.rs','damage.rs') }
#   期望无输出

# ── 10) 模块根形态：只允许 mod/pub mod 行 ──
Get-Content crates\kytrix_gui\src\lib.rs        # 期望 6 行 pub mod（app/error/math/node/paint/window）
Get-Content crates\kytrix_gui\src\node.rs       # 期望 6 行 pub mod
Get-Content crates\kytrix_gui\src\paint.rs      # 期望 6 行 pub mod（scene 已转 pub）

# ── 11) P02 门禁 7c 的更新（P02 曾要求 Scene 写入面「无输出」） ──
#   **限定在 `impl Scene` 内**（B-N6）：`Lane::*` 与 `NodeTree` 有同名方法，全局 grep 会误命中。
$scene = Get-Content crates\kytrix_gui\src\paint\scene.rs
$start = ($scene | Select-String -Pattern '^impl Scene \{' | Select-Object -First 1).LineNumber
$end   = ($scene | Select-String -Pattern '^#\[cfg\(test\)\]' | Select-Object -First 1).LineNumber
$scene[($start - 1)..($end - 2)] | Select-String -Pattern 'pub fn (new_lane|release_lane|push|clear_frame|set_order)'
#   期望 5 行（P03 起写入面是 pub，见 §13.2 / S-33；P02 §15-7c 的旧期望作废）
#   doc 首行必须带 "engine-internal, 应用层不得调用"
Select-String -Path crates\kytrix_gui\src\paint\scene.rs -Pattern 'engine-internal'
#   期望 6 行（new_lane/release_lane/push/clear_frame/set_order/clear_order）
Select-String -Path crates\kytrix_gui\src\paint\scene.rs -Pattern 'derive\(([^)]*Default|Clone)'
#   期望无输出（裁决 ③：Scene 不 Clone / 不 Default）
Select-String -Path crates\kytrix_gui\src\paint\scene.rs -Pattern 'pub fn (new|default)\('
#   期望无输出（构造是 pub(crate) fn empty）

# ── 12) 依赖增量 ──
Select-String -Path crates\kytrix_gui\Cargo.toml -Pattern '^(ahash|smallvec|tokio)\s*='
#   期望 3 行，且 ahash 不在 [dev-dependencies] 里
cargo tree -p kytrix_gui -e normal --depth 1
#   期望新增的直接依赖只有 ahash / smallvec / tokio（其余与 P02 相同）
```

**「完成」判据**：1–12 全绿；`cargo test` 用例数 = **82**（§17）；`--release` 下 `scene.rs` #5 与 `builder.rs` #6
因 `#[cfg(debug_assertions)]` **不编译**（release 用例数 = 82 − 2 = **80**），其余断言一致（N2）。

---

## 19. Cargo 依赖增量

```toml
# crates/kytrix_gui/Cargo.toml —— [dependencies] 段（按字母序插入）
# P03 状态 / 节点 / 容器
ahash = { workspace = true }          # 依赖图与窗口表（§8.2、§5.1）
smallvec = { workspace = true }       # OwnerSet / Node.children/lanes/deps / ReadTracker / EvalStack
tokio = { workspace = true }          # 仅 tokio::runtime::Handle（§2.2）

# 并从 [dev-dependencies] 删除 ahash（P03 起它是 normal 依赖）
```

| 依赖 | 版本（workspace） | 用到的能力 | 为什么不用别的 |
|---|---|---|---|
| `ahash` | `0.8.12`（`features = ["serde"]`） | `ahash::HashMap<K, V>`（`std HashMap` + `RandomState`） | AGENTS §2.3 明确要求；`DependencyGraph.dependents` 与 `App.windows` 都是热路径查表 |
| `smallvec` | `1.16.5` | `SmallVec<[T; N]>` | 设计 §3.2 逐字要求（`children`/`lanes`/`deps`）；`OwnerSet`/`ReadTracker` 的「内联小集合」也依赖它 |
> **`GeometryId` 的可构造性（B-N7）**：`GeometryStore` 属 **P09**，因此 P03 的 `NodeBuilder::geometry(GeometryId)`
> 只能由 crate 内测试句柄（`GeometryId::for_test`，`#[cfg(test)]`）驱动；**应用层在 P09 之前拿不到 `GeometryId`**，
> 也就无法构造 `NodeKind::Geometry`。这是有意的：提前造一个「能构造但没人能兑现」的句柄只会制造假 API。

| `tokio` | `1.54.1`（workspace `features = ["full"]`） | 只用 `tokio::runtime::Handle`（`rt` 面） | D21/I13：`App` 必须持有单一 runtime 的句柄。**不新增 feature 收窄**：workspace 的 `full` 是并集，成员声明无法削减（§2.2） |
| `bitflags` | `2.13.2` | `bitflags!` 宏 | P02 已声明（当时无使用者）；`DirtyFlags` 是它的首个消费者 |
| `slab` | **不加** | — | `Arena` 已提供「槽位 + 自由表 + **代际**」，`slab::Slab` 无代际 → 还要自己包一层 |
| `indexmap` | **不加** | — | `Scene.order` 是「显式且每帧可重写的 lane 顺序」，`IndexMap` 的插入序语义与它不匹配 |
| `static_assertions` | **不加** | — | 尺寸/不变量断言用 `const _: () = assert!(…)` / `assert!`，写在叶子文件里（Rust 1.97 支持 const 断言） |

> **推迟交付表的位置说明**：本文件的推迟/接手表是 **§4.4**（P02 的文档里它是 §19；本文的 §19 是 Cargo）。
> `Application`/`AppHandle` 的「P08/P09 落地」一行在 §4.4 已显式写明（含 `App::handle() -> AppHandle` 的 additive 计划）。

---

## 20. 已裁决的 5 项（2026-11-12 上级裁决：**全部采纳**）

> 记录见 §23。下表保留提案期的「建议 / 备选」，裁决结果列已更新为最终形态。

| # | 裁决项 | 裁决结果 | 备选与代价（提案期记录） |
|---|---|---|---|
| **①** | `Application` / `AppHandle` 是否本阶段落地 | **采纳：推迟 P08/P09**；`App` 侧改名为 `App::runtime_handle(&self) -> &Handle`（`handle()` 留给 P09）（→ S-24、S-37） | 落地：`Application::run` 需要平台事件循环、`AppHandle` 需要 `PlatformDispatcher`；P03 无消费者 → 必然 `dead_code` 或被迫写空壳（违反 AGENTS §1.2/§4） |
| **②** | `versions: Vec<u64>` 是否保留 | **采纳：完全删除**（含每槽位 `version`；无读者就不留）；`App::write<T: Clone + PartialEq + 'static>`，等值短路（→ S-27、S-35） | 保留：push 失效模型下无人读取 → 非 test 构建零读者 → `dead_code` 失败，且违反「不得定义未使用字段」 |
| **③** | `Scene` 的写入面在 P03 是 `pub` 还是 `pub(crate)` | **采纳：`pub` + 约束**；`lane`/`lane_mut` 二选一取**方案②**（只给 `lane_primitives -> &[Primitive]`）；`Scene` 不 `Clone`/不 `Default`/无公开构造（→ S-33、F-38、F-39） | `pub(crate)`：P03 无 crate 内调用者 → `dead_code` 必失败；或把整个 `Scene` 推迟到 P06（则 brief 的「Scene lane 分配/释放/复用/绘制序」交付项与 §17.8 的用例一并落空） |
| **④** | `Owner::Node` 是否携带 `WindowId` | **采纳：携带**；`OwnerSet = SmallVec<[Owner; 2]>`；补 `Copy/Eq/Hash` 与去重/序规则（→ S-32） | 不带：多窗口下 `NodeId`（窗口内 arena 索引）跨窗口碰撞 → `App::write` 会误标另一个窗口的同号节点（stale 之外的第二类错误）。代价：`Owner` 16 B 而非 8 B |
| **⑤** | 派生环检测：注册期拒绝 vs 运行时上报 | **采纳：运行时**（`EvalStack` + `EvalGuard`（`Drop` 出栈）+ `EngineError::CyclicDerived`；`read_derived` 返回 `Result`；求值失败不污染缓存）（→ S-26） | 注册期 DFS：闭包依赖在运行前不可知，**不可行**；把「派生不得读派生」写死可从结构上消除环，但会丢掉设计 §3.9 的派生链（行情派生指标需要它） |

---

## 21. 风险

| # | 风险 | 等级 | 现状与对策 |
|---|---|---|---|
| R1 | `size_of::<Node>()` 过大（内联 `taffy::Style<SmolStr>`，估计 ~800 B）；5000 节点 ≈ 4 MB | 中 | §17.5-8 的上限断言（≤1024 B）先兜底；P04 落地时实测并回填 §11.1。超预期则 `style: Box<Style>`（不改公开面）→ V-30 |
| R2 | 冒泡终止过保守（`Auto` 一律视为「依赖子内容」）→ 深树的一次变化标到根 | 中 | 这是**正确侧**保守（多算不漏算）；§4.4 的精确终止（比较新旧 size）由 P04 的 `set_layout` 补上 → V-27。P03 的单测只断言结构判定，不断言性能 |
| R3 | `OwnerSet` 插入查重 O(k)；主题类信号在 mount 期聚合上千读者 → 一次性 O(k²) | 低 | 稳态写入不触碰集合（只 `replace_deps` 差分）；V-26：P18 若 >1 ms 改 `ahash::HashSet`（接口不变） |
| R4 | 节点卸载后 `dependents` 残留死 owner（惰性剔除） | 低 | 上界 = 历史 owner 数；关窗精确清理；写路径 `retain` 幂等。V-29 |
| R5 | `write` 的相等比较成本（`T` 大时 O(n)） | 低 | 写频率远低于渲染；V-25。若成热点，应用层拆细信号（§3.7 的粒度建议） |
| R6 | 「派生闭包不得读 `Transient`」无强制手段 | 低 | 文档 + V-24；P13 动画落地时若有消费者再加 `debug_assert` |
| R7 | 公开面首次泄漏第三方类型（`taffy::Style<SmolStr>`），`base_ui` 必须声明 taffy | 低 | F-35；taffy 由 workspace 精确钉 `=0.15.0`（P01 §5.3④） |
| R8 | `Scene` 写入面是 `pub`，应用层可调用 | 低 | 应用层拿不到 `&mut Scene`（`Window::scene()` 只读）、`Scene` 不 `Clone`/`Default` 且**无公开构造器**、`Window` 无 `set_scene` → 无可用写路径；写方法的 doc 首行标注 engine-internal（S-33、F-39） |
| R9 | **等值短路的语义风险**：`T` 内部可变而 `PartialEq` 只比外层 ⇒ 「内部变了但不重绘」（stale drawing） | 中 | 这是短路的必然代价（§7.5 已写进应用层契约：**必须用不可变值语义**）；用 §17.2-6 的反例测试固化；`base_ui` 的 code review 约定 + P08 的应用层迁移文档同步 |

---

## 22. 本阶段需追加到 `design/DEVIATIONS.md` 的条目（§八）

> 编号从 **F-34 / S-23 / V-23** 起（§七 的最后编号是 `F-33` / `S-22` / `V-16`；`V-22` 已被 `missing_docs` 债务占用）。
> 历史条目不得改写。完整条目见 `DEVIATIONS.md` §八。

| 编号 | 标题 | 一句话 |
|---|---|---|
| **F-34** | `NodeBuilder` 的公开路径改为 `node::builder::NodeBuilder` | P01 §4.5 写的是模块根；P03 的文件树把 `NodeBuilder` 放叶子文件，按「路径 = 定义文件」改 |
| **F-35** | 公开面首次包含第三方类型（`Style.inner: taffy::Style<SmolStr>`） | 不暴露就必须造布局属性 DSL（D24 禁止）；`base_ui` 需声明 `taffy`（workspace 精确钉版） |
| **F-36** | 不新增 `paint::style::Border`，复用 `paint::primitive::Border` | P01 §4.5 的 `paint::style::Border` 已被 P02 §4.2 修订；两个 `Border` 会变成「一条边框两种真相」 |
| **F-37** | `NodeBuilder::build` 返回 `Result<NodeId, EngineError>` | 陈旧父 id 是应用层可触发的真实错误；静默丢子树 = stale drawing。`StaleNode` 因此有真实生产者 |
| **F-38** | `Scene` 的公开签名按可见性重构（`lane → lane_primitives(&[Primitive])`、`lane_mut` → `push`） | `Lane` 是 `pub(crate)`；公开签名含它会触发 `private_interfaces`（**§2.4 的 probe 已实测 `-D warnings` 下编译失败**）；取裁决 ③ 的**方案②**（只暴露切片投影，公开面更小） |
| **F-39** | `Scene` 不实现 `Clone`/`Default`、无公开构造器（`pub(crate) fn empty()`） | 裁决 ③ 的约束；去掉 `Default` 后 `Scene::new` 会触发 clippy `new_without_default`（`#[allow]` 被禁）→ 改名 `empty` 并降为 `pub(crate)`；`Arena` 也不再需要 `Default` |
| **F-40** | `App::refresh_styles(window) -> Option<StylePassReport>` 与报告类型 | 裁决 6 的细化：报告含 `evaluated`/`layout_dirty`/`paint_dirty`/`wake_requested`；按窗口；`None` = 未知窗口（冻结表无对应错误变体） |
| **S-23** | P03 的落地判据 = 「§1.3 白名单内 **或** 有非 test 消费者」 | 与 P02 §3.2（S-08）同源；`Scene` 是唯一「白名单内但无 crate 内消费者」的例外（S-33） |
| **S-24** | `Application` / `AppHandle` 推迟 P08/P09 | P03 无平台层 → 无消费者；`App` 侧是 `runtime_handle(&self) -> &Handle`（`handle()` 留给 P09 的 `AppHandle`，纯新增） |
| **S-25** | 信号/派生值**不回收**（与 `App` 同寿），`SignalId` 无代际 | `Signal<T>` 是 `Copy` 句柄 → 引用计数不可实现；应用的状态集有界 |
| **S-26** | 派生环检测移到**运行时求值栈**，`read_derived` 返回 `Result`；`EvalGuard`（`Drop`）保证 panic 安全；求值失败不污染缓存 | 注册期 DFS 不可行（闭包依赖运行前未知）；环是应用层可触发的真实错误 |
| **S-27** | 删除 `versions`：**独立向量与每槽位 `version` 字段都删** | push 失效模型下版本比较无读者（裁决 ②：「没有读者就删掉，不为测试保留」）；等值短路由值比较完成 |
| **S-28** | `GEOMETRY`/`REUSABLE` 位在 P03 无读者（位已定义、有写者） | `REUSABLE` 由 `mark_*` 清除（常量被读）；读者在 P06/P08 |
| **S-29** | `Children`/`Slot`/`NodeHandle` 推迟 P08 | 属声明式建树的语法层，消费者是 `base_ui`；P03 的等价能力是 `build` + `append_child` |
| **S-30** | `StaleNode`/`CyclicDerived` 的实现 Phase 由 P04 改为 **P03** | 冻结表按实际落地修正；payload 与文案逐字不变 |
| **S-31** | P03 交付 **82** 个内联单测（brief 估算 40–60） | 每条对应一条独立不变量/终止条件；裁决后新增 6 条（等值短路风险、panic 安全、未知窗口、clip 复位、叶子父 debug 断言、N7 的升级冒泡） |
| **S-32** | `Owner::Node` 携带 `WindowId`；`OwnerSet = SmallVec<[Owner; 2]>`；`Owner` 派生 `Copy/Eq/Hash`（**不** `Ord`），去重 = 线性查重，序 = 首次登记序 | `NodeId` 是窗口内索引；位图/哈希集合在「index 复用 + 代际」下不安全；序的确定性来自 `order()` 遍历 |
| **S-33** | `Scene` 的写入面（`new_lane`/`release_lane`/`push`/`clear_frame`/`set_order`/`clear_order`）在 P03 是 `pub`，doc 首行标注 **engine-internal** | 裁决 ③：P03 无 crate 内调用者；`pub(crate)` 会 `dead_code`；`Scene` 无公开构造器 ⇒ 应用层无可用写路径 |
| **S-34** | `App::refresh_styles` 作为 P03 的样式求值 pass 落地 | 它是 `ReadCtx`/依赖图/`Derived`/`affects_layout`/`bind_style` 等 10 项能力的唯一非 test 消费者；**不产生 Scene 图元**；P06 的 paint 在重建 lane 前调用同一实现（F-40） |
| **S-35** | `App::write<T: Clone + PartialEq + 'static>` + **等值短路**的语义与风险 | `PartialEq` 只在写路径（`Signal<T>` 无约束）；短路用 `owned_ref`（不克隆）；风险 = `T` 内部可变而 `PartialEq` 只比外层 → 不重绘；应用层必须用不可变值语义（R9、§17.2-6） |
| **S-36** | `NodeBuilder::build` 的错误语义冻结 | `StaleNode` 是**唯一**的 `Err`（父不存在/代际不匹配）；叶子父节点（`Canvas`/`Geometry`）与结构环 = `debug_assert` + 明确定义的 release 行为（§12.2） |
| **S-37** | `App::runtime_handle(&self) -> &tokio::runtime::Handle`（字段 `runtime`） | 不占用 `handle()`：P09 加 `handle() -> AppHandle` 是纯新增、零破坏；返回借用避免每次 `clone` |
| **F-41** | `NodeId.generation` 用 `NonZeroU32` | 两个 `u32` 没有 niche ⇒ `Option<NodeId>` 实测 12 B；`NonZeroU32` 把「0 永不是合法代际」提升为类型即证明，`Option<NodeId>` 回到 8 B（公开面不变） |
| **F-42** | `ExpandedDimension::Calc` 是无条件分支（`#[cfg(feature = "calc")]` 不可表达） | crate 不能 `#[cfg]` 依赖的 feature；taffy 的 `calc` 由 workspace 开启 ⇒ 变体恒存在。**变体名是 `Calc` 不是 `FitContentCalc`** |
| **F-43** | `NodeTree::insert_child` 收「部件」而非半成品 `Node` | `Node` 字段是 `node::tree` 私有的，`NodeBuilder` 无法构造；避免引入「占位 id」构造器 |
| **F-44** | `eval_derived` 的 `take_compute == None` 分支不加 `debug_assert` | 该分支包含「上次求值 panic 未放回」的**可恢复路径**（N8）；断言会把恢复路径变成 debug 崩溃 |
| **S-38** | 测试落地实测：**97** 个 P03 用例（文档列 82），三处断言按可测性改写 | 见 `DEVIATIONS.md` S-38（含逐文件分布与 200/194 的 debug/release 差异） |
| **S-39** | 代码审查（A/B）期新增/替换的用例与三处语义变更 | D16/I3 端到端三例、幽灵边退订、子树 PAINT、`mark_a11y` 无 damage、7 个 `Node` 访问器等；逐条见 `DEVIATIONS.md` S-39 |

**待复核项（V 表，4 列）**

| 编号 | 标题 | 复核 Phase | 触发条件 / 处置 |
|---|---|---|---|
| **V-23** | `Style` 的 P06 字段能否安全 additive 加 | P06 | style→paint 落地时若 `Style` 已被应用层按结构体字面量构造（无法 additive），改为 `Style { inner, paint: PaintStyle }` |
| **V-24** | 派生闭包误读 `Transient` 导致永久 stale | P13 | 动画/滚动落地后若出现，加 `debug_assert` 或标记「读过 transient」 |
| **V-25** | `write` 的相等比较成本（`T` 大时 O(n)） | P18 | L4 基准显示写路径占比显著 → 拆细信号；确需强制失效时 additive 加 `write_always` |
| **V-26** | `OwnerSet` 的 mount 期 O(k²) 插入 | P18 | 5000 节点 mount >1 ms → 改 `ahash::HashSet`（接口不变） |
| **V-27** | 冒泡终止过保守（`Auto` 一律冒泡） | P04 | `set_layout` 比较新旧 `size`：相同则不再向上（§4.4）；P03 的表是它的上界 |
| **V-28** | `order_pos` 是 O(n) 线性扫描 | P13 | 虚拟列表每帧上千次结构变更 → 加 `Node.order_index`（additive） |
| **V-29** | 节点卸载后依赖图残留死 owner（惰性剔除） | P08 | 大量增删节点时若内存/写路径扫描变慢 → `App::remove_node(window, node)` 精确退订 |
| **V-30** | `size_of::<Node>()` 实测（内联 `taffy::Style<SmolStr>`） | P04 | 超 1024 B 或 5 万节点内存吃紧 → `style: Box<Style>`（不改公开面） |

---

## 附录 A · 实现者检查单（按执行顺序）

1. **清单**：`lib.rs` 加 3 行；`paint.rs` 的 `scene` 转 `pub mod`；`Cargo.toml` 加 `ahash`/`smallvec`/`tokio` 并从 dev-deps 删 `ahash`。
2. **`node/id.rs`**：`NodeId`（私有字段 + `index`/`generation`/`to_bits`/手写 `Debug`）+ `ArenaKey` + `next_generation` +
   `Arena`（`insert_with`/`remove`/`get`/`get_mut`/`values_mut`/`len`；`derive(Debug)`，**不实现 `Default`**）+ 7 个用例。
3. **`paint/style.rs`**：`Style`/`StylePatch`/`Cursor` + 3 个用例（`Border` 从 `primitive` 导入，**不改 P02 的类型**；字段核对见 §10.2）。
4. **`node/signal.rs`**：`SignalId`/`DerivedId`/三个句柄（手写 `Copy`）+ `StateStore`/`SignalSlot`（**无 version**）/`SignalValue`/`DerivedStore`/`DerivedSlot` + 12 个用例。
5. **`node/dependency.rs`**：`Owner`（`Copy/Eq/Hash`）/`OwnerSet`/`DependencyGraph`/`ReadTracker`/`EvalStack`/`EvalGuard`/`ReadCtx`
   （`tracker` 是 `&mut`；`peek` 带 `#[cfg(test)]`）+ 8 个用例 + **1 个 panic 安全用例**（`17.2b`）。
6. **`paint/scene.rs`**：追加 `LaneId`/`Scene`（只 `derive(Debug)`、`pub(crate) fn empty`）/`impl ArenaKey for LaneId`/`Lane::clear`
   + 9 个用例（P02 的 6 个 `Lane` 用例保持）。
7. **`node/tree.rs`**：`LayoutBox`/`NodeKind`/`Node`（手写 `Debug`，跳过 `bind_style`）+ `NodeTree`（`new` + `Default`/`root`/`get`/`order`/
   `append_child`/`remove`/`insert_child`/`mark_*`/`clear_paint`/`set_style_patch`/`set_deps`/`take_bind_style`/`put_bind_style`/
   `deps`/`any_node`）+ 9 个用例。
8. **`node/dirty.rs`**：`DirtyFlags` + `depends_on_children`/`affects_parent` + 11 个用例。
9. **`node/builder.rs`**：`NodeBuilder` + 5 个用例（`build` 的错误语义按 S-36）。
10. **`window.rs`**：`WindowId`/`WindowOptions`/`Window` + 4 个用例。
11. **`app.rs`**：`App`（`new`/`signal`/`derived`/`transient`/`write`（`Clone + PartialEq`）/`write_transient`/`take_wake_request`/
    `refresh_styles`（按窗口，返回 `Option<StylePassReport>`）/`create_window`/`window`/`window_mut`/`close_window`/
    `runtime_handle`/`windows`）+ `StylePassReport` + 11 个用例。
12. **`error.rs`**：追加 2 个变体；`all_variants()` 补齐（`source_is_none` 用例必须覆盖）。
13. **`node.rs`**：6 行 `pub mod`。
14. 每加一个 `pub(crate)` item，立刻对照 §4.3 确认消费者；没有消费者就删（尤其别为测试保留字段，S-27）。
15. 跑 §18 的 12 组门禁；`cargo doc` 零警告。

### 附录 B · 与上游文档的交叉引用索引

| 本文 | 上游 | 偏差条目 |
|---|---|---|
| §5 所有权 / §14 写路径 | 设计 §1.4、§3.2、§3.5、§3.6、§9.2 | S-24、S-32、S-34 |
| §6 Arena/NodeId | 设计 §3.2、§3.9 | — |
| §7 状态模型 | 设计 §3.2、§3.3、§3.7、§3.9；P01 §6.2.2 #7/#8 | S-25、S-26、S-27、S-30 |
| §8 依赖追踪 | 设计 §3.3、§3.8 | S-32 |
| §9 脏标记与传播 | 设计 §3.4、§4.4、§4.9.4 | S-28、V-27 |
| §10 样式 | 设计 §4.9、D24/I17；P02 §4.2 | F-35、F-36 |
| §11 节点树 | 设计 §3.2、§4.1（I5） | V-30 |
| §12 NodeBuilder | 设计 §3.10、附录 A | F-37、S-29 |
| §13 Scene | 设计 §5.1、§5.3；P02 §7.2/§13.2/§19 | F-38、F-39、S-33 |
| §16 错误面 | P01 §6.2.2 | S-30 |
| §17 测试 | 设计 §6.2（L1 探针）、§4.9.4 | S-31、V-24…V-29 |




---

## 23. 裁决记录（2026-11-12 上级裁决）

> **编号说明**：上级指示「在文档末尾加 §22 裁决记录」；本文的 §22 已被「需追加到 `DEVIATIONS.md` 的条目」占用
> （且 §4.4/§20/§22 的交叉引用已固化），故裁决记录作为 **§23** 追加在文末 —— 内容即裁决记录本身，不改动其它编号。
> 上一轮提出的 5 项裁决**全部采纳**，另有 9 条细化；本节的每一条都已落到正文的指定位置（可逐条复核）。

### 23.1 五项裁决（全部采纳）

| # | 裁决 | 落地位置 | 与提案的差异 |
|---|---|---|---|
| ① | `Application`/`AppHandle` 推迟 P08/P09；`App` 侧命名 **`runtime_handle(&self) -> &tokio::runtime::Handle`**（不占用 `handle()`） | §5.1（`App` 字段 `runtime` + 方法）、§5.1 决策表、§5.4、§4.4（推迟表新增 P09 的 `handle()` 计划）、§17.9-1、S-24、S-37 | 提案是 `handle() -> Handle`（按值）→ 改为**借用**并改名；字段 `handle` → `runtime` |
| ② | 删除 `versions: Vec<u64>`；`App::write<T: Clone + PartialEq + 'static>`；等值短路 + 语义/风险说明；`version` 字段只在不产生 dead_code 时保留 | §5.1（`write` bound）、§7.3（`SignalSlot` 只剩 `value`）、§7.5（整节重写：短路语义 + 风险 + 应用层契约）、§7.7、§14.2、§17.2-4/5/6、§21-R9、S-27、S-35 | **比提案更严**：裁决要求「没有读者就删」，核实后 P03 内 `version` **无任何非 test 读者** → **连每槽位字段一并删除**（不是只删向量） |
| ③ | `Scene` 写入面 P03 公开 + 约束（`Window::scene()` 只读；不 `Clone`/`Default`；写方法 doc 首行 engine-internal）；`Lane` 二选一取**方案②** | §13.1（`Scene` 只 `derive(Debug)`）、§13.2（签名 + `engine-internal` 标注 + 差异表）、§13.3、§5.2（`Scene` 可构造性行）、§18-11（门禁新增 3 条 grep）、S-33、F-38、F-39 | 提案的 `Scene::new()` + `Default` 被裁决否掉 → 改为 `pub(crate) fn empty()`（同时规避 clippy `new_without_default`，因为 `#[allow]` 被禁）；`lane` → `lane_primitives`、`lane_mut` → `push` |
| ④ | `Owner::Node` 携带 `WindowId`；`OwnerSet = SmallVec<[Owner; 2]>`；补 `Copy/Eq/Hash` 与排序/去重规则 | §8.1（derive + **「相等/哈希/去重/序」冻结表**：不实现 `Ord`；去重 = 线性查重；序 = 首次登记序；集合语义与输入顺序无关）、§14.2、S-32 | 提案缺「序」的明文 → 新增冻结表；明确**不**实现 `Ord`（无排序消费者） |
| ⑤ | 环检测运行时求值栈 + `CyclicDerived`；`read_derived` 完整签名、入栈/出栈的 panic 安全（`Drop` guard）、求值失败不污染缓存 | §7.4（完整签名 + 契约 + 逐字伪代码 + `EvalGuard` 的 Drop 语义 + 「失败不污染」的顺序保证）、§8.5（`EvalStack` + `EvalGuard` + `Drop` 实现）、§17.2b（panic 安全用例）、S-26 | 提案只说「求值栈」→ 明确 **`EvalGuard`（拥有 `&mut EvalStack`，`Drop` 里 `pop`）**，并把 `ReadCtx.tracker` 改为 `&'a mut ReadTracker`（否则嵌套求值的借用不成立） |

### 23.2 九条细化（逐条落地）

| # | 细化要求 | 落地位置 | 结论 |
|---|---|---|---|
| 6 | `App::refresh_styles` 的签名/返回值（建议 `-> StylePassReport`，含 `evaluated`/`layout_dirty`/`paint_dirty`/`wake_requested`）；必须是 `ReadCtx`/依赖图/`Derived`/`affects_layout` 的非 test 消费者；写明**不产生 Scene 图元**；P06 复用同一实现 | §5.1（`refresh_styles(&mut self, window: WindowId) -> Option<StylePassReport>` + 报告类型定义与字段语义）、§14.4（逐字 + 决策表新增「不产生 Scene 图元」「为什么按窗口」「与 P06 的关系」）、§17.9-4/5/7/8/11、§4.3（`StylePassReport` 行）、§4.1/§4.2（路径 + 白名单闭包）、S-34、F-40 | **一处细化偏离建议**：返回 `Option<StylePassReport>`（`None` = 该 `WindowId` 不存在）。理由：P03 没有「未知窗口」的错误变体（冻结表），`Option` 比「返回全零报告 + 静默吞掉 bug」诚实。其余完全按建议（字段名、语义、按窗口、不产生图元、P06 复用） |
| 7 | 核对 P02 `Border` 字段是否满足 `StylePatch`（width/color/radius）；不满足则定义 `StyleBorder` 并记偏差，**不要偷偷改 P02** | §10.2 新增「`Border` 的字段核对」表 | **满足**：P02 的 `Border { width: f32, color: Color }` 覆盖 v1 的全部需要；半径属 P06 的 `Style.corner_radius`，逐边边框 v1 不做。**不新增 `StyleBorder`**，也**不改** P02 的类型（F-36 保持） |
| 8 | `build` 除 `StaleNode` 之外的失败路径（父不是容器/`NodeKind` 约束），冻结错误语义 | §12.1 新增「错误语义冻结」表（5 种情形）+ `# Panics` 小节；§12.2 伪代码新增 `debug_assert!(!p.kind().is_leaf())`；§11.2 定义 `NodeKind::is_leaf`；§17.6-6 用例；S-36 | **`StaleNode` 是唯一 `Err`**：叶子父 = `debug_assert` + release 放行（子树不参与布局）；结构环 = `debug_assert` + 不改结构（只可能来自 `append_child`）；`Style` 非法与 OOM 不产生 `Err`（各归其位）。**不新增错误变体**（冻结表） |
| 9 | 公开面泄漏 `taffy::Style<SmolStr>`：写白名单闭包/偏差 + 说明为什么不本阶段包一层 | §4.2（白名单闭包新增 **`taffy::Style<SmolStr>`** 行）、§10.1（决策表：暴露 vs 包装；「为什么不用 `pub type`」）、F-35、V-23 | 登记为白名单闭包扩充 + F-35；`base_ui` 必须声明 `taffy = { workspace = true }`。**不在本阶段包一层**：①40+ 布局属性的访问器就是 D24 禁止的 DSL；②P05/P06 的布局/文本管线直接消费 taffy 类型（包一层会引入两套同义类型 + 每字段转换）；③taffy 由 workspace 精确钉 `=0.15.0`，版本不会漂移 |

### 23.3 本轮修订点清单（供审查对照）

| # | 文件/章节 | 修订 |
|---|---|---|
| 1 | §0-2 | 改为「5 项裁决已全部采纳」并同步新 API（`runtime_handle`、`StylePassReport`、`lane_primitives`、`EvalGuard`） |
| 2 | §4.2 / §4.3 / §4.4 | 白名单闭包加 `StylePassReport` + `taffy::Style<SmolStr>`；审计表加 `Scene::empty`/`EvalGuard`/`NodeKind::is_leaf`/`any_node`/`StylePassReport`；推迟表加 P09 `handle()` 计划 |
| 3 | §5.1 | `App` 字段 `handle`→`runtime`；`write` bound `Clone+PartialEq`；`refresh_styles` 按窗口返回 `Option<StylePassReport>`；`handle()`→`runtime_handle(&self) -> &Handle`；新增 `StylePassReport` 定义；决策表重写 |
| 4 | §5.2 / §5.3 | `Scene` 可构造性行；`NodeTree` 补 `impl Default`（clippy `new_without_default`） |
| 5 | §6.2 | `Arena::new` 的消费者改为 `Scene::empty`；`Arena` **不实现 `Default`** |
| 6 | §7.2 / §7.3 | 新增「句柄上不提供 `Signal::set`」行（若加则同 bound）；`SignalSlot` 删除 `version`；新增 `StateStore::with_capacity`/`split` 与 `DerivedStore`（拆分借用） |
| 7 | §7.4 / §7.5 | 求值模型重写（`EvalGuard`、嵌套 tracker、失败不污染）；`read_derived` 完整签名 + `# Errors`/`# Panics`；§7.5 重写为「dirty 模型 + 等值短路的语义与风险」 |
| 8 | §7.6 / §7.7 | 内存上界（`SignalSlot` 24 B）；transient 写不再提 `version` |
| 9 | §8.1 / §8.4 / §8.5 | `Owner` derive + 「相等/哈希/去重/序」冻结表；`ReadCtx.tracker` 改 `&mut`、删 `into_tracker`、加 `owner()`；`EvalStack` 加 `clear()`，新增 `EvalGuard`（`Drop`） |
| 10 | §10.2 | 新增 `Border` 字段核对表 |
| 11 | §11.2 | 新增 `NodeKind::is_leaf` |
| 12 | §12.1 / §12.2 | 错误语义冻结表 + `# Panics`；伪代码加 `is_leaf` 断言 |
| 13 | §13.1 / §13.2 | `Scene` 只 `derive(Debug)`、`pub(crate) fn empty`、写方法 doc 首行 `engine-internal`、`lane`→`lane_primitives`、差异表重写 |
| 14 | §14.2 / §14.4 | 写路径不再 bump version；`refresh_styles` 逐字重写（按窗口、报告、`any_node`、`eval_stack.clear()`） |
| 15 | §17 | 用例 76→**82**（新增：等值短路风险、panic 安全、未知窗口、`clear_frame` clip 复位、叶子父 debug 断言、N7 升级冒泡）；`write`/`refresh_styles` 相关断言按新签名改 |
| 16 | §18 | 门禁 11 新增三条 grep（`engine-internal` 标注、无 `derive(…Default/Clone…)`、无 `pub fn new/default`）；完成判据 76→**82** |
| 17 | §19 | 加「推迟表在 §4.4（P02 的编号是 §19）」的说明 |
| 18 | §20 / §21 / §22 | §20 改为「已裁决」表；§21 新增 R9（等值短路语义风险）、更新 R5/R8；§22 更新 F-38、S-24、S-26、S-27、S-31…S-34，新增 F-39/F-40、S-35/S-36/S-37 |
| 19 | 附录 A | 实现顺序按新 API 更新（`Arena` 无 `Default`、`EvalGuard`、`lane_primitives`、`runtime_handle`、82 个用例） |

### 23.4 仍存疑项（2 项，均不阻塞实现）

| # | 存疑项 | 现状 | 谁/何时定 |
|---|---|---|---|
| Q1 | `refresh_styles` 建议返回 `StylePassReport`，本轮改为 `Option<StylePassReport>`（`None` = 未知窗口，见 23.2 #6） | 已在 §5.1/§14.4 定死并给出理由 | 若上级要求严格按建议（返回裸 `StylePassReport`），则需为「未知窗口」选定一种表达（全零报告 / 新增错误变体）；**默认按本文件执行** |
| Q2 | `StylePatch` 的 `border` 只需 width+color（v1 单一边框）。若 base_ui 的 hover/focus 态需要「逐边/独立圆角」的边框，P03 的数据契约不够 | §10.2 已冻结 v1 范围；P06 才消费 | P06 落地时若出现真实需求 → additive 加 `paint::style::StyleBorder`（新类型，不动 P02 的 `Border`）+ 记偏差 |
