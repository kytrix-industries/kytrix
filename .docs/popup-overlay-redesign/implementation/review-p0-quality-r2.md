# P0 对抗性审查 第 2 轮（代码质量 / AGENTS 规范 / 健壮性 / 性能 / 测试可靠性）

- 审查对象：worktree `dsh-1d175414-1a41`，HEAD `71a5a5cc` + 未提交 P0 改动（57 文件 + 未跟踪 `paint/extent.rs`，`git diff --stat` = **6038 insertions / 585 deletions**；相比第 1 轮 3488/372，修复轮新增约 2550 行）
- 上一轮报告：`.docs/popup-overlay-redesign/implementation/review-p0-quality.md`（Q-01…Q-16）
- 本轮视角：①逐条复核上轮 16 条；②对修复轮新增代码做规范/健壮性/性能扫描；③复跑门禁
- **结论：P0 = 0，P1 = 0，P2 = 0，P3 = 1（文档数字回写），可清零提交**。上轮 16 条中 **12 条 fixed、1 条 partially**（Q-01，剩余 reparent 已在 §11.3-4 登记且无生产触发）、**2 条 invalid**（Q-06 经复核撤销并补测试；Q-09 我上轮有事实错误，见 §2.16）、**1 条撤回**（Q-12，复核后判定不构成缺陷）。门禁四条本机复跑全绿，另补跑 4 个此前未执行的 widget crate（612 passed）。

---

## 1. 审查范围与方法

### 1.1 本轮读的材料

| 材料 | 用途 |
|---|---|
| 上轮 `review-p0-quality.md` | 逐条复核基准 |
| 修复轮完整 diff：`error.rs`、`node/tree/{tree,arena,content,layout,dirty}.rs`、`paint/pass.rs`（6108 行）新增段、`window/paint.rs`、`render/{damage,delta,compose}.rs`、`test_support/{pixels,record,snapshot,diagnostics}.rs`、`test_support/{app,acceptance}`、`ui_kit/{overlay,color_picker}.rs`、`gallery.rs`、`widgets/*`（R-08 六处指针换算） | findings |
| 设计文档 rev.6 §4.11（558–564）、§9.1 R9（862）、§11.1–§11.7（891–985，本轮新增回写） | 文档准确性 |
| `.docs/.../p0-summary.md`（修复轮版本） | 声明核对 |
| `app/scroll.rs`（`scroll_to`/`clamp_scroll_offsets`）、`math.rs`、`layout/hit_test.rs`、`node/tree.rs::hit_test`、`app/frame.rs`（真实帧序） | 交叉验证（有限性/坐标空间/API 可见性） |

### 1.2 机械扫描（作用于 `git diff -U0` 的全部新增行）

| 扫描项 | 结果 |
|---|---|
| 任何含 `//` 的新增行（行/块/文档注释、注释代码） | **0** |
| `#[allow]` / `#[expect]` / `#[allow(dead_code)]` | 0 |
| `#[path = ...]` / `unsafe` | 0 / 0 |
| 新增第三方依赖（任何 `Cargo.toml` 改动） | 0 文件 |
| `serde_json` / 裸 `std::collections::HashMap/HashSet` / `format!`→`String` | 0 / 0 / 0（新增 `to_string()` 仅 2 处，均在测试：`error.rs:782` 的 Display 测试、`snapshot.rs:1109` 的 Debug 摘要测试） |
| `unwrap()/expect(/panic!/unreachable!/todo!/dbg!` | 318 处，逐一按其所属文件的 `#[cfg(test)]` 边界核对：**全部**位于测试模块/`*_tests.rs`/example（`node/tree/tests.rs` 本身即测试文件）；生产路径 0 |
| 新增 `debug_assert` | 0（`window/paint.rs:258` 的三段槽位平行断言为第 1 轮既有，未改） |
| 新增 `#[cfg(...)]` | 仅 1 处非测试项：`Window::scroll_slots` 的 `#[cfg(any(test, feature = "test-support"))]`（第 1 轮既有）；其余均为 `#[cfg(test)]`/`#[cfg(feature="test-support")]` 测试面 |

### 1.3 门禁复跑（本机，`CARGO_TARGET_DIR=E:\kytrix\target`，2026-11-26 21:0x）

