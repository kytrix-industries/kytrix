# ui_core 逻辑像素 / 物理像素边界重构设计

状态：设计稿（本轮不修改代码）
范围：`ui_core`、`ui_kit` 以及依赖这两个 crate 的应用层 API
决策记录：2026-11-23

## 0. 结论

问题确实存在，而且根因不只在 `ui_kit`。

当前 `ui_core` 只有“逻辑像素的隐式约定”和“物理像素的局部类型”，没有把两者作为不可混用的单位系统：

1. `math::Point/Size/Rect` 是裸 `f32`，名字里没有逻辑像素；`PhysicalRect` 是独立的 `i32` 四边类型，但 `PhysicalBounds`、`[f32; 4]`、`[i32; 4]` 又各自承担物理坐标。单位语义靠命名、注释和调用者记忆维持。
2. 布局阶段已经在做设备像素吸附：`NodeTree::device_scale` + `place_subtree` 把每个节点的绝对逻辑盒吸附到设备像素网格，导致公开的 `Node::layout().bounds` 本身随 DPI 变化。这与设计文档 I11“只在 paint / present 边界做一次换算”相反。
3. 绘制阶段没有统一换算策略：Quad 使用不取整的 `to_physical_f32`，Image/Glyph 使用逐边取整的 `to_physical`，Damage 使用外扩的 `to_physical_covering`，Clip 又先 `to_physical_f32` 再单独 `snap_rect_clip_to_pixels`。同一个逻辑矩形在不同 primitive 上会得到不同的物理矩形。
4. `ui_kit` 为了补偿上述行为，直接读取 `Window::scale()` 并在组件内做设备像素运算。`Toggle` 是最典型的例子：`device_length` / `device_length_with_parity` / `sync_knob` / `sync_ring` / `sync_mark` 在物理像素空间计算，再除以 scale 写回逻辑 `LayoutOverride`；`Brand` / `IconSpec` / `Interactive::mount_dynamic_icon` / `Menu::IconCache` 也都直接读取窗口 scale 选择 SVG 栅格。
5. 更多组件虽然不显式乘 scale，但会读取 `layout().bounds`（TabBar、ScrollBar、Input、ColorPicker、Overlay、Dialog、List、Markdown 等）。由于 bounds 已经被布局阶段吸附，这些组件的几何逻辑同样隐式依赖 DPI，只是问题更隐蔽。

因此，用户提出的方向——`ui_kit` 只使用逻辑像素，`ui_core` 只在绘制边界统一换算为物理像素——是成立的，也是 WebKit/Blink、Qt、GPUI、egui、Flutter 等成熟框架的通用做法。

本设计采用以下已确认决策：

- 单位类型采用 `LogicalPx`（逻辑像素，`f32`）/ `PhysicalPx`（连续物理像素，`f32`）/ `DevicePx`（整数设备像素，`i32`）。
- `Node::layout().bounds` 严格为逻辑像素，与 DPI 无关；删除布局阶段的设备吸附。
- 所有逻辑到物理的换算集中到 paint 编码边界；轴对齐矩形与边框在 paint 时按 `pixelSnappedIntRect` 规则吸附。
- `Style.inner` 私有；`Style` 提供 typed setter 与 `inner()` 只读访问器（仅 `test-support` 下公开，生产 `pub(crate)`）；`StyleBuilder` / `PaintBuilder` 移入 `ui_core::paint::style_builder`，所有长度参数在 C1 后使用 `LogicalPx`。
- Canvas 文本改为 `Canvas::push_text(TextContent, origin)` 的引擎配方模式；删除 `App::text_scale`、`App::shape_text`、`ShapedTextHandle` 等应用层 DPI 接口。
- 不保留旧类型别名、兼容层或过渡实现；全仓一次性迁移。

### 0.1 落地决策（2026-11-23 拍板）

| 项 | 决策 |
|---|---|
| 分支 | 直接在 `main` 上连续提交 |
| 提交可编译性 | 允许阶段内部中间提交暂时不编译；每个阶段收口必须 `cargo check --workspace --all-targets` + 相关测试全绿 |
| 落地顺序 | **行为先行、类型后行**：先用 `f32` 完成“布局去吸附 + paint 吸附 + damage/shadow pad 物理化”，golden 只重基一次；随后类型迁移必须是零像素 diff 的机械改造 |
| Golden | 维持 42 张三端基线，不扩全矩阵；行为阶段期间允许红，行为阶段收口后单批三端重基并逐图说明 |
| 命名 | 四边整数矩形主名 `DeviceRect`（`PhysicalRect` 让给连续物理矩形，即 `Rect<PhysicalPx>`） |
| Canvas 文本 | 纳入既有 text pass，不给 `PaintPass` 加文本服务（见 §4.5） |
| Damage AA pad | `aa_padding: PhysicalPx` 独立参数 |
| a11y 边界 | `to_covering_device_rect` |
| `ShapedLine` 等 | 全部降 `pub(crate)` |
| `Menu::IconCache` | 直接删除组件级缓存，依赖 `ResourceCache` 去重 |
| Canvas 文本断言 | frame/quad 记录断言，不新增 golden |
| `ResourceImage` | 字段私有，只暴露 `id()` / `kind()` / `intrinsic(dpr)` / `variant_for(cache, target)`；资源只描述“源 + 内在尺寸” |
| 资源栅格目标 | `ImageFit` 是绘制属性；引擎在绘制期为每个 (资源, 目标物理尺寸) 选择/生成变体 |
| `App::resources_mut` | 一并收口，应用只能走逻辑资源 API |
| 异步资源 owner | 本轮一并修“异步插入不登记 owner 导致关窗不回收” |
| Dicebear 头像 | 走引擎 SVG 资源，删除 `profile_ui` 的 `image`/`resvg`/`usvg` 依赖 |
| examples 物理观测 | 加 `required-features`，物理观测放 test-support 面 |
| 小数 DPI 对称性 | 接受最多 1 物理像素偏差 |
| 验收口径 | 三端 headless + golden + Windows 真窗口人工验收；`acceptance -- cpu` 文本帧阈值记为既有缺陷排除 |
| 提交格式 | `type(scope): 中文摘要` + 中文正文（问题/改动/验证），不加 AI 署名 |

## 1. 现状核查

### 1.1 单位类型现状

| 位置 | 当前实现 | 问题 |
|---|---|---|
| `crates/ui_core/src/math.rs:6-101` | `Point { x: f32, y: f32 }`、`Size { width: f32, height: f32 }`、`Rect { origin: Point, size: Size }` | 名字没有逻辑像素，任何 `f32` 物理值都能传进来 |
| `crates/ui_core/src/math.rs:507-581` | `PhysicalRect { x0: i32, y0: i32, x1: i32, y1: i32 }` | 这是唯一带“物理”语义的矩形；但 `PhysicalBounds` 又在 `paint/instance.rs:248-254` 重复定义 |
| `crates/ui_core/src/paint/instance.rs:53-62` | `GpuPrimitive.bounds: [f32; 4]` | 物理像素，但类型只是裸数组 |
| `crates/ui_core/src/paint/instance.rs:198-207` | `FrameOp::bounds: PhysicalBounds` | 与 `PhysicalRect` 语义重复 |
| `crates/ui_core/src/paint/primitive.rs:25-112` | `Quad.bounds: Rect`、`Image.bounds: Rect`、`Shadow.rect: Rect`、`ClipRect.rect: Rect`、`PositionedGlyph.position: Point` | 全部是裸逻辑 `f32`，没有编译器约束 |
| `crates/ui_core/src/text.rs:143-157` | `TextStyle.size: f32`、`letter_spacing: f32` | 文本字号/字距是逻辑像素，但没有单位类型 |
| `crates/ui_core/src/text.rs:748-757` | `ShapedLine.width/height: f32`、`scale: f32` | 逻辑尺寸与光栅 scale 混在同一个公开结构里 |

结论：`ui_core` 目前最多只能算“有两个命名类型 `Rect` 和 `PhysicalRect`”，并没有明确的 `LogicalPx` / `PhysicalPx` 类型边界。

### 1.2 布局阶段已经提前物理化

`NodeTree` 保存 `device_scale: Option<f32>`（`crates/ui_core/src/node/tree.rs:313`），并且：

- `Window::set_scale` 调用 `tree.set_device_scale(...)`，随后 `invalidate_layout(root)`（`crates/ui_core/src/window/scale.rs:8-21`）。
- `place_subtree` 对每个节点计算 `abs` 后，再执行 `abs.snap_to_device(scale)`，把 `committed` 写入 `target.layout.bounds`（`crates/ui_core/src/node/tree/layout.rs:255-296`）。
- `LayoutOverride` 子节点在最新提交中被特殊处理为相对父节点的 `committed.origin` 定位，而不是相对 `abs.origin`（`crates/ui_core/src/node/tree/layout.rs:307-315`，提交 `9cd7a1f8`）。

现有测试直接把这个行为钉死：

- `crates/ui_core/src/window/tests.rs:347-396`：`dpi_scale_only_shifts_logical_boxes_by_device_snapping`
- `crates/ui_core/src/node/tree/tests.rs:837-865`：`device_scale_snaps_committed_bounds_not_taffy_layout`
- `crates/ui_core/src/node/tree/tests.rs:904-932`：`a_layout_override_is_positioned_against_the_parents_snapped_box`

这意味着 `Node::layout().bounds` 不是纯逻辑布局结果，而是“逻辑值但已对齐设备像素”的提交盒。所有读取它的应用代码（包括 `ui_kit` 的 Toggle、TabBar、ScrollBar、Input、ColorPicker 等）都隐式依赖当前 DPI。

### 1.3 绘制阶段的换算方法不一致

`Rect` 当前有四个公开的物理化方法，语义不同（`crates/ui_core/src/math.rs:238-303`）：

| 方法 | 取整规则 | 当前使用点 |
|---|---|---|
| `to_physical_f32` | 不取整，直接 × factor | Quad、Shadow、Clip |
| `to_physical` | 每条边取整 | Image、Glyph |
| `to_physical_rounded` | origin 与 size 分别取整 | 少量平台尺寸计算 |
| `to_physical_covering` | floor 左上、ceil 右下 | Damage、Clip covering |

具体调用点：

