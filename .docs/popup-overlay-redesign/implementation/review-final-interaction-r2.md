# 最终对抗性审查 C（交互/宿主/a11y/三端）r2 — `review-final-interaction-r2`

- 审查对象：worktree `E:\kytrix\.agents\worktrees\dsh-1d175414-1a41`；审查目标为修复提交 **`5df859b8`**（基线 `fa41987c`）；审查期间 HEAD 先后前移到 `5374c4c8`（gallery `--real-pixel-check` resize 断言平台无关）与 **`00927ff9`**（kit-gallery 文档回写）——两者不改引擎/kit 行为，r2 结论仍针对 `5df859b8` 的交互修复。只读，未改跟踪文件。
- 方法：`git show 5df859b8` 逐文件读修复 diff + 引擎/kit 新测试审阅 + 仓库外探针（`E:\kytrix\target\final-review-interaction\r2-probe`，公开 API + `ui_core test-support`；输出 `r2-probe-output.txt`）+ 宿主抽样测试 + 真实像素证据只读复核。
- 测试环境：`$env:CARGO_TARGET_DIR='E:\kytrix\target'`；审查期间桌面上有并发 `gallery` 进程（21:43/21:48/21:56/21:58 多次启动），导致 `cargo test -p ui_core -p ui_kit` 的 example（`gallery.exe`）链接出现 `LNK1104: cannot open file ... gallery.exe`；改用 `--lib --tests` 跑完全部库/集成测试（3880 通过），失败为并发文件锁而非代码错误（见 §4）。

## 0. r2 结论（先给计数）

| 严重度 | 数量 | 明细 |
|---|---|---|
| P0 | 0 | — |
| P1 | 0 | — |
| P2 | **1** | **R2-01**：ColorPicker 的 Tab 现在从任意焦点节点立即关闭面板（含 hex/confirm 内部控件），键盘用户失去进入 hex/确认的路径（修复引入的 a11y/键盘回归） |
| P3 | **1** | **R2-02**：`profile_ui` 账户菜单（自定义 `Take` popup）Tab 后焦点离开但菜单不关闭（FI-01 的宿主残余） |
| P4 | 4 组 | gallery Alert 演示 OK/Cancel 不关闭 Dialog；三端 106 交互验收仅 Windows；设计/证据文档未回写最终修复；`tools/source-id.txt` 未跟踪且 Windows 106 report 为 pre-commit dirty 快照 |

**FI-01～FI-04 主体关闭**：引擎/kit 侧 Tab、行 tab_stop、typeahead、Alert 关闭、safe triangle Consume 均有代码+测试+探针证据（§1/§3）。但严格按“交互/a11y 缺陷全 0”口径，**R2-01（P2）与 R2-02（P3）仍需处理或经用户裁决降为 P4**；除此之外未发现 P0–P2 回归。若用户明确接受“ColorPicker Tab 即关闭（内部控件改为仅鼠标/AT）”并把 profile 菜单 Tab 不关登记为 P4，则 P0–P3 可宣布全 0。

---

## 1. FI-01…FI-04 关闭核对

### FI-01 [原 P2] 非模态 popup Tab 闭环 / 行 tab_stop / Enter 高亮错位 — **主体关闭（引擎+kit），宿主 profile 残余**

