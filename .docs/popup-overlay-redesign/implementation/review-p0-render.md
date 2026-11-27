# P0 对抗性审查：渲染 / paint / damage lens（reviewer: p0-review-render）

- 审查对象：worktree `dsh-1d175414-1a41`，HEAD `71a5a5cc` + 未提交 P0 改动（48 文件 modified，1 文件 untracked `crates/ui_core/src/paint/extent.rs`）
- 只读审查：未修改仓库任何文件（`git status --short` 前后一致，唯一新增文件是本报告）
- 结论摘要：**1 条 P0（阻塞，一行修复）、2 条 P1、3 条 P2、4 条 P3**；P0 主干（Canvas 局部坐标 / painted_extent / 结构增量 / 持久像素门禁 / 首帧定位）在逐路径核对后成立，但 Canvas 内容里唯一的 `Primitive::Path` 编码路径把 canvas 偏移加了两次，且现有门禁在结构上不可能看见它。

---

## 1. 审查范围与方法

### 1.1 读过的文件

设计与证据：

- `.docs/popup-overlay-redesign-design.md` rev.6 §2.3、§4.11（I1–I6、damage 契约）、§4.12、§5.1、§5.3、§6 P0（P0-0/1/2/3/4/5/6/7/8）、§7.2、§10
- `.docs/popup-overlay-redesign/implementation/p0-summary.md`
- `.docs/popup-overlay-redesign/evidence/p0-diagnosis.md`

实现（完整读 + 与 HEAD 对比）：

- 新增 `crates/ui_core/src/paint/extent.rs`（34 行，全文）
- `paint/{pass,instance,primitive,canvas,scene,geometry}.rs`、`paint.rs`
- `node/tree.rs`、`node/tree/{arena,content,dirty,layout,tests}.rs`、`node/dirty.rs`
- `window/paint.rs`、`window.rs`、`window/{frame,surface,scale}.rs`、`render/{damage,delta,compose,compositor}.rs`、`render/windows/compositor/{record,tests}.rs`
- `test_support/{pixels,record,snapshot,app,diagnostics}.rs`
- `app/{canvas_text,removal_tests,scroll}.rs`、`layout/{hit_test,taffy_tree}.rs`
- `ui_kit/src/kit/{overlay,color_picker,spinner}.rs`、`ui_kit/examples/gallery.rs`
- `widget/src/test_support.rs` 与 `widgets/*` 17 个迁移文件（candlestick、coin_market…、cryptocurrency_etf、global_shipping_chokepoint、hyperliquid_wallet、order_book、perpetual_statistics、prediction_market、volume_heatmap）

### 1.2 跑过的命令（本轮只跑 ui_core / ui_kit）

| 命令 | 结果 |
|---|---|
| `cargo test -p ui_core -p ui_kit`（`CARGO_TARGET_DIR=E:\kytrix\target`） | EXIT=0：ui_core lib 2699 passed / 1 ignored、`tests/frame_record` 8、`tests/layout` 4、`tests/snapshot` 14、`examples/demo` 39、ui_kit 766 passed；`tests/gui_smoke` 1 ignored |
| `cargo run -p ui_kit --example gallery -- --pixel-check` | EXIT=0：`checks=108 sampled_frames=45 damage_full_frames=0 first_visible=(496,91,300×375) stable=(496,91,300×375) moved=(496,4,300×375)` → `pixel-check PASS (108 checks)`（WARP / `Microsoft Basic Render Driver`） |
| `git diff` 全量、`git show HEAD:<path>` 抽查（`hide_subtree`、`paint_state`、`damage.finish`、`place_subtree`、`set_layout_override` 的旧行为） | 用于判断测试/行为是否被放宽 |
| `git status --short` | 审查开始/结束均为同样的 49 行（48 M + 1 `??`），**无本审查引入的改动** |

> 注：中途一次重复跑 `cargo test` 触发 `link.exe ... exit code 1104`（LNK1104：与并行的 `cargo run` 抢 `E:\kytrix\target` 文件锁），重跑即通过；这是环境竞争，不是测试失败。

### 1.3 方法

- 逐图元核对 encoder 的 offset 传播（quad / shadow / image / glyph / clip / path / geometry / 嵌套 canvas / canvas text），以及 clip 与 transform 的叠加顺序、125% DPR 取整。
- 以「lane 局部坐标 = 布局坐标 − bake（bake = total − slot_offset）」为中心，把 `PaintedExtent`（L 空间）的**所有**消费点（rebuild / hide / geometry clear / place_subtree / remove）逐个做 L→屏幕 换算核对，并构造滚动容器内的反例。
- 对 P0-7 的结构增量路径做「谁被标记 → 谁被访问 → 谁被重建 → order/delta/ops 如何跟上」的完整推演，并对照 full paint 语义。
- 对 P0-5 门禁做「能否看见某类缺陷」的结构性分析（Plan 类型、payload、op 裁剪路径、场景选择），而不是只看它是否 PASS。

---

## 2. Findings（按严重度）

### R-01 [P0] `Primitive::Path` 的 op-bounds 把 canvas 偏移加了两次，导致几何 op 在局部帧被裁剪掉（gauge 色带 / K 线折线在 damage 区域不绘制）

**位置**

