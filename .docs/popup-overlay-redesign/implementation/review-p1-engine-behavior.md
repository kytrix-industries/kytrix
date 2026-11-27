# P1 引擎 popup 行为对抗性审查（review-p1-engine-behavior）

- 审查对象：worktree `dsh-1d175414-1a41`（基线 `d9949aca`，P1 未提交工作树，`crates/ui_core` 36 改 + 新增 `popup.rs`/`popup/`/`app/popup.rs`/`a11y/popup*.rs`/`tests/popup_behavior.rs`）
- 审查范围：**仅 `ui_core` 引擎行为正确性**（settle / dismiss / focus / animation / damage / a11y 投影 / 新代码质量 / 测试真实性）；kit 迁移与旧 API 删除不在本报告范围。
- 审查快照：2026-11-26 23:31（CST）。**审查期间实现 agent 仍在并发改同一 worktree**（23:24 新增 `pointer_transparent` 命中穿透与其 4 条测试；23:25 后无新改动）。本报告的行号/结论对应上述快照，建议收口前对 B-01/B-03/B-04 三条再做一次增量确认。
- 只读约束：本报告只读验证，未改任何源码；`git status --short` 中不新增本报告（`.docs` 被 `.gitignore` 忽略）。

## 1. 范围与方法

### 1.1 读过的设计/证据

- `.docs/popup-overlay-redesign-design.md`：§4.2、§4.3、§4.4、§4.5、§4.6、§4.7（dismiss 矩阵/指针规则/事件模型）、§4.9、§4.10、§4.11（I6–I9）、§4.12、§4.13、§4.14、§5.1、§6(P1)、§7.1、§8、§9、§11（§11.3 偏差、§11.7 P1 起点事项）
- `.docs/popup-overlay-redesign/implementation/p0-summary.md`（P0 damage/结构增量/Canvas 契约基线）
- `.docs/popup-overlay-redesign/implementation/p1-migration-inventory.md`（D1/D3/D9 等引擎补缝决定与 R1–R10 待复核项）
- 实现：`git diff d9949aca -- crates/ui_core` 全量；重点读 `popup/{anchor,placement,settle,stack,dismiss,focus,animation,tests}.rs`、`popup.rs`、`app/{popup,frame,input,focus,scroll}.rs`、`layout/hit_test.rs`、`node/tree{,/content,/arena,/layout}.rs`、`paint/pass.rs`、`window/{paint,frame,focus,a11y}.rs`、`a11y/{action,adapter,mapping,tree,popup,popup_tests}.rs`、`test_support/*`、`tests/{popup_behavior,hud}.rs`、`window/tests.rs`。

### 1.2 跑过的命令（`$env:CARGO_TARGET_DIR='E:\kytrix\target'`）

| 命令 | 结果 |
|---|---|
| `cargo test -p ui_core --features test-support` | **PASS**：lib 2817 passed/1 ignored、frame_record 8、layout 4、**popup_behavior 55**、snapshot 14、demo 39；0 failed |
| `cargo test -p ui_core -p ui_kit`（23:20，kit 迁移尚未落文件时） | **PASS**（含 ui_kit 769）；23:31 复跑因 `ui_kit` 迁移中（`gallery.rs`/`tooltip.rs` 编译错）而红，**与引擎无关** |
| `cargo +nightly clippy -p ui_core --features test-support --all-targets -- -D warnings` | **PASS** |
| `cargo +nightly clippy -p ui_core --all-targets -- -D warnings` / `cargo check -p ui_core --all-targets` | **FAIL**：`tests/popup_behavior.rs` 未声明 `required-features`（见 B-07） |
| `cargo +nightly fmt --all -- --check` | `ui_core` 无 diff；workspace 红仅 `ui_kit/src/kit/{popup_panel,tooltip}.rs`、`market_participants_ui/src/menu.rs`（kit/宿主迁移中的文件，非引擎） |
| `cargo run -p ui_kit --example gallery -- --pixel-check` / `--dump-overlay --auto-close=9` | **未执行**：`ui_kit` 当前不可编译（kit 迁移进行中）。须在 kit 落定后按 §7.5 重跑，不能以“引擎测试绿”替代该门禁 |

结论基线：引擎自身测试全绿、clippy(带 feature) 绿、fmt 绿；但“绿”的覆盖面不等于设计不变量全覆盖（见 B-06/B-17）。

## 2. Findings

### B-01（P0，**阻塞 P1 提交**）`popup_transforms` / `popup_pointer_transparent` 侧表在节点槽位复用时未清理，关闭路径会把“blocked/透明/半透明偏移”泄漏给后续无关节点

