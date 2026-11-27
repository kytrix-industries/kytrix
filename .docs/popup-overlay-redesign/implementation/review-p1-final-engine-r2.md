# P1 终审第 2 轮（收口复核）— review-p1-final-engine-r2

- 审查对象：worktree `E:\kytrix\.agents\worktrees\dsh-1d175414-1a41`，基线 `d9949aca`；P1 工作树（当前 `git status --short`：121 条，均为实现方改动；`.docs` 被 `.gitignore` 忽略）。
- 审查时间：2026-11-27 10:27–10:55（CST）。代码在审查期间仍在变动（`menu.rs` 10:40、`gallery.rs` 10:43），本报告所有结论以 10:55 的文件状态与门禁为准。
- 只读约束：未修改仓库文件；实验代码在仓库外 `E:\kytrix\target\p1-final\exp`（独立 crate，path 依赖指向本 worktree）。审查结束 `git status --short` 与开始时一致（仅实现方改动）。

## 0. 结论（先给 lead）

- **P0 = 0，P1 = 0。**
- **P2 = 1（本轮新 finding）**：**E-17 `raise_popup` 在“模态内含嵌套模态/已展开 Auto”时把外层 modal+scrim 整体抬到后代之上**，破坏“owner-modal 后代必须位于外层 modal/scrim 之上”的栈不变量；仓库外探针已复现（见 §3.1）。另有 2 条早审已登记 P2 未关闭（A-25 kit has_popup/active_descendant 声明、A-29 子菜单 a11y 默认父，见 §5），本轮未新开，若计入“全 0”则本次复核不通过。
- **P3 = 5**：E-12 测试真实性残余、E-13 §12/§4.5 文档残余、E-18 新增生产 `expect`、E-20 窄视口 Dialog 横向裁剪（观察项）、E-21 `PopupSpec.initial_focus` 公开字段不可达。
- **E-01…E-16 复核对：E-01/E-02/E-03/E-04/E-05/E-06/E-07/E-08/E-09/E-10/E-11/E-15/E-16 已 fixed；E-12/E-13/E-14 为 partially（残余见上）。**
- **当前不能写“P0/P1/P2/P3 全 0”，因此不能直接宣布可提交。** 修掉 E-17（建议直接拒绝“有可见 owner-modal 后代的 modal raise”，或把 scrim+owner-modal 后代按嵌套序一起抬高）并处理 A-25/A-29 的登记口径后，P0/P1/P2 才为 0；P3 五项属登记/边界项，若 lead 接受 P3 登记即可提交 P1，否则还需清 E-12/E-13/E-18/E-20/E-21。

## 1. 门禁（本机 Windows 11 / D3D12 / 125% DPI，最终代码状态）

| 命令 | 结果 |
|---|---|
| `cargo test -p ui_core -p ui_kit` | **PASS**：2805 + 8 + 0 + 4 + 109 + 14 + 39 + 798 = **3777 passed / 0 failed / 2 ignored**（ui_core lib 2805+1 ignored、frame_record 8、gui_smoke 0+1 ignored、layout 4、popup_behavior 109、snapshot 14、demo 39、ui_kit 798）。exit 0 |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | **PASS** |
| `cargo +nightly fmt --all -- --check` | **PASS** |
| `cargo check --workspace --all-targets` | **PASS** |
| `cargo run -p ui_kit --example gallery -- --pixel-check` | **PASS**：94 checks、residue 基线扫描 + 2 点注入自检、exit 0 |
| `cargo run -p ui_kit --example gallery -- --debug-stack --auto-close=9` | **PASS**：overlay-check menu `(6, 180.0, 127.5125)`、picker_open=true、picker_children=4、最终 entries=0，exit 0 |
| `AnchorEdge`/`match_anchor_width` 当前源码扫描 | **ZERO_REFS** |

证据日志：`E:\kytrix\target\p1-final\gates-r2c.log`；仓库外探针：`E:\kytrix\target\p1-final\exp`。