- 缺陷行：`crates/ui_core/src/paint/instance.rs:1579-1588`（关键 `:1587`）
  ```rust
  Primitive::Path(path) => {
      self.push_geometry_op(
          path.geometry,
          Transform::translate(offset.x, offset.y),      // :1584 顶点平移（正确）
          color,
          FULL_INDEX_RANGE,
          Some(path.bounds.translate(offset)),           // :1587 ← 错：op_bounds 又被平移一次
      );
  }
  ```
- op_bounds 的语义（必须是**变换前**的局部 AABB）：`instance.rs:1793-1807`，`push_part` 里 `visible_aabb(transformed_aabb(transform, bounds), …)`（`:1795`）——即 `bounds` 会先被 `transform` 变换一次；`(None, …)` 分支用 `meta.bounds` / `snapshot.bounds()`（几何局部坐标），`Primitive::Geometry`（`:1590-1600`）也只传 `draw.transform`，op_bounds 为 `None`。全仓一致约定 = op_bounds 变换前局部坐标。
- 消费点（op bounds 只用于裁剪）：`crates/ui_core/src/render/windows/compositor/record.rs:630-670`
  ```rust
  fn scrolled_bounds(op: FrameOp, scroll: &[[f32; 2]]) -> DeviceRect { op.bounds().translate(...) }
  ...
  if !scrolled_bounds(*op, payload.scroll()).overlaps_inclusive(scissor_bounds) { continue; } // :668
  ```
  `FrameOp::Geometry.bounds` 来自 `append_segment_ops`（`instance.rs:1364-1412`），即 `bounds_of_aabb(visible_aabb(transformed_aabb(transform, path.bounds+offset)))`。`FrameOp::Geometry` 没有 scroll 字段（`FrameOp::scroll()` 恒返回 0，`instance.rs:191-196`），所以 `scrolled_bounds` 就是 op bounds 本身，错位不会被 scroll 平移抵消。linux/macOS 组装的同类 op 裁剪使用同样的 `op.bounds().translate(...)`（`render/linux/compositor.rs:1764`、`render/macos/compositor.rs:914`）。

**证据（代码 + 算术反例）**

- P0-4 之后 `Path.bounds` 是 **canvas 局部坐标**：`widgets/coin_market_cap_fear_and_greed_index/src/gauge.rs:329-332` 先 `LogicalRect::from_origin_size(ZERO, size)` 再 `GaugeGeometry::compute(bounds)`，`:352-356` 用 `mesh.bounds` push；`widgets/candlestick/src/chart/paint.rs:1750-1763` 同样把 `mesh.bounds` push 到局部画布。两者都是生产路径的真实 `push_path` 使用者（全仓仅此两处）。
- 设 canvas 节点内容原点（`CanvasDraw.offset`）`o=(100,50)`，path 局部 bounds `B=(0,0,20,10)`：
  - 顶点变换：`translate(o)` → 屏幕 (100,50)-(120,60) ✓
  - op bounds：`transformed_aabb(translate(o), B.translate(o))` = `B + 2o` = (200,100)-(220,110) ✗（偏移误差 = `o`，125% 下再乘 1.25）
- 后果链：`FrameOp::Geometry.bounds` 被平移 `+o`；`draw_ops` 用 damage scissor 与 op bounds 求交，**不相交就整条 op 跳过**。因此当“真实几何”落在某个 damage 矩形内、而“平移后的 bounds”落在外面时，该几何在**这一帧的 damage 区域里不会重绘**：持久目标上被 damage scissor 覆盖的像素先被背景 op 重画 → path 所在的像素被抹成背景（gauge 色带缺口、K 线折线在 hover/tick 的局部 damage 带内消失），直到下一次更大范围/整帧重绘才恢复。
- 更糟的分支：若 canvas 自己 push 了 clip（gauge 与 candlestick 都 `push_clip` 到画布框，`gauge.rs:340-346`、`candlestick chart/paint.rs:1757-1760`），`visible_aabb` 会与 clip AABB 求交；当 `B+2o` 完全落在 clip 外时交集为空 → `bounds_of_aabb` 返回 `DeviceRect::default()`（0,0,0,0，`instance.rs:245-259`）→ op bounds 退化为零矩形 → **任何不在 (0,0) 附近的 damage scissor 都不会绘制该 path**。
- 该缺陷对现有门禁**不可见**（见 R-03）：无 encoder 测试在 offset≠0 的 canvas 里放 Path；widget 快照/像素测试一律 `Plan::Full`（`test_support/pixels.rs:36`、`render_offscreen`），整帧 scissor 下 op bounds 一定与 scissor 相交，不会触发裁剪。

**影响**：P0-4 契约（I3：`Canvas::bounds()` 与节点布局/滚动/烘焙一致）在“几何 op 的可见范围”这一环被破坏；对含 Path 的图表是**肉眼可见的渲染回归**（仅出现在局部重绘帧）。

**建议修法**

```rust
Some(path.bounds)   // op_bounds 是变换前局部 AABB；顶点位置已由 transform 平移
```
（`Path` 没有自己的用户 transform，所以 `Transform::translate(offset)` 保留）

**配套测试（建议直接进 P0）**

