# P0 对抗性审查（设计一致性 / 迁移完整性 / 测试有效性）

- 审查对象：worktree `dsh-1d175414-1a41`，HEAD `71a5a5cc` + 未提交 P0 改动（48 个已跟踪文件 + 未跟踪 `crates/ui_core/src/paint/extent.rs`；`git diff --stat` = 3488 insertions / 372 deletions）
- 审查者视角：**设计文档 rev.6 一致性、Canvas 迁移完整性、测试有效性（是否有删除/放宽/虚假通过）、验收可验证性**；代码质量/AGENTS 逐条对照由并行的 `review-p0-quality.md` 负责，本报告不重复其 Q-01…Q-16 的推理，只在交叉处引用。
- 结论：**P0 = 0，P1 = 2，P2 = 8，P3 = 4；未发现正确性缺陷、设计硬要求违反、回归或门禁虚假通过**。门禁在本机独立复跑全绿（见 §1.2）。**P0 可提交**，但提交说明/设计 §11 必须补齐本报告 C-03/C-04/C-06 及 summary 未登记的偏差（见 §2），C-02 必须在 P1 列表复用前修掉。
- 只读约束：审查期间未修改任何实现文件；`git status --short` 与审查开始时一致（49 项 = 48 M + 1 未跟踪），仅新增本报告。

---

## 1. 审查范围与方法

### 1.1 材料（逐字读过）

| 材料 | 用途 |
|---|---|
| `.docs/popup-overlay-redesign-design.md` rev.6 全文（928 行） | §2.3、§4.11（554–577）、§5.3（667–681）、§6 P0（689–706）、§7.2（781–788）、§10 DoD（864–872）、§11（876–878）、附录 C P0-0 诊断（920–928）逐条对照 |
| `.docs/popup-overlay-redesign/implementation/p0-summary.md` 全文 | 声明/偏差逐条核对 |
| `evidence/p0-diagnosis.md`（277 行）、`evidence/reproduction.md`（206 行） | P0-0 结论、回归前提、O1–O7 |
| `AGENTS.md`（仓库根，172 行）、`crates/widgets/AGENTS.md`（仅规范相关部分） | 注释/依赖/错误/删除/验收口径 |
| 完整 `git diff`（6030 行 diff 文件，逐 hunk 阅读）+ `paint/extent.rs` 全文 | findings |
| 工作树全文（非 diff）：`paint/pass.rs`（5845 行关键函数）、`paint/instance.rs` 编码器、`node/tree/{arena,content,dirty,layout}.rs`、`node/tree.rs`、`window/paint.rs`、`render/{damage,compose,delta}.rs`、`test_support/pixels.rs`、`test_support/diagnostics.rs`、`ui_kit/{overlay,color_picker,spinner}.rs`、`gallery.rs`、17 个 widgets 文件 | 空间换算 / 迁移语义 / 测试强度 |
| 交叉参照：`review-p0-quality.md`（292 行，另一审查者，结论 P0=0/P1=1/P2=6/P3=8） | 避免重复；核对哪些声明双方一致 |

### 1.2 独立复跑的门禁（本机，`CARGO_TARGET_DIR=E:\kytrix\target`）

| 命令 | 结果 |
|---|---|
| `cargo test -p ui_core -p ui_kit` | EXIT=0（ui_kit lib `766 passed`；ui_core 全绿；与 `review-p0-quality.md §1.3` 的 3531 passed / 2 ignored 一致） |
| `cargo +nightly fmt --all -- --check` | EXIT=0，无输出 |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | EXIT=0 |
| `cargo check --workspace --all-targets` | EXIT=0 |
| `cargo run -p ui_kit --example gallery -- --pixel-check` | EXIT=0；`checks=108 sampled_frames=45 damage_full_frames=0`，`first_visible == stable == (496,91,300,375)`；adapter=`Microsoft Basic Render Driver`（WARP） |
| `cargo run -p ui_kit --example gallery -- --dump-overlay --auto-close=9`（真实窗口） | EXIT=0；`overlay-check PASS (menu=Some((6,180.0,127.5125)) picker_open=true picker_children=Some(4))`；`palette.canvas rect=(44,115 284x284)` 等几何与 HEAD 语义一致 |
| （补充，绕过 cargo 锁直接运行既有测试二进制）`target\debug\deps\snapshot-*.exe golden_` | EXIT=0，`golden_clip/clip_subpixel/colors/gradient/image/shadow/shapes/color_emoji` **8 passed / 0 failed**，golden PNG（`tests/golden/warp/`）未修改 → Canvas 迁移的像素级等价性直接验证 |

### 1.3 机械扫描（针对 diff 与全仓）

| 项 | 结果 |
|---|---|
| 真实 `Canvas::new(` 构造点（全仓 `crates/**/*.rs`，排除 `ChartCanvas::new` 等子串误计） | **124 / 41 文件**：ui_core 85/24、ui_kit 9/2、widget 5/2、widgets 25/13（设计 §5.3 写 132/45；差额为子串口径，见 C-11） |
| 删除的测试/断言 | 0 个测试函数删除；`git diff` 中删除的 `assert` 仅 3 类：`paint_epoch_changed` 被拆 API 的等价重写（`node/tree/tests.rs:1009`）、`Canvas::new` 包装（非断言）、widgets 的“画布=节点盒”被改为“画布尺寸=节点盒尺寸”（语义等价）。**无断言被放宽/删除** |
| `#[ignore]` 新增 | 0（全仓仅 2 处既有忽略：`dark_mode.rs:138`、`gui_smoke.rs:6`） |
| `#[allow]`/`#[expect]`/`#[path]`/`TODO`/`FIXME` 新增 | 0 |
| 新增注释（`//`、`/* */`、文档注释） | 0 |
| 新依赖 / `Cargo.toml` / `Cargo.lock` 改动 | 0 |
| 空断言模式（`let _ =` 断言、循环空断言） | 新增测试中无；`removal_tests.rs` 有一处 `let _ = bystander;`（仅占位变量，不承担断言） |

### 1.4 方法说明

