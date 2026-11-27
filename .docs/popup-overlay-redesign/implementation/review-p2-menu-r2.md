# P2 收口复核（第 2 轮，task-58 修复后）— p2-review-menu

- 对象：worktree `E:\kytrix\.agents\worktrees\dsh-1d175414-1a41`，基线 `152616b1`，task-58 未提交修复（menu.rs 5697 行/68 fns、menu_chain.rs 911 行、context_menu 17 tests、gallery chain 阶段）。
- 方法：只读复查 `menu.rs`/`menu_chain.rs`/`context_menu.rs`/`gallery.rs`/引擎 a11y 补丁 + 复用第 1 轮仓库外探针（`E:\kytrix\target\p2-probe`，新增 J/K/M/N 用例）+ 全量门禁。仓库零改动（本文件除外）。

## 0. 结论

| 项 | 结果 |
|---|---|
| P0/P1 | 0 / 0 |
| P2 | **1（M2-02R，M2-02 残余，阻塞提交）** |
| 已关闭 | M2-01、M2-03、M2-04、M2-05、M2-06、M2-07、M2-08（主体）、M2-09、M2-10（文档已改）、Q2-01、Q2-03 |
| 门禁 | `cargo test -p ui_core -p ui_kit` PASS（ui_core 2805+111、ui_kit 853、其余 8/4/14/39）；`--pixel-check` PASS（94 checks、damage_full=0、exit 0）；`--debug-stack` PASS（3 级链、最终 entries=0、exit 0） |
| 提交判定 | **当前工作树不可提交**：M2-02 仅关闭了 grace 路径；Esc/Left 关子菜单、以及整链关闭后重开（task-58 移除 `on_level_closed` 复位导致的新回归）仍残留 `last_hovered/trigger_over`。修复约 3 行 + 2 条测试；若 lead 决定降级，必须先在 `p2-summary.md §7` 登记为 P3 并写明适用路径。 |

## 1. 逐条复核（M2-01…M2-10 / Q2-01 / Q2-03）

| 编号 | 状态 | 复核证据（代码/测试/探针） |
|---|---|---|
| M2-01 Toggle/Closing 重开链恢复 | **关闭** | `sync_own` Opening｜Open → `reconcile_chain`：Closing→`reopen` + 清 `last_hovered/trigger_over/active_descendant`，Closed→root `open_root`/submenu `revive`（menu.rs:1579-1632）；失败路径 reset+level 清理（:1412-1423）。测试 `a_cancelled_close_restores_the_chain_and_hover_cascade`；探针 [A] 重开后 hover 级联 stack=2（对照 2）；子菜单 Toggle 探针 [M] 重开后 sibling 替换正常（M6 child 换新）。 |
| M2-02 last_hovered 复位 | **部分（P2 残余，见 M2-02R）** | 仅 `fire_close_grace`（:2049-2052）与 `reconcile_chain` Closing 分支复位；`on_level_closed`（:1634）不再复位、`install_handlers` 的 submenu `on_close`（:2659-2670）只 `set_active_row` 不复位。测试 `reentering_the_same_submenu_row_after_the_grace_close_reopens_it` 只覆盖 grace 路径（通过）；Esc/重开路径探针 [J]/[K] 失败。 |
| M2-03 Closing 取消 PendingOpen | **关闭** | `sync_own` DismissPending｜Closing → `cancel_pending_opens`（递归）+`mark_closing`（:1587-1592）；`start_pending_open` 与 `fire_pending_open` 双重校验 panel dismissable + chain Open（:1908-1916、:1938-1948）。测试 `escape_cancels_a_pending_submenu_open`；探针 [H] 关闭动画期间 stack 恒 ≤1、无子菜单闪现。 |
| M2-04 bind_trigger 语义/文档 | **关闭（登记）** | `Menu::bind_trigger` 现仅 Alt+F10、重绑先 `off_action`（:1515-1542）；`ContextMenu::bind_trigger` 仅键盘 + `off_action`（:194-238）；summary §3.1/§3.2/§4-D-P2-4 改为“仅键盘、trigger 需宿主可聚焦、鼠标由宿主绑定”。探针 [F] 与之一致（plain trigger 无焦点；clickable trigger Alt+F10 生效、左键不打开）。 |
| M2-05 内部滚动关整链 | **关闭（登记）** | `sync_anchor` 保持 Moved→close_chain（:2848-2866）；测试更名 `scrolling_the_parent_row_out_of_view_closes_the_whole_chain` 断言整链；summary D-P2-3 改为“一律关整链（含内部滚动）”。探针 [C2] 与之相符。 |
| M2-06 grace 300ms 过期 | **关闭** | open/flip/scroll 均 arm/rearm（`open_submenu_row` :2149、`refresh_geometry` :1774-1793、`update_hover` :1836-1838）；timer 清多边形。测试 `the_safe_triangle_expires_and_the_sibling_replaces_it`（断言 `grace(child).is_none()` + 兄弟替换 + 新层槽/父 active）；仓库另测 `a_press_inside/outside_the_safe_triangle...` 用 `safe_area_contains` 隔离落点（非 root/child rect）。 |
| M2-07 引擎 a11y 自述/幂等 | **关闭（登记 P3）** | summary §3.3 改为“PopupError 仍 13 变体、非法目标清空+`InvalidActiveDescendant` 诊断”，§7 登记 `set_popup_a11y` 重复 `Ok(true)` 为 P3；代码/测试与之一致（popup_behavior:5280、a11y/popup_tests:1722）。本轮未改该幂等实现（仍为已登记 P3，Menu 不触发）。 |
| M2-08 弱测试/覆盖缺口 | **主要关闭** | 新增：`disabled_submenu_rows_never_open`、`shift_tab_closes_the_chain_and_focuses_the_previous_node`、`radio_groups_are_bounded_by_separators`、`the_first_keyboard_event_moves_focus_into_the_hover_opened_child`、`the_safe_triangle_expires...`、`a_failed_open_resets...`、`a_cancelled_close...`；`a_hover_opened_child...` 扩为 3 级 active 隔离。残留见 §2.2（1 条恒真断言 + 2 个测试缺口）。 |
| M2-09 Group/Separator role | **关闭（登记）** | summary 新增 D-P2-10（Group=Label、Separator=Separator+hidden；§7 P3）。 |
| M2-10 ContextMenu 锚点文档 | **关闭（文档）** | summary §3.2/D-P2-4 改为“open_on 底边左端点”；实现 `context_menu.rs:180-193` 一致。设计 §4.2 的 WindowRect 1×1 与实现 0-size point anchor 差异仍未在偏差表单列（P3 观察，见 §2.2）。 |
| Q2-01 open 失败 reset/重试 | **关闭** | `open_focused` Err 路径：root `chain.reset`+level None；submenu `cancel_open`；`active_descendant` 清空（:1412-1424）。测试 `a_failed_open_resets_the_chain_and_the_retry_keeps_submenus_working`（dead window open Err → level None/链空 → 重试键盘 Right 开子菜单、无自引用槽）。 |
| Q2-03 删除 6 点桥接 | **关闭** | `bridge_polygon/bridge_contains` 已删（grep 仅品牌 Longbridge）；`dismiss_outcome` 用 `safe_area_contains`=grace∪`derived_safe_triangle`（menu_chain.rs:127-139、599-613）；测试改名为安全三角 in/out 两条并白盒断言落点不在 root/child rect。 |