1. encoder 单测：`CanvasDraw { offset: (100,50) }` 包一个 `Path { bounds: (0,0,20,10) }`，断言 `FrameOp::Geometry.bounds` == `to_covering_device_rect((100,50,20,10))`（当前会得到 `(200,100,…)`）。
2. 像素级：`PersistentPixels` 场景里放一个 push_path 的 canvas，做一次只覆盖 path 区域的局部 damage 帧，与整帧基线比对（当前会失败）。
3. 同上，加 clip（canvas 内 push_clip），覆盖“交集为空 → 零矩形”的分支。

**是否阻塞 P0 提交**：**阻塞**。一行改动，但属于 P0-4 契约的直接回归，且 P0 验收里“chart 测试绿”无法证明它正确。

---

### R-02 [P1] 删除“滚动容器的直接子节点”时，painted_extent damage 在 L→屏幕 换算中丢失该容器自身的 scroll offset（offset≠0 时 damage 错位）

**位置**

- `crates/ui_core/src/node/tree/arena.rs:400-417`：`remove()` 把子树 extent（L 空间）并进**直接父节点**的 `damage`：
  ```rust
  let painted = tree.painted_extent(*id).map_or_else(LogicalRect::default, |e| e.bounds());
  subtree_damage = subtree_damage.union(target.damage).union(target.layout.bounds).union(painted);
  ...
  p.damage = p.damage.union(subtree_damage);          // :416，L 空间
  ```
- 消费点只按“该节点自己的 total”换算：`paint/pass.rs:1621-1628` `emit_node_damage` 用 `self.node_damage.translate(negate(self.offset))`，而 `offset` 是 `paint_node` 传入的 `total`（祖先滚动量，`paint/pass.rs:455-471` 只把自身 `own` 加到 **children** 的 `child_total`；`content.rs:700-719` `paint_state(screen_offset=total, bake_offset=total-slot_offset)` 同理）。
- 对照：`hide_subtree` 用的是被隐藏节点**自己的** total（`paint/pass.rs:552-628`，子节点递归时 `child_offset = total + scroll_slot(node).offset`，`:577-583`）→ 正确；`rebuild_lane`/`clear_geometry_lanes`/`place_subtree` 也都是“自己节点的 total” → 正确。

**证据（数值反例，基于现有测试 fixture 的坐标约定）**

- 设滚动容器 V 屏幕/L 盒 (100,80) 240×200，内容 1200 高，`scroll offset=(0,300)`；其直接子行 R 的 `layout_override=(20,320)` → 绝对 L = (120,400)-(220,440)。
- R 的 extent（L 空间，`extent_shift = total - slot_offset = (0,300)-(0,300)=0` → 就是 L 坐标）= (120,400)-(220,440)。
- 删除 R：`V.damage ∪= (120,400)-(220,440)`；下一帧 `paint_node(V, total=(0,0))` → `emit_node_damage` 平移 `-total = 0` → **屏幕 damage = (120,400)-(220,440)**。
- 正确值应为 `L - total(V) - own(V) = (120,100)-(220,140)`。误差 = `own(V) = (0,300)`：真正旧像素所在的 (120,100)-(220,140) **没有被 damage 覆盖**（持久目标上会残留），而多出来的 (120,400)-(220,440) 在视口外（V 盒 80..280）→ 无意义重绘。
- 该错误 damage 不会被 `clip.covering` 裁掉：`rebuild_lane` 里 V 的 emitter `clip` 是**祖先**的 clip（V 自己的 clip 是在 lane 重建之后才 push 给子节点的，`paint/pass.rs:449-453`），所以这条错位矩形会真的进入 damage 集合。

**影响范围（比摘要偏差 2 的描述更窄，也更具体）**

- 多数情况下被掩盖：V 自己被 `mark_structure` 标脏并重建 lane，只要 V 有**不透明背景/边框/渐变**，`emit_decorations` 会 push `V.bounds`（屏幕坐标）作为 damage，直接覆盖整个视口 → 旧行区域被顺带修好。
- 真正暴露的条件：滚动容器本身**不绘制任何内容**（纯 `overflow_hidden` 的裁剪壳、无 PaintSpec——P0 新像素测试里的 viewport 就是这种，`test_support/pixels.rs:949-963`）。此时 V 的 lane 无装饰 damage，唯一通道就是上面这条错位的 extent → 残留。
- 与既有测试的错位：`app/removal_tests.rs:1596-1625` 有“滚动容器内删除”的测试，但滚动 offset = 0；`test_support/pixels.rs:942-1037` 有 offset=300 的滚动测试，但只做**移动**（移动的 damage 记在被移动节点自己的 `damage` 上，换算正确）。**“删除 × offset≠0”这个组合完全没有测试**。

**建议修法**

- 方案 A（最小）：`remove()` 记录被删子树的“相对 L 空间”并按“父的 child total”换算——即给 `remove()` 传入/存储 `total(parent children)`，或把 extent 分两步：子树 extent 先平移 `-own(parent)` 再并入父 damage。
- 方案 B（根治，符合设计 §4.11 原文只写 `painted_extent: Option<(DeviceRect, PhysicalPx)>`）：extent 在 **emit 时**就记成屏幕坐标（DeviceRect），而不是 L 空间 + 事后平移；这样 remove 路径不需要任何 total。
- 测试：在 `app/removal_tests.rs` 增加「scroll offset=(0,300) + 透明 viewport + 删除直接子行」，断言 damage 覆盖该行的**屏幕** extent（当前会失败）。

