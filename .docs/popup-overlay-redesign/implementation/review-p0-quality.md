# P0 对抗性审查（代码质量 / AGENTS 规范 / 健壮性 / 性能 / 测试可靠性）

- 审查对象：worktree `dsh-1d175414-1a41`，HEAD `71a5a5cc` + 未提交 P0 改动（48 文件 + 未跟踪 `crates/ui_core/src/paint/extent.rs`，`git diff --stat` = 3488 insertions / 372 deletions）
- 审查者视角：代码质量、AGENTS.md 逐条对照、健壮性、性能/内存/并发、测试可靠性
- 结论：**P0 = 0，可提交**（前提：本文 P1/Q-01 与已披露偏差保持跟踪，P2 项在 P1 起点前处置）。门禁四条在本机复跑全绿，`--pixel-check` 实测 PASS 且 `damage_full_frames=0`。

---

## 1. 审查范围与方法

### 1.1 读过的材料

| 材料 | 用途 |
|---|---|
| `AGENTS.md`（172 行） | 逐条对照（§1–§8） |
| `.docs/popup-overlay-redesign/implementation/p0-summary.md` | 声明与偏差核对 |
| `.docs/popup-overlay-redesign-design.md` rev.6：§4.11（554–577）、§6 P0（689–706）、§7.2（781–788）、§9 风险（833–847）、§10 DoD（864–872）、§11（876–878） | 契约/DoD/文档准确性 |
| 完整 `git diff`（48 文件）逐块阅读；新文件 `paint/extent.rs` 全文 | findings |
| `paint/pass.rs`（5845 行）相关函数全文：`paint_node`/`rebuild_lane`/`hide_subtree`/`paint_geometry_node`/`PrimitiveEmitter` | 空间换算与 damage 正确性 |
| `node/tree/*`、`window/paint.rs`、`render/{damage,compose,delta}.rs`、`test_support/pixels.rs`、`ui_kit/{overlay,color_picker,spinner}.rs`、`examples/gallery.rs` | 同上 |
| `math.rs`（`Transform::then`/`LogicalRect::union/is_empty`/`DeviceRect::intersect`）、`render/compositor.rs::plan`、`window/frame.rs`（`settle_frame`）、`app/frame.rs`（真实帧序） | 交叉验证 |

### 1.2 机械扫描（作用于 `git diff -U0` 的 3834 条新增行）

| 扫描项 | 结果 |
|---|---|
| 任何含 `//` 的新增行（行/块/文档注释、注释代码） | **0 条** |
| `#[allow]` / `#[expect]` / `#[allow(dead_code)]` | 0 |
| `#[path = ...]` | 0 |
| `unwrap()/expect(/panic!/unreachable!/todo!/dbg!` | 162 条，逐条定位后**全部**位于 `#[cfg(test)] mod tests`、`*_tests.rs`、或 example（`gallery.rs`），生产路径 0 条 |
| `format!`/`String::from`/`.to_string()`/`serde_json`/裸 `HashMap` | 0（新代码一律 `format_smolstr!` / `SmolStr::new_static`） |
| 新增第三方依赖（任何 `Cargo.toml` 改动） | 0 文件 |

### 1.3 复跑门禁（本机，`CARGO_TARGET_DIR=E:\kytrix\target`）

| 命令 | 结果 |
|---|---|
| `cargo +nightly fmt --all -- --check` | EXIT=0，无输出 |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | EXIT=0，无 warning |
| `cargo test -p ui_core -p ui_kit` | EXIT=0：3531 passed / 2 ignored / 0 failed（ui_core lib 2699 + ui_kit lib 766 + 集成 66） |
| `cargo check -p ui_core --features test-support --all-targets` | EXIT=0，无 warning（验证 cfg 放宽后非 test 构建不产生死代码告警） |
| `cargo run -q -p ui_kit --example gallery -- --pixel-check` | EXIT=0，`checks=108 sampled_frames=45 damage_full_frames=0`，`first_visible == stable == (496,91,300,375)` |
| 直接运行 `target\debug\examples\gallery.exe --pixel-check` | EXIT=0，**3.61 s**（D3D12 WARP，adapter=`Microsoft Basic Render Driver`） |

新增测试确认真实执行（非 0 项）：`test_support::pixels::tests` 5 条、`app::removal_tests` 新增 7 条、`paint::pass::tests` 新增 13 条（含 Canvas / extent 系列）全部出现在 `cargo test` 输出并 `ok`。

---

## 2. Findings

严重度：P0=正确性缺陷/硬要求违反/回归/门禁虚假通过；P1=重大健壮性或测试可靠性缺口；P2=次要问题；P3=风格/文档。

汇总：**P0 = 0，P1 = 1，P2 = 6，P3 = 9**（共 16 条，ID Q-01…Q-16）。

### P1

#### Q-01 删除/移动子树在「直接父节点即滚动容器且 offset≠0」时 damage 偏移错误（残影）

