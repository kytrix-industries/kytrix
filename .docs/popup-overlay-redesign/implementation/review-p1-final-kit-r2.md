# P1 终审第 2 轮（收口复核）：kit 组件 / 迁移 parity / 测试有效性

- 审查角色：P1 终审对抗性审查员（kit 组件/迁移 parity/测试有效性），只读
- 工作目录/worktree：`E:\kytrix\.agents\worktrees\dsh-1d175414-1a41`；基线 `d9949aca`；审查对象：修复轮后的 P1 工作树（114 个文件变更 + 7 个新路径，`git diff --stat d9949aca` ≈ 7926 insertions / 9250 deletions）
- 复核对象：第 1 轮报告 `review-p1-final-kit.md` 的 K1-01…K1-13，以及修复轮声明的 E-02/E-03/E-04/E-07/E-08 与 Q1-01/Q1-07/Q1-10
- 结论速览：**P0 = 0，P1 = 0，P2 = 0**；第 1 轮 P2 全部关闭（K1-02 为**用户裁决保留**并已登记）。**P3 = 7 项未全部清零**（3 项第 1 轮遗留 + 4 项第 2 轮复核新增），其中 5 项为登记/文档/一小时代码级，2 项为低风险残余。门禁实测全绿。**P0–P2 已可提交；若以字面“P3 全 0”为提交门槛则尚需完成 §5 的 4 项小修（约 0.5–1 天）或由 lead 明确接受 P3 backlog。**

---

## 1. 方法与门禁实测（本机 Windows 11，`CARGO_TARGET_DIR=E:\kytrix\target`）

| 门禁 | 结果 |
|---|---|
| `cargo test -p ui_core -p ui_kit` | **PASS：3777 passed / 0 failed / 2 ignored**（ui_core lib 2805+1 ignored、frame_record 8、gui_smoke 0+1 ignored、layout 4、popup_behavior 109、snapshot 14、demo 39、ui_kit lib 798；doc-tests 0）。第 1 次尝试因另一个 `gallery` 进程占用 `target\debug\examples\gallery.exe` 报 `LNK1104`（环境冲突，非代码），该进程退出后重跑 PASS |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | PASS（exit 0） |
| `cargo +nightly fmt --all -- --check` | PASS（exit 0） |
| `cargo check --workspace --all-targets` | PASS（exit 0） |
| `gallery --pixel-check` | **PASS**：checks=**94**、sampled_frames=32、open_animation_frames=8、close_animation_frames=**6+6=12**、damage_full_frames=0、first_visible==stable=(496,91,300×375)、moved=(496,4,300×375)，exit 0 |
| `gallery --debug-stack --auto-close=9` | **PASS**：menu 度量 `(6,180.0,127.5125)`、menu_frames=344、picker_frames=363、picker_children=Some(4)、最终 entries=0，exit 0 |
| `gallery --dump-overlay`（旧命令） | 打印 `gallery: --dump-overlay was renamed; use --debug-stack --auto-close=9` 并 **exit 2** |
| `gallery --bogus`（未知参数） | 打印 usage 并 **exit 2**（不再“忽略后 exit 0”） |
| 旧 API 7 组 `rg` 组合模式（inventory §5.3 + §12.9） | 全部 0 命中（含 `AnchorEdge`/`match_anchor_width`） |
| 静态 | kit 生产代码无 `#[allow]`/`#[expect]`/注释；第 1 轮的两处 `color_picker` 生产 `expect` 已删；无新增依赖 |

> 计数口径说明：ui_core lib 的 2806 条里有一条 `KYTRIX_UI_CORE_MANIFEST` 环境门控测试会以子进程方式再跑一次单测（输出 `1 passed; 2805 filtered`），不是第二个测试目标；门禁总计 = 2805+8+0+4+109+14+39+798 = **3777 passed**。

---

## 2. 逐条复核：第 1 轮 K1-01…K1-13