> 门禁说明：10:27 首轮门禁曾出现 `cargo test` LNK1104（另一个 agent 正运行 `gallery.exe` 占用目标文件）与一次 `--debug-stack` FAIL（当时 `gallery.rs` 尚未在 10:43 改为 `Anchor::WindowRect(node_screen_rect)` 锚定）。当前代码在无并发占用下全部通过；10:43 的 gallery 修复同时消除了“菜单 open 后首帧 AnchorWatch 把尚未滚入视口的 trigger 判 Lost 而立即 close”的门禁时序抖动。

## 2. E-01…E-16 逐条复核

| Finding | 状态 | 证据 / 判定 |
|---|---|---|
| E-01 模态 DismissPending/Closing 未阻断背景输入 | **fixed** | `topmost_modal` 改用 `state.is_visible()`（`popup/stack.rs:580-585`），关闭期 modal 仍参与输入 scope；`on_pointer_down` 的 modal 分支对 scope 外一律 `Consume`，`Explicit{outside}` 仅对 `is_dismissable()` 生效（`popup/dismiss.rs:319-342`）；新增 `a_closing_modal_swallows_background_presses_until_it_is_removed`、`a_closing_modal_keeps_background_hover_and_scroll_blocked`、`a_nested_modal_escape_sequence_closes_topmost_then_the_parent`、`closing_the_lower_modal_cascades_and_keeps_input_blocked_until_all_leave`。EXP2：淡出期背景按钮计数 **0**、背景 down hit=None，移除后恢复；嵌套/双模态序列测试均 PASS。 |
| E-02 Dialog 遮罩 100% 黑 + 主题 Scrim 死字段 | **fixed** | `ScrimSpec{color,density}`（`popup.rs:241-252`）；`scrim_paint = color.with_opacity(density)`（`app/popup.rs:36-41`）；Dialog 传主题 `components().dialog.backdrop`（Light 0.42/Dark 0.55）与 `density=1.0`；`with_opacity` 为乘法（`paint/color.rs:61-69`）。EXP1 scrim premul `[0,0,0,0.42]`；`the_scrim_uses_the_theme_backdrop_color_instead_of_opaque_black`、`scrim_paints_the_configured_color_and_density` PASS。 |
| E-03 PopupSurface 阴影被内容根裁剪 | **fixed** | `popup_content_style` 移除 `overflow_hidden`（`app/popup.rs:19-25`），panel/body 自己裁剪；`popup_content_does_not_clip_overflowing_children`、`the_panel_shadow_is_not_clipped_by_the_popup_content_root` PASS。EXP9/EXP13：popup 内容根无 ClipPush；面板外 1/2/4px 像素 `245/250/253`（对照阴影 `245/250/253`），不再全是 255。 |
| E-04 `constrained_size` 无 kit 消费者 | **fixed（垂直/主路径）**；横向见 E-20 | `PopupPanel::available_body_height` 读取 `constrained_size`（`kit/popup_panel.rs:313-327`）；Menu `sync_height`/`open`、Dialog `sync_scroll` 消费（`menu.rs:374-378,557-571`、`dialog.rs:787-847`）。`a_small_viewport_shrinks_the_menu_and_scrolls_its_last_row_into_view`、`a_tall_dialog_scrolls_its_content_and_keeps_the_footer_reachable` PASS；EXP6 屏幕坐标末行 `y=162..192` 在 popup `4..196` 内；EXP14 confirm `y=178..208` 在 panel `4..216` 内。§12.2 P1-4 已改写，原“无消费者”不实声称已关闭。 |
| E-05 外部删除 popup content 后 focus.owner 悬挂 | **fixed** | `PopupStack::remove` 清 `focus.owner`（`popup/stack.rs:333-335`）；`validate_owner` 对缺失 entry/节点清 owner/回退（`popup/focus.rs:164-219`）；`reap_orphan_scrims` 统一修复（`app/popup.rs:761-798`）。`removing_a_take_popups_content_node_restores_main_focus_and_tab`、`a_stale_focus_owner_is_repaired_when_the_next_tab_runs` PASS；EXP10 外部删 content 后 focus 回 `first`、Tab 到 `second`。 |
| E-06 失焦后 suppress_release 残留 | **fixed** | `cancel_pointer_session` 清 `suppress_release`（`app/input.rs:947`）；`window_focus_loss_does_not_swallow_the_next_background_click` PASS；EXP11 第一次背景点击即计数 1。 |
| E-07 Menu/ContextMenu 祖先滚动/resize 关链 | **fixed（加宽）** | 引擎 `popup_anchor_rect`/`popup_anchor_visible`/`window_scroll_epoch`（`app/popup.rs:713-726`、`app.rs:1304`）；kit `AnchorWatch/AnchorChange`（`popup_panel.rs:104-149`）：Menu/ContextMenu 在 Lost/Resized/Moved（及外部 Scrolled）关链，Dropdown/Select 在 Lost 关闭、可见滚动重锚定，Tooltip 重定位/滚出隐藏。`an_ancestor_scroll_closes_the_whole_menu_chain`、`scrolling_the_trigger_out_of_the_page_closes_the_dropdown`、`scrolling_a_visible_trigger_re_anchors_the_open_dropdown`、`scrolling_the_anchor_repositions_the_tooltip_and_hides_it_when_it_leaves_the_view`、`popup_anchor_queries_track_scrolling_and_visibility`、`window_scroll_epoch_advances_only_when_an_offset_changes` PASS。注意：对 Node 锚点，祖先滚动通常先命中 `Moved`（屏幕 rect 变了）而非 `Scrolled`，行为上仍关闭/重锚定；`Scrolled` 分支主要覆盖 WindowRect/未移动锚点。 |
| E-08 AT 无法激活 Menu/Dropdown/Select 选项 | **fixed（代码）** | `List::set_row_action` 把 row_action 挂到行节点（`list.rs:769-770,1142-1144`），ItemPanel `panel.on_activate -> activate`（`menu.rs:282-284`）；`effective_actions` 对可点击节点补 `Click`（`a11y/mapping.rs:327-331`）。`an_a11y_click_activates_the_menu_entry_it_targets`、`the_list_activation_path_activates_the_highlighted_entry`、Dropdown/Select 的 a11y click 测试 PASS；EXP12 AT-like click 后 activated=1、menu 关闭。**文档仍写 deferred，见 E-13。** |
| E-09 AnchorEdge/match_anchor_width 死字段 | **fixed** | 字段与导出删除；当前源码扫描 `AnchorEdge|match_anchor_width` = ZERO_REFS；workspace check PASS；`Anchor::Node` 全仓仅 `{node}`。 |
| E-10 ShowTooltip 被广告但未处理 | **fixed（收窄）** | `a11y/popup.rs:162` 只对 Tooltip 广告 `HideTooltip`，trigger 不再广告 `ShowTooltip`；`action.rs:85-101` 对无 handler 的 ShowTooltip 仍记 `UnhandledAction`；`a_tooltip_describes_its_trigger_and_at_hide_tooltip_closes_only_the_tooltip` PASS。 |
| E-11 IME caret 未含 paint_offset | **fixed（代码）**；测试缺口归 E-12 | `window/focus.rs:25-48` 在 `scroll::to_screen` 后累加祖先 `popup_transform.offset`；`scroll::to_screen` 只减 scroll offset，不会双计。当前没有“popup 内输入框 + paint_offset 时 caret”直接测试（现有 `a_child_popup_anchor_follows_the_parent_paint_offset` 测的是 anchor/焦点几何，不是 IME rect）。 |
| E-12 测试真实性/门禁覆盖 | **partially（P3 残余）** | 已修：`[dbg] eprintln` 删除、DPI 真的 1→2→1、slide 断言 FrameRecord Quad 位置跟随 `paint_offset`、新增关闭期输入/关闭顺序/初始焦点/锚点查询等强断言。仍残余：`popup_panel::an_at_least_panel_follows_its_anchor_width` 名不副实（实际测 `set_width`，不读 anchor）；P1 popup 套件无变异自检；gallery `--pixel-check` 只覆盖 ColorPicker，未覆盖 Dialog/Alert/Tooltip/Menu；E-03 阴影只有图元断言无像素断言；E-11 无 caret+paint_offset 直接断言。 |
| E-13 §12 回写与代码不一致 | **partially（P3）** | §12.2 P1-4/D-P1-21 已改正；仍有具体错误：§12.5 写 `3771 passed`/ui_kit `792`，实测 `3777`/`798`；§4.5 仍写“popup 内容根统一 overflow_hidden”，与 E-03/D-P1-30 矛盾（design:470）；§12.6-6 与 §12.10 E-08 仍写 deferred，代码已实现（design:1222,1352）；§12.10 E-14、A-34、B-11 仍声称“生产 expect 无残余”，但 `window.rs:324` 新增了生产 `expect`（见 E-18）；§12.5 的 components 测试计数（PopupPanel 18/Menu 25/Dialog 53 等）未逐项复核，至少总数已偏差。 |
| E-14 生产 expect 兜底 | **partially** | `app/popup.rs`、`popup/stack.rs` 的旧 expect 已清；但 P1 在 `Window::new` 新增 `PopupStack::install(...).expect("a freshly created window root always accepts the popup root")`（`window.rs:323-324`）。见 E-18。 |
| E-15 立即关闭 modal 的后代顺序祖先先于子 | **fixed** | `app/popup.rs:565-581` 按 `Reverse(chain_depth)` 排序后通知/关闭；`immediate_modal_close_notifies_owned_descendants_children_first` PASS。 |
| E-16 raise_popup(modal) 不重排 scrim z | **fixed（单 modal）**；嵌套 modal 见 E-17 | `stack.rs:248-272` 先把 `scrim_of(modal)` 放到 base，再放子树；`raising_a_modal_moves_its_scrim_to_the_adjacent_z_slot` PASS；EXP16 普通 modal+scrim raise 后 z 相邻（(3,4)）。 |