- 位置/证据：
  - 两张新侧表 `NodeTree.popup_transforms` / `popup_pointer_transparent`（`node/tree.rs:394`、`content.rs:701-731`）。
  - 槽位初始化只清一部分侧表：`node/tree/arena.rs:278-309` 只重置 `debug_ids/clip_keys/baked_screen_bounds/painted_extents/paint_marks/scroll_slot_of/subtree_bounds/a11y_slots`，**未重置 `popup_transforms`、`popup_pointer_transparent`**；`arena.rs:492` 起的 `remove` 同样只清 `clip_keys/baked_screen_bounds/painted_extents`（`arena.rs:572-581`）。
  - 节点槽位确实复用：`node/id.rs:130-205`（`free: Vec<u32>` + `insert_with` 先 `free.pop()`）；既有测试 `node/tree/tests.rs:109-151` 明确断言“节点索引必须被复用”，且该测试只覆盖 painted_extents/baked_screen_bounds。
  - 写入/清理不对称：`set_popup_transform(Some)` 出现在 `animation.rs:53-59`（open 动画）、`dismiss.rs:100-107`（`block_entry` 置 `blocked:true`）；清理只有 `animation.rs:172`（open 动画自然结束）与 `dismiss.rs:220`（`finalize_close`，即“有 Closing 动画”的路径）。`close_entry`（`app/popup.rs:466-534`）只清 `pointer_transparent`（`:491`），**不清 transform**。
  - 消费点：`layout/hit_test.rs:360-373`（`blocked` → 整子树不可命中；`popup_pointer_transparent` → 整子树不可命中）、`interaction/focus.rs`（`popup_input_blocked` → 不可聚焦）、`paint/pass.rs:398-407`（`offset/opacity` 参与绘制）。
- 可达路径（全部走公开 API）：
  1. `App::close_popup(id)`（立即卸载，无 Closing 动画）作用于仍在 Opening 动画中的 popup（transform 仍为 `Some{offset, opacity<1}`）；
  2. dismiss 路径但 `PopupAnimation::None`：`begin_close_inner` → `block_entry` 写 `blocked:true` → `close_popup_now`（`dismiss.rs:171-183`）；
  3. Modal 关闭事务级联关闭仍在 Opening 的 `owner_modal` 后代（`close_entry` 递归，`app/popup.rs:481-488`）；
  4. 任何外部 `remove_node` 掉 popup 节点。
  此后新节点若复用该 index：`pick` 直接 `None`（不可命中/不可聚焦）或按残留 `opacity/offset` 绘制（不可见/偏移）。残留不会被任何后续代码清除（popup 之外的节点不会再调用 `set_popup_transform(None)`）。
- 影响：跨节点数据污染；表现为“某块 UI 突然点不到/看不见”，且只在特定关闭时序出现，很难定位；违反 I6/I7 的命中与可见性前提。AGENTS §4 侧表生命周期一致性也要求同 `painted_extents` 处理。
- 修法：
  1. `arena::insert_child` 与 `arena::remove` 增加 `popup_transforms[index] = None`、`popup_pointer_transparent[index] = false`（与 `clip_keys/painted_extents` 同段）；
  2. 或把两张表改为随 `Node` 一起存储（避免遗漏新表）；
  3. 扩展现有 `remove_clears_the_extent_and_baked_screen_bounds_side_tables`：写入 transform/transparent → remove → 新建节点复用 index → 断言两表为默认值；并加“关闭中弹层 → 新节点可命中/不可见性”行为测试。
- 是否阻塞：**是**（引擎正确性 + 数据生命周期缺陷，无测试覆盖）。

### B-02（P1，**阻塞 P1 收口**）Scrim 只实现了“命中/生命周期”，没实现“可见遮罩 + 全视口 + 全视口 damage”；`ScrimSpec.density` 完全未被读取

- 位置/证据：
  - scrim 节点样式只有 absolute/left/top/size（`app/popup.rs:25-32`），`create_scrim_node` 无 `paint(...)`（`app/popup.rs:107-123`）→ **scrim 不绘制任何东西**（无压暗背景）。
  - `ScrimSpec.density`（`popup.rs:241-244`）全仓只在测试里构造，生产代码零读取（`rg density` 仅 `popup.rs` 定义 + 测试）。
  - scrim 被当作普通 popup 走 settle：`settle.rs:85-94` 对每个 entry（含 Scrim）计算 `MaxSize = viewport − 2*margin`；`:166-182` 对全部 id 调 `set_max_size`；`:204-266` 走 `solve_placement`；`:274-299` 写 inset。scrim entry 复制 modal 的 `placement`（`app/popup.rs:226,299`），默认 `MaxSize::Viewport`/margin 4 → 800×600 窗口下 scrim 被夹成 **792×592 并 inset (4,4)**，四边各留 4px 未覆盖。
  - 模态/全视口 damage 契约（§4.11：“模态打开/关闭与 scrim：damage = 全视口”）没有任何实现：`rg request_full_damage` 在 popup 路径零命中。
  - 测试只测生命周期：`popup/tests.rs:1181-1232`（断言 tier/z/parent/scrim_for/回收）、`popup_behavior.rs:939-970`（modal+scrim 生命周期）；**无 rect/覆盖面积/绘制/damage 断言**。
- 影响：P1 的 Dialog 迁移完成后，模态对话框没有独立遮罩视觉（对话框浮在未压暗背景上）；即使补绘制，也会因 max-size/placement 缺少四边 4px；“scrim damage=全视口”不成立，后续给 scrim 上色时还会踩残留像素。
- 修法：scrim 作为引擎合成项应跳过 settle 的 max-size/placement（固定 `(0,0,viewport)`，或 `settle` 对 `PopupTier::Scrim` 直接写 inset/rect）；按 `density` 画全视口背景；打开/关闭时 `request_full_damage`（或至少 damage=全视口）；加 `scrim rect == viewport`、`paint_extent 覆盖全视口`、`modal open/close damage_full==true` 测试。
- 是否阻塞：**是**（设计硬要求 + 用户可见回归 + 未使用公开字段）。