| Finding | 结论 | 证据（当前代码/测试） |
|---|---|---|
| **K1-01** 菜单/右键菜单祖先滚动/resize 关链矩阵未实现 | **fixed** | 引擎新增 `App::popup_anchor_rect`/`popup_anchor_visible`/`window_scroll_epoch`（`app.rs:1294-1306`，`popup/anchor.rs:24-73`；scroll epoch 只对 popup-root 之外的滚动递增：`app/scroll.rs:139-141,179-180`）；kit `AnchorWatch/AnchorChange`（`popup_panel.rs:96-151`）；Menu `sync_anchor` 对 Lost/Resized/Moved 关链、对 `Scrolled` 仅在 `scroll_closes` 且非内部滚动时关链（`menu.rs:1585-1609`），ContextMenu `closing_on_scroll`（`context_menu.rs:98,1216`）；Dropdown/Select 只对 Lost 关闭（`dropdown.rs:537-548`、`select.rs:800-811`），Tooltip 对 Lost 隐藏、其余重定位（`tooltip.rs:329-334`）。测试：`menu::an_ancestor_scroll_closes_the_whole_menu_chain`、`menu::a_window_resize_closes_the_menu_chain`、`context_menu::scrolling_the_page_closes_the_open_context_menu`、`dropdown::scrolling_the_trigger_out_of_the_page_closes_the_dropdown`、`dropdown::scrolling_a_visible_trigger_re_anchors_the_open_dropdown`、`select::...`（同名 2 条）、`tooltip::scrolling_the_anchor_repositions_the_tooltip_and_hides_it_when_it_leaves_the_view`；引擎 `popup_anchor_queries_track_scrolling_and_visibility`、`window_scroll_epoch_advances_only_when_an_offset_changes` |
| **K1-02** Menu/ContextMenu/Dropdown/Select 非模态 + outside Replay 未裁决 | **rejected（用户裁决保留，已登记）** | 设计 §12.3 D-P1-1/D-P1-2（`design.md:1088-1089`）：D6 Auto 非模态、D14 Replay 为用户裁决；§12.10 处置表 `K1-02 rejected`。实现与测试一致（`menu.rs:53-58`、三个 `..._replays_the_press_to_the_page`、`a_menu_is_non_modal_and_keeps_background_hover`） |
| **K1-03** Dropdown/Select 首帧测试空断言 | **fixed** | 测试改为 `press_release`（无 settle）→ 单帧 `drain` → 采样首帧（`dropdown.rs:1335-1365`、`select.rs:2288-2318`；`press_release` 见 `dropdown.rs:1017-1028`、`select.rs:1470-1481`），并额外断言首帧面板宽度已跟随 trigger（AtLeast 宽度首帧生效）。其余组件首帧断言仍为单帧 drain（PopupPanel/Menu/Dialog）；ColorPicker 仍是 open 帧 + sync + 第 2 帧采样（稍弱，见 §4） |
| **K1-04 = Q1-07** 旧 `--dump-overlay` 静默 no-op | **fixed** | `gallery.rs:139-183` 参数表删除旧名；旧名 → 提示 + **exit 2**；未知参数 → usage + **exit 2**；设计 §7.5/§12.3-8、kit-gallery §13.3/§13.5 已同步（`design.md:870,1095`；`kit-gallery.md:715-737`）。实测 exit 2 |
| **K1-05 = B-13** 默认焦点“第一个 Button” | **fixed** | `PopupSpec.initial_focus`（`popup.rs:263`）+ `App::set_popup_initial_focus`（`app.rs:1268-1273`）→ `initial_focus_node` 优先 requested（`popup/focus.rs:37-63`）；Dialog 打开后指定 confirm（`dialog.rs:443-445`）。测试：`dialog::a_content_button_before_the_footer_does_not_take_the_default_focus`（内容里先放可聚焦控件仍聚焦 confirm）、`popup_behavior::set_popup_initial_focus_moves_the_focus_within_a_trap`、`focus.rs::a_requested_initial_focus_node_wins_over_the_policy`。残余：无 `.confirm()` 的自建 footer 对话框仍走 D16 fallback（见 R2-04） |
| **K1-06** sampled-animation-floor 只保证累计下界 | **fixed** | open/close#1/close#2 三段独立断言（`gallery.rs:3652-3675`，open ≥4、close#1 ≥3、close#2 ≥3）；实测 8 / 6+6；checks 93→94 |
| **K1-07** 宿主 `popup_count()` 过滤 Scrim | **open（P3，登记 deferred）** | 7 个 helper 仍 `filter(|entry| entry.tier != PopupTier::Scrim)`：`updater_ui/test_support.rs:104-115`、`instrument_selector_ui/test_support.rs:146-157`、`market_participants_ui/test_support.rs:467-478`、`profile_ui/test_support.rs:789-800`、`widgets/financial_index/view.rs:1988-1996`、`widgets/financial_index/dialog.rs:1481-1489`、`widgets/coin_market_cap_fear_and_greed_index/view/tests.rs:48-56`；设计 §12.6-7 已登记。kit 层 `dialog::tests` 有 `stack().entries.is_empty()`（`dialog.rs:1633`）兜住 kit 自身 scrim，但宿主 release 路径仍无 total-stack 断言 |
| **K1-08** parity/覆盖缺口 a–g | **partial → 仅 e 仍开（P3）** | a（Alert 叠放）= 设计决策 D-P1-4；d（ColorPicker 非模态 Dialog role）= D-P1-6；g（200ms）= D-P1-3；c（typeahead/搜索）= P2-4 计划。**b fixed**：`tooltip::an_explicit_top_placement_puts_the_tooltip_above_the_trigger`（TOP 居中/底边=顶边-gap）；**f fixed**：`dialog::resizing_the_window_recenters_the_dialog`；**e open**：`instrument_selector.rs:61,456` 的 `.width(720)` 面板加宽仍无测试/偏差表登记（仅 §12.6-8 记为“未登记视觉变更”） |
| **K1-09** `PanelState.height` 死字段 | **fixed（代码）** | 字段被 `sync_height` 读取（`menu.rs:562` 读、`:566` 写、`set_content` `:379` 写），已不是死字段。**但设计 §12.6-14/§12.10 仍写“未修”**，见 R2-03 |
| **K1-10** 生产 `expect` | **fixed** | `color_picker.rs` 生产段两处 `expect` 已删（`sync` 改 `if let Some(..) = take()`、数组构造改 `from_fn` 类）；kit 生产段扫描仅剩 P1 未触及的 `brand.rs`/`input.rs`/`test_support.rs`（P1 前既有） |
| **K1-11** FadeSlide 未启用 | **open（P3，登记 deferred）** | kit 仍全 Fade/None（`menu.rs:418`、`tooltip.rs:84`、`color_picker.rs:1380`、Dialog Fade/None）；设计 D-P1-19/§12.6-9 登记 P3 |
| **K1-12** 宿主 settle 放宽 | **accepted（记录）** | `settle_dialog_close`/`settle_close` 的 `closed \|\| find().is_none()` 等价于“多推进 8×40ms 后节点消失”，未断言回调次数但属合理适配；§12.6-10 记录不阻塞 |
| **K1-13** `PopupPanel::release` 触发 `on_close` | **fixed** | `release` 先 `on_close.borrow_mut().take()` 再 `close_now`（`popup_panel.rs:529-533`）；测试 `popup_panel::release_does_not_report_a_close_callback`（`:1301`）断言 reasons 为空、浮层已卸载 |

