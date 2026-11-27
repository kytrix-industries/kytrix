# P1 迁移清点与文件级计划（P1-9 kit / P1-10 宿主 / P1-11 删除 / P1-12 测试）

- 状态：调研完成（只读清点），供 P1-9/P1-10/P1-11/P1-12 实现 agent 直接执行
- 基线：worktree `E:\kytrix\.agents\worktrees\dsh-1d175414-1a41`，HEAD `d9949aca` + 未提交 P1 引擎基础（`crates/ui_core/src/popup*.rs`、`app/frame.rs`、`window.rs`、`a11y/*` 等）
- 依据：`.docs/popup-overlay-redesign-design.md` §4（§4.1/§4.2/§4.6/§4.7/§4.9/§4.10）、§5.1–§5.3、§6 P1、§8；`popup-overlay-redesign/evidence/kit-overlay-audit.md`、`engine-overlay-audit.md`；本文件内所有 `file:line` 均为当前 worktree 实测位置（与设计稿定稿行号有漂移的已单独标注）
- 口径：`crates/**/*.rs`；「生产」= 非 `#[cfg(test)]` 模块且不在 `examples/`；「测试」= `#[cfg(test)] mod` / `tests/` / `test_support`；「示例」= `crates/ui_kit/examples/gallery.rs`

---

## 0. 结论速览

1. **旧机制仍在三条链上同时存在**：`ui_kit::kit::overlay::Overlay`（2871 行）→ `ui_core` 旧 `App::create_overlay`/`OverlaySlot`；`ui_kit::kit::layer::{Layer,Band,LayerSlot}`（964 行）→ `App::overlays/bring_overlay_to_front`；`AnimationRouter`（overlay.rs:37-97）→ `App::on_animation_complete` 单槽。新引擎 `ui_core::popup` 已实现且已接入帧管线（settle 在 `app/frame.rs:197`、popup root 在 `Window::new` 预建），但 **kit 目前零消费者**，`PopupPanel`/`MenuChain` 全仓不存在。
2. **必须迁移的生产调用点约 66-68 个文件**（不含旧定义本身）：ui_kit 7（7 个组件；overlay.rs/layer.rs 属删除对象）、ui_core 约 14、`crates/widget` 2（services/cx；binding/test_support 属测试装置）、`crates/kytrix` 2、5 个业务 ui crate 25、14 个 widgets crate 15、gallery 1；另有测试支持 5 个文件、ui_core 测试 3 个文件（interaction_tests/gui_smoke/a11y）。
3. **实际生产消费面比设计稿估计更集中**：`Dropdown`/`Menu`/`ContextMenu` 在生产代码中 **0 消费者**（仅 gallery + 组件测试）；`Select` 13 个生产文件、`Tooltip` 6 个、`Dialog` 13 个构造点（+7 个 Alert 内容源）、`ColorPicker` 1 个（candlestick 面板）。真正跨 crate 的行为风险集中在 Select/Dialog/Tooltip。
4. **引擎侧还缺 5 个 kit 迁移必需的能力**（详见 §3.7、§7）：`begin_close_popup`（程序化动画关闭）、`set_popup_a11y`（Dialog 标题动态改 a11y）、`popup_state(id)`（is_closing/关闭检测）、`RestoreFocus` 语义（当前枚举未被消费）、`match_anchor_width` 只参与求解不写宽度。PopupPanel 设计必须与这 5 项一起收口，否则组件只能用轮询/自行兼容实现。
5. **旧 `Overlay` 的四个语义在新引擎没有 1:1 等价**：open 幂等/closing 复活、Band 固定绝对层序、`AtLeast`（≥anchor 且内容可撑宽）宽度、淡出完成"同帧卸载 + 下一帧 kit 收尾"。这四项是 P1-9 的主要行为 parity 风险，逐组件清单见 §2。

---

## 1. 旧 API 消费者清单（按 crate 分组）

### 1.1 旧符号总表与处置口径

| 旧符号 | 定义位置 | 新对应 | 处置 |
|---|---|---|---|
| `kit::overlay::Overlay` | `crates/ui_kit/src/kit/overlay.rs:616-1130` | `PopupPanel`（§3） | 迁移后删除整个文件 |
| `OverlayOptions`/`OverlaySurface`/`Scrim`/`OverlayA11y`/`OverlayError` | overlay.rs:182-212,420-584 | `PopupSpec`/`PopupSurface`/`PopupError`/`PopupA11y` | 删除；`PopupSurface` 进 kit |
| `kit::overlay::{Side,Alignment,Placement,PlacementConfig,resolve_origin}` | overlay.rs:98-257 | `ui_core::popup::{Side,Alignment,Placement,PlacementSpec,placement::solve_placement}` | 删除；组件改用引擎类型 |
| `kit::overlay::Anchor` / `anchor_rect` | overlay.rs:359-403 | `ui_core::popup::Anchor` | 删除；`Anchor::Node(node)` → `Anchor::Node{node,edge}` |
| `kit::overlay::DismissPolicy` | overlay.rs:405-418 | `ui_core::popup::DismissPolicy` | 删除 |
| `AnimationRouter` | overlay.rs:36-96 | 无（引擎 `popup::animation` 内部 tick） | 删除（rg 复核：仅 popup 构造/宿主注入，无独立消费者） |
| `kit::layer::{Layer,Band,LayerSlot,Entry}` | `crates/ui_kit/src/kit/layer.rs`（964 行） | `PopupTier`+`ZPolicy`+`PopupStack.order` | 删除整个文件 |
| `ItemPanel`/`ItemPanelOptions`/`PanelWidth`/`PanelContent`/`PanelMeasure`/`PanelRoles` | `crates/ui_kit/src/kit/menu.rs:64-291` | `PopupPanel` 的 menu 视图（`ItemPanel` 保留为 `pub(crate)` 菜单内容视图，去掉 Overlay/Layer 字段） | 改造（不删除类型名，删除 Overlay 依赖） |
| `MenuMode::ListBox` | menu.rs:990-1009 | 删除 | 仅测试使用（menu.rs:2611） |
| `App::create_overlay/close_overlay/place_overlay/bring_overlay_to_front/resolve_overlay_placements/overlays/dismiss_overlay` | `crates/ui_core/src/app.rs:1242-1340`、`app/overlay.rs` | 新 `App::{open_popup,close_popup,close_popup_chain,raise_popup,set_popup_anchor,request_popup_measure,popup_rect,popup_stack,popup_pointer,dismiss_topmost,popup_diagnostics}` | 删除（P1-11） |
| `interaction::{Overlay,OverlayOptions,OverlayPlacement,OverlayDismiss,OverlaySlot}`、`interaction::overlay` 模块 | `crates/ui_core/src/interaction.rs:327-418,601`、`interaction/overlay.rs` | 新 `popup::dismiss`/`popup::focus` | 删除（P1-11） |
| `overlay_style`/`note_overlay_container`/`overlay_containers`/`NodeTree::overlay_slot` | app.rs:302、node/tree.rs:407,554-567,1398、traverse.rs:93-97、arena.rs:89,639-640、content.rs:1021 | popup root 预建 + `PopupStack` | 删除（P1-11） |
| `WidgetServices.layer`/`animation_router`、`Cx::layer()/animation()`、host `layer()/animation()` | `crates/widget/src/services.rs:28-30,45-53,74-82`、`cx.rs:127-137`、各 host | 无（组件构造不再需要） | 删除 |
| `App::overlays` 测试支持 | 4 个业务 test_support + `ui_core/tests/hud.rs` | `App::popup_stack(window)` | 迁移 |

计数口径（本文件 §1.2–§1.9 的实测；正则含测试与 gallery，生产/测试分类见各行）：

| crate | 命中文件 | 旧符号命中数 | 生产文件 | 说明 |
|---|---:|---:|---:|---|
| `ui_kit` | 10 | 394 | 9 + gallery | 含 overlay.rs/layer.rs 自身约 300 处 |
| `ui_core` | 15 | 205 | ~12 | 旧引擎 API 定义与集成 + 3 个测试文件 |
| `widgets`（14 crate） | 22 | 69 | 15 | 其余为测试辅助 |
| `widget` | 4 | 37 | 3（test_support 属测试装置） | services/cx/binding/test_support |
| `profile_ui` | 5 | 34 | 4 | host/profile/menu/dialogs + test_support |
| `market_participants_ui` | 4 | 25 | 3 | host/menu/error + test_support |
| `settings_ui` | 8 | 23 | 8 | host + 7 个 view/dialog 构造点 |
| `instrument_selector_ui` | 6 | 17 | 3 | host + 2 个 selector + test_support/tests |
| `kytrix` | 2 | 6 | 2 | composition + shell/dialogs |
| `updater_ui` | 2 | 6 | 1（test_support 属测试装置） | indicator + test_support + dialog 错误映射 |

> `rg` 汇总命令（PowerShell 口径，实现期复核用）：
> ```powershell
> $files = Get-ChildItem crates -Recurse -Filter *.rs | ForEach-Object FullName
> Select-String -Path $files -Pattern 'kit::overlay|overlay::\{|OverlayOptions|OverlaySurface|OverlayA11y|OverlayError|AnimationRouter|kit::layer|LayerSlot|Band::|create_overlay|\.overlays\(|ItemPanel|MenuMode::ListBox'
> ```

### 1.2 `crates/ui_kit`（必须迁移；旧机制定义层）

**A. 旧机制定义文件（P1-11 整体删除）**

| 文件 | 行数 | 内容 |
|---|---:|---|
| `src/kit/overlay.rs` | 2871 | `AnimationRouter`(36-96)；`Side/Alignment/Placement/PlacementConfig`(98-179)；`OverlayError`(181-212)；`resolve_origin`(215-257)；`Anchor`/`anchor_rect`(359-403)；`DismissPolicy`(405-418)；`OverlaySurface`(420-479)；`Scrim`(481-497)；`OverlayA11y`(499-537)；`OverlayOptions`(539-584)；`Overlay`(615-1130)：`new`(621)、`open`(695，内含 `create_overlay` 741、`layer.register_with` 793、`measure` 802、`reveal` 806、`start_fade` 807)、`measure`(811)、`reveal/revealed`(828/840)、`place`(844)、`sync`(893)、`close/close_now/release`(916/937/947)；测试 1142-2871（37 个，含首帧 4 个） |
| `src/kit/layer.rs` | 964 | `Band`(10-30)、`LayerSlot`(32-37)、`Entry`(39-57)、`Layer`(64-245)：`register/register_with`(86/90)、`remove`(120)、`enforce`(191，调 `App::overlays`+`bring_overlay_to_front`)、`close_top/close_top_of`(219/228)；测试 246-964（14 个，含 `create_overlay` 308、`overlays` 193/340） |
| `src/kit/mod.rs` | 36 | `pub mod layer;`(17)、`pub mod overlay;`(22) 两行删除 |

**B. 生产组件/共享面板（P1-9 迁移）**

| 文件 | 旧 overlay/layer 使用 | 迁移动作 |
|---|---|---|
| `src/kit/menu.rs`（2641） | import 29-34；`ItemPanelOptions` 104-116；`PanelWidth` 118-122；`ItemPanel::new`→`Overlay::new` 233-257；`overlay()` 293；`open` 366-451（`Overlay::open` 400）；`resize` 453-464；`write_surface_width` 466-490；`reset_measure` 492-524；`measure_content` 526-554；`apply_content_width` 556-581；`close/close_now` 583-591；`sync` 647-662（`overlay.sync` 648）；`claim_focus` 664-686；`release` 688-702；`MenuSeed.layer/router` 1109-1110；`MenuInner.layer/router/panel` 1127-1129；`Menu::new` 1146-1169（layer/router 参数）；`ItemPanel::new` 1188-1211；`Menu::open` 1294-1311 / `open_at` 1313-1327（`Result<bool,OverlayError>`）；`panel_node` 1270-1282；子菜单（`MenuSubmenu` 939-988、`MenuMode` 990-1009、`MenuEntry` 971-988、`MenuOptions` 1012-1078、`MenuActivation` 1079-1088、`Menu` 1103+、`MenuKind` 1117-1121、`open_submenu`/hover 计时 ~1618-1751、`bind_extra_keys` 1758）；测试 2014-2641（16 个，harness 2076-2077 建 Layer/AnimationRouter；`MenuMode::ListBox` 2611） | `ItemPanel` 去 Layer/AnimationRouter/Overlay 字段，改为 `PopupPanel`+`List`；`MenuSeed/MenuInner` 去 layer/router；`OverlayError` → `PopupError`；菜单链 P2 引入 `MenuChain`，P1 保留现有 hover/键盘行为 |
| `src/kit/dropdown.rs`（1509） | import 21-26；`panel: ItemPanel` 160；`router` 176；`new` 172-248（ItemPanel::new 183、ItemPanelOptions 189-209、slot Exclusive 207）；`open` 465-496（`Anchor::node` 487、`OverlayError`）；`close/close_now` 498-504；`sync` 506-519；`release` 525-534；测试 807-1509（18 个；harness 829-872；`overlay_wrapper` 831/1253；宽度断言 1160/1314；a11y 1252） | 构造签名去掉 layer/router；`open() -> Result<bool, PopupError>`；`panel_node()` 改走 PopupPanel；宽度三态映射 PopupPanel `PanelWidth`；open 幂等/expanded 双源见 §2.1 |
| `src/kit/select.rs`（2125） | import 36-41；`panel` 208；`router` 225；`new` 221-300（ItemPanel::new 232、options 238-258、slot 256）；`open` 727-760；`close/close_now` 762-768；`sync` 770-784；`release` 790-801；测试 1256-2125（23 个；harness 1273-1309；`overlay_wrapper` 1275/1694；`select:panel:surface` 1915；scope 归还 2079） | 同 Dropdown；多选/搜索语义保留；`sync_tags` 直接写 style 保留 |
| `src/kit/tooltip.rs`（1410） | import 20-24；`router` 134；`Overlay::new` 140-161（`Band::Tooltip` 154、`Explicit` 151、`restore_focus:false` 153、tooltip surface 156、slot 155）；`attach/set_content` 186-206；`overlay()` 213-216（**公共 API**）；`sync` 218-273；`release` 275-279；`show` 328-360（set_a11y 341、`Anchor::node(trigger)` 346）；`hide` 362-369；`reset` 371-377；测试 459-1410（20 个；harness 479-530；`overlays()` 1289/1362；`progress/revealed/overlay_node` 大量） | Hint tier + pointer_transparent；`overlay()` 删除；"淡出窗口内重开"改为显式等待关闭完成（§2.3 风险） |
| `src/kit/context_menu.rs`（848） | import 13-15；`router` 100；`Menu::new` 96-127；`open` 160-172（`Anchor::point`）、`open_at` 174-187；`close/close_now` 189-195；`sync` 208-210；`release` 212-214；`layer.top_of/close_top_of` 237-244；测试 276-848（12 个；harness 298-343；`Band::ContextMenu` 260/608/795；`overlay_wrapper` 301） | `Anchor::WindowRect(点扩展 rect)`；context menu 独占语义改由 `tier:Auto`+同一 opener 链/引擎关闭其它 Auto 根承接；P2 再补 Shift+F10/Menu 键 |
| `src/kit/dialog.rs`（2406） | import 31-34；`DialogError::Overlay` 60-77；`DialogInner.layer/router` 299-300；`Overlay::new` 307-328（placement TOP_START/gap0/margin0/flip false、modal true、restore true、Band::Dialog/Alert、slot Exclusive、dialog surface、`Scrim::default()`、a11y、fade）；`open` 354-402（`set_a11y` 374、`Anchor::point(center)` 375、`Overlay::open` 379、回滚 392-397）；`close/close_now` 404-422；`sync` 424-448（recenter 428、`overlay.sync` 429、`finalize` 443-446）；`is_open/is_closing` 450-458；`set_title` 465-485（`set_a11y` 472、wrapper patch 474-483）；`key` 487-490；`open_state` 492-495；`overlay_node/panel_node/body_node` 497-510；`release` 532-540；`recenter` 690-729（`overlay.open` 当 set_anchor 719-728）；`begin_close` 731-742；`finalize` 744-756；`DialogConfig.band()` 133-138；测试 801-2406（38 个；harness 834-1001（855-856 Layer/AnimationRouter）；`layer.len` 1818/1868/1875/1919/2140；`Overlay::new` 1844；`modal_scope` 1273/1374/1820） | `Band` → Modal tier（Dialog 必须 Tier::Modal + Trap + Scrim）；`recenter` → `set_popup_anchor`；删除 kit `closing/reason` 双状态（改用 PopupPanel 状态机 + `on_close(reason)`）；`overlay_node` → `PopupPanel::popup_node`；`DialogError::Overlay` → `Popup(PopupError)`；标题动态 a11y 需引擎 `set_popup_a11y` |
| `src/kit/color_picker.rs`（4553） | import 35-38；`Inner.overlay/router` 660-690；`Overlay::new` 690-714（placement Bottom/Start、gap4、margin4、flip、OutsideClick、modal false、restore true、`Band::Popup` 704、slot 705、menu surface+padding8 706-709、scrim None、`OverlayA11y::dialog` 711、fade）；`is_open/open_state/open` 795-810；`close` 812-814；`overlay()` 821-824（gallery/测试用）；`build` 857-964；`sync` 966-999；`release` 1001-1012；`on_confirm` 1086（`overlay.close`）；`on_trigger` 1089-1100；`open_panel` 1314-1347（空 `Overlay::open` 1326、`panel_node` 1330、`build_panel` 1334、`measure` 1338）；`build_panel` 1349-1652；`sync_body_canvases` 2084-2126；测试 2248-4553（纯函数 2248-2877；UI harness 2914-3265；`overlay().options().surface` 3284；首帧 3306；K17 坏测试 3349-3361（`let _ = visual;` 在 3360）；`open_state` 3803；`overlay_node` 4423） | 空 open+`build_panel` 合并进 content 闭包；tab 切换补 `request_popup_measure`；`overlay()` 删除（gallery/测试改 PopupPanel 访问器）；`OverlaySurface{padding:8,..menu()}` → `PopupSurface` |
| `src/kit/alert.rs`（905） | **零 overlay/layer 依赖**（仅作为 Dialog 内容面板，`closing_guard` 171-174 由 dialog.rs:684 注入） | 不改；随 Dialog 迁移测试 |

