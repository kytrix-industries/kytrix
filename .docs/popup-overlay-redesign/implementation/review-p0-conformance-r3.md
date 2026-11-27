# P0 一致性审查（第 3 轮 / C2 收口复核）— review-p0-conformance-r3

- 审查者：`p0-review-conformance`，对 `lead` 负责
- 对象：worktree `E:\kytrix\.agents\worktrees\dsh-1d175414-1a41`，HEAD `71a5a5cc` + 未提交 P0 工作树（58 modified + 1 untracked `src/paint/extent.rs`；本轮新增改动集中在 `gallery.rs` 21:15、`pass.rs` 21:15、设计 `21:19`、`p0-summary` 21:19、`kit-gallery.md` 21:17）
- 边界：只读，未改任何实现文件；未运行其它 crate 测试
- 结论摘要：**C2-01/02/04/05/06 = fixed；C2-03 = partially（文档残留 1 句自相矛盾）；新 findings：C3-01、C3-02（均 P3 文档）。P0/P1/P2 = 0。代码与 4 项门禁全绿；修掉 2 句文档后即可“清零提交”，无需改代码或重跑门禁。**

---

## 1. C2-01…C2-06 复核

| ID | 结论 | 证据 |
|---|---|---|
| C2-01 | **fixed** | §11.2 P0-2 `:906` 已改为“防御性测试 + R-04 生产修复”，§11.3-21 `:937` 如实登记两处修复（`delta.rs:394-408` 先校验 generation 再清 lane；`:339-345` 清 `versions[slot]` 前校验 generation），并引用 `compose.rs:664` 与 `delta.rs:856/961/1110` 三条既有用例（均存在、PASS）；`p0-summary:24` 同步删除“无生产改动” |
| C2-02 | **fixed** | 4 项门禁实测：`cargo test -p ui_core -p ui_kit` = **3554 passed / 0 failed / 2 ignored**（ui_core lib **2720**/1 ignored、frame_record 8、layout 4、snapshot 14、demo 39、ui_kit 769），与 §11.5 `:953`、`p0-summary:10` 完全一致；`Canvas::new` 实测 **132 处/41 文件**（ui_core **93/24**、ui_kit 9/2、widget 5/2、widgets 25/13），与 §11.3-7 `:923` 一致；新增测试实测 **pass +19（70=51+19）/ removal +11（26=15+11）/ pixels 7**，与 §11.2 `:905,908,910`、§11.5 一致；`--pixel-check` 输出 `checks=108`，`gallery.rs:2801` 是唯一 `checks += 1`（`record()` 内），`3271` 手工加数已删除，108 分解 `45+20+20+3+2+18` 与代码路径逐一对应（18 条具名里含 `old-region-coverage`；`idle-frames`=常量 20；`restored`=3 次；`close-frames`=2 次）；§11.3-3 `:919` 与 §11.7 `:984` 的“overlay 5 条 + ColorPicker 1 条”现已同口径 |
| C2-03 | **partially** | §11.3-10 `:926` 新增段落正确（Overlay::open 1 次 refresh_text+1 次 compute_layout；ColorPicker open_panel 再 1 次，合计 2 text pass + 2 layout；第二次通常空操作），与 `overlay.rs:811-826`、`overlay.rs:802`、`color_picker.rs:1338` 一致；**但同段末尾仍保留旧句“measure 每次 open 最多 1 次 text pass + 2 次 compute_layout”，与前半段“ColorPicker 路径共 2 次 text pass”自相矛盾**（见 C3-01） |
| C2-04 | **fixed** | §11.3-20 `:936` 已改为“混合空间”：Quad/GlyphRun/Image/Path/Shadow/ClipPush/Canvas 按 lane/绘制空间（Canvas 内部按 offset 累加）；Geometry/ClipPop/Text 回退 `layout().bounds`(L 空间)，并明确“P0 未改、P1 统一前不得依赖单一口径”。与 `record.rs::primitive_kind :291-319` 逐分支核对一致（Geometry/ClipPop/Text 使用 `node_bounds`，其余 `translate(offset)`，Canvas 为 `draw.bounds()+draw.offset+offset`） |
| C2-05 | **fixed** | 覆盖下限：`gallery.rs:2785` `PIXEL_CHECK_MIN_COMPARED_PERMILLE=30`，`:2958-2978` 计算 `required = fixture_pixels*30/1000` 并要求 `compared >= required`（独立 `old-region-coverage` check，失败即 exit 3）；负向自检 `:3066-3091` 取比较集 `first`/`last` 两像素注入品红并要求 `mismatched >= 2`（`first==last` 直接 false）。**文档“26104/480000”可复算**：旧浮层 (496,91,300,375) 外扩 16、裁到 800×600，排除移动后面板 (外扩——y≤395)、原 trigger A (620,470,30,30 外扩 16，占 62×28) 后，比较集 = 320×87 − 62×28 = **26104**；fixture=800×600=480000，30‰ 下限 14400，余量 1.81×，与 §11.3-9 `:925`/§11.5 `:957` 完全一致。实测 gate PASS（108 checks，0 damage_full） |
| C2-06 | **fixed** | §11.1 `:896` 范围已列 `error.rs`，`:897` 新增公开错误面 `EngineError::PixelHarness{ cause }`/`PixelHarnessCause`（4 cause），§11.4 `:944` 纳入 API 清单；`kit-gallery.md` 已新增 §13（`:696-722`：13.1 `--pixel-check`、13.2 P0-8、13.3 `--dump-overlay` 语义、13.4 与旧批次关系），mtime 21:17 晚于 P0 改动 |