---

## 3. 逐条复核：修复轮声明的 E-02/E-03/E-04/E-07/E-08

| Finding | 结论 | 证据 |
|---|---|---|
| **E-02** Dialog 遮罩 100% 不透明黑、Scrim 主题色死字段 | **fixed** | `ScrimSpec{color,density}`（`popup.rs:241-250`）；`scrim_paint = spec.color.with_opacity(density)`（`app/popup.rs:36-41`）；Dialog 用主题 `components().dialog.backdrop`（= `Role::Scrim`，Light α0.42/Dark α0.55，`roles.rs:232`、`palette.rs:101,152`）与 density 1.0（`dialog.rs:416-426`）。测试：`dialog::the_scrim_uses_the_theme_backdrop_color_instead_of_opaque_black`（alpha==主题 backdrop 且 <0.9）、引擎 `scrim_paints_the_configured_color_and_density` |
| **E-03** PopupSurface 阴影被 popup 内容根裁剪 | **fixed** | 引擎 `popup_content_style` 不再 `overflow_hidden`（`app/popup.rs:19-25`），裁剪下放到 kit panel/body（`popup_panel.rs:697-744`）。测试：`popup_panel::the_panel_shadow_is_not_clipped_by_the_popup_content_root`（content overflow==Visible、panel 阴影图元存在且 rect==panel_rect）、引擎 `popup/tests.rs::a_panel_shadow_is_painted_outside_the_popup_content_bounds`；`popup/tests.rs:1611` 亦断言 shadow 图元 |
| **E-04 = Q1-01** `constrained_size` 无 kit 消费者、内部滚动未落地 | **fixed** | `PopupPanel::available_body_height` 读 `constrained_size`/hit_rect（`popup_panel.rs:313-327`）；Menu `sync_height` 收敛 surface 高度（`menu.rs:557-568`，`set_content` `:374-379`）；Dialog 新 `DialogScroll`（`dialog.rs:172-179,622-681`）与 `sync_scroll`（`:787-846`、`sync_scroll_bar` `:849-861`）。测试：`menu::a_small_viewport_shrinks_the_menu_and_scrolls_its_last_row_into_view`（面板被夹住、End 后末行落在面板内）、`menu::a_long_menu_virtualizes_its_rows_and_scrolls_to_the_end`（80 行仅挂 <20）、`dialog::a_tall_dialog_scrolls_its_content_and_keeps_the_footer_reachable`（content>viewport 时内部滚动、footer 固定/可达） |
| **E-07** 同 K1-01 | **fixed** | 见 K1-01 |
| **E-08** a11y 无法激活 Menu/Dropdown/Select 选项 | **fixed（代码/测试；文档仍写 deferred）** | `List::set_row_action` 给行节点绑定 Click action（`list.rs:769-771,1142-1144`）；ItemPanel row handler 用 `ctx.hit().map_or_else(|| ctx.node(), ...)` 同时覆盖指针与 AT 目标（`menu.rs:707-716`）；`activate` 的 Click+hit 守卫避免表面重复激活（`:698-705`）。测试均断言“只激活一次”：`menu::an_a11y_click_activates_the_menu_entry_it_targets`（且激活 AT 指向行而非高亮行）、`menu::the_list_activation_path_activates_the_highlighted_entry`、`menu::a_pointer_press_activates_the_entry_it_lands_on`、`dropdown::an_a11y_click_chooses_the_option_it_targets`、`select::an_a11y_click_chooses_the_option_it_targets`（回调只 1 次）；行 a11y 记录含 `A11yAction::Click`。引擎指针派发只取最近 action 节点（`input.rs:516-541` `focus::action_target`），无双触发。**设计 §12.6-6/§12.10 仍写 deferred/未修 → 文档不一致（R2-03）** |