## 3. 新代码对抗性复扫 / 新 finding

### 3.1 E-17（P2）`raise_popup` 把嵌套 modal 的外层抬到内层之上

- 位置：`crates/ui_core/src/popup/stack.rs:248-272`（`raise` 只处理 `subtree(id)` 与自身 scrim；`owner_modal_descendants` 在 750-766 行只被关闭路径使用，且该函数会返回 Auto/Hint/Modal 后代，不只嵌套 modal）。
- 复现（仓库外探针 EXP18）：外层 Modal A（有 scrim）→ 从 A 内容节点作 opener 打开内层 Modal B（owner_modal=A，带 scrim）→ `app.raise_popup(A)`。z 顺序由 `[(scrimA,1),(A,2),(scrimB,3),(B,4)]` 变为 `[(scrimB,3),(B,4),(scrimA,5),(A,6)]`：**A 的 scrim 盖住 B，A 自身也在 B 之上**，`topmost_modal()` 变为 A，B 的内容被当作 scope 外输入阻断。同理，A 内打开的 Select/Dropdown/Tooltip（owner_modal=A 的 Auto/Hint）也会在 raise A 后落回 A 之下。
- 影响：破坏“owner-modal 后代必须位于 modal 之上、scrim 只盖住其下方内容”的栈不变量；`PopupPanel::open` 对“已打开的 Dialog 再次 open”会调用 `raise_popup(existing)`，若该 Dialog 上叠有嵌套 Dialog/已展开下拉，即可触发。属于引擎公共 API 正确性缺口，非默认 gallery 路径。
- 修法建议：`raise` 时若 `owner_modal_descendants(id)` 非空，按嵌套顺序把外层 scrim → 外层 → 每个后代的 scrim → 后代一起抬到 `next_z` 之上（scrim 只在对应 modal 前一位）；或更简单：**拒绝 raise 具有可见 owner-modal 后代的 modal**（返回强类型错误），并在 `PopupPanel::open` 侧对 modal 不盲目 raise。补回归：`raising_a_modal_keeps_owner_modal_descendants_above_it`（含嵌套 modal 与 Auto 两类）。
- 证据：探针输出见 §4；`cargo test -p ui_core -p ui_kit` 全绿说明当前没有覆盖该矩阵。

