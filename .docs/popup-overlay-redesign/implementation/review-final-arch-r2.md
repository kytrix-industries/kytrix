# 最终对抗性审查 r2（架构 / 行为 / 跨阶段）— review-final-arch-r2

- 审查员：子 agent `final-review-arch`；轮次 **r2**（r1 = `review-final-arch.md`；只读代码 + 仓库外探针 + 门禁复跑）
- 快照：**HEAD `00927ff9`**（r2 开始于 `5df859b8`；审查期间 lead 追加 `5374c4c8 fix(gallery): real-pixel resize 断言平台无关`、`00927ff9 docs(kit-gallery): P3 验收/真实窗口像素/动画回写`）
  - 工作树快照：**非干净**——`git status --short` 在 r2 期间由并发 agent 产生了未提交改动（22:14 快照：`M crates/ui_core/src/popup/focus.rs`、`M crates/ui_core/tests/popup_behavior.rs`、`M crates/ui_kit/src/kit/color_picker.rs`、`M crates/profile_ui/src/profile.rs`、`M tools/macos-gates-runner.sh`；`?? tools/source-id.txt`）。本报告结论以 **HEAD 00927ff9** 为准；并发未提交改动（引擎 `leave_owner_scope` 焦点修复、ColorPicker 内部 Tab 遍历修复+测试、profile 宿主修复、mac runner evidence 路径）不在 HEAD 内，需 lead 提交并复跑。
  - 修复提交 `5df859b8` 相对基线 `fa41987c`：25 files，+2848/−660；其后 `5374c4c8` 8 行（gallery 断言）、`00927ff9` 12 行（kit-gallery 文档）。
- 结论摘要：**FA-01/FA-02/FA-03/FA-04 全部确认关闭**；但 r2 新发现 **P2 = 1（R2-01）**、**P3 = 3（R2-02…R2-04）** → 按用户“P0–P3 全 0”门槛，**当前不可宣布完成**。
  - 最关键：修复把“非 Trap popup 的 Tab 跳出”统一实现为“首个 Tab 即关闭 popup”，对 **ColorPicker** 这种内部有多个可聚焦控件（两个页签、hex 输入、Confirm/Close）的组合面板造成键盘可达性回归（内部控件无法用键盘到达）；Dropdown/Select（单焦点列表）与 Menu（Kit 自带 Tab 关链）不受影响。
  - 另两条 P3 为修复引入的新功能细节（Dropdown/Select typeahead 不识别 Shift+字母）与交付/证据口径（文档“最终 HEAD 5df859b8”与当前 HEAD/证据快照不一致；工作树含未跟踪 `tools/source-id.txt`）。

---

## 1. r1 Findings（FA-01…FA-04）关闭复核

### FA-01 [r1 P2] 安全三角/grace 内按下被 replay 到背景 —— **closed**

| 证据 | 内容 |
|---|---|
| 引擎语义 | `crates/ui_core/src/popup/dismiss.rs::resolve_auto_outside`：`DismissOutcome::Ignore => PressRouting::Consume`（原 `Dispatch`）；`outside_ignores`/`OutsidePress::Ignore` 分支删除。`app/input.rs` 的 `Consume` 路径仍置 `suppress_release=true`，按下与抬起都被吞掉 |
| 引擎测试 | `tests/popup_behavior.rs::an_ignored_outside_press_is_consumed_and_never_reaches_the_background`：断言 down/up 均 consumed、背景 click = 0、popup 仍 Open；另有 `an_ignored_outside_press_does_not_drop_the_popup_or_leave_it_stuck` |
| kit 测试 | `kit/menu.rs::a_press_inside_the_safe_triangle_keeps_the_chain_open` 现于三角落点下**新增覆盖背景节点**（`background("safe-triangle-cover", triangle±12)`），断言 click delta = 0；并加**正向自检**：关闭菜单后同一落点 click delta = 1（证明落点确实在背景控件上），消除 r1 指出的空断言 |
| 独立探针（仓库外） | 同一探针 r1：`background clicks delta = 1`；r2（HEAD `00927ff9`）：**delta = 0**，menu/submenu 均保持 Open（`E:\kytrix\target\final-review-arch\probe-safe-triangle-r2.txt`） |

