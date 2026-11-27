# 最终对抗性审查 C（产品交互 / 宿主真实使用 / a11y / 三端）— `final-review-interaction`

- 审查对象：worktree `E:\kytrix\.agents\worktrees\dsh-1d175414-1a41`；HEAD `fa41987c`（P3）、P0 `d9949aca`、P1 `152616b1`、P2 `f24a3441`；工作树干净（`git status --porcelain` 空）。
- 依据：设计 `.docs/popup-overlay-redesign-design.md`（§0 D5–D16、§3.4、§4.7–§4.12、§6 P0–P3、§7、§10、§14）、`implementation/` 各轮报告、`AGENTS.md`、宿主 crates（`settings_ui` / `profile_ui` / `market_participants_ui` / `instrument_selector_ui` / `updater_ui` / `kytrix` / `widget` / `widgets`）。
- 方法：只读审查 + 仓库外探针（`E:\kytrix\target\final-review-interaction\probe`，公开 API + `ui_core` `test-support`；输出 `probe-output.txt` / `settings-ui-test.log`）+ 宿主 crate 抽样测试 + 证据 `report.txt` 复核。所有 cargo 命令统一 `$env:CARGO_TARGET_DIR='E:\kytrix\target'`（探针自身 target 为 `target/final-review-interaction/probe-target`）。未改任何跟踪文件。
- 结论：**P0 = 0，P1 = 0，P2 = 2，P3 = 2 → 不能宣布 P0/P1/P2/P3 全 0。** P2 为 1 个引擎/kit 级键盘可达性缺陷（非模态 popup 的 Tab 被锁在 popup focus scope，列表行成为 tab stop，Enter 激活项与高亮不一致）与 1 个宿主级模态死路（设置“保存失败”报告的“确定”按钮无动作，鼠标用户无法关闭）。P3 为 1 个宿主测试确定性红（`settings_ui`）与 1 个设计/实施未登记缺口（Dropdown/Select typeahead）。已复核的 Menu/ContextMenu、Tooltip、Dialog/Alert 矩阵大部分成立；三端像素验收证据可用，但“真实交互”口径与文档证据快照字段需收敛（见 §4.2/§5，均 P4 级）。

---

## 1. Findings（P0–P3）

### FI-01 [P2] 非模态 popup 的 Tab 无法离开：被锁在 popup focus scope 内；列表行成为 tab stop；Enter 激活的行与高亮行不一致

**现象（仓库外探针 `target/final-review-interaction/probe-output.txt`）**

```text
DROPDOWN open: entries=1 focus=dd:panel:surface highlighted=Some(0)
  tab#0: focus=dd:panel:list:viewport entries=1 highlighted=Some(0)
  tab#1: focus=dd:panel:list:row:0 entries=1 highlighted=Some(0)
  tab#2: focus=dd:panel:list:row:1 entries=1 highlighted=Some(0)
  tab#3: focus=dd:panel:list:row:2 entries=1 highlighted=Some(0)
  tab#4: focus=dd:panel:list:scrollbar:track entries=1 highlighted=Some(0)
  tab#5: focus=dd:panel:surface entries=1 highlighted=Some(0)
  tab#6: focus=dd:panel:list:viewport entries=1 highlighted=Some(0)
  tab#7: focus=dd:panel:list:row:0 entries=1 highlighted=Some(0)
DROPDOWN mismatch: highlighted=Some(0) focused_row=NodeId(w1.n16.1) selected_after_enter=Some("gamma")
SELECT open: ...（与 Dropdown 相同的 6 个节点循环，entries 恒为 1）
  tab#0..7: focus=sel:panel:list:{viewport,row:0,row:1,row:2,scrollbar:track} / sel:panel:surface
SELECT after Tab x8 + Enter: chosen=["alpha"] ...
PICKER open: entries=1 focus=picker:panel.tab.0
  tab: tab.0 -> tab.1 -> hex.text -> confirm -> tab.0 ...（entries 恒为 1）
MENU open（对照）: entries=1 focus=menu:panel:surface
  tab#0: focus=menu-trigger entries=0   ← 菜单的 Tab 按设计关链并回 opener
```

