# P0 一致性审查（第 2 轮）— review-p0-conformance-r2

- 审查者：`p0-review-conformance`（子代理），对 `lead` 负责
- 审查对象：worktree `E:\kytrix\.agents\worktrees\dsh-1d175414-1a41`，HEAD `71a5a5cc` + 未提交 P0 工作树（57 modified + 1 untracked `crates/ui_core/src/paint/extent.rs`；`git diff --stat` = 6038 insertions / 585 deletions）
- 审查依据：`popup-overlay-redesign-design.md`（rev.6 + §11 回写）、`AGENTS.md`、`implementation/p0-summary.md`、上轮 `review-p0-conformance.md`、`review-p0-quality.md`、`review-p0-render.md`
- 权限/边界：只读代码，**未修改任何仓库实现文件**；本报告写入 gitignored 的 `.docs/`；按指令只跑 `ui_core`/`ui_kit` 门禁与 gallery，不运行其它 crate 测试
- 环境：Windows 11 / D3D12 WARP / 125% DPI；`CARGO_TARGET_DIR=E:\kytrix\target`；2026-11-26

**结论摘要：P0=0，P1=0，P2=1（C2-01），P3=5（C2-02…C2-06）。上轮 C-01…C-14 共 14 条全部关闭（fixed）；代码与门禁满足提交条件，但 §11/P0 结果表仍有 6 处文档与代码/实测不一致，其中 C2-01（“P0-2 无生产改动”与 `render/delta.rs` 生产改动矛盾）必须在提交前修正；修完文档后即可“清零提交”。**

---

## 1. 上轮 C-01…C-14 复核

状态口径：`fixed` = 上轮问题已按报告要求消除且有代码/测试/文档证据；`partially` = 主要部分消除但仍有残留；`not-fixed`；`invalid`（上轮判断不成立）。

