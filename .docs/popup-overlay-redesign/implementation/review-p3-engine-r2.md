# P3 对抗性审查 r2（引擎 lens）— 修复轮收口复核

- 审查员：子 agent `p3-review-engine`（P3 引擎 lens）；轮次：**r2（修复后收口复核，只读）**
- 基线：r1 报告 `review-p3-engine.md`（2026-11-27 16:58，当时工作树 = 未修 P3）
- 当前快照：同一 worktree `E:\kytrix\.agents\worktrees\dsh-1d175414-1a41`，HEAD `f24a3441` + 未提交 P3（含修复轮）
  - `git diff --shortstat f24a3441 -- . ':(exclude).docs'` = **24 tracked 文件，+4357 / −131**（ui_core 16 文件 +1287/−54，ui_kit 8 文件；与 `p3-summary.md` §1 一致）
  - 修复轮新增/修改：`paint/pass.rs`（shadow 向量缩放、去掉 `compose`、paint_scale 键含 pivot）、`popup.rs`（`is_fade_only`）、`popup/animation.rs`（tick 报告 `records`）、`popup/dismiss.rs`、`popup/settle.rs`（settle rounds 回填）、`node/tree/{tree,arena,content}.rs`、`app/frame.rs`、`window.rs`、`window/record.rs`、`test_support/{record,diagnostics}.rs`、`tests/popup_behavior.rs`、`ui_kit/src/kit/{popup_panel,menu,dialog,color_picker}.rs` 等
- 复核手段：只读代码核对 + 门禁复跑 + 定向取证（不改仓库；`git status` 仅 P3 原改动 + 预存 `tools/`）
- **结论：P0 = 0，P1 = 0，P2 = 0，P3 = 2（均为文档/死代码级），另有 3 条观察项。r1 的 9 条 finding 中 8 条确认关闭、1 条（P3E-07）实质关闭但留 1 处文档不一致。引擎 lens 口径：P3 可提交。**

---

## 1. r1 finding 逐条关闭判定

