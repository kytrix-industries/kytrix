# P0 对抗性审查（第 3 轮 / 收口轮）：渲染 / paint / damage lens（reviewer: p0-review-render）

- 审查对象：worktree `dsh-1d175414-1a41`（HEAD `71a5a5cc` + 未提交 P0 改动；本轮 `paint/pass.rs` 的 `canvas_content_extent` 重写为 clip-aware + drawn-range-aware，+3 条测试）
- 只读：未修改仓库任何文件；仓库仅 `.docs` 下新增本报告（被 `.gitignore` 的 `.*` 忽略）
- 结论：**R2-01 已真正关闭**；代码层面**本轮无 finding**（P0/P1/P2/P3 全 0）；唯一残余是 **§11 文档回写滞后（P3，文档，一句改写，不阻塞提交）**。**可提交。**

---

## 1. R2-01 复核：closed

### 1.1 实现（`crates/ui_core/src/paint/pass.rs:1444-1563`）

- `canvas_content_extent` 现在维护**画布内 clip 栈**：`Clip(Push)` → `saved.push(clip)`、`clip = Some(prev ∩ rect)`；`Clip(Pop)` → `clip = saved.pop().flatten()`；每个图元外延经 `clipped_extent(bounds, clip)` 求交后再并集；嵌套 Canvas 递归后按 `inner.offset` 平移再与当前 clip 求交。
- `primitive_extent` 拆出各图元外延（quad/image/path/glyph/shadow-pad）；`geometry_draw_extent` + `geometry_drawn_bounds` 改为**只并集 `draw.range` 命中的 chunk AABB**（`chunk_draw_range(...).is_none() → continue`；`meta.bounds` 为空时回退 `snapshot.bounds()`，与 encoder 的 `push_part` 回退完全同构），再 `transformed_chunk_aabb(draw.transform, local)`。
- 调用点不变：`emit_content` 仍为 `canvas_content_extent(...).translate(origin) ∪ self.bounds`，同一并集用于 `painted_extent` 与 damage（`pass.rs:1759-1773`）。

### 1.2 语义核对（逐项，全部通过）

| 检查点 | 结论 |
|---|---|
| map 场景（大 AABB + 显式 clip + zoom）不 `damage_full` | 是。clip 把 `zoom × region` 的 Geometry AABB 收紧到画布盒；两条新测试直接覆盖首绘与增量重建（见 1.3） |
| C-10 未回退（无显式 clip 的越界内容仍进 extent/damage） | 是。`clip=None` 起点 + 无 Push 时不求交；round-2 用例 `a_canvas_with_content_outside_its_declared_bounds_damages_the_actual_content` 仍绿；新用例 3 断言**无 clip** 时 extent = 图元实际并集（8×8 节点盒之外的 4096 索引块） |
| 嵌套 clip | 是。`clip` 栈按 `prev ∩ rect` 累积，逐层收紧，与 encoder `ClipState` 的求交语义一致 |
| ClipPop 配对 | 平衡时精确恢复；**不平衡 Pop** 时 `saved.pop()==None → clip=None`（放宽）——方向保守（只可能多 damage，不漏 damage）；encoder 对不平衡 Pop 是 no-op，故 extent 只会比实际更宽 |
| range 为空 / 越界 | `chunk_draw_range` 返回 `None` → 该 draw 外延为空（与 encoder 不产生 op 一致：既有 `path_draws_every_chunk`/空 range 用例锁定） |
| cross-chunk range | 命中多个 chunk 时逐个 AABB 并集；`FULL_INDEX_RANGE` 等价全块（新用例 3 同时断言 `full == all chunks`、`0..4096 == chunk0`） |
| shadow pad ∩ clip | 先 `rect+offset` 按物理 blur pad 外扩（与 emitter 的 `note_shadow` 同式），再与 clip 求交；encoder 的 shadow 实例同样被 clip 裁剪，语义一致 |
| 嵌套 canvas | 内层自身 clip 在递归内处理，外层 clip 对内层整体外延（已含 `inner.offset`）再求交 |
| 旋转/多 chunk 的保守性 | 先用并集 rect 再变换 = 对旋转是保守外扩（⊇ 逐块变换并集），方向安全 |
| `meta.bounds` 为空 / 快照缺失 | 与 encoder 同构（`snapshot.bounds()` 回退 / 空外延），不会出现“encoder 画了但 extent 没算”或反之 |
| 性能 | 每 Geometry 图元多一次 `O(chunks)` 遍历；map（单体网格 × 数个 visible range）量级 ~10³–10⁴ 次比较/重建，相对同帧 re-encode 可忽略 |

### 1.3 新增测试（3 条，均**可失败**：在 round-2 实现下会红）

1. `paint/pass.rs::a_canvas_clip_tightens_a_huge_geometry_extent_to_the_canvas_box`：10000×5000 Geometry AABB + `push_clip(200×100)` → 断言 `!report.damage.full` 且 `painted_bounds == node_box`（**精确相等**）。round-2 实现下 extent=10000×5000 → 必然 full（红）。
2. `paint/pass.rs::a_zoomed_map_style_canvas_redraw_stays_local`：20000×10000 网格 + `Transform::scale(2,2)` + clip 到画布盒（复刻 map 的 `push_clip` 模式）→ 首绘 `!damage.full` 且 damage 覆盖节点盒；再 `mark_paint` 走 `paint_incremental(&[node])`（真实增量，非 full）→ 仍 `!damage.full`。round-2 下 40000×20000 外延 → full（红）。
3. `paint/pass.rs::a_geometry_draw_range_narrows_the_canvas_content_extent`：多 chunk 几何，`range=0..4096` 的 extent **精确等于 chunk0**，`FULL_INDEX_RANGE` 的 extent 等于全 chunk 并集。round-2（整份 snapshot AABB）下两者都等于整体，断言会红。