### 3.2 E-18（P3）新增生产 `expect` 与文档“零生产 expect”声明不符

- 位置：`crates/ui_core/src/window.rs:323-324`：`PopupStack::install(id, &mut tree).expect("a freshly created window root always accepts the popup root")`，由 P1 新增（基线 `Window::new` 无 popup 安装）。
- 影响：窗口构造路径存在 panic 分支；虽然“新窗口 root 必存在”是引擎不变量，但 AGENTS 质量口径与本轮 E-14 的“无生产 unwrap/panic”冲突，且 §12.10-E-14/§12.7-A-34/B-11 明确写“剩余命中均位于 `#[cfg(test)]`/无残余”（design:1290,1301,1358），与代码不符。
- 修法建议：让 `PopupStack::install` 对新窗口 root 走不可失败构造（或 `Window::new -> Result` 传递错误，而非 expect）；若接受该 invariant expect，则修正文档口径并在 §12.10-E-14 标注例外。全局扫描确认其余新增 expect 均在 `#[cfg(test)]`/test-support（`ui_kit` tests、`app/tests.rs`、`window/tests.rs` 等），`app/popup.rs`/`stack.rs` 已无生产 expect。

### 3.3 E-19（P3，并入 E-13）文档仍有三处与最终代码矛盾

1. `design:470` §4.5：“popup 内容根统一 `overflow_hidden`”；E-03/D-P1-30 已移除，实际 panel/body 才裁剪。
2. `design:1222`/`design:1352` E-08 仍 `deferred`，实际已接 `List::on_activate` + row_action + a11y Click，并有测试。
3. `design:1181` §12.5 测试计数 `3771`/ui_kit `792`，实测 `3777`/`798`；E-14/A-34/B-11 的“生产 expect 无残余”与 `window.rs:324` 不符。
（lead 已说明 E-08 文档稍后补；此处只记录当前快照，最后提交前需再扫一次。）