- **位置**：`crates/ui_core/src/node/tree/arena.rs:400-417`（`remove()` 把子树 `painted_extent` 与 `layout.bounds` 并入 `parent.damage`）；`crates/ui_core/src/node/tree/layout.rs:288`（`place_subtree` 同通道）；触发点 `crates/ui_core/src/paint/pass.rs:1621-1628`（`emit_node_damage` 用 `negate(self.offset)`，`self.offset = total`）。
- **证据（代码推导）**：
  1. `painted_extent` 存在**绝对布局空间 L**（`pass.rs:743` `extent_shift = total + negate(slot_offset)`，`extent.rs:6-19`），即未减任何滚动的内容坐标。
  2. `remove()` 把 L 空间的 extent 并进父节点的 `damage` 字段（`arena.rs:406-417`）；`place_subtree` 同理（`layout.rs:288`）。
  3. 父节点 lane 重建时 `emit_node_damage` 以 `negate(self.offset)` 发射，而 `offset` 是**该父节点自己的祖先累计滚动量**（`rebuild_lane` 的 `total`，`pass.rs:736`）——**不含父节点自身的 scroll offset**（自身 scroll 只进入子节点的 `total`，见 `pass.rs:459-470`）。
  4. 因此当「父 == 滚动容器」时，damage 坐标 = `L − 祖先滚动`，正确屏幕坐标 = `L − 祖先滚动 − 自身滚动`，两者相差**自身 scroll offset**；再经 `clip.covering`（容器视口，屏幕空间）裁剪后：offset 小 → 部分错位（漏画一条带），offset ≥ 视口高 → damage 被判空、整块丢弃 → 旧像素残留。
  5. 现有覆盖仅 offset = 0：`app/removal_tests.rs:1553-1622`（`adding_and_removing_scrolled_content_stays_incremental`）创建了滚动容器但从未 `scroll_to`；`test_support/pixels.rs:944-1034` 的滚动用例只滚动（不删除滚动容器子节点）。
- **影响**：P1 的 Select/Dropdown/虚拟列表行回收在 offset≠0 时删除行会留残影；与 DoD §10.1「打开/移动/关闭/重开全程无旧像素」在滚动列表路径冲突。**P0 主场景（popup 不滚动）不受影响**，且 `p0-summary.md:25` 偏差 2 已披露（但只写了 remove，`place_subtree` 共用同一通道，等同）。
- **建议修法**：聚合到父节点前先换算到父节点的发射空间（减去父节点自身 scroll 后再进 `damage`），或改走屏幕空间通道（`Window::pending_damage` 同语义），或在 `emit_node_damage` 处对「父为滚动容器」补 `-own_scroll`。补一条 offset≠0 的删除/移动用例（`make_scroll` + `scroll_to(0,300)` 后 `remove_node`，断言 `damage ⊆ 视口` 且覆盖旧行屏幕 rect）。
- **是否阻塞 P0**：不阻塞（主场景 offset=0，且已披露）；**必须在 P1 虚拟列表复用前修掉**。

### P2

#### Q-02 结构帧的 order 重建被执行两次（2×O(n) DFS），`order_rebuilds` 计数翻倍

- **位置**：`crates/ui_core/src/paint/pass.rs:158-166`（增量 pass 内 `order_changed` → `rebuild_order_only` + `scene.set_order` + `note_order_rebuild`）与 `crates/ui_core/src/window/paint.rs:166-176`（`!full_paint && (structure_changed || !removed.is_empty())` → 同样三连）。
- **证据**：插入节点时 `rebuild_lane` 的 `created == true` → `context.order_changed = true`（`pass.rs:786-788`），而 `structure_changed` 在同帧为真（`window/paint.rs:82` 早在 paint 前捕获），两处条件同时成立 → `rebuild_order_only` 与 `note_order_rebuild`（`tree.rs:724-726`）各执行两次。40k 节点用例（`window/paint.rs:384-455`）里单次 DFS 就是 ~40k 次 `tree.get`，popup 开/关（结构帧）会重复一遍。
- **影响**：性能（P0-7 优化的正是结构帧）与可观测性（`PaintCounters.order_rebuilds` 是 acceptance/headless 指标，`examples/acceptance/headless.rs:61-75` 会报告翻倍值）。
- **建议修法**：在 `PaintReport` 加 `order_rebuilt: bool`（或复用现有 `context.order_changed`）并在 window 层条件里排除；或 window 层只处理 `removed`（pass 内无法感知的释放 lane 情形），其余交给 pass。加一条断言 `paint_counters().order_rebuilds` 结构帧只 +1 的用例（当前唯一断言 +1 的 `tests/frame_record.rs:390-397` 是非结构帧）。
- **是否阻塞 P0**：不阻塞（无正确性影响）。

#### Q-03 `Overlay::measure` 只跑 `compute_layout`，与真实帧序（先 shape 文本）不一致；P0-8 的「首帧 == 稳定帧」对文本高度内容无保证且无测试

- **位置**：`crates/ui_kit/src/kit/overlay.rs:814-864`（`measure` 内唯一布局调用是 `target.compute_layout()`，`overlay.rs:827`）；真实帧序见 `crates/ui_core/src/app/frame.rs:128-196`：`layout_if_dirty`（compute_layout）→ **`layout_text`** → `rewindow_virtuals` → `clamp_scroll_offsets` → `resolve_canvas_text`（可能再 `layout_text`）→ paint。
- **证据**：`measure` 在 `open()` 内被调用（`overlay.rs:807-810`）时，`content` 闭包刚把节点插进树，文本从未 shape；taffy 只能用缓存/启发式测量结果，面板高度在随后的 `layout_text` 才确定（设计 §7.1.1「内容高度在 layout_text 后才确定」正是这条）。`revealed` 的置位条件是 `frame_index != frame_at_open`（`overlay.rs:903-914`），所以文本重排若发生在 reveal 之后，首个可见帧仍可能取到测量帧的过期 rect。新增两条首帧用例（`overlay.rs:1655`、`color_picker.rs:3302`）的内容高度都是常量（`build_content` 固定尺寸 / ColorPicker 主体为固定尺寸画布），不覆盖文本主导高度。
- **影响**：P0-8 的收益对「文本主导高度」的浮层（含 String 内容的 Tooltip/Menu）可能失效，表现为首可见帧一次位移/闪烁；且这是 P1 settle 需要接管的点（§4.3/P1-2）。
- **建议修法**：先在 P0-9（或 P1-2 起点）做判别实验：`overlay.open` 内容用多行 `TextContent`（wrap，高度随 available width 变化），断言首可见帧 rect == 稳定帧 rect；若失败，`measure` 需复用帧管线（`App::measure_window` 之类内部入口：compute_layout + layout_text + clamp）而不是只 `compute_layout`。
- **是否阻塞 P0**：不阻塞（属覆盖缺口，非已证实的可见缺陷）。

