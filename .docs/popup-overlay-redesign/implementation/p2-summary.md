# P2 实施摘要（MenuChain / 2–3 级级联子菜单 / 完整键盘与 a11y）

> 范围：设计 §6 P2-1…P2-7（§4.8 菜单链状态机、§4.7 链式 dismiss、§3.4 数值、§4.10 a11y、§7.3 组件测试）。
> 基线：P1 已提交 `152616b1`（`d9949aca` 之上）；P2 全部改动在同一 worktree 的未提交工作树。
> 结论：**P2-1…P2-7 落地（P2-6 为部分 done：右键自动绑定留 P3，见 D-P2-4）**；P2 只改 `ui_kit`（`menu.rs` / 新增 `menu_chain.rs` / `context_menu.rs` / `popup_panel.rs` 小改 / `gallery.rs`）+ 引擎小补（`PopupA11y.active_descendant` 投影；非法目标清空 + `A11yDiagnostic` 诊断，不新增错误变体）。数字为最终门禁实测（task-58 修复后 lead 复跑）；本文件为结构与事实回写。

## 1. 规模与提交内容

| 项 | 结果 |
|---|---|
| 代码 diff（`git diff --shortstat HEAD -- . ':(exclude).docs'`，总行数含空行） | **14 tracked 文件，+5311 / −1475**；另有 **1 个新文件** `crates/ui_kit/src/kit/menu_chain.rs` **959 行**（untracked） |
| 主要文件 | `menu.rs` +4424/−1449（6012 行，76 测试）；`menu_chain.rs` 新增 959 行（13 测试）；`context_menu.rs` +348/−8（1147 行，20 测试）；`gallery.rs` +171/−8（4062 行）；`popup_panel.rs` +21/−6；`dropdown.rs`/`select.rs` 各 +1；`kit/mod.rs` +1 |
| 引擎小补 | `popup.rs` +1 字段（`PopupA11y.active_descendant`；**`PopupError` 仍 13 变体，无新错误**）；`app/popup.rs` +36/−4（打开/更新时校验、非法目标清空 + `A11yDiagnostic::InvalidActiveDescendant`）；`a11y/{popup,tree}.rs` +6（声明值优先于焦点推导）；`a11y/popup_tests.rs` +145（+1 测试）；`tests/popup_behavior.rs` +152（+1 测试） |
| 任务结果 | task-52（P2 实施）、task-53（引擎 `PopupA11y.active_descendant`）、task-55（active_descendant 落点 + safe-triangle Ignore）均 completed；P2 审查修复由 task-58 落地（M2-01/02/03 阻塞项、Q2-03 删除桥接几何、M2-04/05/06/07 等 P3）；P2 收口 task-62 补齐测试真实性（恒真断言、derived_safe_triangle 隔离、failed-open `panel.open` 分支、ContextMenu `open_on`/重复绑定/`on_item_click` check、reconcile pointer-close）；最终门禁见 §6 |

## 2. P2-1…P2-7 结果