**代码（`5df859b8`）**
- 引擎 `crates/ui_core/src/app/focus.rs:229-281`：`focus_next/prev` 现在区分 `topmost_modal`（Trap → `next_in_popup_scope`，闭环）与仅有 `focus_owner`（Take/None → `next_in_owner_scope`，到边界 `leave_owner_scope` 移到主树；不再闭环）。
- `crates/ui_core/src/popup/focus.rs:291-338`：新增 `next_in_owner_scope`/`leave_owner_scope`（清 owner + `apply_effective_focus`）；引擎测试 `a_non_modal_take_popup_tabs_out_to_the_main_tree_instead_of_wrapping`、`..._shift_tab_escapes_backwards`（`tests/popup_behavior.rs:6161-6257`）断言非模态 popup Tab 后焦点到主树、**且 popup 仍 Open**（引擎对通用 Take popup 的语义）。
- `crates/ui_kit/src/kit/list.rs:1117-1123` + `menu.rs:291-320`：`List::set_rows_tab_stop(false)`，`ItemPanel` 行移出 Tab 序（`rows_tab_stop=false`）；Menu 的 `ItemPanel` 也套用，但菜单行不进入 Tab 序（菜单有自己的键盘表）。
- `crates/ui_kit/src/kit/menu.rs` 的 `ItemPanel` 新增 `tab_next/tab_prev` actions 与 `bind_panel_keys` 绑定（Dropdown/Select 用）；`Dropdown`/`Select` 的 `ItemPanelOptions.typeahead = true`；`tab_out` 关面板、焦点回 opener 后 `focus_next/prev`。
- `ColorPicker`：`bind_tab_keys` 绑 Tab/Shift+Tab 到 content（`color_picker.rs:1152-1172`），`tab_out` 关闭面板后焦点回 trigger 再前后移（`:1134-1150`）。
- 新测试：`dropdown::tab_closes_the_dropdown_and_moves_focus_past_the_trigger`、`shift_tab...`、`enter_chooses_the_highlighted_row_and_rows_leave_the_tab_order`；`select`/`color_picker` 同型（ui_kit 889 tests 全绿）。

**r2 探针（`r2-probe-output.txt`）**

```text
DROPDOWN open: entries=1 focus=dd:panel:surface highlighted=Some(0)
DROPDOWN Tab: open=false entries=0 focus=after-dd ...
SELECT Shift+Tab: open=false entries=0 focus=after-sel ...      # 探针节点顺序下“上一可聚焦”落在 after；重点是 entries=0
SELECT Tab: open=false entries=0 focus=before-sel ...
PICKER Tab: open=false entries=0 focus=after-picker ...
MENU Tab: entries=0 focus=menu-trigger ...
DROPDOWN closed typeahead key ignored: highlighted=None open=false
```

**结论**：引擎 Take 的 Tab 逃逸、kit Dropdown/Select/ColorPicker 的关闭+焦点、行 tab_stop、Enter 高亮激活均已按修复目标落地；`ui_core` 124 popup_behavior 与 `ui_kit` 889 全绿。
**残余**：`profile_ui` 账户菜单是唯一未改的宿主 `Take` popup（`profile.rs:632-660`），无 Tab 绑定；引擎新语义（Tab 离开、popup 保持 Open）导致**菜单留在页面上**。见 R2-02。

### FI-02 [原 P2] settings 保存失败 Alert 的“确定”无动作 — **已关闭**

- `settings.rs:532-546`：`DialogBuilder::alert` 的闭包改为使用 `dialog` 句柄并加 `.ok_action(move |_, app| { dialog.close(app); })`；`gallery`/其它 Alert 调用点未影响。
- 新宿主测试 `settings_tests/shell.rs::a_write_that_cannot_be_stored_should_be_reported_over_the_page`：断言 alert 打开 → `click_alert(ok)` → `!alert_is_open`、标题节点消失、`popup_total()==0`。
- `cargo test -p settings_ui --lib`：**236 passed / 0 failed**；定向两条（credentials 失败用例 + shell 报告用例）均 PASS。
- 探针对照：settings 形状 alert（有/无 `ok_action`）——有 action 时点击后 `open=false entries=0`；无 action 时 `dialog_open=true entries=2`（证明修复的是宿主缺 action，Alert 本身不自关的 API 语义未变）。

### FI-03 [原 P3] settings credentials 失败测试红 — **已关闭**

- `settings_tests/credentials.rs:331-334` 在 Escape + `settle(3)` 后补 `harness.advance(200)`（与相邻 clear 用例一致）。
- `cargo test -p settings_ui --lib` 236/0；定向 `settings_tests::credentials::a_failed_write_should_keep_the_dialog_and_what_was_typed` PASS。

