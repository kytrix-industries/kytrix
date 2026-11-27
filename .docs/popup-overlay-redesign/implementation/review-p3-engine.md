# P3 对抗性审查（引擎 lens）— `ui_core` FadeScale / lane transform / damage / 性能

- 审查员：子 agent `p3-review-engine`（P3 对抗性审查，引擎视角）
- 审查对象：worktree `E:\kytrix\.agents\worktrees\dsh-1d175414-1a41`，HEAD `f24a3441`（P2 提交）+ **未提交 P3 工作树**
  - 声明范围：`git diff f24a3441 -- crates/ui_core` = **14 文件 +861/−53**
  - 实际核对范围（P3 全量 22 tracked 文件 +3393/−125）：`crates/ui_core`（引擎）+ `crates/ui_kit`（`ItemPanel`/`PopupPanel`/Dialog/ColorPicker/Dropdown/Select/ContextMenu/Menu 的动画与 close reason）+ gallery 的 `--real-pixel-check`（P3-6 归 acceptance，本身只做编译/入口核对）
- 必读对照：设计 §4.12、D10、§6 P3-2/P3-5、§7.1、§14（已回写）；`AGENTS.md`；`review-p1-final-{engine,kit,quality}*`、`p2-summary`、`review-p2-*`
- 结论摘要：**P0 = 0，P1 = 0，P2 = 2，P3 = 7**。FadeScale 的骨架（pivot/DPR/damage/命中不变/modals fade-only）实现正确且有测试；但 **primitive 变换有 1 处真实渲染错误（Shadow.offset 当点而非向量）**，**modal scrim 在关闭时被误施加 0.98 缩放**，两者均为本轮新增的可见缺陷。

---

## 1. 范围与方法

### 1.1 只读手段

| 手段 | 命令/落点 | 结果 |
|---|---|---|
| 引擎/kit 单测+集成 | `cargo test -p ui_core -p ui_kit --lib --tests`（`CARGO_TARGET_DIR=E:\kytrix\target`） | **3865 passed / 0 failed / 2 ignored**，与设计 §14.5 逐二进制计数一致（2805+1、8、0+1、4、120、14、39、875） |
| popup 行为集 | `cargo test -p ui_core --features test-support --test popup_behavior` | **120 passed** |
| 性能用例（含 stdout） | `cargo test ... -- --nocapture popup_open_and_close_frames_stay_within_the_frame_budget`（单线程/并发各 3 次） | 见 §4 |
| headless 像素门禁 | `cargo run -p ui_kit --example gallery -- --pixel-check` | **PASS 94 checks / sampled 32 / open 8 / close 6+6 / damage_full_frames=0**，exit 0 |
| 真实窗口栈门禁 | `cargo run -p ui_kit --example gallery -- --debug-stack` | **PASS**（picker 栈 1 条、3 级链、rect/Z/parent），exit 0 |
| clippy / fmt / workspace | `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings`、`cargo +nightly fmt --all -- --check`、`cargo check --workspace --all-targets` | 均 **PASS**（exit 0） |
| 定向复现（不改仓库） | 在 `%TEMP%\p3probe` 建独立 crate（`ui_core = { path = ".../crates/ui_core", features=["test-support"] }`，共享 target dir） | 用 `frame_dump().instances` / `popup_stack` 直接取证 P3E-01、P3E-02（证据见 §2） |

> 环境注记：首次执行 `cargo test -p ui_core -p ui_kit` 时 `examples/gallery.exe` 链接失败（`LNK1104 cannot open file`），原因是 acceptance 侧同时在本机运行 gallery/cargo（同一 `CARGO_TARGET_DIR`）；改用 `--lib --tests` + 单独跑 gallery 后全部通过。**非代码问题**。
> 未执行：`gallery --real-pixel-check`（真实窗口截图，P3-6 由 acceptance 负责，本机截屏会干扰桌面）。

### 1.2 审查重点覆盖

FadeScale 数据流（`start_open`→`tick`→`settle`→`paint`→`damage`→移除）、`PopupScale` 对 9 类 primitive 的变换、pivot 空间（window/lane/canvas-local）、DPR 1x/2x、shadow/clip/image/glyph/geometry/canvas 递归、`paint_scale` 缓存键、scale clamp≤1 的 damage 超集性、hit/a11y/layout/settle 不变性、modal/scrim fade-only、关闭 reason 链路、性能计时口径与统计方法、早审遗留（R2-01/R2-02/B-16/A-31/A-32/K1-11/D-P1-7）、代码质量（注释/allow/依赖/死代码/平台 cfg）。

---

## 2. Findings

> 严重度口径沿用前几轮：P0 崩溃/数据丢失/阻断；P1 正确性阻断；P2 应在提交前修；P3 可登记（文档/测试/卫生）。

### P3E-01 [P2] `PopupScale` 把 `Shadow.offset` 当作**点**变换（应为向量 ×factor），FadeScale 期间节点阴影按 `(1−factor)×pivot` 整体漂移

