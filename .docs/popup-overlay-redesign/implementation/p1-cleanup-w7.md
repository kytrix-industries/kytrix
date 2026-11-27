# P1 W7 收口记录：删除旧 overlay/layer 机制 + 测试重写 + gallery 迁移（P1-11 / P1-12）

- 状态：完成（本 worktree 未提交，由 lead 统一提交）
- 范围：task-38；基线为 P1 引擎 popup/kit 全组件迁移/W6 宿主迁移完成后的工作树
- 结论：**旧 overlay/layer 机制与旧引擎 overlay API 已全部删除，浮层只剩引擎 popup 栈 + kit `PopupPanel` 一套机制**；inventory §5.3 组合模式 `rg` 零命中（保留词见 §5）；`cargo test -p ui_core -p ui_kit`、`cargo test -p widget`、workspace check、clippy、fmt、gallery `--pixel-check`、gallery `--debug-stack` 全绿

> **终审后注记（2026-11-27）**：本文件是 **W7 时点快照**（08:47），后续终审修复轮变更以下数据，正文保留历史值不改：
> - `--pixel-check` 由 **93 checks → 94 checks**（`sampled-animation-floor` 由 2 条累计下界改为 **open/close#1/close#2 三段**；idle 期 `background-residue` 执行口径变化，见设计 §12.3-9/§12.5）；
> - `cargo test -p ui_core -p ui_kit` 由 3734/ui_kit 773 → **3780 passed / ui_kit 801**（设计 §12.5）；
> - `--debug-stack` 现在**自行退出 0/3**，`--auto-close` 仅抬升 ≥30s 看门狗（可省略），旧 `--dump-overlay` exit 2。
>
> 最终基线以设计 **§12.5** 与 `.docs/gui-migration/spec-deltas/kit-gallery.md` **§13.5** 为准。

---

## 1. 文件级删除

| 文件 | 说明 |
|---|---|
| `crates/ui_kit/src/kit/overlay.rs` | 旧 `Overlay`/`OverlayOptions`/`OverlaySurface`/`OverlayA11y`/`OverlayError`/`AnimationRouter`/旧 placement 类型 + 37 个测试，整文件删除 |
| `crates/ui_kit/src/kit/layer.rs` | 旧 `Layer`/`Band`/`LayerSlot`/`Entry` + 14 个测试，整文件删除 |
| `crates/ui_core/src/app/overlay.rs` | 旧 `create_overlay/close_overlay/place_overlay/bring_overlay_to_front/overlays/dismiss_overlay/resolve_overlay_placements`，整文件删除 |
| `crates/ui_core/src/interaction/overlay.rs` | 旧 `is_in_subtree/enclosing_overlay/in_modal_scope/outside_click_target/topmost/focus_scope_root` + 10 个测试，整文件删除 |

`crates/ui_kit/src/kit/mod.rs` 删除 `pub mod layer;`/`pub mod overlay;`；`crates/ui_core/src/app.rs` 删除 `mod overlay;`。

## 2. 类型 / API 级删除

- **kit**：`kit::overlay::{Overlay, OverlayOptions, OverlaySurface, OverlayA11y, OverlayError, AnimationRouter, Scrim, Anchor, anchor_rect, DismissPolicy, Side, Alignment, Placement, PlacementConfig, resolve_origin}`、`kit::layer::{Layer, Band, LayerSlot, Entry}`、`kit::test_support::overlay_wrapper`（13 处组件调用方已在 W2–W5 迁移，均已不存在）。
- **ui_core 公共面**：`App::{create_overlay, close_overlay, place_overlay, bring_overlay_to_front, overlays, dismiss_overlay}`、`overlay_style`、`interaction::{Overlay, OverlayDismiss, OverlayOptions, OverlayPlacement, OverlaySlot, AnchorSide, resolve_placement}`、`interaction::overlay` 模块、`NodeInteraction.overlay` 字段、`NodeTree::{overlay_containers, note_overlay_container, overlay_slot}`。
- **引擎内部耦合**：`node/tree/{arena,content,traverse}` 的 overlay 容器/槽位维护；`app/frame.rs` 的 `resolve_overlay_placements` 调用；`app/input.rs` 的旧 outside-click `remove_node` 兜底与 `Action::Dismiss` 旧兜底、仅旧 overlay 使用的 `revalidate_hover`；`app/window_set.rs` 的旧 overlay slot 捕获 / 旧 focus scope 清理 / 旧 `restore_focus`；`app/focus.rs` 全部旧 overlay scope 回退（非 popup 路径的 `focus_scope_root/in_modal_scope/focusable_in_scope` 分支）；`interaction/focus.rs` 的 `in_scope`/`focusable_in_scope` 与 `next_focusable` 的 scope 参数；`window/a11y.rs` 旧 scope 回退；`a11y/tree.rs` 的 `overlay_slot` modal 回退与旧 modal scope 回退；`a11y/action.rs` 旧 scope 回退；`a11y/mapping.rs` 的 `overlay_slot` role 回退。