#### Q-04 `ColorPicker::open_panel` 静默丢弃 `Overlay::measure` 的错误

- **位置**：`crates/ui_kit/src/kit/color_picker.rs:1338`：`let _ = inner.overlay.measure(app);`
- **证据**：`measure` 返回 `Result<bool, EngineError>`，Err 只可能来自 `compute_layout()`（`overlay.rs:827`），但 `open()` 内部对同一调用做了 `close_now + Err` 处理（`overlay.rs:807-810`），两条调用点策略不一致；此处丢弃后既无诊断也不回收，后续 `open_callback` 继续执行。
- **影响**：布局失败时面板留在「已 open、未测量」状态 → 位置错误且无错误可观察；违反 AGENTS §3（上层应能按错误类型处理）与设计 §4.13「禁止静默」精神。
- **建议修法**：`match inner.overlay.measure(app) { Ok(_) => …, Err(_) => { inner.overlay.close_now(app); return; } }`（或把 measure 的失败并入 `open` 的一次调用）。
- **是否阻塞 P0**：不阻塞。

#### Q-05 像素 harness 用 `EngineError::AppInit` 表达非初始化语义（弱类型/语义错位）

- **位置**：`crates/ui_core/src/test_support/pixels.rs:78-83`（"persistent pixels need a committed frame"）、`99-104`（"must not replay across a surface resize"）、`231-235`（"requires an inline (headless) window"）、`249-251`（"does not fit a readback target"）。
- **证据**：`EngineError`（`crates/ui_core/src/error.rs`）已有 `FramePipeline { detail }`、`Composition { detail }` 等分类；`AppInit` 语义是应用初始化失败，与「像素 harness 前置条件不满足」无关。调用方（gallery `pixel_check_first`、`observe`）只能把字符串打到 stdout。
- **影响**：AGENTS §3 强类型错误要求未满足（同类条件无法被上层分类处理/多语言化）；P1 若把这些 harness 提升为正式 API 会继承该错误。
- **建议修法**：新增具名变体（如 `EngineError::PixelHarness { cause: PixelHarnessCause }`，`enum PixelHarnessCause { NoCommittedFrame, SurfaceResized, NotInlineWindow, UnrepresentableSurface }`），并在 `test_support` 内统一。
- **是否阻塞 P0**：不阻塞。

#### Q-06 `place_subtree` 不再并新位置（`abs`）进 damage：Geometry 节点「只动布局不动 transform」时新位置无 damage

- **位置**：`crates/ui_core/src/node/tree/layout.rs:288`：`target.damage = target.damage.union(old.bounds).union(painted)`（旧行为含 `abs`，见 `git diff`）。
- **证据**：新位置 damage 现在完全依赖 lane 重建时的图元 damage。Canvas/文本/图片/阴影路径都按新 `bounds` 发射（`pass.rs:1698-1710`、`pass.rs:1734-1791`），成立；但 `NodeKind::Geometry` 的 chunk damage 由 `geometry_chunk_damage(transform, chunk_bounds, total)` 计算（`pass.rs:1115-1136`），`transform` 是**应用显式设置**、不随布局移动变化，因此布局移动时新位置算出的仍是旧位置附近的 rect。`App::set_geometry_transform` 会自行补 `old ∪ new` damage（`app/geometry.rs:76-87`），所以只有「只改布局、不重设 transform」的用法会漏。
- **影响**：Geometry 节点（仅 examples/acceptance + 测试使用；`crates/widgets` 全走 `Canvas::push_geometry`，偏移由 `CanvasDraw.offset` 提供，不受影响）在验收场景里移动图表可能出现新位置漏画。当前无已知生产触发。
- **建议修法**：在 `place_subtree` 保留 `union(abs)`（成本是一条 rect），或为 Geometry 节点在布局移动时把 `bounds` 的新位置并入 chunk damage（`union(bounds.translate(total))`）。
- **是否阻塞 P0**：不阻塞。

#### Q-07 像素测试用半透明阴影内容，但未断言基线不透明；增量/全量对比 `span<=1` 存在跨驱动 flake 风险

- **位置**：`crates/ui_core/src/test_support/pixels.rs:775-801`（`shadowed()` + `mount_shadowed_canvas_subtree`）、`837-905`（`adding_and_removing_a_shadowed_canvas_subtree_stays_incremental` 用 `assert_matches_baseline`，阈值 `DAMAGE_PAD as u8 = 1`，`pixels.rs:552-561`）。
- **证据**：`gallery --pixel-check` 显式做了基线不透明检查并解释原因（`gallery.rs:3191`：半透明基线上增量重绘会在覆盖边缘累积 alpha），而库内像素测试没有等价断言；一旦 damage 集合出现部分重叠（`DamageAccumulator` 只做包含关系合并，`render/damage.rs:65-77`；`plan()` 的 `coalesce` 只做并集不做重叠消解，`render/compositor.rs`），同一像素会被画两次 → 半透明部分 alpha 双混合 → 与全量基线差异 > 1。
- **影响**：测试可靠性（跨平台/驱动、或未来 damage 集合形态变化时出现难定位的偶发失败），当前在 WARP 下稳定通过。
- **建议修法**：把这 5 条像素测试的基线不透明断言补上（复用 gallery 的检查），或对 `assert_matches_baseline` 使用与 gallery 一致的容差并把「damage 集合无相互重叠」作为显式前置断言。
- **是否阻塞 P0**：不阻塞。

