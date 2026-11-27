# P0 实施摘要（供对抗性审查 / 提交说明素材）

- 分支/worktree：`dsh-1d175414-1a41`（HEAD `71a5a5cc` + 未提交 P0 改动，含 P0 修复轮）
- 设计文档：`.docs/popup-overlay-redesign-design.md` rev.6（§2.3、§4.11、§5.3、§6 P0、§7.2/§7.4/§7.5、§8、§9.1、§10、§11 回写）
- 证据：`.docs/popup-overlay-redesign/evidence/p0-diagnosis.md`、`paint-damage-rootcause.md`、`reproduction.md`
- 审查报告：`.docs/popup-overlay-redesign/implementation/review-p0-{render,conformance,quality}.md`；修复轮对应 §11.3 偏差与 §11.6 未执行项

## 本轮门禁（lead/审查实测，全部 EXIT=0）

- `cargo test -p ui_core -p ui_kit`：**3554 passed / 0 failed / 2 ignored**（ui_core lib 2720 passed/1 ignored、frame_record 8、layout 4、snapshot 14、demo 39；ui_kit lib 769；P0 新增 pass tests 19 / removal 11 / pixels 7）
- `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings`、`cargo +nightly fmt --all -- --check`：PASS
- `cargo check --workspace --all-targets`：PASS；R-08 六 crate 测试已补跑 PASS（1216 passed，见偏差 14）
- `cargo run -p ui_kit --example gallery -- --pixel-check`：PASS（108 条 record 断言 = 45 条逐帧采样断言（44 changed-outside-damage + 1 full-replay 首帧） + 20 idle-frames + 20 background-residue + 3 restored + 2 close-frames + 18 条具名不变量；含覆盖下限 30‰ 与 2 点注入自检；damage_full_frames=0，first_visible==stable）
- `cargo run -p ui_kit --example gallery -- --dump-overlay --auto-close=9`：真实窗口 overlay-check PASS（仅 rect/children；无像素 readback）

## 任务完成情况