- **非模态 popup（`FocusPolicy::Take`）内按 Tab/Shift+Tab，焦点只在 popup focus scope 内循环，popup 永不关闭，焦点永远回不到页面下一个控件**；`Dropdown`、`Select`、`ColorPicker` 均可复现。
- 循环目标包含**列表数据行**（`dd:panel:list:row:2` 等）与滚动条 track、viewport 等基础设施节点：焦点落到行时**高亮仍是 `Alpha`（row 0）**，按 Enter 却选中了 `Gamma`（row 2）——视觉上“当前项”与键盘激活项不一致；行上通常没有可见焦点环，用户只会看到 Enter 选了“错”的项。
- 对照：`Menu` 的 Tab 正常（`menu.rs` 的 `tab_out` 关链并把焦点移到 opener 之后），说明这不是统一的“桌面 popup 行为”，而是只有 Menu 做了 Tab 处理，其余非模态 popup 被引擎 focus scope 困住。

**代码根因（可复核）**

1. `crates/ui_core/src/app/focus.rs:230-268`：`focus_next`/`focus_prev` 只要存在 `focus_owner` 就调用 `popup_focus::next_in_popup_scope`，**不区分 `FocusPolicy::Trap`（模态）与 `FocusPolicy::Take`（非模态）**；设计 §4.9 只要求 `Trap` 在 modal scope 内 closed-loop。
2. `crates/ui_core/src/popup/focus.rs:249-290`：`next_in_popup_scope` 对候选集合做 `% len` 取模回绕，候选仅限 popup focus scope（`node_in_focus_scope`/`node_in_popup_scope`）；Take popup 的 scope 只有 popup 内容，因此 Tab 无限循环。
3. 组件取焦：`Dropdown::open`（`dropdown.rs:501-515`）、`Select::open`（`select.rs:761-775`）、`ColorPicker`（`color_picker.rs:1376-1383`）都是 `FocusPolicy::Take`；它们只把 ↑↓/Home/End 绑到 surface（`dropdown.rs:454-467`、`select.rs:714-729`、`ItemPanel::bind_panel_keys`），**没有 Tab/Shift+Tab 绑定**（只有 `Menu::register_key_actions`/`bind_extra_keys` 有，`menu.rs:2764-2774`、`2983-2991`）。
4. 行成为 tab stop：`ItemPanel` 复用 `List`，`List` 的 row 用 `NodeBuilder::boxed()` 默认 `tab_stop = true` 且挂 `on_click(row_action)`（`list.rs:1111-1146`；`node/builder.rs:69` 默认 `tab_stop: true`）；`ItemPanel` 只把行命中壳设成 `tab_stop(false)`（`menu.rs:262-279`），row 本身仍可聚焦。`ui_kit` 的 `row_hit_shells_are_not_in_the_tab_order` 只断言 `:hit:` 不出现在 Tab 序，未断言 Tab 会离开 popup，因此漏掉了该路径。
5. 宿主同型：`profile_ui` 账户菜单 `PopupSpec.focus = FocusPolicy::Take`（`profile.rs:640-660`），内容是 `Button`（`profile.rs` 的 `MenuContent`），无 Tab 绑定 → 打开账户菜单后 Tab 在 3–4 个按钮间循环，同样出不去；设置页的 AI site/model `Select`、`instrument_selector_ui::PredictionEventSelector` 的 `Select`、widget 内多处 `Select`/`ColorPicker` 都是生产可达路径。

**影响**

- 键盘用户（以及“点开下拉后按 Tab 去下一项”的鼠标用户）无法按标准路径离开非模态浮层；必须知道 Esc。`Select` 上还会出现“高亮 A、Enter 选 B”的错误激活。属于 WCAG 2.1.2（No Keyboard Trap）/ APG combobox-listbox Tab 语义问题。
- **不是 P1 新引入的机制，而是旧行为被保留（需按“此前未覆盖”登记）**：基线 `ItemPanel::sync(claim_focus)` 对 Dropdown/Select 每帧传 `true`（`git show 71a5a5cc:crates/ui_kit/src/kit/select.rs:782`、`dropdown.rs:517`），`claim_focus` 在焦点离开 overlay root 时会把焦点抢回 surface（`71a5a5cc:crates/ui_kit/src/kit/menu.rs:637-685`），所以旧版 Tab 也无法把焦点留到面板外（只是会“弹回”）。新引擎用 popup focus scope + `% len` 回绕实现同一效果，且额外把行/滚动条纳入 Tab 序。设计 §4.9 只写 `Trap` 的 closed-loop，未对 `Take` 作出裁决；本次按当前用户影响记 P2（若产品裁决“弹层打开期间焦点归弹层”成立，则至少行 tab stop/Enter 错位仍需修，并把该行为补登记）。
- 不是 P1：Esc 可退出、无数据损坏、无崩溃；但覆盖面广（三个 kit 组件 + 宿主账户菜单），故 P2。
- 备注：若产品裁决明确为“`Take` popup 在 Esc 前拥有焦点、Tab 不离开”，也必须（a）在设计与 §4.9 中登记该行为（当前只写 `Trap` 闭环），并（b）修掉列表行 tab stop/Enter 激活错位（这两项与“Tab 归属”裁决无关）；现状是设计未决策、实现按闭环走且行可聚焦。