| ID | 原级 | 结论 | 证据 |
|---|---|---|---|
| C-01 | P1 | **fixed** | 验收②改为 headless 路线并记录用户裁决：设计 `:708`（P0 验收②、③）、`:923`（§11.3-8）、`:950-957`（§11.5 审核证据行）、`:971-977`（§11.6 未执行项）；`§7.5 :812-820` 新增 `--pixel-check`/`--dump-overlay` 可复制命令与 P1-12 迁移登记。实测 headless 门禁 PASS（§4）。③ 的 9 crate 1639 仍为自证，但已明确标注“非本轮门禁、未复跑”，且六 crate 1216 已补跑（§11.6-2）；**证据边界已显式登记，不再是无证据的主张** |
| C-02 | P1 | **fixed** | ① `painted_extent` 空间口径改为 L 空间并写入 §4.11 `:562`、§11.3-4 `:919`，`Node` 尺寸未变（§9.1 R9 `:862` 实测 44 B/节点侧表）；② `arena::remove` 逐节点换算滚动差后并入父 damage（`node/tree/arena.rs:369-470`，`parent_own` + 子树内嵌套容器 offset 栈），新增 `removal_tests::removing_a_scrolled_container_child_damages_its_screen_position`（`:1633`）、`removing_a_child_of_a_nested_scroll_container...`（`:1836`）、`test_support/pixels.rs::removing_a_direct_child_of_a_scrolled_container_damages_its_screen_position`（`:1146`，断言滚动后屏幕位 (120,100) 先 BLUE 后 WHITE、damage 覆盖屏幕位、基线一致）；③ `place_subtree` 改为 `target.damage = old.bounds ∪ painted` 且 `mark_paint_without_damage`（`node/tree/layout.rs:284-297`），新位置由 emit 覆盖（`paint/pass.rs:694, 752-759`），`taffy_tree.rs:803-804` 断言同步更新。跨滚动上下文 `append_child` 缺口按 §11.3-4 登记为 P1 前置（代码另在 `append_child :328-383` 对“被搬子树含滚动容器”升 `scroll_reg_epoch` 作部分缓解，未宣称已覆盖） |
| C-03 | P2 | **fixed** | P0-8 已进入设计的任务/验收/迁移/API/偏差全部位置：§6 范围说明 `:688`、任务表 `:706`、验收 `:708`、§7.5 `:820`、§8 `:843`、§10 范围追加 2 `:1035`、§11.1 `:897`、§11.2 `:912`、§11.3-3 `:918`、§11.4 `:943`、§11.7 `:981-983`。代码对应：`kit/overlay.rs:695-891`（open 内 measure→place→reveal，`place()` 不再 reveal，`revealed()` 新查询面）、`kit/color_picker.rs:805-846, 1314-1347`（`Result<bool, OverlayError>` + 失败回滚）、`error.rs`（`OverlayError` 既有）；gallery overlay-check 调用点已适配 `:1886-1900` |
| C-04 | P2 | **fixed** | `test_support/diagnostics.rs:410-419`：`DamageOverlay::canvas` 改为 union 归一化（Canvas 原点 ZERO、rects 平移 `-origin`）；测试 `damage_overlay_covers_every_rect`（`:771-813`）断言 `canvas.bounds().origin == ZERO` 且窗口 (10,20)@2x 归一化为局部 (5.5,10.5)，另有 `:594-605` clip 场景 Fixture 改为局部坐标 |
| C-05 | P2 | **fixed** | gallery 移动阶段改为全区域扫描：`assert_old_popup_restored`（`examples/gallery.rs:2928-2972`）以“旧浮层区域（外扩 16）− 当前浮层（外扩 16）− 新旧 trigger（外扩 16）”对打开前基线逐像素比较（`mismatched == 0 && compared > 0`），并新增负向自检 `residue_detector_self_check`（`:3050-3074`，注入品红像素后必须检测到）；run 路径传入移动前 trigger bounds（`:3355-3367`）；PASS 输出含 `old-region`/`residue-detector`。**残余为覆盖量下限问题，见 C2-05（P3，非 false pass）** |
| C-06 | P2 | **fixed** | `render/compose.rs:620`（释放 lane 后 ops 仅含存活区间）、`:664`（stale generation 不得摘存活 lane）、`:719`（仅顺序变化走 ops 全量重建、实例段复用、ops 分组顺序正确）三条强断言测试；`render/delta.rs` 另有生产修复（见 C2-01）：`note_removed :339-345` 加 generation 守卫、`LaneStore::apply :394-408` 不再对不匹配 generation 的移除 `take()` |
| C-07 | P2 | **fixed** | `removal_tests::a_virtual_list_keeps_its_scroll_table_consistent_across_item_churn_and_popups`（`:1722-1834`）：滚动 600 → 断言 scroll_slots 三要素/`delta.scroll==1`/`!damage_full`；增行 140、减行 80、popup 开/关各自断言 `delta.scroll==0`、scroll 表不变、`!damage_full`。§11.3-16 `:931` 明确本用例为结构/damage/scroll 表断言、未接 `PersistentPixels`，并说明 `test_support/pixels.rs` 滚动重放等价覆盖 |
| C-08 | P2 | **fixed（完整性）** | §11.1–§11.7 已成文（`:891-984`），§11.1 提交内容/范围、§11.2 逐任务结果、§11.3 20 条偏差、§11.4 API、§11.5 门禁基线、§11.6 未执行、§11.7 P1 衔接均存在；`p0-summary.md` 已同步 P0-8/R-08/残影门禁加强等。**数字准确性问题另立 C2-01/C2-02** |
| C-09 | P2 | **fixed** | §7.5 `:808-820` 命令块 + P1-12 迁移“不得静默移除”；§10 DoD#1 `:881` 明确 P0 门禁口径（headless 像素 + 真实窗口 rect，真实像素 P3-6）；§8 `:843` 登记 pixel-check 依赖的旧 API；gallery `--pixel-check` 退出码 0/3 + P1 迁移提示均实现（实测 PASS） |
| C-10 | P2 | **fixed** | `paint/pass.rs::canvas_content_extent`（`:1444-1495`）递归并集实际图元外延：quad/glyph/image/path、shadow 的物理 blur pad、Geometry 经 transform 的 AABB、嵌套 canvas offset；`Content::Canvas` 用该 extent 同时做 `note_painted` 与 damage（`:1759-1773`）；新增 `pass.rs::a_canvas_with_content_outside_its_declared_bounds_damages_the_actual_content`（`:3084`）等测试；§4.11 `:562`、§11.3-17 `:932` 已按“实际并集”契约回写 |
| C-11 | P3 | **fixed（原文错误已消除，计数再次漂移）** | 设计已把 132/45 修正为 124/41 并说明子串口径误计（§5.3 `:682`、§6 `:703`、§9.1 `:856`、§10 `:884`、§11.3-7 `:922`）。但修复轮新增 5 个 `Canvas::new` 测试构造点，实测已为 **129 处/41 文件（ui_core 90/24）**，见 C2-02 |
| C-12 | P3 | **fixed** | 契约统一为“节点内容盒左上”：§4.11 `:562`、§11.3-6 `:921`；`p0-summary:21` 写明 content_offset=0；实测全仓 `Canvas` 构造点均局部坐标（抽查 `kit/color_picker.rs:2199-2204` `node_canvas_bounds` 返回 ZERO 原点；新增 `pixels.rs:870, 1215`、`instance.rs:4008`、`pass.rs:3242,3259` 均为局部），未发现残留绝对坐标 |
| C-13 | P3 | **fixed** | `node/tree/arena.rs:270-282` 的 `clear_side_tables`（remove/insert 时清 `baked_screen_bounds` 与 `painted_extents`）与 `:453-456`；`insert_child :163` / `remove :369` 路径均调用；`node/tree/tests.rs` 新增 1 条测试 |
| C-14 | P3 | **fixed** | `p0-summary.md:23` 已精确表述两条通道：quad/glyph/image/Canvas/shadow/ring 由 emitter 累计、独立 Geometry chunk 由 `paint_geometry_node` 并入、lane reuse 保留、hide/空 lane/geometry 先 damage 旧 extent、remove 对子树 layout∪extent 并集并按滚动差换算、移动 damage=旧 layout∪旧 extent；与 `paint/pass.rs:694-777, 900-901, 1200-1215` 一致 |