### P3

#### Q-08 设计文档 §4.11 / R9 与实现不一致（`painted_extent` 空间与内存估算）

- **位置**：设计文档 `:558`「持久记录 `painted_extent: Option<(DeviceRect, PhysicalPx)>`」与 `:847` R9「设备像素 rect+pad（~24B/节点）」；实现为 `LogicalRect + 固定 physical_px(1.0)`（`paint/extent.rs:6-19`），且是**两张**侧表（`tree.rs:344-345`：`baked_screen_bounds: Vec<Option<LogicalRect>>` 20B/节点 + `painted_extents: Vec<Option<PaintedExtent>>` 24B/节点 ≈ **44 B/节点**，40k 节点约 1.7 MB）。
- **证据**：`p0-summary.md:15` 已声明「绝对布局空间 L」，但设计文档 §11（`:876-878`）仍为空、§4.11/R9 未回写。
- **影响**：文档准确性与内存估算（R9 的缓解措施按 24B 决策）；不是代码缺陷。
- **建议修法**：P0 收口时把「空间选 L、pad 固定 1px、两张侧表 44B/节点」写进 §11 偏差登记，并更新 R9 实测值。
- **是否阻塞 P0**：不阻塞。

#### Q-09 `p0-summary.md` 术语与统计口径

- **位置**：`p0-summary.md:19`（"ops 由 **LaneStore** removal/order 变化触发 `encode_scanned` 重建"）；`:18`（"108 checks"）。
- **证据**：仓库内不存在 `LaneStore`（实际是 `paint::scene::Scene` + `paint::instance::InstanceEncoder` + `render::compose::WindowComposer`）；`108` 里大量是逐帧重复断言：`gallery.rs:3255-3262` 的 20 次 `idle-frames`、`gallery.rs:2811-2900` 每帧一条 `changed-outside-damage`（实测 `sampled_frames=45`）。真正独立的断言约 30 条。
- **影响**：读者会把 108 当成 108 条独立不变量；术语指向不存在的类型。
- **建议修法**：改为「108 条断言（含 45 帧逐帧 damage 断言 + 20 帧空转断言）」并替换 `LaneStore` 为真实类型名。
- **是否阻塞 P0**：不阻塞。

#### Q-10 `PixelFrame`/`SnapshotImage` 的 `Debug` 会把整幅像素缓冲打进失败信息

- **位置**：`crates/ui_core/src/test_support/pixels.rs:43-53`（`#[derive(Clone, Debug, PartialEq, Eq)] pub enum PixelFrame { … SnapshotImage … }`）；`SnapshotImage` 派生 `Debug` 且含 `rgba8: Vec<u8>`（`test_support/snapshot.rs:187-192`）；消费点 `ui_kit/examples/gallery.rs:3150-3160`（`{other:?}`）与 `pixels.rs:694, 762, 921` 的 `assert_eq!(replay(...), PixelFrame::Idle)`。
- **影响**：一次失败即向 stdout 打印 ~4 MB（1280×800×4）像素，淹没门禁日志、拖慢失败诊断。
- **建议修法**：手写 `Debug`（只打 `width/height/首尾若干像素` 或哈希）。
- **是否阻塞 P0**：不阻塞。

#### Q-11 三个相近命名/空间缺少类型区分，易混用

- **位置**：`node/tree/content.rs:711-741`（`screen_baked` 是「节点种类是否需要屏幕烘焙」的 bool；`baked_screen_bounds` 是「上次烘焙的屏幕 rect」；`screen_bounds` 是「本帧推得的屏幕 rect」）；`paint/pass.rs:743`（`extent_shift` → L 空间）与 `pass.rs:1093`（Geometry 的 `extent_shift = total`）。
- **证据**：`PaintedExtent`（L 空间）与 `baked_screen_bounds`（屏幕空间）都是 `LogicalRect`，编译期无法区分；`hide_subtree` 同函数里同时出现 `negate(total)` 与 `slot_offset`（`pass.rs:574-616`）。
- **影响**：可维护性（P1 会把更多路径接进这两张表）；本次审查中「extent 属 L 空间」必须靠 `extent_shift` 反推才能确认。
- **建议修法**：至少引入 newtype（`struct ScreenRect(LogicalRect)` / `struct LayoutRect(LogicalRect)`）或在命名上明确（`painted_extent_layout` / `baked_screen_rect`）。
- **是否阻塞 P0**：不阻塞。

#### Q-12 AGENTS §6：单点使用的包装函数；`PersistentPixels::readback` 仅测试使用却为 pub

- **位置**：`paint/pass.rs:1450-1452`（`note_shadow` 只有 `push_shadow_damage` 一个调用点，函数体是 `note_painted(bounds.outset(...))` 一行）；`test_support/pixels.rs:131-137`（`pub fn readback`）——`gallery.rs` 不用它，唯一调用方是同文件的 `#[cfg(test)] mod tests`（`pixels.rs:654, 699, 725…`）。
- **证据**：AGENTS §6「如果一个操作只在一个地方使用…直接保留在调用位置」；`pixels.rs` 是 `pub mod test_support`（feature 打开即公开 API，`ui_core/src/lib.rs:15-16`），`readback` 因此进入公开面。
- **建议修法**：`note_shadow` 内联；`readback` 降为 `#[cfg(test)]` 或直接删（测试可用 `present_record` 的返回值）。
- **是否阻塞 P0**：不阻塞。