### B-03（P1，**阻塞 P1 收口**）驳回事件模型不完整：多条关闭路径绕过 `on_dismiss`；`DismissReason::{ParentClosed,FocusLost}` 永不发出；`EscapePolicy::CloseChain` 被当成 `CloseTopmost`

- 位置/证据（设计 §4.7 “引擎永不直接删除 popup 节点。所有驳回产生事件” + `DismissReason` 定义）：
  - `window_deactivated` 直接 `begin_close_chain`，不 `dispatch`（`popup/dismiss.rs:446-468`）→ 窗口失焦/最小化关闭时 kit 收不到 `FocusLost`。
  - 打开新 popup 时的 tier 替换：`close_auto_roots` / `close_auto_roots_outside` / `close_hints` 直接 `close_popup`（`app/popup.rs:345-368,370-398,400-423`）→ 被替换链的 `on_dismiss` 不触发，kit 无法同步 `open_state/MenuChain`。
  - 关闭级联：`close_entry`（`app/popup.rs:466-534`）递归关 `owner_modal` 后代与 scrim、`begin_close_inner` 关闭子树（`dismiss.rs:118-210`）都不派发子级事件 → `ParentClosed` 从不产生（`rg DismissReason::ParentClosed` 为零）。
  - `EscapePolicy::CloseChain`（`popup.rs:125-129`）只在 `escape()`/`dismiss_topmost()` 里判 `Ignore`（`dismiss.rs:406-424`、`app/popup.rs:695-697`），**从不据此选择 CloseChain**；引擎默认 handler 返回 `Close`，因此 CloseChain 策略等价于 CloseTopmost（死语义）。
- 影响：kit 的 `PopupPanel::on_close(reason)`/`MenuChain` 无法感知“父级关闭/窗口失焦/被新链替换”；典型后果：Dialog 内 Select 的 open_state 悬挂、菜单链在窗口失焦后状态不一致、组件下次 open 变成双开或不再打开。
- 修法：把上述路径统一收敛到 `dispatch(reason)`（至少 `ParentClosed`、`FocusLost`），由 handler 返回 outcome 后再执行；`escape()` 按 `EscapePolicy::CloseChain` 走 `begin_close_chain`；补 `window_deactivated_results_emit_focus_lost`、`parent_close_emits_parent_closed_for_children`、`escape_close_chain_policy_closes_whole_chain` 测试。
- 是否阻塞：**是**（事件模型是 P1-5 的验收面，且 kit 迁移依赖它）。

### B-04（P1，**阻塞 P1 收口**）窗口失焦时 scrim 被当成候选关闭，反过来把 `DismissPolicy::Chain` 的 Modal 一并关掉，违反“窗口失焦保留 Modal”矩阵

- 位置/证据：
  - `window_deactivated` 候选条件：`entry.parent.is_none() && !keeps_on_focus_loss && entry.tier != PopupTier::Modal`（`dismiss.rs:451-464`）。scrim 满足全部：`parent=None`、`tier=Scrim≠Modal`、`dismiss` 直接复制 modal 的（`app/popup.rs:227`；`PopupSpec::new` 默认 `DismissPolicy::Chain{focus_lost:true}`，`popup.rs:161-170`）→ scrim 进入候选。
  - `begin_close_chain(scrim)` → `close_entry(scrim)`：`is_scrim && scrim_for` 且 modal 仍存在 → `close_entry(modal)`（`app/popup.rs:515-523`）→ **modal 被关**。
  - 现有测试 `popup_behavior.rs:530-562` 的 modal 没有 scrim，因此没覆盖；`a_modal_scrim_closes_with_the_modal` 只走显式关闭。
- 影响：用默认/Chain 策略的 Modal + scrim（API 允许）在窗口失焦/最小化时被关闭；矩阵要求“Modal 保持，窗口销毁时统一清理”。
- 修法：`window_deactivated` 排除 `PopupTier::Scrim`；更根本地，scrim 不应携带可参与 dismiss 的 `dismiss/anchor/placement` 语义（B-02 的同一修法），`DismissReason::FocusLost` 的派发按 B-03 处理。补“modal+scrim 窗口失焦仍保留”测试。
- 是否阻塞：**是**（矩阵违反 + 缺测试）。

### B-05（P1，**阻塞 P1 收口**）`Closing` 中点击 opener 的“取消关闭并回 Opening”未实现，且 `PopupState::Opening` 没有任何转移路径可回到

- 位置/证据：
  - 设计 §4.7 矩阵明确：“若按下命中 opener → `Toggle`（关闭并消费；**若在 Closing 中则同 id 取消关闭并回 Opening**）”；§7.1-7 要求“opener toggle 与 Closing 中重开”测试。
  - 实现里 `is_dismissable()` 只有 `Opening|Open`（`popup.rs:85-95`）；`opener_chain`（`stack.rs:671-688`）与 `auto_roots`（`stack.rs:656-669`）都要求 `is_dismissable()`，因此 Closing/DismissPending 链不会被识别为 opener 目标 → 落到“非命中/普通 dispatch”，不会取消关闭。
  - 全仓 `PopupState::Opening` 的写入点只有 `animation.rs:61`（start_open）；没有任何 `Closing/DismissPending → Opening` 的转移。
  - 无对应用例（`rg "Closing.*Opening|reopen"` 在 popup 测试中无命中）。