- Quad：`crates/ui_core/src/paint/instance.rs:1905-1953`，`quad.bounds.to_physical_f32(self.scale)`
- Shadow：`crates/ui_core/src/paint/instance.rs:1955-1980`，不取整
- Image：`crates/ui_core/src/paint/instance.rs:1982-2011`，`image.bounds.to_physical(self.scale)`
- Glyph：`crates/ui_core/src/paint/instance.rs:2013-2035`，`Rect::from_origin_size(glyph.position, glyph.size).to_physical(self.scale)`
- Clip：`crates/ui_core/src/paint/instance.rs:1683-1718`，先 `to_physical_f32`，矩形 clip 再 `snap_rect_clip_to_pixels`（`crates/ui_core/src/paint/instance.rs:769-783`）
- Damage：`crates/ui_core/src/render/damage.rs:36-53`，`bounds.to_physical_covering(self.scale)`

同一个逻辑矩形在 Quad 与 Image 上可能相差 0.25 到 1 个物理像素。例如逻辑 `x = 10.3`、`width = 10.0`、DPI = 1.25：

- Quad：`12.875 .. 25.375`（不取整）
- Image：`13 .. 25`（逐边取整）
- Damage：`12 .. 26`（外扩覆盖）

这会产生背景与图片错位、边框边缘发虚、同一容器内的图片与背景 1px 缝等问题。`paint/pass.rs:1174-1180` 还在逻辑空间里反向计算 shadow pad（`(物理 pad × scale + 1) / scale`），而 `render/damage.rs:36-48` 的 `aa_padding` 又是物理像素，单位在函数签名里是混用的。

### 1.4 ui_kit 的 DPI 渗透

| 组件 / 文件 | 当前行为 | 具体位置 |
|---|---|---|
| `Toggle` | 读取窗口 scale，用 `device_length` / `device_length_with_parity` 在物理空间算 track/knob/ring/mark，再除回逻辑写 `LayoutOverride` | `crates/ui_kit/src/kit/toggle.rs:341-485`、`576-583`、`735` |
| `Brand` | 读取窗口 scale，`frame * scale` 后调用 `insert_owned_at_size` 生成物理位图 | `crates/ui_kit/src/kit/brand.rs:142-209` |
| `IconSpec` | 读取窗口 scale，传给 `insert_owned_svg_for_display` | `crates/ui_kit/src/kit/icon.rs:328-408` |
| `Interactive::mount_dynamic_icon` | 读取窗口 scale 传资源接口 | `crates/ui_kit/src/kit/interactive.rs:749-759` |
| `Menu::IconCache` | 缓存键包含 `scale_bits`，每次 resolve 读取窗口 scale | `crates/ui_kit/src/kit/menu.rs:48-93` |
| `TabBar` | `indicator_target` 读取 `row/tab.layout().bounds`，这些都是 DPI 吸附后的值 | `crates/ui_kit/src/kit/tab_bar.rs:593-607` |
| `ScrollBar` | thumb rect 基于 `track.layout().bounds` 计算 | `crates/ui_kit/src/kit/scroll_bar.rs:305-332`、`390-437` |
| `Input` | caret/selection canvas 基于 `text.layout().bounds` 与 `window.text_caret_rect` 计算 | `crates/ui_kit/src/kit/input.rs:1676-1728`、`1761-1770` |
| `ColorPicker` | palette/hit/geometry 都基于 `layout().bounds` | `crates/ui_kit/src/kit/color_picker.rs:1108-1198`、`2170` |
| `Overlay` / `Dialog` / `Dropdown` / `Select` / `List` / `Markdown` | 读取 layout bounds 作为定位或尺寸输入 | `overlay.rs:372,813`、`dialog.rs:704`、`dropdown.rs:788`、`select.rs:1129`、`list.rs:909`、`markdown/view.rs:177` |

Toggle 的实现尤其说明问题：一个开关组件为了在 1x/1.25x/1.5x/2x 下对齐物理像素，增加了约 200 行设备空间计算与专门的 DPI 测试。这是把 `ui_core` 的职责错误地下沉到了 `ui_kit`。

### 1.5 直接后果

1. **应用层无法获得稳定的逻辑几何**：`Node::layout().bounds` 随 DPI 变化，任何基于 layout 的自绘、命中、缓存键、位置计算都会随 DPI 改变。
2. **同一逻辑矩形得到不同物理矩形**：Quad/Image/Glyph/Clip/Damage 各自取整，1px 线条和图片边缘在不同组件上表现不一致。
3. **Canvas 内的细线没有布局吸附兜底**：布局吸附只作用于节点盒；Canvas 内的 caret、underline、separator 等 quad 仍然按 `to_physical_f32` 绘制，在 1.25/1.5 等 DPI 下会出现 1.25px/1.5px 的模糊线。
4. **DPI 变化强制重排**：`Window::set_scale` 调用 `invalidate_layout(root)`，而逻辑布局输入并没有变化；这是不必要的全树布局成本，也会让布局结果随 DPI 改变。
5. **文本与布局盒可能不一致**：`text_content_width` 来自 taffy 的未吸附尺寸，`LayoutBox.bounds` 却是吸附后的尺寸；文本 caret / 选区由未吸附宽度计算，再叠加吸附后的 origin，可能产生亚像素到 1px 的偏差。
6. **测试把错误行为固化**：`ui_kit::toggle` 的测试直接断言物理像素方形/居中；这些断言实际上是在测试 ui_kit 自己补偿出来的结果，而不是引擎的绘制契约。

### 1.6 与设计文档的矛盾

- `crates/ui_core/design/custom-gui-engine-design.md:199`（I11）写明“所有几何以逻辑像素存储、以物理像素绘制：只在 paint / present 边界做一次缩放与舍入”。
- 同文件 `:1171-1172`（§4.4）写明“布局与 `LayoutBox` 全用逻辑像素 `f32`；在 paint 生成 `GpuPrimitive.bounds` 时统一乘 scale 并 round”。
- 但实现中 `NodeTree::device_scale` + `snap_to_device` 已在布局阶段吸附；`Window::set_scale` 还会让根 LAYOUT 失效（`:2377-2378`），与“布局不依赖 DPI”矛盾。
- `crates/ui_core/design/custom-gui-engine-design.md:2495-2497` 又把 `Window::scale()` 作为应用可见 API，并要求 canvas 文本按 `App::text_scale` 重新 shape，进一步把 DPI 暴露给应用层。
- `.docs/gui-migration/assess/01-ui-core-api.md:533-537` 也写了“布局全用逻辑像素，物理化只在 paint”，但该文档没有覆盖后来加入的布局吸附实现。

## 2. 主流框架调研

| 框架 | 单位模型 | 与本设计的对应关系 |
|---|---|---|
| WebKit / Blink | `LayoutUnit` 用 1/64 逻辑像素做布局和 hit test；paint 时用 `pixelSnappedIntRect` 对齐设备像素，边框保持整数像素；矩形尺寸不单独 round，而是“位置与尺寸一起 snap” | 直接采用 `pixelSnappedIntRect` 算法；布局保持 `LogicalPx`，paint 输出 `DevicePx` |
| Chromium / Blink 坐标空间 | 明确区分 Physical Pixels、Device Independent Pixels (DIP)、CSS Pixels；`window.devicePixelRatio` 是 CSS px 与物理 px 的比值 | 应用层只看到 `LogicalPx`，`DevicePixelRatio` 只在引擎内部 |
| Qt | 应用和控件几何使用 device-independent pixels；QPainter 自动按 device pixel ratio 绘制；QImage/QPixmap 是原始物理像素并带 DPR 元数据 | `ui_kit` 不读 DPR；图片资源接口按逻辑尺寸注册，引擎选择物理变体 |
| GPUI | `Pixels`（逻辑）/ `ScaledPixels`（物理连续）/ `DevicePixels`（物理整数）三种类型；`Bounds<Pixels>::scale` → `Bounds<ScaledPixels>` → `Bounds<DevicePixels>` | 采用同样的三层单位模型和转换方向 |
| egui | 形状和布局使用 points；`pixels_per_point` 只在 tessellation / feathering 时使用；feathering 以物理像素定义再换算到 points | 边框、AA pad 等物理常量只在 paint 内部换算，不进入组件 |
| Flutter | `ViewConfiguration` 同时提供 physical constraints 与 logical constraints；`toMatrix()` 应用 `devicePixelRatio`；`RenderParagraph.devicePixelRatio` 变化时重新生成文字位图 | 文本按窗口 DPR 重新栅格化；布局和 Canvas 逻辑坐标不变 |

调研结论：用户的直觉是正确的，这不是某个框架的偏好，而是高 DPI 桌面/移动 UI 的通用分层方式。真正需要额外设计的是“细线/小控件在 paint 时如何吸附”，而不是让每个组件自己乘 DPI。

### 2.1 候选方案比较

| 方案 | 做法 | 结论 |
|---|---|---|
| A. 逻辑 Scene + paint 边界换算（本设计） | 全链路 `LogicalPx`，`InstanceEncoder` 统一乘以 DPR 并按 primitive 类型吸附 | 采用。与 WebKit/GPUI/Qt 的做法一致，DPI 变化不重排，应用层无 DPI |
| B. 保留布局吸附，只是禁止 ui_kit 读 scale | 引擎继续把 layout 吸附到设备像素；ui_kit 改读纯逻辑布局快照 | 不采用。需要同时维护“逻辑布局”和“提交布局”两套几何，且 hit/a11y/damage 容易再次分叉，违背单一真相 |
| C. 在 Scene 之前把整棵树铺成物理像素场景 | 布局后立刻生成物理 Scene，应用 Canvas 也按 DPR 烘焙 | 不采用。DPI 变化会让所有 Canvas/缓存失效，应用无法保持逻辑坐标，且文本/路径必须整体重建 |
| D. 完全不吸附，全部依赖 AA | 逻辑坐标直接乘 DPR，不做 pixelSnappedIntRect | 不采用。1px 边框、caret、分隔线在 1.25/1.5 下会发虚，金融桌面 UI 不可接受 |

方案 A 的代价是：paint 需要知道每种 primitive 的物理化与吸附规则，且 golden 会变化；但这些规则本来就是引擎应当统一拥有的，而不是散落在 ui_kit。