### FI-04 [原 P3] Dropdown/Select typeahead 缺失 — **已关闭**

- `ItemPanel` 共享 typeahead：`TYPEAHEAD_KEYS`（36 键）、2s `TYPEAHEAD_TIMEOUT`、buffer/timer、`content.a11y(index).label` 大小写不敏感匹配、跳过 disabled、从高亮下一项循环、关闭/释放清 buffer（`menu.rs` 新增段）。
- 探针：`'A' -> Some(3)（Aurora）`、重复 `'A' -> Some(0)（Alpha）`、2.1s 后 `'A' -> Some(3)`（超时清 buffer 后从高亮下一项重新搜索）；typeahead 不关闭面板。
- 新测试 `dropdown::typeahead_matches_labels_skips_disabled_and_cycles`、`typeahead_buffer_expires_after_the_timeout`、Select 同型；ui_kit 889 全绿。

---

## 2. r2 新发现（P0–P3）

### R2-01 [P2] ColorPicker 的 Tab 被绑在 content 根：从任意内部控件按 Tab 都会立即关闭面板，hex/确认等内部控件对键盘用户不可达（修复引入的回归）

**现象/证据**

- 修复前（r1 探针，`review-final-interaction.md` FI-01）：`PICKER open: focus=picker:panel.tab.0` → Tab 依次 `tab.1 -> picker.hex.text -> picker.panel.confirm -> tab.0`（4 个可聚焦控件，闭环但可达）。
- 修复后（r2 探针）：`PICKER open: ...` → 第一次 Tab 直接 `PICKER Tab: open=false entries=0 focus=after-picker`；`ColorPicker::tab_out` 立即 `close_now` 并离开 popup（`color_picker.rs:1134-1150`）。
- 根因：`bind_tab_keys(app, window, content)` 把 Tab/Shift+Tab **绑在 ColorPicker content 根**（`color_picker.rs:1152-1172, 1759`）；key dispatch 从焦点节点沿祖先找 scoped binding，任何内部控件（`tab.0/tab.1/hex.text/confirm`）按 Tab 都命中该 action，先关面板，永远没有机会在内部移动。
- 影响：ColorPicker 的 hex 输入是键盘用户唯一的文本输入路径（palette/HSLA track 本身是 Canvas 拖拽）；现在键盘用户只能打开/关闭，无法把焦点移到 hex 输入或确认按钮（`confirm` 只能 Enter 时若已在焦点；实际不可达）。相较修复前是**键盘可达性回归**，WCAG 2.1.1 视角可升 P2。
- 对照：Dropdown/Select 只有一个可聚焦 surface，Tab 关闭是正确的；ColorPicker 是多控件 popup，应“内部先走、到边界再关闭并离开”。

**建议**：Tab action 改为 boundary-aware——先 `app.focus_next/prev`，若焦点仍在 ColorPicker 内部（或 owner scope 未离开）则不关闭；离开时才 `close_now` + 回 trigger + 主树前后移；或直接不绑 content 根、由引擎 Take 逃逸 + `on_close`/focus-lost 处理。补测试：“从 tab.0 Tab 到 tab.1/hex/confirm 不关；从最后一个控件 Tab 关并离开；Shift+Tab 对称”。

### R2-02 [P3] `profile_ui` 账户菜单：Tab 后焦点离开页面控件、菜单仍留在屏幕上（FI-01 宿主残余）