| # | 结果 | 证据 |
|---|---|---|
| P2-1 `MenuChain` 状态机 | **done** | `menu_chain.rs`：`MenuChain`（`pub(crate)`）+ `LevelId` + `LevelState{Closed,Opening,Open,Closing}` + `OpenSource{Pointer,Keyboard,Click}`；每层 `parent/child/row/state/source/rect/anchor/exit/grace/active`；`open_root/begin_open/confirm_open/cancel_open/close/mark_closing/reopen/deepest_open/deepest_kept/open_levels/closing_levels`；关闭先子后父（`the_chain_assigns_states_per_level_and_closes_children_first`）；取消 pending open 可复用 slot（`cancelling_a_pending_open_returns_the_slot_to_closed_and_reusable`）；**`mark_closing/reopen/revive` 与引擎 `DismissPending/Closing`/Toggle 同步**（`a_trigger_press_during_the_close_transaction_reopens_the_menu`）；**open 失败 reset 并清理 level**（`a_failed_open_resets_the_chain_and_the_retry_keeps_submenus_working`）；**root `Opening` 期间跳过 anchor watch**；grace 关闭后同一行重入重新 PendingOpen（`reentering_the_same_submenu_row_after_the_grace_close_reopens_it`）；**M2-02R 父层复位口径**：子层关闭时仅当 `parent.last_hovered == Some(closed_row)` 且指针已离开该行（`!over_row`）才清 `last_hovered/trigger_over` 并把父层 active 指回该行（`menu.rs` `panel.on_close` 分支）；timer 绑定具体 level（`Menu` 的 pending/grace/close/typeahead timer 字段，不用全局 topmost） |
| P2-2 safe triangle / grace | **done** | 5 点多边形 `SafeTriangle`（退出点 ±5px bleed 按翻面镜像 + 子菜单近/远端上下角），FOV 点在多边形内判定；打开/翻面/滚动时 `compute_grace` 重算；`HoverTimings{open_delay=200ms, close_grace=300ms, typeahead_timeout=2s}`；grace 期禁止兄弟替换（`pointer_through_the_safe_triangle_keeps_the_child_and_ignores_siblings`、`a_pointer_outside_the_triangle_replaces_the_open_sibling`、`leaving_the_chain_closes_the_pointer_opened_levels_after_the_grace`、`reentering_the_chain_during_the_grace_cancels_the_close`）；**grace 300ms 自 open/几何变化起算并过期清多边形**（`arm_grace_timer/rearm_grace_timer`，M2-06）；外部按下落在 **grace 三角 ∪ 即时 safe triangle** 内 `DismissOutcome::Ignore`（见 D-P2-2；无桥接几何；`a_press_inside_the_safe_triangle_keeps_the_chain_open`、`a_press_outside_the_safe_triangle_closes_the_chain_and_replays`） |
| P2-3 菜单面板行为 | **done** | active_item 高亮 + ensure_visible/滚动（`List`）；子菜单指示 `IconKind::CaretRight`；Group 行（a11y `Label`）、Separator（a11y `Separator`、不聚焦）、disabled 行；Checkbox/Radio 勾选渲染（`●`/勾）+ a11y role `MenuItemCheckBox`/`MenuItemRadio`；长菜单内部滚动，父行滚出视口 = **整链关闭**（`scrolling_the_parent_row_out_of_view_closes_the_whole_chain` 断言整链关闭，M2-05 统一） |
| P2-4 完整键盘表 + typeahead + checkable | **done** | 绑定：↑↓ / Home / End / PageUp / PageDown / ←→ / Enter / Space / Esc / Tab / Shift+Tab / Alt+F10（触发键）；`the_keyboard_table_pages_and_jumps_inside_a_long_menu`、`right_opens_the_submenu_with_focus_and_left_returns_to_the_parent_row`、`tab_closes_the_chain_and_moves_focus_past_the_opener`、`alt_f10_on_the_trigger_opens_the_menu_with_the_first_row_active`；typeahead 36 键（字母/数字）2s 单层 buffer、换层清空、跳过 disabled/separator、循环匹配（`typeahead_cycles_matches_skips_disabled_and_appends_characters`、`typeahead_appends_characters_and_expires_after_the_timeout`）；text/IME 事件不入 buffer（`a_text_event_never_feeds_the_typeahead_buffer`）；checkable：Space 切换 Checkbox 不关菜单、Enter 选择并关链、Radio 同层互斥、`checked` 投影（`space_toggles_a_checkbox_without_closing_and_enter_selects_and_closes`、`radio_members_are_mutually_exclusive_and_expose_their_roles`）；**typeahead 超时按层清理**（`clear_typeahead_for(level)`，timer 绑定 level，只清该层 buffer） |
| P2-5 链式 dismiss | **done** | 外部按下命中祖先层只关后代（`a_press_on_an_ancestor_level_closes_only_its_descendants`）；Esc 逐层 children-first（`escape_closes_one_level_at_a_time_children_first`、`escape_returns_focus_to_the_parent_level`）；关闭原因透传 `on_close(DismissReason)`（`the_close_callback_reports_the_engine_dismiss_reason`）；D14：链外按下默认 Replay、显式 `outside_press(Consume)` 不 replay（`a_consuming_menu_does_not_replay_an_outside_press`）；safe triangle/grace 内按下 `DismissOutcome::Ignore` 不关链；tooltip 优先规则沿用 P1 引擎（Hint tier） |
| P2-6 ContextMenu | **partially done（右键入口见 D-P2-4）** | `ContextMenuOptions{...,outside_press}`；`ContextMenu::{open,open_at,open_on,bind_trigger}`；`bind_trigger` **仅键盘 Shift+F10/ContextMenu 键**（`shift_f10_on_the_focus_node_opens_the_menu_at_the_node`）；WindowRect 指针锚（`open`）与节点锚（`open_on`，取节点底边左端点）；2–3 级子菜单共享 MenuChain（`a_submenu_stacks_above_the_root_and_escape_closes_it_first`、`a_context_menu_chain_opens_three_levels_and_closes_children_first`）；外部 Replay/Consume（`a_press_outside_closes_the_menu_and_replays_the_press_to_the_page`、`a_consuming_context_menu_swallows_the_outside_press`）；外部页面滚动关整链（`scrolling_the_page_closes_the_open_context_menu`）；**`on_item_click` 新签名 `Fn(&mut App, SmolStr, Option<MenuCheckState>)`**；**未内建 secondary-button 自动绑定**，trigger 可聚焦/左键/右键由宿主负责（见 D-P2-4） |
| P2-7 3 级测试 + a11y 链 + gallery | **done** | 3 级：`a_three_level_hover_cascade_stacks_newest_on_top`、`a_three_level_a11y_chain_nests_and_projects_expanded`；2/3 级 a11y 父=父 menuitem（`a_two_level_submenu_chain_attaches_to_its_parent_menu_items`、`a_three_level_a11y_chain_nests_and_projects_expanded`）；expanded 投影（`a_submenu_row_keeps_static_haspopup_and_projects_expanded`）；gallery `--debug-stack` overlay-check 新增 3 级链阶段：`chain-open`、`chain-stack-size`（3 条 popup）、`chain-z-order`（z 升序）、`chain-tier`（Auto）、`chain-parent-level2/level3`、`chain-close-call`、`chain-entries-zero` |