**C. 示例（P1-12）**

| 位置 | 旧使用 | 动作 |
|---|---|---|
| `examples/gallery.rs`（3553） | import 51-55（`Band,Layer`/`Anchor,AnimationRouter,Overlay,OverlayOptions`）；flags 133-158（`--dump-overlay`/`--pixel-check`）；`OverlayCheckReport` 184-201；Gallery 字段 513-522（`overlay` 519、`dialog/alert_dialog/color_picker`）；Layer/Router 537-538；⑫ 演示 1333-1540（`Overlay::new` 1339、dropdown/select/menu/context 构造 1409/1430/1453/1502）；⑬ Dialog 1557-1638；⑭ ColorPicker 1646-1664；probe_nodes 1711-1731；`--dump-overlay` 流程 1848-1944（menu.open 1865、color_picker.open 1889、close 1927）；tick sync 2129-2133；`dump_overlay_nodes` 2654-2690；`picker_metrics` 2731-2739；pixel-check 2779-3450（`overlay().revealed` 2833、`panel_node` 2991、`overlay_node` 3141、`ColorPicker::new` 3204、`run_pixel_check` 3263）；main 3482/3537 | P1-12：全部改新 API；`--dump-overlay` 改 `popup_stack` 快照；`--pixel-check` 改 `debug_stack`+像素断言并保留命令名/退出码（0/3） |

### 1.3 `crates/ui_core`（旧引擎 API，P1-11 删除；测试 P1-12）

**生产（删除/替换）**

| 文件:行 | 旧内容 | 处置 |
|---|---|---|
| `src/app.rs:42,1242-1260,1331-1341` | imports；`create_overlay/close_overlay/place_overlay/bring_overlay_to_front/overlays/dismiss_overlay` 包装 | 删除 |
| `src/app.rs:302` | `fn overlay_style(origin)` | 删除 |
| `src/app/overlay.rs`（1-185） | `create_overlay`(14)/`close_overlay`(69)/`resolve_overlay_placements`(73)/`place_overlay`(109)/`bring_overlay_to_front`(133)/`overlays`(161)/`dismiss_overlay`(175) | 整文件删除 |
| `src/app/frame.rs:59` | `app.resolve_overlay_placements(window)` | 删除（新 settle 在 197） |
| `src/app/input.rs:13,93-99,238-249,823` | `overlay_util` import；`Action::Dismiss` 的 `dismiss_overlay` 兜底(97)；旧外部点击 `outside_click_target`→`remove_node`(238-249)；`handled` 后旧 dismiss 分支(823 附近) | 删除兜底；新 `popup::dismiss`(224)/`escape`(94) 保留 |
| `src/app/window_set.rs:80,92` | 节点删除时 `overlay_slot` 捕获 + `interaction::overlay::is_in_subtree` 焦点清理 | 改走 popup focus 清理（`close_entry` 已处理）；删除旧分支 |
| `src/interaction.rs:4,327-418,601` | `pub mod overlay`；`OverlayDismiss/OverlayPlacement/OverlayOptions/Overlay`；`NodeInteraction.overlay: Option<OverlaySlot>` | 删除（`NodeInteraction` 字段同步删） |
| `src/interaction/overlay.rs`（1-330） | `is_in_subtree/enclosing_overlay/in_modal_scope/outside_click_target/topmost/focus_scope_root` + 测试 | 整文件删除（popup/focus 已提供等价能力） |
| `src/interaction/focus.rs:28,160,569,587,616` | `in_scope` 调旧 `in_modal_scope`；测试构造 `OverlaySlot`/`focus_scope_root` | 改新 popup 作用域；测试重写 |
| `src/node/tree.rs:21,407,554-567,1398` | `OverlaySlot` import；`overlay_containers` 字段/`note_overlay_container`/`overlay_containers`；`overlay_slot` | 删除 |
| `src/node/tree/{arena.rs:89,639-640; content.rs:11,1021; traverse.rs:93-97}` | `overlay_containers` 维护 + `content::overlay_slot` | 删除 |
| `src/window/a11y.rs:12-13` | 旧 `focus_scope_root/in_modal_scope` 过滤 | 改新 popup a11y/focus 作用域 |
| `src/a11y/tree.rs:360,1288-1293,2251-2260` | `tree.overlay_slot(node)` a11y 回落；旧 modal scope；测试 helper `declare_overlay` | 删除/改新 popup 投影（`a11y::popup` 已实现） |
| `src/a11y/action.rs:781,819-824` | 测试 `create_overlay` | 改新 `open_popup` |
| `src/test_support/{a11y.rs:698,729-740; gui_smoke.rs:9,933-938,988,1051,1088; diagnostics.rs:385+（DamageOverlay 无关，保留）}` | 测试 helper 用旧 overlay | 改新 popup |
| `src/app/interaction_tests.rs` | 22 处 `create_overlay`（1710/1771/1825/1885/2021/2035/2092/3045/3252/3280/3335/3347/3417/3429/3504/3563/3894/3927/4004/4063/4115 + 直接调用 2092）与 `overlays()` 断言 1858/1877/1897/2120/3616 | P1-12 全部迁到 `open_popup`/`popup_stack`，或随旧 API 删除改为新 popup 行为测试 |
| `tests/hud.rs:6,335-359` | `hud_stays_above_overlays` 用 `create_overlay` | 改为 `open_popup` + 断言 HUD 在 popup root 之后（设计 §4.6） |

> 说明：`src/a11y/popup.rs`/`popup_tests.rs` 是**新** popup a11y 投影（不是旧 overlay），保留。

### 1.4 `crates/widget`（宿主层，P1-10）

| 文件:行 | 旧内容 | 目标 |
|---|---|---|
| `src/services.rs:11,28-30,45-53,74-82,176-178` | `Layer`/`AnimationRouter` import；`WidgetServices.layer`/`animation_router` 字段；`builder(..., layer, frame_router, animation_router)` | 删除两字段与两个 builder 参数（保留 `frame_router`） |
| `src/cx.rs:18-20,127-137,319` | `layer()/animation()` 访问器；测试断言 | 删除访问器；`cx.rs:319` 指针相等测试删除 |
| `src/binding.rs:445-457` | 测试装置自建 `Layer`/`AnimationRouter` 并注入 | 删除 |
| `src/test_support.rs:33-35,97-160,218-230,247-250` | `WidgetTestApp.layer/router/animation` 字段与访问器；构造注入 | 删除 layer/animation 字段与访问器；如需断言浮层，直接用 `app.popup_stack(window)` |
| `src/services.rs:184-731`（测试） | `Overlay` 测试（412-525）依赖 layer/animation，包含 band/pointer 相等断言 | 改 `open_popup`+`popup_stack`；`cx.layer()` 等价断言删除 |

### 1.5 `crates/kytrix`（组合根，P1-10）

| 文件:行 | 旧内容 | 目标 |
|---|---|---|
| `src/composition.rs:30,149-166,212-214,273-299,350-357` | import；`for_test` 建 Layer/AnimationRouter 并传给 `WidgetServices::builder`；`Composition.layer/animation_router` 字段；`build` 同 | 删除两字段/两构造；builder 调用少两参 |
| `src/shell/dialogs.rs:203-220` | `(*composition.layer).clone()` + `(*composition.animation_router).clone()` 传给 `DialogBuilder::build` | 改为 `DialogBuilder::build(app, theme, key)`（新签名，§2.6）；`composition` 仅用于 theme/services |

### 1.6 业务 ui crate（P1-10）

**instrument_selector_ui**（3 生产文件 + 测试）

| 文件:行 | 旧内容 | 目标 |
|---|---|---|
| `src/host.rs:9,78-90` | `layer()->&Layer`、`animation()->AnimationRouter` | 删除两访问器 |
| `src/instrument_selector.rs:442-443,455` | `Tooltip::new(..., host.layer(), host.animation(), ...)`；`DialogBuilder::new` | 构造去参；Dialog builder 新签名 |
| `src/prediction_selector.rs:929-930,943,981-982,1023,1142` | `Select::new`/`Tooltip::new` 传 layer+animation；`DialogBuilder::new` | 同上 |
| `src/test_support.rs:26,100-103,157` | 自建 Layer/AnimationRouter；`overlays().len()` | 删自建；改 `popup_stack().entries.len()` |
| `src/instrument_button/tests.rs:265,296` | `app().overlays(...)` | 改 popup_stack |
| `src/instrument_selector/tests.rs:988,1000,1011` | `harness.layer().len/is_empty` | 改 popup_stack |

**market_participants_ui**（3 生产文件 + 测试）

| 文件:行 | 旧内容 | 目标 |
|---|---|---|
| `src/host.rs:9,149-155` | `layer()/animation()` | 删除 |
| `src/menu.rs:34-36,252-291,303-316,344,553-554,596` | **直接 `Overlay::new`**（271-291：`Band::Dialog`、modal true、OutsideClick、menu surface、`OverlayA11y::menu`）；`Tooltip::new`(252)；`overlay.sync`(344)；`overlay.release`(596)；另 553-554 用于 `Menu`/内容?（需复核：553 为 `Menu` 构建时 layer/animation） | 直接 consumer → `PopupPanel`（tier Modal + Trap + Explicit{outside:true,escape:true}，无 scrim）或新 `Menu`；Tooltip 构造去参 |
| `src/error.rs:10,16,28-30` | `MenuError::Overlay(OverlayError)` + From | 改 `Popup(PopupError)` 或映射到 `Engine` |
| `src/test_support.rs:24,239-242,474` | 自建 Layer/AnimationRouter；`overlays().len()` | 同 instrument_selector |

**profile_ui**（4 生产文件 + 测试）

| 文件:行 | 旧内容 | 目标 |
|---|---|---|
| `src/host.rs:18,215-226` | `layer()/animation()` | 删除 |
| `src/profile.rs:28-30,164-192,695` | `Overlay::new`（176-192：menu surface、`Band::Popup`、Exclusive、OutsideClick、modal false、`OverlayA11y::menu`）；`Tooltip::new`(164)；`overlay.sync`(695)；错误映射 1329-1341 | 直接 consumer → `PopupPanel`（Auto + Take + Chain 默认）；错误映射改 PopupError |
| `src/menu.rs:20-21,110-111,134-144,187` | `Layer`/`AnimationRouter` 字段传入 `ContentBuild`；`Tooltip::new` 传 layer/animation | 删字段；Tooltip 去参 |
| `src/dialogs.rs:52-53` | `DialogBuilder::build` 传 layer/animation | 新签名 |
| `src/test_support.rs:28,366-368,417,507,794` | 自建 Layer/AnimationRouter；`Menu::new`（测试）；`overlays().len()` | 改 popup_stack；Menu 新签名 |

**settings_ui**（8 生产文件 + 测试）

| 文件:行 | 旧内容 | 目标 |
|---|---|---|
| `src/host.rs:6-9,89-102` | `layer()/animation()` | 删除 |
| `src/layout.rs:328-332` | `Select::new(..., host.layer(), host.animation(), ...)` | 去两参 |
| `src/settings.rs:535-544` | `DialogBuilder::alert` + `build(..., host.layer(), host.animation(), ...)` | 新签名 |
| `src/pages/venues/credentials.rs:680,703-704,843,856-857` | Alert/Select/Dialog 构造 | 去 layer/animation 参数 |
| `src/pages/translation/page.rs:132-136` | Select 构造 | 去两参 |
| `src/pages/proxy/table.rs:172-196` | Alert + Select | 同上 |
| `src/pages/proxy/editor.rs:502-511,762-766,804-808` | Dialog/Select | 同上 |
| `src/settings_tests/test_support.rs:24,87-90` | 自建 Layer/AnimationRouter | 删除 |

**updater_ui**（1 生产 + 测试）