**第 1 轮 P0/P1/P2 清零结论：成立。** 修复轮对 12 条 P2（引擎 8 + kit 3 + quality 1）与 1 条 P0 的处理均有代码+测试证据；K1-02 的“拒绝”是用户裁决而非遗漏。

---

## 4. 新代码复扫（第 2 轮发现）

### R2-01（P3）约束态 popup 的内部滚动高度/滚动区在 `open` 当帧未 settle，首个绘制帧可能显示未夹取的内部内容

- **证据**：内部高度只在 `sync` 中写入：Menu `sync_height`（`menu.rs:557-568`）、Dialog `sync_scroll`（`dialog.rs:787-846`），而 `PopupPanel::available_body_height`（`popup_panel.rs:313-327`）依赖引擎 settle 后才有非空 `hit_rect`/`constrained_size`。`Menu::open`/`Dialog::open` 本身不做这一步；`open` 当帧（输入事件）→ 首帧前 host `sync`（frame hook）拿到的是空 hit_rect → 高度未收敛 → 首帧绘制时 panel 已被 engine 的 MaxSize 夹住，但 surface/scroll 区仍按未夹取内容布局，底部行/滚动区域被 `overflow_hidden` 裁掉；下一帧 `sync` 才把内部高度收敛。
- **测试缺口**：首帧断言都用未约束内容（`popup_panel/menu/dialog` 的 first-visible 测试；dropdown/select 用默认 max_height 在 600+ 高窗口），小视口/高内容用例（`a_small_viewport_shrinks...`、`a_tall_dialog...`）都在 `settle()` 之后断言，未断言首帧。
- **影响**：仅约束态（窗口比菜单 max_height 矮 / 对话框内容高于视口）出现一帧内部裁剪/滚动条范围不准；panel 位置与尺寸首帧仍稳定（`centered_origin` 的 margin 夹取与 placement 的 resize 求解保证），且默认 Fade 首帧透明度接近 0，肉眼风险低；`fade(false)` 的高内容对话框是最可能可见的组合。非回归（内部滚动为本轮新增能力，旧行为是整块溢出）。
- **修法**：在 `open` 完成后立刻做一次“可用高度/滚动内容”收敛（例如 `PopupPanel::open` 末尾提供 `settle_body()`，或 Menu/Dialog 在 `open` 返回后调用自己的 `sync_height/sync_scroll`，允许 `available_body_height` 首帧返回 `None` 时退化为“open 时已知的 viewport/margin 预算”），并补 2 条首帧用例（小视口菜单、高内容对话框）。**阻塞否：否（P3，需登记）。**