**残余风险（不计数）**：`Ignore` 现在语义为“消费按下+抬起、不关链”，与 r1 建议一致；未发现需要 Dispatch 的合法调用方（`OutsidePress::Ignore` 策略变体已删除）。

### FA-02 [r1 P2] workspace `cargo test` 红 —— **closed**

- `settings_ui/src/settings_tests/credentials.rs:332-335` 在 `press(Escape); settle(3)` 后补 `harness.advance(200)`（与姊妹用例一致）；同时 `settings.rs` 的写失败 Alert 增加 `.ok_action(|_, app| dialog.close(app))`，`settings_tests/shell.rs` 新增“确定关闭 Alert + title 消失 + `popup_total()==0`”断言（顺带修复 FI-02 的宿主级死路）。
- 实测：`cargo test -p settings_ui --lib` 在 r1 为 235 passed / 1 failed；修复后 **`cargo test --workspace --no-fail-fast` 退出码 0（无 FAILED/panic）**；HEAD `00927ff9` 上 `cargo test -p ui_core -p ui_kit` 亦全绿。**Gates 不再有确定性红。**

### FA-03 [r1 P3] `PopupState::Closing` 永不赋值 —— **closed**

- `popup/dismiss.rs::begin_close_inner`：`block_entry(...)` 后新增 `mark_closing(...)`（`set_state(Closing)`），动画结束才 `finalize_close`。
- 测试：`popup_behavior` 多处断言 `popup_state(id) == Some(PopupState::Closing)`（关闭、Esc、modal/inner/outer、scrim 捆绑场景）；`kit/dialog.rs` 由 `DismissPending` 改为断言 `Closing` 且 `!is_dismissable`。
- 一致性：`dismiss_topmost`/`escape` 跳过、`cancel_close` 接受 `Closing|DismissPending`、`animation::tick` 的 blocked、a11y `is_visible`（Closing 仍算 visible，模态关闭期继续阻断输入）、settle 跳过非 Opening/Open —— 均与 r1 报告要求一致；ui_core 124 条 `popup_behavior` 全绿。

**残余观察（不计数）**：`PopupState::Closed` 仍不可通过 `App::popup_state` 观测（entry 在 remove 前移出，API 返回 `None`），kit `Some(PopupState::Closed)` 分支仍为死匹配；作为 P4 状态机清理项即可。

### FA-04 [r1 P3] 新增公开 API 死变体 —— **closed（r1 所列项）**

- 删除：`ZPolicy`/`PopupSpec.z`、`popup::MaxSize`/`PlacementSpec.max_size`、`OutsidePress::Ignore`、`EscapePolicy::CloseChain`、`RestoreFocus::RootFallback`；`reserve_z`/`reserve_z_pair` 去掉策略参数；`solve_placement`/`settle::max_size_for` 统一按 viewport clamp（行为等价原 `Viewport` 默认）。
- 同步：`FocusPolicy::None::restore()` 由 RootFallback 改 Previous（None 路径不消费）；`escape` 的 CloseChain 分支删除；相关删除测试一并移除。
- 编译/门禁：`cargo check --workspace --all-targets`、clippy、fmt 全 PASS；设计 §4.2 已回写（`popup-overlay-redesign-design.md:386-395`：ZPolicy/MaxSize 删除声明、PopupSpec 最终字段、Anchor::NodeRect 保留口径等）。
- 残余观察（不计数，均属设计文档化的公开 API）：`Anchor::NodeRect` 仍无生产构造（仅引擎实现 + 测试；lead 的“NodeRect 证据”为测试覆盖 + 设计 §4.2 登记）；`InitialFocus::FirstFocusable` 仅测试构造（设计 §4.2 说明其为 fallback）；`EscapePolicy::Ignore` 由 `DismissPolicy::{Explicit,Hint}.escape=false` 映射构造，非死变体；`ShiftPolicy::None` 已被 kit `context_menu`/`dialog` 使用。若 lead 要求“公开 API 零测试-only 变体”，可再收窄 NodeRect/FirstFocusable，但不属于 r1 FA-04 所列死变体。