**是否阻塞 P0 提交**：不阻塞（摘要已登记为已知偏差 2），但 P0 必须补上这条失败测试（可标 `#[ignore]` 或直接修复），并把偏差 2 的描述更新为“仅透明滚动容器可观测 + `remove` 路径特有”，供 P1 Select/Dropdown/虚拟列表行回收时优先处理。

---

### R-03 [P1] 新门禁看不见“canvas 内非 quad 内容”的偏移错误：持久像素场景只有 ColorPicker（纯 quad），encoder 偏移测试只有 quad/嵌套 canvas，widget 像素测试一律 `Plan::Full`

**位置**

- `ui_kit/examples/gallery.rs:3250-3350`：`pixel_check_scene` 只构造 `ColorPicker`（palette / track / well 全是 Canvas+Quad），无 Path / Geometry / GlyphRun / Image。
- `crates/ui_core/src/test_support/pixels.rs:574-1037`：5 条持久增量测试的场景全部是 `Quad`/`Shadow`/`Canvas+Quad`（`mount_shadowed_canvas_subtree` 里 canvas 只有一个 quad，`:820-828`）。
- encoder 侧：`paint/instance.rs` 里所有 `CanvasDraw::new(...)`（offset=0）——`:2367`、`:2412`、`:3065`；唯一带非零 offset 的编码断言在 pass 层（`paint/pass.rs:2774-2803`、`:2954-2983`），且只含 quad。
- widget 像素/快照通路：`test_support::pixels::render_offscreen` 固定 `Plan::Full`（`pixels.rs:36`），`op bounds` 裁剪分支（`record.rs:668`）因此永远不生效；`crates/widget/src/test_support.rs` 的断言是模型级（`CanvasPrimitive.bounds`），不是像素。

**证据**：R-01 的缺陷在上述三条通路里都不可见；P0 摘要的验收③（“chart/spinner/取色器测试绿”）在实现上等价于“模型/几何断言绿 + 整帧像素绿”，不能证明 §I3 在局部重绘帧成立。本次审查用代码推演发现 R-01，正是这条门禁缺口的具体后果。

**影响**：P0 声称“持久增量像素门禁”是可防回归的唯一门禁（p0-diagnosis §5.4 第 4 条），但它对 canvas 内容类型有结构性盲区；P1 的 PopupPanel 内容（图表弹层）会立刻踩到。

**建议修法**：在 `test_support/pixels.rs` 增加一条用例——“局部 damage 帧下 canvas 含 path/geometry/glyph/image 时，持久目标与整帧基线逐像素一致”，并让 gallery `--pixel-check` 至少包含一个带 Path 的 canvas（例如把 spinner 改成 path，或直接构造 gauge 片段）。

**是否阻塞 P0 提交**：不阻塞（但它把 R-01 从“可见失败”变成“静默回归”，建议与 R-01 一起修）。

---

### R-04 [P2] I2 的 ops 一致性测试只覆盖“释放中间 lane”一种形态；`LaneStore::apply` 对“generation 不匹配的移除通知”有一个静默丢段的脆弱点

**位置**

- 新测试：`crates/ui_core/src/render/compose.rs:619-662`（释放 `order[1]`，断言 `fast_encodes()==0`、`ops` 总长=2）。
- 未覆盖形态：
  1. 同一 delta 内“释放 lane（slot s, gen g）+ 分配新 lane（slot s, gen g+1）”——真正考验“range 被复用后旧 op 残留”的场景；
  2. paint 内 `scene.release_lane`（primitives==0 / clear_geometry_lanes 的 lane）后 slot 被复用，而 `delta.removed` **不包含**该 lane（只靠 order/version 变化收敛）的路径。
- 推演结论：这两种形态当前**是正确的**（`compose.rs:66-68` 先 `encoder.release_lane`，`delta.rs:378-403` 因 removed/order 把 `fast=false`，`encode` 走 `encode_scanned` 全量重建 `ops`；`instance.rs:847-978` 对 generation 不匹配的 segment 会 `release(range)` 后重编码）。但缺测试锁定。
- 脆弱点：`delta.rs:394-403`
  ```rust
  if let Some(stored) = self.lanes.get_mut(slot).and_then(Option::take)
      && stored.generation == removed.generation()
  { changed = true; self.fast = false; self.order.retain(...); }
  ```
  `take()` 在 generation 不匹配时也会把**当前存活 lane** 从 LaneStore 摘掉，且不触发重编码。经推演，在当前调用纪律下（removal note 与 lane 释放同帧产生；未发布的 pending delta 不会让 slot 先行被存储）“不匹配”不可达；但这是一个“静默丢数据”的形状，未来一旦 delta 合并/发布时序变化就会变成“lane 突然消失”。

**建议**：补 2 条 compose 测试（上述形态 1、2）；把 `take()` 改成仅在 generation 匹配时 `take`，否则保留并存断言（或 `debug_assert`）。不阻塞 P0。

---