### R2-02（P3）anchor 矩阵关闭原因上报 `Programmatic`；`Menu/ContextMenu::on_close(DismissReason)` 无法区分 AnchorLost

- **证据**：`PopupPanel::close` 固定 `DismissReason::Programmatic` 并传给 `begin_close_popup`（`popup_panel.rs:427-442`）；kit 的矩阵关闭都走它（Menu `sync_anchor` → `close_chain` → `panel.close`，`menu.rs:1585-1602`；Dropdown/Select/Tooltip/ColorPicker 同）。而 `DismissReason::AnchorLost` 只由引擎 `dismiss::anchor_lost`（`popup/dismiss.rs:521-540`）在 `AnchorNodeMissing` 时发出，kit 在 sync 里先于 settle 关闭，因此“锚点被移除/滚出视口/窗口 resize”在 Menu 的公开 `on_close` 上都表现为 `Programmatic`。现有测试只断言 Escape/Programmatic 两种 reason（`popup_panel.rs:1273,1296`），无矩阵关闭 reason 用例。
- **影响**：P1 新增的 `Menu::on_close(reason)` 契约信息不准确；当前生产 Menu/ContextMenu 消费者为 0（gallery 仅日志），不影响用户可见行为；P2 MenuChain 若按 reason 分支需先修。
- **修法**：`PopupPanel` 增 `close_with_reason(DismissReason)`（或让 `AnchorWatch` 结果映射 `AnchorLost`/`FocusLost`/`Programmatic`），Menu/ContextMenu 用映射后的 reason；补 1 条“祖先滚动关闭 → on_close(AnchorLost)”测试。**阻塞否：否。**

### R2-03（P3）设计 §12 的回写与最终代码不一致：状态行与测试计数仍为修复前

- **状态行**：§12.10 `E-08` 仍写 **deferred/未修**（代码已 fixed，见 §3）；§12.6-6 同；§12.10/§12.6-14 `K1-09` 仍写“有写入、无读取”（实际 `menu.rs:562` 已读）；§12.10/§12.6-8 把 `K1-08` 的 **b（Tooltip 显式 TOP）与 f（Dialog resize recenter）** 仍列为未闭环（两测试已存在）。
- **计数**：§12.5/§12.10 写 `3771 passed`、`ui_kit lib 792`，实测为 **3777 passed / ui_kit lib 798**（ui_core lib 2805、frame_record 8、gui_smoke 0+1 ignored、layout 4、popup_behavior 109、snapshot 14、demo 39）；§12.5 的 kit `#[test]` 计数 `Dropdown 25 / Select 31 / Menu 25` 实测为 **27 / 32 / 28**（其他：PopupPanel 18、ContextMenu 14、Tooltip 28、Dialog 53、Alert 11、ColorPicker 80 均一致；+6 正好解释 792→798）。与 `--pixel-check` 的 94 checks / 采样下限 / §12.9 rg 表无关，那些已核对一致。
- **影响**：P1 收口文档（设计 §12、可能还有提交说明）会出现“已完成项仍列未修”与门禁数字偏小，影响“P3 全 0/可提交”的审计可信度；代码无影响。
- **修法**：按当前实测更新 §12.5/§12.6/§12.10（E-08/K1-09/K1-08b/f 状态，3777/798，27/32/28）。**阻塞否：否（建议提交前改，10 分钟）。**

### R2-04（P3）K1-05 残余：无 `.confirm()` 的自建 footer 对话框仍走 D16 fallback，初始焦点落在 Cancel

- **证据**：Dialog 只在 `state.confirm_node` 存在时设置 `initial_focus`（`dialog.rs:443-445`）；`confirm_node` 仅由 `.confirm()` 构建（`build_button_row`）。`settings_ui/src/pages/proxy/editor.rs:502-507` 的对话框用自建 footer（`:897` Cancel、`:925` Confirm 两个 Button 在 content 内，先 Cancel 后 Confirm），没有 `.confirm()` → `initial_focus=None` → 引擎 `default_button` 取子树第一个 Button = **Cancel**。D16 原文是“默认按钮优先，无默认按钮回退第一可聚焦”，因此该行为**符合 D16 fallback**，但不是编辑器主操作的最佳焦点。
- **影响**：键盘用户打开 proxy 编辑器对话框时初始焦点在 Cancel；非阻塞（D16 合规），但作为 K1-05 的残余应登记或提供能力。
- **修法**：给 `DialogBuilder` 增 `initial_focus(NodeId)`（或 content 侧声明 default 的 API），proxy editor 用它指向 Confirm；否则在 §12.3 登记“自定义 footer 对话框使用 D16 fallback”。**阻塞否：否。**

