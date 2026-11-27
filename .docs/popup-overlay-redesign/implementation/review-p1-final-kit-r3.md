# P1 终审第 3 轮（收口确认）：kit 复核

- 角色：kit 终审对抗性审查员（只读）；worktree `E:\kytrix\.agents\worktrees\dsh-1d175414-1a41`，基线 `d9949aca`
- 复核对象：E-20（窄视口 Dialog 宽度 clamp）、A-25（静态 haspopup）、A-29（子菜单 a11y 父）、E-12（kit 命名）+ 第 2 轮未关闭项登记状态
- 结论：**E-20 / A-25（静态）/ A-29 / E-12（命名）均真正关闭**；**未发现 P1 范围内未登记的 P0–P3 缺陷**；剩余项均为已登记的 P2/P3 阶段计划或已接受的 P3。**P1 可提交。**

---

## 1. 门禁实测（本机 Windows 11，`CARGO_TARGET_DIR=E:\kytrix\target`）

| 门禁 | 结果 |
|---|---|
| `cargo test -p ui_core -p ui_kit` | **PASS：3780 passed / 0 failed / 2 ignored**（ui_core lib 2804+1 ignored、frame_record 8、gui_smoke 0+1 ignored、layout 4、popup_behavior 110、snapshot 14、demo 39、ui_kit lib 801）。首次尝试因另一个 `gallery` 进程占用 `gallery.exe` 报 LNK1104（环境冲突），进程退出后重跑 PASS |
| `gallery --pixel-check` | **PASS**：checks=94、sampled_frames=32、open=8、close=6+6=12、damage_full_frames=0、first_visible==stable=(496,91,300×375)、moved=(496,4,300×375)，exit 0 |
| `gallery --debug-stack --auto-close=9` | **PASS**：menu 度量 `(6,180.0,127.5125)`、menu_frames=395、picker_frames=384、entries=0、retries=0，自退出 0 |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` / `fmt --check` / `cargo check --workspace --all-targets` | 全 PASS |
| 定向复跑 | `a_narrow_viewport_clamps_the_default_dialog_width…`、`a_submenu_row_keeps_static_haspopup…`、`a_two_level_submenu_chain_attaches…`、`an_at_least_panel_honours_minimum_width…` 各 1 passed；`the_trigger_reports_the_selection_and_the_expanded_state` 2 passed（dropdown+select）；`instrument_selector_ui` 的 `keeps_the_footer_reachable` 两条 2 passed |

---

## 2. 逐项复核

### E-20 窄视口 Dialog 宽度 clamp —— **fixed**
- 代码：`PopupPanel::available_body_width`（`popup_panel.rs:329-343`，与 `available_body_height` 同构，读 `constrained_size`/`hit_rect` 减 padding）；Dialog `sync_width`（`dialog.rs:797-824`，`target = config.width.min(available)`，写入 content root，`applied_width` 去抖；`sync()` 内 `sync_width → sync_scroll` 顺序正确，`dialog.rs:484-485`）；reset/close 清 `applied_width`。
- 测试：`dialog::a_narrow_viewport_clamps_the_default_dialog_width_and_keeps_the_buttons_reachable`（320×220：panel ≤312.5 且在视口内、content 宽 = panel−16、content/confirm 不越界、confirm 非零宽）；定向复跑 1 passed。K1-08e 的 instrument Dialog 对应两条测试（设计宽度 736/内容 720 + 窄窗 clip/footer 可达）实测 2 passed，且全部走 `popup_total()` 断言无残留。
- 结论：功能与回归测试均闭合。首帧收敛仍受已登记 R2-01 同类限制（见 R3-01 注记），非 P1 缺陷。

### A-25 静态 aria-haspopup —— **static fixed（active_descendant 为已登记 P2 计划）**
- 代码：Dropdown trigger `.a11y_has_popup(HasPopup::Menu)`（`dropdown.rs:233`）、Select `HasPopup::ListBox`（`select.rs:280`），经 `Interactive` 的 a11y patch 静态投影（`interactive.rs:71,226-228,473-474`）；Menu 子菜单行 a11y patch `has_popup: Some(HasPopup::Menu)` + `expanded: Some(false)`（`menu.rs:1417-1421`），打开期由引擎 popup 投影把 `expanded` 置 true。
- 测试：`dropdown::the_trigger_reports_the_selection_and_the_expanded_state`、`select::the_trigger_reports_the_selection_and_the_expanded_state`（关闭/打开/关闭后 haspopup 保留 + expanded 切换）、`menu::a_submenu_row_keeps_static_haspopup_and_projects_expanded`（打开 expanded=true，关闭后 haspopup 保留/expanded=false）；定向 2+1 passed。引擎侧 `static_haspopup_controls_and_expanded_survive_the_popup_lifecycle`（`a11y/popup_tests.rs:1608`）。
- 残余：`active_descendant` kit 仍未写入，登记于 §12.6-15（需 MenuChain P2；引擎 `a11y_patch_active_descendant_is_a_hover_channel` 通道已具备）。属**已登记 P2 阶段计划**，非 P1 缺陷。

### A-29 子菜单 a11y 父 —— **fixed**
- 代码：`MenuSeed.a11y_parent` 透传到 `PopupA11y.a11y_parent`（`menu.rs:969-1066,1177`）；打开子菜单时写父 menuitem 节点（`menu.rs:1545`）；根菜单保持 None。引擎 `resolve_logical_parent` 优先消费 requested parent 并做合法性诊断（`a11y/popup.rs:227-239`）。
- 测试：`menu::a_two_level_submenu_chain_attaches_to_its_parent_menu_items` — 断言二级 popup a11y parent == root 父行、三级 popup parent == 二级父行，且 root 行 `has_popup=Menu`/`expanded=true`；定向复跑 1 passed。设计 §12.11 已登记 fixed。

### E-12 kit 命名 —— **fixed（命名项；其他 E-12 残余已登记）**
- 测试重命名 `an_at_least_panel_honours_minimum_width_changes`（`popup_panel.rs:1164`），内容为“无测量数据时保持 AtLeast 下限 120 → `set_width` 提高下限到 220 后面板变宽”，断言真实（非仅复用锚点宽度）；定向复跑 1 passed。
- 残余（已登记 §12.6-6）：Menu 指针测试命名、P1 popup 套件无变异自检、`--pixel-check` 仅覆盖 ColorPicker、caret+`paint_offset` 直接断言 —— 均为 P3 测试增强计划，非 P1 缺陷。

---

## 3. 第 2 轮遗留项的最终登记状态（与 §12.11 对齐）

| 项 | 状态 |
|---|---|
| E-20 / E-21 / E-17 / E-18 / E-13 | fixed（本轮复核 E-20；其余见 §12.11 证据） |
| **K1-07** 宿主 `popup_count` 过滤 Scrim | **fixed**：7 个 helper 增 `popup_total()`（`updater_ui:104`、`instrument_selector_ui:157`、`market_participants_ui:467`、`profile_ui:789`、`financial_index view:1988/dialog:1467`、`coin_market_cap view/tests:48`），释放/关闭路径断言 `popup_total()==0`（instrument 6 处、financial_index、coin_market_cap 等） |
| **K1-08 b/e/f** | **fixed**：b=Tooltip 显式 TOP 测试；e=instrument Dialog 设计宽度 + 窄窗 clamp 两条测试；f=Dialog resize recenter 测试（实测均绿） |
| **R2-03** §12 计数/状态不一致 | **fixed**：3780/801、115 files/+8671/−9410、Dropdown 27/Select 32/Menu 30/Dialog 54 与实测一致（本轮逐项核过） |
| **R2-01** 受约束 popup 首帧内部滚动高度未 settle | **deferred（已登记 §12.6-9）**：P3，默认 Fade 风险低 |
| **R2-02** anchor 矩阵关闭 reason=Programmatic | **deferred（已登记 §12.6-10）**：生产 on_close 消费者为 0，P2 MenuChain 前补 `close_with_reason` |
| **R2-04** 无 `.confirm()` 自建 footer 初始焦点回退 Cancel | **accepted（D16 fallback，§12.6-11）** |
| **R2-05** 观察项（scrim 主题刷新/滚动条覆盖/ColorPicker 两帧采样） | **registered（§12.6-12）** |
| **K1-11** FadeSlide 未启用 | **deferred（P3-2 计划，§12.6-7）** |
| **K1-12** 宿主 settle 放宽 | **accepted（§12.6-8）** |
| **A-25 残余** active_descendant | **已登记 P2 阶段计划（§12.6-15，MenuChain）** |
| 外部滚动语义宽于“祖先滚动”字面 | 已登记 P3（§12.6-13） |
| 其他 P3（MaxSize 命名、settle layout_ran、A-26/A-27/A-31、性能、三端、E-12 残余等） | 均已登记 §12.6 |

---

## 4. 第 3 轮新增：仅 1 条登记建议（P3，非 P1 缺陷）

**R3-01（P3，登记建议）E-20 的宽度收敛与 R2-01 同构：`sync_width` 也只在 `sync` 生效。**
- 证据：`available_body_width` 依赖 settle 后的 `hit_rect/constrained_size`；`Dialog::open` 本身不调用 `sync_width`（`dialog.rs:438-458` 只做 initial focus + apply_center + `sync` 路径在 `:479-486`）。因此窄视口 + `fade(false)` 时，首个绘制帧 panel 已被引擎 clamp 到 312，但 content root 仍按 384 布局（右缘被 `overflow_hidden` 裁），下一帧才收敛到 296。
- 影响：与 R2-01 同一“受约束 popup 首帧内部布局”族；panel 位置/尺寸首帧稳定，默认 Fade（首帧近全透明）下肉眼风险低；E-20 测试在 settle 后断言，未覆盖首帧。
- 处理建议：无需改代码即可提交；把 §12.6-9 的文字从“内部滚动高度”扩为“内部滚动高度/宽度（`sync_height`/`sync_scroll`/`sync_width`）”即可，或与 R2-01 一并修复（open 末尾做一次受约束布局收敛 + 加首帧用例）。

---

## 5. 结论

- E-20、A-25（静态部分）、A-29、E-12（命名部分）**均真正关闭**（代码+测试+定向复跑证据）。
- **未发现属于 P1 范围、未登记的 P0–P3 finding**。剩余开放项全部是已登记的 P2/P3 阶段计划或已接受的 P3（K1-11、K1-12、R2-01/02/04/05、A-25 active_descendant、E-12 残余、性能/三端），逐条见 §12.6/§12.11。
- 唯一新增只是 R3-01 的**登记文字扩展**（P3，非缺陷，不影响提交）。
- **最终结论：P1 可以提交**（P0–P2 = 0；门禁 3780 passed/2 ignored、94 checks、debug-stack PASS、clippy/fmt/workspace check PASS）。