- 位置：`crates/ui_core/src/paint/pass.rs:310-316`
  ```rust
  Primitive::Shadow(mut shadow) => {
      shadow.rect = self.rect(shadow.rect);
      shadow.offset = self.point(shadow.offset);   // ← 向量被当作绝对点做 homothety
      shadow.blur = self.length(shadow.blur);
      ...
  ```
- 语义依据（不可辩驳）：`Shadow.offset` 是**平移向量**——发出侧 `emit_decorations` 用 `ShadowSpec.offset`（主题菜单阴影 = `(0,1)` blur 2 / `(0,3)` blur 6，`ui_kit/src/kit/popup_panel.rs:52-65`），消费侧 encoder 为 `shadow.rect.translate(shadow.offset)`（`paint/instance.rs:1926`）。正确缩放是 `f·offset`（`self.length`），而不是 `pivot + f·(offset − pivot)`。
- 误差量级：`offset' − f·offset = (1−f)·pivot`。ColorPicker 面板中心（gallery 场景）≈ `(646, 278.5)`，`f=0.98` ⇒ 阴影偏移 **≈ (12.9, 5.6) px**（正确值应为 `(0, 0.98)` / `(0, 2.94)`），随动画收敛回 0 ⇒ 整个 120ms 开/90ms 关期间阴影相对面板"向右下扫过再归位"。
- 证据（定向复现，`%TEMP%\p3probe`，DPR 1.0，popup 居中 `(556,424,240,160)` → pivot `(676,504)`，采样帧 `f=0.9915625`）：
  ```
  expected correct shadow origin (vector*scale) = (557.0125, 425.6665)
  expected buggy   shadow origin (point-scale)  = (562.7162, 429.9190)
  raw instance bounds = [562.71625, 429.91907, 237.97, 158.65]   ← 阴影实例落在 buggy 位置
  raw instance bounds = [557.0,     425.0,     238.0,  158.0]     ← 面板 quad 在正确位置
  ```
- 影响面：所有**节点级** lane 阴影（PopupPanel surface = 菜单/对话框/取色器的主阴影路径）；本例 ColorPicker（P3-3 明确改成 FadeScale）每次开/关都可见。**Canvas 内部**的 Shadow 反而正确（`scale_canvas` 的局部 scale `pivot = ZERO`，`point()` 退化为 `f·offset`），所以这是"同一 primitive 两种行为"的不一致。
- 现有测试为何全绿：`fade_scale_*` 与 kit 的 `the_panel_fades_and_scales_in_without_moving_the_settled_rect` 只断言 **quad/rect**；`--pixel-check` 只采样存在性/残影；`--real-pixel-check` 的"打开可见/关闭恢复"对阴影位移不敏感。
- 修法（2 行 + 1 条测试）：
  ```rust
  shadow.offset = LogicalPoint::new(self.length(shadow.offset.x), self.length(shadow.offset.y));
  ```
  并在 `popup_behavior` 增加"FadeScale 帧内 Shadow 实例 bounds = 缩放后 rect + f·offset"用例（用 `frame_dump().instances`，参照 `fade_scale_paints_the_same_logical_geometry_at_dpr_one_and_two` 的写法）。
- 阻塞否：**建议提交前修**（P2；不是崩溃级，但这是本轮新增功能可见缺陷，且修复成本极低）。

### P3E-02 [P2] modal **scrim** 在关闭动画中被施加 0.98 缩放（`begin_close_inner` 把 scrim 当非模态），违反 §14.2「modal fade-only」与 D-P3-2「modal/scrim 只淡入淡出」

- 位置：`crates/ui_core/src/popup/dismiss.rs:148-226`
  - `scrims` 被并入 `candidates`（`:148-164`），关闭时给 scrim 也 `push_animation`（`:215-225`）；
  - `let modal = entry.is_modal() || entry.tier == PopupTier::Modal;`（`:206`）对 **Scrim** 条目恒为 `false`（scrim entry：`tier=Scrim`、`focus=None`），于是 `:210-214`
    ```rust
    let scale_to = if animation == FadeScale && !modal { POPUP_SCALE_FROM } else { scale_from };
    ```
    把 scrim 的 `scale_to` 设为 0.98。
- 证据（定向复现，`tier=Modal + FocusPolicy::Trap + animation=FadeScale + ScrimSpec`）：
  ```
  settled entries               = [("Scrim/Open", 1.0, false), ("Modal/Open", 1.0, false)]
  close mid entries (45ms)      = [("Scrim/DismissPending", 0.9825, false), ("Modal/DismissPending", 1.0, false)]
  ```
  ⇒ 面板正确 fade-only，**全窗 scrim 却在 90ms 内缩到 98%**。
