# P1 实施汇总（供审查）：引擎 popup 内核 + kit 全量迁移 + 宿主迁移

- 状态：**实施完成，未提交**（worktree `dsh-1d175414-1a41`，P0 基线提交 `d9949aca`，由 lead 统一提交）
- 详细回写：`.docs/popup-overlay-redesign-design.md` **§12**（P1 回写；§11 为 P0 回写）
- 关联记录：`implementation/p1-migration-inventory.md`（迁移清点）、`implementation/p1-cleanup-w7.md`（P1-11/P1-12 收口）、`implementation/review-p1-engine-behavior.md`、`implementation/review-p1-engine-api.md`（P1 早审）
- 本文用途：把「做了什么 / 偏差在哪 / 最终 API / 实测基线 / 遗留项」压缩成一页供终审；数字均为 2026-11-27 本机实测（Windows 11 / D3D12 / 125% DPI）。

---

## 1. 范围与规模

| 项 | 值 |
|---|---|
| 代码 diff（排除 `.docs`；**总行数含空行**，与 `git diff --stat` 一致） | **115 tracked 文件，+8671 / −9410**（`.docs/gui-migration/spec-deltas/kit-gallery.md` 是 git 跟踪文件；含它则全量 tracked 为 116/+8692/−9415） |
| 删除文件 | 4 个 / **4350 行**：`kit/overlay.rs` 2871、`kit/layer.rs` 964、`ui_core/app/overlay.rs` 184、`ui_core/interaction/overlay.rs` 331 |
| 新增路径 | **7 条 untracked 路径 / 展开 14 个文件 / 15334 行**（含测试）：`popup.rs` 633、`popup/{anchor 73, animation 187, dismiss 582, focus 290, placement 1109, settle 408, stack 775, tests 1626}`、`app/popup.rs` 878、`a11y/popup.rs` 349、`a11y/popup_tests.rs` 1719、`tests/popup_behavior.rs` 5277、`kit/popup_panel.rs` 1428 |
| 触及 crate | `ui_core`、`ui_kit`、`widget`、`kytrix`、14 个 `widgets/*`、`instrument_selector_ui`/`market_participants_ui`/`profile_ui`/`settings_ui`/`updater_ui` |

结论：**旧 overlay/layer 机制与旧引擎 overlay API 全部删除，浮层只剩「引擎 popup 栈 + kit `PopupPanel`」一套机制**；7 组组合模式的旧 API `rg` 零命中（`dump-overlay` 仅剩 gallery 的改名提示，见 §5）。

## 2. 任务结果（P1-0…P1-12）