- 影响：快速“关→点 trigger 重开”在 90ms 窗口内会走成“旧链自然关闭 + 组件层另开新 popup”（不同 PopupId），组件/焦点/a11y 状态与设计语义不一致；也是 §7.1-7 的必需测试缺口。
- 修法：`opener_chain` 增加“Closing/DismissPending 同 opener 链”识别；命中时取消该 id 的 Closing 动画（移除 animation、清 `blocked`、`finalize` 语义回滚）并置 `Opening`（或 Open），按设计消费 press；补测试（fade 关闭中再点 opener，断言 id 不变、状态回 Opening/Open、无第二个 popup）。
- 是否阻塞：**是**。

### B-06（P1，**阻塞 P1 收口**）§7.1 的若干“必须测试”缺失，当前绿门禁无法证明 R2（text/virtual list/settle 交互）与模态 scope 交互正确

- 逐条对照 §7.1（括号内为实现/测试现状）：
  1. §7.1-1“内容高度在 `layout_text` 后才确定；首个 Open 帧 hit_rect == 稳定帧”：`tests/popup_behavior.rs` 与 `src/popup/tests.rs` 中**没有任何文本节点**（`rg Text` 仅 IME editable），首帧测试用固定尺寸 box → 无法覆盖 P0-8/§11.3-10/§11.7 交接的“文本主导首帧”场景（这是本项目 P0 残影的原始触发面）。
  2. §7.1-2“内容增长：tab 切换/列表数据 → 同帧 settle”：只有固定 box 增长；无 virtual list / canvas text popup 用例（`settle` 的 `relayout` 顺序 `settle.rs:48-83` 因此未被真实链路验证）。
  3. §7.1-3“DPI”：无 DPI 测试（逻辑几何不受 DPI 影响，但设计列为必需项）。
  4. §7.1-4“超长菜单缩小 + 内部滚动”：引擎 `set_max_size` 在窗口级有测试（`window/tests.rs:1573+`），但 **popup settle 真实钳制路径无测试**（`popup_settle_rounds_never_exceed_two` 的内容 300×200 小于 792×592，不会触发钳制）；`result.resized`/`Unsettled` 也无断言。
  5. §7.1-6“模态：输入/焦点/a11y/hover/scroll 阻断；modal 内 Select/ColorPicker 可用（owner_modal）；关闭恢复”：只有 press/release 阻断、Tab 循环、a11y 隐藏、owner_modal 级联关闭；**hover/scroll 阻断、owner_modal popup 内可交互/可被外部关闭** 无测试（`closing_a_modal_closes_owner_modal_descendants` 只断言最终为空）。
  6. §7.1-7“Dialog 内 Select 可被外部关闭”“opener toggle 与 Closing 中重开”：均无（后者见 B-05）。
  7. §7.1-11“I7/I8/I9”：I9 有；I7 只测了 z 单调/单子级/模态合成，没有“所有 rect 有限且满足 viewport margin”“modal 之下不得有 popup”等栈不变量断言；I8 的“关闭子先于父”仅测终点（`popup/tests.rs:1096-1118` 名字声称顺序，实际只断言空栈，且 `on_dismiss` 不触发，顺序不可观测）。
- 影响：门禁绿但 R2（text/virtual list）与模态 scope 交互完全无证据；这正是“重构后首个 P1 才暴露、P2 级联菜单才炸”的高风险区。
- 修法：按上表逐条补 `tests/popup_behavior.rs` 用例（文本高度首帧、bind_text 增长、virtual list 窗口内增删、DPI、max-size 钳制+Unsettled→稳定、modal hover/scroll 阻断、Dialog 内 Select 外部关闭、stack invariant helper）；文本/virtual list 用例应接到 `PersistentPixels`（§11.3-16 已登记）。
- 是否阻塞：**是**（P1 验收要求 §7.1 落地；“门禁绿”不能替代）。

### B-07（P2）`tests/popup_behavior.rs` 未声明 `required-features = ["test-support"]`，导致 `cargo test/check/clippy -p ui_core`（不带 feature）编译失败

- 证据：`crates/ui_core/Cargo.toml` 的 `[[test]]` 段只有 frame_record/snapshot/layout/hud（`:52-75`），没有 popup_behavior；该测试文件 `use ui_core::test_support::app::TestApp`（`tests/popup_behavior.rs:17`），而 `test_support` 由 `#[cfg(feature="test-support")]` 门控（`lib.rs:16-17`）。
- 复现：`cargo check -p ui_core --all-targets` / `cargo +nightly clippy -p ui_core --all-targets -- -D warnings` / `cargo test -p ui_core` 均 `E0433: cannot find test_support in ui_core`（本次实测）；`cargo test -p ui_core -p ui_kit` 能过只是因为 `ui_kit` 的 dev-dependency `ui_core = { features=["test-support"] }`（`crates/ui_kit/Cargo.toml:21`）在 resolver 3 下做了 feature 统一——属于“偶然通过”，不是门禁设计。
- 影响：开发者的单 crate 门禁（AGENTS §8 的 `cargo test` 单包路径、clippy 单包路径）失败；一旦 kit 迁移删除/调整该 dev-dep，官方 `cargo test -p ui_core -p ui_kit` 也会随之失败。
- 修法：在 `Cargo.toml` 增加 `[[test]] name="popup_behavior" required-features=["test-support"]`（与同目录其他集成测试一致）。
- 是否阻塞：否（但收口前应修，1 行）。

### B-08（P2）公开/内部死字段与死 API：`solved_origin`、`settled_size`、`PopupEntry::rect()`、`PopupDirty.measure`，以及 `ScrimSpec.density`（见 B-02）