### 3.4 E-20（P3）窄视口 Dialog 横向裁剪

- 复现：320×220 视口，DialogBuilder 默认 `width(384)` 打开高内容。EXP14：panel `x=4..316`，confirm `x=12..396`、宽 384，**右侧 80px 被 panel 裁掉**；垂直方向已通过 `sync_scroll` 落到 `y=178..208` 可达。
- 影响：仅 <~400 逻辑宽的窗口；按钮仍可点到左侧可见部分，但右侧圆角/内边距/文本可能被裁。属边界视觉项，非功能阻断。
- 建议：Dialog 内容宽度取 `min(configured, available)` 或 footer 按钮行 `width_full/flex_shrink`；补 320 宽下 confirm bounds 不出 panel 的断言。当前 `a_tall_dialog_scrolls...` 用 `width(280)` 未覆盖默认宽 384 的窄窗。

### 3.5 E-21（P3）`PopupSpec.initial_focus` 在公开 `open_popup` 路径上不可达

- 位置：`popup.rs:263` 的公开字段、`app/popup.rs:284`/`326` 读取；`open_popup` 的 content 是 `FnOnce(&mut App, WindowId, NodeId)`，调用方在调用前无法知道 closure 将创建的内容节点，而 `focus::initial_focus_node` 明确忽略内容子树外的节点（`popup/focus.rs:316-358` 单测）。
- 现状：全仓生产代码没有 `PopupSpec.initial_focus` 写入（只有测试结构体字面量与 `set_popup_initial_focus` setter）；K1-05 的 Dialog 实际走 `App::set_popup_initial_focus(id, confirm)`。字段只在“已有节点先于 open 存在”时可用，而 popup content 节点在 open 内才创建，因此公开字段实际不可用。
- 影响：新增公开 API 语义空洞，且违反 AGENTS“生产结构不得定义未使用字段”；若未来调用方设置该字段会静默回退到 `InitialFocus` 策略，难以发现。
- 建议：二选一：删除 `PopupSpec.initial_focus`（K1-05 只用 setter，文档 §12.4 同步删）；或提供“预构建/已存在 content 节点”的 open 路径并补真实集成测试（`spec.initial_focus = Some(second)` 打开后焦点落在 second）。当前只有纯函数单测 `a_requested_initial_focus_node_wins_over_the_policy`。

### 3.6 E-12/E-13 残余（不新增编号）

见 §2 E-12/E-13；这两项原级别就是 P3，按 lead 的“全 0”口径仍未关闭。

## 4. 仓库外探针关键输出（当前代码，`E:\kytrix\target\p1-final\exp`）