#### Q-13 `--pixel-check` 与旧 `Overlay` API 强绑定，且空转断言对采样步长敏感（P1 重写/误报风险）

- **位置**：`ui_kit/examples/gallery.rs:3169-3345`（用 `ColorPicker::overlay()`、`Overlay::panel_node/revealed/open/close`）、`:3255-3262` 的 idle 断言（`PIXEL_CHECK_IDLE_STEP_MS=250`，20 次）。
- **证据**：设计 §8/P1-11 会删除 `kit::overlay::Overlay` 全部类型、P1-12 只承诺重写 `--dump-overlay`；`--pixel-check` 的移植未登记。idle 断言要求「5 s 内每帧都 idle」，若 P1 在 popup 内引入光标闪烁/动画（周期与 250 ms 采样步长可通约），该断言会从「残影检查」变成「动画检查」而误报。
- **影响**：测试可维护性；P1 收口时容易出现「门禁红但与被测风险无关」。
- **建议修法**：在 P1 任务表登记 `--pixel-check` 迁移；idle 断言改成「无 paint 工作的帧」显式条件（或先断言无动画源），避免与周期性动画耦合。
- **是否阻塞 P0**：不阻塞。

#### Q-14 新单元测试硬编码绝对像素坐标/窗口尺寸；damage 覆盖断言只取 2 个角点

- **位置**：`test_support/pixels.rs:71-123`（窗口取 `TestApp` 默认尺寸，测试用 `(160,140)`、`(460,240)`、`(400,220)`、`(700,500)` 等绝对像素坐标）；`app/removal_tests.rs:1110-1139` 与 `paint/pass.rs:703-729` 的 `assert_damage_covers_extent` 只探测 extent 的两个角点。
- **影响**：TestApp 默认窗口尺寸/主题变化时失败信息误导（"像素不是蓝色"而非"窗口尺寸变了"）；L 形 damage 缺口可能漏检（对当前矩形并集模型风险低）。
- **建议修法**：坐标由节点 bounds 推导；覆盖断言改为 4 角 + 中心，或直接断言 `damage_rect ⊇ extent`。
- **是否阻塞 P0**：不阻塞。

#### Q-15 `Overlay::measure` 与 `Overlay::place` 近重复（约 40 行），P0 引入第二份实现

- **位置**：`ui_kit/src/kit/overlay.rs:814-864`（measure）与 `:871-926`（place）：同样解析 `(window, handle, anchor)`、`anchor_rect`、`viewport`、`panel_size`、`resolve_origin`、`moved` 判定、`place_overlay`、`sync_scrim`；差异只有 place 额外的 `revealed` 置位与句柄字段（`node` vs `panel`）。
- **证据**：AGENTS §1.1/§1.2（不堆叠、保持简单）；这是 P0-8 的最小修法，但第二份实现会让 P1 删除时漏改（`measure` 是 `pub(crate)`，只有 2 个调用点：`overlay.rs:807`、`color_picker.rs:1338`）。
- **建议修法**：`place` 复用 `measure` 的内核（抽出「计算 origin + 落位 + scrim」一段，`revealed` 单独处理），或 P1 删除时一并移除并把首帧测量并入引擎 settle。
- **是否阻塞 P0**：不阻塞。

#### Q-16 `paint.rs` 模块声明未保持既有字母序（`pub(crate) mod extent` 插在 `color` 与 `geometry` 之间）

- **位置**：`crates/ui_core/src/paint.rs:1-12`。
- **证据**：既有顺序为 `atlas, canvas, color, geometry, geometry_store…`；新增项插在 `color` 后、`geometry` 前，破坏原有排序；`cargo fmt` 不重排模块声明，故门禁不报。
- **影响**：纯风格一致性。
- **建议修法**：`extent` 放到 `color` 之前或 `geometry_store` 之后，保持既有约定。
- **是否阻塞 P0**：不阻塞。

---

## 3. 规范对照结果（逐条 AGENTS 章节）