### R-05 [P2] `Overlay::open` 不再在打开帧 reveal：面板要等下一次 `sync()/place()` 才可见；p0-summary 的“先 place 后 reveal”与代码不符，且 API 契约变成“必须逐帧 sync”

**位置**

- `crates/ui_kit/src/kit/overlay.rs:804-812`：`open()` 里顺序是 `start_fade(1.0)` → `sync_scrim` → `layer.enforce` → `measure()`（**不是**“先 place 后 reveal”）。
- `crates/ui_kit/src/kit/overlay.rs:944-949`（`sync` → `place`）与 `:904-914`（`place()` 里 `measured = frame_index != frame_at_open` 才写 `revealed=true`）。
- `crates/ui_kit/src/kit/overlay.rs:1020-1038`：`group` 的 `visible` 绑定 `revealed` → 打开帧整棵浮层树 `visible=false`。

**证据/影响**

- 首可见帧 == 稳定帧成立（`overlay.rs:1654-1683` 测试 + gallery pixel-check `first_visible == stable`，本轮实测 `(496,91,300×375)` 两次相同），P0-8 目标达成。
- 但代价与契约变化：(a) 打开后至少 1 帧不可见（16ms 级延迟，gallery 无法区分，因为它只比较“revealed 之后”）; (b) **只调 `open()` 不逐帧调 `sync()` 的消费者将永远看不到面板**（旧行为是“打开即淡入，只是位置错一帧”）。仓内 5 个消费者都调 `sync()`（color_picker/dialog/menu/tooltip，grep 见 `color_picker.rs:969` 等），所以当前不炸；但这是一个未写在摘要里的行为契约变更，建议至少在摘要“已知偏差”里加一条，并加一条“open 后不 sync 的帧不产生可见像素”的行为测试，防止未来组件忘记 sync 时无提示失效。

**建议**：摘要把 P0-8 描述改为“面板在测量完成后的下一帧 reveal（open 帧隐藏）”；或把 reveal 下沉到 `open()` 内部的 `measure()`（measure 已能拿到实测位置，可在同一 update 内 place 后立即 reveal），从而真正实现“先 place 后 reveal”。不阻塞 P0。

---

### R-06 [P2] 结构增量路径的 `damage_full` 局部性在“独立 Geometry 节点”上仍会被升格为整帧（I6 的适用边界未写明）

**位置**：`node/tree/arena.rs:386-392`（删除含 `NodeKind::Geometry` 的子树 → `geometry_damage_escalation=true`）、`node/tree/layout.rs:276-283`（Geometry 节点移动同样升格）、`window/paint.rs:41-43`（`take_geometry_damage_escalation()` → `full_damage_pending=true` → `damage.push_full()`）。

**证据/影响**：P0-7 的目标是“非模态 popup 开关不 `damage_full`”（I6/C8）。当前 ColorPicker 的 palette 是 Canvas+Quad，不含 `NodeKind::Geometry`，所以 gallery 门禁 `damage_full_frames=0` 成立；Canvas 内部 `Primitive::Geometry` 也不触发升格（`remove()` 只检查 `target.kind`）。但任何将来用**独立 Geometry 节点**（例如 HUD/图表浮层）的 popup，开关/移动必然整帧，I6 会失效。这一点 P0 摘要与设计都未写明适用范围。

**建议**：在摘要偏差里补一句“I6 目前仅对不含 `NodeKind::Geometry` 的 popup 成立”，P1 的 popup 内容闭包禁止/或改造 Geometry 升格路径。不阻塞 P0。

---

### R-07 [P3] `damage.finish` 的“按 frame 裁剪后面积”阈值：语义正确但无测试，且设计文档未记录阈值基准

**位置**：`crates/ui_core/src/render/damage.rs:96-117`（`rect.intersect(frame).area()` 求和）。设计 §4.11 / §2.4 只提到“50% damage 升级”（`:402`、`:583`），没有写“按可见面积计”。

**核对结论**：安全。`plan()`（`render/compositor.rs:328-345`）本来就是 `rect.intersect(surface)` 后再裁剪并丢弃空矩形，且 `frame_rect()`（`window/surface.rs:356-363`）与 `plan` 的 surface 完全一致（`dp(0),dp(0),surface.w,surface.h`），所以新阈值判据与最终 Plan 判据对齐，不会把“本应 full”的帧降级成 partial（>50% 可见面积仍然 full；窗外面积不再虚增面积）。现有 `area_threshold_boundary_is_exact`（`damage.rs:722-743`）只覆盖窗内矩形。

**建议**：补 1 条单测（一个越过 frame 边界的矩形：可见面积恰好 50% → partial、50%+1px → full；完全在 frame 外 → 保持 partial 但 `plan()` 后为 Idle），并在设计文档里写清阈值是“帧内可见面积”。不阻塞 P0。

---

### R-08 [P3] 迁移后的 widget 指针→局部坐标换算用 `layout().bounds.origin`（L 空间）而不是屏幕 rect，滚动容器内的 hover/zoom/pan 会整体偏移 scroll offset

**位置**（均为 P0-4b 新引入的换算）