- 影响：Dialog/Alert 每次关闭时，scrim 绕**视口中心**缩到 98%（1200×800 时四周约 12/8 px），该环带不再被遮罩变暗；同时 scrim 仍在淡出 ⇒ 窗口边缘出现一圈"未被压暗"的亮边并随动画扩大。`damage_full`/residue 不受影响（modal close 走 `force_full_paint`，`app/popup.rs:593`），是纯视觉回归。P3 之前 Dialog 用 `Fade`（scrim 的 `scale_to == scale_from == 1.0`）没有此现象 ⇒ **本轮引入的回归**。
- 现有测试为何全绿：`modal_fade_scale_is_fade_only` / `the_modal_dialog_degrades_fade_scale_to_fade_only` 只断言 **Modal 条目** 的 `paint_scale`，从不看 Scrim 条目。
- 修法（推荐方案 A，1 行 + 1 断言）：
  - A：`let modal = entry.is_modal() || matches!(entry.tier, PopupTier::Modal | PopupTier::Scrim);`（scrim 永远 fade-only，与 §4.12「大面积缩放会触发 damage 阈值」一致）；
  - B（更彻底）：scrim entry 的 `animation` 固定为 `Fade`（`create_scrim_node`/`insert_entry` 处归一化），因为 scrim 目前**开侧根本没有 `start_open`**（打开时瞬时出现、无动画），给它"关闭有 scale"本来就不对称。
  - 测试：在 `modal_fade_scale_is_fade_only` 里补 `scrim entry paint_scale == 1.0`（关闭中途）。
- 阻塞否：**建议提交前修**（P2；纯视觉、90ms、无残影，但属本轮新回归且修复 1 行）。同时 §14.2/§14.6-D-P3-2 的"modal/scrim 只淡入淡出"当前是**不成立**的陈述，见 P3E-07。

### P3E-03 [P3] 性能门禁与设计目标不一致 + 统计/计时口径三处削弱可信度

1. **守卫阈值比设计目标宽 8 倍**：`crates/ui_core/tests/popup_behavior.rs:5842` `let budget = idle_p95 + Duration::from_micros(16_700);`，而 §6 P3-5 目标是 **`idle p95 + 2ms`**。D-P3-8 已登记"避免 CI 抖动"，但 §14.2 P3-5 又写"设计目标…满足"，实际是**实测算出来的、测试并不守卫**。即：open 帧从 24µs 退化到 15ms 该测试依然绿。
2. **`percentile()` 原地 sort**（`:5496-5500`）导致诊断 `println!` 输出的 `open_frames`/`close_frames` 是**排序后**序列，看起来像"帧越跑越慢"（实测 `[23µs,…,110µs,983µs]`），掩盖了真实分布：**首帧冷启动 ≈1.0ms，其余 7 帧 23–110µs（中位 ≈24µs）**。§14.2 把 `open p95≈1.0ms` 当代表值写进验收，容易被读成"动画帧很贵"。
3. **计时区间不对等**：`frame_cpu = now.elapsed()`（`app/frame.rs:285`），`now` 在 `popup::animation::tick`（`:64`）**之后**取（`:74`）⇒ open/close 帧**不含** popup tick（per-popup transform 写入、damage 计算、`mark_paint`、`finalize_close`）与 frame-start 固定开销；而空闲帧 `note_idle_cpu(app, window, frame_started.elapsed())`（`:47/53/70/83`，`frame_started` 在 `:30`）**包含**这些。两个 p95 相减/相加的物理含义不对等，且恰好把被测量对象的自身开销从分子里剔除了。
4. 附带：`note_idle_cpu` 对 `a11y-only` 早退（`frame.rs:99-119`）与 `begin_frame` 失败（`:90-92`）**不记录**，这两种 Idle 帧读到上一帧的 `frame_cpu`/`popup_*` 统计（stale），没有测试区分。

- 证据（同机重复 3 次，`--test-threads=1` 与并发结果一致）：
  ```
  idle_p95=900ns open_p95=996.9µs close_p95=62.7µs open_frames=[23.1,23.4,23.9,24.7,26.8,41.5,110.1,996.9]µs
  idle_p95=1.9µs open_p95=1.3762ms close_p95=96.5µs  （并发构建期间）
  ```
  定向 probe（同一 TestApp 序列逐帧打印）：`frame0=1.09ms, frame1=139.8µs, frame2..7=48.9/31.7/57.6/32.6/31/27.2µs`，`frame8(done)=21µs` ⇒ 1ms 只出现在**首帧**（首帧 layout+settle+paint+encoder 初始化）。
- 修法：① 门禁改 `idle_p95 + 2ms`，但**先剔除首帧**（或单独断言首帧 ≤ 60Hz 预算），否则 CI 抖动；② `percentile(&mut Vec)` 不要原地排序（`let mut sorted = samples.to_vec()`），或打印未排序副本；③ `frame_cpu` 改为从 `frame_started` 起算，或明确文档化为"layout→paint 区间"，并让 idle 也用同一区间；④ 给 `a11y-only`/`begin_frame` 失败路径补 `note_idle_cpu`。
- 阻塞否：不阻塞（P3，登记即可），但 §14.5/P3-5 的"设计目标满足"应改成"实测满足、测试守卫 16.7ms"并注明首帧冷启动。