## 3. 目标架构

### 3.1 三套单位类型

```rust
pub struct LogicalPx(f32);      // 逻辑像素，布局/API/Canvas/Geometry 的唯一长度单位
pub struct PhysicalPx(f32);   // 连续物理像素，paint 内部用于 SDF/阴影/几何 AABB
pub struct DevicePx(i32);     // 整数设备像素，damage/scissor/atlas/实例 bounds

pub struct Point<T> { pub x: T, pub y: T }
pub struct Size<T> { pub width: T, pub height: T }
pub struct Rect<T> { pub origin: Point<T>, pub size: Size<T> }
pub struct Edges<T> { pub top: T, pub right: T, pub bottom: T, pub left: T }
```

配套别名：

```rust
pub type LogicalPoint = Point<LogicalPx>;
pub type LogicalSize = Size<LogicalPx>;
pub type LogicalRect = Rect<LogicalPx>;

pub type PhysicalPoint = Point<PhysicalPx>;
pub type PhysicalSize = Size<PhysicalPx>;
pub type PhysicalRect = Rect<PhysicalPx>;

pub type DevicePoint = Point<DevicePx>;
pub type DeviceSize = Size<DevicePx>;

pub struct DeviceRect {
    pub x0: DevicePx,
    pub y0: DevicePx,
    pub x1: DevicePx,
    pub y1: DevicePx,
}
```

说明：

- `LogicalPx` / `PhysicalPx` 是 `f32` 长度；`DevicePx` 是 `i32` 整数。`PhysicalPx` 与 `DevicePx` 不能相互隐式转换。
- `DevicePixelRatio` 是唯一的 DPI 类型：

```rust
pub struct DevicePixelRatio(f32);

impl DevicePixelRatio {
    pub const ONE: Self;
    pub fn new(factor: f32) -> Option<Self>;
    pub fn factor(self) -> f32;

    pub fn to_physical(self, value: LogicalPx) -> PhysicalPx;
    pub fn to_logical(self, value: PhysicalPx) -> LogicalPx;
    pub fn to_physical_size(self, size: LogicalSize) -> PhysicalSize;
    pub fn to_physical_rect(self, rect: LogicalRect) -> PhysicalRect;

    pub fn to_device_point(self, point: LogicalPoint) -> DevicePoint;
    pub fn to_device_size(self, size: LogicalSize) -> DeviceSize;
    pub fn to_device_rect(self, rect: LogicalRect) -> DeviceRect;
    pub fn to_covering_device_rect(self, rect: LogicalRect) -> DeviceRect;

    pub fn to_logical_point(self, point: DevicePoint) -> LogicalPoint;
    pub fn to_logical_size(self, size: DeviceSize) -> LogicalSize;
    pub fn to_logical_rect(self, rect: DeviceRect) -> LogicalRect;

    pub fn stroke_width(self, width: LogicalPx) -> PhysicalPx;
}
```

- `DevicePixelRatio` 是唯一持有 DPI 的地方：`LogicalPx ↔ PhysicalPx / DevicePx` 的所有换算都从这里出去，别处不得乘除 DPR。
- `to_device_rect` 是 `pixelSnappedIntRect` 的**唯一实现**（逐边 round，见 §6）；`to_covering_device_rect` 是 floor/ceil 外扩版；`to_physical_rect` 不取整，供 SDF / 阴影 / 圆角半径使用；`stroke_width` 负责边框宽度吸附。`Rect<PhysicalPx>` 在泛型 `Rect` 操作（`translate/inset/outset/union/intersect/contains/min/max/is_empty` 等）之外**额外**提供 `to_aabb()`，不再提供 `snap()`，避免出现第二条吸附实现（两条实现在 f32 下会漂移 1 物理像素）。
- 非有限坐标、非有限/负尺寸输入统一返回零矩形 `DeviceRect::default()`（不是反向哨兵），与旧 `EMPTY_PHYSICAL` 的可见结果一致。

- `DeviceRect` 保持四边 `x0/y0/x1/y1` 的整数矩形语义，用于 damage、scissor、atlas、a11y 等“边缘必须精确”的场景；`Rect<LogicalPx>` / `Rect<PhysicalPx>` 使用 origin + size。这样既保留现有 damage 代码的边界运算，又让逻辑/连续物理矩形有类型。
- `DeviceRect` 的边界恒有序（`x0 <= x1`、`y0 <= y1`），不存在反向空哨兵：空值就是零矩形，`union` 只把“零矩形”当 identity，带位置的零尺寸矩形正常参与聚合。闭区间相交命名为 `overlaps_inclusive`，与 `Rect<T>::intersects` 的半开语义区分开。
- 聚合有两种等价写法：需要表达“还没有任何矩形”时用 `Option<DeviceRect>`（如 `paint/instance.rs::run_bounds` 的 `filter_map(..).reduce(DeviceRect::union)`）；就地累加时以 `DeviceRect::default()`（零矩形）播种即可——零矩形是 `union` 的 identity，因此与 `Option` 写法逐位等价。`OpGroup.bounds` 属于后者（零矩形播种），功能上与 `Option` 等价。
- 删除 `PhysicalBounds`（`paint/instance.rs`），统一使用 `DeviceRect`。
- `Scale` 这个名字被 `DevicePixelRatio` 取代，避免与 `Transform::scale` 混淆。

### 3.2 单向边界

| 层 | 允许的单位 | 允许的 DPI 感知 | 说明 |
|---|---|---|---|
| `ui_kit` / widgets / app code | `LogicalPx` | 否 | 不导入 `DevicePixelRatio`，不调用 `Window::scale`，不乘/除 DPI |
| `ui_core::layout` / `NodeTree` / `Style` / `TextStyle` | `LogicalPx` | 否 | 布局输入输出全部逻辑像素；不得保存 device scale |
| `ui_core::paint::Scene` / `Primitive` / `Canvas` / `Geometry` | `LogicalPx` | 否 | 图元几何是绝对窗口逻辑像素 |
| `ui_core::paint::PaintPass` | `LogicalPx` + `DevicePixelRatio` | 只在生成 damage/clip covering 时 | 不修改布局盒，只把逻辑覆盖区换算为物理 damage |
| `ui_core::paint::InstanceEncoder` | `LogicalPx` → `PhysicalPx` / `DevicePx` | 是，唯一换算点 | 所有 primitive 在这里完成缩放、吸附、物理 bounds 生成 |
| `ui_core::render::*` | `PhysicalPx` / `DevicePx` | 否 | GPU payload、swapchain、atlas、scissor 只接收物理值 |
| `ui_core::platform::*` | `DevicePx`（原生）↔ `LogicalPx`（事件） | 是，平台边界 | 指针、窗口尺寸、IME caret 在这里换算 |
| `ui_core::a11y` | `DevicePx` | 是，a11y 边界 | AccessKit 需要屏幕物理坐标 |
| `ui_core::resources` | 资源 = 源 + 内在尺寸；内部持有物理变体 | 是，资源内部 | 位图登记原生物理像素；SVG 登记内在逻辑尺寸；栅格目标由引擎按绘制盒 × DPR 决定 |

### 3.3 新增/改写的不变量

在 `crates/ui_core/design/custom-gui-engine-design.md` 的 I11 基础上补充：

- **I11-1（应用层无 DPI）**：`ui_kit`、widgets、`profile_ui` 等应用代码不得读取 `DevicePixelRatio`，不得在像素几何计算中乘除 DPI。
- **I11-2（布局纯逻辑）**：`LayoutBox`、`LayoutOverride`、`HitShape`、`Canvas.bounds`、`Primitive` 的几何字段只允许 `LogicalPx`；同一棵树在不同 DPR 下布局结果逐位相同。
- **I11-3（单一换算边界）**：`LogicalPx → PhysicalPx/DevicePx` 只发生在 `InstanceEncoder`（绘制）与平台/资源/a11y 边界；`paint` 之内不得出现组件级 scale 参数。
- **I11-4（物理类型封闭）**：`PhysicalPx`/`DevicePx` 只出现在 `render`、`platform`、`a11y`、`resources` 变体和 test-support 的物理观测面；不进入 `Style`、`PaintSpec`、`TextStyle`、`Canvas`、`Primitive`。
- **I11-5（无兼容别名）**：删除旧的非泛型 `Point/Size/Rect`（改为 `Point<T>/Size<T>/Rect<T>`）、`Scale`、`snap_to_device`、`to_physical_*`、`Window::scale` 等名字，不保留过渡 API。

## 4. ui_core 具体改造

### 4.1 `math.rs` / 单位层

1. 新增 `LogicalPx`、`PhysicalPx`、`DevicePx`、`Point<T>`、`Size<T>`、`Rect<T>`、`Edges<T>`、`DevicePixelRatio`。
2. `Rect<LogicalPx>` 提供 `translate/inset/outset/union/intersect/contains`；`DeviceRect` 提供 `x0/y0/x1/y1/width/height/union/intersect/overlaps_inclusive/translate`。
3. `DevicePixelRatio::to_device_rect()` 返回 `DeviceRect`，是 WebKit `pixelSnappedIntRect` 的唯一实现：

```text
x0 = round(x * s)
y0 = round(y * s)
x1 = round((x + width) * s)
y1 = round((y + height) * s)
width  = x1 - x0
height = y1 - y0
```

右/下边直接用完整坐标 `(x + width) * s` 乘后 round，不先乘再在物理空间相加：f32 下 `x0 + (x1 - x0)` 未必等于 `x1`，那样会得到第二条吸附实现并与逐边 round 相差 1 物理像素。

4. 删除：
   - `Rect::snap_to_device`
   - `Rect::to_physical_f32`
   - `Rect::to_physical`
   - `Rect::to_physical_rounded`
   - `Rect::to_physical_covering`
   - `snap_stroke_width`
   - `Scale`
5. `DevicePixelRatio` 中保留 `to_covering_device_rect`（floor/ceil）用于 damage/clip/a11y；`to_device_rect`（逐边 round）是图形吸附的唯一入口；`to_physical_rect`（不取整）用于 SDF/阴影；`stroke_width` 负责边框宽度。
6. 为了约束逻辑长度构造，提供 `LogicalPx::new(f32)` / `px(f32)` helper；不为 `f32` 实现隐式 `Into<LogicalPx>`，避免物理裸值直接冒充逻辑值。
7. `Transform` 的平移分量改为 `LogicalPx`，矩阵系数保持 `f32`（无量纲）；`GeometryDesc.bounds: Rect<LogicalPx>`，顶点字节仍按逻辑像素解释。

