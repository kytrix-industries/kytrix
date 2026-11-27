# P0 对抗性审查（第 2 轮）：渲染 / paint / damage lens（reviewer: p0-review-render）

- 审查对象：worktree `dsh-1d175414-1a41`，HEAD `71a5a5cc` + 未提交 P0 改动（**57 文件 modified**，1 文件 untracked `crates/ui_core/src/paint/extent.rs`）+ 设计文档 §11 回写
- 只读审查：未修改仓库任何文件；仓库唯一新增文件是本报告（`.docs` 被 `.gitignore` 规则 `.*` 忽略，`git status --short` 与审查开始时逐行一致）
- 结论：**P0 = 0，P1 = 0，P2 = 1（不阻塞，5 行可修或按偏差登记），P3 = 0**；上轮 R-01…R-09 **全部关闭**（R-02/R-06/R-08 带已登记/已复核的残余项）。**本轮可提交 P0**。

---

## 1. 审查范围与方法

### 1.1 读过的文件（相对上轮新增/变化部分）

- 新增代码：`crates/ui_core/src/node/tree/arena.rs`（`remove()` 逐节点滚动差换算）、`crates/ui_core/src/paint/pass.rs`（`canvas_content_extent`、`PaintReport.order_rebuilt`、13 条新测试、`layout_extent_shift` 更名）、`crates/ui_core/src/paint/instance.rs`（Path 修复 + encoder 测试）、`crates/ui_core/src/window/paint.rs`（`order_rebuilt` 短路 + 测试）、`crates/ui_core/src/render/{damage,delta,compose}.rs`（R-07/R-04 修复与测试）、`crates/ui_core/src/test_support/{pixels,record,diagnostics,snapshot}.rs`、`crates/ui_core/src/error.rs`（`PixelHarness`）、`crates/ui_core/src/paint/extent.rs`
- kit：`ui_kit/src/kit/{overlay,color_picker}.rs`、`ui_kit/examples/gallery.rs`（`measure/reveal/place_panel`、`OverlayError` 回滚、`residue_detector_self_check`、`compare_region`、`assert_old_popup_restored`）
- widgets（R-08）：`candlestick/src/view.rs(+tests)`、`cryptocurrency_etf/src/{chart/canvas,view}(+tests)`、`perpetual_statistics/src/{chart/canvas,widget/display/pane}(+pane_tests)`、`volume_heatmap/src/{chart,view}(+tests)`、`prediction_market/src/chart/canvas(+tests)`、`global_shipping_chokepoint/src/{map,view}(+map tests)`
- 设计文档 §10/§11（偏差登记 1–20、最终 API、门禁基线、P1 衔接项）
- 复核用到的既有机制：`layout/hit_test.rs`（`pick`/`is_hittable`/`is_visible_locally`/`local_point`）、`node/tree/traverse.rs`（`for_each_subtree` 与 `note_scanned` 语义）、`window/{frame,surface,scale}.rs`、`render/compositor.rs`（`plan`）、`render/windows/compositor/record.rs`（`draw_ops` op 裁剪）、`math.rs`（`LogicalRect::union` 空矩形语义）

### 1.2 跑过的命令与结果（本轮只跑 ui_core/ui_kit 测试，符合指令）

| 门禁 | 命令 | 结果 |
|---|---|---|
| 单测 | `cargo test -p ui_core -p ui_kit`（`CARGO_TARGET_DIR=E:\kytrix\target`） | **EXIT=0**：ui_core lib **2717 passed / 1 ignored**（2718 用例）、`tests/frame_record` 8、`tests/layout` 4、`tests/snapshot` 14、`examples/demo` 39、ui_kit lib **769 passed**；`tests/gui_smoke` 1 ignored → 合计 **3551 passed / 2 ignored / 0 failed**（比 §11.5 的 3550 多 1，属回写后的测试增量） |
| 像素门禁 | `cargo run -p ui_kit --example gallery -- --pixel-check` | **EXIT=0**：`checks=108 sampled_frames=45 damage_full_frames=0`；`first_visible=(496,91,300×375)` == `stable=(496,91,300×375)`；`moved=(496,4,300×375)`；输出含语义说明与 P1-12 迁移登记；`PASS (108 checks; residue = old-region baseline scan + injected-fault self-check; exit 0)` |
| 真实窗口 | `cargo run -p ui_kit --example gallery -- --dump-overlay --auto-close=9` | **EXIT=0**：`overlay-check PASS (menu=Some((6, 180.0, 127.5125)) picker_open=true picker_children=Some(4))`；`panel.content/footer/palette` rect 落在设计 O1 的最终位置（footer `(44,416 284×30)`、palette cell 网格 `(44,91)` 起 20×20），9 s 自动关闭、事件循环干净退出 |
| 全仓编译 | `cargo check --workspace --all-targets` | **EXIT=0**（`Finished dev profile`） |