### P3E-04 [P3] `idle_frames_do_no_popup_settle_or_damage_work` 的 popup 断言结构性恒真，不能证明"空闲帧 0 popup 工作"

- 位置：`crates/ui_core/tests/popup_behavior.rs:5769-5796`；`app/frame.rs:459-469`。
- 机制：空闲路径 `note_idle_cpu` 写入 `FrameStats { frame_cpu: Some(cpu), ..FrameStats::default() }` ⇒ `popup_settle_rounds=0 / popup_unsettled=false / popup_opening=false / popup_closing=false` 是**构造出来的**，与该帧 `popup::animation::tick` 是否真的做了工作无关（tick 在 `needs_frame` 判定**之前**执行，其 `PopupAnimationTick` 报告根本没有传给 `note_idle_cpu`）。`record.damage.is_empty()` 对 `FrameRecord::idle` 也是恒真（`test_support/record.rs:141-159`）。因此四条断言里只有 `!record.rendered` 有判别力。
- 修法：`note_idle_cpu` 接收 `PopupAnimationTick`（及 settle 结果）写回真实值；测试才有判别力。若不改，测试名应改为"稳定后无渲染帧"，并把 popup 断言登记为未验证。
- 阻塞否：不阻塞（P3）。

### P3E-05 [P3] 节点移除未清理新增侧表 `paint_scales`

- 位置：`crates/ui_core/src/node/tree.rs:416`（新增字段 `paint_scales: Vec<f32>`），`node/tree/content.rs:813-826`（读写）；`node/tree/arena.rs:579-616`（子树移除）逐项清理 `clip_keys/baked_screen_bounds/painted_extents/paint_marks/scroll_slot_of/popup_transforms/popup_pointer_transparent/a11y_slots`，**唯独没有 `paint_scales`**。
- 影响：槽位复用后 `paint_scale_of` 读到上一个节点的残留值。当前只造成 `paint_state` 的 reuse 判据**保守失配**（多一次 lane 重建），不会漏画/错画（新节点首帧 `REUSABLE` 未置位；方向只会"该复用而未复用"）。但破坏了"移除必须清侧表"的既有不变量（已有测试 `remove_clears_the_extent_and_baked_screen_bounds_side_tables` 覆盖其它表），且一旦将来有人把 `paint_scale` 从 reuse 判据挪作他用就会变成真 bug。
- 修法：在 `arena.rs` 移除路径补 `if let Some(slot) = tree.paint_scales.get_mut(index) { *slot = 1.0; }`，并扩充那条侧表测试。
- 阻塞否：不阻塞（P3）。

### P3E-06 [P3] `PopupScale::compose` 当前不可达，且对嵌套 homothety 的合成公式是错的；§14.3 却声称"外层×内层、pivot 也经外层变换"

- 位置：`crates/ui_core/src/paint/pass.rs:243-252`。
- 不可达论证：`popup_transform` 只设在 popup entry 节点上，而所有 popup 节点都由 `create_content_node` 建在 **overlay root 之下（扁平兄弟）**（`app/popup.rs:129-141`），因此任何带 transform 的节点都不可能有带 transform 的祖先 ⇒ `inherited_scale` 恒 `None`，`compose` 的 `outer` 分支是死分支（`code is unreachable in practice`）。
- 数学错误（假设将来可达）：两个不同中心 homothety 的合成仍是 homothety，中心为 `C = [P_o(1−f_o) + f_o P_i(1−f_i)] / (1 − f_o f_i)`；代码取 `C' = P_o(1−f_o) + f_o P_i`，仅当 `f_o = 1` 或 `P_o = P_i` 才相等。`f_o=f_i=0.98` 时 `C ≈ 0.505P_o + 0.495P_i` 而 `C' = 0.02P_o + 0.98P_i`，偏差点位移 ≈ `0.019·|P_i−P_o|`（300px → ≈5.8px）。
- 修法（按 `AGENTS.md` §1.2/§6 删除优于保留）：直接去掉 `compose` 与 `inherited_scale` 链（scale 只可能来自当前 popup 节点），或保留单层并用 debug 断言禁止嵌套；同时修正 §14.3 措辞。
- 阻塞否：不阻塞（P3）。

### P3E-07 [P3] 文档与实现不一致（3 处实质 + 1 处由 P3E-02 推翻）

