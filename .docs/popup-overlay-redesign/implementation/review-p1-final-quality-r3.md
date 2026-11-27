# P1 终审第 3 轮收口确认（文档 / 门禁 / finding 归类）

- 审查对象：worktree `E:\kytrix\.agents\worktrees\dsh-1d175414-1a41`，基线 `d9949aca`，P1 未提交改动（round-3 修复后）。
- 快照：代码最后 mtime **11:19:13**（`popup/stack.rs`），此后稳定；设计 `§12` 11:35:53、`p1-summary` 11:36:20、`kit-gallery` 11:33:27、`p1-cleanup-w7.md` 11:32:19；本报告门禁于 11:37–11:45 复跑。
- 只读；结论：**P0/P1/P2 = 0；无属于 P1 范围且未登记的 P0–P3；P1 可提交。**

## 1. 文档 vs 最终代码/实测（全部一致）

| 文档项 | 文档值 | 实测/代码 | 结论 |
|---|---|---|---|
| §12.1 规模 | 115 tracked 文件 +8671/−9410（`.docs` 除外）；含 `kit-gallery` 为 116/+8692/−9415 | `git diff --shortstat -- . ':(exclude).docs'` = **115 +8671/−9410**；全量 = **116 +8692/−9415** | 一致 |
| §12.1 删除 4 文件 4350 行 | overlay 2871、layer 964、app/overlay 184、interaction/overlay 331 | `git show d9949aca:<file>` 行数逐项一致 | 一致 |
| §12.1 新增文件 7 路径/14 文件/15334 行（逐文件） | popup.rs 633、anchor 73、animation 187、dismiss 582、focus 290、placement 1109、settle 408、stack 775、tests 1626、app/popup 878、a11y/popup 349、a11y/popup_tests 1719、popup_behavior 5277、popup_panel 1428 | 逐文件 `Get-Content` 计数**全部一致**，合计 15334 | 一致 |
| §12.5 测试基线 | **3780 passed / 0 failed / 2 ignored**：ui_core lib 2804+1 ignored、frame_record 8、gui_smoke 0+1 ignored、layout 4、popup_behavior 110、snapshot 14、demo 39、ui_kit 801 | `cargo test -p ui_core -p ui_kit` 逐项一致，**exit 0** | 一致 |
| §12.5 测试规模 | popup/tests 43、popup_behavior 110、a11y 21、placement 34；PopupPanel 18、Dropdown 27、Select 32、Menu 30、ContextMenu 14、Tooltip 28、Dialog 54、Alert 11、ColorPicker 80 | `#[test]` 计数逐项一致 | 一致 |
| §12.5 其它门禁 | clippy/fmt/workspace check PASS；`--pixel-check` 94 checks；`--debug-stack` 自退出 0/3、auto-close 仅 ≥30s 看门狗 | clippy PASS、fmt exit 0、check exit 0、pixel **94/32/8/6+6/0、first==stable**、debug-stack（带 9 与不带）均 PASS exit 0 | 一致 |
| §12.9 rg 表 | 7 组旧 API 0；`dump-overlay` 3 处仅 gallery 改名提示；`AnchorEdge`/`match_anchor_width` 0 | 复跑：旧 kit/engine API 0、`AnchorEdge`/`match_anchor_width` 0、`dump-overlay` 3（gallery.rs:175/176/3741） | 一致 |
| §12.10/§12.11 状态 | E-08/E-17/E-18/E-20/E-21/A-25(partial)/A-29/K1-07/K1-08 b/e/f/K1-09(反转)/Q1-04 等 | 逐条读码/测试复核：Q1-04 `widget::error::{popup,dialog}_engine_error`（6 宿主使用）；K1-07 `popup_total` 29 处；E-18 `PopupStack::install(id,root)->Self` 不可失败；E-20 `available_body_width` + dialog 窄视口测试；E-21 `PopupSpec.initial_focus` 已删、`set_popup_initial_focus` 保留；E-17 `raise` 整组重排 + 测试（popup_behavior.rs:5132）；A-25 静态 has_popup（menu.rs:1419/dropdown:233/select:280 + 测试）；A-29 `a11y_parent=Some(row_node)`（menu.rs:1545 + 2/3 级测试）；K1-09 `PanelState.height` 由 `sync_height` 读取 | 一致 |
| `p1-summary` | 115/+8671/−9410、3780/2804/110/801、测试规模、E-08/E-17/E-18/E-20/E-21/K1-07/K1-08/K1-09/Q1-04/A-25/A-29 状态 | 抽查逐项一致；§8 残余清单与 §12.6 对齐 | 一致 |
| `kit-gallery.md` §13 | §13.3 命令 `--debug-stack`（auto-close 可选）；§13.5 94 checks、P0 108 差异、旧命令 exit 2、round-3 数字 | 逐项一致（tracked 文件，全量 diff 116 的来源） | 一致 |
| `p1-cleanup-w7.md` | 文首“终审后注记”：93→94 checks、3734/773→3780/801、命令语义；正文保留 W7 历史 | 注记与正文历史区分清晰 | 一致 |