- 对 17 个 widgets 文件与 ui_kit/ui_core 所有 Canvas 构造点逐个核对“局部坐标/缓存键/指针局部化/几何顶点”的语义，对照 `git show HEAD:<path>` 判断迁移是否只是“编译通过”。
- 对 P0-0 的判别结论（M-D/M-E refuted）核对其是否被 P0-2 的代码范围与门禁尊重：`git diff` 中 `paint/instance.rs`/`render/delta.rs`/`render/compose.rs` **无生产逻辑改动**（只有测试与 `instance_rects` 测试辅助），符合“禁止无证据重写 ops 生命周期”。
- 未按任务约束运行 ui_core/ui_kit 之外的 crate 测试；`widgets` 的 1639 passed 只能作为自证（见 C-01）。

---

## 2. Findings

汇总：**P0 = 0，P1 = 2，P2 = 8，P3 = 4**。严重度定义按任务：P0=正确性缺陷/设计硬要求违反/回归/门禁虚假通过；P1=重大功能或健壮性缺口/缺失必需测试/迁移错误影响用户可见行为；P2=次要问题；P3=风格/文档。
与 `review-p0-quality.md` 的关系：C-02 与其 Q-01/Q-08 同源（本文从设计 I1/C4 契约角度重述并补 disclosure 缺口）；C-04 补充其未覆盖的 `test_support/diagnostics.rs`；C-06 补 P0-2 的“顺序变化”缺口；其余为本文新增。

### P1

#### C-01 P0 验收 ②/③ 在声明的门禁里不可验证：② 没有任何真实窗口像素证据，③ 的 chart/spinner/picker 测试不在门禁内

- **位置**：设计 `:706`（P0 验收②“残影在真实窗口基线对比中消失（打开前后、关闭后、移动后）”、③“Canvas 全量迁移后所有 chart/spinner/取色器测试绿”）；`p0-summary.md:6`（本轮门禁清单）。
- **证据**：
  1. 本轮门禁里与残影相关的只有 `--pixel-check`（headless `TestApp` + 离屏 `GpuRenderer`，`gallery.rs:3169-3349`）与 `--dump-overlay`（仅 rect，见 `gallery.rs` 未改动、无像素读取）。`--dump-overlay --auto-close=9` 的 PASS 只证明 `overlay-check` 的 rect/children 断言（`O6` 已证其对残影“完全免疫”），**不构成验收②的真实窗口基线对比**。
  2. 验收③的 chart/spinner/picker 测试大部分位于 `crates/widgets/*`；`p0-summary.md:14` 明确写“9 个 widgets crate 测试 1639 passed（**非本轮门禁，自证**）”，而门禁只跑 `ui_core`/`ui_kit`。AGENTS §8 要求 `cargo test`（workspace），本轮未运行。
  3. 设计 §11（`:876-878`）仍是“实施后回写，当前为空”，所以这份未被门禁覆盖的“自证”没有设计侧记录。`review-p0-quality.md` 的 R4 也把它列为残余风险，但未按“验收③未满足”处理。
- **公平说明**：设计 P0-5（`:703`）本身允许“或新增‘持久 target + scissor 增量’测试 harness”，因此 C9 的路径选择**不违反** P0-5；本 finding 针对的是 **§6 验收②把“真实窗口基线对比”写成验收条件、而 summary 以 headless 证据宣布满足**这一不一致（以及 §11 未回写用户对该路线的接受）。
- **影响**：summary 的“本轮门禁全部 EXIT=0”在字面上为真，但**不足以支撑 P0 验收②③**；若直接以“P0 验收通过”进入 P1，③ 的迁移回归（widgets 的图表/滚动行为）只能在 P1 才暴露。
- **建议修法**：提交前由 lead 附一次 `cargo test -p <9 个 widgets crate>` 或 workspace `cargo test` 日志（写进提交说明/§11）；在 §11 明确写“验收②：真实窗口像素 = 未执行（用户接受 headless 路线，人工证据见 evidence），③：命令与结果见附件”。
- **是否阻塞 P0**：不建议阻塞（证据可低成本补齐）；但**不能声称②③按设计完成**。

#### C-02 P0-1 的 `painted_extent` 契约与设计 §4.11 不一致（设计要求 `DeviceRect`，实现为 L 空间 LogicalRect），且在“滚动容器直接子节点删除/跨滚动上下文重挂”时 damage 会偏移容器自身 scroll offset

- **位置**：设计 `:558`（`painted_extent: Option<(DeviceRect, PhysicalPx)>`，即屏幕/设备空间）与 `:559`（“damage = 旧 `painted_extent` ∪ 新”）；实现 `paint/extent.rs:6-34`（`LogicalRect + pad`）、`paint/pass.rs:743`（`extent_shift = total + negate(slot_offset)` → L 空间）、`paint/pass.rs:651-654`（重建时用 `negate(total)` 反推）、`node/tree/arena.rs:400-417`（remove 把 L 空间 extent 并入 `parent.damage`）、`node/tree/layout.rs:281-296`（`place_subtree` 同通道）、`paint/pass.rs:1621-1628`（`emit_node_damage` 以父节点自身的 `total` 发射）。
- **证据（代码推导，与 quality Q-01 独立复核一致）**：
  1. extent 存的是 L（未减任何滚动的内容坐标）；remove 直接并进父节点 `damage`。
  2. 父节点 lane 重建时 `emit_node_damage` 用 `negate(self.offset)`，`self.offset` = 父节点的**祖先累计滚动**，不含父节点**自身**的 scroll offset（自身 scroll 只进入子节点 `total`：`pass.rs:459-470`）。
  3. 因此“父 == 滚动容器且 offset≠0”时，damage 坐标 = `L − 祖先滚动`，正确屏幕坐标 = `L − 祖先滚动 − 自身滚动`；再被容器 clip 裁剪后可能整块落空 → 旧行像素残留。
  4. 同理，跨滚动上下文 `append_child`（`arena.rs:328-366`，`mark_subtree_paint` 后重建）用**新** `total` 反推旧 extent，也会差旧上下文的 scroll 差；当前生产只有 Debug HUD/overlay bring-to-front/WidgetSurface mount 使用 `append_child`（`app/debug.rs:90`、`app/overlay.rs:144`、`widget/src/surface.rs:214`），不在滚动容器内，故尚未触发。
  5. 现有测试把该路径固定成 offset=0：`app/removal_tests.rs` 的 `adding_and_removing_scrolled_content_stays_incremental` 创建了滚动容器但从未 `scroll_to`（`make_scroll`+`set_scroll_content` 后直接插入/删除）；`test_support/pixels.rs:943` 的滚动用例只滚动、不删除滚动容器直接子节点。**这正是“测试避开了实现会错的输入”**。