**C2-03 之外，C2 六条均关闭。**

---

## 2. 本轮新改动扫描

1. **`paint/pass.rs` 的 extent 重写（R2-01）**：`canvas_content_extent` 现在维护画布内 `ClipPush/Pop` 栈并逐图元求交（`:1444-1483`），Geometry 改为 `geometry_drawn_bounds` 只并集 `draw.range` 命中的 chunk（`:1516-1563`，经 `chunk_draw_range`，与 encoder `push_geometry_op` 同构；空 `meta.bounds` 回退 `snapshot.bounds()`，保持保守）；新增 3 条测试 `a_canvas_clip_tightens_a_huge_geometry_extent_to_the_canvas_box`（`:3212`）、`a_zoomed_map_style_canvas_redraw_stays_local`（`:3251`）、`a_geometry_draw_range_narrows_the_canvas_content_extent`（`:3304`），断言 `painted_extent` 精确等于收紧后矩形/区块，在 round-2 实现（clip 不计、Geometry 用整快照 bounds）下必红；**未发现 under-damage 路径**：clip 为轴对齐交集（圆角 clip 保守）、chunk bounds 为空回退整快照、transform AABB 对并集保守、`Content::Canvas` 仍 `∪ self.bounds`，与 encoder 的 clip/range 语义同源；与 §11.3-13/17/22 的新描述一致。
2. **`gallery.rs`**：仅 3 处变更——删除手工 `checks += 1`（`3271`）、新增 coverage check（`:2958-2978`）、自检改为首尾 2 点（`:3066-3091`）；`old-region` 由 `mismatched==0 && compared>0` 拆成 `mismatched==0` + 独立 coverage 下限（更强）；无断言放宽。
3. **测试无放宽/无删除/无 `#[ignore]`**：与 r2 快照逐行比对，pass.rs 本轮净增 281 行全部为 extent 实现与 3 条新测试（旧的 37 行 extent 代码被替换），**被替换行中无任何 assert 行**（0 条断言删除）；gallery 仅新增/强化检查；本轮新增行无 `//` 注释、无 `#[allow]/#[expect]/#[ignore]/#[path]`、无新依赖、无 Cargo 变更。
4. **文档改动**：§11.2/§11.3-7/§11.3-9/§11.3-10/§11.3-13/§11.3-17/§11.3-20/§11.3-21/§11.3-22、§11.4、§11.5、`p0-summary`、`kit-gallery §13` 均已随实测回写；除 C3-01/C3-02 外未发现新数字偏差。