## 2. Findings（第 2 轮）

### M2-02R（P2，阻塞）：`last_hovered/trigger_over` 复位不完整——Esc/Left 关子菜单与整链重开后 hover 意图失效

- **位置**：`menu.rs:1634-1647`（`on_level_closed` 已无复位）、`menu.rs:2659-2670`（submenu `on_close` 只 `set_active_row`）、`menu.rs:2790`（`close_to_parent` 走 `close_now`→同 on_close）、`menu.rs:1604-1632`（root Closed 分支只清 `active_descendant`）。
- **复现（仓库外探针，公开 API）**：
  - **[J]** 打开菜单 → hover 子菜单行（stack=2）→ Esc 关子菜单（root 留存，stack=1）→ 指针移出链再移回**同一行** +300ms：`stack=1`（未重开）。原因：父层 `last_hovered` 仍为旧行，`update_hover` 的 `hovered == last_hovered` 提前返回（menu.rs:1846-1849）。
  - **[K]（task-58 引入的回归）** 打开菜单 → hover 子菜单行 → 两次 Esc 整链关闭（`open=false`）→ host 程序化重开 → 指针移到同一行 +300ms：`stack=1`；全新菜单同序列对照 `stack=2`。原因：`on_level_closed` 删除了旧的 `last_hovered/trigger_over` 复位，重开后状态是上次会话残留。
- **影响**：§4.8 `on pointer_enter` 契约在“Esc 关子菜单后重入同一行”和“重开菜单后指针停在 submenu 行”两条路径失效；鼠标用户会看到“子菜单不再弹”。非主路径、无状态损坏，故 P2（不 P1）。
- **修法（约 3 行）**：
  1. `on_level_closed`：开头 `self.inner.last_hovered.set(None); self.inner.trigger_over.set(false);`（恢复旧行为，修 [K]）；
  2. submenu 关闭时复位父层（避免 sibling 替换被清掉）：在 `on_level_closed`/`install_handlers.on_close` 里取 `chain.parent(level)/row`，仅当 `parent.last_hovered == Some(row)` 时置 None、`trigger_over=false`（修 [J]，且不影响 update_hover 先设置新行的顺序）；
  3. `reconcile_chain` root Closed 分支同样清 `last_hovered/trigger_over`（与 Closing 分支对齐）。
- **测试**：加 2 条（Esc 后离开/重入同一行；整链关→重开→指针移到 submenu 行）。

### 2.2 残余 P3（未在 `p2-summary.md §7` 单独登记，建议登记/可选修复）