- **与设计硬约束的关系**：C4“移动/隐藏/删除必须 damage 上一帧实际绘制范围 ∪ 本帧绘制范围”与 I1 在“滚动容器直接子节点删除”这一组合下不成立；设计要求的 `DeviceRect` 正是为了免除“用当前 total 反推”的时点依赖。
- **P0-7 让该路径第一次真正暴露（HEAD 被“父盒 damage”掩盖）**：HEAD 的 `dirty::touch` 对 `LAYOUT|PAINT` 会调 `target.accumulate_damage()`（`git show HEAD:crates/ui_core/src/node/tree/dirty.rs`，accumulate 语义见当前 `node/tree.rs:300-303`：`damage ∪= layout.bounds`）；`mark_structure` 因此会把父节点自身盒并入 damage。对滚动容器来说父盒 = **整个视口**（其自身 scroll 不移动视口盒），于是“子节点 extent 的坐标错位”在 HEAD 被视口级 damage 掩盖。P0-7 把 `mark_structure` 改为 `touch_without_damage`（`node/tree/dirty.rs:29-45`）后，这个掩盖消失，`remove` 的 extent 通道成为唯一覆盖。
- **当前是否用户可见（谨慎结论）**：生产 virtual-list 的“删除已挂载行”只发生在 `count` 缩小且 offset 仍有效时；对固定行高列表可推出此时 `last < new_count`（视口内没有需要删除的行），而 offset 失效时 `scroll_to`/`clamp_scroll_offsets` 必然改变 offset 并推视口 damage（`app/scroll.rs:133-140`），非可见行的 damage 又会被容器 clip 丢弃——因此**尚不能证明当前产品存在用户可见残影**。但 `remove_node` 直接删除滚动容器直接子节点的任意调用方（P1 的 Select/Dropdown、非虚拟列表行回收）会命中；HEAD 旧代码的“父盒兜底”已按 P0-7 有意移除，P1 前必须有正确修法。
- **p0-summary 的披露不完整**：`p0-summary.md:25` 只写了“滚动容器内删除的 damage … 会差该容器 scroll offset（旧 layout.bounds 通道同样问题）”，**没有**披露：(a) 与设计 `DeviceRect` 的偏差、(b) `place_subtree` 共用同一通道、(c) 跨滚动上下文重挂的同类风险、(d) P0-7 移除了 HEAD 的父盒 damage 兜底、(e) 应在设计 §11 登记。
- **影响**：P1 的 Select/Dropdown/virtual list 行回收（offset≠0）删除行会留残影；与 DoD §10.1“打开/移动/关闭/重开全程无旧像素”在滚动列表路径冲突。对 P0 主场景（popup 不滚动、offset=0）无影响。
- **建议修法**：优先按设计改为屏幕/设备空间存 extent（`DeviceRect + PhysicalPx`），或在并入父节点前把 extent 换算到父节点的发射空间（减父自身 scroll）；补一条 offset≠0 的删除用例（`make_scroll` + `scroll_to(0,300)` 后 `remove_node`，断言 damage 覆盖旧行屏幕 rect 且 `!damage_full`），并补 `append_child` 跨滚动上下文的用例。
- **是否阻塞 P0**：不阻塞（P0 主场景 offset=0；且该 damage 坐标问题在 HEAD 的 `layout.bounds` 通道已存在，P0 未新增用户可见回归）。**必须在 P1 列表复用前修掉，并在 §11 按设计偏差登记**。

### P2

#### C-03 P0-8（旧 kit Overlay 首帧定位）只存在于 p0-summary，设计 rev.6 没有它；且与“P0 只改渲染不变量与 Canvas 语义”“P1 才动 kit 定位时序”的范围声明冲突

- **位置**：`p0-summary.md:20`（P0-8，宣称“用户裁决 A”）；设计 §6 P0 任务表 `:693-706` 只有 P0-0/1/2/7/4/5/6（无 P0-8）；设计 §6 开头 `:687`“P0 只改渲染不变量与 Canvas 语义（最终架构的一部分）”；设计 §5.2/§6 把 kit 定位时序修复归入 P1（`:56`、`:252`、`:924` 附录 C 的“最小修复集 = kit 定位时序（P1 的 settle/内容闭包覆盖）”）；设计 §0 决策 D4“不做过渡实现”、AGENTS §1.5“不接受先这样以后再换的临时架构”。
- **证据**：`ui_kit/src/kit/overlay.rs:814-864` 新增 `pub(crate) fn measure`（约 40 行，与 `place()` `:871-926` 近重复）；`:867-869` 新增 `pub fn revealed`；`overlay.rs:807-810` 在 `open` 末尾调用 `measure`；`color_picker.rs:1338` 再次调用并静默丢弃 `Result`。设计 §8/P1-11 明确会在 P1 删除 `kit::overlay::{Overlay,...}` 全部类型——这份实现是“P1 会删除的过渡代码”，P0 引入的 `compute_layout` 调用（最多 2 次/次 open）不在设计里，也没有文本主导高度的测试（`review-p0-quality.md` Q-03 已从可行性角度确认该覆盖缺口）。
- **影响**：不是正确性缺陷（门禁全绿、`--dump-overlay` PASS）；但“P0 范围”与设计不一致：若用户裁决 A 真实存在，必须回写设计 §0/§6/§11；若不存在，这是越界改动，P1 还要删除，违反不维护过时设计与“架构一次做到位”。
- **建议修法**：在 §11 补“P0-8（用户裁决 A）：旧 Overlay 首帧 measure 为临时防御，P1-11 删除”，或在提交说明中附该裁决；把 `ColorPicker` 对 `measure` 的 `let _` 改为失败即 `close_now`（同时消掉 quality Q-04）。
- **是否阻塞 P0**：不阻塞（文档/范围一致性）。

#### C-04 Canvas 迁移遗漏：设计 §5.3 明确列入的 `test_support/diagnostics.rs` 仍以窗口绝对坐标构造 Canvas