---

## 3. 门禁（本轮实测）

| 门禁 | 结果 |
|---|---|
| `cargo test -p ui_core -p ui_kit` | **EXIT 0**：ui_core lib 2720 passed/1 ignored（另 `pacing_work_...` 单跑 1 passed）、frame_record 8、gui_smoke 1 ignored、layout 4、snapshot 14、demo 39、ui_kit 769；合计 **3554 passed / 0 failed / 2 ignored** |
| `cargo check --workspace --all-targets` | **EXIT 0** |
| `gallery --pixel-check` | **EXIT 0 / PASS**：`checks=108 sampled_frames=45 damage_full_frames=0 first_visible==stable==(496,91,300×375) moved=(496,4,300×375)`；含 coverage floor 与 2 点自检 |
| `gallery --dump-overlay --auto-close=9` | **EXIT 0 / PASS**：`overlay-check PASS (menu=Some((6,180.0,127.5125)) picker_open=true picker_children=Some(4))` |
| 补充核对 §11.5 | `cargo +nightly fmt --all -- --check` **EXIT 0**；`cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` **EXIT 0** |

---

## 4. 新 findings

### C3-01 [P3] §11.3-10 末尾残留旧句，与同段新写的“2 次 text pass”自相矛盾

- 位置：`popup-overlay-redesign-design.md:926`。同段已有正确表述：“ColorPicker 路径每次 open 共 2 次 text pass + 2 次 layout，第二次 `refresh_text` 通常为空操作”；但该段最后一句仍写“measure 每次 open 最多 1 次 text pass + 2 次 compute_layout（open 路径 + ColorPicker 内容二次测量）”。
- 代码事实：`Overlay::open` → `measure`（1 次 refresh_text + 1 次 compute_layout，`overlay.rs:802, 811-826`）；`ColorPicker::open_panel` → 再次 `measure`（第 2 次 refresh_text + 第 2 次 compute_layout，`color_picker.rs:1338`）。因此“最多 1 次 text pass”错误，应为“最多 2 次 refresh_text 调用（第二次通常为空操作）”。
- 建议：删除末句或改为“单次 `measure` 调用 = 1 次 refresh_text + 1 次 compute_layout；ColorPicker open 路径共调用 measure 2 次”。`p0-summary:43` 的表述已正确，可照抄。**这是 C2-03 的残留，修掉即关闭。**

### C3-02 [P3] `p0-summary:36` 的“+8”组成表述自相矛盾（124 + 5 ≠ 132）

- 位置：`implementation/p0-summary.md:36`：“含 P0 新增测试 +8：首轮 124/41 + 修复轮 5（含 R2-01 +3）”。
- 算术：124 + 5 = 129 ≠ 132；实际为 124 + 修复轮 5 + R2-01 3 = 132。设计 §11.3-7 `:923` 的“后续修复轮 5 个（其中 **R2-01 再 +3 个**）”是正确写法。
- 建议：p0-summary 改为“124/41 + 修复轮 5 + R2-01 再 +3”（或“修复轮共 8，其中 R2-01 3”）。**仅 p0-summary 一处，改 1 句。**

除上述 2 条文档 P3 外，本轮无代码/测试 findings；P0/P1/P2 均为 0。

---

## 5. 是否可清零提交

- **代码与门禁**：可提交（4 项门禁 + fmt/clippy 全绿；C2-01/02/04/05/06 关闭；C2-05 的 26104/480000 已由本审查独立复算确认；新增 extent 收紧有 3 条 round-2 下必红测试，无测试放宽）。
- **清零判断**：**现状不是全 0**——C2-03 残留（C3-01）与 p0-summary 组成数字（C3-02）共 2 条 P3 文档。两者都是单句文档修正，不需要改代码、不需要重跑门禁；改完后 **P0/P1/P2/P3 全 0，可提交**。若 lead 决定以“代码 / 门禁零缺陷 + 2 条 P3 文档遗留”提交，也可行，但不应在提交说明中称“清零”。