| 文件:行 | 旧内容 | 目标 |
|---|---|---|
| `src/indicator.rs:186` | `Tooltip::new(..., layer, animation, ...)`（layer/animation 来自 host/services）| 去两参 |
| `src/dialog.rs:30,240,962-963` | `OverlayError` import + `DialogError::Overlay(...)` 映射 | 改 `DialogError::Popup(PopupError)` |
| `src/test_support.rs:21,84-87` | 自建 Layer/AnimationRouter | 删除 |

### 1.7 `crates/widgets`（14 个 crate / 15 个生产文件，P1-10）

全部是同一机械模式：`let layer = ui_kit::kit::layer::Layer::clone(cx.layer()); let router = ui_kit::kit::overlay::AnimationRouter::clone(cx.animation()); <component>::new(app, theme, layer, router, key, opts)`。

| crate | 生产文件:行 | 组件 | 迁移动作 |
|---|---|---|---|
| candlestick | `src/view.rs:1787-1789`、`1843-1845`（2 处 Select） | Select | 去 layer/router |
| candlestick | `src/panel/colors.rs:121-125` | ColorPicker | 去 layer/router |
| coin_market_cap_fear_and_greed_index | `src/view.rs:732-735` | Tooltip | 去两参 |
| cryptocurrency_etf | `src/view.rs:1140-1142` | Select | 去两参 |
| financial_calendar | `src/view.rs:939-942` | Select | 去两参 |
| financial_index | `src/dialog.rs:37,433-434,445` | Dialog（`DialogBuilder::new`） | 新 build 签名；`OverlayError` 映射 1444-1445 |
| global_shipping_chokepoint | `src/view.rs:722-732` | Select | 去两参 |
| hyperliquid_wallet | `src/dialog.rs:37,369-370,380` | Dialog | 同上；错误映射 1210-1211 |
| order_book | `src/view.rs:1733-1736` | Select | 去两参 |
| perpetual_statistics | `src/widget/display/pane.rs:1139-1143` | Select | 去两参 |
| prediction_market | `src/view.rs:28-29,2189-2192` | Select | 去两参（并清理 import） |
| solana_wallet | `src/dialog.rs:39,407-408,415` | Dialog | 同上；错误映射 1605-1606 |
| volume_heatmap | `src/view.rs:950-952` | Select | 去两参 |
| ethereum_wallet | `src/dialog.rs:39,448-449,456` | Dialog | 同上；错误映射 1608-1609 |
| x | `src/dialog.rs:40,555-556,564,1309-1310,1312` | Dialog ×2 | 同上；错误映射 3078-3079 |

测试专用（改断言，不涉及组件构造）：
- `widgets/force_order/src/view.rs:726,1238-1239`（测试建 `AnimationRouter`）
- `widgets/coin_market_cap_fear_and_greed_index/src/view/tests.rs:1775,1797-1799`（`layer().len()`）
- `widgets/financial_index/src/view.rs:4048`、`src/dialog.rs:2499,2506,2555`（`layer().len/is_empty`）
- `widgets/market_trade/src/view/tests.rs:1564`（`overlays()`）

### 1.8 测试支持（P1-10/P1-12）

| 位置:行 | 旧内容 | 目标 |
|---|---|---|
| `crates/instrument_selector_ui/src/test_support.rs:26,103,157` | `AnimationRouter::new`；`app().overlays(...).len()` | 删；改 `popup_stack` |
| `crates/market_participants_ui/src/test_support.rs:24,242,474` | 同上 | 同上 |
| `crates/profile_ui/src/test_support.rs:28,368,794` | 同上 | 同上 |
| `crates/widgets/market_trade/src/view/tests.rs:1564` | `app().overlays(window)` | 改 `popup_stack` |
| `crates/ui_core/tests/hud.rs:6,346` | `create_overlay` | 改 `open_popup` |
| `crates/ui_core/src/test_support/{gui_smoke.rs:933-938,988,1051,1088; a11y.rs:729}` | `create_overlay` + `overlays()` | 改新 popup |
| `crates/ui_kit/src/kit/test_support.rs:61-72` | `overlay_wrapper(app,window,panel)`：从 panel 向上找 root 的直接子节点 | **语义失效**（新架构返回共享 `popup_root`）；改为 `PopupPanel::popup_node()` 或沿父链找最近"含 popup 内容"的祖先（需新增引擎查询）；13 处调用（context_menu 301/660/756/785、dropdown 831/1253、menu 2037/2507/2618、overlay 2293、select 1275/1694） |
| 各 kit 组件测试 harness | `Layer::new`/`AnimationRouter::new` 21 处（overlay 1510-1511、layer 407+、menu 2076-2077、dropdown 871-872、select 1308-1309/2084-2085、tooltip 505/530/1288?/1328、context 342-343、dialog 855-856、color_picker 2930-2931/4363-4364、gallery 537-538/3202-3203、widget services 254/353、binding 445-446、test_support 130、business test_support 若干） | 删除构造；harness 只建 App/Theme |

### 1.9 Gallery（1 文件，P1-12）

见 §1.2C；关键点：⑫⑬⑭ 演示、`--dump-overlay`（1848-1944）、`--probe`（1788-1845）、`--pixel-check`（2779-3450）、`overlay-check` PASS 判据（184-201）。`--pixel-check` 当前直接绑定 `ColorPicker::overlay().revealed/panel_node/overlay_node`（2833/2991/3141），P1-11 删 API 后必须重写为 `debug_stack` + 像素断言，保留命令名与退出码 0/3（设计 §8 已登记）。

### 1.10 生产消费者按组件汇总

| 组件 | 生产构造点 | 生产文件 | 备注 |
|---|---:|---:|---|
| `Select` | 13 | 13 | settings_ui 4、instrument_selector 1、widgets 8（candlestick 2 处/1 文件） |
| `Dropdown` | 0 | 0 | 仅 gallery（1409）+ 测试（880） |
| `Tooltip` | 6 | 6 | instrument_selector、market_participants、profile_ui ×2、updater_ui、coin_market |
| `Menu` | 0 | 0 | 仅 gallery（1453/1465）+ 测试；生产是 profile/market 的自定义 Overlay 面板 |
| `ContextMenu` | 0 | 0 | 仅 gallery（1502）+ 测试 |
| `Dialog`（`DialogBuilder::new`） | 13 | 13 | instrument_selector 2、kytrix 2、settings 2、updater 1、widgets 5、gallery 1 |
| `DialogBuilder::alert` | 7 | 7 | kytrix、market_participants、profile、settings ×3、widgets/x、gallery |
| `ColorPicker` | 1 | 1 | candlestick `panel/colors.rs` |
| 直接 `Overlay::new`（非 kit 组件） | 2 | 2 | market_participants_ui/menu.rs:271、profile_ui/profile.rs:176 |
| `App::overlays` / `layer()` 断言 | 5 文件 + 测试 | — | 4 个 business test_support + market_trade tests + ui_core hud/gui_smoke |

---

## 2. kit 组件迁移映射

### 2.0 共享面：`ItemPanel`（menu/dropdown/select 三方的唯一面板底座）

**现状（menu.rs:64-291，`pub(crate)`）**：`PanelRow`/`PanelContent{rows,build,a11y,measure}`/`PanelMeasure{labels,extra}`/`PanelRoles`/`ItemPanelOptions{size,max_height,row_height,band,modal,placement,gap,roles,label,width,slot}`/`PanelWidth::{Exact,AtLeast}`；内部建 `Overlay::new`(233-257) 与 `List`；surface 是 overlay panel 内的 `{key}:surface`（`tab_stop(true)+on_click(activate)+overflow_hidden+a11y GenericContainer`，366-451）；`AtLeast` 用隐藏 4096px 文本节点 + `text_caret_rect` 量宽（492-581）；`sync(app, claim_focus)` 依赖宿主每帧调用（647-662）。

**目标**：拆成两层——
1. `PopupPanel`（新增，见 §3）：拥有引擎 popup 生命周期、surface（背景/圆角/padding/阴影/裁剪）、动画、关闭事务、宽度策略（`Auto/Exact/AtLeast`）、measure 回写。
2. `ItemPanel`（保留名，`pub(crate)`）：只剩"List + 行内容 + 键盘动作 + 高亮/激活/焦点表面"；字段去掉 `overlay/layer/router`，改为持有 `PopupPanel` + `List`；`ItemPanelOptions` 去掉 `band/slot/modal`，新增 `tier: PopupTier`、`focus: FocusPolicy`、`dismiss: DismissPolicy`（或由调用方传 `PopupSpec` 片段）。

**关键映射**：`OverlayOptions.placement/gap/margin/flip` → `PlacementSpec{side,align,gap,fallbacks,viewport_margin,shift,max_size,match_anchor_width}`；`band` → `PopupTier`（Popup/ContextMenu→Auto、Tooltip→Hint、Dialog/Alert→Modal）；`slot::Exclusive` → Auto 根自动替换（引擎 app/popup.rs:190-192）+ ContextMenu 显式关闭；`modal` → `FocusPolicy::Trap`；`restore_focus` → `FocusPolicy{restore}` + `opener`；`surface` → `PopupSurface`（§3.2）；`fade` → `PopupAnimation`；`a11y` → `PopupA11y`。

**风险**：`AtLeast` 宽度语义（≥anchor 且内容可撑宽）在引擎无等价物（§3.7 D3）；`MENU_PADDING=4`（menu.rs:40）与 `OverlaySurface::menu().padding=4`（overlay.rs:436）重复，须收敛为 `PopupSurface.padding` 单真值；三组件必须同批迁移（ItemPanel 共享）。

### 2.1 `Dropdown`（`crates/ui_kit/src/kit/dropdown.rs`，1509 行）

- **现公开 API**：`DropdownItem`(33-63)/`DropdownOptions`(65-133：size/variant/width/max_height/placement/gap/label/placeholder)/`Dropdown`(136)：`new(app,theme,layer,router,key,options)`(172)、`key`(249)、`trigger_node`(254)、`panel_node`(259)、`open_state`(264)、`is_open`(269)、`selected`(274)、`highlighted`(284)、`items`(289)、`set_items`(293)、`set_selected`(308)、`on_select`(313)、`set_disabled`(318)、`build`(322)、`attach`(433)、`mount`(454)、`open -> Result<bool,OverlayError>`(465)、`close/close_now`(498/502)、`sync`(506)、`dispatch`(521)、`release`(525)、`Debug`(797)。
- **旧 overlay/layer 能力**：`ItemPanel::new` 参数（183-209）：`Band::Popup`、`modal:true`（焦点陷阱）、`OutsideClick`、`slot:Exclusive`（对 Popup band 实际无效，layer.rs:104-108）、`PanelWidth::Exact/AtLeast`（202-206）、menu surface、`OverlayA11y{panel:Some(Menu)}`；`open` 用 `Anchor::node(trigger)` + 宽度=显式或触发器宽（475-487）+ `panel.focus`（493）；`sync` 每帧 `measure_trigger`+`resize`+`panel.sync(claim_focus=true)`（506-519）；`on_close` 焦点回 trigger（641-649）。
- **目标新 API**：`Dropdown::new(app, theme, key, options) -> Self`（去掉 layer/router）；`open(&self, app) -> Result<bool, PopupError>`（内部 `PanelWidth::{Exact(width), AtLeast{min: trigger_width, measure}}`；`PlacementSpec{side:Bottom, align:Start, gap:4, fallbacks:[Top/Start], viewport_margin:4, shift:Limit, max_size:Viewport, match_anchor_width:true}`；`Anchor::Node{node: trigger, edge: AnchorEdge::Bottom}`；`DismissPolicy::Chain{outside:Replay?/Consume?, escape:CloseTopmost, anchor_lost:true, focus_lost:true}`；`FocusPolicy::Take{restore:Opener}`；`opener:Some(trigger)`；`tier:Auto`；`animation:Fade`）；`sync` 保留（只同步 trigger/宽度跟随）；`panel_node` 改 `PopupPanel::panel_node`。
- **parity 要点（P1 必保）**：单选回填 + 回调顺序（先关后回调，652-665）；禁用行不选不关；虚拟列表 overscan=4（menu.rs:176-190）；↑/↓ 在 trigger 打开、面板内 ↑/↓/Home/End/Enter/Space/Esc；`set_items` 按 id 保选、不自动选首项；`set_selected` 不回调；trigger 文本/颜色/图标/caret/`aria-expanded`（383-421, 685-700）；外部按下关闭 + 不落到页面（旧模态消费）+ 焦点回 trigger；关闭态不吞背景点击；面板打开时背景滚动被吞（模态 scope）；`open()` 幂等（旧 Overlay::open 已开返回 false 并重定位，overlay.rs:706-721）。
- **文件/测试落点**：`dropdown.rs` 生产 1-804 改；测试 806-1509（18 个）中约 7 处需改写（开合 1059、宽度 1153/1298/1323、a11y 1244、expanded 1388、无帧 1276），其余保留断言、换节点来源。
- **风险**：宽度三态（`AtLeast` 无引擎等价）；open 幂等丢失（`open_popup` 每次建新 entry → 需自持 `PopupId` 判重）；`aria-expanded` 双源（引擎只在 popup 存在时写 expanded，初始 false 需 kit 信号/补丁，见 tooltip/dropdown 报告 R3）；toggle 与引擎 opener-Toggle 叠加（若设 `opener`+Auto，引擎在 pointer-down 已 toggle 并 Consume，与组件自身 `trigger_click` 会双触发 → **建议 P1 暂不设 `opener`，由组件自持 toggle；`opener` 仅用于焦点恢复**，需 lead 决定）；`Dropdown::release` 未 `release_scope`（既有泄漏，迁移时顺带修）；**typeahead 当前不存在**（全仓 kit 无字符键/搜索代码），设计 §5.2 若要求 typeahead 属新增功能，需单独裁决（建议 P2 与 MenuChain 键盘表一起做）。

### 2.2 `Select`（`crates/ui_kit/src/kit/select.rs`，2125 行）