- **位置**：设计 `:681`（“ui_core 81/22（含 test_support/snapshot/diagnostics）”）；`crates/ui_core/src/test_support/diagnostics.rs:397-410`（`DamageOverlay::canvas`）：`union` 由 `scale.to_logical_rect(*rect)`（**窗口/设备坐标**的 damage rect）求并，`Canvas::new(union)` 直接把窗口绝对 rect 当作画布 bounds，内部 quad/edge 也用窗口绝对坐标。
- **证据**：该函数在 P0 未修改（`git diff` 里 diagnostics 只改了 `:585-600` 的测试）；新契约（设计 `:562`、实现 `paint/pass.rs:1681-1710`）要求 Canvas 内部坐标为节点局部坐标，并由引擎加节点偏移。当前无调用方（全仓 `rg DamageOverlay` 只有定义与自身测试，`:785-803`），所以不是用户可见缺陷；但它是设计点名的迁移文件，属于“132 处全量迁移”声明不成立的一处。
- **影响**：若后续把这个 public test-support helper 挂到 Canvas 节点（其原意就是可视化 damage），内容会被再平移一次节点 origin；也让“迁移清单已全量”的声明不准确。
- **建议修法**：把 `DamageOverlay::canvas` 改为局部坐标（union 归一到 `LogicalPoint::ZERO`，quads 相对 union 平移），或若确认废弃则直接删除（AGENTS §4：不再使用的代码直接删除）。
- **是否阻塞 P0**：不阻塞。

#### C-05 `--pixel-check` 的“移动后残影”断言只检查旧面板 rect 内缩 6px 的 4 个角点，且该阶段不做基线对比；当前 fixture 实际只覆盖 2 个点

- **位置**：`ui_kit/examples/gallery.rs:2918-2961`（`assert_region_restored`：`corners` 只取 `region` 四角 `inset=6`）；`:3260-3271`（move 阶段：`pixel_check_settle(..., "move", false)`，`compare_background=false`；随后 `assert_region_restored(..., old_popup)`）；排除区 `:2875-2885`（`observe` 内 popup/trigger 各外扩 16px，`pixel_check_exclusion` 在 `:2992-2999`）。
- **证据（按实测几何计算）**：`--pixel-check` 输出 `stable=(496,91,300,375)`、`moved=(496,4,300,375)`；旧面板四角内缩 6px = `(502,97),(790,97),(502,460),(790,460)`。移动后 popup 外扩 16 → `x∈[480,812], y∈[-12,395]`，把 y=97 的两个上角全部排除；trigger C `(620,300)` 外扩 16 不覆盖下角。因此实际只对**下沿 2 个点**与打开前基线比较。
- **影响**：设计 P0 验收②要求“移动后”基线对比；若 damage 漏掉旧面板内部非角点区域（例如 Canvas 内容/旧 footer 中段）或旧面板阴影外溢（在 rect 外，4 个内缩点都看不到），这条断言会 PASS。也就是说，`--pixel-check` 对“移动后残影”的证据强度显著低于“打开/关闭”（后者分别是全帧 `compare_pixels(&before, current, [popup,trigger])` 与 `assert_restored` 全帧比较 `:2903-2916`）。
- **建议修法**：move 阶段也做全帧基线对比（`compare_background=true`，排除**新** popup+trigger 即可，旧面板区域必须与打开前一致），并把旧面板矩形外扩阴影量后取 4 角；或者在 harness 里先断言 `changed_pixels ⊆ damage`（已有）再把旧区域以矩形面积统计（而非 4 点）。
- **是否阻塞 P0**：不阻塞（与 C-01 的“②不可验证”同属门禁强度问题）。

#### C-06 P0-2 的“顺序变化”不变量没有测试；且测试落点与设计不符

- **位置**：设计 `:570`（I2：“lane 释放/消失/顺序变化必须使 `buffer.ops` 重建或精确移除”）与 `:699`（P0-2“只补 lane 释放/顺序变化必须使 ops 一致的防御性不变量测试”，落点 `paint/instance.rs`、`render/delta.rs`）；实现只新增 `render/compose.rs:620-661` 的 `releasing_a_lane_rebuilds_the_ops_without_the_freed_range`。
- **证据**：该测试显式 `scene.release_lane(removed)` + `scene.set_order(remaining)` + `builder.note_removed(...)`，只覆盖“释放+移除”；没有“lane 集合不变、仅 order 变化（`Scene::set_order`）时 ops 重建”的用例；`LaneStore::apply` 的 order 变化分支（`render/delta.rs:386-393`，设置 `fast=false`）无测试直接命中。测试也放在 `compose.rs` 而非设计指定的 `instance.rs`/`delta.rs`。
- **影响**：P0-7 现在依赖 order 变化走全量 ops 重建（`compose.rs:87-100`）；缺该用例时，未来 `LaneStore`/encoder 的“快路径”回归不会被本不变量拦截。属“缺失必需测试”。
- **建议修法**：在 `render/delta.rs` 或 `compose.rs` 补一条“仅 `set_order`（lane 集合与内容不变）⇒ `fast_encodes()==0` 且 ops 顺序更新”的测试；把现有测试移到设计指定文件或回写设计落点。
- **是否阻塞 P0**：不阻塞（当前生产逻辑正确）。

#### C-07 P0-7 恢复的 P0-3 要求只有“滚动容器注册/移除”覆盖，设计 §7.2.4 的 virtual list 增删 + popup 开关 + “ScrollTable 无 stale slot”没有用例

- **位置**：设计 `:700`（P0-3 并入 P0-7：node→scroll slot 表在新增/删除/顺序变化时一致）、`:786`（§7.2.4“scroll slot：集合变化重映射；virtual list 增删 + popup 开关；`ScrollTable` 无 stale slot”）；实现测试 `app/removal_tests.rs` 的 `registering_a_scroll_container_is_the_only_structural_full_downgrade`、`adding_and_removing_scrolled_content_stays_incremental`；`window/paint.rs:258-299` 的 `sync_scroll_offsets` 只有 `debug_assert` 长度平行、无“stale slot”断言。
- **证据**：`window/paint.rs` 的全量降级条件是 `registry_changed = scroll_reg_epoch != painted_scroll_reg_epoch`（`:83-85`），`scroll_reg_epoch` 只在 `make_scroll`、`remove` 滚动容器、`append_child` 跨滚动容器时 bump（`arena.rs:393-399`、`:357-364`）。**新增/删除滚动容器内部的行不 bump**（正确，因为 slot 表不按行变化），但没有测试锁定“行增删 + virtual list 重开窗 + popup 开启期间 ScrollTable 仍一致”；`pixels.rs:943` 的滚动重放用例只覆盖“滚动 → 重放”的像素一致性。
- **影响**：P1 的 Select/Dropdown（内部 List/ScrollBar）会大量走这组路径；缺失测试意味着 P0 声称“P0-3 并入 P0-7 已完成”缺少可验证证据。
- **建议修法**：用 `TestApp` 建一个 virtual list，执行“滚动 → 增删条目 → 打开/关闭一个 overlay”三联操作，断言 `record.delta.scroll` 与 `window.scroll_slots()` 无 stale 项、`!damage_full`，并与全量基线像素一致（可复用 `PersistentPixels`）。
- **是否阻塞 P0**：不阻塞（当前逻辑未发现错误）。