### 4.2 `layout` / `NodeTree`

1. `LayoutBox` 改为：

```rust
pub struct LayoutBox {
    pub bounds: Rect<LogicalPx>,
    pub border: Edges<LogicalPx>,
}
```

2. 删除 `NodeTree.device_scale`、`raw_origins`、`set_device_scale`、`snap_to_device` 调用。`place_subtree` 统一使用未吸附的 `abs.origin` 作为子节点 reference；不再有 `layout_override_of(child)` 的特殊分支。
3. `LayoutOverride.rect` 改为 `Rect<LogicalPx>`，语义固定为“相对父 border box 的逻辑像素矩形”。
4. `Window::set_scale` 不再调用 `invalidate_layout(root)`；DPI 变化只：
   - 更新 `Window::device_pixel_ratio`
   - `full_damage_pending = true`
   - `tree.mark_all_text_dirty()`
   - `a11y.note_bounds_changed()`
   - 非平台写入时重算 surface covering 尺寸
5. 删除 `NodeTree` 中为设备吸附服务的 `raw_changed` / `raw_origin` 逻辑。
6. 更新 `crates/ui_core/src/window/tests.rs`、`crates/ui_core/src/node/tree/tests.rs`：把“DPI 改变 layout bounds”改成“DPI 改变不影响 layout bounds”。

### 4.3 `paint/pass.rs`

1. `PaintPass.scale: DevicePixelRatio`；`frame: DeviceRect`；`scroll_slots: Vec<ScrollSlot>`，其中：
   - `ScrollSlot.offset: Point<LogicalPx>`
   - `ScrollSlot.viewport: DeviceRect`
2. `ClipContext` 的 `covering` 保持 `DeviceRect`，通过 `rect.to_covering_device_rect(context.scale)` 计算。
3. `DamageAccumulator` 只接受物理输入：
   - `push_device(rect: DeviceRect, aa_padding: PhysicalPx, clip: Option<DeviceRect>)`
   - `push_logical(bounds: Rect<LogicalPx>, aa_padding: PhysicalPx, clip: Option<DeviceRect>)` 作为方便的转换入口，内部仍转换为 `DeviceRect`。
4. `painted_content` / `emit_decorations` 的 shadow pad 计算全部在物理空间进行：`blur.to_physical(dpr)` + `SHADOW_PAD_SIGMAS`，不再出现 `(物理 pad × scale + 1) / scale`。
5. B 阶段（typed）后 `push_logical` 的 `aa_padding` 改为 `PhysicalPx`，逻辑 bounds 仅作为转换入口。

### 4.4 `paint/instance.rs` / render

1. `InstanceEncoder::encode` 成为唯一 `LogicalPx → PhysicalPx/DevicePx` 入口。
2. `GpuPrimitive.bounds` 字段保留 `#[repr(C)]` 的 `[f32;4]` 布局以维持 shader 契约，但由 `Rect<PhysicalPx>` 生成；`params0.w`（AA pad）为 `PhysicalPx`。
3. `PhysicalBounds` 删除，`FrameOp::bounds` 使用 `DeviceRect`。
4. Primitive 换算规则：

| Primitive | 换算 | 吸附 |
|---|---|---|
| `Quad`（Solid/Gradient/Border/FocusRing） | `dpr.to_device_rect(quad.bounds)` | `pixelSnappedIntRect` → `DevicePx` 边界；边框宽度 `dpr.stroke_width(border.width)`；圆角 `dpr.to_physical(radius)` 保持 `PhysicalPx` |
| `Shadow` | `shadow.rect.translate(offset).to_physical_rect(dpr)` | 不吸附；blur 为 `PhysicalPx` |
| `Image` | `image.bounds.to_device_rect(dpr)` | 逐边 round（pixelSnappedIntRect） |
| `GlyphRun` | `Rect::from_origin_size(position,size).to_device_rect(dpr)` | 逐边 round；文本光栅位置本身已是整数物理像素 |
| `Clip` | `clip.rect.to_covering_device_rect(dpr)` | 矩形 clip floor/ceil；圆角 clip 矩形 floor/ceil，半径 `PhysicalPx` |
| `Path` | `path.bounds.to_covering_device_rect(dpr)` 作 damage AABB | 顶点不吸附 |
| `Geometry` | `GeometryDraw.transform` 保持逻辑；可见 AABB `to_covering_device_rect` | 顶点不吸附 |
| `Canvas` | 内部每一项按上表处理 | 同上 |

5. `GpuPrimitive` / `FrameOp` / `DeviceRect` / `PhysicalPx` 只出现在 `render` 内部以及 `platform` / `a11y` 的物理边界；`Primitive` / `Scene` 仍是逻辑像素。
6. `RootUniform.frame.params.x` 仍用于 Geometry 顶点着色器的逻辑→物理缩放；这是 render 内部唯一的 DPR 值。

### 4.5 文本与 Canvas 文本

1. `TextStyle.size`、`letter_spacing`、`TextStyleExtras.word_spacing` 改为 `LogicalPx`；`line_height` 作为无量纲倍数保留 `f32`。
2. `TextContent`、`InlineBoxSpec.size`、`TextDecoration.rect` 全部 `LogicalPx`。
3. 删除公开的 `ShapedLine` / `ShapedTextLine` / `ShapedTextItem` / `ShapedGlyphRun` / `ShapedInlineBox` / `ShapedDecoration` / `ShapedTextHandle` / `App::shape_text` / `App::shaped_inline_boxes` / `App::text_scale` / `App::set_text_scale` / `App::clear_text_scale_override`；shaping 快照全部降 `pub(crate)`，`ShapedTextHandle` 整体删除（应用不再持有租约）。
4. `Canvas` 新增：

```rust
pub struct CanvasText {
    pub content: TextContent,
    pub origin: Point<LogicalPx>,
}

pub fn push_text(&mut self, content: TextContent, origin: Point<LogicalPx>) -> bool;
```

   Canvas 内只保存逻辑文本配方（`TextContent` + 逻辑原点），不保存按某个 DPI 光栅化的字形 tile；空文本不产生配方（返回 `false`）。
   应用需要文本尺寸时用 `App::measure_text(&mut self, content: &TextContent, max_width: Option<LogicalPx>) -> LogicalSize`：走同一份
   `TextContext::shape_cached`（键不含 DPR），只测逻辑尺寸、不碰字形图集。`max_width` 必须有限且 ≥ 0：非有限 / 负数不会被静默截断，而是按 `None`（max-content）测量并记一条强类型 `TextDiagnostic::InvalidMeasureWidth { width }`。

5. **Canvas 文本纳入既有 text pass，不给 `PaintPass` 加文本服务。** 理由：`TextContext` / `GlyphRasterizer` / `GlyphAtlas` / `AtlasAllocator`
   全在 `App` 上；`InstanceEncoder` 跑在 compositor 线程；且 atlas 上传发生在 paint **之前**，任何“paint 期才光栅化”的方案都会让新 tile
   晚一帧上传，必须额外补帧。

   数据模型：

```rust
pub enum Primitive {
    Text(Arc<CanvasText>),
}

pub(crate) struct ResolvedCanvasText {
    key: CanvasTextKey,
    lines: Vec<Rc<ShapedLine>>,
    pending: bool,
}

pub(crate) struct CanvasTextKey {
    canvas_generation: u32,
    font_stack_version: u32,
    dpr_bits: u32,
    atlas_generation: u32,
    glyph_eviction_epoch: u32,
}
```

   `Window::canvas_text: HashMap<NodeId, ResolvedCanvasText>` 按 canvas 节点保存解析结果；`Node` 记录 `canvas_generation`，
   `Window::set_canvas` 每次替换画布值都前进一格。

   帧内流程（`App::frame`，已实现）：

   - `refresh_styles` → `refresh_text`（节点文本 shaping，缓存键不含 DPR）→ `refresh_images` → `layout` → `layout_text`（节点文本光栅）；
   - **`App::resolve_canvas_text(window)`**：先 prune（`retain` 仍存在的 canvas 节点，卸载的节点不会留下解析表条目），再按绘制顺序遍历该窗口的
     canvas 节点（`NodeTree::canvas_nodes`，DFS 前序 = paint 序，含嵌套 `Primitive::Canvas`），对每个 `Primitive::Text` 配方走
     `TextContext::shape_cached`（键不含 DPR）+ 按窗口 DPR 光栅化，写入 `Window::canvas_text`；key 命中即跳过。key =
     `(canvas_generation, font_stack_version, dpr_bits, atlas_generation, glyph_eviction_epoch)`，其中 `atlas_generation` 按配方光栅化后
     的最新代际与淘汰纪元落账（一轮内换代不白跑一轮）。解析自身导致图集换代 / 逐 tile 淘汰时，作废节点文本行并重跑解析（有界，
     `MAX_LAYOUT_TEXT_PASSES`）；随后按需重跑 `layout_text`。`pending` 逐节点独立：某节点最后一轮干净就不再算 pending，不会把别的 canvas 的
     skipped 传播过去；只有最终仍有 pending 的节点才补帧 / 升级。
   - 软失败（图集满）把条目标 `pending`；每窗口按“连续解析帧”计数（干净的解析帧清零），连续 3 帧才升级 `AtlasExhausted`，与节点文本同口径。
     相邻帧已解析为 live 的旧结果会保留作回退：新一批结果若含失活 tile 且旧结果仍全活，就不覆盖旧结果，只把条目标 pending 等下一帧重解。
     a11y-only 帧不会吞掉待重试请求：只要该窗口还有 pending，就继续请求补帧。
   - `upload_atlas_pages`：同一帧内新分配的 tile 在 paint 前就绪，**不需要补帧**；
   - `paint_frame`：`PaintPass` 的 `Content::Canvas` 分支按同一遍历顺序把 `Primitive::Text` 替换成 `Primitive::GlyphRun`（只做“配方 →
     已解析结果”替换，不含任何 DPR 逻辑）。替换前逐条校验 `ShapedLine::atlas_generation` 与 tile 存活（`AtlasAllocator::is_alive`）：
     解析缺失或 tile 已失活/被淘汰时**只跳过该文本配方**，同一 canvas 的 quad / image / clip / path / 嵌套 canvas 照常发射，并计入
     `PaintReport::unresolved_canvas_text` → `TextDiagnostic::CanvasTextUnresolved { recipes }`（去重的一次性诊断），同时按窗口有界补帧
     （连续跳过 3 帧后停止请求，避免无限补帧）。因此解析预算耗尽也绝不会画 stale UV。解析结果变化的节点会被标 paint 脏位：
     DPI / 图集换代只重建该 lane 的已解析字形，`Arc<Canvas>` 与应用缓存值都不变。