- `profile_ui/src/profile.rs:632-660` 的 `PopupSpec.focus = FocusPolicy::Take { restore: Opener }`，内容是 `MenuContent` 的 `Button`（非 kit `Menu`）；全文件无 Tab/Shift+Tab 绑定与 focus-lost 关闭逻辑（`sync_menu` 只处理 `dismissed`/锚点/内容重建，`:695-741`）。
- 引擎新语义（`popup_behavior.rs:6161-6212` 明确断言）是：非模态 Take popup 的 Tab 逃逸到主树后 **popup 保持 `Open`**；因此 profile 账户菜单 Tab 后表现为：焦点到页面下一个控件，但菜单浮层仍覆盖/存在，直到 Esc/外部点击。
- 探针（通用 Take popup，等价路径）：`CUSTOM tab2: focus=none ... entries=1 still_open=true`（探针 opener 未预置焦点所以为 none；profile 实际会回 previous trigger 后到下一主树焦点，popup 仍 Open）。
- 影响：菜单类宿主浮层不符合用户裁决“非模态 Tab 关闭+焦点”；键盘用户在页面控件上操作时菜单仍挡在上层。P3（Pop-up 仍可用 Esc/外部关闭，不阻断全局；仅这一处宿主）。
- 建议：profile 自定义菜单增加与 `Menu::tab_out` 等价的 Tab/Shift+Tab 绑定（关链 + 焦点移 opener 前后）；或改用 kit `Menu`/`MenuChain`。若有“通用 Take popup Tab 不关闭”语义，应同步在 profile 菜单上显式处理并在设计登记。

---

## 3. 交互回归复扫（用户要求逐项）

| 项 | r2 结果 | 证据 |
|---|---|---|
| 非模态 Tab 关闭+焦点（Dropdown/Select） | **PASS** | 探针 entries=0、focus=after/before；新测试 + 889 kit tests；引擎 124 |
| 非模态 Tab 关闭+焦点（ColorPicker） | **部分**（关闭+焦点 PASS；内部导航失败 **R2-01**） | 探针 + `color_picker.rs:1134-1172` |
| 非模态 Tab 关闭+焦点（profile 菜单） | **FAIL（R2-02）** | 代码无 Tab 绑定 + 引擎测试明确 popup 保持 Open |
| 行 tab_stop / Enter 高亮 | **PASS** | `List::set_rows_tab_stop(false)`；a11y 无 Focus action；探针 Tab 不再落 row；`enter_chooses...` |
| typeahead | **PASS** | 探针 A→3、重复 A→0、2.1s→3；kit 测试 |
| Alert 关闭（settings） | **PASS** | 宿主 236/0 + 定向 2/0；探针有 action 关闭 |
| safe triangle / grace Consume | **PASS** | 引擎 `an_ignored_outside_press_is_consumed...`（`popup_behavior.rs:6090-6120`）+ kit `a_press_inside_the_safe_triangle_keeps_the_chain_open` 含背景 self-check（`menu.rs:5604-5652`） |
| FA-03 `PopupState::Closing` 真状态 | **PASS（未发现回归）** | kit 匹配 Closing 变为活代码；dialog/popup_panel 测试更新；全量测试通过 |
| FA-04 删除死 API（`OutsidePress::Ignore`/`EscapePolicy::CloseChain`/`RestoreFocus::RootFallback`/`ZPolicy`/`popup::MaxSize`） | **PASS（未发现引用/行为回归）** | `rg` 生产路径无引用；`placement` 统一 viewport clamp（默认行为未变）；ui_core/ui_kit 3880 通过 |
| Menu/ContextMenu 原有矩阵 | **PASS（未变）** | 124 engine + 889 kit；真实像素 Windows 106 含 hover/safe triangle/sibling/Esc/outside/keyboard |
| Dialog Trap/Tab 循环/Alert 叠放 | **PASS（未变）** | dialog/alert 测试；settings 236 |

---

## 4. 测试与门禁（r2 实测）

### 4.1 `cargo test -p ui_core -p ui_kit`

- 完整命令（含 examples）**未能完成**：并发 `gallery.exe` 正在运行，链接 `examples\gallery.exe` 报 `LNK1104`（文件被占用）；不是代码错误。审查期间见 4 个并发 gallery 进程（21:43/21:48/21:56/21:58）。
- 改用 `cargo test -p ui_core -p ui_kit --lib --tests`：**全部 PASS**：
  - ui_core lib **2802 passed / 1 ignored**
  - popup_behavior **124 passed**
  - frame_record 8、layout 4、snapshot 14、demo example 39、gui_smoke 0/1 ignored
  - ui_kit lib **889 passed**
  - 合计 **3880 passed**（与提交说明一致）