---

## 2. 修复引入的新风险（逐面扫描）

### 2.1 `Ignore → Consume` 的 dispatch/replay 语义 —— 未发现新问题

- `resolve_auto_outside` 仅剩两条路径：`Ignore → Consume`、`Close/CloseChain →（Consume 策略 ? Consume : Dispatch）`；默认 Replay 的“关链后把 press+up 派发给背景”未被改动（`a_press_outside_the_safe_triangle_closes_the_chain_and_replays` 绿）。
- 模态路径（`Explicit`/`on_pointer_down` modal scope）不经过 `resolve_auto_outside`，不受影响；`DismissOutcome::Ignore` 在 modal 分支不走 PressRouting。
- 触摸/拖拽：`Consume` 会清 `pointer.press/pending_drag` 并置 `suppress_release`，与旧 `Consume` 策略行为一致；未发现 Ignore 场景下需要继续派发的调用方（`OutsidePress::Ignore` 已删除）。
- 新引擎测试覆盖“Ignore 后 popup 仍能正常关闭/继续交互”，无状态卡死。

### 2.2 非 Trap popup 的 Tab 跳出 —— **Menu/Modal 无回归，ColorPicker 有回归（R2-01）**

- 模态（Trap）：`focus_next/prev` 仍走 `next_in_popup_scope`（closed-loop），`dialog_tab_cycles_inside_the_modal_scope` 绿。
- Menu/ContextMenu：Kit 自带 scoped Tab→关链逻辑优先于引擎默认路径，`menu` 相关 Tab/键盘测试全绿。
- Dropdown/Select：`ItemPanel::bind_panel_keys` 新增 Tab/Shift+Tab→`tab_out`（`close_now` + 回 opener + `focus_next/prev`），新测试 `tab_closes_the_dropdown_and_moves_focus_past_the_trigger`、`shift_tab_...`、Select 对称用例均绿；列表行 `rows_tab_stop(false)` 后 Tab 不再落到行/命中壳（FI-02 的 Enter 与高亮错位同时消除）。
- **ColorPicker**：见 R2-01，新增 Tab→无条件 `close_now`，导致内部控件无法用键盘到达。

### 2.3 `Closing` 状态对 kit/dismiss/a11y 的影响 —— 未发现新问题

- `begin_close_inner` 对每个候选先 `block_entry`（hit/focus 立即失效）再 `mark_closing`；scrim 与 modal 同批进入 Closing，`finalize_close` 统一移除。
- `PopupPanel::{is_closing,state,sync}`、`Menu::sync_own` 的 `DismissPending|Closing` 分支、a11y 投影（非 dismissable→hidden、modal scope 仍 visible）、`settle`（仅 Opening/Open）全部兼容；`dismiss_topmost`/`escape` 跳过 Closing 的行为符合 r1 前设计。
- 专项测试：kit dialog Esc 后 `state==Closing` 且 `!is_dismissable`、节点仍在；modal Closing 期间输入阻断用例（E-01）全绿。

### 2.4 删除变体后的 API 完整性 —— 代码完整，历史文档行仍引用已删测试（R2-03 子项）

- 代码：全 workspace check/clippy 绿；`rg` 无 `ZPolicy/MaxSize/OutsidePress::Ignore/EscapePolicy::CloseChain/RestoreFocus::RootFallback` 生产引用（设计 §4.2 已回写删除）。
- 文档缺口：设计 §12.7 的 **A-03（`invalid_z_policy_open_leaves_no_residue_and_next_open_works`）、A-14（`escape_close_chain_policy_closes_the_whole_chain`）** 仍以“已修 + 该测试名”为证据，而这两个测试已随变体删除；§4.7 关闭事务仍写“置 `DismissPending` → 启动动画”（未提 `Closing`），且 §4.7-1.4 仍写“`Consume`/`Ignore` 为显式 opt-in”（`Ignore` 已不是 `OutsidePress` 变体）。属文档-代码一致性 P3（不影响运行）。