**建议**

- 引擎侧：`focus_next`/`focus_prev` 对非 `Trap` 的 focus owner 不闭环——到达 scope 边界时离开 popup focus scope（触发 `DismissReason::FocusLost`/关闭并回主树）；`Trap` 保持闭环。
- kit 侧（更小改动）：给 `Dropdown`/`Select`/`ColorPicker` 绑 Tab/Shift+Tab（scoped 到 surface），行为与 `Menu::tab_out` 一致：关链、焦点回 opener 后 `focus_next`/`focus_prev`；同时把 `ItemPanel` 行设为 `tab_stop(false)`（列表项不进入 Tab 序，active descendant 仍由方向键更新）。
- 宿主：`profile_ui` 自定义菜单至少复用同一套 Tab 关链；或改用 kit `Menu`/`MenuChain`。
- 测试：`Dropdown`/`Select`/`ColorPicker`/profile 菜单各加“Tab 后 popup 关闭且焦点到下一控件”“Tab 到 row 后 Enter 不得激活非高亮项”两条；按 `Menu` 的 tab 测试对照。

### FI-02 [P2] 设置“保存失败”报告的“确定”按钮无动作：模态只能 Esc 关闭（鼠标用户被卡死）

**现象/代码**

- `crates/settings_ui/src/settings.rs:535-546` 的 `report_write_failure` 构造：

  ```rust
  let dialog = DialogBuilder::alert(title.clone(), move |_dialog, _theme| {
      AlertBuilder::new()
          .ok(title.clone(), message.clone())
          .ok_text(confirm.clone())
  })
  ```

  **没有 `.ok_action(...)`/`.cancel_action(...)`**。`DialogBuilder::alert` 默认 `header=false`、`outside=false`、`escape=true`，且 **AlertDialog 永不建右上角关闭按钮**（`dialog.rs:250-259`、`699` 的 `close_button && role == Dialog`）。`AlertBuilder` 的 OK 按钮只在 `ok_action` 存在时执行动作（`alert.rs:299-320`），`Alert` 本身不关闭宿主 `Dialog`。
- 仓库外探针（`probe-output.txt`，构造与设置相同的 alert；用 `ok_action` 计数验证点击确实到达回调）：

  ```text
  ALERT OK click reached callback: clicked=1 dialog_still_open=true entries=2
  ALERT without ok_action (settings shape): dialog_open=true entries=2
  ALERT without ok_action: after Escape dialog_open=false entries=0
  ```

  点击到达回调但 Dialog 仍开（Alert 不自关）；设置路径没有回调 ⇒ 点“确定”**完全无效果**；只有 Esc（经 90 ms 关闭动画）能关。
- 可达性：任何 `SettingsHost::write` 失败都会在 `store.failure` 记录（`session.rs:182-189`），下一次 `Settings::sync` 调 `report_write_failure` 打开该 alert（`settings.rs:521-547/568-569`）。设置页大量写入用 `let _ = host.write(...)`；权限/磁盘/持久化失败是真实错误路径。
- 后果：模态 alert 阻断整个设置页；**没有关闭按钮、点外部不关、OK 无动作**，唯一退出方式是 Esc（或关整个窗口）。鼠标用户（或不熟悉 Esc 的用户）会被卡在“保存失败”弹窗；多次失败会在 `alerts: Vec<Dialog>` 中叠加多个不可关闭的模态层，进一步锁死。
- 现有测试未覆盖该 alert：`settings_tests/` 的写入失败测试都通过页内 alert（有 `ok_action`）覆盖；全局 `report_write_failure` 无测试（`settings_ui` 测试套件见 FI-03，235/236 通过）。

**影响**：用户可见的模态死路（错误路径，非主路径；Esc 可恢复），P2。若要求“鼠标单独可完成”，实际是不可恢复的（只能关窗口），因此也可按 P1 评估；本审查按“Esc 可用 + 错误路径”取 P2。