| 任务 | 结果（一句话） | 证据 |
|---|---|---|
| P1-0 popup root 原型 | 0×0 绝对 root 可行并采用（未回退 viewport-size 方案） | `popup/tests.rs::{popup_content_auto_sizes_to_its_children, absolute_popup_children_are_positioned_in_window_coordinates, opening_a_popup_does_not_change_the_main_tree_layout, the_popup_root_never_wins_the_hit_test, tab_focus_skips_the_popup_root_container}` |
| P1-1 栈/根/层序 | popup 模块 + 固定层序 `[...内容, popup_root, debug_hud]`；`reorder_children` 不置 structure epoch | `popup/tests.rs` 43 条；`tests/hud.rs` |
| P1-2 settle | 帧内 2 轮、首帧即正确、dirty 闸门、报告合并、Unsettled 诊断 | `popup_behavior::{opening_a_popup_settles_geometry_in_the_first_frame, content_growth_settles_in_one_frame_without_a_second_displacement, three_menu_levels_follow_the_parent_without_a_frame_lag, idle_frames_do_no_popup_settle_work, popup_settle_rounds_never_exceed_two}` |
| P1-3 placement | 纯函数 + 34 单测（终审删除 match_anchor_width 相关 2 条）；avoid/flip/bestFit/shift/limitShift/resize/clamp；子菜单方向由引擎强制 | `popup/placement.rs`；`submenu_placement_flips_right_to_left_with_exact_geometry`、`a_submenu_ignores_vertical_fallbacks_and_opens_horizontally` |
| P1-4 max-size | `NodeTree/Window::set_max_size` + `PopupPanel` 消费 `constrained_size`（`available_body_height`）做确定高度内部滚动；Menu 末行/Dialog footer 小视口可达（终审 E-04/Q1-01 补齐；round-3 E-20 补横向 width clamp） | `a_constrained_popup_exposes_its_applied_size_without_unsettled_churn`、`external_max_size_writes_are_reapplied_by_the_next_settle`、`menu::a_small_viewport_shrinks_the_menu_and_scrolls_its_last_row_into_view`、`dialog::a_tall_dialog_scrolls_its_content_and_keeps_the_footer_reachable` |
| P1-5 dismiss | 矩阵/scope/Toggle/Replay/**DismissPending-Closing 输入阻断**（终审 E-01）/窗口生命周期；`ParentClosed`/`FocusLost` 派发 | `popup/dismiss.rs`；`popup_behavior` 20+ 条 + `a_closing_modal_swallows_background_presses_until_it_is_removed` 等 |
| P1-6 focus | 单一真值 + 三分支 restore + fallback + Trap Tab 循环 + IME | `popup/focus.rs`；`restore_*`、`dialog_tab_cycles_inside_the_modal_scope`、`escape_during_ime_composition_cancels_it_and_keeps_the_popup` |
| P1-7 a11y | 链嵌套/唯一父/expanded/haspopup/owns/described_by/active_descendant/AT dismiss | `a11y/popup.rs`；`a11y/popup_tests.rs` 21 条 |
| P1-8 动画 | Fade/FadeSlide（paint_offset，不参与布局）；关闭 90ms；modal fade-only | `popup/animation.rs`；`slide_animation_*`、`closing_animation_*`、`modal_animation_is_fade_only` |
| P1-9 组件 | `PopupPanel` + Dropdown/Select/Tooltip/Menu/ContextMenu/Dialog/Alert/ColorPicker 全迁移（Menu 1 级 parity） | `kit/popup_panel.rs`；组件测试计数见 §5 |
| P1-10 宿主 | `widget` 去 Layer/AnimationRouter、`kytrix` composition/dialogs、14 widgets/5 业务 crate、测试支持迁移 | `cargo check --workspace --all-targets` PASS；§5 |
| P1-11 删除旧 API | `kit::{overlay,layer}`、`app/interaction overlay`、旧 App API、`overlay_style`、`overlay_wrapper`、`MenuMode::ListBox` 等全删 | §5 的 7 组 `rg` |
| P1-12 测试/gallery | `interaction_tests` 6 改写 + 9 删除（均有覆盖去向）；`--dump-overlay`→`--debug-stack`（旧名显式 exit 2）；`--pixel-check` 迁移（94 checks）+ 3 条 `sampled-animation-floor` | `p1-cleanup-w7.md` §3–§4；§5 基线 |

## 3. 偏差 / 有意行为变化（完整编号见设计 §12.3，此处为审查要点）

1. **Menu Auto 非模态 + Take**（旧 modal/trap、独占 band）：根菜单 `tier=Auto`、`Take{Previous}`；hover 子菜单 `FocusPolicy::None`（D13），键盘/点击子菜单 `Take`；旧 modal scope 不存在。**用户已在 P1 终审裁决 D6 保持不变。**
2. **outside press 默认 Replay（D14，用户已确认）**：kit 组件 `DismissPolicy::Chain{outside: Replay, escape: CloseTopmost, anchor_lost: true, focus_lost: true}`；`Consume` 显式 opt-in。旧「模态 + outside Consume」parity 不再保留（K1-02 的裁决即 D14）。
3. **子菜单 hover 150ms → 200ms**（`SUBMENU_DELAY`，可配）。
4. **Alert 允许叠放**：`LayerSlot::Exclusive` 随 layer 删除消失；Alert 是 Dialog 内容面板，多 Dialog/Alert 纯按 z 叠放。
5. **Hint-in-modal 仅背景阻塞（用户裁决 B）**：背景 Hint 被拒（`PopupError::HintBlockedByModal`），`owner_modal` 内的 Hint 允许显示在 modal 之上。
6. **ColorPicker `role=Dialog` 但非模态 + `RestoreFocus::Previous`**（tier=Auto、不阻断背景）。
7. **Dialog recenter 留 P3**（保留 `set_anchor` parity 实现）。
8. **`--dump-overlay` → `--debug-stack`**（P1-12 提前完成并同步 kit-gallery §13；dump `popup_stack` 快照 + probe rect；`overlay-check` 改为 popup 语义，不读像素；**旧名/未知参数显式 exit 2**，不再静默假绿）。
9. **`--pixel-check` 108 → 94 checks**（采样帧 45 → 32；新增 3 条 `sampled-animation-floor`：open ≥4 / close#1 ≥3 / close#2 ≥3，终审 K1-06 改为单次关闭分段）；命令名/退出码（0 PASS / 3 FAIL）不变。**94 条构成 = 32 条逐帧采样断言 + 16 条 `background-residue`（open/reopen 各 8 帧）+ 46 条静态（20 idle + 3 restored + 2 close-frames + 18 具名 + 3 floor）**；实测 open=8、close=6+6=12、sampled=32、checks=94。
10. **真实窗口像素判定仍 P3**（`--debug-stack` 不读像素；headless `PersistentPixels` 承担残影门禁）。
11. **`match_anchor_width` 由 kit 负责（A-09）**：引擎字段无生产消费者，已按死字段规则**删除**（终审 E-09）；kit 用 `PanelWidth::AtLeast` + 测量回写实现。
12. **子菜单 a11y 默认父 / hover active_item 留 P2**（当前默认父 = 父 popup 内容根，非父 menuitem；`A11yPatch.active_descendant` 通道已具备但 kit 未接）。
13. **静态 aria-haspopup 由 kit 在 P2 声明**（引擎能保活 kit 的 `A11yPatch.has_popup`，P1 kit 不写）。
14. **A-24 旧 overlay 层序随删除消解**；`reorder_children` damage 已含子树 painted_extent（A-21）。
15. **性能记 P3**：动画帧 popup 子树 lane 重建、结构帧 O(n) order 重建、settle 触发含 `layout_ran`（B-16）、a11y 全树 DFS（A-32）。
16. **滚动上下文 reparent 缺口 P1 未修**（P0 §11.3-4 已登记；`append_child` 无生产调用方）。
17. **Modal/Scrim 开/关走整帧重发**（`damage_full`）；非模态 popup 全程局部 damage。
18. **`PopupState::Opening` 首帧 settle 后即 `Open`**（B-12）；kit 用 `is_dismissable/is_closing` 收口；采样动画下限按“已提交帧增量”。
19. **kit 未启用 FadeSlide**（全 Fade/None）；引擎能力有测试、生产无使用者。
20. **`Anchor::Node` = 完整 border box**（翻面不重叠锚点节点）；**`AnchorEdge`/`edge` 已删除（终审 E-09/Q1-02）**，边由 `PlacementSpec.side` 决定。
21. **`PopupError` 13 变体**（新增 `HintBlockedByModal`/`StackSync`，`InvalidGeometry` 增 `avoid`）。
22. **`App::modal_scope` 保留**但语义改为 modal popup content 节点；`next_focusable` 去 scope 参数。
23. **外部 `remove_node(popup content)` 语义收窄 + 焦点修复（终审 E-05）**：不再做旧 opener 焦点还原；`PopupStack::remove` 清 `focus.owner`，`validate_owner` 修复失效 owner 并回落主树（Tab 可恢复）。
24. **Menu/ContextMenu 子菜单仅 1 级 parity**（`PopupSpec.parent` 链）；`MenuChain`/safe triangle/typeahead 属 P2。
25. **D-P1-25 遮罩回归主题色（终审 E-02）**：`ScrimSpec{color,density}`，Dialog 用主题 `Role::Scrim`/`backdrop`（Light 0.42 / Dark 0.55），不再 100% 黑。
26. **D-P1-26 Trap 显式初始焦点（终审 K1-05；E-21 收窄）**：`App::set_popup_initial_focus`（`PopupSpec.initial_focus` 字段已删，E-21），Dialog 打开后锁定 confirm，自定义 footer 顺序不再影响初始焦点。
27. **D-P1-27 死字段删除（终审 E-09/Q1-02）**：`Anchor::Node{node}`（删 `AnchorEdge`/`edge`）、`PlacementSpec.match_anchor_width` 删除。
28. **D-P1-28 模态关闭期输入阻断（终审 E-01）**：`DismissPending/Closing` 的 modal（含 Scrim）继续吞背景按下/hover/scroll，直到移除。
29. **D-P1-29 菜单滚动/resize 关链矩阵（终审 E-07/K1-01）**：引擎新增锚点查询 + 滚动纪元；Menu/ContextMenu 关整链，Dropdown/Select 重锚定，Tooltip 重定位/隐藏。
30. **D-P1-30 popup 阴影可见（终审 E-03）**：popup 内容根不再 `overflow_hidden`，panel/body 各自裁剪。

## 4. 最终 API（摘要；完整签名见设计 §12.4/§5.1/§5.2）

**引擎**

- `ui_core::popup`：`PopupId/PopupRect/PopupTier/PopupState/PopupDirty/ZPolicy/OutsidePress/EscapePolicy/DismissPolicy/RestoreFocus/InitialFocus/FocusPolicy/PopupAnimation/HasPopup/PopupA11y/ScrimSpec{density,color}/PopupSpec{...}（无 initial_focus，E-21）/PopupDismiss/DismissReason/DismissOutcome/PopupStackSnapshot/PopupSnapshotEntry/Side/Alignment/Placement/ShiftPolicy/MaxSize/Anchor{Node{node},WindowRect,NodeRect}/PlacementSpec/PlacementInput/PlacementResult/solve_placement/PopupError/PopupDiagnostic`；常量 `MAX_POPUP_DEPTH=32`、`MAX_POPUP_ENTRIES=1024`。`AnchorEdge`/`match_anchor_width` 已删除。
- `App`：`open_popup`、`close_popup`（立即）、`close_popup_chain`（立即整链）、`begin_close_popup`（事务，`Result<bool>`）、`popup_state`、`set_popup_a11y`（`Result<bool>`）、`set_popup_initial_focus`（`Result<bool>`）、`raise_popup`、`set_popup_anchor`、`request_popup_measure`、`popup_rect`、`popup_anchor_rect`、`popup_anchor_visible`、`window_scroll_epoch`、`popup_stack`、`popup_pointer`、`dismiss_topmost`、`popup_diagnostics`、`modal_scope`（保留）。
- `Window`：`node_screen_rect`（含滚动 + popup paint_offset）、`set_max_size`（失效 popup 缓存）、`scroll_epoch`（滚动纪元）。
- `NodeTree`：`set_max_size`、`reorder_children`（全排列、不置 structure epoch）、`hit_test`（既有）。

**kit**

- `kit::popup_panel`：`PopupPanel::{new,open,close,close_now,sync,release,set_anchor,request_measure,set_measure,set_width,set_a11y,is_open,is_closing,state,id,panel_node,body_node,popup_node,open_state,on_close}`；`PopupSurface{menu,dialog,tooltip}`、`PanelWidth{Auto,Exact,AtLeast}`、`PanelMeasure`、`PopupPanelOptions`。
- 组件：`Dropdown`、`Select`、`Menu`（`MenuEntry{Action,Separator,Submenu}`/`MenuAction`/`MenuSubmenu`/`MenuActivation`）、`ContextMenu`（`MenuAnchor`）、`Tooltip`（Hint/穿透/500+100ms）、`DialogBuilder`/`Dialog`（Modal+Trap{DefaultButton,Previous}+Scrim+Explicit）、`AlertBuilder`/`Alert`（Dialog 内容面板）、`ColorPicker`（内容闭包/局部坐标/tab measure）。
- 删除：`kit::overlay::*`、`kit::layer::*`、`AnimationRouter`、`overlay_wrapper`、`MenuMode::ListBox`（`ItemPanel` 保留为新形态）。

## 5. 测试与门禁基线（本机实测，2026-11-27）

| 门禁 | 结果 |
|---|---|
| `cargo test -p ui_core -p ui_kit` | **3780 passed / 0 failed / 2 ignored**（ui_core lib 2804 + 1 ignored、frame_record 8、gui_smoke 0 + 1 ignored、layout 4、popup_behavior 110、snapshot 14、demo 39、ui_kit lib 801；P0 基线 3554 → +226） |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | PASS |
| `cargo +nightly fmt --all -- --check` | PASS |
| `cargo check --workspace --all-targets` | PASS |
| `gallery --pixel-check` | **PASS**：`checks=94 sampled_frames=32 open_animation_frames=8 close_animation_frames=6+6=12 damage_full_frames=0`；`first_visible==stable=(496,91,300×375)`、`moved=(496,4,300×375)`；exit 0 |
| `gallery --debug-stack` | **PASS**：menu 度量 `(6, 180.0, 127.5125)`、picker children=4、`menu_frames=429`/`picker_frames=365`（运行间浮动）、最终 `entries=0`；**自行关窗并 exit 0/3，`--auto-close` 仅 ≥30s 看门狗** |

测试规模（`#[test]` 计数，round-3 最终）：`popup/tests.rs` 43、`tests/popup_behavior.rs` 110、`a11y/popup_tests.rs` 21、`popup/placement.rs` 34；kit：`PopupPanel` 18、`Dropdown` 27、`Select` 32、`Menu` 30、`ContextMenu` 14、`Tooltip` 28、`Dialog` 54、`Alert` 11、`ColorPicker` 80。

`--pixel-check` 94 条构成：32 条逐帧采样断言 + 16 条 `background-residue`（open/reopen 各 8 帧）+ 46 条静态（20 `idle-frames` + 3 `restored` + 2 `close-frames` + 18 具名 + 3 条 `sampled-animation-floor`）。

`rg` 零引用（`crates/**/*.rs`，7 组组合模式旧 API 全 0；`dump-overlay` 3 处均为 gallery 改名提示；详见设计 §12.9）：

```text
kit::overlay|overlay::\{|OverlayOptions|OverlaySurface|OverlayA11y|OverlayError|AnimationRouter
kit::layer|layer::\{|Layer::new|LayerSlot|Band::        # 大小写敏感：Layer::new 仅 CAMetalLayer::new；判据收紧后 0
create_overlay|close_overlay|place_overlay|bring_overlay_to_front|resolve_overlay_placements|dismiss_overlay|\.overlays\(
OverlaySlot|OverlayDismiss|OverlayPlacement|overlay_style|note_overlay_container|overlay_containers|overlay_slot
\.layer\(\)|\.animation\(\)|animation_router|services\.layer
overlay_wrapper|MenuMode::ListBox
dump-overlay|dump_overlay                        # 3 命中：gallery 的旧名失败/改名提示（旧 API 无使用）
AnchorEdge|match_anchor_width                  # 0（终审删除项）
```

## 6. 复跑命令（Windows PowerShell，可复制）

```powershell
$env:CARGO_TARGET_DIR='E:\kytrix\target'
cargo test -p ui_core -p ui_kit
cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings
cargo +nightly fmt --all -- --check
cargo check --workspace --all-targets
cargo run -p ui_kit --example gallery -- --pixel-check                     # exit 0=PASS / 3=FAIL（未知参数/旧 --dump-overlay = exit 2）
cargo run -p ui_kit --example gallery -- --debug-stack                  # 真实窗口：栈快照 + overlay-check（自行退出 0/3；--auto-close 仅 ≥30s 看门狗）
```

## 7. 已知 P2/P3 项（未在 P1 交付）

| 编号 | 内容 | 依据 |
|---|---|---|
| P2-1/P2-2 | `MenuChain` per-level 状态机 + 5 点 safe triangle/grace（300ms、镜像 bleed、open/flip/scroll 重算） | 设计 §4.8；P1 只有 1 级 + 200ms timer |
| P2-3/P2-4 | 菜单行高亮/滚动/子菜单指示/分组/勾选；完整键盘表 + typeahead + checkable | 设计 §4.8 |
| P2-5/P2-6 | 链式 dismiss 的 kit grace 几何（引擎已具备）；ContextMenu 右键/Shift+F10/Menu 事件绑定 | 设计 §4.7/§4.8 |
| P2-7 | 2–3 级级联测试 + a11y 链嵌套测试 + gallery 3 级演示 | 设计 §4.8/P2 验收 |
| P2（a11y） | 子菜单 a11y 父 = 父 menuitem（A-29）；静态 aria-haspopup 由 kit 声明（A-25）；hover active_item 接 `A11yPatch.active_descendant` | 设计 §4.10 |
| P3-1 | Dialog/Alert 细化（删 recenter；默认按钮已由 `App::set_popup_initial_focus` 收敛，K1-05/E-21） | 设计 §6 P3 |
| P3-2 | `Scale` 动画（lane transform 0.98→1） | D10 |
| P3-5 | 性能基线：open/close 帧预算、空闲帧 popup 工作、动画期 lane 重建、O(n) order、a11y 全树 DFS | 设计 §6 P3-5 |
| P3-6 | 三端真实窗口验收（像素 readback/截图、退出码合并） | 设计 §7.4 |
| P3-7 | `kit-gallery.md` §13 同步（`--debug-stack`/94 checks/采样下限/P1 完成状态） | **已完成**（P1 终审文档修复：该文 §13.3/§13.5） |
| P3（终审残余，round-3 后） | `FadeSlide` 未启用（K1-11/D-P1-19）；K1-12 accepted；E-12 残余；A-26/A-27/A-31；R2-01/R2-02/R2-04/R2-05；性能/三端 | 设计 §12.6/§12.11 |
| 未修 | 滚动上下文 reparent（跨滚动容器搬子树）damage 换算缺口；`append_child` 无生产调用方 | P0 §11.3-4 |

## 8. 与设计不一致 / 待复核（终审后仍未闭环项）

1. **两个同名 `MaxSize`**：`popup::MaxSize`（策略枚举）vs `node::tree::MaxSize`（数值 struct）；缓存失效已修，命名未收敛。
2. **settle 触发含 `layout_ran`**（B-16）：主树每次布局都会重解 popup，宽于设计 §4.3 的 dirty 闸门；需性能基线判定。
3. **a11y 混合父链环检测（A-26）**：仅 patch 链有环检测，popup 逻辑父 × patch 交叉成环未见测试。
4. **`PopupPanel::open` 已打开时的复用语义**：对 dismissable 的已有 popup 只 `set_anchor`+`raise` 并返回原 id（不重建内容）；Closing 中走立即 `close_popup` 再重建（终审未改）。
5. **`FrameRecord.popup_damage/z_changes`（§4.14）未加**：仅有 `FrameStats.popup_settle_rounds/popup_unsettled` + `App::popup_diagnostics`（A-31 部分）。
6. **E-12 测试真实性残余 + A-25 残余**：E-12 的 at_least 命名已修，仍余 Menu 指针测试命名、P1 popup 套件无变异自检、`--pixel-check` 仅 ColorPicker 场景、无 caret+`paint_offset` 直接断言；A-25 静态 `has_popup` 已修，`active_descendant` kit 仍未声明（引擎通道有测试）。
7. **K1-11** FadeSlide 未启用；**K1-12** 宿主测试 settle 放宽（accepted）。
8. **R2-01** 受约束 popup 内部滚动高度首帧未 settle；**R2-02** anchor 矩阵关闭 reason 固定 `Programmatic`；**R2-04** 无 `.confirm()` 的自建 footer 初始焦点回退 Cancel（D16 fallback，accepted）；**R2-05** 观察项（scrim 打开期主题刷新/滚动条覆盖/首帧采样）。
9. **外部滚动语义宽于“祖先滚动”字面**：`window_scroll_epoch` 对任意非 popup 子树滚动 +1，Menu/ContextMenu 会对无关滚动容器关链；建议措辞改“外部滚动（含祖先）”。
10. **终审已关闭项（留档，round-3 最终）**：`AnchorEdge`/`match_anchor_width` 删除（E-09/Q1-02）；IME caret `paint_offset`（E-11）；生产 `expect`（E-14/E-18）；E-08（AT 激活）、K1-07（宿主 total-stack 断言）、K1-08 b/e/f（Tooltip TOP/instrument Dialog 宽度/Dialog resize recenter 测试）、K1-09（`PanelState.height` 高度缓存）、Q1-04（widget `dialog_engine_error` 保留 `PopupError`）、E-17（raise 整组）、E-20（Dialog 宽度 clamp）、E-21（`PopupSpec.initial_focus` 字段删除）、A-29（子菜单 a11y 父）与 A-25 静态 has_popup；`interaction_tests` 删除 9 条等价强度（终审已复核）。

## 9. 复跑的注意事项

- 本轮按用户限制只跑 `ui_core`/`ui_kit` 测试与 workspace `check`；**workspace `cargo test` 未跑**。
- `--pixel-check` 是 headless（`TestApp` + 离屏 `GpuRenderer` + `PersistentPixels`）；`--debug-stack` 是真实窗口（本机 D3D12 独显），两者不可相互替代。
- 同一 `CARGO_TARGET_DIR` 下若有其它 agent 正在运行 `gallery.exe`，链接会报 `LNK1104`（文件占用）；等待其退出后重跑，或先跑 `--lib --tests` 采集计数。

## 10. P1 终审 finding 处置（2026-11-27，逐条证据见设计 §12.10/§12.11）

| 来源 | 处置摘要 |
|---|---|
| `review-p1-final-engine.md` E-01…E-16（+r2 E-17/E-18/E-20/E-21） | **P0 E-01 fixed**；P1 E-02/E-03/E-04 fixed；P2 E-05/E-06/E-07/E-08 fixed（E-08 由 r2 复核关闭），E-17 fixed（round-3 整组 raise）；P3 E-09/E-10/E-11/E-13/E-14/E-15/E-16/E-18/E-20/E-21 fixed，E-12 partial（残余 §12.6-6） |
| `review-p1-final-kit.md` K1-01…K1-13（+r2） | K1-01/K1-03/K1-04/K1-05/K1-06/K1-07/K1-08 b/e/f/K1-09/K1-10/K1-13 fixed；K1-02 rejected（用户裁决 D6/D14）；K1-11 deferred、K1-12 accepted（P3，见 §8/§12.6）；R2-01/R2-02/R2-04 deferred、R2-05 registered |
| `review-p1-final-quality.md` Q1-01…Q1-10（+r2 Q2-01/Q2-02） | Q1-01…Q1-10 全部 fixed（Q1-04 由 round-3 `widget::error::dialog_engine_error` 关闭）；Q2-01/Q2-02 由本轮文档同步关闭 |

- **终审结论**：引擎 P0 与全部 P1 已修复；kit 无 P0/P1；剩余均为 P2/P3 且已登记（§7/§8）。
- **门禁复跑（round-3 最终）**：`cargo test -p ui_core -p ui_kit` 3780 passed / 0 failed / 2 ignored；`--pixel-check` 94 checks PASS；`--debug-stack` 自行退出 PASS；clippy/fmt/workspace check 由 r2 报告与 lead 复跑确认（详见 §5）。