| 命令 | 结果 |
|---|---|
| `cargo +nightly fmt --all -- --check` | **EXIT=0**，无输出 |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | **EXIT=0**，无 warning |
| `cargo test -p ui_core -p ui_kit` | **EXIT=0**：各 target 合计 **3552 passed / 2 ignored / 0 failed**（ui_core lib **2717** passed/1 ignored、frame_record 8、layout 4、snapshot 14、demo 39、ui_kit lib 769，另有一次同 lib 的单测过滤运行 1 passed） |
| `cargo check --workspace --all-targets` | **EXIT=0**，无 warning（验证 widgets 迁移 + R-08 编译面，§11.5 声明属实） |
| `cargo run -p ui_kit --example gallery -- --pixel-check` | **EXIT=0**：`checks=108 sampled_frames=45 damage_full_frames=0`，`first_visible == stable == (496,91,300×375)`，`moved=(496,4,300×375)`；输出含 §11.3-9 的语义说明与 P1 迁移提示行 |
| 额外补跑（第 1 轮遗留的未执行 crate） | `cargo test -p widget -p order_book -p hyperliquid_wallet -p coin_market_cap_fear_and_greed_index` → **EXIT=0，612 passed / 0 failed**（widget 183、order_book 134、hyperliquid_wallet 220、coin_market_cap 75） |

新增测试确认真实执行（非 0 项）：`paint::pass::tests` 新增 16 条（含 `a_canvas_with_content_outside_its_declared_bounds_damages_the_actual_content`、`a_layout_only_geometry_move_damages_the_old_extent_and_the_new_box`、`a_path_inside_a_canvas_keeps_local_op_bounds_at_125_percent`）、`app::removal_tests` 新增 11 条（含滚动 offset≠0 与嵌套滚动两条、virtual-list+scroll-table 一条）、`render::compose` 新增 3 条、`window::paint` 新增 1 条、`test_support::pixels` 5 条、`test_support::record` 1 条、`error` 2 条、`snapshot` 1 条、kit overlay 新增 3 条（含文本主导高度）。

---

## 2. 上轮 Q-01…Q-16 复核表