6. 文本缓存键：`(text hash, style, max_width, font_stack_version)`，**不包含 DPR**；字形图集键包含量化 ppem。DPI 变化只让 `CanvasTextKey.dpr_bits`
   失配，重光栅文本，不重建 `Arc<Canvas>`，也不让应用改任何 cache key。
7. 节点文本路径保持逻辑 shaping + 逻辑布局宽度；`TextPass` 只在光栅阶段使用窗口 DPR。`Window::set_scale` 标记所有文本 dirty，但不让布局失效。
8. 删除的安全网（因为配方不再由应用持有）：`Canvas::without_stale_glyph_runs`、`PaintReport::stale_canvas_runs`、`Window::note_stale_canvas_followup`、
   `App` 中的 stale-canvas 补帧分支、`TextDiagnostic::CanvasTextStale`、`Canvas::push_shaped_text`、`App::shape_text`、`App::text_scale`、
   `App::set_text_scale`、`App::clear_text_scale_override`、`App::shaped_inline_boxes`、`EngineError::InvalidTextScale`、`EngineError::TextShapeStandalone`、
   `App::note_window_dpi`、`text::pack_atlas_lease` 与租约判活路径（`AtlasAllocator::is_alive` 保留，改由画布文本绘制期判活用；槽位纪元仍写进 `AtlasTile` 供测试观测）。
### 4.6 资源 / 图片

资源只描述“源 + 内在尺寸”，fit（Contain/Cover/Fill）是绘制属性，永远不进入资源层；栅格目标由引擎接管（参考 Flutter `ResizeImage` / `ImageProvider`、GPUI `ObjectFit`、Qt `QImage` DPR）。

1. 资源种类与句柄：

```rust
pub enum ImageResourceKind {
    Bitmap { native: Size<DevicePx> },
    Vector { intrinsic: Size<LogicalPx> },
}

pub struct ResourceImage {
    id: ResourceId,
    kind: ImageResourceKind,
}

impl ResourceImage {
    pub fn id(&self) -> ResourceId;
    pub fn kind(&self) -> ImageResourceKind;
    pub fn intrinsic(&self, dpr: DevicePixelRatio) -> Size<LogicalPx>;
    pub fn variant_for(&self, cache: &ResourceCache, target: Size<DevicePx>) -> Option<AtlasTile>;
}
```

   - 位图：登记**原生物理像素**；内在逻辑尺寸 = 原生 / 窗口 DPR。解码期同时生成固定 1/2、1/4（最小 1 边）**预缩 LOD 变体**（Triangle 滤波，不按 contain/cover 目标下采样）；绘制时取“≥ 目标的最小变体”，无则原生，再小由 GPU 采样。
   - SVG：登记**内在逻辑尺寸**（仅用于布局内在尺寸，来自调用方给定的 intrinsic）；**栅格化以文档声明尺寸为准**：`scale_x = target_px / tree.size().width`，`scale_y` 同理。intrinsic 与文档纵横比不一致时文档被拉伸到目标盒（与 CSS `width`/`height` 作用于 `<img>` 一致），不报错。
   - `variant_for` 是只读查询（带回退：精确目标缺失时选最近的可用变体）；生成入口是 `ResourceCache::ensure_variant`，由引擎在布局之后的变体准备阶段调用（写路径不暴露给应用）。
   - SVG 变体生成失败时回退到最近的可用变体，并在 `App::image_diagnostics()` 里记录一次 `ImageDiagnostic::VariantUnavailable`（按资源去重；恢复后自动清除）；容量失败不得让图片静默消失。

2. `ResourceCache` 内部按资源保存变体列表：位图 3 条（原生 + 1/2 + 1/4）；SVG 按**量化后的物理尺寸**缓存栅格变体（长边向上取整到 4 物理像素的倍数，保持宽高比），DPI 或布局盒变化时按新目标取/生成变体。每资源变体数上限为 8，超出按 LRU 淘汰期间未被当前帧引用的变体，淘汰会归还图集 tile 与 byte budget；当前帧 `prepare` 触达的变体、以及经 `App::prepare_image_variant` 交给 Canvas/行内图片的变体（exposed）优先保留。SVG 变体的解码预算按 `width × height × 4 B + 小常量` 估算，不使用位图的解码峰值模型。`ResourceStats.{entries, tiles}` 分别统计资源数与变体数。

3. `ResourceImage` 的公开面只有 `id()` / `kind()` / `intrinsic(dpr)` / `variant_for(cache, target)`；`logical_size()`、`tile()`、`reselect`、`insert_for_display`、`insert_at_size`、`insert_svg_at_size` 全部删除，`App::resources_mut` 一并收口；`ResourceCache` 的写入口（`insert*` / `unload` / `release_window`）全部 `pub(crate)`，外部只有只读 `&ResourceCache`。

4. 资源 API 只接收“源 + 内在尺寸”，不再接收逻辑/物理目标：

```rust
impl App {
    pub fn insert_bitmap_for_window(
        &mut self,
        window: WindowId,
        bytes: &[u8],
    ) -> Result<ResourceImage, EngineError>;

    pub fn insert_svg_for_window(
        &mut self,
        window: WindowId,
        bytes: &[u8],
        intrinsic: Size<LogicalPx>,
    ) -> Result<ResourceImage, EngineError>;

    pub fn insert_bitmap_async_for_window(
        &mut self,
        window: WindowId,
        bytes: Bytes,
    ) -> Result<PendingResource, EngineError>;

    pub fn insert_svg_async_for_window(
        &mut self,
        window: WindowId,
        bytes: Bytes,
        intrinsic: Size<LogicalPx>,
    ) -> Result<PendingResource, EngineError>;
}
```

   同步版登记 owner；异步版在交付时登记 owner，关窗会回收 owner 引用并丢弃该窗口仍在途的解码任务。同一 `(window, resource)` 只登记一次 owner 引用，重复插入不复用也不累加（owner 列表不随反复打开菜单增长）。

5. 目标物理尺寸 = 绘制盒经 `ImageFit` 计算后的实际绘制尺寸 × DPR，向上取整到整数：Contain 取内嵌后的等比盒，Cover 按覆盖比例放大 intrinsic（UV 裁切发生在绘制期），Fill 取节点盒。计算入口是 `node::image::variant_target`，绘制与变体准备共用同一函数。

6. 节点 `ImageContent.intrinsic` 由 `ResourceImage::intrinsic(dpr)` 得出。DPI 变化时：位图的内在逻辑尺寸变化并触发 `mark_layout`（本设计的**唯一允许例外**，因为位图形状由原生像素决定）；SVG 内在尺寸不变、只按新 DPR 重新选择/生成变体。引擎为每个图片节点记录上次准备的目标物理尺寸，目标变化即重绘该节点（DPI 换档时 SVG 也要换 tile）。

7. 绘制（`paint::pass`）：按 `fit_bounds` 得图元逻辑 bounds，按 `variant_target` 得物理目标，再向资源取 `variant_for`（失败/缺失时回退到最近可用变体）；`Primitive::Image` 仍只带 `AtlasTile` + 逻辑 bounds，逻辑场景不存 DPR。Cover 在拿到变体后按 `cover_tile` 裁 UV。

8. 变体准备在布局之后、图集上传之前完成：新生成的变体同帧可见，不需要额外一帧；准备阶段只为真正新增/换档的变体标记节点重绘。

9. Canvas / 行内图片通过公开入口取变体：

```rust
impl App {
    pub fn prepare_image_variant(
        &mut self,
        window: WindowId,
        image: ResourceImage,
        target: Size<LogicalPx>,
        fit: ImageFit,
    ) -> Option<AtlasTile>;
    pub fn image_diagnostics(&self) -> &[ImageDiagnostic];
}
```

   `prepare_image_variant` 按窗口 DPR 计算物理目标并生成/复用变体，返回的变体标记为 exposed（LRU 优先保留）。

### 4.7 `window` / `app` / `platform` / `a11y`

1. `Window::size()` / `set_size()` / `WindowOptions.size` 全部 `Size<LogicalPx>`；`WindowOptions.scale` 从生产 API 删除，测试用 `TestApp::with_scale` 注入。
2. `Window::scale()` / `set_scale()` / `dpi_epoch()` 从公开 API 删除；`set_scale` 保留 `pub(crate)` 给平台层，DPR 为引擎内部字段。test-support 侧的注入 / 观测面是 `TestApp::{with_scale, set_dpr}` 与 `Window::{device_pixel_ratio, set_device_pixel_ratio, physical_surface}`。
3. `Event::DpiChanged` 只保留 `dpi: u32`（诊断信息）；删除 `scale` 字段。应用不需要也不应该接收 DPI 比例来做几何。
4. `App::note_window_dpi`、`default_text_scale`、`text_scale_override` 删除；`TextScale` 若未来要支持无障碍字号，作为独立应用级字号倍率而不是 DPI。
5. 平台层：
   - 指针事件、窗口 resize、IME caret、拖拽区域继续在平台边界做 `DevicePx → LogicalPx` 换算。
   - 窗口 surface / swapchain / present 保持 `DevicePx`。
6. `a11y::A11yNode.bounds` / `A11yTree` 使用 `DeviceRect`；`a11y::mapping::bounds_of` 使用 `DevicePixelRatio::to_covering_device_rect`。

### 4.8 公开 API 删除清单

以下旧接口全部删除，不保留别名或兼容层：