---

## 3. Findings（r2 新增；已登记 P4 项不重复计数）

### R2-01 [P2] ColorPicker 在首个 Tab 即关闭，内部可聚焦控件（HSLA 页签 / hex 输入 / Confirm / Close）键盘不可达

**位置/证据**

- `crates/ui_kit/src/kit/color_picker.rs:1134-1150` `tab_out`：`if !self.inner.popup.close_now(app) { return; }` 然后 `set_focus(trigger)` + `focus_next/prev`；`:1152-1173` `bind_tab_keys` 把 Tab/Shift+Tab **scoped 绑定在 content** 上（对面板内任意焦点生效）。
- 新增测试明确锁定“首个 Tab 关闭”：`tab_closes_the_picker_and_moves_focus_to_the_next_control`、`shift_tab_closes_the_picker_and_leaves_the_panel`（断言 `!panel_open` 且焦点离开面板）。
- 面板内可聚焦节点（`tab_stop(true)`/editable）：两个页签（`color_picker.rs:1535` 附近的两个 tab 按钮）、hex 可编辑输入（`InputNode`，构建于 `build_panel`）、Confirm（`:1691` 附近）、Close；`initial_focus_node(Take)` 只把首焦点放在第一个 focusable（首个页签），因此不按 Tab 无法到其余控件。
- 修复前（`fa41987c`）行为：非 Trap popup 的 Tab 由引擎 `next_in_popup_scope` 在 owner scope 内**顺序循环**，ColorPicker 内部控件可达（r1 手读 `app/focus.rs` + `popup/focus.rs` 旧路径；ui_kit 无 Tab 测试锁定，但行为如此）。
- 复核：ColorPicker 除 Tab 外**没有**其他键盘遍历绑定（`rg 'KeyBinding|KeyCode::'` 仅 Tab 两项）；菜单式方向键不适用于取色器。

**影响**：键盘/AT 用户打开取色器后按 Tab 直接关闭面板，无法切换 Palette/HSLA、无法在 hex 输入框键入颜色、无法用键盘触发 Confirm/Close（只能鼠标点击）。这是 r1→r2 修复轮引入的键盘可达性回归，违反项目 a11y 目标（E-08/D8 同口径）。

**修法（建议，供 lead 裁决）**

1. 保留 popup 内 Tab 遍历：不绑定“Tab=关闭”，改为在边界离开（例如 Tab 动作先 `app.focus_next`，若焦点仍在 popup scope 则只移动；仅当焦点将要离开时 `close_now + focus_next`）；或 2) 若 lead 裁决“非 Trap popup 一律 Tab=关闭”为设计选择，则必须为内部控件提供其他键盘路径（如 ↑↓/←→ 切换页签、快捷键聚焦 hex/Confirm），并把该裁决登记为设计偏差（D-P3-x）与测试；
2. 增加“ColorPicker 内部 Tab 遍历/焦点顺序”测试，防止再次回归。
3. 严重度：**P2**（可访问性回归，用户可见）；若 lead 明确裁决为设计选择并补齐替代键盘路径，可降为 P3。

**r2 期间已出现并发候选修复（未提交，含测试）**：工作树快照（22:14）中 `crates/ui_kit/src/kit/color_picker.rs` 被并发 agent 改为 `tab_out` 先 `app.focus_next/prev`，再用 `descends_from(app, window, content, node)` 判断焦点是否仍在取色器内容内；仍在内容内则直接返回（不关闭），仅在焦点离开时 `close_now + 焦点落到触发器之后`；同时新增两条测试：`tab_walks_the_panel_controls_and_closes_only_at_the_last_one`（tab0→tab1→hex→confirm→最后控件 Tab 才关闭）、`shift_tab_walks_back_inside_the_panel_and_closes_at_the_first_control`，并新增 `mount_focusable_before` 助手。方向与本报告修法 1 完全一致，且覆盖了内部 Tab 遍历——**待 lead 提交 + 复跑后 R2-01 即可判 closed**；在 HEAD `00927ff9` 上仍为 P2。