| # | 上轮严重度 | 判定 | 证据（本轮核实） |
|---|---|---|---|
| Q-01 | P1 | **partially（主路径 fixed）** | `arena::remove` 改为显式栈遍历并逐节点累计「相对直接父发射空间」的滚动差（`arena.rs:369-431`：`scroll_extras` + `paint.translate(-extra)`），覆盖「父自身 scroll」与「子树内嵌套滚动」。测试：`removal_tests.rs:1633`（offset=300 单层）、`:1836`（外层 300 + 内层 50 嵌套）。**剩余**：跨滚动上下文 reparent（`append_child` 在两容器间搬子树）旧 damage 用新上下文 `total` 发射——已登记 §11.3-4 且无当前生产调用方（P3 级残余，见 §6.2 R2）。 |
| Q-02 | P2 | **fixed** | `PaintReport.order_rebuilt` 标志（`pass.rs`）→ 三处构造点；`window/paint.rs:168` 条件改为 `!full_paint && !report.order_rebuilt && (structure_changed \|\| !removed.is_empty())`；新增 `a_structural_frame_rebuilds_the_order_once`（`window/paint.rs:460`）对「插入 +1 / 删除 +1 / 首帧 0」三断言。 |
| Q-03 | P2 | **fixed（静态文本），残余已登记** | `Overlay::measure` 现在先 `App::refresh_text` 再 `compute_layout` 再 `place_panel`（`overlay.rs:811-826`）；新增 `text_dominated_content_measures_before_the_first_visible_frame`（`overlay.rs:1734`）：120 px 宽换行文本、断言高度 > 8 px（确保文本真的撑高）且首可见帧 rect == 稳定帧。**残余**：`bind_text` 内容在 style pass 才求值 → 已登记 §11.3-10，由 P1-2 settle 接管。 |
| Q-04 | P2 | **fixed** | `color_picker.rs:1338-1342`：`if let Err(error) = inner.overlay.measure(app) { close_now; return Err(error.into()) }`；`ColorPicker::open` 签名改为返回 `Result`（§11.4）。 |
| Q-05 | P2 | **fixed** | `EngineError::PixelHarness { cause: PixelHarnessCause }`（`error.rs:264-288`），四个具名原因 + `Display`；harness 四处改用（`pixels.rs:75/94/222/238`）；`error.rs` 新增 variant-list 与 Display 文本测试（`:424`、`:770-785`）。 |
| Q-06 | P2 | **invalid（判定撤销，已补测试）** | `place_subtree` 仍只并 `old.bounds ∪ painted`（`layout.rs:284-296`）。经复核：Geometry 节点的绘制/内容 damage 由 `transform`（绝对坐标）决定、不随布局移动；节点盒新位置的 damage 由该节点自身 emit（`emit_node_damage`/背景 quad）覆盖。新增 `a_layout_only_geometry_move_damages_the_old_extent_and_the_new_box`（`pass.rs:3120`）断言旧盒、新盒、几何块三者都被 damage 覆盖 → 上轮的「潜在漏洞」不成立。 |
| Q-07 | P2 | **fixed** | 新增 `assert_baseline_opaque`（`pixels.rs:559-575`），在 4 条像素用例前置断言（`:647/702/773/905`）；排除最外 1 行/列并在 §11.3-18 记录成因（渲染器既存 3 个 alpha=218 边缘像素），gallery 侧的 `baseline-opaque` 为独立严格实现（不引用该排除）。 |
| Q-08 | P3 | **fixed** | 设计 §4.11:558 改为「L 空间 `LogicalRect` + 固定 1 物理 px pad，**不是** DeviceRect，见 §11.3-4」，并补 scroll 差换算与两条侧表说明；§4.11:562/563 补 Canvas 原点口径与 C-10 实际并集语义；R9（:862）更新为 **44 B/节点**（`baked_screen_bounds` 20 + `painted_extents` 24，40k≈1.7 MB）。 |
| Q-09 | P3 | **invalid（我上轮事实错误）+ 另一半点 fixed** | ①「仓库内不存在 `LaneStore`」**错误**：`render/delta.rs:353` 就是 `LaneStore`（本轮 diff 亦改其 stale-generation 逻辑），上轮结论作废并致歉。②「108 checks 统计口径」已修正：§11.2 P0-6 行、§11.3-9、`p0-summary.md:13` 均写明「108 = 45 采样帧逐帧 + 20 空闲帧 + 约 30 条独立不变量」。 |
| Q-10 | P3 | **fixed** | `SnapshotImage` 改手写 `Debug`（`snapshot.rs:194-210`：width/height/bytes/fnv1a，O(n) 哈希仅在失败路径），`PixelFrame` 继承摘要；新增 `snapshot_image_debug_is_a_summary`（`snapshot.rs:1106-1120`）断言 <200 字节且不含 `rgba8: [`。 |
| Q-11 | P3 | **fixed** | `screen_baked` → `needs_screen_bake`（`content.rs:711`）、`stored_screen_bounds`/`current_screen_bounds`（`:720-733`）、`PaintedExtent::bounds()` → `layout_bounds()`（`extent.rs:22`）、`extent_shift` → `layout_extent_shift`（`pass.rs:749`）。 |
| Q-12 | P3 | **撤回（not-a-defect）** | `note_shadow`（`pass.rs:1511`，单调用点）与 `PersistentPixels::readback`（`pixels.rs:127`，仅测试消费）仍在；复核后判定：`readback` 是 §11.4 明确列入的 harness API、`note_shadow` 命名的是「shadow pad 物理外扩」这一独立语义，均不构成 AGENTS §6 意义上的「无意义抽象」；建议 P1 清理时顺手内联，不作为缺陷。 |
| Q-13 | P3 | **fixed** | gallery 增加 `idle-no-animation` 前置断言（`gallery.rs:3340`）、idle 步长 137 ms（与常见动画周期互质）、`--pixel-check` 迁移提示行（`:3245`）、§11.3-9 与 §8/§11.7 均登记 P1-12 必须重写并保留命令/退出码。 |
| Q-14 | P3 | **fixed** | `pass.rs::extent_probe_points` 与 `removal_tests::assert_damage_covers_extent` 都改为 **4 角 + 中心 = 5 点**；`pixels.rs` 已无硬编码绝对像素坐标（改用 `point_inside`/节点 bounds 推导，`:576+`）。 |
| Q-15 | P3 | **fixed** | `measure` 与 `place` 共用 `place_panel`（`overlay.rs:848-891`），`place()` 仅转发；`frame_at_open`/`measured` 机制整体删除（全文件 0 引用）；reveal 移入 `open`（`overlay.rs:806`），语义变化登记 §11.3-3。 |
| Q-16 | P3 | **fixed** | `paint.rs` 现按「公开 mod 字母序 + crate 私有 mod 独立分组（extent/instance/pass）」组织，组内有序。 |