| 章节 | 结论 | 证据 / 说明 |
|---|---|---|
| §1.1 不维护过时设计（无兼容层/migration/fallback） | **通过** | `Primitive::Canvas(Arc<Canvas>)` → `Canvas(CanvasDraw)` 是硬替换（无 `From`/别名/双载荷）；`paint_epoch_changed` 被拆成三个新 API 而非保留旧入口（`tree.rs:695-707`）；`color_picker::node_bounds` 直接改名 `node_canvas_bounds` 并改语义（`color_picker.rs:2195-2200`），未留旧函数；未新增配置层/开关。 |
| §1.2 保持简单 | **基本通过** | 新增 `paint/extent.rs` 仅 34 行、职责单一；`CanvasDraw` 只加 `offset` 一个字段。扣分项见 Q-15（两份 place 逻辑）、Q-12（单点包装）。 |
| §1.3 模块化 / 无 `#[path]` | **通过** | 无 `#[path]`；`paint/extent.rs` 走标准布局。Q-16 仅为声明顺序。 |
| §1.4 复用成熟方案 | **通过** | 复用既有 `DamageAccumulator`/`Plan`/`DamageSummary`/`GpuRenderer::submit_snapshot`/`gpu_guard`；`compare_pixels`/`changed_pixels` 是 30 行纯函数，无新依赖。 |
| §1.5 架构一次做到位 | **通过（有登记项）** | `Canvas` 局部坐标是设计的一部分（P0-4），不是临时架构。`Overlay::revealed`（`overlay.rs:866-869`）与 `PersistentPixels` 是 P0 需要、P1 会删除/替换的面；`--pixel-check` 迁移未登记 → 见 Q-13。 |
| §1.6 不写注释 | **通过（机械验证）** | 3834 条新增行中**含 `//` 的行数 = 0**（含块注释标记 `/* */` = 0）；新文件 `extent.rs` 34 行零注释。 |
| §2.1 Serde | **不适用** | 无 serde 改动、无 `serde_json::*`。 |
| §2.2 SmolStr | **通过** | 新增字符串一律 `SmolStr`：`format_smolstr!`（gallery 失败信息）、`SmolStr::new_static`（pixels harness 错误 detail、check key 文案）；无 `format!`+`SmolStr::from`。 |
| §2.3 集合类型 | **基本通过** | 无裸 `HashMap/HashSet` 新增；迭代器优先（`compare_pixels`/`changed_pixels`/`rebuild_order_only` 均迭代器/显式栈）；已知容量处有预分配（`Vec::with_capacity(2)`、`collect()` 来自 ExactSize 迭代器）。`SmallVec` 复用既有 scratch（`bake_scratch`/`rebuild_scratch`）。 |
| §2.4 字节数据 | **通过** | 未新增 `Vec<u8>` 语义类型；`SnapshotImage.rgba8` 属既有测试基建。 |
| §3 强类型错误 | **不通过（P2）** | Q-05 `EngineError::AppInit` 误用；Q-04 静默丢弃 `Result`。其余新 API（`measure -> Result<bool, EngineError>`、`set_geometry_transform` 等）沿用既有强类型 `EngineError`。 |
| §4 Warning 与无用代码 | **通过** | 0 处 `#[allow]`/`#[expect]`/`#[allow(dead_code)]`；clippy `-D warnings` 全绿；`Window::scroll_slots` 的 cfg 从 `#[cfg(test)]` 放宽为 `#[cfg(any(test, feature="test-support"))]` 后，`cargo check -p ui_core --features test-support --all-targets` 仍无 warning（有真实消费者：`pixels.rs`）。Q-12 的 `readback` 属「只有一个测试消费者」，建议收敛。 |
| §5 依赖管理 | **通过** | 无任何 `Cargo.toml` 改动；新代码只用 workspace 既有依赖（`smol_str`、`smallvec`、`ahash`）。 |
| §6 小型函数/无意义抽象 | **基本通过** | Q-12 `note_shadow`；其余 helper（`negate`/`offset_between`/`painted_extent`）均有 ≥2 调用点或承载边界语义。 |
| §7 实现前清单 | **通过** | P0-0 判别实验先于修法（`evidence/p0-diagnosis.md`）；Engine 侧优先重构（`Canvas` 契约）而非补丁。 |
| §8 验收 | **通过（本机复跑）** | `cargo test -p ui_core -p ui_kit` / `clippy -D warnings` / `fmt --check` 全绿；workspace 级 `cargo test` 我未复跑（见 §6 残余风险）。 |

---

## 4. 性能 / 内存观察（含实测）

### 4.1 实测

| 项 | 数值 | 备注 |
|---|---|---|
| `--pixel-check` 进程总耗时 | **3.61 s** | 直接跑 `target\debug\examples\gallery.exe`，D3D12 WARP（`Microsoft Basic Render Driver`，`warp=true`）；`cargo run` 额外约 14 s 是 cargo 自身开销，不代表门禁成本 |
| `--pixel-check` 采样帧 | 45 帧 / 108 断言 | `damage_full_frames=0` |
| 单帧均摊成本 | ≈ **80 ms/采样帧** | 每帧含整窗离屏 readback（1280×800×4 ≈ 4 MB）+ `compare_pixels` 全图扫描（含排除矩形）+ `changed_pixels` 全图扫描（×damage 矩形数） |
| 全量测试 | 3531 passed，ui_core lib 9.55 s | 新增 5 条 GPU 像素测试在大头里；无抖动 |
| 侧表内存 | **≈44 B/节点**（`Option<LogicalRect>` 20 + `Option<PaintedExtent>` 24） | 设计 R9 估 ~24 B/节点；40k 节点 ≈ 1.7 MB（Q-08）。`Node` 结构体本身未变动（侧表方案），符合「Node 保持 336B」的口径 |
| 结构帧 order 重建 | 2×O(n) DFS（Q-02） | 单次 DFS 对 40k 节点 fixture 即 ~40k 次 `tree.get`；`order_rebuilds` 计数器翻倍 |

### 4.2 代码面观察（无实测数字，但可定位）