- **现公开 API**：`SelectItem`(52-83)/`SelectOptions`(83-177：size/variant/width/max_height/placement/gap/label/placeholder/multiple/clearable/max_tags)/`Select`(179)：`new(app,theme,layer,router,key,options)`(221)、`key`(301)、`trigger_node`(306)、`panel_node`(311)、`open_state`(316)、`is_open`(321)、`chosen`(326)、`chosen_indices`(337)、`highlighted`(342)、`items`(347)、`placeholder_text`(352)、`set_items`(356)、`set_chosen`(376)、`clear`(395)、`on_change`(404)、`tooltip_text/tooltip`(409/415)、`set_disabled/is_disabled`(419/424)、`build`(428)、`attach`(695)、`mount`(716)、`open -> Result<bool,OverlayError>`(727)、`close/close_now`(762/766)、`sync`(770)、`dispatch`(786)、`release`(790)。
- **旧能力**：同 Dropdown 的 ItemPanel 参数（232-258）；`Band::Popup`、`modal:true`；`roles{panel:None,row:ListBoxOption}`；`Anchor::node(trigger)`（746-749）；`sync` 同样每帧 resize/claim；**无 `on_close` 兜底焦点**（靠 `restore_focus:true`，menu.rs:246）；`release` 归还 scope（800）。
- **目标新 API**：同 Dropdown 的签名变化；`panel` role 保持 `None`（a11y List 由内部 List 声明，避免双 List）；行 role=ListBoxOption；`FocusPolicy::Take{restore:Opener}` + `opener:Some(trigger)`。
- **parity 要点**：单选/多选（多选 toggle 不关面板、chosen 排序、`notify` 受控，937-968/955-963）；`max_tags` chip + `+N` 溢出（583-618/1064-1108）；`clearable` × 按钮（620-667）；a11y 摘要（单选选中项/多选 `first +N`，970-993）；行 `selected/checked`（859-873）；复用 ItemPanel 全部键盘/虚拟化/宽度/放置行为；`:hit:` 壳与 `select:panel:surface` debug id 被测试直接引用（1915/1936），保留；`release` scope 归还测试（2079）。
- **文件/测试落点**：`select.rs` 生产 1-1254；测试 1255-2125（23 个）中约 6 处需改写（a11y 1685、AtLeast 1810、换行测量 1865、宽度 1835 保留断言、焦点 1899、expanded 2005），其余保留。
- **风险**：同 Dropdown 全部 + 多选标签 `sync_tags` 直接写 style（1064-1108）不要改成整树重建；`revision` 信号仅用于 bind_text 重算（211/288/503）；`AtLeast` 测量需含 `✓` 列 + 图标 `extra`（874-880）；`clear` 与关闭顺序（928）；**Select 当前没有搜索/typeahead**（grep `search|typeahead` 零命中；设计 §5.2「Select 保留多选/搜索语义」中"搜索"与现状不符）→ P1 按 parity 不新增，若产品要求搜索需单独立项（建议 P2/P3）。

### 2.3 `Tooltip`（`crates/ui_kit/src/kit/tooltip.rs`，1410 行）

- **现公开 API**：`TooltipContent`(30-57)/`TooltipOptions`(60-127：placement/gap/delay/fade/max_width)/`Tooltip`(125)：`new(app,theme,layer,router,key,options)`(130)、`key`(182)、`attach(app,window,trigger)`(186)、`set_content(Option<TooltipContent>) -> bool`(194)、`content`(203)、`is_visible`(208)、**`overlay() -> &Overlay`(213，公共泄漏)**、`sync(app) -> bool`(218，唯一驱动)、`release`(275)。
- **旧能力**：`Overlay::new` 140-161：`Placement`(147)/gap 4/margin 4/flip、**`DismissPolicy::Explicit`**(151，不在外部点击关闭)、**`modal:false`**(152)、**`restore_focus:false`**(153)、**`Band::Tooltip`**(154，永远最高)、`OverlaySurface::tooltip()`(156，SurfaceInverse/radius4/padding4/无阴影)、`OverlayA11y::default()`(158，每次 show 前 `set_a11y(tooltip(label))` 341)、`fade` 可配；状态机 Idle/Waiting/Shown + `delay` 定时器（232-326）；show/hide/reset（328-377）；`sync` 探测引擎侧关闭（218-273）。
- **目标新 API**：`Tooltip::new(app, theme, key, options)`；删除 `overlay()`，改 `popup_node(app)`/`panel_node()`/`is_visible(app)`；`PopupSpec{tier:Hint, anchor:Anchor::Node{node:trigger, edge:Bottom}, placement:{side:Bottom,align:Center,gap:4,fallbacks:[Top/Center],viewport_margin:4}, dismiss:DismissPolicy::Hint{escape:true}, focus:FocusPolicy::None, pointer_transparent:true, opener:Some(trigger), animation: if fade {Fade} else {None}, a11y: PopupA11y{role:Some(Tooltip), label:Some(content.label()), described_by:Some(trigger)}}`；`set_content` 发生变化时先 close 再等关闭完成后 open（沿用 `pending_restart` 语义，但改为基于 `PopupState`）。
- **parity 要点**：延迟状态机（500ms 默认/`immediate()` 0/`delay`）；hover/focus 触发器自身或后代判定（288-301/387-396）；离开/失焦/内容撤回关闭；Esc 只关自身；内容切换时旧面板不得复活（三个测试 990/1052/1105 强保护）；tooltip 不接收点击、不取焦；`max_width` 只约束内容 row（面板总宽=内容+2×4，测试 795 断言 `320+8`）；tooltip 在其它浮层之上（旧 Band::Tooltip=50；新 z=Newest 且 Hint 不参与 dismiss 竞争）。
- **文件/测试落点**：`tooltip.rs` 生产 1-457；测试 459-1410（20 个）中约 10 处需改写（`progress/revealed/overlay_node` 断言 675/741/844/895/990/1052/1105/1209/1249/1322），其余保留。
- **风险**：无 open 幂等/revive → 重入需显式等关闭完成（引擎缺完成回调，§3.7 D2）；`overlays()`/Layer band 断言改 `popup_stack` z 序；`Hint` tier 在 modal 打开时被 `close_hints` 关闭（旧行为是保留在下层）；`Tooltip` 角色会被引擎追加 `HideTooltip` a11y 动作（测试 `actions.is_empty()` 必改）；`max_width` 不能映射到 `MaxSize`（viewport shrink），必须留在 kit 内容构建。

### 2.4 `Menu`（`crates/ui_kit/src/kit/menu.rs`，2641 行）

- **现公开 API**：`MenuAction`(882-935)/`MenuSubmenu`(938-968)/`MenuEntry`(970-986)/`MenuMode`(989-1009)/`MenuOptions`(1011-1074：size/width/max_height/mode/placement/gap/band/label/submenu_delay)/`MenuActivation`(1078-1081)/`Menu`(1102-1391)：`new(app,theme,layer,router,key,entries,options)`(1146)、`set_entries`(1244)、`entries`(1256)、`open_state`(1261)、`is_open`(1266)、`panel_node`(1271)、`surface_node`(1276，死)、`row_count/highlighted`(1285/1290)、`open(app,window,Anchor) -> Result<bool,OverlayError>`(1294)、`open_at`(1313)、`close/close_now`(1329/1334)、`on_activate(MenuActivation)`(1339)、`on_close`(1344)、`sync`(1349)、`release`(1372)、`Debug`。
- **旧能力**：全部经 ItemPanel（233-257）+ `MenuSeed/MenuInner` 的 layer/router 克隆（1109-1110/1127-1128）；root `modal=true`（1199，焦点陷阱），子菜单 `modal=false`+`Nested`+`RIGHT_START`+gap 0（1643-1665）；`Anchor::node(row)`（1660）；`root Children` 即 z 序（layer.enforce）。
- **目标新 API**：`Menu::new(app, theme, key, entries, options)`；`open(app, window, Anchor) -> Result<bool, PopupError>`；内部每层一个 `PopupPanel`；子菜单 `PopupSpec{parent:Some(parent_id), anchor:Node{row,edge:Right}, placement:{side:Right,align:Start,gap:0,fallbacks:[Left/Start]}, tier:Auto, focus:Take{restore:Previous}, opener:Some(row), dismiss:Chain{outside:...,escape:CloseTopmost,anchor_lost:true,focus_lost:false}}`；root `FocusPolicy::{Trap|Take}` + `tier:Auto`（P1 parity 见下）；`MenuOptions.band` 删除。
- **P1 parity（必保，来自审计 §6.1）**：BOTTOM_START/宽度/翻面（1-2）；max_height 内部滚动（3）；行外观/Group/Separator/checkable 静态渲染（4-5）；首行高亮/方向键跳过禁用/不环绕/Home/End（6）；→ 开子菜单夺焦，← 关子菜单回父行（7）；hover 150ms 延迟开子菜单 + 右侧 gap0 + 翻左（8）；hover 普通/禁用/Group/Separator 关子菜单（9）；release 激活 → 关整链 + `on_activate` 一次（10，D15 已一致）；空白点击不激活（11）；Esc 只关最上层、root 关闭才回调一次（12）；外部按下关闭且**不落到页面**（13，需 `OutsidePress::Consume` 决策）；root 菜单焦点陷阱/背景 hover 过滤/关闭恢复焦点（14）；fade 后卸载、`close_now` 立即且幂等（15）；稳定态无帧（16）；a11y wrapper=Menu/面板 GenericContainer/行角色/Group=Label/Separator=hidden/`Menu` 下抑制内层 List（17）；`set_entries` 打开中更新（25，顺带修 K10）。ContextMenu 部分见 §2.5。
- **P2 推迟（P1 不实现）**：safe triangle/grace 多边形、兄弟替换 grace、3 级级联、链式 dismiss（几何 Ignore/CloseChain、D14 replay）、Tab/Shift+Tab 关链、PageUp/PageDown/active_descendant、typeahead、checkable Space/radio 语义、hover 打开不夺焦的 keyboard_target、hover-out 关闭、长菜单父项滚出关闭子级、子菜单 a11y 父链、右键/Shift+F10、Tooltip 优先级细化。
- **文件/测试落点**：建议 `menu.rs` 拆出 `item_panel.rs`（共享面板视图）+ `menu.rs`（Menu 链）；两者 P1 可先留在 menu.rs。测试 2014-2641（16 个）：重写 4 个强依赖结构（2385 子菜单 root_children、2432 Esc 逐层、2499 a11y、2560 modal scope），`root_children` 辅助改 `popup_stack`；`MenuMode::ListBox` 测试 2605-2640 删除（生产零使用）。
- **风险**：`ItemPanel` 三方共享；a11y role 归属（旧在 engine wrapper，新在 popup 内容节点 → 内层面板保持 GenericContainer 才能维持 List 抑制）；外部点击 Consume vs Replay（R3）；关闭事务 on_close 只一次（R4）；root 菜单 modal 语义（`Tier::Modal` 会带 scrim/关其它 auto 根，`FocusPolicy::Trap` 单独即可产生 modal scope，需 lead 定）；`overlay_wrapper`/`panel_node` 语义（R7）；`MENU_PADDING` 与 surface padding 重复（R8）；`max_height` 与引擎 MaxSize 职责重叠（R9）；ContextMenu K9 坐标（R11）；`Band` 级联顺序（R12：Dialog 打开关闭 ContextMenu、Tooltip 永远最高）。

### 2.5 `ContextMenu`（`crates/ui_kit/src/kit/context_menu.rs`，848 行）

- **现公开 API**：`MenuAnchor::TopLeft/BottomLeft`(19-33，映射 BOTTOM_START/TOP_START)/`ContextMenuOptions`(35-80)/`ContextMenu`(83)：`new(app,theme,layer,router,key,entries,options)`(96)、`key`(128，死)、`is_open`(133)、`panel_node`(138)、`highlighted`(143)、`set_entries/entries`(147/156)、`open(app,window,point) -> Result<bool,OverlayError>`(160)、`open_at`(174)、`close/close_now`(189/193)、`on_item_click`(197)、`on_close`(202)、`sync`(208)、`release`(212)。
- **旧能力**：内部持 `Menu`（96-125）；`Anchor::point(point)`（171/186，**K9 坐标系混用**：调用方传内容坐标，窗口坐标未换算，滚动后偏移）；`close_peers` 用 `layer.top_of(Band::ContextMenu)`+`close_top_of`（235-249，同带互斥）；`on_activate` 先 `close_now` 再回调（216-233）。
- **目标新 API**：`ContextMenu::open(app, window, point)` 的 `point` 语义需重定：`Anchor::WindowRect(LogicalRect::from_origin_size(point, ZERO))`，且调用方传**窗口坐标**（`ContextMenu::open_at` 文档明确）；用 `tier:Auto` 根自动替换旧同带互斥；`close_peers` 删除；`key`/死字段删除。
- **parity 要点**：TopLeft/BottomLeft 定位与 4px clamp（475-513）；选条目先关后回调（516-530）；外部按下关闭且不落页面（533-544）；Esc 关闭幂等（547-560）；第二个实例替换第一个（563-576，改由 Auto 根替换）；同实例重开只换锚点、保留根面板、关旧子菜单（579-627，`set_popup_anchor`）；`open_at` 高亮（630-644）；label 透传 wrapper（647-672）；子菜单 z 在父之上 + Esc 先关子（751-825）；浮层下页面不滚动（828-847）。
- **文件/测试落点**：context_menu.rs 生产 1-274；测试 275-848（12 个）重写 5 个（Layer 断言 592/606-609/626/793-812、`overlay_wrapper` 660/756/785、root_children/z 序）。
- **风险**：生产零消费者（只 gallery），可排在 Menu 之后；坐标系契约变更需文档/调用点同步；`LayerSlot::Exclusive` 上下文菜单语义改 Auto 替换后，两个不同窗口/scope 的 ContextMenu 是否允许共存需确认。

### 2.6 `Dialog` / `Alert`

**Dialog（`crates/ui_kit/src/kit/dialog.rs`，2406 行）**

- **现公开 API**：`DialogClose::{Confirmed,Cancelled,Dismissed}`(53-58)/`DialogError::{Overlay(OverlayError),Engine(EngineError),NotOpen}`(60-97)/`DialogContent`(99)/`DialogAlertSource`(101)/`DialogAction`(103)/`DialogCloseCallback`(105)/`DialogBuilder`(187-194)：`new(title)`(196)、`alert(title,source)`(209)、`content`(221)、`confirm/cancel`(230/241)、`header/close_button/close_label`(250-266)、`width`(268)、`dismiss`(276)、`fade`(282)、`on_close`(288)、`build(app,theme,layer,router,key)`(294-351)；`Dialog`(182)：`open(app,window) -> Result<bool,DialogError>`(354)、`close/close_now`(404/411)、`sync`(424)、`is_open`(450)、`is_closing`(455)、`title/set_title`(460/465)、`key/open_state/overlay_node/panel_node/body_node`(487-510)、`confirm_button/cancel_button/close_button`(512-525)、`with_alert`(527)、`release`(532)；私有 `recenter`(690-729)、`begin_close`(731-742)、`finalize`(744-756)。
- **旧能力（307-328）**：placement TOP_START/gap0/margin0/flip false；`modal:true`（trap focus + 模态 scope）；`restore_focus:true`；`Band::Dialog`/`Band::Alert`（Alert 用 `LayerSlot::Exclusive` 独占、Dialog 可与 Alert 叠）；`OverlaySurface::dialog()`（SurfaceOverlay/radius4/padding8/单层大阴影）；`Scrim::default()`(density 1.0)；`OverlayA11y::dialog/alert_dialog(title)`；`fade` 可配；`Anchor::point(viewport center)`（375-378，recenter 每帧 690-729 用 `overlay.open` 当 set_anchor）；kit 自有 `closing: Cell<bool>` + `reason: Cell<Option<DialogClose>>`（K15 双状态）；关闭回调在 `sync` 检测 overlay 卸载后 `finalize`（443-446）。
- **目标新 API**：`DialogBuilder::build(app, theme, key)`（去 layer/router）；`open` 内部 `PopupPanel::open(..., PopupSpec{tier:Modal, focus:Trap{restore:Opener/Previous, initial:DefaultButton}, dismiss:Explicit{outside:config,escape:true}, scrim:Some(ScrimSpec{1.0}), anchor: 见下, placement:{side:Bottom,align:Start,gap:0,fallbacks:[],viewport_margin:4,shift:None,max_size:Viewport}, animation: if fade {Fade} else {None}, a11y: PopupA11y{dialog|alert_dialog, label=title}, opener:None}, content_closure)`。
  **居中锚点配方（需写进实现，旧 recenter 的等价做法）**：`Anchor::WindowRect(LogicalRect::from_origin_size(desired_origin, ZERO))` + `Placement{side:Bottom, align:Start, gap:0}` 时 `solve_placement` 返回 origin == `desired_origin`；`desired_origin = ((viewport - panel_size) * 0.5).max(0)`。panel_size 在 `open` 内测量后可得（`PopupPanel` 的 measure 返回；宽=config.width、高=布局），`Dialog::sync` 在 resize/尺寸变化时读 `popup_rect(id).hit.size` 重算并 `set_anchor`。**不要**照抄旧 `Anchor::point(center)`+`TOP_START` 的“锚点 y = desired.y + 高度”写法（旧实现利用 `resolve_origin` 的 TOP 语义间接得到 origin；新 API 无该隐式行为）。
  recenter → `popup_panel.set_anchor(desired_rect)`（每次 sync 或 resize 时）；删除 kit `closing/reason`，用 `PopupPanel::{is_closing,state}` + `on_close(DismissReason→DialogClose)`；`overlay_node` → `popup_node`；`DialogError::Overlay` → `Popup(PopupError)`；`set_title` → `popup_panel.set_a11y`（引擎需新增 API，§3.7 D1）。