- 证据：`solved_origin` 只在 `settle.rs:292` 写入、`app/popup.rs:281/316` 初始化，无任何读取；`settled_size` 只在 `settle.rs:325` 写入；`PopupEntry::rect()`（`popup.rs:549-557`）仅 `popup/tests.rs:374-377` 使用；`PopupDirty.measure` 只经 `any()` 参与判断（`popup.rs:104-109`），没有独立分支；`ScrimSpec.density` 见 B-02。
- 影响：违反 AGENTS §4“不再使用的代码、类型、函数、字段和依赖应直接删除”；`solved_origin` 本应是设计里“子 popup 锚点增量修正”的真值，实现却用局部 `Solved.delta`（`settle.rs:101-106,219-221`），留下两个真值来源，后续维护者会误用。
- 修法：删除未读字段或用其替代局部 `Solved.delta`（二者选一，保持单一真值）；`rect()`/`measure` 若 kit 不用则删；`density` 按 B-02 接入或删除。
- 是否阻塞：否。

### B-09（P2）`App::close_popup`（立即）与 `begin_close_popup`（事务/动画）语义不对称，缺少显式契约与“Opening 中关闭”的测试

- 证据：`App::close_popup` → `close_subtree` → `close_entry` 立即卸载（`app/popup.rs:433-435,442-456,466-534`），不置 DismissPending、不触发 on_dismiss、不播关闭动画；`begin_close_popup` → `dismiss::request_close` → handler → `apply_outcome` → `begin_close`（`app/popup.rs:651-658`、`dismiss.rs:373-396`）。设计 §4.7“关闭顺序：标记 DismissPending → 子级先关 → 父级”、§4.12“关闭 90ms fade”未区分两条入口；inventory D1 只是“补了 API”，没有定义 `close_popup` 的语义边界。
- 影响：kit `PopupPanel::close_now`/toggle 会走立即路径；若此时 popup 正在 Opening 动画中，会留下 B-01 的 stale transform（组合缺陷）。同时 I8 的“Closing 立即不可命中”在 `close_popup` 路径上不成立（节点直接消失，但会触发级联等副作用）。
- 修法：明确并文档化两个入口的契约（`close_popup = immediate teardown`，`begin_close_popup = transactional`），并在 `close_popup`/`close_entry` 中清理动画与 transform（B-01），补“Opening 中 close_popup”测试。
- 是否阻塞：否（与 B-01 合并修）。

### B-10（P2）外部删除 modal 节点后 scrim 会成为孤立全屏“吞点击”层，设计要求的“modal 不存在时回收 scrim”未实现

- 证据：`PopupStack::remove` 对 `scrim_for == Some(removed)` 只做 `scrim.scrim_for = None`（`stack.rs:315-326`），没有级联删除；`prune_missing` 只回收“自身 node 已消失”的 entry（`stack.rs:351-364`）。`App::remove_node` 是公开 API（`app/window_set.rs:9-15`），删除 popup 内容节点后：entry 被 prune，scrim entry/node 仍存活且 `scrim_for=None`，命中测试仍会命中该全屏节点（dismiss 对 scrim 只 Consume，不关闭）→ 全屏吞点击。
- 设计原文：§4.6“scrim_for 指向的 modal 不存在时 scrim 必须被回收”。
- 修法：`remove(modal)`/`prune_missing` 后扫描并关闭 `scrim_for` 为 None 的 Scrim；或 `close_entry` 外部删除路径同样级联；补测试（remove popup 内容节点后栈空、无孤儿全屏节点）。
- 是否阻塞：否（正常关闭路径不会触发，但公开 API 可达）。

### B-11（P3）新生产代码中有 `expect` 兜底 panic；`sync_order` 的失败路径被 `expect` 吞成 panic 而非 `PopupError`

- 证据：`app/popup.rs:146-148,502-504,545-547` 对 `sync_order` `.expect("popup root children always mirror the popup stack")`；`stack.rs:250,254,290,371`、`placement.rs:186` 为不变量 expect。
- 影响：`sync_order` 在 popup root 子节点集合与 `order` 不一致时返回 `EngineError`，理论上可由“公开 snapshot 暴露 root + 用户/kit 往 popup root 挂节点”或未来 reparent 触发；设计 §4.13 要求变更型 API 返回强类型错误、AGENTS 禁止生产 panic。其余 expect 为已证明不变量，可保留但建议 `debug_assert` 化。
- 修法：`sync_order` 失败映射为 `PopupError::ContentBuild`/新增强类型错误并 `?`；或把 `sync_order` 做成不变量断言 + 文档约束。
- 是否阻塞：否。

### B-12（P3）`PopupState::Opening` 在首帧 settle 后即被改成 `Open`（动画仍在进行），状态机语义与 kit 观察面不一致

- 证据：`start_open` 置 `Opening`（`animation.rs:61`），但同一帧 `settle` 对所有 solved entry 无条件 `set_state(Open)`（`settle.rs:316-329`），动画继续由 `animations` 列表驱动。测试只断言动画结束后 `Open`（`popup_behavior.rs:893-898`），未覆盖“动画中 state”的语义。
- 影响：kit 若按 `PopupState` 判断“开启中”会失效；设计数据模型把 Opening/Open 分开，P2 MenuChain 也可能依赖。至少应在设计 §11 偏差登记中写明“Opening 只表示首帧 settle 前”。
- 修法：settle 只在 `!animating` 或动画完成时置 Open；或文档化并给 popup_behavior 增加“动画中 state==Opening”断言。
- 是否阻塞：否。