| 任务 | 结果 |
|---|---|
| P0-0 判别实验 | 完成：M-D/M-E refuted（E1/E2/E3/E9），M-C 收窄为「打开路径旧位置未进 damage」，M-A confirmed。设计已按结论修订 rev.6。 |
| P0-4a Canvas 局部坐标契约 | `Canvas` 内部坐标为**节点内容盒局部坐标**（border box + padding/content_offset；当前全仓 content_offset=0）；新增 `CanvasDraw { canvas: Arc<Canvas>, offset: LogicalPoint }` 作为 `Primitive::Canvas` 载荷；encoder 递归平移 quad/shadow/image/glyph/clip/path/geometry/嵌套 canvas；`Canvas` 节点 damage 用屏幕外延；`baked_screen_bounds` 侧表使 screen-baked lane 在布局位置/滚动变化时重建；Canvas 构造点 **132/41**（`ui_core` 93/24、`ui_kit` 9/2、`widget` 5/2、`widgets` 25/13；含 P0 新增测试 +8）。 |
| P0-4b 外部迁移（含 R-08） | 迁移 `crates/widget` 与 `crates/widgets` 17 文件；图表 layout 局部化、缓存键去 origin、geometry/path 顶点局部化。**6 处指针→局部坐标换算改为 `NodeTree::hit_test().local_point`**（candlestick、cryptocurrency_etf、perpetual_statistics、volume_heatmap、prediction_market、global_shipping_chokepoint）——滚动祖先下不再整体偏移 scroll offset；各 crate 加滚动容器测试；六 crate 已补跑 **1216 passed**（见偏差 14）。`cargo check --workspace --all-targets` 绿；P0-4b 自证 9 个 widgets crate 1639 passed（非本轮门禁）。 |
| P0-1 painted_extent | 新增 `paint/extent.rs`：`PaintedExtent { bounds: LogicalRect, pad: PhysicalPx }`（绝对布局空间 L，pad 固定 1 物理 px），`NodeTree.painted_extents` 侧表（Node 尺寸保持 336B）；quad/glyph/image/Canvas/shadow/ring 由 emitter 累计，独立 Geometry chunk 由 `paint_geometry_node` 并入；lane reuse 保留；hide/空 lane/geometry 清空并先 damage 旧 extent；remove 对子树 layout.bounds ∪ extent 取并集（并入父 damage 前按祖先滚动差换算到父发射空间，修 R-02）；移动 damage = 旧 layout ∪ 旧 extent（新位置由 emit 覆盖）。侧表内存 ≈44 B/节点（`baked_screen_bounds` 20 + `painted_extents` 24），40k 节点 ≈1.7 MB。 |
| P0-2 防御性不变量测试 + R-04 生产修复 | `render/compose.rs` 新增 `releasing_a_lane_rebuilds_the_ops_without_the_freed_range`、`reordering_lanes_without_removal_rebuilds_the_ops`、`a_removal_note_with_a_stale_generation_keeps_the_live_lane`；**`render/delta.rs` 有 R-04 生产修复**：`LaneStore::apply` 对 generation 不匹配的 removal note 直接跳过（不再误删被复用 slot 的存活 lane，`delta.rs:394-408`）、`note_removed` 清 `versions[slot]` 前校验 generation（`delta.rs:339-345`）；对应测试还有 `a_released_lane_is_not_resurrected_by_the_covered_delta`、`absorb_drops_an_old_generation_lane_when_the_slot_is_reused`、`applying_the_same_delta_twice_is_idempotent`（详见设计 §11.3-21）。 |
| P0-5 持久增量像素 harness | `test_support/pixels.rs` 新增 `PersistentPixels`（同一 persist 纹理跨帧、按 `FrameRecord.damage/damage_full` 走 `Plan::Incremental` + damage scissor、readback、`compare_pixels/changed_pixels`）；P0 新增 7 条测试：机制自检（人为漏 damage 会残留）、移动、open/settle/close/reopen 4 轮、带阴影+Canvas 子树增删、滚动容器同 scroll 表重放等。`Window::scroll_slots` 一行 cfg 放宽到 test-support；并新增公开 `EngineError::PixelHarness`/`PixelHarnessCause`（Q-05）。 |
| P0-6 kit 防御 + gallery | ColorPicker `panel.content`/`palette` 加 `overflow_hidden`；gallery 新增 `--pixel-check`（headless 镜像 TestApp + 真实 ColorPicker，108 条 record 断言：45 条逐帧采样断言（44 changed-outside-damage + 1 full-replay 首帧）（采样帧）+ 20 idle-frames + 20 background-residue + 3 restored + 2 close-frames + 18 条具名不变量，含每帧 `!damage_full`、变化像素⊆damage、popup 外背景恢复、关闭/重开恢复、首可见帧 rect==稳定帧、baseline-opaque）。**修复轮加强**：移动阶段改为“旧浮层区域 − 新浮层/新旧 trigger”全区域对打开前基线逐像素扫描 + **覆盖量下限**（compared ≥ fixture 设备像素 30‰；实测 26104/480000≈5.44%）+ **≥2 个分散像素注入**负向自检（防排除区膨胀假绿）；idle 采样 137 ms×20 且先断言无动画源；输出退出码语义（0/3）与 P1 迁移提示。真实窗口 `--dump-overlay` 旧语义不变（只查 rect）。 |
| P0-7 结构变化增量绘制（用户裁决 A） | `mark_structure` 不再把父盒并入 damage；`window/paint.rs` 收窄 `!removed.is_empty()`/structure epoch/无 lane·clip_key 升 full（拆 `take_force_full_paint`/`structure_changed`/`scroll_registry_changed`）；结构帧增量 paint + `rebuild_order_only` + `scene.set_order`；remove 用 extent 局部 damage；滚动容器注册/移除才安全降级为全量绘制（damage 仍局部）。新增 5 条结构增量测试；virtual-list+popup 开关+`ScrollTable` 用结构/damage/scroll 表断言锁定（无 stale slot、`delta.scroll` 仅真实滚动帧为 1、全程 `!damage_full`，C-07）。 |
| P0-8 旧 kit Overlay 首帧定位（用户裁决 A） | `Overlay::measure`：open 内运行 `App::refresh_text` + `Window::compute_layout`，按实测尺寸 place，随后 reveal（先 place 后 reveal；**宿主不调用 `sync()` 首帧也会显示**，修复 R-05 契约）；`place()` 不再承担首次 reveal；`measure`/`place` 共用定位内核（Q-15）；新增首帧测试（overlay：**文本主导高度**——无 refresh_text 时首帧 y=188 vs 稳定 69、无宿主 sync 也可见、同帧/下一帧 sync 均可见；ColorPicker 首可见帧 rect==稳定帧）。`ColorPicker::open -> Result<bool, OverlayError>`，measure 失败回滚并传播（Q-04）。 |