- **parity 要点（P1）**：打开原子性/构建失败回滚（389-401）；居中 + resize 回中（375-378/690-729）；`Trap` + DefaultButton 初始焦点 + 关闭恢复（旧测试 1325-1366，新引擎 popup/focus.rs:20-49 更确定）；Esc/外部点击策略（`Explicit{outside:false}` 时模态消费不穿透 1280-1302；`outside=true` 关闭 1305-1322）；scrim density 1.0 + a11y hidden（1474-1496）；a11y Dialog/AlertDialog + label；关闭事务：`on_close(reason)` 恰好一次、`is_closing`、淡出期间点击不跑 action、`close_now` 保 reason、closing 中重开先收尾再开（1717-2204）；Alert 内容面板 + `closing_guard`（670-688/alert.rs:171-174）；`sync` 返回 changed + 稳态 Idle；多对话框叠放仅顶层关（1780-1839）；release 摘全部 action（2207-2278）。
- **P3 推迟**：Tab 循环/初始焦点细化（设计 P3-1 细化；引擎 Trap 已提供基础）、`recenter` 删除（设计 §8 列 P3）、Band 固定级联语义重定（Alert 恒高于 Dialog 不再成立 → 用 z/Newest + 打开顺序；需产品确认）。
- **测试落点**：dialog.rs 测试 801-2406（38 个）重写约 13 个（`layer.len` 1818/1868/1875/1919/2140、`Overlay::new` 1844、`modal_scope` 1273/1374/1820、组淡出 1383、帧内卸载 1437、首帧 a11y wrapper）；其余保留（行为断言换 API）。
- **风险**：K15 双状态必须一次清除；Esc 语义收窄（旧 Esc 无条件关最上层，新 `Explicit{escape:false}` 会静默不关 → Dialog 显式 `escape:true`）；`recenter` 的 changed 假阳性（720-728 `open(...).is_ok()` 恒 true，勿照抄）；`DialogRole::AlertDialog` 是否保留 `alert_dialog` role；Dialog 打开时关闭无关 popup 的旧 Layer 语义（layer.rs:99-103）由 `tier:Modal` 的 `close_auto_roots_outside`/`close_hints` 承接（app/popup.rs:186-189）。

**Alert（`crates/ui_kit/src/kit/alert.rs`，905 行）**：零 overlay/layer 依赖，仅作为 Dialog 内容面板（`DialogAlertSource` dialog.rs:101/670-688；`closing_guard` alert.rs:171-174）。P1 **不需改 alert.rs**；仅随 Dialog 迁移改测试脚手架（11 个测试基本原样保留）。

### 2.7 `ColorPicker`（`crates/ui_kit/src/kit/color_picker.rs`，4553 行）

- **现公开 API**：`ColorPickerTab`(461-466)/`ColorPickerLabels`(468-493)/`ColorPicker`(495-498)：`new(app,theme,layer,router,key,initial,UnifiedSize)`(656-748)、`key/unified_size`(750-758)、`color/color_signal/draft/hovered_swatch/active_tab/labels/hex_text`(760-793)、`is_open/open_state`(795-803)、`open(app,window) -> Result<bool,OverlayError>`(805-810)、`close`(812-814)、`trigger_node`(816)、**`overlay() -> &Overlay`(821-824，gallery/测试)**、`on_change`(826)、`on_open`(830)、`set_labels/set_active_tab/set_color`(834-855)、`build`(857-964)、`sync`(966-999)、`release`(1001-1012)、`on_confirm/on_trigger`(1067-1100)、`open_panel`(1314-1347)、`build_panel`(1349-1652)、`sync_body_canvases`(2084-2126)、`reset_after_close`(2155-2170)；无 `on_close`。
- **旧能力（690-714）**：placement Bottom/Start、gap4/margin4/flip；`OutsideClick`；`modal:false`；`restore_focus:true`；`Band::Popup`；slot Exclusive（Popup band 无效）；`OverlaySurface{ padding:8, ..menu() }`（双层菜单阴影+radius4+padding8）；无 scrim；`OverlayA11y::dialog(labels.panel)`；fade；**空 open + `build_panel` + `measure` 两步式**（1324-1341：`Overlay::open(|_,_| Ok(()))` → `panel_node` → `build_panel` → `overlay.measure`）；Canvas 已按 P0 节点局部坐标（2199-2227；`hit.local_point` 1121/1157）；tab 切换重建 body **但没有 re-measure**（1915-1931/2044-2052，已知缺口）。
- **目标新 API**：`ColorPicker::new(app, theme, key, initial, size)`；`open` 合并为单 content 闭包（open 内 build_panel 全量）；`open/close -> Result<bool, PopupError>`；删除 `overlay()`，暴露 `panel_node()`/`popup_node()`（若 gallery/测试需要）；`PopupSpec{tier:Auto, anchor:Node{trigger,edge:Bottom}, placement:{Bottom/Start,gap4,fallbacks:[Top/Start],margin4,match_anchor_width:false}, dismiss:Chain{outside:Replay...,escape:CloseTopmost,...}, focus:Take{restore:Opener}, opener:Some(trigger), animation:Fade}`；`PopupSurface{background:SurfaceOverlay, radius 4, padding 8, shadows: menu()}`；tab 切换后 `panel.request_measure()`；`on_open` 保留（open 成功一次）；新增 `on_close` 可选（设计 §5.2 提到 `open_state`/`on_open`/`on_close`）。
- **parity 要点**：确认回调一次、非确认变更不回调（3681-3738）；触发器开/关 toggle 与外部点击/Esc；hex Enter/失焦/非法回填归一一次（3876-3987）；hover 预览跨间隙保持/关闭清除（3546-3656）；tab 切换几何（3475）+ 新增首帧面板 rect 断言；`open_state` 信号对调用方可见（3403）；无 ghost（3637/3659）；a11y Dialog role + label（4423）；Canvas 局部坐标（4508/4529）；release 摘 action/input/scope（1001-1012）。
- **文件/测试落点**：color_picker.rs 生产 1-2246；纯函数测试 2248-2877 保留；UI 测试 2878-4553（40 个）重写 5 个（surface 阴影 3284、首帧 3306、K17 3349+3360、open_state 3803、a11y 4356/4423），其余保留。
- **P3 推迟**：Palette/HSLA 细节与 hover 预览的进一步打磨、`Scale` 动画、tab re-measure 的更完整像素门禁（P1 先补 `request_popup_measure`）。
- **风险**：空 open 模式必须收进单个 content 闭包（`PopupPanel` content 设计要允许"先拿面板节点再建内容"）；`overlay()` 删除影响 gallery 2833/2991/3141 与测试；`PopupA11y{role:Dialog}` 但非模态（旧 modal:false）是否与 a11y 契约冲突需复核（建议改 role=Group 或保留 Dialog 非 modal，引擎不强制）。

---

## 3. `PopupPanel` 设计草案

### 3.1 模块与文件落点

建议新增 `crates/ui_kit/src/kit/popup_panel.rs`（PG 允许扁平模块，`mod.rs` 仅声明），`kit::popup_panel::{PopupPanel, PopupSurface, PanelWidth, PanelMeasure, PopupPanelOptions}`；`kit/mod.rs` 增 `pub mod popup_panel;`。`ItemPanel` 改为 `pub(crate) mod item_panel`（从 menu.rs 拆出）或暂留 menu.rs（P1 可选）。

### 3.2 类型与方法签名（草案）

```rust
pub struct PopupSurface {
    pub background: Role,
    pub corner_radius: LogicalPx,
    pub padding: LogicalPx,
    pub shadows: Shadows,
}
impl PopupSurface {
    pub fn menu() -> Self;      // 等价 OverlaySurface::menu（SurfaceOverlay/4/4/双层阴影）
    pub fn dialog() -> Self;    // 等价 OverlaySurface::dialog（SurfaceOverlay/4/8/单层大阴影）
    pub fn tooltip() -> Self;   // 等价 OverlaySurface::tooltip（SurfaceInverse/4/4/无阴影）
}

pub enum PanelWidth {
    Auto,                                            // tooltip：宽度=内容自然宽+2*padding
    Exact(LogicalPx),                                // Menu/显式 width Dropdown：面板宽度固定（含 padding）
    AtLeast { min: LogicalPx, measure: PanelMeasure }, // Dropdown/Select：≥anchor 宽，可被内容撑宽
}

pub struct PanelMeasure { pub labels: Vec<SmolStr>, pub extra: LogicalPx } // ItemPanel 现 PanelMeasure 语义

pub struct PopupPanelOptions {
    pub surface: PopupSurface,
    pub width: PanelWidth,
    pub max_height: Option<LogicalPx>,
    pub on_close: Option<Rc<dyn Fn(&mut App, DismissReason)>>, // 可选；默认无
}

#[derive(Clone)]
pub struct PopupPanel { /* Rc<PopupPanelInner> */ }

impl PopupPanel {
    pub fn new(theme: ThemeHandle, key: impl Into<SmolStr>, options: PopupPanelOptions) -> Self;
    pub fn key(&self) -> &str;
    pub fn id(&self) -> Option<PopupId>;
    pub fn is_open(&self, app: &App) -> bool;              // id 存在且 popup_rect 非 None 且 state ∈ {Opening,Open,DismissPending,Closing}
    pub fn is_closing(&self, app: &App) -> bool;           // DismissPending|Closing
    pub fn open_state(&self) -> Signal<bool>;              // 旧 Overlay::open_state 语义（供 trigger a11y/bind_style）
    pub fn panel_node(&self) -> Option<NodeId>;            // surface（含 padding 的面板节点，布局盒==旧 overlay panel）
    pub fn body_node(&self) -> Option<NodeId>;             // 内容槽（content 闭包参数）
    pub fn popup_node(&self, app: &App) -> Option<NodeId>; // 引擎 popup 内容节点（替代旧 overlay_node）
    pub fn rect(&self, app: &App) -> Option<PopupRect>;
    pub fn open(&self, app: &mut App, window: WindowId, spec: PopupSpec,
                content: impl FnOnce(&mut App, WindowId, NodeId) -> Result<(), EngineError>)
        -> Result<PopupId, PopupError>;
    pub fn close(&self, app: &mut App) -> bool;            // 请求动画关闭（引擎 begin_close；见 §3.7 D1）
    pub fn close_now(&self, app: &mut App) -> bool;        // 立即卸载（release/回滚用）
    pub fn close_chain(&self, app: &mut App) -> bool;
    pub fn set_anchor(&self, app: &mut App, anchor: Anchor) -> bool;
    pub fn request_measure(&self, app: &mut App) -> bool;  // AtLeast 内容变化后重算宽度 + 引擎 request_popup_measure
    pub fn set_a11y(&self, app: &mut App, a11y: PopupA11y) -> bool; // 需要引擎 set_popup_a11y（§3.7 D1）
    pub fn sync(&self, app: &mut App) -> bool;             // 每帧：探测外部关闭、更新 open_state、触发 on_close 一次
    pub fn release(&self, app: &mut App);                  // close_now + 清 id/信号
}
```

### 3.3 节点树结构（与旧 Overlay 的对应）

```
root
├─ …普通内容
├─ popup-root（引擎预建：0×0 absolute + HitFocusSkip::Container + pinned tail）
│   └─ popup-content（引擎在 open_popup 内建：absolute 0,0 + overflow_hidden）
│       └─ {key}:panel（PopupPanel：surface 背景/圆角/padding/阴影/overflow_hidden；宽度=PanelWidth；max_height）
│           └─ {key}:body（PopupPanel：flex_col + min_*_0；content 闭包参数）
│               └─ 组件内容（ItemPanel 的 {key}:surface / ColorPicker {key}:panel.content / Dialog 根）
└─ debug-hud（引擎固定子节点，最上、pointer-transparent）
```

- `PopupPanel::panel_node()` 返回 `{key}:panel`，保持旧 Overlay panel 的调试 id 与布局盒语义（`gallery.menu:panel`、`gallery.color-picker:panel` 前缀不变）。
- 旧 `Overlay::overlay_node()` 返回引擎旧 wrapper；新 `popup_node()` 返回 `popup-content`（引擎 entry.node）。**`ui_kit::test_support::overlay_wrapper` 必须删除并改用 `popup_node`**（新架构下它会返回共享 `popup-root`，13 处调用全部受影响）。
- 旧 `scrim_node()` 由引擎 scrim 条目承接（`popup_stack` 中 `tier=Scrim`），kit 不暴露。

### 3.4 打开 / 测量 / 宽度 / 高度

`PopupPanel::open` 流程：
1. 若 `self.id` 已存在且 `is_open`：按组件语义返回既有 id / 或 `set_anchor` + `raise_popup`（默认不重建，保持旧 Overlay 幂等；`open_popup` 本身不幂等）。
2. `App::open_popup(window, spec, |app, window, content_node| { 建 panel/body；调用调用方 content(panel/body) }, Some(dismiss_handler))`。
3. 记录 `PopupId`；若 `PanelWidth::AtLeast` 或 `Exact` 尚未写入：`measure_and_apply(app)`。
4. `app.request_popup_measure(id)`（确保引擎在帧内 settle 使用最终宽度）；返回 `PopupId`。
5. 首可见帧由引擎 settle 保证（`settle` 在 `layout_text` 后、`paint` 前；`PopupState::Open` 同帧置位）。**kit 内不再调用任何 `refresh_text`/`compute_layout` 之外的定位逻辑**。