**建议**：在 `report_write_failure` 里接 `ok_action`（参考 `market_participants_ui/src/menu.rs:552-563` / `profile_ui/src/dialogs.rs:52-90` / `widgets/x/src/dialog.rs:1309-1317`：`ok_action(move |_, app| dialog.close(app))`），并补一条“全局写入失败 → alert 打开 → 鼠标点 OK → alert 关闭、焦点回设置页”的宿主测试；也可考虑给 `AlertBuilder` 文档/调试断言（“`DialogBuilder::alert` 的 OK 默认不会关闭宿主 Dialog”），避免同类宿主误用。

### FI-03 [P3，宿主测试红；若宿主套件纳入门禁则升 P2] `settings_ui` 确定性失败：`a_failed_write_should_keep_the_dialog_and_what_was_typed`

**证据**

```text
$env:CARGO_TARGET_DIR='E:\kytrix\target'; cargo test -p settings_ui --lib
thread 'settings_tests::credentials::a_failed_write_should_keep_the_dialog_and_what_was_typed' panicked at
crates\settings_ui\src\settings_tests\credentials.rs:336:5: the retry should take the dialog away
test result: FAILED. 235 passed; 1 failed; 0 ignored
```

连续复跑确定性失败（非并发/时钟抖动）；`target/final-review-interaction/settings-ui-test.log` 保留日志。

**原因（代码 + P1 diff 对照）**

- 失败测试在 `credentials.rs:303-353`：失败写入 → alert 打开 → **`harness.press(Escape)` + `harness.settle(3)`**（第 332-334 行）后立刻 `confirm_dialog`。`settle` 只 drain 帧、不推进测试时钟（`test_support.rs:359-363`）；对话框关闭动画是 90 ms（`POPUP_CLOSE_DURATION`，且 `Dialog` 用 `FadeScale`），alert 在 Closing 期间仍是 `is_visible` 的 modal（`popup.rs:87-90`），按设计/引擎测试“closing modal keeps lower layers inert until removed”，下一次 Confirm 点击被模态层吞掉 ⇒ 对话框不关。
- 同文件相邻 `a_failed_clear_should_keep_the_set_until_the_retry_writes_it` 在 P1 commit `152616b1` 中被**专门补了 `harness.advance(200)`**（该 commit 对 `credentials.rs` 的唯一改动，diff 位置在 clear 测试第 379-382 行），但 write 测试没有补。即：P1 迁移引入 90 ms 关闭动画后，这个宿主测试从 P1 起就是红的，只是正式门禁未跑 `settings_ui`。
- 产品行为本身大概率正确（等待关闭动画后重试应写入并关闭）；但**测试失败使该重试路径失去验证**，且工作区 `cargo test`（AGENTS §8 的验收项）不绿。

**建议**：在 `credentials.rs:333` 的 `settle(3)` 后补 `harness.advance(200)`（镜像相邻测试）；复跑 `cargo test -p settings_ui`；若仍红，则升级为宿主产品缺陷（Closing modal 的输入吞掉时间窗/焦点恢复）另查。

### FI-04 [P3] Dropdown/Select 的 typeahead 未实现且未登记（设计 §7.3 / P1 §12.4 推到 P2-4，P2-4 只做了 Menu）

- 设计 §7.3 明确列出组件测试范围：`Dropdown/Select：位置/match width/键盘/typeahead/多选/点击回填/外部关闭/滚动跟随/Closing toggle`（design `:855`）；P1 结果又写“typeahead/搜索未实现，属 P2-4”（design `:1091`）；P2-4 的落地清单只描述 Menu 的 36 键 typeahead/2s 单层 buffer（`p2-summary.md:23`），§14.7 的 P4/未闭环列表没有登记该缺口。
- 代码事实：`ui_kit/src/kit/dropdown.rs`、`select.rs` 没有 `typeahead` 相关代码/选项；`TYPEAHEAD_KEYS`、typeahead buffer 只在 `menu.rs`/`menu_chain.rs`（`menu.rs:61`、`menu_chain.rs:15/641`）。仓库外探针：`DROPDOWN typeahead: before=Some(0) after=Some(0)`、`SELECT typeahead: before=Some(0) after=Some(0)`（按 `b` 不移动高亮）。
- 影响：长列表键盘用户只能逐项方向键；设计测试计划项缺失；DoD 3（“D9 全部组件达到 §4/§7”）当前按“已达成”回写，与实际不完全一致。无回归（P1 之前同样没有），故 P3。
- 建议：二选一——(a) 按 Menu 的语义给 Select/Dropdown 实现 2s buffer、前缀匹配、循环、跳过 disabled/separator、换层清空（dropdown 无层级，单层即可）；(b) 明确用户裁决为 P4，写入 §14.7/§10 偏差表并更新 §7.3 的测试计划。不能既声称 DoD 3 达成又不登记。