## 3. API 变化（P1 → P2）

### 3.1 `ui_kit::kit::menu`

- `MenuOptions`：**删除** `submenu_open_delay: Duration`；**新增** `timings: HoverTimings`、`outside_press: OutsidePress`（默认 `Replay`）；builder `timings()` / `outside_press()`。
- `HoverTimings`（`ui_kit::kit::menu_chain::HoverTimings`，`pub`）：`open_delay: Duration = 200ms`、`close_grace: Duration = 300ms`、`typeahead_timeout: Duration = 2s`；`Default` 即设计 §3.4 值。
- `MenuAction`：`checked: Option<bool>` → `check: Option<MenuCheckState>`；builder `checked(bool)` → `checkbox(bool)` / `radio(bool)`。
- 新类型 `MenuCheckState { kind: MenuCheck{Checkbox,Radio}, checked: bool }`。
- `MenuActivation { id: SmolStr, check: Option<MenuCheckState> }`（原只有 `id`）；`Menu::on_activate` 回调签名随之携带 check 状态。
- `Menu` 新增：`open_submenu(app, row) -> Result<Option<Menu>, PopupError>`（按行打开子菜单，返回子层句柄，供程序化构建 2–3 级链与测试/gallery 使用）、`bind_trigger(app, window, trigger) -> Result<(), EngineError>`（**仅键盘 Alt+F10**；重复绑定先释放旧 action；触发点取 trigger 节点矩形；trigger 需已可聚焦，鼠标绑定由宿主负责）。
- 保留：`Menu::{new,set_entries,entries,open,open_at,close,close_now,on_activate,on_close,sync,release,open_state,is_open,row_count,highlighted,panel_node,popup_node}`。
- `MenuEntry::{Action,Group,Separator,Submenu}` 与 `MenuSubmenu` 不变。