- 旧的非泛型 `math::{Point, Size, Rect}` 与 `math::Scale`（替换为 `Point<T>/Size<T>/Rect<T>` 和 `DevicePixelRatio`）
- `Rect::{snap_to_device, to_physical_f32, to_physical, to_physical_rounded, to_physical_covering}`
- `math::snap_stroke_width`（算法移入 `DevicePixelRatio::stroke_width`）
- `Window::{scale, set_scale, dpi_epoch}`（`set_scale` 降为 `pub(crate)`）
- `WindowOptions.scale`
- `Event::DpiChanged.scale`
- `App::{shape_text, shaped_inline_boxes, text_scale, set_text_scale, clear_text_scale_override, note_window_dpi}`
- `text::{ShapedLine, ShapedTextLine, ShapedTextItem, ShapedGlyphRun, ShapedInlineBox, ShapedDecoration}`（全部降 `pub(crate)`）
- `text::ShapedTextHandle` 与 `text::pack_atlas_lease`（整体删除：应用不再持有租约）
- `App::atlas_generation_lease`（`ShapedTextHandle` 的唯一消费者）
- `Canvas::{push_shaped_text, without_stale_glyph_runs}`；`Canvas::push_text(GlyphRun)` 改名 `push_glyph_run` 并降 `pub(crate)`
- `Window::note_stale_canvas_followup`、`PaintReport::stale_canvas_runs`、`TextDiagnostic::CanvasTextStale`
- 编码期 stale-run 安全网（`Canvas::without_stale_glyph_runs` / `PaintReport::stale_canvas_runs`）；`AtlasAllocator::is_alive` 与 `Slot::epoch` 保留，改由画布文本绘制期判活
- `EngineError::{InvalidTextScale, TextShapeStandalone}`
- `ResourceCache::{insert_for_display, insert_svg_for_display, insert_owned_svg_for_display, insert_owned_at_size, insert_at_size, insert_async_at_size}` 的公开面
- `App::resources_mut`（应用只能走逻辑资源 API）
- `PhysicalBounds`（合并到 `DeviceRect`）

## 5. ui_kit / 应用层改造

### 5.1 Toggle 简化

删除：

- `window_scale`
- `device_length`
- `device_length_with_parity`
- `TrackMetrics` 中的 `inset` 物理换算
- `sync_knob` / `sync_ring` / `sync_mark` 中的 `× scale`、`round`、`/ scale`

改为：

```rust
struct TrackMetrics {
    width: LogicalPx,
    height: LogicalPx,
    radius: LogicalPx,
    inset: LogicalPx,
}

fn track_metrics(size: UnifiedSize) -> TrackMetrics {
    let height = size.icon_size() * 1.25;
    TrackMetrics {
        width: height * 1.9,
        height,
        radius: height / 2.0,
        inset: LogicalPx::new(2.0),
    }
}
```

`sync_knob` 直接在逻辑空间计算 `offset = inset + mix * travel`，写回 `Rect<LogicalPx>`；`sync_ring` / `sync_mark` 使用静态逻辑矩形。物理像素对齐由 `InstanceEncoder` 的 `Quad` 吸附完成。

结果：Toggle 中约 200 行 DPI 专用计算与辅助函数被删除，组件回到普通逻辑布局规模；不再有 DPI 专用测试。小数 DPI 下允许最多 1 物理像素的居中偏差（WebKit/CSS 的常规行为）。若产品要求严格对称，设计系统使用 4 的倍数的逻辑尺寸（1.25/1.5/1.75/2/2.5/3 下均为整数物理像素），而不是把 DPI 带回组件。

### 5.2 资源类组件

- `Brand::resource_for`：调用 `App::insert_bitmap_for_window(window, bytes)`；位图不再接收逻辑目标，缓存键由内容字节去重，内在尺寸由引擎按窗口 DPR 维护。
- `IconSpec::resource_for` / `mount_dynamic_icon`：调用 `App::insert_svg_for_window(window, bytes, intrinsic)`，`intrinsic: Size<LogicalPx>` 来自图标的设计尺寸；不再传“显示目标”。
- SVG 的 `intrinsic` 只是**布局内在尺寸**，不是文档尺寸：栅格化按 SVG 自身声明尺寸（`tree.size()`）映射到目标物理像素。只有 viewBox 的资产（如 DiceBear）不会因为 `intrinsic` 较小而被裁掉。
- Canvas / 行内图片：用 `App::prepare_image_variant(window, image, target, fit)` 取目标变体；`ResourceCache` 的写入口不可达。
- `BrandKind::native_size()` 用 `OnceLock` 缓存 PNG 头解析结果，build 热路径不再重复解析。
- `Menu::IconCache`：直接删除组件级缓存，依赖 `ResourceCache` 的 `ContentKey` 去重。
- `AssetImages::request`：增加 `window: WindowId` 参数并调用资源 API；位图异步插入不再传逻辑尺寸。
- 组件只读 `ResourceImage::intrinsic(dpr)`（仅 test-support 断言需要）或直接使用节点 `ImageFit`；不再调用 `ResourceImage::tile()`、`logical_size()`，也不在组件内按 scale 选变体。
- `ImageFit` 只出现在节点的绘制属性上（`NodeBuilder::image` / `ImageContent.fit`）；组件不得把 fit 传给资源 API。
- 所有 `window.scale()` 调用从 `ui_kit/src` 消失；grep 门禁见 §8。

### 5.3 读取 layout bounds 的组件

`TabBar`、`ScrollBar`、`Input`、`ColorPicker`、`Overlay`、`Dialog`、`List`、`Markdown`、`Spinner` 等不再做任何物理换算：

- `track.layout().bounds`、`text.layout().bounds` 现在纯粹是逻辑像素；
- Canvas 文本用 `Canvas::push_text`，不再读 `App::text_scale`；
- 需要像素级对齐的细线（caret、underline、separator）由 `InstanceEncoder` 对 Quad 做 `pixelSnappedIntRect` 处理，应用只需要画逻辑矩形。

### 5.4 `StyleBuilder` / `PaintBuilder`

- 将 `crates/ui_kit/src/style_builder.rs` 移入 `crates/ui_core/src/paint/style_builder.rs`（**B1b 已完成**，`ui_kit` 侧删除且无兼容层）。
- `Style.inner` 字段私有；`inner()` 只读访问器仅在 `test-support`（或 crate 内测试）下公开，生产构建为 `pub(crate)`；外部不再直接写 taffy 字段。
- `StyleBuilder` 的所有长度方法（`gap`、`padding`、`size`、`min_size`、`max_size`、`inset`、`border` 等）接受 `LogicalPx`；`PaintBuilder` 的 `rounded` / `border` 同样接受 `LogicalPx`。
- `ui_kit`、widgets、examples 的 `style.inner.size = Dimension::length(...)` 全部迁移到 `StyleBuilder` 或 `Style` 的 typed setter（**B1b 已完成**：ui_kit 63 处 / prediction_market 4 处 / market_participants_ui 7 处）。
- taffy 枚举（`Dimension` / `LengthPercentageAuto` / `Display` / `Position` / `AlignSelf` / `JustifyContent` / `Overflow`）仍作为 typed setter 入参与只读访问器出现在公开面；消除它们属于 C1 的单位类型与枚举收口，B1 不做。
- 已知边界（P3-3）：`Style::from_taffy` 是 `#[cfg(test)]` 后门；`grid` / `aspect_ratio` / `flex_wrap` 等 taffy 字段暂无 typed setter，测试直接用 taffy 构造，本阶段不补。
- `PaintSpec`、`Border`、`ShadowSpec`、`FocusRing` 的像素字段全部 `LogicalPx`。

### 5.5 widgets / profile_ui / gallery

- `widgets/**`：所有 `App::text_scale()` 缓存键、`ShapedTextHandle::scale()`、`Canvas::push_shaped_text` 迁移到 `Canvas::push_text` + 逻辑文本；文本缓存键不再包含 `text_scale`。
- `widgets/x`、`profile_ui`：`insert_owned_svg_for_display` / `insert_owned_at_size` 改为 `App` 逻辑资源 API；`profile_ui` 的 Dicebear 头像改为引擎 SVG 资源（`App::insert_svg_async_for_window`，逻辑尺寸 `56×56`），删除 `rasterize_avatar_svg` 与 `image` / `resvg` / `usvg` 三个依赖。
- `ui_kit/examples/gallery`：删除 `window.scale()` 的状态行；`Event::DpiChanged` 分支只读 `dpi`。
- `kytrix` 外壳的 `WindowOptions` 不再传 scale；平台事件自动决定。

### 5.6 测试迁移

- `ui_kit::toggle` 的物理像素断言全部删除，改为：
  - 逻辑 `LayoutOverride` 几何断言（`Rect<LogicalPx>`）；
  - `ui_core` 的 `FrameRecord`/`PhysicalFrameRecord` 断言（物理边界为整数、尺寸正确）；
  - 1.25/1.5/2.0 的像素快照或 quad 记录断言。
- `ui_kit::brand` / `icon` 的 `scale` 测试迁移到 `ui_core::resources` 的变体测试；ui_kit 只断言逻辑尺寸和资源存在。
- `ui_core` 增加 DPI 不改变 layout 的测试、Quad/Image/Clip 物理边界测试、Canvas 文本 DPI 重光栅测试。

## 6. 像素吸附规则

采用 WebKit `pixelSnappedIntRect` 作为统一规则：

```text
给定逻辑矩形 (x, y, width, height) 和 DPR s：
x0 = round(x * s)
y0 = round(y * s)
x1 = round((x + width) * s)
y1 = round((y + height) * s)
DeviceRect = (x0, y0, x1, y1)
```