### R2-02 [P3] Dropdown/Select typeahead 只绑定无修饰键，Shift+字母（大写输入）不触发

**位置/证据**

- `crates/ui_kit/src/kit/menu.rs:617-641`（`ItemPanel::bind_panel_keys`）：typeahead 的 `TYPEAHEAD_KEYS` 只以 `Modifiers::empty()` 做 `bind_key_scoped`；`typeahead` 的 label 比较用 `to_lowercase()`，本身支持大小写不敏感，但 **Shift+字母事件 modifiers 非空，查不到绑定**，直接落到其他路径（不触发 typeahead）。
- 对照 Menu 自己的 typeahead：`Menu::bind_extra_keys` 对字母同时绑定 `empty` 与 `SHIFT`（`menu.rs` P2 逻辑），行为一致；新 Dropdown/Select 用例（`typeahead_matches_labels_skips_disabled_and_cycles`、`typeahead_buffer_expires_after_the_timeout`）只用无修饰键 `key()`，未覆盖 Shift。
- 影响：用户实际输入首字母大写（常见）时 Dropdown/Select 的 typeahead 无效，与 Menu 不一致；不影响菜单/选择器其余行为。**P3**（新功能完整性/parity）。
- 修法：为 alphabetic 键补 `Modifiers::SHIFT` 绑定（与 Menu 对齐），并加 Shift 输入用例；或把 `TYPEAHEAD_KEYS` 抽一份共享绑定 helper。

### R2-03 [P3] 交付/证据快照口径滞后：文档写“最终 HEAD 5df859b8/55→106 checks”，但当前 HEAD 00927ff9 且其后仍有代码改动；历史 finding 行引用已删测试

**证据**

- 设计 §14/p3-summary/kit-gallery 现写“Windows 干净 HEAD `5df859b8` 106 checks/0 PASS”（例：`popup-overlay-redesign-design.md:864,1581,1600,1613,1627`；`p3-summary.md:3,26,55,63,77`；`kit-gallery.md:758,762,767`）。而 r2 期间 HEAD 已推进到 **`00927ff9`**（含 `5374c4c8` 对 `gallery.rs` real-pixel resize 断言的**代码改动**），即 evidence 的 `source_revision=5df859b8` 不覆盖当前 gallery 代码。
- 本 r2 已在 HEAD `00927ff9` **独立复跑** `--real-pixel-check`：106 checks / 0 failures / PASS / 39.7 s，证据 `E:\kytrix\.agents\worktrees\dsh-1d175414-1a41\target\real-pixels\r2-arch\report.txt`（`source_revision=00927ff9 status=clean diff_sha=e69de29b…`）。建议 lead 用该证据或复跑结果回写“最终 HEAD”，否则 DoD#6/证据口径停留在旧提交。
- 文档残留：`design:1272`（A-03 引用已删 `invalid_z_policy_open...`）、`design:1283`（A-14 引用已删 `escape_close_chain_policy_closes_the_whole_chain`）、`design:493/511`（关闭事务只写 `DismissPending`，未写 `Closing`）、`design:509`（“`Consume`/`Ignore` 为显式 opt-in”，Ignore 已非 OutsidePress 变体）。**P3**（文档-实现一致性；不影响运行）。
- 说明：文档在 r2 期间被并发回写（design mtime 22:04:17、p3-summary 22:03:00），以上为 r2 快照时仍存在项；若 lead 同轮补齐可关闭。

### R2-04 [P3] 工作树未清：`tools/source-id.txt` 未忽略/未删除；r2 期间另有并发未提交改动

**证据**