| r1 ID | 严重度 | 修复内容（证据） | r1 必红论证 | 判定 |
|---|---|---|---|---|
| **P3E-01** Shadow.offset 当点变换 | P2 | `paint/pass.rs:300-307` `shadow.offset = LogicalPoint::new(self.length(offset.x), self.length(offset.y))`；新增 `popup_behavior::fade_scale_scales_the_shadow_offset_as_a_vector`（`:5947-6048`）：pivot 固定 `(676,504)`、scale 固定 `0.9915625`、offset `(0,13)`，先断言 `PrimitiveKind::Shadow` 的 rect = `scaled_rect_about(settled.outset(2), pivot, f)`，再用 `frame_dump().instances` 断言 GPU 实例 = `scaled_rect.origin + f·offset`（±1.5），并**显式加回归守卫**：旧 buggy 公式与正确值差 >2px 必须成立 | 旧实现误差 `(1−f)·pivot ≈ (5.7, 4.0)px`，超出 ±1.5 容差 ⇒ 必红；`>2.0` 守卫本身也锁死回归 | **closed** |
| **P3E-02** modal scrim 被施加 0.98 | P2 | `popup.rs:571-574` 新增 `PopupEntry::is_fade_only()`（`is_modal() \|\| tier ∈ {Modal, Scrim}`），`popup/animation.rs:42/50/57` 与 `popup/dismiss.rs:206/210` 统一改用；新增 `popup_behavior::fade_scale_modal_scrim_is_fade_only`（`:6051-6111`）断言 open 中途、close 中途 **modal 与 scrim 的 `paint_scale` 均 == 1.0**，且关闭结束后 scrim 被移除 | 旧实现实测 close 中途 scrim `paint_scale=0.9825` ⇒ 必红 | **closed** |
| **P3E-03** 性能口径（守卫/排序/计时区间） | P3 | ① `app/frame.rs:285` `frame_cpu: Some(frame_started.elapsed())`（`frame_started` 在函数入口 `:30`）⇒ **覆盖 popup tick/timer/poll/tick_animations 等**（旧实现从 tick 之后起算）；② `popup_behavior.rs:5497-5502` `percentile(samples: &[Duration])` 内部 `to_vec()` 排序，**不再原地排序**，打印序列恢复时序；③ 守卫改 `budget = idle_p95 + 2ms`（`:5872`）且以**中位帧**断言（`:5867-5880`），p95 仅记录，并把冷启动说明写进 println；④ 采样改为 open 10 帧 / idle 12 帧 / close ≤12 帧 | — | **closed**（残余说明见 §3-obs1） |
| **P3E-04** idle 断言恒真 | P3 | ① `PopupAnimationTick` 增 `records`（`animation.rs:19,120`），`frame()` 在 `needs_frame` 判定前调用 `note_popup_frame_work`（`frame.rs:65`）把 tick 结果写回统计（`window/record.rs:63-72`）；② `note_idle_cpu` 改 `set_frame_cpu`（只写 cpu，不再 `FrameStats::default()` 覆盖）；③ 测试改为断言 `popup_animation_records == 0`、`!opening && !closing`、`!animations_running(window)`（`popup_behavior.rs:5780-5818`）；④ 统计在每次产出 record 时被 `mem::take`（`window/record.rs:19,29,228`）⇒ idle 帧的 `popup_settle_rounds=0` 是真实每帧值，非上一帧残留 | 旧实现 flags 由 `FrameStats::default()` 构造 ⇒ 恒真；新实现若 tick 真做了工作会置真 ⇒ 有判别力 | **closed** |
| **P3E-05** `paint_scales` 移除未清理 | P3 | `node/tree/arena.rs:610-612` 移除时重置 `(1.0, ZERO)`；测试 `remove_clears_the_extent_and_baked_screen_bounds_side_tables` 扩为断言移除后与**槽位复用后**均为 `(1.0, ZERO)`（`node/tree/tests.rs:128-186`） | 旧实现无该重置 ⇒ 断言必红 | **closed** |
| **P3E-06** `compose` 不可达且公式错误 | P3 | `paint/pass.rs` 已无 `compose`（grep 0 命中），`paint_node` 改为 `.filter(factor != 1.0).or(inherited_scale)`（`:531-537`）；文档 §14.3 明确"content 全部平铺在 popup root 下，单层 transform 足够，`compose` 已删除" | — | **closed** |
| **P3E-07** 文档与实现不符 | P3 | a) §14.2 P3-2 已写"**`FadeSlide` 启用 Menu/ContextMenu/Dropdown/Select**（四者都经 `ItemPanel::open`）"，旧的 D-P3-5（"Dropdown/Select 仍 Fade"）已删除（表重编号）；b) §14.6 D-P3-4 rationale 已改为"pivot 变化会改变绘制位置；安全前提是动画期间 tick 每帧 mark_paint"；c) §14.2/§14.5 性能数字改为"首帧冷启动 p95≈1.0–1.13ms、其余帧中位≈29µs、close 中位≈26µs" | — | **实质 closed**，但 `p3-summary.md:86` 仍保留旧 rationale（见 P3R2-01） |
| **P3E-08** kit `close_with_reason` stale reason | P3 | `ui_kit/src/kit/popup_panel.rs:464-477`：只在 `begin_close_popup` 返回成功后 `reason.set(reason)`；新增 `a_refused_close_does_not_leak_its_reason_into_a_later_close`（`:1370-1394`）：dismiss handler `Ignore` 时尝试 `close_with_reason(AnchorLost)` 不进入关闭事务，随后 `close_now` 的 `on_close` 必须收到 `Programmatic` | 旧实现先写 reason ⇒ 该断言必红 | **closed** |
| **P3E-09** settle pivot 不 mark_paint（缓存键不含 pivot） | P3 | 采用"缓存键含 pivot"方案：`paint_scales: Vec<(f32, LogicalPoint)>`（`node/tree.rs:416`）、`paint_state` 键为 `(f32, LogicalPoint)`（`content.rs:741/776`）、三处裸写入均带 pivot（`pass.rs` 934/1175/1392）；新增单测 `the_paint_reuse_key_includes_the_scale_pivot`（`node/tree/tests.rs:1436+`）：factor 相同、pivot 不同必须 `!reusable` | 旧键仅 factor ⇒ 第二条断言必红 | **closed** |