- 左/上边由逻辑坐标乘 s 后 round，右/下边由完整右/下坐标乘 s 后 round，再相减得到宽高；不单独 round 宽高。这样相邻元素的共享边界会落在同一物理像素上，不会产生缝隙。
- 右/下边必须是 `round((x + width) * s)`，不能写成 `round(x * s) + round((x + width) * s - x * s)` 之类的物理空间拼装：f32 下两种写法在 248k 组随机矩形上会差 1 物理像素，只保留一种实现。
- `Quad` 的背景、渐变、边框、焦点环都使用该规则；边框宽度再用 `stroke_width` 取整且至少 1 物理像素。
- `Image` / `Glyph` 使用同一规则；文本位图自身已经是整数物理像素。
- `Clip` / `Damage` 使用 `floor` 左上、`ceil` 右下的外扩规则，保证不裁掉内容。
- `Shadow` / `Path` / `Geometry` 不吸附顶点；阴影模糊和几何抗锯齿保持连续。
- 圆角半径按 `dpr.to_physical(radius)` 保持 `PhysicalPx`，不强制整数；极小半径由 `normalize_physical_corner_radius` / `normalize_logical_corner_radius` 按外框尺寸 clamp。
- 物理跨度 <1 物理像素、整块经 round 退化为空的填充/渐变 `Quad` 不绘制；边框在正常尺寸 Quad 上由 `stroke_width` 保底 ≥1 物理像素。
- 边框宽度先按 `stroke_width` 取整且至少 1 物理像素，再 clamp 到 `min(宽, 高)`：短边 1 物理像素的 Quad 上边框宽度 clamp 到短边（内盒归零），shader 不会反转内边界；中心线仍按 AA 计算约 50% alpha。
- 小数 DPI 下轴对齐矩形最多产生 1 物理像素的尺寸/位置偏差，这是 CSS/WebKit 的既定取舍；不以此为由在 `ui_kit` 内做设备空间补偿。
- 圆角 clip 的矩形按 floor/ceil 外扩，其 AA 边在分数 DPI 下最多外移 1 物理像素（矩形 clip 无此偏差）。

## 7. 迁移计划

按“行为先行、类型后行”落地：先让行为契约正确（布局纯逻辑、paint 单一换算、吸附规则统一），golden 只重基一次；再做单位类型迁移，此时必须证明像素零 diff。

每一步都以编译器和测试为门禁；不在中间状态保留旧 API 兼容层。

### 阶段 A：行为重构（`f32` 类型不变）

**A1 布局纯逻辑 + paint 吸附 + damage/shadow pad 物理化**

- 删除 `NodeTree.device_scale` / `raw_origins` / `set_device_scale`。
- 删除 `place_subtree` 的 `committed` / `snap_to_device` / `layout_override` 特判；`LayoutOverride` 统一相对父节点的未吸附 `abs.origin`。
- `Window::set_scale` 只做 DPR / 全量 damage / 文本 dirty / a11y 通知 / surface 尺寸，不再 `invalidate_layout`。
- `InstanceEncoder`：Quad 改 `pixelSnappedIntRect`、边框 `stroke_width`、Image/Glyph 逐边 round、Clip/Damage 外扩、Shadow/Path/Geometry 不吸附。
- `DamageAccumulator` 只接受物理输入（`DeviceRect` + `PhysicalPx` AA pad）；shadow pad 全部在物理空间计算。
- 重写 layout/DPI 测试；新增 §8.1 的物理边界测试。
- 收口门禁：`cargo check --workspace --all-targets`、`cargo test -p ui_core --lib`、`cargo test -p ui_kit --lib`。

**A2 Golden 重基（三端）**

- 逐图确认并说明预期变化区域。矩形 clip 预期**零变化**（实测逐像素一致）；变化区是圆角 clip 与文本/布局派生几何。零变化结论必须以 `Tolerance::EXACT` 或逐图 diff 验证——默认容差不足以证明零变化。
- 收口门禁：`cargo test -p ui_core --features test-support --test snapshot`（三端）。

**A3 `ui_kit::Toggle` 去 DPI**

- 删除 `window_scale` / `device_length` / `device_length_with_parity` / 设备空间 `sync_*`。
- 4 个 DPI 测试改写为逻辑几何断言。

### 阶段 B：Style 收口

**B1 `StyleBuilder` / `PaintBuilder` 迁入 `ui_core::paint::style_builder`**

- 补齐 typed setter（`display` / `align_self` / 百分比尺寸 / `min_size` / `max_size` / `overflow` / `inset`）。
- `Style.inner` 字段私有 + `inner()` 只读访问器（`test-support` 外为 `pub(crate)`）；迁移 `ui_core` 内全部字面量与 taffy 直写。

**B2 下游迁移**

- `ui_kit` / `widgets` / `*_ui` / `kytrix` 全部迁到 `ui_core` 的 builder 与 typed setter；删除下游 `taffy` 依赖。

### 阶段 C：单位类型

**C1 `ui_core` 单位层**

- 新增 `LogicalPx` / `PhysicalPx` / `DevicePx` / `DevicePixelRatio` / `Point<T>` / `Size<T>` / `Rect<T>` / `Edges<T>` / `DeviceRect`。
- 删除非泛型 `Point/Size/Rect`、`Scale`、`to_physical_*`、`snap_to_device`、`snap_stroke_width`、`PhysicalBounds`。
- 全部几何字段单位化；`#[repr(C)]` 与 `size_of` 基线逐条复核。
- 收口门禁：`cargo check -p ui_core --all-features --all-targets`、`cargo test -p ui_core --lib`、golden 零 diff（证明类型迁移无行为变化）。

**C2 下游单位类型迁移**

- 按 crate 逐个迁移，每个 crate 收口自己的 `cargo test -p <crate>`。

### 阶段 D：资源引擎接管栅格目标

**D1 引擎侧资源模型**

- `ResourceCache` 内部按资源保存变体列表；scale 方法降 `pub(crate)`；`App::resources_mut` 收口。
- `ResourceImage` 字段私有，只暴露 `id()` / `kind()` / `intrinsic(dpr)` / `variant_for(cache, target)`。

**D2 引擎侧逻辑 API**

- 新增 `App::insert_{bitmap,svg,bitmap_async,svg_async}_for_window`。
- 位图只登记原生像素，SVG 只登记内在逻辑尺寸；同步与异步版本都登记 owner，关窗回收并取消在途解码。

**D3a 引擎接管栅格目标（本轮）**

- `ImageResourceKind::{Bitmap { native }, Vector { intrinsic }}`；删除 `logical_size` / `tile` / `reselect` / `insert_for_display` / `insert_at_size` / `insert_svg_at_size`。
- 变体准备阶段（布局之后、图集上传之前）按 `variant_target` 生成/复用 SVG 变体；位图只保留原生变体。
- `paint::pass` 图片发射改为 `fit_bounds` + `variant_target` + `variant_for`；Cover 在变体 tile 上裁 UV。

**D3b 应用侧资源调用迁移**

- `Brand` / `Icon` / `Interactive` / `Menu` / `AssetImages` / `widgets/x` 迁新资源 API；组件删除 `tile()` / `logical_size()` / 组件级变体选择。
- `Menu::IconCache` 删除。

**D3c 应用侧栅格/依赖清理**

- `profile_ui` Dicebear 走引擎 SVG 资源，删 `image` / `resvg` / `usvg`。
- 所有 `window.scale()` 调用从 `ui_kit/src` 消失；grep 门禁见 §8。

### 阶段 E：Canvas 文本（已实现）

**E1 引擎侧（已落地）**

- `Canvas::push_text(TextContent, origin)` + `Primitive::Text(Arc<CanvasText>)`；空文本不产生配方。
- `Window::canvas_text: HashMap<NodeId, ResolvedCanvasText>` 解析表（key = canvas generation / 字体栈版本 / DPR bits / atlas generation / 字形淘汰纪元；
  `Node::canvas_generation` 随 `set_canvas` 前进）+ `App::resolve_canvas_text(window)`（在 `upload_atlas_pages` 之前）。
- `PaintPass` 的 `Content::Canvas` 分支做配方 → `GlyphRun` 替换；解析缺失时跳过并计入 `PaintReport::unresolved_canvas_text` →
  `TextDiagnostic::CanvasTextUnresolved`。
- `App::measure_text(&TextContent, max_width)`：逻辑像素测量入口，走同一 shaping 缓存、不含 DPR。
- 删除 `App::shape_text` / `text_scale` / `set_text_scale` / `clear_text_scale_override` / `shaped_inline_boxes`、`ShapedTextHandle`、
  `Canvas::push_shaped_text`、`without_stale_glyph_runs`、`stale_canvas_runs`、`note_stale_canvas_followup`、
  `TextDiagnostic::CanvasTextStale`、`EngineError::{InvalidTextScale, TextShapeStandalone}`、`App::note_window_dpi`、
  编码期 stale-run 安全网；`AtlasAllocator::is_alive` 保留用于画布文本绘制期判活。`ShapedLine` 等全部降 `pub(crate)`。

**E2 widgets 迁移（已落地）**

- 7 个图表 widget、`coin_market_cap_fear_and_greed_index`、`hyperliquid_wallet` 与 `widget` crate 迁 `push_text`；
  文本缓存改为「逻辑输入 + `App::measure_text` 宽度」，删除 `ChartTextCache.scale`、`text_scale` 键、`has_stale` / `drop_stale`、
  `painted_text` 句柄留存等 DPI / 租约失效判据。

**E1 新增测试**：`canvas_text_does_not_rebuild_on_dpi_change`、`a_matching_recipe_key_does_not_shape_again`、
`nested_canvases_resolve_their_own_text_in_paint_order`、`changing_the_recipe_reresolves_the_canvas`、
`new_glyph_tiles_are_ready_before_the_atlas_upload`、`measure_text_*`、`canvas_text_paints_the_recipe_color`、
`canvas_text_reports_atlas_pressure_and_retries`、`canvas_text_recipes_become_glyph_runs_during_paint`、`an_unresolved_canvas_recipe_is_skipped_and_counted`。

### 阶段 F：DPI 公开面收口（已实现）