### R2-05（观察，不计 finding）Scrim 颜色/滚动条等低风险残余

- `ScrimSpec.color` 在 `open` 时解析一次（`dialog.rs:416-426`），模态打开期间切换 Appearance 不会刷新遮罩颜色（旧 kit 也是 open 时解析；当前无 theme-check 覆盖“模态打开中切换主题”的场景）——P3 观察，未列入 finding。
- Dialog 内部 `ScrollBar` 作为 overlay 覆盖在内容右缘（`size_full` viewport，不预留宽度），hover 滚动条可能遮住内容右缘若干像素；无真机证据，作为 P3-6 真机验收观察项。
- ColorPicker 首帧测试在第 2 帧采样（`color_picker.rs` first-visible 测试：open 帧 drain → sync → 再 drain 才读 rect），比 Dropdown/Select 的新写法弱一档，但非空转；可顺手改成单帧。

---

## 5. 复扫重点逐项结论（对照 lead 指定项）

1. **Internal scroll 的 List 范围**：Menu `ItemPanel` 的 List 视口随 `sync_height` 收敛，80 行/`max_height:200` 只挂 <20 行（`a_long_menu_virtualizes...`），小视口 End 后末行落在面板内（`a_small_viewport_shrinks...`）；Dialog 用独立 scroll viewport（非 List），高内容时 `scroll_state.content > viewport` 且 footer 固定（`a_tall_dialog...`）。范围/虚拟化正确；**首帧约束态未覆盖（R2-01）**。
2. **anchor 矩阵误关闭/漏关闭**：误关闭方面，内部列表滚动不会 bump `window_scroll_epoch`（`app/scroll.rs:139-141,179-180` 跳过 popup-root 子树），Menu 长列表滚动保持打开（`a_long_menu_virtualizes...` 使用 End 后仍打开），Dropdown/Select 忽略 Scrolled/Moved 由引擎 re-settle（`scrolling_a_visible_trigger_re_anchors...`），Tooltip 只查 Lost 并重定位；漏关闭方面，`anchor_visible` 会计入 `Clip::Bounds` 祖先与视口相交（`popup/anchor.rs:29-72`），节点移除/隐藏/完全滚出视口 → Lost → 关（Dropdown/Select/Menu/ColorPicker）或隐藏（Tooltip），`AnchorNodeMissing` 引擎路径仍保留给非 kit popup。**未发现误关/漏关的可复现缺陷**；仅 R2-02 的 reason 语义与“WindowRect 锚的普通 Menu 无 scroll_closes（kit 无此用法）”属残余风险。
3. **Dialog 内部滚动条**：`ScrollBar` 以 hover 模式挂在 scroll viewport 上（`dialog.rs:662-670`），`sync_scroll_bar` 每帧 sync，`release_content` 释放 bar（`:949-952`）；无重复 attach；高度收敛公式 `available - fixed - gaps` 与 panel padding 对齐（`dialog.rs:813-841`）。功能正确；滚动条遮内容右缘与首帧收敛见 R2-01/R2-05。
4. **Scrim 主题色**：见 E-02；Light/Dark 的 `Role::Scrim` 分别为 black α0.42/0.55，`ScrimSpec.color × density` 在引擎绘制；kit 测试断言 alpha==主题 backdrop 且 <0.9。主题切换期间打开模态不刷新（R2-05 观察）。
5. **首帧采样**：K1-03 的 Dropdown/Select 现已真实（press+release → 单帧 drain → 首帧 rect/宽度 → settle 后比较）；Menu/Dialog/PopupPanel 仍为单帧 drain；`--debug-stack` 的 first_visible==stable 仍由 `on_frame` 在首个 Open 非空帧采样（`gallery.rs` capture_debug_stack），实测 PASS。ColorPicker 两帧采样（R2-05）。
6. **gallery 退出码 / 94 checks / 分段下限**：`--pixel-check` exit 0/3 保留；checks=94 = 32 逐帧采样 + 16 background-residue + 46 静态（20 idle + 3 restored + 2 close-frames + 18 具名 + 3 floors）；分段下限 open≥4 / close#1≥3 / close#2≥3（实测 8 / 6 / 6）；覆盖下限 30‰ 与 2 点注入自检仍在（`gallery.rs:3165-3190,3273+`）；旧名/未知参数 exit 2。**门禁有效性满足 P1-12 要求**；唯一残余是 R2-01 的“约束态首帧”不在像素门禁场景内（场景固定 1200×800 未约束）。
7. **a11y 行 action 双触发**：`List::set_row_action` 把 Click action 挂到行节点，hit 壳仍持有同一 action（`menu.rs:232`, `list.rs:1142`）；指针路径经 `focus::action_target` 只派发最近一个 action（`app/input.rs:516-541`），a11y 路径 target 即行节点，`row_at_node` 用 `ctx.node()` 兜底；menu/dropdown/select 三条 a11y 测试均断言“只激活一次/回调一次”，指针点击测试断言激活列表长度 1。**无双触发。**