| # | 文档陈述 | 实际 | 建议 |
|---|---|---|---|
| a | §14.6 **D-P3-5**："`FadeSlide` 只启用 Menu/ContextMenu；Dropdown/Select 仍为 `Fade`" + §14.7 把 "Dropdown/Select FadeSlide" 列为 P4 候选 | **Dropdown/Select 也已 FadeSlide**：三者共用 `ItemPanel::open`，`spec.animation = PopupAnimation::FadeSlide` 在 `ui_kit/src/kit/menu.rs:462-468` 的公共路径里；`dropdown.rs:501`、`select.rs:761` 都调 `self.inner.panel.open(...)` | 删掉 D-P3-5 / 改写为"ItemPanel 共用 ⇒ Menu/ContextMenu/Dropdown/Select 全部 FadeSlide（关闭仍 Fade-only、无 slide）"；P4 行删除 |
| b | §14.2 **P3-5**："open p95≈1.0 ms" | 1.0ms 是 **TestApp 首帧冷启动**（layout+settle+encoder 初始化）；稳态动画帧中位 ≈24µs | 改为"首帧 ≈1.0ms / 稳态帧 23–110µs（中位 ≈24µs）"，与 §14.6 D-P3-8 合并说明 |
| c | §14.6 **D-P3-4**："pivot 变化不影响 layout/paint 内容位置（scale 围绕新中心），若 mark_paint 会在动画结束触发多余重绘/damage_full" | pivot 变化**当然**改变 scale≠1 时的绘制位置；不 mark_paint 之所以安全，是因为 `animation::tick` 在动画帧每帧都会 `mark_paint`（`popup/animation.rs:184-187`），且 settle 在同一帧 paint 之前执行；"动画结束会多余重绘"也不成立（done 帧本来就 `set_popup_transform(None)+mark_paint`） | 改写 rationale：`settle` 的 pivot 更新依赖"同帧 tick 已 mark_paint"这一时序前提；并补 1 条"两帧同时间点 + settle 移动 popup"的用例（见 P3E-09） |
| d | §14.2 P3-2 / §14.6 **D-P3-2**："modal fade-only"/"modal/scrim 只淡入淡出，不缩放" | scrim 关闭时被施加 0.98（P3E-02，已实测 `paint_scale=0.9825`） | 修代码后此陈述自动成立；修前不得保留该句子 |

- 阻塞否：不阻塞（P3，文档），但 D-P3-5 属于"把没做的偏差当成做了、把做了的当成没做"，会误导 P4 计划，建议提交前一并改。

### P3E-08 [P3] kit `close_with_reason` 先写 reason 再 `begin_close_popup`：与引擎 dismiss handler 重复，且失败时留下 stale reason

- 位置：`crates/ui_kit/src/kit/popup_panel.rs:461-474`。
- 事实：`begin_close_popup → request_close → dismiss::dispatch` 会同步调用 kit 安装的 dismiss handler，而该 handler 内部已经把 `inner.reason.set(Some(dismiss.reason))`（`popup_panel.rs:428`）⇒ `:468` 的预写对"正常关闭"完全冗余。
- 风险：`begin_close_popup` 返回 `Ok(false)`（不可 dismiss / handler `Ignore` / `is_closing` 之外的早退）时也不会回滚 reason；若随后面板由**非 dispatch** 路径关闭（`close_popup_now`、`reap_orphan_scrims`、强制移除），`finish_close` 的 `reason.take().unwrap_or(Programmatic)`（`:579-582`）会把上一次遗留的 reason 上报给 `on_close`。
- 修法：删除 `:468` 的预写（依赖 handler 回填），或仅在 `closing == true` 后 `set`。
- 阻塞否：不阻塞（P3）。

### P3E-09 [P3] `settle` 的 pivot 同步不 `mark_paint`：正确性依赖"同帧 tick 已 mark_paint"的隐式时序

- 位置：`crates/ui_core/src/popup/settle.rs:352-367`。
- 分析：`paint_state` 的 reuse 判据只比较 `paint_scale`(=factor)，**不含 pivot**（`node/tree/content.rs:772-777`）。若某帧 `scale != 1.0`、`settle` 更新了 pivot，但该帧 `tick` 没有产生 transform 变化（例：两帧之间时钟未推进 ⇒ `progress` 相同 ⇒ `previous == next` 不 mark），则 popup 子树 lane 会被复用、**按旧 pivot 绘制一帧**。当前所有真实路径都有 layout-move 触发的 paint 标记兜底，故未观察到现象；但这是"不 mark_paint"决定的唯一真实代价，文档 D-P3-4 没有写出来（反而写了错误理由）。
- 修法：`settle` 更新 pivot 时只用 `mark_paint` 的幂等代价换来显式正确性；或在 `paint_state` 的缓存键里加入 pivot（`(factor, pivot)`）。二选一即可，并补一条"无时钟推进的连续两帧 + settle 移动 popup"用例。
- 阻塞否：不阻塞（P3）。

---

## 3. 设计 D10 / §4.12 / P3-5 逐项核对