#### C-08 设计 §11 仍为空，P0 的 deviation/验收状态没有按 DoD #7 回写；`p0-summary` 的偏差清单也不完整

- **位置**：设计 `:872`（DoD #7“实施后在本文件 §11 回写偏差、最终 API、测试/性能基线”）、`:876-878`（§11 空）；`p0-summary.md:22-31`（已知偏差 1–8）。
- **证据**：设计 rev.6 相比 P0-8、P0-1 的 L 空间（vs `DeviceRect`）、`damage` 阈值改成 frame-intersect 后面积（`render/damage.rs:96-113`，summary 偏差 5 只有一句）、`--pixel-check` 及 `PersistentPixels`/`Overlay::revealed` 的 P1 迁移、C-01 的“②只有 headless 证据”、C-04 的 diagnostics 遗漏，都没有进入设计文档；§6 P0 任务表也未包含 P0-8。`review-p0-quality.md` 已把“§11 空/内存口径不一致”记为 P3（Q-08），但未覆盖 P0-8/验收②③。
- **影响**：P1 起点会按设计 §8 删除 `Overlay` API，但 `--pixel-check` 与两条 kit 首帧测试依赖它（quality Q-13/R6）；不写 §11 会让 P1 计划漏项。
- **建议修法**：P0 提交时在 §11 写清：P0-8 的性质与删除阶段、headless 像素门禁的边界（验收②的替代证据与用户裁决）、`painted_extent` 空间/内存实测、阈值口径、`--pixel-check` 的迁移登记。
- **是否阻塞 P0**：不阻塞（文档）。

#### C-09 `--pixel-check` 没有接入任何脚本/CI/门禁文档，且与 `overlay-check` 是分离的；DoD #1 的“overlay-check 能检测残影”仍未实现

- **位置**：DoD `:866`（“`overlay-check` 能检测残影（像素断言）”）；设计 P0-5 `:703`（像素门禁的落点 `test_support/*`、`examples/gallery.rs`）；实现 `gallery.rs:155`（新增 `--pixel-check` 解析）、`:3353-3359`（单独分支 `std::process::exit(3)`）；`overlay-check` 代码未改（diff 中无 `overlay-check`/`pass()` 改动，实测仍只查 rect/children）。
- **证据**：全仓检索 `pixel-check`，除 gallery 自身与 `p0-summary` 外无任何脚本/CI/文档引用；`gallery.rs` 中 `--pixel-check` 与 `--dump-overlay` 互斥分支，`--dump-overlay` 仍保留旧语义（符合“P0 不改旧语义”，但也意味着真实窗口无像素门禁）。
- **影响**：门禁依赖 lead 手工执行；后续改动可能只跑 `cargo test` 而漏像素回归。DoD #1 最终要在 P3 实现“overlay-check 像素断言”，但 P0 至少应把 `--pixel-check` 写进阶段门禁清单（设计 §7.5/§10）与提交说明。
- **建议修法**：把 `cargo run -p ui_kit --example gallery -- --pixel-check` 写入 §7.5 门禁列表/CI 脚本；P1 迁移时保留该命令名（gallery 内实现替换为 debug_stack+像素断言），或在 P1-12 任务表显式登记。
- **是否阻塞 P0**：不阻塞。

#### C-10 Canvas 的 `painted_extent` 以 `Canvas::bounds()` 代表“内容外延”，没有并集内部图元的实际外延；契约未禁止图元画到 bounds 之外

- **位置**：`paint/pass.rs:1681-1710`（`extent = canvas.bounds().translate(origin).union(self.bounds)`，`note_painted(extent)` 与 damage 都用它）；`paint/pass.rs:1392-1394`（旧内容通道 `painted_content` 同样只用 `draw.bounds()`）。`Canvas` 默认不裁剪（设计 §2.3 M-B 已记录：`paint/pass.rs` 旧行号 294-301/1577-1590），`Canvas::push_quad` 也不校验 quad 在 `Canvas::bounds()` 内（`paint/canvas.rs`）。
- **证据**：现有测试只覆盖“Canvas bounds 大于节点盒”（`a_rebuilt_canvas_damages_the_previous_content_extent`，`pass.rs:2985-3017`）与节点移动；没有“图元超出 `Canvas::bounds()`”的 extent/damage 用例。若任一 chart/取色器/自定义 Canvas 画到 `bounds` 外（局部坐标合法、也不会被引擎裁掉），移动/隐藏/删除时 damage 会漏掉那部分实际像素，违反设计 I1“damage ⊇ 上一帧实际绘制范围”。
- **影响**：当前 17 个 widgets 文件的 Canvas 内容均在 `Canvas::bounds()`（多数还加了 canvas 内 clip），未发现实际越界，所以是**潜在**契约缺口；但 P0-4 把 `Canvas::bounds()` 从“节点盒”变成了“内容外延声明”，P1 的 PopupPanel 内容裁剪/内容收缩可能第一次遇到“旧内容外延 > 声明 bounds”。
- **建议修法**：把 `PaintedExtent` 的内容外延改为递归并集内部图元（quad/shadow/image/glyph/path/geometry chunk/nested Canvas）的 actual bounds，或在 `Canvas` 契约里明确“`Canvas::bounds()` 即内容外延”并在 `push_*` 做 `debug_assert` + 补一条越界用例（现契约没有文档化）。
- **是否阻塞 P0**：不阻塞（无已知越界调用方）。

### P3

#### C-11 设计 §5.3 的 Canvas 计数 132/45 与实际构造点 124/41 不一致（子串口径误计）