---

## 3. 新 findings（修复轮代码）

**P0 = 0，P1 = 0，P2 = 0，P3 = 1。**

### Q2-01（P3）§11 的机械计数未随修复轮更新

- **位置**：设计 §11.3-7（canvas 构造点计数）与 §11.5（测试基线计数）。
- **证据**：
  1. §11.3-7 写「实际 `Canvas::new` 构造点 **124/41**」；本轮机械计数（`\bCanvas::new\(`，排除 `ChartCanvas::new` 等）为 **129 处 / 41 文件**——**文件数完全一致**，构造点 +5，与修复轮新增测试（`record.rs` 的 canvas 空间用例、`pass.rs` 的 content 外延/path 局部坐标用例等）吻合。
  2. §11.5 写 `3550 passed`（ui_core lib **2716**）；本轮实测 `3552 passed`，其中 ui_core lib **2717**——多出的 1 条是修复轮新增的 lib 用例 `snapshot_image_debug_is_a_summary`（`snapshot.rs`）。
- **影响**：文档可信度（数字可核对但滞后）；不影响任何结论或门禁。
- **建议**：把 §11.3-7 改为 129/41、§11.5 改为 3552/2717 并把新增 2 条 lib 用例计入明细。
- **是否阻塞提交**：不阻塞（建议提交前一并改掉，即可达成全级别 0）。

---

## 4. 修复轮新增代码的专项扫描

| 新增面 | 规范 | 健壮性 | 性能/内存 | 结论 |
|---|---|---|---|---|
| `EngineError::PixelHarness` + `PixelHarnessCause`（`error.rs:264-288`） | 强类型、`Display` 具名，无字符串错误 | 变体在默认构建下不可达（只有 `feature=test-support` 构造），但枚举变体不产生 dead_code；不影响匹配穷尽性（clippy 绿） | 零成本 | 合规。唯一可讨论点：test-support 专用变体进入公开错误枚举（无 cfg 支持），属可接受取舍 |
| `record_primitives` 递归（`record.rs:254-330`） | 仅 test-support；无注释/无 `unsafe` | 递归深度 = Canvas 嵌套层数（构造深度，通常 ≤2–3）；无环可能（`Canvas` 构造后不可变、`Arc` 无内部可变性）；非有限值不会进入（`primitive_kind` 只做平移） | **每提交帧 O(全部图元，含 Canvas 内层)**；`FrameRecord.primitives` 无上限。§11.3-20 已把「扁平化绘制流、统一 lane 空间」写成契约；`canvases`/`quads` 保持顶层不透明语义（有测试断言）→ 语义自洽 | 合规；仅测试路径成本，实测无回归（见 §5） |
| `canvas_content_extent` 递归并集（`pass.rs:1444-1495`、`:1759`） | 单一职责、无注释 | NaN/Inf：Geometry 走 `transformed_chunk_aabb` 哨兵 + `aabb[2]>=aabb[0]` 守卫；quad/glyph/shadow 走 `Rect::union`（`is_empty` 已含 non-finite 判定）→ 不会污染 extent；嵌套 canvas 的 `offset` 累加（f32，深度有限） | **每次 Canvas lane 重建多一次 O(图元数) 矩形并集**（lane 复用不执行）；与实例编码同阶、常数因子 ≤1×。screen-baked 的 Canvas lane 会在祖先滚动帧重建，因此「滚动图表」每帧多一遍该扫描；无 bench 数据（P3-5 补） | 合规；已登记成本（§11.3-17） |
| `arena::remove` 滚动差遍历（`arena.rs:369-431`） | 用显式栈替代递归（更深也不爆栈），无注释 | `block`/`scroll_extras` 平行 push（同循环内成对）；`LogicalPoint` 累加只依赖 `scroll_slot().offset`，而 `App::scroll_to` 强制有限 + 每帧 `clamp_scroll_offsets`（`app/scroll.rs:104-108/153-178`）→ NaN 不可达；`#[cfg(test-support)] note_scanned` 只在测试面 | 每次删除 O(子树大小)（原 `for_each_subtree` 也是 O(子树)），多一次 `SmallVec` 分配；无损 | 合规 |
| `order_rebuilt` 标志（`pass.rs`/`window/paint.rs:168`） | 无注释、无新抽象 | Full 模式返回 false（window 层不重复重建）；早退分支两处显式 `order_rebuilt: false` | 消除第 1 轮 2×O(n) DFS 与 `order_rebuilds` 翻倍 | 合规 |
| R-08 指针换算（六 crate） | 复用既有 public API `NodeTree::hit_test -> HitResult{node, local_point}`（`node/tree.rs:618`、`layout/hit_test.rs:312`），未新增引擎 API | 全部输入路径都先 `note_pointer` 再 `contains/attribution_contains/zoom_at`（`global_shipping_chokepoint/src/view.rs:296-354`）；`map.rs` 的 `screen_origin` 现由 `position - hit.local_point` 反推（`map.rs:759-781`） | 每次指针 O(祖先链)（hit_test 既有成本）；无逐帧成本 | 合规（残余见 §6.2 R6） |
| gallery 全区域扫描 + 负向自检（`gallery.rs:2928-3070`） | 无注释；失败信息 `format_smolstr!` | `compare_region` 逐像素、边界 `min(width/height)` 保护；`residue_detector_self_check` 先取未排除区域首个像素、内存注入品红并断言检测器必须报错（真正的负向自检） | 每次移动阶段多扫「旧浮层区域」（约 176k 像素@DPR1.25）+ 2 次自检扫描；实测总时长未变（§5） | 合规 |
| §11 文档回写（891-985） | — | 20 条偏差登记与代码逐条对得上（抽查 C-02/Q-01 空间与内存、C-10、C-06/C-07 落点、R-06、R-08、R-09、Q-07 断言范围） | — | 准确（仅 Q2-01 两处计数滞后） |