- **结构帧增量是否真的减少了全量帧**：是。证据：`damage_full_frames=0`（45 帧）；`removal_tests` 5 条断言 `!damage_full` 且 `full_frames` 不增（仅滚动容器注册/移除允许降级一次，`removal_tests.rs:1461-1552`）；`window/paint.rs:384-455` 的 40k 节点用例断言访问数 ≤256 且 `full_frames` 不变。
- **侧表回收**：`arena::insert_child` 清空两张表（`arena.rs:278,282`），`arena::remove` 清空 `painted_extents`（`arena.rs:434-435`），读取全部走 `.get()`（越界安全，`content.rs:685-698`）→ 无泄漏、无越界。
- **`baked_screen_bounds` 未在 remove 清空**（只清 `painted_extents`）：不影响正确性（下次 `insert_child` 重置；节点已不在树中不会被读），但属不对称，建议顺手清掉。
- **Canvas 每帧重克隆/重解析**：未新增。`resolved_canvas` 只在 `canvas_has_text` 时每帧重建（与改动前一致）；`CanvasDraw` 持有 `Arc<Canvas>`，`resolved_canvas` 递归时对嵌套 canvas 新建 `Arc`（与改动前同）。
- **Canvas 节点的复用成本**：`screen_baked` 节点被强制 `slot=0`（`pass.rs:406-410`），滚动时会因 `baked_screen_bounds != screen_bounds` 重建 lane —— 这是设计 I5 的既定取舍（滚动中的 Canvas 每帧重建），性能上需在 P3 基线里对照；当前无回归证据。
- **`--pixel-check` 3.61 s** 对本地门禁可接受；但它在 CI 上需要离屏 GPU + readback（见 §5）。
- **损坏阈值改动**（`render/damage.rs:87-126`）：面积按 `intersect(frame)` 计算 → 窗外 damage 不再触发整帧，属于**减少**全量帧；`DeviceRect::intersect` 会 clamp（`math.rs:776`），不会产生负面积；`plan()` 再把每个 damage rect 裁到 surface 并丢弃空矩形（`render/compositor.rs` `plan()`），因此不会出现「越界 scissor」。实测对照：`cargo test` 中 `area_threshold_boundary_is_exact` 等既有阈值用例全绿。**结论：阈值改动无正确性副作用，属净收益。**

---

## 5. 测试可靠性评估

| 维度 | 结论 | 证据 |
|---|---|---|
| 时序/睡眠依赖 | **无真实睡眠** | 新测试与 gallery 全部用 `TestApp::advance(Duration)` 推进虚拟时钟（`pixels.rs:690, 740, 765`；`gallery.rs:3007`），`idle` 判定基于 `FrameRecord.rendered`。 |
| 真实窗口依赖 | **库内测试无窗口**；gallery `--pixel-check` 也是 headless（`TestApp` + 离屏 `GpuRenderer`） | `pixels.rs:52-137`（`PersistentPixels` 自建 `offscreen_gpu`），`gallery.rs:3169-3175`（`TestApp::new()`）。真实窗口只有 `--dump-overlay`（仅 rect 断言）→ 见残余风险 R1。 |
| GPU 依赖 | **新增 5 条 lib 测试硬依赖可用适配器 + readback**；`gpu_guard()` 只做互斥，不 skip（`test_support/snapshot.rs` 的 `static LOCK: Mutex<()>`） | 无 GPU 的 CI 上会**失败**而非跳过。该属性与既有 snapshot/golden 测试同源（非本次引入），但把 GPU 依赖从 `tests/` 扩到了 lib 单测。 |
| 跨平台可跑性 | 机制上可跑（无平台 API），但 `span<=1` 的「增量 vs 全量逐字节一致」依赖驱动确定性；半透明阴影用例未加基线不透明前提（Q-07） | `pixels.rs:552-561`；对照 gallery 的 `baseline-opaque`（`gallery.rs:3191`） |
| 可复现性 | 确定性伪随机（`0x5EED…` LCG）用于 damage 属性测试；像素用例固定坐标 | `render/damage.rs:573-580` |
| 断言质量 | 好：失败信息带 `{damage:?}`/rect/label，可定位；不足：damage 覆盖只采样 2 角点（Q-14）、`PixelFrame` Debug 会打印 4 MB（Q-10） | 见 Q-10/Q-14 |
| 门禁是否「虚假通过」 | **否**：像素 harness 自带反例校验——`pixels.rs:575-638` 主动构造「漏 damage」帧并断言残影可见（`compare_pixels(...).span > 128`），再断言正确 damage 能自愈；因此 `--pixel-check` 的 PASS 不是空测。 | `pixels.rs:600-638` |
| P1 重构后的存活性 | 引擎侧测试（`removal_tests`/`pass`/`pixels harness`）应存活；`color_picker`/`overlay` 的新用例与 `--pixel-check` 依赖 P1 将删除的 `Overlay` API，需随 P1-12 重写（Q-13） | 设计 §8/P1-11、P1-12 |

---

## 6. 已核实为正确的点 + 残余风险

### 6.1 已核实正确（含验证方式）