**上轮计数对账**：14/14 关闭（fixed）。另核对 `review-p0-render.md` 的 R-01…R-09：R-01 已修为 `Some(path.bounds)` + 两条测试（`paint/instance.rs:4000` 125% 局部 op-bounds、`test_support/pixels.rs:1199` 增量帧 Path 像素红色断言，后者在旧双偏移缺陷下必失败）；R-02→C-02；R-03 由上述像素测试补上反例；R-04 已修但**文档未登记**（C2-01）；R-05 由 `overlay.rs:695-838` + 4 条 overlay 首帧测试 + ColorPicker 首帧测试覆盖；R-06 由 `standalone_geometry_structural_changes_escalate_to_full_damage`（`removal_tests.rs:1921`）+ §11.3-14 `:929` 登记；R-07 由 `render/damage.rs::finish` 按 `rect.intersect(frame).area()` 判阈值（`:98-116`，阈值 0.5）+ 1 条新测试覆盖；R-08 见下；R-09 由 `record_primitives`（`record.rs:254-289`）+ 契约测试覆盖。`review-p0-quality.md` 的 Q-01≈C-02、Q-04/Q-15 见 `color_picker.rs:1338`/`overlay.rs:848`、Q-07 见 `pixels.rs::assert_baseline_opaque` 排除最外行/列 + §11.3-18 `:933`。

**R-08 实点复核**：6 个迁移点确已改为 `NodeTree::hit_test` 的 `local_point`——candlestick `view.rs:1274-1278`（`hit.node == chart.node()`）、cryptocurrency_etf `view.rs:395-408`、perpetual_statistics `pane.rs:465-476`、volume_heatmap `view.rs:534-546`（`chart.rs` 同步删掉 `Layout.origin` 与减法，`ensure_layout` 只按 size）、prediction_market `chart/canvas.rs:350-359`、global_shipping_chokepoint `map.rs:759-774`（缓存 `screen_origin`，view `:296-357` 各事件先 `note_pointer`）。6 crate 各新增 1 条“滚动祖先内 hover/pan/zoom 仍命中”用例（candlestick `view/tests.rs` 滚动 offset=160、窗口坐标≠局部坐标、cursor 仍为图表局部；volume_heatmap `view/tests.rs:1935+` 等）。**未发现 R-08 引入的二次偏移**（各 canvas 的 `point_at`/`hover_at` 均直接接收局部坐标，无 origin 二次相减）；`candlestick::ChartCanvas::bounds()` 因不再使用而删除，仅为死代码清理。

---

## 2. 新 findings（C2）

### C2-01 [P2] §11.2 / P0-summary 声称“P0-2 无生产改动”，与 `render/delta.rs` 生产修复矛盾，且偏差登记缺项

- 位置：`popup-overlay-redesign-design.md:905`（§11.2 P0-2“**无生产改动**，仅 compose.rs 测试”）、`implementation/p0-summary.md:24`（同句）；`§11.3-15 :930` 只登记“测试落点”，未登记生产代码变更。
- 实际代码：`crates/ui_core/src/render/delta.rs`
  - `DeltaBuilder::note_removed :339-345` 新增 generation 守卫（只有 slot 版本匹配才清零版本号）；
  - `LaneStore::apply :394-408` 改为先判断 `stored.generation == removed.generation()` 再清空，不匹配时 `continue`（旧实现先 `Option::take` 再判断，会把存活 lane 丢掉）。