## 2. 修复轮代码抽检（round-3 新增）

- **Q1-04 错误语义**：`popup_engine_error` 保留 `UnknownWindow→UnknownWindow`、`AnchorNodeMissing→StaleNode`、`ContentBuild/StackSync→source`；其余契约错误→`EngineError::FramePipeline{detail}`（`format_smolstr!`，不用 `format!`）。7 个宿主改调 helper，未再出现“全部 UnknownWindow”。
- **K1-07**：`popup_total()`（不过滤 Scrim）加入 4 个业务 ui + 2 个 widgets 测试 helper；释放/关闭路径断言 total==0（29 处调用）。
- **E-17**：`raise` 收集 scrim+子树+`owner_modal_descendants`（含各 scrim），按既有 order 整组抬升，保持组内相对顺序；`raising_a_modal_keeps_its_owner_modal_chain_above_it` 覆盖外层 A+内层 B+无关 C。
- **E-18**：`PopupStack::install` 由 `Result+expect` 改为不可失败；`Window::new` 无生产 expect（新增 popup 代码生产 `expect/unwrap/panic` 仍为 0，按 `#[cfg(test)]` 边界扫描）。
- **E-20**：`PopupPanel::available_body_width` + Dialog 内容宽 clamp；窄视口 320×220 测试 `a_narrow_viewport_clamps_the_default_dialog_width_and_keeps_the_buttons_reachable`（dialog.rs:2100）。
- **E-21**：`PopupSpec.initial_focus` 字段删除（`rg` 无残留），初始焦点统一 `App::set_popup_initial_focus`（Trap 内校验，`set_popup_initial_focus_moves_the_focus_within_a_trap`/`rejects_non_trap_popups`）。
- **A-25/A-29**：Menu 子菜单行/Dropdown/Select trigger 静态 `has_popup` + 打开期 `expanded` 投影；子菜单 `a11y_parent=父 menuitem`（2/3 级测试）。**A-25 `active_descendant` kit 未写入**已标 partial 并登记 §12.6-15（P2 MenuChain 阶段）。
- **E-12 残余**：at_least 命名已修；Menu 指针测试命名、变异自检、pixel 仅 ColorPicker、caret+offset 直接断言仍余 → §12.6-6（P3，已登记）。
- **AGENTS（本轮新增代码）**：累计 diff 新增 8670 行中注释 0、`#[allow]/#[expect]/#[path]` 0、`unwrap/panic/todo` 0、`serde_json` 0；无新依赖；`PopupError` 强类型映射保留。
- 环境：11:37 首次测试重跑因另一 agent 的 `gallery.exe` 占用 target 报 LNK1104；其退出后 11:40 起完整重跑 **exit 0**（非仓库缺陷，`p1-summary` §9 已提醒）。

## 3. 剩余项归类（均有登记，不属 P1 缺陷）

**属 P2 阶段计划（MenuChain 等）**
- A-25 残余 `active_descendant`（kit hover→a11y patch）——§12.6-15，P2 MenuChain 阶段；
- P2-1…P2-7（MenuChain/grace/键盘表/typeahead/ContextMenu/3 级/链式 dismiss kit 侧）——§12.6 P2 表。

**属 P3 阶段计划/已登记 P3**
- K1-11 `FadeSlide` 未启用（D-P1-19）；K1-12 宿主 settle 放宽（accepted）；
- E-12 测试真实性残余；R2-01 受约束首帧内部滚动高度、R2-02 关闭 reason 固定、R2-04 自建 footer 初始焦点、R2-05 观察项；
- 外部滚动语义宽于“祖先滚动”字面（建议 §4.7 措辞统一）；
- A-26 混合父链环、A-27 两个 `MaxSize` 同名、A-31 `FrameRecord.popup_damage/z_changes`；
- P3-1 `Dialog::recenter` 删除、P3-2 `Scale` 动画、P3-5 性能基线、P3-6 三端真实窗口验收。

**不属于 P1 范围/已由设计决策关闭**：K1-02（D6/D14 用户裁决）、K1-08 a/d/g、A-09、B-13 等已在 §12.3/§12.10 记录。

## 4. 结论

- **P0 = 0，P1 = 0，P2 = 0**；第 1/2 轮 Q1、Q2 与 E/K1 修复全部由最终代码/测试复核闭环。
- **P1 范围内无未登记的 P0–P3**：所有剩余项均已在 §12.6（含 P2/P3 衔接表）/§12.10/§12.11 登记，归类为“已登记 P2 阶段计划（MenuChain）”或“已登记 P3 阶段计划（性能/三端/Dialog 细化等）”。
- **P1 可提交**。提交前无需再改代码；唯一建议是保持 §12/p1-summary/kit-gallery/W7 现有同步状态与提交说明一致（当前已一致）。