| # | 项 | 证据/影响 | 建议 |
|---|---|---|---|
| R2-a | typeahead 跨层 timer | buffer 在 chain 全局、timer 在每层 Menu（menu.rs:2408-2437）；A 层旧 timer 到期会 `clear_typeahead()` 清掉 B 层更新的 buffer（提前过期） | timer 捕获 level，回调校验 `typeahead.level==level`；登记 P3 |
| R2-b | 子菜单 Right→Left flip 无集成测试 | 探针 [N]：右边缘锚点下 child 落左侧（390 < panel 590），镜像 safe triangle 内移动 400ms 后 child 保留；实现可用但 §7.3“翻面”无测试 | 补 1 条集成测试（flip + grace/替换）；登记 |
| R2-c | “root Opening 期间跳过 anchor watch”无专项测试 | 代码 `menu.rs:1595` 有钳制；测试 `a_cancelled_close...` 间接覆盖，但未构造 anchor Moved 场景 | 补 1 条（重开首帧旧 anchor 失效不误关） |
| R2-d | 残留恒真断言 | `menu.rs:4465` `assert_ne!(level2.id, root_entry.id)`（child 与 root 的 PopupId 必不同） | 改为 root id 稳定 + child 关系断言；登记 |
| R2-e | 设计 §4.2 WindowRect 1×1 vs 实现 0-size point anchor | `ContextMenu::open_at` 用 `LogicalSize::ZERO`（`popup_panel.rs` point-anchor 特判可用）；无功能问题 | 文档择一；P3 |
| R2-f | `open_submenu` 返回的 submenu 句柄上 `on_activate/on_close` 静默无效（仅 root 生效，共享 sink 的设计） | 新测试 `a_context_menu...` 不涉及；宿主误用会静默无效 | 文档注明句柄只用于 open/close/sync；P3 |

> 已确认均已登记：Separator/Group role（D-P2-10）、R2-02 reason、R2-01 首帧滚动、K1-11 FadeSlide、E-12、D-P2-4 右键自动绑定、P3-1/2/4/5/6、M2-07 幂等、A-26/27/31、外部滚动语义精度。

## 3. 新竞态复扫（结论：无新增 P0–P2，除 M2-02R）

| 检查 | 结果 |
|---|---|
| Closing 期间 PendingOpen 定时器 | 无（探针 [H]：stack 恒 ≤1，子菜单不闪现）；两层递归取消（`cancel_pending_opens`） |
| 子菜单 Toggle 重开（Esc 后点 opener 行） | 正常（探针 [M]：重开后 sibling 替换关闭旧层并开新层；reconcile Closing→reopen） |
| 子菜单 failed open 重试 | 正常（测试 + `cancel_open`/`release` 路径） |
| grace timer 生命周期 | open/flip/scroll 均 arm/rearm；`arm_grace_timer` 先取消旧 timer；无重复/悬空（探针 [I]/仓库测试） |
| safe triangle 过期后兄弟替换 | 正常（仓库测试 `the_safe_triangle_expires...`；过期后多边形清空，替换分配新层槽、父 active 跟随） |
| flip 后几何/grace | 正常（探针 [N]：镜像三角 400ms 内保留） |
| root Opening 首帧 anchor 误关 | 代码钳制在 `sync_own`（`root().panel.state != Opening` 才 observe），未发现误关 |
| active_descendant 缓存 | 仅 active 变化才 `set_a11y`；reconcile/失败路径均清缓存后重投；无遗漏路径 |
| 共享 sink（submenu on_activate/on_close 忽略） | 符合设计；ContextMenu `on_item_click` 已带 `check`；gallery `chain_menu.sync` 已加入帧循环（:2539-2540），否则关闭动画不会推进 |
| 关闭事务后定时器/槽位 | `cancel_timers` 覆盖 4 类 timer；`revive` 仅 Closed 槽、重挂 parent.child；未见自引用槽 |

## 4. 门禁实测（本次复跑）

- `cargo test -p ui_core -p ui_kit`：**PASS**（ui_core lib 2805 passed/1 ignored；popup_behavior 111；frame_record 8；layout 4；snapshot 14；demo example 39；ui_kit **853**；0 failed）。
- `gallery --pixel-check`：**PASS** exit 0，`checks=94`、`sampled_frames=32`、`close 6+6`、`damage_full_frames=0`。
- `gallery --debug-stack`：**PASS** exit 0，3 级链阶段通过、`menu_frames=426/picker_frames=302/retries=0`、最终 `entries=0`。

## 5. 结论（回复 lead）

- P2 修复主体成立：M2-01/03/04/05/06/07/08/09/10、Q2-01、Q2-03 已关闭（含测试/文档），门禁全绿。
- **唯一阻塞项 M2-02R（P2）**：`last_hovered/trigger_over` 只在 grace-close 与 Toggle-reconcile 路径复位；Esc/Left 关子菜单后重入同一行、以及整链关闭→重开后指针停在 submenu 行，hover 不重开（[J]/[K] 探针）。其中 [K] 是 task-58 删除 `on_level_closed` 复位引入的回归。建议 3 行修复 + 2 测试后提交；若判定为 P3，须先在 summary §7 登记（current 未登记）。
- 其余未登记项均为 P3（§2.2 R2-a…R2-f），不阻塞；已登记的 P3（角色、R2-02、性能基线、右键、E-12 等）齐备。