- 影响：这是对 `review-p0-render.md` R-04（stale generation 会摘掉存活 lane）的真实生产修复，且行为由 `compose.rs::a_removal_note_with_a_stale_generation_keeps_the_live_lane` 锁定；但结果表与总结都写“无生产改动”，提交说明若引用该表会与 diff 矛盾，且违反“P0-2 按 P0-0 收窄、不重写 ops 生命周期”的审计叙事（实际是安全性小修，应显式声明）。
- 建议：把 P0-2 结果改为“防御性测试 + 1 处 stale-generation 安全修复（`delta.rs`，R-04）”，在 §11.3 增加一条偏差/修复登记（含测试名与 `delta.rs:339-345/394-408`）；`p0-summary:24` 同步删掉“无生产改动”。**这是本次唯一 P2，必须在提交前改。**

### C2-02 [P3] §11.5/§11.2/§11.3 与 `p0-summary` 的测试/迁移计数已漂移，且 `--pixel-check` 断言计数存在 +1 重复计数

逐项实测（本机本 worktree，§4）：

| 文档主张 | 位置 | 实测 | 判定 |
|---|---|---|---|
| `cargo test -p ui_core -p ui_kit` = **3550 passed**，ui_core lib **2716** passed/1 ignored | §11.5 `:950`、`p0-summary:10` | **3551 passed**，ui_core lib **2717** passed/1 ignored（其余 frame_record 8/layout 4/snapshot 14/demo 39/ui_kit 769 均一致，ignored 2 一致） | 不一致（差 1） |
| Canvas 构造点 **124 处/41 文件**，其中 ui_core **85/24**、ui_kit 9/2、widget 5/2、widgets 25/13 | §5.3 `:682`、§6 `:703`、§9.1 `:856`、§10 `:884`、§11.3-7 `:922`、`p0-summary:36` | **129 处/41 文件**，ui_core **90/24**、ui_kit 9/2、widget 5/2、widgets 25/13（修复轮新增 5 个测试构造点：`instance.rs:4008`、`pixels.rs:870`、`pass.rs:3242/3259`、`node/tree/tests.rs:866`；均局部坐标，语义迁移 129/129） | 不一致 |
| P0-4a “**13 条**新增 pass.rs 测试”；P0-1 “**13+7** 条用例”；P0-5 “**5 条**像素测试”；§11.5 “pixels **5 条**、pass **13 条**、removal **7 条**” | §11.2 `:904, 907, 909`、§11.5 `:950`、`p0-summary:25` | 新增 `#[test]`：pass **16**、removal **11**、pixels **7**、compose 3（文档 P0-2 未给数，但“无改动”问题见 C2-01） | 不一致 |
| §11.3-3 “overlay **5 条** + ColorPicker **1 条**” 首帧相关测试；§11.7 “**4 条** kit 首帧测试” | §11.3-3 `:918`、§11.7 `:981` | 新增 overlay 测试 **4 条**（另有 1 条 `a_fade_less_panel_is_hidden_on_its_first_frame` 被同位置重写为新语义）；ColorPicker 新增 **2 条**（1 条首帧矩形、1 条 clip 防御）；两处口径不一致 | 口径不一致/内部矛盾 |
| `--pixel-check` “**108 条断言** = 45 采样帧逐帧 + 20 空闲帧 + 独立不变量” | §11.2 `:910`、§11.5 `:954`、`p0-summary:13,26` | 实跑 checks=**108**、sampled=**45**（吻合）；但 45+20+“约 30”=95≠108；且 `gallery.rs:3271` 在 `record()` 外手工 `checks += 1`，`record()` 内 `:2800` 再 +1，**baseline-opaque 这 1 条被计两次** → 实际 `record()` 断言数为 **107**，`108` 是人工加数 | 计数口径不一致（建议删 `:3271` 手工加数后统一为 107，或说明计数口径） |

- 影响：§11.5 是提交说明引用的“门禁基线”，数字不可复现会让审查者质疑基线真实性；`124→129`、`13→16` 等漂移均为修复轮新增测试导致，非实现问题。
- 建议：提交前用本次实测值刷新 §5.3/§6/§9.1/§10/§11.2/§11.3-7/§11.5 与 `p0-summary` 对应数字；删除 `gallery.rs:3271` 的手工计数并重跑一次确认输出 `checks=107`（或保留 108 但在 §11.5 注明“含 1 条门禁自检计数”）。