> 备注：P3E-01/02/09 的修复都有"在旧实现下必红"的判别力论证（含 r1 阶段的定向 probe 实测：阴影实例落在 buggy 位置、scrim `paint_scale=0.9825`），不属于"改了但测试测不到"的假绿。

---

## 2. 新增测试质量抽查

| 测试 | 强度 | 备注 |
|---|---|---|
| `fade_scale_scales_the_shadow_offset_as_a_vector` | 强 | 两层断言（primitive rect + GPU 实例 bounds）+ 旧公式 >2px 回归守卫；pivot/scale 用硬断言钉死（`0.9915625`），动画时长/曲线变化会显式报警 |
| `fade_scale_modal_scrim_is_fade_only` | 强 | open/close 两处、modal+scrim 两对象都断言；关闭结束还断言 scrim 移除 |
| `fade_scale_scales_glyph_runs_and_images_in_animation_frames` | 中-强 | 用 `PopupAnimation::None` 的对照 app 取未缩放 bounds/node，再在 FadeScale app 比较 `scaled_rect_about(pivot, f)`；node id 若漂移会 panic（显式失败），尺寸另加 `<` 断言 |
| `the_paint_reuse_key_includes_the_scale_pivot` | 强 | 同 factor 不同 pivot 必须失配（旧实现必红） |
| `idle_frames_do_no_popup_settle_or_damage_work`（改写） | 中-强 | 依据 `mem::take` 语义，flags/rounds 现在是真实每帧值；`!animations_running` 先决条件合理 |
| `popup_open_and_close_frames_stay_within_the_frame_budget`（改写） | 中 | 中位守卫=设计目标；样本数 10/12/≤12，p95 仅打印。见 obs3 |
| kit `a_refused_close_does_not_leak_its_reason_into_a_later_close` | 强 | 直接覆盖 r1 P3E-08 的泄漏路径 |

---

## 3. r2 新发现

### P3R2-01 [P3] `p3-summary.md` §7-D-P3-4 仍是旧（错误）rationale，与设计 §14.6 D-P3-4 冲突

- 位置：`.docs/popup-overlay-redesign/implementation/p3-summary.md:86`：
  > "**D-P3-4** settle 同步 pivot 不 mark_paint：避免动画结束多余重绘/damage_full。"
- 设计 §14.6 D-P3-4 已改为正确表述："pivot 变化**会**改变 scale≠1 时的绘制位置；安全前提是动画期间 `animation::tick` 每帧已 mark_paint；若额外 mark_paint 会在动画结束多触发一次重绘/潜在 damage_full"。
- 影响：两份交付文档对同一决策给出互相矛盾的理由（p3-summary 的说法在 r1 已被证明不成立），后续复核会重复发现。
- 修法：把 p3-summary 该句改成与设计 §14.6 一致（1 句）。不阻塞。

### P3R2-02 [P3] `PopupScale` 的 `Primitive::Text` 分支不可达（`PopupScale` 里唯一的死代码）

- 位置：`crates/ui_core/src/paint/pass.rs:341-344`。
- 论证：`Primitive::Text` 只可能来自 `Canvas::push_text`；而 `emit_content` 在 `self.push(Primitive::Canvas(...))` **之前**已用 `resolved_canvas` 把 `Text` 解析成 `GlyphRun`（`pass.rs:2006` 调用、`:443-483` 实现），解析失败（lines/atlas 不就绪）时该 Text 被**丢弃**（`*skipped += 1`，不 push）；嵌套 Canvas 亦递归解析。⇒ lane 内不存在 `Primitive::Text`，该分支永不命中。
- 影响：`AGENTS.md` §1.2/§6 要求删除无实际需求的代码；且它正是 kit/quality 侧 **P3K-06**（"CanvasText 的 lane scale 只缩放 origin"）的立论基础——按上述路径，`Text` 在缩放前已被解析，**P3K-06 的缺陷不可达**，可从 P4 改为 closed/rejected（或保留一句"防御性但不可达"说明）。
- 修法：删除 `PopupScale::primitive` 的 `Text` 分支（`Primitive` 仍保留 Text 变体供 Canvas 构造），并在 P3K-06 的处置记录里写明不可达论证。不阻塞。