- **位置**：设计 `:681`（“`Canvas::new` 全仓 132 处/45 文件——ui_core 81/22、widgets 37/19、ui_kit 9/2、widget 5/2”）；`p0-summary.md:13-14` 沿用同口径。
- **证据**：用 `(?<![A-Za-z_])Canvas::new\(` 在 `crates/**/*.rs` 上统计：124 处/41 文件（ui_core 85/24、ui_kit 9/2、widget 5/2、widgets 25/13）。设计里的“37/19”包含了 `ChartCanvas::new`/`MapCanvas::new`/`SideCanvas::new` 等**不是** `Canvas` 的构造点；另有新增测试改变计数（+4）。
- **影响**：迁移清单的“全量”判断不能只靠这个数字；实际逐点核对后仍未发现残留绝对坐标（C-04 的 diagnostics 除外）。
- **建议修法**：§11 回写真实构造点清单与例外（diagnostics）。
- **是否阻塞 P0**：不阻塞。

#### C-12 Canvas 局部原点口径三处不一致（设计“节点左上” / 实现“content box（border+padding）” / summary“假设 content_offset=0”）

- **位置**：设计 `:562`（“`Canvas` 内部 quad 坐标 = 节点内容坐标（0,0 = 节点左上）…引擎 emit 时统一加节点屏幕偏移”）；实现 `paint/pass.rs:1681-1685`：`origin = bounds.origin + content_offset`，`bounds.origin` 是节点 border box 的屏幕位置、`content_offset = border+padding`（`node/tree/layout.rs:173`）；`p0-summary.md:27`（偏差 4：“Canvas 局部坐标契约假设 Canvas 节点 content_offset 为 0”）。
- **证据**：`content_offset` 由 layout 写入 = border + padding（`node/tree/layout.rs:173`、`test_support/app.rs:946` 有“content_offset = border + padding”断言）。当前全仓没有带 border/padding 的 Canvas 节点（`rg NodeBuilder::canvas` 附近无 padding/border 生产用例），所以实现实际选择“content box 原点”与设计文字“节点左上”不同，且 summary 的“假设为 0”与代码“显式加 content_offset”不一致。
- **影响**：P1 若给 Canvas 节点加 padding（或把 chart/取色器包在有 padding 的容器里却复用同一 Canvas 节点）会出现内容偏移语义争议；当前无影响。
- **建议修法**：P0 收口时明确“Canvas 局部原点 = 内容盒左上（引擎加 border+padding）”，同步设计 §4.11/summary；或改为 border box 原点并相应改 `origin`。
- **是否阻塞 P0**：不阻塞。

#### C-13 `baked_screen_bounds` 侧表在 `arena::remove` 未清空（与 `painted_extents` 不对称）

- **位置**：`node/tree/arena.rs:431-436`（清 `clip_keys`、`painted_extents`，但未清 `baked_screen_bounds`）；`node/tree/arena.rs:275-278`（`insert_child` 会重置）。
- **证据**：全仓 `baked_screen_bounds` 引用只有 `arena.rs:73,275-278`、`content.rs:677-741`；`remove` 的清理循环没有该字段。由于节点索引复用必经 `insert_child`（唯一分配点，`arena.rs:195`），当前无正确性影响，仅“侧表生命周期不对称”，且 40k 节点下每个已删索引残留 20B 直到复用。
- **建议修法**：在 `remove` 的清理循环里加 `baked_screen_bounds[index] = None`。
- **是否阻塞 P0**：不阻塞。

#### C-14 `p0-summary` 对 P0-1 的描述“lane rebuild/emit 累计（…geometry chunk）”不精确

- **位置**：`p0-summary.md:15`；实现：`PrimitiveEmitter::note_painted` 只累计 quad/glyph/image/Canvas/shadow/ring（`paint/pass.rs:1444-1456`、`:1464`、`:1479`、`:1698-1703`），geometry chunk 的外延由 `paint_geometry_node` 单独 `geometry_chunk_damage(...)` 并集（`paint/pass.rs:1204-1209`）。
- **影响**：仅文档表述；读者可能误以为 geometry 走 emit 累计通道。`review-p0-quality.md` 的 Q-08/Q-09 已覆盖同族问题。
- **建议修法**：summary/§11 改为“quad/glyph/image/Canvas/shadow/ring 由 emitter 累计；geometry chunk 由 paint_geometry_node 并在 extent”。
- **是否阻塞 P0**：不阻塞。

---

## 3. 迁移覆盖率核对表（按 crate）

> 口径：真实 `Canvas::new(` 构造点（正则排除 `ChartCanvas::new` 等子串）；“语义迁移”= 画布内容使用节点局部坐标且引擎只加一次节点偏移（`CanvasDraw.offset`），缓存键/指针/hit-test/几何顶点同步改为局部。

| crate / 区域 | 构造点 / 文件 | 语义迁移结论 | 证据 / 例外 |
|---|---|---|---|
| `ui_core` 生产与共享基建（不含 `#[cfg(test)]`/`*_tests.rs`） | 3 / 3 | **2/3 迁移，1 处遗漏** | `test_support/snapshot.rs:930-938`（golden `build_image` 的 image canvas）已改零原点局部；`examples/demo/scene/resources.rs:52-64` 已改局部；`test_support/diagnostics.rs:397-410`（`DamageOverlay::canvas`）仍窗口绝对坐标 → **C-04** |
| `ui_core` 测试（其余 82 点） | 82 / 21 | 迁移/断言同步 | `canvas_text.rs` 全部 `bounds()` 改为 ZERO 原点（`:264-266`）；`paint/pass.rs` 新增 13 条专测（局部/滚动烘焙/嵌套/125% DPR/旧 extent damage/geometry chunk）；`frame_record.rs`、`taffy_tree.rs`、`hit_test.rs`、`resources_tests.rs`、`geometry.rs` 等构造点均 ZERO 原点或局部 sliver |
| `ui_kit`（color_picker / spinner） | 9 / 2 | **全部迁移** | `color_picker.rs:2195-2200` `node_canvas_bounds` 返回 ZERO+size，`well_canvas/palette_canvas/track_canvas` 全部由该 bounds 推导；`spinner.rs:281` 用 `LogicalRect::from_origin_size(ZERO, size)`；缓存键由 `(bounds,…)` 改为 `(size,…)`（`spinner.rs:54`、`color_picker.rs` sync 的 `last` 键）；kit 首帧两条测试覆盖 overlay 时序 |
| `widget` | 5 / 2 | **全部迁移** | `widget/src/cx.rs`/`test_support.rs` 均为 ZERO 原点（`CANVAS_BOX = (0,0,120,40)`）；`test_support.rs:732` Canvas 图元 bounds 改为 `draw.bounds()+draw.offset`（供 tests 断言画在正确位置） |
| `widgets`（13 文件 / 25 点） | 25 / 13 | **全部迁移（语义层核对通过）** | 图表类：`candlestick/chart/paint.rs:284-290` 传 `LogicalRect::from_origin_size(ZERO,size)`，缓存键 `PaintedKey.size`（`:140`），指针 `chart_position` 减 chart bounds（`view.rs:1283-1289`）；`cryptocurrency_etf`/`perpetual_statistics` 的 `ChartLayout::new(local)` + `point_at` 减 `origin`（`canvas.rs:209/252`、`paint.rs:278/432`）；`prediction_market/chart/canvas.rs:340` local + `paint.rs:134,142` `Canvas::new(layout.bounds)`（layout 已是 local）；`volume_heatmap/chart.rs:332` layout.bounds=ZERO+size、tiles 去 origin、`hit_test` 减 `layout.origin`（`:344-349`）；`order_book/paint.rs:87,119` 与 `prediction_market/book/panel.rs:897` 由 `size` 建 local；`gauge.rs:332-339` local（geometry 顶点同源）；`map.rs:363-372` local、`land_transform` 去 bounds origin、`attribution_contains`/`vessel_at`/`zoom_at` 减 `bounds.origin`；`timeline.rs:755-768,1061-1130` local、文本/quad 去掉 `origin` 平移；`spinner`/`gallery` 同 ui_kit 规则 |
| 其它 crate（`kytrix`、`instrument_selector_ui`、`profile_ui`、`settings_ui`、`market_participants_ui`、`widgets/*` 未列 crate） | **0** | 无需迁移 | 全仓 `Canvas::new` 只出现在上表 4 个目录（`rg` 证据） |
| 全仓合计 | **124 / 41**（设计写 132/45，见 C-11） | 123/124 迁移 | 唯一遗漏：`test_support/diagnostics.rs`（C-04） |