| 设计要求 | 实现 | 判定 |
|---|---|---|
| D10：0.98→1 lane transform、120ms | `POPUP_SCALE_FROM=0.98`、`POPUP_OPEN_DURATION=120ms`、`EaseOutCubic`、`interpolate(scale_from,scale_to,eased)`（`popup/animation.rs:148`） | ✅ 数值/曲线一致 |
| 以 popup 中心为 pivot | `PopupEntry::paint_center()`（paint rect 中心，空则 hit rect）；`start_open`/`tick`/`block_entry` 三处取同一来源 | ✅；但 pivot 取自 `paint_rect`，而 `sync_rects` 的 `paint_rect = painted_extent ∪ bounds`（`popup/stack.rs:390-397`）⇒ 带阴影的面板 pivot 比面板几何中心略偏下（menu 阴影 pad 约 2–4px）。量级可忽略，登记为残余 |
| 只作用于渲染层，不改布局/定位/命中 | scale/pivot 只在 `paint/pass.rs` 消费；hit test、`node_screen_rect`（只加 paint_offset）、a11y projection、settle 求解均不读 scale | ✅ 与 §4.12「位置在首帧即正确」一致；引擎+kit 测试三重断言 |
| 关闭 90ms fade + 反向 1→0.98 | `begin_close_inner` 取当前 transform 的 `scale_from`，`scale_to=0.98`（非 modal），`POPUP_CLOSE_DURATION=90ms` | ✅（非 modal）；❌ scrim 泄漏（P3E-02） |
| 对话框/Alert：fade-only | `is_modal() || tier==Modal` ⇒ scale 1.0；Dialog 用 `tier=Modal` + `Trap` 双保险 | ✅（面板）；❌ scrim（P3E-02） |
| damage 覆盖"带偏移 ∪ 最终位置"、scale≤1 向内 | tick 每帧 damage `rect.translate(prev.offset)` + `rect.translate(offset)`（layout rect）；scale 向内收缩 ⇒ layout rect 是 paint rect 超集；lane 重建对 `previous_extent` 再补 damage；移除帧 damage `bounds ∪ painted_extent` | ✅ 正确且已核对无 under-damage 路径（含 shadow pad / clip / geometry chunk） |
| 模态 damage = 全视口 | modal open/close 走 `force_full_paint`（`app/popup.rs:417,593`） | ✅ 设计允许 |
| Canvas 递归局部坐标（P0 契约） | `scale_canvas` 对内容用 `pivot=ZERO` 的局部 scale、对 `CanvasDraw.offset` 用 `point()` ⇒ 世界坐标恰为 `pivot+f(x−pivot)`；嵌套 canvas 递归同构 | ✅ 已推导 + probe 实测（canvas shadow/quad 均落在正确位置） |
| Geometry transform | `draw.transform.then(self.transform())`，`Transform::then` = "先 self 后 next"（`math.rs:1092-1105`）⇒ 先几何自有变换、后 popup 缩放；`Path` 转 `GeometryDraw{transform, FULL_INDEX_RANGE}` 正确 | ✅ 语义正确（无专项测试，见残余） |
| Shadow / Clip / Image / GlyphRun / Quad / Text | Quad(rect/radius/border/gradient) ✅；Clip(rect/radius)+Push/Pop 平衡 ✅；Image(bounds) ✅；GlyphRun(position/size) ✅；Shadow ❌（offset 见 P3E-01）；Text（canvas 文本在 push 前已 resolve 成 GlyphRun，该分支基本不可达） ✅ | ⚠️ 1 处错误 |
| `paint_scale` 缓存键 | `paint_state` 的 reuse = REUSABLE ∧ clip_key ∧ lane_offset ∧ opacity ∧ **paint_scale(factor)** ∧ screen_bounds；每帧动画 factor 变化 ⇒ 全子树重建 | ✅ 功能正确；键不含 pivot（P3E-09）；侧表移除不清（P3E-05） |
| clamp ≤1 | `PopupTransform::paint_scale()` 对 NaN 回 1.0、否则 `clamp(0,1)` | ✅ NaN 安全；clamp 保证 damage 超集前提 |
| scale 快照可观测 | `PopupSnapshotEntry.paint_scale`（`app/popup.rs:753-758`） | ✅ 测试/门禁都用它 |
| §7.1-9（动画：slide 用 paint_offset、settle rect 不变、damage 覆盖、hit 跟随） | 既有 FadeSlide 路径未被破坏（`slide_animation_*`、`three_menu_levels_follow_the_parent_without_a_frame_lag` 全绿）；Menu 启用 FadeSlide 后新增 `the_menu_slides_in_without_moving_the_settled_rect` | ✅ |
| P3-5：非模态 open/close 帧 CPU ≤ 16.7ms、空闲帧 0 popup 工作、damage_full=false | 实测 open/close p95 @debug ≈1.0ms/63µs；`damage_full=false` 有测试与 pixel-check 双守卫 | ✅ 数值；⚠️ 门禁口径与冷启动解读（P3E-03）、空闲帧断言恒真（P3E-04） |

---

## 4. 性能数据复核