### B-13（P3）`InitialFocus::DefaultButton` 只是“第一个 role==Button 的可聚焦节点”，没有引擎级“默认按钮”标记（D16 语义依赖 kit 约定）

- 证据：`focus.rs:20-35`（遍历子树，取首个 `A11yRole::Button`），`:40-48`（Trap 分支）。设计 D16 要求“默认按钮优先”，但 PopupSpec 无 default 标记；测试 `default_button_wins_the_trap_initial_focus` 通过给两个按钮都设 Button role 来验证“取第一个”，并没有验证“默认按钮”语义。
- 影响：Dialog 的“默认按钮”完全取决于 kit 是否把默认按钮放在第一个/是否给 role；若 kit 约定变化，引擎行为随之改变。建议在 §11 偏差登记，或给 `PopupA11y` 增加 `default_action` 语义（P3）。
- 是否阻塞：否。

### B-14（P3）a11y Dismiss/HideTooltip 作用对象是“最顶层 popup”而非 AT 请求的节点；`ShowTooltip` 已声明但不处理

- 证据：`a11y/action.rs:82-95`（`A11yAction::Dismiss → app.dismiss_topmost`、`HideTooltip → hide_topmost_tooltip`，均不看 `engine` 节点）；`hide_topmost_tooltip`（`:456-482`）以“Hint 且 is_dismissable”取最顶层；`ShowTooltip` 直接返回 false 并记 `UnhandledAction`（`:90`）。设计 §4.10 要求 `Action::HideTooltip → DismissReason::A11yDismiss`（只关 tooltip），menu/dialog 用 CustomAction(“dismiss”)。
- 影响：tooltip 在菜单之上时（Hint 可共存），对菜单执行 AT Dismiss 会关掉 tooltip；`ShowTooltip` 无实现（设计已列 action）。属边界行为，a11y 测试未覆盖。
- 修法：按请求节点反查其所属 popup（`stack.enclosing`），只对目标 popup 生效；`ShowTooltip` 至少走统一 handler 或明确返回未支持并登记。
- 是否阻塞：否。

### B-15（P3）指针/穿透卫生：`suppress_release` 在失焦/离开时不重置；`on_pointer_up` 无 scope 复核；`PopupStack::popup_at`/`subtree_hit_contains` 不看 `pointer_transparent`/blocked

- 证据：`suppress_release` 只在 `input.rs:231` 置 true、`:448` 取走；`on_pointer_leave`/`on_window_focus_lost` 不清（`input.rs:566-589,905-925`）；`popup_at` 只过滤 `tier != Scrim && is_dismissable`（`stack.rs:628-636`），`subtree_hit_contains` 连 state 都不过滤（`:639-648`），均不看 `pointer_transparent`/`blocked`。当前没有可观测的错误路由（tree hit_test 已对透明/blocked 返回 None），因此是潜在一致性问题：一旦 `popup_at` 被用于“命中即视为 popup 内部”的新分支，就会把透明 tooltip/fading 层当内部。
- 修法：`popup_at`/`subtree_hit_contains` 加 `pointer_transparent == false && popup_input_blocked == false`；`suppress_release` 在 leave/focus-lost 清零；`on_pointer_up` 对 `scope_active && !in_input_scope(hit)` 统一 consume 或显式中断 press。
- 是否阻塞：否。

### B-16（P3）settle 触发条件包含 `layout_ran`（任何主树布局帧都会 re-solve 全部 popup），超出 §4.3 的“只有 PopupDirty 置位才运行”

- 证据：`settle.rs:113-127`：`needs = stack.dirty().any() || viewport_changed || (layout_ran && !stack.is_empty()) || stack.animating()`。空闲帧 0 工作仍成立（测试 `idle_frames_do_no_popup_settle_work`），但主树动画/布局帧会带来 O(popups×depth) 的 solve（含 `PlacementSpec` clone）——P3-5 帧预算复核项。
- 修法：记录该行为为设计偏差（锚点可能随主树布局移动，保留是合理的），或维护“可能影响 anchor 的脏集”收窄；在 P3-5 基线里量化。
- 是否阻塞：否。

### B-17（P3）测试真实性细节：首可见帧测试未断言“该帧确实被绘制”；关闭顺序测试只断言终点；popup 套件没有负向/变异自检记录

- 证据：
  - `opening_a_popup_settles_geometry_in_the_first_frame`（`popup_behavior.rs:191-218`）只断言 rect/settle_rounds，未断言本帧 `FrameOutcome::Committed` 或有 damage；若 settle 正确但绘制被推迟到下一帧，测试仍绿。
  - `close_popup_closes_children_before_the_parent`（`popup/tests.rs:1096-1118`）名字声称顺序，实际只断言空栈；由于 B-03（无 on_dismiss），顺序本身不可观测。
  - P0 有“注入故障/≥2 分散像素自检”的负向证据（§11.3-9），P1 popup 套件没有类似记录（是否曾故意破坏实现验证测试会红，无文档）。
  - 正面：时间推进用 `TestApp::advance`（pump，无 sleep）；几何断言多为精确 rect；`raise`/`slide` 有 damage 覆盖与 `!damage_full` 断言；a11y 测试接入 accesskit 影子树契约。