> `git status --short` 审查前后一致：57 M + 1 `??`，无本审查引入的改动。（`cargo +nightly clippy/fmt` 不在本轮指令清单内，未复跑；§11.5 已登记 PASS。）

---

## 2. 上轮 finding 复核表（R-01…R-09）

| ID | 上轮严重度 | 本轮状态 | 证据 |
|---|---|---|---|
| **R-01** Path op-bounds 双重平移 | P0 | **fixed** | `paint/instance.rs:1579-1590`：`Some(path.bounds)`（不再 `.translate(offset)`），顶点仍由 `Transform::translate(offset)` 平移一次；op_bounds 语义=变换前局部 AABB（`instance.rs:1801-1808` `transformed_aabb(transform, bounds)` 未变）。新增测试：`paint/instance.rs::path_op_bounds_stay_in_canvas_local_coordinates`（无 clip/有 clip 两分支，断言 `FrameOp::Geometry.bounds == (199,99,329,301)@scale2`，即局部 AABB + 一次偏移）；`paint/pass.rs::a_path_inside_a_canvas_keeps_local_op_bounds_at_125_percent`（125% DPR，无 clip/带 clip，`check(unclipped_id,(11,4,39,21))`、`check(clipped_id,(374,249,401,266))`）；端到端像素：`test_support/pixels.rs::a_path_canvas_on_an_incremental_frame_renders_inside_the_damage`（局部 damage 帧后 path 位置像素 == RED）。本轮 `cargo test` 全绿，含上述 3 条 |
| **R-02** 滚动容器内删除 damage 错位 | P1 | **fixed** | `node/tree/arena.rs:369-432`：删除遍历改为带 `scroll_extras` 的手写 DFS（每个节点记录「相对直接父节点发射空间」的累计 own-scroll = 父 own + 子链 own，`:383-401`），`subtree_damage ∪= paint.translate(-extra)`（`:421-428`）；`remove` 同时清 `baked_screen_bounds`/`painted_extents`（`:452-459`）。语义核验：`N.damage` 的约定是「screen + total(N)」，父级接手时减去 `own(parent) = total(N) − total(parent)`；对「节点自身 L 空间 damage」「子节点删除写入的 node-total 空间 damage」「extent 的 L 空间」「嵌套/级联删除」四种混合成分逐一验算均正确（同一约定下可加性成立）。测试：`app/removal_tests.rs::removing_a_scrolled_container_child_damages_its_screen_position`、`..._of_a_nested_scroll_container_...`、`a_virtual_list_keeps_its_scroll_table_consistent_across_item_churn_and_popups`；像素级：`test_support/pixels.rs::removing_a_direct_child_of_a_scrolled_container_damages_its_screen_position`（offset=300、透明 viewport，断言 damage 覆盖屏幕 `(120,100,100×40)` 且该点回到背景）。残余：跨滚动上下文 `append_child` 重挂未换算——已在 §11.3-4 登记并转入 P1（本轮复核该登记文字准确） |
| **R-03** 门禁看不见 canvas 内非 quad 内容 | P1 | **fixed** | 端到端：`pixels.rs::a_path_canvas_on_an_incremental_frame_renders_inside_the_damage`（几何 op 在 damage scissor 内必须像素可见）；门禁自证非空洞：`gallery.rs::residue_detector_self_check`（向基线注入一个品红像素，检测器必须报 FAIL，结果计入 `residue-detector` 断言）＋ `assert_old_popup_restored` 改成「旧浮层区域 − 新 popup − 新旧 trigger」全区域逐像素扫描并断言 `compared > 0`、`mismatched == 0`。残余（P3 级）：`Primitive::Geometry`（带非单位 `draw.transform`）在 canvas 内的局部 damage 像素用例仍无（Path 已覆盖同一 op 类型/同一裁剪分支，风险很低） |
| **R-04** LaneStore/ops 一致性测试缺口 + 静默丢段形状 | P2 | **fixed** | `render/delta.rs:394-410`：仅 generation 匹配时 `take` 并置 `changed/fast=false`，不匹配时 `continue`（不再摘掉存活 lane）；`note_removed` 只在与缓存 generation 一致时重置 `versions`（`:339-347`）。测试 3 条：`compose.rs::releasing_a_lane_rebuilds_the_ops_without_the_freed_range`、`a_removal_note_with_a_stale_generation_keeps_the_live_lane`（断言存活 lane 的实例段被复用、ops 总长 2）、`reordering_lanes_without_removal_rebuilds_the_ops`（仅顺序变化 → 全量 ops 重建但实例段复用 3、ops 分组 `[(18,26),(-2,16)]`）。§11.3-15 登记落点变更 |
| **R-05** `Overlay::open` reveal 语义/契约 | P2 | **fixed** | `ui_kit/src/kit/overlay.rs:800-809`：`layer.enforce` → `measure()`（`:811-826`：`refresh_text` + `compute_layout` + `place_panel`）→ `reveal()`（`:828-837`）→ `start_fade(1.0)`；`place()` 拆为 `place_panel()` 不再承担首次 reveal（`:844-891`），`frame_at_open` 字段删除。测试：`the_first_visible_frame_already_has_the_settled_rect`（open 后**不调 sync** 即断言 `revealed` 且首帧 rect==稳定帧）、`open_reveals_and_paints_the_panel_without_a_host_sync`（断言第一帧 scene 里真有 panel 背景 Quad）、`open_with_a_same_frame_or_next_frame_sync_still_paints_on_the_first_frame`、`text_dominated_content_measures_before_the_first_visible_frame`（文本主导高度）；`a_fade_less_panel_is_visible_and_opaque_right_after_open`。§11.3-3 已把「open 内完成 measure+place+reveal」写进偏差/API 变更 |
| **R-06** I6 的 Geometry 边界未登记 | P2 | **fixed（按登记+测试关闭）** | 行为未改（仍 `geometry_damage_escalation` → `damage_full`，引擎既有兜底）：`node/tree/arena.rs:406-412`、`node/tree/layout.rs:276-283`、`window/paint.rs:41-43` 保持；新增测试 `app/removal_tests.rs::standalone_geometry_structural_changes_escalate_to_full_damage` 把边界锁成可执行事实；§11.3-14 明确「I6 只对内容闭包不含独立 Geometry 节点的 popup 成立，P1 禁止或统一」。满足本轮要求（登记+测试） |
| **R-07** damage 阈值按帧裁剪无测试/文档 | P3 | **fixed** | `render/damage.rs:96-117` 逻辑不变；新增测试 `the_area_threshold_counts_only_the_visible_part_of_a_rect`（半个矩形在帧外=可见 50% → Partial、50%+1px → Full、完全在帧外 → Partial 且 `plan()==Idle`）；§11.3-5 已登记口径 |
| **R-08** widgets 指针→局部坐标用 L 空间 origin | P3（需复核） | **fixed** | 统一改走既有 `NodeTree::hit_test(point) -> HitResult { node, local_point }`：candlestick `view.rs:1274-1278 chart_local`、crypto_etf `view.rs:…hit_test().filter(hit.node==node).local_point`、perpetual `pane.rs`、volume_heatmap `view.rs`、prediction_market `chart/canvas.rs:352-356`、map `map.rs note_pointer/local()`（`screen_origin` 由命中反算）。本地复核 `layout/hit_test.rs:350-449`：`pick` 按绘制序取最上层可命中节点，`bounds` 已按祖先滚动平移（`:360`），`local_point = point - bounds.origin`（`:426`），`is_hittable = interactive != Some(false)`、`is_visible_locally` 跳过隐藏/display:none —— 换算坐标系统正确。每 crate 新增「滚动容器内 hover/zoom/pan」用例（如 candlestick `a_pointer_in_a_scrolled_ancestor_should_still_hit_the_candle_under_it`：offset=160，先断言窗口坐标≠图表局部坐标，再断言 cursor 落到相同局部点；map `a_pointer_in_a_scrolled_ancestor_should_still_hit_the_vessel_under_it`）；lead 已补跑 6 crate **1216 passed / 0 failed**（§11.5/§11.6）。§11.3-12 登记了修法与“补跑时发现的是测试装置 flex-shrink 问题、断言未放宽” |
| **R-09** 记录层坐标空间/Canvas 外延 | P3 | **fixed** | ①`test_support/record.rs:254-315` 新增 `record_primitives` 递归：Canvas 内部图元按 `CanvasDraw.offset` 累积平移到统一 lane 空间（Path 也平移），嵌套继续累加；`FrameRecord.canvases/quads` 保持「节点自有」语义；测试 `canvas_inner_primitives_are_recorded_in_lane_space` 断言 Path 记录为 `(105,56,10×8)`（节点原点+局部）而非局部 `(5,6,…)`；§11.3-20 写清契约。②Canvas 内容外延：`pass.rs::canvas_content_extent`（`:1444-1495`）递归累计 quad/glyph/image/path/shadow 物理 pad/Geometry transform AABB/嵌套 canvas offset，用于 `painted_extent` 与 damage；测试 `a_canvas_with_content_outside_its_declared_bounds_damages_the_actual_content`。③`test_support/diagnostics.rs` DamageOverlay Canvas 改局部坐标并加断言（`:405-421`、`:796-812`）；`SnapshotImage` Debug 改摘要（避免整幅像素缓冲进测试输出）。④额外：`node/tree/tests.rs::remove_clears_the_extent_and_baked_screen_bounds_side_tables` 补上了「remove 清 `baked_screen_bounds`」的上轮遗漏（`arena.rs:449-457`）