### C2-03 [P3] §11.3-10“measure 每次 open 最多 1 次 text pass”与 ColorPicker 路径的 2 次 `refresh_text` 调用不符

- 位置：设计 §11.3-10 `:925`；代码 `kit/overlay.rs:811-826`（`measure` 内 `App::refresh_text`）、`overlay.rs:802`（open 调 measure 一次）、`kit/color_picker.rs:1338`（open_panel 再调 `overlay.measure` 一次）。
- 事实：ColorPicker 打开路径会调用 `measure` **两次**（overlay.open 内一次 + 内容构建后一次），因此 `App::refresh_text` 被调用两次（第二次通常无脏文本、成本接近空操作），`Window::compute_layout` 两次——设计写的“最多 1 次 text pass”只对纯 `Overlay::open`（无内容二次测量）成立。
- 建议：改为“最多 2 次 `refresh_text` 调用（第二次通常无脏文本，接近空操作）/ 2 次 `compute_layout`”，或按 Path 分述（Overlay 1/1，ColorPicker 2/2）。

### C2-04 [P3] §11.3-20 `FrameRecord.primitives` “统一 lane/绘制空间”声明过宽：Geometry/ClipPop/Text 仍用 L 空间 `node_bounds`

- 位置：设计 §11.3-20 `:935`；代码 `test_support/record.rs`：`record_primitives :254-289` 对 Canvas 内图元按 `offset + draw.offset` 平移（quad/glyph/image/path/shadow/canvas 均如此），但 `primitive_kind :291-319` 对 `Primitive::Geometry(_)`、`Primitive::Clip(ClipOp::Pop)`、`Primitive::Text(_)` 直接返回 `node_bounds`（`tree.layout().bounds`，L 空间），未加 `offset`。
- 影响：滚动窗口下（`total ≠ 0`）同一 `primitives[]` 会同时出现“lane 空间”与“L 空间”两种坐标；当前消费方（widget 测试、gallery）都在无滚动场景，未暴露。§11.3-20 的“统一 lane/绘制空间”因此不成立（R-09 的修复只覆盖 Canvas 内部图元扁平化）。
- 建议：把契约限定为“Canvas 内部图元扁平化为节点绘制空间；Geometry/ClipPop/Text 仍记录节点 layout bounds（L 空间），消费方需按 kind 判断”，或在这三类也加 `offset` 后再宣称统一。

### C2-05 [P3] 移动残影门禁的覆盖量无下限、负向自检只验证 1 个像素，门禁存在“退化仍 PASS”的路径

- 位置：`examples/gallery.rs::assert_old_popup_restored :2928-2972`（`diff.mismatched == 0 && diff.compared > 0`）、`residue_detector_self_check :3050-3074`（只把第一个被比较像素涂成品红）。
- 事实：排除区（当前浮层/新旧 trigger 各外扩 16）若因 fixture/布局变化变大，`compared` 可跌到个位数而仍 `>0`；自检也只破坏剩余像素中的第一个，无法发现“只剩 1 个像素被比较”。当前 fixture 实测有充足覆盖（旧浮层区域 y≈395..482 的横带，估算约 2.7 万逻辑 px ≈ 4.2 万设备像素），**当前不是 false pass**，但属门禁退化风险。
- 建议：记录/断言 `compared` 下限（如 ≥ 旧浮层面积 × 1/4 或固定阈值），并在输出里打印 `compared/mismatched`；自检改为随机/固定 3 个不同像素（或整行）注入。另：§11.3-9 ⑤ 的措辞“注入旧色板像素”与实际“单个品红像素”不符，建议改“注入单像素故障”。

### C2-06 [P3] §11 文档完备性：§11.1 范围/§11.4 API 未列新公开错误类型；DoD#7 的 `kit-gallery.md` 未同步

- (a) `error.rs` 新增公开 `EngineError::PixelHarness { cause: PixelHarnessCause }`（`error.rs:266-296` 定义与 Display，4 个 cause 变体），但 §11.1 范围 `:896` 未列 `error.rs`、§11.4 API `:937-944` 未列该错误类型；对下游是 `EngineError` 枚举新增变体（穷尽 match 会破坏），属于最终 API 面，应登记。
- (b) §10 DoD#7 `:887` 要求 `.docs/gui-migration/spec-deltas/kit-gallery.md` 同步；该文件 mtime `2026/11/26 15:31`（早于 P0 实施），全文无 `--pixel-check`/`Overlay::revealed`/`P0-8` 记录，而 gallery 新增了门禁命令与 ColorPicker API 变更。若 P0 不要求完成 DoD#7，建议在 §11.6 明确登记为 P3 遗留；否则提交前补一段“P0 追加 `--pixel-check`、`ColorPicker::open -> Result`、P1-12 迁移”说明。

