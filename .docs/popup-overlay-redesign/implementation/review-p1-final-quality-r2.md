# P1 终审对抗性审查·第 2 轮（收口复核）

- 审查对象：worktree `E:\kytrix\.agents\worktrees\dsh-1d175414-1a41`，基线 `d9949aca`，P1 未提交改动（修复轮结束后）。
- 审查时间：2026-11-27 10:27–10:47（CST）。**实现方在复核期间做了最后修改**（最后源码 mtime：`gallery.rs` 10:43:46、`menu.rs` 10:40:28）；本报告门禁数据为**最后修改之后的重跑**（10:44–10:46，见 §1），tree 自 10:43:46 起稳定。
- 文档快照：设计 `§12` 与 `p1-summary.md` 停在 10:20（**尚未包含 E-08 完成后的最终数字/状态**）；`p1-cleanup-w7.md` 停在 08:47。docs agent 的最终同步尚未发生 → §4 的 Q2-01/Q2-02。
- 只读约束：未修改任何源码；报告位于 gitignore 的 `.docs`。
- 严重度口径沿用：**P0=正确性/硬要求/回归/门禁虚假通过；P1=重大健壮性或测试可靠性；P2=次要问题；P3=风格/文档/测试缺口。**

## 1. 门禁复跑（本机，Windows 11 / D3D12 / 125% DPI，修复轮最终代码 10:43:46 之后）

> 首次重跑（10:43）时另一 agent 的 `gallery.exe` 仍占用 target 产物，`cargo test` 的 gallery example link 报 LNK1104（测试 target 本身不受影响）；等待该进程结束后 10:44–10:46 完整重跑通过（下表）。最终 diffstat：**114 files changed, +7928 / −9251**（`git diff --shortstat`，10:45 快照）；新增文件 7 条 untracked 路径 / 14 文件 / 15253 行（与文档一致）。