---

## 5. 性能 / 内存观察（本轮实测）

| 项 | 数值 | 对比/说明 |
|---|---|---|
| `--pixel-check` 进程（直接跑二进制，两次） | **3.05 s / 3.90 s wall；3.81 s / 4.55 s CPU** | 第 1 轮同法 3.61 s → **无回归**。修复轮新增「全区域扫描 + 负向自检 + 137 ms 空闲采样」被 45 采样帧的固定成本摊薄。（中途一次 6.2 s 的读数系并行 cargo 负载与管道开销，非代码回归） |
| ui_core lib 测试二进制（直接跑，2717 用例） | **7.92 s wall / 34.5 s CPU** | 第 1 轮 cargo 报 9.55 s（2699 用例）→ **无回归**；`cargo test` 日志里 25.34 s 系本机同时有其他 agent 的 cargo/rustc（实测存在 2 个 cargo 进程），非套件退化 |
| `cargo check --workspace --all-targets` | EXIT=0 | 编译面覆盖 widgets 迁移与 R-08 |
| 额外 4 个 widget crate 测试 | 612 passed / 0 failed | 补上第 1 轮 R4 的一部分证据缺口 |
| 侧表内存 | **44 B/节点**（20 + 24），40k≈1.7 MB | 设计 R9 已更新为实测值（第 1 轮 Q-08 关闭） |
| Canvas lane 重建附加成本 | 一次 O(图元数) 并集 | 与实例编码同阶；无 bench（P3-5 应纳入 `perf`/`layout_postpass`） |
| `FrameRecord.primitives` 增长 | 含 Canvas 内层图元，无上限 | 仅 `feature=test-support`；单帧瞬时内存 = O(图元数)×~40 B；gallery/acceptance 场景未见退化 |
| 破坏性行为变化 | `Overlay::open` 现在自带 measure+place+reveal；`ColorPicker::open` 返回 Result | 已登记 §11.3-3/§11.4；kit 测试全绿 |

---

## 6. 测试可靠性评估

### 6.1 本轮加强（已核实）