- 修法：首帧测试加 `FrameOutcome::Committed`/damage 非空断言；关闭顺序改为通过 on_dismiss 记录顺序（依赖 B-03 修）；给 2–3 条关键 popup 测试补一次变异自检并记录（例如注释掉 `apply_inset` / 清空 transform 清理，确认对应测试变红）。
- 是否阻塞：否。

## 3. 已核实正确的点（简短）

1. **帧序与 settle 插入点**：`app/frame.rs:157-217` 在 canvas-text eviction 循环之后、`ensure_visible/blink/IME` 之前调用 `settle`；`relayout` 顺序为 `layout_if_dirty → layout_text → rewindow_virtuals → clamp_scroll_offsets → resolve_canvas_text`（`settle.rs:48-83`），与 §4.3 一致；两次 layout 的 report 合并进 `FrameStats`（`frame.rs:213-214`、`merge_*`）。
2. **2 轮上限/一次性 inset**：`settle.rs:166-186`（尺寸轮）→ `:274-299`（一次性 `set_absolute_inset` + 单次 relayout）→ `rounds ≤ 2`，测试 `popup_settle_rounds_never_exceed_two` 通过。
3. **子 popup 同帧跟随**：`settle.rs:219-223`（`anchor + parent.delta`）按深度序处理 + 单次 inset；`three_menu_levels_follow_the_parent_without_a_frame_lag` 精确断言 3 级几何与 re-anchor 同帧跟随。
4. **首可见帧 rect == 稳定帧（固定尺寸内容）**：`opening_a_popup_settles_geometry_in_the_first_frame` 断言精确 rect 并 `stable_frame` 相等（文本内容缺口见 B-06）。
5. **dirty 闸门与 idle**：`needs` 门控 + `take_dirty`；`idle_frames_do_no_popup_settle_work` 断言 `FrameOutcome::Idle`；`Window::needs_frame` 纳入 `popups.dirty()/animating()`（`window/frame.rs:79-88`）；open/close/raise/set_anchor/request_measure 都 `request_frame`。
6. **paint_offset 不参与 layout、hit 跟随、damage 覆盖旧∪新**：`hit_test.rs:360-425` 平移命中、`paint/pass.rs:398-407` 平移绘制（符号一致：均为 `+offset`）、`animation.rs:130-135` damage 旧/新偏移并集；`slide_animation_offsets_paint_and_hit_without_moving_the_settle_rect`、`slide_animation_damage_covers_the_offset_and_the_final_rect` 通过。
7. **Closing 立即不可命中/不可聚焦、动画结束移除、非模态动画帧 !damage_full、modal fade-only**：`dismiss.rs:78-108`（blocked transform）、`hit_test.rs:368-370`、`interaction/focus.rs`（`popup_input_blocked`）、`dismiss.rs:212-228`、`animation.rs:83-187`；`closing_animation_blocks_hit_immediately_then_removes_the_node`、`closing_animation_frames_never_force_a_full_repaint`、`modal_animation_is_fade_only` 通过。
8. **dismiss 解析顺序**：`dismiss.rs:281-371` 严格按 §4.7 的 0（scope 优先/Scrim 命中消费）→1.1（scope 内最高 Auto 链 outside 优先，`auto_roots.last()`）→1.2（同 opener Toggle 消费/不同 opener 先关后不消费）→1.3（命中祖先面板只关后代）→1.4（`on_dismiss`，默认 `Replay`，`Consume` 显式 opt-in）；press/release 抑制用 `suppress_release`（`input.rs:221-236,445-457`）；`pointer_down_on_the_opener_toggles_the_chain_and_consumes_the_press`、`outside_press_replays_the_click_to_the_background`、`outside_press_consume_is_explicit_opt_in`、`a_different_opener_closes_the_old_chain_and_dispatches_the_press`、`pressing_inside_an_ancestor_menu_closes_only_its_descendants`、`a_dialog_blocks_background_openers` 通过。
9. **Esc 语义**：跳过 Closing/DismissPending（`is_dismissable`）、Hint 优先、`consumed` 传播到宿主（`dismiss.rs:398-425`、`input.rs:815-825`）；`escape_closes_exactly_one_open_level`、`a_tooltip_escape_closes_only_the_tooltip`、`escape_skips_closing_entries_and_closes_the_next_open_level` 通过（CloseChain 缺口见 B-03）。
10. **焦点单一真值**：`App::focused` → `app/focus.rs:11-23` → `popup_focus::effective_focus`；paint（`window/paint.rs:140`）、a11y（`window/a11y.rs:4-15`）、input/cursor/IME/blink/ensure_visible/text_edit 全部走 `App::focused`（grep 无旁路）；snapshot/restore/嵌套 owner/“不抢新 popup 焦点”/Trap Tab 闭环/Three-branch RestoreFocus 均有测试并通过。
11. **a11y 链嵌套与属性接线**：`a11y/popup.rs:112-216`（logical parent＝a11y_parent→父 popup→opener、expanded/haspopup/owns/controls/described_by/active_descendant、modal scope、scrim hidden）；accesskit 字段由 `adapter.rs:651-690,797-815` 写入；`assert_unique_parents`、增量投影、modal 背景隐藏、separator 隐藏、active_descendant 跟踪等 16 条 `a11y/popup_tests.rs` 通过；popup root 作为 skip-container 不产生 a11y 节点。
12. **结构增量与 z**：`reorder_children` 不触发 structure epoch、damage 旧序并集（`arena.rs:379-478`）；`raising_a_popup_reorders_paint_and_hit_without_a_full_repaint` 断言 `!damage_full`、`order_rebuilds +1`、命中顺序随绘制顺序；popup root 固定 tail、HUD 永远最上（`tests/hud.rs` 新增用例）。
13. **代码质量（引擎新文件）**：无新增依赖；`rg '#\[allow|#\[expect|//'` 在新增生产/测试文件为 0；强类型错误面完整（`PopupError` + `EngineError::InvalidMaxSize`）；无 `unwrap`；`expect` 仅不变量点（B-11）。
14. **错误路径与回滚**：depth/cycle/stale/limit/invalid geometry/invalid placement/anchor missing/content build 回滚都有强类型测试（`popup/tests.rs:896-1039,1434-1472`）；`open_popup` 失败路径清理节点且不污染栈。