删除后的接管关系：

| 旧能力 | 新真值 |
|---|---|
| 旧 overlay pin_tail/层序 | 引擎 `popup_root` 预建 + `PopupStack::sync_order`（`reorder_children` 不触发 structure epoch） |
| 旧 `focus_scope_root/in_modal_scope`（含 opener 链） | `PopupStack::{node_in_popup_scope, node_in_focus_scope}` + `owner_modal` + `popup::focus` |
| 旧 `outside_click_target/remove_node` | `popup::dismiss::on_pointer_down`（DismissPolicy 矩阵 + Auto 根/opener toggle/Consume 显式 opt-in） |
| 旧 `Action::Dismiss` 兜底 | `App::dismiss_topmost`（Esc 走 `popup::dismiss::escape`；无 popup 时返回 false） |
| 旧 a11y overlay 回落 | `a11y::popup::project` 的 `A11yPopupOverlay` 投影（role/label/modal/expanded/haspopup/owns/described_by/a11y_parent） |
| 旧 `App::modal_scope` | 保留公共 API，返回值改为“最上层 modal popup 的 content 节点（`PopupStackSnapshot` entry.node）” |

## 3. 测试重写 / 删除映射（不静默减少）

### 3.1 `crates/ui_core/src/app/interaction_tests.rs`

保留并改写（6 个，断言等价迁移到新 popup）：

| 旧测试 | 迁移后 |
|---|---|
| `anchor_placement_converges_with_a_root_border` | `open_popup` + `Anchor::Node{edge:Bottom}` + `PlacementSpec{side:Bottom,gap:4}`；断言 `popup_rect().hit` 原点 (100,124) 且 2 帧后无 wake（settle 收敛） |
| `anchored_above_overlay_sits_above_the_anchor` | 同上 `Top` 向；断言 rect (100,66,60×30) |
| `dismiss_action_closes_the_topmost_overlay_including_explicit` | 改名 `dismiss_action_closes_the_topmost_popup_including_explicit`；`DismissPolicy::Explicit{escape:true}` + `Action::Dismiss` 关栈顶、二次 Dismiss=false、`popup_rect` 消失 |
| `closed_window_returns_unknown_window` | 关闭窗口后 `open_popup` 返回 `PopupError::UnknownWindow`；`popup_stack` 为 None |
| `a_modal_opened_without_any_focus_fills_the_first_focusable_node` | `PopupTier::Modal` + `FocusPolicy::Trap{initial:FirstFocusable}`：打开瞬间即聚焦首个可聚焦节点，Enter 派发一次 |
| `enter_cannot_activate_a_background_button_behind_a_modal` | 新 popup modal：背景 Enter/程序化动作被拒、模态内可派发；`App::modal_scope` 返回 popup content 节点、Dismiss 后清空 |

删除（9 个，覆盖已由专门 popup 测试承接）：