---

## 3. 文档 ↔ 代码一致性核对（除上表外的逐条）

| 主张 | 位置 | 核对结果 |
|---|---|---|
| P0-8 的 open 内 measure→place→reveal；“宿主不调 sync 首帧也显示” | §6 `:706`、§11.3-3 `:918`、summary:28 | **一致**：`overlay.rs:802-807`（measure→reveal→start_fade）、`measure :811-826`、`place_panel :848-891`；测试 `overlay.rs` 4 条（文本主导高度、无 sync 可见、同帧/下一帧 sync 均可见、fade=false 立即不透明）全部 PASS |
| `ColorPicker::open` 强类型错误 + measure 失败回滚 | §11.3-11 `:926`、§11.4 `:943` | **一致**：`color_picker.rs:805-846`、`1314-1347`（build_panel/measure 失败均 `close_now` 后 `Err`）；2 条错误路径测试 |
| “旧浮层区域 − 新浮层 − 新旧 trigger”全区域扫描 + 注入故障负向自检 + 137ms×20 idle + 先断言无动画源 + 容差 span≤2 + exit 0/3 | §11.3-9 `:924`、summary:26 | **一致**（代码 `gallery.rs:2928-2972, 3050-3074, 3242-3410`、常量 `:2779-2784`、`:3340-3345` idle 前 `animations_running` 断言）；仅措辞/覆盖量问题见 C2-05、计数见 C2-02 |
| `--dump-overlay` 仅 rect/children，旧语义不变 | §11.3-13 `:928`、§11.5 `:955` | **一致**：实测 PASS，输出 `overlay-check PASS (menu=... picker_children=Some(4))` |
| damage 全帧阈值按“frame 裁剪后的可见面积” | §11.3-5 `:920` | **一致**：`render/damage.rs:98-116`（`rect.intersect(frame).area()` 求和，`FULL_FRAME_AREA_RATIO=0.5`），新增 1 条测试 |
| `PaintedExtent` 不计 clip、保守外扩 | §11.3-13 `:928` | **一致**：`canvas_content_extent` 不处理 `Primitive::Clip`；`painted_content` 的 screen 也只并图元外延 |
| `Node` 结构体大小未变；侧表 44 B/节点，40k≈1.7MB | §9.1 R9 `:862`、§11.3-4 `:919` | **一致**（16+4 的 `PaintedExtent`→24 B；`Option<LogicalRect>` 20 B；40k×44≈1.76 MB；无 `size_of` 测试但类型推导与主张自洽） |
| FrameRecord 契约（Canvas 内部图元扁平化、quads/canvases 保持节点自有、CanvasRecord 局部） | §11.3-20 `:935` | **部分不一致**，见 C2-04 |
| 验收② headless 路线、真实窗口像素 P3-6 | §6 `:708`、§10 `:881`、§11.3-8 `:923` | **一致** |
| 验收③ 只跑 ui_core/ui_kit；widgets 以 golden 8 + 9 crate 1639 自证 | §6 `:708`、§11.3-8 `:923`、§11.5 `:956`、§11.6 `:973-974` | **一致披露**；本审查未独立复跑 widgets（指令限制），六 crate 1216 为 lead 授权补跑数据，我只能核对算术（351+137+164+117+267+180=1216 ✓） |
| 20 条偏差编号与内容 | §11.3 `:916-935` | **一致**：1 P0-3 移除、2 P0-7、3 P0-8、4 painted_extent 空间/内存、5 damage 阈值、6 Canvas 原点、7 计数、8 真实窗口替代、9 pixel-check 语义、10 measure 文本边界（数字见 C2-03）、11 ColorPicker 错误、12 R-08、13 其它登记、14 R-06 Geometry、15 C-06 落点、16 C-07 手段、17 C-10 取舍、18 Q-07、19 C-08 命令、20 R-09 契约（声明过宽见 C2-04） |
| §11.4 最终 API 清单 | `:937-944` | **部分缺失**，见 C2-06(a) |

---

## 4. 门禁执行记录（本审查实测）