---

## 2. 交互矩阵核对（逐项）

| 区域 | 检查项 | 结果 | 依据 |
|---|---|---|---|
| Menu | hover 级联 / 200ms 延迟 / 兄弟替换 / 3 级 | OK | `menu.rs` tests（`a_three_level_hover_cascade...` 等）；`menu_chain.rs` 13 tests |
| Menu | safe triangle / grace 300ms / 镜像 flip / 外部 Ignore | OK | `menu_chain.rs` SafeTriangle/`derived_safe_triangle` tests；P2 审查记录 |
| Menu | ↑↓/Home/End/PgUp/PgDn/←→/Enter/Space/Esc/Tab/Shift+Tab | OK（Menu 自身） | `menu.rs` 键盘测试；探针对照 Tab 关链 |
| Menu | typeahead（2s/单层/跳过禁用） | OK（仅 Menu） | `menu.rs` `TYPEAHEAD_KEYS`/`typeahead_*` tests；Dropdown/Select 见 FI-04 |
| Menu | checkable（Checkbox/Radio/Space/Enter） | OK | `menu.rs` tests；a11y role/checked |
| Menu | disabled/group/separator；2–3 级；z 序 | OK | `the_chain_assigns...`、`a_three_level_a11y_chain...` 等 |
| Menu | outside Replay / Esc 逐层 / Alt+F10 / Shift+F10 / 右键 | Menu/ContextMenu 键盘 OK；右键未内建（D-P2-4，P4 已登记） | `bind_trigger` 仅 Alt+F10/Shift+F10+ContextMenu；gallery 用按钮演示右键语义 |
| Menu | 焦点夺回 / closing toggle / anchor 滚动关闭 | OK | P2 tests；`sync_anchor` 一律关链（D-P2-3） |
| Menu | 多显示器 / 真实窗口 hover/斜向 | 未覆盖（P4，见 §5） | 三端/真实窗口门禁只程序化 open/close |
| Dropdown/Select | 定位 / match width / 长列表虚拟化 / 滚动跟随 | OK | 组件 tests；P3 kit 复核 |
| Dropdown/Select | outside 关链 + Replay、Esc 回焦、Closing toggle | OK | 组件 tests |
| Dropdown/Select | 键盘（方向/Home/End/Enter/Space/Esc） | 部分 OK；**Tab 不可离开、行成 tab stop、Enter 可激活非高亮项（FI-01）**；typeahead 缺失（FI-04） | 探针 |
| Dropdown/Select | flip | 引擎 placement 有 flip 单测；组件层无 flip 集成测试（覆盖观察） | `placement.rs` tests；组件无对应断言 |
| Dropdown/Select | a11y（button + haspopup + expanded；List/ListBoxOption） | 基本 OK；Tab 进入行与 active descendant 不一致（FI-01） | `dropdown.rs:1755+`、`select.rs:1881+` tests；探针 |
| ColorPicker | open/close、局部 Canvas、tab 重测、hex、外部/Esc、锚点丢失 | OK | 组件 tests；P3 复核 |
| ColorPicker | **Tab** | 4 个可聚焦节点循环、popup 不关（FI-01） | 探针 |
| Tooltip | 500/100ms、Hint 穿透、不抢焦、Esc 只关自身、hover 区域含自身、modal 隐藏 | OK | `tooltip.rs` tests；`popup_behavior` hint tests |
| Tooltip | described_by / a11y | OK（引擎投影） | `popup_behavior`/`ui_core::a11y::popup_tests` |
| Dialog | 模态阻断/Scrim/Trap Tab/初焦默认按钮/Esc/outside/关闭动画/叠放/嵌套/IME | OK（kit/引擎 tests；R2-04 自建 footer 初焦为已登记 P4） | `dialog.rs` 55 tests；`popup_behavior` |
| Alert | 作为 Dialog 内容面板、叠放、Esc、按钮动作 | OK；**宿主 settings 的 OK 无动作（FI-02）** | `alert.rs`/`dialog.rs` tests；settings.rs 代码 |
| 宿主 | settings Select/Dialog/Alert 的 sync/release/焦点 | `sync` 大体齐全；**保存失败 alert 死路（FI-02）**；测试红（FI-03） | 代码审查 + 测试 |
| 宿主 | profile 账户菜单 | Take 取焦后 **Tab 被锁（FI-01）**；role=Menu 但内容为 Button、无箭头导航（P4 观察） | `profile.rs:640-660`、`profile_ui/src/menu.rs` |
| 宿主 | market participants 面板 | Modal+Trap（Tab 闭环符合模态）、初始 Container、alert 有 ok_action | 代码审查 + 22 tests |
| 宿主 | instrument selector / updater / widgets | 弹层 sync/release 正常；host tests 通过 | 见 §4 |
| a11y | 菜单链嵌套父、expanded/haspopup、active_descendant | OK | kit/引擎 tests |
| 三端 | DPR/截图/残影 | 证据 55 checks PASS（见 §5）；真实输入路径未覆盖 | report.txt |