**行为语义抽查（不是只编译通过）**：
- 滚动烘焙只加一次：`paint/pass.rs` 测试 `an_ancestor_scroll_bakes_the_canvas_content_exactly_once`（滚动 30→offset 0；滚动 10→offset 20）通过；编码器 `encode_primitive(primitive, offset)` 递归 `canvas_offset = offset + draw.offset`（`instance.rs`），嵌套画布测试 `nested_canvases_accumulate_their_offsets` 通过。
- 位置变化强制重建：`content.rs:720-741` 把 `baked_screen_bounds == screen_bounds` 加入 reuse 判据；测试 `a_layout_change_invalidates_a_cached_canvas_lane_on_the_first_frame` 通过。
- 旧 extent damage：`moving_a_canvas_node_damages_the_previous_content_extent`、`moving_a_shadowed_focused_node_damages_old_and_new_painted_extents`、`hiding_a_geometry_node_clears_its_chunk_extent_and_damages_it`、`removing_a_never_painted_node_falls_back_to_the_layout_bounds` 通过。
- 图表交互语义：candlestick 指针测试全部改为 `window_point(local)=bounds.origin+local`（`view/tests.rs`）；order_book 深度条期望 origin 由 85→75（局部化后的正确值）；gauge/volume_heatmap/prediction_market 测试期望同步改为局部坐标，未发现“只编译通过、画错/点错”的残留。

---

## 4. 已核实为正确的点

1. **门禁声明可复现**：`fmt --check`、`clippy -p ui_core -p ui_kit -D warnings`、`check --workspace --all-targets`、`cargo test -p ui_core -p ui_kit`、`--pixel-check`（108 checks / 45 sampled frames / 0 damage_full / first_visible==stable）、`--dump-overlay --auto-close=9` 真实窗口 PASS，均与 `p0-summary.md:6` 一致（本机独立复跑）。
2. **Canvas 迁移的像素级等价性**：`crates/ui_core/tests/golden/warp/*.png` 未被修改，而 `image_1_light` 等 scene 含 Canvas 绘制；本机直接运行既有 `snapshot-*.exe golden_` 得到 **8 passed / 0 failed**（§1.2 补充行），说明“绝对坐标 → 局部坐标 + 引擎偏移”没有改变渲染结果（不是仅靠单元断言）。
3. **P0-0 结论被尊重**：`paint/instance.rs`、`render/delta.rs`、`render/compose.rs` 的生产逻辑零改动（diff 中只有测试/测试辅助 `instance_rects`），没有按已否证的 M-D/M-E 重写 ops 生命周期；P0-2 只加防御性测试（虽有一处缺口，见 C-06）。
4. **P0-7 结构增量的正确性有独立对照**：`window/paint.rs:731-873` 的 `incremental_matches_forced_full_over_a_scripted_sequence` 用双窗口（增量 vs 强制全量）逐步比对 `lanes/drawn/scroll/damage/delta`，覆盖 30 步（颜色/透明度/文本/滚动/重绘/移动/新增/删除/隐藏）；这比单项断言更强地证明“结构帧增量 = 全量结果”。
5. **P0-1 生命周期覆盖**：lane rebuild（`pass.rs:651-654`）、hide（`:559-616`）、geometry 清空（`:894-897`）、remove 子树并集（`arena.rs:400-417`）、空 lane（`:768` 前后）都有显式 extent 清理/损坏；`PaintedExtent` 只在 `paint` 侧写入、只读访问 `.get()`，无越界/泄漏路径。
6. **像素门禁非空测**：`test_support/pixels.rs:574-638` 主动构造“damage 未覆盖旧区域”的帧，断言持久目标上残影可见（`compare_pixels(...).span > 128`）且补 damage 后自愈；`changed_pixels ⊆ damage` + 与全量基线逐字节对比（`span<=1`）构成双向断言。门禁的“PASS”不是“没有断言”。
7. **P0-2 的 release 路径本身正确**：`render/compose.rs:620-661` 断言 `deltas.removed` 触发 `LaneStore` 全量 ops 重建（`fast_encodes()==0`）且 ops 只覆盖存活 lane 的实例区间（`total==2`）。
8. **P0-6 kit 防御**：`color_picker.rs:1378`（panel.content）与 `:1682`（palette）均 `overflow_hidden()`；新增测试 `the_panel_content_and_palette_clip_their_children` 断言 `Clip::Bounds` 且色板非零；gallery 像素断言覆盖了“面板外不得有旧像素”。
9. **旧 popup 架构与 `--dump-overlay` 语义保持**：`kit::overlay::Overlay`/`Layer`/`Band` 未删、gallery 仍走旧 open/place/reveal；`--dump-overlay`/`overlay-check` 的代码零改动（diff 无该段），真实窗口运行 PASS；P0 没有改 popup 架构（P0-8 只加了首帧 measure，见 C-03）。
10. **P0-4 的破坏性迁移是“硬替换”而非兼容层**：`Primitive::Canvas(CanvasDraw)` 直接替换 `Arc<Canvas>`，`CanvasDraw` 只多 `offset`；`node_bounds` 改名 `node_canvas_bounds` 并改语义，未保留旧函数；全仓无 `From`/别名/双载荷，符合 AGENTS §1.1。
11. **测试有效性核查**：无测试删除、无 `#[ignore]` 新增、无容差放大、无空断言；按 diff 新增 `#[test]` 精确计数为 **29 条**（`removal_tests` 7、`pass.rs` 13、`pixels.rs` 5、`compose.rs` 1、`color_picker.rs` 2、`overlay.rs` 1），均有真实断言（并出现在 `cargo test` 输出中 `ok`）；gallery 108 checks 里虽然包含逐帧重复断言（quality Q-09 口径问题），但独立不变量约 30 条，门禁强度足够。**未发现门禁虚假通过**：像素 harness 的反例自检（§4.6）和全量基线对比使“漏 damage”必然可见。
12. **AGENTS 规范**：新增行零注释、零 `#[allow]/#[expect]`、零新依赖、零 `#[path]`；错误 API 主体沿用 `EngineError`（仅 2 处 P2 由 quality Q-04/Q-05 记录）；`cargo +nightly fmt` 干净。