## 已知偏差与风险（详见设计 §11.3）

1. **真实窗口无 readback**：像素门禁为 headless 镜像（用户已接受路线）；真实窗口仅保留 `--dump-overlay` rect 检查；P3-6 补三端真实窗口像素方案（待定）。验收②按此口径；验收③本轮只跑 ui_core/ui_kit，widgets 以审查复跑 golden 8 passed + P0-4b 自证 1639 passed 为证据（C-01 证据链）。
2. **painted_extent 空间为 L（非设备像素，C-02/Q-01 最终口径）**：设计原写 `DeviceRect`；实现为 `LogicalRect + 1 物理 px pad`。`arena::remove()` 已按「被删子树每节点相对直接父节点发射空间的滚动差」**逐节点**换算后并入父 damage（覆盖直接父自身 scroll offset + 子树内嵌套滚动容器 offset；offset≠0/嵌套/像素回归均有测试，`removal_tests::removing_a_scrolled_container_child_damages_its_screen_position` 等）；`place_subtree` 的移动 damage 记在被移动节点自身并用其自身 total 发射，已验证正确。**未覆盖**：跨滚动上下文重挂（`append_child` 在 A/B 两滚动容器间搬子树）旧 damage 会用新上下文 total 发射、差新旧 total；当前生产调用方都不在滚动容器内（P0 用户不可见），P1 列表复用/reparent 前必须给 `append_child` 补同样换算。
3. **damage 全帧阈值**改为按 frame 裁剪后可见面积计算（窗外面积不计入），damage 矩形不变。
4. **Canvas 局部原点 = 内容盒左上**（border box + content_offset；当前全仓 content_offset=0，故与“节点左上”等价）；P1 加 padding 需遵守。
5. **Canvas 构造点实际 132/41**（设计原 45 文件数为子串口径误计；含 P0 新增测试 +8：首轮 124/41 → 修复轮 +5 → R2-01 再 +3 = 132/41；132/132 语义迁移，含 diagnostics）。
6. **Canvas 内容外延 = 实际图元并集（C-10 取舍，R2-01 收紧）**：`canvas_content_extent` 在 emit 时递归并集 quad/glyph/image/path、shadow 物理 blur pad、Geometry transform AABB、嵌套 canvas offset，再 translate(origin) ∪ 节点盒，同一并集用于 extent 与 damage；成本是 lane 重建多一次 O(图元数) 并集（lane 复用不执行）。不采用“声明 `Canvas::bounds()` 契约 + debug 断言”。R2-01 起另加两道收紧：①画布内 `ClipPush/Pop` 栈逐层求交；②Geometry 只取 `draw.range` 命中的 chunk AABB（与 encoder 同构），map 大 Mercator AABB + clip + zoom 不再退化为 `damage_full`。
7. **I6 的 Geometry 边界（R-06）**：独立 `NodeKind::Geometry` 节点的任何结构变化（新增/布局移动/删除）都因既有 `geometry_damage_escalation` 兜底升格 `damage_full`（Geometry 块 AABB 走 transform 坐标、节点盒走 L 坐标）。I6“非模态 popup 开关不 damage_full”**只对内容闭包含不含独立 Geometry 节点成立**（ColorPicker 满足）；P1 内容闭包应禁止独立 Geometry 或先统一其 damage 坐标语义。
8. **C-06 落点**：order-only 不变量测试实际在 `render/compose.rs`（`releasing_a_lane_rebuilds_the_ops_without_the_freed_range` / `reordering_lanes_without_removal_rebuilds_the_ops` / `a_removal_note_with_a_stale_generation_keeps_the_live_lane`），非设计 I2 原提的 `paint/instance.rs`/`render/delta.rs`；接受该落点。
9. **C-07 验证手段**：virtual-list + popup 开关 + `ScrollTable` 一致性用结构/damage/scroll 表断言锁定（无 stale slot、`delta.scroll` 仅真实滚动帧为 1、全程 `!damage_full`），未接持久像素；ui_core 已有等价像素门禁（`test_support/pixels.rs` 滚动重放），P1 Select/Dropdown 行回收应接 `PersistentPixels` 三联用例。
10. **Q-07 断言范围**：持久像素测试的“基线完全不透明”断言排除最外 1 行/列（TestApp/渲染器既存 3 个 `alpha=218` 边缘伪影），内容区要求完全不透明；gallery 的 `baseline-opaque` 是独立实现。
11. **`--pixel-check` 与旧 Overlay/ColorPicker API 强绑定**：P1-11 删除这些 API → **P1-12 必须迁移**（设计 §8 已登记）；该命令名/退出码保留。
12. **`Overlay::measure` 文本边界**：覆盖静态文本高度；`bind_text` 内容在 style pass 才求值，首帧仍可能位移（P1-2 settle 接管）；measure 次数：`Overlay::open` 对空内容 measure（1 次 refresh_text + 1 次 compute_layout）；ColorPicker `open_panel` 构建内容后再 measure（第 2 次 refresh_text + 第 2 次 compute_layout，第二次 text pass 通常为空操作）。
13. 结构帧 order 重建 O(n) DFS；gallery 像素对比容差 span≤2、排除当前 popup/trigger 各外扩 16 px（opener focus ring）；旧 Overlay `measure` 属 P1 会删除的过渡面。
14. **R-08 六 crate 已补跑 PASS**：`cargo test -p candlestick -p cryptocurrency_etf -p perpetual_statistics -p volume_heatmap -p prediction_market -p global_shipping_chokepoint` → **1216 passed / 0 failed**（351/137/164/117/267/180）；补跑发现的唯一失败是**测试装置**问题（滚动容器内测试页默认 flex-shrink 被压缩 → 已加 `flex_none()`，volume_heatmap 用 4 块热图 fixture 收紧断言），生产换算本身正确。其余 widgets crate 未跑（用户限制）。
15. 旧 `Overlay` 首帧实现（P0-8）与 `--pixel-check`、`PersistentPixels` 是 P1 会删除/替换的面：P1-11 删除 Overlay API，P1-12 重写像素门禁与首帧断言。
16. **P0-2 含 R-04 生产修复（C2-01 更正）**：`render/delta.rs` 的 `LaneStore::apply` 校验 removal note 的 generation（不匹配则跳过，`delta.rs:394-408`），`note_removed` 清 `versions[slot]` 前校验 generation（`delta.rs:339-345`）；测试 `render/compose.rs::a_removal_note_with_a_stale_generation_keeps_the_live_lane` 及 delta 吸收/释放用例。详见设计 §11.3-21。

## 关键文件

- `crates/ui_core/src/paint/{canvas,primitive,instance,pass,extent}.rs`
- `crates/ui_core/src/node/tree.rs`、`node/tree/{content,arena,layout,dirty,tests}.rs`
- `crates/ui_core/src/window/paint.rs`、`crates/ui_core/src/render/{damage,compose,delta}.rs`
- `crates/ui_core/src/test_support/{pixels,record,snapshot,diagnostics}.rs`、`crates/ui_core/src/window.rs`（1 行 cfg）
- `crates/ui_core/src/app/{removal_tests,canvas_text}.rs`
- `crates/ui_kit/src/kit/{overlay,color_picker,spinner}.rs`、`crates/ui_kit/examples/gallery.rs`
- `crates/widget/src/test_support.rs`、`crates/widgets/*`（17 文件 + 6 处指针换算及其测试）