- 删除 `Window::{scale, dpi_epoch}` 公开面与 `WindowOptions.scale`；`set_scale` 降为 `pub(crate)` 给平台层；`Event::DpiChanged` 只保留 `dpi: u32`（`App::{note_window_dpi, default_text_scale, text_scale_override}` 已随 E 阶段文本 scale 机制一并删除）。
- test-support 侧的 DPR 注入 / 观测面：`TestApp::{with_scale, set_dpr}`、`Window::{device_pixel_ratio, set_device_pixel_ratio, physical_surface}`、`SnapshotCase.scale`；`FrameScript` 的 DPI 变化走 `TestApp::set_dpr`。
- `ui_core` 的 `frameless_smoke` / `p1_win_placement_evidence` / `acceptance` 加 `required-features = ["test-support"]`，物理观测收敛到测试面；`demo` 的图标变体改走 `App::prepare_image_variant`，不读 DPR。
- 全仓 grep 旧公开面为 0（`Window::scale` / `.scale().factor` / `WindowOptions` 的 `scale` 字段 / `Event::DpiChanged.scale`）；`dpi_epoch()` 是新的引擎内部名（`pub(crate)`），只出现在 crate 内与既有测试名 `quads_snap_to_device_pixels` 的白名单。
- **F1 新增测试**：`dpi_changed_exposes_only_the_diagnostic_dpi`、`default_window_starts_at_unit_device_pixel_ratio`、`with_scale_injects_the_device_pixel_ratio`、`with_scale_rasterizes_canvas_text_at_the_injected_ratio`、`a_dpi_change_repaints_without_relayout_and_the_event_is_diagnostic_only`；`dpi_change_does_not_invalidate_layout` 保留。

### 阶段 G：验收

- `cargo +nightly fmt --all -- --check`
- `cargo +nightly clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo test --workspace`
- `cargo test -p ui_core --features test-support --test snapshot`（按新基线）
- `cargo test -p ui_core --features test-support --test gui_smoke -- --ignored`
- `acceptance -- cpu` 文本帧阈值记为既有缺陷，不作为本轮门禁。

## 8. 测试与验收

### 8.1 必须新增的测试

| 测试 | 断言 |
|---|---|
| `layout_is_identical_at_every_device_scale` | 同一棵树在 1/1.25/1.5/1.75/2/3 下 `LayoutBox.bounds` 逐位相同 |
| `quads_snap_to_device_pixels` | 1.25/1.5 下 Quad 的物理 `x/y/x1/y1` 全为整数，且比未吸附版本最多差 1 物理像素 |
| `image_and_quad_share_physical_edges` | 同一逻辑矩形上的 Image 与 Quad 物理边界一致 |
| `borders_are_integer_device_pixels` | 1/1.25/1.5/2/3 下边框宽度为整数物理像素且 ≥1 |
| `damage_covers_the_unsnapped_rect` | Damage 使用外扩规则，覆盖 Quad 未吸附/吸附后的所有边界 |
| `dpi_change_does_not_invalidate_layout` | `set_scale` 后根没有 LAYOUT 脏位，layout bounds 不变 |
| `dpi_change_rerasterizes_text_only` | 文本 tile 换代但文本布局宽度不变 |
| `canvas_text_does_not_rebuild_on_dpi_change` | 同一 `Arc<Canvas>` 在 DPI 变化后仍可绘制，文本按新 DPR 重光栅（E1 已落地：`app::canvas_text::tests`） |
| `a_matching_recipe_key_does_not_shape_again` | 配方 key 命中时不再调用 shaping（E1 已落地） |
| `nested_canvases_resolve_their_own_text_in_paint_order` | 多个 canvas 节点按 paint 序各自解析、各自保留 origin（E1 已落地） |
| `changing_the_recipe_reresolves_the_canvas` | 内容/样式变化 ⇒ `canvas_generation` 前进并重新解析（E1 已落地） |
| `new_glyph_tiles_are_ready_before_the_atlas_upload` | 同帧新 tile 在 `upload_atlas_pages` 前就绪，无需补帧（E1 已落地） |
| `measure_text_*` | 逻辑测量：空文本为零、限宽增高、与窗口 DPR 无关（E1 已落地） |
| `ui_kit_has_no_device_scale_dependency` | grep 门禁（见 §8.3）（阶段 A4/E1 落地） |
| `toggle_logical_geometry_is_stable_at_every_dpi` | Toggle 的 layout override 逻辑几何与 DPI 无关（阶段 A4/E1 落地） |

`SnapshotCase.scale` / `TestApp::with_scale` / `TestApp::set_dpr` 只作为 test-support 的 DPR 注入入口保留，不进入生产公开 API。

### 8.2 视觉验收

- 1× / 1.25× / 1.5× / 1.75× / 2× / 3× 六档 DPI。
- 逐项检查：1px 边框、分隔线、输入框 caret、Toggle 开关/复选/单选、ScrollBar thumb、TabBar indicator、Progress、图标。
- 比较同一 UI 在 1.25/1.5 下的物理像素截图：边界应落在整数设备像素上；除字符/图形 AA 外不允许 1px 缝或双线。
- 圆角 clip 在分数 DPI 下 AA 边最多外移 1 物理像素（矩形 clip 零偏差），该处需人工验收确认不影响圆角容器边缘观感。
- Golden 基线变更必须逐图说明预期变化区域；不允许只“bless 掉”差异。
- macOS 无屏幕录制 TCC 授权、Linux 无 compositor，三端真窗口截图不可用；像素证据由三端 golden 与 Windows 真窗口人工验收承担。

### 8.3 静态门禁

```powershell
Select-String -Path crates/ui_kit/src/**,crates/widgets/**,crates/profile_ui/src/** -Pattern 'Window::scale|\.scale\(\)\.factor|device_length|device_length_with_parity|text_scale|shape_text|push_shaped_text'
Select-String -Path crates/ui_core/src/**,crates/ui_kit/src/** -Pattern 'snap_to_device|to_physical_f32|to_physical_rounded|to_physical_covering|snap_stroke_width'
Get-ChildItem -Recurse -Filter *.rs crates | Select-String -Pattern 'Window::scale|\.scale\(\)\.factor|dpi_epoch()|WindowOptions \{ scale|DpiChanged \{ dpi, scale'
```

三个门禁在阶段 F 后都必须是 0（测试代码里的纯逻辑 scale 常量除外，需明确白名单）。

### 8.4 现有测试基线

- `cargo test --workspace`：8604 passed / 0 failed / 30 ignored（102 个测试目标）。
- `cargo test -p ui_core --lib --features hud,test-support`：2348 passed / 1 ignored（E1 删除 standalone shaping API 后其 24 条句柄/租约用例退场，新增 18 条 canvas 文本 / measurement / pass 替换 / 字体栈 / 卸载 prune / 补帧预算用例 + 2 条节点文本 inline-box / ellipsis 等价覆盖；F1 新增 5 条 DPI 公开面用例；`math` 单测 40 个）。
- `cargo test -p ui_kit --lib`：765 passed（kit 716 / theme 49；`style_builder` 测试已随模块迁入 `ui_core`）。
- `cargo test -p ui_core --features test-support --test snapshot`：11 passed（WARP 14 张 golden，C1 期间逐图 `max=0 avg=0.000 mismatch=0`；F 阶段重跑仍为 11 passed，无重基）。
- `gui_smoke`：`--features test-support --test gui_smoke` 默认 1 ignored（需要真实窗口/显示）；`--ignored` 在 WARP 下 1 test（内部 6 场景）PASS。
- `cargo check -p ui_core --all-targets`（**不带 feature**）：0 warning / 0 error。3 个 `required-features = ["test-support"]` 的 example（`snapshot_demo` / `frameless_smoke` / `acceptance` 等）与 `frame_record` / `snapshot` / `layout` / `hud` 集成测试在无 feature 时被 Cargo 跳过，属预期；带 feature 的完整门禁见上。
- `acceptance -- cpu`：**既有缺陷**，文本帧 p99 ≈ 5.9–7.0 ms > 4 ms 阈值；与本轮重构无关。
- Golden 数量与三端目标保持不变（42 张）。

## 9. 风险与取舍

1. **迁移面大**：ui_core 中几何 token 约 4.2k 处（含测试），ui_kit 约 0.94k 处，widgets 约 1.8k 处。采用一次性类型迁移，编译器可发现绝大多数问题；禁止用 `as f32` 静默绕过。
2. **Golden 会变**：布局不再吸附、Quad 改为 paint 吸附必然产生像素基线变化。行为阶段收口时一次性重基并逐图确认。
3. **文本改造风险最高**：`Canvas::push_text` 改的是帧内阶段顺序（新增 `resolve_canvas_text` 在 `upload_atlas_pages` 之前），影响文本缓存、图集淘汰、IME/caret 路径；必须单独一批实现和回归。
4. **`Style.inner` 私有化的迁移成本**：实测 `style.inner` 写入点 274 处（ui_core 166 / ui_kit 100 / widgets 4 / market_participants_ui 4），其中约 30 处是非长度字段（`display` / `align_self` / 百分比尺寸），必须在阶段 B1 一并补齐 typed setter。
5. **性能**：单位类型全部 `#[repr(transparent)]`，零运行时开销；`Canvas::push_text` 每帧按 key 命中跳过，目标不高于当前 `shape_text` 稳态成本。
6. **小数 DPI 的对称性**：不保证 Toggle 等装饰形状绝对对称；这是 WebKit/CSS 的既定取舍。若产品验收要求严格对称，应使用 4 的倍数逻辑尺寸或另立“设备对齐组”设计，不应回到组件内 DPI 计算。
7. **examples 依赖 DPI 做证据**：`frameless_smoke` / `p1_win_placement_evidence` / `acceptance/windowed` 需要 `Window::scale`，删除公开面后改为 `required-features = ["test-support"]` + test-support 物理观测面。

## 10. 参考资料

- Chromium Blink Coordinate Spaces: https://www.chromium.org/developers/design-documents/blink-coordinate-spaces/
- WebKit LayoutUnit 与 pixelSnappedIntRect: https://trac.webkit.org/wiki/LayoutUnit
- Qt High DPI（device-independent pixels / QPainter / QImage DPR）: https://doc.qt.io/qt-6.8/highdpi.html
- GPUI geometry（Pixels / ScaledPixels / DevicePixels）: https://github.com/zed-industries/zed/blob/main/crates/gpui/src/geometry.rs
- egui tessellator（pixels_per_point / feathering）: https://github.com/emilk/egui/blob/master/crates/epaint/src/tessellator.rs
- Flutter ViewConfiguration（physical/logical constraints、devicePixelRatio、toMatrix）: https://api.flutter.dev/flutter/rendering/ViewConfiguration-class.html
- Flutter RenderParagraph.devicePixelRatio（DPI 变化重生成文本位图）: https://api.flutter.dev/flutter/rendering/RenderParagraph/devicePixelRatio.html