- `git status --short`（HEAD `00927ff9`，r2 结束快照 22:15）：`M crates/ui_core/src/popup/focus.rs`、`M crates/ui_core/tests/popup_behavior.rs`、`M crates/ui_kit/src/kit/color_picker.rs`、`M crates/profile_ui/src/profile.rs`、`M crates/profile_ui/src/menu.rs`、`M crates/profile_ui/src/test_support.rs`、`M tools/macos-gates-runner.sh`、`?? tools/source-id.txt`（并发改动仍在增长，属移动目标）。
- 并发未提交改动（均不在 HEAD 内）：`popup/focus.rs` 新增 `next_main_focusable`（跳过 popup 子树，修 Tab 跳出后的主树焦点选择）；`popup_behavior.rs` 修 `a_non_modal_take_popup_shift_tab_escapes_backwards` 期望；`color_picker.rs` 的 Tab 遍历修复 + 两条新测试（见 R2-01）；`profile_ui` 三个文件为宿主侧后续修复（r2 未复核内容）；`tools/macos-gates-runner.sh` 导出 `KYTRIX_REAL_PIXEL_EVIDENCE`。r2 结论不包含这些未提交改动。
- `?? tools/source-id.txt`：生成物（内容 `revision=00927ff9 status=clean diff_sha=e69de29b…`；`.gitignore` 未覆盖 `tools/`；`record-source-id.ps1` 生成）。
- 影响：最终“工作树干净 / 单一 HEAD”交付口径不成立；`source-id.txt` 会随每次 gate 重新生成，污染 `git status`。**P3**（交付卫生）。
- 修法：删除/忽略 `tools/source-id.txt`（或改指向 `$CARGO_TARGET_DIR`）；提交并发改动并复跑全部门禁后给出最终 HEAD；`git status --short` 必须为空。

---

## 4. 门禁实测（HEAD `00927ff9`，本机 Windows 11 / D3D12 WARP / 125% DPI，`CARGO_TARGET_DIR=E:\kytrix\target`）

| 命令 | 结果 |
|---|---|
| `cargo test -p ui_core -p ui_kit` | **PASS，3880 passed / 0 failed / 2 ignored**（ui_core lib 2802+1 ignored、frame_record 8、gui_smoke 0+1 ignored、layout 4、popup_behavior 124、snapshot 14、demo 39、ui_kit 889）。注：最后一次运行的工作树含并发未提交 focus 改动（见 R2-04），HEAD 单独的结果在 `5df859b8`→`00927ff9` 间仅 gallery/docs 变化，产品代码一致 |
| `cargo test --workspace --no-fail-fast` | **PASS（exit 0，无 FAILED/panic）**（r2 执行；运行期工作树含并发未提交改动，最终以 lead 提交后的全量复跑为准；`settings_ui` 修复后不再红） |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | PASS（exit 0） |
| `cargo +nightly fmt --all -- --check` | PASS（exit 0） |
| `cargo check --workspace --all-targets` | PASS（exit 0） |
| `gallery --pixel-check` | **PASS 94 checks**（32 sampled / open 8 / close 6+6 / `damage_full_frames=0` / first_visible==stable），exit 0 |
| `gallery --debug-stack` | **PASS，exit 0**：3 级链最终 entries=0、z_order=[]、retries=0、`event loop exited cleanly`；menu=(9,270.0,180.075)、picker_children=4。注：r2 早前两次在并发 gallery 窗口/被杀进程干扰下出现 stdout PASS 但进程 exit 0xffffffff（环境，非代码；隔离复跑即 0） |
| `gallery --real-pixel-check` | **PASS 106 checks / 0 failures / 39.7 s / frame 277**，exit 0（HEAD `00927ff9` clean，隔离证据 `target/real-pixels/r2-arch/`）。r2 早期并发桌面下的一次运行 9 failures 全部为“交互阶段截图 changed=0 + 残影检测自检失败”，属文档已登记的“并发 gallery/截图遮挡 ⇒ 假红”；隔离复跑证实代码 PASS |
| 旧 API `rg` | 旧 overlay/layer/AnimationRouter 模式仍全零；本轮删除的 `ZPolicy/MaxSize/OutsidePress::Ignore/EscapePolicy::CloseChain/RestoreFocus::RootFallback` 生产引用为零 |
| 工作树并发修复验证 | `cargo test -p ui_kit --lib`（含未提交 ColorPicker Tab 遍历修复+2 条新测试）= **889 passed / 0 failed**；`popup_behavior` 在含未提交引擎 focus 修复的工作树亦 124 passed（r2 期间），说明两份在途修复本身自洽并可提交 |
| `git status --short` | **非空**：`tools/source-id.txt` + 5 个并发未提交修复文件（见 R2-04）→ R2-04 |

