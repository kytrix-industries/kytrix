# P0 对抗性审查处置记录（三轮，最终 P0–P3 全零）

审查材料：
- 第 1 轮：`review-p0-render.md`（R-01…R-09）、`review-p0-conformance.md`（C-01…C-14）、`review-p0-quality.md`（Q-01…Q-16）
- 第 2 轮：`review-p0-render-r2.md`（R2-01）、`review-p0-conformance-r2.md`（C2-01…C2-06）、`review-p0-quality-r2.md`（Q2-01）
- 第 3 轮收口：`review-p0-render-r3.md`（0 finding）、`review-p0-conformance-r3.md`（C3-01/C3-02）、`review-p0-quality-r3.md`（Q3-01）

## 第 1 轮处置

| ID | 严重度 | 处置 | 证据落点 |
|---|---|---|---|
| R-01 | P0 | fixed：Path op-bounds 改用变换前局部 AABB | `paint/instance.rs`；3 条测试（encoder 无/带 clip、125% DPR、持久像素 Path 帧） |
| R-02 / Q-01 / C-02 | P1 | fixed：remove 逐节点滚动差换算并入父 damage；move 路径验证正确并加回归；跨滚动上下文 reparent 登记 §11.3-4 转 P1 | `node/tree/arena.rs`；`removal_tests` 3 条 + `pixels.rs` 1 条 |
| R-03 | P1 | fixed：像素门禁覆盖 canvas 内 Path/Geometry | `pixels.rs`；gallery 注入故障自检 |
| C-01 | P1 | fixed：§11 登记 headless 像素门禁口径、六 crate 1216 补跑、golden 8 passed | 设计 §11.3-8/§11.5/§11.6 |
| Q-02 | P2 | fixed：结构帧 order 重建只执行一次 | `PaintReport.order_rebuilt`；`a_structural_frame_rebuilds_the_order_once` |
| Q-06 | P2 | rejected（有测试锁定）：Geometry 布局移动 damage 正确 | `a_layout_only_geometry_move_damages_the_old_extent_and_the_new_box` |
| R-04 | P2 | fixed：stale generation removal 不再误删存活 lane | `render/delta.rs`；compose 测试 |
| R-06 | P2 | keep+登记：独立 Geometry 结构变化仍升格 full，I6 边界写明 | §11.3-14；`standalone_geometry_structural_changes_escalate_to_full_damage` |
| R-07 | P2 | fixed：damage 阈值按 frame 裁剪面积有测试 | `the_area_threshold_counts_only_the_visible_part_of_a_rect` |
| C-04 | P2 | fixed：diagnostics Canvas 局部化 | `test_support/diagnostics.rs` |
| C-06 | P2 | fixed：order-only 不变量测试（落点 compose.rs，§11.3-15） | `reordering_lanes_without_removal_rebuilds_the_ops` |
| C-07 | P2 | fixed：virtual list + popup + ScrollTable 一致性测试（§11.3-16） | `removal_tests` |
| C-10 | P2 | fixed：Canvas extent 改为实际图元并集（§11.3-17） | `paint/pass.rs::canvas_content_extent` |
| C-13 | P2 | fixed：remove 清理 `baked_screen_bounds` | `node/tree/arena.rs` + 测试 |
| R-05 | P2 | fixed：`Overlay::open` 内 measure→place→reveal；`place()` 不再承担首次 reveal | `kit/overlay.rs` + 首帧测试 |
| Q-03 | P2 | fixed：measure 加 `refresh_text`，文本主导高度首帧测试（负向自检） | `kit/overlay.rs` + ui_tests |
| Q-04 | P2 | fixed：`ColorPicker::open/open_panel -> Result<bool, OverlayError>` 回滚 | `kit/color_picker.rs` |
| Q-15 | P2 | fixed：measure/place 共用 `place_panel` | `kit/overlay.rs` |
| C-03 | P2 | fixed：设计补 P0-8、修正 P0 范围声明 | 设计 §6/附录 C |
| C-05 | P2 | fixed：gallery move 残影检查改全区域基线扫描 + 负向自检 | `gallery.rs` |
| C-09 | P2 | fixed：`--pixel-check` 语义/退出码/迁移登记 | `gallery.rs` + §7.5/§11.3-9 |
| R-09 | P3 | fixed：`FrameRecord.primitives` 统一 lane 空间（混合空间如实登记 C2-04） | `test_support/record.rs` + 测试 |
| Q-05 | P3 | fixed：`EngineError::PixelHarness`/`PixelHarnessCause` | `error.rs` |
| Q-07 | P3 | fixed：基线不透明断言（排除最外 1 行/列边缘伪影） | `pixels.rs` |
| Q-10 | P3 | fixed：SnapshotImage Debug 摘要 | `test_support/snapshot.rs` |
| Q-11 | P3 | fixed：命名收敛（`layout_bounds`/`needs_screen_bake`/`layout_extent_shift`） | `paint/*` |
| Q-14 | P3 | fixed：damage 覆盖断言改 4 角+中心 | `pass.rs`/`removal_tests` |
| Q-16 | P3 | fixed：`paint.rs` 模块分组/字母序 | `paint.rs` |
| Q-08/Q-09 | P3 | fixed：设计 §4.11/R9 与 summary 的 extent 空间/内存/统计口径修正 | 设计 + summary |
| C-08 | P2 | fixed：§11 完整回写 | §11.1–11.7 |
| C-11/C-12/C-14 | P3 | fixed：Canvas 计数、局部原点口径、summary 精确化 | 设计 + summary |
| R-08 | P3 | fixed：widgets 指针换算改 `NodeTree::hit_test().local_point`；六 crate 补跑 1216 passed（用户裁决 A） | 6 个 widgets crate + 测试 |
| Q-12 | P3 | rejected（复核不构成 AGENTS §6 缺陷） | review-p0-quality-r2 §2.16 |
| Q-13 | P3 | fixed：gallery P1 迁移登记 + idle 采样去混叠 | `gallery.rs` + §8/§11.7 |