**小结**：上轮 9 条全部关闭。R-01/R-04/R-05/R-07/R-09 为代码+测试级修复；R-02 为代码+引擎/像素两级测试修复（残余跨滚动上下文重挂已登记转 P1）；R-06 为「测试锁定 + 偏差登记」；R-08 为统一走 `hit_test` 并逐 crate 补滚动容器用例。

---

## 3. 新 findings（对修复引入的新代码逐面复查）

### 复查过的面（结论：未发现正确性问题）

1. **`arena::remove` 逐节点滚动差换算**：见上表 R-02。额外核对（a）被删节点自身是滚动容器时：自身 paint 只用 `extra`（不含 own），子节点才 `+own(current)`（`arena.rs:397-404`）✓；（b）级联删除（先删子、后删父）在同一 update 内：`N.damage` 约定 = `screen + total(N)` 逐级可加，验算一致 ✓；（c）隐藏子树/geometry 节点：extent 与 layout 同为 L 空间，同样 `-extra` 正确 ✓；（d）`block` 顺序与原 `for_each_subtree` 完全一致（pre-order DFS，含 `#[cfg(feature="test-support")] note_scanned` 的对齐，`traverse.rs:25-36`）✓；（e）复杂度仍 O(子树) ✓。
2. **`instance.rs` Path 修复**：见 R-01；嵌套画布/带 clip 画布/125% DPR 三面均有测试 ✓。
3. **`window/paint.rs` `order_rebuilt` 短路**：`pass.rs` 仅在 incremental 分支真正执行 `rebuild_order_only` 时置 `true`（`PaintReport.order_rebuilt`，`pass.rs:158-176`），full 分支与 early `Some(damage)` 分支为 `false`（`window/paint.rs:58`、`:105`）；window 侧条件 `!full_paint && !report.order_rebuilt && (structure_changed || !removed.is_empty())`（`window/paint.rs:168-178`）覆盖「pass 已重建 → 不重复」「pass 未重建（纯删除/无 lane 变化）→ 补一次」「full paint → 跳过」三态；新测试 `a_structural_frame_rebuilds_the_order_once` 断言 insert/remove 各恰好 +1、全量帧 +0 ✓。
4. **`record_primitives` 递归**：递归仅沿 `Primitive::Canvas`，扁平化输出与 encoder 同一 offset 累加规则；`quads`/`canvases` 保持顶层语义（测试断言 `quads.len()==1`）；递归深度=画布嵌套深度（与 encoder/`resolved_canvas`/`canvas_content_extent` 同阶，安全 Rust 下无法构造环）✓。
5. **R-08 用 `hit_test` 替换后的行为差异**：坐标系统正确（见 R-08）；遮挡语义由「几何包含」变为「绘制序最上层可命中节点」——**这是有意的更正确语义**（弹层/兄弟节点覆盖时不再误触发图表 hover），且 6 crate 1216 测试 + 新增滚动用例通过；仓内这 6 个 widget 的图表画布都是叶子节点、标签在画布内绘制，无「兄弟节点盖住画布」的真实场景。列为残余风险（§4.2），不作为 finding。
6. **`Overlay` open 路径**：`measure` 失败 → `close_now` + 返回 `Err`（`:802-805`），`ColorPicker::open_panel` 在 build/measure 失败时同样 `close_now` + 强类型 `OverlayError`（`color_picker.rs:1314-1345`）；`open_panel` 在 `panel_node()` 缺失时现在也会关闭回滚（上轮是静默 return）✓。
7. **`pass.rs` 其余改动**：`extent_shift → layout_extent_shift` 更名、`HideSubtree`/`clear_geometry_lanes`/`rebuild_lane` 的 L→屏幕换算与上轮一致且正确；geometry extent 仍用「chunk AABB ∪ 节点盒 ∪ `painted` ∪ 未重建时的旧 extent」✓；`paint_geometry_node` 的 `set_baked_screen_bounds` 三处写入点完整 ✓。
8. **`error.rs` `PixelHarness`**：新的强类型错误替代 `AppInit` 字符串，`Display` 覆盖 4 种 cause 并有测试；仅 `test_support/pixels.rs` 构造（生产构建下不可达但作为公共枚举变体不影响行为）✓。