---

## 3. 宿主抽查结果

### 3.1 抽样测试（本机 Windows 11 / D3D12 / 125% DPI）

| crate | 命令 | 结果 |
|---|---|---|
| `updater_ui` | `cargo test -p updater_ui` | 72 passed / 0 failed；`tests/fetch` 1 passed |
| `profile_ui` | `cargo test -p profile_ui` | 87 passed / 0 failed |
| `market_participants_ui` | `cargo test -p market_participants_ui` | 22 passed / 0 failed |
| `instrument_selector_ui` | `cargo test -p instrument_selector_ui` | 89 passed / 0 failed |
| `settings_ui` | `cargo test -p settings_ui --lib` | **235 passed / 1 failed（FI-03）** |
| `widget`（框架契约） | `cargo test -p widget` | 182 passed / 0 failed |
| `kytrix`（shell bins） | `cargo test -p kytrix --bins` | 80 passed / 0 failed |
| gallery（headless 像素门禁） | `cargo run -q -p ui_kit --example gallery -- --pixel-check` | **PASS 94 checks**（sampled 32 / open 8 / close 6+6 / `damage_full_frames=0`；first_visible == stable = (496,91,300×375)；exit 0） |

> 说明：以上不属于用户设定的正式门禁（正式门禁仍为 `ui_core`/`ui_kit`），仅用于“宿主真实路径”抽查自证；`settings_ui` 的 1 条红是本次审查发现的新增事实。`--pixel-check` 的组件覆盖仍以 ColorPicker 为主（E-12 / P3K-08 → P4），不代表 Dropdown/Select/Dialog/Tooltip 的残影已被门禁覆盖。

### 3.2 宿主代码审查要点（sync/release/焦点/错误处理）

- `settings_ui`：页面/对话框/select 的 `sync`/`dispatch` 基本齐全（`settings.rs:563-611`、`pages/translation/page.rs:462-581`、`pages/venues/*`、`pages/proxy/editor.rs:520-581`）；`Dialog` 自定义 footer 通过句柄关闭；`alerts` 在关闭后 `release`（`settings.rs:549-561`）。**`report_write_failure` 的 alert 缺 `ok_action`（FI-02）**；`take_failure` 只在 `Settings::sync` 消费，语义一致。
- `profile_ui`：菜单是自定义 `PopupPanel`（非 kit `Menu`），用 `Take` 取焦（`profile.rs:640-660`）；`sync_menu` 处理 dismissed/重开/内容重建，`alert` 有 ok/cancel action（`dialogs.rs`）。键盘缺口即 FI-01；`address_hint` tooltip 的 trigger 是普通文本节点（非可聚焦），AT 只能读到缩短地址，完整地址仅 tooltip 文本（P4 观察）。
- `market_participants_ui`：面板为 `PopupTier::Modal + Trap + InitialFocus::Container`（`menu.rs:291-311`），模态内 Tab 闭环符合设计；alert 有 `ok_action` 关闭（`menu.rs:552-563`）；`sync` 顺序（panel/tooltip/alert）合理；22 tests 覆盖打开/关闭/失败/替换。
- `instrument_selector_ui`：`Dialog` ok/cancel 回写通过 `Dialog::confirm_button`/自定义 footer；`PredictionEventSelector` 的 `Select` 有 `sync`（`prediction_selector.rs:1335`）但无 `dispatch`（只影响 trigger pressed 视觉，不影响功能）；dialog 栈/焦点恢复有测试（`instrument_selector/tests.rs` 多 dialog 用例）。
- `updater_ui`：`DialogBuilder::new` + 自定义 footer，控制映射经 `DialogControl`（`updater.rs:818-846`）；`sync` 在每帧调用（`updater.rs:769/791`）；`close_dialog` 有明确关闭路径。
- `kytrix` shell：`Dialogs::sync` 每帧 sync/关闭后 release（`shell/dialogs.rs:212-229`）；`name_alert` 使用 `AlertBuilder::input_ok_cancel` + ok/cancel action（同名文件 175-195），无 FI-02 类问题。
- 结论：没有发现“编译通过但 sync 缺失导致关闭动画/焦点不推进”的宿主缺陷（除 FI-02 的 OK 动作缺失）；宿主对最终 API 的语义使用整体一致。