| # | 命令 | 结果 |
|---|---|---|
| 1 | `cargo test -p ui_core -p ui_kit` | **PASS，3777 passed / 0 failed / 2 ignored**：ui_core lib 2805（+1 ignored）、frame_record 8、gui_smoke 0（+1 ignored）、layout 4、**popup_behavior 109**、snapshot 14、demo 39、**ui_kit lib 798**。P0 基线 3554 → +223 |
| 2 | `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | **PASS**（无 warning） |
| 3 | `cargo +nightly fmt --all -- --check` | **PASS**（exit 0） |
| 4 | `cargo check --workspace --all-targets` | **PASS**（widgets/business/kytrix 全 target；10:44 复跑；10:43 一次尝试的 gallery link 曾因并发 `gallery.exe` 报 LNK1104，见本节首段） |
| 5 | `gallery --pixel-check` | **PASS，exit 0**：`checks=94 sampled_frames=32 open_animation_frames=8 close_animation_frames=6+6=12 damage_full_frames=0`，first_visible==stable=(496,91,300×375)，moved=(496,4,300×375) |
| 6 | `gallery --debug-stack --auto-close=9` | **PASS，exit 0**：automation 完成后自行关窗（`overlay-check complete -> closing window`），auto-close 仅抬升 ≥30s 看门狗；menu 度量 (6,180.0,127.5125)、picker children=4、最终 entries=0 |
| 7 | `gallery --debug-stack`（无 auto-close） | **PASS，exit 0**（同上；看门狗 30s，实际 ~5s 完成） |
| 8 | `gallery.exe --dump-overlay` / `--bogus` / `--help` | **exit 2 / 2 / 0**（旧命令显式失败并提示改名；unknown 参数 exit 2；help exit 0） |
| 9 | `rg` 零引用复跑 | 见 §5（7 组旧 API 全 0；`dump-overlay` 仅 3 处 gallery 改名提示；`AnchorEdge`/`match_anchor_width` 0） |

> 门禁脆弱性（lead 提示的 `--debug-stack --auto-close=9` 时序假红）**已修复**：`main`/`install` 中 `--auto-close` 对 `--debug-stack` 不生效（改为 `max(auto_close, 30)s` 看门狗），automation 末轮 `completed=true` 后主动 `close_window`，退出码由 report 决定；usage/banner 已写明「auto-close 只是 ≥30s 看门狗」。本次带 9 与不带 auto-close 均 PASS。

## 2. Q1-01…Q1-10 逐条复核（第 1 轮报告）

| ID | 原严重度 | 结论 | 证据 |
|---|---|---|---|
| Q1-01 `PopupPanel` 确定性高度内部滚动未落地、§12.2 口径不实 | P2 | **fixed** | `popup_panel.rs:313-327` `available_body_height` 读 `constrained_size`（无则 `hit_rect`）；`menu.rs:374-379,541-567` 用其收敛 List 高度（`sync_height` 用 `PanelState.height` 缓存，仅变化时写 style）；`dialog.rs:788-855` 用其收敛滚动视口并固定 footer。测试：`menu::a_small_viewport_shrinks_the_menu_and_scrolls_its_last_row_into_view`（menu.rs:2604，面板被夹住 + End 末行进入可见区）、`dialog::a_tall_dialog_scrolls_its_content_and_keeps_the_footer_reachable`（dialog.rs:1995，面板收缩 + 内容滚动 + footer 不动）。§12.2 P1-4 已改写为真（E-04 备注） |
| Q1-02 `Anchor::Node.edge`/`AnchorEdge` 死字段 | P3 | **fixed** | `Anchor::Node { node }`；`rg AnchorEdge|match_anchor_width` = 0（§5）；设计 §4.2 口径 1/§12.4/§12.8 已同步 |
| Q1-03 生产 `expect` 兜底 | P3 | **fixed** | 逐个新文件（`popup.rs`/`popup/*`/`app/popup.rs`/`a11y/popup.rs`/`popup_panel.rs`）按 `#[cfg(test)]` 边界扫描：生产段 `expect/unwrap/panic/todo` 命中 0（仅 `popup/tests.rs` 测试模块有 expect）；`app/popup.rs`/`stack.rs`/`dismiss.rs`/`settle.rs` 无 expect |
| Q1-04 widget 层 `DialogError::Popup(_)` 折叠为 `UnknownWindow` | P3 | **deferred（已登记）** | 仍在 6 处（`financial_index/x/hyperliquid_wallet/solana_wallet/ethereum_wallet` dialog、`updater_ui/dialog`、`profile_ui/profile.rs`）；§12.6-11/§12.10 登记为 P3 遗留，不阻塞 |
| Q1-05 §12.1/§12.5/p1-summary 数字 | P3 | **partially（已被 E-08 再次拉开）** | 上一轮已修为 112 文件/+7660/−9238、3771/792；**最终树为 114 文件/+7926/−9250、3777/798**，且 §12.5 测试规模 Dropdown 25→27、Select 31→32、Menu 25→28 → Q2-01 |
| Q1-06 §12.9 矛盾、§8 `popup_host` 残留、kit-gallery 未同步 | P3 | **fixed** | §12.9-2 改为「0（判据 `\bLayer::new`；原始命中 CAMetalLayer 保留词）」；§8 两行改为「不新增宿主浮层 API，原 PopupHost/cx.popup_host 未实现」并加入最终替代 API 对照；`kit-gallery.md` §13.3/§13.5 已同步（`--debug-stack`、94 checks、P0 108 差异、旧命令 exit 2） |
| Q1-07 `--dump-overlay` 静默假绿 | P3 | **fixed** | `gallery.rs:139-181`：旧名 → 打印改名提示 + `exit(2)`；unknown → `exit(2)`；`--help` → 0；实测 exit 2/2/0 |
| Q1-08 93→108 覆盖差异未说明、floor 口径 | P3 | **fixed** | §12.5/`p1-summary` §5 写明 94 = 32 采样 + 16 background + 46 静态（含 3 条 floor：open≥4、close#1≥3、close#2≥3）；`gallery.rs:3653-3679` 三段 floor；实测 checks=94 |
| Q1-09 §4.7 菜单/右键菜单祖先滚动/resize 关链矩阵未实现、测试固化相反行为 | P2 | **fixed** | 引擎新增 `App::popup_anchor_rect/popup_anchor_visible/window_scroll_epoch` + `Window::scroll_epoch`（`app.rs:1294-1307`、`window.rs:762-766`）；kit `AnchorWatch`（`popup_panel.rs:104-152`）按 Lost/Resized/Moved/Scrolled 分派，`menu.rs:1583-1601` Lost/Resized/Moved→`close_chain`、Scrolled→close（内部滚动除外）；context menu 经 Menu 复用；Dropdown/Select 重锚定、Tooltip 重定位/隐藏。测试：`menu::an_ancestor_scroll_closes_the_whole_menu_chain`（menu.rs:2666）、`context_menu::scrolling_the_page_closes_the_open_context_menu`（context_menu.rs:764）、引擎 `popup_internal_scroll_does_not_advance_the_window_scroll_epoch`（popup_behavior.rs:5020） |
| Q1-10 Dropdown/Select 首可见帧空断言 | P2 | **fixed** | 两条测试改为：先 drain 1 帧 → `press_release` → 再 drain 1 帧即采样 `popup_entry().hit_rect`/面板宽，再 `settle` 比较；并断言首帧面板宽==触发宽、稳定帧宽一致（dropdown.rs:1335-1365、select.rs:2288-2318） |

## 3. E/K1 修复轮抽检（本轮新增，全部实测/读码复核）

- **E-01（P0）关闭事务期 modal 输入阻断**：`stack.rs::topmost_modal` 用 `state.is_visible()`（含 DismissPending/Closing）；`dismiss.rs::on_pointer_down` 在 modal 存在且命中 scope 外时 Consume。测试 `a_closing_modal_swallows_background_presses_until_it_is_removed`（popup_behavior.rs:4089，含「淡出期背景 action=0、移除后恢复」）、`a_closing_modal_keeps_background_hover_and_scroll_blocked`、`closing_the_lower_modal_cascades_and_keeps_input_blocked_until_all_leave`、嵌套 Esc 序列；D-P1-28 已登记。**无 P0 残留。**
- **E-02（P1）Scrim 主题色**：`ScrimSpec{density,color}`，`scrim_paint = color.with_opacity(density)`（乘法 alpha，非替换）；Dialog 用 `theme.components().dialog.backdrop`（Light/Dark 0.42/0.55），density=1.0 → 最终 alpha = backdrop alpha（<0.9）；测试 `the_scrim_uses_the_theme_backdrop_color_instead_of_opaque_black`（dialog.rs:1916）与 `scrim_paints_the_configured_color_and_density`（popup_behavior.rs:4635）。默认 density=0/black → 未配置 scrim 不可见。**通过。**
- **E-03（P1）阴影裁剪**：`app/popup.rs::popup_content_style` 移除 `overflow_hidden`，panel/body 各自裁剪；测试 `the_panel_shadow_is_not_clipped_by_the_popup_content_root`（popup_panel.rs:1002）。**通过。**
- **E-04（P1）kit 内部滚动**：同 Q1-01。**通过。**
- **E-05/E-06/E-08**：`PopupStack::remove` 清 `focus.owner` + `validate_owner` 回落（测试 popup_behavior.rs:4349/4386）；`cancel_pointer_session`/`on_window_focus_lost` 清 `suppress_release`（测试 4424）；E-08（原 deferred，本轮已修）`ItemPanel` 接 `List::on_activate`，Menu/Dropdown/Select 行 a11y 宣告 `A11yAction::Click`，AT Click 激活所指向行：`menu::an_a11y_click_activates_the_menu_entry_it_targets`（menu.rs:2334）、`dropdown::an_a11y_click_chooses_the_option_it_targets`（dropdown.rs:1249）、`select::an_a11y_click_chooses_the_option_it_targets`（select.rs:1769）。**代码已修复，文档仍写 deferred（Q2-01）。**
- **E-09/E-10/E-11/E-12/E-14**：`AnchorEdge`/`match_anchor_width` 删除；tooltip 仅宣告 `HideTooltip`（不再 ShowTooltip）；`text_caret_rect` 累计 `popup_transform.offset`（`window/focus.rs:38-46`）；`[dbg] eprintln` 全仓 0；生产 expect 0。**通过。**
- **K1 系列**：K1-01=E-07/Q1-09 fixed；K1-03=Q1-10 fixed；K1-04=Q1-07 fixed；K1-05 默认按钮用新增 `PopupSpec.initial_focus` + `App::set_popup_initial_focus`（Trap 内校验），测试 `a_content_button_before_the_footer_does_not_take_the_default_focus`（dialog.rs:1936）、`set_popup_initial_focus_moves_the_focus_within_a_trap`/`rejects_non_trap_popups`（popup_behavior.rs:4704/4766）；K1-06 floor 改三段（gallery.rs:3653-3679）；K1-10 = Q1-03 fixed；K1-13 `release_does_not_report_a_close_callback`（popup_panel.rs:1301）。K1-02 按 D-P1-2（D14 用户裁决 Replay）关闭；K1-07/K1-08b/e/f/K1-11/K1-12 登记 P3。

## 4. 修复轮引入问题复扫（Q2）

### Q2-01（P3）§12 / p1-summary 仍是 E-08 之前的快照：最终数字、E-08 状态、K1-09 结论均需更新

- 证据（当前代码/文档实测）：
  - 规模：设计 §12.1（当前行 1063）与 `p1-summary` §1（行 14）写 **112 文件 / +7660 / −9238**；最终树为 **114 文件 / +7928 / −9251**。
  - 门禁：§12.5（1181）与 `p1-summary` §5（91）写 **3771 passed / ui_kit 792**；实际 **3777 / 798**（差 6 条 = E-08 的 Menu+3、Dropdown+2、Select+1 组件测试）；同一段测试规模（§12.5 1189 / p1-summary 98）Dropdown 25→**27**、Select 31→**32**、Menu 25→**28**；§12.10 开头（1339）与 p1-summary §10（170）的「3771」同。
  - E-08：§12.6-6（1222）、§12.10 E-08 行（1352）、结论（1390）、p1-summary §8-6（151）与 §10 表仍写 **deferred / 未实现**；代码已 fixed（§3）并新增 3 条 AT Click 测试。
  - K1-09：§12.6-14（1230）、§12.10 K1-09 行（1374）写「`PanelState.height` 有写入、无读取」——**该结论已失效**：E-04 内部滚动后该字段在 `menu.rs:562` 被读取（`sync_height` 用于高度缓存比较），不是死字段；应删除该条或改为「已作为高度缓存消费」。
  - 文档头部/§10 DoD 的 3777/798 尚缺；`p1-summary` §14 的「P3 终审残余」仍列 K1-09。
- 影响：§12/`p1-summary` 是提交说明来源，数字/状态落后会让审计误判（尤其 E-08 已修却记为 deferred、K1-09 已不成立却留 P3）。
- 修法：docs agent 在提交前重算并按最终代码更新 §12.1 规模、§12.5 门禁/测试规模、§12.10 E-08/K1-09 行与结论、§12.6-6/-14、`p1-summary` §1/§5/§8/§10。**不涉及代码，不阻塞门禁。**
- 是否阻塞：文档门禁上应修；代码/门禁无影响。

### Q2-02（P3）`p1-cleanup-w7.md` 的历史门禁快照仍是 `checks=93`（W7 时点），需一行注记

- 证据：`p1-cleanup-w7.md` 行 84（「保留全部既有断言（93 checks）」）、行 117（门禁表 `checks=93`）、行 122（floor 2 条口径）为 08:47 的 W7 记录；终审后 `--pixel-check` 为 **94 checks**（floor 3 段，§12.5/kit-gallery §13.5 已是 94）。W7 是历史任务记录，但 §12.2 P1-12 把它列为证据，读者可能误判为回归/漏测。
- 修法：在 W7 §4/§6 加一行「终审后 checks=93→94（3 条 floor、idle 背景采样口径变化），最终基线见设计 §12.5 / kit-gallery §13.5」，或明确标注「本文件为 W7 时点快照，后续变更不在此维护」。
- 是否阻塞：否（P3 文档注记）。

### 残余观察（不计 finding，建议登记/由 lead 决定）

1. **外部滚动语义宽于「祖先滚动」字面**：`window_scroll_epoch` 在**任意非 popup 子树**滚动时 +1（`app/scroll.rs:139-141,179-181`），Menu/ContextMenu 的 `AnchorWatch` 因此对「无关滚动容器」也会关链；设计 §4.7 文字是「祖先滚动或窗口 resize」，而 §12.10 E-07 证据写「及外部 Scrolled」。若产品语义是「任何外部滚动都关菜单」（桌面惯例），建议把 §4.7/D-P1-29 措辞改为「外部滚动（含祖先）」并补一个无关滚动容器用例；否则应收紧到祖先链。属 P3 语义/文档精度，不影响当前验收。
2. **受约束菜单/Dialog 的「首可见帧」未锁**：`available_body_height` 的第一次生效依赖 host sync（首帧后一帧），`a_small_viewport...`/`a_tall_dialog...` 均在 settle 后断言；理论上小视口下首帧可能出现一帧「list 末行/footer 被 panel clip」再于下一帧修正（未实测到用户可见回归）。建议补「受约束首帧 == 稳定帧」用例（P3 测试覆盖，归 P3-5/E-04 后续）。
3. **每帧查询成本**：打开状态下每帧 `AnchorWatch.observe`（anchor rect + visible + scroll epoch，O(depth)）与 menu/dialog `available_body_height`（`App::popup_stack` 快照 `Vec` 分配，O(entries)）——数量很小，但属 P3-5 性能基线的输入；无实测预算。
4. **K1-09 结论反转**：见 Q2-01，文档需删/改（代码无问题）。

## 5. `rg` 零引用复跑（`crates/**/*.rs`，1092 文件）

| # | 模式 | 命中 | 判定 |
|---|---|---:|---|
| 1 | `kit::overlay\|overlay::\{\|OverlayOptions\|OverlaySurface\|OverlayA11y\|OverlayError\|AnimationRouter` | 0 | PASS |
| 2 | `kit::layer\|layer::\{\|\bLayer::new\|LayerSlot\|Band::` | 0 | PASS（原始 `Layer::new` 仅 `CAMetalLayer::new()`，保留词） |
| 3 | `create_overlay\|close_overlay\|place_overlay\|bring_overlay_to_front\|resolve_overlay_placements\|dismiss_overlay\|\.overlays\(` | 0 | PASS |
| 4 | `OverlaySlot\|OverlayDismiss\|OverlayPlacement\|overlay_style\|note_overlay_container\|overlay_containers\|overlay_slot` | 0 | PASS |
| 5 | `\.layer\(\)\|\.animation\(\)\|animation_router\|services\.layer` | 0 | PASS |
| 6 | `overlay_wrapper\|MenuMode::ListBox` | 0 | PASS |
| 7 | `dump-overlay\|dump_overlay` | **3**（`gallery.rs:175/176` 旧名失败提示、`:3741` banner「covers the removed --dump-overlay」） | PASS（仅提示字符串，无旧命令/API 使用；§12.9 已登记） |
| 8 | `AnchorEdge\|match_anchor_width`（本轮新增删除项） | 0 | PASS |
| 9 | `\bOverlay\b`（大小写敏感，排除 `DamageOverlay`/`A11yPopupOverlay` 子串） | 0 | PASS |

## 6. AGENTS 对照（修复轮新增代码）

| 条目 | 结果 | 说明 |
|---|---|---|
| §1.1 无兼容层/fallback | 通过 | 旧命令显式 exit 2（不是兼容 shim）；`PopHost` 未实现且文档改为最终形态 |
| §1.3 模块布局/无 `#[path]` | 通过 | 新增 `AnchorWatch` 在 `kit/popup_panel.rs`（popup 支撑组件内聚）；`#[path]` 0 |
| §1.6 无注释 | 通过 | 累计 diff 新增 7890 行中注释 0；新文件生产段注释 0 |
| §2.1 无未使用字段/Serde | 通过 | `AnchorEdge`/`match_anchor_width` 删除；`initial_focus`/`ScrimSpec.color` 均被消费；`PanelState.height` 已有读取（K1-09 反转）；`serde_json`/动态 JSON 0 |
| §2.2 SmolStr / §2.3 集合 | 通过 | 新增代码使用 `SmolStr`/`format_smolstr!`；`SmallVec`/`with_capacity`；`ahash` 仅在既有 a11y 侧表 |
| §3 强类型错误 | 通过（P3 例外已登记） | 新 API `set_popup_initial_focus`→`Result<bool, PopupError>`；`popup_anchor_*`→`Option`；Q1-04 的 widget 折叠仍登记 P3 |
| §4 无 `allow/expect` 属性、无生产 panic | 通过 | 新增 `#[allow]`/`#[expect]` 0；生产 `unwrap/expect/panic/todo` 0（按 `#[cfg(test)]` 边界复核） |
| §5 依赖 | 通过 | 无新依赖；`Cargo.toml` 仅 `[[test]] required-features` |
| §8 验收 | 部分（用户限制） | 本轮仅 `ui_core`/`ui_kit` 测试；workspace 测试仍未跑（§8 残余风险） |

## 7. 性能/行为观察（无实测基线）

- **E-07 新增查询**：每帧每打开 popup 一次 `AnchorWatch.observe`（`popup_anchor_rect` = `node_screen_rect` O(depth) + `anchor_visible` + `window_scroll_epoch` 读取），menu/dialog 另加 `available_body_height` → `App::popup_stack` 快照分配（O(entries)+1 Vec）。计数小、无热点证据，但应计入 P3-5 基线。
- **打开/关闭帧**：仍无 open/close 帧预算实测（P3-5）；首帧 settle ≤2 轮的既有约束未变；非模态 popup 开关 `damage_full==false` 仍由 `popup_behavior::opening_and_closing_a_non_modal_popup_never_forces_a_full_repaint` 锁定；模态/Scrim 走整帧重发（设计允许）。
- **内部滚动高度**：menu `sync_height`/dialog `sync_scroll` 仅在目标高度变化时写 style（`0.01` 阈值），`set_scroll_content` 每帧调用但为纯写入、不 request frame，无空闲帧循环迹象；`--pixel-check` idle 断言仍 PASS。
- **结论**：无新增性能 P0/P1/P2；P3-5 待补基线（已登记）。

## 8. 残余风险与已登记 P3 遗留

**P3 已登记（不阻塞提交，逐条在设计 §12.6/§12.10 与 p1-summary §8 留档）**：
1. K1-07：宿主 `test_support::popup_count()` 过滤 `Tier::Scrim`，orphan scrim 回归可能被掩盖（引擎有 `reap_orphan_scrims` 单测）。
2. K1-08b/e/f：Tooltip 显式 TOP 定位无测试；`instrument_selector` Dialog 显式 width(720) 未登记视觉变更；Dialog resize recenter 无回归测试。
3. K1-11：`FadeSlide` 仍未启用（D-P1-19）。
4. K1-12：宿主测试装置 settle/fade 推进、`closed || find().is_none()` 等放宽已记录不阻塞。
5. Q1-04：6 处 widget `DialogError::Popup(_)` 折叠 `UnknownWindow`（错误语义丢失）。
6. E-12 测试真实性残余：Menu 行点击测试命名、`popup_panel::an_at_least_panel_follows_its_anchor_width` 锚点来源、P1 popup 套件无变异自检、`--pixel-check` 不覆盖 Dialog/Alert/Tooltip/Menu 像素。
7. A-26/A-27/A-31：a11y 混合父链环、两个 `MaxSize` 同名、`FrameRecord.popup_damage/z_changes` 未实现。
8. 性能（P3-5）、三端真实窗口（P3-6）、`Dialog::recenter`（P3-1）、Scale（P3-2）未开始。
9. 外部滚动语义宽于「祖先滚动」、受约束首帧未锁 → 见 §4 残余观察（建议合并进 P3-5/E-04 后续）。

**其它风险**：workspace `cargo test` 未跑（用户限制）；真实窗口像素/三端未做；`target` 与其他 agent 共享时出现过 gallery 链接 LNK1104（本轮无）。

## 9. 结论

- **P0 = 0，P1 = 0，P2 = 0**：第 1 轮 Q1-01…Q1-03、Q1-06…Q1-10 全部 fixed；Q1-04 为已登记 P3；修复轮 E-01（P0）与 E-02/E-03/E-04（P1）均 fixed 并有测试；门禁全部独立复跑通过（3777/0/2 + clippy + fmt + workspace check + 94-check pixel gate + debug-stack 两种调用）。
- **P3 = 2 个文档收口项（Q2-01/Q2-02）**：设计 §12 / `p1-summary` 停在 E-08 前快照（3771/792、112/+7660/−9238、E-08 deferred、K1-09 误记死字段），W7 历史快照仍 93 checks；均为文档同步，不改代码。完成后 P0–P3 无**未登记**项（P3 已登记遗留见 §8）。
- **可提交 P1（代码/门禁层面）：是**。提交前建议 docs agent 完成 Q2-01/Q2-02 的数字与状态同步（若坚持“零 finding 才提交”，以该文档同步为最后前置；代码侧无需再改）。