### R2-01 [P2] `canvas_content_extent` 对「带缩放变换的 ranged Geometry」用整份 snapshot AABB，把地图类画布的绘制外延放大到 `zoom × 画布盒`，`damage_full` 不再局部（性能/局部性回归，非正确性）

**位置**

- `crates/ui_core/src/paint/pass.rs:1444-1495` `canvas_content_extent`，Geometry 分支只在 `:1473-1485`：
  ```rust
  Primitive::Geometry(draw) => {
      if let Some(snapshot) = geometry.snapshot_arc(draw.id) {
          let aabb = transformed_chunk_aabb(draw.transform, snapshot.bounds());   // ← 整份快照，忽略 draw.range
          ...
      }
  }
  ```
  对比 node 级 geometry 的正确做法：`paint_geometry_node` 用「每 chunk 的 AABB」（`geometry_chunk_local_bounds`，`pass.rs:859-870`）而不是 `snapshot.bounds()`。
- 该 extent 同时作为 **damage**：`:1759-1773`（`Content::Canvas` 每次重建 `push_logical(extent, …)`）与 `rebuild_lane` 的 `previous_extent.push_damage`（`:657-660`）。
- 消费链：`DamageAccumulator::finish` 按可见面积判 `>50%` → `damage_full`（`render/damage.rs:96-117`）。

