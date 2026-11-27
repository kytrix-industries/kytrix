# P1 终审第 3 轮（剩余项 + round-3 修复确认）— review-p1-final-engine-r3

- 审查对象：worktree `E:\kytrix\.agents\worktrees\dsh-1d175414-1a41`，基线 `d9949aca`；代码自 11:19 后稳定（本报告复核 11:37–11:50）。
- 只读：未修改仓库；`git status --short` 仍为实现方 123 条改动；实验代码在仓库外 `E:\kytrix\target\p1-final\exp`。
- 前序报告：`review-p1-final-engine.md`（E-01…E-16）、`review-p1-final-engine-r2.md`（E-17…E-21 与 A/B 复核）。

## 1. round-3 三项确认

| 项 | 状态 | 证据 |
|---|---|---|
| E-17 `raise_popup` 嵌套/owner-modal 组 | **fixed** | `PopupStack::raise`（`popup/stack.rs:228-266`）收集 `modal scrim + subtree + owner_modal_descendants(含各自 scrim)`，按当前 `order` 整组抬升以保持组内相对顺序；回归 `popup_behavior::raising_a_modal_keeps_its_owner_modal_chain_above_it`（外层 A + 内层 B + 无关 C，断言 A scrim 紧邻、A<p<B、B 仍是 topmost modal、B 内容输入/焦点/a11y 不被 A 遮挡）**PASS**。EXP18 旧输出 `after=[scrimB,B,scrimA,A]` 已被该测试覆盖为“A scrim、A、p、B scrim、B 顺序且 B 在最上”。 |
| E-18 `Window::new` 生产 expect | **fixed** | `Window::new` 改为 `let popup_root = tree.install_popup_root(); let popups = PopupStack::install(id, popup_root);`（`window.rs:323-324`），`PopupStack::install(window, root) -> Self`（`stack.rs:16`）不再返回 `Result`；`window.rs` 无 `.expect/.unwrap`，`app/popup.rs`、`popup/*`、`a11y/popup.rs` 无新增生产 expect。 |
| E-21 `PopupSpec.initial_focus` | **fixed** | `popup.rs` 已无 `initial_focus` 字段（`rg` 零命中）；`FocusPolicy::Trap` 的初始焦点走打开后的 `App::set_popup_initial_focus`（`app/popup.rs:807-832`），`set_popup_initial_focus_moves_the_focus_within_a_trap`、`set_popup_initial_focus_rejects_non_trap_popups`、`default_button_wins_the_trap_initial_focus` PASS。 |

附带确认：**E-20**（窄视口 Dialog 横向裁剪）也已随 round-3 修复：`PopupPanel::available_body_width` + Dialog 宽度 clamp，`dialog::a_narrow_viewport_clamps_the_default_dialog_width_and_keeps_the_buttons_reachable` PASS（320×220 下 confirm 完整落在 panel 内）。**E-11** 的直接断言已存在：`app::tests::ime_cursor_area_includes_ancestor_popup_transform_offsets`（`app/tests.rs:2145-2190`，断言 caret y +4）。**A-29** 已由 Menu 子菜单 `a11y_parent = 父 menuitem` 实现（`menu.rs:1545`，`a_two_level_submenu_chain_attaches_to_its_parent_menu_items`）；**A-25** 静态 `has_popup` 已由 kit 声明（Menu 行 `a11y.has_popup`、Dropdown `a11y_has_popup(HasPopup::Menu)`、Select ListBox），`active_descendant` 仍为已登记 MenuChain P2。

## 2. 最终门禁（当前代码）

| 命令 | 结果 |
|---|---|
| `cargo test -p ui_core -p ui_kit` | **PASS**：2804 + 8 + 0 + 4 + 110 + 14 + 39 + 801 = **3780 passed / 0 failed / 2 ignored** |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | **PASS** |
| `cargo check --workspace --all-targets` | **PASS** |
| `cargo run -p ui_kit --example gallery -- --pixel-check` | **PASS**：94 checks，exit 0 |
| `cargo run -p ui_kit --example gallery -- --debug-stack --auto-close=9` | **PASS**：overlay-check menu `(6,180.0,127.5125)`、picker children=4、最终 entries=0，exit 0 |

日志：`E:\kytrix\target\p1-final\gates-r3.log`。

## 3. 剩余项归类（是否还有 P1 范围未登记 finding）

**没有发现未登记的 P0/P1/P2/P3 finding。** 现有开放项均可归入已登记阶段计划：

- **E-12 残余（P3，已登记 §12.6-6）**：P1 popup 套件无变异自检；`--pixel-check` 只覆盖 ColorPicker（设计 P3-4 gallery 扩展项：Dialog/Alert/Tooltip/Menu 像素采样）。round-3 已修 `popup_panel` at_least 命名；Menu 指针测试 `a_pointer_press_activates_the_entry_it_lands_on` 命名/语义已准确；caret+paint_offset 直接断言已由 `ime_cursor_area_includes_ancestor_popup_transform_offsets` 关闭。
- **A-25 的 `active_descendant`（P2，已登记 §12.6-15 / MenuChain）**：引擎通道有测试，kit 未写入，按下一阶段 MenuChain 处理；静态 `has_popup` 已关闭。
- **A-26/A-27/A-31/A-32/B-16（P3，已登记）**：a11y 父链环、`MaxSize` 命名、debug/可观测、a11y O(n)、settle `layout_ran` 触发，均非 P1 缺陷。
- **K1-11/K1-12/R2-01/R2-02/R2-04（P3/观察，已登记）**：FadeSlide 未启用、宿主测试 settle 放宽、首帧内部滚动、anchor 关闭 reason、无 confirm 时回退 Cancel。
- **P2 MenuChain/safe triangle**：下一阶段设计任务，非本轮修复遗漏。

唯一轻微文档残留：§12.6-6 仍把已关闭的 Menu 指针命名与 caret+paint_offset 列为“仍余”，属 E-12 登记文本未同步，不影响代码/提交判定，建议提交前顺手更新。

## 4. 结论

- **E-17、E-18、E-21 真正关闭；E-20/E-11/A-25 静态/A-29 也关闭。**
- **P1 范围无未登记 P0/P1/P2/P3 finding。剩余项全部是已登记的 P3 阶段计划或 MenuChain P2 下一阶段任务。**
- **P1 可提交。**（提交前可顺手把 §12.6-6 中两处已关闭残留文字同步；不构成阻塞。）