- 说明：`--lib --tests` 覆盖了全部库/集成测试；唯一未跑的是 example 二进制编译（被并发运行锁住），不影响测试结论。

### 4.2 宿主定向

```text
cargo test -p settings_ui --lib            => 236 passed / 0 failed
定向 credentials::a_failed_write...        => PASS
定向 shell::a_write_that_cannot_be_stored... => PASS
```

- 抽样（本轮实跑，全部 PASS）：
  - `updater_ui` 72 passed + `tests/fetch` 1 passed
  - `profile_ui` 87 passed
  - `market_participants_ui` 22 passed
  - `instrument_selector_ui` 89 passed
  - `widget` 182 passed
  - `kytrix --bins` 80 passed
  - `settings_ui` 236 passed（含新增 shell Alert 测试与修复后的 credentials 用例）

### 4.3 真实窗口证据（只读）

- `target/real-pixels/windows/report.txt`：**checks=106 / failures=0 / result=PASS**（elapsed=41293 ms、last_frame=279；`interact_*` 显示真实 pointer hover/safe triangle/sibling/Esc/outside replay/keyboard Tab+arrow、scroll/resize 重定位、负向自检），证据 PNG 00–17 齐。
- **按 per-run 证据目录（设计 §14.10 的 source-id 机制）复核，Windows 106 在最终 HEAD 有绿证**：`target/real-pixels/windows/20261127T134244Z-e69de29b/report.txt` = `source_revision=5df859b8 / clean / checks=106 / failures=0 / PASS`（43111 ms）；`.../20261127T140144Z-e69de29b/report.txt` = **`source_revision=00927ff9 / clean / checks=106 / failures=0 / PASS`**（43185 ms，含 07–17 交互 PNG/日志）。顶层 `target/real-pixels/windows/report.txt` 在 22:02 被一次并发运行覆盖为 FAIL（`checks=106 / failures=9`，全部为 `changed=0` 的截图/像素检查，且日志明确 note“截图未得到完整标记框（合成撕裂/窗口被遮挡）”）——这是并发 gallery 窗口争抢桌面的假红，**不能作为最终门禁结论**；应以带 `source_diff_sha` 的 per-run 目录为准。审查期间桌面上同时存在多个 `gallery` 进程（21:43/21:48/21:56/21:58…），与设计 §14.4“需独占桌面”一致。
- macOS/Linux `report.txt` 仍是 P3 的 **55 checks** 程序化颜色/像素验收（`f24a3441 dirty(30)`）；提交 `5374c4c8` 说明 Ubuntu 也已跑 106/0，但本地 `target/real-pixels/linux-remote` 未同步该次证据；macOS 未见 106 证据。三端 106 交互验收严格说仅有 Windows 本地绿证（P4；若用户要求三端 106，需补跑）。

---

## 5. 残余风险 / 观察（P4，不影响 P0–P3 计数，但建议登记）