---

## 4. 三端差异与验收口径复核

### 4.1 证据读取（只读 `target/real-pixels/`，未重跑真实窗口门禁）

| 平台 | report | checks | DPR/内容 | 证据快照字段 |
|---|---|---|---|---|
| Windows 11 | `target/real-pixels/windows/report.txt` | 55 / 0 PASS（elapsed 15871 ms、frame 94） | 1.25 / 1200×900 | `source_revision=fa41987c`、`source_status=clean`、`source_identity=git`、`diff_sha=e69de29b…`（空树哈希） |
| macOS | `target/real-pixels/macos-remote/report.txt` | 55 / 0 PASS（31065 ms、frame 97） | 2.0 / 1914×1437（容差 8） | `source_revision=f24a3441`、`dirty(30 files)`、`source_identity=source-id-file`、`diff_sha=0a9a218b…` |
| Ubuntu（nested sway） | `target/real-pixels/linux-remote/report.txt` | 55 / 0 PASS（8082 ms、frame 94） | 1.0 / 1276×670（容差 4） | 同 macOS（`f24a3441` dirty / `source-id-file` / `0a9a218b…`） |

### 4.2 结论与限制

- **三端像素/残影验收成立**：55 条命名断言（marker/content size/thickness/on-screen、baseline-stable、self-check-offset/recolor、open/close 各阶段、scrim theme/composite/alpha、scrim/panel fault）均为 PASS；三端 scrim alpha≈0.55 与设计一致；DPR/内容尺寸通过标记框反解正确；macOS TCC / Linux nested sway 的限制与文档一致（`screencapture` 需 Terminal、linux 需 Wayland+grim、nested compositor 改尺寸）。
- **“真实交互”未被三端验收覆盖**：`--real-pixel-check` 与 `--debug-stack` 的 3 级链均由程序化 `chain_menu.open(...)` + `open_submenu(...)` 驱动（`gallery.rs:434-438`、`5304-5353`），不是 hover/斜向/sibling/Esc/外部点击/键盘/滚动输入；三端也没有运行 `settings`/`profile`/`updater` 等宿主窗口。故 P2 §6 验收文字中的“3 级在真实窗口完成 hover/斜向/sibling/Esc/外部点击/键盘/滚动全路径”只有 headless 单测证据，没有真实窗口证据；DoD 6 的字面（真实窗口截图/残影）已满足，不应据此否定三端结论，但验收口径需要收敛（P4，见 §5）。
- **证据快照字段不一致（P4）**：Windows report 当前是提交后 `fa41987c clean`（我审查期间 19:10 被并发真实窗口运行刷新），而设计 §14.4（`:1614`）写“三端 report 一致 = `f24a3441` dirty(30) + `0a9a218b…` + source-id-file”。Windows 的提交后干净树证据其实更强，但文档（以及 `p3-summary §5`）需要按最终 report 重写；否则“三端一致”的陈述不成立。
- **平台输入差异**：三端平台层均映射 `KeyCode::ContextMenu`（Windows 0x5D、Linux 139/127、macOS 0x6E）与 F10；但 `Menu::bind_trigger` 只绑 Alt+F10、ContextMenu 只绑 Shift+F10/ContextMenu 键，**没有真实三端输入注入验证**（真实窗口门禁不注入输入）；macOS/Windows 对 `Alt+F10` 的窗口管理歧义未验证（P4）。
- **验收质量观察**：`--real-pixel-check` 需独占桌面；本次审查只读证据，未与可能并发的验收进程争抢窗口。`damage_full` 在真实窗口报 `unavailable`（headless 门禁承担），符合设计。

---

## 5. 残余风险（P4，不计入 P0–P3；建议登记/收敛）