### 3.2 `ui_kit::kit::context_menu`

- `ContextMenuOptions` 新增 `outside_press: OutsidePress`（默认 Replay）。
- `ContextMenu` 新增 `open_on(app, window, node)`（节点锚：取节点**底边左端点**；节点不存在返回 `PopupError::AnchorNodeMissing`）/ `bind_trigger(app, window, trigger)`（**仅键盘 Shift+F10/ContextMenu 键**，无裸 F10；重复绑定先释放旧 action；左键/右键由宿主绑定）。
- `ContextMenu::on_item_click` 签名：`Fn(&mut App, SmolStr)` → `Fn(&mut App, SmolStr, Option<MenuCheckState>)`（回调携带菜单项 check 状态）。
- 保留 `open/open_at/close/close_now/on_close/set_entries/entries/sync/release/...`（`on_item_click` 见上）。

### 3.3 引擎（`ui_core::popup`）

- `PopupA11y` 新增 `active_descendant: Option<NodeId>`：打开/更新时投影到引擎创建的 **popup content 根** a11y 节点；目标必须在该 popup content 子树内或在 `owns` 列表内；`App::set_popup_a11y` 同帧更新并标脏。
- `PopupError` **仍 13 变体**（未新增）：非法目标采用“清空 + 诊断”路径——open/set 时目标不存在或不在 content 子树/`owns` 内则清空 `active_descendant` 并留 `A11yDiagnostic::InvalidActiveDescendant`（不 panic、不投影非法目标；强类型错误面不膨胀）。**M2-07 P3 观察**：`set_popup_a11y` 以请求值比较、对持续非法声明会重复返回 `Ok(true)` 并请求帧（Menu 不会触发），登记见 §7。
- 投影优先级：节点 `A11yPatch.active_descendant`（非 popup 通道）> `PopupA11y.active_descendant` 声明值 > 焦点推导（content 子树内聚焦节点）；无声明且无焦点时为空（`the_active_descendant_follows_the_focused_menu_item`、`a_declared_active_descendant_is_projected_updated_and_cleared`、`popup_a11y_active_descendant_is_validated_and_cleared`）。
- 说明：`A11yPatch.active_descendant`（节点级 patch 通道）仍保留给非 popup 的普通子树；Menu 不再使用该通道（P1 的 surface 兜底已删除）。

### 3.4 `PopupPanel`（内部小改）

- `OpenRequest` 新增 `surface_action: Option<Action>`：**Menu 自己传入**给 `ItemPanel::open`，使 panel surface 承载行点击；ContextMenu 无额外挂载，Dropdown/Select 传 `None`。

## 4. 行为偏差登记（D-P2-1…D-P2-10）