## 4. 测试真实性评估与残余风险

### 4.1 真实性结论

- 规模与结构：`popup/tests.rs`（约 53 条单元/集成）、`tests/popup_behavior.rs`（55 条端到端）、`a11y/popup_tests.rs`（16 条），覆盖 settle/dismiss/focus/animation/damage/a11y 的主路径；断言多为精确 rect、状态、damage 覆盖、accesskit 契约，不是空断言/仅 happy path。
- 时间控制：动画/延迟相关全部用 `TestApp::advance(Duration)` + `frame()`（pump），无 `sleep`/硬编码墙钟，可确定复现。
- 可失败性（抽样推理）：`opening_a_popup_settles_geometry_in_the_first_frame` 若删掉 settle 会因 hit_rect 为空/原点失败；`raising_a_popup...` 若删掉 reorder damage 会失败；`outside_press_replays...` 若把默认改 Consume 会失败；`escape_skips_closing...` 若 Esc 不跳 Closing 会失败——说明不是“必然绿”的套件。
- 但**缺负向自检记录**：P0 的像素门禁有“注入故障+2 点分散像素自检”的可审查证据（§11.3-9），P1 popup 套件没有同类记录；B-17 的两处弱断言也说明关键语义（“确已绘制”“关闭顺序”）没有被真正锁定。
- 未覆盖（见 B-06）：文本高度首帧、virtual list、DPI、max-size 真实钳制、modal hover/scroll、owner_modal popup 可交互/外部关闭、I7 栈不变量；以及 B-02/B-04 的 scrim 几何/失焦行为。

### 4.2 残余风险

1. **R2（text/virtual list settle）无证据**：settle 的 `relayout` 内部顺序虽抄自设计，但没有一条能失败的测试证明“文本主导高度/虚拟列表重排 → 首帧稳定”；P0-8 删除后这是唯一防线。
2. **并发修改**：审查期间实现 agent 仍在改引擎命中/测试（23:24-23:25）；B-01/B-03/B-04 的结论基于 23:31 快照，收口前需增量复核（尤其若 `close_entry`/`window_deactivated` 被改动）。
3. **门禁失真**：`ui_kit` 迁移中导致官方命令 `cargo test -p ui_core -p ui_kit`、workspace fmt 当前红（非引擎原因）；`gallery --pixel-check`/`--dump-overlay` 完全未跑。P1 收口必须等 kit 落定后按 §7.5 全量重跑，且 B-07 的 target 声明要修，否则单 crate 门禁永久红。
4. **设计偏差未回写**：`Opening` 语义（B-12）、`close_popup`/`begin_close_popup` 双入口（B-09）、DefaultButton 启发式（B-13）、settle `layout_ran` 触发（B-16）、以及本实现的额外公开 API（`begin_close_popup/popup_state/set_popup_a11y/popup_diagnostics/window_focus_lost(test)`）都应在 §11 偏差登记，否则下一轮审查会重复发现。
5. **性能**：popup 帧的 settle/solve 在“主树布局帧”都会跑（B-16）；动画帧 lane 因 offset/opacity 变化重建（设计允许）；P3-5 需以 `damage_full==false` + 帧预算复核。

## 5. 结论（给 lead）

- 严重度计数：**P0 × 1（B-01）、P1 × 5（B-02/B-03/B-04/B-05/B-06）、P2 × 4（B-07/B-08/B-09/B-10）、P3 × 7（B-11…B-17）**。
- 最关键的 3 条：
  1. **B-01** 侧表不清理导致槽位复用后节点不可命中/不可见（P0，公开 API 可达，无测试）；
  2. **B-02 + B-04** scrim 只有命中/生命周期：无可见遮罩、`density` 未用、非全视口、无全视口 damage，且窗口失焦会经 scrim 反关模态（P1，Dialog parity/矩阵违反）；
  3. **B-03 + B-05** 驳回事件覆盖不全（ParentClosed/FocusLost 永不发、四条关闭路径绕过 handler、CloseChain 死语义）+ Closing 中重开未实现（P1，kit 迁移的直接依赖）。
- **是否可进入 P1 收口：否。** 引擎骨架、settle/动画/焦点/a11y 主路径质量较好（测试 55+53+16 条全绿、clippy/fmt 干净），但上述 1×P0 + 5×P1 触及正确性、设计硬要求与 §7.1 必需测试；建议按 B-01 → B-02/B-04 → B-03 → B-05 → B-06 → B-07 的顺序修复并补测试，然后在 kit 迁移落定后重跑 §7.5 门禁与 gallery 命令再复审。