---

## 5. 设计 DoD 对照（§10）

| DoD | r2 结论 | 依据 |
|---|---|---|
| 1 缺陷清零（残影/首帧/像素门禁） | **PASS** | `--pixel-check` 94/0（damage_full=0、首帧==稳定帧）；`--real-pixel-check` HEAD 106/0 PASS（隔离）；`--debug-stack` 3 级链/entries=0 PASS |
| 2 架构与旧 API 删除 | **PASS** | 独立 `rg` 旧 API 全零；本轮变体删除后 workspace check 绿；设计 §4.2 已回写删除 |
| 3 组件（D9 + 2–3 级） | **PASS（有 R2-01 保留）** | 组件行为测试/ui_kit 889 绿；3 级链真实窗口验收 PASS；ColorPicker 键盘可达性回归见 R2-01（P2） |
| 4 渲染不变量 I1–I9 | **PASS** | P0 render 三轮 + r1 复核 + 本轮 pixel/real-pixel 均通过；I7 modal 作用域/Closing 输入阻断测试绿 |
| 5 门禁 | **PASS（代码/测试/静态）** | ui_core+ui_kit 3880、workspace test exit 0、clippy/fmt/check 绿；真实像素证据见 DoD1 |
| 6 三端真实窗口验收 | **PASS（Windows 本机）；mac/Linux 仍待 acceptance 复跑回填** | Windows HEAD `00927ff9` 106/0 PASS（本 r2 独立复跑）；macOS/Ubuntu 文档仍标“待复跑回填”（r2 未在远端执行） |
| 7 文档回写/kit-gallery 同步 | **PASS（除 R2-03 列举的 5 处残留）** | design §4.2/§14、p3-summary、kit-gallery 已更新；残留见 R2-03 |

---

## 6. 观察项（不计数，供 lead 判 P4）

1. **引擎“Tab 跳出后主树焦点”指向 popup 内节点（并发未提交修复已针对）**：HEAD 中 `leave_owner_scope` 用 `focus::next_focusable(tree, anchor, …)`，若 popup 内还有可聚焦节点，Tab 跳出可能把焦点放回 popup 内容（`next_in_popup_scope` 曾循环）。r2 期间工作树未提交改动新增 `next_main_focusable`（`stack.enclosing(tree,node).is_none()` 过滤 popup 子树），方向正确；需 lead 提交 + 复跑后并入 HEAD，否则该边缘仍存在于 HEAD。
2. `PopupState::Closed` 对外不可观测（`App::popup_state` 返回 `None`）；kit `Some(Closed)` 分支为死匹配（P4 状态机清理）。
3. `Anchor::NodeRect`/`InitialFocus::FirstFocusable` 仍为“仅测试构造”的公开 API（设计 §4.2 已登记为 API/fallback；lead 的 NodeRect 证据是引擎实现 + 测试）；若按 AGENTS 死代码口径继续收紧，可删或补一个生产/宿主用例。
4. `--debug-stack`/`--real-pixel-check` 对并发桌面/窗口遮挡敏感（本次实测假红）；验收脚本与文档已登记该限制，建议 CI/lead 串行执行。
5. mac/Linux 真实像素仍需 acceptance 复跑回填（文档 §14.4 已如实标注）。

---

## 7. 计数与结论

| 严重度 | 数量 | 编号 |
|---|---|---|
| P0 | **0** | — |
| P1 | **0** | — |
| P2 | **1** | R2-01 ColorPicker 首个 Tab 即关闭，内部控件键盘不可达（修复轮引入的可访问性回归） |
| P3 | **3** | R2-02 Dropdown/Select typeahead 不识别 Shift+字母；R2-03 证据/文档快照仍以 `5df859b8` 为“最终 HEAD”且引用已删测试/未写 Closing；R2-04 工作树未清（`tools/source-id.txt` + 并发未提交 focus 改动） |