- `crates/widgets/candlestick/src/view.rs:1283-1290` `chart_position()`
- `crates/widgets/cryptocurrency_etf/src/chart/canvas.rs:124-127` `point_at`（`self.origin = bounds.origin`，`:193`）
- `crates/widgets/perpetual_statistics/src/chart/canvas.rs:165-168`（同上，`:233`）
- `crates/widgets/prediction_market/src/chart/canvas.rs:353-355`
- `crates/widgets/volume_heatmap/src/chart.rs:344-349`（`layout.origin = bounds.origin`）
- `crates/widgets/global_shipping_chokepoint/src/map.rs:886-893` `attribution_contains`（`position - self.bounds.origin`，`self.bounds` 为 layout 绝对坐标）

**证据**：引擎的 `layout().bounds` 不受滚动影响（滚动只在 paint 用 `total` 施加，`paint/pass.rs:455-471`）；而 `InputEvent::position` 是窗口/屏幕逻辑坐标。因此“减 layout origin”只在“没有滚动祖先”时等于局部坐标；有滚动祖先时差 `scroll offset`（hover 高亮到错误的 candle、zoom 锚点偏移）。这是**既有类问题**（旧代码直接拿屏幕 pointer 和 layout bounds 比较，同样差 offset），所以不算 P0 回归；但 P0-4b 是引入局部坐标契约的时刻，设计 §5.1 已规划 `Window::node_screen_rect`，应在 P1 一次性改成屏幕 rect（并加“滚动容器内 hover/拖动”测试）。按要求标 P3 + **需复核**（若产品里这些 widget 不会出现在滚动容器内，则可降为纯文档项）。

---

### R-09 [P3] 契约/记录层的坐标空间残留不一致（低风险）

1. `crates/ui_core/src/test_support/record.rs:271`：`Primitive::Path(path) => (PrimitiveKind::Path, path.bounds)` 记的是 **canvas 局部**坐标，而同一函数里 Quad/GlyphRun/Image/Shadow 记的是 **lane（≈屏幕）**坐标（`:262-277`）。目前 lane 层面永远不会出现 `Primitive::Path`（Path 只存在于 `Primitive::Canvas` 内部，`grep` 生产调用点仅 gauge/candlestick 两处），所以不可达；但 `FrameRecord.primitives` 的“bounds 空间”因此没有明确契约，未来若有人按 primitive 记录做 damage 断言会误判。
2. `PaintedExtent` 的 Canvas 分支只取 `canvas.bounds().translate(origin) ∪ node.bounds`（`paint/pass.rs:1698-1699`）：若 canvas 内容**溢出** `Canvas::new(bounds)`（例如 canvas 里 push 了 shadow / 超出 bounds 的 geometry/path），移动/删除该 Canvas 时 extent 会偏小 → 残留。当前 widgets 都把 canvas bounds 设成内容外框，属于“契约未写明”的隐性假设（与摘要偏差 4 同族），建议 P1 明确写成 `Canvas::bounds()` 必须覆盖全部内容，或在 emit 时累加实际图元 AABB。
3. `crates/ui_core/src/test_support/record.rs:221-226` 的 `CanvasRecord.canvas` 现在存的是**局部坐标**画布；仓内所有相关断言已同步（`record.rs:513-516`、`view/tests.rs:1137-1178`、`panel_tests.rs:642-816`、`widget/src/test_support.rs:728-740`），本轮 `cargo test -p ui_core -p ui_kit` 全绿，未发现遗漏；但外部 crate 若基于 `CanvasRecord` 做题会需要同样迁移（`cargo check --workspace --all-targets` 由 lead 覆盖）。

**建议**：给 `PrimitiveRecord.bounds`/`CanvasRecord` 写清空间约定（或把 Path 记录改成经 `CanvasDraw.offset` 平移后的屏幕坐标，与 Canvas 一致）。不阻塞 P0。

---

## 3. 已核实为正确的点（逐项，含测试落点）

### 3.1 encoder 的 canvas 平移（除 R-01 的 Path op-bounds）

- `Primitive::Canvas(CanvasDraw { canvas, offset })`：嵌套 canvas 累积 `offset`（`instance.rs:1602-1613`），`Deref<Target=Canvas>` 不改变 bounds 语义（`primitive.rs:26-48`）；`resolved_canvas` 重建画布时保留 `offset` 并只解析 text（`paint/pass.rs:312-343`）。
- quad：`quad.bounds.translate(offset)` 后一次性 `to_device_rect`（`instance.rs:1873-1874`）→ 125% 取整正确（测试 `canvas_content_snaps_to_physical_pixels_at_125_percent`，`pass.rs:3019-3045`）。
- shadow：`shadow.rect.translate(shadow.offset).translate(offset)`（`instance.rs:1923-1927`）——offset 只加一次，且加在 shadow 自身 offset 之后（正确顺序）。
- image：`image.bounds.translate(offset)`（`instance.rs:1952-1966`）。
- glyph（含 canvas text 解析后的 glyph run）：`glyph.position + offset`（`instance.rs:1985-2007`）；`push_text_glyph_runs` 用 `CanvasText.origin + glyph.position` 得到**画布局部**坐标（`pass.rs:344-374`），测试断言 glyph 仍是局部、canvas 整体只平移一次（`pass.rs:3373-3386`）。
- clip：`push_clip(clip.rect.translate(offset))`（`instance.rs:1619-1640`）——与几何/quad 的平移同源，因此“canvas 内 clip”与“lane 级 clip（canvas 外）”不会互相污染；lane 级 clip 仍按 `slot_offset` 烘焙（`pass.rs:711-722`），clip 与 transform 的叠加顺序不变（先父 clip 后 canvas 局部 clip，且 canvas 内 clip 已平移）。
- geometry：`draw.transform.then(translate(offset))`（`instance.rs:1590-1600`）——`then` 语义为“先 self 后 next”（`math.rs:1091-1105`），因此顺序正确，且 op_bounds 走 `None`→`meta.bounds`（局部）不重复平移。
- 复用的正确性依赖：`paint_state` 对 `screen_baked`（Canvas/Geometry）新增 `baked_screen_bounds == layout.bounds - total` 判据（`content.rs:711-741`），写入点在 `rebuild_lane`（`pass.rs:769-771`）、`paint_geometry_node`（`pass.rs:1216`）、`hide_subtree`（`pass.rs:611-613`）；节点 id 复用时由 `insert_child` 清空（`arena.rs:272-284`）→ 布局位置变化/滚动变化都会重建，不会沿用旧坐标（测试 `a_layout_change_invalidates_a_cached_canvas_lane_on_the_first_frame`、`an_ancestor_scroll_bakes_the_canvas_content_exactly_once`）。