| 指标 | 文档 §14.2/§14.5 | 本机复测（3 次，`--test-threads=1` 与并发一致） | 备注 |
|---|---|---|---|
| idle p95 | ≈0.9–1 µs | **900 ns / 1.9 µs** | 一致（含 tick/资源轮询等整段 idle 路径） |
| open p95 | ≈1.0 ms | **996.9 µs / 1.0025 ms / 997.2 µs**（并发下 1.3762 ms） | 数值一致；p95 = 8 个样本的最大值 = **首帧冷启动**；其余帧 23–110µs（中位 ≈24µs） |
| close p95 | 63–65 µs | 62.7 / 64.3 / 67.5 µs（并发 103.8） | 一致；最后一个 close 帧是**完成帧**（节点移除+damage），故最大 |
| damage_full | false | `damage_full_frames=0`（pixel-check 32 帧采样）+ 各测试 `!damage_full` | 一致 |
| 测试守卫 | `idle p95 + 16.7ms` | 同（`:5842`） | 与设计目标 `+2ms` 不一致（P3E-03） |
| 采样方法 | 8 open / 8 idle / ≤10 close 帧 | 同 | p95 退化为 max（样本数 8）⇒ 对单次毛刺敏感；`percentile` 原地排序（P3E-03） |

结论：**"设计目标 ≤ idle p95 + 2ms"在数值上成立（最坏样本 ≈1.0ms，稳态 ≈24µs）**，但该结论来自实测而非测试守卫；测试守卫只保证"不超过 60Hz 单帧预算"。`frame_cpu` 的物理区间是 `budget-admit → paint_frame`（不含 `popup::animation::tick`、不含 `commit_frame`），idle 帧却是 `frame_started → 早退`，两者不可直接对比。

---

## 5. 已核实正确点（无 finding）

1. **hit / a11y / layout / settle 不变**：`fade_scale_open_paints_the_panel_scaled_around_its_center`、`fade_scale_close_reverses_the_scale_and_keeps_damage_local`、`the_panel_fades_and_scales_in_without_moving_the_settled_rect`、`the_modal_dialog_degrades_fade_scale_to_fade_only` 断言 `hit_rect == a11y_rect == 首帧 rect`、面板布局盒不变；代码路径核对一致（hit/a11y/settle 均不读 `scale`）。
2. **pivot 空间正确**：lane primitive 空间 = 窗口空间 − `slot_offset`，`lane_scale.pivot = pivot − slot_offset`（`pass.rs:886-892`）自洽；popup 节点 `slot_offset` 恒 0，滚动内容不参与。
3. **Canvas 递归缩放正确**（含嵌套 canvas、clip、shadow）：局部 `pivot=ZERO` + `offset` 点变换的推导成立，probe 实测 canvas 内 shadow 实例 bounds 与 `pivot+f(x−pivot)` 精确一致。
4. **Geometry 变换顺序正确**：`then()` 语义核对 + `push_geometry_op(draw.transform.then(translate(offset)))` 一致；damage 用未缩放 chunk AABB（保守）。
5. **damage 契约无 under-damage**：scale≤1 向内 + layout rect 超集 + 每帧 `previous_extent` 补 damage + 移除帧 `bounds ∪ painted_extent`；关闭帧 `damage ⊇ 旧 rect` 有 3 条测试（引擎 2 + kit/menu、picker 各 1）。
6. **DPR 1/2 逻辑几何一致**：`fade_scale_paints_the_same_logical_geometry_at_dpr_one_and_two` 断言逻辑 quad 相同、物理实例按 DPR 落位（±1.5px）。
7. **modal 面板 fade-only**：引擎 `modal_fade_scale_is_fade_only` + kit Dialog 两条测试；`PopupTier::Modal` 与 `FocusPolicy::Trap` 双条件覆盖。
8. **关闭 reason 链路完整（R2-02 目标路径闭合）**：`begin_close_popup(id, reason) → request_close → dispatch(handler, reason) → kit handler 回填 reason → begin_close_inner → finish_close → on_close(reason)`；`AnchorLost` 已覆盖 Menu/ContextMenu/Dropdown/Select/ColorPicker（新增 5 条 reason 测试）；`begin_close_popup_forwards_an_explicit_reason_to_the_dismiss_handler` 覆盖引擎侧透传。
9. **空闲帧确实无渲染/settle**：稳定后 `frame_record().rendered == false`、`damage` 空；`popup::animation::tick` 在 `!animating()` 时 O(1) 早退（`animation.rs:110-112`）。
10. **无新增依赖、无注释/`#[allow]`/`#[expect]`、无平台 cfg 分支改动**：`git diff f24a3441 -- Cargo.toml crates/*/Cargo.toml` 为空；`paint/pass.rs`/`popup/*`/`node/tree/*` 的 P3 hunk 无注释与 allow；`app/frame.rs` 的 `#[cfg(feature="test-support")]` 沿用既有模式（非平台分支）。
11. **门禁**：`cargo test -p ui_core -p ui_kit` 3865/0/2；clippy `-D warnings` PASS；`fmt --check` PASS；`cargo check --workspace --all-targets` PASS；`gallery --pixel-check` 94 checks PASS；`gallery --debug-stack` PASS。
12. **早审遗留处置核对**：R2-02 **部分关闭**（锚点丢失路径已上报 AnchorLost，其余按 Programmatic 属预期，§14.7 已登记）；K1-11 **已关闭**（且范围大于文档，见 P3E-07a）；D-P1-7 **已关闭**（recenter 删除，resize 不再居中，测试改为 `resizing_the_window_keeps_the_anchor_opened_with`）；R2-01 / B-16 / A-31 / A-32 **未被本轮改动影响，也未关闭**（§14.7 转 P4，可接受）；`stack.animating()` 早已在 P2 的 settle 触发条件里（`git show f24a3441:.../settle.rs` 核对），**不是本轮新增**，故动画帧 settle 的既有成本未回归。