**关键 3 条**

1. **R2-01（P2）**：`ColorPicker::tab_out` 在所有面板内焦点上无条件 `close_now`（`color_picker.rs:1134-1150`、`bind_tab_keys:1152-1173`）；新增测试锁定“首个 Tab 关闭”。面板内至少两个页签、hex 可编辑输入、Confirm/Close 均 `tab_stop/editable` 可聚焦，但除 Tab 外没有其他键盘遍历，导致这些控件键盘不可达（修复前由引擎 `next_in_popup_scope` 顺序循环）。建议保留面板内 Tab 遍历、仅边界离开或提供替代键盘路径；补内部 Tab 顺序测试。
2. **R2-02（P3）**：`ItemPanel::bind_panel_keys` 的 typeahead 只绑 `Modifiers::empty()`（`menu.rs:617-641`），Shift+字母不触发；Menu 自身绑了 SHIFT 变体，Dropdown/Select 新功能与 Menu 不一致，且无 Shift 用例。
3. **R2-03/R2-04（P3）**：交付口径需收口——文档“最终 HEAD 5df859b8 / 106 checks”应更新为当前 HEAD `00927ff9` 或引用本 r2 的 106/0 隔离证据（`target/real-pixels/r2-arch/report.txt`）；并删除/忽略 `tools/source-id.txt`、提交并复跑并发未提交的 focus 改动，保证 `git status --short` 为空。

**是否可宣布完成：否（对 HEAD `00927ff9`）。** FA-01…FA-04 已确认关闭；但 **P2 = 1、P3 = 3**，未满足用户“P0–P3 全 0”的门槛。**有利条件**：r2 期间工作树已出现两份与 R2-01/引擎焦点边缘一致的并发修复并各自通过本地测试（ColorPicker Tab 遍历 + 2 条新测试、`next_main_focusable` 主树焦点），lead 若提交并全量复跑，预期 **R2-01 关闭（P2→0）**；之后只剩 R2-02（P3）/R2-03（P3）/R2-04（P3）。最小收口路径：
1. 提交并发修复（ColorPicker Tab 遍历 + 引擎 focus + mac runner evidence + profile 宿主），复跑 `cargo test -p ui_core -p ui_kit` / workspace / 真实像素；
2. 补 R2-02 SHIFT typeahead 绑定与用例（或裁决登记为 P4）；
3. 按 R2-03 更新文档/证据快照（§4.7 Closing 措辞、§12.7 A-03/A-14 证据行、“最终 HEAD”改为提交后的 HEAD 或引用 r2 隔离 106/0 证据）；
4. 清 R2-04（删除/忽略 `tools/source-id.txt`，提交后 `git status --short` 为空）；
5. 最后复跑：`cargo test -p ui_core -p ui_kit`、`cargo test --workspace --no-fail-fast`、clippy/fmt/check、`--pixel-check`、`--debug-stack`、`--real-pixel-check`（串行/独占桌面）。

---

## 附录：r2 证据

- 隔离真实像素：`E:\kytrix\.agents\worktrees\dsh-1d175414-1a41\target\real-pixels\r2-arch\report.txt`（`source_revision=00927ff9 status=clean checks=106 failures=0 result=PASS`）+ 17 张阶段 PNG
- FA-01 探针：`E:\kytrix\target\final-review-arch\probe-safe-triangle-r2.txt`（`background clicks delta = 0`；对照 r1 `probe-safe-triangle.txt` 的 delta=1）
- 门禁输出摘要：ui_core+ui_kit 3880/0/2、workspace exit 0、pixel 94、debug-stack exit 0、real-pixel 106/0
- 并发说明：r2 期间另有 gallery/测试进程与未提交 focus 改动；已隔离复跑关键门禁，避免把环境假红计入代码结论