### 3.2 `PaintedExtent`

- 写入覆盖：quad（含渐变/边框伤害、`pass.rs:1550-1589`）、shadow（`push_shadow_damage` 用真实物理 pad，并把 pad 换算成逻辑 outset 并入 extent，`pass.rs:1470-1480`）、focus ring（`pass.rs:1592-1618`）、glyph/装饰/inline box/image（`note_painted` 经 `push_damage`，`pass.rs:1458-1468` 及各 emit_* 调用）、Canvas（`canvas.bounds()+origin ∪ node.bounds`，`pass.rs:1681-1710`）、Geometry（`bounds.translate(total) ∪ chunk AABB ∪ 旧 extent`，`pass.rs:1194-1209`）。
- pad：`PaintedExtent::new` 固定 1 物理 px（AA），`push_damage` 用 `PhysicalPx` 传入 → DPR 无关（`extent.rs:14-33`）；shadow 的 `3σ+1` 物理 pad 经 `to_logical` 外扩后再由 `push_logical` 的 covering 取整 + 1px 兜底 → 不小于原 pad（保守）。DPR 变化走 `set_scale` → `full_damage_pending`（`window/scale.rs:8-17`）→ 全量重绘并重算 extent。
- 生命周期：lane reuse 保留（`rebuild_lane` 只在重建分支 `set_painted_extent`；测试 `lane_reuse_keeps_the_painted_extent_and_hiding_clears_it`）、隐藏/清空并先 damage 旧 extent（`hide_subtree`、`clear_geometry_lanes`）、remove 对子树取并集并清空 side table（`arena.rs:400-436`）。
- L→屏幕 换算：`rebuild_lane`（`pass.rs:650-654`）、`hide_subtree`（`pass.rs:614-616` + 子节点 `child_offset`，`pass.rs:577-583`）、`clear_geometry_lanes`（`pass.rs:894-896`）、`place_subtree`（节点自己的 damage → `emit_node_damage` 用自身 total，`pass.rs:1621-1628`）全部正确；唯一缺口见 R-02（remove 并入**父** damage 且父自身是滚动容器）。

### 3.3 结构变化增量绘制（P0-7）

- `window/paint.rs:81-97`：`force_full` / `structure_changed` / `scroll_registry_changed` / `first_paint` 拆分与 `epoch_full` 组合正确；`full_paint` 在增量分支被真实 `full` 覆盖（`:156-162`），`!full_paint && (structure_changed || !removed.is_empty())` 才做 `rebuild_order_only`（`:166-176`），与 `pass.execute` 内 `order_changed` 分支（`pass.rs:158-166`）幂等。
- `structure_changed` 不再强制全量，但 `note_paint_frame` 每帧推进 `painted_structure_epoch`（`tree.rs:709-718`）：增量帧确实把新增/删除反映到 order（`rebuild_order_only` 从整树 DFS 推导，跳过 `display:none`/`visible=false` 子树，`pass.rs:530-550`，与 full paint 的 DFS 顺序一致）；节点被 `mark_structure` 标脏后由 `mark_paint_closure` 沿父链可达（`tree.rs:743-764`），`expand = !reusable` 保证新子树被 `force` 访问（`pass.rs:472-489`）。
- 隐藏→显示链路：`hide_subtree` 清 `clip_key` + `set_reusable`（`pass.rs:607-613`），使复用判据必然失效 → 再显示时整棵子树重建（不会出现“隐藏时清空 lane、显示时却复用空 lane”）。
- `scroll_registry_changed` 只在滚动槽位表结构变化时降级全量：`note_scroll_container`（`traverse.rs:81-84`）、`append_child` 若子树含滚动容器（`arena.rs:357-364`）、`remove` 若删除集合含滚动容器（`arena.rs:393-399`）；这三处覆盖了槽位表需要重建的全部入口，其余结构变化保持增量。
- 本轮实测：gallery pixel-check `damage_full_frames=0`；`cargo test` 中 `removal_tests` 的 insert/remove/reorder/scroll 用例全部 `!damage_full` 且 extent 被覆盖。