| # | 命令 | 结果 |
|---|---|---|
| G1 | `cargo test -p ui_core -p ui_kit` | **EXIT=0**。ui_core lib **2717 passed / 0 failed / 1 ignored**（另有 `pacing_work_covers_every_reason_to_keep_running` 单独运行 1 passed）；`tests/frame_record.rs` 8；`tests/gui_smoke.rs` 0 passed/1 ignored；`tests/layout.rs` 4；`tests/snapshot.rs` 14；`examples/demo` 39；ui_kit lib **769**；doc-tests 0。合计 **3551 passed / 0 failed / 2 ignored** |
| G2 | `cargo check --workspace --all-targets` | **EXIT=0**（Finished dev profile） |
| G3 | `cargo run -p ui_kit --example gallery -- --pixel-check` | **EXIT=0 / PASS**：`checks=108 sampled_frames=45 damage_full_frames=0 first_visible==stable==(496,91,300×375) moved=(496,4,300×375)`；输出含 headless 语义与 P1 迁移提示 |
| G4 | `cargo run -p ui_kit --example gallery -- --dump-overlay --auto-close=9` | **EXIT=0 / PASS**：`menu.close -> true; color-picker.open -> true`；4 轮 picker `is_open=true content_children=Some(4)`；`overlay-check PASS (menu=Some((6,180.0,127.5125)) picker_open=true picker_children=Some(4))` |
| G5（补充核对 §11.5） | `cargo +nightly fmt --all -- --check` | **EXIT=0** |
| G6（补充核对 §11.5） | `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | **EXIT=0** |

> 说明：G1/G2/G3/G4 为指令要求的门禁；G5/G6 为核对 §11.5 声称的 PASS 而补跑（未扩展范围）。并发 cargo 锁风险已通过串行后台作业规避；目标目录与 §11.5 一致。

---

## 5. 测试有效性核查（是否放宽/删除/ignore/新增）

1. **无 `#[ignore]` 新增**：diff 中无 `+#[ignore]`；2 个 ignored 为既有 `ui_core lib` 1 条 + `gui_smoke` 1 条（与 §11.5/实测一致）。
2. **无 `#[allow]`/`#[expect]`/`#[path]`/注释新增**：对 9446 行 diff 扫描 `^\+.*(//|/\*|#\[allow|#\[expect|#\[ignore|#\[path)` 无命中；符合 AGENTS §1.6/§4。
3. **无新依赖/Cargo 变更**：`git status` 仅 57 个 `.rs` 修改 + 1 个新 `.rs`，无 `Cargo.toml/lock` 变更。
4. **测试删除/放宽**：仅 3 处旧测试名消失——`structure_and_scroll_epochs_force_the_next_full_frame`（拆分改名，功能由新测试覆盖）、`canvas_text_recipes_become_glyph_runs_during_paint`（同文件内移位/重写）、`a_fade_less_panel_is_hidden_on_its_first_frame`（**语义变化**：改为“open 后立即可见且不透明”，对应 §11.3-3 P0-8 裁决，属预期行为变更，不是放宽断言；新测试额外断言 `revealed()` 与首帧位置）。未发现容差放宽：`PIXEL_CHECK_TOLERANCE=2`、像素测试 `DAMAGE_PAD=1`、`assert_region_restored` 容差逻辑与上轮一致；`--pixel-check` 数量 108 与上轮相同但内部新增了更强的 old-region/负向自检（计数问题见 C2-02）。
5. **新增测试质量抽查**（均实际执行 PASS）：
   - `pixels.rs::removing_a_direct_child_of_a_scrolled_container...`（`:1146`）先断言滚动后目标像素为 BLUE，再删节点并断言旧屏幕位回 WHITE、damage 覆盖屏幕位、`!damage_full`、与基线逐像素一致——**在旧实现（无滚动差换算）下必失败**；
   - `pixels.rs::a_path_canvas_on_an_incremental_frame_renders_inside_the_damage`（`:1199`）断言局部 damage 帧内 `(place+5, place+2)` 为 RED——**在 R-01 双偏移缺陷下必失败**（op 被移出 damage/scissor）；
   - `removal_tests::removing_a_scrolled_container_child...`（`:1633`）y=300 断言 damage 含屏幕 extent、`!damage_full`、`delta.removed==1`；
   - `compose.rs` 三条测试断言 ops 区间/顺序/复用实例段，覆盖 order-only 与 stale generation；
   - gallery 负向自检 `residue_detector_self_check` + `old-region` 日志，确保“残影检测器本身失效”会 FAIL。