1. **D-P2-1 `active_descendant` 落点 = popup content 根（role=Menu）**：P1 在 Menu surface（role=GenericContainer）上用 `A11yPatch.active_descendant` 兜底；P2 改为 `PopupA11y.active_descendant` 由引擎投影到 content 根，surface patch 删除。收益：AT 看到 role=Menu 节点的 active descendant（ARIA 语义正确），冒泡链不再依赖 kit 的 surface 节点；内容根仍承担 `a11y_parent`（子菜单挂父 menuitem）。
2. **D-P2-2 safe-triangle outside-press `Ignore`**：外部按下若落在当前链的 **grace 三角 ∪ derived safe triangle** 内，菜单 `on_dismiss` 返回 `DismissOutcome::Ignore`——不关链、不 replay；落在链外才按 D14 默认 `Replay`（显式 `outside_press(Consume)` 则消费）。Q2-03 后**不保留 6 点桥接几何**；`derived_safe_triangle` 由该层 anchor/rect 即时计算、**常驻不随 grace 过期**；grace 多边形由 300ms timer 维护（自 open/几何变化起算，到期只清 grace；M2-06）。
3. **D-P2-3 长菜单 anchor 变化一律关整链（M2-05 统一）**：`Menu::sync_anchor` 在 `AnchorChange::{Lost,Resized,Moved}` 时 `close_chain`（含内部 List 滚动导致的父行滚出视口）；`Scrolled` 仅 ContextMenu 的 `closing_on_scroll()` 显式关链。不再声称“内部滚动不误关整链/只关子级”；`scrolling_the_parent_row_out_of_view_closes_the_whole_chain` 断言整链关闭。
4. **D-P2-4 ContextMenu 右键入口 / trigger 绑定**：kit 提供 WindowRect 点锚（`open/open_at`）、节点锚（`open_on`，底边左端点）与键盘（Shift+F10/ContextMenu 键）入口；**`bind_trigger` 仅键盘入口，不绑定左键/右键，也不负责 trigger 可聚焦**（P1/P2 引擎没有“右键 action”抽象）。宿主负责 trigger 可聚焦与左键/右键绑定；右键调用 `ContextMenu::open(point)`/`open_on(node)`。登记为 P3 候选。
5. **D-P2-5 typeahead 不消费 text/IME 事件**：设计“IME composing 时不处理字符”由「只绑定 `KeyCode` + Modifiers 的可打印键 action、不监听 text event」实现；带 IME 的输入法组合不会进入 buffer（`a_text_event_never_feeds_the_typeahead_buffer`）。代价：软键盘/输入法直接投递的字符不参与 typeahead（桌面键盘路径覆盖）。
6. **D-P2-6 键盘焦点归属**：hover 只更新该层 active_item，不移动 DOM focus；第一次键盘事件发生在 hover-opened 更深层时，把 focus 移入该层（`Menu::focus`/keyboard target；`a_hover_opened_child_keeps_its_own_active_item_while_the_pointer_moves`）；`active_descendant` 只引用本层 item。
7. **D-P2-7 `MenuChain` 状态命名**：设计写 `PendingOpen(timer, level_id)/PendingClose(timer, level_id)`；实现为 `LevelState{Closed,Opening,Open,Closing}` + 每层 slot 的 `grace/exit/source`，timer 存放在 `Menu` 的 `pending_timer/grace_timer/close_timer`（仍按 level 绑定，语义等价；不引入全局 topmost）。
8. **D-P2-8 Radio 互斥范围 = 同一菜单层的 radio 列表**：无 group id；进入/选择 radio 时清空同层其它 radio 的 checked（`toggle_checked`），a11y role/checked 同步。设计未要求跨层 group。
9. **D-P2-9 菜单与 ContextMenu 的关闭/滚动语义差异显式化**：Menu 默认 `scroll_closes=false`（anchor Lost/Resized/Moved 一律关整链，含内部滚动；见 D-P2-3）；ContextMenu 构造时 `closing_on_scroll()`（外部页面滚动关整链）。这是 P1 矩阵的延续。
10. **D-P2-10 Group/Separator a11y role**：P2 时 Group=`Label`、Separator=`Separator`；**P3 修正（P3K-03）**：Separator=`A11yRole::Separator` → accesskit Splitter + hidden 本已满足设计 §4.10；Group 改 `A11yRole::Group`（菜单组头语义），无 accesskit 限制问题。

## 5. P1 P2 遗留关闭