---

## 5. 残余风险（未构成 finding，但需在 P0 收口/P1 起点处置）

- **R1（C-01）真实窗口像素无自动化证据**：`--pixel-check` 是 `TestApp` + 独立离屏 `GpuRenderer`（`pixels.rs:52-137`），不经真实 swapchain/DComp；`--dump-overlay` 只断言 rect。DoD §10.1“残影消失”在真实窗口仍靠 P0-0 人工证据。若用户接受该路线，P3-6 必须补真实窗口 readback。
- **R2（C-02/Q-01）滚动上下文 damage**：offset≠0 的删除/跨滚动上下文重挂会在 P1 虚拟列表行回收时命中；summary 已披露 remove，但未披露 `place_subtree`/`append_child` 的同类路径。**P1 列表复用前必须修**。
- **R3（Q-03/C-03）`Overlay::measure` 与真实帧序**：measure 只跑 `compute_layout`，不跑 `layout_text`/`clamp`；文本主导高度（Tooltip/Menu 多行）的首帧 rect 没有测试。P1-2 settle 前应做判别实验，并把 `measure` 与 `place` 合并（quality Q-15）。
- **R4（C-01）widgets 迁移回归面**：17 个 widgets 文件的迁移只被 `cargo check --workspace --all-targets` 与自证测试覆盖，未进本轮门禁；P1 起点建议先跑一次全 workspace `cargo test` 建基线。
- **R5（C-05）像素门禁的“移动后”证据强度**：只覆盖旧面板 2 个角点且该阶段不做基线对比；若 P1 增加 popup 位移/列表重排，应把该阶段升级为全帧基线对比。
- **R6（Q-13/R6）`--pixel-check` 与 `Overlay::revealed`/`Overlay::panel_node` 强绑定**：P1-11 会删除这些 API，P1-12 只承诺迁移 `--dump-overlay`；不走迁移登记的话，P0 的像素门禁会在 P1 直接编译失败。
- **R7（C-02）`PaintedExtent` 的“时点”隐含假设**：extent 在 L 空间，damage 用当前 `total` 反推；任何“绘制后 total 变化再 damage”的路径（滚动/跨滚动重挂/滚动容器直接子节点删除）都有同类风险。P1 若引入 popup 内滚动，必须按设计改用屏幕/设备空间存 extent。
- **R8 GPU 主机依赖**：5 条像素测试硬依赖可用 GPU readback（`gpu_guard` 只互斥、不 skip）；无 GPU 的 CI 会失败而非跳过（与既有 snapshot 测试同源，但 P0 把 GPU 依赖扩到 lib 单测）。
- **R9 交互路径覆盖**：`--pixel-check` 用 `picker.open(app, window)` 程序化打开（`gallery.rs:3227/3300`），不是 P0-0 复现所用的 trigger a11y Invoke/点击；若回归只出现在“由输入事件打开”的帧序（`frame_at_open`/`revealed` 时序），该门禁可能漏检。建议 P1 迁移后补一条“指针点击 trigger → 首帧像素”的用例。

---

## 6. 是否可提交 P0

**可以提交 P0**，与 `review-p0-quality.md` 的“P0=0 可提交”结论一致，依据：

1. 未发现 P0 级 finding：无正确性缺陷、无设计硬要求（C1–C9/I1–I9 中适用项）在 P0 主场景被违反、无 HEAD→P0 用户可见回归、无门禁虚假通过；门禁本机复跑全绿（§1.2）。
2. 设计任务落实：P0-0 结论已回写 rev.6；P0-1 生命周期/damage 主通道有 13 条 pass.rs + 7 条 removal 专项测试；P0-2 无生产改动且 release 不变量有测试（顺序缺口见 C-06）；P0-4 全仓 124 个构造点 123 个语义迁移（唯一遗漏为未使用的 test-support helper，C-04），golden 像素不变；P0-5 持久增量 harness + 5 条测试（含反例自检）；P0-6 裁剪 + gallery 像素断言；P0-7 结构增量 + 双窗口全量对照；P0-8 功能有效但**需登记**（C-03）。
3. 提交前建议的非阻塞动作（低成本，按优先级）：
   1. 设计 §11 回写 C-03/C-04/C-08（P0-8 性质、真实窗口验收②的替代证据、`painted_extent` 空间/内存/阈值、`--pixel-check` 的 P1 迁移）；
   2. 补 C-01 的验收③证据：附一次 widgets/picker 相关 `cargo test` 或 workspace `cargo test` 日志；或明确写“未执行”；
   3. 修 C-02/Q-01（滚动 offset≠0 的 remove damage）留到 P1 起点前，但**在本提交说明中登记为 P1 前置**；
   4. 顺手修 C-04（diagnostics 局部化或删除）与 C-13（remove 清 `baked_screen_bounds`），并把 `ColorPicker::open_panel` 的 `let _ = overlay.measure(...)` 改为失败即关闭（quality Q-04）。