6. **56 条新增测试**（vs HEAD）：removal 11、pass 16、pixels 7、compose 3、overlay 4、color_picker 2、error 1、node/tree/tests 1、instance 1、damage 1、record 1、snapshot 1、window/paint 1、widgets 6；其中修复轮新增 27 条（上轮 diff 为 29 条，见 C2-02 计数漂移说明）。

---

## 6. 残余风险与限制（非 finding，供 lead 决策）

1. **验收③ 证据边界**：9 crate 1639 为自证，六 crate 已补跑 1216；`coin_market_cap_fear_and_greed_index`、`hyperliquid_wallet`、`order_book` 仍未在本轮复跑（受用户限制）；`cargo check --workspace --all-targets` 仅保证其测试可编译。§11.6 已登记，P3 阶段需补 workspace `cargo test`。
2. **跨滚动上下文 reparent**：`append_child` 的旧 damage 空间问题按 §11.3-4 登记为 P1 前置（当前生产无滚动容器内 reparent，用户不可见）；代码对“被搬子树含滚动容器”升 `scroll_reg_epoch` 只解决槽位表，不换算旧 damage。P1 实现列表复用前必须补。
3. **C-07 的像素覆盖**：virtual-list 用例为结构/scroll 表断言，未接 `PersistentPixels`；§11.3-16 声明由 `pixels.rs` 滚动重放等价覆盖，但二者不是同一场景，P1-12 应补三联像素用例（设计已写入 §11.7，我同意该处置）。
4. **真实窗口像素**：`--dump-overlay` 只查 rect/children，真实窗口 readback/截图在 P3-6；当前“残影消失”只在 headless 镜像上证明（用户已接受）。
5. **`--pixel-check` 与旧 kit API 绑定**：P1-11 删除 `kit::overlay`/`ColorPicker` 旧 API 时，若 P1-12 未迁移该门禁会编译失败（不会静默漏测），§7.5/§8 已登记。
6. **覆盖量/自检强度**：见 C2-05，属门禁退化风险，当前非 false pass。
7. **文档同步**：DoD#7 的 `kit-gallery.md` 未同步，见 C2-06(b)。

---

## 7. 结论与提交建议

- **代码质量**：无 P0/P1；上轮 14 条全部关闭，修复轮针对 R-01/R-02/R-03/R-04/R-05/R-06/R-08/R-09 的修复均有可失败回归测试锁定；门禁 6 项全绿；未发现“测试放宽/删除/ignore/新增 allow/注释/依赖”违规。
- **新问题**：P2=1（C2-01 文档声称 P0-2 无生产改动，与 `delta.rs` 生产修复矛盾）、P3=5（C2-02 数字基线漂移与 `--pixel-check` 计数重复；C2-03 measure 文本 pass 次数；C2-04 FrameRecord 空间声明过宽；C2-05 门禁覆盖量下限；C2-06 错误类型/DoD#7 文档缺项）。全部为**文档/门禁自述**问题，不涉及运行时代码正确性。
- **是否可清零提交**：**代码与门禁可提交；但“清零”不成立**——C2-01 必须改（结果表与 diff 矛盾，会影响提交说明可信度），C2-02 必须刷新（3550→3551、2716→2717、124/41→129/41、13/7/5→16/11/7、108→107 或注明口径）。C2-03…C2-06 建议在同一次文档修订中一并处理（约 10–15 分钟，无需改实现）。
- **提交建议**：先改 §11.2 P0-2 + §11.3 新增 R-04 登记（C2-01），再按 §4 实测值刷新 §5.3/§6/§9.1/§10/§11.2/§11.3-7/§11.5 与 `p0-summary`（C2-02），顺手修 C2-03/C2-04/C2-06 措辞并补 C2-05 覆盖量断言（可选）；完成后 **P0/P1/P2/P3 全 0，可提交**。若 lead 选择把 C2-03…C2-06 登记为 P3 遗留，则至少 C2-01/C2-02 修完即可提交（P2=0、P3=4 遗留需在提交说明中列出）。

---

### 附：本次审查产生的证据文件

- 门禁日志：`%TEMP%\p0-review\r2-gates.txt`、`r2-gates2.txt`
- 工作树 diff 快照：`%TEMP%\p0-review\r2.diff`（9446 行，57 文件 + extent.rs 之外的全部变更）
- 上轮报告：`.docs/popup-overlay-redesign/implementation/review-p0-conformance.md`（C-01…C-14）