| 旧测试 | 覆盖去向 |
|---|---|
| `overlay_is_last_and_closes_on_outside_click` | `tests/popup_behavior.rs::outside_press_replays_the_click_to_the_background`（外部点击关闭）；`popup/tests.rs::{the_popup_root_is_the_last_root_child, sync_order_mirrors_the_stack_order_in_the_popup_root}`（层序/根位置）；`popup/tests.rs::the_popup_root_never_wins_the_hit_test`（命中语义） |
| `overlay_outside_click_consumes_the_whole_press` | `popup_behavior::outside_press_consume_is_explicit_opt_in`（Consume 显式 opt-in + 整次按压消费） |
| `a_modal_overlay_traps_focus_and_restores_it_on_close` | `popup_behavior::{opening_a_take_popup_moves_focus_and_closing_restores_it_once, dialog_tab_cycles_inside_the_modal_scope, default_button_wins_the_trap_initial_focus}`；改写后的 `enter_cannot_activate...`（modal scope 查询） |
| `a_popup_opened_from_a_modal_receives_clicks` | `popup_behavior::an_owner_modal_popup_stays_interactive_and_closes_from_inside_the_dialog` |
| `a_popup_opened_in_the_same_callback_as_its_modal_receives_clicks` | `popup_behavior::a_popup_opened_inside_an_owner_modal_popup_keeps_the_modal_scope` |
| `a_popup_opened_without_any_focus_still_receives_clicks` | `popup_behavior::an_owner_modal_popup_stays_interactive_and_closes_from_inside_the_dialog`（owner_modal 真值不依赖既有焦点） |
| `an_overlay_a11y_role_is_declared_explicitly_and_the_modal_scope_is_queryable` | `a11y/popup_tests.rs::{a_modal_popup_marks_the_dialog_hides_the_background_and_hides_the_scrim, a11y_popup_projection_never_breaks_the_accesskit_contract}`；`modal_scope` 查询断言迁入改写后的 `enter_cannot_activate...` |
| `a_modal_filters_background_hover_so_tooltips_cannot_open_over_it` | `popup_behavior::{a_modal_blocks_background_hover_and_scroll, a_background_tooltip_is_rejected_while_a_modal_is_open, opening_a_modal_closes_background_hints}` |
| `a_modal_blocks_background_clicks_and_restores_interaction_on_close` | `popup_behavior::{a_dialog_blocks_background_openers, a_modal_blocks_background_hover_and_scroll}`；gui_smoke 场景 ⑤（重写后仍覆盖背景点击被消费 + Esc 关闭 + 焦点还原 + 像素） |

### 3.2 其它测试文件

| 旧位置 | 处置 |
|---|---|
| `crates/ui_core/src/test_support/a11y.rs::a_modal_scope_change_is_delivered_as_a_legal_incremental_update` | 改写：`open_popup`（Modal + Trap + `PopupA11y{Dialog}`）+ a11y 投影断言全部保留（背景 hidden/恢复、wrapper role、modal、增量合法）；关闭改用 `begin_close_popup` |
| `crates/ui_core/src/test_support/gui_smoke.rs` 场景 ⑤ | 改写：`PopupTier::Modal + Trap{FirstFocusable, restore:Opener}` + `opener`；`overlays()` 断言改 `popup_stack().entries`；Tab 回绕/背景 set_focus 拒绝/背景点击消费/Esc 关闭与焦点还原/像素断言均保留 |
| `crates/ui_core/src/a11y/action.rs::a11y_actions_outside_the_modal_scope_are_rejected` | 改写：`open_popup` modal + 内容内 Slider；作用域外 SetValue/Increment/ScrollDown 被拒 + 诊断，作用域内生效，断言不变 |
| `crates/ui_core/src/a11y/tree.rs::an_overlay_wrapper_is_a_container_unless_it_declares_a_role` | 删除；覆盖去向 `a11y/popup_tests.rs::{a_popup_chain_nests_the_a11y_tree_and_keeps_the_physical_tree_flat, a11y_popup_projection_never_breaks_the_accesskit_contract}`（popup content 默认 GenericContainer、显式 role 生效） |
| `a11y/tree.rs::a_modal_overlay_hides_the_background_from_a11y` | 删除；覆盖去向 `a11y/popup_tests.rs::a_modal_popup_marks_the_dialog_hides_the_background_and_hides_the_scrim` |
| `a11y/tree.rs::a_popup_opened_inside_the_modal_stays_visible_to_a11y` | 删除；覆盖去向 `popup_behavior::a_popup_opened_inside_an_owner_modal_popup_keeps_the_modal_scope` + `a11y/popup_tests.rs::a_modal_dialog_and_its_inner_popup_accept_at_actions` |
| `interaction/focus.rs::a_modal_scope_traps_the_tab_order_inside_the_overlay` | 删除；覆盖去向 `popup_behavior::dialog_tab_cycles_inside_the_modal_scope` + `popup/focus.rs` 的 popup 作用域实现测试 |
| `interaction/focus.rs::no_modal_means_no_restriction` | 删除；无 scope 的 Tab 序由同文件 `tab_order_is_dfs_preorder_and_skips_plain_nodes`/`tab_wraps_both_directions` 继续覆盖 |
| `crates/ui_kit/src/kit/overlay.rs`（37 个测试）、`layer.rs`（14 个测试） | 随文件删除；行为已由 W2–W5 迁移到组件测试与引擎 popup 测试：首帧 4 条（首可见帧==稳定帧/无宿主 sync 也显示）→ `popup_behavior::{opening_a_popup_settles_geometry_in_the_first_frame, text_dominated_height_is_stable_on_the_first_frame}` + 组件侧 `{dropdown,select,dialog,context_menu,menu}::the_first_visible_frame*`；open/close/幂等/带宽/层序 → `popup_behavior` 与 `popup/tests.rs`；外部点击/dismiss 矩阵 → `popup_behavior::{outside_press_*, escape_*, modal_outside_*}` |