---

## 6. 未关闭清单与建议

### 6.1 第 1 轮遗留（P3，仍未关闭）

| 项 | 状态 | 最小修法 |
|---|---|---|
| K1-07 宿主 `popup_count/overlay_count` 过滤 Scrim（7 个 helper） | deferred（§12.6-7） | 每个宿主在关闭/释放用例加一条不过滤的 `popup_stack(...).entries.is_empty()`（或断言无 Scrim），约 30 分钟 |
| K1-08e `instrument_selector` Dialog `.width(720)` 视觉变更未登记/无测试 | open（§12.6-8） | 在 `instrument_selector` 测试断言 dialog/panel 宽度（≈736）或 §12.3 登记“修复旧 720 内容溢出 384 根造成的必然加宽”，约 15 分钟 |
| K1-11 FadeSlide 未启用 | deferred（D-P1-19） | 接受登记，或给 Menu/Dropdown/Select/Tooltip/ColorPicker 换 `FadeSlide`（否则 UI 视觉变化，建议 P3 处理） |

### 6.2 第 2 轮新增（P3）

| 项 | 建议 |
|---|---|
| R2-01 约束态内部滚动首帧未 settle | 登记 P3，或按 §4 修法补 `open` 末尾收敛 + 2 条首帧用例（1–2 小时） |
| R2-02 矩阵关闭 reason=Programmatic | 登记 P3，或加 `close_with_reason` 映射 AnchorLost + 1 条测试（约 30 分钟） |
| R2-03 §12 状态/计数不一致 | **提交前更新**（10 分钟）：E-08 fixed、K1-09 fixed、K1-08b/f fixed；3771→3777；ui_kit 792→798；Dropdown 25→27、Select 31→32、Menu 25→28 |
| R2-04 自定义 footer 对话框默认焦点回退 Cancel | 登记（D16 fallback 合规）或补 `DialogBuilder::initial_focus` |

---

## 7. 结论

- **严重度计数：P0 = 0，P1 = 0，P2 = 0，P3 = 7**（K1-07、K1-08e、K1-11、R2-01、R2-02、R2-03、R2-04）。
- 第 1 轮的全部 P0/P1/P2（含 E-01…E-08、K1-01…K1-06、K1-09/10/13、Q1-01/03/07/10）均已复核关闭；K1-02 按用户裁决保留并已登记；K1-12 作为已记录放宽接受。
- 门禁：`cargo test -p ui_core -p ui_kit` **3777 passed / 2 ignored**、clippy `-D warnings`、fmt、workspace check 全绿；`--pixel-check` **94 checks** PASS；`--debug-stack` PASS；旧命令/未知参数 exit 2。
- **可提交性**：**P0–P2 全 0，且无架构/门禁阻塞，可以提交 P1**。但严格按 lead 的“P0–P3 全 0”目标，当前仍差 7 条 P3（均低风险、可登记）：若接受 §12.6 式 P3 backlog，**明确可提交 P1**；若要求字面全零，建议先做 §6.2 R2-03 文档同步 + K1-07/K1-08e 两个小修（合计约 1 小时）并把 R2-01/R2-02/R2-04/K1-11 登记，即可在“零开放 finding（P3 全部有处置记录）”口径下提交。