`measure_and_apply`（AtLeast）：
1. `app.refresh_text(window)`（文本排版）→ `window.compute_layout()`（taffy）→ 读 `body` 自然宽。
2. 由 `PanelMeasure` 构建的隐藏 measure 节点（复用 menu.rs:1879-1910 的 4096px NoWrap 文本）读 `tree.text_caret_rect` 得最宽行（menu.rs:526-554）。
3. `target = max(min, natural + extra + 2*padding)`；`≤ viewport - 2*margin`；写 panel style 宽度；`app.request_popup_measure(id)`。
4. 若宽度变化会改变内容换行（tooltip 的 `max_width` 在此 clamp），重复 1-3 一次（最多 2 轮）；超轮记录诊断（引擎已有 `PopupDiagnostic::Unsettled`）。

`Exact`：建 panel 时直接写宽度，无需测量。`Auto`：panel `width:auto`，由内容自然宽决定（tooltip/dialog 内容显式宽度）。

**职责边界（与引擎 settle）**：

| 关注点 | 引擎 `ui_core::popup` | kit `PopupPanel` |
|---|---|---|
| 定位求解/flip/shift/clamp/max-size | ✅ `solve_placement`+`settle` | ❌ 不计算 origin |
| 窗口坐标解析/滚动补偿 | ✅ `resolve_anchor`（Node/NodeRect/WindowRect） | 只提供 `Anchor` |
| 布局后二次布局/文本/虚拟列表/clamp 重跑 | ✅ `settle::relayout` | ❌ |
| 内容自然宽测量（文本行宽） | ❌（引擎不懂文本语义） | ✅ `PanelMeasure` + `measure_and_apply` |
| 面板 surface/阴影/圆角/padding/裁剪 | ❌ | ✅ `PopupSurface` + panel 节点 |
| 动画（fade/slide/paint_offset） | ✅ `popup::animation`（120/90ms，引擎常量） | 只选 `PopupAnimation` |
| 关闭事务状态机 | ✅ `DismissPending/Closing/Closed` + `begin_close` | ✅ 触发 + `on_close` 一次性回调 + open 信号 |
| dismiss 事件/矩阵/replay | ✅ `popup::dismiss` | 只注册 `on_dismiss`（P2 MenuChain 用） |
| 焦点快照/恢复/模态 scope | ✅ `popup::focus` | 只给 `FocusPolicy`/`opener` |
| a11y 角色/关系投影 | ✅ `a11y::popup` | 只给 `PopupA11y` |
| damage/paint 契约 | ✅ `painted_extent`+popup transform | 不 direct damage |

### 3.5 关闭事务与 `on_dismiss`

- `open` 时向引擎注册 handler：`Box<dyn Fn(&mut App, PopupDismiss) -> DismissOutcome>`。handler 内：
  1. 记录 `reason`（供 `on_close`）。
  2. 调调用方可选 `on_dismiss`（P2 MenuChain 用；P1 无）。
  3. 返回 `DismissOutcome::Close`（默认）/`CloseChain`/`Ignore`。
- 引擎 `apply_outcome` → `begin_close`（fade）→ 动画完成 `finalize_close` 删除节点。
- `PopupPanel::sync` 每帧检查 `popup_rect(id)` 是否消失（或引擎新增 `popup_state(id)`）：消失后清 `id`、写 `open_state=false`、**只在"曾 opened 且未回调过"时**调 `on_close(reason)` 一次（reason 来自 handler；程序化 `close()` 记 `Programmatic`）。
- `close()`：优先调引擎 `begin_close`（§3.7 D1）；若引擎暂不提供，退化为"kit 自行置 Closing 标记 + 立即 `close_popup`"（**丢失淡出动画，不可接受**，故 D1 必须先落）。
- `close_now()`：`App::close_popup` 直接卸载（立即消失；用于 release/构建失败回滚/关闭中重开前的清场）。
- 关闭中子级先于父级：由引擎 `subtree(...).rev()` 保证（app/popup.rs:448-462）；kit 只需保证 `on_close` 在父级回调时不重复触发子级回调。

### 3.6 旧 `Overlay` 能力 ↔ 新实现对应表

| 旧 `Overlay` 能力 | 新实现位置 |
|---|---|
| `new(app,theme,layer,router,key,options)` | `PopupPanel::new(theme,key,options)`（无 layer/router） |
| `open(app,window,anchor,content)` 一次性 measure/reveal/fade | `PopupPanel::open(app,window,spec,content)` + 引擎 settle（首帧即稳定） |
| `open` 幂等/已开返回 false | `PopupPanel` 判 `id`+`is_open`（组件层） |
| `measure`/`place`/`sync` | 引擎 settle；kit 只 `measure_and_apply`+`sync`（探测关闭） |
| `reveal`/`revealed` | 删除（引擎 `PopupState::Open` 同帧置位，首帧可见） |
| `progress`/`fade`/group opacity | 引擎 `PopupTransform`（paint_offset/opacity）+ `PopupAnimation` |
| `close`/`close_now`/`release` | 同签名（close 走 D1 动画；close_now 立即） |
| `panel_node`/`overlay_node`/`scrim_node` | `panel_node`/`popup_node`/移除（scrim 由引擎） |
| `open_state`/`is_open` | `open_state` Signal + `is_open`（引擎状态派生） |
| `key`/`options` | `key`；surface 由 `PopupPanelOptions` 提供 |
| `set_a11y` | `set_a11y`（需 D1） |
| `OverlaySurface`/`Scrim`/`OverlayA11y` | `PopupSurface`/`ScrimSpec`/`PopupA11y` |
| `OverlayOptions.{placement,gap,margin,flip}` | `PlacementSpec`（flip → fallbacks） |
| `OverlayOptions.{band,slot,modal,restore_focus}` | `PopupSpec.{tier,parent/focus/opener}` |
| `OverlayOptions.dismiss` | `PopupSpec.dismiss` |
| `OverlayOptions.fade` | `PopupSpec.animation` |
| `AnimationRouter` | 引擎 `popup::animation::tick` |

### 3.7 设计文档未明确、需 lead/用户决策的点

| # | 问题 | 影响 | 建议 |
|---|---|---|---|
| D1 | **程序化动画关闭 API**：`App::close_popup` 当前立即删节点（app/popup.rs:431-462），只有 dismiss 路径 `begin_close` 有 fade（dismiss.rs:110-210）。PopupPanel `close()`/Dialog 关闭动画/组件 toggle 都依赖它。 | 无此 API 则 kit 只能立即关（行为回退）或自行实现动画（双机制，违反 §4.1） | 引擎新增 `pub fn begin_close_popup(&mut self, id) -> Result<(), PopupError>`（内部 `dismiss::begin_close`），或把 `close_popup` 改为 begin_close 语义并新增 `close_popup_now`；另加 `App::popup_state(id) -> Option<PopupState>` 与 `set_popup_a11y(id, PopupA11y)` |
| D2 | **关闭完成通知**：tooltip 的"淡出窗口内不得复活/不得重复 open"（3 个测试）需要知道关闭完成；目前只能每帧 `popup_rect(id).is_none()` 轮询。 | tooltip/menu 重入语义需要 | 可在 PopupPanel `sync` 轮询（可接受）；或引擎加 `on_popup_closed(id)` 回调。建议先轮询（P1），P2 MenuChain 再评估 |
| D3 | **`AtLeast` 宽度归属**：引擎 `match_anchor_width` 只参与求解不写布局（placement.rs:36-46），`RequestMeasure` 只置脏（settle 不做文本测量）。 | menu/dropdown/select 宽度保真 | 采用 §3.4：`PanelWidth::AtLeast` 由 PopupPanel 测量并写 panel 宽度，引擎只负责放置；需确认这样不违反"kit 不自算几何"（宽度不算 origin，可接受） |
| D4 | **PopupSpec 是否暴露给组件调用方**：设计 §5.2 写 `open(app, window, spec: PopupSpec, ...)`，但 `PopupSpec` 含 tier/focus/dismiss/scrim/z 等引擎类型。 | kit 公共 API 是否直接泄漏引擎类型 | 建议公开（`ui_core` 已是依赖；组件构造用户需要定制 placement/dismiss 时可直接改 spec），但 `Dropdown/Select` 等高层组件把 spec 收进 options；PopupPanel 作为底层保留完整 spec |
| D5 | **`PopupPanel` 是否暴露 Surface 配置**：设计 §4.1/§5.2 的答案倾向"是"（`PopupSurface` 参数化），但 Menu/ColorPicker 的阴影配方差异需要保留。 | 组件外观保真 | 暴露 `PopupSurface`（see §3.2），并提供 `menu()/dialog()/tooltip()` 预设；ColorPicker 用 `PopupSurface{padding:8,..menu()}` |
| D6 | **root 菜单/右键菜单的 tier 与模态**：旧 root Menu `modal=true`（焦点陷阱+背景阻断），设计 §4.6 将菜单归为 `Auto`（非模态）。 | 背景 hover/scroll/点击行为、a11y modal scope | 需产品裁决：P1 是否保持"菜单打开时背景 hover 被过滤/滚动被吞"（旧行为）；建议 root Menu 用 `tier:Auto + FocusPolicy::Trap`（保留 modal scope，但不触发 scrim/Modal tier 的自动关链），子菜单 `Take`；或按设计改 `Take` 并接受行为变化 |
| D7 | **外部点击 Consume vs Replay（D14）**：旧 menu/context_menu 外部按下被吞（背景 clicks==0，context_menu.rs:533-544）；D14 默认 Replay。 | 菜单/下拉外部关闭后该次点击是否作用于背景 | 建议 menu/context_menu 用 `OutsidePress::Consume`（桌面惯例），dropdown/select/colorpicker 用 `Replay`；需用户确认 |
| D8 | **Tooltip 与 Modal 的层序/关闭**：旧 Band::Tooltip 永远最高且 Dialog 打开不关 tooltip；新 `Hint` 在 modal 打开时被 `close_hints` 关闭、z=Newest。 | tooltip 行为变化 | 设计 §4.6 明确"modal 打开时一律隐藏/关闭 Hint" → 按设计；但"tooltip 在已打开 Dialog 之上"需改为"Dialog 打开后 tooltip 关闭"（测试 1249/1322 重写） |
| D9 | **`RestoreFocus` 语义未实现**：引擎 focus.rs 只用快照，未按 `{Opener,Previous,RootFallback}` 分支（全仓 grep 仅存枚举/测试）。 | 组件无法表达"回 opener"vs"回之前焦点" | P1-6 补齐枚举分支，或 PopupPanel 统一用 `opener`+快照；需引擎 owner 确认 |
| D10 | **`MenuMode::ListBox` 删除后 List 角色归属**：select 面板 role=None、行 ListBoxOption、恰好 1 个 List；menu 面板 role=Menu、行 MenuItem、`Menu` 下抑制 List。 | a11y 断言 | 按现有 role 映射迁移；`MenuMode::ListBox` 删除需同步删 menu.rs:2605-2640 测试，转移到 Select 用例 |
| D11 | **`OverlayError` 移除后的错误类型**：组件 `open` 返回 `Result<bool, OverlayError>`；`DialogError::Overlay`/`MenuError::Overlay` 被 8+ 文件匹配。 | 公共错误面 | 统一 `PopupError`；`DialogError::Popup(PopupError)`、`MenuError::Popup`；引擎错误保留 `EngineError` 来源；`OverlayError::PlacementConfig` 删除（`PopupError::InvalidPlacement` 承接） |
| D12 | **ColorPicker `OverlayA11y::dialog` 但非模态**：新 a11y 投影按 `FocusPolicy::Trap` 派生 modal；非模态 Dialog role 是否可接受？ | a11y 契约/测试 | 建议保留 `role=Dialog` + `FocusPolicy::Take`（非 modal），或改 `role=Group`；需 a11y owner 确认 |
| D13 | **动画时长**：旧 kit 150ms（`ComponentTokens.animation_tick`），新引擎 120ms 开/90ms 关（固定常量）。 | 视觉/测试时序 | P1 接受引擎常量；若产品要求 150ms，引擎需 per-popup duration 扩展（P3） |

---

## 4. 宿主迁移计划

### 4.1 `crates/widget`（宿主框架）

**目标形态**：`WidgetServices` 不再持有 `layer/animation_router`；`WidgetCx::{layer,animation}` 删除；kit 组件构造只需 `theme`（+ key/options）。保留 `frame_router`（单槽每帧扇出）。

**改动文件**：

| 文件 | 改动 |
|---|---|
| `src/services.rs:11,28-30,45-53,74-82,176-178` | 删 import 两行、两字段、builder 两参；`WidgetServicesBuilder` 两字段删除；`build` 解构同步 |
| `src/cx.rs:18-20,127-137` | 删 `layer()/animation()` 与 import |
| `src/cx.rs:300-330`（测试） | 删 `std::ptr::eq` layer/animation 断言 |
| `src/binding.rs:445-457` | 测试装置 `Layer::new()/AnimationRouter::new()` 删除 |
| `src/test_support.rs:33-35,97-160,218-230,247-250` | `WidgetTestApp` 删 `layer/animation` 字段与访问器；构造不再注入 |
| `src/services.rs:184-731`（测试） | `Overlay` 测试（412-525）改 `open_popup`；band/pointer 断言删除 |

**替换 API 草案**：

```rust
// 组件构造（去 layer/router）
Select::new(app, theme, key, options)
Dropdown::new(app, theme, key, options)
Tooltip::new(app, theme, key, options)
Menu::new(app, theme, key, entries, options)
ContextMenu::new(app, theme, key, entries, options)
DialogBuilder::build(app, theme, key)
ColorPicker::new(app, theme, key, initial, size)
```

`WidgetServices::builder(app, runtime, translate, theme, frame_router)`（6 参）。若未来需要宿主级"popup 策略"（如 reduce_motion 控制动画），由组件读 `services.reduce_motion` 决定 `PopupAnimation::None`（保留现有字段）。

### 4.2 `crates/kytrix`（组合根）

| 文件 | 改动 |
|---|---|
| `src/composition.rs:30,149-166,212-214,273-299,350-357` | 删 `Layer/AnimationRouter` 创建、字段、builder 实参 |
| `src/shell/dialogs.rs:203-220` | `DialogBuilder::build(app, theme, key)`（去 `composition.layer/animation_router`） |
| `src/shell/title_bar.rs:251-261`（经 `ProfileMenu`） | 无直接改动（profile_ui 内部迁移） |
| `src/shell/dialogs.rs:175-200`（Alert） | `DialogBuilder::alert` 不变 |

### 4.3 业务 ui crate（25 文件）