### 3.4 damage 阈值与 Plan 一致性

见 R-07：新的“帧内可见面积”判据与 `plan()` 的 `intersect(surface)` + 丢弃空矩形语义一致；`frame_rect()` 与 surface 同源，不会出现“判据用 A 尺寸、绘制用 B 尺寸”。

### 3.5 position / hit / a11y 与 Canvas 局部坐标

- 引擎 hit-test 仍用屏幕坐标：`layout/hit_test.rs:360` `slot.layout().bounds.translate(negate(offset))`（offset 为滚动量）；Canvas 内容坐标化不影响节点命中（Canvas 是叶子，命中仍走节点盒/hit_shape）。
- `ColorPicker` 的命中换算全部改成 **size + `hit.local_point`**（`color_picker.rs:1101-1157`、`:2189-2199`），与“画布局部坐标”契约自洽（不再需要绝对 origin）；`palette_index_at(size, local)`、`set_channel(local.x/width)` 一致。
- `ui_kit` 面板/内容根加 `overflow_hidden` 后，`Clip::Bounds` 同时进入 `content_clip`（emit 时 `pass.rs:1664-1677`，lane 重建时 `pass.rs:690-694`）和 hit-test 裁剪路径（P0-6 的纵深防御生效）；新增测试断言 `Clip::Bounds` 且布局尺寸未被裁成 0（`color_picker.rs:3354-3390`）。
- a11y：P0 未改 a11y bounds 来源，Canvas 局部坐标不参与 a11y rect（a11y 仍由节点 layout 推导）→ 无新回归面（详细由 a11y lens 复核）。

---

## 4. 残余风险 / 本轮未覆盖

1. **真实窗口像素通路仍未验证**：本轮 `--pixel-check` 是 headless `TestApp` + WARP 离屏 + 持久目标（`test_support/pixels.rs:58-134`），Windows 真窗的 `publish_decision`/`draw_ops`/persist 重用尚未有像素级断言（摘要偏差 1 已登记）。R-01 的可见症状（damage 区域内的缺绘）在 headless 的“逐帧全量 readback + partial plan”中同样会出现，但需要新场景才能触发（见 R-03）。
2. **嵌套滚动 + 虚拟列表槽位重映射**（I4 的完整形态）未在本轮验证：只验证了“注册/移除滚动容器 → 全量降级 + 局部 damage”和单层滚动重放。P0-3 并入 P0-7 后的“集合/顺序变化重映射”没有测试；多滚动容器叠加时 `scroll_slot_for` 的缓存命中条件（`pass.rs:497-528`）值得 P1 专项测试。
3. **`PaintedExtent` 与 clip 的交互**：extent 本身不计 clip（设计如此），但 damage push 时会用**当前**位置的 `clip.covering` 裁剪；跨容器 reparent（旧 clip ≠ 新 clip）时旧位置的 damage 可能被新 clip 裁掉（P0 内不可达：overlay 只在 root 层 reparent），未构造实验。
4. **Canvas 内容溢出 `Canvas::bounds()`**（R-09.2）没有契约测试，也没有运行时校验；widgets 目前靠约定。
5. **`--dump-overlay` 真窗路径未复跑**（lead 已在本轮外跑过 PASS）；本审查未独立验证，仅读取代码确认 `Overlay::measure` 的两次 `compute_layout` 只发生在 open 路径（偏差 8 属实）。
6. **perf**：结构帧 `rebuild_order_only` 为 O(n) DFS（偏差 3 属实，`pass.rs:530-550`）；本审查未做基准对比（P3 阶段性能门禁覆盖）。
7. 与渲染无关但影响“是否可提交”的项（`mark_structure` 去掉父盒 damage 后的其它消费方、a11y/hit 语义）由其他 lens 覆盖；本报告只对其渲染/damage 侧面负责。

---

## 5. 计数与结论

| severity | 数量 | 条目 |
|---|---|---|
| P0 | 1 | R-01 |
| P1 | 2 | R-02、R-03 |
| P2 | 3 | R-04、R-05、R-06 |
| P3 | 4 | R-07、R-08、R-09（含 3 个子项） |

- 最关键的 3 条：**R-01（Path op-bounds 双重平移 → 局部帧缺绘，阻塞）**、**R-02（滚动容器内删除的 damage 错位 → 透明容器残留）**、**R-03（门禁结构性看不见 canvas 内非 quad 内容）**。
- 是否可提交 P0：**修掉 R-01（一行）后可提交**。R-02 建议同批补一条（可 `#[ignore]` 的）失败测试并更新摘要偏差 2 的描述；R-03 建议把“canvas 内 path/geometry 的局部 damage 像素用例”列为 P0 收尾项，否则 P1 的弹层内容会在无门禁的情况下复发同类问题。
- 除 R-01 外，P0 主干（Canvas 局部坐标契约、painted_extent 写入/清理/并集、结构增量与 order/ops 一致性、`damage_full==false` 的 popup 全流程、首可见帧 rect）在代码逐路径核对 + 本轮测试/门禁复核下**成立**；未发现 `damage.finish` 阈值变更导致的错误降级，也未发现 DPR/scroll/hide/reuse 的 extent 生命周期遗漏。