1. **真实窗口交互路径未验收**（上 §4.2）：真实窗口门禁只验证程序化 open/close 的几何/像素/栈，不验证 hover cascade、斜向 safe triangle、Esc 逐层、外部 Replay、键盘、滚动、宿主窗口。建议在验收文档把“真实交互”与“像素/栈”分开表述，或补一个 `--interaction-check`（可注入指针/键盘的 gallery 场景）。
2. **三端证据快照文字与当前 report 不一致**（§4.2）：需把 §14.4 / p3-summary 的 `source_*` 描述改成最终实际值（Windows clean fa41987c；macOS/Linux dirty f24a3441 + 0a9a…）。
3. **宿主 popover 的 a11y/键盘语义**：`profile_ui` 账户菜单和 `market_participants_ui` 面板都声明 `A11yRole::Menu` 但内容是普通 `Button`/控件、没有 APG 菜单的 ↑↓/active-descendant 模型（profile 是 `Take` 非模态；market 是 Modal）；`profile_ui` 的地址 tooltip 挂在非可聚焦文本上，AT 无法通过 focus 触发完整地址；`Alert` 的校验错误文本没有 `live` 语义（屏幕阅读器不一定播报）。旧实现已有部分同型问题（role 来自旧 overlay），列为 P4 观察。
4. **Alert API 的宿主陷阱**：`AlertBuilder::ok` 不传 `ok_action` 时“确定”静默无动作，且 `DialogBuilder::alert` 无关闭按钮/outside；建议在 API 文档/调试断言中写明“Alert 不关闭宿主 Dialog，必须由宿主在动作里关闭”，避免 FI-02 类问题再次出现（P4 文档/API 防护）。
5. **已登记 P4 项复核**：D-P2-4 右键自动绑定、D-P2-5 typeahead 不消费 text/IME、R2-04 自建 footer 初焦回退 Cancel、R2-01 首帧内部滚动高度、R2-02 非锚点 reason、E-12 测试真实性、P3K-07 scale 不参与 hit/a11y/IME、P3K-08 debug-stack/pixel 不采样动画帧、ContextMenu 多显示器、macOS TCC 一键化 — 均仍在 §14.7 列表内，本次未发现被遗漏的新 P4 之外问题；其中 FI-04（Dropdown/Select typeahead）若被裁决为 P4，应补登记到该列表。

---

## 6. 复现命令与探针

```powershell
$env:CARGO_TARGET_DIR='E:\kytrix\target'

# 1) 仓库外探针（公开 API；Tab / typeahead / Alert 行为）
$env:CARGO_TARGET_DIR='E:\kytrix\target\final-review-interaction\probe-target'
cargo run --manifest-path E:\kytrix\target\final-review-interaction\probe\Cargo.toml
# 输出：E:\kytrix\target\final-review-interaction\probe-output.txt

# 2) 宿主抽样测试
cargo test -p settings_ui --lib
cargo test -p updater_ui -p profile_ui -p market_participants_ui -p instrument_selector_ui
cargo test -p widget
cargo test -p kytrix --bins

# 3) headless 像素门禁（自证；不占用真实桌面）
cargo run -q -p ui_kit --example gallery -- --pixel-check
```

- 探针源码：`E:\kytrix\target\final-review-interaction\probe\src\main.rs`（只读依赖 worktree 源码，不写仓库）。
- `settings_ui` 失败日志：`E:\kytrix\target\final-review-interaction\settings-ui-test.log`。
- 审查结束时 `git status --porcelain` 为空（`target/` 与 `.docs/` 均被 ignore）。

---

## 7. 结论

| 严重度 | 数量 | 明细 |
|---|---|---|
| P0 | 0 | — |
| P1 | 0 | — |
| P2 | **2** | FI-01 非模态 popup Tab 键盘陷阱/行激活错位；FI-02 设置保存失败 Alert 的“确定”无动作（鼠标模态死路） |
| P3 | **2** | FI-03 `settings_ui` 确定性测试红（test-only，一行 advance 可修）；FI-04 Dropdown/Select typeahead 未实现且未登记 |
| P4 | 5 项（§5） | 真实窗口交互覆盖、证据快照文字、宿主 popover a11y、Alert API 防护、已登记 P4 复核 |

**能否宣布完成（P0/P1/P2/P3 全 0）：不能。** 至少需处理 FI-01、FI-02（P2）；FI-03、FI-04（P3）若按用户“全 0”口径也需修复或经用户明确裁决降为 P4 并回写设计 §14.7。P4 项不阻塞，但三端证据/验收口径建议在最终提交时收敛。

已复核成立的强项（供 lead 参考，不重复展开）：MenuChain/safe triangle/grace/2–3 级键盘与 a11y 链、Tooltip Hint 语义、Dialog Trap/Scrim/初焦/Esc/叠放、引擎 popup settle/damage/pixel 门禁、三端 55 checks 像素证据、宿主 sync/release 的总体一致性。