**证据（数值推导，基于仓内常量）**

- 生产者：`crates/widgets/global_shipping_chokepoint/src/map.rs:398-410` 对每次重绘 push **多个 ranged** `GeometryDraw { id: land.id, transform: land_transform(projection), range }`；`land_transform` = `scale(px/merc).then(translate(zero))`（`:940-945`），`snapshot.bounds()` 是该 tier 陆地网格的**整个区域**（注册时 `GeometryDesc.bounds = mesh.bounds`，`:964-971`；仓内测试 `paint.rs::the_painted_geometry_should_be_offset_by_the_canvas_box`（`:576-603`）明确断言「land mesh 的 box 是 Mercator radians，不是窗口像素」）。
- `fit_px_per_merc(viewport)`（`model.rs:326-334`）保证 zoom=1 时整个区域恰好 fit 画布盒（长边贴边、短边按 region 纵横比 1.396 略溢出；`MAP_WEST/EAST/SOUTH/NORTH` `model.rs:15-18`）。因此
  `canvas_content_extent(map_canvas) ≈ zoom × region_fit_box = zoom × (≥画布盒)`。
- 画布盒为 pane（例如 580×400 = 232k 逻辑 px²），窗口 1200×800 = 960k：zoom=1 时 extent≈250k ≈ 26% 帧面积（partial，正常）；**zoom ≥ √(0.5×960/250) ≈ 1.39 时 extent 的可见面积 >50% → `finish()` 直接 `become_full()`**。地图的 zoom 档位是 1.0/1.6/2.8/4.0（`model.rs:127-128`、§11 表格），**1.6× 起每次地图重绘（hover/pan/zoom/数据 tick，均会 `set_canvas` → Canvas lane 重建）都变成整帧重绘**。
- 前置条件（决定严重度）：该 damage 会被 `self.clip`（=祖先 `Clip::Bounds` 交集，`pass.rs:1759` / `extent.push_damage(..., clip.covering)`）裁剪。gate 的 map 画布节点自身与 widget 根都没有 `overflow_hidden`（`map.rs:263-270`、`view.rs:225-234`），widget 测试装置同样无祖先 clip；真实 dashboard 若宿主卡片设了 `overflow_hidden`，damage 会被裁到卡片盒而不触发 full（**是否触发取决于宿主是否裁剪**）。
- 这不是正确性问题（over-damage 只会多画，不会漏画），但相对 round-1 的 `extent = canvas.bounds() ∪ 节点盒` 是**新引入的局部性回归**：它把地图从「局部重绘」变成「整窗重绘」。§11.3-13 只登记了「PaintedExtent 不计 clip（保守外扩）」，§11.3-17 只写了「Geometry 经 draw.transform 的 AABB」——**均未覆盖“整份 snapshot 而非 drawn range”这一层放大**。