### 观察项（不计入 finding 计数）

- **obs1（P3E-03 残余，已登记）**：`frame_cpu` 现在覆盖 `App::frame` 全程，但 committed 帧仍不含 `commit_frame`/compositor submit；`a11y-only` 与 `begin_frame` 失败早退不写 cpu——这两种早退**不产出 FrameRecord**（`record` 不被更新），因此不存在"读到错误帧统计"的误导，仅是覆盖范围说明。设计 §14.3/D-P3-7 已写明"非单 popup tick 计时"。可留 P4。
- **obs2（测试常量）**：新 shadow 测试用 `instance.kind == 3u32` 表示 `KIND_SHADOW`（`instance.rs:85` 为 `pub(crate) const KIND_SHADOW: f32 = 3.0`，集成测试不可见）。集成测试拿不到该常量，属 test-support API 边界问题；若未来 kind 编码变化，测试会**显式失败**（找不到匹配实例）而非静默通过，可接受。建议 `test_support::diagnostics::InstanceRecord` 暴露 `primitive_kind` 语义化字段（P4）。
- **obs3（守卫强度，可选加强）**：`open_median/close_median ≤ idle_p95+2ms` 守卫符合 §6 P3-5 目标，但中位数允许"半数帧"较高。当前实测 p95（含冷启动首帧）1.118ms 也远低于 2ms，可再加一条"剔除首帧后的 p95 ≤ idle_p95+2ms"作为次级守卫（同时避免冷启动毛刺导致 CI 抖动）。非必需。
- **obs4（错误路径统计污染）**：`note_popup_frame_work` 在 `needs_frame` 判定前写 `window.stats`；若该帧随后在 `begin_frame`/样式/文本/布局/绘制路径 abort（不产出 record、不 `mem::take`），flags 会残留到下一次 idle record。仅在错误路径出现且最多影响一帧诊断值，非产品缺陷，登记 P4。
- **obs5（环境）**：复核时本机有两个 acceptance 侧 `gallery.exe` 进程（PID 67928/73228，17:48 起，窗口标题 `Kytrix · ui_kit kit gallery (P2-6b)`，CPU 累计 ≈1.1s、之后不再增长）长期存活并占用 `E:\kytrix\target\debug\examples\gallery.exe`，导致 `cargo run -p ui_kit --example gallery` 无法 relink（LNK1104）。我改用**当前构建的副本**（exe mtime 17:47:42 > 所有源码 mtime 17:35:31）跑 `--pixel-check`。建议 lead 在最终提交前清理这两个僵持进程，再跑一次三条 gallery 命令留档。

---

## 4. 门禁与性能数据复核（当前快照）

| 门禁 | 结果 |
|---|---|
| `cargo test -p ui_core -p ui_kit --lib --tests` | **3872 passed / 0 failed / 2 ignored** = ui_core lib 2806(+1 ignored) + frame_record 8 + gui_smoke 0(+1) + layout 4 + **popup_behavior 123** + snapshot 14 + demo 39 + **ui_kit 878**（与 `p3-summary.md` §6 逐项一致） |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | PASS（exit 0） |
| `cargo +nightly fmt --all -- --check` | PASS（exit 0） |
| `cargo check --workspace --all-targets` | PASS（exit 0） |
| `gallery --pixel-check` | **PASS 94 checks**（32 sampled / open 8 / close 6+6 / `damage_full_frames=0` / first_visible == stable），exit 0（用当前构建副本执行，见 obs5） |
| `gallery --debug-stack` | 未重跑（acceptance 侧僵持进程占用真实窗口与 exe；该子命令不涉及修复轮改动路径，`p3-summary.md` §6 记录当前构建 PASS） |
| `gallery --real-pixel-check` | 未由我重跑（P3-6 归 acceptance lens）；`p3-summary.md` 记录 Windows **55 checks PASS**（修复后）、macOS/Ubuntu 为**修复前 50 checks**、待复跑（P3A-02/05 已登记） |
| 依赖/注释/allow | `git diff f24a3441 -- Cargo.toml crates/*/Cargo.toml` 空；新增行无 `//`、`#[allow]`、`#[expect]`；无平台 cfg 分支改动 |