见 §1.6 逐文件表。模式：
1. host 结构删 `layer()/animation()`（instrument_selector_ui、market_participants_ui、profile_ui、settings_ui）。
2. 所有组件构造去两参（Select/Dropdown/Tooltip/Menu/ContextMenu/Dialog/ColorPicker）。
3. 直接 `Overlay::new` 的 2 处（market_participants_ui/menu.rs:271、profile_ui/profile.rs:176）改 `PopupPanel`：
   - market_participants panel：`tier:Modal` + `FocusPolicy::Trap{restore:Opener, initial:Container}` + `DismissPolicy::Explicit{outside:true,escape:true}` + `PopupSurface::menu()` + `PopupA11y{role:Menu,label}` + `opener:Some(trigger)` + anchor `Node{trigger,Bottom}` + placement Bottom/Start gap4。`OverlayOptions.scrim=None` → `PopupSpec.scrim=None`（Trap 无 scrim 合法）。
   - profile_ui panel：`tier:Auto` + `FocusPolicy::Take{restore:Opener}` + `DismissPolicy::Chain{outside:Replay,escape:CloseTopmost,anchor_lost:true,focus_lost:true}` + menu surface + `PopupA11y{role:Menu,label}` + anchor `Node{trigger,Bottom}` + placement Bottom/End gap4。
4. `error.rs`/`dialog.rs` 的 `OverlayError` 匹配改 `PopupError`（market_participants_ui/error.rs:10/16/28-30；updater_ui/dialog.rs:30/962-963；widgets 6 个 dialog.rs 的同款映射）。
5. test_support 的 `overlays().len()`/`layer()` 改 `popup_stack(window).map_or(0,|s| s.entries.len())`。

### 4.4 `crates/widgets`（14 crate / 15 生产文件）

机械替换（见 §1.7）：删 `let layer/router` 两行，构造去两参；6 个 `dialog.rs` 改 `DialogBuilder::build(app, theme, key)` 与 `DialogError::Popup` 映射。`crates/widgets/AGENTS.md` §5/§8.1 的"组合根三单例（`cx.layer()/cx.animation()`）"段落需在 P1 完成后更新（属文档回写，P3-7 或本批顺手，非 `.docs` 之外的代码注释不写）。

### 4.5 测试支持

- `WidgetTestApp`：删 `layer/animation`；断言浮层用 `app.popup_stack(window)` / `app.popup_rect(id)` / `popup_state`。
- business test_support 4 处 `overlays()` → `popup_stack().entries.len()`。
- `ui_core/tests/hud.rs` → `open_popup`，断言 HUD 在 popup root 之后（`root.children().last()==hud`，popup root 在其前）。
- `ui_kit/src/kit/test_support.rs::overlay_wrapper`：删除/重写为 `popup_node` 辅助（13 处调用）。

### 4.6 执行顺序与编译依赖图

```
[W0] 引擎补缝（D1/D2/D9）：begin_close_popup、popup_state、set_popup_a11y、RestoreFocus 分支
      └─ 编译独立，先行合入
[W1] PopupPanel + PopupSurface（ui_kit 新模块；无消费者）
[W2] ItemPanel 去 Overlay（menu.rs 内部重构）+ Menu/ContextMenu/Dropdown/Select 迁移
      └─ 同步改：gallery ⑫ 演示、settings_ui(4 Select)、instrument_selector(1 Select)、
         widgets(8 文件/9 处 Select)
      └─ 必须同批：Menu::new/Select::new/Dropdown::new 签名变化
[W3] Tooltip 迁移（ui_kit）+ 6 个宿主构造 + gallery
[W4] Dialog/Alert 迁移（ui_kit）+ 13 个 DialogBuilder::new + 7 个 alert 源 + 6 个 widgets dialog.rs + kytrix/settings/profile/market/updater/instrument + gallery
[W5] ColorPicker 迁移（ui_kit）+ candlestick/panel/colors.rs + gallery
[W6] 宿主管线清理：WidgetServices/Cx/binding/test_support、kytrix composition/dialogs、
     4 个 host 包装、业务 test_support、widgets 测试辅助（此时已无 layer/router 消费者）
[W7] 直接 Overlay 消费方（market_participants_ui/menu.rs、profile_ui/profile.rs）→ PopupPanel
      （可与 W2 并行；但需 W1）
[W8] P1-11 删除旧 API（旧引擎 overlay + kit overlay/layer + AnimationRouter）+ 修引擎内部耦合
[W9] P1-12 测试重写 + gallery dump/pixel-check + 全量门禁
```

编译依赖约束：
- W2 必须同时改所有 `Select::new/Dropdown::new/Menu::new` 调用点（签名破坏），否则 workspace 不编译；W3/W4/W5 同理，彼此可顺序做（组件签名独立）。
- W6 必须在 W3-W5 之后（此时组件构造不再要 layer/router）；但 `WidgetServices` 两字段可在 W2 后暂时保留（无消费者）以减小提交面，最终 W6 删除。
- W7 依赖 W1；与 W2 无交集（不同文件）。
- W8 依赖 W2-W5 + W7（kit 不再 create_overlay、不再用 OverlaySlot）；引擎内部耦合（a11y/tree、window/a11y、node/tree overlay_containers、input 兜底、frame resolve）同批清理。
- W9 依赖 W8（`overlays()`/`create_overlay` 已删才可改写测试）。

### 4.7 需要同步修改的测试（宿主/业务层）

| crate | 测试文件:行 | 断言 |
|---|---|---|
| instrument_selector_ui | `instrument_selector/tests.rs:988,1000,1011`；`instrument_button/tests.rs:265,296` | `layer().len/is_empty`、`overlays()` |
| market_participants_ui | `test_support.rs:474` | `overlays().len()` |
| profile_ui | `test_support.rs:794`；`menu.rs:171-175`（release 断言） | `overlays()` |
| settings_ui | `settings_tests/test_support.rs`、各页面测试 | 组件构造（无直接 layer 断言） |
| updater_ui | `test_support.rs`、`dialog.rs` 错误映射 | 构造/错误 |
| widgets | §1.7 测试专用行（force_order 1239、coin_market tests 1775/1797、financial_index 2499/2506/2555/4048、market_trade 1564） | `AnimationRouter`/`layer()`/`overlays()` |
| widget | `services.rs` 测试 412-525、`cx.rs` 319 | band/pointer/open |
| kytrix | `ui_core/tests/hud.rs` 等 | HUD 层序 |

---

## 5. 删除清单（P1-11，对照设计 §8）

### 5.1 文件级删除

| 文件 | 行数 | 前置条件 |
|---|---:|---|
| `crates/ui_kit/src/kit/overlay.rs` | 2871 | 所有组件迁移完（W2-W5）+ gallery 不再引用 |
| `crates/ui_kit/src/kit/layer.rs` | 964 | 同上 |
| `crates/ui_core/src/app/overlay.rs` | 185 | kit 不再 `create_overlay` |
| `crates/ui_core/src/interaction/overlay.rs` | 330 | 引擎 focus/a11y/hit 不再引用 |

### 5.2 类型/API 级删除

| 位置 | 对象 |
|---|---|
| `crates/ui_kit/src/kit/mod.rs:17,22` | `pub mod layer;`、`pub mod overlay;` |
| `crates/ui_core/src/app.rs:42,302,1242-1260,1331-1341` | `create_overlay/close_overlay/place_overlay/bring_overlay_to_front/overlays/dismiss_overlay`、`overlay_style`、`interaction::{Overlay,OverlayDismiss,OverlayOptions,OverlayPlacement}` imports |
| `crates/ui_core/src/interaction.rs:4,327-418,601` | `overlay` 模块声明、`OverlayDismiss/OverlayPlacement/OverlayOptions/Overlay/OverlaySlot`、`NodeInteraction.overlay` 字段 |
| `crates/ui_core/src/app/frame.rs:59` | `resolve_overlay_placements` 调用 |
| `crates/ui_core/src/app/input.rs:13,97,238-249` | 旧 overlay imports/兜底 dismiss/旧 outside-click remove_node |
| `crates/ui_core/src/app/window_set.rs:80,92` | 旧 overlay_slot 捕获与旧 focus scope 清理 |
| `crates/ui_core/src/node/tree.rs:21,407,554-567,1398` + `arena.rs:89,639-640` + `content.rs:11,1021` + `traverse.rs:93-97` | `overlay_containers`、`note_overlay_container`、`overlay_slot` |
| `crates/ui_core/src/interaction/focus.rs:28,569-620`（测试） | 旧 `in_modal_scope` 包装、旧 OverlaySlot 测试 helper |
| `crates/ui_core/src/window/a11y.rs:12-13`、`a11y/tree.rs:360,1288-1293,2251-2260` | 旧 modal scope / overlay a11y 回落 / 测试 `declare_overlay` |
| `crates/ui_core/src/a11y/action.rs:781,819`（测试） | `create_overlay` 用例 |
| `crates/ui_core/src/test_support/{a11y.rs,gui_smoke.rs}` | 旧 overlay 测试 helper |
| `crates/ui_core/src/app/interaction_tests.rs` | 22 处 `create_overlay` + `overlays()` 断言（改新 popup 或删除） |
| `crates/ui_core/tests/hud.rs:335-359` | `hud_stays_above_overlays` 改 `open_popup` |
| ui_kit 组件内 import/签名 | `menu.rs:29-34`、`dropdown.rs:21-26`、`select.rs:36-41`、`tooltip.rs:20-24`、`context_menu.rs:13-15`、`dialog.rs:31-34`、`color_picker.rs:35-38` 的 `layer::{Band,Layer,LayerSlot}`/`overlay::{...}` |
| `MenuMode::ListBox` | menu.rs:990-1009/1054/2605-2640 |
| 死 API/字段（审计确认零调用） | `Menu::surface_node`(1276)、`Menu::key`(1240)、`MenuEntry::is_actionable`(980)、`ContextMenu::key`(128)、`ContextMenuInner.window`(92)、`PanelState.height`(143)、`MenuOptions` 的 band/submenu_delay/label/mode builder（若确认无外部调用） |
| 宿主 | `WidgetServices.{layer,animation_router}`、`WidgetCx::{layer,animation}`、`WidgetTestApp.{layer,animation}`、4 个 host 访问器、kytrix `Composition.{layer,animation_router}` |
| 测试辅助 | `ui_kit::test_support::overlay_wrapper`（13 调用） |

### 5.3 `rg` 零引用检查（P1-11 完成判据）

```powershell
$env:CARGO_TARGET_DIR='E:\kytrix\target'
# 1) kit 旧 API
rg -n --glob '*.rs' 'kit::overlay|overlay::\{|OverlayOptions|OverlaySurface|OverlayA11y|OverlayError|AnimationRouter' crates
# 2) kit layer
rg -n --glob '*.rs' 'kit::layer|layer::\{|Layer::new|LayerSlot|Band::' crates
# 3) 旧引擎 API
rg -n --glob '*.rs' 'create_overlay|close_overlay|place_overlay|bring_overlay_to_front|resolve_overlay_placements|dismiss_overlay|\.overlays\(' crates
rg -n --glob '*.rs' 'OverlaySlot|OverlayDismiss|OverlayPlacement|overlay_style|note_overlay_container|overlay_containers|overlay_slot' crates
# 4) 共享面板/宿主
rg -n --glob '*.rs' 'ItemPanel|MenuMode::ListBox' crates/ui_kit/src
rg -n --glob '*.rs' '\.layer\(\)|\.animation\(\)|animation_router|services\.layer' crates
rg -n --glob '*.rs' 'overlay_wrapper' crates
# 5) 全量编译
cargo check --workspace --all-targets
```