## 第 2 轮处置

| ID | 严重度 | 处置 |
|---|---|---|
| R2-01 | P2 | fixed：`canvas_content_extent` 改为 clip-aware + `draw.range` 命中的 chunk AABB；3 条新测试（round-2 实现下必红）；§11.3-13/17/22 回写 |
| C2-01 | P2 | fixed：P0-2 如实登记 R-04 生产修复（§11.2/§11.3-21/summary） |
| C2-02 | P3 | fixed：数字基线刷新为实测（3554/2720、Canvas 132/41、pass 19/removal 11/pixels 7）；gallery 计数删除手工 `checks += 1`，108 = record() 调用数 |
| C2-03 | P3 | fixed：§11.3-10 文本 pass 次数如实（2 次 refresh_text + 2 次 compute_layout，第二次通常空操作） |
| C2-04 | P3 | fixed：§11.3-20 改为混合空间，Geometry/ClipPop/Text 仍 L 空间 fallback |
| C2-05 | P3 | fixed：gallery 覆盖量下限 30‰（实测 26104/480000≈5.44%，余量 1.81×）+ 首尾 2 点注入自检（mismatched ≥ 2） |
| C2-06 | P3 | fixed：§11.1/§11.4 补 `EngineError::PixelHarness`/`PixelHarnessCause`；`kit-gallery.md` §13 同步 |
| Q2-01 | P3 | fixed：同 C2-02 |

## 第 3 轮处置

| ID | 严重度 | 处置 |
|---|---|---|
| R3 | — | 0 finding（render 收口确认 R2-01 closed，代码/门禁无 finding） |
| C3-01 | P3 | fixed：删除 §11.3-10 残留旧句 |
| C3-02 | P3 | fixed：`p0-summary` Canvas 计数算术改为「124/41 → +5 → +3 = 132/41」 |
| Q3-01 | P3 | fixed：108 分解改为「45 条逐帧采样断言（44 changed-outside-damage + 1 full-replay 首帧）」 |

## 结论

三轮对抗性审查后：**P0 = 0，P1 = 0，P2 = 0，P3 = 0**；无遗留、无 deferred（跨滚动上下文 reparent 与 P0-8 临时 measure 属 P1 设计范围内的已知前置，已按设计任务登记在 §11.3/§11.7，不作为 P0 finding）。