**性能（`--nocapture` 单次实测，本机）**：

```
popup frame budget: idle_p95=700ns open_median=31.6µs open_p95=1.118ms close_median=24.6µs close_p95=168.5µs
open_samples =[1.118ms, 134.1, 72.9, 31.6, 27.7, 36.9, 28.3, 26.0, 26.2, 26.2]µs
close_samples=[33.8, 25.3, 24.6, 25.8, 23.6, 24.3, 23.8, 23.9, 24.0, 168.5]µs
idle_samples =[1.0, 0.7, 0.6, 0.6, 0.6, 0.7, 0.7, 0.6, 0.6, 0.6, 0.6, 0.6]µs
```

- 守卫：`open_median(31.6µs)` 与 `close_median(24.6µs)` 均 ≤ `idle_p95 + 2ms`（≈2.0007ms）⇒ 设计目标（§6 P3-5）**由测试真实守卫**，余量约 60×；p95 只记录（open p95 1.118ms = 冷启动首帧；close p95 168.5µs = 关闭完成/移除帧的尾部）。
- 样本序列已恢复时序（首帧最慢），不再被排序误导；`p3-summary` 的 "首帧冷启动 p95≈1.0–1.13ms / 其余帧中位≈29µs" 与本次实测一致。

---

## 5. r2 计数与结论

| 严重度 | r1 | r2（当前） | 说明 |
|---|---|---|---|
| P0 | 0 | **0** | — |
| P1 | 0 | **0** | — |
| P2 | 2 | **0** | P3E-01/P3E-02 均修复并有必红测试 |
| P3 | 7 | **2** | 新增 P3R2-01（p3-summary D-P3-4 旧理由）、P3R2-02（`Primitive::Text` 死分支 / P3K-06 可结案）；r1 的 P3E-03…09 全部关闭 |
| 观察 | — | 5（obs1…5） | 均已登记或不影响产品行为 |

- **最关键 3 条（r2 视角）**
  1. **P3E-01（已关）** shadow offset 向量化 + GPU 实例级断言（含旧公式 >2px 回归守卫），这是 r1 唯一"用户可见渲染错误"，现在有双层判别力测试。
  2. **P3E-02（已关）** `is_fade_only()` 统一 modal/Modal/Scrim；scrim 开/关两阶段 `paint_scale==1.0` 断言，`§14.2/§14.6-D-P3-2` 的声明现在成立。
  3. **P3E-03/P3E-04（已关）** `frame_cpu` 覆盖整帧、idle 统计真实回填、中位守卫=设计目标、样本不再被排序；配合 `mem::take` 语义，"空闲帧 0 popup 工作"现在是被测量的事实而非默认值。
- **是否可提交 P3（引擎 lens）：可以提交。** P0/P1/P2 = 0；两条 r2 P3 均为"文档一句话 + 死分支清理"，可在提交时或提交后单独处理（P3R2-02 若删除分支需重跑 ui_core 测试，但不涉及行为）。
- **提交前建议 lead 处理**（非引擎缺陷）：① 清理 obs5 的两个僵持 `gallery` 进程并重跑 `--pixel-check`/`--debug-stack`（真实窗口 `--debug-stack` 我未重跑）；② 接受 acceptance lens 对"macOS/Ubuntu 真实像素证据为修复前"的判定（`p3-summary.md` 已登记 P3A-02/05）；③ 顺手改 P3R2-01 的一句话。