---

## 6. 残余风险 / 未覆盖

1. **Geometry/Image/GlyphRun 的 scale 无专项测试**（只有 Quad 与 Canvas 断言）；尤其 Geometry 的 chunk 复用 + `transform.then` 组合、Image 拉伸、glyph size 缩放后的清晰度。
2. **pivot 取自 `paint_rect`（含阴影 pad）**：带阴影面板的缩放中心比面板几何中心低 2–4px（menu 阴影 blur 6/spread −2）。视觉可忽略，但与"popup 中心"措辞不完全一致。
3. **`scale` 期间 hit rect 仍是稳定 rect**：绘制缩到 98% 时，稳定 rect 最外 ~1–2px 的"透明环"仍可命中/可点击（设计未要求 scale 参与 hit，登记为已知行为）。
4. **scrim 打开无动画**（P1 遗留）：Dialog 打开时背景瞬时变暗、面板淡入；关闭时 scrim 却有动画（并在 P3 被误加 scale）。属对称性缺口，建议与 P3E-02 一起决定口径（scrim 统一 fade 或统一 none）。
5. **`cancel_close` 不 `mark_paint`**（P1 遗留，本轮未改）：重开瞬间若 tick 首帧 `previous == next`，会有一帧沿用关闭时的 opacity 绘制。当前被"opacity 起点即 0"掩盖，无法观察到，但同属"transform 变化必须显式 mark"契约缺口（与 P3E-09 同族）。
6. **A-32（a11y 全树 DFS）未进入性能用例**：`popup_behavior` 的性能测试未 `enable_a11y_capture`，真实窗口（accesskit 投递）的 popup 帧成本未被测量。
7. **性能基线只在 TestApp/headless、debug profile**：无 release 基线、无真实窗口帧时间、无 macOS/Linux 本机 perf；`FrameStats` 三字段仅 `test-support` 编译存在。
8. **menu `ancestors_animating` 抑制 `Moved`**（`menu.rs:2883-2891`）：父层动画期间的真实锚点位移会被忽略一帧；已登记于 §14.1，但无测试覆盖"动画期间内部滚动/几何变化"。
9. **R2-01（受约束首帧内部滚动/宽度未 settle）仍未闭环**（§14.7 → P4）：与 P3 无交集，但 ColorPicker/Dialog 在窄窗下的首帧内容仍可能未收敛。

---

## 7. 结论

- **严重度计数：P0 = 0，P1 = 0，P2 = 2，P3 = 7**（合计 9 条；全部为本轮新增代码，无既有功能回归）。
- **最关键 3 条**
  1. **P3E-01（P2）** `paint/pass.rs:312` 把 `Shadow.offset` 当点变换 ⇒ FadeScale 期间节点阴影按 `(1−f)·pivot` 漂移（ColorPicker 场景 ≈13px），已用 `frame_dump()` 实测定位；正确写法 `f·offset`（2 行）。
  2. **P3E-02（P2）** `dismiss.rs:206-214` 把 Scrim 判为非模态 ⇒ Dialog 关闭 90ms 内全窗 scrim 缩到 98%（实测 `paint_scale=0.9825`），窗口边缘出现未压暗亮边，且直接推翻 §14.2/§14.6-D-P3-2 的"modal/scrim 只淡入淡出"。
  3. **P3E-03（P3）** 性能验收口径：测试守卫是 `idle p95 + 16.7ms` 而非设计目标 `+2ms`；`percentile` 原地排序掩盖"1.0ms 只是首帧冷启动、稳态 24µs"；`frame_cpu` 与 idle 帧计时区间不对等。
- **是否可提交 P3**：**建议先修 P3E-01、P3E-02 再提交**（合计约 30–40 分钟：各 2 行/1 行 + 2 条回归用例），修完后 P0/P1/P2 = 0，P3 的 7 条可按 §14 登记进入 P4（其中 P3E-07 的文档 3 处实质偏差建议与 P3 提交同一轮一并改，避免"登记了没做的、漏记做了的"）。若不修直接提交，则本轮 P3 新增的 `FadeScale` 在 ColorPicker/Dialog 上带有两个可见渲染缺陷，不适合以"P3-2/P3-3 done"口径收口。