1. **Canvas 局部坐标契约完整迁移**：全仓 136 处 `Canvas::new` 无一处以非零原点字面量构造（脚本文法扫描 + 抽查 `order_book`/`gauge`/`timeline`/`map`/`volume_heatmap`/`prediction_market`/`spinner`/`color_picker`/`snapshot` 全部改为 `LogicalRect::from_origin_size(LogicalPoint::ZERO, size)`）；缓存键同步从 `bounds` 改为 `size`（去 origin），避免位置变化导致的重绘。engine 侧 `origin = bounds.origin + content_offset`、`CanvasDraw{canvas, offset}`（`pass.rs:1681-1710`），嵌套 canvas 偏移累加（`instance.rs` 递归 `canvas_offset`），`Transform::then(translate(offset))` 语义核对为 `apply(self) then next`（`math.rs:1092-1110`），即「局部变换后再平移到节点位置」，正确。
2. **`painted_extent` 空间自洽**：lane 空间 = `layout − bake`（`pass.rs:685`）；`extent_shift = total − slot_offset` 把 lane 空间换回 **L 空间**（`pass.rs:743`），与 `destroy` 侧 `negate(total)` 换到屏幕空间互补；Geometry 的 `extent_shift = total` 与其 `bake == total`（`pass.rs:962` 传 `LogicalPoint::ZERO`）自洽。
3. **extent 生命周期无泄漏**：lane 重建/隐藏/几何清空/remove 全部显式覆盖（`pass.rs:610-616, 651-654, 894-897, 1204-1209`；`arena.rs:434-435`），`settle_frame` 在提交后清 damage（`layout.rs:484-493`、`window/frame.rs:204`），不存在 damage 跨帧累积。
4. **结构增量路径的 epoch 消费**：`note_paint_frame` 只在真正跑过 pass 的帧记录 epoch（`tree.rs:709-722`），无「空帧吃掉结构变化」；`first_paint` 仅在前 npm 帧为真，最坏多一次全量。
5. **删除子树 damage 主通道正确（offset=0）**：`arena::remove` → 父 `damage`（L 空间）→ 父被 `mark_structure` 标脏 → 同帧重建 lane → `emit_node_damage` 换算到屏幕；有 7 条 removal 用例 + 像素用例背书。
6. **`scroll_slots` 平行数组不变量**：`debug_assert_eq!(scroll_slots.len(), scroll_slot_nodes.len() + 1)`（`window/paint.rs:258-263`）在增量路径可满足——`scroll_slot_for` 在 `cached == 0`（新容器）时同时 push 两侧（`pass.rs:503-523`）；滚动容器移除会 bump `scroll_reg_epoch`（`arena.rs:393-399`）→ 下一帧强制全量重建表，不会长期挂着 stale slot。
7. **阈值改动无副作用**：`intersect(frame).area()` 无负面积（`DeviceRect::intersect` clamp），`plan()` 二次裁到 surface 并丢空（不会出现越界 scissor），既有阈值/覆盖用例全绿。
8. **test-support 未泄漏到生产 API**：`ui_core::test_support` 由 feature 门控（`lib.rs:15`）；`ui_kit` 仅在 `[dev-dependencies]` 启用（`ui_kit/Cargo.toml:19-21`）；`PersistentPixels` 等消费方是 gallery example 与 lib 测试；`Window::scroll_slots` 仍 `pub(crate)`；`cargo check -p ui_core --features test-support --all-targets` 无 warning。
9. **AGENTS 硬约束**：零注释、零 `#[allow]/#[expect]`、零新依赖、零 `#[path]`、生产路径零 `unwrap/expect/panic`（162 条命中全部落在测试模块/example）。
10. **门禁四条本机复跑全绿**（见 §1.3），`--pixel-check` 的 `first_visible == stable`、`damage_full_frames = 0` 与 summary 声明一致，可复现（两次运行数字相同）。

### 6.2 残余风险（未构成 finding，但需在 P0 收口/ P1 起点处置）

- **R1 真实窗口像素仍无自动化证据**：P0-5 的像素门禁是 headless 镜像（`TestApp` 内存 surface + 独立离屏 renderer），不经真实 swapchain/DComp 呈现路径。DoD §10.1「残影消失」在真实窗口只能靠 `--dump-overlay` 的 rect 断言 + 人工基线（summary 偏差 1 已披露）。P3-6 需补真实窗口 readback 方案；建议 P0 提交说明里明确「真实窗口像素 = 人工证据，不是门禁」。
- **R2 滚动 offset≠0 的删除/移动 damage**（Q-01）：已披露、必须在 P1 虚拟列表复用前修。
- **R3 `measure` 与文本 shape 顺序**（Q-03）：建议 P1-2 前做文本高度判别实验。
- **R4 17 个 widgets/ widget 文件的迁移只由 `cargo check --all-targets` 覆盖**：本次门禁只跑 `ui_core`/`ui_kit` 测试；`p0-summary.md:14` 的「9 个 widgets crate 1639 passed」为自证，未在门禁命令里。AGENTS §8.1 要求 workspace `cargo test`——建议 P0 提交前由 CI/lead 附一次 workspace 测试日志。
- **R5 damage 矩形部分重叠时的双混合**（Q-07）：当前 damage 集合形态下不发生，但 P1 引入多 popup 叠加后需重估容差策略。
- **R6 `Overlay::revealed` 与 `PersistentPixels` 是 P1 会删除/替换的 API**：已在 summary/§8 有对应删除项，但 `--pixel-check` 的迁移未登记（Q-13）。

---

## 7. 是否可提交 P0

**可以提交**，判断依据：

1. 无 P0 级 finding：未发现正确性缺陷、硬要求违反、回归或门禁虚假通过；门禁四条本机复跑全绿，`--pixel-check` 可复现 PASS 且自带「漏 damage ⇒ 残影可见」的反例自检。
2. AGENTS 硬约束全部满足（零注释/零 allow/零新依赖/零生产 unwrap/强类型 API 主体合规）；仅 2 处 P2 级错误处理瑕疵（Q-04/Q-05）。
3. 已披露偏差（真实窗口像素、滚动 offset、阈值口径、内存/空间口径）均**真实存在且被正确披露**，无夸大；文档层面的不一致（Q-08/Q-09）属 P3，可随提交说明或 §11 回写修掉。
4. 建议的提交前动作（低成本、非阻塞）：
   - 修 Q-04（一行 `match`）与 Q-02（一个布尔短路）——两者改动面极小、风险低；
   - 在 summary 或 §11 登记 Q-08/Q-09/Q-13（文档诚实性 + P1 迁移项）；
   - 补一次 workspace `cargo test` 日志（R4）。
5. P0 之后的优先顺序：Q-01（P1 列表复用前必须）> Q-03（P1-2 settle 前实验）> Q-07（多 popup 前）> 其余 P3。