## 4. Gallery 迁移（只改调试/检查通路，不改组件行为）

- **`--dump-overlay` → `--debug-stack`**：打印 `popup_stack` 快照（每个 entry：`id/tier/parent/z/state/hit/paint/a11y(由 popup_rect)/owner_modal/focus/node/paint_offset/constrained_size`），并在 `menu-open/menu/picker-open/picker/final` 五个节点各打印一次；删除旧节点树 dump（`dump_overlay_nodes`）。
- **`overlay-check`（新 popup 语义）**：菜单/Picker 打开后断言栈条目数=1、菜单 tier=Auto、菜单度量（children>0、content>0、thumb>0）、**首可见帧 rect==稳定帧 rect**（由每帧 `on_frame` 采样，仅采 `PopupState::Open` 的已 settle rect）、关闭动画完成后的下一轮断言**栈无残留**、结束时栈为空；任一失败 exit 3。真实窗口 gate：`gallery --debug-stack --auto-close=9` → PASS（menu 度量 `(6, 180.0, 127.5)`，最终 entries=0）。
- **`--pixel-check`**：主体已在 W3/W5 迁到 `PopupPanel/popup_state/popup_node`，本轮清理旧 API/迁移说明字符串，并新增 **`sampled-animation-floor`** 两条断言（open 采样 ≥4、close 采样 ≥3，防止动画常量变小导致采样静默缩水）。保留命令名/退出码（0 PASS / 3 FAIL）与全部既有断言（93 checks）。**终审后为 94 checks（floor 改三段），见文首注记。**

## 5. `rg` 零引用结果

环境无独立 `rg` CLI，使用 VS Code 自带 `ripgrep 15.0.0`（`resources/app/node_modules.asar.unpacked/@vscode/ripgrep-universal/bin/win32-x64/rg.exe`），命令口径与 inventory §5.3 一致：

```powershell
rg -n --glob '*.rs' 'kit::overlay|overlay::\{|OverlayOptions|OverlaySurface|OverlayA11y|OverlayError|AnimationRouter' crates   # 0
rg -n --glob '*.rs' 'kit::layer|layer::\{|\bLayer::new|LayerSlot|Band::' crates                                                # 0
rg -n --glob '*.rs' 'create_overlay|close_overlay|place_overlay|bring_overlay_to_front|resolve_overlay_placements|dismiss_overlay|\.overlays\(' crates  # 0
rg -n --glob '*.rs' 'OverlaySlot|OverlayDismiss|OverlayPlacement|overlay_style|note_overlay_container|overlay_containers|overlay_slot' crates             # 0
rg -n --glob '*.rs' 'overlay_wrapper' crates                   # 0
rg -n --glob '*.rs' 'MenuMode::ListBox' crates                 # 0
rg -n --glob '*.rs' '\.layer\(\)|\.animation\(\)|animation_router|services\.layer' crates  # 0
rg -n 'dump-overlay|dump_overlay' crates                       # 0
```

排除/保留词说明：