| P1 §12.6 项 | P1 状态 | P2 结果 |
|---|---|---|
| P2-1 `MenuChain` 状态机 | 未开始 | **closed**：`menu_chain.rs` + 菜单集成（§2，含 mark_closing/reopen/revive、open 失败 reset、root Opening 跳过 anchor watch） |
| P2-2 5 点 safe triangle/grace | 未开始 | **closed**：5 点 + 300ms grace（自 open/几何变化起算并过期）+ grace 三角∪即时 safe triangle 内 Ignore（无桥接几何） |
| P2-3 面板行为（高亮/滚动/子菜单指示/分组/分隔线/勾选） | 部分 | **closed**：见 §2 P2-3 |
| P2-4 完整键盘表 + typeahead + checkable | 部分 | **closed**：见 §2 P2-4 |
| P2-5 链式 dismiss（祖先只关后代/Esc 逐层/tooltip 优先/D14） | 引擎侧完成，kit 几何 Ignore 属 P2 | **closed**：kit `on_dismiss` + safe triangle/grace Ignore；engine 语义不变 |
| P2-6 ContextMenu（WindowRect/右键/Shift+F10/子菜单共享链） | 部分 | **mostly closed**：WindowRect + Shift+F10/ContextMenu 键 + 2–3 级共享 MenuChain；`on_item_click` 传 check；右键/左键与 trigger 可聚焦由宿主负责（D-P2-4） |
| P2-7 3 级测试 + a11y 链 + gallery 演示 | 未开始 | **closed**：见 §2 P2-7 |
| A-25 残余 `active_descendant` kit 未声明 | P3 残余 | **closed**：引擎 `PopupA11y.active_descendant` + Menu `set_a11y`（D-P2-1） |
| `MenuChain` 未落地 / P1 只有 1 级子菜单 | — | **closed**：任意级链（受 `MAX_POPUP_DEPTH=32` 限制） |
| P1 键盘表缺 typeahead/PgUp/PgDn/Alt+F10 | — | **closed**：见 §2 P2-4 |

## 6. 测试与门禁基线（本机 Windows 11 / D3D12 / 125% DPI，2026-11-27）

| 门禁 | 结果 |
|---|---|
| `cargo test -p ui_core -p ui_kit` | **3847 passed / 0 failed / 2 ignored**（ui_core lib 2805 + 1 ignored、frame_record 8、gui_smoke 0 + 1 ignored、layout 4、popup_behavior 111、snapshot 14、demo 39、ui_kit lib 866）。P0 3554 → **+293**；P1 3780 → **+67**（ui_kit 801→866、popup_behavior 110→111、ui_core lib 2804→2805） |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | PASS |
| `cargo +nightly fmt --all -- --check` | PASS |
| `cargo check --workspace --all-targets` | PASS |
| `gallery --pixel-check` | **PASS**，`checks=94`（32 sampled + 16 background-residue + 46 static）、`close 6+6=12`、`damage_full_frames=0`，exit 0 |
| `gallery --debug-stack` | **PASS**，overlay-check 含 3 级链阶段（chain-open/stack-size/z-order/tier/parent-level2/level3/close/entries-zero）；menu 度量/`menu_frames`/`picker_frames` 运行间浮动（只断言 >0），exit 0 |

逐文件 `#[test]` 计数：引擎 `popup/tests.rs` 43、`popup/placement.rs` 34、`tests/popup_behavior.rs` 111、`a11y/popup_tests.rs` 22；kit `menu` 76、`menu_chain` 13、`context_menu` 20、`popup_panel` 18、`dropdown` 27、`select` 32、`dialog` 54、`tooltip` 28、`alert` 11、`color_picker` 80。

复跑命令：

```powershell
$env:CARGO_TARGET_DIR='E:\kytrix\target'
cargo test -p ui_core -p ui_kit
cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings
cargo +nightly fmt --all -- --check
cargo check --workspace --all-targets
cargo run -p ui_kit --example gallery -- --pixel-check
cargo run -p ui_kit --example gallery -- --debug-stack
```