三条覆盖了「map 大 AABB + clip + zoom → `!damage_full`」、C-10 不回退、drawn range 收窄三个要求。

---

## 2. 本轮新代码扫描（扩展点）：未发现新问题

- `canvas_content_extent` clip 栈语义：见 1.2（嵌套/失配/空 range/shadow/嵌套 canvas 均核对通过）。
- drawn range 计算：与 encoder 的 canvas 分支逐行同构（同一 `chunk_draw_range` 参数、同一 `meta.bounds`/`snapshot.bounds()` 回退、同一 `transformed_chunk_aabb`），不存在“extent 与实际绘制不一致”的新分叉。
- 新测试真实性：3 条都是“改了实现就会红”的断言（见 1.3），非自证/快照式假绿；无 `#[ignore]`、无放宽容差。
- 顺带复核本轮加强的像素门禁（§11.3-9）：`PIXEL_CHECK_MIN_COMPARED_PERMILLE=30` 覆盖量下限（`gallery.rs:2959-2977`）、负向自检注入 **≥2 个分散像素**并要求 `mismatched >= 2`（`:3090`）、`idle-no-animation` 前置断言（`:3362`）——都比 round-2 更抗“排除区膨胀/空洞通过”，未发现新的假绿路径。
- 未发现回退项：round-2 已确认的 Path 修复、`arena::remove` 逐节点滚动换算、`order_rebuilt` 短路、`record_primitives` 递归、R-08 `hit_test` 换算均未被本轮触碰（`ui_core` 用例数 2717→2720 = 仅 +3，与新增测试一致）。

---

## 3. 门禁（本轮实测，EXIT 均 0）

| 门禁 | 结果 |
|---|---|
| `cargo test -p ui_core -p ui_kit` | **EXIT=0**：ui_core lib **2720 passed / 1 ignored**（2721 用例，较上轮 +3）、`frame_record` 8、`layout` 4、`snapshot` 14、`examples/demo` 39、ui_kit **769 passed**；`gui_smoke` 1 ignored → **3554 passed / 2 ignored / 0 failed** |
| `gallery --pixel-check` | **EXIT=0**：`checks=108 sampled_frames=45 damage_full_frames=0`；`first_visible==stable==(496,91,300×375)`；`PASS (108 checks; residue = old-region baseline scan with coverage floor + 2-point injected-fault self-check)` |
| `gallery --dump-overlay --auto-close=9`（真实窗口） | **EXIT=0**：`overlay-check PASS (menu=Some((6,180.0,127.5125)) picker_open=true picker_children=Some(4))`；panel/footer/hex/confirm rect 与设计 O1 一致；9 s 自动关闭、事件循环干净退出 |

---

## 4. 唯一残余：§11 文档回写滞后（P3，文档，非代码）

R2-01 的实现已变，但设计 §11.3 与 p0-summary 仍有 **两句与代码矛盾的旧措辞**：

1. `popup-overlay-redesign-design.md` §11.3-13：「`PaintedExtent` 不计 clip（保守外扩，极大画布被小 clip 裁剪时删除 damage 偏大但不漏）」——**括号里的例子正是本轮修掉的场景**；现在 extent 会计入 Canvas 内部显式 clip（仅不感知节点/祖先 clip）。
2. 同文件 §11.3-17 与 `implementation/p0-summary.md` 偏差 6：`canvas_content_extent` 被描述为「Geometry 经 `draw.transform` 的 AABB」，未提**画布内 clip 栈**与**drawn range 命中的 chunk AABB**。

**建议**：提交前把 §11.3-13 改为「extent 计入 Canvas 内部显式 clip，不感知节点/祖先 clip（后者仍保守外扩）」；§11.3-17 / p0-summary 偏差 6 增补「维护画布内 clip 栈；Geometry 用 `draw.range` 命中的 chunk AABB（`a_canvas_clip_tightens_*`/`a_zoomed_map_style_canvas_redraw_stays_local`/`a_geometry_draw_range_narrows_*` 三条用例）」。各一句，不涉及代码。

> 另：§11.3 未新增 R2-01 条目（只在 §11.3-7 的 Canvas 计数里提到「R2-01 再 +3 个」测试）；若希望偏差清单自洽，可把上述内容作为 §11.3 新条目（如 #22）登记。

---

## 5. 结论

- **R2-01：closed**（clip-aware + drawn-range-aware 实现、3 条可失败测试、map 场景首绘/增量均 `!damage_full`、C-10 未回退）。
- **代码层面本轮无 finding**：P0 = 0、P1 = 0、P2 = 0、P3 = 0。
- 唯一 P3 为**文档回写滞后**（§11.3-13/17 与 p0-summary 偏差 6 各一句），不属于代码缺陷，不阻塞提交；建议 lead 在提交时顺手回写。
- **可以清零提交 P0。**