**影响**：chokepoint 地图在 1.6× 及以上缩放时，每次重绘退化为 `damage_full`（整窗重绘）；`PaintedExtent` 也同步虚高，导致地图节点的移动/删除 damage 同样是整帧量级。对 dashboard 帧预算不利，且与 P0-7「结构/内容变化不再整帧」的方向相反（该 widget 不受 I6 验收覆盖，因此门禁看不出来）。

**建议修法（择一，均小于 10 行）**

1. **推荐**：让 `canvas_content_extent` 维护「画布内 clip 栈」，对每个图元的外延先与当前 clip 区域求交再并集（`Primitive::Clip(Push)` 入栈、`Pop` 出栈；map 在内容开始就 `push_clip` 到画布盒，`map.rs:385-389`，因此 extent 会收紧到画布盒，彻底消除放大；对其它自裁剪图表同样收紧，且语义更贴近「实际绘制面积」）。
2. 或最小改：Geometry 分支使用 `chunk_draw_range(snapshot, draw.range)` 命中的 chunk 的 `bounds`（与 node 级 geometry 一致），把「整区域」缩到「本帧实际绘制的 visible chunks」。
3. 若暂不修：在 §11.3 追加一条偏差登记（「ranged Geometry 的 extent 用整份快照 AABB，带缩放变换的画布在 zoom≥~1.4 会整帧；P1 统一 geometry damage 坐标语义时解决」），并在地图 widget 补一条 `CanvasHarness` 用例断言 1.6× hover 帧 `!record.damage_full`（当前会失败）。