**注意**：`rg 'layer'` 会命中纹理数组/atlas 的 `layer` 词（ui_core render/*、image atlas），不能作为删除判据；只用上面的组合模式。`Overlay` 单词也会命中 `DamageOverlay`（test_support/diagnostics.rs，无关，保留）与 `A11yPopupOverlay`（新代码，保留）。

### 5.4 删除后必须仍通过的门禁

- `cargo check --workspace --all-targets`
- `cargo test -p ui_core -p ui_kit -p widget`（本批允许先跑这三个，全仓门禁见 §8）
- gallery `--pixel-check`（重写后）exit 0
- `rg` 上述模式零命中

---

## 6. P1-12 测试重写清单

### 6.1 kit 组件测试（首可见帧/新节点来源）

| 文件 | 测试总数 | 必改（行） | 说明 |
|---|---:|---|---|
| `overlay.rs` | 37 | 全部（随文件删除） | 首帧 4 个（1631/1659/1734/2712 附近）迁到 PopupPanel settle 断言；`revealed/progress/overlay_node/scrim_node` 断言删除 |
| `menu.rs` | 16 | 2385、2432、2499、2560、2605 | 子菜单 root_children、Esc 逐层、a11y wrapper、modal scope、ListBox 删除；`overlay_wrapper` 2507/2618 重写；`settle()` 2108 改 1 帧断言 |
| `dropdown.rs` | 18 | 1059、1153、1244、1276、1298、1323、1388 | 节点来源、AtLeast 宽度、a11y、idle、expanded |
| `select.rs` | 23 | 1685、1810、1835、1865、1899、2005 | a11y wrapper、AtLeast、换行测量、`:surface` 焦点 |
| `tooltip.rs` | 20 | 675、741、844、895、990、1052、1105、1209、1249、1322 | progress/revealed、a11y actions、fade 重开、overlays/band |
| `context_menu.rs` | 12 | 533、563、579、751、828（+ Layer 断言） | Consume/replay、Auto 替换、重开保根、z/band、滚动 |
| `dialog.rs` | 38 | 1383、1437、1474、1780、1842、1880、2107、2140、2314（+ layer.len 5 处） | 组淡出/帧内卸载/a11y wrapper/多框叠放/ContextMenu 驱逐/Alert 独占/reopen |
| `color_picker.rs` | 73（纯函数 2248-2877 + UI 2878-4553） | 3284、3306、3349（K17 3360）、3797、4356/4423 | surface 配方、首帧、坏测试、open_state、a11y |
| `alert.rs` | 11 | 0（保留） | 仅 harness 构造（无 overlay） |
| `layer.rs` | 14 | 全部（随文件删除） | 语义迁到 popup/tests 或删除 |

**重写统一口径**：
1. 删除 `settle()` 的"多帧 drain 后断言"（K17），改为：`open` 后 `drain` 一帧 → 断言 `popup_rect(id).hit == 稳定帧 rect`（首可见帧 == 稳定帧）。
2. 断言 `FrameRecord.damage_full == false`（非模态 popup 开关/移动）。
3. 节点来源：`panel_node()`（PopupPanel surface）、`popup_node()`（引擎内容节点）；`overlay_wrapper` 删除。
4. a11y：wrapper role 断言改为 `popup` 内容节点（`popup_node`）的 `A11yPopupOverlay` 投影；内部面板仍 GenericContainer；`Menu` 下抑制 List 规则保留。
5. 关闭断言：`close()` 后事件循环内 `is_closing()==true`、`PopupState::Closing`，动画完成（`advance(90ms+)`）后 `is_open()==false` 且节点消失；`close_now()` 立即消失。
6. `overlays()`/`layer()` 断言全部改 `popup_stack(window)`（entries/order/z/tier/state/rect）。

### 6.2 ui_core 测试

| 文件 | 数量 | 处置 |
|---|---:|---|
| `app/interaction_tests.rs` | 22 `create_overlay` + 5 `overlays()` | 改为 `open_popup`/`dismiss_topmost`/`popup_stack`；旧的"引擎直接删节点"断言（1877/1897/2120/3616）改为"dismiss 事件 → handler → Closing → 清理" |
| `test_support/gui_smoke.rs` | 1 `create_overlay` + 3 `overlays()` | 同 |
| `test_support/a11y.rs` | 1 `create_overlay` | 同 |
| `interaction/overlay.rs` | 文件删除（10 个测试） | 语义由 `popup/dismiss.rs`/`popup/focus.rs` 测试承接；缺口（outside click 只关最上层、modal 阻挡）需确认新 popup tests 覆盖 |
| `a11y/action.rs` | 1 `create_overlay` | 改 `open_popup` |
| `a11y/tree.rs` | 测试 `declare_overlay` 5 个用例 | 删除/改新 popup a11y 投影 |
| `interaction/focus.rs` | 569-620 旧 OverlaySlot 测试 | 删除/改新 modal scope |
| `tests/hud.rs` | 1 | 改 `open_popup` |
| `app/overlay_tests.rs`（设计新增） | — | P1 补 I9（popup root 0×0 不改变主树布局）+ damage_full 断言 |

### 6.3 Gallery 重写

| 位置 | 现状 | 目标 |
|---|---|---|
| ⑫ 浮层演示（1333-1540） | 旧 `Overlay::new` + Dropdown/Select/Menu/ContextMenu + `Band/Layer` | PopupPanel + 新组件构造；`Anchor::Node{edge}` |
| ⑬ Dialog/Alert（1557-1638） | DialogBuilder + layer/router | 新 build 签名 |
| ⑭ ColorPicker（1646-1664） | ColorPicker + layer/router | 新构造 |
| `--probe`（1788-1845） | 打印 rect | 保留，改用 popup_rect |
| `--dump-overlay`（1848-1944） | 节点树 dump + `overlay-check` 子节点数量 | 改 `popup_stack` 快照 + 首帧 rect == 稳定帧 + `damage_full==false`；保留命令名 |
| `--pixel-check`（2779-3450） | `ColorPicker::overlay().revealed/panel_node/overlay_node` 驱动，headless `PersistentPixels` | 改 `debug_stack`+`popup_rect`+像素断言；保留命令名/退出码（0 PASS / 3 FAIL）与注入故障负向自检；**不得静默移除**（设计 §8/§11.3-9） |
| `run_pixel_check`（3263） | — | 重写场景：打开→稳定→移动锚点→关闭→重开；基线排除当前 popup paint_rect |
| `--auto-close`/`--idle-check` | 保留 | 保留 |

---

## 7. 风险 / 未决点

### 7.1 需 lead/用户决策（阻塞实现，按优先级）

1. **D1 引擎补缝 API**：`begin_close_popup` / `popup_state` / `set_popup_a11y` / `RestoreFocus` 分支是否在 W0 由引擎批补？不补则 PopupPanel 无法实现动画关闭、Dialog 动态标题 a11y、tooltip closing 检测。（建议：补，工作量约 100-150 行 + 测试。）
2. **D6 菜单模态语义**：root Menu 保持旧"焦点陷阱+背景阻断"（`tier:Auto + FocusPolicy::Trap`）还是按设计改非模态 `Take`？影响 menu/context 的 modal scope、背景 hover、滚动、a11y 测试。
3. **D7 外部点击 Consume/Replay**：menu/context_menu 用 `Consume`（保旧行为），dropdown/select/colorpicker 用 `Replay`（D14）是否可接受？需用户确认 D14 对菜单的例外。
4. **D4/D5 PopupPanel 公开面**：`PopupPanel::open(spec: PopupSpec)` 直接暴露引擎类型 + `PopupSurface` 参数化，是否作为 kit 公开 API（影响后续 P2 `MenuChain` 与业务自定义浮层）。
5. **D8 Tooltip 与 Modal 层序**：接受"modal 打开关闭 tooltip"（设计 §4.6）而非旧的"tooltip 压在 Dialog 上"？
6. **D12 ColorPicker a11y role**：非模态 Dialog role 是否保留。
7. **D11 错误类型**：`DialogError`/`MenuError` 增加 `Popup(PopupError)` 变体并删除 `Overlay(OverlayError)`，还是把 `PopupError` 展平进各自的 `Engine` 变体？影响 8+ 业务文件的 match（`DialogError::Overlay(OverlayError::Engine(e))` 等）。
8. **D9/D13 `RestoreFocus` 与动画时长**：是否在 P1 补齐枚举分支/接受 120/90ms。
9. **`ItemPanel` 拆分**：P1 是否把 `ItemPanel` 从 menu.rs 拆到 `item_panel.rs`（推荐；降低 menu.rs 2641 行复杂度），还是暂留（减少 diff）。
10. **`MenuMode::ListBox` 删除时机**：P1-9 直接删（含测试），还是保留到 P2？设计 §8 列 P1。
11. **typeahead / Select 搜索**：设计 §5.2 写 Dropdown/Select「键盘/typeahead」、Select「多选/搜索语义」，但当前代码 **完全没有** typeahead/搜索（grep 零命中，`SelectOptions` 无搜索字段）。P1 是 parity 迁移：建议 typeahead 归入 P2（MenuChain 键盘表一并做）、Select 搜索单独立项；需用户确认是否允许设计文案与 P1 范围不一致。

### 7.2 技术需复核项（实现 agent 先验证再落）

| # | 项 | 验证方法 |
|---|---|---|
| R1 | `Anchor::NodeRect` 的 `content_offset` 是否与 `node_screen_rect` 重复计滚动 | 引擎单测 + 组件滚动容器用例 |
| R2 | `focus::focusable` 是否把 `tab_stop(true)` surface 视为可聚焦（select 1899 焦点断言依赖） | 读 `interaction/focus.rs:14-20` + 小测试 |
| R3 | `open_popup` 在 `sync`（帧 hook）内调用后，同帧 settle/绘制是否保证（tooltip open 在 sync 内） | gallery/widget 首帧测试 |
| R4 | `overlay_wrapper` 替代语义：`popup_stack` 条目 node 反查 / panel 祖先 | 改 helper 后跑 13 处测试 |
| R5 | `OutsidePress::Consume` 下背景是否完全收不到 press+release（D14 replay 例外） | dismiss 测试 |
| R6 | 非模态 popup 打开时背景滚动是否被吞（旧 menu modal 行为） | 引擎 input/scroll 路径 + 测试 |
| R7 | `Tier::Scrim` 回收/`owner_modal` 后代关闭对 Dialog 内 Select 的影响 | popup/tests 已有，跑通 |
| R8 | `TestApp`/`WidgetTestApp` 下 `drain` 一帧是否完成 settle（`PopupState::Open`） | 首帧断言探针 |
| R9 | `ComponentTokens.animation_tick`(150ms) → 引擎 120/90ms 的视觉差异是否可接受 | 人工/gallery 观察 |
| R10 | `max_height`（kit）与引擎 `MaxSize::Viewport` 的 double-clamp 是否导致面板尺寸与旧不一致 | menu/select 高度测试 |

### 7.3 已识别风险（实现内自决，不需用户）

- `Dropdown::release` 未释放 scope（既有泄漏）→ 迁移时对齐 Select。
- `Dialog::recenter` 的 changed 假阳性 → 迁移时改 `set_popup_anchor` 返回值。
- `MenuOptions.band/submenu_delay builder`、`MenuEntry::is_actionable`、`Menu::key/surface_node`、`ContextMenu::key`、`ContextMenuInner.window`、`PanelState.height` 死代码删除（AGENTS §4）。
- `Tooltip::overlay()`、`ColorPicker::overlay()`、`Dialog::overlay_node()` 公共 API 删除（仅 gallery/测试消费者）。
- `MenuMode::ListBox` 删除后 List/ListBoxOption a11y 覆盖转移到 Select 测试。

---

## 8. P1-9/P1-10/P1-11 拆波顺序建议

### 8.1 依赖图（→ 表示"必须先"）

```
W0 引擎补缝 ──→ W1 PopupPanel ──→ W2 ItemPanel/Menu/ContextMenu/Dropdown/Select
                                   └→ W3 Tooltip
                                   └→ W4 Dialog/Alert
                                   └→ W5 ColorPicker
                                   └→ W7 直接 Overlay 消费方
W2/W3/W4/W5/W7 ──→ W6 宿主 services/cx/binding/composition 清理
W6 ──→ W8 删除旧 API（P1-11）
W8 ──→ W9 测试重写 + gallery（P1-12）＋全量门禁
```

### 8.2 波次任务（每波可编译、可独立审查；同一 P1 阶段收口）

| 波 | 内容 | 关键文件 | 验收 |
|---|---|---|---|
| W0 | 引擎补缝：`begin_close_popup`、`popup_state`、`set_popup_a11y`、`RestoreFocus` 分支（+ 测试） | `crates/ui_core/src/app/popup.rs`、`app.rs`、`popup/focus.rs`、`popup/tests.rs` | `cargo test -p ui_core` |
| W1 | `PopupPanel`+`PopupSurface`+`PanelWidth/PanelMeasure`（无消费者，先单测） | `crates/ui_kit/src/kit/popup_panel.rs`、`kit/mod.rs` | `cargo test -p ui_kit popup_panel` |
| W2 | ItemPanel 去 Overlay（保留 pub(crate) 名与行为）；Menu/ContextMenu/Dropdown/Select 迁移；同批改所有构造点（gallery 4、settings 4、instrument 1、widgets 9、profile/market 内容构建） | menu.rs/dropdown.rs/select.rs/context_menu.rs + §1.6/1.7 构造点 | `cargo check --workspace --all-targets` + 组件测试 |
| W3 | Tooltip 迁移 + 6 宿主构造 + gallery | tooltip.rs + §1.6 | 同上 |
| W4 | Dialog/Alert 迁移 + 13 builder + 7 alert 源 + 6 widgets dialog + kytrix/settings/profile/market/updater/instrument + gallery | dialog.rs + §1.6/1.7 | 同上 |
| W5 | ColorPicker 迁移 + candlestick + gallery（含 pixel-check 临时适配） | color_picker.rs + candlestick | 同上 |
| W6 | 宿主清理：WidgetServices/Cx/binding/test_support、kytrix composition/dialogs、4 host 包装、业务 test_support、widgets 测试辅助 | §1.4/1.5/1.6/1.8 | `cargo test -p widget -p kytrix` |
| W7 | 直接 `Overlay::new` 2 处 → PopupPanel（market_participants_ui/menu.rs、profile_ui/profile.rs）+ 错误类型映射 | 2 文件 + 8 个 dialog.rs 错误映射 | `cargo check -p market_participants_ui -p profile_ui` |
| W8 | P1-11 删除：旧引擎 overlay/layer/API、kit overlay/layer、AnimationRouter、ItemPanel 旧依赖、宿主 layer/animation 字段、`overlay_wrapper`；清理引擎内部耦合（a11y/tree、window/a11y、node/tree overlay_containers、input 兜底、frame resolve、hud 测试） | §5.1/5.2 | `rg` 零命中 + `cargo check --workspace --all-targets` |
| W9 | P1-12 测试重写 + gallery dump/pixel-check | §6 | `cargo test`（workspace）、clippy、fmt、gallery `--pixel-check`/`--dump-overlay` |

### 8.3 每波门禁命令（Windows PowerShell）

```powershell
$env:CARGO_TARGET_DIR='E:\kytrix\target'
cargo check --workspace --all-targets
cargo test -p ui_core -p ui_kit -p widget
cargo +nightly clippy --workspace --all-targets -- -D warnings
cargo +nightly fmt --all -- --check
cargo run -p ui_kit --example gallery -- --pixel-check      # W9 后
cargo run -p ui_kit --example gallery -- --dump-overlay --auto-close=9
```

> 每波提交前跑 `rg` 旧模式检查"本波涉及的符号是否仍在"；W8 后全模式零命中（§5.3）。

### 8.4 与设计 §6 P1 任务的映射

| 设计任务 | 本文件波次 | 备注 |
|---|---|---|
| P1-9 PopupPanel + MenuChain 基础 + 全部组件 | W1-W5 | MenuChain 基础只保留 P1 parity（§2.4 P2 清单推迟） |
| P1-10 宿主迁移 | W6-W7 + W2-W5 内联构造点 | 构造点必须跟组件同波 |
| P1-11 删除旧 API | W8 | 依赖 W2-W7 |
| P1-12 测试重写 | W9 | 依赖 W8；gallery pixel-check 重写同波 |
| P1-0 popup root 原型 | 已完成（引擎 `popup/stack.rs:21-53`） | — |
| P1-1..P1-8 引擎 | 已完成/进行中（p1-engine-*） | W0 补缝属引擎批 |

---

## 附录 A：关键事实快查

- 新引擎 API 入口：`crates/ui_core/src/app.rs:1262-1314`；`app/popup.rs:162-699`；`popup.rs`（类型）；`popup/{anchor,placement,settle,stack,dismiss,focus,animation}.rs`；`a11y/popup.rs`。
- 帧管线：`app/frame.rs:59`（旧 resolve 待删）、`:62`（popup animation tick）、`:197-215`（settle + 报告合并）。
- popup root 预建：`window.rs:322-328`、`popup/stack.rs:21-53`（0×0 absolute + `HitFocusSkip::Container` + `pin_tail_child`）。
- `solve_placement`：`popup/placement.rs`（纯函数，含 `match_anchor_width` 只在求解中生效，line 36-46）。
- 关闭动画：`popup/animation.rs`（120ms 开/90ms 关常量 `popup.rs:583-587`）；`dismiss.rs:110-210`（begin_close）；`animation.rs:178-180` → `finalize_close`。
- 焦点：`popup/focus.rs:37-49`（初始焦点）、`:85-137`（恢复，快照路径；`RestoreFocus` 未分支）。
- a11y 投影：`a11y/popup.rs:120-260`（role/label/expanded/haspopup/owns/described_by/a11y_parent、Tooltip 追加 `HideTooltip`）。
- 旧 overlay 语义：`kit/overlay.rs:695-809`（open 内 measure/reveal）、`:706-721`（已开幂等）、`:848-891`（每帧 place）、`:916-945`（close）、`:1083-1129`（fade + 完成回调）。
- 旧 layer 语义：`kit/layer.rs:11-30`（Band）、`:90-118`（register/级联）、`:191-217`（enforce）。
- 旧引擎：`app/overlay.rs:14-185`、`interaction/overlay.rs`、`interaction.rs:327-418`、`app/frame.rs:59`、`app/input.rs:97,238-249`。
- K17 坏测试：`color_picker.rs:3349-3361`（`let _ = visual;` 在 3360；设计稿记 2964-2969/3315-3327 为旧行号，已漂移）。
- 设计稿行号漂移提醒：`overlay.rs` 实测 2871 行（设计记 2701/2692）、`dialog.rs` 2406（设计 2270）、`color_picker.rs` 4553（设计 4192）、`menu.rs` 2641；实施以**符号名**为准。