- `Layer::new` 原始模式会命中 `crates/ui_core/src/render/macos/swapchain.rs` 的 `CAMetalLayer::new()`（macOS 纹理 layer，保留）；判据改为 `\bLayer::new` 后为 0。
- `Overlay` 单词保留：`A11yPopupOverlay`（新 a11y popup 投影，`a11y/tree.rs`/`a11y/popup.rs`）、`DamageOverlay`（`test_support/diagnostics.rs`，与浮层无关）。
- `ItemPanel` 名称保留：它是设计 §8 允许的“PopupPanel 的 menu 视图”（`pub(crate)`，`menu.rs`/`dropdown.rs`/`select.rs`），旧形态的 `Overlay/Layer/band/slot/modal` 字段已全部删除；因此 `ItemPanel` 不是本轮的零引用判据（inventory §5.3 的字面模式会命中新形态）。
- `Band::` 的唯一旧文字命中（`kit/dialog.rs` 测试消息）已改写为等价语义，不再残留旧符号名。

## 6. 门禁结果（本机 Windows 11，`CARGO_TARGET_DIR=E:\kytrix\target`）

| 门禁 | 结果 |
|---|---|
| `cargo test -p ui_core -p ui_kit` | **3734 passed / 0 failed / 2 ignored**（W7 时点：ui_core lib 2804 + popup_behavior 92 + 8 + 4 + 14 + 39；ui_kit 773。终审后为 3780/ui_kit 801，见文首注记） |
| `cargo test -p widget`（自证） | 182 passed / 0 failed |
| `cargo check --workspace --all-targets` | PASS |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | PASS |
| `cargo +nightly fmt --all -- --check` | PASS |
| `cargo run -p ui_kit --example gallery -- --pixel-check` | **PASS**，checks=93，sampled_frames=32，open_animation_frames=8，close_animation_frames=12，damage_full_frames=0，first_visible==stable=(496,91,300×375)，exit 0（W7 时点；终审后 checks=94、close=6+6，见文首注记） |
| `cargo run -p ui_kit --example gallery -- --debug-stack --auto-close=9` | **PASS**（overlay-check，最终 entries=0，exit 0；终审后 `--debug-stack` 自行退出，auto-close 降级为 ≥30s 看门狗） |

## 7. 偏差 / 风险 / 待 lead 裁决

1. **`sampled-animation-floor` 口径**：引擎 `settle` 在首帧就把 `PopupState::Opening` 置为 `Open`（settle.rs:369），因此“按 Opening 状态计数”恒为 0；实现改为统计 `pixel_check_settle`/`pixel_check_close` 期间的**已提交帧数增量**（open=8、close=12）。该口径仍是严格的“动画存在性下界”：动画被删除/缩短到 <4 帧/<3 帧时门禁必红；但它不声称每帧恰为动画帧（关闭后卸载帧也计入下界）。
2. **真实窗口首可见帧采样**依赖 gallery 的 `on_frame` hook（每帧读 `popup_stack`），只在 `--debug-stack` 流程启用；headless 的严格首帧断言仍在 `--pixel-check`（first_visible==stable）与 `popup_behavior`/组件测试中。
3. **`App::modal_scope` 保留**（tooltip/menu/widgets 测试仍消费），语义改为 popup content 节点；旧 `interaction::overlay` 的 opener 链 modal scope 语义由 `PopupStack.owner_modal` 承接。
4. `interaction/focus.rs` 的 `next_focusable` 去掉了 scope 参数（无 modal 旧机制后恒等价），相关测试保留无 scope 路径。
5. 删除旧测试 14 个（interaction_tests 9 + a11y/tree 3 + interaction/focus 2），另有 kit overlay/layer 的 51 个测试随文件删除；改写 9 个（interaction_tests 6、test_support/a11y 1、gui_smoke 场景 1、a11y/action 1）。每个删除项均在 §3 给出覆盖去向，无静默减少。当前 `cargo test -p ui_core -p ui_kit` 总 3734 passed，高于 P0 基线 3554（P1 新增 popup 引擎/行为测试为净增量）。
6. **外部 `remove_node` 删除 popup content 节点的语义收窄**：`window_set` 不再执行旧 overlay 的 opener 焦点还原与 modal scope 标记；改由 `reap_orphan_scrims` 修剪栈条目、`popup::focus::validate_owner` 在下一输入帧修复失效 owner。受支持路径是 `close_popup`/`PopupPanel::close`（组件与宿主均已使用）；直接删除 popup 节点的调用方（仅测试支持/回滚路径）不再保证焦点回 opener。`popup/tests.rs` 的 transient 删除与 orphan scrim reaping 用例已覆盖栈修剪，未覆盖旧式焦点还原（该语义已随旧机制删除，不做兼容层）。