## 7. 剩余 P3/待复核

| 项 | 状态 | 去向 |
|---|---|---|
| K1-11 `FadeSlide` 未启用（菜单仍 Fade/None） | P3 deferred | 设计 §12.6-7 / §13.7 |
| E-12 测试真实性残余（菜单指针测试命名、变异自检、pixel 覆盖、caret+offset 直接断言） | P3 | §12.6-6 |
| R2-01 受约束 popup 首帧内部滚动高度未 settle | P3 deferred | §12.6-9 |
| R2-02 anchor 矩阵关闭 reason 固定 `Programmatic` | P3 deferred | §12.6-10 |
| R2-04 无 `.confirm()` 自建 footer 初始焦点回退 Cancel | P3 accepted | §12.6-11 |
| R2-05 观察项（scrim 主题刷新/滚动条覆盖/首帧采样） | 观察 | §12.6-12 |
| A-26 混合 a11y 父链环；A-27 `MaxSize` 命名；A-31 `FrameRecord.popup_damage/z_changes` | P3 | §12.6-1/3/5 |
| 外部滚动语义宽于“祖先滚动”字面 | P3 文档精度 | §12.6-13 |
| D-P2-4 ContextMenu 未内建 secondary-button 自动绑定 / trigger 可聚焦由宿主负责 | P3 候选 | §13.4 |
| D-P2-10 Group/Separator a11y role（Separator vs Splitter、Group=Label） | **fixed（P3，P3K-03）** | §13.4/§14.2 |
| P2 收口 task-62 补齐的测试（恒真断言、derived_safe_triangle 隔离、failed-open `panel.open` 分支、ContextMenu `open_on`/重复绑定/`on_item_click` check、reconcile pointer-close） | **fixed（task-62）** | §13.1/§13.2 |
| D-P2-5 typeahead 不消费 text/IME 字符 | 行为限制 | §13.4 |
| M2-07 `set_popup_a11y` 对持续非法声明重复 `Ok(true)`（P3 观察） | P3 | §13.7 |
| P3-1 Dialog recenter 删除、P3-2 `Scale` 动画、P3-5 性能基线、P3-6 三端真实窗口、P3-4 gallery 真实像素/截图 | 未开始 | 设计 §6 P3 |
| menubar / MessageBox | 范围外 | D9/D11 |

**未做（原因）**：P2 不改引擎架构、不改宿主；P2 的“真实右键自动绑定”缺引擎事件抽象（D-P2-4）；P2 未引入新依赖、未加注释/allow/兼容层；`cargo test -p ui_core -p ui_kit` 之外未跑 workspace 测试（沿用 P0/P1 的用户限制；workspace check 绿）。

## 8. 与设计不一致 / 交接

- 设计 §4.8 的 `PendingOpen/PendingClose` 命名与实现的 `Opening/Closing` 差异（D-P2-7）。
- 设计 §4.8 “右键打开 ContextMenu”与实现的 `open(point)`+宿主绑定差异（D-P2-4）。
- 设计 §4.8 “IME composing 时不处理字符”由“不消费 text/IME 事件”实现（D-P2-5）。
- 设计 §4.8 “safe triangle 期间禁止兄弟替换”已实现；safe triangle/grace 内外部按下 Ignore（D-P2-2；Q2-03 后无桥接几何，符合 §4.8 “kit 在 on_dismiss 中用 chain geometry 返回 Ignore/Close/CloseChain”）。
- 设计 §4.10 `active_descendant` 由 `PopupA11y` 投影到 content 根（P1 的 surface 兜底删除），见 D-P2-1。
- 设计 §5.2/§7.3 已按上述 API/测试落点同步（设计 §13.8 有对照摘要）。
- 交接：`kit-gallery.md` §13.6 已同步 P2（3 级链 overlay-check 语义/`--debug-stack` 阶段/P2 完成状态与最终数字）；设计 §13.8 有对照摘要。