```
EXP1 scrim premul rgba=[0.0, 0.0, 0.0, 0.42] (theme Light 0.42)
EXP2 DismissPending: is_closing=true; during-fade background clicks=0, down_hit=None
EXP3 260x200 menu: popup_hit=200x192, panel=200x192, panel_exceeds_popup=false
EXP6 End: highlighted=Some(11); row screen y=162..192 inside popup y=4..196 -> visible=true
EXP9 panel-outside pixels y=213+1/+2/+4 = 245/250/253 (shadow visible)
EXP10 external remove content: focused Some(first), Tab -> second
EXP11 focus loss: first background click already 1 (no swallow)
EXP12 AT-like Click on MenuItem: handled=true, activated=1, menu_open=false
EXP13 ancestor-clip child shadows remain 255; self-clip panel shadow 245 (expected)
EXP14 320x220 Dialog: popup=312x212; confirm y=178..208 reachable; confirm x=12..396 vs panel 4..316 (right clipped)
EXP15 initial_focus setter: before=content, after=second, outside node rejected Ok(false); anchor visible=true
EXP16 scrim/modal z before=(1,2) after raise=(3,4) adjacent=true
EXP17 menu first frame already 200x192; dialog first-frame confirm y=337 (before host dialog.sync; not a finding)
EXP18 before raise=[(scrimA,1),(A,2),(scrimB,3),(B,4)]
EXP18 after  raise=[(scrimB,3),(B,4),(scrimA,5),(A,6)]   <-- E-17
```

## 5. 早审 A/B 残留复核（摘要）

- 已 closed：A-01…A-24（其中 A-09/A-20 已由 E-04/E-09 最终收口）、A-28、A-30、B-01…B-15（B-13 由 `initial_focus` + setter 收口）。
- 仍按已登记 P2/P3 保留（非本轮新 finding）：
  - **A-25**（kit `has_popup`/`active_descendant` 声明）文档仍记 P2；本轮 E-08 解决的是“AT 激活”，未解决 active descendant/子菜单父。
  - **A-26**（混合 a11y 父链环）待复核；`a11y/tree.rs` 仍有 `patch_parent_cycle` 测试。
  - **A-27**（两个 `MaxSize` 命名未收敛）部分；`Window::set_max_size` 已做失效缓存。
  - **A-29**（子菜单 a11y 默认父仍是父 popup 内容根而非 menuitem）仍 P2；`a11y/popup.rs` 的 `resolve_logical_parent` 仍按 `a11y_parent → 父内容根 → opener`，kit 未写 `a11y_parent`。
  - **A-31/A-32/A-33/B-16/B-17** 仍 P3（debug 命名、a11y O(n)、变异自检、settle 触发 `layout_ran`、测试细节）。
- **A-34/B-11 的“无生产 expect/无残余”应改口径**，见 E-18。
- K1-07/K1-08b/e/f/K1-09/K1-11/K1-12/Q1-04 属 kit/质量报告范围，本报告未重开；仅确认本轮未新增同类问题。

## 6. 结论与建议

1. **E-01…E-16**：13 项 fixed，E-12/E-13/E-14 partially（均为 P3 级残余）；E-04/E-03/E-02/E-01 的原始 P0/P1 阻断面已消失。
2. **新 finding E-17 P2**：嵌套 modal 的 `raise_popup` 栈不变量破坏；这是本轮唯一新开的 P2，建议提交前修（拒绝或整组抬升），并补回归；早审 A-25/A-29 两条 P2 仍按已登记状态保留。
3. **P3 五项**：E-12 残余（测试命名/像素覆盖）、E-13 文档矛盾（§4.5/E-08/计数/expect 声明）、E-18 生产 expect、E-20 窄窗 Dialog 横向裁剪、E-21 `PopupSpec.initial_focus` 公开字段不可达。均非运行时崩溃/数据损坏，但按“全 0”口径未关闭。
4. **可提交判定**：**当前不写“可提交 P1”**；修 E-17 + 更新 §12/§4.5/E-08/E-14 文案后，P0/P1/P2 = 0；若 lead 接受 E-12/E-18/E-20/E-21 四项 P3 登记（并确认 A-25/A-29 已在 P2 阶段登记），则可提交 P1，否则还需清 P3。