**是否阻塞 P0 提交**：**不阻塞**（无正确性影响、有明确降级路径与登记选项）。但在「P0/P1/P2 清零」的口径下建议本轮修掉方案 1（改动小、可测：给 `canvas_content_extent` 加 clip 栈后，地图 hover 帧 `damage_full` 应回落到 partial，ui_core 侧的 `a_canvas_with_content_outside_its_declared_bounds_*` 及 pixels 用例可继续通过）。

---

## 4. 残余风险 / 未覆盖（不构成本轮 finding）

1. **跨滚动上下文 `append_child` 重挂**：`remove()` 已按父 own-scroll 换算，但 `append_child` 在 A/B 两个滚动容器之间搬子树时，旧 `damage`/extent 会用新上下文 `total` 发射（差新旧 total）。§11.3-4 已登记，当前生产调用方都不在滚动容器内、P0 用户不可见；P1 列表复用/行回收前必须补同样换算。
2. **R-08 遮挡语义变化**：指针换算从「几何包含」改为「`hit_test` 最上层可命中节点」。当有可命中的节点（`interactive != Some(false)`）绘制在图表之上时，图表 hover/drag 会被清除（更正确）；反之若图表被**非** `hit.node == 图表` 的兄弟节点覆盖会失去交互。6 crate 的图表画布均为叶子、标签在画布内绘制，无此场景；但「被弹层覆盖时 hover 清除」这一行为变化未单独登记/测试，P1 迁移 Dropdown/Select 弹层时应复核。
3. **`map::note_pointer` 的 origin 陈旧**：`hit.node != 自己` 时不更新 `screen_origin`（`map.rs:759-772`），此后 `contains()` 仍用旧 origin；若事件路由把滚动/缩放发给该 widget 而指针实际在浮层上，可能误消费。属「几何 contains 不知道遮挡」的既有类问题，非本轮回归。
4. **`canvas_content_extent` 的 Path/Geometry 外延依赖作者声明**：`Primitive::Path` 用 `path.bounds`（作者提供）；若声明小于真实顶点，extent 会偏小（op 裁剪同源，P0 前既有契约），P1 统一 geometry 语义时一并处理。
5. **真实窗口像素判定**仍由 P3-6 补（§11.3-8）；本轮 `--dump-overlay` 只验证 rect/children，headless `--pixel-check` 是像素证据唯一来源（已含负向自检）。
6. **`cargo test` workspace 其余 crate**：按指令未跑；`cargo check --workspace --all-targets` 已 PASS，R-08 六 crate 由 lead 补跑 1216 passed，其余 widgets/kytrix 测试属 P3 补充项（§11.6-1）。
7. **`record_primitives` 把 Canvas 内部图元全部展开**：`FrameRecord.primitives` 体积随画布图元数线性增长（仅 test-support），对图表类 canvas 的测试快照会更大；当前 ui_core/ui_kit 与 6 crate 测试耗时正常（ui_core lib 36 s 含 GPU 像素用例），无实际症状。

---

## 5. 结论

| severity | 数量 | 条目 |
|---|---|---|
| P0 | 0 | — |
| P1 | 0 | — |
| P2 | 1 | R2-01（不阻塞；建议本轮修「clip-aware extent」或登记偏差） |
| P3 | 0 | — |

- **上轮 R-01…R-09 全部关闭**（R-01/04/05/07/09 代码+测试；R-02 代码+两级测试；R-06 测试+登记；R-08 统一 `hit_test`+6 crate 用例）。
- **本轮门禁全绿**：`cargo test -p ui_core -p ui_kit`（3551 passed / 2 ignored）、`gallery --pixel-check`（108 checks，`damage_full_frames=0`，首可见帧==稳定帧）、`--dump-overlay --auto-close=9`（真实窗口 overlay-check PASS）、`cargo check --workspace --all-targets`。
- **是否可提交 P0**：**可以提交**。唯一新增 P2（R2-01）不涉及正确性、有①修 5–10 行 ②最小改 ③登记偏差 三种收口方式；若本轮坚持「P2 清零」，建议采用方案 1（canvas 内 clip 栈）并在提交前跑一次 `cargo test -p ui_core -p ui_kit` 与 `gallery --pixel-check` 复核。