1. **像素门禁的断言强度**：`extent_probe_points` 5 点（4 角 + 中心）消除 L 形缺口漏检；gallery 移动阶段改为「旧浮层区域逐像素对打开前基线」而非 2 个内缩角点（`gallery.rs:2928-2974`）。
2. **负向自检**：`residue_detector_self_check` 内存注入品红像素并断言比较器必须报错（`gallery.rs:3050-3070`）→ 检测器失效时门禁会红而不是静默 PASS。
3. **半透明累积防护**：库内像素测试补 `assert_baseline_opaque`；gallery 保留独立严格基线检查。
4. **动画/采样混叠**：idle 循环前断言 `!animations_running`，步长 137 ms 与常见周期互质（§11.3-9）。
5. **无真实睡眠**：全部用 `TestApp::advance` 虚拟时钟（`gallery.rs:3080/3345`）；GPU 依赖与既有 snapshot 测试同源（`gpu_guard` 只互斥不 skip）。
6. **跨 crate 覆盖**：R-08 六 crate 由 lead 补跑 1216 passed（§11.5），我另补 4 个未覆盖 crate 612 passed。

### 6.2 残余风险（非缺陷，均已登记或无法在本机判定）

- **R1 真实窗口像素仍无自动化**：headless 镜像 + 真实窗口仅 rect 断言（§11.3-8，P3-6 补三端方案）。
- **R2 跨滚动上下文 reparent**：`append_child` 在 A/B 两滚动容器间搬子树时旧 damage 差新旧 `total`（§11.3-4，当前无生产调用方，P1 前必须补换算）。
- **R3 `bind_text` 内容首帧**：measure 无 style-pass 求值入口，由 P1-2 settle 接管（§11.3-10）。
- **R4 独立 Geometry 节点升格 full damage**（§11.3-14）：I6 只对不含独立 Geometry 的 popup 成立；有专门测试记录现状。
- **R5 workspace 全量 `cargo test` 仍未跑**（AGENTS §8.1）：本轮已补 4 个 crate；`crates/kytrix`、其余 widget 相关 crate 仍缺（§11.6-1/2 已登记）。
- **R6 `map.rs` 的 `screen_origin` 初值**为 L 空间 `bounds.origin`（`map.rs:315`），仅在 `note_pointer` 命中后自愈；现行调用序（view.rs 先 `note_pointer`）下安全，但若命中落在子节点上则不刷新——当前 map 节点为叶子，未触发；建议 P1 用 `hit_test().local_point` 一并修正初值。
- **R7 `--pixel-check` 绑定旧 `Overlay`/`ColorPicker` API**：P1-12 必须迁移（§11.3-9/§11.7 已登记）；kit 首帧测试 4–6 条同理。
- **R8 GPU-only 测试面**：无 GPU 环境下像素/pixel-check 会失败而非 skip（与既有 snapshot 门禁同源，属项目既有策略）。

---

## 7. 结论：是否可清零提交

**可以。** 依据：

1. **P0/P1/P2 = 0**：修复轮把第 1 轮唯一的 P1（Q-01 滚动容器删除 damage）在主路径修复并补了两条精确回归（单层 offset≠0、嵌套滚动），剩余 reparent 已在 §11.3-4 明确登记且无生产触发；其余 P2 全部关闭（Q-02/03/04/05/07，Q-06 经复核撤销并补测试）。
2. **规范**：零注释、零 `#[allow]/#[expect]`、零新依赖、零 `#[path]`、生产路径零 `unwrap/expect/panic`、错误强类型补齐、无新增 `debug_assert`、无新 cfg 放宽（除第 1 轮既有的 `scroll_slots`）；`fmt`/`clippy -D warnings`/`test`/`check --workspace --all-targets` 四条本机复跑全绿。
3. **门禁真实性**：`--pixel-check` 现在同时具备「全区域基线扫描 + 注入故障负向自检 + 首帧==稳定帧 + damage_full=0」四重证据，且实测 `108 checks / 45 帧 / damage_full_frames=0` 与 §11.5 一致。
4. **剩余 P3 仅 1 条**（Q2-01：§11.3-7 的 124→129、§11.5 的 2716→2717），属文档数字回写，一行修正即可达成「全级别 0」；不影响代码、门禁或提交结论。
5. 上轮我的 Q-09 存在事实错误（`LaneStore` 确实存在），已在 §2.16 更正；Q-12 撤回。除该两点外，其余 14 条中 12 条已修复、1 条主路径修复（Q-01）、1 条经复核撤销（Q-06）。

**提交前建议（非阻塞）**：①改 §11.3-7/§11.5 两个数字；②在 §11.3-4 的 reparent 条目后补一句「P1 前为 `append_child` 增补滚动差换算 + 回归用例」；③若时间允许，把 `crates/kytrix`/剩余 widgets crate 的 `cargo test` 日志补进 §11.6，以完全满足 AGENTS §8.1。