1. **R2-03 gallery Alert 演示不关闭 Dialog**：`gallery.rs:2084-2095` 的 `DialogBuilder::alert` 的 OK/Cancel `ok_action/cancel_action` 只写日志、不 `dialog.close`；手动点“删除”后 Dialog 留在屏幕上（demo 非生产；但它是三端验收/手动演示窗口）。建议补 `dialog.close(app)` 或注明 demo 行为。
2. **R2-04 三端 106 交互验收仅 Windows（本地证据）**：Windows 在 `5df859b8`/`00927ff9` 各有 106/0 PASS per-run 证据；`5374c4c8` 提交说明 Ubuntu 也跑过 106/0，但本地 `target/real-pixels/linux-remote` 仍是 P3 的 55 checks，macOS 无 106 证据。建议要么补同步 macOS/Linux 106 证据，要么在 §14.4/§14.10 明确“106 交互验收本地仅 Windows，Linux 见提交说明，macOS 未跑”。
3. **R2-05 设计/实施文档未回写最终修复**：`review-final-interaction-r2.md` 尚未列入 §14.10 审计清单；§4.9 仍只写 Trap 的 Tab 闭环（未写 Take 的逃逸语义）；§7.3 的 Dropdown/Select typeahead 已实现但未在 §14 标注；`review-final-arch/quality` 的 FQ 条目与最终修复映射未统一。属文档收尾（P4）。
4. **R2-06 证据目录/工作树卫生**：`tools/source-id.txt` 当前为未跟踪文件（`git status` 只有 `?? tools/source-id.txt`；设计 §14.10 建议忽略或提交前删除）；顶层 `target/real-pixels/windows/report.txt` 已被并发运行覆盖为假红，交付引用时应使用带 `source_diff_sha` 的 per-run 目录（`20261127T140144Z-e69de29b` 为 `00927ff9` clean 的 106 PASS）。
5. 已登记 P4（r1 §5）仍成立：真实窗口交互的跨端覆盖、证据快照文字、宿主 popover 的 a11y 角色（profile/market 的 role=Menu 内容是 Button、无 APG 方向键）、Alert API 防护、已登记 P4 复核。

---

## 6. 复现命令

```powershell
$env:CARGO_TARGET_DIR='E:\kytrix\target'

# 1) 仓库外 r2 探针（Tab/类型/Alert/自定义 Take popup）
$env:CARGO_TARGET_DIR='E:\kytrix\target\final-review-interaction\r2-probe-target'
cargo run --manifest-path E:\kytrix\target\final-review-interaction\r2-probe\Cargo.toml
# 输出：E:\kytrix\target\final-review-interaction\r2-probe-output.txt

# 2) 正式测试（若桌面有并发 gallery 进程，example 链接会被占；用 --lib --tests 规避）
cargo test -p ui_core -p ui_kit --lib --tests

# 3) 宿主定向
cargo test -p settings_ui --lib
cargo test -p settings_ui --lib -- --exact \
  settings_tests::credentials::a_failed_write_should_keep_the_dialog_and_what_was_typed \
  settings_tests::shell::a_write_that_cannot_be_stored_should_be_reported_over_the_page
```

- r2 审查文件：`.docs/popup-overlay-redesign/implementation/review-final-interaction-r2.md`
- 只读复核；审查结束工作树仍只有 `?? tools/source-id.txt`（生成物，非代码改动）。

---

## 7. 结论

- **FI-01：引擎+kit 侧关闭；宿主 profile 菜单残余 → R2-02 [P3]**。
- **FI-02：已关闭**（settings 236/0 + 定向测试 + 探针）。
- **FI-03：已关闭**（补 `advance(200)`，236/0）。
- **FI-04：已关闭**（typeahead 探针/测试 PASS）。
- **新回归**：**R2-01 [P2] ColorPicker Tab 从任意内部控件立即关闭，hex/确认键盘不可达**（修复过度）。
- **严格口径 P0/P1/P2/P3 全 0：否**（P2=1、P3=1）。若用户裁决“ColorPicker 按 Tab 即关闭（内部控件接受仅鼠标/AT）”并把 profile 菜单 Tab 不关登记为 P4，则 **P0–P3 = 0，可宣布完成**；否则建议按 §2 做两处小修后复跑 `--lib --tests` 与 settings 定向即可。
- **最终 HEAD 验收**：`00927ff9` clean 的 Windows `--real-pixel-check` per-run 证据 `20261127T140144Z-e69de29b` = 106/0 PASS；顶层 `report.txt` 的 106/9 FAIL 是并发 gallery 窗口遮挡造成的假红（状态类检查均 PASS、像素类 `changed=0`），最终交付不要引用顶层失败报告。macOS/Linux 本地仍是 55 checks（P3 旧证据），三端 106 仅 Windows 本地绿证。





